//! Gap 3 (unreleased): the numbered events and paging units of one commit of
//! the [`GraphEventSet::Ttl`](super::GraphEventSet::Ttl) feed.
//!
//! A logged reclaim can remove more than one page of entities, so its
//! operations are split into deterministic chunks: greedily, at most
//! [`CHUNK_OPERATIONS`] operations and [`CHUNK_BYTES`] canonical bytes per
//! event, measured with the event's actual ordinal. The canonical bytes are
//! the compact UTF-8 JSON of the event envelope
//! `{"change":B,"collection":"","committed_at_ms":T,"ordinal":O,"sequence":"S"}`
//! with object keys sorted, counted here without the JSON feature and
//! without building the text. Ordinary events between reclaim runs, and
//! every commit that must stay whole, are indivisible units.

use super::{Change, ChangeKind, GraphChange, GraphFeedChange, GraphFeedError, GraphTtl};
use crate::Error;
use crate::property_graph::GraphRemovalReason;
use crate::property_graph::ttl_feed::TtlEvent;

#[cfg(test)]
mod tests;

/// Most operations in one reclaim chunk.
pub(crate) const CHUNK_OPERATIONS: usize = 256;
/// Most canonical bytes of a reclaim chunk of more than one operation.
pub(crate) const CHUNK_BYTES: usize = 1_048_576;
/// Most canonical bytes of any reclaim chunk, even of one operation.
pub(crate) const EVENT_LIMIT: usize = 16_777_216;

/// Largest event ordinal; `u32::MAX` marks a commit boundary in tokens.
const LAST_ORDINAL: usize = u32::MAX as usize - 1;

/// The byte length of compact JSON text, `None` once it overflowed.
struct Count(Option<usize>);

impl Count {
    fn new() -> Self {
        Self(Some(0))
    }

    fn add(&mut self, bytes: usize) {
        self.0 = self.0.and_then(|total| total.checked_add(bytes));
    }

    fn raw(&mut self, text: &str) {
        self.add(text.len());
    }

    /// A JSON integer.
    fn decimal(&mut self, value: u64) {
        self.add(value.checked_ilog10().map_or(1, |log| log as usize + 1));
    }

    /// A decimal string.
    fn quoted(&mut self, value: u64) {
        self.add(2);
        self.decimal(value);
    }

    /// A JSON string: `\"`, `\\`, `\b`, `\t`, `\n`, `\f`, `\r`, `\u00xx`
    /// for the other controls, raw UTF-8 bytes otherwise.
    fn string(&mut self, text: &str) {
        self.add(2);
        for byte in text.bytes() {
            self.add(match byte {
                b'"' | b'\\' | 0x08 | b'\t' | b'\n' | 0x0c | b'\r' => 2,
                0..=0x1f => 6,
                _ => 1,
            });
        }
    }

    /// The total, saturated on overflow (above every limit).
    fn total(&self) -> usize {
        self.0.unwrap_or(usize::MAX)
    }
}

/// A delete's removal fields: the deadline, the reason and the reclaim
/// clock; an explicit removal has only its reason.
fn removal(ttl: &GraphTtl) -> Option<(Option<u64>, &'static str, Option<u64>)> {
    match ttl {
        GraphTtl::ExplicitRemoval => Some((None, "explicit", None)),
        GraphTtl::ExpiryRemoval {
            removal_reason,
            expires_at_ms,
            removed_at_ms,
        } => {
            let reason = match removal_reason {
                GraphRemovalReason::Expired => "expired",
                GraphRemovalReason::NodeExpired => "node_expired",
            };
            Some((Some(*expires_at_ms), reason, Some(*removed_at_ms)))
        }
        _ => None,
    }
}

/// Canonical bytes of one reclaim operation with its annotation (keys in
/// ASCII order): a node or edge delete with its removal, or a detach
/// without one. `None` for any other shape, which no reclaim produces.
fn operation_bytes(operation: &GraphChange, ttl: Option<&GraphTtl>) -> Option<usize> {
    let mut count = Count::new();
    count.raw("{");
    match (operation, ttl) {
        (GraphChange::NodeDeleted { .. } | GraphChange::EdgeDeleted { .. }, Some(ttl)) => {
            let (deadline, reason, removed) = removal(ttl)?;
            if let Some(deadline) = deadline {
                count.raw(r#""expires_at_ms":"#);
                count.quoted(deadline);
                count.raw(",");
            }
            match operation {
                GraphChange::NodeDeleted { id, key } => {
                    count.raw(r#""id":"#);
                    count.quoted(*id);
                    count.raw(r#","key":"#);
                    count.string(key);
                    count.raw(r#","op":"node_deleted""#);
                }
                GraphChange::EdgeDeleted { id, key } => {
                    count.raw(r#""id":"#);
                    count.quoted(*id);
                    count.raw(r#","key":"#);
                    match key {
                        Some(key) => count.string(key),
                        None => count.raw("null"),
                    }
                    count.raw(r#","op":"edge_deleted""#);
                }
                _ => return None,
            }
            count.raw(r#","removal_reason":"#);
            count.string(reason);
            if let Some(removed) = removed {
                count.raw(r#","removed_at_ms":"#);
                count.quoted(removed);
            }
        }
        (
            GraphChange::Detached {
                node,
                node_key,
                collection,
                record,
            },
            None,
        ) => {
            count.raw(r#""collection":"#);
            count.string(collection);
            count.raw(r#","node":"#);
            count.quoted(*node);
            count.raw(r#","node_key":"#);
            count.string(node_key);
            count.raw(r#","op":"detached","record":"#);
            count.quoted(*record);
        }
        _ => return None,
    }
    count.raw("}");
    Some(count.total())
}

/// Canonical bytes of a `graph_batch` event at `ordinal` without its
/// operations and the commas between them.
fn envelope_bytes(
    sequence: u64,
    committed_at_ms: Option<u64>,
    ordinal: usize,
    graph: &str,
) -> usize {
    let mut count = Count::new();
    count.raw(r#"{"change":{"graph":"#);
    count.string(graph);
    count.raw(r#","operations":[],"revision":"#);
    count.quoted(sequence);
    count.raw(r#","type":"graph_batch"},"collection":"","committed_at_ms":"#);
    match committed_at_ms {
        Some(time) => count.decimal(time),
        None => count.raw("null"),
    }
    count.raw(r#","ordinal":"#);
    count.decimal(ordinal as u64);
    count.raw(r#","sequence":"#);
    count.quoted(sequence);
    count.raw("}");
    count.total()
}

/// One commit's events, numbered by ordinal from zero, and the paging units
/// they form: `ends` holds each unit's exclusive end index, ascending, the
/// last being `events.len()`.
pub(crate) struct Expanded {
    pub(crate) events: Vec<GraphFeedChange>,
    pub(crate) ends: Vec<usize>,
}

/// Numbers the TTL projection of commit `sequence` (its frame at `offset`):
/// each reclaim event is split into chunks, each its own unit, and the
/// other events between them form one unit per run. A `whole` commit is
/// one unit and is never split. A chunk of one operation above
/// [`EVENT_LIMIT`] is `ResponseTooLarge`, and nothing of the commit is
/// returned.
pub(crate) fn expand(
    sequence: u64,
    committed_at_ms: Option<u64>,
    events: Vec<TtlEvent>,
    whole: bool,
    offset: u64,
) -> Result<Expanded, GraphFeedError> {
    let corrupt = |reason| GraphFeedError::Engine(Error::Corrupt { offset, reason });
    let mut expanded = Expanded {
        events: Vec::new(),
        ends: Vec::new(),
    };
    let numbered = |expanded: &mut Expanded, collection, kind, ttl| {
        if expanded.events.len() > LAST_ORDINAL {
            return Err(corrupt("graph feed ordinal overflow"));
        }
        expanded.events.push(GraphFeedChange {
            change: Change {
                sequence,
                ordinal: expanded.events.len() as u32,
                collection,
                committed_at_ms,
                kind,
            },
            ttl,
        });
        Ok(())
    };
    let close = |expanded: &mut Expanded| {
        if expanded.ends.last().copied().unwrap_or(0) < expanded.events.len() {
            expanded.ends.push(expanded.events.len());
        }
    };
    for event in events {
        let (graph, revision, operations) = match event.kind {
            ChangeKind::GraphBatch {
                graph,
                revision,
                operations,
            } if event.reclaim && !whole => (graph, revision, operations),
            kind => {
                numbered(&mut expanded, event.collection, kind, event.ttl)?;
                continue;
            }
        };
        if operations.len() != event.ttl.len() {
            return Err(corrupt("invalid graph feed annotation"));
        }
        close(&mut expanded);
        let mut chunk: Vec<GraphChange> = Vec::new();
        let mut annotations: Vec<Option<GraphTtl>> = Vec::new();
        // Canonical bytes of the chunk's operations and their commas.
        let mut body = 0usize;
        let emit = |expanded: &mut Expanded,
                    chunk: &mut Vec<GraphChange>,
                    annotations: &mut Vec<Option<GraphTtl>>| {
            let kind = ChangeKind::GraphBatch {
                graph: graph.clone(),
                revision,
                operations: std::mem::take(chunk),
            };
            numbered(expanded, String::new(), kind, std::mem::take(annotations))?;
            close(expanded);
            Ok::<(), GraphFeedError>(())
        };
        for (operation, ttl) in operations.into_iter().zip(event.ttl) {
            let size = operation_bytes(&operation, ttl.as_ref())
                .ok_or_else(|| corrupt("invalid graph feed annotation"))?;
            if !chunk.is_empty() {
                let candidate =
                    envelope_bytes(sequence, committed_at_ms, expanded.events.len(), &graph)
                        .saturating_add(body)
                        .saturating_add(1)
                        .saturating_add(size);
                if chunk.len() < CHUNK_OPERATIONS && candidate <= CHUNK_BYTES {
                    chunk.push(operation);
                    annotations.push(ttl);
                    body = body.saturating_add(1).saturating_add(size);
                    continue;
                }
                emit(&mut expanded, &mut chunk, &mut annotations)?;
            }
            let bytes = envelope_bytes(sequence, committed_at_ms, expanded.events.len(), &graph)
                .saturating_add(size);
            if bytes > EVENT_LIMIT {
                return Err(GraphFeedError::ResponseTooLarge {
                    bytes,
                    limit: EVENT_LIMIT,
                });
            }
            chunk.push(operation);
            annotations.push(ttl);
            body = size;
        }
        if !chunk.is_empty() {
            emit(&mut expanded, &mut chunk, &mut annotations)?;
        }
    }
    close(&mut expanded);
    Ok(expanded)
}
