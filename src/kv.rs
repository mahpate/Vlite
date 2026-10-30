//! Namespaced key-value store on top of an ordinary one-dimension collection.
//!
//! No on-disk format change: each entry is a normal record whose vector is the
//! fixed placeholder `[0.0]` and whose payload carries the namespace, key and
//! value. Record IDs are a stable 64-bit hash of `(namespace, key)`; up to
//! [`KV_PROBE_SLOTS`] consecutive IDs are probed so hash collisions are handled
//! without tombstones (every slot stores its full key and is always compared).
//!
//! Versions are record revisions (commit sequences): strictly increasing per
//! key within one file lineage, so they work as compare-and-set tokens and as
//! fencing tokens. Compaction, upgrade and restore rewrite revisions, so a
//! holder of a version from before such an operation must re-read first; its
//! next conditional write fails with `RevisionMismatch` rather than succeeding
//! wrongly. Directory-store checkpoints are not such an operation: they keep
//! every live record's revision, so versions stay valid across them.
//!
//! Expiry needs a v5 file and returns `ExpiryRequiresUpgrade` otherwise.
//! Listing scans the collection (O(entries)); keep large key spaces in
//! separate collections.

use crate::{
    CollectionSettings, CollectionSpec, Database, Error, Expected, MAX_PAYLOAD_BYTES, Metadata,
    Result, current_time_ms,
};

/// Consecutive record IDs probed per key.
pub const KV_PROBE_SLOTS: u64 = 16;
/// Maximum namespace length in bytes.
pub const MAX_KV_NAMESPACE_BYTES: usize = 255;
/// Maximum key length in bytes.
pub const MAX_KV_KEY_BYTES: usize = 1024;
/// Maximum value length in bytes.
pub const MAX_KV_VALUE_BYTES: usize =
    MAX_PAYLOAD_BYTES - HEADER_BYTES - MAX_KV_NAMESPACE_BYTES - MAX_KV_KEY_BYTES;
/// Maximum entries returned by one `list` call.
pub const MAX_KV_LIST_LIMIT: usize = 10_000;

const MAGIC: &[u8; 4] = b"VKV1";
const HEADER_BYTES: usize = 4 + 1 + 2;
const PLACEHOLDER: [f32; 1] = [0.0];

/// One live entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KvEntry {
    pub key: String,
    pub value: Vec<u8>,
    /// Compare-and-set / fencing token; see the module documentation.
    pub version: u64,
    pub expires_at_ms: Option<u64>,
}

/// Options for `KvStore::put`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KvPut {
    /// `Expected::Revision` compares against `KvEntry::version`; `Absent`
    /// also matches an expired entry.
    pub expected: Expected,
    /// Absolute Unix-millisecond expiry. Requires v5.
    pub expires_at_ms: Option<u64>,
}

impl Default for KvPut {
    fn default() -> Self {
        Self {
            expected: Expected::Any,
            expires_at_ms: None,
        }
    }
}

/// Read-only key-value view. Obtain with `Database::kv_view`.
#[derive(Clone, Copy)]
pub struct KvView<'a> {
    db: &'a Database,
    id: u16,
}

/// Mutable key-value store. Obtain with `Database::kv_store`.
pub struct KvStore<'a> {
    db: &'a mut Database,
    id: u16,
}

impl Database {
    /// Opens the key-value store in `collection`, creating a one-dimension
    /// collection when it does not exist (v4+). An existing collection must
    /// have one dimension.
    pub fn kv_store(&mut self, collection: &str) -> Result<KvStore<'_>> {
        let id = match self.resolve(collection) {
            Ok(id) => id,
            Err(Error::CollectionNotFound) => {
                self.create_collection(
                    collection,
                    CollectionSpec {
                        dimensions: 1,
                        settings: CollectionSettings::default(),
                    },
                )?
                .id
            }
            Err(error) => return Err(error),
        };
        check_kv_collection(self, id)?;
        Ok(KvStore { db: self, id })
    }

    /// Read-only view of an existing key-value collection.
    pub fn kv_view(&self, collection: &str) -> Result<KvView<'_>> {
        let id = self.resolve(collection)?;
        check_kv_collection(self, id)?;
        Ok(KvView { db: self, id })
    }
}

fn check_kv_collection(db: &Database, id: u16) -> Result<()> {
    if db.state(id).dimensions == 1 {
        Ok(())
    } else {
        Err(Error::NotKvCollection)
    }
}

/// First record ID probed for `(namespace, key)`; the key may occupy any of
/// the next `KV_PROBE_SLOTS` IDs (wrapping). Stable across releases.
pub fn first_slot(namespace: &str, key: &str) -> u64 {
    slot_base(namespace, key)
}

impl KvView<'_> {
    pub fn get(&self, namespace: &str, key: &str) -> Result<Option<KvEntry>> {
        get_at(self.db, self.id, namespace, key, current_time_ms())
    }

    /// `get` with a caller-supplied Unix-millisecond clock.
    pub fn get_at(&self, namespace: &str, key: &str, now_ms: u64) -> Result<Option<KvEntry>> {
        get_at(self.db, self.id, namespace, key, now_ms)
    }

    /// Live entries of `namespace` whose key starts with `prefix`, in key
    /// order, strictly after `after` when given, at most `limit`
    /// (`1..=MAX_KV_LIST_LIMIT`).
    pub fn list(
        &self,
        namespace: &str,
        prefix: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<KvEntry>> {
        list_at(
            self.db,
            self.id,
            namespace,
            prefix,
            after,
            limit,
            current_time_ms(),
        )
    }

    /// `list` with a caller-supplied Unix-millisecond clock.
    pub fn list_at(
        &self,
        namespace: &str,
        prefix: &str,
        after: Option<&str>,
        limit: usize,
        now_ms: u64,
    ) -> Result<Vec<KvEntry>> {
        list_at(self.db, self.id, namespace, prefix, after, limit, now_ms)
    }
}

impl KvStore<'_> {
    pub fn view(&self) -> KvView<'_> {
        KvView {
            db: self.db,
            id: self.id,
        }
    }

    pub fn get(&self, namespace: &str, key: &str) -> Result<Option<KvEntry>> {
        self.view().get(namespace, key)
    }

    /// Durably writes `value`; returns the new version. A failed precondition
    /// returns `RevisionMismatch { current }` (the live version) unwritten.
    pub fn put(&mut self, namespace: &str, key: &str, value: &[u8], options: KvPut) -> Result<u64> {
        self.put_at(namespace, key, value, options, current_time_ms())
    }

    /// `put` with a caller-supplied Unix-millisecond clock for expiry checks.
    pub fn put_at(
        &mut self,
        namespace: &str,
        key: &str,
        value: &[u8],
        options: KvPut,
        now_ms: u64,
    ) -> Result<u64> {
        validate(namespace, key)?;
        if value.len() > MAX_KV_VALUE_BYTES {
            return Err(Error::PayloadTooLarge(value.len()));
        }
        let probe = probe(self.db, self.id, namespace, key, now_ms);
        options.expected.check(probe.live_version())?;
        let (slot, physical) = match probe.found {
            Some(found) => (found.slot, Expected::Revision(found.revision)),
            None => {
                let (slot, revision) = probe.free.ok_or(Error::KvSlotsExhausted)?;
                (slot, revision.map_or(Expected::Absent, Expected::Revision))
            }
        };
        let payload = encode(namespace, key, value);
        match options.expires_at_ms {
            Some(expiry) => self.db.put_with_expiry_in(
                self.id,
                slot,
                &PLACEHOLDER,
                &payload,
                &Metadata::new(),
                Some(expiry),
                physical,
            ),
            None => self.db.put_in(
                self.id,
                slot,
                &PLACEHOLDER,
                &payload,
                &Metadata::new(),
                physical,
            ),
        }
    }

    /// Durably deletes the entry; returns false when it is absent or expired.
    pub fn delete(&mut self, namespace: &str, key: &str, expected: Expected) -> Result<bool> {
        self.delete_at(namespace, key, expected, current_time_ms())
    }

    /// `delete` with a caller-supplied Unix-millisecond clock.
    pub fn delete_at(
        &mut self,
        namespace: &str,
        key: &str,
        expected: Expected,
        now_ms: u64,
    ) -> Result<bool> {
        validate(namespace, key)?;
        let probe = probe(self.db, self.id, namespace, key, now_ms);
        expected.check(probe.live_version())?;
        match probe.found {
            Some(found) if !found.expired => {
                self.db
                    .delete_in(self.id, found.slot, Expected::Revision(found.revision))
            }
            Some(found) => {
                // Remove the expired leftover so the slot is reusable.
                self.db
                    .delete_in(self.id, found.slot, Expected::Revision(found.revision))?;
                Ok(false)
            }
            None => Ok(false),
        }
    }
}

struct Found {
    slot: u64,
    revision: u64,
    expired: bool,
}

struct Probe {
    found: Option<Found>,
    /// First reusable slot: empty, or holding an expired entry (its revision).
    free: Option<(u64, Option<u64>)>,
}

impl Probe {
    fn live_version(&self) -> Option<u64> {
        self.found
            .as_ref()
            .filter(|found| !found.expired)
            .map(|found| found.revision)
    }
}

fn probe(db: &Database, collection: u16, namespace: &str, key: &str, now_ms: u64) -> Probe {
    let state = db.state(collection);
    let base = slot_base(namespace, key);
    let mut free = None;
    for offset in 0..KV_PROBE_SLOTS {
        let slot = base.wrapping_add(offset);
        let Some(record) = state.records.get(&slot) else {
            free.get_or_insert((slot, None));
            continue;
        };
        let revision = state.revisions.get(&slot).copied().unwrap_or_default();
        let expired = state
            .expiries
            .get(&slot)
            .is_some_and(|expiry| *expiry <= now_ms);
        match decode(&record.payload) {
            Some((ns, k, _)) if ns == namespace && k == key => {
                return Probe {
                    found: Some(Found {
                        slot,
                        revision,
                        expired,
                    }),
                    free,
                };
            }
            // Expired entries of other keys are reusable; foreign records are not.
            Some(_) if expired => {
                free.get_or_insert((slot, Some(revision)));
            }
            _ => {}
        }
    }
    Probe { found: None, free }
}

fn get_at(
    db: &Database,
    collection: u16,
    namespace: &str,
    key: &str,
    now_ms: u64,
) -> Result<Option<KvEntry>> {
    db.ensure_healthy()?;
    validate(namespace, key)?;
    let probe = probe(db, collection, namespace, key, now_ms);
    let Some(found) = probe.found.filter(|found| !found.expired) else {
        return Ok(None);
    };
    Ok(entry(db, collection, found.slot))
}

fn entry(db: &Database, collection: u16, slot: u64) -> Option<KvEntry> {
    let state = db.state(collection);
    let record = state.records.get(&slot)?;
    let (_, key, value) = decode(&record.payload)?;
    Some(KvEntry {
        key: key.to_owned(),
        value: value.to_vec(),
        version: state.revisions.get(&slot).copied().unwrap_or_default(),
        expires_at_ms: state.expiries.get(&slot).copied(),
    })
}

fn list_at(
    db: &Database,
    collection: u16,
    namespace: &str,
    prefix: &str,
    after: Option<&str>,
    limit: usize,
    now_ms: u64,
) -> Result<Vec<KvEntry>> {
    db.ensure_healthy()?;
    validate_namespace(namespace)?;
    if limit == 0 || limit > MAX_KV_LIST_LIMIT {
        return Err(Error::InvalidKvKey("limit must be between 1 and 10000"));
    }
    let state = db.state(collection);
    let mut matches: Vec<(&str, u64)> = state
        .records
        .iter()
        .filter(|(id, _)| {
            !state
                .expiries
                .get(id)
                .is_some_and(|expiry| *expiry <= now_ms)
        })
        .filter_map(|(&id, record)| {
            let (ns, key, _) = decode(&record.payload)?;
            (ns == namespace && key.starts_with(prefix) && after.is_none_or(|after| key > after))
                .then_some((key, id))
        })
        .collect();
    matches.sort_unstable();
    Ok(matches
        .into_iter()
        .take(limit)
        .filter_map(|(_, id)| entry(db, collection, id))
        .collect())
}

fn validate_namespace(namespace: &str) -> Result<()> {
    if namespace.is_empty() || namespace.len() > MAX_KV_NAMESPACE_BYTES {
        return Err(Error::InvalidKvKey("namespace must be 1 to 255 bytes"));
    }
    Ok(())
}

fn validate(namespace: &str, key: &str) -> Result<()> {
    validate_namespace(namespace)?;
    if key.is_empty() || key.len() > MAX_KV_KEY_BYTES {
        return Err(Error::InvalidKvKey("key must be 1 to 1024 bytes"));
    }
    Ok(())
}

/// Stable across builds and platforms: FNV-1a 64 with a length-prefixed
/// namespace, finished with the splitmix64 mixer.
fn slot_base(namespace: &str, key: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let length = (namespace.len() as u16).to_le_bytes();
    for byte in length
        .iter()
        .chain(namespace.as_bytes())
        .chain(key.as_bytes())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash = (hash ^ (hash >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    hash = (hash ^ (hash >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    hash ^ (hash >> 31)
}

fn encode(namespace: &str, key: &str, value: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(HEADER_BYTES + namespace.len() + key.len() + value.len());
    payload.extend_from_slice(MAGIC);
    payload.push(namespace.len() as u8);
    payload.extend_from_slice(&(key.len() as u16).to_le_bytes());
    payload.extend_from_slice(namespace.as_bytes());
    payload.extend_from_slice(key.as_bytes());
    payload.extend_from_slice(value);
    payload
}

fn decode(payload: &[u8]) -> Option<(&str, &str, &[u8])> {
    let rest = payload.strip_prefix(MAGIC)?;
    let (&ns_len, rest) = rest.split_first()?;
    let (key_len, rest) = rest.split_first_chunk::<2>()?;
    let ns_len = usize::from(ns_len);
    let key_len = usize::from(u16::from_le_bytes(*key_len));
    if rest.len() < ns_len + key_len {
        return None;
    }
    let (ns, rest) = rest.split_at(ns_len);
    let (key, value) = rest.split_at(key_len);
    Some((
        std::str::from_utf8(ns).ok()?,
        std::str::from_utf8(key).ok()?,
        value,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_round_trips_and_rejects_foreign_bytes() {
        let payload = encode("agent", "task/1", b"v");
        assert_eq!(decode(&payload), Some(("agent", "task/1", &b"v"[..])));
        assert_eq!(decode(b"VKV1"), None);
        assert_eq!(decode(b"plain payload"), None);
        let mut truncated = encode("agent", "task/1", b"");
        truncated.pop();
        assert_eq!(decode(&truncated), None);
    }

    #[test]
    fn slot_hash_is_stable_and_namespace_separated() {
        // Pinned: changing the hash would orphan every stored key.
        assert_eq!(slot_base("ns", "key"), slot_base("ns", "key"));
        assert_ne!(slot_base("ab", "c"), slot_base("a", "bc"));
        assert_eq!(slot_base("ns", "key"), PINNED_NS_KEY);
    }

    const PINNED_NS_KEY: u64 = 7_779_417_742_689_054_324;
}
