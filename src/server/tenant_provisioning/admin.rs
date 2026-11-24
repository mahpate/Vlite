//! design-tenant-provisioning EPIC-002: administrator onboarding and the
//! observable operation surface over the EPIC-001 contract.
//!
//! ITEM-004: administrator-only REST intents (create, readers, migration,
//! retirement), a redacted status view and generation-bound actions. Each
//! action is idempotent per `(principal, idempotency key)`: the request is
//! persisted create-only before it is applied, and a replay returns the
//! stored result. Transitions whose handlers belong to later epics are
//! refused before anything is persisted. `create` writes the tenant's
//! metadata under the operation's persisted ID, non-routable: no route is
//! published and no cloud action is dispatched (OD-002).
//!
//! ITEM-005: `vlite-server tenant-deployment`, the same contracts from the
//! command line. Requests are read from files, never from arguments.
//!
//! The legacy `POST /v1/admin/tenants` is unchanged (OD-001).
use super::*;
use crate::server::admin::Role;
use crate::server::{audit, cloud_recovery, tenants, users};
use std::ffi::OsString;
use std::path::PathBuf;

/// The coordination objects one deployment surface works on.
pub(in crate::server) struct Deployments {
    objects: Objects,
    ops: Operations,
    /// A serving process's hold on its admin directory's control-state
    /// lock (`TenantDeploymentsConfig::open`); the OS releases it on exit.
    _lock: Option<std::fs::File>,
}

impl Deployments {
    pub(in crate::server) fn new(
        store: Arc<dyn ObjectStore>,
        control_object: &str,
        ops_prefix: &str,
    ) -> Self {
        Self {
            objects: Objects::new(store.clone(), control_object),
            ops: Operations::new(store, ops_prefix),
            _lock: None,
        }
    }

    /// The control-state object of `source` and the operation records beside
    /// it (blocking).
    pub(in crate::server) fn connect(
        source: &control_state::Source,
    ) -> Result<Self, cloud_recovery::RecoveryError> {
        let (store, prefix) = source.coordination()?;
        Ok(Self::new(
            store,
            &format!("{prefix}{}", source.object),
            &format!("{prefix}tenant-operations"),
        ))
    }
}

impl Service {
    /// Serves the managed tenant-deployment routes over `deployments`.
    /// Without it they answer 409 `tenant_deployments_not_configured`.
    pub(in crate::server) fn with_tenant_deployments(
        mut self,
        deployments: Arc<Deployments>,
    ) -> Self {
        self.tenant_deployments = Some(deployments);
        self
    }

    /// Serves the managed tenant-deployment routes over an opened
    /// `--tenant-deployments-file` (vlite-server).
    pub fn with_managed_tenant_deployments(self, deployments: &TenantDeployments) -> Self {
        self.with_tenant_deployments(deployments.0.clone())
    }
}

// ---------------------------------------------------------------- runtime

/// A validated `--tenant-deployments-file`: the control-state source the
/// routes work on (`{target_file, object, shared_key_file?}`, the
/// `control-state` CLI's inputs). Nothing is contacted yet.
pub struct TenantDeploymentsConfig {
    source: control_state::Source,
}

fn runtime_invalid(detail: impl std::fmt::Display) -> String {
    format!("tenant_deployments_invalid: {detail}")
}

impl TenantDeploymentsConfig {
    pub fn read(path: &std::path::Path) -> Result<Self, String> {
        use crate::server::gateway_trust::{MAX_CONFIG_BYTES, read_bounded};
        let bytes = read_bounded(path, MAX_CONFIG_BYTES, "tenant deployments file")
            .map_err(runtime_invalid)?;
        let source: control_state::Source =
            serde_json::from_slice(&bytes).map_err(runtime_invalid)?;
        source.validate().map_err(runtime_invalid)?;
        Ok(Self { source })
    }

    /// Takes the admin directory's control-state lock for the process's
    /// lifetime (so `tenant-deployment` and `control-state` cannot change
    /// the tenants this server holds; else `admin_dir_busy`) and builds the
    /// store client. Writes nothing to the store and contacts nothing.
    /// Blocking: call off the async runtime.
    pub fn open(self, admin_directory: &std::path::Path) -> Result<TenantDeployments, String> {
        use crate::server::gateway_mode::CONTROL_STATE_LOCK;
        let io = |what: &'static str| {
            move |error: std::io::Error| format!("admin_dir_invalid: {what}: {}", error.kind())
        };
        std::fs::create_dir_all(admin_directory).map_err(io("admin directory"))?;
        let directory = std::fs::canonicalize(admin_directory).map_err(io("admin directory"))?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(directory.join(CONTROL_STATE_LOCK))
            .map_err(io("control-state lock"))?;
        lock.try_lock().map_err(|_| {
            "admin_dir_busy: another process holds this admin directory's control-state lock"
                .to_owned()
        })?;
        let deployments = Deployments::connect(&self.source)
            .map_err(|error| format!("{}: {}", error.code, error.message))?;
        Ok(TenantDeployments(Arc::new(Deployments {
            _lock: Some(lock),
            ..deployments
        })))
    }
}

/// An opened `--tenant-deployments-file`, for
/// `Service::with_managed_tenant_deployments`.
pub struct TenantDeployments(Arc<Deployments>);

// --------------------------------------------------------------- requests

/// An intent submission: the typed request plus the exact UTF-8 content of
/// every profile, rule or policy it references by digest.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Submission {
    idempotency_key: String,
    request: Payload,
    #[serde(default)]
    contents: Vec<String>,
}

/// A generation-bound action request. The key is never part of the
/// request digest or of any stored record.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ActionRequest {
    #[serde(skip_serializing)]
    idempotency_key: String,
    expected_operation_generation: u64,
    expected_lifecycle_generation: u64,
    #[serde(default)]
    expected_receipt: Option<String>,
    #[serde(default)]
    ack_irreversible: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verb {
    Resume,
    Cancel,
    AcknowledgeDelivery,
    ReplaceCredentials,
    Rollback,
    Release,
}

impl Verb {
    const ALL: [Self; 6] = [
        Self::Resume,
        Self::Cancel,
        Self::AcknowledgeDelivery,
        Self::ReplaceCredentials,
        Self::Rollback,
        Self::Release,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Resume => "resume",
            Self::Cancel => "cancel",
            Self::AcknowledgeDelivery => "acknowledge-delivery",
            Self::ReplaceCredentials => "replace-credentials",
            Self::Rollback => "rollback",
            Self::Release => "release",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|verb| verb.name() == name)
    }
}

/// One persisted action, under `actions/<sha256(principal, key)>.json`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionRecord {
    schema: String,
    operation_id: String,
    principal: String,
    action: String,
    request_digest: String,
    accepted_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
}

fn not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "operation_not_found",
        "no such operation",
    )
}

fn unavailable_transition(message: &'static str) -> ApiError {
    conflict("transition_unavailable", message)
}

// ------------------------------------------------------------------- core

/// Checks a submission against its route, then accepts it (ITEM-001).
fn accept(
    deployments: &Deployments,
    actor: &str,
    kind: Kind,
    tenant: Option<&str>,
    submission: Submission,
    now: u64,
) -> Result<Accepted, ApiError> {
    let payload = submission.request;
    if payload.kind() != kind || payload.tenant_id() != tenant {
        return Err(invalid(
            "the request kind or tenant does not match the route",
        ));
    }
    // The tenant store's bound, checked before anything is persisted.
    if let Payload::Create { name, .. } = &payload
        && name.len() > 64
    {
        return Err(invalid(
            "the tenant name must be 1..64 printable characters",
        ));
    }
    let contents: Vec<Vec<u8>> = submission
        .contents
        .into_iter()
        .map(String::into_bytes)
        .collect();
    deployments
        .ops
        .accept(actor, &submission.idempotency_key, &payload, &contents, now)
}

/// `create` only: the local tenant under the persisted ID (idempotent).
/// The caller serializes tenant mutations.
fn local_tenant(
    store: &tenants::Store,
    log: Option<&audit::Log>,
    intent: &Intent,
) -> Result<tenants::Tenant, ApiError> {
    let Payload::Create {
        name, max_vectors, ..
    } = &intent.payload
    else {
        return Err(state_invalid());
    };
    store.create_with_id(
        log,
        &intent.tenant_id,
        name,
        *max_vectors,
        &intent.operation_id,
    )
}

/// Publishes a `create` reservation, with the tenant's metadata, unrouted,
/// in one generation unless the operation already ended. Every other kind
/// reserves nothing here: only its plan-anchored actuator reserves, in a
/// CAS bound to the released predecessor and route its plan read
/// (`reserve_after`). A planless slot would be refused by that actuator, and
/// cancelling it would leave a cancelled anchor without a plan for later
/// operations to follow.
fn reserve_for(
    deployments: &Deployments,
    intent: &Intent,
    tenant: Option<&tenants::Tenant>,
    now: u64,
) -> Result<(), ApiError> {
    if deployments.ops.terminal(&intent.operation_id)?.is_some() {
        return Ok(());
    }
    let Some(tenant) = tenant else {
        return Ok(());
    };
    lifecycle_cas(
        &deployments.objects,
        now,
        |snapshot| holds(snapshot, intent).is_some(),
        |current| {
            current.with_tenant(tenant)?.reserve(
                &intent.tenant_id,
                &intent.operation_id,
                Kind::Create,
                &intent.request_digest,
            )
        },
    )
    .map(drop)
}

/// The values an action must quote to act on the current state.
#[derive(Debug, PartialEq, Eq)]
struct Tokens {
    operation_generation: u64,
    lifecycle_generation: u64,
    receipt: Option<String>,
}

pub(super) fn receipt_digest(receipt: &Receipt) -> String {
    sha256_hex(&serde_json::to_vec(receipt).expect("a receipt always serializes"))
}

fn tokens(
    intent: &Intent,
    snapshot: &Snapshot,
    journal: &Journal,
    terminal: Option<&Terminal>,
) -> Tokens {
    let held = match holds(snapshot, intent) {
        Some(Slot::Reserved {
            receipt: Some(receipt),
            ..
        }) => Some(receipt_digest(receipt)),
        _ => None,
    };
    let sealed = match terminal.map(|t| &t.outcome) {
        Some(TerminalOutcome::Succeeded { receipt }) => Some(receipt_digest(receipt)),
        _ => None,
    };
    Tokens {
        operation_generation: journal.generation,
        lifecycle_generation: snapshot
            .lifecycle(&intent.tenant_id)
            .map_or(0, |entry| entry.generation),
        receipt: held.or(sealed),
    }
}

/// The requested state with secret version names reduced to a count.
fn desired(payload: &Payload) -> Value {
    let mut value = serde_json::to_value(payload).unwrap_or(Value::Null);
    if let Payload::Create {
        secret_versions, ..
    } = payload
    {
        value["secret_versions"] = json!(secret_versions.len());
    }
    value
}

/// The redacted status view: never the idempotency key or its digest,
/// retained content, tokens or secret version names.
fn render(
    intent: &Intent,
    snapshot: &Snapshot,
    journal: &Journal,
    terminal: Option<&Terminal>,
) -> Value {
    let slot = holds(snapshot, intent);
    let (reserved, cancelling, activated) = match slot {
        Some(Slot::Reserved {
            cancellation_requested,
            receipt,
            ..
        }) => (true, *cancellation_requested, receipt.is_some()),
        _ => (false, false, false),
    };
    let (phase, next_step) = match terminal.map(|t| &t.outcome) {
        Some(TerminalOutcome::Succeeded { .. }) => ("succeeded", "none: the operation ended"),
        Some(TerminalOutcome::Cancelled) => ("cancelled", "none: the operation ended"),
        Some(TerminalOutcome::Retired) => (
            "retired",
            "none: the route is removed, the apps are stopped and all data is retained",
        ),
        None if activated => (
            "activated",
            "retire the tenant to undo; delivery and release handlers are not available yet",
        ),
        None if cancelling => (
            "cancelling",
            "cancel again to seal and release the reservation",
        ),
        None if reserved && pins_sets(&intent.payload) => (
            "reserved",
            "reader fleets are blocked until reader pin scope is proven (ITEM-008); cancel to release the reservation",
        ),
        None if reserved => (
            "reserved",
            "cloud execution is blocked (OD-002); cancel to release the reservation",
        ),
        None if !matches!(intent.payload, Payload::Create { .. }) && pins_sets(&intent.payload) => {
            (
                "accepted",
                "nothing is reserved: only the plan-anchored actuator reserves, and reader fleets are blocked until reader pin scope is proven (ITEM-008)",
            )
        }
        None if !matches!(intent.payload, Payload::Create { .. }) => (
            "accepted",
            "nothing is reserved: only the plan-anchored actuator reserves, and cloud execution is blocked (OD-002)",
        ),
        None => (
            "accepted",
            "resubmit the same request to reserve the tenant",
        ),
    };
    let tokens = tokens(intent, snapshot, journal, terminal);
    let uncertain = journal
        .steps
        .iter()
        .filter(|step| step.outcome == StepOutcome::Uncertain {})
        .count();
    json!({
        "operation_id": intent.operation_id,
        "kind": intent.kind(),
        "tenant_id": intent.tenant_id,
        "phase": phase,
        "next_step": next_step,
        "operation_generation": tokens.operation_generation,
        "lifecycle_generation": tokens.lifecycle_generation,
        "control_generation": snapshot.generation(),
        "receipt_digest": tokens.receipt,
        "created_unix_ms": intent.created_unix_ms,
        "desired": desired(&intent.payload),
        "observed": {
            "reserved": reserved,
            "routed": snapshot.record_for(&intent.tenant_id).is_some(),
            "cancellation_requested": cancelling,
            "journal_steps": journal.steps.len(),
            "uncertain_steps": uncertain,
            "terminal": terminal.is_some(),
        },
    })
}

fn load(deployments: &Deployments, operation_id: &str) -> Result<Intent, ApiError> {
    // Also keeps a caller-chosen ID out of object names.
    if !is_hex(operation_id, 32) {
        return Err(not_found());
    }
    Ok(deployments.ops.load(operation_id)?.intent)
}

fn view(deployments: &Deployments, intent: &Intent) -> Result<Value, ApiError> {
    let journal = deployments.ops.journal(&intent.operation_id)?;
    let terminal = deployments.ops.terminal(&intent.operation_id)?;
    let (snapshot, _) = deployments.objects.read()?;
    Ok(render(intent, &snapshot, &journal, terminal.as_ref()))
}

fn status(deployments: &Deployments, operation_id: &str) -> Result<Value, ApiError> {
    view(deployments, &load(deployments, operation_id)?)
}

/// The stored action under `name`, refusing a different request under the
/// same key.
fn stored_action(
    deployments: &Deployments,
    name: &str,
    principal: &str,
    digest: &str,
) -> Result<Option<(ActionRecord, String)>, ApiError> {
    let Some((bytes, etag)) = deployments.ops.get(name)? else {
        return Ok(None);
    };
    let record: ActionRecord = Operations::parse(&bytes)?;
    if record.schema != SCHEMA || record.principal != principal {
        return Err(state_invalid());
    }
    if record.request_digest != digest {
        return Err(conflict(
            "action_request_conflict",
            "the idempotency key was already used for a different action request",
        ));
    }
    Ok(Some((record, etag)))
}

/// Refuses a verb whose handler is not available now; nothing is persisted.
/// `slot` is this operation's reservation, if it holds one.
fn available(
    verb: Verb,
    request: &ActionRequest,
    payload: &Payload,
    ended: bool,
    slot: Option<&Slot>,
) -> Result<(), ApiError> {
    match verb {
        Verb::Resume if !ended && pins_sets(payload) => Err(pin_scope_unproven()),
        Verb::Resume => Ok(()),
        Verb::Cancel if ended => Err(super::ended()),
        Verb::Cancel => match slot {
            Some(Slot::Reserved { receipt: None, .. }) => Ok(()),
            Some(_) => Err(conflict(
                "retirement_required",
                "the operation already activated its route; retire the tenant instead",
            )),
            None if matches!(payload, Payload::Create { .. }) => Err(unavailable_transition(
                "cancel needs this operation to hold the tenant reservation; resubmit the request to reserve it",
            )),
            None => Err(unavailable_transition(
                "cancel needs this operation to hold the tenant reservation; only its plan-anchored actuator reserves it",
            )),
        },
        Verb::AcknowledgeDelivery => Err(unavailable_transition(
            "delivery acknowledgement needs credential delivery (EPIC-004 create, EPIC-006 migration)",
        )),
        Verb::ReplaceCredentials => Err(unavailable_transition(
            "credential replacement needs credential delivery (EPIC-004 create, EPIC-006 migration)",
        )),
        Verb::Rollback => Err(unavailable_transition(
            "rollback needs migration orchestration (EPIC-006)",
        )),
        Verb::Release if !request.ack_irreversible => Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "irreversible_ack_required",
            "release is irreversible and needs ack_irreversible: true",
        )),
        Verb::Release => Err(unavailable_transition(
            "release needs migration orchestration (EPIC-006)",
        )),
    }
}

/// One generation-bound action (ITEM-004).
fn act(
    deployments: &Deployments,
    actor: &str,
    operation_id: &str,
    verb: &str,
    request: ActionRequest,
    now: u64,
) -> Result<Value, ApiError> {
    let verb = Verb::parse(verb).ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "tenant_deployment_action_unknown",
            "no such operation action",
        )
    })?;
    let key = &request.idempotency_key;
    if !text(actor) || !text(key) || key.len() > 256 {
        return Err(invalid("principal and idempotency key are required"));
    }
    if request
        .expected_receipt
        .as_deref()
        .is_some_and(|receipt| !is_hex(receipt, 64))
    {
        return Err(invalid("expected_receipt must be a SHA-256 hex digest"));
    }
    let intent = load(deployments, operation_id)?;
    let canonical = serde_json::to_vec(&request).map_err(|_| state_invalid())?;
    let mut digested = verb.name().as_bytes().to_vec();
    digested.push(0);
    digested.extend_from_slice(&canonical);
    let digest = sha256_hex(&digested);
    let name = deployments.ops.name(
        operation_id,
        &format!(
            "actions/{}.json",
            sha256_hex(format!("{actor}\0{key}").as_bytes())
        ),
    );
    let etag = match stored_action(deployments, &name, actor, &digest)? {
        Some((
            ActionRecord {
                result: Some(result),
                ..
            },
            _,
        )) => return Ok(result),
        // Accepted but never finished: apply again (every apply is
        // idempotent); the state already moved, so tokens are not checked.
        Some((_, etag)) => etag,
        None => {
            let journal = deployments.ops.journal(operation_id)?;
            let terminal = deployments.ops.terminal(operation_id)?;
            let (snapshot, _) = deployments.objects.read()?;
            let current = tokens(&intent, &snapshot, &journal, terminal.as_ref());
            let quoted = Tokens {
                operation_generation: request.expected_operation_generation,
                lifecycle_generation: request.expected_lifecycle_generation,
                receipt: request.expected_receipt.clone(),
            };
            if quoted != current {
                return Err(conflict(
                    "operation_generation_stale",
                    "the operation changed since it was read; read its status again",
                ));
            }
            available(
                verb,
                &request,
                &intent.payload,
                terminal.is_some(),
                holds(&snapshot, &intent),
            )?;
            let record = ActionRecord {
                schema: SCHEMA.to_owned(),
                operation_id: operation_id.to_owned(),
                principal: actor.to_owned(),
                action: verb.name().to_owned(),
                request_digest: digest.clone(),
                accepted_unix_ms: now,
                result: None,
            };
            let bytes = serde_json::to_vec(&record).map_err(|_| state_invalid())?;
            deployments.ops.create(&name, &bytes)?;
            match stored_action(deployments, &name, actor, &digest)?.ok_or_else(indeterminate)? {
                (
                    ActionRecord {
                        result: Some(result),
                        ..
                    },
                    _,
                ) => return Ok(result),
                (_, etag) => etag,
            }
        }
    };
    match verb {
        Verb::Resume => {}
        Verb::Cancel => {
            match deployments.ops.terminal(operation_id)? {
                Some(Terminal {
                    outcome: TerminalOutcome::Succeeded { .. },
                    ..
                }) => return Err(super::ended()),
                Some(_) => {}
                None => request_cancel(&deployments.objects, &intent, now)?,
            }
            // Idempotent: completes a release an earlier attempt sealed.
            deployments
                .ops
                .release(&deployments.objects, &intent, now)?;
        }
        // Refused by `available` before any record was written.
        _ => return Err(state_invalid()),
    }
    let result = json!({"action": verb.name(), "operation": view(deployments, &intent)?});
    let finished = ActionRecord {
        schema: SCHEMA.to_owned(),
        operation_id: operation_id.to_owned(),
        principal: actor.to_owned(),
        action: verb.name().to_owned(),
        request_digest: digest,
        accepted_unix_ms: now,
        result: Some(result.clone()),
    };
    // Best effort: a lost write only means a replay applies again.
    if let Ok(bytes) = serde_json::to_vec(&finished) {
        let _ = deployments.ops.store.put(&name, &bytes, Cond::Match(&etag));
    }
    Ok(result)
}

fn audit_event(log: Option<&audit::Log>, event: Value) -> bool {
    log.is_none_or(|log| log.append(event, false).is_ok())
}

fn accepted_event(actor: &str, accepted: &Accepted) -> Value {
    json!({
        "event": "tenant_deployment_accepted",
        "actor": actor,
        "operation_id": accepted.intent.operation_id,
        "kind": accepted.intent.kind(),
        "tenant_id": accepted.intent.tenant_id,
    })
}

fn action_event(actor: &str, operation_id: &str, action: &str) -> Value {
    json!({
        "event": "tenant_deployment_action",
        "actor": actor,
        "operation_id": operation_id,
        "action": action,
    })
}

// ------------------------------------------------------------------- REST

fn rest_actor(
    service: &Service,
    role: Option<Extension<Role>>,
    principal: Option<Extension<users::Principal>>,
) -> Result<String, ApiError> {
    match role {
        Some(Extension(role)) if !service.admin.is_open() => Ok(audit::actor(
            &role,
            principal.as_ref().map(|Extension(principal)| principal),
        )),
        _ => Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "admin_auth_required",
            "managed tenant deployments require configured administrator authentication",
        )),
    }
}

fn configured(service: &Service) -> Result<Arc<Deployments>, ApiError> {
    service.tenant_deployments.clone().ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "tenant_deployments_not_configured",
            "this server has no managed tenant deployment store",
        )
    })
}

fn record(service: &Service, event: Value) {
    if !audit_event(service.admin.audit_log.as_deref(), event) {
        service
            .metrics
            .audit_failures
            .fetch_add(1, Ordering::Relaxed);
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(crate::server::internal)?
}

fn now() -> u64 {
    crate::server::admin::now_ms() as u64
}

async fn submit(
    service: Service,
    role: Option<Extension<Role>>,
    principal: Option<Extension<users::Principal>>,
    kind: Kind,
    tenant: Option<String>,
    input: Result<Json<Submission>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let actor = rest_actor(&service, role, principal)?;
    let input = body(input)?;
    let deployments = configured(&service)?;
    let store = match kind {
        Kind::Create => Some(service.admin.tenants.clone().ok_or_else(|| {
            ApiError::new(
                StatusCode::CONFLICT,
                "tenants_disabled",
                "tenants require admin authentication and an admin directory",
            )
        })?),
        _ => None,
    };
    let now = now();
    let accepted = {
        let (deployments, actor) = (deployments.clone(), actor.clone());
        blocking(move || accept(&deployments, &actor, kind, tenant.as_deref(), input, now)).await?
    };
    let intent = accepted.intent.clone();
    let ended = {
        let (deployments, id) = (deployments.clone(), intent.operation_id.clone());
        blocking(move || deployments.ops.terminal(&id))
            .await?
            .is_some()
    };
    if !ended {
        let tenant = match store {
            // Serialized with the legacy tenant mutations.
            Some(store) => {
                let (log, intent) = (service.admin.audit_log.clone(), intent.clone());
                Some(
                    service
                        .run(move |_db| local_tenant(&store, log.as_deref(), &intent))
                        .await?,
                )
            }
            None => None,
        };
        let (deployments, intent) = (deployments.clone(), intent.clone());
        blocking(move || reserve_for(&deployments, &intent, tenant.as_ref(), now)).await?;
    }
    if !accepted.duplicate {
        record(&service, accepted_event(&actor, &accepted));
    }
    let view = blocking(move || view(&deployments, &intent)).await?;
    let status = if ended {
        StatusCode::OK
    } else {
        StatusCode::ACCEPTED
    };
    Ok((
        status,
        Json(json!({"duplicate": accepted.duplicate, "operation": view})),
    ))
}

pub(super) async fn create(
    State(service): State<Service>,
    role: Option<Extension<Role>>,
    principal: Option<Extension<users::Principal>>,
    input: Result<Json<Submission>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    submit(service, role, principal, Kind::Create, None, input).await
}

pub(super) async fn readers(
    State(service): State<Service>,
    Path(tenant): Path<String>,
    role: Option<Extension<Role>>,
    principal: Option<Extension<users::Principal>>,
    input: Result<Json<Submission>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    submit(
        service,
        role,
        principal,
        Kind::SetReaders,
        Some(tenant),
        input,
    )
    .await
}

pub(super) async fn migrate(
    State(service): State<Service>,
    Path(tenant): Path<String>,
    role: Option<Extension<Role>>,
    principal: Option<Extension<users::Principal>>,
    input: Result<Json<Submission>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    submit(service, role, principal, Kind::Migrate, Some(tenant), input).await
}

pub(super) async fn retire(
    State(service): State<Service>,
    Path(tenant): Path<String>,
    role: Option<Extension<Role>>,
    principal: Option<Extension<users::Principal>>,
    input: Result<Json<Submission>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    submit(service, role, principal, Kind::Retire, Some(tenant), input).await
}

pub(super) async fn operation_status(
    State(service): State<Service>,
    Path(operation): Path<String>,
    role: Option<Extension<Role>>,
    principal: Option<Extension<users::Principal>>,
) -> Result<Json<Value>, ApiError> {
    rest_actor(&service, role, principal)?;
    let deployments = configured(&service)?;
    let view = blocking(move || status(&deployments, &operation)).await?;
    Ok(Json(json!({"operation": view})))
}

pub(super) async fn operation_action(
    State(service): State<Service>,
    Path((operation, action)): Path<(String, String)>,
    role: Option<Extension<Role>>,
    principal: Option<Extension<users::Principal>>,
    input: Result<Json<ActionRequest>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let actor = rest_actor(&service, role, principal)?;
    let input = body(input)?;
    let deployments = configured(&service)?;
    let result = {
        let (actor, operation, action) = (actor.clone(), operation.clone(), action.clone());
        blocking(move || act(&deployments, &actor, &operation, &action, input, now())).await?
    };
    record(&service, action_event(&actor, &operation, &action));
    Ok(Json(result))
}

// -------------------------------------------------------------------- CLI

const USAGE: &str = "vlite-server tenant-deployment <create|readers|migrate|retire> --request-file <file> [--tenant <id>] | status --operation <id> | <resume|cancel|acknowledge-delivery|replace-credentials|rollback|release> --operation <id> --request-file <file>; every verb also takes --target-file <file> --object <name> --admin-dir <dir> [--shared-key-file <file>]";

/// Bound for a request file: the operation records' own bound.
fn read_request(path: &std::path::Path) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|error| format!("invalid_arguments: --request-file: {}", error.kind()))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("invalid_arguments: --request-file is too large".to_owned());
    }
    Ok(bytes)
}

/// What one command line asks for, after its flags are read.
pub(super) enum Command {
    Submit {
        kind: Kind,
        tenant: Option<String>,
        request: Vec<u8>,
    },
    Status {
        operation: String,
    },
    Act {
        operation: String,
        verb: String,
        request: Vec<u8>,
    },
}

const CLI_ACTOR: &str = "cli:tenant-deployment";

/// Runs one command against open stores (blocking); the CLI's testable core.
pub(super) fn run(
    deployments: &Deployments,
    store: &tenants::Store,
    log: Option<&audit::Log>,
    command: Command,
    now: u64,
) -> Result<Value, ApiError> {
    fn parse<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, ApiError> {
        serde_json::from_slice(bytes)
            .map_err(|_| invalid("the request file is not a valid request"))
    }
    match command {
        Command::Submit {
            kind,
            tenant,
            request,
        } => {
            let accepted = accept(
                deployments,
                CLI_ACTOR,
                kind,
                tenant.as_deref(),
                parse(&request)?,
                now,
            )?;
            let intent = &accepted.intent;
            if deployments.ops.terminal(&intent.operation_id)?.is_none() {
                let tenant = match kind {
                    Kind::Create => Some(local_tenant(store, log, intent)?),
                    _ => None,
                };
                reserve_for(deployments, intent, tenant.as_ref(), now)?;
            }
            if !accepted.duplicate {
                audit_event(log, accepted_event(CLI_ACTOR, &accepted));
            }
            Ok(json!({"duplicate": accepted.duplicate, "operation": view(deployments, intent)?}))
        }
        Command::Status { operation } => Ok(json!({"operation": status(deployments, &operation)?})),
        Command::Act {
            operation,
            verb,
            request,
        } => {
            let result = act(
                deployments,
                CLI_ACTOR,
                &operation,
                &verb,
                parse(&request)?,
                now,
            )?;
            audit_event(log, action_event(CLI_ACTOR, &operation, &verb));
            Ok(result)
        }
    }
}

/// `vlite-server tenant-deployment`: the managed tenant-deployment surface
/// from the command line (ITEM-005), with the REST routes' contracts. One
/// command at a time runs per admin directory (`control-state.lock`, shared
/// with `control-state`, else `admin_dir_busy`). Blocking: call off the
/// async runtime.
pub fn tenant_deployment_command(args: &[OsString]) -> Result<Value, String> {
    use crate::server::gateway_mode::{CONTROL_STATE_LOCK, flags, utf8};
    let usage = |detail: &str| format!("invalid_arguments: {detail}; usage: {USAGE}");
    let (verb, rest) = args
        .split_first()
        .ok_or_else(|| usage("a verb is required"))?;
    let verb = verb
        .to_str()
        .ok_or_else(|| usage("the verb is not recognized"))?
        .to_owned();
    let kind = match verb.as_str() {
        "create" => Some(Kind::Create),
        "readers" => Some(Kind::SetReaders),
        "migrate" => Some(Kind::Migrate),
        "retire" => Some(Kind::Retire),
        "status" => None,
        other if Verb::parse(other).is_some() => None,
        _ => return Err(usage("the verb is not recognized")),
    };
    let mut allowed = vec![
        "--target-file",
        "--object",
        "--shared-key-file",
        "--admin-dir",
    ];
    match kind {
        Some(Kind::Create) => allowed.push("--request-file"),
        Some(_) => allowed.extend(["--request-file", "--tenant"]),
        None if verb == "status" => allowed.push("--operation"),
        None => allowed.extend(["--operation", "--request-file"]),
    }
    let mut values = flags(rest, &allowed, USAGE)?;
    let mut required = |name: &str| {
        values
            .remove(name)
            .ok_or_else(|| usage(&format!("{name} is required")))
    };
    let source = control_state::Source {
        target_file: PathBuf::from(required("--target-file")?),
        object: utf8("--object", required("--object")?)?,
        shared_key_file: None,
    };
    let admin_dir = PathBuf::from(required("--admin-dir")?);
    let command = match kind {
        Some(kind) => Command::Submit {
            kind,
            tenant: match kind {
                Kind::Create => None,
                _ => Some(utf8("--tenant", required("--tenant")?)?),
            },
            request: read_request(&PathBuf::from(required("--request-file")?))?,
        },
        None if verb == "status" => Command::Status {
            operation: utf8("--operation", required("--operation")?)?,
        },
        None => Command::Act {
            operation: utf8("--operation", required("--operation")?)?,
            request: read_request(&PathBuf::from(required("--request-file")?))?,
            verb,
        },
    };
    let source = control_state::Source {
        shared_key_file: values.remove("--shared-key-file").map(PathBuf::from),
        ..source
    };
    source
        .validate()
        .map_err(|error| format!("invalid_arguments: {error}"))?;
    let directory = std::fs::canonicalize(&admin_dir)
        .ok()
        .filter(|path| path.is_dir())
        .ok_or_else(|| usage("--admin-dir must be an existing admin directory"))?;
    let io = |what: &'static str| {
        move |error: std::io::Error| format!("admin_dir_invalid: {what}: {}", error.kind())
    };
    // Held until the command returns; the OS releases it if the process dies.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(directory.join(CONTROL_STATE_LOCK))
        .map_err(io("control-state lock"))?;
    lock.try_lock().map_err(|_| {
        "admin_dir_busy: another control-state command is running on this admin directory"
            .to_owned()
    })?;
    let log = audit::Log::open(&directory).map_err(io("audit log"))?;
    let store = tenants::Store::load(&directory).map_err(io("tenants"))?;
    let deployments = Deployments::connect(&source)
        .map_err(|error| format!("{}: {}", error.code, error.message))?;
    let result = run(&deployments, &store, Some(&log), command, now())
        .map_err(|error| format!("{}: {}", error.code, error.message));
    drop(lock);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::cloud_backup::fake::Memory;
    use crate::server::control_state::Route;
    use crate::server::{BearerToken, credentials};
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    const CONTROL: &str = "control/state.json";
    const PREFIX: &str = "tenant-operations";
    const TOKEN: &str = "tenant_deployment_admin_0123456789_ABCDEFGHIJKLMNOP";
    const ROUTED: &str = "fedcba9876543210fedcba9876543210";
    const NOW: u64 = 1_700_000_000_000;
    const PROFILE: &str = r#"{"image":"vlite@sha256:0","cpu":"approved-by-profile"}"#;
    const INIT: &str = r#"{"dimension":"from-approved-profile"}"#;
    const SECRET: &str = "archive-sas@3";

    fn sha(text: &str) -> String {
        sha256_hex(text.as_bytes())
    }

    fn create_request(key: &str, name: &str) -> Value {
        json!({
            "idempotency_key": key,
            "request": {
                "kind": "create",
                "name": name,
                "max_vectors": 1000,
                "writer_profile": sha(PROFILE),
                "initialization": sha(INIT),
                "resources": ["/subscriptions/s/resourceGroups/g/providers/x/apps/w"],
                "secret_versions": [SECRET],
            },
            "contents": [PROFILE, INIT],
        })
    }

    struct Fixture {
        directory: tempfile::TempDir,
        memory: Arc<Memory>,
        service: Service,
        app: Router,
    }

    impl Fixture {
        fn deployments(&self) -> Deployments {
            Deployments::new(self.memory.clone(), CONTROL, PREFIX)
        }

        fn snapshot(&self) -> Snapshot {
            self.deployments().objects.read().unwrap().0
        }

        async fn call(
            &self,
            method: &str,
            path: &str,
            input: Option<Value>,
        ) -> (StatusCode, Value) {
            self.call_as(Some(TOKEN), method, path, input).await
        }

        async fn call_as(
            &self,
            token: Option<&str>,
            method: &str,
            path: &str,
            input: Option<Value>,
        ) -> (StatusCode, Value) {
            let mut request = Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json");
            if let Some(token) = token {
                request = request.header("authorization", format!("Bearer {token}"));
            }
            let body = input.map_or_else(Body::empty, |input| Body::from(input.to_string()));
            let response = self
                .app
                .clone()
                .oneshot(request.body(body).unwrap())
                .await
                .unwrap();
            let status = response.status();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            (
                status,
                serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            )
        }

        async fn create(&self, key: &str) -> Value {
            let (status, body) = self
                .call(
                    "POST",
                    "/v1/admin/tenant-deployments",
                    Some(create_request(key, "contoso")),
                )
                .await;
            assert_eq!(status, StatusCode::ACCEPTED, "{body}");
            body["operation"].clone()
        }

        async fn action(
            &self,
            operation: &Value,
            verb: &str,
            key: &str,
            extra: Value,
        ) -> (StatusCode, Value) {
            let mut input = json!({
                "idempotency_key": key,
                "expected_operation_generation": operation["operation_generation"],
                "expected_lifecycle_generation": operation["lifecycle_generation"],
                "expected_receipt": operation["receipt_digest"],
            });
            for (name, value) in extra.as_object().unwrap() {
                input[name] = value.clone();
            }
            let path = format!(
                "/v1/admin/tenant-deployment-operations/{}/{verb}",
                operation["operation_id"].as_str().unwrap()
            );
            self.call("POST", &path, Some(input)).await
        }
    }

    /// An authenticated server whose control state routes `ROUTED` shared.
    fn fixture_with(configured: bool, token: Option<&str>) -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let store = tenants::Store::load(directory.path()).unwrap();
        store
            .create_with_id(None, ROUTED, "routed", None, "seed")
            .unwrap();
        let credentials = credentials::Store::load(directory.path()).unwrap();
        let memory = Arc::new(Memory::default());
        Objects::new(memory.clone(), CONTROL)
            .create(
                Snapshot::import(&credentials, Some(&store))
                    .unwrap()
                    .with_directory(vec![Route::shared_for_test(ROUTED)]),
                NOW,
            )
            .unwrap();
        let mut service = Service::new(
            crate::Database::create_in_memory(1, crate::CreateOptions::default()).unwrap(),
        );
        let admin = Arc::get_mut(&mut service.admin).unwrap();
        admin.auth.lock().unwrap().admin = token.map(|token| BearerToken::new(token).unwrap());
        admin.tenants = Some(Arc::new(store));
        admin.audit_log = Some(Arc::new(audit::Log::open(directory.path()).unwrap()));
        admin.directory = Some(directory.path().to_owned());
        if configured {
            service = service.with_tenant_deployments(Arc::new(Deployments::new(
                memory.clone(),
                CONTROL,
                PREFIX,
            )));
        }
        let app = crate::server::configured_routes(service.clone());
        Fixture {
            directory,
            memory,
            service,
            app,
        }
    }

    fn fixture() -> Fixture {
        fixture_with(true, Some(TOKEN))
    }

    #[tokio::test]
    async fn create_reserves_a_non_routable_tenant_under_the_persisted_id() {
        let f = fixture();
        let operation = f.create("onboard-contoso").await;
        let tenant = operation["tenant_id"].as_str().unwrap().to_owned();
        assert_eq!(operation["phase"], "reserved");
        assert_eq!(operation["kind"], "create");
        assert_eq!(operation["lifecycle_generation"], 1);
        assert_eq!(operation["observed"]["routed"], false);
        let snapshot = f.snapshot();
        assert!(
            snapshot.record_for(&tenant).is_none(),
            "no route is published"
        );
        assert!(matches!(
            snapshot.lifecycle(&tenant).map(|entry| &entry.slot),
            Some(Slot::Reserved {
                kind: Kind::Create,
                receipt: None,
                ..
            })
        ));
        let local = f.service.admin.tenants.as_ref().unwrap();
        assert_eq!(local.name(&tenant).as_deref(), Some("contoso"));
        let published = serde_json::to_value(&snapshot).unwrap();
        assert!(
            published["tenants"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["id"] == tenant.as_str() && t["name"] == "contoso"),
            "{published}"
        );
        // The legacy store reloads with the new tenant (committed to disk).
        let reloaded = tenants::Store::load(f.directory.path()).unwrap();
        assert_eq!(reloaded.name(&tenant).as_deref(), Some("contoso"));
        let audit = std::fs::read_to_string(f.directory.path().join("audit.jsonl")).unwrap();
        assert!(audit.contains("tenant_deployment_accepted"));
        assert!(audit.contains(&format!(
            "\"operation_id\":\"{}\"",
            operation["operation_id"].as_str().unwrap()
        )));
    }

    #[tokio::test]
    async fn a_replayed_create_returns_the_same_operation_and_a_changed_one_is_refused() {
        let f = fixture();
        let first = f.create("onboard-contoso").await;
        let (status, again) = f
            .call(
                "POST",
                "/v1/admin/tenant-deployments",
                Some(create_request("onboard-contoso", "contoso")),
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(again["duplicate"], true);
        assert_eq!(again["operation"]["operation_id"], first["operation_id"]);
        assert_eq!(again["operation"]["tenant_id"], first["tenant_id"]);
        assert_eq!(
            again["operation"]["lifecycle_generation"], 1,
            "no second reservation"
        );
        let tenants = f
            .service
            .admin
            .tenants
            .as_ref()
            .unwrap()
            .snapshot()
            .unwrap();
        assert_eq!(tenants.len(), 2, "one tenant per operation");
        let (status, body) = f
            .call(
                "POST",
                "/v1/admin/tenant-deployments",
                Some(create_request("onboard-contoso", "fabrikam")),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["code"], "operation_request_conflict");
    }

    #[tokio::test]
    async fn status_is_redacted() {
        let f = fixture();
        let operation = f.create("onboard-contoso-secret-key").await;
        let path = format!(
            "/v1/admin/tenant-deployment-operations/{}",
            operation["operation_id"].as_str().unwrap()
        );
        let (status, body) = f.call("GET", &path, None).await;
        assert_eq!(status, StatusCode::OK);
        let text = body.to_string();
        for secret in [
            "onboard-contoso-secret-key",
            &sha("onboard-contoso-secret-key"),
            SECRET,
            PROFILE,
            TOKEN,
            "idempotency",
        ] {
            assert!(!text.contains(secret), "status leaks {secret}: {text}");
        }
        assert_eq!(body["operation"]["desired"]["secret_versions"], 1);
        assert_eq!(body["operation"]["desired"]["writer_profile"], sha(PROFILE));
        let (status, body) = f
            .call(
                "GET",
                "/v1/admin/tenant-deployment-operations/..%2Fcontents",
                None,
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    }

    #[tokio::test]
    async fn actions_are_idempotent_and_generation_bound() {
        let f = fixture();
        let operation = f.create("onboard-contoso").await;
        let (status, resumed) = f.action(&operation, "resume", "resume-1", json!({})).await;
        assert_eq!(status, StatusCode::OK, "{resumed}");
        assert_eq!(resumed["action"], "resume");
        // A replay returns the stored result, even with stale tokens later.
        let (status, again) = f.action(&operation, "resume", "resume-1", json!({})).await;
        assert_eq!((status, &again), (StatusCode::OK, &resumed));
        // The same key for a different action request is refused.
        let (status, body) = f.action(&operation, "cancel", "resume-1", json!({})).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["code"], "action_request_conflict");
        // A stale generation is refused and not persisted.
        let mut stale = operation.clone();
        stale["lifecycle_generation"] = json!(0);
        let (status, body) = f.action(&stale, "cancel", "cancel-stale", json!({})).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["code"], "operation_generation_stale");
        let (status, body) = f
            .action(&operation, "cancel", "cancel-stale", json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["operation"]["phase"], "cancelled");
        // The stored cancel replays; a stale acknowledgement under a new key does not.
        let (status, replay) = f
            .action(&operation, "cancel", "cancel-stale", json!({}))
            .await;
        assert_eq!((status, &replay), (StatusCode::OK, &body));
        let (status, body) = f
            .action(&operation, "cancel", "cancel-again", json!({}))
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["code"], "operation_generation_stale");
        let (status, body) = f
            .call("POST", "/v1/admin/tenant-deployment-operations/00000000000000000000000000000000/explode", Some(json!({
                "idempotency_key": "k", "expected_operation_generation": 0, "expected_lifecycle_generation": 0})))
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "tenant_deployment_action_unknown");
    }

    #[tokio::test]
    async fn cancel_seals_before_activation_and_requires_retirement_after() {
        let f = fixture();
        let operation = f.create("onboard-contoso").await;
        let (status, cancelled) = f.action(&operation, "cancel", "cancel-1", json!({})).await;
        assert_eq!(status, StatusCode::OK, "{cancelled}");
        let tenant = operation["tenant_id"].as_str().unwrap();
        assert!(matches!(
            f.snapshot().lifecycle(tenant).map(|entry| &entry.slot),
            Some(Slot::Released { .. })
        ));
        // Resubmitting the ended create does not reserve again.
        let (status, body) = f
            .call(
                "POST",
                "/v1/admin/tenant-deployments",
                Some(create_request("onboard-contoso", "contoso")),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["operation"]["phase"], "cancelled");

        let operation = f.create("onboard-fabrikam").await;
        let deployments = f.deployments();
        let intent = deployments
            .ops
            .load(operation["operation_id"].as_str().unwrap())
            .unwrap()
            .intent;
        let route = Route {
            reader: None,
            ..Route::dedicated_for_test(&intent.tenant_id, 1)
        };
        activate(
            &deployments.ops,
            &deployments.objects,
            &intent,
            &route,
            &"ab".repeat(32),
            NOW,
        )
        .unwrap();
        let path = format!(
            "/v1/admin/tenant-deployment-operations/{}",
            intent.operation_id
        );
        let (_, current) = f.call("GET", &path, None).await;
        assert_eq!(current["operation"]["phase"], "activated");
        assert!(current["operation"]["receipt_digest"].is_string());
        let (status, body) = f
            .action(&current["operation"], "cancel", "cancel-2", json!({}))
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["code"], "retirement_required");
        // Refused before persisting: the key is still free for another action.
        let (status, body) = f
            .action(&current["operation"], "resume", "cancel-2", json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["operation"]["phase"], "activated");
    }

    #[test]
    fn cancel_needs_this_operation_to_hold_an_unrouted_reservation() {
        let request: ActionRequest = serde_json::from_value(json!({
            "idempotency_key": "k",
            "expected_operation_generation": 0,
            "expected_lifecycle_generation": 0,
        }))
        .unwrap();
        let readers = Payload::SetReaders {
            tenant_id: ROUTED.into(),
            fleet: Fleet::Absent {},
        };
        let error = available(Verb::Cancel, &request, &readers, false, None).unwrap_err();
        assert_eq!(error.code, "transition_unavailable");
    }

    #[tokio::test]
    async fn unavailable_transitions_and_release_are_refused_before_persisting() {
        let f = fixture();
        let operation = f.create("onboard-contoso").await;
        for verb in ["acknowledge-delivery", "replace-credentials", "rollback"] {
            let (status, body) = f.action(&operation, verb, verb, json!({})).await;
            assert_eq!(status, StatusCode::CONFLICT, "{verb}");
            assert_eq!(body["error"]["code"], "transition_unavailable");
        }
        let (status, body) = f
            .action(&operation, "release", "release-1", json!({}))
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"]["code"], "irreversible_ack_required");
        let (status, body) = f
            .action(
                &operation,
                "release",
                "release-2",
                json!({"ack_irreversible": true}),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["code"], "transition_unavailable");
        // Nothing was persisted: the same keys may carry another action.
        let (status, body) = f.action(&operation, "resume", "release-1", json!({})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        // Generic resume never released the reservation.
        assert_eq!(body["operation"]["phase"], "reserved");
    }

    #[tokio::test]
    async fn intents_for_other_kinds_respect_routing_and_the_path() {
        let f = fixture();
        let readers = json!({
            "idempotency_key": "readers-1",
            "request": {"kind": "set_readers", "tenant_id": ROUTED, "fleet": {"state": "absent"}},
        });
        let path = format!("/v1/admin/tenant-deployments/{ROUTED}/readers");
        let (status, body) = f.call("POST", &path, Some(readers.clone())).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        assert_eq!(body["operation"]["kind"], "set_readers");
        assert_eq!(body["operation"]["observed"]["routed"], true);
        // The path and the request must name the same tenant and kind.
        let wrong = format!("/v1/admin/tenant-deployments/{}/readers", "0".repeat(32));
        let (status, body) = f.call("POST", &wrong, Some(readers)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"]["code"], "operation_request_invalid");
        let retire = json!({
            "idempotency_key": "retire-1",
            "request": {"kind": "retire", "tenant_id": ROUTED, "expected_receipt": "a".repeat(64),
                "reason": "test", "data_disposition": "retain"},
        });
        let path = format!("/v1/admin/tenant-deployments/{ROUTED}/readers");
        let (status, _) = f.call("POST", &path, Some(retire)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    /// ITEM-008: a reader fleet would pin sets with whole-record writes in
    /// the writer's coordination scope, so it cannot advance; it can still
    /// be read, and removing readers needs no pins. R1: a reader submission
    /// takes no slot (only its plan-anchored actuator reserves), so there is
    /// no planless reservation to cancel and later lifecycle anchoring is
    /// never poisoned.
    #[tokio::test]
    async fn reader_fleets_are_denied_until_pin_scope_is_proven() {
        let f = fixture();
        let path = format!("/v1/admin/tenant-deployments/{ROUTED}/readers");
        let present = json!({
            "idempotency_key": "readers-present",
            "request": {"kind": "set_readers", "tenant_id": ROUTED, "fleet": {
                "state": "present", "min_replicas": 1, "max_replicas": 2,
                "reader_profile": sha(PROFILE), "scaling_rule": sha(INIT),
                "read_routing": "writer_only"}},
            "contents": [PROFILE, INIT],
        });
        let before = f.snapshot().lifecycle(ROUTED).cloned();
        let (status, body) = f.call("POST", &path, Some(present.clone())).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let operation = body["operation"].clone();
        assert_eq!(operation["phase"], "accepted");
        assert_eq!(operation["observed"]["reserved"], false);
        let (_, again) = f.call("POST", &path, Some(present)).await;
        assert_eq!(
            again["operation"]["operation_id"],
            operation["operation_id"]
        );
        assert_eq!(f.snapshot().lifecycle(ROUTED).cloned(), before, "no slot");
        assert!(
            operation["next_step"]
                .as_str()
                .unwrap()
                .contains("pin scope"),
            "{operation}"
        );
        let (status, body) = f.action(&operation, "resume", "resume-1", json!({})).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["code"], "pin_scope_unproven");
        // Refused before any record: the key stays free for cancel, which
        // is refused too because nothing is reserved.
        let (status, body) = f.action(&operation, "cancel", "resume-1", json!({})).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"]["code"], "transition_unavailable");
        assert_eq!(f.snapshot().lifecycle(ROUTED).cloned(), before, "no anchor");
        let absent = json!({
            "idempotency_key": "readers-absent",
            "request": {"kind": "set_readers", "tenant_id": ROUTED, "fleet": {"state": "absent"}},
        });
        let (status, body) = f.call("POST", &path, Some(absent)).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let (status, body) = f
            .action(&body["operation"], "resume", "resume-2", json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    #[tokio::test]
    async fn surface_needs_configuration_and_administrator_authentication() {
        let unconfigured = fixture_with(false, Some(TOKEN));
        let (status, body) = unconfigured
            .call(
                "POST",
                "/v1/admin/tenant-deployments",
                Some(create_request("k", "contoso")),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["code"], "tenant_deployments_not_configured");
        let open = fixture_with(true, None);
        let (status, body) = open
            .call_as(
                None,
                "POST",
                "/v1/admin/tenant-deployments",
                Some(create_request("k", "contoso")),
            )
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "admin_auth_required");
        let f = fixture();
        let (status, _) = f
            .call_as(
                None,
                "POST",
                "/v1/admin/tenant-deployments",
                Some(create_request("k", "contoso")),
            )
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn legacy_create_is_unchanged() {
        let f = fixture();
        let (status, body) = f
            .call("POST", "/v1/admin/tenants", Some(json!({"name": "legacy"})))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let id = body["tenant"]["id"].as_str().unwrap();
        assert!(
            f.snapshot().lifecycle(id).is_none(),
            "legacy create reserves nothing"
        );
        assert!(
            f.memory
                .names()
                .iter()
                .all(|name| !name.starts_with(PREFIX)),
            "legacy create records no operation"
        );
    }

    #[test]
    fn create_with_id_is_idempotent_and_refuses_a_different_tenant() {
        let directory = tempfile::tempdir().unwrap();
        let store = tenants::Store::load(directory.path()).unwrap();
        let first = store
            .create_with_id(None, ROUTED, "a", Some(5), "op")
            .unwrap();
        let again = store
            .create_with_id(None, ROUTED, "a", Some(5), "op")
            .unwrap();
        assert_eq!((first.id.as_str(), again.name.as_str()), (ROUTED, "a"));
        assert_eq!(store.snapshot().unwrap().len(), 1);
        for (name, max) in [("b", Some(5)), ("a", None)] {
            let Err(error) = store.create_with_id(None, ROUTED, name, max, "op") else {
                panic!("a different tenant under the same ID was accepted");
            };
            assert_eq!(error.code, "tenant_id_conflict");
        }
        assert!(
            store
                .create_with_id(None, &"0".repeat(32), "", None, "op")
                .is_err()
        );
    }

    #[test]
    fn cli_core_submits_reads_and_acts_with_the_same_contracts() {
        let directory = tempfile::tempdir().unwrap();
        let store = tenants::Store::load(directory.path()).unwrap();
        let credentials = credentials::Store::load(directory.path()).unwrap();
        let memory = Arc::new(Memory::default());
        Objects::new(memory.clone(), CONTROL)
            .create(Snapshot::import(&credentials, Some(&store)).unwrap(), NOW)
            .unwrap();
        let deployments = Deployments::new(memory, CONTROL, PREFIX);
        let log = audit::Log::open(directory.path()).unwrap();
        let submit = Command::Submit {
            kind: Kind::Create,
            tenant: None,
            request: create_request("cli-key", "contoso")
                .to_string()
                .into_bytes(),
        };
        let created = run(&deployments, &store, Some(&log), submit, NOW).unwrap();
        let operation = created["operation"].clone();
        assert_eq!(operation["phase"], "reserved");
        let id = operation["operation_id"].as_str().unwrap().to_owned();
        let status = run(
            &deployments,
            &store,
            None,
            Command::Status {
                operation: id.clone(),
            },
            NOW,
        )
        .unwrap();
        assert_eq!(status["operation"], operation);
        let action = |verb: &str, key: &str| Command::Act {
            operation: id.clone(),
            verb: verb.into(),
            request: json!({
                "idempotency_key": key,
                "expected_operation_generation": operation["operation_generation"],
                "expected_lifecycle_generation": operation["lifecycle_generation"],
            })
            .to_string()
            .into_bytes(),
        };
        let error = run(&deployments, &store, None, action("release", "r"), NOW).unwrap_err();
        assert_eq!(error.code, "irreversible_ack_required");
        let cancelled = run(&deployments, &store, Some(&log), action("cancel", "c"), NOW).unwrap();
        assert_eq!(cancelled["operation"]["phase"], "cancelled");
        let error = run(
            &deployments,
            &store,
            None,
            Command::Act {
                operation: id,
                verb: "cancel".into(),
                request: b"{\"idempotency_key\":\"c\",\"token\":\"x\"}".to_vec(),
            },
            NOW,
        )
        .unwrap_err();
        assert_eq!(error.code, "operation_request_invalid");
    }

    #[test]
    fn cli_refuses_literal_arguments_and_unknown_verbs() {
        let args = |values: &[&str]| values.iter().map(OsString::from).collect::<Vec<_>>();
        let error = tenant_deployment_command(&args(&["explode"])).unwrap_err();
        assert!(error.starts_with("invalid_arguments"), "{error}");
        let error = tenant_deployment_command(&args(&[
            "cancel",
            "--operation",
            "0",
            "--idempotency-key",
            "literal-secret",
        ]))
        .unwrap_err();
        assert!(error.starts_with("invalid_arguments"), "{error}");
        assert!(!error.contains("literal-secret"));
        let error =
            tenant_deployment_command(&args(&["status", "--request-file", "x"])).unwrap_err();
        assert!(error.starts_with("invalid_arguments"), "{error}");
    }

    /// A `--tenant-deployments-file` naming a loopback emulator target
    /// whose port is closed: nothing below contacts it.
    fn runtime_files(root: &std::path::Path) -> (std::path::PathBuf, Value) {
        let target = root.join("target.json");
        std::fs::write(
            &target,
            json!({"account":"devstoreaccount1","container":"data","coordination_container":"coord",
                "prefix":"p/","endpoint":"http://127.0.0.1:9/devstoreaccount1",
                "auth":"emulator_shared_key"})
            .to_string(),
        )
        .unwrap();
        let key = root.join("key");
        std::fs::write(&key, "c2VjcmV0LWtleS1ub3QtZWNob2Vk").unwrap();
        let config = root.join("tenant-deployments.json");
        let value = json!({"target_file": target, "object": "control/state.json",
            "shared_key_file": key});
        std::fs::write(&config, value.to_string()).unwrap();
        (config, value)
    }

    #[test]
    fn runtime_configuration_is_bounded_strict_and_validated() {
        let directory = tempfile::tempdir().unwrap();
        let (config, base) = runtime_files(directory.path());
        assert!(TenantDeploymentsConfig::read(&config).is_ok());
        for change in [
            json!({"unknown": true}),
            json!({"object": "../escape"}),
            json!({"object": "owner.json"}),
        ] {
            let mut value = base.clone();
            for (name, field) in change.as_object().unwrap() {
                value[name] = field.clone();
            }
            std::fs::write(&config, value.to_string()).unwrap();
            let error = TenantDeploymentsConfig::read(&config).err().unwrap();
            assert!(error.starts_with("tenant_deployments_invalid"), "{error}");
        }
        std::fs::write(&config, vec![b' '; 70 * 1024]).unwrap();
        let error = TenantDeploymentsConfig::read(&config).err().unwrap();
        assert!(error.starts_with("tenant_deployments_invalid"), "{error}");
        let error = TenantDeploymentsConfig::read(&directory.path().join("missing"))
            .err()
            .unwrap();
        assert!(error.starts_with("tenant_deployments_invalid"), "{error}");
    }

    #[test]
    fn an_opened_runtime_holds_the_admin_directory_against_the_cli() {
        let directory = tempfile::tempdir().unwrap();
        let (config, value) = runtime_files(directory.path());
        let admin = directory.path().join("admin");
        let opened = TenantDeploymentsConfig::read(&config)
            .unwrap()
            .open(&admin)
            .unwrap();
        // The CLI would otherwise change tenants.json under the server.
        let args: Vec<OsString> = [
            "status",
            "--operation",
            "0",
            "--target-file",
            value["target_file"].as_str().unwrap(),
            "--object",
            "control/state.json",
            "--admin-dir",
            admin.to_str().unwrap(),
        ]
        .iter()
        .map(OsString::from)
        .collect();
        let error = tenant_deployment_command(&args).unwrap_err();
        assert!(error.starts_with("admin_dir_busy"), "{error}");
        let error = TenantDeploymentsConfig::read(&config)
            .unwrap()
            .open(&admin)
            .err()
            .unwrap();
        assert!(error.starts_with("admin_dir_busy"), "{error}");
        drop(opened);
        assert!(
            TenantDeploymentsConfig::read(&config)
                .unwrap()
                .open(&admin)
                .is_ok()
        );
    }

    #[test]
    fn an_opened_runtime_serves_the_routes_with_admin_authentication() {
        let directory = tempfile::tempdir().unwrap();
        let (config, _) = runtime_files(directory.path());
        let opened = TenantDeploymentsConfig::read(&config)
            .unwrap()
            .open(&directory.path().join("admin"))
            .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let unconfigured = fixture_with(false, Some(TOKEN));
        let path = "/v1/admin/tenant-deployment-operations/not-an-operation";
        let (status, body) = runtime.block_on(unconfigured.call("GET", path, None));
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["code"], "tenant_deployments_not_configured");
        let mut served = fixture_with(false, Some(TOKEN));
        served.app = crate::server::configured_routes(
            served
                .service
                .clone()
                .with_managed_tenant_deployments(&opened),
        );
        // Refused before the store is contacted.
        let (status, body) = runtime.block_on(served.call("GET", path, None));
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["error"]["code"], "operation_not_found");
        let open = fixture_with(false, None);
        let open_app = crate::server::configured_routes(
            open.service
                .clone()
                .with_managed_tenant_deployments(&opened),
        );
        let served = Fixture {
            app: open_app,
            ..open
        };
        let (status, body) = runtime.block_on(served.call_as(None, "GET", path, None));
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "admin_auth_required");
        drop(served);
        drop(runtime);
    }
}
