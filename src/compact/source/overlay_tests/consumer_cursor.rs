//! The compact record iterator and its cursor seek (`CollectionRecords::compact_iter`).
//!
//! Expectations come from the resident records of the same image (the oracle) and from id lists worked out by hand
//! from the overlay edits of each test, never from the iterator under test.

use super::super::dispatch_tests::{compact_dispatch_fixture, gates};
use super::{assert_compact_resident_pair, indexed_dispatch_fixture};
use crate::compact::catalog::tests::{source_empty_fixture, source_key, source_view_fixture};
use crate::compact::source::CompactGeneration;
use crate::format::FOOTER_SIZE;
use crate::records::{CollectionRecords, MAX_BROWSE_PAGE};
use crate::{BrowsePage, CollectionRef, Database, Error, Metadata, Record};
use std::collections::BTreeSet;
use std::ops::Bound::{Excluded, Unbounded};
use std::sync::Arc;
use std::sync::atomic::Ordering;

const MAX: u64 = u64::MAX;
/// 2100-01-01: the deadline of the fixture's expiring row (ID 3).
const FAR: u64 = 4_102_444_800_000;
const POISONED: &str = "compact record cell lock poisoned";

type Row = (u64, Vec<u32>, Vec<u8>, Metadata);
type Outcome = Result<u64, &'static str>;

fn canonical(id: u64, record: &Record) -> Row {
    (
        id,
        record.vector.iter().map(|value| value.to_bits()).collect(),
        record.payload.clone(),
        record.metadata.clone(),
    )
}

fn ids(rows: &[Row]) -> Vec<u64> {
    rows.iter().map(|row| row.0).collect()
}

fn page_ids(page: &BrowsePage) -> Vec<u64> {
    page.hits.iter().map(|hit| hit.id).collect()
}

fn records<'a>(db: &'a Database, name: &str) -> &'a CollectionRecords {
    &db.state(db.collection(name).unwrap().id()).records
}

/// The generation-wide count of record cells that were decoded.
pub(super) fn decode_count(db: &Database, name: &str) -> usize {
    decode_count_in(db, db.collection(name).unwrap().id())
}

/// `decode_count` for a handle that may be poisoned, which `Database::collection` refuses.
pub(super) fn decode_count_in(db: &Database, collection: u16) -> usize {
    let CollectionRecords::Compact { view, .. } = &db.state(collection).records else {
        panic!("compact source required");
    };
    view.generation.decode_count.load(Ordering::Relaxed)
}

/// Poisons the lock of one base row's cell, so the first read of that row fails as a failed decode would.
pub(super) fn poison_cell(db: &Database, name: &str, id: u64) {
    let CollectionRecords::Compact { view, .. } = records(db, name) else {
        panic!("compact source required");
    };
    let source = view.source().expect("the collection has a source");
    let (block, row) = source.row(id).expect("a base row");
    let cell = &source.cells[source.block_starts[block] + row];
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = cell.initializing.lock().unwrap();
        panic!("poison compact record cell lock");
    }));
}

/// The rows strictly after `after` in the resident records of the oracle.
fn oracle_rows(oracle: &Database, name: &str, after: Option<u64>, limit: usize) -> Vec<Row> {
    records(oracle, name)
        .resident()
        .expect("the oracle holds resident records")
        .range((after.map_or(Unbounded, Excluded), Unbounded))
        .take(limit)
        .map(|(&id, record)| canonical(id, record))
        .collect()
}

fn candidate_rows(db: &Database, name: &str, after: Option<u64>, limit: usize) -> Vec<Row> {
    records(db, name)
        .compact_iter(after)
        .expect("a compact collection has a record iterator")
        .take(limit)
        .map(|item| {
            let (id, record) = item.unwrap();
            canonical(id, record)
        })
        .collect()
}

fn assert_fused(db: &Database, name: &str, after: Option<u64>) {
    let mut scan = records(db, name)
        .compact_iter(after)
        .expect("a compact collection has a record iterator");
    scan.by_ref().for_each(drop);
    for _ in 0..3 {
        assert!(
            scan.next().is_none(),
            "{name}: the iterator after {after:?} must stay exhausted"
        );
    }
}

/// Seeks to no cursor, 0, `u64::MAX - 1`, `u64::MAX` and, for every ID that occurs in the base or the overlay, to the
/// ID and its two neighbours. The effective IDs after each cursor are `effective` filtered by hand.
fn assert_seek_parity(
    candidate: &Database,
    oracle: &Database,
    name: &str,
    occurring: &BTreeSet<u64>,
    effective: &[u64],
) {
    let mut cursors = BTreeSet::from([None, Some(0), Some(MAX - 1), Some(MAX)]);
    for &id in occurring {
        for cursor in [id.checked_sub(1), Some(id), id.checked_add(1)]
            .into_iter()
            .flatten()
        {
            cursors.insert(Some(cursor));
        }
    }
    for after in cursors {
        let want: Vec<u64> = effective
            .iter()
            .copied()
            .filter(|&id| after.is_none_or(|after| id > after))
            .collect();
        let rows = candidate_rows(candidate, name, after, usize::MAX);
        assert_eq!(ids(&rows), want, "{name}: IDs after {after:?}");
        assert_eq!(
            rows,
            oracle_rows(oracle, name, after, usize::MAX),
            "{name}: rows after {after:?}"
        );
        assert_fused(candidate, name, after);
    }
}

/// Edits that interleave with the base rows 0, 3, 10, 20 and `u64::MAX`: the first row replaced, the second and the
/// last-but-one deleted, overlay-only rows between and just below the last, an ID put and deleted again (a tombstone
/// without a base row), and the base rows 10 and `u64::MAX` left alone. Effective IDs afterwards:
/// 0, 1, 5, 10, 15, `u64::MAX - 1`, `u64::MAX`.
fn edit_interleaved_rows(db: &mut Database, name: &str) {
    let mut collection = db.collection_mut(name).unwrap();
    collection.put(0, &[1.0, 2.0], b"replaced zero").unwrap();
    collection.put(1, &[1.0, 1.0], b"one").unwrap();
    assert!(collection.delete(3).unwrap());
    collection.put(5, &[2.0, 2.0], b"five").unwrap();
    collection.put(7, &[0.5, 0.5], b"seven").unwrap();
    assert!(collection.delete(7).unwrap());
    collection.put(15, &[3.0, 3.0], b"fifteen").unwrap();
    assert!(collection.delete(20).unwrap());
    collection
        .put(MAX - 1, &[5.0, 5.0], b"max minus one")
        .unwrap();
}

#[test]
fn compact_iter_seek_matches_resident_at_every_boundary() {
    let _gates = gates();
    for encrypted in [false, true] {
        let (mut candidate, mut oracle, _) = compact_dispatch_fixture(encrypted);
        for name in ["half", "full"] {
            assert_compact_resident_pair(&candidate, &oracle, name);
            assert!(
                records(&oracle, name).compact_iter(None).is_none(),
                "resident records have no compact iterator"
            );
            let mut occurring: BTreeSet<u64> = records(&oracle, name)
                .resident()
                .unwrap()
                .keys()
                .copied()
                .collect();
            assert_eq!(occurring, BTreeSet::from([0, 3, 10, 20, MAX]));
            // Without an overlay the iterator is the base itself.
            assert_seek_parity(&candidate, &oracle, name, &occurring, &[0, 3, 10, 20, MAX]);

            edit_interleaved_rows(&mut candidate, name);
            edit_interleaved_rows(&mut oracle, name);
            occurring.extend([1, 5, 7, 15, MAX - 1]);
            assert_seek_parity(
                &candidate,
                &oracle,
                name,
                &occurring,
                &[0, 1, 5, 10, 15, MAX - 1, MAX],
            );

            let table: [(Option<u64>, Vec<u64>); 15] = [
                (None, vec![0, 1, 5, 10, 15, MAX - 1, MAX]),
                (Some(0), vec![1, 5, 10, 15, MAX - 1, MAX]),
                (Some(1), vec![5, 10, 15, MAX - 1, MAX]),
                // A cursor on a deleted base ID.
                (Some(3), vec![5, 10, 15, MAX - 1, MAX]),
                (Some(4), vec![5, 10, 15, MAX - 1, MAX]),
                (Some(5), vec![10, 15, MAX - 1, MAX]),
                (Some(9), vec![10, 15, MAX - 1, MAX]),
                // A cursor on an untouched base row.
                (Some(10), vec![15, MAX - 1, MAX]),
                (Some(15), vec![MAX - 1, MAX]),
                (Some(19), vec![MAX - 1, MAX]),
                (Some(20), vec![MAX - 1, MAX]),
                (Some(MAX - 2), vec![MAX - 1, MAX]),
                (Some(MAX - 1), vec![MAX]),
                (Some(MAX), vec![]),
                (Some(21), vec![MAX - 1, MAX]),
            ];
            for (after, want) in table {
                assert_eq!(
                    ids(&candidate_rows(&candidate, name, after, usize::MAX)),
                    want,
                    "{name}: IDs after {after:?}"
                );
            }
            // Overlay rows replace the base row of their ID; untouched base rows come from the image.
            let rows = candidate_rows(&candidate, name, None, usize::MAX);
            assert_eq!(rows[0].2, b"replaced zero");
            assert_eq!(rows[1].2, b"one");
            assert_eq!(rows[3].2, 10u64.to_le_bytes());
            assert_eq!(rows[6].2, MAX.to_le_bytes());
        }
    }
}

fn assert_indexed_scenario(
    edit: impl Fn(&mut crate::CollectionMut<'_>),
    edited: &[u64],
    effective: &[u64],
) {
    let (mut candidate, mut oracle) = indexed_dispatch_fixture();
    assert_compact_resident_pair(&candidate, &oracle, "indexed");
    for db in [&mut candidate, &mut oracle] {
        let mut collection = db.collection_mut("indexed").unwrap();
        edit(&mut collection);
    }
    let occurring: BTreeSet<u64> = [1, 2].iter().chain(edited).copied().collect();
    assert_seek_parity(&candidate, &oracle, "indexed", &occurring, effective);
}

#[test]
fn compact_iter_overlay_around_the_base_matches_resident() {
    let _gates = gates();
    // The base rows are 1 and 2: one row below them, the first deleted, the last replaced, rows after the base,
    // a tombstone without a base row and the largest possible ID.
    assert_indexed_scenario(
        |collection| {
            collection.put(0, &[0.0, 1.0], b"below the base").unwrap();
            assert!(collection.delete(1).unwrap());
            collection
                .put(2, &[5.0, 1.0], b"replaced last base row")
                .unwrap();
            collection.put(7, &[6.0, 1.0], b"after the base").unwrap();
            collection.put(8, &[7.0, 1.0], b"put and deleted").unwrap();
            assert!(collection.delete(8).unwrap());
            collection.put(MAX, &[8.0, 1.0], b"the largest ID").unwrap();
        },
        &[0, 1, 2, 7, 8, MAX],
        &[0, 2, 7, MAX],
    );
    // Both base rows deleted: the iterator drains the base through tombstones and goes on with the overlay.
    assert_indexed_scenario(
        |collection| {
            assert!(collection.delete(1).unwrap());
            assert!(collection.delete(2).unwrap());
            collection.put(7, &[6.0, 1.0], b"seven").unwrap();
            collection.put(8, &[7.0, 1.0], b"eight").unwrap();
        },
        &[1, 2, 7, 8],
        &[7, 8],
    );
    // Nothing is left.
    assert_indexed_scenario(
        |collection| {
            assert!(collection.delete(1).unwrap());
            assert!(collection.delete(2).unwrap());
        },
        &[1, 2],
        &[],
    );
    // No overlay at all.
    assert_indexed_scenario(|_| {}, &[], &[1, 2]);
}

const BASE_ROWS: usize = 2 * 4096 + 3;

/// A float32 image of the default collection with `BASE_ROWS` rows (IDs 0, 2, 4, ...) in three blocks.
fn multiblock_image() -> Vec<u8> {
    let (_, mut input) = source_empty_fixture(None);
    input.sequence = BASE_ROWS as u64 + 1;
    let collection = input.collections.get_mut(&0).unwrap();
    for row in 0..BASE_ROWS {
        collection.put(
            2 * row as u64,
            Record {
                vector: vec![row as f32, -(row as f32)],
                payload: (row as u64).to_le_bytes().to_vec(),
                metadata: Metadata::new(),
            },
            row as u64 + 1,
            None,
        );
    }
    let mut image = Vec::new();
    crate::maintenance::write_v5_image(
        &mut image,
        &input,
        input.lineage.unwrap(),
        None,
        None,
        input.cipher.as_ref(),
        input.flags & (crate::format::FLAG_GRAPH | crate::format::FLAG_PRECISION),
        input.sequence,
    )
    .unwrap();
    image
}

#[test]
fn compact_iter_seek_through_several_blocks_matches_resident() {
    let _gates = gates();
    let image = multiblock_image();
    let mut oracle = {
        let _source_off = crate::compact::source::Activation::set(false);
        Database::open_from_bytes(image.clone(), None).unwrap()
    };
    let mut candidate = Database::open_from_bytes(image, None).unwrap();
    assert_compact_resident_pair(&candidate, &oracle, "default");
    let CollectionRecords::Compact { view, .. } = records(&candidate, "default") else {
        panic!("compact source required");
    };
    assert_eq!(
        view.generation.collections[&0].blocks.len(),
        3,
        "the seek must run across three blocks"
    );

    // Ten deletions and overlay-only rows around both block boundaries (rows 4095 and 4096 hold IDs 8190 and 8192),
    // adjacent tombstones, the last base ID (16388) and rows after the base.
    const DELETED: [u64; 5] = [0, 5000, 5002, 8190, 16388];
    const PUT: [u64; 6] = [1, 5001, 8191, 8192, 16389, MAX];
    let base: BTreeSet<u64> = (0..BASE_ROWS as u64).map(|row| 2 * row).collect();
    let mut effective = base.clone();
    for id in DELETED {
        assert!(effective.remove(&id), "{id} is a base ID");
    }
    effective.extend(PUT);
    assert_eq!(
        effective.len(),
        BASE_ROWS,
        "five rows deleted, five overlay-only rows added and one replaced"
    );
    for db in [&mut candidate, &mut oracle] {
        let mut collection = db.collection_mut("default").unwrap();
        for id in DELETED {
            assert!(collection.delete(id).unwrap());
        }
        for id in PUT {
            collection
                .put(id, &[id as f32, 0.0], &id.to_le_bytes())
                .unwrap();
        }
    }

    let mut cursors = BTreeSet::from([None, Some(0), Some(MAX - 1), Some(MAX)]);
    for &id in base.iter().chain(&PUT) {
        for cursor in [id.checked_sub(1), Some(id), id.checked_add(1)]
            .into_iter()
            .flatten()
        {
            cursors.insert(Some(cursor));
        }
    }
    for &after in &cursors {
        let want: Vec<u64> = effective
            .range((after.map_or(Unbounded, Excluded), Unbounded))
            .take(3)
            .copied()
            .collect();
        let rows = candidate_rows(&candidate, "default", after, 3);
        assert_eq!(ids(&rows), want, "IDs after {after:?}");
        assert_eq!(
            rows,
            oracle_rows(&oracle, "default", after, 3),
            "rows after {after:?}"
        );
    }

    for after in [
        None,
        Some(0),
        Some(1),
        Some(4999),
        Some(5001),
        Some(8189),
        Some(8190),
        Some(8191),
        Some(8192),
        Some(8193),
        Some(16387),
        Some(16388),
        Some(16389),
        Some(MAX - 1),
        Some(MAX),
    ] {
        let want: Vec<u64> = effective
            .range((after.map_or(Unbounded, Excluded), Unbounded))
            .copied()
            .collect();
        let rows = candidate_rows(&candidate, "default", after, usize::MAX);
        assert_eq!(ids(&rows), want, "IDs after {after:?}");
        assert_eq!(
            rows,
            oracle_rows(&oracle, "default", after, usize::MAX),
            "rows after {after:?}"
        );
        assert_fused(&candidate, "default", after);
    }

    let table: [(Option<u64>, Vec<u64>); 8] = [
        (None, vec![1, 2, 4]),
        (Some(1), vec![2, 4, 6]),
        (Some(4999), vec![5001, 5004, 5006]),
        (Some(8189), vec![8191, 8192, 8194]),
        (Some(8191), vec![8192, 8194, 8196]),
        (Some(8192), vec![8194, 8196, 8198]),
        (Some(16385), vec![16386, 16389, MAX]),
        (Some(16389), vec![MAX]),
    ];
    for (after, want) in table {
        assert_eq!(
            ids(&candidate_rows(&candidate, "default", after, 3)),
            want,
            "IDs after {after:?}"
        );
    }

    // A page across the first block boundary: ID 8190 is deleted, 8191 lives only in the overlay and 8192 replaces
    // the first row of the second block.
    let actual = candidate
        .collection("default")
        .unwrap()
        .browse_at(Some(8189), 4, 0)
        .unwrap();
    assert_eq!(
        actual,
        oracle
            .collection("default")
            .unwrap()
            .browse_at(Some(8189), 4, 0)
            .unwrap()
    );
    assert_eq!(page_ids(&actual), [8191, 8192, 8194, 8196]);
    assert_eq!(actual.next_after, Some(8196));
    assert_eq!(actual.hits[1].record.payload, 8192u64.to_le_bytes());
    assert_eq!(actual.hits[2].record.payload, 4097u64.to_le_bytes());
}

fn outcomes_of(rows: &CollectionRecords, after: Option<u64>) -> Vec<Outcome> {
    rows.compact_iter(after)
        .expect("a compact collection has a record iterator")
        .map(|item| match item {
            Ok((id, _)) => Ok(id),
            Err(Error::InvalidQuery(reason) | Error::Corrupt { reason, .. }) => Err(reason),
            Err(other) => panic!("unexpected error {other:?}"),
        })
        .collect()
}

#[test]
fn compact_iter_read_errors_are_items_and_browse_returns_them() {
    let _gates = gates();
    let (mut candidate, mut oracle, _) = compact_dispatch_fixture(false);
    for db in [&mut candidate, &mut oracle] {
        let mut collection = db.collection_mut("full").unwrap();
        collection.put(1, &[1.0, 1.0], b"one").unwrap();
        collection.put(15, &[2.0, 2.0], b"fifteen").unwrap();
    }
    assert_eq!(
        decode_count(&candidate, "full"),
        0,
        "writes of new IDs decode no base row"
    );
    // Base rows 10 (followed by overlay entries) and 20 (after the last overlay entry) cannot be read.
    poison_cell(&candidate, "full", 10);
    poison_cell(&candidate, "full", 20);

    let bad: Outcome = Err(POISONED);
    let table: [(Option<u64>, Vec<Outcome>); 7] = [
        (None, vec![Ok(0), Ok(1), Ok(3), bad, Ok(15), bad, Ok(MAX)]),
        (Some(0), vec![Ok(1), Ok(3), bad, Ok(15), bad, Ok(MAX)]),
        (Some(3), vec![bad, Ok(15), bad, Ok(MAX)]),
        // Rows at or before the cursor are never read.
        (Some(10), vec![Ok(15), bad, Ok(MAX)]),
        (Some(15), vec![bad, Ok(MAX)]),
        (Some(20), vec![Ok(MAX)]),
        (Some(MAX), vec![]),
    ];
    for (after, want) in table {
        assert_eq!(
            outcomes_of(records(&candidate, "full"), after),
            want,
            "outcomes after {after:?}"
        );
    }

    let actual = candidate.collection("full").unwrap();
    let healthy = oracle.collection("full").unwrap();
    let poisoned = |result: crate::Result<BrowsePage>| matches!(result, Err(Error::InvalidQuery(reason)) if reason == POISONED);
    // Two rows and the look-ahead row 3 are readable.
    let page = actual.browse_at(None, 2, 0).unwrap();
    assert_eq!(page, healthy.browse_at(None, 2, 0).unwrap());
    assert_eq!(page_ids(&page), [0, 1]);
    assert_eq!(page.next_after, Some(1));
    // A page ends at the look-ahead row, which is the poisoned row 10 here.
    for (after, limit) in [(Some(1), 1), (None, 3), (Some(10), 10), (Some(15), 5)] {
        assert!(
            poisoned(actual.browse_at(after, limit, 0)),
            "browse after {after:?} of {limit} must return the read error"
        );
    }
    let tail = actual.browse_at(Some(20), 10, 0).unwrap();
    assert_eq!(tail, healthy.browse_at(Some(20), 10, 0).unwrap());
    assert_eq!(page_ids(&tail), [MAX]);

    let snapshot = actual.read_snapshot().unwrap();
    assert!(poisoned(snapshot.browse_at(Some(1), 1, 0)));
    assert_eq!(snapshot.browse_at(Some(20), 10, 0).unwrap(), tail);
}

enum Tamper {
    Checksum,
    Ciphertext,
}

fn tampered(
    loaded: &mut crate::format::Loaded,
    collection: u16,
    how: &Tamper,
) -> Arc<CompactGeneration> {
    let mut generation = loaded.compact_generation.take().unwrap();
    let inner = Arc::get_mut(&mut generation).expect("the fixture owns its only handle");
    let block = &mut inner.collections.get_mut(&collection).unwrap().blocks[0];
    match how {
        Tamper::Checksum => block.checksum ^= 1,
        Tamper::Ciphertext => {
            let end = block.frame.end - FOOTER_SIZE;
            let mut bytes = inner.backing.as_slice().to_vec();
            bytes[end - 1] ^= 0x80;
            let checksum = crate::format::crc32(&bytes[block.frame.start..end]);
            bytes[end + 4..end + 8].copy_from_slice(&checksum.to_le_bytes());
            block.checksum = checksum;
            inner.backing = crate::compact::source::Backing::Owned(Arc::from(bytes));
        }
    }
    generation
}

#[test]
fn compact_iter_tampered_blocks_error_only_where_the_base_is_read() {
    let _gates = gates();
    for (encrypted, how, reason) in [
        (false, Tamper::Checksum, "compact source frame changed"),
        (true, Tamper::Ciphertext, "frame authentication failed"),
    ] {
        let key = encrypted.then(source_key);
        let (_, mut loaded) = source_view_fixture(key.as_ref());
        let sequence = loaded.sequence;
        let (full, base) = loaded
            .collections
            .iter()
            .find(|(_, state)| state.name == "full")
            .map(|(&id, state)| (id, state.records.clone()))
            .expect("the fixture has the collection `full`");
        assert_eq!(
            base.keys().copied().collect::<Vec<_>>(),
            [0, 3, 10, 20, MAX]
        );
        let generation = tampered(&mut loaded, full, &how);
        let mut rows = CollectionRecords::Compact {
            view: generation.view(full),
            overlay: Default::default(),
        };
        let put = |tag: u8| Record {
            vector: vec![f32::from(tag), 1.0],
            payload: vec![tag],
            metadata: Metadata::new(),
        };
        // The overlay covers base rows 3 (deleted) and 10 (replaced) and adds 7: none of them reads the base.
        rows.apply_committed(3, None, sequence + 1, None).unwrap();
        rows.apply_committed(7, Some(put(7)), sequence + 2, None)
            .unwrap();
        rows.apply_committed(10, Some(put(10)), sequence + 3, None)
            .unwrap();

        let bad: Outcome = Err(reason);
        let table: [(Option<u64>, Vec<Outcome>); 8] = [
            (None, vec![bad, Ok(7), Ok(10), bad, bad]),
            (Some(0), vec![Ok(7), Ok(10), bad, bad]),
            (Some(3), vec![Ok(7), Ok(10), bad, bad]),
            (Some(7), vec![Ok(10), bad, bad]),
            (Some(10), vec![bad, bad]),
            (Some(19), vec![bad, bad]),
            (Some(20), vec![bad]),
            (Some(MAX), vec![]),
        ];
        // The same table twice: a failed read publishes no cell, so the next read fails again.
        for _ in 0..2 {
            for (after, want) in &table {
                assert_eq!(
                    &outcomes_of(&rows, *after),
                    want,
                    "{reason}: outcomes after {after:?}"
                );
            }
        }
        let served: Vec<(u64, Record)> = rows
            .compact_iter(None)
            .unwrap()
            .filter_map(Result::ok)
            .map(|(id, record)| (id, record.clone()))
            .collect();
        assert_eq!(served, [(7, put(7)), (10, put(10))]);
    }
}

type Page = (Option<u64>, usize, u64, Vec<u64>, Option<u64>);

/// Deadlines afterwards (`-` for none): 0 -, 1 -, 3 `FAR` (base), 5 at 100, 15 at 200, 20 at 300, `MAX - 1` -,
/// `MAX` - (base); ID 10 is deleted.
fn edit_clock_rows(db: &mut Database, name: &str) {
    let mut collection = db.collection_mut(name).unwrap();
    collection.put(1, &[1.0, 1.0], b"one").unwrap();
    collection
        .put_with_expiry(5, &[2.0, 2.0], b"five", &Metadata::new(), Some(100))
        .unwrap();
    assert!(collection.delete(10).unwrap());
    collection
        .put_with_expiry(15, &[3.0, 3.0], b"fifteen", &Metadata::new(), Some(200))
        .unwrap();
    collection
        .put_with_expiry(20, &[4.0, 4.0], b"twenty", &Metadata::new(), Some(300))
        .unwrap();
    collection
        .put(MAX - 1, &[5.0, 5.0], b"max minus one")
        .unwrap();
}

fn chain_with(mut page: impl FnMut(Option<u64>) -> BrowsePage) -> Vec<BrowsePage> {
    let mut pages = Vec::new();
    let mut after = None;
    loop {
        let next = page(after);
        let cursor = next.next_after;
        pages.push(next);
        assert!(pages.len() < 64, "a page chain must end");
        match cursor {
            Some(cursor) => after = Some(cursor),
            None => return pages,
        }
    }
}

fn chain(db: &Database, name: &str, size: usize, now: u64) -> Vec<BrowsePage> {
    chain_with(|after| {
        db.collection(name)
            .unwrap()
            .browse_at(after, size, now)
            .unwrap()
    })
}

/// Walks the pages of every size at every clock and checks them against the hand-derived visible IDs.
fn assert_chain_shape(pages: &[BrowsePage], visible: &[u64], size: usize, what: &str) {
    let seen: Vec<u64> = pages.iter().flat_map(page_ids).collect();
    assert_eq!(seen, visible, "{what}: concatenated pages");
    assert_eq!(pages.len(), visible.len().div_ceil(size), "{what}: pages");
    for (index, page) in pages.iter().enumerate() {
        let last = index + 1 == pages.len();
        let rows = if last {
            visible.len() - size * index
        } else {
            size
        };
        assert_eq!(page.hits.len(), rows, "{what}: page {index} length");
        let next = if last {
            None
        } else {
            page.hits.last().map(|hit| hit.id)
        };
        assert_eq!(page.next_after, next, "{what}: page {index} cursor");
    }
}

#[test]
fn compact_browse_pages_chain_like_the_resident_records_at_every_clock() {
    let _gates = gates();
    // The clock is explicit: a deadline hides its row at that clock and after it, one millisecond earlier it shows.
    let table: [(u64, Vec<u64>); 10] = [
        (0, vec![0, 1, 3, 5, 15, 20, MAX - 1, MAX]),
        (99, vec![0, 1, 3, 5, 15, 20, MAX - 1, MAX]),
        (100, vec![0, 1, 3, 15, 20, MAX - 1, MAX]),
        (199, vec![0, 1, 3, 15, 20, MAX - 1, MAX]),
        (200, vec![0, 1, 3, 20, MAX - 1, MAX]),
        (299, vec![0, 1, 3, 20, MAX - 1, MAX]),
        (300, vec![0, 1, 3, MAX - 1, MAX]),
        (FAR - 1, vec![0, 1, 3, MAX - 1, MAX]),
        (FAR, vec![0, 1, MAX - 1, MAX]),
        (MAX, vec![0, 1, MAX - 1, MAX]),
    ];
    // (cursor, limit, clock, IDs, next cursor)
    let pages: [Page; 8] = [
        // The cursor row is hidden at the clock; the cursor still decides the position.
        (Some(5), 2, 100, vec![15, 20], Some(20)),
        // A cursor on a deleted base ID.
        (Some(10), 3, 0, vec![15, 20, MAX - 1], Some(MAX - 1)),
        (Some(3), 1, FAR, vec![MAX - 1], Some(MAX - 1)),
        (Some(15), 5, 200, vec![20, MAX - 1, MAX], None),
        (None, 4, 300, vec![0, 1, 3, MAX - 1], Some(MAX - 1)),
        (None, 4, 299, vec![0, 1, 3, 20], Some(20)),
        (Some(0), 2, FAR, vec![1, MAX - 1], Some(MAX - 1)),
        (Some(0), 2, FAR - 1, vec![1, 3], Some(3)),
    ];
    for encrypted in [false, true] {
        let (mut candidate, mut oracle, _) = compact_dispatch_fixture(encrypted);
        for name in ["half", "full"] {
            edit_clock_rows(&mut candidate, name);
            edit_clock_rows(&mut oracle, name);
            assert_compact_resident_pair(&candidate, &oracle, name);
            for (now, visible) in &table {
                for size in [1, 2, 3, 100] {
                    let what = format!("{name}, clock {now}, pages of {size}");
                    let actual = chain(&candidate, name, size, *now);
                    assert_eq!(actual, chain(&oracle, name, size, *now), "{what}");
                    assert_chain_shape(&actual, visible, size, &what);
                    let tail = actual.last().unwrap().hits.last().unwrap().id;
                    let after_last = candidate
                        .collection(name)
                        .unwrap()
                        .browse_at(Some(tail), size, *now)
                        .unwrap();
                    assert!(
                        after_last.hits.is_empty(),
                        "{what}: the page after the last"
                    );
                    assert_eq!(after_last.next_after, None, "{what}");
                }
            }
            for (after, limit, now, want, next) in &pages {
                let page = candidate
                    .collection(name)
                    .unwrap()
                    .browse_at(*after, *limit, *now)
                    .unwrap();
                let what = format!("{name}, after {after:?} of {limit} at {now}");
                assert_eq!(page_ids(&page), *want, "{what}");
                assert_eq!(page.next_after, *next, "{what}");
                assert_eq!(
                    page,
                    oracle
                        .collection(name)
                        .unwrap()
                        .browse_at(*after, *limit, *now)
                        .unwrap(),
                    "{what}"
                );
            }
            // Overlay rows are served, not their base rows: ID 20 was rewritten with another payload.
            let page = candidate
                .collection(name)
                .unwrap()
                .browse_at(Some(15), 1, 0)
                .unwrap();
            assert_eq!(page.hits[0].record.payload, b"twenty");
            // The newest overlay entry decides the deadline too: a put without one clears the deadline of base row 3.
            for db in [&mut candidate, &mut oracle] {
                db.collection_mut(name)
                    .unwrap()
                    .put(3, &[3.0, 3.0], b"three, no deadline")
                    .unwrap();
            }
            for now in [FAR - 1, FAR, MAX] {
                let page = candidate
                    .collection(name)
                    .unwrap()
                    .browse_at(None, 100, now)
                    .unwrap();
                assert_eq!(
                    page_ids(&page),
                    [0, 1, 3, MAX - 1, MAX],
                    "{name}: row 3 has no deadline at {now}"
                );
                assert_eq!(
                    page,
                    oracle
                        .collection(name)
                        .unwrap()
                        .browse_at(None, 100, now)
                        .unwrap()
                );
            }
        }
    }
}

/// Changes made after the first snapshot: 1 deleted, 2 added, 3 rewritten with a deadline of 50, 5 rewritten without
/// one, 20 deleted and `MAX - 3` and `MAX - 2` added.
fn edit_later_rows(db: &mut Database, name: &str) {
    let mut collection = db.collection_mut(name).unwrap();
    assert!(collection.delete(1).unwrap());
    collection.put(2, &[1.0, 2.0], b"two").unwrap();
    collection
        .put_with_expiry(3, &[3.0, 1.0], b"three", &Metadata::new(), Some(50))
        .unwrap();
    collection
        .put(5, &[2.0, 1.0], b"five, no deadline")
        .unwrap();
    assert!(collection.delete(20).unwrap());
    collection
        .put(MAX - 3, &[6.0, 5.0], b"max minus three")
        .unwrap();
    collection
        .put(MAX - 2, &[6.0, 6.0], b"max minus two")
        .unwrap();
}

#[test]
fn compact_snapshot_browse_keeps_the_captured_state_after_later_edits() {
    let _gates = gates();
    let (mut candidate, mut oracle, _) = compact_dispatch_fixture(false);
    for db in [&mut candidate, &mut oracle] {
        edit_clock_rows(db, "full");
    }
    let old = candidate
        .collection("full")
        .unwrap()
        .read_snapshot()
        .unwrap();
    let old_oracle = oracle.collection("full").unwrap().read_snapshot().unwrap();
    for db in [&mut candidate, &mut oracle] {
        edit_later_rows(db, "full");
    }

    // (clock, IDs of the captured state, IDs of the current state)
    let table: [(u64, Vec<u64>, Vec<u64>); 5] = [
        (
            0,
            vec![0, 1, 3, 5, 15, 20, MAX - 1, MAX],
            vec![0, 2, 3, 5, 15, MAX - 3, MAX - 2, MAX - 1, MAX],
        ),
        (
            50,
            vec![0, 1, 3, 5, 15, 20, MAX - 1, MAX],
            vec![0, 2, 5, 15, MAX - 3, MAX - 2, MAX - 1, MAX],
        ),
        (
            100,
            vec![0, 1, 3, 15, 20, MAX - 1, MAX],
            vec![0, 2, 5, 15, MAX - 3, MAX - 2, MAX - 1, MAX],
        ),
        (
            200,
            vec![0, 1, 3, 20, MAX - 1, MAX],
            vec![0, 2, 5, MAX - 3, MAX - 2, MAX - 1, MAX],
        ),
        (
            300,
            vec![0, 1, 3, MAX - 1, MAX],
            vec![0, 2, 5, MAX - 3, MAX - 2, MAX - 1, MAX],
        ),
    ];
    let current = candidate.collection("full").unwrap();
    let current_oracle = oracle.collection("full").unwrap();
    for (now, captured, latest) in &table {
        let page = old.browse_at(None, 100, *now).unwrap();
        assert_eq!(page_ids(&page), *captured, "captured state at {now}");
        assert_eq!(page, old_oracle.browse_at(None, 100, *now).unwrap());
        let page = current.browse_at(None, 100, *now).unwrap();
        assert_eq!(page_ids(&page), *latest, "current state at {now}");
        assert_eq!(page, current_oracle.browse_at(None, 100, *now).unwrap());

        let pages = chain_with(|after| old.browse_at(after, 3, *now).unwrap());
        assert_eq!(
            pages,
            chain_with(|after| old_oracle.browse_at(after, 3, *now).unwrap())
        );
        assert_chain_shape(&pages, captured, 3, &format!("captured chain at {now}"));
    }
    assert_eq!(old.len(), 8, "the capture keeps its physical row count");
    assert_eq!(current.len(), 9);

    // `browse` samples the wall clock; the deadlines 50 to 300 are long past and `FAR` is not.
    let page = old.browse(None, 100).unwrap();
    assert_eq!(page_ids(&page), [0, 1, 3, MAX - 1, MAX]);
    assert_eq!(page, old_oracle.browse(None, 100).unwrap());
    let page = current.browse(None, 100).unwrap();
    assert_eq!(page_ids(&page), [0, 2, 5, MAX - 3, MAX - 2, MAX - 1, MAX]);
    assert_eq!(page, current_oracle.browse(None, 100).unwrap());
}

#[test]
fn compact_browse_validates_the_limit_and_checks_health_first() {
    let _gates = gates();
    let (mut candidate, mut oracle, _) = compact_dispatch_fixture(false);
    let id = candidate.collection("full").unwrap().id();
    let oracle_id = oracle.collection("full").unwrap().id();
    let snapshot = candidate
        .collection("full")
        .unwrap()
        .read_snapshot()
        .unwrap();
    let invalid = |result: crate::Result<BrowsePage>| {
        matches!(
            result,
            Err(Error::InvalidQuery("browse limit must be 1..=1000"))
        )
    };
    for limit in [0, MAX_BROWSE_PAGE + 1, usize::MAX] {
        assert!(
            invalid(
                candidate
                    .collection("full")
                    .unwrap()
                    .browse_at(None, limit, 0)
            ),
            "handle, limit {limit}"
        );
        assert!(
            invalid(oracle.collection("full").unwrap().browse_at(None, limit, 0)),
            "oracle, limit {limit}"
        );
        assert!(
            invalid(snapshot.browse_at(None, limit, 0)),
            "snapshot, limit {limit}"
        );
    }
    // The limits 1 and 1000 are served; the fixture has five rows at clock 0.
    let one = candidate
        .collection("full")
        .unwrap()
        .browse_at(None, 1, 0)
        .unwrap();
    assert_eq!(page_ids(&one), [0]);
    assert_eq!(one.next_after, Some(0));
    let all = candidate
        .collection("full")
        .unwrap()
        .browse_at(None, MAX_BROWSE_PAGE, 0)
        .unwrap();
    assert_eq!(page_ids(&all), [0, 3, 10, 20, MAX]);
    assert_eq!(all.next_after, None);
    assert_eq!(
        all,
        oracle
            .collection("full")
            .unwrap()
            .browse_at(None, MAX_BROWSE_PAGE, 0)
            .unwrap()
    );

    let before = snapshot.browse_at(None, 2, 0).unwrap();
    candidate.poisoned = true;
    oracle.poisoned = true;
    for limit in [0, 1, MAX_BROWSE_PAGE + 1] {
        for (db, id) in [(&candidate, id), (&oracle, oracle_id)] {
            assert!(
                matches!(
                    CollectionRef { db, id }.browse_at(None, limit, 0),
                    Err(Error::Poisoned)
                ),
                "a poisoned handle answers Poisoned before it checks limit {limit}"
            );
        }
    }
    // A snapshot keeps the health it was captured with, but still validates its limit.
    assert_eq!(snapshot.browse_at(None, 2, 0).unwrap(), before);
    assert_eq!(page_ids(&before), [0, 3]);
    assert!(invalid(snapshot.browse_at(None, 0, 0)));
}
