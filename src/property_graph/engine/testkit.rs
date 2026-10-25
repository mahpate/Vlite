//! Test support for the read tests: graphs committed through the planner
//! with every body kept for replay, seeded random graphs, a large hub, and
//! naive oracles that never use the derived indexes.

use super::codec::{GraphBody, GraphEntry, decode_body, encode_body};
use super::error::GraphError;
use super::limits::{GraphLimits, MAX_BATCH_ENTRIES, MAX_DEGREE_LIMIT_NODE_IDS};
use super::meter::ExecutionContext;
use super::model::{
    Direction, EdgeId, GraphEdge, GraphId, NodeId, Precondition, Properties, PropertyValue,
};
use super::mutate::{EdgeSelector, GraphMutation, MutationOutcome};
use super::query::{GraphQuery, QueryOutput, QueryResult, TruncationReason, execute};
use super::state::{Graph, GraphState};
use std::collections::{BTreeSet, HashSet};

pub(super) const G: GraphId = GraphId(0);
/// The view sequence every [`read`] passes; results must echo it.
pub(super) const VIEW: u64 = 7;
pub(super) const TYPES: [&str; 3] = ["R", "S", "T"];

pub(super) fn set(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

pub(super) fn props(pairs: &[(&str, PropertyValue)]) -> Properties {
    pairs
        .iter()
        .map(|(field, value)| ((*field).to_owned(), value.clone()))
        .collect()
}

pub(super) fn int(value: i64) -> PropertyValue {
    PropertyValue::Integer(value)
}

pub(super) fn node(key: &str, labels: &[&str], properties: Properties) -> GraphMutation {
    GraphMutation::CreateNode {
        key: key.into(),
        labels: set(labels),
        properties,
    }
}

pub(super) fn edge(
    key: Option<&str>,
    source: &str,
    relationship: &str,
    destination: &str,
    properties: Properties,
) -> GraphMutation {
    GraphMutation::CreateEdge {
        key: key.map(str::to_owned),
        source: source.into(),
        relationship: relationship.into(),
        destination: destination.into(),
        properties,
    }
}

/// One graph `g` (ID 0) built by committed bodies, like a database would.
#[derive(Clone)]
pub(super) struct TestGraph {
    pub(super) state: GraphState,
    bodies: Vec<(u64, GraphBody)>,
    sequence: u64,
}

impl TestGraph {
    pub(super) fn new() -> Self {
        let mut db = Self {
            state: GraphState::new(),
            bodies: Vec::new(),
            sequence: 0,
        };
        db.commit(GraphBody {
            graph_id: G,
            entries: vec![GraphEntry::CreateGraph { name: "g".into() }],
        });
        db
    }

    /// Applies `body` at the next sequence and keeps it for replay.
    pub(super) fn commit(&mut self, body: GraphBody) {
        self.sequence += 1;
        self.state.apply(self.sequence, &body).unwrap();
        self.bodies.push((self.sequence, body));
    }

    /// Plans `mutations` as one batch and commits it.
    pub(super) fn run(&mut self, mutations: &[GraphMutation]) -> Vec<MutationOutcome> {
        let mut planner = self.state.planner(self.sequence + 1, G).unwrap();
        let outcomes = mutations
            .iter()
            .map(|mutation| planner.mutate(mutation).unwrap())
            .collect();
        let body = planner.finish();
        self.commit(body);
        outcomes
    }

    /// Commits a vector write of `(collection, record)` without graph entries.
    pub(super) fn touch(&mut self, collection: u16, record: u64) {
        self.sequence += 1;
        self.state.touch_record(collection, record, self.sequence);
    }

    pub(super) fn graph(&self) -> &Graph {
        self.state.graph(G).unwrap()
    }

    pub(super) fn id(&self, key: &str) -> NodeId {
        self.graph().node_by_key(key).unwrap().id
    }

    /// The state rebuilt by encoding, decoding and replaying every body.
    pub(super) fn replayed(&self) -> GraphState {
        let mut state = GraphState::new();
        for (sequence, body) in &self.bodies {
            let bytes = encode_body(body).unwrap();
            let entries = u32::try_from(body.entries.len()).unwrap();
            state
                .apply(*sequence, &decode_body(&bytes, entries).unwrap())
                .unwrap();
        }
        state
    }
}

/// Runs `query` with a clock fixed at 0 and checks the invariants every
/// result must hold.
pub(super) fn read(
    graph: &Graph,
    query: &GraphQuery,
    limits: &GraphLimits,
) -> Result<QueryResult, GraphError> {
    let clock = || 0;
    let context = ExecutionContext::new(VIEW, &clock);
    let result = execute(graph, query, limits, &context);
    if let Ok(result) = &result {
        check(result, VIEW);
    }
    result
}

fn rank(reason: &TruncationReason) -> u8 {
    match reason {
        TruncationReason::VisitedLimit => 0,
        TruncationReason::EdgeLimit => 1,
        TruncationReason::DegreeLimit { .. } => 2,
        TruncationReason::OutputLimit => 3,
        TruncationReason::Deadline => 4,
        TruncationReason::Cancelled => 5,
    }
}

/// INV-010 and the result-shape rules.
pub(super) fn check(result: &QueryResult, view: u64) {
    assert_eq!(result.view_sequence, view);
    assert_eq!(result.complete, result.truncation.is_empty(), "{result:?}");
    assert!(
        result
            .truncation
            .windows(2)
            .all(|pair| rank(&pair[0]) < rank(&pair[1])),
        "reasons are unique and ordered: {:?}",
        result.truncation
    );
    for reason in &result.truncation {
        if let TruncationReason::DegreeLimit { node_ids } = reason {
            assert!(!node_ids.is_empty() && node_ids.len() <= MAX_DEGREE_LIMIT_NODE_IDS);
            let unique: HashSet<_> = node_ids.iter().collect();
            assert_eq!(unique.len(), node_ids.len());
        }
    }
    if result.cursor.is_some() {
        assert!(!result.complete);
        assert!(matches!(
            result.output,
            QueryOutput::Nodes(_) | QueryOutput::Neighbors { .. }
        ));
    }
    let stats = &result.stats;
    match &result.output {
        QueryOutput::Nodes(nodes) => {
            assert_eq!(stats.nodes_returned, nodes.len() as u64);
            assert!(stats.nodes_returned <= stats.nodes_scanned);
        }
        QueryOutput::Neighbors { edges, nodes } => {
            assert_eq!(stats.edges_returned, edges.len() as u64);
            assert_eq!(stats.nodes_returned, nodes.len() as u64);
            assert!(stats.edges_returned <= stats.edges_examined);
        }
        QueryOutput::Traversal {
            nodes,
            edges,
            paths,
        } => {
            assert_eq!(stats.nodes_returned, nodes.len() as u64);
            assert_eq!(stats.edges_returned, edges.len() as u64);
            assert_eq!(stats.paths_returned, paths.len() as u64);
            assert!(stats.nodes_returned <= stats.nodes_visited);
            assert!(stats.edges_returned <= stats.edges_examined);
        }
        _ => {}
    }
}

/// Xorshift64; deterministic for a seed.
pub(super) struct Rng(u64);

impl Rng {
    pub(super) fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub(super) fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub(super) fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    pub(super) fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    pub(super) fn pick<T: Copy>(&mut self, values: &[T]) -> T {
        values[self.below(values.len() as u64) as usize]
    }
}

/// An integer `w` in 0..5 with the given probability.
fn weight(rng: &mut Rng, percent: u64) -> Properties {
    let mut properties = Properties::new();
    if rng.chance(percent) {
        properties.insert("w".into(), int(rng.below(5) as i64));
    }
    properties
}

/// A random graph of 2..=25 nodes labelled from {A, B} and 0..=60 edges of
/// types R, S and T, both with an integer property `w` in 0..5 most of the
/// time. Self-loops and parallel edges occur; some edges and isolated nodes
/// are deleted again, so IDs have gaps.
pub(super) fn random_graph(seed: u64) -> TestGraph {
    let mut rng = Rng::new(seed);
    let mut db = TestGraph::new();
    let count = 2 + rng.below(24);
    let nodes: Vec<GraphMutation> = (0..count)
        .map(|index| {
            let mut labels = Vec::new();
            if rng.chance(50) {
                labels.push("A");
            }
            if rng.chance(30) {
                labels.push("B");
            }
            let properties = weight(&mut rng, 80);
            node(&format!("n{index}"), &labels, properties)
        })
        .collect();
    db.run(&nodes);
    let edges: Vec<GraphMutation> = (0..rng.below(61))
        .map(|_| {
            let source = rng.below(count);
            let destination = if rng.chance(10) {
                source
            } else {
                rng.below(count)
            };
            let kind = rng.pick(&TYPES);
            let properties = weight(&mut rng, 70);
            edge(
                None,
                &format!("n{source}"),
                kind,
                &format!("n{destination}"),
                properties,
            )
        })
        .collect();
    if !edges.is_empty() {
        db.run(&edges);
    }
    let deleted: Vec<GraphMutation> = db
        .graph()
        .edges()
        .map(|edge| edge.id)
        .collect::<Vec<_>>()
        .into_iter()
        .filter(|_| rng.chance(10))
        .map(|id| GraphMutation::DeleteEdge {
            edge: EdgeSelector::Id(id),
            expected: Precondition::Any,
        })
        .collect();
    if !deleted.is_empty() {
        db.run(&deleted);
    }
    let graph = db.graph();
    let isolated: Vec<String> = graph
        .nodes()
        .filter(|node| graph.edges_of(node.id, Direction::Both).is_empty())
        .map(|node| node.key.clone())
        .collect();
    let deleted: Vec<GraphMutation> = isolated
        .into_iter()
        .filter(|_| rng.chance(30))
        .map(|key| GraphMutation::DeleteNode {
            key,
            expected: Precondition::Any,
        })
        .collect();
    if !deleted.is_empty() {
        db.run(&deleted);
    }
    db
}

/// Node `hub` (ID 0) with `edges` outgoing `R` edges spread round-robin
/// over the leaves `leaf1..=leafN` (IDs 1..=N), committed as raw bodies of
/// at most 4096 entries.
pub(super) fn hub_graph(leaves: u64, edges: u64) -> TestGraph {
    let mut db = TestGraph::new();
    let nodes: Vec<GraphEntry> = (0..=leaves)
        .map(|id| GraphEntry::PutNode {
            node_id: NodeId(id),
            key: if id == 0 {
                "hub".into()
            } else {
                format!("leaf{id}")
            },
            labels: BTreeSet::new(),
            properties: Properties::new(),
        })
        .collect();
    let edges: Vec<GraphEntry> = (0..edges)
        .map(|id| GraphEntry::PutEdge {
            edge_id: EdgeId(id),
            source: NodeId(0),
            destination: NodeId(1 + id % leaves),
            relationship: "R".into(),
            key: None,
            properties: Properties::new(),
        })
        .collect();
    for entries in [nodes, edges] {
        for chunk in entries.chunks(MAX_BATCH_ENTRIES) {
            db.commit(GraphBody {
                graph_id: G,
                entries: chunk.to_vec(),
            });
        }
    }
    db
}

/// The node reached from `node` over `edge` (`node` itself for a self-loop).
pub(super) fn far_end(edge: &GraphEdge, node: NodeId) -> NodeId {
    if edge.source == node {
        edge.destination
    } else {
        edge.source
    }
}

/// `node`'s adjacency from a scan of every edge, ordered by (relationship
/// type, edge ID); a self-loop appears once.
pub(super) fn naive_adjacency(
    graph: &Graph,
    node: NodeId,
    direction: Direction,
    types: Option<&BTreeSet<String>>,
) -> Vec<(String, EdgeId)> {
    let mut entries: Vec<(String, EdgeId)> = graph
        .edges()
        .filter(|edge| match direction {
            Direction::Out => edge.source == node,
            Direction::In => edge.destination == node,
            Direction::Both => edge.source == node || edge.destination == node,
        })
        .filter(|edge| types.is_none_or(|types| types.contains(&edge.relationship)))
        .map(|edge| (edge.relationship.clone(), edge.id))
        .collect();
    entries.sort();
    entries
}
