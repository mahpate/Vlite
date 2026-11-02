use super::*;
use crate::EncryptionKey;
use crate::compact::Activation as PrecisionActivation;
use crate::compact::catalog::payload::Activation as RecordsActivation;
use crate::compact::catalog::tests::{source_empty_fixture, source_key, source_view_fixture};
use crate::format::{self, FOOTER_SIZE, OP_COMPACT_BLOCK};
use std::io::Cursor;

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
