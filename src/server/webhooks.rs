use super::*;
use crate::changes::{Change, ChangeKind, FeedCursor};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Subscription {
    id: String,
    url: String,
    collections: Option<Vec<String>>,
    /// Selected event kinds; None delivers every kind, vector and graph.
    events: Option<Vec<String>>,
    /// Optional graph-name filter for graph events.
    graphs: Option<Vec<String>>,
    secret: String,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Delivery {
    attempts: u64,
    last_success_ms: Option<u128>,
    last_error: Option<String>,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Persisted {
    subscriptions: BTreeMap<String, Subscription>,
    deliveries: BTreeMap<String, Delivery>,
}

pub(super) struct Dispatcher {
    directory: Mutex<Option<PathBuf>>,
    state: Mutex<Persisted>,
    cursor: Mutex<Option<FeedCursor>>,
    stopping: Arc<AtomicBool>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}
impl Default for Dispatcher {
    fn default() -> Self {
        Self {
            directory: Mutex::new(None),
            state: Mutex::new(Persisted::default()),
            cursor: Mutex::new(None),
            stopping: Arc::new(AtomicBool::new(false)),
            worker: Mutex::new(None),
        }
    }
}
impl Dispatcher {
    pub(super) fn configure(
        self: &Arc<Self>,
        directory: Option<PathBuf>,
        service: Service,
    ) -> std::io::Result<()> {
        let Some(directory) = directory else {
            return Ok(());
        };
        *self
            .directory
            .lock()
            .map_err(|_| std::io::Error::other("webhook lock poisoned"))? = Some(directory.clone());
        let path = directory.join("webhooks.json");
        let loaded = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<Persisted>(&bytes)
                .map_err(|e| std::io::Error::other(format!("invalid webhook store: {e}")))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Persisted::default(),
            Err(error) => return Err(error),
        };
        *self
            .state
            .lock()
            .map_err(|_| std::io::Error::other("webhook lock poisoned"))? = loaded;
        let cursor_path = directory.join("webhooks.cursor");
        *self
            .cursor
            .lock()
            .map_err(|_| std::io::Error::other("webhook lock poisoned"))? =
            FeedCursor::load(cursor_path)
                .map_err(|e| std::io::Error::other(format!("invalid webhook cursor: {e}")))?;
        let this = self.clone();
        let stop = self.stopping.clone();
        let worker = thread::Builder::new()
            .name("vlite-webhooks".into())
            .spawn(move || run_dispatcher(this, service, stop))?;
        *self
            .worker
            .lock()
            .map_err(|_| std::io::Error::other("webhook lock poisoned"))? = Some(worker);
        Ok(())
    }
    fn persist(&self) -> Result<(), ApiError> {
        let directory = self
            .directory
            .lock()
            .map_err(internal)?
            .clone()
            .ok_or_else(disabled)?;
        let state = self.state.lock().map_err(internal)?;
        let bytes = serde_json::to_vec(&*state).map_err(internal)?;
        let path = directory.join("webhooks.json");
        atomic_write(&path, &bytes).map_err(internal)
    }
    pub(super) fn shutdown(&self) {
        self.stopping.store(true, Ordering::Release);
        if let Ok(mut worker) = self.worker.lock()
            && let Some(worker) = worker.take()
        {
            let _ = worker.join();
        }
    }
}

fn disabled() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "webhooks_disabled",
        "webhooks require VLITE_ADMIN_DIR",
    )
}
fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)?;
    use std::io::Write;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

pub(super) fn routes() -> Router<Service> {
    Router::new()
        .route("/v1/admin/webhooks", get(list).post(create))
        .route("/v1/admin/webhooks/{id}", get(status).delete(remove))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    url: String,
    collections: Option<Vec<String>>,
    events: Option<Vec<String>>,
    graphs: Option<Vec<String>>,
    secret: String,
}
fn validate(input: &Create) -> Result<(), ApiError> {
    let u = reqwest::Url::parse(&input.url)
        .map_err(|_| unprocessable("webhook URL must be an absolute HTTP(S) URL"))?;
    if !matches!(u.scheme(), "http" | "https")
        || u.host_str().is_none()
        || !u.username().is_empty()
        || u.password().is_some()
        || u.fragment().is_some()
    {
        return Err(unprocessable(
            "webhook URL must be HTTP(S), without credentials or fragment",
        ));
    }
    if input.secret.len() < 16 || input.secret.len() > 256 {
        return Err(unprocessable("webhook secret must be 16..256 UTF-8 bytes"));
    }
    if input.events.as_ref().is_some_and(|events| {
        events.is_empty()
            || events.len() > EVENT_KINDS.len()
            || events.iter().any(|e| !EVENT_KINDS.contains(&e.as_str()))
    }) {
        return Err(unprocessable(
            "events must be a nonempty list of supported change kinds",
        ));
    }
    if input
        .collections
        .as_ref()
        .is_some_and(|cs| cs.len() > 64 || cs.iter().any(|c| c.is_empty() || c.len() > 63))
    {
        return Err(unprocessable(
            "collection filters must contain at most 64 valid names",
        ));
    }
    if input.graphs.as_ref().is_some_and(|gs| {
        gs.len() > 64
            || gs.iter().any(|g| {
                g.is_empty()
                    || g.len() > crate::property_graph::engine::limits::MAX_GRAPH_NAME_BYTES
            })
    }) {
        return Err(unprocessable(
            "graph filters must contain at most 64 valid names",
        ));
    }
    Ok(())
}
/// Every event kind a subscription may select; a subscription without an
/// `events` list receives all of them.
const EVENT_KINDS: [&str; 10] = [
    "put",
    "delete",
    "metadata_update",
    "collection_created",
    "collection_dropped",
    "settings_updated",
    "vectors_updated",
    "graph_created",
    "graph_dropped",
    "graph_batch",
];
fn public_subscription(s: &Subscription, d: &Delivery) -> Value {
    json!({"id":s.id,"url":s.url,"collections":s.collections,"events":s.events,"graphs":s.graphs,"attempts":d.attempts,"last_success_ms":d.last_success_ms,"last_error":d.last_error})
}
async fn create(
    State(service): State<Service>,
    input: Result<Json<Create>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let input = body(input)?;
    validate(&input)?;
    if service
        .webhooks
        .directory
        .lock()
        .map_err(internal)?
        .is_none()
    {
        return Err(disabled());
    }
    let mut raw = [0u8; 12];
    getrandom::fill(&mut raw).map_err(internal)?;
    let id = hex(&raw);
    let s = Subscription {
        id: id.clone(),
        url: input.url,
        collections: input.collections,
        events: input.events,
        graphs: input.graphs,
        secret: input.secret,
    };
    let d = service.webhooks.clone();
    let output = public_subscription(&s, &Delivery::default());
    tokio::task::spawn_blocking(move || {
        let mut state = d.state.lock().map_err(internal)?;
        state.subscriptions.insert(id.clone(), s);
        state.deliveries.insert(id, Delivery::default());
        drop(state);
        d.persist()?;
        Ok::<(), ApiError>(())
    })
    .await
    .map_err(internal)??;
    Ok((StatusCode::CREATED, Json(output)))
}
async fn list(State(service): State<Service>) -> Result<Json<Value>, ApiError> {
    let d = service.webhooks.clone();
    let values = tokio::task::spawn_blocking(move || {
        let s = d.state.lock().map_err(internal)?;
        Ok::<_, ApiError>(
            s.subscriptions
                .iter()
                .map(|(id, subscription)| {
                    public_subscription(
                        subscription,
                        s.deliveries.get(id).unwrap_or(&Delivery::default()),
                    )
                })
                .collect::<Vec<_>>(),
        )
    })
    .await
    .map_err(internal)??;
    Ok(Json(json!({"items":values})))
}
async fn status(
    State(service): State<Service>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let d = service.webhooks.clone();
    let value = tokio::task::spawn_blocking(move || {
        let s = d.state.lock().map_err(internal)?;
        let sub = s.subscriptions.get(&id).ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "webhook_not_found",
                "webhook subscription not found",
            )
        })?;
        Ok::<_, ApiError>(public_subscription(
            sub,
            s.deliveries.get(&id).unwrap_or(&Delivery::default()),
        ))
    })
    .await
    .map_err(internal)??;
    Ok(Json(value))
}
async fn remove(
    State(service): State<Service>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let d = service.webhooks.clone();
    tokio::task::spawn_blocking(move || {
        let mut s = d.state.lock().map_err(internal)?;
        if s.subscriptions.remove(&id).is_none() {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "webhook_not_found",
                "webhook subscription not found",
            ));
        }
        s.deliveries.remove(&id);
        drop(s);
        d.persist()?;
        Ok::<_, ApiError>(())
    })
    .await
    .map_err(internal)??;
    Ok(Json(json!({"deleted":true})))
}

fn kind_name(change: &Change) -> &'static str {
    match change.kind {
        ChangeKind::Put { .. } => "put",
        ChangeKind::Delete { .. } => "delete",
        ChangeKind::MetadataUpdate { .. } => "metadata_update",
        ChangeKind::CollectionCreated { .. } => "collection_created",
        ChangeKind::CollectionDropped => "collection_dropped",
        ChangeKind::SettingsUpdated { .. } => "settings_updated",
        ChangeKind::VectorsUpdated => "vectors_updated",
        ChangeKind::GraphCreated { .. } => "graph_created",
        ChangeKind::GraphDropped { .. } => "graph_dropped",
        ChangeKind::GraphBatch { .. } => "graph_batch",
    }
}
/// Vector events match the collections filter, graph events the graphs
/// filter; an absent filter matches everything of that family.
fn selects(sub: &Subscription, change: &Change) -> bool {
    sub.events
        .as_ref()
        .is_none_or(|kinds| kinds.iter().any(|kind| kind == kind_name(change)))
        && match super::graph::feed_graph(&change.kind) {
            Some(graph) => sub
                .graphs
                .as_ref()
                .is_none_or(|names| names.iter().any(|n| n == graph)),
            None => sub
                .collections
                .as_ref()
                .is_none_or(|names| names.contains(&change.collection)),
        }
}
fn event_json(change: &Change) -> Value {
    let body = match &change.kind {
        ChangeKind::Put {
            id,
            record,
            expires_at_ms,
        } => {
            json!({"type":"put","id":id.to_string(),"vector":record.vector,"payload_base64":BASE64_STANDARD.encode(&record.payload),"metadata":record.metadata,"expires_at_ms":expires_at_ms})
        }
        ChangeKind::Delete { id } => json!({"type":"delete","id":id.to_string()}),
        ChangeKind::MetadataUpdate { id, metadata } => {
            json!({"type":"metadata_update","id":id.to_string(),"metadata":metadata})
        }
        ChangeKind::CollectionCreated { dimensions, .. } => {
            json!({"type":"collection_created","dimensions":dimensions})
        }
        ChangeKind::CollectionDropped => json!({"type":"collection_dropped"}),
        ChangeKind::SettingsUpdated { .. } => json!({"type":"settings_updated"}),
        ChangeKind::VectorsUpdated => json!({"type":"vectors_updated"}),
        graph @ (ChangeKind::GraphCreated { .. }
        | ChangeKind::GraphDropped { .. }
        | ChangeKind::GraphBatch { .. }) => graph.graph_json().expect("graph feed event"),
    };
    json!({"sequence":change.sequence.to_string(),"ordinal":change.ordinal,"collection":change.collection,"committed_at_ms":change.committed_at_ms,"change":body})
}
fn run_dispatcher(dispatcher: Arc<Dispatcher>, service: Service, stopping: Arc<AtomicBool>) {
    let client = match reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };
    while !stopping.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(500));
        if stopping.load(Ordering::Acquire) {
            break;
        }
        let current = dispatcher.cursor.lock().ok().and_then(|cursor| *cursor);
        let (cursor, batch) = {
            let Ok(owner) = service.database.read() else {
                continue;
            };
            let Some(database) = owner.as_ref() else {
                break;
            };
            let cursor = match current {
                Some(cursor) => cursor,
                None => match database.feed_cursor() {
                    Ok(cursor) => cursor,
                    Err(_) => continue,
                },
            };
            match database.changes_after(&cursor, 1000) {
                Ok(batch) => (cursor, batch),
                Err(_) => continue,
            }
        };
        if current.is_none() {
            if let Ok(mut saved) = dispatcher.cursor.lock() {
                *saved = Some(cursor);
            }
            if let Some(path) = dispatcher
                .directory
                .lock()
                .ok()
                .and_then(|d| d.clone())
                .map(|d| d.join("webhooks.cursor"))
            {
                let _ = cursor.save(path);
            }
        }
        if batch.changes.is_empty() {
            if let Ok(mut saved) = dispatcher.cursor.lock() {
                *saved = Some(batch.next);
            }
            if let Some(path) = dispatcher
                .directory
                .lock()
                .ok()
                .and_then(|d| d.clone())
                .map(|d| d.join("webhooks.cursor"))
            {
                let _ = batch.next.save(path);
            }
            continue;
        }
        let subscriptions = dispatcher
            .state
            .lock()
            .map(|state| state.subscriptions.values().cloned().collect::<Vec<_>>());
        let Ok(subscriptions) = subscriptions else {
            continue;
        };
        let mut complete = true;
        for sub in subscriptions {
            let selected = batch
                .changes
                .iter()
                .filter(|change| selects(&sub, change))
                .map(event_json)
                .collect::<Vec<_>>();
            if selected.is_empty() {
                continue;
            }
            let body =
                match serde_json::to_vec(&json!({"subscription_id":sub.id,"changes":selected})) {
                    Ok(body) => body,
                    Err(_) => continue,
                };
            let signature = hmac_sha256(sub.secret.as_bytes(), &body);
            let mut delivered = false;
            let mut attempts = 0u64;
            let mut last_error = String::new();
            for attempt in 0..5 {
                if stopping.load(Ordering::Acquire) {
                    complete = false;
                    break;
                }
                attempts = attempts.saturating_add(1);
                let response = client
                    .post(&sub.url)
                    .header("content-type", "application/json")
                    .header("x-vlite-signature", format!("sha256={signature}"))
                    .body(body.clone())
                    .send();
                match response {
                    Ok(response) if response.status().is_success() => {
                        delivered = true;
                        break;
                    }
                    Ok(response) => {
                        last_error =
                            format!("receiver returned HTTP {}", response.status().as_u16())
                    }
                    Err(error) => {
                        last_error = if error.is_timeout() {
                            "delivery timed out".into()
                        } else {
                            "delivery transport error".into()
                        }
                    }
                }
                if attempt < 4 {
                    thread::sleep(Duration::from_secs(1u64 << attempt));
                }
            }
            if let Ok(mut state) = dispatcher.state.lock()
                && let Some(delivery) = state.deliveries.get_mut(&sub.id)
            {
                delivery.attempts = delivery.attempts.saturating_add(attempts);
                if delivered {
                    delivery.last_success_ms = Some(super::admin::now_ms());
                    delivery.last_error = None;
                } else {
                    delivery.last_error = Some(last_error.clone());
                }
            }
            let _ = dispatcher.persist();
            if !delivered {
                complete = false;
                break;
            }
        }
        if complete {
            if let Ok(mut saved) = dispatcher.cursor.lock() {
                *saved = Some(batch.next);
            }
            if let Some(path) = dispatcher
                .directory
                .lock()
                .ok()
                .and_then(|d| d.clone())
                .map(|d| d.join("webhooks.cursor"))
            {
                let _ = batch.next.save(path);
            }
        }
        let _ = cursor;
    }
}

fn hmac_sha256(secret: &[u8], message: &[u8]) -> String {
    let mut key = [0u8; 64];
    if secret.len() > 64 {
        let digest = sha2::Sha256::digest(secret);
        key[..32].copy_from_slice(&digest);
    } else {
        key[..secret.len()].copy_from_slice(secret);
    }
    let mut inner = Vec::with_capacity(64 + message.len());
    inner.extend(key.iter().map(|byte| byte ^ 0x36));
    inner.extend_from_slice(message);
    let inner = sha2::Sha256::digest(inner);
    let mut outer = Vec::with_capacity(96);
    outer.extend(key.iter().map(|byte| byte ^ 0x5c));
    outer.extend_from_slice(&inner);
    hex(&sha2::Sha256::digest(outer))
}
