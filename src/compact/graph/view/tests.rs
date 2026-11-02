use super::super::overlay::{Change, Commit, Deadline, Overlay};
use super::*;
use crate::property_graph::engine::{
    EdgeSelector, EdgeView, ExecutionContext, GraphLimits, GraphQuery, NodeSelector, NodeView,
    QueryOp, QueryOutput, execute, execute_as_of,
};

#[path = "seeded.rs"]
mod seeded;

const G: GraphId = GraphId(3);

fn apply(state: &mut GraphState, sequence: u64, entries: Vec<GraphEntry>) {
    state
        .apply(
            sequence,
            &GraphBody {
                graph_id: G,
                entries,
            },
        )
        .unwrap();
}

fn source(state: &GraphState) -> GraphSource {
    GraphSource::from_graph(state.graph(G).unwrap()).unwrap()
}

fn fixture() -> GraphState {
    let mut state = GraphState::new();
    state.set_temporal(true).unwrap();
    let schema = GraphSchema::default()
        .label(
            "Entity",
            LabelDefinition::new("declared").property(
                "number",
                PropertyDefinition::new(PropertyType::Integer)
                    .required()
                    .index(PropertyIndexKind::Range),
            ),
        )
        .relationship(
            "R",
            RelationshipDefinition::new("parallel").endpoint("Entity", "Entity"),
        );
    let mut entries = vec![
        GraphEntry::CreateGraph {
            name: "view_fixture".into(),
        },
        GraphEntry::PutSchema { schema },
    ];
    for (id, deadline) in [
        (0, None),
        (10, Some(0)),
        (20, Some(7)),
        (100, Some(u64::MAX)),
        (u64::MAX - 1, None),
    ] {
        entries.push(GraphEntry::PutNode {
            node_id: NodeId(id),
            key: format!("n{id}"),
            labels: BTreeSet::from(["Entity".into(), format!("L{}", id % 3)]),
            properties: Properties::from([
                ("number".into(), PropertyValue::Integer(id as i64)),
                ("text".into(), PropertyValue::String(format!("value-{id}"))),
            ]),
        });
        if deadline.is_some() {
            entries.push(GraphEntry::NodeExpiry {
                node_id: NodeId(id),
                expires_at_ms: deadline,
            });
        }
    }
    for (id, from, to, kind, deadline) in [
        (0, 0, 0, "R", None),
        (7, 0, 20, "R", None),
        (9, 0, 20, "R", Some(5)),
        (20, 10, 0, "A", None),
        (30, 100, 0, "Z", Some(9)),
    ] {
        entries.push(GraphEntry::PutEdge {
            edge_id: EdgeId(id),
            source: NodeId(from),
            destination: NodeId(to),
            relationship: kind.into(),
            key: Some(format!("e{id}")),
            properties: Properties::from([("weight".into(), PropertyValue::Float(1.5))]),
        });
        if deadline.is_some() {
            entries.push(GraphEntry::EdgeExpiry {
                edge_id: EdgeId(id),
                expires_at_ms: deadline,
            });
        }
    }
    for (id, segments) in [
        (
            40,
            vec![
                Segment {
                    valid_from: None,
                    valid_to: Some(-2),
                    properties: Properties::from([("phase".into(), PropertyValue::Integer(1))]),
                },
                Segment {
                    valid_from: Some(0),
                    valid_to: Some(5),
                    properties: Properties::from([("phase".into(), PropertyValue::Integer(2))]),
                },
                Segment {
                    valid_from: Some(5),
                    valid_to: None,
                    properties: Properties::from([("phase".into(), PropertyValue::Integer(3))]),
                },
            ],
        ),
        (50, Vec::new()),
    ] {
        entries.push(GraphEntry::Temporal(TemporalEntry::PutEdge {
            edge_id: EdgeId(id),
            revision: 0,
            source: NodeId(0),
            destination: NodeId(100),
            relationship: "T".into(),
            key: Some(format!("e{id}")),
            segments,
        }));
    }
    entries.push(GraphEntry::PutEdge {
        edge_id: EdgeId(u64::MAX - 1),
        source: NodeId(0),
        destination: NodeId(u64::MAX - 1),
        relationship: "R".into(),
        key: Some(format!("e{}", u64::MAX - 1)),
        properties: Properties::from([("weight".into(), PropertyValue::Float(1.5))]),
    });
    entries.push(GraphEntry::Attach {
        node_id: NodeId(0),
        collection: 2,
        record_id: 71,
    });
    entries.push(GraphEntry::Attach {
        node_id: NodeId(20),
        collection: 1,
        record_id: 72,
    });
    apply(&mut state, 3, entries);
    state
}

fn base_view(state: &GraphState) -> CompactGraphView {
    CompactGraphView::new(Arc::new(decode(&encode(&source(state)).unwrap()).unwrap()))
}

// Same independent snapshot-diff projection used by the accepted overlay histories.
fn projection(before: &GraphSource, after: &GraphSource) -> Commit {
    fn changes<T: Clone + PartialEq>(
        before: &BTreeMap<u64, T>,
        after: &BTreeMap<u64, T>,
        deadline: impl Fn(&T) -> Option<u64>,
    ) -> Vec<Change<T>> {
        before
            .keys()
            .chain(after.keys())
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .rev()
            .filter_map(|id| {
                let old = before.get(&id);
                let new = after.get(&id);
                (old != new).then(|| Change {
                    id,
                    row: new.cloned(),
                    deadline: if old
                        .zip(new)
                        .is_some_and(|(a, b)| deadline(a) == deadline(b))
                    {
                        Deadline::Preserve
                    } else {
                        Deadline::Set(new.and_then(&deadline))
                    },
                })
            })
            .collect()
    }
    Commit {
        sequence: after.sequence,
        nodes: changes(
            &before.nodes.iter().map(|n| (n.id, n.clone())).collect(),
            &after.nodes.iter().map(|n| (n.id, n.clone())).collect(),
            |n| n.deadline,
        ),
        edges: changes(
            &before.edges.iter().map(|e| (e.id, e.clone())).collect(),
            &after.edges.iter().map(|e| (e.id, e.clone())).collect(),
            |e| e.deadline,
        ),
        high_water: Some((after.next_node_id, after.next_edge_id)),
    }
}

fn query(graph: &Graph, op: QueryOp, clock: u64, as_of: Option<i64>) -> QueryOutput {
    let timer = || 0;
    let mut context = ExecutionContext::new(1, &timer);
    context.now_ms = clock;
    let query = GraphQuery::new(op);
    let limits = GraphLimits::default();
    match as_of {
        Some(time) => execute_as_of(graph, &query, &limits, &context, time),
        None => execute(graph, &query, &limits, &context),
    }
    .unwrap()
    .output
}

fn node_view(node: NodeRef<'_>) -> NodeView {
    NodeView {
        id: node.id,
        key: node.key.unwrap().into(),
        labels: node.labels.iter().cloned().collect(),
        properties: node.properties.clone(),
        revision: node.revision,
        expires_at_ms: node.expires_at_ms,
    }
}

fn edge_view(edge: EdgeRef<'_>, graph: &Graph) -> EdgeView {
    EdgeView {
        id: edge.id,
        key: edge.key.map(str::to_owned),
        relationship: edge.relationship.into(),
        source: edge.source,
        target: edge.destination,
        source_key: graph.node(edge.source).unwrap().key.clone(),
        target_key: graph.node(edge.destination).unwrap().key.clone(),
        properties: edge.properties.clone(),
        revision: edge.revision,
        expires_at_ms: edge.expires_at_ms,
    }
}

fn ids(graph: &Graph) -> Vec<NodeId> {
    graph
        .nodes()
        .map(|n| n.id)
        .chain([
            NodeId(10),
            NodeId(20),
            NodeId(21),
            NodeId(30),
            NodeId(u64::MAX),
        ])
        .collect()
}

fn edge_ids(graph: &Graph) -> Vec<EdgeId> {
    graph
        .edges()
        .map(|e| e.id)
        .chain([
            EdgeId(7),
            EdgeId(20),
            EdgeId(21),
            EdgeId(40),
            EdgeId(60),
            EdgeId(u64::MAX),
        ])
        .collect()
}

fn metadata(view: &CompactGraphView, graph: &Graph) {
    assert_eq!(view.id(), graph.id());
    assert_eq!(view.name(), graph.name());
    assert_eq!(view.graph_sequence(), graph.graph_sequence());
    assert_eq!(view.next_node_id(), graph.next_node_id());
    assert_eq!(view.next_edge_id(), graph.next_edge_id());
    assert_eq!(view.node_count(), graph.node_count());
    assert_eq!(view.edge_count(), graph.edge_count());
    assert_eq!(view.schema(), graph.schema());
    let physical = view.physical();
    assert_eq!(
        physical.nodes().map(|n| n.id).collect::<Vec<_>>(),
        graph.nodes().map(|n| n.id).collect::<Vec<_>>()
    );
    assert_eq!(
        physical.edges().map(|e| e.id).collect::<Vec<_>>(),
        graph.edges().map(|e| e.id).collect::<Vec<_>>()
    );
    assert_eq!(
        physical.live_counts(),
        (graph.node_count(), graph.edge_count())
    );
    assert!(!physical.has_due());
}

fn nodes(view: &CompactGraphView, graph: &Graph) {
    for clock in [0, 4, 5, 6, 7, 9, u64::MAX] {
        let read = view.read(clock, None);
        for id in ids(graph) {
            let QueryOutput::Node(expected) = query(
                graph,
                QueryOp::GetNode {
                    node: NodeSelector::Id(id),
                },
                clock,
                None,
            ) else {
                panic!()
            };
            assert_eq!(read.node(id).map(node_view), expected);
            assert_eq!(read.node_expiry(id), graph.node_expiry(id));
            assert_eq!(read.node_expired(id), graph.node_expired(id, clock));
            let key = graph.node(id).map_or("missing", |node| &node.key);
            let QueryOutput::Node(by_key) = query(
                graph,
                QueryOp::GetNode {
                    node: NodeSelector::Key(key.into()),
                },
                clock,
                None,
            ) else {
                panic!()
            };
            assert_eq!(read.node_by_key(key).map(node_view), by_key);
            let attachments: Vec<_> = if expected.is_some() {
                graph.attachments(id).collect()
            } else {
                Vec::new()
            };
            assert_eq!(read.attachments(id).collect::<Vec<_>>(), attachments);
            for projection in [
                PropertyProjection::All,
                PropertyProjection::None,
                PropertyProjection::Only(BTreeSet::from(["number".into(), "missing".into()])),
            ] {
                let expected = expected.as_ref().map(|n| match &projection {
                    PropertyProjection::All => n.properties.clone(),
                    PropertyProjection::None => Properties::new(),
                    PropertyProjection::Only(fields) => fields
                        .iter()
                        .filter_map(|f| n.properties.get(f).map(|v| (f.clone(), v.clone())))
                        .collect(),
                });
                assert_eq!(read.node_properties(id, &projection), expected);
            }
        }
        for key in ["n10", "n20", "n2", "n30", "n3", "missing"] {
            let QueryOutput::Node(expected) = query(
                graph,
                QueryOp::GetNode {
                    node: NodeSelector::Key(key.into()),
                },
                clock,
                None,
            ) else {
                panic!()
            };
            assert_eq!(read.node_by_key(key).map(node_view), expected);
        }
    }
}

fn scans(view: &CompactGraphView, graph: &Graph) {
    for clock in [0, 5, 7, u64::MAX] {
        let read = view.read(clock, None);
        let visible = |id| !graph.node_expired(id, clock);
        for after in [
            None,
            Some(NodeId(0)),
            Some(NodeId(10)),
            Some(NodeId(21)),
            Some(NodeId(30)),
            Some(NodeId(u64::MAX - 1)),
            Some(NodeId(u64::MAX)),
        ] {
            let expected: Vec<_> = graph
                .node_scan(after)
                .filter(|n| visible(n.id))
                .map(|n| n.id)
                .collect();
            assert_eq!(
                read.node_scan(after).map(|n| n.id).collect::<Vec<_>>(),
                expected
            );
            for label in ["Entity", "L0", "L1", "Extra", "missing"] {
                let expected: Vec<_> = graph
                    .label_scan(label, after)
                    .filter(|&id| visible(id))
                    .collect();
                assert_eq!(read.label_scan(label, after).collect::<Vec<_>>(), expected);
                if after.is_none() {
                    assert_eq!(
                        read.nodes_with_label(label).collect::<Vec<_>>(),
                        graph
                            .nodes_with_label(label)
                            .filter(|&id| visible(id))
                            .collect::<Vec<_>>()
                    );
                }
            }
        }
        assert_eq!(
            read.nodes().map(|n| n.id).collect::<Vec<_>>(),
            graph
                .nodes()
                .filter(|n| visible(n.id))
                .map(|n| n.id)
                .collect::<Vec<_>>()
        );
    }
}

fn edges(view: &CompactGraphView, graph: &Graph) {
    for clock in [0, 4, 5, 7, 9, u64::MAX] {
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
            let read = view.read(clock, as_of);
            let mut expected_ids = Vec::new();
            for id in edge_ids(graph) {
                let QueryOutput::Edge(expected) = query(
                    graph,
                    QueryOp::GetEdge {
                        edge: EdgeSelector::Id(id),
                    },
                    clock,
                    as_of,
                ) else {
                    panic!()
                };
                assert_eq!(read.edge(id).map(|e| edge_view(e, graph)), expected);
                if expected.is_some() && !expected_ids.contains(&id) {
                    expected_ids.push(id);
                }
                assert_eq!(read.edge_expiry(id), graph.edge_expiry(id));
                assert_eq!(
                    read.edge_expired(id),
                    graph.edge(id).is_some_and(|e| graph.edge_expired(e, clock))
                );
                let key = graph
                    .edge(id)
                    .and_then(|e| e.key.as_deref())
                    .unwrap_or("missing");
                let QueryOutput::Edge(by_key) = query(
                    graph,
                    QueryOp::GetEdge {
                        edge: EdgeSelector::Key(key.into()),
                    },
                    clock,
                    as_of,
                ) else {
                    panic!()
                };
                assert_eq!(read.edge_by_key(key).map(|e| edge_view(e, graph)), by_key);
                let segments = graph
                    .edge(id)
                    .filter(|e| !graph.edge_expired(e, clock))
                    .and_then(|_| graph.segments(id));
                assert_eq!(read.segments(id), segments);
                let selected = match as_of {
                    Some(time) => segments.and_then(|series| {
                        series.iter().find(|s| {
                            s.valid_from.is_none_or(|from| from <= time)
                                && s.valid_to.is_none_or(|to| time < to)
                        })
                    }),
                    None => None,
                };
                assert_eq!(read.edge(id).and_then(|e| e.segment), selected);
                for projection in [
                    PropertyProjection::All,
                    PropertyProjection::None,
                    PropertyProjection::Only(BTreeSet::from([
                        "phase".into(),
                        "weight".into(),
                        "missing".into(),
                    ])),
                ] {
                    let expected = expected.as_ref().map(|e| match &projection {
                        PropertyProjection::All => e.properties.clone(),
                        PropertyProjection::None => Properties::new(),
                        PropertyProjection::Only(fields) => fields
                            .iter()
                            .filter_map(|f| e.properties.get(f).map(|v| (f.clone(), v.clone())))
                            .collect(),
                    });
                    assert_eq!(read.edge_properties(id, &projection), expected);
                }
            }
            expected_ids.sort();
            assert_eq!(read.edges().map(|e| e.id).collect::<Vec<_>>(), expected_ids);
            for key in [
                "parallel_a",
                "parallel_b",
                "loop",
                "remove_a",
                "deleted_edge",
                "e7",
                "e9",
                "new_loop",
                "temporal",
                "missing",
            ] {
                let QueryOutput::Edge(expected) = query(
                    graph,
                    QueryOp::GetEdge {
                        edge: EdgeSelector::Key(key.into()),
                    },
                    clock,
                    as_of,
                ) else {
                    panic!()
                };
                assert_eq!(
                    read.edge_by_key(key).map(|edge| edge_view(edge, graph)),
                    expected
                );
            }
        }
    }
}

fn ttl(view: &CompactGraphView, graph: &Graph) {
    for clock in [0, 4, 5, 6, 7, 8, 9, u64::MAX - 1, u64::MAX] {
        for as_of in [None, Some(-1)] {
            let read = view.read(clock, as_of);
            assert_eq!(read.live_counts(), graph.live_counts(clock));
            assert_eq!(read.has_due(), graph.has_due(clock));
        }
    }
}

fn adjacency(view: &CompactGraphView, graph: &Graph, derived: bool) {
    let filters = [
        None,
        Some(BTreeSet::from(["R".into()])),
        Some(BTreeSet::from(["A".into(), "T".into()])),
        Some(BTreeSet::from(["missing".into()])),
        Some(BTreeSet::new()),
    ];
    for clock in [0, 5, 7, u64::MAX] {
        for as_of in [None, Some(-1), Some(0), Some(5)] {
            let read = view.read(clock, as_of);
            for node in ids(graph) {
                for direction in [Direction::Out, Direction::In, Direction::Both] {
                    let visible = |id| {
                        let QueryOutput::Edge(edge) = query(
                            graph,
                            QueryOp::GetEdge {
                                edge: EdgeSelector::Id(id),
                            },
                            clock,
                            as_of,
                        ) else {
                            panic!()
                        };
                        edge.is_some()
                    };
                    for types in &filters {
                        for after in [
                            None,
                            Some(("A", EdgeId(40))),
                            Some(("R", EdgeId(0))),
                            Some(("R", EdgeId(7))),
                            Some(("R", EdgeId(21))),
                            Some(("R", EdgeId(u64::MAX - 1))),
                            Some(("R", EdgeId(u64::MAX))),
                            Some(("Z", EdgeId(30))),
                            Some(("zzz", EdgeId(u64::MAX))),
                        ] {
                            let expected: Vec<_> = graph
                                .adjacency(node, direction, types.as_ref(), after)
                                .filter(|&(_, id)| visible(id))
                                .collect();
                            assert_eq!(
                                read.adjacency(node, direction, types.as_ref(), after)
                                    .collect::<Vec<_>>(),
                                expected
                            );
                        }
                    }
                    if derived {
                        assert_eq!(
                            read.edges_of(node, direction),
                            graph
                                .edges_of(node, direction)
                                .into_iter()
                                .filter(|&id| visible(id))
                                .collect::<Vec<_>>()
                        );
                        for other in ids(graph) {
                            for kind in ["R", "A", "T", "missing"] {
                                assert_eq!(
                                    read.edges_between(node, kind, other).collect::<Vec<_>>(),
                                    graph
                                        .edges_between(node, kind, other)
                                        .filter(|&id| visible(id))
                                        .collect::<Vec<_>>()
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

fn all(view: &CompactGraphView, state: &GraphState) {
    let graph = state.graph(G).unwrap();
    metadata(view, graph);
    nodes(view, graph);
    scans(view, graph);
    edges(view, graph);
    ttl(view, graph);
    adjacency(view, graph, true);
}

#[test]
fn metadata_and_physical_counts_match_engine() {
    let state = fixture();
    metadata(&base_view(&state), state.graph(G).unwrap());
}
#[test]
fn seeded_histories_every_split_match_engine() {
    seeded::histories_every_split();
}
#[test]
fn nodes_keys_projection_and_attachments_match_engine() {
    let state = fixture();
    nodes(&base_view(&state), state.graph(G).unwrap());
}
#[test]
fn scans_labels_and_cursors_match_engine() {
    let state = fixture();
    scans(&base_view(&state), state.graph(G).unwrap());
}
#[test]
fn edges_keys_and_temporal_match_engine() {
    let state = fixture();
    edges(&base_view(&state), state.graph(G).unwrap());
}
#[test]
fn ttl_deadlines_and_cascade_match_engine() {
    let state = fixture();
    ttl(&base_view(&state), state.graph(G).unwrap());
}
#[test]
fn adjacency_directions_types_and_cursors_match_engine() {
    let state = fixture();
    adjacency(&base_view(&state), state.graph(G).unwrap(), false);
}
#[test]
fn edges_of_and_between_match_engine() {
    let state = fixture();
    adjacency(&base_view(&state), state.graph(G).unwrap(), true);
}

#[test]
fn generation_pins_base_and_committed_overlay() {
    let mut state = fixture();
    let initial = source(&state);
    let base = Arc::new(decode(&encode(&initial).unwrap()).unwrap());
    let original = CompactGraphView::new(Arc::clone(&base));
    let mut overlay = Overlay::new(&base, 32);
    apply(
        &mut state,
        11,
        vec![
            GraphEntry::NodeExpiry {
                node_id: NodeId(20),
                expires_at_ms: None,
            },
            GraphEntry::EdgeExpiry {
                edge_id: EdgeId(9),
                expires_at_ms: None,
            },
            GraphEntry::PatchNode {
                node_id: NodeId(0),
                add_labels: BTreeSet::from(["Extra".into()]),
                remove_labels: BTreeSet::new(),
                set: Properties::from([("number".into(), PropertyValue::Integer(999))]),
                unset: BTreeSet::new(),
            },
            GraphEntry::Detach {
                node_id: NodeId(0),
                collection: 2,
            },
        ],
    );
    overlay
        .apply(projection(&initial, &source(&state)))
        .unwrap();
    let first = CompactGraphView::with_overlay(overlay.snapshot(Arc::clone(&base)).unwrap());
    let first_state = state.clone();
    let before = source(&state);
    apply(
        &mut state,
        12,
        vec![
            GraphEntry::NodeExpiry {
                node_id: NodeId(20),
                expires_at_ms: Some(5),
            },
            GraphEntry::EdgeExpiry {
                edge_id: EdgeId(9),
                expires_at_ms: Some(0),
            },
            GraphEntry::DeleteEdge { edge_id: EdgeId(7) },
            GraphEntry::Attach {
                node_id: NodeId(0),
                collection: 4,
                record_id: 99,
            },
        ],
    );
    overlay.apply(projection(&before, &source(&state))).unwrap();
    let second = CompactGraphView::with_overlay(overlay.snapshot(Arc::clone(&base)).unwrap());
    let rebuilt = base_view(&state);
    drop(overlay);
    drop(base);
    all(&original, &fixture());
    all(&first, &first_state);
    all(&second, &state);
    all(&rebuilt, &state);
    assert!(Arc::ptr_eq(&original.base, &first.base));
    assert!(!Arc::ptr_eq(&original.base, &rebuilt.base));
}

#[test]
fn framing_and_payload_routes_match_engine() {
    let state = fixture();
    let (count, block) =
        super::super::framing::encode_graph(state.graph(G).unwrap(), None).unwrap();
    let bytes = &block[40..];
    let bodies = super::super::framing::graph_keys(G.0, count, bytes, 3, 0).unwrap();
    assert!(!bodies.is_empty());
    let mut snapshot = SnapshotBuilder::new(3);
    for body in bodies {
        snapshot.push(body).unwrap();
    }
    let restored = snapshot.finish().unwrap();
    assert_eq!(source(&restored), source(&state));
    let framed = CompactGraphView::new(Arc::new(decode(bytes).unwrap()));
    let direct = base_view(&state);
    assert_eq!(*direct.base, *framed.base);
    all(&direct, &state);
    all(&framed, &restored);
}

#[test]
fn empty_graph_matches_engine() {
    let mut state = GraphState::new();
    apply(
        &mut state,
        7,
        vec![GraphEntry::CreateGraph {
            name: "empty".into(),
        }],
    );
    all(&base_view(&state), &state);
}
