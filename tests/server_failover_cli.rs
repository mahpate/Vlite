#![cfg(feature = "server")]

use serde_json::Value;
use std::process::Command;

#[test]
fn failover_cli_refuses_invalid_arguments_without_stdout() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing-target.json");
    for (args, code) in [
        (vec![], "invalid_arguments"),
        (vec!["unknown"], "invalid_arguments"),
        (vec!["report"], "invalid_arguments"),
        (
            vec!["report", "--target-file", "missing", "--unknown", "secret"],
            "invalid_arguments",
        ),
        (
            vec!["report", "--target-file", missing.to_str().unwrap()],
            "invalid_target",
        ),
        (
            vec!["execute", "--target-file", "missing"],
            "invalid_arguments",
        ),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_vlite-server"))
            .arg("failover")
            .args(args)
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
        assert!(!stderr.contains("secret"));
        assert!(!missing.exists());
    }
}
