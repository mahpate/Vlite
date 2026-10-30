use super::super::error::GraphError;
use super::super::filter::NodeFilter;
use super::super::limits::GraphLimits;
use super::super::model::{Direction, EdgeId, NodeId, Properties};
use super::super::query::{
    GraphPath, GraphQuery, NodeSelector, PathOutcome, QueryOp, QueryOutput, TruncationReason,
};
use super::super::testkit::{TestGraph, edge, hub_graph, node, random_graph, read, set};
use std::collections::{BTreeSet, HashSet};

fn query(op: QueryOp) -> GraphQuery {
    GraphQuery::new(op)
}

fn path_query(source: &str, destination: &str, direction: Direction) -> GraphQuery {
    query(QueryOp::ShortestPath {
        source: NodeSelector::Key(source.into()),
        destination: NodeSelector::Key(destination.into()),
        direction,
        types: None,
    })
}

#[test]
fn traversal_matches_a_seeded_breadth_first_oracle() {
    for seed in 0..200 {
        let db = random_graph(seed + 1);
        let graph = db.graph();
        let Some(start) = graph.nodes().next() else {
            continue;
        };
        let start_id = start.id;
        for direction in [Direction::Out, Direction::In, Direction::Both] {
            for types in [None, Some(set(&["R", "T"]))] {
                let query = query(QueryOp::Traverse {
                    start: vec![NodeSelector::Id(start_id)],
                    direction,
                    types: types.clone(),
                    expand_filter: NodeFilter::default(),
                    return_filter: NodeFilter::default(),
                    edge_filter: vec![],
                    include_paths: false,
                });
                let result = read(graph, &query, &GraphLimits::default()).unwrap();
                let QueryOutput::Traversal {
                    nodes,
                    edges,
                    paths,
                } = result.output
                else {
                    panic!("expected traversal output");
                };
                assert!(paths.is_empty());
                let (expected_nodes, expected_edges) =
                    bfs_oracle(graph, start_id, direction, 2, types.as_ref());
                assert_eq!(
                    nodes
                        .iter()
                        .map(|node| (node.node.id, node.depth))
                        .collect::<Vec<_>>(),
                    expected_nodes,
                    "seed={seed} direction={direction:?} types={types:?}"
                );
                assert_eq!(
                    edges.iter().map(|edge| edge.id).collect::<Vec<_>>(),
                    expected_edges,
                    "seed={seed} direction={direction:?} types={types:?}"
                );
            }
        }
    }
}

fn bfs_oracle(
    graph: &super::super::state::Graph,
    start: NodeId,
    direction: Direction,
    max_hops: u64,
    types: Option<&BTreeSet<String>>,
) -> (Vec<(NodeId, u64)>, Vec<EdgeId>) {
    let mut nodes = vec![(start, 0)];
    let mut seen: HashSet<NodeId> = [start].into();
    let mut edges = Vec::new();
    let mut seen_edges = HashSet::new();
    let mut head = 0;
    while head < nodes.len() {
        let (node, depth) = nodes[head];
        head += 1;
        if depth >= max_hops {
            continue;
        }
        for (kind, id) in super::super::testkit::naive_adjacency(graph, node, direction, types) {
            let _ = kind;
            let edge = graph.edge(id).unwrap();
            if seen_edges.insert(id) {
                edges.push(id);
            }
            let next = super::super::testkit::far_end(edge, node);
            if seen.insert(next) {
                nodes.push((next, depth + 1));
            }
        }
    }
    (nodes, edges)
}

struct PathEnumerator<'a> {
    graph: &'a super::super::state::Graph,
    destination: NodeId,
    direction: Direction,
    types: Option<&'a BTreeSet<String>>,
    max_hops: usize,
    paths: Vec<GraphPath>,
}

impl PathEnumerator<'_> {
    fn visit(&mut self, current: NodeId, visited: &mut HashSet<NodeId>, path: &mut GraphPath) {
        if path.edges.len() == self.max_hops {
            return;
        }
        let adjacent =
            super::super::testkit::naive_adjacency(self.graph, current, self.direction, self.types);
        for (_, edge_id) in adjacent {
            let edge = self.graph.edge(edge_id).unwrap();
            let next = super::super::testkit::far_end(edge, current);
            if !visited.insert(next) {
                continue;
            }
            path.nodes.push(next);
            path.edges.push(edge_id);
            if next == self.destination {
                self.paths.push(path.clone());
            } else {
                self.visit(next, visited, path);
            }
            path.nodes.pop();
            path.edges.pop();
            visited.remove(&next);
        }
    }
}

fn brute_shortest_path(
    graph: &super::super::state::Graph,
    source: NodeId,
    destination: NodeId,
    direction: Direction,
    types: Option<&BTreeSet<String>>,
    max_hops: usize,
) -> Option<GraphPath> {
    if source == destination {
        return Some(GraphPath {
            nodes: vec![source],
            edges: vec![],
        });
    }
    let mut enumerator = PathEnumerator {
        graph,
        destination,
        direction,
        types,
        max_hops,
        paths: Vec::new(),
    };
    enumerator.visit(
        source,
        &mut [source].into(),
        &mut GraphPath {
            nodes: vec![source],
            edges: vec![],
        },
    );
    enumerator.paths.into_iter().min_by(|left, right| {
        left.edges
            .len()
            .cmp(&right.edges.len())
            .then_with(|| left.edges.cmp(&right.edges))
    })
}

#[test]
fn paths_are_canonical_and_self_loops_do_not_extend_them() {
    let mut db = TestGraph::new();
    db.run(&[
        node("a", &[], Properties::new()),
        node("b", &[], Properties::new()),
        node("c", &[], Properties::new()),
        node("d", &[], Properties::new()),
        edge(None, "a", "R", "a", Properties::new()),
        edge(None, "a", "R", "c", Properties::new()),
        edge(None, "a", "R", "c", Properties::new()),
        edge(None, "c", "R", "d", Properties::new()),
        edge(None, "a", "R", "b", Properties::new()),
        edge(None, "b", "R", "d", Properties::new()),
    ]);
    let result = read(
        db.graph(),
        &query(QueryOp::Traverse {
            start: vec![NodeSelector::Key("a".into())],
            direction: Direction::Both,
            types: None,
            expand_filter: NodeFilter::default(),
            return_filter: NodeFilter::default(),
            edge_filter: vec![],
            include_paths: true,
        }),
        &GraphLimits {
            max_hops: 2,
            ..GraphLimits::default()
        },
    )
    .unwrap();
    let QueryOutput::Traversal {
        nodes,
        edges,
        paths,
    } = result.output
    else {
        panic!("expected traversal output");
    };
    assert_eq!(
        edges.iter().map(|edge| edge.id.0).collect::<Vec<_>>(),
        [0, 1, 2, 4, 3, 5]
    );
    let d = nodes.iter().find(|node| node.node.key == "d").unwrap();
    assert_eq!(d.depth, 2);
    let path = paths
        .iter()
        .find(|path| path.nodes.last() == Some(&d.node.id))
        .unwrap();
    assert_eq!(path.edges, [EdgeId(1), EdgeId(3)]);
    assert_eq!(path.nodes, [NodeId(0), NodeId(2), NodeId(3)]);
    assert!(paths.iter().all(|path| !path.edges.contains(&EdgeId(0))));

    let shortest = read(
        db.graph(),
        &path_query("a", "d", Direction::Out),
        &GraphLimits {
            max_hops: 2,
            ..GraphLimits::default()
        },
    )
    .unwrap();
    let QueryOutput::ShortestPath(PathOutcome::Found { path, .. }) = shortest.output else {
        panic!("expected shortest path");
    };
    assert_eq!(path.edges, [EdgeId(1), EdgeId(3)]);
}

#[test]
fn shortest_path_distinguishes_complete_no_path_from_exhaustion() {
    let mut db = TestGraph::new();
    db.run(&[
        node("a", &[], Properties::new()),
        node("b", &[], Properties::new()),
        node("c", &[], Properties::new()),
        edge(None, "a", "R", "b", Properties::new()),
        edge(None, "b", "R", "c", Properties::new()),
    ]);
    let complete = read(
        db.graph(),
        &path_query("a", "c", Direction::Out),
        &GraphLimits {
            max_hops: 1,
            ..GraphLimits::default()
        },
    )
    .unwrap();
    assert!(matches!(
        complete.output,
        QueryOutput::ShortestPath(PathOutcome::NoPathWithinHops)
    ));
    assert!(complete.complete);

    let exhausted = read(
        db.graph(),
        &path_query("a", "c", Direction::Both),
        &GraphLimits {
            max_hops: 3,
            max_edges_examined: 1,
            ..GraphLimits::default()
        },
    )
    .unwrap();
    assert!(matches!(
        exhausted.output,
        QueryOutput::ShortestPath(PathOutcome::Exhausted {
            truncation: ref reasons
        }) if reasons == &[TruncationReason::EdgeLimit]
    ));
    assert_eq!(exhausted.truncation, [TruncationReason::EdgeLimit]);
    assert!(!exhausted.complete);
}

#[test]
fn shortest_path_matches_a_brute_force_simple_path_oracle() {
    for seed in 0..120 {
        let db = random_graph(seed + 0xABCD);
        let graph = db.graph();
        let ids: Vec<_> = graph.nodes().map(|node| node.id).collect();
        if ids.len() < 2 {
            continue;
        }
        let mut rng = super::super::testkit::Rng::new(seed + 77);
        let source = ids[rng.below(ids.len() as u64) as usize];
        let destination = ids[rng.below(ids.len() as u64) as usize];
        for direction in [Direction::Out, Direction::In, Direction::Both] {
            for types in [None, Some(set(&["R", "T"]))] {
                let expected =
                    brute_shortest_path(graph, source, destination, direction, types.as_ref(), 3);
                let result = read(
                    graph,
                    &query(QueryOp::ShortestPath {
                        source: NodeSelector::Id(source),
                        destination: NodeSelector::Id(destination),
                        direction,
                        types: types.clone(),
                    }),
                    &GraphLimits {
                        max_hops: 3,
                        ..GraphLimits::default()
                    },
                )
                .unwrap();
                match (expected, result.output) {
                    (
                        Some(expected),
                        QueryOutput::ShortestPath(PathOutcome::Found { path, .. }),
                    ) => assert_eq!(
                        path, expected,
                        "seed={seed} source={source:?} destination={destination:?} direction={direction:?} types={types:?}"
                    ),
                    (None, QueryOutput::ShortestPath(PathOutcome::NoPathWithinHops)) => {}
                    (expected, actual) => panic!(
                        "seed={seed} source={source:?} destination={destination:?} direction={direction:?} types={types:?}: expected {expected:?}, got {actual:?}"
                    ),
                }
            }
        }
    }
}

#[test]
fn filters_prune_expansion_without_changing_return_selection() {
    let mut db = TestGraph::new();
    db.run(&[
        node("start", &["Return", "Expand"], Properties::new()),
        node("middle", &["Return"], Properties::new()),
        node("end", &["Return"], Properties::new()),
        edge(None, "start", "R", "middle", Properties::new()),
        edge(None, "middle", "R", "end", Properties::new()),
    ]);
    let result = read(
        db.graph(),
        &query(QueryOp::Traverse {
            start: vec![NodeSelector::Key("start".into())],
            direction: Direction::Out,
            types: None,
            expand_filter: NodeFilter {
                labels: set(&["Expand"]),
                conditions: vec![],
            },
            return_filter: NodeFilter {
                labels: set(&["Return"]),
                conditions: vec![],
            },
            edge_filter: vec![],
            include_paths: true,
        }),
        &GraphLimits {
            max_hops: 3,
            ..GraphLimits::default()
        },
    )
    .unwrap();
    let QueryOutput::Traversal {
        nodes,
        edges,
        paths,
    } = result.output
    else {
        panic!("expected traversal output");
    };
    assert_eq!(
        nodes
            .iter()
            .map(|node| node.node.key.as_str())
            .collect::<Vec<_>>(),
        ["start", "middle"]
    );
    assert_eq!(edges.iter().map(|edge| edge.id.0).collect::<Vec<_>>(), [0]);
    assert_eq!(
        paths
            .iter()
            .map(|path| path.edges.as_slice())
            .collect::<Vec<_>>(),
        [&[EdgeId(0)][..]]
    );
}

#[test]
fn work_limits_include_large_hub_degree_cap_and_cancellation() {
    let db = hub_graph(100_000, 100_000);
    let graph = db.graph();
    let result = read(
        graph,
        &query(QueryOp::Traverse {
            start: vec![NodeSelector::Id(NodeId(0))],
            direction: Direction::Out,
            types: None,
            expand_filter: NodeFilter::default(),
            return_filter: NodeFilter::default(),
            edge_filter: vec![],
            include_paths: false,
        }),
        &GraphLimits {
            max_edges_examined: 100,
            max_edges_per_node: 50,
            max_visited_nodes: 100,
            ..GraphLimits::default()
        },
    )
    .unwrap();
    assert_eq!(result.stats.edges_examined, 50);
    assert_eq!(
        result.truncation,
        [TruncationReason::DegreeLimit {
            node_ids: vec![NodeId(0)]
        }]
    );
    assert!(!result.complete);

    let limits = GraphLimits {
        max_edges_examined: 100_000,
        max_edges_per_node: 10_000,
        ..GraphLimits::default()
    };
    let clock_calls = std::cell::Cell::new(0);
    let clock = || {
        clock_calls.set(clock_calls.get() + 1);
        0
    };
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let context = super::super::meter::ExecutionContext {
        view_sequence: super::super::testkit::VIEW,
        clock: &clock,
        cancel: Some(&cancel),
        now_ms: 0,
    };
    let query = query(QueryOp::Neighbors {
        node: NodeSelector::Id(NodeId(0)),
        direction: Direction::Out,
        types: None,
        edge_conditions: vec![],
        limit: Some(2_000),
        cursor: None,
    });
    // The periodic clock callback flips cancellation after the first check.
    let flip = || {
        let calls = clock_calls.get() + 1;
        clock_calls.set(calls);
        if calls >= 2 {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        0
    };
    let context = super::super::meter::ExecutionContext {
        clock: &flip,
        ..context
    };
    let cancelled = super::super::query::execute(graph, &query, &limits, &context).unwrap();
    assert_eq!(cancelled.stats.edges_examined, 64);
    assert_eq!(cancelled.truncation, [TruncationReason::Cancelled]);
    assert!(!cancelled.complete);
}

#[test]
fn output_visited_and_deadline_limits_are_incomplete_without_false_completion() {
    let mut db = TestGraph::new();
    db.run(&[
        node("a", &[], Properties::new()),
        node("b", &[], Properties::new()),
        node("c", &[], Properties::new()),
        edge(None, "a", "R", "b", Properties::new()),
        edge(None, "b", "R", "c", Properties::new()),
    ]);
    let traversal = query(QueryOp::Traverse {
        start: vec![NodeSelector::Id(NodeId(0))],
        direction: Direction::Out,
        types: None,
        expand_filter: NodeFilter::default(),
        return_filter: NodeFilter::default(),
        edge_filter: vec![],
        include_paths: true,
    });
    let output_limited = read(
        db.graph(),
        &traversal,
        &GraphLimits {
            max_hops: 2,
            max_output_nodes: 2,
            max_output_edges: 1,
            max_output_paths: 0,
            ..GraphLimits::default()
        },
    )
    .unwrap();
    assert_eq!(output_limited.truncation, [TruncationReason::OutputLimit]);
    assert!(!output_limited.complete);
    let QueryOutput::Traversal {
        nodes,
        edges,
        paths,
    } = output_limited.output
    else {
        panic!("expected traversal output");
    };
    assert_eq!(nodes.len(), 2);
    assert_eq!(edges.len(), 1);
    assert!(paths.is_empty());

    let visited_limited = read(
        db.graph(),
        &traversal,
        &GraphLimits {
            max_hops: 2,
            max_visited_nodes: 1,
            ..GraphLimits::default()
        },
    )
    .unwrap();
    assert_eq!(visited_limited.truncation, [TruncationReason::VisitedLimit]);
    assert!(!visited_limited.complete);
    assert_eq!(visited_limited.stats.edges_examined, 1);

    let hub = hub_graph(100, 100);
    let calls = std::cell::Cell::new(0);
    let clock = || {
        let call = calls.get();
        calls.set(call + 1);
        if call == 0 { 0 } else { 250 }
    };
    let context = super::super::meter::ExecutionContext::new(super::super::testkit::VIEW, &clock);
    let deadline_limited = super::super::query::execute(
        hub.graph(),
        &query(QueryOp::Neighbors {
            node: NodeSelector::Id(NodeId(0)),
            direction: Direction::Out,
            types: None,
            edge_conditions: vec![],
            limit: Some(100),
            cursor: None,
        }),
        &GraphLimits::default(),
        &context,
    )
    .unwrap();
    assert_eq!(deadline_limited.stats.edges_examined, 64);
    assert_eq!(deadline_limited.truncation, [TruncationReason::Deadline]);
    assert!(!deadline_limited.complete);
}

#[test]
fn unknown_start_and_shortest_path_endpoints_are_errors() {
    let db = TestGraph::new();
    let result = read(
        db.graph(),
        &query(QueryOp::Traverse {
            start: vec![NodeSelector::Key("missing".into())],
            direction: Direction::Out,
            types: None,
            expand_filter: NodeFilter::default(),
            return_filter: NodeFilter::default(),
            edge_filter: vec![],
            include_paths: false,
        }),
        &GraphLimits::default(),
    );
    assert_eq!(result, Err(GraphError::UnknownNodeKey("missing".into())));
    let result = read(
        db.graph(),
        &path_query("missing", "also-missing", Direction::Out),
        &GraphLimits::default(),
    );
    assert_eq!(result, Err(GraphError::UnknownNodeKey("missing".into())));
}
