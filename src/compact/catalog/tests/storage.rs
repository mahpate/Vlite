//! Storage-owned complete-record images through the ordinary WAL readers
//! and copy-out maintenance paths; neither transport nor maintenance may
//! fall back to legacy primary-record frames or lose the restated state.

use super::payload::{assert_source_blocks, fingerprint, populate, snapshot};
use super::*;
use crate::MetadataValue;
use crate::compact::catalog::payload::{self as records, Activation as Records};

fn complete_image(key: Option<&EncryptionKey>) -> (Vec<u8>, Loaded) {
    let mut db = create_store(Backing::Memory, Path::new("unused"), key);
    populate(&mut db);
    let before = snapshot(&mut db, key);
    let image = image_of(&before);
    assert_source_blocks(&image, key);
    assert_eq!(
        fingerprint(&load(&image, key).unwrap()),
        fingerprint(&before)
    );
    (image, before)
}

/// Every suffix commit depends on collections or graph state from the image.
fn ordinary_suffix(image: &[u8], key: Option<&EncryptionKey>) -> (Vec<u8>, Loaded) {
    let mut db = Database::open_from_bytes(image.to_vec(), key).unwrap();
    let metadata = Metadata::from([("suffix".into(), MetadataValue::Boolean(true))]);
    db.collection_mut("half")
        .unwrap()
        .put_with_expiry(99, &[0.1, -0.0], b"ordinary suffix", &metadata, None)
        .unwrap();
    db.collection_mut("full").unwrap().delete(10).unwrap();
    let mut graph = db.graph_mut("g").unwrap();
    graph.create_node("tail", &["suffix"], &metadata).unwrap();
    graph
        .create_edge(Some("tail-edge"), "n", "next", "tail", &metadata)
        .unwrap();
    graph.attach("tail", "half", 99).unwrap();
    let expected = snapshot(&mut db, key);
    (db.to_bytes().unwrap(), expected)
}

fn readers(bytes: &[u8], key: Option<&EncryptionKey>, floor: u64) -> [String; 3] {
    let cursor = FeedCursor {
        lineage: LINEAGE,
        sequence: floor,
    };
    [
        outcome(load(bytes, key)),
        outcome(crate::changes::read_changes(
            &mut Cursor::new(bytes),
            key,
            None,
            &cursor,
            16,
        )),
        outcome(logops::read_segment(
            &mut Cursor::new(bytes),
            key,
            None,
            floor,
            1 << 20,
        )),
    ]
}

#[test]
fn complete_record_writer_declares_one_critical_source_capability() {
    let _precision = Activation::on();
    let _records = Records::on();
    let (image, before) = complete_image(None);
    let (header, frames) = split(&image);
    let table_at = frames.iter().position(|f| is_block(f, KIND_TABLE)).unwrap();
    let table = &frames[table_at];
    let section = sections(table).remove(0);
    let entries = section.as_chunks::<8>().0;
    let sources: Vec<_> = entries
        .iter()
        .filter(|entry| u16_at(entry.as_slice(), 0) == records::KIND)
        .collect();
    assert_eq!(records::KIND, 0x0101);
    assert_eq!(sources.len(), 1);
    assert_eq!(
        [
            u16_at(sources[0], 0),
            u16_at(sources[0], 2),
            u16_at(sources[0], 4),
            u16_at(sources[0], 6),
        ],
        [0x0101, 1, CRITICAL, 0]
    );
    let owners: Vec<_> = frames
        .iter()
        .filter(|frame| is_block(frame, records::KIND))
        .map(|frame| {
            assert_eq!(u64_at(frame, 8), before.sequence);
            assert_eq!(u16_at(frame, PREFIX_SIZE + 4), CRITICAL);
            u16_at(frame, 5)
        })
        .collect();
    assert_eq!(owners.len(), 2, "one block for each populated collection");
    assert_ne!(owners[0], owners[1]);
    assert!(
        frames.iter().any(|frame| frame[4] == 10),
        "vector histories"
    );
    assert!(frames.iter().any(|frame| frame[4] == 15), "graph snapshot");

    let changed = |section: &[u8]| {
        let mut changed = frames[..frames.len() - 1].to_vec();
        changed[table_at] = block_frame(
            before.sequence,
            KIND_TABLE,
            0,
            0,
            (section.len() / 8) as u32,
            &[section],
        );
        sealed(&header, &changed)
    };
    let missing: Vec<_> = entries
        .iter()
        .filter(|entry| u16_at(entry.as_slice(), 0) != records::KIND)
        .flat_map(|entry| entry.iter().copied())
        .collect();
    let source_at = section
        .as_chunks::<8>()
        .0
        .iter()
        .position(|entry| u16_at(entry, 0) == records::KIND)
        .unwrap()
        * 8;
    let mut noncritical = section.clone();
    noncritical[source_at + 4..source_at + 6].copy_from_slice(&0u16.to_le_bytes());
    let mut future = section.clone();
    future[source_at + 2..source_at + 4].copy_from_slice(&2u16.to_le_bytes());
    for (bytes, expected) in [
        (image, "ok"),
        (changed(&missing), "corrupt"),
        (changed(&noncritical), "corrupt"),
        (changed(&future), "unsupported 16"),
    ] {
        assert_eq!(
            readers(&bytes, None, before.sequence),
            std::array::from_fn(|_| expected.to_owned())
        );
        assert_eq!(
            readers(&encrypted(&bytes), Some(&key()), before.sequence),
            std::array::from_fn(|_| expected.to_owned())
        );
    }
}

#[test]
fn complete_record_image_and_ordinary_suffix_replay_feed_export_and_replicate() {
    let _precision = Activation::on();
    let _records = Records::on();
    for key in [None, Some(key())] {
        let key = key.as_ref();
        let directory = tempfile::Builder::new()
            .prefix("p3a-storage-")
            .tempdir_in(".")
            .unwrap();
        let primary = directory.path().join("primary.vlite");
        let replica = directory.path().join("replica.vlite");
        let (image, before) = complete_image(key);
        let floor = before.sequence;
        std::fs::write(&primary, &image).unwrap();
        match key {
            Some(key) => maintenance::backup_with_key(&primary, &replica, key),
            None => maintenance::backup(&primary, &replica),
        }
        .unwrap();
        let (bytes, expected) = ordinary_suffix(&image, key);
        assert_eq!(expected.sequence, floor + 5);
        assert_eq!(expected.history_floor, floor);
        assert_ne!(fingerprint(&expected), fingerprint(&before));
        std::fs::write(&primary, &bytes).unwrap();
        assert_source_blocks(&bytes, key);
        assert_eq!(fingerprint(&stored(&primary, key)), fingerprint(&expected));

        let db = open_path(&primary, key).unwrap();
        let cursor = FeedCursor {
            lineage: LINEAGE,
            sequence: floor,
        };
        let batch = db.changes_after(&cursor, 16).unwrap();
        assert_eq!(batch.changes.len(), 5, "image rows are not feed events");
        assert_eq!(batch.head, expected.sequence);
        assert_eq!(batch.next.sequence, expected.sequence);
        assert!(!batch.has_more);
        assert_eq!(
            batch.changes.iter().map(|c| c.sequence).collect::<Vec<_>>(),
            (floor + 1..=expected.sequence).collect::<Vec<_>>()
        );
        match &batch.changes[0].kind {
            ChangeKind::Put {
                id,
                record,
                expires_at_ms,
            } => {
                assert_eq!((*id, *expires_at_ms), (99, None));
                assert_eq!(batch.changes[0].collection, "half");
                let half = expected
                    .collections
                    .values()
                    .find(|c| c.name == "half")
                    .unwrap();
                let row = &half.records[&99];
                assert_eq!(record.payload, row.payload);
                assert_eq!(record.metadata, row.metadata);
                assert_eq!(
                    record
                        .vector
                        .iter()
                        .map(|v| v.to_bits())
                        .collect::<Vec<_>>(),
                    row.vector.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
                );
            }
            other => panic!("expected suffix put, got {other:?}"),
        }
        assert_eq!(batch.changes[1].collection, "full");
        assert!(matches!(
            batch.changes[1].kind,
            ChangeKind::Delete { id: 10 }
        ));
        for change in &batch.changes[2..] {
            assert!(matches!(&change.kind, ChangeKind::GraphBatch { graph, .. } if graph == "g"));
        }
        let first = db.changes_after(&cursor, 2).unwrap();
        assert_eq!(first.changes, batch.changes[..2]);
        assert!(first.has_more);
        let rest = db.changes_after(&first.next, 16).unwrap();
        assert_eq!(rest.changes, batch.changes[2..]);
        assert!(!rest.has_more);
        assert!(
            db.changes_after(&batch.next, 16)
                .unwrap()
                .changes
                .is_empty()
        );

        let segment = db.export_log(floor, 1 << 20).unwrap();
        assert_eq!(
            (segment.base_sequence, segment.last_sequence),
            (floor, expected.sequence)
        );
        assert_eq!(
            crate::changes::read_changes(&mut Cursor::new(&bytes), key, None, &cursor, 16)
                .unwrap()
                .0,
            batch
        );
        let independent = logops::read_segment(&mut Cursor::new(&bytes), key, None, floor, 1 << 20)
            .unwrap()
            .0;
        assert_eq!(independent.to_bytes(), segment.to_bytes());
        db.close().unwrap();
        assert_eq!(
            logops::export_log_segment(&primary, key, floor, 1 << 20)
                .unwrap()
                .to_bytes(),
            segment.to_bytes()
        );

        let mut follower = ReplicaFollower::open(&replica, key).unwrap();
        assert_eq!(follower.applied_sequence(), floor);
        let status = follower.apply(&segment).unwrap();
        assert_eq!(
            (status.applied_sequence, status.lag),
            (expected.sequence, 0)
        );
        let mut promoted = follower.promote();
        assert_eq!(
            fingerprint(&snapshot(&mut promoted, key)),
            fingerprint(&expected)
        );
        promoted.close().unwrap();
        assert_eq!(fingerprint(&stored(&replica, key)), fingerprint(&expected));
        assert_eq!(std::fs::read(&replica).unwrap(), bytes);
        assert_eq!(std::fs::read(&primary).unwrap(), bytes, "source changed");
    }
}

#[test]
fn complete_record_backup_restore_and_reencryption_preserve_whole_state() {
    let _precision = Activation::on();
    let _records = Records::on();
    for key in [None, Some(key())] {
        let key = key.as_ref();
        let directory = tempfile::Builder::new()
            .prefix("p3a-storage-")
            .tempdir_in(".")
            .unwrap();
        let source = directory.path().join("source.vlite");
        let destination = |name| directory.path().join(name);
        let (image, before) = complete_image(key);
        let (bytes, expected) = ordinary_suffix(&image, key);
        std::fs::write(&source, &bytes).unwrap();
        let expected_fingerprint = fingerprint(&expected);
        let assert_preserved = |path: &Path, key: Option<&EncryptionKey>, new_lineage: bool| {
            let actual = stored(path, key);
            assert_eq!(fingerprint(&actual), expected_fingerprint);
            assert_eq!(
                (actual.sequence, actual.history_floor),
                (expected.sequence, before.sequence)
            );
            assert_eq!(
                actual.flags & !FLAG_ENCRYPTED,
                expected.flags & !FLAG_ENCRYPTED
            );
            assert_eq!(actual.flags & FLAG_ENCRYPTED != 0, key.is_some());
            assert_eq!(actual.lineage != expected.lineage, new_lineage);
            assert_source_blocks(&std::fs::read(path).unwrap(), key);
        };
        match key {
            Some(key) => maintenance::backup_with_key(&source, destination("backup"), key),
            None => maintenance::backup(&source, destination("backup")),
        }
        .unwrap();
        assert_preserved(&destination("backup"), key, false);
        assert_eq!(std::fs::read(destination("backup")).unwrap(), bytes);
        match key {
            Some(key) => {
                maintenance::restore_with_key(destination("backup"), destination("restored"), key)
            }
            None => maintenance::restore(destination("backup"), destination("restored")),
        }
        .unwrap();
        assert_preserved(&destination("restored"), key, true);

        let other = EncryptionKey::from_bytes([9; 32]);
        logops::reencrypt(&source, key, destination("rekeyed"), Some(&other)).unwrap();
        assert_preserved(&destination("rekeyed"), Some(&other), true);
        logops::reencrypt(&source, key, destination("plain"), None).unwrap();
        assert_preserved(&destination("plain"), None, true);
        logops::reencrypt(
            destination("rekeyed"),
            Some(&other),
            destination("again"),
            key,
        )
        .unwrap();
        assert_preserved(&destination("again"), key, true);

        match key {
            Some(key) => {
                maintenance::inspect_with_key(&source, key).unwrap();
                maintenance::verify_with_key(&source, key).unwrap();
            }
            None => {
                maintenance::inspect(&source).unwrap();
                maintenance::verify(&source).unwrap();
            }
        }
        assert_eq!(std::fs::read(&source).unwrap(), bytes, "source changed");
        assert_eq!(std::fs::read(destination("backup")).unwrap(), bytes);
    }
}

#[test]
fn complete_record_point_in_time_restore_keeps_image_and_selected_suffix() {
    let _precision = Activation::on();
    let _records = Records::on();
    for key in [None, Some(key())] {
        let key = key.as_ref();
        let directory = tempfile::Builder::new()
            .prefix("p3a-storage-")
            .tempdir_in(".")
            .unwrap();
        let source = directory.path().join("source.vlite");
        let base = directory.path().join("base.vlite");
        let (image, before) = complete_image(key);
        let (bytes, head) = ordinary_suffix(&image, key);
        std::fs::write(&base, &image).unwrap();
        std::fs::write(&source, &bytes).unwrap();
        // A middle cut keeps the put and delete but none of the graph suffix.
        let mut input = Cursor::new(&bytes);
        let header = format::read_log_header(&mut input, key).unwrap();
        let middle_sequence = before.sequence + 2;
        let mut at = header.start;
        let middle_end = loop {
            let frame = format::next_frame(&mut input, &header, at)
                .unwrap()
                .unwrap();
            at += frame.disk.len() as u64;
            if frame.sequence == middle_sequence {
                break at as usize;
            }
        };
        let middle = load(&bytes[..middle_end], key).unwrap();
        assert_ne!(fingerprint(&middle), fingerprint(&before));
        assert_ne!(fingerprint(&middle), fingerprint(&head));
        for (name, expected) in [("floor", &before), ("middle", &middle), ("head", &head)] {
            let destination = directory.path().join(name);
            let report = logops::restore_to(
                Some(&base),
                &source,
                &destination,
                RestoreTarget::Sequence(expected.sequence),
                key,
            )
            .unwrap();
            assert_eq!(report.restored_sequence, expected.sequence);
            assert_eq!(report.source_head, head.sequence);
            let restored = stored(&destination, key);
            assert_eq!(fingerprint(&restored), fingerprint(expected));
            assert_eq!(
                (restored.sequence, restored.history_floor),
                (expected.sequence, before.sequence)
            );
            assert_ne!(restored.lineage, expected.lineage);
            assert_source_blocks(&std::fs::read(&destination).unwrap(), key);
        }
        let refused = directory.path().join("before-floor");
        assert!(matches!(
            logops::restore_to(
                Some(&base),
                &source,
                &refused,
                RestoreTarget::Sequence(before.sequence - 1),
                key,
            ),
            Err(Error::InvalidRestoreTarget(_))
        ));
        assert!(!refused.exists());
        assert_eq!(std::fs::read(&source).unwrap(), bytes);
        assert_eq!(std::fs::read(&base).unwrap(), image);
    }
}
