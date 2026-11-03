use super::dispatch_tests::{compact_dispatch_fixture, gates, point_parity, unsupported};
use super::{Activation, Backing};
use crate::Database;
use crate::compact::catalog::tests::{source_key, source_view_fixture};
use crate::records::CollectionRecords;
use std::io::Cursor;
use std::sync::{Arc, atomic::Ordering};

#[test]
fn compact_install_same_collection_mixed_encodings_refuse_without_losing_rows() {
    use crate::{Metadata, Record, compact, format, maintenance};
    use sha2::{Digest, Sha256};

    let _gates = gates();
    for encrypted in [true, false] {
        let key = encrypted.then(source_key);
        let (_, mut loaded) = compact::catalog::tests::source_empty_fixture(key.as_ref());
        loaded.sequence = 3;
        for (id, revision) in [(10, 1), (20, 2)] {
            loaded.collections.get_mut(&0).unwrap().put(
                id,
                Record {
                    vector: vec![id as f32, 0.0],
                    payload: vec![id as u8],
                    metadata: Metadata::new(),
                },
                revision,
                None,
            );
        }
        let mut image = Vec::new();
        maintenance::write_v5_image(
            &mut image,
            &loaded,
            loaded.lineage.unwrap(),
            None,
            None,
            loaded.cipher.as_ref(),
            loaded.flags & (format::FLAG_GRAPH | format::FLAG_PRECISION),
            loaded.sequence,
        )
        .unwrap();
        let mut reader = Cursor::new(&image);
        let header = format::read_log_header(&mut reader, key.as_ref()).unwrap();
        let mut at = header.start;
        let mut frames = Vec::new();
        let mut inserted = false;
        while let Some(frame) = format::next_frame(&mut reader, &header, at).unwrap() {
            at += frame.disk.len() as u64;
            if frame.operation == format::OP_COMPACT_BLOCK && frame.id as u16 == compact::KIND_END {
                break;
            }
            if frame.operation == format::OP_COMPACT_BLOCK
                && frame.id as u16 == compact::catalog::payload::KIND
                && !inserted
            {
                let mut legacy = format::RecordBlock::new(0);
                legacy.push(
                    1,
                    1,
                    None,
                    &Record {
                        vector: vec![1.0, 0.0],
                        payload: b"legacy row".to_vec(),
                        metadata: Metadata::new(),
                    },
                );
                let plain = legacy.take_frame(loaded.sequence);
                frames.push(match &loaded.cipher {
                    Some(cipher) => cipher.seal_frame(&plain).unwrap(),
                    None => plain,
                });
                inserted = true;
            }
            frames.push(frame.disk);
        }
        assert!(inserted);
        let mut mixed = image[..header.start as usize].to_vec();
        let mut digest = Sha256::new();
        for frame in &frames {
            digest.update(&frame[..format::PREFIX_SIZE]);
            mixed.extend_from_slice(frame);
        }
        let end = compact::end_frame(
            loaded.sequence,
            frames.len() as u64,
            digest.finalize().into(),
        );
        mixed.extend(match &loaded.cipher {
            Some(cipher) => cipher.seal_frame(&end).unwrap(),
            None => end,
        });
        let oracle = {
            let _source_off = Activation::set(false);
            Database::open_from_bytes(mixed.clone(), key.as_ref()).unwrap()
        };
        assert_eq!(oracle.len(), 3);
        assert_eq!(oracle.get_at(1, 0).unwrap().unwrap().payload, b"legacy row");
        for id in [10, 20] {
            assert!(oracle.get_at(id, 0).unwrap().is_some());
        }
        let checked = format::load_with(&mut Cursor::new(&mixed), key.as_ref()).unwrap();
        assert_eq!(
            checked.sequence, checked.history_floor,
            "valid image, not a WAL suffix"
        );
        assert_eq!(checked.collections[&0].records.len(), 3);
        assert_eq!(checked.compact_generation.unwrap().view(0).len(), 2);
        unsupported(
            Database::open_from_bytes(mixed.clone(), key.as_ref()),
            "partial same-collection compact coverage",
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mixed.vlite");
        std::fs::write(&path, &mixed).unwrap();
        let result = if let Some(key) = &key {
            Database::open_encrypted(&path, key)
        } else {
            Database::open(&path)
        };
        unsupported(result, "partial same-collection file coverage");
        assert_eq!(std::fs::read(path).unwrap(), mixed);
        assert_eq!(oracle.to_bytes().unwrap(), mixed);
        assert_eq!(oracle.get_at(1, 0).unwrap().unwrap().payload, b"legacy row");
    }
}

#[test]
fn compact_install_empty_image_with_committed_suffix_stays_resident_and_writable() {
    let _gates = gates();
    for encrypted in [false, true] {
        let key = encrypted.then(source_key);
        let (image, _) = crate::compact::catalog::tests::source_empty_fixture(key.as_ref());
        let mut resident = Database::open_from_bytes(image, key.as_ref()).unwrap();
        resident.put(99, &[1.0, 2.0], b"suffix").unwrap();
        let bytes = resident.to_bytes().unwrap();
        let loaded = crate::format::load_with(&mut Cursor::new(&bytes), key.as_ref()).unwrap();
        assert!(loaded.compact_generation.is_some());
        assert!(loaded.sequence > loaded.history_floor);
        let mut reopened = Database::open_from_bytes(bytes, key.as_ref()).unwrap();
        assert!(matches!(
            reopened.state(0).records,
            CollectionRecords::Resident(_)
        ));
        assert_eq!(
            reopened.get_at(99, 0).unwrap(),
            resident.get_at(99, 0).unwrap()
        );
        reopened.put(100, &[2.0, 3.0], b"still writable").unwrap();
        assert!(reopened.get_at(100, 0).unwrap().is_some());
    }
}

#[test]
fn compact_install_real_open_owns_source_and_snapshot_accounting() {
    let _gates = gates();
    for encrypted in [false, true] {
        let (candidate, oracle, image) = compact_dispatch_fixture(encrypted);
        let info = oracle
            .collections()
            .unwrap()
            .into_iter()
            .find(|c| c.live_records > 0)
            .unwrap();
        let state = candidate.state(info.id);
        let weak = match &state.records {
            CollectionRecords::Resident(_) => panic!("real open must install compact records"),
            CollectionRecords::Compact { view } => {
                assert!(
                    matches!(view.generation.backing, Backing::Owned(_)),
                    "Database installation must own its verified source"
                );
                Arc::downgrade(&view.generation)
            }
        };
        assert_eq!(
            weak.upgrade().unwrap().decode_count.load(Ordering::Relaxed),
            0,
            "installation must not populate lazy Record cells"
        );
        let bytes = state.live_body_bytes;
        let view = candidate.collection(&info.name).unwrap();
        let readers = Arc::clone(&candidate.snapshot_readers);
        let retained = Arc::clone(&candidate.snapshot_retained_bytes);
        let snapshot = view.read_snapshot().unwrap();
        let clone = snapshot.clone();
        assert_eq!(readers.load(Ordering::Relaxed), 1);
        assert_eq!(retained.load(Ordering::Relaxed), bytes);
        let id = view.first_id().unwrap();
        let first = snapshot.get_at(id, 0).unwrap().unwrap() as *const crate::Record;
        assert_eq!(
            weak.upgrade().unwrap().decode_count.load(Ordering::Relaxed),
            1
        );
        let extras = (1..crate::records::MAX_READ_SNAPSHOTS)
            .map(|_| view.read_snapshot().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            readers.load(Ordering::Relaxed),
            crate::records::MAX_READ_SNAPSHOTS
        );
        assert_eq!(
            retained.load(Ordering::Relaxed),
            bytes * crate::records::MAX_READ_SNAPSHOTS as u64
        );
        assert!(matches!(
            view.read_snapshot(),
            Err(crate::Error::InvalidQuery(
                "read snapshot retention budget exhausted"
            ))
        ));
        drop(extras);
        assert_eq!(readers.load(Ordering::Relaxed), 1);
        assert_eq!(retained.load(Ordering::Relaxed), bytes);
        drop(candidate);
        drop(image);
        assert!(
            weak.upgrade().is_some(),
            "snapshot must pin real-open generation after Database drop"
        );
        assert_eq!(
            clone.get_at(id, 0).unwrap().unwrap() as *const crate::Record,
            first
        );
        assert_eq!(
            snapshot.get_at(id, 0).unwrap(),
            oracle
                .collection(&info.name)
                .unwrap()
                .get_at(id, 0)
                .unwrap()
        );
        drop(snapshot);
        assert_eq!(readers.load(Ordering::Relaxed), 1);
        assert_eq!(retained.load(Ordering::Relaxed), bytes);
        drop(clone);
        assert!(
            weak.upgrade().is_none(),
            "last real-open snapshot must release generation"
        );
        assert_eq!(readers.load(Ordering::Relaxed), 0);
        assert_eq!(retained.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn compact_install_real_committed_suffix_refuses_without_changing_source() {
    let _gates = gates();
    for encrypted in [false, true] {
        let key = encrypted.then(source_key);
        let (image, _) = source_view_fixture(key.as_ref());
        let boundary = Database::open_from_bytes(image.clone(), key.as_ref()).unwrap();
        assert!(
            matches!(boundary.state(1).records, CollectionRecords::Compact { .. }),
            "sequence equal to history floor must install"
        );
        drop(boundary);
        let _source_off = Activation::set(false);
        let mut resident = Database::open_from_bytes(image, key.as_ref()).unwrap();
        resident
            .collection_mut("full")
            .unwrap()
            .put(99, &[1.0, 2.0], b"committed suffix")
            .unwrap();
        let bytes = resident.to_bytes().unwrap();
        let sequence = resident.sequence;
        let loaded = crate::format::load_with(&mut Cursor::new(&bytes), key.as_ref()).unwrap();
        assert!(loaded.sequence > loaded.history_floor);
        {
            let _source_on = Activation::on();
            unsupported(
                Database::open_from_bytes(bytes.clone(), key.as_ref()),
                "post-image committed suffix",
            );
        }
        assert_eq!(resident.sequence, sequence);
        assert_eq!(resident.to_bytes().unwrap(), bytes);
        assert_eq!(
            resident
                .collection("full")
                .unwrap()
                .get_at(99, 0)
                .unwrap()
                .unwrap()
                .payload,
            b"committed suffix"
        );
        let reopened = Database::open_from_bytes(bytes, key.as_ref()).unwrap();
        point_parity(&reopened, &resident);
    }
}

#[test]
fn compact_install_late_image_failure_never_publishes_or_changes_file() {
    let _gates = gates();
    for encrypted in [false, true] {
        let key = encrypted.then(source_key);
        let (candidate, oracle, image) = compact_dispatch_fixture(encrypted);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("late.vlite");
        let mut reader = Cursor::new(&image);
        let header = crate::format::read_log_header(&mut reader, key.as_ref()).unwrap();
        let mut offset = header.start;
        let mut last = offset;
        while let Some(frame) = crate::format::next_frame(&mut reader, &header, offset).unwrap() {
            last = offset;
            offset += frame.disk.len() as u64;
        }
        let missing_end = image[..last as usize].to_vec();
        assert!(
            matches!(
                Database::open_from_bytes(missing_end, key.as_ref()),
                Err(crate::Error::Corrupt {
                    reason: "incomplete compact checkpoint image",
                    ..
                })
            ),
            "complete image end must validate before installation"
        );
        for truncate in [false, true] {
            let mut bad = image.clone();
            if truncate {
                bad.truncate(bad.len() - 1);
            } else {
                let last = bad.len() - 1;
                bad[last] ^= 1;
            }

            assert!(
                crate::format::load_with(&mut Cursor::new(&bad), key.as_ref()).is_err(),
                "late image frame must fail before publication"
            );
            assert!(
                Database::open_from_bytes(bad.clone(), key.as_ref()).is_err(),
                "late image failure must not publish a Database"
            );
            std::fs::write(&path, &bad).unwrap();
            let opened = if let Some(key) = &key {
                Database::open_encrypted(&path, key)
            } else {
                Database::open(&path)
            };
            assert!(
                opened.is_err(),
                "late file failure must not publish a Database"
            );
            assert_eq!(
                std::fs::read(&path).unwrap(),
                bad,
                "failed open must not alter source"
            );
            assert_eq!(candidate.to_bytes().unwrap(), image);
            point_parity(&candidate, &oracle);
        }
        std::fs::write(&path, &image).unwrap();
        let opened = if let Some(key) = &key {
            Database::open_encrypted(&path, key)
        } else {
            Database::open(&path)
        }
        .unwrap();
        assert!(opened.to_bytes().is_none());
        point_parity(&opened, &oracle);
        drop(opened);
        let readonly = if let Some(key) = &key {
            Database::open_read_only_encrypted(&path, key)
        } else {
            Database::open_read_only(&path)
        }
        .unwrap();
        assert!(readonly.is_read_only());
        point_parity(&readonly, &oracle);
    }
}

#[test]
fn compact_install_directory_and_segments_refuse_before_cleanup() {
    let _gates = gates();
    for encrypted in [false, true] {
        let key = encrypted.then(source_key);
        let (image, _) = source_view_fixture(key.as_ref());
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("store");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("LOCK"), []).unwrap();
        // A manifest must end with a WAL. A standalone image followed by its
        // empty matching log header is the ordinary directory representation.
        let header_len = crate::format::HEADER_SIZE as usize
            + if encrypted {
                crate::crypto::DESCRIPTOR_SIZE
            } else {
                0
            };
        let mut header = image[..header_len].to_vec();
        let mut flags = u16::from_le_bytes(header[26..28].try_into().unwrap());
        flags &= !(crate::format::FLAG_COMPACT | crate::format::FLAG_HISTORY);
        header[26..28].copy_from_slice(&flags.to_le_bytes());
        let crc = crate::format::crc32(&header[..28]);
        header[28..32].copy_from_slice(&crc.to_le_bytes());
        std::fs::write(root.join("image-1.vlite"), &image).unwrap();
        std::fs::write(root.join("wal-2.log"), &header).unwrap();
        let manifest = "vlite-manifest 1\nfile image-1.vlite\nfile wal-2.log\n";
        std::fs::write(
            root.join("MANIFEST"),
            format!(
                "{manifest}crc {:08x}\n",
                crate::format::crc32(manifest.as_bytes())
            ),
        )
        .unwrap();
        let orphan = root.join("wal-3.log");
        std::fs::write(&orphan, b"must not clean up on refused open").unwrap();
        let opened = if let Some(key) = &key {
            Database::open_encrypted(&root, key)
        } else {
            Database::open(&root)
        };
        unsupported(opened, "directory compact installation");
        assert_eq!(
            std::fs::read(&orphan).unwrap(),
            b"must not clean up on refused open"
        );
        assert_eq!(std::fs::read(root.join("image-1.vlite")).unwrap(), image);
        let opened = if let Some(key) = &key {
            Database::open_read_only_encrypted(&root, key)
        } else {
            Database::open_read_only(&root)
        };
        unsupported(opened, "directory shared compact installation");
    }
}

#[test]
fn compact_install_repair_and_replica_acquisition_refuse() {
    let _gates = gates();
    for encrypted in [false, true] {
        let key = encrypted.then(source_key);
        let (image, _) = source_view_fixture(key.as_ref());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.vlite");
        std::fs::write(&path, &image).unwrap();
        unsupported(
            crate::replica::ReplicaFollower::open(&path, key.as_ref()),
            "replica acquisition",
        );
        assert_eq!(std::fs::read(&path).unwrap(), image);
        let result = if let Some(key) = &key {
            Database::discard_incomplete_tail_encrypted(&path, key)
        } else {
            Database::discard_incomplete_tail(&path)
        };
        unsupported(result, "compact tail repair");
        assert_eq!(std::fs::read(&path).unwrap(), image);
    }
}

#[test]
fn compact_install_encrypted_point_authenticates_before_cell_publication() {
    let _gates = gates();
    let key = source_key();
    let (image, _) = source_view_fixture(Some(&key));
    let wrong = crate::EncryptionKey::from_bytes([82; 32]);
    assert!(matches!(
        Database::open_from_bytes(image.clone(), Some(&wrong)),
        Err(crate::Error::EncryptionKeyMismatch { .. })
    ));
    let mut db = Database::open_from_bytes(image, Some(&key)).unwrap();
    // Re-checksum a private corrupted ciphertext copy so only authentication,
    // not the outer checksum or recorded-footer guard, can reject the read.
    for (&id, state) in &mut db.collections {
        if id != 1 {
            state.records = CollectionRecords::Resident(crate::records::RecordMap::new());
        }
    }
    let state = db.collections.get_mut(&1).unwrap();
    let CollectionRecords::Compact { view } = &mut state.records else {
        panic!("real compact installation required")
    };
    let generation = Arc::get_mut(&mut view.generation).unwrap();
    let source = &generation.collections[&1];
    let frame = &source.blocks[0];
    let id = frame.source.rows[0].id;
    let byte = frame.frame.start + crate::format::PREFIX_SIZE;
    let range = frame.frame.clone();
    let Backing::Owned(bytes) = &mut generation.backing else {
        panic!("owned source required")
    };
    let bytes = Arc::make_mut(bytes);
    bytes[byte] ^= 1;
    let footer = range.end - crate::format::FOOTER_SIZE;
    let checksum = crate::format::crc32(&bytes[range.start..footer]);
    bytes[footer + 4..footer + 8].copy_from_slice(&checksum.to_le_bytes());
    generation.collections.get_mut(&1).unwrap().blocks[0].checksum = checksum;
    for _ in 0..2 {
        assert!(
            matches!(
                db.collection("half").unwrap().get_at(id, 0),
                Err(crate::Error::Corrupt {
                    reason: "frame authentication failed",
                    ..
                })
            ),
            "installed encrypted point must authenticate before publication"
        );
    }
    let CollectionRecords::Compact { view } = &db.state(1).records else {
        unreachable!()
    };
    assert!(view.source().unwrap().cells[0].record.get().is_none());
    assert_eq!(
        view.generation.decode_count.load(Ordering::Relaxed),
        2,
        "authentication failure must not become a cache hit"
    );
}
