//! Bounded local graph algorithms on [`GraphRef`] (Gap 5 slices S1 and S2).
//!
//! [`GraphRef::algorithm`] extracts one explicit local scope and runs one
//! algorithm on it, within the read limits ([`GraphLimits`]) plus the
//! algorithm caps ([`GraphAlgorithmLimits`]):
//!
//! - Scope ([`GraphAlgorithmScope`]): 1..=`max_visited_nodes` distinct seeds
//!   of this graph (at most 10,000 selectors), a direction, explicit
//!   relationship types (at most 256) or [`RelationshipScope::Any`], an
//!   all-labels node filter (at most 8) or [`LabelScope::Any`], and edge
//!   property conditions (record-filter semantics, at most 64). Seeds must
//!   be visible and carry every scope label. Breadth-first discovery admits
//!   eligible nodes up to `max_hops` hops from the seeds, one level at a
//!   time; then every admitted node's adjacency is scanned again and every
//!   eligible edge between admitted nodes is retained (an induced
//!   projection). Excluded nodes never bridge the scope.
//! - Visibility: one TTL clock (the handle's, see [`GraphRef::with_clock`])
//!   decides every lookup; due nodes and edges are invisible and a due node
//!   hides its edges.
//! - Budgets: the list lengths and limits are checked before any work.
//!   Everything else, including condition conversion, seed and endpoint
//!   resolution, relationship-type probes that find nothing, extraction,
//!   rescans, rejected edges, weight validation, the computation and the
//!   building of the public result, shares `max_edges_examined`,
//!   `max_vertex_operations`, `max_scratch_bytes` (every buffer at its
//!   allocated capacity, reserved before allocation), the deadline and
//!   cancellation, checked before any work, before every 64th charged
//!   operation and before returning. Any stop before the result was built
//!   returns no output. An invalid or missing item is reported only if the
//!   run reaches it.
//! - Results ([`GraphAlgorithmRead`]): rows ascend by node ID, paths keep
//!   traversal order, and the view sequence and TTL clock are echoed, with
//!   the normalized request when the computation finished. Output caps
//!   apply to finished results.
//!
//! Embedded callers choose the graph; nothing follows attachments or
//! returns properties or vectors, and there are no continuation cursors.

use std::borrow::Cow;
use std::fmt;
use std::mem::size_of;
use std::sync::atomic::AtomicBool;

use crate::{Condition, Error, MetadataValue, metadata};

use super::GraphRef;
use super::convert::graph_error;
use super::engine::algorithm as kernel;
pub use super::engine::algorithm::GraphAlgorithmLimits;
use super::engine::{ExecutionContext, Graph, NodeId, PropertyCondition, PropertyValue};
use super::read::{Direction, GraphLimits, NodeSelector};

/// Relationship types a scope follows. There is no default: a scope names
/// its types or `Any` explicitly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelationshipScope {
    Any,
    /// A non-empty list of at most 256 entries; duplicates are ignored.
    Only(Vec<String>),
}

/// Labels every scoped node, seeds included, must carry. There is no
/// default: a scope names its labels or `Any` explicitly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LabelScope {
    Any,
    /// A non-empty list of at most 8 entries (duplicates count); a node
    /// must carry all of them.
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
    /// Vertex operations and scratch bytes of the whole run, from resolving
    /// the request to building this result.
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
    /// The computation and the public result finished, output caps aside.
    pub computation_complete: bool,
    pub view_sequence: u64,
    /// The TTL clock every visibility decision used.
    pub read_time_unix_ms: u64,
    /// Version of the algorithm semantics (1).
    pub algorithm_version: u32,
    /// The normalized request: seeds as ascending distinct node IDs, path
    /// endpoints as node IDs, relationship types and labels sorted and
    /// deduplicated. It is built under the run's budgets, so it is `Some`
    /// exactly when `computation_complete` is true.
    pub request: Option<GraphAlgorithmRequest>,
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
///
/// Limits, then the condition shape (record-filter rules), then the engine's
/// shape checks run before any work; the request is read in place, and its
/// condition conversion, the public output and the request echo are charged
/// to the run's meter.
fn run(
    graph: &Graph,
    view_sequence: u64,
    now_ms: u64,
    request: &GraphAlgorithmRequest,
    options: &GraphAlgorithmOptions<'_>,
    clock: &dyn Fn() -> u64,
) -> Result<GraphAlgorithmRead, GraphAlgorithmError> {
    options.limits.validate().map_err(graph_error)?;
    options.algorithm_limits.validate().map_err(graph_error)?;
    let conditions = &request.scope.edge_conditions;
    metadata::validate_filter(conditions)?;
    let context = ExecutionContext {
        view_sequence,
        clock,
        cancel: options.cancel,
        now_ms,
    };
    let (result, staged) = kernel::run_with(
        graph,
        &borrowed(request),
        &options.limits,
        &options.algorithm_limits,
        &context,
        |meter| property_conditions(conditions, meter).map(Cow::Owned),
        |finished, meter| stage(request, finished, meter),
    )
    .map_err(algorithm_error)?;
    let (output, request) = match staged {
        Some((output, request)) => (output, Some(request)),
        None => (None, None),
    };
    let stats = result.stats;
    Ok(GraphAlgorithmRead {
        output,
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

/// The engine's view of `request`, borrowed in place.
fn borrowed(request: &GraphAlgorithmRequest) -> kernel::Request<'_, NodeSelector> {
    let scope = &request.scope;
    kernel::Request {
        seeds: &scope.seeds,
        direction: scope.direction,
        types: match &scope.relationship_types {
            RelationshipScope::Any => None,
            RelationshipScope::Only(types) => Some(types),
        },
        labels: match &scope.node_labels {
            LabelScope::Any => None,
            LabelScope::All(labels) => Some(labels),
        },
        path: match &request.algorithm {
            GraphAlgorithm::Degree => None,
            GraphAlgorithm::WeightedShortestPath {
                source,
                destination,
                weight_property,
            } => Some(kernel::PathRequest {
                source,
                destination,
                weight_property: weight_property.as_deref(),
            }),
        },
    }
}

impl kernel::Seed for NodeSelector {
    fn selector(&self) -> kernel::Selector<'_> {
        match self {
            Self::Id(id) => kernel::Selector::Id(NodeId(*id)),
            Self::Key(key) => kernel::Selector::Key(key),
        }
    }
}

fn algorithm_error(error: kernel::AlgorithmError) -> GraphAlgorithmError {
    match error {
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
    }
}

/// A copy of `text` at exactly its length.
fn copy(text: &str) -> String {
    let mut copy = String::with_capacity(text.len());
    copy.push_str(text);
    copy
}

/// Heap bytes a copy of `condition` allocates for its strings.
fn condition_bytes(condition: &Condition) -> usize {
    let text = |value: &MetadataValue| match value {
        MetadataValue::String(text) => text.len(),
        _ => 0,
    };
    match condition {
        Condition::Eq { field, value } => field.len() + text(value),
        Condition::Range { field, min, max } => {
            field.len() + min.iter().chain(max).map(text).sum::<usize>()
        }
        Condition::GeoRadius { field, .. } => field.len(),
    }
}

/// The engine form of a property value; strings are copied at their length.
fn property_value(value: &MetadataValue) -> PropertyValue {
    match value {
        MetadataValue::String(text) => PropertyValue::String(copy(text)),
        MetadataValue::Integer(value) => PropertyValue::Integer(*value),
        MetadataValue::Float(value) => PropertyValue::Float(*value),
        MetadataValue::Boolean(value) => PropertyValue::Boolean(*value),
        MetadataValue::GeoPoint {
            latitude,
            longitude,
        } => PropertyValue::GeoPoint {
            latitude: *latitude,
            longitude: *longitude,
        },
    }
}

/// The engine conditions of the validated `conditions`, one vertex unit
/// each, every buffer reserved first.
fn property_conditions(
    conditions: &[Condition],
    meter: &mut kernel::Meter<'_>,
) -> Result<Vec<PropertyCondition>, kernel::AlgorithmTruncation> {
    meter.reserve(conditions.len(), size_of::<PropertyCondition>())?;
    let mut converted = Vec::with_capacity(conditions.len());
    for condition in conditions {
        meter.vertex()?;
        meter.reserve(condition_bytes(condition), 1)?;
        converted.push(match condition {
            Condition::Eq { field, value } => PropertyCondition::Eq {
                field: copy(field),
                value: property_value(value),
            },
            Condition::Range { field, min, max } => PropertyCondition::Range {
                field: copy(field),
                min: min.as_ref().map(property_value),
                max: max.as_ref().map(property_value),
            },
            Condition::GeoRadius {
                field,
                latitude,
                longitude,
                radius_meters,
            } => PropertyCondition::GeoRadius {
                field: copy(field),
                latitude: *latitude,
                longitude: *longitude,
                radius_meters: *radius_meters,
            },
        });
    }
    Ok(converted)
}

/// The public output and the normalized request echo of a finished run,
/// one vertex unit per row, ID, seed, name and condition, every buffer
/// reserved first.
fn stage(
    request: &GraphAlgorithmRequest,
    finished: &kernel::Finished<'_>,
    meter: &mut kernel::Meter<'_>,
) -> Result<(Option<GraphAlgorithmOutput>, GraphAlgorithmRequest), kernel::AlgorithmTruncation> {
    let output = match finished.output {
        None => None,
        Some(output) => Some(public_output(output, meter)?),
    };
    let id = |node: NodeId| NodeSelector::Id(node.0);
    meter.reserve(finished.seeds.len(), size_of::<NodeSelector>())?;
    meter.vertices(finished.seeds.len() as u64)?;
    let mut seeds = Vec::with_capacity(finished.seeds.len());
    seeds.extend(finished.seeds.iter().map(|&node| id(node)));
    let relationship_types = match finished.types {
        None => RelationshipScope::Any,
        Some(types) => RelationshipScope::Only(strings(types, meter)?),
    };
    let node_labels = match finished.labels {
        None => LabelScope::Any,
        Some(labels) => LabelScope::All(strings(labels, meter)?),
    };
    let scope = &request.scope;
    meter.reserve(scope.edge_conditions.len(), size_of::<Condition>())?;
    let mut edge_conditions = Vec::with_capacity(scope.edge_conditions.len());
    for condition in &scope.edge_conditions {
        meter.vertex()?;
        meter.reserve(condition_bytes(condition), 1)?;
        edge_conditions.push(condition.clone());
    }
    let algorithm = match (&request.algorithm, finished.endpoints) {
        (GraphAlgorithm::Degree, _) => GraphAlgorithm::Degree,
        (
            GraphAlgorithm::WeightedShortestPath {
                weight_property, ..
            },
            Some((source, destination)),
        ) => {
            meter.reserve(weight_property.as_ref().map_or(0, String::len), 1)?;
            GraphAlgorithm::WeightedShortestPath {
                source: id(source),
                destination: id(destination),
                weight_property: weight_property.as_deref().map(copy),
            }
        }
        (GraphAlgorithm::WeightedShortestPath { .. }, None) => unreachable!("endpoints resolve"),
    };
    let request = GraphAlgorithmRequest {
        scope: GraphAlgorithmScope {
            seeds,
            direction: scope.direction,
            relationship_types,
            node_labels,
            edge_conditions,
        },
        algorithm,
    };
    Ok((output, request))
}

/// Copies of sorted scope names, one vertex unit each.
fn strings(
    names: &[&str],
    meter: &mut kernel::Meter<'_>,
) -> Result<Vec<String>, kernel::AlgorithmTruncation> {
    meter.reserve(names.len(), size_of::<String>())?;
    let mut copies = Vec::with_capacity(names.len());
    for name in names {
        meter.vertex()?;
        meter.reserve(name.len(), 1)?;
        copies.push(copy(name));
    }
    Ok(copies)
}

/// The public form of a finished output, one vertex unit per row or ID.
fn public_output(
    output: &kernel::AlgorithmOutput,
    meter: &mut kernel::Meter<'_>,
) -> Result<GraphAlgorithmOutput, kernel::AlgorithmTruncation> {
    Ok(match output {
        kernel::AlgorithmOutput::Degree(rows) => {
            meter.reserve(rows.len(), size_of::<DegreeRow>())?;
            meter.vertices(rows.len() as u64)?;
            let mut public = Vec::with_capacity(rows.len());
            public.extend(rows.iter().map(|row| DegreeRow {
                node_id: row.node.0,
                in_degree: row.in_degree,
                out_degree: row.out_degree,
                incident_degree: row.incident_degree,
            }));
            GraphAlgorithmOutput::Degree(public)
        }
        kernel::AlgorithmOutput::WeightedShortestPath(kernel::WeightedPathOutcome::Found(path)) => {
            let ids = path.nodes.len() + path.edges.len();
            meter.reserve(ids, size_of::<u64>())?;
            meter.vertices(ids as u64)?;
            let mut node_ids = Vec::with_capacity(path.nodes.len());
            node_ids.extend(path.nodes.iter().map(|node| node.0));
            let mut edge_ids = Vec::with_capacity(path.edges.len());
            edge_ids.extend(path.edges.iter().map(|edge| edge.0));
            GraphAlgorithmOutput::WeightedShortestPath(WeightedPathOutcome::Found(WeightedPath {
                cost: path.cost,
                node_ids,
                edge_ids,
            }))
        }
        kernel::AlgorithmOutput::WeightedShortestPath(
            kernel::WeightedPathOutcome::NoPathWithinScopeAndHops,
        ) => GraphAlgorithmOutput::WeightedShortestPath(
            WeightedPathOutcome::NoPathWithinScopeAndHops,
        ),
    })
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
