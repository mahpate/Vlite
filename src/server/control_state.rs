//! Shared control state (feature-aca-ha ITEM-008/011, FR-003, FR-012).
//!
//! One small versioned JSON object in the coordination store carries the
//! credential and tenant state every gateway replica authorizes from, with a
//! monotonically increasing generation `A`. Writers change it only by
//! compare-and-swap on the object's ETag (`If-Match`, or `If-None-Match: *`
//! for the first generation); a lost race re-reads and retries a bounded
//! number of times, and an unknown outcome is reported, never retried
//! blindly. Readers poll it into a disposable cache that is served for at
//! most S_ctl after the last confirmed read and refused after that (FM-004:
//! fail closed, never allow by default). S_ctl is injected (OD-005).
//!
//! The snapshot holds secret hashes, so it is as sensitive as
//! `credentials.json` (SEC-004). It never holds signing keys or plaintext
//! secrets: unknown fields are refused, and credentials are only imported
//! from a live store, never from a recovery inventory (rekey-only).
// Consumed by the gateway (EPIC-004); exercised by the tests here.
#![cfg_attr(not(test), allow(dead_code))]
use super::cloud_backup::{Cond, ObjectStore, StoreError};
use super::*;
use serde::Serialize;
use std::sync::PoisonError;

pub(super) const SCHEMA: &str = "vlite.control-state/1";
/// Same bound as `credentials.json` and `tenants.json`.
const MAX_BYTES: u64 = 1024 * 1024;
/// Compare-and-swap attempts before a write reports contention.
const CAS_ATTEMPTS: usize = 4;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Snapshot {
    schema: String,
    /// `A`: 1 for the first published snapshot, +1 per accepted write.
    generation: u64,
    written_unix_ms: u64,
    tenants: Vec<tenants::Tenant>,
    credentials: credentials::Snapshot,
}

fn unavailable(code: &'static str, message: &'static str) -> ApiError {
    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, code, message)
}

impl Snapshot {
    /// The current local credentials and tenants, hashes and revocation and
    /// suspension state verbatim. Not yet published (generation 0).
    pub(super) fn import(
        credentials: &credentials::Store,
        tenants: Option<&tenants::Store>,
    ) -> Result<Self, ApiError> {
        Ok(Self {
            schema: SCHEMA.to_owned(),
            generation: 0,
            written_unix_ms: 0,
            tenants: match tenants {
                Some(tenants) => tenants.snapshot()?,
                None => Vec::new(),
            },
            credentials: credentials.snapshot()?,
        })
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    /// Authenticates a `vlk_` token against this snapshot, with the same
    /// credential and tenant admission rules as a standalone server.
    pub(super) fn authenticate(
        &self,
        token: &str,
        now: u64,
    ) -> Result<Arc<credentials::Grant>, ApiError> {
        let grant = self.credentials.authenticate(token, now)?;
        if let Some(tenant) = &grant.tenant_id {
            tenants::admit(&self.tenants, tenant)?;
        }
        Ok(grant)
    }

    fn invalid() -> ApiError {
        unavailable("control_state_invalid", "control state object is invalid")
    }

    fn parse(bytes: &[u8]) -> Result<Self, ApiError> {
        let snapshot: Self = serde_json::from_slice(bytes).map_err(|_| Self::invalid())?;
        if snapshot.schema != SCHEMA
            || snapshot.generation == 0
            || !tenants::valid_store(&snapshot.tenants)
            || !snapshot.credentials.valid()
        {
            return Err(Self::invalid());
        }
        Ok(snapshot)
    }

    /// `self` stamped as generation `generation`, serialized.
    fn publish(mut self, generation: u64, now: u64) -> Result<(Self, Vec<u8>), ApiError> {
        self.schema = SCHEMA.to_owned();
        self.generation = generation;
        self.written_unix_ms = now;
        let bytes = serde_json::to_vec(&self).map_err(|_| Self::invalid())?;
        // Never publish what a reader would refuse.
        Self::parse(&bytes)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(Self::invalid());
        }
        Ok((self, bytes))
    }
}

/// The control-state object in a coordination store.
pub(super) struct Objects {
    store: Arc<dyn ObjectStore>,
    name: String,
}

impl Objects {
    pub(super) fn new(store: Arc<dyn ObjectStore>, name: impl Into<String>) -> Self {
        Self {
            store,
            name: name.into(),
        }
    }

    /// The published snapshot and its ETag.
    pub(super) fn read(&self) -> Result<(Snapshot, String), ApiError> {
        let blob = self
            .store
            .get(&self.name, MAX_BYTES)
            .map_err(|error| match error {
                StoreError::NotFound => {
                    unavailable("control_state_missing", "control state is not published")
                }
                StoreError::TooLarge => Snapshot::invalid(),
                _ => unavailable(
                    "control_state_unavailable",
                    "control state could not be read",
                ),
            })?;
        Ok((Snapshot::parse(&blob.bytes)?, blob.etag))
    }

    fn write(&self, bytes: &[u8], cond: Cond<'_>) -> Result<(), Option<ApiError>> {
        match self.store.put(&self.name, bytes, cond) {
            Ok(_) => Ok(()),
            Err(StoreError::Conflict) => Err(None),
            Err(StoreError::Unknown(_)) => Err(Some(unavailable(
                "control_state_indeterminate",
                "control state write outcome is unknown; read it before retrying",
            ))),
            Err(_) => Err(Some(unavailable(
                "control_state_unavailable",
                "control state could not be written",
            ))),
        }
    }

    /// Publishes `snapshot` as generation 1; refused if any exists.
    pub(super) fn create(&self, snapshot: Snapshot, now: u64) -> Result<Snapshot, ApiError> {
        let (snapshot, bytes) = snapshot.publish(1, now)?;
        match self.write(&bytes, Cond::Create) {
            Ok(()) => Ok(snapshot),
            Err(None) => Err(ApiError::new(
                StatusCode::CONFLICT,
                "control_state_exists",
                "control state is already published",
            )),
            Err(Some(error)) => Err(error),
        }
    }

    /// Replaces the snapshot with `change(current)` at generation `A + 1`.
    pub(super) fn update(
        &self,
        now: u64,
        mut change: impl FnMut(&Snapshot) -> Result<Snapshot, ApiError>,
    ) -> Result<Snapshot, ApiError> {
        for _ in 0..CAS_ATTEMPTS {
            let (current, etag) = self.read()?;
            let generation = current
                .generation
                .checked_add(1)
                .ok_or_else(Snapshot::invalid)?;
            let (next, bytes) = change(&current)?.publish(generation, now)?;
            match self.write(&bytes, Cond::Match(&etag)) {
                Ok(()) => return Ok(next),
                // Lost the race: re-read and apply the change to the winner.
                Err(None) => continue,
                Err(Some(error)) => return Err(error),
            }
        }
        Err(ApiError::new(
            StatusCode::CONFLICT,
            "control_state_conflict",
            "control state is changing too often; retry later",
        ))
    }
}

struct Cached {
    snapshot: Arc<Snapshot>,
    etag: String,
    /// Monotonic time of the last read that confirmed this snapshot.
    confirmed: u64,
}

/// A replica's disposable cache of the control state.
pub(super) struct Poller {
    objects: Objects,
    s_ctl_ms: u64,
    cache: Mutex<Option<Cached>>,
}

impl Poller {
    pub(super) fn new(objects: Objects, s_ctl_ms: u64) -> Result<Self, &'static str> {
        if s_ctl_ms == 0 {
            return Err("S_ctl must be positive");
        }
        Ok(Self {
            objects,
            s_ctl_ms,
            cache: Mutex::new(None),
        })
    }

    /// Reads the object at monotonic time `now` (ms) and returns the cached
    /// generation. A failed read leaves the cache and its age unchanged.
    pub(super) fn poll(&self, now: u64) -> Result<u64, ApiError> {
        let (snapshot, etag) = self.objects.read()?;
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        match cache.as_mut() {
            Some(cached) if cached.etag == etag => cached.confirmed = cached.confirmed.max(now),
            Some(cached) if snapshot.generation <= cached.snapshot.generation => {
                return Err(unavailable(
                    "control_state_regressed",
                    "control state generation went backwards",
                ));
            }
            _ => {
                *cache = Some(Cached {
                    snapshot: Arc::new(snapshot),
                    etag,
                    confirmed: now,
                })
            }
        }
        Ok(cache
            .as_ref()
            .map_or(0, |cached| cached.snapshot.generation))
    }

    /// The snapshot to authorize with at monotonic time `now`, or 503 when
    /// none was read or it was last confirmed more than S_ctl ago.
    pub(super) fn current(&self, now: u64) -> Result<Arc<Snapshot>, ApiError> {
        let cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        let cached = cache.as_ref().ok_or_else(|| {
            unavailable(
                "control_state_unavailable",
                "control state has not been read",
            )
        })?;
        match now.checked_sub(cached.confirmed) {
            Some(age) if age <= self.s_ctl_ms => Ok(cached.snapshot.clone()),
            _ => Err(unavailable(
                "control_state_stale",
                "control state is older than its staleness bound",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::cloud_backup::fake::{Fault, Memory, Op};
    use super::super::principal_envelope::{self as envelope, tests as env};
    use super::*;

    const T1: &str = "0123456789abcdef0123456789abcdef";
    const T2: &str = "fedcba9876543210fedcba9876543210";
    const NAME: &str = "control/state.json";

    struct World {
        memory: Arc<Memory>,
        credentials: credentials::Store,
        tenants: tenants::Store,
        tokens: Vec<String>,
    }

    fn world() -> World {
        let (credentials, tokens) = credentials::tests::issued(
            &[
                ("aaaaaaaaaaaa", Some(T1)),
                ("bbbbbbbbbbbb", Some(T2)),
                ("cccccccccccc", None),
            ],
            0x77,
        );
        World {
            memory: Arc::new(Memory::default()),
            credentials,
            tenants: tenants::Store::for_test(vec![
                tenants::Tenant::for_test(T1, None),
                tenants::Tenant::for_test(T2, None),
            ]),
            tokens,
        }
    }

    impl World {
        fn objects(&self) -> Objects {
            Objects::new(self.memory.clone(), NAME)
        }
        fn import(&self) -> Snapshot {
            Snapshot::import(&self.credentials, Some(&self.tenants)).unwrap()
        }
    }

    #[test]
    fn import_carries_hashes_and_state_but_no_plaintext_secret() {
        let w = world();
        credentials::tests::revoke(&w.credentials, "cccccccccccc", 5);
        let published = w.objects().create(w.import(), 10).unwrap();
        assert_eq!(published.generation(), 1);
        let raw = String::from_utf8(w.memory.raw(NAME).unwrap()).unwrap();
        for token in &w.tokens {
            let secret = token.rsplit('_').next().unwrap();
            assert!(!raw.contains(secret), "plaintext secret in control state");
        }
        assert!(raw.contains("secret_sha256") && raw.contains(SCHEMA));
        let (read, _) = w.objects().read().unwrap();
        // Same rules as standalone: active works, revoked is refused, a
        // tenant credential is admitted only while its tenant is active.
        let grant = read.authenticate(&w.tokens[0], 20).unwrap();
        assert_eq!(grant.tenant_id.as_deref(), Some(T1));
        assert_eq!(grant.lineage, 0x77);
        assert_eq!(
            read.authenticate(&w.tokens[2], 20).err().unwrap().status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            read.authenticate(
                "vlk_aaaaaaaaaaaa_wrongwrongwrongwrongwrongwrongwrongwrongwro",
                20
            )
            .err()
            .unwrap()
            .status,
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn tenanted_credentials_need_their_tenant_in_the_snapshot() {
        let w = world();
        let alone = Snapshot::import(&w.credentials, None).unwrap();
        assert_eq!(
            alone.authenticate(&w.tokens[0], 1).err().unwrap().code,
            "tenant_unavailable"
        );
        assert!(alone.authenticate(&w.tokens[2], 1).is_ok());
    }

    #[test]
    fn parse_refuses_unknown_schema_fields_and_invalid_content() {
        let w = world();
        w.objects().create(w.import(), 1).unwrap();
        let good: Value = serde_json::from_slice(&w.memory.raw(NAME).unwrap()).unwrap();
        let mut cases = Vec::new();
        let mut v = good.clone();
        v["schema"] = json!("vlite.control-state/2");
        cases.push(v);
        let mut v = good.clone();
        v["signing_key"] = json!("00");
        cases.push(v);
        let mut v = good.clone();
        v["generation"] = json!(0);
        cases.push(v);
        let mut v = good.clone();
        v["credentials"][0]["secret_sha256"] = json!("not-a-hash");
        cases.push(v);
        let mut v = good.clone();
        v["credentials"][1] = v["credentials"][0].clone();
        cases.push(v);
        let mut v = good.clone();
        v["tenants"][1] = v["tenants"][0].clone();
        cases.push(v);
        // A recovery inventory entry has no hash: rekey-only, never imported.
        let mut v = good.clone();
        v["credentials"][0] = json!({"id":"aaaaaaaaaaaa","name":"svc","tenant_id":null,
            "collections":["*"],"operations":["read"],"state":"active","expires_unix_ms":1});
        cases.push(v);
        for case in cases {
            w.memory.set_raw(NAME, &serde_json::to_vec(&case).unwrap());
            let error = w.objects().read().err().expect("refused");
            assert_eq!(error.code, "control_state_invalid");
        }
        w.memory.set_raw(NAME, b"{");
        assert_eq!(
            w.objects().read().err().unwrap().code,
            "control_state_invalid"
        );
    }

    #[test]
    fn create_is_exclusive_and_update_advances_generation_by_cas() {
        let w = world();
        w.objects().create(w.import(), 1).unwrap();
        let again = w.objects().create(w.import(), 2).err().unwrap();
        assert_eq!(again.status, StatusCode::CONFLICT);
        let next = w.objects().update(3, |_| Ok(w.import())).unwrap();
        assert_eq!(next.generation(), 2);
        assert_eq!(w.objects().read().unwrap().0.generation(), 2);
    }

    #[test]
    fn update_retries_a_lost_race_against_the_new_state() {
        let w = world();
        w.objects().create(w.import(), 1).unwrap();
        let mut seen = Vec::new();
        let other = w.memory.clone();
        let mut first = true;
        let result = w
            .objects()
            .update(3, |current| {
                seen.push(current.generation());
                if first {
                    first = false;
                    // Concurrent writer publishes generation 3 first.
                    Objects::new(other.clone(), NAME)
                        .update(3, |s| Ok(s.clone()))
                        .unwrap();
                }
                Ok(current.clone())
            })
            .unwrap();
        assert_eq!(seen, vec![1, 2]);
        assert_eq!(result.generation(), 3);
    }

    #[test]
    fn update_reports_unknown_outcomes_and_exhausted_contention() {
        let w = world();
        w.objects().create(w.import(), 1).unwrap();
        w.memory.fault(Op::Put, NAME, Fault::After, 0, 1);
        let error = w.objects().update(2, |s| Ok(s.clone())).err().unwrap();
        assert_eq!(error.code, "control_state_indeterminate");
        assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
        // Applied despite the unknown result; never re-applied blindly.
        assert_eq!(w.objects().read().unwrap().0.generation(), 2);

        let other = w.memory.clone();
        let error = w
            .objects()
            .update(3, |s| {
                Objects::new(other.clone(), NAME)
                    .update(3, |s| Ok(s.clone()))
                    .unwrap();
                Ok(s.clone())
            })
            .err()
            .unwrap();
        assert_eq!(error.code, "control_state_conflict");
        assert_eq!(error.status, StatusCode::CONFLICT);
    }

    #[test]
    fn poller_fails_closed_without_a_snapshot_and_after_s_ctl() {
        let w = world();
        let poller = Poller::new(w.objects(), 5_000).unwrap();
        assert!(Poller::new(w.objects(), 0).is_err());
        assert_eq!(
            poller.current(0).err().unwrap().code,
            "control_state_unavailable"
        );
        assert_eq!(poller.poll(0).err().unwrap().code, "control_state_missing");
        w.objects().create(w.import(), 1).unwrap();
        assert_eq!(poller.poll(100).unwrap(), 1);
        // TEST-014: served up to S_ctl after the last confirmation, then 503.
        assert!(poller.current(5_100).is_ok());
        let stale = poller.current(5_101).err().unwrap();
        assert_eq!(
            (stale.status, stale.code),
            (StatusCode::SERVICE_UNAVAILABLE, "control_state_stale")
        );
        // Unreachable store: the age is not refreshed, so it stays closed.
        w.memory.fault(Op::Get, NAME, Fault::Before, 0, 1);
        assert!(poller.poll(5_200).is_err());
        assert_eq!(
            poller.current(5_200).err().unwrap().code,
            "control_state_stale"
        );
        // A confirming read with the same ETag refreshes the age.
        let poller = Poller::new(w.objects(), 5_000).unwrap();
        assert_eq!(poller.poll(10).unwrap(), 1);
        assert_eq!(poller.poll(5_000).unwrap(), 1);
        assert!(poller.current(10_000).is_ok());
        // A clock that moves backwards is refused, not trusted.
        assert_eq!(
            poller.current(4_999).err().unwrap().code,
            "control_state_stale"
        );
    }

    #[test]
    fn poller_refuses_a_generation_regression() {
        let w = world();
        w.objects().create(w.import(), 1).unwrap();
        let old = w.memory.raw(NAME).unwrap();
        w.objects().update(2, |s| Ok(s.clone())).unwrap();
        let poller = Poller::new(w.objects(), 1_000).unwrap();
        assert_eq!(poller.poll(0).unwrap(), 2);
        w.memory.set_raw(NAME, &old);
        let error = poller.poll(500).err().unwrap();
        assert_eq!(error.code, "control_state_regressed");
        // Neither replaced nor refreshed by the regressed read.
        assert_eq!(poller.current(1_000).unwrap().generation(), 2);
        assert!(poller.current(1_001).is_err());
    }

    /// TEST-004 (library level): a revocation and a suspension published as
    /// generation 2 reach a replica on its next poll; a backend told the new
    /// generation refuses envelopes minted from the old snapshot; beyond
    /// S_ctl with the store unreachable everything fails closed.
    #[test]
    fn revocation_and_suspension_propagate_and_old_generations_are_refused() {
        let w = world();
        let objects = w.objects();
        objects.create(w.import(), 1).unwrap();
        let stale_replica = Poller::new(w.objects(), 5_000).unwrap();
        let fresh_replica = Poller::new(w.objects(), 5_000).unwrap();
        stale_replica.poll(0).unwrap();
        fresh_replica.poll(0).unwrap();
        let before = stale_replica.current(0).unwrap();
        let grant = before.authenticate(&w.tokens[1], 1).unwrap();
        assert_eq!(grant.tenant_id.as_deref(), Some(T2));

        credentials::tests::revoke(&w.credentials, "aaaaaaaaaaaa", 2);
        w.tenants.suspend_for_test(T2, 2);
        let published = objects.update(3, |_| Ok(w.import())).unwrap();
        assert_eq!(published.generation(), 2);

        assert_eq!(fresh_replica.poll(1_000).unwrap(), 2);
        let after = fresh_replica.current(1_000).unwrap();
        assert_eq!(
            after.authenticate(&w.tokens[0], 4).err().unwrap().status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            after.authenticate(&w.tokens[1], 4).err().unwrap().code,
            "tenant_suspended"
        );
        assert!(after.authenticate(&w.tokens[2], 4).is_ok());

        // A backend that learned A=2 refuses an envelope minted at A=1.
        let key = env::signer("k1", 1);
        let verifier =
            envelope::Verifier::new(vec![("k1".into(), key.public_key())], env::policy(Some(T2)))
                .unwrap();
        verifier.raise_minimum(published.generation());
        let old = envelope::Claims {
            aud: "backend-a".into(),
            iat: 10,
            exp: 20,
            generation: before.generation(),
            principal: credentials::Principal::of(&grant),
        };
        let token = key.sign(&old).unwrap();
        assert_eq!(
            verifier.verify(&token, 10).err(),
            Some(envelope::Refusal::Stale)
        );

        // The replica that cannot reach the store stops within S_ctl.
        w.memory.fault(Op::Get, NAME, Fault::Before, 0, 100);
        assert!(stale_replica.poll(1_000).is_err());
        assert!(stale_replica.current(5_000).is_ok());
        assert_eq!(
            stale_replica.current(5_001).err().unwrap().code,
            "control_state_stale"
        );
    }
}
