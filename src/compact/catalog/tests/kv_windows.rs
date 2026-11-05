//! A key-value collection whose probe windows hold every kind of neighbour: look-alikes inside and outside the
//! window, foreign records, a record of another namespace and entries that fall due. It is the fixture of the
//! key-value tests over compact records, here and in `crate::kv`.
//!
//! Each entry is an ordinary record (vector `[0.0]`, payload `VKV1`, namespace length, key length, namespace, key,
//! value) at the record id `first_slot(namespace, key) + offset`, `offset < KV_PROBE_SLOTS`; `get` resolves a key to
//! the first record in that window that decodes to the key, and `list` offers a key only through the record `get`
//! resolves.

use crate::compact::catalog::Precision;
use crate::kv::{KV_PROBE_SLOTS, KvPut, first_slot};
use crate::{CollectionSettings, CollectionSpec, Database, Expected, Metadata};

pub(crate) const NS: &str = "n";
pub(crate) const CLOCKS: [u64; 14] = [
    0,
    99,
    100,
    101,
    149,
    150,
    151,
    199,
    200,
    201,
    299,
    300,
    301,
    u64::MAX,
];
pub(crate) const KEYS: [&str; 17] = [
    "alpha", "alpine", "apple", "banana", "cherry", "date", "elder", "fig", "ghost", "zebra",
    "missing", "al", "alpha2", "crowded", "reuse", "gone", "kiwi",
];

/// The record layout restated from the module documentation; `check_layout` pins it against what `KvStore` writes.
pub(crate) fn kv_payload(namespace: &str, key: &str, value: &[u8]) -> Vec<u8> {
    let mut payload = b"VKV1".to_vec();
    payload.push(namespace.len() as u8);
    payload.extend_from_slice(&(key.len() as u16).to_le_bytes());
    payload.extend_from_slice(namespace.as_bytes());
    payload.extend_from_slice(key.as_bytes());
    payload.extend_from_slice(value);
    payload
}

/// The record id `offset` slots into the probe window of `(namespace, key)`.
pub(crate) fn slot(namespace: &str, key: &str, offset: u64) -> u64 {
    first_slot(namespace, key).wrapping_add(offset)
}

pub(crate) fn raw(db: &mut Database, slot: u64, payload: &[u8], expiry: Option<u64>) {
    db.collection_mut("kv")
        .unwrap()
        .put_with_expiry(slot, &[0.0], payload, &Metadata::new(), expiry)
        .unwrap();
}

pub(crate) fn check_layout(db: &Database) {
    let record = db
        .collection("kv")
        .unwrap()
        .get(first_slot(NS, "alpha"))
        .unwrap()
        .expect("`alpha` sits at its first slot")
        .clone();
    assert_eq!(record.payload, kv_payload(NS, "alpha", b"1"));
    assert_eq!(record.vector, [0.0]);
}

/// The image: real entries through `KvStore`, then look-alikes written as ordinary committed records.
pub(crate) fn image(precision: Precision) -> Vec<u8> {
    super::source_fixture_with(|db| {
        db.create_collection_as(
            "kv",
            CollectionSpec {
                dimensions: 1,
                settings: CollectionSettings::default(),
            },
            precision,
        )
        .unwrap();
        {
            let mut store = db.kv_store("kv").unwrap();
            for (namespace, key, value, expiry) in [
                (NS, "alpha", &b"1"[..], None),
                (NS, "alpine", b"2", Some(100)),
                (NS, "apple", b"3", None),
                (NS, "banana", b"4", None),
                (NS, "cherry", b"5", Some(200)),
                (NS, "date", b"6", None),
                ("m", "alpha", b"7", None),
            ] {
                store
                    .put_at(
                        namespace,
                        key,
                        value,
                        KvPut {
                            expected: Expected::Any,
                            expires_at_ms: expiry,
                        },
                        0,
                    )
                    .unwrap();
            }
        }
        check_layout(db);
        // `cherry` behind its holder: a second record that decodes to the same key inside the window.
        raw(
            db,
            slot(NS, "cherry", 5),
            &kv_payload(NS, "cherry", b"shadow"),
            None,
        );
        // `banana` outside its window: sixteen slots away, so `get` never resolves to it.
        raw(
            db,
            slot(NS, "banana", KV_PROBE_SLOTS + 3),
            &kv_payload(NS, "banana", b"outside"),
            None,
        );
        // `elder` displaced: three foreign records in front of the real one.
        for offset in 0..3 {
            raw(db, slot(NS, "elder", offset), b"foreign", None);
        }
        raw(
            db,
            slot(NS, "elder", 3),
            &kv_payload(NS, "elder", b"displaced"),
            None,
        );
        // `ghost` has no holder at all, only a look-alike outside its window.
        raw(
            db,
            slot(NS, "ghost", KV_PROBE_SLOTS + 4),
            &kv_payload(NS, "ghost", b"nobody"),
            None,
        );
        // `fig` sits inside the window of `date`, which is not the window of `fig`.
        raw(
            db,
            slot(NS, "date", 1),
            &kv_payload(NS, "fig", b"lost"),
            None,
        );
        // A record that is not a key-value entry at all occupies a slot of `apple`'s window.
        raw(db, slot(NS, "apple", 1), b"", None);
        // `kiwi`: a record of another namespace with the same key sits in front of the real one.
        raw(
            db,
            slot(NS, "kiwi", 0),
            &kv_payload("m", "kiwi", b"wrong namespace"),
            None,
        );
        raw(
            db,
            slot(NS, "kiwi", 1),
            &kv_payload(NS, "kiwi", b"right"),
            None,
        );
        // `crowded`: all sixteen slots of its window hold foreign records, so no slot is free for it.
        for offset in 0..KV_PROBE_SLOTS {
            raw(db, slot(NS, "crowded", offset), b"foreign", None);
        }
        // `reuse`: the first slot holds an entry of another key that is due at 50 (reusable from then on), the second
        // a foreign record (never reusable), the rest is empty.
        raw(
            db,
            slot(NS, "reuse", 0),
            &kv_payload(NS, "gone", b"old"),
            Some(50),
        );
        raw(db, slot(NS, "reuse", 1), b"foreign", None);
    })
}

pub(crate) fn open(precision: Precision) -> (Database, Database) {
    let bytes = image(precision);
    let oracle = {
        let _source = crate::compact::source::Activation::set(false);
        Database::open_from_bytes(bytes.clone(), None).unwrap()
    };
    let candidate = Database::open_from_bytes(bytes, None).unwrap();
    let id = candidate.collection("kv").unwrap().id();
    assert!(candidate.state(id).records.is_compact());
    assert!(
        !oracle
            .state(oracle.collection("kv").unwrap().id())
            .records
            .is_compact()
    );
    (candidate, oracle)
}
