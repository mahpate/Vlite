use super::*;
use crate::compact::catalog::payload::{self, Activation as Records};
use crate::vectors::{RecordVectors, VectorSpec, VersionedVector};
use crate::{DistanceMetric, MetadataValue};
use std::collections::BTreeMap;

pub(super) fn populate(db: &mut Database) {
    for (name, precision) in [("half", F16), ("full", F32)] {
        db.create_collection_as(name, spec(2), precision).unwrap();
        let mut c = db.collection_mut(name).unwrap();
        c.configure_vector(
            "n",
            VectorSpec {
                dimensions: 2,
                metric: DistanceMetric::SquaredL2,
                active_version: "v1".into(),
                history: 2,
            },
        )
        .unwrap();
        for id in [0, 3, 10, u64::MAX] {
            let metadata = Metadata::from([
                ("id".into(), MetadataValue::String(id.to_string())),
                ("flag".into(), MetadataValue::Boolean(true)),
                ("number".into(), MetadataValue::Float(0.125)),
                ("integer".into(), MetadataValue::Integer(i64::MIN)),
                (
                    "place".into(),
                    MetadataValue::GeoPoint {
                        latitude: 0.0,
                        longitude: 1.0,
                    },
                ),
            ]);
            c.put_with_expiry(
                id,
                &[0.1, -0.0],
                &id.to_le_bytes(),
                &metadata,
                (id == 3).then_some(FAR),
            )
            .unwrap();
            c.put_vectors(
                id,
                RecordVectors {
                    dense: BTreeMap::from([(
                        "n".into(),
                        vec![VersionedVector {
                            version: "v1".into(),
                            vector: vec![0.3, -0.0],
                        }],
                    )]),
                    sparse: BTreeMap::from([("s".into(), vec![(3, 0.1), (20, -0.2)])]),
                },
            )
            .unwrap();
        }
        c.begin_reembed("n", "v2").unwrap();
        c.apply_reembed_batch("n", &[(0, vec![0.6, 0.2])]).unwrap();
    }
    for (name, closed) in [("half", true), ("full", false)] {
        let job = db.next_import_job_id().unwrap();
        let mut c = db.collection_mut(name).unwrap();
        c.import_chunk(
            job,
            1,
            &[Mutation::Put {
                id: 20,
                vector: vec![0.25, 0.5],
                payload: vec![0, 0xff],
            }],
            POLICY,
        )
        .unwrap();
        if closed {
            c.finish_import(job).unwrap();
        }
    }
    db.create_graph("g").unwrap();
    let mut g = db.graph_mut("g").unwrap();
    g.create_node("n", &["label"], &Metadata::new()).unwrap();
    g.attach("n", "half", 0).unwrap();
    g.attach("n", "full", 0).unwrap();
}

pub(super) fn fingerprint(loaded: &Loaded) -> Vec<String> {
    let mut result: Vec<_> = loaded
        .collections
        .iter()
        .map(|(id, c)| {
            let rows: Vec<_> = c
                .records
                .iter()
                .map(|(&id, r)| {
                    (
                        id,
                        r.vector.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
                        &r.payload,
                        &r.metadata,
                        c.revisions.get(&id),
                        c.expiries.get(&id),
                    )
                })
                .collect();
            format!(
                "{id} {} {:?} {:?} {:?} {:?}",
                c.name,
                c.precision,
                c.settings,
                rows,
                c.vectors.encode().unwrap()
            )
        })
        .collect();
    if let Some(graph) = &loaded.graph {
        crate::property_graph::frame::compacted_bodies(
            graph,
            None,
            loaded.sequence,
            |id, count, body| {
                let revisions =
                    crate::property_graph::frame::kept_revisions(graph.graph(id).unwrap(), None);
                result.push(format!("graph {id:?} {count} {body:?} {revisions:?}"));
                Ok(())
            },
        )
        .unwrap();
    }
    let registry = crate::import::Registry::from_markers(&loaded.imports);
    result.push(format!("imports {:?}", registry.encode_image().unwrap()));
    result
}

pub(super) fn snapshot(db: &mut Database, key: Option<&EncryptionKey>) -> Loaded {
    format::load_with(&mut db.file, key).unwrap()
}

fn assert_visible(db: &Database, loaded: &Loaded) {
    for c in loaded.collections.values() {
        let view = db.collection(&c.name).unwrap();
        for (&id, record) in &c.records {
            let actual = view.get(id).unwrap().unwrap();
            assert_eq!(actual.payload, record.payload);
            assert_eq!(actual.metadata, record.metadata);
            assert_eq!(
                actual
                    .vector
                    .iter()
                    .map(|v| v.to_bits())
                    .collect::<Vec<_>>(),
                record
                    .vector
                    .iter()
                    .map(|v| v.to_bits())
                    .collect::<Vec<_>>(),
            );
            assert_eq!(view.revision(id).unwrap(), c.revisions.get(&id).copied());
            assert_eq!(view.expires_at(id).unwrap(), c.expiries.get(&id).copied());
            assert_eq!(view.record_vectors(id).unwrap(), c.vectors.records.get(&id));
        }
    }
}

pub(super) fn assert_source_blocks(bytes: &[u8], key: Option<&EncryptionKey>) {
    let mut source = Cursor::new(bytes);
    let header = format::read_log_header(&mut source, key).unwrap();
    let mut at = header.start;
    let mut found = false;
    while let Some(frame) = format::next_frame(&mut source, &header, at).unwrap() {
        assert_ne!(frame.operation, format::OP_RECORD_BLOCK, "legacy fallback");
        found |= frame.operation == 16 && frame.id as u16 == payload::KIND;
        at += frame.disk.len() as u64;
    }
    assert!(found, "checkpoint did not carry complete-record blocks");
}

#[test]
fn complete_records_round_trip_all_backings_and_precisions() {
    let _precision = Activation::on();
    let _records = Records::on();
    for backing in [Backing::Memory, Backing::File, Backing::Directory] {
        for encrypted in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("store");
            let key = encrypted.then(key);
            let mut db = create_store(backing, &path, key.as_ref());
            populate(&mut db);
            let before = snapshot(&mut db, key.as_ref());
            assert_visible(&db, &before);
            let expected = fingerprint(&before);
            let image = image_of(&before);
            let checkpointed = load(&image, key.as_ref()).unwrap();
            assert_source_blocks(&image, key.as_ref());
            assert_eq!(fingerprint(&checkpointed), expected);
            assert_eq!(
                (checkpointed.sequence, checkpointed.history_floor),
                (before.sequence, before.sequence)
            );
            if !encrypted {
                let (_, frames) = split(&image);
                assert!(frames.iter().any(|f| is_block(f, payload::KIND)));
                assert!(!frames.iter().any(|f| f[4] == format::OP_RECORD_BLOCK));
            }
            if let Backing::Directory = backing {
                db.checkpoint().unwrap().unwrap();
                assert_eq!(fingerprint(&snapshot(&mut db, key.as_ref())), expected);
                let manifest = std::fs::read_to_string(path.join("MANIFEST")).unwrap();
                let name = manifest
                    .lines()
                    .find_map(|line| line.strip_prefix("file image-"))
                    .unwrap();
                let bytes = std::fs::read(path.join(format!("image-{name}"))).unwrap();
                assert_source_blocks(&bytes, key.as_ref());
            }
            let (_, mut reopened) = cycle(backing, &path, key.as_ref(), db);
            assert_eq!(
                fingerprint(&snapshot(&mut reopened, key.as_ref())),
                expected
            );
            assert_visible(&reopened, &before);
            if let Backing::File = backing {
                let destination = dir.path().join("checkpoint.vlite");
                maintenance::publish_held(&destination, key.as_ref(), |file| {
                    Ok(file.write_all(&image)?)
                })
                .unwrap();
                let checkpoint = open_path(&destination, key.as_ref()).unwrap();
                assert_visible(&checkpoint, &before);
            }
            let reopened_image = Database::open_from_bytes(image, key.as_ref()).unwrap();
            assert_eq!(
                fingerprint(&load(&reopened_image.to_bytes().unwrap(), key.as_ref()).unwrap()),
                expected
            );
        }
    }
}

fn image() -> (Vec<u8>, u64) {
    let mut db = limits::store(5);
    populate(&mut db);
    limits::imaged(&db)
}

fn refusals(bytes: &[u8], floor: u64) {
    for readers in limits::both(bytes, floor) {
        assert!(readers.iter().all(|r| r != "ok"), "{readers:?}");
    }
}

#[test]
fn complete_records_reject_malformed_images_in_every_reader() {
    let _precision = Activation::on();
    let _records = Records::on();
    let (image, floor) = image();
    for readers in limits::both(&image, floor) {
        assert!(readers.iter().all(|result| result == "ok"), "{readers:?}");
    }
    let (header, frames) = split(&image);
    let at = frames
        .iter()
        .position(|f| is_block(f, payload::KIND))
        .unwrap();
    // Every truncation of the body and every complete-frame boundary.
    for end in 0..frames[at].len() - PREFIX_SIZE - FOOTER_SIZE {
        let changed = refit(&frames[at], |f| {
            f.truncate(PREFIX_SIZE + end.min(f.len() - PREFIX_SIZE))
        });
        let mut bad = frames[..frames.len() - 1].to_vec();
        bad[at] = changed;
        refusals(&sealed(&header, &bad), floor);
    }
    for end in 1..frames.len() {
        refusals(&[header.clone(), frames[..end].concat()].concat(), floor);
    }
    let parts = sections(&frames[at]);
    let rebuild = |parts: &[Vec<u8>], count| {
        let refs: Vec<_> = parts.iter().map(Vec::as_slice).collect();
        block_frame(
            floor,
            payload::KIND,
            u16_at(&frames[at], 5),
            0,
            count,
            &refs,
        )
    };
    let count = u32_at(&frames[at], 24);
    let mut cases = vec![
        rebuild(&parts, count + 1),
        rebuild(&parts, 0),
        rebuild(&parts, u32::MAX),
    ];
    for section in [0, 2, 4] {
        for value in [1, u32::MAX] {
            let mut parts = parts.clone();
            parts[section][..4].copy_from_slice(&value.to_le_bytes());
            cases.push(rebuild(&parts, count));
        }
    }
    // Dense IDs duplicate/out of order, revision/expiry/descriptor corruption,
    // and a component that is not finite.
    for (section, byte, value) in [(0, 4, 3), (1, 8, 0), (1, 8, 1), (2, 0, 0)] {
        let mut parts = parts.clone();
        let start = u32_at(&parts[1], 32 + section * 8) as usize;
        parts[1][start + byte] = value;
        cases.push(rebuild(&parts, count));
    }
    let mut nonfinite = parts.clone();
    let start = u32_at(&nonfinite[1], 32 + 5 * 8) as usize;
    nonfinite[1][start..start + 4].copy_from_slice(&f32::INFINITY.to_le_bytes());
    cases.push(rebuild(&nonfinite, count));
    let mut trailing = parts.clone();
    trailing[5].push(0);
    cases.push(rebuild(&trailing, count));
    let mut metadata = parts.clone();
    metadata[5][0] = 0xff;
    cases.push(rebuild(&metadata, count));
    for frame in cases {
        let mut bad = frames[..frames.len() - 1].to_vec();
        bad[at] = frame;
        refusals(&sealed(&header, &bad), floor);
    }
}

#[test]
fn complete_records_are_private_and_authenticated_before_decode() {
    let _precision = Activation::on();
    let (image, floor) = {
        let _records = Records::on();
        image()
    };
    assert_eq!(outcome(load(&image, None)), "unsupported 16");
    {
        let _release = Activation::set(false);
        assert_eq!(outcome(load(&image, None)), "unsupported 32");
    }
    let _records = Records::on();
    let mut sealed = encrypted(&image);
    // Locate the encrypted complete-record frame without decoding its body.
    let mut at = 96;
    while !(sealed[at + 4] == 16 && u64_at(&sealed, at + 16) as u16 == payload::KIND) {
        at += PREFIX_SIZE + u32_at(&sealed, at + 28) as usize + 28 + FOOTER_SIZE;
    }
    sealed[at + PREFIX_SIZE] ^= 1;
    let end = at + PREFIX_SIZE + u32_at(&sealed, at + 28) as usize + 28;
    let crc = format::crc32(&sealed[at..end]);
    sealed[end + 4..end + 8].copy_from_slice(&crc.to_le_bytes());
    assert!(matches!(
        load(&sealed, Some(&key())),
        Err(Error::Corrupt {
            reason: "frame authentication failed",
            ..
        })
    ));
    assert!(load(&image[..image.len() - 1], None).is_err());
    let encrypted = encrypted(&image);
    assert!(load(&encrypted[..encrypted.len() - 1], Some(&key())).is_err());
    assert!(floor > 0);
}

#[test]
fn complete_records_failed_checkpoint_keeps_previous_generation() {
    let _precision = Activation::on();
    let _records = Records::on();
    for key in [None, Some(key())] {
        failed_checkpoint(key.as_ref());
    }
}

fn failed_checkpoint(key: Option<&EncryptionKey>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let mut db = create_store(Backing::Directory, &path, key);
    populate(&mut db);
    db.checkpoint().unwrap().unwrap();
    db.collection_mut("half")
        .unwrap()
        .put(99, &[0.25, 0.5], b"tail")
        .unwrap();
    let expected = fingerprint(&snapshot(&mut db, key));
    let manifest = std::fs::read(path.join("MANIFEST")).unwrap();
    let mut staged = 0;
    {
        let _failure = failpoint::arm(move |stage| {
            staged += usize::from(stage == Stage::Staged);
            if stage == Stage::Staged && staged == 2 {
                return Err(std::io::Error::other("payload checkpoint failure"));
            }
            Ok(())
        });
        assert!(db.checkpoint().is_err());
    }
    let after = std::fs::read_to_string(path.join("MANIFEST")).unwrap();
    let previous = String::from_utf8(manifest).unwrap();
    let image = |manifest: &str| {
        manifest
            .lines()
            .find(|line| line.starts_with("file image-"))
            .unwrap()
            .to_owned()
    };
    assert_eq!(image(&after), image(&previous));
    let (_, mut db) = cycle(Backing::Directory, &path, key, db);
    assert_eq!(fingerprint(&snapshot(&mut db, key)), expected);
    db.collection_mut("half")
        .unwrap()
        .put(100, &[0.5, 0.25], b"retry")
        .unwrap();
    let expected = fingerprint(&snapshot(&mut db, key));
    db.checkpoint().unwrap().unwrap();
    assert_eq!(fingerprint(&snapshot(&mut db, key)), expected);
}

#[test]
fn complete_record_decode_heap_is_bounded_by_block_and_output() {
    peak::counting_on(1, || {
        let _records = Records::on();
        let (image, _) = image();
        let (_, frames) = split(&image);
        let frame = frames.iter().find(|f| is_block(f, payload::KIND)).unwrap();
        let body = &frame[PREFIX_SIZE..frame.len() - FOOTER_SIZE];
        let (rows, used) = peak::measured(|| {
            payload::check(
                u16_at(frame, 5),
                u64_at(frame, 16),
                u32_at(frame, 24),
                body,
                crate::compact::vectors::Layout {
                    dimensions: 2,
                    precision: F32,
                },
                u64_at(frame, 8),
                0,
            )
            .unwrap()
        });
        assert_eq!(rows.len(), 5);
        let held = peak::held(rows);
        assert_eq!(used.retained, held);
        assert!(used.peak <= held + body.len() as isize + 4096, "{used:?}");
    });
}

#[test]
fn complete_records_partition_on_row_and_payload_bounds() {
    let _precision = Activation::on();
    let _records = Records::on();
    for (records, payload_bytes) in [(4097, 0), (3, crate::MAX_PAYLOAD_BYTES)] {
        let mut db = limits::store(0);
        for id in 1..=records {
            db.collection_mut("c")
                .unwrap()
                .put(id, &[0.5, 0.25], &vec![id as u8; payload_bytes])
                .unwrap();
        }
        let before = load(&db.to_bytes().unwrap(), None).unwrap();
        let image = image_of(&before);
        assert_eq!(
            fingerprint(&load(&image, None).unwrap()),
            fingerprint(&before)
        );
        let (header, frames) = split(&image);
        let blocks: Vec<_> = frames
            .iter()
            .filter(|f| is_block(f, payload::KIND))
            .collect();
        assert_eq!(blocks.len(), if records == 4097 { 2 } else { 3 });
        for (partition, frame) in blocks.iter().enumerate() {
            assert_eq!(u64_at(frame, 16) >> 16, partition as u64);
            assert!(u32_at(frame, 28) as usize <= payload::MAX_BODY);
        }
        // A second partition that repeats the first one's rows is corrupt.
        let mut bad = frames[..frames.len() - 1].to_vec();
        let at = bad.iter().position(|f| is_block(f, payload::KIND)).unwrap();
        bad[at + 1] = refit(blocks[0], |f| {
            f[16..24].copy_from_slice(&(u64::from(payload::KIND) | 1 << 16).to_le_bytes());
            // Both the outer object and the embedded dense object's partition.
            f[PREFIX_SIZE + 8..PREFIX_SIZE + 16].copy_from_slice(&(1u64 | 1 << 16).to_le_bytes());
            let dense = u32_at(f, PREFIX_SIZE + 40) as usize;
            f[PREFIX_SIZE + dense + 8..PREFIX_SIZE + dense + 16]
                .copy_from_slice(&(1u64 | 1 << 16).to_le_bytes());
        });
        // Avoid multiplying the large payload by every transport in this guard.
        assert!(load(&sealed(&header, &bad), None).is_err());
    }
}

#[test]
fn complete_record_decode_refuses_impossible_lengths_without_record_allocations() {
    peak::counting_on(1, || {
        let _records = Records::on();
        let (image, floor) = image();
        let (_, frames) = split(&image);
        let frame = frames.iter().find(|f| is_block(f, payload::KIND)).unwrap();
        let mut body = frame[PREFIX_SIZE..frame.len() - FOOTER_SIZE].to_vec();
        let po = u32_at(&body, 32 + 2 * 8) as usize;
        body[po + 4..po + 8].copy_from_slice(&u32::MAX.to_le_bytes());
        let (result, used) = peak::measured(|| {
            payload::check(
                u16_at(frame, 5),
                u64_at(frame, 16),
                u32_at(frame, 24),
                &body,
                crate::compact::vectors::Layout {
                    dimensions: 2,
                    precision: F32,
                },
                floor,
                0,
            )
        });
        assert!(result.is_err());
        assert_eq!(used.retained, 0);
        assert!(
            used.peak <= 2 * 6 * std::mem::size_of::<&[u8]>() as isize,
            "{used:?}"
        );
    });
}

#[test]
fn complete_records_preserve_zero_expiry_without_clock_and_filter_with_clock() {
    let _precision = Activation::on();
    let _records = Records::on();
    let db = limits::store(1);
    let mut before = load(&db.to_bytes().unwrap(), None).unwrap();
    before
        .collections
        .get_mut(&1)
        .unwrap()
        .expiries
        .insert(1, 0);
    let image = image_of(&before);
    let restored = load(&image, None).unwrap();
    assert_eq!(fingerprint(&restored), fingerprint(&before));
    assert_eq!(restored.collections[&1].expiries.get(&1), Some(&0));
    let mut bytes = Vec::new();
    maintenance::write_v5_image(
        &mut bytes,
        &before,
        LINEAGE,
        Some(0),
        None,
        None,
        before.flags & (FLAG_GRAPH | FLAG_PRECISION),
        before.sequence,
    )
    .unwrap();
    let filtered = load(&bytes, None).unwrap();
    assert!(filtered.collections[&1].records.is_empty());
    let (_, frames) = split(&bytes);
    assert!(!frames.iter().any(|f| is_block(f, payload::KIND)));
    let table = frames.iter().find(|f| is_block(f, KIND_TABLE)).unwrap();
    assert!(
        sections(table)[0]
            .as_chunks::<8>()
            .0
            .iter()
            .all(|entry| u16_at(entry, 0) != payload::KIND)
    );
}

#[test]
fn complete_records_clamp_original_revisions_to_checkpoint_floor() {
    let _precision = Activation::on();
    let _records = Records::on();
    let db = limits::store(5);
    let mut before = load(&db.to_bytes().unwrap(), None).unwrap();
    let floor = before.sequence;
    let revisions = BTreeMap::from([(1, 0), (2, u64::MAX), (3, 1), (4, floor - 1), (5, floor)]);
    before.collections.get_mut(&1).unwrap().revisions = revisions.clone();
    let restored = load(&image_of(&before), None).unwrap();
    assert_eq!(
        restored.collections[&1].revisions,
        revisions
            .into_iter()
            .map(|(id, revision)| (id, revision.clamp(1, floor)))
            .collect()
    );
    assert_eq!(restored.sequence, floor);
    assert_eq!(restored.history_floor, floor);
}
