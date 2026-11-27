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
use std::collections::{BTreeMap, BTreeSet};

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

pub(super) fn is_hex(value: &str, len: usize) -> bool {
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
    /// EPIC-004 ITEM-009: the cloud's asynchronous operation ID, persisted
    /// as soon as a dispatch returns it, so a resumed actuator polls it
    /// instead of dispatching again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) arm_operation: Option<String>,
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
    Succeeded {
        receipt: Receipt,
    },
    Cancelled,
    /// EPIC-004 ITEM-011: a `retire` operation removed the route and
    /// stopped the tenant's apps. Data is always retained.
    Retired,
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

const INDETERMINATE: &str = "operation_indeterminate";

/// Whether `error` leaves an outcome unknown: an operation-store write that
/// may have landed, or a cloud action that may have taken effect. Such an
/// error must be reconciled, never read as a refusal.
pub(super) fn outcome_unknown(error: &ApiError) -> bool {
    matches!(error.code, INDETERMINATE | aca::OUTCOME_UNKNOWN)
}

fn indeterminate() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        INDETERMINATE,
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

/// ITEM-008: reader fleets remain denied until production pin-scope proofs
/// and effective grant and mount enforcement are qualified. Isolated reader
/// pin objects and an opt-in runtime path exist, but their scope proofs are
/// test-only; the legacy record-pin path remains the default.
fn pin_scope_unproven() -> ApiError {
    conflict(
        "pin_scope_unproven",
        "reader fleets are denied: isolated reader-pin objects and an opt-in runtime path exist, but production pin-scope proofs and effective grant and mount enforcement are not qualified (ITEM-008); cancel to release the reservation",
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
        if let Payload::Create { writer_profile, .. } = payload {
            aca::writer_input(supplied[writer_profile])?;
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

    /// ITEM-023: up to `count` operation IDs above `after`, ascending. An
    /// operation exists iff its intent does, as [`Self::load`] decides;
    /// every other name under the prefix is ignored. Each call lists the
    /// whole prefix (`after` and `count` only trim the result), so the
    /// store's listing cap also caps how many operations can be listed.
    pub(super) fn ids(&self, after: Option<&str>, count: usize) -> Result<Vec<String>, ApiError> {
        let root = format!("{}/operations/", self.prefix);
        let names = self.store.list(&root).map_err(|_| unavailable())?;
        let ids: BTreeSet<&str> = names
            .iter()
            .filter_map(|name| name.strip_prefix(&root)?.strip_suffix("/intent.json"))
            .filter(|id| is_hex(id, 32) && after.is_none_or(|after| *id > after))
            .collect();
        Ok(ids.into_iter().take(count).map(str::to_owned).collect())
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
                arm_operation: None,
            });
            Ok((Some(next), Decision::Dispatch(sequence)))
        })
    }

    /// ITEM-009: persists the cloud operation ID a dispatch of step
    /// `sequence` returned. Idempotent; a different ID is refused.
    pub(super) fn observe(
        &self,
        operation_id: &str,
        attempt: &str,
        sequence: u64,
        arm_operation: &str,
    ) -> Result<(), ApiError> {
        if !text(arm_operation) {
            return Err(invalid("cloud operation ID is invalid"));
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
            match journal.steps[index].arm_operation.as_deref() {
                Some(same) if same == arm_operation => Ok((None, ())),
                Some(_) => Err(step_conflict()),
                None if journal.steps[index].outcome != StepOutcome::Uncertain {} => {
                    Err(step_conflict())
                }
                None => {
                    let mut next = journal.clone();
                    next.steps[index].arm_operation = Some(arm_operation.to_owned());
                    Ok((Some(next), ()))
                }
            }
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
                kind,
                receipt,
                cancellation_requested,
                ..
            }) if *operation_id == intent.operation_id => match receipt {
                Some(receipt) => TerminalOutcome::Succeeded {
                    receipt: receipt.clone(),
                },
                None if *cancellation_requested => TerminalOutcome::Cancelled,
                // ITEM-011: a retirement ends once its route is removed;
                // `aca::terminal_ready` below also requires the stop proof.
                None if *kind == Kind::Retire
                    && snapshot.record_for(&intent.tenant_id).is_none() =>
                {
                    TerminalOutcome::Retired
                }
                None => return Err(not_terminal()),
            },
            // EPIC-005: withdrawn before it ever reserved; it never acted.
            _ if snapshot.withdrawn(&intent.tenant_id, &intent.operation_id) => {
                TerminalOutcome::Cancelled
            }
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
            aca::terminal_ready(journal, &intent.payload, &outcome)?;
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

    /// Seals the terminal record, then releases the lifecycle slot. A
    /// withdrawn operation (EPIC-005) holds none: the slot stays as it is.
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
            ) || (*sealed.outcome() == TerminalOutcome::Cancelled
                && snapshot.withdrawn(&intent.tenant_id, &intent.operation_id))
        };
        let sealed = self.seal(intent, &snapshot, now)?;
        lifecycle_cas(
            objects,
            now,
            |snapshot| released(snapshot, &sealed),
            |current| current.release_lifecycle(&sealed, now),
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

/// ITEM-002: reserves the tenant's slot for the operation, unbound to any
/// plan. An operation that has sealed its terminal record can never reserve
/// again. Test fixture only (R1): production reserves `create` with its
/// tenant metadata in `admin`, and every other kind only through
/// [`reserve_after`], bound to the predecessor and route its plan read.
#[cfg(test)]
pub(super) fn reserve(
    ops: &Operations,
    objects: &Objects,
    intent: &Intent,
    now: u64,
) -> Result<(), ApiError> {
    reserve_with(ops, objects, intent, now, |current| {
        current.reserve(
            &intent.tenant_id,
            &intent.operation_id,
            intent.kind(),
            &intent.request_digest,
        )
    })
}

/// EPIC-005: [`reserve`] bound, in the same CAS, to the released operation
/// (`released`: its ID and terminal digest) and route digest the
/// operation's plan was verified against. A stale plan takes no slot:
/// `lifecycle_anchor_changed`.
pub(super) fn reserve_after(
    ops: &Operations,
    objects: &Objects,
    intent: &Intent,
    released: (&str, &str),
    route: &str,
    now: u64,
) -> Result<(), ApiError> {
    reserve_with(ops, objects, intent, now, |current| {
        current.reserve_after(
            &intent.tenant_id,
            &intent.operation_id,
            intent.kind(),
            &intent.request_digest,
            released,
            route,
        )
    })
}

fn reserve_with(
    ops: &Operations,
    objects: &Objects,
    intent: &Intent,
    now: u64,
    change: impl FnMut(&Snapshot) -> Result<Snapshot, ApiError>,
) -> Result<(), ApiError> {
    if ops.read_terminal(&intent.operation_id)?.is_some() {
        return Err(ended());
    }
    lifecycle_cas(
        objects,
        now,
        |snapshot| holds(snapshot, intent).is_some(),
        change,
    )
    .map(drop)
}

/// ITEM-003: requests cancellation. Linearized with `activate` and
/// [`reserve_after`] by the control-state CAS: exactly one of them wins.
/// EPIC-005: a reader change or retirement that holds no slot is withdrawn
/// in that same CAS (it can then never reserve); if its reservation wins,
/// the retried CAS cancels it through that reservation instead.
pub(super) fn request_cancel(
    ops: &Operations,
    objects: &Objects,
    intent: &Intent,
    now: u64,
) -> Result<(), ApiError> {
    let withdrawable = matches!(intent.kind(), Kind::SetReaders | Kind::Retire);
    // ITEM-013: read before the CAS; the journal only grows, and the
    // update step is recorded before it is sent, after the withdrawal.
    let (endpoint, dispatched) = match intent.kind() {
        Kind::SetReaders => {
            aca::reader_cancel_facts(&ops.journal(&intent.operation_id)?, &intent.payload)
        }
        _ => (None, false),
    };
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
            ) || (withdrawable && snapshot.withdrawn(&intent.tenant_id, &intent.operation_id))
        },
        |current| {
            if !withdrawable || holds(current, intent).is_some() {
                return current.request_cancel_readers(
                    &intent.tenant_id,
                    &intent.operation_id,
                    now,
                    endpoint.as_deref(),
                    dispatched,
                );
            }
            // Read after `current`: a slot is released only after its
            // holder sealed, so no terminal means this never held it.
            if ops.read_terminal(&intent.operation_id)?.is_some() {
                return Err(ended());
            }
            current.withdraw(&intent.tenant_id, &intent.operation_id)
        },
    )
    .map(drop)
}

/// Fixture setup for tests that start with an activated route.
#[cfg(test)]
fn activate(
    ops: &Operations,
    objects: &Objects,
    intent: &Intent,
    route: &Route,
    proof_digest: &str,
    now: u64,
) -> Result<Receipt, ApiError> {
    activate_checked(ops, objects, intent, route, proof_digest, now, |_| Ok(()))
}

/// The credential checks and the route/receipt write share a CAS generation.
fn activate_checked(
    ops: &Operations,
    objects: &Objects,
    intent: &Intent,
    route: &Route,
    proof_digest: &str,
    now: u64,
    mut validate: impl FnMut(&Snapshot) -> Result<(), ApiError>,
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
        |current| {
            validate(current)?;
            current.activate(&intent.tenant_id, &intent.operation_id, route, proof_digest)
        },
    )?;
    receipt(&snapshot).ok_or_else(indeterminate)
}

/// EPIC-005 ITEM-012/013: republishes the reserved route (`planned`) with
/// the reader endpoint `reader` as `target`, writer-only; with `proof`, the
/// receipt in the same generation. Already done when the route is `target`
/// (and, with `proof`, the held receipt is this one). A slot this operation
/// already released is not done here: the caller returns the sealed outcome.
#[allow(clippy::too_many_arguments)]
pub(super) fn publish_readers(
    ops: &Operations,
    objects: &Objects,
    intent: &Intent,
    planned: &str,
    target: &str,
    reader: Option<&str>,
    proof: Option<&str>,
    now: u64,
) -> Result<(), ApiError> {
    if ops.read_terminal(&intent.operation_id)?.is_some() {
        return Err(ended());
    }
    let done = |snapshot: &Snapshot| match (holds(snapshot, intent), proof) {
        (Some(Slot::Reserved { receipt, .. }), proof) => match (receipt, proof) {
            (Some(receipt), Some(proof)) => {
                receipt.route_digest == target && receipt.proof_digest == proof
            }
            (_, None) => {
                snapshot
                    .record_for(&intent.tenant_id)
                    .map(control_state::route_digest)
                    .as_deref()
                    == Some(target)
            }
            (None, Some(_)) => false,
        },
        _ => false,
    };
    lifecycle_cas(objects, now, done, |current| {
        current.readers_route(
            &intent.tenant_id,
            &intent.operation_id,
            planned,
            target,
            reader,
            proof,
        )
    })
    .map(drop)
}

/// EPIC-005 ITEM-013: withdraws the reader endpoint `endpoint` from the
/// reserved route `planned` before the reader app is updated in place
/// (`Snapshot::withdraw_reader`). Already done when this operation still
/// holds its unsealed reservation over the withdrawn route.
pub(super) fn withdraw_readers(
    ops: &Operations,
    objects: &Objects,
    intent: &Intent,
    planned: &str,
    endpoint: &str,
    now: u64,
) -> Result<(), ApiError> {
    if ops.read_terminal(&intent.operation_id)?.is_some() {
        return Err(ended());
    }
    let done = |snapshot: &Snapshot| {
        matches!(
            holds(snapshot, intent),
            Some(Slot::Reserved {
                receipt: None,
                cancellation_requested: false,
                expected_route: Some(expected),
                ..
            }) if expected == planned
        ) && snapshot
            .record_for(&intent.tenant_id)
            .is_some_and(|route| control_state::reader_withdrawn(route, planned, endpoint))
    };
    lifecycle_cas(objects, now, done, |current| {
        current.withdraw_reader(&intent.tenant_id, &intent.operation_id, planned, endpoint)
    })
    .map(drop)
}

// ---------------------------------------------------- ITEM-010 create

use super::authority::{self, AuthorityError, AuthorityStore};
use guard::Guards;

/// The new tenant's database seed: its nonzero lineage and the digest of
/// the seeded content. Recorded once; never overwritten or re-keyed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Seed {
    pub(super) lineage: u64,
    pub(super) digest: String,
}

const SEED: &str = "seed";

fn seed_action(digest: &str) -> Action {
    Action {
        resource: SEED.to_owned(),
        operation: SEED.to_owned(),
        request_digest: digest.to_owned(),
        destructive: false,
    }
}

fn seed_observed(seed: &Seed) -> String {
    format!("lineage:{:016x} sha256:{}", seed.lineage, seed.digest)
}

/// The settled seed of a journal, if any.
pub(super) fn recorded_seed(journal: &Journal) -> Option<Seed> {
    journal.steps.iter().find_map(|step| match &step.outcome {
        StepOutcome::Settled { observed } if step.action.resource == SEED => {
            let (lineage, digest) = observed.strip_prefix("lineage:")?.split_once(" sha256:")?;
            Some(Seed {
                lineage: u64::from_str_radix(lineage, 16).ok()?,
                digest: digest.to_owned(),
            })
        }
        _ => None,
    })
}

fn seed_unrecorded() -> ApiError {
    conflict(
        "seed_unrecorded",
        "the tenant's database seed is not recorded yet",
    )
}

fn proof_refused() -> ApiError {
    conflict(
        "readiness_unproven",
        "the writer, archive, authority or policy state does not prove readiness",
    )
}

impl Operations {
    /// ITEM-010: records the seed the operation produced. A different seed
    /// for the same operation is refused: lineage is never re-keyed.
    pub(super) fn record_seed(
        &self,
        operation_id: &str,
        attempt: &str,
        seed: &Seed,
    ) -> Result<(), ApiError> {
        if seed.lineage == 0 || !is_hex(&seed.digest, 64) {
            return Err(invalid("seed is invalid"));
        }
        let journal = self.journal(operation_id)?;
        if journal
            .steps
            .iter()
            .any(|step| step.action.resource == SEED && step.action != seed_action(&seed.digest))
            || recorded_seed(&journal).is_some_and(|recorded| recorded != *seed)
        {
            return Err(conflict(
                "seed_mismatch",
                "a different seed is already recorded for this operation",
            ));
        }
        match self.record(operation_id, attempt, &seed_action(&seed.digest))? {
            Decision::Dispatch(n) | Decision::Reconcile(n) => self
                .settle(operation_id, attempt, n, &seed_observed(seed))
                .map_err(|error| match error.code {
                    "step_conflict" => conflict(
                        "seed_mismatch",
                        "a different seed is already recorded for this operation",
                    ),
                    _ => error,
                }),
            Decision::Settled(_) => Ok(()),
        }
    }
}

/// ITEM-010: holds the target's lifecycle guard, creates (or finds) its
/// authority record bound to the recorded seed lineage, verifies it and
/// enables the writer's claim. An existing record with any other
/// identity is refused, never reset. The route stays absent, so no
/// client write is admitted until [`activate_create`].
pub(super) fn establish_authority(
    ops: &Operations,
    objects: &Objects,
    authority: &AuthorityStore,
    guards: &Guards,
    intent: &Intent,
    installation: &str,
    scope: &str,
) -> Result<authority::Observed, ApiError> {
    if intent.kind() != Kind::Create {
        return Err(invalid("only a create operation establishes authority"));
    }
    let (snapshot, _) = objects.read()?;
    if !matches!(
        holds(&snapshot, intent),
        Some(Slot::Reserved {
            cancellation_requested: false,
            ..
        })
    ) || snapshot.record_for(&intent.tenant_id).is_some()
    {
        return Err(conflict(
            "operation_not_provisionable",
            "the operation does not hold an uncancelled reservation of an unrouted tenant",
        ));
    }
    let seed = recorded_seed(&ops.journal(&intent.operation_id)?).ok_or_else(seed_unrecorded)?;
    // A rerun after writes were enabled must not move the guard backwards.
    let enabled = guards.current()?.is_some_and(|guard| {
        guard.tenant == intent.tenant_id
            && guard.operation_id == intent.operation_id
            && guard.state == guard::State::WriteEnabled
    });
    if !enabled {
        guards.hold(&intent.tenant_id, &intent.operation_id)?;
    }
    let lineage = format!("{:016x}", seed.lineage);
    let observed = match authority.read() {
        Ok(observed) => observed,
        Err(AuthorityError::Missing) => match authority.create(installation, scope, &lineage) {
            Err(AuthorityError::Exists) => authority.read()?,
            created => created?,
        },
        Err(error) => return Err(error.into()),
    };
    let record = &observed.record;
    if record.installation_id != installation
        || record.scope_id != scope
        || record.lineage != lineage
    {
        return Err(conflict(
            "authority_identity_mismatch",
            "the target's authority belongs to another installation, scope or lineage",
        ));
    }
    guards.enable_writes(&intent.tenant_id, &intent.operation_id)?;
    Ok(observed)
}

/// What the operation observed of the new writer and its policy.
struct Readiness {
    /// The archive manifest the writer's first checkpoint published.
    archive_manifest_sha256: String,
    /// The writer app, read back through the ARM port.
    writer: aca::Observed,
    policy_generation_required: u64,
    policy_generation_observed: u64,
    /// The exact writer-only route the readiness observation covered; the
    /// activation may publish only this route.
    route: Route,
}

/// ITEM-010 readiness proof, bound to its operation and exact route. Only
/// [`prove`] creates one.
#[derive(Debug)]
pub(super) struct Proof {
    digest: String,
    operation_id: String,
    route_digest: String,
    epoch: u64,
    lineage: u64,
}

/// ITEM-010: proves the writer serves the seeded lineage at the current
/// epoch, its first archive checkpoint is the authority head, exactly one
/// revision of the operation's writer is active and the required policy
/// generation is applied. Without a qualified clock proof (OD-005) the
/// freshness of the published state cannot be proven, so it refuses first.
fn prove(
    ops: &Operations,
    authority: &AuthorityStore,
    intent: &Intent,
    readiness: &Readiness,
    clock: Option<&projection::ClockProof>,
) -> Result<Proof, ApiError> {
    if clock.is_none() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "projection_freshness_unproven",
            "no qualified clock proof covers the readiness observation",
        ));
    }
    let seed = recorded_seed(&ops.journal(&intent.operation_id)?).ok_or_else(seed_unrecorded)?;
    let record = authority.read()?.record;
    let Some(head) = record.head.as_ref() else {
        return Err(proof_refused());
    };
    let writer = &readiness.writer;
    let active: Vec<_> = match &writer.revisions {
        aca::Revisions::Complete(revisions) => revisions.iter().filter(|r| r.active).collect(),
        _ => Vec::new(),
    };
    let owner = format!("{}:{}", aca::OWNER_TAG, intent.tenant_id);
    let route = &readiness.route;
    if record.phase != authority::Phase::Serving
        || record.writer_id.is_none()
        || head.epoch != record.epoch
        || record.lineage != format!("{:016x}", seed.lineage)
        || head.manifest_sha256 != readiness.archive_manifest_sha256
        || writer.owner.as_deref() != Some(owner.as_str())
        || !writer.succeeded
        || active.len() != 1
        || readiness.policy_generation_required == 0
        || readiness.policy_generation_observed < readiness.policy_generation_required
        || !writer_only(route, intent, seed.lineage, record.epoch)
    {
        return Err(proof_refused());
    }
    let route_digest = control_state::route_digest(route);
    let digest = sha256_hex(
        format!(
            "vlite.readiness/2\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}",
            intent.operation_id,
            record.lineage,
            record.epoch,
            record.revision,
            head.manifest_sha256,
            head.chain_sha256,
            active[0].name,
            readiness.policy_generation_observed,
            route_digest,
        )
        .as_bytes(),
    );
    Ok(Proof {
        digest,
        operation_id: intent.operation_id.clone(),
        route_digest,
        epoch: record.epoch,
        lineage: seed.lineage,
    })
}

/// The writer-only dedicated route of `intent`'s tenant, seeded lineage
/// and `epoch` (no reader, no authority-mode override).
fn writer_only(route: &Route, intent: &Intent, lineage: u64, epoch: u64) -> bool {
    route.tenant == intent.tenant_id
        && route.kind == control_state::Kind::Dedicated
        && route.read_routing == control_state::ReadRouting::WriterOnly
        && route.reader.is_none()
        && route.authority_mode.is_none()
        && route.lineage == lineage
        && epoch > 0
        && route.epoch == Some(epoch)
}

/// ITEM-010 credential delivery: one candidate at a time, replaced only
/// before acknowledgement (the old one is recorded as revoked).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Delivery {
    schema: String,
    operation_id: String,
    pub(super) generation: u64,
    pub(super) credential_id: String,
    pub(super) acknowledged: bool,
    pub(super) revoked: Vec<String>,
}

/// An acknowledged delivery. Only [`Operations::delivered`] creates one.
#[derive(Debug)]
pub(super) struct Delivered {
    operation_id: String,
    generation: u64,
    credential_id: String,
    revoked: Vec<String>,
}

const DELIVERY_SCHEMA: &str = "vlite.credential-delivery/1";

fn credential_id_valid(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

impl Operations {
    fn update_delivery<R>(
        &self,
        operation_id: &str,
        mut change: impl FnMut(Option<&Delivery>) -> Result<(Option<Delivery>, R), ApiError>,
    ) -> Result<R, ApiError> {
        let name = self.name(operation_id, "delivery.json");
        for _ in 0..CAS_ATTEMPTS {
            let current = self.get(&name)?;
            let delivery = match &current {
                Some((bytes, _)) => {
                    let delivery: Delivery = Self::parse(bytes)?;
                    if delivery.schema != DELIVERY_SCHEMA || delivery.operation_id != operation_id {
                        return Err(state_invalid());
                    }
                    Some(delivery)
                }
                None => None,
            };
            let (next, result) = change(delivery.as_ref())?;
            let Some(next) = next else {
                return Ok(result);
            };
            let bytes = serde_json::to_vec(&next).map_err(|_| state_invalid())?;
            let cond = match &current {
                Some((_, etag)) => Cond::Match(etag),
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
            "delivery_conflict",
            "the credential delivery record is changing too often; retry later",
        ))
    }

    pub(super) fn delivery(&self, operation_id: &str) -> Result<Option<Delivery>, ApiError> {
        self.update_delivery(operation_id, |current| Ok((None, current.cloned())))
    }

    /// Records the first candidate credential (idempotent for the same ID).
    pub(super) fn issue_candidate(
        &self,
        operation_id: &str,
        credential_id: &str,
    ) -> Result<Delivery, ApiError> {
        if !credential_id_valid(credential_id) {
            return Err(invalid("credential ID is invalid"));
        }
        self.update_delivery(operation_id, |current| match current {
            Some(delivery) if delivery.credential_id == credential_id => {
                Ok((None, delivery.clone()))
            }
            Some(_) => Err(conflict(
                "delivery_candidate_exists",
                "another candidate credential is already being delivered",
            )),
            None => {
                let delivery = Delivery {
                    schema: DELIVERY_SCHEMA.to_owned(),
                    operation_id: operation_id.to_owned(),
                    generation: 1,
                    credential_id: credential_id.to_owned(),
                    acknowledged: false,
                    revoked: Vec::new(),
                };
                Ok((Some(delivery.clone()), delivery))
            }
        })
    }

    /// Replaces an unacknowledged candidate: the old ID is recorded as
    /// revoked (activation verifies it is not live). Idempotent.
    pub(super) fn replace_candidate(
        &self,
        operation_id: &str,
        expected_generation: u64,
        credential_id: &str,
    ) -> Result<Delivery, ApiError> {
        if !credential_id_valid(credential_id) {
            return Err(invalid("credential ID is invalid"));
        }
        self.update_delivery(operation_id, |current| {
            let Some(delivery) = current else {
                return Err(conflict(
                    "delivery_stale",
                    "no candidate is being delivered",
                ));
            };
            if delivery.generation == expected_generation + 1
                && delivery.credential_id == credential_id
            {
                return Ok((None, delivery.clone()));
            }
            if delivery.generation != expected_generation
                || delivery.acknowledged
                || delivery.credential_id == credential_id
                || delivery.revoked.iter().any(|id| id == credential_id)
            {
                return Err(conflict(
                    "delivery_stale",
                    "the candidate changed, was acknowledged or the ID was used before",
                ));
            }
            let mut next = delivery.clone();
            next.revoked.push(delivery.credential_id.clone());
            next.credential_id = credential_id.to_owned();
            next.generation += 1;
            Ok((Some(next.clone()), next))
        })
    }

    /// The administrator confirms receipt of exactly this candidate.
    pub(super) fn acknowledge_delivery(
        &self,
        operation_id: &str,
        generation: u64,
        credential_id: &str,
    ) -> Result<Delivery, ApiError> {
        self.update_delivery(operation_id, |current| match current {
            Some(delivery)
                if delivery.generation == generation && delivery.credential_id == credential_id =>
            {
                if delivery.acknowledged {
                    return Ok((None, delivery.clone()));
                }
                let mut next = delivery.clone();
                next.acknowledged = true;
                Ok((Some(next.clone()), next))
            }
            _ => Err(conflict(
                "delivery_stale",
                "the acknowledged candidate is not the current one",
            )),
        })
    }

    /// The acknowledged delivery, or 409 `credential_delivery_unacknowledged`.
    pub(super) fn delivered(&self, operation_id: &str) -> Result<Delivered, ApiError> {
        match self.delivery(operation_id)? {
            Some(delivery) if delivery.acknowledged => Ok(Delivered {
                operation_id: delivery.operation_id,
                generation: delivery.generation,
                credential_id: delivery.credential_id,
                revoked: delivery.revoked,
            }),
            _ => Err(conflict(
                "credential_delivery_unacknowledged",
                "the tenant credential's delivery is not acknowledged",
            )),
        }
    }
}

/// The policy of a create's first tenant credential (OD-004/OD-006 decide
/// it): what `credentials::candidate` mints, bound to the tenant's every
/// collection and seeded lineage.
#[derive(Clone, Debug)]
pub(super) struct CandidateGrant {
    pub(super) name: String,
    pub(super) operations: Vec<String>,
    pub(super) graphs: Option<Vec<String>>,
    pub(super) expires_unix_ms: u64,
}

/// The recorded seed of a create that may publish a candidate, with the
/// published state that admitted it.
fn candidate_seed(
    ops: &Operations,
    objects: &Objects,
    intent: &Intent,
) -> Result<(Seed, Snapshot), ApiError> {
    if intent.kind() != Kind::Create {
        return Err(invalid(
            "only a create operation issues a candidate credential",
        ));
    }
    if ops.terminal(&intent.operation_id)?.is_some() {
        return Err(ended());
    }
    let (snapshot, _) = objects.read()?;
    if !matches!(
        holds(&snapshot, intent),
        Some(Slot::Reserved {
            cancellation_requested: false,
            receipt: None,
            ..
        })
    ) || snapshot.record_for(&intent.tenant_id).is_some()
    {
        return Err(conflict(
            "operation_not_provisionable",
            "the operation does not hold an uncancelled reservation of an unrouted tenant",
        ));
    }
    let seed = recorded_seed(&ops.journal(&intent.operation_id)?).ok_or_else(seed_unrecorded)?;
    Ok((seed, snapshot))
}

/// Mints a candidate for the seeded tenant at the delivery generation after
/// `expected` (the first when `None`), returning it with its token, that
/// generation and every candidate it replaces: the expected generation's
/// whole revoked list and its current ID. The delivery record is checked
/// before anything is minted, and the published state must take exactly
/// this publication, so a knowable refusal records nothing.
fn mint_candidate(
    ops: &Operations,
    objects: &Objects,
    intent: &Intent,
    grant: &CandidateGrant,
    expected: Option<u64>,
    now: u64,
) -> Result<(super::credentials::Candidate, String, u64, Vec<String>, u64), ApiError> {
    let (seed, snapshot) = candidate_seed(ops, objects, intent)?;
    let revoked = match (ops.delivery(&intent.operation_id)?, expected) {
        (None, None) => Vec::new(),
        (Some(_), None) => return Err(candidate_recorded()),
        (Some(delivery), Some(expected))
            if delivery.generation == expected && !delivery.acknowledged =>
        {
            let mut revoked = delivery.revoked;
            revoked.push(delivery.credential_id);
            revoked
        }
        _ => return Err(delivery_stale()),
    };
    let generation = expected
        .map_or(Some(1), |expected| expected.checked_add(1))
        .ok_or_else(delivery_stale)?;
    let (candidate, token) = super::credentials::candidate(
        &intent.tenant_id,
        seed.lineage,
        &grant.name,
        &grant.operations,
        grant.graphs.as_deref(),
        grant.expires_unix_ms,
        now,
    )?;
    snapshot.with_candidate(
        &intent.tenant_id,
        &intent.operation_id,
        generation,
        &candidate,
        &revoked,
        now,
    )?;
    Ok((candidate, token, generation, revoked, seed.lineage))
}

/// ITEM-010: a token as minted, bound to the exact delivery generation,
/// credential ID and replaced candidates it was recorded and published as.
/// The token leaves the server only through [`Minted::deliver`], which
/// re-checks that binding, so it is never paired with another candidate's
/// delivery record. A concurrent replacement or cancellation can still
/// supersede the candidate after its corresponding check.
pub(super) struct Minted {
    token: String,
    generation: u64,
    credential_id: String,
    revoked: Vec<String>,
    lineage: u64,
}

impl std::fmt::Debug for Minted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Minted")
            .field("token", &"<redacted>")
            .field("generation", &self.generation)
            .field("credential_id", &self.credential_id)
            .field("revoked", &self.revoked)
            .finish()
    }
}

impl Minted {
    /// The token with the delivery record that names exactly this candidate.
    /// The delivery record is read first and must still be this generation
    /// and ID with the same replaced candidates (it may be acknowledged).
    /// The control-state view is read last, and that read is the final
    /// validation: the view must still publish the token live for the tenant
    /// and seeded lineage as this operation's candidate at its generation,
    /// with the reservation uncancelled and unreleased. Cancellation and
    /// revocation happen only in the view, and a replacement is recorded in
    /// the delivery record before it is published there. A cancellation or
    /// published replacement visible in the final view is refused (409
    /// `candidate_superseded`).
    ///
    /// Nothing is checked after that read. A cancellation, or a
    /// replacement's publication, that lands later still revokes the token
    /// in the view; the client may receive a token that no longer
    /// authenticates, and receiving it never prevents that revocation. A
    /// replacement recorded after the delivery read but published after the
    /// view read is the same race: the returned delivery then names a
    /// superseded generation, so acknowledging it is refused
    /// (`delivery_stale`).
    pub(super) fn deliver(
        self,
        ops: &Operations,
        objects: &Objects,
        intent: &Intent,
        now: u64,
    ) -> Result<(String, Delivery), ApiError> {
        let superseded = || {
            conflict(
                "candidate_superseded",
                "the minted candidate was replaced, revoked or cancelled when checked; the token was discarded",
            )
        };
        let delivery = match ops.delivery(&intent.operation_id)? {
            Some(delivery)
                if delivery.generation == self.generation
                    && delivery.credential_id == self.credential_id
                    && delivery.revoked == self.revoked =>
            {
                delivery
            }
            _ => return Err(superseded()),
        };
        // The final validation: read after the delivery record.
        let (snapshot, _) = objects.read()?;
        let live = matches!(
            holds(&snapshot, intent),
            Some(Slot::Reserved {
                cancellation_requested: false,
                receipt: None,
                ..
            })
        ) && snapshot.records_publication(
            &intent.tenant_id,
            &intent.operation_id,
            self.generation,
            &self.credential_id,
            &self.revoked,
        ) && snapshot.live_credential(&self.credential_id, now)
            == Some((Some(intent.tenant_id.clone()), self.lineage));
        if !live {
            return Err(superseded());
        }
        Ok((self.token, delivery))
    }

    #[cfg(test)]
    pub(super) fn token(&self) -> &str {
        &self.token
    }
}

fn candidate_recorded() -> ApiError {
    conflict(
        "delivery_candidate_exists",
        "another candidate credential is already being delivered",
    )
}

fn delivery_stale() -> ApiError {
    conflict(
        "delivery_stale",
        "the candidate changed, was acknowledged or the ID was used before",
    )
}

/// Records the minted candidate in the delivery record through `record`
/// (an idempotent `issue_candidate` or `replace_candidate`), which must
/// leave exactly delivery `generation` naming `credential_id` with
/// `revoked` replaced. An uncertain write is reconciled while the caller
/// still holds the token, by re-running `record` until it confirms that
/// state or a settled refusal. The token is never reissued: if the outcome
/// stays unknown it is dropped (`operation_indeterminate`), and only a
/// replacement issues another.
fn record_delivery(
    mut record: impl FnMut() -> Result<Delivery, ApiError>,
    generation: u64,
    credential_id: &str,
    revoked: &[String],
) -> Result<(), ApiError> {
    let mut uncertain = false;
    for _ in 0..CAS_ATTEMPTS {
        match record() {
            Ok(delivery) => {
                return if delivery.generation == generation
                    && delivery.credential_id == credential_id
                    && delivery.revoked == revoked
                {
                    Ok(())
                } else {
                    Err(delivery_stale())
                };
            }
            Err(error) if matches!(error.code, "delivery_candidate_exists" | "delivery_stale") => {
                return Err(error);
            }
            Err(error) if !uncertain && !outcome_unknown(&error) => return Err(error),
            Err(_) => uncertain = true,
        }
    }
    Err(indeterminate())
}

/// Publishes candidate `generation`, hash-only, revokes the replaced ones
/// in the control-state view and records the publication, in one CAS that
/// refuses a superseded generation. Done only when the view publishes
/// exactly this candidate as that generation; a lost acknowledgement is
/// resolved by reading back.
fn publish_view(
    objects: &Objects,
    intent: &Intent,
    generation: u64,
    candidate: &super::credentials::Candidate,
    revoked: &[String],
    now: u64,
) -> Result<(), ApiError> {
    let (tenant, operation_id) = (&intent.tenant_id, &intent.operation_id);
    lifecycle_cas(
        objects,
        now,
        |snapshot| {
            snapshot.publishes_candidate(tenant, operation_id, generation, candidate, revoked, now)
        },
        |current| current.with_candidate(tenant, operation_id, generation, candidate, revoked, now),
    )
    .map(drop)
}

/// ITEM-010 (PRD §3.4 step 7): issues a reserved, seeded create's first
/// candidate credential once. Its ID is recorded in the delivery record
/// first, then the credential is published hash-only into the control-state
/// view, bound to the tenant and seeded lineage; the tenant stays unrouted.
/// Returns the token, the only copy of its secret, bound to its delivery
/// ([`Minted::deliver`] releases it for the private delivery, OD-004). A
/// retry never mints silently: once a candidate is recorded it
/// is refused (`delivery_candidate_exists`) before anything is minted, and
/// only [`replace_published_candidate`] issues another. No production
/// caller yet; activation still needs readiness proof (OD-005).
pub(super) fn publish_candidate(
    ops: &Operations,
    objects: &Objects,
    intent: &Intent,
    grant: &CandidateGrant,
    now: u64,
) -> Result<Minted, ApiError> {
    let (candidate, token, generation, revoked, lineage) =
        mint_candidate(ops, objects, intent, grant, None, now)?;
    record_delivery(
        || ops.issue_candidate(&intent.operation_id, candidate.id()),
        generation,
        candidate.id(),
        &revoked,
    )?;
    publish_view(objects, intent, generation, &candidate, &revoked, now)?;
    Ok(Minted {
        token,
        generation,
        credential_id: candidate.id().to_owned(),
        revoked,
        lineage,
    })
}

/// ITEM-010: replaces the unacknowledged candidate at `expected_generation`
/// (a lost or unpublished one included) with a newly minted one, and
/// revokes every replaced candidate in the view in the same publication.
pub(super) fn replace_published_candidate(
    ops: &Operations,
    objects: &Objects,
    intent: &Intent,
    expected_generation: u64,
    grant: &CandidateGrant,
    now: u64,
) -> Result<Minted, ApiError> {
    let (candidate, token, generation, revoked, lineage) =
        mint_candidate(ops, objects, intent, grant, Some(expected_generation), now)?;
    record_delivery(
        || ops.replace_candidate(&intent.operation_id, expected_generation, candidate.id()),
        generation,
        candidate.id(),
        &revoked,
    )?;
    publish_view(objects, intent, generation, &candidate, &revoked, now)?;
    Ok(Minted {
        token,
        generation,
        credential_id: candidate.id().to_owned(),
        revoked,
        lineage,
    })
}

/// ITEM-010: activates a create once readiness is proven and the credential
/// delivery acknowledged: publishes the writer-only route (no reader is
/// ever started here; EPIC-005), releases the guard and seals `succeeded`.
/// The acknowledged credential must already be live in the published view,
/// bound to the tenant and seeded lineage, recorded there as this
/// operation's candidate at the acknowledged delivery generation with
/// exactly the delivery's replaced candidates, and every replaced candidate
/// revoked there; only [`publish_candidate`] publishes it for a reserved
/// tenant, and it has no production caller yet (OD-004), so production
/// still fails closed with `credential_view_unpublished`.
/// `now` timestamps bookkeeping; only a new route write needs a fresh
/// credential observation from `clock`. A committed receipt needs none.
#[allow(clippy::too_many_arguments)]
pub(super) fn activate_create(
    ops: &Operations,
    objects: &Objects,
    guards: &Guards,
    intent: &Intent,
    route: &Route,
    proof: &Proof,
    delivered: &Delivered,
    now: u64,
    mut clock: impl FnMut() -> Result<u64, ApiError>,
) -> Result<Sealed, ApiError> {
    if intent.kind() != Kind::Create || delivered.operation_id != intent.operation_id {
        return Err(invalid("not this create operation's proof or delivery"));
    }
    if ops.terminal(&intent.operation_id)?.is_some() {
        // A retry after the seal: report the same sealed outcome.
        return ops.release(objects, intent, now);
    }
    let route_invalid = || {
        conflict(
            "activation_route_invalid",
            "the route is not the proven writer-only route of the seeded lineage and epoch",
        )
    };
    // A crash after the route CAS: the durable receipt is the proof that
    // ran; a later checkpoint (another readiness digest) or an expired or
    // revoked credential must not strand it. Only this operation's exact
    // committed route is accepted.
    let (snapshot, _) = objects.read()?;
    if let Some(Slot::Reserved {
        receipt: Some(receipt),
        ..
    }) = holds(&snapshot, intent)
        && receipt.operation_id == intent.operation_id
    {
        let committed = snapshot
            .record_for(&intent.tenant_id)
            .map(control_state::route_digest);
        if committed.as_deref() != Some(receipt.route_digest.as_str())
            || control_state::route_digest(route) != receipt.route_digest
        {
            return Err(route_invalid());
        }
        guards.release(&intent.tenant_id, &intent.operation_id)?;
        return ops.release(objects, intent, now);
    }
    let seed = recorded_seed(&ops.journal(&intent.operation_id)?).ok_or_else(seed_unrecorded)?;
    if proof.operation_id != intent.operation_id
        || control_state::route_digest(route) != proof.route_digest
        || !writer_only(route, intent, seed.lineage, proof.epoch)
        || route.lineage != proof.lineage
    {
        return Err(route_invalid());
    }
    let mut previous = now;
    activate_checked(ops, objects, intent, route, &proof.digest, now, |current| {
        let at = clock()?;
        if at < previous {
            return Err(conflict(
                "activation_clock_regressed",
                "the clock moved backwards during activation",
            ));
        }
        previous = at;
        if current.live_credential(&delivered.credential_id, at)
            != Some((Some(intent.tenant_id.clone()), seed.lineage))
        {
            return Err(conflict(
                "credential_view_unpublished",
                "the acknowledged credential is not live in the published view for this tenant and lineage",
            ));
        }
        if delivered
            .revoked
            .iter()
            .any(|id| current.live_credential(id, at).is_some())
        {
            return Err(conflict(
                "candidate_revocation_unconfirmed",
                "a replaced candidate credential is still live",
            ));
        }
        if !current.records_publication(
            &intent.tenant_id,
            &intent.operation_id,
            delivered.generation,
            &delivered.credential_id,
            &delivered.revoked,
        ) {
            return Err(conflict(
                "credential_view_unpublished",
                "the published view does not record the acknowledged credential as this operation's candidate at its delivery generation",
            ));
        }
        Ok(())
    })?;
    guards.release(&intent.tenant_id, &intent.operation_id)?;
    ops.release(objects, intent, previous)
}

// ---------------------------------------------------------------- routes

mod aca;
mod admin;
mod arm;
mod digest;
pub(super) mod guard;
pub(super) mod projection;
mod resource_id;
mod seed;
mod worker;

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
            "/v1/admin/tenant-deployment-operations",
            get(admin::operation_list),
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
mod candidate_tests;
#[cfg(test)]
mod seed_tests;
#[cfg(test)]
mod tests;
