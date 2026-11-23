//! Gateway-trusted backend mode (feature-aca-ha ITEM-010/014/015, SEC-002):
//! `vlite-server <db> --gateway-trust-file <json>`.
//!
//! A trusted backend authenticates a request only by a principal envelope a
//! gateway signed; bearer tokens, sessions, the open development mode and
//! network position grant nothing. It decides independently of the gateway:
//! the envelope's signature, audience, lifetime, skew and generation `A`
//! (`principal_envelope::Verifier`), and, from its own fresh copy of the
//! shared control state, that the envelope's tenant has a directory record
//! naming this backend's audience and database lineage and is admitted
//! (suspended: 403). One `shared-pool` backend so serves every tenant routed
//! to it, each still confined to its own collections by the credential
//! scope, and a dedicated backend serves only its tenant.
//!
//! Its poll thread raises the verifier's minimum `A` to every generation it
//! confirms, independently of request arrival, so an envelope minted before
//! a revocation or suspension is refused once the backend has polled it.
//! It fails closed when its copy is older than S_ctl, and refuses a
//! generation regression. `/ready` (unauthenticated in this mode) reports
//! the generation, or 503 when stale or draining.
//!
//! design-tenant-provisioning ITEM-006: a trust file with
//! `control_projection` instead of `control_state` decides from its one
//! tenant's control projection (`tenant_provisioning::projection`), never
//! the shared control state. Its start is refused without a qualified clock
//! proof, which nothing outside tests can supply yet (OD-005).

use super::cloud_backup::ObjectStore;
use super::control_state::{Feed, Source};
use super::principal_envelope::{Policy, Refusal, Verifier};
use super::tenant_provisioning::projection::{ClockProof, Reader};
use super::*;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// Trust and gateway configuration files are small; refuse anything larger.
pub(super) const MAX_CONFIG_BYTES: u64 = 64 * 1024;
/// Any configured duration above an hour is refused as a typo.
pub(super) const MAX_MS: u64 = 3_600_000;

/// Reads at most `max` bytes of `path`; a larger file is refused.
pub(super) fn read_bounded(path: &Path, max: u64, what: &str) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(max + 1).read_to_end(&mut bytes))
        .map_err(|error| format!("cannot read {what}: {}", error.kind()))?;
    if bytes.len() as u64 > max {
        return Err(format!("{what} is larger than {max} bytes"));
    }
    Ok(bytes)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustFile {
    /// The envelope audience this backend verifies; directory records for
    /// the tenants it serves name it.
    audience: String,
    public_key_bundle_file: PathBuf,
    /// Exactly one of `control_state` and `control_projection`.
    #[serde(default)]
    control_state: Option<Source>,
    #[serde(default)]
    control_projection: Option<ProjectionSource>,
    s_ctl_ms: u64,
    poll_interval_ms: u64,
    skew_ms: u64,
    max_lifetime_ms: u64,
    /// How long `/ready` reports draining before the listener closes.
    readiness_propagation_ms: u64,
}

/// design-tenant-provisioning ITEM-006: decide from `tenant`'s control
/// projection, the object `store.object` under the coordination prefix,
/// instead of the shared control state. It carries no clock evidence.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectionSource {
    store: Source,
    tenant: String,
}

/// The validated control view of a trust file.
enum ViewSource {
    Full(Source),
    Projection(ProjectionSource),
}

/// A validated `--gateway-trust-file`: every field and the public key bundle
/// are checked; the store is not contacted yet.
pub struct GatewayTrustConfig {
    file: TrustFile,
    view: ViewSource,
    verifier: Verifier,
}

fn invalid(detail: impl std::fmt::Display) -> String {
    format!("gateway_trust_invalid: {detail}")
}

impl GatewayTrustConfig {
    pub fn read(path: &Path) -> Result<Self, String> {
        let bytes = read_bounded(path, MAX_CONFIG_BYTES, "trust file").map_err(invalid)?;
        let mut file: TrustFile = serde_json::from_slice(&bytes).map_err(invalid)?;
        let text = |value: &str| {
            !value.is_empty() && value.len() <= 2048 && !value.chars().any(char::is_control)
        };
        if !text(&file.audience) {
            return Err(invalid("audience must be 1..=2048 printable characters"));
        }
        let view = match (file.control_state.take(), file.control_projection.take()) {
            (Some(state), None) => {
                state.validate().map_err(invalid)?;
                ViewSource::Full(state)
            }
            (None, Some(projection)) => {
                if !tenant_provisioning::is_hex(&projection.tenant, 32) {
                    return Err(invalid(
                        "control_projection.tenant must be 32 lowercase hex",
                    ));
                }
                projection.store.validate().map_err(|_| {
                    invalid(
                        "control_projection.store.object must be a relative [a-z0-9._/-] \
                         name outside the set protocol",
                    )
                })?;
                ViewSource::Projection(projection)
            }
            _ => {
                return Err(invalid(
                    "exactly one of control_state or control_projection is required",
                ));
            }
        };
        for (name, value) in [
            ("s_ctl_ms", file.s_ctl_ms),
            ("poll_interval_ms", file.poll_interval_ms),
            ("skew_ms", file.skew_ms),
            ("max_lifetime_ms", file.max_lifetime_ms),
            ("readiness_propagation_ms", file.readiness_propagation_ms),
        ] {
            if value > MAX_MS {
                return Err(invalid(format!("{name} must be at most {MAX_MS}")));
            }
        }
        if file.poll_interval_ms == 0 || file.poll_interval_ms >= file.s_ctl_ms {
            return Err(invalid(
                "poll_interval_ms must be positive and below s_ctl_ms",
            ));
        }
        if file.max_lifetime_ms == 0 {
            return Err(invalid("max_lifetime_ms must be positive"));
        }
        let bundle = read_bounded(
            &file.public_key_bundle_file,
            MAX_CONFIG_BYTES,
            "public key bundle",
        )
        .map_err(invalid)?;
        let verifier = Verifier::from_bundle(
            &bundle,
            Policy {
                audience: file.audience.clone(),
                // The tenant rule is the snapshot's directory (`authenticate`).
                tenant: None,
                skew_ms: file.skew_ms,
                max_lifetime_ms: file.max_lifetime_ms,
                minimum_generation: 1,
            },
        )
        .map_err(invalid)?;
        Ok(Self {
            file,
            view,
            verifier,
        })
    }
}

/// A started trust: connected, read once, polling.
#[derive(Clone)]
pub struct GatewayTrust(Arc<Trust>);

pub(super) struct Trust {
    verifier: Verifier,
    view: View,
    audience: String,
    propagation: Duration,
    draining: AtomicBool,
}

/// What a trusted backend decides from: the shared control state, or
/// (design-tenant-provisioning ITEM-006, by the trust file's
/// `control_projection`) its tenant's control projection. The projection
/// view refuses every decision without a qualified clock proof (OD-005).
enum View {
    Full(Arc<Feed>),
    Projection(Arc<Reader>),
}

impl GatewayTrust {
    /// Blocking (run off the async runtime): connects to the control-state
    /// (or projection) store, requires one confirmed read, then starts the
    /// poll thread that also raises the minimum accepted `A`.
    pub fn start(config: GatewayTrustConfig) -> Result<Self, String> {
        let GatewayTrustConfig {
            file,
            view,
            verifier,
        } = config;
        let source = match view {
            ViewSource::Full(source) => source,
            ViewSource::Projection(projection) => {
                let (store, prefix) = projection
                    .store
                    .coordination()
                    .map_err(|error| format!("control projection store: {error}"))?;
                // No approved clock-proof source exists (OD-005) and nothing
                // in the trust file is clock evidence: the reader refuses
                // its first poll, so this backend never serves.
                return Self::start_projection(file, verifier, projection, store, &prefix, None);
            }
        };
        let objects = source
            .connect()
            .map_err(|error| format!("control state store: {error}"))?;
        let feed =
            Arc::new(Feed::new(objects, file.s_ctl_ms, file.poll_interval_ms).map_err(invalid)?);
        let generation = feed
            .poll_once()
            .map_err(|error| format!("{}: {}", error.code, error.message))?;
        let trust = Arc::new(Trust {
            verifier,
            view: View::Full(feed.clone()),
            audience: file.audience,
            propagation: Duration::from_millis(file.readiness_propagation_ms),
            draining: AtomicBool::new(false),
        });
        trust.verifier.raise_minimum(generation);
        let weak = Arc::downgrade(&trust);
        feed.start(move |generation| {
            if let Some(trust) = weak.upgrade() {
                trust.verifier.raise_minimum(generation);
            }
        })
        .map_err(|error| format!("cannot start the control state poller: {error}"))?;
        let keys = trust.verifier.metadata();
        log_stderr(
            "info",
            "gateway_trust",
            json!({"event":"gateway_trust_started","audience":trust.audience,
                "key_generation":keys.generation,"kids":keys.kids,"generation":generation})
            .to_string(),
        );
        Ok(Self(trust))
    }

    /// The projection view's start: one accepted projection read (refused
    /// without `clock`), then the poll thread raising the minimum `A`.
    fn start_projection(
        file: TrustFile,
        verifier: Verifier,
        projection: ProjectionSource,
        store: Arc<dyn ObjectStore>,
        prefix: &str,
        clock: Option<ClockProof>,
    ) -> Result<Self, String> {
        let name = format!("{prefix}{}", projection.store.object);
        let reader = Arc::new(
            Reader::new(store, name, &projection.tenant, file.s_ctl_ms, clock).map_err(invalid)?,
        );
        let generation = reader
            .poll()
            .map_err(|error| format!("{}: {}", error.code, error.message))?;
        let trust = Arc::new(Trust {
            verifier,
            view: View::Projection(reader.clone()),
            audience: file.audience,
            propagation: Duration::from_millis(file.readiness_propagation_ms),
            draining: AtomicBool::new(false),
        });
        trust.verifier.raise_minimum(generation);
        let weak = Arc::downgrade(&trust);
        reader
            .start(
                Duration::from_millis(file.poll_interval_ms),
                move |polled| match polled {
                    Ok(generation) => {
                        if let Some(trust) = weak.upgrade() {
                            trust.verifier.raise_minimum(generation);
                        }
                    }
                    Err(error) => log_stderr(
                        "warn",
                        "gateway_trust",
                        json!({"event":"control_projection_poll_failed","code":error.code})
                            .to_string(),
                    ),
                },
            )
            .map_err(|error| format!("cannot start the control projection poller: {error}"))?;
        let keys = trust.verifier.metadata();
        log_stderr(
            "info",
            "gateway_trust",
            json!({"event":"gateway_trust_started","view":"projection",
                "audience":trust.audience,"key_generation":keys.generation,"kids":keys.kids,
                "generation":generation})
            .to_string(),
        );
        Ok(Self(trust))
    }

    /// `start` of a `control_projection` config on `store` (prefix "")
    /// with a test-only `clock`.
    #[cfg(test)]
    pub(super) fn start_projection_on(
        config: GatewayTrustConfig,
        store: Arc<dyn ObjectStore>,
        clock: Option<ClockProof>,
    ) -> Result<Self, String> {
        let GatewayTrustConfig {
            file,
            view,
            verifier,
        } = config;
        let ViewSource::Projection(projection) = view else {
            return Err("not a control_projection trust file".to_owned());
        };
        Self::start_projection(file, verifier, projection, store, "", clock)
    }

    /// From now on `/ready` answers 503 `draining`; the caller then waits
    /// `readiness_propagation` before closing the listener.
    pub fn begin_drain(&self) {
        self.0.draining.store(true, Ordering::SeqCst);
        log_stderr(
            "info",
            "gateway_trust",
            json!({"event":"draining","propagation_ms":self.0.propagation.as_millis() as u64})
                .to_string(),
        );
    }

    pub fn readiness_propagation(&self) -> Duration {
        self.0.propagation
    }

    /// Stops and joins the poll thread (blocking; at most one read).
    pub fn stop(&self) {
        match &self.0.view {
            View::Full(feed) => feed.stop(),
            View::Projection(reader) => reader.stop(),
        }
    }
}

impl Service {
    /// Serves in gateway-trusted mode with `trust` (feature-aca-ha
    /// ITEM-010/014). Configure before cloning the service.
    pub fn with_gateway_trust(mut self, trust: &GatewayTrust) -> Self {
        self.trust = Some(trust.0.clone());
        self
    }
}

impl Trust {
    #[cfg(test)]
    pub(super) fn for_test(verifier: Verifier, feed: Arc<Feed>, audience: &str) -> Self {
        Self {
            verifier,
            view: View::Full(feed),
            audience: audience.to_owned(),
            propagation: Duration::ZERO,
            draining: AtomicBool::new(false),
        }
    }

    /// The request's role: `None` without an envelope (then 401), the
    /// envelope's scoped grant when this backend serves its tenant.
    /// `lineage` is this backend's database lineage. Under a projection's
    /// pending admission only the operation's read-only validator is served.
    pub(super) fn authenticate(
        &self,
        headers: &HeaderMap,
        lineage: Option<u64>,
    ) -> Result<Option<admin::Role>, ApiError> {
        let mut values = headers.get_all(principal_envelope::HEADER).iter();
        let Some(value) = values.next() else {
            return Ok(None);
        };
        let token = match (value.to_str(), values.next()) {
            (Ok(token), None) => token,
            _ => return Err(Refusal::Malformed.error()),
        };
        let now = admin::now_ms() as u64;
        let grant = match &self.view {
            View::Full(feed) => {
                let snapshot = feed.current()?;
                let grant = self
                    .verifier
                    .verify_with(token, now, |tenant| {
                        tenant
                            .and_then(|tenant| snapshot.record_for(tenant))
                            .is_some_and(|record| {
                                record.audience == self.audience && lineage == Some(record.lineage)
                            })
                    })
                    .map_err(Refusal::error)?;
                let tenant = grant
                    .tenant_id
                    .as_deref()
                    .ok_or_else(|| Refusal::Tenant.error())?;
                snapshot.admit(tenant)?;
                grant
            }
            View::Projection(reader) => {
                let projection = reader.current()?;
                let grant = self
                    .verifier
                    .verify_with(token, now, |tenant| {
                        projection.serves(tenant, &self.audience, lineage)
                    })
                    .map_err(Refusal::error)?;
                projection.admit_grant(&grant)?;
                grant
            }
        };
        Ok(Some(admin::Role::Scoped(grant)))
    }

    /// The tenant quota from the current view (fail closed when stale).
    pub(super) fn tenant_quota(&self, tenant: &str) -> Result<Option<tenants::Tenant>, ApiError> {
        match &self.view {
            View::Full(feed) => feed.current()?.tenant_quota(tenant),
            View::Projection(reader) => reader.current()?.tenant_quota(tenant),
        }
    }

    /// Admits `tenant` (known, not suspended) against the current view
    /// (fail closed when stale), as the tenant store does standalone.
    pub(super) fn admit(&self, tenant: &str) -> Result<(), ApiError> {
        match &self.view {
            View::Full(feed) => feed.current()?.admit(tenant),
            View::Projection(reader) => reader.current()?.admit(tenant),
        }
    }

    /// `/ready` in this mode.
    pub(super) fn ready(&self) -> Result<Value, ApiError> {
        if self.draining.load(Ordering::SeqCst) {
            return Err(draining());
        }
        let generation = match &self.view {
            View::Full(feed) => feed.current()?.generation(),
            View::Projection(reader) => reader.current()?.source_generation(),
        };
        Ok(json!({"status":"ready","generation":generation,
            "minimum_generation":self.verifier.minimum()}))
    }
}

pub(super) fn draining() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "draining",
        "the server is shutting down; route new requests elsewhere",
    )
}
