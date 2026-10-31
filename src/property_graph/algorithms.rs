//! Bounded local graph algorithms on [`GraphRef`] (Gap 5 slices S1 and S2).
//!
//! [`GraphRef::algorithm`] extracts one explicit local scope and runs one
//! algorithm on it, within the read limits ([`GraphLimits`]) plus the
//! algorithm caps ([`GraphAlgorithmLimits`]):
//!
//! - Scope ([`GraphAlgorithmScope`]): 1..=`max_visited_nodes` distinct seeds
//!   of this graph, a direction, explicit relationship types or
//!   [`RelationshipScope::Any`], an all-labels node filter or
//!   [`LabelScope::Any`], and edge property conditions (record-filter
//!   semantics). Seeds must be visible and carry every scope label.
//!   Breadth-first discovery admits eligible nodes up to `max_hops` hops
//!   from the seeds; then every admitted node's adjacency is scanned again
//!   and every eligible edge between admitted nodes is retained (an induced
//!   projection). Excluded nodes never bridge the scope.
//! - Visibility: one TTL clock (the handle's, see [`GraphRef::with_clock`])
//!   decides every lookup; due nodes and edges are invisible and a due node
//!   hides its edges.
//! - Budgets: extraction, rescans, rejected edges, weight validation and the
//!   computation share `max_edges_examined`, `max_vertex_operations`,
//!   `max_scratch_bytes`, the deadline and cancellation, checked before any
//!   work, before every 64th charged operation and before returning. Any
//!   stop before the computation finished returns no output.
//! - Results ([`GraphAlgorithmRead`]): rows ascend by node ID, paths keep
//!   traversal order, and the normalized request, view sequence and TTL
//!   clock are echoed. Output caps apply to finished results.
//!
//! Embedded callers choose the graph; nothing follows attachments or
//! returns properties or vectors, and there are no continuation cursors.

use std::fmt;
use std::sync::atomic::AtomicBool;

use crate::{Condition, Error};

use super::GraphRef;
use super::convert::{graph_error, property_conditions};
use super::engine::algorithm as kernel;
pub use super::engine::algorithm::GraphAlgorithmLimits;
use super::engine::{ExecutionContext, Graph, NodeId, NodeSelector as EngineSelector};
use super::read::{Direction, GraphLimits, NodeSelector};

/// Relationship types a scope follows. There is no default: a scope names
/// its types or `Any` explicitly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelationshipScope {
    Any,
    /// A non-empty list; duplicates are ignored.
    Only(Vec<String>),
}

/// Labels every scoped node, seeds included, must carry. There is no
/// default: a scope names its labels or `Any` explicitly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LabelScope {
    Any,
    /// A non-empty list (at most 8 labels); a node must carry all of them.
    All(Vec<String>),
}

/// One explicit local scope (see the [module docs](self)).
#[derive(Clone, Debug, PartialEq)]
pub struct GraphAlgorithmScope {
    /// 1..=`max_visited_nodes` distinct nodes after deduplication (at most
    /// 10,000 selectors).
    pub seeds: Vec<NodeSelector>,
    /// `Out` keeps stored edge orientation, `In` reverses it and `Both`
    /// walks every edge both ways (a self-loop once).
    pub direction: Direction,
    pub relationship_types: RelationshipScope,
    pub node_labels: LabelScope,
    /// Conditions every retained edge must match, with record-filter
    /// semantics.
    pub edge_conditions: Vec<Condition>,
}

/// The algorithm run on the extracted scope.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum GraphAlgorithm {
    /// Raw degrees of every scoped node over the retained physical edges
    /// (parallel edges count separately). `In` reverses endpoints; `Out`
    /// and `Both` keep the stored orientation. A self-loop adds one to
    /// each of in, out and incident degree.
    Degree,
    /// The cheapest path of at most `max_hops` edges from `source`, which
    /// must be a seed, to `destination` (a visible node, possibly outside
    /// the scope). Ties go to fewer hops, then to the lexicographically
    /// smallest edge-ID sequence; nodes may repeat. `weight_property` names
    /// an edge property holding a nonnegative integer up to 2^53 or a
    /// finite nonnegative float; every retained edge must carry a valid
    /// weight. `None` weighs every edge 1.
    WeightedShortestPath {
        source: NodeSelector,
        destination: NodeSelector,
        weight_property: Option<String>,
    },
}

/// A scope and the algorithm to run on it.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphAlgorithmRequest {
    pub scope: GraphAlgorithmScope,
    pub algorithm: GraphAlgorithm,
}

/// Per-call options. Limits above their ceilings fail with
/// `GraphLimitExceedsCeiling` and are never clamped. `cancel` is checked
/// before any work, before every 64th charged operation and before
/// returning.
#[derive(Clone, Debug, Default)]
pub struct GraphAlgorithmOptions<'a> {
    /// `max_hops` bounds both the scope radius and the path length;
    /// `max_visited_nodes`, `max_edges_examined`, `max_edges_per_node` and
    /// `deadline_ms` bound the whole run; the output caps apply to the
    /// finished result.
    pub limits: GraphLimits,
    pub algorithm_limits: GraphAlgorithmLimits,
    pub cancel: Option<&'a AtomicBool>,
}

/// Degree of one scoped node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct DegreeRow {
    pub node_id: u64,
    pub in_degree: u64,
    pub out_degree: u64,
    /// Retained edges touching the node; a self-loop counts once.
    pub incident_degree: u64,
}

/// A cheapest path: node IDs (one more than edges) and edge IDs in
/// traversal order.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct WeightedPath {
    /// The exact sum of the edge weights, rounded once to the nearest f64
    /// (ties to even).
    pub cost: f64,
    pub node_ids: Vec<u64>,
    pub edge_ids: Vec<u64>,
}

/// A certified weighted shortest-path answer. Both variants require a
/// complete extraction and computation; an interrupted search returns no
/// output instead.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum WeightedPathOutcome {
    Found(WeightedPath),
    NoPathWithinScopeAndHops,
}

/// The output of a finished computation.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum GraphAlgorithmOutput {
    /// One row per scoped node, ascending by node ID.
    Degree(Vec<DegreeRow>),
    WeightedShortestPath(WeightedPathOutcome),
}

/// Why an algorithm result is incomplete. A result reports at most one
/// reason: the first that stopped it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum GraphAlgorithmTruncation {
    /// Discovery would admit more than `max_visited_nodes` nodes.
    VisitedLimit,
    /// `max_edges_examined` was spent.
    EdgeLimit,
    /// The node whose adjacency exceeds `max_edges_per_node`.
    DegreeLimit {
        node_ids: Vec<u64>,
    },
    /// `max_vertex_operations` was spent.
    VertexLimit,
    /// A buffer would exceed `max_scratch_bytes`.
    MemoryLimit,
    /// The finished output exceeds an output cap: degree rows are cut to
    /// `max_output_nodes`; a path over the node, edge or path cap is
    /// withheld.
    OutputLimit,
    Deadline,
    Cancelled,
}

/// Work done by one run. Only `elapsed_ms` depends on timing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct GraphAlgorithmStats {
    /// Nodes and physical edges of the finished projection (0 when
    /// extraction stopped).
    pub nodes_extracted: u64,
    pub edges_extracted: u64,
    /// Computational arcs: one per retained edge for `Out` and `In`, two
    /// per non-loop edge for `Both`.
    pub arcs: u64,
    /// Edge units charged: adjacency entries (rejected and repeated ones
    /// included), retained edges and arcs.
    pub edges_examined: u64,
    pub vertex_operations: u64,
    /// Estimated bytes reserved before allocation, cumulative over the run
    /// (an upper bound on the scratch and output peak).
    pub scratch_bytes: u64,
    pub elapsed_ms: u64,
}

/// The result of [`GraphRef::algorithm`].
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct GraphAlgorithmRead {
    /// `None` when extraction or computation stopped (or a path exceeds an
    /// output cap); never an analysis of a partial scope.
    pub output: Option<GraphAlgorithmOutput>,
    /// True exactly when `truncation` is empty.
    pub complete: bool,
    pub truncation: Vec<GraphAlgorithmTruncation>,
    /// Extraction finished. Local completeness never means global coverage.
    pub scope_complete: bool,
    /// The computation finished, output caps aside.
    pub computation_complete: bool,
    pub view_sequence: u64,
    /// The TTL clock every visibility decision used.
    pub read_time_unix_ms: u64,
    /// Version of the algorithm semantics (1).
    pub algorithm_version: u32,
    /// The normalized request: seeds as ascending distinct node IDs, path
    /// endpoints as node IDs, relationship types and labels sorted and
    /// deduplicated.
    pub request: GraphAlgorithmRequest,
    /// The hop bound used for the scope and paths.
    pub max_hops: u64,
    pub stats: GraphAlgorithmStats,
}

/// Why [`GraphRef::algorithm`] failed. Invalid inputs and numeric failures
/// are errors, never truncations.
#[derive(Debug)]
#[non_exhaustive]
pub enum GraphAlgorithmError {
    /// Invalid limits, scope, selectors or conditions
    /// (`GraphLimitExceedsCeiling`, `InvalidQuery`, `InvalidFilter`), or a
    /// seed or path endpoint that is missing or not visible
    /// (`GraphNodeNotFound`).
    Read(Error),
    /// A retained edge lacks the weight property (the smallest such edge ID).
    MissingWeight { edge_id: u64 },
    /// A retained edge's weight is negative, non-finite, an integer above
    /// 2^53 or not a number (the smallest such edge ID).
    InvalidWeight { edge_id: u64, reason: &'static str },
    /// The winning path cost exceeds the largest finite f64.
    NumericOverflow,
}

impl fmt::Display for GraphAlgorithmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(error) => error.fmt(f),
            Self::MissingWeight { edge_id } => {
                write!(f, "graph edge {edge_id} has no weight property")
            }
            Self::InvalidWeight { edge_id, reason } => {
                write!(f, "invalid weight on graph edge {edge_id}: {reason}")
            }
            Self::NumericOverflow => write!(f, "path cost exceeds the largest finite f64"),
        }
    }
}

impl std::error::Error for GraphAlgorithmError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(error) => Some(error),
            _ => None,
        }
    }
}

impl From<Error> for GraphAlgorithmError {
    fn from(error: Error) -> Self {
        Self::Read(error)
    }
}

impl GraphRef<'_> {
    /// Runs one bounded local algorithm (see [`GraphAlgorithmRequest`]).
    /// Truncation returns `Ok` with `complete == false`; invalid input,
    /// invalid weights and numeric overflow return `Err`.
    pub fn algorithm(
        &self,
        request: &GraphAlgorithmRequest,
        options: &GraphAlgorithmOptions<'_>,
    ) -> Result<GraphAlgorithmRead, GraphAlgorithmError> {
        self.algorithm_with_clock(request, options, &super::read::clock())
    }

    /// [`GraphRef::algorithm`] with `clock` driving the deadline.
    fn algorithm_with_clock(
        &self,
        request: &GraphAlgorithmRequest,
        options: &GraphAlgorithmOptions<'_>,
        clock: &dyn Fn() -> u64,
    ) -> Result<GraphAlgorithmRead, GraphAlgorithmError> {
        run(
            self.graph,
            self.view_sequence,
            self.now(),
            request,
            options,
            clock,
        )
    }
}

/// Runs `request` on `graph` at TTL clock `now_ms`.
fn run(
    graph: &Graph,
    view_sequence: u64,
    now_ms: u64,
    request: &GraphAlgorithmRequest,
    options: &GraphAlgorithmOptions<'_>,
    clock: &dyn Fn() -> u64,
) -> Result<GraphAlgorithmRead, GraphAlgorithmError> {
    // Limits first, as every read does, then conditions.
    options.limits.validate().map_err(graph_error)?;
    options.algorithm_limits.validate().map_err(graph_error)?;
    let (scope, algorithm) = engine_request(request)?;
    let context = ExecutionContext {
        view_sequence,
        clock,
        cancel: options.cancel,
        now_ms,
    };
    let result = kernel::run(
        graph,
        &scope,
        &algorithm,
        &options.limits,
        &options.algorithm_limits,
        &context,
    )
    .map_err(|error| match error {
        kernel::AlgorithmError::Graph(error) => GraphAlgorithmError::Read(graph_error(error)),
        kernel::AlgorithmError::MissingWeight(edge) => {
            GraphAlgorithmError::MissingWeight { edge_id: edge.0 }
        }
        kernel::AlgorithmError::InvalidWeight { edge, reason } => {
            GraphAlgorithmError::InvalidWeight {
                edge_id: edge.0,
                reason,
            }
        }
        kernel::AlgorithmError::NumericOverflow => GraphAlgorithmError::NumericOverflow,
    })?;

    let id = |node: NodeId| NodeSelector::Id(node.0);
    let algorithm = match (&request.algorithm, result.endpoints) {
        (GraphAlgorithm::Degree, _) => GraphAlgorithm::Degree,
        (
            GraphAlgorithm::WeightedShortestPath {
                weight_property, ..
            },
            Some((source, destination)),
        ) => GraphAlgorithm::WeightedShortestPath {
            source: id(source),
            destination: id(destination),
            weight_property: weight_property.clone(),
        },
        (GraphAlgorithm::WeightedShortestPath { .. }, None) => unreachable!("endpoints resolve"),
    };
    let request = GraphAlgorithmRequest {
        scope: GraphAlgorithmScope {
            seeds: result.seeds.iter().copied().map(id).collect(),
            direction: scope.direction,
            relationship_types: scope.types.map_or(RelationshipScope::Any, |types| {
                RelationshipScope::Only(types.into_iter().collect())
            }),
            node_labels: scope.labels.map_or(LabelScope::Any, |labels| {
                LabelScope::All(labels.into_iter().collect())
            }),
            edge_conditions: request.scope.edge_conditions.clone(),
        },
        algorithm,
    };
    let stats = result.stats;
    Ok(GraphAlgorithmRead {
        output: result.output.map(output),
        complete: result.complete,
        truncation: result.truncation.into_iter().map(truncation).collect(),
        scope_complete: result.scope_complete,
        computation_complete: result.computation_complete,
        view_sequence: result.view_sequence,
        read_time_unix_ms: result.read_time_ms,
        algorithm_version: kernel::ALGORITHM_VERSION,
        request,
        max_hops: options.limits.max_hops,
        stats: GraphAlgorithmStats {
            nodes_extracted: stats.nodes_extracted,
            edges_extracted: stats.edges_extracted,
            arcs: stats.arcs,
            edges_examined: stats.edges_examined,
            vertex_operations: stats.vertex_operations,
            scratch_bytes: stats.scratch_bytes,
            elapsed_ms: stats.elapsed_ms,
        },
    })
}

/// The engine scope and algorithm of `request`; conditions are validated
/// with the record-filter rules.
fn engine_request(
    request: &GraphAlgorithmRequest,
) -> Result<(kernel::AlgorithmScope, kernel::Algorithm), Error> {
    let scope = &request.scope;
    let scope = kernel::AlgorithmScope {
        seeds: scope.seeds.iter().map(node_selector).collect(),
        direction: scope.direction,
        types: match &scope.relationship_types {
            RelationshipScope::Any => None,
            RelationshipScope::Only(types) => Some(types.iter().cloned().collect()),
        },
        labels: match &scope.node_labels {
            LabelScope::Any => None,
            LabelScope::All(labels) => Some(labels.iter().cloned().collect()),
        },
        edge_conditions: property_conditions(&scope.edge_conditions)?,
    };
    let algorithm = match &request.algorithm {
        GraphAlgorithm::Degree => kernel::Algorithm::Degree,
        GraphAlgorithm::WeightedShortestPath {
            source,
            destination,
            weight_property,
        } => kernel::Algorithm::WeightedShortestPath {
            source: node_selector(source),
            destination: node_selector(destination),
            weight_property: weight_property.clone(),
        },
    };
    Ok((scope, algorithm))
}

fn node_selector(selector: &NodeSelector) -> EngineSelector {
    match selector {
        NodeSelector::Id(id) => EngineSelector::Id(NodeId(*id)),
        NodeSelector::Key(key) => EngineSelector::Key(key.clone()),
    }
}

fn output(output: kernel::AlgorithmOutput) -> GraphAlgorithmOutput {
    match output {
        kernel::AlgorithmOutput::Degree(rows) => GraphAlgorithmOutput::Degree(
            rows.into_iter()
                .map(|row| DegreeRow {
                    node_id: row.node.0,
                    in_degree: row.in_degree,
                    out_degree: row.out_degree,
                    incident_degree: row.incident_degree,
                })
                .collect(),
        ),
        kernel::AlgorithmOutput::WeightedShortestPath(outcome) => {
            GraphAlgorithmOutput::WeightedShortestPath(match outcome {
                kernel::WeightedPathOutcome::Found(path) => {
                    WeightedPathOutcome::Found(WeightedPath {
                        cost: path.cost,
                        node_ids: path.nodes.into_iter().map(|node| node.0).collect(),
                        edge_ids: path.edges.into_iter().map(|edge| edge.0).collect(),
                    })
                }
                kernel::WeightedPathOutcome::NoPathWithinScopeAndHops => {
                    WeightedPathOutcome::NoPathWithinScopeAndHops
                }
            })
        }
    }
}

fn truncation(reason: kernel::AlgorithmTruncation) -> GraphAlgorithmTruncation {
    use kernel::AlgorithmTruncation as Engine;
    match reason {
        Engine::VisitedLimit => GraphAlgorithmTruncation::VisitedLimit,
        Engine::EdgeLimit => GraphAlgorithmTruncation::EdgeLimit,
        Engine::DegreeLimit { node_ids } => GraphAlgorithmTruncation::DegreeLimit {
            node_ids: node_ids.into_iter().map(|node| node.0).collect(),
        },
        Engine::VertexLimit => GraphAlgorithmTruncation::VertexLimit,
        Engine::MemoryLimit => GraphAlgorithmTruncation::MemoryLimit,
        Engine::OutputLimit => GraphAlgorithmTruncation::OutputLimit,
        Engine::Deadline => GraphAlgorithmTruncation::Deadline,
        Engine::Cancelled => GraphAlgorithmTruncation::Cancelled,
    }
}

#[cfg(test)]
mod tests;
