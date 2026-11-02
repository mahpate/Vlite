//! Private, inert provider startup registry. No live handler dispatches through it.
#![allow(dead_code)] // Dispatch is deliberately reserved for the next acceptance slice.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    io::{Read, Write},
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Semaphore;

const REQUEST_BYTES: usize = 1_048_576;
const RESPONSE_BYTES: usize = 4_194_304;
const CONFIG_BYTES: usize = 1_048_576;
const CONFIG_ERROR: &str = "invalid provider configuration";
const MAX_ALIASES: usize = 64;
const MAX_BINDINGS: usize = 4096;
const MAX_ALLOWLIST_ADDRESSES: usize = 64;
const MAX_SECRET_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProviderError {
    NotConfigured,
    InvalidInput,
    InvalidResponse,
    Unavailable,
    Timeout,
    Busy,
}

fn check_embedding(vector: &[f32], dimensions: usize) -> Result<(), ProviderError> {
    if vector.len() != dimensions || vector.iter().any(|value| !value.is_finite()) {
        return Err(ProviderError::InvalidResponse);
    }
    Ok(())
}

fn check_ranked(
    results: &[Ranked],
    k: usize,
    candidates: &BTreeSet<u64>,
) -> Result<(), ProviderError> {
    let mut seen = BTreeSet::new();
    if results.len() != k
        || results.iter().any(|result| {
            !result.cost.is_finite() || !candidates.contains(&result.id) || !seen.insert(result.id)
        })
    {
        return Err(ProviderError::InvalidResponse);
    }
    Ok(())
}

fn sort_ranked(results: &mut [Ranked]) {
    for result in results.iter_mut() {
        result.cost += 0.0;
    }
    results.sort_by(|a, b| a.cost.total_cmp(&b.cost).then(a.id.cmp(&b.id)));
}

impl ProviderError {
    pub(crate) fn mapping(self) -> (u16, &'static str, &'static str) {
        match self {
            Self::NotConfigured => (409, "provider_not_configured", "FAILED_PRECONDITION"),
            Self::InvalidInput => (422, "invalid_provider_input", "INVALID_ARGUMENT"),
            Self::InvalidResponse => (502, "provider_invalid_response", "UNAVAILABLE"),
            Self::Unavailable => (503, "provider_unavailable", "UNAVAILABLE"),
            Self::Timeout => (504, "provider_timeout", "DEADLINE_EXCEEDED"),
            Self::Busy => (429, "provider_busy", "RESOURCE_EXHAUSTED"),
        }
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.mapping().1)
    }
}

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Embedding,
    Rerank,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Config {
    aliases: Vec<Alias>,
    bindings: Vec<Binding>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Alias {
    alias: String,
    kind: Kind,
    endpoint: String,
    model: String,
    version: String,
    dimensions: Option<usize>,
    connect_timeout_ms: u64,
    total_timeout_ms: u64,
    concurrency: usize,
    tenant_concurrency: usize,
    replay_safe: bool,
    tls_required: bool,
    allow_local_http: bool,
    endpoint_allowlist: EndpointAllowlist,
    secret: Option<SecretSource>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EndpointAllowlist {
    schemes: Vec<String>,
    hosts: Vec<String>,
    addresses: Vec<IpAddr>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum SecretSource {
    Env(String),
    File(PathBuf),
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    lineage: u64,
    collection_id: u16,
    tenant: String,
    alias: String,
    payload_fields: Vec<String>,
}

struct Provider {
    definition: Alias,
    endpoint: reqwest::Url,
    secret: Option<String>,
    slots: Arc<Semaphore>,
    dns_slots: Arc<Semaphore>,
    resolver: Arc<dyn Resolver>,
}

struct Bound {
    definition: Binding,
    slots: Arc<Semaphore>,
}

type BindingKey = (u64, u16, String, String);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfigReason {
    JsonShape,
    AliasCount,
    BindingCount,
    AliasName,
    ModelName,
    Version,
    EndpointUrl,
    EndpointCredentials,
    EndpointAllowlist,
    TlsPolicy,
    Timeout,
    Concurrency,
    Dimensions,
    DuplicateAlias,
    SecretReference,
    SecretValue,
    BindingAlias,
    BindingFields,
    DuplicateBinding,
}

#[derive(Debug, PartialEq, Eq)]
enum SecretReason {
    UnsupportedPlatform,
    InvalidPath,
    FileUnavailable,
    FileNotPrivate,
    TooLarge,
    InvalidValue,
    MissingEnvironment,
}

#[derive(Debug, PartialEq, Eq)]
struct ConfigFailure(ConfigReason);

impl ConfigFailure {
    fn public_message(&self) -> &'static str {
        CONFIG_ERROR
    }
}

trait Resolver: Send + Sync {
    fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, ProviderError>;
}

struct SystemResolver;

impl Resolver for SystemResolver {
    fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, ProviderError> {
        (host, port)
            .to_socket_addrs()
            .map(|addresses| addresses.take(MAX_ALLOWLIST_ADDRESSES + 1).collect())
            .map_err(|_| ProviderError::Unavailable)
    }
}

fn check_addresses(
    resolved: &[SocketAddr],
    allowed: &[IpAddr],
) -> Result<Vec<SocketAddr>, ProviderError> {
    if resolved.is_empty()
        || resolved.len() > MAX_ALLOWLIST_ADDRESSES
        || resolved
            .iter()
            .any(|address| !allowed.contains(&address.ip()))
    {
        return Err(ProviderError::Unavailable);
    }
    Ok(resolved.to_vec())
}

/// Opaque startup configuration; empty by default and never used by live handlers.
pub struct ProviderRegistry {
    providers: BTreeMap<String, Provider>,
    bindings: BTreeMap<BindingKey, Bound>,
    transport: Arc<dyn Transport>,
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self {
            providers: BTreeMap::new(),
            bindings: BTreeMap::new(),
            transport: Arc::new(HttpTransport),
        }
    }
}

fn name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn mode_is_private(mode: u32) -> bool {
    mode & 0o077 == 0
}

fn trim_secret_file(bytes: &[u8]) -> Result<&[u8], SecretReason> {
    if bytes.len() > MAX_SECRET_BYTES {
        return Err(SecretReason::TooLarge);
    }
    let mut end = bytes.len();
    while end > 0 && matches!(bytes[end - 1], b'\r' | b'\n') {
        end -= 1;
    }
    Ok(&bytes[..end])
}

fn validate_secret_bytes(bytes: &[u8]) -> Result<String, SecretReason> {
    if bytes.is_empty()
        || bytes.len() > MAX_SECRET_BYTES
        || !bytes.iter().all(|b| (0x21..=0x7e).contains(b))
    {
        return Err(SecretReason::InvalidValue);
    }
    String::from_utf8(bytes.to_vec()).map_err(|_| SecretReason::InvalidValue)
}

fn load_secret(source: &SecretSource) -> Result<String, SecretReason> {
    let bytes = match source {
        SecretSource::Env(variable) => {
            if !name(variable) {
                return Err(SecretReason::InvalidValue);
            }
            std::env::var(variable)
                .map_err(|_| SecretReason::MissingEnvironment)?
                .into_bytes()
        }
        SecretSource::File(path) => {
            if !path.is_absolute() {
                return Err(SecretReason::InvalidPath);
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                let before =
                    std::fs::symlink_metadata(path).map_err(|_| SecretReason::FileUnavailable)?;
                if !before.is_file() || before.file_type().is_symlink() {
                    return Err(SecretReason::FileNotPrivate);
                }
                let file = std::fs::File::open(path).map_err(|_| SecretReason::FileUnavailable)?;
                let metadata = file.metadata().map_err(|_| SecretReason::FileUnavailable)?;
                if !metadata.is_file()
                    || before.file_type().is_symlink()
                    || before.dev() != metadata.dev()
                    || before.ino() != metadata.ino()
                    || !mode_is_private(metadata.permissions().mode())
                {
                    return Err(SecretReason::FileNotPrivate);
                }
                let mut bytes = Vec::new();
                file.take(4097)
                    .read_to_end(&mut bytes)
                    .map_err(|_| SecretReason::FileUnavailable)?;
                trim_secret_file(&bytes)?.to_vec()
            }
            #[cfg(not(unix))]
            {
                return Err(SecretReason::UnsupportedPlatform);
            }
        }
    };
    validate_secret_bytes(&bytes)
}

impl ProviderRegistry {
    /// Load strict JSON without contacting any provider. Errors never echo file contents.
    pub fn from_file(path: &Path) -> Result<ProviderRegistry, String> {
        let file =
            std::fs::File::open(path).map_err(|_| "provider configuration file unavailable")?;
        let mut bytes = Vec::new();
        file.take((CONFIG_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| "provider configuration file unavailable")?;
        if bytes.len() > CONFIG_BYTES {
            return Err("provider configuration exceeds 1048576 bytes".into());
        }
        Self::parse_detailed(&bytes).map_err(|error| error.public_message().to_owned())
    }

    fn parse(bytes: &[u8]) -> Result<Self, String> {
        Self::parse_detailed(bytes).map_err(|error| error.public_message().to_owned())
    }

    fn parse_detailed(bytes: &[u8]) -> Result<Self, ConfigFailure> {
        let config: Config =
            serde_json::from_slice(bytes).map_err(|_| ConfigFailure(ConfigReason::JsonShape))?;
        if config.aliases.len() > MAX_ALIASES {
            return Err(ConfigFailure(ConfigReason::AliasCount));
        }
        if config.bindings.len() > MAX_BINDINGS {
            return Err(ConfigFailure(ConfigReason::BindingCount));
        }
        let mut registry = Self::default();
        for alias in config.aliases {
            if !name(&alias.alias) {
                return Err(ConfigFailure(ConfigReason::AliasName));
            }
            if !name(&alias.model) {
                return Err(ConfigFailure(ConfigReason::ModelName));
            }
            if !name(&alias.version)
                || ["latest", "default"]
                    .iter()
                    .any(|reserved| alias.version.eq_ignore_ascii_case(reserved))
            {
                return Err(ConfigFailure(ConfigReason::Version));
            }
            let endpoint = reqwest::Url::parse(&alias.endpoint)
                .map_err(|_| ConfigFailure(ConfigReason::EndpointUrl))?;
            let host = endpoint
                .host_str()
                .ok_or(ConfigFailure(ConfigReason::EndpointUrl))?;
            let allow = &alias.endpoint_allowlist;
            if !endpoint.username().is_empty()
                || endpoint.password().is_some()
                || endpoint.query().is_some()
                || endpoint.fragment().is_some()
                || endpoint.port_or_known_default().is_none()
            {
                return Err(ConfigFailure(ConfigReason::EndpointCredentials));
            }
            if !matches!(endpoint.scheme(), "http" | "https")
                || allow.schemes.len() != 1
                || allow.schemes[0] != endpoint.scheme()
                || allow.hosts.len() != 1
                || allow.hosts[0] != host
                || allow.addresses.is_empty()
                || allow.addresses.len() > MAX_ALLOWLIST_ADDRESSES
                || host
                    .trim_matches(['[', ']'])
                    .parse::<IpAddr>()
                    .is_ok_and(|ip| !allow.addresses.contains(&ip))
            {
                return Err(ConfigFailure(ConfigReason::EndpointAllowlist));
            }
            if (endpoint.scheme() == "https" && !alias.tls_required)
                || (endpoint.scheme() == "http"
                    && (!alias.allow_local_http
                        || alias.tls_required
                        || allow.addresses.iter().any(|ip| !ip.is_loopback())))
            {
                return Err(ConfigFailure(ConfigReason::TlsPolicy));
            }
            if !(1..=2000).contains(&alias.connect_timeout_ms)
                || !(1..=10000).contains(&alias.total_timeout_ms)
                || alias.connect_timeout_ms > alias.total_timeout_ms
            {
                return Err(ConfigFailure(ConfigReason::Timeout));
            }
            if !(1..=64).contains(&alias.concurrency)
                || !(1..=alias.concurrency).contains(&alias.tenant_concurrency)
            {
                return Err(ConfigFailure(ConfigReason::Concurrency));
            }
            if match alias.kind {
                Kind::Embedding => !alias
                    .dimensions
                    .is_some_and(|n| (1..=crate::MAX_DIMENSIONS).contains(&n)),
                Kind::Rerank => alias.dimensions.is_some(),
            } {
                return Err(ConfigFailure(ConfigReason::Dimensions));
            }
            if alias.secret.as_ref().is_some_and(
                |source| matches!(source, SecretSource::Env(variable) if !name(variable)),
            ) {
                return Err(ConfigFailure(ConfigReason::SecretReference));
            }
            let secret = alias
                .secret
                .as_ref()
                .map(load_secret)
                .transpose()
                .map_err(|_| ConfigFailure(ConfigReason::SecretValue))?;
            let slots = Arc::new(Semaphore::new(alias.concurrency));
            let dns_limit = alias.concurrency;
            let key = alias.alias.clone();
            if registry
                .providers
                .insert(
                    key,
                    Provider {
                        definition: alias,
                        endpoint,
                        secret,
                        slots,
                        dns_slots: Arc::new(Semaphore::new(dns_limit)),
                        resolver: Arc::new(SystemResolver),
                    },
                )
                .is_some()
            {
                return Err(ConfigFailure(ConfigReason::DuplicateAlias));
            }
        }
        let mut tenant_slots = BTreeMap::new();
        for binding in config.bindings {
            let provider = registry
                .providers
                .get(&binding.alias)
                .ok_or(ConfigFailure(ConfigReason::BindingAlias))?;
            if !name(&binding.tenant)
                || binding.payload_fields.len() > 64
                || binding.payload_fields.iter().any(|f| !name(f))
                || binding.payload_fields.iter().collect::<BTreeSet<_>>().len()
                    != binding.payload_fields.len()
                || (provider.definition.kind == Kind::Embedding
                    && !binding.payload_fields.is_empty())
            {
                return Err(ConfigFailure(ConfigReason::BindingFields));
            }
            let slots = tenant_slots
                .entry((binding.tenant.clone(), binding.alias.clone()))
                .or_insert_with(|| Arc::new(Semaphore::new(provider.definition.tenant_concurrency)))
                .clone();
            let key = (
                binding.lineage,
                binding.collection_id,
                binding.tenant.clone(),
                binding.alias.clone(),
            );
            if registry
                .bindings
                .insert(
                    key,
                    Bound {
                        definition: binding,
                        slots,
                    },
                )
                .is_some()
            {
                return Err(ConfigFailure(ConfigReason::DuplicateBinding));
            }
        }
        Ok(registry)
    }

    fn bound(&self, key: &BindingKey, kind: Kind) -> Result<(&Provider, &Bound), ProviderError> {
        let bound = self.bindings.get(key).ok_or(ProviderError::NotConfigured)?;
        let provider = self
            .providers
            .get(&bound.definition.alias)
            .ok_or(ProviderError::NotConfigured)?;
        if provider.definition.kind != kind {
            return Err(ProviderError::InvalidInput);
        }
        Ok((provider, bound))
    }

    pub(crate) async fn embed_bound(
        &self,
        key: &BindingKey,
        input: &str,
    ) -> Result<Vec<f32>, ProviderError> {
        self.embed(key, input, self.transport.as_ref()).await
    }

    pub(crate) async fn rerank_bound(
        &self,
        key: &BindingKey,
        query: &[f32],
        candidates: &[Candidate],
        k: usize,
    ) -> Result<Vec<Ranked>, ProviderError> {
        self.rerank(key, query, candidates, k, self.transport.as_ref())
            .await
    }

    async fn embed(
        &self,
        key: &BindingKey,
        input: &str,
        transport: &dyn Transport,
    ) -> Result<Vec<f32>, ProviderError> {
        let started = Instant::now();
        let result = self.embed_inner(key, input, transport, started).await;
        self.record(key, started, 1, &result);
        result
    }

    async fn embed_inner(
        &self,
        key: &BindingKey,
        input: &str,
        transport: &dyn Transport,
        started: Instant,
    ) -> Result<Vec<f32>, ProviderError> {
        let (provider, bound) = self.bound(key, Kind::Embedding)?;
        if input.len() > 16_384 {
            return Err(ProviderError::InvalidInput);
        }
        let body = json!({"model":provider.definition.model, "version":provider.definition.version, "input":input});
        let response = call(provider, bound, body, started, transport).await?;
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Data {
            embedding: Vec<f32>,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Response {
            data: Vec<Data>,
        }
        let mut parsed: Response =
            serde_json::from_slice(&response).map_err(|_| ProviderError::InvalidResponse)?;
        if parsed.data.len() != 1 {
            return Err(ProviderError::InvalidResponse);
        }
        let vector = parsed.data.remove(0).embedding;
        check_embedding(
            &vector,
            provider
                .definition
                .dimensions
                .ok_or(ProviderError::InvalidResponse)?,
        )?;
        check_deadline(started, provider)?;
        Ok(vector)
    }

    async fn rerank(
        &self,
        key: &BindingKey,
        query: &[f32],
        candidates: &[Candidate],
        k: usize,
        transport: &dyn Transport,
    ) -> Result<Vec<Ranked>, ProviderError> {
        let started = Instant::now();
        let result = self
            .rerank_inner(key, query, candidates, k, transport, started)
            .await;
        self.record(key, started, candidates.len(), &result);
        result
    }

    async fn rerank_inner(
        &self,
        key: &BindingKey,
        query: &[f32],
        candidates: &[Candidate],
        k: usize,
        transport: &dyn Transport,
        started: Instant,
    ) -> Result<Vec<Ranked>, ProviderError> {
        let (provider, bound) = self.bound(key, Kind::Rerank)?;
        if k == 0
            || k > candidates.len()
            || candidates.len() > 1000
            || query.is_empty()
            || query.len() > crate::MAX_DIMENSIONS
            || query.iter().any(|v| !v.is_finite())
            || candidates
                .iter()
                .any(|c| c.vector.len() != query.len() || c.vector.iter().any(|v| !v.is_finite()))
            || candidates
                .iter()
                .map(|c| c.id)
                .collect::<BTreeSet<_>>()
                .len()
                != candidates.len()
        {
            return Err(ProviderError::InvalidInput);
        }
        #[derive(Serialize)]
        struct ApprovedCandidate<'a> {
            id: u64,
            vector: &'a [f32],
            payload: BTreeMap<&'a str, &'a Value>,
        }
        #[derive(Serialize)]
        struct Request<'a> {
            model: &'a str,
            version: &'a str,
            query: &'a [f32],
            candidates: Vec<ApprovedCandidate<'a>>,
            k: usize,
        }
        let records: Vec<_> = candidates
            .iter()
            .map(|c| {
                let payload: BTreeMap<_, _> = bound
                    .definition
                    .payload_fields
                    .iter()
                    .filter_map(|field| c.payload.get(field).map(|value| (field.as_str(), value)))
                    .collect();
                ApprovedCandidate {
                    id: c.id,
                    vector: &c.vector,
                    payload,
                }
            })
            .collect();
        let response = call(
            provider,
            bound,
            Request {
                model: &provider.definition.model,
                version: &provider.definition.version,
                query,
                candidates: records,
                k,
            },
            started,
            transport,
        )
        .await?;
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Response {
            results: Vec<Ranked>,
        }
        let mut parsed: Response =
            serde_json::from_slice(&response).map_err(|_| ProviderError::InvalidResponse)?;
        let ids: BTreeSet<_> = candidates.iter().map(|c| c.id).collect();
        check_ranked(&parsed.results, k, &ids)?;
        sort_ranked(&mut parsed.results);
        check_deadline(started, provider)?;
        Ok(parsed.results)
    }

    fn record<T>(
        &self,
        key: &BindingKey,
        started: Instant,
        batch: usize,
        result: &Result<T, ProviderError>,
    ) {
        use sha2::{Digest, Sha256};
        let Some(bound) = self.bindings.get(key) else {
            return;
        };
        let tenant = super::hex(&Sha256::digest(bound.definition.tenant.as_bytes()));
        super::logs::buffer().record("info", "providers", "provider operation completed".into(), json!({
            "alias":bound.definition.alias, "tenant_fingerprint":tenant,
            "duration_ms":started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            "batch_count":batch, "outcome":result.as_ref().map_or_else(|e| e.mapping().1, |_| "ok"),
        }));
    }
}

pub(crate) struct Candidate {
    pub(crate) id: u64,
    pub(crate) vector: Vec<f32>,
    pub(crate) payload: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ranked {
    pub(crate) id: u64,
    pub(crate) cost: f32,
}

fn check_deadline(started: Instant, provider: &Provider) -> Result<Duration, ProviderError> {
    Duration::from_millis(provider.definition.total_timeout_ms)
        .checked_sub(started.elapsed())
        .filter(|d| !d.is_zero())
        .ok_or(ProviderError::Timeout)
}

pub(crate) struct TransportRequest<'a> {
    provider: &'a Provider,
    body: &'a [u8],
    remaining: Duration,
}

pub(crate) struct TransportResponse {
    status: u16,
    body: Vec<u8>,
    retry_after: Option<Duration>,
}

pub(crate) struct TransportFailure {
    error: ProviderError,
    retryable: bool,
}

impl From<ProviderError> for TransportFailure {
    fn from(error: ProviderError) -> Self {
        Self {
            error,
            retryable: false,
        }
    }
}

type TransportFuture<'a> =
    Pin<Box<dyn Future<Output = Result<TransportResponse, TransportFailure>> + Send + 'a>>;

pub(crate) trait Transport: Send + Sync {
    fn send<'a>(&'a self, request: TransportRequest<'a>) -> TransportFuture<'a>;
}

pub(crate) struct HttpTransport;

impl Transport for HttpTransport {
    fn send<'a>(&'a self, request: TransportRequest<'a>) -> TransportFuture<'a> {
        Box::pin(async move {
            let provider = request.provider;
            let connect_started = Instant::now();
            let connect_budget = Duration::from_millis(provider.definition.connect_timeout_ms);
            let host = provider
                .endpoint
                .host_str()
                .ok_or(ProviderError::Unavailable)?;
            let port = provider
                .endpoint
                .port_or_known_default()
                .ok_or(ProviderError::Unavailable)?;
            let addresses = if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>() {
                vec![SocketAddr::new(ip, port)]
            } else {
                let resolver = provider.resolver.clone();
                let dns_slots = provider.dns_slots.clone();
                let host = host.to_owned();
                tokio::time::timeout(
                    connect_budget,
                    tokio::task::spawn_blocking(move || {
                        let _permit = dns_slots
                            .try_acquire_owned()
                            .map_err(|_| ProviderError::Busy)?;
                        resolver.resolve(&host, port)
                    }),
                )
                .await
                .map_err(|_| ProviderError::Timeout)?
                .map_err(|_| ProviderError::Unavailable)??
            };
            let addresses = check_addresses(
                &addresses,
                &provider.definition.endpoint_allowlist.addresses,
            )?;
            let client = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(
                    connect_budget
                        .checked_sub(connect_started.elapsed())
                        .filter(|budget| !budget.is_zero())
                        .ok_or(ProviderError::Timeout)?,
                )
                .timeout(request.remaining)
                .resolve_to_addrs(host, &addresses)
                .build()
                .map_err(|_| ProviderError::Unavailable)?;
            if connect_started.elapsed() >= request.remaining {
                return Err(ProviderError::Timeout.into());
            }
            let mut outgoing = client
                .post(provider.endpoint.clone())
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(request.body.to_vec());
            if let Some(secret) = &provider.secret {
                outgoing = outgoing.bearer_auth(secret);
            }
            let mut response = outgoing.send().await.map_err(http_error)?;
            let status = response.status().as_u16();
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .map(|value| {
                    value
                        .to_str()
                        .ok()
                        .and_then(|v| v.parse::<u64>().ok())
                        .map(Duration::from_secs)
                        .unwrap_or(Duration::MAX)
                });
            // Never read or log error bodies, including redirect locations.
            if !(200..300).contains(&status) {
                return Ok(TransportResponse {
                    status,
                    body: Vec::new(),
                    retry_after,
                });
            }
            if response
                .content_length()
                .is_some_and(|len| len > RESPONSE_BYTES as u64)
            {
                return Err(ProviderError::InvalidResponse.into());
            }
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|e| {
                if e.is_timeout() {
                    ProviderError::Timeout
                } else {
                    ProviderError::InvalidResponse
                }
            })? {
                if chunk.len() > RESPONSE_BYTES - body.len() {
                    return Err(ProviderError::InvalidResponse.into());
                }
                body.extend_from_slice(&chunk);
            }
            Ok(TransportResponse {
                status,
                body,
                retry_after,
            })
        })
    }
}

fn http_error(error: reqwest::Error) -> TransportFailure {
    TransportFailure {
        error: if error.is_timeout() {
            ProviderError::Timeout
        } else {
            ProviderError::Unavailable
        },
        retryable: error.is_connect() && !error.is_timeout(),
    }
}

struct RequestBuffer(Vec<u8>);

impl Write for RequestBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > REQUEST_BYTES - self.0.len() {
            return Err(std::io::Error::other("provider request limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

async fn call(
    provider: &Provider,
    bound: &Bound,
    body: impl Serialize,
    started: Instant,
    transport: &dyn Transport,
) -> Result<Vec<u8>, ProviderError> {
    let _provider_permit = provider
        .slots
        .try_acquire()
        .map_err(|_| ProviderError::Busy)?;
    let _tenant_permit = bound.slots.try_acquire().map_err(|_| ProviderError::Busy)?;
    let mut encoded = RequestBuffer(Vec::new());
    serde_json::to_writer(&mut encoded, &body).map_err(|_| ProviderError::InvalidInput)?;
    let body = encoded.0;
    for attempt in 0..=1 {
        let remaining = check_deadline(started, provider)?;
        let result = tokio::time::timeout(
            remaining,
            transport.send(TransportRequest {
                provider,
                body: &body,
                remaining,
            }),
        )
        .await
        .map_err(|_| ProviderError::Timeout)?;
        let (error, retry_after, retryable) = match result {
            Ok(response) if (200..300).contains(&response.status) => {
                if response.body.len() > RESPONSE_BYTES {
                    return Err(ProviderError::InvalidResponse);
                }
                check_deadline(started, provider)?;
                return Ok(response.body);
            }
            Ok(response) => (
                ProviderError::Unavailable,
                response.retry_after,
                matches!(response.status, 429 | 503),
            ),
            Err(failure) => (failure.error, None, failure.retryable),
        };
        if attempt != 0 || !provider.definition.replay_safe || !retryable {
            return Err(error);
        }
        let mut jitter = [0u8; 1];
        getrandom::fill(&mut jitter).map_err(|_| ProviderError::Unavailable)?;
        let delay = retry_after
            .unwrap_or_default()
            .saturating_add(Duration::from_millis(10 + u64::from(jitter[0]) % 41));
        if delay >= check_deadline(started, provider)? {
            return Err(ProviderError::Timeout);
        }
        tokio::time::sleep(delay).await;
    }
    unreachable!()
}

#[cfg(test)]
mod tests;
