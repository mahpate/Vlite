//! Namespaced key-value store on top of an ordinary one-dimension collection.
//!
//! No on-disk format change: each entry is a normal record whose vector is the
//! fixed placeholder `[0.0]` and whose payload carries the namespace, key and
//! value. Record IDs are a stable 64-bit hash of `(namespace, key)`; up to
//! [`KV_PROBE_SLOTS`] consecutive IDs are probed so hash collisions are handled
//! without tombstones (every slot stores its full key and is always compared).
//!
//! Versions are record revisions (commit sequences): strictly increasing per
//! key within one file lineage, so they work as compare-and-set tokens. They
//! are not durable lease-fencing tokens. Compaction, upgrade and restore
//! rewrite revisions, so a holder of a version from before such an operation
//! must re-read first; its next conditional write fails with
//! `RevisionMismatch` rather than succeeding wrongly. Directory-store
//! checkpoints are not such an operation: they keep every live record's
//! revision, so versions stay valid across them.
//!
//! Expiry needs a v5 file and returns `ExpiryRequiresUpgrade` otherwise.
//! Listing scans the collection (O(entries)); keep large key spaces in
//! separate collections. A scan keeps only the `limit + 1` smallest candidate
//! keys it has found so far, so its working memory is O(`limit`) however many
//! entries match. It offers each key once, as the record `get` resolves it: a
//! look-alike record outside the key's probe window, or behind an earlier
//! holder of the key, is never listed. [`KvView::list_at`] copies every value
//! it returns; [`KvView::visit_at`] lends each value instead.

use crate::{
    CollectionSettings, CollectionSpec, Database, Error, Expected, MAX_PAYLOAD_BYTES, Metadata,
    Result, current_time_ms,
};
use std::{
    collections::{BinaryHeap, HashMap},
    ops::ControlFlow,
};

mod batch;

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
/// Maximum operations in one [`KvStore::write_batch`].
pub const MAX_KV_BATCH_OPERATIONS: usize = 1_000;
/// Maximum total value bytes of the puts in one [`KvStore::write_batch`].
pub const MAX_KV_BATCH_VALUE_BYTES: usize = 4_194_304;
/// Maximum keys named by one [`KvView::get_many`].
pub const MAX_KV_GET_MANY_KEYS: usize = 1_000;

const MAGIC: &[u8; 4] = b"VKV1";
const HEADER_BYTES: usize = 4 + 1 + 2;
const PLACEHOLDER: [f32; 1] = [0.0];

// A full batch is one engine batch frame: its values plus, for each operation,
// the 16-byte entry header, the placeholder vector and the largest payload
// header, namespace and key.
const _: () = assert!(
    MAX_KV_BATCH_OPERATIONS <= crate::MAX_BATCH_OPERATIONS
        && MAX_KV_BATCH_VALUE_BYTES
            + MAX_KV_BATCH_OPERATIONS
                * (16 + 4 + HEADER_BYTES + MAX_KV_NAMESPACE_BYTES + MAX_KV_KEY_BYTES)
            <= crate::MAX_BATCH_BYTES
);

/// One live entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KvEntry {
    pub key: String,
    pub value: Vec<u8>,
    /// Compare-and-set token; see the module documentation.
    pub version: u64,
    pub expires_at_ms: Option<u64>,
}

/// One live entry lent to the visitor of [`KvView::visit_at`]: the key and
/// value borrow the store and are valid only for that call. The struct is
/// `#[non_exhaustive]`: only the library builds one, so a later release can add
/// a field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct KvRef<'a> {
    pub key: &'a str,
    pub value: &'a [u8],
    /// Compare-and-set token; see the module documentation.
    pub version: u64,
    pub expires_at_ms: Option<u64>,
    /// Another live entry follows this one in the listing: this entry is not
    /// the last of the `limit` visited, or it is and a further live entry lies
    /// beyond the limit. Decided from keys alone: no value is read for it. For
    /// [`KvView::visit_many_at`]: a later requested key follows this one.
    pub has_next: bool,
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

/// One operation of [`KvStore::write_batch`]. Marked `#[non_exhaustive]`: a
/// later release can add kinds of operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum KvOp<'a> {
    /// Writes `value` under the key, like [`KvStore::put`]. A batch put has no
    /// deadline in this release: `options.expires_at_ms` must be `None`, and
    /// like any put without one it clears the deadline of the entry it
    /// replaces.
    Put {
        namespace: &'a str,
        key: &'a str,
        value: &'a [u8],
        options: KvPut,
    },
    /// Deletes the entry, like [`KvStore::delete`].
    Delete {
        namespace: &'a str,
        key: &'a str,
        expected: Expected,
    },
}

/// The result of one operation of [`KvStore::write_batch`]. Marked
/// `#[non_exhaustive]`: a later release can add results.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum KvOutcome {
    /// The version of the written entry: the commit's sequence, the same for
    /// every put of the batch.
    Put { version: u64 },
    /// Whether a live entry was deleted. A key that is absent or due gives
    /// `false`.
    Deleted(bool),
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

/// Why a batch given to [`KvStore::write_batch_gated_at`] was not written.
#[cfg(feature = "server")]
#[derive(Debug)]
pub(crate) enum Gated<G> {
    /// The batch could not run; `write_batch_at` fails the same way for it.
    Batch(Error),
    /// The gate refused it.
    Refused(G),
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

#[cfg(test)]
pub(crate) fn compact_view_for_test(db: &Database, id: u16) -> KvView<'_> {
    KvView { db, id }
}

#[cfg(feature = "server")]
impl Database {
    /// `kv_view` for a collection the server layer has already resolved.
    pub(crate) fn kv_view_by_id(&self, id: u16) -> Result<KvView<'_>> {
        self.live_collection(id)?;
        check_kv_collection(self, id)?;
        Ok(KvView { db: self, id })
    }

    /// `kv_store` for an existing collection the server layer has already
    /// resolved. Unlike `kv_store` it never creates one.
    pub(crate) fn kv_store_by_id(&mut self, id: u16) -> Result<KvStore<'_>> {
        self.live_collection(id)?;
        check_kv_collection(self, id)?;
        Ok(KvStore { db: self, id })
    }
}

fn check_kv_collection(db: &Database, id: u16) -> Result<()> {
    db.ensure_resident_in(id)?;
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
    #[cfg(feature = "server")]
    pub(crate) fn put_slot_at(
        &self,
        namespace: &str,
        key: &str,
        expected: Expected,
        now_ms: u64,
    ) -> Result<u64> {
        validate(namespace, key)?;
        let probe = probe(self.db, self.id, namespace, key, now_ms)?;
        expected.check(probe.live_version())?;
        match probe.found {
            Some(found) => Ok(found.slot),
            None => probe
                .free
                .map(|(slot, _)| slot)
                .ok_or(Error::KvSlotsExhausted),
        }
    }

    pub fn get(&self, namespace: &str, key: &str) -> Result<Option<KvEntry>> {
        get_at(self.db, self.id, namespace, key, current_time_ms())
    }

    /// `get` with a caller-supplied Unix-millisecond clock.
    pub fn get_at(&self, namespace: &str, key: &str, now_ms: u64) -> Result<Option<KvEntry>> {
        get_at(self.db, self.id, namespace, key, now_ms)
    }

    /// Live entries of `namespace` whose key starts with `prefix`, in key
    /// order, strictly after `after` when given, at most `limit`
    /// (`1..=MAX_KV_LIST_LIMIT`). Every value is copied; see
    /// [`KvView::list_at`] for what that can cost.
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
    ///
    /// Every returned value is copied, so a result of `limit` entries can
    /// hold `limit` values of up to [`MAX_KV_VALUE_BYTES`] at once: roughly
    /// 10 GB at the largest limit and value size. Use [`KvView::visit_at`]
    /// to read values without copying them, or a smaller `limit`, when values
    /// can be large.
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

    /// Visits the live entries of `namespace` whose key starts with `prefix`,
    /// in key order, strictly after `after` when given, at most `limit`
    /// (`1..=MAX_KV_LIST_LIMIT`), as of the Unix-millisecond clock `now_ms`.
    /// The entries are the ones [`KvView::list_at`] returns for the same
    /// arguments, but each value is lent to `visitor` instead of copied: the
    /// call allocates only what the visitor copies, plus O(`limit`) candidate
    /// keys, whatever the number of matching entries.
    ///
    /// The scan reads the collection once, then calls `visitor` in key order.
    /// The visitor returns [`ControlFlow::Break`] to stop early and
    /// [`ControlFlow::Continue`] to go on; [`KvRef::has_next`] says whether
    /// another live entry follows the one it is given.
    pub fn visit_at<F>(
        &self,
        namespace: &str,
        prefix: &str,
        after: Option<&str>,
        limit: usize,
        now_ms: u64,
        visitor: F,
    ) -> Result<()>
    where
        F: FnMut(KvRef<'_>) -> ControlFlow<()>,
    {
        visit_at(
            self.db, self.id, namespace, prefix, after, limit, now_ms, visitor,
        )
    }

    /// The entries of `keys`, `(namespace, key)` pairs of any namespaces, in
    /// the order given: `None` for a key that is absent or due. A key may
    /// appear more than once. One expiry clock and one view of the store serve
    /// the whole call, so the result is consistent across the keys; nothing is
    /// consistent between two calls.
    ///
    /// At most [`MAX_KV_GET_MANY_KEYS`] keys, each with a namespace and a key
    /// of the sizes [`KvView::get`] takes; otherwise `InvalidKvKey` and
    /// nothing is read. These checks come before any state of the collection
    /// is read, but not before the view is opened: [`Database::kv_view`] has
    /// already failed for a missing collection or one of another kind. Every
    /// returned value is copied, so a result can hold that many values of up
    /// to [`MAX_KV_VALUE_BYTES`] at once: use [`KvView::visit_many_at`] to read
    /// without copying.
    pub fn get_many(&self, keys: &[(&str, &str)]) -> Result<Vec<Option<KvEntry>>> {
        self.get_many_at(keys, current_time_ms())
    }

    /// `get_many` with a caller-supplied Unix-millisecond clock.
    pub fn get_many_at(&self, keys: &[(&str, &str)], now_ms: u64) -> Result<Vec<Option<KvEntry>>> {
        let mut entries = Vec::with_capacity(keys.len());
        batch::visit_many(self.db, self.id, keys, now_ms, |_, entry| {
            entries.push(entry.map(|entry| KvEntry {
                key: entry.key.to_owned(),
                value: entry.value.to_vec(),
                version: entry.version,
                expires_at_ms: entry.expires_at_ms,
            }));
            ControlFlow::Continue(())
        })?;
        Ok(entries)
    }

    /// Visits the keys of `keys` in the order given as of the Unix-millisecond
    /// clock `now_ms`, with each key's position and its live entry, or `None`
    /// when the key is absent or due. The entries are the ones
    /// [`KvView::get_many_at`] returns, but each value is lent to `visitor`
    /// instead of copied, so a caller can build a response of a fixed size.
    /// The visitor returns [`ControlFlow::Break`] to stop early; the keys after
    /// it are not read. [`KvRef::has_next`] says whether a later requested key
    /// follows this one.
    ///
    /// The keys are checked as for [`KvView::get_many`] before the first visit.
    pub fn visit_many_at<F>(&self, keys: &[(&str, &str)], now_ms: u64, visitor: F) -> Result<()>
    where
        F: FnMut(usize, Option<KvRef<'_>>) -> ControlFlow<()>,
    {
        batch::visit_many(self.db, self.id, keys, now_ms, visitor)
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
        let probe = probe(self.db, self.id, namespace, key, now_ms)?;
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
        let probe = probe(self.db, self.id, namespace, key, now_ms)?;
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

    /// Commits `operations`, puts and deletes of any namespaces of this
    /// collection, all or none, and returns one [`KvOutcome`] for each in the
    /// order given.
    ///
    /// The batch behaves as if its operations ran one at a time in the order
    /// given, each against the live state the earlier ones left, on one expiry
    /// clock, and the result were then committed as a single engine batch
    /// frame: a crash leaves all of it or none of it, and a reader sees the
    /// state before the batch or the state after it. The first operation that
    /// cannot run fails the batch with the error its single call would give
    /// (`RevisionMismatch` for a failed condition, `KvSlotsExhausted`,
    /// `GraphAttachedRecord`) and nothing is written. A batch that changes no
    /// record writes no frame.
    ///
    /// The operations are checked first, as a whole: at most
    /// [`MAX_KV_BATCH_OPERATIONS`] of them, each with a namespace, key and
    /// value of the sizes a single call takes, the values of the puts totaling
    /// at most [`MAX_KV_BATCH_VALUE_BYTES`], no put with a deadline, and no two
    /// operations naming the same `(namespace, key)`; otherwise `InvalidKvKey`
    /// (`PayloadTooLarge` for one value) and nothing is written. These checks
    /// come before any state of the collection is read, but not before the
    /// store is opened: [`Database::kv_store`] has already created or refused
    /// the collection. An empty batch is a no-op.
    ///
    /// Every put of the batch gets the same version, the commit's sequence. A
    /// put clears the deadline of the entry it replaces. As for a single write,
    /// a failed write or sync is `CommitIndeterminate`: the batch is then all
    /// or nothing, but which is not known. Read the keys back with
    /// [`KvView::get_many`] before deciding anything.
    pub fn write_batch(&mut self, operations: &[KvOp<'_>]) -> Result<Vec<KvOutcome>> {
        self.write_batch_at(operations, current_time_ms())
    }

    /// `write_batch` with a caller-supplied Unix-millisecond clock for expiry
    /// checks.
    pub fn write_batch_at(
        &mut self,
        operations: &[KvOp<'_>],
        now_ms: u64,
    ) -> Result<Vec<KvOutcome>> {
        let plan = batch::plan(self.db, self.id, operations, now_ms)?;
        self.commit(plan)
    }

    /// `write_batch_at` with a gate between resolving the batch and
    /// committing it. `gate` is called once, with the database as it still
    /// stands (the batch is not written) and each slot the batch writes or
    /// deletes, in order, with whether a record is there afterwards: what the
    /// live-record quota is judged on. It refuses the batch by returning an
    /// error, and then nothing is written. A batch that cannot run fails as
    /// `write_batch_at` does for it, and the gate is not called. The resolved
    /// batch never leaves this call, so what the gate judged is what is
    /// committed.
    #[cfg(feature = "server")]
    pub(crate) fn write_batch_gated_at<G>(
        &mut self,
        operations: &[KvOp<'_>],
        now_ms: u64,
        gate: impl FnOnce(&Database, &[(u64, bool)]) -> std::result::Result<(), G>,
    ) -> std::result::Result<Vec<KvOutcome>, Gated<G>> {
        let plan = batch::plan(self.db, self.id, operations, now_ms).map_err(Gated::Batch)?;
        let changes = batch::changes(&plan.mutations);
        gate(self.db, &changes).map_err(Gated::Refused)?;
        self.commit(plan).map_err(Gated::Batch)
    }

    fn commit(&mut self, plan: batch::Plan) -> Result<Vec<KvOutcome>> {
        // A batch that changes no record goes to the engine too: an empty
        // engine batch writes nothing, on a read-only or format 1 handle as
        // well, so no frame is written for it.
        self.db.write_batch_in(self.id, &plan.mutations)?;
        // `write_batch_in` commits one frame, and every record of a frame
        // takes its sequence as its revision.
        let version = self.db.sequence;
        Ok(plan
            .outcomes
            .into_iter()
            .map(|planned| match planned {
                batch::Planned::Put => KvOutcome::Put { version },
                batch::Planned::Deleted(deleted) => KvOutcome::Deleted(deleted),
            })
            .collect())
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

/// What an earlier operation of the same batch has decided for a slot.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// Written: it holds another key, whatever the stored record says.
    Taken,
    /// Deleted: it is free, whatever the stored record says.
    Freed,
}

fn probe(db: &Database, collection: u16, namespace: &str, key: &str, now_ms: u64) -> Result<Probe> {
    probe_pending(db, collection, namespace, key, now_ms, None)
}

/// `probe` as an earlier operation of the same batch leaves the collection.
/// `pending` holds the slots it wrote or freed and takes precedence over the
/// stored records, so a key looks at the collection as if the earlier writes
/// had been applied. A batch names each key once, so `pending` never holds this
/// key's own live record. It can hold the slot of an expired leftover of this
/// key that an earlier put reused, and then the key reads as absent.
fn probe_pending(
    db: &Database,
    collection: u16,
    namespace: &str,
    key: &str,
    now_ms: u64,
    pending: Option<&HashMap<u64, Slot>>,
) -> Result<Probe> {
    let state = db.state(collection);
    let records = state.records.resident()?;
    let base = slot_base(namespace, key);
    let mut free = None;
    for offset in 0..KV_PROBE_SLOTS {
        let slot = base.wrapping_add(offset);
        match pending.and_then(|pending| pending.get(&slot)) {
            Some(Slot::Taken) => continue,
            Some(Slot::Freed) => {
                free.get_or_insert((slot, None));
                continue;
            }
            None => {}
        }
        let Some(record) = records.get(&slot) else {
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
                return Ok(Probe {
                    found: Some(Found {
                        slot,
                        revision,
                        expired,
                    }),
                    free,
                });
            }
            // Expired entries of other keys are reusable; foreign records are not.
            Some(_) if expired => {
                free.get_or_insert((slot, Some(revision)));
            }
            _ => {}
        }
    }
    Ok(Probe { found: None, free })
}

fn get_at(
    db: &Database,
    collection: u16,
    namespace: &str,
    key: &str,
    now_ms: u64,
) -> Result<Option<KvEntry>> {
    db.ensure_resident_in(collection)?;
    validate(namespace, key)?;
    let probe = probe(db, collection, namespace, key, now_ms)?;
    let Some(found) = probe.found.filter(|found| !found.expired) else {
        return Ok(None);
    };
    entry(db, collection, found.slot)
}

fn entry(db: &Database, collection: u16, slot: u64) -> Result<Option<KvEntry>> {
    let state = db.state(collection);
    let records = state.records.resident()?;
    let Some(record) = records.get(&slot) else {
        return Ok(None);
    };
    let Some((_, key, value)) = decode(&record.payload) else {
        return Ok(None);
    };
    Ok(Some(KvEntry {
        key: key.to_owned(),
        value: value.to_vec(),
        version: state.revisions.get(&slot).copied().unwrap_or_default(),
        expires_at_ms: state.expiries.get(&slot).copied(),
    }))
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
    let mut entries = Vec::new();
    visit_at(
        db,
        collection,
        namespace,
        prefix,
        after,
        limit,
        now_ms,
        |entry| {
            entries.push(KvEntry {
                key: entry.key.to_owned(),
                value: entry.value.to_vec(),
                version: entry.version,
                expires_at_ms: entry.expires_at_ms,
            });
            ControlFlow::Continue(())
        },
    )?;
    Ok(entries)
}

#[cfg(test)]
thread_local! {
    /// The largest candidate set one scan of this thread has held.
    static CANDIDATE_HIGH_WATER: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// How many candidates the scans of this thread have probed.
    static PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[allow(clippy::too_many_arguments)]
fn visit_at<F>(
    db: &Database,
    collection: u16,
    namespace: &str,
    prefix: &str,
    after: Option<&str>,
    limit: usize,
    now_ms: u64,
    mut visitor: F,
) -> Result<()>
where
    F: FnMut(KvRef<'_>) -> ControlFlow<()>,
{
    db.ensure_resident_in(collection)?;
    validate_namespace(namespace)?;
    if limit == 0 || limit > MAX_KV_LIST_LIMIT {
        return Err(Error::InvalidKvKey("limit must be between 1 and 10000"));
    }
    let state = db.state(collection);
    let records = state.records.resident()?;
    // `limit` entries plus one more, so the last visited entry can say whether
    // the listing continues without anything being read for the extra key.
    let capacity = limit + 1;
    let mut selected: BinaryHeap<(&str, u64)> =
        BinaryHeap::with_capacity(capacity.min(records.len()));
    for (&id, record) in records.iter() {
        if state
            .expiries
            .get(&id)
            .is_some_and(|expiry| *expiry <= now_ms)
        {
            continue;
        }
        let Some((ns, key, _)) = decode(&record.payload) else {
            continue;
        };
        if ns != namespace || !key.starts_with(prefix) || after.is_some_and(|after| key <= after) {
            continue;
        }
        let candidate = (key, id);
        if selected.len() == capacity && selected.peek().is_some_and(|worst| candidate >= *worst) {
            continue;
        }
        // `probe` resolves a key only to a record of its own window, the
        // `KV_PROBE_SLOTS` IDs from its first slot (wrapping), so a record
        // outside that window is not an entry: it costs one hash, no lookup.
        if id.wrapping_sub(slot_base(namespace, key)) >= KV_PROBE_SLOTS {
            continue;
        }
        // A record inside the window is that key's entry only when `probe`, the
        // lookup behind `get`, `put` and `delete`, resolves the key to it. A
        // look-alike behind an earlier holder of the key never enters the set:
        // it would otherwise use up room that later real entries need, and the
        // listing would come back short although more entries exist.
        #[cfg(test)]
        PROBES.with(|probes| probes.set(probes.get() + 1));
        let resolved = probe(db, collection, namespace, key, now_ms)?.found;
        if !resolved.is_some_and(|found| found.slot == id) {
            continue;
        }
        if selected.len() == capacity {
            selected.pop();
        }
        selected.push(candidate);
        #[cfg(test)]
        CANDIDATE_HIGH_WATER.with(|mark| mark.set(mark.get().max(selected.len())));
    }
    let selected = selected.into_sorted_vec();
    for (index, &(key, id)) in selected.iter().take(limit).enumerate() {
        let Some((_, _, value)) = records.get(&id).and_then(|record| decode(&record.payload))
        else {
            continue;
        };
        let entry = KvRef {
            key,
            value,
            version: state.revisions.get(&id).copied().unwrap_or_default(),
            expires_at_ms: state.expiries.get(&id).copied(),
            has_next: index + 1 < selected.len(),
        };
        if visitor(entry).is_break() {
            break;
        }
    }
    Ok(())
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
mod batch_tests;
#[cfg(test)]
mod bounded_tests;

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
