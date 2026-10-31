//! Bounded local graph algorithms (Gap 5 slices S1 and S2).
//!
//! [`run`] extracts one explicit local scope and runs one algorithm on it:
//!
//! - Scope: 1..=`max_visited_nodes` distinct seeds (resolved, deduplicated
//!   and sorted; every seed must be visible and carry every scope label), a
//!   direction, an explicit relationship-type set or any type, an all-labels
//!   node filter or any label, and edge property conditions. Breadth-first
//!   discovery from the ascending seeds admits eligible nodes up to
//!   `max_hops` hops away; then every admitted node's adjacency is scanned
//!   again to retain every eligible edge between admitted endpoints (an
//!   induced projection). Visibility is decided once at `now_ms`: due nodes
//!   and edges are invisible and a due endpoint hides its edges. Excluded
//!   nodes never bridge the scope.
//! - Budgets: discovery, rescans, rejected edges, weight validation and every
//!   computational pass share one meter. Adjacency entries, retained edges
//!   and arcs charge `max_edges_examined`; seed resolution, sorting, node
//!   admissions, node loops, state initialization and heap actions charge
//!   `max_vertex_operations`; buffers are reserved against
//!   `max_scratch_bytes` before they are allocated. Cancellation and the
//!   deadline are checked before any work, before every 64th charged
//!   operation (edge and vertex units together) and once more before
//!   returning.
//! - Results: any truncation before the computation finished returns no
//!   output, never an analysis of a prefix. Output caps apply afterwards.
//!
//! Algorithms: [`Algorithm::Degree`] (raw degrees over retained physical
//! edges) and [`Algorithm::WeightedShortestPath`] (Dijkstra over
//! (node, hops) states with exact sums).

use super::error::GraphError;
use super::filter::{conditions_match, validate_conditions};
use super::limits::{CHECK_INTERVAL, GraphLimits, MAX_FIELD_NAME_BYTES};
use super::meter::ExecutionContext;
use super::model::{
    Direction, EdgeId, GraphEdge, GraphNode, NodeId, PropertyCondition, PropertyValue,
    validate_labels, validate_node_key, validate_relationship_type,
};
use super::query::NodeSelector;
use super::state::Graph;
use super::traverse::other_end;
use std::cmp::Ordering;
use std::collections::{BTreeSet, VecDeque};
use std::mem::size_of;
use std::sync::atomic::Ordering as AtomicOrdering;

/// Version of the algorithm semantics reported with every result.
pub const ALGORITHM_VERSION: u32 = 1;

/// The most raw seed selectors one request may name.
const MAX_SEEDS: u64 = GraphLimits::CEILING.max_visited_nodes;
/// Edge-ID sequence slots per path state (`GraphLimits::CEILING.max_hops`).
const MAX_PATH_HOPS: usize = 4;
const _: () = assert!(GraphLimits::CEILING.max_hops as usize == MAX_PATH_HOPS);
/// Estimated bytes of one admitted node: its ordered-set entry, its queue
/// slot and its slot in the sorted node list.
const NODE_BYTES: usize = 48;
/// Estimated fixed bytes of one path state besides its cost limbs.
const STATE_BYTES: usize = size_of::<[EdgeId; MAX_PATH_HOPS]>() + 3 * size_of::<usize>();

/// Caps an algorithm adds to [`GraphLimits`]. Values above
/// [`GraphAlgorithmLimits::CEILING`] are rejected, never clamped, and both
/// must be at least 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct GraphAlgorithmLimits {
    /// Vertex operations: seed resolutions, sort steps, node admissions and
    /// node loops, state initialization and heap actions.
    pub max_vertex_operations: u64,
    /// Estimated bytes of every scratch and output buffer, reserved before
    /// the buffer is allocated. Reservations accumulate, so this bounds the
    /// peak.
    pub max_scratch_bytes: u64,
}

impl GraphAlgorithmLimits {
    pub const DEFAULT: Self = Self {
        max_vertex_operations: 100_000,
        max_scratch_bytes: 64 << 20,
    };

    pub const CEILING: Self = Self {
        max_vertex_operations: 1_000_000,
        max_scratch_bytes: 256 << 20,
    };

    /// Rejects the first cap above its ceiling, then zero caps.
    pub(crate) fn validate(&self) -> Result<(), GraphError> {
        let fields = |limits: &Self| {
            [
                ("max_vertex_operations", limits.max_vertex_operations),
                ("max_scratch_bytes", limits.max_scratch_bytes),
            ]
        };
        for ((limit, requested), (_, ceiling)) in
            fields(self).into_iter().zip(fields(&Self::CEILING))
        {
            if requested > ceiling {
                return Err(GraphError::LimitExceedsCeiling {
                    limit,
                    requested,
                    ceiling,
                });
            }
        }
        if self.max_vertex_operations == 0 {
            return Err(GraphError::InvalidQuery(
                "max_vertex_operations must be at least 1",
            ));
        }
        if self.max_scratch_bytes == 0 {
            return Err(GraphError::InvalidQuery(
                "max_scratch_bytes must be at least 1",
            ));
        }
        Ok(())
    }
}

impl Default for GraphAlgorithmLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// One explicit local scope. `None` means any relationship type or any
/// label; `Some` sets must not be empty.
#[derive(Clone, Debug, PartialEq)]
pub struct AlgorithmScope {
    pub seeds: Vec<NodeSelector>,
    pub direction: Direction,
    pub types: Option<BTreeSet<String>>,
    /// Labels every scoped node, seeds included, must carry.
    pub labels: Option<BTreeSet<String>>,
    pub edge_conditions: Vec<PropertyCondition>,
}

/// The algorithm run on the extracted projection.
#[derive(Clone, Debug, PartialEq)]
pub enum Algorithm {
    /// Raw in, out and incident degree of every scoped node over retained
    /// physical edges. `In` reverses endpoints; `Out` and `Both` keep the
    /// stored orientation. A self-loop adds one to each count.
    Degree,
    /// The cheapest path of at most `max_hops` edges from a seed `source`
    /// to `destination`; ties go to fewer hops, then to the
    /// lexicographically smallest edge-ID sequence. `weight_property` names
    /// a nonnegative integer or float edge property; `None` weighs every
    /// edge 1.
    WeightedShortestPath {
        source: NodeSelector,
        destination: NodeSelector,
        weight_property: Option<String>,
    },
}

/// Why a result is incomplete. Results list each reason at most once, in
/// declaration order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AlgorithmTruncation {
    /// Discovery would admit more than `max_visited_nodes` nodes.
    VisitedLimit,
    /// `max_edges_examined` was spent.
    EdgeLimit,
    /// A node's adjacency has more than `max_edges_per_node` entries; the
    /// node whose scan stopped.
    DegreeLimit {
        node_ids: Vec<NodeId>,
    },
    /// `max_vertex_operations` was spent.
    VertexLimit,
    /// A buffer would exceed `max_scratch_bytes`.
    MemoryLimit,
    /// The finished output exceeds an output cap.
    OutputLimit,
    Deadline,
    Cancelled,
}

/// Degree of one scoped node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DegreeRow {
    pub node: NodeId,
    pub in_degree: u64,
    pub out_degree: u64,
    /// Retained edges touching the node; a self-loop counts once.
    pub incident_degree: u64,
}

/// A cheapest path; `nodes` has one more entry than `edges`.
#[derive(Clone, Debug, PartialEq)]
pub struct WeightedPath {
    /// The exact sum of the edge weights, rounded once to the nearest f64
    /// (ties to even).
    pub cost: f64,
    pub nodes: Vec<NodeId>,
    pub edges: Vec<EdgeId>,
}

/// A certified answer of [`Algorithm::WeightedShortestPath`].
#[derive(Clone, Debug, PartialEq)]
pub enum WeightedPathOutcome {
    Found(WeightedPath),
    /// The complete computation found no path within the scope and the hop
    /// bound.
    NoPathWithinScopeAndHops,
}

/// The output of a finished computation.
#[derive(Clone, Debug, PartialEq)]
pub enum AlgorithmOutput {
    /// One row per scoped node, ascending by node ID.
    Degree(Vec<DegreeRow>),
    WeightedShortestPath(WeightedPathOutcome),
}

/// Work done by one run. Only `elapsed_ms` depends on timing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AlgorithmStats {
    /// Nodes and physical edges of the finished projection (0 when
    /// extraction stopped).
    pub nodes_extracted: u64,
    pub edges_extracted: u64,
    /// Computational arcs: one per retained edge for `Out` and `In`, two
    /// per non-loop edge for `Both`.
    pub arcs: u64,
    /// Edge units charged (adjacency entries, retained edges and arcs).
    pub edges_examined: u64,
    pub vertex_operations: u64,
    /// Estimated bytes reserved before allocation, cumulative over the run
    /// (an upper bound on the scratch and output peak).
    pub scratch_bytes: u64,
    pub elapsed_ms: u64,
}

/// One retained physical edge; endpoints are positions in
/// [`Projection::nodes`], in stored orientation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProjectedEdge {
    pub id: EdgeId,
    pub source: usize,
    pub destination: usize,
}

/// The induced local projection: admitted nodes ascending, retained edges
/// ascending by ID.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Projection {
    pub nodes: Vec<NodeId>,
    pub edges: Vec<ProjectedEdge>,
}

/// The result of [`run`]. `complete` is true exactly when `truncation` is
/// empty; `output` is `None` whenever extraction or computation stopped.
#[derive(Clone, Debug, PartialEq)]
pub struct AlgorithmResult {
    pub output: Option<AlgorithmOutput>,
    pub complete: bool,
    pub truncation: Vec<AlgorithmTruncation>,
    /// Extraction finished.
    pub scope_complete: bool,
    /// The computation finished and passed the final deadline and
    /// cancellation check (output caps aside).
    pub computation_complete: bool,
    pub view_sequence: u64,
    /// The TTL clock every visibility decision used.
    pub read_time_ms: u64,
    /// The resolved seeds, deduplicated and ascending.
    pub seeds: Vec<NodeId>,
    /// The resolved path source and destination.
    pub endpoints: Option<(NodeId, NodeId)>,
    /// The finished projection.
    pub projection: Option<Projection>,
    pub stats: AlgorithmStats,
}

/// Why [`run`] failed. Invalid inputs and numeric failures are errors, not
/// truncations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AlgorithmError {
    /// Limits, scope, selectors or conditions are invalid, or a seed or
    /// path endpoint does not resolve.
    Graph(GraphError),
    /// A retained edge lacks the weight property (the smallest such edge ID).
    MissingWeight(EdgeId),
    /// A retained edge's weight is not a finite nonnegative number within
    /// the exact integer range.
    InvalidWeight { edge: EdgeId, reason: &'static str },
    /// The winning path cost exceeds the largest finite f64.
    NumericOverflow,
}

impl From<GraphError> for AlgorithmError {
    fn from(error: GraphError) -> Self {
        Self::Graph(error)
    }
}

/// Runs `algorithm` on the local scope `scope` of `graph`.
///
/// Limits, caps, the scope and the algorithm are validated and the seeds
/// and path endpoints resolved before the meter starts; resolution is
/// charged right after the start check. Truncation returns `Ok` with no
/// output; invalid input and weights return `Err`.
pub fn run(
    graph: &Graph,
    scope: &AlgorithmScope,
    algorithm: &Algorithm,
    limits: &GraphLimits,
    caps: &GraphAlgorithmLimits,
    context: &ExecutionContext<'_>,
) -> Result<AlgorithmResult, AlgorithmError> {
    limits.validate()?;
    caps.validate()?;
    validate(scope, algorithm)?;
    let expired_at = graph.has_due(context.now_ms).then_some(context.now_ms);
    let mut seeds = Vec::with_capacity(scope.seeds.len());
    for selector in &scope.seeds {
        let node = resolve(graph, selector, expired_at)?;
        if scope
            .labels
            .as_ref()
            .is_some_and(|labels| !labels.is_subset(&node.labels))
        {
            return Err(GraphError::InvalidQuery("every seed must carry every scope label").into());
        }
        seeds.push(node.id);
    }
    seeds.sort_unstable();
    seeds.dedup();
    if seeds.len() as u64 > limits.max_visited_nodes {
        return Err(GraphError::InvalidQuery("distinct seeds exceed max_visited_nodes").into());
    }
    let endpoints = match algorithm {
        Algorithm::Degree => None,
        Algorithm::WeightedShortestPath {
            source,
            destination,
            ..
        } => {
            let source = resolve(graph, source, expired_at)?.id;
            if seeds.binary_search(&source).is_err() {
                return Err(GraphError::InvalidQuery("the path source must be a seed").into());
            }
            Some((source, resolve(graph, destination, expired_at)?.id))
        }
    };

    let (mut meter, begin) = Meter::start(context, limits, caps);
    let mut projection = None;
    let local = Local {
        graph,
        scope,
        limits,
        expired_at,
    };
    let computed = begin.map_err(Halt::from).and_then(|()| {
        // Resolution: one unit per raw selector and endpoint, then the sort.
        let raw = scope.seeds.len();
        meter.reserve(raw, size_of::<NodeId>())?;
        meter.vertices(raw as u64 + 2 * u64::from(endpoints.is_some()))?;
        meter.vertices(sort_cost(raw))?;
        let extracted = projection.insert(local.extract(&seeds, &mut meter)?);
        match (algorithm, endpoints) {
            (Algorithm::Degree, _) => Ok(AlgorithmOutput::Degree(degree(
                extracted,
                scope.direction,
                &mut meter,
            )?)),
            (
                Algorithm::WeightedShortestPath {
                    weight_property, ..
                },
                Some(endpoints),
            ) => Ok(AlgorithmOutput::WeightedShortestPath(local.shortest_path(
                extracted,
                weight_property.as_deref(),
                endpoints,
                &mut meter,
            )?)),
            (Algorithm::WeightedShortestPath { .. }, None) => unreachable!("endpoints resolve"),
        }
    });

    let mut truncation = Vec::new();
    let mut output = None;
    let elapsed_ms = match computed {
        Err(Halt::Failed(error)) => return Err(error),
        Err(Halt::Truncated(reason)) => {
            truncation.push(reason);
            meter.elapsed_ms()
        }
        Ok(finished) => {
            let (elapsed_ms, check) = meter.finish();
            match check {
                Ok(()) => output = Some(finished),
                Err(reason) => truncation.push(reason),
            }
            elapsed_ms
        }
    };
    let computation_complete = output.is_some();
    match &mut output {
        Some(AlgorithmOutput::Degree(rows)) if rows.len() as u64 > limits.max_output_nodes => {
            rows.truncate(limits.max_output_nodes as usize);
            truncation.push(AlgorithmTruncation::OutputLimit);
        }
        Some(AlgorithmOutput::WeightedShortestPath(WeightedPathOutcome::Found(path)))
            if limits.max_output_paths == 0
                || path.nodes.len() as u64 > limits.max_output_nodes
                || path.edges.len() as u64 > limits.max_output_edges =>
        {
            output = None;
            truncation.push(AlgorithmTruncation::OutputLimit);
        }
        _ => {}
    }
    let stats = AlgorithmStats {
        nodes_extracted: projection.as_ref().map_or(0, |p| p.nodes.len() as u64),
        edges_extracted: projection.as_ref().map_or(0, |p| p.edges.len() as u64),
        arcs: projection
            .as_ref()
            .map_or(0, |p| arc_count(p, scope.direction) as u64),
        edges_examined: meter.edges,
        vertex_operations: meter.vertices,
        scratch_bytes: meter.scratch,
        elapsed_ms,
    };
    Ok(AlgorithmResult {
        output,
        complete: truncation.is_empty(),
        truncation,
        scope_complete: projection.is_some(),
        computation_complete,
        view_sequence: context.view_sequence,
        read_time_ms: context.now_ms,
        seeds,
        endpoints,
        projection,
        stats,
    })
}

fn validate(scope: &AlgorithmScope, algorithm: &Algorithm) -> Result<(), GraphError> {
    if scope.seeds.is_empty() {
        return Err(GraphError::InvalidQuery("an algorithm scope needs a seed"));
    }
    if scope.seeds.len() as u64 > MAX_SEEDS {
        return Err(GraphError::LimitExceedsCeiling {
            limit: "seeds",
            requested: scope.seeds.len() as u64,
            ceiling: MAX_SEEDS,
        });
    }
    scope.seeds.iter().try_for_each(validate_selector)?;
    if let Some(types) = &scope.types {
        if types.is_empty() {
            return Err(GraphError::InvalidQuery(
                "a relationship type scope must not be empty",
            ));
        }
        types
            .iter()
            .try_for_each(|kind| validate_relationship_type(kind))?;
    }
    if let Some(labels) = &scope.labels {
        if labels.is_empty() {
            return Err(GraphError::InvalidQuery("a label scope must not be empty"));
        }
        validate_labels(labels)?;
    }
    validate_conditions(&scope.edge_conditions)?;
    if let Algorithm::WeightedShortestPath {
        source,
        destination,
        weight_property,
    } = algorithm
    {
        validate_selector(source)?;
        validate_selector(destination)?;
        if weight_property
            .as_ref()
            .is_some_and(|name| name.is_empty() || name.len() > MAX_FIELD_NAME_BYTES)
        {
            return Err(GraphError::InvalidQuery(
                "a weight property name must have 1..=256 bytes",
            ));
        }
    }
    Ok(())
}

fn validate_selector(selector: &NodeSelector) -> Result<(), GraphError> {
    match selector {
        NodeSelector::Id(_) => Ok(()),
        NodeSelector::Key(key) => validate_node_key(key),
    }
}

/// The visible node `selector` names; a due node does not resolve.
fn resolve<'g>(
    graph: &'g Graph,
    selector: &NodeSelector,
    expired_at: Option<u64>,
) -> Result<&'g GraphNode, GraphError> {
    let missing = || match selector {
        NodeSelector::Id(id) => GraphError::NodeNotFound(*id),
        NodeSelector::Key(key) => GraphError::UnknownNodeKey(key.clone()),
    };
    let node = match selector {
        NodeSelector::Id(id) => graph.node(*id),
        NodeSelector::Key(key) => graph.node_by_key(key),
    }
    .ok_or_else(missing)?;
    if expired_at.is_some_and(|now| graph.node_expired(node.id, now)) {
        return Err(missing());
    }
    Ok(node)
}

/// Comparisons charged for sorting `count` items: `count * bit_length(count)`.
fn sort_cost(count: usize) -> u64 {
    let count = count as u64;
    count * u64::from(u64::BITS - count.leading_zeros())
}

/// Computational arcs of a projection: two per non-loop edge for `Both`.
fn arc_count(projection: &Projection, direction: Direction) -> usize {
    match direction {
        Direction::Both => projection
            .edges
            .iter()
            .map(|edge| {
                if edge.source == edge.destination {
                    1
                } else {
                    2
                }
            })
            .sum(),
        Direction::Out | Direction::In => projection.edges.len(),
    }
}

/// Why a run stopped before it finished.
enum Halt {
    Truncated(AlgorithmTruncation),
    Failed(AlgorithmError),
}

impl From<AlgorithmTruncation> for Halt {
    fn from(reason: AlgorithmTruncation) -> Self {
        Self::Truncated(reason)
    }
}

impl From<AlgorithmError> for Halt {
    fn from(error: AlgorithmError) -> Self {
        Self::Failed(error)
    }
}

impl From<GraphError> for Halt {
    fn from(error: GraphError) -> Self {
        Self::Failed(AlgorithmError::Graph(error))
    }
}

/// Edge, vertex and scratch budgets with one deadline and cancellation
/// check before work, before every 64th charged operation (edge and vertex
/// units together) and before returning.
struct Meter<'a> {
    context: &'a ExecutionContext<'a>,
    started: u64,
    deadline_ms: u64,
    edge_budget: u64,
    vertex_budget: u64,
    scratch_budget: u64,
    edges: u64,
    vertices: u64,
    scratch: u64,
}

impl<'a> Meter<'a> {
    fn start(
        context: &'a ExecutionContext<'a>,
        limits: &GraphLimits,
        caps: &GraphAlgorithmLimits,
    ) -> (Self, Result<(), AlgorithmTruncation>) {
        let meter = Self {
            context,
            started: (context.clock)(),
            deadline_ms: limits.deadline_ms,
            edge_budget: limits.max_edges_examined,
            vertex_budget: caps.max_vertex_operations,
            scratch_budget: caps.max_scratch_bytes,
            edges: 0,
            vertices: 0,
            scratch: 0,
        };
        let check = meter.check(meter.started);
        (meter, check)
    }

    fn check(&self, now: u64) -> Result<(), AlgorithmTruncation> {
        if self
            .context
            .cancel
            .is_some_and(|flag| flag.load(AtomicOrdering::Relaxed))
        {
            return Err(AlgorithmTruncation::Cancelled);
        }
        if now.saturating_sub(self.started) >= self.deadline_ms {
            return Err(AlgorithmTruncation::Deadline);
        }
        Ok(())
    }

    /// The periodic check before an operation.
    fn tick(&self) -> Result<(), AlgorithmTruncation> {
        let spent = self.edges + self.vertices;
        if spent > 0 && spent.is_multiple_of(CHECK_INTERVAL) {
            self.check((self.context.clock)())?;
        }
        Ok(())
    }

    /// One `max_edges_examined` unit.
    fn edge(&mut self) -> Result<(), AlgorithmTruncation> {
        if self.edges >= self.edge_budget {
            return Err(AlgorithmTruncation::EdgeLimit);
        }
        self.tick()?;
        self.edges += 1;
        Ok(())
    }

    /// One `max_vertex_operations` unit.
    fn vertex(&mut self) -> Result<(), AlgorithmTruncation> {
        if self.vertices >= self.vertex_budget {
            return Err(AlgorithmTruncation::VertexLimit);
        }
        self.tick()?;
        self.vertices += 1;
        Ok(())
    }

    fn vertices(&mut self, count: u64) -> Result<(), AlgorithmTruncation> {
        (0..count).try_for_each(|_| self.vertex())
    }

    /// Reserves `count` items of `bytes` each before they are allocated.
    fn reserve(&mut self, count: usize, bytes: usize) -> Result<(), AlgorithmTruncation> {
        let total = (count as u64).saturating_mul(bytes as u64);
        match self.scratch.checked_add(total) {
            Some(scratch) if scratch <= self.scratch_budget => {
                self.scratch = scratch;
                Ok(())
            }
            _ => Err(AlgorithmTruncation::MemoryLimit),
        }
    }

    fn elapsed_ms(&self) -> u64 {
        (self.context.clock)().saturating_sub(self.started)
    }

    /// The final clock read: elapsed time and the last check.
    fn finish(&self) -> (u64, Result<(), AlgorithmTruncation>) {
        let now = (self.context.clock)();
        (now.saturating_sub(self.started), self.check(now))
    }
}

/// The scope of one run with the TTL clock (`None` when nothing is due).
struct Local<'a> {
    graph: &'a Graph,
    scope: &'a AlgorithmScope,
    limits: &'a GraphLimits,
    expired_at: Option<u64>,
}

impl Local<'_> {
    /// Visible and carrying every scope label.
    fn eligible_node(&self, id: NodeId) -> bool {
        self.graph.node(id).is_some_and(|node| {
            self.expired_at
                .is_none_or(|now| !self.graph.node_expired(id, now))
                && self
                    .scope
                    .labels
                    .as_ref()
                    .is_none_or(|labels| labels.is_subset(&node.labels))
        })
    }

    /// Visible (with both endpoints) and matching every edge condition; the
    /// adjacency scan already applied the relationship types.
    fn eligible_edge(&self, edge: &GraphEdge) -> bool {
        self.expired_at
            .is_none_or(|now| !self.graph.edge_expired(edge, now))
            && conditions_match(&edge.properties, &self.scope.edge_conditions)
    }

    /// Calls `visit` with every adjacency entry of `node` in the scope's
    /// direction and types, charging one edge unit each under the per-node
    /// cap. An error from `visit` stops the scan.
    fn scan(
        &self,
        node: NodeId,
        meter: &mut Meter<'_>,
        mut visit: impl FnMut(&GraphEdge, &mut Meter<'_>) -> Result<(), Halt>,
    ) -> Result<(), Halt> {
        for (examined, (_, id)) in self
            .graph
            .adjacency(node, self.scope.direction, self.scope.types.as_ref(), None)
            .enumerate()
        {
            if examined as u64 >= self.limits.max_edges_per_node {
                return Err(AlgorithmTruncation::DegreeLimit {
                    node_ids: vec![node],
                }
                .into());
            }
            meter.edge()?;
            let edge = self.graph.edge(id).ok_or(GraphError::InvariantViolation(
                "adjacency names a missing edge",
            ))?;
            visit(edge, meter)?;
        }
        Ok(())
    }

    /// Breadth-first discovery from the ascending seeds up to `max_hops`,
    /// then a rescan of every admitted node that retains each eligible edge
    /// between admitted nodes once.
    fn extract(&self, seeds: &[NodeId], meter: &mut Meter<'_>) -> Result<Projection, Halt> {
        let mut admitted = BTreeSet::new();
        let mut queue = VecDeque::new();
        for &seed in seeds {
            meter.reserve(1, NODE_BYTES)?;
            meter.vertex()?;
            admitted.insert(seed);
            queue.push_back((seed, 0));
        }
        while let Some((node, depth)) = queue.pop_front() {
            if depth >= self.limits.max_hops {
                break;
            }
            meter.vertex()?;
            self.scan(node, meter, |edge, meter| {
                if !self.eligible_edge(edge) {
                    return Ok(());
                }
                let next = other_end(edge, node);
                if admitted.contains(&next) || !self.eligible_node(next) {
                    return Ok(());
                }
                if admitted.len() as u64 >= self.limits.max_visited_nodes {
                    return Err(AlgorithmTruncation::VisitedLimit.into());
                }
                meter.reserve(1, NODE_BYTES)?;
                meter.vertex()?;
                admitted.insert(next);
                queue.push_back((next, depth + 1));
                Ok(())
            })?;
        }
        drop(queue);
        meter.vertices(sort_cost(admitted.len()))?;
        let nodes: Vec<NodeId> = admitted.into_iter().collect();
        let mut edges = Vec::new();
        for (position, &node) in nodes.iter().enumerate() {
            meter.vertex()?;
            self.scan(node, meter, |edge, meter| {
                // Both meets every edge from each end; keep it at its source.
                if self.scope.direction == Direction::Both && edge.source != node {
                    return Ok(());
                }
                if !self.eligible_edge(edge) {
                    return Ok(());
                }
                let Ok(other) = nodes.binary_search(&other_end(edge, node)) else {
                    return Ok(());
                };
                meter.reserve(1, size_of::<ProjectedEdge>())?;
                let (source, destination) = if edge.source == node {
                    (position, other)
                } else {
                    (other, position)
                };
                edges.push(ProjectedEdge {
                    id: edge.id,
                    source,
                    destination,
                });
                Ok(())
            })?;
        }
        meter.vertices(sort_cost(edges.len()))?;
        edges.sort_unstable_by_key(|edge| edge.id);
        Ok(Projection { nodes, edges })
    }
}

/// Raw degrees over the retained physical edges: `In` reverses endpoints,
/// `Out` and `Both` keep the stored orientation, and a self-loop adds one to
/// each count.
fn degree(
    projection: &Projection,
    direction: Direction,
    meter: &mut Meter<'_>,
) -> Result<Vec<DegreeRow>, Halt> {
    let count = projection.nodes.len();
    meter.reserve(count, size_of::<DegreeRow>())?;
    meter.vertices(count as u64)?;
    let mut rows: Vec<DegreeRow> = projection
        .nodes
        .iter()
        .map(|&node| DegreeRow {
            node,
            in_degree: 0,
            out_degree: 0,
            incident_degree: 0,
        })
        .collect();
    for edge in &projection.edges {
        meter.edge()?;
        let (tail, head) = orient(edge, direction);
        rows[tail].out_degree += 1;
        rows[head].in_degree += 1;
        rows[tail].incident_degree += 1;
        if head != tail {
            rows[head].incident_degree += 1;
        }
    }
    Ok(rows)
}

/// The (tail, head) positions of `edge` walked in `direction`; `Both` also
/// walks the reverse arc of a non-loop edge.
fn orient(edge: &ProjectedEdge, direction: Direction) -> (usize, usize) {
    match direction {
        Direction::In => (edge.destination, edge.source),
        Direction::Out | Direction::Both => (edge.source, edge.destination),
    }
}

/// One computational arc of the search.
#[derive(Clone, Copy)]
struct Step {
    head: usize,
    edge: EdgeId,
    mantissa: u64,
    shift: u32,
}

impl Local<'_> {
    /// Dijkstra over (node, hops) states from `source` to `destination`.
    ///
    /// First every retained edge's weight is validated in ascending edge-ID
    /// order (the smallest bad edge is reported). Costs are exact fixed-point
    /// sums; states settle in (cost, hops, edge-ID sequence) order, which
    /// every extension increases, so the first settled destination state is
    /// the winner and the cost is rounded once at the end.
    fn shortest_path(
        &self,
        projection: &Projection,
        weight_property: Option<&str>,
        (source, destination): (NodeId, NodeId),
        meter: &mut Meter<'_>,
    ) -> Result<WeightedPathOutcome, Halt> {
        let direction = self.scope.direction;
        let count = projection.nodes.len();
        let edges = &projection.edges;
        meter.reserve(edges.len(), size_of::<Weight>())?;
        meter.reserve(count + 1, size_of::<usize>())?;
        let mut weights = Vec::with_capacity(edges.len());
        let mut offsets = vec![0usize; count + 1];
        let mut span = Span::default();
        for edge in edges {
            meter.edge()?;
            let weight = match weight_property {
                None => UNIT,
                Some(field) => {
                    let stored = self
                        .graph
                        .edge(edge.id)
                        .ok_or(GraphError::InvariantViolation(
                            "a projected edge is missing",
                        ))?;
                    let value = stored
                        .properties
                        .get(field)
                        .ok_or(AlgorithmError::MissingWeight(edge.id))?;
                    classify(value).map_err(|reason| AlgorithmError::InvalidWeight {
                        edge: edge.id,
                        reason,
                    })?
                }
            };
            span.include(weight);
            weights.push(weight);
            let (tail, head) = orient(edge, direction);
            offsets[tail + 1] += 1;
            if direction == Direction::Both && tail != head {
                offsets[head + 1] += 1;
            }
        }
        let nodes = &projection.nodes;
        let (Ok(start), Ok(goal)) = (
            nodes.binary_search(&source),
            nodes.binary_search(&destination),
        ) else {
            return Ok(WeightedPathOutcome::NoPathWithinScopeAndHops);
        };
        meter.vertices(count as u64)?;
        for position in 0..count {
            offsets[position + 1] += offsets[position];
        }
        let fixed = span.layout();
        let total = offsets[count];
        meter.reserve(total, size_of::<Step>())?;
        meter.reserve(count, size_of::<usize>())?;
        let mut cursor = offsets[..count].to_vec();
        let empty = Step {
            head: 0,
            edge: EdgeId(0),
            mantissa: 0,
            shift: 0,
        };
        let mut arcs = vec![empty; total];
        let mut place =
            |tail: usize, head: usize, edge: EdgeId, weight: Weight, meter: &mut Meter<'_>| {
                meter.edge()?;
                arcs[cursor[tail]] = Step {
                    head,
                    edge,
                    mantissa: weight.mantissa,
                    shift: fixed.shift(weight),
                };
                cursor[tail] += 1;
                Ok::<(), AlgorithmTruncation>(())
            };
        for (edge, &weight) in edges.iter().zip(&weights) {
            let (tail, head) = orient(edge, direction);
            place(tail, head, edge.id, weight, meter)?;
            if direction == Direction::Both && tail != head {
                place(head, tail, edge.id, weight, meter)?;
            }
        }
        drop((cursor, weights));

        let layers = self.limits.max_hops as usize + 1;
        let states = count * layers;
        meter.reserve(states, STATE_BYTES + fixed.limbs * size_of::<u64>())?;
        meter.reserve(fixed.limbs, size_of::<u64>())?;
        meter.vertices(states as u64)?;
        let mut search = Search::new(states, layers, fixed.limbs);
        let mut candidate = vec![0u64; fixed.limbs];
        meter.vertex()?;
        search.push(start * layers);
        let winner = loop {
            if search.heap.is_empty() {
                return Ok(WeightedPathOutcome::NoPathWithinScopeAndHops);
            }
            meter.vertex()?;
            let state = search.pop();
            let (position, hops) = (state / layers, state % layers);
            if position == goal {
                break state;
            }
            if hops + 1 == layers {
                continue;
            }
            for arc in &arcs[offsets[position]..offsets[position + 1]] {
                meter.edge()?;
                let next = arc.head * layers + hops + 1;
                if search.slots[next] == SETTLED {
                    continue;
                }
                candidate.copy_from_slice(search.cost(state));
                fixed.add(&mut candidate, arc.mantissa, arc.shift);
                let mut sequence = search.sequences[state];
                sequence[hops] = arc.edge;
                let unseen = search.slots[next] == UNSEEN;
                if unseen
                    || cmp_limbs(&candidate, search.cost(next))
                        .then_with(|| sequence[..=hops].cmp(&search.sequences[next][..=hops]))
                        .is_lt()
                {
                    meter.vertex()?;
                    search.costs[next * fixed.limbs..][..fixed.limbs].copy_from_slice(&candidate);
                    search.sequences[next] = sequence;
                    search.parents[next] = state;
                    if unseen {
                        search.push(next);
                    } else {
                        search.decrease(next);
                    }
                }
            }
        };

        let hops = winner % layers;
        meter.vertices(hops as u64 + 1)?;
        meter.reserve(2 * hops + 1, size_of::<u64>())?;
        let mut positions = Vec::with_capacity(hops + 1);
        let mut state = winner;
        loop {
            positions.push(state / layers);
            if state == start * layers {
                break;
            }
            state = search.parents[state];
        }
        let cost = fixed
            .to_f64(search.cost(winner))
            .ok_or(AlgorithmError::NumericOverflow)?;
        Ok(WeightedPathOutcome::Found(WeightedPath {
            cost,
            nodes: positions
                .iter()
                .rev()
                .map(|&position| nodes[position])
                .collect(),
            edges: search.sequences[winner][..hops].to_vec(),
        }))
    }
}

/// Slot marker of a state never reached.
const UNSEEN: usize = usize::MAX;
/// Slot marker of a settled state.
const SETTLED: usize = usize::MAX - 1;

/// States `position * layers + hops` with exact costs, edge-ID sequences,
/// parents and an indexed binary min-heap keyed by (cost, hops, sequence).
struct Search {
    layers: usize,
    limbs: usize,
    costs: Vec<u64>,
    sequences: Vec<[EdgeId; MAX_PATH_HOPS]>,
    parents: Vec<usize>,
    /// The heap position of a queued state, or `UNSEEN` / `SETTLED`.
    slots: Vec<usize>,
    heap: Vec<usize>,
}

impl Search {
    fn new(states: usize, layers: usize, limbs: usize) -> Self {
        Self {
            layers,
            limbs,
            costs: vec![0; states * limbs],
            sequences: vec![[EdgeId(0); MAX_PATH_HOPS]; states],
            parents: vec![UNSEEN; states],
            slots: vec![UNSEEN; states],
            heap: Vec::with_capacity(states),
        }
    }

    fn cost(&self, state: usize) -> &[u64] {
        &self.costs[state * self.limbs..][..self.limbs]
    }

    fn key_cmp(&self, a: usize, b: usize) -> Ordering {
        let (hops_a, hops_b) = (a % self.layers, b % self.layers);
        cmp_limbs(self.cost(a), self.cost(b))
            .then(hops_a.cmp(&hops_b))
            .then_with(|| self.sequences[a][..hops_a].cmp(&self.sequences[b][..hops_b]))
    }

    fn push(&mut self, state: usize) {
        self.slots[state] = self.heap.len();
        self.heap.push(state);
        self.sift_up(self.heap.len() - 1);
    }

    fn decrease(&mut self, state: usize) {
        self.sift_up(self.slots[state]);
    }

    fn pop(&mut self) -> usize {
        let top = self.heap.swap_remove(0);
        self.slots[top] = SETTLED;
        if !self.heap.is_empty() {
            self.slots[self.heap[0]] = 0;
            self.sift_down(0);
        }
        top
    }

    fn swap(&mut self, i: usize, j: usize) {
        self.heap.swap(i, j);
        self.slots[self.heap[i]] = i;
        self.slots[self.heap[j]] = j;
    }

    fn sift_up(&mut self, mut index: usize) {
        while index > 0 {
            let parent = (index - 1) / 2;
            if self.key_cmp(self.heap[index], self.heap[parent]).is_ge() {
                break;
            }
            self.swap(index, parent);
            index = parent;
        }
    }

    fn sift_down(&mut self, mut index: usize) {
        loop {
            let mut least = index;
            for child in [2 * index + 1, 2 * index + 2] {
                if child < self.heap.len()
                    && self.key_cmp(self.heap[child], self.heap[least]).is_lt()
                {
                    least = child;
                }
            }
            if least == index {
                break;
            }
            self.swap(index, least);
            index = least;
        }
    }
}

/// Compares equal-length little-endian limb numbers.
fn cmp_limbs(a: &[u64], b: &[u64]) -> Ordering {
    a.iter().rev().cmp(b.iter().rev())
}

/// A weight `mantissa * 2^exponent`; `mantissa` is odd, or 0 for zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Weight {
    mantissa: u64,
    exponent: i32,
}

const UNIT: Weight = Weight {
    mantissa: 1,
    exponent: 0,
};

impl Weight {
    fn new(mantissa: u64, exponent: i32) -> Self {
        if mantissa == 0 {
            return Self {
                mantissa: 0,
                exponent: 0,
            };
        }
        let zeros = mantissa.trailing_zeros();
        Self {
            mantissa: mantissa >> zeros,
            exponent: exponent + zeros as i32,
        }
    }
}

/// An exact weight: a nonnegative integer up to 2^53 or a finite
/// nonnegative float (-0.0 is 0).
fn classify(value: &PropertyValue) -> Result<Weight, &'static str> {
    match value {
        PropertyValue::Integer(value) if *value < 0 => Err("weights must be nonnegative"),
        PropertyValue::Integer(value) if *value > 1 << 53 => {
            Err("integer weights must be at most 2^53")
        }
        PropertyValue::Integer(value) => Ok(Weight::new(*value as u64, 0)),
        PropertyValue::Float(value) if !value.is_finite() => Err("weights must be finite"),
        PropertyValue::Float(value) if *value < 0.0 => Err("weights must be nonnegative"),
        PropertyValue::Float(value) => {
            let bits = value.abs().to_bits();
            let biased = (bits >> 52) as i32;
            let fraction = bits & ((1 << 52) - 1);
            Ok(if biased == 0 {
                Weight::new(fraction, -1074)
            } else {
                Weight::new(fraction | 1 << 52, biased - 1075)
            })
        }
        _ => Err("weights must be integers or floats"),
    }
}

/// The exponents of the nonzero weights of one search.
#[derive(Clone, Copy, Debug, Default)]
struct Span {
    lowest: Option<i32>,
    highest: i32,
}

impl Span {
    fn include(&mut self, weight: Weight) {
        if weight.mantissa == 0 {
            return;
        }
        let top = weight.exponent + (63 - weight.mantissa.leading_zeros() as i32);
        self.highest = match self.lowest {
            None => top,
            Some(_) => self.highest.max(top),
        };
        self.lowest = Some(
            self.lowest
                .map_or(weight.exponent, |lowest| lowest.min(weight.exponent)),
        );
    }

    /// Units of the lowest weight exponent and room for the sum of
    /// `MAX_PATH_HOPS` (4 = 2^2) of the largest weights.
    fn layout(&self) -> Fixed {
        match self.lowest {
            None => Fixed { base: 0, limbs: 1 },
            Some(base) => Fixed {
                base,
                limbs: ((self.highest - base + 3) as usize).div_ceil(64),
            },
        }
    }
}

/// Exact fixed-point costs: little-endian `u64` limbs in units of `2^base`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fixed {
    base: i32,
    limbs: usize,
}

impl Fixed {
    fn shift(&self, weight: Weight) -> u32 {
        if weight.mantissa == 0 {
            0
        } else {
            (weight.exponent - self.base) as u32
        }
    }

    /// Adds `mantissa << shift`; the layout leaves room for every sum.
    fn add(&self, cost: &mut [u64], mantissa: u64, shift: u32) {
        if mantissa == 0 {
            return;
        }
        let (limb, bit) = ((shift / 64) as usize, shift % 64);
        let mut carry = false;
        let mut addend = mantissa << bit;
        let mut high = if bit == 0 { 0 } else { mantissa >> (64 - bit) };
        for slot in &mut cost[limb..] {
            let (sum, first) = slot.overflowing_add(addend);
            let (sum, second) = sum.overflowing_add(u64::from(carry));
            *slot = sum;
            carry = first || second;
            addend = high;
            high = 0;
            if !carry && addend == 0 {
                break;
            }
        }
    }

    /// The cost rounded once to the nearest f64, ties to even; `None` when
    /// it rounds beyond `f64::MAX`.
    fn to_f64(self, cost: &[u64]) -> Option<f64> {
        let Some(limb) = cost.iter().rposition(|limb| *limb != 0) else {
            return Some(0.0);
        };
        let top = limb as i64 * 64 + 63 - i64::from(cost[limb].leading_zeros());
        let base = i64::from(self.base);
        let mut lsb = (top + base - 52).max(-1074);
        let shift = lsb - base;
        let mut mantissa = if shift <= 0 {
            cost[0] << -shift
        } else {
            let shift = shift as usize;
            let kept = bits_from(cost, shift);
            let half = bits_from(cost, shift - 1) & 1 == 1;
            let sticky = any_below(cost, shift - 1);
            kept + u64::from(half && (sticky || kept & 1 == 1))
        };
        if mantissa == 1 << 53 {
            mantissa = 1 << 52;
            lsb += 1;
        }
        if mantissa < 1 << 52 {
            return Some(f64::from_bits(mantissa));
        }
        let biased = lsb + 1075;
        (biased < 2047).then(|| f64::from_bits((biased as u64) << 52 | (mantissa - (1 << 52))))
    }
}

/// The 64 bits of `cost` starting at bit `shift`.
fn bits_from(cost: &[u64], shift: usize) -> u64 {
    let (limb, bit) = (shift / 64, shift % 64);
    let low = cost.get(limb).map_or(0, |limb| limb >> bit);
    let high = match bit {
        0 => 0,
        _ => cost.get(limb + 1).map_or(0, |limb| limb << (64 - bit)),
    };
    low | high
}

/// Whether any bit of `cost` below bit `index` is set.
fn any_below(cost: &[u64], index: usize) -> bool {
    let (limb, bit) = (index / 64, index % 64);
    cost[..limb].iter().any(|limb| *limb != 0) || cost[limb] & ((1u64 << bit) - 1) != 0
}

#[cfg(test)]
mod tests;
