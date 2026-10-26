//! Opt-in property-graph operations (`graph` feature) carried by the existing
//! owned JSON ABI as `{"op":"graph","request":{"op":...}}`.
//!
//! The request and response JSON mirror the opt-in `vlite-graph-cli`:
//! node/edge/record IDs, revisions and sequences are decimal strings (also
//! accepted as JSON integers), unknown fields are rejected, and graph error
//! codes reuse the REST/gRPC names.
use super::*;
use serde::Deserializer;
use serde::de::Error as _;
use serde_json::Map;
use vlite::property_graph::{
    Direction, EdgeRef, EdgeView, Explanation, FindNodes, GraphBatchResult, GraphInfo, GraphLimits,
    GraphOp, GraphOpOutcome, GraphOutput, GraphPath, GraphQuery, GraphRead, GraphReadStats,
    GraphWrite, IndexChoice, Neighbors, NodeFilter, NodePatch, NodeSelector, NodeView, PathOutcome,
    PropertyProjection, ReadOptions, ShortestPath, Traverse, TruncationReason,
};
use vlite::{Condition, MetadataValue};

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    /// Handle 0: a NEW graph-enabled (V5 + graph flag) file; never converts.
    Create {
        path: String,
        dimensions: usize,
        #[serde(default)]
        options: features::Options,
    },
    IsGraphEnabled {},
    ListGraphs {},
    CreateGraph {
        graph: String,
    },
    DropGraph {
        graph: String,
    },
    GetNode(GetNodeIn),
    GetEdge(GetEdgeIn),
    FindNodes(FindNodesIn),
    Neighbors(NeighborsIn),
    Traverse(TraverseIn),
    ShortestPath(ShortestPathIn),
    /// Plans one query request (any of the six read ops) without running it.
    Explain {
        query: Box<Request>,
    },
    /// One atomic op-12 frame; nothing is written on error.
    Batch {
        graph: String,
        ops: Vec<OpIn>,
    },
}

/// Stable codes: graph variants use the REST/gRPC names; everything else
/// keeps the existing C ABI mapping.
fn graph_failure(error: vlite::Error) -> Failure {
    use vlite::Error as E;
    let code = match &error {
        E::GraphsNotEnabled => "graphs_not_enabled",
        E::GraphsRequireV5 => "graphs_require_v5",
        E::InvalidGraphName => "invalid_graph_name",
        E::GraphExists => "graph_exists",
        E::GraphNotFound => "graph_not_found",
        E::GraphNotEmpty => "graph_not_empty",
        E::TooManyGraphs => "too_many_graphs",
        E::GraphKeyExists => "graph_key_exists",
        E::GraphKeyConflict => "graph_key_conflict",
        E::GraphNodeNotFound => "graph_node_not_found",
        E::GraphEdgeNotFound => "graph_edge_not_found",
        E::GraphNodeHasEdges { .. } => "graph_node_has_edges",
        E::GraphAttachmentConflict => "graph_attachment_conflict",
        E::GraphAttachmentNotFound => "graph_attachment_not_found",
        E::GraphRecordNotAttachable => "graph_record_not_attachable",
        E::GraphAttachedRecord => "graph_attached_record",
        E::GraphAttachedRecordExpiry => "graph_attached_record_expiry",
        E::GraphLimitExceedsCeiling { .. } => "graph_limit_exceeds_ceiling",
        E::GraphCursorStale => "graph_cursor_stale",
        E::GraphChangeFeedUnsupported => "graph_change_feed_unsupported",
        E::GraphUnsupportedOperation => "graph_unsupported_operation",
        E::GraphInvalidOptions(_) => "graph_invalid_options",
        E::GraphSchemaViolation { .. } => "graph_schema_violation",
        E::GraphPivotTooExpensive => "graph_pivot_too_expensive",
        _ => return failure(error),
    };
    (code, error.to_string())
}

// Request JSON. Every object rejects unknown fields.

/// Canonical decimal string or non-negative JSON integer (as `vlite-graph-cli`).
fn parse_u64(value: &Value) -> std::result::Result<u64, String> {
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

fn de_u64<'de, D: Deserializer<'de>>(deserializer: D) -> std::result::Result<u64, D::Error> {
    parse_u64(&Value::deserialize(deserializer)?).map_err(D::Error::custom)
}

/// `Metadata` that rejects a repeated key instead of keeping the last value.
struct Properties(Metadata);

impl<'de> Deserialize<'de> for Properties {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct Unique;
        impl<'de> serde::de::Visitor<'de> for Unique {
            type Value = Metadata;
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("an object of typed values")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Metadata, A::Error> {
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

fn de_properties<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Metadata, D::Error> {
    Properties::deserialize(deserializer).map(|properties| properties.0)
}

fn de_optional_properties<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Metadata>, D::Error> {
    Option::<Properties>::deserialize(deserializer)
        .map(|found| found.map(|properties| properties.0))
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ExpectedIn {
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
enum IdOrKey {
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
enum OpIn {
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
        /// Canonical padded base64, as the vector `put` operation.
        #[serde(default)]
        payload_base64: String,
        #[serde(default, deserialize_with = "de_optional_properties")]
        metadata: Option<Metadata>,
    },
    DeleteVector {
        collection: String,
        #[serde(deserialize_with = "de_u64")]
        id: u64,
    },
}

fn graph_op(op: OpIn) -> Result<GraphOp> {
    Ok(match op {
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
            payload_base64,
            metadata,
        } => {
            let payload = payload(&payload_base64)?;
            GraphOp::Vector {
                collection,
                mutation: match metadata {
                    Some(metadata) => Mutation::PutWithMetadata {
                        id,
                        vector,
                        payload,
                        metadata,
                    },
                    None => Mutation::Put {
                        id,
                        vector,
                        payload,
                    },
                },
            }
        }
        OpIn::DeleteVector { collection, id } => GraphOp::Vector {
            collection,
            mutation: Mutation::Delete { id },
        },
    })
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DirectionIn {
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
enum ProjectionIn {
    #[default]
    All,
    None,
    Only(Vec<String>),
}

/// Unset fields keep `GraphLimits::DEFAULT`.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LimitsIn {
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
    fn resolve(&self) -> GraphLimits {
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
struct FilterIn {
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
struct GetNodeIn {
    graph: String,
    node: IdOrKey,
    #[serde(default)]
    limits: LimitsIn,
    #[serde(default)]
    projection: ProjectionIn,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetEdgeIn {
    graph: String,
    edge: IdOrKey,
    #[serde(default)]
    limits: LimitsIn,
    #[serde(default)]
    projection: ProjectionIn,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FindNodesIn {
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
struct NeighborsIn {
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
struct TraverseIn {
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
struct ShortestPathIn {
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

fn cursor_bytes(cursor: Option<String>) -> Result<Option<Vec<u8>>> {
    let Some(text) = cursor else {
        return Ok(None);
    };
    let bad = || invalid("cursor must be the lowercase hex string a read returned");
    if text.len() % 2 != 0
        || !text
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(bad());
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).map_err(|_| bad()))
        .collect::<Result<Vec<u8>>>()
        .map(Some)
}

struct ReadRequest {
    graph: String,
    query: GraphQuery,
    options: ReadOptions<'static>,
}

fn read_request(request: Request) -> Result<ReadRequest> {
    let (graph, query, limits, projection) = match request {
        Request::GetNode(r) => (
            r.graph,
            GraphQuery::GetNode(r.node.into()),
            r.limits,
            r.projection,
        ),
        Request::GetEdge(r) => (
            r.graph,
            GraphQuery::GetEdge(r.edge.into()),
            r.limits,
            r.projection,
        ),
        Request::FindNodes(r) => {
            let query = GraphQuery::FindNodes(FindNodes {
                label: r.label,
                conditions: r.conditions,
                limit: r.limit,
                cursor: cursor_bytes(r.cursor)?,
            });
            (r.graph, query, r.limits, r.projection)
        }
        Request::Neighbors(r) => {
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
        Request::Traverse(r) => {
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
        Request::ShortestPath(r) => {
            let query = GraphQuery::ShortestPath(ShortestPath {
                source: r.source.into(),
                destination: r.destination.into(),
                direction: r.direction.into(),
                types: r.types,
            });
            (r.graph, query, r.limits, r.projection)
        }
        _ => {
            return Err(invalid(
                "explain query must be get_node, get_edge, find_nodes, neighbors, traverse or shortest_path",
            ));
        }
    };
    Ok(ReadRequest {
        graph,
        query,
        options: ReadOptions {
            limits: limits.resolve(),
            projection: match projection {
                ProjectionIn::All => PropertyProjection::All,
                ProjectionIn::None => PropertyProjection::None,
                ProjectionIn::Only(fields) => PropertyProjection::Only(fields),
            },
            cancel: None,
        },
    })
}

// Output JSON: u64 IDs, revisions and sequences as decimal strings; counts
// and limits (bounded by the read ceilings) as numbers.

fn internal(message: &str) -> Failure {
    ("internal_error", message.to_owned())
}

fn ids(values: &[u64]) -> Value {
    values.iter().map(u64::to_string).collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn node(node: &NodeView) -> Value {
    json!({
        "id": node.id.to_string(),
        "key": node.key,
        "labels": node.labels,
        "properties": node.properties,
        "revision": node.revision.to_string(),
    })
}

fn edge(edge: &EdgeView) -> Value {
    json!({
        "id": edge.id.to_string(),
        "key": edge.key,
        "relationship": edge.relationship,
        "source": edge.source.to_string(),
        "source_key": edge.source_key,
        "target": edge.target.to_string(),
        "target_key": edge.target_key,
        "properties": edge.properties,
        "revision": edge.revision.to_string(),
    })
}

fn nodes(values: &[NodeView]) -> Value {
    values.iter().map(node).collect()
}

fn edges(values: &[EdgeView]) -> Value {
    values.iter().map(edge).collect()
}

fn path(path: &GraphPath) -> Value {
    json!({"nodes": ids(&path.nodes), "edges": ids(&path.edges)})
}

fn direction(direction: Direction) -> &'static str {
    match direction {
        Direction::Out => "out",
        Direction::In => "in",
        Direction::Both => "both",
    }
}

fn truncation(reasons: &[TruncationReason]) -> Value {
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

fn indexes(choices: &[IndexChoice]) -> Value {
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

fn stats(stats: &GraphReadStats) -> Value {
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

fn limits(limits: &GraphLimits) -> Value {
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

fn projection(projection: &PropertyProjection) -> Value {
    match projection {
        PropertyProjection::All => json!("all"),
        PropertyProjection::None => json!("none"),
        PropertyProjection::Only(fields) => json!({"only": fields}),
    }
}

fn selector(selector: &NodeSelector) -> Value {
    match selector {
        NodeSelector::Id(id) => json!({"id": id.to_string()}),
        NodeSelector::Key(key) => json!({"key": key}),
    }
}

fn edge_ref(edge: &EdgeRef) -> Value {
    match edge {
        EdgeRef::Id(id) => json!({"id": id.to_string()}),
        EdgeRef::Key(key) => json!({"key": key}),
    }
}

/// The request form of a condition (`Condition` only implements Deserialize).
fn conditions(conditions: &[Condition]) -> Value {
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

fn filter(filter: &NodeFilter) -> Value {
    json!({"labels": filter.labels, "conditions": conditions(&filter.conditions)})
}

/// A normalized query in its request form, with the request `op` name.
fn query(query: &GraphQuery) -> Result<Value> {
    Ok(match query {
        GraphQuery::GetNode(node) => json!({"op": "get_node", "node": selector(node)}),
        GraphQuery::GetEdge(edge) => json!({"op": "get_edge", "edge": edge_ref(edge)}),
        GraphQuery::FindNodes(find) => json!({
            "op": "find_nodes",
            "label": find.label,
            "conditions": conditions(&find.conditions),
            "limit": find.limit,
            "cursor": find.cursor.as_deref().map(hex),
        }),
        GraphQuery::Neighbors(request) => json!({
            "op": "neighbors",
            "node": selector(&request.node),
            "direction": direction(request.direction),
            "types": request.types,
            "edge_conditions": conditions(&request.edge_conditions),
            "limit": request.limit,
            "cursor": request.cursor.as_deref().map(hex),
        }),
        GraphQuery::Traverse(request) => json!({
            "op": "traverse",
            "start": request.start.iter().map(selector).collect::<Value>(),
            "direction": direction(request.direction),
            "types": request.types,
            "expand_filter": filter(&request.expand_filter),
            "return_filter": filter(&request.return_filter),
            "edge_filter": conditions(&request.edge_filter),
            "include_paths": request.include_paths,
        }),
        GraphQuery::ShortestPath(request) => json!({
            "op": "shortest_path",
            "source": selector(&request.source),
            "destination": selector(&request.destination),
            "direction": direction(request.direction),
            "types": request.types,
        }),
        _ => return Err(internal("unsupported query kind in explanation")),
    })
}

fn explanation(graph: &str, explanation: &Explanation) -> Result<Value> {
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

fn read_output(graph: &str, read: &GraphRead<GraphOutput>) -> Result<Value> {
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
        _ => return Err(internal("unsupported read output")),
    }
    Ok(Value::Object(out))
}

fn info(info: &GraphInfo) -> Value {
    json!({"name": info.name, "node_count": info.node_count, "edge_count": info.edge_count})
}

fn write(write: &GraphWrite) -> Value {
    json!({
        "id": write.id.to_string(),
        "created": write.created,
        "revision": write.revision.to_string(),
    })
}

fn batch_output(graph: &str, result: &GraphBatchResult) -> Result<Value> {
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
                _ => return Err(internal("unsupported batch outcome")),
            })
        })
        .collect::<Result<Vec<Value>>>()?;
    Ok(json!({
        "graph": graph,
        "revision": result.revision.map(|revision| revision.to_string()),
        "outcomes": outcomes,
    }))
}

pub(super) fn execute(state: &mut State, handle: u64, request: Value) -> Result<Value> {
    let request: Request =
        serde_json::from_value(request).map_err(|error| ("invalid_input", error.to_string()))?;
    if let Request::Create {
        path,
        dimensions,
        options,
    } = request
    {
        // Checked before the file is created.
        if handle != 0 {
            return Err(invalid("graph create requires handle 0"));
        }
        let (options, durability) = features::create_options(options)?;
        let db = Database::create_with_graph(path, dimensions, options)
            .and_then(|db| db.with_durability(durability))
            .map_err(graph_failure)?;
        return features::insert(state, handle, db);
    }
    let db = state
        .databases
        .get_mut(&handle)
        .ok_or_else(|| ("invalid_handle", "Unknown or closed handle".into()))?;
    match request {
        Request::IsGraphEnabled {} => Ok(json!({"graph_enabled": db.is_graph_enabled()})),
        Request::ListGraphs {} => {
            let graphs: Value = db
                .list_graphs()
                .map_err(graph_failure)?
                .iter()
                .map(info)
                .collect();
            Ok(json!({"graphs": graphs}))
        }
        Request::CreateGraph { graph } => {
            let created = db.create_graph(&graph).map_err(graph_failure)?;
            Ok(json!({"graph": info(&created)}))
        }
        Request::DropGraph { graph } => {
            db.drop_graph(&graph).map_err(graph_failure)?;
            Ok(json!({"dropped": graph}))
        }
        Request::Batch { graph, ops } => {
            let ops = ops.into_iter().map(graph_op).collect::<Result<Vec<_>>>()?;
            let result = db
                .graph_mut(&graph)
                .map_err(graph_failure)?
                .write_batch(&ops)
                .map_err(graph_failure)?;
            batch_output(&graph, &result)
        }
        Request::Explain { query } => {
            let request = read_request(*query)?;
            let plan = db
                .graph(&request.graph)
                .map_err(graph_failure)?
                .explain(&request.query, &request.options)
                .map_err(graph_failure)?;
            explanation(&request.graph, &plan)
        }
        Request::Create { .. } => unreachable!(),
        query => {
            let request = read_request(query)?;
            let read = db
                .graph(&request.graph)
                .map_err(graph_failure)?
                .query(&request.query, &request.options)
                .map_err(graph_failure)?;
            read_output(&request.graph, &read)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{vlite_execute, vlite_response_free};
    use serde_json::{Value, json};

    fn call(handle: u64, value: Value) -> Value {
        let request = value.to_string();
        unsafe {
            let response = vlite_execute(handle, request.as_ptr(), request.len());
            let value =
                serde_json::from_slice(std::ffi::CStr::from_ptr(response).to_bytes()).unwrap();
            vlite_response_free(response);
            value
        }
    }

    fn graph(handle: u64, request: Value) -> Value {
        call(handle, json!({"op": "graph", "request": request}))
    }

    fn ok(handle: u64, request: Value) -> Value {
        let response = graph(handle, request.clone());
        assert_eq!(response["ok"], true, "{request} -> {response}");
        response["result"].clone()
    }

    fn code(handle: u64, request: Value) -> String {
        let response = graph(handle, request.clone());
        assert_eq!(response["ok"], false, "{request} -> {response}");
        response["error"]["code"].as_str().unwrap().to_owned()
    }

    fn handle(result: &Value) -> u64 {
        result["handle"].as_str().unwrap().parse().unwrap()
    }

    fn close(handle: u64) {
        assert_eq!(call(handle, json!({"op": "close"}))["ok"], true);
    }

    #[test]
    fn capabilities_report_graph() {
        let response = call(0, json!({"op": "capabilities"}));
        assert_eq!(response["result"]["graph"], true, "{response}");
    }

    #[test]
    fn graph_commands_create_write_read_explain_and_persist() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("graph.vlite");
        let db = handle(&ok(
            0,
            json!({"op": "create", "path": path, "dimensions": 2}),
        ));
        assert_eq!(
            code(db, json!({"op": "create", "path": path, "dimensions": 2})),
            "invalid_input"
        );
        assert_eq!(
            ok(db, json!({"op": "is_graph_enabled"})),
            json!({"graph_enabled": true})
        );
        assert_eq!(ok(db, json!({"op": "list_graphs"})), json!({"graphs": []}));
        assert_eq!(
            ok(db, json!({"op": "create_graph", "graph": "g"})),
            json!({"graph": {"name": "g", "node_count": 0, "edge_count": 0}})
        );
        assert_eq!(
            code(db, json!({"op": "create_graph", "graph": "g"})),
            "graph_exists"
        );
        assert_eq!(
            code(db, json!({"op": "create_graph", "graph": "Bad"})),
            "invalid_graph_name"
        );
        assert_eq!(
            call(db, json!({"op": "put", "id": "7", "vector": [1, 0]}))["ok"],
            true
        );

        let person = |name: &str| json!({"name": {"type": "string", "value": name}});
        let batch = ok(
            db,
            json!({"op": "batch", "graph": "g", "ops": [
                {"op": "create_node", "key": "a", "labels": ["Person"], "properties": person("Ada")},
                {"op": "create_node", "key": "b", "labels": ["Person"], "properties": person("Bob")},
                {"op": "create_node", "key": "c", "labels": ["City"]},
                {"op": "create_edge", "key": "ab", "source": "a", "relationship": "KNOWS", "destination": "b"},
                {"op": "create_edge", "source": "b", "relationship": "LIVES_IN", "destination": "c"},
                {"op": "attach", "node": "a", "collection": "default", "record": "7"},
                {"op": "put_vector", "collection": "default", "id": 8, "vector": [0, 1], "payload_base64": "AP8="}
            ]}),
        );
        assert_eq!(batch["graph"], "g");
        assert!(batch["revision"].as_str().unwrap().parse::<u64>().is_ok());
        let outcomes = batch["outcomes"].as_array().unwrap();
        assert_eq!(outcomes.len(), 7);
        assert_eq!(outcomes[0]["node"]["created"], true);
        assert_eq!(outcomes[6], json!("vector"));
        assert_eq!(
            call(db, json!({"op": "get", "id": "8"}))["result"]["payload_base64"],
            "AP8="
        );
        let node_a = outcomes[0]["node"]["id"].as_str().unwrap().to_owned();

        // Atomic: one failing op writes nothing.
        assert_eq!(
            code(
                db,
                json!({"op": "batch", "graph": "g", "ops": [
                    {"op": "create_node", "key": "d"},
                    {"op": "create_node", "key": "a"}
                ]})
            ),
            "graph_key_exists"
        );
        assert_eq!(
            ok(
                db,
                json!({"op": "get_node", "graph": "g", "node": {"key": "d"}})
            )["node"],
            Value::Null
        );
        assert_eq!(
            code(
                db,
                json!({"op": "batch", "graph": "g", "ops": [{"op": "create_node", "key": "x", "extra": 1}]})
            ),
            "invalid_input"
        );
        assert_eq!(
            code(
                db,
                json!({"op": "batch", "graph": "g", "ops": [{"op": "attach", "node": "b", "collection": "default", "record": "07"}]})
            ),
            "invalid_input"
        );

        let got = ok(
            db,
            json!({"op": "get_node", "graph": "g", "node": {"id": node_a}}),
        );
        assert_eq!(got["node"]["key"], "a");
        assert_eq!(got["node"]["properties"], person("Ada"));
        assert_eq!(got["complete"], true);
        assert!(got["view_sequence"].is_string());
        let projected = ok(
            db,
            json!({"op": "get_node", "graph": "g", "node": {"key": "a"}, "projection": "none"}),
        );
        assert_eq!(projected["node"]["properties"], json!({}));
        let edge = ok(
            db,
            json!({"op": "get_edge", "graph": "g", "edge": {"key": "ab"}}),
        );
        assert_eq!(edge["edge"]["relationship"], "KNOWS");
        assert_eq!(edge["edge"]["target_key"], "b");

        let page = ok(
            db,
            json!({"op": "find_nodes", "graph": "g", "label": "Person", "limit": 1}),
        );
        assert_eq!(page["nodes"].as_array().unwrap().len(), 1);
        let cursor = page["cursor"].as_str().unwrap().to_owned();
        let next = ok(
            db,
            json!({"op": "find_nodes", "graph": "g", "label": "Person", "limit": 1, "cursor": cursor}),
        );
        assert_eq!(next["nodes"].as_array().unwrap().len(), 1);
        assert_ne!(next["nodes"][0]["key"], page["nodes"][0]["key"]);
        assert_eq!(
            code(
                db,
                json!({"op": "find_nodes", "graph": "g", "cursor": "XYZ"})
            ),
            "invalid_input"
        );

        let neighbors = ok(
            db,
            json!({"op": "neighbors", "graph": "g", "node": {"key": "b"}, "direction": "both"}),
        );
        assert_eq!(neighbors["edges"].as_array().unwrap().len(), 2);
        let traversal = ok(
            db,
            json!({"op": "traverse", "graph": "g", "start": [{"key": "a"}], "include_paths": true,
                   "limits": {"max_hops": 2}}),
        );
        assert_eq!(traversal["nodes"].as_array().unwrap().len(), 3);
        assert!(!traversal["paths"].as_array().unwrap().is_empty());
        assert_eq!(
            code(
                db,
                json!({"op": "traverse", "graph": "g", "start": [{"key": "a"}],
                            "limits": {"max_hops": 1000000}})
            ),
            "graph_limit_exceeds_ceiling"
        );
        let shortest = ok(
            db,
            json!({"op": "shortest_path", "graph": "g", "source": {"key": "a"}, "destination": {"key": "c"}}),
        );
        assert_eq!(shortest["shortest_path"]["outcome"], "found");
        assert_eq!(
            shortest["shortest_path"]["path"]["edges"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let none = ok(
            db,
            json!({"op": "shortest_path", "graph": "g", "source": {"key": "c"}, "destination": {"key": "a"}}),
        );
        assert_eq!(none["shortest_path"]["outcome"], "no_path_within_hops");

        let explained = ok(
            db,
            json!({"op": "explain", "query": {"op": "neighbors", "graph": "g", "node": {"key": "a"}, "types": ["KNOWS"]}}),
        );
        assert_eq!(explained["explain"]["query"]["op"], "neighbors");
        assert_eq!(explained["explain"]["projection"], "all");
        assert!(
            !explained["explain"]["indexes"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            code(db, json!({"op": "explain", "query": {"op": "list_graphs"}})),
            "invalid_input"
        );

        assert_eq!(
            code(db, json!({"op": "drop_graph", "graph": "g"})),
            "graph_not_empty"
        );
        assert_eq!(
            code(
                db,
                json!({"op": "get_node", "graph": "nope", "node": {"key": "a"}})
            ),
            "graph_not_found"
        );
        assert_eq!(
            code(
                db,
                json!({"op": "get_node", "graph": "g", "node": {"key": "a"}, "x": 1})
            ),
            "invalid_input"
        );
        assert_eq!(code(db, json!({"op": "unknown"})), "invalid_input");
        assert_eq!(
            ok(db, json!({"op": "create_graph", "graph": "empty"}))["graph"]["name"],
            "empty"
        );
        assert_eq!(
            ok(db, json!({"op": "drop_graph", "graph": "empty"})),
            json!({"dropped": "empty"})
        );
        close(db);
        assert_eq!(code(db, json!({"op": "list_graphs"})), "invalid_handle");

        // Read-only handles query; writes fail with read_only.
        let reader =
            handle(&call(0, json!({"op": "open", "path": path, "read_only": true}))["result"]);
        let graphs = ok(reader, json!({"op": "list_graphs"}));
        assert_eq!(
            graphs["graphs"][0],
            json!({"name": "g", "node_count": 3, "edge_count": 2})
        );
        assert_eq!(
            code(
                reader,
                json!({"op": "batch", "graph": "g", "ops": [{"op": "create_node", "key": "z"}]})
            ),
            "read_only"
        );
        close(reader);
    }

    #[test]
    fn vector_only_files_and_bad_create_options_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("vectors.vlite");
        let db = handle(&call(0, json!({"op": "create", "path": path, "dimensions": 2}))["result"]);
        assert_eq!(
            ok(db, json!({"op": "is_graph_enabled"})),
            json!({"graph_enabled": false})
        );
        assert_eq!(code(db, json!({"op": "list_graphs"})), "graphs_not_enabled");
        assert_eq!(
            code(db, json!({"op": "create_graph", "graph": "g"})),
            "graphs_not_enabled"
        );
        close(db);
        let v4 = directory.path().join("v4.vlite");
        assert_eq!(
            code(
                0,
                json!({"op": "create", "path": v4, "dimensions": 2, "options": {"format": 4}})
            ),
            "graphs_require_v5"
        );
        assert!(!v4.exists());
        let timed = directory.path().join("timed.vlite");
        let db = handle(&ok(
            0,
            json!({"op": "create", "path": timed, "dimensions": 2, "options": {"commit_timestamps": true}}),
        ));
        assert_eq!(
            ok(db, json!({"op": "is_graph_enabled"})),
            json!({"graph_enabled": true})
        );
        close(db);
    }
}
