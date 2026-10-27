use std::{collections::BTreeMap, fs};
use vlite::{
    Condition, Database, Error, HnswConfig, Metadata, MetadataValue as V, Mutation, maintenance,
};

fn metadata() -> Metadata {
    BTreeMap::from([
        ("active".into(), V::Boolean(true)),
        ("count".into(), V::Integer(i64::MIN)),
        ("score".into(), V::Float(-0.0)),
        ("source".into(), V::String("docs".into())),
    ])
}

fn fixture() -> Vec<u8> {
    let digits = include_str!("fixtures/v3.hex").trim().as_bytes();
    digits
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

#[test]
fn v3_golden_bytes_pin_typed_put_and_mixed_atomic_batch() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("v3.vlite");
    let mut db = Database::create_with_format(&path, 2, vlite::FormatVersion::V3).unwrap();
    db.put_with_metadata(7, &[1.0, -0.0], &[0, 255], &metadata())
        .unwrap();
    db.write_batch(&[
        Mutation::PutWithMetadata {
            id: 7,
            vector: vec![1.0, -0.0],
            payload: vec![0, 255],
            metadata: metadata(),
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
    assert_eq!(fs::read(&path).unwrap(), fixture());
    let db = Database::open(path).unwrap();
    assert_eq!(db.len(), 1);
    assert!(db.get(9).unwrap().unwrap().metadata.is_empty());
}

#[test]
fn typed_metadata_round_trips_and_survives_all_maintenance_without_source_changes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("data.vlite");
    let mut db = Database::create_with_format(&path, 2, vlite::FormatVersion::V3).unwrap();
    db.put_with_metadata(u64::MAX, &[1.0, -0.0], &[0, 255], &metadata())
        .unwrap();
    db.put_with_metadata(0, &[0.0, 1.0], b"old", &metadata())
        .unwrap();
    db.put(0, &[2.0, 3.0], b"replacement clears metadata")
        .unwrap();
    db.build_hnsw(HnswConfig::default()).unwrap();
    db.write_batch(&[
        Mutation::PutWithMetadata {
            id: 2,
            vector: vec![4.0, 5.0],
            payload: vec![],
            metadata: metadata(),
        },
        Mutation::Delete { id: 0 },
    ])
    .unwrap();
    db.close().unwrap();
    let before = fs::read(&path).unwrap();
    let backup = directory.path().join("backup.vlite");
    let restored = directory.path().join("restored.vlite");
    let compact = directory.path().join("compact.vlite");
    maintenance::backup(&path, &backup).unwrap();
    maintenance::restore(&backup, &restored).unwrap();
    maintenance::compact(&restored, &compact).unwrap();
    for output in [&path, &backup, &restored, &compact] {
        let db = Database::open(output).unwrap();
        assert_eq!(db.len(), 2);
        for id in [u64::MAX, 2] {
            let stored = &db.get(id).unwrap().unwrap().metadata;
            assert_eq!(stored, &metadata());
            if let V::Float(value) = stored["score"] {
                assert_eq!(value.to_bits(), (-0.0f64).to_bits());
            }
        }
    }
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(fs::read(backup).unwrap(), before);
}

#[test]
fn exact_filters_precede_top_k_and_use_strict_types_and_inclusive_ranges() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = Database::create_with_format(
        directory.path().join("filter.vlite"),
        1,
        vlite::FormatVersion::V3,
    )
    .unwrap();
    for id in 0..10 {
        let metadata = BTreeMap::from([
            (
                "kind".into(),
                V::String(if id < 5 { "excluded" } else { "included" }.into()),
            ),
            ("count".into(), V::Integer(id)),
            ("float".into(), V::Float(id as f64)),
        ]);
        db.put_with_metadata(id as u64, &[id as f32], b"", &metadata)
            .unwrap();
    }
    let eq = Condition::Eq {
        field: "kind".into(),
        value: V::String("included".into()),
    };
    assert_eq!(
        db.search_filtered(&[0.0], 2, std::slice::from_ref(&eq))
            .unwrap()
            .iter()
            .map(|hit| hit.id)
            .collect::<Vec<_>>(),
        [5, 6]
    );
    let range = Condition::Range {
        field: "count".into(),
        min: Some(V::Integer(6)),
        max: Some(V::Integer(7)),
    };
    assert_eq!(
        db.search_filtered(&[0.0], 10, &[eq, range])
            .unwrap()
            .iter()
            .map(|hit| hit.id)
            .collect::<Vec<_>>(),
        [6, 7]
    );
    assert!(
        db.search_filtered(
            &[0.0],
            10,
            &[Condition::Eq {
                field: "count".into(),
                value: V::Float(5.0)
            }]
        )
        .unwrap()
        .is_empty()
    );
    assert!(
        db.search_filtered(
            &[0.0],
            10,
            &[Condition::Range {
                field: "count".into(),
                min: Some(V::Float(0.0)),
                max: None
            }]
        )
        .unwrap()
        .is_empty()
    );
    assert!(
        db.search_filtered(
            &[0.0],
            10,
            &[Condition::Eq {
                field: "missing".into(),
                value: V::Boolean(false)
            }]
        )
        .unwrap()
        .is_empty()
    );
    assert_eq!(
        db.search_filtered(&[0.0], 10, &[]).unwrap(),
        db.search(&[0.0], 10).unwrap()
    );
    assert_eq!(
        db.search_filtered(
            &[0.0],
            10,
            &[Condition::Range {
                field: "float".into(),
                min: None,
                max: Some(V::Float(0.0))
            }]
        )
        .unwrap()[0]
            .id,
        0
    );
    for condition in [
        Condition::Range {
            field: "count".into(),
            min: None,
            max: None,
        },
        Condition::Range {
            field: "count".into(),
            min: Some(V::Integer(2)),
            max: Some(V::Integer(1)),
        },
        Condition::Range {
            field: "count".into(),
            min: Some(V::Integer(1)),
            max: Some(V::Float(2.0)),
        },
        Condition::Range {
            field: "count".into(),
            min: Some(V::Boolean(true)),
            max: None,
        },
        Condition::Eq {
            field: "".into(),
            value: V::String("x".into()),
        },
        Condition::Eq {
            field: "float".into(),
            value: V::Float(f64::NAN),
        },
    ] {
        assert!(db.search_filtered(&[0.0], 0, &[condition]).is_err());
    }
    assert!(
        db.search_filtered(
            &[0.0],
            0,
            &vec![
                Condition::Eq {
                    field: "a".into(),
                    value: V::Boolean(true)
                };
                65
            ]
        )
        .is_err()
    );
}

#[test]
fn metadata_limits_reject_whole_batch_before_writing_and_old_files_require_upgrade() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("limits.vlite");
    let mut db = Database::create_with_format(&path, 1, vlite::FormatVersion::V3).unwrap();
    let invalid = [
        BTreeMap::from([("".into(), V::Boolean(true))]),
        BTreeMap::from([("x".repeat(257), V::Boolean(true))]),
        BTreeMap::from([("x".into(), V::String("x".repeat(4097)))]),
        BTreeMap::from([("x".into(), V::Float(f64::INFINITY))]),
        (0..65)
            .map(|id| (id.to_string(), V::Boolean(true)))
            .collect(),
        (0..16)
            .map(|id| (id.to_string(), V::String("x".repeat(4096))))
            .collect(),
    ];
    for metadata in invalid {
        assert!(matches!(
            db.put_with_metadata(0, &[0.0], b"", &metadata),
            Err(Error::InvalidMetadata(_))
        ));
        assert!(
            db.write_batch(&[
                Mutation::Put {
                    id: 1,
                    vector: vec![1.0],
                    payload: vec![]
                },
                Mutation::PutWithMetadata {
                    id: 2,
                    vector: vec![2.0],
                    payload: vec![],
                    metadata
                },
            ])
            .is_err()
        );
        assert!(db.is_empty());
        assert_eq!(fs::metadata(&path).unwrap().len(), 32);
    }
    let maximum = BTreeMap::from([("k".repeat(256), V::String("v".repeat(4096)))]);
    db.put_with_metadata(0, &[0.0], b"", &maximum).unwrap();
    db.close().unwrap();
    for (version, hex) in [
        (1, include_str!("fixtures/v1.hex")),
        (2, include_str!("fixtures/v2.hex")),
    ] {
        let digits = hex.split_whitespace().collect::<String>();
        let bytes = digits
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect::<Vec<_>>();
        let path = directory.path().join(format!("v{version}.vlite"));
        fs::write(&path, &bytes).unwrap();
        let mut db = Database::open(&path).unwrap();
        assert!(matches!(
            db.put_with_metadata(0, &[0.0, 0.0], b"", &metadata()),
            Err(Error::MetadataRequiresUpgrade)
        ));
        db.close().unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes);
        let upgraded = directory.path().join(format!("upgraded-{version}.vlite"));
        maintenance::compact(&path, &upgraded).unwrap();
        let mut db = Database::open(upgraded).unwrap();
        assert_eq!(db.format_version(), 3);
        db.put_with_metadata(0, &[0.0, 0.0], b"", &metadata())
            .unwrap();
    }
}

#[test]
fn every_metadata_frame_truncation_and_corruption_fails_closed() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("damaged.vlite");
    let bytes = fixture();
    // The independent fixture has a 129-byte metadata put followed by a 186-byte mixed batch.
    for (start, end, prior) in [(32, 161, 0), (161, 347, 1)] {
        for cut in start + 1..end {
            fs::write(&path, &bytes[..cut]).unwrap();
            assert!(matches!(
                Database::open(&path),
                Err(Error::IncompleteTail { .. })
            ));
            Database::discard_incomplete_tail(&path).unwrap();
            let db = Database::open(&path).unwrap();
            assert_eq!(db.len(), prior);
            if prior == 1 {
                assert_eq!(db.get(7).unwrap().unwrap().metadata, metadata());
            }
        }
        for position in start..end {
            let mut corrupt = bytes[..end].to_vec();
            corrupt[position] ^= 1;
            fs::write(&path, &corrupt).unwrap();
            assert!(matches!(Database::open(&path), Err(Error::Corrupt { .. })));
            assert!(Database::discard_incomplete_tail(&path).is_err());
            assert_eq!(fs::read(&path).unwrap(), corrupt);
        }
    }
}

#[test]
fn exact_metadata_byte_and_field_limits_round_trip() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("boundaries.vlite");
    let mut db = Database::create_with_format(&path, 1, vlite::FormatVersion::V3).unwrap();
    let mut exact = (0..15)
        .map(|id| (format!("{id:02}"), V::String("x".repeat(4096))))
        .collect::<Metadata>();
    exact.insert("15".into(), V::String("y".repeat(3948)));
    db.put_with_metadata(0, &[0.0], b"", &exact).unwrap();
    let mut too_large = exact.clone();
    if let V::String(value) = too_large.get_mut("15").unwrap() {
        value.push('z');
    }
    assert!(matches!(
        db.put_with_metadata(0, &[0.0], b"", &too_large),
        Err(Error::InvalidMetadata(_))
    ));
    let fields = (0..64)
        .map(|id| (id.to_string(), V::Boolean(true)))
        .collect::<Metadata>();
    db.write_batch(&[Mutation::PutWithMetadata {
        id: 1,
        vector: vec![1.0],
        payload: vec![],
        metadata: fields.clone(),
    }])
    .unwrap();
    db.close().unwrap();
    let db = Database::open(path).unwrap();
    assert_eq!(db.get(0).unwrap().unwrap().metadata, exact);
    assert_eq!(db.get(1).unwrap().unwrap().metadata, fields);
}

#[test]
fn metadata_process_exit_child() {
    let Some(path) = std::env::var_os("VLITE_METADATA_EXIT_PATH") else {
        return;
    };
    let mut db = Database::create_with_format(path, 2, vlite::FormatVersion::V3).unwrap();
    db.put_with_metadata(7, &[1.0, 2.0], b"single", &metadata())
        .unwrap();
    db.write_batch(&[
        Mutation::PutWithMetadata {
            id: 0,
            vector: vec![0.0, 0.0],
            payload: vec![],
            metadata: metadata(),
        },
        Mutation::PutWithMetadata {
            id: u64::MAX,
            vector: vec![3.0, 4.0],
            payload: vec![255],
            metadata: metadata(),
        },
        Mutation::Delete { id: 0 },
    ])
    .unwrap();
    std::process::exit(0);
}

#[test]
fn acknowledged_metadata_commits_survive_exit_without_destructors() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("exit.vlite");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "metadata_process_exit_child"])
        .env("VLITE_METADATA_EXIT_PATH", &path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let db = Database::open(path).unwrap();
    assert_eq!(db.len(), 2);
    assert_eq!(db.get(7).unwrap().unwrap().metadata, metadata());
    assert_eq!(db.get(u64::MAX).unwrap().unwrap().metadata, metadata());
    assert!(db.get(0).unwrap().is_none());
}
