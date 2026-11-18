//! Contract-only writer authority. The stable object is also the head CAS
//! serialization point; no runtime path uses this protocol yet.

use std::sync::Arc;

use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use super::ApiError;
use super::cloud_backup::{
    Cond, ObjectStore, StoreError, is_sha256, lower_hex, random_hex, valid_set_id,
};

#[cfg_attr(not(test), allow(dead_code))]
pub(super) const MAX_AUTHORITY_BYTES: u64 = 64 * 1024;
#[cfg_attr(not(test), allow(dead_code))]
const SCHEMA: &str = "vlite.writer-authority/1";

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct HeadRef {
    pub epoch: u64,
    pub set_id: String,
    pub manifest_sha256: String,
    pub index_sha256: Option<String>,
    pub sequence: u64,
    pub chain_sha256: String,
}

#[cfg_attr(not(test), allow(dead_code))]
impl HeadRef {
    fn valid(&self) -> bool {
        self.epoch >= 1
            && valid_set_id(&self.set_id)
            && is_sha256(&self.manifest_sha256)
            && self.index_sha256.as_deref().is_none_or(is_sha256)
            && is_sha256(&self.chain_sha256)
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Phase {
    Recovering,
    Serving,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Record {
    pub schema: String,
    pub installation_id: String,
    pub scope_id: String,
    pub lineage: String,
    pub writer_id: Option<String>,
    pub epoch: u64,
    pub revision: String,
    pub phase: Phase,
    pub base: Option<HeadRef>,
    pub head: Option<HeadRef>,
    pub claimed_unix_ms: Option<u64>,
}

#[cfg_attr(not(test), allow(dead_code))]
#[allow(clippy::result_large_err)]
impl Record {
    pub fn validate(&self) -> Result<(), AuthorityError> {
        let valid_id = |id: &str| {
            (1..=128).contains(&id.len())
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        };
        let hex_id = |id: &str, len| id.len() == len && lower_hex(id.as_bytes());
        if self.schema != SCHEMA
            || !valid_id(&self.installation_id)
            || !valid_id(&self.scope_id)
            || !hex_id(&self.lineage, 16)
            || self.epoch == 0
            || !hex_id(&self.revision, 32)
            || self.writer_id.as_deref().is_some_and(|id| !hex_id(id, 32))
            || self
                .base
                .as_ref()
                .is_some_and(|base| !base.valid() || base.epoch >= self.epoch || self.epoch == 1)
        {
            return Err(AuthorityError::Invalid);
        }
        match self.phase {
            Phase::Recovering if self.head.is_none() => Ok(()),
            Phase::Serving
                if self.writer_id.is_some()
                    && self
                        .head
                        .as_ref()
                        .is_some_and(|head| head.valid() && head.epoch == self.epoch) =>
            {
                Ok(())
            }
            _ => Err(AuthorityError::Invalid),
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Observed {
    pub record: Record,
    pub etag: String,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Claim {
    pub writer_id: String,
    pub epoch: u64,
    pub revision: String,
}

// Keep the full observation in HeadMoved so callers can explicitly rebase.
#[allow(clippy::large_enum_variant)]
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug)]
pub(super) enum AuthorityError {
    Missing,
    Exists,
    Lost,
    Fenced { observed_epoch: u64 },
    HeadMoved(Observed),
    Unavailable,
    Unresolved,
    Invalid,
}

impl From<AuthorityError> for ApiError {
    fn from(error: AuthorityError) -> Self {
        let (status, code, message) = match error {
            AuthorityError::Missing => (
                StatusCode::SERVICE_UNAVAILABLE,
                "writer_authority_missing",
                "the writer authority is missing; explicit initialization is required",
            ),
            AuthorityError::Exists => (
                StatusCode::CONFLICT,
                "writer_authority_exists",
                "the writer authority already exists",
            ),
            AuthorityError::Lost | AuthorityError::HeadMoved(_) => (
                StatusCode::CONFLICT,
                "writer_authority_conflict",
                "the writer authority changed; read the current authority before proceeding",
            ),
            AuthorityError::Fenced { .. } => (
                StatusCode::CONFLICT,
                "writer_fenced",
                "this writer no longer holds the authority",
            ),
            AuthorityError::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "writer_authority_unavailable",
                "the writer authority store is unavailable",
            ),
            AuthorityError::Unresolved => (
                StatusCode::SERVICE_UNAVAILABLE,
                "writer_authority_unresolved",
                "the writer authority update could not be confirmed",
            ),
            AuthorityError::Invalid => (
                StatusCode::BAD_GATEWAY,
                "writer_authority_invalid",
                "the writer authority record or requested transition is invalid",
            ),
        };
        ApiError::new(status, code, message)
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) struct AuthorityStore {
    store: Arc<dyn ObjectStore>,
    key: String,
}

#[cfg_attr(not(test), allow(dead_code))]
#[allow(clippy::result_large_err)]
impl AuthorityStore {
    pub fn new(store: Arc<dyn ObjectStore>, prefix: String) -> Self {
        Self {
            store,
            key: format!("{prefix}authority.json"),
        }
    }

    fn fresh_id() -> Result<String, AuthorityError> {
        Ok(format!(
            "{}{}",
            random_hex().map_err(|_| AuthorityError::Unavailable)?,
            random_hex().map_err(|_| AuthorityError::Unavailable)?
        ))
    }

    fn encode(record: &Record) -> Result<Vec<u8>, AuthorityError> {
        record.validate()?;
        let bytes = serde_json::to_vec(record).map_err(|_| AuthorityError::Invalid)?;
        if bytes.len() as u64 > MAX_AUTHORITY_BYTES {
            return Err(AuthorityError::Invalid);
        }
        Ok(bytes)
    }

    fn read_bytes(&self) -> Result<(Observed, Vec<u8>), AuthorityError> {
        let blob = self
            .store
            .get(&self.key, MAX_AUTHORITY_BYTES)
            .map_err(|error| match error {
                StoreError::NotFound => AuthorityError::Missing,
                StoreError::TooLarge => AuthorityError::Invalid,
                _ => AuthorityError::Unavailable,
            })?;
        if blob.bytes.len() as u64 > MAX_AUTHORITY_BYTES {
            return Err(AuthorityError::Invalid);
        }
        let record: Record =
            serde_json::from_slice(&blob.bytes).map_err(|_| AuthorityError::Invalid)?;
        record.validate()?;
        Ok((
            Observed {
                record,
                etag: blob.etag,
            },
            blob.bytes,
        ))
    }

    pub fn read(&self) -> Result<Observed, AuthorityError> {
        self.read_bytes().map(|(observed, _)| observed)
    }

    fn readback(&self, intended: &[u8]) -> Result<(Observed, bool), AuthorityError> {
        let (observed, bytes) = self.read_bytes().map_err(|_| AuthorityError::Unresolved)?;
        Ok((observed, bytes == intended))
    }

    pub fn create(
        &self,
        installation_id: &str,
        scope_id: &str,
        lineage: &str,
    ) -> Result<Observed, AuthorityError> {
        let record = Record {
            schema: SCHEMA.into(),
            installation_id: installation_id.into(),
            scope_id: scope_id.into(),
            lineage: lineage.into(),
            writer_id: None,
            epoch: 1,
            revision: Self::fresh_id()?,
            phase: Phase::Recovering,
            base: None,
            head: None,
            claimed_unix_ms: None,
        };
        let bytes = Self::encode(&record)?;
        match self.store.put(&self.key, &bytes, Cond::Create) {
            Ok(etag) => Ok(Observed { record, etag }),
            Err(StoreError::Conflict) => Err(AuthorityError::Exists),
            Err(StoreError::Unknown(_)) => match self.readback(&bytes)? {
                (observed, true) => Ok(observed),
                (_, false) => Err(AuthorityError::Exists),
            },
            Err(_) => Err(AuthorityError::Unavailable),
        }
    }

    pub fn claim(
        &self,
        prepared: &Observed,
        now_ms: u64,
    ) -> Result<(Claim, Observed), AuthorityError> {
        prepared.record.validate()?;
        if prepared.record.writer_id.is_some() || prepared.record.phase != Phase::Recovering {
            return Err(AuthorityError::Invalid);
        }
        let claim = Claim {
            writer_id: Self::fresh_id()?,
            epoch: prepared.record.epoch,
            revision: Self::fresh_id()?,
        };
        let mut record = prepared.record.clone();
        record.writer_id = Some(claim.writer_id.clone());
        record.revision.clone_from(&claim.revision);
        record.claimed_unix_ms = Some(now_ms);
        let bytes = Self::encode(&record)?;
        let observed = match self
            .store
            .put(&self.key, &bytes, Cond::Match(&prepared.etag))
        {
            Ok(etag) => Observed { record, etag },
            Err(StoreError::Conflict) => return Err(AuthorityError::Lost),
            Err(StoreError::Unknown(_)) => match self.readback(&bytes)? {
                (observed, true) => observed,
                (_, false) => return Err(AuthorityError::Lost),
            },
            Err(_) => return Err(AuthorityError::Unavailable),
        };
        Ok((claim, observed))
    }

    fn check_writer(claim: &Claim, observed: &Observed) -> Result<(), AuthorityError> {
        if observed.record.epoch != claim.epoch
            || observed.record.writer_id.as_deref() != Some(claim.writer_id.as_str())
        {
            return Err(AuthorityError::Fenced {
                observed_epoch: observed.record.epoch,
            });
        }
        Ok(())
    }

    pub fn verify_writer(&self, claim: &Claim) -> Result<Observed, AuthorityError> {
        let observed = self.read()?;
        Self::check_writer(claim, &observed)?;
        Ok(observed)
    }

    pub fn advance_head(
        &self,
        claim: &Claim,
        current: &Observed,
        next: HeadRef,
    ) -> Result<Observed, AuthorityError> {
        current.record.validate()?;
        Self::check_writer(claim, current)?;
        if !next.valid()
            || next.epoch != claim.epoch
            || current
                .record
                .head
                .as_ref()
                .is_some_and(|head| next.sequence < head.sequence)
        {
            return Err(AuthorityError::Invalid);
        }
        let mut record = current.record.clone();
        record.head = Some(next);
        record.phase = Phase::Serving;
        record.revision = Self::fresh_id()?;
        let bytes = Self::encode(&record)?;
        let observed = match self
            .store
            .put(&self.key, &bytes, Cond::Match(&current.etag))
        {
            Ok(etag) => return Ok(Observed { record, etag }),
            Err(StoreError::Conflict) => self.read()?,
            Err(StoreError::Unknown(_)) => match self.readback(&bytes)? {
                (observed, true) => return Ok(observed),
                (observed, false) => observed,
            },
            Err(_) => return Err(AuthorityError::Unavailable),
        };
        Self::check_writer(claim, &observed)?;
        Err(AuthorityError::HeadMoved(observed))
    }

    pub fn prepare_transfer(
        &self,
        current: &Observed,
        expected_epoch: u64,
        expected_revision: &str,
    ) -> Result<Observed, AuthorityError> {
        current.record.validate()?;
        if current.record.epoch != expected_epoch || current.record.revision != expected_revision {
            return Err(AuthorityError::Invalid);
        }
        let mut record = current.record.clone();
        record.epoch = record.epoch.checked_add(1).ok_or(AuthorityError::Invalid)?;
        record.base = record.head.take().or(record.base);
        record.writer_id = None;
        record.phase = Phase::Recovering;
        record.revision = Self::fresh_id()?;
        record.claimed_unix_ms = None;
        let bytes = Self::encode(&record)?;
        match self
            .store
            .put(&self.key, &bytes, Cond::Match(&current.etag))
        {
            Ok(etag) => Ok(Observed { record, etag }),
            Err(StoreError::Conflict) => Err(AuthorityError::Lost),
            Err(StoreError::Unknown(_)) => match self.readback(&bytes)? {
                (observed, true) => Ok(observed),
                (_, false) => Err(AuthorityError::Lost),
            },
            Err(_) => Err(AuthorityError::Unavailable),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::cloud_backup::fake::{Fault, Memory, Op};

    const KEY: &str = "test/authority.json";

    fn setup() -> (Arc<Memory>, Arc<AuthorityStore>) {
        let memory = Arc::new(Memory::default());
        let store = Arc::new(AuthorityStore::new(memory.clone(), "test/".into()));
        (memory, store)
    }

    fn create(store: &AuthorityStore) -> Observed {
        store
            .create("installation", "scope", "0123456789abcdef")
            .unwrap()
    }

    fn head(epoch: u64, sequence: u64) -> HeadRef {
        HeadRef {
            epoch,
            set_id: "20261118T000000Z-0123456789abcdef".into(),
            manifest_sha256: "a".repeat(64),
            index_sha256: Some("b".repeat(64)),
            sequence,
            chain_sha256: "c".repeat(64),
        }
    }

    fn serving(store: &AuthorityStore) -> (Claim, Observed) {
        let prepared = create(store);
        let (claim, claimed) = store.claim(&prepared, 42).unwrap();
        let served = store.advance_head(&claim, &claimed, head(1, 10)).unwrap();
        (claim, served)
    }

    #[test]
    fn authority_create_once() {
        let (memory, store) = setup();
        let first = create(&store);
        assert_eq!(first.record.epoch, 1);
        assert_eq!(first.record.phase, Phase::Recovering);
        assert!(first.record.writer_id.is_none());
        assert!(first.record.head.is_none());
        assert!(first.record.base.is_none());
        assert!(first.record.claimed_unix_ms.is_none());
        let bytes = memory.raw(KEY).unwrap();
        assert!(matches!(
            store.create("other", "scope", "0123456789abcdef"),
            Err(AuthorityError::Exists)
        ));
        assert_eq!(memory.raw(KEY).unwrap(), bytes);
        assert_eq!(memory.names(), vec![KEY]);
        assert!(!String::from_utf8(bytes).unwrap().contains("etag"));
    }

    #[test]
    fn authority_claim_consumes_prepared_revision() {
        let (_, store) = setup();
        let prepared = create(&store);
        let (claim, observed) = store.claim(&prepared, 123).unwrap();
        assert_ne!(observed.etag, prepared.etag);
        assert_ne!(claim.revision, prepared.record.revision);
        assert_eq!(claim.revision, observed.record.revision);
        assert_eq!(observed.record.writer_id.as_ref(), Some(&claim.writer_id));
        assert_eq!(observed.record.claimed_unix_ms, Some(123));
        assert!(matches!(
            store.claim(&prepared, 456),
            Err(AuthorityError::Lost)
        ));
        assert!(matches!(
            store.claim(&observed, 456),
            Err(AuthorityError::Invalid)
        ));
    }

    #[test]
    fn authority_double_claim_has_one_winner() {
        let (memory, store) = setup();
        let prepared = create(&store);
        let rival_store = store.clone();
        let rival_prepared = prepared.clone();
        let winner = Arc::new(std::sync::Mutex::new(None));
        let result = winner.clone();
        memory.hook(Op::Put, KEY, move || {
            *result.lock().unwrap() = Some(rival_store.claim(&rival_prepared, 2).unwrap());
        });
        assert!(matches!(
            store.claim(&prepared, 1),
            Err(AuthorityError::Lost)
        ));
        let (claim, observed) = winner.lock().unwrap().take().unwrap();
        assert_eq!(store.verify_writer(&claim).unwrap(), observed);
    }

    #[test]
    fn authority_copied_config_is_not_same_writer() {
        let (_, first) = setup();
        let (_, second) = setup();
        let (a, _) = first.claim(&create(&first), 1).unwrap();
        let (b, _) = second.claim(&create(&second), 1).unwrap();
        assert_ne!(a.writer_id, b.writer_id);
        assert_eq!(a.writer_id.len(), 32);
        assert_eq!(b.writer_id.len(), 32);
        assert!(matches!(
            first.verify_writer(&b),
            Err(AuthorityError::Fenced { observed_epoch: 1 })
        ));
    }

    #[test]
    fn authority_epoch_checked_increment() {
        let (memory, store) = setup();
        let (_, served) = serving(&store);
        assert!(matches!(
            store.prepare_transfer(&served, 0, &served.record.revision),
            Err(AuthorityError::Invalid)
        ));
        assert!(matches!(
            store.prepare_transfer(&served, 1, &"0".repeat(32)),
            Err(AuthorityError::Invalid)
        ));
        let original = memory.raw(KEY).unwrap();
        let mut malformed: serde_json::Value = serde_json::from_slice(&original).unwrap();
        for (field, value) in [
            ("epoch", serde_json::json!(0)),
            ("schema", serde_json::json!("vlite.writer-authority/2")),
            ("installation_id", serde_json::json!("bad/id")),
            ("scope_id", serde_json::json!("")),
            ("scope_id", serde_json::json!("x".repeat(129))),
            ("lineage", serde_json::json!("0123456789abcdeF")),
            ("revision", serde_json::json!("A".repeat(32))),
            ("writer_id", serde_json::json!("short")),
            ("extra", serde_json::json!(true)),
        ] {
            let mut value_record = malformed.clone();
            value_record[field] = value;
            memory.set_raw(KEY, &serde_json::to_vec(&value_record).unwrap());
            assert!(
                matches!(store.read(), Err(AuthorityError::Invalid)),
                "{field}"
            );
        }
        for field in [
            "manifest_sha256",
            "index_sha256",
            "chain_sha256",
            "set_id",
            "extra",
        ] {
            let mut value_record = malformed.clone();
            value_record["head"][field] = serde_json::json!("invalid");
            memory.set_raw(KEY, &serde_json::to_vec(&value_record).unwrap());
            assert!(
                matches!(store.read(), Err(AuthorityError::Invalid)),
                "{field}"
            );
        }
        malformed["epoch"] = serde_json::json!(u64::MAX);
        malformed["head"]["epoch"] = serde_json::json!(u64::MAX);
        memory.set_raw(KEY, &serde_json::to_vec(&malformed).unwrap());
        let maximum = store.read().unwrap();
        assert!(matches!(
            store.prepare_transfer(&maximum, u64::MAX, &maximum.record.revision),
            Err(AuthorityError::Invalid)
        ));
        assert_eq!(store.read().unwrap(), maximum);
        memory.set_raw(KEY, &original);
        let restored = store.read().unwrap();
        let claim = Claim {
            writer_id: restored.record.writer_id.clone().unwrap(),
            epoch: 1,
            revision: restored.record.revision.clone(),
        };
        assert!(matches!(
            store.advance_head(&claim, &restored, head(1, 9)),
            Err(AuthorityError::Invalid)
        ));
        assert!(matches!(
            store.advance_head(&claim, &restored, head(0, 11)),
            Err(AuthorityError::Invalid)
        ));
        let mut invalid = restored.clone();
        invalid.record.epoch = 0;
        assert!(matches!(
            store.prepare_transfer(&invalid, 0, &invalid.record.revision),
            Err(AuthorityError::Invalid)
        ));
    }

    #[test]
    fn authority_unknown_claim_readback() {
        let (memory, store) = setup();
        let prepared = create(&store);
        memory.fault(Op::Put, KEY, Fault::Before, 0, 1);
        assert!(matches!(
            store.claim(&prepared, 1),
            Err(AuthorityError::Lost)
        ));
        assert_eq!(store.read().unwrap(), prepared);
        memory.fault(Op::Put, KEY, Fault::After, 0, 1);
        let (claim, observed) = store.claim(&prepared, 2).unwrap();
        assert_eq!(store.verify_writer(&claim).unwrap(), observed);

        let prepared = store
            .prepare_transfer(&observed, 1, &observed.record.revision)
            .unwrap();
        memory.fault(Op::Put, KEY, Fault::After, 0, 1);
        let replacement = prepared.record.clone();
        let rival_memory = memory.clone();
        memory.hook(Op::Get, KEY, move || {
            rival_memory.set_raw(KEY, &serde_json::to_vec(&replacement).unwrap());
        });
        assert!(matches!(
            store.claim(&prepared, 3),
            Err(AuthorityError::Lost)
        ));

        let prepared = store.read().unwrap();
        memory.fault(Op::Put, KEY, Fault::After, 0, 1);
        memory.fault(Op::Get, KEY, Fault::Before, 0, 1);
        assert!(matches!(
            store.claim(&prepared, 4),
            Err(AuthorityError::Unresolved)
        ));
    }

    #[test]
    fn authority_missing_does_not_reinitialize() {
        let (memory, store) = setup();
        assert!(matches!(store.read(), Err(AuthorityError::Missing)));
        assert!(memory.names().is_empty());
        let (claim, _) = serving(&store);
        memory.remove(KEY);
        assert!(matches!(
            store.verify_writer(&claim),
            Err(AuthorityError::Missing)
        ));
        assert!(memory.names().is_empty());
    }

    #[test]
    fn head_epoch_bump_invalidates_old_etag() {
        let (memory, store) = setup();
        let (claim, served) = serving(&store);
        let verified = store.verify_writer(&claim).unwrap();
        let transferred = store
            .prepare_transfer(&served, 1, &served.record.revision)
            .unwrap();
        assert_eq!(transferred.record.epoch, 2);
        assert_eq!(transferred.record.base, served.record.head);
        assert!(transferred.record.head.is_none());
        assert!(transferred.record.writer_id.is_none());
        assert!(transferred.record.claimed_unix_ms.is_none());
        let frozen = memory.raw(KEY).unwrap();
        assert!(matches!(
            store.advance_head(&claim, &verified, head(1, 11)),
            Err(AuthorityError::Fenced { observed_epoch: 2 })
        ));
        assert_eq!(memory.raw(KEY).unwrap(), frozen);
        let again = store
            .prepare_transfer(&transferred, 2, &transferred.record.revision)
            .unwrap();
        assert_eq!(again.record.epoch, 3);
        assert_eq!(again.record.base, transferred.record.base);
        assert!(matches!(
            store.prepare_transfer(&served, 1, &served.record.revision),
            Err(AuthorityError::Lost)
        ));
    }

    #[test]
    fn head_unknown_commit_requires_exact_reference() {
        let (memory, store) = setup();
        let (claim, served) = serving(&store);
        memory.fault(Op::Put, KEY, Fault::After, 0, 1);
        let committed = store.advance_head(&claim, &served, head(1, 11)).unwrap();
        assert_eq!(committed.record.head, Some(head(1, 11)));
        memory.fault(Op::Put, KEY, Fault::Before, 0, 1);
        assert!(matches!(
            store.advance_head(&claim, &committed, head(1, 12)),
            Err(AuthorityError::HeadMoved(current)) if current == committed
        ));
        memory.fault(Op::Put, KEY, Fault::After, 0, 1);
        let mut replacement = committed.record.clone();
        // Even the same revision and writer cannot stand in for the exact head.
        replacement.head = Some(head(1, 99));
        let rival_memory = memory.clone();
        memory.hook(Op::Get, KEY, move || {
            let mut intended: Record =
                serde_json::from_slice(&rival_memory.raw(KEY).unwrap()).unwrap();
            intended.head = replacement.head;
            rival_memory.set_raw(KEY, &serde_json::to_vec(&intended).unwrap());
        });
        assert!(matches!(
            store.advance_head(&claim, &committed, head(1, 12)),
            Err(AuthorityError::HeadMoved(current))
                if current.record.head == Some(head(1, 99))
        ));
        let current = store.read().unwrap();
        memory.fault(Op::Put, KEY, Fault::After, 0, 1);
        memory.fault(Op::Get, KEY, Fault::Before, 0, 1);
        assert!(matches!(
            store.advance_head(&claim, &current, head(1, 100)),
            Err(AuthorityError::Unresolved)
        ));
    }

    #[test]
    fn head_moved_same_writer_is_rebased_not_overwritten() {
        let (memory, store) = setup();
        let (claim, served) = serving(&store);
        let advanced = store.advance_head(&claim, &served, head(1, 20)).unwrap();
        let bytes = memory.raw(KEY).unwrap();
        let moved = match store.advance_head(&claim, &served, head(1, 11)) {
            Err(AuthorityError::HeadMoved(current)) => current,
            other => panic!("expected head moved, got {other:?}"),
        };
        assert_eq!(moved, advanced);
        assert_eq!(memory.raw(KEY).unwrap(), bytes);
        assert!(matches!(
            store.advance_head(&claim, &moved, head(1, 11)),
            Err(AuthorityError::Invalid)
        ));
        assert_eq!(
            store
                .advance_head(&claim, &moved, head(1, 21))
                .unwrap()
                .record
                .head,
            Some(head(1, 21))
        );
    }

    #[test]
    fn verify_writer_fences_on_new_epoch() {
        let (memory, store) = setup();
        let (claim, served) = serving(&store);
        store
            .prepare_transfer(&served, 1, &served.record.revision)
            .unwrap();
        assert!(matches!(
            store.verify_writer(&claim),
            Err(AuthorityError::Fenced { observed_epoch: 2 })
        ));
        memory.fault(Op::Get, KEY, Fault::Before, 0, 1);
        assert!(matches!(
            store.verify_writer(&claim),
            Err(AuthorityError::Unavailable)
        ));
        memory.fault(Op::Get, KEY, Fault::Refuse, 0, 1);
        assert!(matches!(
            store.verify_writer(&claim),
            Err(AuthorityError::Unavailable)
        ));
    }

    #[test]
    fn serving_requires_head_of_same_epoch() {
        let (_, store) = setup();
        let mut record = create(&store).record;
        record.phase = Phase::Serving;
        assert!(matches!(record.validate(), Err(AuthorityError::Invalid)));
        record.writer_id = Some("1".repeat(32));
        assert!(matches!(record.validate(), Err(AuthorityError::Invalid)));
        record.head = Some(head(2, 1));
        assert!(matches!(record.validate(), Err(AuthorityError::Invalid)));
        record.head = Some(head(1, 1));
        assert!(record.validate().is_ok());
        record.base = Some(head(1, 0));
        assert!(matches!(record.validate(), Err(AuthorityError::Invalid)));
        record.epoch = 2;
        record.head = Some(head(2, 1));
        assert!(record.validate().is_ok());
        record.base = Some(head(0, 0));
        assert!(matches!(record.validate(), Err(AuthorityError::Invalid)));
        record.base = None;
        record.phase = Phase::Recovering;
        assert!(matches!(record.validate(), Err(AuthorityError::Invalid)));
        record.head = None;
        assert!(record.validate().is_ok());
        record.writer_id = None;
        assert!(record.validate().is_ok());
    }

    #[test]
    fn authority_create_and_transfer_unknown_outcomes() {
        let (memory, store) = setup();
        memory.fault(Op::Put, KEY, Fault::Before, 0, 1);
        assert!(matches!(
            store.create("installation", "scope", "0123456789abcdef"),
            Err(AuthorityError::Unresolved)
        ));
        memory.fault(Op::Put, KEY, Fault::After, 0, 1);
        let prepared = create(&store);
        memory.fault(Op::Put, KEY, Fault::After, 0, 1);
        let transferred = store
            .prepare_transfer(&prepared, 1, &prepared.record.revision)
            .unwrap();
        assert_eq!(transferred.record.epoch, 2);
        memory.fault(Op::Put, KEY, Fault::Before, 0, 1);
        assert!(matches!(
            store.prepare_transfer(&transferred, 2, &transferred.record.revision),
            Err(AuthorityError::Lost)
        ));
        memory.fault(Op::Put, KEY, Fault::After, 0, 1);
        memory.fault(Op::Get, KEY, Fault::Before, 0, 1);
        assert!(matches!(
            store.prepare_transfer(&transferred, 2, &transferred.record.revision),
            Err(AuthorityError::Unresolved)
        ));
    }

    #[test]
    fn authority_reads_are_bounded_and_errors_are_sanitized() {
        let (memory, store) = setup();
        memory.set_raw(KEY, &vec![b' '; MAX_AUTHORITY_BYTES as usize + 1]);
        assert!(matches!(store.read(), Err(AuthorityError::Invalid)));
        memory.set_raw(KEY, b"not-json");
        assert!(matches!(store.read(), Err(AuthorityError::Invalid)));
        let prepared = {
            memory.remove(KEY);
            create(&store)
        };
        for fault in [Fault::Before, Fault::Refuse] {
            memory.fault(Op::Get, KEY, fault, 0, 1);
            assert!(matches!(store.read(), Err(AuthorityError::Unavailable)));
        }
        memory.fault(Op::Put, KEY, Fault::Refuse, 0, 1);
        assert!(matches!(
            store.claim(&prepared, 1),
            Err(AuthorityError::Unavailable)
        ));
        for (error, status, code) in [
            (
                AuthorityError::Missing,
                StatusCode::SERVICE_UNAVAILABLE,
                "writer_authority_missing",
            ),
            (
                AuthorityError::Exists,
                StatusCode::CONFLICT,
                "writer_authority_exists",
            ),
            (
                AuthorityError::Lost,
                StatusCode::CONFLICT,
                "writer_authority_conflict",
            ),
            (
                AuthorityError::Fenced { observed_epoch: 2 },
                StatusCode::CONFLICT,
                "writer_fenced",
            ),
            (
                AuthorityError::HeadMoved(prepared),
                StatusCode::CONFLICT,
                "writer_authority_conflict",
            ),
            (
                AuthorityError::Unavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                "writer_authority_unavailable",
            ),
            (
                AuthorityError::Unresolved,
                StatusCode::SERVICE_UNAVAILABLE,
                "writer_authority_unresolved",
            ),
            (
                AuthorityError::Invalid,
                StatusCode::BAD_GATEWAY,
                "writer_authority_invalid",
            ),
        ] {
            let api = ApiError::from(error);
            assert_eq!(api.status, status);
            assert_eq!(api.code, code);
            assert!(!api.message.contains("injected"));
            assert!(!api.message.contains(&store.read().unwrap().etag));
        }
    }
}
