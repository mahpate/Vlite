//! `vlite-graph-cli`: one typed graph operation per invocation (FOLLOWUP-001).
//!
//! Source-only and opt-in: it requires the non-default `graph` and `json`
//! features, so no distributed artifact builds it. Every command maps onto an
//! existing `vlite::property_graph` call; there is no query language. Reads
//! open the file with `Database::open_read_only` (shared lock, never writes);
//! writes need `--writable` and take the exclusive writer lock for the one
//! command. Success prints one JSON object on stdout; failure prints
//! `{"error":{"code","message"}}` on stderr and exits nonzero. Node, edge and
//! record IDs, revisions and sequences are printed as decimal strings and
//! accepted as strings or JSON numbers, so u64 values never lose precision.

use std::io::{self, Read};
use std::process::ExitCode;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value, json};
use vlite::property_graph::{
    Direction, EdgeRef, EdgeView, Explanation, FindNodes, GraphBatchResult, GraphInfo, GraphLimits,
    GraphOp, GraphOpOutcome, GraphOutput, GraphPath, GraphQuery, GraphRead, GraphReadStats,
    GraphWrite, IndexChoice, Neighbors, NodeFilter, NodePatch, NodeSelector, NodeView, PathOutcome,
    PropertyProjection, ReadOptions, ShortestPath, Traverse, TruncationReason,
};
use vlite::{
    Condition, CreateOptions, Database, Error, Expected, Metadata, MetadataValue, Mutation,
};

const HELP: &str = r#"vlite-graph-cli --db PATH [--writable] [--explain] COMMAND [REQUEST]
Runs one typed graph operation on a graph-enabled VLite file and prints one
JSON object. REQUEST is a JSON object; "-" reads it from stdin; omitted is {}.

Reads (shared lock, never write; --explain plans a query without running it):
  graphs
  get-node      {"graph":"g","node":{"key":"a"}}            node: {"key"} | {"id"}
  get-edge      {"graph":"g","edge":{"id":"0"}}             edge: {"key"} | {"id"}
  find-nodes    {"graph":"g","label":"L","conditions":[],"limit":N,"cursor":"hex"}
  neighbors     {"graph":"g","node":{...},"direction":"out|in|both","types":["T"],
                 "edge_conditions":[],"limit":N,"cursor":"hex"}
  traverse      {"graph":"g","start":[{...}],"direction":"out","types":["T"],
                 "expand_filter":{"labels":[],"conditions":[]},"return_filter":{...},
                 "edge_filter":[],"include_paths":false}
  shortest-path {"graph":"g","source":{...},"destination":{...},"direction":"out","types":["T"]}
  Every query also accepts "limits":{"max_hops":2,...} (above the ceiling is an
  error, never clamped) and "projection":"all"|"none"|{"only":["field"]}.

Writes (require --writable; exclusive lock):
  init          {"dimensions":N}   creates a NEW graph-enabled file; never converts
  create-graph  {"graph":"g"}
  drop-graph    {"graph":"g"}      empty graphs only
  batch         {"graph":"g","ops":[...]}   one atomic frame; nothing is written on error
    ops: create_node, upsert_node, patch_node, delete_node, create_edge,
         patch_edge, delete_edge, attach, detach, put_vector, delete_vector

Conditions and metadata use the engine JSON of vlite-shell. Vector-only files
are refused (graphs_not_enabled); convert a copy with vlite-maintain enable-graph.
"#;

/// Requests read from stdin are bounded; the batch frame itself is limited
/// to 4,096 operations and 16 MiB by the library.
const MAX_REQUEST_BYTES: u64 = 64 * 1024 * 1024;

const READS: &[&str] = &[
    "graphs",
    "get-node",
    "get-edge",
    "find-nodes",
    "neighbors",
    "traverse",
    "shortest-path",
];
const WRITES: &[&str] = &["init", "create-graph", "drop-graph", "batch"];

struct Failure {
    code: &'static str,
    message: String,
}

impl Failure {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            code: "usage",
            message: message.into(),
        }
    }

    fn request(message: impl Into<String>) -> Self {
        Self {
            code: "invalid_request",
            message: message.into(),
        }
    }

    fn internal(message: &str) -> Self {
        Self {
            code: "internal_error",
            message: message.into(),
        }
    }
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Self {
            code: error_code(&error),
            message: error.to_string(),
        }
    }
}

/// Stable codes: graph variants use the REST/gRPC names (`server::graph_code`).
fn error_code(error: &Error) -> &'static str {
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
        _ => "database_error",
    }
}

// Command line.

struct Cli {
    db: String,
    writable: bool,
    explain: bool,
    command: String,
    request: Option<String>,
}

fn parse_args(args: Vec<String>) -> Result<Cli, Failure> {
    let (mut db, mut writable, mut explain) = (None, false, false);
    let mut positional = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let seen = match arg.as_str() {
            "--db" => {
                let value = args
                    .next()
                    .ok_or_else(|| Failure::usage("missing value for --db"))?;
                db.replace(value).is_some()
            }
            "--writable" => std::mem::replace(&mut writable, true),
            "--explain" => std::mem::replace(&mut explain, true),
            option if option.starts_with("--") => {
                return Err(Failure::usage(format!(
                    "unknown option {option}; use --help"
                )));
            }
            _ => {
                positional.push(arg);
                false
            }
        };
        if seen {
            return Err(Failure::usage("duplicate option"));
        }
    }
    let db = db.ok_or_else(|| Failure::usage("missing --db; use --help"))?;
    let mut positional = positional.into_iter();
    let command = positional
        .next()
        .ok_or_else(|| Failure::usage("missing COMMAND; use --help"))?;
    let request = positional.next();
    if positional.next().is_some() {
        return Err(Failure::usage("expected COMMAND and at most one REQUEST"));
    }
    let is_read = READS.contains(&command.as_str());
    if !is_read && !WRITES.contains(&command.as_str()) {
        return Err(Failure::usage(format!(
            "unknown command {command}; use --help"
        )));
    }
    if !is_read && !writable {
        return Err(Failure::usage(format!("{command} writes; pass --writable")));
    }
    if is_read && writable {
        return Err(Failure::usage(format!(
            "{command} only reads; --writable is for write commands"
        )));
    }
    if explain && (!is_read || command == "graphs") {
        return Err(Failure::usage("--explain applies to query commands only"));
    }
    Ok(Cli {
        db,
        writable,
        explain,
        command,
        request,
    })
}

fn request_text(request: Option<&str>) -> Result<String, Failure> {
    match request {
        None => Ok("{}".into()),
        Some("-") => {
            let mut text = String::new();
            io::stdin()
                .take(MAX_REQUEST_BYTES + 1)
                .read_to_string(&mut text)
                .map_err(|error| Failure::request(format!("cannot read stdin: {error}")))?;
            if text.len() as u64 > MAX_REQUEST_BYTES {
                return Err(Failure::request("request exceeds 64 MiB"));
            }
            Ok(text)
        }
        Some(text) => Ok(text.into()),
    }
}

fn parse<T: for<'de> Deserialize<'de>>(text: &str) -> Result<T, Failure> {
    serde_json::from_str(text).map_err(|error| Failure::request(error.to_string()))
}

// Request JSON. Every object rejects unknown fields.

/// Canonical decimal string or non-negative JSON integer (as `vlite-mcp`).
fn parse_u64(value: &Value) -> Result<u64, String> {
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

fn de_u64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    parse_u64(&Value::deserialize(deserializer)?).map_err(D::Error::custom)
}

/// `Metadata` that rejects a repeated key instead of keeping the last value.
struct Properties(Metadata);

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

fn de_properties<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Metadata, D::Error> {
    Properties::deserialize(deserializer).map(|properties| properties.0)
}

fn de_optional_properties<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Metadata>, D::Error> {
    Option::<Properties>::deserialize(deserializer)
        .map(|found| found.map(|properties| properties.0))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Init {
    dimensions: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Named {
    graph: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Batch {
    graph: String,
    ops: Vec<OpIn>,
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
        }
    }
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

fn cursor_bytes(cursor: Option<String>) -> Result<Option<Vec<u8>>, Failure> {
    let Some(text) = cursor else {
        return Ok(None);
    };
    let invalid = || Failure::request("cursor must be the lowercase hex string a read returned");
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
        .collect::<Result<Vec<u8>, Failure>>()
        .map(Some)
}

struct ReadRequest {
    graph: String,
    query: GraphQuery,
    limits: GraphLimits,
    projection: PropertyProjection,
}

fn read_request(command: &str, text: &str) -> Result<ReadRequest, Failure> {
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
        _ => return Err(Failure::internal("not a query command")),
    };
    Ok(ReadRequest {
        graph,
        query,
        limits: limits.resolve(),
        projection: match projection {
            ProjectionIn::All => PropertyProjection::All,
            ProjectionIn::None => PropertyProjection::None,
            ProjectionIn::Only(fields) => PropertyProjection::Only(fields),
        },
    })
}

// Output JSON: u64 IDs, revisions and sequences as decimal strings; counts
// and limits (bounded by the read ceilings) as numbers; enums in serde's
// external tagging (`"name"` or `{"name":{...}}`).

fn ids(values: &[u64]) -> Value {
    values.iter().map(u64::to_string).collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn metadata(metadata: &Metadata) -> Value {
    serde_json::to_value(metadata).expect("validated metadata serializes")
}

fn node(node: &NodeView) -> Value {
    json!({
        "id": node.id.to_string(),
        "key": node.key,
        "labels": node.labels,
        "properties": metadata(&node.properties),
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
        "properties": metadata(&edge.properties),
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

/// A normalized query in its request form, with the command as `operation`.
fn query(query: &GraphQuery) -> Result<Value, Failure> {
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
        _ => return Err(Failure::internal("unsupported query kind in explanation")),
    })
}

fn explanation(graph: &str, explanation: &Explanation) -> Result<Value, Failure> {
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

fn read_output(graph: &str, read: &GraphRead<GraphOutput>) -> Result<Value, Failure> {
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
        _ => return Err(Failure::internal("unsupported read output")),
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

fn batch_output(graph: &str, result: &GraphBatchResult) -> Result<Value, Failure> {
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
                _ => return Err(Failure::internal("unsupported batch outcome")),
            })
        })
        .collect::<Result<Vec<Value>, Failure>>()?;
    Ok(json!({
        "graph": graph,
        "revision": result.revision.map(|revision| revision.to_string()),
        "outcomes": outcomes,
    }))
}

// Commands. Requests are parsed before the file is opened.

fn execute(cli: &Cli) -> Result<Value, Failure> {
    let text = request_text(cli.request.as_deref())?;
    debug_assert_eq!(cli.writable, WRITES.contains(&cli.command.as_str()));
    match cli.command.as_str() {
        "init" => {
            let request: Init = parse(&text)?;
            Database::create_with_graph(&cli.db, request.dimensions, CreateOptions::default())?
                .close()?;
            Ok(json!({"dimensions": request.dimensions, "graph_enabled": true}))
        }
        "create-graph" => {
            let request: Named = parse(&text)?;
            let mut db = Database::open(&cli.db)?;
            let created = db.create_graph(&request.graph)?;
            db.close()?;
            Ok(json!({"graph": info(&created)}))
        }
        "drop-graph" => {
            let request: Named = parse(&text)?;
            let mut db = Database::open(&cli.db)?;
            db.drop_graph(&request.graph)?;
            db.close()?;
            Ok(json!({"dropped": request.graph}))
        }
        "batch" => {
            let request: Batch = parse(&text)?;
            let ops: Vec<GraphOp> = request.ops.into_iter().map(Into::into).collect();
            let mut db = Database::open(&cli.db)?;
            let result = db.graph_mut(&request.graph)?.write_batch(&ops)?;
            db.close()?;
            batch_output(&request.graph, &result)
        }
        "graphs" => {
            let _: Empty = parse(&text)?;
            let db = Database::open_read_only(&cli.db)?;
            let graphs: Value = db.list_graphs()?.iter().map(info).collect();
            Ok(json!({"graphs": graphs}))
        }
        command => {
            let request = read_request(command, &text)?;
            let db = Database::open_read_only(&cli.db)?;
            let graph = db.graph(&request.graph)?;
            let options = ReadOptions {
                limits: request.limits,
                projection: request.projection,
                cancel: None,
            };
            if cli.explain {
                explanation(&request.graph, &graph.explain(&request.query, &options)?)
            } else {
                read_output(&request.graph, &graph.query(&request.query, &options)?)
            }
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help") {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    match parse_args(args).and_then(|cli| execute(&cli)) {
        Ok(value) => {
            println!("{value}");
            ExitCode::SUCCESS
        }
        Err(failure) => {
            eprintln!(
                "{}",
                json!({"error": {"code": failure.code, "message": failure.message}})
            );
            ExitCode::FAILURE
        }
    }
}
