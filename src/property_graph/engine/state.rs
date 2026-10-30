//! In-memory graph state: authoritative named graphs, nodes, edges and
//! attachments plus derived indexes (key maps, adjacency, label sets,
//! attachment owners).
//!
//! Every graph body is applied atomically: entries are applied in order, later
//! entries observe earlier ones, and any failure restores the exact previous
//! state. Derived indexes are maintained incrementally and are equal to the
//! indexes rebuilt from the authoritative data (INV-009,
//! [`GraphState::check_invariants`]).

use super::codec::{GraphBody, GraphEntry};
use super::error::GraphError;
use super::limits::{MAX_GRAPHS, MAX_LABELS};
use super::model::{Direction, EdgeId, GraphEdge, GraphId, GraphNode, NodeId, Properties};
use super::properties;
use super::schema::{self, GraphSchema, PropertyIndex};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, btree_set};
use std::iter::Peekable;
use std::ops::Bound::{Excluded, Unbounded};
use std::sync::Arc;

// A thin shared pointer keeps each entry to two machine words; Arc<str>
// would add a length word to every adjacency entry.
type AdjacencyEntry = (Arc<String>, EdgeId);
type AdjacencyIndex = BTreeMap<NodeId, Vec<AdjacencyEntry>>;

/// One named graph.
#[derive(Clone, Debug, PartialEq)]
pub struct Graph {
    id: GraphId,
    name: String,
    next_node_id: u64,
    next_edge_id: u64,
    graph_sequence: u64,
    nodes: BTreeMap<NodeId, GraphNode>,
    edges: BTreeMap<EdgeId, GraphEdge>,
    /// Node → collection → record. At most one record per (node, collection).
    attachments: BTreeMap<NodeId, BTreeMap<u16, u64>>,
    // Derived indexes.
    node_keys: BTreeMap<String, NodeId>,
    edge_keys: BTreeMap<String, EdgeId>,
    out_edges: AdjacencyIndex,
    in_edges: AdjacencyIndex,
    /// Shared relationship names and their live edge counts. Counts belong to
    /// this graph, not to the Arc, so cloning and rollback do not retain symbols.
    relationships: BTreeMap<Arc<String>, usize>,
    labels: BTreeMap<String, BTreeSet<NodeId>>,
    /// Declared schema (EPIC-008); empty when none was declared.
    schema: GraphSchema,
    /// Derived declared property indexes, keyed by (label, property).
    property_indexes: BTreeMap<(String, String), PropertyIndex>,
}

fn remove_from<K: Ord, V: Ord>(map: &mut BTreeMap<K, BTreeSet<V>>, key: K, value: &V) {
    if let Some(set) = map.get_mut(&key) {
        set.remove(value);
        if set.is_empty() {
            map.remove(&key);
        }
    }
}

fn insert_adjacency(index: &mut AdjacencyIndex, node: NodeId, entry: AdjacencyEntry) {
    let entries = index.entry(node).or_default();
    let position = entries
        .binary_search(&entry)
        .expect_err("new adjacency entry");
    entries.insert(position, entry);
}

fn remove_adjacency(index: &mut AdjacencyIndex, node: NodeId, entry: &AdjacencyEntry) {
    let entries = index.get_mut(&node).expect("adjacency exists");
    let position = entries
        .binary_search(entry)
        .expect("adjacency entry exists");
    entries.remove(position);
    if entries.is_empty() {
        index.remove(&node);
    } else if entries.capacity() > 4 && entries.len() <= entries.capacity() / 4 {
        entries.shrink_to(entries.len() * 2);
    }
}

fn adjacency_range<'a>(
    entries: &'a [AdjacencyEntry],
    relationship: &str,
    after: Option<EdgeId>,
) -> &'a [AdjacencyEntry] {
    let start = entries.partition_point(|(kind, id)| {
        kind.as_str() < relationship
            || (kind.as_str() == relationship && after.is_some_and(|after| *id <= after))
    });
    let end = start + entries[start..].partition_point(|(kind, _)| kind.as_str() == relationship);
    &entries[start..end]
}

impl Graph {
    fn new(id: GraphId, name: String, sequence: u64) -> Self {
        Self {
            id,
            name,
            next_node_id: 0,
            next_edge_id: 0,
            graph_sequence: sequence,
            nodes: BTreeMap::new(),
            edges: BTreeMap::new(),
            attachments: BTreeMap::new(),
            node_keys: BTreeMap::new(),
            edge_keys: BTreeMap::new(),
            out_edges: BTreeMap::new(),
            in_edges: BTreeMap::new(),
            relationships: BTreeMap::new(),
            labels: BTreeMap::new(),
            schema: GraphSchema::default(),
            property_indexes: BTreeMap::new(),
        }
    }

    pub fn id(&self) -> GraphId {
        self.id
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    /// Lowest node ID that may be assigned to a new node.
    pub fn next_node_id(&self) -> u64 {
        self.next_node_id
    }
    /// Lowest edge ID that may be assigned to a new edge.
    pub fn next_edge_id(&self) -> u64 {
        self.next_edge_id
    }
    /// Sequence of the last graph frame applied to this graph.
    pub fn graph_sequence(&self) -> u64 {
        self.graph_sequence
    }
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }
    pub fn node(&self, id: NodeId) -> Option<&GraphNode> {
        self.nodes.get(&id)
    }
    pub fn node_by_key(&self, key: &str) -> Option<&GraphNode> {
        self.node_keys.get(key).and_then(|id| self.nodes.get(id))
    }
    pub fn edge(&self, id: EdgeId) -> Option<&GraphEdge> {
        self.edges.get(&id)
    }
    pub fn edge_by_key(&self, key: &str) -> Option<&GraphEdge> {
        self.edge_keys.get(key).and_then(|id| self.edges.get(id))
    }
    /// Nodes in ascending ID order.
    pub fn nodes(&self) -> impl Iterator<Item = &GraphNode> {
        self.nodes.values()
    }
    /// Edges in ascending ID order.
    pub fn edges(&self) -> impl Iterator<Item = &GraphEdge> {
        self.edges.values()
    }
    /// IDs of nodes carrying `label`, ascending.
    pub fn nodes_with_label(&self, label: &str) -> impl Iterator<Item = NodeId> + '_ {
        self.labels.get(label).into_iter().flatten().copied()
    }
    /// Lazily yields the IDs of nodes carrying `label` that are above
    /// `after`, ascending.
    pub fn label_scan(
        &self,
        label: &str,
        after: Option<NodeId>,
    ) -> impl Iterator<Item = NodeId> + '_ {
        let lower = after.map_or(Unbounded, Excluded);
        self.labels
            .get(label)
            .into_iter()
            .flat_map(move |ids| ids.range((lower, Unbounded)))
            .copied()
    }
    /// Lazily yields the nodes above `after` in ascending ID order.
    pub fn node_scan(&self, after: Option<NodeId>) -> impl Iterator<Item = &GraphNode> + '_ {
        self.nodes
            .range((after.map_or(Unbounded, Excluded), Unbounded))
            .map(|(_, node)| node)
    }
    /// Lazily yields `node`'s adjacency entries in `direction` as
    /// (relationship type, edge ID), ascending, starting after `after`.
    ///
    /// With `types`, only those types are read, by one range scan per type.
    /// `Both` merges the outgoing and incoming entries and yields a self-loop
    /// once. Nothing is materialized, so callers can stop at any budget.
    pub fn adjacency<'a>(
        &'a self,
        node: NodeId,
        direction: Direction,
        types: Option<&'a BTreeSet<String>>,
        after: Option<(&str, EdgeId)>,
    ) -> Adjacency<'a> {
        let side = |index: &'a AdjacencyIndex, used: bool| {
            let entries = used
                .then(|| index.get(&node))
                .flatten()
                .map_or(&[][..], Vec::as_slice);
            Side::new(entries, types, after).peekable()
        };
        Adjacency {
            out: side(&self.out_edges, direction != Direction::In),
            inc: side(&self.in_edges, direction != Direction::Out),
        }
    }
    /// Edges incident to `node` in `direction`, ordered by (relationship type,
    /// edge ID) ascending. A self-loop appears once, also for `Both`.
    pub fn edges_of(&self, node: NodeId, direction: Direction) -> Vec<EdgeId> {
        self.adjacency(node, direction, None, None)
            .map(|(_, id)| id)
            .collect()
    }
    /// `(collection, record)` attachments of `node`, ascending by collection.
    pub fn attachments(&self, node: NodeId) -> impl Iterator<Item = (u16, u64)> + '_ {
        self.attachments
            .get(&node)
            .into_iter()
            .flatten()
            .map(|(c, r)| (*c, *r))
    }

    /// The declared schema; empty when none was declared.
    pub fn schema(&self) -> &GraphSchema {
        &self.schema
    }
    /// The declared index on `label`.`field`, if any.
    pub(super) fn property_index(&self, label: &str, field: &str) -> Option<&PropertyIndex> {
        self.property_indexes
            .get(&(label.to_owned(), field.to_owned()))
    }
    /// IDs of `relationship` edges from `source` to `destination`, ascending.
    pub fn edges_between<'a>(
        &'a self,
        source: NodeId,
        relationship: &str,
        destination: NodeId,
    ) -> impl Iterator<Item = EdgeId> + 'a {
        let entries = self.out_edges.get(&source).map_or(&[][..], Vec::as_slice);
        adjacency_range(entries, relationship, None)
            .iter()
            .map(|(_, id)| *id)
            .filter(move |id| {
                self.edges
                    .get(id)
                    .is_some_and(|edge| edge.destination == destination)
            })
    }

    fn rebuild_property_indexes(&mut self) {
        self.property_indexes.clear();
        for node in self.nodes.values() {
            schema::index_node(&self.schema, &mut self.property_indexes, node, true);
        }
    }

    fn incident_edges(&self, node: NodeId) -> usize {
        self.edges_of(node, Direction::Both).len()
    }

    fn insert_node(&mut self, node: GraphNode) {
        schema::index_node(&self.schema, &mut self.property_indexes, &node, true);
        self.node_keys.insert(node.key.clone(), node.id);
        for label in &node.labels {
            self.labels
                .entry(label.clone())
                .or_default()
                .insert(node.id);
        }
        self.nodes.insert(node.id, node);
    }

    fn remove_node(&mut self, id: NodeId) -> Option<GraphNode> {
        let node = self.nodes.remove(&id)?;
        self.node_keys.remove(&node.key);
        for label in &node.labels {
            remove_from(&mut self.labels, label.clone(), &id);
        }
        schema::index_node(&self.schema, &mut self.property_indexes, &node, false);
        Some(node)
    }

    fn insert_edge(&mut self, edge: GraphEdge) {
        if let Some(key) = &edge.key {
            self.edge_keys.insert(key.clone(), edge.id);
        }
        let relationship = match self.relationships.get_key_value(&edge.relationship) {
            Some((symbol, _)) => Arc::clone(symbol),
            None => Arc::new(edge.relationship.clone()),
        };
        *self
            .relationships
            .entry(Arc::clone(&relationship))
            .or_default() += 1;
        let entry = (relationship, edge.id);
        insert_adjacency(&mut self.out_edges, edge.source, entry.clone());
        insert_adjacency(&mut self.in_edges, edge.destination, entry);
        self.edges.insert(edge.id, edge);
    }

    fn remove_edge(&mut self, id: EdgeId) -> Option<GraphEdge> {
        let edge = self.edges.remove(&id)?;
        if let Some(key) = &edge.key {
            self.edge_keys.remove(key);
        }
        let (relationship, count) = self
            .relationships
            .get_key_value(&edge.relationship)
            .expect("relationship exists");
        let entry = (Arc::clone(relationship), id);
        let remaining = count - 1;
        remove_adjacency(&mut self.out_edges, edge.source, &entry);
        remove_adjacency(&mut self.in_edges, edge.destination, &entry);
        if remaining == 0 {
            self.relationships.remove(&edge.relationship);
        } else {
            *self
                .relationships
                .get_mut(&edge.relationship)
                .expect("relationship exists") = remaining;
        }
        Some(edge)
    }

    /// A copy whose derived indexes are rebuilt from authoritative data.
    fn rebuilt(&self) -> Self {
        let mut graph = Self::new(self.id, self.name.clone(), self.graph_sequence);
        graph.next_node_id = self.next_node_id;
        graph.next_edge_id = self.next_edge_id;
        graph.attachments = self.attachments.clone();
        graph.schema = self.schema.clone();
        self.nodes
            .values()
            .cloned()
            .for_each(|n| graph.insert_node(n));
        self.edges
            .values()
            .cloned()
            .for_each(|e| graph.insert_edge(e));
        graph
    }
}

/// Lazy adjacency iterator returned by [`Graph::adjacency`].
pub struct Adjacency<'a> {
    out: Peekable<Side<'a>>,
    inc: Peekable<Side<'a>>,
}

impl<'a> Iterator for Adjacency<'a> {
    type Item = (&'a str, EdgeId);

    fn next(&mut self) -> Option<Self::Item> {
        let order = match (self.out.peek(), self.inc.peek()) {
            (Some(out), Some(inc)) => out.cmp(inc),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => return None,
        };
        match order {
            Ordering::Less => self.out.next(),
            Ordering::Greater => self.inc.next(),
            // Edge IDs are unique, so equal entries are one self-loop.
            Ordering::Equal => {
                self.inc.next();
                self.out.next()
            }
        }
    }
}

/// One adjacency set of one node, restricted to `types`.
struct Side<'a> {
    entries: &'a [AdjacencyEntry],
    /// Types still to scan (typed scans only).
    types: Option<btree_set::Range<'a, String>>,
    current: std::slice::Iter<'a, AdjacencyEntry>,
}

impl<'a> Side<'a> {
    fn new(
        entries: &'a [AdjacencyEntry],
        types: Option<&'a BTreeSet<String>>,
        after: Option<(&str, EdgeId)>,
    ) -> Self {
        let (types, current) = match (types, after) {
            (None, None) => (None, entries.iter()),
            (None, Some((kind, edge))) => {
                let start = entries.partition_point(|(relationship, id)| {
                    (relationship.as_str(), *id) <= (kind, edge)
                });
                (None, entries[start..].iter())
            }
            (Some(types), None) => (Some(types.range::<str, _>(..)), entries[..0].iter()),
            (Some(types), Some((kind, edge))) => (
                Some(types.range::<str, _>((Excluded(kind), Unbounded))),
                if types.contains(kind) {
                    adjacency_range(entries, kind, Some(edge)).iter()
                } else {
                    entries[..0].iter()
                },
            ),
        };
        Self {
            entries,
            types,
            current,
        }
    }
}

impl<'a> Iterator for Side<'a> {
    type Item = (&'a str, EdgeId);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some((kind, edge)) = self.current.next() {
                return Some((kind.as_str(), *edge));
            }
            let kind = self.types.as_mut()?.next()?;
            self.current = adjacency_range(self.entries, kind, None).iter();
        }
    }
}

/// Inverse of one applied entry, used to roll a failed batch back.
pub(super) enum Undo {
    CreateGraph,
    DropGraph(Box<Graph>),
    HighWater {
        next_node_id: u64,
        next_edge_id: u64,
    },
    InsertNode {
        id: NodeId,
        previous_next: u64,
    },
    RestoreNode(GraphNode),
    DeleteNode {
        node: GraphNode,
        attachments: BTreeMap<u16, u64>,
    },
    InsertEdge {
        id: EdgeId,
        previous_next: u64,
    },
    RestoreEdge(GraphEdge),
    DeleteEdge(GraphEdge),
    Attach {
        node: NodeId,
        collection: u16,
    },
    Detach {
        node: NodeId,
        collection: u16,
        record: u64,
    },
    Schema(Box<GraphSchema>),
}

/// All named graphs of one graph-enabled file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GraphState {
    /// Live graphs. At most `MAX_GRAPHS` exist at once; the ID of a dropped
    /// (necessarily empty) graph may be assigned again, while node and edge
    /// high-water marks belong to each graph and start at 0.
    graphs: BTreeMap<GraphId, Graph>,
    last_sequence: u64,
    // Derived indexes.
    names: BTreeMap<String, GraphId>,
    /// PD-012: `(collection, record)` → owning node, across all graphs.
    attachment_owners: BTreeMap<(u16, u64), (GraphId, NodeId)>,
}

fn new_id(next: &mut u64, id: u64) -> Result<u64, GraphError> {
    let following = id.checked_add(1).ok_or(GraphError::IdSpaceExhausted)?;
    let previous = *next;
    *next = following;
    Ok(previous)
}

fn patched(
    mut properties: Properties,
    set: &Properties,
    unset: &BTreeSet<String>,
) -> Result<Properties, GraphError> {
    for name in unset {
        properties.remove(name);
    }
    properties.extend(set.iter().map(|(k, v)| (k.clone(), v.clone())));
    properties::validate(&properties)?;
    Ok(properties)
}

impl GraphState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sequence of the last applied graph frame (0 before the first).
    pub fn last_sequence(&self) -> u64 {
        self.last_sequence
    }
    /// Lowest graph ID not used by a live graph, or `None` when `MAX_GRAPHS`
    /// graphs exist.
    pub fn free_graph_id(&self) -> Option<GraphId> {
        (0..MAX_GRAPHS)
            .map(GraphId)
            .find(|id| !self.graphs.contains_key(id))
    }
    pub fn graph(&self, id: GraphId) -> Option<&Graph> {
        self.graphs.get(&id)
    }
    pub fn graph_by_name(&self, name: &str) -> Option<&Graph> {
        self.names.get(name).and_then(|id| self.graphs.get(id))
    }
    /// Graphs in ascending ID order.
    pub fn graphs(&self) -> impl Iterator<Item = &Graph> {
        self.graphs.values()
    }
    /// The node a record is attached to, if any.
    pub fn attachment_owner(&self, collection: u16, record: u64) -> Option<(GraphId, NodeId)> {
        self.attachment_owners.get(&(collection, record)).copied()
    }
    /// Every attached `(collection, record)`, ascending.
    pub fn attached_records(&self) -> impl Iterator<Item = (u16, u64)> + '_ {
        self.attachment_owners.keys().copied()
    }

    /// Applies `body` committed at `sequence` atomically: on error nothing changes.
    ///
    /// Vector-mutation entries do not change graph state; the adapter applies
    /// them to collections and checks record existence for `Attach`. The
    /// encoded byte limit is enforced by [`super::codec::encode_body`].
    ///
    /// `graph_sequence` advances only for the body's graph when the body has
    /// a graph entry, and for each graph owning a record touched by a vector
    /// mutation (Section 3.5); unrelated vector writes keep graph cursors valid.
    pub fn apply(&mut self, sequence: u64, body: &GraphBody) -> Result<(), GraphError> {
        self.apply_at(sequence, body, false)
    }

    /// [`Self::apply`] for bodies restated by a checkpoint image: several
    /// bodies may share the checkpoint `sequence` (`restated`), so it need
    /// only be at least the last applied sequence.
    pub fn apply_at(
        &mut self,
        sequence: u64,
        body: &GraphBody,
        restated: bool,
    ) -> Result<(), GraphError> {
        self.apply_entries(sequence, body, restated)?;
        // Attachments of other graphs cannot change in this body, and any
        // attach/detach in this graph already advances it, so looking owners
        // up after the body applied is exact.
        let mut changed = BTreeSet::new();
        for entry in &body.entries {
            match entry.vector_record() {
                Some(record) => {
                    if let Some((graph, _)) = self.attachment_owners.get(&record) {
                        changed.insert(*graph);
                    }
                }
                None => {
                    changed.insert(body.graph_id);
                }
            }
        }
        for id in changed {
            if let Some(graph) = self.graphs.get_mut(&id) {
                graph.graph_sequence = sequence;
            }
        }
        self.last_sequence = sequence;
        Ok(())
    }

    /// Records that an ordinary vector commit at `sequence` changed record
    /// `(collection, record)`. When the record is attached, the owning
    /// graph's `graph_sequence` advances (Section 3.5, LEG-005/LEG-006);
    /// otherwise nothing changes.
    pub fn touch_record(&mut self, collection: u16, record: u64, sequence: u64) {
        if let Some((graph, _)) = self.attachment_owners.get(&(collection, record))
            && let Some(graph) = self.graphs.get_mut(graph)
        {
            graph.graph_sequence = graph.graph_sequence.max(sequence);
        }
    }

    /// Returns exactly what [`Self::apply`] would return, leaving the state
    /// unchanged (validate-then-append-then-apply, PAT-001).
    pub fn validate(&mut self, sequence: u64, body: &GraphBody) -> Result<(), GraphError> {
        let undo = self.apply_entries(sequence, body, false)?;
        self.rollback(body.graph_id, undo);
        Ok(())
    }

    fn apply_entries(
        &mut self,
        sequence: u64,
        body: &GraphBody,
        restated: bool,
    ) -> Result<Vec<Undo>, GraphError> {
        if sequence < self.last_sequence || (sequence == self.last_sequence && !restated) {
            return Err(GraphError::SequenceNotIncreasing {
                last: self.last_sequence,
                sequence,
            });
        }
        body.validate()?;
        let mut undo = Vec::with_capacity(body.entries.len());
        for entry in &body.entries {
            match self.apply_entry(body.graph_id, sequence, entry) {
                Ok(Some(inverse)) => undo.push(inverse),
                Ok(None) => {}
                Err(error) => {
                    self.rollback(body.graph_id, undo);
                    return Err(error);
                }
            }
        }
        Ok(undo)
    }

    pub(super) fn apply_entry(
        &mut self,
        graph_id: GraphId,
        sequence: u64,
        entry: &GraphEntry,
    ) -> Result<Option<Undo>, GraphError> {
        let undo = self.apply_entry_unchecked(graph_id, sequence, entry)?;
        // REQ-030: a graph with a declared schema accepts only conforming
        // writes; graphs without one behave exactly as before.
        if let Some(graph) = self.graphs.get(&graph_id)
            && !graph.schema.is_empty()
            && let Err(error) = schema::check_entry(graph, entry)
        {
            if let Some(undo) = undo {
                self.rollback(graph_id, vec![undo]);
            }
            return Err(error);
        }
        Ok(undo)
    }

    fn apply_entry_unchecked(
        &mut self,
        graph_id: GraphId,
        sequence: u64,
        entry: &GraphEntry,
    ) -> Result<Option<Undo>, GraphError> {
        match entry {
            GraphEntry::CreateGraph { name } => {
                if graph_id.0 >= MAX_GRAPHS || self.graphs.contains_key(&graph_id) {
                    return Err(GraphError::GraphIdUnavailable(graph_id));
                }
                if self.names.contains_key(name) {
                    return Err(GraphError::GraphNameExists(name.clone()));
                }
                self.names.insert(name.clone(), graph_id);
                self.graphs
                    .insert(graph_id, Graph::new(graph_id, name.clone(), sequence));
                return Ok(Some(Undo::CreateGraph));
            }
            GraphEntry::VectorMutation { .. } => {
                // Isolation: vectors never change graph state, but the body
                // still addresses an existing graph.
                return if self.graphs.contains_key(&graph_id) {
                    Ok(None)
                } else {
                    Err(GraphError::GraphNotFound(graph_id))
                };
            }
            _ => {}
        }
        let graph = self
            .graphs
            .get_mut(&graph_id)
            .ok_or(GraphError::GraphNotFound(graph_id))?;
        let undo = match entry {
            GraphEntry::CreateGraph { .. } | GraphEntry::VectorMutation { .. } => unreachable!(),
            GraphEntry::PutSchema { schema } => {
                let old = std::mem::replace(&mut graph.schema, schema.clone());
                graph.rebuild_property_indexes();
                Undo::Schema(Box::new(old))
            }
            GraphEntry::DropGraph => {
                if !graph.nodes.is_empty() || !graph.edges.is_empty() {
                    return Err(GraphError::GraphNotEmpty(graph_id));
                }
                let graph = self.graphs.remove(&graph_id).expect("graph exists");
                self.names.remove(&graph.name);
                Undo::DropGraph(Box::new(graph))
            }
            GraphEntry::GraphHighWater {
                next_node_id,
                next_edge_id,
            } => {
                if *next_node_id < graph.next_node_id || *next_edge_id < graph.next_edge_id {
                    return Err(GraphError::HighWaterDecrease);
                }
                let undo = Undo::HighWater {
                    next_node_id: graph.next_node_id,
                    next_edge_id: graph.next_edge_id,
                };
                graph.next_node_id = *next_node_id;
                graph.next_edge_id = *next_edge_id;
                undo
            }
            GraphEntry::PutNode {
                node_id,
                key,
                labels,
                properties,
            } => {
                let node = GraphNode {
                    id: *node_id,
                    key: key.clone(),
                    labels: labels.clone(),
                    properties: properties.clone(),
                    revision: sequence,
                };
                if let Some(existing) = graph.nodes.get(node_id) {
                    if existing.key != *key {
                        return Err(GraphError::NodeKeyMismatch(*node_id));
                    }
                    let old = graph.remove_node(*node_id).expect("node exists");
                    graph.insert_node(node);
                    Undo::RestoreNode(old)
                } else {
                    if node_id.0 < graph.next_node_id {
                        return Err(GraphError::NodeIdUnavailable(*node_id));
                    }
                    if graph.node_keys.contains_key(key) {
                        return Err(GraphError::NodeKeyExists(key.clone()));
                    }
                    let previous_next = new_id(&mut graph.next_node_id, node_id.0)?;
                    graph.insert_node(node);
                    Undo::InsertNode {
                        id: *node_id,
                        previous_next,
                    }
                }
            }
            GraphEntry::PatchNode {
                node_id,
                add_labels,
                remove_labels,
                set,
                unset,
            } => {
                let old = graph
                    .nodes
                    .get(node_id)
                    .ok_or(GraphError::NodeNotFound(*node_id))?;
                let mut labels: BTreeSet<String> =
                    old.labels.difference(remove_labels).cloned().collect();
                labels.extend(add_labels.iter().cloned());
                if labels.len() > MAX_LABELS {
                    return Err(GraphError::TooManyLabels);
                }
                let node = GraphNode {
                    id: *node_id,
                    key: old.key.clone(),
                    labels,
                    properties: patched(old.properties.clone(), set, unset)?,
                    revision: sequence,
                };
                let old = graph.remove_node(*node_id).expect("node exists");
                graph.insert_node(node);
                Undo::RestoreNode(old)
            }
            GraphEntry::DeleteNode { node_id } => {
                if !graph.nodes.contains_key(node_id) {
                    return Err(GraphError::NodeNotFound(*node_id));
                }
                let incident = graph.incident_edges(*node_id);
                if incident > 0 {
                    return Err(GraphError::NodeHasEdges {
                        node: *node_id,
                        incident,
                    });
                }
                let attachments = graph.attachments.remove(node_id).unwrap_or_default();
                for (collection, record) in &attachments {
                    self.attachment_owners.remove(&(*collection, *record));
                }
                let node = graph.remove_node(*node_id).expect("node exists");
                Undo::DeleteNode { node, attachments }
            }
            GraphEntry::PutEdge {
                edge_id,
                source,
                destination,
                relationship,
                key,
                properties,
            } => {
                if edge_id.0 < graph.next_edge_id {
                    return Err(GraphError::EdgeIdUnavailable(*edge_id));
                }
                for endpoint in [source, destination] {
                    if !graph.nodes.contains_key(endpoint) {
                        return Err(GraphError::NodeNotFound(*endpoint));
                    }
                }
                if let Some(key) = key
                    && graph.edge_keys.contains_key(key)
                {
                    return Err(GraphError::EdgeKeyExists(key.clone()));
                }
                let previous_next = new_id(&mut graph.next_edge_id, edge_id.0)?;
                graph.insert_edge(GraphEdge {
                    id: *edge_id,
                    key: key.clone(),
                    source: *source,
                    relationship: relationship.clone(),
                    destination: *destination,
                    properties: properties.clone(),
                    revision: sequence,
                });
                Undo::InsertEdge {
                    id: *edge_id,
                    previous_next,
                }
            }
            GraphEntry::PatchEdge {
                edge_id,
                set,
                unset,
            } => {
                let edge = graph
                    .edges
                    .get_mut(edge_id)
                    .ok_or(GraphError::EdgeNotFound(*edge_id))?;
                let properties = patched(edge.properties.clone(), set, unset)?;
                let old = edge.clone();
                edge.properties = properties;
                edge.revision = sequence;
                Undo::RestoreEdge(old)
            }
            GraphEntry::DeleteEdge { edge_id } => Undo::DeleteEdge(
                graph
                    .remove_edge(*edge_id)
                    .ok_or(GraphError::EdgeNotFound(*edge_id))?,
            ),
            GraphEntry::Attach {
                node_id,
                collection,
                record_id,
            } => {
                if !graph.nodes.contains_key(node_id) {
                    return Err(GraphError::NodeNotFound(*node_id));
                }
                if graph
                    .attachments
                    .get(node_id)
                    .is_some_and(|map| map.contains_key(collection))
                {
                    return Err(GraphError::NodeAlreadyAttached {
                        node: *node_id,
                        collection: *collection,
                    });
                }
                if let Some((graph, node)) = self.attachment_owners.get(&(*collection, *record_id))
                {
                    return Err(GraphError::RecordAlreadyAttached {
                        collection: *collection,
                        record: *record_id,
                        graph: *graph,
                        node: *node,
                    });
                }
                graph
                    .attachments
                    .entry(*node_id)
                    .or_default()
                    .insert(*collection, *record_id);
                self.attachment_owners
                    .insert((*collection, *record_id), (graph_id, *node_id));
                Undo::Attach {
                    node: *node_id,
                    collection: *collection,
                }
            }
            GraphEntry::Detach {
                node_id,
                collection,
            } => {
                if !graph.nodes.contains_key(node_id) {
                    return Err(GraphError::NodeNotFound(*node_id));
                }
                let missing = GraphError::AttachmentNotFound {
                    node: *node_id,
                    collection: *collection,
                };
                let map = graph.attachments.get_mut(node_id).ok_or(missing.clone())?;
                let record = map.remove(collection).ok_or(missing)?;
                if map.is_empty() {
                    graph.attachments.remove(node_id);
                }
                self.attachment_owners.remove(&(*collection, record));
                Undo::Detach {
                    node: *node_id,
                    collection: *collection,
                    record,
                }
            }
        };
        Ok(Some(undo))
    }

    pub(super) fn rollback(&mut self, graph_id: GraphId, undo: Vec<Undo>) {
        for inverse in undo.into_iter().rev() {
            match inverse {
                Undo::CreateGraph => {
                    let graph = self.graphs.remove(&graph_id).expect("created graph");
                    self.names.remove(&graph.name);
                }
                Undo::DropGraph(graph) => {
                    self.names.insert(graph.name.clone(), graph_id);
                    self.graphs.insert(graph_id, *graph);
                }
                other => self.rollback_in_graph(graph_id, other),
            }
        }
    }

    fn rollback_in_graph(&mut self, graph_id: GraphId, inverse: Undo) {
        let graph = self.graphs.get_mut(&graph_id).expect("graph exists");
        match inverse {
            Undo::CreateGraph | Undo::DropGraph(_) => unreachable!(),
            Undo::HighWater {
                next_node_id,
                next_edge_id,
            } => {
                graph.next_node_id = next_node_id;
                graph.next_edge_id = next_edge_id;
            }
            Undo::InsertNode { id, previous_next } => {
                graph.remove_node(id);
                graph.next_node_id = previous_next;
            }
            Undo::RestoreNode(node) => {
                graph.remove_node(node.id);
                graph.insert_node(node);
            }
            Undo::DeleteNode { node, attachments } => {
                for (collection, record) in &attachments {
                    self.attachment_owners
                        .insert((*collection, *record), (graph_id, node.id));
                }
                if !attachments.is_empty() {
                    graph.attachments.insert(node.id, attachments);
                }
                graph.insert_node(node);
            }
            Undo::InsertEdge { id, previous_next } => {
                graph.remove_edge(id);
                graph.next_edge_id = previous_next;
            }
            Undo::RestoreEdge(edge) => {
                graph.edges.insert(edge.id, edge);
            }
            Undo::DeleteEdge(edge) => graph.insert_edge(edge),
            Undo::Attach { node, collection } => {
                let map = graph.attachments.get_mut(&node).expect("attachment");
                let record = map.remove(&collection).expect("attachment");
                if map.is_empty() {
                    graph.attachments.remove(&node);
                }
                self.attachment_owners.remove(&(collection, record));
            }
            Undo::Detach {
                node,
                collection,
                record,
            } => {
                graph
                    .attachments
                    .entry(node)
                    .or_default()
                    .insert(collection, record);
                self.attachment_owners
                    .insert((collection, record), (graph_id, node));
            }
            Undo::Schema(schema) => {
                graph.schema = *schema;
                graph.rebuild_property_indexes();
            }
        }
    }

    /// Checks INV-008 (no dangling edges or attachments), ID high-water marks
    /// and INV-009 (derived indexes equal a rebuild from authoritative data).
    pub fn check_invariants(&self) -> Result<(), GraphError> {
        let violation = GraphError::InvariantViolation;
        let mut names = BTreeMap::new();
        let mut owners = BTreeMap::new();
        for (id, graph) in &self.graphs {
            if graph.id != *id || id.0 >= MAX_GRAPHS {
                return Err(violation("graph id"));
            }
            names.insert(graph.name.clone(), *id);
            for (node_id, node) in &graph.nodes {
                if node.id != *node_id || node_id.0 >= graph.next_node_id {
                    return Err(violation("node id"));
                }
            }
            for (edge_id, edge) in &graph.edges {
                if edge.id != *edge_id || edge_id.0 >= graph.next_edge_id {
                    return Err(violation("edge id"));
                }
                if !graph.nodes.contains_key(&edge.source)
                    || !graph.nodes.contains_key(&edge.destination)
                {
                    return Err(violation("dangling edge"));
                }
            }
            for (node_id, map) in &graph.attachments {
                if !graph.nodes.contains_key(node_id) || map.is_empty() {
                    return Err(violation("dangling attachment"));
                }
                for (collection, record) in map {
                    if owners
                        .insert((*collection, *record), (*id, *node_id))
                        .is_some()
                    {
                        return Err(violation("record attached twice"));
                    }
                }
            }
            if graph.rebuilt() != *graph {
                return Err(violation("derived graph indexes differ from a rebuild"));
            }
        }
        if names != self.names {
            return Err(violation("graph name index"));
        }
        if owners != self.attachment_owners {
            return Err(violation("attachment owner index"));
        }
        Ok(())
    }

    /// Memory diagnostics (NFR-002, ITEM-021): consumes the state and drops
    /// its owned components one at a time, calling `released` right after
    /// each drop (see [`MemoryComponent`] for the order). An
    /// allocation-counting caller attributes the bytes freed since the
    /// previous call to the reported component. Nothing is cloned or
    /// estimated, and this method allocates nothing; `released` must not
    /// allocate or free either, or the attribution is off by that amount.
    pub fn release_components(self, mut released: impl FnMut(MemoryComponent)) {
        let Self {
            graphs,
            last_sequence: _,
            names,
            attachment_owners,
        } = self;
        drop(names);
        released(MemoryComponent::NameIndex);
        drop(attachment_owners);
        released(MemoryComponent::AttachmentOwners);
        for (_, graph) in graphs {
            // Advancing the consuming iterator frees the table nodes it left.
            released(MemoryComponent::GraphTable);
            graph.release_components(&mut released);
        }
        released(MemoryComponent::GraphTable);
    }
}

/// An owned heap component of a [`GraphState`], as reported by
/// [`GraphState::release_components`]: first `NameIndex` and
/// `AttachmentOwners`, then for each graph in ID order `GraphTable` followed
/// by `GraphName` through `LabelIndex` in declaration order, and a final
/// `GraphTable`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryComponent {
    /// Graph name → graph ID index.
    NameIndex,
    /// PD-012 `(collection, record)` → owning node index.
    AttachmentOwners,
    /// The table of live graphs, whose entries hold each graph's inline
    /// fields (counters and the container headers of the components below).
    GraphTable,
    /// One graph's name and declared schema.
    GraphName,
    /// Authoritative node records, including each node's key, labels and
    /// properties.
    NodeRecords,
    /// Authoritative edge records, including each edge's key, relationship
    /// type and properties.
    EdgeRecords,
    /// Node → collection → record attachments.
    Attachments,
    /// Node key → node ID index.
    NodeKeyIndex,
    /// Edge key → edge ID index.
    EdgeKeyIndex,
    /// Outgoing sorted `(interned relationship type, edge ID)` vectors.
    OutAdjacency,
    /// Incoming sorted adjacency vectors and their shared relationship names.
    InAdjacency,
    /// Label → node IDs index and declared property indexes.
    LabelIndex,
}

impl Graph {
    fn release_components(self, released: &mut impl FnMut(MemoryComponent)) {
        let Self {
            id: _,
            name,
            next_node_id: _,
            next_edge_id: _,
            graph_sequence: _,
            nodes,
            edges,
            attachments,
            node_keys,
            edge_keys,
            out_edges,
            in_edges,
            relationships,
            labels,
            schema,
            property_indexes,
        } = self;
        drop(name);
        drop(schema);
        released(MemoryComponent::GraphName);
        drop(nodes);
        released(MemoryComponent::NodeRecords);
        drop(edges);
        released(MemoryComponent::EdgeRecords);
        drop(attachments);
        released(MemoryComponent::Attachments);
        drop(node_keys);
        released(MemoryComponent::NodeKeyIndex);
        drop(edge_keys);
        released(MemoryComponent::EdgeKeyIndex);
        drop(out_edges);
        released(MemoryComponent::OutAdjacency);
        drop(in_edges);
        drop(relationships);
        released(MemoryComponent::InAdjacency);
        drop(labels);
        drop(property_indexes);
        released(MemoryComponent::LabelIndex);
    }
}

mod snapshot;

#[cfg(test)]
mod tests;
