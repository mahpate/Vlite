//! Bounded local graph algorithms (Gap 5 slices S1 to S3).
//!
//! [`run_with`] (and [`run`], its owned-request form) extracts one explicit
//! local scope and runs one algorithm on it:
//!
//! - Shape: limits, caps and the request's bounded shape are checked before
//!   any work: 1..=10,000 seed selectors, 1..=256 relationship types and
//!   1..=8 labels (raw counts, before deduplication), a 1..=256-byte weight
//!   name, PageRank's damping and tolerance (finite, strictly inside the
//!   unit interval) and 1..=10,000 raw personalization entries. Callers
//!   check their edge conditions (at most 64) the same way.
//! - Preparation runs under the meter, which starts before it: edge
//!   condition conversion (the caller's), then relationship types and labels
//!   (validated, sorted and deduplicated), seed selectors (validated,
//!   resolved, label-checked, sorted and deduplicated; every seed must be
//!   visible and carry every scope label), the path endpoints and the
//!   personalization entries (resolved, checked, sorted and summed), one
//!   vertex unit per item and pass plus the sorts. An invalid or missing
//!   item is an error only if the meter reaches it.
//! - Scope: level-synchronous breadth-first discovery from the seeds admits
//!   the eligible nodes up to `max_hops` hops away, one level at a time;
//!   then every admitted node's adjacency is scanned again to retain every
//!   eligible edge between admitted endpoints (an induced projection).
//!   Visibility is decided once at `now_ms`: due nodes and edges are
//!   invisible and a due endpoint hides its edges. Excluded nodes never
//!   bridge the scope.
//! - Budgets: preparation, discovery, rescans, rejected edges, weight
//!   validation, every computational pass and the caller's staging of the
//!   result share one meter. Adjacency entries, retained edges and arcs
//!   charge `max_edges_examined`; item preparation, typed adjacency probes
//!   (one per scanned node and scope type, found or not), sorting, level
//!   merges, node loops, state initialization and heap actions charge
//!   `max_vertex_operations`; every buffer is reserved against
//!   `max_scratch_bytes` at the capacity it allocates, before allocating it
//!   (a growing buffer doubles, and both copies are charged).
//!   Reservations accumulate, so they bound the peak. Cancellation and the
//!   deadline are checked before any work, before every 64th charged
//!   operation (edge and vertex units together) and once more before
//!   returning.
//! - Results: any stop before the computation and staging finished returns
//!   no output, never an analysis of a prefix. Output caps apply to the
//!   finished output before staging. The iterative algorithms stage the
//!   request first, reserve every buffer and prepay their rows before their
//!   first full step; a stop after it (a budget, the deadline, cancellation
//!   or `max_iterations`) returns the last full step, marked incomplete,
//!   and discards the interrupted one.
//!
//! Algorithms: [`Algorithm::Degree`] (raw degrees over retained physical
//! edges), [`Algorithm::WeightedShortestPath`] (Dijkstra over
//! (node, hops) states with exact sums), [`Algorithm::PageRank`] and
//! [`Algorithm::LabelPropagation`] (synchronous and in-place sweeps over the
//! extracted projection, see the `iterative` module).

use super::error::GraphError;
use super::filter::{conditions_match, validate_conditions};
use super::limits::{CHECK_INTERVAL, GraphLimits, MAX_FIELD_NAME_BYTES, MAX_LABELS};
use super::meter::ExecutionContext;
use super::model::{
    Direction, EdgeId, GraphEdge, GraphNode, NodeId, PropertyCondition, PropertyValue,
    validate_label, validate_node_key, validate_relationship_type,
};
use super::query::NodeSelector;
use super::state::Graph;
use super::traverse::other_end;
use std::borrow::Cow;
use std::cmp::Ordering;
use std::mem::size_of;
use std::sync::atomic::Ordering as AtomicOrdering;

/// Version of the algorithm semantics reported with every result.
pub const ALGORITHM_VERSION: u32 = 1;

/// The most raw seed selectors one request may name.
pub const MAX_SEEDS: u64 = GraphLimits::CEILING.max_visited_nodes;
/// The most raw relationship types one scope may name.
pub const MAX_SCOPE_TYPES: u64 = 256;
/// PageRank's damping factor when a caller names none.
pub const DEFAULT_DAMPING: f64 = 0.85;
/// PageRank's convergence bound on the L1 change of one full step when a
/// caller names none.
pub const DEFAULT_TOLERANCE: f64 = 1e-8;
/// Edge-ID sequence slots per path state (`GraphLimits::CEILING.max_hops`).
const MAX_PATH_HOPS: usize = 4;
const _: () = assert!(GraphLimits::CEILING.max_hops as usize == MAX_PATH_HOPS);
/// Fixed bytes of one path state besides its cost limbs: its edge-ID
/// sequence, parent, heap slot and heap entry.
const STATE_BYTES: usize = size_of::<[EdgeId; MAX_PATH_HOPS]>() + 3 * size_of::<usize>();

/// Caps an algorithm adds to [`GraphLimits`]. Values above
/// [`GraphAlgorithmLimits::CEILING`] are rejected, never clamped, and every
/// cap must be at least 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct GraphAlgorithmLimits {
    /// Vertex operations: request items, typed adjacency probes, sort
    /// steps, level merges, node loops, state initialization, heap actions,
    /// iterative node updates and result staging.
    pub max_vertex_operations: u64,
    /// Bytes of every scratch and output buffer at its allocated capacity,
    /// reserved before the buffer is allocated. Reservations accumulate, so
    /// this bounds the peak.
    pub max_scratch_bytes: u64,
    /// Full PageRank iterations or label propagation sweeps (default 20,
    /// ceiling 100). Reaching it without converging stops the run with
    /// [`AlgorithmTruncation::IterationLimit`]; the other algorithms ignore
    /// it.
    pub max_iterations: u64,
}

impl GraphAlgorithmLimits {
    pub const DEFAULT: Self = Self {
        max_vertex_operations: 100_000,
        max_scratch_bytes: 64 << 20,
        max_iterations: 20,
    };

    pub const CEILING: Self = Self {
        max_vertex_operations: 1_000_000,
        max_scratch_bytes: 256 << 20,
        max_iterations: 100,
    };

    /// Rejects the first cap above its ceiling, then zero caps.
    pub(crate) fn validate(&self) -> Result<(), GraphError> {
        let fields = |limits: &Self| {
            [
                ("max_vertex_operations", limits.max_vertex_operations),
                ("max_scratch_bytes", limits.max_scratch_bytes),
                ("max_iterations", limits.max_iterations),
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
        if self.max_iterations == 0 {
            return Err(GraphError::InvalidQuery(
                "max_iterations must be at least 1",
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

/// One explicit local scope, owned (see [`Request`] for the borrowed form).
/// `None` means any relationship type or any label; `Some` lists must not
/// be empty, and duplicates are ignored.
#[derive(Clone, Debug, PartialEq)]
pub struct AlgorithmScope {
    pub seeds: Vec<NodeSelector>,
    pub direction: Direction,
    pub types: Option<Vec<String>>,
    /// Labels every scoped node, seeds included, must carry.
    pub labels: Option<Vec<String>>,
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
    /// Damped PageRank by synchronous full steps over the computational
    /// arcs: `damping` and `tolerance` must be finite and strictly between
    /// 0 and 1. The teleport vector is uniform over the scoped nodes, or
    /// the normalized positive `personalization` masses on distinct seeds.
    /// Parallel arcs add their weights and self-loops count; a node with
    /// no outgoing weight gives its mass to the teleport vector. The run
    /// converges when one full step changes the vector by at most
    /// `tolerance` in L1. `weight_property` names a nonnegative integer or
    /// float edge property; `None` weighs every arc 1.
    PageRank {
        damping: f64,
        tolerance: f64,
        personalization: Option<Vec<(NodeSelector, f64)>>,
        weight_property: Option<String>,
    },
    /// Label propagation over the undirected simple graph of the projection
    /// (parallel edges collapse, self-loops are ignored, direction is not
    /// used): every node starts with its own ID as its label; each sweep
    /// visits nodes ascending and updates in place, keeping the current
    /// label when it ties for the most frequent neighbor label and
    /// otherwise taking the smallest such label. A sweep without changes
    /// converges.
    LabelPropagation,
}

/// A node selector borrowed from a caller's request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selector<'s> {
    Id(NodeId),
    Key(&'s str),
}

/// A caller's node selector type, read in place so that a request is never
/// copied before the meter starts.
pub trait Seed {
    fn selector(&self) -> Selector<'_>;
}

impl Seed for NodeSelector {
    fn selector(&self) -> Selector<'_> {
        match self {
            Self::Id(id) => Selector::Id(*id),
            Self::Key(key) => Selector::Key(key),
        }
    }
}

/// The path part of a [`Request`]: [`Algorithm::WeightedShortestPath`]
/// from `source`, which must be a seed, to `destination`.
#[derive(Debug)]
pub struct PathRequest<'s, S> {
    pub source: &'s S,
    pub destination: &'s S,
    pub weight_property: Option<&'s str>,
}

/// The PageRank part of a [`Request`] (see [`Algorithm::PageRank`]).
#[derive(Debug)]
pub struct RankRequest<'s, S> {
    pub damping: f64,
    pub tolerance: f64,
    pub personalization: Option<&'s [(S, f64)]>,
    pub weight_property: Option<&'s str>,
}

/// The algorithm of a [`Request`].
#[derive(Debug)]
pub enum Job<'s, S> {
    Degree,
    Path(PathRequest<'s, S>),
    PageRank(RankRequest<'s, S>),
    LabelPropagation,
}

/// A borrowed request: one local scope and the algorithm to run on it.
/// `None` types or labels mean any; `Some` lists must not be empty, and
/// duplicates are ignored.
#[derive(Debug)]
pub struct Request<'s, S> {
    pub seeds: &'s [S],
    pub direction: Direction,
    pub types: Option<&'s [String]>,
    /// Labels every scoped node, seeds included, must carry.
    pub labels: Option<&'s [String]>,
    pub job: Job<'s, S>,
}

/// A run handed to the caller's staging step (see [`run_with`]): the capped
/// output (`None` when an output cap withheld a path, and always for the
/// iterative algorithms, which stage before they compute) and the
/// normalized request.
#[derive(Debug)]
pub struct Finished<'r> {
    pub output: Option<&'r AlgorithmOutput>,
    /// The resolved seeds, deduplicated and ascending.
    pub seeds: &'r [NodeId],
    /// The relationship types, sorted and deduplicated.
    pub types: Option<&'r [&'r str]>,
    /// The labels, sorted and deduplicated.
    pub labels: Option<&'r [&'r str]>,
    /// The resolved path source and destination.
    pub endpoints: Option<(NodeId, NodeId)>,
    /// The resolved PageRank personalization, ascending by node, with the
    /// masses as given.
    pub personalization: Option<&'r [(NodeId, f64)]>,
}

/// Why a result is incomplete. Results list each reason at most once, in
/// declaration order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AlgorithmTruncation {
    /// Discovery would admit more than `max_visited_nodes` nodes (detected
    /// when a level ends).
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
    /// `max_iterations` full steps finished without converging.
    IterationLimit,
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
    PageRank(PageRankOutput),
    LabelPropagation(LabelPropagationOutput),
}

/// One node's PageRank score.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct NodeScore {
    pub node_id: u64,
    pub score: f64,
}

/// The last full PageRank step: the scores sum to 1 up to rounding.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct PageRankOutput {
    /// Ascending by node ID; at most `max_output_nodes` rows.
    pub scores: Vec<NodeScore>,
    /// Full steps completed (at least 1).
    pub iterations: u64,
    /// The L1 change of the last full step.
    pub residual: f64,
    /// The last full step changed the vector by at most the tolerance.
    pub converged: bool,
}

/// One node's community: the smallest node ID sharing its label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct CommunityAssignment {
    pub node_id: u64,
    pub community_id: u64,
}

/// The labels after the last full sweep.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LabelPropagationOutput {
    /// Ascending by node ID; at most `max_output_nodes` rows.
    pub assignments: Vec<CommunityAssignment>,
    /// Full sweeps completed (at least 1), the unchanged one included.
    pub sweeps: u64,
    /// The last full sweep changed no label.
    pub converged: bool,
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
    /// Bytes reserved before allocation at allocated capacity, cumulative
    /// over the run (an upper bound on the scratch and output peak).
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

/// The result of [`run`] and [`run_with`]. `complete` is true exactly when
/// `truncation` is empty; `output` is `None` whenever preparation,
/// extraction, computation or staging stopped, except that an iterative
/// algorithm stopped after its first full step keeps the last full step.
#[derive(Clone, Debug, PartialEq)]
pub struct AlgorithmResult {
    pub output: Option<AlgorithmOutput>,
    pub complete: bool,
    pub truncation: Vec<AlgorithmTruncation>,
    /// Extraction finished.
    pub scope_complete: bool,
    /// The computation and staging finished and passed the final deadline
    /// and cancellation check (output caps aside); an iterative algorithm
    /// also converged.
    pub computation_complete: bool,
    pub view_sequence: u64,
    /// The TTL clock every visibility decision used.
    pub read_time_ms: u64,
    /// The resolved seeds, deduplicated and ascending (empty when
    /// preparation stopped).
    pub seeds: Vec<NodeId>,
    /// The resolved path source and destination (`None` when preparation
    /// stopped).
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
    /// The winning path cost, a node's PageRank out-weight or the
    /// personalization total exceeds the largest finite f64.
    NumericOverflow,
}

impl From<GraphError> for AlgorithmError {
    fn from(error: GraphError) -> Self {
        Self::Graph(error)
    }
}

/// Runs `algorithm` on the local scope `scope` of `graph`: [`run_with`] on
/// the borrowed request, with the conditions validated first and no
/// staging.
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
    validate_conditions(&scope.edge_conditions)?;
    let request = Request {
        seeds: &scope.seeds,
        direction: scope.direction,
        types: scope.types.as_deref(),
        labels: scope.labels.as_deref(),
        job: match algorithm {
            Algorithm::Degree => Job::Degree,
            Algorithm::WeightedShortestPath {
                source,
                destination,
                weight_property,
            } => Job::Path(PathRequest {
                source,
                destination,
                weight_property: weight_property.as_deref(),
            }),
            Algorithm::PageRank {
                damping,
                tolerance,
                personalization,
                weight_property,
            } => Job::PageRank(RankRequest {
                damping: *damping,
                tolerance: *tolerance,
                personalization: personalization.as_deref(),
                weight_property: weight_property.as_deref(),
            }),
            Algorithm::LabelPropagation => Job::LabelPropagation,
        },
    };
    let conditions = |_: &mut Meter<'_>| Ok(Cow::Borrowed(&scope.edge_conditions[..]));
    run_with(
        graph,
        &request,
        limits,
        caps,
        context,
        conditions,
        |_, _| Ok(()),
    )
    .map(|(result, _)| result)
}

/// Runs one algorithm on the local scope `request` names.
///
/// Limits, caps and the request's bounded shape are checked first; the
/// caller checks its conditions' shape before calling. Everything else
/// runs under one meter, started before any per-item work: `conditions`
/// converts the edge conditions, then the request is prepared, the scope
/// extracted and the algorithm run. A finished output is capped and handed
/// to `stage`, under the same meter, before the final check; `stage`'s
/// value is returned only when the run completed (output caps aside). The
/// iterative algorithms call `stage` (with no output) before their setup,
/// and return its value with any output they keep.
/// Truncation returns `Ok` with no output; invalid input and weights
/// return `Err`.
pub fn run_with<'s, S: Seed, T>(
    graph: &Graph,
    request: &Request<'s, S>,
    limits: &GraphLimits,
    caps: &GraphAlgorithmLimits,
    context: &ExecutionContext<'_>,
    conditions: impl FnOnce(&mut Meter<'_>) -> Result<Cow<'s, [PropertyCondition]>, AlgorithmTruncation>,
    stage: impl FnOnce(&Finished<'_>, &mut Meter<'_>) -> Result<T, AlgorithmTruncation>,
) -> Result<(AlgorithmResult, Option<T>), AlgorithmError> {
    limits.validate()?;
    caps.validate()?;
    check_shape(request)?;
    let expired_at = graph.has_due(context.now_ms).then_some(context.now_ms);
    let (mut meter, begin) = Meter::start(context, limits, caps);
    let mut prepared = None;
    let mut projection = None;
    let computed = begin.map_err(Halt::from).and_then(|()| {
        let conditions = conditions(&mut meter)?;
        let ready = prepared.insert(Prepared::new(
            graph, request, limits, expired_at, conditions, &mut meter,
        )?);
        let local = Local {
            graph,
            direction: request.direction,
            types: ready.types.as_deref(),
            labels: ready.labels.as_deref(),
            conditions: &ready.conditions,
            limits,
            expired_at,
            edge_zero: ready.types.as_ref().and_then(|_| graph.edge(EdgeId(0))),
        };
        let extracted = projection.insert(local.extract(&ready.seeds, &mut meter)?);
        let finished = |output| Finished {
            output,
            seeds: &ready.seeds,
            types: ready.types.as_deref(),
            labels: ready.labels.as_deref(),
            endpoints: ready.endpoints,
            personalization: ready.teleport.as_ref().map(|teleport| &teleport.masses[..]),
        };
        let iterated = match &request.job {
            Job::Degree | Job::Path(_) => {
                let output = match (&request.job, ready.endpoints) {
                    (Job::Path(path), Some(endpoints)) => {
                        AlgorithmOutput::WeightedShortestPath(local.shortest_path(
                            extracted,
                            path.weight_property,
                            endpoints,
                            &mut meter,
                        )?)
                    }
                    _ => AlgorithmOutput::Degree(degree(extracted, request.direction, &mut meter)?),
                };
                let (output, cut) = cap(output, limits);
                let staged = stage(&finished(output.as_ref()), &mut meter)?;
                return Ok(Computed {
                    output,
                    cut,
                    staged,
                    iterative: false,
                    stop: None,
                });
            }
            Job::PageRank(rank) => {
                let staged = stage(&finished(None), &mut meter)?;
                let settings = iterative::Rank {
                    damping: rank.damping,
                    tolerance: rank.tolerance,
                    weight_property: rank.weight_property,
                    teleport: ready.teleport.as_ref(),
                    max_iterations: caps.max_iterations,
                };
                (local.page_rank(extracted, &settings, &mut meter)?, staged)
            }
            Job::LabelPropagation => {
                let staged = stage(&finished(None), &mut meter)?;
                let iterated = iterative::label_propagation(
                    extracted,
                    caps.max_iterations,
                    limits.max_output_nodes,
                    &mut meter,
                )?;
                (iterated, staged)
            }
        };
        let (iterative::Iterated { output, cut, stop }, staged) = iterated;
        Ok(Computed {
            output: Some(output),
            cut,
            staged,
            iterative: true,
            stop,
        })
    });

    let mut truncation = Vec::new();
    let mut output = None;
    let mut staged = None;
    let mut computation_complete = false;
    let elapsed_ms = match computed {
        Err(Halt::Failed(error)) => return Err(error),
        Err(Halt::Truncated(reason)) => {
            truncation.push(reason);
            meter.elapsed_ms()
        }
        Ok(computed) => {
            let (elapsed_ms, check) = meter.finish();
            match computed.stop.or(check.err()) {
                None => {
                    output = computed.output;
                    staged = Some(computed.staged);
                    computation_complete = true;
                    if computed.cut {
                        truncation.push(AlgorithmTruncation::OutputLimit);
                    }
                }
                Some(reason) => {
                    truncation.push(reason);
                    // An iterative output is the last full step.
                    if computed.iterative {
                        output = computed.output;
                        staged = Some(computed.staged);
                    }
                }
            }
            elapsed_ms
        }
    };
    let (seeds, endpoints) =
        prepared.map_or((Vec::new(), None), |ready| (ready.seeds, ready.endpoints));
    let stats = AlgorithmStats {
        nodes_extracted: projection.as_ref().map_or(0, |p| p.nodes.len() as u64),
        edges_extracted: projection.as_ref().map_or(0, |p| p.edges.len() as u64),
        arcs: projection
            .as_ref()
            .map_or(0, |p| arc_count(p, request.direction) as u64),
        edges_examined: meter.edges,
        vertex_operations: meter.vertices,
        scratch_bytes: meter.scratch,
        elapsed_ms,
    };
    let result = AlgorithmResult {
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
    };
    Ok((result, staged))
}

/// Rejects a request outside the bounded shape before any work.
fn check_shape<S>(request: &Request<'_, S>) -> Result<(), GraphError> {
    let seeds = request.seeds.len() as u64;
    if seeds == 0 {
        return Err(GraphError::InvalidQuery("an algorithm scope needs a seed"));
    }
    if seeds > MAX_SEEDS {
        return Err(GraphError::LimitExceedsCeiling {
            limit: "seeds",
            requested: seeds,
            ceiling: MAX_SEEDS,
        });
    }
    if let Some(types) = request.types {
        if types.is_empty() {
            return Err(GraphError::InvalidQuery(
                "a relationship type scope must not be empty",
            ));
        }
        if types.len() as u64 > MAX_SCOPE_TYPES {
            return Err(GraphError::LimitExceedsCeiling {
                limit: "relationship_types",
                requested: types.len() as u64,
                ceiling: MAX_SCOPE_TYPES,
            });
        }
    }
    if let Some(labels) = request.labels {
        if labels.is_empty() {
            return Err(GraphError::InvalidQuery("a label scope must not be empty"));
        }
        if labels.len() > MAX_LABELS {
            return Err(GraphError::TooManyLabels);
        }
    }
    let weight = match &request.job {
        Job::Degree | Job::LabelPropagation => None,
        Job::Path(path) => path.weight_property,
        Job::PageRank(rank) => {
            let open = |value: f64| value > 0.0 && value < 1.0;
            if !open(rank.damping) {
                return Err(GraphError::InvalidQuery(
                    "damping must be finite and strictly between 0 and 1",
                ));
            }
            if !open(rank.tolerance) {
                return Err(GraphError::InvalidQuery(
                    "tolerance must be finite and strictly between 0 and 1",
                ));
            }
            if let Some(entries) = rank.personalization {
                if entries.is_empty() {
                    return Err(GraphError::InvalidQuery(
                        "personalization must not be empty",
                    ));
                }
                if entries.len() as u64 > MAX_SEEDS {
                    return Err(GraphError::LimitExceedsCeiling {
                        limit: "personalization",
                        requested: entries.len() as u64,
                        ceiling: MAX_SEEDS,
                    });
                }
            }
            rank.weight_property
        }
    };
    if weight.is_some_and(|name| name.is_empty() || name.len() > MAX_FIELD_NAME_BYTES) {
        return Err(GraphError::InvalidQuery(
            "a weight property name must have 1..=256 bytes",
        ));
    }
    Ok(())
}

/// The parts of a run that reached staging: the capped output, whether an
/// output cap cut it, the staged value and, for the iterative algorithms,
/// why they stopped before converging.
struct Computed<T> {
    output: Option<AlgorithmOutput>,
    cut: bool,
    staged: T,
    iterative: bool,
    stop: Option<AlgorithmTruncation>,
}

/// Applies the output caps to a finished output: degree rows are cut to
/// `max_output_nodes`; a path over the node, edge or path cap is withheld.
/// The flag reports a cut.
fn cap(output: AlgorithmOutput, limits: &GraphLimits) -> (Option<AlgorithmOutput>, bool) {
    match output {
        AlgorithmOutput::Degree(mut rows) if rows.len() as u64 > limits.max_output_nodes => {
            rows.truncate(limits.max_output_nodes as usize);
            (Some(AlgorithmOutput::Degree(rows)), true)
        }
        AlgorithmOutput::WeightedShortestPath(WeightedPathOutcome::Found(path))
            if limits.max_output_paths == 0
                || path.nodes.len() as u64 > limits.max_output_nodes
                || path.edges.len() as u64 > limits.max_output_edges =>
        {
            (None, true)
        }
        output => (Some(output), false),
    }
}

/// A request's validated, resolved and normalized items.
struct Prepared<'s> {
    conditions: Cow<'s, [PropertyCondition]>,
    types: Option<Vec<&'s str>>,
    labels: Option<Vec<&'s str>>,
    seeds: Vec<NodeId>,
    endpoints: Option<(NodeId, NodeId)>,
    teleport: Option<iterative::Teleport>,
}

impl<'s> Prepared<'s> {
    /// Types, labels, seeds, then the path endpoints or the
    /// personalization, one vertex unit per item plus the sorts; every
    /// buffer is reserved first.
    fn new<S: Seed>(
        graph: &Graph,
        request: &Request<'s, S>,
        limits: &GraphLimits,
        expired_at: Option<u64>,
        conditions: Cow<'s, [PropertyCondition]>,
        meter: &mut Meter<'_>,
    ) -> Result<Self, Halt> {
        let types = match request.types {
            None => None,
            Some(types) => Some(names(types, validate_relationship_type, meter)?),
        };
        let labels = match request.labels {
            None => None,
            Some(labels) => Some(names(labels, validate_label, meter)?),
        };
        let raw = request.seeds.len();
        meter.reserve(raw, size_of::<NodeId>())?;
        let mut seeds = Vec::with_capacity(raw);
        for seed in request.seeds {
            meter.vertex()?;
            let node = resolve(graph, seed.selector(), expired_at)?;
            if labels
                .as_deref()
                .is_some_and(|labels| !carries(node, labels))
            {
                return Err(
                    GraphError::InvalidQuery("every seed must carry every scope label").into(),
                );
            }
            seeds.push(node.id);
        }
        sort_dedup(&mut seeds, meter)?;
        if seeds.len() as u64 > limits.max_visited_nodes {
            return Err(GraphError::InvalidQuery("distinct seeds exceed max_visited_nodes").into());
        }
        let endpoints = match &request.job {
            Job::Path(path) => {
                meter.vertex()?;
                let source = resolve(graph, path.source.selector(), expired_at)?.id;
                if seeds.binary_search(&source).is_err() {
                    return Err(GraphError::InvalidQuery("the path source must be a seed").into());
                }
                meter.vertex()?;
                let destination = resolve(graph, path.destination.selector(), expired_at)?.id;
                Some((source, destination))
            }
            _ => None,
        };
        let teleport = match &request.job {
            Job::PageRank(RankRequest {
                personalization: Some(entries),
                ..
            }) => Some(iterative::teleport(
                graph, entries, &seeds, expired_at, meter,
            )?),
            _ => None,
        };
        Ok(Self {
            conditions,
            types,
            labels,
            seeds,
            endpoints,
            teleport,
        })
    }
}

/// Validates, sorts and deduplicates relationship types or labels, one
/// vertex unit each plus the sort.
fn names<'s>(
    items: &'s [String],
    validate: fn(&str) -> Result<(), GraphError>,
    meter: &mut Meter<'_>,
) -> Result<Vec<&'s str>, Halt> {
    meter.reserve(items.len(), size_of::<&str>())?;
    let mut names = Vec::with_capacity(items.len());
    for item in items {
        meter.vertex()?;
        validate(item)?;
        names.push(item.as_str());
    }
    sort_dedup(&mut names, meter)?;
    Ok(names)
}

/// The visible node `selector` names; a due node does not resolve.
fn resolve<'g>(
    graph: &'g Graph,
    selector: Selector<'_>,
    expired_at: Option<u64>,
) -> Result<&'g GraphNode, GraphError> {
    let node = match selector {
        Selector::Id(id) => graph.node(id),
        Selector::Key(key) => {
            validate_node_key(key)?;
            graph.node_by_key(key)
        }
    };
    match node {
        Some(node) if expired_at.is_none_or(|now| !graph.node_expired(node.id, now)) => Ok(node),
        _ => Err(match selector {
            Selector::Id(id) => GraphError::NodeNotFound(id),
            Selector::Key(key) => GraphError::UnknownNodeKey(key.to_owned()),
        }),
    }
}

/// Whether `node` carries every label of the sorted `labels`.
fn carries(node: &GraphNode, labels: &[&str]) -> bool {
    labels.iter().all(|label| node.labels.contains(*label))
}

/// Comparisons charged for sorting `count` items: `count * bit_length(count)`.
fn sort_cost(count: usize) -> u64 {
    let count = count as u64;
    count * u64::from(u64::BITS - count.leading_zeros())
}

#[cfg(test)]
thread_local! {
    /// Sort work done on this thread: one per element comparison or move,
    /// duplicate checks included (tests only).
    static SORTED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
thread_local! {
    /// Items a growing buffer moved to its new allocation (tests only).
    static MOVED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Records one moved item (a no-op outside tests).
fn moved() {
    #[cfg(test)]
    MOVED.with(|count| count.set(count.get() + 1));
}

/// Records one unit of sort work (a no-op outside tests).
fn sorted() {
    #[cfg(test)]
    SORTED.with(|count| count.set(count.get() + 1));
}

/// Sorts `items` ascending by `less`, stably (ties keep their order). See
/// [`sort_dedup`].
fn sort_by<T: Copy>(
    items: &mut Vec<T>,
    less: impl Fn(&T, &T) -> bool,
    meter: &mut Meter<'_>,
) -> Result<(), AlgorithmTruncation> {
    merge_sort(items, less, |_, _| false, meter)
}

/// Sorts `items` ascending and keeps the first of each run of equal items,
/// as `sort_unstable` then `dedup` would for a total order.
fn sort_dedup<T: Copy + Ord>(
    items: &mut Vec<T>,
    meter: &mut Meter<'_>,
) -> Result<(), AlgorithmTruncation> {
    merge_sort(items, |a, b| a < b, |a, b| a == b, meter)
}

/// A stable bottom-up merge sort under the meter. A buffer of
/// `items.len()` items is reserved before it is allocated. Each pass
/// examines every item once, with at most two comparisons (the merge and,
/// in the last pass, `same` against the last kept item, which drops it),
/// for one vertex unit; the `sort_cost` units beyond the `ceil(log2 n)`
/// passes are charged after them, so a sort costs exactly `sort_cost(n)`
/// units, each checked as it runs.
fn merge_sort<T: Copy>(
    items: &mut Vec<T>,
    less: impl Fn(&T, &T) -> bool,
    same: impl Fn(&T, &T) -> bool,
    meter: &mut Meter<'_>,
) -> Result<(), AlgorithmTruncation> {
    let count = items.len();
    meter.reserve(count, size_of::<T>())?;
    let mut buffer: Vec<T> = Vec::with_capacity(count);
    let mut examined = 0u64;
    let mut width = 1usize;
    while width < count {
        // The last pass merges one run of every item.
        let last = width >= count - width;
        buffer.clear();
        let mut low = 0;
        while low < count {
            let middle = low + width.min(count - low);
            let high = middle + width.min(count - middle);
            let (mut left, mut right) = (low, middle);
            while left < middle || right < high {
                meter.vertex()?;
                sorted();
                let item = if right < high && (left == middle || less(&items[right], &items[left]))
                {
                    right += 1;
                    items[right - 1]
                } else {
                    left += 1;
                    items[left - 1]
                };
                if !(last && buffer.last().is_some_and(|kept| same(kept, &item))) {
                    buffer.push(item);
                }
                examined += 1;
            }
            low = high;
        }
        std::mem::swap(items, &mut buffer);
        width = width.saturating_mul(2);
    }
    meter.vertices(sort_cost(count) - examined)
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

/// Appends `item`, first doubling a full buffer (at least 4 slots) into a
/// new allocation reserved at its full capacity; the old one stays charged.
/// Each moved item is charged one vertex unit as it moves.
fn push<T>(items: &mut Vec<T>, item: T, meter: &mut Meter<'_>) -> Result<(), AlgorithmTruncation> {
    if items.len() == items.capacity() {
        let capacity = items.capacity().saturating_mul(2).max(4);
        meter.reserve(capacity, size_of::<T>())?;
        let mut grown = Vec::with_capacity(capacity);
        for item in items.drain(..) {
            meter.vertex()?;
            moved();
            grown.push(item);
        }
        *items = grown;
    }
    items.push(item);
    Ok(())
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

/// The budgets of one run: edge units, vertex units and scratch bytes,
/// with one deadline and cancellation check before any work, before every
/// 64th charged operation (edge and vertex units together) and before
/// returning. A caller's condition conversion and staging (see
/// [`run_with`]) charge the same meter.
pub struct Meter<'a> {
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
    pub fn vertex(&mut self) -> Result<(), AlgorithmTruncation> {
        if self.vertices >= self.vertex_budget {
            return Err(AlgorithmTruncation::VertexLimit);
        }
        self.tick()?;
        self.vertices += 1;
        Ok(())
    }

    /// `count` vertex units, one at a time.
    pub fn vertices(&mut self, count: u64) -> Result<(), AlgorithmTruncation> {
        (0..count).try_for_each(|_| self.vertex())
    }

    /// Reserves `count` items of `bytes` each before they are allocated.
    pub fn reserve(&mut self, count: usize, bytes: usize) -> Result<(), AlgorithmTruncation> {
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

/// The prepared scope of one run with the TTL clock (`None` when nothing
/// is due).
struct Local<'a> {
    graph: &'a Graph,
    direction: Direction,
    /// Sorted, deduplicated relationship types; `None` is any.
    types: Option<&'a [&'a str]>,
    /// Sorted, deduplicated labels; `None` is any.
    labels: Option<&'a [&'a str]>,
    conditions: &'a [PropertyCondition],
    limits: &'a GraphLimits,
    expired_at: Option<u64>,
    /// Edge 0 for typed scans, which probe strictly after `(type, 0)`.
    edge_zero: Option<&'a GraphEdge>,
}

impl Local<'_> {
    /// Visible and carrying every scope label.
    fn eligible_node(&self, id: NodeId) -> bool {
        self.graph.node(id).is_some_and(|node| {
            self.expired_at
                .is_none_or(|now| !self.graph.node_expired(id, now))
                && self.labels.is_none_or(|labels| carries(node, labels))
        })
    }

    /// Visible (with both endpoints) and matching every edge condition; the
    /// adjacency scan already applied the relationship types.
    fn eligible_edge(&self, edge: &GraphEdge) -> bool {
        self.expired_at
            .is_none_or(|now| !self.graph.edge_expired(edge, now))
            && conditions_match(&edge.properties, self.conditions)
    }

    /// Calls `visit` with every adjacency entry of `node` in the scope's
    /// direction and types, in (type, edge ID) order, charging one edge
    /// unit per entry under the per-node cap. A typed scope probes each
    /// type with one lazy range lookup charged one vertex unit, whether or
    /// not the type occurs. An error from `visit` stops the scan.
    fn scan(
        &self,
        node: NodeId,
        meter: &mut Meter<'_>,
        mut visit: impl FnMut(&GraphEdge, &mut Meter<'_>) -> Result<(), Halt>,
    ) -> Result<(), Halt> {
        let mut examined = 0;
        let Some(types) = self.types else {
            for (_, id) in self.graph.adjacency(node, self.direction, None, None) {
                self.step(node, id, &mut examined, meter, &mut visit)?;
            }
            return Ok(());
        };
        for &kind in types {
            meter.vertex()?;
            // The lazy probe starts strictly after (kind, 0); edge 0, the
            // only entry it skips, is read directly.
            if let Some(first) = self
                .edge_zero
                .filter(|edge| edge.relationship == kind && self.touches(edge, node))
            {
                self.step(node, first.id, &mut examined, meter, &mut visit)?;
            }
            for (_, id) in self
                .graph
                .adjacency(node, self.direction, None, Some((kind, EdgeId(0))))
                .take_while(|(found, _)| *found == kind)
            {
                self.step(node, id, &mut examined, meter, &mut visit)?;
            }
        }
        Ok(())
    }

    /// Whether `node`'s adjacency in the scope's direction holds `edge`.
    fn touches(&self, edge: &GraphEdge, node: NodeId) -> bool {
        match self.direction {
            Direction::Out => edge.source == node,
            Direction::In => edge.destination == node,
            Direction::Both => edge.source == node || edge.destination == node,
        }
    }

    /// One adjacency entry of `node`: the per-node cap, one edge unit and
    /// `visit`.
    fn step(
        &self,
        node: NodeId,
        id: EdgeId,
        examined: &mut u64,
        meter: &mut Meter<'_>,
        visit: &mut impl FnMut(&GraphEdge, &mut Meter<'_>) -> Result<(), Halt>,
    ) -> Result<(), Halt> {
        if *examined >= self.limits.max_edges_per_node {
            return Err(AlgorithmTruncation::DegreeLimit {
                node_ids: vec![node],
            }
            .into());
        }
        meter.edge()?;
        *examined += 1;
        let edge = self.graph.edge(id).ok_or(GraphError::InvariantViolation(
            "adjacency names a missing edge",
        ))?;
        visit(edge, meter)
    }

    /// Level-synchronous breadth-first discovery from the seeds up to
    /// `max_hops`, then a rescan of every admitted node that retains each
    /// eligible edge between admitted nodes once.
    ///
    /// Each level scans its nodes in ascending order and collects the
    /// eligible new nodes they reach; at the level's end the collection is
    /// sorted, deduplicated, checked against `max_visited_nodes` and merged
    /// into the ascending admitted set, which then answers membership by
    /// binary search.
    fn extract(&self, seeds: &[NodeId], meter: &mut Meter<'_>) -> Result<Projection, Halt> {
        meter.reserve(seeds.len(), size_of::<NodeId>())?;
        let mut admitted = Vec::with_capacity(seeds.len());
        for &seed in seeds {
            meter.vertex()?;
            admitted.push(seed);
        }
        let mut frontier = Vec::new();
        for depth in 0..self.limits.max_hops {
            let level = if depth == 0 { seeds } else { &frontier[..] };
            if level.is_empty() {
                break;
            }
            let mut found = Vec::new();
            for &node in level {
                meter.vertex()?;
                self.scan(node, meter, |edge, meter| {
                    if !self.eligible_edge(edge) {
                        return Ok(());
                    }
                    let next = other_end(edge, node);
                    if found.last() == Some(&next)
                        || admitted.binary_search(&next).is_ok()
                        || !self.eligible_node(next)
                    {
                        return Ok(());
                    }
                    Ok(push(&mut found, next, meter)?)
                })?;
            }
            sort_dedup(&mut found, meter)?;
            if (admitted.len() + found.len()) as u64 > self.limits.max_visited_nodes {
                return Err(AlgorithmTruncation::VisitedLimit.into());
            }
            admitted = merge(&admitted, &found, meter)?;
            frontier = found;
        }
        drop(frontier);
        let nodes = admitted;
        let mut edges = Vec::new();
        for (position, &node) in nodes.iter().enumerate() {
            meter.vertex()?;
            self.scan(node, meter, |edge, meter| {
                // Both meets every edge from each end; keep it at its source.
                if self.direction == Direction::Both && edge.source != node {
                    return Ok(());
                }
                if !self.eligible_edge(edge) {
                    return Ok(());
                }
                let Ok(other) = nodes.binary_search(&other_end(edge, node)) else {
                    return Ok(());
                };
                let (source, destination) = if edge.source == node {
                    (position, other)
                } else {
                    (other, position)
                };
                let projected = ProjectedEdge {
                    id: edge.id,
                    source,
                    destination,
                };
                Ok(push(&mut edges, projected, meter)?)
            })?;
        }
        sort_by(&mut edges, |a, b| a.id < b.id, meter)?;
        Ok(Projection { nodes, edges })
    }
}

/// The ascending union of two disjoint ascending node lists in a new
/// buffer, one vertex unit per node.
fn merge(left: &[NodeId], right: &[NodeId], meter: &mut Meter<'_>) -> Result<Vec<NodeId>, Halt> {
    let total = left.len() + right.len();
    meter.reserve(total, size_of::<NodeId>())?;
    let mut merged = Vec::with_capacity(total);
    let (mut i, mut j) = (0, 0);
    while i < left.len() || j < right.len() {
        meter.vertex()?;
        if j == right.len() || (i < left.len() && left[i] < right[j]) {
            merged.push(left[i]);
            i += 1;
        } else {
            merged.push(right[j]);
            j += 1;
        }
    }
    Ok(merged)
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
    let mut rows = Vec::with_capacity(count);
    rows.extend(projection.nodes.iter().map(|&node| DegreeRow {
        node,
        in_degree: 0,
        out_degree: 0,
        incident_degree: 0,
    }));
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
        let direction = self.direction;
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
        let mut cursor = Vec::with_capacity(count);
        cursor.extend_from_slice(&offsets[..count]);
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

        // The walk back to the source, then the path's node and edge IDs.
        let hops = winner % layers;
        meter.vertices(hops as u64 + 1)?;
        meter.reserve(hops + 1, size_of::<usize>())?;
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
        let mut path_nodes = Vec::with_capacity(hops + 1);
        path_nodes.extend(positions.iter().rev().map(|&position| nodes[position]));
        let mut path_edges = Vec::with_capacity(hops);
        path_edges.extend_from_slice(&search.sequences[winner][..hops]);
        Ok(WeightedPathOutcome::Found(WeightedPath {
            cost,
            nodes: path_nodes,
            edges: path_edges,
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

mod iterative;

#[cfg(test)]
mod tests;
