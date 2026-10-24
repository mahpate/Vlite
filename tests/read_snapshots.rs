use vlite::{Database, DistanceMetric, FormatVersion, Metadata, MetadataValue, Mutation};

fn database() -> Database {
    Database::create_in_memory(
        2,
        vlite::CreateOptions {
            format: Some(FormatVersion::V5),
            ..Default::default()
        },
    )
    .unwrap()
}

#[test]
fn snapshot_preserves_committed_vectors_metadata_payloads_deletes_and_ttl() {
    let mut db = database();
    db.put_with_expiry(1, &[1.0, 2.0], b"before", &Metadata::new(), Some(200))
        .unwrap();
    db.put_with_metadata(
        2,
        &[1.0, 2.0],
        b"tie",
        &Metadata::from([("stage".into(), MetadataValue::Integer(1))]),
    )
    .unwrap();
    let snapshot = db.read_snapshot().unwrap();
    let sequence = snapshot.sequence();
    db.put(1, &[9.0, 9.0], b"after").unwrap();
    db.update_metadata(
        2,
        &Metadata::from([("stage".into(), MetadataValue::Integer(2))]),
    )
    .unwrap();
    assert_eq!(
        snapshot.get(2).unwrap().unwrap().metadata["stage"],
        MetadataValue::Integer(1)
    );
    assert_eq!(
        db.get(2).unwrap().unwrap().metadata["stage"],
        MetadataValue::Integer(2)
    );
    db.delete(2).unwrap();
    db.put(3, &[0.0, 0.0], b"new").unwrap();
    assert_eq!(snapshot.get_at(1, 199).unwrap().unwrap().payload, b"before");
    assert!(snapshot.get_at(1, 200).unwrap().is_none());
    assert!(snapshot.get_at(2, 0).unwrap().is_some());
    assert!(snapshot.get_at(3, 0).unwrap().is_none());
    let hits = snapshot
        .search_filtered_with_metric_at(&[1.0, 2.0], 5, &[], DistanceMetric::SquaredL2, 199)
        .unwrap();
    assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), vec![1, 2]);
    assert!(
        hits.iter()
            .all(|h| h.distance.to_bits() == 0.0f64.to_bits())
    );
    assert!(db.read_snapshot().unwrap().sequence() > sequence);
    db.close().unwrap();
    assert_eq!(
        snapshot.browse_at(None, 1, 199).unwrap().next_after,
        Some(1)
    );
}

#[test]
fn snapshots_capture_entire_atomic_batches_and_survive_failed_validation() {
    let mut db = database();
    db.put(1, &[1.0, 0.0], b"old").unwrap();
    let before = db.read_snapshot().unwrap();
    db.write_batch(&[
        Mutation::Delete { id: 1 },
        Mutation::Put {
            id: 2,
            vector: vec![2.0, 0.0],
            payload: vec![],
        },
        Mutation::Put {
            id: 3,
            vector: vec![3.0, 0.0],
            payload: vec![],
        },
    ])
    .unwrap();
    let after = db.read_snapshot().unwrap();
    assert_eq!(
        before
            .browse(None, 100)
            .unwrap()
            .hits
            .iter()
            .map(|h| h.id)
            .collect::<Vec<_>>(),
        vec![1]
    );
    assert_eq!(
        after
            .browse(None, 100)
            .unwrap()
            .hits
            .iter()
            .map(|h| h.id)
            .collect::<Vec<_>>(),
        vec![2, 3]
    );
    assert!(
        db.write_batch(&[
            Mutation::Delete { id: 2 },
            Mutation::Put {
                id: 4,
                vector: vec![f32::NAN, 0.0],
                payload: vec![]
            },
        ])
        .is_err()
    );
    let final_state = db.read_snapshot().unwrap();
    assert_eq!(after.sequence(), final_state.sequence());
    assert_eq!(
        after.browse(None, 100).unwrap(),
        final_state.browse(None, 100).unwrap()
    );
}

#[test]
fn retention_is_bounded_and_released_only_after_last_clone() {
    let db = database();
    let mut captures = Vec::new();
    for _ in 0..vlite::records::MAX_READ_SNAPSHOTS {
        captures.push(db.read_snapshot().unwrap());
    }
    assert!(db.read_snapshot().is_err());
    let clone = captures[0].clone();
    captures.remove(0);
    assert!(db.read_snapshot().is_err());
    drop(clone);
    let capture = db.read_snapshot().unwrap();
    drop(db);
    assert!(capture.is_empty());
}

#[test]
fn snapshot_is_send_sync_and_does_not_block_writes_or_change_distance_bits() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<vlite::ReadSnapshot>();
    let mut db = database();
    for id in 0..32 {
        db.put(id, &[id as f32, 0.25], b"").unwrap();
    }

    let query = [0.25, -0.5];
    let expected = db.search(&query, 32).unwrap();
    let snapshot = db.read_snapshot().unwrap();
    let reader = std::thread::spawn(move || {
        (0..100)
            .map(|_| snapshot.search(&query, 32).unwrap())
            .collect::<Vec<_>>()
    });
    for id in 0..32 {
        db.put(id, &[1000.0, 1000.0], b"new").unwrap();
    }
    for actual in reader.join().unwrap() {
        assert_eq!(actual, expected);
    }
}

#[test]
fn named_snapshot_outlives_collection_drop_and_same_name_recreation() {
    let mut db = database();
    let spec = vlite::CollectionSpec {
        dimensions: 2,
        settings: Default::default(),
    };
    db.create_collection("named", spec.clone()).unwrap();
    db.collection_mut("named")
        .unwrap()
        .put(1, &[1.0, 2.0], b"old collection")
        .unwrap();
    let snapshot = db.collection("named").unwrap().read_snapshot().unwrap();
    db.collection_mut("named").unwrap().delete(1).unwrap();
    db.drop_collection("named").unwrap();
    db.create_collection("named", spec).unwrap();
    db.collection_mut("named")
        .unwrap()
        .put(1, &[9.0, 9.0], b"new collection")
        .unwrap();
    assert_eq!(snapshot.get(1).unwrap().unwrap().payload, b"old collection");
    assert_eq!(
        db.collection("named")
            .unwrap()
            .get(1)
            .unwrap()
            .unwrap()
            .payload,
        b"new collection"
    );
}
