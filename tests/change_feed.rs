use tempfile::tempdir;
use vlite::changes::{ChangeKind, FeedCursor, read_log_changes};
use vlite::{
    CollectionSettings, CollectionSpec, CreateOptions, Database, EncryptionKey, Error,
    FormatVersion, Metadata, MetadataValue, Mutation, maintenance,
};

fn v5(path: &std::path::Path) -> Database {
    Database::create_with_format(path, 2, FormatVersion::V5).unwrap()
}

#[test]
fn feed_reports_every_operation_kind_in_commit_order() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("feed.vlite");
    let mut db = v5(&path);
    db.set_commit_clock(Box::new(|| 1_700_000_000_000));
    db.create_collection(
        "docs",
        CollectionSpec {
            dimensions: 2,
            settings: CollectionSettings::default(),
        },
    )
    .unwrap();
    db.put(1, &[1.0, 0.0], b"a").unwrap();
    let mut metadata = Metadata::new();
    metadata.insert("k".into(), MetadataValue::String("v".into()));
    db.update_metadata(1, &metadata).unwrap();
    db.put_with_expiry(
        2,
        &[0.0, 1.0],
        b"b",
        &Metadata::new(),
        Some(4_000_000_000_000),
    )
    .unwrap();
    db.write_batch(&[
        Mutation::Put {
            id: 3,
            vector: vec![1.0, 1.0],
            payload: b"c".to_vec(),
        },
        Mutation::Delete { id: 1 },
    ])
    .unwrap();
    db.delete(2).unwrap();
    db.collection_mut("docs")
        .unwrap()
        .put(9, &[1.0, 2.0], b"in-docs")
        .unwrap();

    let batch = db.changes_after(&FeedCursor::beginning(), 1000).unwrap();
    assert!(!batch.has_more);
    assert_eq!(batch.head, 7);
    assert_eq!(batch.next.sequence, 7);
    let kinds: Vec<_> = batch
        .changes
        .iter()
        .map(|change| (change.sequence, change.ordinal, change.collection.as_str()))
        .collect();
    assert_eq!(
        kinds,
        vec![
            (1, 0, "docs"),
            (2, 0, "default"),
            (3, 0, "default"),
            (4, 0, "default"),
            (5, 0, "default"),
            (5, 1, "default"),
            (6, 0, "default"),
            (7, 0, "docs"),
        ]
    );
    assert!(matches!(
        batch.changes[0].kind,
        ChangeKind::CollectionCreated { dimensions: 2, .. }
    ));
    assert!(
        matches!(&batch.changes[2].kind, ChangeKind::MetadataUpdate { id: 1, metadata: m } if m == &metadata)
    );
    assert!(matches!(
        &batch.changes[3].kind,
        ChangeKind::Put {
            id: 2,
            expires_at_ms: Some(4_000_000_000_000),
            ..
        }
    ));
    assert!(matches!(
        batch.changes[6].kind,
        ChangeKind::Delete { id: 2 }
    ));
    assert!(
        batch
            .changes
            .iter()
            .all(|change| change.committed_at_ms.is_none())
    );
}

#[test]
fn cursor_resumes_with_bounded_batches_and_never_splits_a_commit() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("feed.vlite");
    let mut db = v5(&path);
    for id in 0..10u64 {
        db.put(id, &[id as f32, 0.0], b"x").unwrap();
    }
    let operations: Vec<_> = (100..105u64)
        .map(|id| Mutation::Put {
            id,
            vector: vec![1.0, 1.0],
            payload: Vec::new(),
        })
        .collect();
    db.write_batch(&operations).unwrap();
    db.put(200, &[0.0, 0.0], b"tail").unwrap();

    let mut cursor = FeedCursor::beginning();
    let mut seen = Vec::new();
    let mut calls = 0;
    loop {
        let batch = db.changes_after(&cursor, 3).unwrap();
        // The 5-event batch exceeds the bound but is still delivered whole.
        assert!(batch.changes.len() <= 5);
        for change in &batch.changes {
            seen.push((change.sequence, change.ordinal));
        }
        cursor = batch.next;
        calls += 1;
        assert!(calls < 50);
        if !batch.has_more {
            break;
        }
    }
    assert_eq!(seen.len(), 10 + 5 + 1);
    assert!(seen.windows(2).all(|pair| pair[0] < pair[1]));
    let empty = db.changes_after(&cursor, 10).unwrap();
    assert!(empty.changes.is_empty());
    assert_eq!(empty.next, cursor);

    let live = db.feed_cursor().unwrap();
    assert_eq!(live, cursor);
    db.delete(0).unwrap();
    let batch = db.changes_after(&live, 10).unwrap();
    assert_eq!(batch.changes.len(), 1);
}

#[test]
fn durable_cursor_and_token_round_trip_and_reject_damage() {
    let directory = tempdir().unwrap();
    let cursor_path = directory.path().join("cursor");
    assert_eq!(FeedCursor::load(&cursor_path).unwrap(), None);
    let cursor = FeedCursor {
        lineage: 0xABCDEF,
        sequence: 42,
    };
    cursor.save(&cursor_path).unwrap();
    assert_eq!(FeedCursor::load(&cursor_path).unwrap(), Some(cursor));
    assert_eq!(FeedCursor::from_token(&cursor.to_token()).unwrap(), cursor);
    let mut token = cursor.to_token();
    token.replace_range(6..7, "f");
    assert!(matches!(
        FeedCursor::from_token(&token),
        Err(Error::InvalidCursor(_))
    ));
    assert!(FeedCursor::from_token("garbage").is_err());
    std::fs::write(&cursor_path, b"vlc1.zz").unwrap();
    assert!(FeedCursor::load(&cursor_path).is_err());
}

#[test]
fn compaction_and_future_cursors_are_clear_errors() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("feed.vlite");
    let compacted = directory.path().join("compacted.vlite");
    let mut db = v5(&path);
    for id in 0..5u64 {
        db.put(id, &[1.0, id as f32], b"x").unwrap();
    }
    let cursor = db.feed_cursor().unwrap();
    assert!(matches!(
        db.changes_after(
            &FeedCursor {
                sequence: 99,
                ..cursor
            },
            10
        ),
        Err(Error::CursorAhead {
            cursor: 99,
            head: 5
        })
    ));
    db.close().unwrap();

    maintenance::compact(&path, &compacted).unwrap();
    let compacted_db = Database::open(&compacted).unwrap();
    assert!(matches!(
        compacted_db.changes_after(&cursor, 10),
        Err(Error::CursorCompacted { .. })
    ));
    // The beginning of the new history is always readable.
    assert!(
        !compacted_db
            .changes_after(&FeedCursor::beginning(), 10)
            .unwrap()
            .changes
            .is_empty()
    );
    assert!(matches!(
        compacted_db.changes_after(
            &FeedCursor {
                lineage: 0,
                sequence: 3
            },
            10
        ),
        Err(Error::InvalidCursor(_))
    ));
}

#[test]
fn old_formats_and_closed_files_are_handled() {
    let directory = tempdir().unwrap();
    let old = directory.path().join("v3.vlite");
    let mut db = Database::create_with_format(&old, 2, FormatVersion::V3).unwrap();
    db.put(1, &[1.0, 0.0], b"x").unwrap();
    assert!(matches!(
        db.changes_after(&FeedCursor::beginning(), 10),
        Err(Error::ChangeFeedRequiresUpgrade)
    ));
    assert!(matches!(
        db.feed_cursor(),
        Err(Error::ChangeFeedRequiresUpgrade)
    ));
    db.close().unwrap();

    let path = directory.path().join("v5.vlite");
    let mut db = v5(&path);
    db.put(1, &[1.0, 0.0], b"x").unwrap();
    db.close().unwrap();
    let batch = read_log_changes(&path, None, &FeedCursor::beginning(), 10).unwrap();
    assert_eq!(batch.changes.len(), 1);
}

#[test]
fn timestamps_and_encryption_are_supported() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("enc.vlite");
    let key = EncryptionKey::from_bytes([5; 32]);
    let mut db = Database::create_with_options(
        &path,
        2,
        CreateOptions {
            encryption: Some(key.clone()),
            commit_timestamps: true,
            ..CreateOptions::default()
        },
    )
    .unwrap();
    let mut now = 1000u64;
    db.set_commit_clock(Box::new(move || {
        now += 10;
        now
    }));
    db.put(1, &[1.0, 0.0], b"secret").unwrap();
    db.put(2, &[0.0, 1.0], b"secret").unwrap();
    let batch = db.changes_after(&FeedCursor::beginning(), 10).unwrap();
    assert_eq!(
        batch
            .changes
            .iter()
            .map(|change| change.committed_at_ms)
            .collect::<Vec<_>>(),
        vec![Some(1010), Some(1020)]
    );
    db.close().unwrap();
    assert!(matches!(
        read_log_changes(&path, None, &FeedCursor::beginning(), 10),
        Err(Error::EncryptionKeyRequired)
    ));
    let batch = read_log_changes(&path, Some(&key), &FeedCursor::beginning(), 10).unwrap();
    assert_eq!(batch.changes.len(), 2);
}
