//! Bounded background operations (F6 automatic maintenance, F7 scheduled
//! backups with manifests, verification, drills and retention).
//!
//! One worker thread, started only when something is enabled. Every job takes
//! a database worker permit first (busy → skipped and counted), never switches
//! the active file and never migrates. `Service::close` stops and joins it.
use super::*;
use crate::{HnswMaintenancePolicy, HnswPressure, HnswRebuildStatus};
use std::{
    collections::BTreeMap,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path as FilePath, PathBuf},
    sync::{Condvar, Weak},
    thread::JoinHandle,
    time::Duration,
};

const MANIFEST_LIMIT: u64 = 64 * 1024;
const DAY_MS: u64 = 86_400_000;
type AutoTickOutcome = (
    &'static str,
    Option<String>,
    Option<&'static str>,
    Option<u16>,
);

#[derive(Clone)]
struct AutoConfig {
    enabled: bool,
    interval: Duration,
    retry_backoff_initial: Duration,
    retry_backoff_max: Duration,
    stale_ratio: f64,
    rebuild_on_low_recall: bool,
    min_recall: f64,
    quality_samples: usize,
    limits: admin::Limits,
}

impl Default for AutoConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval: Duration::from_secs(60),
            retry_backoff_initial: Duration::ZERO,
            retry_backoff_max: Duration::ZERO,
            stale_ratio: 0.2,
            rebuild_on_low_recall: false,
            min_recall: 0.9,
            quality_samples: 8,
            limits: admin::Limits::default(),
        }
    }
}

#[derive(Default)]
struct AutoState {
    ticks: u64,
    skipped_busy: u64,
    rebuilds_started: u64,
    rebuilds_published: u64,
    errors: u64,
    last_tick_unix_ms: Option<u64>,
    last_collection: Option<String>,
    last_action: Option<&'static str>,
    last_error: Option<String>,
    retries: BTreeMap<u16, RetryState>,
}

#[derive(Clone, Copy, Default)]
struct RetryState {
    attempts: u32,
    until: Option<Instant>,
}

#[derive(Clone)]
struct BackupConfig {
    enabled: bool,
    interval: Duration,
    retention_enabled: bool,
    retain_last: usize,
    retain_days: u64,
    drill_every: u64,
    drill_max_bytes: u64,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval: Duration::from_secs(86_400),
            retention_enabled: false,
            retain_last: 7,
            retain_days: 0,
            drill_every: 1,
            drill_max_bytes: 4 * 1024 * 1024 * 1024,
        }
    }
}

#[derive(Default)]
struct BackupState {
    runs: u64,
    failures: u64,
    skipped_busy: u64,
    last_success_unix_ms: Option<u64>,
    last_failure_unix_ms: Option<u64>,
    last_error: Option<String>,
    last_backup: Option<String>,
    drills: u64,
    drill_failures: u64,
    last_drill_unix_ms: Option<u64>,
    retention_deleted: u64,
    retained: Option<u64>,
    verify_failures: u64,
}

#[derive(Default)]
struct Inner {
    stopped: bool,
    thread: Option<JoinHandle<()>>,
    directory: Option<PathBuf>,
    active: Option<PathBuf>,
    audit: Option<Arc<audit::Log>>,
    auto: AutoConfig,
    auto_state: AutoState,
    next_auto: Option<Instant>,
    backup: BackupConfig,
    backup_state: BackupState,
    next_backup: Option<Instant>,
    cursor: Option<u16>,
    spawn_error: Option<String>,
}

pub(super) struct Ops {
    database: Weak<RwLock<Option<Database>>>,
    workers: Arc<Semaphore>,
    metrics: Arc<Metrics>,
    inner: Mutex<Inner>,
    wake: Condvar,
}

/// Explicit options; everything is off unless set.
#[derive(Clone, Default)]
pub(super) struct Schedule {
    pub auto_interval: Option<Duration>,
    pub backup_interval: Option<Duration>,
    pub retention_enabled: bool,
    pub retain_last: Option<usize>,
    pub retain_days: Option<u64>,
    pub drill_every: Option<u64>,
    pub drill_max_bytes: Option<u64>,
}

fn now_ms() -> u64 {
    admin::now_ms() as u64
}

fn retry_delay(initial: Duration, maximum: Duration, attempts: u32) -> Duration {
    if initial.is_zero() || maximum.is_zero() {
        return Duration::ZERO;
    }
    initial
        .saturating_mul(1u32 << attempts.saturating_sub(1).min(31))
        .min(maximum)
}

fn lock<T>(mutex: &Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>, ApiError> {
    mutex.lock().map_err(internal)
}

impl Ops {
    pub(super) fn new(
        database: Weak<RwLock<Option<Database>>>,
        workers: Arc<Semaphore>,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            database,
            workers,
            metrics,
            inner: Mutex::new(Inner::default()),
            wake: Condvar::new(),
        }
    }

    /// Applies startup configuration. Validated by `ServerOptions::validate`.
    pub(super) fn configure(
        self: &Arc<Self>,
        directory: Option<PathBuf>,
        active: Option<PathBuf>,
        audit: Option<Arc<audit::Log>>,
        schedule: &Schedule,
    ) -> Result<(), ApiError> {
        let mut inner = lock(&self.inner)?;
        inner.directory = directory;
        inner.active = active;
        inner.audit = audit;
        let now = Instant::now();
        if let Some(interval) = schedule.auto_interval {
            inner.auto.enabled = true;
            inner.auto.interval = interval;
            inner.next_auto = Some(now + interval);
        }
        if let Some(interval) = schedule.backup_interval {
            inner.backup.enabled = true;
            inner.backup.interval = interval;
            inner.next_backup = Some(now + interval);
        }
        inner.backup.retention_enabled = schedule.retention_enabled;
        if let Some(n) = schedule.retain_last {
            inner.backup.retain_last = n;
        }
        if let Some(days) = schedule.retain_days {
            inner.backup.retain_days = days;
        }
        if let Some(every) = schedule.drill_every {
            inner.backup.drill_every = every;
        }
        if let Some(bytes) = schedule.drill_max_bytes {
            inner.backup.drill_max_bytes = bytes;
        }
        self.ensure_thread(&mut inner);
        Ok(())
    }

    fn ensure_thread(self: &Arc<Self>, inner: &mut Inner) {
        if inner.stopped || inner.thread.is_some() || !(inner.auto.enabled || inner.backup.enabled)
        {
            self.wake.notify_all();
            return;
        }
        let ops = self.clone();
        match std::thread::Builder::new()
            .name("vlite-ops".into())
            .spawn(move || ops.run_loop())
        {
            Ok(handle) => inner.thread = Some(handle),
            Err(error) => {
                inner.spawn_error = Some(error.to_string());
                super::logs::stderr(
                    "error",
                    "maintenance",
                    json!({"event":"ops_thread_spawn_failed"}).to_string(),
                );
            }
        }
    }

    /// Stops and joins the worker. Idempotent; nothing starts afterwards.
    pub(super) fn shutdown(&self) {
        let handle = match self.inner.lock() {
            Ok(mut inner) => {
                inner.stopped = true;
                inner.thread.take()
            }
            Err(_) => None,
        };
        self.wake.notify_all();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }

    fn run_loop(self: Arc<Self>) {
        loop {
            let (auto, backup) = {
                let Ok(mut inner) = self.inner.lock() else {
                    return;
                };
                loop {
                    if inner.stopped || self.database.strong_count() == 0 {
                        return;
                    }
                    let now = Instant::now();
                    let auto = inner.auto.enabled && inner.next_auto.is_some_and(|t| t <= now);
                    let backup =
                        inner.backup.enabled && inner.next_backup.is_some_and(|t| t <= now);
                    if auto || backup {
                        if auto {
                            inner.next_auto = Some(now + inner.auto.interval);
                        }
                        if backup {
                            inner.next_backup = Some(now + inner.backup.interval);
                        }
                        break (auto, backup);
                    }
                    let next = [
                        inner.next_auto.filter(|_| inner.auto.enabled),
                        inner.next_backup.filter(|_| inner.backup.enabled),
                    ]
                    .into_iter()
                    .flatten()
                    .min();
                    let wait = next
                        .map_or(Duration::from_millis(500), |t| {
                            t.saturating_duration_since(now)
                        })
                        .min(Duration::from_millis(500));
                    inner = match self.wake.wait_timeout(inner, wait) {
                        Ok((guard, _)) => guard,
                        Err(_) => return,
                    };
                }
            };
            if auto {
                self.scheduled_tick();
            }
            if backup {
                let _ = self.scheduled_backup();
            }
        }
    }

    fn with_database<T>(
        &self,
        operation: impl FnOnce(&mut Database) -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        let database = self.database.upgrade().ok_or_else(unavailable)?;
        let mut owner = database.write().map_err(internal)?;
        let db = owner.as_mut().ok_or_else(unavailable)?;
        operation(db)
    }

    fn scheduled_tick(&self) {
        let Ok(_permit) = self.workers.clone().try_acquire_owned() else {
            if let Ok(mut inner) = self.inner.lock() {
                inner.auto_state.skipped_busy += 1;
            }
            return;
        };
        if let Err(error) = self.with_database(|db| Ok(self.auto_tick(db))) {
            self.auto_failed(&error);
        }
    }

    fn auto_failed(&self, error: &ApiError) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.auto_state.errors += 1;
            inner.auto_state.last_error = Some(error.message.clone());
        }
    }

    /// One bounded maintenance step: poll a pending rebuild, or inspect ONE
    /// indexed collection (round-robin) and start a background rebuild of
    /// its current identity when pressure, staleness or sampled recall call
    /// for it. Never changes settings or identity.
    pub(super) fn auto_tick(&self, db: &mut Database) -> Value {
        let (config, cursor) = match self.inner.lock() {
            Ok(inner) => (inner.auto.clone(), inner.cursor),
            Err(_) => return json!({"action":"error","error":"internal state unavailable"}),
        };
        let mut next_cursor = cursor;
        let result = (|| -> Result<AutoTickOutcome, ApiError> {
            if let Some((owner, _)) = &db.rebuild {
                let owner = *owner;
                let name = db.collections.get(&owner).map(|c| c.name.clone());
                let status = db.poll_rebuild_in(owner)?;
                if status == HnswRebuildStatus::Published {
                    self.metrics
                        .background_published
                        .fetch_add(1, Ordering::Relaxed);
                }
                return Ok((admin::outcome(status), name, None, Some(owner)));
            }
            let candidates = db
                .collections
                .iter()
                .filter(|(_, c)| {
                    c.index
                        .as_ref()
                        .is_some_and(|i| i.metric != DistanceMetric::InnerProduct)
                })
                .map(|(&id, _)| id)
                .collect::<Vec<_>>();
            let Some(&first) = candidates.first() else {
                return Ok(("idle", None, None, None));
            };
            let id = candidates
                .iter()
                .copied()
                .find(|&id| cursor.is_none_or(|c| id > c))
                .unwrap_or(first);
            next_cursor = Some(id);
            let state = db.state(id);
            let index = state.index.as_ref().expect("filtered on index");
            let (hnsw, metric, options) = (index.config, index.metric, index.options);
            let (graph, stale) = (index.graph_nodes(), index.stale_nodes());
            let name = state.name.clone();
            let policy = HnswMaintenancePolicy {
                limits: config.limits.core(),
                ..HnswMaintenancePolicy::default()
            };
            let status =
                db.maintenance_status_with_options_in(id, hnsw, metric, options, policy)?;
            let mut reason = if !matches!(status.pressure, HnswPressure::Healthy) {
                Some("pressure")
            } else if graph > 0 && stale as f64 / graph as f64 >= config.stale_ratio {
                Some("stale_ratio")
            } else {
                None
            };
            if reason.is_none() && config.rebuild_on_low_recall {
                let report = CollectionRef { db: &*db, id }.ann_quality(
                    hnsw,
                    metric,
                    options,
                    crate::quality::QualitySampleOptions {
                        samples: config.quality_samples,
                        ..Default::default()
                    },
                )?;
                if report.recall_at_k.is_some_and(|r| r < config.min_recall) {
                    reason = Some("low_recall");
                }
            }
            if reason.is_none() {
                return Ok(("healthy", Some(name), None, Some(id)));
            }
            let retry_wait = self
                .inner
                .lock()
                .ok()
                .filter(|inner| inner.auto.retry_backoff_initial > Duration::ZERO)
                .and_then(|inner| inner.auto_state.retries.get(&id).copied())
                .and_then(|retry| retry.until)
                .map(|until| until.saturating_duration_since(Instant::now()));
            if retry_wait.is_some_and(|wait| wait > Duration::ZERO) {
                return Ok(("retry_backoff", Some(name), reason, Some(id)));
            }
            db.start_rebuild_with_options_in(id, hnsw, metric, options, config.limits.core())?;
            self.metrics
                .background_started
                .fetch_add(1, Ordering::Relaxed);
            Ok(("rebuild_started", Some(name), reason, Some(id)))
        })();
        let Ok(mut inner) = self.inner.lock() else {
            return json!({"action":"error","error":"internal state unavailable"});
        };
        inner.cursor = next_cursor;
        let state = &mut inner.auto_state;
        state.ticks += 1;
        state.last_tick_unix_ms = Some(now_ms());
        match result {
            Ok((action, collection, reason, collection_id)) => {
                match action {
                    "rebuild_started" => state.rebuilds_started += 1,
                    "published" => state.rebuilds_published += 1,
                    _ => {}
                }
                if matches!(action, "catch_up_overflow" | "superseded")
                    && config.retry_backoff_initial > Duration::ZERO
                {
                    let retry = state
                        .retries
                        .entry(collection_id.expect("rebuild owner"))
                        .or_default();
                    retry.attempts = retry.attempts.saturating_add(1);
                    let delay = retry_delay(
                        config.retry_backoff_initial,
                        config.retry_backoff_max,
                        retry.attempts,
                    );
                    retry.until = Some(Instant::now() + delay);
                } else if matches!(action, "published" | "healthy")
                    && let Some(id) = collection_id
                {
                    state.retries.remove(&id);
                }
                state.last_action = Some(action);
                state.last_collection = collection.clone();
                let retry = collection_id.and_then(|id| state.retries.get(&id).copied());
                let retry_after_ms = retry.and_then(|retry| retry.until).map(|until| {
                    until.saturating_duration_since(Instant::now()).as_millis() as u64
                });
                json!({"action": action, "collection": collection, "reason": reason,
                    "retry_attempts": retry.map_or(0, |retry| retry.attempts),
                    "retry_after_ms": retry_after_ms})
            }
            Err(error) => {
                state.errors += 1;
                state.last_action = Some("error");
                state.last_error = Some(error.message.clone());
                json!({"action": "error", "error": {"code": error.code, "message": error.message}})
            }
        }
    }

    fn auto_json(inner: &Inner) -> Value {
        let c = &inner.auto;
        let s = &inner.auto_state;
        json!({
            "config": {"enabled": c.enabled, "interval_ms": c.interval.as_millis() as u64,
                "retry_backoff_initial_ms": c.retry_backoff_initial.as_millis() as u64,
                "retry_backoff_max_ms": c.retry_backoff_max.as_millis() as u64,
                "stale_ratio": c.stale_ratio, "rebuild_on_low_recall": c.rebuild_on_low_recall,
                "min_recall": c.min_recall, "quality_samples": c.quality_samples,
                "limits": c.limits.json()},
            "state": {"ticks": s.ticks, "skipped_busy": s.skipped_busy,
                "rebuilds_started": s.rebuilds_started, "rebuilds_published": s.rebuilds_published,
                "errors": s.errors, "last_tick_unix_ms": s.last_tick_unix_ms,
                "last_collection": s.last_collection, "last_action": s.last_action,
                "last_error": s.last_error, "retry_collections": s.retries.len(),
                "worker_running": inner.thread.is_some(),
                "worker_spawn_error": inner.spawn_error},
        })
    }

    fn backup_json(inner: &Inner) -> Value {
        let c = &inner.backup;
        let s = &inner.backup_state;
        json!({
            "config": {"enabled": c.enabled, "interval_secs": c.interval.as_secs(),
                "retention_enabled": c.retention_enabled, "retain_last": c.retain_last,
                "retain_days": c.retain_days, "drill_every": c.drill_every,
                "drill_max_bytes": c.drill_max_bytes},
            "state": {"runs": s.runs, "failures": s.failures, "skipped_busy": s.skipped_busy,
                "last_success_unix_ms": s.last_success_unix_ms,
                "last_failure_unix_ms": s.last_failure_unix_ms, "last_error": s.last_error,
                "last_backup": s.last_backup, "drills": s.drills, "drill_failures": s.drill_failures,
                "last_drill_unix_ms": s.last_drill_unix_ms, "retention_deleted": s.retention_deleted,
                "retained": s.retained, "verify_failures": s.verify_failures,
                "enabled": inner.directory.is_some()},
        })
    }

    /// Metric values for `/metrics`.
    pub(super) fn metrics_json(&self) -> Value {
        let Ok(inner) = self.inner.lock() else {
            return json!({});
        };
        let a = &inner.auto_state;
        let b = &inner.backup_state;
        json!({
            "auto_maintenance_enabled": inner.auto.enabled,
            "auto_maintenance_ticks_total": a.ticks,
            "auto_maintenance_skipped_busy_total": a.skipped_busy,
            "auto_maintenance_rebuilds_started_total": a.rebuilds_started,
            "auto_maintenance_errors_total": a.errors,
            "backup_schedule_enabled": inner.backup.enabled,
            "backup_runs_total": b.runs,
            "backup_failures_total": b.failures,
            "backup_skipped_busy_total": b.skipped_busy,
            "backup_last_success_unix_ms": b.last_success_unix_ms,
            "backup_last_failure_unix_ms": b.last_failure_unix_ms,
            "backup_drills_total": b.drills,
            "backup_drill_failures_total": b.drill_failures,
            "backup_drill_last_success_unix_ms": b.last_drill_unix_ms,
            "backup_verify_failures_total": b.verify_failures,
            "backup_retention_deleted_total": b.retention_deleted,
            "backups_retained": b.retained,
        })
    }

    pub(super) fn audit(&self, event: Value) -> bool {
        let log = self.inner.lock().ok().and_then(|inner| inner.audit.clone());
        match log {
            Some(log) if log.append(event, true).is_err() => {
                self.metrics.audit_failures.fetch_add(1, Ordering::Relaxed);
                false
            }
            _ => true,
        }
    }

    fn scheduled_backup(&self) -> Result<Value, ApiError> {
        let Ok(permit) = self.workers.clone().try_acquire_owned() else {
            if let Ok(mut inner) = self.inner.lock() {
                inner.backup_state.skipped_busy += 1;
            }
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "busy",
                "database queue is full",
            ));
        };
        let result = self.backup_cycle();
        drop(permit);
        result
    }

    /// Snapshot → verify → manifest → optional drill → optional retention.
    /// Every failure is counted, recorded in state and audited.
    fn backup_cycle(&self) -> Result<Value, ApiError> {
        let (directory, config, active) = {
            let inner = lock(&self.inner)?;
            (
                inner.directory.clone(),
                inner.backup.clone(),
                inner.active.clone(),
            )
        };
        let result = (|| {
            let directory = directory.ok_or_else(admin_directory_disabled)?;
            let (name, staged) = self.with_database(|db| snapshot(db, &directory))?;
            let mut manifest = publish(&directory, staged, &name)?;
            let runs = {
                let mut inner = lock(&self.inner)?;
                inner.backup_state.runs += 1;
                inner.backup_state.runs
            };
            if config.drill_every > 0 && runs % config.drill_every == 0 {
                let bytes = manifest["bytes"].as_u64().unwrap_or(u64::MAX);
                if bytes > config.drill_max_bytes {
                    manifest["drill"] = json!({"status":"skipped","unix_ms":now_ms(),
                        "message":"backup exceeds drill_max_bytes"});
                    write_manifest(&directory, &name, &manifest)?;
                } else {
                    self.drill_into(&directory, &name, &mut manifest)?;
                }
            }
            let mut deleted = Vec::new();
            if config.retention_enabled {
                deleted = self.retention(&directory, &config, active.as_deref())?;
            }
            Ok::<_, ApiError>((name, manifest, deleted))
        })();
        let Ok(mut inner) = self.inner.lock() else {
            return Err(internal("ops state"));
        };
        match result {
            Ok((name, manifest, deleted)) => {
                inner.backup_state.last_success_unix_ms = Some(now_ms());
                inner.backup_state.last_backup = Some(name.clone());
                drop(inner);
                self.audit(json!({"event":"backup_scheduled","name":name,
                    "bytes":manifest["bytes"],"crc32":manifest["crc32"],
                    "drill":manifest["drill"]["status"],"retention_deleted":deleted}));
                Ok(json!({"name": name, "manifest": manifest, "retention_deleted": deleted}))
            }
            Err(error) => {
                inner.backup_state.failures += 1;
                inner.backup_state.last_failure_unix_ms = Some(now_ms());
                inner.backup_state.last_error = Some(error.message.clone());
                drop(inner);
                super::logs::stderr(
                    "error",
                    "maintenance",
                    json!({"event":"scheduled_backup_failed","code":error.code}).to_string(),
                );
                self.audit(json!({"event":"backup_scheduled_failed","code":error.code}));
                Err(error)
            }
        }
    }

    fn drill_into(
        &self,
        directory: &FilePath,
        name: &str,
        manifest: &mut Value,
    ) -> Result<Value, ApiError> {
        let path = admin::safe_file(directory, name, "backup-")?;
        let started = now_ms();
        let report = crate::maintenance::drill(&path, directory);
        let mut inner = lock(&self.inner)?;
        inner.backup_state.drills += 1;
        let outcome = match &report {
            Ok(report) => {
                inner.backup_state.last_drill_unix_ms = Some(now_ms());
                json!({"status":"passed","unix_ms":started,
                    "message":format!("restored {} collections; {} probes passed", report.collections, report.probes)})
            }
            Err(_) => {
                inner.backup_state.drill_failures += 1;
                json!({"status":"failed","unix_ms":started,"message":"restore drill failed; the backup may be unusable"})
            }
        };
        drop(inner);
        manifest["drill"] = outcome.clone();
        write_manifest(directory, name, manifest)?;
        match report {
            Ok(report) => Ok(
                json!({"collections": report.collections, "probes": report.probes,
                "drill": outcome}),
            ),
            Err(_) => Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "drill_failed",
                "restore drill failed; the backup may be unusable",
            )),
        }
    }

    /// Deletes old generated backups. Protected: the newest generated, the
    /// newest verified, the startup selection target, the active database,
    /// symlinks and anything not named `backup-*.vlite`. Each deletion is
    /// audited (synced) first; an audit failure stops retention.
    fn retention(
        &self,
        directory: &FilePath,
        config: &BackupConfig,
        active: Option<&FilePath>,
    ) -> Result<Vec<String>, ApiError> {
        let now = now_ms();
        let mut items = Vec::new();
        for entry in std::fs::read_dir(directory).map_err(internal)? {
            let entry = entry.map_err(internal)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !admin::generated(&name, "backup-") {
                continue;
            }
            let meta = std::fs::symlink_metadata(entry.path()).map_err(internal)?;
            if !meta.is_file() || meta.file_type().is_symlink() {
                continue;
            }
            let manifest = read_manifest(directory, &name);
            let created = manifest
                .as_ref()
                .and_then(|m| m["created_unix_ms"].as_u64())
                .or_else(|| name_time(&name))
                .unwrap_or(0);
            let verified = manifest
                .as_ref()
                .is_some_and(|m| m["verified_unix_ms"].is_u64());
            items.push((created, name, verified));
        }
        items.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        let newest_verified = items.iter().find(|i| i.2).map(|i| i.1.clone());
        let startup = std::fs::read_to_string(directory.join("next-startup")).ok();
        let active = active.and_then(|p| std::fs::canonicalize(p).ok());
        let mut deleted = Vec::new();
        for (rank, (created, name, _)) in items.iter().enumerate() {
            let protected = rank == 0
                || newest_verified.as_deref() == Some(name)
                || startup.as_deref() == Some(name)
                || active.as_ref().is_some_and(|a| {
                    std::fs::canonicalize(directory.join(name)).ok().as_ref() == Some(a)
                });
            let old = config.retain_days == 0
                || now.saturating_sub(*created) > config.retain_days.saturating_mul(DAY_MS);
            if protected || rank < config.retain_last || !old {
                continue;
            }
            if !self.audit(json!({"event":"backup_retention_delete","name":name})) {
                return Err(ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "audit_unavailable",
                    "retention stopped: the audit log could not be written",
                ));
            }
            std::fs::remove_file(directory.join(name)).map_err(internal)?;
            let _ = std::fs::remove_file(manifest_path(directory, name));
            deleted.push(name.clone());
        }
        admin::sync_directory(directory)?;
        let mut inner = lock(&self.inner)?;
        inner.backup_state.retention_deleted += deleted.len() as u64;
        inner.backup_state.retained = Some((items.len() - deleted.len()) as u64);
        Ok(deleted)
    }
}

fn admin_directory_disabled() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "admin_directory_disabled",
        "configure VLITE_ADMIN_DIR to enable backups",
    )
}

fn name_time(name: &str) -> Option<u64> {
    name.strip_prefix("backup-")?
        .split('-')
        .next()?
        .parse()
        .ok()
}

fn manifest_path(directory: &FilePath, name: &str) -> PathBuf {
    directory.join(format!("{name}.manifest.json"))
}

/// Bounded read; a missing, symlinked or invalid manifest reads as None.
pub(super) fn read_manifest(directory: &FilePath, name: &str) -> Option<Value> {
    let path = manifest_path(directory, name);
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > MANIFEST_LIMIT {
        return None;
    }
    let mut text = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MANIFEST_LIMIT)
        .read_to_end(&mut text)
        .ok()?;
    serde_json::from_slice(&text).ok()
}

fn write_manifest(directory: &FilePath, name: &str, manifest: &Value) -> Result<(), ApiError> {
    let mut file = tempfile::NamedTempFile::new_in(directory).map_err(internal)?;
    file.write_all(&serde_json::to_vec_pretty(manifest).map_err(internal)?)
        .map_err(internal)?;
    file.as_file().sync_all().map_err(internal)?;
    file.persist(manifest_path(directory, name))
        .map_err(internal)?;
    admin::sync_directory(directory)
}

/// Copies the database under the owner lock into a staged temporary file.
pub(super) fn snapshot(
    db: &mut Database,
    directory: &FilePath,
) -> Result<(String, tempfile::NamedTempFile), ApiError> {
    db.ensure_healthy()?;
    // Periodic durability: a backup never holds commits the primary could lose.
    db.sync()?;
    let name = admin::name("backup-");
    let mut staged = tempfile::NamedTempFile::new_in(directory).map_err(internal)?;
    let position = db.file.0.stream_position().map_err(internal)?;
    let copied = (|| -> std::io::Result<()> {
        db.file.0.seek(SeekFrom::Start(0))?;
        std::io::copy(&mut db.file.0, staged.as_file_mut())?;
        staged.as_file().sync_all()
    })();
    db.file
        .0
        .seek(SeekFrom::Start(position))
        .map_err(internal)?;
    copied.map_err(internal)?;
    Ok((name, staged))
}

/// Verifies the staged copy, publishes it without clobbering and writes its
/// manifest (verified at creation).
pub(super) fn publish(
    directory: &FilePath,
    staged: tempfile::NamedTempFile,
    name: &str,
) -> Result<Value, ApiError> {
    let verification = crate::maintenance::verify(staged.path())?;
    if verification.inspection.incomplete_tail_bytes != 0 {
        return Err(unprocessable("database snapshot has an incomplete tail"));
    }
    let detail = crate::maintenance::inspect_detail(staged.path())?;
    staged
        .persist_noclobber(directory.join(name))
        .map_err(internal)?;
    admin::sync_directory(directory)?;
    let now = now_ms();
    let manifest = manifest_json(name, now, &verification, &detail);
    write_manifest(directory, name, &manifest)?;
    Ok(manifest)
}

fn manifest_json(
    name: &str,
    created: u64,
    verification: &crate::maintenance::Verification,
    detail: &crate::maintenance::InspectionDetail,
) -> Value {
    let i = &verification.inspection;
    json!({"schema": 1, "name": name, "created_unix_ms": created,
        "bytes": verification.bytes, "crc32": verification.crc32,
        "format_version": i.format_version, "dimensions": i.dimensions,
        "live_records": i.live_records, "commits": i.commits,
        "lineage": detail.lineage.map(|l| format!("{l:016x}")),
        "collections": detail.collections, "verified_unix_ms": created, "drill": null})
}

/// Fields derived from the backup bytes that a stored manifest must match.
const DERIVED_FIELDS: [&str; 10] = [
    "schema",
    "name",
    "bytes",
    "crc32",
    "format_version",
    "dimensions",
    "live_records",
    "commits",
    "lineage",
    "collections",
];

/// Returns a message naming the first derived field that differs.
fn manifest_mismatch(stored: &Value, fresh: &Value) -> Option<&'static str> {
    DERIVED_FIELDS
        .iter()
        .find(|field| stored.get(**field) != fresh.get(**field))
        .map(|field| match *field {
            "schema" => "backup manifest schema does not match",
            "name" => "backup manifest name does not match",
            "bytes" | "crc32" => "backup does not match its manifest checksum",
            "format_version" => "backup manifest format_version does not match the backup",
            "dimensions" => "backup manifest dimensions do not match the backup",
            "live_records" | "commits" => "backup manifest counts do not match the backup",
            "lineage" => "backup manifest lineage does not match the backup",
            _ => "backup manifest collection summary does not match the backup",
        })
}

pub(super) fn routes() -> Router<Service> {
    Router::new()
        .route("/v1/admin/maintenance/auto", get(auto_get).put(auto_put))
        .route("/v1/admin/maintenance/auto/tick", post(auto_tick_now))
        .route(
            "/v1/admin/backups/schedule",
            get(schedule_get).put(schedule_put),
        )
        .route("/v1/admin/backups/schedule/run", post(schedule_run))
        .route("/v1/admin/backups/{name}/verify", post(verify))
        .route("/v1/admin/backups/{name}/drill", post(drill))
}

async fn auto_get(State(service): State<Service>) -> Result<Json<Value>, ApiError> {
    Ok(Json(Ops::auto_json(&*lock(&service.ops.inner)?)))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AutoInput {
    enabled: Option<bool>,
    interval_secs: Option<u64>,
    retry_backoff_initial_ms: Option<u64>,
    retry_backoff_max_ms: Option<u64>,
    stale_ratio: Option<f64>,
    rebuild_on_low_recall: Option<bool>,
    min_recall: Option<f64>,
    quality_samples: Option<usize>,
    limits: Option<admin::Limits>,
}

async fn auto_put(
    State(service): State<Service>,
    input: Result<Json<AutoInput>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let input = body(input)?;
    if input
        .interval_secs
        .is_some_and(|s| !(10..=3600).contains(&s))
    {
        return Err(unprocessable("interval_secs must be 10..3600"));
    }
    if input.retry_backoff_initial_ms.is_some() != input.retry_backoff_max_ms.is_some() {
        return Err(unprocessable(
            "retry_backoff_initial_ms and retry_backoff_max_ms must be set together",
        ));
    }
    if let (Some(initial), Some(maximum)) =
        (input.retry_backoff_initial_ms, input.retry_backoff_max_ms)
        && (maximum > 3_600_000
            || (initial == 0 && maximum != 0)
            || (initial > 0 && initial > maximum))
    {
        return Err(unprocessable(
            "retry backoff must be disabled with both values zero, or initial <= max <= 3600000 ms",
        ));
    }
    if input.stale_ratio.is_some_and(|r| !(r > 0.0 && r <= 1.0)) {
        return Err(unprocessable("stale_ratio must be in (0, 1]"));
    }
    if input.min_recall.is_some_and(|r| !(0.0..=1.0).contains(&r)) {
        return Err(unprocessable("min_recall must be 0..1"));
    }
    if input
        .quality_samples
        .is_some_and(|n| !(1..=32).contains(&n))
    {
        return Err(unprocessable("quality_samples must be 1..32"));
    }
    if let Some(limits) = input.limits {
        limits.core().validate(0, 1)?;
    }
    let ops = service.ops.clone();
    let mut inner = lock(&ops.inner)?;
    {
        let config = &mut inner.auto;
        if let Some(enabled) = input.enabled {
            config.enabled = enabled;
        }
        if let Some(secs) = input.interval_secs {
            config.interval = Duration::from_secs(secs);
        }
        if let (Some(initial), Some(maximum)) =
            (input.retry_backoff_initial_ms, input.retry_backoff_max_ms)
        {
            config.retry_backoff_initial = Duration::from_millis(initial);
            config.retry_backoff_max = Duration::from_millis(maximum);
        }
        if let Some(ratio) = input.stale_ratio {
            config.stale_ratio = ratio;
        }
        if let Some(flag) = input.rebuild_on_low_recall {
            config.rebuild_on_low_recall = flag;
        }
        if let Some(recall) = input.min_recall {
            config.min_recall = recall;
        }
        if let Some(samples) = input.quality_samples {
            config.quality_samples = samples;
        }
        if let Some(limits) = input.limits {
            config.limits = limits;
        }
    }
    if input.retry_backoff_initial_ms.is_some() {
        inner.auto_state.retries.clear();
    }
    inner.next_auto = Some(Instant::now() + inner.auto.interval);
    ops.ensure_thread(&mut inner);
    let value = Ops::auto_json(&inner);
    drop(inner);
    if let Some(log) = &service.admin.audit_log
        && log
            .append(
                json!({"event":"auto_maintenance_configured","config":value["config"]}),
                false,
            )
            .is_err()
    {
        service
            .metrics
            .audit_failures
            .fetch_add(1, Ordering::Relaxed);
    }
    Ok(Json(value))
}

async fn auto_tick_now(State(service): State<Service>) -> Result<Json<Value>, ApiError> {
    let ops = service.ops.clone();
    let result = service.run(move |db| Ok(ops.auto_tick(db))).await?;
    let state = Ops::auto_json(&*lock(&service.ops.inner)?);
    Ok(Json(json!({"result": result, "state": state["state"]})))
}

async fn schedule_get(State(service): State<Service>) -> Result<Json<Value>, ApiError> {
    Ok(Json(Ops::backup_json(&*lock(&service.ops.inner)?)))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScheduleInput {
    enabled: Option<bool>,
    interval_secs: Option<u64>,
    retention_enabled: Option<bool>,
    retain_last: Option<usize>,
    retain_days: Option<u64>,
    drill_every: Option<u64>,
    drill_max_bytes: Option<u64>,
}

async fn schedule_put(
    State(service): State<Service>,
    input: Result<Json<ScheduleInput>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let input = body(input)?;
    if input
        .interval_secs
        .is_some_and(|s| !(300..=604_800).contains(&s))
    {
        return Err(unprocessable("interval_secs must be 300..604800"));
    }
    if input.retain_last.is_some_and(|n| !(1..=100).contains(&n)) {
        return Err(unprocessable("retain_last must be 1..100"));
    }
    if input.retain_days.is_some_and(|d| d > 3650) {
        return Err(unprocessable("retain_days must be 0..3650"));
    }
    if input.drill_every.is_some_and(|n| n > 100) {
        return Err(unprocessable("drill_every must be 0..100"));
    }
    let ops = service.ops.clone();
    let mut inner = lock(&ops.inner)?;
    if inner.directory.is_none()
        && (input.enabled == Some(true) || input.retention_enabled == Some(true))
    {
        return Err(admin_directory_disabled());
    }
    let config = &mut inner.backup;
    if let Some(enabled) = input.enabled {
        config.enabled = enabled;
    }
    if let Some(secs) = input.interval_secs {
        config.interval = Duration::from_secs(secs);
    }
    if let Some(flag) = input.retention_enabled {
        config.retention_enabled = flag;
    }
    if let Some(n) = input.retain_last {
        config.retain_last = n;
    }
    if let Some(days) = input.retain_days {
        config.retain_days = days;
    }
    if let Some(n) = input.drill_every {
        config.drill_every = n;
    }
    if let Some(bytes) = input.drill_max_bytes {
        config.drill_max_bytes = bytes;
    }
    inner.next_backup = Some(Instant::now() + inner.backup.interval);
    ops.ensure_thread(&mut inner);
    let value = Ops::backup_json(&inner);
    drop(inner);
    ops.audit(json!({"event":"backup_schedule_configured","config":value["config"]}));
    Ok(Json(value))
}

async fn schedule_run(State(service): State<Service>) -> Result<Json<Value>, ApiError> {
    let ops = service.ops.clone();
    tokio::task::spawn_blocking(move || ops.scheduled_backup())
        .await
        .map_err(internal)?
        .map(Json)
}

fn backup_name(input: Result<Path<String>, PathRejection>) -> Result<String, ApiError> {
    let Path(name) = input.map_err(|error| {
        ApiError::new(StatusCode::BAD_REQUEST, "invalid_path", error.body_text())
    })?;
    Ok(name)
}

/// Runs filesystem-only work while holding one database worker permit, so
/// verification and drills are bounded by the same concurrency limit.
async fn blocking<T: Send + 'static>(
    service: &Service,
    operation: impl FnOnce(PathBuf) -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    let directory = service
        .admin
        .directory
        .clone()
        .ok_or_else(admin_directory_disabled)?;
    let permit = service.workers.clone().try_acquire_owned().map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "busy",
            "database queue is full",
        )
    })?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        operation(directory)
    })
    .await
    .map_err(internal)?
}

async fn verify(
    State(service): State<Service>,
    name: Result<Path<String>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let name = backup_name(name)?;
    let ops = service.ops.clone();
    blocking(&service, move |directory| {
        let path = admin::safe_file(&directory, &name, "backup-")?;
        let failed = |message: &'static str| {
            if let Ok(mut inner) = ops.inner.lock() {
                inner.backup_state.verify_failures += 1;
            }
            ops.audit(json!({"event":"backup_verify_failed","name":name}));
            ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "verification_failed",
                message,
            )
        };
        let verification =
            crate::maintenance::verify(&path).map_err(|_| failed("backup failed validation"))?;
        if verification.inspection.incomplete_tail_bytes != 0 {
            return Err(failed("backup has an incomplete tail"));
        }
        let detail = crate::maintenance::inspect_detail(&path)
            .map_err(|_| failed("backup failed validation"))?;
        let now = now_ms();
        // Every derived field is recomputed from the backup bytes; a stored
        // manifest must match all of them, so an edited lineage, format,
        // dimension, count or collection summary is never re-marked verified.
        let fresh = manifest_json(
            &name,
            name_time(&name).unwrap_or(now),
            &verification,
            &detail,
        );
        let manifest_exists = std::fs::symlink_metadata(manifest_path(&directory, &name)).is_ok();
        let mut manifest = match read_manifest(&directory, &name) {
            Some(manifest) => {
                if let Some(field) = manifest_mismatch(&manifest, &fresh) {
                    return Err(failed(field));
                }
                manifest
            }
            None if manifest_exists => {
                return Err(failed("backup manifest is unreadable or invalid"));
            }
            None => fresh,
        };
        manifest["verified_unix_ms"] = json!(now);
        write_manifest(&directory, &name, &manifest)?;
        ops.audit(json!({"event":"backup_verified","name":name}));
        Ok(Json(
            json!({"name": name, "verified": true, "manifest": manifest}),
        ))
    })
    .await
}

async fn drill(
    State(service): State<Service>,
    name: Result<Path<String>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let name = backup_name(name)?;
    let ops = service.ops.clone();
    blocking(&service, move |directory| {
        admin::safe_file(&directory, &name, "backup-")?;
        let mut manifest = read_manifest(&directory, &name).unwrap_or_else(
            || json!({"schema": 1, "name": name, "verified_unix_ms": null, "drill": null}),
        );
        let result = ops.drill_into(&directory, &name, &mut manifest);
        ops.audit(json!({"event":"backup_drill","name":name,
            "status":manifest["drill"]["status"]}));
        let mut value = result?;
        value["name"] = json!(name);
        value["manifest"] = manifest;
        Ok(Json(value))
    })
    .await
}

#[cfg(test)]
mod retry_tests {
    use super::{Ops, retry_delay};
    use crate::server::Metrics;
    use crate::{Database, DistanceMetric, HnswConfig, Mutation};
    use std::{
        sync::{Arc, RwLock},
        time::{Duration, Instant},
    };
    use tokio::sync::Semaphore;

    #[test]
    fn retry_backoff_is_disabled_by_default_and_doubles_only_to_configured_cap() {
        assert_eq!(
            retry_delay(Duration::ZERO, Duration::ZERO, 4),
            Duration::ZERO
        );
        let initial = Duration::from_millis(250);
        let maximum = Duration::from_secs(1);
        assert_eq!(retry_delay(initial, maximum, 1), initial);
        assert_eq!(retry_delay(initial, maximum, 2), Duration::from_millis(500));
        assert_eq!(retry_delay(initial, maximum, 3), maximum);
        assert_eq!(retry_delay(initial, maximum, u32::MAX), maximum);
    }

    #[test]
    fn superseded_auto_rebuild_waits_for_explicitly_configured_backoff() {
        let mut db = Database::create_in_memory(2, crate::CreateOptions::default()).unwrap();
        let config = HnswConfig::default();
        let metric = DistanceMetric::SquaredL2;
        db.put(0, &[0.0, 0.0], b"").unwrap();
        db.build_hnsw_with_metric(config, metric).unwrap();
        let writes = |start: u64| {
            (start..start + 129)
                .map(|id| Mutation::Put {
                    id,
                    vector: vec![id as f32, 1.0],
                    payload: Vec::new(),
                })
                .collect::<Vec<_>>()
        };
        db.write_batch(&writes(1)).unwrap();

        let database = Arc::new(RwLock::new(Some(db)));
        let ops = Arc::new(Ops::new(
            Arc::downgrade(&database),
            Arc::new(Semaphore::new(1)),
            Arc::new(Metrics::default()),
        ));
        {
            let mut inner = ops.inner.lock().unwrap();
            inner.auto.enabled = true;
            inner.auto.retry_backoff_initial = Duration::from_secs(5);
            inner.auto.retry_backoff_max = Duration::from_secs(20);
        }
        {
            let mut owner = database.write().unwrap();
            assert_eq!(
                ops.auto_tick(owner.as_mut().unwrap())["action"],
                "rebuild_started"
            );
            let db = owner.as_mut().unwrap();
            db.build_hnsw_with_metric(config, metric).unwrap();
            db.write_batch(&writes(1_000)).unwrap();
        }

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let result = {
                let mut owner = database.write().unwrap();
                ops.auto_tick(owner.as_mut().unwrap())
            };
            if result["action"] == "superseded" {
                break;
            }
            assert_eq!(result["action"], "cancelling", "{result}");
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let result = {
            let mut owner = database.write().unwrap();
            ops.auto_tick(owner.as_mut().unwrap())
        };
        assert_eq!(result["action"], "retry_backoff");
        assert_eq!(result["retry_attempts"], 1);
        assert!(result["retry_after_ms"].as_u64().unwrap() > 0);
        assert!(database.read().unwrap().as_ref().unwrap().rebuild.is_none());
    }
}
