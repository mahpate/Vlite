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
