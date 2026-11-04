//! Memory route handlers (contract sections 3-8, mapping sections 1-3 and
//! 6). Every route checks, in order: authentication and route permission
//! (middleware), binding and exact-scope authorization (one 403 body), the
//! writable precheck for writes, capability refusals, request validation and
//! finally the core call, stopping at the first failure.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Instant;

use axum::{
    Extension, Router,
    body::Bytes,
    extract::{
        Path, State,
        rejection::{BytesRejection, PathRejection},
    },
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::{Map, Value, json};

use super::super::{ApiError, RequestId, Service, admin, credentials::Op};
use super::wire::{self, Fail, Fields};
use crate::Database;
use crate::memory::binding::Binding;
use crate::memory::{
    MemoryError, MemoryItem, MemoryKind, MemoryMut, MemoryRef, MemoryScope, Provenance,
    ReceiptOperation, RememberRequest, WireCode, WireMapping, WireOperation, memory_error,
    receipt_id,
};

/// Mutation `deadline_ms` default and bound (contract `MemoryMutation`).
const DEFAULT_DEADLINE_MS: u64 = 250;
const MAX_DEADLINE_MS: u64 = 5_000;
/// `max_references` default and bound.
const DEFAULT_MAX_REFERENCES: u64 = 64;
const MAX_REFERENCES: u64 = 256;
/// `MemoryItem.content` bound in UTF-8 bytes.
const MAX_CONTENT_BYTES: usize = 4_096;
const MAX_ITEMS: usize = 4_096;
const MAX_PROVENANCE: usize = 64;
const DIGEST_VERSION: &str = "memory-request-v1";

pub(super) const REMEMBER: &str = "/v1/memory/{binding}/remember";
pub(super) const RECALL: &str = "/v1/memory/{binding}/recall";
pub(super) const FIND_FACTS: &str = "/v1/memory/{binding}/facts/query";
pub(super) const CORRECT_FACTS: &str = "/v1/memory/{binding}/facts/correct";
pub(super) const CONSOLIDATE: &str = "/v1/memory/{binding}/consolidate";
pub(super) const FORGET: &str = "/v1/memory/{binding}/forget";
pub(super) const RECEIPT: &str = "/v1/memory/{binding}/receipts/lookup";
pub(super) const GET_ERASURE: &str = "/v1/memory/{binding}/erasures/get";
pub(super) const RESUME_ERASURE: &str = "/v1/memory/{binding}/erasures/resume";

/// The routes served for a non-empty binding set.
pub(super) fn routes() -> Router<Service> {
    Router::new().route(REMEMBER, post(remember))
}

/// Test seam for the engine clock and the elapsed request budget. Absent in
/// production, where the server clock and the measured elapsed time apply.
#[derive(Clone, Debug, Default)]
pub(in crate::server) struct Clock {
    pub now_ms: Arc<AtomicU64>,
    pub elapsed_ms: Arc<AtomicU64>,
}

/// One request's captured context, carried into the database closure.
struct Call {
    service: Service,
    role: admin::Role,
    request_id: String,
    operation: &'static str,
    binding: Option<String>,
    started: Instant,
    clock: Option<Clock>,
}

impl Call {
    fn new(
        service: Service,
        role: admin::Role,
        request_id: RequestId,
        clock: Option<Extension<Clock>>,
        operation: &'static str,
    ) -> Self {
        Self {
            service,
            role,
            request_id: request_id.0,
            operation,
            binding: None,
            started: Instant::now(),
            clock: clock.map(|Extension(clock)| clock),
        }
    }

    /// The engine clock, captured once per request inside the closure.
    fn now(&self) -> u64 {
        match &self.clock {
            Some(clock) => clock.now_ms.load(Ordering::SeqCst),
            None => crate::current_time_ms(),
        }
    }

    fn elapsed_ms(&self) -> u64 {
        let measured = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let injected = self
            .clock
            .as_ref()
            .map_or(0, |clock| clock.elapsed_ms.load(Ordering::SeqCst));
        measured.saturating_add(injected)
    }

    /// The absolute core deadline for a relative budget.
    fn deadline(&self, now: u64, budget_ms: u64) -> u64 {
        now.saturating_add(budget_ms.saturating_sub(self.elapsed_ms()))
    }

    /// One audit entry per memory request: operation, binding, principal and
    /// outcome only, never memory text, keys or identifiers.
    fn audit(&self, outcome: &str, receipt: Option<(&str, u64)>) {
        let Some(log) = &self.service.admin.audit_log else {
            return;
        };
        let principal = match &self.role {
            admin::Role::Scoped(grant) => grant.id.as_str(),
            role => role.name(),
        };
        let mut entry = json!({
            "event": "memory_request",
            "request_id": self.request_id,
            "principal": principal,
            "operation": self.operation,
            "outcome": outcome,
            "duration_ns": self.started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
        });
        if let Some(binding) = &self.binding {
            entry["binding"] = json!(binding);
        }
        if let Some((id, sequence)) = receipt {
            entry["receipt_id"] = json!(id);
            entry["sequence"] = json!(sequence.to_string());
        }
        if log.append(entry, false).is_err() {
            self.service
                .metrics
                .audit_failures
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    fn finish(&self, result: Result<Value, Fail>, receipt: Option<(&str, u64)>) -> Response {
        match result {
            Ok(body) => {
                self.audit("ok", receipt);
                axum::Json(body).into_response()
            }
            Err(fail) => {
                self.audit(fail.code, None);
                fail.into_response()
            }
        }
    }
}

/// The resolved binding facts a request needs, copied out of the set.
#[derive(Clone)]
struct Bound {
    tenant: String,
    graph: String,
    evidence: Option<String>,
    receipt_window_ms: u64,
    refusal: Option<WireCode>,
    scopes: Vec<MemoryScope>,
    grant: Arc<super::super::credentials::Grant>,
}

/// Steps 1-2 of the check order before the body is read: a scoped
/// credential, a binding in its own tenant, a binding grant for it and the
/// ordinary underlying grants (mapping section 6, intersection). Every
/// failure is the same 403 so binding existence is never disclosed.
fn resolve(
    service: &Service,
    role: &admin::Role,
    name: Result<Path<String>, PathRejection>,
    operation: WireOperation,
    evidence_ops: &[Op],
) -> Result<(Bound, String), Fail> {
    let admin::Role::Scoped(grant) = role else {
        return Err(Fail::forbidden());
    };
    let Ok(Path(name)) = name else {
        return Err(Fail::forbidden());
    };
    let binding: &Binding = service
        .memory_bindings
        .resolve(grant.tenant_id.as_deref(), &name)
        .ok_or_else(Fail::forbidden)?;
    let scopes = binding
        .grants
        .iter()
        .find(|entry| entry.principal == grant.id)
        .map(|entry| entry.scopes.clone())
        .ok_or_else(Fail::forbidden)?;
    let (graph, evidence) = super::resources(binding);
    if !grant.graph_allowed(&graph) {
        return Err(Fail::forbidden());
    }
    if let Some(evidence) = &evidence
        && (!grant.names(evidence) || evidence_ops.iter().any(|op| !grant.allows(*op)))
    {
        return Err(Fail::forbidden());
    }
    Ok((
        Bound {
            tenant: binding.tenant.clone(),
            graph,
            evidence,
            receipt_window_ms: binding.receipt_window_ms,
            refusal: binding.refusal(operation),
            scopes,
            grant: grant.clone(),
        },
        name,
    ))
}

impl Bound {
    /// Exact-scope authorization: no parent, child or sibling fallback.
    fn authorize(&self, scope: &MemoryScope) -> Result<(), Fail> {
        if self.scopes.contains(scope) {
            Ok(())
        } else {
            Err(Fail::forbidden())
        }
    }

    /// The evidence collection must still be the one the grant covers (a
    /// dropped and recreated collection has a new ID).
    fn covers(&self, db: &Database) -> Result<(), Fail> {
        if let Some(evidence) = &self.evidence {
            let id = db.resolve(evidence).map_err(ApiError::from)?;
            if !self.grant.covers(db.lineage(), id, evidence) {
                return Err(Fail::forbidden());
            }
        }
        Ok(())
    }
}

/// The request body: `application/json`, at most the shared 8 MiB limit
/// (413), one JSON object with unique keys (400).
fn body(headers: &HeaderMap, bytes: Result<Bytes, BytesRejection>) -> Result<Fields, Fail> {
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"));
    if !json {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "invalid_json",
            "request body must be application/json",
        )
        .into());
    }
    let bytes = bytes.map_err(|error| {
        Fail::from(ApiError::new(
            error.status(),
            "invalid_json",
            "request body could not be read",
        ))
    })?;
    Fields::new(wire::parse(&bytes)?, "body")
}

fn writable(db: &Database) -> Result<(), Fail> {
    if db.is_read_only() {
        return Err(
            ApiError::new(StatusCode::FORBIDDEN, "read_only", "database is read-only").into(),
        );
    }
    Ok(())
}

/// Core error to wire (mapping section 2).
fn core_error(error: MemoryError) -> Fail {
    match memory_error(&error) {
        WireMapping::Code(code) => Fail::wire(code),
        WireMapping::Delegate => match error {
            MemoryError::Storage(error) => ApiError::from(error).into(),
            _ => internal(),
        },
        WireMapping::Cancelled => internal(),
    }
}

fn internal() -> Fail {
    super::super::internal("memory").into()
}

fn feed() -> Value {
    json!({
        "mode": "ordinary_changes",
        "temporal_complete": false,
        "ttl_complete": false,
        "consumer_purge_acknowledged": false,
    })
}

/// A committed or replayed mutation's `MemoryMutationResponse` core fields.
struct Receipt {
    operation: ReceiptOperation,
    revision: u64,
    replayed: bool,
}

/// Builds the receipt block from stored state alone: the receipt ID is
/// derived from the stored revision and `retry_until_ms` is read back.
fn mutation_body(
    db: &Database,
    bound: &Bound,
    scope: &MemoryScope,
    key: &str,
    now: u64,
    receipt: &Receipt,
    outcomes: Vec<Value>,
) -> Result<(Value, String), Fail> {
    let id = receipt_id(receipt.operation, scope, key, receipt.revision).map_err(core_error)?;
    let retry_until = MemoryRef::bind(db, &bound.graph, bound.evidence.as_deref())
        .and_then(|memory| memory.retry_until(receipt.operation, scope, key, now))
        .map_err(core_error)?
        .ok_or_else(internal)?;
    let body = json!({
        "receipt": {
            "id": id,
            "operation": operation_name(receipt.operation),
            "scope": wire::effective(scope),
            "sequence": receipt.revision.to_string(),
            "retry_until_ms": retry_until.to_string(),
            "digest_version": DIGEST_VERSION,
        },
        "replayed": receipt.replayed,
        "outcomes": outcomes,
        "feed": feed(),
        "remaining_work": "0",
    });
    Ok((body, id))
}

fn operation_name(operation: ReceiptOperation) -> &'static str {
    match operation {
        ReceiptOperation::Remember => "remember",
        ReceiptOperation::Forget => "forget",
        ReceiptOperation::Consolidate => "consolidate",
    }
}

/// The `MemoryMutation` common fields.
struct Mutation {
    key: String,
    max_references: u64,
    deadline_ms: u64,
}

/// Reads the common fields after refusals have been checked.
fn mutation(fields: &mut Fields) -> Result<Mutation, Fail> {
    let key = wire::key(fields.require("idempotency_key")?, "idempotency_key")?;
    let max_references = wire::integer(
        fields.take("max_references")?,
        "max_references",
        1,
        MAX_REFERENCES,
        DEFAULT_MAX_REFERENCES,
    )?;
    let deadline_ms = wire::integer(
        fields.take("deadline_ms")?,
        "deadline_ms",
        1,
        MAX_DEADLINE_MS,
        DEFAULT_DEADLINE_MS,
    )?;
    wire::boolean(
        fields.take("require_complete_feed")?,
        "require_complete_feed",
    )?;
    Ok(Mutation {
        key,
        max_references,
        deadline_ms,
    })
}

/// Section 2 refusal priority: binding state, then the route refusal, then
/// `require_complete_feed: true`, then field refusals in request order.
fn refuse(
    bound: &Bound,
    body: &Map<String, Value>,
    fields: impl FnOnce(&Map<String, Value>) -> Option<&'static str>,
) -> Result<(), Fail> {
    if let Some(code) = bound.refusal {
        return Err(Fail::wire(code));
    }
    if body.get("require_complete_feed") == Some(&Value::Bool(true)) {
        return Err(Fail::wire(WireCode {
            status: 409,
            code: "memory_feed_unsupported",
            feature: None,
        }));
    }
    match fields(body) {
        Some(feature) => Err(Fail::wire(crate::memory::unavailable(feature))),
        None => Ok(()),
    }
}

fn present(value: &Value, name: &str) -> bool {
    value.get(name).is_some()
}

/// `MemorySource` to the core provenance, field for field; `source_kind` is
/// required on the wire, so the core kind is always present.
fn source(value: Value, field: &'static str) -> Result<Provenance, Fail> {
    let mut fields = Fields::new(value, field)?;
    let source = wire::key(fields.require("source_id")?, field)?;
    let kind = wire::key(fields.require("source_kind")?, field)?;
    let uri = fields
        .take("uri")?
        .map(|value| wire::key(value, field))
        .transpose()?;
    let transformation = fields
        .take("transformation_id")?
        .map(|value| wire::key(value, field))
        .transpose()?;
    fields.finish()?;
    Ok(Provenance {
        source,
        kind: Some(kind),
        uri,
        transformation,
    })
}

fn kind(value: Value) -> Result<MemoryKind, Fail> {
    match value.as_str() {
        Some("entity") => Ok(MemoryKind::Entity),
        Some("episodic") => Ok(MemoryKind::Episodic),
        _ => Err(Fail::invalid("kind")),
    }
}

fn content(value: Value) -> Result<String, Fail> {
    match value {
        Value::String(text) if text.len() <= MAX_CONTENT_BYTES => Ok(text),
        _ => Err(Fail::invalid("content")),
    }
}

// ---------------------------------------------------------------- Remember

/// Remember field refusals in request field order (mapping sections 1, 3).
fn remember_refusal(body: &Map<String, Value>) -> Option<&'static str> {
    for item in body
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if matches!(
            item.get("kind").and_then(Value::as_str),
            Some("fact" | "relation")
        ) {
            return Some("fact_kinds");
        }
        if present(item, "confidence") {
            return Some("confidence");
        }
        let pinned = item
            .get("references")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|reference| reference.get("revision").cloned())
            .any(|revision| wire::u64_value(revision, "references").is_ok_and(|rev| rev != 0));
        if pinned {
            return Some("reference_revision_pin");
        }
        for field in ["evidence", "metadata", "assertion"] {
            if present(item, field) {
                return Some(field);
            }
        }
        if present(item, "expected") && item.get("merge") != Some(&json!("merge_provenance")) {
            return Some("expected");
        }
    }
    body.contains_key("evidence").then_some("evidence")
}

fn remember_item(
    value: Value,
    bundle: &std::collections::BTreeSet<String>,
    max_references: u64,
) -> Result<MemoryItem, Fail> {
    let mut fields = Fields::new(value, "items")?;
    let id = wire::key(fields.require("id")?, "id")?;
    let kind = kind(fields.require("kind")?)?;
    let content = content(fields.require("content")?)?;
    let observed_at_ms = wire::i64_value(fields.require("observed_at_ms")?, "observed_at_ms")?;
    let importance = fields
        .take("importance")?
        .map(|value| wire::number(value, "importance", 0.0, 1.0))
        .transpose()?;
    let expiry_ms = fields
        .take("expires_at_ms")?
        .map(|value| wire::u64_value(value, "expires_at_ms"))
        .transpose()?;
    let source = source(fields.require("source")?, "source")?;
    let provenance = match fields.take("provenance")? {
        None => vec![source.clone()],
        Some(value) => wire::array(value, "provenance", 0, MAX_PROVENANCE)?
            .into_iter()
            .map(|value| self::source(value, "provenance"))
            .collect::<Result<_, _>>()?,
    };
    let references = match fields.take("references")? {
        None => Vec::new(),
        Some(value) => wire::array(value, "references", 0, max_references as usize)?
            .into_iter()
            .map(|value| {
                let mut reference = Fields::new(value, "references")?;
                let id = wire::key(reference.require("id")?, "references")?;
                let revision = wire::u64_value(reference.require("revision")?, "references")?;
                reference.finish()?;
                // Refusals ran first: only revision 0, naming a bundle item.
                if revision != 0 || !bundle.contains(&id) {
                    return Err(Fail::invalid("references"));
                }
                Ok(id)
            })
            .collect::<Result<_, _>>()?,
    };
    let expected = fields.take("expected")?;
    let merge = match fields.take("merge")? {
        None => false,
        Some(value) => match value.as_str() {
            Some("none") => false,
            Some("merge_provenance") => true,
            _ => return Err(Fail::invalid("merge")),
        },
    };
    let expected_revision = match (merge, expected) {
        (false, None) => None,
        (true, Some(value)) => {
            let mut expected = Fields::new(value, "merge")?;
            let revision = wire::u64_value(expected.require("revision")?, "merge")?;
            expected.finish()?;
            Some(revision)
        }
        _ => return Err(Fail::invalid("merge")),
    };
    fields.finish()?;
    Ok(MemoryItem {
        merge_into: merge.then(|| id.clone()),
        id,
        kind,
        content,
        source: source.source,
        importance,
        observed_at_ms: Some(observed_at_ms),
        provenance,
        references,
        expected_revision,
        expiry_ms,
        expiry_valid: true,
        evidence: Vec::new(),
    })
}

async fn remember(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    Extension(request_id): Extension<RequestId>,
    clock: Option<Extension<Clock>>,
    name: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    bytes: Result<Bytes, BytesRejection>,
) -> Response {
    let mut call = Call::new(service.clone(), role.clone(), request_id, clock, "remember");
    let evidence_ops: &[Op] = &[Op::Write];
    let (bound, name) = match resolve(&service, &role, name, WireOperation::Remember, evidence_ops)
    {
        Ok(found) => found,
        Err(fail) => return call.finish(Err(fail), None),
    };
    call.binding = Some(name);
    let prepared = body(&headers, bytes).and_then(|mut fields| {
        let scope = wire::scope(fields.require("scope")?, &bound.tenant, "scope")?;
        bound.authorize(&scope)?;
        Ok((fields, scope))
    });
    let (fields, scope) = match prepared {
        Ok(prepared) => prepared,
        Err(fail) => return call.finish(Err(fail), None),
    };
    let call = Arc::new(call);
    let inner = call.clone();
    let result = service
        .run_mutation(0, move |db| {
            Ok(remember_in(db, &inner, &bound, fields, scope))
        })
        .await
        .map_err(Fail::from)
        .and_then(|result| result);
    match result {
        Ok((body, id, revision)) => call.finish(Ok(body), Some((&id, revision))),
        Err(fail) => call.finish(Err(fail), None),
    }
}

fn remember_in(
    db: &mut Database,
    call: &Call,
    bound: &Bound,
    mut fields: Fields,
    scope: MemoryScope,
) -> Result<(Value, String, u64), Fail> {
    writable(db)?;
    bound.covers(db)?;
    refuse(bound, fields.raw(), remember_refusal)?;
    let common = mutation(&mut fields)?;
    let items = wire::array(fields.require("items")?, "items", 1, MAX_ITEMS)?;
    let bundle = items
        .iter()
        .filter_map(|item| item.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let items = items
        .into_iter()
        .map(|item| remember_item(item, &bundle, common.max_references))
        .collect::<Result<Vec<_>, _>>()?;
    fields.finish()?;
    let now = call.now();
    let request = RememberRequest {
        scope,
        idempotency_key: common.key,
        receipt_window_ms: bound.receipt_window_ms,
        now_ms: now,
        deadline_ms: Some(call.deadline(now, common.deadline_ms)),
        allow_future_observations: false,
        items,
    };
    let result = MemoryMut::bind(db, &bound.graph, bound.evidence.as_deref())
        .and_then(|mut memory| memory.remember(&request))
        .map_err(core_error)?;
    let outcomes = result
        .actions
        .iter()
        .map(|action| {
            json!({"id": action.id, "action": action.action, "revision": action.revision.to_string()})
        })
        .collect();
    let receipt = Receipt {
        operation: ReceiptOperation::Remember,
        revision: result.receipt_revision,
        replayed: result.replayed,
    };
    let (body, id) = mutation_body(
        db,
        bound,
        &request.scope,
        &request.idempotency_key,
        now,
        &receipt,
        outcomes,
    )?;
    Ok((body, id, result.receipt_revision))
}
