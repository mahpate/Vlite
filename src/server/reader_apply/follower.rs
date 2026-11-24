use std::{
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, PoisonError, mpsc},
    thread::{self, JoinHandle},
    time::Duration,
};

use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::json;

use super::{ApiError, Database, Progress, Service, log_stderr};
use crate::server::{
    cloud_backup::{
        self as cloud, HaIdentity, Manifest, PinKind, Record, SetState, Sets, archive::Follow,
    },
    cloud_recovery,
    serve_mode::unix_ms,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReaderApplyConfig {
    pub target_file: PathBuf,
    pub shared_key_file: Option<PathBuf>,
    pub follower_dir: PathBuf,
    pub capacity_bytes: u64,
    pub staleness_bound_ms: u64,
    pub poll_interval_ms: u64,
}

fn invalid(detail: &str) -> String {
    format!("reader_apply_invalid: {detail}")
}

impl ReaderApplyConfig {
    pub fn load(file: &Path, served: &Path) -> Result<Self, String> {
        let mut file = fs::File::open(file).map_err(|_| invalid("cannot read configuration"))?;
        let too_large = |file: &fs::File| {
            file.metadata()
                .map(|metadata| metadata.len() > 65_536)
                .map_err(|_| invalid("cannot inspect configuration"))
        };
        if too_large(&file)? {
            return Err(invalid("configuration exceeds 64 KiB"));
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take(65_536)
            .read_to_end(&mut bytes)
            .map_err(|_| invalid("cannot read configuration"))?;
        if too_large(&file)? {
            return Err(invalid("configuration exceeds 64 KiB"));
        }
        let config: Self = serde_json::from_slice(&bytes)
            .map_err(|_| invalid("configuration fields are invalid"))?;
        if !config.follower_dir.is_absolute()
            || config
                .follower_dir
                .components()
                .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
            || config
                .follower_dir
                .to_string_lossy()
                .split(std::path::is_separator)
                .any(|c| c == "." || c == "..")
        {
            return Err(invalid(
                "follower_dir must be absolute without dot components",
            ));
        }
        if !(1..=cloud::MAX_DATABASE_BYTES).contains(&config.capacity_bytes) {
            return Err(invalid("capacity_bytes must be between 1 and 1073741824"));
        }
        if !(1..=3_600_000).contains(&config.staleness_bound_ms) {
            return Err(invalid("staleness_bound_ms must be between 1 and 3600000"));
        }
        if config.poll_interval_ms == 0 || config.poll_interval_ms >= config.staleness_bound_ms {
            return Err(invalid(
                "poll_interval_ms must be positive and below the staleness bound",
            ));
        }
        if served.as_os_str()
            != config
                .follower_dir
                .join("reader-a")
                .join("database.vlite")
                .as_os_str()
        {
            return Err(invalid("served path must be slot A"));
        }
        Ok(config)
    }
}

struct State {
    served: usize,
    released: bool,
}

struct Follower {
    sets: Sets,
    config: ReaderApplyConfig,
    record: Record,
    manifest: Manifest,
    ha: Option<HaIdentity>,
    pin: String,
    slots: [PathBuf; 2],
    progress: Arc<Progress>,
    state: Mutex<State>,
}

pub struct ReaderApply {
    inner: Arc<Follower>,
    worker: Option<(mpsc::Sender<()>, JoinHandle<()>)>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    Confirmed,
    Swapped,
    Retry,
    Failed,
    Stopped,
}

fn error(code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, code, message)
}

fn io_error(error: std::io::Error) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "reader_apply_io",
        error.to_string(),
    )
}

fn replica() -> String {
    std::env::var("CONTAINER_APP_REPLICA_NAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| format!("pid-{}", std::process::id()))
        .chars()
        .take(120)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The reason on every set pin a reader replica holds: control-owned
/// recovery (design-tenant-provisioning ITEM-013) selects a stopped
/// replica's pins by it.
pub(in crate::server) fn pin_reason(replica: &str) -> String {
    format!("reader-apply:{replica}")
}

fn slot_dirs(config: &ReaderApplyConfig) -> [PathBuf; 2] {
    [
        config.follower_dir.join("reader-a"),
        config.follower_dir.join("reader-b"),
    ]
}

fn clean_slots(dirs: &[PathBuf; 2]) {
    for dir in dirs {
        // Never follow or remove a non-directory supplied in place of a slot.
        match fs::symlink_metadata(dir) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
                if let Err(error) = fs::remove_dir_all(dir) {
                    log_stderr(
                        "error",
                        "reader_apply",
                        json!({
                            "event":"reader_apply_cleanup_failed","reason":error.to_string()
                        })
                        .to_string(),
                    );
                }
            }
            _ => {}
        }
    }
}

impl ReaderApply {
    pub fn enroll(config: ReaderApplyConfig) -> Result<Self, String> {
        let sets = cloud_recovery::connect(&config.target_file, config.shared_key_file.as_deref())
            .map_err(|error| format!("{}: {}", error.code, error.message))?;
        Self::enroll_with(sets, config, &replica(), unix_ms())
            .map_err(|error| format!("{}: {}", error.code, error.message))
    }

    pub(super) fn enroll_with(
        sets: Sets,
        config: ReaderApplyConfig,
        replica: &str,
        now: u64,
    ) -> Result<Self, ApiError> {
        let authority = sets.read_authority()?;
        let head = authority
            .as_ref()
            .map(|observed| {
                observed
                    .record
                    .head
                    .as_ref()
                    .or(observed.record.base.as_ref())
                    .ok_or_else(|| {
                        error(
                            "reader_apply_no_set",
                            "no published backup set is available",
                        )
                    })
            })
            .transpose()?;
        let listing = sets.list()?;
        let chosen = listing
            .sets
            .iter()
            .filter_map(|set| set.record.as_ref())
            .find(|record| {
                record.state == SetState::Published
                    && head.is_none_or(|head| head.set_id == record.set_id)
            })
            .ok_or_else(|| {
                error(
                    "reader_apply_no_set",
                    "no published backup set is available",
                )
            })?;
        let reason = pin_reason(replica);
        for record in listing.sets.iter().filter_map(|set| set.record.as_ref()) {
            for pin in record.pins.iter().filter(|pin| pin.reason == reason) {
                sets.release(&record.set_id, &pin.id, now)?;
            }
        }
        let pin = sets.pin(&chosen.set_id, &reason, PinKind::Operation, now)?;
        let dirs = slot_dirs(&config);
        let enrolled = (|| {
            for dir in &dirs {
                match fs::symlink_metadata(dir) {
                    Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
                    Ok(_) => {
                        return Err(error(
                            "reader_apply_invalid",
                            "a follower slot is not a real directory",
                        ));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(io_error(e)),
                }
            }
            for dir in &dirs {
                if dir.exists() {
                    fs::remove_dir_all(dir).map_err(io_error)?;
                }
            }
            fs::create_dir_all(&config.follower_dir).map_err(io_error)?;
            for dir in &dirs {
                cloud::create_private_subdir(dir).map_err(io_error)?;
            }
            let fetched =
                sets.fetch_with(&chosen.set_id, &pin.id, &dirs[0], config.capacity_bytes)?;
            let followed = match head {
                Some(head) => {
                    if fetched.ha.is_none()
                        || fetched.record.manifest_sha256 != head.manifest_sha256
                    {
                        return Err(error(
                            "reader_apply_reseed_required",
                            "the authority head does not name this HA baseline",
                        ));
                    }
                    fetched
                        .ha
                        .as_ref()
                        .expect("checked HA manifest")
                        .check_scope(
                            &fetched.manifest.installation_id,
                            &authority.as_ref().expect("head has an authority").record,
                        )?;
                    sets.follow_archive_ha(
                        &fetched.record,
                        &fetched.manifest,
                        &fetched.database,
                        head,
                    )?
                }
                None => {
                    sets.follow_archive(&fetched.record, &fetched.manifest, &fetched.database)?
                }
            };
            let sequence = match followed {
                Follow::Applied { sequence, .. } | Follow::Current { sequence } => sequence,
                Follow::Reseed(reason) => {
                    return Err(error("reader_apply_reseed_required", reason));
                }
            };
            let len = fs::metadata(&fetched.database).map_err(io_error)?.len();
            if len > config.capacity_bytes {
                return Err(error(
                    "reader_capacity_exceeded",
                    "the follower exceeds its capacity ceiling",
                ));
            }
            cloud::check_restore_space(
                cloud::free_bytes(&config.follower_dir),
                len.saturating_add(cloud::RESTORE_SPACE_MARGIN),
            )?;
            let mut input = fs::File::open(&fetched.database).map_err(io_error)?;
            let mut output =
                cloud::create_private_file(&dirs[1].join("database.vlite")).map_err(io_error)?;
            std::io::copy(&mut input, &mut output).map_err(io_error)?;
            output.sync_all().map_err(io_error)?;
            Ok((fetched, sequence))
        })();
        let (fetched, sequence) = match enrolled {
            Ok(value) => value,
            Err(error) => {
                clean_slots(&dirs);
                release(&sets, &chosen.set_id, &pin.id, now);
                return Err(error);
            }
        };
        let progress = Arc::new(Progress::default());
        progress.set_bound(config.staleness_bound_ms);
        progress.confirm(sequence, now);
        // Only an HA follower, whose authority scope is checked, stamps.
        if let (Some(head), Some(_)) = (head, &fetched.ha) {
            progress.set_epoch(head.epoch);
        }
        log_stderr(
            "info",
            "reader_apply",
            json!({
                "event":"reader_apply_enrolled","set_id":chosen.set_id,
                "sequence":sequence,"pin_id":pin.id
            })
            .to_string(),
        );
        Ok(Self {
            inner: Arc::new(Follower {
                sets,
                config,
                record: fetched.record,
                manifest: fetched.manifest,
                ha: authority.and(fetched.ha),
                pin: pin.id,
                slots: dirs.map(|dir| dir.join("database.vlite")),
                progress,
                state: Mutex::new(State {
                    served: 0,
                    released: false,
                }),
            }),
            worker: None,
        })
    }

    pub(in crate::server::reader_apply) fn progress(&self) -> Arc<Progress> {
        self.inner.progress.clone()
    }

    pub fn step(&self, service: &Service, now: u64) -> Step {
        self.inner.step(service, now)
    }

    pub fn start(&mut self, service: Service) {
        if self.worker.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let inner = self.inner.clone();
        match thread::Builder::new()
            .name("vlite-reader-apply".into())
            .spawn(move || {
                while matches!(
                    rx.recv_timeout(Duration::from_millis(inner.config.poll_interval_ms)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    if inner.step(&service, unix_ms()) == Step::Stopped {
                        break;
                    }
                }
            }) {
            Ok(worker) => self.worker = Some((tx, worker)),
            Err(error) => {
                self.inner
                    .progress
                    .fail(format!("cannot start reader apply thread: {error}"));
                self.inner.release(unix_ms());
            }
        }
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some((tx, worker)) = self.worker.take() {
            let _ = tx.send(());
            if worker.join().is_err() {
                self.inner
                    .progress
                    .fail("reader apply thread panicked".into());
            }
        }
        self.inner.release(unix_ms());
    }
}

impl Drop for ReaderApply {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn release(sets: &Sets, set: &str, pin: &str, now: u64) -> bool {
    match sets.release(set, pin, now) {
        Ok(_) => {
            log_stderr(
                "info",
                "reader_apply",
                json!({
                    "event":"reader_apply_pin_released","set_id":set,"pin_id":pin
                })
                .to_string(),
            );
            true
        }
        Err(error) => {
            log_stderr(
                "error",
                "reader_apply",
                json!({
                    "event":"reader_apply_pin_release_failed","code":error.code
                })
                .to_string(),
            );
            false
        }
    }
}

impl Follower {
    fn release(&self, now: u64) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.released {
            state.released = release(&self.sets, &self.record.set_id, &self.pin, now);
        }
    }

    fn step(&self, service: &Service, now: u64) -> Step {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if self.progress.failure().is_some() || state.released {
            return Step::Stopped;
        }
        let result = (|| {
            let authority = self.sets.read_authority()?;
            if self.ha.is_some() && authority.is_none() {
                return Err(crate::server::authority::AuthorityError::Missing.into());
            }
            let head = authority
                .as_ref()
                .map(|observed| {
                    observed
                        .record
                        .head
                        .as_ref()
                        .or(observed.record.base.as_ref())
                        .ok_or_else(|| {
                            error(
                                "reader_apply_no_set",
                                "no published backup set is available",
                            )
                        })
                })
                .transpose()?;
            if let Some(head) = head {
                if head.set_id != self.record.set_id {
                    return Err(error(
                        "reader_apply_reseed_required",
                        "a newer published set needs a re-seed",
                    ));
                }
                if head.sequence < self.progress.applied().0 {
                    return Err(error(
                        "reader_apply_reseed_required",
                        "the archive head moved backwards",
                    ));
                }
                if let Some(identity) = &self.ha {
                    identity.check_scope(
                        &self.manifest.installation_id,
                        &authority.as_ref().expect("head has an authority").record,
                    )?;
                }
            } else {
                let listing = self.sets.list()?;
                let newest = listing
                    .sets
                    .iter()
                    .filter_map(|set| set.record.as_ref())
                    .find(|record| record.state == SetState::Published);
                if newest.is_some_and(|record| {
                    (record.created_unix_ms, &record.set_id)
                        > (self.record.created_unix_ms, &self.record.set_id)
                }) {
                    return Err(error(
                        "reader_apply_reseed_required",
                        "a newer published set needs a re-seed",
                    ));
                }
                if !listing
                    .sets
                    .iter()
                    .filter_map(|set| set.record.as_ref())
                    .any(|record| {
                        record.set_id == self.record.set_id && record.state == SetState::Published
                    })
                {
                    return Err(error(
                        "reader_apply_reseed_required",
                        "the followed set is missing or no longer published",
                    ));
                }
            }
            // Only an HA follower stamps. While the authority recovers, the
            // head is its base, whose epoch is below the record's, so a
            // gateway that already names the new epoch refuses the stamp.
            let epoch = self.ha.as_ref().and(head).map(|head| head.epoch);
            let standby = 1 - state.served;
            let followed = match head {
                Some(head) => self.sets.follow_archive_ha(
                    &self.record,
                    &self.manifest,
                    &self.slots[standby],
                    head,
                )?,
                None => {
                    self.sets
                        .follow_archive(&self.record, &self.manifest, &self.slots[standby])?
                }
            };
            let confirmed_at = if authority.as_ref().is_some_and(|observed| {
                observed.record.phase == crate::server::authority::Phase::Recovering
            }) {
                self.progress.applied().1
            } else {
                now
            };
            let (sequence, segments) = match followed {
                Follow::Applied { sequence, segments } => (sequence, segments),
                Follow::Current { sequence } => (sequence, 0),
                Follow::Reseed(reason) => {
                    return Err(error("reader_apply_reseed_required", reason));
                }
            };
            let served = self.progress.applied().0;
            if sequence < served {
                return Err(error(
                    "reader_apply_reseed_required",
                    "the archive head moved backwards",
                ));
            }
            if sequence == served {
                self.progress.confirm(served, confirmed_at);
                if let Some(epoch) = epoch {
                    self.progress.set_epoch(epoch);
                }
                return Ok(Step::Confirmed);
            }
            if fs::metadata(&self.slots[standby]).map_err(io_error)?.len()
                > self.config.capacity_bytes
            {
                return Err(error(
                    "reader_capacity_exceeded",
                    "the follower exceeds its capacity ceiling",
                ));
            }
            let next = Database::open_read_only(&self.slots[standby])
                .map_err(|e| error("reader_apply_open_failed", e.to_string()))?;
            service
                .swap_reader_generation(next)
                .map_err(|e| error("reader_apply_swap_failed", e.to_string()))?;
            state.served = standby;
            self.progress.confirm(sequence, confirmed_at);
            if let Some(epoch) = epoch {
                self.progress.set_epoch(epoch);
            }
            log_stderr(
                "info",
                "reader_apply",
                json!({
                    "event":"reader_apply_applied","set_id":self.record.set_id,
                    "sequence":sequence,"segments":segments
                })
                .to_string(),
            );
            Ok(Step::Swapped)
        })();
        match result {
            Ok(step) => step,
            Err(error)
                if (error.status == StatusCode::SERVICE_UNAVAILABLE
                    && matches!(
                        error.code,
                        "cloud_store_unavailable" | "writer_authority_unavailable"
                    ))
                    || (error.status == StatusCode::INSUFFICIENT_STORAGE
                        && error.code == "restore_insufficient_space") =>
            {
                Step::Retry
            }
            Err(error) => {
                self.progress
                    .fail(if error.code == "reader_apply_reseed_required" {
                        error.message
                    } else {
                        error.code.into()
                    });
                state.released = release(&self.sets, &self.record.set_id, &self.pin, now);
                Step::Failed
            }
        }
    }
}
