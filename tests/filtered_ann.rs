use std::{
    collections::{BTreeMap, HashSet},
    time::{Duration, Instant},
};
use vlite::{
    Condition, Database, DistanceMetric, Error, HnswConfig, HnswRebuildLimits, HnswRebuildStatus,
    MAX_HNSW_BUFFER_IDS, Metadata, MetadataValue as V, Mutation, SearchHit,
};

const METRICS: [DistanceMetric; 2] = [DistanceMetric::SquaredL2, DistanceMetric::Cosine];

fn tag(value: &str) -> Metadata {
    BTreeMap::from([("tag".into(), V::String(value.into()))])
}

fn eq(value: &str) -> Vec<Condition> {
    vec![Condition::Eq {
        field: "tag".into(),
        value: V::String(value.into()),
    }]
}

fn ids(hits: &[SearchHit]) -> Vec<u64> {
    hits.iter().map(|hit| hit.id).collect()
}

/// Deterministic pseudo-random values in [0, 1).
fn noise(seed: u64) -> f32 {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0xdead_beef;
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    ((x ^ (x >> 31)) >> 40) as f32 / (1u64 << 24) as f32
}

/// 2-D grid shifted away from the origin so cosine never sees zero vectors.
/// Every 50th ID is "rare"; rare IDs are never among the global top-k near (1, 1).
fn grid(path: &std::path::Path) -> Database {
    let mut db = Database::create(path, 2).unwrap();
    let operations: Vec<_> = (0..1000u64)
        .map(|id| Mutation::PutWithMetadata {
            id,
            vector: vec![(id % 40) as f32 + 1.0, (id / 40) as f32 + 1.0],
            payload: vec![],
            metadata: tag(if id % 50 == 7 { "rare" } else { "common" }),
        })
        .collect();
    for chunk in operations.chunks(256) {
        db.write_batch(chunk).unwrap();
    }
    db
}

fn recall(found: &[SearchHit], exact: &[SearchHit]) -> f64 {
    if exact.is_empty() {
        return if found.is_empty() { 1.0 } else { 0.0 };
    }
    let exact: HashSet<_> = exact.iter().map(|hit| hit.id).collect();
    found.iter().filter(|hit| exact.contains(&hit.id)).count() as f64 / exact.len() as f64
}

fn assert_eligible_and_exact(
    db: &Database,
    hits: &[SearchHit],
    query: &[f32],
    filter: &[Condition],
    metric: DistanceMetric,
) {
    let all = db
        .search_filtered_with_metric(query, db.len(), filter, metric)
        .unwrap();
    let eligible: BTreeMap<_, _> = all.iter().map(|hit| (hit.id, hit.distance)).collect();
    for hit in hits {
        assert_eq!(
            eligible.get(&hit.id),
            Some(&hit.distance),
            "{metric:?} {}",
            hit.id
        );
    }
    for pair in hits.windows(2) {
        assert!(
            pair[0].distance < pair[1].distance
                || (pair[0].distance == pair[1].distance && pair[0].id < pair[1].id)
        );
    }
}

#[test]
fn restrictive_filter_fills_k_with_matches_outside_the_unfiltered_top_k() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = grid(&directory.path().join("grid.vlite"));
    let config = HnswConfig::default();
    let rare = eq("rare");
    let query = [2.0, 2.0];
    for metric in METRICS {
        db.build_hnsw_with_metric(config, metric).unwrap();
        let unfiltered = db
            .search_approximate_ready_with_metric(&query, 10, 10, config, metric)
            .unwrap();
        let exact = db
            .search_filtered_with_metric(&query, 10, &rare, metric)
            .unwrap();
        assert_eq!(exact.len(), 10);
        assert!(ids(&unfiltered).iter().all(|id| id % 50 != 7), "{metric:?}");
        // Post-filtering the unfiltered top-k would return nothing here.
        let ready = db
            .search_approximate_ready_filtered_with_metric(&query, 10, 10, config, &rare, metric)
            .unwrap();
        assert_eq!(ready.len(), 10, "{metric:?}");
        assert!(ids(&ready).iter().all(|id| id % 50 == 7));
        assert_eligible_and_exact(&db, &ready, &query, &rare, metric);
        let wide = db
            .search_approximate_ready_filtered_with_metric(&query, 10, 200, config, &rare, metric)
            .unwrap();
        assert!(
            recall(&wide, &exact) >= 0.9,
            "{metric:?} {wide:?} {exact:?}"
        );
        let lazy = db
            .search_approximate_filtered_with_metric(&query, 10, 200, config, &rare, metric)
            .unwrap();
        assert_eq!(lazy, wide);
        // Empty predicates are the unfiltered fast path.
        assert_eq!(
            db.search_approximate_ready_filtered_with_metric(&query, 10, 10, config, &[], metric)
                .unwrap(),
            unfiltered
        );
        assert_eq!(
            db.search_approximate_filtered_with_metric(&query, 10, 10, config, &[], metric)
                .unwrap(),
            db.search_approximate_with_metric(&query, 10, 10, config, metric)
                .unwrap()
        );
    }
}

#[test]
fn no_match_and_range_filters_return_only_eligible_records() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = grid(&directory.path().join("grid.vlite"));
    let config = HnswConfig::default();
    for metric in METRICS {
        db.build_hnsw_with_metric(config, metric).unwrap();
        for filter in [
            eq("missing"),
            vec![Condition::Range {
                field: "tag".into(),
                min: Some(V::Integer(0)),
                max: None,
            }],
        ] {
            assert!(
                db.search_approximate_ready_filtered_with_metric(
                    &[3.0, 3.0],
                    5,
                    50,
                    config,
                    &filter,
                    metric
                )
                .unwrap()
                .is_empty()
            );
            assert!(
                db.search_approximate_filtered_with_metric(
                    &[3.0, 3.0],
                    5,
                    50,
                    config,
                    &filter,
                    metric
                )
                .unwrap()
                .is_empty()
            );
        }
    }
    let mut db = Database::create(directory.path().join("range.vlite"), 1).unwrap();
    for id in 0..300u64 {
        let metadata = BTreeMap::from([("n".into(), V::Integer(id as i64))]);
        db.put_with_metadata(id, &[id as f32 + 1.0], b"", &metadata)
            .unwrap();
    }
    db.build_hnsw(config).unwrap();
    let filter = [Condition::Range {
        field: "n".into(),
        min: Some(V::Integer(250)),
        max: Some(V::Integer(260)),
    }];
    let hits = db
        .search_approximate_ready_filtered_with_metric(
            &[1.0],
            20,
            100,
            config,
            &filter,
            DistanceMetric::SquaredL2,
        )
        .unwrap();
    assert_eq!(ids(&hits), (250..=260).collect::<Vec<_>>());
}

#[test]
fn malformed_filters_validate_before_readiness_zero_k_empty_and_builds() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("invalid.vlite");
    let mut empty = Database::create(directory.path().join("empty.vlite"), 2).unwrap();
    grid(&path).close().unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let mut db = Database::open(&path).unwrap();
    let config = HnswConfig::default();
    let invalid = [
        vec![Condition::Range {
            field: "n".into(),
            min: None,
            max: None,
        }],
        vec![Condition::Range {
            field: "n".into(),
            min: Some(V::String("a".into())),
            max: None,
        }],
        vec![Condition::Range {
            field: "n".into(),
            min: Some(V::Integer(2)),
            max: Some(V::Integer(1)),
        }],
        vec![Condition::Range {
            field: "n".into(),
            min: Some(V::Integer(1)),
            max: Some(V::Float(2.0)),
        }],
        vec![Condition::Eq {
            field: String::new(),
            value: V::Boolean(true),
        }],
        vec![Condition::Eq {
            field: "f".into(),
            value: V::Float(f64::NAN),
        }],
        vec![eq("x").remove(0); 65],
    ];
    for metric in METRICS {
        for filter in &invalid {
            for k in [0, 1] {
                for target in [&mut db, &mut empty] {
                    assert!(matches!(
                        target.search_approximate_ready_filtered_with_metric(
                            &[1.0, 1.0],
                            k,
                            10,
                            config,
                            filter,
                            metric
                        ),
                        Err(Error::InvalidFilter(_) | Error::InvalidMetadata(_))
                    ));
                    assert!(matches!(
                        target.search_approximate_filtered_with_metric(
                            &[1.0, 1.0],
                            k,
                            10,
                            config,
                            filter,
                            metric
                        ),
                        Err(Error::InvalidFilter(_) | Error::InvalidMetadata(_))
                    ));
                }
            }
        }
        assert!(
            !db.hnsw_maintenance_status(config, metric, Default::default())
                .unwrap()
                .ready,
            "invalid filters must not lazily build"
        );
    }
    // Zero cosine queries, unsupported metrics and bad ef_search keep their errors.
    let ok = eq("rare");
    assert!(matches!(
        db.search_approximate_filtered_with_metric(
            &[0.0, 0.0],
            0,
            10,
            config,
            &ok,
            DistanceMetric::Cosine
        ),
        Err(Error::ZeroNormQuery)
    ));
    assert!(matches!(
        db.search_approximate_ready_filtered_with_metric(
            &[0.0, 0.0],
            1,
            10,
            config,
            &ok,
            DistanceMetric::Cosine
        ),
        Err(Error::ZeroNormQuery)
    ));
    assert!(matches!(
        db.search_approximate_filtered_with_metric(
            &[1.0, 1.0],
            1,
            10,
            config,
            &ok,
            DistanceMetric::InnerProduct
        ),
        Err(Error::UnsupportedAnnMetric)
    ));
    assert!(matches!(
        db.search_approximate_filtered_with_metric(
            &[1.0, 1.0],
            5,
            4,
            config,
            &ok,
            DistanceMetric::SquaredL2
        ),
        Err(Error::InvalidEfSearch)
    ));
    // Zero k and empty databases never require or build an index.
    for metric in METRICS {
        assert!(
            db.search_approximate_ready_filtered_with_metric(
                &[1.0, 1.0],
                0,
                10,
                config,
                &ok,
                metric
            )
            .unwrap()
            .is_empty()
        );
        assert!(
            db.search_approximate_filtered_with_metric(&[1.0, 1.0], 0, 10, config, &ok, metric)
                .unwrap()
                .is_empty()
        );
        assert!(
            empty
                .search_approximate_ready_filtered_with_metric(
                    &[1.0, 1.0],
                    3,
                    10,
                    config,
                    &ok,
                    metric
                )
                .unwrap()
                .is_empty()
        );
        assert!(
            !db.hnsw_maintenance_status(config, metric, Default::default())
                .unwrap()
                .ready
        );
        assert!(matches!(
            db.search_approximate_ready_filtered_with_metric(
                &[1.0, 1.0],
                1,
                10,
                config,
                &ok,
                metric
            ),
            Err(Error::IndexNotReady)
        ));
    }
    db.close().unwrap();
    assert_eq!(bytes, std::fs::read(&path).unwrap());
}

#[test]
fn buffered_updates_deletes_and_batches_use_current_metadata_and_vectors() {
    for metric in METRICS {
        let directory = tempfile::tempdir().unwrap();
        let mut db = grid(&directory.path().join("grid.vlite"));
        let config = HnswConfig::default();
        db.build_hnsw_with_metric(config, metric).unwrap();
        let rare = eq("rare");
        let query = [1.0, 1.0];
        let before = db
            .search_approximate_ready_filtered_with_metric(
                &[8.0, 1.0],
                3,
                400,
                config,
                &rare,
                metric,
            )
            .unwrap();
        assert_eq!(ids(&before)[0], 7);
        // 7: rare -> common (stale graph metadata must not leak).
        db.put_with_metadata(7, &[8.0, 1.0], b"", &tag("common"))
            .unwrap();
        // 57: deleted.
        assert!(db.delete(57).unwrap());
        // 1: common -> rare, moved to the query (only the buffer knows it).
        // 2000: brand new rare insert; 107: rare vector moved far away.
        db.write_batch(&[
            Mutation::PutWithMetadata {
                id: 1,
                vector: vec![1.0, 1.0],
                payload: vec![],
                metadata: tag("rare"),
            },
            Mutation::PutWithMetadata {
                id: 2000,
                vector: vec![2.0, 2.0],
                payload: vec![],
                metadata: tag("rare"),
            },
            Mutation::PutWithMetadata {
                id: 107,
                vector: vec![40.0, 25.0],
                payload: vec![],
                metadata: tag("rare"),
            },
            Mutation::Put {
                id: 3,
                vector: vec![1.0, 1.0],
                payload: vec![],
            },
            // Metadata-only transitions: the graph keeps identical vectors.
            Mutation::PutWithMetadata {
                id: 40,
                vector: vec![1.0, 2.0],
                payload: vec![],
                metadata: tag("rare"),
            },
            Mutation::PutWithMetadata {
                id: 157,
                vector: vec![38.0, 4.0],
                payload: vec![],
                metadata: tag("common"),
            },
        ])
        .unwrap();
        for k in [1, 5, 25] {
            let exact = db
                .search_filtered_with_metric(&query, k, &rare, metric)
                .unwrap();
            let ready = db
                .search_approximate_ready_filtered_with_metric(
                    &query, k, 1000, config, &rare, metric,
                )
                .unwrap();
            assert_eq!(ready, exact, "{metric:?} k={k}");
            let found = ids(&ready);
            assert!(![7, 57, 3, 157].iter().any(|id| found.contains(id)));
            assert!(
                k < 25 || found.contains(&40),
                "every eligible record fits in k=25"
            );
        }
        let top = ids(&db
            .search_approximate_ready_filtered_with_metric(&query, 3, 10, config, &rare, metric)
            .unwrap());
        assert!(
            top.contains(&1) && top.contains(&2000),
            "{metric:?} {top:?}"
        );
    }
}

#[test]
fn overflow_reopen_and_lazy_rebuild_follow_existing_readiness_without_mutating_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("grid.vlite");
    let mut db = grid(&path);
    let config = HnswConfig::default();
    let rare = eq("rare");
    let metric = DistanceMetric::SquaredL2;
    db.build_hnsw_with_metric(config, metric).unwrap();
    for id in 0..=MAX_HNSW_BUFFER_IDS as u64 {
        db.put_with_metadata(id, &[id as f32 + 0.5, 1.0], b"", &tag("common"))
            .unwrap();
    }
    db.put_with_metadata(5000, &[0.5, 0.5], b"", &tag("rare"))
        .unwrap();
    assert!(matches!(
        db.search_approximate_ready_filtered_with_metric(&[0.0, 0.0], 1, 10, config, &rare, metric),
        Err(Error::IndexNotReady)
    ));
    db.close().unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let mut db = Database::open(&path).unwrap();
    for metric in METRICS {
        assert!(matches!(
            db.search_approximate_ready_filtered_with_metric(
                &[1.0, 1.0],
                1,
                10,
                config,
                &rare,
                metric
            ),
            Err(Error::IndexNotReady)
        ));
        let lazy = db
            .search_approximate_filtered_with_metric(&[0.5, 0.5], 5, 500, config, &rare, metric)
            .unwrap();
        assert_eq!(
            lazy,
            db.search_filtered_with_metric(&[0.5, 0.5], 5, &rare, metric)
                .unwrap()
        );
        assert_eq!(
            lazy,
            db.search_approximate_ready_filtered_with_metric(
                &[0.5, 0.5],
                5,
                500,
                config,
                &rare,
                metric
            )
            .unwrap()
        );
    }
    db.close().unwrap();
    assert_eq!(bytes, std::fs::read(&path).unwrap());
}

#[test]
fn background_rebuild_publication_serves_current_filtered_results() {
    for metric in METRICS {
        let directory = tempfile::tempdir().unwrap();
        let mut db = grid(&directory.path().join("grid.vlite"));
        let config = HnswConfig::default();
        let rare = eq("rare");
        db.build_hnsw_with_metric(config, metric).unwrap();
        db.start_hnsw_rebuild(config, metric, HnswRebuildLimits::default())
            .unwrap();
        db.put_with_metadata(7, &[1.0, 1.0], b"", &tag("common"))
            .unwrap();
        db.put_with_metadata(41, &[1.0, 1.0], b"", &tag("rare"))
            .unwrap();
        db.delete(57).unwrap();
        let deadline = Instant::now() + Duration::from_secs(600);
        let status = loop {
            let status = db.poll_hnsw_rebuild().unwrap();
            if !matches!(
                status,
                HnswRebuildStatus::Running | HnswRebuildStatus::Cancelling
            ) {
                break status;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(status, HnswRebuildStatus::Published, "{metric:?}");
        let exact = db
            .search_filtered_with_metric(&[1.0, 1.0], 10, &rare, metric)
            .unwrap();
        assert_eq!(
            db.search_approximate_ready_filtered_with_metric(
                &[1.0, 1.0],
                10,
                1000,
                config,
                &rare,
                metric
            )
            .unwrap(),
            exact
        );
        assert_eq!(exact[0].id, 41);
    }
}

#[test]
fn filtered_ann_recall_against_independent_filtered_exact_oracle() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = Database::create(directory.path().join("recall.vlite"), 8).unwrap();
    let operations: Vec<_> = (0..3000u64)
        .map(|id| Mutation::PutWithMetadata {
            id,
            vector: (0..8)
                .map(|d| noise(id * 8 + d) * 2.0 - 1.0 + 0.01)
                .collect(),
            payload: vec![],
            metadata: BTreeMap::from([("bucket".into(), V::Integer((id % 100) as i64))]),
        })
        .collect();
    for chunk in operations.chunks(256) {
        db.write_batch(chunk).unwrap();
    }
    let config = HnswConfig::default();
    for metric in METRICS {
        db.build_hnsw_with_metric(config, metric).unwrap();
        // 50%, 10%, 1% and 0% selectivity.
        for (max, minimum) in [
            (Some(49u64), 0.9),
            (Some(9), 0.9),
            (Some(0), 0.9),
            (None, 1.0),
        ] {
            let filter = vec![match max {
                Some(max) => Condition::Range {
                    field: "bucket".into(),
                    min: Some(V::Integer(0)),
                    max: Some(V::Integer(max as i64)),
                },
                None => Condition::Eq {
                    field: "bucket".into(),
                    value: V::Integer(100),
                },
            }];
            let mut total = 0.0;
            let queries = 20;
            for q in 0..queries {
                let query: Vec<f32> = (0..8)
                    .map(|d| noise(1_000_000 + q * 8 + d) * 2.0 - 1.0)
                    .collect();
                // Independent oracle: brute force over current records.
                let mut oracle: Vec<SearchHit> = db
                    .search_with_metric(&query, db.len(), metric)
                    .unwrap()
                    .into_iter()
                    .filter(|hit| max.is_some_and(|max| hit.id % 100 <= max))
                    .collect();
                oracle.truncate(10);
                let hits = db
                    .search_approximate_ready_filtered_with_metric(
                        &query, 10, 100, config, &filter, metric,
                    )
                    .unwrap();
                assert!(
                    hits.iter()
                        .all(|hit| max.is_some_and(|max| hit.id % 100 <= max))
                );
                assert_eq!(hits.len(), oracle.len());
                total += recall(&hits, &oracle);
            }
            let mean = total / queries as f64;
            assert!(mean >= minimum, "{metric:?} max={max:?} recall={mean}");
        }
    }
}
