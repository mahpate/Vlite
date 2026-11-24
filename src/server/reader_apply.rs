//! feature-aca-ha ITEM-019: a reader that follows the writer's archived,
//! verified checkpoints. The reader serves one private follower file
//! read-only while it advances a second one; a verified, advanced file is
//! opened read-only and swapped in under the database write lock, so a read
//! sees either the old generation or the new one, never a partial apply.

use std::sync::{
    Mutex, PoisonError,
    atomic::{AtomicU64, Ordering},
};

use axum::http::StatusCode;
use serde_json::json;

use super::{ApiError, Service, log_stderr};
use crate::Database;

mod follower;
pub(in crate::server) use follower::pin_reason;
pub use follower::{ReaderApply, ReaderApplyConfig};

/// Apply progress shared by every clone and generation of one reader.
#[derive(Default)]
pub(super) struct Progress {
    /// Set once when applying stops for good; only a restart re-enrolls.
    failed: Mutex<Option<String>>,
    applied_sequence: AtomicU64,
    /// When the reader last confirmed that it serves the newest verified
    /// checkpoint of the archive it follows.
    confirmed_unix_ms: AtomicU64,
    /// The configured staleness bound in milliseconds; 0 until set.
    bound_ms: AtomicU64,
    /// The epoch of the authority head the served generation was last
    /// confirmed against; 0 when the reader follows no authority.
    epoch: AtomicU64,
}

impl Progress {
    /// Stops applying; readiness fails from now on. The first reason wins.
    pub(super) fn fail(&self, reason: String) {
        let mut failed = self.failed.lock().unwrap_or_else(PoisonError::into_inner);
        if failed.is_none() {
            log_stderr(
                "error",
                "reader_apply",
                json!({"event":"reader_apply_failed","reason":reason}).to_string(),
            );
            *failed = Some(reason);
        }
    }

    pub(super) fn failure(&self) -> Option<String> {
        self.failed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Records that the served generation is at `sequence`, the newest
    /// verified checkpoint as of `at` (Unix milliseconds).
    pub(super) fn confirm(&self, sequence: u64, at: u64) {
        self.applied_sequence.store(sequence, Ordering::SeqCst);
        self.confirmed_unix_ms.store(at, Ordering::SeqCst);
    }

    /// The served sequence and when it was last confirmed current.
    pub(super) fn applied(&self) -> (u64, u64) {
        (
            self.applied_sequence.load(Ordering::SeqCst),
            self.confirmed_unix_ms.load(Ordering::SeqCst),
        )
    }

    /// Sets the staleness bound (ITEM-020): beyond it the reader fails
    /// readiness and refuses reads.
    pub(super) fn set_bound(&self, bound_ms: u64) {
        self.bound_ms.store(bound_ms, Ordering::SeqCst);
    }

    /// The staleness bound in milliseconds; 0 when none is set.
    pub(super) fn bound(&self) -> u64 {
        self.bound_ms.load(Ordering::SeqCst)
    }

    /// Records the epoch of the authority head the served generation was
    /// just confirmed against (ITEM-021). Set after the swap, so a response
    /// can carry an older epoch, which a gateway refuses, never a newer one.
    pub(super) fn set_epoch(&self, epoch: u64) {
        self.epoch.store(epoch, Ordering::SeqCst);
    }

    /// The epoch a reader stamps on responses, `None` without an authority.
    pub(super) fn epoch(&self) -> Option<u64> {
        match self.epoch.load(Ordering::SeqCst) {
            0 => None,
            epoch => Some(epoch),
        }
    }

    /// Milliseconds since the served generation was last confirmed current,
    /// or `None` before the first confirmation. A clock that went backwards
    /// reads as age 0.
    pub(super) fn age_ms(&self, now_unix_ms: u64) -> Option<u64> {
        match self.confirmed_unix_ms.load(Ordering::SeqCst) {
            0 => None,
            at => Some(now_unix_ms.saturating_sub(at)),
        }
    }
}

/// Readiness answer of a reader that stopped applying. The reason is kept
/// in the log; positions it may name are not part of the response.
pub(super) fn failed() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "reader_apply_failed",
        "this reader stopped following the writer's archive and needs a re-seed; restart it to enroll again",
    )
}

impl Service {
    /// Attaches archive freshness and failure admission to this reader.
    pub fn with_reader_apply(mut self, apply: &ReaderApply) -> Self {
        self.reader = Some(apply.progress());
        self
    }

    /// The readiness refusal of a reader that stopped applying, if any.
    pub(super) fn reader_failure(&self) -> Option<ApiError> {
        self.reader
            .as_ref()
            .and_then(|progress| progress.failure())
            .map(|_| failed())
    }

    /// Replaces the served read-only database with `next`, a read-only
    /// handle on an advanced copy of the same file family. The swap holds
    /// the database write lock only for the exchange; the retired handle is
    /// closed afterwards, which releases its shared file lock so its file
    /// can be advanced again. A refused `next` is closed and the served
    /// generation is kept.
    pub(super) fn swap_reader_generation(&self, next: Database) -> std::io::Result<()> {
        let refusal = if !next.is_read_only() {
            Some("the new generation is not read-only")
        } else if next.is_encrypted() {
            Some("the new generation is encrypted")
        } else if self.lineage.is_none() || next.lineage() != self.lineage {
            Some("the new generation has another lineage")
        } else if next.format_version() != self.format_version {
            Some("the new generation has another format version")
        } else if next.is_graph_enabled() != self.graph_enabled {
            Some("the new generation has another graph setting")
        } else {
            None
        };
        if let Some(reason) = refusal {
            return refuse(next, reason);
        }
        let previous = {
            let mut owner = self
                .database
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            match owner.as_mut() {
                Some(current) if current.is_read_only() => Ok(std::mem::replace(current, next)),
                Some(_) => Err((next, "the served database is writable")),
                None => Err((next, "the database is closed")),
            }
        };
        match previous {
            Ok(previous) => previous
                .close()
                .map_err(|error| std::io::Error::other(error.to_string())),
            Err((next, reason)) => refuse(next, reason),
        }
    }
}

/// Closes a refused generation outside the database lock.
fn refuse(next: Database, reason: &str) -> std::io::Result<()> {
    let _ = next.close();
    Err(std::io::Error::other(format!(
        "reader generation swap refused: {reason}"
    )))
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;

    use super::{Progress, ReaderApply, ReaderApplyConfig, follower::Step};
    use crate::server::{
        cloud_backup::{
            self as cloud, PinKind, Record, SetState,
            fake::{Fault, Op},
        },
        cloud_recovery::tests::{Env, checkpoint, puts, setup},
    };
    use crate::{
        Database,
        server::{ServeMode, Service, router},
    };

    fn config(env: &Env) -> ReaderApplyConfig {
        ReaderApplyConfig {
            target_file: env.dir.path().join("target.json"),
            shared_key_file: None,
            follower_dir: env.dir.path().join("followers"),
            capacity_bytes: cloud::MAX_DATABASE_BYTES,
            staleness_bound_ms: 30_000,
            poll_interval_ms: 10_000,
        }
    }

    fn enroll(env: &Env) -> ReaderApply {
        ReaderApply::enroll_with(env.sets.clone(), config(env), "replica", 1_000)
            .unwrap_or_else(|e| panic!("{}: {}", e.code, e.message))
    }

    fn slot(env: &Env, name: &str) -> std::path::PathBuf {
        config(env).follower_dir.join(name).join("database.vlite")
    }

    fn record(env: &Env) -> Record {
        env.sets
            .list()
            .unwrap()
            .sets
            .into_iter()
            .find(|set| set.set_id == env.set_id)
            .unwrap()
            .record
            .unwrap()
    }

    fn extra_set(env: &Env, newer: bool, state: SetState) -> String {
        let mut record = record(env);
        record.created_unix_ms = if newer {
            record.created_unix_ms + 10_000
        } else {
            record.created_unix_ms - 10_000
        };
        record.set_id = cloud::new_set_id(record.created_unix_ms).unwrap();
        record.state = state;
        record.pins.clear();
        env.meta.set_raw(
            &format!("vlite/prod/sets/{}/record.json", record.set_id),
            &serde_json::to_vec(&record).unwrap(),
        );
        record.set_id
    }

    #[test]
    fn reader_apply_config_validation() {
        use serde_json::json;
        let dir = tempfile::tempdir().unwrap();
        let follower = dir.path().join("followers");
        let served = follower.join("reader-a").join("database.vlite");
        let file = dir.path().join("reader.json");
        let valid = json!({
            "target_file":"never-read-secret",
            "follower_dir":follower,
            "capacity_bytes":1073741824u64,
            "staleness_bound_ms":3600000,
            "poll_interval_ms":1
        });
        std::fs::write(&file, valid.to_string()).unwrap();
        ReaderApplyConfig::load(&file, &served).unwrap();
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove("poll_interval_ms");
        let mut cases = vec![missing];
        for (key, value) in [
            ("unknown", json!("never-echo")),
            ("capacity_bytes", json!(0)),
            ("capacity_bytes", json!(1073741825u64)),
            ("staleness_bound_ms", json!(0)),
            ("staleness_bound_ms", json!(3600001)),
            ("poll_interval_ms", json!(0)),
            ("poll_interval_ms", json!(3600000)),
            ("follower_dir", json!("relative")),
            ("follower_dir", json!(follower.join("..").join("other"))),
            (
                "follower_dir",
                json!(format!(
                    "{}{s}.{s}other",
                    follower.display(),
                    s = std::path::MAIN_SEPARATOR
                )),
            ),
        ] {
            let mut value_config = valid.clone();
            value_config[key] = value;
            cases.push(value_config);
        }
        for value in cases {
            std::fs::write(&file, value.to_string()).unwrap();
            let error = ReaderApplyConfig::load(&file, &served).unwrap_err();
            assert!(error.starts_with("reader_apply_invalid:"), "{error}");
            assert!(!error.contains("never-"), "{error}");
        }
        std::fs::write(&file, valid.to_string()).unwrap();
        assert!(ReaderApplyConfig::load(&file, &dir.path().join("wrong")).is_err());
        std::fs::write(&file, vec![b' '; 65_537]).unwrap();
        assert!(
            ReaderApplyConfig::load(&file, &served)
                .unwrap_err()
                .contains("64 KiB")
        );
    }

    #[test]
    fn reader_apply_enroll_pins_newest_published_set() {
        let env = setup();
        extra_set(&env, false, SetState::Published);
        extra_set(&env, true, SetState::Staging);
        let untouched = config(&env).follower_dir.join("unrelated");
        std::fs::create_dir_all(&untouched).unwrap();
        std::fs::write(untouched.join("keep"), b"keep").unwrap();
        let apply = enroll(&env);
        let pins = record(&env).pins;
        assert_eq!(pins.len(), 1);
        assert_eq!(pins[0].reason, "reader-apply:replica");
        assert_eq!(pins[0].kind, PinKind::Operation);
        assert_eq!(
            std::fs::read(slot(&env, "reader-a")).unwrap(),
            std::fs::read(slot(&env, "reader-b")).unwrap()
        );
        assert_eq!(apply.progress().applied(), (4, 1_000));
        assert_eq!(apply.progress().bound(), 30_000);
        assert!(untouched.join("keep").exists());
    }

    #[test]
    fn reader_apply_enroll_follows_the_archive_and_replaces_only_its_slots() {
        let env = setup();
        checkpoint(&env, puts(5..=6), None);
        for name in ["reader-a", "reader-b"] {
            std::fs::create_dir_all(slot(&env, name).parent().unwrap()).unwrap();
            std::fs::write(slot(&env, name), b"previous enrollment").unwrap();
        }
        let apply = enroll(&env);
        assert_eq!(apply.progress().applied(), (6, 1_000));
        for name in ["reader-a", "reader-b"] {
            let database = Database::open_read_only(slot(&env, name)).unwrap();
            assert_eq!(database.get(6).unwrap().unwrap().payload, b"p6");
            database.close().unwrap();
        }
    }

    #[test]
    fn reader_apply_enroll_refuses_without_a_published_set() {
        let env = setup();
        let mut record = record(&env);
        record.state = SetState::Staging;
        env.meta.set_raw(
            &format!("vlite/prod/sets/{}/record.json", env.set_id),
            &serde_json::to_vec(&record).unwrap(),
        );
        let error = ReaderApply::enroll_with(env.sets.clone(), config(&env), "replica", 1_000)
            .err()
            .unwrap();
        assert_eq!(error.code, "reader_apply_no_set");
        assert_eq!(env.pins(), 0);
        assert!(!config(&env).follower_dir.exists());
    }

    #[test]
    fn reader_apply_enroll_releases_own_stale_pins() {
        let env = setup();
        let older = extra_set(&env, false, SetState::Published);
        for id in [&env.set_id, &older] {
            env.sets
                .pin(id, "reader-apply:replica", PinKind::Operation, 1)
                .unwrap();
            env.sets
                .pin(id, "reader-apply:other", PinKind::Operation, 1)
                .unwrap();
        }
        let _apply = enroll(&env);
        for set in env.sets.list().unwrap().sets {
            let record = set.record.unwrap();
            assert!(
                record
                    .pins
                    .iter()
                    .any(|pin| pin.reason == "reader-apply:other")
            );
            assert_eq!(
                record
                    .pins
                    .iter()
                    .filter(|pin| pin.reason == "reader-apply:replica")
                    .count(),
                usize::from(set.set_id == env.set_id)
            );
        }
    }

    #[test]
    fn reader_apply_enroll_refuses_over_capacity_before_download() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let env = setup();
        let reads = Arc::new(AtomicUsize::new(0));
        let count = reads.clone();
        env.data.hook(Op::Get, "/objects/", move || {
            count.fetch_add(1, Ordering::SeqCst);
        });
        let mut config = config(&env);
        config.capacity_bytes = 1;
        let error = ReaderApply::enroll_with(env.sets.clone(), config, "replica", 1)
            .err()
            .unwrap();
        assert_eq!(error.code, "reader_capacity_exceeded");
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        assert_eq!(env.pins(), 0);
        assert!(!slot(&env, "reader-a").parent().unwrap().exists());
        assert!(!slot(&env, "reader-b").parent().unwrap().exists());
    }

    #[test]
    fn reader_apply_enroll_refuses_insufficient_space() {
        let env = setup();
        cloud::FREE_BYTES.with(|free| free.set(Some(Some(0))));
        let result = ReaderApply::enroll_with(env.sets.clone(), config(&env), "replica", 1);
        cloud::FREE_BYTES.with(|free| free.set(None));
        assert_eq!(result.err().unwrap().code, "restore_insufficient_space");
        assert_eq!(env.pins(), 0);
        assert!(!slot(&env, "reader-a").parent().unwrap().exists());
    }

    #[test]
    fn reader_apply_enroll_failure_releases_pin_and_slots() {
        let env = setup();
        env.data.fault(Op::Get, "/objects/", Fault::Refuse, 0, 1);
        let error = ReaderApply::enroll_with(env.sets.clone(), config(&env), "replica", 1)
            .err()
            .unwrap();
        assert_eq!(error.code, "cloud_store_failed");
        assert_eq!(env.pins(), 0);
        for name in ["reader-a", "reader-b"] {
            assert!(!slot(&env, name).parent().unwrap().exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn reader_apply_enroll_refuses_symlinked_slot() {
        let env = setup();
        std::fs::create_dir_all(config(&env).follower_dir).unwrap();
        std::os::unix::fs::symlink(env.dir.path(), slot(&env, "reader-a").parent().unwrap())
            .unwrap();
        let error = ReaderApply::enroll_with(env.sets.clone(), config(&env), "replica", 1)
            .err()
            .unwrap();
        assert_eq!(error.code, "reader_apply_invalid");
        assert!(env.dir.path().join("source.vlite").exists());
        assert_eq!(env.pins(), 0);
    }

    #[test]
    fn reader_apply_step_applies_swaps_and_confirms() {
        let env = setup();
        let apply = enroll(&env);
        let service = reader(&slot(&env, "reader-a")).with_reader_apply(&apply);
        checkpoint(&env, puts(5..=6), None);
        assert_eq!(apply.step(&service, 2_000), Step::Swapped);
        assert_eq!(apply.progress().applied(), (6, 2_000));
        assert_eq!(
            service
                .database
                .read()
                .unwrap()
                .as_ref()
                .unwrap()
                .get(6)
                .unwrap()
                .unwrap()
                .payload,
            b"p6"
        );
        // The old served slot is now writable; the new slot is still served.
        Database::open(slot(&env, "reader-a"))
            .unwrap()
            .close()
            .unwrap();
        assert!(matches!(
            Database::open(slot(&env, "reader-b")),
            Err(crate::Error::Locked)
        ));
        assert_eq!(apply.step(&service, 3_000), Step::Confirmed);
        checkpoint(&env, puts(7..=8), None);
        assert_eq!(apply.step(&service, 4_000), Step::Swapped);
        assert_eq!(
            service
                .database
                .read()
                .unwrap()
                .as_ref()
                .unwrap()
                .get(8)
                .unwrap()
                .unwrap()
                .payload,
            b"p8"
        );
        Database::open(slot(&env, "reader-b"))
            .unwrap()
            .close()
            .unwrap();
    }

    #[test]
    fn reader_apply_step_current_confirms() {
        let env = setup();
        let apply = enroll(&env);
        let service = reader(&slot(&env, "reader-a")).with_reader_apply(&apply);
        assert_eq!(apply.step(&service, 2_000), Step::Confirmed);
        assert_eq!(apply.progress().applied(), (4, 2_000));
        assert_eq!(apply.progress().epoch(), None);
    }

    #[test]
    fn ha_reader_enrolls_at_head_swaps_and_retries_authority_outages() {
        use crate::server::cloud_recovery::tests::{checkpoint_ha, setup_ha};
        let (env, authority, claim) = setup_ha();
        checkpoint_ha(&env, &authority, &claim, 5..=6);
        checkpoint_ha(&env, &authority, &claim, 7..=8);
        let apply = enroll(&env);
        assert_eq!(apply.progress().applied(), (8, 1_000));
        assert_eq!(apply.progress().epoch(), Some(claim.epoch));
        let service = reader(&slot(&env, "reader-a")).with_reader_apply(&apply);
        checkpoint_ha(&env, &authority, &claim, 9..=10);
        env.meta
            .fault(Op::Get, "authority.json", Fault::Before, 0, 1);
        assert_eq!(apply.step(&service, 2_000), Step::Retry);
        assert_eq!(apply.progress().applied(), (8, 1_000));
        assert_eq!(apply.step(&service, 3_000), Step::Swapped);
        assert_eq!(apply.progress().applied(), (10, 3_000));
        assert_eq!(apply.progress().epoch(), Some(claim.epoch));
        assert_eq!(apply.step(&service, 4_000), Step::Confirmed);
        env.meta.remove("vlite/prod/authority.json");
        assert_eq!(apply.step(&service, 5_000), Step::Failed);
        assert_eq!(
            apply.progress().failure().as_deref(),
            Some("writer_authority_missing")
        );
    }

    #[test]
    fn ha_reader_frozen_base_is_not_a_freshness_confirmation() {
        use crate::server::cloud_recovery::tests::{checkpoint_ha, setup_ha};
        let (env, authority, claim) = setup_ha();
        checkpoint_ha(&env, &authority, &claim, 5..=6);
        let current = authority.read().unwrap();
        let recovering = authority
            .prepare_transfer(&current, current.record.epoch, &current.record.revision)
            .unwrap();
        let apply = enroll(&env);
        assert_eq!(apply.progress().applied(), (6, 1_000));
        let service = reader(&slot(&env, "reader-a")).with_reader_apply(&apply);
        assert_eq!(apply.step(&service, 2_000), Step::Confirmed);
        assert_eq!(apply.progress().applied(), (6, 1_000));
        assert_eq!(apply.progress().age_ms(40_000), Some(39_000));
        // The frozen base keeps the old epoch, below the recovering record's.
        assert_eq!(apply.progress().epoch(), Some(claim.epoch));
        assert!(recovering.record.epoch > claim.epoch);
    }

    #[test]
    fn ha_reader_reseeds_for_replaced_or_regressed_head() {
        use crate::server::cloud_recovery::tests::{checkpoint_ha, setup_ha};
        for replaced in [false, true] {
            let (env, authority, claim) = setup_ha();
            let baseline = authority.read().unwrap();
            checkpoint_ha(&env, &authority, &claim, 5..=6);
            let apply = enroll(&env);
            let service = reader(&slot(&env, "reader-a")).with_reader_apply(&apply);
            let mut current = baseline.record;
            if replaced {
                current.head.as_mut().unwrap().set_id = cloud::new_set_id(1).unwrap();
            }
            env.meta.set_raw(
                "vlite/prod/authority.json",
                &serde_json::to_vec(&current).unwrap(),
            );
            assert_eq!(apply.step(&service, 2_000), Step::Failed);
            assert_eq!(
                apply.progress().failure().as_deref(),
                Some(if replaced {
                    "a newer published set needs a re-seed"
                } else {
                    "the archive head moved backwards"
                }),
            );
        }
    }

    #[test]
    fn authority_without_head_refuses_legacy_fallback() {
        use crate::server::authority::AuthorityStore;
        let env = setup();
        let archive = env.sets.open_archive(&env.set_id).unwrap();
        let (store, prefix) = env.sets.coordination();
        AuthorityStore::new(store, prefix)
            .create(
                &archive.manifest.installation_id,
                "scope",
                archive.manifest.source_lineage.as_ref().unwrap(),
            )
            .unwrap();
        let error = ReaderApply::enroll_with(env.sets.clone(), config(&env), "replica", 1)
            .err()
            .unwrap();
        assert_eq!(error.code, "reader_apply_no_set");
    }

    #[test]
    fn ha_reader_refuses_non_ha_or_mismatched_manifest() {
        use crate::server::cloud_recovery::tests::setup_ha;
        for legacy in [false, true] {
            let (env, authority, _) = setup_ha();
            let mut observed = authority.read().unwrap();
            if legacy {
                let key = format!("vlite/prod/sets/{}/manifest.json", env.set_id);
                let envelope: serde_json::Value =
                    serde_json::from_slice(&env.data.raw(&key).unwrap()).unwrap();
                let bytes = serde_json::to_vec(&envelope["manifest"]).unwrap();
                let digest = cloud::sha_hex(&bytes);
                env.data.set_raw(&key, &bytes);
                let mut record = record(&env);
                record.manifest_sha256 = digest.clone();
                env.meta.set_raw(
                    &format!("vlite/prod/sets/{}/record.json", env.set_id),
                    &serde_json::to_vec(&record).unwrap(),
                );
                observed.record.head.as_mut().unwrap().manifest_sha256 = digest;
            } else {
                observed.record.head.as_mut().unwrap().manifest_sha256 = "f".repeat(64);
            }
            env.meta.set_raw(
                "vlite/prod/authority.json",
                &serde_json::to_vec(&observed.record).unwrap(),
            );
            let error = ReaderApply::enroll_with(env.sets.clone(), config(&env), "replica", 1)
                .err()
                .unwrap();
            assert_eq!(error.code, "reader_apply_reseed_required");
            assert_eq!(env.pins(), 0);
        }
    }

    #[test]
    fn reader_apply_step_newer_set_fails_and_releases_pin() {
        let env = setup();
        let apply = enroll(&env);
        let service = reader(&slot(&env, "reader-a")).with_reader_apply(&apply);
        extra_set(&env, true, SetState::Published);
        assert_eq!(apply.step(&service, 2_000), Step::Failed);
        assert_eq!(
            apply.progress().failure().as_deref(),
            Some("a newer published set needs a re-seed")
        );
        assert_eq!(env.pins(), 0);
    }

    #[test]
    fn reader_apply_step_unpublished_set_fails_and_releases_pin() {
        let env = setup();
        let apply = enroll(&env);
        let service = reader(&slot(&env, "reader-a")).with_reader_apply(&apply);
        let mut record = record(&env);
        record.state = SetState::Staging;
        env.meta.set_raw(
            &format!("vlite/prod/sets/{}/record.json", env.set_id),
            &serde_json::to_vec(&record).unwrap(),
        );
        assert_eq!(apply.step(&service, 2_000), Step::Failed);
        assert_eq!(env.pins(), 0);
    }

    #[test]
    fn reader_apply_step_capacity_failure_keeps_the_served_generation() {
        let env = setup();
        let mut config = config(&env);
        config.capacity_bytes = std::fs::metadata(env.dir.path().join("source.vlite"))
            .unwrap()
            .len();
        let apply = ReaderApply::enroll_with(env.sets.clone(), config, "replica", 1_000)
            .unwrap_or_else(|e| panic!("{}: {}", e.code, e.message));
        let service = reader(&slot(&env, "reader-a")).with_reader_apply(&apply);
        checkpoint(&env, puts(5..=6), None);
        assert_eq!(apply.step(&service, 2_000), Step::Failed);
        assert_eq!(
            apply.progress().failure().as_deref(),
            Some("reader_capacity_exceeded")
        );
        assert_eq!(apply.progress().applied(), (4, 1_000));
        assert!(
            service
                .database
                .read()
                .unwrap()
                .as_ref()
                .unwrap()
                .get(6)
                .unwrap()
                .is_none()
        );
        assert_eq!(env.pins(), 0);
    }

    #[test]
    fn reader_apply_step_permanent_store_error_fails_and_releases_pin() {
        let env = setup();
        let apply = enroll(&env);
        let service = reader(&slot(&env, "reader-a")).with_reader_apply(&apply);
        env.meta.fault(Op::Get, "record.json", Fault::Refuse, 0, 1);
        assert_eq!(apply.step(&service, 2_000), Step::Failed);
        assert_eq!(
            apply.progress().failure().as_deref(),
            Some("cloud_store_failed")
        );
        assert_eq!(env.pins(), 0);
    }

    #[test]
    fn reader_apply_step_reseed_fails_and_releases_pin() {
        let env = setup();
        let apply = enroll(&env);
        let service = reader(&slot(&env, "reader-a")).with_reader_apply(&apply);
        let mut standby = Database::open(slot(&env, "reader-b")).unwrap();
        standby.put(5, &[0.0, 0.0, 1.0], b"not archived").unwrap();
        standby.close().unwrap();
        assert_eq!(apply.step(&service, 2_000), Step::Failed);
        assert_eq!(env.pins(), 0);
    }

    #[test]
    fn reader_apply_step_transient_store_error_retries() {
        let env = setup();
        let apply = enroll(&env);
        let service = reader(&slot(&env, "reader-a")).with_reader_apply(&apply);
        env.meta.fault(Op::Get, "record.json", Fault::Before, 0, 1);
        assert_eq!(apply.step(&service, 2_000), Step::Retry);
        assert_eq!(apply.progress().applied(), (4, 1_000));
        assert!(apply.progress().failure().is_none());
        checkpoint(&env, puts(5..=6), None);
        cloud::FREE_BYTES.with(|free| free.set(Some(Some(0))));
        let step = apply.step(&service, 3_000);
        cloud::FREE_BYTES.with(|free| free.set(None));
        assert_eq!(step, Step::Retry);
        assert_eq!(apply.progress().applied(), (4, 1_000));
        assert_eq!(env.pins(), 1);
    }

    #[test]
    fn reader_apply_step_after_failure_makes_no_store_call() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let env = setup();
        let apply = enroll(&env);
        let service = reader(&slot(&env, "reader-a")).with_reader_apply(&apply);
        apply.progress().fail("stop".into());
        let called = Arc::new(AtomicBool::new(false));
        let count = called.clone();
        env.meta.hook(Op::List, "", move || {
            count.store(true, Ordering::SeqCst);
        });
        assert_eq!(apply.step(&service, 2_000), Step::Stopped);
        assert!(!called.load(Ordering::SeqCst));
    }

    #[test]
    fn reader_apply_stop_releases_pin() {
        let env = setup();
        let mut apply = enroll(&env);
        let service = reader(&slot(&env, "reader-a")).with_reader_apply(&apply);
        apply.start(service);
        apply.stop();
        assert_eq!(env.pins(), 0);
    }

    #[tokio::test]
    async fn reader_apply_failed_reader_is_not_ready() {
        let env = setup();
        let apply = enroll(&env);
        let service = reader(&slot(&env, "reader-a")).with_reader_apply(&apply);
        extra_set(&env, true, SetState::Published);
        assert_eq!(apply.step(&service, 2_000), Step::Failed);
        let response = router(service)
            .oneshot(
                Request::builder()
                    .uri("/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("reader_apply_failed"));
    }

    fn file(dir: &Path, name: &str, ids: std::ops::RangeInclusive<u64>) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut db = Database::create(&path, 1).unwrap();
        for id in ids {
            db.put(id, &[id as f32], b"").unwrap();
        }
        db.close().unwrap();
        path
    }

    fn advance(path: &Path, ids: std::ops::RangeInclusive<u64>) {
        let mut db = Database::open(path).unwrap();
        for id in ids {
            db.put(id, &[id as f32], b"").unwrap();
        }
        db.close().unwrap();
    }

    fn reader(path: &Path) -> Service {
        Service::new(Database::open_read_only(path).unwrap())
            .with_serve_mode(ServeMode::Reader)
            .unwrap()
    }

    async fn status(service: &Service, uri: &str) -> StatusCode {
        router(service.clone())
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn a_swapped_generation_serves_the_advanced_copy_and_frees_the_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let served = file(dir.path(), "a.vlite", 1..=2);
        let advanced = dir.path().join("b.vlite");
        std::fs::copy(&served, &advanced).unwrap();
        advance(&advanced, 3..=4);
        let service = reader(&served);
        assert_eq!(
            status(&service, "/v1/vectors/4").await,
            StatusCode::NOT_FOUND
        );

        service
            .swap_reader_generation(Database::open_read_only(&advanced).unwrap())
            .unwrap();

        assert_eq!(status(&service, "/v1/vectors/4").await, StatusCode::OK);
        assert_eq!(status(&service, "/ready").await, StatusCode::OK);
        // The retired generation released its shared lock: the file can be
        // advanced again while the new one is served.
        advance(&served, 3..=5);
        assert!(matches!(
            Database::open(&advanced),
            Err(crate::Error::Locked)
        ));
    }

    #[tokio::test]
    async fn a_swap_is_refused_for_another_family_or_a_writable_side() {
        let dir = tempfile::tempdir().unwrap();
        let served = file(dir.path(), "a.vlite", 1..=2);
        let other = file(dir.path(), "other.vlite", 1..=4);
        let service = reader(&served);

        let error = service
            .swap_reader_generation(Database::open_read_only(&other).unwrap())
            .unwrap_err();
        assert!(error.to_string().contains("another lineage"), "{error}");
        // The refused handle was closed and the served one kept.
        Database::open(&other).unwrap().close().unwrap();
        assert_eq!(status(&service, "/v1/vectors/2").await, StatusCode::OK);
        assert_eq!(
            status(&service, "/v1/vectors/4").await,
            StatusCode::NOT_FOUND
        );

        let copy = dir.path().join("b.vlite");
        std::fs::copy(&served, &copy).unwrap();
        let error = service
            .swap_reader_generation(Database::open(&copy).unwrap())
            .unwrap_err();
        assert!(error.to_string().contains("not read-only"), "{error}");
        Database::open(&copy).unwrap().close().unwrap();

        let writable = file(dir.path(), "writer.vlite", 1..=2);
        let twin = dir.path().join("twin.vlite");
        std::fs::copy(&writable, &twin).unwrap();
        let service = Service::new(Database::open(&writable).unwrap());
        let error = service
            .swap_reader_generation(Database::open_read_only(&twin).unwrap())
            .unwrap_err();
        assert!(error.to_string().contains("writable"), "{error}");
        Database::open(&twin).unwrap().close().unwrap();
    }

    #[tokio::test]
    async fn a_failed_reader_is_not_ready_and_keeps_its_first_reason() {
        let dir = tempfile::tempdir().unwrap();
        let served = file(dir.path(), "a.vlite", 1..=2);
        let progress = Arc::new(Progress::default());
        let mut service = reader(&served);
        service.reader = Some(progress.clone());
        assert_eq!(status(&service, "/ready").await, StatusCode::OK);

        progress.fail("a newer published set".into());
        progress.fail("later".into());

        assert_eq!(
            status(&service, "/ready").await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(progress.failure().as_deref(), Some("a newer published set"));
        // Reads keep serving the last verified generation.
        assert_eq!(status(&service, "/v1/vectors/2").await, StatusCode::OK);
    }

    #[test]
    fn progress_reports_the_confirmed_position() {
        let progress = Progress::default();
        progress.confirm(7, 1_000);
        assert_eq!(progress.applied(), (7, 1_000));
    }

    #[test]
    fn age_is_unknown_until_confirmed_and_never_negative() {
        let progress = Progress::default();
        assert_eq!(progress.bound(), 0);
        assert_eq!(progress.age_ms(5_000), None);
        progress.set_bound(30_000);
        progress.confirm(3, 1_000);
        assert_eq!(progress.bound(), 30_000);
        assert_eq!(progress.age_ms(5_000), Some(4_000));
        assert_eq!(progress.age_ms(500), Some(0));
    }
}
