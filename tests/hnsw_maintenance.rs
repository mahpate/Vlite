use std::time::{Duration, Instant};
use vlite::{
    Database, DistanceMetric, Error, HnswConfig, HnswMaintenancePolicy, HnswPressure,
    HnswRebuildLimits, HnswRebuildStatus, MAX_HNSW_BUFFER_IDS, MAX_HNSW_BUFFER_VECTOR_BYTES,
    Mutation,
};

fn wait(db: &mut Database) {
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        match db.poll_hnsw_rebuild().unwrap() {
            HnswRebuildStatus::Running | HnswRebuildStatus::Cancelling => {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(1));
            }
            HnswRebuildStatus::Published => break,
            status => panic!("unexpected {status:?}"),
        }
    }
}

#[test]
fn maintenance_starts_at_threshold_not_before_and_keeps_queries_ready() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = Database::create(directory.path().join("data.vlite"), 2).unwrap();
    db.put(0, &[1.0, 2.0], b"").unwrap();
    let config = HnswConfig::default();
    let metric = DistanceMetric::Cosine;
    let policy = HnswMaintenancePolicy {
        rebuild_at_buffered_ids: 2,
        ..Default::default()
    };
    let first = db.maintain_hnsw(config, metric, policy).unwrap();
    assert!(first.started);
    assert!(!first.status.ready);
    assert_eq!(first.status.pressure, HnswPressure::Unavailable);
    wait(&mut db);
    let idle = db.maintain_hnsw(config, metric, policy).unwrap();
    assert!(!idle.started);
    assert!(idle.status.ready);
    assert_eq!(idle.status.pressure, HnswPressure::Healthy);
    assert_eq!(
        idle.status.active_buffer.unwrap().remaining_ids,
        MAX_HNSW_BUFFER_IDS
    );
    db.put(1, &[2.0, 3.0], b"").unwrap();
    assert!(!db.maintain_hnsw(config, metric, policy).unwrap().started);
    db.delete(0).unwrap();
    let start = db.maintain_hnsw(config, metric, policy).unwrap();
    assert!(start.started);
    assert!(start.status.ready);
    assert_eq!(start.status.pressure, HnswPressure::RebuildRecommended);
    let status = start.status.rebuild.unwrap();
    assert_eq!(status.snapshot_records, 1);
    assert!(status.processed_records <= status.snapshot_records);
    assert_eq!(status.catchup_buffer.unwrap().buffered_ids, 0);
    db.put(2, &[3.0, 4.0], b"").unwrap();
    let status = db.hnsw_maintenance_status(config, metric, policy).unwrap();
    let buffer = status.rebuild.unwrap().catchup_buffer.unwrap();
    assert_eq!(buffer.buffered_ids, 1);
    assert_eq!(buffer.remaining_ids, 255);
    wait(&mut db);
    assert_eq!(
        db.search_approximate_ready_with_metric(&[1.0, 2.0], 10, 100, config, metric)
            .unwrap(),
        db.search_with_metric(&[1.0, 2.0], 10, metric).unwrap()
    );
}

#[test]
fn overload_is_explicit_without_rejecting_durable_writes_or_auto_rebuilding() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("data.vlite");
    let mut db = Database::create(&path, 2).unwrap();
    let config = HnswConfig::default();
    let metric = DistanceMetric::SquaredL2;
    let policy = HnswMaintenancePolicy::default();
    db.build_hnsw(config).unwrap();
    db.start_hnsw_rebuild(config, metric, policy.limits)
        .unwrap();
    let operations: Vec<_> = (0..257)
        .map(|id| Mutation::Put {
            id,
            vector: vec![id as f32, 1.0],
            payload: vec![],
        })
        .collect();
    db.write_batch(&operations).unwrap();
    let status = db.hnsw_maintenance_status(config, metric, policy).unwrap();
    assert!(!status.ready);
    assert_eq!(status.pressure, HnswPressure::CatchUpOverflow);
    let rebuild = status.rebuild.unwrap();
    assert_eq!(
        rebuild.terminal_reason,
        Some(HnswRebuildStatus::CatchUpOverflow)
    );
    assert!(rebuild.catchup_buffer.is_none());
    assert!(matches!(
        db.search_approximate_ready(&[1.0; 2], 10, 300, config),
        Err(Error::IndexNotReady)
    ));
    // No maintenance has been called by the write/search operations.
    assert_eq!(db.len(), 257);
    db.close().unwrap();
    assert_eq!(Database::open(&path).unwrap().len(), 257);
}

#[test]
fn byte_threshold_is_independent_and_policy_validation_is_before_work() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = Database::create(directory.path().join("data.vlite"), 2).unwrap();
    db.put(0, &[1.0; 2], b"").unwrap();
    let config = HnswConfig::default();
    let metric = DistanceMetric::SquaredL2;
    db.build_hnsw(config).unwrap();
    for policy in [
        HnswMaintenancePolicy {
            rebuild_at_buffered_ids: 0,
            ..Default::default()
        },
        HnswMaintenancePolicy {
            rebuild_at_buffered_ids: 257,
            ..Default::default()
        },
        HnswMaintenancePolicy {
            rebuild_at_buffer_vector_bytes: 0,
            ..Default::default()
        },
        HnswMaintenancePolicy {
            rebuild_at_buffer_vector_bytes: MAX_HNSW_BUFFER_VECTOR_BYTES + 1,
            ..Default::default()
        },
    ] {
        assert!(matches!(
            db.maintain_hnsw(config, metric, policy),
            Err(Error::InvalidHnswMaintenancePolicy)
        ));
        assert_eq!(db.poll_hnsw_rebuild().unwrap(), HnswRebuildStatus::Idle);
    }
    db.put(0, &[2.0; 2], b"").unwrap();
    let policy = HnswMaintenancePolicy {
        rebuild_at_buffer_vector_bytes: 16,
        ..Default::default()
    };
    let status = db.hnsw_maintenance_status(config, metric, policy).unwrap();
    assert_eq!(status.active_buffer.unwrap().buffer_vector_bytes, 16);
    assert_eq!(
        status.active_buffer.unwrap().remaining_vector_bytes,
        MAX_HNSW_BUFFER_VECTOR_BYTES - 16
    );
    assert!(db.maintain_hnsw(config, metric, policy).unwrap().started);
}

#[test]
fn mismatched_worker_is_not_silently_polled_published_or_replaced() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = Database::create(directory.path().join("data.vlite"), 2).unwrap();
    db.put(0, &[1.0; 2], b"").unwrap();
    let config = HnswConfig::default();
    let policy = HnswMaintenancePolicy::default();
    db.start_hnsw_rebuild(config, DistanceMetric::SquaredL2, policy.limits)
        .unwrap();
    assert!(matches!(
        db.maintain_hnsw(config, DistanceMetric::Cosine, policy),
        Err(Error::HnswRebuildBusy)
    ));
    wait(&mut db);
    let report = db
        .maintain_hnsw(config, DistanceMetric::Cosine, policy)
        .unwrap();
    assert!(report.started);
    assert!(!report.status.ready);
}

#[test]
fn exhausted_buffer_and_oversized_snapshot_have_distinct_explicit_outcomes() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = Database::create(directory.path().join("data.vlite"), 2).unwrap();
    let config = HnswConfig::default();
    let metric = DistanceMetric::SquaredL2;
    db.build_hnsw(config).unwrap();
    let operations: Vec<_> = (0..256)
        .map(|id| Mutation::Put {
            id,
            vector: vec![1.0; 2],
            payload: vec![],
        })
        .collect();
    db.write_batch(&operations).unwrap();
    let policy = HnswMaintenancePolicy::default();
    let status = db.hnsw_maintenance_status(config, metric, policy).unwrap();
    assert!(status.ready);
    assert_eq!(status.pressure, HnswPressure::CapacityExhausted);
    assert_eq!(status.active_buffer.unwrap().remaining_ids, 0);
    let small = HnswMaintenancePolicy {
        limits: HnswRebuildLimits {
            max_records: 1,
            max_snapshot_vector_bytes: 8,
        },
        ..policy
    };
    assert!(matches!(
        db.maintain_hnsw(config, metric, small),
        Err(Error::HnswRebuildTooLarge)
    ));
    assert!(
        db.search_approximate_ready(&[1.0; 2], 10, 300, config)
            .is_ok()
    );
}

#[test]
fn observation_does_not_publish_finished_worker_and_reports_processed_zero_records() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = Database::create(directory.path().join("data.vlite"), 2).unwrap();
    db.put(1, &[0.0; 2], b"").unwrap();
    db.put(2, &[1.0; 2], b"").unwrap();
    let config = HnswConfig::default();
    let metric = DistanceMetric::Cosine;
    let policy = HnswMaintenancePolicy::default();
    db.start_hnsw_rebuild(config, metric, policy.limits)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(600);
    let mut previous = 0;
    loop {
        let status = db.hnsw_maintenance_status(config, metric, policy).unwrap();
        assert!(!status.ready);
        let job = status.rebuild.unwrap();
        assert!(job.processed_records >= previous);
        assert!(job.processed_records <= 2);
        previous = job.processed_records;
        if job.worker_finished {
            assert_eq!(job.phase, "ready_to_publish");
            break;
        }
        assert_eq!(job.phase, "building");
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(
        db.hnsw_maintenance_status(config, metric, policy)
            .unwrap()
            .rebuild
            .unwrap()
            .processed_records,
        2
    );
    assert!(matches!(
        db.search_approximate_ready_with_metric(&[1.0; 2], 1, 1, config, metric),
        Err(Error::IndexNotReady)
    ));
    let report = db.maintain_hnsw(config, metric, policy).unwrap();
    assert_eq!(report.outcome, HnswRebuildStatus::Published);
    assert!(!report.started);
    assert!(report.status.ready);
    assert!(report.status.rebuild.is_none());
}

#[test]
fn cancellation_tick_does_not_restart_and_invalid_configuration_never_polls() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = Database::create(directory.path().join("data.vlite"), 2).unwrap();
    let config = HnswConfig::default();
    let policy = HnswMaintenancePolicy::default();
    db.start_hnsw_rebuild(config, DistanceMetric::Cosine, policy.limits)
        .unwrap();
    assert!(matches!(
        db.maintain_hnsw(config, DistanceMetric::InnerProduct, policy),
        Err(Error::UnsupportedAnnMetric)
    ));
    assert!(db.cancel_hnsw_rebuild());
    assert!(matches!(
        db.hnsw_maintenance_status(config, DistanceMetric::Cosine, policy)
            .unwrap()
            .rebuild
            .unwrap()
            .phase,
        "cancelling" | "discard_pending"
    ));
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        let report = db
            .maintain_hnsw(config, DistanceMetric::Cosine, policy)
            .unwrap();
        assert!(!report.started);
        if report.outcome == HnswRebuildStatus::Cancelled {
            break;
        }
        assert_eq!(report.outcome, HnswRebuildStatus::Cancelling);
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert!(
        db.maintain_hnsw(config, DistanceMetric::Cosine, policy)
            .unwrap()
            .started
    );
}
