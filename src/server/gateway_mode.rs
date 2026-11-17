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
//! forward to that tenant's writer (`gateway::route_target`) with a principal
//! envelope signed for that writer's audience and the snapshot's generation
//! `A`. The backend (`gateway_trust`) re-verifies all of it independently.
//!
//! Forwarding keeps the method, path, query, body and status; the caller's
//! `Authorization`, `Cookie`, principal, proxy, forwarding and hop-by-hop
//! headers (and any a `Connection` header nominates) are dropped. Request
//! bodies are bounded (`max_request_bytes`), responses are streamed, the
//! upstream client follows no redirect and uses no proxy, and the upstream
//! URL is the record's origin plus the caller's validated path. Nothing is
//! retried: a write whose connection fails after it may have been sent is
//! `502 write_outcome_unknown` (`gateway::ambiguous_write`).
//!
//! Not yet: login, the dashboard and super-admin operations through a
//! gateway (OD-004/OD-012); reader routing (ITEM-021); and fencing. A
//! dedicated record's epoch is carried in the directory, but no backend
//! stamps responses, so `gateway::check_epoch` is not applied (OD-002).

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
const CONTROL_USAGE: &str = "vlite-server control-state publish|update --target-file <json> --object <name> [--shared-key-file <path>] --admin-dir <dir> [--directory-file <json>] [update only, at most one of: --revoke-credential <id> | --suspend-tenant <id> | --resume-tenant <id>]";
/// A signing seed file is 43 base64url characters; allow whitespace.
const MAX_SEED_FILE_BYTES: u64 = 1024;
const MAX_IN_FLIGHT: usize = 65_536;
const MAX_BUCKETS: usize = 1_000_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GatewayFile {
    bind: SocketAddr,
    /// A non-loopback `bind` must be explicitly allowed.
    #[serde(default)]
    allow_remote: bool,
    control_state: Source,
    s_ctl_ms: u64,
    poll_interval_ms: u64,
    /// The replica maximum each credential limit is divided by.
    max_replicas: u32,
    envelope_lifetime_ms: u64,
    public_key_bundle_file: PathBuf,
    signing_key_file: PathBuf,
    active_kid: String,
    /// How long `/ready` reports draining before the listener closes.
    readiness_propagation_ms: u64,
    /// Bound on in-flight requests finishing after the listener closed.
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
struct Settings {
    file: GatewayFile,
    signer: Signer,
}

fn read_config(path: &Path) -> Result<Settings, String> {
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
/// serves, the listener closes, in-flight requests get `drain_timeout_ms`,
/// and the control-state poller stops.
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
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(upstream)
        .timeout(upstream)
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
        draining: AtomicBool::new(false),
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
                    log_stderr(
                        "warn",
                        "gateway",
                        json!({"event":"drain_timeout","drain_timeout_ms":file.drain_timeout_ms})
                            .to_string(),
                    );
                    server.abort();
                }
            }
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

async fn stop_feed(feed: &Arc<Feed>) {
    let feed = feed.clone();
    let _ = tokio::task::spawn_blocking(move || feed.stop()).await;
}

struct Gateway {
    feed: Arc<Feed>,
    signer: Signer,
    client: reqwest::Client,
    draining: AtomicBool,
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
        let target = gateway::route_target(classification, &grant, &snapshot)
            .map_err(RouteRefusal::error)?;
        let url = upstream_url(&target.endpoint, &parts.uri)?;
        let envelope = gateway::mint(
            &self.signer,
            &target,
            &grant,
            snapshot.generation(),
            now,
            self.envelope_lifetime_ms,
        )
        .map_err(super::internal)?;
        let declared = parts
            .headers
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        let too_large = || {
            ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                format!(
                    "the request body must be complete and at most {} bytes",
                    self.max_request_bytes
                ),
            )
        };
        if declared.is_some_and(|length| length > self.max_request_bytes as u64) {
            return Err(too_large());
        }
        let body = axum::body::to_bytes(body, self.max_request_bytes)
            .await
            .map_err(|_| too_large())?;
        let mut headers = request_headers(&parts.headers);
        headers.insert(
            HeaderName::from_static(principal_envelope::HEADER),
            envelope.parse().map_err(super::internal)?,
        );
        let sent = self
            .client
            .request(parts.method.clone(), url)
            .headers(headers)
            .body(body)
            .send()
            .await;
        let upstream = match sent {
            Ok(upstream) => upstream,
            Err(error) => return Err(upstream_failure(&error, target.operation)),
        };
        let status = upstream.status();
        let headers = response_headers(upstream.headers());
        let (sender, receiver) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(4);
        let operation = target.operation;
        tokio::spawn(async move {
            let _permit = permit;
            let mut upstream = upstream;
            loop {
                match upstream.chunk().await {
                    Ok(Some(chunk)) => {
                        if sender.send(Ok(chunk)).await.is_err() {
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(_) => {
                        log_stderr(
                            "warn",
                            "gateway",
                            json!({"event":"gateway_upstream_failed","code":"response_cut_short",
                                "operation":operation_name(operation)})
                            .to_string(),
                        );
                        let _ = sender
                            .send(Err(std::io::Error::other("backend response cut short")))
                            .await;
                        return;
                    }
                }
            }
        });
        let mut response = Response::new(Body::from_stream(
            tokio_stream::wrappers::ReceiverStream::new(receiver),
        ));
        *response.status_mut() = status;
        *response.headers_mut() = headers;
        Ok(response)
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
fn upstream_failure(error: &reqwest::Error, operation: Operation) -> ApiError {
    let failure = if error.is_connect() {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "backend_unavailable",
            "the tenant's backend could not be reached; nothing was sent",
        )
    } else if matches!(operation, Operation::Write) {
        gateway::ambiguous_write()
    } else if error.is_timeout() {
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
/// principal, forwarding/proxy, host, framing or hop-by-hop header.
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
fn flags(
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

fn utf8(name: &str, value: OsString) -> Result<String, String> {
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
}

/// `vlite-server control-state publish|update`: publishes the admin
/// directory's credentials and tenants (plus an optional tenant directory)
/// as the shared control state, through its compare-and-swap. `publish`
/// creates generation 1 and refuses an existing object; `update` applies at
/// most one local mutation (revoke a credential, suspend or resume a
/// tenant) and then republishes at the next generation, keeping the
/// directory unless `--directory-file` replaces it. The store is blocking:
/// call off the async runtime.
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
        allowed.extend(["--revoke-credential", "--suspend-tenant", "--resume-tenant"]);
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
    if mutations.len() > 1 {
        return Err(usage(
            "give at most one of --revoke-credential, --suspend-tenant, --resume-tenant",
        ));
    }
    let api = |error: ApiError| format!("{}: {}", error.code, error.message);
    let directory = values
        .remove("--directory-file")
        .map(|path| control_state::read_directory_file(Path::new(&path)))
        .transpose()
        .map_err(api)?;
    let directory_admin = std::fs::canonicalize(&admin_dir)
        .ok()
        .filter(|path| path.is_dir())
        .ok_or_else(|| usage("--admin-dir must be an existing admin directory"))?;
    let io = |what: &str| {
        let what = what.to_owned();
        move |error: std::io::Error| format!("admin_dir_invalid: {what}: {}", error.kind())
    };
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
        match mutations.pop() {
            Some(Mutation::Revoke(id)) => {
                credentials.revoke_id(&log, &id, now, Some("control-state-cli"))
            }
            Some(Mutation::Suspend(id, suspend)) => {
                tenants.set_suspended(&log, &id, suspend, now, "control-state-cli")
            }
            None => Ok(Value::Null),
        }
        .map_err(api)?;
        objects
            .update(now, |current| {
                let next = current.reimport(&credentials, Some(&tenants))?;
                Ok(match &directory {
                    Some(directory) => next.with_directory(directory.clone()),
                    None => next,
                })
            })
            .map_err(api)?
    };
    Ok(json!({"generation":snapshot.generation(),"object":source.object}))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
