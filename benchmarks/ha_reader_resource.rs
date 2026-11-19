//! Separate, bounded local resource probe for the owner-approved reader paths.

#[cfg(target_os = "linux")]
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(target_os = "linux")]
use std::{
    collections::BTreeMap,
    sync::{
        Mutex, OnceLock,
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
    let cpu_before = cpu_snapshot();
    let cpu_started = Instant::now();

    let (operation_result, monitor_result, cpu_after) = thread::scope(|scope| {
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
        let cpu_after = (cpu_snapshot(), cpu_started.elapsed());
        stop.store(true, Ordering::Release);
        let monitor_result = monitor
            .join()
            .map_err(|_| "resource sampler panicked".to_owned())
            .and_then(|result| result);
        (result, monitor_result, cpu_after)
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
    let mut telemetry = stats.to_json();
    telemetry["cpu"] = cpu_phase_report(
        &cpu_before,
        &cpu_after.0,
        cpu_after.1,
        clock_ticks_per_second(),
    );
    Ok((value, telemetry))
}

#[cfg(target_os = "linux")]
const CGROUP_V2_CPU_STAT: &str = "/sys/fs/cgroup/cpu.stat";
#[cfg(target_os = "linux")]
const CGROUP_V1_CPU_STAT: &str = "/sys/fs/cgroup/cpu/cpu.stat";

/// Counts the CPUs in a kernel list such as "0-3,8,10-11"; malformed text yields `None`.
#[cfg(target_os = "linux")]
pub(crate) fn cpu_list_count(list: &str) -> Option<u64> {
    let list = list.trim();
    if list.is_empty() {
        return None;
    }
    let mut count = 0u64;
    for part in list.split(',') {
        let (low, high) = match part.split_once('-') {
            Some((low, high)) => (low.parse::<u64>().ok()?, high.parse::<u64>().ok()?),
            None => {
                let cpu = part.parse::<u64>().ok()?;
                (cpu, cpu)
            }
        };
        if high < low {
            return None;
        }
        count = count.checked_add(high - low + 1)?;
    }
    Some(count)
}

/// Parses "name value" counter lines (cgroup `cpu.stat`); other lines are skipped.
#[cfg(target_os = "linux")]
pub(crate) fn kernel_counters(text: &str) -> BTreeMap<String, u64> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let (name, value) = (fields.next()?, fields.next()?);
            if fields.next().is_some() {
                return None;
            }
            Some((name.to_owned(), value.parse().ok()?))
        })
        .collect()
}

/// Sums utime and stime (fields 14 and 15 of proc(5) stat) for all threads, in clock ticks.
#[cfg(target_os = "linux")]
pub(crate) fn process_cpu_ticks(stat: &str) -> Option<u64> {
    // The command name may contain spaces or parentheses; fields resume after the last ')'
    // at field 3, so utime and stime are at offsets 11 and 12.
    let mut fields = stat.rsplit_once(')')?.1.split_whitespace().skip(11);
    let utime = fields.next()?.parse::<u64>().ok()?;
    let stime = fields.next()?.parse::<u64>().ok()?;
    utime.checked_add(stime)
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug, Default)]
pub(crate) struct CpuSnapshot {
    process_ticks: Option<u64>,
    cgroup_source: Option<&'static str>,
    cgroup: BTreeMap<String, u64>,
}

#[cfg(target_os = "linux")]
pub(crate) fn cpu_snapshot_from(read: impl Fn(&str) -> Option<String>) -> CpuSnapshot {
    let (cgroup_source, cgroup) = [CGROUP_V2_CPU_STAT, CGROUP_V1_CPU_STAT]
        .into_iter()
        .find_map(|path| read(path).map(|text| (Some(path), kernel_counters(&text))))
        .unwrap_or_default();
    CpuSnapshot {
        process_ticks: read("/proc/self/stat")
            .as_deref()
            .and_then(process_cpu_ticks),
        cgroup_source,
        cgroup,
    }
}

#[cfg(target_os = "linux")]
fn cpu_snapshot() -> CpuSnapshot {
    cpu_snapshot_from(|path| fs::read_to_string(path).ok())
}

#[cfg(target_os = "linux")]
fn clock_ticks_per_second() -> Option<u64> {
    static TICKS: OnceLock<Option<u64>> = OnceLock::new();
    *TICKS.get_or_init(|| {
        command_output("getconf", &["CLK_TCK"])
            .ok()?
            .parse()
            .ok()
            .filter(|ticks| *ticks > 0)
    })
}

/// Reports one phase's raw CPU deltas. Missing counters, a changed `cpu.stat` source or a
/// decreasing counter yield `null`, never zero.
#[cfg(target_os = "linux")]
pub(crate) fn cpu_phase_report(
    before: &CpuSnapshot,
    after: &CpuSnapshot,
    elapsed: Duration,
    clock_ticks_per_second: Option<u64>,
) -> Value {
    let elapsed_seconds = elapsed.as_secs_f64();
    let per_wall_second = |amount: f64| (elapsed_seconds > 0.0).then(|| amount / elapsed_seconds);
    let process_ticks = before
        .process_ticks
        .zip(after.process_ticks)
        .and_then(|(before, after)| after.checked_sub(before));
    let process_seconds = process_ticks
        .zip(clock_ticks_per_second.filter(|ticks| *ticks > 0))
        .map(|(ticks, per_second)| ticks as f64 / per_second as f64);
    let same_source = before.cgroup_source.is_some() && before.cgroup_source == after.cgroup_source;
    let delta: BTreeMap<&str, Option<u64>> = if same_source {
        after
            .cgroup
            .iter()
            .map(|(name, after)| {
                let before = before.cgroup.get(name);
                (
                    name.as_str(),
                    before.and_then(|before| after.checked_sub(*before)),
                )
            })
            .collect()
    } else {
        BTreeMap::new()
    };
    let counter = |name: &str| delta.get(name).copied().flatten();
    let throttled_fraction = counter("nr_throttled")
        .zip(counter("nr_periods").filter(|periods| *periods > 0))
        .map(|(throttled, periods)| throttled as f64 / periods as f64);
    json!({
        "wall_elapsed_ms": elapsed_seconds * 1_000.0,
        "process_cpu_ticks": process_ticks,
        "clock_ticks_per_second": clock_ticks_per_second,
        "process_cpu_seconds": process_seconds,
        "mean_process_cores_used": process_seconds.and_then(per_wall_second),
        "process_cpu_source": "Linux /proc/self/stat utime+stime of every process thread, including the resource sampler; clock-tick resolution; tick rate from getconf CLK_TCK",
        "cgroup_cpu_stat_source": if same_source { before.cgroup_source } else { None },
        "cgroup_cpu_stat_delta": delta,
        "cgroup_throttled_period_fraction": throttled_fraction,
        "mean_cgroup_cores_used": counter("usage_usec")
            .and_then(|usec| per_wall_second(usec as f64 / 1_000_000.0)),
        "interpretation": "Raw kernel deltas for this phase. Missing or decreasing counters stay null. They do not establish the requested tier's vCPU allocation."
    })
}

/// Records the kernel's view of usable CPUs. These label, but do not attest, the effective vCPU.
#[cfg(target_os = "linux")]
pub(crate) fn cpu_signals_from(
    read: impl Fn(&str) -> Option<String>,
    available_parallelism: Option<usize>,
) -> Value {
    let trimmed = |path: &str| read(path).map(|text| text.trim().to_owned());
    let affinity = read("/proc/self/status").and_then(|status| {
        status
            .lines()
            .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
            .map(|list| list.trim().to_owned())
    });
    let online = trimmed("/sys/devices/system/cpu/online");
    let membership = trimmed("/proc/self/cgroup");
    let v2_path = membership
        .as_deref()
        .and_then(|text| text.lines().find_map(|line| line.strip_prefix("0::")));
    json!({
        "available_parallelism": available_parallelism,
        "available_parallelism_source": "Rust std::thread::available_parallelism; considers the affinity mask and cgroup CPU quota on Linux",
        "affinity_cpus": affinity.as_deref().and_then(cpu_list_count),
        "affinity_cpu_list": affinity,
        "online_cpus": online.as_deref().and_then(cpu_list_count),
        "online_cpu_list": online,
        "cgroup_v2_cpu_weight": trimmed("/sys/fs/cgroup/cpu.weight"),
        "cgroup_v1_cpu_shares": trimmed("/sys/fs/cgroup/cpu/cpu.shares"),
        "process_cgroup": membership,
        "cgroup_v2_path_is_root": v2_path.map(|path| path == "/"),
        "source": "/proc/self/status Cpus_allowed_list, /sys/devices/system/cpu/online, /proc/self/cgroup, and cgroup v2 cpu.weight or v1 cpu.shares",
        "interpretation": "Kernel observations only; they label, but do not attest, the effective vCPU allocation. When cgroup_v2_path_is_root is false, /sys/fs/cgroup files may describe an ancestor cgroup."
    })
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
    let cpu_signals = cpu_signals_from(
        |path| fs::read_to_string(path).ok(),
        thread::available_parallelism().ok().map(usize::from),
    );
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
        vec![
            "serving load, restart and re-seed behavior on the measured tier",
            "effective vCPU allocation of the requested tier; recorded CPU signals are kernel observations, not an attestation",
        ]
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
        "schema_version": 2,
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
            "effective_cpu_signals": cpu_signals,
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
