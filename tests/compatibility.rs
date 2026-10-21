use std::fs;
use vlite::{
    Database, Error, ImportState, Metadata, MetadataValue, Mutation, SettingsPersistence,
    maintenance,
};

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

fn v3_fixture_metadata() -> Metadata {
    [
        ("active".to_owned(), MetadataValue::Boolean(true)),
        ("count".to_owned(), MetadataValue::Integer(i64::MIN)),
        ("score".to_owned(), MetadataValue::Float(-0.0)),
        (
            "source".to_owned(),
            MetadataValue::String("docs".to_owned()),
        ),
    ]
    .into()
}

/// AC-002: the refactored collection-aware writer still emits byte-identical v3
/// frames for metadata puts, batches (put-with-metadata, put, delete).
#[test]
fn v3_writer_matches_independently_encoded_golden_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("v3.vlite");
    let expected = decode_hex(include_str!("fixtures/v3.hex"));
    fs::write(&path, &expected[..32]).unwrap();
    let mut db = Database::open(&path).unwrap();
    db.put_with_metadata(7, &[1.0, -0.0], &[0, 0xff], &v3_fixture_metadata())
        .unwrap();
    db.write_batch(&[
        Mutation::PutWithMetadata {
            id: 7,
            vector: vec![1.0, -0.0],
            payload: vec![0, 0xff],
            metadata: v3_fixture_metadata(),
        },
        Mutation::Put {
            id: 9,
            vector: vec![0.0, 2.0],
            payload: b"b".to_vec(),
        },
        Mutation::Delete { id: 7 },
    ])
    .unwrap();
    db.close().unwrap();
    assert_eq!(fs::read(&path).unwrap(), expected);
}

/// AC-002: appends to an existing v3 file keep the baseline frame encodings
/// (single put/delete use v2-compatible ops 1/2; header and prefix untouched).
#[test]
fn v3_appends_keep_prefix_and_baseline_frame_encodings() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("v3.vlite");
    let fixture = decode_hex(include_str!("fixtures/v3.hex"));
    let v2 = decode_hex(include_str!("fixtures/v2.hex"));
    fs::write(&path, &fixture).unwrap();
    let mut db = Database::open(&path).unwrap();
    // CAS and collection-handle writes on v3 must also use baseline encoders.
    db.collection_mut("default")
        .unwrap()
        .put(11, &[3.0, 4.0], b"c")
        .unwrap();
    assert!(db.delete_if(11, vlite::Expected::Revision(3)).unwrap());
    assert_eq!(
        db.set_collection_settings("default", Default::default())
            .unwrap(),
        SettingsPersistence::MemoryOnly
    );
    db.close().unwrap();
    let bytes = fs::read(&path).unwrap();
    assert_eq!(&bytes[..fixture.len()], fixture.as_slice());
    // The v2 fixture's put(11)/delete(11) frames are sequences 2 and 3; the v3
    // continuation produces identical frames at sequences 3 and 4.
    let mut tail = v2[v2.len() - 2 * 52 - 9..].to_vec();
    let put_len = 52 + 9;
    for (offset, sequence) in [(0usize, 3u64), (put_len, 4u64)] {
        let frame_len = if offset == 0 { put_len } else { 52 };
        let frame = &mut tail[offset..offset + frame_len];
        frame[8..16].copy_from_slice(&sequence.to_le_bytes());
        let header_crc = crc32(&frame[..32]);
        frame[32..36].copy_from_slice(&header_crc.to_le_bytes());
        let body_end = frame_len - 16;
        let commit_crc = crc32(&frame[..body_end]);
        frame[body_end + 4..body_end + 8].copy_from_slice(&commit_crc.to_le_bytes());
        frame[body_end + 8..].copy_from_slice(&sequence.to_le_bytes());
    }
    assert_eq!(&bytes[fixture.len()..], tail.as_slice());
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

#[test]
fn v4_fixture_loads_with_collections_settings_and_import_marker() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("v4.vlite");
    let bytes = decode_hex(include_str!("fixtures/v4.hex"));
    fs::write(&path, &bytes).unwrap();
    let mut db = Database::open(&path).unwrap();
    assert_eq!(db.format_version(), 4);
    assert_eq!(db.lineage(), Some(0x0123_4567_89ab_cdef));
    assert_eq!(db.get(9).unwrap().unwrap().vector.len(), 2);
    assert_eq!(
        db.collection("default").unwrap().settings().text_fields,
        ["title"]
    );
    let docs = db.collection("docs").unwrap();
    assert_eq!((docs.id(), docs.dimensions(), docs.len()), (1, 3, 2));
    let status = db.import_status(1).unwrap();
    assert_eq!(status.collection, "docs");
    assert_eq!(status.state, ImportState::Open);
    assert_eq!((status.last_chunk, status.rows_committed), (1, 1));
    assert_ne!(status.last_digest, [0; 32]);
    // Writes continue the v4 log; the fixture prefix is never rewritten.
    db.put(10, &[1.0, 1.0], b"").unwrap();
    db.close().unwrap();
    assert_eq!(&fs::read(&path).unwrap()[..bytes.len()], bytes.as_slice());
    let backup = directory.path().join("backup.vlite");
    maintenance::backup(&path, &backup).unwrap();
    assert_eq!(fs::read(&backup).unwrap(), fs::read(&path).unwrap());
}
