#![cfg(feature = "server")]

use std::{path::Path, process::Command};

fn refused(served: &Path, config: &Path, options: &[&str], message: &str) {
    let output = Command::new(env!("CARGO_BIN_EXE_vlite-server"))
        .arg(served)
        .arg("--reader-apply-file")
        .arg(config)
        .args(["--insecure-local", "--bind", "127.0.0.1:0"])
        .args(options)
        .env_remove("VLITE_SERVE_MODE")
        .env_remove("VLITE_AUTH_TOKEN")
        .env_remove("VLITE_AUTH_TOKEN_FILE")
        .env_remove("VLITE_READ_ONLY_TOKEN")
        .env_remove("VLITE_READ_ONLY_TOKEN_FILE")
        .env_remove("VLITE_ENCRYPTION_KEY")
        .env_remove("VLITE_ENCRYPTION_KEY_HEX")
        .env_remove("VLITE_ADMIN_DIR")
        .env_remove("VLITE_PROVIDERS_FILE")
        .env_remove("VLITE_MEMORY_BINDINGS_FILE")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(message), "{stderr}");
    assert!(!served.exists());
}

#[test]
fn reader_apply_requires_reader_mode() {
    let dir = tempfile::tempdir().unwrap();
    refused(
        &dir.path().join("database"),
        &dir.path().join("missing.json"),
        &[],
        "--reader-apply-file needs --serve-mode reader",
    );
}

#[test]
fn reader_apply_refuses_encryption_before_reading_files() {
    let dir = tempfile::tempdir().unwrap();
    refused(
        &dir.path().join("database"),
        &dir.path().join("missing.json"),
        &[
            "--serve-mode",
            "reader",
            "--encryption-key-file",
            "missing-key",
        ],
        "a reader that applies the archive cannot open an encrypted database",
    );
}

#[test]
fn reader_apply_refuses_invalid_configuration_before_network_access() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("reader.json");
    std::fs::write(
        &config,
        br#"{"target_file":"must-not-be-opened","secret":"never-echo"}"#,
    )
    .unwrap();
    refused(
        &dir.path().join("database"),
        &config,
        &["--serve-mode", "reader"],
        "reader_apply_invalid:",
    );
}

#[test]
fn reader_apply_refuses_wrong_served_path_before_network_access() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("reader.json");
    std::fs::write(
        &config,
        serde_json::json!({
            "target_file":"must-not-be-opened", "follower_dir":dir.path(),
            "capacity_bytes":1073741824u64, "staleness_bound_ms":30000, "poll_interval_ms":1000
        })
        .to_string(),
    )
    .unwrap();
    refused(
        &dir.path().join("database"),
        &config,
        &["--serve-mode", "reader"],
        "reader_apply_invalid: served path must be slot A",
    );
}
