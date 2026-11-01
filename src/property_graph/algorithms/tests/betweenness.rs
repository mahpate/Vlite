//! Adapter tests of sampled betweenness (Gap 5 S4): the golden betweenness
//! cases on exact IDs and through a database file, the request echo, typed
//! errors, partial-result provenance and non-mutation.
//!
//! [`check_case`] compares one read with a case in the shape of the golden
//! betweenness cases, so a separately authored fixture of more cases can
//! reuse it.

use super::*;

fn sampled(sampling_seed: u64) -> GraphAlgorithm {
    GraphAlgorithm::Betweenness { sampling_seed }
}

fn between(seeds: Vec<NodeSelector>, direction: Direction, seed: u64) -> GraphAlgorithmRequest {
    GraphAlgorithmRequest {
        scope: scope(seeds, direction),
        algorithm: sampled(seed),
    }
}

fn sources(read: &GraphAlgorithmRead) -> &BetweennessOutput {
    match &read.output {
        Some(GraphAlgorithmOutput::Betweenness(output)) => output,
        other => panic!("no betweenness: {other:?}"),
    }
}

/// An independent SplitMix64 v1.
fn mix(input: u64) -> u64 {
    let mut x = input.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// The cases of `fixture` whose algorithm is betweenness.
fn betweenness_cases(fixture: Vec<Value>) -> Vec<Value> {
    fixture
        .into_iter()
        .filter(|case| case["algorithm"] == "betweenness")
        .collect()
}

/// The request and options of a betweenness case with nodes named by
/// `node`: its scope, `sampling_seed` and `samples` as
/// `max_sampled_sources`.
fn case_request(
    case: &Value,
    node: &dyn Fn(u64) -> NodeSelector,
) -> (GraphAlgorithmRequest, GraphAlgorithmOptions<'static>) {
    let mut request = fixture_request(case, node).0;
    let parameters = &case["parameters"];
    request.algorithm = sampled(parameters["sampling_seed"].as_u64().unwrap());
    let mut options = fixture_options(case);
    options.algorithm_limits.max_sampled_sources = parameters["samples"].as_u64().unwrap();
    (request, options)
}

/// Checks `read` against a betweenness case whose node `id` is `node(id)`
/// here: the scope node and edge counts, every score within `tolerance`,
/// the completed source count and the estimated flag. The sampled sources
/// are compared exactly when `node` keeps IDs, since sampling ranks by ID;
/// otherwise they must be the case's scope ranked by the mapped IDs.
fn check_case(case: &Value, read: &GraphAlgorithmRead, node: &dyn Fn(u64) -> u64, tolerance: f64) {
    let name = case["name"].as_str().unwrap();
    let expected = &case["expected"];
    let wanted = &expected["output"];
    assert!(
        read.complete && read.scope_complete && read.computation_complete,
        "{name}: {:?}",
        read.truncation
    );
    let scope_nodes = ids(&expected["scope_node_ids"]);
    assert_eq!(
        read.stats.nodes_extracted,
        scope_nodes.len() as u64,
        "{name}"
    );
    assert_eq!(
        read.stats.edges_extracted,
        ids(&expected["scope_edge_ids"]).len() as u64,
        "{name}"
    );
    let output = sources(read);
    let mut rows: Vec<(u64, f64)> = wanted["scores"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                node(row["node_id"].as_u64().unwrap()),
                row["score"].as_f64().unwrap(),
            )
        })
        .collect();
    rows.sort_by_key(|row| row.0);
    assert_eq!(output.scores.len(), rows.len(), "{name}");
    for (actual, (id, score)) in output.scores.iter().zip(&rows) {
        assert_eq!(actual.node_id, *id, "{name}");
        assert!(
            (actual.score - score).abs() <= tolerance,
            "{name}: node {id} scored {} not {score}",
            actual.score
        );
    }
    let k = wanted["completed_source_count"].as_u64().unwrap();
    assert_eq!(output.completed_source_count, k, "{name}");
    assert_eq!(
        output.estimated,
        wanted["estimated"].as_bool().unwrap(),
        "{name}"
    );
    let selected = ids(&wanted["selected_sources"]);
    let completed = ids(&wanted["completed_sources"]);
    assert_eq!(completed.len() as u64, k, "{name}");
    if scope_nodes.iter().all(|&id| node(id) == id) {
        assert_eq!(output.selected_sources, selected, "{name}");
        assert_eq!(output.completed_sources, completed, "{name}");
    } else {
        let mut ranked: Vec<u64> = scope_nodes.iter().map(|&id| node(id)).collect();
        ranked.sort_by_key(|&id| {
            (
                mix(case["parameters"]["sampling_seed"].as_u64().unwrap() ^ id),
                id,
            )
        });
        ranked.truncate(selected.len());
        assert_eq!(output.selected_sources, ranked, "{name}");
        assert_eq!(
            output.completed_sources,
            ranked[..k as usize].to_vec(),
            "{name}"
        );
    }
}

fn tolerance() -> f64 {
    let golden: Value = serde_json::from_str(GOLDEN).unwrap();
    golden["contract"]["score_abs_tolerance"].as_f64().unwrap()
}

#[test]
fn golden_betweenness_matches_the_oracle_on_exact_ids() {
    let clock = || 0;
    let cases = betweenness_cases(cases());
    assert_eq!(cases.len(), 4);
    for case in &cases {
        let state = exact_graph(case);
        let graph = state.graph(G).unwrap();
        let (request, options) = case_request(case, &NodeSelector::Id);
        let read = run(graph, 5, 0, &request, &options, &clock).unwrap();
        check_case(case, &read, &|id| id, tolerance());
    }
}

#[test]
fn golden_betweenness_through_the_public_api() {
    let dir = tempfile::tempdir().unwrap();
    let (db, maps) = golden_database(&dir.path().join("golden.vlite"));
    let mut checked = 0;
    for (case, (nodes, _)) in cases().iter().zip(&maps) {
        if case["algorithm"] != "betweenness" {
            continue;
        }
        let graph = db.graph(case["name"].as_str().unwrap()).unwrap();
        let (request, options) = case_request(case, &|id| NodeSelector::Id(nodes[&id]));
        let read = graph.algorithm(&request, &options).unwrap();
        assert_eq!(read.view_sequence, graph.view_sequence());
        check_case(case, &read, &|id| nodes[&id], tolerance());
        checked += 1;
    }
    assert_eq!(checked, 4);
}

#[test]
fn betweenness_echoes_its_seed_and_reads_do_not_mutate() {
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

    // Both ways, the triangle a, b, c with d off c and x off a (the loop
    // a→a ignored); two hops from a and c admit all five nodes.
    let request = between(vec![key("c"), key("a"), id("a")], Direction::Both, 7);
    let first = graph.algorithm(&request, &hops(2)).unwrap();
    assert!(first.complete && first.computation_complete);
    let echo = between(vec![id("a"), id("c")], Direction::Both, 7);
    assert_eq!(first.request.as_ref(), Some(&echo));
    let output = sources(&first);
    assert_eq!(output.completed_source_count, 5);
    assert!(!output.estimated);
    // c lies on the only shortest paths of {a, d}, {b, d} and {d, x}; a
    // on those of {b, x}, {c, x} and {d, x}: 6 ordered pairs each of
    // 4 * 3.
    let mut expected: Vec<(u64, f64)> = ["a", "b", "c", "d", "x"]
        .iter()
        .map(|node| (n[node], if matches!(*node, "a" | "c") { 0.5 } else { 0.0 }))
        .collect();
    expected.sort_by_key(|row| row.0);
    let actual: Vec<(u64, f64)> = output
        .scores
        .iter()
        .map(|row| (row.node_id, row.score))
        .collect();
    assert_eq!(actual.len(), expected.len());
    for (row, wanted) in actual.iter().zip(&expected) {
        assert_eq!(row.0, wanted.0);
        assert!((row.1 - wanted.1).abs() <= 1e-12, "{actual:?}");
    }
    let mut ranked = vec![n["a"], n["b"], n["c"], n["d"], n["x"]];
    ranked.sort_by_key(|&id| (mix(7 ^ id), id));
    assert_eq!(output.selected_sources, ranked);

    // A repeat and the echo give the same output.
    let mut again = graph.algorithm(&request, &hops(2)).unwrap();
    again.stats.elapsed_ms = first.stats.elapsed_ms;
    assert_eq!(again, first);
    let echoed = graph.algorithm(&echo, &hops(2)).unwrap();
    assert_eq!(
        (&echoed.output, &echoed.request, &echoed.truncation),
        (&first.output, &first.request, &first.truncation)
    );
    // Out from a admits a, b, c and d: a→c leaves b on no shortest path,
    // and c lies between a and d and between b and d, of 3 * 2 pairs.
    let out = graph
        .algorithm(&between(vec![key("a")], Direction::Out, 7), &hops(2))
        .unwrap();
    assert_eq!(out.stats.nodes_extracted, 4);
    let c = sources(&out)
        .scores
        .iter()
        .find(|row| row.node_id == n["c"])
        .unwrap();
    assert!((c.score - 2.0 / 6.0).abs() <= 1e-12);
    assert_eq!((first.read_time_unix_ms, first.max_hops), (graph.now(), 2));
    assert_eq!(first.algorithm_version, 1);

    assert_eq!(graph.graph, &state);
    drop(db);
    assert_eq!(std::fs::read(&file).unwrap(), bytes);
}

#[test]
fn betweenness_errors_are_typed() {
    let mut db = memory();
    small(&mut db);
    let graph = db.graph("g").unwrap();
    let request = between(vec![key("a")], Direction::Out, 0);
    let mut options = hops(2);
    options.algorithm_limits.max_sampled_sources = 65;
    assert!(matches!(
        read_error(graph.algorithm(&request, &options)),
        Error::GraphLimitExceedsCeiling {
            limit: "max_sampled_sources",
            requested: 65,
            ceiling: 64
        }
    ));
    options.algorithm_limits.max_sampled_sources = 0;
    assert_eq!(
        invalid_query(graph.algorithm(&request, &options)),
        "max_sampled_sources must be at least 1"
    );
    // Seeds are checked as for every algorithm.
    let missing = between(vec![key("nope")], Direction::Out, 0);
    assert!(matches!(
        read_error(graph.algorithm(&missing, &hops(2))),
        Error::GraphNodeNotFound
    ));
}

#[test]
fn partial_betweenness_carries_its_provenance() {
    let mut db = memory();
    small(&mut db);
    let graph = db.graph("g").unwrap().with_clock(crate::current_time_ms());
    let request = between(vec![key("a"), key("x")], Direction::Both, 3);
    let capped = |cap: u64| {
        let mut options = hops(2);
        options.algorithm_limits.max_sampled_sources = cap;
        options
    };
    let full = graph.algorithm(&request, &capped(16)).unwrap();
    assert!(full.complete);
    assert_eq!(sources(&full).completed_source_count, 5);

    // A smaller sample is complete but estimated, with the same echo.
    let one = graph.algorithm(&request, &capped(1)).unwrap();
    assert!(one.complete && one.computation_complete);
    assert!(sources(&one).estimated);
    assert_eq!(one.request, full.request);

    // A stop inside the first source: neither output nor echo. A cap of 16
    // copies four more selected sources than a cap of 1 before it.
    let mut options = capped(16);
    options.algorithm_limits.max_vertex_operations = one.stats.vertex_operations + 4 - 1;
    let read = graph.algorithm(&request, &options).unwrap();
    assert_eq!(read.truncation, vec![GraphAlgorithmTruncation::VertexLimit]);
    assert!(read.scope_complete && !read.computation_complete);
    assert_eq!((read.output, read.request), (None, None));

    // A stop inside the last source: the four before it, estimated, echoed.
    let mut options = capped(16);
    options.algorithm_limits.max_vertex_operations = full.stats.vertex_operations - 1;
    let read = graph.algorithm(&request, &options).unwrap();
    assert_eq!(read.truncation, vec![GraphAlgorithmTruncation::VertexLimit]);
    assert!(!read.complete && read.scope_complete && !read.computation_complete);
    assert_eq!(read.request, full.request);
    let four = graph.algorithm(&request, &capped(4)).unwrap();
    let output = sources(&read);
    assert_eq!(output.completed_source_count, 4);
    assert!(output.estimated);
    assert_eq!(output.selected_sources, sources(&full).selected_sources);
    assert_eq!(output.scores, sources(&four).scores);

    // Too little scratch fails before the first source.
    let mut options = capped(16);
    options.algorithm_limits.max_scratch_bytes = full.stats.scratch_bytes - 1;
    let read = graph.algorithm(&request, &options).unwrap();
    assert_eq!(read.truncation, vec![GraphAlgorithmTruncation::MemoryLimit]);
    assert_eq!((read.output, read.request), (None, None));

    // A pre-set cancellation stops before any work.
    let flag = AtomicBool::new(true);
    let options = GraphAlgorithmOptions {
        cancel: Some(&flag),
        ..capped(16)
    };
    let read = graph.algorithm(&request, &options).unwrap();
    assert_eq!(read.truncation, vec![GraphAlgorithmTruncation::Cancelled]);
    assert_eq!((read.output, read.request), (None, None));
    assert_eq!(read.stats.vertex_operations + read.stats.edges_examined, 0);

    // Output caps cut rows, never the sources.
    let mut options = capped(16);
    options.limits.max_output_nodes = 2;
    let cut = graph.algorithm(&request, &options).unwrap();
    assert_eq!(cut.truncation, vec![GraphAlgorithmTruncation::OutputLimit]);
    assert!(cut.computation_complete && cut.request.is_some());
    assert_eq!(sources(&cut).scores, sources(&full).scores[..2].to_vec());
    assert_eq!(
        sources(&cut).selected_sources,
        sources(&full).selected_sources
    );
}

/// Betweenness stages its echo before its first source, under the same
/// checked copies as every algorithm: a stop injected while the seed list
/// is copied lands within 64 copies and returns neither output nor echo.
#[test]
fn betweenness_echo_copies_are_checked_and_return_nothing() {
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
    let request = between(
        names.iter().map(|name| key(name)).collect(),
        Direction::Out,
        11,
    );
    let mut options = hops(0);
    options.limits.deadline_ms = 10;
    ECHOED.with(|count| count.set(0));
    let full = graph
        .algorithm_with_clock(&request, &options, &|| 0)
        .unwrap();
    assert!(full.computation_complete && full.request.is_some());
    let total = echoed();
    assert_eq!(total, 300);
    let mut points: BTreeSet<u64> = (1..=total).step_by(23).collect();
    points.extend([1, 2, 64, 65, total]);
    for inject in points {
        for cancel in [false, true] {
            // The deadline passes, or the flag is set, once `inject` seeds
            // are copied; the clock is read only by checks.
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
            assert!(after <= 64, "{after} echo copies ran unchecked at {inject}");
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
}
