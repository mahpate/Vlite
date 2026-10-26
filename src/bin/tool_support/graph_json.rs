//! Typed graph JSON shared by the opt-in graph tools (`vlite-graph-cli`
//! and the `graph` tools of `vlite-mcp`): request parsing (every object
//! rejects unknown fields) and output encoding. Node, edge and record IDs,
//! revisions and sequences are decimal strings on output and accepted as
//! strings or JSON numbers on input, so u64 values never lose precision.

use std::collections::BTreeMap;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value, json};
use vlite::hybrid::{FusionOptions, VectorLeg};
use vlite::property_graph::{
    Direction, EdgeRef, EdgeView, EvidenceSpec, Expansion, Explanation, FindNodes,
    GraphBatchResult, GraphInfo, GraphLimits, GraphOp, GraphOpOutcome, GraphOutput, GraphPath,
    GraphQuery, GraphQueryErrorKind, GraphRead, GraphReadStats, GraphSchema, GraphTextError,
    GraphWrite, IndexChoice, LabelDefinition, Multiplicity, Neighbors, NodeFilter, NodePatch,
    NodeSelector, NodeView, ObservedProperty, ObservedSchema, PathOutcome, Pivot,
    PropertyDefinition, PropertyIndexKind, PropertyProjection, PropertyType,
    RelationshipDefinition, Retrieval, Retrieve, SchemaOrigin, ShortestPath, Traverse,
    TruncationReason,
};
use vlite::{Condition, DistanceMetric, Error, Expected, Metadata, MetadataValue, Mutation};

/// Stable codes: graph variants use the REST/gRPC names (`server::graph_code`).
pub(crate) fn error_code(error: &Error) -> &'static str {
    match error {
        Error::Io(_) => "io_error",
        Error::Locked => "locked",
        Error::ReadOnly => "read_only",
        Error::RevisionMismatch { .. } => "precondition_failed",
        Error::CollectionNotFound => "collection_not_found",
        Error::InvalidDimensions(_)
        | Error::DimensionMismatch { .. }
        | Error::NonFiniteVector
        | Error::PayloadTooLarge(_)
        | Error::BatchTooLarge
        | Error::InvalidMetadata(_)
        | Error::InvalidFilter(_)
        | Error::InvalidQuery(_)
        | Error::InvalidCollectionName => "invalid_input",
        Error::Corrupt { .. } => "corrupt",
        Error::IncompleteTail { .. } => "incomplete_tail",
        Error::UnsupportedVersion(_) => "unsupported_version",
        Error::UnsupportedFeature(_) => "unsupported_feature",
        Error::EncryptionKeyRequired => "encryption_key_required",
        Error::CommitIndeterminate(_) => "commit_indeterminate",
        Error::Poisoned => "poisoned",
        Error::GraphsNotEnabled => "graphs_not_enabled",
        Error::GraphsRequireV5 => "graphs_require_v5",
        Error::InvalidGraphName => "invalid_graph_name",
        Error::GraphExists => "graph_exists",
        Error::GraphNotFound => "graph_not_found",
        Error::GraphNotEmpty => "graph_not_empty",
        Error::TooManyGraphs => "too_many_graphs",
        Error::GraphKeyExists => "graph_key_exists",
        Error::GraphKeyConflict => "graph_key_conflict",
        Error::GraphNodeNotFound => "graph_node_not_found",
        Error::GraphEdgeNotFound => "graph_edge_not_found",
        Error::GraphNodeHasEdges { .. } => "graph_node_has_edges",
        Error::GraphAttachmentConflict => "graph_attachment_conflict",
        Error::GraphAttachmentNotFound => "graph_attachment_not_found",
        Error::GraphRecordNotAttachable => "graph_record_not_attachable",
        Error::GraphAttachedRecord => "graph_attached_record",
        Error::GraphAttachedRecordExpiry => "graph_attached_record_expiry",
        Error::GraphLimitExceedsCeiling { .. } => "graph_limit_exceeds_ceiling",
        Error::GraphCursorStale => "graph_cursor_stale",
        Error::GraphChangeFeedUnsupported => "graph_change_feed_unsupported",
        Error::GraphUnsupportedOperation => "graph_unsupported_operation",
        Error::GraphInvalidOptions(_) => "graph_invalid_options",
        Error::GraphSchemaViolation { .. } => "graph_schema_violation",
        Error::GraphPivotTooExpensive => "graph_pivot_too_expensive",
        _ => "database_error",
    }
}

// Request JSON. Every object rejects unknown fields.

/// Parses one request object; the message is serde's.
pub(crate) fn parse<T: for<'de> Deserialize<'de>>(text: &str) -> Result<T, String> {
    serde_json::from_str(text).map_err(|error| error.to_string())
}

/// Canonical decimal string or non-negative JSON integer (as `vlite-mcp`).
pub(crate) fn parse_u64(value: &Value) -> Result<u64, String> {
    if let Some(text) = value.as_str() {
        if text.is_empty()
            || (text.len() > 1 && text.starts_with('0'))
            || !text.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err("IDs must be canonical decimal u64 strings".into());
        }
        text.parse().map_err(|_| "ID exceeds u64 range".into())
    } else {
        value
            .as_u64()
            .ok_or_else(|| "IDs must be a non-negative u64 integer or decimal string".into())
    }
}

pub(crate) fn de_u64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    parse_u64(&Value::deserialize(deserializer)?).map_err(D::Error::custom)
}

/// `Metadata` that rejects a repeated key instead of keeping the last value.
pub(crate) struct Properties(Metadata);

impl<'de> Deserialize<'de> for Properties {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Unique;
        impl<'de> serde::de::Visitor<'de> for Unique {
            type Value = Metadata;
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("an object of typed values")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Metadata, A::Error> {
                let mut out = Metadata::new();
                while let Some((key, value)) = map.next_entry::<String, MetadataValue>()? {
                    if out.contains_key(&key) {
                        return Err(serde::de::Error::custom(format!("duplicate key {key:?}")));
                    }
                    out.insert(key, value);
                }
                Ok(out)
            }
        }
        deserializer.deserialize_map(Unique).map(Properties)
    }
}

pub(crate) fn de_properties<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Metadata, D::Error> {
    Properties::deserialize(deserializer).map(|properties| properties.0)
}

pub(crate) fn de_optional_properties<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Metadata>, D::Error> {
    Option::<Properties>::deserialize(deserializer)
        .map(|found| found.map(|properties| properties.0))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Empty {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Named {
    pub(crate) graph: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Batch {
    pub(crate) graph: String,
    pub(crate) ops: Vec<OpIn>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExpectedIn {
    #[default]
    Any,
    Absent,
    Revision(#[serde(deserialize_with = "de_u64")] u64),
}

impl From<ExpectedIn> for Expected {
    fn from(expected: ExpectedIn) -> Self {
        match expected {
            ExpectedIn::Any => Expected::Any,
            ExpectedIn::Absent => Expected::Absent,
            ExpectedIn::Revision(revision) => Expected::Revision(revision),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum IdOrKey {
    Id(#[serde(deserialize_with = "de_u64")] u64),
    Key(String),
}

impl From<IdOrKey> for NodeSelector {
    fn from(selector: IdOrKey) -> Self {
        match selector {
            IdOrKey::Id(id) => NodeSelector::Id(id),
            IdOrKey::Key(key) => NodeSelector::Key(key),
        }
    }
}

impl From<IdOrKey> for EdgeRef {
    fn from(selector: IdOrKey) -> Self {
        match selector {
            IdOrKey::Id(id) => EdgeRef::Id(id),
            IdOrKey::Key(key) => EdgeRef::Key(key),
        }
    }
}

/// One `GraphOp`; `put_vector`/`delete_vector` are `GraphOp::Vector`.
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum OpIn {
    CreateNode {
        key: String,
        #[serde(default)]
        labels: Vec<String>,
        #[serde(default, deserialize_with = "de_properties")]
        properties: Metadata,
    },
    UpsertNode {
        key: String,
        #[serde(default)]
        labels: Vec<String>,
        #[serde(default, deserialize_with = "de_properties")]
        properties: Metadata,
        #[serde(default)]
        expected: ExpectedIn,
    },
    PatchNode {
        key: String,
        #[serde(default)]
        add_labels: Vec<String>,
        #[serde(default)]
        remove_labels: Vec<String>,
        #[serde(default, deserialize_with = "de_properties")]
        set: Metadata,
        #[serde(default)]
        unset: Vec<String>,
        #[serde(default)]
        expected: ExpectedIn,
    },
    DeleteNode {
        key: String,
        #[serde(default)]
        expected: ExpectedIn,
    },
    CreateEdge {
        #[serde(default)]
        key: Option<String>,
        source: String,
        relationship: String,
        destination: String,
        #[serde(default, deserialize_with = "de_properties")]
        properties: Metadata,
    },
    PatchEdge {
        edge: IdOrKey,
        #[serde(default, deserialize_with = "de_properties")]
        set: Metadata,
        #[serde(default)]
        unset: Vec<String>,
        #[serde(default)]
        expected: ExpectedIn,
    },
    DeleteEdge {
        edge: IdOrKey,
        #[serde(default)]
        expected: ExpectedIn,
    },
    Attach {
        node: String,
        collection: String,
        #[serde(deserialize_with = "de_u64")]
        record: u64,
    },
    Detach {
        node: String,
        collection: String,
    },
    /// `Mutation::PutWithMetadata` when `metadata` is present, else `Put`.
    PutVector {
        collection: String,
        #[serde(deserialize_with = "de_u64")]
        id: u64,
        vector: Vec<f32>,
        /// UTF-8 text.
        #[serde(default)]
        payload: String,
        #[serde(default, deserialize_with = "de_optional_properties")]
        metadata: Option<Metadata>,
    },
    DeleteVector {
        collection: String,
        #[serde(deserialize_with = "de_u64")]
        id: u64,
    },
    /// `GraphOp::EnsureEdge`: the type must be declared `unique_triple`.
    EnsureEdge {
        source: String,
        relationship: String,
        destination: String,
        #[serde(default, deserialize_with = "de_properties")]
        properties: Metadata,
    },
    /// `GraphOp::PutSchema`: replaces the declared schema.
    PutSchema {
        schema: SchemaIn,
    },
}

impl From<OpIn> for GraphOp {
    fn from(op: OpIn) -> Self {
        match op {
            OpIn::CreateNode {
                key,
                labels,
                properties,
            } => GraphOp::CreateNode {
                key,
                labels,
                properties,
            },
            OpIn::UpsertNode {
                key,
                labels,
                properties,
                expected,
            } => GraphOp::UpsertNode {
                key,
                labels,
                properties,
                expected: expected.into(),
            },
            OpIn::PatchNode {
                key,
                add_labels,
                remove_labels,
                set,
                unset,
                expected,
            } => GraphOp::PatchNode {
                key,
                patch: NodePatch {
                    add_labels,
                    remove_labels,
                    set,
                    unset,
                },
                expected: expected.into(),
            },
            OpIn::DeleteNode { key, expected } => GraphOp::DeleteNode {
                key,
                expected: expected.into(),
            },
            OpIn::CreateEdge {
                key,
                source,
                relationship,
                destination,
                properties,
            } => GraphOp::CreateEdge {
                key,
                source,
                relationship,
                destination,
                properties,
            },
            OpIn::PatchEdge {
                edge,
                set,
                unset,
                expected,
            } => GraphOp::PatchEdge {
                edge: edge.into(),
                set,
                unset,
                expected: expected.into(),
            },
            OpIn::DeleteEdge { edge, expected } => GraphOp::DeleteEdge {
                edge: edge.into(),
                expected: expected.into(),
            },
            OpIn::Attach {
                node,
                collection,
                record,
            } => GraphOp::Attach {
                node,
                collection,
                record,
            },
            OpIn::Detach { node, collection } => GraphOp::Detach { node, collection },
            OpIn::PutVector {
                collection,
                id,
                vector,
                payload,
                metadata,
            } => GraphOp::Vector {
                collection,
                mutation: match metadata {
                    Some(metadata) => Mutation::PutWithMetadata {
                        id,
                        vector,
                        payload: payload.into_bytes(),
                        metadata,
                    },
                    None => Mutation::Put {
                        id,
                        vector,
                        payload: payload.into_bytes(),
                    },
                },
            },
            OpIn::DeleteVector { collection, id } => GraphOp::Vector {
                collection,
                mutation: Mutation::Delete { id },
            },
            OpIn::EnsureEdge {
                source,
                relationship,
                destination,
                properties,
            } => GraphOp::EnsureEdge {
                source,
                relationship,
                destination,
                properties,
            },
            OpIn::PutSchema { schema } => GraphOp::PutSchema {
                schema: schema.into_schema(),
            },
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DirectionIn {
    #[default]
    Out,
    In,
    Both,
}

impl From<DirectionIn> for Direction {
    fn from(direction: DirectionIn) -> Self {
        match direction {
            DirectionIn::Out => Direction::Out,
            DirectionIn::In => Direction::In,
            DirectionIn::Both => Direction::Both,
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProjectionIn {
    #[default]
    All,
    None,
    Only(Vec<String>),
}

/// Unset fields keep `GraphLimits::DEFAULT`.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LimitsIn {
    max_hops: Option<u64>,
    max_visited_nodes: Option<u64>,
    max_edges_examined: Option<u64>,
    max_edges_per_node: Option<u64>,
    deadline_ms: Option<u64>,
    max_output_nodes: Option<u64>,
    max_output_edges: Option<u64>,
    max_output_paths: Option<u64>,
}

impl LimitsIn {
    pub(crate) fn resolve(&self) -> GraphLimits {
        let base = GraphLimits::DEFAULT;
        GraphLimits {
            max_hops: self.max_hops.unwrap_or(base.max_hops),
            max_visited_nodes: self.max_visited_nodes.unwrap_or(base.max_visited_nodes),
            max_edges_examined: self.max_edges_examined.unwrap_or(base.max_edges_examined),
            max_edges_per_node: self.max_edges_per_node.unwrap_or(base.max_edges_per_node),
            deadline_ms: self.deadline_ms.unwrap_or(base.deadline_ms),
            max_output_nodes: self.max_output_nodes.unwrap_or(base.max_output_nodes),
            max_output_edges: self.max_output_edges.unwrap_or(base.max_output_edges),
            max_output_paths: self.max_output_paths.unwrap_or(base.max_output_paths),
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct FilterIn {
    labels: Vec<String>,
    conditions: Vec<Condition>,
}

impl From<FilterIn> for NodeFilter {
    fn from(filter: FilterIn) -> Self {
        NodeFilter {
            labels: filter.labels,
            conditions: filter.conditions,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GetNodeIn {
    graph: String,
    node: IdOrKey,
    #[serde(default)]
    limits: LimitsIn,
    #[serde(default)]
    projection: ProjectionIn,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GetEdgeIn {
    graph: String,
    edge: IdOrKey,
    #[serde(default)]
    limits: LimitsIn,
    #[serde(default)]
    projection: ProjectionIn,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FindNodesIn {
    graph: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    conditions: Vec<Condition>,
    #[serde(default)]
    limit: Option<u64>,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    limits: LimitsIn,
    #[serde(default)]
    projection: ProjectionIn,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NeighborsIn {
    graph: String,
    node: IdOrKey,
    #[serde(default)]
    direction: DirectionIn,
    #[serde(default)]
    types: Option<Vec<String>>,
    #[serde(default)]
    edge_conditions: Vec<Condition>,
    #[serde(default)]
    limit: Option<u64>,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    limits: LimitsIn,
    #[serde(default)]
    projection: ProjectionIn,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TraverseIn {
    graph: String,
    start: Vec<IdOrKey>,
    #[serde(default)]
    direction: DirectionIn,
    #[serde(default)]
    types: Option<Vec<String>>,
    #[serde(default)]
    expand_filter: FilterIn,
    #[serde(default)]
    return_filter: FilterIn,
    #[serde(default)]
    edge_filter: Vec<Condition>,
    #[serde(default)]
    include_paths: bool,
    #[serde(default)]
    limits: LimitsIn,
    #[serde(default)]
    projection: ProjectionIn,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ShortestPathIn {
    graph: String,
    source: IdOrKey,
    destination: IdOrKey,
    #[serde(default)]
    direction: DirectionIn,
    #[serde(default)]
    types: Option<Vec<String>>,
    #[serde(default)]
    limits: LimitsIn,
    #[serde(default)]
    projection: ProjectionIn,
}

pub(crate) fn cursor_bytes(cursor: Option<String>) -> Result<Option<Vec<u8>>, String> {
    let Some(text) = cursor else {
        return Ok(None);
    };
    let invalid = || String::from("cursor must be the lowercase hex string a read returned");
    if text.len() % 2 != 0 {
        return Err(invalid());
    }
    (0..text.len())
        .step_by(2)
        .map(|at| {
            let pair = text.get(at..at + 2).ok_or_else(invalid)?;
            if !pair
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
            {
                return Err(invalid());
            }
            u8::from_str_radix(pair, 16).map_err(|_| invalid())
        })
        .collect::<Result<Vec<u8>, String>>()
        .map(Some)
}

pub(crate) struct ReadRequest {
    pub(crate) graph: String,
    pub(crate) query: GraphQuery,
    pub(crate) limits: GraphLimits,
    pub(crate) projection: PropertyProjection,
}

pub(crate) fn read_request(command: &str, text: &str) -> Result<ReadRequest, String> {
    let (graph, query, limits, projection) = match command {
        "get-node" => {
            let r: GetNodeIn = parse(text)?;
            let query = GraphQuery::GetNode(r.node.into());
            (r.graph, query, r.limits, r.projection)
        }
        "get-edge" => {
            let r: GetEdgeIn = parse(text)?;
            let query = GraphQuery::GetEdge(r.edge.into());
            (r.graph, query, r.limits, r.projection)
        }
        "find-nodes" => {
            let r: FindNodesIn = parse(text)?;
            let query = GraphQuery::FindNodes(FindNodes {
                label: r.label,
                conditions: r.conditions,
                limit: r.limit,
                cursor: cursor_bytes(r.cursor)?,
            });
            (r.graph, query, r.limits, r.projection)
        }
        "neighbors" => {
            let r: NeighborsIn = parse(text)?;
            let query = GraphQuery::Neighbors(Neighbors {
                node: r.node.into(),
                direction: r.direction.into(),
                types: r.types,
                edge_conditions: r.edge_conditions,
                limit: r.limit,
                cursor: cursor_bytes(r.cursor)?,
            });
            (r.graph, query, r.limits, r.projection)
        }
        "traverse" => {
            let r: TraverseIn = parse(text)?;
            let query = GraphQuery::Traverse(Traverse {
                start: r.start.into_iter().map(Into::into).collect(),
                direction: r.direction.into(),
                types: r.types,
                expand_filter: r.expand_filter.into(),
                return_filter: r.return_filter.into(),
                edge_filter: r.edge_filter,
                include_paths: r.include_paths,
            });
            (r.graph, query, r.limits, r.projection)
        }
        "shortest-path" => {
            let r: ShortestPathIn = parse(text)?;
            let query = GraphQuery::ShortestPath(ShortestPath {
                source: r.source.into(),
                destination: r.destination.into(),
                direction: r.direction.into(),
                types: r.types,
            });
            (r.graph, query, r.limits, r.projection)
        }
        _ => return Err(String::from("not a query command")),
    };
    Ok(ReadRequest {
        graph,
        query,
        limits: limits.resolve(),
        projection: projection.into(),
    })
}

impl From<ProjectionIn> for PropertyProjection {
    fn from(projection: ProjectionIn) -> Self {
        match projection {
            ProjectionIn::All => PropertyProjection::All,
            ProjectionIn::None => PropertyProjection::None,
            ProjectionIn::Only(fields) => PropertyProjection::Only(fields),
        }
    }
}

// Output JSON: u64 IDs, revisions and sequences as decimal strings; counts
// and limits (bounded by the read ceilings) as numbers; enums in serde's
// external tagging (`"name"` or `{"name":{...}}`).

pub(crate) fn ids(values: &[u64]) -> Value {
    values.iter().map(u64::to_string).collect()
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) fn metadata(metadata: &Metadata) -> Value {
    serde_json::to_value(metadata).expect("validated metadata serializes")
}

pub(crate) fn node(node: &NodeView) -> Value {
    json!({
        "id": node.id.to_string(),
        "key": node.key,
        "labels": node.labels,
        "properties": metadata(&node.properties),
        "revision": node.revision.to_string(),
    })
}

pub(crate) fn edge(edge: &EdgeView) -> Value {
    json!({
        "id": edge.id.to_string(),
        "key": edge.key,
        "relationship": edge.relationship,
        "source": edge.source.to_string(),
        "source_key": edge.source_key,
        "target": edge.target.to_string(),
        "target_key": edge.target_key,
        "properties": metadata(&edge.properties),
        "revision": edge.revision.to_string(),
    })
}

pub(crate) fn nodes(values: &[NodeView]) -> Value {
    values.iter().map(node).collect()
}

pub(crate) fn edges(values: &[EdgeView]) -> Value {
    values.iter().map(edge).collect()
}

pub(crate) fn path(path: &GraphPath) -> Value {
    json!({"nodes": ids(&path.nodes), "edges": ids(&path.edges)})
}

pub(crate) fn direction(direction: Direction) -> &'static str {
    match direction {
        Direction::Out => "out",
        Direction::In => "in",
        Direction::Both => "both",
    }
}

pub(crate) fn truncation(reasons: &[TruncationReason]) -> Value {
    reasons
        .iter()
        .map(|reason| match reason {
            TruncationReason::VisitedLimit => json!("visited_limit"),
            TruncationReason::EdgeLimit => json!("edge_limit"),
            TruncationReason::DegreeLimit { node_ids } => {
                json!({"degree_limit": {"node_ids": ids(node_ids)}})
            }
            TruncationReason::OutputLimit => json!("output_limit"),
            TruncationReason::Deadline => json!("deadline"),
            TruncationReason::Cancelled => json!("cancelled"),
        })
        .collect()
}

pub(crate) fn indexes(choices: &[IndexChoice]) -> Value {
    choices
        .iter()
        .map(|choice| match choice {
            IndexChoice::NodeIdLookup => json!("node_id_lookup"),
            IndexChoice::NodeKeyIndex => json!("node_key_index"),
            IndexChoice::EdgeIdLookup => json!("edge_id_lookup"),
            IndexChoice::EdgeKeyIndex => json!("edge_key_index"),
            IndexChoice::LabelIndexScan => json!("label_index_scan"),
            IndexChoice::FullNodeScan => json!("full_node_scan"),
            IndexChoice::PropertyExactIndex => json!("property_exact_index"),
            IndexChoice::PropertyRangeIndex => json!("property_range_index"),
            IndexChoice::AdjacencyRangeScan {
                direction: scan,
                typed,
            } => {
                json!({"adjacency_range_scan": {"direction": direction(*scan), "typed": typed}})
            }
        })
        .collect()
}

pub(crate) fn stats(stats: &GraphReadStats) -> Value {
    json!({
        "nodes_visited": stats.nodes_visited,
        "nodes_scanned": stats.nodes_scanned,
        "edges_examined": stats.edges_examined,
        "nodes_returned": stats.nodes_returned,
        "edges_returned": stats.edges_returned,
        "paths_returned": stats.paths_returned,
        "elapsed_ms": stats.elapsed_ms,
        "indexes": indexes(&stats.indexes),
    })
}

pub(crate) fn limits(limits: &GraphLimits) -> Value {
    json!({
        "max_hops": limits.max_hops,
        "max_visited_nodes": limits.max_visited_nodes,
        "max_edges_examined": limits.max_edges_examined,
        "max_edges_per_node": limits.max_edges_per_node,
        "deadline_ms": limits.deadline_ms,
        "max_output_nodes": limits.max_output_nodes,
        "max_output_edges": limits.max_output_edges,
        "max_output_paths": limits.max_output_paths,
    })
}

pub(crate) fn projection(projection: &PropertyProjection) -> Value {
    match projection {
        PropertyProjection::All => json!("all"),
        PropertyProjection::None => json!("none"),
        PropertyProjection::Only(fields) => json!({"only": fields}),
    }
}

pub(crate) fn selector(selector: &NodeSelector) -> Value {
    match selector {
        NodeSelector::Id(id) => json!({"id": id.to_string()}),
        NodeSelector::Key(key) => json!({"key": key}),
    }
}

pub(crate) fn edge_ref(edge: &EdgeRef) -> Value {
    match edge {
        EdgeRef::Id(id) => json!({"id": id.to_string()}),
        EdgeRef::Key(key) => json!({"key": key}),
    }
}

/// The request form of a condition (`Condition` only implements Deserialize).
pub(crate) fn conditions(conditions: &[Condition]) -> Value {
    let value = |value| serde_json::to_value(value).expect("metadata values serialize");
    conditions
        .iter()
        .map(|condition| match condition {
            Condition::Eq { field, value: v } => {
                json!({"op": "eq", "field": field, "value": value(v)})
            }
            Condition::Range { field, min, max } => json!({
                "op": "range",
                "field": field,
                "min": min.as_ref().map(value),
                "max": max.as_ref().map(value),
            }),
            Condition::GeoRadius {
                field,
                latitude,
                longitude,
                radius_meters,
            } => json!({
                "op": "georadius",
                "field": field,
                "latitude": latitude,
                "longitude": longitude,
                "radius_meters": radius_meters,
            }),
        })
        .collect()
}

pub(crate) fn filter(filter: &NodeFilter) -> Value {
    json!({"labels": filter.labels, "conditions": conditions(&filter.conditions)})
}

/// A normalized query in its request form, with the command as `operation`.
pub(crate) fn query(query: &GraphQuery) -> Result<Value, String> {
    Ok(match query {
        GraphQuery::GetNode(node) => json!({"operation": "get-node", "node": selector(node)}),
        GraphQuery::GetEdge(edge) => json!({"operation": "get-edge", "edge": edge_ref(edge)}),
        GraphQuery::FindNodes(find) => json!({
            "operation": "find-nodes",
            "label": find.label,
            "conditions": conditions(&find.conditions),
            "limit": find.limit,
            "cursor": find.cursor.as_deref().map(hex),
        }),
        GraphQuery::Neighbors(request) => json!({
            "operation": "neighbors",
            "node": selector(&request.node),
            "direction": direction(request.direction),
            "types": request.types,
            "edge_conditions": conditions(&request.edge_conditions),
            "limit": request.limit,
            "cursor": request.cursor.as_deref().map(hex),
        }),
        GraphQuery::Traverse(request) => json!({
            "operation": "traverse",
            "start": request.start.iter().map(selector).collect::<Value>(),
            "direction": direction(request.direction),
            "types": request.types,
            "expand_filter": filter(&request.expand_filter),
            "return_filter": filter(&request.return_filter),
            "edge_filter": conditions(&request.edge_filter),
            "include_paths": request.include_paths,
        }),
        GraphQuery::ShortestPath(request) => json!({
            "operation": "shortest-path",
            "source": selector(&request.source),
            "destination": selector(&request.destination),
            "direction": direction(request.direction),
            "types": request.types,
        }),
        _ => return Err(String::from("unsupported query kind in explanation")),
    })
}

pub(crate) fn explanation(graph: &str, explanation: &Explanation) -> Result<Value, String> {
    Ok(json!({
        "graph": graph,
        "explain": {
            "query": query(&explanation.query)?,
            "limits": limits(&explanation.limits),
            "projection": projection(&explanation.projection),
            "indexes": indexes(&explanation.indexes),
        },
    }))
}

pub(crate) fn read_output(graph: &str, read: &GraphRead<GraphOutput>) -> Result<Value, String> {
    let mut out = Map::new();
    out.insert("graph".into(), json!(graph));
    out.insert(
        "view_sequence".into(),
        json!(read.view_sequence.to_string()),
    );
    out.insert("complete".into(), json!(read.complete));
    out.insert("truncation".into(), truncation(&read.truncation));
    out.insert("stats".into(), stats(&read.stats));
    out.insert("cursor".into(), json!(read.cursor.as_deref().map(hex)));
    match &read.output {
        GraphOutput::Node(found) => {
            out.insert("node".into(), found.as_ref().map_or(Value::Null, node));
        }
        GraphOutput::Edge(found) => {
            out.insert("edge".into(), found.as_ref().map_or(Value::Null, edge));
        }
        GraphOutput::Nodes(found) => {
            out.insert("nodes".into(), nodes(found));
        }
        GraphOutput::Neighbors(neighborhood) => {
            out.insert("edges".into(), edges(&neighborhood.edges));
            out.insert("nodes".into(), nodes(&neighborhood.nodes));
        }
        GraphOutput::Traversal(traversal) => {
            let visited: Value = traversal
                .nodes
                .iter()
                .map(|item| json!({"depth": item.depth, "node": node(&item.node)}))
                .collect();
            out.insert("nodes".into(), visited);
            out.insert("edges".into(), edges(&traversal.edges));
            out.insert(
                "paths".into(),
                traversal.paths.iter().map(path).collect::<Value>(),
            );
        }
        GraphOutput::ShortestPath(outcome) => {
            let outcome = match outcome {
                PathOutcome::Found {
                    path: found,
                    nodes: on_path,
                    edges: along,
                } => json!({
                    "outcome": "found",
                    "path": path(found),
                    "nodes": nodes(on_path),
                    "edges": edges(along),
                }),
                PathOutcome::NoPathWithinHops => json!({"outcome": "no_path_within_hops"}),
                PathOutcome::Exhausted { truncation: cut } => {
                    json!({"outcome": "exhausted", "truncation": truncation(cut)})
                }
            };
            out.insert("shortest_path".into(), outcome);
        }
        _ => return Err(String::from("unsupported read output")),
    }
    Ok(Value::Object(out))
}

pub(crate) fn info(info: &GraphInfo) -> Value {
    json!({"name": info.name, "node_count": info.node_count, "edge_count": info.edge_count})
}

pub(crate) fn write(write: &GraphWrite) -> Value {
    json!({
        "id": write.id.to_string(),
        "created": write.created,
        "revision": write.revision.to_string(),
    })
}

pub(crate) fn batch_output(graph: &str, result: &GraphBatchResult) -> Result<Value, String> {
    let outcomes = result
        .outcomes
        .iter()
        .map(|outcome| {
            Ok(match outcome {
                GraphOpOutcome::Node(written) => json!({"node": write(written)}),
                GraphOpOutcome::Edge(written) => json!({"edge": write(written)}),
                GraphOpOutcome::NodeDeleted { id } => {
                    json!({"node_deleted": {"id": id.to_string()}})
                }
                GraphOpOutcome::EdgeDeleted { id } => {
                    json!({"edge_deleted": {"id": id.to_string()}})
                }
                GraphOpOutcome::Attached { node } => {
                    json!({"attached": {"node": node.to_string()}})
                }
                GraphOpOutcome::Detached { node, record } => json!({
                    "detached": {"node": node.to_string(), "record": record.to_string()}
                }),
                GraphOpOutcome::Vector => json!("vector"),
                GraphOpOutcome::Schema => json!("schema"),
                _ => return Err(String::from("unsupported batch outcome")),
            })
        })
        .collect::<Result<Vec<Value>, String>>()?;
    Ok(json!({
        "graph": graph,
        "revision": result.revision.map(|revision| revision.to_string()),
        "outcomes": outcomes,
    }))
}

// Agent surfaces (wave 2): declared/observed schema, hybrid retrieve and
// graph query text. MCP and REST share these encodings; MCP requests add the
// `graph` field that REST takes from the path.

/// Removes the `graph` field from MCP arguments, leaving the REST body.
pub(crate) fn split_graph(args: &Value) -> Result<(String, Value), String> {
    let mut object = args
        .as_object()
        .cloned()
        .ok_or("arguments must be a JSON object")?;
    let graph = match object.remove("graph") {
        Some(Value::String(graph)) => graph,
        Some(_) => return Err("graph must be a string".into()),
        None => return Err("missing field `graph`".into()),
    };
    Ok((graph, Value::Object(object)))
}

/// Parses a body object that is a JSON value already.
pub(crate) fn parse_value<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, String> {
    serde_json::from_value(value).map_err(|error| error.to_string())
}

/// An object whose repeated keys are rejected instead of keeping the last.
fn de_unique_map<'de, D, T>(deserializer: D) -> Result<BTreeMap<String, T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Unique<T>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for Unique<T> {
        type Value = BTreeMap<String, T>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("an object")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> Result<Self::Value, A::Error> {
            let mut out = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, T>()? {
                if out.contains_key(&key) {
                    return Err(serde::de::Error::custom(format!("duplicate key {key:?}")));
                }
                out.insert(key, value);
            }
            Ok(out)
        }
    }
    deserializer.deserialize_map(Unique(std::marker::PhantomData))
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PropertyTypeIn {
    String,
    Integer,
    Float,
    Boolean,
    Geopoint,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum IndexIn {
    Exact,
    Range,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MultiplicityIn {
    #[default]
    Multi,
    UniqueTriple,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PropertyIn {
    #[serde(rename = "type")]
    value_type: PropertyTypeIn,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    unique: bool,
    #[serde(default)]
    index: Option<IndexIn>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LabelIn {
    #[serde(default)]
    description: String,
    #[serde(default, deserialize_with = "de_unique_map")]
    properties: BTreeMap<String, PropertyIn>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EndpointIn {
    source: String,
    destination: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RelationshipIn {
    #[serde(default)]
    description: String,
    #[serde(default)]
    endpoints: Vec<EndpointIn>,
    #[serde(default)]
    multiplicity: MultiplicityIn,
    #[serde(default, deserialize_with = "de_unique_map")]
    properties: BTreeMap<String, PropertyIn>,
}

/// A declared schema (REQ-030); `{}` declares nothing (removes the schema).
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SchemaIn {
    #[serde(default, deserialize_with = "de_unique_map")]
    labels: BTreeMap<String, LabelIn>,
    #[serde(default, deserialize_with = "de_unique_map")]
    relationships: BTreeMap<String, RelationshipIn>,
}

fn property_definition(property: PropertyIn) -> PropertyDefinition {
    let mut definition = PropertyDefinition::new(match property.value_type {
        PropertyTypeIn::String => PropertyType::String,
        PropertyTypeIn::Integer => PropertyType::Integer,
        PropertyTypeIn::Float => PropertyType::Float,
        PropertyTypeIn::Boolean => PropertyType::Boolean,
        PropertyTypeIn::Geopoint => PropertyType::GeoPoint,
    });
    if property.required {
        definition = definition.required();
    }
    if property.unique {
        definition = definition.unique();
    }
    match property.index {
        Some(IndexIn::Exact) => definition.index(PropertyIndexKind::Exact),
        Some(IndexIn::Range) => definition.index(PropertyIndexKind::Range),
        None => definition,
    }
}

impl SchemaIn {
    /// The library schema; `GraphSchema::validate` runs on write.
    pub(crate) fn into_schema(self) -> GraphSchema {
        let mut schema = GraphSchema::default();
        for (name, label) in self.labels {
            let mut definition = LabelDefinition::new(label.description);
            for (property, declared) in label.properties {
                definition = definition.property(property, property_definition(declared));
            }
            schema = schema.label(name, definition);
        }
        for (name, relationship) in self.relationships {
            let mut definition = RelationshipDefinition::new(relationship.description)
                .multiplicity(match relationship.multiplicity {
                    MultiplicityIn::Multi => Multiplicity::Multi,
                    MultiplicityIn::UniqueTriple => Multiplicity::UniqueTriple,
                });
            for endpoint in relationship.endpoints {
                definition = definition.endpoint(endpoint.source, endpoint.destination);
            }
            for (property, declared) in relationship.properties {
                definition = definition.property(property, property_definition(declared));
            }
            schema = schema.relationship(name, definition);
        }
        schema
    }
}

/// `vlite_graph_schema` / `GET .../schema` options.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SchemaQueryIn {
    #[serde(default, deserialize_with = "de_optional_u64")]
    pub(crate) budget: Option<u64>,
}

fn de_optional_u64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u64>, D::Error> {
    Option::<Value>::deserialize(deserializer)?
        .map(|value| parse_u64(&value).map_err(D::Error::custom))
        .transpose()
}

pub(crate) fn property_type(value_type: PropertyType) -> &'static str {
    value_type.json_name()
}

/// The declared schema in the `SchemaIn` shape (round-trips).
pub(crate) fn declared_schema(schema: &GraphSchema) -> Value {
    schema.to_json()
}

fn origin(origin: SchemaOrigin) -> &'static str {
    match origin {
        SchemaOrigin::Declared => "declared",
        SchemaOrigin::Observed => "observed",
        SchemaOrigin::Both => "both",
    }
}

fn observed_properties(properties: &[ObservedProperty]) -> Value {
    properties
        .iter()
        .map(|property| {
            json!({
                "name": property.name,
                "origin": origin(property.origin),
                "count": property.count,
                "types": property.types.iter().copied().map(property_type).collect::<Vec<_>>(),
            })
        })
        .collect()
}

/// The observed summary: labels, relationship types, endpoint pairs and
/// property types with counts, merged with the declared schema.
pub(crate) fn observed_schema(observed: &ObservedSchema) -> Value {
    let labels: Value = observed
        .labels
        .iter()
        .map(|label| {
            json!({
                "name": label.name,
                "origin": origin(label.origin),
                "count": label.count,
                "properties": observed_properties(&label.properties),
            })
        })
        .collect();
    let relationships: Value = observed
        .relationships
        .iter()
        .map(|relationship| {
            let endpoints: Value = relationship
                .endpoints
                .iter()
                .map(|endpoint| {
                    json!({
                        "source": endpoint.source,
                        "destination": endpoint.destination,
                        "origin": origin(endpoint.origin),
                        "count": endpoint.count,
                    })
                })
                .collect();
            json!({
                "name": relationship.name,
                "origin": origin(relationship.origin),
                "count": relationship.count,
                "endpoints": endpoints,
                "properties": observed_properties(&relationship.properties),
            })
        })
        .collect();
    json!({
        "complete": observed.complete,
        "entities_scanned": observed.entities_scanned,
        "labels": labels,
        "relationships": relationships,
    })
}

pub(crate) fn schema_output(
    graph: &str,
    declared: &GraphSchema,
    observed: &ObservedSchema,
) -> Value {
    json!({
        "graph": graph,
        "declared": declared_schema(declared),
        "observed": observed_schema(observed),
    })
}

pub(crate) fn put_schema_output(graph: &str, revision: u64) -> Value {
    json!({"graph": graph, "revision": revision.to_string()})
}

// Hybrid retrieve (REQ-040..REQ-042).

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MetricIn {
    #[default]
    SquaredL2,
    Cosine,
    InnerProduct,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PivotIn {
    collection: String,
    #[serde(default)]
    vector: Option<Vec<f32>>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    k: Option<usize>,
    #[serde(default)]
    filter: Vec<Condition>,
    #[serde(default)]
    metric: MetricIn,
    #[serde(default, deserialize_with = "de_optional_u64")]
    max_exact_scan_records: Option<u64>,
    #[serde(default)]
    candidates: Option<usize>,
    #[serde(default)]
    rrf_k: Option<u32>,
    #[serde(default)]
    vector_weight: Option<f64>,
    #[serde(default)]
    text_weight: Option<f64>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExpansionIn {
    #[serde(default)]
    direction: DirectionIn,
    #[serde(default)]
    types: Option<Vec<String>>,
    #[serde(default)]
    expand_filter: FilterIn,
    #[serde(default)]
    return_filter: FilterIn,
    #[serde(default)]
    edge_filter: Vec<Condition>,
    #[serde(default)]
    include_paths: bool,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EvidenceIn {
    #[serde(default)]
    collections: Vec<String>,
    #[serde(default)]
    fields: ProjectionIn,
    #[serde(default)]
    max_excerpt_bytes: Option<usize>,
    #[serde(default, deserialize_with = "de_optional_u64")]
    max_items: Option<u64>,
}

/// The retrieve body (MCP adds `graph`).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RetrieveIn {
    pivot: PivotIn,
    #[serde(default)]
    expansion: ExpansionIn,
    #[serde(default)]
    evidence: EvidenceIn,
    #[serde(default)]
    limits: LimitsIn,
    #[serde(default)]
    projection: ProjectionIn,
}

pub(crate) struct RetrieveRequest {
    pub(crate) retrieve: Retrieve,
    pub(crate) limits: GraphLimits,
    pub(crate) projection: PropertyProjection,
}

impl From<RetrieveIn> for RetrieveRequest {
    fn from(input: RetrieveIn) -> Self {
        let pivot_in = input.pivot;
        let mut pivot = Pivot::new(&pivot_in.collection, pivot_in.k.unwrap_or(Pivot::DEFAULT_K));
        pivot.vector = pivot_in.vector;
        pivot.text = pivot_in.text;
        pivot.filter = pivot_in.filter;
        pivot.metric = match pivot_in.metric {
            MetricIn::SquaredL2 => DistanceMetric::SquaredL2,
            MetricIn::Cosine => DistanceMetric::Cosine,
            MetricIn::InnerProduct => DistanceMetric::InnerProduct,
        };
        pivot.vector_leg = VectorLeg::Exact;
        let defaults = FusionOptions::default();
        pivot.fusion = FusionOptions {
            candidates: pivot_in.candidates,
            rrf_k: pivot_in.rrf_k.unwrap_or(defaults.rrf_k),
            vector_weight: pivot_in.vector_weight.unwrap_or(defaults.vector_weight),
            text_weight: pivot_in.text_weight.unwrap_or(defaults.text_weight),
        };
        if let Some(records) = pivot_in.max_exact_scan_records {
            pivot.max_exact_scan_records = records;
        }
        let expansion = Expansion {
            direction: input.expansion.direction.into(),
            types: input.expansion.types,
            expand_filter: input.expansion.expand_filter.into(),
            return_filter: input.expansion.return_filter.into(),
            edge_filter: input.expansion.edge_filter,
            include_paths: input.expansion.include_paths,
        };
        let evidence = EvidenceSpec {
            collections: input.evidence.collections,
            fields: input.evidence.fields.into(),
            max_excerpt_bytes: input
                .evidence
                .max_excerpt_bytes
                .unwrap_or(EvidenceSpec::DEFAULT_EXCERPT_BYTES),
            max_items: input
                .evidence
                .max_items
                .unwrap_or(EvidenceSpec::DEFAULT_ITEMS),
        };
        RetrieveRequest {
            retrieve: Retrieve {
                pivot,
                expansion,
                evidence,
            },
            limits: input.limits.resolve(),
            projection: input.projection.into(),
        }
    }
}

pub(crate) fn retrieve_request(body: Value) -> Result<RetrieveRequest, String> {
    parse_value::<RetrieveIn>(body).map(Into::into)
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let triple = chunk.iter().enumerate().fold(0u32, |acc, (at, byte)| {
            acc | u32::from(*byte) << (16 - 8 * at)
        });
        for at in 0..4 {
            if at <= chunk.len() {
                out.push(char::from(BASE64[(triple >> (18 - 6 * at)) as usize & 63]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// UTF-8 excerpts are text (a character cut by the excerpt bound is
/// dropped); other payloads are base64.
fn excerpt(bytes: &[u8], truncated: bool) -> (Value, Value) {
    match std::str::from_utf8(bytes) {
        Ok(text) => (json!(text), Value::Null),
        Err(error) if truncated && error.error_len().is_none() => {
            let text = std::str::from_utf8(&bytes[..error.valid_up_to()]).unwrap_or_default();
            (json!(text), Value::Null)
        }
        Err(_) => (Value::Null, json!(base64(bytes))),
    }
}

pub(crate) fn evidence(item: &vlite::property_graph::Evidence) -> Value {
    let (text, binary) = excerpt(&item.payload_excerpt, item.excerpt_truncated);
    json!({
        "collection": item.collection,
        "record_id": item.record_id.to_string(),
        "revision": item.revision.to_string(),
        "view_sequence": item.view_sequence.to_string(),
        "node_id": item.node_id.to_string(),
        "metadata": metadata(&item.metadata),
        "payload_excerpt": text,
        "payload_excerpt_base64": binary,
        "payload_bytes": item.payload_bytes,
        "excerpt_truncated": item.excerpt_truncated,
    })
}

/// Compact retrieval: pivots, expanded nodes/edges/paths and evidence; never
/// vectors. `evidence` lists the items to return (REST drops redacted ones).
pub(crate) fn retrieval_output(
    graph: &str,
    retrieval: &Retrieval,
    evidence_items: &[&vlite::property_graph::Evidence],
) -> Value {
    let pivots: Value = retrieval
        .pivots
        .iter()
        .map(|hit| {
            json!({
                "record_id": hit.record_id.to_string(),
                "node_id": hit.node_id.to_string(),
                "node_key": hit.node_key,
                "score": hit.score,
                "distance": hit.distance,
                "bm25": hit.bm25,
                "vector_rank": hit.vector_rank,
                "text_rank": hit.text_rank,
            })
        })
        .collect();
    let visited: Value = retrieval
        .nodes
        .iter()
        .map(|item| json!({"depth": item.depth, "node": node(&item.node)}))
        .collect();
    let stats = &retrieval.stats;
    json!({
        "graph": graph,
        "view_sequence": retrieval.view_sequence.to_string(),
        "complete": retrieval.complete,
        "truncation": truncation(&retrieval.truncation),
        "pivots": pivots,
        "unmapped_pivots": {
            "count": retrieval.unmapped_pivots.count,
            "record_ids": ids(&retrieval.unmapped_pivots.record_ids),
        },
        "nodes": visited,
        "edges": edges(&retrieval.edges),
        "paths": retrieval.paths.iter().map(path).collect::<Value>(),
        "evidence": evidence_items.iter().map(|item| evidence(item)).collect::<Value>(),
        "stats": {
            "pivot_ms": stats.pivot_ms,
            "expand_ms": stats.expand_ms,
            "pivot_candidates": stats.pivot_candidates,
            "pivots_mapped": stats.pivots_mapped,
            "pivots_rejected": stats.pivots_rejected,
            "evidence_items": evidence_items.len(),
            "records_scanned": stats.search.records_scanned,
            "distance_evaluations": stats.search.distance_evaluations,
            "expansion": self::stats(&stats.expansion),
        },
    })
}

// Graph query text (EPIC-015).

/// The query-text body (MCP adds `graph`).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueryTextIn {
    pub(crate) query: String,
    #[serde(default, deserialize_with = "de_properties")]
    pub(crate) params: Metadata,
    #[serde(default)]
    pub(crate) limits: LimitsIn,
    #[serde(default)]
    pub(crate) explain: bool,
}

/// Stable code and message of a query-text failure.
pub(crate) fn text_error(error: &GraphTextError) -> (&'static str, String) {
    match error {
        GraphTextError::Query(error) => (
            match error.kind {
                GraphQueryErrorKind::Syntax => "graph_query_syntax",
                GraphQueryErrorKind::Unsupported => "graph_query_unsupported",
                GraphQueryErrorKind::Parameter => "graph_query_parameter",
                GraphQueryErrorKind::Limit => "graph_query_limit",
            },
            error.to_string(),
        ),
        GraphTextError::Read(error) => (error_code(error), error.to_string()),
        _ => ("database_error", String::from("unknown graph query error")),
    }
}
