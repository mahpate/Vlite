//! EPIC-010 hybrid pivot-and-expand retrieval (ITEM-039, TEST-011).
//!
//! `GraphRef::retrieve` runs a vector/text pivot through the existing
//! `search_hybrid`, maps pivot records to nodes through attachments,
//! expands with the traversal semantics of REQ-022 and projects evidence,
//! all under the one view of the borrowed database. The tests interleave
//! writer commits between calls and check every result against the state
//! observed at the same view (pivot records, attachments, nodes, evidence
//! revisions), re-validate ANN pivots after deletes, list unmapped pivots,
//! and enforce the PD-013/PD-015/PD-016 bounds. Deadline expiry of the
//! expansion phase is covered with an injected clock by the unit tests in
//! `src/property_graph/retrieve.rs` (the public API uses the real clock).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use tempfile::tempdir;
use vlite::hybrid::{HybridQuery, VectorLeg};
use vlite::property_graph::{
    Direction, EvidenceSpec, Expansion, GraphLimits, Pivot, PropertyProjection, ReadOptions,
    Retrieval, Retrieve, TruncationReason,
};
use vlite::{
    AnnOptions, CollectionSettings, CollectionSpec, CreateOptions, Database, Error, HnswConfig,
    Metadata, MetadataValue,
};

/// SplitMix64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

const WORDS: [&str; 6] = ["rust", "graph", "vector", "agent", "storage", "index"];

fn text_settings() -> CollectionSettings {
    let mut settings = CollectionSettings::default();
    settings.text_fields = vec!["title".into()];
    settings
}

fn doc(title: &str, group: i64) -> Metadata {
    Metadata::from([
        ("title".into(), MetadataValue::String(title.into())),
        ("group".into(), MetadataValue::Integer(group)),
    ])
}

fn payload(id: u64, generation: u64) -> Vec<u8> {
    format!("record {id} generation {generation} ")
        .repeat(32)
        .into_bytes()
}

fn vector(id: u64) -> [f32; 2] {
    [id as f32, (id % 7) as f32]
}

/// A graph-enabled file with `records` documents in `default` (text field
/// `title`), `notes` records 1..=`topics`, and graph `g` where `doc-i` is
/// attached to document `i` for every `attach_every`-th document, each
/// document node MENTIONS topic `i % topics`, and `topic-t` is attached to
/// note `t + 1`.
fn fixture(path: &Path, records: u64, topics: u64, attach_every: u64) -> Database {
    let mut db = Database::create_with_graph(path, 2, CreateOptions::default()).unwrap();
    db.set_collection_settings("default", text_settings())
        .unwrap();
    db.create_collection(
        "notes",
        CollectionSpec {
            dimensions: 2,
            settings: CollectionSettings::default(),
        },
    )
    .unwrap();
    for id in 1..=records {
        let title = format!("{} {}", WORDS[(id % 6) as usize], WORDS[(id % 4) as usize]);
        db.put_with_metadata(
            id,
            &vector(id),
            &payload(id, 0),
            &doc(&title, (id % 3) as i64),
        )
        .unwrap();
    }
    {
        let mut notes = db.collection_mut("notes").unwrap();
        for id in 1..=topics {
            notes
                .put_with_metadata(id, &[0.0, id as f32], b"note", &doc("note", 0))
                .unwrap();
        }
    }
    db.create_graph("g").unwrap();
    let mut graph = db.graph_mut("g").unwrap();
    for topic in 0..topics {
        let key = format!("topic-{topic}");
        graph
            .create_node(&key, &["Topic"], &Metadata::new())
            .unwrap();
        graph.attach(&key, "notes", topic + 1).unwrap();
    }
    for id in (1..=records).filter(|id| id % attach_every == 0) {
        let key = format!("doc-{id}");
        graph.create_node(&key, &["Doc"], &Metadata::new()).unwrap();
        graph.attach(&key, "default", id).unwrap();
        graph
            .create_edge(
                None,
                &key,
                "MENTIONS",
                &format!("topic-{}", id % topics),
                &Metadata::new(),
            )
            .unwrap();
    }
    db
}

fn request(vector: Option<Vec<f32>>, text: Option<&str>, k: usize) -> Retrieve {
    let mut pivot = Pivot::new("default", k);
    pivot.vector = vector;
    pivot.text = text.map(str::to_owned);
    Retrieve {
        pivot,
        expansion: Expansion {
            include_paths: true,
            ..Expansion::default()
        },
        evidence: EvidenceSpec {
            collections: vec!["default".into(), "notes".into()],
            ..EvidenceSpec::default()
        },
    }
}

fn wide() -> ReadOptions<'static> {
    ReadOptions {
        limits: GraphLimits {
            max_hops: 1,
            ..GraphLimits::CEILING
        },
        ..ReadOptions::default()
    }
}

/// Checks a retrieval against the state of `db` at the same view and the
/// expected attachments (`collection`, record) → node key.
fn assert_consistent(
    db: &Database,
    request: &Retrieve,
    result: &Retrieval,
    attached: &BTreeMap<(String, u64), String>,
) {
    let graph = db.graph("g").unwrap();
    assert_eq!(result.view_sequence, graph.view_sequence());
    // Every pivot candidate is accounted for: mapped, unmapped or rejected.
    let mut query = HybridQuery::new(
        request.pivot.vector.clone(),
        request.pivot.text.clone().unwrap_or_default(),
        request.pivot.k,
    );
    query.conditions = request.pivot.filter.clone();
    let (hits, _) = db
        .collection("default")
        .unwrap()
        .search_hybrid(&query)
        .unwrap();
    assert_eq!(result.stats.pivot_candidates, hits.len() as u64);
    assert_eq!(
        result.pivots.len() as u64 + result.unmapped_pivots.count + result.stats.pivots_rejected,
        hits.len() as u64
    );
    let mapped: Vec<u64> = hits
        .iter()
        .map(|hit| hit.id)
        .filter(|id| attached.contains_key(&("default".into(), *id)))
        .collect();
    assert_eq!(
        result
            .pivots
            .iter()
            .map(|pivot| pivot.record_id)
            .collect::<Vec<_>>(),
        mapped
    );
    let unmapped: Vec<u64> = hits
        .iter()
        .map(|hit| hit.id)
        .filter(|id| !attached.contains_key(&("default".into(), *id)))
        .collect();
    assert_eq!(result.unmapped_pivots.record_ids, unmapped);
    for (pivot, hit) in result
        .pivots
        .iter()
        .zip(hits.iter().filter(|hit| mapped.contains(&hit.id)))
    {
        assert_eq!(pivot.score, hit.score);
        assert_eq!(pivot.distance, hit.distance);
        assert_eq!(pivot.bm25, hit.bm25);
        let key = &attached[&("default".into(), pivot.record_id)];
        assert_eq!(&pivot.node_key, key);
        let node = graph.get_node(key).unwrap().output.unwrap();
        assert_eq!(node.id, pivot.node_id);
    }
    let pivot_nodes: BTreeSet<u64> = result.pivots.iter().map(|pivot| pivot.node_id).collect();
    for path in &result.paths {
        assert!(
            pivot_nodes.contains(&path.nodes[0]),
            "paths start at pivots"
        );
    }
    for node in &result.nodes {
        let live = graph.get_node_by_id(node.node.id).unwrap().output.unwrap();
        assert_eq!(live, node.node);
    }
    for item in &result.evidence {
        assert_eq!(item.view_sequence, result.view_sequence);
        let collection = db.collection(&item.collection).unwrap();
        assert_eq!(
            collection.revision(item.record_id).unwrap(),
            Some(item.revision)
        );
        let record = collection.get(item.record_id).unwrap().unwrap();
        assert_eq!(item.metadata, record.metadata);
        assert_eq!(item.payload_bytes, record.payload.len() as u64);
        assert!(item.payload_excerpt.len() <= request.evidence.max_excerpt_bytes);
        assert!(record.payload.starts_with(&item.payload_excerpt));
        assert_eq!(
            item.excerpt_truncated,
            item.payload_excerpt.len() < record.payload.len()
        );
        assert!(attached.contains_key(&(item.collection.clone(), item.record_id)));
    }
}

fn attachments(records: u64, topics: u64, attach_every: u64) -> BTreeMap<(String, u64), String> {
    let mut attached = BTreeMap::new();
    for topic in 0..topics {
        attached.insert(("notes".into(), topic + 1), format!("topic-{topic}"));
    }
    for id in (1..=records).filter(|id| id % attach_every == 0) {
        attached.insert(("default".into(), id), format!("doc-{id}"));
    }
    attached
}

#[test]
fn retrieve_maps_pivots_expands_and_projects_bounded_evidence() {
    let dir = tempdir().unwrap();
    let db = fixture(&dir.path().join("basic.vlite"), 40, 4, 2);
    let attached = attachments(40, 4, 2);
    let graph = db.graph("g").unwrap();

    let request = request(Some(vec![10.0, 3.0]), Some("rust"), 20);
    let result = graph.retrieve(&request, &wide()).unwrap();
    assert_consistent(&db, &request, &result, &attached);
    assert!(result.complete);
    assert!(result.truncation.is_empty());
    assert!(!result.pivots.is_empty());
    assert!(
        result.unmapped_pivots.count > 0,
        "odd records are unattached"
    );
    // Pivot nodes are returned at depth 0, their topics at depth 1, with
    // one path from a pivot per non-pivot node.
    let depth0: BTreeSet<u64> = result
        .nodes
        .iter()
        .filter(|node| node.depth == 0)
        .map(|node| node.node.id)
        .collect();
    let pivots: BTreeSet<u64> = result.pivots.iter().map(|pivot| pivot.node_id).collect();
    assert_eq!(depth0, pivots);
    assert!(result.nodes.iter().any(|node| node.depth == 1));
    assert_eq!(result.paths.len(), result.nodes.len() - pivots.len());
    assert!(
        result
            .edges
            .iter()
            .all(|edge| edge.relationship == "MENTIONS")
    );
    // Evidence: one item per attached record of a returned node in the
    // requested collections; payload excerpts bounded at PD-013 (512).
    let evidence: BTreeSet<(String, u64)> = result
        .evidence
        .iter()
        .map(|item| (item.collection.clone(), item.record_id))
        .collect();
    assert_eq!(evidence.len(), result.evidence.len());
    let expected: BTreeSet<(String, u64)> = attached
        .iter()
        .filter(|(_, key)| result.nodes.iter().any(|node| &node.node.key == *key))
        .map(|(attachment, _)| attachment.clone())
        .collect();
    assert_eq!(evidence, expected);
    assert!(
        result
            .evidence
            .iter()
            .any(|item| item.collection == "notes")
    );
    assert!(
        result
            .evidence
            .iter()
            .filter(|item| item.collection == "default")
            .all(|item| item.excerpt_truncated && item.payload_excerpt.len() <= 512)
    );
    assert_eq!(result.stats.evidence_items, result.evidence.len() as u64);
    assert_eq!(result.stats.pivots_mapped, result.pivots.len() as u64);

    // Field projection and a smaller excerpt; collections outside the
    // evidence list are not projected.
    let mut narrow = request.clone();
    narrow.evidence = EvidenceSpec {
        collections: vec!["default".into()],
        fields: PropertyProjection::Only(vec!["title".into(), "missing".into()]),
        max_excerpt_bytes: 5,
        ..EvidenceSpec::default()
    };
    let result = graph.retrieve(&narrow, &wide()).unwrap();
    assert!(!result.evidence.is_empty());
    for item in &result.evidence {
        assert_eq!(item.collection, "default");
        assert_eq!(
            item.metadata.keys().collect::<Vec<_>>(),
            vec![&"title".to_string()]
        );
        assert_eq!(item.payload_excerpt, b"recor");
        assert!(item.excerpt_truncated);
    }

    // A filter applies to the pivot search (both legs) and re-validation.
    let mut filtered = request.clone();
    filtered.pivot.filter = vec![vlite::Condition::Eq {
        field: "group".into(),
        value: MetadataValue::Integer(1),
    }];
    let result = graph.retrieve(&filtered, &wide()).unwrap();
    assert_consistent(&db, &filtered, &result, &attached);
    for pivot in &result.pivots {
        assert_eq!(pivot.record_id % 3, 1);
    }

    // Expansion follows the traversal spec: incoming MENTIONS from a topic
    // pivot collection reaches documents.
    let mut topics = Retrieve {
        pivot: Pivot::new("notes", 2),
        ..request.clone()
    };
    topics.pivot.vector = Some(vec![0.0, 1.0]);
    topics.expansion.direction = Direction::In;
    topics.expansion.types = Some(vec!["MENTIONS".into()]);
    let result = graph.retrieve(&topics, &wide()).unwrap();
    assert_eq!(result.pivots.len(), 2);
    assert!(
        result
            .nodes
            .iter()
            .any(|node| node.depth == 1 && node.node.key.starts_with("doc-"))
    );
}

#[test]
fn unmapped_pivots_are_listed_with_bounded_ids() {
    let dir = tempdir().unwrap();
    let mut db = fixture(&dir.path().join("unmapped.vlite"), 200, 2, 1_000);
    // A record attached to another graph's node is unmapped for `g`.
    db.create_graph("h").unwrap();
    {
        let mut other = db.graph_mut("h").unwrap();
        other.create_node("x", &[], &Metadata::new()).unwrap();
        other.attach("x", "default", 100).unwrap();
    }
    let graph = db.graph("g").unwrap();
    let request = request(Some(vec![100.0, 2.0]), None, 100);
    let result = graph.retrieve(&request, &wide()).unwrap();
    assert!(result.pivots.is_empty());
    assert_eq!(result.unmapped_pivots.count, 100);
    // At most 64 IDs are listed, in pivot order.
    assert_eq!(result.unmapped_pivots.record_ids.len(), 64);
    assert_eq!(result.unmapped_pivots.record_ids[0], 100);
    // Nothing to expand: complete and empty.
    assert!(result.complete);
    assert!(result.nodes.is_empty() && result.edges.is_empty() && result.evidence.is_empty());
}

#[test]
fn every_retrieval_is_internally_consistent_across_interleaved_commits() {
    let dir = tempdir().unwrap();
    let mut db = fixture(&dir.path().join("same-view.vlite"), 60, 5, 3);
    let mut attached = attachments(60, 5, 3);
    let mut generation = BTreeMap::new();
    let mut rng = Rng(0x5eed);
    let mut views = BTreeSet::new();
    for step in 0..60u64 {
        // One writer commit between retrievals.
        let id = 1 + rng.below(60);
        let key = format!("doc-{id}");
        let is_attached = attached.contains_key(&("default".into(), id));
        match rng.below(5) {
            0 | 1 => {
                let next = generation.entry(id).or_insert(0);
                *next += 1;
                let title = WORDS[rng.below(6) as usize];
                db.put_with_metadata(
                    id,
                    &vector(id),
                    &payload(id, *next),
                    &doc(title, rng.below(3) as i64),
                )
                .unwrap();
            }
            2 if is_attached => {
                db.graph_mut("g").unwrap().detach(&key, "default").unwrap();
                attached.remove(&("default".into(), id));
            }
            2 | 3 if !is_attached && db.get(id).unwrap().is_some() => {
                let mut graph = db.graph_mut("g").unwrap();
                if graph.attach(&key, "default", id).is_err() {
                    graph.create_node(&key, &["Doc"], &Metadata::new()).unwrap();
                    graph.attach(&key, "default", id).unwrap();
                }
                attached.insert(("default".into(), id), key);
            }
            _ if !is_attached => {
                db.delete(id).unwrap();
            }
            _ => {
                assert!(matches!(db.delete(id), Err(Error::GraphAttachedRecord)));
            }
        }
        let graph = db.graph("g").unwrap();
        let text = WORDS[(step % 6) as usize];
        let request = request(Some(vec![(step % 60) as f32, 1.0]), Some(text), 10);
        let result = graph.retrieve(&request, &wide()).unwrap();
        assert_consistent(&db, &request, &result, &attached);
        views.insert(result.view_sequence);
    }
    assert!(views.len() > 1, "commits advance the view between calls");

    // A read-only handle keeps its open-time view.
    drop(db);
    let db = Database::open_read_only(dir.path().join("same-view.vlite")).unwrap();
    let graph = db.graph("g").unwrap();
    let request = request(Some(vec![3.0, 3.0]), Some("rust"), 10);
    let result = graph.retrieve(&request, &wide()).unwrap();
    assert_consistent(&db, &request, &result, &attached);
}

#[test]
fn ann_pivots_are_revalidated_after_deletes() {
    let dir = tempdir().unwrap();
    let mut db = fixture(&dir.path().join("ann.vlite"), 80, 4, 2);
    let mut attached = attachments(80, 4, 2);
    let config = HnswConfig::default();
    let mut request = request(Some(vec![20.0, 6.0]), None, 20);
    request.pivot.vector_leg = VectorLeg::ApproximateReady {
        config,
        ef_search: 64,
        options: AnnOptions::default(),
    };
    {
        let graph = db.graph("g").unwrap();
        assert!(matches!(
            graph.retrieve(&request, &wide()),
            Err(Error::IndexNotReady)
        ));
    }
    db.build_hnsw(config).unwrap();
    // Delete unattached records near the query and one detached record.
    let mut deleted = BTreeSet::new();
    for id in [15, 17, 19, 21, 23, 25] {
        db.delete(id).unwrap();
        deleted.insert(id);
    }
    db.graph_mut("g")
        .unwrap()
        .detach("doc-20", "default")
        .unwrap();
    attached.remove(&("default".into(), 20));
    db.delete(20).unwrap();
    deleted.insert(20);

    let graph = db.graph("g").unwrap();
    let result = graph.retrieve(&request, &wide()).unwrap();
    assert!(!result.pivots.is_empty());
    for pivot in &result.pivots {
        assert!(!deleted.contains(&pivot.record_id));
        assert!(db.get(pivot.record_id).unwrap().is_some());
        assert_eq!(
            attached[&("default".into(), pivot.record_id)],
            pivot.node_key
        );
    }
    for id in &result.unmapped_pivots.record_ids {
        assert!(!deleted.contains(id));
    }
    for item in &result.evidence {
        assert!(!(item.collection == "default" && deleted.contains(&item.record_id)));
    }
}

#[test]
fn pivot_and_evidence_bounds_are_enforced_before_execution() {
    let dir = tempdir().unwrap();
    let db = fixture(&dir.path().join("bounds.vlite"), 30, 3, 2);
    let graph = db.graph("g").unwrap();
    let base = request(Some(vec![1.0, 1.0]), Some("rust"), 5);
    let ceiling = |result: vlite::Result<Retrieval>| match result {
        Err(Error::GraphLimitExceedsCeiling { limit, .. }) => limit,
        other => panic!("expected a ceiling error, got {other:?}"),
    };

    // PD-015: pivot k ≤ 100.
    assert_eq!(Pivot::DEFAULT_K, 20);
    assert_eq!(Pivot::MAX_K, 100);
    let mut over = base.clone();
    over.pivot.k = 101;
    assert_eq!(ceiling(graph.retrieve(&over, &wide())), "pivot_k");
    over.pivot.k = 100;
    assert!(graph.retrieve(&over, &wide()).is_ok());

    // PD-016: exact-scan pivots only at or below the live-record bound.
    assert_eq!(Pivot::DEFAULT_MAX_EXACT_SCAN_RECORDS, 100_000);
    assert_eq!(Pivot::MAX_EXACT_SCAN_RECORDS, 1_000_000);
    let mut over = base.clone();
    over.pivot.max_exact_scan_records = 1_000_001;
    assert_eq!(
        ceiling(graph.retrieve(&over, &wide())),
        "max_exact_scan_records"
    );
    let mut small = base.clone();
    small.pivot.max_exact_scan_records = 29;
    assert!(matches!(
        graph.retrieve(&small, &wide()),
        Err(Error::GraphPivotTooExpensive)
    ));
    small.pivot.max_exact_scan_records = 30;
    assert!(graph.retrieve(&small, &wide()).is_ok());
    // Text-only pivots use the BM25 index and no exact scan.
    small.pivot.max_exact_scan_records = 1;
    small.pivot.vector = None;
    assert!(graph.retrieve(&small, &wide()).is_ok());
    // A zero vector weight disables the vector leg.
    small.pivot.vector = Some(vec![1.0, 1.0]);
    small.pivot.fusion.vector_weight = 0.0;
    assert!(graph.retrieve(&small, &wide()).is_ok());

    // PD-013 and the evidence item cap.
    let mut over = base.clone();
    over.evidence.max_excerpt_bytes = 4_097;
    assert_eq!(ceiling(graph.retrieve(&over, &wide())), "max_excerpt_bytes");
    let mut over = base.clone();
    over.evidence.max_items = 1_001;
    assert_eq!(
        ceiling(graph.retrieve(&over, &wide())),
        "max_evidence_items"
    );

    // Expansion limits (PD-004..PD-009) are validated like every read.
    let options = ReadOptions {
        limits: GraphLimits {
            deadline_ms: 5_001,
            ..GraphLimits::DEFAULT
        },
        ..ReadOptions::default()
    };
    assert_eq!(ceiling(graph.retrieve(&base, &options)), "deadline_ms");

    // Invalid input fails like the underlying APIs.
    let mut missing = base.clone();
    missing.pivot.collection = "nope".into();
    assert!(matches!(
        graph.retrieve(&missing, &wide()),
        Err(Error::CollectionNotFound)
    ));
    let mut missing = base.clone();
    missing.evidence.collections = vec!["nope".into()];
    assert!(matches!(
        graph.retrieve(&missing, &wide()),
        Err(Error::CollectionNotFound)
    ));
    let mut empty = base.clone();
    empty.pivot.vector = None;
    empty.pivot.text = None;
    assert!(matches!(
        graph.retrieve(&empty, &wide()),
        Err(Error::InvalidQuery(_))
    ));
    let mut bad = base.clone();
    bad.pivot.vector = Some(vec![1.0, 2.0, 3.0]);
    assert!(matches!(
        graph.retrieve(&bad, &wide()),
        Err(Error::DimensionMismatch { .. })
    ));
}

#[test]
fn truncation_and_cancellation_bound_the_expansion_not_the_pivots() {
    let dir = tempdir().unwrap();
    let db = fixture(&dir.path().join("truncate.vlite"), 40, 4, 2);
    let graph = db.graph("g").unwrap();
    let request = request(Some(vec![10.0, 3.0]), Some("rust"), 10);

    let cancel = AtomicBool::new(true);
    let cancelled = ReadOptions {
        cancel: Some(&cancel),
        ..wide()
    };
    let result = graph.retrieve(&request, &cancelled).unwrap();
    assert!(!result.complete);
    assert_eq!(result.truncation, vec![TruncationReason::Cancelled]);
    assert!(!result.pivots.is_empty(), "pivots are reported");

    let capped = ReadOptions {
        limits: GraphLimits {
            max_output_nodes: 2,
            ..wide().limits
        },
        ..ReadOptions::default()
    };
    let result = graph.retrieve(&request, &capped).unwrap();
    assert!(!result.complete);
    assert_eq!(result.truncation, vec![TruncationReason::OutputLimit]);
    assert_eq!(result.nodes.len(), 2);

    let mut few = request.clone();
    few.evidence.max_items = 1;
    let result = graph.retrieve(&few, &wide()).unwrap();
    assert_eq!(result.evidence.len(), 1);
    assert!(!result.complete);
    assert_eq!(result.truncation, vec![TruncationReason::OutputLimit]);

    let visited = ReadOptions {
        limits: GraphLimits {
            max_visited_nodes: 1,
            ..wide().limits
        },
        ..ReadOptions::default()
    };
    let result = graph.retrieve(&request, &visited).unwrap();
    assert!(result.truncation.contains(&TruncationReason::VisitedLimit));
}

/// Indicative pivot and expansion timings (ITEM-039); not publishable
/// measurements (DO-005). Run with
/// `cargo test --release --features graph --test graph_hybrid -- --ignored --nocapture`.
#[test]
#[ignore]
fn indicative_timings() {
    let dir = tempdir().unwrap();
    let records = 20_000;
    let started = Instant::now();
    let db = fixture(&dir.path().join("timing.vlite"), records, 200, 2);
    eprintln!(
        "fixture: {records} records, {} doc nodes, built in {:?}",
        records / 2,
        started.elapsed()
    );
    let graph = db.graph("g").unwrap();
    for (name, vector, text) in [
        ("vector", Some(vec![5_000.0, 3.0]), None),
        ("text", None, Some("graph agent")),
        ("hybrid", Some(vec![5_000.0, 3.0]), Some("graph agent")),
    ] {
        for hops in [1, 2] {
            let request = request(vector.clone(), text, 20);
            let options = ReadOptions {
                limits: GraphLimits {
                    max_hops: hops,
                    ..GraphLimits::CEILING
                },
                ..ReadOptions::default()
            };
            let mut runs = Vec::new();
            for _ in 0..11 {
                let started = Instant::now();
                let result = graph.retrieve(&request, &options).unwrap();
                runs.push((
                    started.elapsed(),
                    result.stats.pivot_ms,
                    result.stats.expand_ms,
                    result.nodes.len(),
                    result.evidence.len(),
                ));
            }
            runs.sort();
            let (total, pivot_ms, expand_ms, nodes, evidence) = runs[runs.len() / 2];
            eprintln!(
                "{name} hops={hops}: median total {total:?}, pivot_ms {pivot_ms}, \
                 expand_ms {expand_ms}, nodes {nodes}, evidence {evidence}"
            );
        }
    }
}
