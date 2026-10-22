use std::fs;
use std::path::Path;
use tempfile::tempdir;
use vlite::maintenance::{self, RestoreTarget, restore_to};
use vlite::{CreateOptions, Database, EncryptionKey, Error, FormatVersion};

fn timestamped(path: &Path, key: Option<EncryptionKey>) -> Database {
    let mut db = Database::create_with_options(
        path,
        2,
        CreateOptions {
            encryption: key,
            commit_timestamps: true,
            ..CreateOptions::default()
        },
    )
    .unwrap();
    let mut now = 1_000u64;
    db.set_commit_clock(Box::new(move || {
        now += 100;
        now
    }));
    db
}

/// Commits ids 1..=count; commit n happens at 1000 + 100 * n.
fn fill(db: &mut Database, count: u64) {
    for id in 1..=count {
        db.put(id, &[id as f32, 1.0], format!("p{id}").as_bytes())
            .unwrap();
    }
}

fn ids(path: &Path, key: Option<&EncryptionKey>) -> Vec<u64> {
    let db = match key {
        Some(key) => Database::open_encrypted(path, key).unwrap(),
        None => Database::open(path).unwrap(),
    };
    let mut ids: Vec<u64> = db
        .search(&[1.0, 1.0], 100)
        .unwrap()
        .into_iter()
        .map(|hit| hit.id)
        .collect();
    ids.sort_unstable();
    ids
}

#[test]
fn restores_to_a_revision_without_touching_the_source() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source.vlite");
    let mut db = timestamped(&source, None);
    fill(&mut db, 6);
    db.delete(2).unwrap();
    db.close().unwrap();
    let before = fs::read(&source).unwrap();

    let out = directory.path().join("at3.vlite");
    let report = restore_to(None, &source, &out, RestoreTarget::Sequence(3), None).unwrap();
    assert_eq!(report.restored_sequence, 3);
    assert_eq!(report.source_head, 7);
    assert_eq!(report.restored_at_ms, Some(1300));
    assert_eq!(ids(&out, None), vec![1, 2, 3]);

    let out = directory.path().join("at7.vlite");
    restore_to(None, &source, &out, RestoreTarget::Sequence(7), None).unwrap();
    assert_eq!(ids(&out, None), vec![1, 3, 4, 5, 6]);

    let out = directory.path().join("at0.vlite");
    restore_to(None, &source, &out, RestoreTarget::Sequence(0), None).unwrap();
    assert!(ids(&out, None).is_empty());

    assert_eq!(fs::read(&source).unwrap(), before);
    // The restored file is a fresh lineage, so old feed cursors cannot be reused.
    let restored = Database::open(directory.path().join("at3.vlite")).unwrap();
    let original = Database::open(&source).unwrap();
    assert_ne!(
        restored.feed_cursor().unwrap().lineage,
        original.feed_cursor().unwrap().lineage
    );
}

#[test]
fn restores_to_a_timestamp_between_commits() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source.vlite");
    let mut db = timestamped(&source, None);
    fill(&mut db, 5);
    db.close().unwrap();

    let cases = [
        (1_099, 0u64),
        (1_100, 1),
        (1_299, 2),
        (1_300, 3),
        (1_550, 5),
        (u64::MAX, 5),
    ];
    for (timestamp, expected) in cases {
        let out = directory.path().join(format!("t{timestamp}.vlite"));
        let report = restore_to(
            None,
            &source,
            &out,
            RestoreTarget::TimestampMs(timestamp),
            None,
        )
        .unwrap();
        assert_eq!(report.restored_sequence, expected, "timestamp {timestamp}");
        assert_eq!(ids(&out, None).len() as u64, expected);
    }
}

#[test]
fn restored_timestamped_file_keeps_accepting_writes_and_restores() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source.vlite");
    let mut db = timestamped(&source, None);
    fill(&mut db, 4);
    db.close().unwrap();
    let out = directory.path().join("out.vlite");
    restore_to(None, &source, &out, RestoreTarget::Sequence(2), None).unwrap();
    let mut db = Database::open(&out).unwrap();
    assert!(db.has_commit_timestamps());
    db.put(50, &[1.0, 1.0], b"new").unwrap();
    db.close().unwrap();
    assert_eq!(ids(&out, None), vec![1, 2, 50]);
    maintenance::verify(&out).unwrap();
}

#[test]
fn base_backup_must_be_a_prefix_of_the_log() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source.vlite");
    let base = directory.path().join("base.vlite");
    let mut db = timestamped(&source, None);
    fill(&mut db, 3);
    db.close().unwrap();
    maintenance::backup(&source, &base).unwrap();
    let mut db = Database::open(&source).unwrap();
    db.set_commit_clock(Box::new(|| 9_000));
    for id in 10..14 {
        db.put(id, &[1.0, 1.0], b"later").unwrap();
    }
    db.close().unwrap();

    let out = directory.path().join("ok.vlite");
    let report = restore_to(Some(&base), &source, &out, RestoreTarget::Sequence(5), None).unwrap();
    assert_eq!(report.restored_sequence, 5);
    assert_eq!(ids(&out, None), vec![1, 2, 3, 10, 11]);

    let out = directory.path().join("too-old.vlite");
    assert!(matches!(
        restore_to(Some(&base), &source, &out, RestoreTarget::Sequence(2), None),
        Err(Error::InvalidRestoreTarget(_))
    ));
    assert!(!out.exists());

    // Same lineage name, different bytes: not a prefix.
    let forked = directory.path().join("forked.vlite");
    let mut tampered = fs::read(&base).unwrap();
    let last = tampered.len() - 20;
    tampered[last] ^= 1;
    fs::write(&forked, tampered).unwrap();
    let out = directory.path().join("forked-out.vlite");
    assert!(
        restore_to(
            Some(&forked),
            &source,
            &out,
            RestoreTarget::Sequence(5),
            None
        )
        .is_err()
    );
    assert!(!out.exists());

    // A compacted copy has a new lineage and cannot serve as a base.
    let compacted = directory.path().join("compacted.vlite");
    maintenance::compact(&base, &compacted).unwrap();
    let out = directory.path().join("compacted-out.vlite");
    assert!(matches!(
        restore_to(
            Some(&compacted),
            &source,
            &out,
            RestoreTarget::Sequence(5),
            None
        ),
        Err(Error::InvalidRestoreTarget(_))
    ));
}

#[test]
fn invalid_targets_and_untimestamped_logs_are_reported() {
    let directory = tempdir().unwrap();
    let plain = directory.path().join("plain.vlite");
    let mut db = Database::create_with_format(&plain, 2, FormatVersion::V5).unwrap();
    fill(&mut db, 3);
    db.close().unwrap();
    let out = directory.path().join("out.vlite");
    assert!(matches!(
        restore_to(None, &plain, &out, RestoreTarget::TimestampMs(5), None),
        Err(Error::TimestampsUnavailable)
    ));
    assert!(matches!(
        restore_to(None, &plain, &out, RestoreTarget::Sequence(99), None),
        Err(Error::InvalidRestoreTarget(_))
    ));
    assert!(!out.exists());
    // Revision restore still works without timestamps.
    restore_to(None, &plain, &out, RestoreTarget::Sequence(2), None).unwrap();
    assert_eq!(ids(&out, None), vec![1, 2]);
    // An existing destination is never overwritten.
    assert!(restore_to(None, &plain, &out, RestoreTarget::Sequence(1), None).is_err());
    assert_eq!(ids(&out, None), vec![1, 2]);
}

#[test]
fn encrypted_logs_restore_with_the_key_only() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("enc.vlite");
    let key = EncryptionKey::from_bytes([3; 32]);
    let mut db = timestamped(&source, Some(key.clone()));
    fill(&mut db, 4);
    db.close().unwrap();
    let out = directory.path().join("out.vlite");
    assert!(matches!(
        restore_to(None, &source, &out, RestoreTarget::Sequence(2), None),
        Err(Error::EncryptionKeyRequired)
    ));
    assert!(matches!(
        restore_to(
            None,
            &source,
            &out,
            RestoreTarget::Sequence(2),
            Some(&EncryptionKey::from_bytes([4; 32]))
        ),
        Err(Error::EncryptionKeyMismatch { .. })
    ));
    assert!(!out.exists());
    restore_to(
        None,
        &source,
        &out,
        RestoreTarget::TimestampMs(1_250),
        Some(&key),
    )
    .unwrap();
    assert_eq!(ids(&out, Some(&key)), vec![1, 2]);
    assert!(Database::open(&out).is_err());
}
