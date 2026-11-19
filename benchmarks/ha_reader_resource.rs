//! Separate, bounded local resource probe for the owner-approved reader paths.

#[cfg(target_os = "linux")]
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(target_os = "linux")]
use std::{
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Instant,
};

#[cfg(target_os = "linux")]
use serde_json::Value;
#[cfg(target_os = "linux")]
use serde_json::{Map, json};

use super::BoxResult;
#[cfg(target_os = "linux")]
use super::{
    command_output, nanos_since, require,
    strategies::{self, Baseline},
    timed,
};

#[cfg(target_os = "linux")]
const RESOURCE_RECORDS: u64 = 110_000;
#[cfg(target_os = "linux")]
const RESOURCE_DELTA: u64 = 300;
#[cfg(target_os = "linux")]
const RESOURCE_DIMENSIONS: usize = 64;
#[cfg(target_os = "linux")]
const RESOURCE_SAMPLE_INTERVAL: Duration = Duration::from_millis(10);
// Safety bounds on operator-selected inputs; they are not capacity claims.
#[cfg(target_os = "linux")]
const RESOURCE_RECORDS_LIMIT: u64 = 2_000_000;
#[cfg(target_os = "linux")]
const RESOURCE_DELTA_LIMIT: u64 = 100_000;
#[cfg(target_os = "linux")]
const RESOURCE_DIMENSIONS_LIMIT: u64 = 1_024;
#[cfg(target_os = "linux")]
const RESOURCE_VECTOR_VALUES_LIMIT: u64 = 32_000_000;

#[cfg(target_os = "linux")]
#[derive(Debug, PartialEq)]
pub(crate) struct ResourceProfile {
    pub(crate) records: u64,
    pub(crate) delta: u64,
    pub(crate) dimensions: usize,
    pub(crate) sources: [(&'static str, &'static str); 3],
}

#[cfg(target_os = "linux")]
impl ResourceProfile {
    fn overridden(&self) -> bool {
        self.sources
            .iter()
            .any(|(_, source)| *source == "environment")
    }
}

/// Reads optional `VLITE_RESOURCE_RECORDS`, `VLITE_RESOURCE_DELTA` and
/// `VLITE_RESOURCE_DIMENSIONS`; each must be a plain decimal within its bound.
#[cfg(target_os = "linux")]
pub(crate) fn resource_profile(
    lookup: impl Fn(&str) -> BoxResult<Option<String>>,
) -> BoxResult<ResourceProfile> {
    let field = |name: &'static str, default: u64, limit: u64| -> BoxResult<(u64, &'static str)> {
        let Some(value) = lookup(name)? else {
            return Ok((default, "default"));
        };
        require(
            !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()),
            &format!("{name} must be a plain decimal integer"),
        )?;
        let parsed = value
            .parse::<u64>()
            .map_err(|_| format!("{name} is out of range"))?;
        require(
            (1..=limit).contains(&parsed),
            &format!("{name} must be between 1 and {limit}"),
        )?;
        Ok((parsed, "environment"))
    };
    let (records, records_source) = field(
        "VLITE_RESOURCE_RECORDS",
        RESOURCE_RECORDS,
        RESOURCE_RECORDS_LIMIT,
    )?;
    let (delta, delta_source) =
        field("VLITE_RESOURCE_DELTA", RESOURCE_DELTA, RESOURCE_DELTA_LIMIT)?;
    let (dimensions, dimensions_source) = field(
        "VLITE_RESOURCE_DIMENSIONS",
        RESOURCE_DIMENSIONS as u64,
        RESOURCE_DIMENSIONS_LIMIT,
    )?;
    require(
        2 * (delta / 3) + 2 <= records,
        "VLITE_RESOURCE_DELTA is too large for VLITE_RESOURCE_RECORDS",
    )?;
    // The harness keeps several in-memory model copies; refuse accidental exhaustion.
    require(
        records.saturating_mul(dimensions) <= RESOURCE_VECTOR_VALUES_LIMIT,
        &format!(
            "VLITE_RESOURCE_RECORDS x VLITE_RESOURCE_DIMENSIONS must not exceed {RESOURCE_VECTOR_VALUES_LIMIT}"
        ),
    )?;
    Ok(ResourceProfile {
        records,
        delta,
        dimensions: usize::try_from(dimensions)?,
        sources: [
            ("baseline_records", records_source),
            ("delta_mutations", delta_source),
            ("dimensions", dimensions_source),
        ],
    })
}

#[cfg(target_os = "linux")]
fn environment_value(name: &str) -> BoxResult<Option<String>> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!("{name} must be Unicode").into()),
    }
}

#[cfg(target_os = "linux")]
#[derive(Default)]
struct SampleStats {
    sampling_interval: Duration,
    allocated_peak_bytes: u64,
    allocated_final_bytes: u64,
    rss_peak_bytes: u64,
    sample_count: u64,
    last_sample: Option<Instant>,
    max_sample_gap: Duration,
    max_scan_duration: Duration,
}

#[cfg(target_os = "linux")]
impl SampleStats {
    fn record(&mut self, sampled_at: Instant, scan_duration: Duration, disk: u64, rss: u64) {
        if let Some(previous) = self.last_sample {
            self.max_sample_gap = self.max_sample_gap.max(sampled_at.duration_since(previous));
        }
        self.last_sample = Some(sampled_at);
        self.max_scan_duration = self.max_scan_duration.max(scan_duration);
        self.allocated_peak_bytes = self.allocated_peak_bytes.max(disk);
        self.allocated_final_bytes = disk;
        self.rss_peak_bytes = self.rss_peak_bytes.max(rss);
        self.sample_count += 1;
    }

    fn to_json(&self) -> Value {
        json!({
            "allocated_blocks_peak_bytes": self.allocated_peak_bytes,
            "allocated_blocks_final_bytes": self.allocated_final_bytes,
            "allocated_blocks_unit_bytes": 512,
            "sampled_process_rss_peak_bytes": self.rss_peak_bytes,
            "process_rss_source": "Linux /proc/self/status VmRSS; sampled, includes harness and fixture model",
            "sample_count": self.sample_count,
            "sampling_interval_ms": self.sampling_interval.as_millis(),
            "max_observed_sample_gap_ms": self.max_sample_gap.as_secs_f64() * 1_000.0,
            "max_scan_duration_ms": self.max_scan_duration.as_secs_f64() * 1_000.0,
            "sampling_uncertainty": "Disk and VmRSS peaks are sampled; short-lived peaks between scans may be missed. The maximum observed gap and scan duration are retained."
        })
    }
}

#[cfg(target_os = "linux")]
fn sample(paths: &[PathBuf]) -> BoxResult<(u64, u64)> {
    let mut allocated = 0u64;
    for path in paths {
        allocated = allocated
            .checked_add(allocated_bytes(path)?)
            .ok_or("allocated disk byte count overflow")?;
    }
    let memory = process_memory()?;
    Ok((allocated, memory.rss_bytes))
}

#[cfg(target_os = "linux")]
pub(crate) fn observe_phase<T>(
    paths: &[PathBuf],
    interval: Duration,
    operation: impl FnOnce() -> BoxResult<T>,
) -> BoxResult<(T, Value)> {
    require(
        !paths.is_empty(),
        "resource observer needs at least one path",
    )?;
    require(
        !interval.is_zero(),
        "resource sampling interval must be nonzero",
    )?;
    let (disk, rss) = sample(paths)?;
    let mut stats = SampleStats {
        sampling_interval: interval,
        ..SampleStats::default()
    };
    let now = Instant::now();
    stats.record(now, Duration::ZERO, disk, rss);
    let stop = AtomicBool::new(false);
    let shared_stats = Mutex::new(stats);

    let (operation_result, monitor_result) = thread::scope(|scope| {
        let monitor = scope.spawn(|| -> Result<(), String> {
            let mut next = Instant::now() + interval;
            loop {
                thread::sleep(next.saturating_duration_since(Instant::now()));
                if stop.load(Ordering::Acquire) {
                    break;
                }
                let scan_started = Instant::now();
                let (disk, rss) = sample(paths).map_err(|error| error.to_string())?;
                let sampled_at = Instant::now();
                shared_stats
                    .lock()
                    .map_err(|error| error.to_string())?
                    .record(
                        sampled_at,
                        sampled_at.duration_since(scan_started),
                        disk,
                        rss,
                    );
                next = sampled_at + interval;
            }
            Ok(())
        });
        let result = operation();
        stop.store(true, Ordering::Release);
        let monitor_result = monitor
            .join()
            .map_err(|_| "resource sampler panicked".to_owned())
            .and_then(|result| result);
        (result, monitor_result)
    });

    monitor_result.map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
    let scan_started = Instant::now();
    let (disk, rss) = sample(paths)?;
    let sampled_at = Instant::now();
    let mut stats = shared_stats
        .into_inner()
        .map_err(|error| error.to_string())?;
    stats.record(
        sampled_at,
        sampled_at.duration_since(scan_started),
        disk,
        rss,
    );
    let value = operation_result?;
    Ok((value, stats.to_json()))
}

#[cfg(target_os = "linux")]
pub(crate) fn allocated_bytes(path: &Path) -> BoxResult<u64> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "allocated-block telemetry refuses symlink: {}",
            path.display()
        )
        .into());
    }
    let mut allocated = metadata
        .blocks()
        .checked_mul(512)
        .ok_or("allocated block byte count overflow")?;
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            allocated = allocated
                .checked_add(allocated_bytes(&entry?.path())?)
                .ok_or("allocated disk byte count overflow")?;
        }
    }
    Ok(allocated)
}

#[cfg(target_os = "linux")]
struct ProcessMemory {
    rss_bytes: u64,
    high_water_bytes: u64,
}

#[cfg(target_os = "linux")]
fn source_provenance() -> BoxResult<(String, Vec<String>, &'static str)> {
    let revision = std::env::var("VLITE_RESOURCE_SOURCE_REVISION");
    let dirty_paths = std::env::var("VLITE_RESOURCE_SOURCE_DIRTY_PATHS");
    match (revision, dirty_paths) {
        (Ok(revision), Ok(dirty_paths)) => {
            require(
                revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "VLITE_RESOURCE_SOURCE_REVISION must be a full Git commit ID",
            )?;
            Ok((
                revision,
                dirty_paths.lines().map(str::to_owned).collect(),
                "VLITE_RESOURCE_SOURCE_REVISION and VLITE_RESOURCE_SOURCE_DIRTY_PATHS",
            ))
        }
        (Err(std::env::VarError::NotPresent), Err(std::env::VarError::NotPresent)) => {
            let revision = command_output("git", &["rev-parse", "HEAD"])?;
            let dirty_paths = command_output("git", &["status", "--porcelain"])?
                .lines()
                .map(str::to_owned)
                .collect();
            Ok((revision, dirty_paths, "git rev-parse and git status"))
        }
        _ => Err(
            "resource source provenance requires both source environment variables, or neither"
                .into(),
        ),
    }
}

#[cfg(target_os = "linux")]
fn process_memory() -> BoxResult<ProcessMemory> {
    fn value(contents: &str, key: &str) -> BoxResult<u64> {
        let line = contents
            .lines()
            .find(|line| line.starts_with(key))
            .ok_or_else(|| format!("{key} missing from /proc/self/status"))?;
        let kib = line
            .split_whitespace()
            .nth(1)
            .ok_or_else(|| format!("{key} value missing from /proc/self/status"))?
            .parse::<u64>()?;
        kib.checked_mul(1024)
            .ok_or_else(|| format!("{key} byte count overflow").into())
    }

    let status = fs::read_to_string("/proc/self/status")?;
    Ok(ProcessMemory {
        rss_bytes: value(&status, "VmRSS:")?,
        high_water_bytes: value(&status, "VmHWM:")?,
    })
}

/// Reports the container limits the kernel exposes. A missing or unbounded limit is
/// recorded as `null` with its raw text and `limits_status` "incomplete"; it never
/// substitutes the requested platform tier.
#[cfg(target_os = "linux")]
pub(crate) fn cgroup_limits_from(read: impl Fn(&str) -> Option<String>) -> Value {
    // cgroup v1 reports "unlimited" as a page-aligned value near i64::MAX.
    const V1_UNLIMITED_FLOOR: u64 = 1 << 62;
    let finite = |text: &Option<String>| text.as_deref().and_then(|v| v.trim().parse::<u64>().ok());
    if let Some(memory) = read("/sys/fs/cgroup/memory.max") {
        let cpu = read("/sys/fs/cgroup/cpu.max");
        let mut fields = cpu.as_deref().unwrap_or("").split_whitespace();
        let quota = fields.next().and_then(|v| v.parse::<u64>().ok());
        let period = fields
            .next()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|p| *p > 0);
        let memory_limit = finite(&Some(memory.clone()));
        return limits_report(
            "cgroup v2 memory.max and cpu.max",
            memory_limit,
            quota,
            period,
            json!({"memory.max": memory.trim(), "cpu.max": cpu.as_deref().map(str::trim)}),
        );
    }
    let memory = read("/sys/fs/cgroup/memory/memory.limit_in_bytes");
    let quota = read("/sys/fs/cgroup/cpu/cpu.cfs_quota_us");
    let period = read("/sys/fs/cgroup/cpu/cpu.cfs_period_us");
    if memory.is_none() && quota.is_none() {
        return limits_report(
            "no readable cgroup v2 or v1 limit files",
            None,
            None,
            None,
            json!({}),
        );
    }
    limits_report(
        "cgroup v1 memory.limit_in_bytes and cpu.cfs_quota_us/cpu.cfs_period_us",
        finite(&memory).filter(|v| *v < V1_UNLIMITED_FLOOR),
        finite(&quota),
        finite(&period).filter(|p| *p > 0),
        json!({"memory.limit_in_bytes": memory.as_deref().map(str::trim),
            "cpu.cfs_quota_us": quota.as_deref().map(str::trim),
            "cpu.cfs_period_us": period.as_deref().map(str::trim)}),
    )
}

#[cfg(target_os = "linux")]
fn limits_report(
    source: &str,
    memory_limit_bytes: Option<u64>,
    quota: Option<u64>,
    period: Option<u64>,
    raw: Value,
) -> Value {
    let cores = quota.zip(period).map(|(q, p)| q as f64 / p as f64);
    let complete = memory_limit_bytes.is_some() && cores.is_some();
    json!({
        "limits_status": if complete { "finite" } else { "incomplete" },
        "memory_limit_bytes": memory_limit_bytes,
        "cpu_quota": quota.filter(|_| period.is_some()),
        "cpu_period": period.filter(|_| quota.is_some()),
        "cpu_limit_cores": cores,
        "source": source,
        "raw": raw
    })
}

#[cfg(target_os = "linux")]
fn cgroup_limits() -> Value {
    cgroup_limits_from(|path| fs::read_to_string(path).ok())
}

/// ACA injects these into job replicas; their presence labels, but does not attest, the host.
#[cfg(target_os = "linux")]
pub(crate) fn platform_from(lookup: impl Fn(&str) -> Option<String>) -> Value {
    match (
        lookup("CONTAINER_APP_JOB_NAME"),
        lookup("CONTAINER_APP_JOB_EXECUTION_NAME"),
    ) {
        (Some(job), Some(execution)) if !job.is_empty() && !execution.is_empty() => json!({
            "kind": "azure_container_apps_job",
            "job_name": job,
            "execution_name": execution,
            "replica_name": lookup("HOSTNAME"),
            "source": "ACA-injected CONTAINER_APP_JOB_NAME and CONTAINER_APP_JOB_EXECUTION_NAME; labelled, not attested"
        }),
        _ => json!({
            "kind": "local_linux",
            "source": "no ACA job variables present"
        }),
    }
}

#[cfg(target_os = "linux")]
fn execute_strategy(
    strategy: &str,
    root: &Path,
    baseline: &Baseline,
    delta: u64,
    paths: &[PathBuf],
) -> BoxResult<Value> {
    fs::create_dir_all(root)?;
    let (strategy_report, telemetry) = observe_phase(paths, RESOURCE_SAMPLE_INTERVAL, || {
        let rep = strategies::prepare_rep(root, baseline, delta, 0)?;
        let (reader, report) = match strategy {
            "b_incremental_apply" => strategies::incremental_apply(&rep)?,
            "c_baseline_plus_tail" => strategies::baseline_replacement(&rep)?,
            _ => return Err(format!("unknown resource strategy: {strategy}").into()),
        };
        require(
            report["verified"] == true,
            "resource strategy verification failed",
        )?;
        drop(reader);
        Ok(report)
    })?;
    Ok(json!({
        "strategy": strategy,
        "fixture_and_strategy": strategy_report,
        "telemetry": telemetry
    }))
}

#[cfg(target_os = "linux")]
pub(crate) fn run() -> BoxResult<()> {
    let started = Instant::now();
    let profile = resource_profile(environment_value)?;
    let platform = platform_from(|name| std::env::var(name).ok());
    let on_aca = platform["kind"] == "azure_container_apps_job";
    let limits = cgroup_limits();
    let directory = tempfile::tempdir()?;
    let (source_revision, dirty_paths, provenance_source) = source_provenance()?;
    let baseline_root = directory.path().join("baseline");
    fs::create_dir(&baseline_root)?;
    let (baseline, baseline_prepare_ns) = timed(|| {
        strategies::prepare_baseline(&baseline_root, profile.records, profile.dimensions)
    })?;
    let phase_paths = |case_root: &Path| vec![baseline_root.clone(), case_root.to_owned()];
    let mut paths = Map::new();
    for (name, path) in [
        ("b_incremental_apply", directory.path().join("path-b")),
        ("c_baseline_plus_tail", directory.path().join("path-c")),
    ] {
        let result = execute_strategy(name, &path, &baseline, profile.delta, &phase_paths(&path))?;
        paths.insert(name.to_owned(), result);
    }

    let compaction_root = directory.path().join("compaction-case");
    fs::create_dir(&compaction_root)?;
    let compaction_rep = strategies::prepare_rep(&compaction_root, &baseline, profile.delta, 0)?;
    let (compaction, compaction_telemetry) = observe_phase(
        &phase_paths(&compaction_root),
        RESOURCE_SAMPLE_INTERVAL,
        || strategies::measure_compaction(&compaction_rep),
    )?;
    require(
        compaction["output_verified"] == true,
        "compaction output verification failed",
    )?;

    let memory = process_memory()?;
    let mut unqualified = if on_aca {
        vec!["serving load, restart and re-seed behavior on the measured tier"]
    } else {
        vec![
            "ACA CPU and ephemeral-storage tier",
            "storage allocation behavior of the ACA filesystem",
        ]
    };
    unqualified.extend([
        "real tenant workload distribution and concurrency",
        "separate reader-only memory from harness/model memory",
        "ANN serving readiness during ReplicaFollower apply",
    ]);
    if limits["limits_status"] != "finite" {
        unqualified.push("container CPU or memory limit was not exposed as a finite cgroup value");
    }
    let input_sources: Map<String, Value> = profile
        .sources
        .iter()
        .map(|(field, source)| ((*field).to_owned(), json!(source)))
        .collect();
    let report = json!({
        "schema_version": 1,
        "scope": if on_aca {
            "ACA_JOB_RESOURCE_MEASUREMENT_EVIDENCE_NOT_SERVING_QUALIFICATION"
        } else {
            "LOCAL_LINUX_RESOURCE_DEVELOPMENT_EVIDENCE_NOT_ACA_QUALIFICATION"
        },
        "source": {
            "revision": source_revision,
            "provenance_source": provenance_source,
            "working_tree_dirty": !dirty_paths.is_empty(),
            "dirty_paths": dirty_paths,
            "package_version": env!("CARGO_PKG_VERSION"),
            "rustc": command_output("rustc", &["--version"])?,
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "build_profile": if cfg!(debug_assertions) { "debug" } else { "release" },
            "platform": platform,
            "host_resource_limits": limits,
            "temporary_filesystem_location": directory.path().display().to_string(),
            "temporary_filesystem_df_kp": command_output("df", &["-kP", &directory.path().display().to_string()])?,
            "host_note": std::env::var("VLITE_RESOURCE_HOST_NOTE")
                .unwrap_or_else(|_| "unrecorded local Linux host".to_owned()),
            "host_note_source": "VLITE_RESOURCE_HOST_NOTE, operator supplied"
        },
        "profile": {
            "profile_status": if profile.overridden() {
                "operator-selected inputs from the environment, not qualified limits or a representative tenant workload"
            } else {
                "selected development inputs, not qualified limits or a representative tenant workload"
            },
            "baseline_records": profile.records,
            "delta_mutations": profile.delta,
            "dimensions": profile.dimensions,
            "input_sources": input_sources,
            "payload_bytes": 64,
            "source_format": "V5, single-file, unencrypted, Full durability",
            "hnsw": {"m": strategies::INDEX.m, "ef_construction": strategies::INDEX.ef_construction,
                "seed": strategies::INDEX.seed, "metric": "squared_l2", "quantization": "none"},
            "ann_policy": "full HNSW rebuild after apply or baseline-plus-tail; no incremental index readiness",
            "sampling_interval_ms": RESOURCE_SAMPLE_INTERVAL.as_millis(),
            "workload": format!(
                "existing deterministic splitmix64 fixture; {} baseline records; tail of {} single-commit mutations cycling update/delete/insert",
                profile.records, profile.delta
            )
        },
        "verification": {
            "baseline_preparation_ns": baseline_prepare_ns,
            "path_b_strategy_verified": paths["b_incremental_apply"]["fixture_and_strategy"]["verified"],
            "path_c_strategy_verified": paths["c_baseline_plus_tail"]["fixture_and_strategy"]["verified"],
            "compaction_output_verified": compaction["output_verified"],
            "compaction_input_live_records": compaction["input_live_records"],
            "compaction_output_live_records": compaction["output_live_records"]
        },
        "process_memory": {
            "high_water_bytes": memory.high_water_bytes,
            "high_water_source": "Linux /proc/self/status VmHWM; kernel max RSS in KiB converted to bytes, process-wide since start, includes fixture model, harness and HNSW rebuilds",
            "rss_at_end_bytes": memory.rss_bytes
        },
        "paths": paths,
        "compaction": {
            "result": compaction,
            "telemetry": compaction_telemetry
        },
        "unqualified": unqualified,
        "total_elapsed_ns": nanos_since(started)
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    directory.close()?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn run() -> BoxResult<()> {
    Err("resource evidence requires Linux allocated-block and /proc telemetry".into())
}
