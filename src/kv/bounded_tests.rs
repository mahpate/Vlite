//! Bounded candidate selection of `list_at` / `visit_at`.
//!
//! Every expectation below comes from the documented contract: a listing offers
//! each live key once, as `get` resolves it, in unsigned UTF-8 byte order,
//! strictly after `after`, at most `limit`. The oracle is built from `get_at`
//! over a known universe of keys, never from the selection code.
use super::*;
use crate::{CreateOptions, Mutation};

fn memory() -> Database {
    Database::create_in_memory(1, CreateOptions::default()).unwrap()
}

/// Writes `keys` of `namespace` as KV records at their first slot, one frame
/// per 4,096 records.
fn bulk(db: &mut Database, namespace: &str, keys: impl IntoIterator<Item = String>) {
    db.kv_store("default").unwrap();
    let mut batch = Vec::new();
    for key in keys {
        batch.push(Mutation::Put {
            id: first_slot(namespace, &key),
            vector: PLACEHOLDER.to_vec(),
            payload: encode(namespace, &key, key.as_bytes()),
        });
        if batch.len() == crate::MAX_BATCH_OPERATIONS {
            db.write_batch(&batch).unwrap();
            batch.clear();
        }
    }
    db.write_batch(&batch).unwrap();
}

/// A KV-shaped record at an arbitrary record ID: a look-alike when the ID is
/// not the key's resolved slot.
fn plant(db: &mut Database, id: u64, namespace: &str, key: &str, value: &str) {
    db.collection_mut("default")
        .unwrap()
        .put(id, &PLACEHOLDER, &encode(namespace, key, value.as_bytes()))
        .unwrap();
}

fn mark_reset() {
    CANDIDATE_HIGH_WATER.with(|mark| mark.set(0));
}

fn mark() -> usize {
    CANDIDATE_HIGH_WATER.with(std::cell::Cell::get)
}

fn probes_reset() {
    PROBES.with(|probes| probes.set(0));
}

fn probes() -> usize {
    PROBES.with(std::cell::Cell::get)
}

fn keys(entries: &[KvEntry]) -> Vec<&str> {
    entries.iter().map(|entry| entry.key.as_str()).collect()
}

fn visited(
    view: &KvView<'_>,
    namespace: &str,
    prefix: &str,
    after: Option<&str>,
    limit: usize,
    now_ms: u64,
) -> Vec<(KvEntry, bool)> {
    let mut seen = Vec::new();
    view.visit_at(namespace, prefix, after, limit, now_ms, |entry| {
        seen.push((
            KvEntry {
                key: entry.key.to_owned(),
                value: entry.value.to_vec(),
                version: entry.version,
                expires_at_ms: entry.expires_at_ms,
            },
            entry.has_next,
        ));
        ControlFlow::Continue(())
    })
    .unwrap();
    seen
}

#[test]
fn the_candidate_set_never_holds_more_than_limit_plus_one_keys() {
    let mut db = memory();
    bulk(&mut db, "n", (0..2_000).map(|index| format!("k{index:05}")));
    // Look-alikes come first in scan order (small record IDs) and are probed
    // but never kept.
    for id in 1..=40u64 {
        let key = format!("k{:05}", id * 7);
        assert!(
            id.wrapping_sub(first_slot("n", &key)) >= KV_PROBE_SLOTS,
            "{key}: inside the window"
        );
        plant(&mut db, id, "n", &key, "spoof");
    }
    let view = db.kv_view("default").unwrap();
    for limit in [1, 2, 7, 16, 100, 1_999, 2_000, 2_001] {
        mark_reset();
        let entries = view.list_at("n", "", None, limit, 0).unwrap();
        assert_eq!(entries.len(), limit.min(2_000), "limit {limit}");
        assert_eq!(
            mark(),
            (limit + 1).min(2_000),
            "limit {limit}: the scan keeps the entries to return plus one lookahead key"
        );
    }
    mark_reset();
    let all = view.list_at("n", "", None, MAX_KV_LIST_LIMIT, 0).unwrap();
    assert_eq!(all.len(), 2_000);
    assert_eq!(mark(), 2_000);
}

#[test]
fn the_candidate_set_is_capped_at_the_largest_limit_plus_one() {
    let mut db = memory();
    bulk(
        &mut db,
        "n",
        (0..MAX_KV_LIST_LIMIT + 300).map(|index| format!("{index:06}")),
    );
    let view = db.kv_view("default").unwrap();
    mark_reset();
    let entries = view.list_at("n", "", None, MAX_KV_LIST_LIMIT, 0).unwrap();
    assert_eq!(entries.len(), MAX_KV_LIST_LIMIT);
    assert_eq!(mark(), MAX_KV_LIST_LIMIT + 1);
    assert_eq!(entries[0].key, "000000");
    assert_eq!(entries[MAX_KV_LIST_LIMIT - 1].key, "009999");
}

// The scan keeps the smallest keys, not the first ones it meets: record IDs are
// hashes, so key order is unrelated to scan order.
#[test]
fn the_selection_keeps_the_smallest_keys_whatever_the_scan_order() {
    let mut db = memory();
    let all: Vec<String> = (0..300).map(|index| format!("key-{index:04}")).collect();
    bulk(&mut db, "n", all.iter().cloned());
    let view = db.kv_view("default").unwrap();
    for limit in [1, 2, 3, 10, 150, 299, 300, 301] {
        let entries = view.list_at("n", "", None, limit, 0).unwrap();
        let expected: Vec<&str> = all.iter().take(limit).map(String::as_str).collect();
        assert_eq!(keys(&entries), expected, "limit {limit}");
        let behind = view.list_at("n", "", Some("key-0149"), limit, 0).unwrap();
        let expected: Vec<&str> = all
            .iter()
            .skip(150)
            .take(limit)
            .map(String::as_str)
            .collect();
        assert_eq!(keys(&behind), expected, "after key-0149, limit {limit}");
    }
}

// A look-alike must not use up room that a real entry of a later key needs:
// every look-alike key sorts before the real keys it could displace, and the
// scan meets all of them first (their record IDs are the smallest).
#[test]
fn shadowed_candidates_do_not_use_up_the_limit() {
    let mut db = memory();
    bulk(&mut db, "n", ["a", "b", "c", "d", "e"].map(str::to_owned));
    for (id, key) in [
        (1u64, "a"),
        (2, "a"),
        (3, "b"),
        (4, "b"),
        (5, "a0"),
        (6, "a1"),
    ] {
        assert!(
            id.wrapping_sub(first_slot("n", key)) >= KV_PROBE_SLOTS,
            "{key}: inside the window"
        );
        plant(&mut db, id, "n", key, "spoof");
    }
    let view = db.kv_view("default").unwrap();
    let real = ["a", "b", "c", "d", "e"];
    for limit in 1..=7 {
        let expected = &real[..limit.min(real.len())];
        let entries = view.list_at("n", "", None, limit, 0).unwrap();
        assert_eq!(keys(&entries), expected, "limit {limit}");
        assert!(
            entries.iter().all(|entry| entry.value != b"spoof"),
            "limit {limit}: a look-alike was listed"
        );
        let seen = visited(&view, "n", "", None, limit, 0);
        let next: Vec<bool> = seen.iter().map(|(_, next)| *next).collect();
        let total = real.len().min(limit + 1);
        let want: Vec<bool> = (0..expected.len()).map(|index| index + 1 < total).collect();
        assert_eq!(next, want, "limit {limit}: has_next");
    }
}

#[test]
fn visit_reports_whether_the_listing_continues() {
    let mut db = memory();
    bulk(&mut db, "n", ["a", "b", "c"].map(str::to_owned));
    let view = db.kv_view("default").unwrap();
    let flags = |limit: usize| -> Vec<(String, bool)> {
        visited(&view, "n", "", None, limit, 0)
            .into_iter()
            .map(|(entry, next)| (entry.key, next))
            .collect()
    };
    let pair = |key: &str, next: bool| (key.to_owned(), next);
    assert_eq!(flags(1), [pair("a", true)]);
    assert_eq!(flags(2), [pair("a", true), pair("b", true)]);
    assert_eq!(
        flags(3),
        [pair("a", true), pair("b", true), pair("c", false)]
    );
    assert_eq!(
        flags(4),
        [pair("a", true), pair("b", true), pair("c", false)]
    );
    assert!(visited(&view, "n", "zz", None, 5, 0).is_empty());
    assert!(visited(&view, "n", "", Some("c"), 5, 0).is_empty());
    let behind = visited(&view, "n", "", Some("a"), 1, 0);
    assert_eq!(behind.len(), 1);
    assert_eq!((behind[0].0.key.as_str(), behind[0].1), ("b", true));
}

#[test]
fn a_visitor_that_breaks_is_not_called_again() {
    let mut db = memory();
    bulk(&mut db, "n", (0..20).map(|index| format!("k{index:02}")));
    let view = db.kv_view("default").unwrap();
    for stop_after in [1, 2, 7, 20] {
        let mut calls = Vec::new();
        view.visit_at("n", "", None, 100, 0, |entry| {
            calls.push(entry.key.to_owned());
            if calls.len() == stop_after {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .unwrap();
        let expected: Vec<String> = (0..stop_after)
            .map(|index| format!("k{index:02}"))
            .collect();
        assert_eq!(calls, expected, "stop after {stop_after}");
    }
}

#[test]
fn visited_entries_lend_the_stored_value_version_and_expiry() {
    let mut db = memory();
    let mut store = db.kv_store("default").unwrap();
    let plain = store
        .put_at("n", "plain", b"\x00\xffbytes", KvPut::default(), 0)
        .unwrap();
    let options = KvPut {
        expected: Expected::Any,
        expires_at_ms: Some(9_000),
    };
    let timed = store.put_at("n", "timed", b"", options, 0).unwrap();
    let seen = visited(&db.kv_view("default").unwrap(), "n", "", None, 10, 8_999);
    assert_eq!(
        seen,
        [
            (
                KvEntry {
                    key: "plain".into(),
                    value: b"\x00\xffbytes".to_vec(),
                    version: plain,
                    expires_at_ms: None,
                },
                true
            ),
            (
                KvEntry {
                    key: "timed".into(),
                    value: Vec::new(),
                    version: timed,
                    expires_at_ms: Some(9_000),
                },
                false
            ),
        ]
    );
}

#[test]
fn one_clock_applies_to_every_candidate_and_an_expiry_equal_to_now_is_due() {
    let mut db = memory();
    let mut store = db.kv_store("default").unwrap();
    for (key, expiry) in [("a", 4_999), ("b", 5_000), ("c", 5_001), ("d", u64::MAX)] {
        let options = KvPut {
            expected: Expected::Any,
            expires_at_ms: Some(expiry),
        };
        store.put_at("n", key, b"v", options, 0).unwrap();
    }
    store.put_at("n", "e", b"v", KvPut::default(), 0).unwrap();
    let view = db.kv_view("default").unwrap();
    let live = |now: u64, limit: usize| -> Vec<String> {
        visited(&view, "n", "", None, limit, now)
            .into_iter()
            .map(|(entry, _)| entry.key)
            .collect()
    };
    assert_eq!(live(4_998, 10), ["a", "b", "c", "d", "e"]);
    assert_eq!(live(4_999, 10), ["b", "c", "d", "e"]);
    assert_eq!(live(5_000, 10), ["c", "d", "e"]);
    assert_eq!(live(5_001, 10), ["d", "e"]);
    // Due entries leave room for later live ones.
    assert_eq!(live(5_000, 2), ["c", "d"]);
    assert_eq!(live(5_001, 1), ["d"]);
    let listed = view.list_at("n", "", None, 10, 5_000).unwrap();
    assert_eq!(keys(&listed), ["c", "d", "e"]);
}

#[test]
fn after_prefix_and_namespace_select_exactly_what_the_contract_says() {
    let mut db = memory();
    let mut store = db.kv_store("default").unwrap();
    for (namespace, key) in [
        ("n", "app/1"),
        ("n", "app/2"),
        ("n", "app/22"),
        ("n", "apple"),
        ("n", "b"),
        ("m", "app/1"),
        ("m", "app/3"),
    ] {
        store
            .put_at(namespace, key, key.as_bytes(), KvPut::default(), 0)
            .unwrap();
    }
    let view = db.kv_view("default").unwrap();
    let list = |namespace: &str, prefix: &str, after: Option<&str>, limit: usize| {
        view.list_at(namespace, prefix, after, limit, 0).unwrap()
    };
    assert_eq!(
        keys(&list("n", "app/", None, 10)),
        ["app/1", "app/2", "app/22"]
    );
    assert_eq!(
        keys(&list("n", "app", None, 10)),
        ["app/1", "app/2", "app/22", "apple"]
    );
    // `after` is exclusive, also when it is a key that exists or a prefix of one.
    assert_eq!(
        keys(&list("n", "app/", Some("app/1"), 10)),
        ["app/2", "app/22"]
    );
    assert_eq!(keys(&list("n", "app/", Some("app/2"), 10)), ["app/22"]);
    assert_eq!(
        keys(&list("n", "app/", Some("app/"), 10)),
        ["app/1", "app/2", "app/22"]
    );
    assert!(list("n", "app/", Some("app/22"), 10).is_empty());
    assert_eq!(keys(&list("m", "", None, 10)), ["app/1", "app/3"]);
    assert!(list("other", "", None, 10).is_empty());
    assert_eq!(keys(&list("n", "", Some("app/22"), 1)), ["apple"]);
}

// Unsigned UTF-8 byte order after the shared first byte 'k': 41, 61, 7a, 7f,
// c2 80, df bf, e0 a0 80, ee 80 80, f0 90 80 80. UTF-16 code unit order would
// put U+10000 (D800 DC00) before U+E000, and a case-folding order would tie
// "kA" with "ka".
#[test]
fn keys_are_ordered_by_utf8_bytes_for_every_limit() {
    let sorted = [
        "kA",
        "ka",
        "kz",
        "k\u{7f}",
        "k\u{80}",
        "k\u{7ff}",
        "k\u{800}",
        "k\u{e000}",
        "k\u{10000}",
    ];
    let scrambled = [
        "k\u{10000}",
        "kz",
        "k\u{80}",
        "kA",
        "k\u{e000}",
        "k\u{7f}",
        "ka",
        "k\u{800}",
        "k\u{7ff}",
    ];
    let mut db = memory();
    bulk(&mut db, "n", scrambled.map(str::to_owned));
    let view = db.kv_view("default").unwrap();
    for limit in 1..=sorted.len() + 1 {
        let entries = view.list_at("n", "", None, limit, 0).unwrap();
        let expected = &sorted[..limit.min(sorted.len())];
        assert_eq!(keys(&entries), expected, "limit {limit}");
    }
    for (index, key) in sorted.iter().enumerate().take(sorted.len() - 1) {
        let entries = view.list_at("n", "", Some(key), 100, 0).unwrap();
        assert_eq!(keys(&entries), &sorted[index + 1..], "after {key:?}");
    }
}

// The work of a scan is bounded too: a candidate is probed only when it can
// become an entry. Records of another namespace, keys that miss the prefix or
// do not follow the cursor, entries that are due, records outside their key's
// window (`look_alikes_outside_their_window_are_never_probed`), and (once
// `limit + 1` keys are held) keys that are not smaller than the largest one
// held cost no probe.
#[test]
fn only_candidates_that_can_be_entries_are_probed() {
    let mut db = memory();
    bulk(&mut db, "m", (0..400).map(|index| format!("a{index:04}")));
    bulk(&mut db, "n", (0..300).map(|index| format!("b{index:04}")));
    bulk(&mut db, "n", (0..50).map(|index| format!("a{index:04}")));
    let view = db.kv_view("default").unwrap();

    // 450 records carry an "a" key, 50 of them in namespace n.
    probes_reset();
    let entries = view.list_at("n", "a", None, 1_000, 0).unwrap();
    assert_eq!(entries.len(), 50);
    assert_eq!(probes(), 50, "another namespace and another prefix");

    probes_reset();
    let entries = view.list_at("n", "a", Some("a0009"), 1_000, 0).unwrap();
    assert_eq!(entries.len(), 40);
    assert_eq!(probes(), 40, "keys up to the cursor");

    // Past the largest key of the prefix there is nothing to probe.
    probes_reset();
    assert!(
        view.list_at("n", "a", Some("a0049"), 1_000, 0)
            .unwrap()
            .is_empty()
    );
    assert!(view.list_at("n", "zzz", None, 1_000, 0).unwrap().is_empty());
    assert_eq!(probes(), 0);

    // 300 candidates and room for two: the scan meets them in record ID order,
    // which is unrelated to key order, so only the few that are smaller than
    // the largest key held at the time are probed.
    probes_reset();
    let entries = view.list_at("n", "b", None, 1, 0).unwrap();
    assert_eq!(keys(&entries), ["b0000"]);
    assert!(
        (2..=40).contains(&probes()),
        "{} probes for 300 candidates",
        probes()
    );

    // Due entries are not candidates.
    let mut store = db.kv_store("default").unwrap();
    for index in 0..30 {
        let options = KvPut {
            expected: Expected::Any,
            expires_at_ms: Some(1_000),
        };
        store
            .put_at("n", &format!("c{index:02}"), b"v", options, 0)
            .unwrap();
    }
    let view = db.kv_view("default").unwrap();
    probes_reset();
    assert!(view.list_at("n", "c", None, 100, 1_000).unwrap().is_empty());
    assert_eq!(probes(), 0, "due entries");
    probes_reset();
    assert_eq!(view.list_at("n", "c", None, 100, 999).unwrap().len(), 30);
    assert_eq!(probes(), 30);
}

#[test]
fn limit_and_namespace_are_validated_before_any_scan() {
    let mut db = memory();
    bulk(&mut db, "n", ["a".to_owned()]);
    let view = db.kv_view("default").unwrap();
    mark_reset();
    for limit in [0, MAX_KV_LIST_LIMIT + 1, usize::MAX - 1] {
        for result in [
            view.list_at("n", "", None, limit, 0).map(|_| ()),
            view.visit_at("n", "", None, limit, 0, |_| ControlFlow::Continue(())),
        ] {
            assert!(
                matches!(result, Err(Error::InvalidKvKey(_))),
                "limit {limit}: {result:?}"
            );
        }
    }
    for namespace in ["", &"x".repeat(MAX_KV_NAMESPACE_BYTES + 1)] {
        assert!(matches!(
            view.visit_at(namespace, "", None, 1, 0, |_| ControlFlow::Continue(())),
            Err(Error::InvalidKvKey(_))
        ));
    }
    assert_eq!(mark(), 0, "an invalid request must not scan");
}

// A request with both a bad namespace and a bad limit is refused for its
// namespace, as in v0.7.0: the namespace is checked first.
#[test]
fn a_bad_namespace_is_reported_before_a_bad_limit() {
    let mut db = memory();
    bulk(&mut db, "n", ["a".to_owned()]);
    let view = db.kv_view("default").unwrap();
    let namespace = Error::InvalidKvKey("namespace must be 1 to 255 bytes").to_string();
    let limit = Error::InvalidKvKey("limit must be between 1 and 10000").to_string();
    assert_ne!(namespace, limit);
    for bad_limit in [0, MAX_KV_LIST_LIMIT + 1] {
        for result in [
            view.list_at("", "", None, bad_limit, 0).map(|_| ()),
            view.visit_at("", "", None, bad_limit, 0, |_| ControlFlow::Continue(())),
        ] {
            assert_eq!(result.unwrap_err().to_string(), namespace, "{bad_limit}");
        }
        let result = view.list_at("n", "", None, bad_limit, 0);
        assert_eq!(result.unwrap_err().to_string(), limit, "{bad_limit}");
    }
}

// What a scan holds is bounded by the limit, not by the collection: the
// candidate set is allocated for `limit + 1` keys, however many records the
// collection has. Measured as the heap the scan holds while it visits.
#[test]
fn a_scan_allocates_for_the_limit_not_for_the_collection() {
    use crate::test_globals::{counted, live};
    let mut db = memory();
    bulk(
        &mut db,
        "n",
        (0..MAX_KV_LIST_LIMIT + 300).map(|index| format!("{index:06}")),
    );
    let view = db.kv_view("default").unwrap();
    for limit in [1, 2, 10] {
        let (held, _) = counted(|| {
            let mut held = 0;
            view.visit_at("n", "", None, limit, 0, |_| {
                held = held.max(live());
                ControlFlow::Continue(())
            })
            .unwrap();
            held
        });
        // 24 bytes per candidate key and nothing else: a few hundred bytes,
        // where one slot per record would be 240 KB.
        assert!(held > 0 && held < 4_096, "limit {limit}: {held} bytes held");
    }
}

// A record that decodes to a key but lies outside the key's window can never be
// that key's entry: it costs no probe, however many there are and whenever the
// scan meets them. Here 1,000 of them are met first, while the candidate set is
// still empty, and every one sorts after both real keys.
#[test]
fn look_alikes_outside_their_window_are_never_probed() {
    let mut db = memory();
    bulk(&mut db, "n", ["a", "b"].map(str::to_owned));
    for id in 1..=1_000u64 {
        let key = format!("z{id:04}");
        assert!(
            id.wrapping_sub(first_slot("n", &key)) >= KV_PROBE_SLOTS,
            "{key}: inside the window"
        );
        plant(&mut db, id, "n", &key, "spoof");
    }
    // One slot past the window of each real key.
    for key in ["a", "b"] {
        let id = first_slot("n", key).wrapping_add(KV_PROBE_SLOTS);
        plant(&mut db, id, "n", key, "spoof");
    }
    let view = db.kv_view("default").unwrap();
    probes_reset();
    let entries = view.list_at("n", "", None, 1, 0).unwrap();
    assert_eq!(keys(&entries), ["a"]);
    assert_eq!(entries[0].value, b"a");
    assert_eq!(probes(), 2, "only the two real keys can be entries");
    probes_reset();
    let entries = view.list_at("n", "", None, 10, 0).unwrap();
    assert_eq!(keys(&entries), ["a", "b"]);
    assert_eq!(probes(), 2);
}

// The window is exactly `KV_PROBE_SLOTS` long: the last slot resolves, as `get`
// shows, and the one after it does not, and costs no probe either. A lookup
// alone could not tell the two apart: `probe` rejects the look-alike too.
#[test]
fn the_last_slot_of_a_window_is_an_entry_and_the_next_one_is_not() {
    let mut db = memory();
    let inside = first_slot("n", "edge").wrapping_add(KV_PROBE_SLOTS - 1);
    let outside = first_slot("n", "over").wrapping_add(KV_PROBE_SLOTS);
    plant(&mut db, inside, "n", "edge", "in");
    plant(&mut db, outside, "n", "over", "out");
    let view = db.kv_view("default").unwrap();
    assert_eq!(
        view.get_at("n", "edge", 0)
            .unwrap()
            .map(|entry| entry.value),
        Some(b"in".to_vec())
    );
    assert_eq!(view.get_at("n", "over", 0).unwrap(), None);
    probes_reset();
    let entries = view.list_at("n", "", None, 10, 0).unwrap();
    assert_eq!(keys(&entries), ["edge"]);
    assert_eq!(entries[0].value, b"in");
    assert_eq!(probes(), 1, "only the key inside its window is probed");
}

// The universe is small so that the generated state is dense with look-alikes.
#[test]
fn bounded_listing_equals_a_get_derived_oracle_for_every_limit_prefix_after_and_clock() {
    let names = [
        "a",
        "a/1",
        "a/2",
        "a/22",
        "ab",
        "b",
        "b/1",
        "z",
        "Z",
        "\u{e9}",
        "\u{e9}1",
        "\u{e000}",
        "\u{10000}",
        "key",
        "key/a",
        "key/b",
        "key/c",
    ];
    let mut universe: Vec<(&str, &str)> = Vec::new();
    for namespace in ["n", "m"] {
        universe.extend(names.iter().map(|key| (namespace, *key)));
    }
    let mut db = memory();
    db.kv_store("default").unwrap();
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut random = move |bound: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % bound
    };
    for round in 0..1_500u32 {
        let (namespace, key) = universe[random(universe.len() as u64) as usize];
        // Offsets -4..=19 around the first slot: inside the window and just outside it.
        let id = first_slot(namespace, key)
            .wrapping_add(random(24))
            .wrapping_sub(4);
        let value = format!("v{round}");
        let expiry = 1 + random(9_000);
        match random(7) {
            0 => {
                db.collection_mut("default")
                    .unwrap()
                    .put(id, &PLACEHOLDER, b"foreign")
                    .unwrap();
            }
            1 => plant(&mut db, id, namespace, key, &value),
            2 => {
                db.collection_mut("default")
                    .unwrap()
                    .put_with_expiry(
                        id,
                        &PLACEHOLDER,
                        &encode(namespace, key, value.as_bytes()),
                        &Metadata::new(),
                        Some(expiry),
                    )
                    .unwrap();
            }
            3 => {
                let options = KvPut {
                    expected: Expected::Any,
                    expires_at_ms: Some(expiry),
                };
                let _ = db.kv_store("default").unwrap().put_at(
                    namespace,
                    key,
                    value.as_bytes(),
                    options,
                    0,
                );
            }
            4 => {
                let _ = db.kv_store("default").unwrap().put_at(
                    namespace,
                    key,
                    value.as_bytes(),
                    KvPut::default(),
                    0,
                );
            }
            5 => {
                let _ = db
                    .kv_store("default")
                    .unwrap()
                    .delete_at(namespace, key, Expected::Any, 0);
            }
            _ => {
                db.collection_mut("default").unwrap().delete(id).unwrap();
            }
        }
    }
    // The generated state must hold keys with more than one record decoding to
    // them, or the oracle would not exercise shadowing at all.
    let raw = db.collection("default").unwrap();
    let shadowed = universe
        .iter()
        .filter(|(namespace, key)| {
            let wanted = encode(namespace, key, b"");
            let base = first_slot(namespace, key);
            (0..KV_PROBE_SLOTS + 8)
                .filter(|offset| {
                    raw.get_at(base.wrapping_add(*offset).wrapping_sub(4), 0)
                        .unwrap()
                        .is_some_and(|record| record.payload.starts_with(&wanted))
                })
                .count()
                > 1
        })
        .count();
    assert!(shadowed >= 3, "only {shadowed} keys have look-alikes");

    let view = db.kv_view("default").unwrap();
    let mut checked = 0;
    for now in [0, 2_000, 4_999, 5_000, 9_001] {
        for namespace in ["n", "m"] {
            for prefix in ["", "a", "a/", "\u{e9}", "key/", "zz"] {
                for after in [
                    None,
                    Some("a"),
                    Some("a/1"),
                    Some("b"),
                    Some("\u{e9}"),
                    Some("zzz"),
                ] {
                    let mut expected: Vec<KvEntry> = names
                        .iter()
                        .filter(|key| key.starts_with(prefix))
                        .filter(|key| after.is_none_or(|after| **key > after))
                        .filter_map(|key| view.get_at(namespace, key, now).unwrap())
                        .collect();
                    expected.sort_by(|a, b| a.key.as_bytes().cmp(b.key.as_bytes()));
                    for limit in [1, 2, 3, 4, 5, 8, 13, 50, MAX_KV_LIST_LIMIT] {
                        let case = format!(
                            "{namespace} prefix {prefix:?} after {after:?} limit {limit} now {now}"
                        );
                        let wanted: Vec<KvEntry> = expected.iter().take(limit).cloned().collect();
                        let listed = view.list_at(namespace, prefix, after, limit, now).unwrap();
                        assert_eq!(listed, wanted, "{case}");
                        let seen = visited(&view, namespace, prefix, after, limit, now);
                        let entries: Vec<KvEntry> =
                            seen.iter().map(|(entry, _)| entry.clone()).collect();
                        assert_eq!(entries, wanted, "{case}: visited");
                        let total = expected.len().min(limit + 1);
                        for (index, (_, has_next)) in seen.iter().enumerate() {
                            assert_eq!(*has_next, index + 1 < total, "{case}: has_next[{index}]");
                        }
                        checked += 1;
                    }
                }
            }
        }
    }
    assert_eq!(checked, 5 * 2 * 6 * 6 * 9);
}
