use vlite::kv::{KV_PROBE_SLOTS, KvPut, MAX_KV_KEY_BYTES, MAX_KV_VALUE_BYTES, first_slot};
use vlite::{CollectionSettings, CollectionSpec, Database, Error, Expected, FormatVersion};

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
