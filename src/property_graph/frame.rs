//! Op-12 frame adapter (DO-003, ITEM-011): vector-mutation entries and
//! replay of graph bodies that interleave graph, attachment and vector
//! entries.
//!
//! The frame envelope, sealing and commit are the existing `format` writer
//! (`format::graph_frame`, `Database::commit_frame`). A vector entry carries
//! exactly one canonical batch entry (`format::encode_batch_entries`) for one
//! collection, so vector data in graph frames is byte-identical to op-3
//! batch entries.

use std::collections::BTreeMap;

use crate::compact::catalog::Pairs;
use crate::format::{self, LoadedCollection};
use crate::{Error, Mutation, Record, Result};

use super::engine::codec::BODY_HEADER_BYTES;
use super::engine::limits::{MAX_BATCH_BYTES, MAX_BATCH_ENTRIES};
use super::engine::{
    self, Graph, GraphBody, GraphEdge, GraphEntry, GraphId, GraphNode, GraphState, SnapshotBuilder,
};

/// Encodes one vector mutation of `collection` as a graph-frame entry.
pub(crate) fn vector_entry(collection: u16, mutation: &Mutation) -> GraphEntry {
    let mut entry = Vec::new();
    format::encode_batch_entries(std::slice::from_ref(mutation), &mut entry);
    GraphEntry::VectorMutation { collection, entry }
}

fn corrupt(offset: u64, reason: &'static str) -> Error {
    Error::Corrupt { offset, reason }
}

/// The op-12 prefix ID of `body`: its reclaim entries (zero without them).
pub(crate) fn reclaim_count(body: &GraphBody) -> u64 {
    body.entries
        .iter()
        .filter(|entry| matches!(entry, GraphEntry::Reclaim { .. }))
        .count() as u64
}

/// Checks the prefix of an op-12 frame, or of an op-15 snapshot frame when
/// `snapshot`, at `offset`: [`super::valid_frame_prefix`], and ID 0 for
/// snapshots, whose bodies hold no reclaims. The loader applies it before
/// treating a short final frame as a torn tail.
pub(crate) fn check_prefix(
    snapshot: bool,
    id: u64,
    count: usize,
    payload_len: usize,
    offset: u64,
) -> Result<()> {
    if (snapshot && id != 0) || !super::valid_frame_prefix(id, count, payload_len) {
        let reason = if snapshot {
            "invalid graph snapshot"
        } else {
            "invalid graph frame count, reserved ID or body length"
        };
        return Err(corrupt(offset, reason));
    }
    Ok(())
}

/// Decodes the checksum-verified body of an op-12 frame, or of an op-15
/// snapshot frame when `snapshot`: its prefix passes [`check_prefix`] and
/// the body decodes with exactly `id` reclaims. A snapshot body holds only
/// the entries a checkpoint restates ([`restated`]); their order and
/// coverage are checked where the snapshot loads. Full replay, the change
/// feed and the precision image check decode graph frames only through
/// this, so they accept the same frames.
pub(crate) fn decode_frame(
    snapshot: bool,
    id: u64,
    count: u32,
    body: &[u8],
    offset: u64,
) -> Result<GraphBody> {
    check_prefix(snapshot, id, count as usize, body.len(), offset)?;
    let reason = if snapshot {
        "invalid graph snapshot"
    } else {
        "invalid graph frame body"
    };
    engine::decode_body(body, count)
        .ok()
        .filter(|body| reclaim_count(body) == id)
        .filter(|body| !snapshot || body.entries.iter().all(restated))
        .ok_or_else(|| corrupt(offset, reason))
}

/// Whether `entry` is one a checkpoint restates in a snapshot body
/// ([`GraphState::from_snapshot`] loads no others): graph creates and
/// schemas, nodes and edges with their deadlines, attachments and
/// high-waters. Never a vector mutation, removal, patch or cleared deadline.
fn restated(entry: &GraphEntry) -> bool {
    matches!(
        entry,
        GraphEntry::CreateGraph { .. }
            | GraphEntry::PutSchema { .. }
            | GraphEntry::PutNode { .. }
            | GraphEntry::PutEdge { .. }
            | GraphEntry::NodeExpiry {
                expires_at_ms: Some(_),
                ..
            }
            | GraphEntry::EdgeExpiry {
                expires_at_ms: Some(_),
                ..
            }
            | GraphEntry::Attach { .. }
            | GraphEntry::GraphHighWater { .. }
            // Gap 4: a temporal edge with its whole series.
            | GraphEntry::Temporal(engine::codec::TemporalEntry::PutEdge { .. })
    )
}

/// The records that graph frames read and write (P3a): full replay's
/// loaded collections, or the live records that the readers which do not
/// replay a precision log follow (`compact::ImageCheck`). [`replay`] and
/// [`PendingSnapshot::finish`] check and apply graph frames and snapshots
/// to either alike, so those readers accept exactly what replay accepts.
pub(crate) trait GraphRecords {
    /// The dimensions of live collection `collection`.
    fn dimensions(&self, collection: u16) -> Option<usize>;
    /// Refuses a dense `vector` that live collection `collection` does not
    /// store exactly (`format::check_dense`).
    fn check_dense(&self, collection: u16, vector: &[f32], offset: u64) -> Result<()>;
    /// Applies the put (without expiry) or delete of a graph vector entry
    /// committed at `sequence` to live collection `collection`.
    fn apply(&mut self, collection: u16, mutation: Mutation, sequence: u64);
    /// Whether `record` of `collection` is live without expiry.
    fn lasting(&self, collection: u16, record: u64) -> bool;
}

impl GraphRecords for BTreeMap<u16, LoadedCollection> {
    fn dimensions(&self, collection: u16) -> Option<usize> {
        self.get(&collection).map(|target| target.dimensions)
    }

    fn check_dense(&self, collection: u16, vector: &[f32], offset: u64) -> Result<()> {
        self[&collection].check_dense(vector, offset)
    }

    fn apply(&mut self, collection: u16, mutation: Mutation, sequence: u64) {
        let target = self
            .get_mut(&collection)
            .expect("collection checked while decoding");
        match mutation {
            Mutation::Put {
                id,
                vector,
                payload,
            } => target.put(
                id,
                Record {
                    vector,
                    payload,
                    metadata: crate::Metadata::new(),
                },
                sequence,
                None,
            ),
            Mutation::PutWithMetadata {
                id,
                vector,
                payload,
                metadata,
            } => target.put(
                id,
                Record {
                    vector,
                    payload,
                    metadata,
                },
                sequence,
                None,
            ),
            Mutation::Delete { id } => target.delete(id),
        }
    }

    fn lasting(&self, collection: u16, record: u64) -> bool {
        self.get(&collection).is_some_and(|target| {
            target.records.contains_key(&record) && !target.expiries.contains_key(&record)
        })
    }
}

/// Replays one checksum-verified op-12 body committed at `sequence` (frame at
/// `offset`). Every entry is decoded and validated first; then graph entries
/// are applied atomically to `state` and vector entries, in body order, to
/// `records`. Afterwards every attachment the body touched (attached by
/// an `Attach` entry or owning a record changed by a vector entry) must
/// reference a live record without expiry. Any failure fails the open.
/// `restated` bodies come from a checkpoint image and may repeat the last
/// applied sequence. The body is decoded by [`decode_frame`], so the prefix
/// `id` must count its reclaims. Full replay and the image check of readers
/// that do not replay a precision log (P3a) apply graph frames only through
/// this.
#[allow(clippy::too_many_arguments)]
pub(crate) fn replay(
    state: &mut GraphState,
    records: &mut impl GraphRecords,
    sequence: u64,
    id: u64,
    count: u32,
    body: &[u8],
    offset: u64,
    restated: bool,
) -> Result<()> {
    let body = decode_frame(false, id, count, body, offset)?;
    let mut mutations = Vec::new();
    let mut touched = Vec::new();
    for entry in &body.entries {
        match entry {
            GraphEntry::VectorMutation { collection, entry } => {
                let dimensions = records.dimensions(*collection).ok_or_else(|| {
                    corrupt(offset, "graph vector entry references unknown collection")
                })?;
                let (mut decoded, marker) =
                    format::decode_batch(entry, 1, dimensions, offset, format::V5)?;
                // The engine envelope admits only ops 1, 2 and 4.
                debug_assert!(marker.is_none() && decoded.len() == 1);
                let mutation = decoded
                    .pop()
                    .ok_or_else(|| corrupt(offset, "invalid graph vector entry"))?;
                if let Mutation::Put { vector, .. } | Mutation::PutWithMetadata { vector, .. } =
                    &mutation
                {
                    records.check_dense(*collection, vector, offset)?;
                }
                touched.push((*collection, mutation_id(&mutation)));
                mutations.push((*collection, mutation));
            }
            GraphEntry::Attach {
                collection,
                record_id,
                ..
            } => touched.push((*collection, *record_id)),
            _ => {}
        }
    }
    state
        .apply_at(sequence, &body, restated)
        .map_err(|_| corrupt(offset, "graph frame violates graph state"))?;
    for (collection, mutation) in mutations {
        records.apply(collection, mutation, sequence);
    }
    for (collection, record) in touched {
        if state.attachment_owner(collection, record).is_some()
            && !records.lasting(collection, record)
        {
            return Err(corrupt(
                offset,
                "graph attachment references a missing or expiring record",
            ));
        }
    }
    Ok(())
}

/// The graph snapshot of a checkpoint image being read (Speed P2b, P3a):
/// its op-15 bodies, all restated at one sequence, from the frame at
/// `first` on. Each verified body loads as it is read
/// ([`SnapshotBuilder`]), so a reader holds the state built so far and
/// one decoded body, never every decoded body beside that state. The
/// bodies are exactly what [`compacted_bodies`] writes; loading them
/// equals replaying them as op-12 frames. Full replay and the image check
/// of readers that do not replay a precision log load snapshots only
/// through this, so they accept the same ones.
pub(crate) struct PendingSnapshot {
    first: u64,
    /// `None` once a body was refused: the snapshot is refused where it
    /// ends, as loading every body there refuses it, and later bodies are
    /// only dropped.
    builder: Option<SnapshotBuilder>,
}

impl PendingSnapshot {
    /// A snapshot restated at `sequence` whose first frame is at `first`.
    pub(crate) fn new(sequence: u64, first: u64) -> Self {
        Self {
            first,
            builder: Some(SnapshotBuilder::new(sequence)),
        }
    }

    /// The offset of the snapshot's first frame.
    pub(crate) fn first(&self) -> u64 {
        self.first
    }

    /// Loads the next verified body ([`decode_frame`]).
    pub(crate) fn push(&mut self, body: GraphBody) {
        if let Some(builder) = self.builder.as_mut()
            && builder.push(body).is_err()
        {
            self.builder = None;
        }
    }

    /// The loaded state, where the snapshot ends: canonical, and every
    /// attachment references a record that `records` holds live without
    /// expiry.
    pub(crate) fn finish(self, records: &impl GraphRecords) -> Result<GraphState> {
        let state = self
            .builder
            .and_then(|builder| builder.finish().ok())
            .ok_or_else(|| corrupt(self.first, "invalid graph snapshot"))?;
        for (collection, record) in state.attached_records() {
            if !records.lasting(collection, record) {
                return Err(corrupt(
                    self.first,
                    "graph attachment references a missing or expiring record",
                ));
            }
        }
        Ok(state)
    }
}

/// Writes `state` as the op-12 bodies of a compacted file (REQ-052), or the
/// op-15 snapshot bodies of a checkpoint image: per
/// graph in ID order (IDs kept), `CreateGraph`, the declared schema if any,
/// every node, every edge and
/// every attachment, then a `GraphHighWater` entry with the graph's next node
/// and edge IDs so deleted IDs are never reused. Bodies stay within the batch
/// entry and byte limits; `emit` receives each body's graph, entry count and
/// bytes.
/// Attachments are checked against live records, so these frames must
/// follow the record frames. Nodes due at `now_ms` are dropped with
/// their edges and attachments, and so are edges due at `now_ms`; a kept
/// deadline follows its node or edge as an expiry entry. Without `now_ms`
/// (a P3a preserving relocation) every node and edge is kept. Gap 4: a
/// snapshot at `floor` restates each temporal edge's revision; a compacted
/// file (`floor` zero) does not.
pub(crate) fn compacted_bodies(
    state: &GraphState,
    now_ms: Option<u64>,
    floor: u64,
    mut emit: impl FnMut(GraphId, u32, &[u8]) -> Result<()>,
) -> Result<()> {
    let encode = |body: &GraphBody| {
        engine::encode_body(body).map_err(|_| corrupt(0, "graph state cannot be re-encoded"))
    };
    for graph in state.graphs() {
        let graph_id = graph.id();
        let create = GraphEntry::CreateGraph {
            name: graph.name().into(),
        };
        let live_nodes = || kept_nodes(graph, now_ms);
        let nodes = live_nodes().flat_map(|node| {
            let expiry = graph
                .node_expiry(node.id)
                .map(|deadline| GraphEntry::NodeExpiry {
                    node_id: node.id,
                    expires_at_ms: Some(deadline),
                });
            std::iter::once(GraphEntry::PutNode {
                node_id: node.id,
                key: node.key.clone(),
                labels: node.labels.clone(),
                properties: node.properties.clone(),
            })
            .chain(expiry)
        });
        let edges = kept_edges(graph, now_ms).flat_map(|edge| {
            let expiry = graph
                .edge_expiry(edge.id)
                .map(|deadline| GraphEntry::EdgeExpiry {
                    edge_id: edge.id,
                    expires_at_ms: Some(deadline),
                });
            // Gap 4: a temporal edge is restated with its whole series.
            std::iter::once(graph.edge_snapshot_entry(edge, floor)).chain(expiry)
        });
        let attachments = live_nodes().flat_map(|node| {
            graph
                .attachments(node.id)
                .map(move |(collection, record_id)| GraphEntry::Attach {
                    node_id: node.id,
                    collection,
                    record_id,
                })
        });
        let high_water = GraphEntry::GraphHighWater {
            next_node_id: graph.next_node_id(),
            next_edge_id: graph.next_edge_id(),
        };
        let schema = (!graph.schema().is_empty()).then(|| GraphEntry::PutSchema {
            schema: graph.schema().clone(),
        });
        let mut body = GraphBody {
            graph_id,
            entries: Vec::new(),
        };
        let mut used = BODY_HEADER_BYTES;
        let entries = std::iter::once(create)
            .chain(schema)
            .chain(nodes)
            .chain(edges)
            .chain(attachments)
            .chain(std::iter::once(high_water));
        for entry in entries {
            let single = GraphBody {
                graph_id,
                entries: vec![entry],
            };
            let len = encode(&single)?.len() - BODY_HEADER_BYTES;
            if body.entries.len() == MAX_BATCH_ENTRIES || used + len > MAX_BATCH_BYTES {
                emit(graph_id, body.entries.len() as u32, &encode(&body)?)?;
                body.entries.clear();
                used = BODY_HEADER_BYTES;
            }
            used += len;
            body.entries.extend(single.entries);
        }
        emit(graph_id, body.entries.len() as u32, &encode(&body)?)?;
    }
    Ok(())
}

/// The nodes of `graph` that a rewrite at `now_ms` (all without it) keeps,
/// in ID order.
fn kept_nodes(graph: &Graph, now_ms: Option<u64>) -> impl Iterator<Item = &GraphNode> {
    graph
        .nodes()
        .filter(move |node| !now_ms.is_some_and(|now| graph.node_expired(node.id, now)))
}

/// The edges of `graph` that a rewrite at `now_ms` (all without it) keeps,
/// in ID order.
fn kept_edges(graph: &Graph, now_ms: Option<u64>) -> impl Iterator<Item = &GraphEdge> {
    graph
        .edges()
        .filter(move |edge| !now_ms.is_some_and(|now| graph.edge_expired(edge, now)))
}

/// The (ID, revision) pairs of the nodes and of the edges of `graph` that
/// `compacted_bodies` writes at `now_ms`, which a P3a precision image keeps
/// in its revision blocks.
pub(crate) fn kept_revisions(graph: &Graph, now_ms: Option<u64>) -> (Pairs, Pairs) {
    (
        kept_nodes(graph, now_ms)
            .map(|node| (node.id.0, node.revision))
            .collect(),
        kept_edges(graph, now_ms)
            .map(|edge| (edge.id.0, edge.revision))
            .collect(),
    )
}

pub(crate) fn mutation_id(mutation: &Mutation) -> u64 {
    match mutation {
        Mutation::Put { id, .. }
        | Mutation::PutWithMetadata { id, .. }
        | Mutation::Delete { id } => *id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MetadataValue;

    #[test]
    fn vector_entries_reuse_canonical_batch_entries_and_round_trip() {
        let mutations = [
            Mutation::Put {
                id: 7,
                vector: vec![1.0, -2.5],
                payload: b"payload".to_vec(),
            },
            Mutation::PutWithMetadata {
                id: u64::MAX,
                vector: vec![0.0, 3.0],
                payload: Vec::new(),
                metadata: crate::Metadata::from([(
                    "geo".to_string(),
                    MetadataValue::GeoPoint {
                        latitude: 1.0,
                        longitude: 2.0,
                    },
                )]),
            },
            Mutation::Delete { id: 0 },
        ];
        for mutation in mutations {
            let GraphEntry::VectorMutation { collection, entry } = vector_entry(9, &mutation)
            else {
                panic!("vector entry expected");
            };
            assert_eq!(collection, 9);
            let mut canonical = Vec::new();
            format::encode_batch_entries(std::slice::from_ref(&mutation), &mut canonical);
            assert_eq!(entry, canonical);
            let (decoded, marker) = format::decode_batch(&entry, 1, 2, 0, format::V5).unwrap();
            assert!(marker.is_none());
            assert_eq!(decoded, std::slice::from_ref(&mutation));
            // The engine codec accepts and preserves it inside a graph body.
            let body = engine::GraphBody {
                graph_id: engine::GraphId(0),
                entries: vec![GraphEntry::VectorMutation { collection, entry }],
            };
            let encoded = engine::encode_body(&body).unwrap();
            assert_eq!(engine::decode_body(&encoded, 1).unwrap(), body);
        }
    }
}
