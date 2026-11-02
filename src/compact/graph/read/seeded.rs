use super::super::{GraphSource, decode, encode};
use super::*;
use crate::property_graph::engine::PropertyValue;
use crate::property_graph::engine::codec::TemporalEntry;
use crate::property_graph::engine::temporal::Segment;
use crate::property_graph::engine::{
    EdgeId, EdgeSelector, EdgeView, ExecutionContext, GraphBody, GraphEntry, GraphError, GraphId,
    GraphLimits, GraphQuery, GraphState, NodeId, NodeSelector, NodeView, QueryOp, QueryOutput,
    execute, execute_as_of,
};
use std::collections::HashSet;

const SEEDS: [u64; 8] = [
    0x4d59_5df4_d0f3_3173,
    0x1234_5678_9abc_def0,
    0x9e37_79b9_7f4a_7c15,
    0xd1b5_4a32_d192_ed03,
    0xa076_1d64_78bd_642f,
    0xe703_7ed1_a0b4_28db,
    0x8ebc_6af0_9c88_c6e3,
    0x5899_65cc_7537_4cc3,
];

struct Prng(u64);

impl Prng {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    fn next(&mut self) -> u64 {
        let mut value = self.0;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.0 = value;
        value
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() as usize) % bound
    }
}

fn properties(seed: u64, index: usize, prefix: &str) -> Properties {
    let mut properties = Properties::new();
    properties.insert(
        format!("{prefix}_number"),
        PropertyValue::Integer((seed ^ index as u64) as i64),
    );
    properties.insert(
        format!("{prefix}_text"),
        PropertyValue::String(format!("{prefix}-{seed:016x}-{index}")),
    );
    properties.insert(
        format!("{prefix}_flag"),
        PropertyValue::Boolean(index.is_multiple_of(2)),
    );
    properties
}

fn segment(
    seed: u64,
    edge_index: usize,
    segment_index: usize,
    valid_from: Option<i64>,
    valid_to: Option<i64>,
) -> Segment {
    Segment {
        valid_from,
        valid_to,
        properties: properties(seed, edge_index * 7 + segment_index, "segment"),
    }
}

fn make_state(seed: u64) -> GraphState {
    let mut random = Prng::new(seed);
    let node_ids: Vec<_> = (0..8).map(|index| 101 + index as u64 * 19).collect();
    let mut entries = vec![GraphEntry::CreateGraph {
        name: format!("seeded_{:x}", seed & 0xffff),
    }];

    for (index, id) in node_ids.iter().copied().enumerate() {
        entries.push(GraphEntry::PutNode {
            node_id: NodeId(id),
            key: if index == 7 {
                format!("节点_{seed:016x}")
            } else {
                format!("node_{seed:016x}_{index}")
            },
            labels: BTreeSet::from([format!("L{}", index % 3)]),
            properties: properties(seed, index, "node"),
        });
    }

    let node_deadlines = [
        Some(0),
        Some(7),
        Some(u64::MAX),
        None,
        None,
        Some(5),
        None,
        None,
    ];
    for (index, deadline) in node_deadlines.into_iter().enumerate() {
        if let Some(expires_at_ms) = deadline {
            entries.push(GraphEntry::NodeExpiry {
                node_id: NodeId(node_ids[index]),
                expires_at_ms: Some(expires_at_ms),
            });
        }
    }

    let relationships = ["A", "R0", "R1", "Z", "a"];
    let mut edge_deadlines = Vec::new();
    let mut add_edge = |index: usize,
                        source_index: usize,
                        target_index: usize,
                        relationship: &str,
                        temporal: Option<Vec<Segment>>,
                        expires_at_ms: Option<u64>| {
        let id = 37 + index as u64 * 29;
        let key =
            (index != 3 && !index.is_multiple_of(4)).then(|| format!("edge_{seed:016x}_{index}"));
        let properties = properties(seed, index, "edge");
        let entry = if let Some(segments) = temporal {
            GraphEntry::Temporal(TemporalEntry::PutEdge {
                edge_id: EdgeId(id),
                revision: 0,
                source: NodeId(node_ids[source_index]),
                destination: NodeId(node_ids[target_index]),
                relationship: relationship.to_owned(),
                key,
                segments,
            })
        } else {
            GraphEntry::PutEdge {
                edge_id: EdgeId(id),
                source: NodeId(node_ids[source_index]),
                destination: NodeId(node_ids[target_index]),
                relationship: relationship.to_owned(),
                key,
                properties,
            }
        };
        entries.push(entry);
        edge_deadlines.push((EdgeId(id), expires_at_ms));
    };

    // Keep difficult topology and interval cases present in every seeded graph.
    add_edge(0, 3, 3, "R1", Some(Vec::new()), None);
    add_edge(
        1,
        4,
        4,
        "A",
        Some(vec![
            segment(seed, 1, 0, None, Some(-3)),
            segment(seed, 1, 1, Some(-1), Some(0)),
        ]),
        None,
    );
    add_edge(
        2,
        5,
        5,
        "Z",
        Some(vec![segment(seed, 2, 0, None, Some(0))]),
        Some(0),
    );
    add_edge(3, 0, 6, "A", None, None);
    add_edge(
        4,
        6,
        1,
        "R0",
        Some(vec![
            segment(seed, 4, 0, Some(-5), Some(0)),
            segment(seed, 4, 1, Some(0), Some(5)),
        ]),
        Some(u64::MAX),
    );
    add_edge(
        5,
        4,
        4,
        "a",
        Some(vec![
            segment(seed, 5, 0, None, Some(-5)),
            segment(seed, 5, 1, Some(-2), Some(1)),
            segment(seed, 5, 2, Some(4), None),
        ]),
        None,
    );
    add_edge(
        6,
        6,
        2,
        "R1",
        Some(vec![
            segment(seed, 6, 0, None, Some(i64::MIN + 1)),
            segment(seed, 6, 1, Some(i64::MIN + 1), Some(i64::MAX - 1)),
            segment(seed, 6, 2, Some(i64::MAX - 1), None),
        ]),
        None,
    );
    add_edge(7, 3, 3, "R1", None, None);

    for index in 8..20 {
        let source = random.below(node_ids.len() - 1);
        let target = random.below(node_ids.len() - 1);
        let relationship = relationships[random.below(relationships.len())];
        let temporal = if index % 3 == 0 {
            let start = [-9, -3, 0, 4][random.below(4)];
            let width = [1, 2, 5][random.below(3)];
            let end = start + width;
            let series = if index % 2 == 0 {
                vec![
                    segment(seed, index, 0, Some(start), Some(end)),
                    segment(seed, index, 1, Some(end), Some(end + 3)),
                ]
            } else {
                vec![
                    segment(seed, index, 0, None, Some(start)),
                    segment(seed, index, 1, Some(end), None),
                ]
            };
            Some(series)
        } else {
            None
        };
        let expires_at_ms = match random.below(5) {
            0 => Some(0),
            1 => Some(5),
            2 => Some(7),
            3 => Some(u64::MAX),
            _ => None,
        };
        add_edge(index, source, target, relationship, temporal, expires_at_ms);
    }
    drop(add_edge);

    for (edge_id, expires_at_ms) in edge_deadlines {
        if let Some(expires_at_ms) = expires_at_ms {
            entries.push(GraphEntry::EdgeExpiry {
                edge_id,
                expires_at_ms: Some(expires_at_ms),
            });
        }
    }

    let mut state = GraphState::new();
    state.set_temporal(true).unwrap();
    state
        .apply(
            1,
            &GraphBody {
                graph_id: GraphId(0),
                entries,
            },
        )
        .expect("seeded graph is valid");
    state
}

fn decoded_engine_created(state: &GraphState) -> CompactGraph {
    let source = GraphSource::from_graph(state.graph(GraphId(0)).unwrap()).unwrap();
    decode(&encode(&source).unwrap()).unwrap()
}

fn clocks(graph: &CompactGraph) -> Vec<u64> {
    let mut clocks = BTreeSet::from([0, 1, 5, 6, 7, 8, u64::MAX - 1, u64::MAX]);
    for deadline in graph
        .source
        .nodes
        .iter()
        .filter_map(|node| node.deadline)
        .chain(graph.source.edges.iter().filter_map(|edge| edge.deadline))
    {
        clocks.insert(deadline);
        clocks.insert(deadline.saturating_sub(1));
        if let Some(after) = deadline.checked_add(1) {
            clocks.insert(after);
        }
    }
    clocks.into_iter().collect()
}

fn temporal_times(graph: &CompactGraph) -> Vec<i64> {
    let mut times = BTreeSet::from([
        i64::MIN,
        i64::MIN + 1,
        -10,
        -9,
        -5,
        -3,
        -2,
        -1,
        0,
        1,
        4,
        5,
        i64::MAX - 1,
        i64::MAX,
    ]);
    for bound in graph
        .source
        .edges
        .iter()
        .filter_map(|edge| edge.segments.as_ref())
        .flatten()
        .flat_map(|segment| [segment.valid_from, segment.valid_to])
        .flatten()
    {
        times.insert(bound);
        times.extend(bound.checked_sub(1));
        times.extend(bound.checked_add(1));
    }
    times.into_iter().collect()
}

fn source_node_view(node: &SourceNode) -> NodeView {
    NodeView {
        id: NodeId(node.id),
        key: node.key.clone().expect("engine-created nodes have keys"),
        labels: node.labels.iter().cloned().collect(),
        properties: node.properties.clone(),
        revision: node.revision,
        expires_at_ms: node.deadline,
    }
}

fn source_edge_view(graph: &crate::property_graph::engine::Graph, read: ReadEdge<'_>) -> EdgeView {
    let edge = read.edge;
    EdgeView {
        id: EdgeId(edge.id),
        key: edge.key.clone(),
        relationship: edge.relationship.clone(),
        source: NodeId(edge.source),
        source_key: graph.node(NodeId(edge.source)).unwrap().key.clone(),
        target: NodeId(edge.target),
        target_key: graph.node(NodeId(edge.target)).unwrap().key.clone(),
        properties: read.properties.clone(),
        revision: edge.revision,
        expires_at_ms: edge.deadline,
    }
}

fn engine_node(state: &GraphState, selector: NodeSelector, now_ms: u64) -> Option<NodeView> {
    let clock = || 0;
    let mut context = ExecutionContext::new(1, &clock);
    context.now_ms = now_ms;
    let query = GraphQuery::new(QueryOp::GetNode { node: selector });
    match execute(
        state.graph(GraphId(0)).unwrap(),
        &query,
        &GraphLimits::default(),
        &context,
    )
    .unwrap()
    .output
    {
        QueryOutput::Node(node) => node,
        other => panic!("expected node result, got {other:?}"),
    }
}

fn engine_edge(
    state: &GraphState,
    selector: EdgeSelector,
    now_ms: u64,
    as_of: Option<i64>,
) -> Option<EdgeView> {
    let clock = || 0;
    let mut context = ExecutionContext::new(1, &clock);
    context.now_ms = now_ms;
    let query = GraphQuery::new(QueryOp::GetEdge { edge: selector });
    let graph = state.graph(GraphId(0)).unwrap();
    let result = if let Some(as_of) = as_of {
        execute_as_of(graph, &query, &GraphLimits::default(), &context, as_of)
    } else {
        execute(graph, &query, &GraphLimits::default(), &context)
    }
    .unwrap();
    match result.output {
        QueryOutput::Edge(edge) => edge,
        other => panic!("expected edge result, got {other:?}"),
    }
}

fn engine_neighbors(
    state: &GraphState,
    node: u64,
    direction: Direction,
    types: Option<BTreeSet<String>>,
    now_ms: u64,
    as_of: Option<i64>,
) -> (Vec<EdgeView>, Vec<NodeView>) {
    let clock = || 0;
    let mut context = ExecutionContext::new(1, &clock);
    context.now_ms = now_ms;
    let query = GraphQuery::new(QueryOp::Neighbors {
        node: NodeSelector::Id(NodeId(node)),
        direction,
        types,
        edge_conditions: Vec::new(),
        limit: None,
        cursor: None,
    });
    let graph = state.graph(GraphId(0)).unwrap();
    let result = if let Some(as_of) = as_of {
        execute_as_of(graph, &query, &GraphLimits::default(), &context, as_of)
    } else {
        execute(graph, &query, &GraphLimits::default(), &context)
    };
    let result = match result {
        Ok(result) => result,
        Err(GraphError::NodeNotFound(id)) => {
            assert_eq!(id, NodeId(node));
            assert!(graph.node(id).is_none() || graph.node_expired(id, now_ms));
            return (Vec::new(), Vec::new());
        }
        Err(error) => panic!("engine neighbors failed: {error:?}"),
    };
    match result.output {
        QueryOutput::Neighbors { edges, nodes } => (edges, nodes),
        other => panic!("expected neighbors result, got {other:?}"),
    }
}

fn assert_node_parity(reader: &CsrReader<'_>, state: &GraphState, id: u64, now_ms: u64) {
    let expected = reader.node(id, now_ms).map(source_node_view);
    let actual = engine_node(state, NodeSelector::Id(NodeId(id)), now_ms);
    assert_eq!(actual, expected, "node ID {id} at clock {now_ms}");

    let key = reader
        .graph
        .source
        .nodes
        .iter()
        .find(|node| node.id == id)
        .and_then(|node| node.key.as_deref());
    if let Some(key) = key {
        let expected = reader.node_by_key(key, now_ms).map(source_node_view);
        let actual = engine_node(state, NodeSelector::Key(key.to_owned()), now_ms);
        assert_eq!(actual, expected, "node key {key} at clock {now_ms}");
    }
}

fn assert_edge_parity(
    reader: &CsrReader<'_>,
    state: &GraphState,
    id: u64,
    now_ms: u64,
    as_of: Option<i64>,
) {
    let graph = state.graph(GraphId(0)).unwrap();
    let expected = reader
        .edge(id, now_ms, as_of)
        .map(|read| source_edge_view(graph, read));
    let actual = engine_edge(state, EdgeSelector::Id(EdgeId(id)), now_ms, as_of);
    assert_eq!(
        actual, expected,
        "edge ID {id} at clock {now_ms}, as_of {as_of:?}"
    );

    if let Some(key) = reader
        .graph
        .source
        .edges
        .iter()
        .find(|edge| edge.id == id)
        .and_then(|edge| edge.key.as_deref())
    {
        let expected = reader
            .edge_by_key(key, now_ms, as_of)
            .map(|read| source_edge_view(graph, read));
        let actual = engine_edge(state, EdgeSelector::Key(key.to_owned()), now_ms, as_of);
        assert_eq!(
            actual, expected,
            "edge key {key} at clock {now_ms}, as_of {as_of:?}"
        );
    }
}

fn assert_neighbors_parity(
    reader: &CsrReader<'_>,
    state: &GraphState,
    node: u64,
    direction: Direction,
    types: Option<BTreeSet<String>>,
    now_ms: u64,
    as_of: Option<i64>,
) {
    let graph = state.graph(GraphId(0)).unwrap();
    let direct = reader.neighbors(node, direction, types.as_ref(), now_ms, as_of);
    let expected_edges: Vec<_> = direct
        .iter()
        .copied()
        .map(|edge| source_edge_view(graph, edge))
        .collect();
    let mut seen = HashSet::new();
    let expected_nodes: Vec<_> = direct
        .iter()
        .filter_map(|read| {
            let edge = read.edge;
            let other = match direction {
                Direction::Out => edge.target,
                Direction::In => edge.source,
                Direction::Both if edge.source == node => edge.target,
                Direction::Both => edge.source,
            };
            (seen.insert(other))
                .then(|| graph.node(NodeId(other)))
                .flatten()
                .map(|node| NodeView {
                    id: node.id,
                    key: node.key.clone(),
                    labels: node.labels.clone(),
                    properties: node.properties.clone(),
                    revision: node.revision,
                    expires_at_ms: graph.node_expiry(node.id),
                })
        })
        .collect();
    let actual = engine_neighbors(state, node, direction, types, now_ms, as_of);
    assert_eq!(
        actual.0, expected_edges,
        "adjacency order for node {node}, {direction:?}, clock {now_ms}, as_of {as_of:?}"
    );
    assert_eq!(
        actual.1, expected_nodes,
        "other-node order for node {node}, {direction:?}, clock {now_ms}, as_of {as_of:?}"
    );
}

#[test]
fn seeded_randomized_node_and_edge_reads_match_engine_retention() {
    for seed in SEEDS {
        let state = make_state(seed);
        let decoded = decoded_engine_created(&state);
        let reader = CsrReader::new(&decoded);
        for now_ms in clocks(&decoded) {
            for node in &decoded.source.nodes {
                assert_node_parity(&reader, &state, node.id, now_ms);
            }
            assert_node_parity(&reader, &state, u64::MAX, now_ms);
            assert_eq!(reader.node_by_key("unknown-node", now_ms), None);
            assert_eq!(
                engine_node(&state, NodeSelector::Key("unknown-node".into()), now_ms),
                None,
                "unknown node key at clock {now_ms}, seed {seed:016x}"
            );
            for edge in &decoded.source.edges {
                assert_edge_parity(&reader, &state, edge.id, now_ms, None);
            }
            assert_edge_parity(&reader, &state, u64::MAX, now_ms, None);
            assert_eq!(reader.edge_by_key("unknown-edge", now_ms, None), None);
            assert_eq!(
                engine_edge(
                    &state,
                    EdgeSelector::Key("unknown-edge".into()),
                    now_ms,
                    None
                ),
                None,
                "unknown edge key at clock {now_ms}, seed {seed:016x}"
            );
        }
    }
}

#[test]
fn seeded_randomized_neighbors_match_engine_direction_filters_and_order() {
    let type_filters = [
        None,
        Some(BTreeSet::from(["A".to_owned(), "R1".to_owned()])),
        Some(BTreeSet::from(["missing".to_owned()])),
    ];
    let directions = [Direction::Out, Direction::In, Direction::Both];
    for seed in SEEDS {
        let state = make_state(seed);
        let decoded = decoded_engine_created(&state);
        let reader = CsrReader::new(&decoded);
        let now_values = clocks(&decoded);
        let selected_clocks = [
            now_values[0],
            now_values[2],
            now_values[now_values.len() - 3],
            now_values[now_values.len() - 1],
        ];
        let nodes = &decoded.source.nodes;
        for now_ms in selected_clocks {
            for node in nodes {
                for direction in directions {
                    for types in &type_filters {
                        assert_neighbors_parity(
                            &reader,
                            &state,
                            node.id,
                            direction,
                            (*types).clone(),
                            now_ms,
                            None,
                        );
                    }
                }
            }
            for direction in directions {
                assert_neighbors_parity(&reader, &state, u64::MAX, direction, None, now_ms, None);
            }
        }
    }
}

#[test]
fn seeded_randomized_temporal_reads_match_engine_boundaries_and_retention() {
    let type_filters = [
        None,
        Some(BTreeSet::from(["A".to_owned(), "R1".to_owned()])),
        Some(BTreeSet::from(["missing".to_owned()])),
    ];
    let directions = [Direction::Out, Direction::In, Direction::Both];
    for seed in SEEDS {
        let state = make_state(seed);
        let decoded = decoded_engine_created(&state);
        let reader = CsrReader::new(&decoded);
        let times = temporal_times(&decoded);
        for now_ms in clocks(&decoded) {
            for time in &times {
                for edge in &decoded.source.edges {
                    assert_edge_parity(&reader, &state, edge.id, now_ms, Some(*time));
                }
                assert_edge_parity(&reader, &state, u64::MAX, now_ms, Some(*time));
                for node_index in [0, 1, 2, 3, 4, 5, 6, 7] {
                    let node = decoded.source.nodes[node_index].id;
                    for direction in directions {
                        for types in &type_filters {
                            assert_neighbors_parity(
                                &reader,
                                &state,
                                node,
                                direction,
                                (*types).clone(),
                                now_ms,
                                Some(*time),
                            );
                        }
                    }
                }
            }
        }
    }
}
