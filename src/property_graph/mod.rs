//! Named property graphs stored in graph-enabled VLite files.
//!
//! A graph-enabled file is a format-v5 file whose header carries `FLAG_GRAPH`
//! (DO-001). Graph changes are op-12 frames (DO-003) written through the same
//! writer, checksums, encryption, sequence and locking as vector commits, and
//! replayed atomically on open. Releases before v0.5.0 (and v0.5.0 builds
//! without the then-optional `graph` feature) reject these files with `Error::UnsupportedFeature(4)`.
//!
//! Status (EPIC-004 mutations): graphs can be created, dropped and listed;
//! [`GraphMut`] writes nodes, edges, attachments and attached-collection
//! vector mutations as one atomic [`GraphOp`] batch per frame
//! (validate, append, apply). Ordinary vector writes keep attached records
//! intact (delete and expiry are refused with `GraphAttachedRecord` /
//! `GraphAttachedRecordExpiry`). [`GraphRef`] runs bounded reads (EPIC-005):
//! node and edge gets, paged `find_nodes` and `neighbors`, breadth-first
//! traversal, canonical shortest paths and `explain` (see the read types
//! below).
//!
//! Maintenance (EPIC-006): inspect, verify, backup, restore, compact, drill,
//! reencrypt, point-in-time restore, log export and replica followers carry
//! graph state; compaction keeps graph IDs and records each graph's
//! high-water marks so node and edge IDs are never reused. [`enable_graph`]
//! and [`strip_graph`] (also re-exported from `maintenance`) convert files by
//! staged copy-out, and [`inspect_graph`] and [`verify_graph`] add graph
//! counts. Since v0.6.0 `maintenance::upgrade` copies graph files like any
//! latest-format file, and change feeds report graph commits as
//! `ChangeKind::GraphCreated`, `GraphDropped` and `GraphBatch` events
//! (see [`crate::changes::GraphChange`]).
//!
//! Schemas (EPIC-008): [`GraphMut::put_schema`] declares optional label and
//! relationship rules ([`GraphSchema`]) that every later write must satisfy
//! (`GraphSchemaViolation`); graphs without a schema behave as before.
//! Declared exact and range property indexes serve `find_nodes`
//! ([`IndexChoice::PropertyExactIndex`], [`IndexChoice::PropertyRangeIndex`])
//! with the same results as a scan, [`GraphMut::ensure_edge`] writes
//! `UniqueTriple` relationships idempotently, and
//! [`GraphRef::observed_schema`] summarizes declared and observed shapes
//! within a scan budget.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::{
    CreateOptions, Database, Error, Expected, FormatVersion, Metadata, Mutation, Result, format,
};

// The engine source is shared with the workspace-only `vlite-graph` harness
// crate and compiled whole, so parts (for example harness-only accessors) are
// unused in this build.
#[allow(dead_code, unused_imports)]
pub(crate) mod engine;

// Some conversions (for example `Precondition` → `Expected`) are only used
// by tests.
mod change;
#[allow(dead_code)]
mod convert;
pub(crate) mod feed;
pub(crate) mod frame;
mod maintain;
mod read;
mod retrieve;

pub use change::GraphChange;
pub use engine::schema::{
    GraphSchema, LabelDefinition, Multiplicity, ObservedEndpoint, ObservedLabel, ObservedProperty,
    ObservedRelationship, ObservedSchema, PropertyDefinition, PropertyIndexKind, PropertyType,
    RelationshipDefinition, SchemaOrigin,
};
pub use maintain::{
    GraphCounts, GraphInspection, GraphVerification, StripGraphReport, enable_graph, inspect_graph,
    strip_graph, verify_graph,
};
pub use read::{
    Direction, EdgeView, Explanation, FindNodes, GraphLimits, GraphOutput, GraphPath, GraphQuery,
    GraphQueryError, GraphQueryErrorKind, GraphRead, GraphReadStats, GraphTextError,
    GraphTextQuery, IndexChoice, Neighborhood, Neighbors, NodeFilter, NodeSelector, NodeView,
    PathOutcome, PropertyProjection, ReadOptions, ShortestPath, Traversal, Traverse, TraversedNode,
    TruncationReason, parse_graph_query,
};
pub use retrieve::{
    Evidence, EvidenceSpec, Expansion, Pivot, PivotHit, Retrieval, Retrieve, RetrieveStats,
    UnmappedPivots,
};

#[cfg(test)]
mod tests;

use convert::{graph_error, precondition, properties_from_metadata};
use engine::codec::{BODY_HEADER_BYTES, ENTRY_HEADER_BYTES};
use engine::limits::{DEFAULT_OBSERVED_SCHEMA_BUDGET, MAX_BATCH_BYTES, MAX_BATCH_ENTRIES};
use engine::{
    EdgeId, EdgeSelector, Graph, GraphBody, GraphEntry, GraphId, GraphMutation, GraphState,
    MutationOutcome,
};

/// One live named graph.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct GraphInfo {
    pub name: String,
    pub node_count: usize,
    pub edge_count: usize,
}

impl GraphInfo {
    fn of(graph: &Graph) -> Self {
        Self {
            name: graph.name().into(),
            node_count: graph.node_count(),
            edge_count: graph.edge_count(),
        }
    }
}

/// Read handle to one live named graph, from [`Database::graph`].
///
/// It borrows the database, so every read through it observes the same
/// committed state, identified by [`GraphRef::view_sequence`].
pub struct GraphRef<'db> {
    db: &'db Database,
    graph: &'db Graph,
    /// The database's commit sequence when the handle was obtained; for a
    /// read-only database, the open-time sequence.
    view_sequence: u64,
}

impl GraphRef<'_> {
    pub fn name(&self) -> &str {
        self.graph.name()
    }

    pub fn info(&self) -> GraphInfo {
        GraphInfo::of(self.graph)
    }

    /// The declared schema (REQ-030); empty when none was declared.
    pub fn schema(&self) -> GraphSchema {
        self.graph.schema().clone()
    }

    /// Declared and observed labels, relationship types, endpoints and
    /// properties (REQ-032). At most `budget` nodes and edges are scanned
    /// (PD-011: default 100,000, ceiling 1,000,000); `complete` is false
    /// when the budget stopped the scan.
    pub fn observed_schema(&self, budget: Option<u64>) -> Result<ObservedSchema> {
        self.graph
            .observed_schema(budget.unwrap_or(DEFAULT_OBSERVED_SCHEMA_BUDGET))
            .map_err(graph_error)
    }
}

/// Write handle to one live named graph, from [`Database::graph_mut`]. It
/// borrows the database exclusively.
pub struct GraphMut<'db> {
    db: &'db mut Database,
    id: GraphId,
}

/// Addresses an edge by internal ID or by edge key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EdgeRef {
    Id(u64),
    Key(String),
}

/// Changes to a node's labels and properties. Labels in both `add_labels`
/// and `remove_labels`, or fields in both `set` and `unset`, are rejected.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NodePatch {
    pub add_labels: Vec<String>,
    pub remove_labels: Vec<String>,
    pub set: Metadata,
    pub unset: Vec<String>,
}

/// One operation of an atomic graph batch ([`GraphMut::write_batch`]).
///
/// Nodes are addressed by key. Operations apply in order and observe earlier
/// operations of the same batch; the whole batch commits in one frame or not
/// at all. Labels and unset field lists are sets (duplicates are ignored).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum GraphOp {
    /// Creates a node; `GraphKeyExists` when the key is live.
    CreateNode {
        key: String,
        labels: Vec<String>,
        properties: Metadata,
    },
    /// Creates a node or replaces all of its labels and properties
    /// (attachments and incident edges are kept).
    UpsertNode {
        key: String,
        labels: Vec<String>,
        properties: Metadata,
        expected: Expected,
    },
    PatchNode {
        key: String,
        patch: NodePatch,
        expected: Expected,
    },
    /// Deletes a node without incident edges (`GraphNodeHasEdges`
    /// otherwise); its attachments are removed, the records are kept.
    DeleteNode {
        key: String,
        expected: Expected,
    },
    /// Creates a directed edge between two live nodes. With a key that is
    /// already live, an identical edge (same endpoints, type and
    /// bit-identical properties) is returned unchanged with
    /// `created = false`; anything else is `GraphKeyConflict`. Keyless
    /// edges always create a new (possibly parallel) edge.
    CreateEdge {
        key: Option<String>,
        source: String,
        relationship: String,
        destination: String,
        properties: Metadata,
    },
    PatchEdge {
        edge: EdgeRef,
        set: Metadata,
        unset: Vec<String>,
        expected: Expected,
    },
    DeleteEdge {
        edge: EdgeRef,
        expected: Expected,
    },
    /// Attaches a live record without expiry of `collection` to a node. A
    /// node has at most one record per collection and a record at most one
    /// node.
    Attach {
        node: String,
        collection: String,
        record: u64,
    },
    Detach {
        node: String,
        collection: String,
    },
    /// A vector mutation committed in the same frame. Deleting a record
    /// that is attached at that point of the batch is `GraphAttachedRecord`.
    Vector {
        collection: String,
        mutation: Mutation,
    },
    /// Replaces the graph's declared schema (REQ-030); an empty schema
    /// removes it. Existing data must conform (`GraphSchemaViolation`).
    PutSchema {
        schema: GraphSchema,
    },
    /// Creates the `relationship` edge from `source` to `destination`
    /// unless one exists (REQ-034). The type must be declared
    /// [`Multiplicity::UniqueTriple`] (`InvalidQuery` otherwise). An
    /// existing edge with bit-identical properties is returned with
    /// `created = false`; different properties are `GraphKeyConflict`.
    EnsureEdge {
        source: String,
        relationship: String,
        destination: String,
        properties: Metadata,
    },
}

/// A node or edge written by a batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct GraphWrite {
    /// Internal node or edge ID (never reused within the graph).
    pub id: u64,
    /// True when the operation created the node or edge.
    pub created: bool,
    /// The node's or edge's revision after the batch, usable as
    /// `Expected::Revision` (the batch's commit sequence, or the existing
    /// revision for an idempotent edge retry).
    pub revision: u64,
}

/// The result of one [`GraphOp`], in batch order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum GraphOpOutcome {
    Node(GraphWrite),
    Edge(GraphWrite),
    NodeDeleted {
        id: u64,
    },
    EdgeDeleted {
        id: u64,
    },
    Attached {
        node: u64,
    },
    Detached {
        node: u64,
        record: u64,
    },
    Vector,
    /// The declared schema was replaced.
    Schema,
}

/// The result of an atomic graph batch.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct GraphBatchResult {
    /// Commit sequence of the batch's frame; `None` when nothing was written
    /// (an empty batch or only idempotent edge retries).
    pub revision: Option<u64>,
    pub outcomes: Vec<GraphOpOutcome>,
}

impl GraphMut<'_> {
    fn graph(&self) -> &Graph {
        self.db
            .graph
            .as_ref()
            .and_then(|state| state.graph(self.id))
            .expect("the exclusive borrow keeps the graph live")
    }

    pub fn name(&self) -> &str {
        self.graph().name()
    }

    pub fn info(&self) -> GraphInfo {
        GraphInfo::of(self.graph())
    }

    /// Validates every operation, appends one op-12 frame, syncs, then
    /// applies (PAT-001). On any error nothing is written. At most 4096
    /// operations (`BatchTooLarge`); the encoded frame body is limited to
    /// 16 MiB.
    pub fn write_batch(&mut self, ops: &[GraphOp]) -> Result<GraphBatchResult> {
        self.db.write_graph_batch(self.id, ops)
    }

    fn single(&mut self, op: GraphOp) -> Result<(GraphOpOutcome, Option<u64>)> {
        let result = self.write_batch(std::slice::from_ref(&op))?;
        Ok((result.outcomes[0], result.revision))
    }

    fn written(&mut self, op: GraphOp) -> Result<GraphWrite> {
        match self.single(op)?.0 {
            GraphOpOutcome::Node(write) | GraphOpOutcome::Edge(write) => Ok(write),
            _ => unreachable!("node and edge writes report a GraphWrite"),
        }
    }

    fn revision(&mut self, op: GraphOp) -> Result<u64> {
        Ok(self
            .single(op)?
            .1
            .expect("deletes and attachments always write"))
    }

    pub fn create_node(
        &mut self,
        key: &str,
        labels: &[&str],
        properties: &Metadata,
    ) -> Result<GraphWrite> {
        self.written(GraphOp::CreateNode {
            key: key.into(),
            labels: strings(labels),
            properties: properties.clone(),
        })
    }

    pub fn upsert_node(
        &mut self,
        key: &str,
        labels: &[&str],
        properties: &Metadata,
        expected: Expected,
    ) -> Result<GraphWrite> {
        self.written(GraphOp::UpsertNode {
            key: key.into(),
            labels: strings(labels),
            properties: properties.clone(),
            expected,
        })
    }

    pub fn patch_node(
        &mut self,
        key: &str,
        patch: &NodePatch,
        expected: Expected,
    ) -> Result<GraphWrite> {
        self.written(GraphOp::PatchNode {
            key: key.into(),
            patch: patch.clone(),
            expected,
        })
    }

    /// Returns the commit revision.
    pub fn delete_node(&mut self, key: &str, expected: Expected) -> Result<u64> {
        self.revision(GraphOp::DeleteNode {
            key: key.into(),
            expected,
        })
    }

    pub fn create_edge(
        &mut self,
        key: Option<&str>,
        source: &str,
        relationship: &str,
        destination: &str,
        properties: &Metadata,
    ) -> Result<GraphWrite> {
        self.written(GraphOp::CreateEdge {
            key: key.map(str::to_owned),
            source: source.into(),
            relationship: relationship.into(),
            destination: destination.into(),
            properties: properties.clone(),
        })
    }

    pub fn patch_edge(
        &mut self,
        edge: EdgeRef,
        set: &Metadata,
        unset: &[&str],
        expected: Expected,
    ) -> Result<GraphWrite> {
        self.written(GraphOp::PatchEdge {
            edge,
            set: set.clone(),
            unset: strings(unset),
            expected,
        })
    }

    /// Returns the commit revision.
    pub fn delete_edge(&mut self, edge: EdgeRef, expected: Expected) -> Result<u64> {
        self.revision(GraphOp::DeleteEdge { edge, expected })
    }

    /// Returns the commit revision.
    pub fn attach(&mut self, node: &str, collection: &str, record: u64) -> Result<u64> {
        self.revision(GraphOp::Attach {
            node: node.into(),
            collection: collection.into(),
            record,
        })
    }

    /// Returns the ID of the detached record.
    pub fn detach(&mut self, node: &str, collection: &str) -> Result<u64> {
        match self
            .single(GraphOp::Detach {
                node: node.into(),
                collection: collection.into(),
            })?
            .0
        {
            GraphOpOutcome::Detached { record, .. } => Ok(record),
            _ => unreachable!("detach reports the detached record"),
        }
    }

    /// Replaces the declared schema (see [`GraphOp::PutSchema`]); returns
    /// the commit revision.
    pub fn put_schema(&mut self, schema: &GraphSchema) -> Result<u64> {
        self.revision(GraphOp::PutSchema {
            schema: schema.clone(),
        })
    }

    /// Creates the edge unless it exists (see [`GraphOp::EnsureEdge`]).
    pub fn ensure_edge(
        &mut self,
        source: &str,
        relationship: &str,
        destination: &str,
        properties: &Metadata,
    ) -> Result<GraphWrite> {
        self.written(GraphOp::EnsureEdge {
            source: source.into(),
            relationship: relationship.into(),
            destination: destination.into(),
            properties: properties.clone(),
        })
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn set(values: &[String]) -> BTreeSet<String> {
    values.iter().cloned().collect()
}

/// A [`GraphOp`] with names resolved and inputs converted.
enum Planned {
    Graph(GraphMutation),
    Vector(u16, Mutation),
}

/// Op-12 prefix rule: no record ID, 1..=4096 entries, and a body that fits
/// the entry headers and the 16 MiB batch limit.
pub(crate) fn valid_frame_prefix(id: u64, count: usize, payload_len: usize) -> bool {
    id == 0
        && (1..=MAX_BATCH_ENTRIES).contains(&count)
        && payload_len <= MAX_BATCH_BYTES
        && payload_len >= BODY_HEADER_BYTES + count * ENTRY_HEADER_BYTES
}

impl Database {
    /// Creates a graph-enabled database: format V5 with the `FLAG_GRAPH`
    /// header bit, combinable with encryption and commit timestamps. An
    /// explicit V3/V4 `format` returns `GraphsRequireV5`. Graph-unaware
    /// readers, including pre-0.5.1 builds without the `graph` feature, reject the
    /// file with `UnsupportedFeature(4)`. Ordinary files never become
    /// graph-enabled.
    pub fn create_with_graph(
        path: impl AsRef<Path>,
        dimensions: usize,
        options: CreateOptions,
    ) -> Result<Self> {
        if matches!(options.format, Some(format) if format != FormatVersion::V5) {
            return Err(Error::GraphsRequireV5);
        }
        Self::create_file(path.as_ref(), dimensions, options, format::FLAG_GRAPH)
    }

    /// Whether this file is graph-enabled (`FLAG_GRAPH`).
    pub fn is_graph_enabled(&self) -> bool {
        self.graph.is_some()
    }

    fn graphs(&self) -> Result<&GraphState> {
        self.ensure_healthy()?;
        self.graph.as_ref().ok_or(Error::GraphsNotEnabled)
    }

    /// Durably creates an empty named graph. Names follow collection rules,
    /// `^[a-z][a-z0-9_-]{0,62}$`. At most 64 graphs are live at once; the
    /// internal ID of a dropped graph may be reused.
    pub fn create_graph(&mut self, name: &str) -> Result<GraphInfo> {
        let state = self.graphs()?;
        engine::model::validate_graph_name(name).map_err(graph_error)?;
        if state.graph_by_name(name).is_some() {
            return Err(Error::GraphExists);
        }
        let graph_id = state.free_graph_id().ok_or(Error::TooManyGraphs)?;
        self.commit_graph(
            &GraphBody {
                graph_id,
                entries: vec![GraphEntry::CreateGraph { name: name.into() }],
            },
            &[],
        )?;
        Ok(GraphInfo {
            name: name.into(),
            node_count: 0,
            edge_count: 0,
        })
    }

    /// Durably drops an empty named graph (`GraphNotEmpty` otherwise).
    pub fn drop_graph(&mut self, name: &str) -> Result<()> {
        let state = self.graphs()?;
        engine::model::validate_graph_name(name).map_err(graph_error)?;
        let graph_id = state.graph_by_name(name).ok_or(Error::GraphNotFound)?.id();
        self.commit_graph(
            &GraphBody {
                graph_id,
                entries: vec![GraphEntry::DropGraph],
            },
            &[],
        )
        .map(drop)
    }

    /// Live graphs in name order.
    pub fn list_graphs(&self) -> Result<Vec<GraphInfo>> {
        let mut graphs: Vec<GraphInfo> = self.graphs()?.graphs().map(GraphInfo::of).collect();
        graphs.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(graphs)
    }

    /// Read handle to a live graph (`GraphNotFound` otherwise).
    pub fn graph(&self, name: &str) -> Result<GraphRef<'_>> {
        let state = self.graphs()?;
        engine::model::validate_graph_name(name).map_err(graph_error)?;
        let graph = state.graph_by_name(name).ok_or(Error::GraphNotFound)?;
        Ok(GraphRef {
            db: self,
            graph,
            view_sequence: self.sequence,
        })
    }

    /// Write handle to a live graph. Read-only handles return `ReadOnly`.
    pub fn graph_mut(&mut self, name: &str) -> Result<GraphMut<'_>> {
        self.graphs()?;
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        let id = self.graph(name)?.graph.id();
        Ok(GraphMut { db: self, id })
    }

    /// Validate, append one op-12 frame, then apply (PAT-001). `vectors` are
    /// the body's vector mutations in body order; they are applied to their
    /// collections after the graph state. Returns the commit sequence.
    fn commit_graph(&mut self, body: &GraphBody, vectors: &[(u16, Mutation)]) -> Result<u64> {
        let sequence = self.next_sequence()?;
        let state = self.graph.as_mut().ok_or(Error::GraphsNotEnabled)?;
        state.validate(sequence, body).map_err(graph_error)?;
        let encoded = engine::encode_body(body).map_err(graph_error)?;
        let frame = format::graph_frame(sequence, body.entries.len() as u32, &encoded);
        self.commit_frame(sequence, &frame)?;
        if let Some(state) = self.graph.as_mut()
            && state.apply(sequence, body).is_err()
        {
            // Unreachable after a successful validation; never diverge silently.
            self.poisoned = true;
            return Err(Error::Poisoned);
        }
        for (collection, mutation) in vectors {
            self.apply_batch(*collection, std::slice::from_ref(mutation), sequence);
        }
        Ok(sequence)
    }

    /// Resolves names and converts and validates the inputs of one operation.
    fn plan_input(&self, op: &GraphOp) -> Result<Planned> {
        let mutation = match op {
            GraphOp::CreateNode {
                key,
                labels,
                properties,
            } => GraphMutation::CreateNode {
                key: key.clone(),
                labels: set(labels),
                properties: properties_from_metadata(properties)?,
            },
            GraphOp::UpsertNode {
                key,
                labels,
                properties,
                expected,
            } => GraphMutation::UpsertNode {
                key: key.clone(),
                labels: set(labels),
                properties: properties_from_metadata(properties)?,
                expected: precondition(*expected),
            },
            GraphOp::PatchNode {
                key,
                patch,
                expected,
            } => GraphMutation::PatchNode {
                key: key.clone(),
                add_labels: set(&patch.add_labels),
                remove_labels: set(&patch.remove_labels),
                set: properties_from_metadata(&patch.set)?,
                unset: set(&patch.unset),
                expected: precondition(*expected),
            },
            GraphOp::DeleteNode { key, expected } => GraphMutation::DeleteNode {
                key: key.clone(),
                expected: precondition(*expected),
            },
            GraphOp::CreateEdge {
                key,
                source,
                relationship,
                destination,
                properties,
            } => GraphMutation::CreateEdge {
                key: key.clone(),
                source: source.clone(),
                relationship: relationship.clone(),
                destination: destination.clone(),
                properties: properties_from_metadata(properties)?,
            },
            GraphOp::PatchEdge {
                edge,
                set: values,
                unset,
                expected,
            } => GraphMutation::PatchEdge {
                edge: selector(edge),
                set: properties_from_metadata(values)?,
                unset: set(unset),
                expected: precondition(*expected),
            },
            GraphOp::DeleteEdge { edge, expected } => GraphMutation::DeleteEdge {
                edge: selector(edge),
                expected: precondition(*expected),
            },
            GraphOp::Attach {
                node,
                collection,
                record,
            } => GraphMutation::Attach {
                node: node.clone(),
                collection: self.resolve(collection)?,
                record: *record,
            },
            GraphOp::Detach { node, collection } => GraphMutation::Detach {
                node: node.clone(),
                collection: self.resolve(collection)?,
            },
            GraphOp::Vector {
                collection,
                mutation,
            } => {
                let collection = self.resolve(collection)?;
                self.validate_batch(collection, std::slice::from_ref(mutation))?;
                return Ok(Planned::Vector(collection, mutation.clone()));
            }
            GraphOp::PutSchema { schema } => GraphMutation::PutSchema(schema.clone()),
            GraphOp::EnsureEdge {
                source,
                relationship,
                destination,
                properties,
            } => GraphMutation::EnsureEdge {
                source: source.clone(),
                relationship: relationship.clone(),
                destination: destination.clone(),
                properties: properties_from_metadata(properties)?,
            },
        };
        Ok(Planned::Graph(mutation))
    }

    /// Plans `ops` against the current state, then commits them as one
    /// op-12 frame (ITEM-015, PAT-001). See [`GraphMut::write_batch`].
    fn write_graph_batch(
        &mut self,
        graph_id: GraphId,
        ops: &[GraphOp],
    ) -> Result<GraphBatchResult> {
        self.graphs()?;
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        if ops.len() > MAX_BATCH_ENTRIES {
            return Err(Error::BatchTooLarge);
        }
        let mut outcomes = Vec::with_capacity(ops.len());
        if ops.is_empty() {
            return Ok(GraphBatchResult {
                revision: None,
                outcomes,
            });
        }
        let planned = ops
            .iter()
            .map(|op| self.plan_input(op))
            .collect::<Result<Vec<_>>>()?;
        let sequence = self.next_sequence()?;
        let collections = &self.collections;
        let state = self.graph.as_mut().ok_or(Error::GraphsNotEnabled)?;
        let mut planner = state.planner(sequence, graph_id).map_err(graph_error)?;
        // Record liveness after the batch's earlier vector mutations.
        let mut live: BTreeMap<(u16, u64), bool> = BTreeMap::new();
        let mut vectors = Vec::new();
        for item in planned {
            let outcome = match item {
                Planned::Vector(collection, mutation) => {
                    let id = frame::mutation_id(&mutation);
                    let deleting = matches!(mutation, Mutation::Delete { .. });
                    if deleting && planner.state().attachment_owner(collection, id).is_some() {
                        return Err(Error::GraphAttachedRecord);
                    }
                    let GraphEntry::VectorMutation { entry, .. } =
                        frame::vector_entry(collection, &mutation)
                    else {
                        unreachable!("vector_entry returns a vector entry")
                    };
                    planner.vector(collection, entry).map_err(graph_error)?;
                    live.insert((collection, id), !deleting);
                    vectors.push((collection, mutation));
                    GraphOpOutcome::Vector
                }
                Planned::Graph(mutation) => {
                    if let GraphMutation::Attach {
                        collection, record, ..
                    } = &mutation
                    {
                        let attachable =
                            live.get(&(*collection, *record))
                                .copied()
                                .unwrap_or_else(|| {
                                    collections.get(collection).is_some_and(|target| {
                                        target.revisions.contains_key(record)
                                            && !target.expiries.contains_key(record)
                                    })
                                });
                        if !attachable {
                            return Err(Error::GraphRecordNotAttachable);
                        }
                    }
                    let outcome = planner.mutate(&mutation).map_err(graph_error)?;
                    let graph = planner.graph();
                    match outcome {
                        MutationOutcome::Node { id, created } => GraphOpOutcome::Node(GraphWrite {
                            id: id.0,
                            created,
                            revision: graph.node(id).map_or(sequence, |node| node.revision),
                        }),
                        MutationOutcome::Edge { id, created } => GraphOpOutcome::Edge(GraphWrite {
                            id: id.0,
                            created,
                            revision: graph.edge(id).map_or(sequence, |edge| edge.revision),
                        }),
                        MutationOutcome::NodeDeleted(id) => {
                            GraphOpOutcome::NodeDeleted { id: id.0 }
                        }
                        MutationOutcome::EdgeDeleted(id) => {
                            GraphOpOutcome::EdgeDeleted { id: id.0 }
                        }
                        MutationOutcome::Attached(node) => {
                            GraphOpOutcome::Attached { node: node.0 }
                        }
                        MutationOutcome::Detached { node, record } => GraphOpOutcome::Detached {
                            node: node.0,
                            record,
                        },
                        MutationOutcome::Schema => GraphOpOutcome::Schema,
                    }
                }
            };
            outcomes.push(outcome);
        }
        let body = planner.finish();
        if body.entries.is_empty() {
            return Ok(GraphBatchResult {
                revision: None,
                outcomes,
            });
        }
        let revision = self.commit_graph(&body, &vectors)?;
        Ok(GraphBatchResult {
            revision: Some(revision),
            outcomes,
        })
    }

    /// Whether `(collection, record)` is attached to a graph node. False on
    /// vector-only files (the only H-4 cost there is this `is_some` check).
    pub(crate) fn is_attached(&self, collection: u16, record: u64) -> bool {
        self.graph
            .as_ref()
            .is_some_and(|state| state.attachment_owner(collection, record).is_some())
    }

    /// Names of the collections with a record attached to the live node `key`
    /// of `graph`; empty when the graph or node does not exist. REST uses it
    /// to authorize `DeleteNode`, which removes those attachments.
    #[cfg(feature = "server")]
    pub(crate) fn node_attachment_collections(&self, graph: &str, key: &str) -> Vec<String> {
        let Some(graph) = self
            .graph
            .as_ref()
            .and_then(|state| state.graph_by_name(graph))
        else {
            return Vec::new();
        };
        let Some(node) = graph.node_by_key(key) else {
            return Vec::new();
        };
        graph
            .attachments(node.id)
            .filter_map(|(collection, _)| self.collections.get(&collection))
            .map(|collection| collection.name.clone())
            .collect()
    }

    /// H-4 (LEG-002, LEG-007): refuses a batch or import chunk that deletes
    /// an attached record.
    pub(crate) fn ensure_no_attached_delete(
        &self,
        collection: u16,
        operations: &[Mutation],
    ) -> Result<()> {
        if self.graph.is_some()
            && operations.iter().any(|operation| {
                matches!(operation, Mutation::Delete { id } if self.is_attached(collection, *id))
            })
        {
            return Err(Error::GraphAttachedRecord);
        }
        Ok(())
    }

    /// Section 3.5: a committed change to an attached record advances the
    /// owning graph's `graph_sequence` (LEG-005, LEG-006).
    pub(crate) fn touch_graph_record(&mut self, collection: u16, record: u64, sequence: u64) {
        if let Some(state) = self.graph.as_mut() {
            state.touch_record(collection, record, sequence);
        }
    }
}

fn selector(edge: &EdgeRef) -> EdgeSelector {
    match edge {
        EdgeRef::Id(id) => EdgeSelector::Id(EdgeId(*id)),
        EdgeRef::Key(key) => EdgeSelector::Key(key.clone()),
    }
}
