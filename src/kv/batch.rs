//! The planner behind `KvStore::write_batch` and the reader behind
//! `KvView::get_many`; the contract is on those methods.
//!
//! A batch is resolved here, against the live collection and its own pending
//! writes, into the engine's `Mutation` list, which the engine commits as one
//! frame. Nothing here writes.

use super::{
    KvOp, KvRef, MAX_KV_BATCH_OPERATIONS, MAX_KV_BATCH_VALUE_BYTES, MAX_KV_GET_MANY_KEYS,
    MAX_KV_VALUE_BYTES, PLACEHOLDER, Slot, decode, encode, probe, probe_pending, validate,
};
use crate::{Database, Error, Mutation, Result};
use std::{
    collections::{HashMap, HashSet},
    ops::ControlFlow,
};

/// A batch checked and resolved to what the engine commits.
pub(super) struct Plan {
    /// In order; empty when no record changes.
    pub(super) mutations: Vec<Mutation>,
    /// One for each operation, in order.
    pub(super) outcomes: Vec<Planned>,
}

/// The result of one operation before the commit gives the puts their version.
pub(super) enum Planned {
    Put,
    Deleted(bool),
}

pub(super) fn plan(
    db: &Database,
    collection: u16,
    operations: &[KvOp<'_>],
    now_ms: u64,
) -> Result<Plan> {
    check(operations)?;
    db.ensure_resident_in(collection)?;
    let mut pending: HashMap<u64, Slot> = HashMap::with_capacity(operations.len());
    let mut mutations = Vec::with_capacity(operations.len());
    let mut outcomes = Vec::with_capacity(operations.len());
    for operation in operations {
        match *operation {
            KvOp::Put {
                namespace,
                key,
                value,
                options,
            } => {
                let probe = probe_pending(db, collection, namespace, key, now_ms, Some(&pending))?;
                options.expected.check(probe.live_version())?;
                let slot = match probe.found {
                    Some(found) => found.slot,
                    None => probe.free.ok_or(Error::KvSlotsExhausted)?.0,
                };
                pending.insert(slot, Slot::Taken);
                mutations.push(Mutation::Put {
                    id: slot,
                    vector: PLACEHOLDER.to_vec(),
                    payload: encode(namespace, key, value),
                });
                outcomes.push(Planned::Put);
            }
            KvOp::Delete {
                namespace,
                key,
                expected,
            } => {
                let probe = probe_pending(db, collection, namespace, key, now_ms, Some(&pending))?;
                expected.check(probe.live_version())?;
                let Some(found) = probe.found else {
                    outcomes.push(Planned::Deleted(false));
                    continue;
                };
                if db.is_attached(collection, found.slot) {
                    return Err(Error::GraphAttachedRecord);
                }
                pending.insert(found.slot, Slot::Freed);
                mutations.push(Mutation::Delete { id: found.slot });
                // A due leftover is removed like a live entry, and the key
                // reports no deletion.
                outcomes.push(Planned::Deleted(!found.expired));
            }
        }
    }
    Ok(Plan {
        mutations,
        outcomes,
    })
}

/// The checks that need no state, over the whole batch and in order, so a batch
/// that cannot run is refused before any operation is resolved.
fn check(operations: &[KvOp<'_>]) -> Result<()> {
    if operations.len() > MAX_KV_BATCH_OPERATIONS {
        return Err(Error::InvalidKvKey("a batch takes at most 1000 operations"));
    }
    let mut named: HashSet<(&str, &str)> = HashSet::with_capacity(operations.len());
    let mut value_bytes = 0usize;
    for operation in operations {
        let (namespace, key) = match *operation {
            KvOp::Put {
                namespace,
                key,
                value,
                options,
            } => {
                validate(namespace, key)?;
                if options.expires_at_ms.is_some() {
                    return Err(Error::InvalidKvKey("a batch put cannot carry an expiry"));
                }
                if value.len() > MAX_KV_VALUE_BYTES {
                    return Err(Error::PayloadTooLarge(value.len()));
                }
                value_bytes += value.len();
                if value_bytes > MAX_KV_BATCH_VALUE_BYTES {
                    return Err(Error::InvalidKvKey(
                        "the values of a batch total at most 4194304 bytes",
                    ));
                }
                (namespace, key)
            }
            KvOp::Delete { namespace, key, .. } => {
                validate(namespace, key)?;
                (namespace, key)
            }
        };
        if !named.insert((namespace, key)) {
            return Err(Error::InvalidKvKey("a batch names each key at most once"));
        }
    }
    Ok(())
}

pub(super) fn visit_many<F>(
    db: &Database,
    collection: u16,
    keys: &[(&str, &str)],
    now_ms: u64,
    mut visitor: F,
) -> Result<()>
where
    F: FnMut(usize, Option<KvRef<'_>>) -> ControlFlow<()>,
{
    if keys.len() > MAX_KV_GET_MANY_KEYS {
        return Err(Error::InvalidKvKey("get-many takes at most 1000 keys"));
    }
    for &(namespace, key) in keys {
        validate(namespace, key)?;
    }
    db.ensure_resident_in(collection)?;
    let state = db.state(collection);
    let records = state.records.resident()?;
    for (index, &(namespace, key)) in keys.iter().enumerate() {
        let found = probe(db, collection, namespace, key, now_ms)?
            .found
            .filter(|found| !found.expired);
        let entry = found.and_then(|found| {
            let (_, key, value) = decode(&records.get(&found.slot)?.payload)?;
            Some(KvRef {
                key,
                value,
                version: found.revision,
                expires_at_ms: state.expiries.get(&found.slot).copied(),
                has_next: index + 1 < keys.len(),
            })
        });
        if visitor(index, entry).is_break() {
            break;
        }
    }
    Ok(())
}

/// Each slot the plan writes or deletes with whether a record is there
/// afterwards, as the quota check takes them.
#[cfg(feature = "server")]
pub(super) fn changes(mutations: &[Mutation]) -> Vec<(u64, bool)> {
    mutations
        .iter()
        .map(|mutation| match mutation {
            Mutation::Put { id, .. } | Mutation::PutWithMetadata { id, .. } => (*id, true),
            Mutation::Delete { id } => (*id, false),
        })
        .collect()
}
