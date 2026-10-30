//! Speed P2a directory store: create/reopen, checkpoints (explicit and
//! automatic), crash leftovers, encryption, graphs, locking, tail repair and
//! the single-file converters.

use std::fs;
use std::path::Path;

use tempfile::tempdir;
use vlite::changes::FeedCursor;
use vlite::maintenance;
use vlite::{CreateOptions, Database, EncryptionKey, Error, Expected, FormatVersion, Metadata};

fn files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn manifest(dir: &Path) -> String {
    fs::read_to_string(dir.join("MANIFEST")).unwrap()
}

fn fill(db: &mut Database, from: u64, to: u64) {
    for id in from..to {
        db.put(id, &[id as f32, 1.0], format!("p{id}").as_bytes())
            .unwrap();
    }
}

#[test]
fn create_reopen_and_layout() {
    let root = tempdir().unwrap();
    let dir = root.path().join("store");
    let mut db = Database::create_directory(&dir, 2, CreateOptions::default()).unwrap();
    assert!(db.is_directory());
    fill(&mut db, 1, 5);
    db.close().unwrap();
    assert_eq!(files(&dir), ["LOCK", "MANIFEST", "wal-1.log"]);
    assert!(manifest(&dir).starts_with("vlite-manifest 1\nfile wal-1.log\ncrc "));

    let db = Database::open(&dir).unwrap();
    assert!(db.is_directory());
    assert_eq!(db.len(), 4);
    assert_eq!(db.get(3).unwrap().unwrap().payload, b"p3");
    assert_eq!(db.revision(4).unwrap(), Some(4));
    assert!(db.lineage().is_some());
    // A second writer is refused while the first holds the lock.
    assert!(matches!(Database::open(&dir), Err(Error::Locked)));
    db.close().unwrap();

    // Existing paths are never reused.
    assert!(Database::create_directory(&dir, 2, CreateOptions::default()).is_err());
    let report = maintenance::verify(&dir).unwrap();
    assert!(format!("{report:?}").contains('4'));
}

#[test]
fn rejects_timestamps_and_old_formats() {
    let root = tempdir().unwrap();
    let options = CreateOptions {
        commit_timestamps: true,
        ..CreateOptions::default()
    };
    assert!(matches!(
        Database::create_directory(root.path().join("a"), 2, options),
        Err(Error::UnsupportedFeature(2))
    ));
    let options = CreateOptions {
        format: Some(FormatVersion::V4),
        ..CreateOptions::default()
    };
    assert!(matches!(
        Database::create_directory(root.path().join("b"), 2, options),
        Err(Error::UnsupportedVersion(4))
    ));
    assert!(!root.path().join("a").exists());
    assert!(!root.path().join("b").exists());
}

#[test]
fn checkpoint_preserves_revisions_lineage_and_feed_floor() {
    let root = tempdir().unwrap();
    let dir = root.path().join("store");
    let mut db = Database::create_directory(&dir, 2, CreateOptions::default()).unwrap();
    db.set_checkpoint_threshold(None);
    fill(&mut db, 1, 4);
    db.put(1, &[9.0, 9.0], b"again").unwrap();
    db.delete(3).unwrap();
    let lineage = db.lineage().unwrap();
    assert_eq!(db.checkpoint().unwrap(), Some(5));
    // Nothing new to fold.
    assert_eq!(db.checkpoint().unwrap(), None);
    assert_eq!(
        files(&dir),
        ["LOCK", "MANIFEST", "image-3.vlite", "wal-2.log"]
    );

    assert_eq!(db.revision(1).unwrap(), Some(4));
    assert!(matches!(
        db.put_with_metadata_if(
            2,
            &[0.0, 1.0],
            b"x",
            &Metadata::new(),
            Expected::Revision(1)
        ),
        Err(Error::RevisionMismatch { .. })
    ));
    assert_eq!(
        db.put_with_metadata_if(
            2,
            &[0.0, 1.0],
            b"x",
            &Metadata::new(),
            Expected::Revision(2)
        )
        .unwrap(),
        6
    );
    let before = FeedCursor {
        lineage,
        sequence: 4,
    };
    assert!(matches!(
        db.changes_after(&before, 10),
        Err(Error::CursorBeforeHistory {
            cursor: 4,
            floor: 5
        })
    ));
    let at_floor = FeedCursor {
        lineage,
        sequence: 5,
    };
    assert_eq!(db.changes_after(&at_floor, 10).unwrap().changes.len(), 1);
    db.close().unwrap();

    let mut db = Database::open(&dir).unwrap();
    assert_eq!(db.lineage(), Some(lineage));
    assert_eq!(db.len(), 2);
    assert_eq!(db.get(1).unwrap().unwrap().payload, b"again");
    assert_eq!(db.revision(2).unwrap(), Some(6));
    assert!(db.get(3).unwrap().is_none());
    db.put(7, &[7.0, 7.0], b"seven").unwrap();
    // A second checkpoint folds the image and the log into a new image.
    assert_eq!(db.checkpoint().unwrap(), Some(7));
    db.close().unwrap();
    assert_eq!(
        files(&dir),
        ["LOCK", "MANIFEST", "image-5.vlite", "wal-4.log"]
    );
    let db = Database::open(&dir).unwrap();
    assert_eq!(db.len(), 3);
    assert_eq!(db.revision(7).unwrap(), Some(7));
    db.close().unwrap();
    maintenance::verify(&dir).unwrap();
}

#[test]
fn automatic_checkpoint_runs_in_background() {
    let root = tempdir().unwrap();
    let dir = root.path().join("store");
    let mut db = Database::create_directory(&dir, 2, CreateOptions::default()).unwrap();
    db.set_checkpoint_threshold(Some(256));
    fill(&mut db, 1, 200);
    db.close().unwrap();
    let names = files(&dir);
    assert!(
        names.iter().any(|name| name.starts_with("image-")),
        "{names:?}"
    );
    let db = Database::open(&dir).unwrap();
    assert_eq!(db.len(), 199);
    for id in [1, 100, 199] {
        assert_eq!(db.revision(id).unwrap(), Some(id));
    }
    db.close().unwrap();
    maintenance::verify(&dir).unwrap();
}

#[test]
fn leftovers_are_removed_and_damage_is_refused() {
    let root = tempdir().unwrap();
    let dir = root.path().join("store");
    let mut db = Database::create_directory(&dir, 2, CreateOptions::default()).unwrap();
    fill(&mut db, 1, 3);
    db.close().unwrap();
    // An interrupted rotation, build and manifest publication.
    fs::write(dir.join("wal-7.log"), b"partial").unwrap();
    fs::write(dir.join("image-6.vlite"), b"partial").unwrap();
    fs::write(dir.join(".tmpABC"), b"partial").unwrap();
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(files(&dir), ["LOCK", "MANIFEST", "wal-1.log"]);
    assert_eq!(db.len(), 2);
    // New files never reuse a number seen on disk.
    db.set_checkpoint_threshold(None);
    assert_eq!(db.checkpoint().unwrap(), Some(2));
    db.close().unwrap();
    assert_eq!(
        files(&dir),
        ["LOCK", "MANIFEST", "image-9.vlite", "wal-8.log"]
    );

    let mut text = manifest(&dir);
    text = text.replace("wal-8.log", "wal-9.log");
    fs::write(dir.join("MANIFEST"), text).unwrap();
    assert!(matches!(Database::open(&dir), Err(Error::Corrupt { .. })));
}

#[test]
fn encrypted_directory_round_trip() {
    let root = tempdir().unwrap();
    let dir = root.path().join("store");
    let key = EncryptionKey::from_bytes([4; 32]);
    let options = CreateOptions {
        encryption: Some(key.clone()),
        ..CreateOptions::default()
    };
    let mut db = Database::create_directory(&dir, 2, options).unwrap();
    db.set_checkpoint_threshold(None);
    fill(&mut db, 1, 6);
    assert_eq!(db.checkpoint().unwrap(), Some(5));
    db.put(9, &[1.0, 2.0], b"secret-after").unwrap();
    db.close().unwrap();
    for name in files(&dir) {
        let bytes = fs::read(dir.join(&name)).unwrap();
        assert!(!bytes.windows(4).any(|window| window == b"p3p3"));
        assert!(!bytes.windows(12).any(|window| window == b"secret-after"));
    }
    assert!(Database::open(&dir).is_err());
    let db = Database::open_encrypted(&dir, &key).unwrap();
    assert_eq!(db.len(), 6);
    assert_eq!(db.get(9).unwrap().unwrap().payload, b"secret-after");
    db.close().unwrap();
    maintenance::verify_with_key(&dir, &key).unwrap();
}

#[test]
fn graphs_survive_checkpoints() {
    let root = tempdir().unwrap();
    let dir = root.path().join("store");
    let mut db = Database::create_directory(&dir, 2, CreateOptions::default()).unwrap();
    db.set_checkpoint_threshold(None);
    db.create_graph("g").unwrap();
    db.put(1, &[1.0, 0.0], b"one").unwrap();
    assert!(db.checkpoint().unwrap().is_some());
    db.create_graph("h").unwrap();
    db.close().unwrap();
    let db = Database::open(&dir).unwrap();
    assert!(db.graph("g").is_ok());
    assert!(db.graph("h").is_ok());
    assert_eq!(db.len(), 1);
    db.close().unwrap();
}

#[test]
fn read_only_and_shared_readers() {
    let root = tempdir().unwrap();
    let dir = root.path().join("store");
    let mut db = Database::create_directory(&dir, 2, CreateOptions::default()).unwrap();
    db.set_checkpoint_threshold(None);
    fill(&mut db, 1, 4);
    db.checkpoint().unwrap();
    db.put(8, &[8.0, 8.0], b"eight").unwrap();
    db.close().unwrap();

    let mut first = Database::open_read_only(&dir).unwrap();
    let second = Database::open_read_only(&dir).unwrap();
    assert_eq!(first.len(), 4);
    assert_eq!(second.get(8).unwrap().unwrap().payload, b"eight");
    assert!(matches!(first.checkpoint(), Err(Error::ReadOnly)));
    assert!(matches!(Database::open(&dir), Err(Error::Locked)));
    drop(first);
    drop(second);
    Database::open(&dir).unwrap().close().unwrap();
}

#[test]
fn torn_tail_of_active_log_is_repaired() {
    let root = tempdir().unwrap();
    let dir = root.path().join("store");
    let mut db = Database::create_directory(&dir, 2, CreateOptions::default()).unwrap();
    db.set_checkpoint_threshold(None);
    fill(&mut db, 1, 3);
    db.checkpoint().unwrap();
    fill(&mut db, 3, 5);
    db.close().unwrap();
    let wal = dir.join("wal-2.log");
    let length = fs::metadata(&wal).unwrap().len();
    let file = fs::OpenOptions::new().write(true).open(&wal).unwrap();
    file.set_len(length - 5).unwrap();
    drop(file);
    assert!(Database::open(&dir).is_err());
    let report = Database::discard_incomplete_tail(&dir).unwrap();
    assert!(format!("{report:?}").contains('3'));
    let db = Database::open(&dir).unwrap();
    assert_eq!(db.len(), 3);
    assert!(db.get(4).unwrap().is_none());
    db.close().unwrap();
}

#[test]
fn converts_between_file_and_directory() {
    let root = tempdir().unwrap();
    let file = root.path().join("source.vlite");
    let mut db = Database::create_with_options(&file, 2, CreateOptions::default()).unwrap();
    fill(&mut db, 1, 4);
    db.put(2, &[2.0, 2.0], b"two-again").unwrap();
    let lineage = db.lineage().unwrap();
    db.close().unwrap();

    let dir = root.path().join("store");
    maintenance::convert_to_directory(&file, &dir).unwrap();
    assert_eq!(
        files(&dir),
        ["LOCK", "MANIFEST", "image-1.vlite", "wal-2.log"]
    );
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(db.lineage(), Some(lineage));
    assert_eq!(db.revision(2).unwrap(), Some(4));
    db.put(5, &[5.0, 5.0], b"five").unwrap();
    db.close().unwrap();
    assert!(maintenance::convert_to_directory(&file, &dir).is_err());

    // `backup` packs the directory back into one file.
    let packed = root.path().join("packed.vlite");
    maintenance::backup(&dir, &packed).unwrap();
    let db = Database::open(&packed).unwrap();
    assert!(!db.is_directory());
    assert_eq!(db.lineage(), Some(lineage));
    assert_eq!(db.len(), 4);
    assert_eq!(db.revision(5).unwrap(), Some(5));
    db.close().unwrap();

    // Empty sources convert to a log-only store.
    let empty = root.path().join("empty.vlite");
    Database::create_with_options(&empty, 3, CreateOptions::default())
        .unwrap()
        .close()
        .unwrap();
    let empty_dir = root.path().join("empty");
    maintenance::convert_to_directory(&empty, &empty_dir).unwrap();
    assert_eq!(files(&empty_dir), ["LOCK", "MANIFEST", "wal-1.log"]);
    let mut db = Database::open(&empty_dir).unwrap();
    db.put(1, &[1.0, 2.0, 3.0], b"x").unwrap();
    db.close().unwrap();

    // Commit timestamps are not supported in directory stores.
    let stamped = root.path().join("stamped.vlite");
    let options = CreateOptions {
        commit_timestamps: true,
        ..CreateOptions::default()
    };
    Database::create_with_options(&stamped, 2, options)
        .unwrap()
        .close()
        .unwrap();
    assert!(matches!(
        maintenance::convert_to_directory(&stamped, root.path().join("stamped")),
        Err(Error::UnsupportedFeature(2))
    ));
}
