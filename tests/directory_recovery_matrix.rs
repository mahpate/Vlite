use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use tempfile::tempdir;
use vlite::{
    CollectionSettings, CollectionSpec, CreateOptions, Database, EncryptionKey, Error, Expected,
    Metadata, MetadataValue, Mutation, maintenance,
};

fn spec(dimensions: usize) -> CollectionSpec {
    CollectionSpec {
        dimensions,
        settings: CollectionSettings::default(),
    }
}

fn metadata(value: &str) -> Metadata {
    Metadata::from([("stage".into(), MetadataValue::String(value.into()))])
}

fn apply_round(db: &mut Database, round: u64) {
    match round {
        0 => {
            db.create_collection("docs", spec(3)).unwrap();
            db.create_collection("scratch", spec(1)).unwrap();
            db.write_batch(&[
                Mutation::Put {
                    id: 1,
                    vector: vec![1.0, 0.0],
                    payload: b"first".to_vec(),
                },
                Mutation::PutWithMetadata {
                    id: 2,
                    vector: vec![0.0, 2.0],
                    payload: b"second".to_vec(),
                    metadata: metadata("initial"),
                },
            ])
            .unwrap();
            db.collection_mut("docs")
                .unwrap()
                .put_with_metadata(1, &[1.0, 2.0, 3.0], b"named", &metadata("named"))
                .unwrap();
            db.collection_mut("scratch")
                .unwrap()
                .put(1, &[1.0], b"temporary")
                .unwrap();
        }
        1 => {
            let revision = db.revision(2).unwrap().unwrap();
            db.put_with_metadata_if(
                2,
                &[2.0, 2.0],
                b"replacement",
                &metadata("updated"),
                Expected::Revision(revision),
            )
            .unwrap();
            assert!(db.delete(1).unwrap());
            assert!(!db.delete(8).unwrap());
            let mut docs = db.collection_mut("docs").unwrap();
            assert!(docs.delete(1).unwrap());
            docs.put(2, &[3.0, 2.0, 1.0], b"next").unwrap();
            assert!(db.collection_mut("scratch").unwrap().delete(1).unwrap());
            db.drop_collection("scratch").unwrap();
        }
        2 => {
            assert!(db.collection_mut("docs").unwrap().delete(2).unwrap());
            let old_id = db.collection("docs").unwrap().id();
            db.drop_collection("docs").unwrap();
            assert!(db.create_collection("docs", spec(1)).unwrap().id > old_id);
            db.collection_mut("docs")
                .unwrap()
                .put(1, &[9.0], b"new namespace")
                .unwrap();
            db.create_collection("empty", spec(4)).unwrap();
            db.put(1, &[4.0, 1.0], b"reinserted").unwrap();
        }
        _ => unreachable!(),
    }
    db.put(4 + round, &[round as f32, 1.0], b"round").unwrap();
}

fn assert_equivalent(left: &Database, right: &Database) {
    assert_eq!(left.format_version(), right.format_version());
    assert_eq!(left.len(), right.len());
    let collections = left.collections().unwrap();
    assert_eq!(collections, right.collections().unwrap());
    for info in collections {
        let a = left.collection(&info.name).unwrap();
        let b = right.collection(&info.name).unwrap();
        for id in 0..=10 {
            assert_eq!(a.revision(id).unwrap(), b.revision(id).unwrap());
            assert_eq!(a.expires_at(id).unwrap(), b.expires_at(id).unwrap());
            for now in [0, 99, 100, 200, u64::MAX] {
                assert_eq!(
                    a.get_at(id, now).unwrap(),
                    b.get_at(id, now).unwrap(),
                    "collection={}, id={id}, clock={now}",
                    info.name
                );
            }
        }
    }
    for now in [0, 100, u64::MAX] {
        assert_eq!(
            left.search_at(&[0.0, 0.0], 20, now).unwrap(),
            right.search_at(&[0.0, 0.0], 20, now).unwrap()
        );
        let a = left.browse_at(None, 20, now).unwrap();
        let b = right.browse_at(None, 20, now).unwrap();
        assert_eq!(a.hits, b.hits);
        assert_eq!(a.next_after, b.next_after);
    }
}

#[test]
fn mixed_collection_history_matches_single_file_across_three_checkpoints() {
    let root = tempdir().unwrap();
    let file = root.path().join("source.vlite");
    let dir = root.path().join("store");
    let mut source = Database::create_with_options(&file, 2, CreateOptions::default()).unwrap();
    source.put(9, &[9.0, 0.0], b"conversion seed").unwrap();
    let lineage = source.lineage();
    source.close().unwrap();
    let original = fs::read(&file).unwrap();
    maintenance::convert_to_directory(&file, &dir).unwrap();
    assert_eq!(fs::read(&file).unwrap(), original);

    for round in 0..3 {
        let mut single = Database::open(&file).unwrap();
        let mut directory = Database::open(&dir).unwrap();
        directory.set_checkpoint_threshold(None);
        assert_equivalent(&single, &directory);
        apply_round(&mut single, round);
        apply_round(&mut directory, round);
        assert_equivalent(&single, &directory);
        assert!(directory.checkpoint().unwrap().is_some());
        assert_eq!(directory.checkpoint().unwrap(), None);
        single.close().unwrap();
        directory.close().unwrap();

        let single = Database::open_read_only(&file).unwrap();
        let directory = Database::open_read_only(&dir).unwrap();
        assert_eq!(directory.lineage(), lineage);
        assert_equivalent(&single, &directory);
    }
}

fn assert_expiry_state(db: &Database, revisions: [u64; 3], reclaimed: bool) {
    assert_eq!(db.revision(1).unwrap(), Some(revisions[0]));
    assert_eq!(db.get_at(1, u64::MAX).unwrap().unwrap().payload, b"live");
    if reclaimed {
        assert_eq!(db.revision(2).unwrap(), None);
        assert_eq!(db.expires_at(2).unwrap(), None);
        assert!(db.get_at(2, 0).unwrap().is_none());
    } else {
        assert_eq!(db.revision(2).unwrap(), Some(revisions[1]));
        assert_eq!(db.expires_at(2).unwrap(), Some(100));
        let record = db.get_at(2, 99).unwrap().unwrap();
        assert_eq!(record.vector, [2.0, 0.0]);
        assert_eq!(record.payload, b"expired");
        assert_eq!(record.metadata, metadata("retained"));
    }
    assert!(db.get_at(2, 100).unwrap().is_none());
    let docs = db.collection("docs").unwrap();
    assert_eq!(docs.revision(2).unwrap(), Some(revisions[2]));
    let deadline = u64::MAX / 2;
    assert_eq!(docs.expires_at(2).unwrap(), Some(deadline));
    assert_eq!(
        docs.get_at(2, deadline - 1).unwrap().unwrap().payload,
        b"named expiry"
    );
    assert!(docs.get_at(2, deadline).unwrap().is_none());
    assert_eq!(
        db.search_at(&[2.0, 0.0], 10, 99)
            .unwrap()
            .iter()
            .map(|hit| hit.id)
            .collect::<Vec<_>>(),
        if reclaimed { vec![1] } else { vec![2, 1] }
    );
    assert_eq!(
        db.browse_at(None, 10, 100)
            .unwrap()
            .hits
            .iter()
            .map(|hit| hit.id)
            .collect::<Vec<_>>(),
        [1]
    );
}

#[test]
fn checkpoint_reclaims_expired_records_but_preserves_future_expiry_and_live_revisions() {
    let root = tempdir().unwrap();
    let dir = root.path().join("store");
    let mut db = Database::create_directory(&dir, 2, CreateOptions::default()).unwrap();
    db.set_checkpoint_threshold(None);
    db.create_collection("docs", spec(1)).unwrap();
    db.put(1, &[1.0, 0.0], b"live").unwrap();
    let live = db.revision(1).unwrap().unwrap();
    let expired = db
        .put_with_expiry(2, &[2.0, 0.0], b"expired", &metadata("retained"), Some(100))
        .unwrap();
    let named = db
        .collection_mut("docs")
        .unwrap()
        .put_with_expiry(
            2,
            &[2.0],
            b"named expiry",
            &Metadata::new(),
            Some(u64::MAX / 2),
        )
        .unwrap();
    let revisions = [live, expired, named];
    assert_expiry_state(&db, revisions, false);
    for round in 0..2 {
        assert!(db.checkpoint().unwrap().is_some());
        db.close().unwrap();
        db = Database::open(&dir).unwrap();
        db.set_checkpoint_threshold(None);
        assert_expiry_state(&db, revisions, true);
        db.put(4 + round, &[8.0, 0.0], b"later commit").unwrap();
        assert!(db.delete(4 + round).unwrap());
    }
    assert!(matches!(
        db.put_with_metadata_if(
            2,
            &[0.0, 1.0],
            b"stale",
            &Metadata::new(),
            Expected::Revision(expired)
        ),
        Err(Error::RevisionMismatch { current: None })
    ));
    let replacement = db
        .put_with_metadata_if(
            2,
            &[0.0, 1.0],
            b"revived",
            &Metadata::new(),
            Expected::Absent,
        )
        .unwrap();
    assert!(replacement > named);
    assert_eq!(db.expires_at(2).unwrap(), None);
    assert!(db.delete_if(1, Expected::Revision(live)).unwrap());
    assert!(
        db.collection_mut("docs")
            .unwrap()
            .delete_if(2, Expected::Revision(named))
            .unwrap()
    );
    db.checkpoint().unwrap();
    db.close().unwrap();
    let db = Database::open_read_only(&dir).unwrap();
    assert_eq!(db.revision(2).unwrap(), Some(replacement));
    assert_eq!(db.get_at(2, u64::MAX).unwrap().unwrap().payload, b"revived");
    assert_eq!(db.revision(1).unwrap(), None);
    assert_eq!(db.collection("docs").unwrap().revision(2).unwrap(), None);
}

fn directory_bytes(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().into_string().unwrap(),
                fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

#[test]
fn encrypted_packed_backup_matches_image_plus_active_log_without_modifying_source() {
    let root = tempdir().unwrap();
    let dir = root.path().join("store");
    let packed = root.path().join("packed.vlite");
    let key = EncryptionKey::from_bytes([17; 32]);
    let mut db = Database::create_directory(
        &dir,
        2,
        CreateOptions {
            encryption: Some(key.clone()),
            ..CreateOptions::default()
        },
    )
    .unwrap();
    db.set_checkpoint_threshold(None);
    apply_round(&mut db, 0);
    db.checkpoint().unwrap();
    apply_round(&mut db, 1);
    db.put_with_expiry(
        9,
        &[9.0, 0.0],
        b"expiring backup record",
        &metadata("backup"),
        Some(u64::MAX),
    )
    .unwrap();
    let lineage = db.lineage();
    db.close().unwrap();
    let before = directory_bytes(&dir);
    maintenance::backup_with_key(&dir, &packed, &key).unwrap();
    assert_eq!(directory_bytes(&dir), before);
    assert!(Database::open(&packed).is_err());
    let directory = Database::open_read_only_encrypted(&dir, &key).unwrap();
    let backup = Database::open_read_only_encrypted(&packed, &key).unwrap();
    assert!(!backup.is_directory());
    assert_eq!(backup.lineage(), lineage);
    assert_equivalent(&directory, &backup);
    drop(directory);
    drop(backup);

    let mut directory = Database::open_encrypted(&dir, &key).unwrap();
    let mut backup = Database::open_encrypted(&packed, &key).unwrap();
    apply_round(&mut directory, 2);
    apply_round(&mut backup, 2);
    assert_equivalent(&directory, &backup);
    directory.checkpoint().unwrap();
    directory.close().unwrap();
    backup.close().unwrap();
    let directory = Database::open_read_only_encrypted(&dir, &key).unwrap();
    let backup = Database::open_read_only_encrypted(&packed, &key).unwrap();
    assert_equivalent(&directory, &backup);
}

fn sealed_fixture(dir: &Path) -> PathBuf {
    let mut db = Database::create_directory(dir, 2, CreateOptions::default()).unwrap();
    db.set_checkpoint_threshold(None);
    db.put(1, &[1.0, 0.0], b"sealed record").unwrap();
    db.checkpoint().unwrap();
    db.put(2, &[0.0, 1.0], b"active record").unwrap();
    db.close().unwrap();
    let text = fs::read_to_string(dir.join("MANIFEST")).unwrap();
    let listed: Vec<_> = text
        .lines()
        .filter_map(|line| line.strip_prefix("file "))
        .collect();
    assert!(
        listed.len() >= 2,
        "fixture must have sealed and active data"
    );
    dir.join(listed[0])
}

fn assert_rejected_unchanged(dir: &Path) {
    let before = directory_bytes(dir);
    assert!(Database::open_read_only(dir).is_err());
    assert_eq!(directory_bytes(dir), before);
    assert!(Database::open(dir).is_err());
    assert_eq!(directory_bytes(dir), before);
    // Explicit active-tail repair must not discard unrecoverable sealed data.
    assert!(Database::discard_incomplete_tail(dir).is_err());
    assert_eq!(directory_bytes(dir), before);
}

#[test]
fn missing_manifest_listed_sealed_data_is_not_recreated() {
    let root = tempdir().unwrap();
    let dir = root.path().join("store");
    let sealed = sealed_fixture(&dir);
    fs::remove_file(&sealed).unwrap();
    assert_rejected_unchanged(&dir);
    assert!(!sealed.exists());
}

#[test]
fn corrupt_or_truncated_sealed_data_cannot_be_repaired_as_an_active_tail() {
    let root = tempdir().unwrap();
    for damage in ["checksum", "truncated"] {
        let dir = root.path().join(damage);
        let sealed = sealed_fixture(&dir);
        let mut bytes = fs::read(&sealed).unwrap();
        assert!(bytes.len() > 5);
        match damage {
            "checksum" => *bytes.last_mut().unwrap() ^= 0x80,
            "truncated" => bytes.truncate(bytes.len() - 5),
            _ => unreachable!(),
        }
        fs::write(&sealed, bytes).unwrap();
        assert_rejected_unchanged(&dir);
    }
}
