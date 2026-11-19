//! MEAS-006: opt-in writer checks, not a stale-ack window or deployment SLO.
//!
//! `writer_fence_meas006_document` is the job-side entry point: it runs both
//! probes below and writes one JSON document to `VLITE_MEAS006_OUT` only after
//! every metric is complete. `deploy/azure-container-apps/ha/meas006-collect.sh`
//! sets that path and prints the document for the measurement image's existing
//! gzip/SHA-256 transport.

use std::{
    env,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use serde_json::{Map, Value, json};
use tokio::sync::watch;

use super::{
    authority::AuthorityStore,
    cloud_backup::{AuthMode, ObjectStore, Target, azure, fake::Memory, random_hex},
    writer_authority::{claim, recheck_for_measurement},
    writer_fence::{State, WriterFence},
};

/// Gate metrics and which checks each sample runs: (name, commit, ack).
const GATES: [(&str, bool, bool); 3] = [
    ("verify_before_commit", true, false),
    ("verify_before_ack", false, true),
    ("commit_then_ack", true, true),
];
const TRANSFER: &str = "transfer_to_observed_fenced";
/// Name of the document test, invoked by `meas006-collect.sh`.
const DOCUMENT_TEST: &str = "writer_fence_meas006_document";

fn opt_in() -> Result<(), String> {
    if env::var("VLITE_MEAS006").as_deref() != Ok("1") {
        return Err("MEAS-006 requires explicit opt-in: set VLITE_MEAS006=1".into());
    }
    Ok(())
}

#[test]
fn blob_measurement_refuses_unrelated_targets() {
    for url in [
        "https://example.org/devstoreaccount1",
        "http://user@127.0.0.1:10010/devstoreaccount1",
        "http://127.0.0.1:10010/devstoreaccount1?sig=value",
        "http://127.0.0.1:10010/other-account",
    ] {
        assert!(emulator_target(url).is_err());
    }
    let local = emulator_target("http://127.0.0.1:10010/devstoreaccount1").unwrap();
    assert_eq!(local.auth, super::cloud_backup::AuthMode::EmulatorSharedKey);
    assert!(job_target("run001", "vlite-app", "unused").is_err());
    assert!(job_target("RUN!", "vlite-meas-RUN!-c1", "unused").is_err());
    let id = "01234567-89ab-cdef-0123-456789abcdef";
    let aca = job_target("run001", "vlite-meas-run001-c1", id).unwrap();
    assert_eq!(aca.account, "vlitemeasrun001st");
    assert!(aca.endpoint.is_none());
    assert!(aca.validate().is_ok());
}

const REVISION: &str = "0123456789abcdef0123456789abcdef01234567";

fn sample_metrics(requested: &Requested) -> Vec<Value> {
    let mut metrics: Vec<_> = GATES
        .iter()
        .map(|&(metric, ..)| {
            let mut samples = vec![Duration::from_micros(10); requested.gate_samples];
            summary(metric, &mut samples, "microseconds", None).unwrap()
        })
        .collect();
    let mut transfers = vec![Duration::from_millis(51); requested.transfer_repetitions];
    let interval = Some(requested.recheck_interval_ms);
    metrics.push(summary(TRANSFER, &mut transfers, "milliseconds", interval).unwrap());
    metrics
}

fn clean_source() -> Value {
    source_provenance(Some(REVISION.into()), Some(String::new())).unwrap()
}

const REQUESTED: Requested = Requested {
    gate_samples: 3,
    transfer_repetitions: 2,
    recheck_interval_ms: 50,
};

#[test]
fn document_reports_each_metric_once_with_scope_source_and_limits() {
    let doc = document(
        "aca-blob",
        clean_source(),
        &REQUESTED,
        sample_metrics(&REQUESTED),
    )
    .unwrap();
    assert_eq!(doc["schema"], "vlite-meas006/1");
    assert_eq!(doc["measurement"], "MEAS-006");
    assert_eq!(doc["scope"], "aca-blob");
    assert_eq!(
        doc["source"],
        json!({"revision": REVISION, "working_tree_dirty": false, "dirty_paths": []})
    );
    assert_eq!(
        doc["requested"],
        json!({"gate_samples": 3, "transfer_repetitions": 2, "recheck_interval_ms": 50})
    );
    let metrics = doc["metrics"].as_object().unwrap();
    assert_eq!(metrics.len(), 4);
    for (name, ..) in GATES {
        assert_eq!(metrics[name]["count"], 3);
        assert_eq!(metrics[name]["unit"], "microseconds");
        assert_eq!(metrics[name]["p50"], 10.0);
        assert!(metrics[name].get("interval_ms").is_none());
    }
    assert_eq!(metrics[TRANSFER]["count"], 2);
    assert_eq!(metrics[TRANSFER]["unit"], "milliseconds");
    assert_eq!(metrics[TRANSFER]["interval_ms"], 50);
    assert_eq!(metrics[TRANSFER]["max"], 51.0);
    assert_eq!(
        doc["limits"]["not_measured"],
        json!(["stale_acknowledgment_window", "clock_skew", "slo"])
    );
    assert!(!doc.to_string().contains("client"));
}

#[test]
fn document_refuses_partial_or_inconsistent_metrics() {
    let full = sample_metrics(&REQUESTED);
    let edited = |index: usize, edit: fn(&mut Value)| {
        let mut metrics = full.clone();
        edit(&mut metrics[index]);
        metrics
    };
    for metrics in [
        Vec::new(),
        full[..3].to_vec(),
        full[1..].to_vec(),
        [full.clone(), vec![full[0].clone()]].concat(),
        [full[..3].to_vec(), vec![full[0].clone()]].concat(),
        edited(0, |m| m["metric"] = json!("verify_unknown")),
        edited(1, |m| _ = m.as_object_mut().unwrap().remove("metric")),
        edited(2, |m| m["count"] = json!(2)),
        edited(3, |m| m["count"] = json!(3)),
    ] {
        assert!(document("local-fake", Value::Null, &REQUESTED, metrics).is_err());
    }
    assert!(summary("verify_before_ack", &mut [], "microseconds", None).is_err());
}

#[test]
fn aca_document_requires_scoped_blob_and_clean_source() {
    let dirty = source_provenance(Some(REVISION.into()), Some(" M src/lib.rs\n".into())).unwrap();
    assert_eq!(dirty["working_tree_dirty"], true);
    assert_eq!(dirty["dirty_paths"], json!([" M src/lib.rs"]));
    for scope in ["local-fake", "local-azurite"] {
        assert!(check_run(scope, true, &clean_source()).is_err());
        assert!(check_run(scope, false, &Value::Null).is_ok());
        assert!(check_run(scope, false, &dirty).is_ok());
    }
    assert!(check_run("aca-blob", true, &clean_source()).is_ok());
    assert!(check_run("aca-blob", true, &dirty).is_err());
    assert!(check_run("aca-blob", true, &Value::Null).is_err());
    assert_eq!(source_provenance(None, None).unwrap(), Value::Null);
    assert!(source_provenance(Some(REVISION.into()), None).is_err());
    assert!(source_provenance(None, Some(String::new())).is_err());
    let (upper, invalid) = (REVISION.to_uppercase(), "g".repeat(40));
    for revision in [&REVISION[1..], upper.as_str(), invalid.as_str()] {
        assert!(source_provenance(Some(revision.into()), Some(String::new())).is_err());
    }
}

#[test]
fn document_file_must_be_new() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("meas006.json");
    let doc = json!({"measurement": "MEAS-006"});
    write_document(&path, &doc).unwrap();
    assert!(write_document(&path, &json!({"stale": true})).is_err());
    let written: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(written, doc);
}

/// The ACA collector must print only a complete document from the exact test.
#[cfg(unix)]
#[test]
fn collector_prints_only_a_complete_document() {
    use std::{os::unix::fs::PermissionsExt, process::Command};
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/deploy/azure-container-apps/ha/meas006-collect.sh"
    );
    let test = format!(
        "{}::{DOCUMENT_TEST}",
        module_path!().split_once("::").unwrap().1
    );
    let dir = tempfile::tempdir().unwrap();
    let stub = dir.path().join("lib-tests");
    std::fs::write(
        &stub,
        format!(
            "#!/bin/sh\n\
             [ \"$*\" = \"--ignored --exact --test-threads=1 --nocapture {test}\" ] || exit 9\n\
             echo 'running 1 test'\n\
             case \"$STUB\" in\n\
             ok) printf '{{\"ok\":true}}\\n' >\"$VLITE_MEAS006_OUT\" ;;\n\
             fail) printf '{{\"partial\":' >\"$VLITE_MEAS006_OUT\"; exit 101 ;;\n\
             esac\n\
             echo 'test result: ok'\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    let stale = dir.path().join("stale.json");
    let run = |mode: &str, args: &[&Path]| {
        Command::new("sh")
            .arg(script)
            .args(args)
            .env("STUB", mode)
            .env("VLITE_MEAS006_OUT", &stale)
            .output()
            .unwrap()
    };
    let ok = run("ok", &[&stub]);
    assert_eq!(ok.status.code(), Some(0));
    assert_eq!(ok.stdout, b"{\"ok\":true}\n");
    let failed = run("fail", &[&stub]);
    assert_eq!(failed.status.code(), Some(101));
    assert!(failed.stdout.is_empty());
    // A filtered-out or still-ignored test exits 0 without a document.
    let skipped = run("none", &[&stub]);
    assert!(!skipped.status.success());
    assert!(skipped.stdout.is_empty());
    let usage = run("ok", &[]);
    assert_eq!(usage.status.code(), Some(2));
    assert!(usage.stdout.is_empty());
    assert!(!stale.exists());
}

fn bounded(name: &str, default: usize, min: usize, max: usize) -> Result<usize, String> {
    let value = match env::var(name) {
        Ok(value) => value
            .parse::<usize>()
            .map_err(|_| format!("{name} must be an integer in {min}..={max}"))?,
        Err(env::VarError::NotPresent) => default,
        Err(error) => return Err(format!("{name}: {error}")),
    };
    if !(min..=max).contains(&value) {
        return Err(format!("{name} must be in {min}..={max}, got {value}"));
    }
    Ok(value)
}

fn emulator_target(endpoint: &str) -> Result<Target, String> {
    let url = reqwest::Url::parse(endpoint).map_err(|_| "invalid Azurite endpoint")?;
    if !matches!(url.scheme(), "http" | "https")
        || !matches!(url.host_str(), Some("127.0.0.1" | "localhost"))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.query().is_some()
        || url.path().trim_end_matches('/') != "/devstoreaccount1"
    {
        return Err("MEAS-006 requires an uncredentialed loopback Azurite endpoint".into());
    }
    Ok(Target {
        account: "devstoreaccount1".into(),
        endpoint: Some(endpoint.to_owned()),
        auth: AuthMode::EmulatorSharedKey,
        container: "coordination".into(),
        coordination_container: None,
        prefix: "meas006/".into(),
        client_id: None,
    })
}

fn job_target(run: &str, job: &str, client_id: &str) -> Result<Target, String> {
    if !(3..=12).contains(&run.len())
        || !run
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        || job != format!("vlite-meas-{run}-c1")
    {
        return Err("MEAS-006 requires a dedicated measurement job".into());
    }
    let target = Target {
        account: format!("vlitemeas{run}st"),
        endpoint: None,
        auth: AuthMode::ManagedIdentity,
        container: "coordination".into(),
        coordination_container: None,
        prefix: "meas006/".into(),
        client_id: Some(client_id.to_owned()),
    };
    target.validate()?;
    Ok(target)
}

struct Probe {
    scope: &'static str,
    objects: Arc<dyn ObjectStore>,
}

impl Probe {
    fn connect() -> Result<Self, String> {
        let env_text = |name| env::var(name).map_err(|_| format!("missing or invalid {name}"));
        let (target, key, scope) = match (
            env::var_os("VLITE_MEAS006_AZURITE"),
            env::var_os("VLITE_MEAS006_ACA_RUN_ID"),
        ) {
            (None, None) => {
                return Ok(Self {
                    scope: "local-fake",
                    objects: Arc::new(Memory::default()),
                });
            }
            (Some(_), Some(_)) => return Err("select only one MEAS-006 storage mode".into()),
            (Some(_), None) => {
                let target = emulator_target(&env_text("VLITE_MEAS006_AZURITE")?)?;
                let path = env_text("VLITE_MEAS006_KEY_FILE")?;
                let key = azure::read_shared_key(path.as_ref())?;
                (target, Some(key), "local-azurite")
            }
            (None, Some(_)) => {
                let target = job_target(
                    &env_text("VLITE_MEAS006_ACA_RUN_ID")?,
                    &env_text("CONTAINER_APP_JOB_NAME")?,
                    &env_text("VLITE_MEAS006_CLIENT_ID")?,
                )?;
                (target, None, "aca-blob")
            }
        };
        let sets = azure::connect(&target, key).map_err(|e| e.code.to_string())?;
        Ok(Self {
            scope,
            objects: sets.coordination().0,
        })
    }
}

fn setup(objects: &Arc<dyn ObjectStore>) -> Result<(AuthorityStore, WriterFence), String> {
    // Never claim or transfer an existing namespace, even on a repeated run.
    let prefix = format!("meas006/{}/", random_hex().map_err(|e| e.code)?);
    let store = AuthorityStore::new(objects.clone(), prefix.clone());
    store
        .create("installation", "scope", "0123456789abcdef")
        .map_err(|error| format!("create measurement authority: {error:?}"))?;
    let fence = claim(
        AuthorityStore::new(objects.clone(), prefix),
        Some(0x0123456789abcdef),
    )?;
    assert_eq!(fence.state(), State::Serving);
    Ok((store, fence))
}

/// Summarizes one metric; the shape of the existing `MEAS006` lines.
fn summary(
    metric: &str,
    samples: &mut [Duration],
    unit: &str,
    interval_ms: Option<usize>,
) -> Result<Value, String> {
    if samples.is_empty() {
        return Err(format!("MEAS-006 {metric} has no samples"));
    }
    samples.sort_unstable();
    let value = |duration: Duration| {
        duration.as_secs_f64()
            * match unit {
                "microseconds" => 1_000_000.0,
                "milliseconds" => 1_000.0,
                _ => unreachable!("measurement unit"),
            }
    };
    let percentile = |percent: usize| value(samples[(samples.len() * percent).div_ceil(100) - 1]);
    Ok(json!({
        "metric": metric,
        "unit": unit,
        "count": samples.len(),
        "p50": percentile(50),
        "p95": percentile(95),
        "max": value(*samples.last().unwrap()),
        "interval_ms": interval_ms,
        "transfer_clock": interval_ms.map(|_| "immediately-before-transfer-CAS"),
    }))
}

fn print_line(scope: &str, mut metric: Value) {
    metric["scope"] = json!(scope);
    metric["claim"] = json!("not ACA SLO or stale-ack-window proof");
    println!("MEAS006 {metric}");
}

fn gate_samples(scope: &str) -> Result<usize, String> {
    let (default, max) = if scope == "local-fake" {
        (2000, 100_000)
    } else {
        (100, 2000)
    };
    bounded("VLITE_MEAS006_SAMPLES", default, 1, max)
}

/// Transfer repetitions and recheck interval in milliseconds.
fn detection_settings() -> Result<(usize, usize), String> {
    Ok((
        bounded("VLITE_MEAS006_REPETITIONS", 20, 1, 100)?,
        bounded("VLITE_MEAS006_INTERVAL_MS", 50, 5, 5000)?,
    ))
}

fn gate_metrics(probe: &Probe, count: usize) -> Result<Vec<Value>, String> {
    let (_store, fence) = setup(&probe.objects)?;
    let mut metrics = Vec::with_capacity(GATES.len());
    for (metric, commit, ack) in GATES {
        let mut samples = Vec::with_capacity(count);
        for _ in 0..count {
            let start = Instant::now();
            if commit {
                fence
                    .verify_before_commit()
                    .map_err(|error| format!("{metric}: commit gate failed: {error:?}"))?;
            }
            if ack {
                fence
                    .verify_before_ack()
                    .map_err(|error| format!("{metric}: ack gate failed: {error:?}"))?;
            }
            samples.push(start.elapsed());
        }
        metrics.push(summary(metric, &mut samples, "microseconds", None)?);
    }
    Ok(metrics)
}

fn detection_metric(
    probe: &Probe,
    repetitions: usize,
    interval_ms: usize,
) -> Result<Value, String> {
    let interval = Duration::from_millis(interval_ms as u64);
    let timeout = interval * 3 + Duration::from_secs(2);
    let mut samples = Vec::with_capacity(repetitions);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|_| "cannot start the authority recheck runtime")?;
    for _ in 0..repetitions {
        let (store, fence) = setup(&probe.objects)?;
        samples.push(runtime.block_on(observe_transfer(store, fence, interval, timeout))?);
    }
    summary(TRANSFER, &mut samples, "milliseconds", Some(interval_ms))
}

#[test]
#[ignore = "MEAS-006 probe; requires VLITE_MEAS006=1"]
fn writer_fence_gate_measurements() -> Result<(), String> {
    opt_in()?;
    let probe = Probe::connect()?;
    let count = gate_samples(probe.scope)?;
    for metric in gate_metrics(&probe, count)? {
        print_line(probe.scope, metric);
    }
    Ok(())
}

#[test]
#[ignore = "MEAS-006 probe; requires VLITE_MEAS006=1"]
fn writer_fence_detection_measurements() -> Result<(), String> {
    opt_in()?;
    let probe = Probe::connect()?;
    let (repetitions, interval_ms) = detection_settings()?;
    print_line(
        probe.scope,
        detection_metric(&probe, repetitions, interval_ms)?,
    );
    Ok(())
}

struct Requested {
    gate_samples: usize,
    transfer_repetitions: usize,
    recheck_interval_ms: usize,
}

fn optional(name: &str) -> Result<Option<String>, String> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(format!("{name}: {error}")),
    }
}

/// The image's build provenance (as `ha_reader_resource`/`ha_meas002`), or null.
fn source_provenance(
    revision: Option<String>,
    dirty_paths: Option<String>,
) -> Result<Value, String> {
    match (revision, dirty_paths) {
        (None, None) => Ok(Value::Null),
        (Some(revision), Some(dirty_paths)) => {
            if revision.len() != 40
                || !revision
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(
                    "VLITE_RESOURCE_SOURCE_REVISION must be a full lowercase Git commit ID".into(),
                );
            }
            let dirty_paths: Vec<_> = dirty_paths.lines().collect();
            Ok(json!({
                "revision": revision,
                "working_tree_dirty": !dirty_paths.is_empty(),
                "dirty_paths": dirty_paths,
            }))
        }
        _ => Err(
            "MEAS-006 source provenance requires both VLITE_RESOURCE_SOURCE_REVISION \
             and VLITE_RESOURCE_SOURCE_DIRTY_PATHS, or neither"
                .into(),
        ),
    }
}

/// An ACA job must measure its scoped Blob account from clean, attributed source.
fn check_run(scope: &str, aca_job: bool, source: &Value) -> Result<(), String> {
    if aca_job && scope != "aca-blob" {
        return Err(format!(
            "an ACA MEAS-006 run requires aca-blob storage, not {scope}"
        ));
    }
    if scope == "aca-blob" && source["working_tree_dirty"] != json!(false) {
        return Err("aca-blob MEAS-006 requires clean source provenance".into());
    }
    Ok(())
}

/// Builds the job document; refuses anything but each metric once, fully sampled.
fn document(
    scope: &str,
    source: Value,
    requested: &Requested,
    metrics: Vec<Value>,
) -> Result<Value, String> {
    let expected = |name: &str| {
        if name == TRANSFER {
            Some(requested.transfer_repetitions)
        } else {
            GATES
                .iter()
                .any(|&(gate, ..)| gate == name)
                .then_some(requested.gate_samples)
        }
    };
    let mut reported = Map::new();
    for metric in metrics {
        let Value::Object(mut body) = metric else {
            return Err("MEAS-006 metric is not an object".into());
        };
        let Some(Value::String(name)) = body.remove("metric") else {
            return Err("MEAS-006 metric has no name".into());
        };
        let count = expected(&name).ok_or_else(|| format!("unexpected MEAS-006 metric {name}"))?;
        if body.get("count").and_then(Value::as_u64) != Some(count as u64) || count == 0 {
            return Err(format!(
                "MEAS-006 {name} does not have the requested {count} samples"
            ));
        }
        body.retain(|_, value| !value.is_null());
        if reported.insert(name.clone(), Value::Object(body)).is_some() {
            return Err(format!("duplicate MEAS-006 metric {name}"));
        }
    }
    if reported.len() != GATES.len() + 1 {
        return Err(format!(
            "incomplete MEAS-006 run: {} of {} metrics",
            reported.len(),
            GATES.len() + 1
        ));
    }
    Ok(json!({
        "schema": "vlite-meas006/1",
        "measurement": "MEAS-006",
        "scope": scope,
        "source": source,
        "requested": {
            "gate_samples": requested.gate_samples,
            "transfer_repetitions": requested.transfer_repetitions,
            "recheck_interval_ms": requested.recheck_interval_ms,
        },
        "metrics": reported,
        "limits": {
            "claim": "development evidence of writer fence check cost and transfer detection; \
                      not an SLO or HA qualification",
            "not_measured": ["stale_acknowledgment_window", "clock_skew", "slo"],
            "gates": "sequential checks by one writer on a fresh authority prefix; \
                      each check reads the authority object",
            "transfer": "clock starts immediately before the transfer CAS and includes it; \
                         phase-aligned with the recheck start, so about one interval plus \
                         timer, scheduling and storage delay: an idle writer's worst case, \
                         not an average",
        },
    }))
}

fn write_document(path: &Path, document: &Value) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(document).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("create the MEAS-006 document: {error}"))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("write the MEAS-006 document: {error}"))
}

/// Job-side entry point: all four metrics from one run, or no document.
/// Without `VLITE_MEAS006_OUT` it measures nothing, so the local
/// `writer_fence_bench -- --ignored` probe run keeps its two-test behavior.
#[test]
#[ignore = "MEAS-006 job document; requires VLITE_MEAS006=1 and VLITE_MEAS006_OUT"]
fn writer_fence_meas006_document() -> Result<(), String> {
    opt_in()?;
    let Some(out) = env::var_os("VLITE_MEAS006_OUT").map(PathBuf::from) else {
        eprintln!("{DOCUMENT_TEST}: no document; VLITE_MEAS006_OUT is unset");
        return Ok(());
    };
    if out.as_os_str().is_empty() || out.exists() {
        return Err(format!(
            "{DOCUMENT_TEST} requires VLITE_MEAS006_OUT naming a new file"
        ));
    }
    let source = source_provenance(
        optional("VLITE_RESOURCE_SOURCE_REVISION")?,
        optional("VLITE_RESOURCE_SOURCE_DIRTY_PATHS")?,
    )?;
    let aca_job = [
        "CONTAINER_APP_JOB_NAME",
        "CONTAINER_APP_JOB_EXECUTION_NAME",
        "VLITE_MEAS006_ACA_RUN_ID",
    ]
    .iter()
    .any(|name| env::var_os(name).is_some());
    let probe = Probe::connect()?;
    check_run(probe.scope, aca_job, &source)?;
    let (transfer_repetitions, recheck_interval_ms) = detection_settings()?;
    let requested = Requested {
        gate_samples: gate_samples(probe.scope)?,
        transfer_repetitions,
        recheck_interval_ms,
    };
    let mut metrics = gate_metrics(&probe, requested.gate_samples)?;
    metrics.push(detection_metric(
        &probe,
        transfer_repetitions,
        recheck_interval_ms,
    )?);
    write_document(&out, &document(probe.scope, source, &requested, metrics)?)
}

async fn observe_transfer(
    store: AuthorityStore,
    fence: WriterFence,
    interval: Duration,
    timeout: Duration,
) -> Result<Duration, String> {
    let (stop_tx, stop_rx) = watch::channel(false);
    let mut recheck = tokio::spawn(recheck_for_measurement(fence.clone(), interval, stop_rx));
    let measurement = async {
        let transfer = tokio::task::spawn_blocking(move || {
            let observed = store
                .read()
                .map_err(|error| format!("read transfer authority: {error:?}"))?;
            let start = Instant::now();
            let transferred = store
                .prepare_transfer(&observed, observed.record.epoch, &observed.record.revision)
                .map_err(|error| format!("transfer authority: {error:?}"))?;
            assert_eq!(transferred.record.epoch, observed.record.epoch + 1);
            Ok::<_, String>(start)
        })
        .await
        .map_err(|error| format!("transfer task: {error}"))??;
        tokio::time::timeout(timeout, async {
            loop {
                if fence.state() == State::Fenced {
                    return Instant::now().duration_since(transfer);
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .map_err(|_| format!("writer did not become Fenced within {timeout:?}"))
    }
    .await;
    // Signal shutdown even when the observation times out or transfer fails.
    stop_tx.send_replace(true);
    let stopped = match tokio::time::timeout(timeout, &mut recheck).await {
        Ok(result) => result
            .map_err(|error| format!("recheck task: {error}"))
            .and_then(|result| result),
        Err(_) => {
            recheck.abort();
            let _ = recheck.await;
            Err("recheck did not stop after watch notification".into())
        }
    };
    stopped?;
    measurement
}
