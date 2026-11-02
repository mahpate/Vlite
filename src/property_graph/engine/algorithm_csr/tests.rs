use super::{CsrSource, EdgeRef};
use crate::compact::graph::view::history::{pinned_views, pinned_views_reversed_records};
use crate::compact::graph::view::{CompactGraphView, algorithm_test_take_visits};
use crate::property_graph::engine::algorithm::{
    Algorithm, AlgorithmError, AlgorithmOutput, AlgorithmResult, AlgorithmScope,
    AlgorithmTruncation, CommunityAssignment, DegreeRow, GraphAlgorithmLimits, Job,
    LabelPropagationOutput, NodeScore, PageRankOutput, PathRequest, RankRequest, Request, Source,
    WeightedPath, WeightedPathOutcome, run_source_with,
};
use crate::property_graph::engine::codec::TemporalEntry;
use crate::property_graph::engine::temporal::Segment;
use crate::property_graph::engine::{
    Direction, EdgeId, ExecutionContext, Graph, GraphBody, GraphEntry, GraphId, GraphLimits,
    GraphState, NodeId, NodeSelector, Properties, PropertyCondition, PropertyValue,
};
use crate::test_globals::{BLOCKS, LIVE, MOST_BLOCKS, PEAK, measuring};
use std::borrow::Cow;
use std::cell::Cell;
use std::collections::BTreeSet;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;

const GRAPH: GraphId = GraphId(0);
const NORMAL_NOW: u64 = 0;
const EXPIRY_BEFORE: u64 = 9;
const EXPIRY_AT: u64 = 10;
const EXPIRY_AFTER: u64 = 11;
const ALGORITHM_KINDS: [usize; 5] = [0, 1, 2, 3, 4];
const DIRECTIONS: [Direction; 3] = [Direction::Out, Direction::In, Direction::Both];
const HARNESS_THREAD_SETTINGS: [usize; 3] = [1, 2, 4];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConstructionOrder {
    Canonical,
    ReverseSourceRecords,
}

impl ConstructionOrder {
    const ALL: [Self; 2] = [Self::Canonical, Self::ReverseSourceRecords];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceMode {
    Resident,
    CsrBase,
    CsrBaseOverlay,
}

const SOURCE_MODES: [SourceMode; 3] = [
    SourceMode::Resident,
    SourceMode::CsrBase,
    SourceMode::CsrBaseOverlay,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BudgetProfile {
    Edge,
    Vertex,
    Scratch,
    Output,
}

impl BudgetProfile {
    const ALL: [Self; 4] = [Self::Edge, Self::Vertex, Self::Scratch, Self::Output];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    Chain,
    WeightedDiamond,
    DanglingAndIsolates,
    ParallelLoopsAndZero,
    SparseIdsAndTypes,
    LabelsAndConditions,
    TtlBoundaries,
    TemporalOrdinaryRecord,
    OverlayReplacementAndDelete,
    HighDegreeAndAbsentTypes,
    InvalidInputsAndWeights,
    BudgetAndPartialOutput,
}

impl Family {
    const ALL: [Self; 12] = [
        Self::Chain,
        Self::WeightedDiamond,
        Self::DanglingAndIsolates,
        Self::ParallelLoopsAndZero,
        Self::SparseIdsAndTypes,
        Self::LabelsAndConditions,
        Self::TtlBoundaries,
        Self::TemporalOrdinaryRecord,
        Self::OverlayReplacementAndDelete,
        Self::HighDegreeAndAbsentTypes,
        Self::InvalidInputsAndWeights,
        Self::BudgetAndPartialOutput,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Chain => "chain",
            Self::WeightedDiamond => "weighted diamond",
            Self::DanglingAndIsolates => "dangling and isolates",
            Self::ParallelLoopsAndZero => "parallel, loops, and edge zero",
            Self::SparseIdsAndTypes => "sparse IDs and types",
            Self::LabelsAndConditions => "labels and conditions",
            Self::TtlBoundaries => "TTL boundaries",
            Self::TemporalOrdinaryRecord => "temporal ordinary record",
            Self::OverlayReplacementAndDelete => "overlay replacement and delete",
            Self::HighDegreeAndAbsentTypes => "high degree and absent types",
            Self::InvalidInputsAndWeights => "invalid inputs and weights",
            Self::BudgetAndPartialOutput => "budget and partial output",
        }
    }

    fn overlay_changes_logical_graph(self) -> bool {
        self == Self::OverlayReplacementAndDelete
    }
}

#[derive(Clone, Copy)]
struct EdgeSpec {
    id: u64,
    source: u64,
    destination: u64,
    relationship: &'static str,
    weight: Option<i64>,
    temporal: bool,
}

fn edge(
    id: u64,
    source: u64,
    destination: u64,
    relationship: &'static str,
    weight: i64,
) -> EdgeSpec {
    EdgeSpec {
        id,
        source,
        destination,
        relationship,
        weight: Some(weight),
        temporal: false,
    }
}

fn props(weight: Option<i64>) -> Properties {
    let mut properties = Properties::new();
    properties.insert("marker".into(), PropertyValue::Integer(1));
    if let Some(weight) = weight {
        properties.insert("w".into(), PropertyValue::Integer(weight));
    }
    properties
}

fn put_node(id: u64, key: &str, labels: &[&str]) -> GraphEntry {
    GraphEntry::PutNode {
        node_id: NodeId(id),
        key: key.into(),
        labels: labels.iter().map(|label| (*label).into()).collect(),
        properties: props(None),
    }
}

fn apply(state: &mut GraphState, sequence: u64, entries: Vec<GraphEntry>) {
    state
        .apply(
            sequence,
            &GraphBody {
                graph_id: GRAPH,
                entries,
            },
        )
        .unwrap();
}

fn fixture(family: Family) -> Vec<GraphState> {
    let ids = if family == Family::SparseIdsAndTypes {
        [0, 7, 100, 500]
    } else {
        [0, 1, 2, 3]
    };
    let node_keys = ["a", "b", "c", "d"];
    let mut entries = vec![GraphEntry::CreateGraph {
        name: "csr_parity".into(),
    }];
    for (index, (&id, key)) in ids.iter().zip(node_keys).enumerate() {
        let labels: &[&str] = if family == Family::LabelsAndConditions && index == 0 {
            &["A", "B"]
        } else if family == Family::LabelsAndConditions && index == 2 {
            &["B"]
        } else {
            &["A"]
        };
        entries.push(put_node(id, key, labels));
    }

    let mut edges = match family {
        Family::Chain | Family::BudgetAndPartialOutput => vec![
            edge(0, ids[0], ids[1], "R", 1),
            edge(1, ids[1], ids[2], "R", 1),
            edge(2, ids[2], ids[3], "R", 1),
        ],
        Family::WeightedDiamond => vec![
            edge(0, ids[0], ids[1], "R", 1),
            edge(1, ids[0], ids[2], "S", 1),
            edge(2, ids[1], ids[3], "T", 1),
            edge(3, ids[2], ids[3], "U", 1),
        ],
        Family::DanglingAndIsolates => vec![
            edge(0, ids[0], ids[1], "R", 1),
            edge(1, ids[1], ids[2], "R", 1),
        ],
        Family::ParallelLoopsAndZero => vec![
            edge(0, ids[0], ids[1], "R", 1),
            edge(1, ids[0], ids[1], "R", 2),
            edge(2, ids[1], ids[1], "R", 3),
            edge(3, ids[1], ids[3], "S", 1),
        ],
        Family::SparseIdsAndTypes => vec![
            edge(0, ids[0], ids[1], "R", 1),
            edge(17, ids[1], ids[2], "S", 1),
            edge(99, ids[2], ids[3], "R", 1),
        ],
        Family::LabelsAndConditions => vec![
            edge(0, ids[0], ids[1], "R", 1),
            edge(1, ids[1], ids[2], "R", 2),
            edge(2, ids[2], ids[3], "R", 1),
        ],
        Family::TtlBoundaries => vec![
            edge(0, ids[0], ids[1], "R", 1),
            edge(1, ids[1], ids[2], "R", 1),
            edge(2, ids[2], ids[3], "R", 1),
        ],
        Family::TemporalOrdinaryRecord => {
            let mut first = edge(0, ids[0], ids[1], "R", 0);
            first.weight = None;
            first.temporal = true;
            vec![
                first,
                edge(1, ids[1], ids[2], "R", 1),
                edge(2, ids[2], ids[3], "R", 1),
            ]
        }
        Family::OverlayReplacementAndDelete => vec![
            edge(0, ids[0], ids[1], "R", 1),
            edge(1, ids[1], ids[3], "S", 1),
        ],
        Family::HighDegreeAndAbsentTypes => {
            let mut rows: Vec<_> = (0..24).map(|id| edge(id, ids[0], ids[1], "R", 1)).collect();
            rows.push(edge(24, ids[1], ids[3], "R", 1));
            rows
        }
        Family::InvalidInputsAndWeights => vec![
            edge(0, ids[0], ids[1], "R", -1),
            edge(1, ids[1], ids[3], "R", 1),
        ],
    };
    edges.sort_by_key(|row| row.id);
    for row in edges {
        if row.temporal {
            entries.push(GraphEntry::Temporal(TemporalEntry::PutEdge {
                edge_id: EdgeId(row.id),
                revision: 0,
                source: NodeId(row.source),
                destination: NodeId(row.destination),
                relationship: row.relationship.into(),
                key: None,
                segments: vec![Segment {
                    valid_from: None,
                    valid_to: None,
                    properties: props(Some(99)),
                }],
            }));
        } else {
            entries.push(GraphEntry::PutEdge {
                edge_id: EdgeId(row.id),
                source: NodeId(row.source),
                destination: NodeId(row.destination),
                relationship: row.relationship.into(),
                key: None,
                properties: props(row.weight),
            });
        }
    }
    if family == Family::TtlBoundaries {
        entries.push(GraphEntry::NodeExpiry {
            node_id: NodeId(ids[2]),
            expires_at_ms: Some(EXPIRY_AT),
        });
        entries.push(GraphEntry::EdgeExpiry {
            edge_id: EdgeId(1),
            expires_at_ms: Some(EXPIRY_AT),
        });
    }

    let mut state = GraphState::new();
    if family == Family::TemporalOrdinaryRecord {
        state.set_temporal(true).unwrap();
    }
    apply(&mut state, 1, entries);
    let base = state.clone();
    let overlay_entries = if family.overlay_changes_logical_graph() {
        vec![
            GraphEntry::PatchEdge {
                edge_id: EdgeId(0),
                set: props(Some(7)),
                unset: BTreeSet::new(),
            },
            GraphEntry::DeleteEdge { edge_id: EdgeId(1) },
        ]
    } else {
        vec![GraphEntry::PatchNode {
            node_id: NodeId(ids[0]),
            add_labels: BTreeSet::new(),
            remove_labels: BTreeSet::new(),
            set: props(None),
            unset: BTreeSet::new(),
        }]
    };
    apply(&mut state, 2, overlay_entries);
    vec![base, state]
}

fn reverse_update(state: &GraphState, family: Family) -> GraphState {
    let graph = state.graph(GRAPH).unwrap();
    let nodes: Vec<_> = graph.nodes().collect();
    let edges: Vec<_> = graph.edges().collect();
    let mut entries: Vec<_> = nodes
        .iter()
        .rev()
        .map(|node| GraphEntry::PutNode {
            node_id: node.id,
            key: node.key.clone(),
            labels: node.labels.clone(),
            properties: node.properties.clone(),
        })
        .collect();
    entries.extend(
        edges
            .iter()
            .rev()
            .filter(|edge| family != Family::TemporalOrdinaryRecord || edge.id != EdgeId(0))
            .map(|edge| GraphEntry::PatchEdge {
                edge_id: edge.id,
                set: edge.properties.clone(),
                unset: BTreeSet::new(),
            }),
    );
    let mut reversed = state.clone();
    apply(&mut reversed, state.last_sequence() + 1, entries);
    reversed
}

fn algorithm(kind: usize) -> Algorithm {
    match kind {
        0 => Algorithm::Degree,
        1 => Algorithm::WeightedShortestPath {
            source: NodeSelector::Key("a".into()),
            destination: NodeSelector::Key("d".into()),
            weight_property: Some("w".into()),
        },
        2 => Algorithm::PageRank {
            damping: 0.85,
            tolerance: 1e-10,
            personalization: None,
            weight_property: Some("w".into()),
        },
        3 => Algorithm::LabelPropagation,
        4 => Algorithm::Betweenness { sampling_seed: 17 },
        _ => unreachable!(),
    }
}

fn scope(family: Family, direction: Direction) -> AlgorithmScope {
    let mut scope = AlgorithmScope {
        seeds: ["a", "d"]
            .into_iter()
            .map(|key| NodeSelector::Key(key.into()))
            .collect(),
        direction,
        types: None,
        labels: None,
        edge_conditions: Vec::new(),
    };
    match family {
        Family::SparseIdsAndTypes => scope.types = Some(vec!["R".into()]),
        Family::LabelsAndConditions => {
            scope.seeds = [NodeSelector::Key("a".into())].into();
            scope.labels = Some(vec!["A".into(), "B".into()]);
            scope.edge_conditions.push(PropertyCondition::Eq {
                field: "w".into(),
                value: PropertyValue::Integer(1),
            });
        }
        Family::HighDegreeAndAbsentTypes => scope.types = Some(vec!["Absent".into()]),
        _ => {}
    }
    scope
}

fn limits(family: Family) -> (GraphLimits, GraphAlgorithmLimits) {
    let mut limits = GraphLimits {
        max_hops: 3,
        ..GraphLimits::DEFAULT
    };
    let mut caps = GraphAlgorithmLimits::DEFAULT;
    if family == Family::BudgetAndPartialOutput {
        limits.max_output_nodes = 1;
        caps.max_iterations = 1;
        caps.max_sampled_sources = 1;
    }
    (limits, caps)
}

fn budget_profile(profile: BudgetProfile) -> (GraphLimits, GraphAlgorithmLimits) {
    let mut limits = GraphLimits {
        max_hops: 3,
        ..GraphLimits::DEFAULT
    };
    let mut caps = GraphAlgorithmLimits::DEFAULT;
    match profile {
        BudgetProfile::Edge => limits.max_edges_examined = 1,
        BudgetProfile::Vertex => caps.max_vertex_operations = 1,
        BudgetProfile::Scratch => caps.max_scratch_bytes = 1,
        BudgetProfile::Output => {
            limits.max_output_nodes = 1;
            limits.max_output_edges = 1;
            limits.max_output_paths = 1;
        }
    }
    (limits, caps)
}

fn now_values(family: Family) -> &'static [u64] {
    if family == Family::TtlBoundaries {
        &[EXPIRY_BEFORE, EXPIRY_AT, EXPIRY_AFTER]
    } else {
        &[NORMAL_NOW]
    }
}

fn job(algorithm: &Algorithm) -> Job<'_, NodeSelector> {
    match algorithm {
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
        Algorithm::Betweenness { sampling_seed } => Job::Betweenness {
            sampling_seed: *sampling_seed,
        },
    }
}

fn execute<A: Source>(
    source: &A,
    scope: &AlgorithmScope,
    algorithm: &Algorithm,
    limits: &GraphLimits,
    caps: &GraphAlgorithmLimits,
    now_ms: u64,
    sequence: u64,
) -> Result<AlgorithmResult, AlgorithmError> {
    let clock = || 0;
    let context = ExecutionContext {
        now_ms,
        ..ExecutionContext::new(sequence, &clock)
    };
    execute_with_context(source, scope, algorithm, limits, caps, &context)
}

fn execute_with_context<A: Source>(
    source: &A,
    scope: &AlgorithmScope,
    algorithm: &Algorithm,
    limits: &GraphLimits,
    caps: &GraphAlgorithmLimits,
    context: &ExecutionContext<'_>,
) -> Result<AlgorithmResult, AlgorithmError> {
    let request = Request {
        seeds: &scope.seeds,
        direction: scope.direction,
        types: scope.types.as_deref(),
        labels: scope.labels.as_deref(),
        job: job(algorithm),
    };
    run_source_with(
        source,
        &request,
        limits,
        caps,
        context,
        |_| Ok(Cow::Borrowed(scope.edge_conditions.as_slice())),
        |_, _| Ok(()),
    )
    .map(|(result, _)| result)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SourceCounts {
    node: u64,
    node_by_key: u64,
    carries: u64,
    edge: u64,
    may_have_due: u64,
    node_expired: u64,
    edge_expired: u64,
    adjacency_calls: u64,
    adjacency_entries: u64,
}

struct CountingSource<'a, S> {
    source: &'a S,
    counts: Cell<SourceCounts>,
}

impl<'a, S> CountingSource<'a, S> {
    fn new(source: &'a S) -> Self {
        Self {
            source,
            counts: Cell::new(SourceCounts::default()),
        }
    }

    fn bump(&self, update: impl FnOnce(&mut SourceCounts)) {
        let mut counts = self.counts.get();
        update(&mut counts);
        self.counts.set(counts);
    }
}

impl<S: Source> Source for CountingSource<'_, S> {
    fn node(&self, id: NodeId) -> Option<NodeId> {
        self.bump(|counts| counts.node += 1);
        self.source.node(id)
    }

    fn node_by_key(&self, key: &str) -> Option<NodeId> {
        self.bump(|counts| counts.node_by_key += 1);
        self.source.node_by_key(key)
    }

    fn carries(&self, id: NodeId, labels: &[&str]) -> bool {
        self.bump(|counts| counts.carries += 1);
        self.source.carries(id, labels)
    }

    fn edge(&self, id: EdgeId) -> Option<EdgeRef<'_>> {
        self.bump(|counts| counts.edge += 1);
        self.source.edge(id)
    }

    fn may_have_due(&self, clock: u64) -> bool {
        self.bump(|counts| counts.may_have_due += 1);
        self.source.may_have_due(clock)
    }

    fn node_expired(&self, id: NodeId, clock: u64) -> bool {
        self.bump(|counts| counts.node_expired += 1);
        self.source.node_expired(id, clock)
    }

    fn edge_expired(&self, edge: &EdgeRef<'_>, clock: u64) -> bool {
        self.bump(|counts| counts.edge_expired += 1);
        self.source.edge_expired(edge, clock)
    }

    fn adjacency<'g>(
        &'g self,
        node: NodeId,
        direction: Direction,
        after: Option<(&str, EdgeId)>,
    ) -> impl Iterator<Item = (&'g str, EdgeId)> + 'g {
        self.bump(|counts| counts.adjacency_calls += 1);
        self.source
            .adjacency(node, direction, after)
            .inspect(move |_| {
                let mut counts = self.counts.get();
                counts.adjacency_entries += 1;
                self.counts.set(counts);
            })
    }
}

fn float_bits(result: &AlgorithmResult) -> Vec<u64> {
    match result.output.as_ref() {
        Some(AlgorithmOutput::WeightedShortestPath(WeightedPathOutcome::Found(path))) => {
            vec![path.cost.to_bits()]
        }
        Some(AlgorithmOutput::PageRank(output)) => output
            .scores
            .iter()
            .map(|score| score.score.to_bits())
            .chain(std::iter::once(output.residual.to_bits()))
            .collect(),
        Some(AlgorithmOutput::Betweenness(output)) => output
            .scores
            .iter()
            .map(|score| score.score.to_bits())
            .collect(),
        _ => Vec::new(),
    }
}

fn assert_same(
    expected: &Result<AlgorithmResult, AlgorithmError>,
    actual: &Result<AlgorithmResult, AlgorithmError>,
    context: &str,
) {
    assert_eq!(expected, actual, "{context}");
    if let (Ok(expected), Ok(actual)) = (expected, actual) {
        assert_eq!(
            float_bits(expected),
            float_bits(actual),
            "{context}: float bits"
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn assert_all_view_parity(
    graph: &Graph,
    views: &[CompactGraphView],
    scope: &AlgorithmScope,
    algorithm: &Algorithm,
    limits: &GraphLimits,
    caps: &GraphAlgorithmLimits,
    now_ms: u64,
    label: &str,
) -> Result<AlgorithmResult, AlgorithmError> {
    let resident = execute(graph, scope, algorithm, limits, caps, now_ms, 77);
    for (index, view) in views.iter().enumerate() {
        let compact = execute(
            &CsrSource::new(view),
            scope,
            algorithm,
            limits,
            caps,
            now_ms,
            77,
        );
        assert_same(
            &resident,
            &compact,
            &format!("{label} / {:?}", SOURCE_MODES[index + 1]),
        );
    }
    resident
}

fn scheduled_execution<S: Source>(
    source: &S,
    scope: &AlgorithmScope,
    algorithm: &Algorithm,
    limits: &GraphLimits,
    caps: &GraphAlgorithmLimits,
) -> (Result<AlgorithmResult, AlgorithmError>, usize) {
    let calls = Cell::new(0usize);
    let clock = || {
        let call = calls.get();
        calls.set(call + 1);
        u64::from(call > 0)
    };
    let context = ExecutionContext {
        view_sequence: 77,
        clock: &clock,
        cancel: None,
        now_ms: NORMAL_NOW,
    };
    (
        execute_with_context(source, scope, algorithm, limits, caps, &context),
        calls.get(),
    )
}

fn assert_source_counts(resident: SourceCounts, compact: SourceCounts, context: &str) {
    let normalize = |mut counts: SourceCounts| {
        counts.node_expired = 0;
        counts.edge_expired = 0;
        counts
    };
    assert_eq!(normalize(resident), normalize(compact), "{context}");
}

fn measured<T>(body: impl FnOnce() -> T) -> (T, isize, isize, isize) {
    let (live, blocks) = (LIVE.load(Relaxed), BLOCKS.load(Relaxed));
    let outer = (PEAK.swap(live, Relaxed), MOST_BLOCKS.swap(blocks, Relaxed));
    let value = body();
    let peak = PEAK.fetch_max(outer.0, Relaxed) - live;
    let retained = LIVE.load(Relaxed) - live;
    let block_peak = MOST_BLOCKS.fetch_max(outer.1, Relaxed) - blocks;
    (value, peak, retained, block_peak)
}

fn assert_retained_records(graph: &Graph, view: &CompactGraphView, family: Family) {
    let physical = view.physical();
    let resident_nodes: Vec<_> = graph.nodes().collect();
    for node in resident_nodes {
        let compact = physical
            .node(node.id)
            .unwrap_or_else(|| panic!("{}: compact node {} missing", family.name(), node.id.0));
        assert_eq!(compact.id, node.id, "{}", family.name());
        assert_eq!(compact.key, Some(node.key.as_str()), "{}", family.name());
        assert_eq!(
            compact.labels.iter().cloned().collect::<BTreeSet<_>>(),
            node.labels,
            "{}",
            family.name()
        );
        assert_eq!(compact.properties, &node.properties, "{}", family.name());
        assert_eq!(
            compact.expires_at_ms,
            graph.node_expiry(node.id),
            "{}",
            family.name()
        );
    }
    for edge in graph.edges() {
        let compact = physical
            .edge(edge.id)
            .unwrap_or_else(|| panic!("{}: compact edge {} missing", family.name(), edge.id.0));
        assert_eq!(compact.id, edge.id, "{}", family.name());
        assert_eq!(compact.source, edge.source, "{}", family.name());
        assert_eq!(compact.destination, edge.destination, "{}", family.name());
        assert_eq!(compact.relationship, edge.relationship, "{}", family.name());
        assert_eq!(compact.key, edge.key.as_deref(), "{}", family.name());
        assert_eq!(compact.properties, &edge.properties, "{}", family.name());
        assert_eq!(
            compact.expires_at_ms,
            graph.edge_expiry(edge.id),
            "{}",
            family.name()
        );
    }
    if family == Family::TemporalOrdinaryRecord {
        let ordinary = graph.edge(EdgeId(0)).unwrap();
        assert!(ordinary.properties.is_empty());
        assert!(
            view.read(NORMAL_NOW, Some(0))
                .edge(EdgeId(0))
                .unwrap()
                .segment
                .is_some()
        );
    }
}

#[test]
fn parity_matrix_matches_resident_base_and_pinned_overlay_sources() {
    assert_eq!(SOURCE_MODES.len(), 3);
    let mut compared = 0usize;
    let mut source_runs = 0usize;
    let mut manifest_cases = 0usize;
    let mut index_visits_by_order = [0usize; 2];
    for (order_index, order) in ConstructionOrder::ALL.into_iter().enumerate() {
        for family in Family::ALL {
            let states = fixture(family);
            let views = match order {
                ConstructionOrder::Canonical => pinned_views(GRAPH, &states, 4096),
                ConstructionOrder::ReverseSourceRecords => {
                    pinned_views_reversed_records(GRAPH, &states, 4096)
                }
            }
            .unwrap();
            assert_eq!(views.len(), 2, "{} / {order:?}", family.name());
            if family.overlay_changes_logical_graph() {
                for state_index in [0, 1] {
                    assert_retained_records(
                        states[state_index].graph(GRAPH).unwrap(),
                        &views[state_index],
                        family,
                    );
                }
            } else {
                let graph = states[0].graph(GRAPH).unwrap();
                assert_retained_records(graph, &views[0], family);
                assert_retained_records(graph, &views[1], family);
            }
            for now_ms in now_values(family) {
                let (limits, caps) = limits(family);
                for kind in ALGORITHM_KINDS {
                    let algorithm = algorithm(kind);
                    for direction in DIRECTIONS {
                        let scope = scope(family, direction);
                        manifest_cases += 1;
                        let label = format!(
                            "{} / {order:?} / algorithm {kind} / {direction:?} / now {now_ms}",
                            family.name()
                        );
                        if family.overlay_changes_logical_graph() {
                            for state_index in [0, 1] {
                                let csr_mode = if state_index == 0 {
                                    SOURCE_MODES[1]
                                } else {
                                    SOURCE_MODES[2]
                                };
                                let graph = states[state_index].graph(GRAPH).unwrap();
                                let resident_source = CountingSource::new(graph);
                                let csr_source = CsrSource::new(&views[state_index]);
                                let compact_source = CountingSource::new(&csr_source);
                                let resident = execute(
                                    &resident_source,
                                    &scope,
                                    &algorithm,
                                    &limits,
                                    &caps,
                                    *now_ms,
                                    77,
                                );
                                let _ = algorithm_test_take_visits();
                                let compact = execute(
                                    &compact_source,
                                    &scope,
                                    &algorithm,
                                    &limits,
                                    &caps,
                                    *now_ms,
                                    77,
                                );
                                index_visits_by_order[order_index] += algorithm_test_take_visits();
                                assert_same(
                                    &resident,
                                    &compact,
                                    &format!("{label} / {csr_mode:?}"),
                                );
                                assert_source_counts(
                                    resident_source.counts.get(),
                                    compact_source.counts.get(),
                                    &format!("{label} / {csr_mode:?}: source call counts"),
                                );
                                compared += 1;
                                source_runs += 2;
                            }
                        } else {
                            let graph = states[0].graph(GRAPH).unwrap();
                            let resident_source = CountingSource::new(graph);
                            let resident = execute(
                                &resident_source,
                                &scope,
                                &algorithm,
                                &limits,
                                &caps,
                                *now_ms,
                                77,
                            );
                            let mut prior_compact: Option<Result<AlgorithmResult, AlgorithmError>> =
                                None;
                            for (view_index, view) in views.iter().enumerate() {
                                let csr_mode = SOURCE_MODES[view_index + 1];
                                let csr_source = CsrSource::new(view);
                                let compact_source = CountingSource::new(&csr_source);
                                let _ = algorithm_test_take_visits();
                                let compact = execute(
                                    &compact_source,
                                    &scope,
                                    &algorithm,
                                    &limits,
                                    &caps,
                                    *now_ms,
                                    77,
                                );
                                index_visits_by_order[order_index] += algorithm_test_take_visits();
                                assert_same(
                                    &resident,
                                    &compact,
                                    &format!("{label} / {csr_mode:?}"),
                                );
                                assert_source_counts(
                                    resident_source.counts.get(),
                                    compact_source.counts.get(),
                                    &format!("{label} / {csr_mode:?}: source call counts"),
                                );
                                if let Some(base) = &prior_compact {
                                    assert_same(
                                        base,
                                        &compact,
                                        &format!("{label} / {csr_mode:?} pinned generation"),
                                    );
                                } else {
                                    prior_compact = Some(compact.clone());
                                }
                                compared += 1;
                                source_runs += 1;
                            }
                            source_runs += 1;
                        }
                    }
                }
            }
        }
        let states = fixture(Family::Chain);
        let views = match order {
            ConstructionOrder::Canonical => pinned_views(GRAPH, &states, 4096),
            ConstructionOrder::ReverseSourceRecords => {
                pinned_views_reversed_records(GRAPH, &states, 4096)
            }
        }
        .unwrap();
        let graph = states[0].graph(GRAPH).unwrap();
        for profile in BudgetProfile::ALL {
            let (limits, caps) = budget_profile(profile);
            for kind in ALGORITHM_KINDS {
                let algorithm = algorithm(kind);
                for direction in DIRECTIONS {
                    let mut scoped = scope(Family::Chain, direction);
                    if kind == 1 {
                        scoped.seeds = [NodeSelector::Key("a".into())].into();
                    }
                    manifest_cases += 1;
                    let label = format!(
                        "budget {profile:?} / {order:?} / algorithm {kind} / {direction:?}"
                    );
                    let resident_source = CountingSource::new(graph);
                    let resident = execute(
                        &resident_source,
                        &scoped,
                        &algorithm,
                        &limits,
                        &caps,
                        NORMAL_NOW,
                        77,
                    );
                    for (view_index, view) in views.iter().enumerate() {
                        let csr_mode = SOURCE_MODES[view_index + 1];
                        let csr_source = CsrSource::new(view);
                        let compact_source = CountingSource::new(&csr_source);
                        let _ = algorithm_test_take_visits();
                        let compact = execute(
                            &compact_source,
                            &scoped,
                            &algorithm,
                            &limits,
                            &caps,
                            NORMAL_NOW,
                            77,
                        );
                        index_visits_by_order[order_index] += algorithm_test_take_visits();
                        assert_same(&resident, &compact, &format!("{label} / {csr_mode:?}"));
                        assert_source_counts(
                            resident_source.counts.get(),
                            compact_source.counts.get(),
                            &format!("{label} / {csr_mode:?}: source call counts"),
                        );
                        compared += 1;
                        source_runs += 1;
                    }
                    source_runs += 1;
                }
            }
        }
    }
    assert_eq!(manifest_cases, 540);
    assert_eq!(compared, 1_080);
    assert_eq!(source_runs, 1_650);
    assert_eq!(index_visits_by_order[0], index_visits_by_order[1]);
    eprintln!(
        "parity manifest: cases={manifest_cases}, comparisons={compared}, source_runs={source_runs}, indexed_visits_per_order={}",
        index_visits_by_order[0]
    );
    assert_eq!(HARNESS_THREAD_SETTINGS, [1, 2, 4]);
}

#[test]
fn reverse_updates_preserve_ids_and_algorithm_results() {
    let mut cases = 0usize;
    let mut comparisons = 0usize;
    let mut source_runs = 0usize;
    for family in Family::ALL {
        let states = fixture(family);
        let canonical = &states[if family.overlay_changes_logical_graph() {
            1
        } else {
            0
        }];
        let reversed = reverse_update(canonical, family);
        let views = pinned_views(GRAPH, &[canonical.clone(), reversed.clone()], 4096).unwrap();
        let canonical_graph = canonical.graph(GRAPH).unwrap();
        let reversed_graph = reversed.graph(GRAPH).unwrap();
        for &now_ms in now_values(family) {
            for kind in ALGORITHM_KINDS {
                let (limits, caps) = limits(family);
                let algorithm = algorithm(kind);
                for direction in DIRECTIONS {
                    cases += 1;
                    let scope = scope(family, direction);
                    let expected = execute(
                        canonical_graph,
                        &scope,
                        &algorithm,
                        &limits,
                        &caps,
                        now_ms,
                        91,
                    );
                    let reverse_resident = execute(
                        reversed_graph,
                        &scope,
                        &algorithm,
                        &limits,
                        &caps,
                        now_ms,
                        91,
                    );
                    let reverse_csr = execute(
                        &CsrSource::new(&views[1]),
                        &scope,
                        &algorithm,
                        &limits,
                        &caps,
                        now_ms,
                        91,
                    );
                    assert_same(
                        &expected,
                        &reverse_resident,
                        &format!(
                            "{} / reversed resident / {kind} / {direction:?}",
                            family.name()
                        ),
                    );
                    assert_same(
                        &expected,
                        &reverse_csr,
                        &format!("{} / reversed CSR / {kind} / {direction:?}", family.name()),
                    );
                    comparisons += 2;
                    source_runs += 3;
                }
            }
        }
    }
    assert_eq!(cases, 210);
    assert_eq!(comparisons, 420);
    assert_eq!(source_runs, 630);
}

fn state_from_entries(entries: Vec<GraphEntry>, temporal: bool) -> GraphState {
    let mut state = GraphState::new();
    if temporal {
        state.set_temporal(true).unwrap();
    }
    apply(&mut state, 1, entries);
    state
}

fn high_degree_state(edge_count: u64) -> GraphState {
    let mut entries = four_node_prefix("csr_high_degree");
    entries.extend((0..edge_count).map(|id| GraphEntry::PutEdge {
        edge_id: EdgeId(id),
        source: NodeId(0),
        destination: NodeId(1),
        relationship: "R".into(),
        key: None,
        properties: props(Some(1)),
    }));
    state_from_entries(entries, false)
}

fn four_node_prefix(name: &str) -> Vec<GraphEntry> {
    let mut entries = vec![GraphEntry::CreateGraph { name: name.into() }];
    entries.extend([
        put_node(0, "a", &["A"]),
        put_node(1, "b", &["A"]),
        put_node(2, "c", &["A"]),
        put_node(3, "d", &["A"]),
    ]);
    entries
}

fn ttl_cascade_state() -> GraphState {
    let mut entries = four_node_prefix("csr_ttl_cascade");
    entries.extend([
        GraphEntry::PutEdge {
            edge_id: EdgeId(0),
            source: NodeId(0),
            destination: NodeId(1),
            relationship: "R".into(),
            key: None,
            properties: props(Some(1)),
        },
        GraphEntry::PutEdge {
            edge_id: EdgeId(1),
            source: NodeId(1),
            destination: NodeId(2),
            relationship: "R".into(),
            key: None,
            properties: props(Some(1)),
        },
        GraphEntry::PutEdge {
            edge_id: EdgeId(2),
            source: NodeId(0),
            destination: NodeId(2),
            relationship: "R".into(),
            key: None,
            properties: props(Some(1)),
        },
        GraphEntry::NodeExpiry {
            node_id: NodeId(1),
            expires_at_ms: Some(EXPIRY_AT),
        },
    ]);
    state_from_entries(entries, false)
}

#[test]
fn csr_physical_order_and_typed_probes_match() {
    let mut entries = four_node_prefix("csr_physical_order");
    entries.extend([
        GraphEntry::PutEdge {
            edge_id: EdgeId(0),
            source: NodeId(0),
            destination: NodeId(2),
            relationship: "Z".into(),
            key: None,
            properties: props(Some(1)),
        },
        GraphEntry::PutEdge {
            edge_id: EdgeId(1),
            source: NodeId(0),
            destination: NodeId(1),
            relationship: "A".into(),
            key: None,
            properties: props(Some(1)),
        },
    ]);
    let state = state_from_entries(entries, false);
    let graph = state.graph(GRAPH).unwrap();
    let views = pinned_views(GRAPH, std::slice::from_ref(&state), 4096).unwrap();
    let source = CsrSource::new(&views[0]);
    let actual = source
        .adjacency(NodeId(0), Direction::Out, None)
        .collect::<Vec<_>>();
    let expected = [("A", EdgeId(1)), ("Z", EdgeId(0))];
    assert_eq!(actual, expected);
    assert_eq!(
        actual,
        graph
            .adjacency(NodeId(0), Direction::Out, None, None)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        source
            .adjacency(NodeId(0), Direction::Out, Some(("A", EdgeId(1))))
            .collect::<Vec<_>>(),
        [("Z", EdgeId(0))]
    );

    let mut scoped = scope(Family::Chain, Direction::Out);
    scoped.seeds = ["a", "b", "c"]
        .into_iter()
        .map(|key| NodeSelector::Key(key.into()))
        .collect();
    scoped.types = Some(vec!["A".into(), "Z".into()]);
    let algorithm = Algorithm::Degree;
    let (limits, caps) = limits(Family::Chain);
    let resident = execute(graph, &scoped, &algorithm, &limits, &caps, NORMAL_NOW, 1);
    let compact = execute(&source, &scoped, &algorithm, &limits, &caps, NORMAL_NOW, 1);
    assert_same(&resident, &compact, "typed edge probes and physical order");
}

#[test]
fn csr_edge_zero_parallel_and_loops_match() {
    let states = fixture(Family::ParallelLoopsAndZero);
    let graph = states[0].graph(GRAPH).unwrap();
    let views = pinned_views(GRAPH, &states[..1], 4096).unwrap();
    let source = CsrSource::new(&views[0]);
    let mut scoped = scope(Family::ParallelLoopsAndZero, Direction::Out);
    scoped.seeds = ["a", "b", "d"]
        .into_iter()
        .map(|key| NodeSelector::Key(key.into()))
        .collect();
    scoped.types = Some(vec!["R".into()]);
    let (limits, caps) = limits(Family::ParallelLoopsAndZero);
    let expected_out = vec![
        DegreeRow {
            node: NodeId(0),
            in_degree: 0,
            out_degree: 2,
            incident_degree: 2,
        },
        DegreeRow {
            node: NodeId(1),
            in_degree: 3,
            out_degree: 1,
            incident_degree: 3,
        },
        DegreeRow {
            node: NodeId(3),
            in_degree: 0,
            out_degree: 0,
            incident_degree: 0,
        },
    ];
    for direction in DIRECTIONS {
        scoped.direction = direction;
        let expected = if direction == Direction::In {
            vec![
                DegreeRow {
                    node: NodeId(0),
                    in_degree: 2,
                    out_degree: 0,
                    incident_degree: 2,
                },
                DegreeRow {
                    node: NodeId(1),
                    in_degree: 1,
                    out_degree: 3,
                    incident_degree: 3,
                },
                DegreeRow {
                    node: NodeId(3),
                    in_degree: 0,
                    out_degree: 0,
                    incident_degree: 0,
                },
            ]
        } else {
            expected_out.clone()
        };
        let resident = execute(
            graph,
            &scoped,
            &Algorithm::Degree,
            &limits,
            &caps,
            NORMAL_NOW,
            1,
        );
        let compact = execute(
            &source,
            &scoped,
            &Algorithm::Degree,
            &limits,
            &caps,
            NORMAL_NOW,
            1,
        );
        assert_same(&resident, &compact, "edge-zero typed probe");
        assert_eq!(
            resident.unwrap().output,
            Some(AlgorithmOutput::Degree(expected.clone()))
        );
    }
}

#[test]
fn csr_ttl_equality_and_hidden_entry_charges_match() {
    let state = ttl_cascade_state();
    let graph = state.graph(GRAPH).unwrap();
    let views = pinned_views(GRAPH, std::slice::from_ref(&state), 4096).unwrap();
    let source = CsrSource::new(&views[0]);
    let due_node = source.node(NodeId(1)).unwrap();
    let first_edge = source.edge(EdgeId(0)).unwrap();
    let second_edge = source.edge(EdgeId(1)).unwrap();
    let live_edge = source.edge(EdgeId(2)).unwrap();
    assert!(!source.node_expired(due_node, EXPIRY_BEFORE));
    assert!(source.node_expired(due_node, EXPIRY_AT));
    assert!(source.node_expired(due_node, EXPIRY_AFTER));
    assert!(!source.edge_expired(&first_edge, EXPIRY_BEFORE));
    assert!(source.edge_expired(&first_edge, EXPIRY_AT));
    assert!(source.edge_expired(&second_edge, EXPIRY_AT));
    assert!(!source.edge_expired(&live_edge, EXPIRY_AT));
    assert_eq!(
        source
            .adjacency(NodeId(0), Direction::Out, None)
            .collect::<Vec<_>>(),
        [("R", EdgeId(0)), ("R", EdgeId(2))],
        "physical adjacency retains expired entries for algorithm-side charging"
    );

    let mut scoped = scope(Family::Chain, Direction::Both);
    scoped.seeds = ["a", "c", "d"]
        .into_iter()
        .map(|key| NodeSelector::Key(key.into()))
        .collect();
    let (limits, caps) = limits(Family::Chain);
    for now_ms in [EXPIRY_BEFORE, EXPIRY_AT, EXPIRY_AFTER] {
        for direction in DIRECTIONS {
            scoped.direction = direction;
            let resident = execute(
                graph,
                &scoped,
                &Algorithm::Degree,
                &limits,
                &caps,
                now_ms,
                1,
            );
            let compact = execute(
                &source,
                &scoped,
                &Algorithm::Degree,
                &limits,
                &caps,
                now_ms,
                1,
            );
            assert_same(
                &resident,
                &compact,
                &format!("TTL at {now_ms} / {direction:?}"),
            );
            if now_ms == EXPIRY_AT {
                let result = resident.unwrap();
                assert_eq!(
                    result
                        .projection
                        .unwrap()
                        .edges
                        .iter()
                        .map(|edge| edge.id)
                        .collect::<Vec<_>>(),
                    [EdgeId(2)]
                );
                assert!(result.stats.edges_examined > 1);
            }
        }
    }
}

#[test]
fn csr_weight_error_order_and_cancel_precedence_match() {
    let mut entries = four_node_prefix("csr_weight_order");
    entries.extend([
        GraphEntry::PutEdge {
            edge_id: EdgeId(0),
            source: NodeId(0),
            destination: NodeId(1),
            relationship: "Z".into(),
            key: None,
            properties: props(None),
        },
        GraphEntry::PutEdge {
            edge_id: EdgeId(1),
            source: NodeId(0),
            destination: NodeId(2),
            relationship: "A".into(),
            key: None,
            properties: props(Some(-1)),
        },
    ]);
    let state = state_from_entries(entries, false);
    let graph = state.graph(GRAPH).unwrap();
    let views = pinned_views(GRAPH, std::slice::from_ref(&state), 4096).unwrap();
    let source = CsrSource::new(&views[0]);
    let mut scoped = scope(Family::Chain, Direction::Out);
    scoped.seeds = [NodeSelector::Key("a".into())].into();
    let algorithm = Algorithm::PageRank {
        damping: 0.85,
        tolerance: 1e-10,
        personalization: None,
        weight_property: Some("w".into()),
    };
    let (limits, caps) = limits(Family::Chain);
    let resident = execute(graph, &scoped, &algorithm, &limits, &caps, NORMAL_NOW, 1);
    let compact = execute(&source, &scoped, &algorithm, &limits, &caps, NORMAL_NOW, 1);
    assert_eq!(resident, Err(AlgorithmError::MissingWeight(EdgeId(0))));
    assert_same(
        &resident,
        &compact,
        "weight validation follows physical edge ID order",
    );

    let clock = || 0;
    let cancel = AtomicBool::new(true);
    let context = ExecutionContext {
        view_sequence: 1,
        clock: &clock,
        cancel: Some(&cancel),
        now_ms: NORMAL_NOW,
    };
    let cancelled = run_source_with(
        &source,
        &Request {
            seeds: &scoped.seeds,
            direction: scoped.direction,
            types: None,
            labels: None,
            job: job(&algorithm),
        },
        &limits,
        &caps,
        &context,
        |_| Ok(Cow::Borrowed(scoped.edge_conditions.as_slice())),
        |_, _| Ok(()),
    )
    .map(|(result, _)| result)
    .unwrap();
    assert_eq!(cancelled.truncation, [AlgorithmTruncation::Cancelled]);
}

#[test]
fn csr_temporal_records_keep_retention_only_semantics() {
    let states = fixture(Family::TemporalOrdinaryRecord);
    let graph = states[0].graph(GRAPH).unwrap();
    let views = pinned_views(GRAPH, &states[..1], 4096).unwrap();
    let source = CsrSource::new(&views[0]);
    assert!(
        views[0]
            .read(NORMAL_NOW, Some(0))
            .edge(EdgeId(0))
            .unwrap()
            .properties
            .contains_key("w")
    );
    let mut scoped = scope(Family::TemporalOrdinaryRecord, Direction::Out);
    scoped.seeds = [NodeSelector::Key("a".into())].into();
    let algorithm = Algorithm::WeightedShortestPath {
        source: NodeSelector::Key("a".into()),
        destination: NodeSelector::Key("b".into()),
        weight_property: Some("w".into()),
    };
    let (limits, caps) = limits(Family::TemporalOrdinaryRecord);
    let resident = execute(graph, &scoped, &algorithm, &limits, &caps, NORMAL_NOW, 1);
    let compact = execute(&source, &scoped, &algorithm, &limits, &caps, NORMAL_NOW, 1);
    assert_eq!(resident, Err(AlgorithmError::MissingWeight(EdgeId(0))));
    assert_same(&resident, &compact, "algorithm T remains None");
}

#[test]
fn csr_key_misses_use_indexed_lookup() {
    let mut entries = vec![GraphEntry::CreateGraph {
        name: "csr_indexed_key_miss".into(),
    }];
    entries.extend((0..1024).map(|id| GraphEntry::PutNode {
        node_id: NodeId(id),
        key: format!("node-{id:04}"),
        labels: BTreeSet::new(),
        properties: Properties::new(),
    }));
    let state = state_from_entries(entries, false);
    let views = pinned_views(GRAPH, std::slice::from_ref(&state), 4096).unwrap();
    let _ = algorithm_test_take_visits();
    assert!(CsrSource::new(&views[0]).node_by_key("absent").is_none());
    let indexed_visits = algorithm_test_take_visits();
    assert!((1..=11).contains(&indexed_visits), "{indexed_visits}");
    eprintln!("indexed key miss: 1024 keys, {indexed_visits} index visits");
}

#[test]
fn csr_high_degree_scan_allocates_no_row_buffer() {
    let state = high_degree_state(1024);
    let graph = state.graph(GRAPH).unwrap();
    let views = pinned_views(GRAPH, std::slice::from_ref(&state), 4096).unwrap();
    let source = CsrSource::new(&views[0]);
    let _ = algorithm_test_take_visits();
    assert_eq!(
        source.adjacency(NodeId(0), Direction::Out, None).count(),
        1024
    );
    let visits = algorithm_test_take_visits();
    assert!((1024..=1027).contains(&visits), "{visits}");
    eprintln!("high-degree adjacency: 1024 yielded edges, {visits} index visits");

    let scoped = AlgorithmScope {
        seeds: [NodeSelector::Key("a".into())].into(),
        direction: Direction::Out,
        types: None,
        labels: None,
        edge_conditions: Vec::new(),
    };
    let algorithm = Algorithm::Degree;
    let limits = GraphLimits {
        max_edges_examined: 2_000,
        ..GraphLimits::DEFAULT
    };
    let caps = GraphAlgorithmLimits::DEFAULT;
    let measurement = measuring();
    measurement.session().enroll();
    let (resident, resident_peak, resident_retained, resident_blocks) =
        measured(|| execute(graph, &scoped, &algorithm, &limits, &caps, NORMAL_NOW, 1));
    let (compact, compact_peak, compact_retained, compact_blocks) =
        measured(|| execute(&source, &scoped, &algorithm, &limits, &caps, NORMAL_NOW, 1));
    drop(measurement);
    assert_same(&resident, &compact, "high-degree query");
    assert!(resident_peak > 0);
    assert_eq!(resident_peak, compact_peak);
    assert_eq!(resident_retained, compact_retained);
    assert_eq!(resident_blocks, compact_blocks);
    eprintln!(
        "allocator high-degree query: peak={}, retained={}, blocks={}",
        compact_peak, compact_retained, compact_blocks
    );
}

#[test]
fn csr_overlay_generation_stays_pinned() {
    let states = fixture(Family::OverlayReplacementAndDelete);
    let views = pinned_views_reversed_records(GRAPH, &states, 4096).unwrap();
    let base_graph = states[0].graph(GRAPH).unwrap();
    let overlay_graph = states[1].graph(GRAPH).unwrap();
    let base_source = CsrSource::new(&views[0]);
    let overlay_source = CsrSource::new(&views[1]);
    let scope = AlgorithmScope {
        seeds: [NodeSelector::Key("a".into())].into(),
        direction: Direction::Out,
        types: None,
        labels: None,
        edge_conditions: Vec::new(),
    };
    let algorithm = Algorithm::WeightedShortestPath {
        source: NodeSelector::Key("a".into()),
        destination: NodeSelector::Key("d".into()),
        weight_property: Some("w".into()),
    };
    let limits = GraphLimits::DEFAULT;
    let caps = GraphAlgorithmLimits::DEFAULT;
    let before = execute(
        &base_source,
        &scope,
        &algorithm,
        &limits,
        &caps,
        NORMAL_NOW,
        1,
    );
    assert_eq!(
        before.as_ref().unwrap().output,
        Some(AlgorithmOutput::WeightedShortestPath(
            WeightedPathOutcome::Found(WeightedPath {
                cost: 2.0,
                nodes: vec![NodeId(0), NodeId(1), NodeId(3)],
                edges: vec![EdgeId(0), EdgeId(1)],
            })
        ))
    );
    let replaced = execute(
        &overlay_source,
        &scope,
        &algorithm,
        &limits,
        &caps,
        NORMAL_NOW,
        1,
    );
    assert_eq!(
        replaced.as_ref().unwrap().output,
        Some(AlgorithmOutput::WeightedShortestPath(
            WeightedPathOutcome::NoPathWithinScopeAndHops
        ))
    );
    assert_same(
        &execute(
            base_graph, &scope, &algorithm, &limits, &caps, NORMAL_NOW, 1,
        ),
        &before,
        "pinned base before overlay",
    );
    assert_same(
        &execute(
            overlay_graph,
            &scope,
            &algorithm,
            &limits,
            &caps,
            NORMAL_NOW,
            1,
        ),
        &replaced,
        "overlay logical state",
    );
    assert_eq!(
        base_source
            .adjacency(NodeId(0), Direction::Out, None)
            .collect::<Vec<_>>(),
        [("R", EdgeId(0))]
    );
}

fn betweenness_scores(result: &AlgorithmResult) -> &[NodeScore] {
    match result.output.as_ref() {
        Some(AlgorithmOutput::Betweenness(output)) => &output.scores,
        other => panic!("expected betweenness output, got {other:?}"),
    }
}

#[test]
fn csr_betweenness_oracle_and_completed_prefix_match() {
    let states = fixture(Family::Chain);
    let views = pinned_views_reversed_records(GRAPH, &states, 4096).unwrap();
    let graph = states[0].graph(GRAPH).unwrap();
    let algorithm = Algorithm::Betweenness { sampling_seed: 17 };
    let limits = GraphLimits {
        max_hops: 3,
        ..GraphLimits::DEFAULT
    };
    let caps = GraphAlgorithmLimits {
        max_sampled_sources: 4,
        ..GraphAlgorithmLimits::DEFAULT
    };
    for direction in DIRECTIONS {
        let scope = AlgorithmScope {
            seeds: ["a", "b", "c", "d"]
                .into_iter()
                .map(|key| NodeSelector::Key(key.into()))
                .collect(),
            direction,
            types: None,
            labels: None,
            edge_conditions: Vec::new(),
        };
        let resident = execute(graph, &scope, &algorithm, &limits, &caps, NORMAL_NOW, 1).unwrap();
        let compact = execute(
            &CsrSource::new(&views[0]),
            &scope,
            &algorithm,
            &limits,
            &caps,
            NORMAL_NOW,
            1,
        )
        .unwrap();
        assert_same(
            &Ok(resident.clone()),
            &Ok(compact.clone()),
            "full Brandes run",
        );
        let middle = match direction {
            Direction::Out | Direction::In => 1.0 / 3.0,
            Direction::Both => 2.0 / 3.0,
        };
        assert_eq!(
            betweenness_scores(&resident)
                .iter()
                .map(|row| row.score)
                .collect::<Vec<_>>(),
            [0.0, middle, middle, 0.0]
        );
        let expected_selected = match resident.output.as_ref().unwrap() {
            AlgorithmOutput::Betweenness(output) => &output.selected_sources,
            _ => unreachable!(),
        };
        let mut partial = None;
        for max_vertex_operations in 1..=256 {
            let partial_caps = GraphAlgorithmLimits {
                max_vertex_operations,
                ..caps
            };
            let candidate = execute(
                &CsrSource::new(&views[0]),
                &scope,
                &algorithm,
                &limits,
                &partial_caps,
                NORMAL_NOW,
                1,
            )
            .unwrap();
            if candidate.output.is_some()
                && !candidate.computation_complete
                && !candidate.truncation.is_empty()
            {
                partial = Some((partial_caps, candidate));
                break;
            }
        }
        let (partial_caps, partial) = partial.expect("find a stop after a completed source");
        let resident_partial = execute(
            graph,
            &scope,
            &algorithm,
            &limits,
            &partial_caps,
            NORMAL_NOW,
            1,
        )
        .unwrap();
        assert_same(
            &Ok(resident_partial.clone()),
            &Ok(partial.clone()),
            "completed source prefix",
        );
        let AlgorithmOutput::Betweenness(output) = partial.output.unwrap() else {
            panic!("expected partial betweenness output")
        };
        assert!(!output.completed_sources.is_empty());
        assert!(output.completed_sources.len() < expected_selected.len());
        assert_eq!(
            output.completed_sources,
            expected_selected[..output.completed_sources.len()]
        );
    }
}

#[test]
fn csr_all_budget_boundaries_and_check_traces_match() {
    let states = fixture(Family::Chain);
    for order in ConstructionOrder::ALL {
        let views = match order {
            ConstructionOrder::Canonical => pinned_views(GRAPH, &states, 4096),
            ConstructionOrder::ReverseSourceRecords => {
                pinned_views_reversed_records(GRAPH, &states, 4096)
            }
        }
        .unwrap();
        let graph = states[0].graph(GRAPH).unwrap();
        let mut boundary_cases = 0usize;
        for kind in ALGORITHM_KINDS {
            let algorithm = algorithm(kind);
            for direction in DIRECTIONS {
                let mut scope = scope(Family::Chain, direction);
                if kind == 1 {
                    scope.seeds = [NodeSelector::Key("a".into())].into();
                }
                let (base_limits, base_caps) = limits(Family::Chain);
                let baseline = assert_all_view_parity(
                    graph,
                    &views,
                    &scope,
                    &algorithm,
                    &base_limits,
                    &base_caps,
                    NORMAL_NOW,
                    &format!("baseline / {order:?} / {kind} / {direction:?}"),
                )
                .unwrap();
                let used = baseline.stats;
                for (resource, amount) in [
                    ("edges_examined", used.edges_examined),
                    ("vertex_operations", used.vertex_operations),
                    ("scratch_bytes", used.scratch_bytes),
                ] {
                    let mut requested_values = vec![amount, amount + 1];
                    if amount > 1 {
                        requested_values.push(amount - 1);
                    }
                    for requested in requested_values {
                        let (mut limits, mut caps) = (base_limits, base_caps);
                        match resource {
                            "edges_examined" => limits.max_edges_examined = requested,
                            "vertex_operations" => caps.max_vertex_operations = requested,
                            "scratch_bytes" => caps.max_scratch_bytes = requested,
                            _ => unreachable!(),
                        }
                        let _ = assert_all_view_parity(
                            graph,
                            &views,
                            &scope,
                            &algorithm,
                            &limits,
                            &caps,
                            NORMAL_NOW,
                            &format!("{resource}={requested} / {order:?} / {kind} / {direction:?}"),
                        );
                        boundary_cases += 1;
                    }
                }
                for (resource, values) in [
                    ("max_output_nodes", [0, 1, 2]),
                    ("max_output_edges", [0, 1, 2]),
                    ("max_output_paths", [0, 1, 2]),
                ] {
                    for requested in values {
                        let mut limits = base_limits;
                        match resource {
                            "max_output_nodes" => limits.max_output_nodes = requested,
                            "max_output_edges" => limits.max_output_edges = requested,
                            "max_output_paths" => limits.max_output_paths = requested,
                            _ => unreachable!(),
                        }
                        let _ = assert_all_view_parity(
                            graph,
                            &views,
                            &scope,
                            &algorithm,
                            &limits,
                            &base_caps,
                            NORMAL_NOW,
                            &format!("{resource}={requested} / {order:?} / {kind} / {direction:?}"),
                        );
                        boundary_cases += 1;
                    }
                }
                for requested in [1, 3, 4] {
                    let limits = GraphLimits {
                        max_visited_nodes: requested,
                        ..base_limits
                    };
                    let _ = assert_all_view_parity(
                        graph,
                        &views,
                        &scope,
                        &algorithm,
                        &limits,
                        &base_caps,
                        NORMAL_NOW,
                        &format!(
                            "max_visited_nodes={requested} / {order:?} / {kind} / {direction:?}"
                        ),
                    );
                    boundary_cases += 1;
                }
                for requested in [0, 1, 2, 3] {
                    let limits = GraphLimits {
                        max_hops: requested,
                        ..base_limits
                    };
                    let _ = assert_all_view_parity(
                        graph,
                        &views,
                        &scope,
                        &algorithm,
                        &limits,
                        &base_caps,
                        NORMAL_NOW,
                        &format!("max_hops={requested} / {order:?} / {kind} / {direction:?}"),
                    );
                    boundary_cases += 1;
                }
                if matches!(kind, 2 | 3) {
                    for requested in [1, 2, 3] {
                        let caps = GraphAlgorithmLimits {
                            max_iterations: requested,
                            ..base_caps
                        };
                        let _ = assert_all_view_parity(
                            graph,
                            &views,
                            &scope,
                            &algorithm,
                            &base_limits,
                            &caps,
                            NORMAL_NOW,
                            &format!(
                                "max_iterations={requested} / {order:?} / {kind} / {direction:?}"
                            ),
                        );
                        boundary_cases += 1;
                    }
                }
                if kind == 4 {
                    for requested in [1, 2, 4] {
                        let caps = GraphAlgorithmLimits {
                            max_sampled_sources: requested,
                            ..base_caps
                        };
                        let _ = assert_all_view_parity(
                            graph,
                            &views,
                            &scope,
                            &algorithm,
                            &base_limits,
                            &caps,
                            NORMAL_NOW,
                            &format!(
                                "max_sampled_sources={requested} / {order:?} / {kind} / {direction:?}"
                            ),
                        );
                        boundary_cases += 1;
                    }
                }
            }
        }

        let dense = high_degree_state(4);
        let dense_graph = dense.graph(GRAPH).unwrap();
        let dense_views = match order {
            ConstructionOrder::Canonical => pinned_views(GRAPH, std::slice::from_ref(&dense), 4096),
            ConstructionOrder::ReverseSourceRecords => {
                pinned_views_reversed_records(GRAPH, std::slice::from_ref(&dense), 4096)
            }
        }
        .unwrap();
        let degree_scope = AlgorithmScope {
            seeds: [NodeSelector::Key("a".into())].into(),
            direction: Direction::Out,
            types: None,
            labels: None,
            edge_conditions: Vec::new(),
        };
        for direction in DIRECTIONS {
            let scope = AlgorithmScope {
                direction,
                ..degree_scope.clone()
            };
            for requested in [3, 4, 5] {
                let limits = GraphLimits {
                    max_edges_per_node: requested,
                    ..GraphLimits::DEFAULT
                };
                let _ = assert_all_view_parity(
                    dense_graph,
                    &dense_views,
                    &scope,
                    &Algorithm::Degree,
                    &limits,
                    &GraphAlgorithmLimits::DEFAULT,
                    NORMAL_NOW,
                    &format!("max_edges_per_node={requested} / {order:?} / {direction:?}"),
                );
                boundary_cases += 1;
            }
        }

        let cycle = cycle_state();
        let cycle_graph = cycle.graph(GRAPH).unwrap();
        let cycle_views = match order {
            ConstructionOrder::Canonical => pinned_views(GRAPH, std::slice::from_ref(&cycle), 4096),
            ConstructionOrder::ReverseSourceRecords => {
                pinned_views_reversed_records(GRAPH, std::slice::from_ref(&cycle), 4096)
            }
        }
        .unwrap();
        let mut scheduled_cases = 0usize;
        for kind in ALGORITHM_KINDS {
            let algorithm = algorithm(kind);
            for direction in DIRECTIONS {
                let scope = AlgorithmScope {
                    seeds: ["a", "b"]
                        .into_iter()
                        .map(|key| NodeSelector::Key(key.into()))
                        .collect(),
                    direction,
                    types: None,
                    labels: None,
                    edge_conditions: Vec::new(),
                };
                let limits = GraphLimits {
                    max_hops: 1,
                    deadline_ms: 1,
                    ..GraphLimits::DEFAULT
                };
                let resident = scheduled_execution(
                    cycle_graph,
                    &scope,
                    &algorithm,
                    &limits,
                    &GraphAlgorithmLimits::DEFAULT,
                );
                for view in &cycle_views {
                    let compact = scheduled_execution(
                        &CsrSource::new(view),
                        &scope,
                        &algorithm,
                        &limits,
                        &GraphAlgorithmLimits::DEFAULT,
                    );
                    assert_same(&resident.0, &compact.0, "deadline schedule");
                    assert_eq!(resident.1, compact.1, "deadline check trace");
                    scheduled_cases += 1;
                }
            }
        }
        assert_eq!(boundary_cases, 410);
        assert_eq!(scheduled_cases, 15);
        eprintln!(
            "boundary manifest {order:?}: limit cases={boundary_cases}, scheduled comparisons={scheduled_cases}"
        );
    }
}

#[test]
fn absent_type_probe_keeps_the_exact_vertex_charge() {
    let states = fixture(Family::Chain);
    let graph = states[0].graph(GRAPH).unwrap();
    let mut scoped = scope(Family::Chain, Direction::Out);
    scoped.seeds = [NodeSelector::Key("a".into())].into();
    scoped.types = Some(vec!["absent".into()]);
    let result = execute(
        graph,
        &scoped,
        &Algorithm::Degree,
        &GraphLimits::DEFAULT,
        &GraphAlgorithmLimits::DEFAULT,
        NORMAL_NOW,
        77,
    )
    .unwrap();
    assert_eq!(result.stats.vertex_operations, 11);
}

fn cycle_state() -> GraphState {
    let mut state = GraphState::new();
    apply(
        &mut state,
        1,
        vec![
            GraphEntry::CreateGraph {
                name: "csr_hand_oracle".into(),
            },
            put_node(0, "a", &["A"]),
            put_node(1, "b", &["A"]),
            GraphEntry::PutEdge {
                edge_id: EdgeId(0),
                source: NodeId(0),
                destination: NodeId(1),
                relationship: "R".into(),
                key: None,
                properties: props(Some(1)),
            },
            GraphEntry::PutEdge {
                edge_id: EdgeId(1),
                source: NodeId(1),
                destination: NodeId(0),
                relationship: "R".into(),
                key: None,
                properties: props(Some(1)),
            },
        ],
    );
    state
}

fn hand_run(
    graph: &Graph,
    algorithm: &Algorithm,
    direction: Direction,
) -> Result<AlgorithmResult, AlgorithmError> {
    let scope = AlgorithmScope {
        seeds: ["a", "b"]
            .into_iter()
            .map(|key| NodeSelector::Key(key.into()))
            .collect(),
        direction,
        types: None,
        labels: None,
        edge_conditions: Vec::new(),
    };
    execute(
        graph,
        &scope,
        algorithm,
        &GraphLimits {
            max_hops: 1,
            ..GraphLimits::DEFAULT
        },
        &GraphAlgorithmLimits::DEFAULT,
        NORMAL_NOW,
        1,
    )
}

fn scheduled_run<A: Source>(
    source: &A,
    algorithm: &Algorithm,
    direction: Direction,
    cancelled: bool,
) -> Result<AlgorithmResult, AlgorithmError> {
    let scope = AlgorithmScope {
        seeds: ["a", "b"]
            .into_iter()
            .map(|key| NodeSelector::Key(key.into()))
            .collect(),
        direction,
        types: None,
        labels: None,
        edge_conditions: Vec::new(),
    };
    let calls = Cell::new(0);
    let clock = || {
        let call = calls.get();
        calls.set(call + 1);
        u64::from(call > 0)
    };
    let cancel = AtomicBool::new(cancelled);
    let context = ExecutionContext {
        view_sequence: 123,
        clock: &clock,
        cancel: Some(&cancel),
        now_ms: NORMAL_NOW,
    };
    let deadline = GraphLimits {
        max_hops: 1,
        deadline_ms: 1,
        ..GraphLimits::DEFAULT
    };
    execute_with_context(
        source,
        &scope,
        algorithm,
        &deadline,
        &GraphAlgorithmLimits::DEFAULT,
        &context,
    )
}

#[test]
fn two_node_cycle_matches_hand_calculated_algorithm_results() {
    let state = cycle_state();
    let graph = state.graph(GRAPH).unwrap();
    let expected_nodes = vec![NodeId(0), NodeId(1)];
    for direction in [Direction::Out, Direction::In, Direction::Both] {
        let degree = hand_run(graph, &Algorithm::Degree, direction).unwrap();
        let expected_degrees = vec![
            DegreeRow {
                node: NodeId(0),
                in_degree: 1,
                out_degree: 1,
                incident_degree: 2,
            },
            DegreeRow {
                node: NodeId(1),
                in_degree: 1,
                out_degree: 1,
                incident_degree: 2,
            },
        ];
        assert_eq!(
            degree.output,
            Some(AlgorithmOutput::Degree(expected_degrees))
        );
        assert_eq!(degree.projection.as_ref().unwrap().nodes, expected_nodes);
        assert_eq!(
            degree
                .projection
                .as_ref()
                .unwrap()
                .edges
                .iter()
                .map(|edge| edge.id)
                .collect::<Vec<_>>(),
            [EdgeId(0), EdgeId(1)]
        );

        let path = hand_run(
            graph,
            &Algorithm::WeightedShortestPath {
                source: NodeSelector::Key("a".into()),
                destination: NodeSelector::Key("b".into()),
                weight_property: None,
            },
            direction,
        )
        .unwrap();
        let (expected_edge, expected_cost) = match direction {
            Direction::Out | Direction::Both => (EdgeId(0), 1.0),
            Direction::In => (EdgeId(1), 1.0),
        };
        assert_eq!(
            path.output,
            Some(AlgorithmOutput::WeightedShortestPath(
                WeightedPathOutcome::Found(WeightedPath {
                    cost: expected_cost,
                    nodes: vec![NodeId(0), NodeId(1)],
                    edges: vec![expected_edge],
                })
            ))
        );
        assert_eq!(
            float_bits(&path),
            [expected_cost.to_bits()],
            "path cost is compared by bits"
        );

        let rank = hand_run(
            graph,
            &Algorithm::PageRank {
                damping: 0.85,
                tolerance: 1e-10,
                personalization: None,
                weight_property: None,
            },
            direction,
        )
        .unwrap();
        assert_eq!(
            rank.output,
            Some(AlgorithmOutput::PageRank(PageRankOutput {
                scores: vec![
                    NodeScore {
                        node_id: 0,
                        score: 0.5,
                    },
                    NodeScore {
                        node_id: 1,
                        score: 0.5,
                    },
                ],
                iterations: 1,
                residual: 0.0,
                converged: true,
            }))
        );
        assert_eq!(
            float_bits(&rank),
            [0.5f64.to_bits(), 0.5f64.to_bits(), 0.0f64.to_bits()]
        );

        let labels = hand_run(graph, &Algorithm::LabelPropagation, direction).unwrap();
        assert_eq!(
            labels.output,
            Some(AlgorithmOutput::LabelPropagation(LabelPropagationOutput {
                assignments: vec![
                    CommunityAssignment {
                        node_id: 0,
                        community_id: 0,
                    },
                    CommunityAssignment {
                        node_id: 1,
                        community_id: 0,
                    },
                ],
                sweeps: 2,
                converged: true,
            }))
        );

        let between = hand_run(
            graph,
            &Algorithm::Betweenness { sampling_seed: 17 },
            direction,
        )
        .unwrap();
        let Some(AlgorithmOutput::Betweenness(ref output)) = between.output else {
            panic!("expected betweenness output")
        };
        assert_eq!(
            output.scores,
            vec![
                NodeScore {
                    node_id: 0,
                    score: 0.0,
                },
                NodeScore {
                    node_id: 1,
                    score: 0.0,
                },
            ]
        );
        assert_eq!(output.completed_source_count, 2);
        assert_eq!(output.completed_sources, output.selected_sources);
        assert!(!output.estimated);
        assert_eq!(float_bits(&between), [0.0f64.to_bits(), 0.0f64.to_bits()]);
    }
}

#[test]
fn fixed_cancellation_and_deadline_schedules_match_resident_base_and_overlay() {
    let state = cycle_state();
    let mut overlay = state.clone();
    apply(
        &mut overlay,
        2,
        vec![GraphEntry::PatchNode {
            node_id: NodeId(0),
            add_labels: BTreeSet::new(),
            remove_labels: BTreeSet::new(),
            set: props(None),
            unset: BTreeSet::new(),
        }],
    );
    let graph = state.graph(GRAPH).unwrap();
    let views = pinned_views(GRAPH, &[state.clone(), overlay], 16).unwrap();
    for cancelled in [true, false] {
        for kind in 0..5 {
            let algorithm = if kind == 1 {
                Algorithm::WeightedShortestPath {
                    source: NodeSelector::Key("a".into()),
                    destination: NodeSelector::Key("b".into()),
                    weight_property: None,
                }
            } else {
                algorithm(kind)
            };
            for direction in [Direction::Out, Direction::In, Direction::Both] {
                let resident = scheduled_run(graph, &algorithm, direction, cancelled);
                for view in &views {
                    let compact =
                        scheduled_run(&CsrSource::new(view), &algorithm, direction, cancelled);
                    assert_same(
                        &resident,
                        &compact,
                        &format!("schedule cancelled={cancelled}, {kind}, {direction:?}"),
                    );
                }
                if cancelled {
                    let result = resident.as_ref().unwrap();
                    assert_eq!(result.truncation, [AlgorithmTruncation::Cancelled]);
                } else {
                    let result = resident.as_ref().unwrap();
                    assert_eq!(result.truncation, [AlgorithmTruncation::Deadline]);
                }
            }
        }
    }
}
