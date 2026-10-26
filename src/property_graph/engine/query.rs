//! REQ-020..REQ-028 (RD-008): the read query IR ([`GraphQuery`]), its
//! validation and normalization, the result types and the one executor
//! [`execute`] behind every read and `explain`.
//!
//! Precedence of failures: limits, then the query structure, then the
//! cursor (`InvalidCursor` before `CursorStale`), then node resolution.
//! Execution itself never fails: budgets, the deadline and cancellation end
//! a read early and are reported as truncation (INV-010).

use super::cursor::{self, CursorKind, CursorScope, Fingerprint, Position};
use super::error::GraphError;
use super::filter::{NodeFilter, conditions_match, validate_conditions};
use super::limits::{
    GraphLimits, MAX_DEGREE_LIMIT_NODE_IDS, MAX_FIELD_NAME_BYTES, MAX_PROPERTY_FIELDS,
};
use super::meter::{ExecutionContext, Meter, Stop};
use super::model::{
    Direction, EdgeId, GraphEdge, GraphNode, NodeId, Properties, PropertyCondition,
    validate_edge_key, validate_label, validate_node_key, validate_relationship_type,
};
use super::mutate::EdgeSelector;
use super::schema;
use super::state::Graph;
use super::traverse::{self, Shortest, TraversePlan};
use std::collections::{BTreeSet, HashSet};
use std::ops::Bound::{Excluded, Unbounded};

/// Names a node by internal ID or by key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeSelector {
    Id(NodeId),
    Key(String),
}

/// Which properties node and edge views carry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PropertyProjection {
    #[default]
    All,
    None,
    /// At most 64 field names; absent fields are omitted.
    Only(BTreeSet<String>),
}

/// One read. `types: None` means every relationship type; an empty set is
/// invalid. Hop counts come from [`GraphLimits::max_hops`].
#[derive(Clone, Debug, PartialEq)]
pub enum QueryOp {
    /// The node or `None`; unknown nodes are not an error.
    GetNode { node: NodeSelector },
    /// The edge or `None`; unknown edges are not an error.
    GetEdge { edge: EdgeSelector },
    /// One page of the nodes carrying `label` (every node when `None`) that
    /// match all `conditions`, by ascending node ID. `limit` defaults to
    /// `max_output_nodes`.
    FindNodes {
        label: Option<String>,
        conditions: Vec<PropertyCondition>,
        limit: Option<u64>,
        cursor: Option<Vec<u8>>,
    },
    /// One page of `node`'s edges matching `edge_conditions`, by
    /// (relationship type, edge ID), with their other endpoints. `limit`
    /// defaults to `max_output_edges`.
    Neighbors {
        node: NodeSelector,
        direction: Direction,
        types: Option<BTreeSet<String>>,
        edge_conditions: Vec<PropertyCondition>,
        limit: Option<u64>,
        cursor: Option<Vec<u8>>,
    },
    /// Breadth-first traversal from the start nodes (deduplicated, first
    /// occurrence kept). Nodes failing `expand_filter` are visited but not
    /// expanded; `return_filter` only selects the returned nodes; only edges
    /// matching `edge_filter` are followed.
    Traverse {
        start: Vec<NodeSelector>,
        direction: Direction,
        types: Option<BTreeSet<String>>,
        expand_filter: NodeFilter,
        return_filter: NodeFilter,
        edge_filter: Vec<PropertyCondition>,
        include_paths: bool,
    },
    /// Unweighted shortest path; ties go to the lexicographically smallest
    /// edge-ID sequence.
    ShortestPath {
        source: NodeSelector,
        destination: NodeSelector,
        direction: Direction,
        types: Option<BTreeSet<String>>,
    },
}

/// A read: the operation, the property projection of its views and whether
/// to only explain it (REQ-028).
#[derive(Clone, Debug, PartialEq)]
pub struct GraphQuery {
    pub op: QueryOp,
    pub projection: PropertyProjection,
    pub explain: bool,
}

impl GraphQuery {
    pub fn new(op: QueryOp) -> Self {
        Self {
            op,
            projection: PropertyProjection::All,
            explain: false,
        }
    }
}

/// A node as returned by reads (no vectors).
#[derive(Clone, Debug, PartialEq)]
pub struct NodeView {
    pub id: NodeId,
    pub key: String,
    pub labels: BTreeSet<String>,
    pub properties: Properties,
    pub revision: u64,
}

/// An edge as returned by reads, with both endpoint keys.
#[derive(Clone, Debug, PartialEq)]
pub struct EdgeView {
    pub id: EdgeId,
    pub key: Option<String>,
    pub relationship: String,
    pub source: NodeId,
    pub source_key: String,
    pub target: NodeId,
    pub target_key: String,
    pub properties: Properties,
    pub revision: u64,
}

/// A path as its nodes (one more than its edges) and edges, in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GraphPath {
    pub nodes: Vec<NodeId>,
    pub edges: Vec<EdgeId>,
}

/// A node reached by a traversal at its minimum depth.
#[derive(Clone, Debug, PartialEq)]
pub struct TraversedNode {
    pub depth: u64,
    pub node: NodeView,
}

/// AC-008: the answer of a shortest-path read.
#[derive(Clone, Debug, PartialEq)]
pub enum PathOutcome {
    /// A certified shortest path with its node and edge views in path order.
    Found {
        path: GraphPath,
        nodes: Vec<NodeView>,
        edges: Vec<EdgeView>,
    },
    /// Exploration up to `max_hops` (or of the whole reachable region)
    /// finished without truncation and found no path.
    NoPathWithinHops,
    /// A budget, the deadline or cancellation ended the search first.
    Exhausted { truncation: Vec<TruncationReason> },
}

/// INV-010: why a result may be incomplete. Results list each reason at
/// most once, in declaration order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TruncationReason {
    VisitedLimit,
    EdgeLimit,
    /// Nodes whose adjacency was cut at `max_edges_per_node`, first
    /// encountered first, deduplicated, at most 64 listed.
    DegreeLimit {
        node_ids: Vec<NodeId>,
    },
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
    /// A declared exact (or unique) property index on the requested label,
    /// probed by an `Eq` condition (REQ-033).
    PropertyExactIndex,
    /// A declared range property index on the requested label, probed by a
    /// `Range` condition (REQ-033).
    PropertyRangeIndex,
    /// Range scans of the (relationship type, edge ID) adjacency, one per
    /// requested type when `typed`.
    AdjacencyRangeScan {
        direction: Direction,
        typed: bool,
    },
}

/// Work done by a read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueryStats {
    /// Nodes discovered by `neighbors`, `traverse` and `shortest_path`.
    pub nodes_visited: u64,
    /// Nodes evaluated by `find_nodes`.
    pub nodes_scanned: u64,
    /// Adjacency entries examined, filtered and duplicate entries included.
    pub edges_examined: u64,
    pub nodes_returned: u64,
    pub edges_returned: u64,
    pub paths_returned: u64,
    pub elapsed_ms: u64,
    pub indexes: Vec<IndexChoice>,
}

/// What `explain` reports instead of executing.
#[derive(Clone, Debug, PartialEq)]
pub struct Explanation {
    /// The normalized query: defaults applied, start nodes deduplicated and
    /// `explain` cleared, ready to execute.
    pub query: GraphQuery,
    pub limits: GraphLimits,
    pub indexes: Vec<IndexChoice>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum QueryOutput {
    Node(Option<NodeView>),
    Edge(Option<EdgeView>),
    Nodes(Vec<NodeView>),
    /// Edges in adjacency order and their other endpoints, deduplicated, in
    /// first appearance order (a self-loop contributes the node itself).
    Neighbors {
        edges: Vec<EdgeView>,
        nodes: Vec<NodeView>,
    },
    /// Returned nodes in discovery order, the qualifying edges examined from
    /// expanded nodes in first-examined order, and with `include_paths` one
    /// canonical path per returned non-start node whose edges are all
    /// returned.
    Traversal {
        nodes: Vec<TraversedNode>,
        edges: Vec<EdgeView>,
        paths: Vec<GraphPath>,
    },
    ShortestPath(PathOutcome),
    Explain(Box<Explanation>),
}

/// The result of [`execute`]. `complete` is true exactly when `truncation`
/// is empty; `cursor` is set exactly when an incomplete `find_nodes` or
/// `neighbors` page can be resumed.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryResult {
    pub output: QueryOutput,
    pub complete: bool,
    pub truncation: Vec<TruncationReason>,
    /// The caller's [`ExecutionContext::view_sequence`].
    pub view_sequence: u64,
    pub stats: QueryStats,
    pub cursor: Option<Vec<u8>>,
}

/// A length as a counter.
pub(super) fn count(length: usize) -> u64 {
    u64::try_from(length).unwrap_or(u64::MAX)
}

/// Truncation events of one read, collected in any order.
#[derive(Default)]
pub(super) struct Truncation {
    visited: bool,
    edge: bool,
    degree: Option<Vec<NodeId>>,
    output: bool,
    deadline: bool,
    cancelled: bool,
}

impl Truncation {
    pub(super) fn stop(&mut self, stop: Stop) {
        match stop {
            Stop::EdgeLimit => self.edge = true,
            Stop::Deadline => self.deadline = true,
            Stop::Cancelled => self.cancelled = true,
        }
    }

    pub(super) fn visited(&mut self) {
        self.visited = true;
    }

    pub(super) fn output(&mut self) {
        self.output = true;
    }

    pub(super) fn degree(&mut self, node: NodeId) {
        let ids = self.degree.get_or_insert_with(Vec::new);
        if ids.len() < MAX_DEGREE_LIMIT_NODE_IDS && !ids.contains(&node) {
            ids.push(node);
        }
    }

    fn reasons(self) -> Vec<TruncationReason> {
        let mut reasons = Vec::new();
        if self.visited {
            reasons.push(TruncationReason::VisitedLimit);
        }
        if self.edge {
            reasons.push(TruncationReason::EdgeLimit);
        }
        if let Some(node_ids) = self.degree {
            reasons.push(TruncationReason::DegreeLimit { node_ids });
        }
        if self.output {
            reasons.push(TruncationReason::OutputLimit);
        }
        if self.deadline {
            reasons.push(TruncationReason::Deadline);
        }
        if self.cancelled {
            reasons.push(TruncationReason::Cancelled);
        }
        reasons
    }
}

/// State of one executing read, shared with [`super::traverse`].
pub(super) struct Exec<'g, 'c> {
    pub(super) graph: &'g Graph,
    pub(super) limits: GraphLimits,
    pub(super) meter: Meter<'c>,
    pub(super) truncation: Truncation,
    pub(super) stats: QueryStats,
    projection: &'g PropertyProjection,
}

fn project(properties: &Properties, projection: &PropertyProjection) -> Properties {
    match projection {
        PropertyProjection::All => properties.clone(),
        PropertyProjection::None => Properties::new(),
        PropertyProjection::Only(names) => names
            .iter()
            .filter_map(|name| properties.get_key_value(name.as_str()))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
    }
}

impl Exec<'_, '_> {
    pub(super) fn node_view(&self, node: &GraphNode) -> NodeView {
        NodeView {
            id: node.id,
            key: node.key.clone(),
            labels: node.labels.clone(),
            properties: project(&node.properties, self.projection),
            revision: node.revision,
        }
    }

    pub(super) fn edge_view(&self, edge: &GraphEdge) -> Result<EdgeView, GraphError> {
        let source = self
            .graph
            .node(edge.source)
            .ok_or(GraphError::InvariantViolation(
                "edge source is missing during read",
            ))?;
        let target = self
            .graph
            .node(edge.destination)
            .ok_or(GraphError::InvariantViolation(
                "edge target is missing during read",
            ))?;
        Ok(EdgeView {
            id: edge.id,
            key: edge.key.clone(),
            relationship: edge.relationship.clone(),
            source: edge.source,
            source_key: source.key.clone(),
            target: edge.destination,
            target_key: target.key.clone(),
            properties: project(&edge.properties, self.projection),
            revision: edge.revision,
        })
    }
}

/// One page of a paged read: its size, the scope its cursors carry and the
/// position it resumes after.
struct Page {
    size: u64,
    scope: CursorScope,
    after: Position,
}

impl Page {
    fn cursor(&self, position: Position) -> Option<Vec<u8>> {
        Some(cursor::encode(&self.scope, &position))
    }
}

/// A validated query with every node it names resolved.
enum Plan<'q> {
    GetNode(&'q NodeSelector),
    GetEdge(&'q EdgeSelector),
    FindNodes {
        label: Option<&'q str>,
        conditions: &'q [PropertyCondition],
        /// The condition probing a declared property index, if any.
        index: Option<&'q PropertyCondition>,
        page: Page,
    },
    Neighbors {
        node: NodeId,
        direction: Direction,
        types: Option<&'q BTreeSet<String>>,
        conditions: &'q [PropertyCondition],
        page: Page,
    },
    Traverse(TraversePlan<'q>),
    ShortestPath {
        source: NodeId,
        destination: NodeId,
        direction: Direction,
        types: Option<&'q BTreeSet<String>>,
    },
}

fn validate_selector(selector: &NodeSelector) -> Result<(), GraphError> {
    match selector {
        NodeSelector::Id(_) => Ok(()),
        NodeSelector::Key(key) => validate_node_key(key),
    }
}

fn validate_types(types: Option<&BTreeSet<String>>) -> Result<(), GraphError> {
    match types {
        None => Ok(()),
        Some(types) if types.is_empty() => Err(GraphError::InvalidQuery(
            "a relationship type set must not be empty; omit it for every type",
        )),
        Some(types) => types
            .iter()
            .try_for_each(|kind| validate_relationship_type(kind)),
    }
}

fn validate_projection(projection: &PropertyProjection) -> Result<(), GraphError> {
    let PropertyProjection::Only(names) = projection else {
        return Ok(());
    };
    if names.len() > MAX_PROPERTY_FIELDS {
        return Err(GraphError::InvalidQuery(
            "a projection names at most 64 properties",
        ));
    }
    if names
        .iter()
        .any(|name| !(1..=MAX_FIELD_NAME_BYTES).contains(&name.len()))
    {
        return Err(GraphError::InvalidQuery(
            "projected field names must be 1..=256 UTF-8 bytes",
        ));
    }
    Ok(())
}

/// The explicit `limit` or the default output cap; above `ceiling` it is
/// rejected, and a page must hold at least one item.
fn page_size(limit: Option<u64>, default: u64, ceiling: u64) -> Result<u64, GraphError> {
    let size = limit.unwrap_or(default);
    if size > ceiling {
        return Err(GraphError::LimitExceedsCeiling {
            limit: "limit",
            requested: size,
            ceiling,
        });
    }
    if size == 0 {
        return Err(GraphError::InvalidQuery(
            "the page size (limit or its max_output default) must be at least 1",
        ));
    }
    Ok(size)
}

fn page(
    graph: &Graph,
    kind: CursorKind,
    fingerprint: &Fingerprint,
    size: u64,
    bytes: Option<&[u8]>,
) -> Result<Page, GraphError> {
    let scope = CursorScope {
        kind,
        graph: graph.id(),
        graph_sequence: graph.graph_sequence(),
        fingerprint: fingerprint.finish(),
    };
    let after = match bytes {
        Some(bytes) => cursor::decode(bytes, &scope)?,
        None => Position::Start,
    };
    Ok(Page { size, scope, after })
}

fn fingerprint_selector(fingerprint: &mut Fingerprint, selector: &NodeSelector) {
    match selector {
        NodeSelector::Id(id) => {
            fingerprint.u64(0);
            fingerprint.u64(id.0);
        }
        NodeSelector::Key(key) => {
            fingerprint.u64(1);
            fingerprint.str(key);
        }
    }
}

fn fingerprint_types(fingerprint: &mut Fingerprint, types: Option<&BTreeSet<String>>) {
    match types {
        None => fingerprint.u64(0),
        Some(types) => {
            fingerprint.u64(1);
            fingerprint.u64(types.len() as u64);
            for kind in types {
                fingerprint.str(kind);
            }
        }
    }
}

fn fingerprint_projection(fingerprint: &mut Fingerprint, projection: &PropertyProjection) {
    match projection {
        PropertyProjection::All => fingerprint.u64(0),
        PropertyProjection::None => fingerprint.u64(1),
        PropertyProjection::Only(names) => {
            fingerprint.u64(2);
            fingerprint.u64(count(names.len()));
            for name in names {
                fingerprint.str(name);
            }
        }
    }
}

fn find<'g>(graph: &'g Graph, selector: &NodeSelector) -> Option<&'g GraphNode> {
    match selector {
        NodeSelector::Id(id) => graph.node(*id),
        NodeSelector::Key(key) => graph.node_by_key(key),
    }
}

fn resolve(graph: &Graph, selector: &NodeSelector) -> Result<NodeId, GraphError> {
    find(graph, selector)
        .map(|node| node.id)
        .ok_or_else(|| match selector {
            NodeSelector::Id(id) => GraphError::NodeNotFound(*id),
            NodeSelector::Key(key) => GraphError::UnknownNodeKey(key.clone()),
        })
}

fn lookup(selector: &NodeSelector) -> IndexChoice {
    match selector {
        NodeSelector::Id(_) => IndexChoice::NodeIdLookup,
        NodeSelector::Key(_) => IndexChoice::NodeKeyIndex,
    }
}

/// Node lookups in first use order, then the adjacency scan.
fn traversal_indexes<'a>(
    selectors: impl IntoIterator<Item = &'a NodeSelector>,
    direction: Direction,
    types: Option<&BTreeSet<String>>,
) -> Vec<IndexChoice> {
    let mut indexes = Vec::new();
    for selector in selectors {
        let index = lookup(selector);
        if !indexes.contains(&index) {
            indexes.push(index);
        }
    }
    indexes.push(IndexChoice::AdjacencyRangeScan {
        direction,
        typed: types.is_some(),
    });
    indexes
}

fn prepare<'q>(
    graph: &Graph,
    query: &'q GraphQuery,
    limits: &GraphLimits,
) -> Result<(Plan<'q>, Vec<IndexChoice>), GraphError> {
    limits.validate()?;
    validate_projection(&query.projection)?;
    Ok(match &query.op {
        QueryOp::GetNode { node } => {
            validate_selector(node)?;
            (Plan::GetNode(node), vec![lookup(node)])
        }
        QueryOp::GetEdge { edge } => {
            let index = match edge {
                EdgeSelector::Id(_) => IndexChoice::EdgeIdLookup,
                EdgeSelector::Key(key) => {
                    validate_edge_key(key)?;
                    IndexChoice::EdgeKeyIndex
                }
            };
            (Plan::GetEdge(edge), vec![index])
        }
        QueryOp::FindNodes {
            label,
            conditions,
            limit,
            cursor,
        } => {
            if let Some(label) = label {
                validate_label(label)?;
            }
            validate_conditions(conditions)?;
            let size = page_size(
                *limit,
                limits.max_output_nodes,
                GraphLimits::CEILING.max_output_nodes,
            )?;
            let mut fingerprint = Fingerprint::new(CursorKind::FindNodes);
            match label {
                None => fingerprint.u64(0),
                Some(label) => {
                    fingerprint.u64(1);
                    fingerprint.str(label);
                }
            }
            fingerprint.conditions(conditions);
            fingerprint.u64(size);
            fingerprint_projection(&mut fingerprint, &query.projection);
            let page = page(
                graph,
                CursorKind::FindNodes,
                &fingerprint,
                size,
                cursor.as_deref(),
            )?;
            let probe = label
                .as_deref()
                .and_then(|label| schema::usable_index(graph.schema(), label, conditions));
            let index = match (label, probe) {
                (_, Some(PropertyCondition::Range { .. })) => IndexChoice::PropertyRangeIndex,
                (_, Some(_)) => IndexChoice::PropertyExactIndex,
                (Some(_), None) => IndexChoice::LabelIndexScan,
                (None, None) => IndexChoice::FullNodeScan,
            };
            let plan = Plan::FindNodes {
                label: label.as_deref(),
                conditions,
                index: probe,
                page,
            };
            (plan, vec![index])
        }
        QueryOp::Neighbors {
            node,
            direction,
            types,
            edge_conditions,
            limit,
            cursor,
        } => {
            validate_selector(node)?;
            validate_types(types.as_ref())?;
            validate_conditions(edge_conditions)?;
            let size = page_size(
                *limit,
                limits.max_output_edges,
                GraphLimits::CEILING.max_output_edges,
            )?;
            let mut fingerprint = Fingerprint::new(CursorKind::Neighbors);
            fingerprint_selector(&mut fingerprint, node);
            fingerprint.direction(*direction);
            fingerprint_types(&mut fingerprint, types.as_ref());
            fingerprint.conditions(edge_conditions);
            fingerprint.u64(size);
            fingerprint_projection(&mut fingerprint, &query.projection);
            let page = page(
                graph,
                CursorKind::Neighbors,
                &fingerprint,
                size,
                cursor.as_deref(),
            )?;
            let plan = Plan::Neighbors {
                node: resolve(graph, node)?,
                direction: *direction,
                types: types.as_ref(),
                conditions: edge_conditions,
                page,
            };
            let indexes = traversal_indexes([node], *direction, types.as_ref());
            (plan, indexes)
        }
        QueryOp::Traverse {
            start,
            direction,
            types,
            expand_filter,
            return_filter,
            edge_filter,
            include_paths,
        } => {
            if start.is_empty() {
                return Err(GraphError::InvalidQuery(
                    "a traversal needs at least one start node",
                ));
            }
            let ceiling = GraphLimits::CEILING.max_visited_nodes;
            let requested = u64::try_from(start.len()).unwrap_or(u64::MAX);
            if requested > ceiling {
                return Err(GraphError::LimitExceedsCeiling {
                    limit: "start_nodes",
                    requested,
                    ceiling,
                });
            }
            start.iter().try_for_each(validate_selector)?;
            validate_types(types.as_ref())?;
            expand_filter.validate()?;
            return_filter.validate()?;
            validate_conditions(edge_filter)?;
            let mut seen = HashSet::new();
            let mut nodes = Vec::new();
            for selector in start {
                let id = resolve(graph, selector)?;
                if seen.insert(id) {
                    nodes.push((id, selector));
                }
            }
            let indexes = traversal_indexes(
                nodes.iter().map(|(_, selector)| *selector),
                *direction,
                types.as_ref(),
            );
            let plan = Plan::Traverse(TraversePlan {
                start: nodes,
                direction: *direction,
                types: types.as_ref(),
                expand_filter,
                return_filter,
                edge_filter,
                include_paths: *include_paths,
            });
            (plan, indexes)
        }
        QueryOp::ShortestPath {
            source,
            destination,
            direction,
            types,
        } => {
            validate_selector(source)?;
            validate_selector(destination)?;
            validate_types(types.as_ref())?;
            let plan = Plan::ShortestPath {
                source: resolve(graph, source)?,
                destination: resolve(graph, destination)?,
                direction: *direction,
                types: types.as_ref(),
            };
            let indexes = traversal_indexes([source, destination], *direction, types.as_ref());
            (plan, indexes)
        }
    })
}

/// The query as it executes: page sizes filled in and duplicate start
/// nodes removed.
fn normalized(query: &GraphQuery, plan: &Plan<'_>) -> GraphQuery {
    let mut query = query.clone();
    query.explain = false;
    match (&mut query.op, plan) {
        (QueryOp::FindNodes { limit, .. }, Plan::FindNodes { page, .. })
        | (QueryOp::Neighbors { limit, .. }, Plan::Neighbors { page, .. }) => {
            *limit = Some(page.size);
        }
        (QueryOp::Traverse { start, .. }, Plan::Traverse(plan)) => {
            *start = plan
                .start
                .iter()
                .map(|(_, selector)| (*selector).clone())
                .collect();
        }
        _ => {}
    }
    query
}

/// Validates, plans and runs `query` against `graph` (RD-008).
///
/// `explain` returns the normalized query, the effective limits and the
/// index choices after the same validation, without consulting the clock
/// or the cancellation flag and without examining anything.
pub fn execute(
    graph: &Graph,
    query: &GraphQuery,
    limits: &GraphLimits,
    context: &ExecutionContext<'_>,
) -> Result<QueryResult, GraphError> {
    let (plan, indexes) = prepare(graph, query, limits)?;
    if query.explain {
        let explanation = Explanation {
            query: normalized(query, &plan),
            limits: *limits,
            indexes: indexes.clone(),
        };
        return Ok(QueryResult {
            output: QueryOutput::Explain(Box::new(explanation)),
            complete: true,
            truncation: Vec::new(),
            view_sequence: context.view_sequence,
            stats: QueryStats {
                indexes,
                ..QueryStats::default()
            },
            cursor: None,
        });
    }
    let (meter, begin) = Meter::start(context, limits);
    let mut exec = Exec {
        graph,
        limits: *limits,
        meter,
        truncation: Truncation::default(),
        stats: QueryStats {
            indexes,
            ..QueryStats::default()
        },
        projection: &query.projection,
    };
    let (mut output, cursor) = match begin {
        Ok(()) => run(&mut exec, &plan)?,
        Err(stop) => {
            exec.truncation.stop(stop);
            skipped(&plan)
        }
    };
    let truncation = std::mem::take(&mut exec.truncation).reasons();
    if let QueryOutput::ShortestPath(PathOutcome::Exhausted {
        truncation: reasons,
    }) = &mut output
    {
        reasons.clone_from(&truncation);
    }
    let (nodes, edges, paths) = match &output {
        QueryOutput::Node(node) => (u64::from(node.is_some()), 0, 0),
        QueryOutput::Edge(edge) => (0, u64::from(edge.is_some()), 0),
        QueryOutput::Nodes(nodes) => (count(nodes.len()), 0, 0),
        QueryOutput::Neighbors { edges, nodes } => (count(nodes.len()), count(edges.len()), 0),
        QueryOutput::Traversal {
            nodes,
            edges,
            paths,
        } => (count(nodes.len()), count(edges.len()), count(paths.len())),
        QueryOutput::ShortestPath(PathOutcome::Found { nodes, edges, .. }) => {
            (count(nodes.len()), count(edges.len()), 1)
        }
        QueryOutput::ShortestPath(_) | QueryOutput::Explain(_) => (0, 0, 0),
    };
    let mut stats = std::mem::take(&mut exec.stats);
    stats.nodes_returned = nodes;
    stats.edges_returned = edges;
    stats.paths_returned = paths;
    stats.elapsed_ms = exec.meter.elapsed_ms();
    Ok(QueryResult {
        output,
        complete: truncation.is_empty(),
        truncation,
        view_sequence: context.view_sequence,
        stats,
        cursor,
    })
}

/// The output of a read stopped before any work, and the cursor that
/// repeats its page.
fn skipped(plan: &Plan<'_>) -> (QueryOutput, Option<Vec<u8>>) {
    match plan {
        Plan::GetNode(_) => (QueryOutput::Node(None), None),
        Plan::GetEdge(_) => (QueryOutput::Edge(None), None),
        Plan::FindNodes { page, .. } => (
            QueryOutput::Nodes(Vec::new()),
            page.cursor(page.after.clone()),
        ),
        Plan::Neighbors { page, .. } => (
            QueryOutput::Neighbors {
                edges: Vec::new(),
                nodes: Vec::new(),
            },
            page.cursor(page.after.clone()),
        ),
        Plan::Traverse(_) => (
            QueryOutput::Traversal {
                nodes: Vec::new(),
                edges: Vec::new(),
                paths: Vec::new(),
            },
            None,
        ),
        Plan::ShortestPath { .. } => (
            QueryOutput::ShortestPath(PathOutcome::Exhausted {
                truncation: Vec::new(),
            }),
            None,
        ),
    }
}

fn run(
    exec: &mut Exec<'_, '_>,
    plan: &Plan<'_>,
) -> Result<(QueryOutput, Option<Vec<u8>>), GraphError> {
    let graph = exec.graph;
    Ok(match plan {
        Plan::GetNode(selector) => (
            QueryOutput::Node(find(graph, selector).map(|node| exec.node_view(node))),
            None,
        ),
        Plan::GetEdge(selector) => {
            let edge = match selector {
                EdgeSelector::Id(id) => graph.edge(*id),
                EdgeSelector::Key(key) => graph.edge_by_key(key),
            };
            (
                QueryOutput::Edge(edge.map(|edge| exec.edge_view(edge)).transpose()?),
                None,
            )
        }
        Plan::FindNodes {
            label,
            conditions,
            index,
            page,
        } => find_nodes(exec, *label, conditions, *index, page)?,
        Plan::Neighbors {
            node,
            direction,
            types,
            conditions,
            page,
        } => neighbors(exec, *node, *direction, *types, conditions, page)?,
        Plan::Traverse(plan) => (traverse::traverse(exec, plan)?, None),
        Plan::ShortestPath {
            source,
            destination,
            direction,
            types,
        } => {
            let outcome =
                match traverse::shortest_path(exec, *source, *destination, *direction, *types) {
                    Shortest::Found(path) => {
                        let nodes = path
                            .nodes
                            .iter()
                            .map(|id| {
                                graph
                                    .node(*id)
                                    .ok_or(GraphError::InvariantViolation(
                                        "path node is missing during read",
                                    ))
                                    .map(|node| exec.node_view(node))
                            })
                            .collect::<Result<Vec<_>, _>>()?;
                        let edges = path
                            .edges
                            .iter()
                            .map(|id| {
                                graph
                                    .edge(*id)
                                    .ok_or(GraphError::InvariantViolation(
                                        "path edge is missing during read",
                                    ))
                                    .and_then(|edge| exec.edge_view(edge))
                            })
                            .collect::<Result<Vec<_>, _>>()?;
                        PathOutcome::Found { path, nodes, edges }
                    }
                    Shortest::NoPath => PathOutcome::NoPathWithinHops,
                    Shortest::Exhausted => PathOutcome::Exhausted {
                        truncation: Vec::new(),
                    },
                };
            (QueryOutput::ShortestPath(outcome), None)
        }
    })
}

/// Scans candidates in node ID order. `max_edges_examined` bounds the nodes
/// scanned (PD-006); a full page stops while candidates remain. With a
/// declared property index the candidates are its postings above the page
/// position, still re-checked against every condition, so results equal a
/// label scan.
fn find_nodes(
    exec: &mut Exec<'_, '_>,
    label: Option<&str>,
    conditions: &[PropertyCondition],
    index: Option<&PropertyCondition>,
    page: &Page,
) -> Result<(QueryOutput, Option<Vec<u8>>), GraphError> {
    let graph = exec.graph;
    let mut last = match page.after {
        Position::Node(id) => Some(id),
        _ => None,
    };
    let lower = last.map_or(Unbounded, Excluded);
    let candidates: Box<dyn Iterator<Item = &GraphNode> + '_> = match (label, index) {
        (Some(label), Some(PropertyCondition::Eq { field, value })) => Box::new(
            graph
                .property_index(label, field)
                .and_then(|postings| postings.equal(value))
                .into_iter()
                .flat_map(move |ids| ids.range((lower, Unbounded)))
                .filter_map(|id| graph.node(*id)),
        ),
        (Some(label), Some(PropertyCondition::Range { field, min, max })) => {
            let ids = graph
                .property_index(label, field)
                .map(|postings| postings.range(min.as_ref(), max.as_ref()))
                .unwrap_or_default();
            Box::new(
                ids.into_iter()
                    .filter(move |id| last.is_none_or(|last| *id > last))
                    .filter_map(|id| graph.node(id)),
            )
        }
        (Some(label), _) => Box::new(
            graph
                .label_scan(label, last)
                .filter_map(|id| graph.node(id)),
        ),
        (None, _) => Box::new(graph.node_scan(last)),
    };
    let mut nodes = Vec::new();
    let mut stopped = false;
    for node in candidates {
        if count(nodes.len()) >= page.size {
            exec.truncation.output();
            stopped = true;
            break;
        }
        if let Err(stop) = exec.meter.charge() {
            exec.truncation.stop(stop);
            stopped = true;
            break;
        }
        last = Some(node.id);
        if conditions_match(&node.properties, conditions) {
            nodes.push(exec.node_view(node));
        }
    }
    exec.stats.nodes_scanned = exec.meter.spent();
    let cursor = if stopped {
        page.cursor(last.map_or(Position::Start, Position::Node))
    } else {
        None
    };
    Ok((QueryOutput::Nodes(nodes), cursor))
}

/// Reads `node`'s adjacency lazily from the page position; at most
/// `max_edges_per_node` entries per call.
fn neighbors(
    exec: &mut Exec<'_, '_>,
    node: NodeId,
    direction: Direction,
    types: Option<&BTreeSet<String>>,
    conditions: &[PropertyCondition],
    page: &Page,
) -> Result<(QueryOutput, Option<Vec<u8>>), GraphError> {
    let graph = exec.graph;
    let mut last = match &page.after {
        Position::Edge(kind, id) => Some((kind.as_str(), *id)),
        _ => None,
    };
    let mut edges = Vec::new();
    let mut nodes = Vec::new();
    let mut seen = HashSet::new();
    let mut stopped = false;
    exec.stats.nodes_visited = 1;
    for (examined, (kind, id)) in graph.adjacency(node, direction, types, last).enumerate() {
        if count(edges.len()) >= page.size {
            exec.truncation.output();
            stopped = true;
            break;
        }
        if count(examined) >= exec.limits.max_edges_per_node {
            exec.truncation.degree(node);
            stopped = true;
            break;
        }
        if let Err(stop) = exec.meter.charge() {
            exec.truncation.stop(stop);
            stopped = true;
            break;
        }
        last = Some((kind, id));
        let Some(edge) = graph.edge(id) else {
            continue;
        };
        if !conditions_match(&edge.properties, conditions) {
            continue;
        }
        edges.push(exec.edge_view(edge)?);
        let other = traverse::other_end(edge, node);
        if seen.insert(other)
            && let Some(other) = graph.node(other)
        {
            nodes.push(exec.node_view(other));
        }
    }
    exec.stats.edges_examined = exec.meter.spent();
    let cursor = if stopped {
        page.cursor(last.map_or(Position::Start, |(kind, id)| {
            Position::Edge(kind.to_owned(), id)
        }))
    } else {
        None
    };
    Ok((QueryOutput::Neighbors { edges, nodes }, cursor))
}

#[cfg(test)]
mod tests;
