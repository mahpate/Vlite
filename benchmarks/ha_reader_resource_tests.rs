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
