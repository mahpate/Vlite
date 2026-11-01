//! Engine tests of the bounded local algorithms: hand-checked degrees and
//! paths, naive oracles over random graphs, exact budgets, the deadline,
//! cancellation, TTL, validation, weights and determinism.

use super::super::codec::{GraphBody, GraphEntry};
use super::super::limits::MAX_BATCH_ENTRIES;
use super::super::model::{Precondition, Properties};
use super::super::mutate::{EdgeSelector, GraphMutation};
use super::super::testkit::{
    G, Rng, TYPES, TestGraph, VIEW, edge, hub_graph, int, node, props, random_graph,
};
use super::*;
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::AtomicBool;

fn key(key: &str) -> NodeSelector {
    NodeSelector::Key(key.into())
}

/// An owned type or label list.
fn names(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

fn scope(seeds: &[&str], direction: Direction) -> AlgorithmScope {
    AlgorithmScope {
        seeds: seeds.iter().map(|seed| key(seed)).collect(),
        direction,
        types: None,
        labels: None,
        edge_conditions: Vec::new(),
    }
}

fn path(source: &str, destination: &str, weight: Option<&str>) -> Algorithm {
    Algorithm::WeightedShortestPath {
        source: key(source),
        destination: key(destination),
        weight_property: weight.map(str::to_owned),
    }
}

fn hops(max_hops: u64) -> GraphLimits {
    GraphLimits {
        max_hops,
        ..GraphLimits::DEFAULT
    }
}

const CAPS: GraphAlgorithmLimits = GraphAlgorithmLimits::DEFAULT;

/// Checks the invariants every result must satisfy.
fn check(result: &AlgorithmResult, now_ms: u64) {
    assert_eq!(result.complete, result.truncation.is_empty());
    assert!(result.truncation.len() <= 1, "{:?}", result.truncation);
    assert_eq!(result.view_sequence, VIEW);
    assert_eq!(result.read_time_ms, now_ms);
    assert!(result.seeds.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(result.scope_complete, result.projection.is_some());
    assert!(result.scope_complete || !result.computation_complete);
    if !result.computation_complete {
        // Only iterative outputs may be partial: their last full step, or
        // the sources betweenness completed.
        match &result.output {
            None => {}
            Some(
                AlgorithmOutput::PageRank(_)
                | AlgorithmOutput::LabelPropagation(_)
                | AlgorithmOutput::Betweenness(_),
            ) => {
                assert!(result.scope_complete && !result.truncation.is_empty());
            }
            Some(other) => panic!("partial output {other:?}"),
        }
    }
    if result.complete {
        assert!(result.output.is_some());
    }
    let rows: Option<Vec<u64>> = match &result.output {
        Some(AlgorithmOutput::PageRank(output)) => {
            assert!(output.iterations >= 1 && output.residual >= 0.0);
            Some(output.scores.iter().map(|row| row.node_id).collect())
        }
        Some(AlgorithmOutput::LabelPropagation(output)) => {
            assert!(output.sweeps >= 1);
            Some(output.assignments.iter().map(|row| row.node_id).collect())
        }
        Some(AlgorithmOutput::Betweenness(output)) => {
            let k = output.completed_source_count;
            assert!(k >= 1);
            assert_eq!(output.completed_sources.len() as u64, k);
            assert!(
                output
                    .selected_sources
                    .starts_with(&output.completed_sources)
            );
            assert!(output.selected_sources.len() as u64 <= result.stats.nodes_extracted);
            assert_eq!(output.estimated, k < result.stats.nodes_extracted);
            assert!(output.scores.iter().all(|row| row.score.is_finite()));
            Some(output.scores.iter().map(|row| row.node_id).collect())
        }
        _ => None,
    };
    if let Some(rows) = rows {
        assert!(rows.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(rows.len() as u64 <= result.stats.nodes_extracted);
    }
    if let Some(projection) = &result.projection {
        assert!(projection.nodes.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(
            projection
                .edges
                .windows(2)
                .all(|pair| pair[0].id < pair[1].id)
        );
        assert!(
            result
                .seeds
                .iter()
                .all(|seed| projection.nodes.binary_search(seed).is_ok())
        );
        assert_eq!(result.stats.nodes_extracted, projection.nodes.len() as u64);
        assert_eq!(result.stats.edges_extracted, projection.edges.len() as u64);
    } else {
        assert_eq!(result.stats.nodes_extracted, 0);
        assert_eq!(result.stats.edges_extracted, 0);
    }
    assert!(
        result.stats.scratch_bytes >= held_bytes(result),
        "reserved {} < held {}",
        result.stats.scratch_bytes,
        held_bytes(result)
    );
}

/// Heap bytes the buffers of `result` actually hold, at their allocated
/// capacity. Reservations are cumulative, so they must cover these.
fn held_bytes(result: &AlgorithmResult) -> u64 {
    let projection = result.projection.as_ref().map_or(0, |projection| {
        projection.nodes.capacity() * size_of::<NodeId>()
            + projection.edges.capacity() * size_of::<ProjectedEdge>()
    });
    let output = match &result.output {
        Some(AlgorithmOutput::Degree(rows)) => rows.capacity() * size_of::<DegreeRow>(),
        Some(AlgorithmOutput::WeightedShortestPath(WeightedPathOutcome::Found(path))) => {
            path.nodes.capacity() * size_of::<NodeId>()
                + path.edges.capacity() * size_of::<EdgeId>()
        }
        Some(AlgorithmOutput::PageRank(output)) => {
            output.scores.capacity() * size_of::<NodeScore>()
        }
        Some(AlgorithmOutput::LabelPropagation(output)) => {
            output.assignments.capacity() * size_of::<CommunityAssignment>()
        }
        Some(AlgorithmOutput::Betweenness(output)) => {
            output.scores.capacity() * size_of::<NodeScore>()
                + (output.selected_sources.capacity() + output.completed_sources.capacity())
                    * size_of::<u64>()
        }
        _ => 0,
    };
    (result.seeds.capacity() * size_of::<NodeId>() + projection + output) as u64
}

/// Runs at TTL clock `now_ms` with a monotonic clock fixed at 0.
fn run_at(
    graph: &Graph,
    scope: &AlgorithmScope,
    algorithm: &Algorithm,
    limits: &GraphLimits,
    caps: &GraphAlgorithmLimits,
    now_ms: u64,
) -> Result<AlgorithmResult, AlgorithmError> {
    let clock = || 0;
    let context = ExecutionContext {
        now_ms,
        ..ExecutionContext::new(VIEW, &clock)
    };
    let result = run(graph, scope, algorithm, limits, caps, &context);
    if let Ok(result) = &result {
        check(result, now_ms);
    }
    result
}

fn exec(
    graph: &Graph,
    scope: &AlgorithmScope,
    algorithm: &Algorithm,
    limits: &GraphLimits,
) -> AlgorithmResult {
    run_at(graph, scope, algorithm, limits, &CAPS, 0).unwrap()
}

fn degrees(result: &AlgorithmResult) -> Vec<(NodeId, u64, u64, u64)> {
    match &result.output {
        Some(AlgorithmOutput::Degree(rows)) => rows
            .iter()
            .map(|row| (row.node, row.in_degree, row.out_degree, row.incident_degree))
            .collect(),
        other => panic!("not degrees: {other:?}"),
    }
}

fn found(result: &AlgorithmResult) -> &WeightedPath {
    match &result.output {
        Some(AlgorithmOutput::WeightedShortestPath(WeightedPathOutcome::Found(path))) => path,
        other => panic!("no path: {other:?}"),
    }
}

fn no_path(result: &AlgorithmResult) -> bool {
    result.output
        == Some(AlgorithmOutput::WeightedShortestPath(
            WeightedPathOutcome::NoPathWithinScopeAndHops,
        ))
}

fn edge_ids(graph: &Graph) -> Vec<EdgeId> {
    graph.edges().map(|edge| edge.id).collect()
}

fn projected(result: &AlgorithmResult) -> (Vec<NodeId>, Vec<EdgeId>) {
    let projection = result.projection.as_ref().unwrap();
    (
        projection.nodes.clone(),
        projection.edges.iter().map(|edge| edge.id).collect(),
    )
}

/// a→b twice (R, w 1 and 5), b→c (S, w 2), the loop c→c (R, w 0) and c→a
/// (T, w 9); d is isolated. Edge IDs follow creation order.
fn hand_graph() -> TestGraph {
    let mut db = TestGraph::new();
    db.run(&[
        node("a", &["A"], Properties::new()),
        node("b", &["A"], Properties::new()),
        node("c", &["A", "B"], Properties::new()),
        node("d", &[], Properties::new()),
        edge(None, "a", "R", "b", props(&[("w", int(1))])),
        edge(None, "a", "R", "b", props(&[("w", int(5))])),
        edge(None, "b", "S", "c", props(&[("w", int(2))])),
        edge(None, "c", "R", "c", props(&[("w", int(0))])),
        edge(None, "c", "T", "a", props(&[("w", int(9))])),
    ]);
    db
}

#[test]
fn degree_counts_physical_edges_per_direction() {
    let db = hand_graph();
    let graph = db.graph();
    let [a, b, c, d] = ["a", "b", "c", "d"].map(|name| db.id(name));
    let e = edge_ids(graph);
    assert_eq!(e.len(), 5);

    let out = exec(
        graph,
        &scope(&["d", "a"], Direction::Out),
        &Algorithm::Degree,
        &hops(2),
    );
    assert!(out.complete && out.computation_complete && out.scope_complete);
    assert_eq!(out.seeds, vec![a, d]);
    assert_eq!(projected(&out), (vec![a, b, c, d], e.clone()));
    assert_eq!(
        degrees(&out),
        vec![(a, 1, 2, 3), (b, 2, 1, 3), (c, 2, 2, 3), (d, 0, 0, 0)]
    );
    assert_eq!(out.stats.arcs, 5);
    assert_eq!(out.stats.nodes_extracted, 4);
    assert_eq!(out.stats.edges_extracted, 5);
    assert!(out.stats.scratch_bytes > 0 && out.stats.vertex_operations > 0);

    // In reverses every edge: in and out swap, incident stays.
    let inward = exec(
        graph,
        &scope(&["a", "d"], Direction::In),
        &Algorithm::Degree,
        &hops(2),
    );
    assert_eq!(projected(&inward), (vec![a, b, c, d], e.clone()));
    assert_eq!(
        degrees(&inward),
        vec![(a, 2, 1, 3), (b, 1, 2, 3), (c, 2, 2, 3), (d, 0, 0, 0)]
    );
    assert_eq!(inward.stats.arcs, 5);

    // Both keeps the stored orientation and counts a non-loop edge as two
    // arcs but one physical edge.
    let both = exec(
        graph,
        &scope(&["a", "d"], Direction::Both),
        &Algorithm::Degree,
        &hops(1),
    );
    assert_eq!(projected(&both), (vec![a, b, c, d], e.clone()));
    assert_eq!(degrees(&both), degrees(&out));
    assert_eq!(both.stats.arcs, 9);

    // One hop out of a reaches b only; c and its edges stay outside.
    let near = exec(
        graph,
        &scope(&["a"], Direction::Out),
        &Algorithm::Degree,
        &hops(1),
    );
    assert_eq!(projected(&near), (vec![a, b], e[..2].to_vec()));
    assert_eq!(degrees(&near), vec![(a, 0, 2, 2), (b, 2, 0, 2)]);
    // Zero hops keep the seeds and the edges between them.
    let alone = exec(
        graph,
        &scope(&["c"], Direction::Out),
        &Algorithm::Degree,
        &hops(0),
    );
    assert_eq!(projected(&alone), (vec![c], vec![e[3]]));
    assert_eq!(degrees(&alone), vec![(c, 1, 1, 1)]);
    let alone = exec(
        graph,
        &scope(&["a"], Direction::Both),
        &Algorithm::Degree,
        &hops(0),
    );
    assert_eq!(projected(&alone), (vec![a], vec![]));
    assert_eq!(degrees(&alone), vec![(a, 0, 0, 0)]);
}

#[test]
fn scope_filters_apply_to_seeds_intermediates_and_edges() {
    let db = hand_graph();
    let graph = db.graph();
    let [a, b, c] = ["a", "b", "c"].map(|name| db.id(name));
    let e = edge_ids(graph);

    // Only R: b's S edge never leads to c.
    let mut only_r = scope(&["a"], Direction::Out);
    only_r.types = Some(names(&["R"]));
    let result = exec(graph, &only_r, &Algorithm::Degree, &hops(4));
    assert_eq!(projected(&result), (vec![a, b], e[..2].to_vec()));
    // Types also filter the induced edges: e2 (S) and e4 (T) stay out.
    only_r.seeds = vec![key("c"), key("a")];
    only_r.direction = Direction::Both;
    let result = exec(graph, &only_r, &Algorithm::Degree, &hops(4));
    assert_eq!(projected(&result), (vec![a, b, c], vec![e[0], e[1], e[3]]));

    // Labels A and B: only c qualifies, so a and b never bridge.
    let mut ab = scope(&["c"], Direction::Both);
    ab.labels = Some(names(&["A", "B"]));
    let result = exec(graph, &ab, &Algorithm::Degree, &hops(4));
    assert_eq!(projected(&result), (vec![c], vec![e[3]]));
    assert_eq!(degrees(&result), vec![(c, 1, 1, 1)]);
    ab.seeds = vec![key("a")];
    assert_eq!(
        run_at(graph, &ab, &Algorithm::Degree, &hops(4), &CAPS, 0),
        Err(AlgorithmError::Graph(GraphError::InvalidQuery(
            "every seed must carry every scope label"
        )))
    );

    // Edge conditions filter discovery and the induced edges alike.
    let mut light = scope(&["a"], Direction::Both);
    light.edge_conditions = vec![PropertyCondition::Range {
        field: "w".into(),
        min: Some(int(1)),
        max: Some(int(2)),
    }];
    let result = exec(graph, &light, &Algorithm::Degree, &hops(4));
    assert_eq!(projected(&result), (vec![a, b, c], vec![e[0], e[2]]));
    assert_eq!(
        degrees(&result),
        vec![(a, 0, 1, 1), (b, 1, 1, 2), (c, 1, 0, 1)]
    );
}

#[test]
fn weighted_paths_order_by_cost_then_hops_then_edge_ids() {
    let db = hand_graph();
    let graph = db.graph();
    let [a, b, c] = ["a", "b", "c"].map(|name| db.id(name));
    let e = edge_ids(graph);
    let out = scope(&["a"], Direction::Out);

    let result = exec(graph, &out, &path("a", "c", Some("w")), &hops(3));
    assert_eq!(result.endpoints, Some((a, c)));
    let best = found(&result);
    assert_eq!(
        (best.cost, &best.nodes, &best.edges),
        (3.0, &vec![a, b, c], &vec![e[0], e[2]])
    );
    // The zero-weight loop at c never lengthens the winner.
    assert_eq!(best.edges.len(), 2);
    // Unit weights tie e0 and e1; the smaller edge-ID sequence wins.
    let unit = exec(graph, &out, &path("a", "c", None), &hops(2));
    assert_eq!(found(&unit).cost, 2.0);
    assert_eq!(found(&unit).edges, vec![e[0], e[2]]);
    // The source alone is a path of cost 0.
    let here = exec(graph, &out, &path("a", "a", Some("w")), &hops(2));
    assert_eq!(
        found(&here),
        &WeightedPath {
            cost: 0.0,
            nodes: vec![a],
            edges: vec![]
        }
    );
    // c lies outside one hop: certified absence, not an error.
    let short = exec(graph, &out, &path("a", "c", Some("w")), &hops(1));
    assert!(short.complete && no_path(&short));
    // In walks edges backwards: from a over c→a.
    let back = exec(
        graph,
        &scope(&["a"], Direction::In),
        &path("a", "c", Some("w")),
        &hops(2),
    );
    assert_eq!(found(&back).edges, vec![e[4]]);
    assert_eq!(found(&back).nodes, vec![a, c]);
    assert_eq!(found(&back).cost, 9.0);
    // Both prefers the cheaper two-hop path over the direct expensive edge.
    let both = exec(
        graph,
        &scope(&["a"], Direction::Both),
        &path("a", "c", Some("w")),
        &hops(2),
    );
    assert_eq!(found(&both).edges, vec![e[0], e[2]]);
    // The source must be a seed; the destination must resolve.
    assert_eq!(
        run_at(graph, &out, &path("b", "c", None), &hops(2), &CAPS, 0),
        Err(AlgorithmError::Graph(GraphError::InvalidQuery(
            "the path source must be a seed"
        )))
    );
    assert_eq!(
        run_at(graph, &out, &path("a", "zz", None), &hops(2), &CAPS, 0),
        Err(AlgorithmError::Graph(GraphError::UnknownNodeKey(
            "zz".into()
        )))
    );
}

/// s→m→x costs 2 in two hops, s→x costs 5 in one; x→t costs 1. With two
/// hops the only feasible path keeps the costlier, shorter prefix.
#[test]
fn a_cheaper_prefix_that_exhausts_the_hops_does_not_hide_a_feasible_path() {
    let mut db = TestGraph::new();
    db.run(&[
        node("s", &[], Properties::new()),
        node("m", &[], Properties::new()),
        node("x", &[], Properties::new()),
        node("t", &[], Properties::new()),
        edge(None, "s", "R", "m", props(&[("w", int(1))])),
        edge(None, "m", "R", "x", props(&[("w", int(1))])),
        edge(None, "s", "R", "x", props(&[("w", int(5))])),
        edge(None, "x", "R", "t", props(&[("w", int(1))])),
    ]);
    let graph = db.graph();
    let e = edge_ids(graph);
    let out = scope(&["s"], Direction::Out);
    let two = exec(graph, &out, &path("s", "t", Some("w")), &hops(2));
    assert_eq!(
        (found(&two).cost, &found(&two).edges),
        (6.0, &vec![e[2], e[3]])
    );
    let three = exec(graph, &out, &path("s", "t", Some("w")), &hops(3));
    assert_eq!(
        (found(&three).cost, &found(&three).edges),
        (3.0, &vec![e[0], e[1], e[3]])
    );
    let one = exec(graph, &out, &path("s", "t", Some("w")), &hops(1));
    assert!(no_path(&one));
}

const NOW: u64 = 1_000;

fn visible(graph: &Graph, id: NodeId) -> bool {
    graph.node_expiry(id).is_none_or(|at| at > NOW)
}

/// Whether `carried` holds every one of `labels`.
fn carries_all(labels: &[String], carried: &BTreeSet<String>) -> bool {
    labels.iter().all(|label| carried.contains(label))
}

/// The projection by definition: breadth-first layers over eligible arcs,
/// then every eligible edge between admitted nodes.
fn naive_projection(
    graph: &Graph,
    scope: &AlgorithmScope,
    seeds: &[NodeId],
    max_hops: u64,
) -> (Vec<NodeId>, Vec<EdgeId>) {
    let eligible_node = |id: NodeId| {
        visible(graph, id)
            && scope
                .labels
                .as_ref()
                .is_none_or(|labels| carries_all(labels, &graph.node(id).unwrap().labels))
    };
    let eligible: Vec<&GraphEdge> = graph
        .edges()
        .filter(|edge| {
            graph.edge_expiry(edge.id).is_none_or(|at| at > NOW)
                && eligible_node(edge.source)
                && eligible_node(edge.destination)
                && scope
                    .types
                    .as_ref()
                    .is_none_or(|types| types.contains(&edge.relationship))
                && conditions_match(&edge.properties, &scope.edge_conditions)
        })
        .collect();
    let mut admitted: BTreeSet<NodeId> = seeds.iter().copied().collect();
    let mut frontier = admitted.clone();
    for _ in 0..max_hops {
        let mut next = BTreeSet::new();
        for edge in &eligible {
            let (u, v) = (edge.source, edge.destination);
            let mut arcs = vec![];
            if scope.direction != Direction::In {
                arcs.push((u, v));
            }
            if scope.direction != Direction::Out {
                arcs.push((v, u));
            }
            for (from, to) in arcs {
                if frontier.contains(&from) && !admitted.contains(&to) {
                    next.insert(to);
                }
            }
        }
        admitted.extend(next.iter().copied());
        frontier = next;
    }
    let edges = eligible
        .iter()
        .filter(|edge| admitted.contains(&edge.source) && admitted.contains(&edge.destination))
        .map(|edge| edge.id)
        .collect();
    (admitted.into_iter().collect(), edges)
}

fn naive_degrees(
    graph: &Graph,
    nodes: &[NodeId],
    edges: &[EdgeId],
    direction: Direction,
) -> Vec<(NodeId, u64, u64, u64)> {
    nodes
        .iter()
        .map(|&id| {
            let mut row = (id, 0, 0, 0);
            for edge in edges.iter().map(|edge| graph.edge(*edge).unwrap()) {
                let (tail, head) = if direction == Direction::In {
                    (edge.destination, edge.source)
                } else {
                    (edge.source, edge.destination)
                };
                row.1 += u64::from(head == id);
                row.2 += u64::from(tail == id);
                row.3 += u64::from(head == id || tail == id);
            }
            row
        })
        .collect()
}

type NaivePath = (u64, usize, Vec<EdgeId>, Vec<NodeId>);

/// The cheapest walk by layered dynamic programming over exact integer
/// costs: `Err` names the smallest retained edge without an integer weight.
fn naive_path(
    graph: &Graph,
    (nodes, edges): (&[NodeId], &[EdgeId]),
    direction: Direction,
    weight: Option<&str>,
    (source, destination): (NodeId, NodeId),
    max_hops: u64,
) -> Result<Option<NaivePath>, EdgeId> {
    let mut arcs = Vec::new();
    for &id in edges {
        let edge = graph.edge(id).unwrap();
        let cost = match weight {
            None => 1,
            Some(field) => match edge.properties.get(field) {
                Some(PropertyValue::Integer(value)) => *value as u64,
                _ => return Err(id),
            },
        };
        let (u, v) = if direction == Direction::In {
            (edge.destination, edge.source)
        } else {
            (edge.source, edge.destination)
        };
        arcs.push((u, v, id, cost));
        if direction == Direction::Both && u != v {
            arcs.push((v, u, id, cost));
        }
    }
    if !nodes.contains(&destination) {
        return Ok(None);
    }
    let mut layer: BTreeMap<NodeId, (u64, Vec<EdgeId>, Vec<NodeId>)> =
        BTreeMap::from([(source, (0, vec![], vec![source]))]);
    let mut best: Option<NaivePath> = None;
    for h in 0..=max_hops as usize {
        if let Some((cost, edges, nodes)) = layer.get(&destination) {
            let candidate = (*cost, h, edges.clone(), nodes.clone());
            if best.as_ref().is_none_or(|best| candidate < *best) {
                best = Some(candidate);
            }
        }
        let mut next: BTreeMap<NodeId, (u64, Vec<EdgeId>, Vec<NodeId>)> = BTreeMap::new();
        for (node, (cost, edges, nodes)) in &layer {
            for &(_, to, id, weight) in arcs.iter().filter(|arc| arc.0 == *node) {
                let mut candidate = (cost + weight, edges.clone(), nodes.clone());
                candidate.1.push(id);
                candidate.2.push(to);
                let slot = next.entry(to).or_insert_with(|| candidate.clone());
                if (candidate.0, &candidate.1) < (slot.0, &slot.1) {
                    *slot = candidate;
                }
            }
        }
        layer = next;
    }
    Ok(best)
}

fn deadlines(db: &mut TestGraph, rng: &mut Rng) {
    let graph = db.graph();
    let mut mutations = Vec::new();
    for node in graph.nodes() {
        if rng.chance(12) {
            mutations.push(GraphMutation::SetNodeExpiry {
                key: node.key.clone(),
                expires_at_ms: Some(NOW - 1 + rng.below(3)),
                expected: Precondition::Any,
            });
        }
    }
    for edge in graph.edges() {
        if rng.chance(12) {
            mutations.push(GraphMutation::SetEdgeExpiry {
                edge: EdgeSelector::Id(edge.id),
                expires_at_ms: Some(NOW - 1 + rng.below(3)),
                expected: Precondition::Any,
            });
        }
    }
    if !mutations.is_empty() {
        db.run(&mutations);
    }
}

#[test]
fn random_scopes_match_naive_oracles() {
    let mut paths = [0u32; 3];
    for seed in 0..400 {
        let mut db = random_graph(seed);
        let mut rng = Rng::new(seed ^ 0xa160);
        if rng.chance(40) {
            deadlines(&mut db, &mut rng);
        }
        let graph = db.graph();
        let direction = rng.pick(&[Direction::Out, Direction::In, Direction::Both]);
        let types = rng
            .chance(40)
            .then(|| names(&[rng.pick(&TYPES), rng.pick(&TYPES)]));
        let labels = match rng.below(3) {
            0 => None,
            1 => Some(names(&["A"])),
            _ => Some(names(&["A", "B"])),
        };
        let edge_conditions = if rng.chance(30) {
            vec![PropertyCondition::Range {
                field: "w".into(),
                min: Some(int(1)),
                max: None,
            }]
        } else {
            vec![]
        };
        let max_hops = rng.below(5);
        let candidates: Vec<&GraphNode> = graph
            .nodes()
            .filter(|node| {
                visible(graph, node.id)
                    && labels
                        .as_ref()
                        .is_none_or(|labels| carries_all(labels, &node.labels))
            })
            .collect();
        let live: Vec<NodeId> = graph
            .nodes()
            .map(|node| node.id)
            .filter(|id| visible(graph, *id))
            .collect();
        if candidates.is_empty() {
            continue;
        }
        let mut seeds = Vec::new();
        let mut selectors = Vec::new();
        for _ in 0..1 + rng.below(4) {
            let node = candidates[rng.below(candidates.len() as u64) as usize];
            seeds.push(node.id);
            selectors.push(if rng.chance(50) {
                NodeSelector::Id(node.id)
            } else {
                NodeSelector::Key(node.key.clone())
            });
        }
        let scope = AlgorithmScope {
            seeds: selectors,
            direction,
            types,
            labels,
            edge_conditions,
        };
        seeds.sort();
        seeds.dedup();
        let limits = hops(max_hops);
        let (nodes, edges) = naive_projection(graph, &scope, &seeds, max_hops);

        let degree = run_at(graph, &scope, &Algorithm::Degree, &limits, &CAPS, NOW).unwrap();
        assert!(degree.complete, "seed {seed}: {:?}", degree.truncation);
        assert_eq!(degree.seeds, seeds);
        assert_eq!(
            projected(&degree),
            (nodes.clone(), edges.clone()),
            "seed {seed}"
        );
        assert_eq!(
            degrees(&degree),
            naive_degrees(graph, &nodes, &edges, direction)
        );
        let loops = edges
            .iter()
            .filter(|id| {
                let edge = graph.edge(**id).unwrap();
                edge.source == edge.destination
            })
            .count() as u64;
        let arcs = edges.len() as u64 * if direction == Direction::Both { 2 } else { 1 };
        let arcs = arcs
            - if direction == Direction::Both {
                loops
            } else {
                0
            };
        assert_eq!(degree.stats.arcs, arcs);

        let source = seeds[rng.below(seeds.len() as u64) as usize];
        let destination = live[rng.below(live.len() as u64) as usize];
        let weight = rng.chance(70).then_some("w");
        let algorithm = Algorithm::WeightedShortestPath {
            source: NodeSelector::Id(source),
            destination: NodeSelector::Id(destination),
            weight_property: weight.map(str::to_owned),
        };
        let result = run_at(graph, &scope, &algorithm, &limits, &CAPS, NOW);
        let expected = naive_path(
            graph,
            (&nodes, &edges),
            direction,
            weight,
            (source, destination),
            max_hops,
        );
        match expected {
            Err(edge) => {
                paths[0] += 1;
                assert_eq!(
                    result,
                    Err(AlgorithmError::MissingWeight(edge)),
                    "seed {seed}"
                );
            }
            Ok(None) => {
                paths[1] += 1;
                let result = result.unwrap();
                assert!(
                    result.complete && no_path(&result),
                    "seed {seed}: {result:?}"
                );
            }
            Ok(Some((cost, _, edges, nodes))) => {
                paths[2] += 1;
                let result = result.unwrap();
                assert!(result.complete);
                let path = found(&result);
                assert_eq!(
                    (path.cost, &path.edges, &path.nodes),
                    (cost as f64, &edges, &nodes),
                    "seed {seed}"
                );
            }
        }
    }
    assert!(paths.iter().all(|count| *count > 20), "{paths:?}");
}

#[test]
fn every_budget_is_exact_and_stops_without_output() {
    let db = hand_graph();
    let graph = db.graph();
    let cases = [
        (scope(&["a", "d"], Direction::Both), Algorithm::Degree),
        (scope(&["a"], Direction::Out), path("a", "c", Some("w"))),
        (scope(&["a"], Direction::Both), path("a", "c", None)),
    ];
    for (scope, algorithm) in cases {
        let limits = hops(2);
        let full = exec(graph, &scope, &algorithm, &limits);
        let spent = full.stats;
        let at = |limits: GraphLimits, caps: GraphAlgorithmLimits| {
            run_at(graph, &scope, &algorithm, &limits, &caps, 0).unwrap()
        };
        let edges = |budget| GraphLimits {
            max_edges_examined: budget,
            ..limits
        };
        assert_eq!(at(edges(spent.edges_examined), CAPS), full);
        let short = at(edges(spent.edges_examined - 1), CAPS);
        assert_eq!(short.truncation, vec![AlgorithmTruncation::EdgeLimit]);
        assert_eq!(short.stats.edges_examined, spent.edges_examined - 1);

        let vertices = |budget| GraphAlgorithmLimits {
            max_vertex_operations: budget,
            ..CAPS
        };
        assert_eq!(at(limits, vertices(spent.vertex_operations)), full);
        let short = at(limits, vertices(spent.vertex_operations - 1));
        assert_eq!(short.truncation, vec![AlgorithmTruncation::VertexLimit]);
        assert_eq!(short.stats.vertex_operations, spent.vertex_operations - 1);

        let bytes = |budget| GraphAlgorithmLimits {
            max_scratch_bytes: budget,
            ..CAPS
        };
        assert_eq!(at(limits, bytes(spent.scratch_bytes)), full);
        let short = at(limits, bytes(spent.scratch_bytes - 1));
        assert_eq!(short.truncation, vec![AlgorithmTruncation::MemoryLimit]);
        assert!(short.stats.scratch_bytes < spent.scratch_bytes);
        // The last unit is spent computing, after extraction finished:
        // still no partial analysis.
        for short in [
            at(edges(spent.edges_examined - 1), CAPS),
            at(limits, vertices(spent.vertex_operations - 1)),
        ] {
            assert!(short.scope_complete && !short.computation_complete);
            assert_eq!(short.output, None);
        }
    }

    // Discovery admits a, d, b and c.
    let both = scope(&["a", "d"], Direction::Out);
    let visited = |max_visited_nodes| GraphLimits {
        max_visited_nodes,
        ..hops(2)
    };
    let result = run_at(graph, &both, &Algorithm::Degree, &visited(4), &CAPS, 0).unwrap();
    assert!(result.complete);
    let result = run_at(graph, &both, &Algorithm::Degree, &visited(3), &CAPS, 0).unwrap();
    assert_eq!(result.truncation, vec![AlgorithmTruncation::VisitedLimit]);
    assert!(!result.scope_complete && result.output.is_none());
    assert_eq!(
        run_at(graph, &both, &Algorithm::Degree, &visited(1), &CAPS, 0),
        Err(AlgorithmError::Graph(GraphError::InvalidQuery(
            "distinct seeds exceed max_visited_nodes"
        )))
    );

    // The per-node cap counts every adjacency entry the scan reads.
    let db = hub_graph(3, 20);
    let hub = scope(&["hub"], Direction::Out);
    let per_node = |max_edges_per_node| GraphLimits {
        max_edges_per_node,
        ..hops(1)
    };
    let result = run_at(
        db.graph(),
        &hub,
        &Algorithm::Degree,
        &per_node(20),
        &CAPS,
        0,
    )
    .unwrap();
    assert!(result.complete);
    assert_eq!(degrees(&result)[0], (NodeId(0), 0, 20, 20));
    let result = run_at(
        db.graph(),
        &hub,
        &Algorithm::Degree,
        &per_node(19),
        &CAPS,
        0,
    )
    .unwrap();
    assert_eq!(
        result.truncation,
        vec![AlgorithmTruncation::DegreeLimit {
            node_ids: vec![NodeId(0)]
        }]
    );
    assert!(result.output.is_none());
}

/// Runs with `clock` as the monotonic clock and `cancel` as the flag.
fn timed(
    graph: &Graph,
    scope: &AlgorithmScope,
    algorithm: &Algorithm,
    limits: &GraphLimits,
    clock: &dyn Fn() -> u64,
    cancel: Option<&AtomicBool>,
) -> AlgorithmResult {
    let context = ExecutionContext {
        cancel,
        ..ExecutionContext::new(VIEW, clock)
    };
    let result = run(graph, scope, algorithm, limits, &CAPS, &context).unwrap();
    check(&result, 0);
    result
}

fn isolated(count: u64) -> (TestGraph, AlgorithmScope) {
    let mut db = TestGraph::new();
    let nodes: Vec<GraphMutation> = (0..count)
        .map(|index| node(&format!("n{index}"), &[], Properties::new()))
        .collect();
    db.run(&nodes);
    let mut scope = scope(&[], Direction::Out);
    scope.seeds = db
        .graph()
        .nodes()
        .map(|node| NodeSelector::Id(node.id))
        .collect();
    (db, scope)
}

#[test]
fn the_deadline_is_checked_every_64_operations_and_at_the_end() {
    // Start reads 10; the checks before operations 64, 128 and 192 read 20,
    // 30 and 40, which is 30 ms after the start.
    let db = hub_graph(10, 500);
    let now = Cell::new(0u64);
    let clock = || {
        now.set(now.get() + 10);
        now.get()
    };
    let limits = GraphLimits {
        deadline_ms: 25,
        ..hops(1)
    };
    let hub = scope(&["hub"], Direction::Both);
    let result = timed(db.graph(), &hub, &Algorithm::Degree, &limits, &clock, None);
    assert_eq!(result.truncation, vec![AlgorithmTruncation::Deadline]);
    assert_eq!(
        result.stats.edges_examined + result.stats.vertex_operations,
        192
    );
    assert_eq!(result.stats.elapsed_ms, 40);

    // Loops that examine no edges are checked too. 100 isolated seeds
    // spend 100 resolutions, 700 sort steps and 100 admissions; the 16th
    // clock read, before operation 960, falls in the discovery loop.
    let (db, seeds) = isolated(100);
    let reads = Cell::new(0u64);
    let clock = || {
        reads.set(reads.get() + 1);
        if reads.get() < 16 { 0 } else { 1_000 }
    };
    let limits = GraphLimits {
        deadline_ms: 10,
        ..hops(1)
    };
    let result = timed(
        db.graph(),
        &seeds,
        &Algorithm::Degree,
        &limits,
        &clock,
        None,
    );
    assert_eq!(result.truncation, vec![AlgorithmTruncation::Deadline]);
    assert_eq!(
        (result.stats.edges_examined, result.stats.vertex_operations),
        (0, 960)
    );
    assert!(!result.scope_complete && result.output.is_none());

    // A run of fewer than 64 operations reads the clock twice; a late
    // final read withholds the finished output.
    let db = hand_graph();
    let reads = Cell::new(0u64);
    let clock = || {
        reads.set(reads.get() + 1);
        if reads.get() < 2 { 0 } else { 100 }
    };
    let limits = GraphLimits {
        deadline_ms: 50,
        ..hops(1)
    };
    let result = timed(
        db.graph(),
        &scope(&["a"], Direction::Out),
        &Algorithm::Degree,
        &limits,
        &clock,
        None,
    );
    assert_eq!(reads.get(), 2);
    assert_eq!(result.truncation, vec![AlgorithmTruncation::Deadline]);
    assert!(result.scope_complete && !result.computation_complete);
    assert!(result.output.is_none() && result.projection.is_some());
}

#[test]
fn cancellation_is_checked_before_work_periodically_and_at_the_end() {
    let db = hub_graph(10, 500);
    let hub = scope(&["hub"], Direction::Both);
    let flag = AtomicBool::new(true);
    let result = timed(
        db.graph(),
        &hub,
        &Algorithm::Degree,
        &hops(1),
        &|| 0,
        Some(&flag),
    );
    assert_eq!(result.truncation, vec![AlgorithmTruncation::Cancelled]);
    assert_eq!(
        result.stats.vertex_operations + result.stats.edges_examined,
        0
    );

    let flag = AtomicBool::new(false);
    let reads = Cell::new(0u64);
    let clock = || {
        reads.set(reads.get() + 1);
        if reads.get() == 3 {
            flag.store(true, AtomicOrdering::Relaxed);
        }
        0
    };
    let result = timed(
        db.graph(),
        &hub,
        &Algorithm::Degree,
        &hops(1),
        &clock,
        Some(&flag),
    );
    assert_eq!(result.truncation, vec![AlgorithmTruncation::Cancelled]);
    assert_eq!(
        result.stats.edges_examined + result.stats.vertex_operations,
        128
    );

    let hand = hand_graph();
    let flag = AtomicBool::new(false);
    let reads = Cell::new(0u64);
    let clock = || {
        reads.set(reads.get() + 1);
        if reads.get() == 2 {
            flag.store(true, AtomicOrdering::Relaxed);
        }
        0
    };
    let result = timed(
        hand.graph(),
        &scope(&["a"], Direction::Out),
        &path("a", "b", None),
        &hops(1),
        &clock,
        Some(&flag),
    );
    assert_eq!(result.truncation, vec![AlgorithmTruncation::Cancelled]);
    assert!(result.scope_complete && !result.computation_complete && result.output.is_none());
}

#[test]
fn output_caps_apply_to_finished_results() {
    let db = hand_graph();
    let graph = db.graph();
    let [a, b, c] = ["a", "b", "c"].map(|name| db.id(name));
    let seeds = scope(&["a", "d"], Direction::Out);
    let capped = |max_output_nodes, max_output_edges, max_output_paths| GraphLimits {
        max_output_nodes,
        max_output_edges,
        max_output_paths,
        ..hops(2)
    };
    let full = exec(graph, &seeds, &Algorithm::Degree, &hops(2));
    let result = exec(graph, &seeds, &Algorithm::Degree, &capped(2, 0, 0));
    assert_eq!(result.truncation, vec![AlgorithmTruncation::OutputLimit]);
    assert!(result.computation_complete && result.scope_complete);
    assert_eq!(degrees(&result), degrees(&full)[..2].to_vec());
    let result = exec(graph, &seeds, &Algorithm::Degree, &capped(0, 0, 0));
    assert_eq!(result.truncation, vec![AlgorithmTruncation::OutputLimit]);
    assert_eq!(degrees(&result), vec![]);
    assert!(exec(graph, &seeds, &Algorithm::Degree, &capped(4, 0, 0)).complete);

    // The path a→b→c has 3 nodes and 2 edges.
    let out = scope(&["a"], Direction::Out);
    let to_c = path("a", "c", Some("w"));
    let result = exec(graph, &out, &to_c, &capped(3, 2, 1));
    assert!(result.complete);
    assert_eq!(found(&result).nodes, vec![a, b, c]);
    for limits in [capped(2, 2, 1), capped(3, 1, 1), capped(3, 2, 0)] {
        let result = exec(graph, &out, &to_c, &limits);
        assert_eq!(result.truncation, vec![AlgorithmTruncation::OutputLimit]);
        assert!(result.computation_complete && result.output.is_none());
    }
    // A certified absence is never capped.
    let result = exec(graph, &out, &path("a", "d", None), &capped(0, 0, 0));
    assert!(result.complete && no_path(&result));
}

#[test]
fn one_ttl_clock_decides_visibility() {
    let mut db = hand_graph();
    let graph = db.graph();
    let [a, b, c] = ["a", "b", "c"].map(|name| db.id(name));
    let e = edge_ids(graph);
    db.run(&[
        GraphMutation::SetNodeExpiry {
            key: "b".into(),
            expires_at_ms: Some(NOW),
            expected: Precondition::Any,
        },
        GraphMutation::SetEdgeExpiry {
            edge: EdgeSelector::Id(e[0]),
            expires_at_ms: Some(NOW + 1),
            expected: Precondition::Any,
        },
        GraphMutation::SetEdgeExpiry {
            edge: EdgeSelector::Id(e[4]),
            expires_at_ms: Some(NOW - 10),
            expected: Precondition::Any,
        },
    ]);
    let graph = db.graph();
    let both = scope(&["a"], Direction::Both);
    let at = |now| run_at(graph, &both, &Algorithm::Degree, &hops(4), &CAPS, now);
    // Before any deadline everything is visible.
    assert_eq!(
        projected(&at(NOW - 11).unwrap()),
        (vec![a, b, c], e.clone())
    );
    // e4 is due; b and its edges are not yet.
    assert_eq!(
        projected(&at(NOW - 1).unwrap()),
        (vec![a, b, c], e[..4].to_vec())
    );
    // At b's deadline b is hidden, and so is e0 although its own deadline
    // is later; without b and e4, a is alone.
    let result = at(NOW).unwrap();
    assert_eq!(projected(&result), (vec![a], vec![]));
    assert_eq!(result.read_time_ms, NOW);
    // Due seeds and endpoints do not resolve.
    let seeds = scope(&["b"], Direction::Both);
    assert_eq!(
        run_at(graph, &seeds, &Algorithm::Degree, &hops(1), &CAPS, NOW),
        Err(AlgorithmError::Graph(GraphError::UnknownNodeKey(
            "b".into()
        )))
    );
    let mut by_id = seeds.clone();
    by_id.seeds = vec![NodeSelector::Id(b)];
    assert_eq!(
        run_at(graph, &by_id, &Algorithm::Degree, &hops(1), &CAPS, NOW),
        Err(AlgorithmError::Graph(GraphError::NodeNotFound(b)))
    );
    assert_eq!(
        run_at(graph, &both, &path("a", "b", None), &hops(1), &CAPS, NOW),
        Err(AlgorithmError::Graph(GraphError::UnknownNodeKey(
            "b".into()
        )))
    );
    let result = run_at(
        graph,
        &both,
        &path("a", "b", None),
        &hops(1),
        &CAPS,
        NOW - 1,
    )
    .unwrap();
    assert_eq!(found(&result).edges, vec![e[0]]);
}

#[test]
fn invalid_requests_fail_before_any_work() {
    let db = hand_graph();
    let graph = db.graph();
    let base = scope(&["a"], Direction::Out);
    let invalid = |reason| Err(AlgorithmError::Graph(GraphError::InvalidQuery(reason)));
    let flag = AtomicBool::new(true);
    let clock = || 0;
    let context = ExecutionContext {
        cancel: Some(&flag),
        ..ExecutionContext::new(VIEW, &clock)
    };
    // Validation precedes the cancellation check.
    let attempt = |scope: &AlgorithmScope, algorithm: &Algorithm, limits: &GraphLimits, caps| {
        run(graph, scope, algorithm, limits, caps, &context)
    };
    let degree = Algorithm::Degree;
    let limits = hops(2);

    let zero = GraphLimits {
        deadline_ms: 0,
        ..limits
    };
    assert_eq!(
        attempt(&base, &degree, &zero, &CAPS),
        invalid("deadline_ms must be at least 1")
    );
    let high = GraphLimits {
        max_hops: 5,
        ..limits
    };
    assert_eq!(
        attempt(&base, &degree, &high, &CAPS),
        Err(AlgorithmError::Graph(GraphError::LimitExceedsCeiling {
            limit: "max_hops",
            requested: 5,
            ceiling: 4
        }))
    );
    let caps = GraphAlgorithmLimits {
        max_vertex_operations: 0,
        ..CAPS
    };
    assert_eq!(
        attempt(&base, &degree, &limits, &caps),
        invalid("max_vertex_operations must be at least 1")
    );
    let caps = GraphAlgorithmLimits {
        max_scratch_bytes: 0,
        ..CAPS
    };
    assert_eq!(
        attempt(&base, &degree, &limits, &caps),
        invalid("max_scratch_bytes must be at least 1")
    );
    let caps = GraphAlgorithmLimits {
        max_scratch_bytes: (256 << 20) + 1,
        max_vertex_operations: 0,
        ..CAPS
    };
    assert_eq!(
        attempt(&base, &degree, &limits, &caps),
        Err(AlgorithmError::Graph(GraphError::LimitExceedsCeiling {
            limit: "max_scratch_bytes",
            requested: (256 << 20) + 1,
            ceiling: 256 << 20
        }))
    );

    let mut bad = base.clone();
    bad.seeds = vec![];
    assert_eq!(
        attempt(&bad, &degree, &limits, &CAPS),
        invalid("an algorithm scope needs a seed")
    );
    bad.seeds = vec![NodeSelector::Id(NodeId(0)); 10_001];
    assert_eq!(
        attempt(&bad, &degree, &limits, &CAPS),
        Err(AlgorithmError::Graph(GraphError::LimitExceedsCeiling {
            limit: "seeds",
            requested: 10_001,
            ceiling: 10_000
        }))
    );
    let mut bad = base.clone();
    bad.types = Some(vec![]);
    assert_eq!(
        attempt(&bad, &degree, &limits, &CAPS),
        invalid("a relationship type scope must not be empty")
    );
    bad.types = Some(vec!["R".to_owned(); 257]);
    assert_eq!(
        attempt(&bad, &degree, &limits, &CAPS),
        Err(AlgorithmError::Graph(GraphError::LimitExceedsCeiling {
            limit: "relationship_types",
            requested: 257,
            ceiling: 256
        }))
    );
    let mut bad = base.clone();
    bad.labels = Some(vec![]);
    assert_eq!(
        attempt(&bad, &degree, &limits, &CAPS),
        invalid("a label scope must not be empty")
    );
    bad.labels = Some((0..9).map(|index| format!("L{index}")).collect());
    assert_eq!(
        attempt(&bad, &degree, &limits, &CAPS),
        Err(AlgorithmError::Graph(GraphError::TooManyLabels))
    );
    // Duplicates count toward the raw label cap.
    bad.labels = Some(vec!["A".to_owned(); 9]);
    assert_eq!(
        attempt(&bad, &degree, &limits, &CAPS),
        Err(AlgorithmError::Graph(GraphError::TooManyLabels))
    );
    let mut bad = base.clone();
    bad.edge_conditions = vec![PropertyCondition::Range {
        field: "w".into(),
        min: None,
        max: None,
    }];
    assert!(matches!(
        attempt(&bad, &degree, &limits, &CAPS),
        Err(AlgorithmError::Graph(GraphError::InvalidQuery(_)))
    ));
    for weight in [String::new(), "w".repeat(257)] {
        let algorithm = Algorithm::WeightedShortestPath {
            source: key("a"),
            destination: key("b"),
            weight_property: Some(weight),
        };
        assert_eq!(
            attempt(&base, &algorithm, &limits, &CAPS),
            invalid("a weight property name must have 1..=256 bytes")
        );
    }
}

#[test]
fn invalid_items_fail_in_metered_order() {
    let db = hand_graph();
    let graph = db.graph();
    let base = scope(&["a"], Direction::Out);
    let degree = Algorithm::Degree;
    let limits = hops(2);
    let attempt = |scope: &AlgorithmScope, algorithm: &Algorithm| {
        run_at(graph, scope, algorithm, &limits, &CAPS, 0)
    };
    let mut bad = base.clone();
    bad.seeds = vec![key("")];
    assert_eq!(
        attempt(&bad, &degree),
        Err(AlgorithmError::Graph(GraphError::InvalidNodeKey))
    );
    let mut bad = base.clone();
    bad.types = Some(names(&[""]));
    assert_eq!(
        attempt(&bad, &degree),
        Err(AlgorithmError::Graph(GraphError::InvalidRelationshipType))
    );
    let mut bad = base.clone();
    bad.labels = Some(names(&[""]));
    assert_eq!(
        attempt(&bad, &degree),
        Err(AlgorithmError::Graph(GraphError::InvalidLabel))
    );
    // Items are prepared in order: types, labels, seeds, endpoints.
    let mut bad = base.clone();
    bad.seeds = vec![key("")];
    bad.types = Some(names(&["R", ""]));
    bad.labels = Some(names(&[""]));
    assert_eq!(
        attempt(&bad, &degree),
        Err(AlgorithmError::Graph(GraphError::InvalidRelationshipType))
    );
    let algorithm = Algorithm::WeightedShortestPath {
        source: key("a"),
        destination: key(""),
        weight_property: None,
    };
    assert_eq!(
        attempt(&base, &algorithm),
        Err(AlgorithmError::Graph(GraphError::InvalidNodeKey))
    );

    // Resolution: missing seeds by ID or key.
    let mut missing = base.clone();
    missing.seeds = vec![key("a"), NodeSelector::Id(NodeId(99))];
    assert_eq!(
        attempt(&missing, &degree),
        Err(AlgorithmError::Graph(GraphError::NodeNotFound(NodeId(99))))
    );
    missing.seeds = vec![key("zz")];
    assert_eq!(
        attempt(&missing, &degree),
        Err(AlgorithmError::Graph(GraphError::UnknownNodeKey(
            "zz".into()
        )))
    );
}

#[test]
fn weights_must_be_present_finite_nonnegative_and_exact() {
    let db = hand_graph();
    let graph = db.graph();
    let e = edge_ids(graph);
    let out = scope(&["a"], Direction::Out);
    // The smallest retained edge without the property is reported, even
    // when the destination is unreachable.
    assert_eq!(
        run_at(graph, &out, &path("a", "d", Some("x")), &hops(2), &CAPS, 0),
        Err(AlgorithmError::MissingWeight(e[0]))
    );
    let mut db = TestGraph::new();
    db.run(&[
        node("s", &[], Properties::new()),
        node("t", &[], Properties::new()),
        edge(None, "s", "R", "t", props(&[("w", int(-1))])),
        edge(
            None,
            "s",
            "R",
            "t",
            props(&[("w", PropertyValue::String("1".into()))]),
        ),
        edge(None, "s", "R", "t", props(&[("w", int((1 << 53) + 1))])),
    ]);
    let graph = db.graph();
    let e = edge_ids(graph);
    let st = scope(&["s"], Direction::Out);
    assert_eq!(
        run_at(graph, &st, &path("s", "t", Some("w")), &hops(1), &CAPS, 0),
        Err(AlgorithmError::InvalidWeight {
            edge: e[0],
            reason: "weights must be nonnegative"
        })
    );
    let mut types = st.clone();
    types.edge_conditions = vec![PropertyCondition::Eq {
        field: "w".into(),
        value: int((1 << 53) + 1),
    }];
    assert_eq!(
        run_at(
            graph,
            &types,
            &path("s", "t", Some("w")),
            &hops(1),
            &CAPS,
            0
        ),
        Err(AlgorithmError::InvalidWeight {
            edge: e[2],
            reason: "integer weights must be at most 2^53"
        })
    );

    assert_eq!(
        classify(&int(0)),
        Ok(Weight {
            mantissa: 0,
            exponent: 0
        })
    );
    assert_eq!(
        classify(&int(12)),
        Ok(Weight {
            mantissa: 3,
            exponent: 2
        })
    );
    assert_eq!(
        classify(&int(1 << 53)),
        Ok(Weight {
            mantissa: 1,
            exponent: 53
        })
    );
    assert_eq!(
        classify(&int((1 << 53) + 1)),
        Err("integer weights must be at most 2^53")
    );
    assert_eq!(classify(&int(-1)), Err("weights must be nonnegative"));
    let float = PropertyValue::Float;
    assert_eq!(
        classify(&float(0.5)),
        Ok(Weight {
            mantissa: 1,
            exponent: -1
        })
    );
    assert_eq!(
        classify(&float(-0.0)),
        Ok(Weight {
            mantissa: 0,
            exponent: 0
        })
    );
    assert_eq!(
        classify(&float(3.0)),
        Ok(Weight {
            mantissa: 3,
            exponent: 0
        })
    );
    assert_eq!(
        classify(&float(5e-324)),
        Ok(Weight {
            mantissa: 1,
            exponent: -1074
        })
    );
    assert_eq!(
        classify(&float(f64::MAX)),
        Ok(Weight {
            mantissa: (1 << 53) - 1,
            exponent: 971
        })
    );
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(classify(&float(value)), Err("weights must be finite"));
    }
    assert_eq!(
        classify(&float(-1e-300)),
        Err("weights must be nonnegative")
    );
    for value in [
        PropertyValue::String("1".into()),
        PropertyValue::Boolean(true),
    ] {
        assert_eq!(classify(&value), Err("weights must be integers or floats"));
    }
}

/// The exact sum of `values` rounded once, as the search computes costs.
fn exact_sum(values: &[f64]) -> Option<f64> {
    let weights: Vec<Weight> = values
        .iter()
        .map(|value| classify(&PropertyValue::Float(*value)).unwrap())
        .collect();
    let mut span = Span::default();
    weights.iter().for_each(|weight| span.include(*weight));
    let fixed = span.layout();
    let mut cost = vec![0; fixed.limbs];
    for weight in &weights {
        fixed.add(&mut cost, weight.mantissa, fixed.shift(*weight));
    }
    fixed.to_f64(&cost)
}

#[test]
fn costs_are_exact_sums_rounded_once() {
    // IEEE addition of two values is their exact sum rounded once.
    let mut rng = Rng::new(5);
    let mut random = || {
        let exponent = match rng.below(4) {
            0 => 0,
            1 => 2046,
            2 => 1023 + rng.below(8),
            _ => rng.below(2047),
        };
        f64::from_bits(exponent << 52 | rng.next() & ((1 << 52) - 1))
    };
    for _ in 0..20_000 {
        let (a, b) = (random(), random());
        let sum = a + b;
        assert_eq!(
            exact_sum(&[a, b]),
            sum.is_finite().then_some(sum),
            "{a:e} + {b:e}"
        );
    }
    assert_eq!(exact_sum(&[]), Some(0.0));
    assert_eq!(exact_sum(&[0.0, 0.0]), Some(0.0));
    assert_eq!(exact_sum(&[5e-324, 5e-324]), Some(1e-323));
    assert_eq!(exact_sum(&[f64::MAX, f64::MAX]), None);
    // A left fold rounds 2^53 + 1 down twice; the exact sum does not.
    let big = 9_007_199_254_740_992.0;
    assert_eq!(big + 1.0 + 1.0, big);
    assert_eq!(exact_sum(&[big, 1.0, 1.0]), Some(big + 2.0));
    assert_eq!(exact_sum(&[1.0, 1e300, 1.0, 1e-300]), Some(1e300));
}

/// s→t costs 2^53 + 2 directly (a float) and 2^53 + 1 + 1 over x and y.
/// Exact sums tie, so the direct edge wins on hops; a folding sum would
/// pick the longer path as cheaper.
#[test]
fn ranking_uses_exact_costs() {
    let mut db = TestGraph::new();
    let big = 1i64 << 53;
    db.run(&[
        node("s", &[], Properties::new()),
        node("x", &[], Properties::new()),
        node("y", &[], Properties::new()),
        node("t", &[], Properties::new()),
        edge(
            None,
            "s",
            "R",
            "t",
            props(&[("w", PropertyValue::Float((big + 2) as f64))]),
        ),
        edge(None, "s", "R", "x", props(&[("w", int(big))])),
        edge(None, "x", "R", "y", props(&[("w", int(1))])),
        edge(None, "y", "R", "t", props(&[("w", int(1))])),
        edge(
            None,
            "s",
            "S",
            "t",
            props(&[("w", PropertyValue::Float(f64::MAX))]),
        ),
        edge(
            None,
            "s",
            "S",
            "y",
            props(&[("w", PropertyValue::Float(f64::MAX))]),
        ),
        edge(
            None,
            "y",
            "S",
            "t",
            props(&[("w", PropertyValue::Float(f64::MAX))]),
        ),
    ]);
    let graph = db.graph();
    let e = edge_ids(graph);
    let mut r = scope(&["s"], Direction::Out);
    r.types = Some(names(&["R"]));
    let result = exec(graph, &r, &path("s", "t", Some("w")), &hops(3));
    assert_eq!(found(&result).edges, vec![e[0]]);
    assert_eq!(found(&result).cost, (big + 2) as f64);
    // Through x and y the exact cost 2^53 + 1 + 1 is kept, not 2^53.
    let mut db2 = db.clone();
    db2.run(&[GraphMutation::DeleteEdge {
        edge: EdgeSelector::Id(e[0]),
        expected: Precondition::Any,
    }]);
    let result = exec(db2.graph(), &r, &path("s", "t", Some("w")), &hops(3));
    assert_eq!(found(&result).edges, vec![e[1], e[2], e[3]]);
    assert_eq!(found(&result).cost, (big + 2) as f64);

    // Sums beyond f64::MAX: the direct S edge wins; forcing two S edges
    // overflows.
    let mut s = scope(&["s"], Direction::Out);
    s.types = Some(names(&["S"]));
    let result = exec(graph, &s, &path("s", "t", Some("w")), &hops(2));
    assert_eq!(
        (found(&result).cost, &found(&result).edges),
        (f64::MAX, &vec![e[4]])
    );
    let mut db3 = db.clone();
    db3.run(&[GraphMutation::DeleteEdge {
        edge: EdgeSelector::Id(e[4]),
        expected: Precondition::Any,
    }]);
    assert_eq!(
        run_at(
            db3.graph(),
            &s,
            &path("s", "t", Some("w")),
            &hops(2),
            &CAPS,
            0
        ),
        Err(AlgorithmError::NumericOverflow)
    );
}

#[test]
fn results_are_deterministic_and_reads_do_not_mutate() {
    let db = hand_graph();
    let graph = db.graph();
    let before = graph.clone();
    let replayed = db.replayed();
    let a = db.id("a");
    // The same nodes and edges committed one body each, with the same IDs.
    // New IDs must ascend, so insertion order then varies by re-putting
    // every node (removed and reinserted) and patching every edge in
    // descending ID order in one later body.
    let mut split = TestGraph::new();
    for node in db.graph().nodes() {
        split.commit(GraphBody {
            graph_id: G,
            entries: vec![GraphEntry::PutNode {
                node_id: node.id,
                key: node.key.clone(),
                labels: node.labels.clone(),
                properties: node.properties.clone(),
            }],
        });
    }
    for edge in db.graph().edges() {
        split.commit(GraphBody {
            graph_id: G,
            entries: vec![GraphEntry::PutEdge {
                edge_id: edge.id,
                source: edge.source,
                destination: edge.destination,
                relationship: edge.relationship.clone(),
                key: edge.key.clone(),
                properties: edge.properties.clone(),
            }],
        });
    }
    let nodes: Vec<_> = db.graph().nodes().collect();
    let edges: Vec<_> = db.graph().edges().collect();
    let reinserted = nodes.iter().rev().map(|node| GraphEntry::PutNode {
        node_id: node.id,
        key: node.key.clone(),
        labels: node.labels.clone(),
        properties: node.properties.clone(),
    });
    let patched = edges.iter().rev().map(|edge| GraphEntry::PatchEdge {
        edge_id: edge.id,
        set: edge.properties.clone(),
        unset: Default::default(),
    });
    split.commit(GraphBody {
        graph_id: G,
        entries: reinserted.chain(patched).collect(),
    });
    for direction in [Direction::Out, Direction::In, Direction::Both] {
        for algorithm in [
            Algorithm::Degree,
            path("a", "c", Some("w")),
            path("a", "c", None),
            Algorithm::PageRank {
                damping: 0.85,
                tolerance: 1e-10,
                personalization: None,
                weight_property: Some("w".into()),
            },
            Algorithm::LabelPropagation,
        ] {
            let canonical = scope(&["a", "d"], direction);
            let mut shuffled = scope(&["d", "a", "d"], direction);
            shuffled.seeds.push(NodeSelector::Id(a));
            let expected = exec(graph, &canonical, &algorithm, &hops(2));
            // Two extra raw selectors cost two resolutions, a longer sort
            // and their slots in the seed list and the sort's buffer;
            // everything else is identical.
            let mut reordered = exec(graph, &shuffled, &algorithm, &hops(2));
            assert_eq!(
                reordered.stats.edges_examined,
                expected.stats.edges_examined
            );
            assert!(reordered.stats.vertex_operations > expected.stats.vertex_operations);
            assert_eq!(
                reordered.stats.scratch_bytes,
                expected.stats.scratch_bytes + 2 * 2 * 8
            );
            reordered.stats = expected.stats;
            assert_eq!(reordered, expected);
            assert_eq!(exec(graph, &canonical, &algorithm, &hops(2)), expected);
            assert_eq!(
                exec(replayed.graph(G).unwrap(), &canonical, &algorithm, &hops(2)),
                expected
            );
            assert_eq!(
                exec(split.graph(), &canonical, &algorithm, &hops(2)),
                expected
            );
        }
    }
    assert!(graph == &before);
}

/// Node `s` (ID 0) with `count` self-loops of type `R` (IDs from 0),
/// committed as raw bodies of at most 4096 entries.
fn self_loops(count: u64) -> TestGraph {
    let mut db = TestGraph::new();
    db.commit(GraphBody {
        graph_id: G,
        entries: vec![GraphEntry::PutNode {
            node_id: NodeId(0),
            key: "s".into(),
            labels: Default::default(),
            properties: Properties::new(),
        }],
    });
    let loops: Vec<GraphEntry> = (0..count)
        .map(|id| GraphEntry::PutEdge {
            edge_id: EdgeId(id),
            source: NodeId(0),
            destination: NodeId(0),
            relationship: "R".into(),
            key: None,
            properties: Properties::new(),
        })
        .collect();
    for chunk in loops.chunks(MAX_BATCH_ENTRIES) {
        db.commit(GraphBody {
            graph_id: G,
            entries: chunk.to_vec(),
        });
    }
    db
}

/// 4097 retained self-loops: a buffer that grows by doubling holds 8192
/// slots (196,608 bytes of edges alone), so a budget sized for 4097
/// entries must stop, and every finished run must have reserved at least
/// the capacity it holds (see `check`).
#[test]
fn scratch_covers_buffer_growth_before_allocation() {
    let db = self_loops(4097);
    let graph = db.graph();
    let limits = GraphLimits {
        max_edges_per_node: 5000,
        max_edges_examined: 100_000,
        ..hops(0)
    };
    let seed = scope(&["s"], Direction::Out);
    let full = exec(graph, &seed, &Algorithm::Degree, &limits);
    assert!(full.complete);
    assert_eq!(degrees(&full), vec![(NodeId(0), 4097, 4097, 4097)]);
    let projection = full.projection.as_ref().unwrap();
    assert!(projection.edges.capacity() >= 4097);
    assert!(full.stats.scratch_bytes >= held_bytes(&full));
    let bytes = |max_scratch_bytes| GraphAlgorithmLimits {
        max_scratch_bytes,
        ..CAPS
    };
    let result = run_at(graph, &seed, &Algorithm::Degree, &limits, &bytes(98_416), 0).unwrap();
    assert_eq!(result.truncation, vec![AlgorithmTruncation::MemoryLimit]);
    assert!(result.output.is_none() && result.projection.is_none());
    let at = |budget| run_at(graph, &seed, &Algorithm::Degree, &limits, &bytes(budget), 0);
    assert_eq!(at(full.stats.scratch_bytes).unwrap(), full);
    assert_eq!(
        at(full.stats.scratch_bytes - 1).unwrap().truncation,
        vec![AlgorithmTruncation::MemoryLimit]
    );
    // The same holds for a path over the loops and for Both.
    for (direction, algorithm) in [
        (Direction::Both, Algorithm::Degree),
        (Direction::Out, path("s", "s", None)),
        (Direction::In, path("s", "s", None)),
    ] {
        let seed = scope(&["s"], direction);
        let full = exec(graph, &seed, &algorithm, &limits);
        assert!(full.complete, "{direction:?}");
        assert!(full.stats.scratch_bytes >= held_bytes(&full));
    }
}

/// Seed and endpoint resolution, label checks and the seed sort run under
/// the meter: 9,999 duplicate seeds and a missing one.
#[test]
fn preparation_is_metered_from_the_start() {
    let db = hand_graph();
    let graph = db.graph();
    let mut seeds = scope(&["a"], Direction::Out);
    seeds.seeds = vec![key("a"); 9_999];
    seeds.seeds.push(key("zz"));
    let degree = Algorithm::Degree;
    let missing = Err(AlgorithmError::Graph(GraphError::UnknownNodeKey(
        "zz".into(),
    )));
    assert_eq!(run_at(graph, &seeds, &degree, &hops(1), &CAPS, 0), missing);

    // A pre-set cancellation stops before the first resolution.
    let flag = AtomicBool::new(true);
    let result = timed(graph, &seeds, &degree, &hops(1), &|| 0, Some(&flag));
    assert_eq!(result.truncation, vec![AlgorithmTruncation::Cancelled]);
    let stats = result.stats;
    assert_eq!(
        (
            stats.vertex_operations,
            stats.edges_examined,
            stats.scratch_bytes
        ),
        (0, 0, 0)
    );
    assert!(result.seeds.is_empty() && result.endpoints.is_none());
    let to_missing = path("a", "zz", None);
    let result = timed(
        graph,
        &scope(&["a"], Direction::Out),
        &to_missing,
        &hops(1),
        &|| 0,
        Some(&flag),
    );
    assert_eq!(result.truncation, vec![AlgorithmTruncation::Cancelled]);

    // One vertex operation resolves one selector.
    let one = GraphAlgorithmLimits {
        max_vertex_operations: 1,
        ..CAPS
    };
    let result = run_at(graph, &seeds, &degree, &hops(1), &one, 0).unwrap();
    assert_eq!(result.truncation, vec![AlgorithmTruncation::VertexLimit]);
    assert_eq!(result.stats.vertex_operations, 1);
    assert!(result.seeds.is_empty());

    // The seed buffer is reserved before it is allocated.
    let byte = GraphAlgorithmLimits {
        max_scratch_bytes: 1,
        ..CAPS
    };
    let result = run_at(graph, &seeds, &degree, &hops(1), &byte, 0).unwrap();
    assert_eq!(result.truncation, vec![AlgorithmTruncation::MemoryLimit]);
    assert_eq!(
        (result.stats.vertex_operations, result.stats.scratch_bytes),
        (0, 0)
    );

    // The deadline covers resolution: the second clock read, before the
    // 65th operation, is late.
    let reads = Cell::new(0u64);
    let clock = || {
        reads.set(reads.get() + 1);
        if reads.get() < 2 { 0 } else { 1_000 }
    };
    let limits = GraphLimits {
        deadline_ms: 10,
        ..hops(1)
    };
    let result = timed(graph, &seeds, &degree, &limits, &clock, None);
    assert_eq!(result.truncation, vec![AlgorithmTruncation::Deadline]);
    assert_eq!(result.stats.vertex_operations, 64);
    assert!(result.seeds.is_empty());
}

/// 100 seeds in a chain of `R` edges with a scope of 256 absent types:
/// every scan probes each type, and the probes are charged and checked
/// although they yield no edge.
#[test]
fn absent_relationship_type_probes_are_charged_and_interruptible() {
    let mut db = TestGraph::new();
    let names: Vec<String> = (0..100).map(|index| format!("n{index}")).collect();
    let mut mutations: Vec<GraphMutation> = names
        .iter()
        .map(|name| node(name, &[], Properties::new()))
        .collect();
    mutations.extend(
        names
            .windows(2)
            .map(|pair| edge(None, &pair[0], "R", &pair[1], Properties::new())),
    );
    db.run(&mutations);
    let graph = db.graph();
    let mut absent = scope(&[], Direction::Both);
    absent.seeds = graph
        .nodes()
        .map(|node| NodeSelector::Id(node.id))
        .collect();
    absent.types = Some((0..256).map(|index| format!("X{index:03}")).collect());

    let full = exec(graph, &absent, &Algorithm::Degree, &hops(0));
    assert!(full.complete);
    assert_eq!(
        (full.stats.edges_examined, full.stats.edges_extracted),
        (0, 0)
    );
    assert!(
        full.stats.vertex_operations >= 100 * 256,
        "{}",
        full.stats.vertex_operations
    );

    // A vertex cap stops inside the probes.
    let caps = GraphAlgorithmLimits {
        max_vertex_operations: 10_000,
        ..CAPS
    };
    let result = run_at(graph, &absent, &Algorithm::Degree, &hops(0), &caps, 0).unwrap();
    assert_eq!(result.truncation, vec![AlgorithmTruncation::VertexLimit]);
    assert_eq!(
        (result.stats.vertex_operations, result.stats.edges_examined),
        (10_000, 0)
    );
    assert!(!result.scope_complete);

    // Cancellation raised at the 100th clock read interrupts the probes.
    let flag = AtomicBool::new(false);
    let reads = Cell::new(0u64);
    let clock = || {
        reads.set(reads.get() + 1);
        if reads.get() == 100 {
            flag.store(true, AtomicOrdering::Relaxed);
        }
        0
    };
    let result = timed(
        graph,
        &absent,
        &Algorithm::Degree,
        &hops(0),
        &clock,
        Some(&flag),
    );
    assert_eq!(result.truncation, vec![AlgorithmTruncation::Cancelled]);
    assert_eq!(
        (result.stats.vertex_operations, result.stats.edges_examined),
        (99 * 64, 0)
    );
}

/// The case a per-addition oracle gets wrong: s→t weighs 2^53 + 2 and
/// s→x→y→t weighs 2^53, 1 and 1, all floats. The exact sums tie, so the
/// direct edge wins on hops, whichever is created first.
#[test]
fn exact_cost_ties_go_to_fewer_hops() {
    let big = 9_007_199_254_740_992.0;
    let float = |value: f64| props(&[("w", PropertyValue::Float(value))]);
    for direct_first in [true, false] {
        let mut db = TestGraph::new();
        let mut mutations = vec![
            node("s", &[], Properties::new()),
            node("x", &[], Properties::new()),
            node("y", &[], Properties::new()),
            node("t", &[], Properties::new()),
            edge(Some("sx"), "s", "R", "x", float(big)),
            edge(Some("xy"), "x", "R", "y", float(1.0)),
            edge(Some("yt"), "y", "R", "t", float(1.0)),
        ];
        let direct = edge(Some("st"), "s", "R", "t", float(big + 2.0));
        if direct_first {
            mutations.insert(4, direct);
        } else {
            mutations.push(direct);
        }
        db.run(&mutations);
        let graph = db.graph();
        let direct = graph.edge_by_key("st").unwrap().id;
        for direction in [Direction::Out, Direction::Both] {
            let result = exec(
                graph,
                &scope(&["s"], direction),
                &path("s", "t", Some("w")),
                &hops(3),
            );
            let winner = found(&result);
            assert_eq!(winner.edges, vec![direct]);
            assert_eq!(winner.nodes, vec![db.id("s"), db.id("t")]);
            assert_eq!(winner.cost, 9_007_199_254_740_994.0);
        }
    }
}

mod betweenness;
mod iterative;
