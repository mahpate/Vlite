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

use crate::format::{self, LoadedCollection};
use crate::{Error, Mutation, Record, Result};

use super::engine::codec::BODY_HEADER_BYTES;
use super::engine::limits::{MAX_BATCH_BYTES, MAX_BATCH_ENTRIES};
use super::engine::{self, GraphBody, GraphEntry, GraphState};

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

/// Replays one checksum-verified op-12 body committed at `sequence` (frame at
/// `offset`). Every entry is decoded and validated first; then graph entries
/// are applied atomically to `state` and vector entries, in body order, to
/// `collections`. Afterwards every attachment the body touched (attached by
/// an `Attach` entry or owning a record changed by a vector entry) must
/// reference a live record without expiry. Any failure fails the open.
/// `restated` bodies come from a checkpoint image and may repeat the last
/// applied sequence. The prefix `id` must count the body's reclaims.
#[allow(clippy::too_many_arguments)]
pub(crate) fn replay(
    state: &mut GraphState,
    collections: &mut BTreeMap<u16, LoadedCollection>,
    sequence: u64,
    id: u64,
    count: u32,
    body: &[u8],
    offset: u64,
    restated: bool,
) -> Result<()> {
    let body = engine::decode_body(body, count)
        .ok()
        .filter(|body| reclaim_count(body) == id)
        .ok_or_else(|| corrupt(offset, "invalid graph frame body"))?;
    let mut mutations = Vec::new();
    let mut touched = Vec::new();
    for entry in &body.entries {
        match entry {
            GraphEntry::VectorMutation { collection, entry } => {
                let dimensions = collections
                    .get(collection)
                    .ok_or_else(|| {
                        corrupt(offset, "graph vector entry references unknown collection")
                    })?
                    .dimensions;
                let (mut decoded, marker) =
                    format::decode_batch(entry, 1, dimensions, offset, format::V5)?;
                // The engine envelope admits only ops 1, 2 and 4.
                debug_assert!(marker.is_none() && decoded.len() == 1);
                let mutation = decoded
                    .pop()
                    .ok_or_else(|| corrupt(offset, "invalid graph vector entry"))?;
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
        let target = collections
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
    for (collection, record) in touched {
        if state.attachment_owner(collection, record).is_some()
            && !collections.get(&collection).is_some_and(|target| {
                target.records.contains_key(&record) && !target.expiries.contains_key(&record)
            })
        {
            return Err(corrupt(
                offset,
                "graph attachment references a missing or expiring record",
            ));
        }
    }
    Ok(())
}

/// Builds the graph state of a checkpoint image from its op-15 snapshot
/// bodies (Speed P2b), all restated at `sequence`; `offset` is the first
/// snapshot frame. The bodies are exactly what [`compacted_bodies`] writes;
/// loading them equals replaying them as op-12 frames, including the check
/// that every attachment references a live record without expiry.
pub(crate) fn load_snapshot(
    collections: &BTreeMap<u16, LoadedCollection>,
    sequence: u64,
    bodies: Vec<GraphBody>,
    offset: u64,
) -> Result<GraphState> {
    let state = GraphState::from_snapshot(sequence, bodies)
        .map_err(|_| corrupt(offset, "invalid graph snapshot"))?;
    for (collection, record) in state.attached_records() {
        if !collections.get(&collection).is_some_and(|target| {
            target.records.contains_key(&record) && !target.expiries.contains_key(&record)
        }) {
            return Err(corrupt(
                offset,
                "graph attachment references a missing or expiring record",
            ));
        }
    }
    Ok(state)
}

/// Writes `state` as the op-12 bodies of a compacted file (REQ-052), or the
/// op-15 snapshot bodies of a checkpoint image: per
/// graph in ID order (IDs kept), `CreateGraph`, the declared schema if any,
/// every node, every edge and
/// every attachment, then a `GraphHighWater` entry with the graph's next node
/// and edge IDs so deleted IDs are never reused. Bodies stay within the batch
/// entry and byte limits; `emit` receives each body's entry count and bytes.
/// Attachments are checked against live records, so these frames must
/// follow the record frames. Nodes due at `now_ms` are dropped with
/// their edges and attachments, and so are edges due at `now_ms`; a kept
/// deadline follows its node or edge as an expiry entry.
pub(crate) fn compacted_bodies(
    state: &GraphState,
    now_ms: u64,
    mut emit: impl FnMut(u32, &[u8]) -> Result<()>,
) -> Result<()> {
    let encode = |body: &GraphBody| {
        engine::encode_body(body).map_err(|_| corrupt(0, "graph state cannot be re-encoded"))
    };
    for graph in state.graphs() {
        let graph_id = graph.id();
        let create = GraphEntry::CreateGraph {
            name: graph.name().into(),
        };
        let live_nodes = || {
            graph
                .nodes()
                .filter(move |node| !graph.node_expired(node.id, now_ms))
        };
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
        let edges = graph
            .edges()
            .filter(|edge| !graph.edge_expired(edge, now_ms))
            .flat_map(|edge| {
                let expiry = graph
                    .edge_expiry(edge.id)
                    .map(|deadline| GraphEntry::EdgeExpiry {
                        edge_id: edge.id,
                        expires_at_ms: Some(deadline),
                    });
                std::iter::once(GraphEntry::PutEdge {
                    edge_id: edge.id,
                    source: edge.source,
                    destination: edge.destination,
                    relationship: edge.relationship.clone(),
                    key: edge.key.clone(),
                    properties: edge.properties.clone(),
                })
                .chain(expiry)
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
                emit(body.entries.len() as u32, &encode(&body)?)?;
                body.entries.clear();
                used = BODY_HEADER_BYTES;
            }
            used += len;
            body.entries.extend(single.entries);
        }
        emit(body.entries.len() as u32, &encode(&body)?)?;
    }
    Ok(())
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
