use super::*;
use crate::EncryptionKey;
use crate::compact::Activation as PrecisionActivation;
use crate::compact::catalog::payload::Activation as RecordsActivation;
use crate::compact::catalog::tests::{source_empty_fixture, source_key, source_view_fixture};
use crate::format::{self, FOOTER_SIZE, OP_COMPACT_BLOCK};
use std::io::Cursor;

#[test]
fn compact_source_activation_defaults_off_and_restores_nested_guards() {
    std::thread::spawn(|| {
        assert!(
            !enabled(),
            "a fresh thread must start with the source gate off"
        );
        let outer = Activation::on();
        assert!(enabled(), "the outer guard must enable the source gate");
        {
            let _inner = Activation::set(false);
            assert!(!enabled(), "the inner guard must disable the source gate");
        }
        assert!(
            enabled(),
            "dropping the disabled inner guard must restore true"
        );
        {
            let _inner = Activation::on();
            assert!(enabled(), "the enabled inner guard must leave the gate on");
        }
        assert!(
            enabled(),
            "dropping the enabled inner guard must restore true"
        );
        drop(outer);
        assert!(!enabled(), "dropping the outer guard must restore false");
    })
    .join()
    .unwrap();
}

struct TestGates {
    _precision: PrecisionActivation,
    _records: RecordsActivation,
    _source: Activation,
}

fn gates() -> TestGates {
    TestGates {
        _precision: PrecisionActivation::on(),
        _records: RecordsActivation::on(),
        _source: Activation::on(),
    }
}

fn tamper_tag(image: &mut [u8], key: &EncryptionKey) {
    let mut cursor = Cursor::new(&*image);
    let header = format::read_log_header(&mut cursor, Some(key)).unwrap();
    let mut offset = header.start;
    loop {
        let frame = format::next_frame(&mut cursor, &header, offset)
            .unwrap()
            .unwrap();
        if frame.operation == OP_COMPACT_BLOCK
            && frame.id as u16 == crate::compact::catalog::payload::KIND
        {
            let start = offset as usize;
            let end = start + frame.disk.len();
            image[end - FOOTER_SIZE - 1] ^= 0x80;
            let checksum = format::crc32(&image[start..end - FOOTER_SIZE]);
            image[end - FOOTER_SIZE + 4..end - FOOTER_SIZE + 8]
                .copy_from_slice(&checksum.to_le_bytes());
            return;
        }
        offset += frame.disk.len() as u64;
    }
}

#[test]
fn compact_generation_authenticates_before_publish() {
    let _gates = gates();
    let key = source_key();
    let (mut image, _) = source_view_fixture(Some(&key));
    tamper_tag(&mut image, &key);

    let result = format::load_with(&mut Cursor::new(image), Some(&key));
    assert!(matches!(
        result,
        Err(Error::Corrupt {
            reason: "frame authentication failed",
            ..
        })
    ));
}

#[test]
fn compact_generation_pins_source_until_last_handle_drop() {
    let _gates = gates();
    let (image, loaded) = source_view_fixture(None);
    let generation = loaded.compact_generation.as_ref().unwrap();
    let weak = Arc::downgrade(generation);
    let collection = loaded
        .collections
        .iter()
        .find_map(|(&id, collection)| (!collection.records.is_empty()).then_some(id))
        .unwrap();
    let view = generation.view(collection);
    drop(loaded);

    assert!(weak.upgrade().is_some());
    assert!(view.get(0, 0).unwrap().is_some());
    drop(view);
    assert!(weak.upgrade().is_none());
    drop(image);
}

#[test]
fn compact_view_get_matches_resident_after_source_drop() {
    let _gates = gates();
    for encrypted in [false, true] {
        let key = encrypted.then(source_key);
        let (_, loaded) = source_view_fixture(key.as_ref());
        let generation = Arc::clone(loaded.compact_generation.as_ref().unwrap());
        let expected: Vec<_> = loaded
            .collections
            .iter()
            .map(|(&collection, source)| {
                let rows = source
                    .records
                    .iter()
                    .map(|(&id, record)| {
                        (
                            id,
                            record.clone(),
                            source.revisions[&id],
                            source.expiries.get(&id).copied(),
                        )
                    })
                    .collect::<Vec<_>>();
                (collection, rows)
            })
            .collect();
        drop(loaded);

        for (collection, rows) in expected {
            let view = generation.view(collection);
            assert_eq!(view.len(), rows.len());
            assert_eq!(view.first_id(), rows.first().map(|row| row.0));
            for (index, (id, record, revision, expiry)) in rows.iter().enumerate() {
                assert_eq!(view.id(index), Some(*id));
                assert_eq!(view.revision(index), Some(*revision));
                assert_eq!(view.expiry(index), Some(*expiry));
                assert_eq!(view.row_of(*id), Some(index));
                assert!(view.contains_id(*id));
                assert_eq!(view.get(*id, 0).unwrap(), Some(record));
                assert_eq!(
                    view.get(*id, 0)
                        .unwrap()
                        .unwrap()
                        .vector
                        .iter()
                        .map(|value| value.to_bits())
                        .collect::<Vec<_>>(),
                    record
                        .vector
                        .iter()
                        .map(|value| value.to_bits())
                        .collect::<Vec<_>>(),
                    "point reads must preserve resident vector component bits"
                );
                if let Some(expiry) = expiry {
                    if *expiry > 0 {
                        assert!(view.get(*id, expiry - 1).unwrap().is_some());
                    }
                    assert_eq!(view.get(*id, *expiry).unwrap(), None);
                }
                assert_eq!(
                    view.get_at(*id, u64::MAX).unwrap().is_none(),
                    expiry.is_some()
                );
            }
            assert_eq!(
                view.get(u64::MAX, 0).unwrap().is_some(),
                rows.iter().any(|r| r.0 == u64::MAX)
            );
            assert_eq!(view.get(1, 0).unwrap(), None);
        }
    }
}

#[test]
fn compact_get_decodes_each_record_once() {
    let _gates = gates();
    let (_, loaded) = source_view_fixture(None);
    let generation = loaded.compact_generation.as_ref().unwrap();
    let collection = loaded
        .collections
        .iter()
        .find_map(|(&id, collection)| (!collection.records.is_empty()).then_some(id))
        .unwrap();
    let view = generation.view(collection);
    let record = view.get(0, 0).unwrap().unwrap();
    assert_eq!(record, view.get(0, 0).unwrap().unwrap());
    assert_eq!(record, view.get(0, 0).unwrap().unwrap());
    assert_eq!(
        generation
            .decode_count
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
}

#[test]
fn compact_generation_rejects_wrong_key() {
    let _gates = gates();
    let key = source_key();
    let wrong = EncryptionKey::from_bytes([0xA7; 32]);
    let (image, _) = source_view_fixture(Some(&key));
    assert!(matches!(
        format::load_with(&mut Cursor::new(image), Some(&wrong)),
        Err(Error::EncryptionKeyMismatch { .. })
    ));
}

#[test]
fn compact_source_gate_off_refuses_compact_blocks() {
    let _gates = gates();
    let (image, _) = source_view_fixture(None);
    drop(_gates);
    let _precision = PrecisionActivation::on();

    assert!(matches!(
        format::load_with(&mut Cursor::new(image), None),
        Err(Error::UnsupportedFeature(format::FLAG_COMPACT))
    ));
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn compact_view_mapped_and_owned_backings_match() {
    let _gates = gates();
    let (_, loaded) = source_view_fixture(None);
    let generation = loaded.compact_generation.as_ref().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mapped = generation
        .mapped_copy(&dir.path().join("mapped-source"))
        .unwrap();
    for collection in loaded.collections.keys() {
        let owned = generation.view(*collection);
        let mapped = mapped.view(*collection);
        assert_eq!(owned.len(), mapped.len());
        for index in 0..owned.len() {
            let id = owned.id(index).unwrap();
            assert_eq!(owned.revision(index), mapped.revision(index));
            assert_eq!(owned.expiry(index), mapped.expiry(index));
            assert_eq!(owned.get(id, 0).unwrap(), mapped.get(id, 0).unwrap());
        }
    }
}

#[test]
fn compact_empty_image_has_an_empty_point_view() {
    let _gates = gates();
    let (_, loaded) = source_empty_fixture(None);
    let view = loaded.compact_generation.as_ref().unwrap().view(0);
    assert_eq!(view.len(), 0);
    assert_eq!(view.first_id(), None);
    assert!(!view.contains_id(0));
    assert_eq!(view.get(0, 0).unwrap(), None);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn compact_multiblock_point_view_matches_resident() {
    let _gates = gates();
    const ROWS: usize = 2 * 4096 + 3;
    for encrypted in [false, true] {
        let key = encrypted.then(source_key);
        let (_, mut input) = source_empty_fixture(key.as_ref());
        input.sequence = ROWS as u64 + 1;
        let collection = input.collections.get_mut(&0).unwrap();
        for row in 0..ROWS {
            collection.put(
                2 * row as u64,
                Record {
                    vector: vec![row as f32, -(row as f32)],
                    payload: (row as u64).to_le_bytes().to_vec(),
                    metadata: Default::default(),
                },
                row as u64 + 1,
                (row % 3 == 0).then_some(100 + row as u64),
            );
        }
        let mut other = format::LoadedCollection::new(
            "disjoint".into(),
            2,
            crate::CollectionSettings::default(),
        );
        other.precision = Some(Precision::Float32);
        for id in [1, 3] {
            other.put(
                id,
                Record {
                    vector: vec![id as f32, 0.0],
                    payload: vec![id as u8],
                    metadata: Default::default(),
                },
                1,
                None,
            );
        }
        input.collections.insert(1, other);
        input.collection_high = 1;
        let mut image = Vec::new();
        crate::maintenance::write_v5_image(
            &mut image,
            &input,
            input.lineage.unwrap(),
            None,
            None,
            input.cipher.as_ref(),
            input.flags & (format::FLAG_GRAPH | format::FLAG_PRECISION),
            input.sequence,
        )
        .unwrap();
        let loaded = format::load_with(&mut Cursor::new(&image), key.as_ref()).unwrap();
        let resident = {
            let _source_off = Activation::set(false);
            format::load_with(&mut Cursor::new(&image), key.as_ref()).unwrap()
        };
        let generation = loaded.compact_generation.as_ref().unwrap();
        assert_eq!(
            generation.collections[&0].blocks.len(),
            3,
            "collection must retain all three physical blocks"
        );
        assert_eq!(
            generation.collections[&0]
                .blocks
                .iter()
                .map(|block| block.source.rows.len())
                .collect::<Vec<_>>(),
            vec![4096, 4096, 3],
            "the normal row limit must produce three physical blocks"
        );
        let dir = tempfile::tempdir().unwrap();
        let mapped = generation
            .mapped_copy(&dir.path().join("multiblock-source"))
            .unwrap();
        for generation in [generation, &mapped] {
            assert_eq!(
                generation.collections[&0].block_starts,
                vec![0, 4096, 8192],
                "global block starts must preserve both 4096-row boundaries"
            );
            let view = generation.view(0);
            let oracle = &resident.collections[&0];
            assert_eq!(view.len(), oracle.records.len(), "physical row count");
            assert_eq!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| view.first_id())).ok(),
                Some(Some(0)),
                "first row must resolve without a block-boundary panic"
            );
            let mut decoded = 0;
            for (row, (&id, record)) in oracle.records.iter().enumerate() {
                let observed =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| view.id(row)));
                assert_eq!(
                    observed.ok(),
                    Some(Some(id)),
                    "global row {row} must resolve to its resident ID"
                );
                assert_eq!(view.row_of(id), Some(row), "global index for ID {id}");
                assert_eq!(view.revision(row), Some(oracle.revisions[&id]));
                let expiry = oracle.expiries.get(&id).copied();
                assert_eq!(view.expiry(row), Some(expiry));
                assert!(view.contains_id(id));
                if ![0, 1, 4095, 4096, 8192, ROWS - 1].contains(&row) {
                    continue;
                }
                let first = view.get(id, 0).unwrap().unwrap();
                assert_eq!(first, record);
                assert!(std::ptr::eq(first, view.get_at(id, 0).unwrap().unwrap()));
                if let Some(deadline) = expiry {
                    assert_eq!(view.get_at(id, deadline - 1).unwrap(), Some(record));
                    assert_eq!(view.get_at(id, deadline).unwrap(), None);
                } else {
                    assert_eq!(view.get_at(id, u64::MAX).unwrap(), Some(record));
                }
                decoded += 1;
                assert_eq!(
                    generation
                        .decode_count
                        .load(std::sync::atomic::Ordering::Relaxed),
                    decoded,
                    "each accessed row must decode once across block boundaries"
                );
            }
            for row in [4095, 4096, 8192, ROWS - 1] {
                assert_eq!(view.id(row), Some(2 * row as u64));
            }
            assert_eq!(view.row_of(1), None);
            assert!(!view.contains_id(1));
            assert_eq!(view.get(1, 0).unwrap(), None);
            assert_eq!(view.get_at(1, u64::MAX).unwrap(), None);
            for row in [ROWS, usize::MAX] {
                assert_eq!(view.id(row), None);
                assert_eq!(view.revision(row), None);
                assert_eq!(view.expiry(row), None);
            }
            let other = generation.view(1);
            let oracle = &resident.collections[&1];
            assert_eq!(other.len(), oracle.records.len());
            for (row, (&id, record)) in oracle.records.iter().enumerate() {
                assert_eq!(other.id(row), Some(id));
                assert_eq!(other.row_of(id), Some(row));
                assert!(other.contains_id(id), "own collection must contain ID {id}");
                assert_eq!(other.get(id, 0).unwrap(), Some(record));
                assert!(
                    !view.contains_id(id),
                    "other collection's ID must stay absent"
                );
                assert_eq!(view.row_of(id), None);
                assert_eq!(view.get(id, 0).unwrap(), None);
            }
            for &id in resident.collections[&0].records.keys() {
                assert!(
                    !other.contains_id(id),
                    "first collection's ID must stay absent"
                );
                assert_eq!(other.row_of(id), None);
                assert_eq!(other.get(id, 0).unwrap(), None);
            }
            assert_eq!(other.id(oracle.records.len()), None);
        }
    }
}

#[test]
fn compact_source_gate_alone_controls_generation_capture() {
    let _gates = gates();
    for encrypted in [false, true] {
        let key = encrypted.then(source_key);
        let (image, _) = source_view_fixture(key.as_ref());
        let resident = {
            let _source_off = Activation::set(false);
            let loaded = format::load_with(&mut Cursor::new(&image), key.as_ref()).unwrap();
            assert!(
                loaded.compact_generation.is_none(),
                "source gate OFF alone must disable generation capture"
            );
            loaded
        };
        let captured = format::load_with(&mut Cursor::new(&image), key.as_ref()).unwrap();
        assert!(
            captured.compact_generation.is_some(),
            "source gate ON must capture a generation"
        );
        for (&id, collection) in &resident.collections {
            assert_eq!(collection.records, captured.collections[&id].records);
        }
    }
}

#[test]
fn compact_plaintext_checksum_change_is_rejected() {
    let _gates = gates();
    let (_, mut loaded) = source_view_fixture(None);
    let mut generation = loaded.compact_generation.take().unwrap();
    let source = Arc::get_mut(&mut generation).unwrap();
    let (&collection, rows) = source.collections.iter().next().unwrap();
    let block = &rows.blocks[0];
    let id = block.source.rows[0].id;
    let checksum_at = block.frame.end - FOOTER_SIZE + 4;
    let Backing::Owned(bytes) = &mut source.backing else {
        unreachable!()
    };
    let original = bytes.to_vec();
    let bytes = Arc::make_mut(bytes);
    bytes[checksum_at..checksum_at + 4].copy_from_slice(&(!block.checksum).to_le_bytes());
    assert_eq!(&bytes[..checksum_at], &original[..checksum_at]);
    assert_eq!(&bytes[checksum_at + 4..], &original[checksum_at + 4..]);
    assert!(
        matches!(
            generation.view(collection).get_at(id, 0),
            Err(Error::Corrupt {
                reason: "compact source frame changed",
                ..
            })
        ),
        "plaintext checksum-only change must be rejected before decoding"
    );
    let key = source_key();
    let (_, mut loaded) = source_view_fixture(Some(&key));
    let mut generation = loaded.compact_generation.take().unwrap();
    let source = Arc::get_mut(&mut generation).unwrap();
    let (&collection, rows) = source.collections.iter().next().unwrap();
    let block = &rows.blocks[0];
    let id = block.source.rows[0].id;
    let byte = block.frame.start + PREFIX_SIZE;
    let Backing::Owned(bytes) = &mut source.backing else {
        unreachable!()
    };
    Arc::make_mut(bytes)[byte] ^= 0x80;
    assert!(
        matches!(
            generation.view(collection).get_at(id, 0),
            Err(Error::Corrupt { .. })
        ),
        "changed ciphertext must never produce a decoded record"
    );
}

#[test]
fn compact_concurrent_first_access_decodes_once_under_lock() {
    let _gates = gates();
    let (_, loaded) = source_view_fixture(None);
    let generation = loaded.compact_generation.as_ref().unwrap();
    let (&collection, source) = generation.collections.iter().next().unwrap();
    let block = &source.blocks[0];
    let row = &block.source.rows[0];
    let cell = &source.cells[0];
    let initializing = cell.initializing.lock().unwrap();
    let barrier = std::sync::Barrier::new(9);
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let barrier = &barrier;
                let view = generation.view(collection);
                scope.spawn(move || {
                    barrier.wait();
                    let record = view.get_at(row.id, 0).unwrap().unwrap();
                    (record.clone(), record as *const Record as usize)
                })
            })
            .collect();
        barrier.wait();
        std::thread::sleep(std::time::Duration::from_millis(100));
        let record = generation.decode(block, row).unwrap();
        cell.record.set(record).unwrap();
        let expected = cell.record.get().unwrap();
        let address = expected as *const Record as usize;
        drop(initializing);
        for worker in workers {
            let (record, observed_address) = worker.join().unwrap();
            assert_eq!(&record, expected);
            assert_eq!(
                observed_address, address,
                "all readers must share the same cell"
            );
        }
    });
    assert_eq!(
        generation
            .decode_count
            .load(std::sync::atomic::Ordering::Relaxed),
        1,
        "waiters must recheck the once-cell after acquiring the initialization lock"
    );
}

#[test]
fn compact_source_extents_match_precision_and_body_bounds() {
    let _gates = gates();
    let key = source_key();
    for encrypted in [false, true] {
        let (_, loaded) = source_view_fixture(encrypted.then_some(&key));
        let generation = loaded.compact_generation.as_ref().unwrap();
        let mut precisions = std::collections::BTreeSet::new();
        for source in generation.collections.values() {
            for block in &source.blocks {
                let disk = &generation.backing.as_slice()[block.frame.clone()];
                let frame = format::verified_frame(
                    disk,
                    generation.cipher.as_ref(),
                    block.sequence,
                    block.offset,
                )
                .unwrap();
                let body_len = frame.len() - PREFIX_SIZE - FOOTER_SIZE;
                let width = match block.source.layout.precision {
                    Precision::Float32 => 4,
                    Precision::Float16 => 2,
                };
                precisions.insert(width);
                for row in &block.source.rows {
                    assert_eq!(
                        row.vector.len(),
                        block.source.layout.dimensions * width,
                        "vector extent must contain exactly dimensions times component width"
                    );
                    for extent in [&row.vector, &row.payload, &row.metadata] {
                        assert!(
                            extent.start <= extent.end && extent.end <= body_len,
                            "every source extent must lie inside the validated body"
                        );
                    }
                }
            }
        }
        assert_eq!(precisions, std::collections::BTreeSet::from([2, 4]));
    }
}

#[test]
fn compact_source_gate_does_not_capture_noncompact_images() {
    let _gates = gates();
    let log = format::header_v5_flags(2, 1, 0, 0, None);
    let loaded = format::load_with(&mut Cursor::new(&log), None).unwrap();
    assert!(
        loaded.compact_generation.is_none(),
        "source gate must not capture an ordinary log"
    );
    let mut checkpoint = Vec::new();
    crate::maintenance::write_v5_image(
        &mut checkpoint,
        &loaded,
        loaded.lineage.unwrap(),
        None,
        None,
        None,
        0,
        1,
    )
    .unwrap();
    let loaded = format::load_with(&mut Cursor::new(checkpoint), None).unwrap();
    assert!(
        loaded.compact_generation.is_none(),
        "source gate must not capture an ordinary checkpoint"
    );
}

#[test]
fn compact_source_rejects_nonzero_section_padding() {
    let kind = crate::compact::catalog::payload::KIND;
    let mut body = crate::compact::envelope(kind, 0, 0, &[&[1], &[2]], Some(0));
    crate::compact::Envelope::parse(0, u64::from(kind), &body, 0).unwrap();
    let first = u32_at(&body, crate::compact::HEADER_LEN) as usize;
    let length = u32_at(&body, crate::compact::HEADER_LEN + 4) as usize;
    let second = u32_at(&body, crate::compact::HEADER_LEN + 8) as usize;
    assert!(
        first + length < second,
        "test must alter only inter-section padding"
    );
    body[first + length] = 1;
    let frame = format::compact_block_frame(1, 0, u64::from(kind), 0, &body);
    let verified = format::verified_frame(&frame, None, 1, 0).unwrap();
    assert!(
        matches!(
            crate::compact::Envelope::parse(
                0,
                u64::from(kind),
                &verified[PREFIX_SIZE..verified.len() - FOOTER_SIZE],
                0
            ),
            Err(Error::Corrupt {
                reason: "invalid compact block sections",
                ..
            })
        ),
        "nonzero inter-section padding must be rejected after reframing"
    );
}
