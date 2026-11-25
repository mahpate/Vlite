//! design-tenant-provisioning EPIC-009 ITEM-022: the control worker.
//!
//! One `vlite-server tenant-deployment work` invocation selects at most one
//! accepted create operation, passes every gate, claims the operation
//! through the operation journal's compare-and-swap and runs exactly one
//! provision attempt through the ARM port ([`aca::Arm`]).
//!
//! - Every gate refuses before the claim, so before any port call: the
//!   reservation must be held and uncancelled, the retained profile must
//!   resolve ([`aca::Profile::resolve`]; every kind but create is
//!   `profile_unsupported`), [`aca::preflight`] needs qualification, quota
//!   and budget proofs that no production code can produce yet
//!   (`qualification_missing` first), and the production adapter must exist
//!   ([`production_arm`] refuses `transition_unavailable` until ITEM-020 is
//!   wired). Claiming first would strand the operation: no later attempt
//!   can take it over without an `ActuatorStopped` proof (OD-002).
//! - Exactly one claim: an operation another attempt claimed first, or won
//!   in a race, refuses `actuator_fence_unproven` (naming the holder before
//!   any other gate); the worker does not try another operation.
//! - A claim is never released: whatever the outcome, the operation stays
//!   claimed by its attempt, so `failed` is not retryable until OD-002.
//! - No retries: [`aca::Driver::provision`] journals each dispatch before
//!   it is sent and settles it only from an exact readback, so an unknown
//!   cloud outcome stays an uncertain step and is reported `unknown`, never
//!   success.
//! - No routes: the worker never writes the control state. Only the
//!   activation compare-and-swap ([`activate_create`]) publishes a route,
//!   and it needs readiness and delivery proofs nothing here produces.
use super::*;

/// Poll budget for one cloud operation. `Driver` does not sleep between
/// polls, so the production adapter's `poll` must pace itself.
const POLLS: usize = 30;

/// The preflight proofs. Their types have no production constructor yet
/// (OD-006), so production always runs with none and refuses.
#[derive(Default)]
pub(super) struct Proofs {
    pub(super) qualification: Option<aca::Qualification>,
    pub(super) quota: Option<aca::QuotaProof>,
    pub(super) budget: Option<aca::BudgetApproval>,
}

/// Builds the ARM port for one profile's desired resource bodies.
pub(super) type Adapter<'a> =
    dyn Fn(&BTreeMap<String, Value>) -> Result<Box<dyn aca::Arm>, ApiError> + 'a;

/// SEAM(ITEM-020): the production ARM adapter, built from the profile's
/// desired bodies. ITEM-020's `ArmClient` is not wired here yet, so this
/// refuses before anything is claimed or dispatched. Wire it here and
/// nowhere else; every gate in [`work`] still runs first.
pub(super) fn production_arm(
    _bodies: &BTreeMap<String, Value>,
) -> Result<Box<dyn aca::Arm>, ApiError> {
    Err(conflict(
        "transition_unavailable",
        "no production ARM adapter is wired (ITEM-020); nothing was claimed or dispatched",
    ))
}

/// A fresh attempt ID per invocation, so no two workers share one.
pub(super) fn attempt_id() -> Result<String, ApiError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(internal)?;
    Ok(format!("worker-{}", hex(&bytes)))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    /// Every profile resource is created or verified and settled in the
    /// journal. The operation still needs its activation.
    Provisioned,
    /// Refused before anything was dispatched: by a gate or by the claim.
    Blocked,
    /// The attempt ended unprovisioned with every journaled step settled.
    /// The operation stays claimed by this attempt.
    Failed,
    /// Something may have taken effect: a journaled step stays uncertain,
    /// or the claim's own write outcome is unknown.
    Unknown,
}

impl Outcome {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Provisioned => "provisioned",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }
}

/// One invocation's attempt on the operation it selected.
pub(super) struct Attempt {
    pub(super) intent: Intent,
    pub(super) outcome: Outcome,
    /// Why the operation was not provisioned.
    pub(super) error: Option<ApiError>,
}

/// One worker invocation (blocking): `operation`, or else the first
/// unclaimed create (ascending ID) that holds an uncancelled reservation,
/// gets exactly one attempt as `attempt`. None: nothing to claim. An error
/// means no operation was selected (an unknown ID, or a store or record
/// that could not be read): nothing was claimed or dispatched.
pub(super) fn work(
    objects: &Objects,
    ops: &Operations,
    proofs: &Proofs,
    adapter: &Adapter<'_>,
    operation: Option<&str>,
    attempt: &str,
) -> Result<Option<Attempt>, ApiError> {
    let Some(loaded) = select(objects, ops, operation)? else {
        return Ok(None);
    };
    let (outcome, error) = match run(objects, ops, proofs, adapter, &loaded, attempt) {
        Ok(()) => (Outcome::Provisioned, None),
        Err((outcome, error)) => (outcome, Some(error)),
    };
    Ok(Some(Attempt {
        intent: loaded.intent,
        outcome,
        error,
    }))
}

fn provisionable(snapshot: &Snapshot, intent: &Intent) -> bool {
    matches!(
        holds(snapshot, intent),
        Some(Slot::Reserved {
            cancellation_requested: false,
            ..
        })
    )
}

fn select(
    objects: &Objects,
    ops: &Operations,
    operation: Option<&str>,
) -> Result<Option<Loaded>, ApiError> {
    if let Some(id) = operation {
        // Also keeps a caller-chosen ID out of object names.
        if !is_hex(id, 32) {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "operation_not_found",
                "no such operation",
            ));
        }
        return ops.load(id).map(Some);
    }
    let (snapshot, _) = objects.read()?;
    // One listing per invocation: `ids` lists the whole prefix on every
    // call, so paging through it here would list it once per page.
    for id in ops.ids(None, usize::MAX)? {
        let journal = ops.journal(&id)?;
        if journal.attempt.is_some() || journal.closed {
            continue;
        }
        // Fails closed: an unreadable record stops the scan.
        let loaded = ops.load(&id)?;
        if loaded.intent.kind() == Kind::Create && provisionable(&snapshot, &loaded.intent) {
            return Ok(Some(loaded));
        }
    }
    Ok(None)
}

/// Every gate, then the claim, then one provision; never a retry.
fn run(
    objects: &Objects,
    ops: &Operations,
    proofs: &Proofs,
    adapter: &Adapter<'_>,
    loaded: &Loaded,
    attempt: &str,
) -> Result<(), (Outcome, ApiError)> {
    let blocked = |error: ApiError| (Outcome::Blocked, error);
    let intent = &loaded.intent;
    let (snapshot, _) = objects.read().map_err(blocked)?;
    let journal = ops.journal(&intent.operation_id).map_err(blocked)?;
    if journal.closed || !provisionable(&snapshot, intent) {
        return Err(blocked(conflict(
            "operation_not_provisionable",
            "the operation does not hold an uncancelled reservation",
        )));
    }
    // Named before any gate, so a later gate cannot mask why it can never
    // proceed without an `ActuatorStopped` proof (OD-002).
    if let Some(holder) = journal
        .attempt
        .as_deref()
        .filter(|holder| *holder != attempt)
    {
        return Err(blocked(ApiError::new(
            StatusCode::CONFLICT,
            "actuator_fence_unproven",
            format!("the operation is claimed by attempt {holder}, which is not proven stopped"),
        )));
    }
    let profile = aca::Profile::resolve(loaded).map_err(blocked)?;
    let bodies = profile.bodies().clone();
    let preflighted = aca::preflight(
        profile,
        proofs.qualification.as_ref(),
        proofs.quota.as_ref(),
        proofs.budget.as_ref(),
    )
    .map_err(blocked)?;
    let arm = adapter(&bodies).map_err(blocked)?;
    let driver = aca::Driver::new(ops, &*arm, intent, attempt, None, POLLS).map_err(|error| {
        // The claim's own write may have landed.
        let outcome = if outcome_unknown(&error) {
            Outcome::Unknown
        } else {
            Outcome::Blocked
        };
        (outcome, error)
    })?;
    driver
        .provision(objects, &preflighted)
        .map_err(|mut error| {
            let outcome = ended(ops, &intent.operation_id, &error);
            error.message = format!(
                "{}; the operation stays claimed by attempt {attempt} until it is proven stopped",
                error.message
            );
            (outcome, error)
        })
}

/// An attempt that ended in `error` failed only if the journal proves every
/// step settled; anything else is unknown.
fn ended(ops: &Operations, operation_id: &str, error: &ApiError) -> Outcome {
    if outcome_unknown(error) {
        return Outcome::Unknown;
    }
    match ops.journal(operation_id) {
        Ok(journal)
            if journal
                .steps
                .iter()
                .all(|step| matches!(step.outcome, StepOutcome::Settled { .. })) =>
        {
            Outcome::Failed
        }
        _ => Outcome::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::cloud_backup::fake::{Fault, Memory, Op};
    use crate::server::{credentials, tenants};
    use aca::{
        Arm, ArmError, BudgetApproval, Call, Demand, Observed, Poll, Qualification, QuotaProof,
        Revision, Revisions, Verb,
    };
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    const CONTROL: &str = "control/state.json";
    const PREFIX: &str = "tenant-operations";
    const NOW: u64 = 1_700_000_000_000;
    const INIT: &str = r#"{"dimension":"from-approved-profile"}"#;

    /// The tenant's owned writer app.
    fn writer(name: &str) -> String {
        format!(
            "/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/rg-1/providers/Microsoft.App/containerApps/{name}-writer"
        )
    }

    /// The EPIC-008 example's owned writer app, alone, for `name`.
    fn profile(name: &str) -> String {
        json!({"resources": [{
            "id": writer(name),
            "ownership": "owned",
            "body": {
                "location": "westus2",
                "properties": {
                    "environmentId": "/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/rg-1/providers/Microsoft.App/managedEnvironments/vlite-env",
                    "configuration": {"activeRevisionsMode": "Single"},
                    "template": {
                        "containers": [{
                            "name": "vlite",
                            "image": format!("example.azurecr.io/vlite@sha256:{}", "0".repeat(64)),
                            "resources": {"cpu": 0.5, "memory": "1Gi"},
                        }],
                        "scale": {"minReplicas": 1, "maxReplicas": 1},
                    },
                },
            },
        }]})
        .to_string()
    }

    enum Begin {
        Refused,
        Unknown,
    }

    #[derive(Default)]
    struct State {
        begins: VecDeque<Begin>,
        calls: Vec<Call>,
        resources: BTreeMap<String, Observed>,
    }

    /// A scripted ARM port: a create takes effect at once unless scripted.
    #[derive(Clone, Default)]
    struct FakeArm(Arc<Mutex<State>>);

    impl FakeArm {
        fn script(&self, begin: Begin) {
            self.0.lock().unwrap().begins.push_back(begin);
        }

        fn calls(&self) -> usize {
            self.0.lock().unwrap().calls.len()
        }
    }

    impl Arm for FakeArm {
        fn begin(&self, call: &Call) -> Result<String, ArmError> {
            let mut state = self.0.lock().unwrap();
            state.calls.push(call.clone());
            match state.begins.pop_front() {
                Some(Begin::Refused) => Err(ArmError::Refused("QuotaExceeded".into())),
                Some(Begin::Unknown) => Err(ArmError::Unknown),
                None => {
                    if let Verb::Put { config_digest } = &call.verb {
                        let observed = Observed {
                            owner: Some(call.owner.clone()),
                            config_digest: config_digest.clone(),
                            succeeded: true,
                            revisions: Revisions::Complete(vec![Revision {
                                name: "r0".into(),
                                active: true,
                                config_digest: config_digest.clone(),
                            }]),
                        };
                        state.resources.insert(call.resource.clone(), observed);
                    }
                    Ok(format!("arm-op-{}", state.calls.len()))
                }
            }
        }

        fn poll(&self, _operation: &str) -> Result<Poll, ArmError> {
            Ok(Poll::Succeeded)
        }

        fn read(&self, resource: &str) -> Result<Option<Observed>, ArmError> {
            Ok(self.0.lock().unwrap().resources.get(resource).cloned())
        }
    }

    fn full() -> Proofs {
        Proofs {
            qualification: Some(Qualification::for_test()),
            quota: Some(QuotaProof::for_test(Demand {
                apps: 10,
                cores_milli: 10_000,
                memory_mib: 100_000,
            })),
            budget: Some(BudgetApproval::for_test()),
        }
    }

    struct World {
        directory: tempfile::TempDir,
        memory: Arc<Memory>,
        store: tenants::Store,
        objects: Objects,
        ops: Operations,
    }

    /// A control state with no tenant, beside an empty operation store.
    fn world() -> World {
        let directory = tempfile::tempdir().unwrap();
        let store = tenants::Store::load(directory.path()).unwrap();
        let credentials = credentials::Store::load(directory.path()).unwrap();
        let memory = Arc::new(Memory::default());
        Objects::new(memory.clone(), CONTROL)
            .create(Snapshot::import(&credentials, Some(&store)).unwrap(), NOW)
            .unwrap();
        World {
            directory,
            objects: Objects::new(memory.clone(), CONTROL),
            ops: Operations::new(memory.clone(), PREFIX),
            memory,
            store,
        }
    }

    impl World {
        fn deployments(&self) -> admin::Deployments {
            admin::Deployments::new(self.memory.clone(), CONTROL, PREFIX)
        }

        /// Accepts and reserves a create the way the CLI does.
        fn submit(&self, key: &str, name: &str) -> String {
            let profile = profile(name);
            let request = json!({
                "idempotency_key": key,
                "request": {
                    "kind": "create",
                    "name": name,
                    "max_vectors": 1000,
                    "writer_profile": sha256_hex(profile.as_bytes()),
                    "initialization": sha256_hex(INIT.as_bytes()),
                    "resources": [writer(name)],
                    "secret_versions": ["archive-sas@3"],
                },
                "contents": [profile, INIT],
            });
            let submit = admin::Command::Submit {
                kind: Kind::Create,
                tenant: None,
                request: request.to_string().into_bytes(),
            };
            let created = admin::run(&self.deployments(), &self.store, None, submit, NOW).unwrap();
            assert_eq!(created["operation"]["phase"], "reserved");
            created["operation"]["operation_id"]
                .as_str()
                .unwrap()
                .to_owned()
        }

        fn work(
            &self,
            proofs: &Proofs,
            arm: &FakeArm,
            operation: Option<&str>,
            attempt: &str,
        ) -> Result<Option<Attempt>, ApiError> {
            let adapter = |_: &BTreeMap<String, Value>| -> Result<Box<dyn Arm>, ApiError> {
                Ok(Box::new(arm.clone()))
            };
            work(
                &self.objects,
                &self.ops,
                proofs,
                &adapter,
                operation,
                attempt,
            )
        }

        /// One attempt that was selected.
        #[track_caller]
        fn attempt(&self, proofs: &Proofs, arm: &FakeArm, operation: Option<&str>) -> Attempt {
            self.work(proofs, arm, operation, "worker-1")
                .unwrap()
                .expect("an operation is selected")
        }

        fn journal_object(&self, id: &str) -> Option<Vec<u8>> {
            self.memory
                .raw(&format!("{PREFIX}/operations/{id}/journal.json"))
        }
    }

    #[track_caller]
    fn refused(attempt: &Attempt, outcome: Outcome, code: &str) {
        assert_eq!(attempt.outcome, outcome);
        assert_eq!(attempt.error.as_ref().map(|e| e.code), Some(code));
    }

    #[test]
    fn one_invocation_claims_and_provisions_exactly_one_operation() {
        let w = world();
        let mut ids = [
            (w.submit("a", "contoso"), "contoso"),
            (w.submit("b", "fabrikam"), "fabrikam"),
        ];
        ids.sort();
        let [(first, name), (second, _)] = &ids;
        let control = w.memory.raw(CONTROL).unwrap();
        let arm = FakeArm::default();
        let done = w.attempt(&full(), &arm, None);
        assert_eq!(
            (done.outcome, done.error.is_none()),
            (Outcome::Provisioned, true)
        );
        // The lowest ID, and only that one, is claimed and provisioned.
        assert_eq!(&done.intent.operation_id, first);
        assert!(w.journal_object(second).is_none());
        let journal = w.ops.journal(first).unwrap();
        assert_eq!(journal.attempt.as_deref(), Some("worker-1"));
        assert_eq!(journal.steps.len(), 1);
        let step = &journal.steps[0];
        assert_eq!(
            (
                step.action.resource.as_str(),
                step.action.operation.as_str()
            ),
            (writer(name).as_str(), "create-app")
        );
        assert_eq!(
            step.outcome,
            StepOutcome::Settled {
                observed: "created".into()
            }
        );
        assert_eq!(arm.calls(), 1);
        // No route: the control state is untouched.
        assert_eq!(w.memory.raw(CONTROL).unwrap(), control);
        let (snapshot, _) = w.objects.read().unwrap();
        assert!(snapshot.record_for(&done.intent.tenant_id).is_none());
        // The next invocation takes the next operation, the one after none.
        let next = w.work(&full(), &arm, None, "worker-2").unwrap().unwrap();
        assert_eq!(
            (next.intent.operation_id.as_str(), next.outcome),
            (second.as_str(), Outcome::Provisioned)
        );
        assert!(w.work(&full(), &arm, None, "worker-3").unwrap().is_none());
        assert_eq!(arm.calls(), 2);
    }

    #[test]
    fn a_refused_dispatch_is_settled_and_reported_failed() {
        let w = world();
        let id = w.submit("a", "contoso");
        let arm = FakeArm::default();
        arm.script(Begin::Refused);
        let done = w.attempt(&full(), &arm, None);
        refused(&done, Outcome::Failed, "cloud_action_failed");
        assert!(
            done.error
                .unwrap()
                .message
                .ends_with("stays claimed by attempt worker-1 until it is proven stopped")
        );
        let journal = w.ops.journal(&id).unwrap();
        assert_eq!(
            journal.steps[0].outcome,
            StepOutcome::Settled {
                observed: "failed:QuotaExceeded".into()
            }
        );
        assert_eq!(journal.attempt.as_deref(), Some("worker-1"));
        // Failed is not retryable: the claim stands.
        assert!(w.work(&full(), &arm, None, "worker-2").unwrap().is_none());
        assert_eq!(arm.calls(), 1);
    }

    #[test]
    fn an_unknown_dispatch_stays_uncertain_and_is_never_retried() {
        let w = world();
        let id = w.submit("a", "contoso");
        let arm = FakeArm::default();
        arm.script(Begin::Unknown);
        let done = w.attempt(&full(), &arm, None);
        refused(&done, Outcome::Unknown, "cloud_outcome_unknown");
        let journal = w.ops.journal(&id).unwrap();
        assert_eq!(journal.steps.len(), 1);
        assert_eq!(journal.steps[0].outcome, StepOutcome::Uncertain {});
        // A claimed operation is never selected again ...
        assert!(w.work(&full(), &arm, None, "worker-2").unwrap().is_none());
        // ... and naming it is a takeover, refused without the port.
        let again = w
            .work(&full(), &arm, Some(&id), "worker-2")
            .unwrap()
            .unwrap();
        refused(&again, Outcome::Blocked, "actuator_fence_unproven");
        assert_eq!(arm.calls(), 1);
        assert_eq!(w.ops.journal(&id).unwrap(), journal);
    }

    #[test]
    fn a_lost_claim_race_is_refused_before_the_port() {
        let w = world();
        let id = w.submit("a", "contoso");
        let (memory, other) = (w.memory.clone(), id.clone());
        // Another worker claims between this one's selection and its claim.
        w.memory.hook(Op::Put, "journal.json", move || {
            Operations::new(memory, PREFIX)
                .claim(&other, "worker-other", None)
                .unwrap();
        });
        let arm = FakeArm::default();
        let done = w.attempt(&full(), &arm, None);
        refused(&done, Outcome::Blocked, "actuator_fence_unproven");
        assert_eq!(done.intent.operation_id, id);
        assert_eq!(arm.calls(), 0);
        let journal = w.ops.journal(&id).unwrap();
        assert_eq!(journal.attempt.as_deref(), Some("worker-other"));
        assert!(journal.steps.is_empty());
    }

    #[test]
    fn a_foreign_claim_is_never_taken_over() {
        let w = world();
        let id = w.submit("a", "contoso");
        w.ops.claim(&id, "worker-stale", None).unwrap();
        let arm = FakeArm::default();
        assert!(w.work(&full(), &arm, None, "worker-1").unwrap().is_none());
        let done = w.attempt(&full(), &arm, Some(&id));
        refused(&done, Outcome::Blocked, "actuator_fence_unproven");
        assert!(
            done.error.unwrap().message.contains("worker-stale"),
            "the holder is named"
        );
        // The claim is named before a gate that would otherwise refuse.
        let done = w.attempt(&Proofs::default(), &arm, Some(&id));
        refused(&done, Outcome::Blocked, "actuator_fence_unproven");
        assert_eq!(arm.calls(), 0);
        assert_eq!(
            w.ops.journal(&id).unwrap().attempt.as_deref(),
            Some("worker-stale")
        );
    }

    #[test]
    fn nothing_to_claim_is_idle() {
        let w = world();
        let arm = FakeArm::default();
        assert!(w.work(&full(), &arm, None, "worker-1").unwrap().is_none());
        assert!(
            w.work(&Proofs::default(), &arm, None, "worker-1")
                .unwrap()
                .is_none()
        );
        assert_eq!(arm.calls(), 0);
    }

    #[test]
    fn every_gate_refuses_in_order_before_the_claim_and_the_adapter() {
        let w = world();
        let id = w.submit("a", "contoso");
        let built = Cell::new(0);
        let arm = FakeArm::default();
        let adapter = |_: &BTreeMap<String, Value>| -> Result<Box<dyn Arm>, ApiError> {
            built.set(built.get() + 1);
            Ok(Box::new(arm.clone()))
        };
        let full = full();
        let quota = || {
            Some(QuotaProof::for_test(Demand {
                apps: 10,
                cores_milli: 10_000,
                memory_mib: 100_000,
            }))
        };
        let small = Some(QuotaProof::for_test(Demand {
            apps: 1,
            cores_milli: 1,
            memory_mib: 1,
        }));
        let cases = [
            (Proofs::default(), "qualification_missing"),
            (
                Proofs {
                    quota: quota(),
                    budget: Some(BudgetApproval::for_test()),
                    ..Proofs::default()
                },
                "qualification_missing",
            ),
            (
                Proofs {
                    qualification: Some(Qualification::for_test()),
                    budget: Some(BudgetApproval::for_test()),
                    ..Proofs::default()
                },
                "quota_unproven",
            ),
            (
                Proofs {
                    qualification: Some(Qualification::for_test()),
                    quota: small,
                    budget: Some(BudgetApproval::for_test()),
                },
                "quota_exceeded",
            ),
            (
                Proofs {
                    qualification: Some(Qualification::for_test()),
                    quota: quota(),
                    ..Proofs::default()
                },
                "budget_unapproved",
            ),
        ];
        for (proofs, code) in cases {
            let done = work(&w.objects, &w.ops, &proofs, &adapter, None, "worker-1")
                .unwrap()
                .unwrap();
            refused(&done, Outcome::Blocked, code);
            assert_eq!(done.intent.operation_id, id);
        }
        // The production seam refuses last, still before the claim.
        let done = work(&w.objects, &w.ops, &full, &production_arm, None, "worker-1")
            .unwrap()
            .unwrap();
        refused(&done, Outcome::Blocked, "transition_unavailable");
        assert_eq!((built.get(), arm.calls()), (0, 0));
        assert!(w.journal_object(&id).is_none());
        // With every proof and an adapter, the same operation proceeds.
        let done = work(&w.objects, &w.ops, &full, &adapter, None, "worker-1")
            .unwrap()
            .unwrap();
        assert_eq!(done.outcome, Outcome::Provisioned);
        assert_eq!((built.get(), arm.calls()), (1, 1));
    }

    #[test]
    fn production_refuses_through_the_cli_core_before_the_claim() {
        let w = world();
        let id = w.submit("a", "contoso");
        let log = audit::Log::open(w.directory.path()).unwrap();
        let command = |operation: Option<&str>| admin::Command::Work {
            operation: operation.map(str::to_owned),
            attempt: "worker-1".into(),
        };
        let error =
            admin::run(&w.deployments(), &w.store, Some(&log), command(None), NOW).unwrap_err();
        assert_eq!(error.code, "qualification_missing");
        assert!(
            error.message.ends_with(&format!(
                "; operation {id}, attempt worker-1, outcome blocked"
            )),
            "{}",
            error.message
        );
        let error =
            admin::run(&w.deployments(), &w.store, None, command(Some(&id)), NOW).unwrap_err();
        assert_eq!(error.code, "qualification_missing");
        assert!(w.journal_object(&id).is_none());
        let events = log.tail(10).unwrap();
        let event = events
            .iter()
            .find(|e| e["event"] == "tenant_deployment_attempt")
            .unwrap();
        assert_eq!(event["operation_id"], id.as_str());
        assert_eq!(event["attempt"], "worker-1");
        assert_eq!(event["outcome"], "blocked");
        assert_eq!(event["code"], "qualification_missing");
        let error =
            admin::run(&w.deployments(), &w.store, None, command(Some("0")), NOW).unwrap_err();
        assert_eq!(error.code, "operation_not_found");
        let idle = world();
        let result =
            admin::run(&idle.deployments(), &idle.store, None, command(None), NOW).unwrap();
        assert_eq!(result, json!({"outcome": "idle"}));
    }

    #[test]
    fn the_cli_core_reports_a_provisioned_attempt_and_audits_it() {
        let w = world();
        let id = w.submit("a", "contoso");
        let log = audit::Log::open(w.directory.path()).unwrap();
        let arm = FakeArm::default();
        let adapter = |_: &BTreeMap<String, Value>| -> Result<Box<dyn Arm>, ApiError> {
            Ok(Box::new(arm.clone()))
        };
        let result = admin::work(
            &w.deployments(),
            Some(&log),
            &full(),
            &adapter,
            None,
            "worker-1",
        )
        .unwrap();
        assert_eq!(result["outcome"], "provisioned");
        assert_eq!(result["attempt"], "worker-1");
        assert_eq!(result["audit_recorded"], true);
        assert_eq!(result["operation"]["operation_id"], id.as_str());
        assert_eq!(result["operation"]["phase"], "reserved");
        let events = log.tail(10).unwrap();
        let event = events
            .iter()
            .find(|e| e["event"] == "tenant_deployment_attempt")
            .unwrap();
        assert_eq!(event["outcome"], "provisioned");
        assert!(event.get("code").is_none());
        // An unknown outcome is an error, never success.
        let w = world();
        w.submit("a", "fabrikam");
        arm.script(Begin::Unknown);
        let error =
            admin::work(&w.deployments(), None, &full(), &adapter, None, "worker-1").unwrap_err();
        assert_eq!(error.code, "cloud_outcome_unknown");
        assert!(
            error
                .message
                .contains("stays claimed by attempt worker-1 until it is proven stopped")
                && error.message.ends_with("outcome unknown"),
            "{}",
            error.message
        );
    }

    #[test]
    fn a_cancelled_operation_is_not_selected_or_provisioned() {
        let w = world();
        let id = w.submit("a", "contoso");
        let status = admin::run(
            &w.deployments(),
            &w.store,
            None,
            admin::Command::Status {
                operation: id.clone(),
            },
            NOW,
        )
        .unwrap();
        let operation = &status["operation"];
        let cancel = admin::Command::Act {
            operation: id.clone(),
            verb: "cancel".into(),
            request: json!({
                "idempotency_key": "c",
                "expected_operation_generation": operation["operation_generation"],
                "expected_lifecycle_generation": operation["lifecycle_generation"],
            })
            .to_string()
            .into_bytes(),
        };
        admin::run(&w.deployments(), &w.store, None, cancel, NOW).unwrap();
        let arm = FakeArm::default();
        assert!(w.work(&full(), &arm, None, "worker-1").unwrap().is_none());
        let done = w.attempt(&full(), &arm, Some(&id));
        refused(&done, Outcome::Blocked, "operation_not_provisionable");
        assert_eq!(arm.calls(), 0);
    }

    #[test]
    fn store_faults_and_unknown_ids_fail_closed_without_a_claim() {
        let w = world();
        let id = w.submit("a", "contoso");
        let arm = FakeArm::default();
        w.memory.fault(Op::Get, "journal.json", Fault::Refuse, 0, 1);
        let error = w.work(&full(), &arm, None, "worker-1").err().unwrap();
        assert_eq!(error.code, "operation_store_unavailable");
        w.memory.fault(Op::Get, "intent.json", Fault::Refuse, 0, 1);
        let error = w.work(&full(), &arm, None, "worker-1").err().unwrap();
        assert_eq!(error.code, "operation_store_unavailable");
        w.memory.fault(Op::Get, CONTROL, Fault::Refuse, 0, 1);
        let error = w.work(&full(), &arm, None, "worker-1").err().unwrap();
        assert_eq!(error.code, "control_state_unavailable");
        for bad in ["0", "../x", &id.to_uppercase(), &"0".repeat(32)] {
            let error = w.work(&full(), &arm, Some(bad), "worker-1").err().unwrap();
            assert_eq!(error.code, "operation_not_found", "{bad}");
        }
        assert!(w.journal_object(&id).is_none());
        assert_eq!(arm.calls(), 0);
    }

    #[test]
    fn attempt_ids_are_fresh_and_bounded() {
        let (a, b) = (attempt_id().unwrap(), attempt_id().unwrap());
        assert_ne!(a, b);
        assert!(a.starts_with("worker-") && is_hex(&a["worker-".len()..], 32));
    }
}
