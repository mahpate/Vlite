//! Optional HTTP transport over one exclusively owned embedded database.

use std::sync::{Arc, Mutex};

use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Path, State,
        rejection::{JsonRejection, PathRejection},
    },
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, prelude::BASE64_STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Semaphore;

use crate::{Database, Error, MAX_BATCH_OPERATIONS, MAX_PAYLOAD_BYTES, Mutation};

pub const MAX_JSON_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_SEARCH_K: usize = 10_000;
const MAX_IN_FLIGHT: usize = 16;

/// Cloneable service state. Database work runs off the asynchronous I/O threads.
#[derive(Clone)]
pub struct Service {
    database: Arc<Mutex<Option<Database>>>,
    requests: Arc<Semaphore>,
    workers: Arc<Semaphore>,
}

impl Service {
    pub fn new(database: Database) -> Self {
        Self {
            database: Arc::new(Mutex::new(Some(database))),
            requests: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
            workers: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
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
        tokio::task::spawn_blocking(move || {
            // The permit stays with the worker even if the HTTP client disconnects.
            let _permit = permit;
            let mut owner = database.lock().map_err(internal)?;
            let db = owner.as_mut().ok_or_else(unavailable)?;
            operation(db)
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

pub fn router(service: Service) -> Router {
    Router::new()
        .route("/health", get(|| async { Json(json!({"status": "ok"})) }))
        .route("/ready", get(ready))
        .route("/v1/database", get(info))
        .route("/v1/vectors/{id}", get(fetch).put(put).delete(delete))
        .route("/v1/batches", post(batch))
        .route("/v1/search", post(search))
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
        .with_state(service)
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

fn internal(error: impl std::fmt::Display) -> ApiError {
    eprintln!("REST internal error: {error}");
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
            Error::DimensionMismatch { .. }
            | Error::InvalidDimensions(_)
            | Error::NonFiniteVector
            | Error::PayloadTooLarge(_)
            | Error::BatchTooLarge
            | Error::InvalidHnswConfig
            | Error::InvalidEfSearch => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_input",
                error.to_string(),
            ),
            Error::BatchRequiresUpgrade => {
                Self::new(StatusCode::CONFLICT, "upgrade_required", error.to_string())
            }
            Error::CommitIndeterminate(_) => {
                eprintln!("REST commit error: {error}");
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
                "payload_base64": BASE64_STANDARD.encode(&record.payload)
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
            db.put(id, &input.vector, &payload)?;
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
            } => Ok(Mutation::Put {
                id: id(&text)?,
                vector,
                payload: payload(&payload_base64)?,
            }),
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
}

impl TryFrom<String> for SearchMode {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "exact" => Ok(Self::Exact),
            "approximate" => Ok(Self::Approximate),
            _ => Err("mode must be exact or approximate"),
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
    ef_search: Option<usize>,
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
    service
        .run(move |db| {
            let hits = match input.mode {
                SearchMode::Exact => db.search(&input.vector, input.k)?,
                SearchMode::Approximate => db.search_approximate(
                    &input.vector,
                    input.k,
                    input.ef_search.unwrap_or(100.max(input.k.min(db.len()))),
                    crate::HnswConfig::default(),
                )?,
            }
            .into_iter()
            .map(|hit| json!({"id": hit.id.to_string(), "distance": hit.distance}))
            .collect::<Vec<_>>();
            Ok(Json(json!({"hits": hits})))
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
}
