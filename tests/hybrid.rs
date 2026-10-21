//! TEST-008: BM25 text index and weighted-RRF hybrid search.

use vlite::hybrid::{FusionOptions, HybridQuery, VectorLeg};
use vlite::text_index::{TextIndexStatus, tokenize};
use vlite::{
    AnnOptions, CollectionSettings, Condition, Database, DistanceMetric, Error, FormatVersion,
    HnswConfig, Metadata, MetadataValue,
};

fn text_settings() -> CollectionSettings {
    let mut settings = CollectionSettings::default();
    settings.text_fields = vec!["title".into(), "body".into()];
    settings
}

fn doc(title: &str, body: &str, group: i64) -> Metadata {
    Metadata::from([
        ("title".into(), MetadataValue::String(title.into())),
        ("body".into(), MetadataValue::String(body.into())),
        ("group".into(), MetadataValue::Integer(group)),
    ])
}

fn bm25(tf: f64, dl: f64, avgdl: f64, n: f64, df: f64) -> f64 {
    let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
    idf * tf * 2.2 / (tf + 1.2 * (0.25 + 0.75 * dl / avgdl))
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-12
}

fn setup(path: &std::path::Path) -> Database {
    let mut db = Database::create_with_format(path, 2, FormatVersion::V4).unwrap();
    db.set_collection_settings("default", text_settings())
        .unwrap();
    // Token counts: 1 → 4, 2 → 2, 3 → 3, 4 → 0 (no declared text), 5 → 1.
    db.put_with_metadata(1, &[0.0, 0.0], b"", &doc("Rust vector", "rust DATABASE", 1))
        .unwrap();
    db.put_with_metadata(2, &[1.0, 0.0], b"", &doc("Vector", "search", 2))
        .unwrap();
    db.put_with_metadata(3, &[2.0, 0.0], b"", &doc("", "embedded vector store", 1))
        .unwrap();
    db.put_with_metadata(
        4,
        &[3.0, 0.0],
        b"",
        &Metadata::from([("title".into(), MetadataValue::Integer(9))]),
    )
    .unwrap();
    db.put_with_metadata(5, &[4.0, 0.0], b"", &doc("rust", "", 2))
        .unwrap();
    db
}

#[test]
fn tokenizer_contract() {
    assert_eq!(
        tokenize("Hybrid-Search: BM25, ünïcode_ok!"),
        ["hybrid", "search", "bm25", "ünïcode", "ok"]
    );
    assert!(tokenize(&"x".repeat(65)).is_empty());
    assert_eq!(tokenize(&"y".repeat(64)).len(), 1);
}

#[test]
fn bm25_text_leg_matches_hand_computation_and_tracks_changes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("db.vlite");
    let mut db = setup(&path);
    assert_eq!(
        db.text_index_status(),
        TextIndexStatus {
            declared_fields: 2,
            available: true,
            documents: 4,
            terms: 6,
            postings: 9,
        }
    );
    let (n, avgdl) = (4.0, 10.0 / 4.0);
    let (hits, _) = db
        .search_hybrid(&HybridQuery::new(None, "RUST", 10))
        .unwrap();
    assert_eq!(hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), [5, 1]);
    assert!(close(hits[0].bm25.unwrap(), bm25(1.0, 1.0, avgdl, n, 2.0)));
    assert!(close(hits[1].bm25.unwrap(), bm25(2.0, 4.0, avgdl, n, 2.0)));
    assert_eq!((hits[0].text_rank, hits[1].text_rank), (Some(1), Some(2)));
    assert!(close(hits[0].score, 1.0 / 61.0));
    assert!(close(hits[1].score, 1.0 / 62.0));
    assert!(
        hits.iter()
            .all(|hit| hit.distance.is_none() && hit.vector_rank.is_none())
    );

    // Multi-term scores sum per term; every score is finite and positive.
    let (hits, _) = db
        .search_hybrid(&HybridQuery::new(None, "vector rust vector", 10))
        .unwrap();
    let expected_1 = bm25(2.0, 4.0, avgdl, n, 2.0) + bm25(1.0, 4.0, avgdl, n, 3.0);
    let one = hits.iter().find(|hit| hit.id == 1).unwrap();
    assert!(close(one.bm25.unwrap(), expected_1));
    assert!(
        hits.iter()
            .all(|hit| hit.bm25.unwrap().is_finite() && hit.bm25.unwrap() > 0.0)
    );

    // Updates and deletes are reflected; non-string values are ignored.
    db.put_with_metadata(5, &[4.0, 0.0], b"", &doc("python", "", 2))
        .unwrap();
    db.delete(1).unwrap();
    let (hits, _) = db
        .search_hybrid(&HybridQuery::new(None, "rust", 10))
        .unwrap();
    assert!(hits.is_empty());
    drop(db);
    let db = Database::open(&path).unwrap();
    let (hits, _) = db
        .search_hybrid(&HybridQuery::new(None, "python vector", 10))
        .unwrap();
    assert_eq!(hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), [5, 2, 3]);
    assert_eq!(db.text_index_status().documents, 3);
}

#[test]
fn rrf_fusion_weights_ties_and_filters() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = setup(&directory.path().join("db.vlite"));
    // Vector ranks from [1,0]: 2 (0), 1 (1), 3 (1, id tie-break), 4, 5.
    let mut query = HybridQuery::new(Some(vec![1.0, 0.0]), "vector", 3);
    let (text_only, _) = db
        .search_hybrid(&HybridQuery::new(None, "vector", 10))
        .unwrap();
    let text_order: Vec<u64> = text_only.iter().map(|hit| hit.id).collect();
    assert_eq!(text_order, [2, 3, 1]);
    let (hits, stats) = db.search_hybrid(&query).unwrap();
    let score =
        |vector_rank: f64, text_rank: f64| 1.0 / (60.0 + vector_rank) + 1.0 / (60.0 + text_rank);
    // id2: v1 t1; id1: v2 t3; id3: v3 t2 → id1 and id3 tie exactly; id asc.
    assert_eq!(hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), [2, 1, 3]);
    assert!(close(hits[0].score, score(1.0, 1.0)));
    assert!(close(hits[1].score, score(2.0, 3.0)));
    assert_eq!(hits[1].score, hits[2].score);
    assert_eq!(hits[1].distance, Some(1.0));
    assert_eq!((hits[2].vector_rank, hits[2].text_rank), (Some(3), Some(2)));
    assert_eq!(stats.records_scanned, 5);

    // Weights change the fused order; a zero-weight leg is not evaluated.
    query.fusion.text_weight = 3.0;
    let (hits, _) = db.search_hybrid(&query).unwrap();
    assert_eq!(hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), [2, 3, 1]);
    query.fusion = FusionOptions {
        vector_weight: 0.0,
        ..FusionOptions::default()
    };
    let (hits, stats) = db.search_hybrid(&query).unwrap();
    assert_eq!(hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), [2, 3, 1]);
    assert_eq!(stats.records_scanned, 0);
    assert!(hits.iter().all(|hit| hit.vector_rank.is_none()));

    // Filters apply to both legs.
    let mut filtered = HybridQuery::new(Some(vec![0.0, 0.0]), "vector rust", 10);
    filtered.conditions = vec![Condition::Eq {
        field: "group".into(),
        value: MetadataValue::Integer(2),
    }];
    let (hits, _) = db.search_hybrid(&filtered).unwrap();
    assert_eq!(hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), [2, 5]);
    assert!(
        hits.iter()
            .all(|hit| hit.vector_rank.is_some() && hit.text_rank.is_some())
    );

    // Candidates bound each leg's depth.
    let mut shallow = HybridQuery::new(Some(vec![0.0, 0.0]), "rust", 1);
    shallow.fusion.candidates = Some(1);
    let (hits, _) = db.search_hybrid(&shallow).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].vector_rank, Some(1));

    // Ready-only ANN vector leg never builds.
    let config = HnswConfig::default();
    let mut approximate = HybridQuery::new(Some(vec![0.0, 0.0]), "vector", 3);
    approximate.vector_leg = VectorLeg::ApproximateReady {
        config,
        ef_search: 64,
        options: AnnOptions::default(),
    };
    assert!(matches!(
        db.search_hybrid(&approximate),
        Err(Error::IndexNotReady)
    ));
    db.build_hnsw_with_metric(config, DistanceMetric::SquaredL2)
        .unwrap();
    let (hits, stats) = db.search_hybrid(&approximate).unwrap();
    assert_eq!(hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), [2, 1, 3]);
    assert!(stats.graph_nodes_visited > 0);
}

#[test]
fn hybrid_validation() {
    let directory = tempfile::tempdir().unwrap();
    let db = setup(&directory.path().join("db.vlite"));
    let invalid = |query: HybridQuery| {
        assert!(
            matches!(db.search_hybrid(&query), Err(Error::InvalidQuery(_))),
            "{query:?}"
        );
    };
    invalid(HybridQuery::new(None, "x".repeat(1025), 1));
    let many: Vec<String> = (0..33).map(|i| format!("t{i}")).collect();
    invalid(HybridQuery::new(None, many.join(" "), 1));
    invalid(HybridQuery::new(None, "rust", 1001));
    invalid(HybridQuery::new(None, "!!!", 1));
    let mut query = HybridQuery::new(None, "rust", 10);
    query.fusion.candidates = Some(9);
    invalid(query.clone());
    query.fusion.candidates = Some(1001);
    invalid(query.clone());
    query.fusion = FusionOptions {
        rrf_k: 0,
        ..FusionOptions::default()
    };
    invalid(query.clone());
    for (vector_weight, text_weight) in [(f64::NAN, 1.0), (1.0, 10.5), (-1.0, 1.0), (0.0, 0.0)] {
        query.fusion = FusionOptions {
            vector_weight,
            text_weight,
            ..FusionOptions::default()
        };
        invalid(query.clone());
    }
    let mut ok = HybridQuery::new(None, "rust", 1000);
    ok.fusion.candidates = Some(1000);
    assert!(db.search_hybrid(&ok).is_ok());
    let many: Vec<String> = (0..32).map(|i| format!("t{i}")).collect();
    assert!(
        db.search_hybrid(&HybridQuery::new(None, many.join(" t0 "), 1))
            .is_ok()
    );
    assert!(matches!(
        db.search_hybrid(&HybridQuery::new(Some(vec![1.0]), "", 1)),
        Err(Error::DimensionMismatch { .. })
    ));

    // Text without declared fields is invalid; vector-only still works.
    let other = tempfile::tempdir().unwrap();
    let mut plain = Database::create(other.path().join("plain.vlite"), 2).unwrap();
    plain.put(1, &[0.0, 1.0], b"").unwrap();
    assert!(matches!(
        plain.search_hybrid(&HybridQuery::new(None, "rust", 1)),
        Err(Error::InvalidQuery(_))
    ));
    let (hits, _) = plain
        .search_hybrid(&HybridQuery::new(Some(vec![0.0, 1.0]), "", 1))
        .unwrap();
    assert_eq!(hits[0].id, 1);
    assert_eq!(hits[0].distance, Some(0.0));

    // A disabled (zero-weight) text leg never consults the text index, so
    // non-empty text without declared fields matches the vector-only result.
    plain.put(2, &[3.0, 1.0], b"").unwrap();
    let (vector_only, vector_stats) = plain
        .search_hybrid(&HybridQuery::new(Some(vec![1.0, 1.0]), "", 2))
        .unwrap();
    let mut disabled = HybridQuery::new(Some(vec![1.0, 1.0]), "rust database", 2);
    disabled.fusion.text_weight = 0.0;
    let (hits, stats) = plain.search_hybrid(&disabled).unwrap();
    assert_eq!(hits, vector_only);
    assert_eq!(stats, vector_stats);
    assert_eq!(hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), [1, 2]);
    assert!(hits.iter().all(|hit| hit.text_rank.is_none()));
    // Still validated: text limits apply even to a disabled leg.
    disabled.text = "x".repeat(1025);
    assert!(matches!(
        plain.search_hybrid(&disabled),
        Err(Error::InvalidQuery(_))
    ));
    assert_eq!(
        plain.text_index_status(),
        TextIndexStatus {
            declared_fields: 0,
            available: true,
            documents: 0,
            terms: 0,
            postings: 0,
        }
    );
}
