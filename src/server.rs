//! Optional HTTP transport over one exclusively owned embedded database.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use axum::{
    Extension, Json, Router,
    extract::{
        DefaultBodyLimit, MatchedPath, Path, Query, State,
        rejection::{JsonRejection, PathRejection, QueryRejection},
    },
    http::{HeaderMap, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use base64::{Engine, prelude::BASE64_STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;

use crate::{
    AnnOptions, CollectionRef, Database, DistanceMetric, Error, Expected, HnswConfig,
    MAX_BATCH_OPERATIONS, MAX_PAYLOAD_BYTES, Mutation, Quantization, Record, SearchStats,
};
mod admin;
mod audit;
mod collections;
mod credentials;
mod features;
mod group_commit;
pub use group_commit::GroupCommitConfig;
#[cfg(feature = "grpc")]
pub mod grpc;
mod logs;
mod observability;
mod ops;
#[cfg(feature = "otel")]
pub mod telemetry;
mod tenants;
pub mod tls;
mod webhooks;
pub use admin::{ServerOptions, configured_admin_router, read_token_file, selected_database};

/// Emit a legacy stderr line and also capture it in the bounded server log buffer.
pub fn log_stderr(level: &'static str, target: &'static str, message: impl Into<String>) {
    logs::stderr(level, target, message);
}

pub const MAX_JSON_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_SEARCH_K: usize = 10_000;
const MAX_IN_FLIGHT: usize = 16;
const SLOW_QUERY_CAPACITY: usize = 256;

#[derive(Default)]
struct Metrics {
    requests: AtomicU64,
    failures: AtomicU64,
    unauthorized: AtomicU64,
    duration_ns: AtomicU64,
    grpc_requests: AtomicU64,
    grpc_failures: AtomicU64,
    grpc_stream_messages: AtomicU64,
    commit_failures: AtomicU64,
    group_commit_groups: AtomicU64,
    group_commit_requests: AtomicU64,
    rebuilds: AtomicU64,
    rebuild_search_ns: AtomicU64,
    /// Nanoseconds of the last synchronous HNSW build plus one; 0 = none yet.
    last_rebuild_ns: AtomicU64,
    background_started: AtomicU64,
    background_published: AtomicU64,
    by_strategy: Mutex<BTreeMap<&'static str, u64>>,
    graph_nodes_visited: AtomicU64,
    distance_evaluations: AtomicU64,
    quantized_distance_evaluations: AtomicU64,
    reranked: AtomicU64,
    slow_queries: AtomicU64,
    slow: Mutex<VecDeque<Value>>,
    slow_threshold_ns: AtomicU64,
    in_flight: AtomicU64,
    by_route: Mutex<BTreeMap<(String, String), RouteStats>>,
    import_chunks: AtomicU64,
    import_rows_skipped: AtomicU64,
    credential_denials: AtomicU64,
    audit_failures: AtomicU64,
    rate_limited: AtomicU64,
    quota_exceeded: AtomicU64,
    /// f64 bits of the last sampled recall plus a presence flag.
    ann_quality_last_recall: Mutex<Option<f64>>,
}

#[derive(Default)]
struct RouteStats {
    count: u64,
    errors: u64,
    durations_ms: VecDeque<u64>,
}

impl Metrics {
    fn search(&self, strategy: &'static str, stats: &SearchStats) {
        if let Ok(mut counts) = self.by_strategy.lock() {
            *counts.entry(strategy).or_default() += 1;
        }
        self.graph_nodes_visited
            .fetch_add(stats.graph_nodes_visited, Ordering::Relaxed);
        self.distance_evaluations
            .fetch_add(stats.distance_evaluations, Ordering::Relaxed);
        self.quantized_distance_evaluations
            .fetch_add(stats.quantized_distance_evaluations, Ordering::Relaxed);
        self.reranked
            .fetch_add(stats.reranked_candidates, Ordering::Relaxed);
    }

    fn built(&self, started: Instant) {
        let ns = started.elapsed().as_nanos().min(u64::MAX as u128 - 1) as u64;
        self.last_rebuild_ns.store(ns + 1, Ordering::Relaxed);
    }
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
    database: Arc<RwLock<Option<Database>>>,
    requests: Arc<Semaphore>,
    workers: Arc<Semaphore>,
    metrics: Arc<Metrics>,
    admin: Arc<admin::AdminState>,
    /// ETag identity: the v4 lineage (stable across restarts of the same
    /// file family) or a random per-service nonce for v1–v3 files.
    etag_ident: Arc<str>,
    format_version: u32,
    lineage: Option<u64>,
    ops: Arc<ops::Ops>,
    imports: Arc<Mutex<collections::Reservations>>,
    /// Authentication state of the most recently configured REST router on
    /// any clone of this service; other transports share it (same tokens,
    /// credential store, audit log and reloads).
    published_admin: Arc<Mutex<Option<Arc<admin::AdminState>>>>,
    tls: Arc<Mutex<Vec<tls::TlsIdentity>>>,
    rate_limits: Arc<Mutex<BTreeMap<String, (f64, Instant)>>>,
    webhooks: Arc<webhooks::Dispatcher>,
    encryption_key: Option<crate::EncryptionKey>,
    started: Instant,
    group_commit: Option<Arc<group_commit::Coordinator>>,
    snapshot_reads: bool,
    /// Fixed for the service lifetime: the owned file never changes.
    #[cfg(feature = "graph")]
    graph_enabled: bool,
}

impl Service {
    /// Registers a file-backed listener for the admin TLS reload endpoint.
    pub fn register_tls(&self, identity: tls::TlsIdentity) -> std::io::Result<()> {
        let mut identities = self
            .tls
            .lock()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        if identities
            .iter()
            .any(|existing| existing.same_config(&identity))
        {
            return Err(std::io::Error::other("TLS identity is already registered"));
        }
        identities.push(identity);
        Ok(())
    }

    /// Panics if operating-system randomness is unavailable; see `try_new`.
    pub fn new(database: Database) -> Self {
        Self::try_new(database).expect("operating-system randomness for ETag nonce")
    }

    /// Fails explicitly, never falling back, when OS randomness is unavailable.
    pub fn try_new(database: Database) -> std::io::Result<Self> {
        Self::try_new_with_key(database, None)
    }

    pub fn try_new_with_encryption_key(
        database: Database,
        key: crate::EncryptionKey,
    ) -> std::io::Result<Self> {
        Self::try_new_with_key(database, Some(key))
    }

    fn try_new_with_key(
        database: Database,
        encryption_key: Option<crate::EncryptionKey>,
    ) -> std::io::Result<Self> {
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).map_err(|error| {
            std::io::Error::other(format!("operating-system randomness unavailable: {error}"))
        })?;
        let format_version = database.format_version();
        let lineage = database.lineage();
        #[cfg(feature = "graph")]
        let graph_enabled = database.is_graph_enabled();
        let etag_ident: Arc<str> = match lineage {
            Some(lineage) => format!("{lineage:016x}").into(),
            None => hex(&nonce).into(),
        };
        let database = Arc::new(RwLock::new(Some(database)));
        let workers = Arc::new(Semaphore::new(MAX_IN_FLIGHT));
        let metrics = Arc::new(Metrics::default());
        metrics
            .slow_threshold_ns
            .store(500_000_000, Ordering::Relaxed);
        Ok(Self {
            ops: Arc::new(ops::Ops::new(
                Arc::downgrade(&database),
                workers.clone(),
                metrics.clone(),
            )),
            database,
            requests: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
            workers,
            metrics,
            admin: Arc::new(admin::AdminState::default()),
            etag_ident,
            format_version,
            lineage,
            imports: Arc::new(Mutex::new(collections::Reservations::default())),
            published_admin: Arc::default(),
            tls: Arc::default(),
            rate_limits: Arc::default(),
            webhooks: Arc::default(),
            encryption_key,
            started: Instant::now(),
            group_commit: None,
            snapshot_reads: true,
            #[cfg(feature = "graph")]
            graph_enabled,
        })
    }

    /// Choose snapshot-based simple exact reads, or the original lock-held path.
    /// Configure before cloning the service so all listeners share one policy.
    pub fn with_snapshot_reads(mut self, enabled: bool) -> std::io::Result<Self> {
        if Arc::strong_count(&self.database) != 1 {
            return Err(std::io::Error::other(
                "configure snapshot reads before cloning the service",
            ));
        }
        self.snapshot_reads = enabled;
        Ok(self)
    }

    /// Enable bounded durable coalescing before cloning or publishing the service.
    /// Record puts, deletes and batches retain individual validation and frames.
    pub fn with_group_commit(mut self, config: GroupCommitConfig) -> std::io::Result<Self> {
        if self.group_commit.is_some() || Arc::strong_count(&self.database) != 1 {
            return Err(std::io::Error::other(
                "configure group commit once, before cloning the service",
            ));
        }
        self.group_commit = Some(Arc::new(group_commit::Coordinator::start(
            Arc::downgrade(&self.database),
            self.metrics.clone(),
            config,
        )?));
        Ok(self)
    }

    async fn run_mutation<T: Send + 'static>(
        &self,
        bytes: usize,
        operation: impl FnOnce(&mut Database) -> Result<T, ApiError> + Send + 'static,
    ) -> Result<T, ApiError> {
        let Some(group) = &self.group_commit else {
            return self.run(operation).await;
        };
        let permit = self.workers.clone().try_acquire_owned().map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "busy",
                "database queue is full",
            )
        })?;
        group.submit(bytes, permit, operation).await
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
            let mut owner = database.write().map_err(internal)?;
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

    /// Shared-lock variant of `run` for operations that only read. Readers
    /// run concurrently on the blocking pool; a writer in `run` waits for
    /// in-flight readers and blocks new ones until its commit returns, so a
    /// reader never observes a partially applied batch.
    async fn run_read<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&Database) -> Result<T, ApiError> + Send + 'static,
    ) -> Result<T, ApiError> {
        self.run_read_then(operation, Ok).await
    }

    /// Capture bounded immutable state under the owner lock, then perform work
    /// outside it while retaining the same blocking-worker admission permit.
    async fn run_read_then<P: Send + 'static, T: Send + 'static>(
        &self,
        prepare: impl FnOnce(&Database) -> Result<P, ApiError> + Send + 'static,
        complete: impl FnOnce(P) -> Result<T, ApiError> + Send + 'static,
    ) -> Result<T, ApiError> {
        let permit = self.workers.clone().try_acquire_owned().map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "busy",
                "database queue is full",
            )
        })?;
        let database = self.database.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let prepared = {
                let owner = database.read().map_err(internal)?;
                prepare(owner.as_ref().ok_or_else(unavailable)?)?
            };
            complete(prepared)
        })
        .await
        .map_err(internal)?
    }

    /// LEG-019/LEG-020: change-feed consumers (REST `/v1/changes`, gRPC
    /// `Changes`/`WatchChanges`, webhooks) reject graph-enabled files
    /// explicitly (REQ-053) before parsing input or opening a stream.
    #[cfg(feature = "graph")]
    fn reject_graph_feed(&self) -> Result<(), ApiError> {
        if self.graph_enabled {
            return Err(Error::GraphChangeFeedUnsupported.into());
        }
        Ok(())
    }

    /// Call after HTTP graceful shutdown. Stops and joins the maintenance and
    /// backup scheduler, waits for any disconnected writer, then closes.
    pub async fn close(&self) -> crate::Result<()> {
        let database = self.database.clone();
        let ops = self.ops.clone();
        let webhooks = self.webhooks.clone();
        let group = self.group_commit.clone();
        tokio::task::spawn_blocking(move || {
            if let Some(group) = group {
                group.shutdown()?;
            }
            webhooks.shutdown();
            ops.shutdown();
            let mut owner = database
                .write()
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            if let Some(db) = owner.take() {
                db.close()?;
            }
            Ok(())
        })
        .await
        .map_err(|error| std::io::Error::other(error.to_string()))?
    }

    /// Call after every listener has drained and before `close`. Stops and
    /// joins the scheduler (so no maintenance or backup races the save),
    /// waits for any disconnected writer, then saves the active ANN indexes.
    /// It never writes the datafile; `close` remains required.
    pub async fn save_ann_snapshot(
        &self,
        path: impl Into<std::path::PathBuf>,
    ) -> crate::Result<crate::AnnSnapshotReport> {
        let path = path.into();
        let database = self.database.clone();
        let ops = self.ops.clone();
        let webhooks = self.webhooks.clone();
        let group = self.group_commit.clone();
        tokio::task::spawn_blocking(move || {
            if let Some(group) = group {
                group.shutdown()?;
            }
            webhooks.shutdown();
            ops.shutdown();
            let owner = database
                .read()
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            owner
                .as_ref()
                .ok_or_else(|| std::io::Error::other("database is closed"))?
                .save_ann_snapshot(&path)
        })
        .await
        .map_err(|error| std::io::Error::other(error.to_string()))?
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
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

fn configured_router(mut service: Service, token: Option<BearerToken>) -> Router {
    service.admin = Arc::new(admin::AdminState::default());
    service
        .admin
        .auth
        .lock()
        .expect("new authentication state")
        .admin = token;
    configured_routes(service)
}

fn data_routes() -> Router<Service> {
    Router::new()
        .route("/v1/database", get(info))
        .route("/v1/vectors", get(list_vectors))
        .route(
            "/v1/vectors/{id}",
            get(fetch).put(put_vector).delete(delete),
        )
        .route("/v1/batches", post(batch))
        .route("/v1/search", post(search))
        .route(
            "/v1/vectors/{id}/metadata",
            axum::routing::patch(update_metadata),
        )
        .route("/v1/index/hnsw", get(index_status).post(build_index))
        .route("/v1/index/hnsw/quality", post(quality))
        .route(
            "/v1/collections/{collection}/vectors",
            get(collection_list_vectors),
        )
        .route(
            "/v1/collections/{collection}/vectors/{id}",
            get(collection_fetch)
                .put(collection_put)
                .delete(collection_delete),
        )
        .route(
            "/v1/collections/{collection}/vectors/{id}/metadata",
            axum::routing::patch(collection_update_metadata),
        )
        .route(
            "/v1/collections/{collection}/batches",
            post(collection_batch),
        )
        .route(
            "/v1/collections/{collection}/search",
            post(collection_search),
        )
        .route(
            "/v1/collections/{collection}/index/hnsw",
            get(collection_index_status).post(collection_build_index),
        )
        .route(
            "/v1/collections/{collection}/index/hnsw/quality",
            post(collection_quality),
        )
}

fn configured_routes(service: Service) -> Router {
    if let Ok(mut published) = service.published_admin.lock() {
        *published = Some(service.admin.clone());
    }
    let observation = service.clone();
    admin::routes()
        .merge(tls::routes())
        .merge(tenants::routes())
        .merge(webhooks::routes())
        .merge(collections::routes())
        .merge(ops::routes())
        .merge(credentials::routes())
        .merge(data_routes())
        .merge(features::routes())
        .merge(
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
                    "/dashboard/navigation.js",
                    get(|| async {
                        dashboard_asset(
                            "text/javascript; charset=utf-8",
                            include_str!("dashboard/navigation.js"),
                        )
                    }),
                )
                .route(
                    "/dashboard/observability.js",
                    get(|| async {
                        dashboard_asset(
                            "text/javascript; charset=utf-8",
                            include_str!("dashboard/observability.js"),
                        )
                    }),
                )
                .route(
                    "/dashboard/console-tools.js",
                    get(|| async {
                        dashboard_asset(
                            "text/javascript; charset=utf-8",
                            include_str!("dashboard/console-tools.js"),
                        )
                    }),
                )
                .route(
                    "/dashboard/admin-tools.js",
                    get(|| async {
                        dashboard_asset(
                            "text/javascript; charset=utf-8",
                            include_str!("dashboard/admin-tools.js"),
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
                .route("/metrics", get(observability::metrics_route))
                .route("/v1/admin/logs", get(observability::logs))
                .route("/v1/admin/metrics/summary", get(observability::summary))
                .route(
                    "/v1/admin/diagnostics/slow-queries",
                    get(observability::slow_queries),
                )
                .route(
                    "/openapi.json",
                    get(|| async {
                        (
                            [(header::CONTENT_TYPE, "application/json")],
                            include_str!("openapi.json"),
                        )
                    }),
                )
                .fallback(|| async {
                    ApiError::new(StatusCode::NOT_FOUND, "not_found", "route not found")
                })
                .method_not_allowed_fallback(|| async {
                    ApiError::new(
                        StatusCode::METHOD_NOT_ALLOWED,
                        "method_not_allowed",
                        "method not allowed",
                    )
                }),
        )
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

/// Request ID made available to handlers (slow-query log).
#[derive(Clone)]
struct RequestId(String);

/// Error code attached to error responses so observation can count denials.
#[derive(Clone, Copy)]
struct ErrorCode(&'static str);

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let first = values.next();
    if values.next().is_some() {
        return None;
    }
    first
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
        .map(|(_, value)| value)
}

fn authenticate(service: &Service, headers: &HeaderMap) -> Result<Option<admin::Role>, ApiError> {
    let value = bearer(headers);
    {
        let auth = service.admin.auth.lock().map_err(internal)?;
        if auth.admin.is_none()
            || value.is_some_and(|v| auth.admin.as_ref().is_some_and(|t| t.accepts(v)))
        {
            return Ok(Some(admin::Role::Admin));
        }
        if value.is_some_and(|v| auth.read_only.as_ref().is_some_and(|t| t.accepts(v))) {
            return Ok(Some(admin::Role::ReadOnly));
        }
    }
    match (value, &service.admin.credentials) {
        (Some(value), Some(store)) if value.starts_with(credentials::TOKEN_PREFIX) => {
            let grant = store.authenticate(value, admin::now_ms() as u64)?;
            if let Some(id) = &grant.tenant_id {
                service
                    .admin
                    .tenants
                    .as_ref()
                    .ok_or_else(forbidden_scope)?
                    .get(id)?;
            }
            Ok(Some(admin::Role::Scoped(grant)))
        }
        _ => Ok(None),
    }
}

/// Where a scoped credential's collection binding is enforced.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// The handler checks the collection ID and lineage under the database lock.
    Collection,
    /// Requires a `*` grant (database-wide data or operations).
    All,
    /// The handler filters its output to granted collections.
    Filtered,
}

enum Need {
    Any,
    Admin,
    Op(credentials::Op, Reach),
}

/// Scoped-credential policy keyed on the matched route template. Anything not
/// listed is admin-only (deny-unmapped).
fn permission(method: &Method, route: &str) -> Need {
    use credentials::Op;
    let index = route.starts_with("/v1/index/hnsw")
        || route.starts_with("/v1/collections/{collection}/index/hnsw");
    match (method.as_str(), route) {
        ("GET" | "HEAD", "/v1/capabilities" | "/ready" | "/openapi.json") => Need::Any,
        ("GET" | "HEAD", "/metrics" | "/v1/admin/diagnostics/slow-queries") => {
            Need::Op(Op::Diagnostics, Reach::Filtered)
        }
        ("GET" | "HEAD", "/v1/collections") => Need::Op(Op::Read, Reach::Filtered),
        ("GET" | "HEAD", "/v1/changes" | "/v1/admin/replication/export") => {
            Need::Op(Op::Read, Reach::All)
        }
        ("POST", "/v1/collections") => Need::Op(Op::Collections, Reach::All),
        ("DELETE", "/v1/collections/{collection}")
        | ("PUT", "/v1/collections/{collection}/settings") => {
            Need::Op(Op::Collections, Reach::Collection)
        }
        (
            "GET" | "HEAD",
            "/v1/database"
            | "/v1/vectors"
            | "/v1/vectors/{id}"
            | "/v1/collections/{collection}"
            | "/v1/collections/{collection}/vectors"
            | "/v1/collections/{collection}/vectors/{id}"
            | "/v1/collections/{collection}/search/grouped"
            | "/v1/collections/{collection}/recommend"
            | "/v1/collections/{collection}/facets"
            | "/v1/collections/{collection}/stats/numeric"
            | "/v1/collections/{collection}/search/geo"
            | "/v1/collections/{collection}/search/named"
            | "/v1/collections/{collection}/search/sparse"
            | "/v1/collections/{collection}/search/dense-sparse"
            | "/v1/collections/{collection}/search/disk-ann"
            | "/v1/collections/{collection}/query"
            | "/v1/imports/{job}",
        ) => Need::Op(Op::Read, Reach::Collection),
        ("PUT" | "DELETE", "/v1/vectors/{id}" | "/v1/collections/{collection}/vectors/{id}")
        | (
            "PATCH",
            "/v1/vectors/{id}/metadata" | "/v1/collections/{collection}/vectors/{id}/metadata",
        )
        | ("POST", "/v1/batches" | "/v1/collections/{collection}/batches") => {
            Need::Op(Op::Write, Reach::Collection)
        }
        (
            "POST",
            "/v1/search"
            | "/v1/collections/{collection}/search"
            | "/v1/collections/{collection}/search/grouped"
            | "/v1/collections/{collection}/recommend"
            | "/v1/collections/{collection}/facets"
            | "/v1/collections/{collection}/stats/numeric"
            | "/v1/collections/{collection}/search/geo"
            | "/v1/collections/{collection}/search/named"
            | "/v1/collections/{collection}/search/sparse"
            | "/v1/collections/{collection}/search/dense-sparse"
            | "/v1/collections/{collection}/search/disk-ann"
            | "/v1/collections/{collection}/query"
            | "/v1/collections/{collection}/embed",
        ) => Need::Op(Op::Search, Reach::Collection),
        ("POST", "/v1/collections/{collection}/vectors/configure") => {
            Need::Op(Op::Collections, Reach::Collection)
        }
        ("POST", "/v1/collections/{collection}/index/disk-ann") => {
            Need::Op(Op::Index, Reach::Collection)
        }
        ("PUT", "/v1/collections/{collection}/vectors/{id}/named") => {
            Need::Op(Op::Write, Reach::Collection)
        }
        (
            "POST",
            "/v1/collections/{collection}/imports"
            | "/v1/imports/{job}/finish"
            | "/v1/imports/{job}/abort",
        )
        | ("PUT", "/v1/imports/{job}/chunks/{chunk}") => Need::Op(Op::Import, Reach::Collection),
        ("GET" | "HEAD", _) if index => Need::Op(Op::Read, Reach::Collection),
        ("POST", _) if index => Need::Op(Op::Index, Reach::Collection),
        ("GET" | "HEAD", "/v1/admin/backups" | "/v1/admin/backups/schedule")
        | (
            "POST",
            "/v1/admin/backups"
            | "/v1/admin/backups/{name}/verify"
            | "/v1/admin/backups/{name}/drill",
        ) => Need::Op(Op::Backup, Reach::All),
        ("GET" | "HEAD", "/v1/admin/maintenance/auto") => Need::Op(Op::Index, Reach::All),
        _ => Need::Admin,
    }
}

fn forbidden_scope() -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "forbidden_scope",
        "credential scope does not permit this operation or collection",
    )
}

/// REST adapter for `authorize_need`: derives the scope requirement, the
/// read-only rule and the legacy `default` binding from the matched route.
fn authorize(
    service: &Service,
    role: &admin::Role,
    method: &Method,
    route: &str,
    path: &str,
) -> Result<(), ApiError> {
    let read = method == Method::GET || method == Method::HEAD;
    let search = method == Method::POST
        && (path == "/v1/search"
            || route == "/v1/collections/{collection}/search"
            || route.starts_with("/v1/collections/{collection}/search/")
            || route == "/v1/collections/{collection}/recommend"
            || route == "/v1/collections/{collection}/facets"
            || route == "/v1/collections/{collection}/stats/numeric"
            || route == "/v1/collections/{collection}/query"
            || route == "/v1/collections/{collection}/embed"
            || route == "/v1/collections/{collection}/search/disk-ann");
    let read_only_allowed = !path.starts_with("/v1/admin/") && (read || search);
    // Legacy routes address `default` (ID 0); their handlers may not resolve
    // a collection, so the binding is checked here as well.
    let legacy = !route.contains("{collection}") && !route.starts_with("/v1/imports");
    authorize_need(
        service,
        role,
        permission(method, route),
        read_only_allowed,
        legacy,
    )
}

/// Transport-neutral authorization policy shared by REST and gRPC.
/// `legacy` marks requests that address `default` without naming it.
fn authorize_need(
    service: &Service,
    role: &admin::Role,
    need: Need,
    read_only_allowed: bool,
    legacy: bool,
) -> Result<(), ApiError> {
    match role {
        admin::Role::Admin => Ok(()),
        admin::Role::ReadOnly if read_only_allowed => Ok(()),
        admin::Role::ReadOnly => Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "read_only",
            "admin access required",
        )),
        admin::Role::Scoped(grant) => {
            if service.lineage != Some(grant.lineage) {
                return Err(forbidden_scope());
            }
            match need {
                Need::Any => Ok(()),
                Need::Admin => Err(forbidden_scope()),
                Need::Op(op, reach) => {
                    let legacy = legacy && reach == Reach::Collection;
                    if !grant.allows(op)
                        || (reach == Reach::All
                            && (!grant.all
                                || (grant.tenant_id.is_some()
                                    && op != credentials::Op::Collections)))
                        || (legacy && !grant.covers(service.lineage, 0, "default"))
                    {
                        Err(forbidden_scope())
                    } else {
                        Ok(())
                    }
                }
            }
        }
    }
}

async fn observe(
    State(service): State<Service>,
    mut request: axum::extract::Request,
    next: Next,
) -> Response {
    let started = Instant::now();
    service.metrics.in_flight.fetch_add(1, Ordering::Relaxed);
    let request_path = request.uri().path().to_owned();
    let request_id = next_request_id(&service);
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or("unmatched", |path| path.as_str())
        .to_owned();
    #[cfg(feature = "otel")]
    let trace = telemetry::request(
        &request_id,
        &route,
        request
            .uri()
            .path()
            .strip_prefix("/v1/collections/")
            .and_then(|p| p.split('/').next())
            .unwrap_or_else(|| {
                if route.starts_with("/v1/vectors")
                    || route.starts_with("/v1/index/")
                    || matches!(
                        route.as_str(),
                        "/v1/search" | "/v1/batches" | "/v1/database"
                    )
                {
                    "default"
                } else {
                    ""
                }
            }),
    );
    let method = match request.method().as_str() {
        "GET" => "GET",
        "POST" => "POST",
        "PUT" => "PUT",
        "DELETE" => "DELETE",
        _ => "other",
    };
    let public = matches!(
        request.uri().path(),
        "/health"
            | "/dashboard"
            | "/dashboard/app.js"
            | "/dashboard/style.css"
            | "/dashboard/console-tools.js"
            | "/dashboard/admin-tools.js"
            | "/dashboard/observability.js"
            | "/dashboard/navigation.js"
    );
    let authentication = authenticate(&service, request.headers());
    let role = authentication.as_ref().ok().cloned().flatten();
    request
        .extensions_mut()
        .insert(RequestId(request_id.clone()));
    let mut response = if public {
        if let Some(role) = role.clone() {
            request.extensions_mut().insert(role);
        }
        next.run(request).await
    } else {
        match &authentication {
            Err(error) => error.clone().into_response(),
            Ok(None) => ApiError::new(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "valid bearer token required",
            )
            .into_response(),
            Ok(Some(role)) => {
                match rate_limit(&service, role).and_then(|()| {
                    authorize(
                        &service,
                        role,
                        request.method(),
                        &route,
                        request.uri().path(),
                    )
                }) {
                    Err(error) => error.into_response(),
                    Ok(()) => {
                        request.extensions_mut().insert(role.clone());
                        next.run(request).await
                    }
                }
            }
        }
    };
    if response.status() == StatusCode::UNAUTHORIZED {
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            axum::http::HeaderValue::from_static("Bearer"),
        );
    }
    #[cfg(feature = "otel")]
    telemetry::finish(trace, response.status().as_u16());
    let code = response.extensions().get::<ErrorCode>().map(|code| code.0);
    record_request(
        &service,
        &RequestRecord {
            route: &route,
            method,
            status: response.status(),
            code,
            role: role.as_ref(),
            request_id: &request_id,
            started,
        },
    );
    record_http_access(&request_path, method, response.status(), started);
    service.metrics.in_flight.fetch_sub(1, Ordering::Relaxed);
    response.headers_mut().insert(
        "x-request-id",
        request_id.parse().expect("generated ASCII ID"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}

/// Process-unique request ID; also counts the request.
fn next_request_id(service: &Service) -> String {
    let number = service.metrics.requests.fetch_add(1, Ordering::Relaxed) + 1;
    format!("{}-{number}", std::process::id())
}

/// One completed request from either transport.
struct RequestRecord<'a> {
    route: &'a str,
    method: &'static str,
    status: StatusCode,
    code: Option<&'static str>,
    role: Option<&'a admin::Role>,
    request_id: &'a str,
    started: Instant,
}

/// Transport-neutral observation: metrics, scoped-denial audit, the request
/// log line and the in-memory admin request record. Never logs secrets,
/// vectors, payloads or metadata values.
fn record_request(service: &Service, record: &RequestRecord<'_>) {
    let RequestRecord {
        route,
        method,
        status,
        code,
        role,
        request_id,
        started,
    } = *record;
    if code == Some("rate_limited") {
        service.metrics.rate_limited.fetch_add(1, Ordering::Relaxed);
    }
    if code == Some("quota_exceeded") {
        service
            .metrics
            .quota_exceeded
            .fetch_add(1, Ordering::Relaxed);
    }
    if status == StatusCode::UNAUTHORIZED {
        service.metrics.unauthorized.fetch_add(1, Ordering::Relaxed);
    }
    if let (Some(admin::Role::Scoped(grant)), Some("forbidden_scope")) = (role, code) {
        service
            .metrics
            .credential_denials
            .fetch_add(1, Ordering::Relaxed);
        if let Some(log) = &service.admin.audit_log
            && log.should_record_denial(&grant.id, route)
            && log
                .append(
                    json!({"event":"credential_denied","credential_id":grant.id,
                    "method":method,"route":route,"request_id":request_id}),
                    false,
                )
                .is_err()
        {
            service
                .metrics
                .audit_failures
                .fetch_add(1, Ordering::Relaxed);
        }
    }
    let elapsed = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let failed = status.is_client_error() || status.is_server_error();
    if let Ok(mut routes) = service.metrics.by_route.lock() {
        let stats = routes
            .entry((method.to_owned(), route.to_owned()))
            .or_default();
        stats.count = stats.count.saturating_add(1);
        if failed {
            stats.errors = stats.errors.saturating_add(1);
        }
        if stats.durations_ms.len() == 1024 {
            stats.durations_ms.pop_front();
        }
        stats.durations_ms.push_back(elapsed_ms);
    }
    service
        .metrics
        .duration_ns
        .fetch_add(elapsed, Ordering::Relaxed);
    if failed {
        service.metrics.failures.fetch_add(1, Ordering::Relaxed);
    }
    if method == "grpc" {
        service
            .metrics
            .grpc_requests
            .fetch_add(1, Ordering::Relaxed);
        if failed {
            service
                .metrics
                .grpc_failures
                .fetch_add(1, Ordering::Relaxed);
        }
    }
    let credential = match role {
        Some(admin::Role::Scoped(grant)) => Some(grant.id.clone()),
        _ => None,
    };
    let request_line = json!({"event":"request", "request_id":request_id, "method":method,
        "route":route, "status":status.as_u16(), "duration_ns":elapsed,
        "credential_id":credential})
    .to_string();
    logs::stderr("info", "server", request_line);
    service.admin.record(
        json!({"request_id":request_id,"method":method,"route":route,
        "status":status.as_u16(),"role":role.map(admin::Role::name),
        "credential_id":credential,
        "duration_ns":elapsed,"timestamp_unix_ms":admin::now_ms()}),
    );
}

fn record_http_access(path: &str, method: &str, status: StatusCode, started: Instant) {
    let skip = matches!(
        path,
        "/metrics" | "/health" | "/ready" | "/v1/admin/logs" | "/v1/admin/metrics/summary"
    ) || path.starts_with("/dashboard");
    if skip {
        return;
    }
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let collection = path
        .strip_prefix("/v1/collections/")
        .and_then(|tail| tail.split('/').next())
        .filter(|name| !name.is_empty());
    let level = if status.is_server_error() {
        "error"
    } else if status.is_client_error() {
        "warn"
    } else {
        "info"
    };
    logs::buffer().record(
        level,
        "http",
        format!("{method} {path} {}", status.as_u16()),
        json!({
            "method": method,
            "path": path,
            "status": status.as_u16(),
            "duration_ms": duration_ms,
            "collection": collection
        }),
    );
}

/// A failure after a streaming RPC was accepted (and already recorded as a
/// successful call): counts it as a failure and logs it under the same
/// request ID so the stream's outcome stays observable.
#[cfg(feature = "grpc")]
fn record_stream_failure(service: &Service, route: &str, request_id: &str, error: &ApiError) {
    if error.code == "quota_exceeded" {
        service
            .metrics
            .quota_exceeded
            .fetch_add(1, Ordering::Relaxed);
    }
    if error.code == "rate_limited" {
        service.metrics.rate_limited.fetch_add(1, Ordering::Relaxed);
    }
    service.metrics.failures.fetch_add(1, Ordering::Relaxed);
    service
        .metrics
        .grpc_failures
        .fetch_add(1, Ordering::Relaxed);
    logs::stderr(
        "error",
        "grpc",
        json!({"event":"stream_error", "request_id":request_id, "method":"grpc",
            "route":route, "status":error.status.as_u16(), "code":error.code})
        .to_string(),
    );
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

#[derive(Debug, Clone)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    /// Structured error body for transports that cannot carry the REST JSON
    /// body (gRPC trailers).
    #[cfg_attr(not(feature = "grpc"), allow(dead_code))]
    details: Option<String>,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            details: None,
        }
    }

    #[cfg(feature = "grpc")]
    fn with_details(mut self, details: String) -> Self {
        self.details = Some(details);
        self
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            Json(json!({"error": {"code": self.code, "message": self.message}})),
        )
            .into_response();
        if self.status == StatusCode::TOO_MANY_REQUESTS {
            response.headers_mut().insert(
                header::RETRY_AFTER,
                axum::http::HeaderValue::from_static("1"),
            );
        }
        response.extensions_mut().insert(ErrorCode(self.code));
        response
    }
}

fn internal(_error: impl std::fmt::Display) -> ApiError {
    logs::stderr(
        "error",
        "server",
        json!({"event":"internal_error"}).to_string(),
    );
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

fn unprocessable(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_input", message)
}

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        match error {
            Error::HnswRebuildBusy => Self::new(
                StatusCode::CONFLICT,
                "rebuild_busy",
                "a rebuild is already pending",
            ),
            Error::IndexNotReady => Self::new(
                StatusCode::CONFLICT,
                "index_not_ready",
                "HNSW index is not ready; POST /v1/index/hnsw during maintenance, or explicitly choose exact/lazy approximate search",
            ),
            Error::DimensionMismatch { .. }
            | Error::InvalidHnswRebuildLimits
            | Error::InvalidHnswMaintenancePolicy
            | Error::HnswRebuildTooLarge
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
            Error::BatchRequiresUpgrade
            | Error::MetadataRequiresUpgrade
            | Error::ExpiryRequiresUpgrade => {
                Self::new(StatusCode::CONFLICT, "upgrade_required", error.to_string())
            }
            Error::CollectionNotFound => Self::new(
                StatusCode::NOT_FOUND,
                "collection_not_found",
                error.to_string(),
            ),
            Error::ImportNotFound => {
                Self::new(StatusCode::NOT_FOUND, "import_not_found", error.to_string())
            }
            Error::CollectionExists => {
                Self::new(StatusCode::CONFLICT, "collection_exists", error.to_string())
            }
            Error::TooManyCollections => {
                Self::new(StatusCode::CONFLICT, "collection_limit", error.to_string())
            }
            Error::CollectionNotEmpty => Self::new(
                StatusCode::CONFLICT,
                "collection_not_empty",
                error.to_string(),
            ),
            Error::DefaultCollectionImmutable => Self::new(
                StatusCode::CONFLICT,
                "default_collection_immutable",
                error.to_string(),
            ),
            Error::CollectionsRequireUpgrade | Error::ImportsRequireUpgrade => Self::new(
                StatusCode::CONFLICT,
                "requires_format_v4",
                error.to_string(),
            ),
            Error::ImportConflict(_) => {
                Self::new(StatusCode::CONFLICT, "import_conflict", error.to_string())
            }
            Error::TooManyImports => {
                Self::new(StatusCode::CONFLICT, "import_limit", error.to_string())
            }
            Error::RevisionMismatch { .. } => Self::new(
                StatusCode::PRECONDITION_FAILED,
                "precondition_failed",
                error.to_string(),
            ),
            Error::InvalidCollectionName
            | Error::InvalidCollectionSettings(_)
            | Error::InvalidQuery(_) => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_input",
                error.to_string(),
            ),
            Error::CommitIndeterminate(_) => {
                logs::stderr(
                    "error",
                    "server",
                    json!({"event":"commit_indeterminate"}).to_string(),
                );
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
            // LEG-017: every graph variant is explicit, never the 500 below.
            // State conflicts (including references to missing graph
            // entities) are 409, invalid input 422, unsupported surfaces 501.
            #[cfg(feature = "graph")]
            Error::GraphsNotEnabled
            | Error::GraphExists
            | Error::GraphNotFound
            | Error::GraphNotEmpty
            | Error::TooManyGraphs
            | Error::GraphKeyExists
            | Error::GraphKeyConflict
            | Error::GraphNodeNotFound
            | Error::GraphEdgeNotFound
            | Error::GraphNodeHasEdges { .. }
            | Error::GraphAttachmentConflict
            | Error::GraphAttachmentNotFound
            | Error::GraphRecordNotAttachable
            | Error::GraphAttachedRecord
            | Error::GraphAttachedRecordExpiry
            | Error::GraphCursorStale
            | Error::GraphUnsupportedOperation => {
                Self::new(StatusCode::CONFLICT, graph_code(&error), error.to_string())
            }
            #[cfg(feature = "graph")]
            Error::GraphsRequireV5
            | Error::InvalidGraphName
            | Error::GraphLimitExceedsCeiling { .. }
            | Error::GraphInvalidOptions(_) => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                graph_code(&error),
                error.to_string(),
            ),
            #[cfg(feature = "graph")]
            Error::GraphChangeFeedUnsupported => Self::new(
                StatusCode::NOT_IMPLEMENTED,
                graph_code(&error),
                error.to_string(),
            ),
            error => internal(error),
        }
    }
}

/// Stable REST error code (gRPC `vlite-error-code` trailer) of each graph
/// `Error` variant: its snake_case name.
#[cfg(feature = "graph")]
fn graph_code(error: &Error) -> &'static str {
    match error {
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
        _ => "internal_error",
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

/// Parses an optional JSON object body: empty means `T::default()`.
fn optional<T: serde::de::DeserializeOwned + Default>(bytes: &[u8]) -> Result<T, ApiError> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|error| {
        ApiError::new(StatusCode::BAD_REQUEST, "invalid_json", error.to_string())
    })?;
    if !value.is_object() {
        return Err(unprocessable("request body must be a JSON object"));
    }
    serde_json::from_value(value).map_err(|error| unprocessable(error.to_string()))
}

/// Which collection a data route addresses. Legacy routes always address
/// `default` with legacy hard defaults; named routes use collection settings.
#[derive(Clone)]
enum Target {
    Legacy,
    Named(String),
}

impl Target {
    fn retained_bytes(&self) -> usize {
        match self {
            Self::Legacy => 0,
            Self::Named(name) => name.capacity(),
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::Legacy => "default",
            Self::Named(name) => name,
        }
    }

    fn legacy(&self) -> bool {
        matches!(self, Self::Legacy)
    }
}

fn collection_path(input: Result<Path<String>, PathRejection>) -> Result<Target, ApiError> {
    let Path(name) = input.map_err(|error| {
        ApiError::new(StatusCode::BAD_REQUEST, "invalid_path", error.body_text())
    })?;
    Ok(Target::Named(name))
}

fn record_path(
    input: Result<Path<(String, String)>, PathRejection>,
) -> Result<(Target, u64), ApiError> {
    let Path((name, text)) = input.map_err(|error| {
        ApiError::new(StatusCode::BAD_REQUEST, "invalid_path", error.body_text())
    })?;
    Ok((Target::Named(name), id(&text)?))
}

/// Resolves a target under the database lock and enforces scoped bindings:
/// collection ID and database lineage must both match the grant. The name
/// is checked first so out-of-scope callers cannot probe existence.
fn open(db: &Database, role: &admin::Role, target: &Target) -> Result<u16, ApiError> {
    db.ensure_healthy()?;
    let qualified = tenants::qualify(role, target.name());
    let name = qualified.as_str();
    if let admin::Role::Scoped(grant) = role
        && !grant.names(name)
    {
        return Err(forbidden_scope());
    }
    let collection = db.resolve(name)?;
    if let admin::Role::Scoped(grant) = role
        && !grant.covers(db.lineage(), collection, name)
    {
        return Err(forbidden_scope());
    }
    Ok(collection)
}

/// Identity defaults: hard legacy defaults, or the collection's settings.
fn identity(
    db: &Database,
    collection: u16,
    target: &Target,
) -> (DistanceMetric, HnswConfig, AnnOptions) {
    if target.legacy() {
        (
            DistanceMetric::default(),
            HnswConfig::default(),
            AnnOptions::default(),
        )
    } else {
        let settings = &db.state(collection).settings;
        (
            settings.metric,
            settings.hnsw,
            AnnOptions {
                quantization: settings.quantization,
                rerank_multiplier: settings.rerank_multiplier,
            },
        )
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListVectors {
    after: Option<String>,
    limit: Option<usize>,
}

async fn list_vectors(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    query: Result<Query<ListVectors>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    list_in(service, role, Target::Legacy, query).await
}

async fn collection_list_vectors(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    target: Result<Path<String>, PathRejection>,
    query: Result<Query<ListVectors>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    list_in(service, role, collection_path(target)?, query).await
}

async fn list_in(
    service: Service,
    role: admin::Role,
    target: Target,
    query: Result<Query<ListVectors>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(query) = query.map_err(|error| {
        ApiError::new(StatusCode::BAD_REQUEST, "invalid_query", error.body_text())
    })?;
    let after = query.after.as_deref().map(id).transpose()?;
    let limit = query.limit.unwrap_or(25);
    if !(1..=100).contains(&limit) {
        return Err(unprocessable("limit must be 1..100"));
    }
    service
        .run_read(move |db| {
            let collection = open(db, &role, &target)?;
            use std::ops::Bound::{Excluded, Unbounded};
            let mut records = db
                .state(collection)
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

fn payload(text: &str) -> Result<Vec<u8>, ApiError> {
    if text.len() > MAX_PAYLOAD_BYTES.div_ceil(3) * 4 {
        return Err(unprocessable(format!(
            "payload exceeds {MAX_PAYLOAD_BYTES} decoded bytes"
        )));
    }
    let bytes = decode_payload(text)?;
    if bytes.len() > MAX_PAYLOAD_BYTES {
        return Err(Error::PayloadTooLarge(bytes.len()).into());
    }
    Ok(bytes)
}

/// Decodes without the size check so imports can report oversized rows.
fn decode_payload(text: &str) -> Result<Vec<u8>, ApiError> {
    BASE64_STANDARD.decode(text).map_err(|_| {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_payload",
            "payload_base64 must be canonical padded standard base64",
        )
    })
}

async fn ready(State(service): State<Service>) -> Result<Json<Value>, ApiError> {
    service
        .run_read(|db| {
            db.get(0)?;
            Ok(Json(json!({"status": "ready"})))
        })
        .await
}

async fn info(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
) -> Result<Json<Value>, ApiError> {
    service
        .run_read(move |db| {
            open(db, &role, &Target::Legacy)?;
            Ok(Json(json!({
                "dimensions": db.dimensions(),
                "format_version": db.format_version(),
                "records": db.len(),
                "is_encrypted": db.is_encrypted(),
                "encryption_key_id": db.encryption_key_id().map(|key_id| key_id.to_string())
            })))
        })
        .await
}

/// Strong per-record tag: `r2-<ident>-<collection>-<id>-<revision>-<sha256>`.
/// The ident is the v4 lineage (stable across restarts) or a per-process
/// nonce (v1–v3). The SHA-256 covers the full record content, so raw file
/// clones that diverge fail closed. Unrelated writes never invalidate it.
fn etag(ident: &str, collection: u16, id: u64, revision: u64, record: &Record) -> String {
    let digest = Sha256::digest(crate::format::extended_body(
        &record.vector,
        &record.payload,
        &record.metadata,
    ));
    format!(
        "\"r2-{ident}-{collection}-{id}-{revision}-{}\"",
        hex(&digest)
    )
}

fn current_etag(
    db: &Database,
    ident: &str,
    collection: u16,
    id: u64,
) -> Result<Option<(String, u64)>, ApiError> {
    match (db.get_in(collection, id)?, db.revision_in(collection, id)?) {
        (Some(record), Some(revision)) => Ok(Some((
            etag(ident, collection, id, revision, record),
            revision,
        ))),
        _ => Ok(None),
    }
}

/// Accepts absent If-Match or exactly one strong entity tag; nothing else.
fn if_match(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    let mut values = headers.get_all(header::IF_MATCH).iter();
    let Some(first) = values.next() else {
        return Ok(None);
    };
    let invalid = || {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_if_match",
            "If-Match must be exactly one strong entity tag from GET; weak, wildcard and multiple validators are unsupported",
        )
    };
    if values.next().is_some() {
        return Err(invalid());
    }
    let text = first.to_str().map_err(|_| invalid())?;
    let inner = text
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .ok_or_else(invalid)?;
    if !inner
        .bytes()
        .all(|b| b == 0x21 || (0x23..=0x7e).contains(&b))
    {
        return Err(invalid());
    }
    Ok(Some(text.to_owned()))
}

/// Must run under the same owner lock as the write; the returned revision
/// makes the write itself a compare-and-set.
fn precondition(
    db: &Database,
    ident: &str,
    collection: u16,
    id: u64,
    expected: Option<&str>,
) -> Result<Expected, ApiError> {
    let Some(expected) = expected else {
        return Ok(Expected::Any);
    };
    match current_etag(db, ident, collection, id)? {
        Some((tag, revision)) if tag == expected => Ok(Expected::Revision(revision)),
        _ => Err(ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "precondition_failed",
            "record changed, was removed, or the tag is from another database or service instance; GET it again",
        )),
    }
}

async fn fetch(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    input: Result<Path<String>, PathRejection>,
) -> Result<Response, ApiError> {
    fetch_in(service, role, Target::Legacy, path(input)?).await
}

async fn collection_fetch(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    input: Result<Path<(String, String)>, PathRejection>,
) -> Result<Response, ApiError> {
    let (target, id) = record_path(input)?;
    fetch_in(service, role, target, id).await
}

async fn fetch_in(
    service: Service,
    role: admin::Role,
    target: Target,
    id: u64,
) -> Result<Response, ApiError> {
    let fetched = fetch_core(&service, role, target, id).await?;
    let body = Json(json!({
        "id": id.to_string(),
        "vector": fetched.record.vector,
        "payload_base64": BASE64_STANDARD.encode(&fetched.record.payload),
        "metadata": fetched.record.metadata,
        "revision": fetched.revision.to_string(),
    }));
    let tag = etag(
        &service.etag_ident,
        fetched.collection,
        id,
        fetched.revision,
        &fetched.record,
    );
    Ok(([(header::ETAG, tag)], body).into_response())
}

/// One record with its revision and resolved collection ID.
struct Fetched {
    record: Record,
    revision: u64,
    collection: u16,
}

/// Transport-neutral record read with scope enforcement.
async fn fetch_core(
    service: &Service,
    role: admin::Role,
    target: Target,
    id: u64,
) -> Result<Fetched, ApiError> {
    service
        .run_read(move |db| {
            let collection = open(db, &role, &target)?;
            let not_found =
                || ApiError::new(StatusCode::NOT_FOUND, "not_found", "vector not found");
            let record = db.get_in(collection, id)?.ok_or_else(not_found)?.clone();
            let revision = db.revision_in(collection, id)?.ok_or_else(not_found)?;
            Ok(Fetched {
                record,
                revision,
                collection,
            })
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
    expires_at_ms: Option<u64>,
    #[serde(alias = "ttl")]
    ttl_ms: Option<u64>,
}

fn expiry(input: &Put) -> Result<Option<u64>, ApiError> {
    if input.expires_at_ms.is_some() && input.ttl_ms.is_some() {
        return Err(unprocessable(
            "expires_at_ms and ttl_ms are mutually exclusive",
        ));
    }
    if let Some(ttl) = input.ttl_ms {
        let now = u64::try_from(admin::now_ms())
            .map_err(|_| unprocessable("server clock exceeds u64 milliseconds"))?;
        return now
            .checked_add(ttl)
            .map(Some)
            .ok_or_else(|| unprocessable("ttl_ms overflows Unix milliseconds"));
    }
    Ok(input.expires_at_ms)
}

async fn put_vector(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    path_input: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    input: Result<Json<Put>, JsonRejection>,
) -> Result<Response, ApiError> {
    put_in(
        service,
        role,
        Target::Legacy,
        path(path_input)?,
        headers,
        input,
    )
    .await
}

async fn collection_put(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    path_input: Result<Path<(String, String)>, PathRejection>,
    headers: HeaderMap,
    input: Result<Json<Put>, JsonRejection>,
) -> Result<Response, ApiError> {
    let (target, id) = record_path(path_input)?;
    put_in(service, role, target, id, headers, input).await
}

async fn put_in(
    service: Service,
    role: admin::Role,
    target: Target,
    id: u64,
    headers: HeaderMap,
    input: Result<Json<Put>, JsonRejection>,
) -> Result<Response, ApiError> {
    let expected = Guard::ETag(if_match(&headers)?);
    let input = body(input)?;
    let payload = payload(&input.payload_base64)?;
    let expires_at_ms = expiry(&input)?;
    let written = put_core(
        &service,
        role,
        target,
        id,
        expected,
        input.vector,
        payload,
        input.metadata,
        expires_at_ms,
    )
    .await?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert(header::ETAG, written.etag.parse().map_err(internal)?);
    Ok(response)
}

async fn update_metadata(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    input: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    payload: Result<Json<MetadataPatch>, JsonRejection>,
) -> Result<Response, ApiError> {
    update_metadata_in(
        service,
        role,
        Target::Legacy,
        path(input)?,
        headers,
        payload,
    )
    .await
}

async fn collection_update_metadata(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    input: Result<Path<(String, String)>, PathRejection>,
    headers: HeaderMap,
    payload: Result<Json<MetadataPatch>, JsonRejection>,
) -> Result<Response, ApiError> {
    let (target, id) = record_path(input)?;
    update_metadata_in(service, role, target, id, headers, payload).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MetadataPatch {
    metadata: crate::Metadata,
}

async fn update_metadata_in(
    service: Service,
    role: admin::Role,
    target: Target,
    id: u64,
    headers: HeaderMap,
    payload: Result<Json<MetadataPatch>, JsonRejection>,
) -> Result<Response, ApiError> {
    let expected = if_match(&headers)?;
    let MetadataPatch { metadata } = body(payload)?;
    let ident = service.etag_ident.clone();
    let bytes = group_commit::record_bytes(0, 0, &metadata)
        .saturating_add(target.retained_bytes())
        .saturating_add(expected.as_ref().map_or(0, String::capacity));
    service
        .run_mutation(bytes, move |db| {
            let collection = open(db, &role, &target)?;
            if let Some(expected) = expected.as_deref() {
                let Some((current, _)) = current_etag(db, &ident, collection, id)? else {
                    return Err(ApiError::new(
                        StatusCode::PRECONDITION_FAILED,
                        "precondition_failed",
                        "record changed or was removed",
                    ));
                };
                if current != expected {
                    return Err(ApiError::new(
                        StatusCode::PRECONDITION_FAILED,
                        "precondition_failed",
                        "record changed; fetch a fresh ETag",
                    ));
                }
            }
            let revision = db
                .collection_mut(target.name())?
                .update_metadata(id, &metadata)?;
            let (tag, _) = current_etag(db, &ident, collection, id)?
                .ok_or_else(|| internal("metadata update record missing"))?;
            Ok((revision, tag))
        })
        .await
        .map(|(_, tag)| {
            let mut response = StatusCode::NO_CONTENT.into_response();
            if let Ok(value) = tag.parse() {
                response.headers_mut().insert(header::ETAG, value);
            }
            response
        })
}

/// Write precondition in transport form, resolved under the owner lock so
/// the write itself is a compare-and-set.
enum Guard {
    /// REST `If-Match`: absent, or one strong tag from this service.
    ETag(Option<String>),
    /// Engine precondition on the addressed record's revision (gRPC).
    #[cfg_attr(not(feature = "grpc"), allow(dead_code))]
    Expected(Expected),
}

impl Guard {
    fn retained_bytes(&self) -> usize {
        match self {
            Self::ETag(Some(tag)) => tag.capacity(),
            _ => 0,
        }
    }

    fn resolve(
        self,
        db: &Database,
        ident: &str,
        collection: u16,
        id: u64,
    ) -> Result<Expected, ApiError> {
        match self {
            Self::ETag(tag) => precondition(db, ident, collection, id, tag.as_deref()),
            Self::Expected(expected) => Ok(expected),
        }
    }
}

struct Written {
    #[cfg_attr(not(feature = "grpc"), allow(dead_code))]
    revision: u64,
    #[cfg_attr(not(feature = "grpc"), allow(dead_code))]
    created: bool,
    etag: String,
}

/// Transport-neutral conditional put with scope enforcement.
#[allow(clippy::too_many_arguments)]
async fn put_core(
    service: &Service,
    role: admin::Role,
    target: Target,
    id: u64,
    guard: Guard,
    vector: Vec<f32>,
    payload: Vec<u8>,
    metadata: crate::Metadata,
    expires_at_ms: Option<u64>,
) -> Result<Written, ApiError> {
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(Error::PayloadTooLarge(payload.len()).into());
    }
    let ident = service.etag_ident.clone();
    let quota_service = service.clone();
    let bytes = group_commit::record_bytes(vector.capacity(), payload.capacity(), &metadata)
        .saturating_add(target.retained_bytes())
        .saturating_add(guard.retained_bytes());
    service
        .run_mutation(bytes, move |db| {
            let collection = open(db, &role, &target)?;
            enforce_quota(&quota_service, db, &role, collection, &[(id, true)])?;
            let expected = guard.resolve(db, &ident, collection, id)?;
            let created = db.revision_in(collection, id)?.is_none();
            if let Some(expires_at_ms) = expires_at_ms {
                crate::CollectionMut { db, id: collection }.put_with_expiry(
                    id,
                    &vector,
                    &payload,
                    &metadata,
                    Some(expires_at_ms),
                )?;
            } else {
                db.put_in(collection, id, &vector, &payload, &metadata, expected)?;
            }
            let (etag, revision) = current_etag(db, &ident, collection, id)?
                .ok_or_else(|| internal("acknowledged put is not readable under the same lock"))?;
            Ok(Written {
                revision,
                created,
                etag,
            })
        })
        .await
}

async fn delete(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    input: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    delete_in(service, role, Target::Legacy, path(input)?, headers).await
}

async fn collection_delete(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    input: Result<Path<(String, String)>, PathRejection>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let (target, id) = record_path(input)?;
    delete_in(service, role, target, id, headers).await
}

async fn delete_in(
    service: Service,
    role: admin::Role,
    target: Target,
    id: u64,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let expected = Guard::ETag(if_match(&headers)?);
    let deleted = delete_core(&service, role, target, id, expected).await?;
    Ok(Json(json!({"deleted": deleted})))
}

/// Transport-neutral conditional delete with scope enforcement.
async fn delete_core(
    service: &Service,
    role: admin::Role,
    target: Target,
    id: u64,
    guard: Guard,
) -> Result<bool, ApiError> {
    let ident = service.etag_ident.clone();
    let bytes = 1024usize
        .saturating_add(target.retained_bytes())
        .saturating_add(guard.retained_bytes());
    service
        .run_mutation(bytes, move |db| {
            let collection = open(db, &role, &target)?;
            let expected = guard.resolve(db, &ident, collection, id)?;
            Ok(db.delete_in(collection, id, expected)?)
        })
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

/// Converts wire operations. `checked` applies the payload size limit up
/// front (batches); imports leave it to per-row validation.
fn mutations(operations: Vec<Operation>, checked: bool) -> Result<Vec<Mutation>, ApiError> {
    if operations.len() > MAX_BATCH_OPERATIONS {
        return Err(Error::BatchTooLarge.into());
    }
    operations
        .into_iter()
        .map(|operation| match operation {
            Operation::Put {
                id: text,
                vector,
                payload_base64,
                metadata,
            } => {
                let id = id(&text)?;
                let payload = if checked {
                    payload(&payload_base64)?
                } else {
                    decode_payload(&payload_base64)?
                };
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
        .collect()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Batch {
    operations: Vec<Operation>,
}

async fn batch(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    input: Result<Json<Batch>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    batch_in(service, role, Target::Legacy, input).await
}

async fn collection_batch(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    target: Result<Path<String>, PathRejection>,
    input: Result<Json<Batch>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    batch_in(service, role, collection_path(target)?, input).await
}

async fn batch_in(
    service: Service,
    role: admin::Role,
    target: Target,
    input: Result<Json<Batch>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let operations = mutations(body(input)?.operations, true)?;
    batch_core(&service, role, target, operations).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Transport-neutral atomic batch with scope enforcement: every operation
/// commits in one frame, or none does.
async fn batch_core(
    service: &Service,
    role: admin::Role,
    target: Target,
    operations: Vec<Mutation>,
) -> Result<(), ApiError> {
    if operations.len() > MAX_BATCH_OPERATIONS {
        return Err(Error::BatchTooLarge.into());
    }
    for operation in &operations {
        if let Mutation::Put { payload, .. } | Mutation::PutWithMetadata { payload, .. } = operation
            && payload.len() > MAX_PAYLOAD_BYTES
        {
            return Err(Error::PayloadTooLarge(payload.len()).into());
        }
    }
    let quota_service = service.clone();
    let bytes = operations
        .iter()
        .fold(
            operations
                .capacity()
                .saturating_mul(std::mem::size_of::<Mutation>())
                .saturating_add(1024),
            |bytes, operation| {
                bytes.saturating_add(match operation {
                    Mutation::Put {
                        vector, payload, ..
                    } => vector
                        .capacity()
                        .saturating_mul(4)
                        .saturating_add(payload.capacity()),
                    Mutation::PutWithMetadata {
                        vector,
                        payload,
                        metadata,
                        ..
                    } => {
                        group_commit::record_bytes(vector.capacity(), payload.capacity(), metadata)
                    }
                    Mutation::Delete { .. } => 0,
                })
            },
        )
        .saturating_add(target.retained_bytes());
    service
        .run_mutation(bytes, move |db| {
            let collection = open(db, &role, &target)?;
            enforce_quota(
                &quota_service,
                db,
                &role,
                collection,
                &quota_mutations(&operations),
            )?;
            db.write_batch_in(collection, &operations)?;
            Ok(())
        })
        .await
}

/// Parsed from a string so `null`/unknown names are 422 data errors, as before.
#[derive(Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
enum SearchMode {
    #[default]
    Exact,
    Approximate,
    ApproximateReady,
    Auto,
    Hybrid,
}

impl TryFrom<String> for SearchMode {
    type Error = &'static str;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Ok(match text.as_str() {
            "exact" => Self::Exact,
            "approximate" => Self::Approximate,
            "approximate_ready" => Self::ApproximateReady,
            "auto" => Self::Auto,
            "hybrid" => Self::Hybrid,
            _ => return Err("mode must be exact, approximate, approximate_ready, auto or hybrid"),
        })
    }
}

impl SearchMode {
    fn name(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Approximate => "approximate",
            Self::ApproximateReady => "approximate_ready",
            Self::Auto => "auto",
            Self::Hybrid => "hybrid",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
enum VectorLegName {
    Exact,
    ApproximateReady,
}

impl TryFrom<String> for VectorLegName {
    type Error = &'static str;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        match text.as_str() {
            "exact" => Ok(Self::Exact),
            "approximate_ready" => Ok(Self::ApproximateReady),
            _ => Err("vector_leg must be exact or approximate_ready"),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fusion {
    candidates: Option<usize>,
    rrf_k: Option<u32>,
    vector_weight: Option<f64>,
    text_weight: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Search {
    vector: Option<Vec<f32>>,
    k: usize,
    #[serde(default)]
    mode: SearchMode,
    metric: Option<DistanceMetric>,
    config: Option<admin::IndexConfig>,
    ef_search: Option<usize>,
    filter: Option<Vec<crate::Condition>>,
    quantization: Option<collections::QuantizationName>,
    rerank_multiplier: Option<u8>,
    rerank: Option<RerankRequest>,
    text: Option<String>,
    fusion: Option<Fusion>,
    vector_leg: Option<VectorLegName>,
    #[serde(default)]
    explain: bool,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RerankMode {
    Exact,
    Mmr,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RerankRequest {
    mode: RerankMode,
    candidates: Option<usize>,
    lambda: Option<f64>,
}

impl Search {
    /// Request-shape rules that do not depend on database state.
    fn validate(&self, role: &admin::Role) -> Result<(), ApiError> {
        let hybrid = self.mode == SearchMode::Hybrid;
        if let Some(rerank) = &self.rerank {
            if hybrid {
                return Err(unprocessable("reranking is only valid for vector search"));
            }
            if rerank.lambda.is_some() && rerank.mode != RerankMode::Mmr {
                return Err(unprocessable("lambda is only valid for MMR reranking"));
            }
            let candidates = rerank.candidates.unwrap_or(self.k.max(50));
            if self.k == 0 || self.k > 1000 || !(self.k..=1000).contains(&candidates) {
                return Err(unprocessable(
                    "rerank candidates must be between k and 1000",
                ));
            }
            if rerank
                .lambda
                .is_some_and(|lambda| !lambda.is_finite() || !(0.0..=1.0).contains(&lambda))
            {
                return Err(unprocessable(
                    "MMR lambda must be finite and between 0 and 1",
                ));
            }
        }
        if self.mode == SearchMode::Approximate {
            match role {
                admin::Role::ReadOnly => {
                    return Err(ApiError::new(
                        StatusCode::FORBIDDEN,
                        "read_only",
                        "read-only searches must use exact, auto, hybrid or approximate_ready; lazy builds require admin access",
                    ));
                }
                admin::Role::Scoped(grant) if !grant.allows(credentials::Op::SearchBuild) => {
                    return Err(forbidden_scope());
                }
                _ => {}
            }
        }
        if self.k > MAX_SEARCH_K {
            return Err(unprocessable(format!("k must be at most {MAX_SEARCH_K}")));
        }
        if !hybrid && (self.text.is_some() || self.fusion.is_some() || self.vector_leg.is_some()) {
            return Err(unprocessable(
                "text, fusion and vector_leg are only valid in hybrid mode",
            ));
        }
        if !hybrid && self.vector.is_none() {
            return Err(unprocessable("vector is required"));
        }
        let exact_vector = match self.mode {
            SearchMode::Exact => true,
            SearchMode::Hybrid => self.vector_leg != Some(VectorLegName::ApproximateReady),
            _ => false,
        };
        if exact_vector && self.ef_search.is_some() {
            return Err(unprocessable(
                "ef_search is only valid with an approximate vector strategy",
            ));
        }
        if exact_vector && (self.quantization.is_some() || self.rerank_multiplier.is_some()) {
            return Err(unprocessable(
                "quantization and rerank_multiplier are only valid with an index",
            ));
        }
        Ok(())
    }
}

fn stats_json(stats: &SearchStats) -> Value {
    json!({
        "records_scanned": stats.records_scanned,
        "distance_evaluations": stats.distance_evaluations,
        "quantized_distance_evaluations": stats.quantized_distance_evaluations,
        "graph_nodes_visited": stats.graph_nodes_visited,
        "buffered_records_merged": stats.buffered_records_merged,
        "reranked_candidates": stats.reranked_candidates,
        "index_candidates": stats.index_candidates,
    })
}

fn plan_json(plan: &crate::QueryPlan) -> Value {
    json!({
        "strategy": plan.strategy.name(),
        "estimated_candidates": plan.estimated_candidates,
        "live_records": plan.live_records,
        "index_field": plan.index_field,
        "reason": plan.reason,
    })
}

async fn search(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    Extension(request): Extension<RequestId>,
    input: Result<Json<Search>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    search_in(service, role, request, Target::Legacy, input).await
}

async fn collection_search(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    Extension(request): Extension<RequestId>,
    target: Result<Path<String>, PathRejection>,
    input: Result<Json<Search>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    search_in(service, role, request, collection_path(target)?, input).await
}

/// Ranked results: vector hits (exact, approximate, auto) or fused hybrid hits.
enum Hits {
    Vector(Vec<crate::SearchHit>),
    Hybrid(Vec<Value>),
}

impl Hits {
    fn len(&self) -> usize {
        match self {
            Self::Vector(hits) => hits.len(),
            Self::Hybrid(hits) => hits.len(),
        }
    }

    fn json(self) -> Vec<Value> {
        match self {
            Self::Vector(hits) => hits
                .into_iter()
                .map(|hit| json!({"id": hit.id.to_string(), "distance": hit.distance}))
                .collect(),
            Self::Hybrid(hits) => hits,
        }
    }
}

struct Executed {
    hits: Hits,
    stats: SearchStats,
    plan: Option<crate::QueryPlan>,
    ef_search: Option<usize>,
}

/// Effective metric, HNSW identity and ANN options for one search request.
fn search_identity(
    db: &Database,
    collection: u16,
    target: &Target,
    input: &Search,
) -> Result<(DistanceMetric, HnswConfig, AnnOptions), ApiError> {
    let (metric, config, options) = identity(db, collection, target);
    let metric = input.metric.unwrap_or(metric);
    let config = match input.config {
        Some(config) => config.validated()?,
        None => config,
    };
    let options = AnnOptions {
        quantization: input
            .quantization
            .map_or(options.quantization, |name| name.0),
        rerank_multiplier: input.rerank_multiplier.unwrap_or(options.rerank_multiplier),
    };
    Ok((metric, config, options))
}

/// Candidate count and default ef_search for one request.
fn search_sizes(db: &Database, collection: u16, input: &Search) -> (usize, usize) {
    let live = db.state(collection).records.len();
    let candidate_k = input.rerank.as_ref().map_or(input.k, |rerank| {
        rerank.candidates.unwrap_or(input.k.max(50))
    });
    (candidate_k, 100.max(candidate_k.min(live)))
}

/// `mode=approximate` index (re)build, run under the exclusive lock only
/// when the shared-lock attempt found the index stale. Rechecks staleness so
/// concurrent requests that raced to this point rebuild at most once.
fn rebuild_stale(
    db: &mut Database,
    collection: u16,
    target: &Target,
    input: &Search,
    metrics: &Metrics,
) -> Result<(), ApiError> {
    let (metric, config, options) = search_identity(db, collection, target, input)?;
    let (candidate_k, default_ef) = search_sizes(db, collection, input);
    let ef = input.ef_search.unwrap_or(default_ef);
    let filter = input.filter.as_deref().unwrap_or(&[]);
    let vector = input.vector.as_deref().unwrap_or(&[]);
    db.validate_hnsw_query(collection, vector, candidate_k, ef, config, filter, metric)?;
    if approximate_stale(db, collection, input, config, metric, options) {
        let started = Instant::now();
        db.build_hnsw_with_options_in(collection, config, metric, options)?;
        metrics.rebuilds.fetch_add(1, Ordering::Relaxed);
        metrics.built(started);
        metrics.rebuild_search_ns.fetch_add(
            started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
    }
    Ok(())
}

fn approximate_stale(
    db: &Database,
    collection: u16,
    input: &Search,
    config: HnswConfig,
    metric: DistanceMetric,
    options: AnnOptions,
) -> bool {
    input.k > 0
        && !db.state(collection).records.is_empty()
        && db
            .state(collection)
            .index
            .as_ref()
            .is_none_or(|index| !index.ready_for(config, metric, options.quantization))
}

/// Executes a search under the shared lock. Returns `Ok(None)` only for
/// `mode=approximate` with a stale index; the caller then runs
/// `rebuild_stale` exclusively and retries.
fn execute(
    db: &Database,
    collection: u16,
    target: &Target,
    input: &Search,
) -> Result<Option<Executed>, ApiError> {
    let (metric, config, options) = search_identity(db, collection, target, input)?;
    if matches!(
        input.mode,
        SearchMode::Approximate | SearchMode::ApproximateReady
    ) && metric == DistanceMetric::InnerProduct
    {
        return Err(unprocessable(
            "approximate search supports squared_l2 and cosine, not inner_product",
        ));
    }
    let live = db.state(collection).records.len();
    let (candidate_k, default_ef) = search_sizes(db, collection, input);
    let filter = input.filter.as_deref().unwrap_or(&[]);
    let vector = input.vector.as_deref().unwrap_or(&[]);
    let mut stats = SearchStats::default();
    let distances = Hits::Vector;
    let mut executed = match input.mode {
        SearchMode::Exact => Executed {
            hits: distances(db.search_in(
                collection,
                vector,
                candidate_k,
                filter,
                metric,
                &mut stats,
            )?),
            stats,
            plan: None,
            ef_search: None,
        },
        SearchMode::Approximate | SearchMode::ApproximateReady => {
            let ef = input.ef_search.unwrap_or(default_ef);
            if input.mode == SearchMode::Approximate {
                db.validate_hnsw_query(
                    collection,
                    vector,
                    candidate_k,
                    ef,
                    config,
                    filter,
                    metric,
                )?;
                if approximate_stale(db, collection, input, config, metric, options) {
                    return Ok(None);
                }
            }
            Executed {
                hits: distances(db.search_ready_with_options_in(
                    collection,
                    vector,
                    candidate_k,
                    ef,
                    config,
                    options,
                    filter,
                    metric,
                    &mut stats,
                )?),
                stats,
                plan: None,
                ef_search: Some(ef),
            }
        }
        SearchMode::Auto => {
            let ef = input.ef_search.unwrap_or(default_ef);
            let planned = CollectionRef { db, id: collection }.search_auto(
                vector,
                candidate_k,
                filter,
                metric,
                config,
                options,
                ef,
            )?;
            Executed {
                hits: distances(planned.hits),
                stats: planned.stats,
                plan: Some(planned.plan),
                ef_search: Some(ef),
            }
        }
        SearchMode::Hybrid => {
            let fusion = input.fusion.as_ref();
            let defaults = crate::hybrid::FusionOptions::default();
            let fusion = crate::hybrid::FusionOptions {
                candidates: fusion.and_then(|f| f.candidates),
                rrf_k: fusion.and_then(|f| f.rrf_k).unwrap_or(defaults.rrf_k),
                vector_weight: fusion
                    .and_then(|f| f.vector_weight)
                    .unwrap_or(defaults.vector_weight),
                text_weight: fusion
                    .and_then(|f| f.text_weight)
                    .unwrap_or(defaults.text_weight),
            };
            let candidates = fusion.candidates.unwrap_or(input.k.max(50));
            let (vector_leg, ef_search) = match input.vector_leg {
                Some(VectorLegName::ApproximateReady) => {
                    let ef = input.ef_search.unwrap_or(100.max(candidates.min(live)));
                    (
                        crate::hybrid::VectorLeg::ApproximateReady {
                            config,
                            ef_search: ef,
                            options,
                        },
                        Some(ef),
                    )
                }
                _ => (crate::hybrid::VectorLeg::Exact, None),
            };
            let query = crate::hybrid::HybridQuery {
                vector: input.vector.clone(),
                text: input.text.clone().unwrap_or_default(),
                k: input.k,
                conditions: filter.to_vec(),
                metric,
                vector_leg,
                fusion,
            };
            let (hits, stats) = CollectionRef { db, id: collection }.search_hybrid(&query)?;
            Executed {
                hits: Hits::Hybrid(
                    hits.into_iter()
                        .map(|hit| {
                            json!({"id": hit.id.to_string(), "score": hit.score,
                            "distance": hit.distance, "bm25": hit.bm25,
                            "vector_rank": hit.vector_rank, "text_rank": hit.text_rank})
                        })
                        .collect(),
                ),
                stats,
                plan: None,
                ef_search,
            }
        }
    };
    if let Some(rerank) = &input.rerank {
        let reranker: Box<dyn crate::rerank::Reranker> = match rerank.mode {
            RerankMode::Exact => Box::new(crate::rerank::ExactReranker { metric }),
            RerankMode::Mmr => Box::new(crate::rerank::MmrReranker {
                lambda: rerank.lambda.unwrap_or(0.5),
            }),
        };
        let (reranked, candidate_count) = {
            let Hits::Vector(candidates) = &executed.hits else {
                return Err(unprocessable("reranking is only valid for vector search"));
            };
            let count = candidates.len() as u64;
            let reranked = CollectionRef { db, id: collection }.rerank(
                vector,
                candidates,
                input.k,
                reranker.as_ref(),
            )?;
            (reranked, count)
        };
        executed.hits = Hits::Vector(reranked);
        executed.stats.reranked_candidates = candidate_count;
    }
    Ok(Some(executed))
}

async fn search_in(
    service: Service,
    role: admin::Role,
    request: RequestId,
    target: Target,
    input: Result<Json<Search>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let input = body(input)?;
    let route = if target.legacy() {
        "/v1/search"
    } else {
        "/v1/collections/{collection}/search"
    };
    let (input, executed) = search_core(&service, role, &request.0, route, target, input).await?;
    let stats = input.explain.then(|| stats_json(&executed.stats));
    let plan = executed.plan.as_ref().map_or(Value::Null, plan_json);
    let mut response = json!({"hits": executed.hits.json()});
    if let Some(stats) = stats {
        response["stats"] = stats;
        response["plan"] = plan;
    }
    Ok(Json(response))
}

/// Transport-neutral search: request-shape validation, scoped execution,
/// search metrics and the slow-query log (`route` labels the log entry).
async fn search_core(
    service: &Service,
    role: admin::Role,
    request_id: &str,
    route: &str,
    target: Target,
    input: Search,
) -> Result<(Search, Executed), ApiError> {
    input.validate(&role)?;
    let metrics = service.metrics.clone();
    let started = Instant::now();
    let attempt = move |db: &Database, role: admin::Role, target: Target, input: Search| {
        let collection = open(db, &role, &target)?;
        let executed = execute(db, collection, &target, &input)?;
        let binding = (
            tenants::qualify(&role, target.name()),
            collection,
            db.lineage(),
        );
        Ok::<_, ApiError>((executed, binding, role, target, input))
    };
    let snapshot_reads = service.snapshot_reads;
    let (executed, collection, role, target, input) = service
        .run_read_then(
            move |db| {
                let collection = open(db, &role, &target)?;
                if snapshot_reads
                    && input.mode == SearchMode::Exact
                    && input.rerank.is_none()
                    && (input.filter.as_ref().is_none_or(Vec::is_empty)
                        || db.state(collection).settings.metadata_indexes.is_empty())
                {
                    let (metric, _, _) = search_identity(db, collection, &target, &input)?;
                    match (CollectionRef { db, id: collection }).read_snapshot() {
                        Ok(snapshot) => {
                            let binding = (
                                tenants::qualify(&role, target.name()),
                                collection,
                                db.lineage(),
                            );
                            return Ok((
                                Some((snapshot, metric, crate::current_time_ms())),
                                None,
                                binding,
                                role,
                                target,
                                input,
                            ));
                        }
                        // Retention/cardinality/byte limits are optimization limits,
                        // never a new API failure. Keep the indexed/locked path.
                        Err(Error::InvalidQuery(
                            "collection exceeds read snapshot retention limits"
                            | "read snapshot retention budget exhausted",
                        )) => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                let (executed, binding, role, target, input) = attempt(db, role, target, input)?;
                Ok((None, executed, binding, role, target, input))
            },
            |(snapshot, mut executed, binding, role, target, input)| {
                if let Some((snapshot, metric, now_ms)) = snapshot {
                    let mut stats = SearchStats::default();
                    let hits = snapshot.search_with_stats_at(
                        input.vector.as_deref().unwrap_or(&[]),
                        input.k,
                        input.filter.as_deref().unwrap_or(&[]),
                        metric,
                        now_ms,
                        &mut stats,
                    )?;
                    executed = Some(Executed {
                        hits: Hits::Vector(hits),
                        stats,
                        plan: None,
                        ef_search: None,
                    });
                }
                Ok((executed, binding, role, target, input))
            },
        )
        .await?;
    let (input, collection, executed) = match executed {
        Some(executed) => (input, collection, executed),
        None => {
            // Rare path: the approximate index is stale. Rebuild exclusively,
            // then search under the same write guard so a concurrent writer
            // cannot invalidate the fresh index between the two steps.
            service
                .run(move |db| {
                    let collection = open(db, &role, &target)?;
                    rebuild_stale(db, collection, &target, &input, &metrics)?;
                    let (executed, binding, _, _, input) = attempt(&*db, role, target, input)?;
                    let executed = executed.ok_or_else(|| internal("index stale after rebuild"))?;
                    Ok((input, binding, executed))
                })
                .await?
        }
    };
    let (collection, collection_id, lineage) = collection;
    let strategy = executed
        .plan
        .as_ref()
        .map_or(input.mode.name(), |plan| plan.strategy.name());
    service.metrics.search(strategy, &executed.stats);
    let elapsed = started.elapsed();
    let threshold = service.metrics.slow_threshold_ns.load(Ordering::Relaxed);
    if elapsed.as_nanos() >= threshold as u128 {
        service.metrics.slow_queries.fetch_add(1, Ordering::Relaxed);
        // Field names only: never vectors, text or filter values.
        let filter_fields = input
            .filter
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|condition| match condition {
                crate::Condition::Eq { field, .. }
                | crate::Condition::Range { field, .. }
                | crate::Condition::GeoRadius { field, .. } => field.clone(),
            })
            .collect::<Vec<_>>();
        observability::record_slow(
            &service.metrics,
            json!({"request_id": request_id, "unix_ms": admin::now_ms() as u64,
            "route": route, "collection": collection, "collection_id": collection_id,
            "lineage": lineage.map(|lineage| format!("{lineage:016x}")),
            "mode": input.mode.name(),
            "plan_strategy": executed.plan.as_ref().map(|plan| plan.strategy.name()),
            "k": input.k, "ef_search": executed.ef_search, "filter_fields": filter_fields,
            "duration_ms": elapsed.as_secs_f64() * 1e3,
            "stats": stats_json(&executed.stats), "result_count": executed.hits.len()}),
        );
    }
    Ok((input, executed))
}

fn quantization_name(quantization: Quantization) -> &'static str {
    collections::quantization_name(quantization)
}

/// Index state of one collection. `ready` means an index exists;
/// `settings_ready` means it matches the default identity for this route.
fn index_json(db: &Database, collection: u16, target: &Target) -> Value {
    let (metric, config, options) = identity(db, collection, target);
    let state = db.state(collection);
    let index = state.index.as_ref();
    let shown = index.map_or(config, |index| index.config);
    let (buffered_ids, buffer_vector_bytes) = index.map_or((0, 0), |index| index.buffer_usage());
    let resident = state.records.len() as u64 * state.dimensions as u64 * 4;
    json!({
        "ready": index.is_some(),
        "settings_ready": index.is_some_and(|index| index.ready_for(config, metric, options.quantization)),
        "config": {"m": shown.m, "ef_construction": shown.ef_construction, "seed": shown.seed.to_string()},
        "metric": index.map(|index| index.metric),
        "quantization": quantization_name(index.map_or(options.quantization, |index| index.options.quantization)),
        "rerank_multiplier": index.map_or(options.rerank_multiplier, |index| index.options.rerank_multiplier),
        "buffered_ids": buffered_ids,
        "buffer_vector_bytes": buffer_vector_bytes,
        "graph_nodes": index.map_or(0, |index| index.graph_nodes()),
        "stale_nodes": index.map_or(0, |index| index.stale_nodes()),
        "memory": index.map(|index| {
            let memory = index.memory();
            json!({"graph_nodes": memory.graph_nodes, "graph_link_bytes": memory.link_bytes,
                "code_bytes": memory.code_bytes, "codebook_bytes": memory.codebook_bytes,
                "buffer_vector_bytes": memory.buffer_vector_bytes,
                "resident_vector_bytes": resident})
        }),
        "max_buffer_ids": crate::MAX_HNSW_BUFFER_IDS,
        "max_buffer_vector_bytes": crate::MAX_HNSW_BUFFER_VECTOR_BYTES,
    })
}

async fn index_status(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
) -> Result<Json<Value>, ApiError> {
    index_status_in(service, role, Target::Legacy).await
}

async fn collection_index_status(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    target: Result<Path<String>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    index_status_in(service, role, collection_path(target)?).await
}

async fn index_status_in(
    service: Service,
    role: admin::Role,
    target: Target,
) -> Result<Json<Value>, ApiError> {
    service
        .run_read(move |db| {
            let collection = open(db, &role, &target)?;
            Ok(Json(index_json(db, collection, &target)))
        })
        .await
}

/// Optional index identity overrides; omitted fields use the route defaults.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityInput {
    metric: Option<DistanceMetric>,
    config: Option<admin::IndexConfig>,
    quantization: Option<collections::QuantizationName>,
    rerank_multiplier: Option<u8>,
}

impl IdentityInput {
    fn resolve(
        &self,
        db: &Database,
        collection: u16,
        target: &Target,
    ) -> Result<(DistanceMetric, HnswConfig, AnnOptions), ApiError> {
        let (metric, config, options) = identity(db, collection, target);
        Ok((
            self.metric.unwrap_or(metric),
            match self.config {
                Some(config) => config.validated()?,
                None => config,
            },
            AnnOptions {
                quantization: self
                    .quantization
                    .map_or(options.quantization, |name| name.0),
                rerank_multiplier: self.rerank_multiplier.unwrap_or(options.rerank_multiplier),
            },
        ))
    }
}

async fn build_index(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    input: Result<Json<Value>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    build_index_in(service, role, Target::Legacy, input).await
}

async fn collection_build_index(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    target: Result<Path<String>, PathRejection>,
    input: Result<Json<Value>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    build_index_in(service, role, collection_path(target)?, input).await
}

async fn build_index_in(
    service: Service,
    role: admin::Role,
    target: Target,
    input: Result<Json<Value>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let input = body(input)?;
    if !input.is_object() {
        return Err(unprocessable("index build requires a JSON object"));
    }
    let input: IdentityInput =
        serde_json::from_value(input).map_err(|error| unprocessable(error.to_string()))?;
    let metrics = service.metrics.clone();
    service
        .run(move |db| {
            let collection = open(db, &role, &target)?;
            let (metric, config, options) = input.resolve(db, collection, &target)?;
            let started = Instant::now();
            db.build_hnsw_with_options_in(collection, config, metric, options)?;
            metrics.rebuilds.fetch_add(1, Ordering::Relaxed);
            metrics.built(started);
            metrics.rebuild_search_ns.fetch_add(
                started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
                Ordering::Relaxed,
            );
            Ok(StatusCode::NO_CONTENT)
        })
        .await
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Quality {
    samples: usize,
    k: usize,
    ef_search: usize,
    #[serde(deserialize_with = "admin::seed")]
    seed: u64,
    max_distance_evaluations: u64,
    metric: Option<DistanceMetric>,
    config: Option<admin::IndexConfig>,
    quantization: Option<collections::QuantizationName>,
    rerank_multiplier: Option<u8>,
}

impl Default for Quality {
    fn default() -> Self {
        let sample = crate::quality::QualitySampleOptions::default();
        Self {
            samples: sample.samples,
            k: sample.k,
            ef_search: sample.ef_search,
            seed: sample.seed,
            max_distance_evaluations: sample.max_distance_evaluations,
            metric: None,
            config: None,
            quantization: None,
            rerank_multiplier: None,
        }
    }
}

async fn quality(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    input: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    quality_in(service, role, Target::Legacy, &input).await
}

async fn collection_quality(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    target: Result<Path<String>, PathRejection>,
    input: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    quality_in(service, role, collection_path(target)?, &input).await
}

async fn quality_in(
    service: Service,
    role: admin::Role,
    target: Target,
    input: &[u8],
) -> Result<Json<Value>, ApiError> {
    let input: Quality = optional(input)?;
    let metrics = service.metrics.clone();
    service
        .run(move |db| {
            let collection = open(db, &role, &target)?;
            let identity = IdentityInput {
                metric: input.metric,
                config: input.config,
                quantization: input.quantization,
                rerank_multiplier: input.rerank_multiplier,
            };
            let (metric, config, options) = identity.resolve(db, collection, &target)?;
            let report = CollectionRef {
                db: &*db,
                id: collection,
            }
            .ann_quality(
                config,
                metric,
                options,
                crate::quality::QualitySampleOptions {
                    samples: input.samples,
                    k: input.k,
                    ef_search: input.ef_search,
                    seed: input.seed,
                    max_distance_evaluations: input.max_distance_evaluations,
                },
            )?;
            if let Some(recall) = report.recall_at_k
                && let Ok(mut last) = metrics.ann_quality_last_recall.lock()
            {
                *last = Some(recall);
            }
            Ok(Json(json!({
                "recall_at_k": report.recall_at_k,
                "samples_used": report.samples_used,
                "distance_evaluations": report.distance_evaluations,
                "skipped_reason": report.skipped_reason,
            })))
        })
        .await
}

#[cfg(test)]
use observability::metrics;

/// Shared token bucket; default capacity is one second of requests. Monotonic
/// time avoids clock changes granting tokens. State is bounded by store capacity.
fn rate_limit(service: &Service, role: &admin::Role) -> Result<(), ApiError> {
    let admin::Role::Scoped(grant) = role else {
        return Ok(());
    };
    let Some(rate) = grant.requests_per_second else {
        return Ok(());
    };
    let mut buckets = service.rate_limits.lock().map_err(internal)?;
    if buckets.len() >= 512 {
        buckets.retain(|_, (_, at)| at.elapsed().as_secs() < 3600);
    }
    if !buckets.contains_key(&grant.id) && buckets.len() >= 512 {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "busy",
            "rate limiter capacity reached",
        ));
    }
    let (tokens, at) = buckets
        .entry(grant.id.clone())
        .or_insert((rate as f64, Instant::now()));
    *tokens = (*tokens + at.elapsed().as_secs_f64() * rate as f64).min(rate as f64);
    *at = Instant::now();
    if *tokens < 1.0 {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "credential request rate exceeded",
        ));
    }
    *tokens -= 1.0;
    Ok(())
}
fn quota_mutations(operations: &[Mutation]) -> Vec<(u64, bool)> {
    operations
        .iter()
        .map(|op| match op {
            Mutation::Put { id, .. } | Mutation::PutWithMetadata { id, .. } => (*id, true),
            Mutation::Delete { id } => (*id, false),
        })
        .collect()
}
/// Evaluate projected live counts under the same owner lock as the commit.
/// Counts are over all collections visible to a credential (not write history).
fn enforce_quota(
    service: &Service,
    db: &Database,
    role: &admin::Role,
    collection: u16,
    changes: &[(u64, bool)],
) -> Result<(), ApiError> {
    let grant = match role {
        admin::Role::Scoped(grant) => Some(grant),
        _ => None,
    };
    let state = db.state(collection);
    let tenant_id = state
        .name
        .strip_prefix('t')
        .and_then(|name| name.split_once("__"))
        .map(|(id, _)| id)
        .filter(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()));
    let tenant = match (tenant_id, &service.admin.tenants) {
        (Some(id), Some(store)) if store.known(id)? => Some(store.get(id)?),
        _ => None,
    };
    if grant.is_none_or(|g| g.max_vectors.is_none() && g.max_vectors_per_collection.is_none())
        && tenant.as_ref().is_none_or(|t| t.max_vectors.is_none())
    {
        return Ok(());
    }
    let mut projected = BTreeMap::new();
    for &(id, present) in changes {
        projected.insert(id, present);
    }
    let mut count = state.records.len() as i128;
    for (id, present) in projected {
        count += i128::from(present) - i128::from(state.records.contains_key(&id));
    }
    let delta = count - state.records.len() as i128;
    let credential_count: i128 = db
        .collections
        .iter()
        .filter(|(id, state)| grant.is_some_and(|g| g.covers(db.lineage(), **id, &state.name)))
        .map(|(_, s)| s.records.len() as i128)
        .sum();
    let tenant_count: i128 = db
        .collections
        .values()
        .filter(|s| tenant_id.is_some_and(|id| s.name.starts_with(&format!("t{id}__"))))
        .map(|s| s.records.len() as i128)
        .sum();
    if grant
        .and_then(|g| g.max_vectors_per_collection)
        .is_some_and(|max| count > max as i128)
        || grant
            .and_then(|g| g.max_vectors)
            .is_some_and(|max| credential_count + delta > max as i128)
        || tenant
            .and_then(|t| t.max_vectors)
            .is_some_and(|max| tenant_count + delta > max as i128)
    {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "quota_exceeded",
            "live vector quota exceeded; delete vectors or raise the configured quota",
        ));
    }
    Ok(())
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
        db.file = crate::LockedFile::disk(std::fs::File::open(&path).unwrap());
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
    async fn matching_conditional_write_reports_indeterminate_commit_then_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("conditional-failure.vlite");
        let mut db = Database::create(&path, 1).unwrap();
        db.put(1, &[1.0], b"").unwrap();
        db.file = crate::LockedFile::disk(std::fs::File::open(&path).unwrap());
        let service = Service::new(db);
        let app = router(service.clone());
        let send = |method: &str, tag: Option<&str>| {
            let mut builder = Request::builder()
                .method(method)
                .uri("/v1/vectors/1")
                .header(header::CONTENT_TYPE, "application/json");
            if let Some(tag) = tag {
                builder = builder.header(header::IF_MATCH, tag);
            }
            app.clone().oneshot(
                builder
                    .body(Body::from(json!({"vector":[2]}).to_string()))
                    .unwrap(),
            )
        };
        let response = send("GET", None).await.unwrap();
        let tag = response.headers()[header::ETAG]
            .to_str()
            .unwrap()
            .to_owned();
        let response = send("PUT", Some(&tag)).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = http_body_util::BodyExt::collect(response.into_body())
            .await
            .unwrap()
            .to_bytes();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "commit_indeterminate");
        assert_eq!(service.metrics.commit_failures.load(Ordering::Relaxed), 1);
        for method in ["PUT", "DELETE"] {
            let response = send(method, Some(&tag)).await.unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        }
        assert_eq!(service.metrics.commit_failures.load(Ordering::Relaxed), 1);
        assert!(service.close().await.is_err());
    }

    #[tokio::test]
    async fn commit_failure_metrics_count_worker_failures() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("failure.vlite");
        let mut db = Database::create(&path, 1).unwrap();
        db.file = crate::LockedFile::disk(std::fs::File::open(&path).unwrap());
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

/// LEG-017 (TEST-015): every graph `Error` variant has an explicit status and
/// stable code; none reaches the generic 500 arm.
#[cfg(all(test, feature = "graph"))]
mod graph_errors {
    use super::*;

    /// Every graph `Error` variant with its HTTP status and stable code; add
    /// each new graph variant here and to `From<Error> for ApiError`.
    pub(super) fn table() -> Vec<(Error, StatusCode, &'static str)> {
        const CONFLICT: StatusCode = StatusCode::CONFLICT;
        const INVALID: StatusCode = StatusCode::UNPROCESSABLE_ENTITY;
        const UNSUPPORTED: StatusCode = StatusCode::NOT_IMPLEMENTED;
        vec![
            (Error::GraphsNotEnabled, CONFLICT, "graphs_not_enabled"),
            (Error::GraphsRequireV5, INVALID, "graphs_require_v5"),
            (Error::InvalidGraphName, INVALID, "invalid_graph_name"),
            (Error::GraphExists, CONFLICT, "graph_exists"),
            (Error::GraphNotFound, CONFLICT, "graph_not_found"),
            (Error::GraphNotEmpty, CONFLICT, "graph_not_empty"),
            (Error::TooManyGraphs, CONFLICT, "too_many_graphs"),
            (Error::GraphKeyExists, CONFLICT, "graph_key_exists"),
            (Error::GraphKeyConflict, CONFLICT, "graph_key_conflict"),
            (Error::GraphNodeNotFound, CONFLICT, "graph_node_not_found"),
            (Error::GraphEdgeNotFound, CONFLICT, "graph_edge_not_found"),
            (
                Error::GraphNodeHasEdges { incident: 2 },
                CONFLICT,
                "graph_node_has_edges",
            ),
            (
                Error::GraphAttachmentConflict,
                CONFLICT,
                "graph_attachment_conflict",
            ),
            (
                Error::GraphAttachmentNotFound,
                CONFLICT,
                "graph_attachment_not_found",
            ),
            (
                Error::GraphRecordNotAttachable,
                CONFLICT,
                "graph_record_not_attachable",
            ),
            (
                Error::GraphAttachedRecord,
                CONFLICT,
                "graph_attached_record",
            ),
            (
                Error::GraphAttachedRecordExpiry,
                CONFLICT,
                "graph_attached_record_expiry",
            ),
            (
                Error::GraphLimitExceedsCeiling {
                    limit: "max_hops",
                    requested: 9,
                    ceiling: 8,
                },
                INVALID,
                "graph_limit_exceeds_ceiling",
            ),
            (Error::GraphCursorStale, CONFLICT, "graph_cursor_stale"),
            (
                Error::GraphChangeFeedUnsupported,
                UNSUPPORTED,
                "graph_change_feed_unsupported",
            ),
            (
                Error::GraphUnsupportedOperation,
                CONFLICT,
                "graph_unsupported_operation",
            ),
            (
                Error::GraphInvalidOptions("confirm_graph_data_loss is required"),
                INVALID,
                "graph_invalid_options",
            ),
        ]
    }

    #[test]
    fn every_graph_error_maps_explicitly_never_500() {
        for (error, status, code) in table() {
            let message = error.to_string();
            let mapped = ApiError::from(error);
            assert_eq!((mapped.status, mapped.code), (status, code), "{message}");
            assert_eq!(mapped.message, message);
        }
    }
}
