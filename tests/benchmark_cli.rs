use std::process::Command;

#[test]
fn benchmark_emits_verified_metrics_and_removes_temporary_database() {
    let directory = tempfile::tempdir().unwrap();
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_vlite-bench"))
            .args(["32", "4", "5", "8", "3", "42"])
            .env("VLITE_BENCH_DIR", directory.path())
            .output()
            .unwrap()
    };
    let first = run();
    assert!(first.status.success(), "{first:?}");
    let text = String::from_utf8(first.stdout).unwrap();
    assert!(text.starts_with("{\"schema_version\":1,"));
    assert!(text.trim_end().ends_with('}'));
    for field in [
        "\"records\":32",
        "\"format_version\":5",
        "\"dimensions\":4",
        "\"queries\":5",
        "\"batch_size\":8",
        "\"k\":3",
        "\"seed\":42",
        "\"recall_at_k\":1.000000",
        "\"query_p50_ms\":",
        "\"query_p95_ms\":",
        "\"ingest_vectors_per_second\":",
        "\"reopen_ms\":",
        "\"file_bytes\":",
        "\"vector_data_bytes\":512",
        "\"dataset_checksum\":",
    ] {
        assert!(text.contains(field), "missing {field}: {text}");
    }
    let checksum = |text: &str| {
        text.split("\"dataset_checksum\":")
            .nth(1)
            .unwrap()
            .split(',')
            .next()
            .unwrap()
            .to_owned()
    };
    let second = run();
    assert!(second.status.success());
    assert_eq!(
        checksum(&text),
        checksum(&String::from_utf8(second.stdout).unwrap())
    );
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn benchmark_rejects_invalid_or_unbounded_workloads_before_creating_files() {
    let directory = tempfile::tempdir().unwrap();
    for args in [
        vec!["0"],
        vec!["x"],
        vec!["32", "0"],
        vec!["32", "4", "0"],
        vec!["32", "4", "5", "0"],
        vec!["32", "4", "5", "8", "33"],
        vec!["1000000", "65536"],
        vec!["32", "4", "5", "4097"],
        vec!["32", "4", "5", "8", "3", "42", "extra"],
        vec!["--ann", "1", "100", "10"],
        vec!["--ann", "16", "15", "10"],
        vec!["--ann", "16", "100", "0"],
        vec!["--ann", "16", "100", "2", "32", "4", "5", "8", "3", "42"],
        vec!["--filter-every"],
        vec!["--filter-every", "0"],
        vec!["--filter-none", "--filter-every", "2"],
        vec!["--mixed-writes", "0", "--ann", "16", "100", "100"],
        vec!["--mixed-writes", "257", "--ann", "16", "100", "100"],
        vec!["--mixed-writes", "1"],
        vec!["--mixed-deletes", "1"],
        vec!["--rebuild-mode", "background"],
        vec!["--rebuild-mode", "maintenance"],
        vec!["--rebuild-mode", "invalid"],
        vec![
            "--query-interval-ms",
            "1001",
            "--mixed-writes",
            "1",
            "--ann",
            "16",
            "100",
            "100",
        ],
        vec!["--mixed-deletes", "0", "--ann", "16", "100", "100"],
        vec!["--mixed-deletes", "257", "--ann", "16", "100", "100"],
        vec![
            "--mixed-writes",
            "256",
            "--mixed-deletes",
            "1",
            "--ann",
            "16",
            "100",
            "100",
        ],
        vec!["--mixed-deletes", "33", "--ann", "16", "100", "100", "32"],
        vec![
            "--update-vectors",
            "missing",
            "--mixed-writes",
            "1",
            "--ann",
            "16",
            "100",
            "100",
        ],
        vec!["--mixed-writes", "1", "--mixed-writes", "1"],
        vec!["--vectors", "missing"],
        vec![
            "--vectors",
            "missing",
            "--query-vectors",
            "missing",
            "--mixed-writes",
            "1",
            "--ann",
            "16",
            "100",
            "100",
        ],
        vec!["--query-vectors", "missing"],
        vec!["--metric", "invalid"],
        vec!["--metric", "cosine", "--metric", "cosine"],
        vec!["--metric", "inner_product", "--ann", "16", "100", "100"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_vlite-bench"))
            .args(args)
            .env("VLITE_BENCH_DIR", directory.path())
            .output()
            .unwrap();
        assert!(!output.status.success(), "{output:?}");
        assert!(String::from_utf8(output.stderr).unwrap().contains("FAIL:"));
    }

    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn external_vectors_are_validated_and_used_without_publishing_paths() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("private-data.txt");
    let queries = directory.path().join("private-queries.txt");
    std::fs::write(&queries, "1 0\n0 1\n").unwrap();
    let oversized = "0".repeat(300);
    for (content, success) in [
        ("1 0\n0 1\n-1 0\n", true),
        ("1 0\n0 1\n", false),
        ("1 0\n0 1\n-1 0\n0 0\n", false),
        ("1\n0 1\n-1 0\n", false),
        ("1 0 2\n0 1\n-1 0\n", false),
        ("NaN 0\n0 1\n-1 0\n", false),
        ("inf 0\n0 1\n-1 0\n", false),
        ("garbage\n0 1\n-1 0\n", false),
        (oversized.as_str(), false),
    ] {
        std::fs::write(&data, content).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_vlite-bench"))
            .args(["--vectors"])
            .arg(&data)
            .arg("--query-vectors")
            .arg(&queries)
            .args(["3", "2", "2", "3", "2", "42"])
            .env("VLITE_BENCH_DIR", directory.path())
            .output()
            .unwrap();
        assert_eq!(output.status.success(), success, "{output:?}");
        if success {
            let text = String::from_utf8(output.stdout).unwrap();
            assert!(text.contains("\"input\":\"external-f32-text\""));
            assert!(text.contains("\"recall_at_k\":1.000000"));
            assert!(!text.contains("private-"));
        }
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
    }
}

#[cfg(feature = "server")]
#[test]
fn mixed_workload_reports_samples_growth_and_current_state_recall() {
    let directory = tempfile::tempdir().unwrap();
    // Cross the 256-ID threshold, then cycle IDs to exercise updates and rebuilds.
    let output = Command::new(env!("CARGO_BIN_EXE_vlite-bench"))
        .args([
            "--mixed-writes",
            "32",
            "--ann",
            "8",
            "32",
            "300",
            "300",
            "4",
            "12",
            "100",
            "5",
            "42",
        ])
        .env("VLITE_BENCH_DIR", directory.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["hnsw"]["mixed_writes_per_query"], 32);
    assert_eq!(value["hnsw"]["recall_at_k"], 1.0);
    assert!(value["final_file_bytes"].as_u64().unwrap() > value["file_bytes"].as_u64().unwrap());
    for (object, field) in [
        (&value, "query_samples_ms"),
        (&value["hnsw"], "query_samples_ms"),
        (&value["hnsw"], "write_batch_samples_ms"),
    ] {
        let samples = object[field].as_array().unwrap();
        assert_eq!(samples.len(), 12);
        assert!(
            samples
                .iter()
                .all(|v| v.as_f64().unwrap().is_finite() && v.as_f64().unwrap() >= 0.0)
        );
    }
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn filtered_benchmark_checks_ground_truth_and_reports_eligible_counts() {
    let directory = tempfile::tempdir().unwrap();
    for (every, eligible, returned) in [(1, 32, 10), (4, 8, 8), (100, 1, 1)] {
        let output = Command::new(env!("CARGO_BIN_EXE_vlite-bench"))
            .args([
                "--filter-every",
                &every.to_string(),
                "32",
                "4",
                "5",
                "8",
                "10",
                "42",
            ])
            .env("VLITE_BENCH_DIR", directory.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let text = String::from_utf8(output.stdout).unwrap();
        for field in [
            format!("\"filter_every\":{every}"),
            format!("\"eligible_records\":{eligible}"),
            format!("\"returned_per_query\":{returned}"),
            "\"recall_at_k\":1.000000".into(),
        ] {
            assert!(text.contains(&field), "{text}");
        }
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}

#[test]
fn approximate_benchmark_emits_measured_recall_and_build_cost() {
    let directory = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_vlite-bench"))
        .args(["--ann", "8", "32", "32", "64", "8", "10", "16", "5", "42"])
        .env("VLITE_BENCH_DIR", directory.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    for field in [
        "\"hnsw\":{",
        "\"m\":8",
        "\"ef_construction\":32",
        "\"ef_search\":32",
        "\"index_build_ms\":",
        "\"recall_at_k\":1.000000",
    ] {
        assert!(text.contains(field), "{text}");
    }

    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[cfg(feature = "server")]
#[test]
fn filtered_ann_reports_eligible_oracle_denominators_for_both_metrics() {
    let directory = tempfile::tempdir().unwrap();
    for metric in ["squared_l2", "cosine"] {
        for (flags, eligible, returned) in [
            (vec![], 200, 10),
            (vec!["--filter-every", "2"], 100, 10),
            (vec!["--filter-every", "100"], 2, 2),
            (vec!["--filter-none"], 0, 0),
        ] {
            let output = Command::new(env!("CARGO_BIN_EXE_vlite-bench"))
                .args(["--metric", metric])
                .args(&flags)
                .args(["--ann", "8", "32", "200", "200", "4", "5", "50", "10", "42"])
                .env("VLITE_BENCH_DIR", directory.path())
                .output()
                .unwrap();
            assert!(output.status.success(), "{metric} {flags:?}: {output:?}");
            let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["eligible_records"], eligible);
            assert_eq!(value["returned_per_query"], returned);
            let ann = &value["hnsw"];
            assert_eq!(ann["recall_denominator"], returned * 5);
            assert_eq!(ann["returned_counts"], serde_json::json!(vec![returned; 5]));
            assert_eq!(
                ann["eligible_records_samples"],
                serde_json::json!(vec![eligible; 5])
            );
            assert_eq!(
                ann["recall_denominator_samples"],
                serde_json::json!(vec![returned; 5])
            );
            // Empty exact sets use vacuous recall 1, explicitly labelled by denominator 0.
            assert_eq!(ann["recall_at_k"], 1.0);
            assert_eq!(value["recall_at_k"], 1.0);
            assert_eq!(ann["rebuild_query_numbers"], serde_json::json!([]));
        }
    }
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[cfg(feature = "server")]
#[test]
fn filtered_ann_mixed_updates_preserve_metadata_and_current_eligible_oracle() {
    for mode in ["lazy", "background", "maintenance"] {
        let output = Command::new(env!("CARGO_BIN_EXE_vlite-bench"))
            .args([
                "--metric",
                "cosine",
                "--mixed-writes",
                "16",
                "--mixed-deletes",
                "4",
                "--rebuild-mode",
                mode,
                "--filter-every",
                "4",
                "--ann",
                "8",
                "32",
                "100",
                "64",
                "4",
                "5",
                "16",
                "10",
                "42",
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "{mode}: {output:?}");
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            value["hnsw"]["eligible_records_samples"],
            serde_json::json!([15, 14, 13, 12, 12])
        );
        assert_eq!(value["hnsw"]["recall_denominator"], 50);
        assert_eq!(value["hnsw"]["recall_at_k"], 1.0);
    }
}

#[test]
fn metric_benchmarks_verify_exact_filtered_and_mixed_cosine_scores() {
    for metric in ["squared_l2", "cosine", "inner_product"] {
        for mode in [
            vec![],
            vec!["--filter-every", "4"],
            vec!["--mixed-writes", "16", "--ann", "8", "32", "100"],
        ] {
            if metric == "inner_product" && mode.contains(&"--ann") {
                continue;
            }
            let result = Command::new(env!("CARGO_BIN_EXE_vlite-bench"))
                .args(["--metric", metric])
                .args(mode)
                .args(["64", "8", "20", "16", "5", "42"])
                .output()
                .unwrap();
            assert!(result.status.success(), "{result:?}");
            let text = String::from_utf8(result.stdout).unwrap();
            assert!(text.contains(&format!("\"metric\":\"{metric}\"")));
            assert!(text.contains("\"recall_at_k\":1.000000"));
        }
    }
}

#[cfg(feature = "server")]
#[test]
fn filtered_cosine_benchmark_excludes_zero_norm_records_and_rejects_zero_queries() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("vectors.txt");
    let queries = directory.path().join("queries.txt");
    std::fs::write(&data, "1 0\n0 0\n").unwrap();
    std::fs::write(&queries, "1 0\n0 1\n").unwrap();
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_vlite-bench"))
            .args(["--metric", "cosine", "--vectors"])
            .arg(&data)
            .arg("--query-vectors")
            .arg(&queries)
            .args([
                "--filter-every",
                "1",
                "--ann",
                "2",
                "4",
                "4",
                "2",
                "2",
                "2",
                "2",
                "2",
                "42",
            ])
            .env("VLITE_BENCH_DIR", directory.path())
            .output()
            .unwrap()
    };
    let output = run();
    assert!(output.status.success(), "{output:?}");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["eligible_records"], 1);
    for mode in [&value, &value["hnsw"]] {
        assert_eq!(mode["recall_at_k"], 1.0);
        assert_eq!(mode["recall_denominator"], 2);
        assert_eq!(mode["returned_counts"], serde_json::json!([1, 1]));
    }
    assert_eq!(
        value["hnsw"]["eligible_records_samples"],
        serde_json::json!([1, 1])
    );
    assert_eq!(
        value["hnsw"]["recall_denominator_samples"],
        serde_json::json!([1, 1])
    );
    std::fs::write(&queries, "1 0\n0 0\n").unwrap();
    let invalid = run();
    assert!(!invalid.status.success());
    assert!(
        String::from_utf8(invalid.stderr)
            .unwrap()
            .contains("nonzero query")
    );
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
}

#[cfg(feature = "server")]
#[test]
fn external_updates_delete_and_reinsert_use_current_live_oracle() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("private-base.txt");
    let queries = directory.path().join("private-query.txt");
    let updates = directory.path().join("private-updates.txt");
    std::fs::write(&data, "1 0\n0 1\n-1 0\n0 -1\n").unwrap();
    std::fs::write(&queries, "1 0\n1 0\n1 0\n1 0\n").unwrap();
    std::fs::write(&updates, "0 1\n0 -1\n-1 0\n1 0\n0 1\n0 -1\n-1 0\n1 0\n").unwrap();
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_vlite-bench"))
            .args(["--metric", "cosine", "--vectors"])
            .arg(&data)
            .arg("--query-vectors")
            .arg(&queries)
            .arg("--update-vectors")
            .arg(&updates)
            .args([
                "--mixed-writes",
                "2",
                "--mixed-deletes",
                "1",
                "--ann",
                "2",
                "4",
                "10",
                "4",
                "2",
                "4",
                "4",
                "4",
                "42",
            ])
            .env("VLITE_BENCH_DIR", directory.path())
            .output()
            .unwrap()
    };
    let first = run();
    assert!(first.status.success(), "{first:?}");
    let value: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(
        value["hnsw"]["live_records_samples"],
        serde_json::json!([3, 2, 2, 2])
    );
    assert_eq!(
        value["hnsw"]["recall_samples"],
        serde_json::json!([1.0, 1.0, 1.0, 1.0])
    );
    assert_eq!(value["final_live_records"], 2);
    assert!(value["update_checksum"].is_string());
    assert!(
        !String::from_utf8(first.stdout)
            .unwrap()
            .contains("private-")
    );
    std::fs::write(&updates, "1 0\n1 0\n1 0\n1 0\n1 0\n1 0\n1 0\n1 0\n").unwrap();
    let second = run();
    assert!(second.status.success(), "{second:?}");
    let changed: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_ne!(value["update_checksum"], changed["update_checksum"]);
    for content in [
        "1 0\n",
        "1 0\n1 0\n1 0\n1 0\n1 0\n1 0\n1 0\n1 0\n1 0\n",
        "NaN 0\n1 0\n1 0\n1 0\n",
        "1\n1 0\n1 0\n1 0\n",
    ] {
        std::fs::write(&updates, content).unwrap();
        assert!(!run().status.success());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 3);
    }
}

#[cfg(feature = "server")]
#[test]
fn mixed_deletes_report_overflow_rebuilds_and_empty_live_sets() {
    let run = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_vlite-bench"))
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    let empty = run(&[
        "--mixed-deletes",
        "4",
        "--ann",
        "2",
        "4",
        "10",
        "4",
        "2",
        "2",
        "4",
        "4",
        "42",
    ]);
    assert_eq!(
        empty["hnsw"]["live_records_samples"],
        serde_json::json!([0, 0])
    );
    assert_eq!(empty["hnsw"]["recall_at_k"], 1.0);
    let overflow = run(&[
        "--mixed-writes",
        "24",
        "--mixed-deletes",
        "8",
        "--ann",
        "8",
        "32",
        "300",
        "300",
        "4",
        "12",
        "100",
        "5",
        "42",
    ]);
    assert_eq!(
        overflow["hnsw"]["rebuild_query_numbers"],
        serde_json::json!([9])
    );
    assert_eq!(overflow["hnsw"]["recall_at_k"], 1.0);
}

#[cfg(feature = "server")]
#[test]
fn background_benchmark_reports_availability_without_synchronous_fallback() {
    let output = Command::new(env!("CARGO_BIN_EXE_vlite-bench"))
        .args([
            "--metric",
            "cosine",
            "--mixed-writes",
            "24",
            "--mixed-deletes",
            "8",
            "--rebuild-mode",
            "background",
            "--query-interval-ms",
            "0",
            "--ann",
            "8",
            "32",
            "300",
            "300",
            "4",
            "20",
            "100",
            "5",
            "42",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let ann = &value["hnsw"];
    let background = &ann["background"];
    assert_eq!(ann["rebuild_query_numbers"], serde_json::json!([]));
    assert_eq!(ann["query_samples_ms"].as_array().unwrap().len(), 20);
    assert_eq!(background["attempted_queries"], 20);
    assert_eq!(
        background["request_samples_ms"].as_array().unwrap().len(),
        20
    );
    assert_eq!(
        background["maintenance_samples_ms"]
            .as_array()
            .unwrap()
            .len(),
        20
    );
    assert_eq!(
        background["served_queries"].as_u64().unwrap()
            + background["not_ready_query_numbers"]
                .as_array()
                .unwrap()
                .len() as u64,
        20
    );
    assert!(background["served_recall_at_k"].as_f64().unwrap() >= 0.99);
}

#[cfg(feature = "server")]
#[test]
fn maintenance_benchmark_starts_before_exhaustion_and_reports_capacity() {
    let output = Command::new(env!("CARGO_BIN_EXE_vlite-bench"))
        .args([
            "--metric",
            "cosine",
            "--mixed-writes",
            "16",
            "--mixed-deletes",
            "4",
            "--rebuild-mode",
            "maintenance",
            "--ann",
            "8",
            "32",
            "300",
            "300",
            "4",
            "20",
            "100",
            "5",
            "42",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let ann = &value["hnsw"];
    let background = &ann["background"];
    assert_eq!(ann["rebuild_query_numbers"], serde_json::json!([]));
    assert_eq!(background["start_query_numbers"][0], 8);
    let samples = background["policy"]["capacity_samples"].as_array().unwrap();
    assert_eq!(samples.len(), 20);
    assert_eq!(samples[0]["active_buffer"]["remaining_ids"], 236);
    assert_eq!(samples[6]["pressure"], "RebuildRecommended");
    assert!(samples[6]["rebuild"].is_null());
    assert!(samples[7]["rebuild"].is_object());
    assert_eq!(
        background["served_queries"].as_u64().unwrap()
            + background["not_ready_query_numbers"]
                .as_array()
                .unwrap()
                .len() as u64,
        20
    );
}
