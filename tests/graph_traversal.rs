//! EPIC-005 traversal integration test (ITEM-020, TEST-006), built only with
//! the `graph` feature.
//!
//! Seeded random graphs (self-loops, parallel edges, deletions, labels and
//! properties) are queried through the public `GraphRef` reads and compared
//! with the independent breadth-first reference model in `graph_model`:
//! exact equality for complete reads in all three directions with type,
//! expand, return and edge filters, before and after reopening read-only.
//! Truncated reads are never `complete` and never report anything the
//! reference does not. A 100,000-edge hub exercises paging, `DegreeLimit`
//! and certified shortest-path exhaustion; cancellation and ceilings are
//! checked through the public API. Deadline expiry is covered with an
//! injected clock by the engine tests (the public API uses the real clock).

mod graph_model;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::AtomicBool;

use graph_model::{Dir, Edge, Filter, Model, Node, Query};
use tempfile::tempdir;
use vlite::property_graph::{
    Direction, EdgeRef, FindNodes, GraphLimits, GraphOp, GraphOpOutcome, GraphRead, GraphRef,
    Neighbors, NodeFilter, NodeSelector, PathOutcome, ReadOptions, ShortestPath, Traversal,
    Traverse, TruncationReason,
};
use vlite::{Condition, CreateOptions, Database, Error, Expected, Metadata, MetadataValue};

/// SplitMix64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    fn chance(&mut self, one_in: u64) -> bool {
        self.below(one_in) == 0
    }
}

const LABELS: [&str; 2] = ["A", "B"];
const TYPES: [&str; 3] = ["R", "S", "T"];

/// Generous limits under which every read of the small graphs is complete.
const WIDE: GraphLimits = GraphLimits {
    max_hops: 2,
    deadline_ms: 5_000,
    ..GraphLimits::CEILING
};

fn graph_db(path: &Path) -> Database {
    let mut db = Database::create_with_graph(path, 2, CreateOptions::default()).unwrap();
    db.create_graph("g").unwrap();
    db
}

fn properties(rng: &mut Rng) -> Metadata {
    let mut properties = Metadata::new();
    if !rng.chance(4) {
        properties.insert("w".into(), MetadataValue::Integer(rng.below(4) as i64));
    }
    if rng.chance(2) {
        let value = if rng.chance(2) { "x" } else { "y" };
        properties.insert("s".into(), MetadataValue::String(value.into()));
    }
    properties
}

fn key_of(id: u64, keys: &BTreeMap<u64, String>) -> String {
    keys[&id].clone()
}

/// Builds a random graph in `db` and the same graph in the model. Returns
/// the model and the node keys by node ID.
fn random_graph(db: &mut Database, rng: &mut Rng) -> (Model, BTreeMap<u64, String>) {
    let node_count = 12 + rng.below(20);
    let mut model = Model::default();
    let mut keys = BTreeMap::new();
    let mut ops = Vec::new();
    let mut created = Vec::new();
    for index in 0..node_count {
        let labels: BTreeSet<String> = LABELS
            .iter()
            .filter(|_| rng.chance(2))
            .map(|label| label.to_string())
            .collect();
        let properties = properties(rng);
        ops.push(GraphOp::CreateNode {
            key: format!("n{index}"),
            labels: labels.iter().cloned().collect(),
            properties: properties.clone(),
        });
        created.push(Node { labels, properties });
    }
    let result = db.graph_mut("g").unwrap().write_batch(&ops).unwrap();
    for (index, (outcome, node)) in result.outcomes.iter().zip(created).enumerate() {
        let GraphOpOutcome::Node(write) = outcome else {
            panic!("{outcome:?}")
        };
        keys.insert(write.id, format!("n{index}"));
        model.nodes.insert(write.id, node);
    }
    let ids: Vec<u64> = keys.keys().copied().collect();

    // Edges in a few batches, with self-loops and parallel edges.
    let edge_count = node_count * (1 + rng.below(4));
    let mut created: Vec<(String, Edge)> = Vec::new();
    for index in 0..edge_count {
        let source = ids[rng.below(ids.len() as u64) as usize];
        let target = if rng.chance(10) {
            source
        } else if rng.chance(8) && !created.is_empty() {
            // A parallel edge (or a reversed one) to an existing edge.
            let (_, other) = &created[rng.below(created.len() as u64) as usize];
            if rng.chance(2) {
                other.target
            } else {
                other.source
            }
        } else {
            ids[rng.below(ids.len() as u64) as usize]
        };
        created.push((
            format!("e{index}"),
            Edge {
                source,
                target,
                relationship: TYPES[rng.below(3) as usize].into(),
                properties: properties(rng),
            },
        ));
    }
    for chunk in created.chunks(17) {
        let ops: Vec<GraphOp> = chunk
            .iter()
            .map(|(key, edge)| GraphOp::CreateEdge {
                key: Some(key.clone()),
                source: key_of(edge.source, &keys),
                relationship: edge.relationship.clone(),
                destination: key_of(edge.target, &keys),
                properties: edge.properties.clone(),
            })
            .collect();
        let result = db.graph_mut("g").unwrap().write_batch(&ops).unwrap();
        for (outcome, (_, edge)) in result.outcomes.iter().zip(chunk) {
            let GraphOpOutcome::Edge(write) = outcome else {
                panic!("{outcome:?}")
            };
            model.edges.insert(write.id, edge.clone());
        }
    }

    // Delete about a tenth of the edges, then any node left without edges
    // with probability one half (deleted IDs must never reappear).
    let doomed: Vec<u64> = model
        .edges
        .keys()
        .copied()
        .filter(|_| rng.chance(10))
        .collect();
    let mut graph = db.graph_mut("g").unwrap();
    for id in doomed {
        graph.delete_edge(EdgeRef::Id(id), Expected::Any).unwrap();
        model.edges.remove(&id);
    }
    let isolated: Vec<u64> = ids
        .iter()
        .copied()
        .filter(|id| {
            !model
                .edges
                .values()
                .any(|edge| edge.source == *id || edge.target == *id)
        })
        .collect();
    for id in isolated {
        if rng.chance(2) {
            graph.delete_node(&keys[&id], Expected::Any).unwrap();
            model.nodes.remove(&id);
            keys.remove(&id);
        }
    }
    (model, keys)
}

fn direction(dir: Dir) -> Direction {
    match dir {
        Dir::Out => Direction::Out,
        Dir::In => Direction::In,
        Dir::Both => Direction::Both,
    }
}

fn random_conditions(rng: &mut Rng) -> Vec<Condition> {
    let mut conditions = Vec::new();
    if rng.chance(3) {
        conditions.push(Condition::Range {
            field: "w".into(),
            min: Some(MetadataValue::Integer(rng.below(2) as i64)),
            max: if rng.chance(2) {
                Some(MetadataValue::Integer(2 + rng.below(2) as i64))
            } else {
                None
            },
        });
    }
    if rng.chance(5) {
        conditions.push(Condition::Eq {
            field: "s".into(),
            value: MetadataValue::String("x".into()),
        });
    }
    conditions
}

fn random_filter(rng: &mut Rng) -> Filter {
    Filter {
        labels: if rng.chance(4) {
            vec![LABELS[rng.below(2) as usize].into()]
        } else {
            Vec::new()
        },
        conditions: random_conditions(rng),
    }
}

fn random_query(rng: &mut Rng, keys: &BTreeMap<u64, String>) -> Query {
    let ids: Vec<u64> = keys.keys().copied().collect();
    let start = (0..1 + rng.below(3))
        .map(|_| ids[rng.below(ids.len() as u64) as usize])
        .collect();
    let types = if rng.chance(3) {
        let types: Vec<String> = TYPES
            .iter()
            .filter(|_| rng.chance(2))
            .map(|kind| kind.to_string())
            .collect();
        (!types.is_empty()).then_some(types)
    } else {
        None
    };
    let expand = if rng.chance(3) {
        random_filter(rng)
    } else {
        Filter::default()
    };
    Query {
        start,
        dir: [Dir::Out, Dir::In, Dir::Both][rng.below(3) as usize],
        types,
        expand,
        ret: if rng.chance(3) {
            random_filter(rng)
        } else {
            Filter::default()
        },
        edge: if rng.chance(3) {
            random_conditions(rng)
        } else {
            Vec::new()
        },
        max_hops: rng.below(5),
    }
}

fn node_filter(filter: &Filter) -> NodeFilter {
    NodeFilter {
        labels: filter.labels.clone(),
        conditions: filter.conditions.clone(),
    }
}

/// The public request for `query`, naming start nodes by key or by ID.
fn traverse_request(query: &Query, keys: &BTreeMap<u64, String>, rng: &mut Rng) -> Traverse {
    let mut request = Traverse::new(
        query
            .start
            .iter()
            .map(|id| {
                if rng.chance(2) {
                    NodeSelector::Id(*id)
                } else {
                    NodeSelector::key(&keys[id])
                }
            })
            .collect(),
    );
    request.direction = direction(query.dir);
    request.types = query.types.clone();
    request.expand_filter = node_filter(&query.expand);
    request.return_filter = node_filter(&query.ret);
    request.edge_filter = query.edge.clone();
    request.include_paths = true;
    request
}

fn options(limits: GraphLimits) -> ReadOptions<'static> {
    ReadOptions {
        limits,
        ..ReadOptions::default()
    }
}

fn answer(read: &GraphRead<Traversal>) -> graph_model::Answer {
    graph_model::Answer {
        nodes: read
            .output
            .nodes
            .iter()
            .map(|node| (node.node.id, node.depth))
            .collect(),
        edges: read.output.edges.iter().map(|edge| edge.id).collect(),
        paths: read
            .output
            .paths
            .iter()
            .map(|path| (path.nodes.clone(), path.edges.clone()))
            .collect(),
    }
}

fn path_answer(outcome: &PathOutcome) -> Option<(Vec<u64>, Vec<u64>)> {
    match outcome {
        PathOutcome::Found { path, nodes, edges } => {
            let ids: Vec<u64> = nodes.iter().map(|node| node.id).collect();
            assert_eq!(ids, path.nodes);
            let ids: Vec<u64> = edges.iter().map(|edge| edge.id).collect();
            assert_eq!(ids, path.edges);
            Some((path.nodes.clone(), path.edges.clone()))
        }
        PathOutcome::NoPathWithinHops => None,
        other => panic!("uncertified outcome {other:?}"),
    }
}

/// Runs `queries` exact comparisons on `graph`; returns the reads for the
/// reopen comparison.
fn compare_exact(
    graph: &GraphRef<'_>,
    model: &Model,
    keys: &BTreeMap<u64, String>,
    rng: &mut Rng,
    queries: usize,
) -> Vec<String> {
    let ids: Vec<u64> = keys.keys().copied().collect();
    let mut transcript = Vec::new();
    for _ in 0..queries {
        let query = random_query(rng, keys);
        let request = traverse_request(&query, keys, rng);
        let limits = GraphLimits {
            max_hops: query.max_hops,
            ..WIDE
        };
        let read = graph.traverse(&request, &options(limits)).unwrap();
        assert!(read.complete && read.truncation.is_empty(), "{query:?}");
        assert_eq!(read.view_sequence, graph.view_sequence());
        let expected = model.traverse(&query);
        assert_eq!(answer(&read), expected, "{query:?}");
        assert_eq!(read.stats.nodes_returned, expected.nodes.len() as u64);
        // Views are the stored nodes and edges.
        for node in &read.output.nodes {
            let stored = &model.nodes[&node.node.id];
            assert_eq!(node.node.key, keys[&node.node.id]);
            assert_eq!(node.node.labels, stored.labels);
            assert_eq!(node.node.properties, stored.properties);
        }
        for edge in &read.output.edges {
            let stored = &model.edges[&edge.id];
            assert_eq!(
                (edge.source, edge.target, &edge.relationship),
                (stored.source, stored.target, &stored.relationship)
            );
            assert_eq!(edge.properties, stored.properties);
        }
        transcript.push(format!("{:?}", read.output));

        let source = ids[rng.below(ids.len() as u64) as usize];
        let destination = ids[rng.below(ids.len() as u64) as usize];
        let mut request =
            ShortestPath::new(NodeSelector::Id(source), NodeSelector::Id(destination));
        request.direction = direction(query.dir);
        request.types = query.types.clone();
        let hops = 1 + rng.below(4);
        let limits = GraphLimits {
            max_hops: hops,
            ..WIDE
        };
        let read = graph.shortest_path(&request, &options(limits)).unwrap();
        assert!(read.complete);
        let expected = model.shortest_path(source, destination, query.dir, query.types, hops);
        assert_eq!(path_answer(&read.output), expected);
        transcript.push(format!("{:?}", read.output));
    }
    transcript
}

#[test]
fn random_graphs_match_the_reference_model_and_survive_reopen() {
    let dir = tempdir().unwrap();
    for seed in 0..12 {
        let path = dir.path().join(format!("random-{seed}.vlite"));
        let mut rng = Rng(seed);
        let mut db = graph_db(&path);
        let (model, keys) = random_graph(&mut db, &mut rng);
        let before = compare_exact(&db.graph("g").unwrap(), &model, &keys, &mut Rng(seed), 40);
        drop(db);
        let reader = Database::open_read_only(&path).unwrap();
        let after = compare_exact(
            &reader.graph("g").unwrap(),
            &model,
            &keys,
            &mut Rng(seed),
            40,
        );
        assert_eq!(before, after, "seed {seed}");
    }
}

#[test]
fn truncated_reads_are_incomplete_and_never_report_beyond_the_reference() {
    let dir = tempdir().unwrap();
    let (mut truncated, mut exhausted) = (0, 0);
    for seed in 100..110 {
        let mut rng = Rng(seed);
        let mut db = graph_db(&dir.path().join(format!("truncated-{seed}.vlite")));
        let (model, keys) = random_graph(&mut db, &mut rng);
        let graph = db.graph("g").unwrap();
        let ids: Vec<u64> = keys.keys().copied().collect();
        for _ in 0..60 {
            let query = random_query(&mut rng, &keys);
            let request = traverse_request(&query, &keys, &mut rng);
            let limits = GraphLimits {
                max_hops: query.max_hops,
                max_visited_nodes: 1 + rng.below(12),
                max_edges_examined: 1 + rng.below(40),
                max_edges_per_node: 1 + rng.below(4),
                max_output_nodes: rng.below(12),
                max_output_edges: rng.below(20),
                max_output_paths: rng.below(6),
                deadline_ms: 5_000,
            };
            let read = graph.traverse(&request, &options(limits)).unwrap();
            let expected = model.traverse(&query);
            assert_eq!(read.complete, read.truncation.is_empty());
            if read.complete {
                assert_eq!(answer(&read), expected, "{query:?}");
                continue;
            }
            truncated += 1;
            // Truncation can only drop or delay discoveries, never invent
            // nodes or edges.
            let reference: BTreeMap<u64, u64> = expected.nodes.iter().copied().collect();
            for node in &read.output.nodes {
                let depth = reference.get(&node.node.id);
                assert!(depth.is_some_and(|depth| *depth <= node.depth), "{query:?}");
            }
            let edges: BTreeSet<u64> = expected.edges.iter().copied().collect();
            for edge in &read.output.edges {
                assert!(edges.contains(&edge.id), "{query:?}");
            }
            assert!(read.output.nodes.len() as u64 <= limits.max_output_nodes);
            assert!(read.output.edges.len() as u64 <= limits.max_output_edges);
            assert!(read.output.paths.len() as u64 <= limits.max_output_paths);

            let source = ids[rng.below(ids.len() as u64) as usize];
            let destination = ids[rng.below(ids.len() as u64) as usize];
            let mut request =
                ShortestPath::new(NodeSelector::Id(source), NodeSelector::Id(destination));
            request.direction = direction(query.dir);
            let hops = 1 + rng.below(4);
            let read = graph
                .shortest_path(
                    &request,
                    &options(GraphLimits {
                        max_hops: hops,
                        ..limits
                    }),
                )
                .unwrap();
            let expected = model.shortest_path(source, destination, query.dir, None, hops);
            match &read.output {
                PathOutcome::Exhausted { truncation } => {
                    exhausted += 1;
                    assert!(!read.complete && *truncation == read.truncation);
                }
                // Certified answers are exact even under small budgets.
                outcome => {
                    assert!(read.complete);
                    assert_eq!(path_answer(outcome), expected);
                }
            }
        }
    }
    assert!(truncated > 100 && exhausted > 20, "{truncated} {exhausted}");
}

#[test]
fn a_hub_of_100k_edges_pages_and_reports_degree_limits() {
    const LEAVES: u64 = 1_000;
    const PARALLEL: u64 = 100;
    let dir = tempdir().unwrap();
    let mut db = graph_db(&dir.path().join("hub.vlite"));
    let mut ops = vec![GraphOp::CreateNode {
        key: "hub".into(),
        labels: Vec::new(),
        properties: Metadata::new(),
    }];
    ops.extend((0..LEAVES).map(|leaf| GraphOp::CreateNode {
        key: format!("leaf{leaf}"),
        labels: vec!["Leaf".into()],
        properties: Metadata::new(),
    }));
    ops.push(GraphOp::CreateNode {
        key: "far".into(),
        labels: Vec::new(),
        properties: Metadata::new(),
    });
    db.graph_mut("g").unwrap().write_batch(&ops).unwrap();
    let edges: Vec<GraphOp> = (0..LEAVES * PARALLEL)
        .map(|index| GraphOp::CreateEdge {
            key: None,
            source: "hub".into(),
            relationship: "HAS".into(),
            destination: format!("leaf{}", index % LEAVES),
            properties: Metadata::new(),
        })
        .collect();
    for chunk in edges.chunks(4_096) {
        db.graph_mut("g").unwrap().write_batch(chunk).unwrap();
    }
    db.graph_mut("g")
        .unwrap()
        .create_edge(None, "leaf999", "NEXT", "far", &Metadata::new())
        .unwrap();
    let graph = db.graph("g").unwrap();
    let hub = graph.get_node("hub").unwrap().output.unwrap().id;

    // Paging through every edge: ascending (type, ID), no gaps or repeats.
    let limits = GraphLimits {
        max_edges_per_node: 10_000,
        max_edges_examined: 100_000,
        ..GraphLimits::DEFAULT
    };
    let mut request = Neighbors::new(NodeSelector::key("hub"));
    request.limit = Some(2_000);
    let mut seen = Vec::new();
    let mut pages = 0;
    loop {
        let read = graph.neighbors(&request, &options(limits)).unwrap();
        pages += 1;
        seen.extend(read.output.edges.iter().map(|edge| edge.id));
        assert!(read.output.nodes.len() <= LEAVES as usize);
        match read.cursor {
            Some(cursor) => {
                assert_eq!(read.truncation, vec![TruncationReason::OutputLimit]);
                request.cursor = Some(cursor);
            }
            None => {
                assert!(read.complete);
                break;
            }
        }
    }
    assert_eq!(seen.len() as u64, LEAVES * PARALLEL);
    assert!(seen.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(pages, 50);

    // Default limits stop at the hub's degree and say so.
    let neighbors = graph
        .neighbors(
            &Neighbors {
                limit: Some(2_000),
                ..Neighbors::new(NodeSelector::Id(hub))
            },
            &options(GraphLimits::DEFAULT),
        )
        .unwrap();
    assert_eq!(
        neighbors.truncation,
        vec![TruncationReason::DegreeLimit {
            node_ids: vec![hub]
        }]
    );
    assert_eq!(neighbors.output.edges.len(), 1_000);
    assert!(neighbors.cursor.is_some());

    // Default limits: the 1,000th distinct leaf exceeds `max_visited_nodes`,
    // and 999 leaves exceed `max_output_nodes`.
    let traversal = graph
        .traverse(
            &Traverse::new(vec![NodeSelector::Id(hub)]),
            &ReadOptions::default(),
        )
        .unwrap();
    assert!(!traversal.complete);
    assert_eq!(
        traversal.truncation,
        vec![
            TruncationReason::VisitedLimit,
            TruncationReason::OutputLimit
        ]
    );
    // With room for every leaf, the hub's degree is what stops expansion.
    let roomy = GraphLimits {
        max_visited_nodes: 10_000,
        ..GraphLimits::DEFAULT
    };
    let traversal = graph
        .traverse(&Traverse::new(vec![NodeSelector::Id(hub)]), &options(roomy))
        .unwrap();
    assert_eq!(
        traversal.truncation,
        vec![
            TruncationReason::DegreeLimit {
                node_ids: vec![hub]
            },
            TruncationReason::OutputLimit
        ]
    );
    assert_eq!(traversal.stats.nodes_visited, 1_002);

    // The path hub -> leaf999 -> far exists, but the hub's adjacency cannot
    // be certified (its degree exceeds even the ceiling per node):
    // `Exhausted`, never a false answer.
    let request = ShortestPath::new(NodeSelector::key("hub"), NodeSelector::key("far"));
    let ceiling = GraphLimits {
        max_visited_nodes: 10_000,
        ..limits
    };
    for (limits, reason) in [
        (GraphLimits::DEFAULT, TruncationReason::VisitedLimit),
        (
            ceiling,
            TruncationReason::DegreeLimit {
                node_ids: vec![hub],
            },
        ),
    ] {
        let read = graph.shortest_path(&request, &options(limits)).unwrap();
        assert_eq!(
            read.output,
            PathOutcome::Exhausted {
                truncation: vec![reason]
            }
        );
        assert!(!read.complete);
    }
    // Backwards from `far` the search is certified, and the canonical path
    // takes the smallest edge ID among the 100 parallel hub -> leaf999 edges.
    let mut request = ShortestPath::new(NodeSelector::key("far"), NodeSelector::key("hub"));
    request.direction = Direction::In;
    let read = graph
        .shortest_path(&request, &ReadOptions::default())
        .unwrap();
    assert!(read.complete);
    let PathOutcome::Found { path, nodes, .. } = read.output else {
        panic!("{:?}", read.output)
    };
    let keys: Vec<&str> = nodes.iter().map(|node| node.key.as_str()).collect();
    assert_eq!(keys, ["far", "leaf999", "hub"]);
    let next = graph
        .neighbors(
            &Neighbors::new(NodeSelector::key("leaf999")),
            &ReadOptions::default(),
        )
        .unwrap();
    assert_eq!(next.output.edges.len(), 1);
    assert_eq!(next.output.edges[0].id, path.edges[0]);
    let incoming = graph
        .neighbors(
            &Neighbors {
                direction: Direction::In,
                limit: Some(1),
                ..Neighbors::new(NodeSelector::key("leaf999"))
            },
            &ReadOptions::default(),
        )
        .unwrap();
    assert_eq!(incoming.output.edges[0].source, hub);
    assert_eq!(incoming.output.edges[0].id, path.edges[1]);
}

#[test]
fn cursors_go_stale_on_graph_writes_and_survive_reopen() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("cursor.vlite");
    let mut db = graph_db(&path);
    let ops: Vec<GraphOp> = (0..10)
        .map(|index| GraphOp::CreateNode {
            key: format!("n{index}"),
            labels: vec!["Doc".into()],
            properties: Metadata::new(),
        })
        .collect();
    db.graph_mut("g").unwrap().write_batch(&ops).unwrap();
    let request = FindNodes {
        label: Some("Doc".into()),
        limit: Some(3),
        ..FindNodes::default()
    };
    let first = db
        .graph("g")
        .unwrap()
        .find_nodes(&request, &ReadOptions::default())
        .unwrap();
    let cursor = first.cursor.clone().unwrap();
    // A vector write does not touch the graph: the cursor stays valid.
    db.put(7, &[1.0, 0.0], b"").unwrap();
    drop(db);
    let reader = Database::open_read_only(&path).unwrap();
    let next = FindNodes {
        cursor: Some(cursor.clone()),
        ..request.clone()
    };
    let second = reader
        .graph("g")
        .unwrap()
        .find_nodes(&next, &ReadOptions::default())
        .unwrap();
    let keys: Vec<&str> = second.output.iter().map(|node| node.key.as_str()).collect();
    assert_eq!(keys, ["n3", "n4", "n5"]);
    // A cursor is bound to its query.
    let other = FindNodes {
        limit: Some(4),
        ..next.clone()
    };
    assert!(matches!(
        reader
            .graph("g")
            .unwrap()
            .find_nodes(&other, &ReadOptions::default()),
        Err(Error::InvalidQuery(_))
    ));
    drop(reader);

    let mut db = Database::open(&path).unwrap();
    db.graph_mut("g")
        .unwrap()
        .create_node("late", &["Doc"], &Metadata::new())
        .unwrap();
    assert!(matches!(
        db.graph("g")
            .unwrap()
            .find_nodes(&next, &ReadOptions::default()),
        Err(Error::GraphCursorStale)
    ));
}

#[test]
fn cancellation_and_ceilings_through_the_public_api() {
    let dir = tempdir().unwrap();
    let mut db = graph_db(&dir.path().join("cancel.vlite"));
    let (_, keys) = random_graph(&mut db, &mut Rng(7));
    let graph = db.graph("g").unwrap();
    let start = NodeSelector::key(keys.values().next().unwrap());
    let cancel = AtomicBool::new(true);
    let cancelled = ReadOptions {
        cancel: Some(&cancel),
        ..ReadOptions::default()
    };
    let read = graph
        .traverse(&Traverse::new(vec![start.clone()]), &cancelled)
        .unwrap();
    assert!(!read.complete);
    assert_eq!(read.truncation, vec![TruncationReason::Cancelled]);
    assert!(read.output.nodes.is_empty());
    let read = graph
        .shortest_path(&ShortestPath::new(start.clone(), start.clone()), &cancelled)
        .unwrap();
    assert_eq!(
        read.output,
        PathOutcome::Exhausted {
            truncation: vec![TruncationReason::Cancelled]
        }
    );
    let read = graph.find_nodes(&FindNodes::default(), &cancelled).unwrap();
    assert!(read.output.is_empty() && read.cursor.is_some());

    for (limits, name, requested, ceiling) in [
        (
            GraphLimits {
                max_hops: 5,
                ..GraphLimits::DEFAULT
            },
            "max_hops",
            5,
            4,
        ),
        (
            GraphLimits {
                max_visited_nodes: 10_001,
                max_output_paths: 101,
                ..GraphLimits::DEFAULT
            },
            "max_visited_nodes",
            10_001,
            10_000,
        ),
        (
            GraphLimits {
                deadline_ms: 5_001,
                ..GraphLimits::DEFAULT
            },
            "deadline_ms",
            5_001,
            5_000,
        ),
    ] {
        let error = graph
            .traverse(&Traverse::new(vec![start.clone()]), &options(limits))
            .unwrap_err();
        match error {
            Error::GraphLimitExceedsCeiling {
                limit,
                requested: got,
                ceiling: max,
            } => assert_eq!((limit, got, max), (name, requested, ceiling)),
            other => panic!("{other:?}"),
        }
    }
    let zero = GraphLimits {
        max_edges_examined: 0,
        ..GraphLimits::DEFAULT
    };
    assert!(matches!(
        graph.traverse(&Traverse::new(vec![start.clone()]), &options(zero)),
        Err(Error::InvalidQuery(_))
    ));
    let request = FindNodes {
        limit: Some(0),
        ..FindNodes::default()
    };
    assert!(matches!(
        graph.find_nodes(&request, &ReadOptions::default()),
        Err(Error::InvalidQuery(_))
    ));

    // Every limit: the ceiling itself is accepted, one more is rejected on
    // every read kind; zero is valid exactly for hops and output caps.
    type Field = fn(&mut GraphLimits) -> &mut u64;
    let fields: [(&str, Field, bool); 8] = [
        ("max_hops", |l| &mut l.max_hops, true),
        ("max_visited_nodes", |l| &mut l.max_visited_nodes, false),
        ("max_edges_examined", |l| &mut l.max_edges_examined, false),
        ("max_edges_per_node", |l| &mut l.max_edges_per_node, false),
        ("deadline_ms", |l| &mut l.deadline_ms, false),
        ("max_output_nodes", |l| &mut l.max_output_nodes, true),
        ("max_output_edges", |l| &mut l.max_output_edges, true),
        ("max_output_paths", |l| &mut l.max_output_paths, true),
    ];
    let traverse = Traverse::new(vec![start.clone()]);
    let neighbors = Neighbors::new(start.clone());
    let shortest = ShortestPath::new(start.clone(), start.clone());
    for (name, field, zero_ok) in fields {
        let mut limits = GraphLimits::DEFAULT;
        let ceiling = *field(&mut { GraphLimits::CEILING });
        *field(&mut limits) = ceiling;
        assert!(
            graph.traverse(&traverse, &options(limits)).is_ok(),
            "{name}"
        );
        *field(&mut limits) = ceiling + 1;
        let errors = [
            graph.traverse(&traverse, &options(limits)).map(drop),
            graph.neighbors(&neighbors, &options(limits)).map(drop),
            graph.shortest_path(&shortest, &options(limits)).map(drop),
            graph
                .find_nodes(&FindNodes::default(), &options(limits))
                .map(drop),
        ];
        for error in errors {
            assert!(
                matches!(error, Err(Error::GraphLimitExceedsCeiling { limit, .. }) if limit == name),
                "{name}: {error:?}"
            );
        }
        *field(&mut limits) = 0;
        let result = graph.traverse(&traverse, &options(limits));
        assert_eq!(result.is_ok(), zero_ok, "{name}: {result:?}");
    }
}

#[test]
fn self_loops_appear_once_in_every_direction() {
    let dir = tempdir().unwrap();
    let mut db = graph_db(&dir.path().join("loop.vlite"));
    let mut graph = db.graph_mut("g").unwrap();
    graph.create_node("a", &[], &Metadata::new()).unwrap();
    graph.create_node("b", &[], &Metadata::new()).unwrap();
    let looped = graph
        .create_edge(Some("loop"), "a", "R", "a", &Metadata::new())
        .unwrap()
        .id;
    let out = graph
        .create_edge(Some("out"), "a", "R", "b", &Metadata::new())
        .unwrap()
        .id;
    let graph = db.graph("g").unwrap();
    for (dir, edges) in [
        (Direction::Out, vec![looped, out]),
        (Direction::In, vec![looped]),
        (Direction::Both, vec![looped, out]),
    ] {
        let read = graph
            .neighbors(
                &Neighbors {
                    direction: dir,
                    ..Neighbors::new(NodeSelector::key("a"))
                },
                &ReadOptions::default(),
            )
            .unwrap();
        let ids: Vec<u64> = read.output.edges.iter().map(|edge| edge.id).collect();
        assert_eq!(ids, edges, "{dir:?}");
        let mut request = Traverse::new(vec![NodeSelector::key("a")]);
        request.direction = dir;
        request.include_paths = true;
        let read = graph.traverse(&request, &ReadOptions::default()).unwrap();
        let ids: Vec<u64> = read.output.edges.iter().map(|edge| edge.id).collect();
        assert_eq!(ids, edges, "{dir:?}");
        let nodes: Vec<&str> = read
            .output
            .nodes
            .iter()
            .map(|node| node.node.key.as_str())
            .collect();
        let expected: &[&str] = if dir == Direction::In {
            &["a"]
        } else {
            &["a", "b"]
        };
        assert_eq!(nodes, expected);
        assert!(
            read.output
                .paths
                .iter()
                .all(|path| path.nodes[0] != path.nodes[1])
        );
    }
}
