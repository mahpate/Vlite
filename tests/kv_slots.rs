//! Probe slots of the key-value store: an expired entry of another key frees
//! its slot, a live entry and any other record never do, and a put that finds
//! no free slot fails with `KvSlotsExhausted` without writing.

use vlite::kv::{KV_PROBE_SLOTS, KvPut, first_slot};
use vlite::{CreateOptions, Database, Error, Expected, Metadata};

// Namespace "n" keys that all hash to the first slot u64::MAX - 2, found
// offline by a meet-in-the-middle search over the public slot hash (see
// tests/kv.rs). Together they let one window hold several different keys.
const SHARED: [&str; 4] = [
    "wrap-1e3h8xptc3rmwn",
    "wrap-1mr1oejil0ss3d",
    "wrap-rcxazt3ti43gt3",
    "wrap-zh8a7z15oq0wpx",
];

fn slot(offset: u64) -> u64 {
    first_slot("n", SHARED[0]).wrapping_add(offset)
}

fn window(db: &Database) -> Vec<(Option<u64>, Option<Vec<u8>>)> {
    let collection = db.collection("slots").unwrap();
    (0..KV_PROBE_SLOTS)
        .map(|offset| {
            (
                collection.revision(slot(offset)).unwrap(),
                collection
                    .get(slot(offset))
                    .unwrap()
                    .map(|record| record.payload.clone()),
            )
        })
        .collect()
}

fn lease(expires_at_ms: u64) -> KvPut {
    KvPut {
        expected: Expected::Any,
        expires_at_ms: Some(expires_at_ms),
    }
}

#[test]
fn an_expired_entry_frees_its_slot_but_live_and_foreign_slots_stay_taken() {
    for key in SHARED {
        assert_eq!(first_slot("n", key), slot(0), "{key}");
    }
    let [a, b, c, d] = SHARED;
    let mut db = Database::create_in_memory(4, CreateOptions::default()).unwrap();
    db.kv_store("slots").unwrap();
    {
        let mut kv = db.kv_store("slots").unwrap();
        // The first free slot of the window is taken in order: a, then b.
        kv.put_at("n", a, b"a", lease(1_000), 0).unwrap();
        kv.put_at("n", b, b"b", KvPut::default(), 0).unwrap();
    }
    {
        let mut records = db.collection_mut("slots").unwrap();
        for offset in 2..KV_PROBE_SLOTS {
            records.put(slot(offset), &[1.0], b"foreign").unwrap();
        }
    }
    let full = window(&db);
    assert!(full.iter().all(|(revision, _)| revision.is_some()));

    // Every slot is taken: a live entry of another key, a live entry with no
    // deadline, and fourteen records that are not entries. A new key has
    // nowhere to go, up to the last instant before a's deadline.
    for now in [0, 500, 999] {
        let mut kv = db.kv_store("slots").unwrap();
        assert!(
            matches!(
                kv.put_at("n", c, b"c", KvPut::default(), now),
                Err(Error::KvSlotsExhausted)
            ),
            "now={now}"
        );
        // A key that already owns a slot is not another key: it can still be written.
        kv.put_at("n", b, b"b", KvPut::default(), now).unwrap();
    }
    let after_refusals = window(&db);
    assert_eq!(after_refusals[0], full[0], "refusals leave a's slot alone");
    assert_eq!(after_refusals[2..], full[2..], "and the foreign records");
    assert_eq!(after_refusals[1].1, full[1].1, "b was rewritten in place");
    assert!(after_refusals[1].0 > full[1].0);

    // At its deadline a's entry is expired, so its slot is free for another key.
    let version = db
        .kv_store("slots")
        .unwrap()
        .put_at("n", c, b"c", KvPut::default(), 1_000)
        .unwrap();
    let reused = window(&db);
    assert_eq!(
        db.collection("slots").unwrap().revision(slot(0)).unwrap(),
        Some(version),
        "c took the slot of the expired entry"
    );
    assert_eq!(reused[1], after_refusals[1], "b's slot is untouched");
    assert_eq!(reused[2..], full[2..], "foreign slots are untouched");
    let view = db.kv_view("slots").unwrap();
    assert_eq!(view.get_at("n", c, 1_000).unwrap().unwrap().value, b"c");
    assert_eq!(view.get_at("n", b, 1_000).unwrap().unwrap().value, b"b");
    assert_eq!(view.get_at("n", a, 1_000).unwrap(), None);

    // The reused slot is live again, so a fourth key finds the window full.
    let before = window(&db);
    assert!(matches!(
        db.kv_store("slots")
            .unwrap()
            .put_at("n", d, b"d", KvPut::default(), 1_000),
        Err(Error::KvSlotsExhausted)
    ));
    assert_eq!(window(&db), before);
}

#[test]
fn the_first_expired_slot_is_the_one_reused() {
    let [a, b, c, _] = SHARED;
    let mut db = Database::create_in_memory(4, CreateOptions::default()).unwrap();
    let mut kv = db.kv_store("slots").unwrap();
    kv.put_at("n", a, b"a", lease(100), 0).unwrap();
    kv.put_at("n", b, b"b", lease(200), 0).unwrap();
    // Both are expired at 300; the entry nearest the first probe slot is replaced.
    let version = kv.put_at("n", c, b"c", KvPut::default(), 300).unwrap();
    assert_eq!(
        db.collection("slots").unwrap().revision(slot(0)).unwrap(),
        Some(version)
    );
    assert_eq!(
        db.kv_view("slots").unwrap().get_at("n", b, 300).unwrap(),
        None,
        "b is expired and still hidden"
    );
}

#[test]
fn a_record_that_is_not_an_entry_keeps_its_slot_even_when_it_has_expired() {
    let mut db = Database::create_in_memory(4, CreateOptions::default()).unwrap();
    db.kv_store("slots").unwrap();
    {
        let mut records = db.collection_mut("slots").unwrap();
        for offset in 0..KV_PROBE_SLOTS {
            records
                .put_with_expiry(slot(offset), &[1.0], b"foreign", &Metadata::new(), Some(10))
                .unwrap();
        }
    }
    let before = window(&db);
    for now in [0, 10, u64::MAX] {
        let result =
            db.kv_store("slots")
                .unwrap()
                .put_at("n", SHARED[0], b"v", KvPut::default(), now);
        assert!(matches!(result, Err(Error::KvSlotsExhausted)), "now={now}");
    }
    assert_eq!(window(&db), before);
}
