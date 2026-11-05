//! Ordered-ID cursors of the compact read seam (0.7.1 R0, slice 3).
//!
//! `CompactView::base_rows`, `CollectionRecords::affected_ids` and
//! `CollectionRecords::ordered_ids` are pinned by statements BAS, AFF and ORD
//! of `docs/projects/roadmap/p3b-seam-freeze.md`. Expectations are literals,
//! the resident records of the same image, or a model that the test keeps
//! next to every commit it applies, never the code under test. Source and
//! records are test-gated, so nothing here touches a release build.

use super::dispatch_tests::gates;
use super::*;
use crate::compact::catalog::tests::{source_empty_fixture, source_key};
use crate::format;
use crate::records::{Affected, CollectionRecords, RecordMap};
use crate::{EncryptionKey, Metadata, MetadataValue};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;
use std::iter::FusedIterator;
use std::sync::atomic::Ordering;

fn tagged(tag: &str) -> Metadata {
    Metadata::from([("tag".into(), MetadataValue::String(tag.into()))])
}

fn record(id: u64, tag: &str) -> Record {
    Record {
        vector: vec![id as f32, 0.5],
        payload: format!("payload {id}").into_bytes(),
        metadata: tagged(tag),
    }
}

const ROWS: [(u64, f32, &str, Option<u64>); 4] = [
    (1, 1.0, "one", None),
    (2, 2.0, "two", Some(100)),
    (3, 3.0, "three", None),
    (4, 4.0, "four", Some(1_000)),
];

/// Collection 0 as a float32 compact image holding exactly `ROWS`, loaded back
/// with its resident maps as the oracle.
fn small(key: Option<&EncryptionKey>) -> format::Loaded {
    let mut db = crate::Database::create_in_memory(
        2,
        crate::CreateOptions {
            encryption: key.cloned(),
            ..crate::CreateOptions::default()
        },
    )
    .unwrap();
    for (id, start, tag, expiry) in ROWS {
        db.put_with_expiry(
            id,
            &[start, start + 0.5],
            format!("payload {id}").as_bytes(),
            &tagged(tag),
            expiry,
        )
        .unwrap();
    }
    let mut loaded = format::load_with(&mut Cursor::new(db.to_bytes().unwrap()), key).unwrap();
    loaded.flags |= format::FLAG_PRECISION;
    for collection in loaded.collections.values_mut() {
        collection.precision = Some(Precision::Float32);
    }
    let mut image = Vec::new();
    crate::maintenance::write_v5_image(
        &mut image,
        &loaded,
        loaded.lineage.unwrap(),
        None,
        None,
        loaded.cipher.as_ref(),
        loaded.flags,
        loaded.sequence,
    )
    .unwrap();
    format::load_with(&mut Cursor::new(image), key).unwrap()
}

const BLOCK_ROWS: usize = 2 * 4096 + 3;

/// Collection 0 holds `BLOCK_ROWS` rows over three physical blocks: row `n` has
/// ID `2 * n`, revision `n + 1` and a deadline `100 + n` when `n % 3 == 0`.
/// Collection 1 holds the IDs 1 and 3 in one block.
fn multiblock(key: Option<&EncryptionKey>) -> format::Loaded {
    let (_, mut input) = source_empty_fixture(key);
    input.sequence = BLOCK_ROWS as u64 + 1;
    let collection = input.collections.get_mut(&0).unwrap();
    for row in 0..BLOCK_ROWS {
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
    let mut other =
        format::LoadedCollection::new("other".into(), 2, crate::CollectionSettings::default());
    other.precision = Some(Precision::Float32);
    for id in [1, 3] {
        other.put(id, record(id, "other"), 1, None);
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
    let loaded = format::load_with(&mut Cursor::new(&image), key).unwrap();
    let blocks = &loaded.compact_generation.as_ref().unwrap().collections[&0].blocks;
    assert_eq!(
        blocks
            .iter()
            .map(|block| block.source.rows.len())
            .collect::<Vec<_>>(),
        vec![4096, 4096, 3],
        "the fixture must keep three physical blocks"
    );
    loaded
}

fn multiblock_rows() -> Vec<BaseRow> {
    (0..BLOCK_ROWS)
        .map(|row| BaseRow {
            row,
            id: 2 * row as u64,
            revision: row as u64 + 1,
            expiry: (row % 3 == 0).then_some(100 + row as u64),
        })
        .collect()
}

/// The physical rows of an image as its resident decode lists them, in ID order.
fn oracle_rows(expected: &format::LoadedCollection) -> Vec<BaseRow> {
    expected
        .records
        .keys()
        .enumerate()
        .map(|(row, &id)| BaseRow {
            row,
            id,
            revision: expected.revisions[&id],
            expiry: expected.expiries.get(&id).copied(),
        })
        .collect()
}

fn is_exact_fused_clone<I: Clone + ExactSizeIterator + FusedIterator>(_: &I) {}

fn no_activity(accounting: &SeamAccounting) -> bool {
    accounting.hits == 0
        && accounting.misses == 0
        && accounting.evictions == 0
        && accounting.vectors_live == 0
        && accounting.plaintext_live == 0
        && accounting.cells_live == 0
}

#[test]
fn ordered_base_rows_list_every_physical_row_in_ascending_id_order() {
    let _gates = gates();
    for encrypted in [false, true] {
        let key = encrypted.then(source_key);
        let loaded = small(key.as_ref());
        let expected = &loaded.collections[&0];
        let generation = loaded.compact_generation.as_ref().unwrap();
        let view = generation.view(0);

        // The oracle is the resident decode of the same image, in ID order.
        let oracle = oracle_rows(expected);
        let rows: Vec<BaseRow> = view.base_rows().collect();
        assert_eq!(rows, oracle);
        assert_eq!(
            rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
        assert_eq!(
            rows.iter().map(|row| row.expiry).collect::<Vec<_>>(),
            vec![None, Some(100), None, Some(1_000)]
        );
        // The deadline-bearing rows are listed although a clock past them
        // hides them from every deadline-aware reader.
        assert_eq!(view.get_at(2, u64::MAX).unwrap(), None);
        assert_eq!(view.get_at(4, 1_000).unwrap(), None);
        assert_eq!(rows[1].id, 2);
        assert_eq!(rows[3].id, 4);
        // The cursor agrees with the point accessors row by row.
        for row in &rows {
            assert_eq!(view.id(row.row), Some(row.id));
            assert_eq!(view.revision(row.row), Some(row.revision));
            assert_eq!(view.expiry(row.row), Some(row.expiry));
            assert_eq!(view.row_of(row.id), Some(row.row));
        }

        // Exact size at every step, then fused.
        let mut cursor = view.base_rows();
        is_exact_fused_clone(&cursor);
        assert_eq!(cursor.len(), 4);
        for taken in 1..=4 {
            assert!(cursor.next().is_some());
            assert_eq!(cursor.len(), 4 - taken);
            assert_eq!(cursor.size_hint(), (4 - taken, Some(4 - taken)));
        }
        assert_eq!(cursor.next(), None);
        assert_eq!(cursor.next(), None);

        // A clone walks on its own from where it was taken.
        let mut original = view.base_rows();
        original.next();
        let copy = original.clone();
        assert_eq!(
            original.map(|row| row.id).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
        assert_eq!(copy.len(), 3);
        assert_eq!(copy.map(|row| row.id).collect::<Vec<_>>(), vec![2, 3, 4]);

        // A collection the generation does not hold has no rows.
        assert_eq!(generation.view(9).base_rows().len(), 0);
        assert_eq!(generation.view(9).base_rows().next(), None);
    }
}

#[test]
fn ordered_base_rows_cross_every_block_and_read_nothing() {
    let _gates = gates();
    for encrypted in [false, true] {
        let key = encrypted.then(source_key);
        let mut loaded = multiblock(key.as_ref());
        let mut generation = loaded.compact_generation.take().unwrap();
        let expected = multiblock_rows();

        let rows: Vec<BaseRow> = generation.view(0).base_rows().collect();
        assert_eq!(rows.len(), BLOCK_ROWS);
        assert_eq!(rows, expected);
        assert_eq!(generation.view(0).base_rows().len(), BLOCK_ROWS);
        // Each collection lists its own rows only.
        assert_eq!(
            generation
                .view(1)
                .base_rows()
                .map(|row| (row.row, row.id))
                .collect::<Vec<_>>(),
            vec![(0, 1), (1, 3)]
        );
        assert_eq!(generation.view(2).base_rows().count(), 0);

        // Point every block outside the backing: any read of a block fails,
        // yet the directory still lists every row and no counter moves.
        let inner = Arc::get_mut(&mut generation).unwrap();
        let end = inner.backing.as_slice().len();
        for source in inner.collections.values_mut() {
            for block in &mut source.blocks {
                block.frame = end..end + 8;
            }
        }
        let view = generation.view(0);
        let mut workspace = ReadWorkspace::new(4 * 1024 * 1024).unwrap();
        assert!(matches!(
            view.read_payload(0, &mut workspace),
            Err(Error::Corrupt { reason, .. }) if reason == "compact source frame is out of bounds"
        ));
        let mut walked = 0;
        for (row, listed) in view.base_rows().zip(&expected) {
            assert_eq!(&row, listed);
            walked += 1;
        }
        assert_eq!(walked, BLOCK_ROWS);
        let accounting = workspace.accounting(&view).unwrap();
        assert!(no_activity(&accounting), "{accounting:?}");
    }
}

/// One compact collection with its resident oracle and the model that the test
/// keeps next to every commit: the IDs that are effective and the entries the
/// overlay must hold.
struct Model {
    view: CompactView,
    rows: CollectionRecords,
    resident: CollectionRecords,
    /// Revision and deadline of every effective ID; its keys are the effective IDs.
    state: BTreeMap<u64, (u64, Option<u64>)>,
    affected: BTreeMap<u64, Affected>,
    revision: u64,
    commits: usize,
    cursor_every: usize,
}

impl Model {
    /// A compact handle replayed from the given state at `revision`, next to
    /// its resident oracle. `affected` is what the replay must have recorded.
    fn new(
        view: CompactView,
        records: RecordMap,
        revisions: &BTreeMap<u64, u64>,
        expiries: &BTreeMap<u64, u64>,
        affected: BTreeMap<u64, Affected>,
        revision: u64,
        cursor_every: usize,
    ) -> Self {
        let rows = CollectionRecords::compact_from_replay(
            view.clone(),
            &records,
            revisions,
            expiries,
            revision,
        )
        .unwrap();
        let state = records
            .keys()
            .map(|&id| (id, (revisions[&id], expiries.get(&id).copied())))
            .collect();
        Self {
            view,
            rows,
            resident: CollectionRecords::Resident(records),
            state,
            affected,
            revision,
            commits: 0,
            cursor_every,
        }
    }

    fn ids(&self) -> Vec<u64> {
        self.state.keys().copied().collect()
    }

    fn decodes(&self) -> usize {
        self.view.generation.decode_count.load(Ordering::Relaxed)
    }

    fn affected_list(&self) -> Vec<(u64, Affected)> {
        self.affected
            .iter()
            .map(|(&id, &affected)| (id, affected))
            .collect()
    }

    fn commit(&mut self, id: u64, record: Option<Record>, expiry: Option<u64>) {
        self.revision += 1;
        self.commits += 1;
        self.rows
            .apply_committed(id, record.clone(), self.revision, expiry)
            .unwrap();
        self.resident
            .apply_committed(id, record.clone(), self.revision, expiry)
            .unwrap();
        if record.is_some() {
            self.state.insert(id, (self.revision, expiry));
            self.affected.insert(
                id,
                Affected::Put {
                    revision: self.revision,
                    expiry,
                },
            );
        } else {
            self.state.remove(&id);
            self.affected.insert(
                id,
                Affected::Delete {
                    revision: self.revision,
                },
            );
        }
        self.check();
        if self.commits.is_multiple_of(self.cursor_every) {
            self.check_cursor();
        }
    }

    fn check(&self) {
        let expected = self.ids();
        let decoded = self.decodes();
        let listed: Vec<u64> = self.rows.ordered_ids().collect();
        let affected: Vec<(u64, Affected)> = self.rows.affected_ids().collect();
        assert_eq!(self.decodes(), decoded, "the cursors decode no record");
        assert_eq!(listed, expected);
        assert_eq!(affected, self.affected_list());
        assert_eq!(
            self.rows.effective_ids(),
            expected,
            "the materializing reader agrees"
        );
        assert_eq!(
            self.resident.ordered_ids().collect::<Vec<_>>(),
            expected,
            "the resident oracle agrees"
        );
        assert_eq!(self.resident.affected_ids().count(), 0);
    }

    /// The record cursor is the other merge of base and overlay: it lists the
    /// same IDs, from the start and after every seek point. A small set is
    /// seeked at every ID and its neighbours. A large one at the first, last
    /// and every 2,000th ID and at both sides of every block boundary of the
    /// base (rows 4,095 and 4,096, then 8,191 and 8,192, have the IDs
    /// 8,190 and 8,192, then 16,382 and 16,384).
    fn check_cursor(&self) {
        let expected = self.ids();
        let walked = |after: Option<u64>| -> Vec<u64> {
            self.rows
                .compact_iter(after)
                .unwrap()
                .map(|item| item.unwrap().0)
                .collect()
        };
        assert_eq!(walked(None), expected, "compact_iter from the start");
        let around = |id: u64| [id.saturating_sub(1), id, id.saturating_add(1)];
        let mut seeks = BTreeSet::from([0, u64::MAX]);
        for (index, &id) in expected.iter().enumerate() {
            if expected.len() <= 64 || index % 2_000 == 0 || index + 1 == expected.len() {
                seeks.extend(around(id));
            }
        }
        if expected.len() > 64 {
            for boundary in [8_190, 8_192, 16_382, 16_384] {
                seeks.extend(around(boundary));
            }
        }
        for after in seeks {
            let rest: Vec<u64> = expected.iter().copied().filter(|&id| id > after).collect();
            assert_eq!(walked(Some(after)), rest, "compact_iter after {after}");
        }
    }

    /// What a reopen holds. Replaying the final state over the same base must
    /// list the same IDs and hold exactly the state difference that the
    /// contract (PRE-3) derives from the base rows and the final state alone:
    /// a put for every ID that is new or whose revision or deadline changed,
    /// a tombstone at the replay sequence for every base ID that is gone, and
    /// nothing for an unchanged ID. It returns the IDs for which the live
    /// handle holds a tombstone although the base never held them: the reopen
    /// holds no entry for any of those.
    fn check_reopen(&self, base: &[BaseRow]) -> Vec<u64> {
        let CollectionRecords::Resident(records) = &self.resident else {
            unreachable!("the oracle is resident");
        };
        let revisions: BTreeMap<u64, u64> = self
            .state
            .iter()
            .map(|(&id, &(revision, _))| (id, revision))
            .collect();
        let expiries: BTreeMap<u64, u64> = self
            .state
            .iter()
            .filter_map(|(&id, &(_, expiry))| expiry.map(|expiry| (id, expiry)))
            .collect();
        let reopened = CollectionRecords::compact_from_replay(
            self.view.clone(),
            records,
            &revisions,
            &expiries,
            self.revision,
        )
        .unwrap();
        assert_eq!(reopened.ordered_ids().collect::<Vec<_>>(), self.ids());

        let held: BTreeMap<u64, &BaseRow> = base.iter().map(|row| (row.id, row)).collect();
        let mut every: BTreeSet<u64> = held.keys().copied().collect();
        every.extend(self.state.keys().copied());
        let mut expected = Vec::new();
        for id in every {
            match (held.get(&id), self.state.get(&id)) {
                (Some(_), None) => expected.push((
                    id,
                    Affected::Delete {
                        revision: self.revision,
                    },
                )),
                (None, Some(&(revision, expiry))) => {
                    expected.push((id, Affected::Put { revision, expiry }));
                }
                (Some(row), Some(&(revision, expiry)))
                    if (row.revision, row.expiry) != (revision, expiry) =>
                {
                    expected.push((id, Affected::Put { revision, expiry }));
                }
                _ => {}
            }
        }
        assert_eq!(reopened.affected_ids().collect::<Vec<_>>(), expected);

        // The IDs for which the live handle holds a tombstone although the base
        // never held them. The comparison above leaves them without an entry,
        // because the oracle has none for an ID that is in neither the base nor
        // the final state.
        self.affected
            .iter()
            .filter(|(id, affected)| {
                matches!(affected, Affected::Delete { .. }) && !held.contains_key(*id)
            })
            .map(|(&id, _)| id)
            .collect()
    }
}

#[test]
fn ordered_ids_and_affected_ids_follow_every_kind_of_change() {
    let _gates = gates();
    for encrypted in [false, true] {
        let key = encrypted.then(source_key);
        let loaded = small(key.as_ref());
        let expected = &loaded.collections[&0];
        let base = oracle_rows(expected);
        let view = loaded.compact_generation.as_ref().unwrap().view(0);
        let sequence = loaded.sequence;

        // An untouched image has an empty overlay: its IDs are the base rows.
        let untouched = CollectionRecords::compact_from_replay(
            view.clone(),
            &expected.records,
            &expected.revisions,
            &expected.expiries,
            sequence,
        )
        .unwrap();
        assert_eq!(
            untouched.ordered_ids().collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
        assert_eq!(untouched.affected_ids().count(), 0);

        // What a reopen replays from the log after the image: ID 1 changes its
        // metadata only, ID 2 its deadline only, ID 3 is deleted, ID 4 stays,
        // ID 7 is new.
        let mut records = expected.records.clone();
        let mut revisions = expected.revisions.clone();
        let mut expiries = expected.expiries.clone();
        let mut changed = expected.records[&1].clone();
        changed.metadata = tagged("uno");
        records.insert(1, changed);
        revisions.insert(1, sequence + 1);
        revisions.insert(2, sequence + 2);
        expiries.insert(2, 500);
        records.remove(&3);
        revisions.remove(&3);
        expiries.remove(&3);
        records.insert(7, record(7, "seven"));
        revisions.insert(7, sequence + 3);
        let mut model = Model::new(
            view,
            records,
            &revisions,
            &expiries,
            BTreeMap::from([
                (
                    1,
                    Affected::Put {
                        revision: sequence + 1,
                        expiry: None,
                    },
                ),
                (
                    2,
                    Affected::Put {
                        revision: sequence + 2,
                        expiry: Some(500),
                    },
                ),
                (
                    3,
                    Affected::Delete {
                        revision: sequence + 3,
                    },
                ),
                (
                    7,
                    Affected::Put {
                        revision: sequence + 3,
                        expiry: None,
                    },
                ),
            ]),
            sequence + 3,
            1,
        );
        // The first look at the cursors, before anything has decoded a record.
        model.check();
        assert_eq!(
            model.rows.ordered_ids().collect::<Vec<_>>(),
            vec![1, 2, 4, 7]
        );
        model.check_cursor();
        assert!(model.check_reopen(&base).is_empty());

        // A pinned clone keeps the overlay root it was cloned with, and what it
        // lists is what the model held when it was taken.
        let pinned = model.rows.pin_effective();
        let pinned_ids = model.ids();
        let pinned_affected = model.affected_list();
        let check_pin = || {
            assert_eq!(pinned.ordered_ids().collect::<Vec<_>>(), pinned_ids);
            assert_eq!(pinned.affected_ids().collect::<Vec<_>>(), pinned_affected);
        };
        check_pin();
        assert_eq!(pinned_ids, vec![1, 2, 4, 7]);

        // Reinsert after a tombstone.
        model.commit(3, Some(record(3, "again")), None);
        // A batch that writes one new ID twice: the newest entry wins.
        model.commit(5, Some(record(5, "five")), None);
        model.commit(5, Some(record(5, "five again")), Some(900));
        // Deletes of IDs that no image or commit ever held: one above the base
        // rows, one below them while every base row is still to be listed.
        model.commit(9, None, None);
        model.commit(0, None, None);
        assert_eq!(
            model.rows.ordered_ids().collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5, 7]
        );
        // A new ID below the base, and a delete of a base ID without an entry.
        model.commit(0, Some(record(0, "zero")), None);
        model.commit(4, None, None);
        // Delete, reinsert and delete again an ID that is not in the base.
        model.commit(7, None, None);
        model.commit(7, Some(record(7, "seven again")), None);
        model.commit(7, None, None);
        // Metadata-only and deadline-only rewrites of base IDs, then a delete.
        model.commit(2, Some(record(2, "two again")), Some(500));
        model.commit(1, Some(record(1, "uno again")), None);
        model.commit(1, None, None);
        assert_eq!(
            model.rows.ordered_ids().collect::<Vec<_>>(),
            vec![0, 2, 3, 5]
        );

        // The earlier pin did not move.
        check_pin();

        // The live handle holds tombstones for IDs 7 and 9, which the base
        // never held. After a reopen the same final state holds no entry for
        // them, and exactly the state difference for every other ID.
        assert_eq!(model.check_reopen(&base), vec![7, 9]);
    }
}

#[test]
fn ordered_ids_ignore_deadlines_and_read_no_block() {
    let _gates = gates();
    for encrypted in [false, true] {
        let key = encrypted.then(source_key);
        let mut loaded = small(key.as_ref());
        let expected = &loaded.collections[&0];
        let mut generation = loaded.compact_generation.take().unwrap();
        let sequence = loaded.sequence;

        // Frames that can be read: the cursors still decode no record and
        // create no cell, which a decode that fails could not show. The
        // overlay holds a put of 8 with a deadline that has long passed, a put
        // of base row 2 and a tombstone of base row 3, so a cursor that decoded
        // the old record of an ID with an entry would leave a cell behind. Row
        // 4 holds a deadline of 1,000 and the put of 2 one of 500: none is
        // applied, so all of them are listed.
        {
            let view = generation.view(0);
            let mut rows = CollectionRecords::compact_from_replay(
                view.clone(),
                &expected.records,
                &expected.revisions,
                &expected.expiries,
                sequence,
            )
            .unwrap();
            rows.apply_committed(8, Some(record(8, "late")), sequence + 1, Some(1))
                .unwrap();
            rows.apply_committed(2, Some(record(2, "two again")), sequence + 2, Some(500))
                .unwrap();
            rows.apply_committed(3, None, sequence + 3, None).unwrap();
            let decodes = || view.generation.decode_count.load(Ordering::Relaxed);
            let decoded = decodes();
            assert_eq!(rows.ordered_ids().collect::<Vec<_>>(), vec![1, 2, 4, 8]);
            assert_eq!(rows.affected_ids().count(), 3);
            assert_eq!(view.base_rows().count(), 4);
            assert_eq!(decodes(), decoded, "no cursor decodes a record");
            let mut workspace = ReadWorkspace::new(4 * 1024 * 1024).unwrap();
            let accounting = rows.source_accounting(&mut workspace).unwrap().unwrap();
            assert!(no_activity(&accounting), "{accounting:?}");
        }

        // Frames that cannot be read: every block is pointed outside the
        // backing, so any block read or decode fails. The cursors never needed
        // one and make no attempt at one.
        let inner = Arc::get_mut(&mut generation).unwrap();
        let end = inner.backing.as_slice().len();
        for source in inner.collections.values_mut() {
            for block in &mut source.blocks {
                block.frame = end..end + 8;
            }
        }
        let view = generation.view(0);
        let mut rows = CollectionRecords::compact_from_replay(
            view.clone(),
            &expected.records,
            &expected.revisions,
            &expected.expiries,
            sequence,
        )
        .unwrap();
        // An overlay put whose deadline has long passed, and the same entries
        // over base rows 2 and 3.
        rows.apply_committed(8, Some(record(8, "late")), sequence + 1, Some(1))
            .unwrap();
        rows.apply_committed(2, Some(record(2, "two again")), sequence + 2, Some(500))
            .unwrap();
        rows.apply_committed(3, None, sequence + 3, None).unwrap();
        let mut workspace = ReadWorkspace::new(4 * 1024 * 1024).unwrap();
        let attempts = || view.generation.decode_count.load(Ordering::Relaxed);
        let attempted = attempts();

        assert_eq!(rows.ordered_ids().collect::<Vec<_>>(), vec![1, 2, 4, 8]);
        assert_eq!(rows.effective_ids(), vec![1, 2, 4, 8]);
        assert!(!rows.effective_visible(8, 1, &BTreeMap::new()));
        assert_eq!(
            rows.affected_ids().collect::<Vec<_>>(),
            vec![
                (
                    2,
                    Affected::Put {
                        revision: sequence + 2,
                        expiry: Some(500)
                    }
                ),
                (
                    3,
                    Affected::Delete {
                        revision: sequence + 3
                    }
                ),
                (
                    8,
                    Affected::Put {
                        revision: sequence + 1,
                        expiry: Some(1)
                    }
                ),
            ]
        );
        assert_eq!(view.base_rows().count(), 4);
        assert_eq!(attempts(), attempted, "no cursor attempts a decode");
        // The frames are unreadable, so any block read fails.
        assert!(matches!(
            rows.effective_vector(1, &mut workspace),
            Err(Error::Corrupt { .. })
        ));
        let accounting = rows.source_accounting(&mut workspace).unwrap().unwrap();
        assert_eq!(accounting.cells_live, 0);
        assert_eq!(accounting.hits, 0);
        assert_eq!(accounting.vectors_live, 0);
    }
}

/// xorshift64*: a fixed stream, so the sequence of operations is a constant.
fn next_random(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_f491_4f6c_dd1d)
}

#[test]
fn ordered_ids_match_a_seeded_model_over_random_commits() {
    let _gates = gates();
    let loaded = multiblock(None);
    let expected = &loaded.collections[&0];
    let view = loaded.compact_generation.as_ref().unwrap().view(0);
    let sequence = loaded.sequence;
    let base = multiblock_rows();
    let mut model = Model::new(
        view,
        expected.records.clone(),
        &expected.revisions,
        &expected.expiries,
        BTreeMap::new(),
        sequence,
        15,
    );
    assert_eq!(model.state.len(), BLOCK_ROWS);
    let base_ids = model.ids();
    model.check();
    model.check_cursor();

    // The base holds the even IDs 0 to 16,388. This pool has the IDs on both
    // sides of each block boundary (8,190 and 8,192, 16,382 and 16,384),
    // odd IDs that sit between base rows or above the last one, and the
    // largest ID.
    let pool: [u64; 18] = [
        0,
        1,
        2,
        8_186,
        8_188,
        8_189,
        8_190,
        8_192,
        8_193,
        16_380,
        16_382,
        16_383,
        16_384,
        16_386,
        16_388,
        16_389,
        16_390,
        u64::MAX,
    ];
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    for _ in 0..90 {
        let draw = next_random(&mut state);
        let id = pool[(draw % pool.len() as u64) as usize];
        let put = !(draw >> 8).is_multiple_of(3);
        let expiry = (draw >> 16)
            .is_multiple_of(2)
            .then_some((draw >> 24) % 5_000);
        model.commit(id, put.then(|| record(id, "random")), expiry);
    }
    assert!(model.affected.len() > 8, "the stream must touch many IDs");
    assert_ne!(model.ids(), base_ids, "the stream must change the set");

    // A reopen of the final state holds no entry for an ID that the base never
    // held, and the state difference for every other.
    assert!(
        !model.check_reopen(&base).is_empty(),
        "the stream must leave a tombstone for an ID that the base never held"
    );
}

#[test]
fn ordered_ids_resident_variant_lists_its_keys_and_no_overlay() {
    let _gates = gates();
    let mut records = RecordMap::new();
    for id in [5, 3, 9] {
        records.insert(id, record(id, "resident"));
    }
    let mut rows = CollectionRecords::Resident(records);
    assert_eq!(rows.ordered_ids().collect::<Vec<_>>(), vec![3, 5, 9]);
    assert_eq!(rows.affected_ids().count(), 0);
    rows.apply_committed(4, Some(record(4, "new")), 10, None)
        .unwrap();
    rows.apply_committed(3, None, 11, None).unwrap();
    assert_eq!(rows.ordered_ids().collect::<Vec<_>>(), vec![4, 5, 9]);
    assert_eq!(rows.affected_ids().count(), 0);
    assert_eq!(
        CollectionRecords::Resident(RecordMap::new())
            .ordered_ids()
            .next(),
        None
    );
}

#[test]
fn ordered_ids_over_a_collection_without_base_rows() {
    let _gates = gates();
    let loaded = small(None);
    // The generation holds no collection 9, as it holds none for a collection
    // that was empty in the image: the view has no base rows and the overlay
    // alone decides.
    let view = loaded.compact_generation.as_ref().unwrap().view(9);
    assert_eq!(view.base_rows().count(), 0);
    let mut model = Model::new(
        view,
        RecordMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        BTreeMap::new(),
        loaded.sequence,
        1,
    );
    model.check();
    model.check_cursor();
    model.commit(5, Some(record(5, "five")), None);
    model.commit(3, Some(record(3, "three")), Some(50));
    model.commit(4, None, None);
    model.commit(5, None, None);
    model.commit(5, Some(record(5, "five again")), None);
    assert_eq!(model.rows.ordered_ids().collect::<Vec<_>>(), vec![3, 5]);
    assert_eq!(model.check_reopen(&[]), vec![4]);
}
