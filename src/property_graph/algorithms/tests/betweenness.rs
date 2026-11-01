//! Adapter tests of sampled betweenness (Gap 5 S4): the 4 golden
//! betweenness cases of `golden.json` on exact IDs and through a database
//! file, the request echo, typed errors, partial-result provenance and
//! non-mutation.
//!
//! The 23 cases of the independent oracle fixture `betweenness-cases.json`
//! (exact fractions from `tests/graph_betweenness_oracle.py`) are read on
//! their sparse node IDs at a TTL clock that hides their expired nodes and
//! edges: 19 directly, `max_seed_path` (node ID `u64::MAX`, which the
//! store cannot hold) on order- and hash-preserving mapped IDs, and the 3
//! cases outside the public domain (`samples` 0, no visible node) are shown
//! rejected. Scores match the once-rounded fractions within a relative
//! 1e-12, and their raw sums separately check the normalization. The
//! oracle's interruptions are vertex-budget stops, swept over every budget.

use super::super::super::engine::GraphError;
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

/// Checks `read` against a `golden.json` betweenness case whose node `id`
/// is `node(id)` here: the scope node and edge counts, every score within
/// `tolerance`,
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

// The independent betweenness oracle: `betweenness-cases.json`, checked by
// `tests/graph_betweenness_oracle.py` with exact fractions and no Rust.

const ORACLE: &str =
    include_str!("../../../../tests/fixtures/graph_algorithms/betweenness-cases.json");

/// The TTL clock of oracle reads; expired fixture nodes and edges are due
/// at it.
const ORACLE_NOW: u64 = 1_000;

/// A score matches the oracle's exact fraction rounded once to binary64
/// within this tolerance relative to the larger of 1 and that value; the
/// engine rounds at every step, so equal bits are not required. A zero
/// fraction must be exactly 0.
const RELATIVE: f64 = 1e-12;

/// XOR with the top bit maps IDs of at least 2^63 below `u64::MAX`, keeping
/// their order and, with the seed mapped alike, every `seed XOR id`.
const TOP: u64 = 1 << 63;

fn oracle_cases() -> Vec<Value> {
    let fixture: Value = serde_json::from_str(ORACLE).unwrap();
    assert_eq!(fixture["schema"], "vlite.graph-betweenness-golden");
    assert_eq!(fixture["version"], 1);
    let cases = fixture["cases"].as_array().unwrap().clone();
    assert_eq!(cases.len(), 23);
    cases
}

fn number(value: &Value) -> u64 {
    value.as_u64().unwrap()
}

fn expired(item: &Value) -> bool {
    item.get("expired")
        .is_some_and(|flag| flag.as_bool().unwrap())
}

fn oracle_direction(case: &Value) -> Direction {
    match case["direction"].as_str().unwrap() {
        "Out" => Direction::Out,
        "In" => Direction::In,
        "Both" => Direction::Both,
        other => panic!("unknown direction {other}"),
    }
}

/// A case's graph on its node IDs XOR `mask`, read at `ORACLE_NOW`: nodes
/// keyed `n{id}` and edges of type `R`, each put in ascending ID order (the
/// only order the store accepts), the expired ones due at the clock. The
/// store cannot hold ID `u64::MAX` (see
/// `oracle_ids_outside_the_store_are_rejected`), so an edge with it, always
/// the last, takes the next free ID: betweenness reports no edge IDs.
/// Returns the graph, the visible nodes in fixture order and the number of
/// visible edges.
fn oracle_graph(case: &Value, mask: u64) -> (GraphState, Vec<u64>, u64) {
    let mut state = GraphState::new();
    let create = GraphBody {
        graph_id: G,
        entries: vec![GraphEntry::CreateGraph { name: "g".into() }],
    };
    state.apply(1, &create).unwrap();
    let mut nodes: Vec<(u64, bool)> = case["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| (number(&node["id"]) ^ mask, expired(node)))
        .collect();
    let visible: Vec<u64> = nodes
        .iter()
        .filter(|node| !node.1)
        .map(|node| node.0)
        .collect();
    nodes.sort_unstable();
    let mut edges: Vec<(u64, u64, u64, bool)> = case["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|edge| {
            (
                number(&edge["id"]),
                number(&edge["source"]) ^ mask,
                number(&edge["target"]) ^ mask,
                expired(edge),
            )
        })
        .collect();
    edges.sort_unstable();
    let mut entries: Vec<GraphEntry> = nodes
        .iter()
        .map(|&(id, _)| GraphEntry::PutNode {
            node_id: NodeId(id),
            key: format!("n{id}"),
            labels: BTreeSet::new(),
            properties: Properties::new(),
        })
        .collect();
    let mut expiries: Vec<GraphEntry> = nodes
        .iter()
        .filter(|node| node.1)
        .map(|&(id, _)| GraphEntry::NodeExpiry {
            node_id: NodeId(id),
            expires_at_ms: Some(ORACLE_NOW),
        })
        .collect();
    let (mut next, mut shown) = (0, 0);
    for (id, source, target, gone) in edges {
        let id = if id == u64::MAX { next } else { id };
        next = id + 1;
        entries.push(GraphEntry::PutEdge {
            edge_id: EdgeId(id),
            source: NodeId(source),
            destination: NodeId(target),
            relationship: "R".into(),
            key: None,
            properties: Properties::new(),
        });
        if gone {
            expiries.push(GraphEntry::EdgeExpiry {
                edge_id: EdgeId(id),
                expires_at_ms: Some(ORACLE_NOW),
            });
        } else if visible.contains(&source) && visible.contains(&target) {
            shown += 1;
        }
    }
    entries.extend(expiries);
    let body = GraphBody {
        graph_id: G,
        entries,
    };
    state.apply(2, &body).unwrap();
    (state, visible, shown)
}

/// A read of `case` from `seeds` (in this order) at `ORACLE_NOW` within two
/// hops, with `samples` as `max_sampled_sources` and `max_vertex_operations`
/// `budget`, if any. Every scoped node is a seed, so the scope is exactly
/// the visible nodes and every edge between them.
fn oracle_read(
    graph: &Graph,
    case: &Value,
    (seeds, seed): (&[u64], u64),
    samples: u64,
    budget: Option<u64>,
) -> GraphAlgorithmRead {
    let selectors = seeds.iter().copied().map(NodeSelector::Id).collect();
    let request = between(selectors, oracle_direction(case), seed);
    let mut options = hops(2);
    options.algorithm_limits.max_sampled_sources = samples;
    if let Some(budget) = budget {
        options.algorithm_limits.max_vertex_operations = budget;
    }
    run(graph, 2, ORACLE_NOW, &request, &options, &|| 0).unwrap()
}

/// An oracle fraction `p/q` rounded once to binary64: both parts are below
/// 2^53 here, so the quotient is one correctly rounded division.
fn fraction(exact: &Value) -> f64 {
    let (p, q) = exact.as_str().unwrap().split_once('/').unwrap();
    let (p, q): (u64, u64) = (p.parse().unwrap(), q.parse().unwrap());
    assert!(p < 1 << 53 && 0 < q && q < 1 << 53, "{exact}");
    p as f64 / q as f64
}

fn near(actual: f64, wanted: f64) -> bool {
    (actual - wanted).abs() <= RELATIVE * wanted.abs().max(1.0)
}

fn score_bits(output: &BetweennessOutput) -> Vec<(u64, u64)> {
    output
        .scores
        .iter()
        .map(|row| (row.node_id, row.score.to_bits()))
        .collect()
}

/// Checks a read of `case`, on its node IDs XOR `mask`, whose extraction
/// finished: the scope node count, `edges` visible edges, the status, the
/// selected and completed sources in rank order, k, rows ascending by ID,
/// and each score against the oracle's once-rounded fraction and against
/// its raw dependency sum normalized here as `(n / k) * sum / ((n - 1) *
/// (n - 2))`, both within [`RELATIVE`], a zero fraction exactly.
fn check_oracle(case: &Value, read: &GraphAlgorithmRead, mask: u64, edges: u64) {
    let name = case["id"].as_str().unwrap();
    let expected = &case["expected"];
    let (n, k) = (number(&expected["n"]), number(&expected["k"]));
    assert!(read.scope_complete, "{name}");
    assert_eq!(read.stats.nodes_extracted, n, "{name}");
    assert_eq!(read.stats.edges_extracted, edges, "{name}");
    let status = expected["status"].as_str().unwrap();
    if status == "no_scores" {
        assert_eq!(k, 0, "{name}");
        assert!(expected["scores"].is_null(), "{name}");
        assert!(ids(&expected["completed"]).is_empty(), "{name}");
        assert_eq!((&read.output, &read.request), (&None, &None), "{name}");
        assert_eq!(read.truncation.len(), 1, "{name}");
        assert!(!read.computation_complete, "{name}");
        return;
    }
    let output = sources(read);
    assert!(read.request.is_some(), "{name}");
    assert_eq!(
        unmask(&output.selected_sources, mask),
        ids(&expected["selected"]),
        "{name}"
    );
    assert_eq!(
        unmask(&output.completed_sources, mask),
        ids(&expected["completed"]),
        "{name}"
    );
    assert_eq!(output.completed_source_count, k, "{name}");
    match status {
        "exact" => {
            assert_eq!(k, n, "{name}");
            assert!(read.complete && read.computation_complete && !output.estimated);
        }
        "complete_estimated" => {
            assert!(read.complete && read.computation_complete && output.estimated);
        }
        "partial_estimated" => {
            assert_eq!(
                read.truncation,
                vec![GraphAlgorithmTruncation::VertexLimit],
                "{name}"
            );
            assert!(!read.complete && !read.computation_complete && output.estimated);
        }
        other => panic!("{name}: unknown status {other}"),
    }
    let rows = expected["scores"].as_array().unwrap();
    let sums = expected["raw_sums"].as_array().unwrap();
    assert_eq!((output.scores.len(), sums.len()), (rows.len(), rows.len()));
    assert!(
        output
            .scores
            .windows(2)
            .all(|pair| pair[0].node_id < pair[1].node_id),
        "{name}"
    );
    for ((actual, row), sum) in output.scores.iter().zip(rows).zip(sums) {
        let id = number(&row["node_id"]);
        assert_eq!((actual.node_id ^ mask, number(&sum["node_id"])), (id, id));
        let wanted: f64 = row["f64_repr"].as_str().unwrap().parse().unwrap();
        assert_eq!(
            wanted.to_bits(),
            fraction(&row["exact"]).to_bits(),
            "{name}"
        );
        let normalized = if n > 2 {
            n as f64 / k as f64 * fraction(&sum["exact"]) / ((n - 1) * (n - 2)) as f64
        } else {
            0.0
        };
        let score = actual.score;
        assert!(
            near(score, wanted) && near(score, normalized),
            "{name}: node {id} scored {score}, not {wanted} or {normalized}"
        );
        if wanted == 0.0 {
            assert_eq!(score, 0.0, "{name}: node {id}");
        }
    }
}

/// Runs `case` under every vertex budget below the full read's, which stops
/// it at that budget's next unit: in extraction, setup, or a source's
/// search, dependencies or commit. The completed count never drops, reaches
/// every proper prefix of the selection, and is 0 exactly when nothing is
/// returned. A later stop returns the full selection, the completed prefix,
/// the echo and, bit for bit, the scores of an uninterrupted read of that
/// many sources: the interrupted source is discarded entirely. The full
/// budget completes. Returns the stopped reads in budget order.
fn sweep(
    graph: &Graph,
    case: &Value,
    seeds: (&[u64], u64),
    samples: u64,
    full: &GraphAlgorithmRead,
) -> Vec<GraphAlgorithmRead> {
    let name = case["id"].as_str().unwrap();
    let total = full.stats.vertex_operations;
    let selected = &sources(full).selected_sources;
    let (mut reads, mut reached) = (Vec::new(), BTreeSet::new());
    for budget in 1..total {
        let read = oracle_read(graph, case, seeds, samples, Some(budget));
        assert_eq!(
            read.truncation,
            vec![GraphAlgorithmTruncation::VertexLimit],
            "{name} at {budget}"
        );
        assert!(!read.complete && !read.computation_complete);
        let k = completed(&read);
        if k == 0 {
            assert_eq!((&read.output, &read.request), (&None, &None));
        } else {
            let output = sources(&read);
            assert!(
                read.scope_complete && output.estimated,
                "{name} at {budget}"
            );
            assert_eq!(&output.selected_sources, selected, "{name} at {budget}");
            assert_eq!(output.completed_sources, selected[..k as usize]);
            assert_eq!(read.request, full.request);
            let prefix = oracle_read(graph, case, seeds, k, None);
            assert!(prefix.complete);
            assert_eq!(score_bits(output), score_bits(sources(&prefix)), "{name}");
        }
        assert!(
            k >= reached.last().copied().unwrap_or(0),
            "{name} at {budget}"
        );
        reached.insert(k);
        reads.push(read);
    }
    let prefixes: BTreeSet<u64> = (0..selected.len() as u64).collect();
    assert_eq!(reached, prefixes, "{name}");
    let read = oracle_read(graph, case, seeds, samples, Some(total));
    assert!(read.complete, "{name}");
    assert_eq!(read.output, full.output, "{name}");
    reads
}

fn completed(read: &GraphAlgorithmRead) -> u64 {
    read.output
        .as_ref()
        .map_or(0, |_| sources(read).completed_source_count)
}

/// `samples` 0 is outside the public domain: zero work limits fail rather
/// than clamp, so `max_sampled_sources` 0 is `InvalidQuery` before any
/// work, where the oracle answers no sources and no scores.
fn zero_samples_are_rejected(case: &Value, graph: &Graph, seeds: &[u64]) {
    let expected = &case["expected"];
    assert_eq!(expected["status"], "no_scores");
    assert_eq!(number(&expected["k"]), 0);
    assert!(expected["scores"].is_null());
    assert!(ids(&expected["selected"]).is_empty() && ids(&expected["completed"]).is_empty());
    assert_eq!(number(&expected["n"]), seeds.len() as u64);
    let selectors = seeds.iter().copied().map(NodeSelector::Id).collect();
    let request = between(selectors, oracle_direction(case), number(&case["seed"]));
    let mut options = hops(2);
    options.algorithm_limits.max_sampled_sources = 0;
    assert_eq!(
        invalid_query(run(graph, 2, ORACLE_NOW, &request, &options, &|| 0)),
        "max_sampled_sources must be at least 1"
    );
}

/// No visible node is outside the public domain: a scope needs a seed and
/// a due seed is not found, so no read has n = 0, where the oracle answers
/// no sources and no scores.
fn empty_scopes_are_rejected(case: &Value, graph: &Graph) {
    let expected = &case["expected"];
    assert_eq!(expected["status"], "no_scores");
    assert_eq!((number(&expected["n"]), number(&expected["k"])), (0, 0));
    assert!(expected["scores"].is_null());
    assert!(ids(&expected["selected"]).is_empty());
    let (direction, seed) = (oracle_direction(case), number(&case["seed"]));
    let read = |seeds| {
        run(
            graph,
            2,
            ORACLE_NOW,
            &between(seeds, direction, seed),
            &hops(2),
            &|| 0,
        )
    };
    for node in case["nodes"].as_array().unwrap() {
        assert!(expired(node));
        assert!(matches!(
            read_error(read(vec![NodeSelector::Id(number(&node["id"]))])),
            Error::GraphNodeNotFound
        ));
    }
    assert_eq!(
        invalid_query(read(Vec::new())),
        "an algorithm scope needs a seed"
    );
}

#[test]
fn oracle_splitmix64_vectors_match() {
    let fixture: Value = serde_json::from_str(ORACLE).unwrap();
    let vectors = fixture["splitmix64_vectors"].as_array().unwrap();
    assert_eq!(vectors.len(), 7);
    for vector in vectors {
        let hex = vector["output"]
            .as_str()
            .unwrap()
            .strip_prefix("0x")
            .unwrap();
        let input = number(&vector["input"]);
        assert_eq!(mix(input), u64::from_str_radix(hex, 16).unwrap(), "{input}");
    }
}

/// The store takes a node or edge ID only by moving its next free ID past
/// it, so `u64::MAX` is out of reach; the oracle cases using it are stored
/// as [`oracle_graph`] describes.
#[test]
fn oracle_ids_outside_the_store_are_rejected() {
    let put = |entries: Vec<GraphEntry>| {
        let mut state = GraphState::new();
        let create = GraphBody {
            graph_id: G,
            entries: vec![GraphEntry::CreateGraph { name: "g".into() }],
        };
        state.apply(1, &create).unwrap();
        state.apply(
            2,
            &GraphBody {
                graph_id: G,
                entries,
            },
        )
    };
    let node = |id| GraphEntry::PutNode {
        node_id: NodeId(id),
        key: format!("n{id}"),
        labels: BTreeSet::new(),
        properties: Properties::new(),
    };
    assert!(matches!(
        put(vec![node(u64::MAX)]),
        Err(GraphError::IdSpaceExhausted)
    ));
    let edge = GraphEntry::PutEdge {
        edge_id: EdgeId(u64::MAX),
        source: NodeId(0),
        destination: NodeId(1),
        relationship: "R".into(),
        key: None,
        properties: Properties::new(),
    };
    assert!(matches!(
        put(vec![node(0), node(1), edge]),
        Err(GraphError::IdSpaceExhausted)
    ));
    assert!(put(vec![node(u64::MAX - 1)]).is_ok());
}

/// Every oracle case: 19 read on their node IDs, one (`max_seed_path`, with
/// node ID `u64::MAX`) on IDs XOR 2^63, which keeps the order and every
/// `seed XOR id`, with the seed mapped alike; and three outside the public
/// domain shown rejected. Each read case also reads the same from its
/// seeds reversed, ranks its fixture selection by [`mix`] and passes
/// [`sweep`]. An interrupted case is checked at every budget that stops it
/// after its `interrupt_after_sources` completed sources.
#[test]
fn oracle_betweenness_cases_match() {
    let mut classes: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let mut stops = Vec::new();
    for case in oracle_cases() {
        let name = case["id"].as_str().unwrap().to_owned();
        let expected = &case["expected"];
        let (samples, n) = (number(&case["samples"]), number(&expected["n"]));
        let all = case["nodes"].as_array().unwrap().iter();
        let mask = if all.clone().any(|node| number(&node["id"]) == u64::MAX) {
            assert!(all.clone().all(|node| number(&node["id"]) >= TOP));
            TOP
        } else {
            0
        };
        let (state, seeds, edges) = oracle_graph(&case, mask);
        let graph = state.graph(G).unwrap();
        assert_eq!(seeds.len() as u64, n, "{name}");
        let class = if samples == 0 {
            zero_samples_are_rejected(&case, graph, &seeds);
            "rejected: samples 0"
        } else if n == 0 {
            empty_scopes_are_rejected(&case, graph);
            "rejected: no visible node"
        } else {
            let seed = number(&case["seed"]);
            let mut ranked: Vec<u64> = seeds.iter().map(|id| id ^ mask).collect();
            ranked.sort_by_key(|&id| (mix(seed ^ id), id));
            ranked.truncate(samples.min(n) as usize);
            assert_eq!(ranked, ids(&expected["selected"]), "{name}");

            let at = (&seeds[..], seed ^ mask);
            let full = oracle_read(graph, &case, at, samples, None);
            let reversed: Vec<u64> = seeds.iter().rev().copied().collect();
            let again = oracle_read(graph, &case, (&reversed, seed ^ mask), samples, None);
            assert_eq!(
                (&again.output, &again.request, &again.truncation),
                (&full.output, &full.request, &full.truncation),
                "{name}"
            );
            assert!(full.complete, "{name}");
            let reads = sweep(graph, &case, at, samples, &full);
            if let Some(after) = case.get("interrupt_after_sources") {
                let after = number(after);
                assert_eq!(
                    ids(&expected["selected"]),
                    unmask(&sources(&full).selected_sources, mask),
                    "{name}"
                );
                let mut count = 0;
                for read in reads {
                    if read.scope_complete && completed(&read) == after {
                        check_oracle(&case, &read, mask, edges);
                        count += 1;
                    }
                }
                assert!(count > 0, "{name}");
                stops.push((name.clone(), after, count));
            } else {
                check_oracle(&case, &full, mask, edges);
            }
            if mask == 0 {
                "read"
            } else {
                "read on mapped IDs"
            }
        };
        classes.entry(class).or_default().push(name);
    }
    let counts: Vec<(&str, usize)> = classes
        .iter()
        .map(|(class, names)| (*class, names.len()))
        .collect();
    assert_eq!(
        counts,
        [
            ("read", 19),
            ("read on mapped IDs", 1),
            ("rejected: no visible node", 1),
            ("rejected: samples 0", 2),
        ]
    );
    assert_eq!(classes["read on mapped IDs"], ["max_seed_path"]);
    assert_eq!(
        classes["rejected: no visible node"],
        ["empty_visible_graph"]
    );
    assert_eq!(
        classes["rejected: samples 0"],
        ["zero_requested_samples", "one_node_no_sources"]
    );
    let interrupted: Vec<(&str, u64)> = stops.iter().map(|stop| (&*stop.0, stop.1)).collect();
    assert_eq!(
        interrupted,
        [("interrupted_path", 2), ("interrupted_before_first", 0)]
    );
    println!("interrupted oracle reads checked: {stops:?}");
}

fn unmask(list: &[u64], mask: u64) -> Vec<u64> {
    list.iter().map(|id| id ^ mask).collect()
}
