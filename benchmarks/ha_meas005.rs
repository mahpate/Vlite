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
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClockSample {}
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
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Analyze {
    mutation_evidence_file: PathBuf,
    replicas: Vec<Replica>,
    /// The harness must inventory the same app/revision/replica set before
    /// and after. False/null never establishes fixed-set coverage.
    fixed_inventory_confirmed: Option<bool>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Replica {
    replica: String,
    role: String,
    report_file: PathBuf,
    /// Observer UTC minus mutator UTC, calibrated externally before/after.
    clock_offset_ms: Option<f64>,
    clock_uncertainty_ms: Option<f64>,
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
    drop(child);
    drop(runtime);
    if signal_failed.load(Ordering::SeqCst) {
        return Err("observer_signal_failed".into());
    }
    Ok(
        json!({"schema":"vlite-meas005-observer/1","qualification":"proposal_evidence_only",
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
            "readiness_polls_are_not_Blob_GETs":true,"shutdown":"measurement_child_killed_and_reaped_at_end"}}),
    )
}

fn analyze(config: Analyze) -> Result<Value> {
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
        let calibrated = matches!((replica.clock_offset_ms, replica.clock_uncertainty_ms),
            (Some(offset), Some(error)) if offset.is_finite() && error.is_finite() && (0.0..=60000.).contains(&error))
            && report["max_clock_jump_ms"]
                .as_f64()
                .is_some_and(|x| x <= 10.);
        if !calibrated || report["terminated_early"] != false || report["interrupted"] != false {
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
                && calibrated
            {
                let offset = replica.clock_offset_ms.unwrap();
                // Expand by both calibration and observed local clock drift.
                let error = replica.clock_uncertainty_ms.unwrap()
                    + report["max_clock_jump_ms"].as_f64().unwrap()
                    + 10.;
                let c0 = commit["start_unix_ms"]
                    .as_f64()
                    .ok_or("commit_bracket_invalid")?;
                let c1 = commit["end_unix_ms"]
                    .as_f64()
                    .ok_or("commit_bracket_invalid")?;
                let o0 = event["install_lower_unix_ms"]
                    .as_f64()
                    .ok_or("observation_bracket_invalid")?
                    - offset
                    - error;
                let o1 = event["end_unix_ms"]
                    .as_f64()
                    .ok_or("observation_bracket_invalid")?
                    - offset
                    + error;
                let deadline = mutation["observation_deadline_ms"]
                    .as_f64()
                    .ok_or("observation_deadline_invalid")?;
                let lifetime_start = report["start_unix_ms"]
                    .as_f64()
                    .ok_or("observer_lifetime_invalid")?
                    - offset
                    + error;
                let lifetime_end = report["end_unix_ms"]
                    .as_f64()
                    .ok_or("observer_lifetime_invalid")?
                    - offset
                    - error;
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
                    && clock_consistent;
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
                "censoring":if covered {"interval"} else if !calibrated {"clock_unknown"} else if observed.is_some() {"deadline_or_lifetime_incomplete"} else {"not_observed"}}));
        }
        reports.push(json!({"replica":replica.replica,"role":replica.role,"boot_id":report["boot_id"],
            "clock_calibrated":calibrated,"samples":samples,"incomplete_or_censored":missing,
            "successful_lower_ms":summary(&lower_values),"successful_upper_ms":summary(&upper_values),
            "readiness_polls":report["polls"],"readiness_failures":report["readiness_failures"],
            "max_sample_start_gap_ms":report["max_sample_start_gap_ms"],
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
        "uncertainty":{"timing":"interval_censored_command_bracket","physical_blob_GETs":null,
            "logical_feed_polls":null,"clock_calibration":"supplied_external_bounds_not_inferred_from_ingress_RTT",
            "mutator_clock_drift_allowance_ms":10,
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
                clock_offset_ms:calibrated.then_some(0.), clock_uncertainty_ms:calibrated.then_some(0.) }
        }).collect();
        Analyze {
            mutation_evidence_file: file,
            replicas,
            fixed_inventory_confirmed: Some(true),
        }
    }

    #[test]
    fn analysis_requires_actual_floor_and_clocks_and_keeps_unknowns_null() {
        let root = tempfile::tempdir().unwrap();
        let measured = analyze(analysis_fixture(root.path(), 1, true)).unwrap();
        assert_eq!(measured["complete"], true);
        assert_eq!(measured["replicas"][0]["successful_lower_ms"]["p99"], 60.);
        assert_eq!(measured["replicas"][0]["successful_upper_ms"]["p99"], 120.);
        let missing_floor = analyze(analysis_fixture(root.path(), 0, true)).unwrap();
        assert_eq!(missing_floor["complete"], false);
        assert_eq!(
            missing_floor["replicas"][1]["successful_upper_ms"]["count"],
            0
        );
        assert!(missing_floor["replicas"][1]["successful_upper_ms"]["p99"].is_null());
        let no_clocks = analyze(analysis_fixture(root.path(), 1, false)).unwrap();
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
            let report = analyze(config).unwrap();
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
        let report = analyze(config).unwrap();
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
        assert_eq!(analyze(config).unwrap()["complete"], false);
        let config = analysis_fixture(root.path(), 1, true);
        let mut observer: Value = read(&config.replicas[0].report_file).unwrap();
        observer["start_unix_ms"] = json!(1200);
        std::fs::write(&config.replicas[0].report_file, observer.to_string()).unwrap();
        assert_eq!(analyze(config).unwrap()["complete"], false);
    }
}
