use super::super::*;
use super::*;

use crate::property_graph::engine::{
    EdgeSelector, EdgeView, ExecutionContext, GraphLimits, GraphQuery, GraphState, NodeSelector,
    QueryOp, QueryOutput, execute,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

fn decoded_graph(state: &GraphState) -> (Graph, CompactGraph) {
    let graph = state.graph(GraphId(0)).unwrap();
    let source = GraphSource::from_graph(graph).unwrap();
    let decoded = decode(&encode(&source).unwrap()).unwrap();
    (graph.clone(), decoded)
}

fn property_map<const N: usize>(entries: [(&str, PropertyValue); N]) -> Properties {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

fn execute_read(
    graph: &Graph,
    query: &GraphQuery,
    clock_ms: u64,
    as_of: Option<i64>,
) -> Result<QueryOutput, crate::property_graph::engine::GraphError> {
    let monotonic_clock = || 0;
    let mut context = ExecutionContext::new(graph.graph_sequence(), &monotonic_clock);
    context.now_ms = clock_ms;
    let result = match as_of {
        Some(as_of) => crate::property_graph::engine::execute_as_of(
            graph,
            query,
            &GraphLimits::default(),
            &context,
            as_of,
        ),
        None => execute(graph, query, &GraphLimits::default(), &context),
    }?;
    Ok(result.output)
}

fn engine_node(
    graph: &Graph,
    selector: NodeSelector,
    clock_ms: u64,
    as_of: Option<i64>,
) -> Option<crate::property_graph::engine::NodeView> {
    let query = GraphQuery::new(QueryOp::GetNode { node: selector });
    match execute_read(graph, &query, clock_ms, as_of).unwrap() {
        QueryOutput::Node(node) => node,
        output => panic!("get-node returned {output:?}"),
    }
}

fn engine_edge(
    graph: &Graph,
    selector: EdgeSelector,
    clock_ms: u64,
    as_of: Option<i64>,
) -> Option<EdgeView> {
    let query = GraphQuery::new(QueryOp::GetEdge { edge: selector });
    match execute_read(graph, &query, clock_ms, as_of).unwrap() {
        QueryOutput::Edge(edge) => edge,
        output => panic!("get-edge returned {output:?}"),
    }
}

fn engine_neighbors(
    graph: &Graph,
    node: u64,
    direction: Direction,
    types: Option<BTreeSet<String>>,
    clock_ms: u64,
    as_of: Option<i64>,
) -> Vec<EdgeView> {
    let query = GraphQuery::new(QueryOp::Neighbors {
        node: NodeSelector::Id(NodeId(node)),
        direction,
        types,
        edge_conditions: Vec::new(),
        limit: None,
        cursor: None,
    });
    match execute_read(graph, &query, clock_ms, as_of) {
        Ok(QueryOutput::Neighbors { edges, .. }) => edges,
        Err(crate::property_graph::engine::GraphError::NodeNotFound(_)) => Vec::new(),
        Ok(output) => panic!("neighbors returned {output:?}"),
        Err(error) => panic!("neighbors query failed: {error}"),
    }
}

fn assert_node_matches_view(
    actual: Option<&SourceNode>,
    expected: Option<&crate::property_graph::engine::NodeView>,
    graph: &Graph,
) {
    match (actual, expected) {
        (None, None) => {}
        (Some(actual), Some(expected)) => {
            assert_eq!(actual.id, expected.id.0);
            assert_eq!(actual.key.as_deref(), Some(expected.key.as_str()));
            assert_eq!(
                actual.labels.iter().cloned().collect::<BTreeSet<_>>(),
                expected.labels
            );
            assert_eq!(actual.properties, expected.properties);
            assert_eq!(actual.revision, expected.revision);
            assert_eq!(actual.deadline, expected.expires_at_ms);
            assert_eq!(
                actual.attachments,
                graph
                    .attachments(NodeId(actual.id))
                    .collect::<BTreeMap<_, _>>()
            );
        }
        (actual, expected) => panic!("node mismatch: {actual:?} != {expected:?}"),
    }
}

fn assert_read_edges_match_views(actual: &[ReadEdge<'_>], expected: &[EdgeView]) {
    assert_eq!(actual.len(), expected.len());
    for (actual, expected) in actual.iter().zip(expected) {
        assert_eq!(actual.edge.id, expected.id.0);
        assert_eq!(actual.edge.key, expected.key);
        assert_eq!(actual.edge.source, expected.source.0);
        assert_eq!(actual.edge.target, expected.target.0);
        assert_eq!(actual.edge.relationship, expected.relationship);
        assert_eq!(actual.edge.revision, expected.revision);
        assert_eq!(actual.edge.deadline, expected.expires_at_ms);
        assert_eq!(actual.properties, &expected.properties);
    }
}

fn assert_read_edge_matches_view(actual: Option<ReadEdge<'_>>, expected: Option<EdgeView>) {
    match (actual, expected) {
        (None, None) => {}
        (Some(actual), Some(expected)) => {
            assert_read_edges_match_views(&[actual], &[expected]);
        }
        (actual, expected) => panic!("edge mismatch: {actual:?} != {expected:?}"),
    }
}

fn ordinary_metadata_state() -> GraphState {
    let mut state = GraphState::new();
    state
        .apply(
            1,
            &GraphBody {
                graph_id: GraphId(0),
                entries: vec![
                    GraphEntry::CreateGraph {
                        name: "metadata".into(),
                    },
                    GraphEntry::PutNode {
                        node_id: NodeId(0),
                        key: "origin".into(),
                        labels: BTreeSet::from(["Person".into(), "Root".into()]),
                        properties: property_map([
                            ("active", PropertyValue::Boolean(true)),
                            ("rank", PropertyValue::Integer(3)),
                        ]),
                    },
                    GraphEntry::PutNode {
                        node_id: NodeId(1),
                        key: "first".into(),
                        labels: BTreeSet::from(["Person".into()]),
                        properties: property_map([("name", PropertyValue::String("first".into()))]),
                    },
                    GraphEntry::PutNode {
                        node_id: NodeId(2),
                        key: "second".into(),
                        labels: BTreeSet::from(["Place".into()]),
                        properties: Properties::new(),
                    },
                    GraphEntry::NodeExpiry {
                        node_id: NodeId(2),
                        expires_at_ms: Some(50),
                    },
                    GraphEntry::Attach {
                        node_id: NodeId(0),
                        collection: 2,
                        record_id: 17,
                    },
                    GraphEntry::Attach {
                        node_id: NodeId(0),
                        collection: u16::MAX,
                        record_id: u64::MAX,
                    },
                    GraphEntry::Attach {
                        node_id: NodeId(1),
                        collection: 2,
                        record_id: 18,
                    },
                ],
            },
        )
        .unwrap();
    state
        .apply(
            2,
            &GraphBody {
                graph_id: GraphId(0),
                entries: vec![
                    GraphEntry::PutEdge {
                        edge_id: EdgeId(0),
                        source: NodeId(0),
                        destination: NodeId(0),
                        relationship: "alpha".into(),
                        key: Some("loop".into()),
                        properties: property_map([("kind", PropertyValue::String("loop".into()))]),
                    },
                    GraphEntry::PutEdge {
                        edge_id: EdgeId(7),
                        source: NodeId(0),
                        destination: NodeId(1),
                        relationship: "alpha".into(),
                        key: Some("forward-one".into()),
                        properties: property_map([("weight", PropertyValue::Integer(7))]),
                    },
                    GraphEntry::PutEdge {
                        edge_id: EdgeId(8),
                        source: NodeId(1),
                        destination: NodeId(0),
                        relationship: "alpha".into(),
                        key: Some("reverse".into()),
                        properties: property_map([("weight", PropertyValue::Integer(8))]),
                    },
                    GraphEntry::PutEdge {
                        edge_id: EdgeId(9),
                        source: NodeId(0),
                        destination: NodeId(1),
                        relationship: "beta".into(),
                        key: Some("forward-two".into()),
                        properties: property_map([("weight", PropertyValue::Integer(9))]),
                    },
                    GraphEntry::PutEdge {
                        edge_id: EdgeId(10),
                        source: NodeId(2),
                        destination: NodeId(0),
                        relationship: "gamma".into(),
                        key: Some("expiring-endpoint".into()),
                        properties: property_map([("weight", PropertyValue::Integer(10))]),
                    },
                    GraphEntry::EdgeExpiry {
                        edge_id: EdgeId(10),
                        expires_at_ms: Some(40),
                    },
                ],
            },
        )
        .unwrap();
    state
}

fn neighbor_case_query(query: &Value) -> (Direction, Option<BTreeSet<String>>) {
    let direction = match query["direction"].as_str().unwrap() {
        "out" => Direction::Out,
        "in" => Direction::In,
        "both" => Direction::Both,
        other => panic!("unknown read-case direction: {other}"),
    };
    let types = query["relationship"].as_array().map(|values| {
        values
            .iter()
            .map(|value| value.as_str().unwrap().to_owned())
            .collect()
    });
    (direction, types)
}

fn read_case_graph(case: &Value) -> (GraphState, CompactGraph, bool) {
    let mut representable = case.clone();
    let mut remapped = false;
    let mut remapped_node = false;
    for node in representable["input"]["nodes"].as_array_mut().unwrap() {
        if node["id"].as_u64() == Some(u64::MAX) {
            node["id"] = Value::from(u64::MAX - 1);
            remapped_node = true;
        }
    }
    for edge in representable["input"]["edges"].as_array_mut().unwrap() {
        if edge["id"].as_u64() == Some(u64::MAX) {
            edge["id"] = Value::from(u64::MAX - 1);
            remapped = true;
        }
        for endpoint in ["source", "target"] {
            if edge[endpoint].as_u64() == Some(u64::MAX) {
                edge[endpoint] = Value::from(u64::MAX - 1);
            }
        }
    }
    representable["input"]["nodes"]
        .as_array_mut()
        .unwrap()
        .sort_by_key(|node| node["id"].as_u64().unwrap());
    representable["input"]["edges"]
        .as_array_mut()
        .unwrap()
        .sort_by_key(|edge| edge["id"].as_u64().unwrap());
    let state = super::super::tests::read_case_state(&representable);
    let source = GraphSource::from_graph(state.graph(GraphId(0)).unwrap()).unwrap();
    let mut compact = if remapped || remapped_node {
        build_inner(source, false).unwrap()
    } else {
        decode(&encode(&source).unwrap()).unwrap()
    };
    if remapped {
        // The engine needs a representable next ID; logical CSR does not.
        let edge = compact
            .source
            .edges
            .iter_mut()
            .find(|edge| edge.id == u64::MAX - 1)
            .unwrap();
        edge.id = u64::MAX;
        compact
            .edge_rows
            .iter_mut()
            .find(|edge| edge.id == u64::MAX - 1)
            .unwrap()
            .id = u64::MAX;
    }
    if remapped_node {
        let node = compact
            .source
            .nodes
            .iter_mut()
            .find(|node| node.id == u64::MAX - 1)
            .unwrap();
        node.id = u64::MAX;
        node.key = Some(format!("n{}", u64::MAX));
        for edge in &mut compact.source.edges {
            if edge.source == u64::MAX - 1 {
                edge.source = u64::MAX;
            }
            if edge.target == u64::MAX - 1 {
                edge.target = u64::MAX;
            }
        }
        let ordinal = compact.node_ordinals.remove(&(u64::MAX - 1)).unwrap();
        compact.node_ordinals.insert(u64::MAX, ordinal);
    }
    (state, compact, remapped || remapped_node)
}

#[test]
fn retained_temporal_interval_goldens_match_original_engine_reads() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/temporal_intervals/golden.json"
    ))
    .unwrap();
    let mut retained_cases = 0;
    for case in fixture["series"].as_array().unwrap() {
        if !case["edge"]["retained"].as_bool().unwrap()
            || !case["source"]["retained"].as_bool().unwrap()
            || !case["target"]["retained"].as_bool().unwrap()
        {
            continue;
        }
        retained_cases += 1;
        let state = super::super::tests::golden_state(case);
        let (graph, decoded) = decoded_graph(&state);
        let reader = CsrReader::new(&decoded);
        for query in case["queries"].as_array().unwrap() {
            let time = query["T"].as_i64().unwrap();
            let clock = query["C"].as_u64().unwrap();
            let expected = query["expected"].is_null();
            let direct = reader.edge(1, clock, Some(time));
            let engine = engine_edge(&graph, EdgeSelector::Id(EdgeId(1)), clock, Some(time));
            assert_eq!(direct.is_none(), expected, "{}", case["name"]);
            assert_eq!(engine.is_none(), expected, "{}", case["name"]);
            assert_read_edge_matches_view(direct, engine);

            let direct_neighbors = reader.neighbors(0, Direction::Out, None, clock, Some(time));
            let engine_neighbors =
                engine_neighbors(&graph, 0, Direction::Out, None, clock, Some(time));
            assert_read_edges_match_views(&direct_neighbors, &engine_neighbors);
            assert_eq!(direct_neighbors.is_empty(), expected, "{}", case["name"]);
        }
        let direct_ordinary = reader.edge(1, 0, None);
        let engine_ordinary = engine_edge(&graph, EdgeSelector::Key("edge".into()), 0, None);
        assert_read_edge_matches_view(direct_ordinary, engine_ordinary);
    }
    assert!(
        retained_cases >= 8,
        "expected all retained interval goldens"
    );
}

#[test]
fn temporal_upper_bound_is_exclusive() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/temporal_intervals/golden.json"
    ))
    .unwrap();
    let case = fixture["series"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == "finite-negative-touching-gap-and-own-properties")
        .unwrap();
    let state = super::super::tests::golden_state(case);
    let (graph, decoded) = decoded_graph(&state);
    let reader = CsrReader::new(&decoded);
    assert!(reader.edge(1, 0, Some(0)).is_none());
    assert!(engine_edge(&graph, EdgeSelector::Id(EdgeId(1)), 0, Some(0)).is_none());
    assert!(
        reader
            .neighbors(0, Direction::Out, None, 0, Some(0))
            .is_empty()
    );
    assert!(engine_neighbors(&graph, 0, Direction::Out, None, 0, Some(0)).is_empty());
}

#[test]
fn full_temporal_series_reads_match_original_engine_across_segments_and_ttl() {
    let state = super::super::tests::full_temporal_state();
    let (graph, decoded) = decoded_graph(&state);
    let reader = CsrReader::new(&decoded);

    let maximum_series = decoded.source.edges[1].segments.as_ref().unwrap();
    assert_eq!(maximum_series.len(), MAX_SEGMENTS);
    let mut boundary_times = BTreeSet::from([i64::MIN, i64::MAX]);
    for segment in maximum_series {
        boundary_times.extend(segment.valid_from);
        boundary_times.extend(segment.valid_to);
    }
    for time in boundary_times {
        let direct = reader.edge(1, 60, Some(time));
        let engine = engine_edge(&graph, EdgeSelector::Id(EdgeId(1)), 60, Some(time));
        assert_read_edge_matches_view(direct, engine);
        let direct_neighbors = reader.neighbors(0, Direction::Out, None, 60, Some(time));
        let engine_neighbors = engine_neighbors(&graph, 0, Direction::Out, None, 60, Some(time));
        assert_read_edges_match_views(&direct_neighbors, &engine_neighbors);
    }

    let extreme_times = [i64::MIN, i64::MIN + 1, i64::MAX - 1, i64::MAX];
    for clock in [60, 70, 80, 90, 100] {
        for time in extreme_times {
            for id in [0, 1, 2] {
                let direct = reader.edge(id, clock, Some(time));
                let engine = engine_edge(&graph, EdgeSelector::Id(EdgeId(id)), clock, Some(time));
                assert_read_edge_matches_view(direct, engine);
            }
            let direct = reader.neighbors(0, Direction::Out, None, clock, Some(time));
            let engine = engine_neighbors(&graph, 0, Direction::Out, None, clock, Some(time));
            assert_read_edges_match_views(&direct, &engine);
        }
    }

    for id in [0, 1, 2] {
        let direct = reader.edge(id, 60, None);
        let engine = engine_edge(&graph, EdgeSelector::Id(EdgeId(id)), 60, None);
        assert_read_edge_matches_view(direct, engine);
    }
    let legacy = reader.edge(0, 60, None).unwrap();
    assert_eq!(legacy.properties, &decoded.source.edges[0].properties);
    assert!(reader.node(0, 100).is_none());
    assert!(reader.node_by_key("source", 100).is_none());
    assert!(reader.node(u64::MAX, 0).is_none());
}

#[test]
fn removed_empty_temporal_series_match_original_engine() {
    let state = super::super::tests::temporal_state_with_removed_series();
    let (graph, decoded) = decoded_graph(&state);
    let reader = CsrReader::new(&decoded);
    let empty = decoded
        .source
        .edges
        .iter()
        .find(|edge| edge.id == 2)
        .unwrap();
    assert_eq!(empty.segments, Some(Vec::new()));

    for (clock, time) in [(0, 5), (0, 25), (100, 25), (100, i64::MAX)] {
        let direct = reader.edge(2, clock, Some(time));
        let engine = engine_edge(
            &graph,
            EdgeSelector::Key("emptied".into()),
            clock,
            Some(time),
        );
        assert_read_edge_matches_view(direct, engine);
        assert!(direct.is_none());
    }

    for (clock, time) in [(0, 5), (0, 15), (0, 25), (100, 25)] {
        for id in [0, 1, 2] {
            assert_read_edge_matches_view(
                reader.edge(id, clock, Some(time)),
                engine_edge(&graph, EdgeSelector::Id(EdgeId(id)), clock, Some(time)),
            );
        }
        assert_read_edges_match_views(
            &reader.neighbors(0, Direction::Out, None, clock, Some(time)),
            &engine_neighbors(&graph, 0, Direction::Out, None, clock, Some(time)),
        );
    }
    assert_read_edge_matches_view(
        reader.edge_by_key("emptied", 0, None),
        engine_edge(&graph, EdgeSelector::Key("emptied".into()), 0, None),
    );
}

#[test]
fn read_cases_neighbors_match_original_engine_and_fixture_order() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/temporal_intervals/read-cases.json"
    ))
    .unwrap();
    let mut covered = 0;
    for case in fixture["cases"].as_array().unwrap() {
        let (state, compact, has_unrepresentable_edge_id) = read_case_graph(case);
        let graph = state.graph(GraphId(0)).unwrap();
        let reader = CsrReader::new(&compact);
        for (index, query) in case["input"]["queries"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            if query["op"] != "neighbors" || query.get("after").and_then(Value::as_object).is_some()
            {
                continue;
            }
            let node = query["node"].as_u64().unwrap();
            let time = query["T"].as_i64().unwrap();
            let clock = query["C"].as_u64().unwrap();
            let (direction, types) = neighbor_case_query(query);
            let actual = reader.neighbors(node, direction, types.as_ref(), clock, Some(time));
            let expected_ids = case["expected"][index]["edges"]
                .as_array()
                .unwrap()
                .iter()
                .map(|id| id.as_u64().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(
                actual.iter().map(|edge| edge.edge.id).collect::<Vec<_>>(),
                expected_ids,
                "{} query {index}",
                case["name"]
            );
            if !has_unrepresentable_edge_id {
                let expected =
                    engine_neighbors(graph, node, direction, types.clone(), clock, Some(time));
                assert_read_edges_match_views(&actual, &expected);
                let ordinary_actual =
                    reader.neighbors(node, direction, types.as_ref(), clock, None);
                let ordinary_expected =
                    engine_neighbors(graph, node, direction, types, clock, None);
                assert_read_edges_match_views(&ordinary_actual, &ordinary_expected);
            }
            covered += 1;
        }
    }
    assert!(covered >= 15, "expected retained neighbor read cases");
}

#[test]
fn both_deduplicates_self_loops_preserves_parallel_edges() {
    let state = ordinary_metadata_state();
    let (graph, decoded) = decoded_graph(&state);
    let reader = CsrReader::new(&decoded);
    for (direction, expected_ids) in [
        (Direction::Both, vec![0, 7, 8, 9, 10]),
        (Direction::Out, vec![0, 7, 9]),
        (Direction::In, vec![0, 8, 10]),
    ] {
        let direct = reader.neighbors(0, direction, None, 0, None);
        let expected = engine_neighbors(&graph, 0, direction, None, 0, None);
        assert_read_edges_match_views(&direct, &expected);
        assert_eq!(
            direct.iter().map(|edge| edge.edge.id).collect::<Vec<_>>(),
            expected_ids
        );
    }
    let only_alpha = BTreeSet::from(["alpha".to_owned()]);
    let filtered = reader.neighbors(0, Direction::Both, Some(&only_alpha), 0, None);
    let filtered_engine = engine_neighbors(
        &graph,
        0,
        Direction::Both,
        Some(only_alpha.clone()),
        0,
        None,
    );
    assert_read_edges_match_views(&filtered, &filtered_engine);
    assert_eq!(
        filtered.iter().map(|edge| edge.edge.id).collect::<Vec<_>>(),
        [0, 7, 8]
    );
}

#[test]
fn public_id_and_key_reads_preserve_metadata_labels_and_attachments() {
    let state = ordinary_metadata_state();
    let (graph, decoded) = decoded_graph(&state);
    let reader = CsrReader::new(&decoded);

    for (id, key) in [(0, "origin"), (1, "first"), (2, "second")] {
        for node in [reader.node(id, 0), reader.node_by_key(key, 0)] {
            let expected = engine_node(&graph, NodeSelector::Id(NodeId(id)), 0, None);
            assert_node_matches_view(node, expected.as_ref(), &graph);
        }
        let by_key = engine_node(&graph, NodeSelector::Key(key.into()), 0, None);
        assert_node_matches_view(reader.node_by_key(key, 0), by_key.as_ref(), &graph);
    }
    assert_node_matches_view(
        reader.node(u64::MAX, 0),
        engine_node(&graph, NodeSelector::Id(NodeId(u64::MAX)), 0, None).as_ref(),
        &graph,
    );
    assert!(reader.node_by_key("missing", 0).is_none());
    assert_node_matches_view(
        reader.node(2, 50),
        engine_node(&graph, NodeSelector::Id(NodeId(2)), 50, None).as_ref(),
        &graph,
    );

    for (id, key) in [
        (0, "loop"),
        (7, "forward-one"),
        (8, "reverse"),
        (9, "forward-two"),
        (10, "expiring-endpoint"),
    ] {
        for (direct, engine) in [
            (
                reader.edge(id, 0, None),
                engine_edge(&graph, EdgeSelector::Id(EdgeId(id)), 0, None),
            ),
            (
                reader.edge_by_key(key, 0, None),
                engine_edge(&graph, EdgeSelector::Key(key.into()), 0, None),
            ),
        ] {
            assert_read_edge_matches_view(direct, engine);
        }
    }
    assert_read_edge_matches_view(
        reader.edge(u64::MAX, 0, None),
        engine_edge(&graph, EdgeSelector::Id(EdgeId(u64::MAX)), 0, None),
    );
    assert_read_edge_matches_view(
        reader.edge_by_key("missing", 0, None),
        engine_edge(&graph, EdgeSelector::Key("missing".into()), 0, None),
    );
    assert_read_edge_matches_view(
        reader.edge(10, 40, None),
        engine_edge(&graph, EdgeSelector::Id(EdgeId(10)), 40, None),
    );
}

#[test]
fn parallel_loops_and_both_merge_fixture_keeps_exact_retained_order() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/temporal_intervals/read-cases.json"
    ))
    .unwrap();
    let case = fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == "parallel-loops-and-both-merge")
        .unwrap();
    let (_, compact, remapped) = read_case_graph(case);
    assert!(remapped);
    assert!(compact.source.edges.iter().any(|edge| edge.id == u64::MAX));
    let reader = CsrReader::new(&compact);

    for (direction, expected) in [
        (Direction::Both, vec![0, 7, 8, 9, 10, u64::MAX]),
        (Direction::Out, vec![0, 7, 9, u64::MAX]),
        (Direction::In, vec![0, 8, 10]),
    ] {
        let actual = reader.neighbors(0, direction, None, 0, Some(5));
        assert_eq!(
            actual.iter().map(|edge| edge.edge.id).collect::<Vec<_>>(),
            expected
        );
    }
}
