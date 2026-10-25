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

use super::engine::{self, GraphEntry, GraphState};

/// Encodes one vector mutation of `collection` as a graph-frame entry.
pub(crate) fn vector_entry(collection: u16, mutation: &Mutation) -> GraphEntry {
    let mut entry = Vec::new();
    format::encode_batch_entries(std::slice::from_ref(mutation), &mut entry);
    GraphEntry::VectorMutation { collection, entry }
}

fn corrupt(offset: u64, reason: &'static str) -> Error {
    Error::Corrupt { offset, reason }
}

/// Replays one checksum-verified op-12 body committed at `sequence` (frame at
/// `offset`). Every entry is decoded and validated first; then graph entries
/// are applied atomically to `state` and vector entries, in body order, to
/// `collections`. Afterwards every attachment the body touched (attached by
/// an `Attach` entry or owning a record changed by a vector entry) must
/// reference a live record without expiry. Any failure fails the open.
pub(crate) fn replay(
    state: &mut GraphState,
    collections: &mut BTreeMap<u16, LoadedCollection>,
    sequence: u64,
    count: u32,
    body: &[u8],
    offset: u64,
) -> Result<()> {
    let body = engine::decode_body(body, count)
        .map_err(|_| corrupt(offset, "invalid graph frame body"))?;
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
        .apply(sequence, &body)
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
