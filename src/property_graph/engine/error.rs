//! Error type shared by the codec, the in-memory state and reads.

use super::model::{EdgeId, GraphId, NodeId};
use std::fmt;

/// Errors from encoding, decoding, validating or applying graph entries.
///
/// When a committed graph frame fails to decode or apply during replay, the
/// adapter must report the existing corruption error regardless of variant.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum GraphError {
    /// The bytes violate the DO-003 body or entry layout.
    Corrupt(&'static str),
    /// An entry tag outside the closed set.
    UnknownEntryTag(u8),
    /// A batch has no entries, more than `MAX_BATCH_ENTRIES` entries or more
    /// than `MAX_BATCH_BYTES` encoded bytes.
    BatchSize(&'static str),
    InvalidGraphName,
    InvalidNodeKey,
    InvalidEdgeKey,
    InvalidLabel,
    TooManyLabels,
    InvalidRelationshipType,
    InvalidProperties(&'static str),
    /// A vector-mutation entry is not one structurally valid batch entry.
    InvalidVectorMutation(&'static str),
    GraphNameExists(String),
    /// The graph ID is at or above `MAX_GRAPHS` or belongs to a live graph.
    GraphIdUnavailable(GraphId),
    GraphNotFound(GraphId),
    GraphNotEmpty(GraphId),
    /// A new node ID is below the graph's high-water mark (IDs are never reused).
    NodeIdUnavailable(NodeId),
    /// A new edge ID is below the graph's high-water mark (IDs are never reused).
    EdgeIdUnavailable(EdgeId),
    /// `u64::MAX` cannot be allocated because the high-water mark could not advance.
    IdSpaceExhausted,
    /// A high-water entry would lower `next_node_id` or `next_edge_id`.
    HighWaterDecrease,
    NodeKeyExists(String),
    /// `PutNode` names an existing node ID with a different key.
    NodeKeyMismatch(NodeId),
    EdgeKeyExists(String),
    NodeNotFound(NodeId),
    EdgeNotFound(EdgeId),
    /// REQ-012: a node with incident edges (self-loops included) cannot be deleted.
    NodeHasEdges {
        node: NodeId,
        incident: usize,
    },
    /// REQ-013: at most one attachment per (node, collection).
    NodeAlreadyAttached {
        node: NodeId,
        collection: u16,
    },
    /// PD-012: a record is attached to at most one node across the file.
    RecordAlreadyAttached {
        collection: u16,
        record: u64,
        graph: GraphId,
        node: NodeId,
    },
    AttachmentNotFound {
        node: NodeId,
        collection: u16,
    },
    /// Frames must be applied in strictly increasing sequence order.
    SequenceNotIncreasing {
        last: u64,
        sequence: u64,
    },
    /// `GraphState::check_invariants` found inconsistent state.
    InvariantViolation(&'static str),
    /// A key-addressed mutation's `Precondition` failed; `current` is the
    /// node's or edge's current revision.
    PreconditionFailed {
        current: Option<u64>,
    },
    /// REQ-009: an edge key exists with a different source, relationship
    /// type, destination or properties.
    EdgeKeyConflict(String),
    /// No live node has this key.
    UnknownNodeKey(String),
    /// No live edge has this key.
    UnknownEdgeKey(String),
    /// REQ-024: a read limit is above its ceiling; nothing was executed.
    LimitExceedsCeiling {
        limit: &'static str,
        requested: u64,
        ceiling: u64,
    },
    /// REQ-027: the graph changed after the cursor was issued.
    CursorStale,
    /// The cursor is malformed or belongs to another graph or query.
    InvalidCursor(&'static str),
    /// A read query is structurally invalid (for example an empty type set
    /// or a zero page size).
    InvalidQuery(&'static str),
    /// A declared schema breaks a structural rule (names, sizes, index
    /// kinds); nothing depends on graph data.
    InvalidSchema(&'static str),
    /// REQ-030, FM-013: a write breaks a declared rule. `rule` is one of
    /// `type`, `required`, `unique`, `endpoints` or `unique_triple`;
    /// `element` is the label or relationship type and `property` the
    /// property, if the rule concerns one.
    SchemaViolation {
        rule: &'static str,
        element: String,
        property: Option<String>,
    },
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Corrupt(reason) => write!(f, "corrupt graph body: {reason}"),
            Self::UnknownEntryTag(tag) => write!(f, "unknown graph entry tag {tag}"),
            Self::BatchSize(reason) => write!(f, "graph batch size: {reason}"),
            Self::InvalidGraphName => {
                write!(f, "graph names must match ^[a-z][a-z0-9_-]{{0,62}}$")
            }
            Self::InvalidNodeKey => write!(f, "node keys must be 1..=256 UTF-8 bytes"),
            Self::InvalidEdgeKey => write!(f, "edge keys must be 1..=256 UTF-8 bytes"),
            Self::InvalidLabel => write!(f, "labels must match ^[A-Za-z0-9_]{{1,63}}$"),
            Self::TooManyLabels => write!(f, "a node has at most 8 labels"),
            Self::InvalidRelationshipType => {
                write!(f, "relationship types must match ^[A-Za-z0-9_]{{1,63}}$")
            }
            Self::InvalidProperties(reason) => write!(f, "invalid properties: {reason}"),
            Self::InvalidVectorMutation(reason) => {
                write!(f, "invalid vector mutation entry: {reason}")
            }
            Self::GraphNameExists(name) => write!(f, "graph {name:?} already exists"),
            Self::GraphIdUnavailable(id) => write!(f, "graph id {} is unavailable", id.0),
            Self::GraphNotFound(id) => write!(f, "graph id {} not found", id.0),
            Self::GraphNotEmpty(id) => write!(f, "graph id {} is not empty", id.0),
            Self::NodeIdUnavailable(id) => write!(f, "node id {} is unavailable", id.0),
            Self::EdgeIdUnavailable(id) => write!(f, "edge id {} is unavailable", id.0),
            Self::IdSpaceExhausted => write!(f, "graph id space exhausted"),
            Self::HighWaterDecrease => write!(f, "graph high-water marks never decrease"),
            Self::NodeKeyExists(key) => write!(f, "node key {key:?} already exists"),
            Self::NodeKeyMismatch(id) => write!(f, "node id {} has a different key", id.0),
            Self::EdgeKeyExists(key) => write!(f, "edge key {key:?} already exists"),
            Self::NodeNotFound(id) => write!(f, "node id {} not found", id.0),
            Self::EdgeNotFound(id) => write!(f, "edge id {} not found", id.0),
            Self::NodeHasEdges { node, incident } => {
                write!(f, "node id {} has {incident} incident edges", node.0)
            }
            Self::NodeAlreadyAttached { node, collection } => write!(
                f,
                "node id {} already has an attachment in collection {collection}",
                node.0
            ),
            Self::RecordAlreadyAttached {
                collection,
                record,
                graph,
                node,
            } => write!(
                f,
                "record {record} in collection {collection} is attached to node id {} in graph id {}",
                node.0, graph.0
            ),
            Self::AttachmentNotFound { node, collection } => write!(
                f,
                "node id {} has no attachment in collection {collection}",
                node.0
            ),
            Self::SequenceNotIncreasing { last, sequence } => {
                write!(f, "graph frame sequence {sequence} does not follow {last}")
            }
            Self::InvariantViolation(reason) => write!(f, "graph invariant violated: {reason}"),
            Self::PreconditionFailed { current } => {
                write!(f, "graph precondition failed; current revision {current:?}")
            }
            Self::EdgeKeyConflict(key) => {
                write!(f, "edge key {key:?} exists with different content")
            }
            Self::UnknownNodeKey(key) => write!(f, "node key {key:?} not found"),
            Self::UnknownEdgeKey(key) => write!(f, "edge key {key:?} not found"),
            Self::LimitExceedsCeiling {
                limit,
                requested,
                ceiling,
            } => write!(f, "{limit} = {requested} exceeds its ceiling {ceiling}"),
            Self::CursorStale => write!(f, "the graph changed after the cursor was issued"),
            Self::InvalidCursor(reason) => write!(f, "invalid graph cursor: {reason}"),
            Self::InvalidQuery(reason) => write!(f, "invalid graph query: {reason}"),
            Self::InvalidSchema(reason) => write!(f, "invalid graph schema: {reason}"),
            Self::SchemaViolation {
                rule,
                element,
                property: Some(property),
            } => write!(
                f,
                "graph schema rule {rule} violated by {element}.{property}"
            ),
            Self::SchemaViolation {
                rule,
                element,
                property: None,
            } => write!(f, "graph schema rule {rule} violated by {element}"),
        }
    }
}

impl std::error::Error for GraphError {}
