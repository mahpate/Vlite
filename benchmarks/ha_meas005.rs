#[path = "ha_gateway_support.rs"]
mod support;

use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use support::*;

fn polls() -> u64 {
    1000
}
fn observe_interval() -> u64 {
    50
}
fn observe_duration() -> u64 {
    1200
}
fn deadline() -> u64 {
    30000
}
fn mutations() -> usize {
    20
}
fn seed() -> u64 {
    1
}
fn timeout() -> u64 {
    1000
}

#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
enum Config {
    Run(Run),
    Observe(Observe),
    Analyze(Analyze),
    ClockSample(ClockSample),
    ClockReference(ClockReference),
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClockSample {}
/// A bounded reference-clock endpoint for replica-initiated polling inside an
/// isolated test environment. It supplies samples only, never qualification.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClockReference {
    bind: String,
    #[serde(default = "observe_duration")]
    duration_secs: u64,
}
impl ClockReference {
    fn validate(&self) -> std::result::Result<(), &'static str> {
        if self.bind.parse::<std::net::SocketAddr>().is_err() {
            return Err("clock_reference_bind_invalid");
        }
        if !(1..=7200).contains(&self.duration_secs) {
            return Err("duration_secs_out_of_range");
        }
        Ok(())
    }
}
fn reference_interval() -> u64 {
    1000
}
/// Optional observer polling of a `clock_reference` endpoint. The URL is
/// private configuration and never appears in evidence.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReferencePoll {
    url: String,
    #[serde(default = "reference_interval")]
    interval_ms: u64,
    #[serde(default = "timeout")]
    timeout_ms: u64,
}
impl ReferencePoll {
    fn validate(&self, duration_secs: u64) -> std::result::Result<(), &'static str> {
        let url = reqwest::Url::parse(&self.url).map_err(|_| "clock_reference_url_invalid")?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/clock"
        {
            return Err("clock_reference_url_invalid");
        }
        if !(100..=60000).contains(&self.interval_ms) {
            return Err("clock_reference_interval_ms_out_of_range");
        }
        if !(10..=1000).contains(&self.timeout_ms) {
            return Err("clock_reference_timeout_ms_out_of_range");
        }
        if duration_secs.saturating_mul(1000) / self.interval_ms + 1
            > MAX_REFERENCE_EXCHANGES as u64
        {
            return Err("clock_reference_sample_limit");
        }
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Run {
    server: PathBuf,
    bootstrap_dir: PathBuf,
    target_file: PathBuf,
    object: String,
    shared_key_file: Option<PathBuf>,
    gateway_url: String,
    #[serde(default = "polls")]
    poll_interval_ms: u64,
    #[serde(default = "deadline")]
    observation_deadline_ms: u64,
    #[serde(default = "mutations")]
    mutations: usize,
    #[serde(default = "seed")]
    seed: u64,
    requested: BTreeMap<String, String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Observe {
    server: Program,
    ready_url: String,
    role: String,
    /// Only for local smoke runs. ACA requires actual platform identity env.
    local_replica: Option<String>,
    #[serde(default = "observe_interval")]
    interval_ms: u64,
    #[serde(default = "observe_duration")]
    duration_secs: u64,
    #[serde(default = "timeout")]
    timeout_ms: u64,
    clock_reference: Option<ReferencePoll>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Analyze {
    mutation_evidence_file: PathBuf,
    replicas: Vec<Replica>,
    /// The harness must inventory the same app/revision/replica set before
    /// and after. False/null never establishes fixed-set coverage.
    fixed_inventory_confirmed: Option<bool>,
    /// Cross-clock calibration policy; absent means every replica is rejected.
    calibration: Option<CalibrationPolicy>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Replica {
    replica: String,
    role: String,
    report_file: PathBuf,
    /// Exchanges against the policy reference clock bracketing the run.
    calibration: Option<Bracket>,
}
fn resolution() -> f64 {
    1.
}
const MAX_EXCHANGES: usize = 64;
const MAX_REFERENCE_EXCHANGES: usize = 4096;
const REFERENCE_SCHEMA: &str = "vlite-meas005-clock-reference/1";
const REFERENCE_QUALIFICATION: &str = "diagnostic_only_motion_between_samples_unobserved";
/// Absolute budget for reading one request head, measured from accept.
const REFERENCE_CONNECTION_MS: u64 = 1000;
/// Bound on theta = subject - reference from one replica-initiated exchange
/// `[subject send, reference receive, reference reply, subject receive]`, all
/// floor-rounded unix ms: theta lies in [s0 - r1 - 1, s3 - r2 + 1]. Returns
/// None for orderings that no causal exchange can produce.
fn reference_bounds(sample: &[u64; 4]) -> Option<(f64, f64)> {
    let [s0, r1, r2, s3] = sample.map(|x| x as f64);
    let bounds = (s0 - r1 - 1., s3 - r2 + 1.);
    (s3 >= s0 && r2 >= r1 && bounds.0 <= bounds.1).then_some(bounds)
}
/// Serves `GET /clock` until `until`, one bounded connection at a time.
/// Returns the number of clock replies written. The server is serial, so a
/// connection waits behind at most one other connection's budget (reading
/// is capped at `REFERENCE_CONNECTION_MS` from accept, writing at 1 s).
fn serve_reference(
    listener: std::net::TcpListener,
    until: Instant,
) -> std::result::Result<u64, &'static str> {
    use std::io::{ErrorKind, Read, Write};
    listener
        .set_nonblocking(true)
        .map_err(|_| "clock_reference_listener_failed")?;
    let mut served = 0u64;
    while Instant::now() < until {
        let mut stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            // One peer aborting its own connection must not stop the
            // reference for every other replica.
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::ConnectionAborted
                        | ErrorKind::ConnectionReset
                        | ErrorKind::Interrupted
                        | ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(_) => return Err("clock_reference_accept_failed"),
        };
        // Per-read timeouts alone let a slow, dripping client hold this
        // serial server indefinitely, so reading has one absolute deadline.
        let connection_deadline = Instant::now() + Duration::from_millis(REFERENCE_CONNECTION_MS);
        let configured = stream.set_nonblocking(false).is_ok()
            && stream
                .set_write_timeout(Some(Duration::from_millis(1000)))
                .is_ok();
        let mut head = Vec::new();
        let mut buffer = [0u8; 512];
        while configured && head.len() <= 2048 && !head.windows(4).any(|w| w == b"\r\n\r\n") {
            let remaining = connection_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() || stream.set_read_timeout(Some(remaining)).is_err() {
                break;
            }
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => head.extend_from_slice(&buffer[..n]),
            }
        }
        // Receive time is taken only after the full request head arrived.
        let receive = now_ms();
        let clock = head.windows(4).any(|w| w == b"\r\n\r\n")
            && (head.starts_with(b"GET /clock HTTP/1.1\r\n")
                || head.starts_with(b"GET /clock HTTP/1.0\r\n"));
        let (status, body) = if clock {
            let reply = now_ms();
            (
                "200 OK",
                json!({"schema":REFERENCE_SCHEMA,"receive_unix_ms":receive,"reply_unix_ms":reply})
                    .to_string(),
            )
        } else {
            ("404 Not Found", "{}".to_string())
        };
        let written = stream.write_all(
            format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        );
        if clock && written.is_ok() {
            served += 1;
        }
    }
    Ok(served)
}
/// One replica-initiated exchange; any transport, status or shape fault, or a
/// causally impossible ordering, yields None (counted as a failure).
fn poll_reference(client: &reqwest::blocking::Client, url: &str) -> Option<[u64; 4]> {
    let send = now_ms();
    let reply = request(client, url, "", "").ok()?;
    let receive = now_ms();
    if reply.status != 200 || reply.body["schema"] != REFERENCE_SCHEMA {
        return None;
    }
    let sample = [
        send,
        reply.body["receive_unix_ms"].as_u64()?,
        reply.body["reply_unix_ms"].as_u64()?,
        receive,
    ];
    reference_bounds(&sample).map(|_| sample)
}
/// Sampled diagnostics only: the samples observe the offset at discrete
/// points, so motion between samples stays unobserved and no continuity
/// envelope is derived.
fn reference_diagnostics(collected: &Value) -> Value {
    if collected.is_null() {
        return Value::Null;
    }
    let status = |status: &str| {
        json!({"status":status,"continuity_envelope_ms":null,
            "qualification":REFERENCE_QUALIFICATION})
    };
    let invalid = status("invalid");
    let Some(exchanges) = collected["exchanges"]
        .as_array()
        .filter(|x| x.len() <= MAX_REFERENCE_EXCHANGES)
    else {
        return invalid;
    };
    // Every attempt either yields one exchange or counts as one failure.
    let (Some(attempts), Some(failures)) = (
        collected["attempts"].as_u64(),
        collected["failures"].as_u64(),
    ) else {
        return invalid;
    };
    if failures > attempts || attempts - failures != exchanges.len() as u64 {
        return invalid;
    }
    if exchanges.is_empty() {
        let mut empty = status("no_samples");
        empty["attempts"] = json!(attempts);
        empty["failures"] = json!(failures);
        return empty;
    }
    let mut samples = Vec::new();
    for exchange in exchanges {
        let field = |name: &str| exchange[name].as_u64();
        let (Some(s0), Some(r1), Some(r2), Some(s3)) = (
            field("subject_send_unix_ms"),
            field("reference_receive_unix_ms"),
            field("reference_reply_unix_ms"),
            field("subject_receive_unix_ms"),
        ) else {
            return invalid;
        };
        let Some(bounds) = reference_bounds(&[s0, r1, r2, s3]) else {
            return invalid;
        };
        // Polls are sequential, so a send before the previous receive means
        // the subject's wall clock moved backward (evidence, not malformed).
        if samples
            .last()
            .is_some_and(|(previous, _): &([u64; 4], _)| previous[3] > s0)
        {
            return status("subject_clock_stepped_backward");
        }
        samples.push(([s0, r1, r2, s3], bounds));
    }
    let midpoints: Vec<f64> = samples.iter().map(|(_, (l, u))| (l + u) / 2.).collect();
    let lowest_upper = samples
        .iter()
        .map(|(_, b)| b.1)
        .fold(f64::INFINITY, f64::min);
    let highest_lower = samples
        .iter()
        .map(|(_, b)| b.0)
        .fold(f64::NEG_INFINITY, f64::max);
    let gaps = samples.windows(2).map(|w| w[1].0[0] - w[0].0[0]).max();
    let net = samples
        .iter()
        .map(|(s, _)| (s[3] - s[0]) as f64 - (s[2] - s[1]) as f64)
        .reduce(f64::min);
    let spread = midpoints
        .iter()
        .copied()
        .reduce(f64::max)
        .zip(midpoints.iter().copied().reduce(f64::min))
        .map(|(max, min)| max - min);
    json!({"status":"collected","samples":samples.len(),
        "attempts":attempts,"failures":failures,
        "first_subject_send_unix_ms":samples.first().map(|s| s.0[0]),
        "last_subject_receive_unix_ms":samples.last().map(|s| s.0[3]),
        "max_subject_gap_ms":gaps,"min_net_rtt_ms":net,
        "sampled_midpoint_spread_ms":spread,
        "constant_offset_contradicted":highest_lower > lowest_upper,
        "continuity_envelope_ms":null,
        "qualification":REFERENCE_QUALIFICATION})
}
const MUTATOR_DRIFT_ALLOWANCE_MS: f64 = 10.;
/// One NTP-style exchange: t0/t3 on the reference clock, t1/t2 on the subject
/// clock (subject receive/reply). Values are floor-rounded to `resolution_ms`.
#[derive(Deserialize, Clone, Copy)]
#[serde(deny_unknown_fields)]
struct Exchange {
    t0_ms: f64,
    t1_ms: f64,
    t2_ms: f64,
    t3_ms: f64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Bracket {
    pre: Vec<Exchange>,
    post: Vec<Exchange>,
}
#[derive(Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Reference {
    /// t0/t3 were read from the mutator's own clock.
    Mutator,
    /// t0/t3 were read from a common reference; `mutator` is then required.
    Common,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CalibrationPolicy {
    reference: Reference,
    mutator: Option<Bracket>,
    min_samples: usize,
    /// Upper bound on the chosen exchange's RTT net of subject processing.
    max_rtt_ms: f64,
    /// Largest clock error any corrected latency bound may carry.
    slo_margin_ms: f64,
    #[serde(default = "resolution")]
    resolution_ms: f64,
}
type Rejection = &'static str;
#[derive(Clone, Copy)]
struct Estimate {
    /// Subject clock minus reference clock at `at` (reference ms).
    offset: f64,
    error: f64,
    at: f64,
    rtt: f64,
    samples: usize,
    start: f64,
    end: f64,
}
fn policy_valid(policy: &CalibrationPolicy) -> bool {
    (1..=MAX_EXCHANGES).contains(&policy.min_samples)
        && policy.max_rtt_ms.is_finite()
        && policy.max_rtt_ms > 0.
        && policy.max_rtt_ms <= 60000.
        && policy.slo_margin_ms.is_finite()
        && policy.slo_margin_ms > 0.
        && policy.slo_margin_ms <= 60000.
        && (0.0..=1000.).contains(&policy.resolution_ms)
}
/// Causality bounds the offset to [t2-t3-r, t1-t0+r] under any delay asymmetry;
/// the min-RTT exchange supplies the midpoint and half-width.
fn estimate(
    exchanges: &[Exchange],
    policy: &CalibrationPolicy,
) -> std::result::Result<Estimate, Rejection> {
    let r = policy.resolution_ms;
    let net = |e: &Exchange| (e.t3_ms - e.t0_ms) - (e.t2_ms - e.t1_ms);
    if exchanges.len() > MAX_EXCHANGES
        || exchanges.iter().any(|e| {
            ![e.t0_ms, e.t1_ms, e.t2_ms, e.t3_ms]
                .iter()
                .all(|x| x.is_finite())
                || e.t3_ms < e.t0_ms
                || e.t2_ms < e.t1_ms
                || net(e) < -2. * r
        })
    {
        return Err("calibration_exchange_invalid");
    }
    if exchanges.len() < policy.min_samples {
        return Err("calibration_too_few_samples");
    }
    let best = exchanges
        .iter()
        .min_by(|a, b| net(a).total_cmp(&net(b)))
        .ok_or("calibration_too_few_samples")?;
    if net(best) > policy.max_rtt_ms {
        return Err("calibration_rtt_exceeds_bound");
    }
    let low = exchanges
        .iter()
        .map(|e| e.t2_ms - e.t3_ms - r)
        .fold(f64::NEG_INFINITY, f64::max);
    let high = exchanges
        .iter()
        .map(|e| e.t1_ms - e.t0_ms + r)
        .fold(f64::INFINITY, f64::min);
    if low > high {
        return Err("calibration_exchange_inconsistent");
    }
    Ok(Estimate {
        offset: ((best.t1_ms - best.t0_ms) + (best.t2_ms - best.t3_ms)) / 2.,
        error: net(best) / 2. + r,
        at: (best.t0_ms + best.t3_ms) / 2.,
        rtt: net(best),
        samples: exchanges.len(),
        start: exchanges
            .iter()
            .map(|e| e.t0_ms)
            .fold(f64::INFINITY, f64::min),
        end: exchanges
            .iter()
            .map(|e| e.t3_ms)
            .fold(f64::NEG_INFINITY, f64::max),
    })
}
struct Clock {
    pre: Estimate,
    post: Estimate,
    /// Independent run-wide bound on |theta(t) - theta(endpoint)| for any t
    /// between the brackets. Endpoint offsets alone bound only the mean
    /// change: an interior excursion that returns to baseline is invisible to
    /// them, so without this bound every interior offset is unknown.
    envelope: Option<f64>,
}
fn bracket(bracket: &Bracket, policy: &CalibrationPolicy) -> std::result::Result<Clock, Rejection> {
    let pre = estimate(&bracket.pre, policy)?;
    let post = estimate(&bracket.post, policy)?;
    if pre.end >= post.start {
        return Err("calibration_does_not_bracket_run");
    }
    // No evidence source yet supplies a measured run-wide continuity envelope.
    Ok(Clock {
        pre,
        post,
        envelope: None,
    })
}
impl Clock {
    fn continuity(&self) -> std::result::Result<(), Rejection> {
        let envelope = self
            .envelope
            .filter(|e| e.is_finite() && *e >= 0.)
            .ok_or("calibration_continuity_unbounded")?;
        if (self.post.offset - self.pre.offset).abs() > envelope + self.pre.error + self.post.error
        {
            return Err("calibration_endpoints_exceed_continuity_envelope");
        }
        Ok(())
    }
    /// Nearest calibration offset, widened by the run-wide envelope (no rate is
    /// assumed). Unbounded continuity yields an infinite error.
    fn at(&self, reference_ms: f64) -> (f64, f64) {
        let near = if (reference_ms - self.pre.at).abs() <= (self.post.at - reference_ms).abs() {
            &self.pre
        } else {
            &self.post
        };
        (
            near.offset,
            near.error + self.envelope.unwrap_or(f64::INFINITY),
        )
    }
    fn json(&self) -> Value {
        let side = |e: &Estimate| {
            json!({"offset_ms":e.offset,"error_ms":e.error,"at_reference_unix_ms":e.at,
                "min_rtt_net_ms":e.rtt,"samples":e.samples})
        };
        let elapsed = self.post.at - self.pre.at;
        json!({"pre":side(&self.pre),"post":side(&self.post),"elapsed_ms":elapsed,
            "endpoint_mean_rate_ms_per_s":(self.post.offset - self.pre.offset) / elapsed * 1000.,
            "endpoint_rate_bounds_interior":false,
            "continuity_envelope_ms":self.envelope})
    }
}
/// Observer-minus-mutator correction for one replica.
struct Pair<'a> {
    observer: Clock,
    mutator: Option<&'a Clock>,
}
impl Clock {
    /// Offset and error for a reading taken on the subject clock. The envelope
    /// holds over the whole run, so the uncertain reference time does not widen it.
    fn subject(&self, subject_ms: f64) -> (f64, f64) {
        self.at(subject_ms - self.pre.offset)
    }
}
impl Pair<'_> {
    /// (reference time, error) for a reading on the mutator clock.
    fn mutator_reference(&self, mutator_ms: f64) -> (f64, f64) {
        self.mutator.map_or((mutator_ms, 0.), |m| {
            let (offset, error) = m.subject(mutator_ms);
            (mutator_ms - offset, error)
        })
    }
    /// (observer-minus-mutator offset, error, reference time) for an observer reading.
    fn observer_at(&self, observer_ms: f64) -> (f64, f64, f64) {
        let (offset, error) = self.observer.subject(observer_ms);
        let reference = observer_ms - offset;
        let (mutator, mutator_error) = self.mutator.map_or((0., 0.), |m| m.at(reference));
        (offset - mutator, error + mutator_error, reference)
    }
    /// Whether a reference interval lies between every pre and post exchange.
    fn brackets(&self, first: f64, last: f64) -> bool {
        let mut pre_end = self.observer.pre.end;
        let mut post_start = self.observer.post.start;
        if let Some(m) = self.mutator {
            pre_end = pre_end.max(m.pre.end);
            post_start = post_start.min(m.post.start);
        }
        first >= pre_end && last <= post_start
    }
}
fn mutator_clock(
    policy: Option<&CalibrationPolicy>,
) -> std::result::Result<Option<Clock>, Rejection> {
    let policy = policy.ok_or("calibration_missing")?;
    if !policy_valid(policy) {
        return Err("calibration_policy_invalid");
    }
    match (policy.reference, &policy.mutator) {
        (Reference::Mutator, None) => Ok(None),
        (Reference::Mutator, Some(_)) => Err("calibration_policy_invalid"),
        (Reference::Common, None) => Err("calibration_missing"),
        (Reference::Common, Some(bracket_)) => bracket(bracket_, policy).map(Some),
    }
}
#[derive(Deserialize)]
struct Manifest {
    writer_url: String,
    credentials: Vec<Credential>,
}
#[derive(Deserialize)]
struct Credential {
    purpose: String,
    id: String,
    tenant: String,
    token_file: PathBuf,
}

fn safe_requested(values: &BTreeMap<String, String>) -> Result<()> {
    if values.len() > 32 || values.iter().any(|(k, v)| !label(k) || !label(v)) {
        return Err("requested_labels_invalid".into());
    }
    Ok(())
}
fn jitter(state: &mut u64, max: u64) -> u64 {
    // SplitMix64: deterministic even for seed zero.
    *state = state.wrapping_add(0x9e3779b97f4a7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
    (z ^ (z >> 31)) % (max + 1)
}

fn run(config: Run) -> Result<Value> {
    bounded("poll_interval_ms", config.poll_interval_ms, 1, 5000)?;
    bounded(
        "observation_deadline_ms",
        config.observation_deadline_ms,
        100,
        30000,
    )?;
    bounded("mutations", config.mutations as u64, 3, 100)?;
    safe_requested(&config.requested)?;
    if !config.server.is_absolute()
        || config.object.len() > 256
        || config.mutations as u64
            * (config.observation_deadline_ms + config.poll_interval_ms + 138000)
            > 3_540_000
    {
        return Err("command_budget_exceeded".into());
    }
    let gateway = origin(&config.gateway_url)?.to_string();
    let manifest: Manifest = read(&config.bootstrap_dir.join("manifest.json"))?;
    let direct = origin(&manifest.writer_url)?.to_string();
    let direct_url = format!("{}{PATH}", direct.trim_end_matches('/'));
    let load = manifest
        .credentials
        .iter()
        .find(|c| c.purpose == "load")
        .ok_or("manifest_invalid")?;
    let suspension = manifest
        .credentials
        .iter()
        .find(|c| c.purpose == "suspension")
        .ok_or("manifest_invalid")?;
    let revoked: Vec<_> = manifest
        .credentials
        .iter()
        .filter(|c| c.purpose == "revocation")
        .collect();
    let revoke_count = config.mutations / 2;
    if revoked.len() < revoke_count {
        return Err("insufficient_sacrificial_credentials".into());
    }
    let load_token = token(&load.token_file)?;
    let expected: Value = read(&config.bootstrap_dir.join("load.expected.json"))?;
    let suspended_expected: Value = read(&config.bootstrap_dir.join("suspension.expected.json"))?;
    let client = client(1000)?;
    let url = format!("{}{PATH}", gateway.trim_end_matches('/'));
    let authorized = request(
        &client,
        &url,
        "authorization",
        &format!("Bearer {load_token}"),
    )?;
    if authorized.status != 200 || authorized.body != expected {
        return Err("preflight_auth_or_fixture_failed".into());
    }
    let mut state = config.seed;
    let mut records = Vec::new();
    let mut complete = true;
    let mut previous_generation = None;
    for index in 0..config.mutations {
        let delay = jitter(&mut state, config.poll_interval_ms);
        thread::sleep(Duration::from_millis(delay));
        let (kind, flag, credential, desired) = if index < revoke_count {
            (
                "revocation",
                "--revoke-credential",
                *revoked
                    .get(index)
                    .ok_or("insufficient_sacrificial_credentials")?,
                401,
            )
        } else if (index - revoke_count).is_multiple_of(2) {
            ("suspension", "--suspend-tenant", suspension, 403)
        } else {
            ("resumption", "--resume-tenant", suspension, 200)
        };
        let private = tempfile::tempdir().map_err(|_| "envelope_directory_failed")?;
        let envelope_file = private.path().join("envelope");
        let mint_args = vec![
            "gateway-measure".into(),
            "mint".into(),
            "--gateway-config".into(),
            config
                .bootstrap_dir
                .join("gateway.json")
                .to_string_lossy()
                .into_owned(),
            "--token-file".into(),
            load.token_file.to_string_lossy().into_owned(),
            "--output".into(),
            envelope_file.to_string_lossy().into_owned(),
        ];
        let old = match Job::start(&config.server, &mint_args, 15000).and_then(|mut j| j.wait()) {
            Ok(value) => value,
            Err(error) => {
                records.push(json!({"index":index,"kind":kind,"jitter_ms":delay,
                    "outcome":"not_attempted_signing_read_failed","error":error,"generation":null}));
                complete = false;
                break;
            }
        };
        let old_generation = old["generation"]
            .as_u64()
            .ok_or("envelope_generation_missing")?;
        let old_envelope = token(&envelope_file)?;
        let bearer = format!("Bearer {}", token(&credential.token_file)?);
        let mut args = vec![
            "control-state".into(),
            "update".into(),
            "--target-file".into(),
            config.target_file.to_string_lossy().into_owned(),
            "--object".into(),
            config.object.clone(),
            "--admin-dir".into(),
            config
                .bootstrap_dir
                .join("admin")
                .to_string_lossy()
                .into_owned(),
            flag.into(),
            if flag == "--revoke-credential" {
                credential.id.clone()
            } else {
                credential.tenant.clone()
            },
        ];
        if let Some(key) = &config.shared_key_file {
            args.extend([
                "--shared-key-file".into(),
                key.to_string_lossy().into_owned(),
            ]);
        }
        let command_start = now_ms();
        let command_monotonic = Instant::now();
        let result = Job::start(&config.server, &args, 120000).and_then(|mut j| j.wait());
        let command_end = now_ms();
        let command_elapsed_ms = command_monotonic.elapsed().as_secs_f64() * 1000.;
        let generation = match result {
            Ok(value) => value["generation"]
                .as_u64()
                .ok_or("mutation_generation_missing")?,
            Err(error) => {
                records.push(json!({"index":index,"kind":kind,"jitter_ms":delay,
                    "commit":{"kind":"command_bracket","start_unix_ms":command_start,
                        "end_unix_ms":command_end,"elapsed_ms":command_elapsed_ms},
                    "outcome":"unknown_do_not_retry","error":error,"generation":null}));
                complete = false;
                break;
            }
        };
        if previous_generation.is_some_and(|previous| generation != previous + 1)
            || generation != old_generation + 1
        {
            complete = false;
        }
        previous_generation = Some(generation);
        let behavior_start = Instant::now();
        let mut behavior = None;
        let mut attempts = 0u64;
        let mut status_counts = BTreeMap::<u16, u64>::new();
        let mut error_count = 0u64;
        while behavior_start.elapsed() < Duration::from_millis(config.observation_deadline_ms) {
            attempts += 1;
            let affected = request(&client, &url, "authorization", &bearer);
            let unaffected = request(
                &client,
                &url,
                "authorization",
                &format!("Bearer {load_token}"),
            );
            let stale = request(&client, &direct_url, "x-vlite-principal", &old_envelope);
            for reply in [&affected, &unaffected, &stale] {
                match reply {
                    Ok(reply) => *status_counts.entry(reply.status).or_default() += 1,
                    Err(_) => error_count += 1,
                }
            }
            let affected_ok = affected.as_ref().is_ok_and(|r| {
                r.status == desired
                    && match kind {
                        "suspension" => r.body["error"]["code"] == "tenant_suspended",
                        "resumption" => r.body == suspended_expected,
                        _ => true,
                    }
            });
            let unaffected_ok = unaffected
                .as_ref()
                .is_ok_and(|r| r.status == 200 && r.body == expected);
            let stale_ok = stale
                .as_ref()
                .is_ok_and(|r| r.status == 401 && r.body["error"]["code"] == "envelope_stale");
            if affected_ok
                && unaffected_ok
                && stale_ok
                && behavior_start.elapsed() <= Duration::from_millis(config.observation_deadline_ms)
            {
                behavior = Some(
                    json!({"observed_unix_ms":now_ms(),"affected_status":desired,
                    "unaffected_semantic_match":true,"old_generation_envelope":"envelope_stale",
                    "scope":"ingress_behavior_only_not_replica_coverage"}),
                );
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        if behavior.is_none() {
            complete = false;
        }
        // Keep a full declared observation window for every replica, even
        // when the ingress behavior converges earlier.
        thread::sleep(
            Duration::from_millis(config.observation_deadline_ms)
                .saturating_sub(behavior_start.elapsed()),
        );
        records.push(json!({"index":index,"kind":kind,"jitter_ms":delay,"generation":generation,
            "outcome":"acknowledged","commit":{"kind":"command_bracket","start_unix_ms":command_start,
                "end_unix_ms":command_end,"elapsed_ms":command_elapsed_ms},
            "behavior":behavior,"behavior_attempts":attempts,"behavior_statuses":status_counts,
            "behavior_transport_or_body_errors":error_count,
            "observation_deadline_ms":config.observation_deadline_ms}));
        if !complete {
            break;
        }
    }
    Ok(
        json!({"schema":"vlite-meas005/1","qualification":"proposal_evidence_only","stage":"mutations",
        "source":provenance(),"complete":false,"mutation_and_ingress_checks_complete":complete,
        "requested":{"mutations":config.mutations,"seed":config.seed,"poll_interval_ms":config.poll_interval_ms,
            "observation_deadline_ms":config.observation_deadline_ms,"labels":config.requested},
        "limits":{"mutations":100,"mutation_command_timeout_ms":120000,
            "envelope_prepare_timeout_ms":15000,"behavior_deadline_overrun_max_ms":3000,
            "retries":0,"body_bytes":BODY_LIMIT},
        "uncertainty":{"per_replica_coverage":"requires_analyze_with_observer_reports_and_fixed_inventory",
            "timing":"command_bracket_not_exact_CAS","physical_blob_GETs":null,
            "logical_feed_polls":null,"read_cost_model":"one_full_snapshot_logical_GET_per_feed_poll_plus_startup; adapter_retries_and_identity_requests_unknown",
            "mutator_clock_offset_ms":0,"clock_calibration":"external_before_and_after_required"},
        "mutations":records}),
    )
}

fn observe(config: Observe) -> Result<Value> {
    config.server.validate()?;
    bounded("interval_ms", config.interval_ms, 10, 5000)?;
    bounded("duration_secs", config.duration_secs, 1, 3540)?;
    bounded("timeout_ms", config.timeout_ms, 10, 1000)?;
    if let Some(reference) = &config.clock_reference {
        reference.validate(config.duration_secs)?;
    }
    if !matches!(config.role.as_str(), "gateway" | "backend") {
        return Err("observer_role_invalid".into());
    }
    let url = reqwest::Url::parse(&config.ready_url).map_err(|_| "observer_url_invalid")?;
    let loopback = url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if !loopback
        || url.scheme() != "http"
        || url.path() != "/ready"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("observer_requires_loopback_ready".into());
    }
    let platform_replica = std::env::var("CONTAINER_APP_REPLICA_NAME").ok();
    if platform_replica.is_some() && config.local_replica.is_some() {
        return Err("local_identity_forbidden_on_ACA".into());
    }
    let replica = platform_replica
        .clone()
        .or(config.local_replica)
        .ok_or("replica_identity_missing")?;
    if !label(&replica) {
        return Err("replica_identity_invalid".into());
    }
    let app = std::env::var("CONTAINER_APP_NAME").ok();
    let revision = std::env::var("CONTAINER_APP_REVISION").ok();
    if app.as_ref().is_some_and(|x| !label(x)) || revision.as_ref().is_some_and(|x| !label(x)) {
        return Err("platform_identity_invalid".into());
    }
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|_| "observer_random_failed")?;
    let boot = random
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let client = client(config.timeout_ms)?;
    let interrupted = Arc::new(AtomicBool::new(false));
    let signal_failed = Arc::new(AtomicBool::new(false));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map_err(|_| "observer_signal_runtime_failed")?;
    let stop = interrupted.clone();
    let failed = signal_failed.clone();
    runtime.spawn(async move {
        #[cfg(unix)]
        let result = async {
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            tokio::select! {
                result = tokio::signal::ctrl_c() => result,
                _ = terminate.recv() => Ok(()),
            }
        }
        .await;
        #[cfg(windows)]
        let result = async {
            let mut interrupt = tokio::signal::windows::ctrl_break()?;
            tokio::select! {
                result = tokio::signal::ctrl_c() => result,
                _ = interrupt.recv() => Ok(()),
            }
        }
        .await;
        if result.is_err() {
            failed.store(true, Ordering::SeqCst);
        }
        stop.store(true, Ordering::SeqCst);
    });
    let child = Command::new(&config.server.program)
        .args(&config.server.args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "observer_server_spawn_failed")?;
    struct Owned(std::process::Child);
    impl Drop for Owned {
        fn drop(&mut self) {
            if !matches!(self.0.try_wait(), Ok(Some(_))) {
                let _ = self.0.kill();
            }
            let _ = self.0.wait();
        }
    }
    let mut child = Owned(child);
    let start_utc = now_ms();
    let start = Instant::now();
    // Reference polling runs on its own thread so its round trips never
    // stretch the readiness sampling cadence. The guard stops the poller on
    // every exit path, including early error returns below.
    struct StopOnDrop(Arc<AtomicBool>);
    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let reference_stop = StopOnDrop(Arc::new(AtomicBool::new(false)));
    let reference = match &config.clock_reference {
        Some(poll) => {
            let reference_client = support::client(poll.timeout_ms)?;
            let (url, interval) = (poll.url.clone(), Duration::from_millis(poll.interval_ms));
            let (stop, window) = (
                reference_stop.0.clone(),
                Duration::from_secs(config.duration_secs),
            );
            Some(thread::spawn(move || {
                let (mut exchanges, mut attempts, mut failures) = (Vec::new(), 0u64, 0u64);
                while !stop.load(Ordering::SeqCst)
                    && start.elapsed() < window
                    && exchanges.len() < MAX_REFERENCE_EXCHANGES
                {
                    let began = Instant::now();
                    attempts += 1;
                    match poll_reference(&reference_client, &url) {
                        Some([s0, r1, r2, s3]) => exchanges.push(json!({
                            "subject_send_unix_ms":s0,"reference_receive_unix_ms":r1,
                            "reference_reply_unix_ms":r2,"subject_receive_unix_ms":s3})),
                        None => failures += 1,
                    }
                    while !stop.load(Ordering::SeqCst) && began.elapsed() < interval {
                        thread::sleep(
                            interval
                                .saturating_sub(began.elapsed())
                                .min(Duration::from_millis(50)),
                        );
                    }
                }
                (exchanges, attempts, failures)
            }))
        }
        None => None,
    };
    let mut events = Vec::new();
    let mut previous = Value::Null;
    let mut polls = 0u64;
    let mut failures = 0u64;
    let mut max_gap_ms = 0f64;
    let mut last_start = start;
    let mut max_clock_jump_ms = 0f64;
    let mut terminated_early = false;
    let mut last_ready_start = start_utc;
    while start.elapsed() < Duration::from_secs(config.duration_secs) {
        if interrupted.load(Ordering::SeqCst) {
            break;
        }
        if child
            .0
            .try_wait()
            .map_err(|_| "observer_child_wait_failed")?
            .is_some()
        {
            terminated_early = true;
            break;
        }
        let before = now_ms();
        let mono_before = start.elapsed().as_secs_f64() * 1000.;
        let poll_start = Instant::now();
        max_gap_ms = max_gap_ms.max(poll_start.duration_since(last_start).as_secs_f64() * 1000.);
        last_start = poll_start;
        let reply = request(&client, &config.ready_url, "", "");
        let after = now_ms();
        let mono_after = start.elapsed().as_secs_f64() * 1000.;
        max_clock_jump_ms = max_clock_jump_ms
            .max(((before as f64 - start_utc as f64) - mono_before).abs())
            .max(((after as f64 - start_utc as f64) - mono_after).abs());
        polls += 1;
        let state = match reply {
            Ok(reply)
                if reply.status == 200
                    && reply.body["generation"].as_u64().is_some()
                    && (config.role == "gateway"
                        || reply.body["minimum_generation"].as_u64().is_some()) =>
            {
                json!({"status":200,"generation":reply.body["generation"],
                    "minimum_generation":reply.body.get("minimum_generation").unwrap_or(&Value::Null)})
            }
            Ok(reply) => {
                failures += 1;
                json!({"status":reply.status,"generation":null,"minimum_generation":null})
            }
            Err(_) => {
                failures += 1;
                json!({"status":null,"generation":null,"minimum_generation":null})
            }
        };
        if state != previous {
            if events.len() == 2048 {
                return Err("observer_event_limit".into());
            }
            events.push(json!({"start_unix_ms":before,"end_unix_ms":after,
                "install_lower_unix_ms":last_ready_start,
                "start_monotonic_ms":mono_before,"end_monotonic_ms":mono_after,"state":state}));
            previous = state;
        }
        if previous["status"] == 200 {
            last_ready_start = before;
        }
        thread::sleep(
            Duration::from_millis(config.interval_ms).saturating_sub(poll_start.elapsed()),
        );
    }
    let end_utc = now_ms();
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.;
    reference_stop.0.store(true, Ordering::SeqCst);
    let reference = match (reference, &config.clock_reference) {
        (Some(handle), Some(poll)) => {
            let (exchanges, attempts, failures) =
                handle.join().map_err(|_| "clock_reference_poller_failed")?;
            json!({"qualification":REFERENCE_QUALIFICATION,
                "interval_ms":poll.interval_ms,"timeout_ms":poll.timeout_ms,
                "attempts":attempts,"failures":failures,"limit":MAX_REFERENCE_EXCHANGES,
                "exchanges":exchanges})
        }
        _ => Value::Null,
    };
    drop(child);
    drop(runtime);
    if signal_failed.load(Ordering::SeqCst) {
        return Err("observer_signal_failed".into());
    }
    let mut report = json!({"schema":"vlite-meas005-observer/1","qualification":"proposal_evidence_only",
        "source":provenance(),"replica":replica,"role":config.role,"app":app,"revision":revision,
        "identity_source":if platform_replica.is_some() {"ACA_environment"} else {"local_explicit"},
        "boot_id":boot,"owns_serving_child":true,"terminated_early":terminated_early,
        "interrupted":interrupted.load(Ordering::SeqCst),
        "start_unix_ms":start_utc,"end_unix_ms":end_utc,"elapsed_ms":elapsed_ms,
        "polls":polls,"readiness_failures":failures,"max_sample_start_gap_ms":max_gap_ms,
        "max_clock_jump_ms":max_clock_jump_ms,"events":events,
        "requested":{"interval_ms":config.interval_ms,"duration_secs":config.duration_secs,"timeout_ms":config.timeout_ms},
        "limits":{"events":2048,"duration_secs":3540,"body_bytes":BODY_LIMIT},
        "uncertainty":{"clock_offset":"external_calibration_required","feed_poll_count":null,
            "readiness_polls_are_not_Blob_GETs":true,"shutdown":"measurement_child_killed_and_reaped_at_end"}});
    // Emitted only when configured so the existing gateway entrypoint shape
    // (which does not yet forward clock_reference) is unchanged.
    if !reference.is_null() {
        report["clock_reference"] = reference;
    }
    Ok(report)
}

fn analyze(config: Analyze) -> Result<Value> {
    // No accepted evidence source supplies a run-wide continuity envelope, so
    // production analysis rejects every cross-clock correction.
    analyze_with(config, None)
}
/// `envelope` is an independent run-wide continuity bound applied to every
/// calibrated clock. It is not configurable: only tests inject one, to
/// exercise the correction that a future measured envelope would enable.
fn analyze_with(config: Analyze, envelope: Option<f64>) -> Result<Value> {
    if config.replicas.is_empty() || config.replicas.len() > 64 {
        return Err("replica_count_invalid".into());
    }
    let evidence: Value =
        serde_json::from_slice(&bytes(&config.mutation_evidence_file, 4_194_304)?)
            .map_err(|_| "mutation_evidence_invalid")?;
    if evidence["schema"] != "vlite-meas005/1"
        || evidence["stage"] != "mutations"
        || evidence["qualification"] != "proposal_evidence_only"
    {
        return Err("mutation_evidence_invalid".into());
    }
    let mutations = evidence["mutations"]
        .as_array()
        .ok_or("mutation_evidence_invalid")?;
    if mutations.is_empty() || mutations.len() > 100 {
        return Err("mutation_count_invalid".into());
    }
    let mut identities = BTreeSet::new();
    let mut reports = Vec::new();
    let mut complete = evidence["mutation_and_ingress_checks_complete"] == true
        && config.fixed_inventory_confirmed == Some(true)
        && evidence["source"].is_object()
        && evidence["requested"]["mutations"].as_u64() == Some(mutations.len() as u64);
    let policy = config.calibration.as_ref();
    let mutator_clock = mutator_clock(policy).and_then(|clock| match clock {
        Some(mut clock) => {
            clock.envelope = envelope;
            clock.continuity().map(|()| Some(clock))
        }
        None => Ok(None),
    });
    let mutator_calibration = match &mutator_clock {
        Ok(Some(clock)) => {
            json!({"status":"accepted","reference":"common","bracket":clock.json()})
        }
        Ok(None) => json!({"status":"accepted","reference":"mutator","bracket":null}),
        Err(reason) => json!({"status":"rejected","reasons":[reason]}),
    };
    for replica in &config.replicas {
        if !label(&replica.replica)
            || !identities.insert(replica.replica.clone())
            || !matches!(replica.role.as_str(), "gateway" | "backend")
        {
            return Err("replica_inventory_invalid".into());
        }
        let report: Value = serde_json::from_slice(&bytes(&replica.report_file, 4_194_304)?)
            .map_err(|_| "observer_report_invalid")?;
        if report["schema"] != "vlite-meas005-observer/1"
            || report["replica"] != replica.replica
            || report["role"] != replica.role
            || report["qualification"] != "proposal_evidence_only"
            || report["owns_serving_child"] != true
            || !report["boot_id"].as_str().is_some_and(label)
            || report["source"] != evidence["source"]
        {
            return Err("observer_report_identity_or_source_mismatch".into());
        }
        let events = report["events"]
            .as_array()
            .ok_or("observer_events_invalid")?;
        if events.len() > 2048 {
            return Err("observer_events_invalid".into());
        }
        let mut reasons: Vec<Rejection> = Vec::new();
        let jump = report["max_clock_jump_ms"]
            .as_f64()
            .filter(|x| (0.0..=MUTATOR_DRIFT_ALLOWANCE_MS).contains(x));
        if jump.is_none() {
            reasons.push("observer_clock_jump_exceeds_allowance");
        }
        let pair = match (&mutator_clock, policy, replica.calibration.as_ref()) {
            (Err(reason), _, _) => Err(*reason),
            (Ok(_), Some(policy), Some(observer)) => bracket(observer, policy),
            _ => Err("calibration_missing"),
        }
        .map(|mut observer| {
            observer.envelope = envelope;
            if let Err(reason) = observer.continuity() {
                reasons.push(reason);
            }
            Pair {
                observer,
                mutator: mutator_clock.as_ref().ok().and_then(Option::as_ref),
            }
        })
        .map_err(|reason| reasons.push(reason))
        .ok();
        let slo_margin = policy.map_or(0., |p| p.slo_margin_ms);
        let mut max_applied_error: Option<f64> = None;
        if report["terminated_early"] != false || report["interrupted"] != false {
            complete = false;
        }
        let mut samples = Vec::new();
        let mut lower_values = Vec::new();
        let mut upper_values = Vec::new();
        let mut missing = 0u64;
        for mutation in mutations {
            let Some(generation) = mutation["generation"].as_u64() else {
                missing += 1;
                complete = false;
                samples.push(json!({"generation":null,"generation_and_floor_observed":false,
                    "timing_complete":false,"latency_ms":null,"censoring":"commit_outcome_unknown"}));
                continue;
            };
            let observed = events.iter().find(|event| {
                event["state"]["status"] == 200
                    && event["state"]["generation"] == generation
                    && (replica.role == "gateway"
                        || event["state"]["minimum_generation"]
                            .as_u64()
                            .is_some_and(|floor| floor >= generation))
            });
            let commit = &mutation["commit"];
            let mut interval = Value::Null;
            let mut covered = false;
            if let Some(event) = observed
                && let Some(pair) = pair.as_ref().filter(|_| reasons.is_empty())
                && let Some(jump) = jump
            {
                // Observer reading -> mutator clock: (corrected, applied error, reference time).
                let correct = |value: Option<f64>, code: &'static str| {
                    let value = value.ok_or(code)?;
                    let (offset, error, reference) = pair.observer_at(value);
                    Ok::<_, &'static str>((
                        value - offset,
                        error + jump + MUTATOR_DRIFT_ALLOWANCE_MS,
                        reference,
                    ))
                };
                let c0 = commit["start_unix_ms"]
                    .as_f64()
                    .ok_or("commit_bracket_invalid")?;
                let c1 = commit["end_unix_ms"]
                    .as_f64()
                    .ok_or("commit_bracket_invalid")?;
                let (install, install_error, _) = correct(
                    event["install_lower_unix_ms"].as_f64(),
                    "observation_bracket_invalid",
                )?;
                let (end, end_error, end_reference) =
                    correct(event["end_unix_ms"].as_f64(), "observation_bracket_invalid")?;
                let o0 = install - install_error;
                let o1 = end + end_error;
                let deadline = mutation["observation_deadline_ms"]
                    .as_f64()
                    .ok_or("observation_deadline_invalid")?;
                let (start, start_error, _) = correct(
                    report["start_unix_ms"].as_f64(),
                    "observer_lifetime_invalid",
                )?;
                let lifetime_start = start + start_error;
                let (stop, stop_error, _) =
                    correct(report["end_unix_ms"].as_f64(), "observer_lifetime_invalid")?;
                let lifetime_end = stop - stop_error;
                let (c0_reference, c0_error) = pair.mutator_reference(c0);
                let sample_error = install_error.max(end_error);
                max_applied_error =
                    Some(max_applied_error.map_or(sample_error, |error| error.max(sample_error)));
                let mut sample_ok = true;
                if !pair.brackets(c0_reference - c0_error, end_reference + end_error) {
                    reasons.push("calibration_does_not_bracket_run");
                    sample_ok = false;
                }
                if sample_error > slo_margin {
                    reasons.push("calibration_uncertainty_exceeds_slo_margin");
                    sample_ok = false;
                }
                let command_elapsed = commit["elapsed_ms"]
                    .as_f64()
                    .ok_or("commit_bracket_invalid")?;
                let clock_consistent = c1 >= c0
                    && o1 >= o0
                    && commit["kind"] == "command_bracket"
                    && ((c1 - c0) - command_elapsed).abs() <= 10.;
                covered = lifetime_start <= c0
                    && lifetime_end >= o1
                    && lifetime_end >= c1 + deadline
                    && o1 <= c1 + deadline
                    && o1 >= c0
                    && clock_consistent
                    && sample_ok;
                if covered {
                    let lower = (o0 - c1).max(0.);
                    let upper = (o1 - c0).max(0.);
                    lower_values.push(lower);
                    upper_values.push(upper);
                    interval = json!({"lower":lower,"upper":upper,"commit_kind":"command_bracket"});
                }
            }
            if !covered {
                missing += 1;
                complete = false;
            }
            samples.push(json!({"generation":generation,"generation_and_floor_observed":observed.is_some(),
                "timing_complete":covered,"latency_ms":interval,
                "censoring":if covered {"interval"} else if !reasons.is_empty() {"clock_unknown"} else if observed.is_some() {"deadline_or_lifetime_incomplete"} else {"not_observed"}}));
        }
        reasons.sort_unstable();
        reasons.dedup();
        let calibrated = reasons.is_empty();
        if !calibrated {
            complete = false;
        }
        let clock_calibration = json!({"status":if calibrated {"accepted"} else {"rejected"},
            "reasons":reasons,"observer":pair.as_ref().map(|p| p.observer.json()),
            "max_applied_error_ms":max_applied_error});
        reports.push(json!({"replica":replica.replica,"role":replica.role,"boot_id":report["boot_id"],
            "clock_calibrated":calibrated,"clock_calibration":clock_calibration,
            "samples":samples,"incomplete_or_censored":missing,
            "successful_lower_ms":summary(&lower_values),"successful_upper_ms":summary(&upper_values),
            "readiness_polls":report["polls"],"readiness_failures":report["readiness_failures"],
            "max_sample_start_gap_ms":report["max_sample_start_gap_ms"],
            "clock_reference_samples":reference_diagnostics(&report["clock_reference"]),
            "logical_feed_polls":null,"physical_blob_GETs":null}));
    }
    if !config.replicas.iter().any(|r| r.role == "gateway")
        || !config.replicas.iter().any(|r| r.role == "backend")
    {
        complete = false;
    }
    Ok(
        json!({"schema":"vlite-meas005/1","qualification":"proposal_evidence_only","stage":"analysis",
        "source":evidence["source"],"complete":complete,"requested":evidence["requested"],
        "limits":{"replicas":64,"mutations":100,"events_per_replica":2048},
        "fixed_inventory_confirmed":config.fixed_inventory_confirmed,
        "mutator_clock_calibration":mutator_calibration,
        "uncertainty":{"timing":"interval_censored_command_bracket","physical_blob_GETs":null,
            "logical_feed_polls":null,"clock_calibration":"four_timestamp_min_rtt_exchanges_bracketing_run",
            "drift_model":"endpoint_offsets_do_not_bound_interior_excursion",
            "continuity_envelope":"required_none_available_rejects_clock_unknown",
            "calibration_qualification":"advisory_unqualified_until_live_run",
            "mutator_clock_drift_allowance_ms":MUTATOR_DRIFT_ALLOWANCE_MS,
            "install_time":"between_last_old_ready_sample_start_and_first_new_ready_sample_end",
            "coverage":"only_explicit_inventory_and_owned_process_observers",
            "qualification":"no_S_ctl_or_deployment_approval_inferred"},
        "mutations":evidence["mutations"],"replicas":reports}),
    )
}
fn main() {
    let result = (|| match read::<Config>(&config_arg()?)? {
        Config::Run(config) => run(config),
        Config::Observe(config) => observe(config),
        Config::Analyze(config) => analyze(config),
        Config::ClockReference(config) => {
            config.validate()?;
            let listener = std::net::TcpListener::bind(&config.bind)
                .map_err(|_| "clock_reference_bind_failed")?;
            let started = now_ms();
            let served = serve_reference(
                listener,
                Instant::now() + Duration::from_secs(config.duration_secs),
            )?;
            Ok(
                json!({"schema":REFERENCE_SCHEMA,"qualification":"proposal_evidence_only",
                "source":provenance(),"start_unix_ms":started,"end_unix_ms":now_ms(),
                "replies":served,"duration_secs":config.duration_secs}),
            )
        }
        Config::ClockSample(_) => {
            let before = now_ms();
            let mono = Instant::now();
            Ok(
                json!({"schema":"vlite-meas005-clock/1","qualification":"proposal_evidence_only",
                "start_unix_ms":before,"end_unix_ms":now_ms(),
                "elapsed_ms":mono.elapsed().as_secs_f64()*1000.,"source":provenance()}),
            )
        }
    })();
    match result {
        Ok(report) => {
            let incomplete = report["mutation_and_ingress_checks_complete"] == false
                || report["stage"] == "analysis" && report["complete"] != true
                || report["terminated_early"] == true
                || report["interrupted"] == true;
            println!("{report}");
            if incomplete {
                eprintln!("{}", json!({"error":{"code":"measurement_incomplete"}}));
                std::process::exit(2);
            }
        }
        Err(error) => finish(Err(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn jitter_is_seeded_and_bounded_including_zero() {
        let mut a = 0;
        let mut b = 0;
        for _ in 0..100 {
            let x = jitter(&mut a, 1000);
            assert_eq!(x, jitter(&mut b, 1000));
            assert!(x <= 1000);
        }
    }
    #[test]
    fn config_refuses_unknown_fields_and_identity_guesses() {
        assert!(
            serde_json::from_value::<Config>(json!({"mode":"run","auth_bypass":true})).is_err()
        );
        assert!(!label("replica/secret"));
        assert!(
            serde_json::from_value::<Config>(json!({"mode":"clock_sample","unexpected":true}))
                .is_err()
        );
    }

    fn analysis_fixture(root: &std::path::Path, floor: u64, calibrated: bool) -> Analyze {
        let source = json!({"revision":"test","dirty_paths":[]});
        let mutations = (0..3)
            .map(|i| {
                json!({"generation":i+2,
            "commit":{"kind":"command_bracket","start_unix_ms":1000+i*200,
                "end_unix_ms":1010+i*200,"elapsed_ms":10.},
            "observation_deadline_ms":1000,"behavior":{"unaffected_semantic_match":true},
            "outcome":"acknowledged"})
            })
            .collect::<Vec<_>>();
        let evidence = json!({"schema":"vlite-meas005/1","stage":"mutations","qualification":"proposal_evidence_only",
            "source":source,"requested":{"mutations":3},"mutation_and_ingress_checks_complete":true,
            "mutations":mutations});
        let file = root.join("mutations.json");
        std::fs::write(&file, evidence.to_string()).unwrap();
        let replicas = ["gateway","backend"].into_iter().map(|role| {
            let events = (0..3).map(|i| json!({"start_unix_ms":1100+i*200,"end_unix_ms":1110+i*200,
                "install_lower_unix_ms":1080+i*200,"state":{"status":200,"generation":i+2,
                    "minimum_generation":if floor == 0 {0} else {i+2}}})).collect::<Vec<_>>();
            let report = json!({"schema":"vlite-meas005-observer/1","qualification":"proposal_evidence_only",
                "source":source,"replica":role,"role":role,"boot_id":format!("boot-{role}"),
                "owns_serving_child":true,"terminated_early":false,"interrupted":false,
                "start_unix_ms":500,"end_unix_ms":3000,"max_clock_jump_ms":0.,
                "polls":50,"readiness_failures":0,"max_sample_start_gap_ms":50,"events":events});
            let report_file = root.join(format!("{role}.json"));
            std::fs::write(&report_file,report.to_string()).unwrap();
            Replica { replica:role.into(), role:role.into(), report_file,
                calibration:calibrated.then(|| exact_bracket(0., 100_000.)) }
        }).collect();
        Analyze {
            mutation_evidence_file: file,
            replicas,
            fixed_inventory_confirmed: Some(true),
            calibration: Some(policy(Reference::Mutator, 1)),
        }
    }

    fn policy(reference: Reference, min_samples: usize) -> CalibrationPolicy {
        CalibrationPolicy {
            reference,
            mutator: None,
            min_samples,
            max_rtt_ms: 1000.,
            slo_margin_ms: 60000.,
            resolution_ms: 0.,
        }
    }
    fn instant(at: f64, offset: f64) -> Exchange {
        Exchange {
            t0_ms: at,
            t1_ms: at + offset,
            t2_ms: at + offset,
            t3_ms: at,
        }
    }
    fn exact_bracket(pre: f64, post: f64) -> Bracket {
        Bracket {
            pre: vec![instant(pre, 0.)],
            post: vec![instant(post, 0.)],
        }
    }
    /// Subject clock = reference + 500 ms + 50 ppm; delays are asymmetric.
    fn truth(reference_ms: f64) -> f64 {
        500. + 50e-6 * reference_ms
    }
    fn exchange(at: f64, forward: f64, processing: f64, back: f64) -> Exchange {
        let receive = at + forward;
        let reply = receive + processing;
        Exchange {
            t0_ms: at,
            t1_ms: receive + truth(receive),
            t2_ms: reply + truth(reply),
            t3_ms: reply + back,
        }
    }
    fn synthetic(start: f64) -> Vec<Exchange> {
        (0..8)
            .map(|i| {
                let i = i as f64;
                exchange(start + i * 100., 1. + i, 3., 9. + 2. * i)
            })
            .collect()
    }

    /// Analysis with a hypothetical verified run-wide continuity envelope. No
    /// production path supplies one; this exercises the correction logic only.
    fn verified(config: Analyze, envelope: f64) -> Value {
        analyze_with(config, Some(envelope)).unwrap()
    }

    #[test]
    fn calibration_recovers_known_offset_within_supplied_envelope() {
        let policy = policy(Reference::Mutator, 4);
        let pre = synthetic(10_000.);
        let mut clock = bracket(
            &Bracket {
                pre: pre.clone(),
                post: synthetic(1_010_000.),
            },
            &policy,
        )
        .unwrap();
        // The first exchange has the smallest net round trip (1 + 9 ms).
        assert!((clock.pre.rtt - 10.).abs() < 1e-3);
        assert_eq!(clock.pre.samples, 8);
        for estimate in [clock.pre, clock.post] {
            let truth = truth(estimate.at);
            assert!((estimate.offset - truth).abs() <= estimate.error + 1e-9);
            // Asymmetric delay biases the midpoint, but never past the bound.
            assert!((estimate.offset - truth).abs() > 1.);
        }
        // Endpoints alone never bound the interior.
        assert_eq!(clock.envelope, None);
        assert_eq!(
            clock.continuity().err(),
            Some("calibration_continuity_unbounded")
        );
        assert!(clock.at(510_000.).1.is_infinite());
        // The true offset moves 50 ms over the run; a smaller envelope contradicts
        // the endpoints, an adequate one bounds every interior reading.
        clock.envelope = Some(1.);
        assert_eq!(
            clock.continuity().err(),
            Some("calibration_endpoints_exceed_continuity_envelope")
        );
        clock.envelope = Some(51.);
        assert_eq!(clock.continuity(), Ok(()));
        for reference in [10_000., 300_000., 510_000., 900_000., 1_010_500.] {
            let (offset, error) = clock.at(reference);
            assert!((offset - truth(reference)).abs() <= error + 1e-9);
            let subject = reference + truth(reference);
            let (offset, error) = clock.subject(subject);
            assert!((offset - truth(reference)).abs() <= error + 1e-9);
            assert!((subject - offset - reference).abs() <= error + 1e-9);
        }
        let mut worse = pre;
        worse.rotate_left(3);
        assert!((estimate(&worse, &policy).unwrap().rtt - 10.).abs() < 1e-3);
    }

    #[test]
    fn calibration_rejections_use_static_reason_codes() {
        let base = policy(Reference::Mutator, 2);
        let one = [instant(0., 0.)];
        assert_eq!(
            estimate(&one, &base).err(),
            Some("calibration_too_few_samples")
        );
        let slow = [exchange(0., 600., 1., 600.), exchange(10., 700., 1., 700.)];
        assert_eq!(
            estimate(&slow, &base).err(),
            Some("calibration_rtt_exceeds_bound")
        );
        let backwards = [
            instant(0., 0.),
            Exchange {
                t3_ms: -1.,
                ..instant(0., 0.)
            },
        ];
        assert_eq!(
            estimate(&backwards, &base).err(),
            Some("calibration_exchange_invalid")
        );
        let nan = [instant(0., 0.), instant(f64::NAN, 0.)];
        assert_eq!(
            estimate(&nan, &base).err(),
            Some("calibration_exchange_invalid")
        );
        let disjoint = [instant(0., 0.), instant(10., 50.)];
        assert_eq!(
            estimate(&disjoint, &base).err(),
            Some("calibration_exchange_inconsistent")
        );
        let overlapping = Bracket {
            pre: vec![instant(0., 0.), instant(100., 0.)],
            post: vec![instant(50., 0.), instant(200., 0.)],
        };
        assert_eq!(
            bracket(&overlapping, &base).err(),
            Some("calibration_does_not_bracket_run")
        );
        assert_eq!(mutator_clock(None).err(), Some("calibration_missing"));
        let mut invalid = policy(Reference::Mutator, 1);
        invalid.mutator = Some(exact_bracket(0., 1.));
        assert_eq!(
            mutator_clock(Some(&invalid)).err(),
            Some("calibration_policy_invalid")
        );
        assert_eq!(
            mutator_clock(Some(&policy(Reference::Mutator, 0))).err(),
            Some("calibration_policy_invalid")
        );
        assert_eq!(
            mutator_clock(Some(&policy(Reference::Common, 1))).err(),
            Some("calibration_missing")
        );
    }

    #[test]
    fn common_reference_combines_observer_and_mutator_errors() {
        let mut common = policy(Reference::Common, 1);
        let wide = |offset: f64| Bracket {
            pre: vec![Exchange {
                t0_ms: 0.,
                t1_ms: 2. + offset,
                t2_ms: 2. + offset,
                t3_ms: 4.,
            }],
            post: vec![Exchange {
                t0_ms: 100_000.,
                t1_ms: 100_002. + offset,
                t2_ms: 100_002. + offset,
                t3_ms: 100_004.,
            }],
        };
        common.mutator = Some(wide(-300.));
        let mut mutator = mutator_clock(Some(&common)).unwrap().unwrap();
        mutator.envelope = Some(0.);
        let mut observer = bracket(&wide(200.), &common).unwrap();
        observer.envelope = Some(0.);
        let pair = Pair {
            observer,
            mutator: Some(&mutator),
        };
        let (offset, error, reference) = pair.observer_at(202.);
        assert!((offset - 500.).abs() < 1e-9);
        assert!(error >= 4. && error < 4.1);
        assert!((reference - 2.).abs() < 1e-9);
        let (reference, error) = pair.mutator_reference(-298.);
        assert!((reference - 2.).abs() < 1e-9 && (2.0..2.1).contains(&error));
        assert!(pair.brackets(4., 100_000.) && !pair.brackets(3., 100_000.));
    }

    fn reasons(report: &Value, replica: usize) -> Vec<String> {
        serde_json::from_value(report["replicas"][replica]["clock_calibration"]["reasons"].clone())
            .unwrap()
    }

    #[test]
    fn analysis_fails_closed_on_missing_unbracketed_or_drifting_calibration() {
        let root = tempfile::tempdir().unwrap();
        let mut config = analysis_fixture(root.path(), 1, true);
        config.calibration = None;
        let report = verified(config, 0.);
        assert_eq!(report["complete"], false);
        assert_eq!(report["mutator_clock_calibration"]["status"], "rejected");
        assert_eq!(reasons(&report, 0), ["calibration_missing"]);
        assert_eq!(
            report["replicas"][0]["samples"][0]["censoring"],
            "clock_unknown"
        );

        let mut config = analysis_fixture(root.path(), 1, true);
        config.replicas[0].calibration = Some(exact_bracket(0., 1200.));
        let report = verified(config, 0.);
        assert_eq!(report["complete"], false);
        assert_eq!(reasons(&report, 0), ["calibration_does_not_bracket_run"]);
        assert!(report["replicas"][0]["samples"][2]["latency_ms"].is_null());
        assert_eq!(reasons(&report, 1), Vec::<String>::new());

        let mut config = analysis_fixture(root.path(), 1, true);
        config.replicas[0].calibration = Some(Bracket {
            pre: vec![instant(0., 0.)],
            post: vec![instant(100_000., 2000.)],
        });
        let report = verified(config, 0.);
        assert_eq!(report["complete"], false);
        assert_eq!(
            reasons(&report, 0),
            ["calibration_endpoints_exceed_continuity_envelope"]
        );
        assert_eq!(report["replicas"][1]["clock_calibrated"], true);

        let mut config = analysis_fixture(root.path(), 1, true);
        config.calibration.as_mut().unwrap().slo_margin_ms = 50.;
        let wide = |at: f64| Exchange {
            t0_ms: at,
            t1_ms: at + 50.,
            t2_ms: at + 50.,
            t3_ms: at + 100.,
        };
        config.replicas[0].calibration = Some(Bracket {
            pre: vec![wide(0.)],
            post: vec![wide(100_000.)],
        });
        let report = verified(config, 0.);
        assert_eq!(report["complete"], false);
        assert_eq!(
            reasons(&report, 0),
            ["calibration_uncertainty_exceeds_slo_margin"]
        );
        assert!(report["replicas"][0]["successful_upper_ms"]["p99"].is_null());
        assert_eq!(report["replicas"][1]["clock_calibrated"], true);

        let config = analysis_fixture(root.path(), 1, true);
        let mut observer: Value = read(&config.replicas[0].report_file).unwrap();
        observer["max_clock_jump_ms"] = json!(11.);
        std::fs::write(&config.replicas[0].report_file, observer.to_string()).unwrap();
        let report = verified(config, 0.);
        assert_eq!(
            reasons(&report, 0),
            ["observer_clock_jump_exceeds_allowance"]
        );
    }

    #[test]
    fn identical_endpoints_cannot_bound_an_interior_excursion() {
        // Both brackets read exactly zero offset, so the endpoint mean rate is
        // zero. The observer clock may still have stepped (e.g. +2 s) between
        // them and returned to baseline: nothing in the endpoints excludes it.
        let root = tempfile::tempdir().unwrap();
        let config = analysis_fixture(root.path(), 1, true);
        assert!(
            config
                .replicas
                .iter()
                .all(|r| r.calibration.as_ref().is_some_and(|b| {
                    b.pre[0].t1_ms == b.pre[0].t0_ms && b.post[0].t1_ms == b.post[0].t0_ms
                }))
        );
        let report = analyze(config).unwrap();
        assert_eq!(report["complete"], false);
        assert_eq!(
            report["uncertainty"]["calibration_qualification"],
            "advisory_unqualified_until_live_run"
        );
        assert_eq!(report["mutator_clock_calibration"]["reference"], "mutator");
        for replica in 0..2 {
            let calibration = &report["replicas"][replica]["clock_calibration"];
            assert_eq!(calibration["status"], "rejected");
            assert_eq!(
                reasons(&report, replica),
                ["calibration_continuity_unbounded"]
            );
            assert_eq!(calibration["observer"]["post"]["error_ms"], 0.);
            assert_eq!(calibration["observer"]["endpoint_mean_rate_ms_per_s"], 0.);
            assert_eq!(
                calibration["observer"]["endpoint_rate_bounds_interior"],
                false
            );
            assert!(calibration["observer"]["continuity_envelope_ms"].is_null());
            assert!(calibration["max_applied_error_ms"].is_null());
            assert_eq!(report["replicas"][replica]["clock_calibrated"], false);
            assert!(report["replicas"][replica]["successful_upper_ms"]["p99"].is_null());
            for sample in report["replicas"][replica]["samples"].as_array().unwrap() {
                assert_eq!(sample["censoring"], "clock_unknown");
                assert!(sample["latency_ms"].is_null());
            }
        }
    }

    #[test]
    fn analysis_reports_advisory_calibration_evidence_with_verified_envelope() {
        let root = tempfile::tempdir().unwrap();
        let report = verified(analysis_fixture(root.path(), 1, true), 0.);
        assert_eq!(
            report["uncertainty"]["calibration_qualification"],
            "advisory_unqualified_until_live_run"
        );
        let calibration = &report["replicas"][0]["clock_calibration"];
        assert_eq!(calibration["status"], "accepted");
        assert_eq!(calibration["max_applied_error_ms"], 10.);
        assert_eq!(calibration["observer"]["post"]["samples"], 1);
        assert_eq!(calibration["observer"]["continuity_envelope_ms"], 0.);
        // The envelope widens every corrected bound.
        let widened = verified(analysis_fixture(root.path(), 1, true), 5.);
        assert_eq!(
            widened["replicas"][0]["clock_calibration"]["max_applied_error_ms"],
            15.
        );
    }

    #[test]
    fn analysis_requires_actual_floor_and_clocks_and_keeps_unknowns_null() {
        let root = tempfile::tempdir().unwrap();
        let measured = verified(analysis_fixture(root.path(), 1, true), 0.);
        assert_eq!(measured["complete"], true);
        assert_eq!(measured["replicas"][0]["successful_lower_ms"]["p99"], 60.);
        assert_eq!(measured["replicas"][0]["successful_upper_ms"]["p99"], 120.);
        let missing_floor = verified(analysis_fixture(root.path(), 0, true), 0.);
        assert_eq!(missing_floor["complete"], false);
        assert_eq!(
            missing_floor["replicas"][1]["successful_upper_ms"]["count"],
            0
        );
        assert!(missing_floor["replicas"][1]["successful_upper_ms"]["p99"].is_null());
        let no_clocks = verified(analysis_fixture(root.path(), 1, false), 0.);
        assert_eq!(no_clocks["complete"], false);
        assert_eq!(
            no_clocks["replicas"][0]["samples"][0]["generation_and_floor_observed"],
            true
        );
        assert!(no_clocks["replicas"][0]["samples"][0]["latency_ms"].is_null());
    }

    #[test]
    fn analysis_requires_lifetime_through_entire_observation_deadline() {
        let root = tempfile::tempdir().unwrap();
        for (end, complete, count) in [(2420, true, 3), (2419, false, 2), (2000, false, 0)] {
            let config = analysis_fixture(root.path(), 1, true);
            let mut observer: Value = read(&config.replicas[0].report_file).unwrap();
            observer["end_unix_ms"] = json!(end);
            std::fs::write(&config.replicas[0].report_file, observer.to_string()).unwrap();
            let report = verified(config, 0.);
            assert_eq!(report["complete"], complete);
            assert_eq!(report["replicas"][0]["successful_upper_ms"]["count"], count);
            if !complete {
                assert!(report["replicas"][0]["samples"][2]["latency_ms"].is_null());
            }
        }
    }

    #[test]
    fn analysis_accepts_observation_before_command_ack_but_not_bad_lifetimes() {
        let root = tempfile::tempdir().unwrap();
        let config = analysis_fixture(root.path(), 1, true);
        let mut evidence: Value = read(&config.mutation_evidence_file).unwrap();
        evidence["mutations"][0]["commit"]["end_unix_ms"] = json!(1150);
        evidence["mutations"][0]["commit"]["elapsed_ms"] = json!(150.);
        std::fs::write(&config.mutation_evidence_file, evidence.to_string()).unwrap();
        let report = verified(config, 0.);
        assert_eq!(report["complete"], true);
        assert_eq!(
            report["replicas"][0]["samples"][0]["latency_ms"]["lower"],
            0.
        );
        assert_eq!(
            report["replicas"][0]["samples"][0]["latency_ms"]["upper"],
            120.
        );
        let mut config = analysis_fixture(root.path(), 1, true);
        config.fixed_inventory_confirmed = Some(false);
        assert_eq!(verified(config, 0.)["complete"], false);
        let config = analysis_fixture(root.path(), 1, true);
        let mut observer: Value = read(&config.replicas[0].report_file).unwrap();
        observer["start_unix_ms"] = json!(1200);
        std::fs::write(&config.replicas[0].report_file, observer.to_string()).unwrap();
        assert_eq!(verified(config, 0.)["complete"], false);
    }

    /// Subject (replica) clock = reference + theta; the replica initiates.
    fn subject_exchange(at: f64, theta: f64, forward: f64, processing: f64, back: f64) -> [u64; 4] {
        let receive = at + forward;
        let reply = receive + processing;
        [
            (at + theta).floor() as u64,
            receive.floor() as u64,
            reply.floor() as u64,
            (reply + back + theta).floor() as u64,
        ]
    }

    #[test]
    fn replica_initiated_exchange_bounds_true_offset_with_ms_quantization() {
        for (theta, forward, back) in [(0., 0.4, 0.4), (250.7, 1.2, 9.9), (-75.3, 30., 0.1)] {
            let sample = subject_exchange(1_000_000.5, theta, forward, 2.3, back);
            let (lower, upper) = reference_bounds(&sample).unwrap();
            assert!(
                lower <= theta && theta <= upper,
                "{theta} not in [{lower},{upper}]"
            );
            // The bound width is the net round trip plus two quantization steps.
            assert!(upper - lower <= forward + back + 2. + 2.);
        }
        // Reply before receive, or receive before send: not an exchange.
        assert_eq!(reference_bounds(&[10, 20, 19, 30]), None);
        assert_eq!(reference_bounds(&[30, 20, 20, 29]), None);
        // A reference that spent longer than the round trip is causally impossible.
        assert_eq!(reference_bounds(&[10, 0, 20, 15]), None);
    }

    #[test]
    fn reference_poll_config_is_bounded_and_never_exposes_the_url() {
        let poll = |url: &str, interval_ms: u64| ReferencePoll {
            url: url.into(),
            interval_ms,
            timeout_ms: 1000,
        };
        assert_eq!(
            poll("http://ref.internal:8080/clock", 1000).validate(1200),
            Ok(())
        );
        assert_eq!(
            poll("https://ref.internal/clock", 865).validate(3540),
            Ok(())
        );
        assert_eq!(
            poll("https://ref.internal/clock", 864).validate(3540).err(),
            Some("clock_reference_sample_limit")
        );
        assert_eq!(
            poll("http://ref.internal/clock", 99).validate(10).err(),
            Some("clock_reference_interval_ms_out_of_range")
        );
        for bad in [
            "ftp://ref/clock",
            "http://user:pw@ref/clock",
            "http://ref/clock?sig=1",
            "http://ref/clock#x",
            "http://ref/other",
            "not a url",
        ] {
            assert_eq!(
                poll(bad, 1000).validate(10).err(),
                Some("clock_reference_url_invalid")
            );
        }
        let parsed: Config = serde_json::from_value(json!({"mode":"observe",
            "server":{"program":"/bin/true","args":[]},"ready_url":"http://127.0.0.1:8080/ready",
            "role":"gateway","clock_reference":{"url":"http://ref/clock"}}))
        .unwrap();
        let Config::Observe(observe) = parsed else {
            panic!("observe")
        };
        let reference = observe.clock_reference.unwrap();
        assert_eq!((reference.interval_ms, reference.timeout_ms), (1000, 1000));
        assert!(
            serde_json::from_value::<Config>(json!({"mode":"observe",
            "server":{"program":"/bin/true","args":[]},"ready_url":"http://127.0.0.1:8080/ready",
            "role":"gateway","clock_reference":{"url":"http://ref/clock","envelope_ms":1}}))
            .is_err()
        );
        assert!(
            serde_json::from_value::<Config>(
                json!({"mode":"clock_reference","bind":"0.0.0.0:8080",
                "duration_secs":10,"extra":1})
            )
            .is_err()
        );
        let bind = |bind: &str, duration_secs| ClockReference {
            bind: bind.into(),
            duration_secs,
        };
        assert_eq!(bind("0.0.0.0:8080", 3600).validate(), Ok(()));
        assert_eq!(
            bind("0.0.0.0:8080", 7201).validate().err(),
            Some("duration_secs_out_of_range")
        );
        assert_eq!(
            bind("ref:8080", 10).validate().err(),
            Some("clock_reference_bind_invalid")
        );
    }

    #[test]
    fn reference_endpoint_answers_replica_polls_on_loopback() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            serve_reference(listener, Instant::now() + Duration::from_secs(3))
        });
        let client = client(1000).unwrap();
        let sample = poll_reference(&client, &format!("http://{address}/clock")).unwrap();
        // Same host clock: theta = 0 must lie in the bound.
        let (lower, upper) = reference_bounds(&sample).unwrap();
        assert!(lower <= 0. && 0. <= upper);
        assert_eq!(
            poll_reference(&client, &format!("http://{address}/other")),
            None
        );
        assert_eq!(server.join().unwrap(), Ok(1));
    }

    #[test]
    fn reference_endpoint_cuts_off_a_dripping_client() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            serve_reference(listener, Instant::now() + Duration::from_secs(3))
        });
        // One byte every 200 ms never completes a head; a per-read timeout
        // alone would hold the serial server for the whole 4 s drip.
        let mut drip = std::net::TcpStream::connect(address).unwrap();
        drip.write_all(b"G").unwrap();
        thread::spawn(move || {
            for _ in 0..20 {
                thread::sleep(Duration::from_millis(200));
                if drip.write_all(b"x").is_err() {
                    break;
                }
            }
        });
        thread::sleep(Duration::from_millis(100));
        let began = Instant::now();
        let sample = poll_reference(&client(3000).unwrap(), &format!("http://{address}/clock"));
        assert!(sample.is_some());
        assert!(began.elapsed() < Duration::from_millis(2500));
        assert_eq!(server.join().unwrap(), Ok(1));
    }

    #[test]
    fn reference_diagnostics_separate_steps_empty_runs_and_miscounts() {
        let exchange = |e: [u64; 4]| {
            json!({"subject_send_unix_ms":e[0],"reference_receive_unix_ms":e[1],
                "reference_reply_unix_ms":e[2],"subject_receive_unix_ms":e[3]})
        };
        let diagnose = |exchanges: Vec<Value>, attempts: Value, failures: Value| {
            reference_diagnostics(
                &json!({"attempts":attempts,"failures":failures,"exchanges":exchanges}),
            )
        };
        // The second send precedes the first receive: the subject's clock
        // stepped backward, which is evidence rather than malformed input.
        let stepped = diagnose(
            vec![
                exchange([1000, 1000, 1001, 1010]),
                exchange([1005, 1003, 1004, 1012]),
            ],
            json!(2),
            json!(0),
        );
        assert_eq!(stepped["status"], "subject_clock_stepped_backward");
        assert_eq!(stepped["continuity_envelope_ms"], Value::Null);
        assert_eq!(stepped["qualification"], REFERENCE_QUALIFICATION);
        let empty = diagnose(vec![], json!(3), json!(3));
        assert_eq!(empty["status"], "no_samples");
        assert_eq!(
            (&empty["attempts"], &empty["failures"]),
            (&json!(3), &json!(3))
        );
        assert_eq!(empty["qualification"], REFERENCE_QUALIFICATION);
        let one = || vec![exchange([1000, 1000, 1001, 1010])];
        assert_eq!(diagnose(one(), json!(1), json!(0))["status"], "collected");
        for (attempts, failures) in [
            (json!(2), json!(0)),
            (json!(1), json!(2)),
            (json!(-1), json!(0)),
            (json!(1.5), json!(0)),
            (Value::Null, json!(0)),
            (json!(1), Value::Null),
        ] {
            assert_eq!(diagnose(one(), attempts, failures)["status"], "invalid");
        }
    }

    fn with_reference(config: &Analyze, samples: Value, attempts: u64, failures: u64) {
        for replica in &config.replicas {
            let mut observer: Value = read(&replica.report_file).unwrap();
            observer["clock_reference"] = json!({"qualification":REFERENCE_QUALIFICATION,
                "interval_ms":1000,"timeout_ms":1000,"attempts":attempts,"failures":failures,
                "limit":MAX_REFERENCE_EXCHANGES,"exchanges":samples});
            std::fs::write(&replica.report_file, observer.to_string()).unwrap();
        }
    }

    #[test]
    fn reference_samples_are_diagnostics_and_never_qualify_continuity() {
        let root = tempfile::tempdir().unwrap();
        let config = analysis_fixture(root.path(), 1, true);
        let exchanges = [
            [1000u64, 1000, 1001, 1010],
            [2000, 1995, 1996, 2004],
            [3000, 3002, 3002, 3008],
        ];
        with_reference(
            &config,
            json!(
                exchanges
                    .iter()
                    .map(|e| json!({"subject_send_unix_ms":e[0],
            "reference_receive_unix_ms":e[1],"reference_reply_unix_ms":e[2],
            "subject_receive_unix_ms":e[3]}))
                    .collect::<Vec<_>>()
            ),
            4,
            1,
        );
        let report = analyze(config).unwrap();
        assert_eq!(report["complete"], false);
        for replica in report["replicas"].as_array().unwrap() {
            assert_eq!(replica["clock_calibrated"], false);
            assert!(
                replica["clock_calibration"]["reasons"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("calibration_continuity_unbounded"))
            );
            let diagnostics = &replica["clock_reference_samples"];
            assert_eq!(diagnostics["status"], "collected");
            assert_eq!(diagnostics["samples"], 3);
            assert_eq!(diagnostics["attempts"], 4);
            assert_eq!(diagnostics["failures"], 1);
            assert_eq!(diagnostics["max_subject_gap_ms"], 1000);
            assert_eq!(diagnostics["min_net_rtt_ms"], 3.);
            // Midpoints: 4.5, 6.5, 2 -> sampled spread 4.5 ms.
            assert_eq!(diagnostics["sampled_midpoint_spread_ms"], 4.5);
            // Bounds [-1,10], [4,9], [-3,7] intersect, so a constant offset is
            // not contradicted; that is still not a continuity bound.
            assert_eq!(diagnostics["constant_offset_contradicted"], false);
            assert_eq!(diagnostics["continuity_envelope_ms"], Value::Null);
            assert_eq!(diagnostics["qualification"], REFERENCE_QUALIFICATION);
        }
        // Malformed sample evidence is reported invalid rather than repaired.
        let config = analysis_fixture(root.path(), 1, true);
        with_reference(
            &config,
            json!([{"subject_send_unix_ms":10,"reference_receive_unix_ms":20,
            "reference_reply_unix_ms":19,"subject_receive_unix_ms":30}]),
            1,
            0,
        );
        let report = analyze(config).unwrap();
        assert_eq!(
            report["replicas"][0]["clock_reference_samples"]["status"],
            "invalid"
        );
        // Without collection the field is explicitly null.
        let report = analyze(analysis_fixture(root.path(), 1, true)).unwrap();
        assert!(report["replicas"][0]["clock_reference_samples"].is_null());
    }
}
