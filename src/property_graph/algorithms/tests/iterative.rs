//! Adapter tests of the iterative algorithms (Gap 5 S3): PageRank and label
//! propagation.
//!
//! Stationary goldens come from `tests/fixtures/graph_algorithms/golden.json`
//! (checked by `tests/graph_algorithm_oracle.py`): its 6 PageRank cases
//! match within 1e-8 with total mass 1 within 1e-12, and its 3 label
//! propagation cases match exactly, every per-sweep assignment included, on
//! exact IDs and through a database file. Finite-step goldens come from
//! `tests/fixtures/graph_algorithms/iterative-cases.json` (checked by
//! `tests/graph_iteration_oracle.py`): all 12 cases, each of their 17 full
//! steps or sweeps within 1e-14, and each result. Their synthetic update
//! counts are not engine budgets: an interruption maps to the vertex budget
//! that stops the same candidate update of the same step. The remaining
//! tests cover the request echo, partial-result provenance, typed errors and
//! the shared budget.

use super::*;

const ITERATIVE: &str =
    include_str!("../../../../tests/fixtures/graph_algorithms/iterative-cases.json");

fn rank_output(read: &GraphAlgorithmRead) -> &PageRankOutput {
    match &read.output {
        Some(GraphAlgorithmOutput::PageRank(output)) => output,
        other => panic!("expected scores, got {other:?}"),
    }
}

fn community_output(read: &GraphAlgorithmRead) -> &LabelPropagationOutput {
    match &read.output {
        Some(GraphAlgorithmOutput::LabelPropagation(output)) => output,
        other => panic!("expected communities, got {other:?}"),
    }
}

fn community_rows(read: &GraphAlgorithmRead) -> Vec<(u64, u64)> {
    community_output(read)
        .assignments
        .iter()
        .map(|row| (row.node_id, row.community_id))
        .collect()
}

/// `options` with an iteration or sweep cap.
fn capped(
    options: &GraphAlgorithmOptions<'static>,
    max_iterations: u64,
) -> GraphAlgorithmOptions<'static> {
    let mut options = options.clone();
    options.algorithm_limits.max_iterations = max_iterations;
    options
}

/// A fixture personalization: `null` or `[{node_id, mass}]`.
fn masses(value: &Value, node: &dyn Fn(u64) -> NodeSelector) -> Option<Vec<(NodeSelector, f64)>> {
    value.as_array().map(|entries| {
        entries
            .iter()
            .map(|entry| {
                (
                    node(entry["node_id"].as_u64().unwrap()),
                    entry["mass"].as_f64().unwrap(),
                )
            })
            .collect()
    })
}

/// The fixture's iteration or sweep cap.
fn fixture_cap(case: &Value) -> u64 {
    let parameters = &case["parameters"];
    parameters
        .get("max_iterations")
        .or_else(|| parameters.get("max_sweeps"))
        .and_then(Value::as_u64)
        .unwrap()
}

/// The PageRank or label-propagation request of a stationary golden case.
fn stationary_request(case: &Value, node: &dyn Fn(u64) -> NodeSelector) -> GraphAlgorithmRequest {
    let (mut request, _) = fixture_request(case, node);
    let parameters = &case["parameters"];
    request.algorithm = match case["algorithm"].as_str().unwrap() {
        "pagerank" => GraphAlgorithm::PageRank {
            damping: parameters["damping"].as_f64(),
            tolerance: parameters["tolerance"].as_f64(),
            personalization: masses(&parameters["personalization"], node),
            weight_property: match parameters["weight_policy"].as_str().unwrap() {
                "explicit" => Some("weight".into()),
                "unit" => None,
                other => panic!("unknown weight policy {other}"),
            },
        },
        "label_propagation" => GraphAlgorithm::LabelPropagation,
        other => panic!("not iterative: {other}"),
    };
    request
}

/// Stationary scores within 1e-8 and total mass 1 within 1e-12; `node`
/// maps fixture IDs.
fn check_stationary(read: &GraphAlgorithmRead, case: &Value, node: &dyn Fn(u64) -> u64) {
    let name = case["name"].as_str().unwrap();
    let expected = &case["expected"]["output"];
    assert_eq!(expected["mass"], 1);
    assert!(read.complete && read.computation_complete, "{name}");
    let output = rank_output(read);
    assert!(
        output.converged && output.residual <= 1e-10,
        "{name}: {output:?}"
    );
    let rows = expected["stationary_scores"].as_array().unwrap();
    assert_eq!(output.scores.len(), rows.len(), "{name}");
    let mut mass = 0.0_f64;
    for (row, expected) in output.scores.iter().zip(rows) {
        assert_eq!(
            row.node_id,
            node(expected["node_id"].as_u64().unwrap()),
            "{name}"
        );
        let score = expected["score"].as_f64().unwrap();
        assert!(
            (row.score - score).abs() <= 1e-8,
            "{name}: {row:?} vs {score}"
        );
        mass += row.score;
    }
    assert!((mass - 1.0).abs() <= 1e-12, "{name}: mass {mass}");
}

/// The communities of `labels` (`[{node_id, label}]`): each node's
/// community is the smallest node ID sharing its label.
fn canonical(labels: &Value, node: &dyn Fn(u64) -> u64) -> Vec<(u64, u64)> {
    let rows: Vec<(u64, u64)> = labels
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["node_id"].as_u64().unwrap(),
                row["label"].as_u64().unwrap(),
            )
        })
        .collect();
    let mut communities: Vec<(u64, u64)> = rows
        .iter()
        .map(|&(id, label)| {
            let first = rows
                .iter()
                .filter(|other| other.1 == label)
                .map(|other| other.0)
                .min()
                .unwrap();
            (node(id), node(first))
        })
        .collect();
    communities.sort_unstable();
    communities
}

/// A stationary label-propagation case: assignments, sweeps and
/// convergence at the fixture cap, then every sweep through a cap of that
/// many sweeps. `run` runs with the given cap.
fn check_communities(
    case: &Value,
    node: &dyn Fn(u64) -> u64,
    run: &dyn Fn(u64) -> GraphAlgorithmRead,
) {
    let name = case["name"].as_str().unwrap();
    let expected = &case["expected"]["output"];
    let read = run(fixture_cap(case));
    let assignments: Vec<(u64, u64)> = expected["assignments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                node(row["node_id"].as_u64().unwrap()),
                node(row["community_id"].as_u64().unwrap()),
            )
        })
        .collect();
    assert_eq!(community_rows(&read), assignments, "{name}");
    let output = community_output(&read);
    assert_eq!(
        (output.sweeps, output.converged, read.complete),
        (
            expected["sweeps"].as_u64().unwrap(),
            expected["converged"].as_bool().unwrap(),
            expected["complete"].as_bool().unwrap()
        ),
        "{name}"
    );
    if !read.complete {
        assert_eq!(
            read.truncation,
            vec![GraphAlgorithmTruncation::IterationLimit]
        );
        assert!(read.scope_complete && !read.computation_complete);
        assert!(read.request.is_some());
    }
    let sweeps = expected["sweep_assignments"].as_array().unwrap();
    assert_eq!(sweeps.len() as u64, output.sweeps, "{name}");
    for (k, labels) in (1..).zip(sweeps) {
        let read = run(k);
        assert_eq!(
            community_rows(&read),
            canonical(labels, node),
            "{name} sweep {k}"
        );
        assert_eq!(community_output(&read).sweeps, k);
    }
}

fn is_iterative(case: &Value) -> bool {
    case["algorithm"] == "pagerank" || case["algorithm"] == "label_propagation"
}

#[test]
fn stationary_goldens_match_on_exact_ids() {
    let clock = || 0;
    let mut counts = (0, 0);
    for case in cases().iter().filter(|case| is_iterative(case)) {
        let state = exact_graph(case);
        let graph = state.graph(G).unwrap();
        let request = stationary_request(case, &NodeSelector::Id);
        let options = fixture_options(case);
        let at = |cap| run(graph, 5, 0, &request, &capped(&options, cap), &clock).unwrap();
        if case["algorithm"] == "pagerank" {
            check_stationary(&at(fixture_cap(case)), case, &|id| id);
            counts.0 += 1;
        } else {
            check_communities(case, &|id| id, &at);
            counts.1 += 1;
        }
    }
    assert_eq!(counts, (6, 3));
}

#[test]
fn stationary_goldens_through_the_public_api() {
    let dir = tempfile::tempdir().unwrap();
    let (db, maps) = golden_database(&dir.path().join("golden.vlite"));
    let mut counts = (0, 0);
    for (case, (nodes, _)) in cases().iter().zip(&maps) {
        if !is_iterative(case) {
            continue;
        }
        let graph = db.graph(case["name"].as_str().unwrap()).unwrap();
        let request = stationary_request(case, &|id| NodeSelector::Id(nodes[&id]));
        let options = fixture_options(case);
        let node = |id| nodes[&id];
        let at = |cap| graph.algorithm(&request, &capped(&options, cap)).unwrap();
        let read = at(fixture_cap(case));
        if case["algorithm"] == "pagerank" {
            check_stationary(&read, case, &node);
            counts.0 += 1;
        } else {
            check_communities(case, &node, &at);
            counts.1 += 1;
        }
        // The echo reruns to the same output.
        let echo = read.request.as_ref().unwrap();
        let again = graph
            .algorithm(echo, &capped(&options, fixture_cap(case)))
            .unwrap();
        assert_eq!(again.output, read.output);
        assert_eq!(again.request, read.request);
    }
    assert_eq!(counts, (6, 3));
}

fn iterative_cases() -> Vec<Value> {
    let fixture: Value = serde_json::from_str(ITERATIVE).unwrap();
    assert_eq!(fixture["schema"], "vlite.graph-iteration-golden");
    assert_eq!(fixture["version"], 1);
    assert_eq!(fixture["contract"]["score_abs_tolerance"], 1e-14);
    assert_eq!(fixture["contract"]["residual_abs_tolerance"], 1e-14);
    let cases = fixture["cases"].as_array().unwrap().clone();
    assert_eq!(cases.len(), 12);
    cases
}

fn property(value: MetadataValue) -> PropertyValue {
    match value {
        MetadataValue::Integer(integer) => PropertyValue::Integer(integer),
        MetadataValue::Float(float) => PropertyValue::Float(float),
        other => panic!("not a weight: {other:?}"),
    }
}

/// A finite-step case's graph on exact IDs: nodes keyed `n{id}`, edges of
/// type `r` carrying their fixture weight, if any, as `weight`.
fn trace_graph(case: &Value) -> GraphState {
    let mut state = GraphState::new();
    let create = GraphBody {
        graph_id: G,
        entries: vec![GraphEntry::CreateGraph { name: "g".into() }],
    };
    state.apply(1, &create).unwrap();
    let mut nodes = ids(&case["nodes"]);
    nodes.sort_unstable();
    let mut entries: Vec<GraphEntry> = nodes
        .iter()
        .map(|&id| GraphEntry::PutNode {
            node_id: NodeId(id),
            key: format!("n{id}"),
            labels: BTreeSet::new(),
            properties: Properties::new(),
        })
        .collect();
    for edge in sorted(case, "edges") {
        let mut properties = Properties::new();
        if let Some(value) = edge.get("weight") {
            properties.insert("weight".into(), property(weight(value)));
        }
        entries.push(GraphEntry::PutEdge {
            edge_id: EdgeId(edge["id"].as_u64().unwrap()),
            source: NodeId(edge["source"].as_u64().unwrap()),
            destination: NodeId(edge["target"].as_u64().unwrap()),
            relationship: "r".into(),
            key: None,
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

/// The case's request and options: PageRank from its seeds along the
/// stored orientation within 4 hops (which reach every node), label
/// propagation from every node.
fn trace_request(case: &Value) -> (GraphAlgorithmRequest, GraphAlgorithmOptions<'static>) {
    let parameters = &case["parameters"];
    let ranked = case["algorithm"] == "pagerank";
    let seeds = if ranked {
        ids(&parameters["seed_ids"])
    } else {
        ids(&case["nodes"])
    };
    let scope = GraphAlgorithmScope {
        seeds: seeds.into_iter().map(NodeSelector::Id).collect(),
        direction: Direction::Out,
        relationship_types: RelationshipScope::Any,
        node_labels: LabelScope::Any,
        edge_conditions: Vec::new(),
    };
    let algorithm = if ranked {
        GraphAlgorithm::PageRank {
            damping: parameters["damping"].as_f64(),
            tolerance: parameters["tolerance"].as_f64(),
            personalization: masses(&parameters["personalization"], &NodeSelector::Id),
            weight_property: Some("weight".into()),
        }
    } else {
        GraphAlgorithm::LabelPropagation
    };
    let options = GraphAlgorithmOptions {
        limits: GraphLimits {
            max_hops: if ranked { 4 } else { 0 },
            ..GraphLimits::DEFAULT
        },
        ..GraphAlgorithmOptions::default()
    };
    let options = capped(&options, fixture_cap(case));
    (GraphAlgorithmRequest { scope, algorithm }, options)
}

/// Fixture scores (`[{node_id, score}]`) within 1e-14.
fn check_scores(scores: &[NodeScore], expected: &Value, name: &str) {
    let expected = expected.as_array().unwrap();
    assert_eq!(scores.len(), expected.len(), "{name}");
    for (row, expected) in scores.iter().zip(expected) {
        assert_eq!(row.node_id, expected["node_id"].as_u64().unwrap(), "{name}");
        let score = expected["score"].as_f64().unwrap();
        assert!(
            (row.score - score).abs() <= 1e-14,
            "{name}: {row:?} vs {score}"
        );
    }
}

/// One full step or sweep of the trace: `read` stopped after `step` of
/// them.
fn check_step(case: &Value, read: &GraphAlgorithmRead, entry: &Value, step: u64) {
    let name = case["name"].as_str().unwrap();
    if case["algorithm"] == "pagerank" {
        let output = rank_output(read);
        assert_eq!(output.iterations, step, "{name}");
        check_scores(&output.scores, &entry["scores"], name);
        let residual = entry["residual"].as_f64().unwrap();
        assert!(
            (output.residual - residual).abs() <= 1e-14,
            "{name} step {step}"
        );
    } else {
        assert_eq!(community_output(read).sweeps, step, "{name}");
        assert_eq!(
            community_rows(read),
            canonical(entry, &|id| id),
            "{name} sweep {step}"
        );
    }
}

/// The case's result, stopped by `reason` if any.
fn check_result(case: &Value, read: &GraphAlgorithmRead, reason: Option<GraphAlgorithmTruncation>) {
    let name = case["name"].as_str().unwrap();
    let result = &case["expected"]["result"];
    assert_eq!(read.truncation, Vec::from_iter(reason), "{name}");
    assert_eq!(
        read.complete,
        result["complete"].as_bool().unwrap(),
        "{name}"
    );
    assert!(read.scope_complete, "{name}");
    assert_eq!(read.computation_complete, read.complete, "{name}");
    // A partial output keeps its provenance: the echo comes with any output.
    assert_eq!(read.request.is_some(), read.output.is_some(), "{name}");
    let converged = result["converged"].as_bool().unwrap();
    if case["algorithm"] == "pagerank" {
        if result["scores"].is_null() {
            assert_eq!(read.output, None, "{name}");
            assert_eq!(result["completed_iterations"], 0);
            return;
        }
        let output = rank_output(read);
        check_scores(&output.scores, &result["scores"], name);
        let residual = result["residual"].as_f64().unwrap();
        assert!((output.residual - residual).abs() <= 1e-14, "{name}");
        assert_eq!(
            (output.iterations, output.converged),
            (result["completed_iterations"].as_u64().unwrap(), converged),
            "{name}"
        );
    } else {
        if result["assignments"].is_null() {
            assert_eq!(read.output, None, "{name}");
            assert_eq!(result["completed_sweeps"], 0);
            return;
        }
        let rows = &result["assignments"];
        let expected: Vec<(u64, u64)> = rows
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row["node_id"].as_u64().unwrap(),
                    row["community_id"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(canonical(rows, &|id| id), expected, "{name}");
        assert_eq!(community_rows(read), expected, "{name}");
        let output = community_output(read);
        assert_eq!(
            (output.sweeps, output.converged),
            (result["completed_sweeps"].as_u64().unwrap(), converged),
            "{name}"
        );
    }
}

#[test]
fn finite_step_goldens_match_every_full_step() {
    let clock = || 0;
    let (mut steps, mut interrupted, mut boundaries) = (0, 0, 0);
    for case in &iterative_cases() {
        let name = case["name"].as_str().unwrap();
        let ranked = case["algorithm"] == "pagerank";
        let state = trace_graph(case);
        let graph = state.graph(G).unwrap();
        let (request, options) = trace_request(case);
        let at = |options: &GraphAlgorithmOptions<'_>| {
            run(graph, 5, 0, &request, options, &clock).unwrap()
        };
        let expected = &case["expected"];
        let trace = expected["trace"].as_array().unwrap();
        let count = ids(&case["nodes"]).len() as u64;

        // The scope is the whole fixture projection.
        let first = at(&capped(&options, 1));
        assert_eq!(first.stats.nodes_extracted, count, "{name}");
        assert_eq!(
            first.stats.edges_extracted,
            case["edges"].as_array().unwrap().len() as u64,
            "{name}"
        );

        // A cap of k returns exactly full step k. A PageRank step charges
        // one vertex unit per node twice (the dangling pass, then each
        // node's update); a sweep charges one per node update.
        let per_step = if ranked { 2 * count } else { count };
        let mut spent = vec![first.stats.vertex_operations - per_step];
        for (k, entry) in (1..).zip(trace) {
            let read = at(&capped(&options, k));
            check_step(case, &read, entry, k);
            spent.push(read.stats.vertex_operations);
            steps += 1;
        }
        if trace.is_empty() {
            spent.push(first.stats.vertex_operations);
        }
        assert!(
            spent.windows(2).all(|pair| pair[1] - pair[0] == per_step),
            "{name}: {spent:?}"
        );

        let reason = expected["result"]["reason"].as_str();
        match reason {
            None => check_result(case, &at(&options), None),
            Some("iteration_limit") => check_result(
                case,
                &at(&options),
                Some(GraphAlgorithmTruncation::IterationLimit),
            ),
            Some("synthetic_interruption") => {}
            Some(other) => panic!("unknown reason {other}"),
        }

        // A synthetic interruption after `budget` candidate updates: k full
        // steps, then j updates of step k + 1 (after its dangling pass).
        // The same stop in the engine is a vertex budget.
        let Some(budget) = case["synthetic_vertex_update_budget"].as_u64() else {
            continue;
        };
        let (k, j) = (budget / count, budget % count);
        let limited = |max_vertex_operations| {
            let mut limited = options.clone();
            limited.algorithm_limits.max_vertex_operations = max_vertex_operations;
            at(&limited)
        };
        if reason == Some("synthetic_interruption") {
            let dangling = if ranked { count } else { 0 };
            let read = limited(spent[k as usize] + dangling + j);
            check_result(case, &read, Some(GraphAlgorithmTruncation::VertexLimit));
            interrupted += 1;
        } else {
            // The budget ends exactly with the converging step; one unit
            // less returns the step before, or nothing.
            assert_eq!(j, 0, "{name}");
            check_result(case, &limited(spent[k as usize]), None);
            let short = limited(spent[k as usize] - 1);
            assert_eq!(
                short.truncation,
                vec![GraphAlgorithmTruncation::VertexLimit]
            );
            match k {
                1 => assert_eq!((short.output, short.request), (None, None)),
                _ => check_step(case, &short, &trace[k as usize - 2], k - 1),
            }
            boundaries += 1;
        }
    }
    assert_eq!((steps, interrupted, boundaries), (17, 4, 2));
}

fn rank(
    seeds: Vec<NodeSelector>,
    personalization: Option<Vec<(NodeSelector, f64)>>,
) -> GraphAlgorithmRequest {
    GraphAlgorithmRequest {
        scope: scope(seeds, Direction::Out),
        algorithm: GraphAlgorithm::PageRank {
            damping: None,
            tolerance: None,
            personalization,
            weight_property: Some("weight".into()),
        },
    }
}

fn propagate(seeds: Vec<NodeSelector>) -> GraphAlgorithmRequest {
    GraphAlgorithmRequest {
        scope: scope(seeds, Direction::Out),
        algorithm: GraphAlgorithm::LabelPropagation,
    }
}

fn sweeps(max_hops: u64, max_iterations: u64) -> GraphAlgorithmOptions<'static> {
    capped(&hops(max_hops), max_iterations)
}

#[test]
fn requests_echo_effective_parameters_and_reads_do_not_mutate() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("graph.vlite");
    let mut db = Database::create(&file, 2).unwrap();
    let (n, _) = small(&mut db);
    db.close().unwrap();
    let bytes = std::fs::read(&file).unwrap();

    let db = Database::open_read_only(&file).unwrap();
    let graph = db.graph("g").unwrap().with_clock(crate::current_time_ms());
    let state = graph.graph.clone();
    let id = |key: &str| NodeSelector::Id(n[key]);

    // Defaults become their effective values; personalization is listed
    // by node ID with its raw masses.
    let request = rank(
        vec![key("b"), key("a"), id("a")],
        Some(vec![(key("b"), 3.0), (id("a"), 1.0)]),
    );
    let first = graph.algorithm(&request, &hops(2)).unwrap();
    assert!(first.output.is_some());
    let echo = GraphAlgorithmRequest {
        scope: scope(vec![id("a"), id("b")], Direction::Out),
        algorithm: GraphAlgorithm::PageRank {
            damping: Some(0.85),
            tolerance: Some(1e-8),
            personalization: Some(vec![(id("a"), 1.0), (id("b"), 3.0)]),
            weight_property: Some("weight".into()),
        },
    };
    assert_eq!(first.request.as_ref(), Some(&echo));
    // Explicit defaults and a repeat give the same read; the echo, which
    // names fewer seeds and so does less normalization work, reproduces
    // the output and the echo itself.
    let mut explicit = request.clone();
    explicit.algorithm = GraphAlgorithm::PageRank {
        damping: Some(0.85),
        tolerance: Some(1e-8),
        personalization: Some(vec![(key("b"), 3.0), (id("a"), 1.0)]),
        weight_property: Some("weight".into()),
    };
    for again in [&explicit, &request] {
        let mut read = graph.algorithm(again, &hops(2)).unwrap();
        read.stats.elapsed_ms = first.stats.elapsed_ms;
        assert_eq!(read, first);
    }
    let again = graph.algorithm(&echo, &hops(2)).unwrap();
    assert_eq!(
        (&again.output, &again.request, &again.truncation),
        (&first.output, &first.request, &first.truncation)
    );

    // Label propagation echoes its normalized scope.
    let communities = graph
        .algorithm(&propagate(vec![key("c"), key("a")]), &hops(2))
        .unwrap();
    assert!(communities.complete);
    assert_eq!(communities.request, Some(propagate(vec![id("a"), id("c")])));
    assert_eq!(
        community_rows(&communities),
        vec![
            (n["a"], n["a"]),
            (n["b"], n["a"]),
            (n["c"], n["a"]),
            (n["d"], n["a"])
        ]
    );
    assert_eq!(
        (communities.read_time_unix_ms, communities.max_hops),
        (graph.now(), 2)
    );
    assert_eq!(communities.algorithm_version, 1);

    assert_eq!(graph.graph, &state);
    drop(db);
    assert_eq!(std::fs::read(&file).unwrap(), bytes);
}

#[test]
fn partial_results_carry_their_provenance() {
    let mut db = memory();
    small(&mut db);
    let graph = db.graph("g").unwrap().with_clock(crate::current_time_ms());
    for request in [rank(vec![key("a")], None), propagate(vec![key("a")])] {
        let full = graph.algorithm(&request, &sweeps(2, 100)).unwrap();
        assert!(full.output.is_some() && full.request.is_some());

        // A cap: the last full step, incomplete, with the same echo.
        let one = graph.algorithm(&request, &sweeps(2, 1)).unwrap();
        assert_eq!(
            one.truncation,
            vec![GraphAlgorithmTruncation::IterationLimit]
        );
        assert!(!one.complete && one.scope_complete && !one.computation_complete);
        assert!(one.output.is_some());
        assert_eq!(one.request, full.request);

        // A stop inside the first step: neither output nor echo.
        let mut options = sweeps(2, 1);
        options.algorithm_limits.max_vertex_operations = one.stats.vertex_operations - 1;
        let read = graph.algorithm(&request, &options).unwrap();
        assert_eq!(read.truncation, vec![GraphAlgorithmTruncation::VertexLimit]);
        assert!(read.scope_complete && !read.computation_complete);
        assert_eq!((read.output, read.request), (None, None));

        // A stop inside the second step: the first step, echoed.
        let two = graph.algorithm(&request, &sweeps(2, 2)).unwrap();
        let mut options = sweeps(2, 2);
        options.algorithm_limits.max_vertex_operations = two.stats.vertex_operations - 1;
        let read = graph.algorithm(&request, &options).unwrap();
        assert_eq!(read.truncation, vec![GraphAlgorithmTruncation::VertexLimit]);
        assert_eq!((&read.output, &read.request), (&one.output, &one.request));

        // A pre-set cancellation stops before any work.
        let flag = AtomicBool::new(true);
        let options = GraphAlgorithmOptions {
            cancel: Some(&flag),
            ..sweeps(2, 100)
        };
        let read = graph.algorithm(&request, &options).unwrap();
        assert_eq!(read.truncation, vec![GraphAlgorithmTruncation::Cancelled]);
        assert_eq!((read.output, read.request), (None, None));
        assert_eq!(read.stats.vertex_operations + read.stats.edges_examined, 0);
    }

    // Output caps cut rows; the first reason wins.
    let mut options = sweeps(2, 100);
    options.limits.max_output_nodes = 1;
    let cut = graph
        .algorithm(&propagate(vec![key("a")]), &options)
        .unwrap();
    assert_eq!(cut.truncation, vec![GraphAlgorithmTruncation::OutputLimit]);
    assert!(cut.scope_complete && cut.computation_complete && cut.request.is_some());
    assert_eq!(community_output(&cut).assignments.len(), 1);
    assert_eq!(cut.stats.nodes_extracted, 4);
    options.algorithm_limits.max_iterations = 1;
    let cut = graph
        .algorithm(&rank(vec![key("a")], None), &options)
        .unwrap();
    assert_eq!(
        cut.truncation,
        vec![GraphAlgorithmTruncation::IterationLimit]
    );
    assert_eq!(rank_output(&cut).scores.len(), 1);
}

#[test]
fn iterative_errors_are_typed() {
    let mut db = memory();
    let (n, _) = small(&mut db);
    build(&mut db, "h", &[("only_h", &[])], &[]);
    let (_, e) = build(
        &mut db,
        "w",
        &[("p", &[]), ("q", &[])],
        &[("pq", "p", "R", "q", None)],
    );
    let graph = db.graph("g").unwrap();
    let page = |damping, tolerance, personalization, weight: Option<&str>| GraphAlgorithmRequest {
        scope: scope(vec![key("a"), key("b")], Direction::Out),
        algorithm: GraphAlgorithm::PageRank {
            damping,
            tolerance,
            personalization,
            weight_property: weight.map(str::to_owned),
        },
    };
    let attempt = |request: &GraphAlgorithmRequest| graph.algorithm(request, &hops(2));
    let masses = |entries: Vec<(NodeSelector, f64)>| page(None, None, Some(entries), None);

    // Shape: before any work.
    for damping in [0.0, 1.0, -0.5, f64::NAN, f64::INFINITY] {
        assert_eq!(
            invalid_query(attempt(&page(Some(damping), None, None, None))),
            "damping must be finite and strictly between 0 and 1"
        );
    }
    for tolerance in [0.0, 1.5, f64::NAN, f64::NEG_INFINITY] {
        assert_eq!(
            invalid_query(attempt(&page(None, Some(tolerance), None, None))),
            "tolerance must be finite and strictly between 0 and 1"
        );
    }
    assert_eq!(
        invalid_query(attempt(&masses(vec![]))),
        "personalization must not be empty"
    );
    let crowd = masses(vec![(key("a"), 1.0); 10_001]);
    assert!(matches!(
        read_error(attempt(&crowd)),
        Error::GraphLimitExceedsCeiling {
            limit: "personalization",
            requested: 10_001,
            ceiling: 10_000
        }
    ));
    assert_eq!(
        invalid_query(attempt(&page(None, None, None, Some("")))),
        "a weight property name must have 1..=256 bytes"
    );
    for max_iterations in [0, 101] {
        for request in [rank(vec![key("a")], None), propagate(vec![key("a")])] {
            let result = graph.algorithm(&request, &sweeps(2, max_iterations));
            match max_iterations {
                0 => assert_eq!(invalid_query(result), "max_iterations must be at least 1"),
                _ => assert!(matches!(
                    read_error(result),
                    Error::GraphLimitExceedsCeiling {
                        limit: "max_iterations",
                        requested: 101,
                        ceiling: 100
                    }
                )),
            }
        }
    }

    // Entries: resolved, checked and summed under the meter, in order.
    let flag = AtomicBool::new(true);
    let cancelled = GraphAlgorithmOptions {
        cancel: Some(&flag),
        ..hops(2)
    };
    let mut many = vec![(key("a"), 1.0); 9_999];
    many.push((key("zz"), 1.0));
    let many = masses(many);
    let read = graph.algorithm(&many, &cancelled).unwrap();
    assert_eq!(read.truncation, vec![GraphAlgorithmTruncation::Cancelled]);
    for missing in [key("zz"), key("only_h"), NodeSelector::Id(999)] {
        assert!(matches!(
            read_error(attempt(&masses(vec![(key("a"), 1.0), (missing, 1.0)]))),
            Error::GraphNodeNotFound
        ));
    }
    for mass in [0.0, -0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert_eq!(
            invalid_query(attempt(&masses(vec![(key("a"), mass)]))),
            "personalization masses must be positive and finite"
        );
    }
    assert_eq!(
        invalid_query(attempt(&masses(vec![(key("c"), 1.0)]))),
        "personalization nodes must be seeds"
    );
    assert_eq!(
        invalid_query(attempt(&masses(vec![
            (key("a"), 1.0),
            (NodeSelector::Id(n["a"]), 2.0)
        ]))),
        "personalization must not repeat a node"
    );
    let overflow = attempt(&masses(vec![(key("a"), f64::MAX), (key("b"), f64::MAX)]));
    assert!(matches!(
        overflow,
        Err(GraphAlgorithmError::NumericOverflow)
    ));
    assert_eq!(
        GraphAlgorithmError::NumericOverflow.to_string(),
        "a numeric result exceeds the largest finite f64"
    );

    // Weights: every retained edge; label propagation reads none.
    let other = db.graph("w").unwrap();
    let weighted = rank(vec![key("p")], None);
    assert!(matches!(
        other.algorithm(&weighted, &hops(1)),
        Err(GraphAlgorithmError::MissingWeight { edge_id }) if edge_id == e["pq"]
    ));
    let read = other
        .algorithm(&propagate(vec![key("p")]), &hops(1))
        .unwrap();
    assert!(read.complete);
}

#[test]
fn the_echo_and_every_step_share_one_budget() {
    let mut db = memory();
    small(&mut db);
    let graph = db.graph("g").unwrap().with_clock(crate::current_time_ms());
    for request in [
        rank(vec![key("a"), key("b")], Some(vec![(key("b"), 1.0)])),
        propagate(vec![key("a")]),
    ] {
        let options = sweeps(2, 2);
        let read = graph.algorithm(&request, &options).unwrap();
        assert!(read.output.is_some());
        // The echo is staged under the meter before the first step; the
        // rows are the engine's, moved without a copy.
        let engine = engine_stats(&graph, &request, &options);
        assert_eq!(read.stats.edges_examined, engine.edges_examined);
        assert!(read.stats.vertex_operations > engine.vertex_operations);
        assert!(read.stats.scratch_bytes > engine.scratch_bytes);
        // Every buffer, the echo's included, is reserved before the first
        // step: a byte short returns nothing.
        let mut short = options.clone();
        short.algorithm_limits.max_scratch_bytes = read.stats.scratch_bytes - 1;
        let stopped = graph.algorithm(&request, &short).unwrap();
        assert_eq!(
            stopped.truncation,
            vec![GraphAlgorithmTruncation::MemoryLimit]
        );
        assert_eq!((stopped.output, stopped.request), (None, None));
        // Steps reserve nothing more.
        let one = graph.algorithm(&request, &sweeps(2, 1)).unwrap();
        assert_eq!(one.stats.scratch_bytes, read.stats.scratch_bytes);
    }
}

/// The request echo is copied under the meter: a stop injected while seed or
/// personalization entries are being copied lands within 64 copies and
/// returns neither output nor echo.
#[test]
fn echo_copies_are_checked_and_return_nothing() {
    use super::super::ECHOED;
    let echoed = || ECHOED.with(Cell::get);
    let mut db = memory();
    db.create_graph("p").unwrap();
    // Scrambled keys, so the echo is the sorted, resolved seed list.
    let names: Vec<String> = (0..300).map(|at| format!("n{}", at * 7 % 300)).collect();
    for name in &names {
        db.graph_mut("p")
            .unwrap()
            .create_node(name, &[], &Metadata::new())
            .unwrap();
    }
    let graph = db.graph("p").unwrap().with_clock(crate::current_time_ms());
    let seeds = || names.iter().map(|name| key(name)).collect::<Vec<_>>();
    let masses = names.iter().map(|name| (key(name), 1.0)).collect();
    // 300 seeds to echo, and for PageRank 300 personalization entries.
    let requests = [
        ("PageRank", rank(seeds(), Some(masses))),
        ("LabelPropagation", propagate(seeds())),
        ("Degree", degree(scope(seeds(), Direction::Out))),
    ];
    for (name, request) in requests {
        let mut options = sweeps(0, 100);
        options.limits.deadline_ms = 10;
        ECHOED.with(|count| count.set(0));
        let full = graph
            .algorithm_with_clock(&request, &options, &|| 0)
            .unwrap();
        assert!(full.computation_complete && full.request.is_some());
        let total = echoed();
        assert!(total >= 300, "{total}");
        let mut points: BTreeSet<u64> = (1..=total).step_by(23).collect();
        points.extend([1, 2, 64, 65, 301, total]);
        points.retain(|&point| point <= total);
        let mut unchecked = Vec::new();
        for inject in points {
            for cancel in [false, true] {
                // The deadline passes, or the flag is set, once `inject`
                // entries are copied; the clock is read only by checks.
                let flag = AtomicBool::new(false);
                let clock = || {
                    if echoed() < inject {
                        return 0;
                    }
                    if cancel {
                        flag.store(true, std::sync::atomic::Ordering::Relaxed);
                        0
                    } else {
                        1_000
                    }
                };
                let options = GraphAlgorithmOptions {
                    cancel: Some(&flag),
                    ..options.clone()
                };
                ECHOED.with(|count| count.set(0));
                let read = graph
                    .algorithm_with_clock(&request, &options, &clock)
                    .unwrap();
                let after = echoed() - inject;
                if after > 64 {
                    unchecked.push((inject, after));
                }
                let reason = if cancel {
                    GraphAlgorithmTruncation::Cancelled
                } else {
                    GraphAlgorithmTruncation::Deadline
                };
                assert_eq!(read.truncation, vec![reason]);
                assert_eq!((read.output, read.request), (None, None));
                assert!(!read.complete && !read.computation_complete);
            }
        }
        assert!(
            unchecked.is_empty(),
            "{name}: {} stops ran over 64 echo copies unchecked (point, copies): {:?}",
            unchecked.len(),
            &unchecked[..unchecked.len().min(4)]
        );
    }
}
