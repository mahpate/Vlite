//! design-tenant-provisioning EPIC-004 ITEM-009/011: Azure Container Apps
//! effects behind a mockable ARM port, journaled by the ITEM-003 contract.
//!
//! [`Arm`] is the port; nothing in production implements it, so no cloud
//! action can be dispatched (OD-002: no platform proof that a former
//! actuator stopped, and no qualified effects adapter). Every effect goes
//! through one [`Driver`], which claims the operation first: without an
//! `ActuatorStopped` proof a second attempt is refused before the port is
//! called.
//!
//! - ITEM-009: an operation-owned resource is created with a create-only
//!   PUT (the port contract: `If-None-Match: *`), recorded before dispatch,
//!   its cloud operation ID persisted as soon as it is returned, polled a
//!   bounded number of times and settled only from an exact readback
//!   (owner tag, configuration, provisioning state and, for an app, a
//!   complete revision inventory with exactly one active revision). A
//!   resume polls the persisted operation and reads back; it never
//!   dispatches again. A resource that exists before its first dispatch is
//!   refused, never adopted (so neither is the singleton); a prerequisite
//!   the profile names as pre-existing is only read and verified.
//! - ITEM-011: [`preflight`] needs a supported profile and qualification,
//!   quota and budget proofs that no production code can produce yet
//!   (OD-006), so it refuses; [`Driver::cleanup`] deletes, after a cancel,
//!   only apps this operation created; [`Driver::retire`] removes the
//!   verified route, stops the apps and seals `retired`. Data, archive and
//!   every pre-existing or non-app resource are always retained.
//! - EPIC-005 ITEM-012/013 (offline): [`Driver::reserve_readers`] anchors a
//!   reader change on the tenant's latest sealed receipt and plans the
//!   route it will publish; [`Driver::set_readers`] creates or updates the
//!   one private reader app, requires an eligible observed fleet (the
//!   writer authority's head, lineage, epoch, binding and staleness bound;
//!   unique replicas within the desired bounds) before it republishes the
//!   route with the reader endpoint, and removes a reader by unpublishing
//!   the endpoint before stopping the app. The route stays writer-only and
//!   the writer is never touched. Every proof (pin scope, observation, stop)
//!   exists only in tests: production refuses (ITEM-008, OD-002, OD-005).
use super::guard::{self, Guards};
use super::*;
use crate::server::cloud_backup::{Pin, PinKind, Sets};

/// Owner tag value prefix on every resource this contract creates.
pub(super) const OWNER_TAG: &str = "vlite-tenant";
/// Journal action names.
const CREATE: &str = "create";
const CREATE_APP: &str = "create-app";
const VERIFY: &str = "verify";
const DELETE_APP: &str = "delete-app";
const STOP_APP: &str = "stop-app";
/// EPIC-005 journal actions: an in-place reader update, the observed
/// fleet's eligibility (a read; `fleet/<app>`) and a control-owned release
/// of a stopped replica's pin (`pin/<set>/<pin>`).
const UPDATE_APP: &str = "update-app";
const ELIGIBLE: &str = "eligible";
const RELEASE_PIN: &str = "release-pin";
/// Reader plan tokens (EPIC-005), after the anchor and its route.
const TARGET: &str = "target=";
const ENDPOINT: &str = "endpoint=";
const READER: &str = "reader=";
const NO_ENDPOINT: &str = "-";
/// The retirement plan step (ITEM-011).
const PLAN: &str = "control/receipt";
/// Journal resource for the retirement's lifecycle guard hold/release.
const GUARD: &str = "control/guard";
const HOLD: &str = "hold";
const RELEASE: &str = "release";
/// A hold step's durable binding: the one guard version its writes may
/// replace (absence, or `GUARD_VERSION` + etag), or the cancellation's
/// fence, after which no hold write may be issued.
const GUARD_ABSENT: &str = "guard:absent";
const GUARD_VERSION: &str = "guard:version:";
const GUARD_FENCED: &str = "guard:fenced";

// ------------------------------------------------------------------ port

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Verb {
    /// Create-only: the port must refuse to overwrite an existing resource.
    Put {
        config_digest: String,
    },
    Delete,
    /// Deactivate every revision; the app and its storage remain.
    Stop,
    /// Roll an existing app to a new configuration (one new active
    /// revision); the port must refuse a missing app.
    Update {
        config_digest: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Call {
    pub(super) resource: String,
    /// The owner tag a `Put` sets.
    pub(super) owner: String,
    pub(super) verb: Verb,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Poll {
    Running,
    Succeeded,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ArmError {
    /// The platform refused the request; it took no effect.
    Refused(String),
    /// Whether the request took effect is unknown.
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Revision {
    pub(super) name: String,
    pub(super) active: bool,
    pub(super) config_digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Revisions {
    /// Not an app.
    NotApplicable,
    /// Every revision, active or not.
    Complete(Vec<Revision>),
    /// Paged or partial: never proof.
    Incomplete,
}

/// One resource as the platform reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Observed {
    pub(super) owner: Option<String>,
    pub(super) config_digest: String,
    pub(super) succeeded: bool,
    pub(super) revisions: Revisions,
}

/// The ARM effects port. No production implementation exists (OD-002).
pub(super) trait Arm {
    /// Starts `call`; returns the cloud's asynchronous operation ID.
    fn begin(&self, call: &Call) -> Result<String, ArmError>;
    fn poll(&self, operation: &str) -> Result<Poll, ArmError>;
    /// The resource, or None when it does not exist.
    fn read(&self, resource: &str) -> Result<Option<Observed>, ArmError>;
}

// --------------------------------------------------------------- profile

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Ownership {
    /// Created and owned by the operation.
    Owned,
    /// Named by the profile as pre-existing: read and verified only.
    Verified,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Desired {
    pub(super) resource: String,
    pub(super) config_digest: String,
    pub(super) app: bool,
    pub(super) ownership: Ownership,
}

/// Peak demand, including revision overlap during a rollout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Demand {
    pub(super) apps: u32,
    pub(super) cores_milli: u64,
    pub(super) memory_mib: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Profile {
    resources: Vec<Desired>,
    peak: Demand,
}

impl Profile {
    /// No approved profile schema exists yet (OD-006): a retained profile
    /// is never interpreted as a deployable resource plan.
    pub(super) fn resolve(_loaded: &Loaded) -> Result<Self, ApiError> {
        Err(conflict(
            "profile_unsupported",
            "no approved deployment profile schema exists yet (OD-006); nothing is provisioned",
        ))
    }

    #[cfg(test)]
    pub(super) fn for_test(resources: Vec<Desired>, peak: Demand) -> Self {
        Self { resources, peak }
    }

    fn valid(&self) -> bool {
        let mut names = std::collections::BTreeSet::new();
        let apps = self.resources.iter().filter(|d| d.app).count();
        !self.resources.is_empty()
            && self.resources.iter().all(|d| {
                text(&d.resource)
                    && d.resource.len() <= 512
                    && !d.resource.contains(' ')
                    && !d.resource.contains('=')
                    && d.resource != PLAN
                    && is_hex(&d.config_digest, 64)
                    && names.insert(d.resource.as_str())
            })
            && u32::try_from(apps).is_ok_and(|apps| apps <= self.peak.apps)
    }
}

/// Qualification evidence for the profile (feature-aca-ha gates). Only
/// tests construct it.
pub(super) struct Qualification(());
/// Proven quota headroom in the target region. Only tests construct it.
pub(super) struct QuotaProof {
    headroom: Demand,
}
/// An approved cost budget (OD-006). Only tests construct it.
pub(super) struct BudgetApproval(());

#[cfg(test)]
impl Qualification {
    pub(super) fn for_test() -> Self {
        Self(())
    }
}

#[cfg(test)]
impl QuotaProof {
    pub(super) fn for_test(headroom: Demand) -> Self {
        Self { headroom }
    }
}

#[cfg(test)]
impl BudgetApproval {
    pub(super) fn for_test() -> Self {
        Self(())
    }
}

/// A profile that passed [`preflight`]; the only input `provision` takes.
pub(super) struct Preflighted {
    profile: Profile,
}

/// ITEM-011 preflight. Every missing proof is a `blocked` refusal (409),
/// never success, in this order.
pub(super) fn preflight(
    profile: Profile,
    qualification: Option<&Qualification>,
    quota: Option<&QuotaProof>,
    budget: Option<&BudgetApproval>,
) -> Result<Preflighted, ApiError> {
    if !profile.valid() {
        return Err(invalid("deployment profile is invalid"));
    }
    if qualification.is_none() {
        return Err(conflict(
            "qualification_missing",
            "the profile has no accepted qualification evidence",
        ));
    }
    let Some(quota) = quota else {
        return Err(conflict(
            "quota_unproven",
            "no quota proof covers the profile's peak demand",
        ));
    };
    let (peak, room) = (profile.peak, quota.headroom);
    if peak.apps > room.apps
        || peak.cores_milli > room.cores_milli
        || peak.memory_mib > room.memory_mib
    {
        return Err(conflict(
            "quota_exceeded",
            "the profile's peak demand exceeds the proven quota headroom",
        ));
    }
    if budget.is_none() {
        return Err(conflict(
            "budget_unapproved",
            "no approved cost budget covers the profile (OD-006)",
        ));
    }
    Ok(Preflighted { profile })
}

/// EPIC-005: proof that a reader's set pins are scoped to its tenant
/// (ITEM-008). None can exist while pins rewrite the whole set record in
/// the writer's coordination scope; only tests construct it, so every
/// production reader fleet refuses `pin_scope_unproven`.
pub(super) struct PinScope(());

/// EPIC-005: one reader replica as observed (enrollment and follower
/// status). No production source exists yet; only tests construct a
/// [`FleetObservation`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ReplicaStatus {
    pub(super) replica: String,
    /// The reader app and revision the replica runs in: it counts only in
    /// the tenant's reader app's single active revision.
    pub(super) app: String,
    pub(super) revision: String,
    /// The verified set the replica serves.
    pub(super) set_id: String,
    pub(super) lineage: u64,
    pub(super) binding_generation: u64,
    pub(super) epoch: u64,
    /// Time since the replica last confirmed it serves the newest verified
    /// checkpoint, and its configured staleness bound (0: unset).
    pub(super) age_ms: u64,
    pub(super) bound_ms: u64,
    pub(super) failed: bool,
}

/// The writer authority and every reader replica, read together.
pub(super) struct FleetObservation {
    authority: authority::Record,
    replicas: Vec<ReplicaStatus>,
}

/// Platform proof that a reader replica of `tenant`'s reader `app` stopped
/// at `stopped_unix_ms` (on the clock its pins were created by: OD-005).
/// Only tests construct it.
pub(super) struct ReplicaStopped {
    tenant: String,
    app: String,
    replica: String,
    stopped_unix_ms: u64,
}

#[cfg(test)]
impl PinScope {
    pub(super) fn for_test() -> Self {
        Self(())
    }
}

#[cfg(test)]
impl FleetObservation {
    pub(super) fn for_test(authority: authority::Record, replicas: Vec<ReplicaStatus>) -> Self {
        Self {
            authority,
            replicas,
        }
    }
}

#[cfg(test)]
impl ReplicaStopped {
    pub(super) fn for_test(tenant: &str, app: &str, replica: &str, stopped_unix_ms: u64) -> Self {
        Self {
            tenant: tenant.to_owned(),
            app: app.to_owned(),
            replica: replica.to_owned(),
            stopped_unix_ms,
        }
    }
}

/// An app a reader plan names: the writer's, or the operation-owned reader.
#[derive(Clone, Debug, PartialEq, Eq)]
struct App {
    name: String,
    reader: bool,
}

/// A verified reader change (EPIC-005), as journaled at reservation: the
/// anchoring operation, the route it published (`planned`, the reserved
/// route), the route this change publishes (`target`), the reader endpoint
/// and every app the tenant owns.
struct ReaderPlan {
    planned: String,
    target: String,
    endpoint: Option<String>,
    apps: Vec<App>,
}

impl ReaderPlan {
    fn parse(plan: &str) -> Option<Self> {
        let mut words = plan.split(' ').skip(1);
        let planned = words.next()?.to_owned();
        let target = words.next()?.strip_prefix(TARGET)?.to_owned();
        let endpoint = match words.next()?.strip_prefix(ENDPOINT)? {
            NO_ENDPOINT => None,
            endpoint => Some(endpoint.to_owned()),
        };
        Some(Self {
            planned,
            target,
            endpoint,
            apps: words.map(app).collect(),
        })
    }

    fn readers(&self) -> impl Iterator<Item = &str> {
        self.apps
            .iter()
            .filter(|app| app.reader)
            .map(|app| app.name.as_str())
    }
}

/// The tenant's latest sealed receipt, its published route and owned apps,
/// and the slot's released operation and terminal digest as read (what a
/// reservation is bound to).
struct Anchor {
    operation_id: String,
    receipt: Receipt,
    route: Route,
    apps: Vec<App>,
    released: (String, String),
}

fn app(word: &str) -> App {
    match word.strip_prefix(READER) {
        Some(name) => App {
            name: name.to_owned(),
            reader: true,
        },
        None => App {
            name: word.to_owned(),
            reader: false,
        },
    }
}

/// A reader endpoint fits one plan token.
fn endpoint_valid(endpoint: &str) -> bool {
    text(endpoint)
        && endpoint.len() <= 2048
        && endpoint != NO_ENDPOINT
        && !endpoint.chars().any(char::is_whitespace)
}

// ---------------------------------------------------------------- errors

fn unknown() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "cloud_outcome_unknown",
        "the cloud action's outcome is unknown; resume the operation to reconcile it",
    )
}

fn ownership_conflict() -> ApiError {
    conflict(
        "resource_ownership_conflict",
        "the resource exists but this operation did not create it; it is never adopted",
    )
}

fn failed() -> ApiError {
    conflict(
        "cloud_action_failed",
        "the cloud refused or failed the action",
    )
}

fn failure(code: &str) -> String {
    let code: String = code
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || "._-".contains(*c))
        .take(128)
        .collect();
    format!(
        "failed:{}",
        if code.is_empty() {
            "unspecified"
        } else {
            &code
        }
    )
}

// ---------------------------------------------------------------- driver

/// The journaled effects of one operation by one actuator attempt.
pub(super) struct Driver<'a> {
    ops: &'a Operations,
    arm: &'a dyn Arm,
    intent: &'a Intent,
    attempt: String,
    polls: usize,
}

impl<'a> Driver<'a> {
    /// Claims the operation for `attempt` before anything can reach the
    /// port: taking over another attempt needs its `ActuatorStopped` proof.
    pub(super) fn new(
        ops: &'a Operations,
        arm: &'a dyn Arm,
        intent: &'a Intent,
        attempt: &str,
        prior: Option<&ActuatorStopped>,
        polls: usize,
    ) -> Result<Self, ApiError> {
        ops.claim(&intent.operation_id, attempt, prior)?;
        Ok(Self {
            ops,
            arm,
            intent,
            attempt: attempt.to_owned(),
            polls,
        })
    }

    fn op(&self) -> &str {
        &self.intent.operation_id
    }

    fn owner(&self) -> String {
        format!("{OWNER_TAG}:{}", self.intent.tenant_id)
    }

    fn read(&self, resource: &str) -> Result<Option<Observed>, ApiError> {
        self.arm.read(resource).map_err(|_| unknown())
    }

    fn action(&self, resource: &str, operation: &str, detail: &str, destructive: bool) -> Action {
        Action {
            resource: resource.to_owned(),
            operation: operation.to_owned(),
            request_digest: sha256_hex(
                format!("{}\0{resource}\0{operation}\0{detail}", self.owner()).as_bytes(),
            ),
            destructive,
        }
    }

    fn settle(&self, sequence: u64, observed: &str) -> Result<(), ApiError> {
        self.ops
            .settle(self.op(), &self.attempt, sequence, observed)
    }

    /// The latest step for `action` in the journal.
    fn step(&self, action: &Action) -> Result<Option<Step>, ApiError> {
        Ok(self
            .ops
            .journal(self.op())?
            .steps
            .into_iter()
            .rev()
            .find(|step| step.action == *action))
    }

    /// Polls the persisted cloud operation. Only a terminal poll result is
    /// [`Outcome::Succeeded`] or [`Outcome::Failed`]; no ID, a running or
    /// unknown operation after the budget, or a refused poll stay
    /// [`Outcome::Open`]: the operation may still act.
    fn wait(&self, arm_operation: Option<&str>) -> Outcome {
        let Some(operation) = arm_operation else {
            return Outcome::Open;
        };
        for _ in 0..self.polls {
            match self.arm.poll(operation) {
                Ok(Poll::Succeeded) => return Outcome::Succeeded,
                Ok(Poll::Failed(code)) => return Outcome::Failed(code),
                Ok(Poll::Running) | Err(ArmError::Unknown | ArmError::Refused(_)) => {}
            }
        }
        Outcome::Open
    }

    /// Records `action` and dispatches it once, persisting the returned
    /// cloud operation ID; a resume polls that ID instead. Returns the step
    /// and the operation's outcome, for the caller's readback.
    fn dispatch(&self, action: &Action, call: Call) -> Result<Dispatched, ApiError> {
        match self.ops.record(self.op(), &self.attempt, action)? {
            Decision::Settled(_) => Ok(Dispatched::Settled),
            Decision::Dispatch(sequence) => match self.arm.begin(&call) {
                Ok(operation) => {
                    self.ops
                        .observe(self.op(), &self.attempt, sequence, &operation)?;
                    Ok(Dispatched::Pending(sequence, self.wait(Some(&operation))))
                }
                // The port guarantees a refused call took no effect, so
                // nothing at the name can be this dispatch's doing.
                Err(ArmError::Refused(code)) => Ok(Dispatched::Refused(sequence, code)),
                Err(ArmError::Unknown) => Ok(Dispatched::Pending(sequence, Outcome::Open)),
            },
            Decision::Reconcile(sequence) => {
                let step = self.step(action)?.ok_or_else(state_invalid)?;
                Ok(Dispatched::Pending(
                    sequence,
                    self.wait(step.arm_operation.as_deref()),
                ))
            }
        }
    }

    // ------------------------------------------------- ITEM-009

    /// Creates the operation-owned resources and verifies the pre-existing
    /// ones, in profile order, while the operation holds its reservation
    /// and no cancellation was requested.
    pub(super) fn provision(
        &self,
        objects: &Objects,
        preflighted: &Preflighted,
    ) -> Result<(), ApiError> {
        if self.intent.kind() != Kind::Create {
            return Err(invalid("only a create operation provisions resources"));
        }
        for desired in &preflighted.profile.resources {
            let (snapshot, _) = objects.read()?;
            match holds(&snapshot, self.intent) {
                Some(Slot::Reserved {
                    cancellation_requested: false,
                    ..
                }) => {}
                _ => {
                    return Err(conflict(
                        "operation_not_provisionable",
                        "the operation does not hold an uncancelled reservation",
                    ));
                }
            }
            match desired.ownership {
                Ownership::Verified => self.verify(desired)?,
                Ownership::Owned => self.create(desired)?,
            }
        }
        Ok(())
    }

    fn verify(&self, desired: &Desired) -> Result<(), ApiError> {
        let action = self.action(&desired.resource, VERIFY, &desired.config_digest, false);
        if let Some(Step {
            outcome: StepOutcome::Settled { .. },
            ..
        }) = self.step(&action)?
        {
            return Ok(());
        }
        let found = self.read(&desired.resource)?.ok_or_else(|| {
            conflict(
                "prerequisite_missing",
                "a prerequisite the profile names as pre-existing does not exist",
            )
        })?;
        if found
            .owner
            .as_deref()
            .is_some_and(|owner| owner.starts_with(OWNER_TAG) && owner != self.owner())
        {
            return Err(ownership_conflict());
        }
        if found.config_digest != desired.config_digest || !found.succeeded {
            return Err(conflict(
                "prerequisite_mismatch",
                "a pre-existing prerequisite does not match the profile",
            ));
        }
        match self.ops.record(self.op(), &self.attempt, &action)? {
            Decision::Dispatch(n) | Decision::Reconcile(n) => self.settle(n, "verified"),
            Decision::Settled(_) => Ok(()),
        }
    }

    fn create(&self, desired: &Desired) -> Result<(), ApiError> {
        let operation = if desired.app { CREATE_APP } else { CREATE };
        let action = self.action(&desired.resource, operation, &desired.config_digest, false);
        match self.step(&action)? {
            Some(Step {
                outcome: StepOutcome::Settled { observed },
                ..
            }) => return created(&observed),
            Some(_) => {}
            // Never adopt: anything there before the first dispatch is not ours.
            None if self.read(&desired.resource)?.is_some() => return Err(ownership_conflict()),
            None => {}
        }
        let call = Call {
            resource: desired.resource.clone(),
            owner: self.owner(),
            verb: Verb::Put {
                config_digest: desired.config_digest.clone(),
            },
        };
        let (sequence, outcome) = match self.dispatch(&action, call)? {
            Dispatched::Settled => {
                let step = self.step(&action)?.ok_or_else(state_invalid)?;
                return match step.outcome {
                    StepOutcome::Settled { observed } => created(&observed),
                    StepOutcome::Uncertain {} => Err(unknown()),
                };
            }
            Dispatched::Refused(sequence, code) => {
                self.settle(sequence, &failure(&code))?;
                return Err(failed());
            }
            Dispatched::Pending(sequence, outcome) => (sequence, outcome),
        };
        let found = self.read(&desired.resource)?;
        match &found {
            // Someone else's resource is no proof this dispatch is over:
            // while its operation may still act, the step stays unknown.
            Some(found)
                if found.owner.as_deref() != Some(self.owner().as_str())
                    && outcome == Outcome::Open =>
            {
                Err(unknown())
            }
            Some(found) if found.owner.as_deref() != Some(self.owner().as_str()) => {
                self.settle(sequence, "conflict")?;
                Err(ownership_conflict())
            }
            Some(found) => {
                if let Revisions::Complete(revisions) = &found.revisions
                    && revisions.iter().filter(|r| r.active).count() > 1
                {
                    return Err(conflict(
                        "revision_overlap",
                        "more than one revision is active; the outcome stays unresolved",
                    ));
                }
                if desired.app && found.revisions == Revisions::Incomplete {
                    return Err(ApiError::new(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "revision_inventory_incomplete",
                        "the revision inventory is incomplete; nothing is proven",
                    ));
                }
                // Presence is proof only once this dispatch's own cloud
                // operation finished successfully.
                if outcome == Outcome::Succeeded && exact(desired, found) {
                    self.settle(sequence, "created")
                } else {
                    Err(unknown())
                }
            }
            None => match outcome {
                Outcome::Failed(code) => {
                    self.settle(sequence, &failure(&code))?;
                    Err(failed())
                }
                Outcome::Succeeded | Outcome::Open => Err(unknown()),
            },
        }
    }

    // ------------------------------------------------- ITEM-011

    /// After a cancel: deletes each app this operation created (never a
    /// verified, non-app, data or archive resource), resolving unknown
    /// creates by their owner tag first. The seal requires it
    /// ([`terminal_ready`]); the caller then calls `Operations::release`.
    pub(super) fn cleanup(&self, objects: &Objects, guards: &Guards) -> Result<(), ApiError> {
        let (snapshot, _) = objects.read()?;
        if !matches!(
            holds(&snapshot, self.intent),
            Some(Slot::Reserved {
                cancellation_requested: true,
                receipt: None,
                ..
            })
        ) {
            return Err(conflict(
                "operation_not_cancelled",
                "cleanup runs only after a cancellation was requested",
            ));
        }
        // A cancelled retirement never removed the route (the cancel and
        // the removal are mutually exclusive), so its guard is released.
        if self.intent.kind() == Kind::Retire {
            self.release_guard(guards)?;
        }
        for step in self.ops.journal(self.op())?.steps {
            if step.outcome != (StepOutcome::Uncertain {}) {
                continue;
            }
            match step.action.operation.as_str() {
                CREATE | CREATE_APP => self.resolve_create(&step)?,
                // A read with no side effect: nothing was verified.
                VERIFY => self.settle(step.sequence, "abandoned")?,
                // Reconciled by its own resumed delete below.
                DELETE_APP => {}
                // EPIC-005: an update or stop already reached an app this
                // tenant owns; it cannot be rolled back, only resolved once
                // its cloud operation is terminal.
                UPDATE_APP | STOP_APP => match self.wait(step.arm_operation.as_deref()) {
                    Outcome::Open => return Err(unknown()),
                    Outcome::Succeeded => self.settle(step.sequence, "resolved")?,
                    Outcome::Failed(code) => self.settle(step.sequence, &failure(&code))?,
                },
                // A read: nothing was proven eligible.
                ELIGIBLE => self.settle(step.sequence, "abandoned")?,
                _ => return Err(unknown()),
            }
        }
        for resource in apps(&self.ops.journal(self.op())?, CREATE_APP, &["created"]) {
            self.destructive(&resource, DELETE_APP, Verb::Delete, "deleted", |found| {
                found.is_none()
            })?;
        }
        Ok(())
    }

    /// An unknown create, resolved by the owner tag only once its recorded
    /// cloud operation is terminal (the desired configuration is not needed
    /// to know the resource is ours). While the operation may still act —
    /// running, unpollable, or never identified — it stays unknown, so
    /// nothing is deleted and the seal waits.
    fn resolve_create(&self, step: &Step) -> Result<(), ApiError> {
        let outcome = self.wait(step.arm_operation.as_deref());
        if outcome == Outcome::Open {
            return Err(unknown());
        }
        match (self.read(&step.action.resource)?, outcome) {
            (Some(found), Outcome::Succeeded)
                if found.owner.as_deref() == Some(self.owner().as_str()) =>
            {
                self.settle(step.sequence, "created")
            }
            // Ours, yet the operation failed: whether it is this
            // operation's own partial effect is unproven.
            (Some(found), _) if found.owner.as_deref() == Some(self.owner().as_str()) => {
                Err(unknown())
            }
            (Some(_), _) => self.settle(step.sequence, "conflict"),
            (None, Outcome::Failed(code)) => self.settle(step.sequence, &failure(&code)),
            (None, _) => Err(unknown()),
        }
    }

    /// One destructive action on an owned resource, settled as `done` only
    /// when its cloud operation succeeded (or the port refused it with no
    /// effect) and the readback satisfies `proven`: an apparently finished
    /// effect is no proof while the operation may still act, and a failed
    /// operation is never success. An absent resource needs no action; one
    /// owned by anyone else is never touched.
    fn destructive(
        &self,
        resource: &str,
        operation: &str,
        verb: Verb,
        done: &str,
        proven: impl Fn(Option<&Observed>) -> bool,
    ) -> Result<(), ApiError> {
        let action = self.action(resource, operation, "", true);
        match self.step(&action)? {
            Some(Step {
                outcome: StepOutcome::Settled { observed },
                ..
            }) => {
                return if observed == done || observed == "absent" {
                    Ok(())
                } else {
                    Err(failed())
                };
            }
            Some(_) => {}
            None => match self.read(resource)? {
                // Only a newly recorded step proves no executor dispatched
                // this action before the absence was read. A reconciled
                // step was recorded (and may be running) meanwhile: it
                // needs its own terminal poll below.
                None => match self.ops.record(self.op(), &self.attempt, &action)? {
                    Decision::Dispatch(n) => return self.settle(n, "absent"),
                    Decision::Reconcile(_) | Decision::Settled(_) => {}
                },
                Some(found) if found.owner.as_deref() != Some(self.owner().as_str()) => {
                    return Err(ownership_conflict());
                }
                Some(_) => {}
            },
        }
        let call = Call {
            resource: resource.to_owned(),
            owner: self.owner(),
            verb,
        };
        let (sequence, outcome, refused) = match self.dispatch(&action, call)? {
            // Settled by another executor in between: its outcome stands.
            Dispatched::Settled => {
                let step = self.step(&action)?.ok_or_else(state_invalid)?;
                return match step.outcome {
                    StepOutcome::Settled { observed }
                        if observed == done || observed == "absent" =>
                    {
                        Ok(())
                    }
                    StepOutcome::Settled { .. } => Err(failed()),
                    StepOutcome::Uncertain {} => Err(unknown()),
                };
            }
            Dispatched::Refused(sequence, code) => (sequence, Outcome::Failed(code), true),
            Dispatched::Pending(sequence, outcome) => (sequence, outcome, false),
        };
        let found = self.read(resource)?;
        if found
            .as_ref()
            .is_some_and(|found| found.owner.as_deref() != Some(self.owner().as_str()))
        {
            return Err(ownership_conflict());
        }
        if (refused || outcome == Outcome::Succeeded) && proven(found.as_ref()) {
            return self.settle(sequence, done);
        }
        match outcome {
            Outcome::Failed(code) => {
                self.settle(sequence, &failure(&code))?;
                Err(failed())
            }
            Outcome::Succeeded | Outcome::Open => Err(unknown()),
        }
    }

    /// ITEM-011: verifies the retirement against the tenant's sealed
    /// activation, records the plan (the receipt's route and the apps the
    /// create operation created), then reserves the slot. Every mismatch
    /// refuses `retirement_receipt_mismatch`.
    pub(super) fn reserve_retirement(&self, objects: &Objects, now: u64) -> Result<(), ApiError> {
        let Payload::Retire {
            expected_receipt, ..
        } = &self.intent.payload
        else {
            return Err(invalid("not a retirement"));
        };
        let action = Action {
            resource: PLAN.to_owned(),
            operation: VERIFY.to_owned(),
            request_digest: expected_receipt.clone(),
            destructive: false,
        };
        if !matches!(
            self.step(&action)?,
            Some(Step {
                outcome: StepOutcome::Settled { .. },
                ..
            })
        ) {
            let plan = self.verify_receipt(objects, expected_receipt)?;
            match self.ops.record(self.op(), &self.attempt, &action)? {
                Decision::Dispatch(n) | Decision::Reconcile(n) => self.settle(n, &plan)?,
                Decision::Settled(_) => {}
            }
        }
        self.reserve_planned(objects, retirement_mismatch, now)
    }

    fn verify_receipt(&self, objects: &Objects, expected: &str) -> Result<String, ApiError> {
        let mismatch = retirement_mismatch;
        let anchor = self.anchor(objects, mismatch)?;
        if super::admin::receipt_digest(&anchor.receipt) != expected {
            return Err(mismatch());
        }
        // Every app the tenant owns, the reader's included, is stopped.
        Ok([anchor.operation_id, anchor.receipt.route_digest]
            .into_iter()
            .chain(anchor.apps.into_iter().map(|app| app.name))
            .collect::<Vec<_>>()
            .join(" "))
    }

    /// The tenant's latest sealed receipt: the slot's released operation
    /// when it succeeded, or, after a cancelled retirement or reader change
    /// (neither changed the route), exactly one hop back through its
    /// verified plan to the receipt it verified. The published route must
    /// be that receipt's. Also returns the apps the tenant owns: those the
    /// anchor's own plan names plus the readers it created, or, for the
    /// create, the apps it created. Anything else refuses `mismatch`.
    fn anchor(&self, objects: &Objects, mismatch: fn() -> ApiError) -> Result<Anchor, ApiError> {
        let (snapshot, _) = objects.read()?;
        let Some(Slot::Released {
            operation_id,
            terminal_digest,
        }) = snapshot.lifecycle(&self.intent.tenant_id).map(|e| &e.slot)
        else {
            return Err(mismatch());
        };
        let Some((terminal, digest)) = self.ops.read_terminal(operation_id)? else {
            return Err(mismatch());
        };
        if digest != *terminal_digest || terminal.tenant_id != self.intent.tenant_id {
            return Err(mismatch());
        }
        let (anchor, terminal) = match &terminal.outcome {
            TerminalOutcome::Succeeded { .. } => (operation_id.clone(), terminal),
            TerminalOutcome::Cancelled => {
                let journal = self.ops.journal(operation_id)?;
                let mut words = plan(&journal).ok_or_else(mismatch)?.split(' ');
                let anchor = words.next().ok_or_else(mismatch)?.to_owned();
                let planned_route = words.next().ok_or_else(mismatch)?.to_owned();
                let Some((terminal, _)) = self.ops.read_terminal(&anchor)? else {
                    return Err(mismatch());
                };
                match &terminal.outcome {
                    TerminalOutcome::Succeeded { receipt }
                        if receipt.operation_id == anchor
                            && receipt.route_digest == planned_route => {}
                    _ => return Err(mismatch()),
                }
                (anchor, terminal)
            }
            TerminalOutcome::Retired => return Err(mismatch()),
        };
        let TerminalOutcome::Succeeded { receipt } = terminal.outcome else {
            return Err(mismatch());
        };
        let route = snapshot.record_for(&self.intent.tenant_id).cloned();
        if terminal.tenant_id != self.intent.tenant_id
            || route.as_ref().map(control_state::route_digest).as_deref()
                != Some(receipt.route_digest.as_str())
        {
            return Err(mismatch());
        }
        let journal = self.ops.journal(&anchor)?;
        let created = apps(&journal, CREATE_APP, &["created"]);
        let mut owned: Vec<App> = match plan(&journal).and_then(ReaderPlan::parse) {
            Some(plan) => plan
                .apps
                .into_iter()
                .chain(created.into_iter().map(|name| App { name, reader: true }))
                .collect(),
            None => created
                .into_iter()
                .map(|name| App {
                    name,
                    reader: false,
                })
                .collect(),
        };
        let mut seen = std::collections::BTreeSet::new();
        owned.retain(|app| seen.insert(app.name.clone()));
        Ok(Anchor {
            operation_id: anchor,
            receipt,
            route: route.ok_or_else(mismatch)?,
            apps: owned,
            released: (operation_id.clone(), terminal_digest.clone()),
        })
    }

    /// Reserves the slot for this operation's settled plan (its first two
    /// words: the anchoring operation and its route). The plan must still
    /// be the tenant's anchor, and the reservation CAS itself requires the
    /// slot still released by the operation this check read and the route
    /// unchanged, so a stale plan (another change sealed or cancelled since,
    /// even with the same route) takes no slot: `mismatch`. A same-operation
    /// retry that already holds the slot succeeds only when the slot was
    /// reserved against this plan's route; a slot this operation holds
    /// without that binding (state persisted by an earlier build's planless
    /// admin reservation) is refused, never adopted. Such a slot taken at
    /// the same route after a newer receipt is indistinguishable, since a
    /// reserved slot records no predecessor (documented limitation).
    fn reserve_planned(
        &self,
        objects: &Objects,
        mismatch: fn() -> ApiError,
        now: u64,
    ) -> Result<(), ApiError> {
        if self.ops.read_terminal(self.op())?.is_some() {
            return Err(ended());
        }
        let journal = self.ops.journal(self.op())?;
        let mut words = plan(&journal).ok_or_else(mismatch)?.split(' ');
        let (Some(planned), Some(route)) = (words.next(), words.next()) else {
            return Err(mismatch());
        };
        let (snapshot, _) = objects.read()?;
        if let Some(slot) = holds(&snapshot, self.intent) {
            return match slot {
                Slot::Reserved {
                    expected_route: Some(held),
                    ..
                } if held == route => Ok(()),
                _ => Err(mismatch()),
            };
        }
        let anchor = self.anchor(objects, mismatch)?;
        if anchor.operation_id != planned || anchor.receipt.route_digest != route {
            return Err(mismatch());
        }
        let (op, digest) = &anchor.released;
        reserve_after(self.ops, objects, self.intent, (op, digest), route, now).map_err(|error| {
            if error.code == "lifecycle_anchor_changed" {
                mismatch()
            } else {
                error
            }
        })
    }

    /// ITEM-011: holds the target's lifecycle guard (no writer claim or
    /// failover from here on; it stays held), removes exactly the verified
    /// route, stops each created app with a readback proof that no revision
    /// is active, then seals `retired` and releases the slot. Resumable at
    /// every boundary; an unknown stop prevents completion.
    pub(super) fn retire(
        &self,
        objects: &Objects,
        guards: &Guards,
        now: u64,
    ) -> Result<Sealed, ApiError> {
        if self.ops.terminal(self.op())?.is_some() {
            return self.ops.release(objects, self.intent, now);
        }
        let journal = self.ops.journal(self.op())?;
        let plan = plan(&journal).ok_or_else(|| {
            conflict(
                "retirement_unverified",
                "the retirement was not verified and reserved; reserve it first",
            )
        })?;
        let mut words = plan.split(' ').skip(1);
        let route = words.next().ok_or_else(state_invalid)?.to_owned();
        let planned: Vec<String> = words.map(str::to_owned).collect();
        if self.cancelled_before_removal(objects)? {
            self.release_guard(guards)?;
            return Err(operation_cancelled());
        }
        self.hold_guard(guards)?;
        let removed = lifecycle_cas(
            objects,
            now,
            |snapshot| {
                snapshot.record_for(&self.intent.tenant_id).is_none()
                    && holds(snapshot, self.intent).is_some()
            },
            |current| current.retire_route(&self.intent.tenant_id, self.op(), &route),
        );
        if let Err(error) = removed {
            // The cancellation won the race after the hold: nothing was
            // removed, so this operation's guard must not stay held.
            if self.cancelled_before_removal(objects)? {
                self.release_guard(guards)?;
            }
            return Err(error);
        }
        for app in &planned {
            self.destructive(app, STOP_APP, Verb::Stop, "stopped", |found| {
                found.is_none_or(|found| {
                    matches!(&found.revisions, Revisions::Complete(revisions)
                        if revisions.iter().all(|r| !r.active))
                })
            })?;
        }
        self.ops.release(objects, self.intent, now)
    }

    /// A cancellation was requested and the route is still there (a
    /// removal and a cancel are mutually exclusive in the control state).
    fn cancelled_before_removal(&self, objects: &Objects) -> Result<bool, ApiError> {
        let (snapshot, _) = objects.read()?;
        Ok(matches!(
            holds(&snapshot, self.intent),
            Some(Slot::Reserved {
                cancellation_requested: true,
                ..
            })
        ) && snapshot.record_for(&self.intent.tenant_id).is_some())
    }

    fn guard_action(&self, operation: &str) -> Action {
        self.action(GUARD, operation, &self.intent.tenant_id, false)
    }

    /// Holds the lifecycle guard, journaled first so that a cancel always
    /// knows to reconcile it. Before any guard write, the hold step durably
    /// binds (first writer wins, [`Operations::observe`]) the one guard
    /// version it may replace: absence or a released guard. Every executor
    /// and retry reuses that binding and never refreshes it, so a hold
    /// paused across a cancellation cannot acquire a successor's version;
    /// a cancellation that finds the hold unbound binds [`GUARD_FENCED`]
    /// instead, so that no guard write can ever follow.
    fn hold_guard(&self, guards: &Guards) -> Result<(), ApiError> {
        let action = self.guard_action(HOLD);
        let sequence = match self.step(&action)? {
            Some(Step {
                outcome: StepOutcome::Settled { observed },
                ..
            }) => return held(&observed),
            _ => match self.ops.record(self.op(), &self.attempt, &action)? {
                Decision::Dispatch(n) | Decision::Reconcile(n) => n,
                Decision::Settled(_) => {
                    return match self.step(&action)?.map(|step| step.outcome) {
                        Some(StepOutcome::Settled { observed }) => held(&observed),
                        _ => Err(state_invalid()),
                    };
                }
            },
        };
        let bound = match self.step(&action)?.and_then(|step| step.arm_operation) {
            Some(bound) => bound,
            None => {
                let bound = match guards.version()? {
                    None => GUARD_ABSENT.to_owned(),
                    Some((current, _))
                        if current.state != guard::State::Released
                            && current.operation_id != self.op() =>
                    {
                        return Err(conflict(
                            "lifecycle_guard_held",
                            "another operation holds this target's lifecycle guard",
                        ));
                    }
                    Some((_, etag)) => format!("{GUARD_VERSION}{etag}"),
                };
                // A different binding (another executor's, or the fence)
                // refuses `step_conflict`; a retry then reads it.
                self.ops
                    .observe(self.op(), &self.attempt, sequence, &bound)?;
                bound
            }
        };
        let version = match bound.as_str() {
            GUARD_FENCED => return Err(operation_cancelled()),
            GUARD_ABSENT => None,
            bound => Some(
                bound
                    .strip_prefix(GUARD_VERSION)
                    .ok_or_else(state_invalid)?,
            ),
        };
        guards.hold_bound(&self.intent.tenant_id, self.op(), version)?;
        self.settle(sequence, "held")
    }

    /// After a cancellation won: fences this operation's guard and journals
    /// the release the seal requires ([`terminal_ready`]). An unbound hold
    /// is bound to [`GUARD_FENCED`] first, so no executor can write it any
    /// more; a bound one is fenced over its bound version
    /// ([`Guards::fence`]), so a hold whose outcome is still unknown can no
    /// longer land afterwards. Until that is proven the hold stays
    /// unresolved and the seal is refused. Idempotent; a no-op without a
    /// journaled hold.
    fn release_guard(&self, guards: &Guards) -> Result<(), ApiError> {
        let action = self.guard_action(HOLD);
        let Some(mut hold) = self.step(&action)? else {
            return Ok(());
        };
        if hold.arm_operation.is_none() {
            self.ops
                .observe(self.op(), &self.attempt, hold.sequence, GUARD_FENCED)?;
            hold = self.step(&action)?.ok_or_else(state_invalid)?;
        }
        let tenant = &self.intent.tenant_id;
        match hold.arm_operation.as_deref().ok_or_else(state_invalid)? {
            GUARD_FENCED => {}
            GUARD_ABSENT => guards.fence(tenant, self.op(), None)?,
            bound => {
                let version = bound
                    .strip_prefix(GUARD_VERSION)
                    .ok_or_else(state_invalid)?;
                guards.fence(tenant, self.op(), Some(version))?;
            }
        }
        if hold.outcome == (StepOutcome::Uncertain {}) {
            self.settle(hold.sequence, "released")?;
        }
        let action = self.guard_action(RELEASE);
        if let Some(Step {
            outcome: StepOutcome::Settled { .. },
            ..
        }) = self.step(&action)?
        {
            return Ok(());
        }
        match self.ops.record(self.op(), &self.attempt, &action)? {
            Decision::Dispatch(n) | Decision::Reconcile(n) => self.settle(n, "released"),
            Decision::Settled(_) => Ok(()),
        }
    }
}

// ------------------------------------------------- EPIC-005 ITEM-012/013

impl Driver<'_> {
    /// ITEM-012/013: verifies a reader change against the tenant's latest
    /// sealed receipt and plans it (the anchor, its route, the route this
    /// change publishes, the endpoint and every app the tenant owns), then
    /// reserves the slot. Reader routing, a moved endpoint and a route no
    /// sealed lifecycle operation published are refused before any record.
    pub(super) fn reserve_readers(
        &self,
        objects: &Objects,
        endpoint: Option<&str>,
        now: u64,
    ) -> Result<(), ApiError> {
        let Payload::SetReaders { fleet, .. } = &self.intent.payload else {
            return Err(invalid("not a reader change"));
        };
        if self.ops.terminal(self.op())?.is_some() {
            return Err(ended());
        }
        match (fleet, endpoint) {
            (Fleet::Present { read_routing, .. }, _)
                if *read_routing != control_state::ReadRouting::WriterOnly =>
            {
                return Err(routing_unqualified());
            }
            (Fleet::Present { .. }, Some(endpoint)) if endpoint_valid(endpoint) => {}
            (Fleet::Absent {}, None) => {}
            _ => {
                return Err(invalid(
                    "a reader fleet needs exactly one valid endpoint; removing it takes none",
                ));
            }
        }
        let action = Action {
            resource: PLAN.to_owned(),
            operation: VERIFY.to_owned(),
            request_digest: self.intent.request_digest.clone(),
            destructive: false,
        };
        if !matches!(
            self.step(&action)?,
            Some(Step {
                outcome: StepOutcome::Settled { .. },
                ..
            })
        ) {
            let anchor = self.anchor(objects, readers_unanchored)?;
            if let (Some(endpoint), Some(current)) = (endpoint, anchor.route.reader.as_deref())
                && endpoint != current
            {
                return Err(conflict(
                    "reader_endpoint_changed",
                    "the reader endpoint differs from the published one; remove the readers first",
                ));
            }
            let mut target = anchor.route.clone();
            target.reader = endpoint.map(str::to_owned);
            let plan = [
                anchor.operation_id,
                anchor.receipt.route_digest,
                format!("{TARGET}{}", control_state::route_digest(&target)),
                format!("{ENDPOINT}{}", endpoint.unwrap_or(NO_ENDPOINT)),
            ]
            .into_iter()
            .chain(anchor.apps.into_iter().map(|app| {
                if app.reader {
                    format!("{READER}{}", app.name)
                } else {
                    app.name
                }
            }))
            .collect::<Vec<_>>()
            .join(" ");
            match self.ops.record(self.op(), &self.attempt, &action)? {
                Decision::Dispatch(n) | Decision::Reconcile(n) => self.settle(n, &plan)?,
                Decision::Settled(_) => {}
            }
        }
        self.reserve_planned(objects, readers_unanchored, now)
    }

    /// ITEM-012/013: applies the reserved reader change, then seals and
    /// releases it. Present: creates (or, when the tenant already owns one,
    /// updates in place) exactly one private reader app, proves the
    /// observed fleet eligible, and only then publishes the endpoint with
    /// the receipt. Absent: unpublishes the endpoint first (from here on the
    /// change cannot be cancelled), proves every reader app stopped, then
    /// publishes the receipt. Every proof is required on every call that
    /// may need it: none is a default. A same-attempt executor that sealed
    /// the change meanwhile returns its outcome.
    pub(super) fn set_readers(
        &self,
        objects: &Objects,
        profile: Option<&Preflighted>,
        scope: Option<&PinScope>,
        observed: Option<&FleetObservation>,
        now: u64,
    ) -> Result<Sealed, ApiError> {
        match self.apply_readers(objects, profile, scope, observed, now) {
            Ok(()) => self.ops.release(objects, self.intent, now),
            Err(error) => match self.ops.terminal(self.op())? {
                Some(_) => self.ops.release(objects, self.intent, now),
                None => Err(error),
            },
        }
    }

    fn apply_readers(
        &self,
        objects: &Objects,
        profile: Option<&Preflighted>,
        scope: Option<&PinScope>,
        observed: Option<&FleetObservation>,
        now: u64,
    ) -> Result<(), ApiError> {
        let Payload::SetReaders { fleet, .. } = &self.intent.payload else {
            return Err(invalid("not a reader change"));
        };
        if self.ops.terminal(self.op())?.is_some() {
            return Ok(());
        }
        let (snapshot, _) = objects.read()?;
        if matches!(
            holds(&snapshot, self.intent),
            Some(Slot::Reserved {
                receipt: Some(_),
                ..
            })
        ) {
            return Ok(());
        }
        let journal = self.ops.journal(self.op())?;
        let plan = plan(&journal).and_then(ReaderPlan::parse).ok_or_else(|| {
            conflict(
                "readers_unverified",
                "the reader change was not verified and reserved; reserve it first",
            )
        })?;
        let proof = sha256_hex(format!("readers\0{}\0{}", self.op(), plan.target).as_bytes());
        let publish = |reader: Option<&str>, proof: Option<&str>| {
            publish_readers(
                self.ops,
                objects,
                self.intent,
                &plan.planned,
                &plan.target,
                reader,
                proof,
                now,
            )
        };
        match fleet {
            Fleet::Present {
                read_routing,
                min_replicas,
                max_replicas,
                ..
            } => {
                if *read_routing != control_state::ReadRouting::WriterOnly {
                    return Err(routing_unqualified());
                }
                if scope.is_none() {
                    return Err(pin_scope_unproven());
                }
                let profile = profile.ok_or_else(|| {
                    conflict(
                        "profile_unsupported",
                        "no supported, qualified reader profile was preflighted (OD-006)",
                    )
                })?;
                let mut owned = profile
                    .profile
                    .resources
                    .iter()
                    .filter(|d| d.ownership == Ownership::Owned);
                let reader = match (owned.next(), owned.next()) {
                    (Some(reader), None) if reader.app => reader,
                    _ => {
                        return Err(ApiError::new(
                            StatusCode::BAD_REQUEST,
                            "reader_profile_invalid",
                            "a reader profile owns exactly one app",
                        ));
                    }
                };
                let mismatch = || {
                    conflict(
                        "reader_app_mismatch",
                        "the profile's reader app is not the tenant's reader app",
                    )
                };
                if plan
                    .apps
                    .iter()
                    .any(|app| !app.reader && app.name == reader.resource)
                {
                    return Err(mismatch());
                }
                self.holding(objects)?;
                for desired in &profile.profile.resources {
                    if desired.ownership == Ownership::Verified {
                        self.verify(desired)?;
                    }
                }
                let mut existing = plan.readers();
                match existing.next() {
                    Some(name) if name == reader.resource && existing.next().is_none() => {
                        self.update(reader)?;
                    }
                    Some(_) => return Err(mismatch()),
                    None => self.create(reader)?,
                }
                self.holding(objects)?;
                self.eligible(
                    objects,
                    reader,
                    &plan.target,
                    (*min_replicas, *max_replicas),
                    observed,
                )?;
                publish(plan.endpoint.as_deref(), Some(&proof))
            }
            Fleet::Absent {} => {
                // An endpoint leaves the route before anything stops.
                self.holding(objects)?;
                publish(None, None)?;
                for app in plan.readers() {
                    self.removing(objects)?;
                    self.destructive(app, STOP_APP, Verb::Stop, "stopped", |found| {
                        found.is_none_or(|found| {
                            matches!(&found.revisions, Revisions::Complete(revisions)
                                if revisions.iter().all(|r| !r.active))
                        })
                    })?;
                }
                publish(None, Some(&proof))
            }
        }
    }

    /// The operation still holds its uncancelled reservation, no receipt.
    fn holding(&self, objects: &Objects) -> Result<(), ApiError> {
        let (snapshot, _) = objects.read()?;
        match holds(&snapshot, self.intent) {
            Some(Slot::Reserved {
                cancellation_requested: false,
                receipt: None,
                ..
            }) => Ok(()),
            _ => Err(readers_not_held()),
        }
    }

    /// A removal past its unpublish: the reservation is still held (a
    /// cancel is refused once the route changed).
    fn removing(&self, objects: &Objects) -> Result<(), ApiError> {
        let (snapshot, _) = objects.read()?;
        match holds(&snapshot, self.intent) {
            Some(Slot::Reserved { receipt: None, .. }) => Ok(()),
            _ => Err(readers_not_held()),
        }
    }

    /// Updates the tenant's existing reader app in place: never one it does
    /// not own, settled only from this dispatch's successful operation and
    /// an exact readback.
    fn update(&self, desired: &Desired) -> Result<(), ApiError> {
        let action = self.action(&desired.resource, UPDATE_APP, &desired.config_digest, false);
        let updated = |observed: &str| {
            if observed == "updated" {
                Ok(())
            } else {
                Err(failed())
            }
        };
        match self.step(&action)? {
            Some(Step {
                outcome: StepOutcome::Settled { observed },
                ..
            }) => return updated(&observed),
            Some(_) => {}
            None => match self.read(&desired.resource)? {
                None => {
                    return Err(conflict(
                        "reader_app_missing",
                        "the tenant's reader app no longer exists",
                    ));
                }
                Some(found) if found.owner.as_deref() != Some(self.owner().as_str()) => {
                    return Err(ownership_conflict());
                }
                Some(_) => {}
            },
        }
        let call = Call {
            resource: desired.resource.clone(),
            owner: self.owner(),
            verb: Verb::Update {
                config_digest: desired.config_digest.clone(),
            },
        };
        let (sequence, outcome) = match self.dispatch(&action, call)? {
            Dispatched::Settled => {
                return match self.step(&action)?.map(|step| step.outcome) {
                    Some(StepOutcome::Settled { observed }) => updated(&observed),
                    _ => Err(unknown()),
                };
            }
            Dispatched::Refused(sequence, code) => {
                self.settle(sequence, &failure(&code))?;
                return Err(failed());
            }
            Dispatched::Pending(sequence, outcome) => (sequence, outcome),
        };
        match outcome {
            Outcome::Succeeded => match self.read(&desired.resource)? {
                Some(found)
                    if found.owner.as_deref() == Some(self.owner().as_str())
                        && exact(desired, &found) =>
                {
                    self.settle(sequence, "updated")
                }
                _ => Err(unknown()),
            },
            Outcome::Failed(code) => {
                self.settle(sequence, &failure(&code))?;
                Err(failed())
            }
            Outcome::Open => Err(unknown()),
        }
    }

    /// The observed fleet may serve the published route: the writer
    /// authority serves a head (no base transition), at the route's lineage
    /// and epoch; the tenant's reader app is ours and runs `desired` in a
    /// single active revision; every replica is unique, runs in that app and
    /// revision, and serves exactly that head at the route's lineage,
    /// binding and epoch within its staleness bound; and their number is
    /// within the desired bounds (a scale-in is verified only once the fleet
    /// is reduced). Proven afresh on every call, even after a retry's
    /// earlier proof was journaled: an endpoint is published only on a
    /// current observation.
    fn eligible(
        &self,
        objects: &Objects,
        desired: &Desired,
        target: &str,
        (min, max): (u32, u32),
        observed: Option<&FleetObservation>,
    ) -> Result<(), ApiError> {
        let action = self.action(
            &format!("fleet/{}", desired.resource),
            ELIGIBLE,
            target,
            false,
        );
        let observed = observed.ok_or_else(|| {
            conflict(
                "fleet_observation_unavailable",
                "no observation of the reader fleet and writer authority is available",
            )
        })?;
        let revision = match self.read(&desired.resource)? {
            Some(found)
                if found.owner.as_deref() == Some(self.owner().as_str())
                    && exact(desired, &found) =>
            {
                match found.revisions {
                    Revisions::Complete(revisions) => revisions
                        .into_iter()
                        .find(|r| r.active)
                        .map(|r| r.name)
                        .ok_or_else(unknown)?,
                    _ => return Err(unknown()),
                }
            }
            _ => {
                return Err(conflict(
                    "reader_revision_unproven",
                    "the tenant's reader app does not run its desired configuration in one active revision",
                ));
            }
        };
        let (snapshot, _) = objects.read()?;
        let route = snapshot
            .record_for(&self.intent.tenant_id)
            .ok_or_else(state_invalid)?;
        let authority = &observed.authority;
        let head = match (authority.validate(), &authority.head) {
            (Ok(()), Some(head)) if authority.phase == authority::Phase::Serving => head,
            _ => {
                return Err(conflict(
                    "reader_base_transition",
                    "the writer authority is not serving a verified head",
                ));
            }
        };
        let ineligible = || {
            conflict(
                "replica_ineligible",
                "a reader replica is not bound to the writer's head, lineage, binding and epoch within its staleness bound",
            )
        };
        if authority.lineage != format!("{:016x}", route.lineage)
            || Some(authority.epoch) != route.epoch
        {
            return Err(ineligible());
        }
        let replicas = &observed.replicas;
        let mut names = std::collections::BTreeSet::new();
        if !replicas.iter().all(|r| names.insert(r.replica.as_str())) {
            return Err(conflict(
                "replica_identity_conflict",
                "two observed replicas share an identity",
            ));
        }
        if replicas.len() > max as usize {
            return Err(conflict(
                "reader_fleet_not_reduced",
                "more replicas run than the desired maximum; the scale-in is not verified",
            ));
        }
        if replicas.len() < min as usize {
            return Err(conflict(
                "reader_fleet_below_min",
                "fewer eligible replicas run than the desired minimum",
            ));
        }
        if replicas.iter().any(|r| {
            r.app != desired.resource
                || r.revision != revision
                || r.set_id != head.set_id
                || r.lineage != route.lineage
                || r.binding_generation != route.binding_generation
                || Some(r.epoch) != route.epoch
                || r.failed
                || r.bound_ms == 0
                || r.age_ms > r.bound_ms
        }) {
            return Err(ineligible());
        }
        if self
            .step(&action)?
            .is_some_and(|step| matches!(step.outcome, StepOutcome::Settled { .. }))
        {
            return Ok(());
        }
        match self.ops.record(self.op(), &self.attempt, &action)? {
            Decision::Dispatch(n) | Decision::Reconcile(n) => self.settle(n, "eligible"),
            Decision::Settled(_) => Ok(()),
        }
    }

    /// ITEM-013: control-owned recovery of the reader-apply pins of one
    /// replica proven stopped: only its own operation pins created no later
    /// than the stop, never an operator pin or another replica's. Each
    /// release is journaled before it is made and is idempotent. Needs the
    /// held reader reservation and the pin-scope proof (ITEM-008).
    pub(super) fn recover_pins(
        &self,
        objects: &Objects,
        sets: &Sets,
        scope: Option<&PinScope>,
        stopped: Option<&ReplicaStopped>,
        now: u64,
    ) -> Result<usize, ApiError> {
        let (snapshot, _) = objects.read()?;
        if self.intent.kind() != Kind::SetReaders
            || !matches!(
                holds(&snapshot, self.intent),
                Some(Slot::Reserved { receipt: None, .. })
            )
        {
            return Err(conflict(
                "operation_not_reserved",
                "pin recovery runs only within a held reader reservation",
            ));
        }
        if scope.is_none() {
            return Err(pin_scope_unproven());
        }
        let stopped = stopped.ok_or_else(|| {
            conflict(
                "replica_stop_unproven",
                "no platform proof that the replica stopped",
            )
        })?;
        // The proof must name this tenant and one of the reader apps it
        // owned when the change was planned. Pins themselves carry only the
        // replica name (ITEM-008), so this binds the proof, not the pins.
        let journal = self.ops.journal(self.op())?;
        let owned = plan(&journal)
            .and_then(ReaderPlan::parse)
            .is_some_and(|plan| plan.readers().any(|app| app == stopped.app));
        if stopped.tenant != self.intent.tenant_id || !owned {
            return Err(conflict(
                "replica_stop_unproven",
                "the stop proof is not for one of this tenant's reader apps",
            ));
        }
        let release = |resource: &str, sequence: u64| {
            let mut parts = resource.splitn(3, '/');
            let (Some(kind), Some(set_id), Some(pin_id)) =
                (parts.next(), parts.next(), parts.next())
            else {
                return Err(state_invalid());
            };
            match kind {
                "pin" => sets.release(set_id, pin_id, now)?,
                "reader-pin" => sets.release_reader(set_id, pin_id)?,
                _ => return Err(state_invalid()),
            }
            self.settle(sequence, "released")
        };
        for step in journal.steps {
            if step.action.operation == RELEASE_PIN && step.outcome == (StepOutcome::Uncertain {}) {
                release(&step.action.resource, step.sequence)?;
            }
        }
        let reason = crate::server::reader_apply::pin_reason(&stopped.replica);
        let abandoned = |pin: &Pin| {
            pin.reason == reason
                && pin.kind == PinKind::Operation
                && pin.created_unix_ms <= stopped.stopped_unix_ms
        };
        // Record pins, then isolated reader pin objects (ITEM-008).
        let mut candidates = Vec::new();
        for record in sets.list()?.sets.into_iter().filter_map(|s| s.record) {
            for pin in record.pins.iter().filter(|pin| abandoned(pin)) {
                candidates.push(format!("pin/{}/{}", record.set_id, pin.id));
            }
        }
        for (set_id, pin) in sets.reader_pins()? {
            if abandoned(&pin) {
                candidates.push(format!("reader-pin/{set_id}/{}", pin.id));
            }
        }
        let mut released = 0;
        for resource in candidates {
            let action = self.action(&resource, RELEASE_PIN, &stopped.replica, true);
            match self.ops.record(self.op(), &self.attempt, &action)? {
                Decision::Dispatch(n) | Decision::Reconcile(n) => {
                    release(&resource, n)?;
                    released += 1;
                }
                Decision::Settled(_) => {}
            }
        }
        Ok(released)
    }
}

fn routing_unqualified() -> ApiError {
    conflict(
        "reader_routing_unqualified",
        "routing reads to readers is not qualified; the route stays writer-only",
    )
}

fn readers_not_held() -> ApiError {
    conflict(
        "operation_not_provisionable",
        "the operation does not hold its uncancelled reader reservation",
    )
}

fn operation_cancelled() -> ApiError {
    conflict(
        "operation_cancelled",
        "a cancellation was requested; the retirement did not remove the route",
    )
}

enum Dispatched {
    /// Already settled earlier.
    Settled,
    /// Step `n` was refused by the port with no effect.
    Refused(u64, String),
    /// Step `n` awaits its readback, with its cloud operation's outcome.
    Pending(u64, Outcome),
}

/// A cloud operation's polled state.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Succeeded,
    Failed(String),
    /// Not known to be terminal: it may still act.
    Open,
}

/// A settled hold step: held, or released after a cancellation.
fn held(observed: &str) -> Result<(), ApiError> {
    if observed == "held" {
        Ok(())
    } else {
        Err(operation_cancelled())
    }
}

fn created(observed: &str) -> Result<(), ApiError> {
    match observed {
        "created" => Ok(()),
        "conflict" => Err(ownership_conflict()),
        _ => Err(failed()),
    }
}

/// Ours, as configured, provisioned and, for an app, with a complete
/// inventory whose single active revision runs the configuration.
fn exact(desired: &Desired, found: &Observed) -> bool {
    let revision = match &found.revisions {
        Revisions::NotApplicable => !desired.app,
        Revisions::Complete(revisions) => {
            let active: Vec<_> = revisions.iter().filter(|r| r.active).collect();
            desired.app && active.len() == 1 && active[0].config_digest == desired.config_digest
        }
        Revisions::Incomplete => false,
    };
    found.config_digest == desired.config_digest && found.succeeded && revision
}

/// Resources of `operation` steps settled as one of `observed`, in order.
fn apps(journal: &Journal, operation: &str, observed: &[&str]) -> Vec<String> {
    journal
        .steps
        .iter()
        .filter(|step| {
            step.action.operation == operation
                && matches!(&step.outcome, StepOutcome::Settled { observed: o } if observed.contains(&o.as_str()))
        })
        .map(|step| step.action.resource.clone())
        .collect()
}

fn readers_unanchored() -> ApiError {
    conflict(
        "readers_unanchored",
        "the tenant's route was not published by a sealed lifecycle operation",
    )
}

fn retirement_mismatch() -> ApiError {
    conflict(
        "retirement_receipt_mismatch",
        "the tenant's sealed activation, receipt or route does not match the retirement",
    )
}

fn plan(journal: &Journal) -> Option<&str> {
    journal.steps.iter().find_map(|step| match &step.outcome {
        StepOutcome::Settled { observed }
            if step.action.resource == PLAN && step.action.operation == VERIFY =>
        {
            Some(observed.as_str())
        }
        _ => None,
    })
}

/// Called by the sealing journal CAS: a cancelled operation ends only once
/// every app it created is deleted, a retirement only once every planned
/// app is proven stopped (or absent), and a reader change (EPIC-005) only
/// once its reader app is created or updated and its fleet proven eligible,
/// or, for a removal, every reader app is proven stopped (or absent).
pub(super) fn terminal_ready(
    journal: &Journal,
    payload: &Payload,
    outcome: &TerminalOutcome,
) -> Result<(), ApiError> {
    match outcome {
        TerminalOutcome::Succeeded { .. } => {
            let Payload::SetReaders { fleet, .. } = payload else {
                return Ok(());
            };
            let ready = match fleet {
                Fleet::Present { .. } => {
                    (!apps(journal, CREATE_APP, &["created"]).is_empty()
                        || !apps(journal, UPDATE_APP, &["updated"]).is_empty())
                        && !apps(journal, ELIGIBLE, &["eligible"]).is_empty()
                }
                Fleet::Absent {} => {
                    let stopped = apps(journal, STOP_APP, &["stopped", "absent"]);
                    plan(journal)
                        .and_then(ReaderPlan::parse)
                        .is_some_and(|plan| {
                            plan.readers().all(|app| stopped.iter().any(|s| s == app))
                        })
                }
            };
            if ready {
                Ok(())
            } else {
                Err(conflict(
                    "readers_incomplete",
                    "the reader app is not proven updated and eligible, or a removed reader app is not proven stopped",
                ))
            }
        }
        TerminalOutcome::Cancelled => {
            let deleted = apps(journal, DELETE_APP, &["deleted", "absent"]);
            let held = journal
                .steps
                .iter()
                .any(|step| step.action.resource == GUARD);
            let released = journal.steps.iter().any(|step| {
                step.action.resource == GUARD
                    && step.action.operation == RELEASE
                    && matches!(&step.outcome, StepOutcome::Settled { .. })
            });
            if apps(journal, CREATE_APP, &["created"])
                .iter()
                .all(|app| deleted.contains(app))
                && (!held || released)
            {
                Ok(())
            } else {
                Err(conflict(
                    "cleanup_pending",
                    "an app this operation created is not deleted yet, or its lifecycle guard is not released; run the cleanup",
                ))
            }
        }
        TerminalOutcome::Retired => {
            let stopped = apps(journal, STOP_APP, &["stopped", "absent"]);
            match plan(journal) {
                Some(plan)
                    if plan
                        .split(' ')
                        .skip(2)
                        .all(|app| stopped.iter().any(|s| s == app)) =>
                {
                    Ok(())
                }
                _ => Err(conflict(
                    "retirement_incomplete",
                    "a planned app is not proven stopped",
                )),
            }
        }
    }
}
