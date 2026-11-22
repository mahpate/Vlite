//! feature-aca-ha EPIC-010: opt-in automatic failover. Unqualified until
//! GATE-005 passes: off by default, switched by a shared policy object that a
//! person can disable it (with a final pre-transfer cutoff), and built only
//! from the operator-acknowledged failover steps (fence, loss report, transfer).

use std::{
    ffi::OsString,
    io::Read,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

use axum::{
    Extension, Json, Router,
    extract::{State as Extract, rejection::JsonRejection},
    http::StatusCode,
    routing::get,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
    ApiError, Service, admin, audit,
    authority::{AuthorityStore, Phase, Record},
    cloud_backup::{Cond, ObjectStore, StoreError, random_hex},
    failover::{FailoverAck, FailoverError, LossReport, build_report, execute},
    gateway_mode::{flags, utf8},
    users,
};

pub(super) const POLICY_SCHEMA: &str = "vlite.auto-failover/1";
const MAX_POLICY_BYTES: u64 = 4 * 1024;
const MAX_FILE_BYTES: u64 = 64 * 1024;
const GATE: &str = "GATE-005";
const WARNING: &str = "Automatic failover is unqualified: GATE-005 has not passed. When it is on, a supervisor that loses contact with the writer fences it and transfers authority without a person reviewing the loss report, so recent commits can be lost.";
const AUTO_USAGE: &str = "vlite-server failover auto --config <json>";
const POLICY_USAGE: &str = "vlite-server failover policy show|enable|disable --target-file <json> [--shared-key-file <path>] [enable only: --expected-revision <rev> (when a policy exists) --acknowledge-unqualified true]";

// ----------------------------------------------------------------- policy

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Policy {
    pub schema: String,
    pub enabled: bool,
    pub revision: String,
    pub updated_unix_ms: u64,
    pub source: String,
}

impl Policy {
    fn valid(&self) -> bool {
        self.schema == POLICY_SCHEMA && valid_revision(&self.revision) && valid_source(&self.source)
    }
}

fn valid_revision(revision: &str) -> bool {
    revision.len() == 32
        && revision
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn valid_source(source: &str) -> bool {
    !source.is_empty() && source.len() <= 256 && !source.chars().any(char::is_control)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Current {
    Missing,
    Valid(Policy, String),
    /// Unreadable policy, with its ETag when it could be fetched.
    Invalid(Option<String>),
}

impl Current {
    fn enabled(&self) -> bool {
        matches!(self, Self::Valid(policy, _) if policy.enabled)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PolicyError {
    Conflict,
    Unavailable,
    Invalid,
    Indeterminate,
}

impl PolicyError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Conflict => "auto_failover_conflict",
            Self::Unavailable => "auto_failover_unavailable",
            Self::Invalid => "auto_failover_invalid",
            Self::Indeterminate => "auto_failover_indeterminate",
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Self::Conflict => "the policy changed; read it again and retry",
            Self::Unavailable => "the coordination store is unavailable; retry",
            Self::Invalid => "the stored policy is invalid; turn automatic failover off first",
            Self::Indeterminate => {
                "the coordination store did not confirm the change; read it again"
            }
        }
    }
}

/// The shared on/off switch beside the authority record.
pub(super) struct PolicyStore {
    store: Arc<dyn ObjectStore>,
    key: String,
}

impl PolicyStore {
    pub fn new(store: Arc<dyn ObjectStore>, prefix: String) -> Self {
        Self {
            store,
            key: format!("{prefix}auto-failover.json"),
        }
    }

    pub fn current(&self) -> Result<Current, PolicyError> {
        let blob = match self.store.get(&self.key, MAX_POLICY_BYTES) {
            Ok(blob) => blob,
            Err(StoreError::NotFound) => return Ok(Current::Missing),
            Err(StoreError::TooLarge) => return Ok(Current::Invalid(None)),
            Err(_) => return Err(PolicyError::Unavailable),
        };
        Ok(
            match serde_json::from_slice::<Policy>(&blob.bytes)
                .ok()
                .filter(|policy| blob.bytes.len() as u64 <= MAX_POLICY_BYTES && policy.valid())
            {
                Some(policy) => Current::Valid(policy, blob.etag),
                None => Current::Invalid(Some(blob.etag)),
            },
        )
    }

    /// Turning on needs the revision the caller saw (`None` when no policy
    /// exists). Turning off ignores the revision and always writes a fresh
    /// disabled revision, replacing invalid policies and retrying lost races.
    pub fn set(
        &self,
        enabled: bool,
        expected_revision: Option<&str>,
        source: &str,
        now_ms: u64,
    ) -> Result<Option<Policy>, PolicyError> {
        if !valid_source(source) {
            return Err(PolicyError::Invalid);
        }
        if enabled {
            return match self.current()? {
                Current::Invalid(_) => Err(PolicyError::Invalid),
                Current::Missing if expected_revision.is_none() => {
                    self.write(true, source, now_ms, Cond::Create).map(Some)
                }
                Current::Valid(policy, etag)
                    if expected_revision == Some(policy.revision.as_str()) =>
                {
                    if policy.enabled {
                        Ok(Some(policy))
                    } else {
                        self.write(true, source, now_ms, Cond::Match(&etag))
                            .map(Some)
                    }
                }
                _ => Err(PolicyError::Conflict),
            };
        }
        let mut attempt = 0;
        loop {
            attempt += 1;
            let result = match self.current()? {
                Current::Missing => self.write(false, source, now_ms, Cond::Create),
                Current::Valid(_, etag) => self.write(false, source, now_ms, Cond::Match(&etag)),
                // Too large to read, so it has no ETag to match: remove it,
                // then create the off policy (a racing create retries).
                Current::Invalid(None) => match self.store.delete(&self.key) {
                    Ok(()) | Err(StoreError::NotFound) => {
                        self.write(false, source, now_ms, Cond::Create)
                    }
                    Err(_) => return Err(PolicyError::Unavailable),
                },
                Current::Invalid(Some(etag)) => {
                    self.write(false, source, now_ms, Cond::Match(&etag))
                }
            };
            match result {
                Err(PolicyError::Conflict) if attempt < 3 => continue,
                result => return result.map(Some),
            }
        }
    }

    fn write(
        &self,
        enabled: bool,
        source: &str,
        now_ms: u64,
        cond: Cond<'_>,
    ) -> Result<Policy, PolicyError> {
        let fresh = || random_hex().map_err(|_| PolicyError::Unavailable);
        let policy = Policy {
            schema: POLICY_SCHEMA.into(),
            enabled,
            revision: format!("{}{}", fresh()?, fresh()?),
            updated_unix_ms: now_ms,
            source: source.into(),
        };
        let bytes = serde_json::to_vec(&policy).map_err(|_| PolicyError::Invalid)?;
        match self.store.put(&self.key, &bytes, cond) {
            Ok(_) => Ok(policy),
            Err(StoreError::Conflict | StoreError::NotFound) => Err(PolicyError::Conflict),
            Err(StoreError::Unknown(_)) => match self.store.get(&self.key, MAX_POLICY_BYTES) {
                Ok(blob) if blob.bytes == bytes => Ok(policy),
                Ok(_) | Err(StoreError::NotFound) => Err(PolicyError::Conflict),
                Err(_) => Err(PolicyError::Indeterminate),
            },
            Err(_) => Err(PolicyError::Unavailable),
        }
    }
}

fn view(policy: Option<&Policy>, invalid: bool) -> Value {
    json!({
        "available": true,
        "enabled": policy.is_some_and(|policy| policy.enabled),
        "revision": policy.map(|policy| policy.revision.as_str()),
        "updated_unix_ms": policy.map(|policy| policy.updated_unix_ms),
        "source": policy.map(|policy| policy.source.as_str()),
        "invalid": invalid,
        "qualified": false,
        "gate": GATE,
        "warning": WARNING,
    })
}

fn current_view(current: &Current) -> Value {
    match current {
        Current::Missing => view(None, false),
        Current::Valid(policy, _) => view(Some(policy), false),
        Current::Invalid(_) => view(None, true),
    }
}

// --------------------------------------------------------------- detector

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    Healthy,
    Suspected(u32),
    Promote,
    Hold(&'static str),
}

/// What one probe of the writer's readiness URL showed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Probe {
    Ready,
    Down,
    /// The writer answered but refused or redirected the probe: a
    /// configuration fault, never evidence that the writer is down.
    Refused(&'static str),
    /// The response was inconclusive; do not count it as a failed probe.
    Hold(&'static str),
}

/// Counts consecutive failed probes of one authority revision. Any change of
/// epoch or revision (for example a writer that still publishes) restarts it.
/// It promotes only a writer (epoch and writer id) that this supervisor has
/// seen ready, so a probe URL that never reached the writer cannot fence it.
pub(super) struct Detector {
    threshold: u32,
    failures: u32,
    seen: Option<(u64, String)>,
    ready_writer: Option<(u64, String)>,
}

impl Detector {
    pub fn new(threshold: u32) -> Self {
        Self {
            threshold: threshold.max(1),
            failures: 0,
            seen: None,
            ready_writer: None,
        }
    }

    pub fn reset(&mut self) {
        self.failures = 0;
        self.seen = None;
    }

    pub fn observe(&mut self, record: &Record, ready: bool) -> Verdict {
        if self
            .seen
            .as_ref()
            .is_none_or(|(epoch, revision)| *epoch != record.epoch || *revision != record.revision)
        {
            self.failures = 0;
            self.seen = Some((record.epoch, record.revision.clone()));
        }
        if ready {
            self.failures = 0;
            if let Some(writer) = &record.writer_id {
                self.ready_writer = Some((record.epoch, writer.clone()));
            }
            return Verdict::Healthy;
        }
        self.failures = self.failures.saturating_add(1);
        if self.failures < self.threshold {
            return Verdict::Suspected(self.failures);
        }
        match &record.writer_id {
            Some(writer) if record.phase == Phase::Serving => {
                if self
                    .ready_writer
                    .as_ref()
                    .is_some_and(|(epoch, ready)| *epoch == record.epoch && ready == writer)
                {
                    Verdict::Promote
                } else {
                    Verdict::Hold("writer_never_seen_healthy")
                }
            }
            _ => Verdict::Hold("no_serving_writer"),
        }
    }
}

// ------------------------------------------------------------- supervisor

/// The side effects of one supervisor, replaced by fakes in the drills.
pub(super) trait Actions {
    fn probe(&self, expected: &Record) -> Probe;
    /// Stops the writer named by `record` and returns its remaining replicas.
    fn fence(&self, record: &Record) -> Result<u64, String>;
    /// Starts the standby after `old` moved to `new_epoch`; `Ok(false)` when
    /// none is configured.
    fn promote(&self, old: &Record, new_epoch: u64) -> Result<bool, String>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Tick {
    Disabled,
    Held(&'static str),
    Healthy,
    Suspected(u32),
    Promoted {
        old_epoch: u64,
        new_epoch: u64,
        report: Box<LossReport>,
        standby: &'static str,
    },
    Aborted(&'static str),
}

impl Tick {
    fn finished(&self) -> bool {
        matches!(self, Self::Promoted { .. } | Self::Aborted(_))
    }

    fn json(&self) -> Value {
        match self {
            Self::Disabled => json!({"state": "disabled"}),
            Self::Held(reason) => json!({"state": "held", "reason": reason}),
            Self::Healthy => json!({"state": "healthy"}),
            Self::Suspected(failures) => json!({"state": "suspected", "failures": failures}),
            // The accepted loss report is kept in the output and the log, so
            // the loss can be reviewed after the unattended transfer.
            Self::Promoted {
                old_epoch,
                new_epoch,
                report,
                standby,
            } => json!({
                "state": "promoted",
                "old_epoch": old_epoch,
                "new_epoch": new_epoch,
                "report_id": report.report_id,
                "report": report,
                "text": report.render(),
                "standby": standby,
            }),
            Self::Aborted(reason) => json!({"state": "aborted", "reason": reason}),
        }
    }
}

pub(super) struct Supervisor<A: Actions> {
    policy: PolicyStore,
    authority: AuthorityStore,
    actions: A,
    detector: Detector,
}

impl<A: Actions> Supervisor<A> {
    pub fn new(objects: Arc<dyn ObjectStore>, prefix: String, threshold: u32, actions: A) -> Self {
        Self {
            policy: PolicyStore::new(objects.clone(), prefix.clone()),
            authority: AuthorityStore::new(objects, prefix),
            actions,
            detector: Detector::new(threshold),
        }
    }

    /// One probe. Never fences unless the policy is on and the authority is
    /// unchanged immediately before the fence, and never transfers unless the
    /// policy is still on after it and the authority is unchanged throughout.
    pub fn tick(&mut self) -> Tick {
        match self.policy.current() {
            Ok(current) if current.enabled() => {}
            Ok(_) => {
                self.detector.reset();
                return Tick::Disabled;
            }
            Err(_) => {
                self.detector.reset();
                return Tick::Held("policy_unavailable");
            }
        }
        let record = match self.authority.read() {
            Ok(observed) => observed.record,
            Err(_) => {
                self.detector.reset();
                return Tick::Held("authority_unavailable");
            }
        };
        let ready = match self.actions.probe(&record) {
            Probe::Ready => true,
            Probe::Down => false,
            Probe::Refused(reason) => {
                self.detector.reset();
                return Tick::Held(reason);
            }
            Probe::Hold(reason) => {
                self.detector.reset();
                return Tick::Held(reason);
            }
        };
        match self.detector.observe(&record, ready) {
            Verdict::Healthy => Tick::Healthy,
            Verdict::Suspected(failures) => Tick::Suspected(failures),
            Verdict::Hold(reason) => Tick::Held(reason),
            Verdict::Promote => {
                self.detector.reset();
                self.promote(&record)
            }
        }
    }

    fn promote(&self, seen: &Record) -> Tick {
        // Read again immediately before the fence: nothing has happened yet,
        // so a change here only stops this attempt.
        match self.policy.current() {
            Ok(current) if current.enabled() => {}
            Ok(_) => return Tick::Disabled,
            Err(_) => return Tick::Held("policy_unavailable"),
        }
        match self.authority.read() {
            Ok(observed) if observed.record == *seen => {}
            Ok(_) => return Tick::Held("authority_changed"),
            Err(_) => return Tick::Held("authority_unavailable"),
        }
        match self.actions.fence(seen) {
            Err(_) => return Tick::Aborted("fence_failed"),
            Ok(0) => {}
            Ok(_) => return Tick::Aborted("fence_unconfirmed"),
        }
        let Ok(report) = build_report(&self.authority, None, None) else {
            return Tick::Aborted("report_failed");
        };
        if report.epoch != seen.epoch || report.revision != seen.revision {
            return Tick::Aborted("authority_changed");
        }
        let ack = FailoverAck {
            report_id: report.report_id.clone(),
            expected_epoch: report.epoch,
            expected_revision: report.revision.clone(),
            old_replicas: 0,
        };
        // This is the cancellation cutoff: disabling before this check aborts
        // promotion; a transfer already past it may still complete.
        match self.policy.current() {
            Ok(current) if current.enabled() => {}
            Ok(_) => return Tick::Aborted("disabled_during_promotion"),
            Err(_) => return Tick::Aborted("policy_unavailable_during_promotion"),
        }
        let transferred = match execute(&self.authority, &report, &ack) {
            Ok(transferred) => transferred,
            Err(FailoverError::ReportStale | FailoverError::Conflict) => {
                return Tick::Aborted("authority_changed");
            }
            Err(_) => return Tick::Aborted("transfer_failed"),
        };
        let new_epoch = transferred.record.epoch;
        Tick::Promoted {
            old_epoch: report.epoch,
            new_epoch,
            report: Box::new(report),
            standby: match self.actions.promote(seen, new_epoch) {
                Ok(true) => "started",
                Ok(false) => "not_configured",
                Err(_) => "start_failed",
            },
        }
    }
}

// ----------------------------------------------------------- live actions

struct Live {
    client: reqwest::blocking::Client,
    url: String,
    token: Option<String>,
    fence: Vec<String>,
    promote: Option<Vec<String>>,
    timeout: Duration,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FenceOutput {
    old_replicas: u64,
}

/// The writer a command acts on, passed in its environment.
fn record_env(record: &Record) -> Vec<(&'static str, String)> {
    let mut env = vec![
        ("VLITE_FAILOVER_EPOCH", record.epoch.to_string()),
        ("VLITE_FAILOVER_REVISION", record.revision.clone()),
    ];
    if let Some(writer) = &record.writer_id {
        env.push(("VLITE_FAILOVER_WRITER_ID", writer.clone()));
    }
    env
}

fn classify_probe_status(status: reqwest::StatusCode) -> Probe {
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        Probe::Hold("probe_throttled")
    } else if status == reqwest::StatusCode::UNAUTHORIZED
        || status == reqwest::StatusCode::FORBIDDEN
    {
        Probe::Refused("probe_unauthorized")
    } else if status.is_redirection() {
        Probe::Refused("probe_redirected")
    } else if status.is_server_error() {
        Probe::Down
    } else {
        Probe::Refused("probe_unexpected_status")
    }
}

fn classify_probe_body(body: &[u8], expected_epoch: u64) -> Probe {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return Probe::Hold("probe_response_invalid");
    };
    if value["status"] == "ready"
        && value["mode"] == "writer"
        && value["epoch"].as_u64() == Some(expected_epoch)
    {
        Probe::Ready
    } else {
        Probe::Refused("probe_identity_mismatch")
    }
}

impl Actions for Live {
    fn probe(&self, expected: &Record) -> Probe {
        let mut request = self.client.get(&self.url);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = match request.send() {
            Ok(response) => response,
            Err(_) => return Probe::Down,
        };
        let status = response.status();
        if status != reqwest::StatusCode::OK {
            return classify_probe_status(status);
        }
        let mut body = Vec::new();
        if response.take(4097).read_to_end(&mut body).is_err() || body.len() > 4096 {
            return Probe::Hold("probe_response_invalid");
        }
        classify_probe_body(&body, expected.epoch)
    }

    fn fence(&self, record: &Record) -> Result<u64, String> {
        let output = run_command(&self.fence, &record_env(record), self.timeout)?;
        serde_json::from_slice::<FenceOutput>(&output)
            .map(|output| output.old_replicas)
            .map_err(|_| "fence_output_invalid".into())
    }

    fn promote(&self, old: &Record, new_epoch: u64) -> Result<bool, String> {
        match &self.promote {
            None => Ok(false),
            Some(argv) => {
                let mut env = record_env(old);
                env.push(("VLITE_FAILOVER_NEW_EPOCH", new_epoch.to_string()));
                run_command(argv, &env, self.timeout).map(|_| true)
            }
        }
    }
}

/// Runs an argument vector without a shell; standard output is bounded and
/// standard error passes through to the supervisor's. A command must not
/// leave a background process holding its standard output open.
fn run_command(
    argv: &[String],
    env: &[(&str, String)],
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    let (program, args) = argv.split_first().ok_or("command_empty")?;
    let deadline = Instant::now() + timeout;
    let mut child = Command::new(program)
        .args(args)
        .envs(env.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("command_spawn_failed: {}", error.kind()))?;
    let stdout = child.stdout.take().ok_or("command_spawn_failed")?;
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let read = stdout.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes);
        let _ = sender.send(read.map(|_| bytes));
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("command_timed_out".into());
            }
        }
    };
    let bytes = receiver
        .recv_timeout(Duration::from_secs(2))
        .map_err(|_| "command_output_unfinished".to_owned())?
        .map_err(|_| "command_output_failed".to_owned())?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err("command_output_too_large".into());
    }
    if !status.success() {
        return Err("command_failed".into());
    }
    Ok(bytes)
}

// -------------------------------------------------------------------- CLI

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    target_file: PathBuf,
    #[serde(default)]
    shared_key_file: Option<PathBuf>,
    writer_ready_url: String,
    /// A bearer token for `/ready`, needed unless the writer trusts a proxy.
    #[serde(default)]
    probe_token_file: Option<PathBuf>,
    probe_interval_ms: u64,
    probe_timeout_ms: u64,
    failure_threshold: u32,
    fence_command: Vec<String>,
    #[serde(default)]
    promote_command: Option<Vec<String>>,
    #[serde(default = "default_command_timeout_ms")]
    command_timeout_ms: u64,
}

fn default_command_timeout_ms() -> u64 {
    300_000
}

fn valid_argv(argv: &[String]) -> bool {
    (1..=64).contains(&argv.len())
        && argv
            .iter()
            .all(|arg| arg.len() <= 4096 && !arg.contains('\0'))
        && !argv[0].is_empty()
}

impl Config {
    fn validate(&self) -> Result<(), String> {
        let invalid = |detail: &str| Err(format!("invalid_config: {detail}"));
        let url = reqwest::Url::parse(&self.writer_ready_url).ok();
        if self.writer_ready_url.len() > 2048
            || !url.is_some_and(|url| matches!(url.scheme(), "http" | "https") && url.has_host())
        {
            return invalid("writer_ready_url must be an http or https URL of at most 2048 bytes");
        }
        if !(500..=60_000).contains(&self.probe_interval_ms) {
            return invalid("probe_interval_ms must be 500 to 60000");
        }
        if self.probe_timeout_ms < 100 || self.probe_timeout_ms >= self.probe_interval_ms {
            return invalid("probe_timeout_ms must be at least 100 and below probe_interval_ms");
        }
        if !(2..=100).contains(&self.failure_threshold) {
            return invalid("failure_threshold must be 2 to 100");
        }
        if !valid_argv(&self.fence_command)
            || self
                .promote_command
                .as_deref()
                .is_some_and(|argv| !valid_argv(argv))
        {
            return invalid("commands must have 1 to 64 arguments of at most 4096 bytes");
        }
        if !(1_000..=3_600_000).contains(&self.command_timeout_ms) {
            return invalid("command_timeout_ms must be 1000 to 3600000");
        }
        Ok(())
    }
}

fn read_file(name: &str, path: PathBuf) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|error| format!("invalid_arguments: {name} cannot be read: {}", error.kind()))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(format!("invalid_arguments: {name} is larger than 64 KiB"));
    }
    Ok(bytes)
}

fn read_token(path: PathBuf) -> Result<String, String> {
    let bytes = read_file("probe_token_file", path)?;
    let token = std::str::from_utf8(&bytes)
        .map(str::trim)
        .unwrap_or_default();
    if token.is_empty() || token.len() > 4096 || !token.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(
            "invalid_config: probe_token_file must hold one printable ASCII token of at most 4096 bytes"
                .into(),
        );
    }
    Ok(token.to_owned())
}

fn connect(
    target: &std::path::Path,
    key: Option<&std::path::Path>,
) -> Result<(Arc<dyn ObjectStore>, String), String> {
    let sets = super::cloud_recovery::connect(target, key)
        .map_err(|error| format!("{}: {}", error.code, error.message))?;
    Ok(sets.coordination())
}

fn log(event: &str, detail: Value) {
    let mut line = json!({"event": event});
    if let (Some(line), Value::Object(detail)) = (line.as_object_mut(), detail) {
        line.extend(detail);
    }
    super::log_stderr("info", "failover", line.to_string());
}

/// A promotion prints its result; an abort, or a failed standby-start command,
/// exits nonzero so wrappers never mistake it for a confirmed ready writer.
fn outcome(tick: &Tick) -> Result<Value, String> {
    match tick {
        Tick::Aborted(reason) => Err(format!(
            "auto_failover_aborted: {reason}; the old writer may already be stopped and, after transfer_failed, authority may have changed: inspect it before any retry"
        )),
        Tick::Promoted {
            old_epoch,
            new_epoch,
            report,
            standby: "start_failed",
        } => Err(format!(
            "auto_failover_standby_failed: authority moved from epoch {old_epoch} to {new_epoch} (report {}) but promote_command failed; replacement state is unconfirmed: inspect authority and actual writer replicas before starting another writer",
            report.report_id
        )),
        tick => Ok(tick.json()),
    }
}

/// Blocking supervisor; runs until it promotes or aborts.
pub(super) fn auto_command(args: &[OsString]) -> Result<Value, String> {
    let mut values = flags(args, &["--config"], AUTO_USAGE)?;
    let path = values
        .remove("--config")
        .ok_or_else(|| format!("invalid_arguments: --config is required; usage: {AUTO_USAGE}"))?;
    let bytes = read_file("--config", PathBuf::from(path))?;
    let config: Config =
        serde_json::from_slice(&bytes).map_err(|error| format!("invalid_config: {error}"))?;
    config.validate()?;
    let token = config
        .probe_token_file
        .clone()
        .map(read_token)
        .transpose()?;
    let (objects, prefix) = connect(&config.target_file, config.shared_key_file.as_deref())?;
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_millis(config.probe_timeout_ms))
        .build()
        .map_err(|_| "internal_error: the probe client cannot be built".to_owned())?;
    let mut supervisor = Supervisor::new(
        objects,
        prefix,
        config.failure_threshold,
        Live {
            client,
            url: config.writer_ready_url,
            token,
            fence: config.fence_command,
            promote: config.promote_command,
            timeout: Duration::from_millis(config.command_timeout_ms),
        },
    );
    log(
        "auto_failover_started",
        json!({"qualified": false, "gate": GATE, "failure_threshold": config.failure_threshold}),
    );
    let interval = Duration::from_millis(config.probe_interval_ms);
    let mut last = None;
    loop {
        let started = Instant::now();
        let tick = supervisor.tick();
        if last.as_ref() != Some(&tick) {
            log("auto_failover_tick", tick.json());
        }
        if tick.finished() {
            return outcome(&tick);
        }
        last = Some(tick);
        std::thread::sleep(interval.saturating_sub(started.elapsed()));
    }
}

/// Blocking policy switch for operators without the dashboard.
pub(super) fn policy_command(args: &[OsString]) -> Result<Value, String> {
    let usage = |detail: &str| format!("invalid_arguments: {detail}; usage: {POLICY_USAGE}");
    let verb = match args.first().and_then(|arg| arg.to_str()) {
        Some(verb @ ("show" | "enable" | "disable")) => verb,
        _ => return Err(usage("the verb must be show, enable or disable")),
    };
    let mut allowed = vec!["--target-file", "--shared-key-file"];
    if verb == "enable" {
        allowed.extend(["--expected-revision", "--acknowledge-unqualified"]);
    }
    let mut values = flags(&args[1..], &allowed, POLICY_USAGE)?;
    let target = PathBuf::from(
        values
            .remove("--target-file")
            .ok_or_else(|| usage("--target-file is required"))?,
    );
    let expected = values
        .remove("--expected-revision")
        .map(|value| utf8("--expected-revision", value))
        .transpose()?;
    if expected
        .as_deref()
        .is_some_and(|value| !valid_revision(value))
    {
        return Err(usage(
            "--expected-revision must be 32 lowercase hexadecimal characters",
        ));
    }
    if verb == "enable"
        && values
            .remove("--acknowledge-unqualified")
            .map(|value| utf8("--acknowledge-unqualified", value))
            .transpose()?
            .as_deref()
            != Some("true")
    {
        return Err(usage(
            "enable requires --acknowledge-unqualified true: GATE-005 has not passed",
        ));
    }
    let key = values.remove("--shared-key-file").map(PathBuf::from);
    let (objects, prefix) = connect(&target, key.as_deref())?;
    let store = PolicyStore::new(objects, prefix);
    let failed = |error: PolicyError| format!("{}: {}", error.code(), error.describe());
    let output = match verb {
        "show" => current_view(&store.current().map_err(failed)?),
        verb => {
            let enabled = verb == "enable";
            let policy = store
                .set(
                    enabled,
                    expected.as_deref(),
                    "cli",
                    super::serve_mode::unix_ms(),
                )
                .map_err(failed)?;
            log(
                "auto_failover_policy_changed",
                json!({"enabled": enabled, "source": "cli"}),
            );
            view(policy.as_ref(), false)
        }
    };
    Ok(output)
}

// ------------------------------------------------------------------- REST

pub(super) fn routes() -> Router<Service> {
    Router::new().route("/v1/admin/auto-failover", get(status).put(update))
}

fn policy_store(service: &Service) -> Option<PolicyStore> {
    service
        .writer_fence
        .authority()
        .map(|(store, _)| PolicyStore::new(store.objects(), store.prefix().to_owned()))
}

fn api_error(error: PolicyError) -> ApiError {
    let status = match error {
        PolicyError::Conflict | PolicyError::Invalid => StatusCode::CONFLICT,
        PolicyError::Unavailable | PolicyError::Indeterminate => StatusCode::SERVICE_UNAVAILABLE,
    };
    ApiError::new(status, error.code(), error.describe())
}

/// `rest:<actor>` without control characters, cut on a character boundary
/// so that it fits the policy's 256-byte source limit.
fn policy_source(actor: &str) -> String {
    let mut source = String::from("rest:");
    for char in actor.chars().filter(|char| !char.is_control()) {
        if source.len() + char.len_utf8() > 256 {
            break;
        }
        source.push(char);
    }
    source
}

async fn status(Extract(service): Extract<Service>) -> Result<Json<Value>, ApiError> {
    let Some(store) = policy_store(&service) else {
        let mut unavailable = view(None, false);
        unavailable["available"] = json!(false);
        return Ok(Json(unavailable));
    };
    let current = tokio::task::spawn_blocking(move || store.current())
        .await
        .map_err(super::internal)?
        .map_err(api_error)?;
    Ok(Json(current_view(&current)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Update {
    enabled: bool,
    #[serde(default)]
    expected_revision: Option<String>,
    #[serde(default)]
    acknowledge_unqualified: bool,
}

async fn update(
    Extract(service): Extract<Service>,
    Extension(role): Extension<admin::Role>,
    principal: Option<Extension<users::Principal>>,
    input: Result<Json<Update>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let input = super::body(input)?;
    if service.admin.is_open() {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "admin_auth_required",
            "automatic failover policy changes require configured administrator authentication",
        ));
    }
    if input.enabled && !input.acknowledge_unqualified {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "auto_failover_unacknowledged",
            "turning automatic failover on requires acknowledge_unqualified: GATE-005 has not passed",
        ));
    }
    // Turning off ignores the revision, so a malformed one never blocks the kill switch.
    if input.enabled
        && input
            .expected_revision
            .as_deref()
            .is_some_and(|revision| !valid_revision(revision))
    {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_json",
            "expected_revision must be 32 lowercase hexadecimal characters",
        ));
    }
    let Some(store) = policy_store(&service) else {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "auto_failover_not_configured",
            "automatic failover needs a writer with an authority target",
        ));
    };
    let session = principal.as_ref().map(|Extension(principal)| principal);
    let actor = audit::actor(&role, session);
    let source = policy_source(&actor);
    let enabled = input.enabled;
    let policy = tokio::task::spawn_blocking(move || {
        store.set(
            enabled,
            input.expected_revision.as_deref(),
            &source,
            super::serve_mode::unix_ms(),
        )
    })
    .await
    .map_err(super::internal)?
    .map_err(api_error)?;
    let revision = policy.as_ref().map(|policy| policy.revision.as_str());
    if let Some(log) = &service.admin.audit_log
        && log
            .append(
                json!({
                    "event": "auto_failover_policy_changed",
                    "actor": actor.clone(),
                    "enabled": enabled,
                    "revision": revision,
                }),
                false,
            )
            .is_err()
    {
        service
            .metrics
            .audit_failures
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    log(
        "auto_failover_policy_changed",
        json!({"actor": actor, "enabled": enabled, "revision": revision}),
    );
    Ok(Json(view(policy.as_ref(), false)))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::server::{
        authority::{Claim, HeadRef, Observed},
        cloud_backup::fake::{Fault, Memory, Op},
        failover::Loss,
    };

    const POLICY: &str = "test/auto-failover.json";

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

    fn authority(memory: &Arc<Memory>) -> AuthorityStore {
        AuthorityStore::new(memory.clone(), "test/".into())
    }

    fn setup() -> (Arc<Memory>, Claim, Observed) {
        let memory = Arc::new(Memory::default());
        let store = authority(&memory);
        let prepared = store
            .create("installation", "scope", "0123456789abcdef")
            .unwrap();
        let (claim, claimed) = store.claim(&prepared, 42).unwrap();
        let served = store.advance_head(&claim, &claimed, head(1, 10)).unwrap();
        (memory, claim, served)
    }

    fn policies(memory: &Arc<Memory>) -> PolicyStore {
        PolicyStore::new(memory.clone(), "test/".into())
    }

    fn enable(memory: &Arc<Memory>) -> Policy {
        policies(memory)
            .set(true, None, "test", 1)
            .unwrap()
            .unwrap()
    }

    type Hook = Mutex<Option<Box<dyn FnOnce() + Send>>>;

    #[derive(Default)]
    struct Fake {
        healthy: Mutex<bool>,
        refused: Mutex<Option<&'static str>>,
        replicas: Mutex<Option<u64>>,
        promote_fails: Mutex<bool>,
        /// The epoch and writer of each fenced record.
        fenced: Mutex<Vec<(u64, Option<String>)>>,
        /// The old and new epoch of each promotion.
        promoted: Mutex<Vec<(u64, u64)>>,
        during_probe: Hook,
        during_fence: Hook,
    }

    impl Fake {
        fn down() -> Self {
            let fake = Self::default();
            *fake.replicas.lock().unwrap() = Some(0);
            fake
        }

        fn fences(&self) -> usize {
            self.fenced.lock().unwrap().len()
        }

        fn promotes(&self) -> usize {
            self.promoted.lock().unwrap().len()
        }
    }

    impl Actions for &Fake {
        fn probe(&self, _expected: &Record) -> Probe {
            if let Some(during) = self.during_probe.lock().unwrap().take() {
                during();
            }
            match *self.refused.lock().unwrap() {
                Some(reason) => Probe::Refused(reason),
                None if *self.healthy.lock().unwrap() => Probe::Ready,
                None => Probe::Down,
            }
        }

        fn fence(&self, record: &Record) -> Result<u64, String> {
            self.fenced
                .lock()
                .unwrap()
                .push((record.epoch, record.writer_id.clone()));
            if let Some(during) = self.during_fence.lock().unwrap().take() {
                during();
            }
            self.replicas
                .lock()
                .unwrap()
                .ok_or_else(|| "fence_failed".to_owned())
        }

        fn promote(&self, old: &Record, new_epoch: u64) -> Result<bool, String> {
            self.promoted.lock().unwrap().push((old.epoch, new_epoch));
            if *self.promote_fails.lock().unwrap() {
                return Err("command_failed".into());
            }
            Ok(true)
        }
    }

    fn supervisor<'a>(
        memory: &Arc<Memory>,
        fake: &'a Fake,
        threshold: u32,
    ) -> Supervisor<&'a Fake> {
        Supervisor::new(memory.clone(), "test/".into(), threshold, fake)
    }

    /// One ready probe, so the supervisor has seen the writer healthy, then
    /// the writer goes down.
    fn seen_ready(supervisor: &mut Supervisor<&Fake>, fake: &Fake) {
        *fake.healthy.lock().unwrap() = true;
        assert_eq!(supervisor.tick(), Tick::Healthy);
        *fake.healthy.lock().unwrap() = false;
    }

    #[test]
    fn policy_is_off_until_enabled_and_turning_on_needs_the_seen_revision() {
        let (memory, _, _) = setup();
        let store = policies(&memory);
        assert_eq!(store.current().unwrap(), Current::Missing);
        assert!(!store.current().unwrap().enabled());
        assert_eq!(
            store.set(true, Some("0".repeat(32).as_str()), "test", 1),
            Err(PolicyError::Conflict)
        );
        let on = store.set(true, None, "test", 1).unwrap().unwrap();
        assert!(on.enabled && on.valid());
        assert_eq!(store.set(true, None, "test", 2), Err(PolicyError::Conflict));
        assert_eq!(
            store.set(true, Some(&on.revision), "test", 2).unwrap(),
            Some(on.clone())
        );
        let off = store.set(false, None, "test", 3).unwrap().unwrap();
        assert!(!off.enabled);
        assert_eq!(
            store.set(true, Some(&on.revision), "test", 4),
            Err(PolicyError::Conflict)
        );
        assert!(
            store
                .set(true, Some(&off.revision), "test", 4)
                .unwrap()
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn turning_off_always_wins() {
        let (memory, _, _) = setup();
        let store = policies(&memory);
        let disabled = store.set(false, None, "test", 1).unwrap().unwrap();
        assert!(!disabled.enabled);
        assert_eq!(store.set(true, None, "test", 1), Err(PolicyError::Conflict));
        store
            .set(true, Some(&disabled.revision), "test", 1)
            .unwrap()
            .unwrap();
        memory.fault(Op::Get, POLICY, Fault::Refuse, 0, 1);
        assert_eq!(
            store.set(false, None, "test", 2),
            Err(PolicyError::Unavailable)
        );
        assert!(store.current().unwrap().enabled());
        // A concurrent writer replaces the policy between the read and the
        // conditional write; turning off retries and still wins.
        let raced = Arc::downgrade(&memory);
        memory.hook(Op::Put, POLICY, move || {
            if let Some(memory) = raced.upgrade() {
                let store = PolicyStore::new(memory.clone(), "test/".into());
                let Current::Valid(policy, _) = store.current().unwrap() else {
                    panic!("policy");
                };
                let mut bytes = serde_json::to_vec(&Policy {
                    revision: "f".repeat(32),
                    ..policy
                })
                .unwrap();
                bytes.push(b' ');
                memory.set_raw(POLICY, &bytes);
            }
        });
        assert!(!store.set(false, None, "test", 3).unwrap().unwrap().enabled);
        memory.set_raw(POLICY, b"{\"schema\":\"other\"}");
        assert!(matches!(
            store.current().unwrap(),
            Current::Invalid(Some(_))
        ));
        assert_eq!(store.set(true, None, "test", 4), Err(PolicyError::Invalid));
        assert!(!store.set(false, None, "test", 5).unwrap().unwrap().enabled);
    }

    #[test]
    fn probe_requires_the_expected_ready_writer_and_holds_on_throttling() {
        assert_eq!(
            classify_probe_body(br#"{"status":"ready","mode":"writer","epoch":7}"#, 7),
            Probe::Ready
        );
        assert_eq!(
            classify_probe_body(br#"{"status":"ready","mode":"writer","epoch":8}"#, 7),
            Probe::Refused("probe_identity_mismatch")
        );
        assert_eq!(
            classify_probe_body(br#"{"status":"ready"}"#, 7),
            Probe::Refused("probe_identity_mismatch")
        );
        assert_eq!(
            classify_probe_status(reqwest::StatusCode::TOO_MANY_REQUESTS),
            Probe::Hold("probe_throttled")
        );
        assert_eq!(
            classify_probe_status(reqwest::StatusCode::SERVICE_UNAVAILABLE),
            Probe::Down
        );
    }

    #[test]
    fn turning_off_replaces_an_oversized_policy() {
        let (memory, _, _) = setup();
        let store = policies(&memory);
        enable(&memory);
        memory.set_raw(POLICY, &[b' '; 5 * 1024]);
        assert_eq!(store.current().unwrap(), Current::Invalid(None));
        assert_eq!(store.set(true, None, "test", 2), Err(PolicyError::Invalid));
        memory.fault(Op::Delete, POLICY, Fault::Refuse, 0, 1);
        assert_eq!(
            store.set(false, None, "test", 3),
            Err(PolicyError::Unavailable)
        );
        let off = store.set(false, None, "test", 4).unwrap().unwrap();
        assert!(!off.enabled && off.valid());
        assert!(matches!(store.current().unwrap(), Current::Valid(policy, _) if policy == off));
    }

    #[test]
    fn an_unconfirmed_write_is_settled_by_reading_back() {
        let (memory, _, _) = setup();
        let store = policies(&memory);
        memory.fault(Op::Put, POLICY, Fault::After, 0, 1);
        assert!(store.set(true, None, "test", 1).unwrap().unwrap().enabled);
        memory.fault(Op::Put, POLICY, Fault::Before, 0, 1);
        memory.fault(Op::Get, POLICY, Fault::Before, 1, 1);
        assert_eq!(
            store.set(false, None, "test", 2),
            Err(PolicyError::Indeterminate)
        );
        assert!(store.current().unwrap().enabled());
    }

    #[test]
    fn drill_missing_invalid_or_off_policy_never_probes_or_fences() {
        let (memory, _, before) = setup();
        let fake = Fake::down();
        let mut supervisor = supervisor(&memory, &fake, 2);
        for _ in 0..5 {
            assert_eq!(supervisor.tick(), Tick::Disabled);
        }
        memory.set_raw(POLICY, b"not json");
        assert_eq!(supervisor.tick(), Tick::Disabled);
        policies(&memory).set(false, None, "test", 1).unwrap();
        assert_eq!(supervisor.tick(), Tick::Disabled);
        memory.fault(Op::Get, POLICY, Fault::Refuse, 0, 1);
        assert_eq!(supervisor.tick(), Tick::Held("policy_unavailable"));
        assert_eq!(fake.fences(), 0);
        assert_eq!(authority(&memory).read().unwrap().record, before.record);
    }

    #[test]
    fn drill_threshold_promotion_transfers_once_and_a_standby_can_claim() {
        let (memory, _, before) = setup();
        enable(&memory);
        let fake = Fake::down();
        let mut supervisor = supervisor(&memory, &fake, 3);
        seen_ready(&mut supervisor, &fake);
        assert_eq!(supervisor.tick(), Tick::Suspected(1));
        assert_eq!(supervisor.tick(), Tick::Suspected(2));
        let promoted = supervisor.tick();
        let Tick::Promoted {
            old_epoch,
            new_epoch,
            report,
            standby,
        } = &promoted
        else {
            panic!("promotion");
        };
        let (old_epoch, new_epoch) = (*old_epoch, *new_epoch);
        assert_eq!(old_epoch, before.record.epoch);
        assert!(new_epoch > old_epoch);
        assert_eq!(*standby, "started");
        // AC-009: the accepted report names the archived position and, with
        // no observation of the lost writer, never states a known loss.
        assert_eq!(report.epoch, old_epoch);
        assert_eq!(report.revision, before.record.revision);
        assert_eq!(report.archived, before.record.head);
        assert_eq!(report.archived.as_ref().map(|head| head.sequence), Some(10));
        assert_eq!(report.loss, Loss::Unknown);
        assert!(!report.report_id.is_empty());
        let output = promoted.json();
        assert_eq!(output["report_id"], report.report_id);
        assert_eq!(output["report"]["report_id"], report.report_id);
        assert_eq!(output["report"]["loss"]["kind"], "unknown");
        assert_eq!(output["text"], report.render());
        assert!(
            report
                .render()
                .contains("All commits after archived sequence 10 are lost.")
        );
        assert_eq!(
            *fake.fenced.lock().unwrap(),
            [(old_epoch, before.record.writer_id.clone())]
        );
        assert_eq!(*fake.promoted.lock().unwrap(), [(old_epoch, new_epoch)]);
        let store = authority(&memory);
        let transferred = store.read().unwrap();
        assert_eq!(transferred.record.epoch, new_epoch);
        assert!(transferred.record.writer_id.is_none());
        assert_eq!(supervisor.tick(), Tick::Suspected(1));
        assert_eq!(supervisor.tick(), Tick::Suspected(2));
        assert_eq!(supervisor.tick(), Tick::Held("no_serving_writer"));
        assert_eq!(fake.fences(), 1);
        let (claim, _) = store.claim(&transferred, 99).unwrap();
        assert_eq!(store.verify_writer(&claim).unwrap().record.epoch, new_epoch);
    }

    #[test]
    fn drill_a_healthy_probe_or_a_moving_authority_resets_the_count() {
        let (memory, claim, served) = setup();
        enable(&memory);
        let fake = Fake::down();
        let mut supervisor = supervisor(&memory, &fake, 2);
        seen_ready(&mut supervisor, &fake);
        assert_eq!(supervisor.tick(), Tick::Suspected(1));
        *fake.healthy.lock().unwrap() = true;
        assert_eq!(supervisor.tick(), Tick::Healthy);
        *fake.healthy.lock().unwrap() = false;
        assert_eq!(supervisor.tick(), Tick::Suspected(1));
        authority(&memory)
            .advance_head(&claim, &served, head(1, 11))
            .unwrap();
        assert_eq!(supervisor.tick(), Tick::Suspected(1));
        policies(&memory).set(false, None, "test", 2).unwrap();
        assert_eq!(supervisor.tick(), Tick::Disabled);
        let Current::Valid(off, _) = policies(&memory).current().unwrap() else {
            panic!("policy");
        };
        policies(&memory)
            .set(true, Some(&off.revision), "test", 3)
            .unwrap();
        assert_eq!(supervisor.tick(), Tick::Suspected(1));
        assert_eq!(fake.fences(), 0);
    }

    #[test]
    fn drill_a_refused_probe_holds_without_counting() {
        let (memory, _, before) = setup();
        enable(&memory);
        let fake = Fake::down();
        let mut supervisor = supervisor(&memory, &fake, 2);
        seen_ready(&mut supervisor, &fake);
        assert_eq!(supervisor.tick(), Tick::Suspected(1));
        *fake.refused.lock().unwrap() = Some("probe_unauthorized");
        for _ in 0..5 {
            assert_eq!(supervisor.tick(), Tick::Held("probe_unauthorized"));
        }
        *fake.refused.lock().unwrap() = None;
        assert_eq!(supervisor.tick(), Tick::Suspected(1));
        assert_eq!(fake.fences(), 0);
        assert_eq!(authority(&memory).read().unwrap().record, before.record);
    }

    #[test]
    fn drill_a_writer_never_seen_ready_is_never_fenced() {
        let (memory, _, served) = setup();
        enable(&memory);
        let fake = Fake::down();
        let mut supervisor = supervisor(&memory, &fake, 2);
        assert_eq!(supervisor.tick(), Tick::Suspected(1));
        for _ in 0..3 {
            assert_eq!(supervisor.tick(), Tick::Held("writer_never_seen_healthy"));
        }
        // Writer A was seen ready, then authority moved to writer B, which
        // this supervisor has never reached.
        seen_ready(&mut supervisor, &fake);
        let store = authority(&memory);
        let moved = store
            .prepare_transfer(&served, served.record.epoch, &served.record.revision)
            .unwrap();
        let (claim, claimed) = store.claim(&moved, 43).unwrap();
        let next = store
            .advance_head(&claim, &claimed, head(moved.record.epoch, 11))
            .unwrap();
        assert_ne!(next.record.writer_id, served.record.writer_id);
        assert_eq!(supervisor.tick(), Tick::Suspected(1));
        assert_eq!(supervisor.tick(), Tick::Held("writer_never_seen_healthy"));
        assert_eq!(fake.fences(), 0);
        assert_eq!(store.read().unwrap().record, next.record);
    }

    #[test]
    fn drill_a_change_just_before_the_fence_stops_without_fencing() {
        let (memory, claim, served) = setup();
        enable(&memory);
        let fake = Fake::down();
        let mut supervisor = supervisor(&memory, &fake, 2);
        seen_ready(&mut supervisor, &fake);
        assert_eq!(supervisor.tick(), Tick::Suspected(1));
        // The writer publishes after this tick read the authority.
        let writer = memory.clone();
        *fake.during_probe.lock().unwrap() = Some(Box::new(move || {
            authority(&writer)
                .advance_head(&claim, &served, head(1, 11))
                .unwrap();
        }));
        assert_eq!(supervisor.tick(), Tick::Held("authority_changed"));
        assert_eq!(supervisor.tick(), Tick::Suspected(1));
        // The kill switch turns off after this tick read the policy.
        let switch = memory.clone();
        *fake.during_probe.lock().unwrap() = Some(Box::new(move || {
            policies(&switch).set(false, None, "operator", 9).unwrap();
        }));
        assert_eq!(supervisor.tick(), Tick::Disabled);
        assert_eq!(supervisor.tick(), Tick::Disabled);
        assert_eq!(fake.fences(), 0);
    }

    #[test]
    fn drill_a_failed_or_unconfirmed_fence_never_transfers() {
        for replicas in [None, Some(1)] {
            let (memory, _, before) = setup();
            enable(&memory);
            let fake = Fake::default();
            *fake.replicas.lock().unwrap() = replicas;
            let mut supervisor = supervisor(&memory, &fake, 2);
            seen_ready(&mut supervisor, &fake);
            supervisor.tick();
            assert_eq!(
                supervisor.tick(),
                Tick::Aborted(if replicas.is_none() {
                    "fence_failed"
                } else {
                    "fence_unconfirmed"
                })
            );
            assert_eq!(authority(&memory).read().unwrap().record, before.record);
            assert_eq!(fake.promotes(), 0);
        }
    }

    #[test]
    fn drill_the_kill_switch_during_the_fence_aborts_before_any_transfer() {
        let (memory, _, before) = setup();
        enable(&memory);
        let fake = Fake::down();
        let switch = memory.clone();
        *fake.during_fence.lock().unwrap() = Some(Box::new(move || {
            policies(&switch).set(false, None, "operator", 9).unwrap();
        }));
        let mut supervisor = supervisor(&memory, &fake, 2);
        seen_ready(&mut supervisor, &fake);
        supervisor.tick();
        assert_eq!(
            supervisor.tick(),
            Tick::Aborted("disabled_during_promotion")
        );
        assert_eq!(authority(&memory).read().unwrap().record, before.record);
        assert_eq!(fake.promotes(), 0);
    }

    #[test]
    fn drill_an_unreadable_policy_after_the_fence_aborts_before_any_transfer() {
        let (memory, _, before) = setup();
        enable(&memory);
        let fake = Fake::down();
        let faults = memory.clone();
        *fake.during_fence.lock().unwrap() = Some(Box::new(move || {
            faults.fault(Op::Get, POLICY, Fault::Refuse, 0, 1);
        }));
        let mut supervisor = supervisor(&memory, &fake, 2);
        seen_ready(&mut supervisor, &fake);
        supervisor.tick();
        let tick = supervisor.tick();
        assert_eq!(tick, Tick::Aborted("policy_unavailable_during_promotion"));
        assert!(outcome(&tick).is_err());
        assert_eq!(authority(&memory).read().unwrap().record, before.record);
        assert_eq!(fake.promotes(), 0);
    }

    #[test]
    fn drill_a_standby_that_fails_to_start_exits_nonzero_after_the_transfer() {
        let (memory, _, before) = setup();
        enable(&memory);
        let fake = Fake::down();
        *fake.promote_fails.lock().unwrap() = true;
        let mut supervisor = supervisor(&memory, &fake, 2);
        seen_ready(&mut supervisor, &fake);
        supervisor.tick();
        let tick = supervisor.tick();
        let Tick::Promoted {
            new_epoch, standby, ..
        } = &tick
        else {
            panic!("promotion");
        };
        assert_eq!(*standby, "start_failed");
        assert_eq!(authority(&memory).read().unwrap().record.epoch, *new_epoch);
        let error = outcome(&tick).unwrap_err();
        assert!(
            error.starts_with(&format!(
                "auto_failover_standby_failed: authority moved from epoch {} to {new_epoch}",
                before.record.epoch
            )),
            "{error}"
        );
    }

    #[test]
    fn drill_an_authority_change_during_the_fence_aborts() {
        let (memory, claim, served) = setup();
        enable(&memory);
        let fake = Fake::down();
        let writer = memory.clone();
        *fake.during_fence.lock().unwrap() = Some(Box::new(move || {
            authority(&writer)
                .advance_head(&claim, &served, head(1, 12))
                .unwrap();
        }));
        let mut supervisor = supervisor(&memory, &fake, 2);
        seen_ready(&mut supervisor, &fake);
        supervisor.tick();
        let tick = supervisor.tick();
        assert_eq!(tick, Tick::Aborted("authority_changed"));
        // The command exits nonzero on an abort, never as a success.
        assert!(
            outcome(&tick)
                .unwrap_err()
                .starts_with("auto_failover_aborted: authority_changed;")
        );
        assert!(
            authority(&memory)
                .read()
                .unwrap()
                .record
                .writer_id
                .is_some()
        );
    }

    #[test]
    fn drill_two_supervisors_promote_once() {
        let (memory, _, _) = setup();
        enable(&memory);
        let (first, second) = (Fake::down(), Fake::down());
        let mut a = supervisor(&memory, &first, 2);
        let mut b = supervisor(&memory, &second, 2);
        seen_ready(&mut a, &first);
        seen_ready(&mut b, &second);
        a.tick();
        b.tick();
        let other = memory.clone();
        // b fences while a completes its transfer.
        *second.during_fence.lock().unwrap() = Some(Box::new(move || {
            let fake = Fake::down();
            let mut a = supervisor(&other, &fake, 1);
            seen_ready(&mut a, &fake);
            assert!(matches!(a.tick(), Tick::Promoted { .. }));
        }));
        assert_eq!(b.tick(), Tick::Aborted("authority_changed"));
        assert_eq!(a.tick(), Tick::Suspected(1));
        assert_eq!(a.tick(), Tick::Held("no_serving_writer"));
        assert_eq!(first.promotes(), 0);
        assert_eq!(second.promotes(), 0);
    }

    #[test]
    fn config_rejects_unsafe_values() {
        let base = json!({
            "target_file": "target.json",
            "writer_ready_url": "http://writer:8080/readyz",
            "probe_interval_ms": 1000,
            "probe_timeout_ms": 500,
            "failure_threshold": 3,
            "fence_command": ["fence.sh"],
        });
        let parse = |patch: Value| -> Result<(), String> {
            let mut value = base.clone();
            value
                .as_object_mut()
                .unwrap()
                .extend(patch.as_object().unwrap().clone());
            serde_json::from_value::<Config>(value)
                .map_err(|error| error.to_string())?
                .validate()
        };
        assert!(parse(json!({})).is_ok());
        assert!(parse(json!({"probe_token_file": "token"})).is_ok());
        for patch in [
            json!({"writer_ready_url": "file:///etc/passwd"}),
            json!({"probe_interval_ms": 100}),
            json!({"probe_timeout_ms": 1000}),
            json!({"failure_threshold": 1}),
            json!({"fence_command": []}),
            json!({"promote_command": [""]}),
            json!({"command_timeout_ms": 10}),
            json!({"shell": "sh -c"}),
        ] {
            assert!(parse(patch.clone()).is_err(), "{patch}");
        }
    }

    #[test]
    fn the_probe_token_is_one_printable_token() {
        let dir = tempfile::tempdir().unwrap();
        let file = |name: &str, bytes: &[u8]| {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        };
        assert_eq!(
            read_token(file("ok", b"  abc.DEF-123\r\n")).unwrap(),
            "abc.DEF-123"
        );
        for (name, bytes) in [
            ("empty", &b"\n"[..]),
            ("space", b"two words"),
            ("binary", b"\xff\xfe"),
            ("long", &[b'a'; 4097][..]),
        ] {
            assert!(
                read_token(file(name, bytes))
                    .unwrap_err()
                    .starts_with("invalid_config: probe_token_file"),
                "{name}"
            );
        }
        assert!(read_token(dir.path().join("missing")).is_err());
    }

    #[test]
    fn policy_command_requires_the_unqualified_acknowledgement() {
        let args = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(
            policy_command(&args(&["enable", "--target-file", "missing.json"]))
                .unwrap_err()
                .contains("--acknowledge-unqualified true")
        );
        assert!(
            policy_command(&args(&["toggle"]))
                .unwrap_err()
                .starts_with("invalid_arguments: the verb must be show, enable or disable")
        );
        assert!(
            policy_command(&args(&[
                "show",
                "--expected-revision",
                "x",
                "--target-file",
                "t"
            ]))
            .is_err()
        );
        assert!(
            policy_command(&args(&[
                "enable",
                "--expected-revision",
                "X".repeat(32).as_str(),
                "--acknowledge-unqualified",
                "true",
                "--target-file",
                "missing.json"
            ]))
            .unwrap_err()
            .contains("--expected-revision must be 32 lowercase hexadecimal characters")
        );
    }

    #[cfg(unix)]
    #[test]
    fn commands_run_without_a_shell_and_time_out() {
        let argv = |list: &[&str]| list.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            run_command(&argv(&["echo", "$HOME"]), &[], Duration::from_secs(5)).unwrap(),
            b"$HOME\n"
        );
        assert_eq!(
            run_command(
                &argv(&["printenv", "VLITE_FAILOVER_EPOCH"]),
                &[("VLITE_FAILOVER_EPOCH", "7".into())],
                Duration::from_secs(5)
            )
            .unwrap(),
            b"7\n"
        );
        assert_eq!(
            run_command(&argv(&["false"]), &[], Duration::from_secs(5)),
            Err("command_failed".into())
        );
        assert_eq!(
            run_command(&argv(&["sleep", "5"]), &[], Duration::from_millis(200)),
            Err("command_timed_out".into())
        );
        assert_eq!(
            run_command(
                &argv(&["head", "-c", "70000", "/dev/zero"]),
                &[],
                Duration::from_secs(5)
            ),
            Err("command_output_too_large".into())
        );
    }

    mod rest {
        use super::*;
        use crate::server::writer_fence::{State as FenceState, WriterFence};
        use crate::server::*;
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;

        const ADMIN_TOKEN: &str = "auto-failover-admin-test-token-0123456789";

        fn service() -> Service {
            let service = Service::new(
                Database::create_in_memory(1, crate::CreateOptions::default()).unwrap(),
            );
            service.admin.auth.lock().unwrap().admin = Some(BearerToken::new(ADMIN_TOKEN).unwrap());
            service
        }

        fn writer(memory: &Arc<Memory>) -> Service {
            let store = authority(memory);
            let created = store
                .create("installation", "scope", "0123456789abcdef")
                .unwrap();
            let (claim, _) = store.claim(&created, 1).unwrap();
            let fence = WriterFence::new(store, claim);
            fence.transition(FenceState::Recovering).unwrap();
            fence.transition(FenceState::Serving).unwrap();
            service()
                .with_serve_mode(ServeMode::Writer)
                .unwrap()
                .with_writer_fence(fence)
                .unwrap()
        }

        async fn call(router: &Router, method: &str, body: &str) -> (StatusCode, Value) {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri("/v1/admin/auto-failover")
                        .header(header::CONTENT_TYPE, "application/json")
                        .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
                        .body(Body::from(body.to_owned()))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap();
            (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
        }

        #[tokio::test]
        async fn open_administration_cannot_change_the_policy() {
            let service = Service::new(
                Database::create_in_memory(1, crate::CreateOptions::default()).unwrap(),
            );
            let router = router(service);
            let (status, body) = call(&router, "PUT", r#"{"enabled":false}"#).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
            assert_eq!(body["error"]["code"], "admin_auth_required");
        }

        #[tokio::test]
        async fn without_an_authority_it_is_unavailable_and_cannot_be_turned_on() {
            let router = authenticated_router(service(), BearerToken::new(ADMIN_TOKEN).unwrap());
            let (status, body) = call(&router, "GET", "").await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["available"], false);
            assert_eq!(body["enabled"], false);
            assert_eq!(body["qualified"], false);
            assert_eq!(body["gate"], "GATE-005");
            let (status, body) = call(
                &router,
                "PUT",
                r#"{"enabled":true,"acknowledge_unqualified":true}"#,
            )
            .await;
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            assert_eq!(body["error"]["code"], "auto_failover_not_configured");
        }

        #[tokio::test]
        async fn a_writer_turns_it_on_with_acknowledgement_and_off_without() {
            let memory = Arc::new(Memory::default());
            let router =
                authenticated_router(writer(&memory), BearerToken::new(ADMIN_TOKEN).unwrap());
            let (status, body) = call(&router, "GET", "").await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["available"], true);
            assert_eq!(body["enabled"], false);
            assert_eq!(body["revision"], Value::Null);
            let (status, body) = call(&router, "PUT", r#"{"enabled":true}"#).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(body["error"]["code"], "auto_failover_unacknowledged");
            let (status, body) = call(&router, "PUT", r#"{"enabled":true,"other":1}"#).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(body["error"]["code"], "invalid_json");
            let (status, body) = call(
                &router,
                "PUT",
                r#"{"enabled":true,"acknowledge_unqualified":true,"expected_revision":"x"}"#,
            )
            .await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(body["error"]["code"], "invalid_json");
            let (status, on) = call(
                &router,
                "PUT",
                r#"{"enabled":true,"acknowledge_unqualified":true}"#,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{on}");
            assert_eq!(on["enabled"], true);
            assert_eq!(on["source"], "rest:admin_token");
            assert!(policies(&memory).current().unwrap().enabled());
            let (status, body) = call(
                &router,
                "PUT",
                r#"{"enabled":true,"acknowledge_unqualified":true}"#,
            )
            .await;
            assert_eq!(status, StatusCode::CONFLICT);
            assert_eq!(body["error"]["code"], "auto_failover_conflict");
            let (status, off) = call(
                &router,
                "PUT",
                r#"{"enabled":false,"expected_revision":"x"}"#,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{off}");
            assert_eq!(off["enabled"], false);
            assert_ne!(off["revision"], on["revision"]);
            let (_, body) = call(&router, "GET", "").await;
            assert_eq!(body["enabled"], false);
            assert_eq!(body["revision"], off["revision"]);
        }

        #[test]
        fn the_recorded_source_is_bounded_and_printable() {
            assert_eq!(policy_source("admin_token"), "rest:admin_token");
            assert_eq!(policy_source("a\u{7}b\nc"), "rest:abc");
            let long = policy_source(&"é".repeat(400));
            assert!(long.len() <= 256 && long.len() > 250, "{}", long.len());
            assert!(long.starts_with("rest:é"));
            assert!(valid_source(&long));
        }
    }
}
