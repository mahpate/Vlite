use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn smoke(action: &str, path: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_vlite-smoke"))
        .arg(action)
        .arg(path)
        .output()
        .unwrap()
}

#[test]
fn smoke_initializes_then_verifies_across_invocations_without_changing_existing_data() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("persistent smoke.vlite");
    let first = smoke("run", &path);
    assert!(first.status.success(), "{first:?}");
    let stdout = String::from_utf8(first.stdout).unwrap();
    assert!(stdout.contains("PASS init:"));
    assert!(stdout.contains("PASS verify:"));
    assert!(stdout.contains("PASS end-to-end:"));
    let bytes = fs::read(&path).unwrap();
    for action in ["verify", "run"] {
        let output = smoke(action, &path);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
    let duplicate = smoke("init", &path);
    assert!(!duplicate.status.success());
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn smoke_fails_on_corruption_missing_files_and_unexpected_existing_records() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("invalid.vlite");
    assert!(!smoke("verify", &path).status.success());
    assert!(!path.exists());
    fs::write(&path, b"do not overwrite").unwrap();
    let output = smoke("run", &path);
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr).unwrap().contains("FAIL:"));
    assert_eq!(fs::read(&path).unwrap(), b"do not overwrite");
    let other = directory.path().join("unrelated.vlite");
    vlite::Database::create(&other, 3).unwrap().close().unwrap();
    let before = fs::read(&other).unwrap();
    assert!(!smoke("run", &other).status.success());
    assert_eq!(fs::read(&other).unwrap(), before);
}

#[test]
fn smoke_rejects_invalid_arguments() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("unused.vlite");
    assert!(!smoke("unknown", &path).status.success());
    let output = Command::new(env!("CARGO_BIN_EXE_vlite-smoke"))
        .args(["run", "unused.vlite", "extra"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr).unwrap().contains("usage:"));
    assert!(!path.exists());
}
