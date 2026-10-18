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
        "\"format_version\":3",
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
        vec!["--filter-every", "2", "--ann", "16", "100", "100"],
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
