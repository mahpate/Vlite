//! Gap 3: graph node and edge TTL.
//!
//! Nodes and edges carry an optional absolute deadline in Unix milliseconds.
//! At a read clock `t` an entity whose deadline is `<= t` is hidden from every
//! read; an expired node hides all of its edges (cascade) and an expired edge
//! hides only itself. Writes treat expired entities as absent, compaction and
//! directory checkpoints drop them physically, and deadlines persist through
//! reopen, checkpoints, compaction, conversion and backup. Files that never
//! set a deadline keep their exact encoding.
//!
//! Deadlines stay far from the wall clock: `PAST` is already expired and
//! `FAR` is never reached, and explicit read clocks check the boundaries.

use std::fs;
use std::path::Path;

use tempfile::tempdir;
use vlite::maintenance;
use vlite::property_graph::{
    Direction, EdgeRef, EvidenceSpec, Expansion, FindNodes, GraphOp, GraphOpOutcome, GraphRef,
    GraphSchema, LabelDefinition, Multiplicity, Neighbors, NodePatch, NodeSelector, PathOutcome,
    Pivot, PropertyDefinition, PropertyType, ReadOptions, RelationshipDefinition, Retrieve,
    ShortestPath, Traverse, inspect_graph, verify_graph,
};
use vlite::{CreateOptions, Database, Error, Expected, Metadata, MetadataValue, Mutation};

/// Expired at any wall-clock time the tests run.
const PAST: u64 = 1;
/// Never reached by the wall clock.
const FAR: u64 = u64::MAX / 2;

fn memory() -> Database {
    let mut db = Database::create_in_memory(2, CreateOptions::default()).unwrap();
    db.create_graph("g").unwrap();
    db
}

fn props(name: &str, value: i64) -> Metadata {
    Metadata::from([(name.to_owned(), MetadataValue::Integer(value))])
}

/// Creates `N` nodes and `R` edges named after their endpoints: edge "ab"
/// goes from node "a" to node "b".
fn build(db: &mut Database, nodes: &[&str], edges: &[&str]) {
    let mut graph = db.graph_mut("g").unwrap();
    for key in nodes {
        graph.create_node(key, &["N"], &Metadata::new()).unwrap();
    }
    for key in edges {
        let (source, destination) = key.split_at(1);
        graph
            .create_edge(Some(key), source, "R", destination, &Metadata::new())
            .unwrap();
    }
}

fn expire_node(db: &mut Database, key: &str, at: Option<u64>) {
    db.graph_mut("g")
        .unwrap()
        .set_node_expiry(key, at, Expected::Any)
        .unwrap();
}

fn expire_edge(db: &mut Database, key: &str, at: Option<u64>) {
    db.graph_mut("g")
        .unwrap()
        .set_edge_expiry(EdgeRef::Key(key.into()), at, Expected::Any)
        .unwrap();
}

fn expire_node_op(key: &str, at: Option<u64>) -> GraphOp {
    GraphOp::SetNodeExpiry {
        key: key.into(),
        expires_at_ms: at,
        expected: Expected::Any,
    }
}

fn expire_edge_op(key: &str, at: Option<u64>) -> GraphOp {
    GraphOp::SetEdgeExpiry {
        edge: EdgeRef::Key(key.into()),
        expires_at_ms: at,
        expected: Expected::Any,
    }
}

fn edge_op(key: Option<&str>, source: &str, destination: &str) -> GraphOp {
    GraphOp::CreateEdge {
        key: key.map(str::to_owned),
        source: source.into(),
        relationship: "R".into(),
        destination: destination.into(),
        properties: Metadata::new(),
    }
}

fn node_expiry(graph: &GraphRef<'_>, key: &str) -> Option<u64> {
    graph.get_node(key).unwrap().output.unwrap().expires_at_ms
}

fn edge_expiry(graph: &GraphRef<'_>, key: &str) -> Option<u64> {
    graph
        .get_edge_by_key(key)
        .unwrap()
        .output
        .unwrap()
        .expires_at_ms
}

/// Visible node keys in ID order.
fn nodes(graph: &GraphRef<'_>) -> Vec<String> {
    found(graph, None)
}

fn found(graph: &GraphRef<'_>, label: Option<&str>) -> Vec<String> {
    let request = FindNodes {
        label: label.map(str::to_owned),
        ..FindNodes::default()
    };
    graph
        .find_nodes(&request, &ReadOptions::default())
        .unwrap()
        .output
        .into_iter()
        .map(|node| node.key)
        .collect()
}

/// The visible edges among `keys`.
fn edges(graph: &GraphRef<'_>, keys: &[&str]) -> Vec<String> {
    keys.iter()
        .filter(|key| graph.get_edge_by_key(key).unwrap().output.is_some())
        .map(|key| (*key).to_owned())
        .collect()
}

fn none() -> Vec<String> {
    Vec::new()
}

fn counts(graph: &GraphRef<'_>) -> (usize, usize) {
    let info = graph.info();
    (info.node_count, info.edge_count)
}

fn neighbor_edges(graph: &GraphRef<'_>, key: &str, direction: Direction) -> Vec<String> {
    let request = Neighbors {
        direction,
        ..Neighbors::new(NodeSelector::key(key))
    };
    graph
        .neighbors(&request, &ReadOptions::default())
        .unwrap()
        .output
        .edges
        .into_iter()
        .map(|edge| edge.key.unwrap())
        .collect()
}

/// Traversed nodes as "key" followed by depth.
fn reached(graph: &GraphRef<'_>, start: &str) -> Vec<String> {
    let request = Traverse::new(vec![NodeSelector::key(start)]);
    graph
        .traverse(&request, &ReadOptions::default())
        .unwrap()
        .output
        .nodes
        .into_iter()
        .map(|node| format!("{}{}", node.node.key, node.depth))
        .collect()
}

fn path(graph: &GraphRef<'_>, from: &str, to: &str) -> Option<Vec<String>> {
    let request = ShortestPath::new(NodeSelector::key(from), NodeSelector::key(to));
    match graph
        .shortest_path(&request, &ReadOptions::default())
        .unwrap()
        .output
    {
        PathOutcome::Found { nodes, .. } => Some(nodes.into_iter().map(|node| node.key).collect()),
        PathOutcome::NoPathWithinHops => None,
        other => panic!("unexpected path outcome {other:?}"),
    }
}

/// Stored (not visible) graph, node, edge and attachment counts.
fn stored(path: &Path) -> (usize, usize, usize) {
    let counts = inspect_graph(path, None).unwrap().counts;
    (counts.nodes, counts.edges, counts.attachments)
}

#[test]
fn an_expiring_node_hides_itself_and_every_edge_from_its_deadline() {
    let mut db = memory();
    build(&mut db, &["a", "b", "c"], &["ab", "bc", "ca"]);
    expire_node(&mut db, "b", Some(FAR));
    let all = ["ab", "bc", "ca"];

    // The default clock is the wall clock, long before the deadline.
    let graph = db.graph("g").unwrap();
    assert_eq!(nodes(&graph), ["a", "b", "c"]);
    assert_eq!(edges(&graph, &all), all);
    assert_eq!(counts(&graph), (3, 3));
    let b = graph.get_node("b").unwrap().output.unwrap();
    assert_eq!(b.expires_at_ms, Some(FAR));
    let ab = graph.get_edge_by_key("ab").unwrap().output.unwrap();
    assert_eq!(ab.expires_at_ms, None);

    let before = graph.with_clock(FAR - 1);
    assert_eq!(nodes(&before), ["a", "b", "c"]);
    assert_eq!(edges(&before, &all), all);
    assert_eq!(counts(&before), (3, 3));
    for now in [FAR, FAR + 1, u64::MAX] {
        let at = graph.with_clock(now);
        assert_eq!(nodes(&at), ["a", "c"]);
        assert_eq!(edges(&at, &all), ["ca"]);
        assert!(at.get_node("b").unwrap().output.is_none());
        assert!(at.get_node_by_id(b.id).unwrap().output.is_none());
        assert!(at.get_edge(ab.id).unwrap().output.is_none());
        assert_eq!(counts(&at), (2, 1));
    }
}

#[test]
fn an_expiring_edge_hides_only_itself() {
    let mut db = memory();
    build(&mut db, &["a", "b"], &["ab"]);
    db.graph_mut("g")
        .unwrap()
        .create_edge(Some("parallel"), "a", "R", "b", &Metadata::new())
        .unwrap();
    expire_edge(&mut db, "ab", Some(FAR));
    let all = ["ab", "parallel"];

    let graph = db.graph("g").unwrap();
    assert_eq!(edge_expiry(&graph, "ab"), Some(FAR));
    assert_eq!(node_expiry(&graph, "a"), None);
    let before = graph.with_clock(FAR - 1);
    assert_eq!(edges(&before, &all), all);
    assert_eq!(neighbor_edges(&before, "a", Direction::Out), all);

    let after = graph.with_clock(FAR);
    assert_eq!(nodes(&after), ["a", "b"]);
    assert_eq!(edges(&after, &all), ["parallel"]);
    assert_eq!(neighbor_edges(&after, "a", Direction::Out), ["parallel"]);
    assert_eq!(neighbor_edges(&after, "b", Direction::In), ["parallel"]);
    assert_eq!(counts(&after), (2, 1));
}

#[test]
fn an_expired_node_takes_edges_that_would_outlive_it() {
    let mut db = memory();
    build(&mut db, &["a", "b", "c"], &["ab", "bc", "ac"]);
    expire_edge(&mut db, "ab", Some(FAR));
    expire_node(&mut db, "b", Some(FAR - 10));
    let all = ["ab", "bc", "ac"];

    let graph = db.graph("g").unwrap();
    let before = graph.with_clock(FAR - 11);
    assert_eq!(edges(&before, &all), all);
    // Views carry the edge's own deadline.
    assert_eq!(edge_expiry(&before, "ab"), Some(FAR));

    let after = graph.with_clock(FAR - 10);
    assert_eq!(nodes(&after), ["a", "c"]);
    assert_eq!(edges(&after, &all), ["ac"]);
    assert_eq!(neighbor_edges(&after, "a", Direction::Both), ["ac"]);
    assert_eq!(neighbor_edges(&after, "c", Direction::In), ["ac"]);
    assert_eq!(counts(&after), (2, 1));
}

#[test]
fn deadlines_are_set_with_a_create_cleared_by_none_and_kept_by_other_writes() {
    let mut db = memory();
    let result = db
        .graph_mut("g")
        .unwrap()
        .write_batch(&[
            GraphOp::CreateNode {
                key: "a".into(),
                labels: vec!["N".into()],
                properties: Metadata::new(),
            },
            GraphOp::SetNodeExpiry {
                key: "a".into(),
                expires_at_ms: Some(FAR),
                expected: Expected::Any,
            },
            GraphOp::CreateNode {
                key: "b".into(),
                labels: vec!["N".into()],
                properties: Metadata::new(),
            },
            GraphOp::CreateEdge {
                key: Some("ab".into()),
                source: "a".into(),
                relationship: "R".into(),
                destination: "b".into(),
                properties: Metadata::new(),
            },
            GraphOp::SetEdgeExpiry {
                edge: EdgeRef::Key("ab".into()),
                expires_at_ms: Some(FAR),
                expected: Expected::Any,
            },
        ])
        .unwrap();
    let revision = result.revision.unwrap();
    let (
        GraphOpOutcome::Node(created),
        GraphOpOutcome::Node(set),
        GraphOpOutcome::Edge(edge),
        GraphOpOutcome::Edge(edge_set),
    ) = (
        result.outcomes[0],
        result.outcomes[1],
        result.outcomes[3],
        result.outcomes[4],
    )
    else {
        panic!("unexpected outcomes {:?}", result.outcomes);
    };
    assert!(created.created && edge.created);
    assert!(!set.created && !edge_set.created);
    assert_eq!((set.id, set.revision), (created.id, revision));
    assert_eq!((edge_set.id, edge_set.revision), (edge.id, revision));
    {
        let graph = db.graph("g").unwrap();
        assert_eq!(node_expiry(&graph, "a"), Some(FAR));
        assert_eq!(edge_expiry(&graph, "ab"), Some(FAR));
        assert_eq!(node_expiry(&graph, "b"), None);
    }

    // Upserts, patches and an idempotent edge retry keep both deadlines.
    {
        let mut graph = db.graph_mut("g").unwrap();
        graph
            .upsert_node("a", &["M"], &props("v", 1), Expected::Any)
            .unwrap();
        let patch = NodePatch {
            add_labels: vec!["P".into()],
            ..NodePatch::default()
        };
        graph.patch_node("a", &patch, Expected::Any).unwrap();
        graph
            .patch_edge(
                EdgeRef::Key("ab".into()),
                &props("w", 2),
                &[],
                Expected::Any,
            )
            .unwrap();
        let retry = graph
            .create_edge(Some("ab"), "a", "R", "b", &props("w", 2))
            .unwrap();
        assert!(!retry.created);
    }
    let current = {
        let graph = db.graph("g").unwrap();
        assert_eq!(node_expiry(&graph, "a"), Some(FAR));
        assert_eq!(edge_expiry(&graph, "ab"), Some(FAR));
        graph.get_node("a").unwrap().output.unwrap().revision
    };

    // Deadline writes are conditional like any other write.
    let mut graph = db.graph_mut("g").unwrap();
    assert!(matches!(
        graph.set_node_expiry("a", Some(FAR - 1), Expected::Revision(current - 1)),
        Err(Error::RevisionMismatch { current: Some(found) }) if found == current
    ));
    assert!(matches!(
        graph.set_node_expiry("a", None, Expected::Absent),
        Err(Error::RevisionMismatch { current: Some(_) })
    ));
    let write = graph
        .set_node_expiry("a", Some(FAR - 1), Expected::Revision(current))
        .unwrap();
    assert!(write.revision > current);
    assert_eq!(node_expiry(&db.graph("g").unwrap(), "a"), Some(FAR - 1));

    // `None` clears a deadline.
    let mut graph = db.graph_mut("g").unwrap();
    graph
        .set_node_expiry("a", None, Expected::Revision(write.revision))
        .unwrap();
    graph
        .set_edge_expiry(EdgeRef::Key("ab".into()), None, Expected::Any)
        .unwrap();
    {
        let graph = db.graph("g").unwrap();
        assert_eq!(node_expiry(&graph, "a"), None);
        assert_eq!(edge_expiry(&graph, "ab"), None);
        let end = graph.with_clock(u64::MAX);
        assert_eq!(nodes(&end), ["a", "b"]);
        assert_eq!(edges(&end, &["ab"]), ["ab"]);
    }

    // Unknown targets fail like other node and edge writes.
    let mut graph = db.graph_mut("g").unwrap();
    assert!(matches!(
        graph.set_node_expiry("zz", Some(FAR), Expected::Any),
        Err(Error::GraphNodeNotFound)
    ));
    assert!(matches!(
        graph.set_edge_expiry(EdgeRef::Key("zz".into()), Some(FAR), Expected::Any),
        Err(Error::GraphEdgeNotFound)
    ));
    assert!(matches!(
        graph.set_edge_expiry(EdgeRef::Id(999), None, Expected::Any),
        Err(Error::GraphEdgeNotFound)
    ));
}

/// a → b → d and a → c → d, all labelled `N` and b also `B`; record 1 is
/// attached to b and record 2 to a. b expires at `FAR - 10`, edge cd at `FAR`.
fn diamond() -> Database {
    let mut db = memory();
    db.put(1, &[1.0, 0.0], b"one").unwrap();
    db.put(2, &[0.0, 1.0], b"two").unwrap();
    build(&mut db, &["a", "b", "c", "d"], &["ab", "bd", "ac", "cd"]);
    {
        let mut graph = db.graph_mut("g").unwrap();
        let patch = NodePatch {
            add_labels: vec!["B".into()],
            ..NodePatch::default()
        };
        graph.patch_node("b", &patch, Expected::Any).unwrap();
        graph.attach("b", "default", 1).unwrap();
        graph.attach("a", "default", 2).unwrap();
    }
    expire_node(&mut db, "b", Some(FAR - 10));
    expire_edge(&mut db, "cd", Some(FAR));
    db
}

#[test]
fn every_read_honours_the_clock() {
    let db = diamond();
    let graph = db.graph("g").unwrap();

    let before = graph.with_clock(FAR - 11);
    assert_eq!(reached(&before, "a"), ["a0", "b1", "c1", "d2"]);
    assert_eq!(path(&before, "a", "d").unwrap(), ["a", "b", "d"]);
    assert_eq!(neighbor_edges(&before, "a", Direction::Out), ["ab", "ac"]);
    assert_eq!(found(&before, Some("B")), ["b"]);

    let b_gone = graph.with_clock(FAR - 10);
    assert_eq!(reached(&b_gone, "a"), ["a0", "c1", "d2"]);
    assert_eq!(path(&b_gone, "a", "d").unwrap(), ["a", "c", "d"]);
    assert_eq!(neighbor_edges(&b_gone, "a", Direction::Out), ["ac"]);
    assert_eq!(neighbor_edges(&b_gone, "d", Direction::In), ["cd"]);
    assert_eq!(found(&b_gone, Some("B")), none());
    assert_eq!(found(&b_gone, Some("N")), ["a", "c", "d"]);
    assert_eq!(nodes(&b_gone), ["a", "c", "d"]);

    let cd_gone = graph.with_clock(FAR);
    assert_eq!(reached(&cd_gone, "a"), ["a0", "c1"]);
    assert_eq!(path(&cd_gone, "a", "d"), None);
    assert_eq!(neighbor_edges(&cd_gone, "d", Direction::In), none());

    // An expired node is missing, like an unknown key.
    for missing in ["b", "zz"] {
        let options = ReadOptions::default();
        let start = NodeSelector::key(missing);
        assert!(matches!(
            b_gone.neighbors(&Neighbors::new(start.clone()), &options),
            Err(Error::GraphNodeNotFound)
        ));
        assert!(matches!(
            b_gone.traverse(&Traverse::new(vec![start.clone()]), &options),
            Err(Error::GraphNodeNotFound)
        ));
        let to = ShortestPath::new(NodeSelector::key("a"), start);
        assert!(matches!(
            b_gone.shortest_path(&to, &options),
            Err(Error::GraphNodeNotFound)
        ));
    }

    // Observed schema counts only visible entities.
    let tally = |at: &GraphRef<'_>| {
        let observed = at.observed_schema(None).unwrap();
        let labels: Vec<(String, u64)> = observed
            .labels
            .iter()
            .map(|label| (label.name.clone(), label.count))
            .collect();
        let relationships: Vec<(String, u64)> = observed
            .relationships
            .iter()
            .map(|relationship| (relationship.name.clone(), relationship.count))
            .collect();
        (labels, relationships)
    };
    assert_eq!(
        tally(&before),
        (
            vec![("B".to_owned(), 1), ("N".to_owned(), 4)],
            vec![("R".to_owned(), 4)]
        )
    );
    assert_eq!(
        tally(&b_gone),
        (vec![("N".to_owned(), 3)], vec![("R".to_owned(), 2)])
    );

    // Retrieve maps a pivot record only to a visible node.
    let mut pivot = Pivot::new("default", 2);
    pivot.vector = Some(vec![1.0, 0.0]);
    let request = Retrieve {
        pivot,
        expansion: Expansion::default(),
        evidence: EvidenceSpec {
            collections: vec!["default".into()],
            ..EvidenceSpec::default()
        },
    };
    let options = ReadOptions::default();
    let result = before.retrieve(&request, &options).unwrap();
    let pivots: Vec<&str> = result
        .pivots
        .iter()
        .map(|hit| hit.node_key.as_str())
        .collect();
    assert_eq!(pivots, ["b", "a"]);
    assert_eq!(result.unmapped_pivots.count, 0);
    let result = b_gone.retrieve(&request, &options).unwrap();
    let pivots: Vec<&str> = result
        .pivots
        .iter()
        .map(|hit| hit.node_key.as_str())
        .collect();
    assert_eq!(pivots, ["a"]);
    assert_eq!(result.unmapped_pivots.record_ids, [1]);
    let expanded: Vec<&str> = result
        .nodes
        .iter()
        .map(|node| node.node.key.as_str())
        .collect();
    assert_eq!(expanded, ["a", "c", "d"]);
    let evidence: Vec<u64> = result.evidence.iter().map(|item| item.record_id).collect();
    assert_eq!(evidence, [2]);
}

#[test]
fn expired_entities_are_absent_for_writes() {
    let mut db = memory();
    db.put(1, &[1.0, 0.0], b"one").unwrap();
    build(&mut db, &["a", "b", "c", "d"], &["bc", "cd"]);
    let (old_a, old_cd) = {
        let graph = db.graph("g").unwrap();
        (
            graph.get_node("a").unwrap().output.unwrap().id,
            graph.get_edge_by_key("cd").unwrap().output.unwrap().id,
        )
    };
    let result = db
        .graph_mut("g")
        .unwrap()
        .write_batch(&[
            GraphOp::SetNodeExpiry {
                key: "a".into(),
                expires_at_ms: Some(PAST),
                expected: Expected::Any,
            },
            GraphOp::SetNodeExpiry {
                key: "b".into(),
                expires_at_ms: Some(PAST),
                expected: Expected::Any,
            },
            GraphOp::SetEdgeExpiry {
                edge: EdgeRef::Key("cd".into()),
                expires_at_ms: Some(PAST),
                expected: Expected::Any,
            },
        ])
        .unwrap();
    let GraphOpOutcome::Node(a_write) = result.outcomes[0] else {
        panic!("unexpected outcome {:?}", result.outcomes[0]);
    };

    // Hidden at the wall clock at once. The batch removed a and b itself (each
    // before its next operation); cd stays stored until the next write.
    {
        let graph = db.graph("g").unwrap();
        assert_eq!(nodes(&graph), ["c", "d"]);
        assert_eq!(edges(&graph, &["bc", "cd"]), none());
        assert_eq!(counts(&graph), (2, 0));
        let listed = db.list_graphs().unwrap();
        assert_eq!((listed[0].node_count, listed[0].edge_count), (2, 0));
    }

    let mut graph = db.graph_mut("g").unwrap();
    // Like a missing node or edge.
    assert!(matches!(
        graph.patch_node("b", &NodePatch::default(), Expected::Any),
        Err(Error::GraphNodeNotFound)
    ));
    assert!(matches!(
        graph.set_node_expiry("b", None, Expected::Any),
        Err(Error::GraphNodeNotFound)
    ));
    assert!(matches!(
        graph.create_edge(None, "c", "R", "b", &Metadata::new()),
        Err(Error::GraphNodeNotFound)
    ));
    assert!(matches!(
        graph.attach("b", "default", 1),
        Err(Error::GraphNodeNotFound)
    ));
    assert!(matches!(
        graph.patch_edge(
            EdgeRef::Key("cd".into()),
            &props("w", 1),
            &[],
            Expected::Any
        ),
        Err(Error::GraphEdgeNotFound)
    ));
    assert!(matches!(
        graph.delete_edge(EdgeRef::Id(old_cd), Expected::Any),
        Err(Error::GraphEdgeNotFound)
    ));
    // A revision read before the deadline matches nothing now.
    assert!(matches!(
        graph.upsert_node(
            "a",
            &[],
            &Metadata::new(),
            Expected::Revision(a_write.revision)
        ),
        Err(Error::RevisionMismatch { current: None })
    ));
    // `Absent` holds, and the key names a new node.
    let a = graph
        .upsert_node("a", &["N"], &Metadata::new(), Expected::Absent)
        .unwrap();
    assert!(a.created);
    assert_ne!(a.id, old_a);
    assert!(
        graph
            .create_node("b", &["N"], &Metadata::new())
            .unwrap()
            .created
    );
    // d's only edge expired, so d can be deleted, and the key is free again.
    graph.delete_node("d", Expected::Any).unwrap();
    let cd = graph
        .create_edge(Some("cd"), "c", "R", "a", &Metadata::new())
        .unwrap();
    assert!(cd.created);
    assert_ne!(cd.id, old_cd);

    let graph = db.graph("g").unwrap();
    assert_eq!(node_expiry(&graph, "a"), None);
    // The new b has no edges: bc went with the old one.
    assert_eq!(neighbor_edges(&graph, "b", Direction::Both), none());
    assert_eq!(nodes(&graph), ["c", "a", "b"]);
    assert_eq!(counts(&graph), (3, 1));
}

/// A deadline due at the batch's clock takes effect for the rest of the
/// batch: the node goes with every edge and attachment of it, the edge on its
/// own, and later operations may reuse their keys and the record.
#[test]
fn later_operations_of_a_batch_see_its_due_deadlines() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("g.vlite");
    let mut db = Database::create(&path, 2).unwrap();
    db.put(1, &[1.0, 0.0], b"one").unwrap();
    db.create_graph("g").unwrap();
    build(&mut db, &["a", "b", "c"], &["ab", "bc", "ca"]);
    db.graph_mut("g")
        .unwrap()
        .attach("a", "default", 1)
        .unwrap();
    let (old_a, old_ab, old_bc) = {
        let graph = db.graph("g").unwrap();
        let edge = |key| graph.get_edge_by_key(key).unwrap().output.unwrap().id;
        (
            graph.get_node("a").unwrap().output.unwrap().id,
            edge("ab"),
            edge("bc"),
        )
    };
    let result = db
        .graph_mut("g")
        .unwrap()
        .write_batch(&[
            expire_node_op("a", Some(PAST)),
            // The record is free once a is gone.
            GraphOp::Vector {
                collection: "default".into(),
                mutation: Mutation::Delete { id: 1 },
            },
            GraphOp::UpsertNode {
                key: "a".into(),
                labels: vec!["N".into()],
                properties: Metadata::new(),
                expected: Expected::Absent,
            },
            edge_op(Some("ab"), "a", "b"),
            expire_edge_op("bc", Some(PAST)),
            edge_op(Some("bc"), "b", "c"),
        ])
        .unwrap();
    let written: Vec<(u64, bool)> = result
        .outcomes
        .iter()
        .filter_map(|outcome| match outcome {
            GraphOpOutcome::Node(write) | GraphOpOutcome::Edge(write) => {
                Some((write.id, write.created))
            }
            _ => None,
        })
        .collect();
    assert_eq!(written[0], (old_a, false));
    assert_eq!(written[3], (old_bc, false));
    for (index, old) in [(1, old_a), (2, old_ab), (4, old_bc)] {
        assert!(written[index].1 && written[index].0 != old, "{written:?}");
    }

    let check = |db: &Database| {
        assert!(db.get(1).unwrap().is_none());
        let graph = db.graph("g").unwrap();
        assert_eq!(nodes(&graph), ["b", "c", "a"]);
        // ca went with the old a.
        assert_eq!(edges(&graph, &["ab", "bc", "ca"]), ["ab", "bc"]);
        assert_eq!(node_expiry(&graph, "a"), None);
        assert_eq!(edge_expiry(&graph, "bc"), None);
        assert_eq!(counts(&graph), (3, 2));
    };
    check(&db);
    drop(db);
    // The batch itself removed everything it expired, and replays the same.
    assert_eq!(stored(&path), (3, 2, 0));
    verify_graph(&path, None).unwrap();
    check(&Database::open(&path).unwrap());
}

/// What a batch's own deadline expired cannot be revived, patched or linked
/// to by its later operations, and the failed batch writes nothing.
#[test]
fn later_operations_of_a_batch_cannot_revive_what_it_expired() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("g.vlite");
    let mut db = Database::create(&path, 2).unwrap();
    db.put(1, &[1.0, 0.0], b"one").unwrap();
    db.create_graph("g").unwrap();
    build(&mut db, &["a", "b", "c"], &["ab", "bc"]);
    let (b, ab) = {
        let graph = db.graph("g").unwrap();
        (
            graph.get_node("b").unwrap().output.unwrap().revision,
            graph
                .get_edge_by_key("ab")
                .unwrap()
                .output
                .unwrap()
                .revision,
        )
    };
    let length = fs::metadata(&path).unwrap().len();
    let unchanged = |db: &Database| {
        let graph = db.graph("g").unwrap();
        assert_eq!(nodes(&graph), ["a", "b", "c"]);
        assert_eq!(edges(&graph, &["ab", "bc"]), ["ab", "bc"]);
        let node = graph.get_node("b").unwrap().output.unwrap();
        assert_eq!((node.revision, node.expires_at_ms), (b, None));
        let edge = graph.get_edge_by_key("ab").unwrap().output.unwrap();
        assert_eq!((edge.revision, edge.expires_at_ms), (ab, None));
    };
    let node_gone = |error: &Error| matches!(error, Error::GraphNodeNotFound);
    let edge_gone = |error: &Error| matches!(error, Error::GraphEdgeNotFound);
    let stale = |error: &Error| matches!(error, Error::RevisionMismatch { current: None });
    let expire_b = || expire_node_op("b", Some(PAST));
    let expire_ab = || expire_edge_op("ab", Some(PAST));
    type Check<'a> = &'a dyn Fn(&Error) -> bool;
    let cases: Vec<(Vec<GraphOp>, Check<'_>)> = vec![
        (vec![expire_b(), expire_node_op("b", None)], &node_gone),
        (
            vec![
                expire_b(),
                GraphOp::PatchNode {
                    key: "b".into(),
                    patch: NodePatch::default(),
                    expected: Expected::Any,
                },
            ],
            &node_gone,
        ),
        (
            vec![
                expire_b(),
                GraphOp::UpsertNode {
                    key: "b".into(),
                    labels: Vec::new(),
                    properties: Metadata::new(),
                    expected: Expected::Revision(b),
                },
            ],
            &stale,
        ),
        (vec![expire_b(), edge_op(None, "a", "b")], &node_gone),
        (
            vec![
                expire_b(),
                GraphOp::Attach {
                    node: "b".into(),
                    collection: "default".into(),
                    record: 1,
                },
            ],
            &node_gone,
        ),
        // Its edges went with it.
        (vec![expire_b(), expire_edge_op("ab", None)], &edge_gone),
        (vec![expire_ab(), expire_edge_op("ab", None)], &edge_gone),
        (
            vec![
                expire_ab(),
                GraphOp::PatchEdge {
                    edge: EdgeRef::Key("ab".into()),
                    set: props("w", 1),
                    unset: Vec::new(),
                    expected: Expected::Any,
                },
            ],
            &edge_gone,
        ),
        (
            vec![
                expire_ab(),
                GraphOp::DeleteEdge {
                    edge: EdgeRef::Key("ab".into()),
                    expected: Expected::Revision(ab),
                },
            ],
            &stale,
        ),
        // A replacement planned earlier in a failing batch is rolled back.
        (
            vec![
                expire_b(),
                GraphOp::CreateNode {
                    key: "b".into(),
                    labels: Vec::new(),
                    properties: Metadata::new(),
                },
                edge_op(None, "b", "z"),
            ],
            &node_gone,
        ),
    ];
    for (ops, expected) in cases {
        let error = db.graph_mut("g").unwrap().write_batch(&ops).unwrap_err();
        assert!(expected(&error), "{ops:?}: {error:?}");
        assert_eq!(fs::metadata(&path).unwrap().len(), length, "{ops:?}");
        unchanged(&db);
    }
    drop(db);
    verify_graph(&path, None).unwrap();
    unchanged(&Database::open(&path).unwrap());
}

/// A full batch whose every other operation expires a node commits with a
/// removal after each of them, beyond 4,097 frame entries, and replays.
#[test]
fn a_full_batch_that_expires_as_it_goes_commits_and_replays() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("g.vlite");
    let mut db = Database::create(&path, 2).unwrap();
    db.create_graph("g").unwrap();
    let half = 2048;
    let creates = |prefix: &str| -> Vec<GraphOp> {
        (0..half)
            .map(|index| GraphOp::CreateNode {
                key: format!("{prefix}{index}"),
                labels: Vec::new(),
                properties: Metadata::new(),
            })
            .collect()
    };
    let mut graph = db.graph_mut("g").unwrap();
    graph.write_batch(&creates("n")).unwrap();
    let ops: Vec<GraphOp> = creates("m")
        .into_iter()
        .enumerate()
        .flat_map(|(index, create)| [expire_node_op(&format!("n{index}"), Some(PAST)), create])
        .collect();
    assert_eq!(ops.len(), 4096);
    assert_eq!(graph.write_batch(&ops).unwrap().outcomes.len(), 4096);
    drop(db);

    assert_eq!(stored(&path), (half, 0, 0));
    verify_graph(&path, None).unwrap();
    let db = Database::open(&path).unwrap();
    let graph = db.graph("g").unwrap();
    assert_eq!(counts(&graph), (half, 0));
    assert!(graph.get_node("n0").unwrap().output.is_none());
    assert!(graph.get_node("m2047").unwrap().output.is_some());
}

#[test]
fn expired_entities_free_their_unique_values() {
    let mut db = memory();
    let schema = GraphSchema::default()
        .label(
            "U",
            LabelDefinition::new("").property(
                "email",
                PropertyDefinition::new(PropertyType::String).unique(),
            ),
        )
        .label("V", LabelDefinition::new(""))
        .relationship(
            "ONE",
            RelationshipDefinition::new("")
                .endpoint("V", "V")
                .multiplicity(Multiplicity::UniqueTriple),
        );
    let email = Metadata::from([("email".to_owned(), MetadataValue::String("x@y".into()))]);
    let mut graph = db.graph_mut("g").unwrap();
    graph.put_schema(&schema).unwrap();
    graph.create_node("u1", &["U"], &email).unwrap();
    graph.create_node("v", &["V"], &Metadata::new()).unwrap();
    graph.create_node("w", &["V"], &Metadata::new()).unwrap();
    assert!(matches!(
        graph.create_node("u2", &["U"], &email),
        Err(Error::GraphSchemaViolation { rule: "unique", .. })
    ));
    let first = graph
        .ensure_edge("v", "ONE", "w", &Metadata::new())
        .unwrap();
    assert!(first.created);
    assert!(
        !graph
            .ensure_edge("v", "ONE", "w", &Metadata::new())
            .unwrap()
            .created
    );

    graph
        .set_node_expiry("u1", Some(PAST), Expected::Any)
        .unwrap();
    graph
        .set_edge_expiry(EdgeRef::Id(first.id), Some(PAST), Expected::Any)
        .unwrap();
    assert!(graph.create_node("u2", &["U"], &email).unwrap().created);
    let second = graph
        .ensure_edge("v", "ONE", "w", &Metadata::new())
        .unwrap();
    assert!(second.created);
    assert_ne!(second.id, first.id);

    // The operations after a deadline due in the same batch see the value
    // and the edge identity free as well.
    let result = graph
        .write_batch(&[
            expire_node_op("u2", Some(PAST)),
            GraphOp::CreateNode {
                key: "u3".into(),
                labels: vec!["U".into()],
                properties: email.clone(),
            },
            GraphOp::SetEdgeExpiry {
                edge: EdgeRef::Id(second.id),
                expires_at_ms: Some(PAST),
                expected: Expected::Any,
            },
            GraphOp::EnsureEdge {
                source: "v".into(),
                relationship: "ONE".into(),
                destination: "w".into(),
                properties: Metadata::new(),
            },
        ])
        .unwrap();
    let GraphOpOutcome::Edge(third) = result.outcomes[3] else {
        panic!("unexpected outcome {:?}", result.outcomes[3]);
    };
    assert!(third.created);
    assert_ne!(third.id, second.id);
    let graph = db.graph("g").unwrap();
    assert_eq!(nodes(&graph), ["v", "w", "u3"]);
    assert_eq!(counts(&graph), (3, 1));
}

#[test]
fn a_graph_whose_entities_all_expired_can_be_dropped() {
    let mut db = memory();
    build(&mut db, &["a", "b"], &["ab"]);
    assert!(matches!(db.drop_graph("g"), Err(Error::GraphNotEmpty)));
    expire_node(&mut db, "a", Some(PAST));
    assert!(matches!(db.drop_graph("g"), Err(Error::GraphNotEmpty)));
    expire_node(&mut db, "b", Some(PAST));
    db.drop_graph("g").unwrap();
    assert!(db.list_graphs().unwrap().is_empty());
}

#[test]
fn reclamation_leaves_no_dangling_edges_and_fits_a_full_batch() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("g.vlite");
    let mut db = Database::create(&path, 2).unwrap();
    db.create_graph("g").unwrap();
    build(&mut db, &["h", "s"], &["sh", "hs"]);
    {
        let spokes: Vec<GraphOp> = (0..10)
            .map(|_| GraphOp::CreateEdge {
                key: None,
                source: "s".into(),
                relationship: "R".into(),
                destination: "h".into(),
                properties: Metadata::new(),
            })
            .collect();
        let mut graph = db.graph_mut("g").unwrap();
        graph.write_batch(&spokes).unwrap();
        graph
            .set_node_expiry("h", Some(PAST), Expected::Any)
            .unwrap();
    }
    assert_eq!(counts(&db.graph("g").unwrap()), (1, 0));

    // The 4,096-operation limit still holds when entities are due: they are
    // reclaimed by the same frame without counting as operations.
    let ops: Vec<GraphOp> = (0..4096)
        .map(|index| GraphOp::CreateNode {
            key: format!("n{index}"),
            labels: Vec::new(),
            properties: Metadata::new(),
        })
        .collect();
    let result = db.graph_mut("g").unwrap().write_batch(&ops).unwrap();
    assert_eq!(result.outcomes.len(), 4096);
    drop(db);

    assert_eq!(stored(&path), (4097, 0, 0));
    verify_graph(&path, None).unwrap();
    let db = Database::open(&path).unwrap();
    let graph = db.graph("g").unwrap();
    assert_eq!(counts(&graph), (4097, 0));
    assert!(graph.get_node("h").unwrap().output.is_none());
}

#[test]
fn deadlines_survive_reopen_and_unreclaimed_entities_stay_hidden() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("g.vlite");
    let mut db = Database::create(&path, 2).unwrap();
    db.create_graph("g").unwrap();
    build(&mut db, &["a", "b", "c", "x"], &["ab", "bc"]);
    expire_node(&mut db, "a", Some(FAR));
    expire_edge(&mut db, "bc", Some(FAR - 1));
    expire_node(&mut db, "x", Some(PAST));
    drop(db);
    // x is still stored: nothing has been written since its deadline.
    assert_eq!(stored(&path), (4, 2, 0));

    let mut db = Database::open(&path).unwrap();
    {
        let graph = db.graph("g").unwrap();
        assert_eq!(node_expiry(&graph, "a"), Some(FAR));
        assert_eq!(edge_expiry(&graph, "bc"), Some(FAR - 1));
        assert_eq!(nodes(&graph), ["a", "b", "c"]);
        let at = graph.with_clock(FAR);
        assert_eq!(nodes(&at), ["b", "c"]);
        assert_eq!(edges(&at, &["ab", "bc"]), none());
    }
    // The next graph write reclaims x for good.
    db.graph_mut("g")
        .unwrap()
        .create_node("y", &[], &Metadata::new())
        .unwrap();
    drop(db);
    assert_eq!(stored(&path), (4, 2, 0));
    verify_graph(&path, None).unwrap();
    let db = Database::open(&path).unwrap();
    let graph = db.graph("g").unwrap();
    assert_eq!(nodes(&graph), ["a", "b", "c", "y"]);
    assert_eq!(node_expiry(&graph, "a"), Some(FAR));
}

/// Nodes a..d with edges ab, bc, cd, da and ca and record 1 attached to b.
/// d expires at `FAR` and edge ca at `FAR - 5`. The last write expires edge
/// cd, which its next operation removes, and then b (taking ab and bc),
/// which the source still stores.
fn expiring_source(db: &mut Database) {
    db.put(1, &[1.0, 0.0], b"one").unwrap();
    db.create_graph("g").unwrap();
    build(db, &["a", "b", "c", "d"], &["ab", "bc", "cd", "da", "ca"]);
    let mut graph = db.graph_mut("g").unwrap();
    graph.attach("b", "default", 1).unwrap();
    graph
        .set_node_expiry("d", Some(FAR), Expected::Any)
        .unwrap();
    graph
        .set_edge_expiry(EdgeRef::Key("ca".into()), Some(FAR - 5), Expected::Any)
        .unwrap();
    graph
        .write_batch(&[
            expire_edge_op("cd", Some(PAST)),
            expire_node_op("b", Some(PAST)),
        ])
        .unwrap();
}

/// The state `expiring_source` leaves once b, ab, bc and cd are gone.
fn assert_reclaimed(path: &Path) {
    assert_eq!(stored(path), (3, 2, 0));
    verify_graph(path, None).unwrap();
    let db = Database::open(path).unwrap();
    assert!(db.get(1).unwrap().is_some());
    let graph = db.graph("g").unwrap();
    assert_eq!(nodes(&graph), ["a", "c", "d"]);
    assert_eq!(edges(&graph, &["ab", "bc", "cd", "da", "ca"]), ["da", "ca"]);
    assert_eq!(node_expiry(&graph, "d"), Some(FAR));
    assert_eq!(edge_expiry(&graph, "ca"), Some(FAR - 5));
    assert_eq!(counts(&graph.with_clock(FAR)), (2, 0));
}

#[test]
fn compaction_drops_expired_nodes_with_their_edges() {
    let dir = tempdir().unwrap();
    let source = dir.path().join("s.vlite");
    let mut db = Database::create(&source, 2).unwrap();
    expiring_source(&mut db);
    drop(db);
    assert_eq!(stored(&source), (4, 4, 1));

    let compacted = dir.path().join("c.vlite");
    maintenance::compact(&source, &compacted).unwrap();
    assert_reclaimed(&compacted);

    for (now, expected) in [(FAR - 6, (3, 2, 0)), (FAR - 5, (3, 1, 0)), (FAR, (2, 0, 0))] {
        let target = dir.path().join(format!("at-{now}.vlite"));
        maintenance::compact_at(&source, &target, now).unwrap();
        assert_eq!(stored(&target), expected);
        verify_graph(&target, None).unwrap();
    }
}

#[test]
fn directory_checkpoints_drop_expired_nodes_with_their_edges() {
    let dir = tempdir().unwrap();
    let store = dir.path().join("store");
    let mut db = Database::create_directory(&store, 2, CreateOptions::default()).unwrap();
    db.set_checkpoint_threshold(None);
    expiring_source(&mut db);
    assert!(db.checkpoint().unwrap().is_some());
    // The open handle still hides them; the image no longer stores them.
    assert_eq!(nodes(&db.graph("g").unwrap()), ["a", "c", "d"]);
    // A later write reclaims them in memory too, and replays cleanly.
    db.graph_mut("g")
        .unwrap()
        .create_node("e", &[], &Metadata::new())
        .unwrap();
    db.graph_mut("g")
        .unwrap()
        .delete_node("e", Expected::Any)
        .unwrap();
    drop(db);
    assert_reclaimed(&store);
}

#[test]
fn deadlines_survive_conversion_and_backup() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("f.vlite");
    let mut db = Database::create(&file, 2).unwrap();
    expiring_source(&mut db);
    drop(db);

    // A file backup is a byte copy: the expired entities are still stored.
    let copy = dir.path().join("copy.vlite");
    maintenance::backup(&file, &copy).unwrap();
    assert_eq!(stored(&copy), (4, 4, 1));
    verify_graph(&copy, None).unwrap();
    {
        let db = Database::open(&copy).unwrap();
        let graph = db.graph("g").unwrap();
        assert_eq!(nodes(&graph), ["a", "c", "d"]);
        assert_eq!(node_expiry(&graph, "d"), Some(FAR));
    }

    // Conversion writes a checkpoint image, which drops them.
    let store = dir.path().join("store");
    maintenance::convert_to_directory(&file, &store).unwrap();
    assert_reclaimed(&store);

    // And back to one file.
    let packed = dir.path().join("packed.vlite");
    maintenance::backup(&store, &packed).unwrap();
    assert_reclaimed(&packed);
}

/// Pre-TTL encodings of `workload` (captured at d530b4c, after the 32-byte
/// header): files that never set a deadline are byte-identical in the log and
/// after compaction.
const LOG_HEX: &str = "5652454301000000010000000000000001000000000000000200000001000000c6562a16000000000000803f7856434d54308e41e80100000000000000565245430c00000002000000000000000000000000000000010000000e0000004b987d3c010000000100000002000000016756434d54b421d3920200000000000000565245430c00000003000000000000000000000000000000010000002e00000052526a1d0100000004000000220000000000000000000000010061010141100000000100000004006e616d6501010000007856434d54301101530300000000000000565245430c0000000400000000000000000000000000000001000000200000001612adcc010000000400000014000000010000000000000001006200040000000000000056434d54f70f4c240400000000000000565245430c00000005000000000000000000000000000000010000003d00000073f8fbef0100000007000000310000000000000000000000000000000000000001000000000000000152010065100000000100000001007702010000000000000056434d54c898624b0500000000000000565245430c0000000600000000000000000000000000000001000000210000007cb92aad01000000050000001500000000000000000000000101420004000000000000000056434d5431cd90b50600000000000000565245430c0000000700000000000000000000000000000001000000200000003ebbb394010000000a00000014000000000000000000000000000000010000000000000056434d5451446f4f0700000000000000565245430c00000008000000000000000000000000000000020000004e0000002437d02201000000040000001600000002000000000000000100630101430400000000000000070000002400000001000000000000000100000000000000020000000000000001530000040000000000000056434d546e39e9c10800000000000000565245430c00000009000000000000000000000000000000010000001400000026baca89010000000900000008000000000000000000000056434d544689d11d0900000000000000";
const COMPACT_HEX: &str = "565245430900000001000000000000000100000000000000020000001a000000b06e9d9c000000000000000000000000000000803f01000000780000000056434d54d26dc4300100000000000000565245430a000000020000000000000000000000000000000000000020000000964e86b5010000000000000000000000000000000000000000000000000000000000000056434d547fb76c9b0200000000000000565245430c0000000300000000000000000000000000000007000000d400000016adde320100000001000000020000000167040000002400000000000000000000000100610201410142100000000100000004006e616d650101000000780400000014000000010000000000000001006200040000000000000004000000160000000200000000000000010063010143040000000000000007000000240000000100000000000000010000000000000002000000000000000153000004000000000000000a00000014000000000000000000000000000000010000000000000003000000100000000300000000000000020000000000000056434d54fc81473b0300000000000000";

fn workload(db: &mut Database) {
    db.put(1, &[0.0, 1.0], b"x").unwrap();
    db.create_graph("g").unwrap();
    let mut graph = db.graph_mut("g").unwrap();
    graph
        .create_node(
            "a",
            &["A"],
            &Metadata::from([("name".into(), MetadataValue::String("x".into()))]),
        )
        .unwrap();
    graph.create_node("b", &[], &Metadata::new()).unwrap();
    graph
        .create_edge(Some("e"), "a", "R", "b", &props("w", 1))
        .unwrap();
    let patch = NodePatch {
        add_labels: vec!["B".into()],
        ..NodePatch::default()
    };
    graph.patch_node("a", &patch, Expected::Any).unwrap();
    graph.attach("a", "default", 1).unwrap();
    graph
        .write_batch(&[
            GraphOp::UpsertNode {
                key: "c".into(),
                labels: vec!["C".into()],
                properties: Metadata::new(),
                expected: Expected::Absent,
            },
            GraphOp::CreateEdge {
                key: None,
                source: "b".into(),
                relationship: "S".into(),
                destination: "c".into(),
                properties: Metadata::new(),
            },
        ])
        .unwrap();
    graph
        .delete_edge(EdgeRef::Key("e".into()), Expected::Any)
        .unwrap();
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn files_without_deadlines_keep_their_encoding() {
    let mut db = Database::create_in_memory(2, CreateOptions::default()).unwrap();
    workload(&mut db);
    assert_eq!(hex(&db.to_bytes().unwrap()[32..]), LOG_HEX);

    let dir = tempdir().unwrap();
    let source = dir.path().join("s.vlite");
    let mut db = Database::create(&source, 2).unwrap();
    workload(&mut db);
    drop(db);
    let compacted = dir.path().join("c.vlite");
    maintenance::compact(&source, &compacted).unwrap();
    assert_eq!(hex(&fs::read(&compacted).unwrap()[32..]), COMPACT_HEX);
}
