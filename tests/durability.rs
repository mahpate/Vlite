//! `Durability::Periodic` (the default since v0.5.1): acknowledged commits are
//! written before the call returns, so a process crash (here: `process::exit`
//! in a child process, which runs no destructors and therefore no final sync)
//! loses nothing; graph frames, encryption and commit timestamps use the same
//! path. `Durability::None` is the in-memory-only mode.

mod graph_support;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use vlite::{CreateOptions, Database, Durability, EncryptionKey, FormatVersion};

const CHILD: &str = "VLITE_DURABILITY_CHILD";

fn lazy() -> Durability {
    Durability::periodic(Duration::from_secs(60)).unwrap()
}

fn key() -> EncryptionKey {
    EncryptionKey::from_bytes([3; 32])
}

fn open(path: &Path, encrypted: bool) -> Database {
    if encrypted {
        Database::open_encrypted(path, &key()).unwrap()
    } else {
        Database::open(path).unwrap()
    }
}

/// Runs `test` in a fresh copy of this test binary with `CHILD=path` and
/// requires it to exit without panicking.
fn run_child(test: &str, path: &Path) {
    let status = Command::new(std::env::current_exe().unwrap())
        .args([test, "--exact", "--test-threads=1", "--nocapture"])
        .env(CHILD, path)
        .status()
        .unwrap();
    assert!(status.success(), "child {test} failed: {status}");
}

/// Child half: writes under a 60 s flush interval, then exits the process
/// abruptly so no flusher sync, `close` or destructor runs.
#[test]
fn child_writes_then_crashes() {
    let Some(path) = std::env::var_os(CHILD) else {
        return;
    };
    let path = Path::new(&path);
    let encrypted = path.to_string_lossy().contains("encrypted");
    let mut db = open(path, encrypted).with_durability(lazy()).unwrap();
    for id in 0..100u64 {
        db.put(id, &[id as f32, 1.0], format!("p{id}").as_bytes())
            .unwrap();
    }
    db.delete(7).unwrap();
    assert!(db.unsynced_commits() > 0);
    std::process::exit(0);
}

#[test]
fn periodic_commits_survive_a_process_crash() {
    let directory = tempfile::tempdir().unwrap();
    for (name, options) in [
        ("plain.vlite", CreateOptions::default()),
        (
            "encrypted-timestamped.vlite",
            CreateOptions {
                format: Some(FormatVersion::V5),
                encryption: Some(key()),
                commit_timestamps: true,
            },
        ),
    ] {
        let path = directory.path().join(name);
        Database::create_with_options(&path, 2, options)
            .unwrap()
            .close()
            .unwrap();
        run_child("child_writes_then_crashes", &path);
        let db = open(&path, name.contains("encrypted"));
        assert_eq!(db.len(), 99, "{name}");
        assert!(db.get(7).unwrap().is_none());
        assert_eq!(db.get(99).unwrap().unwrap().payload, b"p99");
        assert_eq!(db.durability(), Durability::default());
    }
}

#[test]
fn file_handles_default_to_periodic_and_memory_handles_are_none_only() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("modes.vlite");
    let mut db = Database::create(&path, 1).unwrap();
    assert_eq!(db.durability(), lazy());
    assert_eq!(Durability::default(), lazy());
    assert_eq!(vlite::DEFAULT_FLUSH_INTERVAL, Duration::from_secs(60));
    assert!(matches!(
        db.set_durability(Durability::None),
        Err(vlite::Error::InvalidDurability(_))
    ));
    db.set_durability(Durability::Full).unwrap();
    db.put(1, &[1.0], b"").unwrap();
    db.close().unwrap();

    let mut memory = Database::create_in_memory(1, CreateOptions::default()).unwrap();
    assert_eq!(memory.durability(), Durability::None);
    assert!(matches!(
        memory.set_durability(Durability::Full),
        Err(vlite::Error::InvalidDurability(_))
    ));
    memory.put(1, &[1.0], b"memory").unwrap();
    assert_eq!(memory.unsynced_commits(), 0);
    memory.sync().unwrap();
    // Nothing is written anywhere: dropping the handle loses the data unless
    // the caller kept `to_bytes()`.
    let image = memory.to_bytes().unwrap();
    drop(memory);
    let reopened = Database::open_from_bytes(image, None).unwrap();
    assert_eq!(reopened.durability(), Durability::None);
    assert_eq!(reopened.get(1).unwrap().unwrap().payload, b"memory");
    assert_eq!(
        std::fs::read_dir(directory.path()).unwrap().count(),
        1,
        "in-memory handles never create files"
    );
}

/// What an OS crash can leave of the unsynced suffix and what reopen does:
/// a cleanly missing suffix opens as the synced prefix; a partial last frame
/// is an `IncompleteTail` that needs explicit repair; a zero-filled suffix
/// (file size updated but data blocks never written) fails closed as
/// `Corrupt` at the synced end, so an operator truncates there or restores.
#[test]
fn unsynced_suffix_loss_shapes_fail_closed_or_need_explicit_repair() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("torn.vlite");
    let mut db = Database::create(&path, 2).unwrap();
    db.put(1, &[1.0, 1.0], b"synced").unwrap();
    db.close().unwrap();
    let synced = std::fs::read(&path).unwrap();
    let mut db = Database::open(&path)
        .unwrap()
        .with_durability(lazy())
        .unwrap();
    db.put(2, &[2.0, 2.0], b"lazy").unwrap();
    db.put(3, &[3.0, 3.0], b"lazy").unwrap();
    db.close().unwrap();
    let full = std::fs::read(&path).unwrap();
    assert!(full.starts_with(&synced));

    std::fs::write(&path, &synced).unwrap();
    assert_eq!(Database::open(&path).unwrap().len(), 1);

    std::fs::write(&path, &full[..full.len() - 3]).unwrap();
    assert!(matches!(
        Database::open(&path),
        Err(vlite::Error::IncompleteTail { .. })
    ));
    Database::discard_incomplete_tail(&path).unwrap();
    assert_eq!(Database::open(&path).unwrap().len(), 2);

    let mut zeroed = synced.clone();
    zeroed.resize(full.len(), 0);
    std::fs::write(&path, &zeroed).unwrap();
    let error = Database::open(&path)
        .err()
        .expect("zero-filled suffix must not open");
    assert!(
        matches!(error, vlite::Error::Corrupt { offset, .. } if offset == synced.len() as u64),
        "{error:?}"
    );
    // The reported offset is the synced end: truncating there recovers it.
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(synced.len() as u64)
        .unwrap();
    assert_eq!(Database::open(&path).unwrap().len(), 1);
}

#[test]
fn periodic_handles_reject_invalid_intervals_and_expose_sync() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("api.vlite");
    let mut db = Database::create(&path, 1).unwrap();
    assert!(Durability::periodic(Duration::ZERO).is_err());
    assert!(Durability::periodic_ms(60_001).is_err());
    assert!(
        db.set_durability(Durability::Periodic {
            interval: Duration::from_secs(61)
        })
        .is_err()
    );
    db.set_durability(Durability::periodic_ms(10).unwrap())
        .unwrap();
    db.put(1, &[1.0], b"").unwrap();
    db.sync().unwrap();
    assert_eq!(db.unsynced_commits(), 0);
    db.close().unwrap();
}

mod graph {
    use super::*;
    use graph_support::*;

    /// Child half: the full graph workload under a 60 s flush interval,
    /// then an abrupt exit.
    #[test]
    fn child_graph_writes_then_crashes() {
        let Some(path) = std::env::var_os(CHILD) else {
            return;
        };
        let path = Path::new(&path);
        let kind = KINDS
            .into_iter()
            .find(|kind| path.to_string_lossy().contains(&format!("{kind:?}")))
            .unwrap();
        let mut db = graph_support::open(path, kind.key().as_ref())
            .with_durability(lazy())
            .unwrap();
        clock(&mut db, 0);
        populate(&mut db);
        extend(&mut db);
        assert!(db.unsynced_commits() > 0);
        std::process::exit(0);
    }

    #[test]
    fn periodic_graph_commits_match_full_durability_after_a_process_crash() {
        let directory = tempfile::tempdir().unwrap();
        for kind in KINDS {
            let reference = directory.path().join(format!("full-{kind:?}.vlite"));
            let mut db = Database::create_with_options(&reference, 2, kind.options()).unwrap();
            clock(&mut db, 0);
            populate(&mut db);
            extend(&mut db);
            db.close().unwrap();

            let path = directory.path().join(format!("lazy-{kind:?}.vlite"));
            Database::create_with_options(&path, 2, kind.options())
                .unwrap()
                .close()
                .unwrap();
            run_child("graph::child_graph_writes_then_crashes", &path);
            let secret = kind.key();
            assert_eq!(
                snapshot(&path, secret.as_ref()),
                snapshot(&reference, secret.as_ref()),
                "{kind:?}"
            );
        }
    }
}
