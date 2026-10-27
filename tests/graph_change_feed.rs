//! v0.6.0 graph/vector parity: the change feed covers graph-enabled files.
//!
//! Graph commits (op-12 frames) appear as `GraphCreated`, `GraphDropped` and
//! `GraphBatch` events in log order next to the vector events of the same
//! commit; cursors and batch boundaries work as for vector-only files, in
//! plain, encrypted and commit-timestamp files, on closed files
//! (`read_log_changes`), compacted copies, upgraded copies and replica
//! followers.

mod graph_support;

use graph_support::*;
use tempfile::tempdir;
use vlite::changes::{Change, ChangeKind, FeedCursor, GraphChange, read_log_changes};
use vlite::maintenance;
use vlite::property_graph::{EdgeRef, GraphOp, GraphSchema, LabelDefinition, NodePatch};
use vlite::{Database, Expected, Metadata, MetadataValue, Mutation, ReplicaFollower};

fn all(db: &Database) -> Vec<Change> {
    let batch = db.changes_after(&FeedCursor::beginning(), 10_000).unwrap();
    assert!(!batch.has_more);
    batch.changes
}

fn text(value: &str) -> MetadataValue {
    MetadataValue::String(value.into())
}

fn graph_events(changes: &[Change]) -> Vec<&ChangeKind> {
    changes
        .iter()
        .map(|change| &change.kind)
        .filter(|kind| {
            matches!(
                kind,
                ChangeKind::GraphCreated { .. }
                    | ChangeKind::GraphDropped { .. }
                    | ChangeKind::GraphBatch { .. }
            )
        })
        .collect()
}

#[test]
fn graph_commits_are_reported_with_keys_ids_and_revisions() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("feed.vlite");
    let mut db = Database::create_with_options(&path, 2, Kind::Timestamped.options()).unwrap();
    clock(&mut db, 0);
    db.put(1, &[1.0, 0.0], b"one").unwrap(); // 1
    db.create_graph("kg").unwrap(); // 2
    let mut graph = db.graph_mut("kg").unwrap();
    let ada = graph
        .create_node(
            "ada",
            &["Person"],
            &Metadata::from([("age".into(), MetadataValue::Integer(36))]),
        )
        .unwrap(); // 3
    let bob = graph.create_node("bob", &[], &Metadata::new()).unwrap(); // 4
    let edge = graph
        .create_edge(Some("e1"), "ada", "KNOWS", "bob", &Metadata::new())
        .unwrap(); // 5
    graph
        .patch_node(
            "bob",
            &NodePatch {
                add_labels: vec!["Reviewer".into()],
                set: Metadata::from([("level".into(), MetadataValue::Integer(3))]),
                ..NodePatch::default()
            },
            Expected::Any,
        )
        .unwrap(); // 6
    graph.attach("ada", "default", 1).unwrap(); // 7
    graph.detach("ada", "default").unwrap(); // 8
    graph
        .delete_edge(EdgeRef::Id(edge.id), Expected::Any)
        .unwrap(); // 9
    graph.delete_node("bob", Expected::Any).unwrap(); // 10
    let schema = GraphSchema::default().label("Person", LabelDefinition::new("people"));
    graph.put_schema(&schema).unwrap(); // 11
    db.create_graph("tmp").unwrap(); // 12
    db.drop_graph("tmp").unwrap(); // 13

    let changes = all(&db);
    assert_eq!(changes.len(), 13);
    for (index, change) in changes.iter().enumerate() {
        assert_eq!(change.sequence, index as u64 + 1);
        assert_eq!(change.ordinal, 0);
        assert!(change.committed_at_ms.is_some());
    }
    assert_eq!(changes[0].collection, "default");
    assert_eq!(changes[1].collection, "");
    assert_eq!(
        changes[1].kind,
        ChangeKind::GraphCreated { graph: "kg".into() }
    );
    let batch = |index: usize| match &changes[index].kind {
        ChangeKind::GraphBatch {
            graph,
            revision,
            operations,
        } => {
            assert_eq!(graph, "kg");
            assert_eq!(*revision, changes[index].sequence);
            operations.clone()
        }
        other => panic!("expected a graph batch, got {other:?}"),
    };
    assert_eq!(
        batch(2),
        vec![GraphChange::NodeUpserted {
            id: ada.id,
            key: "ada".into(),
            labels: vec!["Person".into()],
            properties: Metadata::from([("age".into(), MetadataValue::Integer(36))]),
            created: true,
        }]
    );
    assert_eq!(
        batch(3),
        vec![GraphChange::NodeUpserted {
            id: bob.id,
            key: "bob".into(),
            labels: vec![],
            properties: Metadata::new(),
            created: true,
        }]
    );
    assert_eq!(
        batch(4),
        vec![GraphChange::EdgeUpserted {
            id: edge.id,
            key: Some("e1".into()),
            source: ada.id,
            source_key: "ada".into(),
            relationship: "KNOWS".into(),
            destination: bob.id,
            destination_key: "bob".into(),
            properties: Metadata::new(),
            created: true,
        }]
    );
    assert_eq!(
        batch(5),
        vec![GraphChange::NodePatched {
            id: bob.id,
            key: "bob".into(),
            add_labels: vec!["Reviewer".into()],
            remove_labels: vec![],
            set: Metadata::from([("level".into(), MetadataValue::Integer(3))]),
            unset: vec![],
        }]
    );
    assert_eq!(
        batch(6),
        vec![GraphChange::Attached {
            node: ada.id,
            node_key: "ada".into(),
            collection: "default".into(),
            record: 1,
        }]
    );
    assert_eq!(
        batch(7),
        vec![GraphChange::Detached {
            node: ada.id,
            node_key: "ada".into(),
            collection: "default".into(),
            record: 1,
        }]
    );
    assert_eq!(
        batch(8),
        vec![GraphChange::EdgeDeleted {
            id: edge.id,
            key: Some("e1".into()),
        }]
    );
    assert_eq!(
        batch(9),
        vec![GraphChange::NodeDeleted {
            id: bob.id,
            key: "bob".into(),
        }]
    );
    assert_eq!(batch(10), vec![GraphChange::SchemaUpdated { schema }]);
    assert_eq!(
        changes[11].kind,
        ChangeKind::GraphCreated {
            graph: "tmp".into()
        }
    );
    assert_eq!(
        changes[12].kind,
        ChangeKind::GraphDropped {
            graph: "tmp".into()
        }
    );

    // A cursor at the head sees only later commits, graph or vector.
    let cursor = db.feed_cursor().unwrap();
    assert_eq!(cursor.sequence, 13);
    db.graph_mut("kg")
        .unwrap()
        .create_node("carol", &[], &Metadata::new())
        .unwrap();
    let next = db.changes_after(&cursor, 100).unwrap();
    assert_eq!(next.changes.len(), 1);
    assert_eq!(next.changes[0].sequence, 14);
    assert!(matches!(
        &next.changes[0].kind,
        ChangeKind::GraphBatch { operations, .. }
            if matches!(&operations[..], [GraphChange::NodeUpserted { key, created: true, .. }] if key == "carol")
    ));
}

#[test]
fn mixed_commits_keep_log_order_and_are_never_split() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("mixed.vlite");
    let mut db = Database::create_with_options(&path, 2, Kind::Plain.options()).unwrap();
    db.create_graph("kg").unwrap(); // 1
    let mut graph = db.graph_mut("kg").unwrap();
    graph.create_node("ada", &[], &Metadata::new()).unwrap(); // 2
    graph
        .write_batch(&[
            GraphOp::CreateNode {
                key: "carol".into(),
                labels: vec!["Person".into()],
                properties: Metadata::from([("name".into(), text("Carol"))]),
            },
            GraphOp::Vector {
                collection: "default".into(),
                mutation: Mutation::Put {
                    id: 4,
                    vector: vec![4.0, 4.0],
                    payload: b"four".to_vec(),
                },
            },
            GraphOp::Attach {
                node: "carol".into(),
                collection: "default".into(),
                record: 4,
            },
            GraphOp::CreateEdge {
                key: None,
                source: "carol".into(),
                relationship: "KNOWS".into(),
                destination: "ada".into(),
                properties: Metadata::new(),
            },
        ])
        .unwrap(); // 3

    let changes = all(&db);
    let mixed: Vec<&Change> = changes.iter().filter(|c| c.sequence == 3).collect();
    assert_eq!(mixed.len(), 3);
    assert_eq!(
        mixed.iter().map(|c| c.ordinal).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert!(matches!(
        &mixed[0].kind,
        ChangeKind::GraphBatch { operations, .. }
            if matches!(&operations[..], [GraphChange::NodeUpserted { key, .. }] if key == "carol")
    ));
    assert_eq!(mixed[1].collection, "default");
    assert!(matches!(&mixed[1].kind, ChangeKind::Put { id: 4, .. }));
    match &mixed[2].kind {
        ChangeKind::GraphBatch {
            graph,
            revision,
            operations,
        } => {
            assert_eq!((graph.as_str(), *revision), ("kg", 3));
            assert!(matches!(
                &operations[..],
                [
                    GraphChange::Attached { node_key, collection, record: 4, .. },
                    GraphChange::EdgeUpserted { source_key, destination_key, key: None, created: true, .. },
                ] if node_key == "carol" && collection == "default" && source_key == "carol" && destination_key == "ada"
            ));
        }
        other => panic!("expected a graph batch, got {other:?}"),
    }

    // A one-event budget still returns the whole three-event commit.
    let after_two = FeedCursor {
        lineage: db.feed_cursor().unwrap().lineage,
        sequence: 2,
    };
    let batch = db.changes_after(&after_two, 1).unwrap();
    assert_eq!(batch.changes.len(), 3);
    assert_eq!(batch.next.sequence, 3);
    assert!(!batch.has_more);
    let batch = db.changes_after(&FeedCursor::beginning(), 1).unwrap();
    assert_eq!(batch.changes.len(), 1);
    assert!(batch.has_more);
}

/// Pages through the whole feed with a small budget.
fn paged(read: impl Fn(&FeedCursor) -> vlite::changes::ChangeBatch) -> Vec<Change> {
    let mut cursor = FeedCursor::beginning();
    let mut changes = Vec::new();
    loop {
        let batch = read(&cursor);
        changes.extend(batch.changes);
        cursor = batch.next;
        if !batch.has_more {
            return changes;
        }
    }
}

#[test]
fn every_file_kind_reports_the_populate_workload_live_and_closed() {
    let dir = tempdir().unwrap();
    for kind in KINDS {
        let key = kind.key();
        let path = graph_file(dir.path(), "populate", kind);
        let db = open(&path, key.as_ref());
        let live = all(&db);
        assert_eq!(
            paged(|cursor| db.changes_after(cursor, 3).unwrap()),
            live,
            "{kind:?}"
        );
        drop(db);
        let closed = read_log_changes(&path, key.as_ref(), &FeedCursor::beginning(), 10_000)
            .unwrap()
            .changes;
        assert_eq!(closed, live, "{kind:?}");
        assert_eq!(
            paged(|cursor| read_log_changes(&path, key.as_ref(), cursor, 2).unwrap()),
            live
        );
        assert_eq!(
            live.last().unwrap().sequence,
            commits(&path, key.as_ref()),
            "every commit, graph or vector, is in the feed"
        );
        let created: Vec<&str> = live
            .iter()
            .filter_map(|change| match &change.kind {
                ChangeKind::GraphCreated { graph } => Some(graph.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(created, vec!["alpha", "tmp", "beta", "empty"]);
        assert!(live.iter().any(
            |change| matches!(&change.kind, ChangeKind::GraphDropped { graph } if graph == "tmp")
        ));
        assert_eq!(kind == Kind::Timestamped, live[0].committed_at_ms.is_some());
    }
}

#[test]
fn compacted_and_upgraded_graph_files_replay_their_graphs_into_the_feed() {
    let dir = tempdir().unwrap();
    let source = graph_file(dir.path(), "source", Kind::Plain);
    let compacted = dir.path().join("compacted.vlite");
    maintenance::compact_at(&source, &compacted, 5_000).unwrap();
    let upgraded = dir.path().join("upgraded.vlite");
    maintenance::upgrade(&source, &upgraded).unwrap();
    for path in [&compacted, &upgraded] {
        let changes = read_log_changes(path, None, &FeedCursor::beginning(), 10_000)
            .unwrap()
            .changes;
        let graph = graph_events(&changes);
        let created: Vec<&str> = graph
            .iter()
            .filter_map(|kind| match kind {
                ChangeKind::GraphCreated { graph } => Some(graph.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(created, vec!["alpha", "beta", "empty"]);
        assert!(
            !graph
                .iter()
                .any(|kind| matches!(kind, ChangeKind::GraphDropped { .. }))
        );
        let operations: Vec<&GraphChange> = graph
            .iter()
            .filter_map(|kind| match kind {
                ChangeKind::GraphBatch { operations, .. } => Some(operations),
                _ => None,
            })
            .flatten()
            .collect();
        let count =
            |pick: fn(&GraphChange) -> bool| operations.iter().filter(|op| pick(op)).count();
        assert_eq!(
            [
                count(|op| matches!(op, GraphChange::NodeUpserted { created: true, .. })),
                count(|op| matches!(op, GraphChange::EdgeUpserted { created: true, .. })),
                count(|op| matches!(op, GraphChange::Attached { .. })),
            ],
            [POPULATED[1], POPULATED[2], POPULATED[3]]
        );
    }
}

#[test]
fn upgrade_on_a_graph_file_is_a_validated_copy() {
    let dir = tempdir().unwrap();
    let source = graph_file(dir.path(), "source", Kind::Plain);
    let original = std::fs::read(&source).unwrap();
    let output = dir.path().join("upgraded.vlite");
    let report = maintenance::upgrade(&source, &output).unwrap();
    assert_eq!(std::fs::read(&source).unwrap(), original);
    assert_eq!(report.source.format_version, 5);
    assert_eq!(report.output.format_version, 5);
    assert_eq!(header_flags(&output) & FLAG_GRAPH, FLAG_GRAPH);
    assert_eq!(
        snapshot(&output, None).without_revisions(),
        snapshot(&source, None).without_revisions()
    );
    assert_counts(
        &maintenance::verify_graph(&output, None).unwrap().counts,
        POPULATED,
    );
    // High-water marks survive: new IDs are never reused.
    let mut db = open(&output, None);
    let next = db
        .graph_mut("alpha")
        .unwrap()
        .create_node("new", &[], &Metadata::new())
        .unwrap();
    assert_eq!(next.id, ALPHA_HIGH_WATER.node);
    // The upgraded file has a new lineage; its feed starts there.
    let cursor = db.feed_cursor().unwrap();
    assert_ne!(
        cursor.lineage,
        open(&source, None).feed_cursor().unwrap().lineage
    );
    // The destination must not exist (as for vector-only files).
    drop(db);
    assert!(maintenance::upgrade(&source, &output).is_err());
}

#[test]
fn replica_followers_expose_the_graph_feed() {
    let dir = tempdir().unwrap();
    let primary = dir.path().join("primary.vlite");
    let replica = dir.path().join("replica.vlite");
    let mut db = Database::create_with_options(&primary, 2, Kind::Plain.options()).unwrap();
    clock(&mut db, 0);
    db.put(1, &[1.0, 1.0], b"one").unwrap();
    let segment = db.export_log(0, usize::MAX).unwrap();
    let mut follower = ReplicaFollower::create(&replica, &segment, None).unwrap();
    populate(&mut db);
    let segment = db
        .export_log(follower.applied_sequence(), usize::MAX)
        .unwrap();
    follower.apply(&segment).unwrap();
    let primary_feed = all(&db);
    let follower_feed = all(follower.db());
    assert_eq!(follower_feed, primary_feed);
    assert!(!graph_events(&follower_feed).is_empty());
    assert_eq!(
        follower.db().feed_cursor().unwrap(),
        db.feed_cursor().unwrap()
    );
}
