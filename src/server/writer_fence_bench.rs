//! MEAS-006: opt-in local measurements, not deployment SLO evidence.

use std::{
    env,
    sync::Arc,
    time::{Duration, Instant},
};

use serde_json::json;
use tokio::sync::watch;

use super::{
    authority::AuthorityStore,
    cloud_backup::fake::Memory,
    writer_authority::{claim, recheck_for_measurement},
    writer_fence::{State, WriterFence},
};

fn opt_in() -> Result<(), String> {
    if env::var("VLITE_MEAS006").as_deref() != Ok("1") {
        return Err("MEAS-006 requires explicit opt-in: set VLITE_MEAS006=1".into());
    }
    Ok(())
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

fn setup() -> Result<(AuthorityStore, WriterFence), String> {
    let memory = Arc::new(Memory::default());
    let store = AuthorityStore::new(memory.clone(), "meas006/".into());
    store
        .create("installation", "scope", "0123456789abcdef")
        .map_err(|error| format!("create measurement authority: {error:?}"))?;
    let fence = claim(
        AuthorityStore::new(memory, "meas006/".into()),
        Some(0x0123456789abcdef),
    )?;
    assert_eq!(fence.state(), State::Serving);
    Ok((store, fence))
}

fn report(metric: &str, samples: &mut [Duration], unit: &str, interval_ms: Option<usize>) {
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
            "scope": "local-fake",
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

fn azurite_skip_reason() -> Result<(), String> {
    match env::var("VLITE_MEAS006_AZURITE") {
        Err(env::VarError::NotPresent) => {
            println!("MEAS-006 Azurite skipped: VLITE_MEAS006_AZURITE is not set");
        }
        Err(error) => return Err(format!("VLITE_MEAS006_AZURITE: {error}")),
        Ok(endpoint) => {
            let url = reqwest::Url::parse(&endpoint)
                .map_err(|error| format!("invalid VLITE_MEAS006_AZURITE URL: {error}"))?;
            if !matches!(url.scheme(), "http" | "https")
                || !matches!(url.host_str(), Some("127.0.0.1" | "localhost"))
                || !url.username().is_empty()
                || url.password().is_some()
            {
                return Err("VLITE_MEAS006_AZURITE must be a 127.0.0.1/localhost HTTP(S) URL; no network request made".into());
            }
            // The existing object-boundary test creates its container through
            // private Azure internals; it exposes no reusable emulator fixture.
            println!(
                "MEAS-006 Azurite skipped: no reusable loopback authority/container test helper; no network request made"
            );
        }
    }
    Ok(())
}

#[test]
#[ignore = "MEAS-006 local probe; requires VLITE_MEAS006=1"]
fn writer_fence_gate_measurements() -> Result<(), String> {
    opt_in()?;
    let count = bounded("VLITE_MEAS006_SAMPLES", 2000, 1, 100_000)?;
    azurite_skip_reason()?;
    let (_store, fence) = setup()?;
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
        report(metric, &mut samples, "microseconds", None);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "MEAS-006 local probe; requires VLITE_MEAS006=1"]
async fn writer_fence_detection_measurements() -> Result<(), String> {
    opt_in()?;
    let repetitions = bounded("VLITE_MEAS006_REPETITIONS", 20, 1, 100)?;
    let interval_ms = bounded("VLITE_MEAS006_INTERVAL_MS", 50, 5, 5000)?;
    let interval = Duration::from_millis(interval_ms as u64);
    let timeout = interval * 3 + Duration::from_secs(2);
    let mut samples = Vec::with_capacity(repetitions);
    for _ in 0..repetitions {
        let (store, fence) = setup()?;
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
        let elapsed = measurement?;
        stopped?;
        samples.push(elapsed);
    }
    report(
        "transfer_to_observed_fenced",
        &mut samples,
        "milliseconds",
        Some(interval_ms),
    );
    Ok(())
}
