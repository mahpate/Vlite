use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;
use vlite::{Database, Error, MetadataValue, Mutation, Record, maintenance};

fn next(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state
}

fn verify(db: &Database, model: &BTreeMap<u64, Record>) {
    assert_eq!(db.len(), model.len());
    for id in (0..24).chain([u64::MAX]) {
        assert_eq!(db.get(id).unwrap(), model.get(&id), "record {id}");
    }
    for query in [[0.0, 0.0, 0.0], [3.0, -2.0, 1.0]] {
        let mut expected: Vec<_> = model
            .iter()
            .map(|(&id, record)| {
                let distance: f64 = record
                    .vector
                    .iter()
                    .zip(query)
                    .map(|(&left, right)| (f64::from(left) - f64::from(right)).powi(2))
                    .sum();
                (id, distance)
            })
            .collect();
        expected.sort_by(|left, right| {
            left.1
                .total_cmp(&right.1)
                .then_with(|| left.0.cmp(&right.0))
        });
        for k in [0, 1, 7, 30] {
            let actual: Vec<_> = db
                .search(&query, k)
                .unwrap()
                .iter()
                .map(|hit| (hit.id, hit.distance))
                .collect();
            assert_eq!(actual, expected[..k.min(expected.len())]);
        }
    }
}

#[test]
fn deterministic_mixed_history_matches_model_across_reopen_and_maintenance() {
    for seed in [1, 42, 0xdead_beef, u64::MAX] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.vlite");
        let mut db = Database::create(&path, 3).unwrap();
        let mut model = BTreeMap::new();
        let mut state = seed;
        for step in 0..160 {
            let mut operations = Vec::new();
            let mut candidate = model.clone();
            for _ in 0..1 + next(&mut state) % 5 {
                let value = next(&mut state);
                let id = if value.is_multiple_of(13) {
                    u64::MAX
                } else {
                    value % 24
                };
                if next(&mut state).is_multiple_of(4) {
                    operations.push(Mutation::Delete { id });
                    candidate.remove(&id);
                } else {
                    let record = Record {
                        vector: (0..3)
                            .map(|_| (next(&mut state) % 17) as f32 - 8.0)
                            .collect(),
                        payload: vec![(value >> 32) as u8, 0, 255, step as u8],
                        metadata: if value.is_multiple_of(3) {
                            BTreeMap::new()
                        } else {
                            BTreeMap::from([("step".into(), MetadataValue::Integer(step))])
                        },
                    };
                    operations.push(if record.metadata.is_empty() {
                        Mutation::Put {
                            id,
                            vector: record.vector.clone(),
                            payload: record.payload.clone(),
                        }
                    } else {
                        Mutation::PutWithMetadata {
                            id,
                            vector: record.vector.clone(),
                            payload: record.payload.clone(),
                            metadata: record.metadata.clone(),
                        }
                    });
                    candidate.insert(id, record);
                }
            }
            if step % 11 == 0 {
                db.close().unwrap();
                let before = fs::read(&path).unwrap();
                db = Database::open(&path).unwrap();
                operations.push(Mutation::Put {
                    id: 0,
                    vector: vec![f32::NAN; 3],
                    payload: vec![],
                });
                assert!(db.write_batch(&operations).is_err());
                db.close().unwrap();
                assert_eq!(fs::read(&path).unwrap(), before);
                db = Database::open(&path).unwrap();
            } else {
                match operations.as_slice() {
                    [
                        Mutation::Put {
                            id,
                            vector,
                            payload,
                        },
                    ] => db.put(*id, vector, payload).unwrap(),
                    [
                        Mutation::PutWithMetadata {
                            id,
                            vector,
                            payload,
                            metadata,
                        },
                    ] => {
                        db.put_with_metadata(*id, vector, payload, metadata)
                            .unwrap();
                    }
                    [Mutation::Delete { id }] => {
                        assert_eq!(db.delete(*id).unwrap(), model.contains_key(id));
                    }
                    _ => db.write_batch(&operations).unwrap(),
                }
                model = candidate;
            }
            verify(&db, &model);
            if step % 17 == 0 {
                db.close().unwrap();
                db = Database::open(&path).unwrap();
                verify(&db, &model);
            }
        }
        db.close().unwrap();
        let backup = directory.path().join("backup.vlite");
        maintenance::backup(&path, &backup).unwrap();
        assert_eq!(fs::read(&path).unwrap(), fs::read(&backup).unwrap());
        let compact = directory.path().join("compact.vlite");
        maintenance::compact(&backup, &compact).unwrap();
        for output in [&path, &backup, &compact] {
            let db = Database::open(output).unwrap();
            verify(&db, &model);
            db.close().unwrap();
        }
    }
}

struct RunningChild(Child);

impl Drop for RunningChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "subprocess fixture, invoked explicitly by forced_termination_preserves_acknowledged_commits"]
fn crash_writer_child() {
    let path = std::env::var_os("VLITE_CRASH_TEST_PATH").expect("parent supplies a private path");
    let mut db = Database::create(path, 3).unwrap();
    for id in 0..8 {
        db.put(id, &[id as f32, 0.0, 0.0], &id.to_le_bytes())
            .unwrap();
    }
    println!("ACKNOWLEDGED");
    std::io::stdout().flush().unwrap();
    let mut command = String::new();
    std::io::stdin().read_line(&mut command).unwrap();
    assert_eq!(command.trim(), "GO");
    let batch: Vec<_> = (100..132)
        .map(|id| Mutation::Put {
            id,
            vector: vec![1.0, 2.0, 3.0],
            payload: vec![0x5a; 128 * 1024],
        })
        .collect();
    println!("INFLIGHT");
    std::io::stdout().flush().unwrap();
    db.write_batch(&batch).unwrap();
    println!("BATCH_COMMITTED");
    std::io::stdout().flush().unwrap();
    command.clear();
    std::io::stdin().read_line(&mut command).unwrap();
    panic!("parent must terminate this process without graceful close");
}

#[test]
fn forced_termination_preserves_acknowledged_commits() {
    for delay_ms in [None, Some(0), Some(2), Some(10)] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("crash.vlite");
        let mut child = RunningChild(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "crash_writer_child", "--ignored", "--nocapture"])
                .env("VLITE_CRASH_TEST_PATH", &path)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        let stdout = child.0.stdout.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if sender.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        let wait_for = |expected| loop {
            let line = receiver
                .recv_timeout(Duration::from_secs(120))
                .expect("child did not reach durability checkpoint");
            if line == expected {
                break;
            }
        };
        wait_for("ACKNOWLEDGED");
        assert!(matches!(Database::open(&path), Err(Error::Locked)));
        if let Some(delay) = delay_ms {
            child.0.stdin.as_mut().unwrap().write_all(b"GO\n").unwrap();
            wait_for("INFLIGHT");
            std::thread::sleep(Duration::from_millis(delay));
        }
        child.0.kill().unwrap();
        assert!(!child.0.wait().unwrap().success());
        reader.join().unwrap();
        let before_open = fs::read(&path).unwrap();
        let db = match Database::open(&path) {
            Ok(db) => db,
            Err(Error::IncompleteTail { .. }) => {
                assert_eq!(fs::read(&path).unwrap(), before_open);
                fs::write(directory.path().join("before-repair.bin"), &before_open).unwrap();
                Database::discard_incomplete_tail(&path).unwrap();
                Database::open(&path).unwrap()
            }
            Err(error) => panic!("unexpected recovery result: {error}"),
        };
        for id in 0u64..8 {
            let record = db.get(id).unwrap().expect("acknowledged data lost");
            assert_eq!(record.vector, [id as f32, 0.0, 0.0]);
            assert_eq!(record.payload, id.to_le_bytes());
        }
        let batch_present = db.get(100).unwrap().is_some();
        for id in 100..132 {
            let record = db.get(id).unwrap();
            assert_eq!(record.is_some(), batch_present, "partial atomic batch");
            if let Some(record) = record {
                assert_eq!(record.vector, [1.0, 2.0, 3.0]);
                assert_eq!(record.payload, vec![0x5a; 128 * 1024]);
            }
        }
        assert_eq!(db.len(), if batch_present { 40 } else { 8 });
        if delay_ms.is_none() {
            assert!(!batch_present);
        }
        db.close().unwrap();
        let mut reopened = Database::open(&path).unwrap();
        reopened.put(u64::MAX, &[0.0; 3], b"after crash").unwrap();
        reopened.close().unwrap();
        assert_eq!(
            Database::open(&path)
                .unwrap()
                .get(u64::MAX)
                .unwrap()
                .unwrap()
                .payload,
            b"after crash"
        );
    }
}

/// Builds a source large enough that copy-out operations are observable in
/// flight, with overwritten values so compaction actually rewrites.
fn maintenance_source(path: &std::path::Path) -> BTreeMap<u64, Record> {
    let mut db = Database::create(path, 3).unwrap();
    let mut model = BTreeMap::new();
    for round in 0..2u8 {
        let batch: Vec<_> = (0..48u64)
            .map(|id| Mutation::Put {
                id,
                vector: vec![id as f32, f32::from(round), 1.0],
                payload: vec![round ^ id as u8; 96 * 1024],
            })
            .collect();
        db.write_batch(&batch).unwrap();
    }
    for id in 0..48u64 {
        model.insert(id, db.get(id).unwrap().unwrap().clone());
    }
    db.close().unwrap();
    model
}

/// Killing `vlite-maintain` at any point never modifies the source and never
/// publishes a partial destination: it is either absent or complete and equal
/// to the source's live records. A retry after the kill succeeds.
#[test]
fn forced_termination_of_copy_out_maintenance_never_publishes_partial_output() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.vlite");
    let model = maintenance_source(&source);
    let backup = directory.path().join("seed-backup.vlite");
    maintenance::backup(&source, &backup).unwrap();
    let source_bytes = fs::read(&source).unwrap();
    let backup_bytes = fs::read(&backup).unwrap();
    let check = |path: &std::path::Path| {
        let db = Database::open(path).unwrap();
        assert_eq!(db.len(), model.len());
        for (id, record) in &model {
            assert_eq!(db.get(*id).unwrap(), Some(record), "record {id}");
        }
        db.close().unwrap();
    };
    let mut published = 0;
    let mut absent = 0;
    for operation in ["backup", "restore", "compact", "upgrade"] {
        let input = if operation == "restore" {
            &backup
        } else {
            &source
        };
        for delay_ms in [0u64, 10, 40, 5000] {
            let output = directory
                .path()
                .join(format!("{operation}-{delay_ms}.vlite"));
            let mut child = RunningChild(
                Command::new(env!("CARGO_BIN_EXE_vlite-maintain"))
                    .arg(operation)
                    .arg(input)
                    .arg(&output)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap(),
            );
            std::thread::sleep(Duration::from_millis(delay_ms));
            let _ = child.0.kill();
            let _ = child.0.wait();
            assert_eq!(fs::read(&source).unwrap(), source_bytes, "{operation}");
            assert_eq!(fs::read(&backup).unwrap(), backup_bytes, "{operation}");
            if output.exists() {
                published += 1;
                check(&output);
            } else {
                absent += 1;
                // The interrupted run left no lock or partial file behind.
                let status = Command::new(env!("CARGO_BIN_EXE_vlite-maintain"))
                    .arg(operation)
                    .arg(input)
                    .arg(&output)
                    .stdout(Stdio::null())
                    .status()
                    .unwrap();
                assert!(status.success(), "{operation} retry after kill");
                check(&output);
            }
            // Never overwrites a published destination.
            let again = Command::new(env!("CARGO_BIN_EXE_vlite-maintain"))
                .arg(operation)
                .arg(input)
                .arg(&output)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(!again.success(), "{operation} overwrote its destination");
        }
    }
    // Staging files may be abandoned, but none ever carries a final name.
    for entry in fs::read_dir(directory.path()).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        assert!(
            name.ends_with(".vlite") || name.starts_with(".tmp"),
            "unexpected file {name}"
        );
    }
    assert_eq!(published + absent, 16);
    eprintln!("maintenance kill outcomes: {published} published, {absent} absent");
    check(&source);
}
