use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;
use vlite::{Database, Error, MAX_DIMENSIONS, MAX_PAYLOAD_BYTES};

fn setup() -> (TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("test.vlite");
    (directory, path)
}

#[test]
fn create_crud_close_reopen() {
    let (_directory, path) = setup();
    let mut db = Database::create(&path, 3).unwrap();
    assert_eq!(db.dimensions(), 3);
    assert!(db.is_empty());
    assert_eq!(db.get(100).unwrap(), None);
    assert!(!db.delete(100).unwrap());
    db.put(0, &[1.0, 2.0, 3.0], b"original").unwrap();
    db.put(u64::MAX, &[0.0, -0.0, 1.0], b"opaque\0bytes")
        .unwrap();
    db.put(0, &[4.0, 5.0, 6.0], b"replacement").unwrap();
    assert_eq!(db.len(), 2);
    assert!(db.delete(u64::MAX).unwrap());
    assert!(!db.delete(u64::MAX).unwrap());
    db.close().unwrap();

    let db = Database::open(&path).unwrap();
    assert_eq!(db.len(), 1);
    let record = db.get(0).unwrap().unwrap();
    assert_eq!(record.vector, [4.0, 5.0, 6.0]);
    assert_eq!(record.payload, b"replacement");
    assert_eq!(db.get(u64::MAX).unwrap(), None);
    db.close().unwrap();

    let mut db = Database::open(&path).unwrap();
    db.put(2, &[7.0, 8.0, 9.0], b"after reopen").unwrap();
    db.close().unwrap();
    assert_eq!(Database::open(path).unwrap().len(), 2);
}

#[test]
fn creation_never_overwrites_and_missing_open_never_creates() {
    let (_directory, path) = setup();
    assert!(matches!(Database::open(&path), Err(Error::Io(_))));
    assert!(!path.exists());
    fs::write(&path, b"existing unrelated data").unwrap();
    assert!(matches!(Database::create(&path, 2), Err(Error::Io(_))));
    assert_eq!(fs::read(path).unwrap(), b"existing unrelated data");
}

#[test]
fn invalid_dimensions_do_not_create_a_file() {
    let (_directory, path) = setup();
    for dimensions in [0, MAX_DIMENSIONS + 1, usize::MAX] {
        assert!(matches!(
            Database::create(&path, dimensions),
            Err(Error::InvalidDimensions(_))
        ));
        assert!(!path.exists());
    }
}

#[test]
fn validation_is_consistent_and_never_changes_storage() {
    let (_directory, path) = setup();
    Database::create(&path, 2).unwrap().close().unwrap();
    let original = fs::read(&path).unwrap();
    let mut db = Database::open(&path).unwrap();
    for vector in [&[][..], &[1.0][..], &[1.0, 2.0, 3.0][..]] {
        assert!(matches!(
            db.put(1, vector, b""),
            Err(Error::DimensionMismatch { .. })
        ));
        assert!(matches!(
            db.search(vector, 0),
            Err(Error::DimensionMismatch { .. })
        ));
    }
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert!(matches!(
            db.put(1, &[value, 0.0], b""),
            Err(Error::NonFiniteVector)
        ));
        assert!(matches!(
            db.search(&[0.0, value], 1),
            Err(Error::NonFiniteVector)
        ));
    }
    assert!(matches!(
        db.put(1, &[0.0, 0.0], &vec![0; MAX_PAYLOAD_BYTES + 1]),
        Err(Error::PayloadTooLarge(_))
    ));
    assert!(db.is_empty());
    assert!(db.search(&[0.0, 0.0], 10).unwrap().is_empty());
    db.close().unwrap();
    assert_eq!(fs::read(&path).unwrap(), original);
}

#[test]
fn exact_search_has_stable_ties_and_defined_k_behavior() {
    let (_directory, path) = setup();
    let mut db = Database::create(&path, 2).unwrap();
    for (id, vector) in [
        (9, [1.0, 0.0]),
        (2, [-1.0, 0.0]),
        (1, [0.0, 0.0]),
        (5, [2.0, 2.0]),
    ] {
        db.put(id, &vector, b"").unwrap();
    }
    assert!(db.search(&[0.0, 0.0], 0).unwrap().is_empty());
    let hits = db.search(&[0.0, 0.0], usize::MAX).unwrap();
    assert_eq!(
        hits.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        [1, 2, 9, 5]
    );
    assert_eq!(
        hits.iter().map(|hit| hit.distance).collect::<Vec<_>>(),
        [0.0, 1.0, 1.0, 8.0]
    );
    assert_eq!(db.search(&[0.0, 0.0], 2).unwrap(), hits[..2]);
    db.delete(1).unwrap();
    db.put(9, &[10.0, 10.0], b"").unwrap();
    db.close().unwrap();
    let db = Database::open(path).unwrap();
    assert_eq!(db.search(&[0.0, 0.0], 1).unwrap()[0].id, 2);
}

#[test]
fn exact_search_matches_independent_reference() {
    let (_directory, path) = setup();
    let mut db = Database::create(&path, 4).unwrap();
    let mut vectors = Vec::new();
    for id in 0..64u64 {
        let vector = [
            (id % 7) as f32 - 3.0,
            (id * 3 % 11) as f32,
            -(id as f32),
            id as f32 * 0.25,
        ];
        db.put(id, &vector, b"").unwrap();
        vectors.push((id, vector));
    }
    for query in [[1.5, -2.0, 3.0, 0.5], [0.0; 4], [50.0; 4]] {
        let mut expected: Vec<_> = vectors
            .iter()
            .map(|(id, vector)| {
                let distance = (0..4)
                    .map(|i| (f64::from(vector[i]) - f64::from(query[i])).powi(2))
                    .sum::<f64>();
                (*id, distance)
            })
            .collect();
        expected.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        let actual = db.search(&query, 13).unwrap();
        assert_eq!(
            actual
                .iter()
                .map(|hit| (hit.id, hit.distance))
                .collect::<Vec<_>>(),
            expected[..13]
        );
    }
}

#[test]
fn extreme_finite_vectors_do_not_overflow_distance() {
    let (_directory, path) = setup();
    let mut db = Database::create(path, 1).unwrap();
    db.put(1, &[f32::MAX], b"").unwrap();
    let hit = &db.search(&[-f32::MAX], 1).unwrap()[0];
    assert!(hit.distance.is_finite());
    assert!(hit.distance > f64::from(f32::MAX));
}

#[test]
fn maximum_record_sizes_round_trip() {
    let (_directory, path) = setup();
    let mut db = Database::create(&path, MAX_DIMENSIONS).unwrap();
    let vector = vec![1.25; MAX_DIMENSIONS];
    let payload = vec![0xa5; MAX_PAYLOAD_BYTES];
    db.put(1, &vector, &payload).unwrap();
    db.close().unwrap();
    let db = Database::open(path).unwrap();
    let record = db.get(1).unwrap().unwrap();
    assert_eq!(record.vector, vector);
    assert_eq!(record.payload, payload);
}

#[test]
fn exclusive_locks_release_on_close_and_failed_open() {
    let (_directory, path) = setup();
    let db = Database::create(&path, 2).unwrap();
    assert!(matches!(Database::open(&path), Err(Error::Locked)));
    assert!(matches!(
        Database::discard_incomplete_tail(&path),
        Err(Error::Locked)
    ));
    db.close().unwrap();
    Database::open(&path).unwrap().close().unwrap();
    fs::write(&path, b"not a database").unwrap();
    assert!(matches!(Database::open(&path), Err(Error::Corrupt { .. })));
    assert!(matches!(Database::open(&path), Err(Error::Corrupt { .. })));
}

#[test]
fn incomplete_write_at_every_byte_requires_explicit_repair() {
    let (_directory, path) = setup();
    let mut db = Database::create(&path, 2).unwrap();
    db.put(1, &[1.0, 2.0], b"acknowledged").unwrap();
    let committed_length = fs::metadata(&path).unwrap().len() as usize;
    db.put(2, &[3.0, 4.0], b"new record").unwrap();
    db.close().unwrap();
    let complete = fs::read(&path).unwrap();
    for cut in committed_length + 1..complete.len() {
        fs::write(&path, &complete[..cut]).unwrap();
        assert!(
            matches!(
                Database::open(&path),
                Err(Error::IncompleteTail { offset, bytes })
                    if offset == committed_length as u64
                        && bytes == (cut - committed_length) as u64
            ),
            "unexpected result at cut {cut}"
        );
        assert_eq!(fs::read(&path).unwrap(), complete[..cut]);
        let report = Database::discard_incomplete_tail(&path).unwrap();
        assert_eq!(report.discarded_bytes, (cut - committed_length) as u64);
        assert_eq!(report.retained_commits, 1);
        let mut db = Database::open(&path).unwrap();
        assert_eq!(db.get(1).unwrap().unwrap().payload, b"acknowledged");
        assert!(db.get(2).unwrap().is_none());
        db.put(3, &[5.0, 6.0], b"after repair").unwrap();
        db.close().unwrap();
        assert_eq!(Database::open(&path).unwrap().len(), 2);
    }
    fs::write(&path, &complete).unwrap();
    assert_eq!(Database::open(&path).unwrap().len(), 2);
}

#[test]
fn incomplete_update_and_delete_preserve_previous_state_on_explicit_repair() {
    let (_directory, path) = setup();
    let mut db = Database::create(&path, 1).unwrap();
    db.put(1, &[1.0], b"old").unwrap();
    let original_end = fs::metadata(&path).unwrap().len();
    db.put(1, &[2.0], b"new").unwrap();
    db.close().unwrap();
    let full = fs::read(&path).unwrap();
    fs::write(&path, &full[..full.len() - 1]).unwrap();
    Database::discard_incomplete_tail(&path).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().len(), original_end);
    let mut db = Database::open(&path).unwrap();
    assert_eq!(db.get(1).unwrap().unwrap().payload, b"old");
    db.delete(1).unwrap();
    db.close().unwrap();
    let full = fs::read(&path).unwrap();
    fs::write(&path, &full[..full.len() - 1]).unwrap();
    Database::discard_incomplete_tail(&path).unwrap();
    assert_eq!(
        Database::open(&path)
            .unwrap()
            .get(1)
            .unwrap()
            .unwrap()
            .vector,
        [1.0]
    );
}

#[test]
fn corruption_is_never_automatically_or_explicitly_discarded() {
    let (_directory, path) = setup();
    let mut db = Database::create(&path, 2).unwrap();
    db.put(1, &[1.0, 2.0], b"payload").unwrap();
    db.put(2, &[3.0, 4.0], b"more").unwrap();
    db.close().unwrap();
    let original = fs::read(&path).unwrap();
    for offset in 0..original.len() {
        let mut corrupt = original.clone();
        corrupt[offset] ^= 0x01;
        fs::write(&path, &corrupt).unwrap();
        assert!(
            matches!(Database::open(&path), Err(Error::Corrupt { .. })),
            "byte {offset}"
        );
        assert!(matches!(
            Database::discard_incomplete_tail(&path),
            Err(Error::Corrupt { .. })
        ));
        assert_eq!(fs::read(&path).unwrap(), corrupt);
    }
}

#[test]
fn bad_header_and_garbage_tail_are_not_repairable() {
    let (_directory, path) = setup();
    Database::create(&path, 1).unwrap().close().unwrap();
    let header = fs::read(&path).unwrap();
    for cut in 0..header.len() {
        fs::write(&path, &header[..cut]).unwrap();
        assert!(matches!(
            Database::discard_incomplete_tail(&path),
            Err(Error::Corrupt { .. })
        ));
    }
    fs::write(&path, &header).unwrap();
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"garbage")
        .unwrap();
    assert!(matches!(Database::open(&path), Err(Error::Corrupt { .. })));
    assert!(matches!(
        Database::discard_incomplete_tail(&path),
        Err(Error::Corrupt { .. })
    ));
}

#[test]
fn repair_of_clean_file_is_a_noop_and_no_sidecars_are_created() {
    let (directory, path) = setup();
    let mut db = Database::create(&path, 1).unwrap();
    db.put(1, &[1.0], b"").unwrap();
    db.close().unwrap();
    let before = fs::read(&path).unwrap();
    let report = Database::discard_incomplete_tail(&path).unwrap();
    assert_eq!(report.discarded_bytes, 0);
    assert_eq!(report.retained_commits, 1);
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

fn child(action: &str, path: &Path) -> std::process::Output {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "subprocess_entry", "--nocapture"])
        .env("VLITE_TEST_ACTION", action)
        .env("VLITE_TEST_PATH", path)
        .output()
        .unwrap()
}

#[test]
fn subprocess_entry() {
    let Ok(action) = std::env::var("VLITE_TEST_ACTION") else {
        return;
    };
    let path = std::env::var_os("VLITE_TEST_PATH").unwrap();
    match action.as_str() {
        "write-without-drop" => {
            let mut db = Database::create(path, 2).unwrap();
            db.put(1, &[1.0, 2.0], b"durable").unwrap();
            db.put(2, &[3.0, 4.0], b"deleted").unwrap();
            db.delete(2).unwrap();
            std::process::exit(0);
        }
        "expect-locked" => {
            assert!(matches!(Database::open(path), Err(Error::Locked)));
        }
        _ => panic!("unknown subprocess action"),
    }
}

#[test]
fn acknowledged_commits_survive_process_exit_without_destructors() {
    let (_directory, path) = setup();
    let output = child("write-without-drop", &path);
    assert!(output.status.success(), "{output:?}");
    let db = Database::open(path).unwrap();
    assert_eq!(db.len(), 1);
    assert_eq!(db.get(1).unwrap().unwrap().payload, b"durable");
    assert!(db.get(2).unwrap().is_none());
}

#[test]
fn lock_excludes_other_processes() {
    let (_directory, path) = setup();
    let db = Database::create(&path, 2).unwrap();
    let output = child("expect-locked", &path);
    assert!(output.status.success(), "{output:?}");
    db.close().unwrap();
}
