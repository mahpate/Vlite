use serde_json::json;
use std::process::Command;

#[test]
fn gateway_measure_cli_is_opt_in_bounded_and_redacted() {
    let server = env!("CARGO_BIN_EXE_vlite-server");
    for args in [
        vec!["gateway-measure"],
        vec![
            "gateway-measure",
            "mint",
            "--gateway-config",
            "not-a-secret-file",
            "--token-file",
            "SENTINEL_SECRET",
            "--output",
            "unused",
        ],
    ] {
        let output = Command::new(server).args(args).output().unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("SENTINEL_SECRET"));
        let _: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    }
}

#[test]
fn bootstrap_creates_private_real_fixture_without_publishing_or_overwriting() {
    let temp = tempfile::tempdir().unwrap();
    let output_dir = temp.path().join("bootstrap");
    let config_file = temp.path().join("bootstrap.json");
    std::fs::write(
        &config_file,
        json!({
            "output_dir":output_dir,
            "control_state":{"target_file":temp.path().join("target.json"),"object":"control.json"},
            "writer_url":"http://127.0.0.1:8081","audience":"measurement","revocations":1
        })
        .to_string(),
    )
    .unwrap();
    let invoke = || {
        Command::new(env!("CARGO_BIN_EXE_vlite-server"))
            .args(["gateway-measure", "bootstrap", "--config"])
            .arg(&config_file)
            .output()
            .unwrap()
    };
    let output = invoke();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["control_state_published"], false);
    assert_eq!(String::from_utf8_lossy(&output.stdout).lines().count(), 1);
    for file in [
        "database.vlite",
        "admin",
        "gateway.json",
        "trust.json",
        "manifest.json",
        "load-0.token",
        "load-1.token",
        "suspension-0.token",
        "load.expected.json",
        "signing.key",
    ] {
        assert!(output_dir.join(file).exists(), "{file}");
    }
    let token = std::fs::read_to_string(output_dir.join("load-0.token")).unwrap();
    assert!(token.starts_with("vlk_"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains(&token));
    let fixture = vlite::Database::open(output_dir.join("database.vlite")).unwrap();
    assert_eq!(fixture.lineage(), report["lineage"].as_u64());
    fixture.close().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&output_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(output_dir.join("load-0.token"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    let second = invoke();
    assert!(!second.status.success());
    assert_eq!(
        std::fs::read_to_string(output_dir.join("load-0.token")).unwrap(),
        token
    );
}

#[test]
fn bootstrap_refuses_unknown_fields_and_bounds_without_effect_or_secret_echo() {
    let temp = tempfile::tempdir().unwrap();
    let output_dir = temp.path().join("bootstrap");
    let path = temp.path().join("invalid.json");
    let base = json!({"output_dir":output_dir,
        "control_state":{"target_file":temp.path().join("target.json"),"object":"control.json"},
        "writer_url":"http://127.0.0.1:8081","audience":"measurement"});
    let mut unknown = base.clone();
    unknown["secret_unrecognized_field"] = json!("SENTINEL_SECRET");
    let mut over_bound = base;
    over_bound["revocations"] = json!(101);
    for config in [unknown, over_bound] {
        std::fs::write(&path, config.to_string()).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_vlite-server"))
            .args(["gateway-measure", "bootstrap", "--config"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("SENTINEL_SECRET"));
        assert!(!output_dir.exists());
    }
}
