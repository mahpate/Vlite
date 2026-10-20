//! Optional HTTP transport over one exclusively owned embedded database.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Path, Query, State,
        rejection::{JsonRejection, PathRejection, QueryRejection},
    },
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, prelude::BASE64_STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;

use crate::{Database, Error, MAX_BATCH_OPERATIONS, MAX_PAYLOAD_BYTES, Mutation};

pub const MAX_JSON_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_SEARCH_K: usize = 10_000;
const MAX_IN_FLIGHT: usize = 16;

#[derive(Default)]
struct Metrics {
    requests: AtomicU64,
    failures: AtomicU64,
    unauthorized: AtomicU64,
    duration_ns: AtomicU64,
    commit_failures: AtomicU64,
    rebuilds: AtomicU64,
    rebuild_search_ns: AtomicU64,
}

/// Validated high-entropy bearer token. Intentionally does not implement Debug.
#[derive(Clone)]
pub struct BearerToken([u8; 256]);

impl BearerToken {
    pub fn new(value: &str) -> Result<Self, &'static str> {
        if !(32..=256).contains(&value.len())
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err("token must be 32..256 base64url characters; generate a random secret");
        }
        let mut bytes = [0; 256];
        bytes[..value.len()].copy_from_slice(value.as_bytes());
        Ok(Self(bytes))
    }

    fn accepts(&self, value: &str) -> bool {
        let Ok(candidate) = Self::new(value) else {
            return false;
        };
        bool::from(self.0.ct_eq(&candidate.0))
    }
}

/// Cloneable service state. Database work runs off the asynchronous I/O threads.
#[derive(Clone)]
pub struct Service {
    database: Arc<Mutex<Option<Database>>>,
    requests: Arc<Semaphore>,
    workers: Arc<Semaphore>,
    metrics: Arc<Metrics>,
}

impl Service {
    pub fn new(database: Database) -> Self {
        Self {
            database: Arc::new(Mutex::new(Some(database))),
            requests: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
            workers: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
            metrics: Arc::new(Metrics::default()),
        }
    }

    async fn run<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut Database) -> Result<T, ApiError> + Send + 'static,
    ) -> Result<T, ApiError> {
        let permit = self.workers.clone().try_acquire_owned().map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "busy",
                "database queue is full",
            )
        })?;
        let database = self.database.clone();
        let metrics = self.metrics.clone();
        tokio::task::spawn_blocking(move || {
            // The permit stays with the worker even if the HTTP client disconnects.
            let _permit = permit;
            let mut owner = database.lock().map_err(internal)?;
            let db = owner.as_mut().ok_or_else(unavailable)?;
            let result = operation(db);
            if result
                .as_ref()
                .is_err_and(|error| error.code == "commit_indeterminate")
            {
                metrics.commit_failures.fetch_add(1, Ordering::Relaxed);
            }
            result
        })
        .await
        .map_err(internal)?
    }

    /// Call after HTTP graceful shutdown; waits for any disconnected writer too.
    pub async fn close(&self) -> crate::Result<()> {
        let database = self.database.clone();
        tokio::task::spawn_blocking(move || {
            let mut owner = database
                .lock()
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            if let Some(db) = owner.take() {
                db.close()?;
            }
            Ok(())
        })
        .await
        .map_err(|error| std::io::Error::other(error.to_string()))?
    }
}

/// Unauthenticated router for explicitly local development or in-process use.
/// Network callers should use `authenticated_router`.
pub fn router(service: Service) -> Router {
    configured_router(service, None)
}

/// Protects data routes with one bearer token; health and static dashboard assets are public.
pub fn authenticated_router(service: Service, token: BearerToken) -> Router {
    configured_router(service, Some(token))
}

fn configured_router(service: Service, token: Option<BearerToken>) -> Router {
    let observation = (service.clone(), token);
    Router::new()
        .route("/health", get(|| async { Json(json!({"status": "ok"})) }))
        .route(
            "/dashboard",
            get(|| async {
                dashboard_asset(
                    "text/html; charset=utf-8",
                    include_str!("dashboard/index.html"),
                )
            }),
        )
        .route(
            "/dashboard/app.js",
            get(|| async {
                dashboard_asset(
                    "text/javascript; charset=utf-8",
                    include_str!("dashboard/app.js"),
                )
            }),
        )
        .route(
            "/dashboard/style.css",
            get(|| async {
                dashboard_asset(
                    "text/css; charset=utf-8",
                    include_str!("dashboard/style.css"),
                )
            }),
        )
        .route("/ready", get(ready))
        .route("/metrics", get(metrics))
        .route("/v1/database", get(info))
        .route("/v1/vectors", get(list_vectors))
        .route("/v1/vectors/{id}", get(fetch).put(put).delete(delete))
        .route("/v1/batches", post(batch))
        .route("/v1/search", post(search))
        .route("/v1/index/hnsw", get(index_status).post(build_index))
        .route(
            "/openapi.json",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "application/json")],
                    include_str!("openapi.json"),
                )
            }),
        )
        .fallback(|| async { ApiError::new(StatusCode::NOT_FOUND, "not_found", "route not found") })
        .method_not_allowed_fallback(|| async {
            ApiError::new(
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                "method not allowed",
            )
        })
        .layer(DefaultBodyLimit::max(MAX_JSON_BYTES))
        .layer(middleware::from_fn_with_state(service.clone(), admission))
        .layer(middleware::from_fn_with_state(observation, observe))
        .with_state(service)
}

fn dashboard_asset(content_type: &'static str, content: &'static str) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
            ),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        content,
    )
}

async fn observe(
    State((service, token)): State<(Service, Option<BearerToken>)>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let started = Instant::now();
    let number = service.metrics.requests.fetch_add(1, Ordering::Relaxed) + 1;
    let request_id = format!("{}-{number}", std::process::id());
    let route = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map_or("unmatched", |path| path.as_str())
        .to_owned();
    let method = match request.method().as_str() {
        "GET" => "GET",
        "POST" => "POST",
        "PUT" => "PUT",
        "DELETE" => "DELETE",
        _ => "other",
    };
    let authorized = matches!(
        request.uri().path(),
        "/health" | "/dashboard" | "/dashboard/app.js" | "/dashboard/style.css"
    ) || token.as_ref().is_none_or(|token| {
        let mut values = request.headers().get_all(header::AUTHORIZATION).iter();
        let first = values.next();
        values.next().is_none()
            && first
                .and_then(|h| h.to_str().ok())
                .and_then(|h| h.split_once(' '))
                .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
                .map(|(_, value)| value)
                .is_some_and(|value| token.accepts(value))
    });
    let mut response = if authorized {
        next.run(request).await
    } else {
        service.metrics.unauthorized.fetch_add(1, Ordering::Relaxed);
        let mut response = ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "valid bearer token required",
        )
        .into_response();
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            axum::http::HeaderValue::from_static("Bearer"),
        );
        response
    };
    response.headers_mut().insert(
        "x-request-id",
        request_id.parse().expect("generated ASCII ID"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    let elapsed = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
    service
        .metrics
        .duration_ns
        .fetch_add(elapsed, Ordering::Relaxed);
    if response.status().is_client_error() || response.status().is_server_error() {
        service.metrics.failures.fetch_add(1, Ordering::Relaxed);
    }
    eprintln!(
        "{}",
        json!({"event":"request", "request_id":request_id, "method":method,
        "route":route, "status":response.status().as_u16(), "duration_ns":elapsed})
    );
    response
}

async fn metrics(State(service): State<Service>) -> Result<Json<Value>, ApiError> {
    let metrics = service.metrics.clone();
    service.run(move |db| {
        let healthy = !db.poisoned;
        let database_bytes = if healthy {
            Some(db.file.0.metadata().map_err(crate::Error::from)?.len())
        } else {
            None
        };
        let (buffered_ids, buffer_bytes) = db.index.as_ref().map_or((0,0), |index| index.buffer_usage());
        Ok(Json(json!({
            "requests_total":metrics.requests.load(Ordering::Relaxed),
            "request_failures_total":metrics.failures.load(Ordering::Relaxed),
            "unauthorized_total":metrics.unauthorized.load(Ordering::Relaxed),
            "request_duration_seconds_total":metrics.duration_ns.load(Ordering::Relaxed) as f64 / 1e9,
            "commit_failures_total":metrics.commit_failures.load(Ordering::Relaxed),
            "hnsw_rebuilds_total":metrics.rebuilds.load(Ordering::Relaxed),
            "hnsw_buffered_ids":buffered_ids,
            "hnsw_buffer_vector_bytes":buffer_bytes,
            "hnsw_rebuild_and_search_seconds_total":metrics.rebuild_search_ns.load(Ordering::Relaxed) as f64 / 1e9,
            "database_healthy":healthy,
            "database_bytes":database_bytes,
            "live_records":healthy.then(|| db.len())
        })))
    }).await
}

async fn admission(
    State(service): State<Service>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let Ok(_permit) = service.requests.clone().try_acquire_owned() else {
        return ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "busy",
            "too many in-flight requests",
        )
        .into_response();
    };
    next.run(request).await
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({"error": {"code": self.code, "message": self.message}})),
        )
            .into_response()
    }
}

fn internal(_error: impl std::fmt::Display) -> ApiError {
    eprintln!("{}", json!({"event":"internal_error"}));
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal_error",
        "internal service error; inspect server logs",
    )
}

fn unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "unavailable",
        "database unavailable",
    )
}

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        match error {
            Error::IndexNotReady => Self::new(
                StatusCode::CONFLICT,
                "index_not_ready",
                "HNSW index is not ready; POST /v1/index/hnsw during maintenance, or explicitly choose exact/lazy approximate search",
            ),
            Error::DimensionMismatch { .. }
            | Error::InvalidDimensions(_)
            | Error::NonFiniteVector
            | Error::ZeroNormQuery
            | Error::UnsupportedAnnMetric
            | Error::PayloadTooLarge(_)
            | Error::BatchTooLarge
            | Error::InvalidHnswConfig
            | Error::InvalidEfSearch
            | Error::InvalidMetadata(_)
            | Error::InvalidFilter(_) => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_input",
                error.to_string(),
            ),
            Error::BatchRequiresUpgrade | Error::MetadataRequiresUpgrade => {
                Self::new(StatusCode::CONFLICT, "upgrade_required", error.to_string())
            }
            Error::CommitIndeterminate(_) => {
                eprintln!("{}", json!({"event":"commit_indeterminate"}));
                Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "commit_indeterminate",
                    "commit may have succeeded; do not blindly retry; stop and inspect the database",
                )
            }
            Error::Poisoned => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "poisoned",
                "database requires operator inspection and reopen after a failed commit",
            ),
            error => internal(error),
        }
    }
}

fn body<T>(input: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    input
        .map(|Json(value)| value)
        .map_err(|error| ApiError::new(error.status(), "invalid_json", error.body_text()))
}

fn id(text: &str) -> Result<u64, ApiError> {
    if text.is_empty()
        || (text.len() > 1 && text.starts_with('0'))
        || !text.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_id",
            "ID must be a canonical decimal u64 string",
        ));
    }
    text.parse().map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_id",
            "ID exceeds u64 range",
        )
    })
}

fn path(input: Result<Path<String>, PathRejection>) -> Result<u64, ApiError> {
    let Path(text) = input
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, "invalid_id", error.body_text()))?;
    id(&text)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListVectors {
    after: Option<String>,
    limit: Option<usize>,
}

async fn list_vectors(
    State(service): State<Service>,
    query: Result<Query<ListVectors>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(query) = query.map_err(|error| {
        ApiError::new(StatusCode::BAD_REQUEST, "invalid_query", error.body_text())
    })?;
    let after = query.after.as_deref().map(id).transpose()?;
    let limit = query.limit.unwrap_or(25);
    if !(1..=100).contains(&limit) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
            "limit must be 1..100",
        ));
    }
    service
        .run(move |db| {
            db.ensure_healthy()?;
            use std::ops::Bound::{Excluded, Unbounded};
            let mut records = db
                .records
                .range((after.map_or(Unbounded, Excluded), Unbounded))
                .peekable();
            let mut items = Vec::with_capacity(limit);
            // Reserve envelope space; even maximally escaped single-record metadata fits.
            let mut bytes = 1024usize;
            let mut last = None;
            while let Some(&(&record_id, record)) = records.peek() {
                if items.len() == limit {
                    break;
                }
                let item = json!({"id":record_id.to_string(), "metadata":record.metadata});
                let size = serde_json::to_vec(&item).map_err(internal)?.len() + 1;
                if bytes + size > 1_048_576 {
                    break;
                }
                bytes += size;
                items.push(item);
                last = Some(record_id);
                records.next();
            }
            let next_after = if records.peek().is_some() {
                last.map(|id| id.to_string())
            } else {
                None
            };
            Ok(Json(json!({"items":items, "next_after":next_after})))
        })
        .await
}

async fn index_status(State(service): State<Service>) -> Result<Json<Value>, ApiError> {
    service.run(|db| {
        db.ensure_healthy()?;
        let config = crate::HnswConfig::default();
        let ready = db.index.as_ref().is_some_and(|index| index.config == config);
        let (buffered_ids, buffer_vector_bytes) = db.index.as_ref().map_or((0, 0), |index| index.buffer_usage());
        Ok(Json(json!({
            "ready":ready, "buffered_ids":buffered_ids, "buffer_vector_bytes":buffer_vector_bytes,
            "metric":db.index.as_ref().map(|index| index.metric),
            "max_buffer_ids":crate::MAX_HNSW_BUFFER_IDS,
            "max_buffer_vector_bytes":crate::MAX_HNSW_BUFFER_VECTOR_BYTES,
            "config":{"m":config.m, "ef_construction":config.ef_construction, "seed":config.seed},
        })))
    }).await
}

fn payload(text: &str) -> Result<Vec<u8>, ApiError> {
    if text.len() > MAX_PAYLOAD_BYTES.div_ceil(3) * 4 {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
            format!("payload exceeds {MAX_PAYLOAD_BYTES} decoded bytes"),
        ));
    }
    let bytes = BASE64_STANDARD.decode(text).map_err(|_| {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_payload",
            "payload_base64 must be canonical padded standard base64",
        )
    })?;
    if bytes.len() > MAX_PAYLOAD_BYTES {
        return Err(Error::PayloadTooLarge(bytes.len()).into());
    }
    Ok(bytes)
}

async fn ready(State(service): State<Service>) -> Result<Json<Value>, ApiError> {
    service
        .run(|db| {
            db.get(0)?;
            Ok(Json(json!({"status": "ready"})))
        })
        .await
}

async fn info(State(service): State<Service>) -> Result<Json<Value>, ApiError> {
    service
        .run(|db| {
            db.get(0)?;
            Ok(Json(json!({
                "dimensions": db.dimensions(),
                "format_version": db.format_version(),
                "records": db.len()
            })))
        })
        .await
}

async fn fetch(
    State(service): State<Service>,
    input: Result<Path<String>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = path(input)?;
    service
        .run(move |db| {
            let record = db.get(id)?.ok_or_else(|| {
                ApiError::new(StatusCode::NOT_FOUND, "not_found", "vector not found")
            })?;
            Ok(Json(json!({
                "id": id.to_string(),
                "vector": record.vector,
                "payload_base64": BASE64_STANDARD.encode(&record.payload),
                "metadata":record.metadata,
            })))
        })
        .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Put {
    vector: Vec<f32>,
    #[serde(default)]
    payload_base64: String,
    #[serde(default)]
    metadata: crate::Metadata,
}

async fn put(
    State(service): State<Service>,
    path_input: Result<Path<String>, PathRejection>,
    input: Result<Json<Put>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let id = path(path_input)?;
    let input = body(input)?;
    let payload = payload(&input.payload_base64)?;
    service
        .run(move |db| {
            db.put_with_metadata(id, &input.vector, &payload, &input.metadata)?;
            Ok(StatusCode::NO_CONTENT)
        })
        .await
}

async fn delete(
    State(service): State<Service>,
    input: Result<Path<String>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = path(input)?;
    service
        .run(move |db| Ok(Json(json!({"deleted": db.delete(id)?}))))
        .await
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "lowercase", deny_unknown_fields)]
enum Operation {
    Put {
        id: String,
        vector: Vec<f32>,
        #[serde(default)]
        payload_base64: String,
        #[serde(default)]
        metadata: crate::Metadata,
    },
    Delete {
        id: String,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Batch {
    operations: Vec<Operation>,
}

async fn batch(
    State(service): State<Service>,
    input: Result<Json<Batch>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let input = body(input)?;
    if input.operations.len() > MAX_BATCH_OPERATIONS {
        return Err(Error::BatchTooLarge.into());
    }
    let operations = input
        .operations
        .into_iter()
        .map(|operation| match operation {
            Operation::Put {
                id: text,
                vector,
                payload_base64,
                metadata,
            } => {
                let id = id(&text)?;
                let payload = payload(&payload_base64)?;
                if metadata.is_empty() {
                    Ok(Mutation::Put {
                        id,
                        vector,
                        payload,
                    })
                } else {
                    Ok(Mutation::PutWithMetadata {
                        id,
                        vector,
                        payload,
                        metadata,
                    })
                }
            }
            Operation::Delete { id: text } => Ok(Mutation::Delete { id: id(&text)? }),
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    service
        .run(move |db| {
            db.write_batch(&operations)?;
            Ok(StatusCode::NO_CONTENT)
        })
        .await
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(try_from = "String")]
enum SearchMode {
    #[default]
    Exact,
    Approximate,
    ApproximateReady,
}

impl TryFrom<String> for SearchMode {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "exact" => Ok(Self::Exact),
            "approximate" => Ok(Self::Approximate),
            "approximate_ready" => Ok(Self::ApproximateReady),
            _ => Err("mode must be exact, approximate or approximate_ready"),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Search {
    vector: Vec<f32>,
    k: usize,
    #[serde(default)]
    mode: SearchMode,
    #[serde(default)]
    metric: crate::DistanceMetric,
    ef_search: Option<usize>,
    filter: Option<Vec<crate::Condition>>,
}

async fn search(
    State(service): State<Service>,
    input: Result<Json<Search>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let input = body(input)?;
    if input.k > MAX_SEARCH_K {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
            format!("k must be at most {MAX_SEARCH_K}"),
        ));
    }
    if matches!(input.mode, SearchMode::Exact) && input.ef_search.is_some() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
            "ef_search is only valid in approximate mode",
        ));
    }
    if !matches!(input.mode, SearchMode::Exact) && input.filter.is_some() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
            "filters require mode exact; approximate filtering is not supported",
        ));
    }
    if !matches!(input.mode, SearchMode::Exact)
        && input.metric == crate::DistanceMetric::InnerProduct
    {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
            "approximate search supports squared_l2 and cosine, not inner_product",
        ));
    }
    let metrics = service.metrics.clone();
    service
        .run(move |db| {
            let rebuilding = matches!(input.mode, SearchMode::Approximate)
                && input.k > 0
                && !db.is_empty()
                && db.index.as_ref().is_none_or(|index| {
                    index.metric != input.metric || index.config != crate::HnswConfig::default()
                });
            let started = Instant::now();
            let hits = match input.mode {
                SearchMode::Exact => db.search_filtered_with_metric(
                    &input.vector,
                    input.k,
                    input.filter.as_deref().unwrap_or(&[]),
                    input.metric,
                )?,
                SearchMode::Approximate => db.search_approximate_with_metric(
                    &input.vector,
                    input.k,
                    input.ef_search.unwrap_or(100.max(input.k.min(db.len()))),
                    crate::HnswConfig::default(),
                    input.metric,
                )?,
                SearchMode::ApproximateReady => db.search_approximate_ready_with_metric(
                    &input.vector,
                    input.k,
                    input.ef_search.unwrap_or(100.max(input.k.min(db.len()))),
                    crate::HnswConfig::default(),
                    input.metric,
                )?,
            };
            if rebuilding && db.index.is_some() {
                metrics.rebuilds.fetch_add(1, Ordering::Relaxed);
                metrics.rebuild_search_ns.fetch_add(
                    started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
                    Ordering::Relaxed,
                );
            }
            let hits = hits
                .into_iter()
                .map(|hit| json!({"id": hit.id.to_string(), "distance": hit.distance}))
                .collect::<Vec<_>>();
            Ok(Json(json!({"hits": hits})))
        })
        .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BuildIndex {
    #[serde(default)]
    metric: crate::DistanceMetric,
}

async fn build_index(
    State(service): State<Service>,
    input: Result<Json<Value>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let input = body(input)?;
    if !input.is_object() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
            "index build requires a JSON object",
        ));
    }
    let input: BuildIndex = serde_json::from_value(input).map_err(|error| {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
            error.to_string(),
        )
    })?;
    let metrics = service.metrics.clone();
    service
        .run(move |db| {
            let started = Instant::now();
            db.build_hnsw_with_metric(crate::HnswConfig::default(), input.metric)?;
            metrics.rebuilds.fetch_add(1, Ordering::Relaxed);
            metrics.rebuild_search_ns.fetch_add(
                started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
                Ordering::Relaxed,
            );
            Ok(StatusCode::NO_CONTENT)
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    #[tokio::test]
    async fn disconnected_worker_keeps_its_permit_and_close_waits_for_its_commit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("drain.vlite");
        let service = Service::new(Database::create(&path, 1).unwrap());
        let worker = service.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let request = tokio::spawn(async move {
            worker
                .run(move |db| {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    db.put(7, &[1.0], b"after disconnect")?;
                    Ok(())
                })
                .await
        });
        started_rx.await.unwrap();
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        assert_eq!(service.workers.available_permits(), MAX_IN_FLIGHT - 1);
        let owner = service.clone();
        let closing = tokio::spawn(async move { owner.close().await });
        release_tx.send(()).unwrap();
        closing.await.unwrap().unwrap();
        let permits = service
            .workers
            .clone()
            .acquire_many_owned(MAX_IN_FLIGHT as u32)
            .await
            .unwrap();
        drop(permits);
        assert_eq!(service.workers.available_permits(), MAX_IN_FLIGHT);
        let db = Database::open(path).unwrap();
        assert_eq!(db.get(7).unwrap().unwrap().payload, b"after disconnect");
    }

    #[tokio::test]
    async fn busy_and_poisoned_services_never_report_ready_or_accept_mutations() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("poison.vlite");
        let mut db = Database::create(&path, 1).unwrap();
        db.file = crate::LockedFile(std::fs::File::open(&path).unwrap());
        assert!(matches!(
            db.put(0, &[1.0], b""),
            Err(Error::CommitIndeterminate(_))
        ));
        let service = Service::new(db);
        let app = router(service.clone());
        for uri in ["/ready", "/v1/database", "/v1/vectors/0"] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        }
        let permit = service
            .requests
            .clone()
            .acquire_many_owned(MAX_IN_FLIGHT as u32)
            .await
            .unwrap();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        drop(permit);
        let worker_permit = service
            .workers
            .clone()
            .acquire_many_owned(MAX_IN_FLIGHT as u32)
            .await
            .unwrap();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        drop(worker_permit);
        assert!(matches!(service.close().await, Err(Error::Poisoned)));
        let error = ApiError::from(Error::CommitIndeterminate(std::io::Error::other(
            "injected",
        )));
        assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error.code, "commit_indeterminate");
    }

    #[tokio::test]
    async fn commit_failure_metrics_count_worker_failures() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("failure.vlite");
        let mut db = Database::create(&path, 1).unwrap();
        db.file = crate::LockedFile(std::fs::File::open(&path).unwrap());
        let service = Service::new(db);
        let error = service
            .run(|db| {
                db.put(1, &[1.0], b"")?;
                Ok(())
            })
            .await
            .unwrap_err();
        assert_eq!(error.code, "commit_indeterminate");
        assert_eq!(service.metrics.commit_failures.load(Ordering::Relaxed), 1);
        let snapshot = metrics(State(service.clone())).await.unwrap().0;
        assert_eq!(snapshot["commit_failures_total"], 1);
        assert_eq!(snapshot["database_healthy"], false);
        assert!(snapshot["database_bytes"].is_null());
        assert!(snapshot["live_records"].is_null());
        assert!(service.close().await.is_err());
    }
}
