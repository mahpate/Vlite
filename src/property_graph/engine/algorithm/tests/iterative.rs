//! Engine tests of the iterative algorithms (Gap 5 S3): PageRank and label
//! propagation on hand-checked graphs, naive oracles over random scopes,
//! iteration caps, full-step rollback under every budget, the deadline,
//! cancellation, output caps, validation, weights and the TTL clock.

use super::*;

fn float(value: f64) -> PropertyValue {
    PropertyValue::Float(value)
}

/// PageRank with tolerance 1e-10 and uniform teleport.
fn rank(damping: f64, weight: Option<&str>) -> Algorithm {
    Algorithm::PageRank {
        damping,
        tolerance: 1e-10,
        personalization: None,
        weight_property: weight.map(str::to_owned),
    }
}

fn personalized(damping: f64, masses: &[(&str, f64)], weight: Option<&str>) -> Algorithm {
    Algorithm::PageRank {
        damping,
        tolerance: 1e-10,
        personalization: Some(
            masses
                .iter()
                .map(|(node, mass)| (key(node), *mass))
                .collect(),
        ),
        weight_property: weight.map(str::to_owned),
    }
}

fn iterations(max_iterations: u64) -> GraphAlgorithmLimits {
    GraphAlgorithmLimits {
        max_iterations,
        ..CAPS
    }
}

fn ranks(result: &AlgorithmResult) -> &PageRankOutput {
    match &result.output {
        Some(AlgorithmOutput::PageRank(output)) => output,
        other => panic!("no scores: {other:?}"),
    }
}

fn scores(result: &AlgorithmResult) -> Vec<(u64, f64)> {
    ranks(result)
        .scores
        .iter()
        .map(|row| (row.node_id, row.score))
        .collect()
}

fn communities(result: &AlgorithmResult) -> &LabelPropagationOutput {
    match &result.output {
        Some(AlgorithmOutput::LabelPropagation(output)) => output,
        other => panic!("no communities: {other:?}"),
    }
}

fn groups(result: &AlgorithmResult) -> Vec<(u64, u64)> {
    communities(result)
        .assignments
        .iter()
        .map(|row| (row.node_id, row.community_id))
        .collect()
}

/// Completed iterations or sweeps of an iterative output.
fn steps(result: &AlgorithmResult) -> u64 {
    match &result.output {
        Some(AlgorithmOutput::PageRank(output)) => output.iterations,
        Some(AlgorithmOutput::LabelPropagation(output)) => output.sweeps,
        other => panic!("not iterative: {other:?}"),
    }
}

/// Scores within 1e-8 of `expected`, with total mass 1 within 1e-12.
fn close(actual: &[(u64, f64)], expected: &[(u64, f64)]) {
    assert_eq!(actual.len(), expected.len(), "{actual:?}");
    for (actual_row, expected_row) in actual.iter().zip(expected) {
        assert_eq!(actual_row.0, expected_row.0);
        assert!(
            (actual_row.1 - expected_row.1).abs() <= 1e-8,
            "{actual:?} vs {expected:?}"
        );
    }
    let mass: f64 = actual.iter().map(|row| row.1).sum();
    assert!((mass - 1.0).abs() <= 1e-12, "mass {mass}");
}

/// Unlabelled nodes and `R` edges with an optional weight `w`.
fn small(nodes: &[&str], edges: &[(&str, &str, Option<PropertyValue>)]) -> TestGraph {
    let mut db = TestGraph::new();
    let mut mutations: Vec<GraphMutation> = nodes
        .iter()
        .map(|key| node(key, &[], Properties::new()))
        .collect();
    for (source, destination, weight) in edges {
        let properties = weight
            .clone()
            .map_or_else(Properties::new, |weight| props(&[("w", weight)]));
        mutations.push(edge(None, source, "R", destination, properties));
    }
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

/// a→e (w 1), d→b (2), e→b (3), e→f (1), d→f (2), e→b again (1) and the
/// loop f→f (4); c is isolated. Weighted `Out` PageRank needs 59 steps at
/// tolerance 1e-10 and label propagation 4 sweeps.
fn slow_graph() -> TestGraph {
    small(
        &["a", "b", "c", "d", "e", "f"],
        &[
            ("a", "e", Some(int(1))),
            ("d", "b", Some(int(2))),
            ("e", "b", Some(int(3))),
            ("e", "f", Some(int(1))),
            ("d", "f", Some(int(2))),
            ("e", "b", Some(int(1))),
            ("f", "f", Some(int(4))),
        ],
    )
}

type NaiveRank = (Vec<f64>, u64, f64, bool);

/// PageRank by definition, in the documented evaluation order: dangling
/// mass ascending, arcs by edge ID (a `Both` edge forward, then reversed),
/// then each node's value and its residual term ascending. `Err` names the
/// smallest retained edge without an integer weight.
fn naive_rank(
    graph: &Graph,
    (nodes, edges): (&[NodeId], &[EdgeId]),
    direction: Direction,
    weight: Option<&str>,
    (damping, tolerance, max_iterations): (f64, f64, u64),
    teleport: Option<&[(NodeId, f64)]>,
) -> Result<NaiveRank, EdgeId> {
    let count = nodes.len();
    let at = |id: NodeId| nodes.binary_search(&id).unwrap();
    let mut arcs = Vec::new();
    for &id in edges {
        let edge = graph.edge(id).unwrap();
        let w = match weight {
            None => 1.0,
            Some(field) => match edge.properties.get(field) {
                Some(PropertyValue::Integer(value)) => *value as f64,
                _ => return Err(id),
            },
        };
        let (u, v) = if direction == Direction::In {
            (edge.destination, edge.source)
        } else {
            (edge.source, edge.destination)
        };
        arcs.push((at(u), at(v), w));
        if direction == Direction::Both && u != v {
            arcs.push((at(v), at(u), w));
        }
    }
    let mut out = vec![0.0; count];
    for &(u, _, w) in &arcs {
        out[u] += w;
    }
    let p = match teleport {
        None => vec![1.0 / count as f64; count],
        Some(masses) => {
            let mut masses = masses.to_vec();
            masses.sort_by_key(|entry| entry.0);
            let mut total = 0.0;
            for (_, mass) in &masses {
                total += mass;
            }
            let mut p = vec![0.0; count];
            for (id, mass) in masses {
                p[at(id)] = mass / total;
            }
            p
        }
    };
    let mut r = p.clone();
    let mut iteration = 0;
    loop {
        iteration += 1;
        let mut dangling = 0.0;
        for v in 0..count {
            if out[v] == 0.0 {
                dangling += r[v];
            }
        }
        let mut next = vec![0.0; count];
        for &(u, v, w) in &arcs {
            if out[u] != 0.0 {
                next[v] += r[u] * (w / out[u]);
            }
        }
        let mut residual = 0.0;
        let mut values = vec![0.0; count];
        for v in 0..count {
            values[v] = (1.0 - damping) * p[v] + damping * (next[v] + dangling * p[v]);
            residual += (values[v] - r[v]).abs();
        }
        r = values;
        if residual <= tolerance || iteration == max_iterations {
            return Ok((r, iteration, residual, residual <= tolerance));
        }
    }
}

/// Label propagation by definition over node-ID labels: canonical
/// community IDs, sweeps and convergence.
fn naive_communities(
    graph: &Graph,
    (nodes, edges): (&[NodeId], &[EdgeId]),
    max_sweeps: u64,
) -> (Vec<u64>, u64, bool) {
    let at = |id: NodeId| nodes.binary_search(&id).unwrap();
    let mut neighbors = vec![BTreeSet::new(); nodes.len()];
    for &id in edges {
        let edge = graph.edge(id).unwrap();
        let (u, v) = (at(edge.source), at(edge.destination));
        if u != v {
            neighbors[u].insert(v);
            neighbors[v].insert(u);
        }
    }
    let mut labels: Vec<u64> = nodes.iter().map(|node| node.0).collect();
    let (mut sweeps, mut converged) = (0, false);
    while sweeps < max_sweeps && !converged {
        sweeps += 1;
        converged = true;
        for v in 0..nodes.len() {
            let mut votes = BTreeMap::new();
            for &u in &neighbors[v] {
                *votes.entry(labels[u]).or_insert(0u64) += 1;
            }
            let Some(&best) = votes.values().max() else {
                continue;
            };
            if votes.get(&labels[v]) != Some(&best) {
                labels[v] = *votes.iter().find(|(_, count)| **count == best).unwrap().0;
                converged = false;
            }
        }
    }
    let community = |label: u64| nodes[labels.iter().position(|other| *other == label).unwrap()].0;
    (
        labels.iter().map(|label| community(*label)).collect(),
        sweeps,
        converged,
    )
}

fn bits(values: &[(u64, f64)]) -> Vec<(u64, u64)> {
    values
        .iter()
        .map(|(node, value)| (*node, value.to_bits()))
        .collect()
}

#[test]
fn pagerank_matches_closed_forms() {
    let a_b = small(&["a", "b"], &[("a", "b", Some(int(2)))]);
    let reciprocal = small(&["a", "b"], &[("a", "b", None), ("b", "a", None)]);
    let isolates = small(&["a", "b"], &[]);
    let zero = small(
        &["a", "b"],
        &[("a", "b", Some(int(0))), ("b", "b", Some(float(-0.0)))],
    );
    let looped = small(
        &["a", "b"],
        &[
            ("a", "a", Some(int(1))),
            ("a", "b", Some(int(1))),
            ("a", "b", Some(float(2.0))),
        ],
    );
    let caps = iterations(100);
    let run = |db: &TestGraph, direction, algorithm: &Algorithm| {
        let graph = db.graph();
        run_at(
            graph,
            &everything(graph, direction),
            algorithm,
            &hops(0),
            &caps,
            0,
        )
        .unwrap()
    };

    // The reciprocal pair is stationary from the start: one step, no change.
    let result = run(&reciprocal, Direction::Out, &rank(0.85, None));
    assert!(result.complete && result.computation_complete);
    assert_eq!(scores(&result), vec![(0, 0.5), (1, 0.5)]);
    assert_eq!(
        (ranks(&result).iterations, ranks(&result).residual),
        (1, 0.0)
    );
    assert!(ranks(&result).converged);

    // a→b with b dangling: (1/(2+d), (1+d)/(2+d)) = (20/57, 37/57).
    let result = run(&a_b, Direction::Out, &rank(0.85, Some("w")));
    assert!(result.complete);
    close(&scores(&result), &[(0, 20.0 / 57.0), (1, 37.0 / 57.0)]);
    assert!(ranks(&result).residual <= 1e-10);
    // In reverses it.
    let result = run(&a_b, Direction::In, &rank(0.85, Some("w")));
    close(&scores(&result), &[(0, 37.0 / 57.0), (1, 20.0 / 57.0)]);

    // Personalized to a with d = 0.5: (2/3, 1/3).
    let result = run(
        &a_b,
        Direction::Out,
        &personalized(0.5, &[("a", 7.0)], Some("w")),
    );
    close(&scores(&result), &[(0, 2.0 / 3.0), (1, 1.0 / 3.0)]);

    // Isolates redistribute everything to p; listed in any order.
    let result = run(
        &isolates,
        Direction::Out,
        &personalized(0.85, &[("b", 3.0), ("a", 1.0)], None),
    );
    close(&scores(&result), &[(0, 0.25), (1, 0.75)]);
    assert!(ranks(&result).converged);

    // Zero-weight arcs (and -0.0) leave their tails dangling.
    let result = run(
        &zero,
        Direction::Out,
        &personalized(0.85, &[("a", 1.0), ("b", 3.0)], Some("w")),
    );
    close(&scores(&result), &[(0, 0.25), (1, 0.75)]);

    // Both walks a loop once and each parallel edge both ways: 5 arcs,
    // a→a 1/4, a→b 3/4, b→a 1; d = 0.5 gives (6/11, 5/11).
    let result = run(&looped, Direction::Both, &rank(0.5, Some("w")));
    close(&scores(&result), &[(0, 6.0 / 11.0), (1, 5.0 / 11.0)]);
    assert_eq!((result.stats.edges_extracted, result.stats.arcs), (3, 5));
}

#[test]
fn label_propagation_matches_documented_sweeps() {
    // Two pairs (one doubled, one with a loop) and an isolate.
    let db = small(
        &["a", "b", "c", "d", "e"],
        &[
            ("a", "b", None),
            ("a", "b", None),
            ("c", "d", None),
            ("a", "a", None),
        ],
    );
    let graph = db.graph();
    let all = everything(graph, Direction::Out);
    let result = exec(graph, &all, &Algorithm::LabelPropagation, &hops(0));
    assert!(result.complete);
    assert_eq!(
        groups(&result),
        vec![(0, 0), (1, 0), (2, 2), (3, 2), (4, 4)]
    );
    assert_eq!(
        (communities(&result).sweeps, communities(&result).converged),
        (2, true)
    );
    // Communities ignore direction and weights.
    let mut weighted = db.clone();
    weighted.run(&[edge(None, "e", "R", "e", props(&[("w", float(-1.0))]))]);
    for direction in [Direction::In, Direction::Both] {
        let graph = weighted.graph();
        let same = exec(
            graph,
            &everything(graph, direction),
            &Algorithm::LabelPropagation,
            &hops(0),
        );
        assert_eq!(same.output, result.output);
    }

    // A path a-b-c: a takes b's label, b keeps its tied label, c takes b's.
    // One sweep changed labels, so a cap of 1 is incomplete.
    let db = small(&["a", "b", "c"], &[("a", "b", None), ("c", "b", None)]);
    let graph = db.graph();
    let all = everything(graph, Direction::Out);
    let lpa = Algorithm::LabelPropagation;
    let capped = run_at(graph, &all, &lpa, &hops(0), &iterations(1), 0).unwrap();
    assert_eq!(capped.truncation, vec![AlgorithmTruncation::IterationLimit]);
    assert!(capped.scope_complete && !capped.computation_complete);
    assert_eq!(groups(&capped), vec![(0, 0), (1, 0), (2, 0)]);
    assert_eq!(
        (communities(&capped).sweeps, communities(&capped).converged),
        (1, false)
    );
    let done = run_at(graph, &all, &lpa, &hops(0), &iterations(2), 0).unwrap();
    assert!(done.complete && communities(&done).converged);
    assert_eq!(communities(&done).sweeps, 2);

    // The slow graph needs four sweeps.
    let db = slow_graph();
    let graph = db.graph();
    let result = run_at(
        graph,
        &everything(graph, Direction::Out),
        &lpa,
        &hops(0),
        &iterations(100),
        0,
    )
    .unwrap();
    assert_eq!(communities(&result).sweeps, 4);
    assert_eq!(
        groups(&result),
        vec![(0, 0), (1, 0), (2, 2), (3, 0), (4, 0), (5, 0)]
    );
}

#[test]
fn random_scopes_match_naive_iterations() {
    // Missing weight, converged and capped PageRank; converged and capped
    // label propagation.
    let mut seen = [0u32; 5];
    for seed in 0..300 {
        let mut db = random_graph(seed);
        let mut rng = Rng::new(seed ^ 0x53_7a);
        if rng.chance(40) {
            deadlines(&mut db, &mut rng);
        }
        let graph = db.graph();
        let direction = rng.pick(&[Direction::Out, Direction::In, Direction::Both]);
        let types = rng
            .chance(30)
            .then(|| names(&[rng.pick(&TYPES), rng.pick(&TYPES)]));
        let labels = rng.chance(25).then(|| names(&["A"]));
        let edge_conditions = if rng.chance(20) {
            vec![PropertyCondition::Range {
                field: "w".into(),
                min: Some(int(1)),
                max: None,
            }]
        } else {
            vec![]
        };
        let max_hops = rng.below(5);
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
            seeds: seeds.iter().map(|id| NodeSelector::Id(*id)).collect(),
            direction,
            types,
            labels,
            edge_conditions,
        };
        seeds.sort();
        seeds.dedup();
        let limits = hops(max_hops);
        let (nodes, edges) = naive_projection(graph, &scope, &seeds, max_hops);
        let ids: Vec<u64> = nodes.iter().map(|node| node.0).collect();

        let damping = rng.pick(&[0.85, 0.5, 0.99]);
        let tolerance = rng.pick(&[1e-3, 1e-6, 1e-10]);
        let max_iterations = rng.pick(&[1, 5, 20, 100]);
        let weight = rng.chance(60).then_some("w");
        let mut teleport = None;
        if rng.chance(40) {
            let mut masses = Vec::new();
            for &id in &seeds {
                if rng.chance(70) {
                    masses.push((id, (1 + rng.below(4)) as f64));
                }
            }
            teleport = (!masses.is_empty()).then_some(masses);
        }
        let algorithm = Algorithm::PageRank {
            damping,
            tolerance,
            personalization: teleport.as_ref().map(|masses| {
                masses
                    .iter()
                    .rev()
                    .map(|&(id, mass)| (NodeSelector::Id(id), mass))
                    .collect()
            }),
            weight_property: weight.map(str::to_owned),
        };
        let caps = iterations(max_iterations);
        let result = run_at(graph, &scope, &algorithm, &limits, &caps, NOW);
        let expected = naive_rank(
            graph,
            (&nodes, &edges),
            direction,
            weight,
            (damping, tolerance, max_iterations),
            teleport.as_deref(),
        );
        match expected {
            Err(edge) => {
                seen[0] += 1;
                assert_eq!(
                    result,
                    Err(AlgorithmError::MissingWeight(edge)),
                    "seed {seed}"
                );
            }
            Ok((values, done, residual, converged)) => {
                let result = result.unwrap();
                assert_eq!(projected(&result), (nodes.clone(), edges.clone()));
                let expected: Vec<(u64, f64)> = ids.iter().copied().zip(values).collect();
                assert_eq!(bits(&scores(&result)), bits(&expected), "seed {seed}");
                let output = ranks(&result);
                assert_eq!(
                    (
                        output.iterations,
                        output.residual.to_bits(),
                        output.converged
                    ),
                    (done, residual.to_bits(), converged),
                    "seed {seed}"
                );
                let mass: f64 = output.scores.iter().map(|row| row.score).sum();
                assert!((mass - 1.0).abs() <= 1e-12, "seed {seed}: mass {mass}");
                assert!(output.scores.iter().all(|row| row.score >= 0.0));
                assert_eq!(result.complete, converged);
                if !converged {
                    assert_eq!(result.truncation, vec![AlgorithmTruncation::IterationLimit]);
                }
                seen[1 + usize::from(!converged)] += 1;
            }
        }

        let max_sweeps = rng.pick(&[1, 2, 20]);
        let lpa = Algorithm::LabelPropagation;
        let result = run_at(graph, &scope, &lpa, &limits, &iterations(max_sweeps), NOW).unwrap();
        let (expected, sweeps, converged) = naive_communities(graph, (&nodes, &edges), max_sweeps);
        assert_eq!(
            groups(&result),
            ids.iter().copied().zip(expected).collect::<Vec<_>>(),
            "seed {seed}"
        );
        let output = communities(&result);
        assert_eq!((output.sweeps, output.converged), (sweeps, converged));
        assert_eq!(result.complete, converged);
        seen[3 + usize::from(!converged)] += 1;
    }
    assert!(seen.iter().all(|count| *count >= 10), "{seen:?}");
}

#[test]
fn iteration_caps_keep_the_last_full_step() {
    let db = slow_graph();
    let graph = db.graph();
    let all = everything(graph, Direction::Out);
    let algorithm = rank(0.85, Some("w"));
    let full = run_at(graph, &all, &algorithm, &hops(0), &iterations(100), 0).unwrap();
    assert!(full.complete && ranks(&full).converged);
    assert_eq!(ranks(&full).iterations, 59);

    // The default cap is 20.
    let capped = exec(graph, &all, &algorithm, &hops(0));
    assert_eq!(capped.truncation, vec![AlgorithmTruncation::IterationLimit]);
    assert!(capped.scope_complete && !capped.computation_complete);
    assert_eq!(ranks(&capped).iterations, 20);
    assert!(!ranks(&capped).converged);

    // Every cap returns exactly its last full step; a step costs one vertex
    // unit per node twice and one edge unit per arc.
    let (nodes, edges) = projected(&full);
    let mut previous: Option<AlgorithmStats> = None;
    for k in 1..=59 {
        let result = run_at(graph, &all, &algorithm, &hops(0), &iterations(k), 0).unwrap();
        let (values, done, residual, converged) = naive_rank(
            graph,
            (&nodes, &edges),
            Direction::Out,
            Some("w"),
            (0.85, 1e-10, k),
            None,
        )
        .unwrap();
        let ids = nodes.iter().map(|node| node.0);
        assert_eq!(
            bits(&scores(&result)),
            bits(&ids.zip(values).collect::<Vec<_>>())
        );
        assert_eq!(
            (ranks(&result).iterations, ranks(&result).residual),
            (done, residual)
        );
        assert_eq!(converged, k == 59);
        assert_eq!(result.complete, k == 59);
        if let Some(previous) = previous {
            assert_eq!(
                result.stats.vertex_operations - previous.vertex_operations,
                12
            );
            assert_eq!(result.stats.edges_examined - previous.edges_examined, 7);
            assert_eq!(result.stats.scratch_bytes, previous.scratch_bytes);
        }
        previous = Some(result.stats);
    }
    assert_eq!(
        previous.unwrap(),
        AlgorithmStats {
            elapsed_ms: 0,
            ..full.stats
        }
    );

    // Label propagation: one vertex unit per node and one edge unit per
    // collapsed neighbor entry (5 pairs) per sweep.
    let lpa = Algorithm::LabelPropagation;
    let mut previous: Option<AlgorithmStats> = None;
    for k in 1..=4 {
        let result = run_at(graph, &all, &lpa, &hops(0), &iterations(k), 0).unwrap();
        let (expected, sweeps, converged) = naive_communities(graph, (&nodes, &edges), k);
        let ids = nodes.iter().map(|node| node.0);
        assert_eq!(groups(&result), ids.zip(expected).collect::<Vec<_>>());
        assert_eq!(
            (communities(&result).sweeps, communities(&result).converged),
            (sweeps, converged)
        );
        assert_eq!(result.complete, k == 4);
        if let Some(previous) = previous {
            assert_eq!(
                result.stats.vertex_operations - previous.vertex_operations,
                6
            );
            assert_eq!(result.stats.edges_examined - previous.edges_examined, 10);
        }
        previous = Some(result.stats);
    }
}

#[test]
fn every_budget_stop_keeps_only_full_steps() {
    let db = slow_graph();
    let graph = db.graph();
    let all = everything(graph, Direction::Out);
    for algorithm in [rank(0.85, Some("w")), Algorithm::LabelPropagation] {
        let caps = iterations(100);
        let at = |limits: GraphLimits, caps: GraphAlgorithmLimits| {
            run_at(graph, &all, &algorithm, &limits, &caps, 0).unwrap()
        };
        let full = at(hops(0), caps);
        assert!(full.complete);
        let total = steps(&full);
        let capped: Vec<_> = (1..total)
            .map(|k| at(hops(0), iterations(k)).output)
            .collect();
        let spent = full.stats;
        // A stopped run returns no output before its first full step, else
        // exactly the output of its last full step.
        let stopped = |result: AlgorithmResult, reason| {
            assert_eq!(result.truncation, vec![reason]);
            assert!(!result.complete && !result.computation_complete);
            result.output.as_ref().map(|_| {
                let k = steps(&result);
                assert!(result.scope_complete && k < total);
                assert_eq!(result.output, capped[k as usize - 1]);
                k
            })
        };

        let mut seen = BTreeSet::new();
        for budget in 1..spent.vertex_operations {
            let caps = GraphAlgorithmLimits {
                max_vertex_operations: budget,
                ..caps
            };
            let result = at(hops(0), caps);
            assert_eq!(result.stats.vertex_operations, budget);
            seen.extend(stopped(result, AlgorithmTruncation::VertexLimit));
        }
        assert_eq!(seen, (1..total).collect());

        let mut seen = BTreeSet::new();
        for budget in 1..spent.edges_examined {
            let limits = GraphLimits {
                max_edges_examined: budget,
                ..hops(0)
            };
            seen.extend(stopped(at(limits, caps), AlgorithmTruncation::EdgeLimit));
        }
        assert_eq!(seen, (1..total).collect());

        // Every buffer is reserved before the first step.
        for budget in (1..spent.scratch_bytes).step_by(7) {
            let caps = GraphAlgorithmLimits {
                max_scratch_bytes: budget,
                ..caps
            };
            let result = at(hops(0), caps);
            assert_eq!(result.truncation, vec![AlgorithmTruncation::MemoryLimit]);
            assert_eq!(result.output, None);
        }

        let exact = at(
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
        assert_eq!(exact, full);
    }
}

/// Runs with `clock` as the monotonic clock and `cancel` as the flag.
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

#[test]
fn the_deadline_and_cancellation_keep_only_full_steps() {
    let db = slow_graph();
    let graph = db.graph();
    let all = everything(graph, Direction::Out);
    let caps = iterations(100);
    for algorithm in [rank(0.85, Some("w")), Algorithm::LabelPropagation] {
        let full = run_at(graph, &all, &algorithm, &hops(0), &caps, 0).unwrap();
        let total = full.stats.edges_examined + full.stats.vertex_operations;
        // The start, one read before every 64th operation and the final one.
        let reads = 2 + (total - 1) / 64;
        for late in 2..=reads {
            let count = Cell::new(0u64);
            let clock = || {
                count.set(count.get() + 1);
                if count.get() < late { 0 } else { 1_000 }
            };
            let result = clocked(graph, &all, &algorithm, &caps, &clock, None);
            assert_eq!(result.truncation, vec![AlgorithmTruncation::Deadline]);
            assert!(!result.computation_complete);
            let spent = result.stats.edges_examined + result.stats.vertex_operations;
            if late < reads {
                assert_eq!(spent, 64 * (late - 1));
                if result.output.is_some() {
                    assert!(steps(&result) < steps(&full));
                    let capped = run_at(
                        graph,
                        &all,
                        &algorithm,
                        &hops(0),
                        &iterations(steps(&result)),
                        0,
                    )
                    .unwrap();
                    assert_eq!(result.output, capped.output);
                }
            } else {
                // A late final read keeps the last full step, incomplete.
                assert_eq!(spent, total);
                assert_eq!(result.output, full.output);
            }
        }

        // Cancellation set by the third clock read stops before operation
        // 128; a flag set before the run stops before any work.
        let flag = AtomicBool::new(false);
        let count = Cell::new(0u64);
        let clock = || {
            count.set(count.get() + 1);
            if count.get() == 3 {
                flag.store(true, AtomicOrdering::Relaxed);
            }
            0
        };
        let result = clocked(graph, &all, &algorithm, &caps, &clock, Some(&flag));
        assert_eq!(result.truncation, vec![AlgorithmTruncation::Cancelled]);
        assert_eq!(
            result.stats.edges_examined + result.stats.vertex_operations,
            128
        );
        let flag = AtomicBool::new(true);
        let result = clocked(graph, &all, &algorithm, &caps, &|| 0, Some(&flag));
        assert_eq!(result.truncation, vec![AlgorithmTruncation::Cancelled]);
        assert_eq!(result.output, None);
        assert_eq!(
            result.stats.edges_examined + result.stats.vertex_operations,
            0
        );
    }

    // Steps over isolated nodes examine no edges and are still checked:
    // a late read inside the first step returns nothing.
    let (db, seeds) = isolated(300);
    let graph = db.graph();
    for (algorithm, per_step) in [(rank(0.85, None), 600), (Algorithm::LabelPropagation, 300)] {
        let full = run_at(graph, &seeds, &algorithm, &hops(0), &caps, 0).unwrap();
        // The default output cap of 100 rows cuts the 300 converged rows.
        assert!(full.computation_complete && steps(&full) == 1);
        assert_eq!(full.truncation, vec![AlgorithmTruncation::OutputLimit]);
        assert_eq!(full.stats.edges_examined, 0);
        let before = full.stats.vertex_operations - per_step;
        let late = before / 64 + 2;
        assert!(64 * (late - 1) < full.stats.vertex_operations);
        let count = Cell::new(0u64);
        let clock = || {
            count.set(count.get() + 1);
            if count.get() < late { 0 } else { 1_000 }
        };
        let result = clocked(graph, &seeds, &algorithm, &caps, &clock, None);
        assert_eq!(result.truncation, vec![AlgorithmTruncation::Deadline]);
        assert_eq!(result.stats.vertex_operations, 64 * (late - 1));
        assert!(result.scope_complete && result.output.is_none());
    }
}

/// A path n0→n1→…→n{count-1} of unweighted `R` edges.
fn chain(count: usize) -> TestGraph {
    let names: Vec<String> = (0..count).map(|index| format!("n{index}")).collect();
    let keys: Vec<&str> = names.iter().map(String::as_str).collect();
    let edges: Vec<_> = keys
        .windows(2)
        .map(|pair| (pair[0], pair[1], None))
        .collect();
    small(&keys, &edges)
}

#[test]
fn interrupted_staging_is_checked_and_keeps_the_last_full_step() {
    use super::super::iterative::STAGED;
    let staged = || STAGED.with(Cell::get);
    let reset = || STAGED.with(|count| count.set(0));
    let caps = iterations(100);
    // 300 nodes: the default output cap keeps 100 rows.
    for db in [chain(300), isolated(300).0] {
        let graph = db.graph();
        let all = everything(graph, Direction::Out);
        for algorithm in [rank(0.5, None), Algorithm::LabelPropagation] {
            reset();
            let full = run_at(graph, &all, &algorithm, &hops(0), &caps, 0).unwrap();
            let total = staged();
            assert!(full.computation_complete && full.scope_complete);
            assert_eq!(full.truncation, vec![AlgorithmTruncation::OutputLimit]);
            let rows = match &full.output {
                Some(AlgorithmOutput::PageRank(output)) => output.scores.len(),
                Some(AlgorithmOutput::LabelPropagation(output)) => output.assignments.len(),
                other => panic!("{other:?}"),
            };
            assert_eq!(rows, 100);
            reset();
            run_at(graph, &all, &algorithm, &hops(0), &iterations(1), 0).unwrap();
            let first = staged();
            assert!(first > 64 && total >= first);

            let mut points: BTreeSet<u64> = (1..=total).step_by(29).collect();
            for at in [
                1,
                63,
                64,
                65,
                first / 2,
                first,
                first + 1,
                first + 64,
                total,
            ] {
                points.insert(at.clamp(1, total));
            }
            let mut kept = BTreeSet::new();
            for inject in points {
                for cancel in [false, true] {
                    // The deadline passes, or the flag is set, once staging
                    // work `inject` has run; the clock is read only by checks.
                    let flag = AtomicBool::new(false);
                    let clock = || {
                        if staged() < inject {
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
                    let after = staged() - inject;
                    assert!(
                        after <= 64,
                        "{after} staging units ran unchecked at {inject}"
                    );
                    let reason = if cancel {
                        AlgorithmTruncation::Cancelled
                    } else {
                        AlgorithmTruncation::Deadline
                    };
                    assert_eq!(result.truncation, vec![reason]);
                    assert!(!result.computation_complete && result.scope_complete);
                    // Nothing before the first full step, else exactly the
                    // last full step.
                    kept.insert(result.output.as_ref().map(|_| steps(&result)));
                    if result.output.is_some() {
                        let k = steps(&result);
                        assert!(k <= steps(&full));
                        let capped =
                            run_at(graph, &all, &algorithm, &hops(0), &iterations(k), 0).unwrap();
                        assert_eq!(result.output, capped.output);
                    }
                }
            }
            assert!(kept.contains(&None), "{kept:?}");
            if steps(&full) > 1 {
                assert!(kept.contains(&Some(1)), "{kept:?}");
            }
        }
    }
}

#[test]
fn output_caps_cut_rows_of_iterative_results() {
    let db = slow_graph();
    let graph = db.graph();
    let all = everything(graph, Direction::Out);
    let caps = iterations(100);
    for algorithm in [rank(0.85, Some("w")), Algorithm::LabelPropagation] {
        let full = run_at(graph, &all, &algorithm, &hops(0), &caps, 0).unwrap();
        for cap in [0, 2, 5, 6] {
            let limits = GraphLimits {
                max_output_nodes: cap,
                ..hops(0)
            };
            let result = run_at(graph, &all, &algorithm, &limits, &caps, 0).unwrap();
            assert!(result.computation_complete && result.scope_complete);
            let expected = match (&full.output, cap) {
                (output, 6) => output.clone(),
                (Some(AlgorithmOutput::PageRank(output)), _) => {
                    let mut output = output.clone();
                    output.scores.truncate(cap as usize);
                    Some(AlgorithmOutput::PageRank(output))
                }
                (Some(AlgorithmOutput::LabelPropagation(output)), _) => {
                    let mut output = output.clone();
                    output.assignments.truncate(cap as usize);
                    Some(AlgorithmOutput::LabelPropagation(output))
                }
                (other, _) => panic!("{other:?}"),
            };
            assert_eq!(result.output, expected);
            if cap == 6 {
                assert!(result.complete);
            } else {
                assert_eq!(result.truncation, vec![AlgorithmTruncation::OutputLimit]);
            }
        }
        // Only the first reason is reported.
        let limits = GraphLimits {
            max_output_nodes: 2,
            ..hops(0)
        };
        let result = run_at(graph, &all, &algorithm, &limits, &iterations(1), 0).unwrap();
        assert_eq!(result.truncation, vec![AlgorithmTruncation::IterationLimit]);
        assert_eq!(steps(&result), 1);
    }
    let limits = GraphLimits {
        max_output_nodes: 2,
        ..hops(0)
    };
    let result = run_at(graph, &all, &rank(0.85, Some("w")), &limits, &caps, 0).unwrap();
    assert_eq!(scores(&result).len(), 2);
}

#[test]
fn invalid_iterative_requests_fail_in_order() {
    let db = small(&["a", "b", "c"], &[("a", "b", Some(int(1)))]);
    let graph = db.graph();
    let base = scope(&["a", "b"], Direction::Out);
    let invalid = |reason| Err(AlgorithmError::Graph(GraphError::InvalidQuery(reason)));
    let flag = AtomicBool::new(true);
    let clock = || 0;
    let cancelled = ExecutionContext {
        cancel: Some(&flag),
        ..ExecutionContext::new(VIEW, &clock)
    };
    let attempt = |algorithm: &Algorithm, caps: &GraphAlgorithmLimits| {
        run(graph, &base, algorithm, &hops(1), caps, &cancelled)
    };
    let page = |damping, tolerance, personalization, weight: Option<&str>| Algorithm::PageRank {
        damping,
        tolerance,
        personalization,
        weight_property: weight.map(str::to_owned),
    };

    // Shape errors precede the cancellation check.
    for algorithm in [
        Algorithm::Degree,
        Algorithm::LabelPropagation,
        rank(0.85, None),
    ] {
        assert_eq!(
            attempt(&algorithm, &iterations(0)),
            invalid("max_iterations must be at least 1")
        );
        assert_eq!(
            attempt(&algorithm, &iterations(101)),
            Err(AlgorithmError::Graph(GraphError::LimitExceedsCeiling {
                limit: "max_iterations",
                requested: 101,
                ceiling: 100
            }))
        );
    }
    for damping in [0.0, 1.0, -0.5, 1.5, f64::NAN, f64::INFINITY] {
        assert_eq!(
            attempt(&page(damping, 1e-8, None, None), &CAPS),
            invalid("damping must be finite and strictly between 0 and 1")
        );
    }
    for tolerance in [0.0, 1.0, -1e-8, f64::NAN, f64::NEG_INFINITY] {
        assert_eq!(
            attempt(&page(0.85, tolerance, None, None), &CAPS),
            invalid("tolerance must be finite and strictly between 0 and 1")
        );
    }
    assert_eq!(
        attempt(&page(0.85, 1e-8, Some(vec![]), None), &CAPS),
        invalid("personalization must not be empty")
    );
    assert_eq!(
        attempt(
            &page(0.85, 1e-8, Some(vec![(key("a"), 1.0); 10_001]), None),
            &CAPS
        ),
        Err(AlgorithmError::Graph(GraphError::LimitExceedsCeiling {
            limit: "personalization",
            requested: 10_001,
            ceiling: 10_000
        }))
    );
    assert_eq!(
        attempt(&page(0.85, 1e-8, None, Some("")), &CAPS),
        invalid("a weight property name must have 1..=256 bytes")
    );
    // Per-entry checks run under the meter: a cancelled run never reaches
    // them.
    let bad = page(0.85, 1e-8, Some(vec![(key("zz"), 1.0)]), None);
    let result = attempt(&bad, &CAPS).unwrap();
    assert_eq!(result.truncation, vec![AlgorithmTruncation::Cancelled]);

    let attempt = |algorithm: &Algorithm| run_at(graph, &base, algorithm, &hops(1), &CAPS, 0);
    let masses = |entries: &[(NodeSelector, f64)]| page(0.85, 1e-8, Some(entries.to_vec()), None);
    assert_eq!(
        attempt(&masses(&[(key("a"), 1.0), (key("zz"), 1.0)])),
        Err(AlgorithmError::Graph(GraphError::UnknownNodeKey(
            "zz".into()
        )))
    );
    assert_eq!(
        attempt(&masses(&[(NodeSelector::Id(NodeId(99)), 1.0)])),
        Err(AlgorithmError::Graph(GraphError::NodeNotFound(NodeId(99))))
    );
    for mass in [0.0, -1.0, f64::NAN, f64::INFINITY, -0.0] {
        assert_eq!(
            attempt(&masses(&[(key("a"), mass)])),
            invalid("personalization masses must be positive and finite")
        );
    }
    // c exists but is not a seed.
    assert_eq!(
        attempt(&masses(&[(key("a"), 1.0), (key("c"), 1.0)])),
        invalid("personalization nodes must be seeds")
    );
    // The same node by key and by ID.
    assert_eq!(
        attempt(&masses(&[
            (key("b"), 1.0),
            (NodeSelector::Id(db.id("b")), 2.0)
        ])),
        invalid("personalization must not repeat a node")
    );
    assert_eq!(
        attempt(&masses(&[(key("a"), f64::MAX), (key("b"), f64::MAX)])),
        Err(AlgorithmError::NumericOverflow)
    );
    // Normalization is exact for dyadic masses: tiny masses give the same
    // teleport as 1:3.
    let tiny = 2f64.powi(-1000);
    let small_masses = attempt(&masses(&[(key("a"), tiny), (key("b"), 3.0 * tiny)])).unwrap();
    let plain = attempt(&masses(&[(key("b"), 3.0), (key("a"), 1.0)])).unwrap();
    assert_eq!(small_masses.output, plain.output);
    assert!(plain.output.is_some());
}

#[test]
fn pagerank_validates_every_retained_weight() {
    let run_out = |db: &TestGraph, direction| {
        let graph = db.graph();
        run_at(
            graph,
            &everything(graph, direction),
            &rank(0.85, Some("w")),
            &hops(0),
            &CAPS,
            0,
        )
    };
    let db = small(
        &["a", "b", "c"],
        &[
            ("a", "b", Some(int(1))),
            ("b", "c", None),
            ("c", "a", Some(int(-1))),
        ],
    );
    let e = edge_ids(db.graph());
    assert_eq!(
        run_out(&db, Direction::Out),
        Err(AlgorithmError::MissingWeight(e[1]))
    );
    for (value, reason) in [
        (int(-1), "weights must be nonnegative"),
        (float(-0.5), "weights must be nonnegative"),
        (int((1 << 53) + 1), "integer weights must be at most 2^53"),
        (
            PropertyValue::String("1".into()),
            "weights must be integers or floats",
        ),
    ] {
        let db = small(
            &["a", "b"],
            &[("a", "b", Some(int(1))), ("b", "a", Some(value))],
        );
        let e = edge_ids(db.graph());
        assert_eq!(
            run_out(&db, Direction::Out),
            Err(AlgorithmError::InvalidWeight { edge: e[1], reason })
        );
    }

    // Out-weight sums must stay finite: a's two arcs overflow under Both
    // only.
    let db = small(
        &["a", "b", "c"],
        &[
            ("a", "b", Some(float(f64::MAX))),
            ("c", "a", Some(float(f64::MAX))),
        ],
    );
    assert!(run_out(&db, Direction::Out).unwrap().output.is_some());
    assert_eq!(
        run_out(&db, Direction::Both),
        Err(AlgorithmError::NumericOverflow)
    );
    // Every weight is validated before the sums are checked.
    let mut invalid = db.clone();
    invalid.run(&[edge(None, "c", "R", "b", props(&[("w", int(-2))]))]);
    let e = edge_ids(invalid.graph());
    assert_eq!(
        run_out(&invalid, Direction::Both),
        Err(AlgorithmError::InvalidWeight {
            edge: e[2],
            reason: "weights must be nonnegative"
        })
    );

    // Integers and floats of the same value rank identically.
    let integers = small(
        &["a", "b", "c"],
        &[
            ("a", "b", Some(int(3))),
            ("b", "c", Some(int(1))),
            ("c", "a", Some(int(2))),
        ],
    );
    let floats = small(
        &["a", "b", "c"],
        &[
            ("a", "b", Some(float(3.0))),
            ("b", "c", Some(float(1.0))),
            ("c", "a", Some(float(2.0))),
        ],
    );
    assert_eq!(
        run_out(&integers, Direction::Both).unwrap().output,
        run_out(&floats, Direction::Both).unwrap().output
    );
}

#[test]
fn iterative_scopes_use_one_ttl_clock() {
    let mut db = small(
        &["a", "b", "c"],
        &[("a", "b", Some(int(1))), ("b", "c", Some(int(1)))],
    );
    db.run(&[GraphMutation::SetNodeExpiry {
        key: "c".into(),
        expires_at_ms: Some(NOW),
        expected: Precondition::Any,
    }]);
    let graph = db.graph();
    let from_a = scope(&["a"], Direction::Out);
    let at = |algorithm: &Algorithm, now| run_at(graph, &from_a, algorithm, &hops(2), &CAPS, now);
    let ranked = rank(0.85, Some("w"));
    // Due at exactly NOW: c is invisible and hides b→c.
    let now = at(&ranked, NOW).unwrap();
    assert_eq!(scores(&now).len(), 2);
    assert_eq!(now.stats.edges_extracted, 1);
    let before = at(&ranked, NOW - 1).unwrap();
    assert_eq!(scores(&before).len(), 3);
    let lpa = at(&Algorithm::LabelPropagation, NOW).unwrap();
    assert_eq!(groups(&lpa).len(), 2);
    assert_eq!(
        at(&personalized(0.85, &[("c", 1.0)], None), NOW),
        Err(AlgorithmError::Graph(GraphError::UnknownNodeKey(
            "c".into()
        )))
    );
}
