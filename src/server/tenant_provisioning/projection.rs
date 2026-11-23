//! design-tenant-provisioning ITEM-006: the tenant-only backend control
//! projection and its upstream freshness witness (PRD section 3.4).
//!
//! The control worker (`Publisher`) derives one tenant's view from a new
//! read of the shared control state: the tenant record, either its
//! directory route (`active`) or an operation-bound read-only validation
//! admission on a tenant reserved for `create` and still unrouted
//! (`pending`), the source generation `A` and digest, and a witness: a
//! revision, the wall time taken *before* the source read and a deadline of
//! that time plus the existing S_ctl, so a slow read consumes the budget.
//! No credential (hash or otherwise) and no other tenant is ever in it.
//! Publishing writes nothing to the control state, so the public gateway,
//! which routes by the control-state directory, never sees a pending
//! tenant. Publication is a compare-and-swap that refuses a source,
//! read-start or revision regression; the publisher keeps no cache, so a
//! restarted or resumed worker publishes only from a new source read.
//!
//! A backend (`Reader`) accepts a witness only with a `ClockProof`, the
//! approved pairwise clock uncertainty backed by qualified evidence; without
//! one every decision is 503 `projection_freshness_unproven` (OD-005), and
//! there is no production constructor. A new revision's remaining validity,
//! `deadline - wall - uncertainty`, is anchored to the monotonic clock and
//! never extended by rereading the same witness, however healthy the
//! projection reads are. A wall clock that falls behind the monotonic one by
//! more than the uncertainty is a sticky `projection_clock_regressed`.
//!
//! Not covered (see the PRD, section 15): there is no production publisher
//! (the control worker, ITEM-010/016) and no `--gateway-trust-file` opt-in
//! (its strict schema refuses one); both wait on OD-005 clock evidence.
//! Only a `create` reservation can carry a pending admission, and the
//! private readiness proof channel is EPIC-004.
#![cfg_attr(not(test), allow(dead_code))]

use super::*;
use crate::server::credentials::{Grant, Op};
use crate::server::principal_envelope::Refusal;
use std::sync::{Mutex, PoisonError};
use std::time::Instant;

pub(in crate::server) const SCHEMA: &str = "vlite.control-projection/1";
const MAX_PROJECTION_BYTES: u64 = 64 * 1024;
/// The only operations a pending validator's grant may allow.
const READ_ONLY: &[Op] = &[Op::Read, Op::Search, Op::GraphRead];

/// An operation-bound validation admission: only the control worker's
/// validator credential for `operation_id`, read-only, at one backend.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::server) struct Pending {
    pub(in crate::server) operation_id: String,
    pub(in crate::server) audience: String,
    pub(in crate::server) lineage: u64,
    /// The credential ID of the operation's validator.
    pub(in crate::server) validator: String,
}

impl Pending {
    fn valid(&self) -> bool {
        is_hex(&self.operation_id, 32)
            && text(&self.audience)
            && self.lineage != 0
            && text(&self.validator)
            && self.validator.len() <= 64
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum Admission {
    Active { route: Route },
    Pending { pending: Pending },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::server) struct Projection {
    schema: String,
    tenant_id: String,
    /// Absent when the source no longer has a live tenant record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tenant: Option<tenants::Tenant>,
    /// Absent when nothing admits the tenant here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    admission: Option<Admission>,
    source_generation: u64,
    source_digest: String,
    witness_revision: u64,
    read_start_unix_ms: u64,
    deadline_unix_ms: u64,
}

fn text(value: &str) -> bool {
    !value.is_empty() && value.len() <= 2048 && !value.chars().any(char::is_control)
}

fn unavailable(code: &'static str, message: &'static str) -> ApiError {
    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, code, message)
}

fn conflict(code: &'static str, message: &'static str) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, code, message)
}

fn invalid() -> ApiError {
    unavailable("projection_invalid", "the control projection is invalid")
}

fn stale() -> ApiError {
    unavailable(
        "projection_stale",
        "the control projection's freshness deadline has passed",
    )
}

impl Projection {
    fn valid(&self, tenant: &str, s_ctl_ms: u64) -> bool {
        self.schema == SCHEMA
            && self.tenant_id == tenant
            && self
                .tenant
                .as_ref()
                .is_none_or(|record| record.id == tenant)
            && self.source_generation > 0
            && self.witness_revision > 0
            && is_hex(&self.source_digest, 64)
            && self.deadline_unix_ms > self.read_start_unix_ms
            && self.deadline_unix_ms - self.read_start_unix_ms <= s_ctl_ms
            && match &self.admission {
                None => true,
                Some(Admission::Active { route }) => {
                    route.tenant == tenant && text(&route.audience)
                }
                Some(Admission::Pending { pending }) => pending.valid() && self.tenant.is_some(),
            }
    }

    pub(in crate::server) fn source_generation(&self) -> u64 {
        self.source_generation
    }

    /// Whether an envelope for `tenant` naming `audience` is served by this
    /// backend of database `lineage`.
    pub(in crate::server) fn serves(
        &self,
        tenant: Option<&str>,
        audience: &str,
        lineage: Option<u64>,
    ) -> bool {
        tenant == Some(self.tenant_id.as_str())
            && match &self.admission {
                Some(Admission::Active { route }) => {
                    route.audience == audience && lineage == Some(route.lineage)
                }
                Some(Admission::Pending { pending }) => {
                    pending.audience == audience && lineage == Some(pending.lineage)
                }
                None => false,
            }
    }

    /// Admits a verified envelope grant for the projected tenant. Under a
    /// pending admission only the operation's validator is admitted, and
    /// only with a read-only grant; the request is then still checked by
    /// the usual permission rules.
    pub(in crate::server) fn admit_grant(&self, grant: &Grant) -> Result<(), ApiError> {
        if let Some(Admission::Pending { pending }) = &self.admission {
            if grant.id != pending.validator {
                return Err(Refusal::Tenant.error());
            }
            if !grant.allows_only(READ_ONLY) {
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "pending_validation_read_only",
                    "a pending validation admits only read-only operations",
                ));
            }
        }
        self.admit(&self.tenant_id)
    }

    /// Tenant admission (unknown or deleted: 503, suspended: 403).
    pub(in crate::server) fn admit(&self, tenant: &str) -> Result<(), ApiError> {
        tenants::admit(self.tenant.as_slice(), tenant).map(drop)
    }

    pub(in crate::server) fn tenant_quota(
        &self,
        tenant: &str,
    ) -> Result<Option<tenants::Tenant>, ApiError> {
        tenants::quota_in(self.tenant.as_slice(), tenant)
    }
}

/// The tenant's record and admission in `snapshot`. A pending admission
/// needs the tenant unrouted and its slot reserved for `create` by
/// `pending.operation_id`, not cancelled and without a receipt.
fn derive(
    snapshot: &Snapshot,
    tenant: &str,
    pending: Option<&Pending>,
) -> Result<(Option<tenants::Tenant>, Option<Admission>), ApiError> {
    let record = snapshot.tenant_quota(tenant).ok().flatten();
    let admission = match (snapshot.record_for(tenant), pending) {
        (Some(route), None) => Some(Admission::Active {
            route: route.clone(),
        }),
        (None, None) => None,
        (None, Some(pending))
            if pending.valid()
                && record.is_some()
                && matches!(
                    snapshot.lifecycle(tenant).map(|entry| &entry.slot),
                    Some(Slot::Reserved {
                        operation_id,
                        kind: Kind::Create,
                        cancellation_requested: false,
                        receipt: None,
                        ..
                    }) if *operation_id == pending.operation_id
                ) =>
        {
            Some(Admission::Pending {
                pending: pending.clone(),
            })
        }
        _ => {
            return Err(conflict(
                "projection_pending_unbound",
                "a pending admission needs the tenant unrouted and reserved for create by this operation",
            ));
        }
    };
    Ok((record, admission))
}

/// The published projection, its byte digest and ETag.
fn read(
    store: &dyn ObjectStore,
    name: &str,
    tenant: &str,
    s_ctl_ms: u64,
) -> Result<Option<(Projection, String, String)>, ApiError> {
    let blob = match store.get(name, MAX_PROJECTION_BYTES) {
        Ok(blob) => blob,
        Err(StoreError::NotFound) => return Ok(None),
        Err(StoreError::TooLarge) => return Err(invalid()),
        Err(_) => {
            return Err(unavailable(
                "projection_unavailable",
                "the control projection could not be read",
            ));
        }
    };
    let projection = serde_json::from_slice::<Projection>(&blob.bytes)
        .ok()
        .filter(|projection| projection.valid(tenant, s_ctl_ms))
        .ok_or_else(invalid)?;
    Ok(Some((projection, sha256_hex(&blob.bytes), blob.etag)))
}

/// The control worker's side: one tenant's projection object.
pub(in crate::server) struct Publisher {
    source: Objects,
    store: Arc<dyn ObjectStore>,
    name: String,
    s_ctl_ms: u64,
}

impl Publisher {
    pub(in crate::server) fn new(
        source: Objects,
        store: Arc<dyn ObjectStore>,
        name: impl Into<String>,
        s_ctl_ms: u64,
    ) -> Result<Self, &'static str> {
        if s_ctl_ms == 0 {
            return Err("S_ctl must be positive");
        }
        Ok(Self {
            source,
            store,
            name: name.into(),
            s_ctl_ms,
        })
    }

    /// Issues a new witness for `tenant` from a new source read; `wall`
    /// (Unix ms) is read before it. Nothing is written on any refusal.
    pub(in crate::server) fn publish(
        &self,
        tenant: &str,
        pending: Option<&Pending>,
        wall: impl Fn() -> u64,
    ) -> Result<Projection, ApiError> {
        if !is_hex(tenant, 32) {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "projection_request_invalid",
                "the tenant ID must be 32 lowercase hex",
            ));
        }
        let read_start = wall();
        let (snapshot, _) = self.source.read()?;
        let (record, admission) = derive(&snapshot, tenant, pending)?;
        let current = read(&*self.store, &self.name, tenant, self.s_ctl_ms)?;
        if let Some((current, _, _)) = &current
            && (current.source_generation > snapshot.generation()
                || current.read_start_unix_ms > read_start)
        {
            return Err(conflict(
                "projection_regressed",
                "a newer witness is already published; nothing was written",
            ));
        }
        let projection = Projection {
            schema: SCHEMA.to_owned(),
            tenant_id: tenant.to_owned(),
            tenant: record,
            admission,
            source_generation: snapshot.generation(),
            source_digest: sha256_hex(
                &serde_json::to_vec(&snapshot).expect("a snapshot always serializes"),
            ),
            witness_revision: current
                .as_ref()
                .map_or(Some(1), |(current, _, _)| {
                    current.witness_revision.checked_add(1)
                })
                .ok_or_else(invalid)?,
            read_start_unix_ms: read_start,
            deadline_unix_ms: read_start.checked_add(self.s_ctl_ms).ok_or_else(invalid)?,
        };
        let bytes = serde_json::to_vec(&projection).expect("a projection always serializes");
        let cond = match &current {
            Some((_, _, etag)) => Cond::Match(etag),
            None => Cond::Create,
        };
        match self.store.put(&self.name, &bytes, cond) {
            Ok(_) => Ok(projection),
            Err(StoreError::Conflict) => Err(conflict(
                "projection_conflict",
                "another publisher wrote the projection first; nothing was written",
            )),
            Err(StoreError::Unknown(_)) => Err(unavailable(
                "projection_indeterminate",
                "the projection write outcome is unknown; read it before retrying",
            )),
            Err(_) => Err(unavailable(
                "projection_unavailable",
                "the control projection could not be written",
            )),
        }
    }
}

/// The approved pairwise clock uncertainty, backed by qualified clock
/// evidence (OD-005). Nothing can produce one outside tests yet.
pub(in crate::server) struct ClockProof {
    uncertainty_ms: u64,
}

impl ClockProof {
    #[cfg(test)]
    pub(in crate::server) fn for_test(uncertainty_ms: u64) -> Self {
        Self { uncertainty_ms }
    }
}

struct Accepted {
    projection: Arc<Projection>,
    digest: String,
    /// Monotonic ms at which the witness expires here.
    expires: u64,
}

#[derive(Default)]
struct State {
    accepted: Option<Accepted>,
    /// (wall, monotonic) ms of the latest clock check.
    anchor: Option<(u64, u64)>,
    regressed: bool,
}

/// A backend's view of its tenant's projection.
pub(in crate::server) struct Reader {
    store: Arc<dyn ObjectStore>,
    name: String,
    tenant: String,
    s_ctl_ms: u64,
    clock: Option<ClockProof>,
    base: Instant,
    state: Mutex<State>,
}

impl Reader {
    pub(in crate::server) fn new(
        store: Arc<dyn ObjectStore>,
        name: impl Into<String>,
        tenant: &str,
        s_ctl_ms: u64,
        clock: Option<ClockProof>,
    ) -> Result<Self, &'static str> {
        if s_ctl_ms == 0 || !is_hex(tenant, 32) {
            return Err("S_ctl must be positive and the tenant 32 lowercase hex");
        }
        Ok(Self {
            store,
            name: name.into(),
            tenant: tenant.to_owned(),
            s_ctl_ms,
            clock,
            base: Instant::now(),
            state: Mutex::new(State::default()),
        })
    }

    /// (wall, monotonic) now, in ms.
    fn now(&self) -> (u64, u64) {
        (
            crate::server::admin::now_ms() as u64,
            u64::try_from(self.base.elapsed().as_millis()).unwrap_or(u64::MAX),
        )
    }

    pub(in crate::server) fn poll(&self) -> Result<u64, ApiError> {
        self.poll_with(|| self.now())
    }

    pub(in crate::server) fn current(&self) -> Result<Arc<Projection>, ApiError> {
        self.current_with(|| self.now())
    }

    /// The uncertainty, or a refusal without a clock proof or after a wall
    /// clock regression (sticky until a restart with a proven clock). The
    /// wall clock is checked against the monotonic clock, so drift between
    /// them beyond the proven uncertainty also trips it: a qualified clock
    /// proof must bound that drift over the backend's lifetime (OD-005).
    fn check_clock(&self, state: &mut State, wall: u64, mono: u64) -> Result<u64, ApiError> {
        let Some(proof) = &self.clock else {
            return Err(unavailable(
                "projection_freshness_unproven",
                "no qualified clock proof covers the projection's freshness",
            ));
        };
        if !state.regressed {
            match state
                .anchor
                .map(|(w, m)| mono.checked_sub(m).map(|elapsed| w.saturating_add(elapsed)))
            {
                None => state.anchor = Some((wall, mono)),
                Some(Some(expected)) if wall.saturating_add(proof.uncertainty_ms) >= expected => {
                    state.anchor = Some((wall.max(expected), mono));
                }
                Some(_) => state.regressed = true,
            }
        }
        if state.regressed {
            return Err(unavailable(
                "projection_clock_regressed",
                "the wall clock went backwards; restart with a proven clock",
            ));
        }
        Ok(proof.uncertainty_ms)
    }

    /// One projection read at (`wall`, `mono`) ms. A new witness revision
    /// is anchored to `mono`; a reread of the same one changes nothing.
    /// Returns the source generation.
    #[cfg(test)]
    pub(in crate::server) fn poll_at(&self, wall: u64, mono: u64) -> Result<u64, ApiError> {
        self.poll_with(|| (wall, mono))
    }

    /// The clocks are sampled under the lock, after the read, so that
    /// concurrent callers present nondecreasing monotonic times.
    fn poll_with(&self, now: impl FnOnce() -> (u64, u64)) -> Result<u64, ApiError> {
        let read = read(&*self.store, &self.name, &self.tenant, self.s_ctl_ms);
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let (wall, mono) = now();
        let uncertainty = self.check_clock(&mut state, wall, mono)?;
        let (projection, digest, _) = read?.ok_or_else(|| {
            unavailable(
                "projection_missing",
                "the control projection is not published",
            )
        })?;
        if let Some(accepted) = &state.accepted {
            let old = &accepted.projection;
            if projection.witness_revision < old.witness_revision
                || projection.source_generation < old.source_generation
                || projection.read_start_unix_ms < old.read_start_unix_ms
                || (projection.witness_revision == old.witness_revision
                    && digest != accepted.digest)
            {
                return Err(unavailable(
                    "projection_regressed",
                    "the control projection went backwards",
                ));
            }
            if projection.witness_revision == old.witness_revision {
                return Ok(old.source_generation);
            }
        }
        let remaining = projection
            .deadline_unix_ms
            .checked_sub(wall)
            .and_then(|left| left.checked_sub(uncertainty))
            .filter(|left| *left > 0)
            .ok_or_else(stale)?;
        let generation = projection.source_generation;
        state.accepted = Some(Accepted {
            projection: Arc::new(projection),
            digest,
            expires: mono.saturating_add(remaining),
        });
        Ok(generation)
    }

    /// The projection to decide with at (`wall`, `mono`) ms.
    #[cfg(test)]
    pub(in crate::server) fn current_at(
        &self,
        wall: u64,
        mono: u64,
    ) -> Result<Arc<Projection>, ApiError> {
        self.current_with(|| (wall, mono))
    }

    /// The projection to decide with, or the fail-closed 503.
    fn current_with(&self, now: impl FnOnce() -> (u64, u64)) -> Result<Arc<Projection>, ApiError> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let (wall, mono) = now();
        let uncertainty = self.check_clock(&mut state, wall, mono)?;
        let accepted = state.accepted.as_ref().ok_or_else(|| {
            unavailable(
                "projection_unavailable",
                "the control projection has not been read",
            )
        })?;
        if mono >= accepted.expires
            || wall.saturating_add(uncertainty) >= accepted.projection.deadline_unix_ms
        {
            return Err(stale());
        }
        Ok(accepted.projection.clone())
    }
}

#[cfg(test)]
pub(in crate::server) mod tests {
    use super::*;
    use crate::server::cloud_backup::fake::{Fault, Memory, Op as StoreOp};
    use std::sync::atomic::{AtomicU64, Ordering};

    pub(in crate::server) const T: &str = "00112233445566778899aabbccddeeff";
    const OTHER: &str = "fedcba9876543210fedcba9876543210";
    pub(in crate::server) const OP: &str = "0123456789abcdef0123456789abcdef";
    pub(in crate::server) const SOURCE: &str = "control/state.json";
    pub(in crate::server) const NAME: &str = "control/projection-t.json";
    pub(in crate::server) const VALIDATOR: &str = "vvvvvvvvvvvv";
    const S_CTL: u64 = 10_000;
    const U: u64 = 500;

    /// The source control state: T and OTHER with tenant-bound credentials,
    /// OTHER routed; T routed to `backend-t` (`routed`) or reserved for
    /// `create` by `OP` and unrouted.
    pub(in crate::server) fn world(routed: bool, lineage: u64) -> Arc<Memory> {
        let (credentials, _) = credentials::tests::issued(
            &[("aaaaaaaaaaaa", Some(T)), ("bbbbbbbbbbbb", Some(OTHER))],
            lineage,
        );
        let tenants = tenants::Store::for_test(vec![
            tenants::Tenant::for_test(T, None),
            tenants::Tenant::for_test(OTHER, None),
        ]);
        let mut directory = vec![Route::shared_for_test(OTHER)];
        if routed {
            directory.push(Route {
                audience: "backend-t".into(),
                lineage,
                ..Route::dedicated_for_test(T, 1)
            });
        }
        let snapshot = Snapshot::import(&credentials, Some(&tenants))
            .unwrap()
            .with_directory(directory);
        let memory = Arc::new(Memory::default());
        let objects = Objects::new(memory.clone(), SOURCE);
        objects.create(snapshot, 1).unwrap();
        if !routed {
            objects
                .update(1, |c| c.reserve(T, OP, Kind::Create, &"4".repeat(64)))
                .unwrap();
        }
        memory
    }

    pub(in crate::server) fn pending(audience: &str, lineage: u64) -> Pending {
        Pending {
            operation_id: OP.into(),
            audience: audience.into(),
            lineage,
            validator: VALIDATOR.into(),
        }
    }

    pub(in crate::server) fn publisher(memory: &Arc<Memory>) -> Publisher {
        Publisher::new(
            Objects::new(memory.clone(), SOURCE),
            memory.clone(),
            NAME,
            S_CTL,
        )
        .unwrap()
    }

    fn reader(memory: &Arc<Memory>) -> Reader {
        Reader::new(
            memory.clone(),
            NAME,
            T,
            S_CTL,
            Some(ClockProof::for_test(U)),
        )
        .unwrap()
    }

    fn grant(id: &str, ops: &[Op]) -> Grant {
        Grant::for_user(
            id.into(),
            "v".into(),
            Some(T.into()),
            ops,
            0x1234,
            u64::MAX / 2,
        )
    }

    fn revision(memory: &Memory) -> u64 {
        serde_json::from_slice::<Value>(&memory.raw(NAME).unwrap()).unwrap()["witness_revision"]
            .as_u64()
            .unwrap()
    }

    fn code<R>(result: Result<R, ApiError>) -> &'static str {
        result.err().unwrap().code
    }

    #[test]
    fn the_projection_is_tenant_only_and_never_written_to_the_control_state() {
        let memory = world(true, 0x1234);
        let source = memory.raw(SOURCE).unwrap();
        let projection = publisher(&memory).publish(T, None, || 1_000).unwrap();
        assert_eq!(memory.raw(SOURCE).unwrap(), source);
        let bytes = String::from_utf8(memory.raw(NAME).unwrap()).unwrap();
        for absent in [
            OTHER,
            "aaaaaaaaaaaa",
            "bbbbbbbbbbbb",
            "secret",
            "credentials",
            "lifecycle",
        ] {
            assert!(!bytes.contains(absent), "{absent} leaked: {bytes}");
        }
        assert!(bytes.contains("backend-t"));
        assert_eq!(projection.source_generation(), 1);
        assert_eq!(
            (projection.read_start_unix_ms, projection.deadline_unix_ms),
            (1_000, 1_000 + S_CTL)
        );
        assert!(projection.serves(Some(T), "backend-t", Some(0x1234)));
        for (tenant, audience, lineage) in [
            (Some(OTHER), "backend-t", Some(0x1234)),
            (None, "backend-t", Some(0x1234)),
            (Some(T), "shared-pool", Some(0x1234)),
            (Some(T), "backend-t", Some(7)),
            (Some(T), "backend-t", None),
        ] {
            assert!(!projection.serves(tenant, audience, lineage));
        }
        // A routed tenant takes any of its credentials; no pending rules.
        assert!(projection.admit_grant(&grant("x", &[Op::Write])).is_ok());
        assert_eq!(
            code(publisher(&memory).publish(T, Some(&pending("backend-t", 1)), || 2_000)),
            "projection_pending_unbound"
        );
        assert_eq!(
            code(publisher(&memory).publish("T", None, || 2_000)),
            "projection_request_invalid"
        );
    }

    #[test]
    fn a_pending_admission_is_operation_bound_read_only_and_never_routed() {
        let memory = world(false, 0x1234);
        let objects = Objects::new(memory.clone(), SOURCE);
        let pending = pending("backend-t", 0x1234);
        let projection = publisher(&memory)
            .publish(T, Some(&pending), || 1_000)
            .unwrap();
        // The gateway's directory still has no route for T.
        assert!(objects.read().unwrap().0.record_for(T).is_none());
        assert!(projection.serves(Some(T), "backend-t", Some(0x1234)));
        assert!(!projection.serves(Some(T), "backend-t", Some(7)));
        assert!(
            projection
                .admit_grant(&grant(VALIDATOR, &[Op::Read, Op::Search, Op::GraphRead]))
                .is_ok()
        );
        for ops in [
            &[Op::Read, Op::Write][..],
            &[Op::Collections],
            &[Op::Import],
            &[Op::Index],
            &[Op::SearchBuild],
            &[Op::GraphWrite],
            &[Op::Backup],
            &[Op::Diagnostics],
        ] {
            assert_eq!(
                code(projection.admit_grant(&grant(VALIDATOR, ops))),
                "pending_validation_read_only"
            );
        }
        assert_eq!(
            code(projection.admit_grant(&grant("aaaaaaaaaaaa", &[Op::Read]))),
            "envelope_invalid"
        );
        // Bound to this operation and a live create reservation.
        let other_op = Pending {
            operation_id: "f".repeat(32),
            ..pending.clone()
        };
        let malformed = Pending {
            validator: String::new(),
            ..pending.clone()
        };
        for wrong in [&other_op, &malformed] {
            assert_eq!(
                code(publisher(&memory).publish(T, Some(wrong), || 2_000)),
                "projection_pending_unbound"
            );
        }
        objects.update(2, |c| c.request_cancel(T, OP)).unwrap();
        assert_eq!(
            code(publisher(&memory).publish(T, Some(&pending), || 2_000)),
            "projection_pending_unbound"
        );
        // Revocation: the next witness carries no admission.
        let revoked = publisher(&memory).publish(T, None, || 3_000).unwrap();
        assert!(!revoked.serves(Some(T), "backend-t", Some(0x1234)));
        assert_eq!(revision(&memory), 2);
    }

    #[test]
    fn publication_refuses_regression_lost_races_and_resumed_workers() {
        let memory = world(true, 0x1234);
        publisher(&memory).publish(T, None, || 5_000).unwrap();
        let first = memory.raw(NAME).unwrap();
        // An older read start (clock went back, or an old worker): refused.
        assert_eq!(
            code(publisher(&memory).publish(T, None, || 4_999)),
            "projection_regressed"
        );
        assert_eq!(memory.raw(NAME).unwrap(), first);
        // A worker that read the source at 6_000 resumes after another
        // published from a read at 7_000: refused, nothing written.
        let other = memory.clone();
        memory.hook(StoreOp::Get, "projection", move || {
            publisher(&other).publish(T, None, || 7_000).unwrap();
        });
        assert_eq!(
            code(publisher(&memory).publish(T, None, || 6_000)),
            "projection_regressed"
        );
        assert_eq!(revision(&memory), 2);
        // A write between its read and its put: the CAS refuses it.
        let other = memory.clone();
        let winner = memory.raw(NAME).unwrap();
        memory.hook(StoreOp::Put, "projection", move || {
            other.set_raw(NAME, &winner);
        });
        assert_eq!(
            code(publisher(&memory).publish(T, None, || 8_000)),
            "projection_conflict"
        );
        memory.fault(StoreOp::Put, "projection", Fault::Before, 0, 1);
        assert_eq!(
            code(publisher(&memory).publish(T, None, || 8_000)),
            "projection_indeterminate"
        );
        memory.fault(StoreOp::Put, "projection", Fault::Refuse, 0, 1);
        assert_eq!(
            code(publisher(&memory).publish(T, None, || 8_000)),
            "projection_unavailable"
        );
        assert_eq!(revision(&memory), 2);
        publisher(&memory).publish(T, None, || 8_000).unwrap();
        assert_eq!(revision(&memory), 3);
    }

    #[test]
    fn a_restarted_publisher_needs_a_new_source_read() {
        let memory = world(true, 0x1234);
        publisher(&memory).publish(T, None, || 1_000).unwrap();
        let published = memory.raw(NAME).unwrap();
        memory.fault(StoreOp::Get, "state", Fault::Refuse, 0, 1);
        assert_eq!(
            code(publisher(&memory).publish(T, None, || 2_000)),
            "control_state_unavailable"
        );
        assert_eq!(memory.raw(NAME).unwrap(), published);
    }

    #[test]
    fn every_decision_needs_a_clock_proof() {
        let memory = world(true, 0x1234);
        publisher(&memory).publish(T, None, || 1_000).unwrap();
        let unproven = Reader::new(memory.clone(), NAME, T, S_CTL, None).unwrap();
        assert_eq!(
            code(unproven.poll_at(1_000, 0)),
            "projection_freshness_unproven"
        );
        assert_eq!(
            code(unproven.current_at(1_000, 0)),
            "projection_freshness_unproven"
        );
        assert_eq!(code(unproven.poll()), "projection_freshness_unproven");
    }

    #[test]
    fn a_stalled_publisher_expires_at_the_original_deadline_despite_healthy_reads() {
        let memory = world(true, 0x1234);
        let objects = Objects::new(memory.clone(), SOURCE);
        publisher(&memory).publish(T, None, || 1_000).unwrap();
        let reader = reader(&memory);
        // Received at wall 1_200: 11_000 - 1_200 - 500 = 9_300 ms left.
        assert_eq!(reader.poll_at(1_200, 0).unwrap(), 1);
        // The source suspends T; the publisher has stalled.
        objects
            .apply(2, &control_state::Edit::Suspend(T.into(), true), None, None)
            .unwrap();
        for mono in [1_000, 5_000, 9_299] {
            reader.poll_at(1_200 + mono, mono).unwrap();
            assert!(reader.current_at(1_200 + mono, mono).is_ok());
        }
        // Healthy rereads of the same witness never extend it.
        reader.poll_at(10_500, 9_300).unwrap();
        assert_eq!(code(reader.current_at(10_500, 9_300)), "projection_stale");
        // A new witness from a new source read carries the suspension.
        publisher(&memory).publish(T, None, || 10_600).unwrap();
        assert_eq!(reader.poll_at(10_700, 9_500).unwrap(), 2);
        let projection = reader.current_at(10_700, 9_500).unwrap();
        assert_eq!(code(projection.admit(T)), "tenant_suspended");
        assert_eq!(
            code(projection.admit_grant(&grant("aaaaaaaaaaaa", &[Op::Read]))),
            "tenant_suspended"
        );
    }

    #[test]
    fn a_slow_source_read_consumes_the_same_budget() {
        let memory = world(true, 0x1234);
        let wall = Arc::new(AtomicU64::new(1_000));
        let clock = wall.clone();
        memory.hook(StoreOp::Get, "state", move || {
            clock.fetch_add(9_000, Ordering::SeqCst);
        });
        let projection = publisher(&memory)
            .publish(T, None, || wall.load(Ordering::SeqCst))
            .unwrap();
        assert_eq!(projection.deadline_unix_ms, 1_000 + S_CTL);
        let reader = reader(&memory);
        // At 10_000: 11_000 - 10_000 - 500 = 500 ms left, not a new S_ctl.
        reader.poll_at(10_000, 0).unwrap();
        assert!(reader.current_at(10_499, 499).is_ok());
        assert_eq!(code(reader.current_at(10_500, 500)), "projection_stale");
        // Received too late to be inside the budget at all.
        assert_eq!(
            code(super::tests::reader(&memory).poll_at(10_500, 0)),
            "projection_stale"
        );
    }

    #[test]
    fn replayed_or_altered_witnesses_are_refused() {
        let memory = world(true, 0x1234);
        publisher(&memory).publish(T, None, || 1_000).unwrap();
        let first = memory.raw(NAME).unwrap();
        let reader = reader(&memory);
        reader.poll_at(1_000, 0).unwrap();
        publisher(&memory).publish(T, None, || 2_000).unwrap();
        let second = memory.raw(NAME).unwrap();
        assert_eq!(reader.poll_at(2_000, 1_000).unwrap(), 1);
        memory.set_raw(NAME, &first);
        assert_eq!(code(reader.poll_at(2_100, 1_100)), "projection_regressed");
        // Same revision, different bytes.
        let mut altered: Value = serde_json::from_slice(&second).unwrap();
        altered["deadline_unix_ms"] = json!(2_000 + S_CTL - 1);
        memory.set_raw(NAME, &serde_json::to_vec(&altered).unwrap());
        assert_eq!(code(reader.poll_at(2_200, 1_200)), "projection_regressed");
        // The accepted witness is still the second one, to its own deadline.
        memory.set_raw(NAME, &second);
        reader.poll_at(2_300, 1_300).unwrap();
        let left = 2_000 + S_CTL - 2_000 - U;
        assert!(
            reader
                .current_at(2_000 + left - 1, 1_000 + left - 1)
                .is_ok()
        );
        assert_eq!(
            code(reader.current_at(2_000 + left, 1_000 + left)),
            "projection_stale"
        );
    }

    #[test]
    fn a_wall_clock_regression_is_refused_until_restart() {
        let memory = world(true, 0x1234);
        publisher(&memory).publish(T, None, || 1_000).unwrap();
        let reader = reader(&memory);
        reader.poll_at(1_000, 0).unwrap();
        // Behind the monotonic clock by exactly the uncertainty: accepted.
        assert!(reader.current_at(1_500, 1_000).is_ok());
        // By more: refused, and it sticks once the clock looks right again.
        assert_eq!(
            code(reader.current_at(1_900, 2_001)),
            "projection_clock_regressed"
        );
        assert_eq!(
            code(reader.current_at(5_000, 3_000)),
            "projection_clock_regressed"
        );
        assert_eq!(
            code(reader.poll_at(5_000, 3_000)),
            "projection_clock_regressed"
        );
        // A restarted reader needs the witness to still be inside its
        // deadline by its own (proven) clock.
        let restarted = super::tests::reader(&memory);
        restarted.poll_at(5_000, 0).unwrap();
        assert!(restarted.current_at(5_000, 0).is_ok());
        let late = super::tests::reader(&memory);
        assert_eq!(code(late.poll_at(1_000 + S_CTL - U, 0)), "projection_stale");
        assert_eq!(
            code(late.current_at(1_000 + S_CTL - U, 0)),
            "projection_unavailable"
        );
    }

    #[test]
    fn foreign_unbounded_or_unreadable_projections_are_refused() {
        let memory = world(true, 0x1234);
        let reader = reader(&memory);
        assert_eq!(code(reader.poll_at(1_000, 0)), "projection_missing");
        publisher(&memory).publish(T, None, || 1_000).unwrap();
        let good: Value = serde_json::from_slice(&memory.raw(NAME).unwrap()).unwrap();
        let mut cases = Vec::new();
        for (field, value) in [
            ("tenant_id", json!(OTHER)),
            ("deadline_unix_ms", json!(1_000 + S_CTL + 1)),
            ("deadline_unix_ms", json!(1_000)),
            ("schema", json!("vlite.control-projection/2")),
            ("source_generation", json!(0)),
            ("credentials", json!({})),
        ] {
            let mut bad = good.clone();
            bad[field] = value;
            cases.push(serde_json::to_vec(&bad).unwrap());
        }
        cases.push(vec![b'x'; MAX_PROJECTION_BYTES as usize + 1]);
        for bad in cases {
            memory.set_raw(NAME, &bad);
            assert_eq!(code(reader.poll_at(1_000, 0)), "projection_invalid");
        }
        memory.set_raw(NAME, &serde_json::to_vec(&good).unwrap());
        memory.fault(StoreOp::Get, "projection", Fault::Refuse, 0, 1);
        assert_eq!(code(reader.poll_at(1_000, 0)), "projection_unavailable");
        assert_eq!(code(reader.current_at(1_000, 0)), "projection_unavailable");
        reader.poll_at(1_000, 0).unwrap();
        assert!(reader.current_at(1_000, 0).is_ok());
    }
}
