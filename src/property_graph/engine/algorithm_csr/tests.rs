use super::{CsrSource, EdgeRef};
use crate::compact::graph::view::CompactGraphView;
use crate::compact::graph::view::history::pinned_views;
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
use std::borrow::Cow;
use std::cell::Cell;
use std::collections::BTreeSet;
use std::sync::atomic::AtomicBool;

const GRAPH: GraphId = GraphId(0);
const NORMAL_NOW: u64 = 0;
const EXPIRY_BEFORE: u64 = 9;
const EXPIRY_AT: u64 = 10;

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
        let labels: &[&str] = if family == Family::LabelsAndConditions && index == 2 {
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
            scope.labels = Some(vec!["A".into()]);
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

fn now_values(family: Family) -> &'static [u64] {
    if family == Family::TtlBoundaries {
        &[EXPIRY_BEFORE, EXPIRY_AT]
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

fn assert_source_counts(resident: SourceCounts, compact: SourceCounts, context: &str) {
    let normalize = |mut counts: SourceCounts| {
        counts.node_expired = 0;
        counts.edge_expired = 0;
        counts
    };
    assert_eq!(normalize(resident), normalize(compact), "{context}");
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
    let mut compared = 0usize;
    let mut source_runs = 0usize;
    for family in Family::ALL {
        let states = fixture(family);
        let views = pinned_views(GRAPH, &states, 4096).unwrap();
        assert_eq!(views.len(), 2, "{}", family.name());
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
            for kind in 0..5 {
                let algorithm = algorithm(kind);
                for direction in [Direction::Out, Direction::In, Direction::Both] {
                    let scope = scope(family, direction);
                    let label = format!(
                        "{} / algorithm {kind} / {direction:?} / now {now_ms}",
                        family.name()
                    );
                    if family.overlay_changes_logical_graph() {
                        for state_index in [0, 1] {
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
                            let compact = execute(
                                &compact_source,
                                &scope,
                                &algorithm,
                                &limits,
                                &caps,
                                *now_ms,
                                77,
                            );
                            assert_same(
                                &resident,
                                &compact,
                                &format!("{label} / state {state_index}"),
                            );
                            assert_source_counts(
                                resident_source.counts.get(),
                                compact_source.counts.get(),
                                &format!("{label} / state {state_index}: source call counts"),
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
                        for view in &views {
                            let csr_source = CsrSource::new(view);
                            let compact_source = CountingSource::new(&csr_source);
                            let compact = execute(
                                &compact_source,
                                &scope,
                                &algorithm,
                                &limits,
                                &caps,
                                *now_ms,
                                77,
                            );
                            assert_same(&resident, &compact, &label);
                            assert_source_counts(
                                resident_source.counts.get(),
                                compact_source.counts.get(),
                                &format!("{label}: source call counts"),
                            );
                            if let Some(base) = &prior_compact {
                                assert_same(
                                    base,
                                    &compact,
                                    &format!("{label} / pinned generation"),
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
    assert_eq!(compared, 390);
    assert_eq!(source_runs, 600);
}

#[test]
fn reverse_updates_preserve_ids_and_algorithm_results() {
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
            for kind in 0..5 {
                let (limits, caps) = limits(family);
                let algorithm = algorithm(kind);
                for direction in [Direction::Out, Direction::In, Direction::Both] {
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
                }
            }
        }
    }
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
