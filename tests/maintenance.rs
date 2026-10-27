use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::Command;
use tempfile::tempdir;
use vlite::{
    CollectionSettings, CollectionSpec, Database, Error, FormatVersion, Metadata, MetadataValue,
    maintenance,
};

fn fixture(path: &Path) {
    let mut db = Database::create_with_format(path, 2, FormatVersion::V3).unwrap();
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
    let db = Database::create_with_format(&source, 1, FormatVersion::V3).unwrap();
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
    let mut db = Database::create_with_format(&source, 1, FormatVersion::V3).unwrap();
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

fn decode_hex(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    digits
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn sha(path: &Path) -> Vec<u8> {
    Sha256::digest(fs::read(path).unwrap()).to_vec()
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

type Snapshot = Vec<(u64, Vec<u32>, Vec<u8>, Metadata)>;

fn snapshot(db: &Database, ids: &[u64]) -> Snapshot {
    ids.iter()
        .filter_map(|&id| {
            db.get(id).unwrap().map(|record| {
                (
                    id,
                    record.vector.iter().map(|value| value.to_bits()).collect(),
                    record.payload.clone(),
                    record.metadata.clone(),
                )
            })
        })
        .collect()
}

/// TEST-005 / AC-015: upgrade v1/v2/v3 to v4 preserving IDs, exact float bits,
/// payloads and metadata; the source is SHA-equal before and after.
#[test]
fn upgrade_converts_every_legacy_version_to_v4_without_touching_the_source() {
    let directory = tempdir().unwrap();
    let ids: Vec<u64> = (0..16).collect();
    let mut sources = Vec::new();
    for (version, hex) in [
        (1, include_str!("fixtures/v1.hex")),
        (2, include_str!("fixtures/v2.hex")),
        (3, include_str!("fixtures/v3.hex")),
    ] {
        let path = directory.path().join(format!("v{version}.vlite"));
        fs::write(&path, decode_hex(hex)).unwrap();
        sources.push((version, path));
    }
    let path = directory.path().join("generated-v3.vlite");
    fixture(&path);
    {
        let mut db = Database::open(&path).unwrap();
        let metadata: Metadata = [
            ("f".to_owned(), MetadataValue::Float(-0.0)),
            ("i".to_owned(), MetadataValue::Integer(i64::MIN)),
        ]
        .into();
        db.put_with_metadata(3, &[-0.0, f32::MIN_POSITIVE], b"m", &metadata)
            .unwrap();
        db.close().unwrap();
    }
    sources.push((3, path));

    for (index, (version, source)) in sources.iter().enumerate() {
        let before = sha(source);
        let expected = {
            let db = Database::open(source).unwrap();
            assert_eq!(db.format_version(), *version);
            snapshot(&db, &ids)
        };
        let output = directory.path().join(format!("upgraded-{index}.vlite"));
        let report = maintenance::upgrade(source, &output).unwrap();
        assert_eq!(report.source.format_version, *version);
        assert_eq!(report.output.format_version, 4);
        assert_eq!(report.output.live_records, expected.len());
        assert_eq!(sha(source), before);
        let mut db = Database::open(&output).unwrap();
        assert_eq!(db.format_version(), 4);
        assert_ne!(db.lineage(), None);
        assert_eq!(snapshot(&db, &ids), expected);
        // The upgraded copy supports collections immediately.
        db.create_collection(
            "docs",
            CollectionSpec {
                dimensions: 4,
                settings: CollectionSettings::default(),
            },
        )
        .unwrap();
        db.close().unwrap();
        // Destinations are never clobbered.
        assert!(maintenance::upgrade(source, &output).is_err());
        assert_eq!(sha(source), before);
    }
}

#[test]
fn verify_reports_independent_crc_and_rejects_incomplete_tails() {
    let directory = tempdir().unwrap();
    for format in [FormatVersion::V3, FormatVersion::V4] {
        let path = directory.path().join(format!("{format:?}.vlite"));
        let mut db = Database::create_with_format(&path, 2, format).unwrap();
        db.put(1, &[1.0, 2.0], b"x").unwrap();
        db.close().unwrap();
        let bytes = fs::read(&path).unwrap();
        let report = maintenance::verify(&path).unwrap();
        assert_eq!(report.bytes, bytes.len() as u64);
        assert_eq!(report.crc32, crc32(&bytes));
        assert_eq!(report.inspection.live_records, 1);
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"VR")
            .unwrap();
        assert!(matches!(
            maintenance::verify(&path),
            Err(Error::IncompleteTail { .. })
        ));
        let mut corrupt = bytes.clone();
        corrupt[40] ^= 1;
        fs::write(&path, &corrupt).unwrap();
        assert!(matches!(
            maintenance::verify(&path),
            Err(Error::Corrupt { .. })
        ));
        assert_eq!(fs::read(&path).unwrap(), corrupt);
    }
}

#[test]
fn drill_restores_probes_and_removes_its_scratch_file() {
    let directory = tempdir().unwrap();
    let workdir = directory.path().join("work");
    fs::create_dir(&workdir).unwrap();
    let backup = directory.path().join("backup.vlite");
    let mut db = Database::create_with_format(&backup, 2, FormatVersion::V4).unwrap();
    db.put(5, &[1.0, 0.0], b"").unwrap();
    db.put(2, &[0.0, 1.0], b"").unwrap();
    db.create_collection(
        "docs",
        CollectionSpec {
            dimensions: 3,
            settings: CollectionSettings::default(),
        },
    )
    .unwrap();
    db.collection_mut("docs")
        .unwrap()
        .put(8, &[1.0, 2.0, 3.0], b"")
        .unwrap();
    db.create_collection(
        "empty",
        CollectionSpec {
            dimensions: 2,
            settings: CollectionSettings::default(),
        },
    )
    .unwrap();
    let lineage = db.lineage().unwrap();
    db.close().unwrap();
    let before = sha(&backup);
    let report = maintenance::drill(&backup, &workdir).unwrap();
    assert_eq!(report.backup.live_records, 3);
    assert_eq!((report.collections, report.probes), (3, 2));
    assert_ne!(report.scratch_lineage, Some(lineage));
    assert!(report.scratch_lineage.is_some());
    assert_eq!(fs::read_dir(&workdir).unwrap().count(), 0);
    assert_eq!(sha(&backup), before);

    let legacy = directory.path().join("legacy.vlite");
    fixture(&legacy);
    let before = sha(&legacy);
    let report = maintenance::drill(&legacy, &workdir).unwrap();
    assert_eq!((report.collections, report.probes), (1, 1));
    assert_eq!(report.scratch_lineage, None);
    assert_eq!(fs::read_dir(&workdir).unwrap().count(), 0);
    assert_eq!(sha(&legacy), before);

    OpenOptions::new()
        .append(true)
        .open(&legacy)
        .unwrap()
        .write_all(b"VREC")
        .unwrap();
    assert!(maintenance::drill(&legacy, &workdir).is_err());
    assert_eq!(fs::read_dir(&workdir).unwrap().count(), 0);
}

#[test]
fn v4_restore_and_compaction_relineage_but_keep_data() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source.vlite");
    let mut db = Database::create_with_format(&source, 2, FormatVersion::V4).unwrap();
    db.create_collection(
        "docs",
        CollectionSpec {
            dimensions: 2,
            settings: CollectionSettings::default(),
        },
    )
    .unwrap();
    db.collection_mut("docs")
        .unwrap()
        .put(1, &[-0.0, 1.0], b"d")
        .unwrap();
    db.put(1, &[1.0, 1.0], b"default").unwrap();
    db.close().unwrap();
    let original = fs::read(&source).unwrap();
    let restored = directory.path().join("restored.vlite");
    maintenance::restore(&source, &restored).unwrap();
    let copy = fs::read(&restored).unwrap();
    assert_eq!(copy.len(), original.len());
    assert_ne!(copy[16..24], original[16..24]);
    assert_eq!(copy[32..], original[32..]);
    let compacted = directory.path().join("compacted.vlite");
    maintenance::compact(&source, &compacted).unwrap();
    assert_eq!(fs::read(&source).unwrap(), original);
    for path in [&restored, &compacted] {
        let db = Database::open(path).unwrap();
        assert_eq!(db.format_version(), 4);
        assert_eq!(db.get(1).unwrap().unwrap().payload, b"default");
        let docs = db.collection("docs").unwrap();
        assert_eq!(
            docs.get(1).unwrap().unwrap().vector[0].to_bits(),
            (-0.0f32).to_bits()
        );
    }
    let detail = maintenance::inspect_detail(&compacted).unwrap();
    assert_eq!((detail.collections, detail.open_imports), (2, 0));
    assert_eq!(detail.inspection.live_records, 2);
}

#[test]
fn cli_upgrade_verify_and_drill_report_machine_readable_results() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source.vlite");
    let upgraded = directory.path().join("upgraded.vlite");
    let workdir = directory.path().join("work");
    fs::create_dir(&workdir).unwrap();
    fixture(&source);
    let before = sha(&source);
    let run = |args: &[&Path]| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_vlite-maintain"));
        cmd.args(args);
        cmd.output().unwrap()
    };
    let stdout = |output: std::process::Output| {
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    };
    let out = stdout(run(&[Path::new("upgrade"), &source, &upgraded]));
    assert!(out.contains("source_format_version=3"), "{out}");
    assert!(out.contains("format_version=4"), "{out}");
    let crc = crc32(&fs::read(&upgraded).unwrap());
    let out = stdout(run(&[Path::new("verify"), &upgraded]));
    assert!(out.contains(&format!("crc32={crc:08x}")), "{out}");
    let out = stdout(run(&[Path::new("drill"), &upgraded, &workdir]));
    assert!(out.contains("drill=passed collections=1 probes=1"), "{out}");
    assert_eq!(fs::read_dir(&workdir).unwrap().count(), 0);
    assert_eq!(sha(&source), before);
    assert!(
        !run(&[Path::new("upgrade"), &source, &upgraded])
            .status
            .success()
    );
    assert!(!run(&[Path::new("verify")]).status.success());
    assert!(!run(&[Path::new("drill"), &upgraded]).status.success());
}
