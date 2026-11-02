use super::*;
use crate::compact::catalog::Precision;
use crate::compact::catalog::payload;
use crate::compact::vectors::{self, Layout, Row};
use crate::crypto::{Cipher, EncryptionKey};
use crate::format::{self, FOOTER_SIZE, PREFIX_SIZE, u32_at};
use crate::metadata::{self, Metadata, MetadataValue};
use crate::records::RecordMap;
use crate::{Error, MAX_METADATA_BYTES, MAX_PAYLOAD_BYTES, Record};
use std::fs::File;
use std::io::Write;

const OWNER: u16 = 3;
const FLOOR: u64 = 4;
type TestRecord = (u64, u64, Option<u64>, Record);

fn layout(precision: Precision) -> Layout {
    Layout {
        dimensions: 3,
        precision,
    }
}

fn records() -> (Vec<Row<'static>>, Vec<TestRecord>) {
    let vectors = [
        (2, [0.5, -0.0, 1.25], None),
        (10, [2.0, 3.5, -4.0], Some(13)),
        (u64::MAX, [5.0, 6.0, 7.0], Some(20)),
    ];
    let legacy = vectors
        .into_iter()
        .enumerate()
        .map(|(index, (id, vector, expiry))| {
            let metadata = Metadata::from([
                ("index".into(), MetadataValue::Integer(index as i64)),
                ("visible".into(), MetadataValue::Boolean(true)),
            ]);
            (
                id,
                (index as u64) + 1,
                expiry,
                Record {
                    vector: vector.to_vec(),
                    payload: id.to_le_bytes().to_vec(),
                    metadata,
                },
            )
        })
        .collect();
    let rows = vec![
        Row {
            id: 2,
            revision: 1,
            expiry: None,
            vector: &[0.5, -0.0, 1.25],
        },
        Row {
            id: 10,
            revision: 2,
            expiry: Some(13),
            vector: &[2.0, 3.5, -4.0],
        },
        Row {
            id: u64::MAX,
            revision: 3,
            expiry: Some(20),
            vector: &[5.0, 6.0, 7.0],
        },
    ];
    (rows, legacy)
}

fn record_body(precision: Precision) -> (Vec<u8>, RecordMap) {
    let (rows, records) = records();
    let layout = layout(precision);
    let mut legacy = RecordMap::new();
    let encoded_records: Vec<_> = records
        .iter()
        .map(|(id, _, _, record)| {
            legacy.insert(*id, record.clone());
            (
                rows.iter().find(|row| row.id == *id).copied().unwrap(),
                record,
            )
        })
        .collect();
    (
        payload::encode(OWNER, 0, layout, &encoded_records).unwrap(),
        legacy,
    )
}

fn dense_body(precision: Precision, ids: &[u64]) -> Vec<u8> {
    let rows: Vec<_> = ids
        .iter()
        .enumerate()
        .map(|(index, &id)| Row {
            id,
            revision: index as u64 + 1,
            expiry: (index == 1).then_some(13),
            vector: match index {
                0 => &[1.0, -0.0, 2.0],
                1 => &[2.0, -0.0, 2.0],
                _ => &[3.0, -0.0, 2.0],
            },
        })
        .collect();
    vectors::encode(OWNER, 0, layout(precision), &rows, 0).unwrap()
}

fn frame_body(body: &[u8], id: u64, count: u32) -> Vec<u8> {
    let frame = crate::compact::tests::frame(OWNER, id, count, body);
    let key = EncryptionKey::from_bytes([7; 32]);
    let cipher = Cipher::create(&key).unwrap();
    let sealed = cipher.seal_frame(&frame).unwrap();
    let opened = format::verified_frame(&sealed, Some(&cipher), FLOOR, 0).unwrap();
    opened[PREFIX_SIZE..opened.len() - FOOTER_SIZE].to_vec()
}

fn assert_corrupt(result: Result<impl Sized>) {
    assert!(matches!(result, Err(Error::Corrupt { .. })));
}

fn section_bounds(body: &[u8], section: usize) -> std::ops::Range<usize> {
    let table = crate::compact::HEADER_LEN + 8 * section;
    let start = u32_at(body, table) as usize;
    let end = start + u32_at(body, table + 4) as usize;
    start..end
}

fn sections(body: &[u8]) -> Vec<Vec<u8>> {
    let count = u16::from_le_bytes(body[6..8].try_into().unwrap()) as usize;
    (0..count)
        .map(|section| body[section_bounds(body, section)].to_vec())
        .collect()
}

fn record_rebuild(_body: &[u8], sections: &[Vec<u8>]) -> Vec<u8> {
    let refs: Vec<_> = sections.iter().map(Vec::as_slice).collect();
    crate::compact::envelope(payload::KIND, OWNER, 0, &refs, None)
}

fn dense_rebuild(_body: &[u8], sections: &[Vec<u8>]) -> Vec<u8> {
    let refs: Vec<_> = sections.iter().map(Vec::as_slice).collect();
    crate::compact::envelope(vectors::KIND_DENSE, OWNER, 0, &refs, None)
}

fn check_dense<'a>(id: u64, body: &'a [u8], layout: Layout) -> Result<DenseColumn<'a>> {
    DenseColumn::check(OWNER, id, 2, body, layout, FLOOR, 0)
}

fn check_record<'a>(body: &'a [u8], layout: Layout) -> Result<RecordColumn<'a>> {
    RecordColumn::check(OWNER, u64::from(payload::KIND), 3, body, layout, FLOOR, 0)
}

#[test]
fn borrowed_columns_match_legacy_records_for_both_precisions_and_encryption() {
    for precision in [Precision::Float32, Precision::Float16] {
        let layout = layout(precision);
        let (body, legacy) = record_body(precision);
        for encrypted in [false, true] {
            let id = u64::from(payload::KIND);
            let opened = if encrypted {
                frame_body(&body, id, 3)
            } else {
                body.clone()
            };
            let column = RecordColumn::check(OWNER, id, 3, &opened, layout, FLOOR, 0).unwrap();
            let mut cache = ScratchCache::new(1 << 20);
            assert_eq!(column.len(), legacy.len());
            assert_eq!(column.row_of(2), Some(0));
            assert_eq!(column.row_of(10), Some(1));
            assert_eq!(column.row_of(u64::MAX), Some(2));
            assert_eq!(column.row_of(0), None);
            assert_eq!(column.row_of(3), None);
            assert_eq!(column.row_of(u64::MAX - 1), None);
            for row in 0..column.len() {
                let id = column.id(row);
                let expected = legacy.get(&id).unwrap();
                assert_eq!(column.revision(row), row as u64 + 1);
                assert_eq!(column.expiry(row), [None, Some(13), Some(20)][row]);
                assert_eq!(column.payload(row), expected.payload);
                assert_eq!(column.metadata(row), metadata::encode(&expected.metadata));
                assert_eq!(
                    column
                        .vector(&mut cache, u64::from(encrypted), row)
                        .unwrap()
                        .iter()
                        .map(|value| value.to_bits())
                        .collect::<Vec<_>>(),
                    expected
                        .vector
                        .iter()
                        .map(|value| value.to_bits())
                        .collect::<Vec<_>>()
                );
            }
        }
    }
}

#[test]
fn dense_row_search_and_values_match_legacy_records() {
    let (rows, _) = records();
    for precision in [Precision::Float32, Precision::Float16] {
        let (_, legacy) = record_body(precision);
        let source = rows.clone();
        let layout = layout(precision);
        let body = vectors::encode(OWNER, 0, layout, &source, 0).unwrap();
        let column = DenseColumn::check(
            OWNER,
            u64::from(vectors::KIND_DENSE),
            3,
            &body,
            layout,
            FLOOR,
            0,
        )
        .unwrap();
        let mut cache = ScratchCache::new(1 << 20);
        assert_eq!(column.row_of(2), Some(0));
        assert_eq!(column.row_of(u64::MAX), Some(2));
        assert_eq!(column.row_of(0), None);
        assert_eq!(column.row_of(3), None);
        for row in 0..column.len() {
            let id = column.id(row);
            let expected = legacy.get(&id).unwrap();
            assert_eq!(column.revision(row), row as u64 + 1);
            assert_eq!(column.expiry(row), [None, Some(13), Some(20)][row]);
            assert_eq!(
                column
                    .vector(&mut cache, 0, row)
                    .unwrap()
                    .iter()
                    .map(|value| value.to_bits())
                    .collect::<Vec<_>>(),
                expected
                    .vector
                    .iter()
                    .map(|value| { precision.canonical(*value).unwrap().to_bits() })
                    .collect::<Vec<_>>()
            );
        }
    }
}

#[test]
fn row_lookup_and_directory_cover_boundaries_gaps_and_invalid_order() {
    let left = BlockRange {
        first_id: 2,
        last_id: 10,
        rows: 2,
    };
    let right = BlockRange {
        first_id: 20,
        last_id: u64::MAX,
        rows: 2,
    };
    let directory = BlockDirectory::new(vec![left, right]).unwrap();
    assert_eq!(directory.block_of(2), Some(0));
    assert_eq!(directory.block_of(10), Some(0));
    assert_eq!(directory.block_of(11), None);
    assert_eq!(directory.block_of(19), None);
    assert_eq!(directory.block_of(20), Some(1));
    assert_eq!(directory.block_of(u64::MAX), Some(1));
    for invalid in [
        vec![
            left,
            BlockRange {
                first_id: 10,
                last_id: 20,
                rows: 1,
            },
        ],
        vec![right, left],
        vec![BlockRange {
            first_id: 10,
            last_id: 9,
            rows: 1,
        }],
        vec![BlockRange {
            first_id: 1,
            last_id: 2,
            rows: 0,
        }],
    ] {
        assert!(matches!(
            BlockDirectory::new(invalid),
            Err(Error::Corrupt { .. })
        ));
    }
    assert!(
        BlockDirectory::new(Vec::new())
            .unwrap()
            .block_of(0)
            .is_none()
    );
    let first_body = dense_body(Precision::Float32, &[2, 10]);
    let second_body = dense_body(Precision::Float32, &[20, u64::MAX]);
    let columns = [
        DenseColumn::check(
            OWNER,
            0x0100,
            2,
            &first_body,
            Layout {
                dimensions: 3,
                precision: Precision::Float32,
            },
            FLOOR,
            0,
        )
        .unwrap(),
        DenseColumn::check(
            OWNER,
            0x0100,
            2,
            &second_body,
            Layout {
                dimensions: 3,
                precision: Precision::Float32,
            },
            FLOOR,
            0,
        )
        .unwrap(),
    ];
    assert_eq!(
        BlockDirectory::from_dense(&columns)
            .unwrap()
            .block_of(u64::MAX),
        Some(1)
    );
    let (record_block, _) = record_body(Precision::Float32);
    let records = [RecordColumn::check(
        OWNER,
        u64::from(payload::KIND),
        3,
        &record_block,
        Layout {
            dimensions: 3,
            precision: Precision::Float32,
        },
        FLOOR,
        0,
    )
    .unwrap()];
    assert_eq!(
        BlockDirectory::from_records(&records).unwrap().block_of(10),
        Some(0)
    );
}

#[test]
fn scratch_budget_is_bounded_and_lru_eviction_is_deterministic() {
    let layout = Layout {
        dimensions: 3,
        precision: Precision::Float32,
    };
    let a_body = dense_body(layout.precision, &[1, 2]);
    let b_body = dense_body(layout.precision, &[10, 11]);
    let c_body = dense_body(layout.precision, &[20, 21]);
    let a = DenseColumn::check(OWNER, 0x0100, 2, &a_body, layout, FLOOR, 0).unwrap();
    let b = DenseColumn::check(OWNER, 0x0100, 2, &b_body, layout, FLOOR, 0).unwrap();
    let c = DenseColumn::check(OWNER, 0x0100, 2, &c_body, layout, FLOOR, 0).unwrap();
    let mut cache = ScratchCache::new(48);
    assert_eq!(a.vector(&mut cache, 1, 0).unwrap(), &[1.0, -0.0, 2.0]);
    assert_eq!(b.vector(&mut cache, 2, 0).unwrap(), &[1.0, -0.0, 2.0]);
    assert_eq!(a.vector(&mut cache, 1, 1).unwrap(), &[2.0, -0.0, 2.0]);
    assert_eq!(cache.keys_lru(), [2, 1]);
    assert_eq!(c.vector(&mut cache, 3, 0).unwrap(), &[1.0, -0.0, 2.0]);
    assert_eq!(cache.keys_lru(), [1, 3]);
    assert_eq!(cache.used_bytes(), 48);
    assert_eq!(c.vector(&mut cache, 3, 0).unwrap(), &[1.0, -0.0, 2.0]);

    let mut too_small = ScratchCache::new(23);
    assert!(matches!(
        a.vector(&mut too_small, 1, 0),
        Err(Error::InvalidQuery(
            "compact scratch budget is smaller than one decoded block"
        ))
    ));
    assert_eq!(too_small.used_bytes(), 0);
}

#[test]
fn owned_and_memory_mapped_bodies_have_identical_access() {
    let layout = layout(Precision::Float32);
    let (owned, legacy) = record_body(layout.precision);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("block.bin");
    File::create(&path).unwrap().write_all(&owned).unwrap();
    let file = File::open(path).unwrap();
    // SAFETY: the file is immutable for the mapping's lifetime in this test.
    let mapped = unsafe { memmap2::MmapOptions::new().map(&file) }.unwrap();
    let id = u64::from(payload::KIND);
    let owned_column = RecordColumn::check(OWNER, id, 3, &owned, layout, FLOOR, 0).unwrap();
    let mapped_column = RecordColumn::check(OWNER, id, 3, &mapped, layout, FLOOR, 0).unwrap();
    let (mut owned_cache, mut mapped_cache) =
        (ScratchCache::new(1 << 20), ScratchCache::new(1 << 20));
    for row in 0..3 {
        let record = legacy.get(&owned_column.id(row)).unwrap();
        assert_eq!(owned_column.payload(row), mapped_column.payload(row));
        assert_eq!(owned_column.metadata(row), mapped_column.metadata(row));
        assert_eq!(
            owned_column.vector(&mut owned_cache, 0, row).unwrap(),
            mapped_column.vector(&mut mapped_cache, 0, row).unwrap()
        );
        assert_eq!(owned_column.payload(row), record.payload);
        assert_eq!(
            owned_column.metadata(row),
            metadata::encode(&record.metadata)
        );
    }
}

#[test]
fn float32_body_at_misaligned_address_uses_identical_scratch_values() {
    let layout = layout(Precision::Float32);
    let body = dense_body(layout.precision, &[1, 2]);
    let mut padded = vec![0; body.len() + 4];
    let start = (0..4)
        .find(|&offset| !(padded.as_ptr() as usize + offset).is_multiple_of(4))
        .unwrap();
    padded[start..start + body.len()].copy_from_slice(&body);
    let misaligned = &padded[start..start + body.len()];
    assert_ne!((misaligned.as_ptr() as usize) % 4, 0);
    let column = DenseColumn::check(OWNER, 0x0100, 2, misaligned, layout, FLOOR, 0).unwrap();
    let mut cache = ScratchCache::new(1024);
    assert_eq!(column.vector(&mut cache, 0, 0).unwrap(), &[1.0, -0.0, 2.0]);
    assert_eq!(column.vector(&mut cache, 0, 1).unwrap(), &[2.0, -0.0, 2.0]);
}

#[test]
fn empty_and_maximum_payload_and_metadata_are_borrowed_exactly() {
    let layout = Layout {
        dimensions: 1,
        precision: Precision::Float32,
    };
    let rows = [
        Row {
            id: 1,
            revision: 1,
            expiry: None,
            vector: &[1.0],
        },
        Row {
            id: 2,
            revision: 2,
            expiry: Some(0),
            vector: &[2.0],
        },
    ];
    let empty = Record {
        vector: vec![1.0],
        payload: Vec::new(),
        metadata: Metadata::new(),
    };
    let mut max_metadata = Metadata::new();
    for index in 0..15 {
        max_metadata.insert(
            format!("k{index:02}"),
            MetadataValue::String("m".repeat(4096)),
        );
    }
    let current = metadata::validate(&max_metadata).unwrap();
    let final_value_len = MAX_METADATA_BYTES - current - 8;
    max_metadata.insert(
        "z".into(),
        MetadataValue::String("m".repeat(final_value_len)),
    );
    let max_payload = Record {
        vector: vec![2.0],
        payload: vec![0xA5; MAX_PAYLOAD_BYTES],
        metadata: max_metadata,
    };
    assert_eq!(
        metadata::validate(&max_payload.metadata).unwrap(),
        MAX_METADATA_BYTES
    );
    let body = payload::encode(
        OWNER,
        0,
        layout,
        &[(rows[0], &empty), (rows[1], &max_payload)],
    )
    .unwrap();
    let column =
        RecordColumn::check(OWNER, u64::from(payload::KIND), 2, &body, layout, FLOOR, 0).unwrap();
    assert!(column.payload(0).is_empty());
    assert_eq!(column.metadata(0), metadata::encode(&empty.metadata));
    assert_eq!(column.payload(1), max_payload.payload);
    assert_eq!(column.metadata(1), metadata::encode(&max_payload.metadata));
    assert_eq!(column.expiry(0), None);
    assert_eq!(column.expiry(1), Some(0));
}

#[test]
fn malformed_dense_and_record_blocks_are_corrupt_without_panicking() {
    let layout = layout(Precision::Float32);
    let valid_dense = dense_body(layout.precision, &[2, 10]);
    let mut malformed = valid_dense.clone();
    malformed.truncate(malformed.len() - 1);
    assert_corrupt(check_dense(0x0100, &malformed, layout));
    assert_corrupt(check_dense(0x0101, &valid_dense, layout));
    let mut wrong_kind = valid_dense.clone();
    wrong_kind[..2].copy_from_slice(&0x0101u16.to_le_bytes());
    assert_corrupt(check_dense(0x0100, &wrong_kind, layout));
    for (field, value) in [(2, 2u16), (4, 0u16)] {
        let mut body = valid_dense.clone();
        body[field..field + 2].copy_from_slice(&value.to_le_bytes());
        assert_corrupt(check_dense(0x0100, &body, layout));
    }
    let mut dense_parts = sections(&valid_dense);
    dense_parts[5][..4].copy_from_slice(&f32::NAN.to_le_bytes());
    assert_corrupt(check_dense(
        0x0100,
        &dense_rebuild(&valid_dense, &dense_parts),
        layout,
    ));
    let mut dense_parts = sections(&valid_dense);
    dense_parts[1][8..16].copy_from_slice(&1u64.to_le_bytes());
    assert_corrupt(check_dense(
        0x0100,
        &dense_rebuild(&valid_dense, &dense_parts),
        layout,
    ));
    let mut dense_parts = sections(&valid_dense);
    dense_parts[2][..8].copy_from_slice(&(FLOOR + 1).to_le_bytes());
    assert_corrupt(check_dense(
        0x0100,
        &dense_rebuild(&valid_dense, &dense_parts),
        layout,
    ));
    let mut dense_parts = sections(&valid_dense);
    dense_parts[3] = 2u32.to_le_bytes().to_vec();
    dense_parts[4] = 1u64.to_le_bytes().to_vec();
    assert_corrupt(check_dense(
        0x0100,
        &dense_rebuild(&valid_dense, &dense_parts),
        layout,
    ));

    let (valid_record, _) = record_body(Precision::Float32);
    assert_corrupt(check_record(
        &valid_record[..valid_record.len() - 1],
        layout,
    ));
    assert_corrupt(RecordColumn::check(
        OWNER,
        u64::from(payload::KIND + 1),
        3,
        &valid_record,
        layout,
        FLOOR,
        0,
    ));
    let mut wrong_record_kind = valid_record.clone();
    wrong_record_kind[..2].copy_from_slice(&(payload::KIND + 1).to_le_bytes());
    assert_corrupt(check_record(&wrong_record_kind, layout));
    for offset_section in [2, 4] {
        for defect in 0..3 {
            let mut parts = sections(&valid_record);
            match defect {
                0 => {
                    let mut offsets = parts[offset_section].clone();
                    offsets[4..8].copy_from_slice(
                        &u32_at(&parts[if offset_section == 2 { 3 } else { 5 }], 0).to_le_bytes(),
                    );
                    offsets[8..12].copy_from_slice(&0u32.to_le_bytes());
                    parts[offset_section] = offsets;
                }
                1 => {
                    let data_len = parts[if offset_section == 2 { 3 } else { 5 }].len() as u32;
                    parts[offset_section][8..12].copy_from_slice(&(data_len + 1).to_le_bytes());
                }
                _ => {
                    let len = parts[offset_section].len();
                    parts[offset_section].truncate(len - 4);
                }
            }
            assert_corrupt(check_record(&record_rebuild(&valid_record, &parts), layout));
        }
    }
    let mut version = valid_record.clone();
    version[2..4].copy_from_slice(&2u16.to_le_bytes());
    assert_corrupt(check_record(&version, layout));
    let mut flags = valid_record.clone();
    flags[4..6].copy_from_slice(&0u16.to_le_bytes());
    assert_corrupt(check_record(&flags, layout));
    let mut invalid_metadata = sections(&valid_record);
    invalid_metadata[5][..4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_corrupt(check_record(
        &record_rebuild(&valid_record, &invalid_metadata),
        layout,
    ));
}

#[test]
fn cached_views_are_repeatable_and_record_blocks_validate_nested_rows() {
    let layout = layout(Precision::Float16);
    let (body, legacy) = record_body(Precision::Float16);
    let column =
        RecordColumn::check(OWNER, u64::from(payload::KIND), 3, &body, layout, FLOOR, 0).unwrap();
    let mut cache = ScratchCache::new(1 << 20);
    let first = column.vector(&mut cache, 7, 1).unwrap().to_vec();
    let second = column.vector(&mut cache, 7, 1).unwrap().to_vec();
    assert_eq!(first, second);
    assert_eq!(column.payload(1), legacy.get(&10).unwrap().payload);

    let mut nested = sections(&body);
    let mut dense = sections(&nested[1]);
    dense[1][8..16].copy_from_slice(&2u64.to_le_bytes());
    nested[1] = dense_rebuild(&nested[1], &dense);
    assert_corrupt(check_record(&record_rebuild(&body, &nested), layout));
}
