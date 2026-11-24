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
//!   every pre-existing or non-app resource are always retained. No reader
//!   is ever started (EPIC-005), so no reader pin exists to clean up.
use super::guard::Guards;
use super::*;

/// Owner tag value prefix on every resource this contract creates.
pub(super) const OWNER_TAG: &str = "vlite-tenant";
/// Journal action names.
const CREATE: &str = "create";
const CREATE_APP: &str = "create-app";
const VERIFY: &str = "verify";
const DELETE_APP: &str = "delete-app";
const STOP_APP: &str = "stop-app";
/// The retirement plan step (ITEM-011).
const PLAN: &str = "control/receipt";
/// Journal resource for the retirement's lifecycle guard hold/release.
const GUARD: &str = "control/guard";
const HOLD: &str = "hold";
const RELEASE: &str = "release";

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
    /// when the readback satisfies `proven`. An absent resource needs no
    /// action; one owned by anyone else is never touched.
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
                None => {
                    return match self.ops.record(self.op(), &self.attempt, &action)? {
                        Decision::Dispatch(n) | Decision::Reconcile(n) => self.settle(n, "absent"),
                        Decision::Settled(_) => Ok(()),
                    };
                }
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
        let (sequence, outcome) = match self.dispatch(&action, call)? {
            Dispatched::Settled => return Err(state_invalid()),
            Dispatched::Refused(sequence, code) => (sequence, Outcome::Failed(code)),
            Dispatched::Pending(sequence, outcome) => (sequence, outcome),
        };
        let found = self.read(resource)?;
        if found
            .as_ref()
            .is_some_and(|found| found.owner.as_deref() != Some(self.owner().as_str()))
        {
            return Err(ownership_conflict());
        }
        if proven(found.as_ref()) {
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
        reserve(self.ops, objects, self.intent, now)
    }

    fn verify_receipt(&self, objects: &Objects, expected: &str) -> Result<String, ApiError> {
        let mismatch = || {
            conflict(
                "retirement_receipt_mismatch",
                "the tenant's sealed activation, receipt or route does not match the retirement",
            )
        };
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
        // A cancelled retirement replaced the slot but never removed the
        // route: follow its verified plan exactly one hop back to the
        // activation it verified (a later plan names that activation again).
        let (activation, terminal) = match &terminal.outcome {
            TerminalOutcome::Succeeded { .. } => (operation_id.clone(), terminal),
            TerminalOutcome::Cancelled => {
                let journal = self.ops.journal(operation_id)?;
                let mut words = plan(&journal).ok_or_else(mismatch)?.split(' ');
                let activation = words.next().ok_or_else(mismatch)?.to_owned();
                let planned_route = words.next().ok_or_else(mismatch)?.to_owned();
                let Some((terminal, _)) = self.ops.read_terminal(&activation)? else {
                    return Err(mismatch());
                };
                match &terminal.outcome {
                    TerminalOutcome::Succeeded { receipt }
                        if receipt.operation_id == activation
                            && receipt.route_digest == planned_route => {}
                    _ => return Err(mismatch()),
                }
                (activation, terminal)
            }
            TerminalOutcome::Retired => return Err(mismatch()),
        };
        let TerminalOutcome::Succeeded { receipt } = &terminal.outcome else {
            return Err(mismatch());
        };
        let route = snapshot
            .record_for(&self.intent.tenant_id)
            .map(control_state::route_digest);
        if terminal.tenant_id != self.intent.tenant_id
            || super::admin::receipt_digest(receipt) != expected
            || route.as_deref() != Some(receipt.route_digest.as_str())
        {
            return Err(mismatch());
        }
        let apps = apps(&self.ops.journal(&activation)?, CREATE_APP, &["created"]);
        Ok(std::iter::once(activation)
            .chain(std::iter::once(receipt.route_digest.clone()))
            .chain(apps)
            .collect::<Vec<_>>()
            .join(" "))
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
    /// knows to reconcile it.
    fn hold_guard(&self, guards: &Guards) -> Result<(), ApiError> {
        let action = self.guard_action(HOLD);
        if let Some(Step {
            outcome: StepOutcome::Settled { .. },
            ..
        }) = self.step(&action)?
        {
            return Ok(());
        }
        match self.ops.record(self.op(), &self.attempt, &action)? {
            Decision::Settled(_) => Ok(()),
            Decision::Dispatch(n) | Decision::Reconcile(n) => {
                guards.hold(&self.intent.tenant_id, self.op())?;
                self.settle(n, "held")
            }
        }
    }

    /// After a cancellation won: releases this operation's guard if it
    /// holds it (never another operation's), and journals the release the
    /// seal requires ([`terminal_ready`]). Idempotent; a no-op without a
    /// journaled hold.
    fn release_guard(&self, guards: &Guards) -> Result<(), ApiError> {
        let journal = self.ops.journal(self.op())?;
        let Some(hold) = journal
            .steps
            .iter()
            .find(|step| step.action == self.guard_action(HOLD))
        else {
            return Ok(());
        };
        if let Some(guard) = guards.current()?
            && guard.operation_id == *self.op()
            && guard.state != super::guard::State::Released
        {
            guards.release(&self.intent.tenant_id, self.op())?;
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
/// every app it created is deleted, and a retirement only once every
/// planned app is proven stopped (or absent).
pub(super) fn terminal_ready(journal: &Journal, outcome: &TerminalOutcome) -> Result<(), ApiError> {
    match outcome {
        TerminalOutcome::Succeeded { .. } => Ok(()),
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
