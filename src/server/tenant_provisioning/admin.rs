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
use crate::server::{audit, cloud_recovery, credentials, tenants, users};
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
    /// DD-002: the explicit grant of the candidate credential that
    /// `issue-credentials` or `replace-credentials` mints; never defaulted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    grant: Option<GrantInput>,
    /// DD-002: the delivery generation and credential ID being acknowledged
    /// or replaced, exactly as a status read or the delivery reported them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    delivery_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credential_id: Option<String>,
}

/// The grant of a create's candidate credential, checked by the rules of
/// the credential API before anything is persisted. Always bound to the
/// operation's tenant and seeded lineage; the request cannot name either.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantInput {
    name: String,
    operations: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    graphs: Option<Vec<String>>,
    expires_unix_ms: u64,
}

impl GrantInput {
    fn grant(&self) -> CandidateGrant {
        CandidateGrant {
            name: self.name.clone(),
            operations: self.operations.clone(),
            graphs: self.graphs.clone(),
            expires_unix_ms: self.expires_unix_ms,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verb {
    Resume,
    Cancel,
    IssueCredentials,
    AcknowledgeDelivery,
    ReplaceCredentials,
    Rollback,
    Release,
}

impl Verb {
    const ALL: [Self; 7] = [
        Self::Resume,
        Self::Cancel,
        Self::IssueCredentials,
        Self::AcknowledgeDelivery,
        Self::ReplaceCredentials,
        Self::Rollback,
        Self::Release,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Resume => "resume",
            Self::Cancel => "cancel",
            Self::IssueCredentials => "issue-credentials",
            Self::AcknowledgeDelivery => "acknowledge-delivery",
            Self::ReplaceCredentials => "replace-credentials",
            Self::Rollback => "rollback",
            Self::Release => "release",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|verb| verb.name() == name)
    }

    /// DD-002: a verb whose response carries a newly minted token.
    fn mints(self) -> bool {
        matches!(self, Self::IssueCredentials | Self::ReplaceCredentials)
    }
}

/// DD-002: refuses delivery fields a verb does not take, or lacks, before
/// anything is read or persisted.
fn delivery_fields(verb: Verb, request: &ActionRequest, now: u64) -> Result<(), ApiError> {
    let quoted = request.delivery_generation.is_some() || request.credential_id.is_some();
    let complete = request.delivery_generation.is_some_and(|g| g > 0)
        && request.credential_id.as_deref().is_some_and(text);
    let (grant, quote) = match verb {
        Verb::IssueCredentials => (true, false),
        Verb::AcknowledgeDelivery => (false, true),
        Verb::ReplaceCredentials => (true, true),
        _ => (false, false),
    };
    if request.grant.is_some() != grant || (quote && !complete) || (!quote && quoted) {
        return Err(invalid(
            "issue-credentials takes a grant; acknowledge-delivery takes delivery_generation and credential_id; replace-credentials takes all three; no other action takes any",
        ));
    }
    if let Some(input) = &request.grant {
        credentials::candidate_grant(
            &input.name,
            &input.operations,
            input.graphs.as_deref(),
            input.expires_unix_ms,
            now,
        )?;
    }
    Ok(())
}

/// The delivery an action reports: IDs and generations, never a secret.
fn delivery_view(delivery: &Delivery) -> Value {
    json!({
        "generation": delivery.generation,
        "credential_id": delivery.credential_id,
        "acknowledged": delivery.acknowledged,
        "revoked": delivery.revoked,
    })
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

pub(super) fn can_finish_cancel(outcome: Option<&TerminalOutcome>, slot: Option<&Slot>) -> bool {
    matches!(
        (outcome, slot),
        (
            Some(TerminalOutcome::Cancelled),
            Some(Slot::Reserved {
                cancellation_requested: true,
                receipt: None,
                ..
            })
        )
    )
}

/// The redacted status view: never the idempotency key or its digest,
/// retained content, tokens or secret version names.
fn render(
    intent: &Intent,
    snapshot: &Snapshot,
    journal: &Journal,
    terminal: Option<&Terminal>,
    delivery: Option<&Delivery>,
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
    let withdrawn = snapshot.withdrawn(&intent.tenant_id, &intent.operation_id);
    let outcome = terminal.map(|t| &t.outcome);
    let release_pending = can_finish_cancel(outcome, slot);
    let (phase, next_step) = match outcome {
        Some(TerminalOutcome::Succeeded { .. }) => ("succeeded", "none: the operation ended"),
        Some(TerminalOutcome::Cancelled) if release_pending => {
            ("cancelled", "cancel again to release the reservation")
        }
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
        None if withdrawn => (
            "cancelling",
            "cancel again to seal the withdrawal; nothing is reserved",
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
            "cancellation_requested": cancelling || withdrawn,
            "release_pending": release_pending,
            "journal_steps": journal.steps.len(),
            "uncertain_steps": uncertain,
            "terminal": terminal.is_some(),
            // DD-002: IDs only; a lost token is recovered by replacement.
            "credential_delivery": delivery.map(|delivery| json!({
                "generation": delivery.generation,
                "credential_id": delivery.credential_id,
                "acknowledged": delivery.acknowledged,
            })),
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
    let delivery = deployments.ops.delivery(&intent.operation_id)?;
    let (snapshot, _) = deployments.objects.read()?;
    Ok(render(
        intent,
        &snapshot,
        &journal,
        terminal.as_ref(),
        delivery.as_ref(),
    ))
}

fn status(deployments: &Deployments, operation_id: &str) -> Result<Value, ApiError> {
    view(deployments, &load(deployments, operation_id)?)
}

/// ITEM-023: one page of [`status`] views, ascending by operation ID after
/// the exclusive `after`. Reads only the page's records, then the control
/// state once; `next_after` is set iff more operations follow the page.
fn list(deployments: &Deployments, after: Option<&str>, limit: usize) -> Result<Value, ApiError> {
    let mut ids = deployments.ops.ids(after, limit + 1)?;
    let more = ids.len() > limit;
    ids.truncate(limit);
    let mut records = Vec::with_capacity(ids.len());
    for id in &ids {
        let intent = load(deployments, id)?;
        let journal = deployments.ops.journal(id)?;
        let terminal = deployments.ops.terminal(id)?;
        let delivery = deployments.ops.delivery(id)?;
        records.push((intent, journal, terminal, delivery));
    }
    let (snapshot, _) = deployments.objects.read()?;
    let operations: Vec<Value> = records
        .iter()
        .map(|(intent, journal, terminal, delivery)| {
            render(
                intent,
                &snapshot,
                journal,
                terminal.as_ref(),
                delivery.as_ref(),
            )
        })
        .collect();
    let next_after = if more { ids.last() } else { None };
    Ok(json!({"operations": operations, "next_after": next_after}))
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
/// `terminal` is the sealed outcome, if any; `slot` is this operation's
/// reservation, if it holds one.
fn available(
    verb: Verb,
    request: &ActionRequest,
    payload: &Payload,
    terminal: Option<&TerminalOutcome>,
    slot: Option<&Slot>,
) -> Result<(), ApiError> {
    let ended = terminal.is_some();
    match verb {
        Verb::Resume if !ended && pins_sets(payload) => Err(pin_scope_unproven()),
        Verb::Resume => Ok(()),
        // A sealed cancellation whose release was lost still holds the slot:
        // a fresh cancel quoting the current tokens completes that release.
        Verb::Cancel if ended && can_finish_cancel(terminal, slot) => Ok(()),
        Verb::Cancel if ended => Err(super::ended()),
        Verb::Cancel => match slot {
            Some(Slot::Reserved { receipt: None, .. }) => Ok(()),
            Some(_) => Err(conflict(
                "retirement_required",
                "the operation already activated its route; retire the tenant instead",
            )),
            // EPIC-005: withdrawn in the same CAS as its reservation.
            None if matches!(payload, Payload::SetReaders { .. } | Payload::Retire { .. }) => {
                Ok(())
            }
            None if matches!(payload, Payload::Create { .. }) => Err(unavailable_transition(
                "cancel needs this operation to hold the tenant reservation; resubmit the request to reserve it",
            )),
            None => Err(unavailable_transition(
                "cancel needs this operation to hold the tenant reservation; only its plan-anchored actuator reserves it",
            )),
        },
        Verb::IssueCredentials | Verb::AcknowledgeDelivery | Verb::ReplaceCredentials
            if !matches!(payload, Payload::Create { .. }) =>
        {
            Err(unavailable_transition(
                "credential delivery for a migration needs migration orchestration (EPIC-006)",
            ))
        }
        Verb::ReplaceCredentials if !request.ack_irreversible => Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "irreversible_ack_required",
            "replacement irreversibly revokes the unacknowledged credential and needs ack_irreversible: true",
        )),
        Verb::IssueCredentials | Verb::AcknowledgeDelivery | Verb::ReplaceCredentials if ended => {
            Err(super::ended())
        }
        Verb::IssueCredentials | Verb::AcknowledgeDelivery | Verb::ReplaceCredentials => {
            match slot {
                Some(Slot::Reserved {
                    cancellation_requested: false,
                    receipt: None,
                    ..
                }) => Ok(()),
                _ => Err(conflict(
                    "operation_not_provisionable",
                    "credential delivery needs this create to hold an uncancelled, unactivated reservation",
                )),
            }
        }
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

/// DD-002: refuses a delivery action the current delivery state settles,
/// before anything is persisted and again before it is applied. An issue
/// needs a recorded seed and no candidate; a replacement or an
/// acknowledgement must quote the current generation and credential ID
/// (a replacement only while it is unacknowledged), and an acknowledgement
/// only a candidate the published view holds live for this tenant and its
/// seeded lineage, so an unpublished one is replaced, never acknowledged.
fn delivery_ready(
    deployments: &Deployments,
    intent: &Intent,
    verb: Verb,
    request: &ActionRequest,
    now: u64,
) -> Result<(), ApiError> {
    if !matches!(
        verb,
        Verb::IssueCredentials | Verb::AcknowledgeDelivery | Verb::ReplaceCredentials
    ) {
        return Ok(());
    }
    let id = &intent.operation_id;
    let seed = recorded_seed(&deployments.ops.journal(id)?).ok_or_else(seed_unrecorded)?;
    let current = deployments.ops.delivery(id)?;
    if verb == Verb::IssueCredentials {
        return match current {
            Some(_) => Err(candidate_recorded()),
            None => Ok(()),
        };
    }
    let Some(delivery) = current.filter(|delivery| {
        Some(delivery.generation) == request.delivery_generation
            && Some(delivery.credential_id.as_str()) == request.credential_id.as_deref()
    }) else {
        return Err(delivery_stale());
    };
    if verb == Verb::ReplaceCredentials {
        return if delivery.acknowledged {
            Err(delivery_stale())
        } else {
            Ok(())
        };
    }
    let (snapshot, _) = deployments.objects.read()?;
    let published = snapshot.records_publication(
        &intent.tenant_id,
        id,
        delivery.generation,
        &delivery.credential_id,
        &delivery.revoked,
    ) && snapshot.live_credential(&delivery.credential_id, now)
        == Some((Some(intent.tenant_id.clone()), seed.lineage));
    if !published {
        return Err(conflict(
            "credential_view_unpublished",
            "the candidate is not published live for this tenant; replace it instead",
        ));
    }
    Ok(())
}

/// One generation-bound action (ITEM-004). `private` is set only by the
/// authenticated REST response (DD-002): only there may an action mint a
/// token, which is returned beside the stored result, never inside it.
fn act(
    deployments: &Deployments,
    actor: &str,
    operation_id: &str,
    verb: &str,
    request: ActionRequest,
    private: bool,
    now: u64,
) -> Result<(Value, Option<String>), ApiError> {
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
    delivery_fields(verb, &request, now)?;
    if verb.mints() && !private {
        return Err(unavailable_transition(
            "a candidate credential is delivered only in the authenticated admin HTTPS response",
        ));
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
        )) => return Ok((result, None)),
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
                terminal.as_ref().map(|t| &t.outcome),
                holds(&snapshot, &intent),
            )?;
            delivery_ready(deployments, &intent, verb, &request, now)?;
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
                ) => return Ok((result, None)),
                (_, etag) => etag,
            }
        }
    };
    let mut minted = None;
    let mut delivery = None;
    match verb {
        Verb::Resume => {}
        Verb::Cancel => {
            match deployments.ops.terminal(operation_id)? {
                Some(Terminal {
                    outcome: TerminalOutcome::Succeeded { .. },
                    ..
                }) => return Err(super::ended()),
                Some(_) => {}
                None => request_cancel(&deployments.ops, &deployments.objects, &intent, now)?,
            }
            // Idempotent: completes a release an earlier attempt sealed.
            deployments
                .ops
                .release(&deployments.objects, &intent, now)?;
        }
        // A re-applied mint is refused by the delivery record, so a lost
        // response is never answered with another token: the administrator
        // replaces it instead.
        Verb::IssueCredentials | Verb::ReplaceCredentials | Verb::AcknowledgeDelivery => {
            delivery_ready(deployments, &intent, verb, &request, now)?;
            let (ops, objects) = (&deployments.ops, &deployments.objects);
            let grant = request.grant.as_ref().map(GrantInput::grant);
            match (verb, grant, request.delivery_generation, &request.credential_id) {
                (Verb::IssueCredentials, Some(grant), _, _) => {
                    minted = Some(publish_candidate(ops, objects, &intent, &grant, now)?);
                }
                (Verb::ReplaceCredentials, Some(grant), Some(generation), _) => {
                    minted = Some(replace_published_candidate(
                        ops, objects, &intent, generation, &grant, now,
                    )?);
                }
                (Verb::AcknowledgeDelivery, None, Some(generation), Some(credential_id)) => {
                    delivery =
                        Some(ops.acknowledge_delivery(operation_id, generation, credential_id)?);
                }
                // `delivery_fields` refused every other shape.
                _ => return Err(state_invalid()),
            }
        }
        // Refused by `available` before any record was written.
        _ => return Err(state_invalid()),
    }
    let mut result = json!({"action": verb.name(), "operation": view(deployments, &intent)?});
    // The delivery an action reports is the one it produced, never a later
    // re-read: a minted token is released only beside the exact candidate it
    // was recorded as. The operation view above is read first; `deliver`
    // then re-reads the delivery record and, last, the control state, so the
    // view, the delivery and the token describe one uncancelled publication
    // of this candidate. A concurrent replacement or cancellation before
    // that last read drops the token (the action stays unfinished; a replay
    // is refused, never re-minted); one after it revokes the returned token.
    let mut token = None;
    if let Some(minted) = minted {
        let (secret, current) =
            minted.deliver(&deployments.ops, &deployments.objects, &intent, now)?;
        token = Some(secret);
        delivery = Some(current);
    }
    if let Some(delivery) = &delivery {
        result["delivery"] = delivery_view(delivery);
    }
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
    Ok((result, token))
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

/// ITEM-023: the query of `GET /v1/admin/tenant-deployment-operations`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ListQuery {
    after: Option<String>,
    limit: Option<String>,
}

const DEFAULT_LIST_LIMIT: usize = 50;
const MAX_LIST_LIMIT: usize = 100;

fn invalid_query(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "invalid_query", message)
}

/// ITEM-023: pages of the redacted status of the recorded operations; it is
/// read-only, so it records no audit event, like [`operation_status`].
pub(super) async fn operation_list(
    State(service): State<Service>,
    role: Option<Extension<Role>>,
    principal: Option<Extension<users::Principal>>,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    rest_actor(&service, role, principal)?;
    let deployments = configured(&service)?;
    let Query(query) = query.map_err(|_| invalid_query("invalid query parameters"))?;
    let limit = match query.limit.as_deref() {
        None => DEFAULT_LIST_LIMIT,
        // Canonical decimal only: no sign, space or leading zero.
        Some(text) => text
            .parse()
            .ok()
            .filter(|limit| (1..=MAX_LIST_LIMIT).contains(limit) && limit.to_string() == text)
            .ok_or_else(|| {
                invalid_query(format!(
                    "limit must be a decimal integer from 1 to {MAX_LIST_LIMIT}"
                ))
            })?,
    };
    if query
        .after
        .as_deref()
        .is_some_and(|after| !is_hex(after, 32))
    {
        return Err(invalid_query("after must be an operation ID"));
    }
    let page = blocking(move || list(&deployments, query.after.as_deref(), limit)).await?;
    Ok(Json(page))
}

pub(super) async fn operation_action(
    State(service): State<Service>,
    Path((operation, action)): Path<(String, String)>,
    role: Option<Extension<Role>>,
    principal: Option<Extension<users::Principal>>,
    headers: axum::http::HeaderMap,
    input: Result<Json<ActionRequest>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let actor = rest_actor(&service, role, principal)?;
    // DD-002: a minted secret travels only over HTTPS, as the account
    // layer proves it: native TLS, or `VLITE_TRUSTED_PROXY=1` with the
    // proxy's own `X-Forwarded-Proto: https`. Refused before anything is
    // minted or persisted; no accounts, no proof.
    let secure = service
        .admin
        .users
        .as_ref()
        .is_some_and(|accounts| accounts.secure_request(&headers));
    if !secure && Verb::parse(&action).is_some_and(Verb::mints) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "insecure_transport",
            "candidate credentials are delivered only over HTTPS: native TLS, or VLITE_TRUSTED_PROXY=1 with X-Forwarded-Proto: https",
        ));
    }
    let input = body(input)?;
    let deployments = configured(&service)?;
    let (mut result, token) = {
        let (actor, operation, action) = (actor.clone(), operation.clone(), action.clone());
        blocking(move || {
            act(
                &deployments,
                &actor,
                &operation,
                &action,
                input,
                secure,
                now(),
            )
        })
        .await?
    };
    record(&service, action_event(&actor, &operation, &action));
    // DD-002: the only copy of a minted secret, in this no-store response.
    if let Some(token) = token {
        result["token"] = Value::String(token);
    }
    Ok(Json(result))
}

// -------------------------------------------------------------------- CLI

const USAGE: &str = "vlite-server tenant-deployment <create|readers|migrate|retire> --request-file <file> [--tenant <id>] | status --operation <id> | work [--operation <id>] | <resume|cancel|acknowledge-delivery|replace-credentials|rollback|release> --operation <id> --request-file <file> (issue-credentials and replace-credentials only over the admin HTTPS API); every verb also takes --target-file <file> --object <name> --admin-dir <dir> [--shared-key-file <file>]";

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
    /// ITEM-022: one control-worker attempt ([`super::worker`]).
    Work {
        operation: Option<String>,
        attempt: String,
        /// The managed identity for ARM tokens: the target's client ID.
        client_id: Option<String>,
    },
}

const CLI_ACTOR: &str = "cli:tenant-deployment";

/// ITEM-022: one control-worker invocation ([`super::worker::work`]) with
/// its outcome audited. ITEM-024: it first releases every sealed
/// cancellation whose release failed ([`super::worker::release_cancelled`]),
/// auditing each and listing it in `releases_recovered`; if that sweep
/// fails, no operation is attempted. Idle when nothing can be claimed; an
/// attempt that did not provision is an error whose message names the
/// operation, the attempt and the outcome (`blocked`, `failed` or
/// `unknown`).
pub(super) fn work(
    deployments: &Deployments,
    log: Option<&audit::Log>,
    proofs: &super::worker::Proofs,
    adapter: &super::worker::Adapter<'_>,
    operation: Option<&str>,
    attempt: &str,
    now: u64,
) -> Result<Value, ApiError> {
    // A malformed ID is refused before any store request, sweep included.
    if operation.is_some_and(|id| !is_hex(id, 32)) {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "operation_not_found",
            "no such operation",
        ));
    }
    let mut recovered = Vec::new();
    let swept =
        super::worker::release_cancelled(&deployments.objects, &deployments.ops, now, |intent| {
            let recorded = log.is_some()
                && audit_event(
                    log,
                    json!({
                        "event": "tenant_deployment_release_recovered",
                        "actor": CLI_ACTOR,
                        "operation_id": intent.operation_id,
                        "tenant_id": intent.tenant_id,
                        "attempt": attempt,
                    }),
                );
            recovered
                .push(json!({"operation_id": intent.operation_id, "audit_recorded": recorded}));
        });
    let first = |recovered: &[Value]| match recovered.is_empty() {
        true => String::new(),
        false => format!("; releases recovered first: {}", json!(recovered)),
    };
    if let Err(mut error) = swept {
        error.message = format!(
            "{}{}; no operation was attempted",
            error.message,
            first(&recovered)
        );
        return Err(error);
    }
    let Some(done) = super::worker::work(
        &deployments.objects,
        &deployments.ops,
        proofs,
        adapter,
        operation,
        attempt,
    )
    .map_err(|mut error| {
        error.message = format!("{}{}", error.message, first(&recovered));
        error
    })?
    else {
        return Ok(json!({"outcome": "idle", "releases_recovered": recovered}));
    };
    let (id, outcome) = (&done.intent.operation_id, done.outcome.as_str());
    let mut event = json!({
        "event": "tenant_deployment_attempt",
        "actor": CLI_ACTOR,
        "operation_id": id,
        "attempt": attempt,
        "outcome": outcome,
    });
    if let Some(error) = &done.error {
        event["code"] = json!(error.code);
    }
    let recorded = audit_event(log, event);
    match done.error {
        None => Ok(json!({
            "outcome": outcome,
            "operation_id": id,
            "attempt": attempt,
            "audit_recorded": recorded,
            // A failed readback must not mask the recorded outcome.
            "operation": view(deployments, &done.intent).ok(),
            "releases_recovered": recovered,
        })),
        Some(mut error) => {
            let unrecorded = if recorded {
                ""
            } else {
                "; the audit event was not recorded"
            };
            error.message = format!(
                "{}; operation {id}, attempt {attempt}, outcome {outcome}{unrecorded}{}",
                error.message,
                first(&recovered)
            );
            Err(error)
        }
    }
}

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
            let (result, _) = act(
                deployments,
                CLI_ACTOR,
                &operation,
                &verb,
                parse(&request)?,
                false,
                now,
            )?;
            audit_event(log, action_event(CLI_ACTOR, &operation, &verb));
            Ok(result)
        }
        Command::Work {
            operation,
            attempt,
            client_id,
        } => work(
            deployments,
            log,
            // No production proof exists yet (OD-006), so preflight refuses
            // before the adapter is built or anything is claimed.
            &super::worker::Proofs::default(),
            &super::worker::production_arm(client_id),
            operation.as_deref(),
            &attempt,
            now,
        ),
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
        "status" | "work" => None,
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
        None if verb == "status" || verb == "work" => allowed.push("--operation"),
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
        None if verb == "work" => Command::Work {
            operation: values
                .remove("--operation")
                .map(|value| utf8("--operation", value))
                .transpose()?,
            attempt: super::worker::attempt_id()
                .map_err(|error| format!("{}: {}", error.code, error.message))?,
            client_id: crate::server::cloud_recovery::target_client_id(&source.target_file)
                .map_err(|error| error.to_string())?,
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
    use crate::server::cloud_backup::fake::{Fault, Memory, Op};
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
        /// Headers every request carries (e.g. `X-Forwarded-Proto`).
        headers: Vec<(&'static str, &'static str)>,
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
            for (name, value) in &self.headers {
                request = request.header(*name, *value);
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

    /// An authenticated server whose control state routes `ROUTED` shared,
    /// serving native HTTPS.
    fn fixture_with(configured: bool, token: Option<&str>) -> Fixture {
        fixture_over(configured, token, true, false)
    }

    /// [`fixture_with`] whose REST listener terminates TLS (`https`) or
    /// trusts a reverse proxy's `X-Forwarded-Proto` (`trusted_proxy`), as
    /// `ServerOptions` configures the dashboard accounts.
    fn fixture_over(
        configured: bool,
        token: Option<&str>,
        https: bool,
        trusted_proxy: bool,
    ) -> Fixture {
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
        admin.users = Some(Arc::new(
            users::Accounts::load(
                directory.path(),
                &users::Settings {
                    idle: users::DEFAULT_IDLE,
                    lifetime: users::DEFAULT_LIFETIME,
                    https,
                    loopback_only: true,
                    trusted_proxy,
                },
            )
            .unwrap(),
        ));
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
            headers: Vec::new(),
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

    /// A cancel sealed whose release was never published leaves the slot
    /// reserved. Any admin's fresh cancel quoting the current tokens
    /// completes that release; once released it stays ended.
    #[tokio::test]
    async fn a_fresh_cancel_completes_a_sealed_cancellation_whose_release_failed() {
        let f = fixture();
        let operation = f.create("onboard-contoso").await;
        let id = operation["operation_id"].as_str().unwrap().to_owned();
        let tenant = operation["tenant_id"].as_str().unwrap().to_owned();
        assert_eq!(operation["observed"]["release_pending"], false);
        // The first control-state write requests the cancellation; the
        // second, the release after the seal, is refused.
        f.memory.fault(Op::Put, CONTROL, Fault::Refuse, 1, 1);
        let (status, body) = f
            .action(&operation, "cancel", "cancel-lost", json!({}))
            .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert!(matches!(
            f.snapshot().lifecycle(&tenant).map(|entry| &entry.slot),
            Some(Slot::Reserved {
                cancellation_requested: true,
                receipt: None,
                ..
            })
        ));
        let path = format!("/v1/admin/tenant-deployment-operations/{id}");
        let (_, current) = f.call("GET", &path, None).await;
        let current = current["operation"].clone();
        assert_eq!(current["phase"], "cancelled");
        assert_eq!(current["observed"]["terminal"], true);
        assert_eq!(current["observed"]["reserved"], true);
        assert_eq!(current["observed"]["release_pending"], true);
        assert_eq!(
            current["next_step"],
            "cancel again to release the reservation"
        );
        let (status, body) = f
            .action(&operation, "cancel", "recover-stale", json!({}))
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["code"], "operation_generation_stale");

        let (status, recovered) = f.action(&current, "cancel", "recover-1", json!({})).await;
        assert_eq!(status, StatusCode::OK, "{recovered}");
        assert_eq!(recovered["operation"]["phase"], "cancelled");
        assert_eq!(recovered["operation"]["observed"]["reserved"], false);
        assert_eq!(recovered["operation"]["observed"]["release_pending"], false);
        assert_eq!(
            recovered["operation"]["next_step"],
            "none: the operation ended"
        );
        assert!(matches!(
            f.snapshot().lifecycle(&tenant).map(|entry| &entry.slot),
            Some(Slot::Released { operation_id, .. }) if *operation_id == id
        ));
        let audit = std::fs::read_to_string(f.directory.path().join("audit.jsonl")).unwrap();
        assert!(
            audit
                .lines()
                .any(|line| line.contains("tenant_deployment_action")
                    && line.contains(&id)
                    && line.contains("\"action\":\"cancel\"")),
            "{audit}"
        );
        // The lost attempt's exact resend reaches the same end.
        let (status, replay) = f
            .action(&operation, "cancel", "cancel-lost", json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{replay}");
        assert_eq!(replay["operation"]["observed"]["reserved"], false);
        // Released: a further fresh cancel is ended, even with current tokens.
        let (status, body) = f
            .action(&recovered["operation"], "cancel", "recover-2", json!({}))
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["code"], "operation_terminal");
    }

    /// An ended operation takes a cancel only to release its own slot
    /// after a sealed cancellation: never a released, replaced or
    /// withdrawn slot (none is held), a retirement or an activation.
    #[test]
    fn an_ended_cancel_only_releases_its_own_sealed_cancellation() {
        let request: ActionRequest = serde_json::from_value(json!({
            "idempotency_key": "k",
            "expected_operation_generation": 0,
            "expected_lifecycle_generation": 0,
        }))
        .unwrap();
        let payload = Payload::Retire {
            tenant_id: ROUTED.into(),
            expected_receipt: "a".repeat(64),
            reason: "test".into(),
            data_disposition: DataDisposition::Retain,
        };
        let reserved = |cancellation_requested| Slot::Reserved {
            operation_id: "a".repeat(32),
            kind: Kind::Retire,
            request_digest: "b".repeat(64),
            expected_route: None,
            cancellation_requested,
            receipt: None,
        };
        let cancelled = Some(&TerminalOutcome::Cancelled);
        available(
            Verb::Cancel,
            &request,
            &payload,
            cancelled,
            Some(&reserved(true)),
        )
        .unwrap();
        for (terminal, slot) in [
            (cancelled, None),
            (cancelled, Some(reserved(false))),
            (Some(&TerminalOutcome::Retired), Some(reserved(false))),
            (Some(&TerminalOutcome::Retired), Some(reserved(true))),
        ] {
            let error =
                available(Verb::Cancel, &request, &payload, terminal, slot.as_ref()).unwrap_err();
            assert_eq!(error.code, "operation_terminal");
        }
    }

    /// ITEM-024: the release-recovery contract the dashboard test and the
    /// browser fake also read; this pins it to the server's own rule.
    #[test]
    fn release_recovery_contract_cases_match_the_server_rule() {
        let cases: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/ha/tenant-deployment-release-cases.json"
        ))
        .unwrap();
        let request: ActionRequest = serde_json::from_value(json!({
            "idempotency_key": "k",
            "expected_operation_generation": 0,
            "expected_lifecycle_generation": 0,
        }))
        .unwrap();
        let payload = Payload::Retire {
            tenant_id: ROUTED.into(),
            expected_receipt: "a".repeat(64),
            reason: "test".into(),
            data_disposition: DataDisposition::Retain,
        };
        let cases = cases["cases"].as_array().unwrap();
        assert!(cases.len() >= 8);
        for case in cases {
            let name = &case["name"];
            let terminal = match case["terminal"].as_str() {
                None => None,
                Some("cancelled") => Some(TerminalOutcome::Cancelled),
                Some("retired") => Some(TerminalOutcome::Retired),
                Some(other) => panic!("{name}: unknown terminal {other}"),
            };
            let slot = case["slot"].as_object().map(|slot| Slot::Reserved {
                operation_id: "a".repeat(32),
                kind: Kind::Retire,
                request_digest: "b".repeat(64),
                expected_route: None,
                cancellation_requested: slot["cancellation_requested"].as_bool().unwrap(),
                receipt: slot["receipt"].as_bool().unwrap().then(|| {
                    crate::server::control_state::Receipt {
                        operation_id: "a".repeat(32),
                        request_digest: "b".repeat(64),
                        generation: 2,
                        route_digest: "c".repeat(64),
                        lineage: 1,
                        epoch: 1,
                        proof_digest: "d".repeat(64),
                    }
                }),
            });
            assert_eq!(
                can_finish_cancel(terminal.as_ref(), slot.as_ref()),
                case["release_pending"].as_bool().unwrap(),
                "{name}"
            );
            let cancel = available(
                Verb::Cancel,
                &request,
                &payload,
                terminal.as_ref(),
                slot.as_ref(),
            )
            .map_or_else(|error| error.code.to_string(), |()| "ok".to_owned());
            assert_eq!(cancel, case["cancel"].as_str().unwrap(), "{name}");
        }
    }

    /// EPIC-005: an unreserved reader change or retirement is cancelled by
    /// withdrawing it (`request_cancel`); a create or a migration still
    /// needs to hold its reservation.
    #[test]
    fn cancel_without_a_reservation_withdraws_only_readers_and_retirements() {
        let request: ActionRequest = serde_json::from_value(json!({
            "idempotency_key": "k",
            "expected_operation_generation": 0,
            "expected_lifecycle_generation": 0,
        }))
        .unwrap();
        let withdrawable = [
            Payload::SetReaders {
                tenant_id: ROUTED.into(),
                fleet: Fleet::Absent {},
            },
            Payload::Retire {
                tenant_id: ROUTED.into(),
                expected_receipt: "a".repeat(64),
                reason: "test".into(),
                data_disposition: DataDisposition::Retain,
            },
        ];
        for payload in &withdrawable {
            available(Verb::Cancel, &request, payload, None, None).unwrap();
        }
        let held_first = [
            Payload::Create {
                name: "contoso".into(),
                max_vectors: None,
                writer_profile: "a".repeat(64),
                initialization: "a".repeat(64),
                resources: Vec::new(),
                secret_versions: Vec::new(),
            },
            Payload::Migrate {
                tenant_id: ROUTED.into(),
                expected_source_route: "a".repeat(64),
                expected_source_lineage: 1,
                target_profile: "a".repeat(64),
                resources: Vec::new(),
                migration_profile: "a".repeat(64),
                verification_policy: "a".repeat(64),
                onboarding_policy: "a".repeat(64),
            },
        ];
        for payload in &held_first {
            let error = available(Verb::Cancel, &request, payload, None, None).unwrap_err();
            assert_eq!(error.code, "transition_unavailable");
        }
    }

    #[tokio::test]
    async fn unavailable_transitions_and_release_are_refused_before_persisting() {
        let f = fixture();
        let operation = f.create("onboard-contoso").await;
        let (status, body) = f.action(&operation, "rollback", "rollback", json!({})).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["code"], "transition_unavailable");
        // DD-002: delivery actions need their exact fields, then a seed.
        for verb in ["acknowledge-delivery", "replace-credentials"] {
            let (status, body) = f.action(&operation, verb, verb, json!({})).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{verb}");
            assert_eq!(body["error"]["code"], "operation_request_invalid");
        }
        let quoted = json!({"delivery_generation": 1, "credential_id": "abc"});
        let (status, body) = f
            .action(&operation, "acknowledge-delivery", "rollback", quoted)
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"]["code"], "seed_unrecorded");
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

    // ------------------------------------------- ITEM-010 / DD-002 delivery

    const LINEAGE: u64 = 0x77;
    const DAY: u64 = 24 * 3600 * 1000;

    fn delivery_grant() -> Value {
        json!({
            "name": "contoso-initial",
            "operations": ["read", "search", "write"],
            "expires_unix_ms": now() + 30 * DAY,
        })
    }

    fn secret_of(token: &str) -> &str {
        let rest = token.strip_prefix(credentials::TOKEN_PREFIX).unwrap();
        rest.split_once('_').unwrap().1
    }

    fn id_of(token: &str) -> &str {
        let rest = token.strip_prefix(credentials::TOKEN_PREFIX).unwrap();
        rest.split_once('_').unwrap().0
    }

    impl Fixture {
        async fn status(&self, operation_id: &str) -> Value {
            let path = format!("/v1/admin/tenant-deployment-operations/{operation_id}");
            let (status, body) = self.call("GET", &path, None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            body["operation"].clone()
        }

        /// A reserved create whose seed a worker recorded, as re-read.
        async fn seeded(&self, key: &str) -> Value {
            let operation = self.create(key).await;
            let id = operation["operation_id"].as_str().unwrap();
            let ops = self.deployments().ops;
            ops.claim(id, "worker-a", None).unwrap();
            let seed = Seed {
                lineage: LINEAGE,
                digest: sha256_hex(b"seed"),
            };
            ops.record_seed(id, "worker-a", &seed).unwrap();
            self.status(id).await
        }

        /// Fails if `needle` occurs in any stored object or any file under
        /// the admin directory (the audit log included).
        fn assert_nowhere(&self, needle: &str) {
            for name in self.memory.names() {
                let bytes = self.memory.raw(&name).unwrap();
                assert!(
                    !String::from_utf8_lossy(&bytes).contains(needle),
                    "plaintext in object {name}"
                );
            }
            let mut pending = vec![self.directory.path().to_owned()];
            while let Some(dir) = pending.pop() {
                for entry in std::fs::read_dir(dir).unwrap() {
                    let path = entry.unwrap().path();
                    if path.is_dir() {
                        pending.push(path);
                    } else if let Ok(bytes) = std::fs::read(&path) {
                        assert!(
                            !String::from_utf8_lossy(&bytes).contains(needle),
                            "plaintext in file {}",
                            path.display()
                        );
                    }
                }
            }
        }
    }

    /// DD-002: one authenticated HTTPS response carries the plaintext; the
    /// stored action result, the delivery record, the view and the audit log
    /// hold none of it, and a retry never mints another.
    #[tokio::test]
    async fn issue_credentials_delivers_the_token_once_in_the_admin_response() {
        let f = fixture();
        let operation = f.seeded("onboard-contoso").await;
        let (id, tenant) = (
            operation["operation_id"].as_str().unwrap().to_owned(),
            operation["tenant_id"].as_str().unwrap().to_owned(),
        );
        assert_eq!(operation["observed"]["credential_delivery"], Value::Null);
        let grant = json!({"grant": delivery_grant()});
        let (status, body) = f
            .action(&operation, "issue-credentials", "issue-1", grant.clone())
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let token = body["token"].as_str().unwrap().to_owned();
        let credential = id_of(&token).to_owned();
        assert_eq!(
            body["delivery"],
            json!({"generation": 1, "credential_id": credential, "acknowledged": false, "revoked": []})
        );
        assert_eq!(body["operation"]["phase"], "reserved");
        assert_eq!(body["operation"]["observed"]["routed"], false);
        assert_eq!(
            body["operation"]["observed"]["credential_delivery"],
            json!({"generation": 1, "credential_id": credential, "acknowledged": false})
        );
        // Hash-only, bound to the tenant and seeded lineage, route absent.
        let snapshot = f.snapshot();
        assert!(snapshot.record_for(&tenant).is_none());
        assert!(snapshot.records_publication(&tenant, &id, 1, &credential, &[]));
        assert_eq!(
            snapshot.live_credential(&credential, now()),
            Some((Some(tenant.clone()), LINEAGE))
        );
        f.assert_nowhere(secret_of(&token));
        // A replay returns the stored result: no plaintext, no new mint.
        let (status, replay) = f
            .action(&operation, "issue-credentials", "issue-1", grant.clone())
            .await;
        assert_eq!(status, StatusCode::OK, "{replay}");
        assert!(replay.get("token").is_none(), "{replay}");
        assert_eq!(replay["delivery"], body["delivery"]);
        // Another key cannot reissue either.
        let (status, again) = f
            .action(&operation, "issue-credentials", "issue-2", grant)
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{again}");
        assert_eq!(again["error"]["code"], "delivery_candidate_exists");
        assert!(!again.to_string().contains(secret_of(&token)));
        let delivery = f.deployments().ops.delivery(&id).unwrap().unwrap();
        assert_eq!((delivery.generation, delivery.credential_id.as_str()), (1, credential.as_str()));
        f.assert_nowhere(secret_of(&token));
    }

    /// DD-002: the grant is explicit and bounded, the seed must be recorded,
    /// and only an administrator of an open, reserved create may issue.
    #[tokio::test]
    async fn issue_credentials_needs_an_explicit_grant_a_seed_and_an_administrator() {
        let f = fixture();
        let unseeded = f.create("onboard-unseeded").await;
        let grant = json!({"grant": delivery_grant()});
        let (status, body) = f
            .action(&unseeded, "issue-credentials", "issue-unseeded", grant.clone())
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"]["code"], "seed_unrecorded");

        let operation = f.seeded("onboard-contoso").await;
        let id = operation["operation_id"].as_str().unwrap().to_owned();
        // No default grant: absent, or broader than the 365-day maximum.
        let (status, body) = f
            .action(&operation, "issue-credentials", "issue-nogrant", json!({}))
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["error"]["code"], "operation_request_invalid");
        let mut long = delivery_grant();
        long["expires_unix_ms"] = json!(now() + 366 * DAY);
        let (status, body) = f
            .action(&operation, "issue-credentials", "issue-long", json!({"grant": long}))
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        let mut unknown = delivery_grant();
        unknown["tenant_id"] = json!(ROUTED);
        let (status, _) = f
            .action(&operation, "issue-credentials", "issue-unknown", json!({"grant": unknown}))
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        // Delivery fields belong to other verbs.
        let (status, _) = f
            .action(&operation, "cancel", "cancel-grant", grant.clone())
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(f.deployments().ops.delivery(&id).unwrap().is_none());

        // Only the administrator: read-only and unauthenticated are refused.
        let reader = "tenant_deployment_reader_0123456789_ABCDEFGHIJ";
        f.service.admin.auth.lock().unwrap().read_only = Some(BearerToken::new(reader).unwrap());
        let path = format!("/v1/admin/tenant-deployment-operations/{id}/issue-credentials");
        let mut input = json!({
            "idempotency_key": "issue-reader",
            "expected_operation_generation": operation["operation_generation"],
            "expected_lifecycle_generation": operation["lifecycle_generation"],
            "grant": delivery_grant(),
        });
        let (status, body) = f.call_as(Some(reader), "POST", &path, Some(input.clone())).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        let (status, _) = f.call_as(None, "POST", &path, Some(input.clone())).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let open = fixture_with(true, None);
        input["idempotency_key"] = json!("issue-open");
        let (status, body) = open.call_as(None, "POST", &path, Some(input)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        assert!(f.deployments().ops.delivery(&id).unwrap().is_none());

        // A cancelled create issues nothing.
        let (status, body) = f.action(&operation, "cancel", "cancel-1", json!({})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = f
            .action(&body["operation"], "issue-credentials", "issue-late", grant)
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"]["code"], "operation_terminal");
        assert!(f.deployments().ops.delivery(&id).unwrap().is_none());
    }

    /// DD-002: acknowledgement quotes the exact generation and credential ID
    /// the view publishes; activation's `delivered` follows it.
    #[tokio::test]
    async fn acknowledge_delivery_needs_the_exact_published_generation_and_id() {
        let f = fixture();
        let operation = f.seeded("onboard-contoso").await;
        let id = operation["operation_id"].as_str().unwrap().to_owned();
        let (status, issued) = f
            .action(&operation, "issue-credentials", "issue-1", json!({"grant": delivery_grant()}))
            .await;
        assert_eq!(status, StatusCode::OK, "{issued}");
        let credential = issued["delivery"]["credential_id"].clone();
        let (status, body) = f
            .action(&operation, "acknowledge-delivery", "ack-0", json!({}))
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        for (key, generation, credential_id) in [
            ("ack-gen", json!(2), credential.clone()),
            ("ack-id", json!(1), json!("not-the-candidate")),
        ] {
            let quoted = json!({"delivery_generation": generation, "credential_id": credential_id});
            let (status, body) = f.action(&operation, "acknowledge-delivery", key, quoted).await;
            assert_eq!(status, StatusCode::CONFLICT, "{key}: {body}");
            assert_eq!(body["error"]["code"], "delivery_stale");
        }
        assert_eq!(
            f.deployments().ops.delivered(&id).unwrap_err().code,
            "credential_delivery_unacknowledged"
        );
        let quoted = json!({"delivery_generation": 1, "credential_id": credential});
        let (status, body) = f
            .action(&operation, "acknowledge-delivery", "ack-1", quoted.clone())
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.get("token").is_none());
        assert_eq!(body["delivery"]["acknowledged"], true);
        assert_eq!(
            body["operation"]["observed"]["credential_delivery"]["acknowledged"],
            true
        );
        f.deployments().ops.delivered(&id).unwrap();
        // Still unrouted: acknowledgement does not activate (OD-005).
        assert_eq!(body["operation"]["phase"], "reserved");
        assert_eq!(body["operation"]["observed"]["routed"], false);
        // An acknowledged candidate is never replaced.
        let replace = json!({
            "delivery_generation": 1, "credential_id": credential,
            "ack_irreversible": true, "grant": delivery_grant(),
        });
        let (status, body) = f
            .action(&operation, "replace-credentials", "replace-1", replace)
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"]["code"], "delivery_stale");
        assert!(body.get("token").is_none());
    }

    /// DD-002: a lost, unacknowledged token is replaced only by quoting it
    /// and acknowledging its revocation; the replacement is delivered once.
    #[tokio::test]
    async fn replace_credentials_explicitly_revokes_the_unacknowledged_generation() {
        let f = fixture();
        let operation = f.seeded("onboard-contoso").await;
        let (id, tenant) = (
            operation["operation_id"].as_str().unwrap().to_owned(),
            operation["tenant_id"].as_str().unwrap().to_owned(),
        );
        let (status, issued) = f
            .action(&operation, "issue-credentials", "issue-1", json!({"grant": delivery_grant()}))
            .await;
        assert_eq!(status, StatusCode::OK, "{issued}");
        let lost = issued["token"].as_str().unwrap().to_owned();
        let old = id_of(&lost).to_owned();
        let mut replace = json!({
            "delivery_generation": 1, "credential_id": old, "grant": delivery_grant(),
        });
        let (status, body) = f
            .action(&operation, "replace-credentials", "replace-noack", replace.clone())
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["error"]["code"], "irreversible_ack_required");
        replace["ack_irreversible"] = json!(true);
        let mut wrong = replace.clone();
        wrong["credential_id"] = json!("someone-else");
        let (status, body) = f
            .action(&operation, "replace-credentials", "replace-wrong", wrong)
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"]["code"], "delivery_stale");
        let mut nogrant = replace.clone();
        nogrant.as_object_mut().unwrap().remove("grant");
        let (status, _) = f
            .action(&operation, "replace-credentials", "replace-nogrant", nogrant)
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(f.deployments().ops.delivery(&id).unwrap().unwrap().generation, 1);

        let (status, body) = f
            .action(&operation, "replace-credentials", "replace-1", replace.clone())
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let token = body["token"].as_str().unwrap().to_owned();
        let new = id_of(&token).to_owned();
        assert_ne!(new, old);
        assert_eq!(
            body["delivery"],
            json!({"generation": 2, "credential_id": new, "acknowledged": false, "revoked": [old]})
        );
        let snapshot = f.snapshot();
        assert_eq!(snapshot.live_credential(&old, now()), None, "the lost token is revoked");
        assert_eq!(
            snapshot.live_credential(&new, now()),
            Some((Some(tenant.clone()), LINEAGE))
        );
        assert!(snapshot.records_publication(&tenant, &id, 2, &new, std::slice::from_ref(&old)));
        assert!(snapshot.record_for(&tenant).is_none());
        f.assert_nowhere(secret_of(&lost));
        f.assert_nowhere(secret_of(&token));
        // Retries never reissue: the replay is redacted, a new key is stale.
        let (status, replay) = f
            .action(&operation, "replace-credentials", "replace-1", replace.clone())
            .await;
        assert_eq!(status, StatusCode::OK, "{replay}");
        assert!(replay.get("token").is_none());
        let (status, body) = f
            .action(&operation, "replace-credentials", "replace-2", replace)
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"]["code"], "delivery_stale");
        // The revoked generation can no longer be acknowledged.
        let (status, _) = f
            .action(
                &operation,
                "acknowledge-delivery",
                "ack-old",
                json!({"delivery_generation": 1, "credential_id": old}),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, body) = f
            .action(
                &operation,
                "acknowledge-delivery",
                "ack-new",
                json!({"delivery_generation": 2, "credential_id": new}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        f.deployments().ops.delivered(&id).unwrap();
    }

    /// DD-002: an action accepted but never finished (its result write was
    /// lost) is applied again on replay, and that re-apply is refused by the
    /// delivery record: a retry never mints a second token.
    #[tokio::test]
    async fn a_lost_finish_never_reissues_the_candidate() {
        let f = fixture();
        let operation = f.seeded("onboard-contoso").await;
        let id = operation["operation_id"].as_str().unwrap().to_owned();
        let grant = json!({"grant": delivery_grant()});
        let (status, issued) = f
            .action(&operation, "issue-credentials", "issue-1", grant.clone())
            .await;
        assert_eq!(status, StatusCode::OK, "{issued}");
        let ops = f.deployments().ops;
        let actions: Vec<String> = f
            .memory
            .names()
            .into_iter()
            .filter(|name| name.contains(&id) && name.contains("/actions/"))
            .collect();
        assert_eq!(actions.len(), 1, "{actions:?}");
        let (bytes, etag) = ops.get(&actions[0]).unwrap().unwrap();
        let mut record: Value = serde_json::from_slice(&bytes).unwrap();
        record.as_object_mut().unwrap().remove("result").unwrap();
        ops.store
            .put(&actions[0], &serde_json::to_vec(&record).unwrap(), Cond::Match(&etag))
            .unwrap();
        let (status, body) = f
            .action(&operation, "issue-credentials", "issue-1", grant)
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"]["code"], "delivery_candidate_exists");
        assert!(body.get("token").is_none());
        let delivery = ops.delivery(&id).unwrap().unwrap();
        assert_eq!(delivery.generation, 1);
        assert_eq!(delivery.credential_id, issued["delivery"]["credential_id"].as_str().unwrap());
    }

    /// A create whose retained writer profile carries the DD-001
    /// `seed_configuration` and whose manifest is the strict empty one, so
    /// the real seed engine can seed it.
    async fn seedable(f: &Fixture, key: &str, name: &str) -> Value {
        const WRITER: &str = "/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/rg-1/providers/Microsoft.App/containerApps/tenant-a-writer";
        const EMPTY: &str = r#"{"mode":"empty"}"#;
        let profile = json!({
            "resources": [{
                "id": WRITER,
                "ownership": "owned",
                "body": {"location": "westus2", "properties": {
                    "environmentId": "/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/rg-1/providers/Microsoft.App/managedEnvironments/vlite-env",
                    "configuration": {"activeRevisionsMode": "Single"},
                    "template": {
                        "containers": [{"name": "vlite", "image": "example.azurecr.io/vlite@sha256:0000000000000000000000000000000000000000000000000000000000000000", "resources": {"cpu": 0.5, "memory": "1Gi"}}],
                        "scale": {"minReplicas": 1, "maxReplicas": 1}
                    }
                }}
            }],
            "seed_configuration": {"dimensions": 3, "encryption": {"mode": "none"}},
        })
        .to_string();
        let request = json!({
            "idempotency_key": key,
            "request": {
                "kind": "create",
                "name": name,
                "max_vectors": 1000,
                "writer_profile": sha(&profile),
                "initialization": sha(EMPTY),
                "resources": [WRITER],
                "secret_versions": [SECRET],
            },
            "contents": [profile, EMPTY],
        });
        let (status, body) = f
            .call("POST", "/v1/admin/tenant-deployments", Some(request))
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        body["operation"].clone()
    }

    /// ITEM-010 local end-to-end: the real offline seed engine creates the
    /// create's VLite database and create-only receipt from its retained
    /// writer profile, `seed::seed_create` journals the receipt's seed in
    /// the operation store the admin routes read, and the admin REST
    /// actions issue and acknowledge a candidate bound to the receipt's
    /// lineage. A crash before the durable receipt is refused and leaves
    /// issuance at `seed_unrecorded`; a crash after it is journaled on
    /// rerun; a changed database is refused. Activation still refuses
    /// without a qualified clock proof (OD-005): no route is published.
    /// Nothing here records a seed, issues a credential or proves readiness
    /// by hand.
    #[tokio::test]
    async fn a_real_seed_receipt_drives_delivery_and_activation_still_refuses() {
        use super::super::seed::{self, SeedConfig};
        let f = fixture();
        let ops = f.deployments().ops;
        let grant = json!({"grant": delivery_grant()});
        let files = |dir: &std::path::Path, id: &str| {
            (
                dir.join(format!("{id}.vlite")),
                dir.join(format!("{id}.seed-receipt.json")),
            )
        };

        // Crash before the receipt is durable: the database is preserved
        // and refused, nothing is journaled, so nothing can be issued.
        let lost = seedable(&f, "onboard-fabrikam", "fabrikam").await;
        let lost_id = lost["operation_id"].as_str().unwrap().to_owned();
        ops.claim(&lost_id, "worker-a", None).unwrap();
        let lost_dir = tempfile::tempdir().unwrap();
        let config = SeedConfig::from_loaded(&ops.load(&lost_id).unwrap()).unwrap();
        seed::seed(lost_dir.path(), &lost_id, &config, None).unwrap();
        let (database, receipt) = files(lost_dir.path(), &lost_id);
        std::fs::remove_file(&receipt).unwrap();
        let bytes = std::fs::read(&database).unwrap();
        for _ in 0..2 {
            let error = seed::seed_create(&ops, &lost_id, "worker-a", lost_dir.path(), None)
                .unwrap_err();
            assert_eq!(error.code, "seed_artifact_unverified");
            assert_eq!(std::fs::read(&database).unwrap(), bytes);
            assert!(!receipt.exists());
        }
        assert_eq!(recorded_seed(&ops.journal(&lost_id).unwrap()), None);
        let lost = f.status(&lost_id).await;
        let (status, body) = f
            .action(&lost, "issue-credentials", "issue-lost", grant.clone())
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"]["code"], "seed_unrecorded");
        assert!(ops.delivery(&lost_id).unwrap().is_none());

        // Crash after the durable receipt, before the journal step: still
        // unissuable until the rerun journals the verified receipt.
        let operation = seedable(&f, "onboard-contoso", "contoso").await;
        let (id, tenant) = (
            operation["operation_id"].as_str().unwrap().to_owned(),
            operation["tenant_id"].as_str().unwrap().to_owned(),
        );
        ops.claim(&id, "worker-a", None).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = SeedConfig::from_loaded(&ops.load(&id).unwrap()).unwrap();
        let made = seed::seed(dir.path(), &id, &config, None).unwrap();
        let operation = f.status(&id).await;
        let (status, body) = f
            .action(
                &operation,
                "issue-credentials",
                "issue-early",
                grant.clone(),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"]["code"], "seed_unrecorded");
        let journaled = seed::seed_create(&ops, &id, "worker-a", dir.path(), None).unwrap();
        assert_eq!(journaled, made);
        assert_eq!(recorded_seed(&ops.journal(&id).unwrap()), Some(made.seed()));
        let journal = ops.journal(&id).unwrap();
        assert_eq!(
            seed::seed_create(&ops, &id, "worker-a", dir.path(), None).unwrap(),
            made
        );
        assert_eq!(ops.journal(&id).unwrap(), journal);
        let (database, _) = files(dir.path(), &id);
        let db = crate::Database::open(&database).unwrap();
        assert_eq!(db.lineage(), Some(made.lineage));
        drop(db);
        assert_eq!(seed::verify(dir.path(), &id, &config, None).unwrap(), made);

        // Issuance over the admin route binds the candidate to the
        // receipt's lineage; a replay never reissues the token.
        let operation = f.status(&id).await;
        let (status, issued) = f
            .action(&operation, "issue-credentials", "issue-1", grant.clone())
            .await;
        assert_eq!(status, StatusCode::OK, "{issued}");
        let token = issued["token"].as_str().unwrap().to_owned();
        let credential = id_of(&token).to_owned();
        let snapshot = f.snapshot();
        assert_eq!(
            snapshot.live_credential(&credential, now()),
            Some((Some(tenant.clone()), made.lineage))
        );
        assert!(snapshot.records_publication(&tenant, &id, 1, &credential, &[]));
        let (status, replay) = f
            .action(&operation, "issue-credentials", "issue-1", grant)
            .await;
        assert_eq!(status, StatusCode::OK, "{replay}");
        assert!(replay.get("token").is_none(), "{replay}");

        // Acknowledgement validates the published, live candidate.
        let quoted = json!({"delivery_generation": 1, "credential_id": credential});
        let (status, body) = f
            .action(&operation, "acknowledge-delivery", "ack-1", quoted)
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let delivered = ops.delivered(&id).unwrap();
        assert_eq!(
            (delivered.credential_id.as_str(), delivered.generation),
            (credential.as_str(), 1)
        );

        // A changed database no longer matches its receipt: refused, the
        // journal is unchanged and nothing is re-seeded.
        let journal = ops.journal(&id).unwrap();
        let original = std::fs::read(&database).unwrap();
        let mut changed = original.clone();
        changed.push(0);
        std::fs::write(&database, &changed).unwrap();
        assert_eq!(
            seed::verify(dir.path(), &id, &config, None)
                .unwrap_err()
                .code,
            "seed_artifact_unverified"
        );
        let error = seed::seed_create(&ops, &id, "worker-a", dir.path(), None).unwrap_err();
        assert_eq!(error.code, "seed_artifact_unverified");
        assert_eq!(std::fs::read(&database).unwrap(), changed);
        assert_eq!(ops.journal(&id).unwrap(), journal);
        std::fs::write(&database, &original).unwrap();

        // Activation needs a readiness proof, and without a qualified clock
        // proof (OD-005) none can be made, whatever is observed.
        let intent = intent_of(&f, &id);
        let route = Route {
            reader: None,
            lineage: made.lineage,
            ..Route::dedicated_for_test(&tenant, 1)
        };
        let readiness = Readiness {
            archive_manifest_sha256: "1".repeat(64),
            writer: aca::Observed {
                owner: Some(format!("{}:{tenant}", aca::OWNER_TAG)),
                config_digest: sha("writer"),
                succeeded: true,
                revisions: aca::Revisions::Complete(Vec::new()),
                mounts: false,
            },
            policy_generation_required: 1,
            policy_generation_observed: 1,
            route,
        };
        let authority =
            crate::server::authority::AuthorityStore::new(f.memory.clone(), "target/".into());
        let error = prove(&ops, &authority, &intent, &readiness, None).unwrap_err();
        assert_eq!(
            (error.status, error.code),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "projection_freshness_unproven"
            )
        );
        let operation = f.status(&id).await;
        assert_eq!(operation["phase"], "reserved");
        assert_eq!(operation["observed"]["routed"], false);
        assert!(f.snapshot().record_for(&tenant).is_none());
        assert!(ops.terminal(&id).unwrap().is_none());
        f.assert_nowhere(secret_of(&token));
    }

    /// Runs `between` once, inside the minting action, after its view
    /// publication and before its response is assembled: just before the
    /// first delivery-record read that follows the control-state write.
    fn after_publication(f: &Fixture, between: impl FnOnce() + Send + 'static) {
        let memory = f.memory.clone();
        f.memory.hook(Op::Put, CONTROL, move || {
            memory.hook(Op::Get, "delivery.json", between);
        });
    }

    fn intent_of(f: &Fixture, id: &str) -> Intent {
        f.deployments().ops.load(id).unwrap().intent
    }

    fn direct_grant() -> super::super::CandidateGrant {
        super::super::CandidateGrant {
            name: "contoso-other".into(),
            operations: vec!["read".into()],
            graphs: None,
            expires_unix_ms: now() + 30 * DAY,
        }
    }

    /// Reviewer finding: the token of a candidate that a concurrent
    /// replacement, a replacement's record alone, or a cancellation
    /// superseded after its publication is dropped (409
    /// `candidate_superseded`), never paired with the newer candidate's ID
    /// and generation nor handed out once revoked; a replay mints nothing.
    #[tokio::test]
    async fn a_token_superseded_before_its_response_is_never_returned() {
        for case in ["replace", "record", "cancel"] {
            let f = fixture();
            let operation = f.seeded("onboard-contoso").await;
            let id = operation["operation_id"].as_str().unwrap().to_owned();
            let tenant = operation["tenant_id"].as_str().unwrap().to_owned();
            let intent = intent_of(&f, &id);
            let deployments = f.deployments();
            let other = Arc::new(std::sync::Mutex::new(None::<String>));
            let slot = other.clone();
            after_publication(&f, move || {
                let (ops, objects) = (&deployments.ops, &deployments.objects);
                match case {
                    "replace" => {
                        let minted = super::super::replace_published_candidate(
                            ops, objects, &intent, 1, &direct_grant(), now(),
                        )
                        .unwrap();
                        let (token, _) = minted.deliver(ops, objects, &intent, now()).unwrap();
                        *slot.lock().unwrap() = Some(token);
                    }
                    "record" => {
                        ops.replace_candidate(&intent.operation_id, 1, "zzzzzzzzzzzz")
                            .unwrap();
                    }
                    _ => request_cancel(ops, objects, &intent, now()).unwrap(),
                }
            });
            let grant = json!({"grant": delivery_grant()});
            let (status, body) = f
                .action(&operation, "issue-credentials", "issue-1", grant.clone())
                .await;
            assert_eq!(status, StatusCode::CONFLICT, "{case}: {body}");
            assert_eq!(body["error"]["code"], "candidate_superseded", "{case}");
            assert!(body.get("token").is_none(), "{case}: {body}");
            let delivery = f.deployments().ops.delivery(&id).unwrap().unwrap();
            let first = if case == "cancel" {
                assert_eq!(delivery.generation, 1);
                delivery.credential_id.clone()
            } else {
                assert_eq!(delivery.generation, 2, "{case}");
                delivery.revoked[0].clone()
            };
            let snapshot = f.snapshot();
            if case != "record" {
                assert_eq!(snapshot.live_credential(&first, now()), None, "{case}");
            }
            if case == "replace" {
                let token = other.lock().unwrap().clone().unwrap();
                assert_eq!(id_of(&token), delivery.credential_id);
                assert!(snapshot.authenticate(&token, now()).is_ok());
                assert_eq!(
                    snapshot.live_credential(&delivery.credential_id, now()),
                    Some((Some(tenant.clone()), LINEAGE))
                );
                f.assert_nowhere(secret_of(&token));
            }
            // A replay re-applies and is refused: nothing is minted again.
            let (status, replay) = f
                .action(&operation, "issue-credentials", "issue-1", grant)
                .await;
            assert_eq!(status, StatusCode::CONFLICT, "{case}: {replay}");
            assert!(replay.get("token").is_none(), "{case}: {replay}");
            assert_eq!(
                f.deployments().ops.delivery(&id).unwrap().unwrap(),
                delivery,
                "{case}"
            );
        }
    }

    /// Review finding: `deliver` reads the delivery record, then the control
    /// state last as its final validation. A cancellation just before either
    /// read, after the operation view, revokes the token while leaving the
    /// delivery record unchanged; it is still caught and the token dropped.
    /// A replacement recorded after the delivery read but not yet published
    /// at the final read is the documented race after that point: the token
    /// is returned with the generation it was published as, and the
    /// acknowledgement of that superseded generation is refused.
    #[tokio::test]
    async fn a_cancellation_before_the_final_control_read_drops_the_token() {
        for case in ["cancel-before-delivery", "cancel-before-control", "record-before-control"] {
            let f = fixture();
            let operation = f.seeded("onboard-contoso").await;
            let id = operation["operation_id"].as_str().unwrap().to_owned();
            let intent = intent_of(&f, &id);
            let deployments = f.deployments();
            let change = move || {
                let (ops, objects) = (&deployments.ops, &deployments.objects);
                if case == "record-before-control" {
                    ops.replace_candidate(&intent.operation_id, 1, "zzzzzzzzzzzz")
                        .unwrap();
                } else {
                    request_cancel(ops, objects, &intent, now()).unwrap();
                }
            };
            // After the publication the operation view reads the delivery
            // record first; `deliver` reads it second, then the control state.
            let memory = f.memory.clone();
            after_publication(&f, move || {
                let inner = memory.clone();
                memory.hook(Op::Get, "delivery.json", move || {
                    if case == "cancel-before-delivery" {
                        change();
                    } else {
                        inner.hook(Op::Get, CONTROL, change);
                    }
                });
            });
            let (status, body) = f
                .action(&operation, "issue-credentials", "issue-1", json!({"grant": delivery_grant()}))
                .await;
            let delivery = f.deployments().ops.delivery(&id).unwrap().unwrap();
            if case == "record-before-control" {
                assert_eq!(status, StatusCode::OK, "{case}: {body}");
                let token = body["token"].as_str().unwrap();
                let first = &delivery.revoked[0];
                assert_eq!(id_of(token), first);
                assert_eq!(body["delivery"]["generation"], 1, "{body}");
                assert_eq!(body["delivery"]["credential_id"], first.as_str());
                let observed = &body["operation"]["observed"];
                assert_eq!(observed["credential_delivery"]["generation"], 1, "{body}");
                assert_eq!(observed["cancellation_requested"], false, "{body}");
                assert_eq!(delivery.generation, 2);
                let (status, ack) = f
                    .action(
                        &operation,
                        "acknowledge-delivery",
                        "ack-1",
                        json!({"delivery_generation": 1, "credential_id": first}),
                    )
                    .await;
                assert_eq!(status, StatusCode::CONFLICT, "{ack}");
                assert_eq!(ack["error"]["code"], "delivery_stale");
                continue;
            }
            assert_eq!(status, StatusCode::CONFLICT, "{case}: {body}");
            assert_eq!(body["error"]["code"], "candidate_superseded", "{case}");
            assert!(body.get("token").is_none(), "{case}: {body}");
            // The delivery record never changed; only the view revoked it.
            assert_eq!(delivery.generation, 1, "{case}");
            assert!(!delivery.acknowledged, "{case}");
            assert_eq!(
                f.snapshot().live_credential(&delivery.credential_id, now()),
                None,
                "{case}"
            );
        }
    }

    /// The same for a replacement overtaken by another replacement: its
    /// token is dropped, the acknowledgement of its ID is refused, and only
    /// the winner's exact generation and ID can be acknowledged.
    #[tokio::test]
    async fn a_replacement_overtaken_by_another_returns_no_token() {
        let f = fixture();
        let operation = f.seeded("onboard-contoso").await;
        let id = operation["operation_id"].as_str().unwrap().to_owned();
        let (status, issued) = f
            .action(&operation, "issue-credentials", "issue-1", json!({"grant": delivery_grant()}))
            .await;
        assert_eq!(status, StatusCode::OK, "{issued}");
        let first = issued["delivery"]["credential_id"].as_str().unwrap().to_owned();
        let intent = intent_of(&f, &id);
        let deployments = f.deployments();
        after_publication(&f, move || {
            let (ops, objects) = (&deployments.ops, &deployments.objects);
            super::super::replace_published_candidate(
                ops, objects, &intent, 2, &direct_grant(), now(),
            )
            .unwrap();
        });
        let replace = json!({
            "delivery_generation": 1, "credential_id": first,
            "ack_irreversible": true, "grant": delivery_grant(),
        });
        let (status, body) = f
            .action(&operation, "replace-credentials", "replace-1", replace)
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"]["code"], "candidate_superseded");
        assert!(body.get("token").is_none());
        let delivery = f.deployments().ops.delivery(&id).unwrap().unwrap();
        assert_eq!(delivery.generation, 3);
        let second = delivery.revoked[1].clone();
        let snapshot = f.snapshot();
        assert_eq!(snapshot.live_credential(&first, now()), None);
        assert_eq!(snapshot.live_credential(&second, now()), None);
        let (status, body) = f
            .action(
                &operation,
                "acknowledge-delivery",
                "ack-overtaken",
                json!({"delivery_generation": 2, "credential_id": second}),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"]["code"], "delivery_stale");
        let (status, body) = f
            .action(
                &operation,
                "acknowledge-delivery",
                "ack-winner",
                json!({"delivery_generation": 3, "credential_id": delivery.credential_id}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["delivery"]["credential_id"], json!(delivery.credential_id));
        assert_eq!(body["delivery"]["generation"], 3);
    }

    /// A concurrent acknowledgement of the same candidate does not supersede
    /// it: the token is returned beside exactly its own ID and generation.
    #[tokio::test]
    async fn a_token_is_returned_only_beside_its_own_candidate() {
        let f = fixture();
        let operation = f.seeded("onboard-contoso").await;
        let id = operation["operation_id"].as_str().unwrap().to_owned();
        let deployments = f.deployments();
        let op = id.clone();
        after_publication(&f, move || {
            let delivery = deployments.ops.delivery(&op).unwrap().unwrap();
            deployments
                .ops
                .acknowledge_delivery(&op, delivery.generation, &delivery.credential_id)
                .unwrap();
        });
        let (status, body) = f
            .action(&operation, "issue-credentials", "issue-1", json!({"grant": delivery_grant()}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let token = body["token"].as_str().unwrap();
        assert_eq!(
            body["delivery"],
            json!({"generation": 1, "credential_id": id_of(token), "acknowledged": true, "revoked": []})
        );
        assert_eq!(
            body["operation"]["observed"]["credential_delivery"],
            json!({"generation": 1, "credential_id": id_of(token), "acknowledged": true})
        );
        assert!(f.snapshot().authenticate(token, now()).is_ok());
    }

    /// Every stored object and its bytes: a refusal must leave them as is.
    fn stored(f: &Fixture) -> Vec<(String, Vec<u8>)> {
        let mut names = f.memory.names();
        names.sort();
        names
            .into_iter()
            .map(|name| {
                let bytes = f.memory.raw(&name).unwrap();
                (name, bytes)
            })
            .collect()
    }

    async fn mint_refused(f: &Fixture, operation: &Value, key: &str) {
        for (verb, extra) in [
            ("issue-credentials", json!({"grant": delivery_grant()})),
            (
                "replace-credentials",
                json!({
                    "delivery_generation": 1, "credential_id": "aaaaaaaaaaaa",
                    "grant": delivery_grant(), "ack_irreversible": true,
                }),
            ),
        ] {
            let (status, body) = f.action(operation, verb, key, extra).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{verb}: {body}");
            assert_eq!(body["error"]["code"], "insecure_transport", "{verb}");
            assert!(body.get("token").is_none());
        }
    }

    /// Review finding 1: `no-store` is not HTTPS. Over plain HTTP (a
    /// loopback-only listener included) issue and replace are refused
    /// before anything is minted, recorded or published, and a client's
    /// own `X-Forwarded-Proto` is not trusted without a configured proxy.
    #[tokio::test]
    async fn minting_over_plain_http_is_refused_before_anything_is_persisted() {
        let mut f = fixture_over(true, Some(TOKEN), false, false);
        let operation = f.seeded("onboard-contoso").await;
        let id = operation["operation_id"].as_str().unwrap().to_owned();
        let before = stored(&f);
        mint_refused(&f, &operation, "issue-plain").await;
        f.headers = vec![("x-forwarded-proto", "https")];
        mint_refused(&f, &operation, "issue-forged").await;
        assert_eq!(stored(&f), before, "a refusal persists nothing");
        assert!(f.deployments().ops.delivery(&id).unwrap().is_none());
        // Non-minting actions are unaffected.
        let (status, body) = f.action(&operation, "resume", "issue-plain", json!({})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    /// Review finding 1: behind `VLITE_TRUSTED_PROXY=1` only the proxy's
    /// own (last) `X-Forwarded-Proto: https` proves HTTPS.
    #[tokio::test]
    async fn minting_behind_a_trusted_proxy_needs_its_https_forwarding() {
        let mut f = fixture_over(true, Some(TOKEN), false, true);
        let operation = f.seeded("onboard-contoso").await;
        let id = operation["operation_id"].as_str().unwrap().to_owned();
        let before = stored(&f);
        for (key, headers) in [
            ("issue-none", vec![]),
            ("issue-http", vec![("x-forwarded-proto", "http")]),
            // A client-sent value the proxy appended `http` to.
            ("issue-spoofed", vec![("x-forwarded-proto", "https, http")]),
        ] {
            f.headers = headers;
            mint_refused(&f, &operation, key).await;
        }
        assert_eq!(stored(&f), before);
        f.headers = vec![("x-forwarded-proto", "https")];
        let (status, body) = f
            .action(&operation, "issue-credentials", "issue-1", json!({"grant": delivery_grant()}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let token = body["token"].as_str().unwrap();
        assert_eq!(
            f.deployments().ops.delivery(&id).unwrap().unwrap().credential_id,
            id_of(token)
        );
    }

    /// Review finding 2: cancelling an issued create revokes its candidate
    /// in the cancellation's own CAS, so a lost release cannot leave it
    /// live and the completed release never does.
    #[tokio::test]
    async fn cancel_after_issue_leaves_no_live_candidate() {
        let f = fixture();
        let operation = f.seeded("onboard-contoso").await;
        let tenant = operation["tenant_id"].as_str().unwrap().to_owned();
        let (status, issued) = f
            .action(&operation, "issue-credentials", "issue-1", json!({"grant": delivery_grant()}))
            .await;
        assert_eq!(status, StatusCode::OK, "{issued}");
        let token = issued["token"].as_str().unwrap().to_owned();
        let credential = id_of(&token).to_owned();
        assert!(f.snapshot().authenticate(&token, now()).is_ok());
        // The cancellation request lands; the release after the seal fails.
        f.memory.fault(Op::Put, CONTROL, Fault::Refuse, 1, 1);
        let (status, body) = f
            .action(&issued["operation"], "cancel", "cancel-1", json!({}))
            .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        let snapshot = f.snapshot();
        assert!(matches!(
            snapshot.lifecycle(&tenant).map(|entry| &entry.slot),
            Some(Slot::Reserved {
                cancellation_requested: true,
                ..
            })
        ));
        assert_eq!(snapshot.live_credential(&credential, now()), None);
        assert!(snapshot.authenticate(&token, now()).is_err());
        let (status, body) = f
            .action(&issued["operation"], "cancel", "cancel-1", json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["operation"]["observed"]["reserved"], false);
        let snapshot = f.snapshot();
        assert!(matches!(
            snapshot.lifecycle(&tenant).map(|entry| &entry.slot),
            Some(Slot::Released { .. })
        ));
        assert_eq!(snapshot.live_credential(&credential, now()), None);
        assert!(snapshot.authenticate(&token, now()).is_err());
    }

    /// DD-002: the command line never prints a credential; it may only
    /// acknowledge. Nothing is persisted by the refusal.
    #[test]
    fn cli_never_mints_a_candidate_credential() {
        let directory = tempfile::tempdir().unwrap();
        let store = tenants::Store::load(directory.path()).unwrap();
        let credentials = credentials::Store::load(directory.path()).unwrap();
        let memory = Arc::new(Memory::default());
        Objects::new(memory.clone(), CONTROL)
            .create(Snapshot::import(&credentials, Some(&store)).unwrap(), NOW)
            .unwrap();
        let deployments = Deployments::new(memory.clone(), CONTROL, PREFIX);
        let submit = Command::Submit {
            kind: Kind::Create,
            tenant: None,
            request: create_request("cli-key", "contoso")
                .to_string()
                .into_bytes(),
        };
        let created = run(&deployments, &store, None, submit, NOW).unwrap();
        let id = created["operation"]["operation_id"]
            .as_str()
            .unwrap()
            .to_owned();
        deployments.ops.claim(&id, "worker-a", None).unwrap();
        let seed = Seed {
            lineage: LINEAGE,
            digest: sha256_hex(b"seed"),
        };
        deployments.ops.record_seed(&id, "worker-a", &seed).unwrap();
        let operation = status(&deployments, &id).unwrap();
        let before = memory.names();
        for verb in ["issue-credentials", "replace-credentials"] {
            let request = json!({
                "idempotency_key": verb,
                "expected_operation_generation": operation["operation_generation"],
                "expected_lifecycle_generation": operation["lifecycle_generation"],
                "delivery_generation": 1,
                "credential_id": "abc",
                "ack_irreversible": true,
                "grant": {
                    "name": "contoso-initial",
                    "operations": ["read"],
                    "expires_unix_ms": NOW + DAY,
                },
            });
            let mut request = request;
            if verb == "issue-credentials" {
                let fields = request.as_object_mut().unwrap();
                fields.remove("delivery_generation");
                fields.remove("credential_id");
                fields.remove("ack_irreversible");
            }
            let command = Command::Act {
                operation: id.clone(),
                verb: verb.into(),
                request: request.to_string().into_bytes(),
            };
            let error = run(&deployments, &store, None, command, NOW).unwrap_err();
            assert_eq!(error.code, "transition_unavailable", "{verb}");
        }
        assert_eq!(memory.names(), before, "nothing was persisted");
        assert!(deployments.ops.delivery(&id).unwrap().is_none());
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

    /// ITEM-008: the legacy reader-pin path rewrites whole set records;
    /// isolated pins and an offline scoped-grant template exist, but
    /// production scope and mount enforcement remain unproven. R1: a reader
    /// submission takes no slot (only its plan-anchored actuator reserves);
    /// EPIC-005: cancelling it withdraws it without a slot, so later
    /// lifecycle anchoring is never poisoned.
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
        // withdraws the unreserved submission and leaves the slot alone.
        let (status, body) = f.action(&operation, "cancel", "resume-1", json!({})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["operation"]["phase"], "cancelled");
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

    /// Capabilities advertise `features.tenant_deployments` exactly when the
    /// managed routes are configured, so the dashboard need not probe for 404.
    #[test]
    fn capabilities_advertise_whether_tenant_deployments_are_configured() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for (configured, token) in [
            (true, Some(TOKEN)),
            (false, Some(TOKEN)),
            (true, None),
            (false, None),
        ] {
            let f = fixture_with(configured, token);
            let (status, body) =
                runtime.block_on(f.call_as(token, "GET", "/v1/capabilities", None));
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(
                body["features"]["tenant_deployments"],
                json!(configured),
                "configured={configured} token={token:?}: {body}"
            );
        }
        // The flag follows the managed runtime wiring too.
        let directory = tempfile::tempdir().unwrap();
        let (config, _) = runtime_files(directory.path());
        let opened = TenantDeploymentsConfig::read(&config)
            .unwrap()
            .open(&directory.path().join("admin"))
            .unwrap();
        let mut served = fixture_with(false, Some(TOKEN));
        served.app = crate::server::configured_routes(
            served
                .service
                .clone()
                .with_managed_tenant_deployments(&opened),
        );
        let (status, body) = runtime.block_on(served.call("GET", "/v1/capabilities", None));
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["features"]["tenant_deployments"], true, "{body}");
        drop(served);
        drop(runtime);
    }

    const LIST: &str = "/v1/admin/tenant-deployment-operations";

    /// ITEM-023 (TEST-015): the list is refused exactly like its sibling
    /// status route, and a read-only token is not an administrator.
    #[tokio::test]
    async fn operation_list_needs_configuration_and_administrator_authentication() {
        let unconfigured = fixture_with(false, Some(TOKEN));
        let (status, body) = unconfigured.call("GET", LIST, None).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"]["code"], "tenant_deployments_not_configured");
        let open = fixture_with(true, None);
        let (status, body) = open.call_as(None, "GET", LIST, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        assert_eq!(body["error"]["code"], "admin_auth_required");
        let f = fixture();
        let (status, _) = f.call_as(None, "GET", LIST, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let reader = "tenant_deployment_reader_0123456789_ABCDEFGHIJ";
        f.service.admin.auth.lock().unwrap().read_only = Some(BearerToken::new(reader).unwrap());
        let sibling = format!("{LIST}/{}", "0".repeat(32));
        for path in [LIST, sibling.as_str()] {
            let (status, body) = f.call_as(Some(reader), "GET", path, None).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {body}");
            assert_eq!(body["error"]["code"], "read_only", "{path}");
        }
    }

    /// ITEM-023 (TEST-015): pages of exactly the sibling's redacted status,
    /// ascending by operation ID after an exclusive cursor.
    #[tokio::test]
    async fn operation_list_pages_the_redacted_status_by_operation_id() {
        let f = fixture();
        let (status, body) = f.call("GET", LIST, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, json!({"operations": [], "next_after": null}));
        let reserved = f.create("list-reserved-secret-key").await;
        let cancelled = f.create("list-cancelled-secret-key").await;
        let (status, body) = f
            .action(&cancelled, "cancel", "list-cancel-secret-key", json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let readers = json!({
            "idempotency_key": "list-readers-secret-key",
            "request": {"kind": "set_readers", "tenant_id": ROUTED, "fleet": {"state": "absent"}},
        });
        let path = format!("/v1/admin/tenant-deployments/{ROUTED}/readers");
        let (status, accepted) = f.call("POST", &path, Some(readers)).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
        let mut ids: Vec<String> = [&reserved, &cancelled, &accepted["operation"]]
            .iter()
            .map(|operation| operation["operation_id"].as_str().unwrap().to_owned())
            .collect();
        ids.sort();
        let mut statuses = Vec::new();
        for id in &ids {
            let (status, body) = f.call("GET", &format!("{LIST}/{id}"), None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            statuses.push(body["operation"].clone());
        }
        let mut phases: Vec<&str> = statuses
            .iter()
            .map(|operation| operation["phase"].as_str().unwrap())
            .collect();
        phases.sort();
        assert_eq!(phases, ["accepted", "cancelled", "reserved"]);
        for (query, expected) in [
            (
                String::new(),
                json!({"operations": statuses, "next_after": null}),
            ),
            (
                "?limit=3".to_owned(),
                json!({"operations": statuses, "next_after": null}),
            ),
            (
                "?limit=2".to_owned(),
                json!({"operations": statuses[..2], "next_after": ids[1]}),
            ),
            (
                format!("?after={}&limit=2", ids[1]),
                json!({"operations": statuses[2..], "next_after": null}),
            ),
            (
                format!("?limit=1&after={}", ids[0]),
                json!({"operations": statuses[1..2], "next_after": ids[1]}),
            ),
            (
                format!("?after={}", "0".repeat(32)),
                json!({"operations": statuses, "next_after": null}),
            ),
            (
                format!("?after={}", ids[2]),
                json!({"operations": [], "next_after": null}),
            ),
            (
                format!("?after={}", "f".repeat(32)),
                json!({"operations": [], "next_after": null}),
            ),
        ] {
            let (status, body) = f.call("GET", &format!("{LIST}{query}"), None).await;
            assert_eq!(status, StatusCode::OK, "{query}: {body}");
            assert_eq!(body, expected, "{query}");
        }
        let (_, body) = f.call("GET", LIST, None).await;
        let text = body.to_string();
        for secret in [
            "list-reserved-secret-key",
            "list-cancelled-secret-key",
            "list-cancel-secret-key",
            "list-readers-secret-key",
            &sha("list-reserved-secret-key"),
            SECRET,
            PROFILE,
            TOKEN,
            "idempotency",
        ] {
            assert!(!text.contains(secret), "the list leaks {secret}: {text}");
        }
    }

    /// ITEM-023: a malformed, out-of-range, repeated or unknown query
    /// parameter is refused like kv's and logs' list queries.
    #[tokio::test]
    async fn operation_list_refuses_invalid_queries() {
        let f = fixture();
        let afters = [
            "a".repeat(31),
            "a".repeat(33),
            "A".repeat(32),
            "g".repeat(32),
        ];
        for query in [
            "limit=0",
            "limit=101",
            "limit=-1",
            "limit=%2B1",
            "limit=+1",
            "limit=%201",
            "limit=1.0",
            "limit=abc",
            "limit=",
            "limit=01",
            "limit=1&limit=2",
            "after=",
            "after=not-an-operation-id",
            "cursor=x",
            "limit=1&tenant=x",
        ]
        .map(str::to_owned)
        .into_iter()
        .chain(afters.map(|after| format!("after={after}")))
        {
            let (status, body) = f.call("GET", &format!("{LIST}?{query}"), None).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
            assert_eq!(body["error"]["code"], "invalid_query", "{query}");
        }
        for query in ["limit=1", "limit=100"] {
            let (status, body) = f.call("GET", &format!("{LIST}?{query}"), None).await;
            assert_eq!(status, StatusCode::OK, "{query}: {body}");
        }
    }

    /// ITEM-023: an operation is listed iff its intent exists, as status
    /// decides, and other names under the prefix are ignored. A page reads
    /// only its own records; one it cannot read fails it as status fails.
    #[tokio::test]
    async fn operation_list_reads_only_its_page_and_ignores_stray_names() {
        let f = fixture();
        let mut ids = Vec::new();
        for key in ["list-a", "list-b", "list-c"] {
            let operation = f.create(key).await;
            ids.push(operation["operation_id"].as_str().unwrap().to_owned());
        }
        ids.sort();
        let (status, all) = f.call("GET", LIST, None).await;
        assert_eq!(status, StatusCode::OK, "{all}");
        for stray in [
            format!("{PREFIX}/stray.json"),
            format!("{PREFIX}/contents/{}", sha("stray")),
            format!("{PREFIX}/operations/not-an-operation/intent.json"),
            format!("{PREFIX}/operations/{}/intent.json", "A".repeat(32)),
            format!("{PREFIX}/operations/{}/journal.json", "b".repeat(32)),
            format!("{PREFIX}/operations/{}/actions/{}.json", ids[0], sha("x")),
        ] {
            f.memory.set_raw(&stray, b"{}");
        }
        assert_eq!(
            f.call("GET", LIST, None).await,
            (StatusCode::OK, all.clone())
        );
        let operations = all["operations"].as_array().unwrap();
        // The third record is unreadable: only a page holding it fails.
        f.memory.fault(
            Op::Get,
            &format!("{}/", ids[2]),
            Fault::Refuse,
            0,
            usize::MAX,
        );
        let (status, body) = f.call("GET", &format!("{LIST}?limit=2"), None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body,
            json!({"operations": operations[..2], "next_after": ids[1]})
        );
        let single = f.call("GET", &format!("{LIST}/{}", ids[2]), None).await;
        assert_eq!(single.0, StatusCode::SERVICE_UNAVAILABLE, "{}", single.1);
        let after = format!("{LIST}?after={}", ids[1]);
        assert_eq!(f.call("GET", &after, None).await, single);
        // A corrupt intent fails its page with status's error too.
        f.memory.set_raw(
            &format!("{PREFIX}/operations/{}/intent.json", ids[0]),
            b"not json",
        );
        let single = f.call("GET", &format!("{LIST}/{}", ids[0]), None).await;
        assert_eq!(single.1["error"]["code"], "operation_state_invalid");
        let first = format!("{LIST}?limit=1");
        assert_eq!(f.call("GET", &first, None).await, single);
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

    /// ITEM-022: `work` takes only an optional `--operation` beside the
    /// store flags, and holds the admin directory like every verb.
    #[test]
    fn cli_work_takes_an_optional_operation_under_the_admin_lock() {
        let args = |values: &[&str]| values.iter().map(OsString::from).collect::<Vec<_>>();
        for refused in [
            &["work", "--request-file", "x"][..],
            &["work", "--tenant", "t"],
            &["work", "--operation"],
            &["work"],
        ] {
            let error = tenant_deployment_command(&args(refused)).unwrap_err();
            assert!(error.starts_with("invalid_arguments"), "{error}");
        }
        assert!(USAGE.contains("| work [--operation <id>] |"));
        let directory = tempfile::tempdir().unwrap();
        let (config, value) = runtime_files(directory.path());
        let admin = directory.path().join("admin");
        let opened = TenantDeploymentsConfig::read(&config)
            .unwrap()
            .open(&admin)
            .unwrap();
        let mut work = vec![
            "work",
            "--operation",
            "0",
            "--target-file",
            value["target_file"].as_str().unwrap(),
            "--object",
            "control/state.json",
            "--admin-dir",
            admin.to_str().unwrap(),
        ];
        let error = tenant_deployment_command(&args(&work)).unwrap_err();
        assert!(error.starts_with("admin_dir_busy"), "{error}");
        drop(opened);
        // Refused before any store request: the target's port is closed.
        work.extend([
            "--shared-key-file",
            value["shared_key_file"].as_str().unwrap(),
        ]);
        let error = tenant_deployment_command(&args(&work)).unwrap_err();
        assert!(error.starts_with("operation_not_found"), "{error}");
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
