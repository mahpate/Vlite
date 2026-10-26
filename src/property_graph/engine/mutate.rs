//! Key-addressed graph mutations (ITEM-015, REQ-008..REQ-013).
//!
//! A [`Planner`] turns caller mutations into graph entries for one batch.
//! Each mutation is resolved against the current state plus every earlier
//! mutation of the same batch (they are applied tentatively as they are
//! planned), so later mutations observe earlier ones. Dropping or finishing
//! the planner rolls every tentative change back: planning never changes
//! committed state. The caller then commits the returned body and applies it
//! with [`GraphState::apply`] (validate, append, apply: PAT-001).
//!
//! Record existence for attachments and vector-mutation contents are checked
//! by the adapter, which can inspect the tentative attachments through
//! [`Planner::state`] between mutations.

use super::codec::{GraphBody, GraphEntry};
use super::error::GraphError;
use super::limits::MAX_BATCH_ENTRIES;
use super::model::{
    EdgeId, GraphEdge, GraphId, NodeId, Precondition, Properties, PropertyValue, validate_edge_key,
    validate_node_key,
};
use super::schema::{GraphSchema, Multiplicity};
use super::state::{Graph, GraphState, Undo};
use std::collections::BTreeSet;

/// Addresses an edge by internal ID or by edge key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EdgeSelector {
    Id(EdgeId),
    Key(String),
}

/// One key-addressed mutation of the planner's graph.
#[derive(Clone, Debug, PartialEq)]
pub enum GraphMutation {
    /// Creates a node; fails `NodeKeyExists` when the key is live.
    CreateNode {
        key: String,
        labels: BTreeSet<String>,
        properties: Properties,
    },
    /// Creates a node or fully replaces its labels and properties.
    UpsertNode {
        key: String,
        labels: BTreeSet<String>,
        properties: Properties,
        expected: Precondition,
    },
    /// Adds/removes labels and sets/unsets properties of a live node.
    PatchNode {
        key: String,
        add_labels: BTreeSet<String>,
        remove_labels: BTreeSet<String>,
        set: Properties,
        unset: BTreeSet<String>,
        expected: Precondition,
    },
    /// Deletes a node without incident edges, dropping its attachments.
    DeleteNode {
        key: String,
        expected: Precondition,
    },
    /// Creates an edge. With a key that is live, an identical edge is
    /// returned unchanged (`created = false`) and a different one fails
    /// `EdgeKeyConflict` (REQ-009).
    CreateEdge {
        key: Option<String>,
        source: String,
        relationship: String,
        destination: String,
        properties: Properties,
    },
    PatchEdge {
        edge: EdgeSelector,
        set: Properties,
        unset: BTreeSet<String>,
        expected: Precondition,
    },
    DeleteEdge {
        edge: EdgeSelector,
        expected: Precondition,
    },
    /// Attaches a record; the adapter has checked that it is live without expiry.
    Attach {
        node: String,
        collection: u16,
        record: u64,
    },
    Detach {
        node: String,
        collection: u16,
    },
    /// Replaces the graph's declared schema; existing data must conform.
    PutSchema(GraphSchema),
    /// Creates the `relationship` edge between two nodes unless one exists.
    /// The type must be declared `UniqueTriple`; an existing edge with
    /// identical properties is returned (`created = false`) and one with
    /// different properties fails `EdgeKeyConflict`.
    EnsureEdge {
        source: String,
        relationship: String,
        destination: String,
        properties: Properties,
    },
}

/// The result of one planned mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MutationOutcome {
    /// The node written; `created` is false for replacements and patches.
    Node {
        id: NodeId,
        created: bool,
    },
    /// The edge written, or the existing identical keyed edge (`created = false`).
    Edge {
        id: EdgeId,
        created: bool,
    },
    NodeDeleted(NodeId),
    EdgeDeleted(EdgeId),
    Attached(NodeId),
    Detached {
        node: NodeId,
        record: u64,
    },
    /// The declared schema was replaced.
    Schema,
}

/// Checks a precondition against a current revision.
fn check(expected: Precondition, current: Option<u64>) -> Result<(), GraphError> {
    let matches = match expected {
        Precondition::Any => true,
        Precondition::Absent => current.is_none(),
        Precondition::Revision(revision) => current == Some(revision),
    };
    if matches {
        Ok(())
    } else {
        Err(GraphError::PreconditionFailed { current })
    }
}

fn same_value(a: &PropertyValue, b: &PropertyValue) -> bool {
    match (a, b) {
        (PropertyValue::Float(a), PropertyValue::Float(b)) => a.to_bits() == b.to_bits(),
        (
            PropertyValue::GeoPoint {
                latitude: a,
                longitude: b,
            },
            PropertyValue::GeoPoint {
                latitude: c,
                longitude: d,
            },
        ) => a.to_bits() == c.to_bits() && b.to_bits() == d.to_bits(),
        _ => a == b,
    }
}

/// Bit-exact property equality (`-0.0` differs from `0.0`), matching the
/// encoded bytes.
fn same_properties(a: &Properties, b: &Properties) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|((ka, va), (kb, vb))| ka == kb && same_value(va, vb))
}

/// Plans one batch for one graph. See the module documentation.
pub struct Planner<'a> {
    state: &'a mut GraphState,
    graph_id: GraphId,
    sequence: u64,
    entries: Vec<GraphEntry>,
    undo: Vec<Undo>,
}

impl GraphState {
    /// Starts planning a batch for `graph_id` that will commit at `sequence`.
    pub fn planner(&mut self, sequence: u64, graph_id: GraphId) -> Result<Planner<'_>, GraphError> {
        if sequence <= self.last_sequence() {
            return Err(GraphError::SequenceNotIncreasing {
                last: self.last_sequence(),
                sequence,
            });
        }
        if self.graph(graph_id).is_none() {
            return Err(GraphError::GraphNotFound(graph_id));
        }
        Ok(Planner {
            state: self,
            graph_id,
            sequence,
            entries: Vec::new(),
            undo: Vec::new(),
        })
    }
}

impl Drop for Planner<'_> {
    fn drop(&mut self) {
        let undo = std::mem::take(&mut self.undo);
        self.state.rollback(self.graph_id, undo);
    }
}

impl Planner<'_> {
    /// The state including every mutation planned so far.
    pub fn state(&self) -> &GraphState {
        self.state
    }

    /// The planned graph including every mutation planned so far.
    pub fn graph(&self) -> &Graph {
        self.state
            .graph(self.graph_id)
            .expect("the planned graph stays live: DropGraph is never planned")
    }

    /// Entries planned so far.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Validates and tentatively applies one entry. On error nothing changes.
    fn push(&mut self, entry: GraphEntry) -> Result<(), GraphError> {
        if self.entries.len() >= MAX_BATCH_ENTRIES {
            return Err(GraphError::BatchSize("more than 4096 entries"));
        }
        entry.validate()?;
        if let Some(undo) = self
            .state
            .apply_entry(self.graph_id, self.sequence, &entry)?
        {
            self.undo.push(undo);
        }
        self.entries.push(entry);
        Ok(())
    }

    /// Adds one vector-mutation entry (one `encode_batch_entries` entry for
    /// `collection`, already validated by the adapter).
    pub fn vector(&mut self, collection: u16, entry: Vec<u8>) -> Result<(), GraphError> {
        self.push(GraphEntry::VectorMutation { collection, entry })
    }

    fn node_id(&self, key: &str) -> Result<NodeId, GraphError> {
        self.graph()
            .node_by_key(key)
            .map(|node| node.id)
            .ok_or_else(|| GraphError::UnknownNodeKey(key.into()))
    }

    /// A live node by key after its precondition passed.
    fn checked_node(&self, key: &str, expected: Precondition) -> Result<NodeId, GraphError> {
        validate_node_key(key)?;
        let node = self.graph().node_by_key(key);
        check(expected, node.map(|node| node.revision))?;
        node.map(|node| node.id)
            .ok_or_else(|| GraphError::UnknownNodeKey(key.into()))
    }

    /// A live edge after its precondition passed.
    fn checked_edge(
        &self,
        edge: &EdgeSelector,
        expected: Precondition,
    ) -> Result<EdgeId, GraphError> {
        let found: Option<&GraphEdge> = match edge {
            EdgeSelector::Id(id) => self.graph().edge(*id),
            EdgeSelector::Key(key) => {
                validate_edge_key(key)?;
                self.graph().edge_by_key(key)
            }
        };
        check(expected, found.map(|edge| edge.revision))?;
        match (found, edge) {
            (Some(edge), _) => Ok(edge.id),
            (None, EdgeSelector::Id(id)) => Err(GraphError::EdgeNotFound(*id)),
            (None, EdgeSelector::Key(key)) => Err(GraphError::UnknownEdgeKey(key.clone())),
        }
    }

    /// Plans one mutation. On error nothing planned by this call remains;
    /// earlier mutations stay planned.
    pub fn mutate(&mut self, mutation: &GraphMutation) -> Result<MutationOutcome, GraphError> {
        match mutation {
            GraphMutation::CreateNode {
                key,
                labels,
                properties,
            } => {
                validate_node_key(key)?;
                if self.graph().node_by_key(key).is_some() {
                    return Err(GraphError::NodeKeyExists(key.clone()));
                }
                let id = NodeId(self.graph().next_node_id());
                self.push(GraphEntry::PutNode {
                    node_id: id,
                    key: key.clone(),
                    labels: labels.clone(),
                    properties: properties.clone(),
                })?;
                Ok(MutationOutcome::Node { id, created: true })
            }
            GraphMutation::UpsertNode {
                key,
                labels,
                properties,
                expected,
            } => {
                validate_node_key(key)?;
                let current = self.graph().node_by_key(key);
                check(*expected, current.map(|node| node.revision))?;
                let (id, created) = match current {
                    Some(node) => (node.id, false),
                    None => (NodeId(self.graph().next_node_id()), true),
                };
                self.push(GraphEntry::PutNode {
                    node_id: id,
                    key: key.clone(),
                    labels: labels.clone(),
                    properties: properties.clone(),
                })?;
                Ok(MutationOutcome::Node { id, created })
            }
            GraphMutation::PatchNode {
                key,
                add_labels,
                remove_labels,
                set,
                unset,
                expected,
            } => {
                let id = self.checked_node(key, *expected)?;
                self.push(GraphEntry::PatchNode {
                    node_id: id,
                    add_labels: add_labels.clone(),
                    remove_labels: remove_labels.clone(),
                    set: set.clone(),
                    unset: unset.clone(),
                })?;
                Ok(MutationOutcome::Node { id, created: false })
            }
            GraphMutation::DeleteNode { key, expected } => {
                let id = self.checked_node(key, *expected)?;
                self.push(GraphEntry::DeleteNode { node_id: id })?;
                Ok(MutationOutcome::NodeDeleted(id))
            }
            GraphMutation::CreateEdge {
                key,
                source,
                relationship,
                destination,
                properties,
            } => {
                let mut entry = GraphEntry::PutEdge {
                    edge_id: EdgeId(self.graph().next_edge_id()),
                    source: NodeId(0),
                    destination: NodeId(0),
                    relationship: relationship.clone(),
                    key: key.clone(),
                    properties: properties.clone(),
                };
                // Input rules first, so an invalid retry never reads as a conflict.
                entry.validate()?;
                validate_node_key(source)?;
                validate_node_key(destination)?;
                let source = self.node_id(source)?;
                let destination = self.node_id(destination)?;
                if let Some(key) = key
                    && let Some(existing) = self.graph().edge_by_key(key)
                {
                    let identical = existing.source == source
                        && existing.destination == destination
                        && existing.relationship == *relationship
                        && same_properties(&existing.properties, properties);
                    return if identical {
                        Ok(MutationOutcome::Edge {
                            id: existing.id,
                            created: false,
                        })
                    } else {
                        Err(GraphError::EdgeKeyConflict(key.clone()))
                    };
                }
                if let GraphEntry::PutEdge {
                    edge_id,
                    source: s,
                    destination: d,
                    ..
                } = &mut entry
                {
                    *s = source;
                    *d = destination;
                    let id = *edge_id;
                    self.push(entry)?;
                    return Ok(MutationOutcome::Edge { id, created: true });
                }
                unreachable!("entry is a PutEdge")
            }
            GraphMutation::PutSchema(schema) => {
                self.push(GraphEntry::PutSchema {
                    schema: schema.clone(),
                })?;
                Ok(MutationOutcome::Schema)
            }
            GraphMutation::EnsureEdge {
                source,
                relationship,
                destination,
                properties,
            } => {
                let id = EdgeId(self.graph().next_edge_id());
                let mut entry = GraphEntry::PutEdge {
                    edge_id: id,
                    source: NodeId(0),
                    destination: NodeId(0),
                    relationship: relationship.clone(),
                    key: None,
                    properties: properties.clone(),
                };
                entry.validate()?;
                validate_node_key(source)?;
                validate_node_key(destination)?;
                let unique = self
                    .graph()
                    .schema()
                    .relationships
                    .get(relationship)
                    .is_some_and(|rule| rule.multiplicity == Multiplicity::UniqueTriple);
                if !unique {
                    return Err(GraphError::InvalidSchema(
                        "ensure_edge requires a declared UniqueTriple relationship type",
                    ));
                }
                let source = self.node_id(source)?;
                let destination = self.node_id(destination)?;
                let graph = self.graph();
                if let Some(existing) = graph
                    .edges_between(source, relationship, destination)
                    .next()
                    .and_then(|id| graph.edge(id))
                {
                    return if same_properties(&existing.properties, properties) {
                        Ok(MutationOutcome::Edge {
                            id: existing.id,
                            created: false,
                        })
                    } else {
                        Err(GraphError::EdgeKeyConflict(format!(
                            "{}-{relationship}->{}",
                            source.0, destination.0
                        )))
                    };
                }
                if let GraphEntry::PutEdge {
                    source: s,
                    destination: d,
                    ..
                } = &mut entry
                {
                    *s = source;
                    *d = destination;
                }
                self.push(entry)?;
                Ok(MutationOutcome::Edge { id, created: true })
            }
            GraphMutation::PatchEdge {
                edge,
                set,
                unset,
                expected,
            } => {
                let id = self.checked_edge(edge, *expected)?;
                self.push(GraphEntry::PatchEdge {
                    edge_id: id,
                    set: set.clone(),
                    unset: unset.clone(),
                })?;
                Ok(MutationOutcome::Edge { id, created: false })
            }
            GraphMutation::DeleteEdge { edge, expected } => {
                let id = self.checked_edge(edge, *expected)?;
                self.push(GraphEntry::DeleteEdge { edge_id: id })?;
                Ok(MutationOutcome::EdgeDeleted(id))
            }
            GraphMutation::Attach {
                node,
                collection,
                record,
            } => {
                validate_node_key(node)?;
                let id = self.node_id(node)?;
                self.push(GraphEntry::Attach {
                    node_id: id,
                    collection: *collection,
                    record_id: *record,
                })?;
                Ok(MutationOutcome::Attached(id))
            }
            GraphMutation::Detach { node, collection } => {
                validate_node_key(node)?;
                let id = self.node_id(node)?;
                let record = self
                    .graph()
                    .attachments(id)
                    .find(|(c, _)| c == collection)
                    .map(|(_, record)| record);
                self.push(GraphEntry::Detach {
                    node_id: id,
                    collection: *collection,
                })?;
                Ok(MutationOutcome::Detached {
                    node: id,
                    record: record.expect("detach succeeded, so the attachment existed"),
                })
            }
        }
    }

    /// The planned body. Tentative changes are rolled back; commit the body
    /// and then apply it with [`GraphState::apply`]. An empty body (only
    /// idempotent edge retries) must not be committed.
    pub fn finish(mut self) -> GraphBody {
        GraphBody {
            graph_id: self.graph_id,
            entries: std::mem::take(&mut self.entries),
        }
    }
}

#[cfg(test)]
mod tests;
