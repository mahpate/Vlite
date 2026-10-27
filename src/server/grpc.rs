//! Opt-in gRPC transport (`vlite.v1.VLite`, Cargo feature `grpc`).
//!
//! A thin adapter: every RPC converts wire types into the same inputs REST
//! uses and calls the same transport-neutral cores, so authentication
//! (`authenticate`), scope policy (`authorize_need`), shared admission and
//! worker permits, engine limits, validation, metrics and audit are
//! identical. Nothing here issues an HTTP call. Errors carry the REST error
//! message and the stable REST code in the `vlite-error-code` trailer.

use super::*;
use std::{collections::HashMap, future::Future};
use tonic::{Request, Response, Status, metadata::MetadataValue as Ascii};

/// Generated protobuf types, server and client for `proto/vlite/v1/vlite.proto`.
#[allow(clippy::all, clippy::pedantic, missing_docs)]
pub mod proto {
    tonic::include_proto!("vlite.v1");
}

use proto::v_lite_server::{VLite, VLiteServer};

/// Decoded request message limit; equals the REST body limit.
pub const MAX_DECODING_MESSAGE_BYTES: usize = MAX_JSON_BYTES;
/// Encoded response message limit.
pub const MAX_ENCODING_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
/// Fully qualified service name reported by `grpc.health.v1.Health`.
pub const SERVICE_NAME: &str = "vlite.v1.VLite";
/// Trailer carrying the stable REST error code of a failed call.
pub const ERROR_CODE_TRAILER: &str = "vlite-error-code";

/// gRPC routes (the VLite service plus `grpc.health.v1.Health`) and their
/// health handle, built over one engine handle.
pub struct GrpcServer {
    routes: tonic::service::Routes,
    health: GrpcHealth,
    tls: Option<tonic::transport::ServerTlsConfig>,
    reloadable_tls: Option<super::tls::TlsIdentity>,
}

/// Validated PEM server identity for native gRPC TLS (rustls, TLS 1.2+).
#[derive(Clone)]
pub struct GrpcTls(tonic::transport::ServerTlsConfig);

impl GrpcTls {
    /// Parses a PEM certificate chain (leaf first) and its PEM private key.
    /// Fails if either is malformed or the key does not match the leaf, so a
    /// bad identity is reported before a listener serves any request.
    pub fn from_pem(
        cert_chain: impl AsRef<[u8]>,
        key: impl AsRef<[u8]>,
    ) -> Result<Self, tonic::transport::Error> {
        let config = tonic::transport::ServerTlsConfig::new()
            .identity(tonic::transport::Identity::from_pem(cert_chain, key));
        // Building the rustls acceptor is what validates the identity.
        tonic::transport::Server::builder().tls_config(config.clone())?;
        Ok(Self(config))
    }
}

/// Health reporter for the gRPC listener. Health calls never require
/// authentication, like REST `GET /health`.
#[derive(Clone)]
pub struct GrpcHealth(tonic_health::server::HealthReporter);

impl GrpcHealth {
    async fn set(&self, status: tonic_health::ServingStatus) {
        for name in ["", SERVICE_NAME] {
            self.0.set_service_status(name, status).await;
        }
    }

    /// Reports `NOT_SERVING` for `""` and `vlite.v1.VLite`; call first on shutdown.
    pub async fn set_not_serving(&self) {
        self.set(tonic_health::ServingStatus::NotServing).await;
    }
}

/// Builds the gRPC routes over an opened database and reports `SERVING`.
///
/// Call it with a clone of the `Service` after building the REST router
/// (`configured_admin_router` or `authenticated_router`): gRPC then shares
/// that router's tokens, credential store, token reloads and audit log.
/// Without a configured REST router it uses the service's own state, which,
/// like `router`, has no admin token (local development only).
pub async fn grpc_router(mut service: Service) -> GrpcServer {
    if let Some(admin) = service
        .published_admin
        .lock()
        .ok()
        .and_then(|published| published.clone())
    {
        service.admin = admin;
    }
    let (reporter, health_service) = tonic_health::server::health_reporter();
    let health = GrpcHealth(reporter);
    health.set(tonic_health::ServingStatus::Serving).await;
    let vlite = VLiteServer::new(Adapter { service })
        .max_decoding_message_size(MAX_DECODING_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_ENCODING_MESSAGE_BYTES);
    let routes = tonic::service::Routes::new(health_service).add_service(vlite);
    GrpcServer {
        routes,
        health,
        tls: None,
        reloadable_tls: None,
    }
}

impl GrpcServer {
    /// Uses a reloadable native TLS listener, including optional client authentication.
    #[must_use]
    pub fn with_reloadable_tls(mut self, tls: super::tls::TlsIdentity) -> Self {
        self.reloadable_tls = Some(tls);
        self
    }

    pub fn health(&self) -> GrpcHealth {
        self.health.clone()
    }

    /// Serves TLS instead of plaintext HTTP/2 (clients use `https://`).
    #[must_use]
    pub fn with_tls(mut self, tls: GrpcTls) -> Self {
        self.tls = Some(tls.0);
        self
    }

    /// Serves HTTP/2 (TLS when configured with [`Self::with_tls`]) until
    /// `shutdown` resolves, then drains in-flight calls. Each connection is
    /// limited to the same 16 in-flight requests as the shared admission
    /// permits.
    pub async fn serve_with_shutdown(
        self,
        listener: tokio::net::TcpListener,
        shutdown: impl Future<Output = ()> + Send,
    ) -> Result<(), tonic::transport::Error> {
        if let Some(tls) = self.reloadable_tls {
            let listener = super::tls::TlsListener::new(listener, tls).expect("bound TLS listener");
            let incoming = tokio_stream::wrappers::ReceiverStream::new({
                let (tx, rx) = tokio::sync::mpsc::channel(64);
                tokio::spawn(async move {
                    let mut listener = listener;
                    loop {
                        tokio::select! {
                            _ = tx.closed() => break,
                            stream = listener.next() => match stream {
                                Some(stream) => if tx.send(stream).await.is_err() { break },
                                None => break,
                            }
                        }
                    }
                });
                rx
            });
            return tonic::transport::Server::builder()
                .concurrency_limit_per_connection(MAX_IN_FLIGHT)
                .add_routes(self.routes)
                .serve_with_incoming_shutdown(incoming, shutdown)
                .await;
        }
        let mut builder = tonic::transport::Server::builder();
        if let Some(tls) = self.tls {
            builder = builder.tls_config(tls)?;
        }
        builder
            .concurrency_limit_per_connection(MAX_IN_FLIGHT)
            .add_routes(self.routes)
            .serve_with_incoming_shutdown(
                tonic::transport::server::TcpIncoming::from(listener).with_nodelay(Some(true)),
                shutdown,
            )
            .await
    }
}

struct Adapter {
    service: Service,
}

/// Maps the single REST error currency onto gRPC codes.
fn status_code(error: &ApiError) -> tonic::Code {
    use tonic::Code;
    match (error.status.as_u16(), error.code) {
        (429, _) => Code::ResourceExhausted,
        (401, _) => Code::Unauthenticated,
        (403, _) => Code::PermissionDenied,
        (404, _) => Code::NotFound,
        (409, "collection_exists") => Code::AlreadyExists,
        (409, "rebuild_busy") | (412, _) => Code::Aborted,
        (409, _) => Code::FailedPrecondition,
        (503, "busy" | "unavailable") => Code::Unavailable,
        // The write may have committed: never safe to retry blindly.
        (503, "commit_indeterminate") => Code::Unknown,
        (400..=499, _) => Code::InvalidArgument,
        _ => Code::Internal,
    }
}

fn status(error: ApiError, request_id: &str) -> Status {
    let mut status = Status::new(status_code(&error), error.message);
    let metadata = status.metadata_mut();
    if error.status == StatusCode::TOO_MANY_REQUESTS {
        metadata.insert("retry-after", Ascii::from_static("1"));
    }
    metadata.insert(ERROR_CODE_TRAILER, Ascii::from_static(error.code));
    if let Some(details) = error.details {
        let encoded = BASE64_STANDARD.encode(details);
        if let Ok(value) = encoded.parse() {
            metadata.insert("vlite-error-details-base64", value);
        }
    }
    if let Ok(value) = request_id.parse() {
        metadata.insert("x-request-id", value);
    }
    status
}

impl Adapter {
    /// Per-call pipeline: shared admission permit → authenticate → static
    /// scope policy → core → observation. The pipeline runs in its own task,
    /// so a client cancellation never releases the admission permit or
    /// skips observation before the blocking engine work has completed.
    async fn call<Req, Res, F, Fut>(
        &self,
        route: &'static str,
        request: Request<Req>,
        need: Need,
        read_only_allowed: bool,
        legacy: bool,
        core: F,
    ) -> Result<Response<Res>, Status>
    where
        Req: Send + 'static,
        Res: Send + 'static,
        F: FnOnce(Service, admin::Role, String, Req) -> Fut + Send + 'static,
        Fut: Future<Output = Result<Res, ApiError>> + Send,
    {
        let service = self.service.clone();
        let headers = request.metadata().clone().into_headers();
        let message = request.into_inner();
        #[cfg(feature = "otel")]
        let trace_collection = request_collection(&message);
        let task = tokio::spawn(async move {
            let started = Instant::now();
            let request_id = next_request_id(&service);
            #[cfg(feature = "otel")]
            let trace = super::telemetry::request(&request_id, route, &trace_collection);
            let mut role = None;
            let result = match service.requests.clone().try_acquire_owned() {
                Err(_) => Err(ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "busy",
                    "too many in-flight requests",
                )),
                Ok(_permit) => match authenticate(&service, &headers) {
                    Err(error) => Err(error),
                    Ok(None) => Err(ApiError::new(
                        StatusCode::UNAUTHORIZED,
                        "unauthorized",
                        "valid bearer token required",
                    )),
                    Ok(Some(caller)) => {
                        role = Some(caller.clone());
                        match rate_limit(&service, &caller).and_then(|()| {
                            authorize_need(&service, &caller, need, read_only_allowed, legacy)
                        }) {
                            Err(error) => Err(error),
                            Ok(()) => {
                                core(service.clone(), caller, request_id.clone(), message).await
                            }
                        }
                    }
                },
            };
            let (status_code, code) = match &result {
                Ok(_) => (StatusCode::OK, None),
                Err(error) => (error.status, Some(error.code)),
            };
            #[cfg(feature = "otel")]
            super::telemetry::finish(trace, status_code.as_u16());
            record_request(
                &service,
                &RequestRecord {
                    route,
                    method: "grpc",
                    status: status_code,
                    code,
                    role: role.as_ref(),
                    request_id: &request_id,
                    started,
                },
            );
            (request_id, result)
        });
        let (request_id, result) = task.await.map_err(|error| status(internal(error), ""))?;
        match result {
            Ok(message) => {
                let mut response = Response::new(message);
                if let Ok(value) = request_id.parse() {
                    response.metadata_mut().insert("x-request-id", value);
                }
                Ok(response)
            }
            Err(error) => Err(status(error, &request_id)),
        }
    }
}

/// Empty `collection` addresses legacy `default`, like the `/v1/...` routes.
fn target(collection: String) -> Target {
    if collection.is_empty() {
        Target::Legacy
    } else {
        Target::Named(collection)
    }
}

/// Opens a collection name already qualified for a scoped tenant, without
/// applying tenant qualification twice.
fn open_qualified(db: &Database, role: &admin::Role, name: &str) -> Result<u16, ApiError> {
    let raw = tenants::unqualify(role, name);
    open(db, role, &Target::Named(raw))
}

fn value_in(value: Option<proto::MetadataValue>) -> Result<crate::MetadataValue, ApiError> {
    use proto::metadata_value::Kind;
    match value.and_then(|value| value.kind) {
        Some(Kind::StringValue(value)) => Ok(crate::MetadataValue::String(value)),
        Some(Kind::IntegerValue(value)) => Ok(crate::MetadataValue::Integer(value)),
        Some(Kind::FloatValue(value)) => Ok(crate::MetadataValue::Float(value)),
        Some(Kind::BooleanValue(value)) => Ok(crate::MetadataValue::Boolean(value)),
        Some(Kind::GeoPoint(value)) => Ok(crate::MetadataValue::GeoPoint {
            latitude: value.latitude,
            longitude: value.longitude,
        }),
        None => Err(unprocessable(
            "metadata values must set string_value, integer_value, float_value or boolean_value",
        )),
    }
}

fn value_out(value: crate::MetadataValue) -> proto::MetadataValue {
    use proto::metadata_value::Kind;
    proto::MetadataValue {
        kind: Some(match value {
            crate::MetadataValue::String(value) => Kind::StringValue(value),
            crate::MetadataValue::Integer(value) => Kind::IntegerValue(value),
            crate::MetadataValue::Float(value) => Kind::FloatValue(value),
            crate::MetadataValue::Boolean(value) => Kind::BooleanValue(value),
            crate::MetadataValue::GeoPoint {
                latitude,
                longitude,
            } => Kind::GeoPoint(proto::GeoPoint {
                latitude,
                longitude,
            }),
        }),
    }
}

/// Converts wire metadata; field and value rules are enforced by the engine.
fn metadata_in(
    metadata: HashMap<String, proto::MetadataValue>,
) -> Result<crate::Metadata, ApiError> {
    metadata
        .into_iter()
        .map(|(field, value)| Ok((field, value_in(Some(value))?)))
        .collect()
}

fn metadata_out(metadata: crate::Metadata) -> HashMap<String, proto::MetadataValue> {
    metadata
        .into_iter()
        .map(|(field, value)| (field, value_out(value)))
        .collect()
}

fn expected(expected: Option<proto::Expected>) -> Guard {
    use proto::expected::Kind;
    Guard::Expected(match expected.and_then(|expected| expected.kind) {
        None | Some(Kind::Any(_)) => Expected::Any,
        Some(Kind::Absent(_)) => Expected::Absent,
        Some(Kind::Revision(revision)) => Expected::Revision(revision),
    })
}

fn condition(condition: proto::Condition) -> Result<crate::Condition, ApiError> {
    use proto::condition::Kind;
    match condition.kind {
        Some(Kind::Eq(eq)) => Ok(crate::Condition::Eq {
            field: eq.field,
            value: value_in(eq.value)?,
        }),
        Some(Kind::GeoRadius(geo)) => Ok(crate::Condition::GeoRadius {
            field: geo.field,
            latitude: geo.latitude,
            longitude: geo.longitude,
            radius_meters: geo.radius_meters,
        }),
        Some(Kind::Range(range)) => Ok(crate::Condition::Range {
            field: range.field,
            min: range.min.map(|value| value_in(Some(value))).transpose()?,
            max: range.max.map(|value| value_in(Some(value))).transpose()?,
        }),
        None => Err(unprocessable("filter conditions must set eq or range")),
    }
}

/// Converts a wire request into the REST `Search` input. Lazy index builds
/// are not exposed over gRPC.
fn search_input(request: proto::SearchRequest) -> Result<Search, ApiError> {
    let mode = match proto::SearchMode::try_from(request.mode) {
        Ok(proto::SearchMode::Unspecified | proto::SearchMode::Exact) => SearchMode::Exact,
        Ok(proto::SearchMode::ApproximateReady) => SearchMode::ApproximateReady,
        Ok(proto::SearchMode::Hybrid) => SearchMode::Hybrid,
        Err(_) => return Err(unprocessable("unknown search mode")),
    };
    let metric = match proto::Metric::try_from(request.metric) {
        Ok(proto::Metric::Unspecified) => None,
        Ok(proto::Metric::SquaredL2) => Some(DistanceMetric::SquaredL2),
        Ok(proto::Metric::Cosine) => Some(DistanceMetric::Cosine),
        Ok(proto::Metric::InnerProduct) => Some(DistanceMetric::InnerProduct),
        Err(_) => return Err(unprocessable("unknown metric")),
    };
    let quantization = match proto::Quantization::try_from(request.quantization) {
        Ok(proto::Quantization::Unspecified) => None,
        Ok(proto::Quantization::None) => Some(collections::QuantizationName(Quantization::None)),
        Ok(proto::Quantization::ScalarU8) => {
            Some(collections::QuantizationName(Quantization::ScalarU8))
        }
        Err(_) => return Err(unprocessable("unknown quantization")),
    };
    // The REST identity type parses partial configs with the same defaults.
    let config = request
        .config
        .map(|config| {
            let mut object = serde_json::Map::new();
            if let Some(m) = config.m {
                object.insert("m".into(), json!(m));
            }
            if let Some(ef_construction) = config.ef_construction {
                object.insert("ef_construction".into(), json!(ef_construction));
            }
            if let Some(seed) = config.seed {
                object.insert("seed".into(), json!(seed));
            }
            serde_json::from_value::<admin::IndexConfig>(Value::Object(object))
                .map_err(|error| unprocessable(error.to_string()))
        })
        .transpose()?;
    let rerank_multiplier = request
        .rerank_multiplier
        .map(|value| {
            u8::try_from(value).map_err(|_| unprocessable("rerank_multiplier must be 0..255"))
        })
        .transpose()?;
    let filter = request
        .filter
        .into_iter()
        .map(condition)
        .collect::<Result<Vec<_>, _>>()?;
    let vector_leg = match proto::HybridVectorLeg::try_from(request.vector_leg) {
        Ok(proto::HybridVectorLeg::Exact) => None,
        Ok(proto::HybridVectorLeg::ApproximateReady) => Some(VectorLegName::ApproximateReady),
        Err(_) => return Err(unprocessable("unknown hybrid vector leg")),
    };
    let fusion = request
        .fusion
        .map(|fusion| -> Result<Fusion, ApiError> {
            let candidates = fusion
                .candidates
                .map(usize::try_from)
                .transpose()
                .map_err(|_| unprocessable("fusion candidates exceed platform limits"))?;
            Ok(Fusion {
                candidates,
                rrf_k: fusion.rrf_k,
                vector_weight: fusion.vector_weight,
                text_weight: fusion.text_weight,
            })
        })
        .transpose()?;
    let vector =
        if mode != SearchMode::Hybrid || request.vector_enabled || !request.vector.is_empty() {
            Some(request.vector)
        } else {
            None
        };
    Ok(Search {
        vector,
        k: request.k as usize,
        mode,
        metric,
        config,
        ef_search: request.ef_search.map(|ef| ef as usize),
        filter: (!filter.is_empty()).then_some(filter),
        quantization,
        rerank_multiplier,
        rerank: None,
        text: request.text,
        fusion,
        vector_leg,
        explain: false,
    })
}

fn collection_out(info: crate::CollectionInfo) -> proto::Collection {
    proto::Collection {
        id: u32::from(info.id),
        settings_json: collections::settings_json(&info.settings).to_string(),
        name: info.name,
        dimensions: u32::try_from(info.dimensions).unwrap_or(u32::MAX),
        live_records: info.live_records as u64,
        settings_persisted: info.settings_persisted,
    }
}

fn mutation(operation: proto::Operation) -> Result<Mutation, ApiError> {
    use proto::operation::Kind;
    match operation.kind {
        Some(Kind::Put(put)) => {
            let metadata = metadata_in(put.metadata)?;
            Ok(if metadata.is_empty() {
                Mutation::Put {
                    id: put.id,
                    vector: put.vector,
                    payload: put.payload,
                }
            } else {
                Mutation::PutWithMetadata {
                    id: put.id,
                    vector: put.vector,
                    payload: put.payload,
                    metadata,
                }
            })
        }
        Some(Kind::Delete(delete)) => Ok(Mutation::Delete { id: delete.id }),
        None => Err(unprocessable("operations must set put or delete")),
    }
}

fn import_json(value: Value) -> Result<proto::ImportJsonResponse, ApiError> {
    Ok(proto::ImportJsonResponse {
        json: serde_json::to_string(&value).map_err(internal)?,
    })
}

use credentials::Op;

fn metric_in(metric: i32) -> Result<DistanceMetric, ApiError> {
    match proto::Metric::try_from(metric) {
        Ok(proto::Metric::Unspecified | proto::Metric::SquaredL2) => Ok(DistanceMetric::SquaredL2),
        Ok(proto::Metric::Cosine) => Ok(DistanceMetric::Cosine),
        Ok(proto::Metric::InnerProduct) => Ok(DistanceMetric::InnerProduct),
        Err(_) => Err(unprocessable("unknown metric")),
    }
}

fn conditions_in(items: Vec<proto::Condition>) -> Result<Vec<crate::Condition>, ApiError> {
    items.into_iter().map(condition).collect()
}

fn hits_out(hits: Vec<crate::SearchHit>) -> proto::SearchResponse {
    proto::SearchResponse {
        hits: hits
            .into_iter()
            .map(|hit| proto::Hit {
                id: hit.id,
                distance: hit.distance,
            })
            .collect(),
        hybrid_hits: Vec::new(),
    }
}

fn sparse_in(indices: Vec<u32>, values: Vec<f32>) -> Result<Vec<(u32, f32)>, ApiError> {
    if indices.len() != values.len() {
        return Err(unprocessable(
            "sparse indices and values must have equal lengths",
        ));
    }
    Ok(indices.into_iter().zip(values).collect())
}

fn change_out(change: crate::changes::Change) -> proto::ChangeEvent {
    use crate::changes::ChangeKind;
    let mut event = proto::ChangeEvent {
        sequence: change.sequence,
        ordinal: change.ordinal,
        collection: change.collection,
        committed_at_ms: change.committed_at_ms,
        kind: proto::ChangeKind::Unspecified as i32,
        id: None,
        expires_at_ms: None,
        vector: Vec::new(),
        payload: Vec::new(),
        metadata: HashMap::new(),
        dimensions: None,
        settings_json: String::new(),
        graph: String::new(),
        revision: None,
        graph_operations: Vec::new(),
    };
    match change.kind {
        ChangeKind::Put {
            id,
            record,
            expires_at_ms,
        } => {
            event.kind = proto::ChangeKind::Put as i32;
            event.id = Some(id);
            event.expires_at_ms = expires_at_ms;
            event.vector = record.vector;
            event.payload = record.payload;
            event.metadata = metadata_out(record.metadata);
        }
        ChangeKind::Delete { id } => {
            event.kind = proto::ChangeKind::Delete as i32;
            event.id = Some(id);
        }
        ChangeKind::MetadataUpdate { id, metadata } => {
            event.kind = proto::ChangeKind::MetadataUpdate as i32;
            event.id = Some(id);
            event.metadata = metadata_out(metadata);
        }
        ChangeKind::CollectionCreated {
            dimensions,
            settings,
        } => {
            event.kind = proto::ChangeKind::CollectionCreated as i32;
            event.dimensions = u32::try_from(dimensions).ok();
            event.settings_json = collections::settings_json(&settings).to_string();
        }
        ChangeKind::CollectionDropped => event.kind = proto::ChangeKind::CollectionDropped as i32,
        ChangeKind::SettingsUpdated { settings } => {
            event.kind = proto::ChangeKind::SettingsUpdated as i32;
            event.settings_json = collections::settings_json(&settings).to_string();
        }
        ChangeKind::VectorsUpdated => event.kind = proto::ChangeKind::VectorsUpdated as i32,
        ChangeKind::GraphCreated { graph } => {
            event.kind = proto::ChangeKind::GraphCreated as i32;
            event.graph = graph;
        }
        ChangeKind::GraphDropped { graph } => {
            event.kind = proto::ChangeKind::GraphDropped as i32;
            event.graph = graph;
        }
        ChangeKind::GraphBatch {
            graph,
            revision,
            operations,
        } => {
            event.kind = proto::ChangeKind::GraphBatch as i32;
            event.graph = graph;
            event.revision = Some(revision);
            event.graph_operations = operations.into_iter().map(graph_change_out).collect();
        }
    }
    event
}

fn graph_change_out(change: crate::changes::GraphChange) -> proto::GraphChangeOperation {
    use crate::changes::GraphChange;
    use proto::GraphChangeOp as Op;
    let mut out = proto::GraphChangeOperation::default();
    let op = match change {
        GraphChange::NodeUpserted {
            id,
            key,
            labels,
            properties,
            created,
        } => {
            out.id = Some(id);
            out.key = Some(key);
            out.labels = labels;
            out.properties = metadata_out(properties);
            out.created = created;
            Op::NodeUpserted
        }
        GraphChange::NodePatched {
            id,
            key,
            add_labels,
            remove_labels,
            set,
            unset,
        } => {
            out.id = Some(id);
            out.key = Some(key);
            out.labels = add_labels;
            out.remove_labels = remove_labels;
            out.properties = metadata_out(set);
            out.unset = unset;
            Op::NodePatched
        }
        GraphChange::NodeDeleted { id, key } => {
            out.id = Some(id);
            out.key = Some(key);
            Op::NodeDeleted
        }
        GraphChange::EdgeUpserted {
            id,
            key,
            source,
            source_key,
            relationship,
            destination,
            destination_key,
            properties,
            created,
        } => {
            out.id = Some(id);
            out.key = key;
            out.source = Some(source);
            out.source_key = source_key;
            out.relationship = relationship;
            out.destination = Some(destination);
            out.destination_key = destination_key;
            out.properties = metadata_out(properties);
            out.created = created;
            Op::EdgeUpserted
        }
        GraphChange::EdgePatched {
            id,
            key,
            set,
            unset,
        } => {
            out.id = Some(id);
            out.key = key;
            out.properties = metadata_out(set);
            out.unset = unset;
            Op::EdgePatched
        }
        GraphChange::EdgeDeleted { id, key } => {
            out.id = Some(id);
            out.key = key;
            Op::EdgeDeleted
        }
        GraphChange::Attached {
            node,
            node_key,
            collection,
            record,
        } => {
            out.node = Some(node);
            out.node_key = node_key;
            out.collection = collection;
            out.record = Some(record);
            Op::Attached
        }
        GraphChange::Detached {
            node,
            node_key,
            collection,
            record,
        } => {
            out.node = Some(node);
            out.node_key = node_key;
            out.collection = collection;
            out.record = Some(record);
            Op::Detached
        }
        GraphChange::SchemaUpdated { schema } => {
            out.schema_json = schema.to_json().to_string();
            Op::SchemaUpdated
        }
    };
    out.op = op as i32;
    out
}

async fn poll_changes(
    service: &Service,
    headers: &axum::http::HeaderMap,
    cursor: crate::changes::FeedCursor,
    limit: usize,
) -> Result<crate::changes::ChangeBatch, ApiError> {
    let _permit = service
        .requests
        .clone()
        .acquire_owned()
        .await
        .map_err(internal)?;
    let role = authenticate(service, headers)?.ok_or_else(|| {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "valid bearer token required",
        )
    })?;
    rate_limit(service, &role)?;
    authorize_need(service, &role, Need::Admin, false, false)?;
    service
        .run_read(move |db| Ok(db.changes_after(&cursor, limit)?))
        .await
}

#[tonic::async_trait]
impl VLite for Adapter {
    async fn list_collections(
        &self,
        request: Request<proto::ListCollectionsRequest>,
    ) -> Result<Response<proto::ListCollectionsResponse>, Status> {
        let need = Need::Op(Op::Read, Reach::Filtered);
        self.call(
            "grpc:/vlite.v1.VLite/ListCollections",
            request,
            need,
            true,
            false,
            |service, role, _, _| async move {
                let (items, format_version) = collections::list_core(&service, role).await?;
                Ok(proto::ListCollectionsResponse {
                    items: items.into_iter().map(collection_out).collect(),
                    format_version,
                })
            },
        )
        .await
    }

    async fn get_collection(
        &self,
        request: Request<proto::GetCollectionRequest>,
    ) -> Result<Response<proto::Collection>, Status> {
        let need = Need::Op(Op::Read, Reach::Collection);
        self.call(
            "grpc:/vlite.v1.VLite/GetCollection",
            request,
            need,
            true,
            false,
            |service, role, _, request| async move {
                let target = Target::Named(request.name);
                let (info, _) = collections::fetch_core(&service, role, target).await?;
                Ok(collection_out(info))
            },
        )
        .await
    }

    async fn create_collection(
        &self,
        request: Request<proto::CreateCollectionRequest>,
    ) -> Result<Response<proto::Collection>, Status> {
        let need = Need::Op(Op::Collections, Reach::All);
        self.call(
            "grpc:/vlite.v1.VLite/CreateCollection",
            request,
            need,
            false,
            false,
            |service, role, _, request| async move {
                let input = collections::Create::new(
                    request.name,
                    request.dimensions as usize,
                    request.settings_json.as_deref(),
                )?;
                Ok(collection_out(
                    collections::create_core(&service, role, input).await?,
                ))
            },
        )
        .await
    }

    async fn drop_collection(
        &self,
        request: Request<proto::DropCollectionRequest>,
    ) -> Result<Response<proto::DropCollectionResponse>, Status> {
        let need = Need::Op(Op::Collections, Reach::Collection);
        self.call(
            "grpc:/vlite.v1.VLite/DropCollection",
            request,
            need,
            false,
            false,
            |service, role, _, request| async move {
                let target = Target::Named(request.name);
                collections::drop_core(&service, role, target).await?;
                Ok(proto::DropCollectionResponse { deleted: true })
            },
        )
        .await
    }

    async fn get_vector(
        &self,
        request: Request<proto::GetVectorRequest>,
    ) -> Result<Response<proto::Vector>, Status> {
        let need = Need::Op(Op::Read, Reach::Collection);
        let legacy = request.get_ref().collection.is_empty();
        self.call(
            "grpc:/vlite.v1.VLite/GetVector",
            request,
            need,
            true,
            legacy,
            |service, role, _, request| async move {
                let id = request.id;
                let addressed = target(request.collection);
                service
                    .run_read(move |db| {
                        let collection = open(db, &role, &addressed)?;
                        let not_found = || {
                            ApiError::new(StatusCode::NOT_FOUND, "not_found", "vector not found")
                        };
                        let record = db.get_in(collection, id)?.ok_or_else(not_found)?.clone();
                        let revision = db.revision_in(collection, id)?.ok_or_else(not_found)?;
                        let expires_at_ms = db.state(collection).expiries.get(&id).copied();
                        Ok(proto::Vector {
                            id,
                            vector: record.vector,
                            payload: record.payload,
                            metadata: metadata_out(record.metadata),
                            revision,
                            expires_at_ms,
                        })
                    })
                    .await
            },
        )
        .await
    }

    async fn put_vector(
        &self,
        request: Request<proto::PutVectorRequest>,
    ) -> Result<Response<proto::PutVectorResponse>, Status> {
        let need = Need::Op(Op::Write, Reach::Collection);
        let legacy = request.get_ref().collection.is_empty();
        self.call(
            "grpc:/vlite.v1.VLite/PutVector",
            request,
            need,
            false,
            legacy,
            |service, role, _, request| async move {
                let metadata = metadata_in(request.metadata)?;
                let addressed = target(request.collection);
                let written = if let Some(expiry) = request.expires_at_ms {
                    if request.payload.len() > MAX_PAYLOAD_BYTES {
                        return Err(Error::PayloadTooLarge(request.payload.len()).into());
                    }
                    let quota_service = service.clone();
                    let ident = service.etag_ident.clone();
                    let guard = expected(request.expected);
                    let id = request.id;
                    let vector = request.vector;
                    let payload = request.payload;
                    let expiry_target = addressed.clone();
                    let bytes = group_commit::record_bytes(
                        vector.capacity(),
                        payload.capacity(),
                        &metadata,
                    )
                    .saturating_add(expiry_target.retained_bytes())
                    .saturating_add(guard.retained_bytes());
                    service
                        .run_mutation(bytes, move |db| {
                            let collection = open(db, &role, &expiry_target)?;
                            enforce_quota(&quota_service, db, &role, collection, &[(id, true)])?;
                            let expected = guard.resolve(db, &ident, collection, id)?;
                            let created = db.revision_in(collection, id)?.is_none();
                            let revision = db.put_with_expiry_in(
                                collection,
                                id,
                                &vector,
                                &payload,
                                &metadata,
                                Some(expiry),
                                expected,
                            )?;
                            Ok(Written {
                                revision,
                                created,
                                etag: String::new(),
                            })
                        })
                        .await?
                } else {
                    put_core(
                        &service,
                        role,
                        addressed,
                        request.id,
                        expected(request.expected),
                        request.vector,
                        request.payload,
                        metadata,
                        None,
                    )
                    .await?
                };
                Ok(proto::PutVectorResponse {
                    revision: written.revision,
                    created: written.created,
                    expires_at_ms: request.expires_at_ms,
                })
            },
        )
        .await
    }

    async fn delete_vector(
        &self,
        request: Request<proto::DeleteVectorRequest>,
    ) -> Result<Response<proto::DeleteVectorResponse>, Status> {
        let need = Need::Op(Op::Write, Reach::Collection);
        let legacy = request.get_ref().collection.is_empty();
        self.call(
            "grpc:/vlite.v1.VLite/DeleteVector",
            request,
            need,
            false,
            legacy,
            |service, role, _, request| async move {
                let deleted = delete_core(
                    &service,
                    role,
                    target(request.collection),
                    request.id,
                    expected(request.expected),
                )
                .await?;
                Ok(proto::DeleteVectorResponse { deleted })
            },
        )
        .await
    }

    async fn write_batch(
        &self,
        request: Request<proto::WriteBatchRequest>,
    ) -> Result<Response<proto::WriteBatchResponse>, Status> {
        let need = Need::Op(Op::Write, Reach::Collection);
        let legacy = request.get_ref().collection.is_empty();
        self.call(
            "grpc:/vlite.v1.VLite/WriteBatch",
            request,
            need,
            false,
            legacy,
            |service, role, _, request| async move {
                if request.operations.len() > MAX_BATCH_OPERATIONS {
                    return Err(Error::BatchTooLarge.into());
                }
                let operations = request
                    .operations
                    .into_iter()
                    .map(mutation)
                    .collect::<Result<Vec<_>, _>>()?;
                batch_core(&service, role, target(request.collection), operations).await?;
                Ok(proto::WriteBatchResponse {})
            },
        )
        .await
    }

    async fn search(
        &self,
        request: Request<proto::SearchRequest>,
    ) -> Result<Response<proto::SearchResponse>, Status> {
        const ROUTE: &str = "grpc:/vlite.v1.VLite/Search";
        let need = Need::Op(Op::Search, Reach::Collection);
        let legacy = request.get_ref().collection.is_empty();
        self.call(
            ROUTE,
            request,
            need,
            true,
            legacy,
            |service, role, request_id, request| async move {
                let target = target(request.collection.clone());
                let requested_k = request.k as usize;
                let rerank_metric = metric_in(request.metric)?;
                let rerank_metric_explicit = request.metric != proto::Metric::Unspecified as i32;
                let rerank_mode = proto::RerankMode::try_from(request.rerank_mode)
                    .map_err(|_| unprocessable("unknown rerank mode"))?;
                let candidate_count = request.rerank_candidates.map_or_else(
                    || requested_k.saturating_mul(4).max(requested_k),
                    |value| value as usize,
                );
                if rerank_mode != proto::RerankMode::None
                    && (requested_k > crate::MAX_BATCH_OPERATIONS
                        || candidate_count < requested_k
                        || candidate_count > crate::MAX_BATCH_OPERATIONS)
                {
                    return Err(unprocessable(
                        "rerank candidate count must be at least k and at most 4096",
                    ));
                }
                if rerank_mode != proto::RerankMode::None
                    && proto::SearchMode::try_from(request.mode) == Ok(proto::SearchMode::Hybrid)
                {
                    return Err(unprocessable(
                        "reranking is only available for vector search",
                    ));
                }
                if (request.mmr_lambda.is_some() && rerank_mode != proto::RerankMode::Mmr)
                    || (request.rerank_candidates.is_some()
                        && rerank_mode == proto::RerankMode::None)
                {
                    return Err(unprocessable(
                        "MMR lambda and candidate depth require a matching rerank mode",
                    ));
                }
                let mmr_lambda = request.mmr_lambda.unwrap_or(0.5);
                let mut input = search_input(request)?;
                if rerank_mode != proto::RerankMode::None {
                    input.k = candidate_count;
                }
                let rerank_role = role.clone();
                let rerank_target = target.clone();
                let query = input.vector.clone().unwrap_or_default();
                let (_, executed) =
                    search_core(&service, role, &request_id, ROUTE, target, input).await?;
                match executed.hits {
                    Hits::Vector(hits) => {
                        let hits = if rerank_mode == proto::RerankMode::None {
                            hits
                        } else {
                            let name = tenants::qualify(&rerank_role, rerank_target.name());
                            service
                                .run(move |db| {
                                    let collection = open(db, &rerank_role, &rerank_target)?;
                                    let metric = if rerank_metric_explicit {
                                        rerank_metric
                                    } else {
                                        identity(db, collection, &rerank_target).0
                                    };
                                    let reranker: Box<dyn crate::rerank::Reranker> =
                                        match rerank_mode {
                                            proto::RerankMode::Exact => {
                                                Box::new(crate::rerank::ExactReranker { metric })
                                            }
                                            proto::RerankMode::Mmr => {
                                                Box::new(crate::rerank::MmrReranker {
                                                    lambda: mmr_lambda,
                                                })
                                            }
                                            proto::RerankMode::None => unreachable!(),
                                        };
                                    Ok(db.collection(&name)?.rerank(
                                        &query,
                                        &hits,
                                        requested_k,
                                        reranker.as_ref(),
                                    )?)
                                })
                                .await?
                        };
                        Ok(hits_out(hits))
                    }
                    Hits::Hybrid(hits) => {
                        let hybrid_hits = hits
                            .into_iter()
                            .map(|hit| {
                                let id = hit["id"]
                                    .as_str()
                                    .and_then(|id| id.parse().ok())
                                    .ok_or_else(|| internal("hybrid result has invalid id"))?;
                                let score = hit["score"]
                                    .as_f64()
                                    .ok_or_else(|| internal("hybrid result has invalid score"))?;
                                Ok(proto::HybridHit {
                                    id,
                                    score,
                                    distance: hit["distance"].as_f64(),
                                    bm25: hit["bm25"].as_f64(),
                                    vector_rank: hit["vector_rank"]
                                        .as_u64()
                                        .and_then(|rank| u32::try_from(rank).ok()),
                                    text_rank: hit["text_rank"]
                                        .as_u64()
                                        .and_then(|rank| u32::try_from(rank).ok()),
                                })
                            })
                            .collect::<Result<Vec<_>, ApiError>>()?;
                        Ok(proto::SearchResponse {
                            hits: Vec::new(),
                            hybrid_hits,
                        })
                    }
                }
            },
        )
        .await
    }

    async fn update_metadata(
        &self,
        request: Request<proto::UpdateMetadataRequest>,
    ) -> Result<Response<proto::UpdateMetadataResponse>, Status> {
        let legacy = request.get_ref().collection.is_empty();
        self.call(
            "grpc:/vlite.v1.VLite/UpdateMetadata",
            request,
            Need::Op(Op::Write, Reach::Collection),
            false,
            legacy,
            |service, role, _, request| async move {
                let name = tenants::qualify(&role, target(request.collection).name());
                let id = request.id;
                let metadata = metadata_in(request.metadata)?;
                let bytes =
                    group_commit::record_bytes(0, 0, &metadata).saturating_add(name.capacity());
                service
                    .run_mutation(bytes, move |db| {
                        open_qualified(db, &role, &name)?;
                        let revision = db.collection_mut(&name)?.update_metadata(id, &metadata)?;
                        Ok(proto::UpdateMetadataResponse { revision })
                    })
                    .await
            },
        )
        .await
    }

    async fn search_grouped(
        &self,
        request: Request<proto::GroupedSearchRequest>,
    ) -> Result<Response<proto::GroupedSearchResponse>, Status> {
        let legacy = request.get_ref().collection.is_empty();
        self.call(
            "grpc:/vlite.v1.VLite/SearchGrouped",
            request,
            Need::Op(Op::Search, Reach::Collection),
            true,
            legacy,
            |service, role, _, request| async move {
                let name = tenants::qualify(&role, target(request.collection).name());
                let filters = conditions_in(request.filter)?;
                let metric = metric_in(request.metric)?;
                service
                    .run_read(move |db| {
                        open_qualified(db, &role, &name)?;
                        let groups = db.collection(&name)?.search_grouped(
                            &request.vector,
                            request.groups as usize,
                            request.hits_per_group as usize,
                            &request.field,
                            &filters,
                            metric,
                        )?;
                        Ok(proto::GroupedSearchResponse {
                            groups: groups
                                .into_iter()
                                .map(|group| proto::SearchGroup {
                                    value: Some(value_out(group.value)),
                                    hits: group
                                        .hits
                                        .into_iter()
                                        .map(|hit| proto::Hit {
                                            id: hit.id,
                                            distance: hit.distance,
                                        })
                                        .collect(),
                                })
                                .collect(),
                        })
                    })
                    .await
            },
        )
        .await
    }

    async fn recommend(
        &self,
        request: Request<proto::RecommendRequest>,
    ) -> Result<Response<proto::SearchResponse>, Status> {
        let legacy = request.get_ref().collection.is_empty();
        self.call(
            "grpc:/vlite.v1.VLite/Recommend",
            request,
            Need::Op(Op::Search, Reach::Collection),
            true,
            legacy,
            |service, role, _, request| async move {
                let name = tenants::qualify(&role, target(request.collection).name());
                let conditions = conditions_in(request.filter)?;
                let metric = metric_in(request.metric)?;
                let query = crate::RecommendationQuery {
                    positive_ids: request.positive_ids,
                    negative_ids: request.negative_ids,
                    positive_vectors: request
                        .positive_vectors
                        .into_iter()
                        .map(|v| v.vector)
                        .collect(),
                    negative_vectors: request
                        .negative_vectors
                        .into_iter()
                        .map(|v| v.vector)
                        .collect(),
                    k: request.k as usize,
                    conditions,
                    metric,
                };
                service
                    .run_read(move |db| {
                        open_qualified(db, &role, &name)?;
                        Ok(hits_out(db.collection(&name)?.recommend(&query)?))
                    })
                    .await
            },
        )
        .await
    }

    async fn facets(
        &self,
        request: Request<proto::FacetsRequest>,
    ) -> Result<Response<proto::FacetsResponse>, Status> {
        let legacy = request.get_ref().collection.is_empty();
        self.call(
            "grpc:/vlite.v1.VLite/Facets",
            request,
            Need::Op(Op::Read, Reach::Collection),
            true,
            legacy,
            |service, role, _, request| async move {
                let name = tenants::qualify(&role, target(request.collection).name());
                let filters = conditions_in(request.filter)?;
                service
                    .run_read(move |db| {
                        open_qualified(db, &role, &name)?;
                        let facets = db.collection(&name)?.facets(&request.field, &filters)?;
                        Ok(proto::FacetsResponse {
                            facets: facets
                                .into_iter()
                                .map(|facet| proto::Facet {
                                    value: Some(value_out(facet.value)),
                                    count: facet.count,
                                })
                                .collect(),
                        })
                    })
                    .await
            },
        )
        .await
    }

    async fn numeric_stats(
        &self,
        request: Request<proto::NumericStatsRequest>,
    ) -> Result<Response<proto::NumericStatsResponse>, Status> {
        let legacy = request.get_ref().collection.is_empty();
        self.call(
            "grpc:/vlite.v1.VLite/NumericStats",
            request,
            Need::Op(Op::Read, Reach::Collection),
            true,
            legacy,
            |service, role, _, request| async move {
                let name = tenants::qualify(&role, target(request.collection).name());
                let filters = conditions_in(request.filter)?;
                service
                    .run_read(move |db| {
                        open_qualified(db, &role, &name)?;
                        let stats = db
                            .collection(&name)?
                            .numeric_stats(&request.field, &filters)?;
                        Ok(proto::NumericStatsResponse {
                            count: stats.count,
                            min: stats.min,
                            max: stats.max,
                            sum: stats.sum,
                            average: stats.average,
                        })
                    })
                    .await
            },
        )
        .await
    }

    async fn geo_search(
        &self,
        request: Request<proto::GeoSearchRequest>,
    ) -> Result<Response<proto::GeoSearchResponse>, Status> {
        let legacy = request.get_ref().collection.is_empty();
        self.call(
            "grpc:/vlite.v1.VLite/GeoSearch",
            request,
            Need::Op(Op::Search, Reach::Collection),
            true,
            legacy,
            |service, role, _, request| async move {
                let name = tenants::qualify(&role, target(request.collection).name());
                let conditions = conditions_in(request.filter)?;
                let metric = metric_in(request.metric)?;
                let sort = match proto::GeoSort::try_from(request.sort) {
                    Ok(proto::GeoSort::VectorDistance) => crate::GeoSort::VectorDistance,
                    Ok(proto::GeoSort::GeographicDistance) => crate::GeoSort::GeoDistance,
                    Err(_) => return Err(unprocessable("unknown geo sort")),
                };
                let query = crate::GeoSearchQuery {
                    vector: request.vector,
                    k: request.k as usize,
                    field: request.field,
                    latitude: request.latitude,
                    longitude: request.longitude,
                    radius_meters: request.radius_meters,
                    conditions,
                    metric,
                    sort,
                };
                service
                    .run_read(move |db| {
                        open_qualified(db, &role, &name)?;
                        let hits = db.collection(&name)?.search_geo(&query)?;
                        Ok(proto::GeoSearchResponse {
                            hits: hits
                                .into_iter()
                                .map(|hit| proto::GeoHit {
                                    id: hit.id,
                                    vector_distance: hit.vector_distance,
                                    geo_distance_meters: hit.geo_distance_meters,
                                })
                                .collect(),
                        })
                    })
                    .await
            },
        )
        .await
    }

    async fn configure_named_vector(
        &self,
        request: Request<proto::ConfigureNamedVectorRequest>,
    ) -> Result<Response<proto::Empty>, Status> {
        let legacy = request.get_ref().collection.is_empty();
        self.call(
            "grpc:/vlite.v1.VLite/ConfigureNamedVector",
            request,
            Need::Op(Op::Write, Reach::Collection),
            false,
            legacy,
            |service, role, _, request| async move {
                let spec = request
                    .spec
                    .ok_or_else(|| unprocessable("named vector spec is required"))?;
                let name = tenants::qualify(&role, target(request.collection).name());
                let vector_metric = metric_in(spec.metric)?;
                service
                    .run(move |db| {
                        open_qualified(db, &role, &name)?;
                        db.collection_mut(&name)?.configure_vector(
                            &spec.name,
                            crate::vectors::VectorSpec {
                                dimensions: spec.dimensions as usize,
                                metric: vector_metric,
                                active_version: spec.active_version,
                                history: spec.history as usize,
                            },
                        )?;
                        Ok(proto::Empty {})
                    })
                    .await
            },
        )
        .await
    }

    async fn put_named_vectors(
        &self,
        request: Request<proto::PutNamedVectorsRequest>,
    ) -> Result<Response<proto::Empty>, Status> {
        let legacy = request.get_ref().collection.is_empty();
        self.call(
            "grpc:/vlite.v1.VLite/PutNamedVectors",
            request,
            Need::Op(Op::Write, Reach::Collection),
            false,
            legacy,
            |service, role, _, request| async move {
                let name = tenants::qualify(&role, target(request.collection).name());
                let mut vectors = crate::vectors::RecordVectors::default();
                for vector in request.dense {
                    vectors.dense.entry(vector.name).or_default().push(
                        crate::vectors::VersionedVector {
                            version: vector.version,
                            vector: vector.vector,
                        },
                    );
                }
                for sparse in request.sparse {
                    let values = sparse_in(sparse.indices, sparse.values)?;
                    if vectors.sparse.insert(sparse.name, values).is_some() {
                        return Err(unprocessable("duplicate sparse vector name"));
                    }
                }
                service
                    .run(move |db| {
                        open_qualified(db, &role, &name)?;
                        db.collection_mut(&name)?.put_vectors(request.id, vectors)?;
                        Ok(proto::Empty {})
                    })
                    .await
            },
        )
        .await
    }

    async fn put_vector_version(
        &self,
        request: Request<proto::PutVectorVersionRequest>,
    ) -> Result<Response<proto::Empty>, Status> {
        let legacy = request.get_ref().collection.is_empty();
        self.call(
            "grpc:/vlite.v1.VLite/PutVectorVersion",
            request,
            Need::Op(Op::Write, Reach::Collection),
            false,
            legacy,
            |service, role, _, request| async move {
                let name = tenants::qualify(&role, target(request.collection).name());
                service
                    .run(move |db| {
                        open_qualified(db, &role, &name)?;
                        db.collection_mut(&name)?.put_vector_version(
                            request.id,
                            &request.name,
                            &request.version,
                            request.vector,
                        )?;
                        Ok(proto::Empty {})
                    })
                    .await
            },
        )
        .await
    }

    async fn search_named(
        &self,
        request: Request<proto::SearchNamedRequest>,
    ) -> Result<Response<proto::SearchResponse>, Status> {
        let legacy = request.get_ref().collection.is_empty();
        self.call(
            "grpc:/vlite.v1.VLite/SearchNamed",
            request,
            Need::Op(Op::Search, Reach::Collection),
            true,
            legacy,
            |service, role, _, request| async move {
                let name = tenants::qualify(&role, target(request.collection).name());
                service
                    .run_read(move |db| {
                        open_qualified(db, &role, &name)?;
                        Ok(hits_out(db.collection(&name)?.search_named(
                            &request.name,
                            request.version.as_deref(),
                            &request.vector,
                            request.k as usize,
                        )?))
                    })
                    .await
            },
        )
        .await
    }

    async fn search_sparse(
        &self,
        request: Request<proto::SearchSparseRequest>,
    ) -> Result<Response<proto::SearchResponse>, Status> {
        let legacy = request.get_ref().collection.is_empty();
        self.call(
            "grpc:/vlite.v1.VLite/SearchSparse",
            request,
            Need::Op(Op::Search, Reach::Collection),
            true,
            legacy,
            |service, role, _, request| async move {
                let name = tenants::qualify(&role, target(request.collection).name());
                let query = sparse_in(request.indices, request.values)?;
                service
                    .run_read(move |db| {
                        open_qualified(db, &role, &name)?;
                        Ok(hits_out(db.collection(&name)?.search_sparse(
                            &request.name,
                            &query,
                            request.k as usize,
                        )?))
                    })
                    .await
            },
        )
        .await
    }

    async fn search_dense_sparse(
        &self,
        request: Request<proto::SearchDenseSparseRequest>,
    ) -> Result<Response<proto::SearchResponse>, Status> {
        let legacy = request.get_ref().collection.is_empty();
        self.call(
            "grpc:/vlite.v1.VLite/SearchDenseSparse",
            request,
            Need::Op(Op::Search, Reach::Collection),
            true,
            legacy,
            |service, role, _, request| async move {
                let name = tenants::qualify(&role, target(request.collection).name());
                let sparse = sparse_in(request.sparse_indices, request.sparse_values)?;
                let fusion =
                    request
                        .fusion
                        .map_or_else(crate::hybrid::FusionOptions::default, |options| {
                            crate::hybrid::FusionOptions {
                                candidates: options.candidates.map(|v| v as usize),
                                rrf_k: options.rrf_k.unwrap_or(60),
                                vector_weight: options.vector_weight.unwrap_or(1.0),
                                text_weight: options.text_weight.unwrap_or(1.0),
                            }
                        });
                let query = crate::vectors::DenseSparseQuery {
                    dense_name: request.dense_name,
                    version: request.version,
                    dense: request.dense,
                    sparse_name: request.sparse_name,
                    sparse,
                    k: request.k as usize,
                    fusion,
                };
                service
                    .run_read(move |db| {
                        open_qualified(db, &role, &name)?;
                        let hits = db.collection(&name)?.search_dense_sparse(&query)?;
                        Ok(proto::SearchResponse {
                            hits: Vec::new(),
                            hybrid_hits: hits
                                .into_iter()
                                .map(|hit| proto::HybridHit {
                                    id: hit.id,
                                    score: hit.score,
                                    distance: hit.distance,
                                    bm25: hit.bm25,
                                    vector_rank: hit.vector_rank,
                                    text_rank: hit.text_rank,
                                })
                                .collect(),
                        })
                    })
                    .await
            },
        )
        .await
    }

    async fn query(
        &self,
        request: Request<proto::QueryRequest>,
    ) -> Result<Response<proto::QueryResponse>, Status> {
        let legacy = request.get_ref().collection.is_empty();
        self.call(
            "grpc:/vlite.v1.VLite/Query",
            request,
            Need::Op(Op::Search, Reach::Collection),
            true,
            legacy,
            |service, role, _, request| async move {
                let name = tenants::qualify(&role, target(request.collection).name());
                let parsed = crate::query_language::parse(&request.query)
                    .map_err(|e| unprocessable(e.to_string()))?;
                let parameters = request
                    .parameters
                    .into_iter()
                    .map(|(name, vector)| (name, vector.vector))
                    .collect();
                service
                    .run_read(move |db| {
                        open_qualified(db, &role, &name)?;
                        let output = parsed.execute(db.collection(&name)?, &parameters)?;
                        Ok(proto::QueryResponse {
                            hits: output
                                .hits
                                .into_iter()
                                .map(|hit| proto::Hit {
                                    id: hit.id,
                                    distance: hit.distance,
                                })
                                .collect(),
                            groups: output
                                .groups
                                .into_iter()
                                .map(|group| proto::SearchGroup {
                                    value: Some(value_out(group.value)),
                                    hits: group
                                        .hits
                                        .into_iter()
                                        .map(|hit| proto::Hit {
                                            id: hit.id,
                                            distance: hit.distance,
                                        })
                                        .collect(),
                                })
                                .collect(),
                            facets: output
                                .facets
                                .into_iter()
                                .map(|facet| proto::Facet {
                                    value: Some(value_out(facet.value)),
                                    count: facet.count,
                                })
                                .collect(),
                        })
                    })
                    .await
            },
        )
        .await
    }

    async fn changes(
        &self,
        request: Request<proto::ChangesRequest>,
    ) -> Result<Response<proto::ChangesResponse>, Status> {
        self.call(
            "grpc:/vlite.v1.VLite/Changes",
            request,
            Need::Admin,
            false,
            false,
            |service, _, _, request| async move {
                let cursor = if request.cursor.is_empty() {
                    crate::changes::FeedCursor::beginning()
                } else {
                    crate::changes::FeedCursor::from_token(&request.cursor)?
                };
                let limit = if request.limit == 0 {
                    crate::changes::DEFAULT_FEED_BATCH
                } else {
                    request.limit as usize
                };
                if limit > crate::changes::MAX_FEED_BATCH {
                    return Err(unprocessable("change limit exceeds 10000"));
                }
                service
                    .run_read(move |db| {
                        let batch = db.changes_after(&cursor, limit)?;
                        Ok(proto::ChangesResponse {
                            cursor: batch.next.to_token(),
                            has_more: batch.has_more,
                            head: batch.head,
                            changes: batch.changes.into_iter().map(change_out).collect(),
                        })
                    })
                    .await
            },
        )
        .await
    }

    type WatchChangesStream = std::pin::Pin<
        Box<
            dyn tokio_stream::Stream<Item = Result<proto::ChangesResponse, Status>>
                + Send
                + 'static,
        >,
    >;

    async fn watch_changes(
        &self,
        request: Request<proto::WatchChangesRequest>,
    ) -> Result<Response<Self::WatchChangesStream>, Status> {
        let headers = request.metadata().clone().into_headers();
        let opened = self
            .call(
                "grpc:/vlite.v1.VLite/WatchChanges",
                request,
                Need::Admin,
                false,
                false,
                |_service, _, request_id, request| async move {
                    let cursor = if request.cursor.is_empty() {
                        crate::changes::FeedCursor::beginning()
                    } else {
                        crate::changes::FeedCursor::from_token(&request.cursor)?
                    };
                    let limit = if request.limit == 0 {
                        crate::changes::DEFAULT_FEED_BATCH
                    } else {
                        request.limit as usize
                    };
                    if limit > crate::changes::MAX_FEED_BATCH {
                        return Err(unprocessable("change limit exceeds 10000"));
                    }
                    let interval = std::time::Duration::from_millis(
                        u64::from(request.poll_interval_ms).clamp(100, 60_000),
                    );
                    Ok((request_id, cursor, limit, interval))
                },
            )
            .await?;
        let (request_id, mut cursor, limit, interval) = opened.into_inner();
        let service = self.service.clone();
        let response_request_id = request_id.clone();
        let (sender, receiver) = tokio::sync::mpsc::channel(4);
        tokio::spawn(async move {
            loop {
                if sender.is_closed() {
                    return;
                }
                match poll_changes(&service, &headers, cursor, limit).await {
                    Ok(batch) => {
                        cursor = batch.next;
                        if !batch.changes.is_empty() || batch.has_more {
                            let response = proto::ChangesResponse {
                                cursor: cursor.to_token(),
                                has_more: batch.has_more,
                                head: batch.head,
                                changes: batch.changes.into_iter().map(change_out).collect(),
                            };
                            if sender.send(Ok(response)).await.is_err() {
                                return;
                            }
                            service
                                .metrics
                                .grpc_stream_messages
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(error) => {
                        record_stream_failure(
                            &service,
                            "grpc:/vlite.v1.VLite/WatchChanges",
                            &request_id,
                            &error,
                        );
                        let _ = sender.send(Err(status(error, &request_id))).await;
                        return;
                    }
                }
                tokio::select! {
                    _ = sender.closed() => return,
                    _ = tokio::time::sleep(interval) => {},
                }
            }
        });
        let mut response: Response<Self::WatchChangesStream> = Response::new(Box::pin(
            tokio_stream::wrappers::ReceiverStream::new(receiver),
        ));
        if let Ok(value) = response_request_id.parse() {
            response.metadata_mut().insert("x-request-id", value);
        }
        Ok(response)
    }

    async fn build_disk_ann(
        &self,
        request: Request<proto::BuildDiskAnnRequest>,
    ) -> Result<Response<proto::DiskAnnBuildResponse>, Status> {
        self.call(
            "grpc:/vlite.v1.VLite/BuildDiskAnn",
            request,
            Need::Op(Op::Index, Reach::Collection),
            false,
            false,
            |service, role, _, request| async move {
                if request.path.is_empty() {
                    return Err(unprocessable("disk ANN path is required"));
                }
                let name = tenants::qualify(&role, target(request.collection).name());
                let metric = metric_in(request.metric)?;
                let config = request
                    .config
                    .map(|c| crate::HnswConfig {
                        m: c.m.unwrap_or(16) as usize,
                        ef_construction: c.ef_construction.unwrap_or(200) as usize,
                        seed: c.seed.unwrap_or(42),
                    })
                    .unwrap_or_default();
                service
                    .run(move |db| {
                        let collection_id = open_qualified(db, &role, &name)?;
                        let source = db.build_disk_ann(&name, &request.path, config, metric)?;
                        Ok(proto::DiskAnnBuildResponse {
                            lineage: source.lineage,
                            sequence: source.sequence,
                            collection_id: u32::from(collection_id),
                            live_records: source.live_records,
                        })
                    })
                    .await
            },
        )
        .await
    }

    async fn search_disk_ann(
        &self,
        request: Request<proto::SearchDiskAnnRequest>,
    ) -> Result<Response<proto::SearchResponse>, Status> {
        let legacy = request.get_ref().collection.is_empty();
        self.call(
            "grpc:/vlite.v1.VLite/SearchDiskAnn",
            request,
            Need::Op(Op::Search, Reach::Collection),
            true,
            legacy,
            |service, role, _, request| async move {
                if request.path.is_empty() {
                    return Err(unprocessable("disk ANN path is required"));
                }
                let name = tenants::qualify(&role, target(request.collection).name());
                let options = crate::DiskAnnOptions {
                    cache_bytes: if request.cache_bytes == 0 {
                        crate::disk_ann::DEFAULT_CACHE_BYTES
                    } else {
                        request.cache_bytes as usize
                    },
                };
                let index = crate::DiskAnnIndex::open(&request.path, options)?;
                service
                    .run_read(move |db| {
                        open_qualified(db, &role, &name)?;
                        Ok(hits_out(db.search_disk_ann(
                            &index,
                            &request.vector,
                            request.k as usize,
                            request.ef_search as usize,
                        )?))
                    })
                    .await
            },
        )
        .await
    }

    async fn info(
        &self,
        request: Request<proto::InfoRequest>,
    ) -> Result<Response<proto::InfoResponse>, Status> {
        self.call(
            "grpc:/vlite.v1.VLite/Info",
            request,
            Need::Admin,
            false,
            false,
            |service, _, _, _| async move {
                service
                    .run_read(move |db| {
                        Ok(proto::InfoResponse {
                            format_version: db.format_version(),
                            sequence: db.sequence,
                            lineage: db.lineage(),
                            encrypted: db.is_encrypted(),
                            encryption_key_id: db.encryption_key_id(),
                            commit_timestamps: db.has_commit_timestamps(),
                            is_replica: false,
                            replica: None,
                        })
                    })
                    .await
            },
        )
        .await
    }

    async fn reserve_import(
        &self,
        request: Request<proto::ReserveImportRequest>,
    ) -> Result<Response<proto::ImportJsonResponse>, Status> {
        self.call(
            "grpc:/vlite.v1.VLite/ReserveImport",
            request,
            Need::Op(Op::Import, Reach::Collection),
            false,
            false,
            |service, role, _, request| async move {
                let value =
                    collections::reserve_core(&service, role, Target::Named(request.collection))
                        .await?;
                import_json(value)
            },
        )
        .await
    }

    async fn get_import_status(
        &self,
        request: Request<proto::ImportJobRequest>,
    ) -> Result<Response<proto::ImportJsonResponse>, Status> {
        self.call(
            "grpc:/vlite.v1.VLite/GetImportStatus",
            request,
            Need::Op(Op::Read, Reach::Collection),
            true,
            false,
            |service, role, _, request| async move {
                import_json(collections::status_core(&service, role, request.job).await?)
            },
        )
        .await
    }

    async fn import_chunk(
        &self,
        request: Request<proto::ImportChunkRequest>,
    ) -> Result<Response<proto::ImportJsonResponse>, Status> {
        self.call(
            "grpc:/vlite.v1.VLite/ImportChunk",
            request,
            Need::Op(Op::Import, Reach::Collection),
            false,
            false,
            |service, role, _, request| async move {
                apply_chunk(
                    &service,
                    role,
                    request.job,
                    request.chunk,
                    request.operations,
                    request.policy,
                )
                .await
                .and_then(import_json)
            },
        )
        .await
    }

    async fn finish_import(
        &self,
        request: Request<proto::ImportJobRequest>,
    ) -> Result<Response<proto::ImportJsonResponse>, Status> {
        self.close_import(request, false).await
    }

    async fn abort_import(
        &self,
        request: Request<proto::ImportJobRequest>,
    ) -> Result<Response<proto::ImportJsonResponse>, Status> {
        self.close_import(request, true).await
    }

    type ExportVectorsStream = std::pin::Pin<
        Box<dyn tokio_stream::Stream<Item = Result<proto::Vector, Status>> + Send + 'static>,
    >;

    async fn export_vectors(
        &self,
        request: Request<proto::ExportVectorsRequest>,
    ) -> Result<Response<Self::ExportVectorsStream>, Status> {
        let legacy = request.get_ref().collection.is_empty();
        let headers = request.metadata().clone().into_headers();
        // The unary pipeline authenticates, authorizes, validates and records
        // the call; each page below re-authenticates so revocation applies
        // mid-stream.
        let opened = self
            .call(
                "grpc:/vlite.v1.VLite/ExportVectors",
                request,
                Need::Op(Op::Read, Reach::Collection),
                true,
                legacy,
                |service, role, request_id, request| async move {
                    let page = match request.page_size {
                        0 => 256,
                        size @ 1..=1000 => size as usize,
                        _ => return Err(unprocessable("page_size must be 0..1000")),
                    };
                    let target = target(request.collection);
                    let check = target.clone();
                    service
                        .run_read(move |db| open(db, &role, &check).map(|_| ()))
                        .await?;
                    Ok((request_id, target, request.after, page))
                },
            )
            .await?;
        let (request_id, target, mut after, page) = opened.into_inner();
        let service = self.service.clone();
        let (sender, receiver) = tokio::sync::mpsc::channel(page.min(64));
        let header_id = request_id.clone();
        tokio::spawn(async move {
            const ROUTE: &str = "grpc:/vlite.v1.VLite/ExportVectors";
            loop {
                let result = export_page(&service, &headers, &target, after, page).await;
                let (records, more) = match result {
                    Ok(page) => page,
                    Err(error) => {
                        record_stream_failure(&service, ROUTE, &request_id, &error);
                        let _ = sender.send(Err(status(error, &request_id))).await;
                        return;
                    }
                };
                for record in records {
                    after = Some(record.id);
                    if sender.send(Ok(record)).await.is_err() {
                        return;
                    }
                    service
                        .metrics
                        .grpc_stream_messages
                        .fetch_add(1, Ordering::Relaxed);
                }
                if !more {
                    return;
                }
            }
        });
        let mut response: Response<Self::ExportVectorsStream> = Response::new(Box::pin(
            tokio_stream::wrappers::ReceiverStream::new(receiver),
        ));
        if let Ok(value) = header_id.parse() {
            response.metadata_mut().insert("x-request-id", value);
        }
        Ok(response)
    }

    async fn stream_import(
        &self,
        request: Request<tonic::Streaming<proto::StreamImportRequest>>,
    ) -> Result<Response<proto::ImportJsonResponse>, Status> {
        self.call(
            "grpc:/vlite.v1.VLite/StreamImport",
            request,
            Need::Op(Op::Import, Reach::Collection),
            false,
            false,
            |service, role, _, mut stream| async move {
                let mut job = None;
                let mut finished = None;
                while let Some(message) = stream.message().await.map_err(|error| {
                    ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "stream_error",
                        format!("import stream failed: {}", error.message()),
                    )
                })? {
                    service
                        .metrics
                        .grpc_stream_messages
                        .fetch_add(1, Ordering::Relaxed);
                    if finished.is_some() {
                        return Err(unprocessable("finish must be on the last stream message"));
                    }
                    if *job.get_or_insert(message.job) != message.job {
                        return Err(unprocessable("every stream message must name the same job"));
                    }
                    let control_only = message.chunk == 0 && message.operations.is_empty();
                    if !(control_only && message.finish) {
                        apply_chunk(
                            &service,
                            role.clone(),
                            message.job,
                            message.chunk,
                            message.operations,
                            message.policy,
                        )
                        .await?;
                    }
                    if message.finish {
                        finished = Some(
                            collections::close_core(&service, role.clone(), message.job, false)
                                .await?,
                        );
                    }
                }
                match (finished, job) {
                    (Some(value), _) => import_json(value),
                    (None, Some(job)) => {
                        import_json(collections::status_core(&service, role, job).await?)
                    }
                    (None, None) => Err(unprocessable("import stream contained no messages")),
                }
            },
        )
        .await
    }
}

/// Reads one atomic page of records after `after` with a fresh admission
/// permit and authentication. Returns the page and whether more remain.
async fn export_page(
    service: &Service,
    headers: &axum::http::HeaderMap,
    target: &Target,
    after: Option<u64>,
    page: usize,
) -> Result<(Vec<proto::Vector>, bool), ApiError> {
    let _permit = service
        .requests
        .clone()
        .acquire_owned()
        .await
        .map_err(internal)?;
    let role = authenticate(service, headers)?.ok_or_else(|| {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "valid bearer token required",
        )
    })?;
    let target = target.clone();
    service
        .run_read(move |db| {
            use std::ops::Bound::{Excluded, Unbounded};
            let collection = open(db, &role, &target)?;
            let state = db.state(collection);
            let mut records = state
                .records
                .range((after.map_or(Unbounded, Excluded), Unbounded))
                .peekable();
            let mut items = Vec::with_capacity(page);
            while items.len() < page {
                let Some((&id, record)) = records.next() else {
                    break;
                };
                let revision = db.revision_in(collection, id)?.unwrap_or_default();
                items.push(proto::Vector {
                    id,
                    vector: record.vector.clone(),
                    payload: record.payload.clone(),
                    metadata: metadata_out(record.metadata.clone()),
                    revision,
                    expires_at_ms: state.expiries.get(&id).copied(),
                });
            }
            Ok((items, records.peek().is_some()))
        })
        .await
}

/// ImportChunk semantics shared by the unary and streaming RPCs.
async fn apply_chunk(
    service: &Service,
    role: admin::Role,
    job: u64,
    chunk: u64,
    operations: Vec<proto::Operation>,
    policy: i32,
) -> Result<Value, ApiError> {
    let policy = match proto::ImportPolicy::try_from(policy) {
        Ok(proto::ImportPolicy::RejectChunk) => crate::InvalidRowPolicy::RejectChunk,
        Ok(proto::ImportPolicy::SkipInvalid) => crate::InvalidRowPolicy::SkipInvalid,
        Err(_) => return Err(unprocessable("unknown import policy")),
    };
    if operations.len() > MAX_BATCH_OPERATIONS {
        return Err(Error::BatchTooLarge.into());
    }
    let operations = operations
        .into_iter()
        .map(mutation)
        .collect::<Result<Vec<_>, _>>()?;
    let value =
        collections::import_chunk_core(service, role, job, chunk, operations, policy).await?;
    if value["outcome"] == "rejected" {
        let details = serde_json::to_string(&value).map_err(internal)?;
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "import_rows_invalid",
            "chunk rejected; nothing was written; correct invalid rows or use skip_invalid",
        )
        .with_details(details));
    }
    Ok(value)
}

mod tokio_stream {
    pub use tonic::codegen::tokio_stream::*;
}

impl Adapter {
    async fn close_import(
        &self,
        request: Request<proto::ImportJobRequest>,
        abort: bool,
    ) -> Result<Response<proto::ImportJsonResponse>, Status> {
        let route = if abort {
            "grpc:/vlite.v1.VLite/AbortImport"
        } else {
            "grpc:/vlite.v1.VLite/FinishImport"
        };
        self.call(
            route,
            request,
            Need::Op(Op::Import, Reach::Collection),
            false,
            false,
            move |service, role, _, request| async move {
                import_json(collections::close_core(&service, role, request.job, abort).await?)
            },
        )
        .await
    }
}

#[cfg(feature = "otel")]
fn request_collection<T: 'static>(request: &T) -> String {
    let request = request as &dyn std::any::Any;
    macro_rules! field {
        ($type:ty, $field:ident) => {
            if let Some(r) = request.downcast_ref::<$type>() {
                return r.$field.clone();
            }
        };
    }
    field!(proto::GetCollectionRequest, name);
    field!(proto::CreateCollectionRequest, name);
    field!(proto::DropCollectionRequest, name);
    field!(proto::GetVectorRequest, collection);
    field!(proto::PutVectorRequest, collection);
    field!(proto::DeleteVectorRequest, collection);
    field!(proto::WriteBatchRequest, collection);
    field!(proto::SearchRequest, collection);
    field!(proto::ReserveImportRequest, collection);
    field!(proto::ExportVectorsRequest, collection);
    String::new()
}

/// LEG-017 (TEST-015): graph errors keep explicit gRPC codes and the stable
/// REST code in the trailer.
#[cfg(test)]
mod graph_errors {
    use super::*;

    #[test]
    fn every_graph_error_maps_to_failed_precondition_or_invalid_argument() {
        for (error, http, code) in crate::server::graph_errors::table() {
            let expected = match http.as_u16() {
                409 => tonic::Code::FailedPrecondition,
                422 => tonic::Code::InvalidArgument,
                other => panic!("{code}: unexpected HTTP {other}"),
            };
            let status = status(ApiError::from(error), "graph-errors");
            assert_eq!(status.code(), expected, "{code}");
            assert_eq!(
                status
                    .metadata()
                    .get(ERROR_CODE_TRAILER)
                    .and_then(|value| value.to_str().ok()),
                Some(code)
            );
        }
    }
}
