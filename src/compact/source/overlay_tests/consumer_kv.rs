//! Key-value reads over compact records: point reads, prefix listing, probe windows, deadlines and explicit errors.
//!
//! The oracle is the resident store opened from the same image with source installation off. Each entry is an ordinary
//! record (vector `[0.0]`, payload `VKV1`, namespace length, key length, namespace, key, value) at the record id
//! `first_slot(namespace, key) + offset`, `offset < KV_PROBE_SLOTS`; `get` resolves a key to the first record in that
//! window that decodes to the key, and `list` offers a key only through the record `get` resolves.

use super::gates;
use crate::compact::catalog::Precision;
use crate::compact::catalog::tests::kv_windows::{CLOCKS, KEYS, NS, kv_payload, open, raw, slot};
use crate::kv::{KV_PROBE_SLOTS, KvEntry, KvPut, first_slot, probe_for_test, view_for_test};
use crate::{CreateOptions, Database, Error, Metadata, Result};

fn same<T: std::fmt::Debug + PartialEq>(candidate: &Result<T>, oracle: &Result<T>, what: &str) {
    assert_eq!(format!("{candidate:?}"), format!("{oracle:?}"), "{what}");
}

fn keys(entries: &Result<Vec<KvEntry>>) -> Vec<String> {
    entries
        .as_ref()
        .unwrap()
        .iter()
        .map(|entry| entry.key.clone())
        .collect()
}

/// Every point read and a grid of listings agree with the resident store at every clock.
fn compare_everything(candidate: &Database, oracle: &Database, phase: &str) {
    let actual = candidate.kv_view("kv").unwrap();
    let expected = oracle.kv_view("kv").unwrap();
    for &now_ms in &CLOCKS {
        for namespace in [NS, "m", "x"] {
            for key in KEYS {
                same(
                    &actual.get_at(namespace, key, now_ms),
                    &expected.get_at(namespace, key, now_ms),
                    &format!("{phase}: get {namespace}/{key} at {now_ms}"),
                );
            }
            for prefix in ["", "a", "al", "b", "ghost", "z"] {
                for after in [None, Some("alpha"), Some("b"), Some("zz")] {
                    for limit in [1, 2, 3, 100] {
                        same(
                            &actual.list_at(namespace, prefix, after, limit, now_ms),
                            &expected.list_at(namespace, prefix, after, limit, now_ms),
                            &format!(
                                "{phase}: list {namespace}/{prefix}* after {after:?} limit {limit} at {now_ms}"
                            ),
                        );
                    }
                }
            }
        }
    }
}

/// Table J, `compact_kv_point_and_prefix_match_resident`: expiry equality, empty and tombstoned winners, namespace and
/// cursor ordering, on both source precisions, through every kind of overlay edit.
pub(super) fn point_and_prefix_phases() {
    let _gates = gates();
    for precision in [Precision::Float32, Precision::Float16] {
        let (mut candidate, mut oracle) = open(precision);
        compare_everything(&candidate, &oracle, "base");
        let base = candidate.kv_view("kv").unwrap();
        // Hand-computed: the listing is in key order, hides what a deadline hides, never offers look-alikes
        // (`fig`, `ghost`, `gone`) or the second record of a key, and resolves displaced keys (`elder`, `kiwi`).
        let all = |now_ms| keys(&base.list_at(NS, "", None, 100, now_ms));
        assert_eq!(
            all(0),
            [
                "alpha", "alpine", "apple", "banana", "cherry", "date", "elder", "kiwi"
            ]
        );
        assert_eq!(all(99), all(0));
        assert_eq!(
            all(100),
            [
                "alpha", "apple", "banana", "cherry", "date", "elder", "kiwi"
            ]
        );
        assert_eq!(all(199), all(100));
        assert_eq!(
            all(200),
            ["alpha", "apple", "banana", "date", "elder", "kiwi"]
        );
        assert_eq!(all(u64::MAX), all(200));
        assert_eq!(keys(&base.list_at("m", "", None, 100, 0)), ["alpha"]);
        let value = |key: &str| base.get_at(NS, key, 0).unwrap().map(|entry| entry.value);
        assert_eq!(value("cherry"), Some(b"5".to_vec()));
        assert_eq!(value("banana"), Some(b"4".to_vec()));
        assert_eq!(value("elder"), Some(b"displaced".to_vec()));
        assert_eq!(value("kiwi"), Some(b"right".to_vec()));
        assert_eq!(value("ghost"), None);
        assert_eq!(value("fig"), None);
        assert_eq!(value("gone"), None);
        assert_eq!(value("crowded"), None);
        assert_eq!(value("reuse"), None);

        // 1. The real holder of `cherry` is deleted: the record behind it now resolves, in get and in list.
        for db in [&mut candidate, &mut oracle] {
            db.collection_mut("kv")
                .unwrap()
                .delete(first_slot(NS, "cherry"))
                .unwrap();
        }
        compare_everything(&candidate, &oracle, "holder deleted");
        let view = candidate.kv_view("kv").unwrap();
        assert_eq!(
            view.get_at(NS, "cherry", 250).unwrap().unwrap().value,
            b"shadow"
        );
        assert!(keys(&view.list_at(NS, "ch", None, 10, 250)).contains(&"cherry".to_string()));

        // 2. A value rewrite at the same slot replaces the base row and bumps its version.
        let before = candidate
            .kv_view("kv")
            .unwrap()
            .get_at(NS, "alpha", 0)
            .unwrap()
            .unwrap();
        for db in [&mut candidate, &mut oracle] {
            raw(
                db,
                first_slot(NS, "alpha"),
                &kv_payload(NS, "alpha", b"1 again"),
                None,
            );
        }
        compare_everything(&candidate, &oracle, "value rewritten");
        let after = candidate
            .kv_view("kv")
            .unwrap()
            .get_at(NS, "alpha", 0)
            .unwrap()
            .unwrap();
        assert_eq!(after.value, b"1 again");
        assert!(after.version > before.version);

        // 3. A key that only exists in the overlay, with a deadline.
        for db in [&mut candidate, &mut oracle] {
            raw(
                db,
                first_slot(NS, "zebra"),
                &kv_payload(NS, "zebra", b"stripes"),
                Some(300),
            );
        }
        compare_everything(&candidate, &oracle, "overlay-only key");
        let view = candidate.kv_view("kv").unwrap();
        assert_eq!(
            view.get_at(NS, "zebra", 299)
                .unwrap()
                .unwrap()
                .expires_at_ms,
            Some(300)
        );
        assert!(view.get_at(NS, "zebra", 300).unwrap().is_none());

        // 4. A deadline-only write on `apple`: same payload, new deadline.
        for db in [&mut candidate, &mut oracle] {
            raw(
                db,
                first_slot(NS, "apple"),
                &kv_payload(NS, "apple", b"3"),
                Some(150),
            );
        }
        compare_everything(&candidate, &oracle, "deadline only");
        let view = candidate.kv_view("kv").unwrap();
        assert!(view.get_at(NS, "apple", 149).unwrap().is_some());
        assert!(view.get_at(NS, "apple", 150).unwrap().is_none());

        // 5. The displaced `elder` and the rewritten `alpha` are deleted; a tombstone shows nowhere.
        for db in [&mut candidate, &mut oracle] {
            let mut collection = db.collection_mut("kv").unwrap();
            collection.delete(slot(NS, "elder", 3)).unwrap();
            collection.delete(first_slot(NS, "alpha")).unwrap();
        }
        compare_everything(&candidate, &oracle, "tombstones");
        let view = candidate.kv_view("kv").unwrap();
        assert!(view.get_at(NS, "elder", 0).unwrap().is_none());
        assert!(view.get_at(NS, "alpha", 0).unwrap().is_none());
        assert!(!keys(&view.list_at(NS, "", None, 100, 0)).contains(&"alpha".to_string()));
    }
}

#[test]
fn compact_kv_list_offers_only_the_record_get_resolves() {
    let _gates = gates();
    let (candidate, _) = open(Precision::Float32);
    let view = candidate.kv_view("kv").unwrap();
    let listed = view.list_at(NS, "", None, 100, 0).unwrap();
    // Each key once, and exactly as `get` resolves it.
    let mut seen = std::collections::BTreeSet::new();
    for entry in &listed {
        assert!(
            seen.insert(entry.key.clone()),
            "{} is listed twice",
            entry.key
        );
        assert_eq!(
            view.get_at(NS, &entry.key, 0).unwrap().as_ref(),
            Some(entry)
        );
    }
    assert_eq!(listed.len(), 8);
    for key in ["fig", "ghost", "gone", "crowded", "reuse"] {
        assert!(view.get_at(NS, key, 0).unwrap().is_none());
        assert!(!seen.contains(key), "{key} has no record that get resolves");
    }
    // `kiwi` is offered through the second record of its window: the first belongs to another namespace.
    let kiwi = listed.iter().find(|entry| entry.key == "kiwi").unwrap();
    assert_eq!(kiwi.value, b"right");
    // A page that has to skip look-alikes is still full: the limit counts offered entries only.
    assert_eq!(
        keys(&view.list_at(NS, "", Some("cherry"), 2, 0)),
        ["date", "elder"]
    );
    assert_eq!(
        keys(&view.list_at(NS, "", Some("banana"), 1, 0)),
        ["cherry"]
    );
    assert_eq!(keys(&view.list_at(NS, "", Some("elder"), 5, 0)), ["kiwi"]);
}

#[test]
fn compact_kv_probe_window_is_exactly_sixteen_slots() {
    let _gates = gates();
    let (mut candidate, mut oracle) = open(Precision::Float32);
    // `elder` is found three slots in; a key at the last slot of the window is found, one slot further it is not.
    for db in [&mut candidate, &mut oracle] {
        raw(
            db,
            slot(NS, "pear", KV_PROBE_SLOTS - 1),
            &kv_payload(NS, "pear", b"last"),
            None,
        );
        raw(
            db,
            slot(NS, "plum", KV_PROBE_SLOTS),
            &kv_payload(NS, "plum", b"too far"),
            None,
        );
    }
    let actual = candidate.kv_view("kv").unwrap();
    let expected = oracle.kv_view("kv").unwrap();
    assert_eq!(
        actual.get_at(NS, "pear", 0).unwrap().unwrap().value,
        b"last"
    );
    assert!(actual.get_at(NS, "plum", 0).unwrap().is_none());
    for key in ["pear", "plum", "elder"] {
        same(
            &actual.get_at(NS, key, 0),
            &expected.get_at(NS, key, 0),
            key,
        );
    }
    assert!(!keys(&actual.list_at(NS, "p", None, 10, 0)).contains(&"plum".to_string()));
}

#[test]
fn compact_kv_view_checks_health_then_dimension_then_arguments() {
    let _gates = gates();
    let (mut candidate, mut oracle) = open(Precision::Float32);
    // A compact collection of another dimension is not a key-value collection.
    for db in [&candidate, &oracle] {
        assert!(matches!(db.kv_view("default"), Err(Error::NotKvCollection)));
    }
    let view = candidate.kv_view("kv").unwrap();
    let reference = oracle.kv_view("kv").unwrap();
    let long_namespace = "n".repeat(crate::kv::MAX_KV_NAMESPACE_BYTES + 1);
    let long_key = "k".repeat(crate::kv::MAX_KV_KEY_BYTES + 1);
    let get = |view: &crate::kv::KvView<'_>, namespace: &str, key: &str| {
        view.get_at(namespace, key, 0).map(|_| ())
    };
    for (namespace, key) in [
        ("", "a"),
        (NS, ""),
        (long_namespace.as_str(), "a"),
        (NS, long_key.as_str()),
        ("", ""),
    ] {
        let result = get(&view, namespace, key);
        assert!(
            matches!(result, Err(Error::InvalidKvKey(_))),
            "{namespace}/{key}: {result:?}"
        );
        same(&result, &get(&reference, namespace, key), "get validation");
    }
    let list = |view: &crate::kv::KvView<'_>, namespace: &str, limit: usize| {
        view.list_at(namespace, "", None, limit, 0).map(|_| ())
    };
    for (namespace, limit) in [
        ("", 1),
        (long_namespace.as_str(), 1),
        (NS, 0),
        (NS, crate::kv::MAX_KV_LIST_LIMIT + 1),
        ("", 0),
    ] {
        let result = list(&view, namespace, limit);
        assert!(
            matches!(result, Err(Error::InvalidKvKey(_))),
            "{namespace}/{limit}: {result:?}"
        );
        same(
            &result,
            &list(&reference, namespace, limit),
            "list validation",
        );
    }
    assert!(list(&view, NS, crate::kv::MAX_KV_LIST_LIMIT).is_ok());
    candidate.poisoned = true;
    oracle.poisoned = true;
    for db in [&candidate, &oracle] {
        assert!(matches!(db.kv_view("kv"), Err(Error::Poisoned)));
        assert!(matches!(db.kv_view("default"), Err(Error::Poisoned)));
    }
}

#[test]
fn compact_kv_reads_report_base_row_errors() {
    let _gates = gates();
    let (candidate, _) = open(Precision::Float32);
    let id = candidate.collection("kv").unwrap().id();
    let crate::records::CollectionRecords::Compact { view, .. } = &candidate.state(id).records
    else {
        panic!("compact source required")
    };
    let source = view.source().unwrap();
    let slot = first_slot(NS, "apple");
    let (block, row) = source.row(slot).unwrap();
    let cell = &source.cells[source.block_starts[block] + row];
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = cell.initializing.lock().unwrap();
        panic!("poison compact record cell lock");
    }));
    let reader = candidate.kv_view("kv").unwrap();
    for result in [
        reader.get_at(NS, "apple", 0).map(|_| ()),
        reader.list_at(NS, "", None, 10, 0).map(|_| ()),
        reader.list_at(NS, "ap", None, 1, 0).map(|_| ()),
    ] {
        assert!(
            matches!(
                &result,
                Err(Error::InvalidQuery("compact record cell lock poisoned"))
            ),
            "{result:?}"
        );
    }
    // Rows that are not behind the poisoned cell are still readable by key.
    assert!(reader.get_at(NS, "banana", 0).unwrap().is_some());
}

/// A probe as plain values: the record `get` resolves, the slot a new entry could take and the compare-and-set token.
fn probed(
    db: &Database,
    namespace: &str,
    key: &str,
    now_ms: u64,
) -> std::result::Result<crate::kv::ProbeSeen, String> {
    let collection = db.collection("kv").unwrap().id();
    probe_for_test(db, collection, namespace, key, now_ms).map_err(|error| format!("{error:?}"))
}

fn compare_every_probe(candidate: &Database, oracle: &Database, phase: &str) {
    for &now_ms in &CLOCKS {
        for namespace in [NS, "m", "x"] {
            for key in KEYS {
                assert_eq!(
                    probed(candidate, namespace, key, now_ms),
                    probed(oracle, namespace, key, now_ms),
                    "{phase}: probe {namespace}/{key} at {now_ms}"
                );
            }
        }
    }
}

/// The probe behind `get`, `put`, `delete` and `list`: which record a key resolves to, whether it is due, its revision
/// (the compare-and-set token) and the first slot a new entry could take.
#[test]
fn compact_probe_matches_resident_in_every_window() {
    let _gates = gates();
    for precision in [Precision::Float32, Precision::Float16] {
        let (mut candidate, mut oracle) = open(precision);
        compare_every_probe(&candidate, &oracle, "base");

        let at = |key: &str, now_ms| probed(&candidate, NS, key, now_ms).unwrap();
        // A live key: found at its first slot, nothing free in front of it, its revision is the token.
        let (found, free, live_version) = at("apple", 0);
        let (apple_slot, apple_revision, due) = found.unwrap();
        assert_eq!((apple_slot, due), (slot(NS, "apple", 0), false));
        assert_eq!((live_version, free), (Some(apple_revision), None));
        // A due key is still found, but it has no live version: `Absent` may take its slot.
        let (found, _, live_version) = at("alpine", 100);
        assert_eq!(
            found.map(|f| (f.0, f.2)),
            Some((slot(NS, "alpine", 0), true))
        );
        assert_eq!(live_version, None);
        // Displaced behind three foreign records: found at offset 3, and none of the three is free.
        let (found, free, _) = at("elder", 0);
        assert_eq!(found.map(|f| f.0), Some(slot(NS, "elder", 3)));
        assert_eq!(free, None);
        // Another namespace's record with the same key is passed over.
        assert_eq!(at("kiwi", 0).0.map(|f| f.0), Some(slot(NS, "kiwi", 1)));
        // An entry that is not in its own window is not found, however it is stored.
        for key in ["gone", "fig", "ghost"] {
            assert_eq!(at(key, 0).0, None, "{key}");
        }
        // The first slot that is empty, or holds a due entry of another key; a live other key and a foreign record
        // are not free.
        assert_eq!(at("reuse", 0).1, Some((slot(NS, "reuse", 2), None)));
        assert_eq!(at("reuse", 49).1, Some((slot(NS, "reuse", 2), None)));
        let reusable = at("reuse", 50).1.unwrap();
        assert_eq!(reusable.0, slot(NS, "reuse", 0));
        assert!(reusable.1.is_some_and(|revision| revision > 0));
        // A window of foreign records has no free slot.
        assert_eq!((at("crowded", 0).0, at("crowded", 0).1), (None, None));

        // The overlay: a tombstoned holder, a due entry rewritten as live, a rewritten value.
        for db in [&mut candidate, &mut oracle] {
            db.collection_mut("kv")
                .unwrap()
                .delete(slot(NS, "cherry", 0))
                .unwrap();
            raw(
                db,
                slot(NS, "reuse", 0),
                &kv_payload(NS, "gone", b"new"),
                Some(1_000),
            );
            raw(
                db,
                slot(NS, "apple", 0),
                &kv_payload(NS, "apple", b"3 again"),
                None,
            );
        }
        compare_every_probe(&candidate, &oracle, "overlay");
        let at = |key: &str, now_ms| probed(&candidate, NS, key, now_ms).unwrap();
        // The tombstone shows the record behind the holder, five slots in.
        assert_eq!(at("cherry", 0).0.map(|f| f.0), Some(slot(NS, "cherry", 5)));
        // The rewritten record has a newer revision than the base row had.
        assert!(at("apple", 0).2.unwrap() > apple_revision);
        // The slot of `reuse` is live again until its new deadline.
        assert_eq!(at("reuse", 50).1, Some((slot(NS, "reuse", 2), None)));
        assert_eq!(
            at("reuse", 1_000).1.map(|f| f.0),
            Some(slot(NS, "reuse", 0))
        );
    }
}

/// A view cannot be handed out by a poisoned handle (it borrows the database), so the views are built directly:
/// every one of their reads answers `Poisoned` before it looks at its arguments.
#[test]
fn compact_views_check_health_before_anything_else() {
    let _gates = gates();
    let (mut candidate, mut oracle) = open(Precision::Float32);
    let candidate_id = candidate.collection("kv").unwrap().id();
    let oracle_id = oracle.collection("kv").unwrap().id();
    candidate.poisoned = true;
    oracle.poisoned = true;
    for (db, id) in [(&candidate, candidate_id), (&oracle, oracle_id)] {
        let view = view_for_test(db, id);
        assert!(matches!(view.get_at("", "", 0), Err(Error::Poisoned)));
        assert!(matches!(
            view.list_at("", "", None, 0, 0),
            Err(Error::Poisoned)
        ));
        assert!(matches!(
            view.visit_at("", "", None, 0, 0, |_| std::ops::ControlFlow::Break(())),
            Err(Error::Poisoned)
        ));
        assert!(matches!(view.get_at(NS, "apple", 0), Err(Error::Poisoned)));
    }
}

/// A key-value deadline is the second public way to give a record an expiry. Like `put_with_expiry` it is refused for an
/// entry that a graph node attaches, and it writes nothing. This pins the invariant behind
/// `pa-graph-evidence-wall-clock` (an attached record never carries a deadline) on the path that
/// `an_attached_record_never_carries_a_deadline_on_compact_or_resident_records` does not enumerate.
#[test]
fn an_attached_key_value_entry_cannot_be_given_a_deadline() {
    let mut db = Database::create_in_memory(1, CreateOptions::default()).unwrap();
    let version = db
        .kv_store("kv")
        .unwrap()
        .put_at("agent", "a", b"one", KvPut::default(), 0)
        .unwrap();
    // The collection was empty, so the entry holds the first slot of its window.
    let slot = first_slot("agent", "a");
    db.create_graph("g").unwrap();
    {
        let mut graph = db.graph_mut("g").unwrap();
        graph.create_node("n", &[], &Metadata::new()).unwrap();
        graph.attach("n", "kv", slot).unwrap();
    }
    let mut store = db.kv_store("kv").unwrap();
    for deadline in [0, 1, 4_102_444_800_000, u64::MAX] {
        let put = KvPut {
            expires_at_ms: Some(deadline),
            ..KvPut::default()
        };
        let refused = store.put_at("agent", "a", b"two", put, 0);
        assert!(
            matches!(refused, Err(Error::GraphAttachedRecordExpiry)),
            "{deadline}: {refused:?}"
        );
    }
    let entry = store.view().get_at("agent", "a", 0).unwrap().unwrap();
    assert_eq!(
        (entry.value.as_slice(), entry.version, entry.expires_at_ms),
        (&b"one"[..], version, None)
    );
    assert_eq!(db.collection("kv").unwrap().expires_at(slot).unwrap(), None);
}
