use vlite::kv::{KV_PROBE_SLOTS, KvPut, MAX_KV_KEY_BYTES, MAX_KV_VALUE_BYTES, first_slot};
use vlite::{
    CollectionSettings, CollectionSpec, Database, Error, Expected, FormatVersion, Metadata,
};

fn create(format: FormatVersion) -> (tempfile::TempDir, std::path::PathBuf, Database) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("kv.vlite");
    let db = Database::create_with_format(&path, 4, format).unwrap();
    (directory, path, db)
}

#[test]
fn put_get_overwrite_delete_and_reopen() {
    let (_dir, path, mut db) = create(FormatVersion::V5);
    let mut kv = db.kv_store("memory").unwrap();
    assert_eq!(kv.get("agent", "a").unwrap(), None);
    let v1 = kv.put("agent", "a", b"one", KvPut::default()).unwrap();
    let v2 = kv.put("agent", "a", b"two", KvPut::default()).unwrap();
    assert!(v2 > v1);
    kv.put("other", "a", b"other-ns", KvPut::default()).unwrap();
    let entry = kv.get("agent", "a").unwrap().unwrap();
    assert_eq!(
        (entry.key.as_str(), entry.value.as_slice(), entry.version),
        ("a", &b"two"[..], v2)
    );
    assert_eq!(
        kv.get("other", "a").unwrap().unwrap().value,
        b"other-ns".to_vec()
    );
    drop(db);

    let mut db = Database::open(&path).unwrap();
    assert_eq!(db.collection("memory").unwrap().dimensions(), 1);
    let mut kv = db.kv_store("memory").unwrap();
    assert_eq!(kv.get("agent", "a").unwrap().unwrap().version, v2);
    assert!(kv.delete("agent", "a", Expected::Any).unwrap());
    assert!(!kv.delete("agent", "a", Expected::Any).unwrap());
    assert_eq!(kv.get("agent", "a").unwrap(), None);
    assert!(kv.get("other", "a").unwrap().is_some());
}

#[test]
fn compare_and_set_and_fencing() {
    let (_dir, _path, mut db) = create(FormatVersion::V5);
    let mut kv = db.kv_store("locks").unwrap();
    let absent = KvPut {
        expected: Expected::Absent,
        ..KvPut::default()
    };
    let token = kv.put("pool", "claim/1", b"worker-a", absent).unwrap();
    assert!(matches!(
        kv.put("pool", "claim/1", b"worker-b", absent),
        Err(Error::RevisionMismatch { current: Some(v) }) if v == token
    ));
    let renewed = kv
        .put(
            "pool",
            "claim/1",
            b"worker-a",
            KvPut {
                expected: Expected::Revision(token),
                ..KvPut::default()
            },
        )
        .unwrap();
    // The stale token is fenced off.
    assert!(matches!(
        kv.delete("pool", "claim/1", Expected::Revision(token)),
        Err(Error::RevisionMismatch { current: Some(v) }) if v == renewed
    ));
    assert!(
        kv.delete("pool", "claim/1", Expected::Revision(renewed))
            .unwrap()
    );
    assert_eq!(kv.get("pool", "claim/1").unwrap(), None);
}

#[test]
fn expiry_hides_entries_and_frees_them_for_absent_puts() {
    let (_dir, _path, mut db) = create(FormatVersion::V5);
    let mut kv = db.kv_store("leases").unwrap();
    let lease = KvPut {
        expected: Expected::Absent,
        expires_at_ms: Some(1_000),
    };
    let first = kv.put_at("pool", "lease", b"a", lease, 0).unwrap();
    let view = kv.view();
    let live = view.get_at("pool", "lease", 999).unwrap().unwrap();
    assert_eq!((live.version, live.expires_at_ms), (first, Some(1_000)));
    assert_eq!(view.get_at("pool", "lease", 1_000).unwrap(), None);
    assert!(
        view.list_at("pool", "", None, 10, 1_000)
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        kv.put_at("pool", "lease", b"b", lease, 500),
        Err(Error::RevisionMismatch { .. })
    ));
    let second = kv.put_at("pool", "lease", b"b", lease, 1_000).unwrap();
    assert!(second > first);
    let entry = kv.view().get_at("pool", "lease", 1_500).unwrap();
    assert_eq!(
        entry.map(|e| e.value),
        None,
        "the new lease also expires at 1000"
    );
    let persistent = kv
        .put_at("pool", "lease", b"c", KvPut::default(), 2_000)
        .unwrap();
    let entry = kv
        .view()
        .get_at("pool", "lease", u64::MAX)
        .unwrap()
        .unwrap();
    assert_eq!((entry.version, entry.expires_at_ms), (persistent, None));
}

#[test]
fn list_is_ordered_prefixed_and_paginated() {
    let (_dir, _path, mut db) = create(FormatVersion::V5);
    let mut kv = db.kv_store("memory").unwrap();
    for key in ["task/3", "task/1", "note/1", "task/2", "task/10"] {
        kv.put("agent", key, key.as_bytes(), KvPut::default())
            .unwrap();
    }
    kv.put("other", "task/9", b"x", KvPut::default()).unwrap();
    let view = kv.view();
    let keys = |after, limit| -> Vec<String> {
        view.list("agent", "task/", after, limit)
            .unwrap()
            .into_iter()
            .map(|entry| entry.key)
            .collect()
    };
    assert_eq!(keys(None, 10), ["task/1", "task/10", "task/2", "task/3"]);
    assert_eq!(keys(None, 2), ["task/1", "task/10"]);
    assert_eq!(keys(Some("task/10"), 10), ["task/2", "task/3"]);
    assert!(matches!(
        view.list("agent", "", None, 0),
        Err(Error::InvalidKvKey(_))
    ));
}

#[test]
fn many_keys_round_trip() {
    let db = Database::create_in_memory(4, vlite::CreateOptions::default()).unwrap();
    let mut db = db;
    let mut kv = db.kv_store("bulk").unwrap();
    for index in 0..2_000u32 {
        kv.put(
            "n",
            &format!("k{index}"),
            &index.to_le_bytes(),
            KvPut::default(),
        )
        .unwrap();
    }
    for index in 0..2_000u32 {
        let entry = kv.get("n", &format!("k{index}")).unwrap().unwrap();
        assert_eq!(entry.value, index.to_le_bytes());
    }
    assert_eq!(kv.view().list("n", "", None, 10_000).unwrap().len(), 2_000);
}

#[test]
fn validation_and_format_gates() {
    let (_dir, _path, mut db) = create(FormatVersion::V4);
    let mut kv = db.kv_store("memory").unwrap();
    kv.put("ns", "k", b"v4 works", KvPut::default()).unwrap();
    assert!(matches!(
        kv.put(
            "ns",
            "k",
            b"v",
            KvPut {
                expires_at_ms: Some(1),
                ..KvPut::default()
            }
        ),
        Err(Error::ExpiryRequiresUpgrade)
    ));
    assert!(matches!(
        kv.put("", "k", b"v", KvPut::default()),
        Err(Error::InvalidKvKey(_))
    ));
    assert!(matches!(
        kv.put("ns", "", b"v", KvPut::default()),
        Err(Error::InvalidKvKey(_))
    ));
    let long_key = "k".repeat(MAX_KV_KEY_BYTES + 1);
    assert!(matches!(
        kv.put("ns", &long_key, b"v", KvPut::default()),
        Err(Error::InvalidKvKey(_))
    ));
    let big = vec![0; MAX_KV_VALUE_BYTES + 1];
    assert!(matches!(
        kv.put("ns", "k", &big, KvPut::default()),
        Err(Error::PayloadTooLarge(_))
    ));
    let max = "k".repeat(MAX_KV_KEY_BYTES);
    kv.put(
        &"n".repeat(255),
        &max,
        &vec![1; MAX_KV_VALUE_BYTES],
        KvPut::default(),
    )
    .unwrap();

    db.create_collection(
        "vectors",
        CollectionSpec {
            dimensions: 3,
            settings: CollectionSettings::default(),
        },
    )
    .unwrap();
    assert!(matches!(
        db.kv_store("vectors"),
        Err(Error::NotKvCollection)
    ));
    assert!(matches!(
        db.kv_view("missing"),
        Err(Error::CollectionNotFound)
    ));

    let (_dir3, _path3, mut v3) = create(FormatVersion::V3);
    assert!(matches!(
        v3.kv_store("memory"),
        Err(Error::CollectionsRequireUpgrade)
    ));
}

#[test]
fn foreign_records_are_never_overwritten() {
    let (_dir, _path, mut db) = create(FormatVersion::V5);
    db.kv_store("mixed").unwrap();
    let first = first_slot("ns", "key");
    {
        let mut collection = db.collection_mut("mixed").unwrap();
        for offset in 0..KV_PROBE_SLOTS {
            collection
                .put(first.wrapping_add(offset), &[1.0], b"not a kv entry")
                .unwrap();
        }
    }
    {
        let mut kv = db.kv_store("mixed").unwrap();
        assert!(matches!(
            kv.put("ns", "key", b"v", KvPut::default()),
            Err(Error::KvSlotsExhausted)
        ));
        assert_eq!(kv.get("ns", "key").unwrap(), None);
        assert!(kv.view().list("ns", "", None, 10).unwrap().is_empty());
    }

    let freed = first.wrapping_add(5);
    db.collection_mut("mixed").unwrap().delete(freed).unwrap();
    let version = {
        let mut kv = db.kv_store("mixed").unwrap();
        let version = kv.put("ns", "key", b"v", KvPut::default()).unwrap();
        assert_eq!(kv.get("ns", "key").unwrap().unwrap().version, version);
        version
    };
    let collection = db.collection("mixed").unwrap();
    assert_eq!(collection.revision(freed).unwrap(), Some(version));
    let foreign = collection.get(first).unwrap().unwrap();
    assert_eq!(foreign.payload, b"not a kv entry");
}

// Namespace "n" keys whose first slot lies within 15 of u64::MAX, so the 16-slot
// window wraps past the last record ID. The slot hash is public and invertible;
// these were found offline by a meet-in-the-middle search over it. The first
// four share the first slot u64::MAX - 2; the last has u64::MAX - 14.
const WRAP_SHARED: [&str; 4] = [
    "wrap-1e3h8xptc3rmwn",
    "wrap-1mr1oejil0ss3d",
    "wrap-rcxazt3ti43gt3",
    "wrap-zh8a7z15oq0wpx",
];
const WRAP_LAST: &str = "wrap-2m9uqt7oe9ihm2";

fn memory() -> Database {
    Database::create_in_memory(4, vlite::CreateOptions::default()).unwrap()
}

/// The payload `kv` stores: "VKV1", namespace length, key length (u16 little
/// endian), namespace, key, value. Anyone who may write ordinary records can
/// forge one at any record ID.
fn kv_shaped(namespace: &str, key: &str, value: &str) -> Vec<u8> {
    let mut payload = b"VKV1".to_vec();
    payload.push(namespace.len() as u8);
    payload.extend_from_slice(&(key.len() as u16).to_le_bytes());
    payload.extend_from_slice(namespace.as_bytes());
    payload.extend_from_slice(key.as_bytes());
    payload.extend_from_slice(value.as_bytes());
    payload
}

fn plant(db: &mut Database, collection: &str, id: u64, namespace: &str, key: &str, value: &str) {
    db.collection_mut(collection)
        .unwrap()
        .put(id, &[0.0], &kv_shaped(namespace, key, value))
        .unwrap();
}

fn listed(db: &Database, collection: &str, now_ms: u64) -> Vec<(String, Vec<u8>)> {
    db.kv_view(collection)
        .unwrap()
        .list_at("n", "", None, 10_000, now_ms)
        .unwrap()
        .into_iter()
        .map(|entry| (entry.key, entry.value))
        .collect()
}

fn pairs(items: &[(&str, &str)]) -> Vec<(String, Vec<u8>)> {
    items
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.as_bytes().to_vec()))
        .collect()
}

fn value_of(db: &Database, collection: &str, key: &str, now_ms: u64) -> Option<Vec<u8>> {
    db.kv_view(collection)
        .unwrap()
        .get_at("n", key, now_ms)
        .unwrap()
        .map(|entry| entry.value)
}

#[test]
fn the_wrapping_fixtures_have_their_pinned_first_slots() {
    for key in WRAP_SHARED {
        assert_eq!(first_slot("n", key), u64::MAX - 2, "{key}");
    }
    assert_eq!(first_slot("n", WRAP_LAST), u64::MAX - 14);
}

#[test]
fn a_look_alike_outside_the_probe_window_is_neither_listed_nor_paginated() {
    let mut db = memory();
    for (key, value) in [("g", "g"), ("h", "real"), ("i", "i")] {
        db.kv_store("victim")
            .unwrap()
            .put("n", key, value.as_bytes(), KvPut::default())
            .unwrap();
    }
    let base = first_slot("n", "h");
    // Record 7 is the reproduction; the others sit just past either end of the window.
    let outside = [7, base.wrapping_add(KV_PROBE_SLOTS), base.wrapping_sub(1)];
    for id in outside {
        assert!(
            id.wrapping_sub(base) >= KV_PROBE_SLOTS,
            "{id} is in the window"
        );
        plant(&mut db, "victim", id, "n", "h", "SPOOF");
    }
    assert_eq!(value_of(&db, "victim", "h", 0), Some(b"real".to_vec()));
    assert_eq!(
        listed(&db, "victim", 0),
        pairs(&[("g", "g"), ("h", "real"), ("i", "i")])
    );
    // One entry per page: the real h, not a look-alike with the same key.
    let page = |after: Option<&str>| -> Vec<(String, Vec<u8>)> {
        db.kv_view("victim")
            .unwrap()
            .list_at("n", "", after, 1, 0)
            .unwrap()
            .into_iter()
            .map(|entry| (entry.key, entry.value))
            .collect()
    };
    assert_eq!(page(None), pairs(&[("g", "g")]));
    assert_eq!(page(Some("g")), pairs(&[("h", "real")]));
    assert_eq!(page(Some("h")), pairs(&[("i", "i")]));
    assert!(page(Some("i")).is_empty());

    // Deleting the key leaves no phantom; the stray records are still there.
    assert!(
        db.kv_store("victim")
            .unwrap()
            .delete("n", "h", Expected::Any)
            .unwrap()
    );
    assert_eq!(value_of(&db, "victim", "h", 0), None);
    assert_eq!(listed(&db, "victim", 0), pairs(&[("g", "g"), ("i", "i")]));
    for id in outside {
        assert!(db.collection("victim").unwrap().get(id).unwrap().is_some());
    }
}

#[test]
fn a_duplicate_inside_the_window_follows_probe_order_like_get() {
    let mut db = memory();
    db.kv_store("dup").unwrap();
    let base = first_slot("n", "dup");
    for offset in [0, 1] {
        db.collection_mut("dup")
            .unwrap()
            .put(base.wrapping_add(offset), &[0.0], b"not a kv entry")
            .unwrap();
    }
    // Offsets 0 and 1 hold foreign records, so the entry lands at offset 2.
    let real = db
        .kv_store("dup")
        .unwrap()
        .put("n", "dup", b"real", KvPut::default())
        .unwrap();
    assert_eq!(
        db.collection("dup")
            .unwrap()
            .revision(base.wrapping_add(2))
            .unwrap(),
        Some(real)
    );
    plant(&mut db, "dup", base.wrapping_add(9), "n", "dup", "late");
    assert_eq!(value_of(&db, "dup", "dup", 0), Some(b"real".to_vec()));
    assert_eq!(listed(&db, "dup", 0), pairs(&[("dup", "real")]));

    // A forged entry in front of it takes over for get and for list alike.
    plant(&mut db, "dup", base, "n", "dup", "early");
    assert_eq!(value_of(&db, "dup", "dup", 0), Some(b"early".to_vec()));
    assert_eq!(listed(&db, "dup", 0), pairs(&[("dup", "early")]));

    // Each delete removes the first holder of the key and the next one takes over.
    for remaining in ["real", "late"] {
        let deleted = db
            .kv_store("dup")
            .unwrap()
            .delete("n", "dup", Expected::Any)
            .unwrap();
        assert!(deleted);
        assert_eq!(
            value_of(&db, "dup", "dup", 0),
            Some(remaining.as_bytes().to_vec())
        );
        assert_eq!(listed(&db, "dup", 0), pairs(&[("dup", remaining)]));
    }
    assert!(
        db.kv_store("dup")
            .unwrap()
            .delete("n", "dup", Expected::Any)
            .unwrap()
    );
    assert_eq!(value_of(&db, "dup", "dup", 0), None);
    assert!(listed(&db, "dup", 0).is_empty());
}

#[test]
fn an_expired_entry_is_not_listed_and_look_alikes_do_not_take_its_place() {
    let mut db = memory();
    let lease = KvPut {
        expected: Expected::Absent,
        expires_at_ms: Some(1_000),
    };
    db.kv_store("leases")
        .unwrap()
        .put_at("n", "lease", b"real", lease, 0)
        .unwrap();
    let base = first_slot("n", "lease");
    // Live look-alikes: one past the window and one inside it, behind the entry.
    plant(
        &mut db,
        "leases",
        base.wrapping_add(KV_PROBE_SLOTS),
        "n",
        "lease",
        "outside",
    );
    plant(
        &mut db,
        "leases",
        base.wrapping_add(4),
        "n",
        "lease",
        "inside",
    );
    assert_eq!(listed(&db, "leases", 999), pairs(&[("lease", "real")]));
    // Once the deadline passes get finds nothing, so list must not offer a look-alike.
    for now in [1_000, 2_000, u64::MAX] {
        assert_eq!(value_of(&db, "leases", "lease", now), None, "now={now}");
        assert!(listed(&db, "leases", now).is_empty(), "now={now}");
    }
    // A new put reuses the expired slot; the look-alikes stay ignored.
    db.kv_store("leases")
        .unwrap()
        .put_at("n", "lease", b"fresh", KvPut::default(), 2_000)
        .unwrap();
    assert_eq!(listed(&db, "leases", 2_000), pairs(&[("lease", "fresh")]));
    assert_eq!(
        value_of(&db, "leases", "lease", 2_000),
        Some(b"fresh".to_vec())
    );
}

#[test]
fn every_key_of_a_well_formed_collection_is_listed_once_in_order() {
    let mut db = memory();
    {
        let mut kv = db.kv_store("full").unwrap();
        for index in 0..300u32 {
            let key = format!("key-{index:03}");
            kv.put("n", &key, &index.to_le_bytes(), KvPut::default())
                .unwrap();
            kv.put("m", &format!("other-{index:03}"), b"m", KvPut::default())
                .unwrap();
        }
    }
    // The four wrapped keys share one first slot. Put in this order they fill
    // u64::MAX - 2, u64::MAX - 1, u64::MAX and then ID 0, past the wrap.
    let order = [
        WRAP_SHARED[3],
        WRAP_SHARED[1],
        WRAP_SHARED[2],
        WRAP_SHARED[0],
    ];
    let slots = [u64::MAX - 2, u64::MAX - 1, u64::MAX, 0];
    for (key, slot) in order.into_iter().zip(slots) {
        let version = db
            .kv_store("full")
            .unwrap()
            .put("n", key, key.as_bytes(), KvPut::default())
            .unwrap();
        assert_eq!(
            db.collection("full").unwrap().revision(slot).unwrap(),
            Some(version),
            "{key}"
        );
    }
    let mut expected: Vec<String> = (0..300)
        .map(|index| format!("key-{index:03}"))
        .chain(WRAP_SHARED.map(str::to_owned))
        .collect();
    expected.sort();
    let check = |db: &Database, expected: &[String]| {
        let view = db.kv_view("full").unwrap();
        let all = view.list_at("n", "", None, 10_000, 0).unwrap();
        let keys: Vec<String> = all.iter().map(|entry| entry.key.clone()).collect();
        assert_eq!(keys, expected);
        for entry in &all {
            assert_eq!(
                view.get_at("n", &entry.key, 0).unwrap().as_ref(),
                Some(entry)
            );
        }
        // Pages of seven, following the cursor, give the same sequence.
        let mut paged = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let page = view.list_at("n", "", after.as_deref(), 7, 0).unwrap();
            let Some(last) = page.last() else { break };
            after = Some(last.key.clone());
            paged.extend(page.into_iter().map(|entry| entry.key));
        }
        assert_eq!(paged, expected);
    };
    check(&db, &expected);

    // A hole in front of the wrapped key changes nothing for get or list.
    assert!(
        db.kv_store("full")
            .unwrap()
            .delete("n", WRAP_SHARED[2], Expected::Any)
            .unwrap()
    );
    expected.retain(|key| key != WRAP_SHARED[2]);
    check(&db, &expected);
    // The next put of the key takes the first free slot of its window again.
    let again = db
        .kv_store("full")
        .unwrap()
        .put("n", WRAP_SHARED[2], b"again", KvPut::default())
        .unwrap();
    assert_eq!(
        db.collection("full").unwrap().revision(u64::MAX).unwrap(),
        Some(again)
    );
    expected.push(WRAP_SHARED[2].to_owned());
    expected.sort();
    // Look-alikes just before the window, just past it, and one inside it.
    plant(&mut db, "full", u64::MAX - 3, "n", WRAP_SHARED[1], "before");
    plant(&mut db, "full", 13, "n", WRAP_SHARED[2], "after");
    plant(&mut db, "full", 5, "n", WRAP_SHARED[0], "inside");
    check(&db, &expected);
}

#[test]
fn a_key_in_the_last_wrapped_slot_is_found_and_look_alikes_around_it_are_not() {
    let mut db = memory();
    db.kv_store("edge").unwrap();
    let base = first_slot("n", WRAP_LAST);
    assert_eq!(base, u64::MAX - 14);
    // Fifteen foreign records fill offsets 0..=14, so the key lands at offset 15: ID 0.
    for offset in 0..KV_PROBE_SLOTS - 1 {
        db.collection_mut("edge")
            .unwrap()
            .put(base.wrapping_add(offset), &[0.0], b"not a kv entry")
            .unwrap();
    }
    let version = db
        .kv_store("edge")
        .unwrap()
        .put("n", WRAP_LAST, b"last slot", KvPut::default())
        .unwrap();
    assert_eq!(
        db.collection("edge").unwrap().revision(0).unwrap(),
        Some(version)
    );
    plant(&mut db, "edge", 1, "n", WRAP_LAST, "past the window");
    plant(
        &mut db,
        "edge",
        u64::MAX - 15,
        "n",
        WRAP_LAST,
        "before the window",
    );
    assert_eq!(
        value_of(&db, "edge", WRAP_LAST, 0),
        Some(b"last slot".to_vec())
    );
    assert_eq!(listed(&db, "edge", 0), pairs(&[(WRAP_LAST, "last slot")]));
    assert!(
        db.kv_store("edge")
            .unwrap()
            .delete("n", WRAP_LAST, Expected::Any)
            .unwrap()
    );
    assert!(listed(&db, "edge", 0).is_empty());
}

#[test]
fn list_and_get_agree_on_a_collection_full_of_look_alikes() {
    let mut db = memory();
    db.kv_store("mixed").unwrap();
    let mut universe: Vec<(&str, &str)> =
        vec![("n", "a"), ("n", "b"), ("n", "h"), ("m", "a"), ("m", "h")];
    universe.extend(WRAP_SHARED.map(|key| ("n", key)));
    universe.push(("n", WRAP_LAST));
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut random = move |bound: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % bound
    };
    for round in 0..600 {
        let (namespace, key) = universe[random(universe.len() as u64) as usize];
        // Offsets -4..=19 around the first slot: inside the window and just outside it.
        let id = first_slot(namespace, key)
            .wrapping_add(random(24))
            .wrapping_sub(4);
        let expiry = 1 + random(9_000);
        let value = format!("v{round}");
        match random(6) {
            0 => {
                db.collection_mut("mixed")
                    .unwrap()
                    .put(id, &[0.0], b"foreign")
                    .unwrap();
            }
            1 => plant(&mut db, "mixed", id, namespace, key, &value),
            2 => {
                db.collection_mut("mixed")
                    .unwrap()
                    .put_with_expiry(
                        id,
                        &[0.0],
                        &kv_shaped(namespace, key, &value),
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
                let _ = db.kv_store("mixed").unwrap().put_at(
                    namespace,
                    key,
                    value.as_bytes(),
                    options,
                    0,
                );
            }
            4 => {
                let _ = db.kv_store("mixed").unwrap().put_at(
                    namespace,
                    key,
                    value.as_bytes(),
                    KvPut::default(),
                    0,
                );
            }
            _ => {
                db.collection_mut("mixed").unwrap().delete(id).unwrap();
            }
        }
    }
    // The state must hold keys with more than one record decoding to them.
    let view = db.collection("mixed").unwrap();
    let duplicated = universe
        .iter()
        .filter(|(namespace, key)| {
            let base = first_slot(namespace, key);
            let wanted = kv_shaped(namespace, key, "");
            (0..KV_PROBE_SLOTS + 8)
                .filter(|offset| {
                    let id = base.wrapping_add(*offset).wrapping_sub(4);
                    view.get_at(id, 0)
                        .unwrap()
                        .is_some_and(|record| record.payload.starts_with(&wanted))
                })
                .count()
                > 1
        })
        .count();
    assert!(duplicated >= 1, "the generated state has no duplicates");

    for now in [0, 2_000, 5_000, 9_999, 20_000] {
        let view = db.kv_view("mixed").unwrap();
        for namespace in ["n", "m"] {
            let all = view.list_at(namespace, "", None, 10_000, now).unwrap();
            let keys: Vec<&str> = all.iter().map(|entry| entry.key.as_str()).collect();
            let mut sorted = keys.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(keys, sorted, "{namespace} at {now}: in order, once each");
            for entry in &all {
                let found = view.get_at(namespace, &entry.key, now).unwrap();
                assert_eq!(
                    found.as_ref(),
                    Some(entry),
                    "{namespace}/{} at {now}",
                    entry.key
                );
            }
            for (_, key) in universe.iter().filter(|(ns, _)| *ns == namespace) {
                let found = view.get_at(namespace, key, now).unwrap();
                assert_eq!(
                    found.is_some(),
                    keys.contains(key),
                    "{namespace}/{key} at {now}: get and list must agree"
                );
            }
        }
    }
}
