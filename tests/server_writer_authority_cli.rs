#![cfg(feature = "server")]

use serde_json::Value;
use std::process::Command;

fn refused(args: &[&str], code: &str) {
    let output = Command::new(env!("CARGO_BIN_EXE_vlite-server"))
        .args(args)
        .env_remove("VLITE_SERVE_MODE")
        .env_remove("VLITE_ADMIN_DIR")
        .env_remove("VLITE_AUTH_TOKEN")
        .env_remove("VLITE_AUTH_TOKEN_FILE")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    let error: Value = serde_json::from_str(stderr.lines().last().unwrap()).unwrap();
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .starts_with(&format!("{code}: ")),
        "{stderr}"
    );
    assert!(!stderr.contains("do-not-echo-this-secret"), "{stderr}");
}

#[test]
fn authority_cli_refuses_arguments_and_invalid_targets_without_side_effects() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("missing.json");
    let target = target.to_str().unwrap();
    for args in [
        vec!["authority"],
        vec!["authority", "unknown"],
        vec!["authority", "init"],
        vec!["authority", "show"],
        vec!["authority", "show", "--lineage", "do-not-echo-this-secret"],
        vec![
            "authority",
            "show",
            "--target-file",
            target,
            "--target-file",
            target,
        ],
        vec![
            "authority",
            "init",
            "--target-file",
            target,
            "--installation-id",
            "id",
        ],
    ] {
        refused(&args, "invalid_arguments");
    }
    refused(
        &["authority", "show", "--target-file", target],
        "invalid_target",
    );
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn startup_authority_options_are_validated_before_creating_a_database() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("never-created.vlite");
    let missing = directory.path().join("missing-target.json");
    let base = [
        database.to_str().unwrap(),
        "--insecure-local",
        "--create-if-missing",
        "1",
        "--authority-target-file",
        missing.to_str().unwrap(),
    ];
    for extra in [
        vec![],
        vec!["--serve-mode", "reader"],
        vec!["--serve-mode", "writer", "--durability", "none"],
        vec!["--serve-mode", "writer", "--grpc-bind", "127.0.0.1:0"],
        vec!["--serve-mode", "writer", "--authority-recheck-ms", "499"],
        vec!["--serve-mode", "writer", "--authority-recheck-ms", "60001"],
        vec![
            "--serve-mode",
            "writer",
            "--authority-recheck-ms",
            "do-not-echo-this-secret",
        ],
    ] {
        refused(
            &[base.as_slice(), extra.as_slice()].concat(),
            "invalid_arguments",
        );
    }
    refused(
        &[base.as_slice(), &["--serve-mode", "writer"]].concat(),
        "invalid_target",
    );
    for extra in [
        vec!["--authority-recheck-ms", "500"],
        vec!["--authority-shared-key-file", "do-not-echo-this-secret"],
        vec!["--authority-target-file"],
    ] {
        refused(
            &[&base[..4], &["--serve-mode", "writer"], extra.as_slice()].concat(),
            "invalid_arguments",
        );
    }
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}
