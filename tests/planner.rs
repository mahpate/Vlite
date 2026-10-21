//! TEST-007: the `auto` planner follows Section 3.3 and never builds.

use vlite::{
    AnnOptions, CollectionSettings, CollectionSpec, Condition, Database, DistanceMetric, Error,
    FormatVersion, HnswConfig, Metadata, MetadataIndexKind, MetadataIndexSpec, MetadataValue,
    Mutation, PlanStrategy, PlannerPolicy, Quantization,
};

const LIVE: u64 = 10_000;

fn settings(policy: PlannerPolicy) -> CollectionSettings {
    let mut settings = CollectionSettings::default();
    settings.planner = policy;
    settings.metadata_indexes = vec![
        MetadataIndexSpec {
            field: "bucket".into(),
            kind: MetadataIndexKind::Eq,
        },
        MetadataIndexSpec {
            field: "half".into(),
            kind: MetadataIndexKind::EqRange,
        },
    ];
    settings
}

fn int(field: &str, value: i64) -> Condition {
    Condition::Eq {
        field: field.into(),
        value: MetadataValue::Integer(value),
    }
}

fn setup(path: &std::path::Path) -> Database {
    let mut db = Database::create_with_format(path, 4, FormatVersion::V4).unwrap();
    db.set_collection_settings("default", settings(PlannerPolicy::default()))
        .unwrap();
    let operations: Vec<Mutation> = (0..LIVE)
        .map(|id| {
            let x = id as f32;
            Mutation::PutWithMetadata {
                id,
                vector: vec![(x * 0.37).sin(), (x * 0.11).cos(), (x * 0.07).sin(), 1.0],
                payload: vec![],
                metadata: Metadata::from([
                    ("bucket".into(), MetadataValue::Integer((id % 100) as i64)),
                    ("half".into(), MetadataValue::Integer((id % 2) as i64)),
                    ("plain".into(), MetadataValue::Integer((id % 3) as i64)),
                ]),
            }
        })
        .collect();
    for chunk in operations.chunks(4096) {
        db.write_batch(chunk).unwrap();
    }
    db
}

#[test]
fn auto_chooses_by_estimate_and_readiness_and_never_builds() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = setup(&directory.path().join("db.vlite"));
    let config = HnswConfig {
        m: 8,
        ef_construction: 32,
        seed: 1,
    };
    let metric = DistanceMetric::SquaredL2;
    let options = AnnOptions::default();
    let query = [0.2, 0.4, -0.1, 1.0];
    let auto = |db: &Database, conditions: &[Condition]| {
        db.search_auto(&query, 10, conditions, metric, config, options, 64)
            .unwrap()
    };

    // Selective indexed filter: exact over re-verified candidates.
    let bucket = [int("bucket", 3)];
    let planned = auto(&db, &bucket);
    assert_eq!(planned.plan.strategy, PlanStrategy::ExactIndexed);
    assert_eq!(planned.plan.estimated_candidates, Some(100));
    assert_eq!(planned.plan.index_field.as_deref(), Some("bucket"));
    assert_eq!(planned.plan.live_records, LIVE);
    assert_eq!(planned.stats.index_candidates, 100);
    assert_eq!(planned.stats.records_scanned, 100);
    assert_eq!(
        planned.hits,
        db.search_filtered_with_metric(&query, 10, &bucket, metric)
            .unwrap()
    );

    // Unselective filter without a ready index: exact scan; nothing built.
    let half = [int("half", 0)];
    let planned = auto(&db, &half);
    assert_eq!(planned.plan.strategy, PlanStrategy::ExactScan);
    assert_eq!(planned.plan.estimated_candidates, Some(5000));
    // Range counts stop once they exceed the exact limit.
    let half_range = [Condition::Range {
        field: "half".into(),
        min: Some(MetadataValue::Integer(0)),
        max: Some(MetadataValue::Integer(0)),
    }];
    let planned = auto(&db, &half_range);
    assert_eq!(planned.plan.strategy, PlanStrategy::ExactScan);
    assert_eq!(planned.plan.estimated_candidates, Some(4097));
    assert_eq!(planned.stats.records_scanned, LIVE);
    assert_eq!(
        planned.hits,
        db.search_filtered_with_metric(&query, 10, &half, metric)
            .unwrap()
    );
    assert_eq!(auto(&db, &[]).plan.strategy, PlanStrategy::ExactScan);
    assert!(matches!(
        db.hnsw_health(config, metric, options),
        Err(Error::IndexNotReady)
    ));

    db.build_hnsw_with_metric(config, metric).unwrap();
    let planned = auto(&db, &half);
    assert_eq!(planned.plan.strategy, PlanStrategy::FilteredAnn);
    assert!(planned.stats.graph_nodes_visited > 0);
    assert_eq!(planned.hits.len(), 10);
    let unfiltered = auto(&db, &[]);
    assert_eq!(unfiltered.plan.strategy, PlanStrategy::Ann);
    assert_eq!(unfiltered.plan.estimated_candidates, None);
    let unindexed = auto(&db, &[int("plain", 1)]);
    assert_eq!(unindexed.plan.strategy, PlanStrategy::FilteredAnn);
    assert_eq!(unindexed.plan.index_field, None);
    assert_eq!(auto(&db, &bucket).plan.strategy, PlanStrategy::ExactIndexed);

    // Intersection of two indexed conditions.
    let both = auto(&db, &[int("bucket", 4), int("half", 0)]);
    assert_eq!(both.plan.strategy, PlanStrategy::ExactIndexed);
    assert_eq!(both.plan.estimated_candidates, Some(100));

    // Readiness identity includes config, metric and quantization.
    let other = HnswConfig::default();
    let planned = db
        .search_auto(&query, 10, &half, metric, other, options, 64)
        .unwrap();
    assert_eq!(planned.plan.strategy, PlanStrategy::ExactScan);
    let quantized = AnnOptions {
        quantization: Quantization::ScalarU8,
        ..AnnOptions::default()
    };
    let planned = db
        .search_auto(&query, 10, &[], metric, config, quantized, 64)
        .unwrap();
    assert_eq!(planned.plan.strategy, PlanStrategy::ExactScan);
    let planned = db
        .search_auto(
            &query,
            10,
            &half,
            DistanceMetric::InnerProduct,
            config,
            options,
            64,
        )
        .unwrap();
    assert_eq!(planned.plan.strategy, PlanStrategy::ExactScan);
    assert_eq!(planned.plan.reason, "metric has no ANN support");

    // Policy thresholds: absolute candidates and selectivity ppm.
    db.set_collection_settings(
        "default",
        settings(PlannerPolicy {
            exact_max_candidates: 50,
            exact_max_selectivity_ppm: 0,
        }),
    )
    .unwrap();
    db.build_hnsw_with_metric(config, metric).unwrap();
    let planned = auto(&db, &bucket);
    assert_eq!(planned.plan.strategy, PlanStrategy::FilteredAnn);
    assert_eq!(planned.plan.estimated_candidates, Some(100));
    db.set_collection_settings(
        "default",
        settings(PlannerPolicy {
            exact_max_candidates: 1,
            exact_max_selectivity_ppm: 600_000,
        }),
    )
    .unwrap();
    let planned = auto(&db, &half);
    assert_eq!(planned.plan.strategy, PlanStrategy::ExactIndexed);
    assert_eq!(planned.plan.estimated_candidates, Some(5000));

    // Validation does not depend on the chosen plan.
    assert!(matches!(
        db.search_auto(&query, 10, &bucket, metric, config, options, 5),
        Err(Error::InvalidEfSearch)
    ));
    assert!(matches!(
        db.search_auto(
            &query,
            10,
            &bucket,
            metric,
            config,
            AnnOptions {
                rerank_multiplier: 0,
                ..AnnOptions::default()
            },
            64
        ),
        Err(Error::InvalidQuery(_))
    ));
}

#[test]
fn empty_collections_and_settings_driven_auto() {
    let directory = tempfile::tempdir().unwrap();
    let mut db =
        Database::create_with_format(directory.path().join("db.vlite"), 2, FormatVersion::V4)
            .unwrap();
    let mut spec_settings = settings(PlannerPolicy::default());
    spec_settings.metric = DistanceMetric::Cosine;
    db.create_collection(
        "docs",
        CollectionSpec {
            dimensions: 2,
            settings: spec_settings,
        },
    )
    .unwrap();
    let docs = db.collection("docs").unwrap();
    let config = HnswConfig::default();
    for conditions in [vec![], vec![int("bucket", 1)], vec![int("plain", 1)]] {
        let planned = docs
            .search_auto(
                &[1.0, 0.0],
                5,
                &conditions,
                DistanceMetric::SquaredL2,
                config,
                AnnOptions::default(),
                1,
            )
            .unwrap();
        assert!(planned.hits.is_empty());
        assert_eq!(planned.plan.live_records, 0);
        assert_eq!(planned.plan.reason, "empty collection");
        assert!(matches!(
            planned.plan.strategy,
            PlanStrategy::ExactScan | PlanStrategy::ExactIndexed
        ));
    }

    let mut docs = db.collection_mut("docs").unwrap();
    for id in 0..20u64 {
        let metadata = Metadata::from([("bucket".into(), MetadataValue::Integer((id % 4) as i64))]);
        docs.put_with_metadata(id, &[id as f32 + 1.0, 1.0], b"", &metadata)
            .unwrap();
    }
    docs.build_hnsw_with_metric(config, DistanceMetric::Cosine)
        .unwrap();
    let planned = docs
        .search_auto_with_settings(&[1.0, 1.0], 3, &[], 16)
        .unwrap();
    assert_eq!(planned.plan.strategy, PlanStrategy::Ann);
    let exact = docs
        .view()
        .search_with_metric(&[1.0, 1.0], 3, DistanceMetric::Cosine)
        .unwrap();
    assert_eq!(planned.hits, exact);
    let planned = docs
        .search_auto_with_settings(&[1.0, 1.0], 3, &[int("bucket", 2)], 16)
        .unwrap();
    assert_eq!(planned.plan.strategy, PlanStrategy::ExactIndexed);
    assert_eq!(planned.stats.index_candidates, 5);
}
