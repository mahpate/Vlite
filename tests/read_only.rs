use std::sync::Arc;
use tempfile::tempdir;
use vlite::{CreateOptions, Database, EncryptionKey, Error, Mutation};

fn seeded(path: &std::path::Path) {
    let mut db = Database::create(path, 2).unwrap();
    db.put(1, &[0.0, 0.0], b"a").unwrap();
    db.put(2, &[1.0, 1.0], b"b").unwrap();
    db.close().unwrap();
}

#[test]
fn read_only_handles_share_the_file_and_exclude_writers() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("shared.vlite");
    seeded(&path);

    let first = Database::open_read_only(&path).unwrap();
    let second = Database::open_read_only(&path).unwrap();
    assert!(first.is_read_only() && second.is_read_only());
    assert_eq!(first.len(), 2);
    assert_eq!(second.get(2).unwrap().unwrap().payload, b"b");
    assert!(matches!(Database::open(&path), Err(Error::Locked)));

    drop(first);
    drop(second);
    let writer = Database::open(&path).unwrap();
    assert!(!writer.is_read_only());
    assert!(matches!(
        Database::open_read_only(&path),
        Err(Error::Locked)
    ));
}

#[test]
fn mutations_on_read_only_handle_fail_without_poisoning_or_writing() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("ro.vlite");
    seeded(&path);
    let before = std::fs::read(&path).unwrap();

    let mut db = Database::open_read_only(&path).unwrap();
    assert!(matches!(db.put(3, &[2.0, 2.0], b"c"), Err(Error::ReadOnly)));
    assert!(matches!(db.delete(1), Err(Error::ReadOnly)));
    assert!(matches!(
        db.write_batch(&[Mutation::Delete { id: 2 }]),
        Err(Error::ReadOnly)
    ));
    // The handle stays healthy and unchanged.
    assert_eq!(db.len(), 2);
    let hits = db.search(&[0.9, 0.9], 1).unwrap();
    assert_eq!(hits[0].id, 2);
    drop(db);
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn read_only_handle_is_a_snapshot_until_reopened() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("snapshot.vlite");
    seeded(&path);
    let reader = Database::open_read_only(&path).unwrap();
    assert_eq!(reader.len(), 2);
    drop(reader);
    let mut writer = Database::open(&path).unwrap();
    writer.put(3, &[5.0, 5.0], b"c").unwrap();
    writer.close().unwrap();
    assert_eq!(Database::open_read_only(&path).unwrap().len(), 3);
}

#[test]
fn encrypted_databases_open_read_only_with_the_key() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("enc.vlite");
    let key = EncryptionKey::from_bytes([7; 32]);
    let mut db = Database::create_with_options(
        &path,
        2,
        CreateOptions {
            encryption: Some(EncryptionKey::from_bytes([7; 32])),
            ..CreateOptions::default()
        },
    )
    .unwrap();
    db.put(9, &[3.0, 4.0], b"secret").unwrap();
    db.close().unwrap();

    let reader = Database::open_read_only_encrypted(&path, &key).unwrap();
    assert!(reader.is_read_only());
    assert_eq!(reader.get(9).unwrap().unwrap().payload, b"secret");
    assert!(Database::open_read_only(&path).is_err());
}

#[test]
fn threads_share_one_handle_for_concurrent_search() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("threads.vlite");
    let mut db = Database::create(&path, 4).unwrap();
    for id in 0..500u64 {
        let value = id as f32;
        db.put(id, &[value, 0.0, 0.0, 0.0], b"").unwrap();
    }
    let db = Arc::new(db);
    let workers: Vec<_> = (0..8u64)
        .map(|worker| {
            let db = Arc::clone(&db);
            std::thread::spawn(move || {
                for step in 0..50u64 {
                    let target = (worker * 50 + step) % 500;
                    let hits = db.search(&[target as f32, 0.0, 0.0, 0.0], 1).unwrap();
                    assert_eq!(hits[0].id, target);
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
}

/// Large enough to cross the parallel exact-scan threshold; integer
/// coordinates create many distance ties, so ID tie-breaking is exercised.
#[test]
fn large_exact_search_matches_brute_force_with_ties() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("parallel.vlite");
    let dimensions = 128;
    let rows = 6000u64;
    let mut db = Database::create(&path, dimensions).unwrap();
    let vector = |id: u64| -> Vec<f32> {
        (0..dimensions)
            .map(|d| ((id * 7 + d as u64 * 3) % 5) as f32)
            .collect()
    };
    let mut batch = Vec::new();
    for id in 0..rows {
        batch.push(Mutation::Put {
            id,
            vector: vector(id),
            payload: Vec::new(),
        });
    }
    for chunk in batch.chunks(500) {
        db.write_batch(chunk).unwrap();
    }

    let query = vector(17);
    let mut expected: Vec<(f64, u64)> = (0..rows)
        .map(|id| {
            let distance = vector(id)
                .iter()
                .zip(&query)
                .map(|(&a, &b)| {
                    let difference = f64::from(a) - f64::from(b);
                    difference * difference
                })
                .sum();
            (distance, id)
        })
        .collect();
    expected.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));

    for k in [1, 10, 100, 1000] {
        let hits = db.search(&query, k).unwrap();
        let got: Vec<(f64, u64)> = hits.iter().map(|hit| (hit.distance, hit.id)).collect();
        assert_eq!(got, expected[..k].to_vec(), "k={k}");
    }
}
