//! Gap 3 (unreleased): the TTL projection of graph frames for the
//! [`GraphEventSet::Ttl`] feed.
//!
//! A tracker separate from the legacy [`super::feed::FeedGraphs`], whose
//! output must not change. It replays every frame from the log start and
//! keeps, per graph, the node and edge keys and own deadlines, edge
//! endpoints, attachments and the ID high-water: enough to report logged
//! deadline changes as otherwise-empty patches and each logged reclaim as
//! its edge deletes, detaches and node deletes, with [`GraphTtl`]
//! annotations. It never reads a clock: removals come only from logged
//! reclaims. A logged reference the tracker cannot resolve is
//! `Error::Corrupt`, never a default key.

use std::collections::{BTreeMap, BTreeSet};

use crate::changes::{
    ChangeKind, GraphChange, GraphEventSet, GraphFeedBatch, GraphFeedCursor, GraphFeedError,
};
use crate::{Database, Error, Metadata, Mutation, Record, Result, format};

use super::change::{GraphRemovalReason, GraphTtl, TemporalChange};
use super::convert::metadata_from_properties;
use super::engine::codec::TemporalEntry;
use super::engine::{GraphEntry, SnapshotBuilder};

#[cfg(test)]
mod tests;

struct Node {
    key: String,
    expires_at_ms: Option<u64>,
}

struct Edge {
    key: Option<String>,
    source: u64,
    destination: u64,
    expires_at_ms: Option<u64>,
}

struct Graph {
    name: String,
    nodes: BTreeMap<u64, Node>,
    node_keys: BTreeSet<String>,
    edges: BTreeMap<u64, Edge>,
    edge_keys: BTreeSet<String>,
    /// `(node, collection)` → attached record.
    attachments: BTreeMap<(u64, u16), u64>,
    next_node_id: u64,
    next_edge_id: u64,
}

impl Graph {
    fn new(name: String) -> Self {
        Self {
            name,
            nodes: BTreeMap::new(),
            node_keys: BTreeSet::new(),
            edges: BTreeMap::new(),
            edge_keys: BTreeSet::new(),
            attachments: BTreeMap::new(),
            next_node_id: 0,
            next_edge_id: 0,
        }
    }
}

/// One event of a frame's TTL projection, before paging numbers it.
#[derive(Debug, PartialEq)]
pub(crate) struct TtlEvent {
    pub(crate) collection: String,
    pub(crate) kind: ChangeKind,
    /// One annotation per operation of a `GraphBatch`; empty otherwise.
    pub(crate) ttl: Vec<Option<GraphTtl>>,
    /// The operations of one logged reclaim, which paging may split.
    pub(crate) reclaim: bool,
}

impl Database {
    /// Gap 3 (unreleased): a page of committed changes after `cursor`, with
    /// exactly the optional graph events `graph_events` selects.
    ///
    /// [`GraphEventSet::Empty`] is the legacy feed of
    /// [`changes_after`](Database::changes_after), each change wrapped with
    /// an empty `ttl`. [`GraphEventSet::Ttl`] also reports graph TTL: each
    /// logged deadline change as an otherwise-empty node or edge patch, and
    /// each logged expiry reclaim as its edge deletes, detaches and node
    /// deletes, with one [`GraphTtl`] annotation
    /// per graph operation. Removals come only from reclaims in the log,
    /// never from the reader's clock. A large reclaim is delivered in
    /// chunks, so `next` may stand inside a commit; pass it back unchanged.
    /// A page holds whole units and at least one; `max_events` is clamped
    /// to `1..=10000`.
    ///
    /// Errors: a cursor bound to another set is
    /// [`GraphFeedError::CursorModeMismatch`] (convert a boundary with
    /// [`GraphFeedCursor::at_commit`]); a split cursor at a checkpoint
    /// floor is [`GraphFeedError::CursorBeforeHistory`]; a reclaim
    /// operation whose event exceeds 16 MiB is
    /// [`GraphFeedError::ResponseTooLarge`]; everything the legacy feed
    /// refuses (lineage, ahead, before history, corruption, temporal
    /// entries) is [`GraphFeedError::Engine`], and an ordinal naming no
    /// event is `Engine(Error::InvalidCursor)`.
    pub fn graph_changes_after(
        &self,
        cursor: &GraphFeedCursor,
        graph_events: GraphEventSet,
        max_events: usize,
    ) -> std::result::Result<GraphFeedBatch, GraphFeedError> {
        self.lineage.ok_or(Error::ChangeFeedRequiresUpgrade)?;
        // As `changes_after`: never publish commits a crash could lose.
        self.sync()?;
        let _scan = self.lock_log_scan()?;
        let mut file = &self.file;
        crate::changes::read_graph_changes(
            &mut file,
            None,
            self.cipher.as_ref(),
            cursor,
            graph_events,
            max_events,
        )
        .map(|(batch, _)| batch)
    }
}

/// Where the log is relative to its op-15 snapshot, which a checkpoint
/// image restates before any op-12 commit, at one sequence.
enum Snapshot {
    /// No op-12 frame and no snapshot yet.
    Before,
    /// Snapshot frames at `sequence` from the frame at `first` on, loaded
    /// through the engine's snapshot path to check their order.
    Open {
        builder: Box<SnapshotBuilder>,
        sequence: u64,
        first: u64,
    },
    /// A snapshot ended or an op-12 frame was read: no snapshot may follow.
    Done,
}

/// Graph state the TTL projection needs. `owners` maps each attached
/// `(collection, record)` to its `(graph, node)` across graphs.
pub(crate) struct TtlGraphs {
    graphs: BTreeMap<u16, Graph>,
    owners: BTreeMap<(u16, u64), (u16, u64)>,
    flags: u16,
    snapshot: Snapshot,
}

/// Static reasons of the tracker's `Error::Corrupt`.
pub(crate) mod reason {
    pub(crate) const GRAPH: &str = "graph entry references an unknown graph";
    pub(crate) const GRAPH_EXISTS: &str = "graph created twice or with a used name";
    pub(crate) const GRAPH_NOT_EMPTY: &str = "dropped graph is not empty";
    pub(crate) const HIGH_WATER: &str = "graph high-water decreases";
    pub(crate) const NODE: &str = "graph entry references an unknown node";
    pub(crate) const NODE_ID: &str = "graph node ID reused or key changed";
    pub(crate) const NODE_KEY: &str = "duplicate graph node key";
    pub(crate) const NODE_EDGES: &str = "deleted graph node has edges";
    pub(crate) const EDGE: &str = "graph entry references an unknown edge";
    pub(crate) const EDGE_ID: &str = "graph edge ID reused";
    pub(crate) const EDGE_KEY: &str = "duplicate graph edge key";
    pub(crate) const COLLECTION: &str = "graph entry references an unknown collection";
    pub(crate) const ATTACHED: &str = "graph node or record attached twice";
    pub(crate) const DETACHED: &str = "graph detach without an attachment";
    pub(crate) const ID_SPACE: &str = "graph ID space exhausted";
    pub(crate) const SNAPSHOT: &str = "invalid graph snapshot";
    pub(crate) const SNAPSHOT_ORDER: &str = "graph snapshot out of order";
}

fn labels(set: &BTreeSet<String>) -> Vec<String> {
    set.iter().cloned().collect()
}

/// The next ID after allocating `id`, which must not be below `next`.
fn allocate(next: u64, id: u64, reused: &'static str, offset: u64) -> Result<u64> {
    let corrupt = |reason| Error::Corrupt { offset, reason };
    if id < next {
        return Err(corrupt(reused));
    }
    id.checked_add(1).ok_or_else(|| corrupt(reason::ID_SPACE))
}

/// The ordinary graph operations collected since the last boundary.
#[derive(Default)]
struct Run {
    operations: Vec<GraphChange>,
    ttl: Vec<Option<GraphTtl>>,
}

impl Run {
    fn push(&mut self, operation: GraphChange, ttl: Option<GraphTtl>) {
        self.operations.push(operation);
        self.ttl.push(ttl);
    }

    /// Ends the run as one `GraphBatch` of `graph`, if it has operations.
    fn flush(&mut self, graph: &str, sequence: u64, reclaim: bool, events: &mut Vec<TtlEvent>) {
        if self.operations.is_empty() {
            return;
        }
        events.push(TtlEvent {
            collection: String::new(),
            kind: ChangeKind::GraphBatch {
                graph: graph.to_owned(),
                revision: sequence,
                operations: std::mem::take(&mut self.operations),
            },
            ttl: std::mem::take(&mut self.ttl),
            reclaim,
        });
    }
}

impl TtlGraphs {
    /// A tracker for a log with header `flags`.
    pub(crate) fn new(flags: u16) -> Self {
        Self {
            graphs: BTreeMap::new(),
            owners: BTreeMap::new(),
            flags,
            snapshot: Snapshot::Before,
        }
    }

    /// Ends an open snapshot where the frames restating it end: it must be
    /// complete and canonical, as full replay loads it.
    pub(crate) fn end_snapshot(&mut self) -> Result<()> {
        if !matches!(self.snapshot, Snapshot::Open { .. }) {
            return Ok(());
        }
        if let Snapshot::Open { builder, first, .. } =
            std::mem::replace(&mut self.snapshot, Snapshot::Done)
        {
            builder.finish().map_err(|_| Error::Corrupt {
                offset: first,
                reason: reason::SNAPSHOT,
            })?;
        }
        Ok(())
    }

    /// Projects one checksum-verified op-12 or op-15 frame that starts at
    /// `offset`, decoded by the rule full replay applies
    /// ([`super::temporal::decode_logged`]). `collections` maps collection
    /// IDs to names and dimensions. Op-15 snapshot frames only hydrate the
    /// tracker: they restate history at the floor and report nothing.
    /// Temporal entries are refused with `UnsupportedFeature(FLAG_TEMPORAL)`,
    /// never omitted, while temporal support is off; a test that switches
    /// it on gets them as unannotated `GraphChange::Temporal` operations.
    pub(crate) fn frame(
        &mut self,
        frame: &format::RawFrame,
        offset: u64,
        collections: &BTreeMap<u16, (String, usize)>,
    ) -> Result<Vec<TtlEvent>> {
        let corrupt = |reason| Error::Corrupt { offset, reason };
        let sequence = frame.sequence;
        let body = super::temporal::decode_logged(self.flags, frame, offset)?;
        let restated = frame.operation == format::OP_GRAPH_SNAPSHOT;
        if restated {
            if let Snapshot::Before = self.snapshot {
                self.snapshot = Snapshot::Open {
                    builder: Box::new(SnapshotBuilder::new(sequence)),
                    sequence,
                    first: offset,
                };
            }
            match &mut self.snapshot {
                Snapshot::Open {
                    builder,
                    sequence: restated_at,
                    ..
                } if *restated_at == sequence => builder
                    .push(body.clone())
                    .map_err(|_| corrupt(reason::SNAPSHOT))?,
                _ => return Err(corrupt(reason::SNAPSHOT_ORDER)),
            }
        } else {
            self.end_snapshot()?;
            self.snapshot = Snapshot::Done;
        }
        let graph_id = body.graph_id.0;
        let collection_name = |collection: u16| {
            collections
                .get(&collection)
                .map(|entry| entry.0.clone())
                .ok_or_else(|| corrupt(reason::COLLECTION))
        };
        let mut events = Vec::new();
        let mut run = Run::default();
        let name = |graphs: &BTreeMap<u16, Graph>| {
            graphs
                .get(&graph_id)
                .map(|graph| graph.name.clone())
                .ok_or_else(|| corrupt(reason::GRAPH))
        };
        // Ends the ordinary run at a lifecycle, vector or reclaim boundary.
        let flush = |run: &mut Run, events: &mut Vec<TtlEvent>, graphs: &BTreeMap<u16, Graph>| {
            if !run.operations.is_empty() {
                run.flush(&name(graphs)?, sequence, false, events);
            }
            Ok::<(), Error>(())
        };
        for entry in body.entries {
            match entry {
                GraphEntry::CreateGraph { name: created } => {
                    flush(&mut run, &mut events, &self.graphs)?;
                    if self.graphs.contains_key(&graph_id)
                        || self.graphs.values().any(|graph| graph.name == created)
                    {
                        return Err(corrupt(reason::GRAPH_EXISTS));
                    }
                    self.graphs.insert(graph_id, Graph::new(created.clone()));
                    events.push(event(ChangeKind::GraphCreated { graph: created }));
                    continue;
                }
                GraphEntry::DropGraph => {
                    flush(&mut run, &mut events, &self.graphs)?;
                    let graph = name(&self.graphs)?;
                    let tracked = self
                        .graphs
                        .get(&graph_id)
                        .ok_or_else(|| corrupt(reason::GRAPH))?;
                    if !tracked.nodes.is_empty() || !tracked.edges.is_empty() {
                        return Err(corrupt(reason::GRAPH_NOT_EMPTY));
                    }
                    self.graphs.remove(&graph_id);
                    events.push(event(ChangeKind::GraphDropped { graph }));
                    continue;
                }
                GraphEntry::VectorMutation { collection, entry } => {
                    flush(&mut run, &mut events, &self.graphs)?;
                    // Vectors change no graph state, but address a graph.
                    name(&self.graphs)?;
                    let (collection_name, dimensions) = collections
                        .get(&collection)
                        .cloned()
                        .ok_or_else(|| corrupt(reason::COLLECTION))?;
                    let (decoded, _) =
                        format::decode_batch(&entry, 1, dimensions, offset, format::V5)?;
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
                        events.push(TtlEvent {
                            collection: collection_name.clone(),
                            kind,
                            ttl: Vec::new(),
                            reclaim: false,
                        });
                    }
                    continue;
                }
                GraphEntry::Reclaim { at_ms } => {
                    flush(&mut run, &mut events, &self.graphs)?;
                    let graph_name = name(&self.graphs)?;
                    let graph = self
                        .graphs
                        .get_mut(&graph_id)
                        .ok_or_else(|| corrupt(reason::GRAPH))?;
                    let mut reclaimed =
                        reclaim(graph, &mut self.owners, at_ms, collections, offset)?;
                    reclaimed.flush(&graph_name, sequence, true, &mut events);
                    continue;
                }
                GraphEntry::Temporal(_) if !super::temporal::supported() => {
                    return Err(Error::UnsupportedFeature(format::FLAG_TEMPORAL));
                }
                _ => {}
            }
            let graph = self
                .graphs
                .get_mut(&graph_id)
                .ok_or_else(|| corrupt(reason::GRAPH))?;
            let node_key = |graph: &Graph, id: u64| {
                graph
                    .nodes
                    .get(&id)
                    .map(|node| node.key.clone())
                    .ok_or_else(|| corrupt(reason::NODE))
            };
            let (operation, ttl) = match entry {
                GraphEntry::GraphHighWater {
                    next_node_id,
                    next_edge_id,
                } => {
                    if next_node_id < graph.next_node_id || next_edge_id < graph.next_edge_id {
                        return Err(corrupt(reason::HIGH_WATER));
                    }
                    graph.next_node_id = next_node_id;
                    graph.next_edge_id = next_edge_id;
                    continue;
                }
                GraphEntry::PutNode {
                    node_id,
                    key,
                    labels: set,
                    properties,
                } => {
                    let id = node_id.0;
                    let (created, expires_at_ms) = match graph.nodes.get(&id) {
                        // A replacement keeps the node's deadline.
                        Some(node) if node.key == key => (false, node.expires_at_ms),
                        Some(_) => return Err(corrupt(reason::NODE_ID)),
                        None => {
                            graph.next_node_id =
                                allocate(graph.next_node_id, id, reason::NODE_ID, offset)?;
                            if !graph.node_keys.insert(key.clone()) {
                                return Err(corrupt(reason::NODE_KEY));
                            }
                            graph.nodes.insert(
                                id,
                                Node {
                                    key: key.clone(),
                                    expires_at_ms: None,
                                },
                            );
                            (true, None)
                        }
                    };
                    (
                        GraphChange::NodeUpserted {
                            id,
                            key,
                            labels: labels(&set),
                            properties: metadata_from_properties(&properties),
                            created,
                        },
                        Some(GraphTtl::Updated {
                            expires_at_ms,
                            expiry_changed: false,
                        }),
                    )
                }
                GraphEntry::PatchNode {
                    node_id,
                    add_labels,
                    remove_labels,
                    set,
                    unset,
                } => {
                    let node = graph
                        .nodes
                        .get(&node_id.0)
                        .ok_or_else(|| corrupt(reason::NODE))?;
                    (
                        GraphChange::NodePatched {
                            id: node_id.0,
                            key: node.key.clone(),
                            add_labels: labels(&add_labels),
                            remove_labels: labels(&remove_labels),
                            set: metadata_from_properties(&set),
                            unset: labels(&unset),
                        },
                        Some(GraphTtl::Updated {
                            expires_at_ms: node.expires_at_ms,
                            expiry_changed: false,
                        }),
                    )
                }
                GraphEntry::NodeExpiry {
                    node_id,
                    expires_at_ms,
                } => {
                    let node = graph
                        .nodes
                        .get_mut(&node_id.0)
                        .ok_or_else(|| corrupt(reason::NODE))?;
                    node.expires_at_ms = expires_at_ms;
                    (
                        GraphChange::NodePatched {
                            id: node_id.0,
                            key: node.key.clone(),
                            add_labels: Vec::new(),
                            remove_labels: Vec::new(),
                            set: Metadata::new(),
                            unset: Vec::new(),
                        },
                        Some(GraphTtl::Updated {
                            expires_at_ms,
                            expiry_changed: true,
                        }),
                    )
                }
                GraphEntry::DeleteNode { node_id } => {
                    let id = node_id.0;
                    if graph
                        .edges
                        .values()
                        .any(|edge| edge.source == id || edge.destination == id)
                    {
                        return Err(corrupt(reason::NODE_EDGES));
                    }
                    let node = graph
                        .nodes
                        .remove(&id)
                        .ok_or_else(|| corrupt(reason::NODE))?;
                    graph.node_keys.remove(&node.key);
                    // Engine state releases the node's attachments silently;
                    // the feed reports no detach for them.
                    let released: Vec<(u16, u64)> = graph
                        .attachments
                        .range((id, 0)..=(id, u16::MAX))
                        .map(|(&(_, collection), &record)| (collection, record))
                        .collect();
                    for (collection, record) in released {
                        graph.attachments.remove(&(id, collection));
                        self.owners.remove(&(collection, record));
                    }
                    (
                        GraphChange::NodeDeleted { id, key: node.key },
                        Some(GraphTtl::ExplicitRemoval),
                    )
                }
                GraphEntry::PutEdge {
                    edge_id,
                    source,
                    destination,
                    relationship,
                    key,
                    properties,
                } => {
                    let id = edge_id.0;
                    graph.next_edge_id = allocate(graph.next_edge_id, id, reason::EDGE_ID, offset)?;
                    let source_key = node_key(graph, source.0)?;
                    let destination_key = node_key(graph, destination.0)?;
                    if let Some(key) = &key
                        && !graph.edge_keys.insert(key.clone())
                    {
                        return Err(corrupt(reason::EDGE_KEY));
                    }
                    graph.edges.insert(
                        id,
                        Edge {
                            key: key.clone(),
                            source: source.0,
                            destination: destination.0,
                            expires_at_ms: None,
                        },
                    );
                    (
                        GraphChange::EdgeUpserted {
                            id,
                            key,
                            source: source.0,
                            source_key,
                            relationship,
                            destination: destination.0,
                            destination_key,
                            properties: metadata_from_properties(&properties),
                            created: true,
                        },
                        Some(GraphTtl::Updated {
                            expires_at_ms: None,
                            expiry_changed: false,
                        }),
                    )
                }
                GraphEntry::PatchEdge {
                    edge_id,
                    set,
                    unset,
                } => {
                    let edge = graph
                        .edges
                        .get(&edge_id.0)
                        .ok_or_else(|| corrupt(reason::EDGE))?;
                    (
                        GraphChange::EdgePatched {
                            id: edge_id.0,
                            key: edge.key.clone(),
                            set: metadata_from_properties(&set),
                            unset: labels(&unset),
                        },
                        Some(GraphTtl::Updated {
                            expires_at_ms: edge.expires_at_ms,
                            expiry_changed: false,
                        }),
                    )
                }
                GraphEntry::EdgeExpiry {
                    edge_id,
                    expires_at_ms,
                } => {
                    let edge = graph
                        .edges
                        .get_mut(&edge_id.0)
                        .ok_or_else(|| corrupt(reason::EDGE))?;
                    edge.expires_at_ms = expires_at_ms;
                    (
                        GraphChange::EdgePatched {
                            id: edge_id.0,
                            key: edge.key.clone(),
                            set: Metadata::new(),
                            unset: Vec::new(),
                        },
                        Some(GraphTtl::Updated {
                            expires_at_ms,
                            expiry_changed: true,
                        }),
                    )
                }
                GraphEntry::DeleteEdge { edge_id } => {
                    let edge = graph
                        .edges
                        .remove(&edge_id.0)
                        .ok_or_else(|| corrupt(reason::EDGE))?;
                    if let Some(key) = &edge.key {
                        graph.edge_keys.remove(key);
                    }
                    (
                        GraphChange::EdgeDeleted {
                            id: edge_id.0,
                            key: edge.key,
                        },
                        Some(GraphTtl::ExplicitRemoval),
                    )
                }
                GraphEntry::Attach {
                    node_id,
                    collection,
                    record_id,
                } => {
                    let node_key = node_key(graph, node_id.0)?;
                    let collection_name = collection_name(collection)?;
                    if graph.attachments.contains_key(&(node_id.0, collection))
                        || self.owners.contains_key(&(collection, record_id))
                    {
                        return Err(corrupt(reason::ATTACHED));
                    }
                    graph.attachments.insert((node_id.0, collection), record_id);
                    self.owners
                        .insert((collection, record_id), (graph_id, node_id.0));
                    (
                        GraphChange::Attached {
                            node: node_id.0,
                            node_key,
                            collection: collection_name,
                            record: record_id,
                        },
                        None,
                    )
                }
                GraphEntry::Detach {
                    node_id,
                    collection,
                } => {
                    let node_key = node_key(graph, node_id.0)?;
                    let collection_name = collection_name(collection)?;
                    let record = graph
                        .attachments
                        .remove(&(node_id.0, collection))
                        .ok_or_else(|| corrupt(reason::DETACHED))?;
                    self.owners.remove(&(collection, record));
                    (
                        GraphChange::Detached {
                            node: node_id.0,
                            node_key,
                            collection: collection_name,
                            record,
                        },
                        None,
                    )
                }
                GraphEntry::PutSchema { schema } => (GraphChange::SchemaUpdated { schema }, None),
                GraphEntry::Temporal(entry) => {
                    (GraphChange::Temporal(temporal(graph, entry, offset)?), None)
                }
                GraphEntry::CreateGraph { .. }
                | GraphEntry::DropGraph
                | GraphEntry::VectorMutation { .. }
                | GraphEntry::Reclaim { .. } => continue,
            };
            run.push(operation, ttl);
        }
        flush(&mut run, &mut events, &self.graphs)?;
        if restated {
            // Hydrated only: history at the floor is not reported.
            events.clear();
        }
        Ok(events)
    }
}

/// One temporal entry's change (test-only temporal support): a new
/// temporal edge is tracked like an edge put, and every reference must
/// resolve. Corrections carry the entry's bounds and new values only.
fn temporal(graph: &mut Graph, entry: TemporalEntry, offset: u64) -> Result<TemporalChange> {
    use super::temporal::segment_out;
    let corrupt = |reason| Error::Corrupt { offset, reason };
    let key = |graph: &Graph, id: u64| {
        graph
            .edges
            .get(&id)
            .map(|edge| edge.key.clone())
            .ok_or_else(|| corrupt(reason::EDGE))
    };
    let node_key = |graph: &Graph, id: u64| {
        graph
            .nodes
            .get(&id)
            .map(|node| node.key.clone())
            .ok_or_else(|| corrupt(reason::NODE))
    };
    Ok(match entry {
        TemporalEntry::PutEdge {
            edge_id,
            source,
            destination,
            relationship,
            key,
            segments,
            ..
        } => {
            let id = edge_id.0;
            let source_key = node_key(graph, source.0)?;
            let destination_key = node_key(graph, destination.0)?;
            let created = !graph.edges.contains_key(&id);
            if created {
                graph.next_edge_id = allocate(graph.next_edge_id, id, reason::EDGE_ID, offset)?;
                if let Some(key) = &key
                    && !graph.edge_keys.insert(key.clone())
                {
                    return Err(corrupt(reason::EDGE_KEY));
                }
                graph.edges.insert(
                    id,
                    Edge {
                        key: key.clone(),
                        source: source.0,
                        destination: destination.0,
                        expires_at_ms: None,
                    },
                );
            }
            TemporalChange::EdgePut {
                id,
                key,
                source: source.0,
                source_key,
                relationship,
                destination: destination.0,
                destination_key,
                segments: segments.iter().map(segment_out).collect(),
                created,
            }
        }
        TemporalEntry::AppendSegment { edge_id, segment } => TemporalChange::SegmentAppended {
            id: edge_id.0,
            key: key(graph, edge_id.0)?,
            segment: segment_out(&segment),
        },
        TemporalEntry::ReplaceSegment {
            edge_id,
            valid_from,
            valid_to,
            segment,
        } => TemporalChange::SegmentReplaced {
            id: edge_id.0,
            key: key(graph, edge_id.0)?,
            valid_from,
            valid_to,
            segment: segment_out(&segment),
        },
        TemporalEntry::CloseSegment {
            edge_id,
            valid_from,
            valid_to,
            at,
        } => TemporalChange::SegmentClosed {
            id: edge_id.0,
            key: key(graph, edge_id.0)?,
            valid_from,
            valid_to,
            at,
        },
        TemporalEntry::RemoveSegment {
            edge_id,
            valid_from,
            valid_to,
        } => TemporalChange::SegmentRemoved {
            id: edge_id.0,
            key: key(graph, edge_id.0)?,
            valid_from,
            valid_to,
        },
    })
}

fn event(kind: ChangeKind) -> TtlEvent {
    TtlEvent {
        collection: String::new(),
        kind,
        ttl: Vec::new(),
        reclaim: false,
    }
}

/// The operations of one logged reclaim at `at_ms`, applied to `graph`:
/// every node due at `at_ms` (own deadline `<= at_ms`) and every edge due
/// itself or incident to such a node, each once. Edge deletes go first in
/// ID order, then the due nodes' detaches by `(node, collection name bytes,
/// record)`, then node deletes in ID order. Annotations are computed before
/// any state is erased.
fn reclaim(
    graph: &mut Graph,
    owners: &mut BTreeMap<(u16, u64), (u16, u64)>,
    at_ms: u64,
    collections: &BTreeMap<u16, (String, usize)>,
    offset: u64,
) -> Result<Run> {
    let corrupt = |reason| Error::Corrupt { offset, reason };
    let due = |deadline: Option<u64>| deadline.is_some_and(|deadline| deadline <= at_ms);
    let nodes: BTreeSet<u64> = graph
        .nodes
        .iter()
        .filter(|(_, node)| due(node.expires_at_ms))
        .map(|(id, _)| *id)
        .collect();
    let deadline_of = |id: u64| {
        graph
            .nodes
            .get(&id)
            .map(|node| node.expires_at_ms)
            .ok_or_else(|| corrupt(reason::NODE))
    };
    let mut run = Run::default();
    let mut edges = Vec::new();
    for (&id, edge) in &graph.edges {
        let incident = nodes.contains(&edge.source) || nodes.contains(&edge.destination);
        if !incident && !due(edge.expires_at_ms) {
            continue;
        }
        let effective = [
            edge.expires_at_ms,
            deadline_of(edge.source)?,
            deadline_of(edge.destination)?,
        ]
        .into_iter()
        .flatten()
        .min()
        .filter(|effective| *effective <= at_ms)
        .ok_or_else(|| corrupt(reason::EDGE))?;
        let removal_reason = if edge.expires_at_ms == Some(effective) {
            GraphRemovalReason::Expired
        } else {
            GraphRemovalReason::NodeExpired
        };
        edges.push(id);
        run.push(
            GraphChange::EdgeDeleted {
                id,
                key: edge.key.clone(),
            },
            Some(GraphTtl::ExpiryRemoval {
                removal_reason,
                expires_at_ms: effective,
                removed_at_ms: at_ms,
            }),
        );
    }
    let mut detaches = Vec::new();
    for &node in &nodes {
        for (&(_, collection), &record) in graph.attachments.range((node, 0)..=(node, u16::MAX)) {
            let name = collections
                .get(&collection)
                .map(|entry| entry.0.clone())
                .ok_or_else(|| corrupt(reason::COLLECTION))?;
            detaches.push((node, name, record, collection));
        }
    }
    detaches.sort_by(|a, b| (a.0, a.1.as_bytes(), a.2).cmp(&(b.0, b.1.as_bytes(), b.2)));
    for (node, collection, record, _) in &detaches {
        run.push(
            GraphChange::Detached {
                node: *node,
                node_key: graph
                    .nodes
                    .get(node)
                    .map(|node| node.key.clone())
                    .ok_or_else(|| corrupt(reason::NODE))?,
                collection: collection.clone(),
                record: *record,
            },
            None,
        );
    }
    for &id in &nodes {
        let node = graph.nodes.get(&id).ok_or_else(|| corrupt(reason::NODE))?;
        let expires_at_ms = node.expires_at_ms.ok_or_else(|| corrupt(reason::NODE))?;
        run.push(
            GraphChange::NodeDeleted {
                id,
                key: node.key.clone(),
            },
            Some(GraphTtl::ExpiryRemoval {
                removal_reason: GraphRemovalReason::Expired,
                expires_at_ms,
                removed_at_ms: at_ms,
            }),
        );
    }
    // Erase only after every annotation was computed.
    for id in edges {
        if let Some(edge) = graph.edges.remove(&id)
            && let Some(key) = edge.key
        {
            graph.edge_keys.remove(&key);
        }
    }
    for (node, _, record, collection) in detaches {
        graph.attachments.remove(&(node, collection));
        owners.remove(&(collection, record));
    }
    for id in nodes {
        if let Some(node) = graph.nodes.remove(&id) {
            graph.node_keys.remove(&node.key);
        }
    }
    Ok(run)
}
