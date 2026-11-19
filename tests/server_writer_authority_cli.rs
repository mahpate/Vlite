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

    // A writer without durable commits has no archived base to restore from.
    refused(
        &[
            base.as_slice(),
            &["--serve-mode", "writer", "--durability", "none"],
        ]
        .concat(),
        "writer_authority_base_unverified",
    );
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

#[test]
fn archive_policy_startup_refusals_precede_listening_and_database_creation() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("never-created.vlite");
    let target = directory.path().join("target.json");
    let policy = directory.path().join("policy.json");
    let key = directory.path().join("emulator.key");
    std::fs::write(
        &key,
        "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==",
    )
    .unwrap();
    let target_value = serde_json::json!({
        "account":"devstoreaccount1", "container":"data",
        "coordination_container":"coordination", "prefix":"writer/",
        "endpoint":"http://127.0.0.1:10010/devstoreaccount1", "auth":"emulator_shared_key"
    });
    std::fs::write(&target, target_value.to_string()).unwrap();
    let base = [
        database.to_str().unwrap(),
        "--insecure-local",
        "--archive-policy-file",
        policy.to_str().unwrap(),
    ];
    refused(&base, "invalid_arguments");
    refused(
        &[base.as_slice(), &["--serve-mode", "writer"]].concat(),
        "invalid_arguments",
    );
    refused(
        &[
            base.as_slice(),
            &["--authority-target-file", target.to_str().unwrap()],
        ]
        .concat(),
        "invalid_arguments",
    );
    let args = [
        base.as_slice(),
        &[
            "--serve-mode",
            "writer",
            "--authority-target-file",
            target.to_str().unwrap(),
            "--authority-shared-key-file",
            key.to_str().unwrap(),
        ],
    ]
    .concat();
    let output = Command::new(env!("CARGO_BIN_EXE_vlite-server"))
        .args(&args)
        .env("VLITE_ADMIN_DIR", directory.path().join("admin"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("--archive-policy-file is for writers without VLITE_ADMIN_DIR")
    );
    refused(&args, "invalid_arguments");
    for contents in [
            "{".to_owned(),
            serde_json::json!({"target":target_value,"enabled":true,"unknown":true}).to_string(),
            serde_json::json!({"target":target_value,"enabled":true,"archive":{"enabled":false}}).to_string(),
            serde_json::json!({"target":target_value,"enabled":false,"archive":{"enabled":true}}).to_string(),
            serde_json::json!({"target":target_value,"enabled":true,"interval_secs":299,"archive":{"enabled":true}}).to_string(),
            " ".repeat(65537),
        ] {
            std::fs::write(&policy, contents).unwrap();
            refused(&args, "invalid_arguments");
        }
    let mut changed = target_value.clone();
    changed["prefix"] = serde_json::json!("other/");
    std::fs::write(
        &policy,
        serde_json::json!({
            "target":changed,"enabled":true,"archive":{"enabled":true}
        })
        .to_string(),
    )
    .unwrap();
    refused(&args, "writer_authority_target_mismatch");
    assert!(!database.exists());
    assert!(!directory.path().join("admin").exists());
}
