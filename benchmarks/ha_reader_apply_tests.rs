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
