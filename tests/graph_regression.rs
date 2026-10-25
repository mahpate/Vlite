//! TEST-001 graph-off parity harness (feature-agent-graph EPIC-001 ITEM-004).
//!
//! Compares the current build against golden captures produced from the
//! baseline commit `347b01d` by `python scripts\graph_baseline.py fixtures`
//! (an isolated `git archive` build; see tests\fixtures\graph_baseline\README.md):
//!
//! * REG-003 byte identity of newly created V3 files; V4/V5 identical after
//!   masking lineage (+ header CRC) and, for encrypted files, the random
//!   salt/nonce-derived bytes (descriptor, sealed bodies, body CRCs).
//! * REG-004 golden V3/V4/V5 (plain, encrypted, timestamps) files are opened,
//!   read, written, reopened, compacted, backed up, restored and verified with
//!   identical results and no header/version change.
//! * REG-005 search parity (exact, filtered, HNSW, quantized, lexical, hybrid,
//!   grouping, facets, geo, `query_language::parse`).
//! * REG-006 mutation-surface results and errors (populate log).
//! * REG-007 change feed events and replica bootstrap.
//!
//! No graph code is exercised here: this file runs with `graph` compiled in
//! (C-2) and must pass unchanged.

#[path = "graph_baseline/scenario.rs"]
mod scenario;

use std::fs;
use std::path::{Path, PathBuf};

use scenario::{VARIANTS, Variant};
use sha2::{Digest, Sha256};
use tempfile::tempdir;
use vlite::maintenance;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("graph_baseline")
}

/// Text fixtures are compared LF-normalized (Windows checkouts may use CRLF).
fn text(name: &str) -> String {
    let path = fixture_dir().join(name);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .replace("\r\n", "\n")
}

fn decode_hex(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    assert!(digits.len().is_multiple_of(2));
    digits
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn golden_bytes(variant: &Variant) -> Vec<u8> {
    decode_hex(&text(&format!("{}.vlite.hex", variant.name)))
}

/// Canonical content hashed by MANIFEST.sha256: decoded bytes for `.hex`,
/// LF-normalized UTF-8 for text.
fn canonical(name: &str) -> Vec<u8> {
    if name.ends_with(".hex") {
        decode_hex(&text(name))
    } else {
        text(name).into_bytes()
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
fn reg003_manifest_covers_every_fixture_and_matches() {
    let manifest = text("MANIFEST.sha256");
    let mut listed = Vec::new();
    for line in manifest.lines().filter(|line| !line.starts_with('#')) {
        let (digest, name) = line.split_once("  ").expect("`<sha256>  <name>`");
        assert_eq!(hex_digest(&canonical(name)), digest, "{name} drifted");
        listed.push(name.to_string());
    }
    let mut present: Vec<String> = fs::read_dir(fixture_dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name != "MANIFEST.sha256" && name != "README.md")
        .collect();
    present.sort();
    listed.sort();
    assert_eq!(present, listed, "every golden file must be in the manifest");
    for variant in VARIANTS {
        for suffix in [
            "vlite.hex",
            "ops.txt",
            "report.txt",
            "state.txt",
            "maintenance.txt",
        ] {
            assert!(
                listed.contains(&format!("{}.{suffix}", variant.name)),
                "{} missing {suffix}",
                variant.name
            );
        }
    }
}

/// Shape of a log file with random bytes masked (REG-003). Plain files keep
/// every byte except lineage and header CRC; encrypted files additionally
/// replace the descriptor, sealed bodies and body CRCs by their lengths.
fn masked(bytes: &[u8], encrypted: bool) -> Vec<u8> {
    let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let mut header = bytes[..32].to_vec();
    if version >= 4 {
        header[16..24].fill(0xee);
        header[28..32].fill(0xee);
    }
    if !encrypted {
        let mut out = header;
        out.extend_from_slice(&bytes[32..]);
        return out;
    }
    let mut out = header;
    let first = 32
        + bytes[32..]
            .windows(4)
            .position(|window| window == b"VREC")
            .expect("first frame");
    out.extend_from_slice(b"DESC");
    out.extend_from_slice(&((first - 32) as u64).to_le_bytes());
    let mut offset = first;
    while offset < bytes.len() {
        assert_eq!(&bytes[offset..offset + 4], b"VREC", "frame at {offset}");
        let prefix = &bytes[offset..offset + 36];
        let sequence = &prefix[8..16];
        out.extend_from_slice(prefix);
        let body_start = offset + 36;
        let footer = (body_start..=bytes.len() - 16)
            .find(|&at| {
                &bytes[at..at + 4] == b"VCMT"
                    && &bytes[at + 8..at + 16] == sequence
                    && (at + 16 == bytes.len() || &bytes[at + 16..at + 20] == b"VREC")
            })
            .expect("commit footer");
        out.extend_from_slice(b"BODY");
        out.extend_from_slice(&((footer - body_start) as u64).to_le_bytes());
        out.extend_from_slice(b"VCMT");
        out.extend_from_slice(sequence);
        offset = footer + 16;
    }
    out
}

#[test]
fn reg003_reg006_new_files_match_baseline_bytes_and_results() {
    let dir = tempdir().unwrap();
    for variant in VARIANTS {
        let path = dir.path().join(format!("{}.vlite", variant.name));
        let mut db = variant.create(&path);
        let ops = scenario::populate(&mut db);
        db.close().unwrap();
        assert_eq!(
            ops,
            text(&format!("{}.ops.txt", variant.name)),
            "{}: mutation results/errors differ from baseline (REG-006)",
            variant.name
        );
        let created = fs::read(&path).unwrap();
        let golden = golden_bytes(&variant);
        if variant.name == "v3" {
            assert!(
                created == golden,
                "v3 file must be byte-identical (REG-003)"
            );
        } else {
            assert!(
                masked(&created, variant.encrypted) == masked(&golden, variant.encrypted),
                "{}: masked bytes differ from baseline (REG-003)",
                variant.name
            );
        }
    }
}

fn header_version(path: &Path) -> (Vec<u8>, u32) {
    let bytes = fs::read(path).unwrap();
    let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    (bytes[..12].to_vec(), version)
}

fn state_report(variant: &Variant, path: &Path, scratch: &Path) -> String {
    let mut db = variant.open(path);
    let key = variant.key();
    let report = scenario::report(&mut db, scratch, false, key.as_ref());
    db.close().unwrap();
    report
}

#[test]
fn reg004_to_reg007_golden_files_round_trip_with_identical_results() {
    let dir = tempdir().unwrap();
    for variant in VARIANTS {
        let work = dir.path().join(variant.name);
        fs::create_dir_all(&work).unwrap();
        let scratch = |name: &str| {
            let path = work.join(format!("scratch-{name}"));
            fs::create_dir_all(&path).unwrap();
            path
        };
        let path = work.join("golden.vlite");
        let golden = golden_bytes(&variant);
        fs::write(&path, &golden).unwrap();
        let (header, version) = header_version(&path);
        let key = variant.key();
        let expected_full = text(&format!("{}.report.txt", variant.name));
        let expected_state = text(&format!("{}.state.txt", variant.name));

        // Open + read: search/feed/replica parity (REG-005, REG-007).
        let mut db = variant.open(&path);
        let full = scenario::report(&mut db, &scratch("full"), true, key.as_ref());
        db.close().unwrap();
        assert_eq!(full, expected_full, "{}: report differs", variant.name);
        assert!(
            fs::read(&path).unwrap() == golden,
            "{}: read-only use modified the file",
            variant.name
        );
        assert_eq!(
            state_report(&variant, &path, &scratch("state")),
            expected_state,
            "{}",
            variant.name
        );

        // Backup, restore, compact, verify (REG-004) with identical results.
        let maintenance_dir = work.join("maintenance");
        fs::create_dir_all(&maintenance_dir).unwrap();
        assert_eq!(
            scenario::maintenance_report(&variant, &path, &maintenance_dir),
            text(&format!("{}.maintenance.txt", variant.name)),
            "{}: maintenance round trip differs",
            variant.name
        );
        assert!(
            fs::read(maintenance_dir.join("backup.vlite")).unwrap() == golden,
            "{}: backup must be a byte copy",
            variant.name
        );
        for output in ["backup.vlite", "restored.vlite", "compacted.vlite"] {
            assert_eq!(
                header_version(&maintenance_dir.join(output)).1,
                version,
                "{} {output}: version changed",
                variant.name
            );
        }

        // Write, reopen, verify; header and version unchanged.
        let mut db = variant.open(&path);
        db.put(100, &scenario::vector(100), b"after-baseline")
            .unwrap();
        db.close().unwrap();
        let db = variant.open(&path);
        assert_eq!(
            db.get(100).unwrap().unwrap().payload,
            b"after-baseline".to_vec()
        );
        assert_eq!(db.format_version(), version);
        drop(db);
        assert_eq!(header_version(&path).0, header, "{}", variant.name);
        let verification = match &key {
            Some(key) => maintenance::verify_with_key(&path, key).unwrap(),
            None => maintenance::verify(&path).unwrap(),
        };
        assert_eq!(verification.inspection.format_version, version);
        assert_eq!(
            fs::read(&path).unwrap()[..golden.len()],
            golden[..],
            "{}: writes must only append",
            variant.name
        );
    }
}
