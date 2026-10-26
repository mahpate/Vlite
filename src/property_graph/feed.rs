//! Change-feed decoding of op-12 graph frames (v0.6.0 graph/vector parity).
//!
//! The feed scans the log from its beginning, so this tracker sees every
//! graph frame in order and can report node and edge keys, attachment
//! records and graph names next to the IDs the frames store. One frame is one
//! commit: `CreateGraph` and `DropGraph` become their own events, each
//! maximal run of other graph entries becomes one `GraphBatch`, and vector
//! entries become ordinary `Put`/`Delete` events at their position, so the
//! events of a commit keep log order. `GraphHighWater` entries carry no
//! visible change and are skipped.

use std::collections::{BTreeMap, BTreeSet};

use crate::changes::{ChangeKind, GraphChange};
use crate::{Error, Metadata, Mutation, Record, Result, format};

use super::convert::metadata_from_properties;
use super::engine::{self, GraphEntry};

#[derive(Default)]
struct Tracked {
    name: String,
    nodes: BTreeMap<u64, String>,
    edges: BTreeMap<u64, Option<String>>,
    /// `(node, collection)` → attached record.
    attachments: BTreeMap<(u64, u16), u64>,
}

/// Graph state the feed needs to name what each frame changed.
#[derive(Default)]
pub(crate) struct FeedGraphs {
    graphs: BTreeMap<u16, Tracked>,
}

fn labels(set: &BTreeSet<String>) -> Vec<String> {
    set.iter().cloned().collect()
}

impl FeedGraphs {
    /// Decodes one checksum-verified op-12 body into `(collection, kind)`
    /// events. `collections` maps collection IDs to names and dimensions.
    pub(crate) fn frame(
        &mut self,
        sequence: u64,
        count: u32,
        body: &[u8],
        offset: u64,
        collections: &BTreeMap<u16, (String, usize)>,
    ) -> Result<Vec<(String, ChangeKind)>> {
        let corrupt = |reason| Error::Corrupt { offset, reason };
        let body =
            engine::decode_body(body, count).map_err(|_| corrupt("invalid graph frame body"))?;
        let graph_id = body.graph_id.0;
        let collection_name = |collection: u16| {
            collections
                .get(&collection)
                .map(|entry| entry.0.clone())
                .unwrap_or_default()
        };
        let mut events = Vec::new();
        let mut run: Vec<GraphChange> = Vec::new();
        let flush = |run: &mut Vec<GraphChange>,
                     events: &mut Vec<(String, ChangeKind)>,
                     graphs: &BTreeMap<u16, Tracked>| {
            if !run.is_empty() {
                let graph = graphs
                    .get(&graph_id)
                    .map(|tracked| tracked.name.clone())
                    .unwrap_or_default();
                events.push((
                    String::new(),
                    ChangeKind::GraphBatch {
                        graph,
                        revision: sequence,
                        operations: std::mem::take(run),
                    },
                ));
            }
        };
        for entry in body.entries {
            if let GraphEntry::VectorMutation { collection, entry } = &entry {
                flush(&mut run, &mut events, &self.graphs);
                let dimensions = collections
                    .get(collection)
                    .ok_or_else(|| corrupt("graph vector entry references unknown collection"))?
                    .1;
                let (decoded, _) = format::decode_batch(entry, 1, dimensions, offset, format::V5)?;
                for mutation in decoded {
                    let kind = match mutation {
                        Mutation::Put {
                            id,
                            vector,
                            payload,
                        } => ChangeKind::Put {
                            id,
                            record: Record {
                                vector,
                                payload,
                                metadata: Metadata::new(),
                            },
                            expires_at_ms: None,
                        },
                        Mutation::PutWithMetadata {
                            id,
                            vector,
                            payload,
                            metadata,
                        } => ChangeKind::Put {
                            id,
                            record: Record {
                                vector,
                                payload,
                                metadata,
                            },
                            expires_at_ms: None,
                        },
                        Mutation::Delete { id } => ChangeKind::Delete { id },
                    };
                    events.push((collection_name(*collection), kind));
                }
                continue;
            }
            if let GraphEntry::CreateGraph { name } = &entry {
                flush(&mut run, &mut events, &self.graphs);
                self.graphs.insert(
                    graph_id,
                    Tracked {
                        name: name.clone(),
                        ..Tracked::default()
                    },
                );
                events.push((
                    String::new(),
                    ChangeKind::GraphCreated {
                        graph: name.clone(),
                    },
                ));
                continue;
            }
            if let GraphEntry::DropGraph = &entry {
                flush(&mut run, &mut events, &self.graphs);
                let graph = self
                    .graphs
                    .remove(&graph_id)
                    .map(|tracked| tracked.name)
                    .unwrap_or_default();
                events.push((String::new(), ChangeKind::GraphDropped { graph }));
                continue;
            }
            let tracked = self.graphs.entry(graph_id).or_default();
            let node_key =
                |tracked: &Tracked, id: u64| tracked.nodes.get(&id).cloned().unwrap_or_default();
            let change = match entry {
                GraphEntry::PutNode {
                    node_id,
                    key,
                    labels: set,
                    properties,
                } => {
                    let created = tracked.nodes.insert(node_id.0, key.clone()).is_none();
                    GraphChange::NodeUpserted {
                        id: node_id.0,
                        key,
                        labels: labels(&set),
                        properties: metadata_from_properties(&properties),
                        created,
                    }
                }
                GraphEntry::PatchNode {
                    node_id,
                    add_labels,
                    remove_labels,
                    set,
                    unset,
                } => GraphChange::NodePatched {
                    id: node_id.0,
                    key: node_key(tracked, node_id.0),
                    add_labels: labels(&add_labels),
                    remove_labels: labels(&remove_labels),
                    set: metadata_from_properties(&set),
                    unset: labels(&unset),
                },
                GraphEntry::DeleteNode { node_id } => {
                    let key = tracked.nodes.remove(&node_id.0).unwrap_or_default();
                    tracked
                        .attachments
                        .retain(|(node, _), _| *node != node_id.0);
                    GraphChange::NodeDeleted { id: node_id.0, key }
                }
                GraphEntry::PutEdge {
                    edge_id,
                    source,
                    destination,
                    relationship,
                    key,
                    properties,
                } => {
                    let created = tracked.edges.insert(edge_id.0, key.clone()).is_none();
                    GraphChange::EdgeUpserted {
                        id: edge_id.0,
                        key,
                        source: source.0,
                        source_key: node_key(tracked, source.0),
                        relationship,
                        destination: destination.0,
                        destination_key: node_key(tracked, destination.0),
                        properties: metadata_from_properties(&properties),
                        created,
                    }
                }
                GraphEntry::PatchEdge {
                    edge_id,
                    set,
                    unset,
                } => GraphChange::EdgePatched {
                    id: edge_id.0,
                    key: tracked.edges.get(&edge_id.0).cloned().flatten(),
                    set: metadata_from_properties(&set),
                    unset: labels(&unset),
                },
                GraphEntry::DeleteEdge { edge_id } => GraphChange::EdgeDeleted {
                    id: edge_id.0,
                    key: tracked.edges.remove(&edge_id.0).flatten(),
                },
                GraphEntry::Attach {
                    node_id,
                    collection,
                    record_id,
                } => {
                    tracked
                        .attachments
                        .insert((node_id.0, collection), record_id);
                    GraphChange::Attached {
                        node: node_id.0,
                        node_key: node_key(tracked, node_id.0),
                        collection: collection_name(collection),
                        record: record_id,
                    }
                }
                GraphEntry::Detach {
                    node_id,
                    collection,
                } => GraphChange::Detached {
                    node: node_id.0,
                    node_key: node_key(tracked, node_id.0),
                    collection: collection_name(collection),
                    record: tracked
                        .attachments
                        .remove(&(node_id.0, collection))
                        .unwrap_or_default(),
                },
                GraphEntry::PutSchema { schema } => GraphChange::SchemaUpdated { schema },
                GraphEntry::GraphHighWater { .. }
                | GraphEntry::CreateGraph { .. }
                | GraphEntry::DropGraph
                | GraphEntry::VectorMutation { .. } => continue,
            };
            run.push(change);
        }
        flush(&mut run, &mut events, &self.graphs);
        Ok(events)
    }
}
