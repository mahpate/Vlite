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
        "\"format_version\":2",
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
