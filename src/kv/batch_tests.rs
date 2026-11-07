//! `write_batch`, `get_many` and the planner behind them.
//!
//! Every expectation comes from docs/projects/roadmap/kv-batch-contract.md: a
//! batch is the same operations run one at a time, in order, on one clock, and
//! committed once. The oracle is the one-at-a-time API on a twin database,
//! never the planner.
use super::*;
use crate::{CreateOptions, Mutation};
use std::collections::BTreeMap;

const NOW: u64 = 1_000;

// Namespace "n" keys that share the first slot u64::MAX - 2, so one window
// holds several different keys (see tests/kv.rs for how they were found).
const SHARED: [&str; 4] = [
    "wrap-1e3h8xptc3rmwn",
    "wrap-1mr1oejil0ss3d",
    "wrap-rcxazt3ti43gt3",
    "wrap-zh8a7z15oq0wpx",
];

fn memory() -> Database {
    let mut db = Database::create_in_memory(1, CreateOptions::default()).unwrap();
    db.kv_store("default").unwrap();
    db
}

fn slot(offset: u64) -> u64 {
    first_slot("n", SHARED[0]).wrapping_add(offset)
}

fn put<'a>(namespace: &'a str, key: &'a str, value: &'a [u8]) -> KvOp<'a> {
    put_if(namespace, key, value, Expected::Any)
}

fn put_if<'a>(namespace: &'a str, key: &'a str, value: &'a [u8], expected: Expected) -> KvOp<'a> {
    KvOp::Put {
        namespace,
        key,
        value,
        options: KvPut {
            expected,
            expires_at_ms: None,
        },
    }
}

fn delete<'a>(namespace: &'a str, key: &'a str) -> KvOp<'a> {
    delete_if(namespace, key, Expected::Any)
}

fn delete_if<'a>(namespace: &'a str, key: &'a str, expected: Expected) -> KvOp<'a> {
    KvOp::Delete {
        namespace,
        key,
        expected,
    }
}

fn batch(db: &mut Database, operations: &[KvOp<'_>]) -> Result<Vec<KvOutcome>> {
    db.kv_store("default")
        .unwrap()
        .write_batch_at(operations, NOW)
}

fn lease(expires_at_ms: u64) -> KvPut {
    KvPut {
        expected: Expected::Any,
        expires_at_ms: Some(expires_at_ms),
    }
}

/// Every record of the collection: slot, payload, deadline and revision.
type Snapshot = BTreeMap<u64, (Vec<u8>, Option<u64>, u64)>;

fn snapshot(db: &Database) -> Snapshot {
    let id = db.resolve("default").unwrap();
    let state = db.state(id);
    state
        .records
        .resident()
        .unwrap()
        .iter()
        .map(|(&slot, record)| {
            (
                slot,
                (
                    record.payload.clone(),
                    state.expiries.get(&slot).copied(),
                    state.revisions.get(&slot).copied().unwrap_or_default(),
                ),
            )
        })
        .collect()
}

/// The snapshot without revisions: a batch gives its puts one version, one
/// at a time gives each its own.
fn bare(snapshot: &Snapshot) -> BTreeMap<u64, (Vec<u8>, Option<u64>)> {
    snapshot
        .iter()
        .map(|(&slot, (payload, expiry, _))| (slot, (payload.clone(), *expiry)))
        .collect()
}

fn foreign(db: &mut Database, offsets: impl IntoIterator<Item = u64>) {
    let mut records = db.collection_mut("default").unwrap();
    for offset in offsets {
        records.put(slot(offset), &[1.0], b"foreign").unwrap();
    }
}

fn value(db: &Database, key: &str) -> Option<Vec<u8>> {
    db.kv_view("default")
        .unwrap()
        .get_at("n", key, NOW)
        .unwrap()
        .map(|entry| entry.value)
}

fn slot_of(db: &Database, key: &str) -> Option<u64> {
    let id = db.resolve("default").unwrap();
    probe(db, id, "n", key, NOW)
        .unwrap()
        .found
        .filter(|found| !found.expired)
        .map(|found| found.slot)
}

#[test]
fn a_batch_is_one_commit_and_its_puts_share_one_version() {
    let mut db = memory();
    let before = db.sequence;
    let outcomes = batch(
        &mut db,
        &[
            put("a", "x", b"1"),
            put("b", "y", b"2"),
            delete("a", "never-written"),
        ],
    )
    .unwrap();
    assert_eq!(db.sequence, before + 1);
    assert_eq!(
        outcomes,
        [
            KvOutcome::Put {
                version: before + 1
            },
            KvOutcome::Put {
                version: before + 1
            },
            KvOutcome::Deleted(false),
        ]
    );
    let view = db.kv_view("default").unwrap();
    for (namespace, key) in [("a", "x"), ("b", "y")] {
        assert_eq!(
            view.get_at(namespace, key, NOW).unwrap().unwrap().version,
            before + 1
        );
    }
    // The version a put reports is a working compare-and-set token.
    let mut kv = db.kv_store("default").unwrap();
    kv.put_at(
        "a",
        "x",
        b"3",
        KvPut {
            expected: Expected::Revision(before + 1),
            expires_at_ms: None,
        },
        NOW,
    )
    .unwrap();
    assert_eq!(db.sequence, before + 2);
}

#[test]
fn a_batch_that_changes_no_record_commits_nothing() {
    let mut db = memory();
    batch(&mut db, &[put("a", "x", b"1")]).unwrap();
    let before = (db.sequence, snapshot(&db));
    assert!(batch(&mut db, &[]).unwrap().is_empty());
    assert_eq!(
        batch(
            &mut db,
            &[
                delete("a", "missing"),
                delete_if("a", "also-missing", Expected::Absent),
            ]
        )
        .unwrap(),
        [KvOutcome::Deleted(false), KvOutcome::Deleted(false)]
    );
    assert_eq!((db.sequence, snapshot(&db)), before);
}

#[test]
fn a_delete_of_an_expired_leftover_removes_the_record_and_reports_false() {
    let mut db = memory();
    db.kv_store("default")
        .unwrap()
        .put_at("a", "x", b"1", lease(500), 0)
        .unwrap();
    let before = db.sequence;
    assert_eq!(snapshot(&db).len(), 1);
    assert_eq!(
        batch(&mut db, &[delete("a", "x")]).unwrap(),
        [KvOutcome::Deleted(false)]
    );
    assert!(snapshot(&db).is_empty());
    assert_eq!(db.sequence, before + 1);
}

#[test]
fn a_failed_condition_reports_the_current_version_and_writes_nothing() {
    let mut db = memory();
    batch(&mut db, &[put("a", "x", b"1")]).unwrap();
    let version = db.sequence;
    let before = (db.sequence, snapshot(&db));
    for (expected, current) in [
        (Expected::Absent, Some(version)),
        (Expected::Revision(version + 1), Some(version)),
    ] {
        let error = batch(
            &mut db,
            &[put("b", "fresh", b"new"), put_if("a", "x", b"2", expected)],
        )
        .unwrap_err();
        assert!(
            matches!(error, Error::RevisionMismatch { current: seen } if seen == current),
            "{error:?}"
        );
        let error = batch(
            &mut db,
            &[put("b", "fresh", b"new"), delete_if("a", "x", expected)],
        )
        .unwrap_err();
        assert!(
            matches!(error, Error::RevisionMismatch { current: seen } if seen == current),
            "{error:?}"
        );
        assert_eq!((db.sequence, snapshot(&db)), before);
    }
    // A condition on a key that has no live entry sees `None`.
    let error = batch(&mut db, &[put_if("a", "nope", b"1", Expected::Revision(1))]).unwrap_err();
    assert!(matches!(error, Error::RevisionMismatch { current: None }));
    assert_eq!((db.sequence, snapshot(&db)), before);
}

#[test]
fn conditions_follow_the_single_operations_for_due_entries() {
    let mut db = memory();
    db.kv_store("default")
        .unwrap()
        .put_at("a", "x", b"1", lease(500), 0)
        .unwrap();
    let version = db.sequence;
    // A due entry satisfies `absent` and never matches a revision.
    assert!(
        batch(
            &mut db,
            &[put_if("a", "x", b"2", Expected::Revision(version))]
        )
        .is_err()
    );
    assert_eq!(
        batch(&mut db, &[put_if("a", "x", b"2", Expected::Absent)]).unwrap(),
        [KvOutcome::Put {
            version: version + 1
        }]
    );
    assert_eq!(snapshot(&db).len(), 1);
}

#[test]
fn a_batch_put_clears_the_deadline_of_the_entry_it_replaces() {
    let mut db = memory();
    db.kv_store("default")
        .unwrap()
        .put_at("a", "x", b"1", lease(5_000), 0)
        .unwrap();
    assert_eq!(snapshot(&db).values().next().unwrap().1, Some(5_000));
    batch(&mut db, &[put("a", "x", b"2")]).unwrap();
    let entry = db
        .kv_view("default")
        .unwrap()
        .get_at("a", "x", NOW)
        .unwrap()
        .unwrap();
    assert_eq!((entry.value, entry.expires_at_ms), (b"2".to_vec(), None));
    assert_eq!(snapshot(&db).values().next().unwrap().1, None);
}

#[test]
fn a_batch_put_with_a_deadline_is_refused_unwritten() {
    let mut db = memory();
    let before = (db.sequence, snapshot(&db));
    for expires_at_ms in [0, NOW, u64::MAX] {
        let error = batch(
            &mut db,
            &[
                put("a", "x", b"1"),
                KvOp::Put {
                    namespace: "a",
                    key: "y",
                    value: b"2",
                    options: lease(expires_at_ms),
                },
            ],
        )
        .unwrap_err();
        assert!(matches!(error, Error::InvalidKvKey(_)), "{error:?}");
        assert_eq!((db.sequence, snapshot(&db)), before);
    }
}

#[test]
fn the_whole_batch_is_checked_before_any_operation_runs() {
    let mut db = memory();
    batch(&mut db, &[put("a", "x", b"1")]).unwrap();
    let before = (db.sequence, snapshot(&db));
    // The first operation would fail its condition and the second is invalid:
    // the invalid one is reported, so what a caller sees never depends on the
    // state of the store.
    let long_key = "k".repeat(MAX_KV_KEY_BYTES + 1);
    let error = batch(
        &mut db,
        &[
            put_if("a", "x", b"2", Expected::Absent),
            put("a", &long_key, b""),
        ],
    )
    .unwrap_err();
    assert!(matches!(error, Error::InvalidKvKey(_)), "{error:?}");
    let error = batch(
        &mut db,
        &[
            put_if("a", "x", b"2", Expected::Absent),
            put("a", "dup", b"1"),
            delete("a", "dup"),
        ],
    )
    .unwrap_err();
    assert!(matches!(error, Error::InvalidKvKey(_)), "{error:?}");
    assert_eq!((db.sequence, snapshot(&db)), before);
}

#[test]
fn keys_and_values_keep_the_bounds_of_the_single_operations() {
    let mut db = memory();
    let before = (db.sequence, snapshot(&db));
    let long_namespace = "n".repeat(MAX_KV_NAMESPACE_BYTES + 1);
    let long_key = "k".repeat(MAX_KV_KEY_BYTES + 1);
    for operation in [
        put("", "x", b""),
        put("a", "", b""),
        put(&long_namespace, "x", b""),
        put("a", &long_key, b""),
        delete("", "x"),
        delete("a", ""),
        delete(&long_namespace, "x"),
        delete("a", &long_key),
    ] {
        let error = batch(&mut db, &[operation]).unwrap_err();
        assert!(matches!(error, Error::InvalidKvKey(_)), "{operation:?}");
    }
    let too_large = vec![0; MAX_KV_VALUE_BYTES + 1];
    let error = batch(&mut db, &[put("a", "x", &too_large)]).unwrap_err();
    assert!(
        matches!(error, Error::PayloadTooLarge(size) if size == MAX_KV_VALUE_BYTES + 1),
        "{error:?}"
    );
    assert_eq!((db.sequence, snapshot(&db)), before);
    // The longest namespace, key and value are fine.
    let namespace = "n".repeat(MAX_KV_NAMESPACE_BYTES);
    let key = "k".repeat(MAX_KV_KEY_BYTES);
    let largest = vec![7; MAX_KV_VALUE_BYTES];
    batch(&mut db, &[put(&namespace, &key, &largest)]).unwrap();
    assert_eq!(
        db.kv_view("default")
            .unwrap()
            .get_at(&namespace, &key, NOW)
            .unwrap()
            .unwrap()
            .value,
        largest
    );
}

#[test]
fn a_key_is_named_once_per_batch_whatever_the_operations() {
    let mut db = memory();
    let before = (db.sequence, snapshot(&db));
    for operations in [
        vec![put("a", "x", b"1"), put("a", "x", b"2")],
        vec![put("a", "x", b"1"), delete("a", "x")],
        vec![delete("a", "x"), put("a", "x", b"1")],
        vec![delete("a", "x"), delete("a", "x")],
        vec![
            put("a", "x", b"1"),
            put("b", "y", b"1"),
            put("a", "x", b"1"),
        ],
    ] {
        let error = batch(&mut db, &operations).unwrap_err();
        assert!(matches!(error, Error::InvalidKvKey(_)), "{operations:?}");
        assert_eq!((db.sequence, snapshot(&db)), before);
    }
    // The namespace is part of the name: the same key in two namespaces is
    // two keys, and so is a key and its prefix.
    batch(
        &mut db,
        &[
            put("a", "x", b"1"),
            put("b", "x", b"2"),
            put("a", "xx", b"3"),
        ],
    )
    .unwrap();
    assert_eq!(snapshot(&db).len(), 3);
}

#[test]
fn the_operation_and_value_byte_limits_are_exact() {
    let mut db = memory();
    let keys: Vec<String> = (0..=MAX_KV_BATCH_OPERATIONS)
        .map(|index| format!("k{index:04}"))
        .collect();
    let operations: Vec<_> = keys.iter().map(|key| put("a", key, b"v")).collect();
    let before = (db.sequence, snapshot(&db));
    let error = batch(&mut db, &operations).unwrap_err();
    assert!(matches!(error, Error::InvalidKvKey(_)), "{error:?}");
    assert_eq!((db.sequence, snapshot(&db)), before);
    let outcomes = batch(&mut db, &operations[..MAX_KV_BATCH_OPERATIONS]).unwrap();
    assert_eq!(outcomes.len(), MAX_KV_BATCH_OPERATIONS);
    assert_eq!(snapshot(&db).len(), MAX_KV_BATCH_OPERATIONS);
    assert_eq!(db.sequence, before.0 + 1);

    // Deletes count as operations too.
    let deletes: Vec<_> = keys.iter().map(|key| delete("a", key)).collect();
    assert!(batch(&mut db, &deletes).is_err());
    assert_eq!(
        batch(&mut db, &deletes[..MAX_KV_BATCH_OPERATIONS])
            .unwrap()
            .len(),
        1_000
    );
    assert!(snapshot(&db).is_empty());

    // Value bytes: exactly the limit passes, one more does not.
    let big = vec![1; MAX_KV_VALUE_BYTES];
    let rest = vec![2; MAX_KV_BATCH_VALUE_BYTES - 4 * MAX_KV_VALUE_BYTES];
    let mut operations = vec![
        put("a", "1", &big),
        put("a", "2", &big),
        put("a", "3", &big),
        put("a", "4", &big),
        put("a", "5", &rest),
    ];
    let before = (db.sequence, snapshot(&db));
    batch(&mut db, &operations).unwrap();
    assert_eq!(db.sequence, before.0 + 1);
    let one_more = vec![2; rest.len() + 1];
    operations[4] = put("a", "5", &one_more);
    let before = (db.sequence, snapshot(&db));
    let error = batch(&mut db, &operations).unwrap_err();
    assert!(matches!(error, Error::InvalidKvKey(_)), "{error:?}");
    assert_eq!((db.sequence, snapshot(&db)), before);
}

#[test]
fn a_batch_at_every_limit_at_once_fits_one_engine_frame() {
    let mut db = memory();
    let namespace = "n".repeat(MAX_KV_NAMESPACE_BYTES);
    let keys: Vec<String> = (0..MAX_KV_BATCH_OPERATIONS)
        .map(|index| format!("{index:04}{}", "k".repeat(MAX_KV_KEY_BYTES - 4)))
        .collect();
    let share = MAX_KV_BATCH_VALUE_BYTES / MAX_KV_BATCH_OPERATIONS;
    let mut values: Vec<Vec<u8>> = vec![vec![9; share]; MAX_KV_BATCH_OPERATIONS];
    values[0].extend(vec![
        9;
        MAX_KV_BATCH_VALUE_BYTES
            - share * MAX_KV_BATCH_OPERATIONS
    ]);
    let operations: Vec<_> = keys
        .iter()
        .zip(&values)
        .map(|(key, value)| put(&namespace, key, value))
        .collect();
    let before = db.sequence;
    assert_eq!(
        batch(&mut db, &operations).unwrap().len(),
        MAX_KV_BATCH_OPERATIONS
    );
    assert_eq!(db.sequence, before + 1);
    assert_eq!(snapshot(&db).len(), MAX_KV_BATCH_OPERATIONS);
}

#[test]
fn new_keys_of_one_window_take_the_first_free_slots_in_order() {
    let mut db = memory();
    batch(
        &mut db,
        &[
            put("n", SHARED[0], b"a"),
            put("n", SHARED[1], b"b"),
            put("n", SHARED[2], b"c"),
        ],
    )
    .unwrap();
    assert_eq!(
        [SHARED[0], SHARED[1], SHARED[2]].map(|key| slot_of(&db, key)),
        [Some(slot(0)), Some(slot(1)), Some(slot(2))]
    );
}

#[test]
fn a_put_takes_the_slot_an_earlier_delete_of_the_batch_freed() {
    let [a, b, ..] = SHARED;
    let mut db = memory();
    foreign(&mut db, 1..KV_PROBE_SLOTS);
    batch(&mut db, &[put("n", a, b"a")]).unwrap();
    assert_eq!(slot_of(&db, a), Some(slot(0)));
    // The window is full: only the slot that a's delete frees can take b.
    assert!(matches!(
        batch(&mut db, &[put("n", b, b"b")]),
        Err(Error::KvSlotsExhausted)
    ));
    batch(&mut db, &[delete("n", a), put("n", b, b"b")]).unwrap();
    assert_eq!((slot_of(&db, a), slot_of(&db, b)), (None, Some(slot(0))));
    assert_eq!(value(&db, b), Some(b"b".to_vec()));
}

#[test]
fn a_put_cannot_take_a_slot_an_earlier_put_of_the_batch_took() {
    let [a, b, ..] = SHARED;
    let mut db = memory();
    foreign(&mut db, 1..KV_PROBE_SLOTS);
    let before = (db.sequence, snapshot(&db));
    // One free slot, two new keys: the second finds the window full.
    assert!(matches!(
        batch(&mut db, &[put("n", a, b"a"), put("n", b, b"b")]),
        Err(Error::KvSlotsExhausted)
    ));
    assert_eq!((db.sequence, snapshot(&db)), before);
    batch(&mut db, &[put("n", a, b"a")]).unwrap();
    assert_eq!(value(&db, a), Some(b"a".to_vec()));
}

#[test]
fn a_key_does_not_find_its_expired_leftover_in_a_slot_an_earlier_put_reused() {
    let [a, b, ..] = SHARED;
    let mut db = memory();
    db.kv_store("default")
        .unwrap()
        .put_at("n", a, b"old", lease(500), 0)
        .unwrap();
    assert_eq!(snapshot(&db).keys().copied().collect::<Vec<_>>(), [slot(0)]);
    // b reuses a's expired slot; a then looks absent and takes the next slot.
    let outcomes = batch(&mut db, &[put("n", b, b"b"), put("n", a, b"a")]).unwrap();
    assert!(matches!(
        outcomes[..],
        [KvOutcome::Put { .. }, KvOutcome::Put { .. }]
    ));
    assert_eq!(
        (slot_of(&db, b), slot_of(&db, a)),
        (Some(slot(0)), Some(slot(1)))
    );
    assert_eq!(
        (value(&db, b), value(&db, a)),
        (Some(b"b".to_vec()), Some(b"a".to_vec()))
    );
}

#[test]
fn a_delete_of_a_key_whose_expired_leftover_was_reused_removes_nothing() {
    let [a, b, ..] = SHARED;
    let mut db = memory();
    db.kv_store("default")
        .unwrap()
        .put_at("n", a, b"old", lease(500), 0)
        .unwrap();
    let outcomes = batch(&mut db, &[put("n", b, b"b"), delete("n", a)]).unwrap();
    assert!(matches!(
        outcomes[..],
        [KvOutcome::Put { .. }, KvOutcome::Deleted(false)]
    ));
    assert_eq!(
        (slot_of(&db, b), value(&db, b)),
        (Some(slot(0)), Some(b"b".to_vec()))
    );
    assert_eq!(snapshot(&db).len(), 1);
}

#[test]
fn a_delete_of_a_live_key_frees_its_slot_for_a_later_put_even_with_a_condition() {
    let [a, b, c, _] = SHARED;
    let mut db = memory();
    batch(
        &mut db,
        &[put("n", a, b"a"), put("n", b, b"b"), put("n", c, b"c")],
    )
    .unwrap();
    let version = db.sequence;
    // Free the first two slots, then add two keys: they take the freed slots
    // in order, as one at a time would.
    batch(
        &mut db,
        &[
            delete_if("n", a, Expected::Revision(version)),
            delete("n", b),
            put("n", SHARED[3], b"d"),
        ],
    )
    .unwrap();
    assert_eq!(slot_of(&db, SHARED[3]), Some(slot(0)));
    assert_eq!(slot_of(&db, c), Some(slot(2)));
    assert_eq!(snapshot(&db).len(), 2);
}

/// Deterministic xorshift64*.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

/// Every key the property test draws from: four share one window, the others
/// have windows of their own.
fn universe() -> Vec<(&'static str, &'static str)> {
    let mut keys: Vec<_> = SHARED.iter().map(|key| ("n", *key)).collect();
    keys.extend([("m", "x0"), ("m", "x1"), ("o", "x0"), ("o", "x1")]);
    keys
}

#[derive(Clone, Copy)]
enum Setup {
    Live,
    Leased,
    Expired,
}

fn setup(db: &mut Database, keys: &[(&str, &str)], plan: &[Option<Setup>], foreign: &[u64]) {
    self::foreign(db, foreign.iter().copied());
    let mut kv = db.kv_store("default").unwrap();
    for (&(namespace, key), plan) in keys.iter().zip(plan) {
        let options = match plan {
            None => continue,
            Some(Setup::Live) => KvPut::default(),
            Some(Setup::Leased) => lease(5_000),
            Some(Setup::Expired) => lease(500),
        };
        // A window with no free slot leaves the key absent in both twins.
        let _ = kv.put_at(namespace, key, key.as_bytes(), options, 0);
    }
}

fn describe(result: &Result<Vec<KvOutcome>>) -> String {
    match result {
        Ok(_) => "ok".to_owned(),
        Err(error) => format!("{error:?}"),
    }
}

/// Whether a delete is followed by a put to the slot it freed.
fn reuses_a_freed_slot(mutations: &[Mutation]) -> bool {
    mutations
        .iter()
        .enumerate()
        .any(|(index, mutation)| match mutation {
            Mutation::Delete { id } => mutations[index + 1..]
                .iter()
                .any(|later| matches!(later, Mutation::Put { id: put, .. } if put == id)),
            _ => false,
        })
}

#[test]
fn a_batch_equals_the_same_operations_run_one_at_a_time() {
    let universe = universe();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let (mut committed, mut failed_condition, mut exhausted, mut reuse, mut wide) = (0, 0, 0, 0, 0);
    for round in 0..3_000 {
        let plan: Vec<Option<Setup>> = (0..universe.len())
            .map(|_| match rng.below(5) {
                0 => None,
                1 => Some(Setup::Leased),
                2 => Some(Setup::Expired),
                _ => Some(Setup::Live),
            })
            .collect();
        let foreign_offsets: Vec<u64> = if rng.below(3) == 0 {
            (0..KV_PROBE_SLOTS).filter(|_| rng.below(10) < 7).collect()
        } else {
            Vec::new()
        };
        let mut batched = memory();
        let mut oracle = memory();
        setup(&mut batched, &universe, &plan, &foreign_offsets);
        setup(&mut oracle, &universe, &plan, &foreign_offsets);
        assert_eq!(snapshot(&batched), snapshot(&oracle), "round {round} setup");

        // A random sequence of distinct keys, each a put or a delete with a
        // random condition: right, wrong or none.
        let mut order: Vec<usize> = (0..universe.len()).collect();
        for index in (1..order.len()).rev() {
            order.swap(index, rng.below(index + 1));
        }
        order.truncate(rng.below(universe.len() + 1));
        let values: Vec<Vec<u8>> = order
            .iter()
            .map(|&index| format!("v{round}-{index}").into_bytes())
            .collect();
        let view = batched.kv_view("default").unwrap();
        let operations: Vec<KvOp<'_>> = order
            .iter()
            .zip(&values)
            .map(|(&index, value)| {
                let (namespace, key) = universe[index];
                let live = view
                    .get_at(namespace, key, NOW)
                    .unwrap()
                    .map(|entry| entry.version);
                let expected = match rng.below(10) {
                    0..=6 => Expected::Any,
                    7 => Expected::Absent,
                    8 => live.map_or(Expected::Revision(424_242), Expected::Revision),
                    _ => Expected::Revision(424_242),
                };
                if rng.below(2) == 0 {
                    put_if(namespace, key, value, expected)
                } else {
                    delete_if(namespace, key, expected)
                }
            })
            .collect();

        let before = (batched.sequence, snapshot(&batched));
        let mutations = super::batch::plan(
            &batched,
            batched.resolve("default").unwrap(),
            &operations,
            NOW,
        )
        .map(|plan| plan.mutations)
        .ok();
        let result = batch(&mut batched, &operations);

        let mut expected_outcomes = Vec::new();
        let mut oracle_error = None;
        {
            let mut kv = oracle.kv_store("default").unwrap();
            for operation in &operations {
                let step = match *operation {
                    KvOp::Put {
                        namespace,
                        key,
                        value,
                        options,
                    } => kv
                        .put_at(namespace, key, value, options, NOW)
                        .map(|version| KvOutcome::Put { version }),
                    KvOp::Delete {
                        namespace,
                        key,
                        expected,
                    } => kv
                        .delete_at(namespace, key, expected, NOW)
                        .map(KvOutcome::Deleted),
                };
                match step {
                    Ok(outcome) => expected_outcomes.push(outcome),
                    Err(error) => {
                        oracle_error = Some(error);
                        break;
                    }
                }
            }
        }

        match (&result, oracle_error) {
            (Ok(outcomes), None) => {
                assert_eq!(outcomes.len(), expected_outcomes.len(), "round {round}");
                for (got, want) in outcomes.iter().zip(&expected_outcomes) {
                    match (got, want) {
                        (KvOutcome::Put { version }, KvOutcome::Put { .. }) => {
                            assert_eq!(*version, batched.sequence, "round {round}");
                        }
                        _ => assert_eq!(got, want, "round {round}"),
                    }
                }
                assert_eq!(
                    bare(&snapshot(&batched)),
                    bare(&snapshot(&oracle)),
                    "round {round}: {operations:?}"
                );
                let changed = mutations.as_ref().is_some_and(|m| !m.is_empty());
                assert_eq!(
                    batched.sequence,
                    before.0 + u64::from(changed),
                    "round {round}"
                );
                // Every put of the batch carries the commit's sequence, and a
                // record that no operation touched is exactly as it was.
                let after = snapshot(&batched);
                let touched: Vec<u64> = mutations
                    .iter()
                    .flatten()
                    .map(|mutation| match mutation {
                        Mutation::Put { id, .. }
                        | Mutation::PutWithMetadata { id, .. }
                        | Mutation::Delete { id } => *id,
                    })
                    .collect();
                for (slot, record) in &after {
                    if touched.contains(slot) {
                        assert_eq!(record.2, batched.sequence, "round {round}");
                    } else {
                        assert_eq!(Some(record), before.1.get(slot), "round {round}");
                    }
                }
                committed += usize::from(changed);
                if let Some(mutations) = &mutations {
                    wide += usize::from(mutations.len() >= 3);
                    reuse += usize::from(reuses_a_freed_slot(mutations));
                }
            }
            (Err(got), Some(want)) => {
                assert_eq!(
                    format!("{got:?}"),
                    format!("{want:?}"),
                    "round {round}: {operations:?}"
                );
                assert_eq!(
                    (batched.sequence, snapshot(&batched)),
                    before,
                    "round {round}: a failed batch wrote"
                );
                failed_condition += usize::from(matches!(got, Error::RevisionMismatch { .. }));
                exhausted += usize::from(matches!(got, Error::KvSlotsExhausted));
            }
            (got, want) => panic!(
                "round {round}: batch {} but one at a time {want:?}: {operations:?}",
                describe(got)
            ),
        }
    }
    // The generator must reach every branch it is meant to compare.
    let counts = format!(
        "committed {committed}, failed condition {failed_condition}, exhausted {exhausted}, \
         reused a freed slot {reuse}, three or more changes {wide}"
    );
    assert!(committed > 1_000, "{counts}");
    assert!(failed_condition > 100, "{counts}");
    assert!(exhausted > 20, "{counts}");
    assert!(reuse > 20, "{counts}");
    assert!(wide > 500, "{counts}");
}

#[test]
fn get_many_equals_one_get_per_key_in_the_order_given() {
    let universe = universe();
    let mut rng = Rng(0x1234_5678_9abc_def1);
    for round in 0..300 {
        let plan: Vec<Option<Setup>> = (0..universe.len())
            .map(|_| match rng.below(4) {
                0 => None,
                1 => Some(Setup::Leased),
                2 => Some(Setup::Expired),
                _ => Some(Setup::Live),
            })
            .collect();
        let mut db = memory();
        setup(&mut db, &universe, &plan, &[]);
        let view = db.kv_view("default").unwrap();
        // Repeats and keys that were never written are fine.
        let keys: Vec<(&str, &str)> = (0..rng.below(20))
            .map(|_| match rng.below(10) {
                0 => ("n", "never-written"),
                _ => universe[rng.below(universe.len())],
            })
            .collect();
        let want: Vec<Option<KvEntry>> = keys
            .iter()
            .map(|&(namespace, key)| view.get_at(namespace, key, NOW).unwrap())
            .collect();
        assert_eq!(view.get_many_at(&keys, NOW).unwrap(), want, "round {round}");
    }
}

#[test]
fn get_many_reports_versions_deadlines_and_one_clock_for_every_key() {
    let mut db = memory();
    {
        let mut kv = db.kv_store("default").unwrap();
        kv.put_at("a", "due", b"1", lease(NOW), 0).unwrap();
        kv.put_at("a", "late", b"2", lease(NOW + 1), 0).unwrap();
        kv.put_at("a", "plain", b"3", KvPut::default(), 0).unwrap();
    }
    let view = db.kv_view("default").unwrap();
    let entries = view
        .get_many_at(
            &[("a", "due"), ("a", "late"), ("a", "plain"), ("b", "plain")],
            NOW,
        )
        .unwrap();
    // An entry whose deadline is `now` is due; one millisecond later it is not.
    assert_eq!(entries[0], None);
    assert_eq!(entries[1].as_ref().unwrap().expires_at_ms, Some(NOW + 1));
    assert_eq!(entries[2].as_ref().unwrap().expires_at_ms, None);
    assert_eq!(entries[3], None);
    assert_eq!(
        entries[2].as_ref().unwrap().version,
        view.get_at("a", "plain", NOW).unwrap().unwrap().version
    );
    assert!(view.get_many_at(&[], NOW).unwrap().is_empty());
}

#[test]
fn get_many_checks_every_key_before_it_reads_one() {
    let mut db = memory();
    batch(&mut db, &[put("a", "x", b"1")]).unwrap();
    let view = db.kv_view("default").unwrap();
    let long_key = "k".repeat(MAX_KV_KEY_BYTES + 1);
    for keys in [
        vec![("a", "x"), ("a", "")],
        vec![("a", "x"), ("", "x")],
        vec![("a", "x"), ("a", long_key.as_str())],
    ] {
        let mut visits = 0;
        let error = view
            .visit_many_at(&keys, NOW, |_, _| {
                visits += 1;
                ControlFlow::Continue(())
            })
            .unwrap_err();
        assert!(matches!(error, Error::InvalidKvKey(_)), "{keys:?}");
        assert_eq!(visits, 0, "{keys:?}");
        assert!(matches!(
            view.get_many_at(&keys, NOW),
            Err(Error::InvalidKvKey(_))
        ));
    }
}

#[test]
fn get_many_takes_exactly_the_documented_number_of_keys() {
    let mut db = memory();
    batch(&mut db, &[put("a", "x", b"1")]).unwrap();
    let view = db.kv_view("default").unwrap();
    let keys = vec![("a", "x"); MAX_KV_GET_MANY_KEYS + 1];
    let mut visits = 0;
    let error = view
        .visit_many_at(&keys, NOW, |_, _| {
            visits += 1;
            ControlFlow::Continue(())
        })
        .unwrap_err();
    assert!(matches!(error, Error::InvalidKvKey(_)));
    assert_eq!(visits, 0);
    let entries = view
        .get_many_at(&keys[..MAX_KV_GET_MANY_KEYS], NOW)
        .unwrap();
    assert_eq!(entries.len(), MAX_KV_GET_MANY_KEYS);
    assert!(entries.iter().all(Option::is_some));
}

#[test]
fn visit_many_lends_entries_in_order_and_stops_when_the_visitor_breaks() {
    let mut db = memory();
    batch(&mut db, &[put("a", "x", b"1"), put("a", "z", b"3")]).unwrap();
    let view = db.kv_view("default").unwrap();
    let keys = [("a", "x"), ("a", "missing"), ("a", "z"), ("a", "x")];
    let mut seen = Vec::new();
    view.visit_many_at(&keys, NOW, |index, entry| {
        seen.push((
            index,
            entry.map(|entry| (entry.key.to_owned(), entry.value.to_vec(), entry.has_next)),
        ));
        ControlFlow::Continue(())
    })
    .unwrap();
    assert_eq!(
        seen,
        [
            (0, Some(("x".to_owned(), b"1".to_vec(), true))),
            (1, None),
            (2, Some(("z".to_owned(), b"3".to_vec(), true))),
            (3, Some(("x".to_owned(), b"1".to_vec(), false))),
        ]
    );
    let mut calls = 0;
    view.visit_many_at(&keys, NOW, |index, _| {
        calls += 1;
        if index == 1 {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })
    .unwrap();
    assert_eq!(calls, 2);
}

/// The slot changes that the gate of `write_batch_gated_at` is given for
/// `operations`. The gate refuses, so nothing is written.
#[cfg(feature = "server")]
fn changes_seen(db: &mut Database, operations: &[KvOp<'_>]) -> Result<Vec<(u64, bool)>> {
    let id = db.resolve("default").unwrap();
    let mut seen = Vec::new();
    let result =
        db.kv_store_by_id(id)
            .unwrap()
            .write_batch_gated_at(operations, NOW, |_, changes| {
                seen = changes.to_vec();
                Err(())
            });
    match result {
        Err(Gated::Refused(())) => Ok(seen),
        Err(Gated::Batch(error)) => Err(error),
        Ok(outcomes) => panic!("a refusing gate let a batch through: {outcomes:?}"),
    }
}

#[cfg(feature = "server")]
#[test]
fn the_quota_check_sees_the_slots_the_batch_will_write_and_delete() {
    let [a, b, c, _] = SHARED;
    let mut db = memory();
    batch(&mut db, &[put("n", a, b"a"), put("n", b, b"b")]).unwrap();
    let changes = changes_seen(
        &mut db,
        &[
            delete("n", a),
            put("n", c, b"c"),
            put("n", b, b"b2"),
            delete("n", "absent"),
        ],
    )
    .unwrap();
    // a's slot is freed and then taken by c; b keeps its own slot; the delete
    // of a key that is not there changes nothing.
    assert_eq!(
        changes,
        [(slot(0), false), (slot(0), true), (slot(1), true)]
    );
    let error = changes_seen(&mut db, &[put_if("n", a, b"x", Expected::Absent)]).unwrap_err();
    assert!(matches!(error, Error::RevisionMismatch { .. }));
}

#[cfg(feature = "server")]
#[test]
fn a_gate_sees_the_batch_before_it_is_written_and_can_refuse_it() {
    let [a, b, c, _] = SHARED;
    let mut gated = memory();
    let mut direct = memory();
    let mut version = 0;
    for db in [&mut gated, &mut direct] {
        let outcomes = batch(db, &[put("n", a, b"a"), put("n", b, b"b")]).unwrap();
        let KvOutcome::Put { version: written } = outcomes[1] else {
            panic!("a put gives a version");
        };
        version = written;
    }
    let operations = [
        delete("n", a),
        put("n", c, b"c"),
        put_if("n", b, b"b2", Expected::Revision(version)),
        delete("n", "absent"),
    ];
    let id = gated.resolve("default").unwrap();
    let sequence = gated.sequence;
    let before = snapshot(&gated);
    let mut calls = 0;
    // The gate runs once, before anything is written, and the batch is then
    // committed as `write_batch_at` commits it.
    let outcomes = gated
        .kv_store_by_id(id)
        .unwrap()
        .write_batch_gated_at(&operations, NOW, |db, _| {
            calls += 1;
            assert_eq!(db.sequence, sequence);
            assert_eq!(snapshot(db), before);
            Ok::<(), ()>(())
        })
        .unwrap();
    assert_eq!(calls, 1);
    assert_eq!(outcomes, batch(&mut direct, &operations).unwrap());
    assert_eq!(snapshot(&gated), snapshot(&direct));
    assert_eq!(gated.sequence, direct.sequence);

    // A refusal writes nothing and leaves the handle as it was.
    let before = (gated.sequence, snapshot(&gated));
    let refused = gated.kv_store_by_id(id).unwrap().write_batch_gated_at(
        &[put("n", a, b"again")],
        NOW,
        |_, _| Err("over quota"),
    );
    assert!(matches!(refused, Err(Gated::Refused("over quota"))));
    assert_eq!((gated.sequence, snapshot(&gated)), before);

    // A batch that cannot run fails as it does without a gate, and the gate is
    // not asked.
    let mut asked = false;
    let failed = gated.kv_store_by_id(id).unwrap().write_batch_gated_at(
        &[put_if("n", c, b"x", Expected::Absent)],
        NOW,
        |_, _| {
            asked = true;
            Ok::<(), ()>(())
        },
    );
    assert!(matches!(
        failed,
        Err(Gated::Batch(Error::RevisionMismatch { .. }))
    ));
    assert!(!asked);
}

#[test]
fn a_request_error_comes_before_the_state_of_the_handle() {
    let mut db = memory();
    let id = db.resolve("default").unwrap();
    db.poisoned = true;
    let long_key = "k".repeat(MAX_KV_KEY_BYTES + 1);
    for operations in [
        vec![put("a", &long_key, b"1")],
        vec![put("a", "x", b"1"), put("a", "x", b"2")],
    ] {
        assert!(
            matches!(
                super::batch::plan(&db, id, &operations, NOW).err(),
                Some(Error::InvalidKvKey(_))
            ),
            "{operations:?}"
        );
    }
    let too_many = vec![("a", "x"); MAX_KV_GET_MANY_KEYS + 1];
    let blank = [("a", "")];
    for keys in [&too_many[..], &blank[..]] {
        let result = super::batch::visit_many(&db, id, keys, NOW, |_, _| ControlFlow::Continue(()));
        assert!(
            matches!(result, Err(Error::InvalidKvKey(_))),
            "{} keys",
            keys.len()
        );
    }
    // A request that is valid meets the state of the handle.
    assert!(matches!(
        super::batch::plan(&db, id, &[put("a", "x", b"1")], NOW).err(),
        Some(Error::Poisoned)
    ));
    assert!(matches!(
        super::batch::visit_many(
            &db,
            id,
            &[("a", "x")],
            NOW,
            |_, _| ControlFlow::Continue(())
        ),
        Err(Error::Poisoned)
    ));
}

#[test]
fn the_error_messages_name_the_limits_the_constants_set() {
    let mut db = memory();
    let long_key = "k".repeat(MAX_KV_KEY_BYTES);
    let operations: Vec<_> = (0..=MAX_KV_BATCH_OPERATIONS)
        .map(|_| delete("a", &long_key))
        .collect();
    let Err(Error::InvalidKvKey(message)) = batch(&mut db, &operations) else {
        panic!("too many operations is InvalidKvKey");
    };
    assert!(
        message.contains(&MAX_KV_BATCH_OPERATIONS.to_string()),
        "{message}"
    );
    let big = vec![0; MAX_KV_VALUE_BYTES];
    let operations = [
        put("a", "1", &big),
        put("a", "2", &big),
        put("a", "3", &big),
        put("a", "4", &big),
        put("a", "5", &big),
    ];
    let Err(Error::InvalidKvKey(message)) = batch(&mut db, &operations) else {
        panic!("too many value bytes is InvalidKvKey");
    };
    assert!(
        message.contains(&MAX_KV_BATCH_VALUE_BYTES.to_string()),
        "{message}"
    );
    let keys = vec![("a", "x"); MAX_KV_GET_MANY_KEYS + 1];
    let Err(Error::InvalidKvKey(message)) = db.kv_view("default").unwrap().get_many_at(&keys, NOW)
    else {
        panic!("too many keys is InvalidKvKey");
    };
    assert!(
        message.contains(&MAX_KV_GET_MANY_KEYS.to_string()),
        "{message}"
    );
}
