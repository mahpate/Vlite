//! Gateway operation classifier (feature-aca-ha ITEM-013, FR-002, TEST-002).
//!
//! A gateway must decide where a request may go before any backend sees it,
//! so it classifies by operation, not HTTP method: GET/HEAD and the
//! search-like POST routes are reads, through the same `read_request` rule
//! that the read-only token and reader mode use, and the scoped-credential
//! requirement comes from the same `permission` table. Only the registered
//! REST routes are known (`ROUTES`, which `tests/api_route_inventory.py`
//! keeps equal to the router); anything else is refused (deny-unmapped),
//! never forwarded as a read.
//!
//! Paths match templates the way the router matches them: the path alone
//! chooses the template, a literal segment beating a parameter at the first
//! position where two candidates differ, and only then is the method checked
//! (HEAD is served by GET routes).
//!
//! It also sizes each replica's slice of a credential rate limit
//! (`replica_share`, ITEM-016, PRD section 3.4.7).
//!
//! Routing (ITEM-014, PRD sections 3.4.6, 3.4.8 and 3.7): `route_target`
//! picks the backend for an authenticated tenant from the control-state
//! directory and `mint` signs that backend's principal envelope; the
//! gateway mode (`gateway_mode`, `vlite-server gateway`) calls both on every
//! forwarded request, and a gateway-trusted backend (`gateway_trust`)
//! accepts an envelope only for a tenant whose directory record names its
//! audience and lineage, so one `shared-pool` backend serves several
//! tenants. The target is always the writer; reader routing is ITEM-021.
//! Untenanted and super-admin requests have no route (OD-004/OD-012).
//!
//! Not yet used by the gateway mode: it never retries, so
//! `Delivery::may_retry` is unused, and no backend stamps an epoch on its
//! responses, so `check_epoch` is not applied (OD-002 fencing is open).

use axum::http::{Method, StatusCode};

use super::{
    ApiError, Need, control_state, credentials, permission, principal_envelope, read_request,
};

/// Whether a classified request only reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Operation {
    Read,
    Write,
}

/// Why a request is refused before it could be forwarded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Refusal {
    /// No registered route template matches the path.
    UnknownRoute,
    /// A template matches the path, but not with this method.
    MethodNotAllowed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Classification {
    /// The matched route template, spelled as the router reports it.
    pub(super) template: &'static str,
    /// Served without authentication (the `observe` public list).
    pub(super) public: bool,
    pub(super) operation: Operation,
    /// What a scoped credential needs (`permission`).
    pub(super) need: Need,
}

/// Every registered REST route (179) as (method, template).
const ROUTES: &[(&str, &str)] = &[
    ("GET", "/dashboard"),
    ("GET", "/dashboard/admin-tools.js"),
    ("GET", "/dashboard/ambience.js"),
    ("GET", "/dashboard/app.js"),
    ("GET", "/dashboard/console-tools.js"),
    ("GET", "/dashboard/fonts/OFL-JetBrainsMono.txt"),
    ("GET", "/dashboard/fonts/OFL-Outfit.txt"),
    (
        "GET",
        "/dashboard/fonts/jetbrains-mono-latin-wght-normal.woff2",
    ),
    ("GET", "/dashboard/fonts/outfit-latin-wght-normal.woff2"),
    ("GET", "/dashboard/graph.css"),
    ("GET", "/dashboard/graph.js"),
    ("GET", "/dashboard/graph3d.js"),
    ("GET", "/dashboard/navigation.js"),
    ("GET", "/dashboard/observability.js"),
    ("GET", "/dashboard/style.css"),
    ("GET", "/dashboard/theme.css"),
    ("GET", "/dashboard/three.LICENSE"),
    ("GET", "/dashboard/three.min.js"),
    ("GET", "/health"),
    ("GET", "/metrics"),
    ("GET", "/openapi.json"),
    ("GET", "/ready"),
    ("GET", "/v1/admin/audit"),
    ("GET", "/v1/admin/audit/persistent"),
    ("GET", "/v1/admin/backups"),
    ("POST", "/v1/admin/backups"),
    ("GET", "/v1/admin/backups/schedule"),
    ("PUT", "/v1/admin/backups/schedule"),
    ("POST", "/v1/admin/backups/schedule/run"),
    ("POST", "/v1/admin/backups/{name}/drill"),
    ("POST", "/v1/admin/backups/{name}/verify"),
    ("GET", "/v1/admin/cloud-backup"),
    ("PUT", "/v1/admin/cloud-backup"),
    ("POST", "/v1/admin/cloud-backup/collect"),
    ("POST", "/v1/admin/cloud-backup/run"),
    ("GET", "/v1/admin/cloud-backup/sets"),
    ("POST", "/v1/admin/cloud-backup/sets/{id}/drill"),
    ("POST", "/v1/admin/cloud-backup/sets/{id}/pins"),
    ("DELETE", "/v1/admin/cloud-backup/sets/{id}/pins/{pin}"),
    ("POST", "/v1/admin/cloud-backup/sets/{id}/restore"),
    ("GET", "/v1/admin/compaction"),
    ("PUT", "/v1/admin/compaction/config"),
    ("POST", "/v1/admin/compaction/jobs"),
    ("GET", "/v1/admin/compaction/jobs/{id}"),
    ("POST", "/v1/admin/compaction/jobs/{id}/cancel"),
    ("GET", "/v1/admin/credentials"),
    ("POST", "/v1/admin/credentials"),
    ("POST", "/v1/admin/credentials/{id}/revoke"),
    ("POST", "/v1/admin/credentials/{id}/rotate"),
    ("GET", "/v1/admin/diagnostics/slow-queries"),
    ("GET", "/v1/admin/graph/stats"),
    ("GET", "/v1/admin/logs"),
    ("GET", "/v1/admin/maintenance/auto"),
    ("PUT", "/v1/admin/maintenance/auto"),
    ("POST", "/v1/admin/maintenance/auto/tick"),
    ("POST", "/v1/admin/memory/bindings/preflight"),
    ("POST", "/v1/admin/memory/bindings/reload"),
    ("GET", "/v1/admin/memory/bindings/status"),
    ("GET", "/v1/admin/metrics/summary"),
    ("GET", "/v1/admin/replication/export"),
    ("POST", "/v1/admin/restores"),
    ("POST", "/v1/admin/restores/select"),
    ("GET", "/v1/admin/serve-status"),
    ("GET", "/v1/admin/tenants"),
    ("POST", "/v1/admin/tenants"),
    ("DELETE", "/v1/admin/tenants/{id}"),
    ("POST", "/v1/admin/tenants/{id}/resume"),
    ("PUT", "/v1/admin/tenants/{id}/settings"),
    ("POST", "/v1/admin/tenants/{id}/suspend"),
    ("GET", "/v1/admin/tenants/{id}/usage"),
    ("POST", "/v1/admin/tls/reload"),
    ("GET", "/v1/admin/tokens"),
    ("POST", "/v1/admin/tokens/reload"),
    ("POST", "/v1/admin/tokens/revoke-read-only"),
    ("GET", "/v1/admin/users"),
    ("POST", "/v1/admin/users"),
    ("DELETE", "/v1/admin/users/{id}"),
    ("PUT", "/v1/admin/users/{id}"),
    ("POST", "/v1/admin/users/{id}/password"),
    ("GET", "/v1/admin/webhooks"),
    ("POST", "/v1/admin/webhooks"),
    ("DELETE", "/v1/admin/webhooks/{id}"),
    ("GET", "/v1/admin/webhooks/{id}"),
    ("GET", "/v1/auth/config"),
    ("POST", "/v1/auth/login"),
    ("POST", "/v1/auth/logout"),
    ("POST", "/v1/auth/password"),
    ("GET", "/v1/auth/session"),
    ("POST", "/v1/batches"),
    ("GET", "/v1/capabilities"),
    ("GET", "/v1/changes"),
    ("GET", "/v1/collections"),
    ("POST", "/v1/collections"),
    ("DELETE", "/v1/collections/{collection}"),
    ("GET", "/v1/collections/{collection}"),
    ("POST", "/v1/collections/{collection}/batches"),
    ("POST", "/v1/collections/{collection}/embed"),
    ("POST", "/v1/collections/{collection}/facets"),
    ("POST", "/v1/collections/{collection}/imports"),
    ("POST", "/v1/collections/{collection}/index/disk-ann"),
    ("GET", "/v1/collections/{collection}/index/hnsw"),
    ("POST", "/v1/collections/{collection}/index/hnsw"),
    ("GET", "/v1/collections/{collection}/index/hnsw/maintenance"),
    (
        "POST",
        "/v1/collections/{collection}/index/hnsw/maintenance/tick",
    ),
    ("POST", "/v1/collections/{collection}/index/hnsw/quality"),
    ("GET", "/v1/collections/{collection}/index/hnsw/rebuild"),
    ("POST", "/v1/collections/{collection}/index/hnsw/rebuild"),
    (
        "POST",
        "/v1/collections/{collection}/index/hnsw/rebuild/cancel",
    ),
    (
        "POST",
        "/v1/collections/{collection}/index/hnsw/rebuild/poll",
    ),
    ("POST", "/v1/collections/{collection}/kv/batch"),
    ("GET", "/v1/collections/{collection}/kv/entries"),
    ("POST", "/v1/collections/{collection}/kv/entries/get"),
    ("DELETE", "/v1/collections/{collection}/kv/entry"),
    ("GET", "/v1/collections/{collection}/kv/entry"),
    ("PUT", "/v1/collections/{collection}/kv/entry"),
    ("POST", "/v1/collections/{collection}/query"),
    ("POST", "/v1/collections/{collection}/recommend"),
    ("POST", "/v1/collections/{collection}/search"),
    ("POST", "/v1/collections/{collection}/search/dense-sparse"),
    ("POST", "/v1/collections/{collection}/search/disk-ann"),
    ("POST", "/v1/collections/{collection}/search/geo"),
    ("POST", "/v1/collections/{collection}/search/grouped"),
    ("POST", "/v1/collections/{collection}/search/named"),
    ("POST", "/v1/collections/{collection}/search/sparse"),
    ("PUT", "/v1/collections/{collection}/settings"),
    ("POST", "/v1/collections/{collection}/stats/numeric"),
    ("GET", "/v1/collections/{collection}/vectors"),
    ("POST", "/v1/collections/{collection}/vectors/configure"),
    ("DELETE", "/v1/collections/{collection}/vectors/{id}"),
    ("GET", "/v1/collections/{collection}/vectors/{id}"),
    ("PUT", "/v1/collections/{collection}/vectors/{id}"),
    (
        "PATCH",
        "/v1/collections/{collection}/vectors/{id}/metadata",
    ),
    ("PUT", "/v1/collections/{collection}/vectors/{id}/named"),
    ("GET", "/v1/database"),
    ("GET", "/v1/graphs"),
    ("POST", "/v1/graphs"),
    ("DELETE", "/v1/graphs/{graph}"),
    ("GET", "/v1/graphs/{graph}"),
    ("POST", "/v1/graphs/{graph}/batch"),
    ("POST", "/v1/graphs/{graph}/edges/get"),
    ("POST", "/v1/graphs/{graph}/explain"),
    ("POST", "/v1/graphs/{graph}/neighbors"),
    ("POST", "/v1/graphs/{graph}/nodes/find"),
    ("POST", "/v1/graphs/{graph}/nodes/get"),
    ("POST", "/v1/graphs/{graph}/query"),
    ("POST", "/v1/graphs/{graph}/retrieve"),
    ("GET", "/v1/graphs/{graph}/schema"),
    ("PUT", "/v1/graphs/{graph}/schema"),
    ("POST", "/v1/graphs/{graph}/shortest-path"),
    ("POST", "/v1/graphs/{graph}/traverse"),
    ("GET", "/v1/imports/{job}"),
    ("POST", "/v1/imports/{job}/abort"),
    ("PUT", "/v1/imports/{job}/chunks/{chunk}"),
    ("POST", "/v1/imports/{job}/finish"),
    ("GET", "/v1/index/hnsw"),
    ("POST", "/v1/index/hnsw"),
    ("GET", "/v1/index/hnsw/maintenance"),
    ("PUT", "/v1/index/hnsw/maintenance"),
    ("POST", "/v1/index/hnsw/maintenance/tick"),
    ("POST", "/v1/index/hnsw/quality"),
    ("GET", "/v1/index/hnsw/rebuild"),
    ("POST", "/v1/index/hnsw/rebuild"),
    ("POST", "/v1/index/hnsw/rebuild/cancel"),
    ("POST", "/v1/index/hnsw/rebuild/poll"),
    ("POST", "/v1/memory/{binding}/consolidate"),
    ("POST", "/v1/memory/{binding}/erasures/get"),
    ("POST", "/v1/memory/{binding}/erasures/resume"),
    ("POST", "/v1/memory/{binding}/facts/correct"),
    ("POST", "/v1/memory/{binding}/facts/query"),
    ("POST", "/v1/memory/{binding}/forget"),
    ("POST", "/v1/memory/{binding}/recall"),
    ("POST", "/v1/memory/{binding}/receipts/lookup"),
    ("POST", "/v1/memory/{binding}/remember"),
    ("POST", "/v1/search"),
    ("GET", "/v1/vectors"),
    ("DELETE", "/v1/vectors/{id}"),
    ("GET", "/v1/vectors/{id}"),
    ("PUT", "/v1/vectors/{id}"),
    ("PATCH", "/v1/vectors/{id}/metadata"),
];

/// Paths answered without authentication; equal to the list in `observe`.
pub(super) const PUBLIC: &[&str] = &[
    "/health",
    "/dashboard",
    "/dashboard/app.js",
    "/dashboard/style.css",
    "/dashboard/theme.css",
    "/dashboard/console-tools.js",
    "/dashboard/admin-tools.js",
    "/dashboard/graph.js",
    "/dashboard/graph.css",
    "/dashboard/graph3d.js",
    "/dashboard/ambience.js",
    "/dashboard/three.min.js",
    "/dashboard/three.LICENSE",
    "/dashboard/fonts/outfit-latin-wght-normal.woff2",
    "/dashboard/fonts/jetbrains-mono-latin-wght-normal.woff2",
    "/dashboard/fonts/OFL-Outfit.txt",
    "/dashboard/fonts/OFL-JetBrainsMono.txt",
    "/dashboard/observability.js",
    "/dashboard/navigation.js",
    "/v1/auth/config",
    "/v1/auth/login",
];

/// Classifies a request by its method and path (without the query), or
/// refuses it.
pub(super) fn classify(method: &Method, path: &str) -> Result<Classification, Refusal> {
    let segments = split(path).ok_or(Refusal::UnknownRoute)?;
    let mut best: Option<&'static str> = None;
    for &(_, template) in ROUTES {
        if matches(template, &segments) && best.is_none_or(|chosen| beats(template, chosen)) {
            best = Some(template);
        }
    }
    let template = best.ok_or(Refusal::UnknownRoute)?;
    let serves = |wanted: &str| {
        ROUTES
            .iter()
            .any(|&(method, candidate)| candidate == template && method == wanted)
    };
    if !(serves(method.as_str()) || (method == Method::HEAD && serves("GET"))) {
        return Err(Refusal::MethodNotAllowed);
    }
    Ok(Classification {
        template,
        public: PUBLIC.contains(&path),
        operation: if read_request(method, template, path) {
            Operation::Read
        } else {
            Operation::Write
        },
        need: permission(method, template),
    })
}

/// The path's segments, or `None` for a path no route serves: empty,
/// relative, with a trailing slash or with an empty segment.
fn split(path: &str) -> Option<Vec<&str>> {
    let segments: Vec<&str> = path.strip_prefix('/')?.split('/').collect();
    segments
        .iter()
        .all(|segment| !segment.is_empty())
        .then_some(segments)
}

fn parts(template: &str) -> impl Iterator<Item = &str> {
    template[1..].split('/')
}

fn is_param(part: &str) -> bool {
    part.starts_with('{') && part.ends_with('}')
}

fn matches(template: &str, segments: &[&str]) -> bool {
    parts(template).count() == segments.len()
        && parts(template)
            .zip(segments)
            .all(|(part, segment)| is_param(part) || part == *segment)
}

/// Whether `candidate` is preferred to `chosen` for a path both match: at the
/// first position where one has a literal and the other a parameter, the
/// literal wins.
fn beats(candidate: &str, chosen: &str) -> bool {
    parts(candidate)
        .zip(parts(chosen))
        .find(|(a, b)| is_param(a) != is_param(b))
        .is_some_and(|(a, _)| !is_param(a))
}

/// Why a credential limit has no per-replica share.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ShareRefusal {
    /// The replica maximum is zero.
    NoReplicas,
    /// The share is under one request a second. A bucket holding one second
    /// of that rate could never admit a request, and rounding it up to one
    /// would let the replicas together exceed the limit.
    BelowOneRequest,
}

/// Each gateway replica's share of a credential's `requests_per_second`,
/// used as both its refill rate and its bucket capacity (the backend bucket
/// holds one second of requests). The largest value whose `max_replicas`
/// multiples stay within the limit: never rounded up. If ACA briefly runs
/// more replicas than the maximum, the aggregate exceeds the limit by that
/// ratio; the backend's own bucket still applies.
pub(super) fn replica_share(
    requests_per_second: u32,
    max_replicas: u32,
) -> Result<f64, ShareRefusal> {
    if max_replicas == 0 {
        return Err(ShareRefusal::NoReplicas);
    }
    let (limit, replicas) = (f64::from(requests_per_second), f64::from(max_replicas));
    let mut share = limit / replicas;
    // Division rounds to nearest; `mul_add` keeps the product's sign exact.
    if share.mul_add(replicas, -limit) > 0.0 {
        share = share.next_down();
    }
    if share < 1.0 {
        return Err(ShareRefusal::BelowOneRequest);
    }
    Ok(share)
}

/// Where the gateway sends an authenticated request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Target {
    /// Base URL of the receiving backend.
    pub(super) endpoint: String,
    /// The envelope audience that backend verifies.
    pub(super) audience: String,
    /// `E` the backend must stamp on its response; `None` for `shared-pool`,
    /// which has no failover to fence. Not checked yet (OD-002).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) epoch: Option<u64>,
    pub(super) operation: Operation,
}

/// Why an authenticated request has no backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RouteRefusal {
    /// The credential has no tenant; how such requests pass a gateway is
    /// still open (OD-004, OD-012).
    NoTenant,
    /// The tenant has no directory record.
    NoRecord,
}

impl RouteRefusal {
    pub(super) fn error(self) -> ApiError {
        match self {
            Self::NoTenant => ApiError::new(
                StatusCode::FORBIDDEN,
                "gateway_tenant_required",
                "this gateway serves tenant credentials only",
            ),
            Self::NoRecord => ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "tenant_not_routed",
                "the tenant has no backend yet; ask an administrator",
            ),
        }
    }
}

/// The backend for `grant`'s request, from the directory in `snapshot`
/// (the snapshot that authenticated `grant`).
pub(super) fn route_target(
    classification: Classification,
    grant: &credentials::Grant,
    snapshot: &control_state::Snapshot,
) -> Result<Target, RouteRefusal> {
    let tenant = grant.tenant_id.as_deref().ok_or(RouteRefusal::NoTenant)?;
    target_for(classification, snapshot.record_for(tenant))
}

/// Every request goes to the writer: a write must reach the writer whose
/// epoch the record names (PRD section 3.7), and reads stay there until
/// reader routing (ITEM-021) acts on `read_routing`.
fn target_for(
    classification: Classification,
    record: Option<&control_state::Route>,
) -> Result<Target, RouteRefusal> {
    let record = record.ok_or(RouteRefusal::NoRecord)?;
    Ok(Target {
        endpoint: record.writer.clone(),
        audience: record.audience.clone(),
        epoch: record.epoch,
        operation: classification.operation,
    })
}

/// The principal envelope for `grant` to `target`, valid from `now` for
/// `lifetime_ms` and carrying control-state generation `A` (RD-004).
pub(super) fn mint(
    signer: &principal_envelope::Signer,
    target: &Target,
    grant: &credentials::Grant,
    generation: u64,
    now: u64,
    lifetime_ms: u64,
) -> Result<String, &'static str> {
    if lifetime_ms == 0 {
        return Err("an envelope needs a positive lifetime");
    }
    let exp = now
        .checked_add(lifetime_ms)
        .ok_or("envelope expiry overflows")?;
    signer.sign(&principal_envelope::Claims {
        aud: target.audience.clone(),
        iat: now,
        exp,
        generation,
        principal: credentials::Principal::of(grant),
    })
}

/// How far a forwarded request got before it failed (PRD section 3.4.6).
/// The gateway mode never retries, so it does not consult this yet.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Delivery {
    /// Refused before any byte was sent: no backend saw it.
    NotSent,
    /// Failed after a byte was sent, before a response arrived.
    Sent,
    /// The response was lost or cut short after the backend may have acted.
    ResponseLost,
}

impl Delivery {
    /// Writes have no general idempotency (SRC-015), so only a write that
    /// was never sent may be retried; a read may be retried after any
    /// failure (FR-007).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn may_retry(self, operation: Operation) -> bool {
        self == Self::NotSent || operation == Operation::Read
    }
}

/// What a client is told when a write may or may not have been applied.
pub(super) fn ambiguous_write() -> ApiError {
    ApiError::new(
        StatusCode::BAD_GATEWAY,
        "write_outcome_unknown",
        "the write may or may not have been applied; read before retrying",
    )
}

/// Why a backend response is not accepted (PRD section 3.4.8). Unused
/// until a backend stamps its epoch (OD-002).
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EpochRefusal {
    /// The record names an epoch and the response carries none.
    Missing,
    /// The response comes from an epoch older than the record's: a deposed
    /// writer answered.
    Stale,
}

impl EpochRefusal {
    /// A refused write may have been applied by the deposed writer, so it
    /// is ambiguous; a refused read is stale and may be retried.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn error(self, operation: Operation) -> ApiError {
        match operation {
            Operation::Write => ambiguous_write(),
            Operation::Read => ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "backend_epoch_stale",
                "the tenant's writer changed; retry the request",
            ),
        }
    }
}

/// Checks the epoch `stamped` on a response against the `expected` one in
/// the gateway's directory. A newer stamp is accepted: the directory is
/// behind and catches up within S_ctl. The residual window, a deposed
/// writer answering before the directory moves, is OD-002.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn check_epoch(expected: Option<u64>, stamped: Option<u64>) -> Result<(), EpochRefusal> {
    let Some(expected) = expected else {
        return Ok(());
    };
    match stamped {
        None => Err(EpochRefusal::Missing),
        Some(stamped) if stamped < expected => Err(EpochRefusal::Stale),
        Some(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{
        BearerToken, ServeMode, Service, admin, authenticated_router, authorize, serve_mode,
    };
    use axum::body::Body;
    use axum::http::Request;
    use std::collections::{BTreeMap, BTreeSet};
    use tower::ServiceExt;

    fn method(name: &str) -> Method {
        Method::from_bytes(name.as_bytes()).unwrap()
    }

    /// A concrete path for a template: every parameter becomes `p1`.
    fn concrete(template: &str) -> String {
        parts(template)
            .map(|part| if is_param(part) { "p1" } else { part })
            .fold(String::new(), |path, part| path + "/" + part)
    }

    fn standalone() -> Service {
        Service::new(crate::Database::create_in_memory(1, crate::CreateOptions::default()).unwrap())
    }

    fn reader(directory: &tempfile::TempDir) -> Service {
        let path = directory.path().join("reader.vlite");
        let mut db =
            crate::Database::create_with_format(&path, 2, crate::FormatVersion::V5).unwrap();
        db.put(1, &[1.0, 0.0], b"").unwrap();
        db.close().unwrap();
        Service::new(crate::Database::open_read_only(&path).unwrap())
            .with_serve_mode(ServeMode::Reader)
            .unwrap()
    }

    #[test]
    fn every_registered_route_classifies_to_itself() {
        let mut seen = BTreeSet::new();
        for &(name, template) in ROUTES {
            assert!(seen.insert((name, template)), "duplicate {name} {template}");
            let path = concrete(template);
            let classified = classify(&method(name), &path)
                .unwrap_or_else(|refusal| panic!("{name} {template}: {refusal:?}"));
            assert_eq!(classified.template, template, "{name} {path}");
            if name == "GET" {
                assert_eq!(classify(&Method::HEAD, &path).unwrap().template, template);
            }
        }
        for path in PUBLIC {
            let served = ROUTES.iter().find(|(_, template)| template == path);
            let &(name, _) = served.unwrap_or_else(|| panic!("public {path} is not a route"));
            assert!(classify(&method(name), path).unwrap().public, "{path}");
        }
    }

    #[test]
    fn operations_follow_the_read_rule_not_the_method() {
        use Operation::{Read, Write};
        for (name, path, operation) in [
            ("GET", "/v1/collections", Read),
            ("HEAD", "/v1/collections", Read),
            ("POST", "/v1/search", Read),
            ("POST", "/v1/collections/c1/search", Read),
            ("POST", "/v1/collections/c1/search/geo", Read),
            ("POST", "/v1/collections/c1/query", Read),
            ("POST", "/v1/collections/c1/kv/entries/get", Read),
            ("POST", "/v1/graphs/g1/traverse", Read),
            ("POST", "/v1/graphs/g1/explain", Read),
            ("POST", "/v1/memory/b1/recall", Read),
            ("GET", "/v1/admin/serve-status", Read),
            ("POST", "/v1/collections", Write),
            ("POST", "/v1/batches", Write),
            ("POST", "/v1/graphs/g1/batch", Write),
            ("POST", "/v1/memory/b1/remember", Write),
            ("PUT", "/v1/graphs/g1/schema", Write),
            ("PATCH", "/v1/vectors/7/metadata", Write),
            ("DELETE", "/v1/collections/c1", Write),
        ] {
            let classified = classify(&method(name), path).unwrap();
            assert_eq!(classified.operation, operation, "{name} {path}");
            assert!(!classified.public, "{name} {path}");
        }
        assert!(classify(&Method::GET, "/health").unwrap().public);
        assert!(classify(&Method::POST, "/v1/auth/login").unwrap().public);
        assert_eq!(
            classify(&Method::GET, "/v1/capabilities").unwrap().need,
            Need::Any
        );
        assert_eq!(
            classify(&Method::GET, "/v1/admin/serve-status")
                .unwrap()
                .need,
            Need::Admin
        );
    }

    #[test]
    fn unmapped_requests_are_refused_never_forwarded() {
        use Refusal::{MethodNotAllowed, UnknownRoute};
        for (name, path, refusal) in [
            ("GET", "/v1/no-such-route", UnknownRoute),
            ("GET", "/v1/collections/", UnknownRoute),
            ("GET", "/v1//collections", UnknownRoute),
            ("GET", "", UnknownRoute),
            ("GET", "/", UnknownRoute),
            ("GET", "v1/collections", UnknownRoute),
            (
                "GET",
                "/v1/collections/c1/vectors/7/extra/deep",
                UnknownRoute,
            ),
            ("OPTIONS", "/v1/collections", MethodNotAllowed),
            ("POST", "/health", MethodNotAllowed),
            ("DELETE", "/v1/search", MethodNotAllowed),
            ("HEAD", "/v1/search", MethodNotAllowed),
            ("TRACE", "/v1/collections", MethodNotAllowed),
        ] {
            assert_eq!(
                classify(&method(name), path),
                Err(refusal),
                "{name} {path:?}"
            );
        }
    }

    #[test]
    fn a_literal_segment_beats_a_parameter_and_the_path_picks_first() {
        for (name, path, template) in [
            (
                "GET",
                "/v1/admin/backups/schedule",
                "/v1/admin/backups/schedule",
            ),
            (
                "POST",
                "/v1/admin/backups/schedule/run",
                "/v1/admin/backups/schedule/run",
            ),
            (
                "POST",
                "/v1/admin/backups/schedule/verify",
                "/v1/admin/backups/{name}/verify",
            ),
            (
                "POST",
                "/v1/admin/backups/nightly/drill",
                "/v1/admin/backups/{name}/drill",
            ),
        ] {
            assert_eq!(
                classify(&method(name), path).unwrap().template,
                template,
                "{name} {path}"
            );
        }
        assert_eq!(
            classify(&Method::DELETE, "/v1/admin/backups/schedule"),
            Err(Refusal::MethodNotAllowed)
        );
    }

    /// TEST-002: the classifier agrees with `authorize` and reader admission
    /// for every inventoried route.
    #[test]
    fn classification_agrees_with_authorize_and_reader_admission() {
        let directory = tempfile::tempdir().unwrap();
        let (standalone, reader) = (standalone(), reader(&directory));
        for &(name, template) in ROUTES {
            let (method, path) = (method(name), concrete(template));
            let classified = classify(&method, &path).unwrap();
            let read = classified.operation == Operation::Read;
            assert_eq!(
                read,
                read_request(&method, template, &path),
                "{name} {template}"
            );
            assert_eq!(classified.need, permission(&method, template));
            let read_only = authorize(
                &standalone,
                &admin::Role::ReadOnly,
                &method,
                template,
                &path,
            )
            .is_ok();
            let expected =
                path.starts_with("/v1/auth/") || (!path.starts_with("/v1/admin/") && read);
            assert_eq!(read_only, expected, "read-only token: {name} {template}");
            assert_eq!(
                serve_mode::admit(&reader, &method, template, &path).is_ok(),
                read,
                "reader: {name} {template}"
            );
            assert!(serve_mode::admit(&standalone, &method, template, &path).is_ok());
        }
    }

    /// Drift guard: send every route (and the overlap and unknown probes)
    /// through the real router and check that the router reports the same
    /// matched template the classifier chose. Memory routes are registered
    /// only while bindings exist, so they are covered by the table tests.
    #[tokio::test]
    async fn the_router_matches_the_template_the_classifier_chooses() {
        let service = standalone();
        let app = authenticated_router(
            service.clone(),
            BearerToken::new("gateway-classifier-drift-guard-token").unwrap(),
        );
        let mut probes: Vec<(Method, String)> = ROUTES
            .iter()
            .filter(|(_, template)| !template.starts_with("/v1/memory/"))
            .map(|&(name, template)| (method(name), concrete(template)))
            .collect();
        for (name, path) in [
            ("HEAD", "/v1/collections"),
            ("GET", "/v1/admin/backups/schedule"),
            ("POST", "/v1/admin/backups/schedule/run"),
            ("POST", "/v1/admin/backups/schedule/verify"),
            ("POST", "/v1/admin/backups/schedule/drill"),
            ("GET", "/v1/no-such-route"),
            ("GET", "/v1/collections/"),
        ] {
            probes.push((method(name), path.to_owned()));
        }
        let mut expected: BTreeMap<(String, String), u64> = BTreeMap::new();
        for (method, path) in &probes {
            let template = match classify(method, path) {
                Ok(classified) => classified.template,
                Err(Refusal::UnknownRoute) => "unmatched",
                Err(refusal) => panic!("{method} {path}: {refusal:?}"),
            };
            let label = match method.as_str() {
                "GET" | "POST" | "PUT" | "DELETE" => method.as_str(),
                _ => "other",
            };
            *expected
                .entry((label.to_owned(), template.to_owned()))
                .or_default() += 1;
            let request = Request::builder()
                .method(method.clone())
                .uri(path.as_str())
                .body(Body::empty())
                .unwrap();
            app.clone().oneshot(request).await.unwrap();
        }
        let recorded: BTreeMap<(String, String), u64> = service
            .metrics
            .by_route
            .lock()
            .unwrap()
            .iter()
            .map(|(key, stats)| (key.clone(), stats.count))
            .collect();
        assert_eq!(recorded, expected);
    }

    /// Whether `share * replicas <= limit` holds exactly, using the share's
    /// mantissa and exponent rather than float arithmetic.
    fn fits(share: f64, replicas: u32, limit: u32) -> bool {
        let bits = share.to_bits();
        let exponent = i32::try_from((bits >> 52) & 0x7ff).unwrap() - 1075;
        assert!((-52..=0).contains(&exponent), "share {share} out of range");
        let mantissa = u128::from((bits & ((1 << 52) - 1)) | (1 << 52));
        mantissa * u128::from(replicas) <= u128::from(limit) << exponent.unsigned_abs()
    }

    #[test]
    fn replica_shares_never_sum_past_the_limit_and_are_the_largest_that_fit() {
        let limits = (1..=3_000).chain([777_777, 999_983, 1_000_000]);
        let mut corrected = 0;
        for limit in limits {
            for replicas in 1..=100 {
                let Ok(share) = replica_share(limit, replicas) else {
                    assert!(limit < replicas, "{limit}/{replicas} refused");
                    continue;
                };
                assert!(
                    fits(share, replicas, limit),
                    "{limit}/{replicas} rounded up"
                );
                assert!(
                    !fits(share.next_up(), replicas, limit),
                    "{limit}/{replicas} not largest"
                );
                if share != f64::from(limit) / f64::from(replicas) {
                    corrected += 1;
                }
            }
        }
        // The sweep must reach quotients that division rounds up.
        assert!(corrected > 0);
        assert_eq!(replica_share(100, 4), Ok(25.0));
        assert_eq!(replica_share(250, 1), Ok(250.0));
        assert_eq!(replica_share(4, 4), Ok(1.0));
    }

    #[test]
    fn replica_shares_refuse_zero_replicas_and_sub_request_slices() {
        assert_eq!(replica_share(100, 0), Err(ShareRefusal::NoReplicas));
        assert_eq!(replica_share(3, 4), Err(ShareRefusal::BelowOneRequest));
        assert_eq!(
            replica_share(1, u32::MAX),
            Err(ShareRefusal::BelowOneRequest)
        );
    }

    mod routing {
        use super::super::*;
        use crate::server::control_state::{ReadRouting, Route, Snapshot};
        use crate::server::principal_envelope::{Policy, Refusal as EnvelopeRefusal, Verifier};
        use crate::server::{credentials, tenants};

        const T1: &str = "0123456789abcdef0123456789abcdef";
        const T2: &str = "fedcba9876543210fedcba9876543210";

        fn read() -> Classification {
            classify(&Method::GET, "/v1/collections").unwrap()
        }

        fn write() -> Classification {
            classify(&Method::POST, "/v1/collections").unwrap()
        }

        /// T1 routed to a dedicated writer at epoch 5; T2 a tenant with no
        /// record; the third token untenanted.
        fn routed() -> (Snapshot, Vec<String>) {
            let (credentials, tokens) = credentials::tests::issued(
                &[
                    ("aaaaaaaaaaaa", Some(T1)),
                    ("bbbbbbbbbbbb", Some(T2)),
                    ("cccccccccccc", None),
                ],
                0x77,
            );
            let tenants = tenants::Store::for_test(vec![
                tenants::Tenant::for_test(T1, None),
                tenants::Tenant::for_test(T2, None),
            ]);
            let snapshot = Snapshot::import(&credentials, Some(&tenants))
                .unwrap()
                .with_directory(vec![Route::dedicated_for_test(T1, 5)]);
            (snapshot, tokens)
        }

        #[test]
        fn every_request_targets_the_writer_even_with_reader_routing() {
            let mut record = Route::dedicated_for_test(T1, 5);
            record.read_routing = ReadRouting::Readers;
            for (classification, operation) in
                [(read(), Operation::Read), (write(), Operation::Write)]
            {
                let target = target_for(classification, Some(&record)).unwrap();
                assert_eq!(
                    target,
                    Target {
                        endpoint: record.writer.clone(),
                        audience: record.audience.clone(),
                        epoch: Some(5),
                        operation,
                    }
                );
                assert_ne!(Some(target.endpoint), record.reader);
            }
            let shared = Route::shared_for_test(T1);
            let target = target_for(write(), Some(&shared)).unwrap();
            assert_eq!(target.endpoint, shared.writer);
            assert_eq!(target.audience, "shared-pool");
            assert_eq!(target.epoch, None);
            assert_eq!(target_for(read(), None), Err(RouteRefusal::NoRecord));
        }

        #[test]
        fn route_target_uses_the_authenticated_tenant_record() {
            let (snapshot, tokens) = routed();
            let grant = snapshot.authenticate(&tokens[0], 1).unwrap();
            let target = route_target(write(), &grant, &snapshot).unwrap();
            assert_eq!(target.endpoint, format!("https://writer-{T1}.internal"));
            assert_eq!(target.epoch, Some(5));

            let unrouted = snapshot.authenticate(&tokens[1], 1).unwrap();
            let refusal = route_target(read(), &unrouted, &snapshot).unwrap_err();
            assert_eq!(refusal, RouteRefusal::NoRecord);
            let error = refusal.error();
            assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(error.code, "tenant_not_routed");

            let untenanted = snapshot.authenticate(&tokens[2], 1).unwrap();
            let refusal = route_target(read(), &untenanted, &snapshot).unwrap_err();
            assert_eq!(refusal, RouteRefusal::NoTenant);
            let error = refusal.error();
            assert_eq!(error.status, StatusCode::FORBIDDEN);
            assert_eq!(error.code, "gateway_tenant_required");
        }

        #[test]
        fn only_unsent_writes_are_retried() {
            for delivery in [Delivery::NotSent, Delivery::Sent, Delivery::ResponseLost] {
                assert!(delivery.may_retry(Operation::Read), "{delivery:?}");
                assert_eq!(
                    delivery.may_retry(Operation::Write),
                    delivery == Delivery::NotSent,
                    "{delivery:?}"
                );
            }
            let error = ambiguous_write();
            assert_eq!(error.status, StatusCode::BAD_GATEWAY);
            assert_eq!(error.code, "write_outcome_unknown");
        }

        #[test]
        fn a_minted_envelope_verifies_only_at_its_target() {
            let (snapshot, tokens) = routed();
            let grant = snapshot.authenticate(&tokens[0], 1).unwrap();
            let target = route_target(write(), &grant, &snapshot).unwrap();
            let signer = principal_envelope::Signer::from_seed("gw-1", &[7; 32]).unwrap();
            let token = mint(&signer, &target, &grant, 4, 1_000, 30_000).unwrap();
            let verifier = |audience: &str| {
                Verifier::new(
                    vec![(signer.metadata().active_kid.clone(), signer.public_key())],
                    Policy {
                        audience: audience.to_owned(),
                        tenant: Some(T1.to_owned()),
                        skew_ms: 1_000,
                        max_lifetime_ms: 60_000,
                        minimum_generation: 1,
                    },
                )
                .unwrap()
            };
            let verified = verifier(&target.audience).verify(&token, 2_000).unwrap();
            assert_eq!(verified.tenant_id.as_deref(), Some(T1));
            assert_eq!(verified.id, grant.id);
            assert_eq!(
                verifier("backend-other").verify(&token, 2_000).err(),
                Some(EnvelopeRefusal::Audience)
            );
            assert!(mint(&signer, &target, &grant, 4, 1_000, 0).is_err());
            assert!(mint(&signer, &target, &grant, 4, u64::MAX, 1).is_err());
        }

        #[test]
        fn responses_from_an_older_epoch_are_refused() {
            assert_eq!(check_epoch(None, None), Ok(()));
            assert_eq!(check_epoch(None, Some(3)), Ok(()));
            assert_eq!(check_epoch(Some(5), None), Err(EpochRefusal::Missing));
            assert_eq!(check_epoch(Some(5), Some(4)), Err(EpochRefusal::Stale));
            assert_eq!(check_epoch(Some(5), Some(5)), Ok(()));
            assert_eq!(check_epoch(Some(5), Some(6)), Ok(()));
            for refusal in [EpochRefusal::Missing, EpochRefusal::Stale] {
                let write = refusal.error(Operation::Write);
                assert_eq!(write.status, StatusCode::BAD_GATEWAY);
                assert_eq!(write.code, "write_outcome_unknown");
                let read = refusal.error(Operation::Read);
                assert_eq!(read.status, StatusCode::SERVICE_UNAVAILABLE);
                assert_eq!(read.code, "backend_epoch_stale");
            }
        }
    }
}
