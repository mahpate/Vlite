//! Bounded graph reads on [`GraphRef`] (EPIC-005, REQ-020..REQ-028).
//!
//! Every read builds the engine's one query IR and runs the one engine
//! executor; `explain` validates and plans the same IR without executing it.
//! Inputs use the public `Condition`, `Metadata` and selector types and are
//! validated with the existing metadata validators before conversion, so
//! property conditions behave exactly like record filters
//! (`metadata::matches`). Results never carry vectors.
//!
//! Every result reports `complete`, `truncation`, `view_sequence` and `stats`
//! (REQ-025). `complete` is true exactly when `truncation` is empty: budgets,
//! output caps, the deadline and cancellation end a read early and are
//! reported, never hidden. `view_sequence` is the database's commit sequence
//! when the [`GraphRef`] was obtained. A `GraphRef` borrows the database, so
//! no commit can happen while it lives; for a read-only handle it is the
//! open-time sequence (Section 3.5, RD-009).
//!
//! Paged reads (`find_nodes`, `neighbors`) return an opaque `cursor` when a
//! page stopped early. Continuing it after any commit that changed the graph
//! (or an attached record's base record) fails with `GraphCursorStale`; a
//! cursor used with a different query is `InvalidQuery`.

use std::collections::BTreeSet;
use std::sync::atomic::AtomicBool;

use crate::{Condition, Metadata, Result};

use super::convert::{condition, graph_error, metadata_from_properties, property_conditions};
use super::engine::{self, EdgeSelector, ExecutionContext, QueryOp};
pub use super::engine::{Direction, GraphLimits};
use super::{EdgeRef, GraphRef, selector};

/// Names a node by internal ID or by key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum NodeSelector {
    Id(u64),
    Key(String),
}

impl NodeSelector {
    pub fn key(key: &str) -> Self {
        Self::Key(key.into())
    }
}

/// Which properties returned nodes and edges carry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PropertyProjection {
    #[default]
    All,
    None,
    /// At most 64 field names; fields a node or edge lacks are omitted.
    Only(Vec<String>),
}

/// Per-call read options. Limits above [`GraphLimits::CEILING`] fail with
/// `GraphLimitExceedsCeiling` and are never clamped. `cancel` is checked
/// before any work and at least every 64 budget units (PD-010).
#[derive(Clone, Debug, Default)]
pub struct ReadOptions<'a> {
    pub limits: GraphLimits,
    pub projection: PropertyProjection,
    pub cancel: Option<&'a AtomicBool>,
}

/// Labels (all required) and property conditions a node must match. The
/// default matches every node.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NodeFilter {
    pub labels: Vec<String>,
    pub conditions: Vec<Condition>,
}

/// One page of nodes with `label` (any node when `None`) matching all
/// `conditions`, by ascending node ID (REQ-020). `limit` defaults to
/// `max_output_nodes`; at most `max_edges_examined` nodes are scanned.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FindNodes {
    pub label: Option<String>,
    pub conditions: Vec<Condition>,
    pub limit: Option<u64>,
    pub cursor: Option<Vec<u8>>,
}

/// One page of a node's edges matching `edge_conditions`, ordered by
/// (relationship type, edge ID), with their other endpoints (REQ-021).
/// `types: None` means every type; an empty list is invalid. `limit`
/// defaults to `max_output_edges`.
#[derive(Clone, Debug, PartialEq)]
pub struct Neighbors {
    pub node: NodeSelector,
    pub direction: Direction,
    pub types: Option<Vec<String>>,
    pub edge_conditions: Vec<Condition>,
    pub limit: Option<u64>,
    pub cursor: Option<Vec<u8>>,
}

impl Neighbors {
    /// Outgoing edges of every type, first page.
    pub fn new(node: NodeSelector) -> Self {
        Self {
            node,
            direction: Direction::Out,
            types: None,
            edge_conditions: Vec::new(),
            limit: None,
            cursor: None,
        }
    }
}

/// Breadth-first traversal up to `max_hops` (REQ-022). Each node is visited
/// once at its minimum depth. Nodes failing `expand_filter` are visited but
/// not expanded; `return_filter` only selects returned nodes; only edges
/// matching `edge_filter` are followed. With `include_paths`, one canonical
/// shortest path per returned non-start node.
#[derive(Clone, Debug, PartialEq)]
pub struct Traverse {
    pub start: Vec<NodeSelector>,
    pub direction: Direction,
    pub types: Option<Vec<String>>,
    pub expand_filter: NodeFilter,
    pub return_filter: NodeFilter,
    pub edge_filter: Vec<Condition>,
    pub include_paths: bool,
}

impl Traverse {
    /// Outgoing edges of every type, no filters, no paths.
    pub fn new(start: Vec<NodeSelector>) -> Self {
        Self {
            start,
            direction: Direction::Out,
            types: None,
            expand_filter: NodeFilter::default(),
            return_filter: NodeFilter::default(),
            edge_filter: Vec::new(),
            include_paths: false,
        }
    }
}

/// One unweighted shortest path of at most `max_hops` edges (REQ-023). Ties
/// go to the lexicographically smallest edge-ID sequence.
#[derive(Clone, Debug, PartialEq)]
pub struct ShortestPath {
    pub source: NodeSelector,
    pub destination: NodeSelector,
    pub direction: Direction,
    pub types: Option<Vec<String>>,
}

impl ShortestPath {
    /// Following outgoing edges of every type.
    pub fn new(source: NodeSelector, destination: NodeSelector) -> Self {
        Self {
            source,
            destination,
            direction: Direction::Out,
            types: None,
        }
    }
}

/// Any graph read, for [`GraphRef::query`] and [`GraphRef::explain`].
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum GraphQuery {
    /// A node or `None`.
    GetNode(NodeSelector),
    /// An edge or `None`.
    GetEdge(EdgeRef),
    FindNodes(FindNodes),
    Neighbors(Neighbors),
    Traverse(Traverse),
    ShortestPath(ShortestPath),
}

/// A node as returned by reads (no vectors).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct NodeView {
    pub id: u64,
    pub key: String,
    pub labels: BTreeSet<String>,
    pub properties: Metadata,
    pub revision: u64,
}

/// An edge as returned by reads, with both endpoint keys.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct EdgeView {
    pub id: u64,
    pub key: Option<String>,
    pub relationship: String,
    pub source: u64,
    pub source_key: String,
    pub target: u64,
    pub target_key: String,
    pub properties: Metadata,
    pub revision: u64,
}

/// A path: node IDs (one more than edges) and edge IDs, in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct GraphPath {
    pub nodes: Vec<u64>,
    pub edges: Vec<u64>,
}

/// A node reached by a traversal, at its minimum hop depth.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct TraversedNode {
    pub depth: u64,
    pub node: NodeView,
}

/// Edges in adjacency order and their other endpoints, deduplicated in
/// first appearance order (a self-loop contributes the node itself).
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct Neighborhood {
    pub edges: Vec<EdgeView>,
    pub nodes: Vec<NodeView>,
}

/// Returned nodes in discovery order, the qualifying edges examined from
/// expanded nodes in first-examined order, and the requested paths.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct Traversal {
    pub nodes: Vec<TraversedNode>,
    pub edges: Vec<EdgeView>,
    pub paths: Vec<GraphPath>,
}

/// AC-008: a shortest-path answer. `NoPathWithinHops` is only reported after
/// a complete search; any truncation gives `Exhausted`.
#[derive(Clone, Debug, PartialEq)]
pub enum PathOutcome {
    Found {
        path: GraphPath,
        nodes: Vec<NodeView>,
        edges: Vec<EdgeView>,
    },
    NoPathWithinHops,
    Exhausted {
        truncation: Vec<TruncationReason>,
    },
}

/// INV-010: why a result is incomplete. Each reason appears at most once,
/// in declaration order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TruncationReason {
    VisitedLimit,
    EdgeLimit,
    /// Nodes whose adjacency was cut at `max_edges_per_node` (at most 64).
    DegreeLimit {
        node_ids: Vec<u64>,
    },
    /// An output cap or page size; never a work bound.
    OutputLimit,
    Deadline,
    Cancelled,
}

/// An index or access path a read uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexChoice {
    NodeIdLookup,
    NodeKeyIndex,
    EdgeIdLookup,
    EdgeKeyIndex,
    LabelIndexScan,
    FullNodeScan,
    /// Range scans of the (relationship type, edge ID) adjacency, one per
    /// requested type when `typed`.
    AdjacencyRangeScan {
        direction: Direction,
        typed: bool,
    },
}

/// Work done by a read. Only `elapsed_ms` depends on timing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct GraphReadStats {
    pub nodes_visited: u64,
    pub nodes_scanned: u64,
    pub edges_examined: u64,
    pub nodes_returned: u64,
    pub edges_returned: u64,
    pub paths_returned: u64,
    pub elapsed_ms: u64,
    pub indexes: Vec<IndexChoice>,
}

/// The result of a read (REQ-025).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct GraphRead<T> {
    pub output: T,
    /// True exactly when `truncation` is empty.
    pub complete: bool,
    pub truncation: Vec<TruncationReason>,
    pub view_sequence: u64,
    pub stats: GraphReadStats,
    /// Resumes an incomplete `find_nodes` or `neighbors` page.
    pub cursor: Option<Vec<u8>>,
}

/// The output of [`GraphRef::query`].
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum GraphOutput {
    Node(Option<NodeView>),
    Edge(Option<EdgeView>),
    Nodes(Vec<NodeView>),
    Neighbors(Neighborhood),
    Traversal(Traversal),
    ShortestPath(PathOutcome),
}

/// What [`GraphRef::explain`] reports instead of executing (REQ-028).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Explanation {
    /// The normalized query: page sizes filled in and duplicate start nodes
    /// removed.
    pub query: GraphQuery,
    pub limits: GraphLimits,
    pub projection: PropertyProjection,
    pub indexes: Vec<IndexChoice>,
}

// Public inputs → engine IR.

fn node_selector(selector: &NodeSelector) -> engine::NodeSelector {
    match selector {
        NodeSelector::Id(id) => engine::NodeSelector::Id(engine::NodeId(*id)),
        NodeSelector::Key(key) => engine::NodeSelector::Key(key.clone()),
    }
}

fn type_set(types: &Option<Vec<String>>) -> Option<BTreeSet<String>> {
    types.as_ref().map(|types| types.iter().cloned().collect())
}

fn node_filter(filter: &NodeFilter) -> Result<engine::NodeFilter> {
    Ok(engine::NodeFilter {
        labels: filter.labels.iter().cloned().collect(),
        conditions: property_conditions(&filter.conditions)?,
    })
}

fn projection(projection: &PropertyProjection) -> engine::PropertyProjection {
    match projection {
        PropertyProjection::All => engine::PropertyProjection::All,
        PropertyProjection::None => engine::PropertyProjection::None,
        PropertyProjection::Only(names) => {
            engine::PropertyProjection::Only(names.iter().cloned().collect())
        }
    }
}

fn find_nodes_op(request: &FindNodes) -> Result<QueryOp> {
    Ok(QueryOp::FindNodes {
        label: request.label.clone(),
        conditions: property_conditions(&request.conditions)?,
        limit: request.limit,
        cursor: request.cursor.clone(),
    })
}

fn neighbors_op(request: &Neighbors) -> Result<QueryOp> {
    Ok(QueryOp::Neighbors {
        node: node_selector(&request.node),
        direction: request.direction,
        types: type_set(&request.types),
        edge_conditions: property_conditions(&request.edge_conditions)?,
        limit: request.limit,
        cursor: request.cursor.clone(),
    })
}

fn traverse_op(request: &Traverse) -> Result<QueryOp> {
    Ok(QueryOp::Traverse {
        start: request.start.iter().map(node_selector).collect(),
        direction: request.direction,
        types: type_set(&request.types),
        expand_filter: node_filter(&request.expand_filter)?,
        return_filter: node_filter(&request.return_filter)?,
        edge_filter: property_conditions(&request.edge_filter)?,
        include_paths: request.include_paths,
    })
}

fn shortest_path_op(request: &ShortestPath) -> QueryOp {
    QueryOp::ShortestPath {
        source: node_selector(&request.source),
        destination: node_selector(&request.destination),
        direction: request.direction,
        types: type_set(&request.types),
    }
}

fn query_op(query: &GraphQuery) -> Result<QueryOp> {
    Ok(match query {
        GraphQuery::GetNode(node) => QueryOp::GetNode {
            node: node_selector(node),
        },
        GraphQuery::GetEdge(edge) => QueryOp::GetEdge {
            edge: selector(edge),
        },
        GraphQuery::FindNodes(request) => find_nodes_op(request)?,
        GraphQuery::Neighbors(request) => neighbors_op(request)?,
        GraphQuery::Traverse(request) => traverse_op(request)?,
        GraphQuery::ShortestPath(request) => shortest_path_op(request),
    })
}

// Engine results → public outputs.

fn public_selector(selector: &engine::NodeSelector) -> NodeSelector {
    match selector {
        engine::NodeSelector::Id(id) => NodeSelector::Id(id.0),
        engine::NodeSelector::Key(key) => NodeSelector::Key(key.clone()),
    }
}

fn public_types(types: &Option<BTreeSet<String>>) -> Option<Vec<String>> {
    types.as_ref().map(|types| types.iter().cloned().collect())
}

fn public_conditions(conditions: &[engine::PropertyCondition]) -> Vec<Condition> {
    conditions.iter().map(condition).collect()
}

fn public_filter(filter: &engine::NodeFilter) -> NodeFilter {
    NodeFilter {
        labels: filter.labels.iter().cloned().collect(),
        conditions: public_conditions(&filter.conditions),
    }
}

fn public_query(op: &QueryOp) -> GraphQuery {
    match op {
        QueryOp::GetNode { node } => GraphQuery::GetNode(public_selector(node)),
        QueryOp::GetEdge { edge } => GraphQuery::GetEdge(match edge {
            EdgeSelector::Id(id) => EdgeRef::Id(id.0),
            EdgeSelector::Key(key) => EdgeRef::Key(key.clone()),
        }),
        QueryOp::FindNodes {
            label,
            conditions,
            limit,
            cursor,
        } => GraphQuery::FindNodes(FindNodes {
            label: label.clone(),
            conditions: public_conditions(conditions),
            limit: *limit,
            cursor: cursor.clone(),
        }),
        QueryOp::Neighbors {
            node,
            direction,
            types,
            edge_conditions,
            limit,
            cursor,
        } => GraphQuery::Neighbors(Neighbors {
            node: public_selector(node),
            direction: *direction,
            types: public_types(types),
            edge_conditions: public_conditions(edge_conditions),
            limit: *limit,
            cursor: cursor.clone(),
        }),
        QueryOp::Traverse {
            start,
            direction,
            types,
            expand_filter,
            return_filter,
            edge_filter,
            include_paths,
        } => GraphQuery::Traverse(Traverse {
            start: start.iter().map(public_selector).collect(),
            direction: *direction,
            types: public_types(types),
            expand_filter: public_filter(expand_filter),
            return_filter: public_filter(return_filter),
            edge_filter: public_conditions(edge_filter),
            include_paths: *include_paths,
        }),
        QueryOp::ShortestPath {
            source,
            destination,
            direction,
            types,
        } => GraphQuery::ShortestPath(ShortestPath {
            source: public_selector(source),
            destination: public_selector(destination),
            direction: *direction,
            types: public_types(types),
        }),
    }
}

fn public_projection(projection: &engine::PropertyProjection) -> PropertyProjection {
    match projection {
        engine::PropertyProjection::All => PropertyProjection::All,
        engine::PropertyProjection::None => PropertyProjection::None,
        engine::PropertyProjection::Only(names) => {
            PropertyProjection::Only(names.iter().cloned().collect())
        }
    }
}

fn node_view(node: engine::NodeView) -> NodeView {
    NodeView {
        id: node.id.0,
        key: node.key,
        labels: node.labels,
        properties: metadata_from_properties(&node.properties),
        revision: node.revision,
    }
}

fn edge_view(edge: engine::EdgeView) -> EdgeView {
    EdgeView {
        id: edge.id.0,
        key: edge.key,
        relationship: edge.relationship,
        source: edge.source.0,
        source_key: edge.source_key,
        target: edge.target.0,
        target_key: edge.target_key,
        properties: metadata_from_properties(&edge.properties),
        revision: edge.revision,
    }
}

fn path(path: engine::GraphPath) -> GraphPath {
    GraphPath {
        nodes: path.nodes.into_iter().map(|id| id.0).collect(),
        edges: path.edges.into_iter().map(|id| id.0).collect(),
    }
}

fn truncation(reasons: Vec<engine::TruncationReason>) -> Vec<TruncationReason> {
    reasons
        .into_iter()
        .map(|reason| match reason {
            engine::TruncationReason::VisitedLimit => TruncationReason::VisitedLimit,
            engine::TruncationReason::EdgeLimit => TruncationReason::EdgeLimit,
            engine::TruncationReason::DegreeLimit { node_ids } => TruncationReason::DegreeLimit {
                node_ids: node_ids.into_iter().map(|id| id.0).collect(),
            },
            engine::TruncationReason::OutputLimit => TruncationReason::OutputLimit,
            engine::TruncationReason::Deadline => TruncationReason::Deadline,
            engine::TruncationReason::Cancelled => TruncationReason::Cancelled,
        })
        .collect()
}

fn index(choice: engine::IndexChoice) -> IndexChoice {
    match choice {
        engine::IndexChoice::NodeIdLookup => IndexChoice::NodeIdLookup,
        engine::IndexChoice::NodeKeyIndex => IndexChoice::NodeKeyIndex,
        engine::IndexChoice::EdgeIdLookup => IndexChoice::EdgeIdLookup,
        engine::IndexChoice::EdgeKeyIndex => IndexChoice::EdgeKeyIndex,
        engine::IndexChoice::LabelIndexScan => IndexChoice::LabelIndexScan,
        engine::IndexChoice::FullNodeScan => IndexChoice::FullNodeScan,
        engine::IndexChoice::AdjacencyRangeScan { direction, typed } => {
            IndexChoice::AdjacencyRangeScan { direction, typed }
        }
    }
}

fn output(output: engine::QueryOutput) -> GraphOutput {
    match output {
        engine::QueryOutput::Node(node) => GraphOutput::Node(node.map(node_view)),
        engine::QueryOutput::Edge(edge) => GraphOutput::Edge(edge.map(edge_view)),
        engine::QueryOutput::Nodes(nodes) => {
            GraphOutput::Nodes(nodes.into_iter().map(node_view).collect())
        }
        engine::QueryOutput::Neighbors { edges, nodes } => GraphOutput::Neighbors(Neighborhood {
            edges: edges.into_iter().map(edge_view).collect(),
            nodes: nodes.into_iter().map(node_view).collect(),
        }),
        engine::QueryOutput::Traversal {
            nodes,
            edges,
            paths,
        } => GraphOutput::Traversal(Traversal {
            nodes: nodes
                .into_iter()
                .map(|node| TraversedNode {
                    depth: node.depth,
                    node: node_view(node.node),
                })
                .collect(),
            edges: edges.into_iter().map(edge_view).collect(),
            paths: paths.into_iter().map(path).collect(),
        }),
        engine::QueryOutput::ShortestPath(outcome) => GraphOutput::ShortestPath(match outcome {
            engine::PathOutcome::Found {
                path: found,
                nodes,
                edges,
            } => PathOutcome::Found {
                path: path(found),
                nodes: nodes.into_iter().map(node_view).collect(),
                edges: edges.into_iter().map(edge_view).collect(),
            },
            engine::PathOutcome::NoPathWithinHops => PathOutcome::NoPathWithinHops,
            engine::PathOutcome::Exhausted {
                truncation: reasons,
            } => PathOutcome::Exhausted {
                truncation: truncation(reasons),
            },
        }),
        engine::QueryOutput::Explain(_) => unreachable!("only explain() requests explanations"),
    }
}

fn stats(stats: engine::QueryStats) -> GraphReadStats {
    GraphReadStats {
        nodes_visited: stats.nodes_visited,
        nodes_scanned: stats.nodes_scanned,
        edges_examined: stats.edges_examined,
        nodes_returned: stats.nodes_returned,
        edges_returned: stats.edges_returned,
        paths_returned: stats.paths_returned,
        elapsed_ms: stats.elapsed_ms,
        indexes: stats.indexes.into_iter().map(index).collect(),
    }
}

/// Monotonic milliseconds for the read deadline (PD-008).
#[cfg(not(target_arch = "wasm32"))]
fn clock() -> impl Fn() -> u64 {
    let start = std::time::Instant::now();
    move || u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The host-set clock; the engine tolerates a clock that goes backwards.
#[cfg(target_arch = "wasm32")]
fn clock() -> impl Fn() -> u64 {
    crate::current_time_ms
}

impl GraphRef<'_> {
    /// The commit sequence this handle reads at (echoed in every result).
    pub fn view_sequence(&self) -> u64 {
        self.view_sequence
    }

    fn execute(
        &self,
        op: QueryOp,
        options: &ReadOptions<'_>,
        explain: bool,
    ) -> Result<engine::QueryResult> {
        let query = engine::GraphQuery {
            op,
            projection: projection(&options.projection),
            explain,
        };
        let clock = clock();
        let context = ExecutionContext {
            view_sequence: self.view_sequence,
            clock: &clock,
            cancel: options.cancel,
        };
        engine::execute(self.graph, &query, &options.limits, &context).map_err(graph_error)
    }

    fn read<T>(
        &self,
        op: QueryOp,
        options: &ReadOptions<'_>,
        pick: fn(GraphOutput) -> Option<T>,
    ) -> Result<GraphRead<T>> {
        let result = self.execute(op, options, false)?;
        let output = pick(output(result.output)).expect("the executor answers in the query's kind");
        Ok(GraphRead {
            output,
            complete: result.complete,
            truncation: truncation(result.truncation),
            view_sequence: result.view_sequence,
            stats: stats(result.stats),
            cursor: result.cursor,
        })
    }

    /// Runs any read.
    pub fn query(
        &self,
        query: &GraphQuery,
        options: &ReadOptions<'_>,
    ) -> Result<GraphRead<GraphOutput>> {
        options.limits.validate().map_err(graph_error)?;
        self.read(query_op(query)?, options, Some)
    }

    /// Validates and plans `query` like [`GraphRef::query`] and returns the
    /// normalized query, effective limits and index choices without
    /// examining any node or edge (REQ-028, AC-024).
    pub fn explain(&self, query: &GraphQuery, options: &ReadOptions<'_>) -> Result<Explanation> {
        options.limits.validate().map_err(graph_error)?;
        let result = self.execute(query_op(query)?, options, true)?;
        let engine::QueryOutput::Explain(explanation) = result.output else {
            unreachable!("explain requests return an explanation")
        };
        Ok(Explanation {
            query: public_query(&explanation.query.op),
            limits: explanation.limits,
            projection: public_projection(&explanation.query.projection),
            indexes: explanation.indexes.into_iter().map(index).collect(),
        })
    }

    fn get_node_by(&self, node: NodeSelector) -> Result<GraphRead<Option<NodeView>>> {
        let op = QueryOp::GetNode {
            node: node_selector(&node),
        };
        self.read(op, &ReadOptions::default(), |output| match output {
            GraphOutput::Node(node) => Some(node),
            _ => None,
        })
    }

    fn get_edge_by(&self, edge: EdgeRef) -> Result<GraphRead<Option<EdgeView>>> {
        let op = QueryOp::GetEdge {
            edge: selector(&edge),
        };
        self.read(op, &ReadOptions::default(), |output| match output {
            GraphOutput::Edge(edge) => Some(edge),
            _ => None,
        })
    }

    /// The node with `key`, or `None` (REQ-020).
    pub fn get_node(&self, key: &str) -> Result<GraphRead<Option<NodeView>>> {
        self.get_node_by(NodeSelector::key(key))
    }

    /// The node with internal ID `id`, or `None`.
    pub fn get_node_by_id(&self, id: u64) -> Result<GraphRead<Option<NodeView>>> {
        self.get_node_by(NodeSelector::Id(id))
    }

    /// The edge with internal ID `id`, or `None`.
    pub fn get_edge(&self, id: u64) -> Result<GraphRead<Option<EdgeView>>> {
        self.get_edge_by(EdgeRef::Id(id))
    }

    /// The edge with edge key `key`, or `None`.
    pub fn get_edge_by_key(&self, key: &str) -> Result<GraphRead<Option<EdgeView>>> {
        self.get_edge_by(EdgeRef::Key(key.into()))
    }

    /// See [`FindNodes`].
    pub fn find_nodes(
        &self,
        request: &FindNodes,
        options: &ReadOptions<'_>,
    ) -> Result<GraphRead<Vec<NodeView>>> {
        options.limits.validate().map_err(graph_error)?;
        self.read(find_nodes_op(request)?, options, |output| match output {
            GraphOutput::Nodes(nodes) => Some(nodes),
            _ => None,
        })
    }

    /// See [`Neighbors`].
    pub fn neighbors(
        &self,
        request: &Neighbors,
        options: &ReadOptions<'_>,
    ) -> Result<GraphRead<Neighborhood>> {
        options.limits.validate().map_err(graph_error)?;
        self.read(neighbors_op(request)?, options, |output| match output {
            GraphOutput::Neighbors(neighborhood) => Some(neighborhood),
            _ => None,
        })
    }

    /// See [`Traverse`].
    pub fn traverse(
        &self,
        request: &Traverse,
        options: &ReadOptions<'_>,
    ) -> Result<GraphRead<Traversal>> {
        options.limits.validate().map_err(graph_error)?;
        self.read(traverse_op(request)?, options, |output| match output {
            GraphOutput::Traversal(traversal) => Some(traversal),
            _ => None,
        })
    }

    /// See [`ShortestPath`].
    pub fn shortest_path(
        &self,
        request: &ShortestPath,
        options: &ReadOptions<'_>,
    ) -> Result<GraphRead<PathOutcome>> {
        options.limits.validate().map_err(graph_error)?;
        self.read(shortest_path_op(request), options, |output| match output {
            GraphOutput::ShortestPath(outcome) => Some(outcome),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests;
