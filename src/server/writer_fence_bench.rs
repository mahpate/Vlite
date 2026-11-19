//! MEAS-006: opt-in writer checks, not a stale-ack window or deployment SLO.

use std::{
    env,
    sync::Arc,
    time::{Duration, Instant},
};

use serde_json::json;
use tokio::sync::watch;

use super::{
    authority::AuthorityStore,
    cloud_backup::{AuthMode, ObjectStore, Target, azure, fake::Memory, random_hex},
    writer_authority::{claim, recheck_for_measurement},
    writer_fence::{State, WriterFence},
};

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

fn report(
    scope: &str,
    metric: &str,
    samples: &mut [Duration],
    unit: &str,
    interval_ms: Option<usize>,
) {
    assert!(!samples.is_empty(), "MEAS-006 cannot report zero samples");
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
    println!(
        "MEAS006 {}",
        json!({
            "scope": scope,
            "claim": "not ACA SLO or stale-ack-window proof",
            "metric": metric,
            "unit": unit,
            "count": samples.len(),
            "p50": percentile(50),
            "p95": percentile(95),
            "max": value(*samples.last().unwrap()),
            "interval_ms": interval_ms,
            "transfer_clock": interval_ms.map(|_| "immediately-before-transfer-CAS"),
        })
    );
}

#[test]
#[ignore = "MEAS-006 probe; requires VLITE_MEAS006=1"]
fn writer_fence_gate_measurements() -> Result<(), String> {
    opt_in()?;
    let probe = Probe::connect()?;
    let (default, max) = if probe.scope == "local-fake" {
        (2000, 100_000)
    } else {
        (100, 2000)
    };
    let count = bounded("VLITE_MEAS006_SAMPLES", default, 1, max)?;
    let (_store, fence) = setup(&probe.objects)?;
    for (metric, commit, ack) in [
        ("verify_before_commit", true, false),
        ("verify_before_ack", false, true),
        ("commit_then_ack", true, true),
    ] {
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
        report(probe.scope, metric, &mut samples, "microseconds", None);
    }
    Ok(())
}

#[test]
#[ignore = "MEAS-006 probe; requires VLITE_MEAS006=1"]
fn writer_fence_detection_measurements() -> Result<(), String> {
    opt_in()?;
    let probe = Probe::connect()?;
    let repetitions = bounded("VLITE_MEAS006_REPETITIONS", 20, 1, 100)?;
    let interval_ms = bounded("VLITE_MEAS006_INTERVAL_MS", 50, 5, 5000)?;
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
    report(
        probe.scope,
        "transfer_to_observed_fenced",
        &mut samples,
        "milliseconds",
        Some(interval_ms),
    );
    Ok(())
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
