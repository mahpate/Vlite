#![cfg(feature = "tools")]
mod cli_support;
use cli_support::{Fixture, success};
use std::process::Command;
#[test]
fn advisor_meets_target_and_reports_components() {
    let f = Fixture::new();
    let db = f.path("db");
    cli_support::database(&db);
    let before = std::fs::read(&db).unwrap();
    let out = success(
        Command::new(env!("CARGO_BIN_EXE_vlite-tune"))
            .args([
                "--db",
                db.to_str().unwrap(),
                "--m",
                "2,4",
                "--ef-construction",
                "16",
                "--ef-search",
                "20",
                "--k",
                "3",
                "--samples",
                "4",
                "--target-recall",
                "1",
                "--json",
            ])
            .output()
            .unwrap(),
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["candidates"].as_array().unwrap().len(), 2);
    assert_eq!(v["recommended"]["recall_at_k"], 1.0);
    assert!(v["recommended"]["memory_bytes_estimate"].as_u64().unwrap() > 0);
    assert_eq!(before, std::fs::read(db).unwrap());
}
#[test]
fn quality_compares_configs_and_files() {
    let f = Fixture::new();
    let db = f.path("db");
    let other = f.path("other");
    cli_support::database(&db);
    std::fs::copy(&db, &other).unwrap();
    for extra in [
        vec!["--other-m", "4"],
        vec!["--other-db", other.to_str().unwrap()],
    ] {
        let out = success(
            Command::new(env!("CARGO_BIN_EXE_vlite-quality"))
                .args([
                    "--db",
                    db.to_str().unwrap(),
                    "--k",
                    "3",
                    "--samples",
                    "4",
                    "--ef-search",
                    "20",
                    "--json",
                ])
                .args(extra)
                .output()
                .unwrap(),
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["primary"]["recall_at_k"], 1.0);
        assert_eq!(v["comparison"]["overlap_at_k"], 1.0);
        assert_eq!(v["comparison"]["rank_correlation"], 1.0);
    }
}
#[test]
fn rejects_invalid_sweep() {
    let f = Fixture::new();
    let db = f.path("db");
    cli_support::database(&db);
    assert!(
        !Command::new(env!("CARGO_BIN_EXE_vlite-tune"))
            .args(["--db", db.to_str().unwrap(), "--target-recall", "NaN"])
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[test]
fn human_output_is_labeled() {
    let f = Fixture::new();
    let db = f.path("db");
    cli_support::database(&db);
    let output = success(
        Command::new(env!("CARGO_BIN_EXE_vlite-quality"))
            .args(["--db", db.to_str().unwrap(), "--k", "3"])
            .output()
            .unwrap(),
    );
    assert!(output.contains("Primary: M="));
    assert!(output.contains("recall@3="));
}
