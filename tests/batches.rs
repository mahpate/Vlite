use std::fs;
use std::process::Command;
use vlite::{Database, Error, MAX_BATCH_BYTES, MAX_BATCH_OPERATIONS, Mutation, maintenance};

fn put(id: u64, vector: &[f32], payload: &[u8]) -> Mutation {
    Mutation::Put {
        id,
        vector: vector.to_vec(),
        payload: payload.to_vec(),
    }
}

fn operations() -> Vec<Mutation> {
    vec![
        put(1, &[1.0, 2.0], b"temporary"),
        put(2, &[3.0, -0.0], b"new"),
        Mutation::Delete { id: 1 },
        Mutation::Delete { id: 99 },
        put(1, &[5.0, 6.0], b"last wins"),
        Mutation::Delete { id: 3 },
    ]
}

#[test]
fn batch_is_one_commit_applied_in_order_and_survives_maintenance() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("batch.vlite");
    let mut db = Database::create(&path, 2).unwrap();
    assert_eq!(db.format_version(), 3);
    db.put(3, &[9.0, 9.0], b"old").unwrap();
    db.write_batch(&operations()).unwrap();
    assert_eq!(db.len(), 2);
    assert_eq!(db.get(1).unwrap().unwrap().payload, b"last wins");
    assert_eq!(
        db.get(2).unwrap().unwrap().vector[1].to_bits(),
        (-0.0f32).to_bits()
    );
    assert!(db.get(3).unwrap().is_none());
    let expected = db.search(&[0.0, 0.0], 10).unwrap();
    db.close().unwrap();
    assert_eq!(maintenance::inspect(&path).unwrap().commits, 2);
    let backup = directory.path().join("backup.vlite");
    maintenance::backup(&path, &backup).unwrap();
    assert_eq!(fs::read(&path).unwrap(), fs::read(&backup).unwrap());
    let restored = directory.path().join("restored.vlite");
    maintenance::restore(&backup, &restored).unwrap();
    let compact = directory.path().join("compact.vlite");
    maintenance::compact(&restored, &compact).unwrap();
    for output in [&path, &backup, &restored, &compact] {
        let mut db = Database::open(output).unwrap();
        assert_eq!(db.get(1).unwrap().unwrap().payload, b"last wins");
        assert!(db.get(3).unwrap().is_none());
        assert_eq!(db.search(&[0.0, 0.0], 10).unwrap(), expected);
        db.write_batch(&[Mutation::Delete { id: 1 }]).unwrap();
        db.close().unwrap();
        assert_eq!(Database::open(output).unwrap().len(), 1);
    }
}

#[test]
fn invalid_batches_never_write_a_prefix_and_empty_batches_are_noops() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("batch.vlite");
    let mut db = Database::create(&path, 2).unwrap();
    db.put(1, &[9.0, 9.0], b"old").unwrap();
    db.close().unwrap();
    let before = fs::read(&path).unwrap();
    let mut db = Database::open(&path).unwrap();
    db.write_batch(&[]).unwrap();
    for invalid in [
        put(3, &[1.0], b""),
        put(3, &[f32::NAN, 0.0], b""),
        put(3, &[0.0, f32::INFINITY], b""),
        put(3, &[0.0, 0.0], &vec![0; vlite::MAX_PAYLOAD_BYTES + 1]),
    ] {
        assert!(
            db.write_batch(&[Mutation::Delete { id: 1 }, invalid])
                .is_err()
        );
        assert_eq!(db.get(1).unwrap().unwrap().payload, b"old");
        assert_eq!(db.len(), 1);
    }
    let excessive = vec![Mutation::Delete { id: 1 }; MAX_BATCH_OPERATIONS + 1];
    assert!(matches!(
        db.write_batch(&excessive),
        Err(Error::BatchTooLarge)
    ));
    db.close().unwrap();
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn exact_operation_and_encoded_byte_limits_are_enforced() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("limits.vlite");
    let mut db = Database::create(&path, 1).unwrap();
    db.write_batch(&vec![Mutation::Delete { id: 1 }; MAX_BATCH_OPERATIONS])
        .unwrap();
    // 16 put entries: 16-byte metadata + 4-byte vector + payload.
    let payload = vec![0; vlite::MAX_PAYLOAD_BYTES];
    let mut batch: Vec<_> = (0..16).map(|id| put(id, &[1.0], &payload)).collect();
    if let Mutation::Put { payload, .. } = &mut batch[15] {
        payload.truncate(payload.len() - 16 * 20);
    }
    let encoded: usize = batch
        .iter()
        .map(|op| match op {
            Mutation::Put {
                vector, payload, ..
            } => 16 + vector.len() * 4 + payload.len(),
            Mutation::Delete { .. } => 16,
            Mutation::PutWithMetadata { .. } => panic!("this fixture has no metadata"),
        })
        .sum();
    assert_eq!(encoded, MAX_BATCH_BYTES);
    db.write_batch(&batch).unwrap();
    if let Mutation::Put { payload, .. } = &mut batch[15] {
        payload.push(0);
    }
    assert!(matches!(db.write_batch(&batch), Err(Error::BatchTooLarge)));
    db.close().unwrap();
    assert_eq!(Database::open(path).unwrap().len(), 16);
}

#[test]
fn every_interrupted_batch_byte_preserves_the_entire_previous_state() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("interrupt.vlite");
    let mut db = Database::create(&path, 2).unwrap();
    db.put(3, &[9.0, 9.0], b"old").unwrap();
    db.close().unwrap();
    let before = fs::read(&path).unwrap();
    let mut db = Database::open(&path).unwrap();
    db.write_batch(&operations()).unwrap();
    db.close().unwrap();
    let complete = fs::read(&path).unwrap();
    for cut in before.len() + 1..complete.len() {
        fs::write(&path, &complete[..cut]).unwrap();
        assert!(
            matches!(Database::open(&path), Err(Error::IncompleteTail { .. })),
            "cut {cut}"
        );
        assert_eq!(maintenance::inspect(&path).unwrap().commits, 1);
        let report = Database::discard_incomplete_tail(&path).unwrap();
        assert_eq!(report.retained_commits, 1);
        assert_eq!(fs::read(&path).unwrap(), before);
        let db = Database::open(&path).unwrap();
        assert_eq!(db.len(), 1);
        assert_eq!(db.get(3).unwrap().unwrap().payload, b"old");
        assert!(db.get(1).unwrap().is_none());
        assert!(db.get(2).unwrap().is_none());
    }
    fs::write(&path, &complete).unwrap();
    assert_eq!(Database::open(&path).unwrap().len(), 2);
}

#[test]
fn every_corrupt_batch_byte_fails_closed_and_never_repairs() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("corrupt.vlite");
    let mut db = Database::create(&path, 2).unwrap();
    db.write_batch(&operations()).unwrap();
    db.close().unwrap();
    let complete = fs::read(&path).unwrap();
    for offset in 32..complete.len() {
        let mut damaged = complete.clone();
        damaged[offset] ^= 1;
        fs::write(&path, &damaged).unwrap();
        assert!(
            matches!(Database::open(&path), Err(Error::Corrupt { .. })),
            "byte {offset}"
        );
        assert!(matches!(
            Database::discard_incomplete_tail(&path),
            Err(Error::Corrupt { .. })
        ));
        assert_eq!(fs::read(&path).unwrap(), damaged);
    }
}

#[test]
fn batch_process_exit_child() {
    let Some(path) = std::env::var_os("VLITE_BATCH_EXIT_PATH") else {
        return;
    };
    let mut db = Database::create(path, 2).unwrap();
    db.write_batch(&operations()).unwrap();
    std::process::exit(0);
}

#[test]
fn acknowledged_batch_survives_exit_without_destructors() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("exit.vlite");
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "batch_process_exit_child"])
        .env("VLITE_BATCH_EXIT_PATH", &path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let db = Database::open(&path).unwrap();
    assert_eq!(db.len(), 2);
    assert_eq!(db.get(1).unwrap().unwrap().payload, b"last wins");
    assert!(db.get(3).unwrap().is_none());
}
