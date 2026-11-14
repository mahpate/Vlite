//! Persisted, opt-in Azure Blob backup policy, its scheduler and the admin
//! routes (feature-backup-ha ITEM-006/007).
//!
//! State lives under `<admin>/cloud-backup/`:
//!
//! - `state.json`: one versioned document with the operator policy (target,
//!   schedule, retention, drill cadence) and the run status, including the
//!   handle of a publication whose final result was unknown (resolved by the
//!   next run before anything else). Policy and status are replaced together
//!   by one atomic rename, so a crash never pairs a new policy with an older
//!   status. It holds no secret: managed identity needs none and the
//!   emulator key is read per process from `VLITE_CLOUD_BACKUP_SHARED_KEY_FILE`.
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
use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path as FilePath, PathBuf};
use std::sync::Weak;
use std::sync::atomic::AtomicBool;

const STATE: &str = "state.json";
const FORMAT: u32 = 1;
const INSTALLATION: &str = "installation-id";
const STAGING: &str = "staging";
const MAX_FILE: u64 = 64 * 1024;
/// IDs kept per list in the persisted collection summary; the HTTP response
/// and the audit log carry the complete lists.
const SUMMARY_SAMPLE: usize = 20;
/// Deleted set IDs per audit event.
const AUDIT_CHUNK: usize = 500;
/// Characters kept from an error message in the persisted status.
const MAX_MESSAGE: usize = 500;
/// Retention floors (PRD sec. 3.3): a policy can keep more, never fewer. The
/// newest verified set is kept on top of these by the collection itself.
const MIN_KEEP_SETS: usize = 3;
const MIN_KEEP_DAYS: u64 = 30;
/// Delay before a scheduled run skipped by a busy queue or operation retries.
const RETRY_MS: u64 = 30_000;
/// Process-level emulator key file; never part of the persisted policy.
pub(super) const SHARED_KEY_ENV: &str = "VLITE_CLOUD_BACKUP_SHARED_KEY_FILE";

fn default_interval() -> u64 {
    3600
}
fn default_keep_sets() -> usize {
    MIN_KEEP_SETS
}
fn default_keep_days() -> u64 {
    MIN_KEEP_DAYS
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
        if !(MIN_KEEP_SETS..=1000).contains(&self.keep_sets) {
            return Err(format!("keep_sets must be {MIN_KEEP_SETS}-1000"));
        }
        if !(MIN_KEEP_DAYS..=3650).contains(&self.keep_days) {
            return Err(format!("keep_days must be {MIN_KEEP_DAYS}-3650"));
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
    /// The run in progress, saved before any remote work. One still present
    /// at startup, or left by a panic, is recorded as interrupted.
    pub job: Option<Job>,
    /// Runs that stopped without finishing (panic or process exit).
    pub interrupted: u64,
    /// When schedule times found in the future were pulled back to the
    /// clock (a clock that ran ahead and was corrected).
    pub clock_reset_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Job {
    pub id: String,
    pub scheduled: bool,
    pub started_unix_ms: u64,
    /// `publish` until the new set is published, then `after_publish`
    /// (drill and collection).
    pub phase: String,
}

const PHASE_PUBLISH: &str = "publish";
const PHASE_AFTER_PUBLISH: &str = "after_publish";

/// Records a job that never finished. Only an interrupted publication counts
/// as a failed run; after publication the set exists, so only the drill or
/// collection was cut short.
fn interrupt(status: &mut Status, id: Option<&str>, at: u64) -> Option<Job> {
    let job = status.job.take_if(|job| id.is_none_or(|id| job.id == id))?;
    if job.phase == PHASE_PUBLISH {
        status.runs += 1;
        status.failures += 1;
        status.last_failure_unix_ms = Some(at);
    }
    status.interrupted += 1;
    status.last_error = Some(LastError {
        code: "cloud_backup_interrupted".into(),
        message: format!(
            "cloud backup job {} stopped during its {} phase without finishing",
            job.id, job.phase
        ),
    });
    Some(job)
}

/// The durable document (`state.json`).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    format: u32,
    policy: Option<Policy>,
    #[serde(default)]
    status: Status,
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
    /// Operation pins (restores, drills) this process is still using: even a
    /// forced removal refuses them.
    live: Mutex<HashSet<String>>,
    /// The thread of the last scheduled run, waited for at shutdown.
    operation: Mutex<Option<std::thread::JoinHandle<()>>>,
    connector: Connector,
}

/// Finalizes a job whose cycle panicked; a normal return finalizes itself.
struct Unwinding<'a> {
    runtime: &'a Runtime,
    handle: &'a Handle,
    id: &'a str,
}

impl Drop for Unwinding<'_> {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        let at = now();
        let mut phase = None;
        self.runtime.note(|l| {
            phase = interrupt(&mut l.status, Some(self.id), at).map(|job| job.phase);
        });
        self.handle
            .audit(json!({"event":"cloud_backup_interrupted","job_id":self.id,"phase":phase}));
        logs::stderr(
            "error",
            "cloud_backup",
            json!({"event":"cloud_backup_interrupted","job_id":self.id,"phase":phase}).to_string(),
        );
    }
}

/// Releases the single-operation flag.
struct Busy<'a>(&'a AtomicBool);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// An operation pin held by this process; it leaves the live set on drop,
/// after its owner released (or failed to release) it.
struct Held<'a> {
    live: &'a Mutex<HashSet<String>>,
    pin: cloud::Pin,
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.pin.id);
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

fn state_too_large() -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "cloud_backup_state_too_large",
        "the cloud backup status exceeds its size limit and was not saved",
    )
}

/// Encodes the durable document; one that the next start could not read is
/// refused before anything is written.
fn encode(policy: Option<&Policy>, status: &Status) -> Result<Vec<u8>, ApiError> {
    let document = json!({"format":FORMAT,"policy":policy,"status":status});
    let bytes = serde_json::to_vec_pretty(&document).map_err(internal)?;
    if bytes.len() as u64 > MAX_FILE {
        return Err(state_too_large());
    }
    Ok(bytes)
}

fn read_document(dir: &FilePath) -> std::io::Result<Option<Document>> {
    let Some(document) = read_json::<Document>(&dir.join(STATE))? else {
        return Ok(None);
    };
    if document.format != FORMAT {
        return Err(io_error(format!(
            "unsupported cloud backup state format {}",
            document.format
        )));
    }
    if let Some(policy) = &document.policy {
        policy
            .validate()
            .map_err(|e| io_error(format!("invalid cloud backup policy: {e}")))?;
    }
    Ok(Some(document))
}

fn valid_installation(id: &str) -> bool {
    id.len() == 16
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
thread_local! {
    /// Test fault: fail the next write before (`false`) or after (`true`)
    /// its rename.
    static WRITE_FAULT: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

fn write_atomic(dir: &FilePath, name: &str, bytes: &[u8]) -> Result<(), ApiError> {
    #[cfg(test)]
    let fault = WRITE_FAULT.with(|f| f.take());
    #[cfg(test)]
    if fault == Some(false) {
        return Err(internal("injected write fault"));
    }
    let mut file = tempfile::NamedTempFile::new_in(dir).map_err(internal)?;
    file.write_all(bytes).map_err(internal)?;
    file.as_file().sync_all().map_err(internal)?;
    file.persist(dir.join(name)).map_err(internal)?;
    #[cfg(test)]
    if fault == Some(true) {
        return Err(internal("injected write fault"));
    }
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

pub(super) fn inspection(i: &maintenance::Inspection) -> Value {
    json!({"format_version":i.format_version,"dimensions":i.dimensions,"live_records":i.live_records,
        "commits":i.commits,"file_bytes":i.file_bytes,"valid_bytes":i.valid_bytes,
        "incomplete_tail_bytes":i.incomplete_tail_bytes})
}

pub(super) fn lineage_hex(lineage: Option<u64>) -> Option<String> {
    lineage.map(|l| format!("{l:016x}"))
}

/// A message bounded for the persisted status.
fn clip(message: &str) -> String {
    message.chars().take(MAX_MESSAGE).collect()
}

/// The persisted form of a collection: counts and a bounded sample of IDs.
fn summarize(collection: &cloud::Collection) -> Value {
    let list = |ids: &[String]| {
        json!({"count":ids.len(),"sample":&ids[..ids.len().min(SUMMARY_SAMPLE)],
            "truncated":ids.len() > SUMMARY_SAMPLE})
    };
    let failures = &collection.failures;
    let sample: Vec<Value> = failures
        .iter()
        .take(SUMMARY_SAMPLE)
        .map(|f| json!({"set_id":f.set_id,"code":f.code,"message":clip(&f.message)}))
        .collect();
    json!({"decision":collection.decision,"deleted":list(&collection.deleted),
        "orphans_deleted":list(&collection.orphans_deleted),"resumed":list(&collection.resumed),
        "skipped":list(&collection.skipped),"swept":list(&collection.swept),
        "unreadable":list(&collection.unreadable),
        "failures":{"count":failures.len(),"sample":sample,
            "truncated":failures.len() > SUMMARY_SAMPLE}})
}

/// What the operator must re-provision; the bundle never carries secrets.
pub(super) fn bundle_summary(bundle: &recovery_bundle::Bundle) -> Value {
    let c = &bundle.config;
    json!({"schema":bundle.schema,"created_unix_ms":bundle.created_unix_ms,
        "installation_id":bundle.installation_id,"source_lineage":bundle.source_lineage,
        "rekey_only":bundle.rekey_only,"tenants":c.tenants.len(),"accounts":c.accounts.len(),
        "credentials":c.credentials.len(),"webhook_subscriptions":c.webhook_subscriptions,
        "secret_references":c.secret_references,"excluded":c.excluded})
}

/// Pulls schedule times that lie in the future back to `now`. A clock that
/// ran ahead and was then corrected would otherwise defer every run by the
/// size of the error; this caps the delay at one interval (or one retry).
fn reconcile_clock(loaded: &mut Loaded, now: u64) -> bool {
    let status = &mut loaded.status;
    let mut changed = false;
    for stamp in [
        &mut status.last_attempt_unix_ms,
        &mut status.policy_updated_unix_ms,
    ] {
        if stamp.is_some_and(|at| at > now) {
            *stamp = Some(now);
            changed = true;
        }
    }
    let retry = now.saturating_add(RETRY_MS);
    if loaded.retry_at.is_some_and(|at| at > retry) {
        loaded.retry_at = Some(retry);
        changed = true;
    }
    if changed {
        status.clock_reset_unix_ms = Some(now);
        logs::stderr(
            "warn",
            "cloud_backup",
            json!({"event":"cloud_backup_clock_reconciled","now_unix_ms":now}).to_string(),
        );
    }
    changed
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
        cloud::create_private_subdir(&staging)?;
        let (policy, mut status) = match read_document(&dir)? {
            Some(document) => (document.policy, document.status),
            None => (None, Status::default()),
        };
        // A job saved by a process that stopped mid-run: record it once.
        if let Some(job) = interrupt(&mut status, None, now()) {
            let bytes = encode(policy.as_ref(), &status).map_err(|e| io_error(e.message))?;
            write_atomic(&dir, STATE, &bytes).map_err(|e| io_error(e.message))?;
            logs::stderr(
                "warn",
                "cloud_backup",
                json!({"event":"cloud_backup_interrupted","job_id":job.id,"phase":job.phase,
                    "detected":"startup"})
                .to_string(),
            );
        }
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
        let mut loaded = Loaded {
            policy,
            status,
            installation,
            retry_at: None,
        };
        if reconcile_clock(&mut loaded, now()) {
            let bytes =
                encode(loaded.policy.as_ref(), &loaded.status).map_err(|e| io_error(e.message))?;
            write_atomic(&dir, STATE, &bytes).map_err(|e| io_error(e.message))?;
        }
        Ok(Self {
            dir,
            state: Mutex::new(loaded),
            busy: AtomicBool::new(false),
            live: Mutex::default(),
            operation: Mutex::default(),
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

    /// Pins a set for a restore or drill run by this process.
    fn hold(&self, sets: &Sets, set_id: &str, reason: &str) -> Result<Held<'_>, ApiError> {
        let pin = sets.pin(set_id, reason, cloud::PinKind::Operation, now())?;
        self.live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(pin.id.clone());
        Ok(Held {
            live: &self.live,
            pin,
        })
    }

    fn workdir(&self) -> Result<tempfile::TempDir, ApiError> {
        cloud::private_dir(&self.staging(), "fetch-").map_err(internal)
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
    /// so status writes never reorder. A status that would exceed the read
    /// limit is refused and rolled back before the durable file is replaced:
    /// an unreadable `state.json` would stop the server from starting.
    fn update(&self, change: impl FnOnce(&mut Loaded)) -> Result<(), ApiError> {
        let mut loaded = lock(&self.state)?;
        let previous = loaded.status.clone();
        change(&mut loaded);
        let bytes = match encode(loaded.policy.as_ref(), &loaded.status) {
            Ok(bytes) => bytes,
            Err(error) => {
                loaded.status = previous;
                return Err(error);
            }
        };
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
    /// publication of the old target (it stays an orphan there). Policy and
    /// status are one document: a failed write whose rename may or may not
    /// have happened adopts what the file holds, so memory always matches
    /// what the next start reads, and the caller is told to check.
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
            let bytes = encode(Some(&policy), &status)?;
            if let Err(error) = write_atomic(&self.dir, STATE, &bytes) {
                if let Ok(document) = read_document(&self.dir) {
                    let (policy, status) =
                        document.map_or((None, Status::default()), |d| (d.policy, d.status));
                    loaded.policy = policy;
                    loaded.status = status;
                    loaded.retry_at = None;
                }
                logs::stderr(
                    "error",
                    "cloud_backup",
                    json!({"event":"cloud_backup_policy_write_failed","code":error.code})
                        .to_string(),
                );
                return Err(ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "cloud_backup_policy_uncertain",
                    "the policy change could not be confirmed on disk; GET /v1/admin/cloud-backup shows the policy in effect",
                ));
            }
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
        // Saved with the next status write; a restart reconciles again.
        reconcile_clock(&mut loaded, now);
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
    /// claim. `Service::close` waits a bounded time for it
    /// (`wait_for_operation`); a run still going then is recorded as
    /// interrupted at the next start, and leaves an unpublished set that
    /// collection removes, or a pending handle.
    pub(super) fn start_claimed(self: &Arc<Self>, handle: Handle) {
        let runtime = self.clone();
        let spawned = std::thread::Builder::new()
            .name("vlite-cloud-backup".into())
            .spawn(move || {
                let _claim = Busy(&runtime.busy);
                let _ = runtime.cycle(&handle, true);
            });
        if let Ok(thread) = spawned {
            *self
                .operation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(thread);
        } else {
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

    /// Waits up to `wait` for the last scheduled run's thread. Returns false
    /// (and logs) if it is still running; that job is then recorded as
    /// interrupted at the next start.
    pub(super) fn wait_for_operation(&self, wait: std::time::Duration) -> bool {
        let slot = || {
            self.operation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        let Some(thread) = slot().take() else {
            return true;
        };
        let deadline = std::time::Instant::now() + wait;
        while !thread.is_finished() {
            if std::time::Instant::now() >= deadline {
                slot().get_or_insert(thread);
                logs::stderr(
                    "warn",
                    "cloud_backup",
                    json!({"event":"cloud_backup_shutdown_timeout",
                        "waited_ms":wait.as_millis() as u64})
                    .to_string(),
                );
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // A panic was already recorded by the cycle's unwinding guard.
        let _ = thread.join();
        true
    }

    /// An operator-requested run; refused while another one runs.
    pub(super) fn run_now(&self, handle: &Handle) -> Result<Value, ApiError> {
        let _busy = self.try_busy()?;
        self.cycle(handle, false)
    }

    /// Saves the job before any remote work, runs it, then clears it. A
    /// panic is recorded by the unwinding guard; a process that stops
    /// mid-run leaves the job for the next start to record.
    fn cycle(&self, handle: &Handle, scheduled: bool) -> Result<Value, ApiError> {
        let started = now();
        let (policy, installation) = self.configured()?;
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).map_err(internal)?;
        let id = hex(&bytes);
        let job = Job {
            id: id.clone(),
            scheduled,
            started_unix_ms: started,
            phase: PHASE_PUBLISH.into(),
        };
        let mut pending = None;
        let admitted = self.update(|l| {
            l.status.last_attempt_unix_ms = Some(started);
            l.status.job = Some(job);
            pending = l.status.pending.clone();
        });
        if let Err(error) = admitted {
            // Not durably admitted: nothing remote was attempted.
            if let Ok(mut loaded) = self.state.lock() {
                loaded.status.job.take_if(|job| job.id == id);
            }
            return Err(error);
        }
        let guard = Unwinding {
            runtime: self,
            handle,
            id: &id,
        };
        let result = self.run_job(handle, scheduled, &policy, &installation, pending, started);
        drop(guard);
        self.note(|l| {
            l.status.job.take_if(|job| job.id == id);
        });
        result
    }

    /// Settles a pending publication, captures a consistent database copy and
    /// recovery bundle, publishes the set, then drills and collects as the
    /// policy asks. Every outcome is recorded in the status and audited.
    fn run_job(
        &self,
        handle: &Handle,
        scheduled: bool,
        policy: &Policy,
        installation: &str,
        pending: Option<cloud::Pending>,
        started: u64,
    ) -> Result<Value, ApiError> {
        let outcome = self.publish(handle, policy, installation, pending, started);
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
                        message: clip(&error.message),
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
            if let Some(job) = &mut l.status.job {
                job.phase = PHASE_AFTER_PUBLISH.into();
            }
            published = l.status.published;
        });
        handle.audit(
            json!({"event":"cloud_backup_published","set_id":record.set_id,
            "scheduled":scheduled}),
        );
        let drill = (policy.drill_every > 0 && published % policy.drill_every == 0).then(|| {
            match self.hold(&sets, &record.set_id, "drill") {
                Ok(held) => {
                    self.drill_pinned(handle, &sets, &record.set_id, &held.pin)
                        .0
                }
                Err(error) => self.drilled(handle, &record.set_id, Err(error), false).0,
            }
        });
        let collection = policy
            .retention_enabled
            .then(|| self.collect_with(handle, &sets, policy, installation).0);
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
            cloud::check_database_size(db.file.0.metadata().map_err(internal)?.len())?;
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
            Err(error) => {
                logs::stderr(
                    "warn",
                    "cloud_backup",
                    json!({"event":"cloud_drill_unrecorded","set_id":set_id,
                        "code":error.code,"message":error.message})
                    .to_string(),
                );
                // Best effort: a pin left behind only delays retention.
                let _ = sets.release(set_id, &pin.id, now());
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
        let mut summary = report.clone();
        if let Some(message) = summary["error"]["message"].as_str() {
            summary["error"]["message"] = json!(clip(message));
        }
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
        let (report, summary, error) = match sets.collect(installation, &policy.retention(), at) {
            Ok(collection) => {
                for chunk in collection.deleted.chunks(AUDIT_CHUNK) {
                    handle.audit(json!({"event":"cloud_backup_sets_deleted","set_ids":chunk}));
                }
                (
                    json!({"unix_ms":at,"ok":true,
                    "retention_enabled":policy.retention_enabled,"result":collection}),
                    json!({"unix_ms":at,"ok":true,
                    "retention_enabled":policy.retention_enabled,
                    "result":summarize(&collection)}),
                    None,
                )
            }
            Err(error) => {
                let report = json!({"unix_ms":at,"ok":false,
                    "error":{"code":error.code,"message":clip(&error.message)}});
                (report.clone(), report, Some(error))
            }
        };
        let deleted = summary["result"]["deleted"]["count"].clone();
        self.note(|l| l.status.last_collection = Some(summary));
        handle.audit(json!({"event":"cloud_backup_collected","ok":report["ok"],
            "deleted":deleted}));
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
        let held = self.hold(&sets, set_id, "drill")?;
        match self.drill_pinned(handle, &sets, set_id, &held.pin) {
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
        let pin = sets.pin(set_id, reason, cloud::PinKind::Operator, now())?;
        handle.audit(json!({"event":"cloud_backup_pinned","set_id":set_id,"pin_id":pin.id}));
        Ok(json!({"set_id":set_id,"pin":pin}))
    }

    /// Removes an operator hold. An operation pin needs `force` and is
    /// refused while this process still runs its restore or drill; a forced
    /// removal is for an owner that crashed and is audited separately.
    pub(super) fn unpin(
        &self,
        handle: &Handle,
        set_id: &str,
        pin_id: &str,
        force: bool,
    ) -> Result<Value, ApiError> {
        if force
            && self
                .live
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(pin_id)
        {
            return Err(conflict(
                "cloud_pin_in_use",
                "a restore or drill on this server still holds the pin; it is released when that operation ends",
            ));
        }
        let (sets, _, _) = self.connect()?;
        let kind = sets.unpin(set_id, pin_id, force, now())?;
        let forced = kind == Some(cloud::PinKind::Operation);
        let event = if forced {
            "cloud_backup_pin_force_released"
        } else {
            "cloud_backup_unpinned"
        };
        handle.audit(json!({"event":event,"set_id":set_id,"pin_id":pin_id}));
        Ok(json!({"set_id":set_id,"pin_id":pin_id,"removed":true,"forced":forced}))
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
        let held = self.hold(&sets, set_id, "restore")?;
        let result = (|| {
            let work = self.workdir()?;
            let fetched = sets.fetch(set_id, &held.pin.id, work.path())?;
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
        let released = sets.release(set_id, &held.pin.id, now()).is_ok();
        drop(held);
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

/// Parses a JSON body without echoing any of it: an unknown field name or
/// a misplaced value may be a pasted secret, so diagnostics are fixed text.
fn parsed<T>(input: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    input.map(|Json(value)| value).map_err(|error| {
        let status = error.status();
        let message = match status {
            StatusCode::UNSUPPORTED_MEDIA_TYPE => {
                "the request needs Content-Type: application/json"
            }
            StatusCode::PAYLOAD_TOO_LARGE => "the request body is too large",
            StatusCode::UNPROCESSABLE_ENTITY => {
                "the request body has an unknown field or a value of the wrong type; see the API reference for the accepted fields"
            }
            _ => "the request body is not valid JSON",
        };
        ApiError::new(status, "invalid_json", message)
    })
}

/// The restore body: empty, or `{"mode": "restore" | "seed"}`. Parse
/// failures never echo the input.
fn restore_input(bytes: &[u8]) -> Result<RestoreInput, ApiError> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(RestoreInput::default());
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_json",
            "the request body is not valid JSON",
        )
    })?;
    serde_json::from_value(value).map_err(|_| {
        unprocessable(r#"the request body may only hold "mode": "restore" (the default) or "seed""#)
    })
}

async fn status(State(s): State<Service>) -> Result<Json<Value>, ApiError> {
    Ok(Json(runtime(&s)?.view()?))
}

async fn configure(
    State(s): State<Service>,
    input: Result<Json<Policy>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let policy = parsed(input)?;
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
    let input = restore_input(&bytes)?;
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
    let input = parsed(input)?;
    set_id(&id)?;
    let runtime = runtime(&s)?;
    let handle = Handle::new(&s);
    let report = blocking(move || runtime.pin(&handle, &id, &input.reason)).await?;
    Ok((StatusCode::CREATED, Json(report)))
}

async fn unpin(
    State(s): State<Service>,
    Path((id, pin)): Path<(String, String)>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Result<Json<Value>, ApiError> {
    let force = match query.as_deref() {
        None | Some("") | Some("force=false") => false,
        Some("force=true") => true,
        Some(_) => return Err(unprocessable("the only query parameter is force=true")),
    };
    set_id(&id)?;
    if !cloud::valid_pin_id(&pin) {
        return Err(unprocessable("invalid pin id"));
    }
    let runtime = runtime(&s)?;
    let handle = Handle::new(&s);
    Ok(Json(
        blocking(move || runtime.unpin(&handle, &id, &pin, force)).await?,
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

    fn document(policy: &Policy, status: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({"format":FORMAT,"policy":policy,"status":status})).unwrap()
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
            json!({"keep_sets":2}),
            json!({"keep_sets":1001}),
            json!({"keep_days":0}),
            json!({"keep_days":29}),
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

        let valid = document(&policy(json!({"enabled":true})), json!({}));
        std::fs::write(cloud.join(STATE), &valid).unwrap();
        let error = open().expect("a policy without its installation id must fail");
        assert!(error.to_string().contains("installation-id"), "{error}");
        std::fs::write(cloud.join(INSTALLATION), b"0123456789abcdef").unwrap();
        assert!(open().is_none());

        let mut out_of_range = policy(json!({}));
        out_of_range.interval_secs = 1;
        std::fs::write(cloud.join(STATE), document(&out_of_range, json!({}))).unwrap();
        let error = open().expect("an out-of-range policy must fail");
        assert!(error.to_string().contains("interval_secs"), "{error}");
        for bad in [
            &b"{not json"[..],
            br#"{"format":2,"policy":null}"#,
            br#"{"policy":null}"#,
            br#"{"format":1,"policy":null,"extra":1}"#,
        ] {
            std::fs::write(cloud.join(STATE), bad).unwrap();
            assert!(open().is_some(), "{}", String::from_utf8_lossy(bad));
        }

        std::fs::write(cloud.join(STATE), &valid).unwrap();
        std::fs::write(cloud.join(INSTALLATION), b"../../etc/passwd").unwrap();
        assert!(open().is_some());
        std::fs::write(cloud.join(INSTALLATION), b"0123456789abcdef").unwrap();
        let wrong = document(&policy(json!({})), json!({"runs":"many"}));
        std::fs::write(cloud.join(STATE), wrong).unwrap();
        assert!(open().is_some());
    }

    fn job_file(env: &Env, job: Value) {
        let path = env.root.join("cloud-backup").join(STATE);
        let mut file: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        file["status"]["job"] = job;
        std::fs::write(path, serde_json::to_vec(&file).unwrap()).unwrap();
    }

    #[test]
    fn an_interrupted_job_is_recorded_once_at_startup() {
        // Review M5: a process that stopped mid-run left no trace of the run.
        let env = setup();
        let h = Handle::new(&env.service);
        env.runtime
            .configure(policy(json!({"drill_every":0})), &h, BASE)
            .unwrap();
        job_file(
            &env,
            json!({"id":"00000000000000aa","scheduled":true,"started_unix_ms":BASE,
                "phase":"publish"}),
        );
        let first = status(&reopen(&env));
        assert!(first["job"].is_null(), "{first}");
        assert_eq!(first["interrupted"], 1, "{first}");
        assert_eq!(
            (first["runs"].clone(), first["failures"].clone()),
            (json!(1), json!(1))
        );
        assert_eq!(first["last_error"]["code"], "cloud_backup_interrupted");
        // Recorded durably: a second start does not count it again.
        assert_eq!(status(&reopen(&env)), first);
        // After publication only the drill or collection was cut short.
        job_file(
            &env,
            json!({"id":"00000000000000bb","scheduled":false,"started_unix_ms":BASE,
                "phase":"after_publish"}),
        );
        let second = status(&reopen(&env));
        assert_eq!(second["interrupted"], 2, "{second}");
        assert_eq!(
            (second["runs"].clone(), second["failures"].clone()),
            (json!(1), json!(1))
        );
        job_file(&env, json!({"id":"x","phase":"publish","extra":1}));
        assert!(
            Runtime::open_with(
                env.root.join("cloud-backup"),
                connector(&env.meta, &env.data)
            )
            .is_err()
        );
    }

    #[test]
    fn a_panicking_run_is_recorded_and_the_scheduled_thread_is_joined() {
        let env = setup();
        let h = Handle::new(&env.service);
        let armed = Arc::new(AtomicBool::new(true));
        let gate = Arc::new(AtomicBool::new(true));
        let (meta, data) = (env.meta.clone(), env.data.clone());
        let (a, g) = (armed.clone(), gate.clone());
        let runtime = Arc::new(
            Runtime::open_with(
                env.root.join("cloud-backup"),
                Box::new(move |target: &Target| {
                    while !g.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    if a.swap(false, Ordering::SeqCst) {
                        panic!("injected connector panic");
                    }
                    Ok(Sets::new(
                        meta.clone(),
                        data.clone(),
                        false,
                        target.prefix.clone(),
                    ))
                }),
            )
            .unwrap(),
        );
        runtime
            .configure(policy(json!({"drill_every":0})), &h, BASE)
            .unwrap();
        // An operator run that panics is finalized and frees the runtime.
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| runtime.run_now(&h)));
        assert!(outcome.is_err());
        let after = status(&runtime);
        assert!(after["job"].is_null(), "{after}");
        assert_eq!(
            (after["interrupted"].clone(), after["failures"].clone()),
            (json!(1), json!(1))
        );
        assert_eq!(after["last_error"]["code"], "cloud_backup_interrupted");
        assert!(!runtime.busy.load(Ordering::Acquire));
        assert_eq!(status(&reopen(&env)), after, "recorded on disk, once");
        assert_eq!(runtime.run_now(&h).unwrap()["record"]["state"], "published");

        // A scheduled run that panics on its thread is joined at shutdown.
        armed.store(true, Ordering::SeqCst);
        runtime.busy.store(true, Ordering::Release);
        runtime.start_claimed(h.clone());
        assert!(runtime.wait_for_operation(Duration::from_secs(10)));
        assert_eq!(status(&runtime)["interrupted"], 2);
        assert!(!runtime.busy.load(Ordering::Acquire));

        // A run still going when the bounded wait ends is reported, its job
        // stays durable, and a later wait joins it.
        gate.store(false, Ordering::SeqCst);
        runtime.busy.store(true, Ordering::Release);
        runtime.start_claimed(h.clone());
        assert!(!runtime.wait_for_operation(Duration::from_millis(50)));
        let deadline = Instant::now() + Duration::from_secs(10);
        // Read the file: opening a runtime would record the job as interrupted.
        let state = env.root.join("cloud-backup").join(STATE);
        let saved =
            || -> Value { serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap() };
        while saved()["status"]["job"].is_null() {
            assert!(Instant::now() < deadline, "the job was never admitted");
            std::thread::sleep(Duration::from_millis(5));
        }
        gate.store(true, Ordering::SeqCst);
        assert!(runtime.wait_for_operation(Duration::from_secs(10)));
        let done = status(&runtime);
        assert!(done["job"].is_null(), "{done}");
        assert_eq!(
            (done["interrupted"].clone(), done["published"].clone()),
            (json!(2), json!(2))
        );
        assert!(runtime.wait_for_operation(Duration::ZERO));
    }

    #[test]
    fn a_corrected_clock_defers_runs_by_at_most_one_interval() {
        // Review M4: a clock that ran ahead and was corrected deferred every
        // run by the size of the error, even across a restart.
        let env = setup();
        let h = Handle::new(&env.service);
        let enabled = || policy(json!({"enabled":true,"interval_secs":300,"drill_every":0}));
        let year = 365 * 24 * 3_600_000;
        env.runtime.configure(enabled(), &h, BASE + year).unwrap();
        assert!(!env.runtime.claim_due_at(BASE));
        let view = env.runtime.view().unwrap();
        assert_eq!(view["next_run_unix_ms"], BASE + 300_000, "{view}");
        assert_eq!(view["status"]["clock_reset_unix_ms"], BASE);
        assert!(!env.runtime.claim_due_at(BASE + 299_999));
        assert!(env.runtime.claim_due_at(BASE + 300_000));
        env.runtime.busy.store(false, Ordering::Release);
        // A busy skip far in the future is capped at one retry.
        env.runtime.busy.store(true, Ordering::Release);
        assert!(!env.runtime.claim_due_at(BASE + year));
        env.runtime.busy.store(false, Ordering::Release);
        assert!(!env.runtime.claim_due_at(BASE + 600_000));
        assert!(env.runtime.claim_due_at(BASE + 600_000 + RETRY_MS));
        env.runtime.busy.store(false, Ordering::Release);

        // Persisted future times are pulled back at startup.
        env.runtime.configure(enabled(), &h, now() + year).unwrap();
        let before = now();
        let reopened = reopen(&env);
        let after = now();
        let view = reopened.view().unwrap();
        let updated = view["status"]["policy_updated_unix_ms"].as_u64().unwrap();
        assert!((before..=after).contains(&updated), "{view}");
        let next = view["next_run_unix_ms"].as_u64().unwrap();
        assert!(next <= after + 300_000, "{view}");
        let saved: Value = serde_json::from_slice(
            &std::fs::read(env.root.join("cloud-backup").join(STATE)).unwrap(),
        )
        .unwrap();
        assert_eq!(saved["status"]["policy_updated_unix_ms"], updated);
    }

    #[test]
    fn policy_and_status_are_one_atomic_document() {
        // Review M3: the policy and the status were two renames; a crash or a
        // failed write between them paired a new policy with an old pending
        // handle, and memory differed from what the next start reads.
        let env = setup();
        let h = Handle::new(&env.service);
        env.runtime
            .configure(policy(json!({"drill_every":0})), &h, BASE)
            .unwrap();
        env.meta.fault(Op::Put, "record.json", Fault::Before, 1, 1);
        env.meta.fault(Op::Get, "record.json", Fault::Before, 0, 1);
        assert_eq!(code(env.runtime.run_now(&h)), "cloud_publication_unknown");
        let dir = env.root.join("cloud-backup");
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(
            !names.iter().any(|n| n.ends_with(".json") && n != STATE),
            "{names:?}"
        );
        let file: Value = serde_json::from_slice(&std::fs::read(dir.join(STATE)).unwrap()).unwrap();
        assert_eq!(file["format"], FORMAT);
        assert_eq!(file["policy"]["target"]["prefix"], "vlite/prod/");
        assert!(file["status"]["pending"].is_object());

        let durable = |runtime: &Runtime| {
            let view = runtime.view().unwrap();
            (view["policy"].clone(), view["status"]["pending"].clone())
        };
        let mut moved = policy(json!({"drill_every":0}));
        moved.target.prefix = "vlite/next/".into();
        // The rename did not happen: the old policy and its handle stay.
        WRITE_FAULT.with(|f| f.set(Some(false)));
        assert_eq!(
            code(env.runtime.configure(moved.clone(), &h, BASE)),
            "cloud_backup_policy_uncertain"
        );
        let (kept, pending) = durable(&env.runtime);
        assert_eq!(kept["target"]["prefix"], "vlite/prod/");
        assert!(pending.is_object());
        assert_eq!(durable(&reopen(&env)), (kept, pending));
        // The rename happened but was not confirmed: the new policy (without
        // the old target's handle) is what memory and the next start see.
        WRITE_FAULT.with(|f| f.set(Some(true)));
        assert_eq!(
            code(env.runtime.configure(moved, &h, BASE)),
            "cloud_backup_policy_uncertain"
        );
        let (adopted, pending) = durable(&env.runtime);
        assert_eq!(adopted["target"]["prefix"], "vlite/next/");
        assert!(pending.is_null());
        assert_eq!(durable(&reopen(&env)), (adopted, pending));
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

    /// Moves a set's creation time into the past, beyond keep_days.
    fn age(env: &Env, id: &str, created: u64) {
        let name = format!("vlite/prod/sets/{id}/record.json");
        let mut record: cloud::Record =
            serde_json::from_slice(&env.meta.raw(&name).unwrap()).unwrap();
        record.created_unix_ms = created;
        env.meta
            .set_raw(&name, &serde_json::to_vec(&record).unwrap());
    }

    #[test]
    fn retention_keeps_newest_and_pinned_sets() {
        let env = setup();
        let h = Handle::new(&env.service);
        env.runtime
            .configure(
                policy(json!({"drill_every":0,"retention_enabled":true})),
                &h,
                BASE,
            )
            .unwrap();
        let mut ids = Vec::new();
        for _ in 0..5 {
            std::thread::sleep(Duration::from_millis(5));
            let report = env.runtime.run_now(&h).unwrap();
            assert_eq!(report["collection"]["ok"], true, "{report}");
            ids.push(report["set_id"].as_str().unwrap().to_owned());
            if ids.len() == 1 {
                let pinned = env.runtime.pin(&h, &ids[0], "legal hold").unwrap();
                assert_eq!(pinned["pin"]["reason"], "legal hold");
            }
        }
        // Everything is younger than keep_days: nothing was deleted.
        assert_eq!(
            env.runtime.list().unwrap()["sets"]
                .as_array()
                .unwrap()
                .len(),
            5
        );
        for (i, id) in ids.iter().enumerate() {
            age(&env, id, BASE + i as u64);
        }
        let collected = env.runtime.collect(&h).unwrap();
        assert_eq!(collected["result"]["deleted"], json!([ids[1]]));
        let listing = env.runtime.list().unwrap();
        let active: Vec<&str> = listing["sets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["set_id"].as_str().unwrap())
            .collect();
        assert_eq!(active, vec![&ids[4], &ids[3], &ids[2], &ids[0]]);
        assert_eq!(listing["tombstoned"][0]["set_id"], ids[1]);
        assert_eq!(listing["tombstoned"][0]["data_objects"], 0);

        // Unpinning makes the old set collectable by the next collection.
        let pin = set(&listing, &ids[0])["record"]["pins"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        env.runtime.unpin(&h, &ids[0], &pin, false).unwrap();
        let collected = env.runtime.collect(&h).unwrap();
        assert_eq!(collected["result"]["deleted"], json!([ids[0]]));
        assert_eq!(
            status(&env.runtime)["last_collection"]["result"]["deleted"],
            json!({"count":1,"sample":[ids[0]],"truncated":false})
        );
    }

    #[test]
    fn delete_active_restore_pin_is_refused_and_source_survives_collect() {
        // Review M2: DELETE could remove a running restore's pin, and a
        // collection then deleted the set mid-download.
        let env = setup();
        let h = Handle::new(&env.service);
        env.runtime
            .configure(
                policy(json!({"drill_every":0,"retention_enabled":true})),
                &h,
                BASE,
            )
            .unwrap();
        let mut ids = Vec::new();
        for _ in 0..5 {
            std::thread::sleep(Duration::from_millis(5));
            let report = env.runtime.run_now(&h).unwrap();
            ids.push(report["set_id"].as_str().unwrap().to_owned());
        }
        for (i, id) in ids.iter().enumerate() {
            age(&env, id, BASE + i as u64);
        }
        // Pause the restore of the oldest (collectable) set after its pin was
        // taken: the admin and the janitor act while the download runs.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (runtime, handle, source, out) =
            (env.runtime.clone(), h.clone(), ids[0].clone(), seen.clone());
        env.data
            .hook(Op::Get, &format!("{source}/manifest.json"), move || {
                let listing = runtime.list().unwrap();
                let pin = set(&listing, &source)["record"]["pins"][0].clone();
                let id = pin["id"].as_str().unwrap().to_owned();
                let mut out = out.lock().unwrap();
                out.push(pin["kind"].clone());
                out.push(json!(code(runtime.unpin(&handle, &source, &id, false))));
                out.push(json!(code(runtime.unpin(&handle, &source, &id, true))));
                out.push(runtime.collect(&handle).unwrap()["result"]["deleted"].clone());
            });
        let admin_dir = env.root.join("admin");
        std::fs::create_dir(&admin_dir).unwrap();
        let report = env.runtime.restore(&h, &ids[0], &admin_dir).unwrap();
        assert_eq!(report["pin_released"], true);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                json!("operation"),
                json!("cloud_pin_owned"),
                json!("cloud_pin_in_use"),
                json!([ids[1]]),
            ]
        );
        // Released by its owner, the source is collectable again.
        let collected = env.runtime.collect(&h).unwrap();
        assert_eq!(collected["result"]["deleted"], json!([ids[0]]));
    }

    #[test]
    fn a_crashed_owners_operation_pin_needs_an_audited_force() {
        let env = setup();
        let h = Handle::new(&env.service);
        env.runtime
            .configure(policy(json!({"drill_every":0})), &h, BASE)
            .unwrap();
        let id = env.runtime.run_now(&h).unwrap()["set_id"]
            .as_str()
            .unwrap()
            .to_owned();
        // An operation pin left by a process that is gone (not live here).
        let (sets, _, _) = env.runtime.connect().unwrap();
        let pin = sets
            .pin(&id, "restore", cloud::PinKind::Operation, BASE)
            .unwrap();
        assert_eq!(
            code(env.runtime.unpin(&h, &id, &pin.id, false)),
            "cloud_pin_owned"
        );
        let report = env.runtime.unpin(&h, &id, &pin.id, true).unwrap();
        assert_eq!(report["forced"], true);
        let listing = env.runtime.list().unwrap();
        assert_eq!(set(&listing, &id)["record"]["pins"], json!([]));
        // An operator hold needs no force and is not reported as forced.
        let hold = env.runtime.pin(&h, &id, "legal hold").unwrap();
        let hold = hold["pin"]["id"].as_str().unwrap();
        assert_eq!(
            env.runtime.unpin(&h, &id, hold, false).unwrap()["forced"],
            false
        );
    }

    #[test]
    fn collection_summary_is_bounded_and_restartable_at_max_sets() {
        // Review H1: a collection deleting thousands of sets persisted every
        // ID, so state.json outgrew its read limit and startup failed.
        let env = setup();
        let h = Handle::new(&env.service);
        env.runtime
            .configure(
                policy(json!({"drill_every":0,"retention_enabled":true})),
                &h,
                BASE,
            )
            .unwrap();
        let id = env.runtime.run_now(&h).unwrap()["set_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let template = env
            .meta
            .raw(&format!("vlite/prod/sets/{id}/record.json"))
            .unwrap();
        // The listing accepts at most MAX_SETS active sets; fill it with old
        // ones (older than keep_days) so all but keep_sets are deleted.
        for i in 0..cloud::MAX_SETS as u64 - 1 {
            let old = cloud::new_set_id(BASE + i).unwrap();
            let mut record: cloud::Record = serde_json::from_slice(&template).unwrap();
            record.set_id = old.clone();
            record.created_unix_ms = BASE + i;
            record.updated_unix_ms = BASE + i;
            env.meta.set_raw(
                &format!("vlite/prod/sets/{old}/record.json"),
                &serde_json::to_vec(&record).unwrap(),
            );
        }
        let report = env.runtime.collect(&h).unwrap();
        let deleted = report["result"]["deleted"].as_array().unwrap().len();
        assert_eq!(deleted, cloud::MAX_SETS - 3, "the response is complete");
        let state = env.root.join("cloud-backup").join(STATE);
        let bytes = std::fs::metadata(&state).unwrap().len();
        assert!(bytes <= MAX_FILE / 4, "state.json has {bytes} bytes");
        let reopened = reopen(&env);
        let last = &status(&reopened)["last_collection"]["result"];
        assert_eq!(last["deleted"]["count"], deleted);
        assert_eq!(
            last["deleted"]["sample"].as_array().unwrap().len(),
            SUMMARY_SAMPLE
        );
        assert_eq!(last["skipped"]["count"], 0);
    }

    #[test]
    fn an_oversized_status_never_replaces_the_state_file() {
        let env = setup();
        let h = Handle::new(&env.service);
        env.runtime.configure(policy(json!({})), &h, BASE).unwrap();
        let state = env.root.join("cloud-backup").join(STATE);
        let before = std::fs::read(&state).unwrap();
        let huge = "x".repeat(MAX_FILE as usize);
        let error = env
            .runtime
            .update(|l| l.status.last_collection = Some(json!(huge)))
            .expect_err("an oversized status must be refused");
        assert_eq!(error.code, "cloud_backup_state_too_large");
        assert_eq!(std::fs::read(&state).unwrap(), before);
        assert!(reopen(&env).view().is_ok());
    }

    #[test]
    fn the_maintenance_worker_starts_a_due_cloud_run() {
        let dir = tempfile::tempdir().unwrap();
        let cloud = dir.path().join("cloud-backup");
        std::fs::create_dir_all(&cloud).unwrap();
        let enabled = policy(json!({"enabled":true,"interval_secs":300,"drill_every":0}));
        std::fs::write(cloud.join(INSTALLATION), b"0123456789abcdef").unwrap();
        // Configured long ago: the first run is due at startup.
        let state = document(&enabled, json!({"policy_updated_unix_ms":1}));
        std::fs::write(cloud.join(STATE), state).unwrap();
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
        assert_eq!(body["pin"]["kind"], "operator");
        for query in ["force=maybe", "force=true&x=1", "FAKE_SECRET_REVIEW_MARKER"] {
            let (status, body) =
                call(&app, "DELETE", &format!("{sets}/pins/{pin}?{query}"), None).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            assert!(!body.to_string().contains("MARKER"), "{body}");
        }
        let (status, body) = call(&app, "DELETE", &format!("{sets}/pins/{pin}"), None).await;
        assert_eq!((status, &body["removed"]), (StatusCode::OK, &json!(true)));
        assert_eq!(body["forced"], false);
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

    #[tokio::test]
    async fn parse_errors_never_echo_the_request_body() {
        const MARKER: &str = "FAKE_SECRET_REVIEW_MARKER";
        let env = setup();
        let mut service = env.service.clone();
        let mut state = admin::AdminState::default();
        state.directory = Some(env.root.join("admin"));
        std::fs::create_dir(env.root.join("admin")).unwrap();
        let _ = state.cloud.set(env.runtime.clone());
        service.admin = Arc::new(state);
        let app = routes().with_state(service);
        let base = "/v1/admin/cloud-backup";
        let set = format!("{base}/sets/20250101T000000Z-0123456789abcdef");
        let mut tagged = target();
        tagged[MARKER] = json!("x");
        let cases = [
            (
                "PUT",
                base.to_owned(),
                json!({"target": target(), MARKER: "x"}),
            ),
            ("PUT", base.to_owned(), json!({"target": tagged})),
            (
                "PUT",
                base.to_owned(),
                json!({"target": target(), "enabled": MARKER}),
            ),
            (
                "PUT",
                base.to_owned(),
                json!({"target": target(), "keep_sets": MARKER}),
            ),
            (
                "PUT",
                base.to_owned(),
                json!({"target": {"account": MARKER}}),
            ),
            ("POST", format!("{set}/restore"), json!({"mode": MARKER})),
            ("POST", format!("{set}/restore"), json!({MARKER: "restore"})),
            ("POST", format!("{set}/restore"), json!([MARKER])),
            (
                "POST",
                format!("{set}/pins"),
                json!({"reason": "r", MARKER: 1}),
            ),
            ("POST", format!("{set}/pins"), json!({"reason": [MARKER]})),
        ];
        for (method, uri, body) in cases {
            let (status, reply) = call(&app, method, &uri, Some(body.clone())).await;
            assert!(status.is_client_error(), "{method} {uri} {body}: {status}");
            assert!(
                !reply.to_string().contains("MARKER"),
                "{method} {uri} echoed {reply}"
            );
        }
        // Malformed JSON, the restore route's raw parser included.
        for uri in [
            base.to_owned(),
            format!("{set}/restore"),
            format!("{set}/pins"),
        ] {
            let method = if uri == base { "PUT" } else { "POST" };
            let request = Request::builder()
                .method(method)
                .uri(&uri)
                .header("content-type", "application/json")
                .body(Body::from(format!("{{\"{MARKER}\": tru")))
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            // An unknown key may be refused (422) before the syntax error.
            assert!(response.status().is_client_error(), "{uri}");
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let text = String::from_utf8_lossy(&bytes);
            assert!(!text.contains("MARKER"), "{uri} echoed {text}");
            assert!(text.contains("invalid_json"), "{uri}: {text}");
        }
    }
}
