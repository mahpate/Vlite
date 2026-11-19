use super::*;
use std::{thread, time::Duration};

#[cfg(target_os = "linux")]
#[test]
fn resource_observer_captures_transient_allocated_blocks_and_sampled_rss() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let transient = root.join("transient.bin");
    let (value, report) =
        resource::observe_phase(&[root.to_owned()], Duration::from_millis(5), || {
            fs::write(&transient, vec![0x5a; 32 * 1024])?;
            thread::sleep(Duration::from_millis(35));
            fs::remove_file(&transient)?;
            Ok(7)
        })
        .unwrap();

    assert_eq!(value, 7);
    assert!(report["allocated_blocks_peak_bytes"].as_u64().unwrap() >= 32 * 1024);
    assert_eq!(
        report["allocated_blocks_final_bytes"],
        resource::allocated_bytes(root).unwrap()
    );
    assert!(!transient.exists());
    assert!(report["sampled_process_rss_peak_bytes"].as_u64().unwrap() > 0);
    assert!(report["sample_count"].as_u64().unwrap() >= 3);
    assert_eq!(report["sampling_interval_ms"], 5);
    assert!(report["max_observed_sample_gap_ms"].as_f64().unwrap() >= 0.0);
    assert!(report["max_scan_duration_ms"].as_f64().unwrap() >= 0.0);
    let cpu = &report["cpu"];
    assert!(cpu["wall_elapsed_ms"].as_f64().unwrap() >= 35.0);
    assert!(cpu["process_cpu_ticks"].is_u64(), "{cpu}");
    assert!(cpu["cgroup_cpu_stat_delta"].is_object(), "{cpu}");
}

#[cfg(target_os = "linux")]
fn fake_files(entries: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
    move |path: &str| {
        entries
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, text)| (*text).to_owned())
    }
}

#[cfg(target_os = "linux")]
#[test]
fn cpu_list_and_counter_parsers_reject_malformed_text() {
    assert_eq!(resource::cpu_list_count("0-3,8,10-11\n"), Some(7));
    assert_eq!(resource::cpu_list_count("0"), Some(1));
    for malformed in ["", "3-1", "0-3,", "a", "0-x"] {
        assert_eq!(resource::cpu_list_count(malformed), None, "{malformed:?}");
    }

    let counters =
        resource::kernel_counters("usage_usec 42\nnr_periods 7\nbad line here\nname x\n");
    assert_eq!(counters.len(), 2);
    assert_eq!(counters["usage_usec"], 42);

    let stat = "1234 (my) prog) S 1 2 3 4 5 6 7 8 9 10 250 50 0 0 20 0";
    assert_eq!(resource::process_cpu_ticks(stat), Some(300));
    assert_eq!(resource::process_cpu_ticks("1234 (x) S 1"), None);
}

#[cfg(target_os = "linux")]
#[test]
fn cpu_phase_report_records_raw_deltas_and_never_invents_values() {
    let before = resource::cpu_snapshot_from(fake_files(&[
        (
            "/proc/self/stat",
            "9 (vlite) R 1 2 3 4 5 6 7 8 9 10 100 50 0 0",
        ),
        (
            "/sys/fs/cgroup/cpu.stat",
            "usage_usec 1000000\nnr_periods 10\nnr_throttled 2\nthrottled_usec 5000\n",
        ),
    ]));
    let after = resource::cpu_snapshot_from(fake_files(&[
        (
            "/proc/self/stat",
            "9 (vlite) R 1 2 3 4 5 6 7 8 9 10 300 150 0 0",
        ),
        (
            "/sys/fs/cgroup/cpu.stat",
            "usage_usec 3000000\nnr_periods 30\nnr_throttled 7\nthrottled_usec 15000\nnr_bursts 1\n",
        ),
        ("/sys/fs/cgroup/cpu/cpu.stat", "nr_periods 99\n"),
    ]));
    let two_seconds = Duration::from_secs(2);
    let report = resource::cpu_phase_report(&before, &after, two_seconds, Some(100));
    assert_eq!(report["process_cpu_ticks"], 300);
    assert_eq!(report["process_cpu_seconds"], 3.0);
    assert_eq!(report["mean_process_cores_used"], 1.5);
    assert_eq!(report["cgroup_cpu_stat_source"], "/sys/fs/cgroup/cpu.stat");
    assert_eq!(report["cgroup_cpu_stat_delta"]["usage_usec"], 2_000_000);
    assert_eq!(report["cgroup_cpu_stat_delta"]["throttled_usec"], 10_000);
    assert!(report["cgroup_cpu_stat_delta"]["nr_bursts"].is_null());
    assert_eq!(report["cgroup_throttled_period_fraction"], 0.25);
    assert_eq!(report["mean_cgroup_cores_used"], 1.0);

    let unknown_rate = resource::cpu_phase_report(&before, &after, Duration::ZERO, None);
    assert_eq!(unknown_rate["process_cpu_ticks"], 300);
    assert!(unknown_rate["process_cpu_seconds"].is_null());
    assert!(unknown_rate["mean_cgroup_cores_used"].is_null());

    let reset = resource::cpu_phase_report(&after, &before, two_seconds, Some(100));
    assert!(reset["process_cpu_ticks"].is_null());
    assert!(reset["cgroup_cpu_stat_delta"]["usage_usec"].is_null());
    assert!(reset["cgroup_throttled_period_fraction"].is_null());

    let v1 = resource::cpu_snapshot_from(fake_files(&[(
        "/sys/fs/cgroup/cpu/cpu.stat",
        "nr_periods 5\nnr_throttled 1\nthrottled_time 100\n",
    )]));
    let source_changed = resource::cpu_phase_report(&before, &v1, two_seconds, Some(100));
    assert!(source_changed["cgroup_cpu_stat_source"].is_null());
    assert_eq!(source_changed["cgroup_cpu_stat_delta"], json!({}));
    assert!(source_changed["process_cpu_ticks"].is_null());

    let v1_report = resource::cpu_phase_report(&v1, &v1, two_seconds, Some(100));
    assert_eq!(
        v1_report["cgroup_cpu_stat_source"],
        "/sys/fs/cgroup/cpu/cpu.stat"
    );
    assert_eq!(v1_report["cgroup_cpu_stat_delta"]["throttled_time"], 0);
    assert!(v1_report["mean_cgroup_cores_used"].is_null());
}

#[cfg(target_os = "linux")]
#[test]
fn cpu_signals_record_kernel_views_without_attesting_an_allocation() {
    let signals = resource::cpu_signals_from(
        fake_files(&[
            (
                "/proc/self/status",
                "Name:\tvlite\nCpus_allowed_list:\t0-3\n",
            ),
            ("/sys/devices/system/cpu/online", "0-15\n"),
            ("/proc/self/cgroup", "0::/\n"),
            ("/sys/fs/cgroup/cpu.weight", "100\n"),
        ]),
        Some(4),
    );
    assert_eq!(signals["available_parallelism"], 4);
    assert_eq!(signals["affinity_cpus"], 4);
    assert_eq!(signals["affinity_cpu_list"], "0-3");
    assert_eq!(signals["online_cpus"], 16);
    assert_eq!(signals["cgroup_v2_cpu_weight"], "100");
    assert!(signals["cgroup_v1_cpu_shares"].is_null());
    assert_eq!(signals["cgroup_v2_path_is_root"], true);

    let nested = resource::cpu_signals_from(
        fake_files(&[("/proc/self/cgroup", "0::/system.slice/vlite.scope\n")]),
        None,
    );
    assert_eq!(nested["cgroup_v2_path_is_root"], false);
    assert!(nested["available_parallelism"].is_null());

    let v1 = resource::cpu_signals_from(
        fake_files(&[
            ("/proc/self/cgroup", "12:cpu,cpuacct:/docker/abc\n"),
            ("/sys/fs/cgroup/cpu/cpu.shares", "1024\n"),
        ]),
        Some(2),
    );
    assert!(v1["cgroup_v2_path_is_root"].is_null());
    assert_eq!(v1["cgroup_v1_cpu_shares"], "1024");

    let none = resource::cpu_signals_from(fake_files(&[]), None);
    for key in [
        "affinity_cpus",
        "online_cpus",
        "process_cgroup",
        "cgroup_v2_path_is_root",
    ] {
        assert!(none[key].is_null(), "{key}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn allocated_block_telemetry_fails_for_a_missing_path() {
    let missing = std::env::temp_dir().join(format!(
        "vlite-ha-resource-missing-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    assert!(resource::allocated_bytes(&missing).is_err());
}

#[cfg(target_os = "linux")]
fn profile_from(pairs: &[(&str, &str)]) -> BoxResult<resource::ResourceProfile> {
    let values: std::collections::HashMap<String, String> = pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    resource::resource_profile(|name| Ok(values.get(name).cloned()))
}

#[cfg(target_os = "linux")]
#[test]
fn resource_profile_defaults_and_validated_overrides() {
    let defaults = profile_from(&[]).unwrap();
    assert_eq!(
        (defaults.records, defaults.delta, defaults.dimensions),
        (110_000, 300, 64)
    );
    assert!(
        defaults
            .sources
            .iter()
            .all(|(_, source)| *source == "default")
    );

    let chosen = profile_from(&[
        ("VLITE_RESOURCE_RECORDS", "400000"),
        ("VLITE_RESOURCE_DIMENSIONS", "32"),
    ])
    .unwrap();
    assert_eq!(
        (chosen.records, chosen.delta, chosen.dimensions),
        (400_000, 300, 32)
    );
    assert_eq!(
        chosen.sources,
        [
            ("baseline_records", "environment"),
            ("delta_mutations", "default"),
            ("dimensions", "environment"),
        ]
    );
}

#[cfg(target_os = "linux")]
#[test]
fn resource_profile_refuses_malformed_or_unsafe_inputs() {
    for (pairs, message) in [
        (vec![("VLITE_RESOURCE_RECORDS", "")], "plain decimal"),
        (vec![("VLITE_RESOURCE_RECORDS", "+5")], "plain decimal"),
        (vec![("VLITE_RESOURCE_DELTA", "1e3")], "plain decimal"),
        (
            vec![("VLITE_RESOURCE_DIMENSIONS", "0")],
            "between 1 and 1024",
        ),
        (
            vec![("VLITE_RESOURCE_DELTA", "100001")],
            "between 1 and 100000",
        ),
        (
            vec![("VLITE_RESOURCE_RECORDS", "99999999999999999999999")],
            "out of range",
        ),
        (
            vec![
                ("VLITE_RESOURCE_RECORDS", "10"),
                ("VLITE_RESOURCE_DELTA", "15"),
            ],
            "too large",
        ),
        (
            vec![
                ("VLITE_RESOURCE_RECORDS", "2000000"),
                ("VLITE_RESOURCE_DIMENSIONS", "17"),
            ],
            "must not exceed",
        ),
    ] {
        let error = profile_from(&pairs).unwrap_err().to_string();
        assert!(error.contains(message), "{pairs:?}: {error}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn cgroup_limits_report_unbounded_or_missing_values_without_inventing_them() {
    let files = |entries: &'static [(&'static str, &'static str)]| {
        move |path: &str| {
            entries
                .iter()
                .find(|(name, _)| *name == path)
                .map(|(_, text)| (*text).to_owned())
        }
    };
    let v2 = resource::cgroup_limits_from(files(&[
        ("/sys/fs/cgroup/memory.max", "2147483648\n"),
        ("/sys/fs/cgroup/cpu.max", "100000 100000\n"),
    ]));
    assert_eq!(v2["limits_status"], "finite");
    assert_eq!(v2["memory_limit_bytes"], 2_147_483_648u64);
    assert_eq!(v2["cpu_limit_cores"], 1.0);

    let unbounded = resource::cgroup_limits_from(files(&[
        ("/sys/fs/cgroup/memory.max", "max\n"),
        ("/sys/fs/cgroup/cpu.max", "max 100000\n"),
    ]));
    assert_eq!(unbounded["limits_status"], "incomplete");
    assert!(unbounded["memory_limit_bytes"].is_null());
    assert!(unbounded["cpu_limit_cores"].is_null());
    assert_eq!(unbounded["raw"]["memory.max"], "max");

    let v1 = resource::cgroup_limits_from(files(&[
        (
            "/sys/fs/cgroup/memory/memory.limit_in_bytes",
            "9223372036854771712\n",
        ),
        ("/sys/fs/cgroup/cpu/cpu.cfs_quota_us", "50000\n"),
        ("/sys/fs/cgroup/cpu/cpu.cfs_period_us", "100000\n"),
    ]));
    assert_eq!(v1["limits_status"], "incomplete");
    assert!(v1["memory_limit_bytes"].is_null());
    assert_eq!(v1["cpu_limit_cores"], 0.5);

    let none = resource::cgroup_limits_from(files(&[]));
    assert_eq!(none["limits_status"], "incomplete");
}

#[cfg(target_os = "linux")]
#[test]
fn platform_label_requires_both_aca_job_variables() {
    let aca = resource::platform_from(|name| match name {
        "CONTAINER_APP_JOB_NAME" => Some("vlite-meas".to_owned()),
        "CONTAINER_APP_JOB_EXECUTION_NAME" => Some("vlite-meas-abc12".to_owned()),
        _ => None,
    });
    assert_eq!(aca["kind"], "azure_container_apps_job");
    assert_eq!(aca["execution_name"], "vlite-meas-abc12");

    let partial = resource::platform_from(|name| {
        (name == "CONTAINER_APP_JOB_NAME").then(|| "vlite-meas".to_owned())
    });
    assert_eq!(partial["kind"], "local_linux");
}
