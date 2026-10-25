use super::super::error::GraphError;
use super::super::limits::GraphLimits;
use super::super::meter::ExecutionContext;
use super::super::model::{Direction, EdgeId, NodeId, Precondition, Properties};
use super::super::mutate::{EdgeSelector, GraphMutation};
use super::super::query::{
    GraphQuery, NodeSelector, PropertyProjection, QueryOp, QueryOutput, TruncationReason, execute,
};
use super::super::testkit::{G, TestGraph, VIEW, edge, int, node, props, read, set};
use std::cell::Cell;

fn query(op: QueryOp) -> GraphQuery {
    GraphQuery::new(op)
}

#[test]
fn gets_use_indexes_and_projection_is_owned() {
    let mut db = TestGraph::new();
    db.run(&[
        node("a", &["A"], props(&[("keep", int(1)), ("drop", int(2))])),
        node("b", &["B"], props(&[("keep", int(3))])),
        edge(Some("ab"), "a", "R", "b", props(&[("weight", int(7))])),
    ]);
    let graph = db.graph();
    let mut get_node = query(QueryOp::GetNode {
        node: NodeSelector::Key("a".into()),
    });
    get_node.projection = PropertyProjection::Only(set(&["keep"]));
    let result = read(graph, &get_node, &GraphLimits::default()).unwrap();
    assert!(result.complete);
    assert_eq!(result.view_sequence, VIEW);
    assert_eq!(result.stats.indexes.len(), 1);
    assert!(matches!(
        &result.stats.indexes[0],
        super::super::query::IndexChoice::NodeKeyIndex
    ));
    let QueryOutput::Node(Some(node)) = result.output else {
        panic!("expected node");
    };
    assert_eq!(node.key, "a");
    assert_eq!(node.properties, props(&[("keep", int(1))]));
    let mut by_id_query = query(QueryOp::GetNode {
        node: NodeSelector::Id(db.id("a")),
    });
    by_id_query.projection = get_node.projection.clone();
    let by_id = read(graph, &by_id_query, &GraphLimits::default()).unwrap();
    assert_eq!(by_id.output, QueryOutput::Node(Some(node.clone())));

    let get_edge = query(QueryOp::GetEdge {
        edge: EdgeSelector::Key("ab".into()),
    });
    let result = read(graph, &get_edge, &GraphLimits::default()).unwrap();
    let QueryOutput::Edge(Some(edge)) = result.output else {
        panic!("expected edge");
    };
    assert_eq!(
        (edge.source_key.as_str(), edge.target_key.as_str()),
        ("a", "b")
    );
    assert_eq!(edge.properties, props(&[("weight", int(7))]));

    assert!(matches!(
        read(
            graph,
            &query(QueryOp::GetNode {
                node: NodeSelector::Id(NodeId(u64::MAX)),
            }),
            &GraphLimits::default()
        )
        .unwrap()
        .output,
        QueryOutput::Node(None)
    ));
}

#[test]
fn read_results_are_stable_after_replay_and_every_graph_write_stales_cursors() {
    let mut base = TestGraph::new();
    base.run(&[
        node("a", &["A"], Properties::new()),
        node("b", &["A"], Properties::new()),
        edge(None, "a", "R", "b", Properties::new()),
    ]);
    let read_query = query(QueryOp::Traverse {
        start: vec![NodeSelector::Key("a".into())],
        direction: Direction::Both,
        types: None,
        expand_filter: super::super::filter::NodeFilter::default(),
        return_filter: super::super::filter::NodeFilter::default(),
        edge_filter: vec![],
        include_paths: true,
    });
    let before = read(base.graph(), &read_query, &GraphLimits::default()).unwrap();
    let replayed = base.replayed();
    let after = read(
        replayed.graph(G).unwrap(),
        &read_query,
        &GraphLimits::default(),
    )
    .unwrap();
    assert_eq!(before, after);

    let mutations = [
        GraphMutation::CreateNode {
            key: "c".into(),
            labels: set(&[]),
            properties: Properties::new(),
        },
        GraphMutation::PatchNode {
            key: "a".into(),
            add_labels: set(&[]),
            remove_labels: set(&[]),
            set: props(&[("p", int(1))]),
            unset: set(&[]),
            expected: Precondition::Any,
        },
        GraphMutation::DeleteEdge {
            edge: EdgeSelector::Id(EdgeId(0)),
            expected: Precondition::Any,
        },
        GraphMutation::Attach {
            node: "a".into(),
            collection: 1,
            record: 99,
        },
    ];
    for mutation in mutations {
        let mut db = base.clone();
        let first = read(
            db.graph(),
            &query(QueryOp::FindNodes {
                label: None,
                conditions: vec![],
                limit: Some(1),
                cursor: None,
            }),
            &GraphLimits::default(),
        )
        .unwrap();
        let cursor = first.cursor.expect("a second node remains");
        db.run(&[mutation]);
        let stale = read(
            db.graph(),
            &query(QueryOp::FindNodes {
                label: None,
                conditions: vec![],
                limit: Some(1),
                cursor: Some(cursor),
            }),
            &GraphLimits::default(),
        );
        assert_eq!(stale, Err(GraphError::CursorStale));
    }

    let mut attached = TestGraph::new();
    attached.run(&[
        node("a", &[], Properties::new()),
        node("b", &[], Properties::new()),
        GraphMutation::Attach {
            node: "a".into(),
            collection: 1,
            record: 99,
        },
    ]);
    let first = read(
        attached.graph(),
        &query(QueryOp::FindNodes {
            label: None,
            conditions: vec![],
            limit: Some(1),
            cursor: None,
        }),
        &GraphLimits::default(),
    )
    .unwrap();
    attached.touch(1, 99);
    let stale = read(
        attached.graph(),
        &query(QueryOp::FindNodes {
            label: None,
            conditions: vec![],
            limit: Some(1),
            cursor: first.cursor,
        }),
        &GraphLimits::default(),
    );
    assert_eq!(stale, Err(GraphError::CursorStale));
}

#[test]
fn find_nodes_pages_are_ordered_and_stale_cursors_fail() {
    let mut db = TestGraph::new();
    db.run(&[
        node("a", &["A"], props(&[("n", int(0))])),
        node("b", &["A"], props(&[("n", int(1))])),
        node("c", &["A"], props(&[("n", int(2))])),
        node("d", &["A"], props(&[("n", int(3))])),
        node("z", &["B"], Properties::new()),
    ]);
    let graph = db.graph();
    let op = || QueryOp::FindNodes {
        label: Some("A".into()),
        conditions: vec![],
        limit: Some(2),
        cursor: None,
    };
    let first = read(graph, &query(op()), &GraphLimits::default()).unwrap();
    let QueryOutput::Nodes(first_nodes) = &first.output else {
        panic!("expected node page");
    };
    assert_eq!(
        first_nodes
            .iter()
            .map(|node| node.key.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert!(!first.complete);
    assert_eq!(first.truncation, [TruncationReason::OutputLimit]);
    let cursor = first.cursor.clone().expect("second page cursor");
    let second_query = query(QueryOp::FindNodes {
        label: Some("A".into()),
        conditions: vec![],
        limit: Some(2),
        cursor: Some(cursor.clone()),
    });
    let second = read(graph, &second_query, &GraphLimits::default()).unwrap();
    let QueryOutput::Nodes(second_nodes) = &second.output else {
        panic!("expected node page");
    };
    assert_eq!(
        second_nodes
            .iter()
            .map(|node| node.key.as_str())
            .collect::<Vec<_>>(),
        ["c", "d"]
    );
    assert!(second.complete);
    assert!(second.cursor.is_none());

    db.run(&[node("e", &["A"], Properties::new())]);
    let stale = read(
        db.graph(),
        &query(QueryOp::FindNodes {
            label: Some("A".into()),
            conditions: vec![],
            limit: Some(2),
            cursor: Some(cursor),
        }),
        &GraphLimits::default(),
    );
    assert_eq!(stale, Err(GraphError::CursorStale));
}

#[test]
fn find_nodes_budget_resumes_after_the_last_scanned_candidate() {
    let mut db = TestGraph::new();
    db.run(&[
        node("a", &["A"], Properties::new()),
        node("b", &["A"], Properties::new()),
        node("c", &["A"], Properties::new()),
        node("d", &["A"], Properties::new()),
    ]);
    let first = read(
        db.graph(),
        &query(QueryOp::FindNodes {
            label: None,
            conditions: vec![],
            limit: Some(10),
            cursor: None,
        }),
        &GraphLimits {
            max_edges_examined: 2,
            ..GraphLimits::default()
        },
    )
    .unwrap();
    let QueryOutput::Nodes(nodes) = first.output else {
        panic!("expected node page");
    };
    assert_eq!(
        nodes
            .iter()
            .map(|node| node.key.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert_eq!(first.stats.nodes_scanned, 2);
    assert_eq!(first.truncation, [TruncationReason::EdgeLimit]);
    let next = read(
        db.graph(),
        &query(QueryOp::FindNodes {
            label: None,
            conditions: vec![],
            limit: Some(10),
            cursor: first.cursor,
        }),
        &GraphLimits {
            max_edges_examined: 10,
            ..GraphLimits::default()
        },
    )
    .unwrap();
    let QueryOutput::Nodes(nodes) = next.output else {
        panic!("expected node page");
    };
    assert_eq!(
        nodes
            .iter()
            .map(|node| node.key.as_str())
            .collect::<Vec<_>>(),
        ["c", "d"]
    );
    assert!(next.complete);
}

#[test]
fn neighbors_merge_directions_filter_types_and_page_in_adjacency_order() {
    let mut db = TestGraph::new();
    db.run(&[
        node("a", &[], Properties::new()),
        node("b", &[], Properties::new()),
        node("c", &[], Properties::new()),
        edge(None, "a", "S", "b", Properties::new()),
        edge(None, "c", "R", "a", Properties::new()),
        edge(None, "a", "R", "a", Properties::new()),
        edge(None, "a", "R", "b", Properties::new()),
    ]);
    let graph = db.graph();
    let first = read(
        graph,
        &query(QueryOp::Neighbors {
            node: NodeSelector::Key("a".into()),
            direction: Direction::Both,
            types: Some(set(&["R", "S"])),
            edge_conditions: vec![],
            limit: Some(2),
            cursor: None,
        }),
        &GraphLimits::default(),
    )
    .unwrap();
    let QueryOutput::Neighbors { edges, nodes } = &first.output else {
        panic!("expected neighbor page");
    };
    assert_eq!(
        edges
            .iter()
            .map(|edge| (edge.relationship.as_str(), edge.id.0))
            .collect::<Vec<_>>(),
        [("R", 1), ("R", 2)]
    );
    assert_eq!(
        nodes
            .iter()
            .map(|node| node.key.as_str())
            .collect::<Vec<_>>(),
        ["c", "a"]
    );
    assert!(!first.complete);
    let second = read(
        graph,
        &query(QueryOp::Neighbors {
            node: NodeSelector::Key("a".into()),
            direction: Direction::Both,
            types: Some(set(&["R", "S"])),
            edge_conditions: vec![],
            limit: Some(2),
            cursor: first.cursor,
        }),
        &GraphLimits::default(),
    )
    .unwrap();
    let QueryOutput::Neighbors { edges, .. } = second.output else {
        panic!("expected neighbor page");
    };
    assert_eq!(
        edges
            .iter()
            .map(|edge| (edge.relationship.as_str(), edge.id.0))
            .collect::<Vec<_>>(),
        [("R", 3), ("S", 0)]
    );
    assert!(second.complete);
}

#[test]
fn neighbor_degree_and_edge_budgets_return_resumable_cursors() {
    let db = super::super::testkit::hub_graph(4, 12);
    let graph = db.graph();
    for (limits, expected) in [
        (
            GraphLimits {
                max_edges_per_node: 2,
                ..GraphLimits::default()
            },
            TruncationReason::DegreeLimit {
                node_ids: vec![NodeId(0)],
            },
        ),
        (
            GraphLimits {
                max_edges_examined: 2,
                ..GraphLimits::default()
            },
            TruncationReason::EdgeLimit,
        ),
    ] {
        let result = read(
            graph,
            &query(QueryOp::Neighbors {
                node: NodeSelector::Id(NodeId(0)),
                direction: Direction::Out,
                types: None,
                edge_conditions: vec![],
                limit: Some(10),
                cursor: None,
            }),
            &limits,
        )
        .unwrap();
        assert_eq!(result.truncation, [expected]);
        assert!(result.cursor.is_some());
        assert_eq!(result.stats.edges_examined, 2);
    }
}

#[test]
fn invalid_queries_are_rejected_and_explain_does_no_work() {
    let mut db = TestGraph::new();
    db.run(&[node("a", &[], Properties::new())]);
    let graph = db.graph();
    let empty_types = query(QueryOp::Neighbors {
        node: NodeSelector::Id(NodeId(0)),
        direction: Direction::Out,
        types: Some(set(&[])),
        edge_conditions: vec![],
        limit: Some(1),
        cursor: None,
    });
    assert!(matches!(
        read(graph, &empty_types, &GraphLimits::default()),
        Err(GraphError::InvalidQuery(_))
    ));
    let zero_page = query(QueryOp::FindNodes {
        label: None,
        conditions: vec![],
        limit: Some(0),
        cursor: None,
    });
    assert!(matches!(
        read(graph, &zero_page, &GraphLimits::default()),
        Err(GraphError::InvalidQuery(_))
    ));

    let calls = Cell::new(0);
    let clock = || {
        calls.set(calls.get() + 1);
        panic!("explain must not consult the clock");
    };
    let context = ExecutionContext::new(42, &clock);
    let mut explain = query(QueryOp::FindNodes {
        label: None,
        conditions: vec![],
        limit: None,
        cursor: None,
    });
    explain.explain = true;
    let result = execute(graph, &explain, &GraphLimits::default(), &context).unwrap();
    assert!(result.complete);
    assert_eq!(result.view_sequence, 42);
    assert_eq!(result.stats.edges_examined, 0);
    assert_eq!(calls.get(), 0);
    assert!(matches!(result.output, QueryOutput::Explain(_)));
}

#[test]
fn cursor_query_mismatch_is_invalid_before_staleness() {
    let mut db = TestGraph::new();
    db.run(&[
        node("a", &["A"], Properties::new()),
        node("b", &["A"], Properties::new()),
    ]);
    let first = read(
        db.graph(),
        &query(QueryOp::FindNodes {
            label: Some("A".into()),
            conditions: vec![],
            limit: Some(1),
            cursor: None,
        }),
        &GraphLimits::default(),
    )
    .unwrap();
    let cursor = first.cursor.unwrap();
    let result = read(
        db.graph(),
        &query(QueryOp::FindNodes {
            label: Some("A".into()),
            conditions: vec![],
            limit: Some(2),
            cursor: Some(cursor.clone()),
        }),
        &GraphLimits::default(),
    );
    assert!(matches!(result, Err(GraphError::InvalidCursor(_))));

    let mut projected = query(QueryOp::FindNodes {
        label: Some("A".into()),
        conditions: vec![],
        limit: Some(1),
        cursor: Some(cursor.clone()),
    });
    projected.projection = PropertyProjection::None;
    let result = read(db.graph(), &projected, &GraphLimits::default());
    assert!(matches!(result, Err(GraphError::InvalidCursor(_))));

    let result = read(
        db.graph(),
        &query(QueryOp::FindNodes {
            label: None,
            conditions: vec![],
            limit: Some(1),
            cursor: Some(cursor),
        }),
        &GraphLimits::default(),
    );
    assert!(matches!(result, Err(GraphError::InvalidCursor(_))));
}
