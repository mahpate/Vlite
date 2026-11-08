//! Bounded reclamation of expired A2 and C1 receipt groups.
//!
//! A successful, non-replayed remember, forget or consolidate write plans a
//! bounded number of `DeleteNode` operations for receipt groups (a receipt
//! plus its outcome chunks) whose `expires_at_ms` lies at least
//! [`RECEIPT_RECLAIM_GRACE_MS`] before the request clock, and prepends them to
//! its own batch. Planning examines one seeded window of the receipt label
//! index, skips anything it does not fully recognize and never surfaces a read
//! error; the write falls back to the own operations alone when the combined
//! batch fails before its frame.

use std::collections::BTreeMap;

use super::consolidate::{
    Field, OUTCOME_FIELDS, RECEIPT_FIELDS, SCHEMA, canonical_decimal, digest_shaped, exact_node,
    integer, text,
};
use super::{
    LABEL_RECEIPT, LABEL_RECEIPT_OUTCOME, MAX_BODY_BYTES, MAX_STRING_VALUE_BYTES, erasure,
    receipt_outcome_key,
};
use crate::{
    Database, Error, Expected,
    property_graph::{
        Direction, GraphBatchResult, GraphOp, GraphRef, Neighbors, NodeSelector, NodeView,
        ReadOptions,
    },
};

/// Minimum time after `expires_at_ms` before a group may be deleted.
pub(super) const RECEIPT_RECLAIM_GRACE_MS: u64 = 300_000;
/// Label members visited per write (the window size).
pub(super) const RECLAIM_SCAN: usize = 512;
/// Spreads the window starts of successive commits over the node ID space.
pub(super) const RECLAIM_SEED_MULTIPLIER: u64 = 0x9E37_79B9_7F4A_7C15;
/// Delete operations per write, except the first group.
pub(super) const RECLAIM_MAX_OPS: usize = 1_024;
/// Point reads per write after discovery.
pub(super) const RECLAIM_MAX_READS: usize = 16_384;

/// The most outcome chunks a receipt can name (the `decode_outcome` bound).
const MAX_CHUNKS: usize = MAX_BODY_BYTES.div_ceil(MAX_STRING_VALUE_BYTES);

/// The exact A2 receipt written by remember and forget.
const A2_FIELDS: &[(&str, Field)] = &[
    ("scope", Field::String),
    ("digest", Field::String),
    ("expires_at_ms", Field::String),
    ("outcome_chunks", Field::Integer),
];

#[cfg(test)]
thread_local! {
    /// Makes the next combined attempt return this error without calling the engine.
    pub(super) static FAIL_RECLAIM_ATTEMPT: std::cell::Cell<Option<fn() -> crate::Error>> =
        const { std::cell::Cell::new(None) };
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Limits {
    pub(super) scan: usize,
    pub(super) max_ops: usize,
    pub(super) max_reads: usize,
}

impl Limits {
    pub(super) const DEFAULT: Limits = Limits {
        scan: RECLAIM_SCAN,
        max_ops: RECLAIM_MAX_OPS,
        max_reads: RECLAIM_MAX_READS,
    };
}

/// The delete operations of one bounded window, seeded by the handle's view.
pub(super) fn plan(
    read: &GraphRef<'_>,
    own_receipt_key: &str,
    now_ms: u64,
    headroom: usize,
) -> Vec<GraphOp> {
    plan_with(
        read,
        own_receipt_key,
        now_ms,
        headroom,
        read.view_sequence().wrapping_mul(RECLAIM_SEED_MULTIPLIER),
        Limits::DEFAULT,
    )
}

pub(super) fn plan_with(
    read: &GraphRef<'_>,
    own_receipt_key: &str,
    now_ms: u64,
    headroom: usize,
    seed: u64,
    limits: Limits,
) -> Vec<GraphOp> {
    let mut planned = Vec::new();
    if headroom == 0 {
        return planned;
    }
    let mut reader = Reader {
        read,
        reads: 0,
        max_reads: limits.max_reads,
        barriers: BTreeMap::new(),
    };
    for id in read.memory_label_window(LABEL_RECEIPT, seed, limits.scan) {
        let cap = if planned.is_empty() {
            headroom
        } else {
            headroom.min(limits.max_ops)
        };
        if planned.len() + 2 > cap {
            // Even the smallest group (a receipt and one chunk) cannot fit.
            break;
        }
        let Ok(found) = read.get_node_by_id(id) else {
            break;
        };
        let Some(receipt) = found.output else {
            continue;
        };
        let Some(chunks) = chunk_count(&receipt, own_receipt_key, now_ms) else {
            continue;
        };
        if planned.len() + 1 + chunks > cap {
            continue;
        }
        match reader.group(&receipt, chunks, cap - planned.len()) {
            Ok(Some(group)) => planned.extend(group),
            Ok(None) => {}
            Err(Exhausted) => break,
        }
    }
    planned
}

/// Plans the reclamation of expired receipts at the request clock and writes
/// it ahead of `ops`: the one entry point of a committing remember, forget or
/// consolidate. `headroom` is the operation count the caller's own batch
/// leaves free.
pub(super) fn commit(
    db: &mut Database,
    graph: &str,
    own_receipt_key: &str,
    now_ms: u64,
    headroom: usize,
    ops: Vec<GraphOp>,
) -> crate::Result<GraphBatchResult> {
    let reclaim = plan(
        &db.graph(graph)?.with_clock(now_ms),
        own_receipt_key,
        now_ms,
        headroom,
    );
    write(db, graph, reclaim, ops, now_ms)
}

/// The batch `reclaim` then `ops` at the request clock; on a pre-frame
/// planning error of that attempt, `ops` alone, exactly once.
pub(super) fn write(
    db: &mut Database,
    graph: &str,
    mut reclaim: Vec<GraphOp>,
    ops: Vec<GraphOp>,
    now_ms: u64,
) -> crate::Result<GraphBatchResult> {
    if reclaim.is_empty() {
        return db.graph_mut(graph)?.write_batch_at(&ops, now_ms);
    }
    let own_start = reclaim.len();
    reclaim.extend(ops);
    match combined(db, graph, &reclaim, now_ms) {
        Err(
            Error::BatchTooLarge
            | Error::GraphNodeNotFound
            | Error::GraphNodeHasEdges { .. }
            | Error::RevisionMismatch { .. },
        ) => {
            let ops = reclaim.split_off(own_start);
            db.graph_mut(graph)?.write_batch_at(&ops, now_ms)
        }
        other => other,
    }
}

fn combined(
    db: &mut Database,
    graph: &str,
    batch: &[GraphOp],
    now_ms: u64,
) -> crate::Result<GraphBatchResult> {
    #[cfg(test)]
    if let Some(error) = FAIL_RECLAIM_ATTEMPT.with(std::cell::Cell::take) {
        return Err(error());
    }
    db.graph_mut(graph)?.write_batch_at(batch, now_ms)
}

/// Rules 1-9: the outcome chunk count of an eligible receipt, from the
/// receipt node alone.
fn chunk_count(receipt: &NodeView, own_receipt_key: &str, now_ms: u64) -> Option<usize> {
    if receipt.key == own_receipt_key || receipt.expires_at_ms.is_some() {
        return None;
    }
    let a2 = exact_node(receipt, LABEL_RECEIPT, A2_FIELDS);
    let c1 = exact_node(receipt, LABEL_RECEIPT, RECEIPT_FIELDS)
        && text(receipt, "c1_schema") == SCHEMA
        && integer(receipt, "outcome_chunks") == Some(1);
    if !a2 && !c1 {
        return None;
    }
    let scope = text(receipt, "scope");
    let scope_shaped = scope.strip_prefix("scope:v1:").is_some_and(lower_hex_64);
    let key_shaped = receipt
        .key
        .strip_prefix("receipt:v1:")
        .and_then(|rest| rest.strip_prefix(scope))
        .and_then(|rest| rest.strip_prefix(':'))
        .is_some_and(lower_hex_64);
    if !scope_shaped || !key_shaped || !digest_shaped(text(receipt, "digest")) {
        return None;
    }
    let expires_at_ms = canonical_decimal(text(receipt, "expires_at_ms"), 20)?;
    let chunks = usize::try_from(integer(receipt, "outcome_chunks")?)
        .ok()
        .filter(|chunks| (1..=MAX_CHUNKS).contains(chunks))?;
    let eligible_from = expires_at_ms.checked_add(RECEIPT_RECLAIM_GRACE_MS)?;
    (now_ms >= eligible_from).then_some(chunks)
}

fn lower_hex_64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// The read budget ran out or a read failed: planning stops.
struct Exhausted;

/// Counted point reads for rules 10-12.
struct Reader<'a, 'db> {
    read: &'a GraphRef<'db>,
    reads: usize,
    max_reads: usize,
    /// Rule 10 per scope: true when the scope has no erasure barrier.
    barriers: BTreeMap<String, bool>,
}

impl Reader<'_, '_> {
    fn charge(&mut self) -> Result<(), Exhausted> {
        if self.reads >= self.max_reads {
            return Err(Exhausted);
        }
        self.reads += 1;
        Ok(())
    }

    fn node(&mut self, key: &str) -> Result<Option<NodeView>, Exhausted> {
        self.charge()?;
        self.read
            .get_node(key)
            .map(|found| found.output)
            .map_err(|_| Exhausted)
    }

    /// Rule 10, cached per scope.
    fn open(&mut self, scope: &str) -> Result<bool, Exhausted> {
        if let Some(open) = self.barriers.get(scope) {
            return Ok(*open);
        }
        self.charge()?;
        let open = erasure::barrier(self.read, scope)
            .map_err(|_| Exhausted)?
            .is_none();
        self.barriers.insert(scope.to_owned(), open);
        Ok(open)
    }

    /// Rule 12: no edge and no attachment.
    fn isolated(&mut self, key: &str) -> Result<bool, Exhausted> {
        self.charge()?;
        let edges = self
            .read
            .neighbors(
                &Neighbors {
                    node: NodeSelector::key(key),
                    direction: Direction::Both,
                    types: None,
                    edge_conditions: Vec::new(),
                    limit: Some(1),
                    cursor: None,
                },
                &ReadOptions::default(),
            )
            .map_err(|_| Exhausted)?;
        if !edges.complete || !edges.output.edges.is_empty() {
            return Ok(false);
        }
        self.charge()?;
        let attachments = self
            .read
            .memory_node_attachments(key)
            .map_err(|_| Exhausted)?;
        Ok(attachments.is_empty())
    }

    /// Rules 10-12 for one receipt with `chunks` named chunks: the group's
    /// deletes (receipt, chunks, surplus chunks), or `None` when the group is
    /// skipped or exceeds `room`.
    fn group(
        &mut self,
        receipt: &NodeView,
        chunks: usize,
        room: usize,
    ) -> Result<Option<Vec<GraphOp>>, Exhausted> {
        if !self.open(text(receipt, "scope"))? || !self.isolated(&receipt.key)? {
            return Ok(None);
        }
        let mut group = vec![GraphOp::DeleteNode {
            key: receipt.key.clone(),
            expected: Expected::Revision(receipt.revision),
        }];
        // Probing stops at the first absent chunk past the named ones. A
        // surplus chunk behind a gap, past `MAX_CHUNKS` or hidden by its own
        // expiry is never found and stays behind: storage only, and only a
        // raw graph write can create one.
        for index in 0..MAX_CHUNKS {
            let key = receipt_outcome_key(&receipt.key, index);
            let Some(chunk) = self.node(&key)? else {
                if index < chunks {
                    return Ok(None);
                }
                break;
            };
            if group.len() == room || !chunk_shaped(&chunk, &receipt.key, index) {
                return Ok(None);
            }
            if !self.isolated(&key)? {
                return Ok(None);
            }
            group.push(GraphOp::DeleteNode {
                key,
                expected: Expected::Revision(chunk.revision),
            });
        }
        Ok(Some(group))
    }
}

/// Rule 11 for one chunk.
fn chunk_shaped(chunk: &NodeView, receipt_key: &str, index: usize) -> bool {
    exact_node(chunk, LABEL_RECEIPT_OUTCOME, OUTCOME_FIELDS)
        && chunk.expires_at_ms.is_none()
        && text(chunk, "receipt_key") == receipt_key
        && integer(chunk, "index").and_then(|value| usize::try_from(value).ok()) == Some(index)
}

#[cfg(test)]
mod tests;
