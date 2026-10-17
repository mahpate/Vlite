use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::Command;
use tempfile::tempdir;
use vlite::{Database, Error, maintenance};

fn fixture(path: &Path) {
    let mut db = Database::create(path, 2).unwrap();
    db.put(9, &[1.0, -0.0], b"old").unwrap();
    db.put(4, &[-2.0, 3.0], b"deleted").unwrap();
    db.put(9, &[f32::MAX, -0.0], b"latest\0payload").unwrap();
    db.delete(4).unwrap();
    db.put(1, &[1.0, 2.0], b"first").unwrap();
    db.close().unwrap();
}

#[test]
fn inspection_validates_and_reports_without_changing_bytes() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("source.vlite");
    fixture(&path);
    let original = fs::read(&path).unwrap();
    let report = maintenance::inspect(&path).unwrap();
    assert_eq!(report.dimensions, 2);
    assert_eq!(report.live_records, 2);
    assert_eq!(report.commits, 5);
    assert_eq!(report.file_bytes, original.len() as u64);
    assert_eq!(report.valid_bytes, original.len() as u64);
    assert_eq!(report.incomplete_tail_bytes, 0);
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"VREC")
        .unwrap();
    let report = maintenance::inspect(&path).unwrap();
    assert_eq!(report.incomplete_tail_bytes, 4);
    assert_eq!(report.valid_bytes, original.len() as u64);
    assert_eq!(report.file_bytes, original.len() as u64 + 4);
    assert_eq!(&fs::read(&path).unwrap()[..original.len()], original);
}

#[test]
fn backup_restore_and_compaction_preserve_data_and_leave_source_untouched() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source.vlite");
    let backup = directory.path().join("backup.vlite");
    let restored = directory.path().join("restored.vlite");
    let compacted = directory.path().join("compact.vlite");
    fixture(&source);
    let original = fs::read(&source).unwrap();
    let backed_up = maintenance::backup(&source, &backup).unwrap();
    assert_eq!(backed_up.commits, 5);
    maintenance::restore(&backup, &restored).unwrap();
    assert_eq!(fs::read(&backup).unwrap(), original);
    assert_eq!(fs::read(&restored).unwrap(), original);
    let report = maintenance::compact(&source, &compacted).unwrap();
    assert_eq!(report.source.commits, 5);
    assert_eq!(report.output.commits, 2);
    assert_eq!(report.output.live_records, 2);
    assert!(report.output.file_bytes < report.source.file_bytes);
    assert_eq!(fs::read(&source).unwrap(), original);
    let source_db = Database::open(&source).unwrap();
    let mut compact_db = Database::open(&compacted).unwrap();
    for id in [1, 4, 9] {
        assert_eq!(source_db.get(id).unwrap(), compact_db.get(id).unwrap());
    }
    assert_eq!(
        compact_db.get(9).unwrap().unwrap().vector[1].to_bits(),
        (-0.0f32).to_bits()
    );
    for query in [[0.0, 0.0], [f32::MAX, -0.0]] {
        assert_eq!(
            source_db.search(&query, 10).unwrap(),
            compact_db.search(&query, 10).unwrap()
        );
    }
    compact_db.put(2, &[4.0, 5.0], b"new").unwrap();
    compact_db.delete(9).unwrap();
    compact_db.close().unwrap();
    let reopened = Database::open(&compacted).unwrap();
    assert_eq!(reopened.len(), 2);
    assert!(reopened.get(9).unwrap().is_none());
    assert_eq!(reopened.get(2).unwrap().unwrap().payload, b"new");
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 4);
}

#[test]
fn all_copy_operations_reject_incomplete_and_corrupt_sources_without_output() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source.vlite");
    let output = directory.path().join("output.vlite");
    fixture(&source);
    let original = fs::read(&source).unwrap();
    let mut incomplete = original.clone();
    incomplete.extend_from_slice(b"VREC");
    let mut corrupt = original.clone();
    corrupt[70] ^= 1;
    for bytes in [incomplete, corrupt] {
        fs::write(&source, &bytes).unwrap();
        assert!(maintenance::backup(&source, &output).is_err());
        assert!(maintenance::restore(&source, &output).is_err());
        assert!(maintenance::compact(&source, &output).is_err());
        assert!(!output.exists());
        assert_eq!(fs::read(&source).unwrap(), bytes);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}

#[test]
fn maintenance_never_overwrites_existing_destinations_or_source_aliases() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source.vlite");
    let alias = directory.path().join("alias.vlite");
    let existing = directory.path().join("existing");
    fixture(&source);
    fs::hard_link(&source, &alias).unwrap();
    fs::write(&existing, b"unrelated").unwrap();
    let original = fs::read(&source).unwrap();
    for output in [&source, &alias, &existing, directory.path()] {
        assert!(maintenance::backup(&source, output).is_err());
        assert!(maintenance::restore(&source, output).is_err());
        assert!(maintenance::compact(&source, output).is_err());
    }
    assert_eq!(fs::read(&source).unwrap(), original);
    assert_eq!(fs::read(&alias).unwrap(), original);
    assert_eq!(fs::read(&existing).unwrap(), b"unrelated");
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 3);
}

#[test]
fn maintenance_requires_exclusive_source_ownership_and_never_creates_missing_source() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source.vlite");
    let output = directory.path().join("output.vlite");
    assert!(maintenance::inspect(&source).is_err());
    assert!(maintenance::backup(&source, &output).is_err());
    assert!(!source.exists());
    assert!(!output.exists());
    let db = Database::create(&source, 1).unwrap();
    assert!(matches!(maintenance::inspect(&source), Err(Error::Locked)));
    assert!(matches!(
        maintenance::backup(&source, &output),
        Err(Error::Locked)
    ));
    assert!(matches!(
        maintenance::restore(&source, &output),
        Err(Error::Locked)
    ));
    assert!(matches!(
        maintenance::compact(&source, &output),
        Err(Error::Locked)
    ));
    assert!(!output.exists());
    db.close().unwrap();
    maintenance::inspect(&source).unwrap();
}

#[test]
fn empty_compaction_removes_all_history_and_stays_writable() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source.vlite");
    let output = directory.path().join("output.vlite");
    let mut db = Database::create(&source, 1).unwrap();
    db.put(1, &[1.0], b"").unwrap();
    db.delete(1).unwrap();
    db.close().unwrap();
    let report = maintenance::compact(&source, &output).unwrap();
    assert_eq!(report.output.file_bytes, 32);
    assert_eq!(report.output.commits, 0);
    assert_eq!(report.output.live_records, 0);
    let mut db = Database::open(output).unwrap();
    db.put(0, &[3.0], b"after compaction").unwrap();
    db.close().unwrap();
}

#[test]
fn cli_round_trip_errors_and_incomplete_inspection_are_machine_detectable() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source with spaces.vlite");
    let backup = directory.path().join("backup.vlite");
    let restored = directory.path().join("restored.vlite");
    let compact = directory.path().join("compact.vlite");
    fixture(&source);
    let command = |action: &str, source: &Path, target: Option<&Path>| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_vlite-maintain"));
        cmd.arg(action).arg(source);
        if let Some(target) = target {
            cmd.arg(target);
        }
        cmd.output().unwrap()
    };
    for (action, input, output) in [
        ("inspect", &source, None),
        ("backup", &source, Some(backup.as_path())),
        ("restore", &backup, Some(restored.as_path())),
        ("compact", &restored, Some(compact.as_path())),
    ] {
        let result = command(action, input, output);
        assert!(result.status.success(), "{result:?}");
    }
    assert!(!command("backup", &source, Some(&backup)).status.success());
    assert!(!command("unknown", &source, None).status.success());
    assert!(!command("restore", &backup, None).status.success());
    OpenOptions::new()
        .append(true)
        .open(&source)
        .unwrap()
        .write_all(b"VREC")
        .unwrap();
    let result = command("inspect", &source, None);
    assert!(!result.status.success());
    assert!(
        String::from_utf8(result.stdout)
            .unwrap()
            .contains("incomplete_tail_bytes=4")
    );
}
