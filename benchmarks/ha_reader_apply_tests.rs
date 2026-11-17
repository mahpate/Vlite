use super::*;
use serde_json::json;
use vlite::{FormatVersion, ReplicaFollower};

#[test]
fn smoke_reports_contiguous_verified_progress_and_honest_cost_boundaries() {
    let directory = tempfile::tempdir().unwrap();
    let report = run_case(directory.path(), 8).unwrap();
    let samples = report["samples"].as_array().unwrap();
    assert_eq!(samples.len(), 3);
    let mut previous = report["baseline"]["sequence"].as_u64().unwrap();
    let lineage = report["baseline"]["lineage"].as_u64().unwrap();
    let baseline_bytes = report["baseline"]["bytes"].as_u64().unwrap();
    let mut staging_bytes = baseline_bytes;
    for sample in samples {
        assert_eq!(sample["base_sequence"], previous);
        assert_eq!(sample["last_sequence"], previous + 3);
        assert_eq!(sample["applied_sequence"], sample["last_sequence"]);
        assert_eq!(sample["reopened_sequence"], sample["last_sequence"]);
        assert_eq!(sample["lineage"], lineage);
        assert_eq!(sample["lag"], 0);
        assert_eq!(sample["data_equivalent"], true);
        assert_eq!(sample["rejections_leave_file_unchanged"], true);
        assert_eq!(
            sample["rejected_cases"],
            json!([
                "duplicate",
                "gap",
                "foreign_lineage",
                "corrupt_file",
                "truncated_file"
            ])
        );
        assert!(sample["apply_fsync_ns"].is_null());
        assert!(sample["apply_replay_ns"].is_null());
        for field in [
            "export_ns",
            "save_ns",
            "load_verify_ns",
            "apply_inclusive_ns",
            "read_only_reopen_ns",
            "detached_index_build_ns",
            "follower_reopen_ns",
            "oldest_ack_to_exact_visible_ns",
            "oldest_ack_to_detached_index_ready_ns",
        ] {
            assert!(sample[field].is_u64(), "missing observation: {field}");
        }
        assert!(
            sample["oldest_ack_to_detached_index_ready_ns"]
                .as_u64()
                .unwrap()
                >= sample["oldest_ack_to_exact_visible_ns"].as_u64().unwrap()
        );
        staging_bytes += sample["serialized_segment_bytes"].as_u64().unwrap();
        let files = &sample["files"];
        assert_eq!(files["retained_staging_bytes"], staging_bytes);
        assert_eq!(files["source_bytes"], files["follower_bytes"]);
        assert_eq!(
            files["sampled_total_bytes"].as_u64().unwrap(),
            files["source_bytes"].as_u64().unwrap()
                + files["follower_bytes"].as_u64().unwrap()
                + staging_bytes
        );
        previous += 3;
    }
    assert_eq!(report["final"]["byte_identical"], true);
    assert_eq!(report["final"]["empty_tail_verified"], true);
    assert_eq!(report["final"]["sequence"], previous);
}

#[test]
fn equivalence_checks_content_and_lineage_not_just_counts() {
    let directory = tempfile::tempdir().unwrap();
    let mut source =
        Database::create_with_format(directory.path().join("source"), 2, FormatVersion::V5)
            .unwrap();
    source.put(0, &[1.0, 2.0], b"old").unwrap();
    let segment = source.export_log(0, 1024).unwrap();
    let reader = ReplicaFollower::create(directory.path().join("reader"), &segment, None).unwrap();
    verify_equivalence(&source, reader.db(), 1).unwrap();
    source.put(0, &[1.0, 2.0], b"changed").unwrap();
    assert!(verify_equivalence(&source, reader.db(), 1).is_err());
    let mut foreign =
        Database::create_with_format(directory.path().join("foreign"), 2, FormatVersion::V5)
            .unwrap();
    foreign.put(0, &[1.0, 2.0], b"old").unwrap();
    assert!(verify_equivalence(&foreign, reader.db(), 1).is_err());
}

// ---------------------------------------------------------------- ITEM-002

#[cfg(target_os = "linux")]
use super::resource;
use super::strategies::{
    self, HighWater, Mode, Strategy, baseline_replacement, dir_bytes, head_sequence,
    incremental_apply, measure_compaction, measure_staleness, parse_mode, percentile, poll_peak,
    prepare_baseline, prepare_rep, snapshot_refresh, summarize,
};
#[path = "ha_reader_resource_tests.rs"]
#[cfg(target_os = "linux")]
mod resource_tests;

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

#[test]
fn mode_parsing_accepts_exactly_one_known_flag() {
    assert!(matches!(
        parse_mode(&args(&["--smoke"])).unwrap(),
        Mode::Smoke
    ));
    assert!(matches!(
        parse_mode(&args(&["--measure"])).unwrap(),
        Mode::Measure
    ));
    assert!(matches!(
        parse_mode(&args(&["--resource-evidence"])).unwrap(),
        Mode::ResourceEvidence
    ));
    for invalid in [
        &[][..],
        &["--measure", "--smoke"][..],
        &["--smoke", "--smoke"][..],
        &["--measure=1"][..],
        &["--resource-evidence", "--smoke"][..],
        &["measure"][..],
        &["--records", "10"][..],
    ] {
        assert!(parse_mode(&args(invalid)).is_err(), "{invalid:?}");
    }
}

#[test]
fn percentiles_use_nearest_rank_and_summary_reports_min_median_max() {
    let sorted = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100];
    assert_eq!(percentile(&sorted, 50.0), 50);
    assert_eq!(percentile(&sorted, 90.0), 90);
    assert_eq!(percentile(&sorted, 99.0), 100);
    assert_eq!(percentile(&sorted, 100.0), 100);
    assert_eq!(percentile(&[7], 99.0), 7);
    let summary = summarize(&[5, 1, 4, 2, 3]).unwrap();
    assert_eq!((summary.min, summary.median, summary.max), (1, 3, 5));
    // Even counts report the lower middle (nearest-rank p50), never an average.
    let even = summarize(&[4, 1, 3, 2]).unwrap();
    assert_eq!((even.min, even.median, even.max), (1, 2, 4));
    assert!(summarize(&[]).is_none());
    let report = strategies::summary_json(&[3, 1, 2]);
    assert_eq!(report["raw"], json!([3, 1, 2]));
    assert_eq!(report["min"], 1);
    assert_eq!(report["median"], 2);
    assert_eq!(report["max"], 3);
}

#[test]
fn peak_bytes_sampling_tracks_the_high_water_mark_and_phase() {
    let directory = tempfile::tempdir().unwrap();
    let reader = directory.path().join("reader");
    let ship = directory.path().join("ship");
    fs::create_dir_all(reader.join("nested")).unwrap();
    fs::create_dir(&ship).unwrap();
    assert_eq!(dir_bytes(&reader).unwrap(), 0);
    let mut high_water = HighWater::default();
    fs::write(reader.join("old"), [0u8; 100]).unwrap();
    high_water.sample("prepared", &reader, &ship).unwrap();
    fs::write(reader.join("nested").join("new"), [0u8; 150]).unwrap();
    fs::write(ship.join("snapshot"), [0u8; 999]).unwrap();
    high_water.sample("both", &reader, &ship).unwrap();
    fs::remove_file(reader.join("old")).unwrap();
    fs::remove_file(ship.join("snapshot")).unwrap();
    high_water.sample("swapped", &reader, &ship).unwrap();
    let report = high_water.to_json();
    assert_eq!(report["reader_peak_bytes"], 250);
    assert_eq!(report["reader_peak_phase"], "both");
    assert_eq!(report["ship_peak_bytes"], 999);
    assert_eq!(report["samples"].as_array().unwrap().len(), 3);
    assert_eq!(report["samples"][2]["reader_bytes"], 150);

    // The background poller sees a transient file that is gone afterwards.
    let transient = reader.join("transient");
    let (value, peak) = poll_peak(&reader, || {
        fs::write(&transient, [0u8; 4096])?;
        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::remove_file(&transient)?;
        Ok(7)
    })
    .unwrap();
    assert_eq!(value, 7);
    assert!(peak >= 4096 + 150, "peak {peak}");
    assert_eq!(dir_bytes(&reader).unwrap(), 150);
}

fn assert_reader_matches_source(source: &Path, reader: &Database, bound: u64) {
    let source = Database::open_read_only(source).unwrap();
    verify_equivalence(&source, reader, bound).unwrap();
    assert_eq!(
        head_sequence(&source).unwrap(),
        head_sequence(reader).unwrap()
    );
    // Updated record 0 carries its delta vector; the index must find it.
    let query = source.get(0).unwrap().unwrap().vector.clone();
    let exact = reader.search(&query, 1).unwrap();
    let approximate = reader
        .search_approximate_ready(&query, 1, strategies::QUERY_EF, strategies::INDEX)
        .unwrap();
    assert_eq!(exact[0].id, 0);
    assert_eq!(exact, approximate);
}

#[test]
fn each_strategy_converges_to_the_source_at_a_tiny_size() {
    let directory = tempfile::tempdir().unwrap();
    let baseline = prepare_baseline(directory.path(), 30, 8).unwrap();
    let rep = prepare_rep(directory.path(), &baseline, 6, 0).unwrap();
    assert!(rep.expected_sequence > baseline.sequence);
    let bound = rep.id_bound;

    let (reader, report) = snapshot_refresh(&rep).unwrap();
    assert_eq!(report["strategy"], Strategy::SnapshotRefresh.name());
    assert_eq!(report["verified"], true);
    assert_reader_matches_source(&rep.source, &reader, bound);
    for field in [
        "source_backup",
        "reader_restore",
        "open_read_only",
        "build_hnsw",
        "swap",
        "refresh_inclusive",
        "harness_verify",
    ] {
        assert!(report["ns"][field].is_u64(), "missing {field}");
    }
    // Old and new reader files coexist until the swap.
    assert!(
        report["bytes"]["reader_peak_bytes"].as_u64().unwrap()
            > report["bytes"]["reader_file_bytes"].as_u64().unwrap()
    );
    drop(reader);

    let (reader, report) = incremental_apply(&rep).unwrap();
    assert_eq!(report["strategy"], Strategy::IncrementalApply.name());
    assert_eq!(report["verified"], true);
    assert_reader_matches_source(&rep.source, &reader, bound);
    for field in [
        "export",
        "save",
        "load_verify",
        "apply_inclusive",
        "exact_visible_inclusive",
        "read_only_reopen",
        "build_hnsw",
        "index_ready_inclusive",
        "harness_verify",
    ] {
        assert!(report["ns"][field].is_u64(), "missing {field}");
    }
    assert!(report["ns"]["apply_fsync"].is_null());
    assert!(report["ns"]["apply_replay"].is_null());
    assert!(
        report["ns"]["index_ready_inclusive"].as_u64().unwrap()
            >= report["ns"]["exact_visible_inclusive"].as_u64().unwrap()
    );
    assert_eq!(report["in_place_index_maintenance"], "UNAVAILABLE");
    drop(reader);

    let (reader, report) = baseline_replacement(&rep).unwrap();
    assert_eq!(report["strategy"], Strategy::BaselineReplacement.name());
    assert_eq!(report["verified"], true);
    assert_eq!(report["tail_commits"], 6);
    assert_reader_matches_source(&rep.source, &reader, bound);
    for field in [
        "baseline_restore",
        "export",
        "save",
        "follower_open",
        "load_verify",
        "apply_inclusive",
        "read_only_reopen",
        "build_hnsw",
        "swap",
        "refresh_inclusive",
        "harness_verify",
    ] {
        assert!(report["ns"][field].is_u64(), "missing {field}");
    }
    drop(reader);

    let compaction = measure_compaction(&rep).unwrap();
    assert!(compaction["input_bytes"].as_u64().unwrap() > 0);
    assert!(compaction["output_bytes"].as_u64().unwrap() > 0);
    assert!(
        compaction["sampled_peak_bytes"].as_u64().unwrap()
            >= compaction["input_bytes"].as_u64().unwrap()
    );
    assert_eq!(compaction["output_verified"], true);
}

#[test]
fn a_tampered_reader_fails_strategy_verification() {
    let directory = tempfile::tempdir().unwrap();
    let baseline = prepare_baseline(directory.path(), 12, 4).unwrap();
    let mut rep = prepare_rep(directory.path(), &baseline, 3, 1).unwrap();
    // The model no longer matches the source, so every strategy must refuse.
    rep.corrupt_model_for_test();
    assert!(snapshot_refresh(&rep).is_err());
    assert!(incremental_apply(&rep).is_err());
    assert!(baseline_replacement(&rep).is_err());
}

#[test]
fn staleness_records_every_commit_from_ack_to_exact_visible() {
    let directory = tempfile::tempdir().unwrap();
    let report = measure_staleness(directory.path(), 20, 20, 300, 100).unwrap();
    let commits = report["commits"].as_u64().unwrap();
    assert!(commits > 0);
    assert_eq!(report["latency_ns"]["count"], commits);
    let p50 = report["latency_ns"]["p50"].as_u64().unwrap();
    let p90 = report["latency_ns"]["p90"].as_u64().unwrap();
    let p99 = report["latency_ns"]["p99"].as_u64().unwrap();
    let max = report["latency_ns"]["max"].as_u64().unwrap();
    assert!(p50 <= p90 && p90 <= p99 && p99 <= max);
    assert!(report["achieved_commits_per_second"].as_f64().unwrap() > 0.0);
    assert_eq!(report["final_verified"], true);
    assert!(report["ships"].as_u64().unwrap() >= 1);
}
