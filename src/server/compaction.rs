//! Managed single-file compaction (feature-dashboard-compaction).
//!
//! A job retires the whole serving generation, freezes a verified copy of
//! the source, builds a candidate with the two-hop preservation transform,
//! durably selects it and starts a fresh generation around it. Before the
//! selecting intent every exit reconstructs a fresh generation around the
//! untouched source; after it the candidate is authoritative and the source
//! is never reopened automatically.
//!
//! Durable records live in `<admin>/compaction/`: `state.json` (settings,
//! schedule history and the active job journal, checksummed) and
//! `selection.json` (the stable selection authority, read at startup before
//! the legacy `next-startup` marker). Candidates use the `restore-*.vlite`
//! namespace; frozen recovery copies stay in `compaction/`.

use super::admin::{self, AdminState, Role, ServerOptions};
use super::{ApiError, Service, audit, body, internal, logs, users};
use crate::{Database, Durability};
use axum::{
    Extension, Json, Router,
    extract::{Path as UrlPath, Request, State, rejection::JsonRejection},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::Duration;
use tokio::sync::Notify;

/// The only qualified runtime profile.
pub(super) const PROFILE: &str = "linux-local-ext4-v1";
/// Largest source file the profile was qualified with.
const PROFILE_MAX: u64 = 1 << 30;
const MIB: u64 = 1 << 20;
/// Drain budget for admitted requests and worker permits.
const DRAIN: Duration = Duration::from_secs(30);
const STATE_MAX: u64 = 1 << 20;
const SELECTION_MAX: u64 = 64 << 10;
const HISTORY: usize = 16;
const KEYS: usize = 32;
const DAY_MS: u64 = 86_400_000;

const OPEN: u8 = 0;
const MAINTENANCE: u8 = 1;
const RECOVERY: u8 = 2;

/// Test-only profile (relaxed size floors, no filesystem gate). Requires the
/// `compaction-test-hooks` build feature AND the environment switch.
fn test_profile() -> bool {
    cfg!(feature = "compaction-test-hooks")
        && std::env::var("VLITE_COMPACTION_TEST_PROFILE").is_ok_and(|v| v == "1")
}

/// Test-only fault injection point.
fn fault(point: &str) -> bool {
    cfg!(feature = "compaction-test-hooks")
        && std::env::var("VLITE_COMPACTION_FAULT").is_ok_and(|v| v.split(',').any(|p| p == point))
}

fn crash_point(point: &str) {
    if fault(point) {
        std::process::abort();
    }
}

fn now() -> u64 {
    u64::try_from(admin::now_ms()).unwrap_or(u64::MAX)
}

// ---------------------------------------------------------------- settings

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(deny_unknown_fields)]
pub(super) struct Settings {
    enabled: bool,
    daily_start_utc: String,
    window_minutes: u32,
    max_run_seconds: u32,
    min_file_bytes: u64,
    min_growth_bytes: u64,
    min_growth_percent: u32,
    cooldown_hours: u32,
    max_source_bytes: u64,
    history_policy: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            daily_start_utc: "02:00".into(),
            window_minutes: 30,
            max_run_seconds: 900,
            min_file_bytes: 256 * MIB,
            min_growth_bytes: 128 * MIB,
            min_growth_percent: 25,
            cooldown_hours: 168,
            max_source_bytes: PROFILE_MAX,
            history_policy: "unacknowledged".into(),
        }
    }
}

fn parse_hhmm(text: &str) -> Option<u32> {
    let bytes = text.as_bytes();
    if bytes.len() != 5 || bytes[2] != b':' {
        return None;
    }
    let digits = |a: u8, b: u8| -> Option<u32> {
        (a.is_ascii_digit() && b.is_ascii_digit())
            .then(|| u32::from(a - b'0') * 10 + u32::from(b - b'0'))
    };
    let hours = digits(bytes[0], bytes[1])?;
    let minutes = digits(bytes[3], bytes[4])?;
    (hours < 24 && minutes < 60).then_some(hours * 60 + minutes)
}

impl Settings {
    fn validate(&self) -> Result<(), String> {
        let floor = if test_profile() { 0 } else { 64 * MIB };
        if parse_hhmm(&self.daily_start_utc).is_none() {
            return Err("daily_start_utc must be HH:MM in UTC (00:00..23:59)".into());
        }
        if !(5..=120).contains(&self.window_minutes) {
            return Err("window_minutes must be between 5 and 120".into());
        }
        if !(60..=3600).contains(&self.max_run_seconds)
            || self.max_run_seconds + 60 > self.window_minutes * 60
        {
            return Err(
                "max_run_seconds must be 60..3600 and leave one minute of the window".into(),
            );
        }
        if self.max_source_bytes < floor.max(1) || self.max_source_bytes > PROFILE_MAX {
            return Err(format!(
                "max_source_bytes must be between {} and {PROFILE_MAX} (the qualified profile maximum)",
                floor.max(1)
            ));
        }
        if self.min_file_bytes < floor || self.min_file_bytes > self.max_source_bytes {
            return Err(format!(
                "min_file_bytes must be at least {floor} and not above max_source_bytes"
            ));
        }
        if self.min_growth_bytes < floor || self.min_growth_bytes > self.max_source_bytes {
            return Err(format!(
                "min_growth_bytes must be at least {floor} and not above max_source_bytes"
            ));
        }
        if !(10..=200).contains(&self.min_growth_percent) {
            return Err("min_growth_percent must be between 10 and 200".into());
        }
        if !(24..=720).contains(&self.cooldown_hours) {
            return Err("cooldown_hours must be between 24 and 720".into());
        }
        if !matches!(
            self.history_policy.as_str(),
            "unacknowledged" | "resnapshot_allowed"
        ) {
            return Err("history_policy must be unacknowledged or resnapshot_allowed".into());
        }
        Ok(())
    }
}

// ------------------------------------------------------------ durable state

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
struct Ack {
    lineage: String,
    unix_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(deny_unknown_fields)]
pub(super) struct Job {
    id: String,
    trigger: String,
    occurrence: Option<String>,
    phase: String,
    created_unix_ms: u64,
    updated_unix_ms: u64,
    finished_unix_ms: Option<u64>,
    deadline_unix_ms: u64,
    cancel_requested: bool,
    /// `not_published`, `committed` or `outcome_unknown`.
    selection: String,
    settings_version: u64,
    source: String,
    lineage: Option<String>,
    sequence: Option<u64>,
    before_bytes: Option<u64>,
    after_bytes: Option<u64>,
    candidate: Option<String>,
    recovery: Option<String>,
    outcome: Option<String>,
    code: Option<String>,
    action: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
struct Key {
    key: String,
    digest: String,
    job: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub(super) struct Persisted {
    schema: u32,
    epoch: u64,
    settings_version: u64,
    settings: Settings,
    ack: Option<Ack>,
    lineage: Option<String>,
    baseline_bytes: Option<u64>,
    failures: u32,
    suspended: bool,
    next_eligible_unix_ms: u64,
    last_occurrence: Option<String>,
    job: Option<Job>,
    history: Vec<Job>,
    keys: Vec<Key>,
}

impl Default for Persisted {
    fn default() -> Self {
        Self {
            schema: 1,
            epoch: 0,
            settings_version: 1,
            settings: Settings::default(),
            ack: None,
            lineage: None,
            baseline_bytes: None,
            failures: 0,
            suspended: false,
            next_eligible_unix_ms: 0,
            last_occurrence: None,
            job: None,
            history: Vec::new(),
            keys: Vec::new(),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn digest(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn regular(path: &Path) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => Err(std::io::Error::other(
            "compaction record must be a regular file",
        )),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn read_bounded(path: &Path, limit: u64) -> std::io::Result<Option<Vec<u8>>> {
    if !regular(path)? {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(std::io::Error::other("compaction record is too large"));
    }
    Ok(Some(bytes))
}

/// Write-new, file sync, atomic rename, directory sync.
fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(dir.join(name)).map_err(|e| e.error)?;
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

fn load_state(work: &Path) -> std::io::Result<Persisted> {
    let Some(bytes) = read_bounded(&work.join("state.json"), STATE_MAX)? else {
        return Ok(Persisted::default());
    };
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| std::io::Error::other("compaction state is not UTF-8"))?;
    let (sum, json) = text
        .split_once('\n')
        .ok_or_else(|| std::io::Error::other("compaction state is malformed"))?;
    let json = json.trim_end_matches('\n');
    if digest(json.as_bytes()) != sum {
        return Err(std::io::Error::other("compaction state checksum mismatch"));
    }
    let state: Persisted = serde_json::from_str(json)
        .map_err(|_| std::io::Error::other("compaction state is invalid"))?;
    if state.schema != 1 || state.settings.validate_shape().is_err() {
        return Err(std::io::Error::other("compaction state is unsupported"));
    }
    Ok(state)
}

impl Settings {
    /// Stored settings only need a valid shape (profile floors may differ).
    fn validate_shape(&self) -> Result<(), ()> {
        if parse_hhmm(&self.daily_start_utc).is_none() || self.max_source_bytes > PROFILE_MAX {
            return Err(());
        }
        Ok(())
    }
}

fn store_state(work: &Path, state: &mut Persisted) -> std::io::Result<()> {
    if fault("state_write_error") && state.job.as_ref().is_some_and(|j| j.phase == "selecting") {
        return Err(std::io::Error::other("injected state write failure"));
    }
    state.epoch += 1;
    let json = serde_json::to_string(state).map_err(std::io::Error::other)?;
    let text = format!("{}\n{json}\n", digest(json.as_bytes()));
    write_atomic(work, "state.json", text.as_bytes())
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
struct Selection {
    schema: u32,
    epoch: u64,
    candidate: String,
    lineage: String,
    sequence: u64,
    job: String,
    predecessor: String,
}

fn read_selection(dir: &Path) -> std::io::Result<Option<Selection>> {
    let Some(bytes) = read_bounded(
        &dir.join("compaction").join("selection.json"),
        SELECTION_MAX,
    )?
    else {
        return Ok(None);
    };
    let selection: Selection = serde_json::from_slice(&bytes)
        .map_err(|_| std::io::Error::other("compaction selection is invalid"))?;
    if selection.schema != 1 || !admin::generated(&selection.candidate, "restore-") {
        return Err(std::io::Error::other("compaction selection is unsupported"));
    }
    Ok(Some(selection))
}

/// Validates a candidate file: generated name, regular file, complete tail
/// and the expected lineage.
fn validate_candidate(dir: &Path, name: &str, lineage: &str) -> std::io::Result<PathBuf> {
    let path = admin::safe_file(dir, name, "restore-")
        .map_err(|_| std::io::Error::other("compaction candidate is missing"))?;
    let detail = crate::maintenance::inspect_detail(&path)
        .map_err(|_| std::io::Error::other("compaction candidate failed validation"))?;
    if detail.inspection.incomplete_tail_bytes != 0 {
        return Err(std::io::Error::other(
            "compaction candidate has an incomplete tail",
        ));
    }
    if detail.lineage.map(|l| format!("{l:016x}")).as_deref() != Some(lineage) {
        return Err(std::io::Error::other(
            "compaction candidate lineage mismatch",
        ));
    }
    Ok(path)
}

/// The authoritative compaction selection, consulted by `selected_database`
/// before the legacy marker. A corrupt or failing authority is an error,
/// never a fallback to the source.
pub(super) fn selected(dir: &Path) -> std::io::Result<Option<PathBuf>> {
    let Some(selection) = read_selection(dir)? else {
        return Ok(None);
    };
    validate_candidate(dir, &selection.candidate, &selection.lineage).map(Some)
}

/// Serializes restore selection with compaction selection. Refused while a
/// job is active or once a compaction selection authority exists.
pub(super) fn selection_guard<'a>(
    coordinator: Option<&'a Coordinator>,
    dir: &Path,
) -> Result<Option<MutexGuard<'a, ()>>, ApiError> {
    let guard = coordinator
        .map(|c| c.selection.lock().map_err(internal))
        .transpose()?;
    if coordinator.is_some_and(|c| c.active.load(Ordering::SeqCst)) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "compaction_busy",
            "a compaction job is active; restore selection waits until it finishes",
        ));
    }
    if dir.join("compaction").join("selection.json").exists() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "compaction_selection_pinned",
            "a committed compaction selection pins the startup database; changing it needs a recovery-capable selection transition that this release does not provide",
        ));
    }
    Ok(guard)
}

// -------------------------------------------------------------- coordinator

struct Generation {
    router: Router,
    service: Service,
    path: PathBuf,
}

pub struct Coordinator {
    dir: PathBuf,
    work: PathBuf,
    _owner: std::fs::File,
    state: Mutex<Persisted>,
    fence: AtomicU8,
    recovery_code: Mutex<Option<&'static str>>,
    tickets: AtomicUsize,
    drained: Notify,
    selection: Mutex<()>,
    active: AtomicBool,
    finished: Notify,
    stopping: AtomicBool,
    generation: RwLock<Option<Generation>>,
    control: Arc<AdminState>,
    options: ServerOptions,
    durability: Mutex<Option<Durability>>,
    /// Jobs finished by this process, indexed like `OUTCOMES`.
    outcomes: [AtomicU64; 4],
}

/// Bounded outcome labels for the `/metrics` counters.
const OUTCOMES: [&str; 4] = ["succeeded", "failed", "cancelled", "skipped"];

impl Coordinator {
    fn snapshot(&self) -> Persisted {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Mutates and durably stores the state; on failure the in-memory state
    /// keeps the mutation (it describes the attempted transition).
    fn update(&self, change: impl FnOnce(&mut Persisted)) -> std::io::Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let running = state.job.as_ref().map(|job| job.id.clone());
        change(&mut state);
        if let (Some(id), None, Some(last)) = (running, &state.job, state.history.first())
            && last.id == id
            && let Some(index) = OUTCOMES
                .iter()
                .position(|o| Some(*o) == last.outcome.as_deref())
        {
            self.outcomes[index].fetch_add(1, Ordering::Relaxed);
        }
        let mut copy = state.clone();
        let result = store_state(&self.work, &mut copy);
        state.epoch = copy.epoch;
        result
    }

    fn job(&self, change: impl FnOnce(&mut Job)) -> std::io::Result<()> {
        self.update(|state| {
            if let Some(job) = state.job.as_mut() {
                change(job);
                job.updated_unix_ms = now();
            }
        })
    }

    /// Secret-free `/metrics` fields for administrators: outcome counters for
    /// this process, schedule state and the last finished job's sizes. No
    /// file names, job IDs or error text.
    pub(super) fn metrics(&self) -> serde_json::Map<String, Value> {
        let state = self.snapshot();
        let last = state.history.first();
        let mut out = serde_json::Map::new();
        for (name, counter) in OUTCOMES.iter().zip(&self.outcomes) {
            out.insert(
                format!("compaction_jobs_{name}_total"),
                json!(counter.load(Ordering::Relaxed)),
            );
        }
        out.insert(
            "compaction_schedule_enabled".into(),
            json!(u8::from(state.settings.enabled)),
        );
        out.insert(
            "compaction_schedule_suspended".into(),
            json!(u8::from(state.suspended)),
        );
        out.insert(
            "compaction_scheduled_failures".into(),
            json!(state.failures),
        );
        out.insert(
            "compaction_last_finished_unix_seconds".into(),
            json!(last.and_then(|j| j.finished_unix_ms).map(|ms| ms / 1000)),
        );
        out.insert(
            "compaction_last_succeeded".into(),
            json!(last.map(|j| u8::from(j.outcome.as_deref() == Some("succeeded")))),
        );
        out.insert(
            "compaction_last_before_bytes".into(),
            json!(last.and_then(|j| j.before_bytes)),
        );
        out.insert(
            "compaction_last_after_bytes".into(),
            json!(last.and_then(|j| j.after_bytes)),
        );
        out
    }

    fn current(&self) -> Option<(Service, PathBuf)> {
        self.generation
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|g| (g.service.clone(), g.path.clone()))
    }

    fn fence_recovery(&self, code: &'static str) {
        *self
            .recovery_code
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(code);
        self.fence.store(RECOVERY, Ordering::SeqCst);
    }

    fn recovery(&self) -> Option<&'static str> {
        if self.fence.load(Ordering::SeqCst) != RECOVERY {
            return None;
        }
        self.recovery_code
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .or(Some("recovery_required"))
    }

    fn cancel_requested(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .job
            .as_ref()
            .is_some_and(|j| j.cancel_requested)
    }

    fn generation_options(&self, path: &Path) -> ServerOptions {
        let mut options = self.options.clone();
        options.database_path = Some(path.to_path_buf());
        options
    }

    fn install(&self, service: Service, path: PathBuf, router: Router) {
        *self
            .generation
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Generation {
            router,
            service,
            path,
        });
    }

    /// Builds and installs a fresh generation around `database`.
    fn rebuild(&self, template: &Service, database: Database, path: &Path) -> std::io::Result<()> {
        let service = template.successor(database)?;
        match admin::configured_service(
            service.clone(),
            self.generation_options(path),
            Some(self.control.clone()),
        ) {
            Ok((configured, router)) => {
                self.install(configured, path.to_path_buf(), router);
                Ok(())
            }
            Err(error) => {
                let closing = service.clone();
                tokio::spawn(async move {
                    let _ = closing.close().await;
                });
                Err(error)
            }
        }
    }

    fn audit(&self, event: Value) {
        if let Some(log) = &self.control.audit_log {
            let _ = log.append(event, true);
        }
    }
}

/// Router fallback that serves every request through the current generation.
async fn dispatch(State(c): State<Arc<Coordinator>>, request: Request) -> Response {
    use tower::ServiceExt;
    let router = c
        .generation
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map(|g| g.router.clone());
    match router {
        Some(router) => match router.oneshot(request).await {
            Ok(response) => response,
            Err(never) => match never {},
        },
        None => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "recovery_required",
            "the database is unavailable",
        )
        .into_response(),
    }
}

/// Namespace fence: after authentication, before admission. Control-plane
/// routes stay available; every other request takes a ticket that the
/// coordinator drains before retiring the generation.
pub(super) async fn fence(
    State(service): State<Service>,
    request: Request,
    next: Next,
) -> Response {
    let Some(c) = service.admin.compaction.get().cloned() else {
        return next.run(request).await;
    };
    let path = request.uri().path();
    if path == "/health"
        || path == "/dashboard"
        || path.starts_with("/dashboard/")
        || path.starts_with("/v1/auth/")
        || path == "/v1/capabilities"
        || path == "/v1/admin/compaction"
        || path.starts_with("/v1/admin/compaction/")
    {
        return next.run(request).await;
    }
    let fenced = |c: &Coordinator| match c.fence.load(Ordering::SeqCst) {
        OPEN => None,
        MAINTENANCE => Some(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "maintenance_in_progress",
            "file compaction is in progress; retry later (a write with an unknown outcome must be checked before retrying)",
        )),
        _ => Some(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "recovery_required",
            "the database needs operator recovery; see Administration -> Backup & recovery",
        )),
    };
    if let Some(error) = fenced(&c) {
        return error.into_response();
    }
    c.tickets.fetch_add(1, Ordering::SeqCst);
    struct Ticket(Arc<Coordinator>);
    impl Drop for Ticket {
        fn drop(&mut self) {
            if self.0.tickets.fetch_sub(1, Ordering::SeqCst) == 1 {
                self.0.drained.notify_waiters();
            }
        }
    }
    let ticket = Ticket(c);
    if let Some(error) = fenced(&ticket.0) {
        drop(ticket);
        return error.into_response();
    }
    let response = next.run(request).await;
    drop(ticket);
    response
}

// ----------------------------------------------------------------- runtime

/// Startup half of the managed runtime: loads database-independent control
/// state and reconciles the compaction journal before any database opens.
pub struct Managed {
    coordinator: Arc<Coordinator>,
    recovery: Option<&'static str>,
}

/// Running managed runtime.
#[derive(Clone)]
pub struct ManagedHandle {
    coordinator: Arc<Coordinator>,
}

fn finish_job(state: &mut Persisted, outcome: &str, code: Option<&str>, action: Option<&str>) {
    if let Some(mut job) = state.job.take() {
        let at = now();
        job.phase = outcome.into();
        job.outcome = Some(outcome.into());
        job.code = code.map(Into::into);
        job.action = action.map(Into::into);
        job.finished_unix_ms = Some(at);
        job.updated_unix_ms = at;
        if job.trigger == "scheduled" && outcome == "failed" {
            state.failures += 1;
            match state.failures {
                1 => state.next_eligible_unix_ms = at.saturating_add(DAY_MS),
                2 => state.next_eligible_unix_ms = at.saturating_add(2 * DAY_MS),
                _ => state.suspended = true,
            }
        }
        state.history.insert(0, job);
        state.history.truncate(HISTORY);
    }
}

fn remove_quietly(path: &Path) {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
        let _ = std::fs::remove_dir_all(path);
    } else {
        let _ = std::fs::remove_file(path);
    }
}

/// Removes the unselected artifacts of a pre-selection exit.
fn cleanup(dir: &Path, work: &Path, job: &Job) {
    if let Some(name) = &job.candidate
        && admin::generated(name, "restore-")
    {
        remove_quietly(&dir.join(name));
    }
    if let Some(name) = &job.recovery {
        remove_quietly(&work.join(name));
    }
    remove_quietly(&work.join(format!("staging-{}", job.id)));
}

const PRESELECTION: &[&str] = &[
    "preflight",
    "quiescing",
    "freezing",
    "building",
    "validating",
    "ready_to_select",
    "recovering_source",
];

fn write_selection(dir: &Path, job: &Job) -> std::io::Result<()> {
    let previous = read_selection(dir).ok().flatten();
    let selection = Selection {
        schema: 1,
        epoch: previous.as_ref().map_or(1, |s| s.epoch + 1),
        candidate: job.candidate.clone().unwrap_or_default(),
        lineage: job.lineage.clone().unwrap_or_default(),
        sequence: job.sequence.unwrap_or_default(),
        job: job.id.clone(),
        predecessor: job.source.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&selection).map_err(std::io::Error::other)?;
    write_atomic(&dir.join("compaction"), "selection.json", &bytes)?;
    if fault("selection_effect_error") {
        return Err(std::io::Error::other(
            "injected error after selection write",
        ));
    }
    Ok(())
}

fn write_marker(dir: &Path, name: &str) -> std::io::Result<()> {
    write_atomic(dir, "next-startup", name.as_bytes())
}

impl Managed {
    /// Loads control state, takes the compaction owner lock and reconciles
    /// an interrupted job. Requires an admin directory.
    pub fn prepare(options: &ServerOptions) -> std::io::Result<Self> {
        let control = admin::load_control(options)?;
        let dir = control
            .directory
            .clone()
            .ok_or_else(|| std::io::Error::other("managed compaction needs an admin directory"))?;
        let work = dir.join("compaction");
        std::fs::create_dir_all(&work)?;
        if std::fs::symlink_metadata(&work)?.file_type().is_symlink() {
            return Err(std::io::Error::other(
                "compaction directory must not be a symlink",
            ));
        }
        let owner = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(work.join("owner.lock"))?;
        owner.try_lock().map_err(|_| {
            std::io::Error::other("another server owns this admin directory's compaction state")
        })?;
        let mut recovery = None;
        let mut state = match load_state(&work) {
            Ok(state) => state,
            Err(_) => {
                recovery = Some("compaction_state_invalid");
                Persisted::default()
            }
        };
        let selection = match read_selection(&dir) {
            Ok(selection) => selection,
            Err(_) => {
                recovery = Some("selection_invalid");
                None
            }
        };
        if recovery.is_none()
            && let Some(job) = state.job.clone()
        {
            let resolved = selection.as_ref().is_some_and(|s| s.job == job.id);
            if PRESELECTION.contains(&job.phase.as_str()) {
                cleanup(&dir, &work, &job);
                finish_job(
                    &mut state,
                    "failed",
                    Some("interrupted"),
                    Some(
                        "The server stopped during compaction before selection; the source file stayed authoritative.",
                    ),
                );
                store_state(&work, &mut state)?;
            } else if resolved {
                // Authority committed: repair the job record and the marker.
                if let Some(job) = state.job.as_mut() {
                    job.selection = "committed".into();
                    job.phase = "reopening".into();
                }
                if let Some(name) = &job.candidate {
                    let _ = write_marker(&dir, name);
                }
                store_state(&work, &mut state)?;
            } else {
                // SELECTING or OUTCOME_UNKNOWN without a conclusive authority:
                // finish the selection only for a validated candidate.
                let finished = job
                    .candidate
                    .as_deref()
                    .zip(job.lineage.as_deref())
                    .ok_or_else(|| std::io::Error::other("incomplete intent"))
                    .and_then(|(name, lineage)| validate_candidate(&dir, name, lineage))
                    .and_then(|_| write_selection(&dir, &job));
                match finished {
                    Ok(()) => {
                        if let Some(job) = state.job.as_mut() {
                            job.selection = "committed".into();
                            job.phase = "reopening".into();
                        }
                        if let Some(name) = &job.candidate {
                            let _ = write_marker(&dir, name);
                        }
                        store_state(&work, &mut state)?;
                    }
                    Err(_) => {
                        if let Some(job) = state.job.as_mut() {
                            job.selection = "outcome_unknown".into();
                        }
                        recovery = Some("selection_unresolved");
                    }
                }
            }
        }
        if recovery.is_none()
            && read_selection(&dir).is_ok_and(|s| s.is_some())
            && selected(&dir).is_err()
        {
            recovery = Some("selected_candidate_invalid");
        }
        let coordinator = Arc::new(Coordinator {
            dir,
            work,
            _owner: owner,
            state: Mutex::new(state),
            fence: AtomicU8::new(OPEN),
            recovery_code: Mutex::new(None),
            tickets: AtomicUsize::new(0),
            drained: Notify::new(),
            selection: Mutex::new(()),
            active: AtomicBool::new(false),
            finished: Notify::new(),
            stopping: AtomicBool::new(false),
            generation: RwLock::new(None),
            control: control.clone(),
            options: options.clone(),
            durability: Mutex::new(None),
            outcomes: Default::default(),
        });
        if let Some(code) = recovery {
            coordinator.fence_recovery(code);
        }
        control
            .compaction
            .set(coordinator.clone())
            .map_err(|_| std::io::Error::other("compaction runtime is already configured"))?;
        Ok(Self {
            coordinator,
            recovery,
        })
    }

    /// Why startup must not open a database (authenticated recovery mode).
    pub fn recovery_reason(&self) -> Option<&'static str> {
        self.recovery
    }

    /// Whether a committed compaction selection exists (a failed open then
    /// enters recovery mode instead of exiting).
    pub fn has_selection(&self) -> bool {
        read_selection(&self.coordinator.dir).is_ok_and(|s| s.is_some())
    }

    /// Serves `service` (opened at `active` with `durability`).
    pub fn serve(
        self,
        service: Service,
        active: PathBuf,
        durability: Durability,
    ) -> std::io::Result<(Router, ManagedHandle)> {
        let c = self.coordinator;
        *c.durability
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(durability);
        let lineage = service.lineage.map(|l| format!("{l:016x}"));
        let active_name = file_name(&active);
        c.update(|state| {
            if state.lineage != lineage {
                if state.lineage.is_some() {
                    // A different file family: never carry consent or a baseline over.
                    state.settings.enabled = false;
                    state.ack = None;
                    state.baseline_bytes = None;
                }
                state.lineage = lineage.clone();
            }
            if let Some(job) = &state.job
                && job.selection == "committed"
                && job.candidate.as_deref() == Some(active_name.as_str())
            {
                {
                    let after = std::fs::metadata(&active).map(|m| m.len()).ok();
                    let cooldown = u64::from(state.settings.cooldown_hours) * 3_600_000;
                    if let Some(job) = state.job.as_mut() {
                        job.after_bytes = after;
                    }
                    state.baseline_bytes = after;
                    state.next_eligible_unix_ms = now().saturating_add(cooldown);
                    state.failures = 0;
                    finish_job(state, "succeeded", None, None);
                }
            }
        })?;
        let (service, router) = admin::configured_service(
            service,
            c.generation_options(&active),
            Some(c.control.clone()),
        )?;
        c.install(service, active, router);
        if c.snapshot().job.is_some() && c.recovery().is_none() {
            c.fence_recovery("selection_unresolved");
        }
        tokio::spawn(scheduler(c.clone()));
        Ok((
            Router::new().fallback(dispatch).with_state(c.clone()),
            ManagedHandle { coordinator: c },
        ))
    }

    /// Authenticated recovery mode: no database file is opened. Data routes
    /// answer `recovery_required`; sign-in and compaction status work.
    pub fn serve_recovery(self, code: &'static str) -> std::io::Result<(Router, ManagedHandle)> {
        let c = self.coordinator;
        c.fence_recovery(code);
        let placeholder = Database::create_in_memory(1, crate::CreateOptions::default())
            .map_err(std::io::Error::other)?;
        let service = Service::try_new(placeholder)?;
        let mut options = c.options.clone();
        options.database_path = None;
        options.auto_maintenance_interval = None;
        options.backup_interval = None;
        options.backup_retention_enabled = false;
        let (service, router) =
            admin::configured_service(service, options, Some(c.control.clone()))?;
        *c.generation
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Generation {
            router,
            service,
            path: PathBuf::new(),
        });
        Ok((
            Router::new().fallback(dispatch).with_state(c.clone()),
            ManagedHandle { coordinator: c },
        ))
    }
}

impl ManagedHandle {
    /// Stops the scheduler, asks an active pre-selection job to cancel and
    /// waits (bounded) for the job to finish.
    pub async fn shutdown(&self) {
        let c = &self.coordinator;
        c.stopping.store(true, Ordering::SeqCst);
        if c.active.load(Ordering::SeqCst) {
            let _ = c.job(|job| {
                if PRESELECTION.contains(&job.phase.as_str()) {
                    job.cancel_requested = true;
                }
            });
            let _ = tokio::time::timeout(Duration::from_secs(120), async {
                while c.active.load(Ordering::SeqCst) {
                    let notified = c.finished.notified();
                    if !c.active.load(Ordering::SeqCst) {
                        break;
                    }
                    let _ = tokio::time::timeout(Duration::from_millis(250), notified).await;
                }
            })
            .await;
        }
    }

    /// The current generation's service (close it after shutdown).
    pub fn current(&self) -> Option<Service> {
        self.coordinator.current().map(|(service, _)| service)
    }

    /// Active database path; `None` in recovery mode.
    pub fn path(&self) -> Option<PathBuf> {
        if self.coordinator.fence.load(Ordering::SeqCst) == RECOVERY {
            return None;
        }
        self.coordinator
            .current()
            .map(|(_, path)| path)
            .filter(|p| !p.as_os_str().is_empty())
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

// ------------------------------------------------------------- eligibility

#[cfg(target_os = "linux")]
fn unescape_mount(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && bytes[i + 1..i + 4]
                .iter()
                .all(|b| (b'0'..=b'7').contains(b))
        {
            let value =
                (bytes[i + 1] - b'0') * 64 + (bytes[i + 2] - b'0') * 8 + (bytes[i + 3] - b'0');
            out.push(value);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The filesystem type of the mount holding `path` (Linux mountinfo).
#[cfg(target_os = "linux")]
fn filesystem_type(path: &Path) -> Option<String> {
    let path = std::fs::canonicalize(path).ok()?;
    let info = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let mut best: Option<(usize, String)> = None;
    for line in info.lines() {
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let Some(mount) = left.split(' ').nth(4) else {
            continue;
        };
        let mount = unescape_mount(mount);
        if path.starts_with(&mount) && best.as_ref().is_none_or(|(len, _)| mount.len() >= *len) {
            best = Some((
                mount.len(),
                right.split(' ').next().unwrap_or("").to_owned(),
            ));
        }
    }
    best.map(|(_, fstype)| fstype)
}

/// Linux, local ext4, active file and admin directory on one filesystem.
fn filesystem_qualified(active: &Path, dir: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        let (Ok(a), Ok(b)) = (std::fs::metadata(active), std::fs::metadata(dir)) else {
            return false;
        };
        a.dev() == b.dev()
            && filesystem_type(active).as_deref() == Some("ext4")
            && filesystem_type(dir).as_deref() == Some("ext4")
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (active, dir);
        false
    }
}

#[cfg(unix)]
#[allow(clippy::unnecessary_cast)]
pub(super) fn free_bytes(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: `path` is a valid NUL-terminated string and `stat` is a plain
    // C struct that statvfs fully initializes on success.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    Some((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}

#[cfg(not(unix))]
pub(super) fn free_bytes(_dir: &Path) -> Option<u64> {
    None
}

fn reason(code: &'static str) -> Value {
    let message = match code {
        "managed_runtime_required" => {
            "Compaction needs the managed vlite-server runtime with an admin directory, a durable file and no gRPC listener."
        }
        "authentication_required" => {
            "Configure an administration token; compaction is unavailable in open (no-token) mode."
        }
        "encrypted_compaction_unqualified" => {
            "Encrypted databases are not qualified for compaction."
        }
        "unqualified_filesystem" => {
            "The database and admin directory must be on one local Linux ext4 filesystem (the qualified profile). Azure Files/SMB, bind mounts from other filesystems and Windows hosts are not qualified."
        }
        "memory_reservations_present" => {
            "Memory bindings or reserved memory resources are present; compaction would not preserve their reservation state."
        }
        "webhook_subscriptions_present" => {
            "Webhook subscriptions are configured; compaction does not preserve delivery state."
        }
        "import_state_unsupported" => "An import job is in progress; finish or abort it first.",
        "recovery_required" => {
            "The runtime is in recovery mode; resolve recovery before compacting."
        }
        "unsupported_source" => "Only format v5 database files are supported.",
        "source_too_large" => "The database file is larger than the qualified 1 GiB maximum.",
        "insufficient_space" => {
            "Free space must be at least four times the file size plus a safety margin."
        }
        "restore_selection_pending" => {
            "A restore is selected for next startup; restart or clear it before compacting."
        }
        "history_ack_required" => {
            "Acknowledge first that change history from before compaction is not kept: change-feed and replication readers must take a new snapshot."
        }
        "job_active" => "A compaction job is already running.",
        _ => "Compaction is unavailable.",
    };
    json!({"code": code, "message": message})
}

/// Plain disabled reasons; metadata only, never the database lock.
fn reasons(c: &Coordinator, service: &Service, path: &Path) -> Vec<&'static str> {
    let mut out = Vec::new();
    if c.recovery().is_some() {
        out.push("recovery_required");
        return out;
    }
    if service.admin.is_open() {
        out.push("authentication_required");
    }
    if service.encryption_key.is_some() {
        out.push("encrypted_compaction_unqualified");
    }
    if !test_profile() && !filesystem_qualified(path, &c.dir) {
        out.push("unqualified_filesystem");
    }
    if service.has_bindings() {
        out.push("memory_reservations_present");
    }
    if service.webhooks.has_subscriptions() {
        out.push("webhook_subscriptions_present");
    }
    if !service.imports.lock().is_ok_and(|r| r.is_empty()) {
        out.push("import_state_unsupported");
    }
    if service.format_version != 5 {
        out.push("unsupported_source");
    }
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if bytes > PROFILE_MAX {
        out.push("source_too_large");
    }
    if let Some(free) = free_bytes(&c.dir) {
        let need = bytes
            .saturating_mul(4)
            .saturating_add((64 * MIB).max(bytes / 10));
        if free < need {
            out.push("insufficient_space");
        }
    }
    if let Ok(mut marker) = std::fs::File::open(c.dir.join("next-startup")) {
        let mut name = String::new();
        let _ = (&mut marker).take(97).read_to_string(&mut name);
        if name != file_name(path) {
            out.push("restore_selection_pending");
        }
    }
    out
}

fn history_acknowledged(state: &Persisted) -> bool {
    state.settings.history_policy == "resnapshot_allowed"
        && state
            .ack
            .as_ref()
            .is_some_and(|a| Some(&a.lineage) == state.lineage.as_ref())
}

// --------------------------------------------------------------- schedule

/// Civil date (UTC) of a day count since 1970-01-01.
pub(super) fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// The window occurrence containing `now_ms`: (occurrence ID, ms left).
fn window(settings: &Settings, now_ms: u64) -> Option<(String, u64)> {
    let start = u64::from(parse_hhmm(&settings.daily_start_utc)?);
    let minute = (now_ms / 60_000) % 1440;
    let offset = (minute + 1440 - start) % 1440;
    let window = u64::from(settings.window_minutes);
    if offset >= window {
        return None;
    }
    let into = offset * 60_000 + now_ms % 60_000;
    let started = now_ms - into;
    let (y, m, d) = civil(i64::try_from(started / DAY_MS).ok()?);
    Some((format!("{y:04}-{m:02}-{d:02}"), window * 60_000 - into))
}

/// Next window start after `now_ms` (for display).
fn next_window(settings: &Settings, now_ms: u64) -> Option<u64> {
    let start = u64::from(parse_hhmm(&settings.daily_start_utc)?) * 60_000;
    let day = now_ms - now_ms % DAY_MS;
    let today = day + start;
    Some(if today > now_ms {
        today
    } else {
        today + DAY_MS
    })
}

async fn scheduler(c: Arc<Coordinator>) {
    let tick = if test_profile() {
        Duration::from_secs(1)
    } else {
        Duration::from_secs(30)
    };
    loop {
        tokio::time::sleep(tick).await;
        if c.stopping.load(Ordering::SeqCst) {
            return;
        }
        let _ = evaluate(&c);
    }
}

/// One scheduler evaluation; starts at most one job per occurrence.
fn evaluate(c: &Arc<Coordinator>) -> Option<()> {
    if c.active.load(Ordering::SeqCst) || c.fence.load(Ordering::SeqCst) != OPEN {
        return None;
    }
    let (service, path) = c.current()?;
    let state = c.snapshot();
    let settings = &state.settings;
    if !settings.enabled || state.suspended || !history_acknowledged(&state) {
        return None;
    }
    let at = now();
    let (occurrence, left) = window(settings, at)?;
    if state.last_occurrence.as_deref() == Some(occurrence.as_str())
        || left < u64::from(settings.max_run_seconds) * 1000 + 60_000
        || !reasons(c, &service, &path).is_empty()
    {
        return None;
    }
    // A clock moved backwards never postpones beyond one maximal cooldown.
    let horizon = at.saturating_add(720 * 3_600_000);
    if at < state.next_eligible_unix_ms.min(horizon) {
        return None;
    }
    let bytes = std::fs::metadata(&path).ok()?.len();
    if bytes < settings.min_file_bytes || bytes > settings.max_source_bytes {
        return None;
    }
    let Some(baseline) = state.baseline_bytes.filter(|&b| b <= bytes) else {
        let _ = c.update(|s| s.baseline_bytes = Some(bytes));
        return None;
    };
    let growth = bytes - baseline;
    let percent_ok = growth
        .checked_mul(100)
        .zip(baseline.checked_mul(u64::from(settings.min_growth_percent)))
        .is_some_and(|(g, b)| g >= b);
    if growth < settings.min_growth_bytes || !percent_ok {
        return None;
    }
    let _ = start(c, &service, &path, "scheduled", Some(occurrence), None);
    Some(())
}

fn job_id() -> String {
    let mut bytes = [0u8; 6];
    let _ = getrandom::fill(&mut bytes);
    format!("cj-{}-{}", now(), hex(&bytes))
}

/// Durably accepts a job and spawns its pipeline.
fn start(
    c: &Arc<Coordinator>,
    service: &Service,
    path: &Path,
    trigger: &str,
    occurrence: Option<String>,
    key: Option<Key>,
) -> Result<Job, ApiError> {
    let _selection = c.selection.lock().map_err(internal)?;
    if c.active.load(Ordering::SeqCst) {
        return Err(conflict(
            "compaction_busy",
            "a compaction job is already running",
        ));
    }
    let at = now();
    let mut accepted = None;
    let id = key.as_ref().map_or_else(job_id, |k| k.job.clone());
    c.update(|state| {
        let job = Job {
            id: id.clone(),
            trigger: trigger.into(),
            occurrence: occurrence.clone(),
            phase: "preflight".into(),
            created_unix_ms: at,
            updated_unix_ms: at,
            deadline_unix_ms: at + u64::from(state.settings.max_run_seconds) * 1000,
            selection: "not_published".into(),
            settings_version: state.settings_version,
            source: file_name(path),
            lineage: state.lineage.clone(),
            before_bytes: std::fs::metadata(path).map(|m| m.len()).ok(),
            ..Job::default()
        };
        if occurrence.is_some() {
            state.last_occurrence = occurrence.clone();
        }
        if let Some(key) = key.clone() {
            state.keys.insert(0, key);
            state.keys.truncate(KEYS);
        }
        state.job = Some(job.clone());
        accepted = Some(job);
    })
    .map_err(|_| {
        // Not durably accepted: forget it.
        c.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .job = None;
        internal("compaction state write failed")
    })?;
    let job = accepted.ok_or_else(|| internal("job"))?;
    c.active.store(true, Ordering::SeqCst);
    c.audit(json!({"event":"compaction_accepted","job":job.id,"trigger":trigger}));
    let runner = c.clone();
    let template = service.clone();
    tokio::spawn(async move {
        pipeline(&runner, &template).await;
        let finished = runner.snapshot().history.first().cloned();
        if let Some(job) = finished {
            runner.audit(json!({"event":"compaction_finished","job":job.id,
                "outcome":job.outcome,"code":job.code,
                "before_bytes":job.before_bytes,"after_bytes":job.after_bytes}));
        }
        runner.active.store(false, Ordering::SeqCst);
        runner.finished.notify_waiters();
    });
    Ok(job)
}

fn conflict(code: &'static str, message: &'static str) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, code, message)
}

// ---------------------------------------------------------------- pipeline

enum Exit {
    /// Pre-selection exit: reconstruct the source, then finish.
    Source {
        outcome: &'static str,
        code: Option<&'static str>,
        action: &'static str,
        baseline: Option<u64>,
    },
}

async fn wait_drained(c: &Coordinator, budget: Duration) -> bool {
    tokio::time::timeout(budget, async {
        loop {
            let notified = c.drained.notified();
            if c.tickets.load(Ordering::SeqCst) == 0 {
                return;
            }
            let _ = tokio::time::timeout(Duration::from_millis(100), notified).await;
        }
    })
    .await
    .is_ok()
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> std::io::Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(std::io::Error::other)
}

async fn pipeline(c: &Arc<Coordinator>, template: &Service) {
    let Some((_, source)) = c.current() else {
        return;
    };
    let durability = c
        .durability
        .lock()
        .ok()
        .and_then(|d| *d)
        .unwrap_or_default();
    // QUIESCING: close admission, drain tickets and every worker permit.
    let _ = c.job(|job| job.phase = "quiescing".into());
    c.fence.store(MAINTENANCE, Ordering::SeqCst);
    let started = std::time::Instant::now();
    let drained = wait_drained(c, DRAIN).await;
    let retired = if drained {
        template
            .retire(DRAIN.saturating_sub(started.elapsed()))
            .await
    } else {
        Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "drain"))
    };
    let database = match retired {
        Ok(database) => database,
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
            // Nothing changed: the generation is intact; reopen admission.
            let _ = c.update(|state| {
                finish_job(
                    state,
                    "failed",
                    Some("drain_timeout"),
                    Some("In-flight work did not finish within 30 seconds; nothing changed. Retry when traffic is lower."),
                );
            });
            c.fence.store(OPEN, Ordering::SeqCst);
            return;
        }
        Err(_) => {
            recover_source(
                c,
                template,
                None,
                &source,
                durability,
                failed("quiesce_failed"),
            )
            .await;
            return;
        }
    };
    match build(c, database, &source).await {
        Ok((database, job)) => select(c, template, database, &source, durability, job).await,
        Err((database, exit)) => {
            recover_source(c, template, database, &source, durability, exit).await
        }
    }
}

/// Server-log detail for an operator (never returned through the API).
fn note(code: &str, detail: &str) {
    logs::stderr(
        "error",
        "compaction",
        json!({"event":"compaction_error","code":code,"detail":detail}).to_string(),
    );
}

fn failed(code: &'static str) -> Exit {
    Exit::Source {
        outcome: "failed",
        code: Some(code),
        action: "The source file stayed authoritative and serving resumed. Inspect the server log, then retry.",
        baseline: None,
    }
}

#[allow(clippy::result_large_err)]
/// FREEZING, BUILDING, VALIDATING, READY_TO_SELECT. Returns the still-owned
/// source and the job ready for selection, or a pre-selection exit.
async fn build(
    c: &Arc<Coordinator>,
    database: Database,
    source: &Path,
) -> Result<(Database, Job), (Option<Database>, Exit)> {
    let cancelled = Exit::Source {
        outcome: "cancelled",
        code: Some("cancelled"),
        action: "Cancelled before selection; the source file stayed authoritative.",
        baseline: None,
    };
    if c.cancel_requested() {
        return Err((Some(database), cancelled));
    }
    let id = c.snapshot().job.map(|j| j.id).unwrap_or_default();
    let recovery_name = format!("recovery-{id}.vlite");
    let candidate_name = admin::name("restore-");
    let recovery_name_for_job = recovery_name.clone();
    let candidate_for_job = candidate_name.clone();
    // Names are durable before the files exist.
    if c.job(|job| {
        job.phase = "freezing".into();
        job.recovery = Some(recovery_name_for_job);
        job.candidate = Some(candidate_for_job);
    })
    .is_err()
    {
        return Err((Some(database), failed("state_write_failed")));
    }
    let work = c.work.clone();
    let frozen_path = work.join(&recovery_name);
    let frozen = frozen_path.clone();
    let source_path = source.to_path_buf();
    let freezing = blocking(move || {
        let mut database = database;
        let result = (|| -> Result<u64, ApiError> {
            let (_, staged) = super::ops::snapshot(&mut database, &work)?;
            let verified = crate::maintenance::verify(staged.path())?;
            if verified.inspection.incomplete_tail_bytes != 0 {
                return Err(internal("frozen copy has an incomplete tail"));
            }
            staged.persist_noclobber(&frozen).map_err(internal)?;
            admin::sync_directory(&work)?;
            Ok(std::fs::metadata(&source_path).map_err(internal)?.len())
        })();
        (database, result)
    })
    .await;
    let (database, before) = match freezing {
        Ok((database, Ok(bytes))) => (database, bytes),
        Ok((database, Err(_))) => return Err((Some(database), failed("freeze_failed"))),
        Err(_) => return Err((None, failed("freeze_failed"))),
    };
    crash_point("crash_after_freeze");
    if c.cancel_requested() {
        return Err((Some(database), cancelled));
    }
    let _ = c.job(|job| {
        job.phase = "building".into();
        job.before_bytes = Some(before);
    });
    let staging = c.work.join(format!("staging-{id}"));
    let candidate = c.dir.join(&candidate_name);
    let (frozen, staging_for_build, candidate_for_build) =
        (frozen_path.clone(), staging.clone(), candidate.clone());
    let built = blocking(move || {
        if fault("slow_build") {
            std::thread::sleep(Duration::from_secs(4));
        }
        if fault("very_slow_build") {
            // Long enough for a browser test to observe the build phase between 5 s polls.
            std::thread::sleep(Duration::from_secs(15));
        }
        if fault("build_fail") {
            return Err(crate::maintenance::compaction::TransformError::Failed(
                crate::Error::Io(std::io::Error::other("injected build failure")),
            ));
        }
        let result = crate::maintenance::compaction::transform(
            &frozen,
            &staging_for_build,
            &candidate_for_build,
        );
        remove_quietly(&staging_for_build);
        if result.is_ok()
            && let Err(error) = std::fs::OpenOptions::new()
                .read(true)
                .write(cfg!(windows))
                .open(&candidate_for_build)
                .and_then(|f| f.sync_all())
        {
            {
                return Err(crate::maintenance::compaction::TransformError::Failed(
                    crate::Error::Io(error),
                ));
            }
        }
        result
    })
    .await;
    let transformed = match built {
        Ok(Ok(transformed)) => transformed,
        Ok(Err(crate::maintenance::compaction::TransformError::Refused(refusal))) => {
            return Err((
                Some(database),
                Exit::Source {
                    outcome: "failed",
                    code: Some(refusal.code),
                    action: "This file holds state that compaction cannot preserve yet; the source stayed authoritative.",
                    baseline: None,
                },
            ));
        }
        Ok(Err(crate::maintenance::compaction::TransformError::Mismatch(detail))) => {
            note("validation_failed", &detail);
            return Err((Some(database), failed("validation_failed")));
        }
        Ok(Err(crate::maintenance::compaction::TransformError::Failed(error))) => {
            note("build_failed", &error.to_string());
            return Err((Some(database), failed("build_failed")));
        }
        Err(error) => {
            note("build_failed", &error.to_string());
            return Err((Some(database), failed("build_failed")));
        }
    };
    crash_point("crash_after_build");
    let _ = c.job(|job| job.phase = "validating".into());
    let after = std::fs::metadata(&candidate)
        .map(|m| m.len())
        .unwrap_or(u64::MAX);
    let floor = if test_profile() {
        1
    } else {
        (16 * MIB).max(before.div_ceil(20))
    };
    if before.saturating_sub(after) < floor {
        let _ = c.job(|job| job.after_bytes = Some(after));
        return Err((
            Some(database),
            Exit::Source {
                outcome: "skipped",
                code: Some("no_material_savings"),
                action: "The rewritten file would not be materially smaller; nothing changed. The schedule waits for more growth.",
                baseline: Some(before),
            },
        ));
    }
    logs::stderr(
        "info",
        "compaction",
        json!({"event":"compaction_candidate_built","before_sequence":transformed.before.sequence,
            "after_sequence":transformed.after.sequence,"before_bytes":before,"after_bytes":after})
        .to_string(),
    );
    let _ = c.job(|job| {
        job.phase = "ready_to_select".into();
        job.after_bytes = Some(after);
        job.lineage = Some(format!("{:016x}", transformed.after.lineage));
        job.sequence = Some(transformed.after.sequence);
    });
    if c.cancel_requested() {
        return Err((Some(database), cancelled));
    }
    let job = c.snapshot().job.unwrap_or_default();
    if now() > job.deadline_unix_ms {
        return Err((
            Some(database),
            Exit::Source {
                outcome: "failed",
                code: Some("maintenance_budget_exceeded"),
                action: "The run exceeded max_run_seconds before selection; the source stayed authoritative.",
                baseline: None,
            },
        ));
    }
    if job.trigger == "scheduled" {
        let settings = c.snapshot().settings;
        if window(&settings, now()).is_none() {
            return Err((
                Some(database),
                Exit::Source {
                    outcome: "failed",
                    code: Some("window_closed"),
                    action: "The UTC window closed before selection; the source stayed authoritative.",
                    baseline: None,
                },
            ));
        }
    }
    Ok((database, job))
}

/// SELECTING -> COMMITTED -> REOPENING -> SUCCEEDED. Never reopens the source.
async fn select(
    c: &Arc<Coordinator>,
    template: &Service,
    database: Database,
    source: &Path,
    durability: Durability,
    job: Job,
) {
    let dir = c.dir.clone();
    let candidate = dir.join(job.candidate.clone().unwrap_or_default());
    enum Decision {
        Cancelled,
        IntentFailed,
        Unknown,
        Committed,
    }
    let decision = {
        let _selection = c
            .selection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if c.cancel_requested() {
            Decision::Cancelled
        } else if c.job(|job| job.phase = "selecting".into()).is_err() {
            // The intent may or may not be durable: never reopen the source.
            Decision::IntentFailed
        } else {
            crash_point("crash_after_intent");
            let mut current = c.snapshot().job.unwrap_or_default();
            current.lineage = job.lineage.clone();
            match write_selection(&dir, &current) {
                Ok(()) => Decision::Committed,
                Err(_) => {
                    let _ = c.job(|job| {
                        job.selection = "outcome_unknown".into();
                        job.phase = "selection_outcome_unknown".into();
                        job.code = Some("selection_outcome_unknown".into());
                    });
                    Decision::Unknown
                }
            }
        }
    };
    match decision {
        Decision::Committed => {}
        Decision::Cancelled => {
            let exit = Exit::Source {
                outcome: "cancelled",
                code: Some("cancelled"),
                action: "Cancelled before selection; the source file stayed authoritative.",
                baseline: None,
            };
            recover_source(c, template, Some(database), source, durability, exit).await;
            return;
        }
        Decision::IntentFailed | Decision::Unknown => {
            let code = if matches!(decision, Decision::IntentFailed) {
                "selection_intent_uncertain"
            } else {
                "selection_outcome_unknown"
            };
            c.fence_recovery(code);
            let _ = blocking(move || database.close()).await;
            return;
        }
    }
    let _ = c.job(|job| {
        job.selection = "committed".into();
        job.phase = "committed".into();
    });
    crash_point("crash_after_selection");
    if let Some(name) = &job.candidate {
        let _ = write_marker(&dir, name);
    }
    let _ = blocking(move || database.close()).await;
    let _ = c.job(|job| job.phase = "reopening".into());
    let reopened = blocking({
        let candidate = candidate.clone();
        move || -> crate::Result<Database> {
            if fault("reopen_fail") {
                return Err(crate::Error::Io(std::io::Error::other(
                    "injected reopen failure",
                )));
            }
            Database::open(&candidate)?.with_durability(durability)
        }
    })
    .await;
    let rebuilt = match reopened {
        Ok(Ok(database)) => c
            .rebuild(template, database, &candidate)
            .map_err(|_| "reopen_failed"),
        _ => Err("reopen_failed"),
    };
    match rebuilt {
        Ok(()) => {
            let after = std::fs::metadata(&candidate).map(|m| m.len()).ok();
            let _ = c.update(|state| {
                let cooldown = u64::from(state.settings.cooldown_hours) * 3_600_000;
                if let Some(job) = state.job.as_mut() {
                    job.after_bytes = after.or(job.after_bytes);
                }
                state.baseline_bytes = after;
                state.next_eligible_unix_ms = now().saturating_add(cooldown);
                state.failures = 0;
                finish_job(state, "succeeded", None, None);
            });
            c.fence.store(OPEN, Ordering::SeqCst);
        }
        Err(code) => {
            let _ = c.update(|state| {
                state.suspended = true;
                finish_job(
                    state,
                    "failed",
                    Some(code),
                    Some("The compacted file is selected but did not reopen. Restart the server; startup reopens the selected file. The previous file is retained."),
                );
                if let Some(job) = state.history.first_mut() {
                    job.selection = "committed".into();
                }
            });
            c.fence_recovery(code);
        }
    }
}

/// RECOVERING_SOURCE: a fresh generation around the untouched source.
async fn recover_source(
    c: &Arc<Coordinator>,
    template: &Service,
    database: Option<Database>,
    source: &Path,
    durability: Durability,
    exit: Exit,
) {
    let Exit::Source {
        outcome,
        code,
        action,
        baseline,
    } = exit;
    let _ = c.job(|job| job.phase = "recovering_source".into());
    if let Some(database) = database {
        let _ = blocking(move || database.close()).await;
    }
    let path = source.to_path_buf();
    let reopened = blocking(move || Database::open(&path)?.with_durability(durability)).await;
    let rebuilt = match reopened {
        Ok(Ok(database)) => c.rebuild(template, database, source).is_ok(),
        _ => false,
    };
    let job = c.snapshot().job;
    if let Some(job) = &job {
        cleanup(&c.dir, &c.work, job);
    }
    if !rebuilt {
        let _ = c.update(|state| {
            finish_job(
                state,
                "failed",
                Some("source_open_failed"),
                Some("The source file could not be reopened. Restart the server; no file was selected."),
            );
        });
        c.fence_recovery("source_open_failed");
        return;
    }
    let _ = c.update(|state| {
        if let Some(bytes) = baseline {
            state.baseline_bytes = Some(bytes);
            let cooldown = u64::from(state.settings.cooldown_hours) * 3_600_000;
            state.next_eligible_unix_ms = now().saturating_add(cooldown);
            state.failures = 0;
        }
        finish_job(state, outcome, code, Some(action));
    });
    c.fence.store(OPEN, Ordering::SeqCst);
}

// ------------------------------------------------------------------ routes

pub(super) fn routes() -> Router<Service> {
    Router::new()
        .route("/v1/admin/compaction", get(status))
        .route("/v1/admin/compaction/config", put(configure))
        .route("/v1/admin/compaction/jobs", post(submit))
        .route("/v1/admin/compaction/jobs/{id}", get(job_status))
        .route("/v1/admin/compaction/jobs/{id}/cancel", post(cancel))
}

fn coordinator(service: &Service) -> Result<Arc<Coordinator>, ApiError> {
    service.admin.compaction.get().cloned().ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "managed_runtime_required",
            "compaction needs the managed vlite-server runtime",
        )
    })
}

fn job_json(job: &Job) -> Value {
    let mut value = serde_json::to_value(job).unwrap_or(Value::Null);
    if let (Some(before), Some(after)) = (job.before_bytes, job.after_bytes) {
        value["saved_bytes"] = json!(before.saturating_sub(after));
    }
    value
}

fn retained(work: &Path) -> (u64, usize) {
    let mut bytes = 0;
    let mut count = 0;
    if let Ok(entries) = std::fs::read_dir(work) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("recovery-") && name.ends_with(".vlite") {
                bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
                count += 1;
            }
        }
    }
    (bytes, count)
}

async fn status(State(service): State<Service>) -> Json<Value> {
    let Some(c) = service.admin.compaction.get().cloned() else {
        return Json(json!({"capability":false,"profile":PROFILE,
            "reasons":[reason("managed_runtime_required")],"settings":null,"job":null}));
    };
    let (current, path) = c.current().unwrap_or((service.clone(), PathBuf::new()));
    let state = c.snapshot();
    let at = now();
    let mut list = reasons(&c, &current, &path);
    let acknowledged = history_acknowledged(&state);
    if !acknowledged {
        list.push("history_ack_required");
    }
    if state.job.is_some() {
        list.push("job_active");
    }
    let bytes = std::fs::metadata(&path).map(|m| m.len()).ok();
    let growth = bytes
        .zip(state.baseline_bytes)
        .map(|(b, base)| b.saturating_sub(base));
    let (retained_bytes, retained_copies) = retained(&c.work);
    let selection = read_selection(&c.dir).ok().flatten();
    let blocking: Vec<&str> = list
        .iter()
        .copied()
        .filter(|r| *r != "history_ack_required" && *r != "job_active")
        .collect();
    Json(json!({
        "capability": true,
        "profile": PROFILE,
        "test_profile": test_profile(),
        "available": blocking.is_empty(),
        "reasons": list.iter().map(|r| reason(r)).collect::<Vec<_>>(),
        "file": {"name": file_name(&path), "bytes": bytes, "sampled_unix_ms": at},
        "settings": state.settings,
        "settings_version": state.settings_version,
        "history_acknowledged": acknowledged,
        "schedule": {
            "enabled": state.settings.enabled,
            "suspended": state.suspended,
            "failures": state.failures,
            "baseline_bytes": state.baseline_bytes,
            "growth_bytes": growth,
            "next_window_unix_ms": next_window(&state.settings, at),
            "in_window": window(&state.settings, at).is_some(),
            "next_eligible_unix_ms": state.next_eligible_unix_ms,
            "last_occurrence": state.last_occurrence,
        },
        "job": state.job.as_ref().map(job_json),
        "last_job": state.history.first().map(job_json),
        "phase": state.job.as_ref().map(|j| j.phase.clone()),
        "fence": match c.fence.load(Ordering::SeqCst) { OPEN => "open", MAINTENANCE => "maintenance", _ => "recovery" },
        "recovery": {"required": c.recovery().is_some(), "code": c.recovery(),
            "retained_bytes": retained_bytes, "retained_copies": retained_copies},
        "selection": selection.map(|s| json!({"candidate": s.candidate, "epoch": s.epoch, "job": s.job})),
        "warning": "Compaction briefly stops serving (reads and writes answer 503 maintenance_in_progress), keeps the previous file and a recovery copy, drops change history from before the run (change-feed and replication readers must take a new snapshot), may make graph clients re-read revisions before their next conditional write, and pins restore selection to the compacted file afterwards.",
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigInput {
    expected_version: u64,
    settings: Settings,
    #[serde(default)]
    acknowledge_history: bool,
}

async fn configure(
    State(service): State<Service>,
    Extension(role): Extension<Role>,
    principal: Option<Extension<users::Principal>>,
    input: Result<Json<ConfigInput>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let c = coordinator(&service)?;
    let input = body(input)?;
    input.settings.validate().map_err(|message| {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_settings",
            message,
        )
    })?;
    let (current, path) = c.current().unwrap_or((service.clone(), PathBuf::new()));
    let blocking = reasons(&c, &current, &path);
    let actor = audit::actor(&role, principal.as_ref().map(|Extension(p)| p));
    let mut outcome = Ok(());
    let mut version = 0;
    let result = {
        let snapshot = c.snapshot();
        if snapshot.settings_version != input.expected_version {
            return Err(conflict(
                "settings_version_conflict",
                "the settings changed meanwhile; reload and try again",
            ));
        }
        let acknowledged = input.acknowledge_history
            || (history_acknowledged(&snapshot)
                && snapshot.settings.history_policy == input.settings.history_policy);
        if input.settings.enabled {
            if input.settings.history_policy != "resnapshot_allowed" || !acknowledged {
                return Err(conflict(
                    "history_ack_required",
                    "enabling the schedule needs history_policy resnapshot_allowed and acknowledge_history",
                ));
            }
            if let Some(first) = blocking.first() {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    first,
                    reason(first)["message"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                ));
            }
        }
        c.update(|state| {
            if state.settings_version != input.expected_version {
                outcome = Err(());
                return;
            }
            state.settings = input.settings.clone();
            state.settings_version += 1;
            version = state.settings_version;
            state.suspended = false;
            state.failures = 0;
            if input.acknowledge_history && input.settings.history_policy == "resnapshot_allowed" {
                state.ack = state.lineage.clone().map(|lineage| Ack {
                    lineage,
                    unix_ms: now(),
                });
            } else if input.settings.history_policy != "resnapshot_allowed" {
                state.ack = None;
            }
        })
    };
    if outcome.is_err() {
        return Err(conflict(
            "settings_version_conflict",
            "the settings changed meanwhile; reload and try again",
        ));
    }
    result.map_err(internal)?;
    c.audit(json!({"event":"compaction_settings_changed","actor":actor,
        "settings_version":version,"enabled":input.settings.enabled}));
    Ok(status(State(service)).await)
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct JobInput {
    expected_version: u64,
    acknowledge_downtime: bool,
    acknowledge_history: bool,
    idempotency_key: String,
}

fn uuid(text: &str) -> bool {
    text.len() == 36
        && text.char_indices().all(|(i, ch)| match i {
            8 | 13 | 18 | 23 => ch == '-',
            _ => ch.is_ascii_hexdigit(),
        })
}

async fn submit(
    State(service): State<Service>,
    Extension(role): Extension<Role>,
    principal: Option<Extension<users::Principal>>,
    input: Result<Json<JobInput>, JsonRejection>,
) -> Result<Response, ApiError> {
    let c = coordinator(&service)?;
    let input = body(input)?;
    if !uuid(&input.idempotency_key) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
            "idempotency_key must be a UUID",
        ));
    }
    let body_digest = digest(&serde_json::to_vec(&input).map_err(internal)?);
    let state = c.snapshot();
    if let Some(previous) = state.keys.iter().find(|k| k.key == input.idempotency_key) {
        if previous.digest != body_digest {
            return Err(conflict(
                "idempotency_key_reused",
                "this idempotency key was used with a different request",
            ));
        }
        let job = state
            .job
            .iter()
            .chain(state.history.iter())
            .find(|j| j.id == previous.job)
            .map(job_json);
        return Ok((
            StatusCode::ACCEPTED,
            Json(json!({"job": job, "replayed": true})),
        )
            .into_response());
    }
    if !input.acknowledge_downtime || !input.acknowledge_history {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "acknowledgement_required",
            "acknowledge the brief outage and the loss of change history from before compaction first",
        ));
    }
    if state.settings_version != input.expected_version {
        return Err(conflict(
            "settings_version_conflict",
            "the settings changed meanwhile; reload and try again",
        ));
    }
    let (current, path) = c.current().ok_or_else(|| internal("no generation"))?;
    if let Some(first) = reasons(&c, &current, &path).first() {
        let status = if *first == "recovery_required" {
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            StatusCode::CONFLICT
        };
        return Err(ApiError::new(
            status,
            first,
            reason(first)["message"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        ));
    }
    let actor = audit::actor(&role, principal.as_ref().map(|Extension(p)| p));
    let key = Key {
        key: input.idempotency_key.clone(),
        digest: body_digest,
        job: job_id(),
    };
    let job = start(&c, &current, &path, "manual", None, Some(key))?;
    c.audit(json!({"event":"compaction_requested","actor":actor,"job":job.id}));
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"job": job_json(&job), "replayed": false})),
    )
        .into_response())
}

fn find(c: &Coordinator, id: &str) -> Option<Job> {
    let state = c.snapshot();
    state
        .job
        .iter()
        .chain(state.history.iter())
        .find(|j| j.id == id)
        .cloned()
}

async fn job_status(
    State(service): State<Service>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<Value>, ApiError> {
    let c = coordinator(&service)?;
    find(&c, &id)
        .map(|job| Json(json!({"job": job_json(&job)})))
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "not_found", "unknown compaction job"))
}

async fn cancel(
    State(service): State<Service>,
    Extension(role): Extension<Role>,
    principal: Option<Extension<users::Principal>>,
    UrlPath(id): UrlPath<String>,
) -> Result<Response, ApiError> {
    let c = coordinator(&service)?;
    let Some(job) = find(&c, &id) else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "unknown compaction job",
        ));
    };
    let _selection = c.selection.lock().map_err(internal)?;
    let state = c.snapshot();
    let active = state.job.as_ref().filter(|j| j.id == id);
    let Some(active) = active else {
        return Err(conflict("job_finished", "the job already finished"));
    };
    if !PRESELECTION.contains(&active.phase.as_str()) || active.phase == "recovering_source" {
        let code = match active.selection.as_str() {
            "committed" => "selection_committed",
            "outcome_unknown" => "selection_outcome_unknown",
            _ => "selection_in_progress",
        };
        return Err(conflict(
            code,
            "the job is past the point where cancellation is safe",
        ));
    }
    c.job(|job| job.cancel_requested = true).map_err(internal)?;
    let actor = audit::actor(&role, principal.as_ref().map(|Extension(p)| p));
    c.audit(json!({"event":"compaction_cancel_requested","actor":actor,"job":job.id}));
    let job = find(&c, &id).unwrap_or(job);
    Ok((StatusCode::ACCEPTED, Json(json!({"job": job_json(&job)}))).into_response())
}
