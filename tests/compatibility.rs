use std::fs;
use vlite::{Database, Error, Mutation, maintenance};

fn decode_hex(text: &str) -> Vec<u8> {
    let digits: String = text
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .collect();
    assert!(digits.len().is_multiple_of(2));
    digits
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

#[test]
fn fixed_legacy_and_current_files_replay_and_keep_their_format_on_single_writes() {
    let directory = tempfile::tempdir().unwrap();
    for (version, fixture) in [
        (1, include_str!("fixtures/v1.hex")),
        (2, include_str!("fixtures/v2.hex")),
        (3, include_str!("fixtures/v3.hex")),
    ] {
        let path = directory.path().join(format!("v{version}.vlite"));
        let bytes = decode_hex(fixture);
        fs::write(&path, &bytes).unwrap();
        let backup = directory.path().join(format!("backup-v{version}.vlite"));
        let restored = directory.path().join(format!("restored-v{version}.vlite"));
        assert_eq!(
            maintenance::backup(&path, &backup).unwrap().format_version,
            version
        );
        assert_eq!(
            maintenance::restore(&backup, &restored)
                .unwrap()
                .format_version,
            version
        );
        assert_eq!(fs::read(&backup).unwrap(), bytes);
        assert_eq!(fs::read(&restored).unwrap(), bytes);
        let mut db = Database::open(&path).unwrap();
        assert_eq!(db.format_version(), version);
        assert_eq!(db.dimensions(), 2);
        assert_eq!(db.len(), 1);
        assert!(db.get(7).unwrap().is_none());
        assert!(db.get(11).unwrap().is_none());
        let record = db.get(9).unwrap().unwrap();
        assert_eq!(record.vector, [0.0, 2.0]);
        assert_eq!(record.payload, b"b");
        if version == 1 {
            db.write_batch(&[]).unwrap();
            assert!(matches!(
                db.write_batch(&[Mutation::Delete { id: 9 }]),
                Err(Error::BatchRequiresUpgrade)
            ));
            db.close().unwrap();
            assert_eq!(fs::read(&path).unwrap(), bytes);
            db = Database::open(&path).unwrap();
        }
        db.put(0, &[1.0, -0.0], b"after fixture").unwrap();
        db.close().unwrap();
        let db = Database::open(&path).unwrap();
        assert_eq!(db.format_version(), version);
        assert_eq!(
            db.get(0).unwrap().unwrap().vector[1].to_bits(),
            (-0.0f32).to_bits()
        );
    }
}

#[test]
fn v2_writer_matches_independently_encoded_golden_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("v2.vlite");
    let fixture = decode_hex(include_str!("fixtures/v2.hex"));
    fs::write(&path, &fixture[..32]).unwrap();
    let mut db = Database::open(&path).unwrap();
    db.write_batch(&[
        Mutation::Put {
            id: 7,
            vector: vec![1.0, -0.0],
            payload: b"a".to_vec(),
        },
        Mutation::Put {
            id: 9,
            vector: vec![0.0, 2.0],
            payload: b"b".to_vec(),
        },
        Mutation::Delete { id: 7 },
    ])
    .unwrap();
    db.put(11, &[3.0, 4.0], b"c").unwrap();
    db.delete(11).unwrap();
    db.close().unwrap();
    assert_eq!(
        fs::read(path).unwrap(),
        decode_hex(include_str!("fixtures/v2.hex"))
    );
}

#[test]
fn legacy_single_operation_writer_still_matches_original_golden_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("v1.vlite");
    let expected = decode_hex(include_str!("fixtures/v1.hex"));
    fs::write(&path, &expected[..32]).unwrap();
    let mut db = Database::open(&path).unwrap();
    db.put(7, &[1.0, -0.0], b"a").unwrap();
    db.put(9, &[0.0, 2.0], b"b").unwrap();
    db.delete(7).unwrap();
    db.close().unwrap();
    assert_eq!(fs::read(path).unwrap(), expected);
}
#[test]
fn copy_out_compaction_explicitly_upgrades_legacy_files_without_changing_the_source() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("v1.vlite");
    let output = directory.path().join("v2.vlite");
    let bytes = decode_hex(include_str!("fixtures/v1.hex"));
    fs::write(&source, &bytes).unwrap();
    let report = maintenance::compact(&source, &output).unwrap();
    assert_eq!(report.source.format_version, 1);
    assert_eq!(report.output.format_version, 3);
    assert_eq!(fs::read(source).unwrap(), bytes);
    let mut db = Database::open(&output).unwrap();
    db.write_batch(&[Mutation::Delete { id: 9 }]).unwrap();
    db.close().unwrap();
    assert!(Database::open(output).unwrap().is_empty());
}
