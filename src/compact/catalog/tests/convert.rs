//! P3a item 5: private copy-out precision conversion, not public activation.

use super::payload::{fingerprint, populate, snapshot};
use super::*;
use crate::compact::catalog::payload::Activation as Records;
use crate::vectors::{VectorSpec, VersionedVector};

fn convert(
    source: &Path,
    destination: &Path,
    key: Option<&EncryptionKey>,
    selected: &[(&str, Precision)],
    directory: bool,
) -> Result<maintenance::Inspection> {
    maintenance::convert_precision(source, destination, key, selected, directory)
}

fn fixture(key: Option<&EncryptionKey>) -> Loaded {
    let mut db = create_store(Backing::Memory, Path::new("unused"), key);
    populate(&mut db);
    db.collection_mut("full").unwrap().finish_import(2).unwrap();
    db.create_collection("dropped", spec(2)).unwrap();
    let job = db.next_import_job_id().unwrap();
    db.collection_mut("dropped")
        .unwrap()
        .import_chunk(job, 1, &[], POLICY)
        .unwrap();
    db.collection_mut("dropped")
        .unwrap()
        .abort_import(job)
        .unwrap();
    db.drop_collection("dropped").unwrap();
    db.create_collection_as("empty", spec(3), F16).unwrap();
    db.put_with_expiry(9, &[0.1, -0.0], b"expired", &Metadata::new(), Some(0))
        .unwrap();
    let mut g = db.graph_mut("g").unwrap();
    g.create_node("next", &["other"], &Metadata::new()).unwrap();
    g.create_edge(Some("edge"), "n", "expired", "next", &Metadata::new())
        .unwrap();
    g.set_edge_expiry(
        crate::property_graph::EdgeRef::Key("edge".into()),
        Some(0),
        Expected::Any,
    )
    .unwrap();
    g.set_node_expiry("n", Some(0), Expected::Any).unwrap();
    snapshot(&mut db, key)
}

fn save(path: &Path, loaded: &Loaded, directory: bool) {
    if directory {
        drop(crate::directory::create_converted(path, loaded).unwrap());
    } else {
        std::fs::write(path, image_of(loaded)).unwrap();
    }
}

fn bytes(path: &Path) -> Vec<(String, Vec<u8>)> {
    if path.is_dir() {
        listing(path)
            .into_iter()
            .map(|name| {
                let bytes = std::fs::read(path.join(&name)).unwrap();
                (name, bytes)
            })
            .collect()
    } else {
        vec![("file".into(), std::fs::read(path).unwrap())]
    }
}

fn expected(mut loaded: Loaded) -> Loaded {
    for (id, precision) in [(1, F32), (2, F16), (4, F32)] {
        let c = loaded.collections.get_mut(&id).unwrap();
        c.precision = Some(precision);
        let rows: Vec<_> = c.records.keys().copied().collect();
        for id in rows {
            for value in &mut c.records.get_mut(&id).unwrap().vector {
                *value = precision.canonical(*value).unwrap();
            }
        }
        for record in c.vectors.records.values_mut() {
            for version in record.dense.values_mut().flatten() {
                for value in &mut version.vector {
                    *value = precision.canonical(*value).unwrap();
                }
            }
        }
    }
    loaded
}

#[test]
fn conversion_preserves_all_state_across_file_directory_and_encryption() {
    let _precision = Activation::on();
    for records in [false, true] {
        let _records = Records::set(records);
        for key in [None, Some(key())] {
            let key = key.as_ref();
            for source_directory in [false, true] {
                for destination_directory in [false, true] {
                    let dir = tempfile::tempdir().unwrap();
                    let source = dir.path().join("source");
                    let destination = dir.path().join("converted");
                    let before = fixture(key);
                    let head = before.sequence;
                    let high = before.collection_high;
                    let wanted = fingerprint(&expected(fixture(key)));
                    save(&source, &before, source_directory);
                    let original = bytes(&source);
                    let report = convert(
                        &source,
                        &destination,
                        key,
                        &[("full", F16), ("half", F32), ("empty", F32)],
                        destination_directory,
                    )
                    .unwrap();
                    assert_eq!(report.commits, head);
                    let after = stored(&destination, key);
                    assert_eq!(fingerprint(&after), wanted);
                    assert_eq!(after.collection_high, high);
                    assert_eq!((after.sequence, after.history_floor), (head, head));
                    assert_ne!(after.lineage, before.lineage);
                    assert_eq!(
                        after.flags,
                        before.flags | FLAG_HISTORY | FLAG_COMPACT | FLAG_PRECISION
                    );
                    assert_eq!(bytes(&source), original);
                    let db = open_path(&destination, key).unwrap();
                    assert_eq!(
                        db.collection("full").unwrap().vector_specs().unwrap()["n"].history,
                        2
                    );
                    assert_eq!(db.next_import_job_id().unwrap(), 4);
                    db.close().unwrap();
                    {
                        let _disabled = Activation::set(false);
                        assert_eq!(outcome(open_path(&destination, key)), "unsupported 32");
                        assert_eq!(outcome(open_path(&destination, None)), "unsupported 32");
                    }
                    if records {
                        {
                            let _older = Records::set(false);
                            assert_eq!(outcome(open_path(&destination, key)), "unsupported 16");
                        }
                        let packed = dir.path().join("packed");
                        match key {
                            Some(key) => maintenance::backup_with_key(&destination, &packed, key),
                            None => maintenance::backup(&destination, &packed),
                        }
                        .unwrap();
                        super::payload::assert_source_blocks(&std::fs::read(packed).unwrap(), key);
                    }
                }
            }
        }
    }
}

#[test]
fn conversion_new_lineage_rejects_source_history() {
    let _precision = Activation::on();
    let _records = Records::on();
    for key in [None, Some(key())] {
        let key = key.as_ref();
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let destination = dir.path().join("converted");
        let replica = dir.path().join("replica");
        let before = fixture(key);
        save(&source, &before, false);
        convert(&source, &destination, key, &[("full", F16)], false).unwrap();
        let db = open_path(&destination, key).unwrap();
        assert_ne!(
            db.lineage(),
            before.lineage,
            "conversion must start a new lineage"
        );
        let cursor = FeedCursor {
            lineage: before.lineage.unwrap(),
            sequence: before.sequence,
        };
        assert!(matches!(
            db.changes_after(&cursor, 10),
            Err(Error::CursorCompacted { .. })
        ));
        assert!(matches!(
            db.export_log(before.sequence - 1, 1 << 20),
            Err(Error::CursorBeforeHistory { cursor, floor })
                if cursor == before.sequence - 1 && floor == before.sequence
        ));
        let source_segment =
            logops::export_log_segment(&source, key, before.sequence, 1 << 20).unwrap();
        db.close().unwrap();
        let original = bytes(&destination);
        let mut follower = ReplicaFollower::open(&destination, key).unwrap();
        assert!(matches!(
            follower.apply(&source_segment),
            Err(Error::ReplicaDiverged(_))
        ));
        drop(follower);
        assert_eq!(bytes(&destination), original);
        // A new backup is a valid bootstrap. Subsequent ordinary writes ship
        // without translating the new lineage or the retained numeric revisions.
        match key {
            Some(key) => maintenance::backup_with_key(&destination, &replica, key),
            None => maintenance::backup(&destination, &replica),
        }
        .unwrap();
        let mut db = open_path(&destination, key).unwrap();
        let revision = before.collections[&2].revisions[&0];
        assert_eq!(
            db.collection("full").unwrap().revision(0).unwrap(),
            Some(revision)
        );
        db.collection_mut("full")
            .unwrap()
            .put_with_metadata_if(
                0,
                &[0.2, -0.0],
                b"CAS survives",
                &Metadata::new(),
                Expected::Revision(revision),
            )
            .unwrap();
        let segment = db.export_log(before.sequence, 1 << 20).unwrap();
        let mut follower = ReplicaFollower::open(&replica, key).unwrap();
        follower.apply(&segment).unwrap();
        assert_eq!(
            follower
                .db()
                .collection("full")
                .unwrap()
                .get(0)
                .unwrap()
                .unwrap()
                .payload,
            b"CAS survives"
        );
    }
}

#[test]
fn conversion_matches_commit_time_binary16_goldens_and_exact_widening() {
    let _precision = Activation::on();
    let _records = Records::on();
    let document: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/compact/float16-cases.json"
    ))
    .unwrap();
    let accepted: Vec<_> = document["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| case.get("error").is_none())
        .collect();
    let bits = |case: &serde_json::Value, field: &str| {
        u32::from_str_radix(case[field].as_str().unwrap(), 16).unwrap()
    };
    let raw: Vec<_> = accepted
        .iter()
        .map(|case| f32::from_bits(bits(case, "input_f32_bits")))
        .collect();
    let golden: Vec<_> = accepted
        .iter()
        .map(|case| bits(case, "canonical_f32_bits"))
        .collect();
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let destination = dir.path().join("converted");
    let widened = dir.path().join("widened");
    let header = format::header_v5_flags(raw.len(), LINEAGE, 0, 0, None);
    let mut db = Database::open_from_bytes(header, None).unwrap();
    db.put(1, &raw, b"goldens").unwrap();
    std::fs::write(&source, db.to_bytes().unwrap()).unwrap();
    let original = bytes(&source);
    convert(&source, &destination, None, &[("default", F16)], false).unwrap();
    assert_eq!(bytes(&source), original);
    let after = stored(&destination, None);
    let actual: Vec<_> = after.collections[&0].records[&1]
        .vector
        .iter()
        .map(|v| v.to_bits())
        .collect();
    assert_eq!(actual, golden);
    let mut db = Database::open_from_bytes(image_of(&initial(None)), None).unwrap();
    db.create_collection_as("half", spec(raw.len()), F16)
        .unwrap();
    db.collection_mut("half")
        .unwrap()
        .put(1, &raw, b"goldens")
        .unwrap();
    let committed: Vec<_> = db
        .collection("half")
        .unwrap()
        .get(1)
        .unwrap()
        .unwrap()
        .vector
        .iter()
        .map(|v| v.to_bits())
        .collect();
    assert_eq!(actual, committed);
    convert(&destination, &widened, None, &[("default", F32)], true).unwrap();
    let widened = stored(&widened, None);
    assert_eq!(widened.collections[&0].precision, Some(F32));
    assert_eq!(
        widened.collections[&0].records,
        after.collections[&0].records
    );
    assert_eq!(
        widened.collections[&0].revisions,
        after.collections[&0].revisions
    );
}

#[test]
fn conversion_preflights_expired_primary_history_and_staged_overflow() {
    let _precision = Activation::on();
    for key in [None, Some(key())] {
        let key = key.as_ref();
        for path in 0..3 {
            for directory in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let source = dir.path().join("source");
                let destination = dir.path().join("converted");
                let mut db = create_store(Backing::Memory, Path::new("unused"), key);
                db.put_with_expiry(1, &[0.1, 0.0], b"p", &Metadata::new(), None)
                    .unwrap();
                if path == 0 {
                    db.put_with_expiry(1, &[65520.0, 0.0], b"expired", &Metadata::new(), Some(0))
                        .unwrap();
                } else {
                    let mut c = db.collection_mut("default").unwrap();
                    c.configure_vector(
                        "n",
                        VectorSpec {
                            dimensions: 2,
                            metric: crate::DistanceMetric::SquaredL2,
                            active_version: "v2".into(),
                            history: 2,
                        },
                    )
                    .unwrap();
                    if path == 1 {
                        c.put_vectors(
                            1,
                            crate::vectors::RecordVectors {
                                dense: std::collections::BTreeMap::from([(
                                    "n".into(),
                                    vec![
                                        VersionedVector {
                                            version: "v1".into(),
                                            vector: vec![65520.0, 0.0],
                                        },
                                        VersionedVector {
                                            version: "v2".into(),
                                            vector: vec![0.1, 0.0],
                                        },
                                    ],
                                )]),
                                sparse: Default::default(),
                            },
                        )
                        .unwrap();
                    } else {
                        c.begin_reembed("n", "v3").unwrap();
                        c.apply_reembed_batch("n", &[(1, vec![65520.0, 0.0])])
                            .unwrap();
                    }
                }
                std::fs::write(&source, db.to_bytes().unwrap()).unwrap();
                let original = bytes(&source);
                assert!(
                    matches!(
                        convert(&source, &destination, key, &[("default", F16)], directory),
                        Err(Error::NonFiniteVector)
                    ),
                    "path {path}, directory {directory}"
                );
                assert!(!destination.exists());
                assert_eq!(bytes(&source), original);
                assert_eq!(listing(dir.path()), vec!["source"]);
            }
        }
    }
}

#[test]
fn conversion_checks_only_selected_dense_values_and_all_selected_collections() {
    let _precision = Activation::on();
    let _records = Records::on();
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let destination = dir.path().join("converted");
    let rejected = dir.path().join("rejected");
    let mut db = create_store(Backing::Memory, Path::new("unused"), None);
    db.put(1, &[0.1, -0.0], b"selected").unwrap();
    db.create_collection("unselected", spec(2)).unwrap();
    db.collection_mut("unselected")
        .unwrap()
        .put(2, &[f32::MAX, -0.0], b"unselected overflow")
        .unwrap();
    db.collection_mut("default")
        .unwrap()
        .put_vectors(
            1,
            crate::vectors::RecordVectors {
                dense: Default::default(),
                sparse: std::collections::BTreeMap::from([("s".into(), vec![(3, f32::MAX)])]),
            },
        )
        .unwrap();
    let before = snapshot(&mut db, None);
    std::fs::write(&source, db.to_bytes().unwrap()).unwrap();
    let original = bytes(&source);
    convert(&source, &destination, None, &[("default", F16)], false).unwrap();
    let after = stored(&destination, None);
    assert_eq!(after.collections[&1].precision, Some(F32));
    assert_eq!(
        after.collections[&1].records,
        before.collections[&1].records
    );
    assert_eq!(
        after.collections[&0].vectors.encode().unwrap(),
        before.collections[&0].vectors.encode().unwrap()
    );
    assert!(matches!(
        convert(
            &source,
            &rejected,
            None,
            &[("default", F16), ("unselected", F16)],
            true,
        ),
        Err(Error::NonFiniteVector)
    ));
    assert!(!rejected.exists());
    assert_eq!(bytes(&source), original);
}

#[test]
fn conversion_is_private_and_rejects_incompatible_sources_before_output() {
    let _precision = Activation::on();
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let destination = dir.path().join("converted");
    let valid = format::header_v5_flags(2, LINEAGE, 0, 0, None);
    let mut future = valid.clone();
    future[8..12].copy_from_slice(&6u32.to_le_bytes());
    let crc = format::crc32(&future[..28]);
    future[28..32].copy_from_slice(&crc.to_le_bytes());
    let mut unsupported = valid.clone();
    unsupported[26..28].copy_from_slice(&0x8000u16.to_le_bytes());
    let crc = format::crc32(&unsupported[..28]);
    unsupported[28..32].copy_from_slice(&crc.to_le_bytes());
    let mut partial = valid.clone();
    let frame = format::record_frame_v5(
        1,
        0,
        1,
        &crate::Record {
            vector: vec![0.1, 0.0],
            payload: vec![],
            metadata: Metadata::new(),
        },
        None,
    );
    partial.extend_from_slice(&frame[..frame.len() - 1]);
    for (bytes, expected) in [
        (format::header_v4(2, LINEAGE, 0), "UnsupportedVersion(4)"),
        (future, "UnsupportedVersion(6)"),
        (unsupported, "unsupported 32768"),
        (
            format::header_v5_flags(2, LINEAGE, 0, FLAG_TIMESTAMPS, None),
            "unsupported 2",
        ),
        (partial, "incomplete"),
    ] {
        std::fs::write(&source, &bytes).unwrap();
        for directory in [false, true] {
            let result = convert(&source, &destination, None, &[("default", F16)], directory);
            if expected == "incomplete" {
                assert!(matches!(result, Err(Error::IncompleteTail { .. })));
            } else {
                assert_eq!(outcome(result), expected);
            }
            assert!(!destination.exists());
            assert_eq!(std::fs::read(&source).unwrap(), bytes);
        }
    }
    std::fs::write(&source, &valid).unwrap();
    {
        let _disabled = Activation::set(false);
        assert_eq!(
            outcome(convert(
                &source,
                &destination,
                None,
                &[("default", F16)],
                false
            )),
            "unsupported 32"
        );
    }
    convert(&source, &destination, None, &[("default", F16)], false).unwrap();
    let converted = std::fs::read(&destination).unwrap();
    assert_eq!(
        (
            stored(&destination, None).sequence,
            stored(&destination, None).history_floor
        ),
        (1, 1)
    );
    {
        let _disabled = Activation::set(false);
        assert_eq!(outcome(load(&converted, None)), "unsupported 32");
        // A compact-aware older reader cannot bypass the critical catalog by
        // stripping only the new header guard.
        let mut stripped = converted.clone();
        stripped[26..28].copy_from_slice(&(FLAG_HISTORY | FLAG_COMPACT).to_le_bytes());
        let crc = format::crc32(&stripped[..28]);
        stripped[28..32].copy_from_slice(&crc.to_le_bytes());
        assert!(load(&stripped, None).is_err());
    }
    let empty_directory = dir.path().join("empty-directory");
    convert(&source, &empty_directory, None, &[("default", F16)], true).unwrap();
    let empty = stored(&empty_directory, None);
    assert_eq!((empty.sequence, empty.history_floor), (1, 1));
    assert_eq!(
        listing(&empty_directory),
        vec!["LOCK", "MANIFEST", "image-1.vlite", "wal-2.log"]
    );
}

#[test]
fn conversion_refuses_open_imports_invalid_selection_locks_and_existing_paths() {
    let _precision = Activation::on();
    for directory in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let destination = dir.path().join("converted");
        let mut db = create_store(Backing::File, &source, None);
        assert!(matches!(
            convert(&source, &destination, None, &[("default", F16)], directory),
            Err(Error::Locked)
        ));
        db.import_chunk(1, 1, &[], POLICY).unwrap();
        db.close().unwrap();
        let original = bytes(&source);
        assert!(matches!(
            convert(&source, &destination, None, &[("default", F16)], directory),
            Err(Error::InvalidQuery(
                "precision conversion requires closed imports"
            ))
        ));
        assert!(!destination.exists());
        assert_eq!(bytes(&source), original);
        let mut db = open_path(&source, None).unwrap();
        db.finish_import(1).unwrap();
        db.close().unwrap();
        for selection in [
            vec![],
            vec![("missing", F16)],
            vec![("default", F16), ("default", F32)],
        ] {
            assert!(convert(&source, &destination, None, &selection, directory).is_err());
            assert!(!destination.exists());
        }
        std::fs::write(&destination, b"do not replace").unwrap();
        let original = bytes(&source);
        assert!(convert(&source, &destination, None, &[("default", F16)], directory).is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), b"do not replace");
        assert!(convert(&source, &source, None, &[("default", F16)], directory).is_err());
        assert_eq!(bytes(&source), original);
    }
}

#[test]
fn conversion_publication_failure_leaves_source_and_no_unpublished_destination() {
    let _precision = Activation::on();
    let _records = Records::on();
    for directory in [false, true] {
        for key in [None, Some(key())] {
            let key = key.as_ref();
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("source");
            let destination = dir.path().join("converted");
            save(&source, &fixture(key), false);
            let original = bytes(&source);
            {
                let _armed = failpoint::arm(failing(Stage::Staged));
                assert!(matches!(
                    convert(&source, &destination, key, &[("full", F16)], directory),
                    Err(Error::Io(_))
                ));
            }
            assert!(!destination.exists());
            assert_eq!(listing(dir.path()), vec!["source"]);
            assert_eq!(bytes(&source), original);
            {
                let _armed = failpoint::arm(failing(Stage::Visible));
                assert!(matches!(
                    convert(&source, &destination, key, &[("full", F16)], directory),
                    Err(Error::PublicationIndeterminate { .. })
                ));
            }
            assert_eq!(
                stored(&destination, key).history_floor,
                stored(&source, key).sequence
            );
            assert_eq!(bytes(&source), original);
        }
    }
}
