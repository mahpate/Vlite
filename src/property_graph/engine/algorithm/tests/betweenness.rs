//! Engine tests of sampled betweenness (Gap 5 S4): hand-checked Brandes
//! scores, SplitMix64 sampling, a definitional oracle over random scopes,
//! sampled scaling, the documented accounting, every budget at its
//! boundary, the deadline and cancellation at points spread over the run,
//! output caps, the TTL clock and numeric overflow.

use super::super::SORTED;
use super::super::betweenness::{WORKED, splitmix64};
use super::*;

fn sampled(sampling_seed: u64) -> Algorithm {
    Algorithm::Betweenness { sampling_seed }
}

fn samples(max_sampled_sources: u64) -> GraphAlgorithmLimits {
    GraphAlgorithmLimits {
        max_sampled_sources,
        ..CAPS
    }
}

fn sources(result: &AlgorithmResult) -> &BetweennessOutput {
    match &result.output {
        Some(AlgorithmOutput::Betweenness(output)) => output,
        other => panic!("no betweenness: {other:?}"),
    }
}

fn scores(result: &AlgorithmResult) -> Vec<(u64, f64)> {
    sources(result)
        .scores
        .iter()
        .map(|row| (row.node_id, row.score))
        .collect()
}

/// An independent SplitMix64 v1 for the oracles.
fn mix(input: u64) -> u64 {
    let mut x = input.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// The first `cap` of `ids` ranked by (SplitMix64(seed XOR id), id).
fn ranked(ids: &[u64], seed: u64, cap: u64) -> Vec<u64> {
    let mut order = ids.to_vec();
    order.sort_by_key(|&id| (mix(seed ^ id), id));
    order.truncate(cap as usize);
    order
}

/// Unlabelled nodes and unweighted `R` edges, created in order.
fn simple(nodes: &[&str], edges: &[(&str, &str)]) -> TestGraph {
    let mut db = TestGraph::new();
    let mut mutations: Vec<GraphMutation> = nodes
        .iter()
        .map(|key| node(key, &[], Properties::new()))
        .collect();
    mutations.extend(
        edges
            .iter()
            .map(|(source, destination)| edge(None, source, "R", destination, Properties::new())),
    );
    db.run(&mutations);
    db
}

/// Every node of `graph` as a seed.
fn everything(graph: &Graph, direction: Direction) -> AlgorithmScope {
    AlgorithmScope {
        seeds: graph
            .nodes()
            .map(|node| NodeSelector::Id(node.id))
            .collect(),
        direction,
        types: None,
        labels: None,
        edge_conditions: Vec::new(),
    }
}

/// Betweenness over every node of `db` with zero hops at TTL clock 0.
fn measure(db: &TestGraph, direction: Direction, seed: u64, cap: u64) -> AlgorithmResult {
    let graph = db.graph();
    let all = everything(graph, direction);
    run_at(graph, &all, &sampled(seed), &hops(0), &samples(cap), 0).unwrap()
}

/// Every node of `db` ascending by ID with its expected score: the named
/// ones as given, the others 0.
fn expect(db: &TestGraph, named: &[(&str, f64)]) -> Vec<(u64, f64)> {
    let mut rows: Vec<(u64, f64)> = db.graph().nodes().map(|node| (node.id.0, 0.0)).collect();
    rows.sort_by_key(|row| row.0);
    for (key, score) in named {
        let id = db.id(key).0;
        rows.iter_mut().find(|row| row.0 == id).unwrap().1 = *score;
    }
    rows
}

/// Scores within 1e-12 of `expected`, row by row.
fn close(actual: &[(u64, f64)], expected: &[(u64, f64)]) {
    assert_eq!(actual.len(), expected.len(), "{actual:?} vs {expected:?}");
    for (row, wanted) in actual.iter().zip(expected) {
        assert_eq!(row.0, wanted.0, "{actual:?} vs {expected:?}");
        assert!(
            (row.1 - wanted.1).abs() <= 1e-12,
            "{actual:?} vs {expected:?}"
        );
    }
}

/// The output of a complete run that sampled every node: exact.
fn exact(result: &AlgorithmResult) -> &BetweennessOutput {
    assert!(
        result.complete && result.computation_complete,
        "{:?}",
        result.truncation
    );
    let output = sources(result);
    assert_eq!(output.completed_source_count, result.stats.nodes_extracted);
    assert_eq!(output.completed_sources, output.selected_sources);
    assert!(!output.estimated);
    output
}

#[test]
fn hand_checked_scores_count_every_shortest_path() {
    // a→b→c→d: the ordered pairs (a, c), (a, d) and (b, d) cross b or c;
    // n = 4 normalizes by 3 * 2.
    let chain = simple(&["a", "b", "c", "d"], &[("a", "b"), ("b", "c"), ("c", "d")]);
    for direction in [Direction::Out, Direction::In] {
        let result = measure(&chain, direction, 0, 16);
        exact(&result);
        close(
            &scores(&result),
            &expect(&chain, &[("b", 2.0 / 6.0), ("c", 2.0 / 6.0)]),
        );
    }
    // Both adds the reverse pairs and nothing halves them again.
    let result = measure(&chain, Direction::Both, 0, 16);
    exact(&result);
    close(
        &scores(&result),
        &expect(&chain, &[("b", 4.0 / 6.0), ("c", 4.0 / 6.0)]),
    );

    // A star out of h: no directed path crosses h; Both makes h the middle
    // of all 12 ordered leaf pairs, the normalized maximum 1.
    let star = simple(
        &["h", "l1", "l2", "l3", "l4"],
        &[("h", "l1"), ("h", "l2"), ("h", "l3"), ("h", "l4")],
    );
    for direction in [Direction::Out, Direction::In] {
        let result = measure(&star, direction, 0, 16);
        exact(&result);
        close(&scores(&result), &expect(&star, &[]));
    }
    let result = measure(&star, Direction::Both, 0, 16);
    exact(&result);
    close(&scores(&result), &expect(&star, &[("h", 1.0)]));

    // A diamond s→a→t, s→b→t: both shortest paths from s to t count, so a
    // and b take half each; undirected, every node is the middle of two
    // ordered pairs with two paths each.
    let diamond = simple(
        &["s", "a", "b", "t"],
        &[("s", "a"), ("s", "b"), ("a", "t"), ("b", "t")],
    );
    for direction in [Direction::Out, Direction::In] {
        let result = measure(&diamond, direction, 0, 16);
        exact(&result);
        close(
            &scores(&result),
            &expect(&diamond, &[("a", 0.5 / 6.0), ("b", 0.5 / 6.0)]),
        );
    }
    let result = measure(&diamond, Direction::Both, 0, 16);
    exact(&result);
    let sixth = 1.0 / 6.0;
    close(
        &scores(&result),
        &expect(
            &diamond,
            &[("s", sixth), ("a", sixth), ("b", sixth), ("t", sixth)],
        ),
    );
}

#[test]
fn parallel_arcs_collapse_and_self_loops_are_ignored() {
    // Counted twice, the doubled s→a would give a 2/3 of the pair (s, t).
    let names = ["s", "a", "b", "t"];
    let doubled = simple(
        &names,
        &[
            ("s", "a"),
            ("s", "a"),
            ("s", "b"),
            ("a", "a"),
            ("a", "t"),
            ("b", "t"),
            ("t", "t"),
        ],
    );
    let plain = simple(&names, &[("s", "a"), ("s", "b"), ("a", "t"), ("b", "t")]);
    for direction in [Direction::Out, Direction::In, Direction::Both] {
        let collapsed = measure(&doubled, direction, 5, 16);
        exact(&collapsed);
        assert_eq!(
            sources(&collapsed),
            sources(&measure(&plain, direction, 5, 16))
        );
    }
    close(
        &scores(&measure(&doubled, Direction::Out, 5, 16)),
        &expect(&doubled, &[("a", 0.5 / 6.0), ("b", 0.5 / 6.0)]),
    );

    // Loops on a chain change nothing: b carries (a, c) alone.
    let looped = simple(
        &["a", "b", "c"],
        &[("a", "a"), ("a", "b"), ("b", "b"), ("b", "c")],
    );
    let result = measure(&looped, Direction::Out, 0, 16);
    exact(&result);
    close(&scores(&result), &expect(&looped, &[("b", 0.5)]));
}

#[test]
fn disconnected_parts_and_isolates_count_in_the_scope_size() {
    let db = simple(
        &["a", "b", "c", "d", "e", "f", "g"],
        &[("a", "b"), ("b", "c"), ("d", "e"), ("e", "f")],
    );
    for direction in [Direction::Out, Direction::In] {
        let result = measure(&db, direction, 0, 16);
        exact(&result);
        close(
            &scores(&result),
            &expect(&db, &[("b", 1.0 / 30.0), ("e", 1.0 / 30.0)]),
        );
    }
    let result = measure(&db, Direction::Both, 0, 16);
    close(
        &scores(&result),
        &expect(&db, &[("b", 2.0 / 30.0), ("e", 2.0 / 30.0)]),
    );
}

#[test]
fn two_nodes_or_fewer_score_zero() {
    let pair = simple(&["a", "b"], &[("a", "b"), ("b", "a")]);
    for direction in [Direction::Out, Direction::In, Direction::Both] {
        let result = measure(&pair, direction, 0, 16);
        exact(&result);
        close(&scores(&result), &expect(&pair, &[]));
    }
    // One source of two: estimated, still zero.
    let result = measure(&pair, Direction::Out, 0, 1);
    assert!(result.complete && result.computation_complete);
    let output = sources(&result);
    assert_eq!(output.completed_source_count, 1);
    assert_eq!(output.selected_sources.len(), 1);
    assert!(output.estimated);
    close(&scores(&result), &expect(&pair, &[]));

    let single = simple(&["a"], &[("a", "a")]);
    let result = measure(&single, Direction::Both, 9, 16);
    assert_eq!(exact(&result).selected_sources, vec![single.id("a").0]);
    close(&scores(&result), &expect(&single, &[]));
}

#[test]
fn splitmix64_matches_known_vectors() {
    for (input, expected) in [
        (0, 0xE220_A839_7B1D_CDAF),
        (1, 0x910A_2DEC_8902_5CC1),
        (2, 0x9758_35DE_1C97_56CE),
        (7, 0x63CB_E1E4_5932_0DD7),
        (42, 0xBDD7_3226_2FEB_6E95),
        (0x0123_4567_89AB_CDEF, 0x157A_3807_A48F_AA9D),
        (u64::MAX, 0xE4D9_7177_1B65_2C20),
    ] {
        assert_eq!(splitmix64(input), expected, "{input:#x}");
        assert_eq!(mix(input), expected, "{input:#x}");
    }
}

/// Isolated nodes with exactly the IDs `ids`, keyed `n{id}`.
fn numbered(ids: &[u64]) -> TestGraph {
    let mut db = TestGraph::new();
    db.commit(GraphBody {
        graph_id: G,
        entries: ids
            .iter()
            .map(|&id| GraphEntry::PutNode {
                node_id: NodeId(id),
                key: format!("n{id}"),
                labels: BTreeSet::new(),
                properties: Properties::new(),
            })
            .collect(),
    });
    db
}

#[test]
fn sources_are_ranked_by_hash_then_id() {
    let db = numbered(&[1, 2, 3, 4, 5]);
    for (seed, order) in [
        (0, [3, 5, 4, 1, 2]),
        (1, [2, 4, 5, 3, 1]),
        (7, [4, 2, 3, 5, 1]),
        (42, [3, 2, 5, 1, 4]),
        (0xDEAD_BEEF, [2, 3, 1, 4, 5]),
    ] {
        assert_eq!(ranked(&[5, 4, 3, 2, 1], seed, 64), order);
        let result = measure(&db, Direction::Out, seed, 64);
        assert_eq!(exact(&result).selected_sources, order);
        // A smaller cap keeps the prefix; the run is complete but
        // estimated.
        let result = measure(&db, Direction::Out, seed, 2);
        assert!(result.complete && result.computation_complete);
        let output = sources(&result);
        assert_eq!(output.selected_sources, order[..2].to_vec());
        assert_eq!(output.completed_sources, order[..2].to_vec());
        assert_eq!(output.completed_source_count, 2);
        assert!(output.estimated);
    }
}

/// Sampled betweenness by definition over a projection: sources ranked by
/// the independent [`mix`], shortest-path counts by breadth-first layers
/// from every node, and each pair dependency `sigma_sv * sigma_vt /
/// sigma_st` added to every intermediate `v` on a shortest path. Returns
/// the scores ascending by node and the sampled sources.
fn naive_betweenness(
    graph: &Graph,
    (nodes, edges): (&[NodeId], &[EdgeId]),
    direction: Direction,
    seed: u64,
    cap: u64,
) -> (Vec<(u64, f64)>, Vec<u64>) {
    let n = nodes.len();
    let at = |id: NodeId| nodes.binary_search(&id).unwrap();
    let mut arcs = BTreeSet::new();
    for &id in edges {
        let edge = graph.edge(id).unwrap();
        let (mut u, mut v) = (at(edge.source), at(edge.destination));
        if direction == Direction::In {
            (u, v) = (v, u);
        }
        if u != v {
            arcs.insert((u, v));
            if direction == Direction::Both {
                arcs.insert((v, u));
            }
        }
    }
    // dist[s][v] and sigma[s][v] (u128 counts), None when unreachable.
    let mut dist = vec![vec![None; n]; n];
    let mut sigma = vec![vec![0u128; n]; n];
    for s in 0..n {
        dist[s][s] = Some(0usize);
        sigma[s][s] = 1;
        let mut layer = vec![s];
        let mut depth = 0;
        while !layer.is_empty() {
            let mut next = BTreeSet::new();
            for &u in &layer {
                for &(_, v) in arcs.range((u, 0)..(u + 1, 0)) {
                    if dist[s][v].is_none() || dist[s][v] == Some(depth + 1) {
                        dist[s][v] = Some(depth + 1);
                        sigma[s][v] += sigma[s][u];
                        next.insert(v);
                    }
                }
            }
            layer = next.into_iter().collect();
            depth += 1;
        }
    }
    let ids: Vec<u64> = nodes.iter().map(|node| node.0).collect();
    let chosen = ranked(&ids, seed, cap);
    let k = chosen.len() as f64;
    let scores = (0..n)
        .map(|v| {
            let mut total = 0.0;
            for &source in &chosen {
                let s = at(NodeId(source));
                for t in 0..n {
                    if s == v || t == v || s == t {
                        continue;
                    }
                    if let (Some(sv), Some(vt), Some(st)) = (dist[s][v], dist[v][t], dist[s][t])
                        && sv + vt == st
                    {
                        total += (sigma[s][v] * sigma[v][t]) as f64 / sigma[s][t] as f64;
                    }
                }
            }
            let score = if n > 2 {
                (n as f64 / k) * total / ((n - 1) * (n - 2)) as f64
            } else {
                0.0
            };
            (ids[v], score)
        })
        .collect();
    (scores, chosen)
}

#[test]
fn random_scopes_match_a_definitional_oracle() {
    let (mut full, mut partial) = (0, 0);
    for seed in 0..300 {
        let mut db = random_graph(seed);
        let mut rng = Rng::new(seed ^ 0xbe7);
        if rng.chance(40) {
            deadlines(&mut db, &mut rng);
        }
        let graph = db.graph();
        let direction = rng.pick(&[Direction::Out, Direction::In, Direction::Both]);
        let types = rng
            .chance(30)
            .then(|| names(&[rng.pick(&TYPES), rng.pick(&TYPES)]));
        let labels = rng.chance(30).then(|| names(&["A"]));
        let candidates: Vec<NodeId> = graph
            .nodes()
            .filter(|node| {
                visible(graph, node.id)
                    && labels
                        .as_ref()
                        .is_none_or(|labels| carries_all(labels, &node.labels))
            })
            .map(|node| node.id)
            .collect();
        if candidates.is_empty() {
            continue;
        }
        let mut seeds: Vec<NodeId> = (0..1 + rng.below(4))
            .map(|_| candidates[rng.below(candidates.len() as u64) as usize])
            .collect();
        let scope = AlgorithmScope {
            seeds: seeds.iter().map(|&id| NodeSelector::Id(id)).collect(),
            direction,
            types,
            labels,
            edge_conditions: Vec::new(),
        };
        seeds.sort();
        seeds.dedup();
        let max_hops = rng.below(5);
        let cap = rng.pick(&[1, 2, 3, 5, 64]);
        let sampling_seed = rng.next();
        let (nodes, edges) = naive_projection(graph, &scope, &seeds, max_hops);
        let result = run_at(
            graph,
            &scope,
            &sampled(sampling_seed),
            &hops(max_hops),
            &samples(cap),
            NOW,
        )
        .unwrap();
        assert!(result.complete, "seed {seed}: {:?}", result.truncation);
        assert_eq!(projected(&result), (nodes.clone(), edges.clone()));
        let (expected, chosen) =
            naive_betweenness(graph, (&nodes, &edges), direction, sampling_seed, cap);
        let output = sources(&result);
        assert_eq!(output.selected_sources, chosen, "seed {seed}");
        assert_eq!(output.completed_sources, chosen, "seed {seed}");
        assert_eq!(output.completed_source_count, chosen.len() as u64);
        assert_eq!(output.estimated, chosen.len() < nodes.len());
        let actual = scores(&result);
        assert_eq!(actual.len(), expected.len());
        for (row, wanted) in actual.iter().zip(&expected) {
            assert_eq!(row.0, wanted.0, "seed {seed}");
            assert!(
                (row.1 - wanted.1).abs() <= 1e-9,
                "seed {seed}: {actual:?} vs {expected:?}"
            );
        }
        if output.estimated {
            partial += 1;
        } else {
            full += 1;
        }
    }
    assert!(full > 50 && partial > 50, "{full} full, {partial} partial");
}

#[test]
fn partial_samples_scale_by_completed_sources() {
    // a→b→c→d: source a gives b 2 (pairs (a, c) and (a, d)) and c 1; b
    // gives c 1; c and d give nothing.
    let chain = simple(&["a", "b", "c", "d"], &[("a", "b"), ("b", "c"), ("c", "d")]);
    let [a, b, c, d] = ["a", "b", "c", "d"].map(|key| chain.id(key).0);
    let dependency = |source: u64, node: u64| match (source, node) {
        (s, v) if s == a && v == b => 2.0,
        (s, v) if (s == a || s == b) && v == c => 1.0,
        _ => 0.0,
    };
    for seed in 0..20 {
        for cap in 1..=4u64 {
            let result = measure(&chain, Direction::Out, seed, cap);
            assert!(result.complete && result.computation_complete);
            let output = sources(&result);
            let chosen = ranked(&[a, b, c, d], seed, cap);
            assert_eq!(output.selected_sources, chosen);
            assert_eq!(output.completed_source_count, cap);
            assert_eq!(output.estimated, cap < 4);
            let expected: Vec<(u64, f64)> = [a, b, c, d]
                .into_iter()
                .map(|node| {
                    let total: f64 = chosen.iter().map(|&s| dependency(s, node)).sum();
                    (node, (4.0 / cap as f64) * total / 6.0)
                })
                .collect();
            close(&scores(&result), &expected);
        }
    }
}

#[test]
fn results_depend_only_on_the_scope_and_the_seed() {
    let names = ["s", "a", "b", "t", "u"];
    let edges = [
        ("s", "a"),
        ("s", "b"),
        ("a", "t"),
        ("b", "t"),
        ("t", "u"),
        ("u", "s"),
        ("a", "b"),
    ];
    let forward = simple(&names, &edges);
    let mut reversed_edges = edges;
    reversed_edges.reverse();
    // The same node IDs with other edge IDs.
    let reversed = simple(&names, &reversed_edges);
    for direction in [Direction::Out, Direction::In, Direction::Both] {
        for seed in [0, 1, 99, u64::MAX] {
            for cap in [2, 16] {
                let first = measure(&forward, direction, seed, cap);
                assert_eq!(first, measure(&forward, direction, seed, cap));
                // Seeds in another order and form, repeated.
                let graph = reversed.graph();
                let mut scope = everything(graph, direction);
                scope.seeds.reverse();
                scope.seeds.push(key("t"));
                scope.seeds.push(key("s"));
                let other =
                    run_at(graph, &scope, &sampled(seed), &hops(0), &samples(cap), 0).unwrap();
                assert_eq!(other.output, first.output);
            }
        }
    }
    // Other seeds sample other sources; full samples agree up to the
    // order of summation.
    let zero = measure(&forward, Direction::Both, 0, 16);
    let one = measure(&forward, Direction::Both, 1, 16);
    assert_ne!(
        sources(&zero).selected_sources,
        sources(&one).selected_sources
    );
    close(&scores(&zero), &scores(&one));
}

/// Comparisons charged for sorting `count` items (see `sort_cost`).
fn sort_units(count: u64) -> u64 {
    count * u64::from(u64::BITS - count.leading_zeros())
}

#[test]
fn metering_follows_the_documented_accounting() {
    // a→b twice, b→c, c→d and the loop c→c, walked Out: n = 4 nodes, e = 5
    // edges, 4 non-loop arcs and 3 distinct ones.
    let db = simple(
        &["a", "b", "c", "d"],
        &[("a", "b"), ("a", "b"), ("b", "c"), ("c", "c"), ("c", "d")],
    );
    let graph = db.graph();
    let all = everything(graph, Direction::Out);
    let (n, e, arcs, distinct) = (4u64, 5u64, 4u64, 3u64);
    let degree = run_at(graph, &all, &Algorithm::Degree, &hops(0), &CAPS, 0).unwrap();
    // Degree adds n vertex units, e edge units and n rows to the shared
    // preparation and extraction.
    let shared = (
        degree.stats.vertex_operations - n,
        degree.stats.edges_examined - e,
        degree.stats.scratch_bytes - n * size_of::<DegreeRow>() as u64,
    );
    // Per source: (nodes reached, arcs scanned from them, its own arcs).
    let [a, b, c, d] = ["a", "b", "c", "d"].map(|key| db.id(key).0);
    let reach = |source: u64| match source {
        s if s == a => (4, 3, 1),
        s if s == b => (3, 2, 1),
        s if s == c => (2, 1, 1),
        s if s == d => (1, 0, 0),
        _ => unreachable!(),
    };
    for cap in 1..=4u64 {
        let result = run_at(graph, &all, &sampled(11), &hops(0), &samples(cap), 0).unwrap();
        assert!(result.complete);
        let m = cap;
        // Setup: arcs (one edge unit per edge), the sort with duplicate
        // removal, offsets (n + 1 fills, one edge unit per distinct arc,
        // n prefix sums), n hashes and their sort, m selected copies, then
        // n state and n total fills.
        let mut vertices = sort_units(arcs) + (n + 1) + n + n + sort_units(n) + m + n + n;
        let mut edges = e + distinct;
        // Each source: a pop per reached node and a scan per arc; the
        // dependencies of every reached node but the source; then one
        // commit and staging unit per node.
        for &source in &sources(&result).selected_sources {
            let (reached, scanned, own) = reach(source);
            vertices += reached + (reached - 1) + n;
            edges += scanned + (scanned - own);
        }
        // Scratch: the arcs at capacity e and the sort buffer, offsets,
        // hashes and their sort buffer, the selected and completed lists,
        // search state, the queue, the totals and two row buffers.
        let scratch = 16 * e
            + 16 * arcs
            + 8 * (n + 1)
            + 16 * n
            + 16 * n
            + 8 * 2 * m
            + 24 * n
            + 8 * n
            + 8 * n
            + 16 * 2 * n;
        assert_eq!(
            (
                result.stats.vertex_operations,
                result.stats.edges_examined,
                result.stats.scratch_bytes
            ),
            (shared.0 + vertices, shared.1 + edges, shared.2 + scratch),
            "cap {cap}"
        );
    }
}

/// A `rows` x `cols` grid `g{r}_{c}` with edges right and down.
fn grid(rows: usize, cols: usize) -> TestGraph {
    let names: Vec<String> = (0..rows)
        .flat_map(|r| (0..cols).map(move |c| format!("g{r}_{c}")))
        .collect();
    let mut pairs = Vec::new();
    for r in 0..rows {
        for c in 0..cols {
            if c + 1 < cols {
                pairs.push((format!("g{r}_{c}"), format!("g{r}_{}", c + 1)));
            }
            if r + 1 < rows {
                pairs.push((format!("g{r}_{c}"), format!("g{}_{c}", r + 1)));
            }
        }
    }
    let keys: Vec<&str> = names.iter().map(String::as_str).collect();
    let edges: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(source, destination)| (source.as_str(), destination.as_str()))
        .collect();
    simple(&keys, &edges)
}

/// `stopped`'s output equals the run that sampled only its completed
/// sources, besides the selected list, which is the full run's.
fn same_as_capped(
    stopped: &AlgorithmResult,
    full: &AlgorithmResult,
    rerun: &dyn Fn(u64) -> AlgorithmResult,
) -> u64 {
    let output = sources(stopped);
    let k = output.completed_source_count;
    assert!(k >= 1);
    assert_eq!(output.selected_sources, sources(full).selected_sources);
    let capped = rerun(k);
    let expected = sources(&capped);
    assert_eq!(output.completed_sources, expected.completed_sources);
    assert_eq!(output.scores, expected.scores);
    assert_eq!(output.estimated, expected.estimated);
    // Every buffer was reserved before the first source.
    assert_eq!(stopped.stats.scratch_bytes, full.stats.scratch_bytes);
    k
}

#[test]
fn every_budget_stops_at_its_boundary_and_keeps_only_full_sources() {
    let db = grid(4, 4);
    let graph = db.graph();
    let all = everything(graph, Direction::Both);
    let algorithm = sampled(3);
    let at = |limits: GraphLimits, caps: GraphAlgorithmLimits| {
        run_at(graph, &all, &algorithm, &limits, &caps, 0).unwrap()
    };
    let caps = samples(6);
    let full = at(hops(0), caps);
    assert!(full.complete);
    let m = sources(&full).selected_sources.len() as u64;
    assert_eq!(m, 6);
    let rerun = |k: u64| at(hops(0), samples(k));
    let spent = full.stats;

    let mut seen = BTreeSet::new();
    for budget in 1..spent.vertex_operations {
        let result = at(
            hops(0),
            GraphAlgorithmLimits {
                max_vertex_operations: budget,
                ..caps
            },
        );
        assert_eq!(result.truncation, vec![AlgorithmTruncation::VertexLimit]);
        assert_eq!(result.stats.vertex_operations, budget);
        seen.insert(
            result
                .output
                .as_ref()
                .map(|_| same_as_capped(&result, &full, &rerun)),
        );
    }
    let expected: BTreeSet<Option<u64>> = std::iter::once(None).chain((1..m).map(Some)).collect();
    assert_eq!(seen, expected);

    let mut seen = BTreeSet::new();
    for budget in 1..spent.edges_examined {
        let limits = GraphLimits {
            max_edges_examined: budget,
            ..hops(0)
        };
        let result = at(limits, caps);
        assert_eq!(result.truncation, vec![AlgorithmTruncation::EdgeLimit]);
        seen.insert(
            result
                .output
                .as_ref()
                .map(|_| same_as_capped(&result, &full, &rerun)),
        );
    }
    assert_eq!(seen, expected);

    // Every buffer is reserved before the first source.
    let mut budgets: Vec<u64> = (1..spent.scratch_bytes).step_by(7).collect();
    budgets.push(spent.scratch_bytes - 1);
    for budget in budgets {
        let result = at(
            hops(0),
            GraphAlgorithmLimits {
                max_scratch_bytes: budget,
                ..caps
            },
        );
        assert_eq!(result.truncation, vec![AlgorithmTruncation::MemoryLimit]);
        assert_eq!(result.output, None);
    }

    // Exact budgets reproduce the run.
    let exact_budgets = at(
        GraphLimits {
            max_edges_examined: spent.edges_examined,
            ..hops(0)
        },
        GraphAlgorithmLimits {
            max_vertex_operations: spent.vertex_operations,
            max_scratch_bytes: spent.scratch_bytes,
            ..caps
        },
    );
    assert_eq!(exact_budgets, full);

    // The source cap: n sources are exact, fewer are estimated, the
    // ceiling is 64 and the floor 1.
    let n = full.stats.nodes_extracted;
    let all_sources = at(hops(0), samples(n));
    exact(&all_sources);
    assert_eq!(at(hops(0), samples(64)).output, all_sources.output);
    let fewer = at(hops(0), samples(n - 1));
    assert!(fewer.complete && sources(&fewer).estimated);
    assert_eq!(sources(&fewer).completed_source_count, n - 1);
    assert_eq!(
        run_at(graph, &all, &algorithm, &hops(0), &samples(65), 0),
        Err(AlgorithmError::Graph(GraphError::LimitExceedsCeiling {
            limit: "max_sampled_sources",
            requested: 65,
            ceiling: 64,
        }))
    );
    assert_eq!(
        run_at(graph, &all, &algorithm, &hops(0), &samples(0), 0),
        Err(AlgorithmError::Graph(GraphError::InvalidQuery(
            "max_sampled_sources must be at least 1"
        )))
    );
    assert_eq!(GraphAlgorithmLimits::DEFAULT.max_sampled_sources, 16);
    assert_eq!(GraphAlgorithmLimits::CEILING.max_sampled_sources, 64);
}

/// Runs with `clock` as the monotonic clock, `cancel` as the flag, a 10 ms
/// deadline and zero hops.
fn clocked(
    graph: &Graph,
    scope: &AlgorithmScope,
    algorithm: &Algorithm,
    caps: &GraphAlgorithmLimits,
    clock: &dyn Fn() -> u64,
    cancel: Option<&AtomicBool>,
) -> AlgorithmResult {
    let context = ExecutionContext {
        cancel,
        ..ExecutionContext::new(VIEW, clock)
    };
    let limits = GraphLimits {
        deadline_ms: 10,
        ..hops(0)
    };
    let result = run(graph, scope, algorithm, &limits, caps, &context).unwrap();
    check(&result, 0);
    result
}

/// Counted work on this thread: sort comparisons and moves plus every
/// betweenness unit (setup, pops, arc scans, dependencies, staging).
fn progress() -> u64 {
    SORTED.with(Cell::get) + WORKED.with(Cell::get)
}

fn reset() {
    SORTED.with(|count| count.set(0));
    WORKED.with(|count| count.set(0));
}

#[test]
fn the_deadline_and_cancellation_stop_within_64_counted_operations() {
    let cases = [
        (grid(5, 6), Direction::Both),
        (grid(5, 6), Direction::Out),
        (isolated(200).0, Direction::Out),
    ];
    for (db, direction) in &cases {
        let graph = db.graph();
        let all = everything(graph, *direction);
        let algorithm = sampled(7);
        let caps = samples(16);
        reset();
        let full = run_at(graph, &all, &algorithm, &hops(0), &caps, 0).unwrap();
        let total = progress();
        assert!(full.computation_complete, "{:?}", full.truncation);
        let m = sources(&full).selected_sources.len() as u64;
        assert_eq!(m, 16);
        // Counted work at the end of each source: a run of cap j counts
        // m - j fewer selected copies.
        let ends: Vec<u64> = (1..=m)
            .map(|j| {
                reset();
                run_at(graph, &all, &algorithm, &hops(0), &samples(j), 0).unwrap();
                progress() + (m - j)
            })
            .collect();
        assert_eq!(*ends.last().unwrap(), total);
        let rerun = |k: u64| run_at(graph, &all, &algorithm, &hops(0), &samples(k), 0).unwrap();

        let mut points: BTreeSet<u64> = (1..=total).step_by(29).collect();
        points.extend([1, 63, 64, 65, ends[0] - 1, ends[0], ends[0] + 1, total]);
        for end in &ends {
            points.insert((end + 30).min(total));
        }
        let mut kept = BTreeSet::new();
        for inject in points {
            for cancel in [false, true] {
                // The deadline passes, or the flag is set, once counted
                // work `inject` has run; the clock is read only by checks.
                let flag = AtomicBool::new(false);
                let clock = || {
                    if progress() < inject {
                        return 0;
                    }
                    if cancel {
                        flag.store(true, AtomicOrdering::Relaxed);
                        0
                    } else {
                        1_000
                    }
                };
                reset();
                let result = clocked(graph, &all, &algorithm, &caps, &clock, Some(&flag));
                let stopped_at = progress();
                assert!(
                    stopped_at >= inject && stopped_at - inject <= 64,
                    "{} counted units ran unchecked at {inject}",
                    stopped_at - inject
                );
                let reason = if cancel {
                    AlgorithmTruncation::Cancelled
                } else {
                    AlgorithmTruncation::Deadline
                };
                assert_eq!(result.truncation, vec![reason]);
                assert!(!result.computation_complete);
                // Exactly the sources finished before the stop count; the
                // interrupted one is discarded.
                let finished = ends.iter().filter(|&&end| end <= stopped_at).count() as u64;
                match &result.output {
                    None => assert_eq!(finished, 0, "at {inject}"),
                    Some(_) => {
                        assert_eq!(same_as_capped(&result, &full, &rerun), finished);
                    }
                }
                kept.insert(finished);
            }
        }
        assert!(kept.contains(&0) && kept.contains(&1) && kept.contains(&m));
        assert!(kept.len() as u64 > m / 2, "{kept:?}");

        // A flag set before the run stops before any work.
        let flag = AtomicBool::new(true);
        let result = clocked(graph, &all, &algorithm, &caps, &|| 0, Some(&flag));
        assert_eq!(result.truncation, vec![AlgorithmTruncation::Cancelled]);
        assert_eq!(result.output, None);
        assert_eq!(
            result.stats.edges_examined + result.stats.vertex_operations,
            0
        );
    }
}

#[test]
fn output_caps_cut_rows_but_never_sources() {
    let db = simple(
        &["a", "b", "c", "d", "e"],
        &[("a", "b"), ("b", "c"), ("c", "d"), ("d", "e")],
    );
    let graph = db.graph();
    let all = everything(graph, Direction::Both);
    let full = measure(&db, Direction::Both, 0, 16);
    exact(&full);
    let rows = &sources(&full).scores;
    for max_output_nodes in [0u64, 2, 5] {
        let limits = GraphLimits {
            max_output_nodes,
            ..hops(0)
        };
        let result = run_at(graph, &all, &sampled(0), &limits, &samples(16), 0).unwrap();
        let cut = max_output_nodes < 5;
        let reasons = if cut {
            vec![AlgorithmTruncation::OutputLimit]
        } else {
            vec![]
        };
        assert_eq!(result.truncation, reasons);
        assert!(result.computation_complete && result.scope_complete);
        let output = sources(&result);
        assert_eq!(output.scores, rows[..max_output_nodes as usize].to_vec());
        assert_eq!(output.selected_sources, sources(&full).selected_sources);
        assert_eq!(output.completed_source_count, 5);
        // Only the rows kept are reserved.
        assert_eq!(
            full.stats.scratch_bytes - result.stats.scratch_bytes,
            2 * (5 - max_output_nodes) * size_of::<NodeScore>() as u64
        );
    }
    // A run stopped in its last source with rows cut reports only the stop.
    let limits = GraphLimits {
        max_output_nodes: 2,
        max_edges_examined: full.stats.edges_examined - 1,
        ..hops(0)
    };
    let result = run_at(graph, &all, &sampled(0), &limits, &samples(16), 0).unwrap();
    assert_eq!(result.truncation, vec![AlgorithmTruncation::EdgeLimit]);
    let output = sources(&result);
    assert_eq!(output.completed_source_count, 4);
    assert_eq!(output.scores.len(), 2);
}

#[test]
fn one_ttl_clock_hides_due_endpoints_and_their_edges() {
    let names = ["a", "b", "c", "x"];
    let edges = [("a", "b"), ("b", "c"), ("a", "x"), ("x", "c")];
    let mut db = simple(&names, &edges);
    db.run(&[GraphMutation::SetNodeExpiry {
        key: "x".into(),
        expires_at_ms: Some(NOW),
        expected: Precondition::Any,
    }]);
    let seeds = scope(&["a"], Direction::Out);
    let at = |db: &TestGraph, now| {
        run_at(db.graph(), &seeds, &sampled(0), &hops(2), &samples(16), now).unwrap()
    };
    // Before the deadline x shares the pair (a, c) with b.
    let before = at(&db, NOW - 1);
    exact(&before);
    close(
        &scores(&before),
        &expect(&db, &[("b", 0.5 / 6.0), ("x", 0.5 / 6.0)]),
    );
    // At it, x and its edges are gone: n = 3 and b carries the pair alone.
    let due = at(&db, NOW);
    exact(&due);
    assert_eq!(due.stats.edges_extracted, 2);
    let hidden = db.id("x").0;
    let expected: Vec<(u64, f64)> = expect(&db, &[("b", 0.5)])
        .into_iter()
        .filter(|row| row.0 != hidden)
        .collect();
    close(&scores(&due), &expected);

    // A due edge: without b→c, x carries the pair alone.
    let mut db = simple(&names, &edges);
    let (b, c) = (db.id("b"), db.id("c"));
    let bc = db
        .graph()
        .edges()
        .find(|edge| edge.source == b && edge.destination == c)
        .unwrap()
        .id;
    db.run(&[GraphMutation::SetEdgeExpiry {
        edge: EdgeSelector::Id(bc),
        expires_at_ms: Some(NOW),
        expected: Precondition::Any,
    }]);
    let due = at(&db, NOW);
    exact(&due);
    close(&scores(&due), &expect(&db, &[("x", 1.0 / 6.0)]));
}

/// `layers` layers of two nodes `l{i}a` and `l{i}b`, each joined to both
/// nodes of the next layer: a node `j` layers past a single source is the
/// end of 2^(j-1) shortest paths.
fn ladder(layers: usize) -> TestGraph {
    let mut db = TestGraph::new();
    let nodes: Vec<GraphMutation> = (0..layers)
        .flat_map(|layer| {
            ["a", "b"].map(|side| node(&format!("l{layer}{side}"), &[], Properties::new()))
        })
        .collect();
    db.run(&nodes);
    let edges: Vec<GraphMutation> = (0..layers - 1)
        .flat_map(|layer| {
            [("a", "a"), ("a", "b"), ("b", "a"), ("b", "b")].map(|(from, to)| {
                edge(
                    None,
                    &format!("l{layer}{from}"),
                    "R",
                    &format!("l{}{to}", layer + 1),
                    Properties::new(),
                )
            })
        })
        .collect();
    for chunk in edges.chunks(2_000) {
        db.run(chunk);
    }
    db
}

#[test]
fn path_counts_beyond_f64_are_a_numeric_error() {
    let db = ladder(1_100);
    let graph = db.graph();
    let all = everything(graph, Direction::Out);
    let ids: Vec<u64> = graph.nodes().map(|node| node.id.0).collect();
    let layer_of = |id: u64| {
        let key = &graph.node(NodeId(id)).unwrap().key;
        key[1..key.len() - 1].parse::<usize>().unwrap()
    };
    // A sampling seed whose first source lies in a layer below `upto` or
    // from `from` on.
    let first_in = |range: std::ops::Range<usize>| {
        (0u64..)
            .find(|&seed| range.contains(&layer_of(ranked(&ids, seed, 1)[0])))
            .unwrap()
    };
    let limits = GraphLimits {
        max_visited_nodes: 10_000,
        max_edges_examined: 100_000,
        ..hops(0)
    };
    let caps = GraphAlgorithmLimits {
        max_vertex_operations: 1_000_000,
        ..samples(1)
    };
    // From the first 50 layers, layer 1,025 onward ends 2^1024 paths.
    let early = first_in(0..50);
    assert_eq!(
        run_at(graph, &all, &sampled(early), &limits, &caps, 0),
        Err(AlgorithmError::NumericOverflow)
    );
    // From the last 50 layers every count is finite; only the rows are cut
    // (2,200 nodes exceed the output ceiling).
    let late = first_in(1_050..1_100);
    let result = run_at(graph, &all, &sampled(late), &limits, &caps, 0).unwrap();
    assert!(result.computation_complete && sources(&result).estimated);
    assert_eq!(result.truncation, [AlgorithmTruncation::OutputLimit]);
}

/// The shared extraction checks hold on the betweenness route: arcs are
/// counted as extraction retains each edge, no projection edge is visited
/// after the final check, a run stopped in its last source keeps its scope
/// and arc count, and a stop injected while a growing buffer relocates its
/// items lands within 64 moves, before any betweenness work, and returns
/// nothing.
#[test]
fn extraction_counts_arcs_and_checks_growth_on_the_betweenness_route() {
    use super::super::{LATE, MOVED};
    let names: Vec<String> = (0..300).map(|at| format!("n{at}")).collect();
    let keys: Vec<&str> = names.iter().map(String::as_str).collect();
    // A 299-edge chain plus a self-loop on every tenth node.
    let mut edges: Vec<(&str, &str)> = keys.windows(2).map(|pair| (pair[0], pair[1])).collect();
    edges.extend(keys.iter().step_by(10).map(|&key| (key, key)));
    let db = simple(&keys, &edges);
    let graph = db.graph();
    let wide = GraphLimits {
        max_edges_examined: GraphLimits::CEILING.max_edges_examined,
        ..hops(0)
    };
    let algorithm = sampled(5);
    for (direction, arcs) in [
        (Direction::Both, 2 * 299 + 30),
        (Direction::Out, 329),
        (Direction::In, 329),
    ] {
        let all = everything(graph, direction);
        LATE.with(|count| count.set(0));
        let full = run_at(graph, &all, &algorithm, &wide, &samples(4), 0).unwrap();
        assert!(full.computation_complete, "{:?}", full.truncation);
        assert_eq!(LATE.with(Cell::get), 0, "{direction:?}");
        assert_eq!(full.stats.arcs, arcs, "{direction:?}");
        let caps = GraphAlgorithmLimits {
            max_vertex_operations: full.stats.vertex_operations - 1,
            ..samples(4)
        };
        let short = run_at(graph, &all, &algorithm, &wide, &caps, 0).unwrap();
        assert_eq!(short.truncation, vec![AlgorithmTruncation::VertexLimit]);
        assert!(short.scope_complete && short.stats.arcs == arcs);
        assert_eq!(sources(&short).completed_source_count, 3);
        assert_eq!(LATE.with(Cell::get), 0, "{direction:?}");
    }

    // 329 retained edges grow through 4..=256 slots.
    let all = everything(graph, Direction::Out);
    let caps = samples(4);
    let moved = || MOVED.with(Cell::get);
    MOVED.with(|count| count.set(0));
    let full = clocked(graph, &all, &algorithm, &caps, &|| 0, None);
    assert!(full.computation_complete, "{:?}", full.truncation);
    let total = moved();
    assert_eq!(total, 508);
    let mut points: BTreeSet<u64> = (1..=total).step_by(17).collect();
    points.extend([1, 65, 253, 509, total]);
    points.retain(|&point| point <= total);
    for inject in points {
        for cancel in [false, true] {
            let flag = AtomicBool::new(false);
            let clock = || {
                if moved() < inject {
                    return 0;
                }
                if cancel {
                    flag.store(true, AtomicOrdering::Relaxed);
                    0
                } else {
                    1_000
                }
            };
            MOVED.with(|count| count.set(0));
            reset();
            let result = clocked(graph, &all, &algorithm, &caps, &clock, Some(&flag));
            let after = moved() - inject;
            assert!(after <= 64, "{after} moves ran unchecked at {inject}");
            let reason = if cancel {
                AlgorithmTruncation::Cancelled
            } else {
                AlgorithmTruncation::Deadline
            };
            assert_eq!(result.truncation, vec![reason]);
            assert!(result.output.is_none() && !result.computation_complete);
            assert!(result.projection.is_none());
            assert_eq!(WORKED.with(Cell::get), 0, "betweenness ran at {inject}");
        }
    }
}
