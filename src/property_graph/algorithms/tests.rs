//! Adapter tests for bounded local algorithms (Gap 5 S1 and S2).
//!
//! Golden cases come from `tests/fixtures/graph_algorithms/golden.json`,
//! checked independently by `tests/graph_algorithm_oracle.py`. Every one of
//! the 21 cases checks the S1 projection (its expected scope node and edge
//! IDs) and the degrees derived from that expected scope; the 8
//! `weighted_shortest_path` cases also check the S2 path outcome. PageRank,
//! label propagation and betweenness outputs belong to later slices and are
//! not claimed. The remaining tests cover the public request and result
//! mapping, typed errors, the TTL clock, budgets, cancellation, the
//! deadline, output caps and non-mutation through a real database file.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::AtomicBool;

use serde_json::Value;

use super::super::EdgeRef;
use super::super::engine::{
    EdgeId, ExecutionContext, GraphBody, GraphEntry, GraphId, GraphState, NodeId, Properties,
    PropertyValue,
};
use super::*;
use crate::metadata::MetadataValue;
use crate::{Database, Expected, Metadata};

const GOLDEN: &str = include_str!("../../../tests/fixtures/graph_algorithms/golden.json");
const G: GraphId = GraphId(0);

fn cases() -> Vec<Value> {
    let golden: Value = serde_json::from_str(GOLDEN).unwrap();
    assert_eq!(golden["schema"], "vlite.graph-algorithm-golden");
    assert_eq!(golden["version"], 1);
    let cases = golden["cases"].as_array().unwrap().clone();
    assert_eq!(cases.len(), 21);
    cases
}

fn ids(value: &Value) -> Vec<u64> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_u64().unwrap())
        .collect()
}

/// A fixture list, or `None` for `"any"`.
fn names(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::String(any) => {
            assert_eq!(any, "any");
            None
        }
        list => Some(
            list.as_array()
                .unwrap()
                .iter()
                .map(|name| name.as_str().unwrap().to_owned())
                .collect(),
        ),
    }
}

fn direction(case: &Value) -> Direction {
    match case["parameters"]["scope"]["direction"].as_str().unwrap() {
        "out" => Direction::Out,
        "in" => Direction::In,
        "both" => Direction::Both,
        other => panic!("unknown direction {other}"),
    }
}

/// Nodes or edges of a case, ascending by fixture ID.
fn sorted(case: &Value, field: &str) -> Vec<Value> {
    let mut items = case[field].as_array().unwrap().clone();
    items.sort_by_key(|item| item["id"].as_u64().unwrap());
    items
}

fn weight(value: &Value) -> MetadataValue {
    match value.as_i64() {
        Some(integer) => MetadataValue::Integer(integer),
        None => MetadataValue::Float(value.as_f64().unwrap()),
    }
}

/// The case's request with nodes named by `node`, and whether it is a
/// weighted path case. Cases of later slices get `Degree`, which still
/// checks their scope.
fn fixture_request(
    case: &Value,
    node: &dyn Fn(u64) -> NodeSelector,
) -> (GraphAlgorithmRequest, bool) {
    let scope = &case["parameters"]["scope"];
    let scope = GraphAlgorithmScope {
        seeds: ids(&scope["seeds"]).into_iter().map(node).collect(),
        direction: direction(case),
        relationship_types: names(&scope["relationship_types"])
            .map_or(RelationshipScope::Any, RelationshipScope::Only),
        node_labels: names(&scope["node_labels"]).map_or(LabelScope::Any, LabelScope::All),
        edge_conditions: Vec::new(),
    };
    let parameters = &case["parameters"];
    let path = case["algorithm"] == "weighted_shortest_path";
    let algorithm = if path {
        GraphAlgorithm::WeightedShortestPath {
            source: node(parameters["source"].as_u64().unwrap()),
            destination: node(parameters["target"].as_u64().unwrap()),
            weight_property: match parameters["weight_policy"].as_str().unwrap() {
                "explicit" => Some("weight".into()),
                "unit" => None,
                other => panic!("unknown weight policy {other}"),
            },
        }
    } else {
        GraphAlgorithm::Degree
    };
    (GraphAlgorithmRequest { scope, algorithm }, path)
}

fn fixture_options(case: &Value) -> GraphAlgorithmOptions<'static> {
    GraphAlgorithmOptions {
        limits: GraphLimits {
            max_hops: case["parameters"]["scope"]["max_hops"].as_u64().unwrap(),
            ..GraphLimits::DEFAULT
        },
        ..GraphAlgorithmOptions::default()
    }
}

fn degree_request(request: &GraphAlgorithmRequest) -> GraphAlgorithmRequest {
    GraphAlgorithmRequest {
        scope: request.scope.clone(),
        algorithm: GraphAlgorithm::Degree,
    }
}

/// Degrees over the expected scope edges, by the documented rule: `In`
/// reverses endpoints and a self-loop counts once as incident.
fn fixture_degrees(case: &Value) -> Vec<(u64, u64, u64, u64)> {
    let expected = &case["expected"];
    let edges: BTreeSet<u64> = ids(&expected["scope_edge_ids"]).into_iter().collect();
    let mut rows: BTreeMap<u64, (u64, u64, u64)> = ids(&expected["scope_node_ids"])
        .into_iter()
        .map(|node| (node, (0, 0, 0)))
        .collect();
    for edge in case["edges"].as_array().unwrap() {
        if !edges.contains(&edge["id"].as_u64().unwrap()) {
            continue;
        }
        let (mut tail, mut head) = (
            edge["source"].as_u64().unwrap(),
            edge["target"].as_u64().unwrap(),
        );
        if direction(case) == Direction::In {
            (tail, head) = (head, tail);
        }
        rows.get_mut(&tail).unwrap().1 += 1;
        rows.get_mut(&head).unwrap().0 += 1;
        rows.get_mut(&tail).unwrap().2 += 1;
        if head != tail {
            rows.get_mut(&head).unwrap().2 += 1;
        }
    }
    rows.into_iter()
        .map(|(node, (in_degree, out_degree, incident))| (node, in_degree, out_degree, incident))
        .collect()
}

fn degree_rows(read: &GraphAlgorithmRead) -> Vec<(u64, u64, u64, u64)> {
    match &read.output {
        Some(GraphAlgorithmOutput::Degree(rows)) => rows
            .iter()
            .map(|row| {
                (
                    row.node_id,
                    row.in_degree,
                    row.out_degree,
                    row.incident_degree,
                )
            })
            .collect(),
        other => panic!("expected degree rows, got {other:?}"),
    }
}

/// The expected path outcome with node and edge IDs mapped by `node` and
/// `edge`.
fn fixture_path(
    case: &Value,
    node: &dyn Fn(u64) -> u64,
    edge: &dyn Fn(u64) -> u64,
) -> WeightedPathOutcome {
    let output = &case["expected"]["output"];
    match output["status"].as_str().unwrap() {
        "found" => {
            let path = &output["path"];
            let edge_ids: Vec<u64> = ids(&path["edge_ids"]).into_iter().map(edge).collect();
            assert_eq!(path["hops"].as_u64().unwrap(), edge_ids.len() as u64);
            WeightedPathOutcome::Found(WeightedPath {
                cost: path["cost"].as_f64().unwrap(),
                node_ids: ids(&path["node_ids"]).into_iter().map(node).collect(),
                edge_ids,
            })
        }
        "no_path_within_scope_and_hops" => WeightedPathOutcome::NoPathWithinScopeAndHops,
        other => panic!("unknown path status {other}"),
    }
}

fn path_outcome(read: &GraphAlgorithmRead) -> &WeightedPathOutcome {
    match &read.output {
        Some(GraphAlgorithmOutput::WeightedShortestPath(outcome)) => outcome,
        other => panic!("expected a path outcome, got {other:?}"),
    }
}

/// The case's graph with the exact fixture IDs.
fn exact_graph(case: &Value) -> GraphState {
    let mut state = GraphState::new();
    let create = GraphBody {
        graph_id: G,
        entries: vec![GraphEntry::CreateGraph { name: "g".into() }],
    };
    state.apply(1, &create).unwrap();
    let mut entries = Vec::new();
    for node in sorted(case, "nodes") {
        entries.push(GraphEntry::PutNode {
            node_id: NodeId(node["id"].as_u64().unwrap()),
            key: node["public_id"].as_str().unwrap().into(),
            labels: names(&node["labels"]).unwrap().into_iter().collect(),
            properties: Properties::new(),
        });
    }
    for edge in sorted(case, "edges") {
        let mut properties = Properties::new();
        let value = match weight(&edge["weight"]) {
            MetadataValue::Integer(integer) => PropertyValue::Integer(integer),
            MetadataValue::Float(float) => PropertyValue::Float(float),
            _ => unreachable!(),
        };
        properties.insert("weight".into(), value);
        entries.push(GraphEntry::PutEdge {
            edge_id: EdgeId(edge["id"].as_u64().unwrap()),
            source: NodeId(edge["source"].as_u64().unwrap()),
            destination: NodeId(edge["target"].as_u64().unwrap()),
            relationship: edge["relationship"].as_str().unwrap().into(),
            key: Some(edge["public_id"].as_str().unwrap().into()),
            properties,
        });
    }
    state
        .apply(
            2,
            &GraphBody {
                graph_id: G,
                entries,
            },
        )
        .unwrap();
    state
}

#[test]
fn golden_scopes_and_paths_match_the_oracle_on_exact_ids() {
    let (mut scopes, mut paths) = (0, 0);
    let clock = || 0;
    for case in cases() {
        let name = case["name"].as_str().unwrap();
        let state = exact_graph(&case);
        let graph = state.graph(G).unwrap();
        let (request, path) = fixture_request(&case, &NodeSelector::Id);
        let options = fixture_options(&case);

        // S1: the engine's projection is exactly the expected scope.
        let (scope, algorithm) = engine_request(&degree_request(&request)).unwrap();
        let context = ExecutionContext::new(5, &clock);
        let projection = kernel::run(
            graph,
            &scope,
            &algorithm,
            &options.limits,
            &options.algorithm_limits,
            &context,
        )
        .unwrap()
        .projection
        .unwrap();
        let expected = &case["expected"];
        assert_eq!(
            projection
                .nodes
                .iter()
                .map(|node| node.0)
                .collect::<Vec<_>>(),
            ids(&expected["scope_node_ids"]),
            "{name}"
        );
        assert_eq!(
            projection
                .edges
                .iter()
                .map(|edge| edge.id.0)
                .collect::<Vec<_>>(),
            ids(&expected["scope_edge_ids"]),
            "{name}"
        );

        // Degree over the same scope through the public mapping.
        let read = run(graph, 5, 0, &degree_request(&request), &options, &clock).unwrap();
        assert!(read.complete && read.scope_complete && read.computation_complete);
        assert_eq!(degree_rows(&read), fixture_degrees(&case), "{name}");
        assert_eq!(read.stats.nodes_extracted, projection.nodes.len() as u64);
        assert_eq!(read.stats.edges_extracted, projection.edges.len() as u64);
        scopes += 1;

        // S2: the path outcome.
        if path {
            let read = run(graph, 5, 0, &request, &options, &clock).unwrap();
            assert!(read.complete, "{name}");
            let expected = fixture_path(&case, &|id| id, &|id| id);
            assert_eq!(*path_outcome(&read), expected, "{name}");
            paths += 1;
        }
    }
    assert_eq!((scopes, paths), (21, 8));
}

/// Fixture-to-database node and edge IDs of one case.
type IdMaps = (BTreeMap<u64, u64>, BTreeMap<u64, u64>);

/// Builds every case as one graph of a database file. Nodes and edges are
/// created in ascending fixture-ID order, so assigned IDs keep the
/// fixture's order.
fn golden_database(path: &Path) -> (Database, Vec<IdMaps>) {
    let mut db = Database::create(path, 2).unwrap();
    let mut maps = Vec::new();
    for case in cases() {
        let name = case["name"].as_str().unwrap();
        db.create_graph(name).unwrap();
        let mut graph = db.graph_mut(name).unwrap();
        let (mut nodes, mut edges, mut keys) = (BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
        for node in sorted(&case, "nodes") {
            let labels = names(&node["labels"]).unwrap();
            let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
            let key = node["public_id"].as_str().unwrap();
            let id = graph
                .create_node(key, &labels, &Metadata::new())
                .unwrap()
                .id;
            nodes.insert(node["id"].as_u64().unwrap(), id);
            keys.insert(node["id"].as_u64().unwrap(), key.to_owned());
        }
        for edge in sorted(&case, "edges") {
            let mut properties = Metadata::new();
            properties.insert("weight".into(), weight(&edge["weight"]));
            let id = graph
                .create_edge(
                    Some(edge["public_id"].as_str().unwrap()),
                    &keys[&edge["source"].as_u64().unwrap()],
                    edge["relationship"].as_str().unwrap(),
                    &keys[&edge["target"].as_u64().unwrap()],
                    &properties,
                )
                .unwrap()
                .id;
            edges.insert(edge["id"].as_u64().unwrap(), id);
        }
        maps.push((nodes, edges));
    }
    (db, maps)
}

#[test]
fn golden_cases_through_the_public_api() {
    let dir = tempfile::tempdir().unwrap();
    let (db, maps) = golden_database(&dir.path().join("golden.vlite"));
    let mut paths = 0;
    for (case, (nodes, edges)) in cases().iter().zip(&maps) {
        let name = case["name"].as_str().unwrap();
        let graph = db.graph(name).unwrap();
        let (request, path) = fixture_request(case, &|id| NodeSelector::Id(nodes[&id]));
        let options = fixture_options(case);

        let read = graph
            .algorithm(&degree_request(&request), &options)
            .unwrap();
        assert!(read.complete, "{name}");
        assert_eq!(read.view_sequence, graph.view_sequence());
        let mapped: Vec<_> = fixture_degrees(case)
            .into_iter()
            .map(|(node, in_degree, out_degree, incident)| {
                (nodes[&node], in_degree, out_degree, incident)
            })
            .collect();
        assert_eq!(degree_rows(&read), mapped, "{name}");
        assert_eq!(
            read.stats.edges_extracted,
            ids(&case["expected"]["scope_edge_ids"]).len() as u64
        );

        if path {
            let read = graph.algorithm(&request, &options).unwrap();
            let expected = fixture_path(case, &|id| nodes[&id], &|id| edges[&id]);
            assert_eq!(*path_outcome(&read), expected, "{name}");
            paths += 1;
        }
    }
    assert_eq!(paths, 8);
}

// Hand-built graphs through the public API.

/// Node and edge IDs by key.
type Keys = BTreeMap<&'static str, u64>;

fn key(key: &str) -> NodeSelector {
    NodeSelector::Key(key.into())
}

fn weighted(value: MetadataValue) -> Metadata {
    Metadata::from([("weight".to_owned(), value)])
}

/// Creates `nodes` (key, labels) and `edges` (key, source, type,
/// destination, weight or none) in graph `name`.
fn build(
    db: &mut Database,
    name: &str,
    nodes: &[(&'static str, &[&str])],
    edges: &[(&'static str, &str, &str, &str, Option<MetadataValue>)],
) -> (Keys, Keys) {
    db.create_graph(name).unwrap();
    let mut graph = db.graph_mut(name).unwrap();
    let (mut node_ids, mut edge_ids) = (Keys::new(), Keys::new());
    for (key, labels) in nodes {
        let id = graph.create_node(key, labels, &Metadata::new()).unwrap().id;
        node_ids.insert(key, id);
    }
    for (key, source, kind, destination, weight) in edges {
        let properties = weight.clone().map_or_else(Metadata::new, weighted);
        let id = graph
            .create_edge(Some(key), source, kind, destination, &properties)
            .unwrap()
            .id;
        edge_ids.insert(key, id);
    }
    (node_ids, edge_ids)
}

/// Graph "g": nodes a, b, c labelled N, d labelled N and M, x labelled M;
/// edges ab, bc, ac (type R, weights 1, 2, 5), cd (type S, weight 1), the
/// self-loop aa (R, weight 0) and xa (R, weight 1).
fn small(db: &mut Database) -> (Keys, Keys) {
    let int = |value| Some(MetadataValue::Integer(value));
    build(
        db,
        "g",
        &[
            ("a", &["N"]),
            ("b", &["N"]),
            ("c", &["N"]),
            ("d", &["N", "M"]),
            ("x", &["M"]),
        ],
        &[
            ("ab", "a", "R", "b", int(1)),
            ("bc", "b", "R", "c", int(2)),
            ("ac", "a", "R", "c", int(5)),
            ("cd", "c", "S", "d", int(1)),
            ("aa", "a", "R", "a", int(0)),
            ("xa", "x", "R", "a", int(1)),
        ],
    )
}

fn memory() -> Database {
    Database::create_in_memory(2, crate::CreateOptions::default()).unwrap()
}

fn scope(seeds: Vec<NodeSelector>, direction: Direction) -> GraphAlgorithmScope {
    GraphAlgorithmScope {
        seeds,
        direction,
        relationship_types: RelationshipScope::Any,
        node_labels: LabelScope::Any,
        edge_conditions: Vec::new(),
    }
}

fn degree(scope: GraphAlgorithmScope) -> GraphAlgorithmRequest {
    GraphAlgorithmRequest {
        scope,
        algorithm: GraphAlgorithm::Degree,
    }
}

/// A path request from seed `source` to `destination`.
fn path(
    source: &str,
    destination: &str,
    direction: Direction,
    weight: Option<&str>,
) -> GraphAlgorithmRequest {
    GraphAlgorithmRequest {
        scope: scope(vec![key(source)], direction),
        algorithm: GraphAlgorithm::WeightedShortestPath {
            source: key(source),
            destination: key(destination),
            weight_property: weight.map(str::to_owned),
        },
    }
}

fn hops(max_hops: u64) -> GraphAlgorithmOptions<'static> {
    GraphAlgorithmOptions {
        limits: GraphLimits {
            max_hops,
            ..GraphLimits::DEFAULT
        },
        ..GraphAlgorithmOptions::default()
    }
}

fn found(cost: f64, nodes: &[u64], edges: &[u64]) -> WeightedPathOutcome {
    WeightedPathOutcome::Found(WeightedPath {
        cost,
        node_ids: nodes.to_vec(),
        edge_ids: edges.to_vec(),
    })
}

fn rows(nodes: &Keys, expected: &[(&str, u64, u64, u64)]) -> Vec<(u64, u64, u64, u64)> {
    expected
        .iter()
        .map(|(key, in_degree, out_degree, incident)| {
            (nodes[key], *in_degree, *out_degree, *incident)
        })
        .collect()
}

#[test]
fn degree_follows_direction_filters_and_physical_edges() {
    let mut db = memory();
    let (n, _) = small(&mut db);
    let graph = db.graph("g").unwrap();

    // Both: every node within two hops; the self-loop is one physical edge
    // and one arc, counted once in each degree.
    let read = graph
        .algorithm(&degree(scope(vec![key("a")], Direction::Both)), &hops(2))
        .unwrap();
    let expected = rows(
        &n,
        &[
            ("a", 2, 3, 4),
            ("b", 1, 1, 2),
            ("c", 2, 1, 3),
            ("d", 1, 0, 1),
            ("x", 0, 1, 1),
        ],
    );
    assert_eq!(degree_rows(&read), expected);
    assert_eq!(
        (
            read.stats.nodes_extracted,
            read.stats.edges_extracted,
            read.stats.arcs
        ),
        (5, 6, 11)
    );
    assert!(read.complete && read.truncation.is_empty());
    assert_eq!((read.algorithm_version, read.max_hops), (1, 2));

    // In: discovery follows incoming edges and degrees reverse endpoints.
    let read = graph
        .algorithm(&degree(scope(vec![key("a")], Direction::In)), &hops(1))
        .unwrap();
    assert_eq!(
        degree_rows(&read),
        rows(&n, &[("a", 1, 2, 2), ("x", 1, 0, 1)])
    );
    assert_eq!(read.stats.arcs, 2);

    // Out with zero hops: the seed and its self-loop only.
    let read = graph
        .algorithm(&degree(scope(vec![key("a")], Direction::Out)), &hops(0))
        .unwrap();
    assert_eq!(degree_rows(&read), rows(&n, &[("a", 1, 1, 1)]));

    // Types, all-labels and edge conditions apply to discovery and to the
    // induced edges: S, M-only x and the zero-weight loop are excluded.
    let mut filtered = scope(vec![key("a")], Direction::Both);
    filtered.relationship_types = RelationshipScope::Only(vec!["R".into()]);
    filtered.node_labels = LabelScope::All(vec!["N".into()]);
    filtered.edge_conditions = vec![Condition::Range {
        field: "weight".into(),
        min: Some(MetadataValue::Integer(1)),
        max: None,
    }];
    let read = graph.algorithm(&degree(filtered), &hops(4)).unwrap();
    assert_eq!(
        degree_rows(&read),
        rows(&n, &[("a", 0, 2, 2), ("b", 1, 1, 2), ("c", 2, 0, 2)])
    );

    // An excluded node never bridges: from d with label M, c is excluded.
    let mut bridged = scope(vec![key("d")], Direction::Both);
    bridged.node_labels = LabelScope::All(vec!["M".into()]);
    let read = graph.algorithm(&degree(bridged), &hops(4)).unwrap();
    assert_eq!(degree_rows(&read), rows(&n, &[("d", 0, 0, 0)]));
}

#[test]
fn weighted_paths_respect_hops_weights_and_scope() {
    let mut db = memory();
    let (n, e) = small(&mut db);
    let graph = db.graph("g").unwrap();
    let outcome = |request: &GraphAlgorithmRequest, max_hops| {
        let read = graph.algorithm(request, &hops(max_hops)).unwrap();
        assert!(read.complete && read.computation_complete);
        path_outcome(&read).clone()
    };
    let weight = Some("weight");

    // Cheapest within two hops; the costlier direct edge wins with one.
    let to_c = path("a", "c", Direction::Out, weight);
    assert_eq!(
        outcome(&to_c, 2),
        found(3.0, &[n["a"], n["b"], n["c"]], &[e["ab"], e["bc"]])
    );
    assert_eq!(outcome(&to_c, 1), found(5.0, &[n["a"], n["c"]], &[e["ac"]]));

    // A costlier shorter prefix stays feasible: a-c-d within two hops,
    // a-b-c-d within three.
    let to_d = path("a", "d", Direction::Out, weight);
    assert_eq!(
        outcome(&to_d, 2),
        found(6.0, &[n["a"], n["c"], n["d"]], &[e["ac"], e["cd"]])
    );
    assert_eq!(
        outcome(&to_d, 3),
        found(
            4.0,
            &[n["a"], n["b"], n["c"], n["d"]],
            &[e["ab"], e["bc"], e["cd"]]
        )
    );

    // Unit weights count edges.
    let unit = path("a", "c", Direction::Out, None);
    assert_eq!(outcome(&unit, 2), found(1.0, &[n["a"], n["c"]], &[e["ac"]]));

    // The source itself is a zero-hop path.
    let itself = path("a", "a", Direction::Out, weight);
    assert_eq!(outcome(&itself, 2), found(0.0, &[n["a"]], &[]));

    // Direction, hops and types bound the certified search.
    let backwards = path("a", "x", Direction::Out, weight);
    assert_eq!(
        outcome(&backwards, 4),
        WeightedPathOutcome::NoPathWithinScopeAndHops
    );
    let reversed = path("a", "x", Direction::In, weight);
    assert_eq!(
        outcome(&reversed, 1),
        found(1.0, &[n["a"], n["x"]], &[e["xa"]])
    );
    let mut typed = path("a", "d", Direction::Out, weight);
    typed.scope.relationship_types = RelationshipScope::Only(vec!["R".into()]);
    assert_eq!(
        outcome(&typed, 4),
        WeightedPathOutcome::NoPathWithinScopeAndHops
    );
    assert_eq!(
        outcome(&to_d, 1),
        WeightedPathOutcome::NoPathWithinScopeAndHops
    );
}
fn read_error(result: Result<GraphAlgorithmRead, GraphAlgorithmError>) -> Error {
    match result {
        Err(GraphAlgorithmError::Read(error)) => error,
        other => panic!("expected a read error, got {other:?}"),
    }
}

fn invalid_query(result: Result<GraphAlgorithmRead, GraphAlgorithmError>) -> &'static str {
    match read_error(result) {
        Error::InvalidQuery(reason) => reason,
        other => panic!("expected InvalidQuery, got {other:?}"),
    }
}

#[test]
fn invalid_requests_are_typed_errors_before_any_work() {
    let mut db = memory();
    small(&mut db);
    build(&mut db, "h", &[("only_h", &["N"])], &[]);
    let graph = db.graph("g").unwrap();
    let request = degree(scope(vec![key("a")], Direction::Out));
    let run = |request: &GraphAlgorithmRequest, options: &GraphAlgorithmOptions<'_>| {
        graph.algorithm(request, options)
    };

    // Limits: the existing read ceilings, then the algorithm caps; both
    // before conditions are checked.
    let mut options = hops(5);
    let mut bad_filter = request.clone();
    bad_filter.scope.edge_conditions = vec![Condition::Range {
        field: "weight".into(),
        min: None,
        max: None,
    }];
    assert!(matches!(
        read_error(run(&bad_filter, &options)),
        Error::GraphLimitExceedsCeiling {
            limit: "max_hops",
            requested: 5,
            ceiling: 4
        }
    ));
    options = hops(2);
    options.algorithm_limits.max_vertex_operations = 1_000_001;
    assert!(matches!(
        read_error(run(&bad_filter, &options)),
        Error::GraphLimitExceedsCeiling {
            limit: "max_vertex_operations",
            requested: 1_000_001,
            ceiling: 1_000_000
        }
    ));
    options = hops(2);
    options.algorithm_limits.max_scratch_bytes = 0;
    assert_eq!(
        invalid_query(run(&request, &options)),
        "max_scratch_bytes must be at least 1"
    );
    options = hops(2);
    options.limits.max_edges_examined = 0;
    assert_eq!(
        invalid_query(run(&request, &options)),
        "max_edges_examined must be at least 1"
    );
    assert!(matches!(
        read_error(run(&bad_filter, &hops(2))),
        Error::InvalidFilter(_)
    ));

    // Scope shape.
    let options = hops(2);
    let mut empty = request.clone();
    empty.scope.seeds.clear();
    assert_eq!(
        invalid_query(run(&empty, &options)),
        "an algorithm scope needs a seed"
    );
    let mut no_types = request.clone();
    no_types.scope.relationship_types = RelationshipScope::Only(Vec::new());
    assert_eq!(
        invalid_query(run(&no_types, &options)),
        "a relationship type scope must not be empty"
    );
    let mut bad_type = request.clone();
    bad_type.scope.relationship_types = RelationshipScope::Only(vec!["bad type".into()]);
    invalid_query(run(&bad_type, &options));
    let mut no_labels = request.clone();
    no_labels.scope.node_labels = LabelScope::All(Vec::new());
    assert_eq!(
        invalid_query(run(&no_labels, &options)),
        "a label scope must not be empty"
    );
    let mut bad_key = request.clone();
    bad_key.scope.seeds = vec![key("")];
    invalid_query(run(&bad_key, &options));
    let no_weight_name = path("a", "c", Direction::Out, Some(""));
    assert_eq!(
        invalid_query(run(&no_weight_name, &options)),
        "a weight property name must have 1..=256 bytes"
    );

    // Resolution: seeds and endpoints are visible nodes of this graph.
    for seed in [key("missing"), key("only_h"), NodeSelector::Id(999)] {
        let mut missing = request.clone();
        missing.scope.seeds = vec![key("a"), seed];
        assert!(matches!(
            read_error(run(&missing, &options)),
            Error::GraphNodeNotFound
        ));
    }
    let foreign = path("a", "only_h", Direction::Out, None);
    assert!(matches!(
        read_error(run(&foreign, &options)),
        Error::GraphNodeNotFound
    ));
    let mut not_seed = path("a", "c", Direction::Out, None);
    not_seed.algorithm = GraphAlgorithm::WeightedShortestPath {
        source: key("b"),
        destination: key("c"),
        weight_property: None,
    };
    assert_eq!(
        invalid_query(run(&not_seed, &options)),
        "the path source must be a seed"
    );
    let mut unlabelled = degree(scope(vec![key("a"), key("x")], Direction::Out));
    unlabelled.scope.node_labels = LabelScope::All(vec!["N".into()]);
    assert_eq!(
        invalid_query(run(&unlabelled, &options)),
        "every seed must carry every scope label"
    );
    let mut crowded = hops(2);
    crowded.limits.max_visited_nodes = 1;
    let two = degree(scope(vec![key("a"), key("b"), key("a")], Direction::Out));
    assert_eq!(
        invalid_query(run(&two, &crowded)),
        "distinct seeds exceed max_visited_nodes"
    );

    // Display and source.
    let error = run(&empty, &options).unwrap_err();
    assert_eq!(
        error.to_string(),
        Error::InvalidQuery("an algorithm scope needs a seed").to_string()
    );
    assert!(std::error::Error::source(&error).is_some());
    let error = GraphAlgorithmError::MissingWeight { edge_id: 7 };
    assert_eq!(error.to_string(), "graph edge 7 has no weight property");
    assert!(std::error::Error::source(&error).is_none());
    let error = GraphAlgorithmError::InvalidWeight {
        edge_id: 7,
        reason: "weights must be nonnegative",
    };
    assert_eq!(
        error.to_string(),
        "invalid weight on graph edge 7: weights must be nonnegative"
    );
    assert_eq!(
        GraphAlgorithmError::NumericOverflow.to_string(),
        "path cost exceeds the largest finite f64"
    );
    assert!(matches!(
        GraphAlgorithmError::from(Error::GraphNodeNotFound),
        GraphAlgorithmError::Read(Error::GraphNodeNotFound)
    ));
}

#[test]
fn weights_are_validated_over_the_retained_scope() {
    let mut db = memory();
    let (n, e) = build(
        &mut db,
        "w",
        &[("p", &["N"]), ("q", &["N"]), ("r", &["N"]), ("s", &["N"])],
        &[
            ("pq", "p", "A", "q", Some(MetadataValue::Integer(1))),
            ("qr", "q", "B", "r", None),
            (
                "pr",
                "p",
                "C",
                "r",
                Some(MetadataValue::String("heavy".into())),
            ),
            ("rs", "r", "D", "s", Some(MetadataValue::Float(-0.5))),
            (
                "sp",
                "s",
                "E",
                "p",
                Some(MetadataValue::Integer((1 << 53) + 1)),
            ),
            ("qs", "q", "F", "s", Some(MetadataValue::Float(-0.0))),
        ],
    );
    let graph = db.graph("w").unwrap();
    let typed = |source: &str, types: &[&str]| {
        let mut request = path(source, "s", Direction::Out, Some("weight"));
        request.scope.relationship_types =
            RelationshipScope::Only(types.iter().map(|kind| (*kind).to_owned()).collect());
        graph.algorithm(&request, &hops(3))
    };

    // The smallest offending retained edge decides the error.
    assert!(matches!(
        typed("p", &["A", "B", "C", "D"]),
        Err(GraphAlgorithmError::MissingWeight { edge_id }) if edge_id == e["qr"]
    ));
    assert!(matches!(
        typed("p", &["A", "C", "D"]),
        Err(GraphAlgorithmError::InvalidWeight { edge_id, reason: "weights must be integers or floats" })
            if edge_id == e["pr"]
    ));
    assert!(matches!(
        typed("r", &["D"]),
        Err(GraphAlgorithmError::InvalidWeight { edge_id, reason: "weights must be nonnegative" })
            if edge_id == e["rs"]
    ));
    assert!(matches!(
        typed("s", &["E"]),
        Err(GraphAlgorithmError::InvalidWeight { edge_id, reason: "integer weights must be at most 2^53" })
            if edge_id == e["sp"]
    ));

    // Negative zero is zero; excluded edges are never validated.
    let read = typed("p", &["A", "F"]).unwrap();
    assert_eq!(
        *path_outcome(&read),
        found(1.0, &[n["p"], n["q"], n["s"]], &[e["pq"], e["qs"]])
    );

    // Unit weights and degree ignore weight properties.
    let unit = graph
        .algorithm(&path("p", "s", Direction::Out, None), &hops(3))
        .unwrap();
    assert_eq!(
        *path_outcome(&unit),
        found(2.0, &[n["p"], n["q"], n["s"]], &[e["pq"], e["qs"]])
    );
    let read = graph
        .algorithm(&degree(scope(vec![key("p")], Direction::Out)), &hops(3))
        .unwrap();
    assert_eq!(read.stats.edges_extracted, 6);

    // Exact sums round once; a winning cost past f64::MAX is an error.
    let mut db = memory();
    let (n, e) = build(
        &mut db,
        "o",
        &[("u", &["N"]), ("v", &["N"]), ("t", &["N"])],
        &[
            ("uv", "u", "R", "v", Some(MetadataValue::Float(f64::MAX))),
            ("vt", "v", "R", "t", Some(MetadataValue::Float(f64::MAX))),
        ],
    );
    let graph = db.graph("o").unwrap();
    let read = graph
        .algorithm(&path("u", "v", Direction::Out, Some("weight")), &hops(2))
        .unwrap();
    assert_eq!(
        *path_outcome(&read),
        found(f64::MAX, &[n["u"], n["v"]], &[e["uv"]])
    );
    assert!(matches!(
        graph.algorithm(&path("u", "t", Direction::Out, Some("weight")), &hops(2)),
        Err(GraphAlgorithmError::NumericOverflow)
    ));
}

/// Far from any wall clock; reads set their clock explicitly.
const FAR: u64 = u64::MAX / 2;

#[test]
fn one_ttl_clock_decides_visibility_at_the_deadline() {
    let mut db = memory();
    let (n, e) = small(&mut db);
    let mut graph = db.graph_mut("g").unwrap();
    graph
        .set_node_expiry("b", Some(FAR), Expected::Any)
        .unwrap();
    graph
        .set_edge_expiry(EdgeRef::Key("ac".into()), Some(FAR + 10), Expected::Any)
        .unwrap();
    let graph = db.graph("g").unwrap();
    let to_c = path("a", "c", Direction::Out, Some("weight"));

    // Before the deadline b is visible; at it, b and its edges are hidden.
    let before = graph
        .with_clock(FAR - 1)
        .algorithm(&to_c, &hops(2))
        .unwrap();
    assert_eq!(
        *path_outcome(&before),
        found(3.0, &[n["a"], n["b"], n["c"]], &[e["ab"], e["bc"]])
    );
    assert_eq!(before.read_time_unix_ms, FAR - 1);
    let at = graph.with_clock(FAR).algorithm(&to_c, &hops(2)).unwrap();
    assert_eq!(
        *path_outcome(&at),
        found(5.0, &[n["a"], n["c"]], &[e["ac"]])
    );
    assert_eq!(at.read_time_unix_ms, FAR);
    let read = graph
        .with_clock(FAR)
        .algorithm(&degree(scope(vec![key("a")], Direction::Both)), &hops(2))
        .unwrap();
    assert_eq!(
        degree_rows(&read),
        rows(
            &n,
            &[
                ("a", 2, 2, 3),
                ("c", 1, 1, 2),
                ("d", 1, 0, 1),
                ("x", 0, 1, 1)
            ]
        )
    );

    // A due seed or endpoint does not resolve.
    for request in [
        path("b", "c", Direction::Out, None),
        path("a", "b", Direction::Out, None),
    ] {
        assert!(matches!(
            read_error(graph.with_clock(FAR).algorithm(&request, &hops(2))),
            Error::GraphNodeNotFound
        ));
    }

    // A due edge is hidden at its deadline: the certified answer is no path.
    let last = graph
        .with_clock(FAR + 9)
        .algorithm(&to_c, &hops(2))
        .unwrap();
    assert_eq!(
        *path_outcome(&last),
        found(5.0, &[n["a"], n["c"]], &[e["ac"]])
    );
    let gone = graph
        .with_clock(FAR + 10)
        .algorithm(&to_c, &hops(2))
        .unwrap();
    assert!(gone.complete);
    assert_eq!(
        *path_outcome(&gone),
        WeightedPathOutcome::NoPathWithinScopeAndHops
    );
}
#[test]
fn budgets_cancellation_and_deadline_return_no_output() {
    let mut db = memory();
    let (n, _) = small(&mut db);
    // Pinned so whole results compare equal across calls.
    let graph = db.graph("g").unwrap().with_clock(crate::current_time_ms());
    let request = degree(scope(vec![key("a")], Direction::Both));
    let stopped = |read: GraphAlgorithmRead, reason: GraphAlgorithmTruncation| {
        assert_eq!(read.truncation, vec![reason]);
        assert_eq!(read.output, None);
        assert!(!read.complete && !read.scope_complete && !read.computation_complete);
        assert_eq!(
            (read.stats.nodes_extracted, read.stats.edges_extracted),
            (0, 0)
        );
    };

    let cancel = AtomicBool::new(true);
    let options = GraphAlgorithmOptions {
        cancel: Some(&cancel),
        ..hops(2)
    };
    let read = graph.algorithm(&request, &options).unwrap();
    assert_eq!(read.stats.edges_examined + read.stats.vertex_operations, 0);
    stopped(read, GraphAlgorithmTruncation::Cancelled);

    // A fake monotonic clock: each reading is a second later.
    let ticks = Cell::new(0);
    let clock = || {
        ticks.set(ticks.get() + 1_000);
        ticks.get()
    };
    let read = graph
        .algorithm_with_clock(&request, &hops(2), &clock)
        .unwrap();
    stopped(read, GraphAlgorithmTruncation::Deadline);

    let mut options = hops(2);
    options.limits.max_visited_nodes = 1;
    stopped(
        graph.algorithm(&request, &options).unwrap(),
        GraphAlgorithmTruncation::VisitedLimit,
    );
    let mut options = hops(2);
    options.limits.max_edges_examined = 1;
    stopped(
        graph.algorithm(&request, &options).unwrap(),
        GraphAlgorithmTruncation::EdgeLimit,
    );
    let mut options = hops(2);
    options.limits.max_edges_per_node = 1;
    stopped(
        graph.algorithm(&request, &options).unwrap(),
        GraphAlgorithmTruncation::DegreeLimit {
            node_ids: vec![n["a"]],
        },
    );
    let mut options = hops(2);
    options.algorithm_limits.max_vertex_operations = 1;
    stopped(
        graph.algorithm(&request, &options).unwrap(),
        GraphAlgorithmTruncation::VertexLimit,
    );
    let mut options = hops(2);
    options.algorithm_limits.max_scratch_bytes = 1;
    stopped(
        graph.algorithm(&request, &options).unwrap(),
        GraphAlgorithmTruncation::MemoryLimit,
    );

    // A path search that runs out of budget is not certified: no outcome.
    let full = graph
        .algorithm(&path("a", "d", Direction::Out, Some("weight")), &hops(3))
        .unwrap();
    let mut options = hops(3);
    options.algorithm_limits.max_vertex_operations = full.stats.vertex_operations - 1;
    let read = graph
        .algorithm(&path("a", "d", Direction::Out, Some("weight")), &options)
        .unwrap();
    assert_eq!(read.truncation, vec![GraphAlgorithmTruncation::VertexLimit]);
    assert!(read.scope_complete && !read.computation_complete && read.output.is_none());
    options.algorithm_limits.max_vertex_operations = full.stats.vertex_operations;
    let read = graph
        .algorithm(&path("a", "d", Direction::Out, Some("weight")), &options)
        .unwrap();
    assert_eq!(
        read,
        GraphAlgorithmRead {
            stats: GraphAlgorithmStats {
                elapsed_ms: read.stats.elapsed_ms,
                ..full.stats
            },
            ..full.clone()
        }
    );
}

#[test]
fn output_caps_apply_after_a_complete_computation() {
    let mut db = memory();
    let (n, _) = small(&mut db);
    let graph = db.graph("g").unwrap();

    // Degree rows are cut to max_output_nodes.
    let mut options = hops(2);
    options.limits.max_output_nodes = 2;
    let read = graph
        .algorithm(&degree(scope(vec![key("a")], Direction::Both)), &options)
        .unwrap();
    assert_eq!(
        degree_rows(&read),
        rows(&n, &[("a", 2, 3, 4), ("b", 1, 1, 2)])
    );
    assert_eq!(read.truncation, vec![GraphAlgorithmTruncation::OutputLimit]);
    assert!(!read.complete && read.scope_complete && read.computation_complete);
    assert_eq!(read.stats.nodes_extracted, 5);

    // A path over a cap is withheld, never cut.
    let to_c = path("a", "c", Direction::Out, Some("weight"));
    let caps: [fn(&mut GraphLimits); 3] = [
        |limits| limits.max_output_edges = 1,
        |limits| limits.max_output_nodes = 2,
        |limits| limits.max_output_paths = 0,
    ];
    for cap in caps {
        let mut options = hops(2);
        cap(&mut options.limits);
        let read = graph.algorithm(&to_c, &options).unwrap();
        assert_eq!(read.output, None);
        assert_eq!(read.truncation, vec![GraphAlgorithmTruncation::OutputLimit]);
        assert!(read.scope_complete && read.computation_complete);
    }

    // A certified absence returns no items, so no cap applies.
    let mut options = hops(4);
    options.limits.max_output_paths = 0;
    let read = graph
        .algorithm(&path("a", "x", Direction::Out, Some("weight")), &options)
        .unwrap();
    assert!(read.complete);
    assert_eq!(
        *path_outcome(&read),
        WeightedPathOutcome::NoPathWithinScopeAndHops
    );
}

#[test]
fn requests_are_normalized_and_reads_do_not_mutate() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("graph.vlite");
    let mut db = Database::create(&file, 2).unwrap();
    let (n, e) = small(&mut db);
    db.close().unwrap();
    let bytes = std::fs::read(&file).unwrap();

    let db = Database::open_read_only(&file).unwrap();
    // One pinned TTL clock: an unpinned handle samples the time per call.
    let now = crate::current_time_ms();
    let graph = db.graph("g").unwrap().with_clock(now);
    let state = graph.graph.clone();
    let conditions = vec![Condition::Range {
        field: "weight".into(),
        min: Some(MetadataValue::Integer(0)),
        max: None,
    }];
    let request = GraphAlgorithmRequest {
        scope: GraphAlgorithmScope {
            seeds: vec![key("c"), NodeSelector::Id(n["a"]), key("a"), key("c")],
            direction: Direction::Out,
            relationship_types: RelationshipScope::Only(vec!["S".into(), "R".into(), "R".into()]),
            node_labels: LabelScope::All(vec!["N".into(), "N".into()]),
            edge_conditions: conditions.clone(),
        },
        algorithm: GraphAlgorithm::WeightedShortestPath {
            source: key("a"),
            destination: key("d"),
            weight_property: Some("weight".into()),
        },
    };
    let first = graph.algorithm(&request, &hops(3)).unwrap();
    assert_eq!(
        *path_outcome(&first),
        found(
            4.0,
            &[n["a"], n["b"], n["c"], n["d"]],
            &[e["ab"], e["bc"], e["cd"]]
        )
    );
    assert_eq!(
        first.request,
        GraphAlgorithmRequest {
            scope: GraphAlgorithmScope {
                seeds: vec![NodeSelector::Id(n["a"]), NodeSelector::Id(n["c"])],
                direction: Direction::Out,
                relationship_types: RelationshipScope::Only(vec!["R".into(), "S".into()]),
                node_labels: LabelScope::All(vec!["N".into()]),
                edge_conditions: conditions,
            },
            algorithm: GraphAlgorithm::WeightedShortestPath {
                source: NodeSelector::Id(n["a"]),
                destination: NodeSelector::Id(n["d"]),
                weight_property: Some("weight".into()),
            },
        }
    );
    assert_eq!(first.view_sequence, graph.view_sequence());
    assert_eq!(first.read_time_unix_ms, now);
    assert_eq!((first.algorithm_version, first.max_hops), (1, 3));

    // The normalized request reproduces the result; reads are repeatable
    // and leave the state and the file untouched.
    let again = graph.algorithm(&first.request, &hops(3)).unwrap();
    let mut second = graph.algorithm(&request, &hops(3)).unwrap();
    second.stats.elapsed_ms = first.stats.elapsed_ms;
    assert_eq!(second, first);
    assert_eq!(again.output, first.output);
    assert_eq!(again.request, first.request);
    assert_eq!(graph.graph, &state);
    drop(db);
    assert_eq!(std::fs::read(&file).unwrap(), bytes);
}
