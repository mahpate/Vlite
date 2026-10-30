use super::super::codec::{decode_body, encode_body};
use super::super::limits::MAX_BATCH_ENTRIES;
use super::*;
use std::collections::BTreeMap;

const G: GraphId = GraphId(0);

fn set(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|v| (*v).to_owned()).collect()
}

fn props(pairs: &[(&str, PropertyValue)]) -> Properties {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

fn int(value: i64) -> PropertyValue {
    PropertyValue::Integer(value)
}

fn create(key: &str) -> GraphMutation {
    GraphMutation::CreateNode {
        key: key.into(),
        labels: set(&["L"]),
        properties: Properties::new(),
    }
}

fn edge(key: Option<&str>, source: &str, destination: &str) -> GraphMutation {
    GraphMutation::CreateEdge {
        key: key.map(str::to_owned),
        source: source.into(),
        relationship: "R".into(),
        destination: destination.into(),
        properties: Properties::new(),
    }
}

fn delete_node(key: &str, expected: Precondition) -> GraphMutation {
    GraphMutation::DeleteNode {
        key: key.into(),
        expected,
    }
}

fn node(id: u64, created: bool) -> MutationOutcome {
    MutationOutcome::Node {
        id: NodeId(id),
        created,
    }
}

fn edge_outcome(id: u64, created: bool) -> MutationOutcome {
    MutationOutcome::Edge {
        id: EdgeId(id),
        created,
    }
}

/// Plans and commits batches. Every committed body is encoded, decoded and
/// replayed into `replica` (INV-009); failed plans must leave state unchanged.
struct Db {
    state: GraphState,
    replica: GraphState,
    sequence: u64,
}

impl Db {
    fn new() -> Self {
        let mut db = Self {
            state: GraphState::new(),
            replica: GraphState::new(),
            sequence: 1,
        };
        let create = GraphBody {
            graph_id: G,
            entries: vec![GraphEntry::CreateGraph { name: "g".into() }],
        };
        db.state.apply(1, &create).unwrap();
        db.replica.apply(1, &create).unwrap();
        db
    }

    fn run(&mut self, mutations: &[GraphMutation]) -> Result<Vec<MutationOutcome>, GraphError> {
        self.run_at(None, mutations)
    }

    /// `run`, with the batch first reclaiming what is due at `now` (Gap 3).
    fn run_at(
        &mut self,
        now: Option<u64>,
        mutations: &[GraphMutation],
    ) -> Result<Vec<MutationOutcome>, GraphError> {
        self.sequence += 1;
        let before = self.state.clone();
        let sequence = self.sequence;
        let state = &mut self.state;
        let planned = (|| -> Result<_, GraphError> {
            let mut planner = state.planner(sequence, G)?;
            if let Some(now) = now {
                planner.reclaim_expired(now);
            }
            let outcomes = mutations
                .iter()
                .map(|m| planner.mutate(m))
                .collect::<Result<Vec<_>, _>>()?;
            Ok((outcomes, planner.finish()))
        })();
        assert_eq!(self.state, before, "planning must never change state");
        let (outcomes, body) = planned?;
        if !body.entries.is_empty() {
            self.state.apply(self.sequence, &body).unwrap();
            let bytes = encode_body(&body).unwrap();
            let decoded = decode_body(&bytes, body.entries.len() as u32).unwrap();
            self.replica.apply(self.sequence, &decoded).unwrap();
        }
        Ok(outcomes)
    }

    fn ok(&mut self, mutations: &[GraphMutation]) -> Vec<MutationOutcome> {
        self.run(mutations).unwrap()
    }

    fn err(&mut self, mutations: &[GraphMutation]) -> GraphError {
        self.run(mutations).unwrap_err()
    }

    fn graph(&self) -> &Graph {
        self.state.graph(G).unwrap()
    }

    fn check(&self) {
        self.state.check_invariants().unwrap();
        assert_eq!(self.state, self.replica, "replay must rebuild equal state");
    }
}

#[test]
fn planner_requires_an_increasing_sequence_and_a_live_graph() {
    let mut db = Db::new();
    assert_eq!(
        db.state.planner(1, G).err(),
        Some(GraphError::SequenceNotIncreasing {
            last: 1,
            sequence: 1
        })
    );
    assert_eq!(
        db.state.planner(2, GraphId(1)).err(),
        Some(GraphError::GraphNotFound(GraphId(1)))
    );
    let planner = db.state.planner(2, G).unwrap();
    assert!(planner.is_empty());
    assert_eq!(planner.finish().entries, Vec::new());
}

#[test]
fn create_node_assigns_ids_and_rejects_existing_keys() {
    let mut db = Db::new();
    assert_eq!(
        db.ok(&[create("a"), create("b")]),
        [node(0, true), node(1, true)]
    );
    assert_eq!(db.graph().node_by_key("a").unwrap().revision, 2);
    assert_eq!(
        db.err(&[create("a")]),
        GraphError::NodeKeyExists("a".into())
    );
    // The failed batch planned nothing: the next ID is still 2.
    assert_eq!(
        db.err(&[create("c"), create("c")]),
        GraphError::NodeKeyExists("c".into())
    );
    assert_eq!(db.graph().next_node_id(), 2);
    assert_eq!(db.err(&[create("")]), GraphError::InvalidNodeKey);
    assert_eq!(
        db.err(&[create(&"k".repeat(257))]),
        GraphError::InvalidNodeKey
    );
    db.ok(&[create(&"k".repeat(256))]);
    db.check();
}

#[test]
fn upsert_creates_or_replaces_and_checks_revisions() {
    let mut db = Db::new();
    let upsert = |expected, value| GraphMutation::UpsertNode {
        key: "a".into(),
        labels: set(&["X"]),
        properties: props(&[("p", int(value))]),
        expected,
    };
    assert_eq!(
        db.err(&[upsert(Precondition::Revision(1), 0)]),
        GraphError::PreconditionFailed { current: None }
    );
    assert_eq!(db.ok(&[upsert(Precondition::Absent, 0)]), [node(0, true)]);
    let revision = db.graph().node_by_key("a").unwrap().revision;
    assert_eq!(
        db.err(&[upsert(Precondition::Absent, 1)]),
        GraphError::PreconditionFailed {
            current: Some(revision)
        }
    );
    assert_eq!(
        db.err(&[upsert(Precondition::Revision(revision + 7), 1)]),
        GraphError::PreconditionFailed {
            current: Some(revision)
        }
    );
    assert_eq!(
        db.ok(&[upsert(Precondition::Revision(revision), 1)]),
        [node(0, false)]
    );
    let current = db.graph().node_by_key("a").unwrap();
    assert_eq!(current.properties, props(&[("p", int(1))]));
    assert!(current.revision > revision);
    db.ok(&[upsert(Precondition::Any, 2)]);
    assert_eq!(db.graph().next_node_id(), 1);
    db.check();
}

#[test]
fn patch_node_merges_and_validates() {
    let mut db = Db::new();
    db.ok(&[GraphMutation::CreateNode {
        key: "a".into(),
        labels: set(&["A", "B"]),
        properties: props(&[("p", int(1)), ("q", int(2))]),
    }]);
    let patch = |add: &[&str], remove: &[&str], expected| GraphMutation::PatchNode {
        key: "a".into(),
        add_labels: set(add),
        remove_labels: set(remove),
        set: props(&[("r", int(3))]),
        unset: set(&["q"]),
        expected,
    };
    assert_eq!(
        db.ok(&[patch(&["C"], &["A"], Precondition::Revision(2))]),
        [node(0, false)]
    );
    let current = db.graph().node_by_key("a").unwrap();
    assert_eq!(current.labels, set(&["B", "C"]));
    assert_eq!(current.properties, props(&[("p", int(1)), ("r", int(3))]));
    assert_eq!(current.revision, 3);
    assert_eq!(
        db.err(&[patch(&["C"], &["C"], Precondition::Any)]),
        GraphError::InvalidLabel
    );
    assert_eq!(
        db.err(&[patch(&[], &[], Precondition::Absent)]),
        GraphError::PreconditionFailed { current: Some(3) }
    );
    let missing = GraphMutation::PatchNode {
        key: "z".into(),
        add_labels: BTreeSet::new(),
        remove_labels: BTreeSet::new(),
        set: Properties::new(),
        unset: BTreeSet::new(),
        expected: Precondition::Any,
    };
    assert_eq!(db.err(&[missing]), GraphError::UnknownNodeKey("z".into()));
    // Two labels exist; seven more exceed MAX_LABELS (8) only after merging.
    let more: Vec<String> = (0..7).map(|i| format!("L{i}")).collect();
    let more: Vec<&str> = more.iter().map(String::as_str).collect();
    assert_eq!(
        db.err(&[patch(&more, &[], Precondition::Any)]),
        GraphError::TooManyLabels
    );
    db.check();
}

#[test]
fn delete_node_requires_no_edges_and_drops_attachments() {
    let mut db = Db::new();
    db.ok(&[
        create("a"),
        create("b"),
        edge(None, "a", "b"),
        GraphMutation::Attach {
            node: "a".into(),
            collection: 3,
            record: 9,
        },
    ]);
    assert!(matches!(
        db.err(&[delete_node("a", Precondition::Any)]),
        GraphError::NodeHasEdges {
            node: NodeId(0),
            ..
        }
    ));
    assert_eq!(
        db.err(&[delete_node("z", Precondition::Any)]),
        GraphError::UnknownNodeKey("z".into())
    );
    assert_eq!(
        db.err(&[delete_node("z", Precondition::Revision(4))]),
        GraphError::PreconditionFailed { current: None }
    );
    // Later mutations observe earlier ones in the same batch.
    assert_eq!(
        db.ok(&[
            GraphMutation::DeleteEdge {
                edge: EdgeSelector::Id(EdgeId(0)),
                expected: Precondition::Revision(2)
            },
            delete_node("a", Precondition::Revision(2)),
        ]),
        [
            MutationOutcome::EdgeDeleted(EdgeId(0)),
            MutationOutcome::NodeDeleted(NodeId(0))
        ]
    );
    assert_eq!(db.state.attachment_owner(3, 9), None);
    // The deleted key is reusable but its ID is not.
    assert_eq!(db.ok(&[create("a")]), [node(2, true)]);
    db.check();
}

#[test]
fn keyed_edges_are_idempotent_and_conflicts_are_rejected() {
    let mut db = Db::new();
    db.ok(&[create("a"), create("b")]);
    let keyed = |destination: &str, value: f64| GraphMutation::CreateEdge {
        key: Some("e".into()),
        source: "a".into(),
        relationship: "R".into(),
        destination: destination.into(),
        properties: props(&[("w", PropertyValue::Float(value))]),
    };
    assert_eq!(db.ok(&[keyed("b", 0.0)]), [edge_outcome(0, true)]);
    let before = db.state.clone();
    // An identical retry writes nothing, even twice in one batch.
    assert_eq!(
        db.ok(&[keyed("b", 0.0), keyed("b", 0.0)]),
        vec![edge_outcome(0, false); 2]
    );
    assert_eq!(db.state, before);
    assert_eq!(
        db.err(&[keyed("a", 0.0)]),
        GraphError::EdgeKeyConflict("e".into())
    );
    // Property identity is bit-exact: -0.0 is a different value.
    assert_eq!(
        db.err(&[keyed("b", -0.0)]),
        GraphError::EdgeKeyConflict("e".into())
    );
    let mut other_type = keyed("b", 0.0);
    if let GraphMutation::CreateEdge { relationship, .. } = &mut other_type {
        *relationship = "S".into();
    }
    assert_eq!(
        db.err(&[other_type]),
        GraphError::EdgeKeyConflict("e".into())
    );
    // Missing endpoints and invalid input are reported before key identity.
    assert_eq!(
        db.err(&[keyed("z", 0.0)]),
        GraphError::UnknownNodeKey("z".into())
    );
    let mut invalid = keyed("b", 0.0);
    if let GraphMutation::CreateEdge { relationship, .. } = &mut invalid {
        *relationship = "bad type".into();
    }
    assert_eq!(db.err(&[invalid]), GraphError::InvalidRelationshipType);
    // Keyless edges always create parallel edges.
    assert_eq!(
        db.ok(&[edge(None, "a", "b"), edge(None, "a", "b")]),
        [edge_outcome(1, true), edge_outcome(2, true)]
    );
    // A deleted key can be created again with a new ID.
    db.ok(&[GraphMutation::DeleteEdge {
        edge: EdgeSelector::Key("e".into()),
        expected: Precondition::Any,
    }]);
    assert_eq!(db.ok(&[keyed("a", 1.0)]), [edge_outcome(3, true)]);
    db.check();
}

#[test]
fn edges_are_addressed_by_id_or_key_with_revisions() {
    let mut db = Db::new();
    db.ok(&[create("a"), edge(Some("e"), "a", "a")]);
    let patch = |edge, expected| GraphMutation::PatchEdge {
        edge,
        set: props(&[("p", int(1))]),
        unset: BTreeSet::new(),
        expected,
    };
    assert_eq!(
        db.err(&[patch(EdgeSelector::Key("e".into()), Precondition::Absent)]),
        GraphError::PreconditionFailed { current: Some(2) }
    );
    assert_eq!(
        db.ok(&[patch(
            EdgeSelector::Id(EdgeId(0)),
            Precondition::Revision(2)
        )]),
        [edge_outcome(0, false)]
    );
    let revision = db.graph().edge(EdgeId(0)).unwrap().revision;
    assert_eq!(revision, db.sequence);
    assert_eq!(
        db.err(&[patch(EdgeSelector::Id(EdgeId(5)), Precondition::Any)]),
        GraphError::EdgeNotFound(EdgeId(5))
    );
    assert_eq!(
        db.err(&[patch(EdgeSelector::Key("x".into()), Precondition::Any)]),
        GraphError::UnknownEdgeKey("x".into())
    );
    assert_eq!(
        db.err(&[patch(EdgeSelector::Key(String::new()), Precondition::Any)]),
        GraphError::InvalidEdgeKey
    );
    assert_eq!(
        db.err(&[GraphMutation::DeleteEdge {
            edge: EdgeSelector::Key("e".into()),
            expected: Precondition::Revision(2)
        }]),
        GraphError::PreconditionFailed {
            current: Some(revision)
        }
    );
    db.ok(&[GraphMutation::DeleteEdge {
        edge: EdgeSelector::Key("e".into()),
        expected: Precondition::Revision(revision),
    }]);
    assert_eq!(db.graph().edge_count(), 0);
    db.check();
}

#[test]
fn attachments_are_planned_and_reported() {
    let mut db = Db::new();
    db.ok(&[create("a"), create("b")]);
    let attach = |node: &str, collection, record| GraphMutation::Attach {
        node: node.into(),
        collection,
        record,
    };
    assert_eq!(
        db.ok(&[attach("a", 1, 7)]),
        [MutationOutcome::Attached(NodeId(0))]
    );
    assert!(matches!(
        db.err(&[attach("b", 1, 7)]),
        GraphError::RecordAlreadyAttached { record: 7, .. }
    ));
    assert!(matches!(
        db.err(&[attach("a", 1, 8)]),
        GraphError::NodeAlreadyAttached { .. }
    ));
    assert_eq!(
        db.err(&[attach("z", 1, 8)]),
        GraphError::UnknownNodeKey("z".into())
    );
    // Moving an attachment within one batch.
    assert_eq!(
        db.ok(&[
            GraphMutation::Detach {
                node: "a".into(),
                collection: 1
            },
            attach("b", 1, 7),
        ]),
        [
            MutationOutcome::Detached {
                node: NodeId(0),
                record: 7
            },
            MutationOutcome::Attached(NodeId(1))
        ]
    );
    assert_eq!(db.state.attachment_owner(1, 7), Some((G, NodeId(1))));
    assert!(matches!(
        db.err(&[GraphMutation::Detach {
            node: "a".into(),
            collection: 1
        }]),
        GraphError::AttachmentNotFound { .. }
    ));
    db.check();
}

#[test]
fn planner_enforces_the_entry_limit() {
    let mut db = Db::new();
    let mutations: Vec<_> = (0..MAX_BATCH_ENTRIES)
        .map(|i| create(&format!("n{i}")))
        .collect();
    db.ok(&mutations);
    assert_eq!(db.graph().node_count(), MAX_BATCH_ENTRIES);
    let mut too_many: Vec<_> = (0..MAX_BATCH_ENTRIES)
        .map(|i| create(&format!("m{i}")))
        .collect();
    too_many.push(create("last"));
    assert_eq!(
        db.err(&too_many),
        GraphError::BatchSize("more than 4096 entries")
    );
    assert_eq!(db.graph().node_count(), MAX_BATCH_ENTRIES);
    db.check();
}

#[test]
fn vector_entries_are_validated_and_planned() {
    let mut db = Db::new();
    let mut planner = db.state.planner(2, G).unwrap();
    assert_eq!(
        planner.vector(1, vec![9; 16]),
        Err(GraphError::InvalidVectorMutation("reserved bytes"))
    );
    let mut delete = vec![2, 0, 0, 0];
    delete.extend_from_slice(&5u64.to_le_bytes());
    delete.extend_from_slice(&0u32.to_le_bytes());
    planner.vector(1, delete.clone()).unwrap();
    assert_eq!(planner.len(), 1);
    let body = planner.finish();
    assert_eq!(
        body.entries,
        [GraphEntry::VectorMutation {
            collection: 1,
            entry: delete
        }]
    );
}

#[test]
fn touch_record_advances_only_the_owning_graph() {
    let mut db = Db::new();
    db.ok(&[
        create("a"),
        GraphMutation::Attach {
            node: "a".into(),
            collection: 1,
            record: 4,
        },
    ]);
    let before = db.graph().graph_sequence();
    db.state.touch_record(1, 5, 50);
    assert_eq!(db.graph().graph_sequence(), before);
    db.state.touch_record(1, 4, 60);
    assert_eq!(db.graph().graph_sequence(), 60);
    db.state.touch_record(1, 4, 55);
    assert_eq!(db.graph().graph_sequence(), 60);
}

/// Deadlines are planned like other writes, and a batch that starts
/// with `reclaim_expired` sees due nodes (with all their edges) and due
/// edges as absent. The reclaim does not count against the entry limit.
#[test]
fn deadlines_are_planned_and_due_entities_are_absent() {
    let mut db = Db::new();
    db.ok(&[
        create("a"),
        create("b"),
        edge(Some("e"), "a", "b"),
        edge(Some("f"), "b", "b"),
    ]);
    let node_expiry = |key: &str, expires_at_ms, expected| GraphMutation::SetNodeExpiry {
        key: key.into(),
        expires_at_ms,
        expected,
    };
    let edge_expiry = |key: &str, expires_at_ms, expected| GraphMutation::SetEdgeExpiry {
        edge: EdgeSelector::Key(key.into()),
        expires_at_ms,
        expected,
    };
    assert_eq!(
        db.err(&[node_expiry("a", Some(10), Precondition::Absent)]),
        GraphError::PreconditionFailed { current: Some(2) }
    );
    assert_eq!(
        db.ok(&[
            node_expiry("a", Some(10), Precondition::Revision(2)),
            edge_expiry("f", Some(20), Precondition::Any),
        ]),
        [node(0, false), edge_outcome(1, false)]
    );
    assert_eq!(db.graph().node_expiry(NodeId(0)), Some(10));
    assert_eq!(db.graph().node(NodeId(0)).unwrap().revision, db.sequence);
    assert_eq!(db.graph().edge_expiry(EdgeId(1)), Some(20));
    assert_eq!(
        db.err(&[node_expiry("z", None, Precondition::Any)]),
        GraphError::UnknownNodeKey("z".into())
    );
    assert_eq!(
        db.err(&[edge_expiry("z", None, Precondition::Any)]),
        GraphError::UnknownEdgeKey("z".into())
    );
    db.check();

    // Nothing is due before the first deadline, so nothing is planned.
    assert!(db.run_at(Some(9), &[]).unwrap().is_empty());
    assert_eq!(db.graph().node_count(), 2);
    // From 10, `a` and its edge `e` are absent.
    assert_eq!(
        db.run_at(Some(10), &[node_expiry("a", None, Precondition::Any)]),
        Err(GraphError::UnknownNodeKey("a".into()))
    );
    assert_eq!(
        db.run_at(Some(10), &[edge(None, "b", "a")]),
        Err(GraphError::UnknownNodeKey("a".into()))
    );
    assert_eq!(
        db.run_at(Some(10), &[edge_expiry("e", None, Precondition::Any)]),
        Err(GraphError::UnknownEdgeKey("e".into()))
    );
    assert_eq!(
        db.run_at(Some(10), &[create("a"), edge(Some("e"), "a", "b")]),
        Ok(vec![node(2, true), edge_outcome(2, true)])
    );
    assert_eq!(db.graph().node_count(), 2);
    assert_eq!(db.graph().node_expiry(NodeId(0)), None);
    db.check();
    // From 20, `f` is absent on its own; `b` stays.
    db.run_at(Some(20), &[create("c")]).unwrap();
    assert!(db.graph().edge_by_key("f").is_none());
    assert_eq!((db.graph().node_count(), db.graph().edge_count()), (3, 1));
    db.check();

    db.ok(&[node_expiry("c", Some(30), Precondition::Any)]);
    let full: Vec<_> = (0..MAX_BATCH_ENTRIES)
        .map(|i| create(&format!("n{i}")))
        .collect();
    db.run_at(Some(30), &full).unwrap();
    assert_eq!(db.graph().node_count(), 2 + MAX_BATCH_ENTRIES);
    db.check();
}

// ---------------------------------------------------------------------------
// Reference model (TEST-003): an independent map-based model of the planner's
// semantics, driven by seeded random batches.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct MNode {
    id: u64,
    labels: BTreeSet<String>,
    properties: Properties,
    revision: u64,
}

#[derive(Clone, Debug)]
struct MEdge {
    key: Option<String>,
    source: u64,
    relationship: String,
    destination: u64,
    properties: Properties,
    revision: u64,
}

#[derive(Clone, Debug, Default)]
struct Model {
    nodes: BTreeMap<String, MNode>,
    edges: BTreeMap<u64, MEdge>,
    next_node: u64,
    next_edge: u64,
    attachments: BTreeMap<u64, BTreeMap<u16, u64>>,
    graph_sequence: u64,
}

/// Properties with floats rendered as bit patterns, for bit-exact comparison.
fn bits(properties: &Properties) -> Vec<(String, String)> {
    properties
        .iter()
        .map(|(k, v)| {
            let v = match v {
                PropertyValue::Float(f) => format!("f{}", f.to_bits()),
                other => format!("{other:?}"),
            };
            (k.clone(), v)
        })
        .collect()
}

impl Model {
    fn precondition(expected: Precondition, current: Option<u64>) -> Result<(), GraphError> {
        let ok = match expected {
            Precondition::Any => true,
            Precondition::Absent => current.is_none(),
            Precondition::Revision(r) => current == Some(r),
        };
        if ok {
            Ok(())
        } else {
            Err(GraphError::PreconditionFailed { current })
        }
    }

    fn node(&self, key: &str) -> Result<u64, GraphError> {
        self.nodes
            .get(key)
            .map(|n| n.id)
            .ok_or_else(|| GraphError::UnknownNodeKey(key.into()))
    }

    fn checked_node(&self, key: &str, expected: Precondition) -> Result<u64, GraphError> {
        Self::precondition(expected, self.nodes.get(key).map(|n| n.revision))?;
        self.node(key)
    }

    fn checked_edge(&self, edge: &EdgeSelector, expected: Precondition) -> Result<u64, GraphError> {
        let found = match edge {
            EdgeSelector::Id(id) => self.edges.get(&id.0).map(|e| (id.0, e)),
            EdgeSelector::Key(key) => self
                .edges
                .iter()
                .find(|(_, e)| e.key.as_deref() == Some(key))
                .map(|(id, e)| (*id, e)),
        };
        Self::precondition(expected, found.map(|(_, e)| e.revision))?;
        match (found, edge) {
            (Some((id, _)), _) => Ok(id),
            (None, EdgeSelector::Id(id)) => Err(GraphError::EdgeNotFound(*id)),
            (None, EdgeSelector::Key(key)) => Err(GraphError::UnknownEdgeKey(key.clone())),
        }
    }

    fn owner(&self, collection: u16, record: u64) -> Option<u64> {
        self.attachments
            .iter()
            .find(|(_, map)| map.get(&collection) == Some(&record))
            .map(|(node, _)| *node)
    }

    /// Returns the outcome and whether an entry was written.
    fn apply(
        &mut self,
        mutation: &GraphMutation,
        sequence: u64,
    ) -> Result<(MutationOutcome, bool), GraphError> {
        Ok(match mutation {
            GraphMutation::CreateNode {
                key,
                labels,
                properties,
            } => {
                if self.nodes.contains_key(key) {
                    return Err(GraphError::NodeKeyExists(key.clone()));
                }
                let id = self.next_node;
                self.next_node += 1;
                self.nodes.insert(
                    key.clone(),
                    MNode {
                        id,
                        labels: labels.clone(),
                        properties: properties.clone(),
                        revision: sequence,
                    },
                );
                (node(id, true), true)
            }
            GraphMutation::UpsertNode {
                key,
                labels,
                properties,
                expected,
            } => {
                Self::precondition(*expected, self.nodes.get(key).map(|n| n.revision))?;
                let (id, created) = match self.nodes.get(key) {
                    Some(existing) => (existing.id, false),
                    None => {
                        self.next_node += 1;
                        (self.next_node - 1, true)
                    }
                };
                self.nodes.insert(
                    key.clone(),
                    MNode {
                        id,
                        labels: labels.clone(),
                        properties: properties.clone(),
                        revision: sequence,
                    },
                );
                (node(id, created), true)
            }
            GraphMutation::PatchNode {
                key,
                add_labels,
                remove_labels,
                set,
                unset,
                expected,
            } => {
                let id = self.checked_node(key, *expected)?;
                let current = self.nodes.get_mut(key).unwrap();
                current.labels.retain(|l| !remove_labels.contains(l));
                current.labels.extend(add_labels.iter().cloned());
                current.properties.retain(|k, _| !unset.contains(k));
                current
                    .properties
                    .extend(set.iter().map(|(k, v)| (k.clone(), v.clone())));
                current.revision = sequence;
                (node(id, false), true)
            }
            GraphMutation::DeleteNode { key, expected } => {
                let id = self.checked_node(key, *expected)?;
                let incident = self
                    .edges
                    .values()
                    .filter(|e| e.source == id || e.destination == id)
                    .count();
                if incident > 0 {
                    return Err(GraphError::NodeHasEdges {
                        node: NodeId(id),
                        incident,
                    });
                }
                self.nodes.remove(key);
                self.attachments.remove(&id);
                (MutationOutcome::NodeDeleted(NodeId(id)), true)
            }
            GraphMutation::CreateEdge {
                key,
                source,
                relationship,
                destination,
                properties,
            } => {
                let source = self.node(source)?;
                let destination = self.node(destination)?;
                if let Some(key) = key
                    && let Some((id, existing)) =
                        self.edges.iter().find(|(_, e)| e.key.as_ref() == Some(key))
                {
                    let identical = existing.source == source
                        && existing.destination == destination
                        && existing.relationship == *relationship
                        && bits(&existing.properties) == bits(properties);
                    return if identical {
                        Ok((edge_outcome(*id, false), false))
                    } else {
                        Err(GraphError::EdgeKeyConflict(key.clone()))
                    };
                }
                let id = self.next_edge;
                self.next_edge += 1;
                self.edges.insert(
                    id,
                    MEdge {
                        key: key.clone(),
                        source,
                        relationship: relationship.clone(),
                        destination,
                        properties: properties.clone(),
                        revision: sequence,
                    },
                );
                (edge_outcome(id, true), true)
            }
            GraphMutation::PatchEdge {
                edge,
                set,
                unset,
                expected,
            } => {
                let id = self.checked_edge(edge, *expected)?;
                let current = self.edges.get_mut(&id).unwrap();
                current.properties.retain(|k, _| !unset.contains(k));
                current
                    .properties
                    .extend(set.iter().map(|(k, v)| (k.clone(), v.clone())));
                current.revision = sequence;
                (edge_outcome(id, false), true)
            }
            GraphMutation::DeleteEdge { edge, expected } => {
                let id = self.checked_edge(edge, *expected)?;
                self.edges.remove(&id);
                (MutationOutcome::EdgeDeleted(EdgeId(id)), true)
            }
            GraphMutation::Attach {
                node,
                collection,
                record,
            } => {
                let id = self.node(node)?;
                if self
                    .attachments
                    .get(&id)
                    .is_some_and(|m| m.contains_key(collection))
                {
                    return Err(GraphError::NodeAlreadyAttached {
                        node: NodeId(id),
                        collection: *collection,
                    });
                }
                if let Some(owner) = self.owner(*collection, *record) {
                    return Err(GraphError::RecordAlreadyAttached {
                        collection: *collection,
                        record: *record,
                        graph: G,
                        node: NodeId(owner),
                    });
                }
                self.attachments
                    .entry(id)
                    .or_default()
                    .insert(*collection, *record);
                (MutationOutcome::Attached(NodeId(id)), true)
            }
            GraphMutation::Detach { node, collection } => {
                let id = self.node(node)?;
                let missing = GraphError::AttachmentNotFound {
                    node: NodeId(id),
                    collection: *collection,
                };
                let map = self.attachments.get_mut(&id).ok_or(missing.clone())?;
                let record = map.remove(collection).ok_or(missing)?;
                if map.is_empty() {
                    self.attachments.remove(&id);
                }
                (
                    MutationOutcome::Detached {
                        node: NodeId(id),
                        record,
                    },
                    true,
                )
            }
            GraphMutation::PutSchema(_)
            | GraphMutation::EnsureEdge { .. }
            | GraphMutation::SetNodeExpiry { .. }
            | GraphMutation::SetEdgeExpiry { .. } => {
                unreachable!("the model generates no schema or expiry mutations")
            }
        })
    }

    fn batch(
        &mut self,
        mutations: &[GraphMutation],
        sequence: u64,
    ) -> Result<Vec<MutationOutcome>, GraphError> {
        let mut next = self.clone();
        let mut wrote = false;
        let mut outcomes = Vec::new();
        for mutation in mutations {
            let (outcome, written) = next.apply(mutation, sequence)?;
            wrote |= written;
            outcomes.push(outcome);
        }
        if wrote {
            next.graph_sequence = sequence;
        }
        *self = next;
        Ok(outcomes)
    }

    fn assert_matches(&self, graph: &Graph, context: &str) {
        assert_eq!(graph.node_count(), self.nodes.len(), "{context}");
        assert_eq!(graph.edge_count(), self.edges.len(), "{context}");
        assert_eq!(graph.next_node_id(), self.next_node, "{context}");
        assert_eq!(graph.next_edge_id(), self.next_edge, "{context}");
        assert_eq!(graph.graph_sequence(), self.graph_sequence, "{context}");
        for (key, expected) in &self.nodes {
            let actual = graph.node_by_key(key).expect(context);
            assert_eq!(actual.id.0, expected.id, "{context}");
            assert_eq!(actual.labels, expected.labels, "{context}");
            assert_eq!(
                bits(&actual.properties),
                bits(&expected.properties),
                "{context}"
            );
            assert_eq!(actual.revision, expected.revision, "{context}");
            let attachments: BTreeMap<u16, u64> = graph.attachments(actual.id).collect();
            assert_eq!(
                attachments,
                self.attachments
                    .get(&expected.id)
                    .cloned()
                    .unwrap_or_default(),
                "{context}"
            );
        }
        for (id, expected) in &self.edges {
            let actual = graph.edge(EdgeId(*id)).expect(context);
            assert_eq!(actual.key, expected.key, "{context}");
            assert_eq!(actual.source.0, expected.source, "{context}");
            assert_eq!(actual.destination.0, expected.destination, "{context}");
            assert_eq!(actual.relationship, expected.relationship, "{context}");
            assert_eq!(
                bits(&actual.properties),
                bits(&expected.properties),
                "{context}"
            );
            assert_eq!(actual.revision, expected.revision, "{context}");
        }
    }
}

/// Compares errors, ignoring the incident-edge count convention.
fn same_error(actual: &GraphError, expected: &GraphError) -> bool {
    match (actual, expected) {
        (GraphError::NodeHasEdges { node: a, .. }, GraphError::NodeHasEdges { node: b, .. }) => {
            a == b
        }
        _ => actual == expected,
    }
}

/// TEST-003 (planner part): 20 seeds × 10,000 mutations in batches of 1..=3.
/// Every batch is compared with the model (outcomes or exact errors, and
/// atomicity through the model's all-or-nothing batches); the full state,
/// invariants and replay equality are compared every 100 batches and at the
/// end of each seed.
#[test]
fn reference_model_matches_planner_over_seeded_mutations() {
    for seed in 1..=20u64 {
        let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut next = move |bound: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % bound
        };
        let mut db = Db::new();
        let mut model = Model {
            graph_sequence: 1,
            ..Model::default()
        };
        let (mut mutations_run, mut batches, mut ok, mut failed) = (0usize, 0u64, 0, 0);
        while mutations_run < 10_000 {
            let size = 1 + next(3) as usize;
            let mut batch = Vec::with_capacity(size);
            for _ in 0..size {
                let key = format!("k{}", next(12));
                let other = format!("k{}", next(12));
                let expected = match next(4) {
                    0 | 1 => Precondition::Any,
                    2 => Precondition::Absent,
                    _ => {
                        let current = model.nodes.get(&key).map(|n| n.revision);
                        Precondition::Revision(match (current, next(2)) {
                            (Some(r), 0) => r,
                            _ => next(db.sequence + 1),
                        })
                    }
                };
                let value = match next(5) {
                    0 => None,
                    1 => Some(PropertyValue::Float(0.0)),
                    2 => Some(PropertyValue::Float(-0.0)),
                    _ => Some(int(next(3) as i64)),
                };
                let properties: Properties =
                    value.into_iter().map(|v| ("p".to_owned(), v)).collect();
                let labels = set(&[["A", "B", "C"][next(3) as usize]]);
                let selector = if next(2) == 0 {
                    EdgeSelector::Id(EdgeId(next(model.next_edge + 2)))
                } else {
                    EdgeSelector::Key(format!("e{}", next(8)))
                };
                let edge_expected = match (&selector, next(3)) {
                    (_, 0 | 1) => Precondition::Any,
                    (EdgeSelector::Id(id), _) => model
                        .edges
                        .get(&id.0)
                        .map_or(Precondition::Absent, |e| Precondition::Revision(e.revision)),
                    (EdgeSelector::Key(_), _) => Precondition::Revision(next(db.sequence + 1)),
                };
                batch.push(match next(14) {
                    0 | 1 => GraphMutation::CreateNode {
                        key,
                        labels,
                        properties,
                    },
                    2 => GraphMutation::UpsertNode {
                        key,
                        labels,
                        properties,
                        expected,
                    },
                    3 => GraphMutation::PatchNode {
                        key,
                        add_labels: labels,
                        remove_labels: set(&["D"]),
                        set: properties,
                        unset: if next(2) == 0 {
                            set(&["q"])
                        } else {
                            BTreeSet::new()
                        },
                        expected,
                    },
                    4 => delete_node(&key, expected),
                    5..=7 => GraphMutation::CreateEdge {
                        key: (next(3) != 0).then(|| format!("e{}", next(8))),
                        source: key,
                        relationship: ["R", "S"][next(2) as usize].into(),
                        destination: other,
                        properties,
                    },
                    8 => GraphMutation::PatchEdge {
                        edge: selector,
                        set: properties,
                        unset: BTreeSet::new(),
                        expected: edge_expected,
                    },
                    9 | 10 => GraphMutation::DeleteEdge {
                        edge: selector,
                        expected: edge_expected,
                    },
                    11 | 12 => GraphMutation::Attach {
                        node: key,
                        collection: next(2) as u16,
                        record: next(6),
                    },
                    _ => GraphMutation::Detach {
                        node: key,
                        collection: next(2) as u16,
                    },
                });
            }
            mutations_run += batch.len();
            batches += 1;
            let actual = db.run(&batch);
            let expected = model.batch(&batch, db.sequence);
            match (&actual, &expected) {
                (Ok(a), Ok(e)) => {
                    assert_eq!(a, e, "seed {seed} batch {batches}: {batch:?}");
                    ok += 1;
                }
                (Err(a), Err(e)) => {
                    assert!(
                        same_error(a, e),
                        "seed {seed} batch {batches}: {batch:?}: {a:?} != {e:?}"
                    );
                    failed += 1;
                }
                _ => panic!("seed {seed} batch {batches}: {batch:?}: {actual:?} vs {expected:?}"),
            }
            if batches.is_multiple_of(100) {
                model.assert_matches(db.graph(), &format!("seed {seed} batch {batches}"));
                db.check();
            }
        }
        model.assert_matches(db.graph(), &format!("seed {seed} end"));
        db.check();
        assert!(
            ok > 500 && failed > 500,
            "seed {seed}: {ok} ok / {failed} failed"
        );
    }
}
