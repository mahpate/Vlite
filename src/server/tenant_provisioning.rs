//! design-tenant-provisioning EPIC-001: durable operation contract.
//!
//! ITEM-001: one strict, typed intent per `(principal, idempotency key)`,
//! stored create-only under the operation ID with the exact content of every
//! referenced profile, rule and policy, so recovery needs only the operation
//! ID. The plaintext key is never stored. An identical retry returns the
//! same operation; a different request under the same key is refused.
//!
//! ITEM-002 lives in `control_state` (the per-tenant lifecycle slot); the
//! wrappers here run its transitions through the control-state CAS and
//! resolve lost acknowledgements by reading back, never by blind replay.
//!
//! ITEM-003: a journal records each external action before dispatch as
//! `uncertain` and settles it only from an observed result. Winning the
//! journal CAS is not proof that a former actuator stopped: taking over an
//! attempt needs an `ActuatorStopped` proof, which no production code can
//! produce yet (OD-002), so takeover fails closed. A terminal record is
//! sealed before the slot is released, and only after every recorded side
//! effect is settled.
//!
//! EPIC-002 (`admin`): the administrator routes and the `tenant-deployment`
//! CLI over this contract. No cloud action is dispatched (OD-002).
//!
//! EPIC-003: `projection` is the tenant-only, freshness-bound control view a
//! backend could serve a pending validator from (ITEM-006; no production
//! opt-in yet); `guard` is the durable target lifecycle guard checked by
//! manual and automatic failover and by the writer's authority claim
//! (ITEM-007); [`pin_scope_unproven`] keeps reader fleets denied (ITEM-008).
#![cfg_attr(not(test), allow(dead_code))]

use super::cloud_backup::{Cond, ObjectStore, StoreError};
use super::control_state::{self, Objects, Receipt, Route, Slot, Snapshot};
use super::*;
use serde::Serialize;
use std::collections::BTreeMap;

pub(super) use super::control_state::OperationKind as Kind;

pub(super) const SCHEMA: &str = "vlite.tenant-operation/1";
/// Same bound as the control-state object.
const MAX_BYTES: u64 = 1024 * 1024;
/// Journal compare-and-swap attempts before reporting contention.
const CAS_ATTEMPTS: usize = 4;

// ------------------------------------------------------------- payloads

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Payload {
    Create {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_vectors: Option<u64>,
        /// Content digests (SHA-256 hex) of retained inputs.
        writer_profile: String,
        initialization: String,
        resources: Vec<String>,
        secret_versions: Vec<String>,
    },
    SetReaders {
        tenant_id: String,
        fleet: Fleet,
    },
    Migrate {
        tenant_id: String,
        /// `control_state::route_digest` of the expected source route.
        expected_source_route: String,
        expected_source_lineage: u64,
        target_profile: String,
        resources: Vec<String>,
        migration_profile: String,
        verification_policy: String,
        onboarding_policy: String,
    },
    Retire {
        tenant_id: String,
        expected_receipt: String,
        reason: String,
        data_disposition: DataDisposition,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Fleet {
    Absent {},
    Present {
        min_replicas: u32,
        max_replicas: u32,
        reader_profile: String,
        scaling_rule: String,
        read_routing: control_state::ReadRouting,
    },
}

/// Only retention is defined; destructive disposition is a later decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum DataDisposition {
    Retain,
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn is_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn text(value: &str) -> bool {
    !value.is_empty() && value.len() <= 2048 && !value.chars().any(char::is_control)
}

fn invalid(message: &'static str) -> ApiError {
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "operation_request_invalid",
        message,
    )
}

impl Payload {
    pub(super) fn kind(&self) -> Kind {
        match self {
            Self::Create { .. } => Kind::Create,
            Self::SetReaders { .. } => Kind::SetReaders,
            Self::Migrate { .. } => Kind::Migrate,
            Self::Retire { .. } => Kind::Retire,
        }
    }

    fn tenant_id(&self) -> Option<&str> {
        match self {
            Self::Create { .. } => None,
            Self::SetReaders { tenant_id, .. }
            | Self::Migrate { tenant_id, .. }
            | Self::Retire { tenant_id, .. } => Some(tenant_id),
        }
    }

    /// The content digests this payload references, in field order.
    fn references(&self) -> Vec<&str> {
        match self {
            Self::Create {
                writer_profile,
                initialization,
                ..
            } => vec![writer_profile, initialization],
            Self::SetReaders {
                fleet:
                    Fleet::Present {
                        reader_profile,
                        scaling_rule,
                        ..
                    },
                ..
            } => vec![reader_profile, scaling_rule],
            Self::SetReaders { .. } | Self::Retire { .. } => Vec::new(),
            Self::Migrate {
                target_profile,
                migration_profile,
                verification_policy,
                onboarding_policy,
                ..
            } => vec![
                target_profile,
                migration_profile,
                verification_policy,
                onboarding_policy,
            ],
        }
    }

    pub(super) fn validate(&self) -> Result<(), ApiError> {
        let lists = |values: &[String]| values.len() <= 64 && values.iter().all(|v| text(v));
        let ok = self.tenant_id().is_none_or(|id| is_hex(id, 32))
            && self.references().iter().all(|d| is_hex(d, 64))
            && match self {
                Self::Create {
                    name,
                    max_vectors,
                    resources,
                    secret_versions,
                    ..
                } => {
                    text(name)
                        && name.len() <= 128
                        && max_vectors.is_none_or(|max| max > 0)
                        && lists(resources)
                        && lists(secret_versions)
                }
                Self::SetReaders { fleet, .. } => match fleet {
                    Fleet::Absent {} => true,
                    Fleet::Present {
                        min_replicas,
                        max_replicas,
                        ..
                    } => *max_replicas > 0 && min_replicas <= max_replicas,
                },
                Self::Migrate {
                    expected_source_route,
                    expected_source_lineage,
                    resources,
                    ..
                } => {
                    is_hex(expected_source_route, 64)
                        && *expected_source_lineage != 0
                        && lists(resources)
                }
                Self::Retire {
                    expected_receipt,
                    reason,
                    ..
                } => is_hex(expected_receipt, 64) && text(reason) && reason.len() <= 512,
            };
        if ok {
            Ok(())
        } else {
            Err(invalid("operation request fields are invalid"))
        }
    }
}

// --------------------------------------------------------------- intent

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Intent {
    pub(super) schema: String,
    pub(super) operation_id: String,
    pub(super) principal: String,
    /// SHA-256 of the idempotency key; the key itself is never stored.
    pub(super) idempotency_key_digest: String,
    pub(super) request_digest: String,
    /// The payload's tenant, or for `create` the ID allocated once here.
    pub(super) tenant_id: String,
    pub(super) payload: Payload,
    /// Digests of the retained content, sorted.
    pub(super) contents: Vec<String>,
    pub(super) created_unix_ms: u64,
}

impl Intent {
    pub(super) fn kind(&self) -> Kind {
        self.payload.kind()
    }
}

#[derive(Debug)]
pub(super) struct Accepted {
    pub(super) intent: Intent,
    /// The operation already existed with the same request.
    pub(super) duplicate: bool,
}

#[derive(Debug)]
pub(super) struct Loaded {
    pub(super) intent: Intent,
    pub(super) contents: BTreeMap<String, Vec<u8>>,
}

// -------------------------------------------------------------- journal

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Action {
    pub(super) resource: String,
    pub(super) operation: String,
    pub(super) request_digest: String,
    pub(super) destructive: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum StepOutcome {
    /// Recorded before dispatch; whether it took effect is unknown.
    Uncertain {},
    Settled {
        observed: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Step {
    pub(super) sequence: u64,
    pub(super) attempt: String,
    pub(super) action: Action,
    pub(super) outcome: StepOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Journal {
    pub(super) schema: String,
    pub(super) operation_id: String,
    pub(super) generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) attempt: Option<String>,
    pub(super) steps: Vec<Step>,
    /// Set by the sealing CAS once no step is uncertain; no further action
    /// may be recorded (so none can be dispatched) after it.
    pub(super) closed: bool,
}

/// What the actuator may do with an action it is about to dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Decision {
    /// Newly recorded as step `n`: dispatch it.
    Dispatch(u64),
    /// Step `n` is the same action with an unknown outcome: read the exact
    /// resource back and settle it; never dispatch it again blindly.
    Reconcile(u64),
    /// Step `n` already settled this action.
    Settled(u64),
}

/// Proof that a former actuator attempt can no longer dispatch (OD-002).
/// Only tests construct it today, so production takeover fails closed.
pub(super) struct ActuatorStopped {
    attempt: String,
}

#[cfg(test)]
impl ActuatorStopped {
    pub(super) fn proven_for_test(attempt: &str) -> Self {
        Self {
            attempt: attempt.to_owned(),
        }
    }
}

// ------------------------------------------------------------- terminal

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum TerminalOutcome {
    Succeeded { receipt: Receipt },
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Terminal {
    pub(super) schema: String,
    pub(super) operation_id: String,
    pub(super) tenant_id: String,
    pub(super) request_digest: String,
    pub(super) outcome: TerminalOutcome,
    pub(super) sealed_unix_ms: u64,
}

/// A terminal record known to be durably stored, with the digest of its
/// stored bytes. Only `Operations::seal` creates one.
#[derive(Clone, Debug)]
pub(super) struct Sealed {
    operation_id: String,
    tenant_id: String,
    digest: String,
    outcome: TerminalOutcome,
}

impl Sealed {
    pub(super) fn operation_id(&self) -> &str {
        &self.operation_id
    }
    pub(super) fn tenant_id(&self) -> &str {
        &self.tenant_id
    }
    pub(super) fn digest(&self) -> &str {
        &self.digest
    }
    pub(super) fn outcome(&self) -> &TerminalOutcome {
        &self.outcome
    }
}

// ----------------------------------------------------------- operations

fn indeterminate() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "operation_indeterminate",
        "operation write outcome is unknown; retry with the same idempotency key or operation ID",
    )
}

fn unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "operation_store_unavailable",
        "operation store could not be reached",
    )
}

fn state_invalid() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "operation_state_invalid",
        "stored operation state is invalid",
    )
}

fn conflict(code: &'static str, message: &'static str) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, code, message)
}

fn ended() -> ApiError {
    conflict("operation_terminal", "the operation has already ended")
}

/// Whether this payload would run readers, which pin sets (ITEM-008).
fn pins_sets(payload: &Payload) -> bool {
    matches!(
        payload,
        Payload::SetReaders {
            fleet: Fleet::Present { .. },
            ..
        }
    )
}

/// ITEM-008: reader fleets are denied until a reader's pin scope is proven.
///
/// A reader pins its set by rewriting the whole set record
/// (`{p}sets/{id}/record.json`, `Sets::pin`): the same object carries the
/// publication and deletion state, the manifest digest and the verification,
/// and it lives in the same coordination store and prefix as the writer
/// authority, the auto-failover state and the lifecycle guard. A Blob grant
/// is per object at best, so no grant on this layout allows the pin without
/// those writes, and offline fixtures prove neither Azure RBAC nor mount
/// enforcement. Bounded follow-up before ACA execution: broker pin and
/// release through the VLite control role as an operation-bound request, or
/// move pins to per-replica objects a reader may write alone; then qualify
/// the grants and mounts in the cloud.
fn pin_scope_unproven() -> ApiError {
    conflict(
        "pin_scope_unproven",
        "reader fleets are denied: reader pins rewrite the whole set record in the writer's coordination scope, and no tenant-scoped pin grant or brokered pin path is proven yet (ITEM-008); cancel to release the reservation",
    )
}

fn not_owner() -> ApiError {
    conflict(
        "actuator_not_owner",
        "another actuator attempt owns this operation",
    )
}

fn step_conflict() -> ApiError {
    conflict(
        "operation_step_conflict",
        "the step does not exist or already settled differently",
    )
}

/// Durable operation records under `{prefix}/`.
pub(super) struct Operations {
    store: Arc<dyn ObjectStore>,
    prefix: String,
}

impl Operations {
    pub(super) fn new(store: Arc<dyn ObjectStore>, prefix: impl Into<String>) -> Self {
        Self {
            store,
            prefix: prefix.into(),
        }
    }

    fn name(&self, operation_id: &str, file: &str) -> String {
        format!("{}/operations/{operation_id}/{file}", self.prefix)
    }

    fn content_name(&self, digest: &str) -> String {
        format!("{}/contents/{digest}", self.prefix)
    }

    /// The object, or None when it does not exist.
    fn get(&self, name: &str) -> Result<Option<(Vec<u8>, String)>, ApiError> {
        match self.store.get(name, MAX_BYTES) {
            Ok(blob) => Ok(Some((blob.bytes, blob.etag))),
            Err(StoreError::NotFound) => Ok(None),
            Err(StoreError::TooLarge) => Err(state_invalid()),
            Err(_) => Err(unavailable()),
        }
    }

    fn parse<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, ApiError> {
        serde_json::from_slice(bytes).map_err(|_| state_invalid())
    }

    /// Create-only write. `Ok(true)`: written by this call; `Ok(false)`: an
    /// object already exists (written by someone, possibly an earlier
    /// ambiguous attempt); the caller reads it back.
    fn create(&self, name: &str, bytes: &[u8]) -> Result<bool, ApiError> {
        match self.store.put(name, bytes, Cond::Create) {
            Ok(_) => Ok(true),
            Err(StoreError::Conflict) => Ok(false),
            Err(StoreError::Unknown(_)) => match self.get(name)? {
                Some(_) => Ok(false),
                None => Err(indeterminate()),
            },
            Err(_) => Err(unavailable()),
        }
    }

    pub(super) fn operation_id(principal: &str, key: &str) -> String {
        sha256_hex(format!("{SCHEMA}\0{principal}\0{key}").as_bytes())[..32].to_owned()
    }

    /// ITEM-001: accepts or reconciles the operation for `(principal, key)`.
    /// `contents` must be exactly the content the payload references.
    pub(super) fn accept(
        &self,
        principal: &str,
        key: &str,
        payload: &Payload,
        contents: &[Vec<u8>],
        now: u64,
    ) -> Result<Accepted, ApiError> {
        if !text(principal) || !text(key) || key.len() > 256 {
            return Err(invalid("principal and idempotency key are required"));
        }
        payload.validate()?;
        let mut supplied = BTreeMap::new();
        for content in contents {
            if content.len() as u64 > MAX_BYTES {
                return Err(invalid("referenced content is too large"));
            }
            if supplied.insert(sha256_hex(content), content).is_some() {
                return Err(invalid("referenced content is duplicated"));
            }
        }
        let mut references: Vec<String> = payload
            .references()
            .into_iter()
            .map(str::to_owned)
            .collect();
        references.sort();
        references.dedup();
        if !supplied.keys().eq(references.iter()) {
            return Err(invalid(
                "supplied content must match the referenced digests exactly",
            ));
        }
        let operation_id = Self::operation_id(principal, key);
        let request_digest = sha256_hex(&serde_json::to_vec(payload).map_err(|_| state_invalid())?);
        let name = self.name(&operation_id, "intent.json");
        let check = |stored: Intent| {
            if stored.principal == principal && stored.request_digest == request_digest {
                Ok(stored)
            } else {
                Err(conflict(
                    "operation_request_conflict",
                    "the idempotency key was already used for a different request",
                ))
            }
        };
        if let Some((bytes, _)) = self.get(&name)? {
            return Ok(Accepted {
                intent: check(Self::parse(&bytes)?)?,
                duplicate: true,
            });
        }
        for (digest, content) in &supplied {
            let content_name = self.content_name(digest);
            if !self.create(&content_name, content)? {
                let stored = self.get(&content_name)?.ok_or_else(indeterminate)?;
                if sha256_hex(&stored.0) != **digest {
                    return Err(ApiError::new(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "operation_input_unavailable",
                        "retained operation content is corrupt",
                    ));
                }
            }
        }
        let tenant_id = match payload.tenant_id() {
            Some(id) => id.to_owned(),
            None => {
                let mut bytes = [0u8; 16];
                getrandom::fill(&mut bytes).map_err(|_| unavailable())?;
                hex(&bytes)
            }
        };
        let intent = Intent {
            schema: SCHEMA.to_owned(),
            operation_id,
            principal: principal.to_owned(),
            idempotency_key_digest: sha256_hex(key.as_bytes()),
            request_digest: request_digest.clone(),
            tenant_id,
            payload: payload.clone(),
            contents: references,
            created_unix_ms: now,
        };
        let bytes = serde_json::to_vec(&intent).map_err(|_| state_invalid())?;
        if self.create(&name, &bytes)? {
            return Ok(Accepted {
                intent,
                duplicate: false,
            });
        }
        let (stored, _) = self.get(&name)?.ok_or_else(indeterminate)?;
        let stored = check(Self::parse(&stored)?)?;
        Ok(Accepted {
            duplicate: stored != intent,
            intent: stored,
        })
    }

    /// The intent and its verified retained content, from the ID alone.
    pub(super) fn load(&self, operation_id: &str) -> Result<Loaded, ApiError> {
        let (bytes, _) = self
            .get(&self.name(operation_id, "intent.json"))?
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::NOT_FOUND,
                    "operation_not_found",
                    "no such operation",
                )
            })?;
        let intent: Intent = Self::parse(&bytes)?;
        if intent.schema != SCHEMA || intent.operation_id != operation_id {
            return Err(state_invalid());
        }
        let mut contents = BTreeMap::new();
        for digest in &intent.contents {
            let content = self
                .get(&self.content_name(digest))
                .ok()
                .flatten()
                .map(|(bytes, _)| bytes)
                .filter(|bytes| sha256_hex(bytes) == *digest)
                .ok_or_else(|| {
                    ApiError::new(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "operation_input_unavailable",
                        "retained operation content is missing or corrupt",
                    )
                })?;
            contents.insert(digest.clone(), content);
        }
        Ok(Loaded { intent, contents })
    }

    // ------------------------------------------------------- journal

    fn read_journal(&self, operation_id: &str) -> Result<(Journal, Option<String>), ApiError> {
        match self.get(&self.name(operation_id, "journal.json"))? {
            Some((bytes, etag)) => {
                let journal: Journal = Self::parse(&bytes)?;
                if journal.schema != SCHEMA || journal.operation_id != operation_id {
                    return Err(state_invalid());
                }
                Ok((journal, Some(etag)))
            }
            None => Ok((
                Journal {
                    schema: SCHEMA.to_owned(),
                    operation_id: operation_id.to_owned(),
                    generation: 0,
                    attempt: None,
                    steps: Vec::new(),
                    closed: false,
                },
                None,
            )),
        }
    }

    pub(super) fn journal(&self, operation_id: &str) -> Result<Journal, ApiError> {
        Ok(self.read_journal(operation_id)?.0)
    }

    /// One journal CAS. `change` returns the next journal (or None: nothing
    /// to write) plus its result. An unknown write outcome is reported; the
    /// caller retries the same call, which reads back first.
    fn update_journal<R>(
        &self,
        operation_id: &str,
        mut change: impl FnMut(&Journal) -> Result<(Option<Journal>, R), ApiError>,
    ) -> Result<R, ApiError> {
        let name = self.name(operation_id, "journal.json");
        for _ in 0..CAS_ATTEMPTS {
            let (current, etag) = self.read_journal(operation_id)?;
            let (next, result) = change(&current)?;
            let Some(mut next) = next else {
                return Ok(result);
            };
            next.generation = current.generation + 1;
            let bytes = serde_json::to_vec(&next).map_err(|_| state_invalid())?;
            let cond = match &etag {
                Some(etag) => Cond::Match(etag),
                None => Cond::Create,
            };
            match self.store.put(&name, &bytes, cond) {
                Ok(_) => return Ok(result),
                Err(StoreError::Conflict) => continue,
                Err(StoreError::Unknown(_)) => return Err(indeterminate()),
                Err(_) => return Err(unavailable()),
            }
        }
        Err(conflict(
            "operation_journal_conflict",
            "the operation journal is changing too often; retry later",
        ))
    }

    /// Claims the operation for actuator `attempt`. A first claim is free;
    /// replacing another attempt needs proof that it stopped.
    pub(super) fn claim(
        &self,
        operation_id: &str,
        attempt: &str,
        prior: Option<&ActuatorStopped>,
    ) -> Result<(), ApiError> {
        if !text(attempt) || attempt.len() > 128 {
            return Err(invalid("actuator attempt is invalid"));
        }
        self.update_journal(operation_id, |journal| match journal.attempt.as_deref() {
            Some(owner) if owner == attempt => Ok((None, ())),
            Some(owner) if prior.is_none_or(|proof| proof.attempt != owner) => Err(conflict(
                "actuator_fence_unproven",
                "the current actuator attempt is not proven stopped",
            )),
            _ => {
                let mut next = journal.clone();
                next.attempt = Some(attempt.to_owned());
                Ok((Some(next), ()))
            }
        })
    }

    /// Records `action` before dispatch (ITEM-003).
    pub(super) fn record(
        &self,
        operation_id: &str,
        attempt: &str,
        action: &Action,
    ) -> Result<Decision, ApiError> {
        if !text(&action.resource)
            || !text(&action.operation)
            || !is_hex(&action.request_digest, 64)
        {
            return Err(invalid("action fields are invalid"));
        }
        self.update_journal(operation_id, |journal| {
            if journal.closed {
                return Err(ended());
            }
            if journal.attempt.as_deref() != Some(attempt) {
                return Err(not_owner());
            }
            if let Some(step) = journal
                .steps
                .iter()
                .find(|step| step.outcome == StepOutcome::Uncertain {})
            {
                return if step.action == *action {
                    Ok((None, Decision::Reconcile(step.sequence)))
                } else {
                    Err(conflict(
                        "prior_action_unknown",
                        "an earlier action's outcome is unknown; reconcile it first",
                    ))
                };
            }
            if let Some(step) = journal.steps.iter().rev().find(|s| s.action == *action) {
                return Ok((None, Decision::Settled(step.sequence)));
            }
            let sequence = journal.steps.len() as u64 + 1;
            let mut next = journal.clone();
            next.steps.push(Step {
                sequence,
                attempt: attempt.to_owned(),
                action: action.clone(),
                outcome: StepOutcome::Uncertain {},
            });
            Ok((Some(next), Decision::Dispatch(sequence)))
        })
    }

    /// Settles step `sequence` from an observed result. Idempotent for the
    /// same observation; a settled step never changes.
    pub(super) fn settle(
        &self,
        operation_id: &str,
        attempt: &str,
        sequence: u64,
        observed: &str,
    ) -> Result<(), ApiError> {
        if !text(observed) {
            return Err(invalid("observed result is invalid"));
        }
        self.update_journal(operation_id, |journal| {
            if journal.attempt.as_deref() != Some(attempt) {
                return Err(not_owner());
            }
            let index = journal
                .steps
                .iter()
                .position(|step| step.sequence == sequence)
                .ok_or_else(step_conflict)?;
            match &journal.steps[index].outcome {
                StepOutcome::Settled { observed: same } if same == observed => Ok((None, ())),
                StepOutcome::Settled { .. } => Err(step_conflict()),
                StepOutcome::Uncertain {} => {
                    let mut next = journal.clone();
                    next.steps[index].outcome = StepOutcome::Settled {
                        observed: observed.to_owned(),
                    };
                    Ok((Some(next), ()))
                }
            }
        })
    }

    // ------------------------------------------------------ terminal

    pub(super) fn terminal(&self, operation_id: &str) -> Result<Option<Terminal>, ApiError> {
        Ok(self
            .read_terminal(operation_id)?
            .map(|(terminal, _)| terminal))
    }

    fn read_terminal(&self, operation_id: &str) -> Result<Option<(Terminal, String)>, ApiError> {
        let Some((bytes, _)) = self.get(&self.name(operation_id, "terminal.json"))? else {
            return Ok(None);
        };
        let terminal: Terminal = Self::parse(&bytes)?;
        if terminal.schema != SCHEMA || terminal.operation_id != operation_id {
            return Err(state_invalid());
        }
        Ok(Some((terminal, sha256_hex(&bytes))))
    }

    /// Seals the terminal record once from the published lifecycle slot.
    fn seal(&self, intent: &Intent, snapshot: &Snapshot, now: u64) -> Result<Sealed, ApiError> {
        let sealed = |(terminal, digest): (Terminal, String)| {
            if terminal.tenant_id != intent.tenant_id
                || terminal.request_digest != intent.request_digest
            {
                return Err(state_invalid());
            }
            Ok(Sealed {
                operation_id: terminal.operation_id,
                tenant_id: terminal.tenant_id,
                digest,
                outcome: terminal.outcome,
            })
        };
        if let Some(stored) = self.read_terminal(&intent.operation_id)? {
            return sealed(stored);
        }
        let not_terminal = || {
            conflict(
                "operation_not_terminal",
                "the operation has neither activated nor been cancelled",
            )
        };
        let outcome = match snapshot.lifecycle(&intent.tenant_id).map(|e| &e.slot) {
            Some(Slot::Reserved {
                operation_id,
                receipt,
                cancellation_requested,
                ..
            }) if *operation_id == intent.operation_id => match receipt {
                Some(receipt) => TerminalOutcome::Succeeded {
                    receipt: receipt.clone(),
                },
                None if *cancellation_requested => TerminalOutcome::Cancelled,
                None => return Err(not_terminal()),
            },
            _ => return Err(not_terminal()),
        };
        // Close the journal by CAS so no action can be recorded (and so
        // dispatched) between this check and the terminal write.
        self.update_journal(&intent.operation_id, |journal| {
            if journal
                .steps
                .iter()
                .any(|step| step.outcome == StepOutcome::Uncertain {})
            {
                return Err(conflict(
                    "cleanup_pending",
                    "a recorded side effect is unresolved; reconcile it before ending the operation",
                ));
            }
            if journal.closed {
                return Ok((None, ()));
            }
            let mut next = journal.clone();
            next.closed = true;
            Ok((Some(next), ()))
        })?;
        let terminal = Terminal {
            schema: SCHEMA.to_owned(),
            operation_id: intent.operation_id.clone(),
            tenant_id: intent.tenant_id.clone(),
            request_digest: intent.request_digest.clone(),
            outcome,
            sealed_unix_ms: now,
        };
        let bytes = serde_json::to_vec(&terminal).map_err(|_| state_invalid())?;
        self.create(&self.name(&intent.operation_id, "terminal.json"), &bytes)?;
        let stored = self
            .read_terminal(&intent.operation_id)?
            .ok_or_else(indeterminate)?;
        if stored.0.outcome != terminal.outcome {
            return Err(conflict(
                "operation_terminal_conflict",
                "a different terminal record is already sealed",
            ));
        }
        sealed(stored)
    }

    /// Seals the terminal record, then releases the lifecycle slot.
    pub(super) fn release(
        &self,
        objects: &Objects,
        intent: &Intent,
        now: u64,
    ) -> Result<Sealed, ApiError> {
        let (snapshot, _) = objects.read()?;
        let released = |snapshot: &Snapshot, sealed: &Sealed| {
            matches!(
                snapshot.lifecycle(&intent.tenant_id).map(|e| &e.slot),
                Some(Slot::Released { operation_id, terminal_digest })
                    if *operation_id == intent.operation_id && terminal_digest == sealed.digest()
            )
        };
        let sealed = self.seal(intent, &snapshot, now)?;
        lifecycle_cas(
            objects,
            now,
            |snapshot| released(snapshot, &sealed),
            |current| current.release_lifecycle(&sealed),
        )?;
        Ok(sealed)
    }
}

// ------------------------------------------------------- lifecycle CAS

/// Runs one lifecycle transition through the control-state CAS. `done`
/// says whether the published state already reflects it: checked first (no
/// write), and after any failure by reading back, so a lost acknowledgement
/// is resolved even after later, unrelated generations.
fn lifecycle_cas(
    objects: &Objects,
    now: u64,
    done: impl Fn(&Snapshot) -> bool,
    change: impl FnMut(&Snapshot) -> Result<Snapshot, ApiError>,
) -> Result<Snapshot, ApiError> {
    let (current, _) = objects.read()?;
    if done(&current) {
        return Ok(current);
    }
    match objects.update(now, change) {
        Ok(next) => Ok(next),
        Err(error) => {
            if let Ok((current, _)) = objects.read()
                && done(&current)
            {
                return Ok(current);
            }
            Err(match error.code {
                "control_state_indeterminate" => indeterminate(),
                "control_state_conflict" => conflict(
                    "lifecycle_conflict",
                    "control state is changing too often; retry later",
                ),
                _ => error,
            })
        }
    }
}

fn holds<'a>(snapshot: &'a Snapshot, intent: &Intent) -> Option<&'a Slot> {
    snapshot
        .lifecycle(&intent.tenant_id)
        .map(|entry| &entry.slot)
        .filter(|slot| {
            matches!(slot, Slot::Reserved { operation_id, .. } if *operation_id == intent.operation_id)
        })
}

/// ITEM-002: reserves the tenant's slot for the operation. An operation
/// that has sealed its terminal record can never reserve again.
pub(super) fn reserve(
    ops: &Operations,
    objects: &Objects,
    intent: &Intent,
    now: u64,
) -> Result<(), ApiError> {
    if ops.read_terminal(&intent.operation_id)?.is_some() {
        return Err(ended());
    }
    lifecycle_cas(
        objects,
        now,
        |snapshot| holds(snapshot, intent).is_some(),
        |current| {
            current.reserve(
                &intent.tenant_id,
                &intent.operation_id,
                intent.kind(),
                &intent.request_digest,
            )
        },
    )
    .map(drop)
}

/// ITEM-003: requests cancellation. Linearized with `activate` by the
/// control-state CAS: exactly one of them wins.
pub(super) fn request_cancel(objects: &Objects, intent: &Intent, now: u64) -> Result<(), ApiError> {
    lifecycle_cas(
        objects,
        now,
        |snapshot| {
            matches!(
                holds(snapshot, intent),
                Some(Slot::Reserved {
                    cancellation_requested: true,
                    ..
                })
            )
        },
        |current| current.request_cancel(&intent.tenant_id, &intent.operation_id),
    )
    .map(drop)
}

/// ITEM-002: publishes the route and its receipt in one generation and
/// returns the receipt (the same one on every retry).
pub(super) fn activate(
    ops: &Operations,
    objects: &Objects,
    intent: &Intent,
    route: &Route,
    proof_digest: &str,
    now: u64,
) -> Result<Receipt, ApiError> {
    if ops.read_terminal(&intent.operation_id)?.is_some() {
        return Err(ended());
    }
    let digest = control_state::route_digest(route);
    let receipt = |snapshot: &Snapshot| match holds(snapshot, intent) {
        Some(Slot::Reserved {
            receipt: Some(receipt),
            ..
        }) if receipt.route_digest == digest && receipt.proof_digest == proof_digest => {
            Some(receipt.clone())
        }
        _ => None,
    };
    let snapshot = lifecycle_cas(
        objects,
        now,
        |snapshot| receipt(snapshot).is_some(),
        |current| current.activate(&intent.tenant_id, &intent.operation_id, route, proof_digest),
    )?;
    receipt(&snapshot).ok_or_else(indeterminate)
}

// ---------------------------------------------------------------- routes

mod admin;
pub(super) mod guard;
pub(super) mod projection;

pub(super) use admin::Deployments;
pub use admin::{TenantDeployments, TenantDeploymentsConfig, tenant_deployment_command};

/// design-tenant-provisioning ITEM-004: the managed tenant-deployment routes.
pub(super) fn routes() -> Router<Service> {
    Router::new()
        .route("/v1/admin/tenant-deployments", post(admin::create))
        .route(
            "/v1/admin/tenant-deployments/{tenant}/readers",
            post(admin::readers),
        )
        .route(
            "/v1/admin/tenant-deployments/{tenant}/migrations",
            post(admin::migrate),
        )
        .route(
            "/v1/admin/tenant-deployments/{tenant}/retire",
            post(admin::retire),
        )
        .route(
            "/v1/admin/tenant-deployment-operations/{operation}",
            get(admin::operation_status),
        )
        .route(
            "/v1/admin/tenant-deployment-operations/{operation}/{action}",
            post(admin::operation_action),
        )
}

#[cfg(test)]
mod tests;
