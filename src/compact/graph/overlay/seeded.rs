use super::*;
use crate::property_graph::engine::{
    EdgeSelector, EdgeView, ExecutionContext, GraphError, GraphLimits, GraphQuery, NodeSelector,
    NodeView, QueryOp, QueryOutput, execute, execute_as_of,
};

const SEEDS: [u64; 4] = [
    0x4d59_5df4_d0f3_3173,
    0x1234_5678_9abc_def0,
    0x9e37_79b9_7f4a_7c15,
    0xd1b5_4a32_d192_ed03,
];

struct Prng(u64);

impl Prng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

fn payload(seed: u64, step: u64) -> Properties {
    Properties::from([
        (
            "number".into(),
            PropertyValue::Integer((seed ^ step) as i64),
        ),
        (
            "opaque".into(),
            PropertyValue::String(format!("节点\0{seed:016x}\n{step}")),
        ),
        (
            "flag".into(),
            PropertyValue::Boolean(step.is_multiple_of(2)),
        ),
        ("float".into(), PropertyValue::Float(-0.125 * step as f64)),
        (
            "point".into(),
            PropertyValue::GeoPoint {
                latitude: 12.5,
                longitude: -179.75,
            },
        ),
    ])
}

fn payload_without(seed: u64, step: u64, field: &str) -> Properties {
    let mut properties = payload(seed, step);
    properties.remove(field);
    properties
}

fn put_node(id: u64, key: &str, seed: u64, step: u64) -> GraphEntry {
    GraphEntry::PutNode {
        node_id: NodeId(id),
        key: key.into(),
        labels: BTreeSet::from(["Entity".into()]),
        properties: payload(seed, step),
    }
}

fn put_edge(
    id: u64,
    source: u64,
    target: u64,
    relationship: &str,
    key: Option<&str>,
    seed: u64,
) -> GraphEntry {
    GraphEntry::PutEdge {
        edge_id: EdgeId(id),
        source: NodeId(source),
        destination: NodeId(target),
        relationship: relationship.into(),
        key: key.map(str::to_owned),
        properties: payload(seed, id),
    }
}

fn segment(seed: u64, from: Option<i64>, to: Option<i64>, step: u64) -> Segment {
    Segment {
        valid_from: from,
        valid_to: to,
        properties: payload(seed, step),
    }
}

fn apply_engine(state: &mut GraphState, sequence: u64, entries: Vec<GraphEntry>) {
    state
        .apply(
            sequence,
            &GraphBody {
                graph_id: GraphId(0),
                entries,
            },
        )
        .unwrap_or_else(|error| panic!("engine commit {sequence}: {error:?}"));
}

fn source(state: &GraphState) -> GraphSource {
    GraphSource::from_graph(state.graph(GraphId(0)).unwrap()).unwrap()
}

fn base_state(seed: u64) -> GraphState {
    let schema = GraphSchema::default()
        .label(
            "Entity",
            LabelDefinition::new("seeded entity").property(
                "number",
                PropertyDefinition::new(PropertyType::Integer)
                    .required()
                    .index(PropertyIndexKind::Range),
            ),
        )
        .relationship(
            "R",
            RelationshipDefinition::new("parallel edges allowed").endpoint("Entity", "Entity"),
        );
    let mut entries = vec![
        GraphEntry::CreateGraph {
            name: format!("overlay_{seed:016x}"),
        },
        GraphEntry::PutSchema { schema },
    ];
    for index in 0..6 {
        entries.push(put_node(
            (index + 1) * 10,
            &format!("n{index}"),
            seed,
            index,
        ));
    }
    entries.extend([
        GraphEntry::NodeExpiry {
            node_id: NodeId(20),
            expires_at_ms: Some(7),
        },
        GraphEntry::NodeExpiry {
            node_id: NodeId(60),
            expires_at_ms: Some(5),
        },
        GraphEntry::Attach {
            node_id: NodeId(60),
            collection: 2,
            record_id: seed,
        },
        put_edge(10, 10, 20, "R", Some("parallel_a"), seed),
        put_edge(20, 10, 20, "R", Some("parallel_b"), seed),
        put_edge(30, 10, 10, "R", Some("loop"), seed),
        put_edge(40, 20, 30, "A", None, seed),
        put_edge(50, 60, 10, "R", Some("reclaim_me"), seed),
        put_edge(60, 30, 40, "Z", Some("deleted_edge"), seed),
        GraphEntry::Temporal(TemporalEntry::PutEdge {
            edge_id: EdgeId(70),
            revision: 0,
            source: NodeId(40),
            destination: NodeId(40),
            relationship: "T".into(),
            key: Some("temporal".into()),
            segments: vec![
                segment(seed, None, Some(-2), 1),
                segment(seed, Some(0), Some(5), 2),
                segment(seed, Some(5), None, 3),
            ],
        }),
        GraphEntry::EdgeExpiry {
            edge_id: EdgeId(20),
            expires_at_ms: Some(9),
        },
        GraphEntry::EdgeExpiry {
            edge_id: EdgeId(30),
            expires_at_ms: Some(7),
        },
    ]);
    let mut state = GraphState::new();
    state.set_temporal(true).unwrap();
    apply_engine(&mut state, 3, entries);
    state
}

// Diff engine snapshots by ID, never using the overlay's source/fold to plan
// changes. Unchanged rows are omitted, including when a high-water mark moves.
fn projection(before: &GraphSource, after: &GraphSource) -> Commit {
    let old_nodes: BTreeMap<_, _> = before.nodes.iter().map(|row| (row.id, row)).collect();
    let new_nodes: BTreeMap<_, _> = after.nodes.iter().map(|row| (row.id, row)).collect();
    let old_edges: BTreeMap<_, _> = before.edges.iter().map(|row| (row.id, row)).collect();
    let new_edges: BTreeMap<_, _> = after.edges.iter().map(|row| (row.id, row)).collect();
    let node_ids: BTreeSet<_> = old_nodes.keys().chain(new_nodes.keys()).copied().collect();
    let edge_ids: BTreeSet<_> = old_edges.keys().chain(new_edges.keys()).copied().collect();
    let nodes = node_ids
        .into_iter()
        .rev()
        .filter_map(|id| {
            let old = old_nodes.get(&id).copied();
            let new = new_nodes.get(&id).copied();
            if old == new {
                return None;
            }
            let preserve = old.zip(new).is_some_and(|(a, b)| a.deadline == b.deadline);
            let mut row = new.cloned();
            if preserve {
                // Preserve must ignore the snapshot's deadline field.
                row.as_mut().unwrap().deadline = Some(123_456);
            }
            Some(Change {
                id,
                row,
                deadline: if preserve {
                    Deadline::Preserve
                } else {
                    Deadline::Set(new.and_then(|row| row.deadline))
                },
            })
        })
        .collect();
    let edges = edge_ids
        .into_iter()
        .rev()
        .filter_map(|id| {
            let old = old_edges.get(&id).copied();
            let new = new_edges.get(&id).copied();
            if old == new {
                return None;
            }
            let preserve = old.zip(new).is_some_and(|(a, b)| a.deadline == b.deadline);
            let mut row = new.cloned();
            if preserve {
                row.as_mut().unwrap().deadline = Some(123_456);
            }
            Some(Change {
                id,
                row,
                deadline: if preserve {
                    Deadline::Preserve
                } else {
                    Deadline::Set(new.and_then(|row| row.deadline))
                },
            })
        })
        .collect();
    Commit {
        sequence: after.sequence,
        nodes,
        edges,
        high_water: Some((after.next_node_id, after.next_edge_id)),
    }
}

fn node_view(node: &SourceNode) -> NodeView {
    NodeView {
        id: NodeId(node.id),
        key: node.key.clone().unwrap(),
        labels: node.labels.iter().cloned().collect(),
        properties: node.properties.clone(),
        revision: node.revision,
        expires_at_ms: node.deadline,
    }
}

fn edge_view(state: &GraphState, read: ReadEdge<'_>) -> EdgeView {
    let graph = state.graph(GraphId(0)).unwrap();
    let edge = read.edge;
    EdgeView {
        id: EdgeId(edge.id),
        key: edge.key.clone(),
        relationship: edge.relationship.clone(),
        source: NodeId(edge.source),
        source_key: graph.node(NodeId(edge.source)).unwrap().key.clone(),
        target: NodeId(edge.target),
        target_key: graph.node(NodeId(edge.target)).unwrap().key.clone(),
        properties: read.properties.clone(),
        revision: edge.revision,
        expires_at_ms: edge.deadline,
    }
}

fn query(
    state: &GraphState,
    op: QueryOp,
    clock: u64,
    as_of: Option<i64>,
) -> Result<QueryOutput, GraphError> {
    let timer = || 0;
    let mut context = ExecutionContext::new(1, &timer);
    context.now_ms = clock;
    let graph = state.graph(GraphId(0)).unwrap();
    let query = GraphQuery::new(op);
    let limits = GraphLimits::default();
    match as_of {
        Some(time) => execute_as_of(graph, &query, &limits, &context, time),
        None => execute(graph, &query, &limits, &context),
    }
    .map(|result| result.output)
}

#[derive(Default)]
struct Seen {
    nodes: BTreeSet<u64>,
    edges: BTreeSet<u64>,
    node_keys: BTreeSet<String>,
    edge_keys: BTreeSet<String>,
}

impl Seen {
    fn include(&mut self, source: &GraphSource) {
        self.nodes.extend(source.nodes.iter().map(|row| row.id));
        self.edges.extend(source.edges.iter().map(|row| row.id));
        self.node_keys
            .extend(source.nodes.iter().filter_map(|row| row.key.clone()));
        self.edge_keys
            .extend(source.edges.iter().filter_map(|row| row.key.clone()));
    }
}

fn assert_parity(overlay: &Overlay<'_>, state: &GraphState, seen: &Seen) {
    let graph = state.graph(GraphId(0)).unwrap();
    let filters = [
        None,
        Some(BTreeSet::from(["R".into(), "T".into()])),
        Some(BTreeSet::from(["missing".into()])),
    ];
    for clock in [0, 4, 5, 6, 7, 8, 9, 10, u64::MAX - 1, u64::MAX] {
        assert_eq!(
            overlay.counts(clock),
            graph.live_counts(clock),
            "counts, sequence {}, clock {clock}",
            graph.graph_sequence()
        );
        for &id in &seen.nodes {
            let QueryOutput::Node(expected) = query(
                state,
                QueryOp::GetNode {
                    node: NodeSelector::Id(NodeId(id)),
                },
                clock,
                None,
            )
            .unwrap() else {
                panic!("expected node output");
            };
            assert_eq!(
                overlay.node(id, clock).map(node_view),
                expected,
                "node {id}"
            );
        }
        for key in &seen.node_keys {
            let QueryOutput::Node(expected) = query(
                state,
                QueryOp::GetNode {
                    node: NodeSelector::Key(key.clone()),
                },
                clock,
                None,
            )
            .unwrap() else {
                panic!("expected node output");
            };
            assert_eq!(
                overlay.node_by_key(key, clock).map(node_view),
                expected,
                "node key {key}, clock {clock}"
            );
        }
        for as_of in [
            None,
            Some(i64::MIN),
            Some(-2),
            Some(-1),
            Some(0),
            Some(4),
            Some(5),
            Some(i64::MAX),
        ] {
            for &id in &seen.edges {
                let QueryOutput::Edge(expected) = query(
                    state,
                    QueryOp::GetEdge {
                        edge: EdgeSelector::Id(EdgeId(id)),
                    },
                    clock,
                    as_of,
                )
                .unwrap() else {
                    panic!("expected edge output");
                };
                assert_eq!(
                    overlay
                        .edge(id, clock, as_of)
                        .map(|read| edge_view(state, read)),
                    expected,
                    "edge {id}, clock {clock}, as_of {as_of:?}"
                );
            }
            for key in &seen.edge_keys {
                let QueryOutput::Edge(expected) = query(
                    state,
                    QueryOp::GetEdge {
                        edge: EdgeSelector::Key(key.clone()),
                    },
                    clock,
                    as_of,
                )
                .unwrap() else {
                    panic!("expected edge output");
                };
                assert_eq!(
                    overlay
                        .edge_by_key(key, clock, as_of)
                        .map(|read| edge_view(state, read)),
                    expected,
                    "edge key {key}, clock {clock}, as_of {as_of:?}"
                );
            }
            for &node in &seen.nodes {
                for direction in [Direction::Out, Direction::In, Direction::Both] {
                    for types in &filters {
                        let actual: Vec<_> = overlay
                            .neighbors(node, direction, types.as_ref(), clock, as_of)
                            .into_iter()
                            .map(|read| edge_view(state, read))
                            .collect();
                        match query(
                            state,
                            QueryOp::Neighbors {
                                node: NodeSelector::Id(NodeId(node)),
                                direction,
                                types: types.clone(),
                                edge_conditions: Vec::new(),
                                limit: None,
                                cursor: None,
                            },
                            clock,
                            as_of,
                        ) {
                            Ok(QueryOutput::Neighbors { edges, .. }) => assert_eq!(
                                actual, edges,
                                "neighbors {node}, {direction:?}, {types:?}, clock {clock}, as_of {as_of:?}"
                            ),
                            Err(GraphError::NodeNotFound(id)) => {
                                assert_eq!(id, NodeId(node));
                                assert!(graph.node(id).is_none() || graph.node_expired(id, clock));
                                assert!(actual.is_empty());
                            }
                            other => panic!("unexpected neighbors result: {other:?}"),
                        }
                    }
                }
            }
        }
    }
}

fn commit_and_check(
    state: &mut GraphState,
    overlay: &mut Overlay<'_>,
    seen: &mut Seen,
    sequence: u64,
    entries: Vec<GraphEntry>,
) {
    let before = source(state);
    apply_engine(state, sequence, entries);
    let after = source(state);
    let commit = projection(&before, &after);
    overlay
        .apply(commit)
        .unwrap_or_else(|error| panic!("projection commit {sequence}: {error:?}"));
    seen.include(&after);
    assert_parity(overlay, state, seen);
    let folded = overlay.fold().unwrap();
    assert_eq!(
        encode(&folded.source).unwrap(),
        encode(&after).unwrap(),
        "physical fold differs at sequence {sequence}"
    );
    assert_eq!(folded.source, after, "full snapshots/revisions/attachments");
}

#[test]
fn seeded_committed_histories_match_engine_queries_and_direct_encoded_fold() {
    for seed in SEEDS {
        let mut state = base_state(seed);
        let initial = source(&state);
        let base = decode(&encode(&initial).unwrap()).unwrap();
        assert!(!base.source.nodes.is_empty() && !base.source.edges.is_empty());
        let mut overlay = Overlay::new(&base, 128);
        let mut seen = Seen::default();
        seen.include(&initial);
        seen.nodes.insert(u64::MAX);
        seen.edges.insert(u64::MAX);
        seen.node_keys.insert("unknown-node".into());
        seen.edge_keys.insert("unknown-edge".into());
        assert_parity(&overlay, &state, &seen);

        commit_and_check(
            &mut state,
            &mut overlay,
            &mut seen,
            11,
            vec![
                GraphEntry::PatchNode {
                    node_id: NodeId(20),
                    add_labels: BTreeSet::from(["Extra".into()]),
                    remove_labels: BTreeSet::new(),
                    set: payload(seed, 11),
                    unset: BTreeSet::new(),
                },
                GraphEntry::PatchEdge {
                    edge_id: EdgeId(20),
                    set: payload_without(seed, 11, "flag"),
                    unset: BTreeSet::from(["flag".into()]),
                },
            ],
        );
        assert_eq!(overlay.node(20, 6).unwrap().deadline, Some(7));
        assert!(overlay.node(20, 7).is_none());
        assert!(overlay.edge(10, 7, None).is_none());
        assert!(overlay.edge(20, 7, None).is_none());
        commit_and_check(
            &mut state,
            &mut overlay,
            &mut seen,
            23,
            vec![
                GraphEntry::NodeExpiry {
                    node_id: NodeId(20),
                    expires_at_ms: None,
                },
                GraphEntry::EdgeExpiry {
                    edge_id: EdgeId(30),
                    expires_at_ms: Some(5),
                },
            ],
        );
        assert!(overlay.edge(10, 7, None).is_some());
        assert!(overlay.edge(30, 5, None).is_none());
        commit_and_check(
            &mut state,
            &mut overlay,
            &mut seen,
            31,
            vec![
                GraphEntry::DeleteEdge {
                    edge_id: EdgeId(60),
                },
                GraphEntry::DeleteEdge {
                    edge_id: EdgeId(40),
                },
                GraphEntry::DeleteNode {
                    node_id: NodeId(30),
                },
                GraphEntry::Detach {
                    node_id: NodeId(60),
                    collection: 2,
                },
            ],
        );
        // Attachment-only changes keep the engine revision, not overlay sequence.
        assert_eq!(overlay.node(60, 0).unwrap().revision, 3);
        assert!(overlay.node(60, 0).unwrap().attachments.is_empty());
        commit_and_check(
            &mut state,
            &mut overlay,
            &mut seen,
            44,
            vec![
                put_node(80, "n2", seed, 44),
                put_edge(90, 80, 10, "R", Some("deleted_edge"), seed),
                put_edge(100, 80, 80, "R", Some("new_loop"), seed),
                put_edge(110, 10, 20, "R", None, seed),
            ],
        );
        assert_eq!(overlay.node_by_key("n2", 0).unwrap().id, 80);
        assert_eq!(
            overlay
                .edge_by_key("deleted_edge", 0, None)
                .unwrap()
                .edge
                .id,
            90
        );
        assert!(overlay.node(30, 0).is_none() && overlay.edge(60, 0, None).is_none());
        commit_and_check(
            &mut state,
            &mut overlay,
            &mut seen,
            51,
            vec![
                GraphEntry::NodeExpiry {
                    node_id: NodeId(80),
                    expires_at_ms: Some(7),
                },
                GraphEntry::EdgeExpiry {
                    edge_id: EdgeId(90),
                    expires_at_ms: Some(5),
                },
                GraphEntry::Attach {
                    node_id: NodeId(80),
                    collection: 2,
                    record_id: seed,
                },
            ],
        );
        for (sequence, at_ms) in [(60, 5), (71, 7)] {
            commit_and_check(
                &mut state,
                &mut overlay,
                &mut seen,
                sequence,
                vec![GraphEntry::Reclaim { at_ms }],
            );
        }

        assert!(overlay.node(60, 0).is_none() && overlay.node(80, 0).is_none());
        assert!(overlay.edge(100, 0, None).is_none());
        commit_and_check(
            &mut state,
            &mut overlay,
            &mut seen,
            80,
            vec![
                put_node(120, "n2", seed, 80),
                GraphEntry::NodeExpiry {
                    node_id: NodeId(120),
                    expires_at_ms: Some(u64::MAX),
                },
                GraphEntry::Attach {
                    node_id: NodeId(120),
                    collection: 2,
                    record_id: seed,
                },
                put_edge(130, 120, 10, "R", Some("deleted_edge"), seed),
            ],
        );
        assert_eq!(overlay.node_by_key("n2", 0).unwrap().id, 120);
        commit_and_check(
            &mut state,
            &mut overlay,
            &mut seen,
            91,
            vec![
                put_node(10, "n0", seed, 91),
                GraphEntry::Temporal(TemporalEntry::ReplaceSegment {
                    edge_id: EdgeId(70),
                    valid_from: Some(0),
                    valid_to: Some(5),
                    segment: segment(seed, Some(-1), Some(5), 91),
                }),
            ],
        );
        commit_and_check(
            &mut state,
            &mut overlay,
            &mut seen,
            102,
            vec![GraphEntry::GraphHighWater {
                next_node_id: 500,
                next_edge_id: 500,
            }],
        );

        let mut random = Prng(seed);
        for step in 0..18 {
            let snapshot = source(&state);
            let node = &snapshot.nodes[random.below(snapshot.nodes.len())];
            let ordinary: Vec<_> = snapshot
                .edges
                .iter()
                .filter(|row| row.segments.is_none())
                .collect();
            let sequence = 113 + step * 11;
            let entries = match step % 6 {
                0 => vec![GraphEntry::PatchNode {
                    node_id: NodeId(node.id),
                    add_labels: BTreeSet::new(),
                    remove_labels: BTreeSet::from(["Extra".into()]),
                    set: payload_without(random.next(), sequence, "flag"),
                    unset: BTreeSet::from(["flag".into()]),
                }],
                1 => vec![GraphEntry::NodeExpiry {
                    node_id: NodeId(node.id),
                    expires_at_ms: [None, Some(0), Some(7), Some(u64::MAX)][random.below(4)],
                }],
                2 => {
                    let target = &snapshot.nodes[random.below(snapshot.nodes.len())];
                    vec![put_edge(
                        snapshot.next_edge_id + random.next() % 3,
                        node.id,
                        target.id,
                        ["A", "R", "Z"][random.below(3)],
                        Some(&format!("random_{step}")),
                        seed,
                    )]
                }
                3 => vec![GraphEntry::PatchEdge {
                    edge_id: EdgeId(ordinary[random.below(ordinary.len())].id),
                    set: payload_without(random.next(), sequence, "opaque"),
                    unset: BTreeSet::from(["opaque".into()]),
                }],
                4 => vec![GraphEntry::DeleteEdge {
                    edge_id: EdgeId(ordinary[random.below(ordinary.len())].id),
                }],
                _ => {
                    let edge = ordinary[random.below(ordinary.len())];
                    vec![GraphEntry::EdgeExpiry {
                        edge_id: EdgeId(edge.id),
                        expires_at_ms: [None, Some(0), Some(9), Some(u64::MAX)][random.below(4)],
                    }]
                }
            };
            commit_and_check(&mut state, &mut overlay, &mut seen, sequence, entries);
        }
        assert_eq!(
            encode(&overlay.fold().unwrap().source).unwrap(),
            encode(&source(&state)).unwrap(),
            "final encoded bytes, seed {seed:016x}"
        );
        assert_eq!(base.source, initial, "immutable decoded base");
    }
}

#[test]
fn seeded_temporal_addition_and_last_series_removal_match_direct_fold() {
    for seed in SEEDS {
        let mut state = base_state(seed);
        apply_engine(
            &mut state,
            5,
            vec![GraphEntry::DeleteEdge {
                edge_id: EdgeId(70),
            }],
        );
        let initial = source(&state);
        assert!(!initial.temporal_enabled);
        let base = decode(&encode(&initial).unwrap()).unwrap();
        let mut overlay = Overlay::new(&base, 1);
        let mut seen = Seen::default();
        seen.include(&initial);
        commit_and_check(
            &mut state,
            &mut overlay,
            &mut seen,
            11,
            vec![GraphEntry::Temporal(TemporalEntry::PutEdge {
                edge_id: EdgeId(100),
                revision: 0,
                source: NodeId(10),
                destination: NodeId(10),
                relationship: "T".into(),
                key: Some("new_temporal".into()),
                segments: vec![segment(seed, Some(-1), Some(5), 11)],
            })],
        );
        assert!(overlay.fold().unwrap().source.temporal_enabled);
        commit_and_check(
            &mut state,
            &mut overlay,
            &mut seen,
            23,
            vec![GraphEntry::Temporal(TemporalEntry::RemoveSegment {
                edge_id: EdgeId(100),
                valid_from: Some(-1),
                valid_to: Some(5),
            })],
        );
        // An empty temporal series remains a physical edge, but has no as-of view.
        assert!(overlay.edge(100, 0, None).is_some());
        assert!(overlay.edge(100, 0, Some(0)).is_none());
        assert!(overlay.fold().unwrap().source.temporal_enabled);
        commit_and_check(
            &mut state,
            &mut overlay,
            &mut seen,
            31,
            vec![GraphEntry::DeleteEdge {
                edge_id: EdgeId(100),
            }],
        );
        assert!(!overlay.fold().unwrap().source.temporal_enabled);
        assert_eq!(
            overlay.edges.len(),
            1,
            "one distinct ID, including its tombstone"
        );
        assert_eq!(base.source, initial);
    }
}
