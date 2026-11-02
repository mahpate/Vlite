use super::*;
use crate::compact::catalog::Precision;
use crate::compact::catalog::payload;
use crate::compact::vectors::{self, Layout, Row};
use crate::crypto::{Cipher, EncryptionKey};
use crate::format::{self, FOOTER_SIZE, PREFIX_SIZE, u32_at};
use crate::metadata::{self, Metadata, MetadataValue};
use crate::records::RecordMap;
use crate::{Error, MAX_METADATA_BYTES, MAX_PAYLOAD_BYTES, Record};
#[cfg(not(target_arch = "wasm32"))]
use std::{fs::File, io::Write};

const OWNER: u16 = 3;
const FLOOR: u64 = 4;
type TestRecord = (u64, u64, Option<u64>, Record);
type CacheState = (usize, u64, Vec<(u64, Vec<u32>, usize, u64)>);

fn bits(values: &[f32]) -> Vec<u32> {
    values.iter().map(|value| value.to_bits()).collect()
}

fn cache_state(cache: &ScratchCache) -> CacheState {
    (
        cache.used_bytes,
        cache.clock,
        cache
            .entries
            .iter()
            .map(|(&key, entry)| {
                (
                    key,
                    bits(&entry.values),
                    entry.values.capacity(),
                    entry.last_used,
                )
            })
            .collect(),
    )
}

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
    let vectors: Vec<_> = ids
        .iter()
        .map(|&id| [((id % 32) + 1) as f32, -0.0, 2.0])
        .collect();
    let rows: Vec<_> = ids
        .iter()
        .enumerate()
        .map(|(index, &id)| Row {
            id,
            revision: index as u64 + 1,
            expiry: (index == 1).then_some(13),
            vector: &vectors[index],
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

fn assert_query<T>(result: Result<T>, reason: &'static str) {
    match result {
        Err(Error::InvalidQuery(actual)) => assert_eq!(actual, reason),
        Err(error) => panic!("expected InvalidQuery({reason}), got {error:?}"),
        Ok(_) => panic!("expected InvalidQuery({reason}), got success"),
    }
}

fn assert_record_corrupt(body: &[u8], reason: &'static str) {
    match RecordColumn::check(
        OWNER,
        u64::from(payload::KIND),
        3,
        body,
        layout(Precision::Float32),
        FLOOR,
        7,
    ) {
        Err(Error::Corrupt {
            offset,
            reason: actual,
        }) => {
            assert_eq!(offset, 7, "corruption offset");
            assert_eq!(actual, reason, "corruption reason");
        }
        Err(error) => panic!("expected Corrupt, got {error:?}"),
        Ok(_) => panic!("expected Corrupt, got success"),
    }
}

#[test]
fn cache_shape() {
    let f32_layout = layout(Precision::Float32);
    let first = dense_body(Precision::Float32, &[1, 2]);
    let dense = DenseColumn::check(OWNER, 0x0100, 2, &first, f32_layout, FLOOR, 0).unwrap();
    let ids: Vec<_> = (10..20).collect();
    let vectors = vec![[9.0, 8.0, 7.0]; ids.len()];
    let rows: Vec<_> = ids
        .iter()
        .zip(&vectors)
        .map(|(&id, vector)| Row {
            id,
            revision: 1,
            expiry: None,
            vector,
        })
        .collect();
    let larger = vectors::encode(OWNER, 0, f32_layout, &rows, 0).unwrap();
    let large = DenseColumn::check(OWNER, 0x0100, 10, &larger, f32_layout, FLOOR, 0).unwrap();
    let (record_body, _) = record_body(Precision::Float32);
    let record = RecordColumn::check(OWNER, 0x0101, 3, &record_body, f32_layout, FLOOR, 0).unwrap();
    let half_body = dense_body(Precision::Float16, &[1, 2]);
    let half = DenseColumn::check(
        OWNER,
        0x0100,
        2,
        &half_body,
        layout(Precision::Float16),
        FLOOR,
        0,
    )
    .unwrap();
    let dimensions = Layout {
        dimensions: 2,
        precision: Precision::Float32,
    };
    let rows = [1, 2, 3].map(|id| Row {
        id,
        revision: 1,
        expiry: None,
        vector: &[1.0, 2.0],
    });
    let same_elements = vectors::encode(OWNER, 0, dimensions, &rows, 0).unwrap();
    let other_dimensions =
        DenseColumn::check(OWNER, 0x0100, 3, &same_elements, dimensions, FLOOR, 0).unwrap();
    let mut cache = ScratchCache::new(1024);
    dense.vector(&mut cache, 7, 0).unwrap();
    dense.vector(&mut cache, 8, 0).unwrap();
    let before = cache_state(&cache);
    let mismatch = "compact cache key has incompatible shape";
    assert_query(large.vector(&mut cache, 7, 5), mismatch);
    assert_eq!(cache_state(&cache), before);
    assert_query(record.vector(&mut cache, 7, 1), mismatch);
    assert_eq!(cache_state(&cache), before);
    assert_query(half.vector(&mut cache, 7, 1), mismatch);
    assert_eq!(cache_state(&cache), before);
    assert_query(other_dimensions.vector(&mut cache, 7, 1), mismatch);
    assert_eq!(cache_state(&cache), before);
}

#[test]
fn cache_boundaries() {
    for precision in [Precision::Float32, Precision::Float16] {
        let body = dense_body(precision, &[1, 2]);
        let column =
            DenseColumn::check(OWNER, 0x0100, 2, &body, layout(precision), FLOOR, 0).unwrap();
        let mut cache = ScratchCache::new(24);
        assert_eq!(
            bits(column.vector(&mut cache, 1, 0).unwrap()),
            bits(&[2.0, -0.0, 2.0])
        );
        assert_eq!(cache.used_bytes(), 24);
        assert_eq!(
            cache.entries[&1].values.capacity() * 4,
            24,
            "retained capacity"
        );
        column.vector(&mut cache, 2, 1).unwrap();
        assert_eq!(cache.keys_lru(), [2]);
        assert_eq!(cache.used_bytes(), 24);
        let mut short = ScratchCache::new(23);
        assert_query(
            column.vector(&mut short, 1, 0),
            "compact scratch budget is smaller than one decoded block",
        );
        assert_eq!(short.used_bytes(), 0);
        cache.clock = u64::MAX;
        let before = cache_state(&cache);
        for key in [2, 3] {
            assert_query(
                column.vector(&mut cache, key, 0),
                "compact cache access counter exhausted",
            );
            assert_eq!(cache_state(&cache), before);
        }
    }
}

#[test]
fn out_of_range() {
    let body = dense_body(Precision::Float32, &[1, 2]);
    let dense = DenseColumn::check(
        OWNER,
        0x0100,
        2,
        &body,
        layout(Precision::Float32),
        FLOOR,
        0,
    )
    .unwrap();
    let (body, _) = record_body(Precision::Float16);
    let record = RecordColumn::check(
        OWNER,
        0x0101,
        3,
        &body,
        layout(Precision::Float16),
        FLOOR,
        0,
    )
    .unwrap();
    let mut cache = ScratchCache::new(1024);
    for row in [dense.len(), usize::MAX] {
        assert_query(
            dense.vector(&mut cache, 1, row),
            "compact column row is out of range",
        );
        assert_eq!(cache.used_bytes(), 0);
    }
    for row in [record.len(), usize::MAX] {
        assert_query(
            record.vector(&mut cache, 2, row),
            "compact column row is out of range",
        );
        assert_eq!(cache.used_bytes(), 0);
    }
}

#[test]
fn directory_shapes() {
    let expected = vec![
        BlockRange {
            first_id: 5,
            last_id: 5,
            rows: 1,
        },
        BlockRange {
            first_id: 6,
            last_id: 9,
            rows: 4,
        },
    ];
    let directory = BlockDirectory::new(expected).unwrap();
    assert_eq!(directory.block_of(5), Some(0));
    assert_eq!(directory.block_of(6), Some(1));
    assert_eq!(directory.block_of(4), None);
    assert_eq!(directory.block_of(10), None);
    for (block, reason) in [
        (
            BlockRange {
                first_id: 5,
                last_id: 5,
                rows: 0,
            },
            "compact column block has zero rows",
        ),
        (
            BlockRange {
                first_id: 6,
                last_id: 5,
                rows: 1,
            },
            "compact column block has reversed ID range",
        ),
        (
            BlockRange {
                first_id: 5,
                last_id: 5,
                rows: 9,
            },
            "compact column row count exceeds ID span",
        ),
    ] {
        match BlockDirectory::new(vec![block]) {
            Err(Error::Corrupt {
                offset,
                reason: actual,
            }) => {
                assert_eq!(offset, 0);
                assert_eq!(actual, reason);
            }
            _ => panic!("expected Corrupt({reason})"),
        }
    }
    let overlapping = vec![
        BlockRange {
            first_id: 0,
            last_id: 6,
            rows: 2,
        },
        BlockRange {
            first_id: 6,
            last_id: 9,
            rows: 2,
        },
    ];
    for ranges in [overlapping.clone(), overlapping.into_iter().rev().collect()] {
        match BlockDirectory::new(ranges) {
            Err(Error::Corrupt { reason, .. }) => {
                assert_eq!(reason, "compact column blocks overlap or are out of order")
            }
            _ => panic!("expected unordered range error"),
        }
    }
    assert!(
        BlockDirectory::new(vec![BlockRange {
            first_id: 0,
            last_id: u64::MAX,
            rows: usize::MAX
        }])
        .is_ok()
    );
    let bodies = [
        dense_body(Precision::Float32, &[5, 6]),
        dense_body(Precision::Float32, &[9, 10]),
    ];
    let columns: Vec<_> = bodies
        .iter()
        .map(|body| {
            DenseColumn::check(OWNER, 0x0100, 2, body, layout(Precision::Float32), FLOOR, 0)
                .unwrap()
        })
        .collect();
    let expected = vec![
        BlockRange {
            first_id: 5,
            last_id: 6,
            rows: 2,
        },
        BlockRange {
            first_id: 9,
            last_id: 10,
            rows: 2,
        },
    ];
    assert_eq!(
        BlockDirectory::from_dense(&columns).unwrap().blocks,
        expected
    );
    let (body, _) = record_body(Precision::Float32);
    let mut left = sections(&body);
    let mut right = sections(&body);
    left[1] = dense_body(Precision::Float32, &[5, 6, 7]);
    right[1] = dense_body(Precision::Float32, &[9, 10, 11]);
    let bodies = [record_rebuild(&body, &left), record_rebuild(&body, &right)];
    let columns: Vec<_> = bodies
        .iter()
        .map(|body| check_record(body, layout(Precision::Float32)).unwrap())
        .collect();
    assert_eq!(
        BlockDirectory::from_records(&columns).unwrap().blocks,
        vec![
            BlockRange {
                first_id: 5,
                last_id: 7,
                rows: 3
            },
            BlockRange {
                first_id: 9,
                last_id: 11,
                rows: 3
            },
        ]
    );
}

#[test]
fn corruption_provenance() {
    let (body, _) = record_body(Precision::Float32);
    let original = sections(&body);
    let invalid = "invalid complete records block";
    for row in 0..3 {
        assert!(
            check_record(
                &record_rebuild(&body, &original),
                layout(Precision::Float32)
            )
            .is_ok()
        );
        let mut parts = original.clone();
        let start = u32_at(&parts[4], row * 4) as usize;
        parts[5][start..start + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_record_corrupt(&record_rebuild(&body, &parts), invalid);
    }
    for defect in 0..2 {
        assert!(
            check_record(
                &record_rebuild(&body, &original),
                layout(Precision::Float32)
            )
            .is_ok()
        );
        let mut parts = original.clone();
        if defect == 0 {
            parts[0][4..8].copy_from_slice(&1u32.to_le_bytes());
        } else {
            parts[0].extend_from_slice(&[0; 4]);
        }
        assert_record_corrupt(&record_rebuild(&body, &parts), invalid);
    }
    let dense_parts = sections(&original[1]);
    assert!(
        check_record(
            &record_rebuild(&body, &original),
            layout(Precision::Float32)
        )
        .is_ok()
    );
    assert!(
        vectors::check(
            OWNER,
            0x0100,
            3,
            &dense_rebuild(&original[1], &dense_parts),
            layout(Precision::Float32),
            FLOOR,
            7
        )
        .is_ok()
    );
    let mut parts = original.clone();
    let mut dense_parts = dense_parts;
    dense_parts[2][..8].copy_from_slice(&0u64.to_le_bytes());
    parts[1] = dense_rebuild(&original[1], &dense_parts);
    assert_record_corrupt(
        &record_rebuild(&body, &parts),
        "dense row revision outside the checkpoint",
    );
}

#[test]
fn offset_limits() {
    let (body, _) = record_body(Precision::Float32);
    let original = sections(&body);
    let invalid = "invalid complete records block";
    for index in [2, 4] {
        for defect in 0..3 {
            assert!(
                check_record(
                    &record_rebuild(&body, &original),
                    layout(Precision::Float32)
                )
                .is_ok()
            );
            let mut parts = original.clone();
            match defect {
                0 => parts[index].extend_from_slice(&[0; 4]),
                1 => parts[index][..4].copy_from_slice(&1u32.to_le_bytes()),
                _ => parts[index + 1].push(0),
            }
            assert_record_corrupt(&record_rebuild(&body, &parts), invalid);
        }
    }
    for (offsets, maximum) in [(2, MAX_PAYLOAD_BYTES), (4, MAX_METADATA_BYTES)] {
        let mut parts = original.clone();
        let data = offsets + 1;
        let old_end = u32_at(&parts[offsets], 4) as usize;
        parts[data].splice(old_end..old_end, vec![0; maximum + 1 - old_end]);
        for row in 1..=3 {
            let old = u32_at(&parts[offsets], row * 4) as usize;
            parts[offsets][row * 4..row * 4 + 4]
                .copy_from_slice(&((old + maximum + 1 - old_end) as u32).to_le_bytes());
        }
        assert_record_corrupt(&record_rebuild(&body, &parts), invalid);
    }
    let max_metadata = metadata::encode(&maximum_metadata());
    let cap = payload::TARGET + MAX_PAYLOAD_BYTES + MAX_METADATA_BYTES;
    assert_eq!(MAX_PAYLOAD_BYTES, 1 << 20);
    let mut parts = original.clone();
    parts[2] = [
        0u32,
        MAX_PAYLOAD_BYTES as u32,
        (2 * MAX_PAYLOAD_BYTES - 8) as u32,
        (2 * MAX_PAYLOAD_BYTES - 8) as u32,
    ]
    .into_iter()
    .flat_map(u32::to_le_bytes)
    .collect();
    parts[3] = vec![0; 2 * MAX_PAYLOAD_BYTES - 8];
    parts[4] = [
        0u32,
        MAX_METADATA_BYTES as u32,
        (MAX_METADATA_BYTES + 4) as u32,
        (MAX_METADATA_BYTES + 8) as u32,
    ]
    .into_iter()
    .flat_map(u32::to_le_bytes)
    .collect();
    parts[5] = max_metadata;
    parts[5].extend_from_slice(&[0; 8]);
    assert_eq!(parts[3].len() + parts[5].len(), cap);
    assert!(check_record(&record_rebuild(&body, &parts), layout(Precision::Float32)).is_ok());
    parts[3].push(0);
    let end = parts[3].len() as u32;
    parts[2][12..16].copy_from_slice(&end.to_le_bytes());
    assert_eq!(parts[3].len() + parts[5].len(), cap + 1);
    assert_record_corrupt(&record_rebuild(&body, &parts), invalid);
}

fn maximum_metadata() -> Metadata {
    let mut metadata = Metadata::new();
    for index in 0..15 {
        metadata.insert(
            format!("k{index:02}"),
            MetadataValue::String("m".repeat(4096)),
        );
    }
    let length = metadata::validate(&metadata).unwrap();
    metadata.insert(
        "z".into(),
        MetadataValue::String("m".repeat(MAX_METADATA_BYTES - length - 8)),
    );
    metadata
}

#[test]
fn independent_oracles() {
    // Independent IEEE binary16 bit patterns, including signed zeros and subnormals.
    let halves = [0x3c00u16, 0xc000, 0x7bff, 0x0001, 0x03ff, 0, 0x8000];
    let expected = [
        0x3f80_0000,
        0xc000_0000,
        0x477f_e000,
        0x3380_0000,
        0x387f_c000,
        0,
        0x8000_0000,
    ];
    let layout = Layout {
        dimensions: 7,
        precision: Precision::Float16,
    };
    let descriptor = vec![7, 0, 0, 0, 2, 0, 0, 0];
    let dense_parts = vec![
        descriptor,
        5u64.to_le_bytes().to_vec(),
        2u64.to_le_bytes().to_vec(),
        0u32.to_le_bytes().to_vec(),
        0u64.to_le_bytes().to_vec(),
        halves.into_iter().flat_map(u16::to_le_bytes).collect(),
    ];
    let dense_body = dense_rebuild(&[], &dense_parts);
    let dense = DenseColumn::check(OWNER, 0x0100, 1, &dense_body, layout, FLOOR, 0).unwrap();
    let legacy_dense = vectors::check(OWNER, 0x0100, 1, &dense_body, layout, FLOOR, 0).unwrap();
    // One payload [aa,bb]; one metadata field "x" = true, encoded by hand.
    let metadata_bytes = vec![1, 0, 0, 0, 1, 0, b'x', 4, 1];
    let record_parts = vec![
        vec![1, 0, 0, 0, 0, 0, 0, 0],
        dense_body.clone(),
        vec![0, 0, 0, 0, 2, 0, 0, 0],
        vec![0xaa, 0xbb],
        vec![0, 0, 0, 0, 9, 0, 0, 0],
        metadata_bytes.clone(),
    ];
    let record_body = record_rebuild(&[], &record_parts);
    let record = RecordColumn::check(OWNER, 0x0101, 1, &record_body, layout, FLOOR, 0).unwrap();
    let legacy = payload::check(OWNER, 0x0101, 1, &record_body, layout, FLOOR, 0).unwrap();
    let mut cache = ScratchCache::new(28);
    assert_eq!(bits(dense.vector(&mut cache, 1, 0).unwrap()), expected);
    assert_eq!(bits(record.vector(&mut cache, 2, 0).unwrap()), expected);
    assert_eq!(bits(&legacy[0].record.vector), expected);
    assert_eq!(bits(&legacy_dense.vector(0).collect::<Vec<_>>()), expected);
    assert_eq!(dense.id(0), legacy_dense.id(0));
    assert_eq!(dense.revision(0), legacy_dense.revision(0));
    assert_eq!(dense.expiry(0), legacy_dense.expiry(0));
    assert_eq!(record.id(0), legacy[0].id);
    assert_eq!(record.revision(0), legacy[0].revision);
    assert_eq!(record.expiry(0), legacy[0].expiry);
    assert_eq!(record.payload(0), [0xaa, 0xbb]);
    assert_eq!(record.payload(0), legacy[0].record.payload);
    assert_eq!(record.metadata(0), metadata_bytes);
    assert_eq!(
        legacy[0].record.metadata,
        Metadata::from([("x".into(), MetadataValue::Boolean(true))])
    );
    assert_eq!(
        metadata::encode(&legacy[0].record.metadata),
        record.metadata(0)
    );
}

#[test]
fn lru_values() {
    let bodies: Vec<_> = [1, 10, 20, 30]
        .into_iter()
        .map(|first| dense_body(Precision::Float32, &[first, first + 1]))
        .collect();
    let columns: Vec<_> = bodies
        .iter()
        .map(|body| {
            DenseColumn::check(OWNER, 0x0100, 2, body, layout(Precision::Float32), FLOOR, 0)
                .unwrap()
        })
        .collect();
    let mut cache = ScratchCache::new(72);
    for (block, row, expected, order) in [
        (0, 0, 2.0, vec![0]),
        (1, 1, 12.0, vec![0, 1]),
        (2, 0, 21.0, vec![0, 1, 2]),
        (0, 1, 3.0, vec![1, 2, 0]),
        (3, 0, 31.0, vec![2, 0, 3]),
        (1, 0, 11.0, vec![0, 3, 1]),
    ] {
        assert_eq!(
            bits(
                columns[block]
                    .vector(&mut cache, block as u64, row)
                    .unwrap()
            ),
            bits(&[expected, -0.0, 2.0])
        );
        assert_eq!(cache.keys_lru(), order);
        assert!(cache.used_bytes() <= 72);
    }
    let large_layout = Layout {
        dimensions: 15,
        precision: Precision::Float32,
    };
    let row = Row {
        id: 100,
        revision: 1,
        expiry: None,
        vector: &[0.0; 15],
    };
    let body = vectors::encode(OWNER, 0, large_layout, &[row], 0).unwrap();
    let column = DenseColumn::check(OWNER, 0x0100, 1, &body, large_layout, FLOOR, 0).unwrap();
    assert_eq!(bits(column.vector(&mut cache, 4, 0).unwrap()), vec![0; 15]);
    assert_eq!(cache.keys_lru(), [4]);
    assert_eq!(cache.used_bytes(), 60);
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
                    .map(|value| value.to_bits())
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
    assert_eq!(
        bits(a.vector(&mut cache, 1, 0).unwrap()),
        bits(&[2.0, -0.0, 2.0])
    );
    assert_eq!(
        bits(b.vector(&mut cache, 2, 0).unwrap()),
        bits(&[11.0, -0.0, 2.0])
    );
    assert_eq!(
        bits(a.vector(&mut cache, 1, 1).unwrap()),
        bits(&[3.0, -0.0, 2.0])
    );
    assert_eq!(cache.keys_lru(), [2, 1]);
    assert_eq!(
        bits(c.vector(&mut cache, 3, 0).unwrap()),
        bits(&[21.0, -0.0, 2.0])
    );
    assert_eq!(cache.keys_lru(), [1, 3]);
    assert_eq!(cache.used_bytes(), 48);
    assert_eq!(
        bits(c.vector(&mut cache, 3, 0).unwrap()),
        bits(&[21.0, -0.0, 2.0])
    );

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
#[cfg(not(target_arch = "wasm32"))]
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
    assert_eq!(
        bits(column.vector(&mut cache, 0, 0).unwrap()),
        bits(&[2.0, -0.0, 2.0])
    );
    assert_eq!(
        bits(column.vector(&mut cache, 0, 1).unwrap()),
        bits(&[3.0, -0.0, 2.0])
    );
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
    let mut version = valid_dense.clone();
    version[2..4].copy_from_slice(&2u16.to_le_bytes());
    assert!(matches!(
        check_dense(0x0100, &version, layout),
        Err(Error::UnsupportedFeature(format::FLAG_COMPACT))
    ));
    let mut flags = valid_dense.clone();
    flags[4..6].copy_from_slice(&0u16.to_le_bytes());
    assert_corrupt(check_dense(0x0100, &flags, layout));
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
    assert!(matches!(
        check_record(&version, layout),
        Err(Error::UnsupportedFeature(format::FLAG_COMPACT))
    ));
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
