//! Explicit writer enrollment and the serving process's periodic authority check.

use std::{ffi::OsString, path::Path, path::PathBuf, time::Duration};

use serde_json::{Value, json};
use tokio::sync::watch;

use super::{
    ApiError, Service,
    authority::{AuthorityError, AuthorityStore, Observed, Phase, Record},
    cloud_backup::{PinKind, Sets},
    cloud_recovery::{Staging, rename_no_replace, sync_directory},
    gateway_mode::{flags, utf8},
    tenant_provisioning::guard::Guards,
    writer_fence::{State, WriterFence},
};

const AUTHORITY_USAGE: &str = "vlite-server authority init|show --target-file <json> [--shared-key-file <path>] [init only, all required: --installation-id <id> --scope-id <id> --lineage <16 lowercase hex>]";

fn describe(error: AuthorityError) -> String {
    let error = ApiError::from(error);
    format!("{}: {}", error.code, error.message)
}

fn summary(record: &Record) -> Value {
    json!({
        "schema": record.schema,
        "installation_id": record.installation_id,
        "scope_id": record.scope_id,
        "lineage": record.lineage,
        "epoch": record.epoch,
        "revision": record.revision,
        "phase": record.phase,
        "writer_present": record.writer_id.is_some(),
        "base": record.base,
        "head": record.head,
    })
}

/// Blocking operator command; call off the async runtime.
pub fn authority_command(args: &[OsString]) -> Result<Value, String> {
    let verb = match args.first().and_then(|arg| arg.to_str()) {
        Some("init") => "init",
        Some("show") => "show",
        _ => "invalid",
    };
    let result = (|| {
        let usage = |detail: &str| format!("invalid_arguments: {detail}; usage: {AUTHORITY_USAGE}");
        if verb == "invalid" {
            return Err(usage("the verb must be init or show"));
        }
        let mut allowed = vec!["--target-file", "--shared-key-file"];
        if verb == "init" {
            allowed.extend(["--installation-id", "--scope-id", "--lineage"]);
        }
        let mut values = flags(&args[1..], &allowed, AUTHORITY_USAGE)?;
        let mut required = |name: &str| {
            values
                .remove(name)
                .ok_or_else(|| usage(&format!("{name} is required")))
        };
        let target = PathBuf::from(required("--target-file")?);
        let init = if verb == "init" {
            Some((
                utf8("--installation-id", required("--installation-id")?)?,
                utf8("--scope-id", required("--scope-id")?)?,
                utf8("--lineage", required("--lineage")?)?,
            ))
        } else {
            None
        };
        let key = values.remove("--shared-key-file").map(PathBuf::from);
        let authority = WriterAuthority::connect(&target, key.as_deref())?;
        let observed = match init {
            Some((installation, scope, lineage)) => {
                authority.store.create(&installation, &scope, &lineage)
            }
            None => authority.store.read(),
        }
        .map_err(describe)?;
        Ok(summary(&observed.record))
    })();
    let outcome = match &result {
        Ok(_) => "ok",
        Err(error) => error.split(':').next().unwrap_or("internal_error"),
    };
    super::log_stderr(
        if result.is_ok() { "info" } else { "warn" },
        "authority",
        json!({"event": "authority_command", "verb": verb, "outcome": outcome}).to_string(),
    );
    result
}

/// Unclaimed authority connection used only by the server binary.
#[doc(hidden)]
pub struct WriterAuthority {
    store: AuthorityStore,
    target: super::cloud_backup::Target,
    sets: Sets,
    prepared: Option<Prepared>,
}

struct Prepared {
    observed: Observed,
    restored: Option<(u64, u64)>,
}

fn base_refusal(code: &'static str, message: impl Into<String>) -> String {
    let error = ApiError::new(axum::http::StatusCode::CONFLICT, code, message);
    format!("{}: {}", error.code, error.message)
}

fn mismatch(message: impl Into<String>) -> String {
    base_refusal("writer_authority_base_mismatch", message)
}

fn restore_error(error: ApiError) -> String {
    match error.code {
        "cloud_set_corrupt"
        | "cloud_set_not_found"
        | "cloud_set_unpublished"
        | "writer_authority_invalid"
        | "pitr_archive_missing" => mismatch(error.message),
        _ => format!("{}: {}", error.code, error.message),
    }
}

impl WriterAuthority {
    #[doc(hidden)]
    pub fn validate_archive_policy(&self, path: &Path) -> Result<(), String> {
        let policy = super::backup_policy::read_writer_policy(path).map_err(|e| e.to_string())?;
        super::backup_policy::check_writer_target(&policy.target, &self.target)
            .map_err(|error| format!("{}: {}", error.code, error.message))
    }

    #[doc(hidden)]
    pub fn connect(target: &Path, key: Option<&Path>) -> Result<Self, String> {
        let invalid =
            || "invalid_target: the authority target could not be read consistently".to_owned();
        let bytes = super::backup_policy::read_limited(target)
            .map_err(|_| invalid())?
            .ok_or_else(invalid)?;
        let sets = super::cloud_recovery::connect_explicit(target, key)
            .map_err(|error| format!("{}: {}", error.code, error.message))?;
        if super::backup_policy::read_limited(target)
            .map_err(|_| invalid())?
            .as_ref()
            != Some(&bytes)
        {
            return Err(invalid());
        }
        let target = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        let (store, prefix) = sets.coordination();
        Ok(Self {
            store: AuthorityStore::new(store, prefix),
            target,
            sets,
            prepared: None,
        })
    }

    /// Rejects configurations for which the plaintext, persistent restore is unsafe.
    #[doc(hidden)]
    pub fn check_database_configuration(
        persistent: bool,
        encrypted: bool,
        selected: bool,
        recovery: bool,
    ) -> Result<(), String> {
        if !persistent || encrypted || selected || recovery {
            return Err(base_refusal(
                "writer_authority_base_unverified",
                "archive recovery requires a persistent, unencrypted database without a managed selection or recovery mode",
            ));
        }
        Ok(())
    }

    /// Blocking startup recovery, before opening the database or claiming authority.
    #[doc(hidden)]
    pub fn prepare_database(&mut self, database: &Path) -> Result<Option<Value>, String> {
        if self.prepared.is_some() {
            return Err(base_refusal(
                "writer_authority_conflict",
                "writer startup was already prepared; it cannot be retried",
            ));
        }
        let observed = self.store.read().map_err(|error| match error {
            AuthorityError::Invalid => mismatch("the archived base authority record is invalid"),
            error => describe(error),
        })?;
        self.prepared = Some(Prepared {
            observed: observed.clone(),
            restored: None,
        });
        let record = &observed.record;
        let Some(base) = record.base.as_ref().filter(|_| record.writer_id.is_none()) else {
            return Ok(None);
        };
        if record.phase != Phase::Recovering || base.epoch >= record.epoch {
            return Err(mismatch("the base must precede the recovering epoch"));
        }
        let parent = database
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut name = database.as_os_str().to_os_string();
        name.push(format!(".superseded-epoch{}", base.epoch));
        let superseded = PathBuf::from(name);
        let io_error = |error: std::io::Error| {
            format!("writer_authority_unavailable: cannot install the archived base: {error}")
        };
        let exists = |path: &Path| match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(true),
            Ok(_) => Err(base_refusal(
                "writer_authority_base_unverified",
                "the restore destination must be a regular file",
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(io_error(error)),
        };
        if exists(&superseded)? {
            return Err(base_refusal(
                "writer_authority_base_unverified",
                "the superseded database already exists; preserve or remove it before restarting",
            ));
        }
        let had_database = exists(database)?;
        let staging = Staging::create(parent).map_err(io_error)?;
        let download = staging.path().join("download");
        super::cloud_backup::create_private_subdir(&download).map_err(io_error)?;
        let pin = self
            .sets
            .pin(
                &base.set_id,
                "writer startup base restore",
                PinKind::Operation,
                super::admin::now_ms() as u64,
            )
            .map_err(restore_error)?;
        let verified = (|| {
            let fetched = self
                .sets
                .fetch(&base.set_id, &pin.id, &download)
                .map_err(restore_error)?;
            let identity = fetched
                .ha
                .as_ref()
                .ok_or_else(|| mismatch("the base has no HA manifest"))?;
            identity
                .check_scope(&fetched.manifest.installation_id, record)
                .map_err(restore_error)?;
            if fetched.record.set_id != base.set_id
                || fetched.record.manifest_sha256 != base.manifest_sha256
                || fetched.manifest.source_lineage.as_deref() != Some(record.lineage.as_str())
                || identity.epoch != base.epoch
            {
                return Err(mismatch(
                    "the base set, manifest, epoch or lineage differs from the authority",
                ));
            }
            self.sets
                .restore_ha_head(&fetched, &download, base)
                .map_err(restore_error)?;
            let detail =
                crate::maintenance::inspect_detail(&fetched.database).map_err(|error| {
                    mismatch(format!("the restored database failed inspection: {error}"))
                })?;
            if detail.inspection.commits != base.sequence
                || detail
                    .lineage
                    .map(|lineage| format!("{lineage:016x}"))
                    .as_deref()
                    != Some(record.lineage.as_str())
            {
                return Err(mismatch(
                    "the restored sequence or lineage differs from the base",
                ));
            }
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&fetched.database)
                .and_then(|file| file.sync_all())
                .map_err(io_error)?;
            Ok((fetched.database, detail.lineage.expect("verified lineage")))
        })();
        let released = self
            .sets
            .release(&base.set_id, &pin.id, super::admin::now_ms() as u64);
        let (staged, lineage) = match (verified, released) {
            (Ok(verified), Ok(())) => verified,
            (Err(error), Ok(())) => return Err(error),
            (result, Err(error)) => {
                return Err(format!(
                    "{}; operation pin {} on set {} could not be released: {}",
                    result.err().unwrap_or_else(|| {
                        "writer_authority_unavailable: restore cleanup failed".into()
                    }),
                    pin.id,
                    base.set_id,
                    error.message
                ));
            }
        };
        // The engine has no WAL or lock sidecar. Drop the disposable ANN cache
        // so no cache from the superseded database can be loaded after restore.
        match std::fs::remove_file(crate::Database::ann_snapshot_path(database)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(error)),
        }
        if had_database {
            rename_no_replace(database, &superseded).map_err(io_error)?;
        }
        if let Err(error) = rename_no_replace(&staged, database) {
            if had_database {
                rename_no_replace(&superseded, database).map_err(|rollback| format!(
                    "writer_authority_unavailable: base installation failed ({error}); rollback failed ({rollback}); the original database is at {}",
                    superseded.display()
                ))?;
            }
            return Err(io_error(error));
        }
        sync_directory(parent).map_err(io_error)?;
        self.prepared
            .as_mut()
            .expect("prepared observation")
            .restored = Some((base.sequence, lineage));
        Ok(Some(json!({
            "set_id": base.set_id, "epoch": base.epoch, "sequence": base.sequence,
            "lineage": record.lineage,
            "superseded": had_database.then(|| superseded.display().to_string()),
        })))
    }
}

#[cfg(test)]
pub(super) fn claim(store: AuthorityStore, lineage: Option<u64>) -> Result<WriterFence, String> {
    claim_prepared(store, lineage, None, None)
}

fn claim_prepared(
    store: AuthorityStore,
    lineage: Option<u64>,
    sequence: Option<u64>,
    prepared: Option<Prepared>,
) -> Result<WriterFence, String> {
    let (observed, restored) = match prepared {
        Some(prepared) => (prepared.observed, prepared.restored),
        None => (store.read().map_err(describe)?, None),
    };
    if lineage.map(|lineage| format!("{lineage:016x}")).as_deref()
        != Some(observed.record.lineage.as_str())
    {
        if restored.is_some() {
            return Err(mismatch(
                "the opened database lineage differs from the restored base",
            ));
        }
        return Err(
            "writer_authority_lineage_mismatch: authority and database must have the same v4 lineage"
                .into(),
        );
    }
    if observed.record.writer_id.is_some() {
        return Err(
            "writer_authority_claimed: explicit failover is required before another writer starts"
                .into(),
        );
    }
    if let Some(base) = &observed.record.base {
        match restored {
            None => {
                return Err(base_refusal(
                    "writer_authority_base_unverified",
                    "startup has not restored the archived base",
                ));
            }
            Some((restored_sequence, restored_lineage))
                if sequence == Some(base.sequence)
                    && sequence == Some(restored_sequence)
                    && lineage == Some(restored_lineage) => {}
            Some(_) => {
                return Err(mismatch(
                    "the opened database differs from the restored base",
                ));
            }
        }
    }
    // design-tenant-provisioning ITEM-007: a target a managed lifecycle
    // operation holds stays unwritable until that operation enables writes.
    // Checked at the claim only: a writer already serving is not stopped.
    Guards::of(&store)
        .check_write_enable()
        .map_err(|refusal| format!("{}: {}", refusal.code(), refusal.message()))?;
    let (claim, _) = store
        .claim(&observed, super::admin::now_ms() as u64)
        .map_err(describe)?;
    let epoch = claim.epoch;
    let writer_prefix = claim.writer_id[..8].to_owned();
    let fence = WriterFence::new(store, claim);
    for state in [State::Claiming, State::Recovering, State::Serving] {
        fence
            .transition(state)
            .map_err(|_| "internal_error: invalid writer startup transition".to_owned())?;
    }
    super::log_stderr(
        "info",
        "authority",
        json!({"event":"writer_authority_claimed", "epoch":epoch, "writer_id_prefix":writer_prefix})
            .to_string(),
    );
    Ok(fence)
}

impl Service {
    #[doc(hidden)]
    pub async fn initialize_archive_base(&self) {
        let ops = self.ops.clone();
        if tokio::task::spawn_blocking(move || ops.initial_archive_base())
            .await
            .is_err()
        {
            super::logs::stderr(
                "error",
                "cloud_backup",
                json!({"event":"writer_archive_initial_base_failed","code":"internal_error"})
                    .to_string(),
            );
        }
    }

    /// Blocking startup enrollment, immediately after choosing writer serve mode.
    #[doc(hidden)]
    pub fn with_writer_authority(self, authority: WriterAuthority) -> Result<Self, String> {
        if self.serve.mode != super::ServeMode::Writer {
            return Err("invalid_arguments: writer authority needs --serve-mode writer".into());
        }
        let sequence = self
            .database
            .try_read()
            .map_err(|_| "internal_error: cannot inspect the startup database".to_owned())?
            .as_ref()
            .map(|database| database.sequence);
        let fence = claim_prepared(authority.store, self.lineage, sequence, authority.prepared)?
            .with_target(authority.target);
        self.with_writer_fence(fence)
            .map_err(|_| "internal_error: cannot install writer authority".into())
    }

    #[doc(hidden)]
    pub async fn recheck_writer_authority(
        &self,
        interval: Duration,
        stop: watch::Receiver<bool>,
    ) -> Result<(), String> {
        recheck(self.writer_fence.clone(), interval, stop).await
    }
}

#[cfg(test)]
pub(super) async fn recheck_for_measurement(
    fence: WriterFence,
    interval: Duration,
    stop: watch::Receiver<bool>,
) -> Result<(), String> {
    recheck(fence, interval, stop).await
}

async fn recheck(
    fence: WriterFence,
    interval: Duration,
    mut stop: watch::Receiver<bool>,
) -> Result<(), String> {
    loop {
        if !matches!(fence.state(), State::Serving | State::AuthorityUnavailable) || *stop.borrow()
        {
            return Ok(());
        }
        tokio::select! {
            () = tokio::time::sleep(interval) => {}
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    return Ok(());
                }
                continue;
            }
        }
        let fence = fence.clone();
        tokio::task::spawn_blocking(move || {
            if matches!(fence.state(), State::Serving | State::AuthorityUnavailable) {
                let before = fence.state();
                if let Err(error) = fence.verify_before_commit()
                    && before != State::AuthorityUnavailable
                    && fence.state() == State::AuthorityUnavailable
                {
                    super::log_stderr(
                        "warn",
                        "authority",
                        json!({"event":"writer_authority_recheck", "code":error.code}).to_string(),
                    );
                }
            }
        })
        .await
        .map_err(|_| "internal_error: writer authority recheck stopped unexpectedly".to_owned())?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{
        authority::Record,
        cloud_backup::fake::{Fault, Memory, Op},
        cloud_recovery::tests::{Env, checkpoint_ha, setup_ha},
    };
    use std::sync::Arc;

    const KEY: &str = "test/authority.json";
    const LINEAGE: u64 = 0x0123456789abcdef;

    fn setup() -> (Arc<Memory>, AuthorityStore) {
        let memory = Arc::new(Memory::default());
        let store = AuthorityStore::new(memory.clone(), "test/".into());
        store
            .create("installation", "scope", "0123456789abcdef")
            .unwrap();
        (memory, store)
    }

    fn refusal(store: AuthorityStore, lineage: Option<u64>, code: &str) {
        let error = claim(store, lineage).err().expect("startup must refuse");
        assert!(error.starts_with(&format!("{code}: ")), "{error}");
    }

    #[test]
    fn fresh_authority_is_claimed_once_and_output_redacts_writer() {
        let (memory, store) = setup();
        let fence = claim(store, Some(LINEAGE)).unwrap();
        assert_eq!(fence.state(), State::Serving);
        assert_eq!(fence.epoch(), Some(1));
        let record: Record = serde_json::from_slice(&memory.raw(KEY).unwrap()).unwrap();
        let output = summary(&record);
        assert_eq!(output["writer_present"], true);
        assert!(
            !output
                .to_string()
                .contains(record.writer_id.as_ref().unwrap())
        );
        assert!(record.claimed_unix_ms.is_some());
        refusal(
            AuthorityStore::new(memory, "test/".into()),
            Some(LINEAGE),
            "writer_authority_claimed",
        );
    }

    #[test]
    fn startup_refuses_missing_unavailable_invalid_and_wrong_lineage() {
        let (memory, store) = setup();
        memory.remove(KEY);
        refusal(store, Some(LINEAGE), "writer_authority_missing");
        let (memory, store) = setup();
        memory.fault(Op::Get, KEY, Fault::Refuse, 0, 1);
        refusal(store, Some(LINEAGE), "writer_authority_unavailable");
        let (memory, store) = setup();
        memory.set_raw(KEY, b"not-json");
        refusal(store, Some(LINEAGE), "writer_authority_invalid");
        for lineage in [None, Some(LINEAGE + 1)] {
            let (_, store) = setup();
            refusal(store, lineage, "writer_authority_lineage_mismatch");
        }
    }

    #[test]
    fn startup_refuses_a_claim_until_the_lifecycle_guard_enables_writes() {
        use crate::server::tenant_provisioning::guard::{
            Guards,
            tests::{OPERATION, TENANT, held},
        };
        let (memory, store) = setup();
        held(&memory, "test/", false);
        let before = memory.raw(KEY);
        refusal(store, Some(LINEAGE), "lifecycle_write_disabled");
        assert_eq!(memory.raw(KEY), before, "a refused claim must not write");
        let (memory, store) = setup();
        memory.fault(Op::Get, "lifecycle-guard", Fault::Refuse, 0, 1);
        refusal(store, Some(LINEAGE), "lifecycle_guard_unavailable");
        let (memory, store) = setup();
        memory.set_raw("test/lifecycle-guard.json", b"{}");
        refusal(store, Some(LINEAGE), "lifecycle_guard_invalid");
        // Write-enabled and released guards both let the writer claim.
        let (memory, store) = setup();
        held(&memory, "test/", true);
        assert_eq!(claim(store, Some(LINEAGE)).unwrap().epoch(), Some(1));
        let (memory, store) = setup();
        held(&memory, "test/", false);
        Guards::new(memory.clone(), "test/")
            .release(TENANT, OPERATION)
            .unwrap();
        assert_eq!(claim(store, Some(LINEAGE)).unwrap().epoch(), Some(1));
    }

    fn prepared(env: &Env, store: &AuthorityStore) -> WriterAuthority {
        let observed = store.read().unwrap();
        store
            .prepare_transfer(&observed, 1, &observed.record.revision)
            .unwrap();
        let (objects, prefix) = env.sets.coordination();
        WriterAuthority {
            store: AuthorityStore::new(objects, prefix),
            sets: env.sets.clone(),
            target: serde_json::from_slice(
                &std::fs::read(env.dir.path().join("target.json")).unwrap(),
            )
            .unwrap(),
            prepared: None,
        }
    }

    fn enroll(database: &Path, authority: WriterAuthority) -> Result<Service, String> {
        Service::try_new(crate::Database::open(database).unwrap())
            .unwrap()
            .with_serve_mode(super::super::ServeMode::Writer)
            .unwrap()
            .with_writer_authority(authority)
    }

    #[test]
    fn startup_restores_exact_base_and_preserves_superseded_bytes() {
        for archived in [false, true] {
            for existing in [false, true] {
                let (env, store, owner) = setup_ha();
                if archived {
                    checkpoint_ha(&env, &store, &owner, 5..=6);
                }
                let mut authority = prepared(&env, &store);
                let base = store.read().unwrap().record.base.unwrap();
                let database = env.dir.path().join("writer.vlite");
                let original = if existing {
                    std::fs::copy(env.dir.path().join("source.vlite"), &database).unwrap();
                    let mut db = crate::Database::open(&database).unwrap();
                    db.put(99, &[1.0, 2.0, 3.0], b"unarchived").unwrap();
                    db.close().unwrap();
                    Some(std::fs::read(&database).unwrap())
                } else {
                    None
                };
                let report = authority.prepare_database(&database).unwrap().unwrap();
                assert_eq!(report["sequence"], base.sequence);
                assert_eq!(report["lineage"], store.read().unwrap().record.lineage);
                let db = crate::Database::open_read_only(&database).unwrap();
                assert_eq!(db.feed_cursor().unwrap().sequence, base.sequence);
                assert!(db.get(99).unwrap().is_none());
                if archived {
                    assert!(db.get(6).unwrap().is_some());
                }
                db.close().unwrap();
                if let Some(original) = original {
                    assert_eq!(
                        std::fs::read(report["superseded"].as_str().unwrap()).unwrap(),
                        original
                    );
                } else {
                    assert!(report["superseded"].is_null());
                }
                assert_eq!(env.pins(), 0);
                assert!(
                    std::fs::read_dir(env.dir.path()).unwrap().all(|e| !e
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .contains("staging"))
                );
                let service = enroll(&database, authority).unwrap();
                assert_eq!(service.writer_fence.epoch(), Some(2));
            }
        }
    }

    #[test]
    fn base_mismatches_leave_local_database_unchanged() {
        for mismatch in [
            "manifest",
            "set",
            "lineage",
            "epoch",
            "index",
            "chain",
            "sequence",
            "installation",
            "scope",
        ] {
            let (env, store, owner) = setup_ha();
            checkpoint_ha(&env, &store, &owner, 5..=6);
            let mut authority = prepared(&env, &store);
            let mut record = store.read().unwrap().record;
            match mismatch {
                "manifest" => record.base.as_mut().unwrap().manifest_sha256 = "e".repeat(64),
                "set" => {
                    record.base.as_mut().unwrap().set_id =
                        "20261118T000000Z-0123456789abcdef".into()
                }
                "lineage" => record.lineage = "e".repeat(16),
                "epoch" => record.base.as_mut().unwrap().epoch = record.epoch,
                "index" => record.base.as_mut().unwrap().index_sha256 = Some("e".repeat(64)),
                "chain" => record.base.as_mut().unwrap().chain_sha256 = "e".repeat(64),
                "sequence" => record.base.as_mut().unwrap().sequence += 1,
                "installation" => record.installation_id = "other".into(),
                "scope" => record.scope_id = "other".into(),
                _ => unreachable!(),
            }
            env.meta.set_raw(
                "vlite/prod/authority.json",
                &serde_json::to_vec(&record).unwrap(),
            );
            let database = env.dir.path().join("writer.vlite");
            let bytes = b"local forensic bytes";
            std::fs::write(&database, bytes).unwrap();
            let error = authority.prepare_database(&database).unwrap_err();
            assert!(
                error.starts_with("writer_authority_base_mismatch:"),
                "{mismatch}: {error}"
            );
            assert_eq!(std::fs::read(&database).unwrap(), bytes);
            assert!(
                !database
                    .with_file_name("writer.vlite.superseded-epoch1")
                    .exists()
            );
            assert_eq!(env.pins(), 0);
        }
    }

    #[test]
    fn authority_change_after_restore_conflicts_without_retry() {
        for rival_claim in [false, true] {
            let (env, store, _) = setup_ha();
            let mut authority = prepared(&env, &store);
            let database = env.dir.path().join("writer.vlite");
            authority.prepare_database(&database).unwrap();
            let observed = store.read().unwrap();
            if rival_claim {
                store.claim(&observed, 1).unwrap();
            } else {
                let mut record = observed.record;
                record.revision = "e".repeat(32);
                env.meta.set_raw(
                    "vlite/prod/authority.json",
                    &serde_json::to_vec(&record).unwrap(),
                );
            }
            let error = enroll(&database, authority).err().unwrap();
            assert!(error.starts_with("writer_authority_conflict:"), "{error}");
        }
    }

    #[test]
    fn unsafe_configurations_are_refused_before_any_set_access() {
        let (env, store, _) = setup_ha();
        let _authority = prepared(&env, &store);
        let key = format!("vlite/prod/sets/{}/record.json", env.set_id);
        env.meta.hook(Op::Get, &key, || {
            panic!("unsafe configuration accessed the set")
        });
        for (persistent, encrypted, selected, recovery) in [
            (false, false, false, false),
            (true, true, false, false),
            (true, false, true, false),
            (true, false, false, true),
        ] {
            let error = WriterAuthority::check_database_configuration(
                persistent, encrypted, selected, recovery,
            )
            .unwrap_err();
            assert!(
                error.starts_with("writer_authority_base_unverified:"),
                "{error}"
            );
        }
    }

    #[test]
    fn superseded_collision_refuses_before_download_and_ann_cache_is_removed_on_success() {
        let (env, store, _) = setup_ha();
        let mut authority = prepared(&env, &store);
        let database = env.dir.path().join("writer.vlite");
        let superseded = env.dir.path().join("writer.vlite.superseded-epoch1");
        std::fs::write(&database, b"old bytes").unwrap();
        std::fs::write(&superseded, b"keep forensic evidence").unwrap();
        let error = authority.prepare_database(&database).unwrap_err();
        assert!(error.starts_with("writer_authority_base_unverified:"));
        assert_eq!(std::fs::read(&database).unwrap(), b"old bytes");
        assert_eq!(
            std::fs::read(&superseded).unwrap(),
            b"keep forensic evidence"
        );
        assert_eq!(env.pins(), 0);

        let (objects, prefix) = env.sets.coordination();
        authority.store = AuthorityStore::new(objects, prefix);
        authority.prepared = None;
        std::fs::remove_file(&superseded).unwrap();
        let ann = crate::Database::ann_snapshot_path(&database);
        std::fs::write(&ann, b"stale cache").unwrap();
        authority.prepare_database(&database).unwrap();
        assert!(!ann.exists());
    }

    #[test]
    fn base_without_restore_is_not_claimed() {
        let (env, store, _) = setup_ha();
        let authority = prepared(&env, &store);
        let database = env.dir.path().join("source.vlite");
        let error = enroll(&database, authority).err().unwrap();
        assert!(
            error.starts_with("writer_authority_base_unverified:"),
            "{error}"
        );
    }

    #[test]
    fn opened_database_must_still_match_the_restore_evidence() {
        for change_lineage in [false, true] {
            let (env, store, _) = setup_ha();
            let mut authority = prepared(&env, &store);
            let database = env.dir.path().join("writer.vlite");
            authority.prepare_database(&database).unwrap();
            if change_lineage {
                std::fs::remove_file(&database).unwrap();
                crate::Database::create_with_format(&database, 3, crate::FormatVersion::V4)
                    .unwrap()
                    .close()
                    .unwrap();
            } else {
                let mut db = crate::Database::open(&database).unwrap();
                db.put(99, &[1.0, 2.0, 3.0], b"changed after restore")
                    .unwrap();
                db.close().unwrap();
            }
            let error = enroll(&database, authority).err().unwrap();
            assert!(
                error.starts_with("writer_authority_base_mismatch:"),
                "{error}"
            );
            assert!(store.read().unwrap().record.writer_id.is_none());
        }
    }

    #[test]
    fn claim_cas_conflict_does_not_retry() {
        let (memory, store) = setup();
        let rival = AuthorityStore::new(memory.clone(), "test/".into());
        memory.hook(Op::Put, KEY, move || {
            rival.claim(&rival.read().unwrap(), 1).unwrap();
        });
        refusal(store, Some(LINEAGE), "writer_authority_conflict");
    }

    #[test]
    fn unknown_claim_is_read_back_or_refused_as_unresolved() {
        let (memory, store) = setup();
        memory.fault(Op::Put, KEY, Fault::After, 0, 1);
        assert!(claim(store, Some(LINEAGE)).unwrap().ready());
        let (memory, store) = setup();
        memory.fault(Op::Put, KEY, Fault::After, 0, 1);
        memory.fault(Op::Get, KEY, Fault::Refuse, 1, 1);
        refusal(store, Some(LINEAGE), "writer_authority_unresolved");
    }

    #[tokio::test]
    async fn periodic_recheck_recovers_then_fences_an_idle_writer() {
        let (memory, store) = setup();
        let fence = claim(store, Some(LINEAGE)).unwrap();
        let mut authority_stop = fence.stop_signal().unwrap();
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        memory.fault(Op::Get, KEY, Fault::Refuse, 0, 1);
        assert!(fence.verify_before_commit().is_err());
        assert_eq!(fence.state(), State::AuthorityUnavailable);
        let task = tokio::spawn(recheck(fence.clone(), Duration::from_millis(5), stop_rx));
        tokio::time::timeout(Duration::from_secs(2), async {
            while fence.state() != State::Serving {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let store = AuthorityStore::new(memory, "test/".into());
        let observed = store.read().unwrap();
        store
            .prepare_transfer(&observed, 1, &observed.record.revision)
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), authority_stop.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(*authority_stop.borrow());
        assert_eq!(fence.state(), State::Fenced);
        assert_eq!(fence.exit_code(), Some(1));
        task.await.unwrap().unwrap();
        drop(stop_tx);
    }

    #[tokio::test]
    async fn disabled_recheck_does_no_store_operation() {
        let (memory, _) = setup();
        memory.hook(Op::Get, KEY, || panic!("disabled authority must not read"));
        let (_stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        recheck(WriterFence::disabled(), Duration::from_millis(1), stop_rx)
            .await
            .unwrap();
    }
}
