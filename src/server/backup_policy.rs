//! Persisted, opt-in Azure Blob backup policy, its scheduler and the admin
//! routes (feature-backup-ha ITEM-006/007).
//!
//! State lives under `<admin>/cloud-backup/`:
//!
//! - `policy.json`: the operator policy (target, schedule, retention, drill
//!   cadence). It holds no secret: managed identity needs none and the
//!   emulator key is read per process from `VLITE_CLOUD_BACKUP_SHARED_KEY_FILE`.
//! - `state.json`: run status and the handle of a publication whose final
//!   result was unknown, resolved by the next run before anything else.
//! - `installation-id`: this installation's identity, created with the first
//!   policy; it owns the configured prefix.
//! - `staging/`: snapshot copies and download directories, emptied at startup.
//!
//! One cloud operation that writes (run, collection, policy change) runs at a
//! time; downloads (drill, restore) and pins only take pins on the remote
//! record. A restore never touches the live database: it writes a new
//! `restore-*.vlite` copy with a new lineage that the existing restore
//! selection activates at the next start.
use super::cloud_backup::{self as cloud, Manifest, Publication, Retention, Sets, Target, Upload};
use super::*;
use crate::maintenance;
use serde::{Serialize, de::DeserializeOwned};
use std::io::{Read, Write};
use std::path::{Path as FilePath, PathBuf};
use std::sync::Weak;
use std::sync::atomic::AtomicBool;

const POLICY: &str = "policy.json";
const STATE: &str = "state.json";
const INSTALLATION: &str = "installation-id";
const STAGING: &str = "staging";
const MAX_FILE: u64 = 64 * 1024;
/// Delay before a scheduled run skipped by a busy queue or operation retries.
const RETRY_MS: u64 = 30_000;
/// Process-level emulator key file; never part of the persisted policy.
pub(super) const SHARED_KEY_ENV: &str = "VLITE_CLOUD_BACKUP_SHARED_KEY_FILE";

fn default_interval() -> u64 {
    3600
}
fn default_keep_sets() -> usize {
    3
}
fn default_keep_days() -> u64 {
    30
}
fn default_grace() -> u64 {
    24
}
fn default_drill_every() -> u64 {
    24
}

/// The operator policy. Everything is off until `enabled` is set; collection
/// deletes nothing unless `retention_enabled` is set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Policy {
    pub target: Target,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
    #[serde(default)]
    pub retention_enabled: bool,
    #[serde(default = "default_keep_sets")]
    pub keep_sets: usize,
    #[serde(default = "default_keep_days")]
    pub keep_days: u64,
    #[serde(default = "default_grace")]
    pub orphan_grace_hours: u64,
    /// Drill every Nth published set (0 disables scheduled drills).
    #[serde(default = "default_drill_every")]
    pub drill_every: u64,
}

impl Policy {
    pub(super) fn validate(&self) -> Result<(), String> {
        self.target.validate()?;
        if !(300..=604_800).contains(&self.interval_secs) {
            return Err("interval_secs must be 300-604800".into());
        }
        if !(1..=1000).contains(&self.keep_sets) {
            return Err("keep_sets must be 1-1000".into());
        }
        if self.keep_days > 3650 {
            return Err("keep_days must be 0-3650".into());
        }
        if !(1..=720).contains(&self.orphan_grace_hours) {
            return Err("orphan_grace_hours must be 1-720".into());
        }
        if self.drill_every > 100 {
            return Err("drill_every must be 0-100".into());
        }
        Ok(())
    }

    fn retention(&self) -> Retention {
        Retention {
            enabled: self.retention_enabled,
            keep_sets: self.keep_sets,
            keep_days: self.keep_days,
            orphan_grace_hours: self.orphan_grace_hours,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct LastError {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Status {
    /// Runs that reached the cloud (refusals included, busy skips excluded).
    pub runs: u64,
    pub published: u64,
    pub failures: u64,
    pub skipped_busy: u64,
    pub last_attempt_unix_ms: Option<u64>,
    pub last_success_unix_ms: Option<u64>,
    pub last_failure_unix_ms: Option<u64>,
    pub last_error: Option<LastError>,
    pub last_set: Option<String>,
    pub drills: u64,
    pub drill_failures: u64,
    pub last_drill: Option<Value>,
    pub last_collection: Option<Value>,
    /// A publication whose final CAS result is unknown.
    pub pending: Option<cloud::Pending>,
    pub policy_updated_unix_ms: Option<u64>,
}

struct Loaded {
    policy: Option<Policy>,
    status: Status,
    installation: Option<String>,
    retry_at: Option<u64>,
}

type Connector = Box<dyn Fn(&Target) -> Result<Sets, ApiError> + Send + Sync>;

pub(super) struct Runtime {
    dir: PathBuf,
    state: Mutex<Loaded>,
    busy: AtomicBool,
    connector: Connector,
}

/// Releases the single-operation flag.
struct Busy<'a>(&'a AtomicBool);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// What a cycle needs from the service, without keeping it alive.
#[derive(Clone)]
pub(super) struct Handle {
    database: Weak<RwLock<Option<Database>>>,
    workers: Arc<Semaphore>,
    admin: Weak<admin::AdminState>,
    webhooks: Arc<webhooks::Dispatcher>,
    served: Arc<reserved::Served>,
    encrypted: bool,
    file_backed: bool,
    metrics: Arc<Metrics>,
}

impl Handle {
    pub(super) fn new(s: &Service) -> Self {
        Self {
            database: Arc::downgrade(&s.database),
            workers: s.workers.clone(),
            admin: Arc::downgrade(&s.admin),
            webhooks: s.webhooks.clone(),
            served: s.memory_bindings.clone(),
            encrypted: s.encryption_key.is_some(),
            file_backed: s.ops.file_backed(),
            metrics: s.metrics.clone(),
        }
    }

    fn refusal(&self) -> Result<(), ApiError> {
        if !self.file_backed {
            return Err(conflict(
                "cloud_backup_requires_database_file",
                "cloud backups need a file-backed database; this server runs in memory or in recovery mode",
            ));
        }
        match recovery_bundle::refusal(self.encrypted, !self.served.is_empty()) {
            Some((code, message)) => Err(ApiError::new(StatusCode::CONFLICT, code, message)),
            None => Ok(()),
        }
    }

    fn audit(&self, event: Value) {
        let Some(admin) = self.admin.upgrade() else {
            return;
        };
        if let Some(log) = &admin.audit_log
            && log.append(event, true).is_err()
        {
            self.metrics.audit_failures.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn now() -> u64 {
    admin::now_ms() as u64
}

fn lock<T>(mutex: &Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>, ApiError> {
    mutex.lock().map_err(internal)
}

fn conflict(code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, code, message)
}

fn not_configured() -> ApiError {
    conflict(
        "cloud_backup_not_configured",
        "configure the cloud backup policy first (PUT /v1/admin/cloud-backup)",
    )
}

fn busy_queue() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "busy",
        "database queue is full",
    )
}

fn io_error(message: impl Into<String>) -> std::io::Error {
    std::io::Error::other(message.into())
}

fn read_limited(path: &FilePath) -> std::io::Result<Option<Vec<u8>>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.take(MAX_FILE + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE {
        return Err(io_error(format!("{} is too large", path.display())));
    }
    Ok(Some(bytes))
}

fn read_json<T: DeserializeOwned>(path: &FilePath) -> std::io::Result<Option<T>> {
    match read_limited(path)? {
        None => Ok(None),
        Some(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| io_error(format!("invalid {}: {e}", path.display()))),
    }
}

fn valid_installation(id: &str) -> bool {
    id.len() == 16
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn write_atomic(dir: &FilePath, name: &str, bytes: &[u8]) -> Result<(), ApiError> {
    let mut file = tempfile::NamedTempFile::new_in(dir).map_err(internal)?;
    file.write_all(bytes).map_err(internal)?;
    file.as_file().sync_all().map_err(internal)?;
    file.persist(dir.join(name)).map_err(internal)?;
    admin::sync_directory(dir)
}

fn real_directory(path: &FilePath) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => Err(io_error(format!(
            "{} must be a real directory",
            path.display()
        ))),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// The production connector: managed identity, or the emulator shared key
/// read from the process-level key file.
fn connect(target: &Target) -> Result<Sets, ApiError> {
    let key = match (target.auth, std::env::var_os(SHARED_KEY_ENV)) {
        (cloud::AuthMode::EmulatorSharedKey, Some(path)) => {
            Some(cloud::azure::read_shared_key(FilePath::new(&path)).map_err(unprocessable)?)
        }
        _ => None,
    };
    cloud::azure::connect(target, key)
}

fn inspection(i: &maintenance::Inspection) -> Value {
    json!({"format_version":i.format_version,"dimensions":i.dimensions,"live_records":i.live_records,
        "commits":i.commits,"file_bytes":i.file_bytes,"valid_bytes":i.valid_bytes,
        "incomplete_tail_bytes":i.incomplete_tail_bytes})
}

fn lineage_hex(lineage: Option<u64>) -> Option<String> {
    lineage.map(|l| format!("{l:016x}"))
}

/// What the operator must re-provision; the bundle never carries secrets.
fn bundle_summary(bundle: &recovery_bundle::Bundle) -> Value {
    let c = &bundle.config;
    json!({"schema":bundle.schema,"created_unix_ms":bundle.created_unix_ms,
        "installation_id":bundle.installation_id,"source_lineage":bundle.source_lineage,
        "rekey_only":bundle.rekey_only,"tenants":c.tenants.len(),"accounts":c.accounts.len(),
        "credentials":c.credentials.len(),"webhook_subscriptions":c.webhook_subscriptions,
        "secret_references":c.secret_references,"excluded":c.excluded})
}

fn next_run(loaded: &Loaded) -> Option<u64> {
    let policy = loaded.policy.as_ref().filter(|p| p.enabled)?;
    Some(loaded.retry_at.unwrap_or_else(|| {
        let s = &loaded.status;
        s.last_attempt_unix_ms
            .max(s.policy_updated_unix_ms)
            .unwrap_or(0)
            .saturating_add(policy.interval_secs.saturating_mul(1000))
    }))
}

impl Runtime {
    pub(super) fn open(dir: PathBuf) -> std::io::Result<Self> {
        Self::open_with(dir, Box::new(connect))
    }

    /// Loads the policy and state; an invalid file fails startup rather than
    /// silently disabling backups. Leftover staging data is removed.
    pub(super) fn open_with(dir: PathBuf, connector: Connector) -> std::io::Result<Self> {
        if !real_directory(&dir)? {
            std::fs::create_dir_all(&dir)?;
        }
        let staging = dir.join(STAGING);
        if real_directory(&staging)? {
            std::fs::remove_dir_all(&staging)?;
        }
        std::fs::create_dir(&staging)?;
        let policy: Option<Policy> = read_json(&dir.join(POLICY))?;
        if let Some(policy) = &policy {
            policy
                .validate()
                .map_err(|e| io_error(format!("invalid cloud backup policy: {e}")))?;
        }
        let status: Status = read_json(&dir.join(STATE))?.unwrap_or_default();
        let installation = match read_limited(&dir.join(INSTALLATION))? {
            None => None,
            Some(bytes) => {
                let id = String::from_utf8(bytes)
                    .ok()
                    .filter(|id| valid_installation(id))
                    .ok_or_else(|| io_error("invalid cloud backup installation-id"))?;
                Some(id)
            }
        };
        if policy.is_some() && installation.is_none() {
            return Err(io_error(
                "cloud backup policy without an installation-id; restore the file or remove the policy",
            ));
        }
        Ok(Self {
            dir,
            state: Mutex::new(Loaded {
                policy,
                status,
                installation,
                retry_at: None,
            }),
            busy: AtomicBool::new(false),
            connector,
        })
    }

    pub(super) fn enabled(&self) -> bool {
        self.state
            .lock()
            .is_ok_and(|s| s.policy.as_ref().is_some_and(|p| p.enabled))
    }

    fn try_busy(&self) -> Result<Busy<'_>, ApiError> {
        if self.busy.swap(true, Ordering::AcqRel) {
            return Err(conflict(
                "cloud_backup_busy",
                "another cloud backup run, collection or policy change is in progress; retry later",
            ));
        }
        Ok(Busy(&self.busy))
    }

    fn staging(&self) -> PathBuf {
        self.dir.join(STAGING)
    }

    fn workdir(&self) -> Result<tempfile::TempDir, ApiError> {
        tempfile::Builder::new()
            .prefix("fetch-")
            .tempdir_in(self.staging())
            .map_err(internal)
    }

    pub(super) fn view(&self) -> Result<Value, ApiError> {
        let loaded = lock(&self.state)?;
        Ok(json!({
            "configured": loaded.policy.is_some(),
            "policy": loaded.policy,
            "installation_id": loaded.installation,
            "status": loaded.status,
            "running": self.busy.load(Ordering::Acquire),
            "next_run_unix_ms": next_run(&loaded),
        }))
    }

    /// Applies `change` and persists the status while holding the state lock,
    /// so status writes never reorder.
    fn update(&self, change: impl FnOnce(&mut Loaded)) -> Result<(), ApiError> {
        let mut loaded = lock(&self.state)?;
        change(&mut loaded);
        let bytes = serde_json::to_vec_pretty(&loaded.status).map_err(internal)?;
        write_atomic(&self.dir, STATE, &bytes)
    }

    /// `update` whose persistence failure is logged: the in-memory status is
    /// still current and the next write retries.
    fn note(&self, change: impl FnOnce(&mut Loaded)) {
        if let Err(error) = self.update(change) {
            logs::stderr(
                "error",
                "cloud_backup",
                json!({"event":"cloud_backup_state_write_failed","code":error.code}).to_string(),
            );
        }
    }

    fn configured(&self) -> Result<(Policy, String), ApiError> {
        let loaded = lock(&self.state)?;
        match (&loaded.policy, &loaded.installation) {
            (Some(policy), Some(installation)) => Ok((policy.clone(), installation.clone())),
            _ => Err(not_configured()),
        }
    }

    fn connect(&self) -> Result<(Sets, Policy, String), ApiError> {
        let (policy, installation) = self.configured()?;
        let sets = (self.connector)(&policy.target)?;
        Ok((sets, policy, installation))
    }

    /// Replaces the policy. Changing the target abandons an unresolved
    /// publication of the old target (it stays an orphan there).
    pub(super) fn configure(
        &self,
        policy: Policy,
        handle: &Handle,
        now: u64,
    ) -> Result<Value, ApiError> {
        handle.refusal_for_policy()?;
        policy.validate().map_err(unprocessable)?;
        let _busy = self.try_busy()?;
        let abandoned = {
            let mut loaded = lock(&self.state)?;
            if loaded.installation.is_none() {
                let mut bytes = [0u8; 8];
                getrandom::fill(&mut bytes).map_err(internal)?;
                let id = hex(&bytes);
                write_atomic(&self.dir, INSTALLATION, id.as_bytes())?;
                loaded.installation = Some(id);
            }
            let mut status = loaded.status.clone();
            let changed = loaded.policy.as_ref().map(|p| &p.target) != Some(&policy.target);
            let abandoned = if changed { status.pending.take() } else { None };
            status.policy_updated_unix_ms = Some(now);
            let policy_bytes = serde_json::to_vec_pretty(&policy).map_err(internal)?;
            let status_bytes = serde_json::to_vec_pretty(&status).map_err(internal)?;
            write_atomic(&self.dir, POLICY, &policy_bytes)?;
            write_atomic(&self.dir, STATE, &status_bytes)?;
            loaded.policy = Some(policy.clone());
            loaded.status = status;
            loaded.retry_at = None;
            abandoned
        };
        if let Some(pending) = abandoned {
            handle.audit(json!({"event":"cloud_backup_pending_abandoned","set_id":pending.set_id}));
        }
        handle.audit(
            json!({"event":"cloud_backup_policy_updated","enabled":policy.enabled,
            "account":policy.target.account,"container":policy.target.container,
            "prefix":policy.target.prefix,"retention_enabled":policy.retention_enabled}),
        );
        self.view()
    }

    /// Claims a due scheduled run (the caller then calls `start_claimed`).
    /// Called by the maintenance scheduler under its own lock.
    pub(super) fn claim_due(&self) -> bool {
        self.claim_due_at(now())
    }

    fn claim_due_at(&self, now: u64) -> bool {
        let Ok(mut loaded) = self.state.lock() else {
            return false;
        };
        match next_run(&loaded) {
            Some(next) if now >= next => {}
            _ => return false,
        }
        if self.busy.swap(true, Ordering::AcqRel) {
            loaded.status.skipped_busy += 1;
            loaded.retry_at = Some(now + RETRY_MS);
            return false;
        }
        loaded.status.last_attempt_unix_ms = Some(now);
        loaded.retry_at = None;
        true
    }

    /// Runs a claimed scheduled cycle on its own thread, then releases the
    /// claim. Shutdown does not wait for it: an interrupted run leaves an
    /// unpublished set that collection removes, or a pending handle.
    pub(super) fn start_claimed(self: &Arc<Self>, handle: Handle) {
        let runtime = self.clone();
        let spawned = std::thread::Builder::new()
            .name("vlite-cloud-backup".into())
            .spawn(move || {
                let _claim = Busy(&runtime.busy);
                let _ = runtime.cycle(&handle, true);
            });
        if spawned.is_err() {
            self.busy.store(false, Ordering::Release);
            let at = now();
            self.note(|l| {
                l.status.failures += 1;
                l.status.last_failure_unix_ms = Some(at);
                l.status.last_error = Some(LastError {
                    code: "cloud_backup_thread_failed".into(),
                    message: "the scheduled cloud backup thread could not start".into(),
                });
            });
        }
    }

    /// An operator-requested run; refused while another one runs.
    pub(super) fn run_now(&self, handle: &Handle) -> Result<Value, ApiError> {
        let _busy = self.try_busy()?;
        self.cycle(handle, false)
    }

    /// Settles a pending publication, captures a consistent database copy and
    /// recovery bundle, publishes the set, then drills and collects as the
    /// policy asks. Every outcome is recorded in the status and audited.
    fn cycle(&self, handle: &Handle, scheduled: bool) -> Result<Value, ApiError> {
        let started = now();
        let (policy, installation) = self.configured()?;
        let pending = {
            let mut loaded = lock(&self.state)?;
            loaded.status.last_attempt_unix_ms = Some(started);
            loaded.status.pending.clone()
        };
        let outcome = self.publish(handle, &policy, &installation, pending, started);
        let (sets, record, settled) = match outcome {
            Ok(done) => done,
            Err(error) if error.code == "busy" => {
                self.note(|l| {
                    l.status.skipped_busy += 1;
                    if scheduled {
                        l.retry_at = Some(started + RETRY_MS);
                    }
                });
                return Err(error);
            }
            Err(error) => {
                let at = now();
                self.note(|l| {
                    l.status.runs += 1;
                    l.status.failures += 1;
                    l.status.last_failure_unix_ms = Some(at);
                    l.status.last_error = Some(LastError {
                        code: error.code.into(),
                        message: error.message.clone(),
                    });
                });
                handle.audit(json!({"event":"cloud_backup_failed","code":error.code,
                    "message":error.message,"scheduled":scheduled}));
                logs::stderr(
                    "error",
                    "cloud_backup",
                    json!({"event":"cloud_backup_failed","code":error.code,"scheduled":scheduled})
                        .to_string(),
                );
                return Err(error);
            }
        };
        let at = now();
        let mut published = 0;
        self.note(|l| {
            l.status.runs += 1;
            l.status.published += 1;
            l.status.last_success_unix_ms = Some(at);
            l.status.last_set = Some(record.set_id.clone());
            published = l.status.published;
        });
        handle.audit(
            json!({"event":"cloud_backup_published","set_id":record.set_id,
            "scheduled":scheduled}),
        );
        let drill = (policy.drill_every > 0 && published % policy.drill_every == 0).then(|| {
            match sets.pin(&record.set_id, "drill", now()) {
                Ok(pin) => self.drill_pinned(handle, &sets, &record.set_id, &pin).0,
                Err(error) => self.drilled(handle, &record.set_id, Err(error), false).0,
            }
        });
        let collection = policy
            .retention_enabled
            .then(|| self.collect_with(handle, &sets, &policy, &installation).0);
        Ok(
            json!({"set_id":record.set_id,"record":record,"settled":settled,
            "drill":drill,"collection":collection}),
        )
    }

    fn publish(
        &self,
        handle: &Handle,
        policy: &Policy,
        installation: &str,
        pending: Option<cloud::Pending>,
        now: u64,
    ) -> Result<(Sets, cloud::Record, Option<Value>), ApiError> {
        handle.refusal()?;
        let sets = (self.connector)(&policy.target)?;
        sets.ensure_owner(installation, now)?;
        let settled = match pending {
            Some(pending) => Some(self.settle(handle, &sets, &pending, now)?),
            None => None,
        };
        let admin = handle.admin.upgrade().ok_or_else(unavailable)?;
        // The configuration is read before and after the copy (outside the
        // database lock, so no lock order is introduced); a change between
        // the two fails the set instead of pairing mismatched states.
        let before = recovery_bundle::capture(&admin, &handle.webhooks, now)?;
        let staged = {
            let Ok(_permit) = handle.workers.clone().try_acquire_owned() else {
                return Err(busy_queue());
            };
            let database = handle.database.upgrade().ok_or_else(unavailable)?;
            let mut owner = database.write().map_err(internal)?;
            let db = owner.as_mut().ok_or_else(unavailable)?;
            ops::snapshot(db, &self.staging())?.1
        };
        let after = recovery_bundle::capture(&admin, &handle.webhooks, now)?;
        recovery_bundle::stable(&before, &after)?;
        drop(admin);
        let verification = maintenance::verify(staged.path())?;
        let detail = maintenance::inspect_detail(staged.path())?;
        let bundle = recovery_bundle::bundle(after, installation, detail.lineage, now);
        let uploads = vec![
            Upload::file(cloud::ROLE_DATABASE, staged.path().to_path_buf()).map_err(internal)?,
            Upload::bytes(
                cloud::ROLE_BUNDLE,
                serde_json::to_vec_pretty(&bundle).map_err(internal)?,
            ),
        ];
        let set_id = cloud::new_set_id(now)?;
        let manifest = Manifest::new(&set_id, installation, now, &verification, &detail, &uploads);
        let publication = sets.publish(&manifest, &uploads, now)?;
        drop(staged);
        match publication {
            Publication::Published(record) => Ok((sets, record, settled)),
            Publication::Pending(pending) => {
                let set_id = pending.set_id.clone();
                self.note(|l| l.status.pending = Some(pending));
                Err(ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "cloud_publication_unknown",
                    format!(
                        "the publication of {set_id} could not be confirmed; the next run resolves it before publishing again"
                    ),
                ))
            }
        }
    }

    /// Resolves the persisted pending publication before a new one.
    fn settle(
        &self,
        handle: &Handle,
        sets: &Sets,
        pending: &cloud::Pending,
        now: u64,
    ) -> Result<Value, ApiError> {
        match sets.resolve_pending(pending, now) {
            Ok(Publication::Published(record)) => {
                self.note(|l| {
                    l.status.pending = None;
                    l.status.published += 1;
                    l.status.last_success_unix_ms = Some(now);
                    l.status.last_set = Some(record.set_id.clone());
                });
                handle.audit(
                    json!({"event":"cloud_backup_published","set_id":record.set_id,
                    "resolved":true}),
                );
                Ok(json!({"set_id":record.set_id,"outcome":"published"}))
            }
            Ok(Publication::Pending(_)) => Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "cloud_publication_unresolved",
                format!(
                    "the earlier publication of {} is still unconfirmed; retry later",
                    pending.set_id
                ),
            )),
            Err(error) if error.code == "cloud_publication_lost" => {
                self.note(|l| l.status.pending = None);
                handle.audit(json!({"event":"cloud_backup_publication_lost",
                    "set_id":pending.set_id}));
                Ok(json!({"set_id":pending.set_id,"outcome":"lost"}))
            }
            Err(error) => Err(error),
        }
    }

    /// Downloads and verifies a set pinned for the drill, restores it to a
    /// scratch copy and probes it, then records the result on the set
    /// (removing the pin). Returns the report and the failure, if any.
    fn drill_pinned(
        &self,
        handle: &Handle,
        sets: &Sets,
        set_id: &str,
        pin: &cloud::Pin,
    ) -> (Value, Option<ApiError>) {
        let result = (|| -> Result<Value, ApiError> {
            let work = self.workdir()?;
            let fetched = sets.fetch(set_id, &pin.id, work.path())?;
            let report = maintenance::drill(&fetched.database, work.path())?;
            Ok(json!({"backup":inspection(&report.backup),
                "scratch_lineage":lineage_hex(report.scratch_lineage),
                "collections":report.collections,"probes":report.probes,
                "verified_bytes":fetched.verification.bytes,"crc32":fetched.verification.crc32}))
        })();
        let detail = match &result {
            Ok(_) => "download, verification, restore and probes passed".to_owned(),
            Err(error) => format!("{}: {}", error.code, error.message)
                .chars()
                .take(500)
                .collect(),
        };
        let drill = cloud::DrillResult {
            ok: result.is_ok(),
            unix_ms: now(),
            detail,
        };
        let recorded = match sets.record_drill(set_id, &pin.id, &drill, now()) {
            Ok(_) => true,
            Err(_) => {
                // Best effort: a pin left behind only delays retention.
                let _ = sets.unpin(set_id, &pin.id, now());
                false
            }
        };
        self.drilled(handle, set_id, result, recorded)
    }

    fn drilled(
        &self,
        handle: &Handle,
        set_id: &str,
        result: Result<Value, ApiError>,
        recorded: bool,
    ) -> (Value, Option<ApiError>) {
        let at = now();
        let report = match &result {
            Ok(detail) => json!({"set_id":set_id,"ok":true,"unix_ms":at,"recorded":recorded,
                "detail":detail}),
            Err(error) => json!({"set_id":set_id,"ok":false,"unix_ms":at,"recorded":recorded,
                "error":{"code":error.code,"message":error.message}}),
        };
        let ok = result.is_ok();
        let summary = report.clone();
        self.note(|l| {
            l.status.drills += 1;
            if !ok {
                l.status.drill_failures += 1;
            }
            l.status.last_drill = Some(summary);
        });
        handle.audit(
            json!({"event": if ok {"cloud_backup_drill_passed"} else {"cloud_backup_drill_failed"},
            "set_id":set_id}),
        );
        (report, result.err())
    }

    fn collect_with(
        &self,
        handle: &Handle,
        sets: &Sets,
        policy: &Policy,
        installation: &str,
    ) -> (Value, Option<ApiError>) {
        let at = now();
        let (report, error) = match sets.collect(installation, &policy.retention(), at) {
            Ok(collection) => (
                json!({"unix_ms":at,"ok":true,
                "retention_enabled":policy.retention_enabled,"result":collection}),
                None,
            ),
            Err(error) => (
                json!({"unix_ms":at,"ok":false,
                "error":{"code":error.code,"message":error.message}}),
                Some(error),
            ),
        };
        let summary = report.clone();
        self.note(|l| l.status.last_collection = Some(summary));
        handle.audit(json!({"event":"cloud_backup_collected","ok":report["ok"],
            "deleted":report["result"]["deleted"]}));
        (report, error)
    }

    /// Janitor (staging orphans, interrupted deletions) plus retention when
    /// enabled. Needs the janitor role; refused while a run is active.
    pub(super) fn collect(&self, handle: &Handle) -> Result<Value, ApiError> {
        let _busy = self.try_busy()?;
        let (sets, policy, installation) = self.connect()?;
        match self.collect_with(handle, &sets, &policy, &installation) {
            (report, None) => Ok(report),
            (_, Some(error)) => Err(error),
        }
    }

    pub(super) fn list(&self) -> Result<Value, ApiError> {
        let (sets, _, installation) = self.connect()?;
        let listing = sets.list()?;
        let tombstoned: Vec<Value> = listing
            .tombstoned
            .iter()
            .map(|(id, objects)| json!({"set_id":id,"data_objects":objects}))
            .collect();
        Ok(json!({"installation_id":installation,"sets":listing.sets,"tombstoned":tombstoned}))
    }

    /// An operator drill. A set that cannot be pinned (absent, not published)
    /// is refused without counting a drill; any later failure is recorded on
    /// the set and in the status, then returned.
    pub(super) fn drill(&self, handle: &Handle, set_id: &str) -> Result<Value, ApiError> {
        let (sets, _, _) = self.connect()?;
        let pin = sets.pin(set_id, "drill", now())?;
        match self.drill_pinned(handle, &sets, set_id, &pin) {
            (report, None) => Ok(report),
            (_, Some(error)) => Err(error),
        }
    }

    pub(super) fn pin(
        &self,
        handle: &Handle,
        set_id: &str,
        reason: &str,
    ) -> Result<Value, ApiError> {
        let (sets, _, _) = self.connect()?;
        let pin = sets.pin(set_id, reason, now())?;
        handle.audit(json!({"event":"cloud_backup_pinned","set_id":set_id,"pin_id":pin.id}));
        Ok(json!({"set_id":set_id,"pin":pin}))
    }

    pub(super) fn unpin(
        &self,
        handle: &Handle,
        set_id: &str,
        pin_id: &str,
    ) -> Result<Value, ApiError> {
        let (sets, _, _) = self.connect()?;
        sets.unpin(set_id, pin_id, now())?;
        handle.audit(json!({"event":"cloud_backup_unpinned","set_id":set_id,"pin_id":pin_id}));
        Ok(json!({"set_id":set_id,"pin_id":pin_id,"removed":true}))
    }

    /// Downloads and verifies a set, then restores it (new lineage) as a new
    /// `restore-*.vlite` in the admin directory. Nothing is selected and no
    /// configuration is applied: the bundle summary lists what to re-key.
    pub(super) fn restore(
        &self,
        handle: &Handle,
        set_id: &str,
        admin_dir: &FilePath,
    ) -> Result<Value, ApiError> {
        handle.refusal_for_policy()?;
        let (sets, _, _) = self.connect()?;
        let pin = sets.pin(set_id, "restore", now())?;
        let result = (|| {
            let work = self.workdir()?;
            let fetched = sets.fetch(set_id, &pin.id, work.path())?;
            let name = admin::name("restore-");
            let destination = admin_dir.join(&name);
            let restored = maintenance::restore(&fetched.database, &destination)?;
            admin::sync_directory(admin_dir)?;
            let lineage = maintenance::inspect_detail(&destination)?.lineage;
            Ok::<_, ApiError>(json!({"name":name,"set_id":set_id,
                "inspection":inspection(&restored),"lineage":lineage_hex(lineage),
                "source_lineage":fetched.manifest.source_lineage,
                "source_installation_id":fetched.record.installation_id,
                "bundle":bundle_summary(&fetched.bundle),"rekey_only":true,
                "restart_required":true,
                "next":"select the copy with POST /v1/admin/restores/select, then restart"}))
        })();
        let released = sets.unpin(set_id, &pin.id, now()).is_ok();
        let mut report = result?;
        report["pin_released"] = json!(released);
        handle.audit(json!({"event":"cloud_backup_restored","set_id":set_id,
            "name":report["name"],"pin_released":released}));
        Ok(report)
    }
}

impl Handle {
    /// Policy changes and restores are refused for an encrypted database
    /// (memory reservations are checked per run: they can be reloaded).
    fn refusal_for_policy(&self) -> Result<(), ApiError> {
        match recovery_bundle::refusal(self.encrypted, false) {
            Some((code, message)) => Err(conflict(code, message)),
            None => Ok(()),
        }
    }
}

// ----------------------------------------------------------------- routes

pub(super) fn routes() -> Router<Service> {
    Router::new()
        .route("/v1/admin/cloud-backup", get(status).put(configure))
        .route("/v1/admin/cloud-backup/run", post(run))
        .route("/v1/admin/cloud-backup/collect", post(collect))
        .route("/v1/admin/cloud-backup/sets", get(list))
        .route("/v1/admin/cloud-backup/sets/{id}/drill", post(drill))
        .route("/v1/admin/cloud-backup/sets/{id}/restore", post(restore))
        .route("/v1/admin/cloud-backup/sets/{id}/pins", post(pin))
        .route(
            "/v1/admin/cloud-backup/sets/{id}/pins/{pin}",
            axum::routing::delete(unpin),
        )
}

fn runtime(s: &Service) -> Result<Arc<Runtime>, ApiError> {
    s.admin.cloud.get().cloned().ok_or_else(|| {
        conflict(
            "admin_directory_disabled",
            "configure VLITE_ADMIN_DIR to enable cloud backups",
        )
    })
}

fn set_id(id: &str) -> Result<(), ApiError> {
    if cloud::valid_set_id(id) {
        Ok(())
    } else {
        Err(unprocessable("invalid backup set id"))
    }
}

/// Cloud requests block (and their HTTP client must not run on the async
/// runtime): every operation runs on the blocking pool.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(work).await.map_err(internal)?
}

async fn status(State(s): State<Service>) -> Result<Json<Value>, ApiError> {
    Ok(Json(runtime(&s)?.view()?))
}

async fn configure(
    State(s): State<Service>,
    input: Result<Json<Policy>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let policy = body(input)?;
    let runtime = runtime(&s)?;
    let handle = Handle::new(&s);
    let view = blocking(move || runtime.configure(policy, &handle, now())).await?;
    s.ops.cloud_changed();
    Ok(Json(view))
}

async fn run(State(s): State<Service>) -> Result<(StatusCode, Json<Value>), ApiError> {
    let runtime = runtime(&s)?;
    let handle = Handle::new(&s);
    let report = blocking(move || runtime.run_now(&handle)).await?;
    Ok((StatusCode::CREATED, Json(report)))
}

async fn collect(State(s): State<Service>) -> Result<Json<Value>, ApiError> {
    let runtime = runtime(&s)?;
    let handle = Handle::new(&s);
    Ok(Json(blocking(move || runtime.collect(&handle)).await?))
}

async fn list(State(s): State<Service>) -> Result<Json<Value>, ApiError> {
    let runtime = runtime(&s)?;
    Ok(Json(blocking(move || runtime.list()).await?))
}

async fn drill(State(s): State<Service>, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    set_id(&id)?;
    let runtime = runtime(&s)?;
    let handle = Handle::new(&s);
    Ok(Json(blocking(move || runtime.drill(&handle, &id)).await?))
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
    /// Restore as a new installation (new lineage, re-key everything).
    #[default]
    Restore,
    /// Same-lineage standby seeding: HA scope, not this milestone.
    Seed,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RestoreInput {
    #[serde(default)]
    mode: Mode,
}

async fn restore(
    State(s): State<Service>,
    Path(id): Path<String>,
    bytes: axum::body::Bytes,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let input: RestoreInput = optional(&bytes)?;
    if input.mode == Mode::Seed {
        return Err(conflict(
            "cloud_seed_mode_deferred",
            "same-lineage standby seeding is not available; restore creates a new lineage",
        ));
    }
    set_id(&id)?;
    let runtime = runtime(&s)?;
    let dir = s.admin.directory.clone().ok_or_else(unavailable)?;
    let handle = Handle::new(&s);
    let report = blocking(move || runtime.restore(&handle, &id, &dir)).await?;
    Ok((StatusCode::CREATED, Json(report)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PinInput {
    reason: String,
}

async fn pin(
    State(s): State<Service>,
    Path(id): Path<String>,
    input: Result<Json<PinInput>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let input = body(input)?;
    set_id(&id)?;
    let runtime = runtime(&s)?;
    let handle = Handle::new(&s);
    let report = blocking(move || runtime.pin(&handle, &id, &input.reason)).await?;
    Ok((StatusCode::CREATED, Json(report)))
}

async fn unpin(
    State(s): State<Service>,
    Path((id, pin)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    set_id(&id)?;
    if !cloud::valid_pin_id(&pin) {
        return Err(unprocessable("invalid pin id"));
    }
    let runtime = runtime(&s)?;
    let handle = Handle::new(&s);
    Ok(Json(
        blocking(move || runtime.unpin(&handle, &id, &pin)).await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::super::cloud_backup::fake::{Fault, Memory, Op};
    use super::*;
    use axum::{body::Body, http::Request};
    use std::time::{Duration, Instant};
    use tower::ServiceExt;

    const BASE: u64 = 1_700_000_000_000;

    struct Env {
        _dir: tempfile::TempDir,
        root: PathBuf,
        meta: Arc<Memory>,
        data: Arc<Memory>,
        service: Service,
        runtime: Arc<Runtime>,
    }

    fn target() -> Value {
        json!({"account":"vlitebackup","container":"vlite-data",
            "coordination_container":"vlite-meta","prefix":"vlite/prod/"})
    }

    fn policy(extra: Value) -> Policy {
        let mut value = json!({ "target": target() });
        for (key, item) in extra.as_object().unwrap() {
            value[key] = item.clone();
        }
        serde_json::from_value(value).unwrap()
    }

    fn connector(meta: &Arc<Memory>, data: &Arc<Memory>) -> Connector {
        let (meta, data) = (meta.clone(), data.clone());
        Box::new(move |target: &Target| {
            Ok(Sets::new(
                meta.clone(),
                data.clone(),
                false,
                target.prefix.clone(),
            ))
        })
    }

    /// A file-backed service, as `configured_service` leaves it.
    fn service(root: &FilePath) -> Service {
        let path = root.join("live.vlite");
        let mut database = Database::create(&path, 3).unwrap();
        for id in 1..=4u64 {
            database
                .put(id, &[id as f32, 0.5, -1.0], format!("p{id}").as_bytes())
                .unwrap();
        }
        let service = Service::new(database);
        service
            .ops
            .configure(None, Some(path), None, &ops::Schedule::default())
            .unwrap();
        service
    }

    fn setup() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let (meta, data) = (Arc::new(Memory::default()), Arc::new(Memory::default()));
        let service = service(&root);
        let runtime = Runtime::open_with(root.join("cloud-backup"), connector(&meta, &data));
        Env {
            _dir: dir,
            meta,
            data,
            service,
            runtime: Arc::new(runtime.unwrap()),
            root,
        }
    }

    fn reopen(env: &Env) -> Runtime {
        Runtime::open_with(
            env.root.join("cloud-backup"),
            connector(&env.meta, &env.data),
        )
        .unwrap()
    }

    fn status(runtime: &Runtime) -> Value {
        runtime.view().unwrap()["status"].clone()
    }

    fn staging_empty(runtime: &Runtime) -> bool {
        std::fs::read_dir(runtime.staging())
            .unwrap()
            .next()
            .is_none()
    }

    #[track_caller]
    fn code<T>(result: Result<T, ApiError>) -> &'static str {
        result.err().expect("expected an error").code
    }

    fn set(listing: &Value, id: &str) -> Value {
        listing["sets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["set_id"] == id)
            .cloned()
            .unwrap_or(Value::Null)
    }

    #[test]
    fn policy_is_off_by_default_and_bounded() {
        let p = policy(json!({}));
        assert!(!p.enabled && !p.retention_enabled);
        assert_eq!(
            (
                p.interval_secs,
                p.keep_sets,
                p.keep_days,
                p.orphan_grace_hours,
                p.drill_every
            ),
            (3600, 3, 30, 24, 24)
        );
        p.validate().unwrap();
        for bad in [
            json!({"interval_secs":299}),
            json!({"interval_secs":604_801}),
            json!({"keep_sets":0}),
            json!({"keep_sets":1001}),
            json!({"keep_days":3651}),
            json!({"orphan_grace_hours":0}),
            json!({"orphan_grace_hours":721}),
            json!({"drill_every":101}),
        ] {
            assert!(policy(bad.clone()).validate().is_err(), "{bad}");
        }
        // A secret can never be part of the persisted policy.
        for field in [
            "shared_key",
            "account_key",
            "sas_token",
            "connection_string",
        ] {
            let mut value = json!({ "target": target() });
            value[field] = json!("secret");
            assert!(serde_json::from_value::<Policy>(value).is_err(), "{field}");
            let mut value = json!({ "target": target() });
            value["target"][field] = json!("secret");
            assert!(serde_json::from_value::<Policy>(value).is_err(), "{field}");
        }
    }

    #[test]
    fn startup_clears_staging_and_fails_on_invalid_state() {
        let dir = tempfile::tempdir().unwrap();
        let cloud = dir.path().join("cloud-backup");
        let (meta, data) = (Arc::new(Memory::default()), Arc::new(Memory::default()));
        std::fs::create_dir_all(cloud.join("staging/fetch-old")).unwrap();
        std::fs::write(cloud.join("staging/fetch-old/database"), b"stale").unwrap();
        let runtime = Runtime::open_with(cloud.clone(), connector(&meta, &data)).unwrap();
        assert!(staging_empty(&runtime));
        assert_eq!(runtime.view().unwrap()["configured"], false);
        assert!(!runtime.enabled());
        drop(runtime);
        let open = || Runtime::open_with(cloud.clone(), connector(&meta, &data)).err();

        let valid = serde_json::to_vec(&policy(json!({"enabled":true}))).unwrap();
        std::fs::write(cloud.join(POLICY), &valid).unwrap();
        let error = open().expect("a policy without its installation id must fail");
        assert!(error.to_string().contains("installation-id"), "{error}");
        std::fs::write(cloud.join(INSTALLATION), b"0123456789abcdef").unwrap();
        assert!(open().is_none());

        let mut out_of_range = policy(json!({}));
        out_of_range.interval_secs = 1;
        let bytes = serde_json::to_vec(&out_of_range).unwrap();
        std::fs::write(cloud.join(POLICY), bytes).unwrap();
        let error = open().expect("an out-of-range policy must fail");
        assert!(error.to_string().contains("interval_secs"), "{error}");
        std::fs::write(cloud.join(POLICY), b"{not json").unwrap();
        assert!(open().is_some());

        std::fs::write(cloud.join(POLICY), &valid).unwrap();
        std::fs::write(cloud.join(INSTALLATION), b"../../etc/passwd").unwrap();
        assert!(open().is_some());
        std::fs::write(cloud.join(INSTALLATION), b"0123456789abcdef").unwrap();
        std::fs::write(cloud.join(STATE), br#"{"runs":"many"}"#).unwrap();
        assert!(open().is_some());
    }

    #[test]
    fn manual_run_publishes_drills_and_persists_status() {
        let env = setup();
        let h = Handle::new(&env.service);
        assert_eq!(code(env.runtime.run_now(&h)), "cloud_backup_not_configured");
        assert_eq!(code(env.runtime.list()), "cloud_backup_not_configured");
        let view = env
            .runtime
            .configure(policy(json!({"drill_every":1})), &h, BASE)
            .unwrap();
        assert_eq!(view["configured"], true);
        assert_eq!(view["next_run_unix_ms"], Value::Null, "disabled policy");
        let installation = view["installation_id"].as_str().unwrap().to_owned();

        let report = env.runtime.run_now(&h).unwrap();
        let id = report["set_id"].as_str().unwrap().to_owned();
        assert_eq!(report["record"]["state"], "published");
        assert_eq!(report["drill"]["ok"], true, "{report}");
        assert_eq!(report["drill"]["recorded"], true);
        assert_eq!(report["drill"]["detail"]["backup"]["live_records"], 4);
        assert!(report["collection"].is_null(), "retention is off");
        assert!(report["settled"].is_null());
        let s = status(&env.runtime);
        assert_eq!(
            (&s["runs"], &s["published"], &s["failures"], &s["drills"]),
            (&json!(1), &json!(1), &json!(0), &json!(1))
        );
        assert_eq!(s["last_set"], id);
        assert!(staging_empty(&env.runtime));

        let listing = env.runtime.list().unwrap();
        assert_eq!(listing["installation_id"], installation);
        let listed = set(&listing, &id);
        assert_eq!(listed["record"]["state"], "published");
        assert_eq!(listed["record"]["installation_id"], installation);
        assert_eq!(listed["record"]["pins"], json!([]), "the drill pin is gone");
        assert_eq!(listed["record"]["last_drill"]["ok"], true);
        // Hashed database and bundle objects plus the manifest; the record,
        // pins and owner live in the coordination container.
        let data = env.data.names();
        assert_eq!(data.iter().filter(|n| n.contains("/objects/")).count(), 2);
        assert!(data.iter().any(|n| n.ends_with("/manifest.json")));
        assert!(!data.iter().any(|n| n.ends_with("record.json")));
        assert!(env.meta.names().iter().any(|n| n.ends_with("owner.json")));

        // Policy, identity and status survive a restart.
        let reopened = reopen(&env);
        let view = reopened.view().unwrap();
        assert_eq!(view["installation_id"], installation);
        assert_eq!(view["status"]["published"], 1);
        assert_eq!(view["policy"]["drill_every"], 1);
    }

    #[test]
    fn unsupported_sources_and_foreign_prefixes_are_refused() {
        let env = setup();
        let h = Handle::new(&env.service);
        let mut encrypted = h.clone();
        encrypted.encrypted = true;
        assert_eq!(
            code(env.runtime.configure(policy(json!({})), &encrypted, BASE)),
            "encrypted_cloud_backup_unqualified"
        );
        assert_eq!(env.runtime.view().unwrap()["configured"], false);
        assert_eq!(
            code(
                env.runtime
                    .configure(policy(json!({"keep_sets":0})), &h, BASE)
            ),
            "invalid_input"
        );
        env.runtime
            .configure(policy(json!({"drill_every":0})), &h, BASE)
            .unwrap();
        env.runtime.run_now(&h).unwrap();
        let objects = env.data.names().len();

        assert_eq!(
            code(env.runtime.run_now(&encrypted)),
            "encrypted_cloud_backup_unqualified"
        );
        let memory =
            Service::new(Database::create_in_memory(3, crate::CreateOptions::default()).unwrap());
        assert_eq!(
            code(env.runtime.run_now(&Handle::new(&memory))),
            "cloud_backup_requires_database_file"
        );
        let s = status(&env.runtime);
        assert_eq!((&s["published"], &s["failures"]), (&json!(1), &json!(2)));
        assert_eq!(
            s["last_error"]["code"],
            "cloud_backup_requires_database_file"
        );

        // A second installation (for example a restored copy) may not write
        // into the prefix of the first.
        let other = Runtime::open_with(
            env.root.join("other-cloud-backup"),
            connector(&env.meta, &env.data),
        )
        .unwrap();
        other.configure(policy(json!({})), &h, BASE).unwrap();
        assert_eq!(code(other.run_now(&h)), "cloud_prefix_owned");
        assert_eq!(code(other.collect(&h)), "cloud_prefix_owned");
        assert_eq!(env.data.names().len(), objects, "refusals write nothing");
    }

    #[test]
    fn one_operation_at_a_time_and_due_claims() {
        let env = setup();
        let h = Handle::new(&env.service);
        env.runtime
            .configure(policy(json!({"interval_secs":300})), &h, BASE)
            .unwrap();
        assert!(!env.runtime.claim_due_at(BASE + 10_000_000), "disabled");
        env.runtime
            .configure(
                policy(json!({"enabled":true,"interval_secs":300})),
                &h,
                BASE,
            )
            .unwrap();
        assert_eq!(
            env.runtime.view().unwrap()["next_run_unix_ms"],
            BASE + 300_000
        );
        assert!(!env.runtime.claim_due_at(BASE + 299_999));
        assert!(env.runtime.claim_due_at(BASE + 300_000));
        // The claim excludes every writing operation.
        assert_eq!(code(env.runtime.run_now(&h)), "cloud_backup_busy");
        assert_eq!(code(env.runtime.collect(&h)), "cloud_backup_busy");
        assert_eq!(
            code(env.runtime.configure(policy(json!({})), &h, BASE)),
            "cloud_backup_busy"
        );
        // A due run that finds the claim held retries shortly.
        assert!(!env.runtime.claim_due_at(BASE + 600_000));
        let view = env.runtime.view().unwrap();
        assert_eq!(view["running"], true);
        assert_eq!(view["status"]["skipped_busy"], 1);
        assert_eq!(view["next_run_unix_ms"], BASE + 600_000 + RETRY_MS);
        env.runtime.busy.store(false, Ordering::Release);
        assert!(!env.runtime.claim_due_at(BASE + 600_000 + RETRY_MS - 1));
        assert!(env.runtime.claim_due_at(BASE + 600_000 + RETRY_MS));
    }

    #[test]
    fn an_unknown_publication_is_settled_before_the_next_one() {
        let env = setup();
        let h = Handle::new(&env.service);
        env.runtime
            .configure(policy(json!({"drill_every":0})), &h, BASE)
            .unwrap();
        env.meta.fault(Op::Put, "record.json", Fault::Before, 1, 1);
        env.meta.fault(Op::Get, "record.json", Fault::Before, 0, 1);
        assert_eq!(code(env.runtime.run_now(&h)), "cloud_publication_unknown");
        let s = status(&env.runtime);
        let pending = s["pending"]["set_id"].as_str().unwrap().to_owned();
        assert_eq!((&s["published"], &s["failures"]), (&json!(0), &json!(1)));
        assert!(staging_empty(&env.runtime));

        // The handle is durable: a restarted process settles it first.
        let reopened = reopen(&env);
        assert_eq!(status(&reopened)["pending"]["set_id"], pending);
        let report = reopened.run_now(&h).unwrap();
        assert_eq!(
            report["settled"],
            json!({"set_id":pending,"outcome":"published"})
        );
        assert_ne!(report["set_id"], pending);
        let s = status(&reopened);
        assert_eq!(s["published"], 2);
        assert!(s["pending"].is_null());
        let listing = reopened.list().unwrap();
        assert_eq!(set(&listing, &pending)["record"]["state"], "published");
    }

    #[test]
    fn a_new_target_abandons_the_pending_publication() {
        let env = setup();
        let h = Handle::new(&env.service);
        env.runtime
            .configure(policy(json!({"drill_every":0})), &h, BASE)
            .unwrap();
        env.meta.fault(Op::Put, "record.json", Fault::Before, 1, 1);
        env.meta.fault(Op::Get, "record.json", Fault::Before, 0, 1);
        assert_eq!(code(env.runtime.run_now(&h)), "cloud_publication_unknown");
        // Schedule-only edits keep it.
        env.runtime
            .configure(policy(json!({"drill_every":0,"keep_sets":5})), &h, BASE)
            .unwrap();
        assert!(status(&env.runtime)["pending"].is_object());
        let mut moved = policy(json!({"drill_every":0}));
        moved.target.prefix = "vlite/next/".into();
        env.runtime.configure(moved, &h, BASE).unwrap();
        assert!(status(&env.runtime)["pending"].is_null());
        let report = env.runtime.run_now(&h).unwrap();
        assert!(report["settled"].is_null());
        assert_eq!(status(&env.runtime)["published"], 1);
        let id = report["set_id"].as_str().unwrap();
        assert!(
            env.meta
                .names()
                .iter()
                .any(|n| n.starts_with("vlite/next/sets/") && n.contains(id))
        );
    }

    #[test]
    fn restore_writes_a_new_lineage_copy_and_releases_its_pin() {
        let env = setup();
        let h = Handle::new(&env.service);
        env.runtime
            .configure(policy(json!({"drill_every":0})), &h, BASE)
            .unwrap();
        let id = env.runtime.run_now(&h).unwrap()["set_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let admin_dir = env.root.join("admin");
        std::fs::create_dir(&admin_dir).unwrap();
        let report = env.runtime.restore(&h, &id, &admin_dir).unwrap();
        let name = report["name"].as_str().unwrap();
        let path = admin::safe_file(&admin_dir, name, "restore-").unwrap();
        assert_eq!(report["rekey_only"], true);
        assert_eq!(report["pin_released"], true);
        assert_eq!(report["restart_required"], true);
        assert_eq!(report["inspection"]["live_records"], 4);
        assert_eq!(report["bundle"]["rekey_only"], true);
        assert_eq!(report["source_installation_id"], status_installation(&env));
        assert!(report["source_lineage"].is_string(), "{report}");
        assert!(report["lineage"].is_string(), "{report}");
        assert_ne!(report["lineage"], report["source_lineage"]);
        let restored = Database::open(&path).unwrap();
        assert_eq!(restored.len(), 4);
        drop(restored);
        let listing = env.runtime.list().unwrap();
        assert_eq!(set(&listing, &id)["record"]["pins"], json!([]));

        // Nothing is written for a set that cannot be pinned, nor for an
        // encrypted server.
        let missing = cloud::new_set_id(BASE).unwrap();
        assert_eq!(
            code(env.runtime.restore(&h, &missing, &admin_dir)),
            "cloud_set_not_found"
        );
        let mut encrypted = h.clone();
        encrypted.encrypted = true;
        assert_eq!(
            code(env.runtime.restore(&encrypted, &id, &admin_dir)),
            "encrypted_cloud_backup_unqualified"
        );
        assert_eq!(std::fs::read_dir(&admin_dir).unwrap().count(), 1);
    }

    fn status_installation(env: &Env) -> Value {
        env.runtime.view().unwrap()["installation_id"].clone()
    }

    #[test]
    fn retention_keeps_newest_and_pinned_sets() {
        let env = setup();
        let h = Handle::new(&env.service);
        env.runtime
            .configure(
                policy(
                    json!({"drill_every":0,"retention_enabled":true,"keep_sets":1,
                    "keep_days":0}),
                ),
                &h,
                BASE,
            )
            .unwrap();
        let mut ids = Vec::new();
        for _ in 0..3 {
            std::thread::sleep(Duration::from_millis(5));
            let report = env.runtime.run_now(&h).unwrap();
            assert_eq!(report["collection"]["ok"], true, "{report}");
            ids.push(report["set_id"].as_str().unwrap().to_owned());
            if ids.len() == 1 {
                let pinned = env.runtime.pin(&h, &ids[0], "legal hold").unwrap();
                assert_eq!(pinned["pin"]["reason"], "legal hold");
            }
        }
        let listing = env.runtime.list().unwrap();
        let active: Vec<&str> = listing["sets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["set_id"].as_str().unwrap())
            .collect();
        assert_eq!(active, vec![ids[2].as_str(), ids[0].as_str()]);
        assert_eq!(listing["tombstoned"][0]["set_id"], ids[1]);
        assert_eq!(listing["tombstoned"][0]["data_objects"], 0);

        // Unpinning makes the old set collectable by the next collection.
        let pin = set(&listing, &ids[0])["record"]["pins"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        env.runtime.unpin(&h, &ids[0], &pin).unwrap();
        let collected = env.runtime.collect(&h).unwrap();
        assert_eq!(collected["result"]["deleted"], json!([ids[0]]));
        assert_eq!(
            status(&env.runtime)["last_collection"]["result"]["deleted"],
            json!([ids[0]])
        );
    }

    #[test]
    fn the_maintenance_worker_starts_a_due_cloud_run() {
        let dir = tempfile::tempdir().unwrap();
        let cloud = dir.path().join("cloud-backup");
        std::fs::create_dir_all(&cloud).unwrap();
        let enabled = policy(json!({"enabled":true,"interval_secs":300,"drill_every":0}));
        std::fs::write(cloud.join(POLICY), serde_json::to_vec(&enabled).unwrap()).unwrap();
        std::fs::write(cloud.join(INSTALLATION), b"0123456789abcdef").unwrap();
        // Configured long ago: the first run is due at startup.
        std::fs::write(cloud.join(STATE), br#"{"policy_updated_unix_ms":1}"#).unwrap();
        let (meta, data) = (Arc::new(Memory::default()), Arc::new(Memory::default()));
        let runtime = Arc::new(Runtime::open_with(cloud, connector(&meta, &data)).unwrap());
        let service = service(dir.path());
        service
            .ops
            .configure_cloud(runtime.clone(), Handle::new(&service));
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let view = runtime.view().unwrap();
            if view["status"]["published"] == 1 && view["running"] == false {
                break;
            }
            assert!(Instant::now() < deadline, "no scheduled run: {view}");
            std::thread::sleep(Duration::from_millis(50));
        }
        service.ops.shutdown();
        let view = runtime.view().unwrap();
        let attempt = view["status"]["last_attempt_unix_ms"].as_u64().unwrap();
        assert_eq!(view["next_run_unix_ms"], attempt + 300_000);
        assert_eq!(view["status"]["runs"], 1);
    }

    async fn call(
        app: &Router,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let request = Request::builder().method(method).uri(uri);
        let request = match body {
            Some(body) => request
                .header("content-type", "application/json")
                .body(Body::from(body.to_string())),
            None => request.body(Body::empty()),
        };
        let response = app.clone().oneshot(request.unwrap()).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    #[tokio::test]
    async fn routes_configure_run_list_pin_and_refuse() {
        let env = setup();
        let mut service = env.service.clone();
        let app = routes().with_state(service.clone());
        let (status, body) = call(&app, "GET", "/v1/admin/cloud-backup", None).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["code"], "admin_directory_disabled");

        let mut state = admin::AdminState::default();
        state.directory = Some(env.root.join("admin"));
        std::fs::create_dir(env.root.join("admin")).unwrap();
        let _ = state.cloud.set(env.runtime.clone());
        service.admin = Arc::new(state);
        let app = routes().with_state(service);

        let (status, body) = call(&app, "GET", "/v1/admin/cloud-backup", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["configured"], false);
        let (status, _) = call(&app, "POST", "/v1/admin/cloud-backup/run", None).await;
        assert_eq!(status, StatusCode::CONFLICT);
        let base = "/v1/admin/cloud-backup";
        let mut invalid = json!({ "target": target(), "interval_secs": 1 });
        let (status, body) = call(&app, "PUT", base, Some(invalid.clone())).await;
        assert_eq!(
            (status, body["error"]["code"].as_str()),
            (StatusCode::UNPROCESSABLE_ENTITY, Some("invalid_input"))
        );
        invalid = json!({ "target": target(), "account_key": "secret" });
        let (status, _) = call(&app, "PUT", base, Some(invalid)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        let (status, body) = call(
            &app,
            "PUT",
            base,
            Some(json!({"target":target(),"drill_every":0})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["policy"]["enabled"], false);
        let (status, body) = call(&app, "POST", &format!("{base}/run"), None).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let id = body["set_id"].as_str().unwrap().to_owned();
        let (status, body) = call(&app, "GET", &format!("{base}/sets"), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["sets"][0]["set_id"], id);

        let sets = format!("{base}/sets/{id}");
        let (status, body) = call(
            &app,
            "POST",
            &format!("{sets}/pins"),
            Some(json!({"reason":"audit"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let pin = body["pin"]["id"].as_str().unwrap().to_owned();
        let (status, body) = call(&app, "DELETE", &format!("{sets}/pins/{pin}"), None).await;
        assert_eq!((status, &body["removed"]), (StatusCode::OK, &json!(true)));
        let (status, body) = call(
            &app,
            "POST",
            &format!("{sets}/pins"),
            Some(json!({"reason":"bad\nreason"})),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

        let (status, body) = call(
            &app,
            "POST",
            &format!("{sets}/restore"),
            Some(json!({"mode":"seed"})),
        )
        .await;
        assert_eq!(
            (status, body["error"]["code"].as_str()),
            (StatusCode::CONFLICT, Some("cloud_seed_mode_deferred"))
        );
        let (status, _) = call(
            &app,
            "POST",
            &format!("{sets}/restore"),
            Some(json!({"mode":"other"})),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        let (status, body) = call(&app, "POST", &format!("{sets}/drill"), None).await;
        assert_eq!(
            (status, &body["ok"]),
            (StatusCode::OK, &json!(true)),
            "{body}"
        );
        let (status, body) = call(&app, "POST", &format!("{sets}/restore"), None).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert!(body["name"].as_str().unwrap().starts_with("restore-"));

        for uri in [
            format!("{base}/sets/not-a-set/drill"),
            format!("{base}/sets/not-a-set/restore"),
            format!("{sets}/pins/not-a-pin"),
        ] {
            let method = if uri.contains("/pins/") {
                "DELETE"
            } else {
                "POST"
            };
            let (status, body) = call(&app, method, &uri, None).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{uri}: {body}");
        }
    }
}
