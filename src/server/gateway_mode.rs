//! Gateway mode (feature-aca-ha ITEM-014/015/016): `vlite-server gateway
//! --config <json>`, plus the `gateway-keygen` and `control-state` commands
//! that make its inputs.
//!
//! A gateway opens no database. Every request is authorized from one fresh
//! control-state snapshot (`control_state::Feed`): authenticate a `vlk_`
//! tenant credential, classify the operation (`gateway::classify`, unmapped
//! routes refused), apply the same scoped-credential rule as a backend for
//! the routed tenant's lineage, take one request from this replica's share
//! of the credential's rate limit (`gateway::replica_share`), and only then
//! forward to that tenant's writer or opted-in reader (`gateway::route_target`)
//! with a principal envelope signed for the record's audience and snapshot generation
//! `A`. The backend (`gateway_trust`) re-verifies all of it independently.
//!
//! Forwarding keeps the method, path, query, body and status; the caller's
//! `Authorization`, `Cookie`, principal, proxy, forwarding and hop-by-hop
//! headers (and any a `Connection` header nominates) are dropped. Request
//! bodies are bounded (`max_request_bytes`), responses are streamed, the
//! upstream client follows no redirect and uses no proxy, and the upstream
//! URL is the record's origin plus the caller's validated path.
//! `upstream_timeout_ms` bounds connecting, waiting for the response head and
//! each wait for the next piece of a streamed body, in either direction; it
//! is not a limit on the whole request or response. A request body that
//! stalls is `408 request_body_timeout` with nothing sent, and a client that
//! stops reading a response loses it, so neither holds an in-flight slot
//! indefinitely. Reader failures before a response head, 503s, and lineage
//! or binding-generation mismatches fall back to the writer once. Writes are
//! never retried: a connection failure after a write may have been sent is
//! `502 write_outcome_unknown` (`gateway::ambiguous_write`).
//!
//! Past `drain_timeout_ms` at shutdown every request still running is
//! cancelled: one not yet sent is `503 draining`, a sent write is
//! `502 write_outcome_unknown`, a sent read is `503 draining`, and a
//! streaming response ends with an error; nothing is retried or re-routed.
//!
//! Not yet: login, the dashboard and super-admin operations through a
//! gateway (OD-004/OD-012). Dedicated responses require a valid epoch stamp
//! checked against the freshest usable same-lineage directory before streaming;
//! an epoch refusal is final, with no retry or writer fallback.

use super::control_state::{self, Feed, Snapshot, Source};
use super::gateway::{self, Operation, RouteRefusal};
use super::gateway_trust::{MAX_CONFIG_BYTES, MAX_MS, read_bounded};
use super::principal_envelope::{self, GatewayKeyBundle, Signer};
use super::*;
use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::HeaderName;
use std::collections::HashMap;
use std::ffi::OsString;
use std::future::{Future, IntoFuture};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

const GATEWAY_USAGE: &str = "vlite-server gateway --config <json>";
const KEYGEN_USAGE: &str = "vlite-server gateway-keygen --kid <kid> --generation <n> --signing-key-out <new-file> --public-bundle-out <new-file>";
const CONTROL_USAGE: &str = "vlite-server control-state publish|update --target-file <json> --object <name> [--shared-key-file <path>] --admin-dir <dir> [--directory-file <json>] [update only, at most one of: --revoke-credential <id> | --suspend-tenant <id> | --resume-tenant <id> | --reimport-if-generation <n>]";
/// A signing seed file is 43 base64url characters; allow whitespace.
const MAX_SEED_FILE_BYTES: u64 = 1024;
const MAX_IN_FLIGHT: usize = 65_536;
const MAX_BUCKETS: usize = 1_000_000;
/// After the drain deadline: how long graceful HTTP connections get to settle
/// after cancellation, and separately how long the control-state poller gets to
/// stop before it is left to finish its read on its own.
const SHUTDOWN_SETTLE: Duration = Duration::from_secs(1);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GatewayFile {
    bind: SocketAddr,
    /// A non-loopback `bind` must be explicitly allowed.
    #[serde(default)]
    allow_remote: bool,
    pub(super) control_state: Source,
    s_ctl_ms: u64,
    poll_interval_ms: u64,
    /// The replica maximum each credential limit is divided by.
    pub(super) max_replicas: u32,
    pub(super) envelope_lifetime_ms: u64,
    public_key_bundle_file: PathBuf,
    signing_key_file: PathBuf,
    active_kid: String,
    /// How long `/ready` reports draining before the listener closes.
    readiness_propagation_ms: u64,
    /// Bound on in-flight requests finishing after the listener closed;
    /// requests still running then are cancelled.
    drain_timeout_ms: u64,
    max_request_bytes: usize,
    max_in_flight: usize,
    upstream_timeout_ms: u64,
    /// Bound on per-credential edge rate-limit buckets.
    max_buckets: usize,
}

fn invalid(detail: impl std::fmt::Display) -> String {
    format!("gateway_config_invalid: {detail}")
}

/// A validated gateway configuration with its signer; nothing contacted.
pub(super) struct Settings {
    pub(super) file: GatewayFile,
    pub(super) signer: Signer,
}

pub(super) fn read_config(path: &Path) -> Result<Settings, String> {
    let bytes = read_bounded(path, MAX_CONFIG_BYTES, "gateway config").map_err(invalid)?;
    let file: GatewayFile = serde_json::from_slice(&bytes).map_err(invalid)?;
    file.control_state.validate().map_err(invalid)?;
    for (name, value) in [
        ("s_ctl_ms", file.s_ctl_ms),
        ("poll_interval_ms", file.poll_interval_ms),
        ("envelope_lifetime_ms", file.envelope_lifetime_ms),
        ("readiness_propagation_ms", file.readiness_propagation_ms),
        ("drain_timeout_ms", file.drain_timeout_ms),
        ("upstream_timeout_ms", file.upstream_timeout_ms),
    ] {
        if value > MAX_MS {
            return Err(invalid(format!("{name} must be at most {MAX_MS}")));
        }
        if value == 0 && name != "readiness_propagation_ms" {
            return Err(invalid(format!("{name} must be positive")));
        }
    }
    if file.poll_interval_ms >= file.s_ctl_ms {
        return Err(invalid("poll_interval_ms must be below s_ctl_ms"));
    }
    if file.max_replicas == 0 {
        return Err(invalid("max_replicas must be positive"));
    }
    if !(1..=MAX_JSON_BYTES).contains(&file.max_request_bytes) {
        return Err(invalid(format!(
            "max_request_bytes must be 1..={MAX_JSON_BYTES}"
        )));
    }
    if !(1..=MAX_IN_FLIGHT).contains(&file.max_in_flight) {
        return Err(invalid(format!(
            "max_in_flight must be 1..={MAX_IN_FLIGHT}"
        )));
    }
    if !(1..=MAX_BUCKETS).contains(&file.max_buckets) {
        return Err(invalid(format!("max_buckets must be 1..={MAX_BUCKETS}")));
    }
    if !file.allow_remote && !file.bind.ip().is_loopback() {
        return Err(invalid(
            "bind must be a loopback address unless allow_remote is true",
        ));
    }
    let bundle = read_bounded(
        &file.public_key_bundle_file,
        MAX_CONFIG_BYTES,
        "public key bundle",
    )
    .map_err(invalid)?;
    let mut text = read_bounded(&file.signing_key_file, MAX_SEED_FILE_BYTES, "signing key")
        .map_err(invalid)?;
    let seed = std::str::from_utf8(&text)
        .map_err(|_| "invalid signing material")
        .and_then(principal_envelope::decode_seed);
    text.fill(0);
    let mut seed = seed.map_err(invalid)?;
    let signer = Signer::from_gateway_bundle(GatewayKeyBundle::new(
        &bundle,
        &file.active_kid,
        Some(&seed[..]),
    ));
    seed.fill(0);
    let signer = signer.map_err(invalid)?;
    Ok(Settings { file, signer })
}

/// Runs a gateway until `shutdown` resolves, then drains: `/ready` answers
/// 503 `draining` for `readiness_propagation_ms` while the listener still
/// serves, the listener closes, in-flight requests get `drain_timeout_ms`
/// and are then cancelled (at most `SHUTDOWN_SETTLE` more to flush responses),
/// and the control-state poller stops (at most `SHUTDOWN_SETTLE`).
pub async fn run_gateway(
    args: Vec<OsString>,
    shutdown: impl Future<Output = std::io::Result<()>> + Send,
) -> Result<(), String> {
    let path = match args.as_slice() {
        [flag, path] if flag == "--config" => PathBuf::from(path),
        _ => return Err(format!("invalid_arguments: usage: {GATEWAY_USAGE}")),
    };
    let Settings { file, signer } = read_config(&path)?;
    let upstream = Duration::from_millis(file.upstream_timeout_ms);
    // No client-wide `timeout`: reqwest would count it from the connect to
    // the end of the body and cut a long streamed response short. A connect
    // failure still proves nothing was sent; the other deadlines are applied
    // per phase in `forward`.
    let client = reqwest::Client::builder()
        .connect_timeout(upstream)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .pool_idle_timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| format!("gateway_start_failed: upstream client: {error}"))?;
    let (source, s_ctl, poll) = (
        file.control_state.clone(),
        file.s_ctl_ms,
        file.poll_interval_ms,
    );
    // One confirmed read before the listener binds; then poll.
    let feed = tokio::task::spawn_blocking(move || -> Result<Arc<Feed>, String> {
        let objects = source
            .connect()
            .map_err(|error| format!("{}: {}", error.code, error.message))?;
        let feed = Arc::new(Feed::new(objects, s_ctl, poll).map_err(invalid)?);
        feed.poll_once()
            .map_err(|error| format!("{}: {}", error.code, error.message))?;
        feed.start(|_| {})
            .map_err(|error| format!("gateway_start_failed: poller: {error}"))?;
        Ok(feed)
    })
    .await
    .map_err(|error| format!("gateway_start_failed: {error}"))??;
    let listener = match tokio::net::TcpListener::bind(file.bind).await {
        Ok(listener) => listener,
        Err(error) => {
            stop_feed(&feed).await;
            return Err(format!("gateway_start_failed: bind: {error}"));
        }
    };
    let address = listener
        .local_addr()
        .map_err(|error| format!("gateway_start_failed: {error}"))?;
    let keys = signer.metadata().clone();
    let gateway = Arc::new(Gateway {
        feed: feed.clone(),
        signer,
        client,
        upstream_timeout: upstream,
        draining: AtomicBool::new(false),
        cancel: tokio::sync::watch::Sender::new(false),
        in_flight: Arc::new(Semaphore::new(file.max_in_flight)),
        buckets: Buckets::new(file.max_buckets),
        max_request_bytes: file.max_request_bytes,
        envelope_lifetime_ms: file.envelope_lifetime_ms,
        max_replicas: file.max_replicas,
    });
    let router = Router::new().fallback(handle).with_state(gateway.clone());
    let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
    let server = axum::serve(listener, router).with_graceful_shutdown(async move {
        let _ = stop_rx.wait_for(|stop| *stop).await;
    });
    let mut server = tokio::spawn(server.into_future());
    log_stderr(
        "info",
        "gateway",
        json!({"event":"gateway_started","address":address.to_string(),
            "generation":feed.current().map(|snapshot| snapshot.generation()).ok(),
            "key_generation":keys.generation,"active_kid":keys.active_kid,"kids":keys.kids,
            "max_replicas":file.max_replicas,
            "write_capacity":"gateway replicas add no write capacity and are not writer HA: \
                each tenant has one writer"})
        .to_string(),
    );
    println!("LISTENING http://{address}");
    use std::io::Write as _;
    let _ = std::io::stdout().flush();

    let mut outcome = Ok(());
    tokio::select! {
        signal = shutdown => {
            if let Err(error) = signal {
                outcome = Err(format!("signal handling failed: {error}"));
            }
            gateway.draining.store(true, Ordering::SeqCst);
            log_stderr(
                "info",
                "gateway",
                json!({"event":"draining","propagation_ms":file.readiness_propagation_ms})
                    .to_string(),
            );
            tokio::time::sleep(Duration::from_millis(file.readiness_propagation_ms)).await;
            let _ = stop_tx.send(true);
            match tokio::time::timeout(Duration::from_millis(file.drain_timeout_ms), &mut server)
                .await
            {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(error))) => outcome = Err(format!("gateway_serve_failed: {error}")),
                Ok(Err(error)) => outcome = Err(format!("gateway_serve_failed: {error}")),
                Err(_) => {
                    let settled = cancel_and_settle(
                        &mut server,
                        &gateway.cancel,
                        SHUTDOWN_SETTLE,
                    ).await;
                    match &settled {
                        Some(Ok(Ok(()))) | None => {}
                        Some(Ok(Err(error))) => outcome = Err(format!("gateway_serve_failed: {error}")),
                        Some(Err(error)) => outcome = Err(format!("gateway_serve_failed: {error}")),
                    }
                    log_stderr(
                        "warn",
                        "gateway",
                        json!({"event":"drain_timeout","drain_timeout_ms":file.drain_timeout_ms,
                            "settled":matches!(settled, Some(Ok(Ok(()))))})
                        .to_string(),
                    );
                }
            }
            gateway.cancel.send_replace(true);
        }
        served = &mut server => {
            outcome = Err(match served {
                Ok(Ok(())) => "gateway_serve_failed: the listener stopped".to_owned(),
                Ok(Err(error)) => format!("gateway_serve_failed: {error}"),
                Err(error) => format!("gateway_serve_failed: {error}"),
            });
        }
    }
    stop_feed(&feed).await;
    log_stderr(
        "info",
        "gateway",
        json!({"event":"server_shutdown"}).to_string(),
    );
    outcome
}

async fn cancel_and_settle(
    server: &mut tokio::task::JoinHandle<std::io::Result<()>>,
    cancel: &tokio::sync::watch::Sender<bool>,
    settle: Duration,
) -> Option<Result<std::io::Result<()>, tokio::task::JoinError>> {
    cancel.send_replace(true);
    // Released handler permits do not prove that HTTP responses were flushed.
    match tokio::time::timeout(settle, &mut *server).await {
        Ok(result) => Some(result),
        Err(_) => {
            server.abort();
            None
        }
    }
}

/// Stops the poll thread, waiting at most `settle` for a read in progress;
/// past that the thread is left to end after its read.
async fn stop_feed_within(feed: &Feed, settle: Duration) {
    let Some(thread) = feed.begin_stop() else {
        return;
    };
    let deadline = Instant::now() + settle;
    while !thread.is_finished() {
        if Instant::now() >= deadline {
            log_stderr(
                "warn",
                "gateway",
                json!({"event":"poller_stop_timeout","settle_ms":settle.as_millis() as u64})
                    .to_string(),
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let _ = thread.join();
}

async fn stop_feed(feed: &Feed) {
    stop_feed_within(feed, SHUTDOWN_SETTLE).await;
}

/// Resolves once `cancel` is set; never if its sender is gone.
async fn cancelled(cancel: &mut tokio::sync::watch::Receiver<bool>) {
    if cancel.wait_for(|cancelled| *cancelled).await.is_err() {
        std::future::pending::<()>().await;
    }
}

struct Gateway {
    feed: Arc<Feed>,
    signer: Signer,
    client: reqwest::Client,
    /// Bounds connecting and waiting for the response head, then each wait
    /// for the next piece of a request or streamed response body.
    upstream_timeout: Duration,
    draining: AtomicBool,
    /// Set when the drain deadline passes: every request still running ends.
    cancel: tokio::sync::watch::Sender<bool>,
    in_flight: Arc<Semaphore>,
    buckets: Buckets,
    max_request_bytes: usize,
    envelope_lifetime_ms: u64,
    max_replicas: u32,
}

async fn handle(State(gateway): State<Arc<Gateway>>, request: Request) -> Response {
    match gateway.forward(request).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "route not found")
}

fn unauthorized() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "a tenant API credential (Bearer vlk_...) is required",
    )
}

impl Gateway {
    /// Checks a backend's epoch stamp, in headers already filtered by
    /// `response_headers`, against the higher of the dispatched epoch and the
    /// tenant's current record. A route without an epoch always passes.
    fn check_epoch(
        &self,
        tenant: &str,
        target: &gateway::Target,
        headers: &HeaderMap,
    ) -> Result<(), gateway::EpochRefusal> {
        let Some(dispatched) = target.epoch else {
            return Ok(());
        };
        let current = self
            .feed
            .current()
            .map_err(|_| gateway::EpochRefusal::Stale)?;
        let record = current
            .record_for(tenant)
            .filter(|record| record.lineage == target.lineage)
            .ok_or(gateway::EpochRefusal::Stale)?;
        let expected = dispatched.max(record.epoch.unwrap_or(dispatched));
        gateway::check_epoch(Some(expected), gateway::stamped_epoch(headers))
    }

    fn ready(&self) -> Result<Value, ApiError> {
        if self.draining.load(Ordering::SeqCst) {
            return Err(gateway_trust::draining());
        }
        let snapshot = self.feed.current()?;
        Ok(json!({"status":"ready","generation":snapshot.generation()}))
    }

    async fn forward(self: Arc<Self>, request: Request) -> Result<Response, ApiError> {
        let (parts, body) = request.into_parts();
        let path = parts.uri.path();
        if path == "/health" || path == "/ready" {
            if !matches!(parts.method, Method::GET | Method::HEAD) {
                return Err(ApiError::new(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "method_not_allowed",
                    "method not allowed",
                ));
            }
            let body = if path == "/health" {
                json!({"status":"ok"})
            } else {
                self.ready()?
            };
            return Ok(Json(body).into_response());
        }
        let permit = self.in_flight.clone().try_acquire_owned().map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "overloaded",
                "too many requests in flight; retry later",
            )
        })?;
        if !safe_path(path) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_path",
                "the path has dot, encoded-separator or backslash segments",
            ));
        }
        let classification =
            gateway::classify(&parts.method, path).map_err(|refusal| match refusal {
                gateway::Refusal::UnknownRoute => not_found(),
                gateway::Refusal::MethodNotAllowed => ApiError::new(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "method_not_allowed",
                    "method not allowed",
                ),
            })?;
        // Login, sessions and the dashboard are not served by a gateway yet.
        if classification.public {
            return Err(not_found());
        }
        let snapshot = self.feed.current()?;
        let token = tenant_bearer(&parts.headers).ok_or_else(unauthorized)?;
        let now = admin::now_ms() as u64;
        let grant = snapshot.authenticate(token, now)?;
        let tenant = grant
            .tenant_id
            .as_deref()
            .ok_or_else(|| RouteRefusal::NoTenant.error())?;
        let record = snapshot
            .record_for(tenant)
            .ok_or_else(|| RouteRefusal::NoRecord.error())?;
        let template = classification.template;
        let legacy = !template.contains("{collection}") && !template.starts_with("/v1/imports");
        super::authorize_scoped(
            &grant,
            Some(record.lineage),
            classification.need,
            legacy,
            false,
        )?;
        if let Some(limit) = grant.requests_per_second {
            let share = gateway::replica_share(limit, self.max_replicas).map_err(|_| {
                ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "rate_limit_share_below_one",
                    "the credential's request rate divided by the gateway replica maximum is below one request a second",
                )
            })?;
            self.buckets.take(&grant.id, share, Instant::now())?;
        }
        let mut target = gateway::route_target(classification, &grant, &snapshot)
            .map_err(RouteRefusal::error)?;
        let declared = parts
            .headers
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        if declared.is_some_and(|length| length > self.max_request_bytes as u64) {
            return Err(too_large(self.max_request_bytes));
        }
        let mut cancel = self.cancel.subscribe();
        let body = tokio::select! {
            biased;
            () = cancelled(&mut cancel) => {
                return Err(drain_cancelled("request_body", target.operation, false));
            }
            body = receive_body(body, self.max_request_bytes, self.upstream_timeout) => body?,
        };
        let upstream = loop {
            if *cancel.borrow() {
                return Err(drain_cancelled("dispatch", target.operation, false));
            }
            let url = upstream_url(&target.endpoint, &parts.uri)?;
            let envelope = gateway::mint(
                &self.signer,
                &target,
                &grant,
                snapshot.generation(),
                admin::now_ms() as u64,
                self.envelope_lifetime_ms,
            )
            .map_err(super::internal)?;
            let mut headers = request_headers(&parts.headers);
            headers.insert(
                HeaderName::from_static(principal_envelope::HEADER),
                envelope.parse().map_err(super::internal)?,
            );
            let sent = dispatch(
                self.client
                    .request(parts.method.clone(), url)
                    .headers(headers)
                    .body(body.clone())
                    .send(),
                self.upstream_timeout,
                &mut cancel,
                target.operation,
            )
            .await?;
            let (delivery, failure) = match sent {
                Ok(Ok(upstream)) => {
                    if target.fallback.is_none()
                        || (eligible_reader(&upstream, &target)
                            && self
                                .check_epoch(tenant, &target, &response_headers(upstream.headers()))
                                .is_ok())
                    {
                        break upstream;
                    }
                    (
                        gateway::Delivery::ResponseLost,
                        ApiError::new(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "reader_ineligible",
                            "the reader is unavailable or has a different lineage, binding generation or epoch",
                        ),
                    )
                }
                Ok(Err(error)) => (
                    if error.is_connect() {
                        gateway::Delivery::NotSent
                    } else {
                        gateway::Delivery::Sent
                    },
                    upstream_failure(error.is_connect(), error.is_timeout(), target.operation),
                ),
                // Whether the request was sent before the head timeout is unknown.
                Err(_) => (
                    gateway::Delivery::Sent,
                    upstream_failure(false, true, target.operation),
                ),
            };
            if delivery.may_retry(target.operation)
                && let Some(writer) = target.fallback.take()
            {
                log_stderr(
                    "warn",
                    "gateway",
                    json!({"event":"gateway_reader_fallback","code":failure.code}).to_string(),
                );
                target.endpoint = writer;
            } else {
                return Err(failure);
            }
        };
        let status = upstream.status();
        let headers = response_headers(upstream.headers());
        if let Err(refusal) = self.check_epoch(tenant, &target, &headers) {
            log_stderr(
                "warn",
                "gateway",
                json!({"event":"gateway_upstream_failed","code":"backend_epoch_stale",
                    "operation":operation_name(target.operation)})
                .to_string(),
            );
            return Err(refusal.error(target.operation));
        }
        let (sender, receiver) = tokio::sync::mpsc::channel::<Piece>(4);
        let operation = target.operation;
        tokio::spawn(pump(
            upstream,
            sender,
            self.upstream_timeout,
            cancel,
            permit,
            operation,
        ));
        let mut response = Response::new(Body::from_stream(Relay::new(receiver)));
        *response.status_mut() = status;
        *response.headers_mut() = headers;
        Ok(response)
    }
}

async fn dispatch<F: Future>(
    send: F,
    timeout: Duration,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
    operation: Operation,
) -> Result<Result<F::Output, tokio::time::error::Elapsed>, ApiError> {
    let started = AtomicBool::new(false);
    tokio::pin!(send);
    let send = std::future::poll_fn(|cx| {
        started.store(true, Ordering::Relaxed);
        send.as_mut().poll(cx)
    });
    tokio::select! {
        biased;
        // Nothing is retried after a cancel, not even a read.
        () = cancelled(cancel) => Err(drain_cancelled("dispatch", operation, started.load(Ordering::Relaxed))),
        sent = tokio::time::timeout(timeout, send) => Ok(sent),
    }
}

fn eligible_reader(upstream: &reqwest::Response, target: &gateway::Target) -> bool {
    let headers = upstream.headers();
    upstream.status() != StatusCode::SERVICE_UNAVAILABLE
        && headers
            .get("x-vlite-lineage")
            .is_some_and(|value| value.as_bytes() == format!("{:016x}", target.lineage).as_bytes())
        && headers
            .get("x-vlite-binding-generation")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            == Some(target.binding_generation)
}

/// Copies the upstream body to the client. Each wait for the next piece, and
/// each wait for the client to take one, gets `idle`, so a long stream is not
/// cut off while data keeps moving. A stall, an error, a client that stops
/// reading or a drain cancel ends the copy without `Piece::End`, which the
/// client sees as a truncated body. `_permit` is held until the copy ends.
async fn pump(
    mut upstream: reqwest::Response,
    sender: tokio::sync::mpsc::Sender<Piece>,
    idle: Duration,
    mut cancel: tokio::sync::watch::Receiver<bool>,
    _permit: tokio::sync::OwnedSemaphorePermit,
    operation: Operation,
) {
    loop {
        let next = tokio::select! {
            biased;
            () = cancelled(&mut cancel) => {
                log_drain_cancelled("response_body", operation);
                return;
            }
            next = tokio::time::timeout(idle, upstream.chunk()) => next,
        };
        let piece = match next {
            Ok(Ok(Some(chunk))) => Piece::Data(chunk),
            Ok(Ok(None)) => Piece::End,
            Ok(Err(_)) | Err(_) => {
                log_stderr(
                    "warn",
                    "gateway",
                    json!({"event":"gateway_upstream_failed","code":"response_cut_short",
                        "operation":operation_name(operation)})
                    .to_string(),
                );
                return;
            }
        };
        let end = matches!(piece, Piece::End);
        let delivered = tokio::select! {
            biased;
            () = cancelled(&mut cancel) => {
                log_drain_cancelled("response_body", operation);
                return;
            }
            delivered = tokio::time::timeout(idle, sender.send(piece)) => delivered,
        };
        match delivered {
            Ok(Ok(())) if !end => {}
            Ok(_) => return,
            Err(_) => {
                log_stderr(
                    "warn",
                    "gateway",
                    json!({"event":"gateway_client_stalled","operation":operation_name(operation)})
                        .to_string(),
                );
                return;
            }
        }
    }
}

/// One piece of a relayed response body; only `End` finishes it cleanly.
enum Piece {
    Data(Bytes),
    End,
}

/// The client's view of a relayed body: if the pump stops without `End`,
/// the body ends with an error, so the response is never mistaken for a
/// complete one.
struct Relay {
    receiver: tokio::sync::mpsc::Receiver<Piece>,
    finished: bool,
}

impl Relay {
    fn new(receiver: tokio::sync::mpsc::Receiver<Piece>) -> Self {
        Self {
            receiver,
            finished: false,
        }
    }
}

impl tokio_stream::Stream for Relay {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        if self.finished {
            return Poll::Ready(None);
        }
        match self.receiver.poll_recv(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Piece::Data(bytes))) => Poll::Ready(Some(Ok(bytes))),
            Poll::Ready(Some(Piece::End)) => {
                self.finished = true;
                Poll::Ready(None)
            }
            Poll::Ready(None) => {
                self.finished = true;
                Poll::Ready(Some(Err(std::io::Error::other(
                    "backend response cut short",
                ))))
            }
        }
    }
}

fn too_large(limit: usize) -> ApiError {
    ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "payload_too_large",
        format!("the request body must be complete and at most {limit} bytes"),
    )
}

/// Reads a request body of at most `limit` bytes, giving each wait for the
/// next piece `idle`, so a client that stops sending cannot hold an
/// in-flight slot.
async fn receive_body(body: Body, limit: usize, idle: Duration) -> Result<Bytes, ApiError> {
    use tokio_stream::StreamExt;
    let mut stream = body.into_data_stream();
    let mut received = Vec::new();
    loop {
        match tokio::time::timeout(idle, stream.next()).await {
            Ok(None) => return Ok(Bytes::from(received)),
            Ok(Some(Ok(piece))) => {
                if piece.len() > limit - received.len() {
                    return Err(too_large(limit));
                }
                received.extend_from_slice(&piece);
            }
            Ok(Some(Err(_))) => return Err(too_large(limit)),
            Err(_) => {
                log_stderr(
                    "warn",
                    "gateway",
                    json!({"event":"request_body_timeout"}).to_string(),
                );
                return Err(ApiError::new(
                    StatusCode::REQUEST_TIMEOUT,
                    "request_body_timeout",
                    "the request body stalled; nothing was sent",
                ));
            }
        }
    }
}

fn log_drain_cancelled(phase: &str, operation: Operation) {
    log_stderr(
        "warn",
        "gateway",
        json!({"event":"drain_cancelled","phase":phase,"operation":operation_name(operation)})
            .to_string(),
    );
}

/// The client error for a request cancelled at the drain deadline: a write
/// that may have been sent is ambiguous, anything else was not completed.
fn drain_cancelled(phase: &str, operation: Operation, sent: bool) -> ApiError {
    log_drain_cancelled(phase, operation);
    if sent && matches!(operation, Operation::Write) {
        gateway::ambiguous_write()
    } else {
        gateway_trust::draining()
    }
}

fn operation_name(operation: Operation) -> &'static str {
    match operation {
        Operation::Read => "read",
        Operation::Write => "write",
    }
}

/// The client error for a failed forward. Only a connect failure proves
/// nothing was sent; otherwise a write may have been applied.
fn upstream_failure(connect: bool, timed_out: bool, operation: Operation) -> ApiError {
    let failure = if connect {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "backend_unavailable",
            "the tenant's backend could not be reached; nothing was sent",
        )
    } else if matches!(operation, Operation::Write) {
        gateway::ambiguous_write()
    } else if timed_out {
        ApiError::new(
            StatusCode::GATEWAY_TIMEOUT,
            "backend_timeout",
            "the tenant's backend did not answer in time",
        )
    } else {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "backend_failed",
            "the tenant's backend connection failed",
        )
    };
    log_stderr(
        "warn",
        "gateway",
        json!({"event":"gateway_upstream_failed","code":failure.code,
            "operation":operation_name(operation)})
        .to_string(),
    );
    failure
}

/// The single bearer credential, if it is a tenant `vlk_` credential.
fn tenant_bearer(headers: &HeaderMap) -> Option<&str> {
    super::bearer(headers).filter(|token| token.starts_with("vlk_"))
}

/// Absolute, with no `.`/`..` segment, backslash or percent-encoded dot,
/// slash or backslash: nothing a backend or URL parser could resolve to a
/// different path than the one classified.
fn safe_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    path.starts_with('/')
        && !path.contains('\\')
        && !["%2e", "%2f", "%5c"]
            .iter()
            .any(|encoded| lower.contains(encoded))
        && path
            .split('/')
            .all(|segment| !matches!(segment, "." | ".."))
}

/// The record's origin with the caller's path and query, refusing a record
/// endpoint that carries credentials, a path, query or fragment, and any
/// result whose path or query differs from the caller's.
fn upstream_url(endpoint: &str, uri: &axum::http::Uri) -> Result<reqwest::Url, ApiError> {
    let misrouted = || {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "tenant_route_invalid",
            "the tenant's directory record has an unusable writer endpoint",
        )
    };
    let base = reqwest::Url::parse(endpoint).map_err(|_| misrouted())?;
    if !matches!(base.scheme(), "http" | "https")
        || base.host_str().is_none()
        || !base.username().is_empty()
        || base.password().is_some()
        || !matches!(base.path(), "" | "/")
        || base.query().is_some()
        || base.fragment().is_some()
    {
        return Err(misrouted());
    }
    let tail = uri.path_and_query().map_or("/", |tail| tail.as_str());
    let url = reqwest::Url::parse(&format!("{}{tail}", base.origin().ascii_serialization()))
        .map_err(|_| misrouted())?;
    if url.path() != uri.path() || url.query() != uri.query() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_path",
            "the path or query does not survive URL normalization unchanged",
        ));
    }
    Ok(url)
}

/// Headers that are never forwarded either way: hop-by-hop (RFC 9110
/// section 7.6.1) and proxy headers.
fn hop_by_hop(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
    ) || name.starts_with("proxy-")
}

/// Header names a `Connection` header nominates as hop-by-hop.
fn nominated(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|name| name.trim().to_ascii_lowercase())
        .filter(|name| !name.is_empty())
        .collect()
}

/// The caller's headers that may reach a backend: no credential, cookie,
/// principal, epoch, forwarding/proxy, host, framing or hop-by-hop header.
fn request_headers(incoming: &HeaderMap) -> HeaderMap {
    let nominated = nominated(incoming);
    let mut headers = HeaderMap::new();
    for (name, value) in incoming {
        let name_text = name.as_str();
        let dropped = hop_by_hop(name_text)
            || matches!(
                name_text,
                "authorization"
                    | "cookie"
                    | "forwarded"
                    | "x-real-ip"
                    | "host"
                    | "expect"
                    | "content-length"
                    | "x-vlite-epoch"
            )
            || name_text == principal_envelope::HEADER
            || name_text.starts_with("x-forwarded-")
            || nominated.iter().any(|nominated| nominated == name_text);
        if !dropped {
            headers.append(name.clone(), value.clone());
        }
    }
    headers
}

/// The backend's response headers without hop-by-hop or `Set-Cookie`.
fn response_headers(incoming: &HeaderMap) -> HeaderMap {
    let nominated = nominated(incoming);
    let mut headers = HeaderMap::new();
    for (name, value) in incoming {
        let name_text = name.as_str();
        if !(hop_by_hop(name_text)
            || name_text == "set-cookie"
            || nominated.iter().any(|nominated| nominated == name_text))
        {
            headers.append(name.clone(), value.clone());
        }
    }
    headers
}

/// Per-credential edge buckets: capacity and refill rate are both this
/// replica's share. The backend's own bucket and quotas still apply.
struct Buckets {
    max: usize,
    entries: Mutex<HashMap<String, Bucket>>,
}

#[derive(Clone, Copy)]
struct Bucket {
    tokens: f64,
    share: f64,
    at: Instant,
}

impl Bucket {
    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.tokens = elapsed.mul_add(self.share, self.tokens).min(self.share);
        self.at = now;
    }
}

impl Buckets {
    fn new(max: usize) -> Self {
        Self {
            max,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Takes one request from credential `id`'s bucket. At `max` buckets,
    /// full ones (indistinguishable from new) are evicted; if none is full,
    /// the request is refused rather than the bound exceeded.
    fn take(&self, id: &str, share: f64, now: Instant) -> Result<(), ApiError> {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        if !entries.contains_key(id) && entries.len() >= self.max {
            entries.retain(|_, bucket| {
                bucket.refill(now);
                bucket.tokens < bucket.share
            });
            if entries.len() >= self.max {
                return Err(ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "rate_limit_capacity",
                    "too many rate-limited credentials are active on this gateway; retry later",
                ));
            }
        }
        let bucket = entries.entry(id.to_owned()).or_insert(Bucket {
            tokens: share,
            share,
            at: now,
        });
        // A changed limit applies from the next snapshot on.
        bucket.share = share;
        bucket.refill(now);
        if bucket.tokens < 1.0 {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "credential request rate exceeded",
            ));
        }
        bucket.tokens -= 1.0;
        Ok(())
    }
}

/// Command-line flags: each allowed flag at most once with one value.
/// Errors never echo an argument (a mistyped value may be a secret).
pub(super) fn flags(
    args: &[OsString],
    allowed: &[&'static str],
    usage: &str,
) -> Result<BTreeMap<&'static str, OsString>, String> {
    let mut values = BTreeMap::new();
    let mut rest = args.iter().enumerate();
    while let Some((position, flag)) = rest.next() {
        let name = flag
            .to_str()
            .and_then(|flag| allowed.iter().copied().find(|allowed| *allowed == flag))
            .ok_or_else(|| {
                format!(
                    "invalid_arguments: argument {} is not an option of this command; usage: {usage}",
                    position + 1
                )
            })?;
        let (_, value) = rest
            .next()
            .ok_or_else(|| format!("invalid_arguments: {name} needs a value; usage: {usage}"))?;
        if values.insert(name, value.clone()).is_some() {
            return Err(format!(
                "invalid_arguments: {name} is given more than once; usage: {usage}"
            ));
        }
    }
    Ok(values)
}

pub(super) fn utf8(name: &str, value: OsString) -> Result<String, String> {
    value
        .into_string()
        .map_err(|_| format!("invalid_arguments: {name} must be valid UTF-8"))
}

/// Writes a new file (never replacing one); owner-only on Unix.
fn write_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// `vlite-server gateway-keygen`: a new Ed25519 signing seed file and its
/// public bundle. Neither key is printed; existing files are never replaced.
pub fn gateway_keygen(args: &[OsString]) -> Result<Value, String> {
    let mut values = flags(
        args,
        &[
            "--kid",
            "--generation",
            "--signing-key-out",
            "--public-bundle-out",
        ],
        KEYGEN_USAGE,
    )?;
    let mut take = |name: &str| {
        values
            .remove(name)
            .ok_or_else(|| format!("invalid_arguments: {name} is required; usage: {KEYGEN_USAGE}"))
    };
    let kid = utf8("--kid", take("--kid")?)?;
    let generation = utf8("--generation", take("--generation")?)?
        .parse::<u64>()
        .map_err(|_| "invalid_arguments: --generation must be a positive integer".to_owned())?;
    let key_out = PathBuf::from(take("--signing-key-out")?);
    let bundle_out = PathBuf::from(take("--public-bundle-out")?);
    if key_out == bundle_out {
        return Err("invalid_arguments: the two output files must differ".to_owned());
    }
    for (what, path) in [("signing key", &key_out), ("public bundle", &bundle_out)] {
        if std::fs::symlink_metadata(path).is_ok() {
            return Err(format!(
                "output_exists: the {what} output already exists; nothing was written"
            ));
        }
    }
    let (mut seed, bundle) = principal_envelope::generate(&kid, generation)
        .map_err(|error| format!("invalid_arguments: {error}"))?;
    seed.push('\n');
    let written = write_new(&key_out, seed.as_bytes());
    // Best-effort scrub of the encoded seed's buffer.
    seed.into_bytes().fill(0);
    written.map_err(|error| format!("write_failed: signing key output: {}", error.kind()))?;
    let bundle = serde_json::to_vec_pretty(&bundle).map_err(|error| error.to_string())?;
    if let Err(error) = write_new(&bundle_out, &bundle) {
        let _ = std::fs::remove_file(&key_out);
        return Err(format!(
            "write_failed: public bundle output: {}",
            error.kind()
        ));
    }
    Ok(json!({"kid":kid,"generation":generation,
        "signing_key_file":key_out.display().to_string(),
        "public_bundle_file":bundle_out.display().to_string()}))
}

enum Mutation {
    Revoke(String),
    Suspend(String, bool),
    Reimport(u64),
}

/// Name of the admin-directory file `control-state` locks, so two commands
/// on one admin directory never interleave.
pub(super) const CONTROL_STATE_LOCK: &str = "control-state.lock";

/// `vlite-server control-state publish|update`: the shared control state's
/// command line, through its compare-and-swap. `publish` creates generation 1
/// from the admin directory's credentials and tenants (plus an optional
/// tenant directory) and refuses an existing object. `update` publishes the
/// next generation with exactly one change:
/// - `--revoke-credential`, `--suspend-tenant` or `--resume-tenant` applies
///   the change to the admin directory (for its audit log) and then to the
///   published state, never republishing the rest of the admin directory, so
///   concurrent or stale admin directories cannot undo each other's changes
///   (security review, finding 1). The local change is applied first, so a
///   failed publish leaves it applied and unpublished; repeating the command
///   is safe (the change is idempotent) and publishes it.
/// - `--reimport-if-generation <n>` republishes the admin directory's
///   credentials and tenants (to publish newly issued credentials or new
///   tenants), only while the published generation is still `n`; restrictions
///   already published stay.
/// - `--directory-file` alone replaces only the tenant routes; with one of the
///   above it replaces them too.
///
/// One command at a time runs per admin directory (`control-state.lock`,
/// else `admin_dir_busy`). The store is blocking: call off the async runtime.
pub fn control_state_command(args: &[OsString]) -> Result<Value, String> {
    let usage = |detail: &str| format!("invalid_arguments: {detail}; usage: {CONTROL_USAGE}");
    let (verb, rest) = args
        .split_first()
        .ok_or_else(|| usage("a verb is required"))?;
    let publish = match verb.to_str() {
        Some("publish") => true,
        Some("update") => false,
        _ => return Err(usage("the verb must be publish or update")),
    };
    let mut allowed = vec![
        "--target-file",
        "--object",
        "--shared-key-file",
        "--admin-dir",
        "--directory-file",
    ];
    if !publish {
        allowed.extend([
            "--revoke-credential",
            "--suspend-tenant",
            "--resume-tenant",
            "--reimport-if-generation",
        ]);
    }
    let mut values = flags(rest, &allowed, CONTROL_USAGE)?;
    let mut required = |name: &str| {
        values
            .remove(name)
            .ok_or_else(|| usage(&format!("{name} is required")))
    };
    let source = Source {
        target_file: PathBuf::from(required("--target-file")?),
        object: utf8("--object", required("--object")?)?,
        shared_key_file: None,
    };
    let admin_dir = PathBuf::from(required("--admin-dir")?);
    let source = Source {
        shared_key_file: values.remove("--shared-key-file").map(PathBuf::from),
        ..source
    };
    source
        .validate()
        .map_err(|error| format!("invalid_arguments: {error}"))?;
    let mut mutations = Vec::new();
    for (flag, make) in [
        (
            "--revoke-credential",
            (|id| Mutation::Revoke(id)) as fn(String) -> Mutation,
        ),
        ("--suspend-tenant", |id| Mutation::Suspend(id, true)),
        ("--resume-tenant", |id| Mutation::Suspend(id, false)),
    ] {
        if let Some(value) = values.remove(flag) {
            mutations.push(make(utf8(flag, value)?));
        }
    }
    if let Some(value) = values.remove("--reimport-if-generation") {
        let generation = utf8("--reimport-if-generation", value)?
            .parse::<u64>()
            .ok()
            .filter(|generation| *generation > 0)
            .ok_or_else(|| {
                usage("--reimport-if-generation must be a published generation (1 or more)")
            })?;
        mutations.push(Mutation::Reimport(generation));
    }
    if mutations.len() > 1 {
        return Err(usage(
            "give at most one of --revoke-credential, --suspend-tenant, --resume-tenant, --reimport-if-generation",
        ));
    }
    let api = |error: ApiError| format!("{}: {}", error.code, error.message);
    let directory = values
        .remove("--directory-file")
        .map(|path| control_state::read_directory_file(Path::new(&path)))
        .transpose()
        .map_err(api)?;
    if !publish && mutations.is_empty() && directory.is_none() {
        return Err(usage(
            "update needs one of --revoke-credential, --suspend-tenant, --resume-tenant, --reimport-if-generation or --directory-file",
        ));
    }
    let directory_admin = std::fs::canonicalize(&admin_dir)
        .ok()
        .filter(|path| path.is_dir())
        .ok_or_else(|| usage("--admin-dir must be an existing admin directory"))?;
    let io = |what: &str| {
        let what = what.to_owned();
        move |error: std::io::Error| format!("admin_dir_invalid: {what}: {}", error.kind())
    };
    // Held until the command returns; the OS releases it if the process dies.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(directory_admin.join(CONTROL_STATE_LOCK))
        .map_err(io("control-state lock"))?;
    lock.try_lock().map_err(|_| {
        "admin_dir_busy: another control-state command is running on this admin directory"
            .to_owned()
    })?;
    let log = audit::Log::open(&directory_admin).map_err(io("audit log"))?;
    let credentials = credentials::Store::load(&directory_admin).map_err(io("credentials"))?;
    let tenants = tenants::Store::load(&directory_admin).map_err(io("tenants"))?;
    let objects = source
        .connect()
        .map_err(|error| format!("{}: {}", error.code, error.message))?;
    let now = admin::now_ms() as u64;
    let snapshot = if publish {
        let snapshot = Snapshot::import(&credentials, Some(&tenants)).map_err(api)?;
        let snapshot = snapshot.with_directory(directory.unwrap_or_default());
        objects.create(snapshot, now).map_err(api)?
    } else {
        let (edit, expected) = match mutations.pop() {
            Some(Mutation::Revoke(id)) => {
                credentials
                    .revoke_id(&log, &id, now, Some("control-state-cli"))
                    .map_err(api)?;
                (control_state::Edit::Revoke(id), None)
            }
            Some(Mutation::Suspend(id, suspend)) => {
                tenants
                    .set_suspended(&log, &id, suspend, now, "control-state-cli")
                    .map_err(api)?;
                (control_state::Edit::Suspend(id, suspend), None)
            }
            Some(Mutation::Reimport(generation)) => (
                control_state::Edit::Reimport(&credentials, &tenants),
                Some(generation),
            ),
            None => (control_state::Edit::Directory, None),
        };
        objects
            .apply(now, &edit, directory.as_deref(), expected)
            .map_err(api)?
    };
    drop(lock);
    Ok(json!({"generation":snapshot.generation(),"object":source.object}))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TENANT: &str = "0123456789abcdef0123456789abcdef";

    struct Upstream {
        endpoint: String,
        requests: Arc<Mutex<Vec<(HeaderMap, Bytes)>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Upstream {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn upstream(status: StatusCode, headers: HeaderMap, delay: Duration) -> Upstream {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let router = Router::new().fallback(move |request: Request| {
            let captured = captured.clone();
            let headers = headers.clone();
            async move {
                let (parts, body) = request.into_parts();
                let body = axum::body::to_bytes(body, 1024).await.unwrap();
                captured.lock().unwrap().push((parts.headers, body));
                tokio::time::sleep(delay).await;
                (status, headers, "backend answer")
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Upstream {
            endpoint,
            requests,
            task,
        }
    }

    fn reader_headers() -> HeaderMap {
        map(&[
            ("x-vlite-epoch", "5"),
            ("x-vlite-lineage", "0000000000001234"),
            ("x-vlite-binding-generation", "3"),
            ("x-vlite-applied-age-ms", "12"),
            ("x-vlite-applied-sequence", "42"),
        ])
    }

    fn routed_gateway(
        writer: &str,
        reader: &str,
        routing: control_state::ReadRouting,
    ) -> (Arc<Gateway>, String) {
        let mut record = control_state::Route::dedicated_for_test(TENANT, 5);
        record.writer = writer.to_owned();
        record.reader = Some(reader.to_owned());
        record.read_routing = routing;
        let (gateway, token, _) = gateway_with_route(record);
        (gateway, token)
    }

    fn gateway_with_route(
        record: control_state::Route,
    ) -> (Arc<Gateway>, String, control_state::Objects) {
        let (credentials, tokens) =
            credentials::tests::issued(&[("aaaaaaaaaaaa", Some(TENANT))], 0x1234);
        let tenants = tenants::Store::for_test(vec![tenants::Tenant::for_test(TENANT, None)]);
        let snapshot = Snapshot::import(&credentials, Some(&tenants))
            .unwrap()
            .with_directory(vec![record]);
        let mut value = serde_json::to_value(snapshot).unwrap();
        value["credentials"][0]["operations"] = json!(["read", "write", "search", "collections"]);
        let snapshot = serde_json::from_value(value).unwrap();
        let store = Arc::new(super::super::cloud_backup::fake::Memory::default());
        let objects = control_state::Objects::new(store.clone(), "control/state.json");
        objects.create(snapshot, admin::now_ms() as u64).unwrap();
        let feed = Arc::new(
            Feed::new(
                control_state::Objects::new(store, "control/state.json"),
                60_000,
                1_000,
            )
            .unwrap(),
        );
        feed.poll_once().unwrap();
        (
            Arc::new(Gateway {
                feed,
                signer: Signer::from_seed("gw-1", &[7; 32]).unwrap(),
                client: reqwest::Client::builder().no_proxy().build().unwrap(),
                upstream_timeout: Duration::from_millis(100),
                draining: AtomicBool::new(false),
                cancel: tokio::sync::watch::Sender::new(false),
                in_flight: Arc::new(Semaphore::new(8)),
                buckets: Buckets::new(8),
                max_request_bytes: 1024,
                envelope_lifetime_ms: 5_000,
                max_replicas: 1,
            }),
            tokens[0].clone(),
            objects,
        )
    }

    async fn routed_request(gateway: Arc<Gateway>, token: &str, write: bool) -> Response {
        request_with_body(gateway, token, write, Body::from(r#"{"limit":3}"#)).await
    }

    async fn request_with_body(
        gateway: Arc<Gateway>,
        token: &str,
        write: bool,
        body: Body,
    ) -> Response {
        gateway
            .forward(
                Request::builder()
                    .method(Method::POST)
                    .uri(if write {
                        "/v1/collections"
                    } else {
                        "/v1/collections/docs/search"
                    })
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap_or_else(IntoResponse::into_response)
    }

    #[tokio::test]
    async fn reader_response_with_matching_identity_is_returned_unchanged() {
        let writer = upstream(StatusCode::OK, HeaderMap::new(), Duration::ZERO).await;
        let reader = upstream(StatusCode::ACCEPTED, reader_headers(), Duration::ZERO).await;
        let (gateway, token) = routed_gateway(
            &writer.endpoint,
            &reader.endpoint,
            control_state::ReadRouting::Readers,
        );
        let response = routed_request(gateway, &token, false).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        for (name, value) in &reader_headers() {
            assert_eq!(response.headers().get(name), Some(value));
        }
        assert_eq!(
            axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap(),
            "backend answer"
        );
        assert!(writer.requests.lock().unwrap().is_empty());
        assert_eq!(reader.requests.lock().unwrap().len(), 1);
    }

    async fn assert_reader_fallback(status: StatusCode, headers: HeaderMap, delay: Duration) {
        // A writer's 503 is final even when its headers would fail reader validation.
        let writer = upstream(
            StatusCode::SERVICE_UNAVAILABLE,
            map(&[("x-vlite-epoch", "5")]),
            Duration::ZERO,
        )
        .await;
        let reader = upstream(status, headers, delay).await;
        let (gateway, token) = routed_gateway(
            &writer.endpoint,
            &reader.endpoint,
            control_state::ReadRouting::Readers,
        );
        let response = routed_request(gateway, &token, false).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let writer_requests = writer.requests.lock().unwrap();
        let reader_requests = reader.requests.lock().unwrap();
        assert_eq!(writer_requests.len(), 1);
        assert_eq!(reader_requests.len(), 1);
        assert_eq!(writer_requests[0].1, reader_requests[0].1);
        assert_eq!(writer_requests[0].1, r#"{"limit":3}"#);
        for request in [&writer_requests[0], &reader_requests[0]] {
            assert!(request.0.contains_key(principal_envelope::HEADER));
            assert!(!request.0.contains_key(header::AUTHORIZATION));
        }
    }

    #[tokio::test]
    async fn reader_503_falls_back_to_writer_once() {
        assert_reader_fallback(
            StatusCode::SERVICE_UNAVAILABLE,
            reader_headers(),
            Duration::ZERO,
        )
        .await;
    }

    #[tokio::test]
    async fn missing_or_mismatched_reader_lineage_falls_back_to_writer() {
        for lineage in [None, Some("0000000000005678")] {
            let mut headers = reader_headers();
            headers.remove("x-vlite-lineage");
            if let Some(lineage) = lineage {
                headers.insert("x-vlite-lineage", lineage.parse().unwrap());
            }
            assert_reader_fallback(StatusCode::OK, headers, Duration::ZERO).await;
        }
    }

    #[tokio::test]
    async fn missing_mismatched_or_invalid_reader_binding_generation_falls_back_to_writer() {
        for generation in [None, Some("4"), Some("invalid")] {
            let mut headers = reader_headers();
            headers.remove("x-vlite-binding-generation");
            if let Some(generation) = generation {
                headers.insert("x-vlite-binding-generation", generation.parse().unwrap());
            }
            assert_reader_fallback(StatusCode::OK, headers, Duration::ZERO).await;
        }
    }

    #[tokio::test]
    async fn reader_response_head_timeout_falls_back_to_writer() {
        assert_reader_fallback(StatusCode::OK, reader_headers(), Duration::from_secs(1)).await;
    }

    #[tokio::test]
    async fn reader_connection_refusal_falls_back_to_writer() {
        let writer = upstream(
            StatusCode::OK,
            map(&[("x-vlite-epoch", "5")]),
            Duration::ZERO,
        )
        .await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let reader = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let (gateway, token) = routed_gateway(
            &writer.endpoint,
            &reader,
            control_state::ReadRouting::Readers,
        );
        assert_eq!(
            routed_request(gateway, &token, false).await.status(),
            StatusCode::OK
        );
        assert_eq!(writer.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn writes_and_writer_only_reads_never_touch_the_reader() {
        for (routing, write) in [
            (control_state::ReadRouting::Readers, true),
            (control_state::ReadRouting::WriterOnly, false),
        ] {
            let writer = upstream(
                StatusCode::OK,
                map(&[("x-vlite-epoch", "5")]),
                Duration::ZERO,
            )
            .await;
            let reader = upstream(StatusCode::OK, reader_headers(), Duration::ZERO).await;
            let (gateway, token) = routed_gateway(&writer.endpoint, &reader.endpoint, routing);
            let response = routed_request(gateway, &token, write).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert!(!response.headers().contains_key("x-vlite-lineage"));
            assert_eq!(writer.requests.lock().unwrap().len(), 1);
            assert!(reader.requests.lock().unwrap().is_empty());
        }
    }

    fn map(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(
                HeaderName::from_static(name),
                axum::http::HeaderValue::from_static(value),
            );
        }
        headers
    }

    async fn assert_epoch_refusal(response: Response, write: bool) {
        assert_eq!(
            response.status(),
            if write {
                StatusCode::BAD_GATEWAY
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            }
        );
        assert!(!response.headers().contains_key("x-vlite-epoch"));
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert!(
            !body
                .windows(b"backend answer".len())
                .any(|part| part == b"backend answer")
        );
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body["error"]["code"],
            if write {
                "write_outcome_unknown"
            } else {
                "backend_epoch_stale"
            }
        );
    }

    #[tokio::test]
    async fn gateway_checks_epoch_before_stream() {
        // HTTP normalizes field-edge whitespace; the parser test covers it directly.
        let cases = [
            (map(&[]), false),
            (map(&[("x-vlite-epoch", "4")]), false),
            (map(&[("x-vlite-epoch", "05")]), false),
            (map(&[("x-vlite-epoch", "+5")]), false),
            (map(&[("x-vlite-epoch", "5 5")]), false),
            (map(&[("x-vlite-epoch", "5,5")]), false),
            (
                map(&[("x-vlite-epoch", "5"), ("x-vlite-epoch", "5")]),
                false,
            ),
            (map(&[("x-vlite-epoch", "0")]), false),
            (map(&[("x-vlite-epoch", "18446744073709551616")]), false),
            (map(&[("x-vlite-epoch", "5")]), true),
            (map(&[("x-vlite-epoch", "6")]), true),
        ];
        for (headers, accepted) in cases {
            let writer = upstream(StatusCode::ACCEPTED, headers.clone(), Duration::ZERO).await;
            let (gateway, token) = routed_gateway(
                &writer.endpoint,
                &writer.endpoint,
                control_state::ReadRouting::WriterOnly,
            );
            let response = routed_request(gateway, &token, false).await;
            if accepted {
                assert_eq!(response.status(), StatusCode::ACCEPTED);
                assert_eq!(
                    response.headers().get("x-vlite-epoch"),
                    headers.get("x-vlite-epoch")
                );
                assert_eq!(
                    axum::body::to_bytes(response.into_body(), 1024)
                        .await
                        .unwrap(),
                    "backend answer"
                );
            } else {
                assert_epoch_refusal(response, false).await;
            }
            assert_eq!(writer.requests.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn gateway_epoch_refusal_never_retries_write() {
        let writer = upstream(
            StatusCode::OK,
            map(&[("x-vlite-epoch", "4")]),
            Duration::ZERO,
        )
        .await;
        let reader = upstream(StatusCode::OK, reader_headers(), Duration::ZERO).await;
        let (gateway, token) = routed_gateway(
            &writer.endpoint,
            &reader.endpoint,
            control_state::ReadRouting::Readers,
        );
        assert_epoch_refusal(routed_request(gateway, &token, true).await, true).await;
        assert_eq!(writer.requests.lock().unwrap().len(), 1);
        assert!(reader.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn stale_missing_or_invalid_reader_epoch_falls_back_to_writer() {
        for epoch in [None, Some("4"), Some("05"), Some("malformed")] {
            let mut headers = reader_headers();
            headers.remove("x-vlite-epoch");
            if let Some(epoch) = epoch {
                headers.insert("x-vlite-epoch", epoch.parse().unwrap());
            }
            assert_reader_fallback(StatusCode::OK, headers, Duration::ZERO).await;
        }
        let mut nominated = reader_headers();
        nominated.insert("connection", "x-vlite-epoch".parse().unwrap());
        assert_reader_fallback(StatusCode::OK, nominated, Duration::ZERO).await;
    }

    #[tokio::test]
    async fn reader_epoch_fallback_still_checks_the_writer_stamp() {
        let writer = upstream(StatusCode::OK, HeaderMap::new(), Duration::ZERO).await;
        let mut headers = reader_headers();
        headers.insert("x-vlite-epoch", "4".parse().unwrap());
        let reader = upstream(StatusCode::OK, headers, Duration::ZERO).await;
        let (gateway, token) = routed_gateway(
            &writer.endpoint,
            &reader.endpoint,
            control_state::ReadRouting::Readers,
        );
        assert_epoch_refusal(routed_request(gateway, &token, false).await, false).await;
        assert_eq!(writer.requests.lock().unwrap().len(), 1);
        assert_eq!(reader.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn gateway_strips_forged_epoch_request() {
        let writer = upstream(
            StatusCode::OK,
            map(&[("x-vlite-epoch", "5")]),
            Duration::ZERO,
        )
        .await;
        let (gateway, token) = routed_gateway(
            &writer.endpoint,
            &writer.endpoint,
            control_state::ReadRouting::WriterOnly,
        );
        let response = gateway
            .forward(
                Request::builder()
                    .uri("/v1/collections")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header("X-VLite-Epoch", "99")
                    .header("x-vlite-custom", "preserved")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let requests = writer.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].0.contains_key("x-vlite-epoch"));
        assert_eq!(requests[0].0.get("x-vlite-custom").unwrap(), "preserved");
    }

    #[tokio::test]
    async fn gateway_connection_nominated_epoch_refused() {
        let writer = upstream(
            StatusCode::OK,
            map(&[("x-vlite-epoch", "5"), ("connection", "x-vlite-epoch")]),
            Duration::ZERO,
        )
        .await;
        let (gateway, token) = routed_gateway(
            &writer.endpoint,
            &writer.endpoint,
            control_state::ReadRouting::WriterOnly,
        );
        assert_epoch_refusal(routed_request(gateway, &token, false).await, false).await;
        assert_eq!(writer.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn gateway_directory_advance_raises_expected_epoch() {
        assert_directory_change_refused(|mut record| {
            record.epoch = Some(6);
            Some(record)
        })
        .await;
    }

    #[tokio::test]
    async fn gateway_directory_removal_or_lineage_change_fails_closed() {
        assert_directory_change_refused(|_| None).await;
        assert_directory_change_refused(|mut record| {
            record.lineage = 0x5678;
            Some(record)
        })
        .await;
    }

    async fn assert_directory_change_refused(
        change: impl FnOnce(control_state::Route) -> Option<control_state::Route>,
    ) {
        let writer = upstream(
            StatusCode::OK,
            map(&[("x-vlite-epoch", "5")]),
            Duration::from_millis(200),
        )
        .await;
        let mut record = control_state::Route::dedicated_for_test(TENANT, 5);
        record.writer = writer.endpoint.clone();
        let (mut gateway, token, objects) = gateway_with_route(record.clone());
        Arc::get_mut(&mut gateway).unwrap().upstream_timeout = Duration::from_secs(2);
        let directory: Vec<_> = change(record).into_iter().collect();
        let update = async {
            tokio::time::timeout(Duration::from_secs(1), async {
                while writer.requests.lock().unwrap().is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            objects
                .update(admin::now_ms() as u64, |snapshot| {
                    Ok(snapshot.clone().with_directory(directory.clone()))
                })
                .unwrap();
            gateway.feed.poll_once().unwrap();
        };
        let (response, ()) = tokio::join!(routed_request(gateway.clone(), &token, false), update);
        assert_epoch_refusal(response, false).await;
        assert_eq!(writer.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn gateway_expired_control_state_refuses_dedicated_but_not_shared_responses() {
        for (dedicated, write) in [(true, false), (true, true), (false, false)] {
            let writer = upstream(
                StatusCode::ACCEPTED,
                map(&[("x-vlite-epoch", "5")]),
                Duration::from_millis(500),
            )
            .await;
            let mut record = if dedicated {
                control_state::Route::dedicated_for_test(TENANT, 5)
            } else {
                control_state::Route::shared_for_test(TENANT)
            };
            record.writer = writer.endpoint.clone();
            let (mut gateway, token, objects) = gateway_with_route(record);
            let settings = Arc::get_mut(&mut gateway).unwrap();
            settings.upstream_timeout = Duration::from_secs(2);
            settings.feed = Arc::new(Feed::new(objects, 250, 10).unwrap());
            settings.feed.poll_once().unwrap();
            let response = routed_request(gateway.clone(), &token, write).await;
            assert!(gateway.feed.current().is_err());
            if dedicated {
                assert_epoch_refusal(response, write).await;
            } else {
                assert_eq!(response.status(), StatusCode::ACCEPTED);
                assert_eq!(response.headers().get("x-vlite-epoch").unwrap(), "5");
                assert_eq!(
                    axum::body::to_bytes(response.into_body(), 1024)
                        .await
                        .unwrap(),
                    "backend answer"
                );
            }
            assert_eq!(writer.requests.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn gateway_shared_pool_ignores_epoch_header() {
        for headers in [
            map(&[]),
            map(&[("x-vlite-epoch", "malformed")]),
            map(&[("x-vlite-epoch", "0"), ("x-vlite-epoch", "05")]),
        ] {
            let writer = upstream(StatusCode::ACCEPTED, headers.clone(), Duration::ZERO).await;
            let mut record = control_state::Route::shared_for_test(TENANT);
            record.writer = writer.endpoint.clone();
            let (gateway, token, _) = gateway_with_route(record);
            let response = routed_request(gateway, &token, false).await;
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            assert_eq!(
                response
                    .headers()
                    .get_all("x-vlite-epoch")
                    .iter()
                    .collect::<Vec<_>>(),
                headers.get_all("x-vlite-epoch").iter().collect::<Vec<_>>()
            );
            assert_eq!(
                axum::body::to_bytes(response.into_body(), 1024)
                    .await
                    .unwrap(),
                "backend answer"
            );
        }
    }

    #[test]
    fn forwarded_requests_carry_no_caller_authority_or_hop_headers() {
        let incoming = map(&[
            ("authorization", "Bearer vlk_x"),
            ("cookie", "s=1"),
            ("x-vlite-principal", "forged"),
            ("forwarded", "for=1.2.3.4"),
            ("x-forwarded-for", "1.2.3.4"),
            ("x-real-ip", "1.2.3.4"),
            ("host", "evil"),
            ("connection", "keep-alive, x-secret"),
            ("x-secret", "nominated"),
            ("keep-alive", "timeout=5"),
            ("proxy-authorization", "Basic x"),
            ("te", "trailers"),
            ("transfer-encoding", "chunked"),
            ("upgrade", "h2c"),
            ("expect", "100-continue"),
            ("content-length", "3"),
            ("content-type", "application/json"),
            ("accept", "application/json"),
            ("x-request-id", "r1"),
        ]);
        let forwarded = request_headers(&incoming);
        let names: Vec<&str> = forwarded.keys().map(HeaderName::as_str).collect();
        assert_eq!(names, ["content-type", "accept", "x-request-id"]);

        let response = response_headers(&map(&[
            ("set-cookie", "s=1"),
            ("connection", "x-hop"),
            ("x-hop", "1"),
            ("transfer-encoding", "chunked"),
            ("content-type", "application/json"),
            ("content-length", "2"),
            ("retry-after", "1"),
        ]));
        let names: Vec<&str> = response.keys().map(HeaderName::as_str).collect();
        assert_eq!(names, ["content-type", "content-length", "retry-after"]);
    }

    /// A one-shot HTTP/1.1 backend that sends its head, then `chunks` four-byte
    /// chunks `gap` apart, and then either ends the body or stalls.
    async fn trickle(chunks: usize, gap: Duration, stall: bool) -> reqwest::Response {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = socket.read(&mut [0u8; 1024]).await;
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                .await;
            for _ in 0..chunks {
                tokio::time::sleep(gap).await;
                let _ = socket.write_all(b"4\r\nabcd\r\n").await;
            }
            if stall {
                tokio::time::sleep(Duration::from_secs(30)).await;
            } else {
                let _ = socket.write_all(b"0\r\n\r\n").await;
            }
        });
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap()
    }

    fn permit() -> tokio::sync::OwnedSemaphorePermit {
        Arc::new(tokio::sync::Semaphore::new(1))
            .try_acquire_owned()
            .unwrap()
    }

    async fn next_piece(relay: &mut Relay) -> Option<Result<Bytes, std::io::Error>> {
        tokio::time::timeout(Duration::from_secs(5), tokio_stream::StreamExt::next(relay))
            .await
            .expect("the relay answers promptly")
    }

    async fn error_code(response: Response) -> String {
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        body["error"]["code"].as_str().unwrap().to_owned()
    }

    /// Waits until every slot of `semaphore` is free again.
    async fn released(semaphore: &Semaphore, slots: usize, within: Duration) {
        let deadline = std::time::Instant::now() + within;
        while semaphore.available_permits() < slots {
            assert!(
                std::time::Instant::now() < deadline,
                "the in-flight slot was not released"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn a_streamed_body_longer_than_the_timeout_arrives_intact() {
        // 8 chunks 100 ms apart take about 800 ms, twice the 400 ms timeout;
        // no single wait comes near it.
        let upstream = trickle(8, Duration::from_millis(100), false).await;
        let (sender, receiver) = tokio::sync::mpsc::channel::<Piece>(4);
        let (_cancel, cancel_rx) = tokio::sync::watch::channel(false);
        tokio::spawn(pump(
            upstream,
            sender,
            Duration::from_millis(400),
            cancel_rx,
            permit(),
            Operation::Read,
        ));
        let mut relay = Relay::new(receiver);
        let mut body = Vec::new();
        while let Some(piece) = next_piece(&mut relay).await {
            body.extend_from_slice(&piece.expect("no piece fails"));
        }
        assert_eq!(body, b"abcd".repeat(8));
    }

    #[tokio::test]
    async fn a_stalled_stream_is_cut_short_after_the_timeout() {
        let upstream = trickle(1, Duration::from_millis(10), true).await;
        let (sender, receiver) = tokio::sync::mpsc::channel::<Piece>(4);
        let (_cancel, cancel_rx) = tokio::sync::watch::channel(false);
        tokio::spawn(pump(
            upstream,
            sender,
            Duration::from_millis(300),
            cancel_rx,
            permit(),
            Operation::Write,
        ));
        let mut relay = Relay::new(receiver);
        assert_eq!(next_piece(&mut relay).await.unwrap().unwrap(), "abcd");
        assert!(next_piece(&mut relay).await.unwrap().is_err());
        assert!(next_piece(&mut relay).await.is_none());
    }

    #[tokio::test]
    async fn a_drain_cancel_cuts_a_stream_short_and_frees_its_slot() {
        let upstream = trickle(1, Duration::from_millis(10), true).await;
        let in_flight = Arc::new(Semaphore::new(1));
        let (sender, receiver) = tokio::sync::mpsc::channel::<Piece>(4);
        let (cancel, cancel_rx) = tokio::sync::watch::channel(false);
        tokio::spawn(pump(
            upstream,
            sender,
            Duration::from_secs(30),
            cancel_rx,
            in_flight.clone().try_acquire_owned().unwrap(),
            Operation::Read,
        ));
        let mut relay = Relay::new(receiver);
        assert_eq!(next_piece(&mut relay).await.unwrap().unwrap(), "abcd");
        cancel.send_replace(true);
        assert!(next_piece(&mut relay).await.unwrap().is_err());
        assert!(next_piece(&mut relay).await.is_none());
        released(&in_flight, 1, Duration::from_secs(2)).await;
    }

    #[tokio::test]
    async fn a_client_that_stops_reading_frees_its_slot() {
        let upstream = trickle(16, Duration::from_millis(5), false).await;
        let in_flight = Arc::new(Semaphore::new(1));
        let (sender, receiver) = tokio::sync::mpsc::channel::<Piece>(1);
        let (_cancel, cancel_rx) = tokio::sync::watch::channel(false);
        tokio::spawn(pump(
            upstream,
            sender,
            Duration::from_millis(200),
            cancel_rx,
            in_flight.clone().try_acquire_owned().unwrap(),
            Operation::Read,
        ));
        // The relay stays open but is never read.
        let _relay = Relay::new(receiver);
        released(&in_flight, 1, Duration::from_secs(2)).await;
    }

    #[tokio::test]
    async fn a_stalled_request_body_times_out_and_frees_its_slot() {
        let writer = upstream(
            StatusCode::OK,
            map(&[("x-vlite-epoch", "5")]),
            Duration::ZERO,
        )
        .await;
        let reader = upstream(StatusCode::OK, reader_headers(), Duration::ZERO).await;
        let (mut gateway, token) = routed_gateway(
            &writer.endpoint,
            &reader.endpoint,
            control_state::ReadRouting::Readers,
        );
        Arc::get_mut(&mut gateway).unwrap().in_flight = Arc::new(Semaphore::new(1));
        let (body_tx, body_rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(1);
        body_tx
            .send(Ok(Bytes::from_static(b"{\"na")))
            .await
            .unwrap();
        let body = Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(body_rx));
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            request_with_body(gateway.clone(), &token, true, body),
        )
        .await
        .expect("a stalled body is refused promptly");
        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
        assert_eq!(error_code(response).await, "request_body_timeout");
        assert!(writer.requests.lock().unwrap().is_empty());
        drop(body_tx);
        // The only slot is free again: the next request is not `overloaded`.
        let response = routed_request(gateway, &token, true).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(writer.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_drain_cancel_refuses_a_request_still_receiving_its_body() {
        let writer = upstream(
            StatusCode::OK,
            map(&[("x-vlite-epoch", "5")]),
            Duration::ZERO,
        )
        .await;
        let reader = upstream(StatusCode::OK, reader_headers(), Duration::ZERO).await;
        let (mut gateway, token) = routed_gateway(
            &writer.endpoint,
            &reader.endpoint,
            control_state::ReadRouting::Readers,
        );
        Arc::get_mut(&mut gateway).unwrap().upstream_timeout = Duration::from_secs(5);
        let (body_tx, body_rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(1);
        let body = Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(body_rx));
        let forwarded = tokio::spawn({
            let gateway = gateway.clone();
            async move { request_with_body(gateway, &token, true, body).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        gateway.cancel.send_replace(true);
        let response = tokio::time::timeout(Duration::from_secs(1), forwarded)
            .await
            .expect("the cancel ends the request promptly")
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error_code(response).await, "draining");
        assert!(writer.requests.lock().unwrap().is_empty());
        drop(body_tx);
    }

    #[tokio::test]
    async fn a_drain_cancel_before_the_first_send_poll_is_not_an_unknown_write() {
        let (cancel, mut cancel_rx) = tokio::sync::watch::channel(true);
        let send = std::future::poll_fn(|_| -> std::task::Poll<()> {
            panic!("a cancelled request must not be dispatched");
        });
        let response = dispatch(
            send,
            Duration::from_secs(1),
            &mut cancel_rx,
            Operation::Write,
        )
        .await
        .unwrap_err()
        .into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error_code(response).await, "draining");
        drop(cancel);
    }

    #[tokio::test]
    async fn a_drain_cancel_after_the_first_send_poll_is_an_unknown_write() {
        let (cancel, mut cancel_rx) = tokio::sync::watch::channel(false);
        let polls = std::cell::Cell::new(0);
        let send = std::future::poll_fn(|_| -> std::task::Poll<()> {
            polls.set(polls.get() + 1);
            cancel.send_replace(true);
            std::task::Poll::Pending
        });
        let response = dispatch(
            send,
            Duration::from_secs(1),
            &mut cancel_rx,
            Operation::Write,
        )
        .await
        .unwrap_err()
        .into_response();
        assert_eq!(polls.get(), 1);
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(error_code(response).await, "write_outcome_unknown");
    }

    #[tokio::test]
    async fn drain_settle_waits_for_the_http_error_body_after_handler_permits_are_free() {
        let writer = upstream(
            StatusCode::OK,
            map(&[("x-vlite-epoch", "5")]),
            Duration::from_secs(5),
        )
        .await;
        let (mut gateway, token) = routed_gateway(
            &writer.endpoint,
            &writer.endpoint,
            control_state::ReadRouting::Readers,
        );
        Arc::get_mut(&mut gateway).unwrap().upstream_timeout = Duration::from_secs(10);
        let generated = Arc::new(tokio::sync::Notify::new());
        let flush = Arc::new(tokio::sync::Notify::new());
        let router = Router::new().fallback({
            let gateway = gateway.clone();
            let generated = generated.clone();
            let flush = flush.clone();
            move |request: Request| {
                let gateway = gateway.clone();
                let generated = generated.clone();
                let flush = flush.clone();
                async move {
                    let response = handle(State(gateway), request).await;
                    let (parts, body) = response.into_parts();
                    let bytes = axum::body::to_bytes(body, 4096).await.unwrap();
                    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(1);
                    tokio::spawn(async move {
                        generated.notify_one();
                        flush.notified().await;
                        tx.send(Ok(bytes)).await.unwrap();
                    });
                    Response::from_parts(
                        parts,
                        Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx)),
                    )
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, mut stop_rx) = tokio::sync::watch::channel(false);
        let mut server = tokio::spawn(
            axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    stop_rx.wait_for(|stop| *stop).await.unwrap();
                })
                .into_future(),
        );
        let client = tokio::spawn(async move {
            reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap()
                .post(format!("http://{address}/v1/collections"))
                .bearer_auth(token)
                .body(r#"{"name":"docs"}"#)
                .send()
                .await
                .unwrap()
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while writer.requests.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline, "the write was not dispatched");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        stop.send_replace(true);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut server)
                .await
                .is_err()
        );
        let settling = tokio::spawn({
            let gateway = gateway.clone();
            async move { cancel_and_settle(&mut server, &gateway.cancel, Duration::from_secs(2)).await }
        });
        tokio::time::timeout(Duration::from_secs(1), generated.notified())
            .await
            .expect("cancellation produces an HTTP error");
        assert_eq!(gateway.in_flight.available_permits(), 8);
        assert!(
            !settling.is_finished(),
            "permits do not prove HTTP completion"
        );
        flush.notify_one();
        assert!(matches!(settling.await.unwrap(), Some(Ok(Ok(())))));
        let response = client.await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["code"],
            "write_outcome_unknown"
        );
    }

    #[tokio::test]
    async fn drain_settle_aborts_only_after_its_deadline() {
        let (cancel, _) = tokio::sync::watch::channel(false);
        let mut server = tokio::spawn(std::future::pending::<std::io::Result<()>>());
        let settle = Duration::from_millis(20);
        let started = Instant::now();
        assert!(
            cancel_and_settle(&mut server, &cancel, settle)
                .await
                .is_none()
        );
        assert!(*cancel.borrow());
        assert!(started.elapsed() >= settle);
        assert!(server.await.unwrap_err().is_cancelled());
    }

    /// Starts a request against a backend that answers after 5 s, waits
    /// until the backend has it, then cancels.
    async fn cancel_after_send(write: bool) -> (Response, usize, usize) {
        let delay = Duration::from_secs(5);
        let writer = upstream(StatusCode::OK, map(&[("x-vlite-epoch", "5")]), delay).await;
        let reader = upstream(StatusCode::OK, reader_headers(), delay).await;
        let (mut gateway, token) = routed_gateway(
            &writer.endpoint,
            &reader.endpoint,
            control_state::ReadRouting::Readers,
        );
        Arc::get_mut(&mut gateway).unwrap().upstream_timeout = Duration::from_secs(10);
        let forwarded = tokio::spawn({
            let gateway = gateway.clone();
            async move { routed_request(gateway, &token, write).await }
        });
        let target = if write { &writer } else { &reader };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while target.requests.lock().unwrap().is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "the backend never got the request"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        gateway.cancel.send_replace(true);
        let response = tokio::time::timeout(Duration::from_secs(1), forwarded)
            .await
            .expect("the cancel ends the request promptly")
            .unwrap();
        let writes = writer.requests.lock().unwrap().len();
        let reads = reader.requests.lock().unwrap().len();
        (response, writes, reads)
    }

    #[tokio::test]
    async fn a_drain_cancel_after_a_write_was_sent_says_the_outcome_is_unknown() {
        let (response, writes, reads) = cancel_after_send(true).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(error_code(response).await, "write_outcome_unknown");
        assert_eq!((writes, reads), (1, 0));
    }

    #[tokio::test]
    async fn a_drain_cancel_after_a_read_was_sent_is_not_retried_on_the_writer() {
        let (response, writes, reads) = cancel_after_send(false).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error_code(response).await, "draining");
        assert_eq!((writes, reads), (0, 1));
    }

    #[tokio::test]
    async fn stopping_the_feed_does_not_wait_out_a_blocked_read() {
        use super::super::cloud_backup::fake::{Memory, Op};
        let memory = Arc::new(Memory::default());
        let feed = Arc::new(
            Feed::new(
                control_state::Objects::new(memory.clone(), "control/state.json"),
                60_000,
                10,
            )
            .unwrap(),
        );
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        memory.hook(Op::Get, "control/state.json", move || {
            entered_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        feed.start(|_| {}).unwrap();
        tokio::task::spawn_blocking(move || entered_rx.recv_timeout(Duration::from_secs(5)))
            .await
            .unwrap()
            .expect("the poll thread reads");
        let started = std::time::Instant::now();
        stop_feed_within(&feed, Duration::from_millis(200)).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(200) && elapsed < Duration::from_secs(2),
            "{elapsed:?}"
        );
        release_tx.send(()).unwrap();
    }

    #[test]
    fn upstream_failures_say_whether_anything_was_sent() {
        let answer = |connect, timed_out, operation| {
            let failure = upstream_failure(connect, timed_out, operation);
            (failure.status.as_u16(), failure.code)
        };
        for operation in [Operation::Read, Operation::Write] {
            assert_eq!(answer(true, false, operation), (502, "backend_unavailable"));
            assert_eq!(answer(true, true, operation), (502, "backend_unavailable"));
        }
        assert_eq!(
            answer(false, true, Operation::Read),
            (504, "backend_timeout")
        );
        assert_eq!(
            answer(false, false, Operation::Read),
            (502, "backend_failed")
        );
        for timed_out in [false, true] {
            assert_eq!(
                answer(false, timed_out, Operation::Write),
                (502, "write_outcome_unknown")
            );
        }
    }

    #[test]
    fn only_one_tenant_bearer_credential_is_accepted() {
        assert_eq!(
            tenant_bearer(&map(&[("authorization", "Bearer vlk_a")])),
            Some("vlk_a")
        );
        for headers in [
            map(&[]),
            map(&[("authorization", "Bearer admin-token")]),
            map(&[("authorization", "Basic dmxrXw==")]),
            map(&[
                ("authorization", "Bearer vlk_a"),
                ("authorization", "Bearer vlk_b"),
            ]),
        ] {
            assert_eq!(tenant_bearer(&headers), None);
        }
    }

    #[test]
    fn upstream_urls_keep_the_callers_path_on_the_records_origin() {
        let uri: axum::http::Uri = "/v1/collections/docs/search?limit=3".parse().unwrap();
        let url = upstream_url("http://10.0.0.5:8080/", &uri).unwrap();
        assert_eq!(
            url.as_str(),
            "http://10.0.0.5:8080/v1/collections/docs/search?limit=3"
        );
        let url = upstream_url("https://backend.internal", &uri).unwrap();
        assert_eq!(
            url.as_str(),
            "https://backend.internal/v1/collections/docs/search?limit=3"
        );
        for endpoint in [
            "http://user:pw@backend/",
            "http://backend/prefix",
            "http://backend/?q=1",
            "http://backend/#f",
            "ftp://backend/",
            "file:///etc/passwd",
            "not a url",
        ] {
            let error = upstream_url(endpoint, &uri).unwrap_err();
            assert_eq!(error.code, "tenant_route_invalid", "{endpoint}");
        }
        // An absolute-form request target cannot choose the host.
        let uri: axum::http::Uri = "http://evil.example/v1/collections".parse().unwrap();
        let url = upstream_url("http://backend:1/", &uri).unwrap();
        assert_eq!(url.as_str(), "http://backend:1/v1/collections");
    }

    #[test]
    fn dot_segments_encoded_separators_and_backslashes_are_refused() {
        assert!(safe_path("/v1/collections/docs/vectors/1"));
        for path in [
            "/v1/collections/docs/../admin",
            "/v1/collections/./docs",
            "/v1/collections/%2E%2E/x",
            "/v1/collections/a%2fb",
            "/v1/collections/a%5Cb",
            "/v1\\collections",
            "v1/collections",
        ] {
            assert!(!safe_path(path), "{path}");
        }
    }

    #[test]
    fn edge_buckets_hold_and_refill_exactly_the_share() {
        let buckets = Buckets::new(2);
        let start = Instant::now();
        // Share 2.5: a new bucket admits two, then refills 2.5 a second.
        for _ in 0..2 {
            buckets.take("c1", 2.5, start).unwrap();
        }
        let refused = buckets.take("c1", 2.5, start).unwrap_err();
        assert_eq!(
            (refused.status.as_u16(), refused.code),
            (429, "rate_limited")
        );
        let later = start + Duration::from_millis(400);
        buckets.take("c1", 2.5, later).unwrap();
        assert!(buckets.take("c1", 2.5, later).is_err());
        // Capacity caps refill: after a long idle, still only the share.
        let idle = start + Duration::from_secs(60);
        for _ in 0..2 {
            buckets.take("c1", 2.5, idle).unwrap();
        }
        assert!(buckets.take("c1", 2.5, idle).is_err());
    }

    #[test]
    fn bucket_table_is_bounded_and_evicts_only_full_buckets() {
        let buckets = Buckets::new(2);
        let start = Instant::now();
        buckets.take("a", 1.0, start).unwrap();
        buckets.take("b", 1.0, start).unwrap();
        let refused = buckets.take("c", 1.0, start).unwrap_err();
        assert_eq!(refused.code, "rate_limit_capacity");
        // Once a and b have refilled they are indistinguishable from new.
        let later = start + Duration::from_secs(1);
        buckets.take("c", 1.0, later).unwrap();
        assert!(buckets.take("c", 1.0, later).is_err());
    }

    #[test]
    fn a_share_below_one_request_fails_closed() {
        assert!(gateway::replica_share(1, 2).is_err());
        assert!(gateway::replica_share(2, 0).is_err());
        assert_eq!(gateway::replica_share(2, 2), Ok(1.0));
    }

    #[test]
    fn command_flags_refuse_unknown_duplicate_and_missing_values_without_echo() {
        let args = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();
        let allowed = &["--a", "--b"];
        let parsed = flags(&args(&["--a", "1", "--b", "2"]), allowed, "u").unwrap();
        assert_eq!(parsed.len(), 2);
        for list in [
            &["--a", "1", "--a", "2"][..],
            &["--c", "secret-value"],
            &["--a"],
            &["secret-value"],
        ] {
            let error = flags(&args(list), allowed, "u").unwrap_err();
            assert!(!error.contains("secret-value"), "{error}");
        }
        assert!(control_state_command(&args(&["rotate"])).is_err());
        assert!(control_state_command(&args(&["publish", "--revoke-credential", "x"])).is_err());
        assert!(
            control_state_command(&args(&[
                "update",
                "--target-file",
                "t",
                "--object",
                "o.json",
                "--admin-dir",
                "d",
                "--suspend-tenant",
                "a",
                "--resume-tenant",
                "a",
            ]))
            .unwrap_err()
            .contains("at most one")
        );
    }

    #[test]
    fn control_state_update_refuses_blind_republishes_and_concurrent_runs() {
        // Security review, finding 1.
        let dir = tempfile::tempdir().unwrap();
        let admin = dir.path().to_str().unwrap();
        let run = |extra: &[&str]| {
            let mut list = vec![
                "update",
                "--target-file",
                "missing-target.json",
                "--object",
                "o.json",
                "--admin-dir",
                admin,
            ];
            list.extend_from_slice(extra);
            let args = list.iter().map(OsString::from).collect::<Vec<_>>();
            control_state_command(&args).unwrap_err()
        };
        assert!(run(&[]).contains("update needs one of"));
        for generation in ["0", "abc", "-1"] {
            let error = run(&["--reimport-if-generation", generation]);
            assert!(error.contains("published generation"), "{error}");
        }
        let error = run(&["--revoke-credential", "x", "--reimport-if-generation", "3"]);
        assert!(error.contains("at most one"), "{error}");

        let held = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.path().join(CONTROL_STATE_LOCK))
            .unwrap();
        held.try_lock().unwrap();
        let error = run(&["--revoke-credential", "aaaaaaaaaaaa"]);
        assert!(error.starts_with("admin_dir_busy"), "{error}");
        drop(held);
        // Free again: the run gets past the lock (and stops at the empty
        // admin directory or the missing target instead).
        let error = run(&["--revoke-credential", "aaaaaaaaaaaa"]);
        assert!(!error.starts_with("admin_dir_busy"), "{error}");
    }
}
