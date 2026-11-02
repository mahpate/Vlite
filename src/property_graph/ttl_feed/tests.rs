//! Gap 3 tracker acceptance. Expected values are literals written by hand
//! or come from the independent model below, never from the tracker or the
//! engine's own due computation.

use super::*;
use crate::property_graph::engine::codec::OP_GRAPH;
use crate::property_graph::engine::{EdgeId, GraphBody, GraphId, NodeId, Properties, encode_body};
use crate::property_graph::frame::{reclaim_count, vector_entry};
use std::cmp::Ordering;

const OFFSET: u64 = 4242;
/// Collection IDs whose names sort the other way round.
const ZETA: u16 = 1;
const ALPHA: u16 = 2;

fn collections() -> BTreeMap<u16, (String, usize)> {
    BTreeMap::from([
        (0, ("default".to_string(), 2)),
        (ZETA, ("zeta".to_string(), 2)),
        (ALPHA, ("alpha".to_string(), 2)),
    ])
}

/// An op-12 frame of `entries` for graph `graph` at `sequence`.
fn raw(sequence: u64, graph: u16, entries: Vec<GraphEntry>) -> format::RawFrame {
    let body = GraphBody {
        graph_id: GraphId(graph),
        entries,
    };
    let mut plain = vec![0; format::PREFIX_SIZE];
    plain.extend(encode_body(&body).expect("valid body"));
    plain.extend([0; format::FOOTER_SIZE]);
    format::RawFrame {
        operation: OP_GRAPH,
        collection: 0,
        sequence,
        id: reclaim_count(&body),
        count: body.entries.len() as u32,
        disk: Vec::new(),
        plain,
    }
}

struct Fixture {
    graphs: TtlGraphs,
    sequence: u64,
}

impl Fixture {
    fn new() -> Self {
        Self {
            graphs: TtlGraphs::new(format::FLAG_GRAPH),
            sequence: 0,
        }
    }

    fn with(
        &mut self,
        graph: u16,
        entries: Vec<GraphEntry>,
        collections: &BTreeMap<u16, (String, usize)>,
    ) -> Result<Vec<TtlEvent>> {
        self.sequence += 1;
        self.graphs
            .frame(&raw(self.sequence, graph, entries), OFFSET, collections)
    }

    fn apply(&mut self, entries: Vec<GraphEntry>) -> Vec<TtlEvent> {
        self.with(0, entries, &collections()).expect("valid frame")
    }
}

fn put_node(id: u64, key: &str, labels: &[&str]) -> GraphEntry {
    GraphEntry::PutNode {
        node_id: NodeId(id),
        key: key.into(),
        labels: labels.iter().map(|label| label.to_string()).collect(),
        properties: Properties::new(),
    }
}

fn put_edge(id: u64, source: u64, destination: u64, key: Option<&str>) -> GraphEntry {
    GraphEntry::PutEdge {
        edge_id: EdgeId(id),
        source: NodeId(source),
        destination: NodeId(destination),
        relationship: "R".into(),
        key: key.map(str::to_string),
        properties: Properties::new(),
    }
}

fn node_expiry(id: u64, expires_at_ms: Option<u64>) -> GraphEntry {
    GraphEntry::NodeExpiry {
        node_id: NodeId(id),
        expires_at_ms,
    }
}

fn edge_expiry(id: u64, expires_at_ms: Option<u64>) -> GraphEntry {
    GraphEntry::EdgeExpiry {
        edge_id: EdgeId(id),
        expires_at_ms,
    }
}

fn attach(node: u64, collection: u16, record: u64) -> GraphEntry {
    GraphEntry::Attach {
        node_id: NodeId(node),
        collection,
        record_id: record,
    }
}

fn detach(node: u64, collection: u16) -> GraphEntry {
    GraphEntry::Detach {
        node_id: NodeId(node),
        collection,
    }
}

fn delete_node(id: u64) -> GraphEntry {
    GraphEntry::DeleteNode {
        node_id: NodeId(id),
    }
}

fn delete_edge(id: u64) -> GraphEntry {
    GraphEntry::DeleteEdge {
        edge_id: EdgeId(id),
    }
}

fn patch_node(id: u64) -> GraphEntry {
    GraphEntry::PatchNode {
        node_id: NodeId(id),
        add_labels: BTreeSet::from(["P".to_string()]),
        remove_labels: BTreeSet::new(),
        set: Properties::new(),
        unset: BTreeSet::new(),
    }
}

fn patch_edge(id: u64) -> GraphEntry {
    GraphEntry::PatchEdge {
        edge_id: EdgeId(id),
        set: Properties::new(),
        unset: BTreeSet::from(["gone".to_string()]),
    }
}

fn reclaim_at(at_ms: u64) -> GraphEntry {
    GraphEntry::Reclaim { at_ms }
}

fn create() -> GraphEntry {
    GraphEntry::CreateGraph { name: "g".into() }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

fn node_upserted(id: u64, key: &str, labels: &[&str], created: bool) -> GraphChange {
    GraphChange::NodeUpserted {
        id,
        key: key.into(),
        labels: strings(labels),
        properties: Metadata::new(),
        created,
    }
}

fn node_patched(id: u64, key: &str, add_labels: &[&str]) -> GraphChange {
    GraphChange::NodePatched {
        id,
        key: key.into(),
        add_labels: strings(add_labels),
        remove_labels: Vec::new(),
        set: Metadata::new(),
        unset: Vec::new(),
    }
}

fn edge_upserted(
    id: u64,
    key: Option<&str>,
    (source, source_key): (u64, &str),
    (destination, destination_key): (u64, &str),
) -> GraphChange {
    GraphChange::EdgeUpserted {
        id,
        key: key.map(str::to_string),
        source,
        source_key: source_key.into(),
        relationship: "R".into(),
        destination,
        destination_key: destination_key.into(),
        properties: Metadata::new(),
        created: true,
    }
}

fn edge_patched(id: u64, key: Option<&str>, unset: &[&str]) -> GraphChange {
    GraphChange::EdgePatched {
        id,
        key: key.map(str::to_string),
        set: Metadata::new(),
        unset: strings(unset),
    }
}

fn node_deleted(id: u64, key: &str) -> GraphChange {
    GraphChange::NodeDeleted {
        id,
        key: key.into(),
    }
}

fn edge_deleted(id: u64, key: Option<&str>) -> GraphChange {
    GraphChange::EdgeDeleted {
        id,
        key: key.map(str::to_string),
    }
}

fn attached(node: u64, node_key: &str, collection: &str, record: u64) -> GraphChange {
    GraphChange::Attached {
        node,
        node_key: node_key.into(),
        collection: collection.into(),
        record,
    }
}

fn detached(node: u64, node_key: &str, collection: &str, record: u64) -> GraphChange {
    GraphChange::Detached {
        node,
        node_key: node_key.into(),
        collection: collection.into(),
        record,
    }
}

fn updated(expires_at_ms: Option<u64>, expiry_changed: bool) -> Option<GraphTtl> {
    Some(GraphTtl::Updated {
        expires_at_ms,
        expiry_changed,
    })
}

fn removed(removal_reason: GraphRemovalReason, expires_at_ms: u64, at: u64) -> Option<GraphTtl> {
    Some(GraphTtl::ExpiryRemoval {
        removal_reason,
        expires_at_ms,
        removed_at_ms: at,
    })
}

const EXPLICIT: Option<GraphTtl> = Some(GraphTtl::ExplicitRemoval);
const EXPIRED: GraphRemovalReason = GraphRemovalReason::Expired;
const NODE_EXPIRED: GraphRemovalReason = GraphRemovalReason::NodeExpired;

/// One `GraphBatch` event of graph "g".
fn batch(
    sequence: u64,
    operations: Vec<(GraphChange, Option<GraphTtl>)>,
    reclaim: bool,
) -> TtlEvent {
    let (operations, ttl) = operations.into_iter().unzip();
    TtlEvent {
        collection: String::new(),
        kind: ChangeKind::GraphBatch {
            graph: "g".into(),
            revision: sequence,
            operations,
        },
        ttl,
        reclaim,
    }
}

fn lifecycle(kind: ChangeKind) -> TtlEvent {
    TtlEvent {
        collection: String::new(),
        kind,
        ttl: Vec::new(),
        reclaim: false,
    }
}

#[test]
fn ttl_operation_goldens() {
    let mut log = Fixture::new();
    assert_eq!(
        log.apply(vec![create()]),
        [lifecycle(ChangeKind::GraphCreated { graph: "g".into() })]
    );

    // New entities start without a deadline; a vector put keeps its own
    // (absent) expiry and splits the graph run at its position.
    let put = Mutation::Put {
        id: 7,
        vector: vec![1.0, 0.0],
        payload: Vec::new(),
    };
    assert_eq!(
        log.apply(vec![
            put_node(0, "a", &["L"]),
            put_node(1, "b", &[]),
            put_edge(0, 0, 1, Some("ab")),
            vector_entry(ZETA, &put),
            attach(0, ZETA, 7),
        ]),
        [
            batch(
                2,
                vec![
                    (node_upserted(0, "a", &["L"], true), updated(None, false)),
                    (node_upserted(1, "b", &[], true), updated(None, false)),
                    (
                        edge_upserted(0, Some("ab"), (0, "a"), (1, "b")),
                        updated(None, false)
                    ),
                ],
                false
            ),
            TtlEvent {
                collection: "zeta".into(),
                kind: ChangeKind::Put {
                    id: 7,
                    record: Record {
                        vector: vec![1.0, 0.0],
                        payload: Vec::new(),
                        metadata: Metadata::new(),
                    },
                    expires_at_ms: None,
                },
                ttl: Vec::new(),
                reclaim: false,
            },
            batch(2, vec![(attached(0, "a", "zeta", 7), None)], false),
        ]
    );

    // A logged deadline is an otherwise-empty patch; a replacement and
    // ordinary patches keep and report the own deadline; a clear is an
    // explicit null, not an omission.
    assert_eq!(
        log.apply(vec![
            node_expiry(0, Some(100)),
            put_node(0, "a", &["M"]),
            node_expiry(1, Some(20)),
            node_expiry(1, None),
            patch_node(1),
            edge_expiry(0, Some(300)),
            patch_edge(0),
            edge_expiry(0, None),
            patch_edge(0),
        ]),
        [batch(
            3,
            vec![
                (node_patched(0, "a", &[]), updated(Some(100), true)),
                (
                    node_upserted(0, "a", &["M"], false),
                    updated(Some(100), false)
                ),
                (node_patched(1, "b", &[]), updated(Some(20), true)),
                (node_patched(1, "b", &[]), updated(None, true)),
                (node_patched(1, "b", &["P"]), updated(None, false)),
                (edge_patched(0, Some("ab"), &[]), updated(Some(300), true)),
                (
                    edge_patched(0, Some("ab"), &["gone"]),
                    updated(Some(300), false)
                ),
                (edge_patched(0, Some("ab"), &[]), updated(None, true)),
                (edge_patched(0, Some("ab"), &["gone"]), updated(None, false)),
            ],
            false
        )]
    );

    // Set then immediate reclaim: the deadline patch comes first, then the
    // reclaim as its own run. The edge's cleared own deadline leaves b's.
    assert_eq!(
        log.apply(vec![node_expiry(1, Some(50)), reclaim_at(60)]),
        [
            batch(
                4,
                vec![(node_patched(1, "b", &[]), updated(Some(50), true))],
                false
            ),
            batch(
                4,
                vec![
                    (edge_deleted(0, Some("ab")), removed(NODE_EXPIRED, 50, 60)),
                    (node_deleted(1, "b"), removed(EXPIRED, 50, 60)),
                ],
                true
            ),
        ]
    );

    // Reclaiming an attached node detaches the record, which is neither
    // removed nor given the node's deadline. As in the engine's frames, the
    // reclaim opens a body that has a counted entry.
    assert_eq!(
        log.apply(vec![reclaim_at(100), put_node(2, "c", &[])]),
        [
            batch(
                5,
                vec![
                    (detached(0, "a", "zeta", 7), None),
                    (node_deleted(0, "a"), removed(EXPIRED, 100, 100)),
                ],
                true
            ),
            batch(
                5,
                vec![(node_upserted(2, "c", &[], true), updated(None, false))],
                false
            ),
        ]
    );

    // Explicit node deletion releases attachments without a detach event;
    // the record can then be attached again. Explicit deletes carry no
    // deadline or removal clock.
    assert_eq!(
        log.apply(vec![
            put_node(3, "d", &[]),
            attach(2, ZETA, 8),
            delete_node(2),
            attach(3, ZETA, 8),
            put_edge(1, 3, 3, None),
            delete_edge(1),
        ]),
        [batch(
            6,
            vec![
                (node_upserted(3, "d", &[], true), updated(None, false)),
                (attached(2, "c", "zeta", 8), None),
                (node_deleted(2, "c"), EXPLICIT),
                (attached(3, "d", "zeta", 8), None),
                (
                    edge_upserted(1, None, (3, "d"), (3, "d")),
                    updated(None, false)
                ),
                (edge_deleted(1, None), EXPLICIT),
            ],
            false
        )]
    );

    // A reclaim with nothing due emits nothing.
    assert_eq!(
        log.apply(vec![reclaim_at(u64::MAX), patch_node(3)]),
        [batch(
            7,
            vec![(node_patched(3, "d", &["P"]), updated(None, false))],
            false
        )]
    );
}

#[test]
fn ttl_reclaim_order() {
    let mut log = Fixture::new();
    log.apply(vec![create()]);
    // Deadlines in non-ID order; parallel edges, self-loops, an edge with
    // two due endpoints and a tie between own and endpoint deadline.
    log.apply(vec![
        put_node(0, "a", &[]),
        put_node(1, "b", &[]),
        put_node(2, "c", &[]),
        put_node(3, "d", &[]),
        put_node(4, "e", &[]),
        put_edge(0, 2, 3, Some("cd")),
        put_edge(1, 1, 1, None),
        put_edge(2, 0, 1, Some("ab1")),
        put_edge(3, 0, 1, Some("ab2")),
        put_edge(4, 2, 2, None),
        put_edge(5, 2, 4, None),
        put_edge(6, 3, 1, None),
        attach(3, ZETA, 5),
        attach(3, ALPHA, 6),
        attach(1, ZETA, 9),
        attach(0, ALPHA, 3),
        attach(2, ALPHA, 4),
        node_expiry(0, Some(250)),
        node_expiry(1, Some(100)),
        node_expiry(3, Some(200)),
        node_expiry(4, Some(270)),
        edge_expiry(1, Some(150)),
        edge_expiry(3, Some(100)),
        edge_expiry(4, Some(50)),
        edge_expiry(6, Some(90)),
    ]);
    assert_eq!(
        log.apply(vec![reclaim_at(260), patch_node(2)]),
        [
            batch(
                3,
                vec![
                    (edge_deleted(0, Some("cd")), removed(NODE_EXPIRED, 200, 260)),
                    (edge_deleted(1, None), removed(NODE_EXPIRED, 100, 260)),
                    (
                        edge_deleted(2, Some("ab1")),
                        removed(NODE_EXPIRED, 100, 260)
                    ),
                    (edge_deleted(3, Some("ab2")), removed(EXPIRED, 100, 260)),
                    (edge_deleted(4, None), removed(EXPIRED, 50, 260)),
                    (edge_deleted(6, None), removed(EXPIRED, 90, 260)),
                    (detached(0, "a", "alpha", 3), None),
                    (detached(1, "b", "zeta", 9), None),
                    (detached(3, "d", "alpha", 6), None),
                    (detached(3, "d", "zeta", 5), None),
                    (node_deleted(0, "a"), removed(EXPIRED, 250, 260)),
                    (node_deleted(1, "b"), removed(EXPIRED, 100, 260)),
                    (node_deleted(3, "d"), removed(EXPIRED, 200, 260)),
                ],
                true
            ),
            batch(
                3,
                vec![(node_patched(2, "c", &["P"]), updated(None, false))],
                false
            ),
        ]
    );
    // Repeating the clock finds nothing; a later clock takes node e.
    assert_eq!(
        log.apply(vec![reclaim_at(260), patch_node(2)]),
        [batch(
            4,
            vec![(node_patched(2, "c", &["P"]), updated(None, false))],
            false
        ),]
    );
    assert_eq!(
        log.apply(vec![reclaim_at(270), patch_node(2)]),
        [
            batch(
                5,
                vec![
                    (edge_deleted(5, None), removed(NODE_EXPIRED, 270, 270)),
                    (node_deleted(4, "e"), removed(EXPIRED, 270, 270)),
                ],
                true
            ),
            batch(
                5,
                vec![(node_patched(2, "c", &["P"]), updated(None, false))],
                false
            ),
        ]
    );
    // Two reclaims of one body stay distinct runs, between ordinary runs.
    assert_eq!(
        log.apply(vec![
            put_node(5, "f", &[]),
            put_node(6, "g", &[]),
            node_expiry(5, Some(280)),
            reclaim_at(280),
            node_expiry(6, Some(280)),
            reclaim_at(280),
        ]),
        [
            batch(
                6,
                vec![
                    (node_upserted(5, "f", &[], true), updated(None, false)),
                    (node_upserted(6, "g", &[], true), updated(None, false)),
                    (node_patched(5, "f", &[]), updated(Some(280), true)),
                ],
                false
            ),
            batch(
                6,
                vec![(node_deleted(5, "f"), removed(EXPIRED, 280, 280))],
                true
            ),
            batch(
                6,
                vec![(node_patched(6, "g", &[]), updated(Some(280), true))],
                false
            ),
            batch(
                6,
                vec![(node_deleted(6, "g"), removed(EXPIRED, 280, 280))],
                true
            ),
        ]
    );
    // Dropping and recreating the graph leaves no stale state: the new
    // node 0 has no old deadline or attachment, and record 4 is free.
    assert_eq!(
        log.apply(vec![delete_node(2), GraphEntry::DropGraph]),
        [
            batch(7, vec![(node_deleted(2, "c"), EXPLICIT)], false),
            lifecycle(ChangeKind::GraphDropped { graph: "g".into() }),
        ]
    );
    log.apply(vec![create(), put_node(0, "a", &[]), attach(0, ALPHA, 4)]);
    assert_eq!(
        log.apply(vec![reclaim_at(u64::MAX), patch_node(0)]),
        [batch(
            9,
            vec![(node_patched(0, "a", &["P"]), updated(None, false))],
            false
        )]
    );
    assert_eq!(
        log.apply(vec![node_expiry(0, Some(10)), reclaim_at(10)]),
        [
            batch(
                10,
                vec![(node_patched(0, "a", &[]), updated(Some(10), true))],
                false
            ),
            batch(
                10,
                vec![
                    (detached(0, "a", "alpha", 4), None),
                    (node_deleted(0, "a"), removed(EXPIRED, 10, 10)),
                ],
                true
            ),
        ]
    );
}

/// Fixed setup of graph "g": nodes a(0), b(1), edge ab(0), record 7 of
/// zeta attached to a, and node 3 created and deleted. Returns the error of
/// then applying `bad` to graph `graph` with `collections`.
fn corrupt_after(
    graph: u16,
    bad: Vec<GraphEntry>,
    collections: &BTreeMap<u16, (String, usize)>,
) -> Error {
    let mut log = Fixture::new();
    log.apply(vec![create()]);
    log.apply(vec![
        put_node(0, "a", &[]),
        put_node(1, "b", &[]),
        put_edge(0, 0, 1, Some("ab")),
        attach(0, ZETA, 7),
        put_node(3, "x", &[]),
        delete_node(3),
    ]);
    log.with(graph, bad, collections)
        .expect_err("corrupt frame")
}

#[test]
fn ttl_tracker_corrupt() {
    use reason::*;
    let all = collections();
    let high_water = |next_node_id, next_edge_id| GraphEntry::GraphHighWater {
        next_node_id,
        next_edge_id,
    };
    let cases: Vec<(u16, Vec<GraphEntry>, &str)> = vec![
        (1, vec![put_node(0, "a", &[])], GRAPH),
        (1, vec![reclaim_at(5), patch_node(0)], GRAPH),
        (0, vec![create()], GRAPH_EXISTS),
        (1, vec![create()], GRAPH_EXISTS),
        (0, vec![GraphEntry::DropGraph], GRAPH_NOT_EMPTY),
        (0, vec![high_water(3, 1)], HIGH_WATER),
        (0, vec![high_water(4, 0)], HIGH_WATER),
        (0, vec![patch_node(2)], NODE),
        (0, vec![node_expiry(3, Some(1))], NODE),
        (0, vec![delete_node(2)], NODE),
        (0, vec![put_edge(1, 0, 2, None)], NODE),
        (0, vec![attach(2, ZETA, 8)], NODE),
        (0, vec![detach(2, ZETA)], NODE),
        (0, vec![put_node(3, "y", &[])], NODE_ID),
        (0, vec![put_node(1, "c", &[])], NODE_ID),
        (0, vec![put_node(u64::MAX, "z", &[])], ID_SPACE),
        (0, vec![put_node(4, "b", &[])], NODE_KEY),
        (0, vec![delete_node(1)], NODE_EDGES),
        (0, vec![put_edge(0, 1, 0, None)], EDGE_ID),
        (0, vec![put_edge(1, 1, 0, Some("ab"))], EDGE_KEY),
        (0, vec![patch_edge(1)], EDGE),
        (0, vec![edge_expiry(1, Some(1))], EDGE),
        (0, vec![delete_edge(1)], EDGE),
        (0, vec![attach(0, ZETA, 8)], ATTACHED),
        (0, vec![attach(1, ZETA, 7)], ATTACHED),
        (0, vec![attach(1, 9, 8)], COLLECTION),
        (0, vec![detach(1, ZETA)], DETACHED),
        (0, vec![detach(0, ALPHA)], DETACHED),
    ];
    for (graph, bad, expected) in cases {
        let error = corrupt_after(graph, bad.clone(), &all);
        assert!(
            matches!(error, Error::Corrupt { offset: OFFSET, reason } if reason == expected),
            "{bad:?}: {error:?}"
        );
    }
    // A reclaim whose detach target's collection is unknown.
    let without_zeta: BTreeMap<_, _> = all.into_iter().filter(|(id, _)| *id != ZETA).collect();
    let error = corrupt_after(
        0,
        vec![node_expiry(0, Some(1)), reclaim_at(1)],
        &without_zeta,
    );
    assert!(
        matches!(error, Error::Corrupt { offset: OFFSET, reason } if reason == COLLECTION),
        "{error:?}"
    );
}

type ModelNode = (u64, String, Option<u64>);
type ModelEdge = (u64, Option<String>, u64, u64, Option<u64>);

/// The independent model: plain vectors in ID order, due tested by
/// comparing orderings, effective deadlines from a sorted candidate list.
#[derive(Default)]
struct Model {
    nodes: Vec<ModelNode>,
    edges: Vec<ModelEdge>,
    /// `(node, collection, record)`.
    attachments: Vec<(u64, u16, u64)>,
    next_node: u64,
    next_edge: u64,
    next_record: u64,
}

impl Model {
    fn node(&self, id: u64) -> &ModelNode {
        self.nodes.iter().find(|node| node.0 == id).unwrap()
    }

    fn collection(id: u16) -> &'static str {
        if id == ZETA { "zeta" } else { "alpha" }
    }

    fn reclaim(&mut self, at: u64) -> Vec<(GraphChange, Option<GraphTtl>)> {
        let due = |deadline: Option<u64>| {
            deadline.is_some_and(|deadline| deadline.cmp(&at) != Ordering::Greater)
        };
        let nodes: Vec<u64> = self
            .nodes
            .iter()
            .filter(|node| due(node.2))
            .map(|node| node.0)
            .collect();
        let mut out = Vec::new();
        let mut gone_edges = Vec::new();
        for (id, key, source, destination, own) in &self.edges {
            let cascades = nodes.contains(source) || nodes.contains(destination);
            if !(due(*own) || cascades) {
                continue;
            }
            let mut candidates: Vec<u64> = [*own, self.node(*source).2, self.node(*destination).2]
                .into_iter()
                .flatten()
                .collect();
            candidates.sort();
            let effective = candidates[0];
            let reason = if *own == Some(effective) {
                EXPIRED
            } else {
                NODE_EXPIRED
            };
            out.push((
                edge_deleted(*id, key.as_deref()),
                removed(reason, effective, at),
            ));
            gone_edges.push(*id);
        }
        let mut detaches: Vec<(u64, Vec<u8>, u64, u16)> = self
            .attachments
            .iter()
            .filter(|(node, _, _)| nodes.contains(node))
            .map(|(node, collection, record)| {
                (
                    *node,
                    Self::collection(*collection).as_bytes().to_vec(),
                    *record,
                    *collection,
                )
            })
            .collect();
        detaches.sort();
        for (node, _, record, collection) in &detaches {
            out.push((
                detached(
                    *node,
                    &self.node(*node).1,
                    Self::collection(*collection),
                    *record,
                ),
                None,
            ));
        }
        for id in &nodes {
            let node = self.node(*id);
            out.push((
                node_deleted(*id, &node.1),
                removed(EXPIRED, node.2.unwrap(), at),
            ));
        }
        self.edges.retain(|edge| !gone_edges.contains(&edge.0));
        self.attachments
            .retain(|(node, _, _)| !nodes.contains(node));
        self.nodes.retain(|node| !nodes.contains(&node.0));
        out
    }
}

/// Deterministic xorshift generator.
struct Rng(u64);

impl Rng {
    fn below(&mut self, bound: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % bound as u64) as usize
    }

    fn pick<T: Copy>(&mut self, values: &[T]) -> T {
        values[self.below(values.len())]
    }
}

const DEADLINES: [Option<u64>; 7] = [
    Some(0),
    Some(1),
    Some(5),
    Some(9),
    Some(10),
    Some(u64::MAX),
    None,
];
const CLOCKS: [u64; 6] = [0, 1, 5, 9, 10, u64::MAX];

type Expected = (GraphEntry, GraphChange, Option<GraphTtl>);

/// One ordinary operation the model allows now, with its expected output.
fn ordinary(model: &mut Model, rng: &mut Rng) -> Option<Expected> {
    match rng.below(10) {
        0 => {
            let id = model.next_node + rng.below(2) as u64;
            model.next_node = id + 1;
            // Keys come from a small pool, so removed keys are reused.
            let key = format!("n{}", rng.below(6));
            if model.nodes.iter().any(|node| node.1 == key) {
                return None;
            }
            model.nodes.push((id, key.clone(), None));
            Some((
                put_node(id, &key, &[]),
                node_upserted(id, &key, &[], true),
                updated(None, false),
            ))
        }
        1 if !model.nodes.is_empty() => {
            let (id, key, deadline) = model.nodes[rng.below(model.nodes.len())].clone();
            Some((
                put_node(id, &key, &["L"]),
                node_upserted(id, &key, &["L"], false),
                updated(deadline, false),
            ))
        }
        2 if !model.nodes.is_empty() => {
            let source = model.nodes[rng.below(model.nodes.len())].clone();
            let destination = model.nodes[rng.below(model.nodes.len())].clone();
            let key = (rng.below(2) == 0).then(|| format!("e{}", rng.below(4)));
            if key.is_some() && model.edges.iter().any(|edge| edge.1 == key) {
                return None;
            }
            let id = model.next_edge;
            model.next_edge += 1;
            model
                .edges
                .push((id, key.clone(), source.0, destination.0, None));
            Some((
                put_edge(id, source.0, destination.0, key.as_deref()),
                edge_upserted(
                    id,
                    key.as_deref(),
                    (source.0, &source.1),
                    (destination.0, &destination.1),
                ),
                updated(None, false),
            ))
        }
        3 if !model.nodes.is_empty() => {
            let at = rng.below(model.nodes.len());
            let deadline = rng.pick(&DEADLINES);
            model.nodes[at].2 = deadline;
            let (id, key, _) = &model.nodes[at];
            Some((
                node_expiry(*id, deadline),
                node_patched(*id, key, &[]),
                updated(deadline, true),
            ))
        }
        4 if !model.edges.is_empty() => {
            let at = rng.below(model.edges.len());
            let deadline = rng.pick(&DEADLINES);
            model.edges[at].4 = deadline;
            let (id, key, ..) = &model.edges[at];
            Some((
                edge_expiry(*id, deadline),
                edge_patched(*id, key.as_deref(), &[]),
                updated(deadline, true),
            ))
        }
        5 if !model.edges.is_empty() => {
            let (id, key, ..) = model.edges.remove(rng.below(model.edges.len()));
            Some((delete_edge(id), edge_deleted(id, key.as_deref()), EXPLICIT))
        }
        6 => {
            let free: Vec<u64> = model
                .nodes
                .iter()
                .map(|node| node.0)
                .filter(|id| {
                    !model
                        .edges
                        .iter()
                        .any(|edge| edge.2 == *id || edge.3 == *id)
                })
                .collect();
            if free.is_empty() {
                return None;
            }
            let id = rng.pick(&free);
            let key = model.node(id).1.clone();
            model.nodes.retain(|node| node.0 != id);
            model.attachments.retain(|attachment| attachment.0 != id);
            Some((delete_node(id), node_deleted(id, &key), EXPLICIT))
        }
        7 if !model.nodes.is_empty() => {
            let (id, key, _) = model.nodes[rng.below(model.nodes.len())].clone();
            let collection = rng.pick(&[ZETA, ALPHA]);
            if model
                .attachments
                .iter()
                .any(|attachment| attachment.0 == id && attachment.1 == collection)
            {
                return None;
            }
            let record = model.next_record;
            model.next_record += 1;
            model.attachments.push((id, collection, record));
            Some((
                attach(id, collection, record),
                attached(id, &key, Model::collection(collection), record),
                None,
            ))
        }
        8 if !model.attachments.is_empty() => {
            let (id, collection, record) =
                model.attachments.remove(rng.below(model.attachments.len()));
            let key = model.node(id).1.clone();
            Some((
                detach(id, collection),
                detached(id, &key, Model::collection(collection), record),
                None,
            ))
        }
        9 if !model.edges.is_empty() => {
            let (id, key, .., deadline) = model.edges[rng.below(model.edges.len())].clone();
            Some((
                patch_edge(id),
                edge_patched(id, key.as_deref(), &["gone"]),
                updated(deadline, false),
            ))
        }
        _ => None,
    }
}

#[test]
fn ttl_replay_model() {
    let mut reclaimed = 0;
    for history in 0..300u64 {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ (history + 1));
        let mut model = Model::default();
        let mut log = Fixture::new();
        log.apply(vec![create()]);
        for _ in 0..40 {
            let sequence = log.sequence + 1;
            let mut entries = Vec::new();
            let mut expected = Vec::new();
            let choice = rng.below(10);
            if choice <= 6 {
                let mut operations = Vec::new();
                for _ in 0..1 + rng.below(4) {
                    if let Some((entry, change, ttl)) = ordinary(&mut model, &mut rng) {
                        entries.push(entry);
                        operations.push((change, ttl));
                    }
                }
                if entries.is_empty() {
                    continue;
                }
                expected.push(batch(sequence, operations, false));
            } else {
                let at = rng.pick(&CLOCKS);
                // A deadline due at the clock, set right before it.
                if choice == 9 && !model.nodes.is_empty() {
                    let index = rng.below(model.nodes.len());
                    let deadline = rng.pick(&[0, at / 2, at]);
                    model.nodes[index].2 = Some(deadline);
                    let (id, key, _) = model.nodes[index].clone();
                    entries.push(node_expiry(id, Some(deadline)));
                    expected.push(batch(
                        sequence,
                        vec![(node_patched(id, &key, &[]), updated(Some(deadline), true))],
                        false,
                    ));
                }
                entries.push(reclaim_at(at));
                let gone = model.reclaim(at);
                if !gone.is_empty() {
                    reclaimed += gone.len();
                    expected.push(batch(sequence, gone, true));
                }
                // Logged bodies always have a counted entry after reclaims.
                let (entry, change, ttl) = loop {
                    if let Some(next) = ordinary(&mut model, &mut rng) {
                        break next;
                    }
                };
                entries.push(entry);
                expected.push(batch(sequence, vec![(change, ttl)], false));
            }
            assert_eq!(
                log.apply(entries),
                expected,
                "history {history}, {sequence}"
            );
        }
    }
    // The histories really exercise reclaims.
    assert!(reclaimed > 1000, "{reclaimed}");
}
