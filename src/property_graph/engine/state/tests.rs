use super::super::codec::{decode_body, encode_body};
use super::super::model::PropertyValue;
use super::*;

const G: GraphId = GraphId(0);

fn set(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|v| (*v).to_owned()).collect()
}

fn put_node(id: u64, key: &str, labels: &[&str]) -> GraphEntry {
    GraphEntry::PutNode {
        node_id: NodeId(id),
        key: key.into(),
        labels: set(labels),
        properties: Properties::new(),
    }
}

fn put_edge(
    id: u64,
    source: u64,
    destination: u64,
    relationship: &str,
    key: Option<&str>,
) -> GraphEntry {
    GraphEntry::PutEdge {
        edge_id: EdgeId(id),
        source: NodeId(source),
        destination: NodeId(destination),
        relationship: relationship.into(),
        key: key.map(str::to_owned),
        properties: Properties::new(),
    }
}

fn attach(node: u64, collection: u16, record: u64) -> GraphEntry {
    GraphEntry::Attach {
        node_id: NodeId(node),
        collection,
        record_id: record,
    }
}

fn delete_vector(record: u64) -> GraphEntry {
    delete_vector_in(1, record)
}

fn delete_vector_in(collection: u16, record: u64) -> GraphEntry {
    let mut entry = vec![2, 0, 0, 0];
    entry.extend_from_slice(&record.to_le_bytes());
    entry.extend_from_slice(&0u32.to_le_bytes());
    GraphEntry::VectorMutation { collection, entry }
}

fn body(graph: GraphId, entries: Vec<GraphEntry>) -> GraphBody {
    GraphBody {
        graph_id: graph,
        entries,
    }
}

/// A state plus a sequence counter. Every commit is validated and applied,
/// and successful commits are replayed from their encoded bytes into
/// `replica` to prove INV-009.
struct Harness {
    state: GraphState,
    replica: GraphState,
    sequence: u64,
}

impl Harness {
    fn new() -> Self {
        let mut harness = Self {
            state: GraphState::new(),
            replica: GraphState::new(),
            sequence: 0,
        };
        harness
            .commit(G, vec![GraphEntry::CreateGraph { name: "g".into() }])
            .unwrap();
        harness
    }

    fn commit(&mut self, graph: GraphId, entries: Vec<GraphEntry>) -> Result<(), GraphError> {
        self.sequence += 1;
        let body = body(graph, entries);
        let before = self.state.clone();
        let validated = self.state.validate(self.sequence, &body);
        assert_eq!(self.state, before, "validate must not change state");
        let applied = self.state.apply(self.sequence, &body);
        assert_eq!(validated, applied, "validate and apply must agree");
        match &applied {
            Ok(()) => {
                let bytes = encode_body(&body).unwrap();
                let decoded = decode_body(&bytes, body.entries.len() as u32).unwrap();
                assert_eq!(decoded, body);
                self.replica.apply(self.sequence, &decoded).unwrap();
                assert_eq!(self.state, self.replica, "replay must rebuild equal state");
            }
            Err(_) => assert_eq!(self.state, before, "a failed batch must change nothing"),
        }
        self.state.check_invariants().unwrap();
        applied
    }

    fn ok(&mut self, entries: Vec<GraphEntry>) {
        self.commit(G, entries).unwrap();
    }

    fn err(&mut self, entries: Vec<GraphEntry>) -> GraphError {
        self.commit(G, entries).unwrap_err()
    }

    fn graph(&self) -> &Graph {
        self.state.graph(G).unwrap()
    }
}

#[test]
fn named_graphs_are_created_and_found() {
    let mut h = Harness::new();
    h.commit(
        GraphId(5),
        vec![GraphEntry::CreateGraph {
            name: "other".into(),
        }],
    )
    .unwrap();
    assert_eq!(h.state.free_graph_id(), Some(GraphId(1)));
    assert_eq!(h.state.graph_by_name("other").unwrap().id(), GraphId(5));
    assert_eq!(h.state.graph(G).unwrap().name(), "g");
    let ids: Vec<_> = h.state.graphs().map(Graph::id).collect();
    assert_eq!(ids, [G, GraphId(5)]);
    assert_eq!(
        h.commit(
            GraphId(6),
            vec![GraphEntry::CreateGraph { name: "g".into() }]
        ),
        Err(GraphError::GraphNameExists("g".into()))
    );
    // IDs of live graphs are unavailable; free gaps may be assigned.
    for id in [0, 5] {
        assert_eq!(
            h.commit(
                GraphId(id),
                vec![GraphEntry::CreateGraph { name: "x".into() }]
            ),
            Err(GraphError::GraphIdUnavailable(GraphId(id)))
        );
    }
    h.commit(
        GraphId(3),
        vec![GraphEntry::CreateGraph { name: "x".into() }],
    )
    .unwrap();
    assert_eq!(h.state.free_graph_id(), Some(GraphId(1)));
    assert_eq!(
        h.commit(
            GraphId(64),
            vec![GraphEntry::CreateGraph { name: "x".into() }]
        ),
        Err(GraphError::GraphIdUnavailable(GraphId(64)))
    );
    assert_eq!(
        h.commit(GraphId(9), vec![put_node(0, "a", &[])]),
        Err(GraphError::GraphNotFound(GraphId(9)))
    );
}

#[test]
fn at_most_64_graph_ids_exist() {
    let mut h = Harness::new();
    for id in 1..64u16 {
        h.commit(
            GraphId(id),
            vec![GraphEntry::CreateGraph {
                name: format!("g{id}"),
            }],
        )
        .unwrap();
    }
    assert_eq!(h.state.graphs().count(), 64);
    assert_eq!(h.state.free_graph_id(), None);
    for id in [17, 64] {
        assert_eq!(
            h.commit(
                GraphId(id),
                vec![GraphEntry::CreateGraph {
                    name: "late".into()
                }]
            ),
            Err(GraphError::GraphIdUnavailable(GraphId(id)))
        );
    }
    // Dropping one graph frees exactly its ID; the live limit stays 64.
    h.commit(GraphId(17), vec![GraphEntry::DropGraph]).unwrap();
    assert_eq!(h.state.free_graph_id(), Some(GraphId(17)));
    h.commit(
        GraphId(17),
        vec![GraphEntry::CreateGraph {
            name: "late".into(),
        }],
    )
    .unwrap();
    assert_eq!(h.state.graphs().count(), 64);
    assert_eq!(h.state.free_graph_id(), None);
}

#[test]
fn drop_graph_only_when_empty_and_its_id_is_reusable() {
    let mut h = Harness::new();
    h.ok(vec![put_node(0, "a", &[])]);
    assert_eq!(
        h.err(vec![GraphEntry::DropGraph]),
        GraphError::GraphNotEmpty(G)
    );
    h.ok(vec![
        GraphEntry::DeleteNode { node_id: NodeId(0) },
        GraphEntry::DropGraph,
    ]);
    assert!(h.state.graph(G).is_none());
    assert!(h.state.graph_by_name("g").is_none());
    assert_eq!(
        h.err(vec![GraphEntry::DropGraph]),
        GraphError::GraphNotFound(G)
    );
    // Both the name and the ID are reusable; the recreated graph is new and
    // its node and edge high-water marks start again at 0.
    assert_eq!(h.state.free_graph_id(), Some(G));
    h.commit(G, vec![GraphEntry::CreateGraph { name: "g".into() }])
        .unwrap();
    assert_eq!(h.graph().next_node_id(), 0);
    assert_eq!(h.graph().node_count(), 0);
    h.commit(
        GraphId(1),
        vec![GraphEntry::CreateGraph { name: "h".into() }],
    )
    .unwrap();
    // Drop and recreate in one batch is atomic; a failure restores the old graph.
    h.commit(
        G,
        vec![
            GraphEntry::DropGraph,
            GraphEntry::CreateGraph { name: "g2".into() },
        ],
    )
    .unwrap();
    assert_eq!(h.graph().name(), "g2");
    assert_eq!(
        h.commit(
            G,
            vec![
                GraphEntry::DropGraph,
                GraphEntry::CreateGraph { name: "h".into() },
            ]
        ),
        Err(GraphError::GraphNameExists("h".into()))
    );
    assert_eq!(h.graph().name(), "g2");
    // Entries after a drop in the same batch see the graph as absent.
    assert_eq!(
        h.commit(
            GraphId(1),
            vec![GraphEntry::DropGraph, put_node(0, "a", &[])]
        ),
        Err(GraphError::GraphNotFound(GraphId(1)))
    );
    assert!(h.state.graph(GraphId(1)).is_some());
}

#[test]
fn nodes_have_unique_keys_and_never_reuse_ids() {
    let mut h = Harness::new();
    h.ok(vec![
        put_node(0, "alice", &["Person"]),
        put_node(3, "bob", &[]),
    ]);
    assert_eq!(h.graph().next_node_id(), 4);
    assert_eq!(h.graph().node_by_key("bob").unwrap().id, NodeId(3));
    assert_eq!(
        h.err(vec![put_node(4, "alice", &[])]),
        GraphError::NodeKeyExists("alice".into())
    );
    assert_eq!(
        h.err(vec![put_node(2, "carol", &[])]),
        GraphError::NodeIdUnavailable(NodeId(2))
    );
    assert_eq!(
        h.err(vec![put_node(0, "carol", &[])]),
        GraphError::NodeKeyMismatch(NodeId(0))
    );
    // Two new nodes with one key in one batch.
    assert_eq!(
        h.err(vec![put_node(4, "dup", &[]), put_node(5, "dup", &[])]),
        GraphError::NodeKeyExists("dup".into())
    );
    // After a delete the key is free again; the ID is not.
    h.ok(vec![GraphEntry::DeleteNode { node_id: NodeId(3) }]);
    assert!(h.graph().node_by_key("bob").is_none());
    assert_eq!(
        h.err(vec![put_node(3, "bob", &[])]),
        GraphError::NodeIdUnavailable(NodeId(3))
    );
    h.ok(vec![put_node(4, "bob", &[])]);
    assert_eq!(h.graph().node_by_key("bob").unwrap().id, NodeId(4));
    assert_eq!(
        h.err(vec![put_node(u64::MAX, "max", &[])]),
        GraphError::IdSpaceExhausted
    );
    h.ok(vec![put_node(u64::MAX - 1, "last", &[])]);
    assert_eq!(h.graph().next_node_id(), u64::MAX);
}

#[test]
fn put_node_replaces_labels_and_properties_and_sets_revision() {
    let mut h = Harness::new();
    h.ok(vec![put_node(0, "a", &["A", "B"])]);
    assert_eq!(h.graph().node(NodeId(0)).unwrap().revision, h.sequence);
    h.ok(vec![GraphEntry::PutNode {
        node_id: NodeId(0),
        key: "a".into(),
        labels: set(&["C"]),
        properties: Properties::from([("x".into(), PropertyValue::Integer(1))]),
    }]);
    let node = h.graph().node(NodeId(0)).unwrap();
    assert_eq!(node.labels, set(&["C"]));
    assert_eq!(node.properties["x"], PropertyValue::Integer(1));
    assert_eq!(node.revision, h.sequence);
    assert_eq!(h.graph().nodes_with_label("A").count(), 0);
    assert_eq!(
        h.graph().nodes_with_label("C").collect::<Vec<_>>(),
        [NodeId(0)]
    );
    assert_eq!(h.graph().next_node_id(), 1);
}

#[test]
fn patch_node_merges_labels_and_properties() {
    let mut h = Harness::new();
    h.ok(vec![GraphEntry::PutNode {
        node_id: NodeId(0),
        key: "a".into(),
        labels: set(&["A", "B"]),
        properties: Properties::from([
            ("keep".into(), PropertyValue::Boolean(true)),
            ("drop".into(), PropertyValue::Boolean(true)),
        ]),
    }]);
    let patch = |add: &[&str], remove: &[&str], set_props: Properties, unset: &[&str]| {
        GraphEntry::PatchNode {
            node_id: NodeId(0),
            add_labels: set(add),
            remove_labels: set(remove),
            set: set_props,
            unset: set(unset),
        }
    };
    // Removing an absent label or unsetting an absent field is a no-op.
    h.ok(vec![patch(
        &["C", "A"],
        &["B", "Absent"],
        Properties::from([("new".into(), PropertyValue::Integer(2))]),
        &["drop", "absent"],
    )]);
    let node = h.graph().node(NodeId(0)).unwrap();
    assert_eq!(node.labels, set(&["A", "C"]));
    assert_eq!(
        node.properties,
        Properties::from([
            ("keep".into(), PropertyValue::Boolean(true)),
            ("new".into(), PropertyValue::Integer(2)),
        ])
    );
    assert_eq!(node.revision, h.sequence);
    assert_eq!(h.graph().nodes_with_label("B").count(), 0);
    let six: Vec<String> = (0..6).map(|i| format!("L{i}")).collect();
    let six: Vec<&str> = six.iter().map(String::as_str).collect();
    h.ok(vec![patch(&six, &[], Properties::new(), &[])]);
    assert_eq!(h.graph().node(NodeId(0)).unwrap().labels.len(), 8);
    assert_eq!(
        h.err(vec![patch(&["Ninth"], &[], Properties::new(), &[])]),
        GraphError::TooManyLabels
    );
    // Replacing one label keeps the count at 8.
    h.ok(vec![patch(&["Ninth"], &["L0"], Properties::new(), &[])]);
    // The merged properties must still satisfy the 64-field limit.
    let many: Properties = (0..63)
        .map(|i| (format!("f{i:02}"), PropertyValue::Integer(i)))
        .collect();
    assert_eq!(
        h.err(vec![patch(&[], &[], many, &[])]),
        GraphError::InvalidProperties("properties exceed 64 fields")
    );
    assert_eq!(
        h.err(vec![GraphEntry::PatchNode {
            node_id: NodeId(9),
            add_labels: BTreeSet::new(),
            remove_labels: BTreeSet::new(),
            set: Properties::new(),
            unset: BTreeSet::new(),
        }]),
        GraphError::NodeNotFound(NodeId(9))
    );
}

#[test]
fn edges_validate_endpoints_and_allow_parallel_edges_and_self_loops() {
    let mut h = Harness::new();
    h.ok(vec![put_node(0, "a", &[]), put_node(1, "b", &[])]);
    h.ok(vec![
        put_edge(0, 0, 1, "R", None),
        put_edge(1, 0, 1, "R", None),
        put_edge(2, 0, 0, "SELF", None),
    ]);
    assert_eq!(h.graph().edge_count(), 3);
    assert_eq!(
        h.err(vec![put_edge(3, 0, 7, "R", None)]),
        GraphError::NodeNotFound(NodeId(7))
    );
    assert_eq!(
        h.err(vec![put_edge(3, 7, 0, "R", None)]),
        GraphError::NodeNotFound(NodeId(7))
    );
    // Endpoints created earlier in the same batch are visible; deleted ones are not.
    h.ok(vec![put_node(2, "c", &[]), put_edge(3, 2, 0, "R", None)]);
    assert_eq!(
        h.err(vec![
            put_node(3, "d", &[]),
            GraphEntry::DeleteNode { node_id: NodeId(3) },
            put_edge(4, 3, 0, "R", None),
        ]),
        GraphError::NodeNotFound(NodeId(3))
    );
    // Cross-graph: node IDs of another graph are not endpoints here.
    h.commit(
        GraphId(1),
        vec![
            GraphEntry::CreateGraph { name: "h".into() },
            put_node(9, "far", &[]),
        ],
    )
    .unwrap();
    assert_eq!(
        h.err(vec![put_edge(4, 0, 9, "R", None)]),
        GraphError::NodeNotFound(NodeId(9))
    );
    assert_eq!(h.graph().edge_count(), 4);
    assert_eq!(h.state.graph(GraphId(1)).unwrap().edge_count(), 0);
}

#[test]
fn edge_keys_are_unique_and_edge_ids_are_never_reused() {
    let mut h = Harness::new();
    h.ok(vec![
        put_node(0, "a", &[]),
        put_edge(0, 0, 0, "R", Some("k")),
    ]);
    assert_eq!(h.graph().edge_by_key("k").unwrap().id, EdgeId(0));
    assert_eq!(
        h.err(vec![put_edge(1, 0, 0, "R", Some("k"))]),
        GraphError::EdgeKeyExists("k".into())
    );
    // Node keys and edge keys are separate namespaces.
    h.ok(vec![put_edge(1, 0, 0, "R", Some("a"))]);
    assert_eq!(
        h.err(vec![put_edge(1, 0, 0, "R", None)]),
        GraphError::EdgeIdUnavailable(EdgeId(1))
    );
    h.ok(vec![GraphEntry::DeleteEdge { edge_id: EdgeId(0) }]);
    assert!(h.graph().edge_by_key("k").is_none());
    assert_eq!(
        h.err(vec![put_edge(0, 0, 0, "R", None)]),
        GraphError::EdgeIdUnavailable(EdgeId(0))
    );
    h.ok(vec![put_edge(5, 0, 0, "R", Some("k"))]);
    assert_eq!(h.graph().next_edge_id(), 6);
    assert_eq!(
        h.err(vec![GraphEntry::DeleteEdge { edge_id: EdgeId(0) }]),
        GraphError::EdgeNotFound(EdgeId(0))
    );
    assert_eq!(
        h.err(vec![put_edge(u64::MAX, 0, 0, "R", None)]),
        GraphError::IdSpaceExhausted
    );
}

#[test]
fn patch_edge_updates_properties_and_revision_only() {
    let mut h = Harness::new();
    h.ok(vec![
        put_node(0, "a", &[]),
        GraphEntry::PutEdge {
            edge_id: EdgeId(0),
            source: NodeId(0),
            destination: NodeId(0),
            relationship: "R".into(),
            key: Some("k".into()),
            properties: Properties::from([("a".into(), PropertyValue::Integer(1))]),
        },
    ]);
    h.ok(vec![GraphEntry::PatchEdge {
        edge_id: EdgeId(0),
        set: Properties::from([("b".into(), PropertyValue::Float(0.5))]),
        unset: set(&["a"]),
    }]);
    let edge = h.graph().edge(EdgeId(0)).unwrap();
    assert_eq!(
        edge.properties,
        Properties::from([("b".into(), PropertyValue::Float(0.5))])
    );
    assert_eq!(edge.revision, h.sequence);
    assert_eq!(
        (edge.key.as_deref(), edge.relationship.as_str()),
        (Some("k"), "R")
    );
    assert_eq!(
        h.err(vec![GraphEntry::PatchEdge {
            edge_id: EdgeId(4),
            set: Properties::new(),
            unset: BTreeSet::new()
        }]),
        GraphError::EdgeNotFound(EdgeId(4))
    );
}

#[test]
fn adjacency_is_ordered_by_relationship_type_then_edge_id() {
    let mut h = Harness::new();
    h.ok(vec![put_node(0, "a", &[]), put_node(1, "b", &[])]);
    h.ok(vec![
        put_edge(0, 0, 1, "b", None),
        put_edge(1, 0, 1, "a", None),
        put_edge(2, 1, 0, "a", None),
        put_edge(3, 0, 0, "a", None),
        put_edge(4, 0, 1, "B", None),
        put_edge(5, 0, 1, "a", None),
    ]);
    let g = h.graph();
    let ids = |v: Vec<EdgeId>| v.into_iter().map(|e| e.0).collect::<Vec<_>>();
    // Byte order: "B" < "a" < "b".
    assert_eq!(ids(g.edges_of(NodeId(0), Direction::Out)), [4, 1, 3, 5, 0]);
    assert_eq!(ids(g.edges_of(NodeId(0), Direction::In)), [2, 3]);
    // The self-loop (3) appears once.
    assert_eq!(
        ids(g.edges_of(NodeId(0), Direction::Both)),
        [4, 1, 2, 3, 5, 0]
    );
    assert_eq!(ids(g.edges_of(NodeId(1), Direction::Both)), [4, 1, 2, 5, 0]);
    assert!(g.edges_of(NodeId(9), Direction::Both).is_empty());
}

#[test]
fn compact_adjacency_matches_ordered_sets_for_every_cursor_and_type_filter() {
    let mut h = Harness::new();
    h.ok(vec![
        put_node(0, "a", &[]),
        put_node(1, "b", &[]),
        put_node(2, "isolated", &[]),
        put_edge(0, 0, 1, "z", None),
        put_edge(1, 0, 0, "a", None),
        put_edge(2, 1, 0, "B", None),
        put_edge(3, 0, 1, "a", None),
        put_edge(4, 1, 0, "a", None),
        put_edge(5, 0, 0, "B", None),
        put_edge(6, 0, 1, "_", None),
    ]);
    let filters = [
        None,
        Some(set(&[])),
        Some(set(&["missing"])),
        Some(set(&["a"])),
        Some(set(&["B", "a", "missing"])),
        Some(set(&["B", "_", "a", "z"])),
    ];
    let mut cursors = vec![None];
    for kind in ["0", "B", "C", "_", "a", "b", "z", "zz"] {
        for id in [0, 1, 3, 6, 7, u64::MAX] {
            cursors.push(Some((kind, EdgeId(id))));
        }
    }
    let graph = h.graph();
    for node in [NodeId(0), NodeId(1), NodeId(2), NodeId(99)] {
        for direction in [Direction::Out, Direction::In, Direction::Both] {
            let expected: BTreeSet<_> = graph
                .edges()
                .filter(|edge| {
                    (direction != Direction::In && edge.source == node)
                        || (direction != Direction::Out && edge.destination == node)
                })
                .map(|edge| (edge.relationship.as_str(), edge.id))
                .collect();
            for types in &filters {
                for after in &cursors {
                    let expected: Vec<_> = expected
                        .iter()
                        .copied()
                        .filter(|(kind, _)| {
                            types.as_ref().is_none_or(|types| types.contains(*kind))
                        })
                        .filter(|entry| after.is_none_or(|after| *entry > after))
                        .collect();
                    let actual: Vec<_> = graph
                        .adjacency(node, direction, types.as_ref(), *after)
                        .collect();
                    assert_eq!(
                        actual, expected,
                        "{node:?} {direction:?} {types:?} {after:?}"
                    );
                }
            }
        }
        for relationship in ["B", "_", "a", "missing", "z"] {
            for destination in [NodeId(0), NodeId(1), NodeId(2)] {
                let expected: Vec<_> = graph
                    .edges()
                    .filter(|edge| {
                        edge.source == node
                            && edge.relationship == relationship
                            && edge.destination == destination
                    })
                    .map(|edge| edge.id)
                    .collect();
                assert_eq!(
                    graph
                        .edges_between(node, relationship, destination)
                        .collect::<Vec<_>>(),
                    expected
                );
            }
        }
    }
}

#[test]
fn adjacency_symbols_are_shared_and_reclaimed_across_clones_and_rollback() {
    let mut h = Harness::new();
    h.ok(vec![
        put_node(0, "a", &[]),
        put_node(1, "b", &[]),
        put_edge(0, 0, 1, "R", None),
        put_edge(1, 1, 0, "R", None),
    ]);
    let graph = h.graph();
    assert_eq!(graph.relationships.len(), 1);
    assert_eq!(
        graph
            .relationships
            .get(&std::sync::Arc::new(String::from("R"))),
        Some(&2)
    );
    let out = &graph.out_edges[&NodeId(0)][0].0;
    let inc = &graph.in_edges[&NodeId(1)][0].0;
    let parallel = &graph.out_edges[&NodeId(1)][0].0;
    assert!(Arc::ptr_eq(out, inc));
    assert!(Arc::ptr_eq(out, parallel));
    assert_eq!(
        std::mem::size_of::<AdjacencyEntry>(),
        std::mem::size_of::<usize>() + std::mem::size_of::<EdgeId>()
    );

    let snapshot = h.state.clone();
    h.ok(vec![
        GraphEntry::DeleteEdge { edge_id: EdgeId(0) },
        GraphEntry::DeleteEdge { edge_id: EdgeId(1) },
    ]);
    assert!(h.graph().relationships.is_empty());
    assert!(h.graph().out_edges.is_empty());
    assert!(h.graph().in_edges.is_empty());
    assert_eq!(snapshot.graph(G).unwrap().relationships.len(), 1);
    h.err(vec![
        put_edge(2, 0, 0, "TEMP", None),
        put_edge(3, 0, 99, "FAIL", None),
    ]);
    assert!(h.graph().relationships.is_empty());
    h.ok(vec![put_edge(2, 0, 0, "R", None)]);
    h.err(vec![
        GraphEntry::DeleteEdge { edge_id: EdgeId(2) },
        put_edge(3, 0, 99, "FAIL", None),
    ]);
    assert_eq!(
        h.graph()
            .relationships
            .get(&std::sync::Arc::new(String::from("R"))),
        Some(&1)
    );
}

#[test]
fn compact_adjacency_reclaims_capacity_after_degree_drops() {
    let mut index = AdjacencyIndex::new();
    let relationship = Arc::new("R".to_owned());
    let node = NodeId(0);
    for id in 0..256 {
        insert_adjacency(&mut index, node, (Arc::clone(&relationship), EdgeId(id)));
    }
    for id in 0..255 {
        remove_adjacency(&mut index, node, &(Arc::clone(&relationship), EdgeId(id)));
    }
    assert_eq!(index[&node].len(), 1);
    assert!(index[&node].capacity() <= 4);
    remove_adjacency(&mut index, node, &(relationship, EdgeId(255)));
    assert!(index.is_empty());
}

#[test]
fn indexes_do_not_depend_on_commit_grouping() {
    let mut first = Harness::new();
    let mut second = Harness::new();
    let nodes = vec![put_node(0, "a", &["X", "Y"]), put_node(1, "b", &["Y"])];
    let edges = vec![
        put_edge(0, 0, 1, "z", None),
        put_edge(1, 0, 1, "a", None),
        put_edge(2, 1, 0, "m", None),
    ];
    let mut all = nodes.clone();
    all.extend(edges.clone());
    first.ok(all);
    second.ok(nodes);
    second.ok(vec![edges[0].clone()]);
    second.ok(vec![edges[1].clone(), edges[2].clone()]);
    for direction in [Direction::Out, Direction::In, Direction::Both] {
        for node in [NodeId(0), NodeId(1)] {
            assert_eq!(
                first.graph().edges_of(node, direction),
                second.graph().edges_of(node, direction)
            );
        }
    }
    assert_eq!(
        first.graph().edges_of(NodeId(0), Direction::Out),
        [EdgeId(1), EdgeId(0)]
    );
    assert_eq!(
        first.graph().nodes_with_label("Y").collect::<Vec<_>>(),
        [NodeId(0), NodeId(1)]
    );
    // Only revisions and sequences differ.
    let strip = |g: &Graph| {
        let nodes: Vec<_> = g
            .nodes()
            .map(|n| (n.id, n.key.clone(), n.labels.clone()))
            .collect();
        let edges: Vec<_> = g
            .edges()
            .map(|e| (e.id, e.source, e.destination, e.relationship.clone()))
            .collect();
        (nodes, edges)
    };
    assert_eq!(strip(first.graph()), strip(second.graph()));
}

#[test]
fn nodes_with_incident_edges_cannot_be_deleted() {
    let mut h = Harness::new();
    h.ok(vec![
        put_node(0, "a", &[]),
        put_node(1, "b", &[]),
        put_node(2, "c", &[]),
    ]);
    h.ok(vec![
        put_edge(0, 0, 1, "R", None),
        put_edge(1, 2, 0, "R", None),
        put_edge(2, 0, 0, "R", None),
    ]);
    let delete = |id| GraphEntry::DeleteNode {
        node_id: NodeId(id),
    };
    // Out + in + self-loop counted once.
    assert_eq!(
        h.err(vec![delete(0)]),
        GraphError::NodeHasEdges {
            node: NodeId(0),
            incident: 3
        }
    );
    assert_eq!(
        h.err(vec![delete(1)]),
        GraphError::NodeHasEdges {
            node: NodeId(1),
            incident: 1
        }
    );
    h.ok(vec![
        GraphEntry::DeleteEdge { edge_id: EdgeId(0) },
        GraphEntry::DeleteEdge { edge_id: EdgeId(1) },
    ]);
    // Only the self-loop remains.
    assert_eq!(
        h.err(vec![delete(0)]),
        GraphError::NodeHasEdges {
            node: NodeId(0),
            incident: 1
        }
    );
    h.ok(vec![
        GraphEntry::DeleteEdge { edge_id: EdgeId(2) },
        delete(0),
        delete(1),
    ]);
    assert_eq!(h.graph().node_count(), 1);
    assert_eq!(h.err(vec![delete(0)]), GraphError::NodeNotFound(NodeId(0)));
}

#[test]
fn attachments_follow_uniqueness_rules() {
    let mut h = Harness::new();
    h.ok(vec![put_node(0, "a", &[]), put_node(1, "b", &[])]);
    h.ok(vec![attach(0, 1, 10), attach(0, 2, 10)]);
    assert_eq!(
        h.graph().attachments(NodeId(0)).collect::<Vec<_>>(),
        [(1, 10), (2, 10)]
    );
    assert_eq!(h.state.attachment_owner(1, 10), Some((G, NodeId(0))));
    // One attachment per (node, collection).
    assert_eq!(
        h.err(vec![attach(0, 1, 11)]),
        GraphError::NodeAlreadyAttached {
            node: NodeId(0),
            collection: 1
        }
    );
    // One node per record, within a graph and across graphs.
    let taken = GraphError::RecordAlreadyAttached {
        collection: 1,
        record: 10,
        graph: G,
        node: NodeId(0),
    };
    assert_eq!(h.err(vec![attach(1, 1, 10)]), taken);
    h.commit(
        GraphId(1),
        vec![
            GraphEntry::CreateGraph { name: "h".into() },
            put_node(0, "x", &[]),
        ],
    )
    .unwrap();
    assert_eq!(h.commit(GraphId(1), vec![attach(0, 1, 10)]), Err(taken));
    assert_eq!(
        h.err(vec![attach(7, 1, 12)]),
        GraphError::NodeNotFound(NodeId(7))
    );
    // Detach frees the record for another node, in the same batch.
    h.ok(vec![
        GraphEntry::Detach {
            node_id: NodeId(0),
            collection: 1,
        },
        attach(1, 1, 10),
    ]);
    assert_eq!(h.state.attachment_owner(1, 10), Some((G, NodeId(1))));
    assert_eq!(
        h.err(vec![GraphEntry::Detach {
            node_id: NodeId(0),
            collection: 1
        }]),
        GraphError::AttachmentNotFound {
            node: NodeId(0),
            collection: 1
        }
    );
    assert_eq!(
        h.err(vec![GraphEntry::Detach {
            node_id: NodeId(9),
            collection: 1
        }]),
        GraphError::NodeNotFound(NodeId(9))
    );
    // Replacing a node keeps its attachments.
    h.ok(vec![put_node(0, "a", &["Replaced"])]);
    assert_eq!(
        h.graph().attachments(NodeId(0)).collect::<Vec<_>>(),
        [(2, 10)]
    );
    // Deleting a node removes its attachments (records are the adapter's).
    h.ok(vec![GraphEntry::DeleteNode { node_id: NodeId(0) }]);
    assert_eq!(h.state.attachment_owner(2, 10), None);
    h.ok(vec![attach(1, 2, 10)]);
}

#[test]
fn high_water_marks_never_decrease_and_block_lower_ids() {
    let mut h = Harness::new();
    h.ok(vec![put_node(0, "a", &[]), put_edge(0, 0, 0, "R", None)]);
    let hw = |n, e| GraphEntry::GraphHighWater {
        next_node_id: n,
        next_edge_id: e,
    };
    h.ok(vec![hw(100, 200)]);
    assert_eq!(
        (h.graph().next_node_id(), h.graph().next_edge_id()),
        (100, 200)
    );
    h.ok(vec![hw(100, 200)]);
    assert_eq!(h.err(vec![hw(99, 300)]), GraphError::HighWaterDecrease);
    assert_eq!(h.err(vec![hw(300, 199)]), GraphError::HighWaterDecrease);
    assert_eq!(
        h.err(vec![put_node(50, "b", &[])]),
        GraphError::NodeIdUnavailable(NodeId(50))
    );
    assert_eq!(
        h.err(vec![put_edge(150, 0, 0, "R", None)]),
        GraphError::EdgeIdUnavailable(EdgeId(150))
    );
    h.ok(vec![
        put_node(100, "b", &[]),
        put_edge(200, 0, 100, "R", None),
    ]);
    // A compaction-style rewrite: existing IDs ascending, then the high-water
    // mark, reproduces the allocator state.
    let mut copy = GraphState::new();
    let entries = vec![
        GraphEntry::CreateGraph { name: "g".into() },
        put_node(0, "a", &[]),
        put_node(100, "b", &[]),
        put_edge(0, 0, 0, "R", None),
        put_edge(200, 0, 100, "R", None),
        hw(h.graph().next_node_id(), h.graph().next_edge_id()),
    ];
    copy.apply(1, &body(G, entries)).unwrap();
    copy.check_invariants().unwrap();
    assert_eq!(copy.graph(G).unwrap().next_node_id(), 101);
    assert_eq!(copy.graph(G).unwrap().next_edge_id(), 201);
}

#[test]
fn failed_batches_roll_back_every_entry_kind() {
    let mut h = Harness::new();
    h.ok(vec![
        put_node(0, "a", &["A"]),
        put_node(1, "b", &[]),
        put_node(2, "lonely", &["L"]),
        put_edge(0, 0, 1, "R", Some("e")),
        attach(0, 1, 1),
        attach(2, 1, 2),
    ]);
    let fail = put_edge(99, 0, 42, "R", None);
    // Harness::commit asserts the state is unchanged after the failure.
    let error = h.err(vec![
        GraphEntry::GraphHighWater {
            next_node_id: 10,
            next_edge_id: 10,
        },
        put_node(10, "new", &["N"]),
        GraphEntry::PutNode {
            node_id: NodeId(0),
            key: "a".into(),
            labels: set(&["Z"]),
            properties: Properties::new(),
        },
        GraphEntry::PatchNode {
            node_id: NodeId(1),
            add_labels: set(&["P"]),
            remove_labels: BTreeSet::new(),
            set: Properties::new(),
            unset: BTreeSet::new(),
        },
        GraphEntry::PatchEdge {
            edge_id: EdgeId(0),
            set: Properties::from([("x".into(), PropertyValue::Boolean(true))]),
            unset: BTreeSet::new(),
        },
        GraphEntry::DeleteEdge { edge_id: EdgeId(0) },
        put_edge(10, 10, 0, "Q", Some("e")),
        GraphEntry::Detach {
            node_id: NodeId(0),
            collection: 1,
        },
        attach(1, 1, 1),
        GraphEntry::DeleteNode { node_id: NodeId(2) },
        attach(10, 1, 2),
        delete_vector(1),
        fail.clone(),
    ]);
    assert_eq!(error, GraphError::NodeNotFound(NodeId(42)));
    // Graph creation and drop roll back too.
    let mut state = h.state.clone();
    let before = state.clone();
    let create_then_fail = body(
        GraphId(7),
        vec![
            GraphEntry::CreateGraph { name: "t".into() },
            put_node(0, "x", &[]),
            fail,
        ],
    );
    assert!(state.apply(100, &create_then_fail).is_err());
    assert_eq!(state, before);
    h.commit(
        GraphId(3),
        vec![GraphEntry::CreateGraph {
            name: "empty".into(),
        }],
    )
    .unwrap();
    let mut state = h.state.clone();
    let before = state.clone();
    let drop_then_fail = body(
        GraphId(3),
        vec![
            GraphEntry::DropGraph,
            GraphEntry::CreateGraph {
                name: "empty".into(),
            },
            GraphEntry::DropGraph,
            GraphEntry::DropGraph,
        ],
    );
    assert_eq!(
        state.apply(100, &drop_then_fail),
        Err(GraphError::GraphNotFound(GraphId(3)))
    );
    assert_eq!(state, before);
}

/// A reclaim deletes every node due at its clock with all of the
/// node's edges and attachments, and every edge due on its own. Keys become
/// reusable; IDs do not.
#[test]
fn reclaim_removes_due_nodes_with_their_edges_and_due_edges() {
    let node_expiry = |id, expires_at_ms| GraphEntry::NodeExpiry {
        node_id: NodeId(id),
        expires_at_ms,
    };
    let edge_expiry = |id, expires_at_ms| GraphEntry::EdgeExpiry {
        edge_id: EdgeId(id),
        expires_at_ms,
    };
    let mut h = Harness::new();
    h.ok(vec![
        put_node(0, "a", &[]),
        put_node(1, "b", &[]),
        put_node(2, "c", &[]),
        put_edge(0, 0, 1, "R", Some("ab")),
        put_edge(1, 1, 2, "R", None),
        put_edge(2, 2, 0, "R", None),
        attach(1, 1, 7),
    ]);
    h.ok(vec![
        node_expiry(1, Some(10)),
        edge_expiry(2, Some(20)),
        edge_expiry(0, Some(u64::MAX)),
    ]);
    let graph = h.graph();
    assert_eq!(graph.node(NodeId(1)).unwrap().revision, h.sequence);
    assert_eq!(graph.node_expiry(NodeId(1)), Some(10));
    assert_eq!(graph.edge_expiry(EdgeId(2)), Some(20));
    assert!(!graph.has_due(9) && graph.has_due(10));
    assert_eq!(graph.live_counts(9), (3, 3));
    assert_eq!(graph.live_counts(10), (2, 1));
    assert_eq!(graph.live_counts(20), (2, 0));
    let edge = graph.edge(EdgeId(0)).unwrap();
    assert!(!graph.edge_expired(edge, 9) && graph.edge_expired(edge, 10));

    h.ok(vec![GraphEntry::Reclaim { at_ms: 9 }]);
    assert_eq!((h.graph().node_count(), h.graph().edge_count()), (3, 3));
    h.ok(vec![GraphEntry::Reclaim { at_ms: 10 }]);
    let graph = h.graph();
    assert_eq!((graph.node_count(), graph.edge_count()), (2, 1));
    assert!(graph.node(NodeId(1)).is_none() && graph.edge(EdgeId(2)).is_some());
    assert_eq!(graph.edge_expiry(EdgeId(0)), None);
    assert_eq!(h.state.attachment_owner(1, 7), None);
    h.ok(vec![
        put_node(3, "b", &[]),
        put_edge(3, 0, 3, "R", Some("ab")),
    ]);
    assert_eq!(
        h.err(vec![node_expiry(1, None)]),
        GraphError::NodeNotFound(NodeId(1))
    );
    assert_eq!(
        h.err(vec![edge_expiry(1, None)]),
        GraphError::EdgeNotFound(EdgeId(1))
    );
    // A cleared deadline is never due.
    h.ok(vec![edge_expiry(2, None)]);
    h.ok(vec![GraphEntry::Reclaim { at_ms: u64::MAX }]);
    assert_eq!(h.graph().edge_count(), 2);

    // An edge due on its own and through its node is deleted once, and a
    // failing batch rolls its reclaim back.
    h.ok(vec![node_expiry(3, Some(30)), edge_expiry(3, Some(25))]);
    assert_eq!(
        h.err(vec![
            GraphEntry::Reclaim { at_ms: 30 },
            put_edge(99, 0, 42, "R", None),
        ]),
        GraphError::NodeNotFound(NodeId(42))
    );
    assert_eq!(h.graph().live_counts(30), (2, 1));
    h.ok(vec![GraphEntry::Reclaim { at_ms: 30 }]);
    assert_eq!((h.graph().node_count(), h.graph().edge_count()), (2, 1));
    assert!(!h.graph().has_due(u64::MAX));
}

#[test]
fn later_entries_observe_earlier_ones() {
    let mut h = Harness::new();
    h.ok(vec![
        put_node(0, "a", &[]),
        put_edge(0, 0, 0, "R", Some("e")),
        GraphEntry::DeleteEdge { edge_id: EdgeId(0) },
        put_edge(1, 0, 0, "R", Some("e")),
        attach(0, 1, 1),
        GraphEntry::Detach {
            node_id: NodeId(0),
            collection: 1,
        },
        attach(0, 1, 2),
    ]);
    assert_eq!(h.graph().edge_by_key("e").unwrap().id, EdgeId(1));
    assert_eq!(h.state.attachment_owner(1, 2), Some((G, NodeId(0))));
    assert_eq!(h.state.attachment_owner(1, 1), None);
}

#[test]
fn sequences_increase_and_graph_sequence_tracks_each_graph() {
    let mut h = Harness::new();
    h.commit(
        GraphId(1),
        vec![GraphEntry::CreateGraph { name: "h".into() }],
    )
    .unwrap();
    let h_created = h.sequence;
    h.ok(vec![put_node(0, "a", &[])]);
    assert_eq!(h.graph().graph_sequence(), h.sequence);
    assert_eq!(
        h.state.graph(GraphId(1)).unwrap().graph_sequence(),
        h_created
    );
    assert_eq!(h.state.last_sequence(), h.sequence);
    // A failed batch advances nothing.
    let last = h.state.last_sequence();
    h.err(vec![put_node(0, "b", &[])]);
    assert_eq!(h.state.last_sequence(), last);
    let b = body(G, vec![put_node(1, "b", &[])]);
    let mut state = h.state.clone();
    for sequence in [0, last] {
        let expected = Err(GraphError::SequenceNotIncreasing { last, sequence });
        assert_eq!(state.validate(sequence, &b), expected);
        assert_eq!(state.apply(sequence, &b), expected);
    }
    // validate does not advance the sequence; gaps (vector frames) are fine.
    state.validate(last + 1, &b).unwrap();
    assert_eq!(state.last_sequence(), last);
    state.apply(last + 5, &b).unwrap();
    assert_eq!(state.last_sequence(), last + 5);
    assert_eq!(
        state.graph(G).unwrap().node(NodeId(1)).unwrap().revision,
        last + 5
    );
}

#[test]
fn vector_entries_never_change_graph_state() {
    let mut h = Harness::new();
    h.ok(vec![
        put_node(0, "a", &[]),
        attach(0, 1, 7),
        put_edge(0, 0, 0, "R", None),
    ]);
    let before = h.graph().clone();
    // Deleting or replacing an attached record's vector is checked by the
    // adapter hooks; graph state ignores vector entries entirely.
    h.ok(vec![delete_vector(7), delete_vector(8)]);
    let after = h.graph();
    assert_eq!(
        after.nodes().collect::<Vec<_>>(),
        before.nodes().collect::<Vec<_>>()
    );
    assert_eq!(
        after.edges().collect::<Vec<_>>(),
        before.edges().collect::<Vec<_>>()
    );
    assert_eq!(after.attachments(NodeId(0)).collect::<Vec<_>>(), [(1, 7)]);
    assert_eq!((after.next_node_id(), after.next_edge_id()), (1, 1));
    assert_eq!(after.graph_sequence(), h.sequence);
    assert_eq!(
        h.commit(GraphId(9), vec![delete_vector(1)]),
        Err(GraphError::GraphNotFound(GraphId(9)))
    );
}

#[test]
fn unrelated_vector_mutations_preserve_graph_sequence() {
    let mut h = Harness::new();
    h.ok(vec![put_node(0, "a", &[]), attach(0, 1, 7)]);
    let cursor = h.graph().graph_sequence();
    // Unattached record, and the attached record ID in another collection.
    h.ok(vec![delete_vector(8), delete_vector_in(2, 7)]);
    assert_eq!(h.graph().graph_sequence(), cursor);
    assert_eq!(h.state.last_sequence(), h.sequence);
    // A put of an unattached record is unrelated too.
    h.ok(vec![GraphEntry::VectorMutation {
        collection: 1,
        entry: {
            let mut entry = vec![1, 0, 0, 0];
            entry.extend_from_slice(&9u64.to_le_bytes());
            entry.extend_from_slice(&0u32.to_le_bytes());
            entry.extend_from_slice(&1.0f32.to_le_bytes());
            entry
        },
    }]);
    assert_eq!(h.graph().graph_sequence(), cursor);
    // A failed batch leaves it unchanged as well.
    h.err(vec![delete_vector(7), put_node(0, "b", &[])]);
    assert_eq!(h.graph().graph_sequence(), cursor);
}

#[test]
fn attached_vector_mutations_advance_the_owning_graph_sequence() {
    let mut h = Harness::new();
    h.commit(
        GraphId(1),
        vec![
            GraphEntry::CreateGraph { name: "h".into() },
            put_node(0, "x", &[]),
            attach(0, 1, 9),
        ],
    )
    .unwrap();
    let other = GraphId(1);
    h.ok(vec![put_node(0, "a", &[]), attach(0, 1, 7)]);
    let g_cursor = h.graph().graph_sequence();
    let h_cursor = h.state.graph(other).unwrap().graph_sequence();

    h.ok(vec![delete_vector(7)]);
    assert_eq!(h.graph().graph_sequence(), h.sequence);
    assert!(h.graph().graph_sequence() > g_cursor);
    assert_eq!(h.state.graph(other).unwrap().graph_sequence(), h_cursor);

    // A record attached to another graph advances that graph only.
    let g_cursor = h.graph().graph_sequence();
    h.ok(vec![delete_vector(9)]);
    assert_eq!(h.state.graph(other).unwrap().graph_sequence(), h.sequence);
    assert_eq!(h.graph().graph_sequence(), g_cursor);
}

#[test]
fn invalid_bodies_are_rejected_before_any_entry_applies() {
    let mut h = Harness::new();
    assert_eq!(
        h.err(vec![]),
        GraphError::BatchSize("a graph batch has at least one entry")
    );
    assert_eq!(
        h.err(vec![put_node(0, "", &[])]),
        GraphError::InvalidNodeKey
    );
    assert_eq!(
        h.err(vec![
            put_node(0, "a", &[]),
            put_edge(0, 0, 0, "bad-type", None)
        ]),
        GraphError::InvalidRelationshipType
    );
    let too_many = vec![GraphEntry::DropGraph; super::super::limits::MAX_BATCH_ENTRIES + 1];
    assert_eq!(
        h.err(too_many),
        GraphError::BatchSize("more than 4096 entries")
    );
    assert_eq!(h.graph().node_count(), 0);
}

#[test]
fn check_invariants_detects_inconsistent_state() {
    let mut h = Harness::new();
    h.ok(vec![
        put_node(0, "a", &["A"]),
        put_node(1, "b", &[]),
        put_edge(0, 0, 1, "R", Some("e")),
        attach(1, 1, 1),
    ]);
    let corrupt = |f: &dyn Fn(&mut GraphState)| {
        let mut state = h.state.clone();
        f(&mut state);
        state.check_invariants().unwrap_err()
    };
    let rebuild = GraphError::InvariantViolation("derived graph indexes differ from a rebuild");
    fn g(s: &mut GraphState) -> &mut Graph {
        s.graphs.get_mut(&G).unwrap()
    }
    assert_eq!(
        corrupt(&|s| drop(g(s).nodes.remove(&NodeId(0)))),
        GraphError::InvariantViolation("dangling edge")
    );
    assert_eq!(corrupt(&|s| g(s).edges.clear()), rebuild);
    assert_eq!(corrupt(&|s| g(s).labels.clear()), rebuild);
    assert_eq!(corrupt(&|s| g(s).node_keys.clear()), rebuild);
    assert_eq!(corrupt(&|s| g(s).edge_keys.clear()), rebuild);
    assert_eq!(corrupt(&|s| g(s).in_edges.clear()), rebuild);
    assert_eq!(
        corrupt(&|s| g(s).next_node_id = 1),
        GraphError::InvariantViolation("node id")
    );
    assert_eq!(
        corrupt(&|s| g(s).next_edge_id = 0),
        GraphError::InvariantViolation("edge id")
    );
    assert_eq!(
        corrupt(&|s| {
            let graph = s.graphs.remove(&G).unwrap();
            s.graphs.insert(GraphId(64), graph);
        }),
        GraphError::InvariantViolation("graph id")
    );
    assert_eq!(
        corrupt(&|s| s.attachment_owners.clear()),
        GraphError::InvariantViolation("attachment owner index")
    );
    assert_eq!(
        corrupt(&|s| s.names.clear()),
        GraphError::InvariantViolation("graph name index")
    );
    assert_eq!(
        corrupt(&|s| {
            let graph = g(s);
            graph.remove_edge(EdgeId(0));
            graph.remove_node(NodeId(1));
        }),
        GraphError::InvariantViolation("dangling attachment")
    );
    assert_eq!(
        corrupt(&|s| {
            g(s).node_expiry.set(NodeId(9), Some(1));
        }),
        GraphError::InvariantViolation("dangling expiry")
    );
    assert_eq!(
        corrupt(&|s| {
            g(s).edge_expiry.set(EdgeId(9), Some(1));
        }),
        GraphError::InvariantViolation("dangling expiry")
    );
    assert_eq!(
        corrupt(&|s| {
            g(s).node_expiry.order.insert((1, NodeId(0)));
        }),
        rebuild
    );
}

/// Seeded random batches mixing valid and invalid entries. Every commit is
/// checked by the harness: atomicity, validate/apply agreement, replay from
/// encoded bytes into an equal state, and all invariants.
#[test]
fn seeded_random_batches_keep_invariants_and_replay_equal() {
    for seed in 1..=8u64 {
        let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let mut next = move |bound: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % bound
        };
        let mut h = Harness::new();
        let (mut ok, mut failed) = (0, 0);
        for _ in 0..300 {
            let mut entries = Vec::new();
            if next(4) == 0 {
                entries.push(GraphEntry::Reclaim { at_ms: next(4) });
            }
            for _ in 0..=next(2) {
                let graph = h.graph();
                let node = next(graph.next_node_id() + 2);
                let other = next(graph.next_node_id() + 2);
                let edge = next(graph.next_edge_id() + 2);
                let new_node = graph.next_node_id() + next(2);
                let new_edge = graph.next_edge_id() + next(2);
                let label = ["A", "B", "C"][next(3) as usize];
                let relationship = ["R", "S"][next(2) as usize];
                let edge_key = (next(2) == 0).then(|| format!("e{}", next(8)));
                entries.push(match next(13) {
                    0 | 1 => {
                        let id = if next(3) == 0 { node } else { new_node };
                        put_node(id, &format!("k{}", next(16)), &[label])
                    }
                    2 => GraphEntry::PatchNode {
                        node_id: NodeId(node),
                        add_labels: set(&[label]),
                        remove_labels: BTreeSet::new(),
                        set: Properties::from([(
                            "p".into(),
                            PropertyValue::Integer(next(100) as i64),
                        )]),
                        unset: BTreeSet::new(),
                    },
                    3 => GraphEntry::DeleteNode {
                        node_id: NodeId(node),
                    },
                    4 | 5 => put_edge(new_edge, node, other, relationship, edge_key.as_deref()),
                    6 => GraphEntry::PatchEdge {
                        edge_id: EdgeId(edge),
                        set: Properties::new(),
                        unset: set(&["p"]),
                    },
                    7 => GraphEntry::DeleteEdge {
                        edge_id: EdgeId(edge),
                    },
                    8 => attach(node, next(3) as u16, next(6)),
                    9 => GraphEntry::Detach {
                        node_id: NodeId(node),
                        collection: next(3) as u16,
                    },
                    10 => GraphEntry::NodeExpiry {
                        node_id: NodeId(node),
                        expires_at_ms: (next(2) == 0).then(|| next(4)),
                    },
                    11 => GraphEntry::EdgeExpiry {
                        edge_id: EdgeId(edge),
                        expires_at_ms: (next(2) == 0).then(|| next(4)),
                    },
                    _ => delete_vector(next(6)),
                });
            }
            match h.commit(G, entries) {
                Ok(()) => ok += 1,
                Err(_) => failed += 1,
            }
        }
        assert!(
            ok > 30 && failed > 30,
            "seed {seed}: {ok} ok / {failed} failed"
        );
        assert!(h.graph().node_count() > 0, "seed {seed}");
    }
}

#[test]
fn release_components_reports_every_component_in_order() {
    use MemoryComponent::*;
    let per_graph = [
        GraphTable,
        GraphName,
        NodeRecords,
        EdgeRecords,
        Attachments,
        NodeKeyIndex,
        EdgeKeyIndex,
        OutAdjacency,
        InAdjacency,
        LabelIndex,
    ];
    let released = |state: GraphState| {
        let mut seen = Vec::new();
        state.release_components(|component| seen.push(component));
        seen
    };
    assert_eq!(
        released(GraphState::new()),
        [NameIndex, AttachmentOwners, GraphTable]
    );
    let mut h = Harness::new();
    h.ok(vec![
        put_node(0, "a", &["L"]),
        put_node(1, "b", &[]),
        put_edge(0, 0, 1, "T", Some("e")),
        attach(0, 1, 7),
    ]);
    h.commit(
        GraphId(3),
        vec![GraphEntry::CreateGraph { name: "h".into() }],
    )
    .unwrap();
    let mut expected = vec![NameIndex, AttachmentOwners];
    expected.extend(per_graph);
    expected.extend(per_graph);
    expected.push(GraphTable);
    assert_eq!(released(h.state), expected);
}
