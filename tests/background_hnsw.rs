use std::time::{Duration, Instant};
use vlite::{
    Database, DistanceMetric, Error, HnswConfig, HnswRebuildLimits, HnswRebuildStatus, Mutation,
};

fn wait(db: &mut Database) -> vlite::Result<HnswRebuildStatus> {
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        let status = db.poll_hnsw_rebuild()?;
        if !matches!(
            status,
            HnswRebuildStatus::Running | HnswRebuildStatus::Cancelling
        ) {
            return Ok(status);
        }
        assert!(
            Instant::now() < deadline,
            "background worker failed to finish"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn populate(db: &mut Database) {
    let operations: Vec<_> = (0..100)
        .map(|id| Mutation::Put {
            id,
            vector: vec![id as f32 + 1.0, 1.0, 2.0],
            payload: vec![1; 100],
        })
        .collect();
    db.write_batch(&operations).unwrap();
}

#[test]
fn snapshot_catchup_preserves_current_scores_deletes_inserts_and_zero_transitions() {
    for metric in [DistanceMetric::SquaredL2, DistanceMetric::Cosine] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("data.vlite");
        let mut db = Database::create(&path, 3).unwrap();
        populate(&mut db);
        db.put(100, &[0.0; 3], b"zero").unwrap();
        let config = HnswConfig::default();
        db.build_hnsw_with_metric(config, metric).unwrap();
        db.start_hnsw_rebuild(config, metric, HnswRebuildLimits::default())
            .unwrap();
        assert!(matches!(
            db.start_hnsw_rebuild(config, metric, HnswRebuildLimits::default()),
            Err(Error::HnswRebuildBusy)
        ));
        db.write_batch(&[
            Mutation::Put {
                id: 0,
                vector: vec![3.0, 2.0, 1.0],
                payload: vec![],
            },
            Mutation::Delete { id: 1 },
            Mutation::Put {
                id: 100,
                vector: vec![1.0, 0.0, 1.0],
                payload: vec![],
            },
            Mutation::Put {
                id: 101,
                vector: vec![0.0, 1.0, 1.0],
                payload: vec![],
            },
            Mutation::Delete { id: 2 },
            Mutation::Put {
                id: 2,
                vector: vec![0.0; 3],
                payload: vec![],
            },
        ])
        .unwrap();
        // The old matching index remains usable while the worker builds.
        let query = [1.0, 2.0, 1.0];
        let expected = db.search_with_metric(&query, 10, metric).unwrap();
        assert_eq!(
            db.search_approximate_ready_with_metric(&query, 10, 200, config, metric)
                .unwrap(),
            expected
        );
        assert_eq!(wait(&mut db).unwrap(), HnswRebuildStatus::Published);
        assert_eq!(
            db.search_approximate_ready_with_metric(&query, 10, 200, config, metric)
                .unwrap(),
            expected
        );
        assert_eq!(db.poll_hnsw_rebuild().unwrap(), HnswRebuildStatus::Idle);
        let bytes = std::fs::metadata(&path).unwrap().len();
        db.start_hnsw_rebuild(config, metric, HnswRebuildLimits::default())
            .unwrap();
        assert_eq!(wait(&mut db).unwrap(), HnswRebuildStatus::Published);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), bytes);
        db.close().unwrap();
        let db = Database::open(&path).unwrap();
        assert_eq!(db.search_with_metric(&query, 10, metric).unwrap(), expected);
        assert!(matches!(
            db.search_approximate_ready_with_metric(&query, 10, 200, config, metric),
            Err(Error::IndexNotReady)
        ));
    }
}

#[test]
fn snapshot_limits_validation_and_cancel_do_not_replace_the_old_index() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = Database::create(directory.path().join("data.vlite"), 3).unwrap();
    populate(&mut db);
    let config = HnswConfig::default();
    db.build_hnsw(config).unwrap();
    for limits in [
        HnswRebuildLimits {
            max_records: 99,
            max_snapshot_vector_bytes: 1200,
        },
        HnswRebuildLimits {
            max_records: 100,
            max_snapshot_vector_bytes: 1199,
        },
    ] {
        assert!(matches!(
            db.start_hnsw_rebuild(config, DistanceMetric::SquaredL2, limits),
            Err(Error::HnswRebuildTooLarge)
        ));
    }
    assert!(matches!(
        db.start_hnsw_rebuild(
            config,
            DistanceMetric::InnerProduct,
            HnswRebuildLimits::default()
        ),
        Err(Error::UnsupportedAnnMetric)
    ));
    let limits = HnswRebuildLimits {
        max_records: 100,
        max_snapshot_vector_bytes: 1200,
    };
    db.start_hnsw_rebuild(config, DistanceMetric::Cosine, limits)
        .unwrap();
    assert!(db.cancel_hnsw_rebuild());
    assert_eq!(wait(&mut db).unwrap(), HnswRebuildStatus::Cancelled);
    assert!(!db.cancel_hnsw_rebuild());
    assert!(
        db.search_approximate_ready(&[1.0; 3], 10, 100, config)
            .is_ok()
    );
    assert!(matches!(
        db.search_approximate_ready_with_metric(&[1.0; 3], 10, 100, config, DistanceMetric::Cosine),
        Err(Error::IndexNotReady)
    ));
}

#[test]
fn catchup_overflow_is_explicit_never_publishes_stale_snapshot_and_keeps_writes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("data.vlite");
    let mut db = Database::create(&path, 3).unwrap();
    populate(&mut db);
    let config = HnswConfig::default();
    db.start_hnsw_rebuild(
        config,
        DistanceMetric::SquaredL2,
        HnswRebuildLimits::default(),
    )
    .unwrap();
    let operations: Vec<_> = (0..257)
        .map(|id| Mutation::Put {
            id,
            vector: vec![id as f32, 2.0, 3.0],
            payload: vec![],
        })
        .collect();
    db.write_batch(&operations).unwrap();
    assert_eq!(wait(&mut db).unwrap(), HnswRebuildStatus::CatchUpOverflow);
    assert_eq!(db.len(), 257);
    assert!(matches!(
        db.search_approximate_ready(&[1.0; 3], 10, 300, config),
        Err(Error::IndexNotReady)
    ));
    db.start_hnsw_rebuild(
        config,
        DistanceMetric::SquaredL2,
        HnswRebuildLimits::default(),
    )
    .unwrap();
    assert_eq!(wait(&mut db).unwrap(), HnswRebuildStatus::Published);
    assert_eq!(
        db.search_approximate_ready(&[1.0; 3], 10, 300, config)
            .unwrap(),
        db.search(&[1.0; 3], 10).unwrap()
    );
    db.close().unwrap();
    assert_eq!(Database::open(&path).unwrap().len(), 257);
}

#[test]
fn synchronous_build_supersedes_worker_and_close_drop_join_and_unlock() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("data.vlite");
    let mut db = Database::create(&path, 3).unwrap();
    populate(&mut db);
    let config = HnswConfig::default();
    db.start_hnsw_rebuild(
        config,
        DistanceMetric::SquaredL2,
        HnswRebuildLimits::default(),
    )
    .unwrap();
    db.build_hnsw_with_metric(config, DistanceMetric::Cosine)
        .unwrap();
    assert_eq!(wait(&mut db).unwrap(), HnswRebuildStatus::Superseded);
    assert!(
        db.search_approximate_ready_with_metric(&[1.0; 3], 10, 100, config, DistanceMetric::Cosine)
            .is_ok()
    );
    db.start_hnsw_rebuild(
        config,
        DistanceMetric::SquaredL2,
        HnswRebuildLimits::default(),
    )
    .unwrap();
    db.close().unwrap();
    let mut db = Database::open(&path).unwrap();
    db.start_hnsw_rebuild(config, DistanceMetric::Cosine, HnswRebuildLimits::default())
        .unwrap();
    drop(db);
    assert_eq!(Database::open(&path).unwrap().len(), 100);
}

#[test]
fn empty_and_all_zero_snapshots_accept_new_live_records_during_build() {
    for zeros in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let mut db = Database::create(directory.path().join("data.vlite"), 3).unwrap();
        if zeros {
            db.put(1, &[0.0; 3], b"").unwrap();
        }
        let config = HnswConfig::default();
        db.start_hnsw_rebuild(config, DistanceMetric::Cosine, HnswRebuildLimits::default())
            .unwrap();
        db.put(1, &[1.0; 3], b"new nonzero").unwrap();
        assert_eq!(wait(&mut db).unwrap(), HnswRebuildStatus::Published);
        assert_eq!(
            db.search_approximate_ready_with_metric(
                &[1.0; 3],
                1,
                1,
                config,
                DistanceMetric::Cosine
            )
            .unwrap(),
            db.search_with_metric(&[1.0; 3], 1, DistanceMetric::Cosine)
                .unwrap()
        );
    }
}

#[test]
fn repeated_committed_histories_keep_snapshot_originals_and_publish_current_live_state() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = Database::create(directory.path().join("data.vlite"), 3).unwrap();
    populate(&mut db);
    let config = HnswConfig::default();
    for round in 0..12 {
        let metric = if round % 2 == 0 {
            DistanceMetric::Cosine
        } else {
            DistanceMetric::SquaredL2
        };
        db.start_hnsw_rebuild(config, metric, HnswRebuildLimits::default())
            .unwrap();
        for pass in 0..4 {
            let operations: Vec<_> = (0..50)
                .map(|id| {
                    if (id + round + pass) % 3 == 0 {
                        Mutation::Delete { id }
                    } else {
                        Mutation::Put {
                            id,
                            vector: vec![round as f32 + 1.0, pass as f32, id as f32],
                            payload: vec![],
                        }
                    }
                })
                .collect();
            db.write_batch(&operations).unwrap();
        }
        assert_eq!(wait(&mut db).unwrap(), HnswRebuildStatus::Published);
        for query in [[1.0, 2.0, 3.0], [-1.0, 1.0, 0.0]] {
            // Full traversal removes approximate recall as a variable in visibility tests.
            assert_eq!(
                db.search_approximate_ready_with_metric(&query, db.len(), 300, config, metric)
                    .unwrap(),
                db.search_with_metric(&query, db.len(), metric).unwrap()
            );
        }
    }
}
