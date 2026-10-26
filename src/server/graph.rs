//! EPIC-011 (ITEM-040, ITEM-041): REST property-graph routes, compiled only
//! with `graph`.
//!
//! Request and response JSON mirror `vlite-graph-cli` (the graph name comes
//! from the path instead of the body): u64 IDs, revisions and sequences are
//! decimal strings on output and decimal strings or JSON integers on input,
//! unknown fields are rejected, and error codes are the REST graph names
//! (`graph_code`). Graph errors are never 500.
//!
//! # Authorization model (GATE-007)
//!
//! Every graph route is authenticated by the shared `observe` middleware and
//! then authorized in two steps:
//!
//! 1. Route level (`permission` → `route_op`): reads (list, info, get node /
//!    edge, find nodes, neighbors, traverse, shortest path, explain) need
//!    `graph_read`; graph creation, drop and batch writes need `graph_write`.
//!    The admin token may do everything. The read-only token may use the
//!    read routes only (`read_only` otherwise). A scoped credential needs the
//!    operation in its `operations` and the database lineage it was issued
//!    for. Collection scope never implies graph scope: a credential without
//!    `graph_read` / `graph_write` gets 403 `forbidden_scope` even with
//!    `collections: ["*"]`.
//! 2. Handler level: the addressed graph name must be in the credential's
//!    `graphs` binding (`["*"]` or exact names) before anything is looked up,
//!    so out-of-scope callers cannot probe which graphs exist; `GET
//!    /v1/graphs` lists only bound graphs. Batch operations that touch vector
//!    records (`attach`, `detach`, `put_vector`, `delete_vector`) also need
//!    the `write` operation and a collection binding covering every such
//!    collection, checked under the write guard before the batch runs, and
//!    live vector quotas apply as on the vector write routes.
//!
//! Wave-1 read outputs carry graph structure only (nodes, edges, paths,
//! properties) and never vector record data, so REQ-043 per-hop evidence
//! redaction has nothing to redact here; the `redacted` counts belong to the
//! wave-2 retrieve route that returns attached records.
//!
//! Reads run under one database read guard, writes under the write guard;
//! the time each guard is held is recorded (`GET /v1/admin/graph/stats`,
//! admin only).
use super::*;
use crate::property_graph::{
    Direction, EdgeRef, EdgeView, Explanation, FindNodes, GraphBatchResult, GraphInfo, GraphLimits,
    GraphOp, GraphOpOutcome, GraphOutput, GraphPath, GraphQuery, GraphRead, GraphReadStats,
    GraphWrite, IndexChoice, Neighbors, NodeFilter, NodePatch, NodeSelector, NodeView, PathOutcome,
    PropertyProjection, ReadOptions, ShortestPath, Traverse, TruncationReason,
};
use crate::{Condition, Metadata, MetadataValue};
use axum::{body::Bytes, extract::rejection::BytesRejection, routing::MethodRouter};
use serde::{Deserializer, de::Error as _};
use serde_json::Map;
use std::time::Duration;

/// Read request bodies are small queries; batches keep `MAX_JSON_BYTES`.
const MAX_READ_BODY: usize = 1024 * 1024;
/// The library's batch frame limit, checked before any lock is taken.
const MAX_BATCH_OPS: usize = 4096;
/// Hold-time samples kept per guard (the most recent ones).
const SAMPLES: usize = 4096;

#[derive(Clone, Copy)]
enum Kind {
    GetNode,
    GetEdge,
    FindNodes,
    Neighbors,
    Traverse,
    ShortestPath,
}

const QUERIES: [(&str, &str, Kind); 6] = [
    ("/v1/graphs/{graph}/nodes/get", "get-node", Kind::GetNode),
    ("/v1/graphs/{graph}/edges/get", "get-edge", Kind::GetEdge),
    (
        "/v1/graphs/{graph}/nodes/find",
        "find-nodes",
        Kind::FindNodes,
    ),
    ("/v1/graphs/{graph}/neighbors", "neighbors", Kind::Neighbors),
    ("/v1/graphs/{graph}/traverse", "traverse", Kind::Traverse),
    (
        "/v1/graphs/{graph}/shortest-path",
        "shortest-path",
        Kind::ShortestPath,
    ),
];
const EXPLAIN: &str = "/v1/graphs/{graph}/explain";

/// The credential operation a graph route needs; `None` for other routes.
pub(super) fn route_op(method: &Method, route: &str) -> Option<credentials::Op> {
    if read_post(method, route) {
        return Some(credentials::Op::GraphRead);
    }
    match (method.as_str(), route) {
        ("GET" | "HEAD", "/v1/graphs" | "/v1/graphs/{graph}") => Some(credentials::Op::GraphRead),
        ("POST", "/v1/graphs" | "/v1/graphs/{graph}/batch") | ("DELETE", "/v1/graphs/{graph}") => {
            Some(credentials::Op::GraphWrite)
        }
        _ => None,
    }
}

/// POST graph routes that only read (allowed for the read-only token).
pub(super) fn read_post(method: &Method, route: &str) -> bool {
    method == Method::POST && (route == EXPLAIN || QUERIES.iter().any(|(r, _, _)| *r == route))
}

pub(super) fn routes() -> Router<Service> {
    let mut reads = Router::new().route(EXPLAIN, post(explain));
    for (route, _, kind) in QUERIES {
        reads = reads.route(route, query_route(kind));
    }
    Router::new()
        .route("/v1/graphs", get(list).post(create))
        .route("/v1/graphs/{graph}", get(info_route).delete(drop_route))
        .route("/v1/graphs/{graph}/batch", post(batch))
        .route("/v1/admin/graph/stats", get(guard_stats))
        .merge(reads.layer(DefaultBodyLimit::max(MAX_READ_BODY)))
}

// Guard hold time.

#[derive(Default)]
struct Ring {
    count: u64,
    samples: VecDeque<u64>,
}

impl Ring {
    fn push(&mut self, held: Duration) {
        self.count += 1;
        if self.samples.len() == SAMPLES {
            self.samples.pop_front();
        }
        self.samples
            .push_back(held.as_micros().min(u128::from(u64::MAX)) as u64);
    }

    fn json(&self) -> Value {
        let mut sorted: Vec<u64> = self.samples.iter().copied().collect();
        sorted.sort_unstable();
        // Nearest rank: the smallest sample with at least p% at or below it.
        let rank = |p: usize| {
            (!sorted.is_empty()).then(|| sorted[(sorted.len() * p).div_ceil(100).max(1) - 1])
        };
        json!({"count": self.count, "samples": sorted.len(), "p50_us": rank(50),
            "p99_us": rank(99), "max_us": sorted.last()})
    }
}

/// Read- and write-guard hold times of graph requests, plus truncation
/// counts by reason (PRD Section 8 metrics, exposed to the admin only).
#[derive(Default)]
pub(super) struct GuardStats {
    inner: Mutex<GuardInner>,
}

#[derive(Default)]
struct GuardInner {
    read: Ring,
    write: Ring,
    truncations: BTreeMap<&'static str, u64>,
}

impl GuardStats {
    fn read(&self, held: Duration, truncation: &[TruncationReason]) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.read.push(held);
            for reason in truncation {
                *inner.truncations.entry(reason_name(reason)).or_default() += 1;
            }
        }
    }

    fn write(&self, held: Duration) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.write.push(held);
        }
    }
}

fn reason_name(reason: &TruncationReason) -> &'static str {
    match reason {
        TruncationReason::VisitedLimit => "visited_limit",
        TruncationReason::EdgeLimit => "edge_limit",
        TruncationReason::DegreeLimit { .. } => "degree_limit",
        TruncationReason::OutputLimit => "output_limit",
        TruncationReason::Deadline => "deadline",
        TruncationReason::Cancelled => "cancelled",
    }
}

async fn guard_stats(State(service): State<Service>) -> Result<Json<Value>, ApiError> {
    let inner = service.graph_stats.inner.lock().map_err(internal)?;
    Ok(Json(json!({
        "read_guard": inner.read.json(),
        "write_guard": inner.write.json(),
        "requests": {"read": inner.read.count, "write": inner.write.count},
        "truncations": inner.truncations,
    })))
}

/// Runs `read` under the read guard, timing only the guarded section.
async fn guarded_read<T: Send + 'static>(
    service: &Service,
    read: impl FnOnce(&Database) -> Result<T, ApiError> + Send + 'static,
    truncation: fn(&T) -> &[TruncationReason],
) -> Result<T, ApiError> {
    let stats = service.graph_stats.clone();
    service
        .run_read(move |db| {
            let started = Instant::now();
            let result = db
                .ensure_healthy()
                .map_err(ApiError::from)
                .and_then(|()| read(db));
            let held = started.elapsed();
            match &result {
                Ok(value) => stats.read(held, truncation(value)),
                Err(_) => stats.read(held, &[]),
            }
            result
        })
        .await
}

/// Runs `write` under the write guard, timing only the guarded section.
async fn guarded_write<T: Send + 'static>(
    service: &Service,
    write: impl FnOnce(&mut Database) -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    let stats = service.graph_stats.clone();
    service
        .run(move |db| {
            let started = Instant::now();
            let result = db
                .ensure_healthy()
                .map_err(ApiError::from)
                .and_then(|()| write(db));
            stats.write(started.elapsed());
            result
        })
        .await
}

fn untruncated<T>(_: &T) -> &[TruncationReason] {
    &[]
}

// Scope and request plumbing.

/// Name binding check, before any lookup (no existence probing).
fn scope(role: &admin::Role, graph: &str) -> Result<(), ApiError> {
    match role {
        admin::Role::Scoped(grant) if !grant.graph_allowed(graph) => Err(forbidden_scope()),
        _ => Ok(()),
    }
}

/// Vector-only files refuse every graph route before the lock is taken.
fn enabled(service: &Service) -> Result<(), ApiError> {
    if service.graph_enabled {
        Ok(())
    } else {
        Err(Error::GraphsNotEnabled.into())
    }
}

fn graph_path(input: Result<Path<String>, PathRejection>) -> Result<String, ApiError> {
    input
        .map(|Path(name)| name)
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, "invalid_path", error.body_text()))
}

/// Parses a JSON object body (empty means `{}`): syntax errors are 400 and
/// shape errors 422, both `invalid_json`, as the `Json` extractor reports.
fn parse<T: serde::de::DeserializeOwned>(
    input: Result<Bytes, BytesRejection>,
) -> Result<T, ApiError> {
    let bytes =
        input.map_err(|error| ApiError::new(error.status(), "invalid_json", error.body_text()))?;
    let bytes: &[u8] = if bytes.iter().all(u8::is_ascii_whitespace) {
        b"{}"
    } else {
        &bytes
    };
    serde_json::from_slice(bytes).map_err(json_error)
}

fn json_error(error: serde_json::Error) -> ApiError {
    let status = match error.classify() {
        serde_json::error::Category::Data => StatusCode::UNPROCESSABLE_ENTITY,
        _ => StatusCode::BAD_REQUEST,
    };
    ApiError::new(status, "invalid_json", error.to_string())
}

// Routes.

async fn list(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
) -> Result<Json<Value>, ApiError> {
    enabled(&service)?;
    let graphs = guarded_read(&service, |db| Ok(db.list_graphs()?), untruncated).await?;
    let graphs: Value = graphs
        .iter()
        .filter(|graph| scope(&role, &graph.name).is_ok())
        .map(info)
        .collect();
    Ok(Json(json!({"graphs": graphs})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Named {
    graph: String,
}

async fn create(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    input: Result<Bytes, BytesRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let Named { graph } = parse(input)?;
    scope(&role, &graph)?;
    enabled(&service)?;
    let created = guarded_write(&service, move |db| Ok(db.create_graph(&graph)?)).await?;
    Ok((StatusCode::CREATED, Json(json!({"graph": info(&created)}))))
}

async fn info_route(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    graph: Result<Path<String>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let graph = graph_path(graph)?;
    scope(&role, &graph)?;
    enabled(&service)?;
    let found = guarded_read(
        &service,
        move |db| Ok(db.graph(&graph)?.info()),
        untruncated,
    )
    .await?;
    Ok(Json(json!({"graph": info(&found)})))
}

async fn drop_route(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    graph: Result<Path<String>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let graph = graph_path(graph)?;
    scope(&role, &graph)?;
    enabled(&service)?;
    let name = graph.clone();
    guarded_write(&service, move |db| Ok(db.drop_graph(&name)?)).await?;
    Ok(Json(json!({"dropped": graph})))
}

fn query_route(kind: Kind) -> MethodRouter<Service> {
    post(
        move |State(service): State<Service>,
              Extension(role): Extension<admin::Role>,
              graph: Result<Path<String>, PathRejection>,
              input: Result<Bytes, BytesRejection>| async move {
            let graph = graph_path(graph)?;
            scope(&role, &graph)?;
            enabled(&service)?;
            let request = read_request(kind, parse(input)?)?;
            run_query(&service, graph, request).await
        },
    )
}

async fn run_query(
    service: &Service,
    graph: String,
    request: ReadRequest,
) -> Result<Json<Value>, ApiError> {
    let name = graph.clone();
    let read = guarded_read(
        service,
        move |db| {
            let options = ReadOptions {
                limits: request.limits,
                projection: request.projection,
                cancel: None,
            };
            Ok(db.graph(&name)?.query(&request.query, &options)?)
        },
        |read: &GraphRead<GraphOutput>| &read.truncation,
    )
    .await?;
    read_output(&graph, &read).map(Json)
}

async fn explain(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    graph: Result<Path<String>, PathRejection>,
    input: Result<Bytes, BytesRejection>,
) -> Result<Json<Value>, ApiError> {
    let graph = graph_path(graph)?;
    scope(&role, &graph)?;
    enabled(&service)?;
    let mut body: Map<String, Value> = parse(input)?;
    let operation = body.remove("operation");
    let kind = QUERIES
        .iter()
        .find(|(_, name, _)| operation.as_ref().and_then(Value::as_str) == Some(*name))
        .map(|(_, _, kind)| *kind)
        .ok_or_else(|| {
            unprocessable(
                "explain needs \"operation\": get-node, get-edge, find-nodes, neighbors, \
                 traverse or shortest-path",
            )
        })?;
    let request = read_request(kind, Value::Object(body))?;
    let name = graph.clone();
    let explained = guarded_read(
        &service,
        move |db| {
            let options = ReadOptions {
                limits: request.limits,
                projection: request.projection,
                cancel: None,
            };
            Ok(db.graph(&name)?.explain(&request.query, &options)?)
        },
        untruncated,
    )
    .await?;
    explanation(&graph, &explained).map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchIn {
    ops: Vec<OpIn>,
}

async fn batch(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    graph: Result<Path<String>, PathRejection>,
    input: Result<Bytes, BytesRejection>,
) -> Result<Json<Value>, ApiError> {
    let graph = graph_path(graph)?;
    scope(&role, &graph)?;
    enabled(&service)?;
    let BatchIn { ops } = parse(input)?;
    if ops.len() > MAX_BATCH_OPS {
        return Err(unprocessable(format!(
            "a batch holds at most {MAX_BATCH_OPS} operations"
        )));
    }
    let ops: Vec<GraphOp> = ops.into_iter().map(Into::into).collect();
    let name = graph.clone();
    let owner = service.clone();
    let result = guarded_write(&service, move |db| {
        authorize_records(&owner, db, &role, &name, &ops)?;
        Ok(db.graph_mut(&name)?.write_batch(&ops)?)
    })
    .await?;
    batch_output(&graph, &result).map(Json)
}

/// Record-touching operations need `write` plus a binding covering each
/// collection, and vector mutations are held to live quotas. `delete_node`
/// removes the node's attachments, so it is held to the same rule for every
/// collection attached to it. Runs under the write guard, so the checks see
/// the state the batch commits against.
fn authorize_records(
    service: &Service,
    db: &Database,
    role: &admin::Role,
    graph: &str,
    ops: &[GraphOp],
) -> Result<(), ApiError> {
    let mut mutations: BTreeMap<&str, Vec<Mutation>> = BTreeMap::new();
    for op in ops {
        let collections = match op {
            GraphOp::Attach { collection, .. } | GraphOp::Detach { collection, .. } => {
                vec![collection.clone()]
            }
            GraphOp::Vector {
                collection,
                mutation,
            } => {
                mutations
                    .entry(collection)
                    .or_default()
                    .push(mutation.clone());
                vec![collection.clone()]
            }
            GraphOp::DeleteNode { key, .. } if matches!(role, admin::Role::Scoped(_)) => {
                db.node_attachment_collections(graph, key)
            }
            _ => continue,
        };
        if let admin::Role::Scoped(grant) = role {
            if collections.is_empty() {
                continue;
            }
            if !grant.allows(credentials::Op::Write) {
                return Err(forbidden_scope());
            }
            for collection in collections {
                open(db, role, &Target::Named(collection))?;
            }
        }
    }
    for (collection, mutations) in mutations {
        // An unknown collection is reported by the batch itself.
        if let Ok(id) = db.resolve(collection) {
            enforce_quota(service, db, role, id, &quota_mutations(&mutations))?;
        }
    }
    Ok(())
}

// Request JSON (the `vlite-graph-cli` contract without the `graph` field).

/// Canonical decimal string or non-negative JSON integer.
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

/// Unset fields keep `GraphLimits::DEFAULT`; values above
/// `GraphLimits::CEILING` are rejected by the engine, never clamped.
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
    node: IdOrKey,
    #[serde(default)]
    limits: LimitsIn,
    #[serde(default)]
    projection: ProjectionIn,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetEdgeIn {
    edge: IdOrKey,
    #[serde(default)]
    limits: LimitsIn,
    #[serde(default)]
    projection: ProjectionIn,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FindNodesIn {
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

fn cursor_bytes(cursor: Option<String>) -> Result<Option<Vec<u8>>, ApiError> {
    let Some(text) = cursor else {
        return Ok(None);
    };
    let invalid = || unprocessable("cursor must be the lowercase hex string a read returned");
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
        .collect::<Result<Vec<u8>, ApiError>>()
        .map(Some)
}

struct ReadRequest {
    query: GraphQuery,
    limits: GraphLimits,
    projection: PropertyProjection,
}

fn from_value<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, ApiError> {
    serde_json::from_value(value).map_err(json_error)
}

fn read_request(kind: Kind, body: Value) -> Result<ReadRequest, ApiError> {
    let (query, limits, projection) = match kind {
        Kind::GetNode => {
            let r: GetNodeIn = from_value(body)?;
            (GraphQuery::GetNode(r.node.into()), r.limits, r.projection)
        }
        Kind::GetEdge => {
            let r: GetEdgeIn = from_value(body)?;
            (GraphQuery::GetEdge(r.edge.into()), r.limits, r.projection)
        }
        Kind::FindNodes => {
            let r: FindNodesIn = from_value(body)?;
            let query = GraphQuery::FindNodes(FindNodes {
                label: r.label,
                conditions: r.conditions,
                limit: r.limit,
                cursor: cursor_bytes(r.cursor)?,
            });
            (query, r.limits, r.projection)
        }
        Kind::Neighbors => {
            let r: NeighborsIn = from_value(body)?;
            let query = GraphQuery::Neighbors(Neighbors {
                node: r.node.into(),
                direction: r.direction.into(),
                types: r.types,
                edge_conditions: r.edge_conditions,
                limit: r.limit,
                cursor: cursor_bytes(r.cursor)?,
            });
            (query, r.limits, r.projection)
        }
        Kind::Traverse => {
            let r: TraverseIn = from_value(body)?;
            let query = GraphQuery::Traverse(Traverse {
                start: r.start.into_iter().map(Into::into).collect(),
                direction: r.direction.into(),
                types: r.types,
                expand_filter: r.expand_filter.into(),
                return_filter: r.return_filter.into(),
                edge_filter: r.edge_filter,
                include_paths: r.include_paths,
            });
            (query, r.limits, r.projection)
        }
        Kind::ShortestPath => {
            let r: ShortestPathIn = from_value(body)?;
            let query = GraphQuery::ShortestPath(ShortestPath {
                source: r.source.into(),
                destination: r.destination.into(),
                direction: r.direction.into(),
                types: r.types,
            });
            (query, r.limits, r.projection)
        }
    };
    Ok(ReadRequest {
        query,
        limits: limits.resolve(),
        projection: match projection {
            ProjectionIn::All => PropertyProjection::All,
            ProjectionIn::None => PropertyProjection::None,
            ProjectionIn::Only(fields) => PropertyProjection::Only(fields),
        },
    })
}

// Response JSON (identical to `vlite-graph-cli`).

fn ids(values: &[u64]) -> Value {
    values.iter().map(u64::to_string).collect()
}

fn metadata(metadata: &Metadata) -> Value {
    serde_json::to_value(metadata).unwrap_or(Value::Null)
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

fn truncation(reasons: &[TruncationReason]) -> Result<Value, ApiError> {
    reasons
        .iter()
        .map(|reason| {
            Ok(match reason {
                TruncationReason::DegreeLimit { node_ids } => {
                    json!({"degree_limit": {"node_ids": ids(node_ids)}})
                }
                TruncationReason::VisitedLimit
                | TruncationReason::EdgeLimit
                | TruncationReason::OutputLimit
                | TruncationReason::Deadline
                | TruncationReason::Cancelled => json!(reason_name(reason)),
            })
        })
        .collect()
}

fn indexes(choices: &[IndexChoice]) -> Result<Value, ApiError> {
    choices
        .iter()
        .map(|choice| {
            Ok(match choice {
                IndexChoice::NodeIdLookup => json!("node_id_lookup"),
                IndexChoice::NodeKeyIndex => json!("node_key_index"),
                IndexChoice::EdgeIdLookup => json!("edge_id_lookup"),
                IndexChoice::EdgeKeyIndex => json!("edge_key_index"),
                IndexChoice::LabelIndexScan => json!("label_index_scan"),
                IndexChoice::FullNodeScan => json!("full_node_scan"),
                IndexChoice::AdjacencyRangeScan {
                    direction: scan,
                    typed,
                } => {
                    json!({"adjacency_range_scan": {"direction": direction(*scan), "typed": typed}})
                }
                #[allow(unreachable_patterns)]
                _ => return Err(internal("unsupported index choice")),
            })
        })
        .collect()
}

fn stats(stats: &GraphReadStats) -> Result<Value, ApiError> {
    Ok(json!({
        "nodes_visited": stats.nodes_visited,
        "nodes_scanned": stats.nodes_scanned,
        "edges_examined": stats.edges_examined,
        "nodes_returned": stats.nodes_returned,
        "edges_returned": stats.edges_returned,
        "paths_returned": stats.paths_returned,
        "elapsed_ms": stats.elapsed_ms,
        "indexes": indexes(&stats.indexes)?,
    }))
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
    let value = |value| serde_json::to_value(value).unwrap_or(Value::Null);
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

/// A normalized query in its request form, with the operation name.
fn query(query: &GraphQuery) -> Result<Value, ApiError> {
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
        #[allow(unreachable_patterns)]
        _ => return Err(internal("unsupported query kind in explanation")),
    })
}

fn explanation(graph: &str, explanation: &Explanation) -> Result<Value, ApiError> {
    Ok(json!({
        "graph": graph,
        "explain": {
            "query": query(&explanation.query)?,
            "limits": limits(&explanation.limits),
            "projection": projection(&explanation.projection),
            "indexes": indexes(&explanation.indexes)?,
        },
    }))
}

fn read_output(graph: &str, read: &GraphRead<GraphOutput>) -> Result<Value, ApiError> {
    let mut out = Map::new();
    out.insert("graph".into(), json!(graph));
    out.insert(
        "view_sequence".into(),
        json!(read.view_sequence.to_string()),
    );
    out.insert("complete".into(), json!(read.complete));
    out.insert("truncation".into(), truncation(&read.truncation)?);
    out.insert("stats".into(), stats(&read.stats)?);
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
                    json!({"outcome": "exhausted", "truncation": truncation(cut)?})
                }
                #[allow(unreachable_patterns)]
                _ => return Err(internal("unsupported path outcome")),
            };
            out.insert("shortest_path".into(), outcome);
        }
        #[allow(unreachable_patterns)]
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

fn batch_output(graph: &str, result: &GraphBatchResult) -> Result<Value, ApiError> {
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
                #[allow(unreachable_patterns)]
                _ => return Err(internal("unsupported batch outcome")),
            })
        })
        .collect::<Result<Vec<Value>, ApiError>>()?;
    Ok(json!({
        "graph": graph,
        "revision": result.revision.map(|revision| revision.to_string()),
        "outcomes": outcomes,
    }))
}

// OpenAPI (RD-013): the served document is `src/openapi.json` with the
// fragment's path items inserted as whole lines before the last base path
// line; every other byte of the base document is unchanged.

pub(super) fn openapi() -> &'static str {
    static DOCUMENT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DOCUMENT.get_or_init(|| {
        merge_openapi(
            include_str!("../openapi.json"),
            include_str!("../openapi_graph.json"),
        )
        .unwrap_or_else(|| include_str!("../openapi.json").to_owned())
    })
}

fn merge_openapi(base: &str, fragment: &str) -> Option<String> {
    let fragment: Value = serde_json::from_str(fragment).ok()?;
    let newline = if base.contains("\r\n") { "\r\n" } else { "\n" };
    let end = base.find(&format!("{newline}  }},{newline}  \"components\""))?;
    let last = base[..end].rfind(newline)? + newline.len();
    let mut inserted = String::new();
    for (path, item) in fragment.get("paths")?.as_object()? {
        inserted.push_str(&format!(
            "    {}: {},{newline}",
            serde_json::to_string(path).ok()?,
            serde_json::to_string(item).ok()?
        ));
    }
    Some(format!("{}{inserted}{}", &base[..last], &base[last..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merged_openapi_parses_and_keeps_base_lines() {
        let merged = openapi();
        let document: Value = serde_json::from_str(merged).unwrap();
        assert!(document["paths"]["/v1/graphs/{graph}/traverse"]["post"].is_object());
        assert!(document["paths"]["/openapi.json"].is_object());
        let base = include_str!("../openapi.json");
        let stripped: String = merged
            .split_inclusive('\n')
            .filter(|line| {
                !line.starts_with("    \"/v1/graphs") && !line.starts_with("    \"/v1/admin/graph/")
            })
            .collect();
        assert_eq!(stripped, base);
        let crlf = base.replace("\r\n", "\n").replace('\n', "\r\n");
        let merged = merge_openapi(&crlf, include_str!("../openapi_graph.json")).unwrap();
        assert!(serde_json::from_str::<Value>(&merged).is_ok());
        assert!(!merged.replace("\r\n", "").contains('\n'));
    }

    #[test]
    fn every_graph_route_maps_to_a_graph_operation() {
        use credentials::Op;
        for (route, _, _) in QUERIES {
            assert!(matches!(
                route_op(&Method::POST, route),
                Some(Op::GraphRead)
            ));
        }
        assert!(matches!(
            route_op(&Method::POST, EXPLAIN),
            Some(Op::GraphRead)
        ));
        assert!(matches!(
            route_op(&Method::GET, "/v1/graphs/{graph}"),
            Some(Op::GraphRead)
        ));
        assert!(matches!(
            route_op(&Method::DELETE, "/v1/graphs/{graph}"),
            Some(Op::GraphWrite)
        ));
        assert!(route_op(&Method::GET, "/v1/admin/graph/stats").is_none());
        assert!(route_op(&Method::PUT, "/v1/graphs/{graph}/batch").is_none());
    }
}
