use std::fs;
use tempfile::tempdir;
use vlite::maintenance::{self, export_log_segment};
use vlite::{
    CollectionSettings, CollectionSpec, CreateOptions, Database, EncryptionKey, Error,
    FormatVersion, LogSegment, Mutation, ReplicaFollower,
};

fn ids(db: &Database) -> Vec<u64> {
    let mut ids: Vec<u64> = db
        .search(&[1.0, 1.0], 1000)
        .unwrap()
        .into_iter()
        .map(|hit| hit.id)
        .collect();
    ids.sort_unstable();
    ids
}

fn primary(path: &std::path::Path) -> Database {
    Database::create_with_format(path, 2, FormatVersion::V5).unwrap()
}

#[test]
fn follower_bootstraps_and_tails_in_order_with_lag_reporting() {
    let directory = tempdir().unwrap();
    let mut primary = primary(&directory.path().join("primary.vlite"));
    for id in 0..5u64 {
        primary.put(id, &[1.0, id as f32], b"x").unwrap();
    }
    let first = primary.export_log(0, 1 << 20).unwrap();
    assert_eq!(first.last_sequence, 5);
    let follower_path = directory.path().join("follower.vlite");
    let mut follower = ReplicaFollower::create(&follower_path, &first, None).unwrap();
    assert_eq!(follower.db().len(), 5);
    assert_eq!(follower.status().lag, 0);

    for id in 5..12u64 {
        primary.put(id, &[1.0, id as f32], b"x").unwrap();
    }
    primary.delete(0).unwrap();
    primary.collection_mut("default").unwrap();
    primary
        .create_collection(
            "docs",
            CollectionSpec {
                dimensions: 2,
                settings: CollectionSettings::default(),
            },
        )
        .unwrap();
    primary
        .collection_mut("docs")
        .unwrap()
        .put(1, &[1.0, 1.0], b"d")
        .unwrap();

    // Bounded segments apply in order; a small bound forces several.
    let mut applied_segments = 0;
    loop {
        let segment = primary
            .export_log(follower.applied_sequence(), 200)
            .unwrap();
        let status = follower.apply(&segment).unwrap();
        if segment.is_empty() {
            assert_eq!(status.lag, 0);
            break;
        }
        applied_segments += 1;
        assert_eq!(status.primary_head, 15);
        assert_eq!(status.lag, 15 - status.applied_sequence);
        assert!(applied_segments < 30);
    }
    assert!(applied_segments > 1);
    assert_eq!(ids(follower.db()), ids(&primary));
    assert_eq!(follower.db().collection("docs").unwrap().len(), 1);

    // Byte-identical log, reopenable and verifiable independently.
    let follower_bytes = {
        drop(follower);
        fs::read(&follower_path).unwrap()
    };
    primary.close().unwrap();
    let primary_bytes = fs::read(directory.path().join("primary.vlite")).unwrap();
    assert_eq!(follower_bytes, primary_bytes);
    maintenance::verify(&follower_path).unwrap();
}

#[test]
fn followers_refuse_writes_and_bad_segments_leave_state_unchanged() {
    let directory = tempdir().unwrap();
    let mut primary = primary(&directory.path().join("p.vlite"));
    primary.put(1, &[1.0, 1.0], b"a").unwrap();
    let first = primary.export_log(0, 1 << 20).unwrap();
    let mut follower =
        ReplicaFollower::create(directory.path().join("f.vlite"), &first, None).unwrap();
    assert!(matches!(
        follower.put(2, &[1.0, 1.0], b"x"),
        Err(Error::ReadOnlyReplica)
    ));
    assert!(matches!(follower.delete(1), Err(Error::ReadOnlyReplica)));
    assert!(matches!(
        follower.write_batch(&[Mutation::Delete { id: 1 }]),
        Err(Error::ReadOnlyReplica)
    ));

    primary.put(2, &[1.0, 1.0], b"b").unwrap();
    primary.put(3, &[1.0, 1.0], b"c").unwrap();
    let good = primary.export_log(1, 1 << 20).unwrap();

    // Gap: segment starting after the applied revision.
    let later = LogSegment {
        base_sequence: 2,
        ..good.clone()
    };
    assert!(matches!(
        follower.apply(&later),
        Err(Error::ReplicaDiverged(_))
    ));
    // Wrong lineage.
    let foreign = LogSegment {
        lineage: good.lineage ^ 1,
        ..good.clone()
    };
    assert!(matches!(
        follower.apply(&foreign),
        Err(Error::ReplicaDiverged(_))
    ));
    // Corrupted frame bytes fail checksum validation and roll back.
    let mut damaged = good.clone();
    let middle = damaged.frames.len() / 2;
    damaged.frames[middle] ^= 0x40;
    assert!(follower.apply(&damaged).is_err());
    assert_eq!(follower.applied_sequence(), 1);
    assert_eq!(follower.db().len(), 1);
    // Truncated frame bytes also roll back.
    let mut short = good.clone();
    short.frames.truncate(short.frames.len() - 5);
    assert!(follower.apply(&short).is_err());
    assert_eq!(follower.applied_sequence(), 1);
    // The file on disk is still exactly the bootstrap prefix and the good
    // segment still applies afterwards.
    follower.apply(&good).unwrap();
    assert_eq!(follower.db().len(), 3);
    // Re-applying is rejected (not idempotent by design: callers resume from status).
    assert!(follower.apply(&good).is_err());
}

#[test]
fn file_shipping_serializes_segments_and_survives_reopen() {
    let directory = tempdir().unwrap();
    let primary_path = directory.path().join("p.vlite");
    let mut primary = primary(&primary_path);
    for id in 0..4u64 {
        primary.put(id, &[1.0, 1.0], b"x").unwrap();
    }
    let segment = primary.export_log(0, 1 << 20).unwrap();
    let shipped = directory.path().join("seg-0.vseg");
    segment.save(&shipped).unwrap();
    assert_eq!(LogSegment::load(&shipped).unwrap(), segment);

    let follower_path = directory.path().join("f.vlite");
    let mut follower =
        ReplicaFollower::create(&follower_path, &LogSegment::load(&shipped).unwrap(), None)
            .unwrap();
    primary.put(10, &[1.0, 1.0], b"more").unwrap();
    primary
        .export_log(4, 1 << 20)
        .unwrap()
        .save(directory.path().join("seg-1.vseg"))
        .unwrap();
    follower
        .apply_file(directory.path().join("seg-1.vseg"))
        .unwrap();
    assert_eq!(follower.applied_sequence(), 5);
    drop(follower);

    // Tampering with a shipped file is detected before anything is applied.
    let mut bytes = fs::read(&shipped).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 1;
    assert!(LogSegment::from_bytes(&bytes).is_err());

    let mut reopened = ReplicaFollower::open(&follower_path, None).unwrap();
    assert_eq!(reopened.applied_sequence(), 5);
    primary.put(11, &[1.0, 1.0], b"newer").unwrap();
    primary.close().unwrap();

    // Same-machine tail of the (now closed) primary log.
    let status = reopened.sync_from_log(&primary_path, 1 << 20).unwrap();
    assert_eq!(status.applied_sequence, 6);
    assert_eq!(status.lag, 0);
    assert_eq!(reopened.db().len(), 6);
    let promoted = reopened.promote();
    assert_eq!(promoted.len(), 6);
}

#[test]
fn follower_recovers_from_an_interrupted_apply_and_from_a_backup_copy() {
    let directory = tempdir().unwrap();
    let primary_path = directory.path().join("p.vlite");
    let mut primary = primary(&primary_path);
    for id in 0..3u64 {
        primary.put(id, &[1.0, 1.0], b"x").unwrap();
    }
    let base = directory.path().join("base.vlite");
    // Bootstrap from a byte copy of the primary (same lineage).
    primary.close().unwrap();
    maintenance::backup(&primary_path, &base).unwrap();
    let mut primary = Database::open(&primary_path).unwrap();
    primary.put(7, &[1.0, 1.0], b"y").unwrap();
    let segment = primary.export_log(3, 1 << 20).unwrap();

    // Simulate a crash in the middle of an apply: partial frame at the tail.
    let mut bytes = fs::read(&base).unwrap();
    bytes.extend_from_slice(&segment.frames[..segment.frames.len() / 2]);
    fs::write(&base, bytes).unwrap();
    let mut follower = ReplicaFollower::open(&base, None).unwrap();
    assert_eq!(follower.applied_sequence(), 3);
    follower.apply(&segment).unwrap();
    assert_eq!(follower.db().len(), 4);
}

#[test]
fn encrypted_replication_ships_ciphertext_and_needs_the_key() {
    let directory = tempdir().unwrap();
    let key = EncryptionKey::from_bytes([8; 32]);
    let mut primary = Database::create_with_options(
        directory.path().join("p.vlite"),
        2,
        CreateOptions {
            encryption: Some(key.clone()),
            commit_timestamps: true,
            ..CreateOptions::default()
        },
    )
    .unwrap();
    primary.put(1, &[1.0, 1.0], b"top-secret").unwrap();
    let first = primary.export_log(0, 1 << 20).unwrap();
    assert!(
        !first
            .to_bytes()
            .windows(10)
            .any(|window| window == b"top-secret")
    );
    let path = directory.path().join("f.vlite");
    assert!(matches!(
        ReplicaFollower::create(&path, &first, None),
        Err(Error::EncryptionKeyRequired)
    ));
    assert!(!path.exists());
    let mut follower = ReplicaFollower::create(&path, &first, Some(&key)).unwrap();
    primary.put(2, &[1.0, 1.0], b"more").unwrap();
    follower
        .apply(
            &primary
                .export_log(follower.applied_sequence(), 1 << 20)
                .unwrap(),
        )
        .unwrap();
    assert_eq!(follower.db().len(), 2);
    assert_eq!(
        follower.db().get(1).unwrap().unwrap().payload,
        b"top-secret"
    );
    drop(follower);
    primary.close().unwrap();
    let segment =
        export_log_segment(directory.path().join("p.vlite"), Some(&key), 1, 1 << 20).unwrap();
    assert_eq!(segment.last_sequence, 2);
}
