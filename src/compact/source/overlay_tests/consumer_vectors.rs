//! Named-vector search, record vectors and re-embedding over compact records.
//!
//! Each test edits a compact candidate and a resident oracle (the same image, source installation off) the same way
//! and compares the two at one clock. The expectations are also literals worked out from the seam freeze rules
//! (`docs/projects/roadmap/p3b-seam-freeze.md`, section 3): a deadline hides a row at `T >= deadline`, the newest
//! overlay entry decides, a tombstone hides its ID at every clock, and an error from a base read is never absence.
//! Code that both arms share (ranking, the clock passed down, the re-embedding filters) is pinned by those literals,
//! because comparing it with itself proves nothing.

use super::super::dispatch_tests::{compact_dispatch_fixture, gates, unsupported};
use super::assert_compact_resident_pair;
use crate::compact::source::Backing;
use crate::format::{FOOTER_SIZE, crc32};
use crate::records::{CollectionRecords, RecordMap};
use crate::vectors::{RecordVectors, VersionedVector};
use crate::{CollectionRef, Database, Error, HnswConfig, Metadata, MetadataValue, SearchHit};
use std::collections::BTreeMap;
use std::sync::Arc;

/// A deadline far in the future (2100-01-01).
const FAR: u64 = 4_102_444_800_000;
const COLLECTIONS: [&str; 2] = ["half", "full"];
const INVALID: &str = "invalid vector declaration, version, batch or sparse vector";
const POISONED_CELL: &str = "compact record cell lock poisoned";

/// The named-vector cell of a record: the vector `n` with the given `(version, components)` pairs.
fn named(versions: &[(&str, [f32; 2])]) -> RecordVectors {
    RecordVectors {
        dense: BTreeMap::from([(
            "n".to_owned(),
            versions
                .iter()
                .map(|&(version, vector)| VersionedVector {
                    version: version.to_owned(),
                    vector: vector.to_vec(),
                })
                .collect(),
        )]),
        sparse: BTreeMap::new(),
    }
}

/// Installs the cell of `id`. The named-vector store is independent of the primary rows and every put or delete drops
/// the cell of its ID, so tests install cells after their edits, also for IDs whose primary row is hidden or absent.
fn set_named(db: &mut Database, collection: &str, id: u64, vectors: RecordVectors) {
    let collection_id = db.collection(collection).unwrap().id();
    db.state_mut(collection_id)
        .vectors
        .records
        .insert(id, vectors);
}

fn phase(value: &str) -> Metadata {
    Metadata::from([("phase".to_owned(), MetadataValue::String(value.to_owned()))])
}

fn ranked(hits: &[SearchHit]) -> Vec<(u64, u64)> {
    hits.iter()
        .map(|hit| (hit.id, hit.distance.to_bits()))
        .collect()
}

fn literal(rows: &[(u64, f64)]) -> Vec<(u64, u64)> {
    rows.iter()
        .map(|&(id, distance)| (id, distance.to_bits()))
        .collect()
}

fn bits(vector: &[f32]) -> Vec<u32> {
    vector.iter().map(|value| value.to_bits()).collect()
}

/// Edits both collections so that every kind of overlay entry has a row with a named vector:
/// 0 is attached and untouched; 3 is a deadline-only overlay change (base deadline `FAR`, overlay deadline 1000);
/// 7 exists only in the overlay; 10 is a tombstone that keeps its cell; 20 is an overlay put over a live base row that
/// expires at 500; `u64::MAX` is deleted and put again; 8 is overlay-only without a cell; 99 has a cell but no row.
fn edit_for_named_search(db: &mut Database) {
    for name in COLLECTIONS {
        let base = db
            .collection(name)
            .unwrap()
            .get_at(3, 0)
            .unwrap()
            .unwrap()
            .clone();
        {
            let mut collection = db.collection_mut(name).unwrap();
            collection
                .put_with_expiry(3, &base.vector, &base.payload, &base.metadata, Some(1_000))
                .unwrap();
            assert!(collection.delete(10).unwrap());
            collection
                .put_with_expiry(20, &[0.25, 0.5], b"twenty", &Metadata::new(), Some(500))
                .unwrap();
            assert!(collection.delete(u64::MAX).unwrap());
            collection.put(u64::MAX, &[0.25, 0.25], b"again").unwrap();
            collection.put(7, &[0.5, 0.5], b"seven").unwrap();
            collection.put(8, &[0.5, 0.5], b"eight").unwrap();
        }
        set_named(
            db,
            name,
            0,
            named(&[("v1", [0.5, 0.0]), ("v2", [0.75, 0.25])]),
        );
        set_named(db, name, 3, named(&[("v1", [1.0, 0.0])]));
        set_named(db, name, 7, named(&[("v1", [0.75, 0.0])]));
        set_named(db, name, 10, named(&[("v1", [0.5, 0.0])]));
        set_named(db, name, 20, named(&[("v1", [0.625, 0.0])]));
        set_named(db, name, u64::MAX, named(&[("v1", [0.5, 0.5])]));
        set_named(db, name, 99, named(&[("v1", [0.5, 0.0])]));
    }
}

#[test]
fn compact_named_search_matches_resident_at_every_deadline_boundary() {
    let _gates = gates();
    let (mut candidate, mut oracle, _) = compact_dispatch_fixture(false);
    edit_for_named_search(&mut candidate);
    edit_for_named_search(&mut oracle);
    let query = [0.5, 0.0];
    // The visible rows in rank order with the squared L2 distance of their active vector v1 to the query. IDs 3 and
    // u64::MAX tie at 0.25 and order by ascending ID. Row 10 (a tombstone), 8 (no cell) and 99 (no row) never appear.
    let rows = [
        (0, 0.0),
        (20, 0.015_625),
        (7, 0.0625),
        (3, 0.25),
        (u64::MAX, 0.25),
    ];
    // The rows hidden at each clock: 20 at 500 and 3 at 1000 (not at its base deadline), exactly at the deadline.
    let clocks: [(u64, &[u64]); 6] = [
        (0, &[]),
        (499, &[]),
        (500, &[20]),
        (999, &[20]),
        (1_000, &[20, 3]),
        (u64::MAX, &[20, 3]),
    ];
    for name in COLLECTIONS {
        assert_compact_resident_pair(&candidate, &oracle, name);
        let actual = candidate.collection(name).unwrap();
        let reference = oracle.collection(name).unwrap();
        for (now_ms, hidden) in clocks {
            let want: Vec<_> = rows
                .iter()
                .copied()
                .filter(|(id, _)| !hidden.contains(id))
                .collect();
            let hits = actual
                .search_named_at("n", None, &query, 10, now_ms)
                .unwrap();
            assert_eq!(ranked(&hits), literal(&want), "{name} at {now_ms}");
            assert_eq!(
                hits,
                reference
                    .search_named_at("n", None, &query, 10, now_ms)
                    .unwrap(),
                "{name} at {now_ms}: resident parity"
            );
            assert_eq!(
                ranked(&hits),
                ranked(
                    &reference
                        .search_named_at("n", None, &query, 10, now_ms)
                        .unwrap()
                ),
                "{name} at {now_ms}: distances bit for bit"
            );
        }

        // The version argument picks the vector: only row 0 holds a v2, at (0.75 - 0.5)^2 + 0.25^2.
        for (version, want) in [
            (Some("v1"), literal(&rows)),
            (Some("v2"), literal(&[(0, 0.125)])),
            (Some("v9"), Vec::new()),
        ] {
            let hits = actual.search_named_at("n", version, &query, 10, 0).unwrap();
            assert_eq!(ranked(&hits), want, "{name} version {version:?}");
            assert_eq!(
                hits,
                reference
                    .search_named_at("n", version, &query, 10, 0)
                    .unwrap()
            );
        }

        // The result is cut after `k` hits in rank order.
        for (k, take) in [(0, 0), (1, 1), (3, 3), (5, 5), (6, 5)] {
            let hits = actual.search_named_at("n", None, &query, k, 0).unwrap();
            assert_eq!(ranked(&hits), literal(&rows[..take]), "{name} k={k}");
        }

        // Without a clock the call samples the wall clock: both deadlines lie far before it, row 3 has none left.
        let hits = actual.search_named("n", None, &query, 10).unwrap();
        assert_eq!(
            ranked(&hits),
            literal(&[(0, 0.0), (7, 0.0625), (u64::MAX, 0.25)]),
            "{name} wall clock"
        );
        assert_eq!(hits, reference.search_named("n", None, &query, 10).unwrap());
    }
}

#[test]
fn compact_named_search_validates_in_the_resident_order() {
    let _gates = gates();
    let (candidate, oracle, _) = compact_dispatch_fixture(false);
    for db in [&candidate, &oracle] {
        let view = db.collection("full").unwrap();
        let nan = [f32::NAN, 0.0];
        // The vector name, then the version, then the query shape: each earlier error wins over a later one.
        assert!(matches!(
            view.search_named_at("missing", None, &[0.5], 1, 0),
            Err(Error::InvalidQuery(INVALID))
        ));
        assert!(matches!(
            view.search_named_at("n", Some(""), &[0.5], 1, 0),
            Err(Error::InvalidQuery(INVALID))
        ));
        assert!(matches!(
            view.search_named_at("n", None, &[0.5], 1, 0),
            Err(Error::DimensionMismatch {
                expected: 2,
                actual: 1
            })
        ));
        assert!(matches!(
            view.search_named_at("n", None, &nan, 1, 0),
            Err(Error::NonFiniteVector)
        ));
        // A well-formed call answers, so the errors above are the arguments' and not the fixture's.
        assert_eq!(
            view.search_named_at("n", None, &[0.3, -0.0], 1, 0)
                .unwrap()
                .len(),
            1
        );
    }
}

#[test]
fn compact_record_vectors_hide_tombstoned_expired_and_absent_rows() {
    let _gates = gates();
    let (mut candidate, mut oracle, _) = compact_dispatch_fixture(false);
    for db in [&mut candidate, &mut oracle] {
        for name in COLLECTIONS {
            {
                let mut collection = db.collection_mut(name).unwrap();
                assert!(collection.delete(10).unwrap());
                // A deadline in the past hides the row at the wall clock; one in 2100 does not.
                collection
                    .put_with_expiry(20, &[0.25, 0.5], b"expired", &Metadata::new(), Some(1))
                    .unwrap();
                collection
                    .put_with_expiry(u64::MAX, &[0.25, 0.25], b"far", &Metadata::new(), Some(FAR))
                    .unwrap();
                collection.put(7, &[0.5, 0.5], b"seven").unwrap();
            }
            for (id, x) in [
                (0, 0.5),
                (3, 1.0),
                (10, 0.25),
                (20, 0.625),
                (u64::MAX, 0.75),
                (99, 0.125),
            ] {
                set_named(db, name, id, named(&[("v1", [x, 0.0])]));
            }
        }
    }
    // The cell of each row, or None where the row is hidden or absent. Rows 10, 20 and 99 do hold a cell.
    let cases = [
        (0, Some(0.5)),
        (3, Some(1.0)),
        (10, None),
        (20, None),
        (u64::MAX, Some(0.75)),
        (7, None),
        (99, None),
        (12_345, None),
    ];
    for name in COLLECTIONS {
        let collection_id = candidate.collection(name).unwrap().id();
        for id in [10, 20, 99] {
            assert!(
                candidate
                    .state(collection_id)
                    .vectors
                    .records
                    .contains_key(&id),
                "{name}: the cell of the hidden row {id} exists"
            );
        }
        let actual = candidate.collection(name).unwrap();
        let reference = oracle.collection(name).unwrap();
        for (id, x) in cases {
            let want = x.map(|x| named(&[("v1", [x, 0.0])]));
            assert_eq!(
                actual.record_vectors(id).unwrap().cloned(),
                want,
                "{name}: row {id}"
            );
            assert_eq!(
                actual.record_vectors(id).unwrap(),
                reference.record_vectors(id).unwrap(),
                "{name}: resident parity of row {id}"
            );
        }
    }
}

/// Rows after the edits (target version v2): 0 has it, so it is not pending; 3 is a base row with a far-future
/// deadline; 7 is overlay-only and has it; 8 is overlay-only and only another vector name holds a v2; 10 is a
/// tombstone; 15 is overlay-only with a far-future deadline and an older version only; 20 is an overlay put over a base
/// row; `u64::MAX` expired in the past. Returns the revision of each overlay put that is pending.
fn edit_for_reembed(db: &mut Database, name: &str) -> BTreeMap<u64, u64> {
    let mut revisions = BTreeMap::new();
    {
        let mut collection = db.collection_mut(name).unwrap();
        revisions.insert(
            20,
            collection
                .put_with_metadata(20, &[0.5, 0.5], b"twenty overlay", &phase("overlay"))
                .unwrap(),
        );
        assert!(collection.delete(10).unwrap());
        collection
            .put_with_expiry(
                u64::MAX,
                &[0.25, 0.25],
                b"expired",
                &Metadata::new(),
                Some(1),
            )
            .unwrap();
        collection.put(7, &[0.5, 0.25], b"seven").unwrap();
        revisions.insert(8, collection.put(8, &[0.25, 0.25], b"eight").unwrap());
        revisions.insert(
            15,
            collection
                .put_with_expiry(15, &[0.75, 0.75], b"fifteen", &phase("gap"), Some(FAR))
                .unwrap(),
        );
    }
    set_named(
        db,
        name,
        7,
        named(&[("v1", [0.5, 0.0]), ("v2", [0.5, 0.25])]),
    );
    set_named(
        db,
        name,
        8,
        RecordVectors {
            dense: BTreeMap::from([(
                "other".to_owned(),
                vec![VersionedVector {
                    version: "v2".to_owned(),
                    vector: vec![0.0, 0.0],
                }],
            )]),
            sparse: BTreeMap::new(),
        },
    );
    set_named(db, name, 10, named(&[("v1", [0.5, 0.0])]));
    set_named(db, name, 15, named(&[("v1", [0.5, 0.0])]));
    set_named(db, name, u64::MAX, named(&[("v1", [0.5, 0.0])]));
    revisions
}

#[test]
fn compact_reembed_pending_lists_the_effective_rows_without_the_target_version() {
    let _gates = gates();
    let (mut candidate, mut oracle, _) = compact_dispatch_fixture(false);
    for name in COLLECTIONS {
        let actual_revisions = edit_for_reembed(&mut candidate, name);
        let reference_revisions = edit_for_reembed(&mut oracle, name);
        assert_eq!(actual_revisions, reference_revisions);
        assert_compact_resident_pair(&candidate, &oracle, name);
        let actual = candidate.collection(name).unwrap();
        let reference = oracle.collection(name).unwrap();
        let job = target_version(&candidate, name);
        assert_eq!(job, "v2", "the fixture re-embeds into v2");

        // The pending rows ascend by ID; overlay-only rows are included, the tombstone and the row expired in the past
        // are not, the row that already holds v2 (0, 7) is not, and a row whose only v2 sits under another name is.
        let pending = [3, 8, 15, 20];
        let all = actual.reembed_pending("n", 100).unwrap();
        assert_eq!(
            all.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            pending,
            "{name}"
        );
        assert_eq!(all, reference.reembed_pending("n", 100).unwrap());
        // The rows are the effective ones: the base row of 3, and the overlay content of the others.
        let row = |id: u64| &all.iter().find(|(found, _)| *found == id).unwrap().1;
        assert_eq!(row(3).payload, 3u64.to_le_bytes());
        assert_eq!(
            row(3).metadata.get("id"),
            Some(&MetadataValue::String("3".to_owned()))
        );
        assert_eq!(row(8).payload, b"eight");
        assert_eq!(bits(&row(8).vector), bits(&[0.25, 0.25]));
        assert_eq!(row(8).metadata, Metadata::new());
        assert_eq!(row(15).payload, b"fifteen");
        assert_eq!(row(15).metadata, phase("gap"));
        assert_eq!(bits(&row(15).vector), bits(&[0.75, 0.75]));
        assert_eq!(row(20).payload, b"twenty overlay");
        assert_eq!(row(20).metadata, phase("overlay"));
        assert_eq!(bits(&row(20).vector), bits(&[0.5, 0.5]));
        for ((_, left), (_, right)) in all
            .iter()
            .zip(reference.reembed_pending("n", 100).unwrap().iter())
        {
            assert_eq!(bits(&left.vector), bits(&right.vector), "{name}");
        }

        // `limit` smaller than, equal to and above the pending count, and zero.
        for (limit, take) in [(0, 0), (1, 1), (2, 2), (3, 3), (4, 4), (5, 4), (4_096, 4)] {
            let rows = actual.reembed_pending("n", limit).unwrap();
            assert_eq!(
                rows.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
                pending[..take],
                "{name} limit {limit}"
            );
            assert_eq!(rows, reference.reembed_pending("n", limit).unwrap());
        }
        assert!(matches!(
            actual.reembed_pending("n", 4_097),
            Err(Error::BatchTooLarge)
        ));
        assert!(matches!(
            reference.reembed_pending("n", 4_097),
            Err(Error::BatchTooLarge)
        ));

        // The checked variant adds the revision of each row: the put's sequence for an overlay row.
        let checked = actual.reembed_pending_checked("n", 100).unwrap();
        let expected = reference.reembed_pending_checked("n", 100).unwrap();
        assert_eq!(checked.len(), pending.len());
        for ((record, mirror), want) in checked.iter().zip(&expected).zip(pending) {
            assert_eq!(record.id, want, "{name}");
            assert_eq!(mirror.id, want);
            assert_eq!(record.revision, mirror.revision, "{name}: row {want}");
            assert_eq!(record.record, mirror.record);
            assert_eq!(
                Some(record.revision),
                actual.revision(want).unwrap(),
                "{name}: the revision is the row's own"
            );
            if let Some(&put) = actual_revisions.get(&want) {
                assert_eq!(record.revision, put, "{name}: row {want} keeps its put");
            }
        }
        assert_eq!(checked[0].record.payload, 3u64.to_le_bytes());
        assert_eq!(checked[3].record.payload, b"twenty overlay");
        for limit in [0, 1, 3, 4, 5] {
            let rows = actual.reembed_pending_checked("n", limit).unwrap();
            assert_eq!(
                rows.iter().map(|row| row.id).collect::<Vec<_>>(),
                pending[..limit.min(4)],
                "{name} checked limit {limit}"
            );
        }
        assert!(matches!(
            actual.reembed_pending_checked("n", 4_097),
            Err(Error::BatchTooLarge)
        ));
    }
}

/// The target version of the job on `n`.
fn target_version(db: &Database, name: &str) -> String {
    let collection_id = db.collection(name).unwrap().id();
    db.state(collection_id)
        .vectors
        .jobs
        .get("n")
        .expect("the fixture starts a job on n")
        .target_version
        .clone()
}

#[test]
fn compact_reembed_pending_validates_in_the_resident_order() {
    let _gates = gates();
    let (candidate, oracle, _) = compact_dispatch_fixture(false);
    for db in [&candidate, &oracle] {
        let view = db.collection("full").unwrap();
        // The limit is checked before the job, and the largest limit still answers.
        assert!(matches!(
            view.reembed_pending("missing", crate::MAX_BATCH_OPERATIONS + 1),
            Err(Error::BatchTooLarge)
        ));
        assert!(matches!(
            view.reembed_pending("missing", crate::MAX_BATCH_OPERATIONS),
            Err(Error::InvalidQuery(INVALID))
        ));
        assert!(matches!(
            view.reembed_pending_checked("missing", 1),
            Err(Error::InvalidQuery(INVALID))
        ));
        assert_eq!(
            view.reembed_pending("n", crate::MAX_BATCH_OPERATIONS)
                .unwrap()
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            [3, 10, 20, u64::MAX],
            "the fixture's rows without a v2"
        );
    }
    // A vector name without a re-embedding job is as unknown as a missing name.
    let (mut candidate, mut oracle, _) = compact_dispatch_fixture(false);
    for db in [&mut candidate, &mut oracle] {
        let collection_id = db.collection("full").unwrap().id();
        db.state_mut(collection_id).vectors.jobs.clear();
        assert!(matches!(
            db.collection("full").unwrap().reembed_pending("n", 1),
            Err(Error::InvalidQuery(INVALID))
        ));
    }
}

/// Poisons the lock of the record cell of one base row, as
/// `compact_named_vector_search_propagates_primary_read_error` does. The row must not have been read yet: a cell that
/// is already materialized is served without taking the lock.
pub(super) fn poison_cell(db: &Database, collection: &str, id: u64) {
    let collection_id = db.collection(collection).unwrap().id();
    let CollectionRecords::Compact { view, .. } = &db.state(collection_id).records else {
        panic!("compact source required")
    };
    let source = view.source().unwrap();
    let (block, row) = source.row(id).unwrap();
    let cell = &source.cells[source.block_starts[block] + row];
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = cell.initializing.lock().unwrap();
        panic!("poison compact record cell lock");
    }));
}

pub(super) enum Tamper {
    Checksum,
    Ciphertext,
}

/// Damages the only block of `collection` in the pinned image, as
/// `freeze_frame_tampering_is_an_error_on_every_read_path` does for a bare generation. Every compact collection
/// releases its pin for a moment so that the generation is uniquely owned, then takes it back with its overlay.
pub(super) fn tamper_block(db: &mut Database, collection: &str, how: &Tamper) {
    let id = db.collection(collection).unwrap().id();
    let mut generation = None;
    let mut parked = Vec::new();
    for (&other, state) in &mut db.collections {
        if !state.records.is_compact() {
            continue;
        }
        let CollectionRecords::Compact { view, overlay } = std::mem::replace(
            &mut state.records,
            CollectionRecords::Resident(RecordMap::new()),
        ) else {
            unreachable!("checked above")
        };
        generation.get_or_insert_with(|| Arc::clone(&view.generation));
        parked.push((other, overlay));
    }
    let mut generation = generation.expect("the fixture has compact collections");
    let inner = Arc::get_mut(&mut generation).expect("the fixture owns its only generation handle");
    let source = inner.collections.get_mut(&id).unwrap();
    assert_eq!(source.blocks.len(), 1, "one block per collection");
    let block = &mut source.blocks[0];
    match how {
        Tamper::Checksum => block.checksum ^= 1,
        Tamper::Ciphertext => {
            let end = block.frame.end - FOOTER_SIZE;
            let mut bytes = inner.backing.as_slice().to_vec();
            bytes[end - 1] ^= 0x80;
            let checksum = crc32(&bytes[block.frame.start..end]);
            bytes[end + 4..end + 8].copy_from_slice(&checksum.to_le_bytes());
            block.checksum = checksum;
            inner.backing = Backing::Owned(Arc::from(bytes));
        }
    }
    for (other, overlay) in parked {
        db.state_mut(other).records = CollectionRecords::Compact {
            view: generation.view(other),
            overlay,
        };
    }
}

pub(super) fn is_poisoned_cell<T>(result: &Result<T, Error>) -> bool {
    matches!(result, Err(Error::InvalidQuery(reason)) if *reason == POISONED_CELL)
}

pub(super) fn is_corrupt<T>(result: &Result<T, Error>, why: &str) -> bool {
    matches!(result, Err(Error::Corrupt { reason, .. }) if *reason == why)
}

#[test]
fn compact_named_vector_consumers_propagate_a_poisoned_cell() {
    let _gates = gates();
    for name in COLLECTIONS {
        let (candidate, oracle, _) = compact_dispatch_fixture(false);
        // Row 3 holds a named vector, has no v2 and is a base row: every consumer below has to read it.
        poison_cell(&candidate, name, 3);
        let actual = candidate.collection(name).unwrap();
        let reference = oracle.collection(name).unwrap();
        let query = [0.3, -0.0];

        // The oracle answers the same calls in full, so a failure below is the cell's.
        assert_eq!(
            reference
                .search_named_at("n", None, &query, 10, 0)
                .unwrap()
                .iter()
                .map(|hit| hit.id)
                .collect::<Vec<_>>(),
            [0, 3, 10, u64::MAX]
        );
        assert_eq!(
            reference
                .reembed_pending("n", 100)
                .unwrap()
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            [3, 10, 20, u64::MAX]
        );

        let search = actual.search_named_at("n", None, &query, 10, 0);
        assert!(is_poisoned_cell(&search), "{name}: {search:?}");
        let vectors = actual.record_vectors(3);
        assert!(is_poisoned_cell(&vectors), "{name}: {vectors:?}");
        for limit in [1, 4, 100] {
            let pending = actual.reembed_pending("n", limit);
            assert!(is_poisoned_cell(&pending), "{name} {limit}: {pending:?}");
            let checked = actual.reembed_pending_checked("n", limit);
            assert!(is_poisoned_cell(&checked), "{name} {limit}: {checked:?}");
        }
        // The other rows still answer: the failure is the one row's, never a general refusal.
        assert!(actual.record_vectors(0).unwrap().is_some());
    }
}

#[test]
fn compact_named_vector_consumers_propagate_a_tampered_block() {
    let _gates = gates();
    for (how, encrypted, why) in [
        (Tamper::Checksum, false, "compact source frame changed"),
        (Tamper::Ciphertext, true, "frame authentication failed"),
    ] {
        for name in COLLECTIONS {
            let (mut candidate, oracle, _) = compact_dispatch_fixture(encrypted);
            tamper_block(&mut candidate, name, &how);
            let actual = candidate.collection(name).unwrap();
            let reference = oracle.collection(name).unwrap();
            let query = [0.3, -0.0];
            assert_eq!(
                reference
                    .search_named_at("n", None, &query, 10, 0)
                    .unwrap()
                    .len(),
                4
            );

            let search = actual.search_named_at("n", None, &query, 10, 0);
            assert!(is_corrupt(&search, why), "{name}: {search:?}");
            let vectors = actual.record_vectors(0);
            assert!(is_corrupt(&vectors, why), "{name}: {vectors:?}");
            let pending = actual.reembed_pending("n", 100);
            assert!(is_corrupt(&pending, why), "{name}: {pending:?}");
            let checked = actual.reembed_pending_checked("n", 100);
            assert!(is_corrupt(&checked, why), "{name}: {checked:?}");
        }
    }
}

#[test]
fn compact_named_vector_consumers_read_no_base_row_that_the_overlay_shadows() {
    let _gates = gates();
    for name in COLLECTIONS {
        let (mut candidate, _, _) = compact_dispatch_fixture(false);
        {
            // Every base row is replaced before the block is damaged: the replacements are the effective rows.
            let mut collection = candidate.collection_mut(name).unwrap();
            for (id, x) in [
                (0, 0.5),
                (3, 1.0),
                (10, 0.25),
                (20, 0.625),
                (u64::MAX, 0.75),
            ] {
                collection
                    .put(id, &[x, 0.0], format!("overlay {id}").as_bytes())
                    .unwrap();
            }
        }
        set_named(
            &mut candidate,
            name,
            3,
            named(&[("v1", [1.0, 0.0]), ("v2", [1.0, 0.0])]),
        );
        set_named(
            &mut candidate,
            name,
            20,
            named(&[("v1", [0.5, 0.0]), ("v2", [0.5, 0.0])]),
        );
        set_named(&mut candidate, name, 10, named(&[("v1", [0.25, 0.0])]));
        tamper_block(&mut candidate, name, &Tamper::Checksum);

        let view = candidate.collection(name).unwrap();
        // Rows 3 and 20 hold a v2; the other three are pending, with their overlay content.
        let pending = view.reembed_pending("n", 100).unwrap();
        assert_eq!(
            pending.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            [0, 10, u64::MAX],
            "{name}"
        );
        assert_eq!(pending[1].1.payload, b"overlay 10");
        let checked = view.reembed_pending_checked("n", 100).unwrap();
        assert_eq!(
            checked.iter().map(|row| row.id).collect::<Vec<_>>(),
            [0, 10, u64::MAX]
        );
        assert_eq!(
            view.record_vectors(10).unwrap().cloned(),
            Some(named(&[("v1", [0.25, 0.0])]))
        );
        let hits = view
            .search_named_at("n", None, &[0.25, 0.0], 10, 0)
            .unwrap();
        assert_eq!(
            ranked(&hits),
            literal(&[(10, 0.0), (20, 0.0625), (3, 0.5625)]),
            "{name}"
        );
    }
}

/// The error each consumer gives for arguments that are invalid in the way it checks first, with the handle healthy.
fn invalid_calls(view: CollectionRef<'_>) -> Vec<(&'static str, Error)> {
    let too_large = crate::MAX_BATCH_OPERATIONS + 1;
    vec![
        (
            "search_named_at",
            view.search_named_at("missing", Some(""), &[], 0, 0)
                .unwrap_err(),
        ),
        (
            "search_named",
            view.search_named("missing", Some(""), &[], 0).unwrap_err(),
        ),
        (
            "reembed_pending",
            view.reembed_pending("missing", too_large).unwrap_err(),
        ),
        (
            "reembed_pending_checked",
            view.reembed_pending_checked("missing", too_large)
                .unwrap_err(),
        ),
        (
            "search_named_ann_at",
            view.search_named_ann_at("missing", Some(""), &[], 0, 0, 0)
                .unwrap_err(),
        ),
        (
            "search_named_ann",
            view.search_named_ann("missing", Some(""), &[], 0, 0)
                .unwrap_err(),
        ),
    ]
}

#[test]
fn compact_named_vector_consumers_check_health_before_anything_else() {
    let _gates = gates();
    let (mut candidate, mut oracle, _) = compact_dispatch_fixture(false);
    let candidate_id = candidate.collection("full").unwrap().id();
    let oracle_id = oracle.collection("full").unwrap().id();

    // Healthy: the specific error of each call. The ANN calls are refused on compact records before their arguments are
    // read; the resident arm reads the vector name first.
    for (db, id, compact) in [
        (&candidate, candidate_id, true),
        (&oracle, oracle_id, false),
    ] {
        for (method, error) in invalid_calls(CollectionRef { db, id }) {
            let expected = match method {
                "reembed_pending" | "reembed_pending_checked" => {
                    matches!(&error, Error::BatchTooLarge)
                }
                "search_named_ann_at" | "search_named_ann" if compact => matches!(
                    &error,
                    Error::UnsupportedFeature(flag) if *flag == crate::format::FLAG_COMPACT
                ),
                _ => matches!(&error, Error::InvalidQuery(INVALID)),
            };
            assert!(expected, "{method} (compact {compact}): {error:?}");
        }
    }

    // Poisoned: every consumer answers Poisoned for the same invalid arguments, compact and resident alike.
    candidate.poisoned = true;
    oracle.poisoned = true;
    for (db, id) in [(&candidate, candidate_id), (&oracle, oracle_id)] {
        let view = CollectionRef { db, id };
        for (method, error) in invalid_calls(view) {
            assert!(matches!(error, Error::Poisoned), "{method}: {error:?}");
        }
        for found in [0, 3, 12_345] {
            assert!(
                matches!(view.record_vectors(found), Err(Error::Poisoned)),
                "record_vectors({found})"
            );
        }
        assert!(matches!(
            view.search_named_at("n", None, &[0.3, -0.0], 1, 0),
            Err(Error::Poisoned)
        ));
        assert!(matches!(view.reembed_pending("n", 1), Err(Error::Poisoned)));
    }
}

#[test]
fn compact_named_ann_stays_refused_and_leaves_the_state_unchanged() {
    let _gates = gates();
    let (mut candidate, mut oracle, _) = compact_dispatch_fixture(false);
    let id = candidate.collection("full").unwrap().id();
    let bytes = candidate.to_bytes();
    let sequence = candidate.sequence;
    let query = [0.3, -0.0];
    {
        let view = candidate.collection("full").unwrap();
        // A valid call, an unknown name, a bad version, a wrong dimension and bad `k` and `ef`: the refusal comes first.
        for (name, version, query, k, ef) in [
            ("n", None, &query[..], 1, 4),
            ("missing", None, &query[..], 1, 4),
            ("n", Some(""), &query[..], 1, 4),
            ("n", None, &query[..1], 1, 4),
            ("n", None, &query[..], 4, 1),
            ("n", None, &query[..], 1, 0),
        ] {
            unsupported(
                view.search_named_ann_at(name, version, query, k, ef, 0),
                "search_named_ann_at",
            );
            unsupported(
                view.search_named_ann(name, version, query, k, ef),
                "search_named_ann",
            );
        }
    }
    // Rebuilding the index is refused as well, also for an invalid configuration, and builds nothing.
    for config in [
        HnswConfig::default(),
        HnswConfig {
            m: 0,
            ..HnswConfig::default()
        },
    ] {
        unsupported(
            candidate
                .collection_mut("full")
                .unwrap()
                .rebuild_named_hnsw("n", None, config),
            "rebuild_named_hnsw",
        );
    }
    assert!(candidate.state(id).vector_indexes.is_empty());
    assert_eq!(candidate.sequence, sequence);
    assert_eq!(candidate.to_bytes(), bytes);

    // The same calls are valid on the resident oracle, so the refusal is the compact arm's and not the arguments'.
    oracle
        .collection_mut("full")
        .unwrap()
        .rebuild_named_hnsw("n", None, HnswConfig::default())
        .unwrap();
    let hits = oracle
        .collection("full")
        .unwrap()
        .search_named_ann_at("n", None, &query, 1, 4, 0)
        .unwrap();
    assert_eq!(hits.len(), 1);
}
