//! Repeatable bounded mixed-load maintenance latency capture.
//!
//! Run with `cargo run --release --example maintenance_mixed_load -- [--output FILE]`.
//! The optional output file is created without overwriting an existing archive.
//! Raw nanosecond samples, configuration, dataset digest, and build/platform
//! provenance are retained; timings are observations, not pass/fail thresholds.

use std::{
    error::Error,
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use vlite::{
    AnnOptions, Condition, Database, DistanceMetric, HnswConfig, HnswRebuildLimits, Metadata,
    MetadataValue, Mutation,
};

const RECORDS: usize = 2_048;
const DIMS: usize = 32;
const BASELINE_ROUNDS: usize = 8;
const REBUILD_ROUNDS: usize = 320;
const POST_ROUNDS: usize = 8;
const LARGE_PAYLOAD_BYTES: usize = 32 * 1024;
const SEED: u64 = 0x564C_4954_455F_3341;

#[derive(Clone)]
struct Sample {
    phase: &'static str,
    operation: &'static str,
    round: usize,
    elapsed_ns: u128,
    rebuild_pending: bool,
}

struct Workload<'a> {
    vectors: &'a [Vec<f32>],
    large_payload: &'a [u8],
}

fn percentile(samples: &[u128], quantile: f64) -> u128 {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    sorted[((sorted.len() - 1) as f64 * quantile).ceil() as usize]
}

fn summarize(samples: &[Sample]) -> Vec<Value> {
    let mut groups = std::collections::BTreeMap::<(&str, &str), Vec<u128>>::new();
    for sample in samples {
        groups
            .entry((sample.phase, sample.operation))
            .or_default()
            .push(sample.elapsed_ns);
    }
    groups
        .into_iter()
        .map(|((phase, operation), values)| {
            json!({
                "phase": phase,
                "operation": operation,
                "count": values.len(),
                "p50_ns": percentile(&values, 0.50),
                "p95_ns": percentile(&values, 0.95),
                "max_ns": values.iter().copied().max().unwrap_or_default(),
            })
        })
        .collect()
}

fn measure<T>(
    samples: &mut Vec<Sample>,
    phase: &'static str,
    operation: &'static str,
    round: usize,
    rebuild_pending: bool,
    f: impl FnOnce() -> vlite::Result<T>,
) -> vlite::Result<T> {
    let started = Instant::now();
    let value = f()?;
    samples.push(Sample {
        phase,
        operation,
        round,
        elapsed_ns: started.elapsed().as_nanos(),
        rebuild_pending,
    });
    Ok(value)
}

fn vectors() -> (Vec<Vec<f32>>, String) {
    let mut state = SEED;
    let mut digest = Sha256::new();
    let rows = (0..RECORDS)
        .map(|_| {
            (0..DIMS)
                .map(|_| {
                    state = state
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407);
                    let value = ((state >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0;
                    digest.update(value.to_bits().to_le_bytes());
                    value
                })
                .collect()
        })
        .collect::<Vec<_>>();
    let digest = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    (rows, digest)
}

fn group(id: u64) -> Metadata {
    Metadata::from([("group".to_owned(), MetadataValue::Integer((id % 16) as i64))])
}

fn run_rounds(
    db: &mut Database,
    workload: &Workload<'_>,
    samples: &mut Vec<Sample>,
    phase: &'static str,
    rounds: usize,
    id_base: u64,
    stages: &mut Vec<String>,
) -> vlite::Result<()> {
    let metric = DistanceMetric::Cosine;
    let config = HnswConfig::default();
    let pending = |db: &Database| -> vlite::Result<bool> {
        Ok(db
            .hnsw_maintenance_status(config, metric, Default::default())?
            .rebuild
            .is_some())
    };
    for round in 0..rounds {
        let id = id_base + round as u64;
        let vector = &workload.vectors[round.wrapping_mul(17) % workload.vectors.len()];
        let payload: &[u8] = if round % 8 == 0 {
            workload.large_payload
        } else {
            b"small-update"
        };
        measure(samples, phase, "write", round, pending(db)?, || {
            db.put_with_metadata(id, vector, payload, &group(id))
        })?;
        measure(samples, phase, "read_scan", round, pending(db)?, || {
            db.search_with_metric(vector, 10, metric)
        })?;
        measure(samples, phase, "approximate_read", round, pending(db)?, || {
            db.search_approximate_with_metric(vector, 10, 100, config, metric)
        })?;
        let condition = [Condition::Eq {
            field: "group".to_owned(),
            value: MetadataValue::Integer((id % 16) as i64),
        }];
        measure(samples, phase, "filtered_read", round, pending(db)?, || {
            db.search_approximate_filtered_with_metric(vector, 10, 100, config, &condition, metric)
        })?;
        if phase == "rebuild"
            && let Some(progress) = db
                .hnsw_maintenance_status(config, metric, Default::default())?
                .rebuild
            && stages.last().map(String::as_str) != Some(progress.phase)
        {
            stages.push(progress.phase.to_owned());
        }
    }
    Ok(())
}

fn output_path() -> Result<Option<PathBuf>, Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let Some(flag) = args.next() else {
        return Ok(None);
    };
    let path = if flag == "--output" {
        args.next().ok_or("--output requires a path")?
    } else if flag.starts_with("--") {
        return Err(format!("unknown argument {flag}").into());
    } else {
        flag
    };
    if args.next().is_some() {
        return Err("unexpected trailing argument".into());
    }
    Ok(Some(path.into()))
}

fn main() -> Result<(), Box<dyn Error>> {
    let archive_path = output_path()?;
    let (vectors, data_sha256) = vectors();
    let directory = tempfile::tempdir()?;
    let mut db = Database::create(directory.path().join("mixed-load.vlite"), DIMS)?;
    for (chunk_no, chunk) in vectors.chunks(256).enumerate() {
        let operations = chunk
            .iter()
            .enumerate()
            .map(|(offset, vector)| {
                let id = (chunk_no * 256 + offset) as u64;
                Mutation::PutWithMetadata {
                    id,
                    vector: vector.clone(),
                    payload: b"initial-payload".to_vec(),
                    metadata: group(id),
                }
            })
            .collect::<Vec<_>>();
        db.write_batch(&operations)?;
    }

    let config = HnswConfig::default();
    let metric = DistanceMetric::Cosine;
    let options = AnnOptions::default();
    db.build_hnsw_with_options(config, metric, options)?;
    let mut samples = Vec::new();
    let mut stages = Vec::new();
    let large_payload = vec![0xA5; LARGE_PAYLOAD_BYTES];
    let workload = Workload {
        vectors: &vectors,
        large_payload: &large_payload,
    };

    run_rounds(
        &mut db,
        &workload,
        &mut samples,
        "baseline",
        BASELINE_ROUNDS,
        20_000,
        &mut stages,
    )?;

    db.set_background_hnsw_rebuild(Some(HnswRebuildLimits::default()))?;
    let rebuild_started = Instant::now();
    db.start_hnsw_rebuild_with_options(config, metric, options, HnswRebuildLimits::default())?;
    let rebuild_start_ns = rebuild_started.elapsed().as_nanos();
    run_rounds(
        &mut db,
        &workload,
        &mut samples,
        "rebuild",
        REBUILD_ROUNDS,
        21_000,
        &mut stages,
    )?;

    let wait_started = Instant::now();
    let deadline = Instant::now() + std::time::Duration::from_secs(60);
    loop {
        match db.poll_hnsw_rebuild()? {
            vlite::HnswRebuildStatus::Published | vlite::HnswRebuildStatus::Idle => break,
            vlite::HnswRebuildStatus::Running => {}
            outcome => return Err(format!("rebuild ended as {outcome:?}").into()),
        }
        if Instant::now() >= deadline {
            return Err("background rebuild exceeded its 60-second bound".into());
        }
        std::thread::yield_now();
    }
    let rebuild_wait_ns = wait_started.elapsed().as_nanos();

    run_rounds(
        &mut db,
        &workload,
        &mut samples,
        "published",
        POST_ROUNDS,
        22_000,
        &mut stages,
    )?;

    let raw_samples = samples
        .iter()
        .map(|sample| {
            json!({
                "phase": sample.phase,
                "operation": sample.operation,
                "round": sample.round,
                "elapsed_ns": sample.elapsed_ns,
                "rebuild_pending": sample.rebuild_pending,
            })
        })
        .collect::<Vec<_>>();
    let max_query_during_rebuild_phase_ns = samples
        .iter()
        .filter(|sample| {
            sample.phase == "rebuild"
                && matches!(sample.operation, "approximate_read" | "filtered_read")
        })
        .map(|sample| sample.elapsed_ns)
        .max()
        .unwrap_or_default();
    let max_query_with_rebuild_pending_ns = samples
        .iter()
        .filter(|sample| {
            sample.rebuild_pending
                && matches!(sample.operation, "approximate_read" | "filtered_read")
        })
        .map(|sample| sample.elapsed_ns)
        .max();
    let archive = json!({
        "schema": 1,
        "benchmark": "maintenance_mixed_load",
        "captured_unix_ms": SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
        "provenance": {
            "vlite_version": env!("CARGO_PKG_VERSION"),
            "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "source": "deterministic synthetic vectors",
            "seed": SEED,
            "vector_sha256": data_sha256,
        },
        "config": {
            "records": RECORDS,
            "dimensions": DIMS,
            "baseline_rounds": BASELINE_ROUNDS,
            "rebuild_rounds": REBUILD_ROUNDS,
            "published_rounds": POST_ROUNDS,
            "large_payload_bytes": LARGE_PAYLOAD_BYTES,
            "large_payload_every_nth_write": 8,
            "metric": "cosine",
            "query_k": 10,
            "ef_search": 100,
            "background_hnsw_rebuild": true,
            "hnsw": {
                "m": config.m,
                "ef_construction": config.ef_construction,
                "seed": config.seed,
            },
            "measurement": "single-process operation latency; no pass/fail threshold",
        },
        "rebuild_start_ns": rebuild_start_ns,
        "rebuild_wait_ns": rebuild_wait_ns,
        "max_query_during_rebuild_phase_ns": max_query_during_rebuild_phase_ns,
        "max_query_with_rebuild_pending_ns": max_query_with_rebuild_pending_ns,
        "observed_rebuild_phases": stages,
        "summary": summarize(&samples),
        "raw_samples": raw_samples,
    });
    db.close()?;
    let bytes = serde_json::to_vec_pretty(&archive)?;
    if let Some(path) = archive_path {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.write_all(&bytes)?;
        println!("archived {} samples to {}", samples.len(), path.display());
    } else {
        println!("{}", String::from_utf8(bytes)?);
    }
    Ok(())
}
