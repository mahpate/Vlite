// DO-001 cross-version rejection probe (feature-agent-graph ITEM-007).
//
// NOT part of the VLite build. `scripts\graph_compat_check.py` copies this
// file into a throwaway crate that depends on an isolated `git archive`
// extraction of one revision (HEAD or a release tag), keeping only the
// `// @block <symbol> ...` sections whose symbols exist in that revision's
// `src`. Each block prints one tab-separated line per entry point:
//   RESULT <fixture> <entry point> ok|err <Debug of the error>
//   PUBLISHED <fixture> <entry point> <path>   (an output file was left behind)
#![allow(unused_imports, unused_variables, unused_mut, dead_code)]

use std::fmt::Debug;
use std::path::{Path, PathBuf};

fn show<T, E: Debug>(fixture: &str, entry: &str, result: Result<T, E>) {
    match result {
        Ok(_) => println!("RESULT\t{fixture}\t{entry}\tok\t"),
        Err(error) => println!("RESULT\t{fixture}\t{entry}\terr\t{error:?}"),
    }
}

fn absent(fixture: &str, entry: &str, path: &Path) {
    if path.exists() {
        println!("PUBLISHED\t{fixture}\t{entry}\t{}", path.display());
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("make-encrypted") => make_encrypted(Path::new(&args[2])),
        Some("check") => {
            let scratch = PathBuf::from(&args[2]);
            for spec in &args[3..] {
                let (name, path) = spec.split_once('=').expect("name=path");
                check(name, Path::new(path), &scratch.join(name));
            }
        }
        _ => panic!("usage: probe make-encrypted PATH | probe check SCRATCH NAME=PATH..."),
    }
}

fn make_encrypted(path: &Path) {
    // @block pub struct CreateOptions | pub fn create_with_options( | pub fn from_bytes(key: [u8; 32])
    let options = vlite::CreateOptions {
        format: Some(vlite::FormatVersion::V5),
        encryption: Some(vlite::EncryptionKey::from_bytes([0x42; 32])),
        commit_timestamps: false,
    };
    let mut db = vlite::Database::create_with_options(path, 2, options).unwrap();
    db.put(7, &[1.0, 2.0], b"probe").unwrap();
    db.close().unwrap();
    println!("CREATED\t{}", path.display());
    return;
    // @end
    #[allow(unreachable_code)]
    {
        panic!("this revision cannot create encrypted files");
    }
}

fn check(name: &str, source: &Path, scratch: &Path) {
    std::fs::create_dir_all(scratch).unwrap();
    let out = |entry: &str| scratch.join(format!("{entry}.vlite"));
    let bytes = std::fs::read(source).unwrap();

    show(name, "Database::open", vlite::Database::open(source));

    let destination = out("backup");
    show(
        name,
        "maintenance::backup",
        vlite::maintenance::backup(source, &destination),
    );
    absent(name, "maintenance::backup", &destination);
    let destination = out("restore");
    show(
        name,
        "maintenance::restore",
        vlite::maintenance::restore(source, &destination),
    );
    absent(name, "maintenance::restore", &destination);
    let destination = out("compact");
    show(
        name,
        "maintenance::compact",
        vlite::maintenance::compact(source, &destination),
    );
    absent(name, "maintenance::compact", &destination);
    show(
        name,
        "maintenance::inspect",
        vlite::maintenance::inspect(source),
    );

    // @block pub fn inspect_detail(
    show(
        name,
        "maintenance::inspect_detail",
        vlite::maintenance::inspect_detail(source),
    );
    // @end
    // @block pub fn verify(path
    show(
        name,
        "maintenance::verify",
        vlite::maintenance::verify(source),
    );
    // @end
    // @block pub fn upgrade(
    let destination = out("upgrade");
    show(
        name,
        "maintenance::upgrade",
        vlite::maintenance::upgrade(source, &destination),
    );
    absent(name, "maintenance::upgrade", &destination);
    // @end
    // @block pub fn drill(
    let workdir = scratch.join("drill");
    std::fs::create_dir_all(&workdir).unwrap();
    show(
        name,
        "maintenance::drill",
        vlite::maintenance::drill(source, &workdir),
    );
    if std::fs::read_dir(&workdir).unwrap().next().is_some() {
        println!(
            "PUBLISHED\t{name}\tmaintenance::drill\t{}",
            workdir.display()
        );
    }
    // @end
    // @block pub fn open_read_only(path
    show(
        name,
        "Database::open_read_only",
        vlite::Database::open_read_only(source),
    );
    // @end
    // @block pub fn compact_at(
    let destination = out("compact_at");
    show(
        name,
        "maintenance::compact_at",
        vlite::maintenance::compact_at(source, &destination, 1_700_000_000_000),
    );
    absent(name, "maintenance::compact_at", &destination);
    // @end
    // @block pub fn open_from_bytes(
    show(
        name,
        "Database::open_from_bytes(None)",
        vlite::Database::open_from_bytes(bytes.clone(), None),
    );
    // @end
    // @block pub fn discard_incomplete_tail(path
    show(
        name,
        "Database::discard_incomplete_tail",
        vlite::Database::discard_incomplete_tail(source),
    );
    // @end
    // @block pub fn from_bytes(key: [u8; 32])
    let key = vlite::EncryptionKey::from_bytes([0x42; 32]);
    // @end
    // @block pub fn from_bytes(key: [u8; 32]) | pub fn open_encrypted(
    show(
        name,
        "Database::open_encrypted",
        vlite::Database::open_encrypted(source, &key),
    );
    // @end
    // @block pub fn from_bytes(key: [u8; 32]) | pub fn open_read_only_encrypted(
    show(
        name,
        "Database::open_read_only_encrypted",
        vlite::Database::open_read_only_encrypted(source, &key),
    );
    // @end
    // @block pub fn from_bytes(key: [u8; 32]) | pub fn open_from_bytes(
    show(
        name,
        "Database::open_from_bytes(key)",
        vlite::Database::open_from_bytes(bytes.clone(), Some(&key)),
    );
    // @end
    // @block pub fn from_bytes(key: [u8; 32]) | pub fn discard_incomplete_tail_encrypted(
    show(
        name,
        "Database::discard_incomplete_tail_encrypted",
        vlite::Database::discard_incomplete_tail_encrypted(source, &key),
    );
    // @end
    // @block pub fn from_bytes(key: [u8; 32]) | pub fn inspect_with_key(
    show(
        name,
        "maintenance::inspect_with_key",
        vlite::maintenance::inspect_with_key(source, &key),
    );
    // @end
    // @block pub fn from_bytes(key: [u8; 32]) | pub fn verify_with_key(
    show(
        name,
        "maintenance::verify_with_key",
        vlite::maintenance::verify_with_key(source, &key),
    );
    // @end
    // @block pub fn from_bytes(key: [u8; 32]) | pub fn backup_with_key(
    let destination = out("backup_with_key");
    show(
        name,
        "maintenance::backup_with_key",
        vlite::maintenance::backup_with_key(source, &destination, &key),
    );
    absent(name, "maintenance::backup_with_key", &destination);
    // @end
    // @block pub fn from_bytes(key: [u8; 32]) | pub fn restore_with_key(
    let destination = out("restore_with_key");
    show(
        name,
        "maintenance::restore_with_key",
        vlite::maintenance::restore_with_key(source, &destination, &key),
    );
    absent(name, "maintenance::restore_with_key", &destination);
    // @end
    // @block pub fn from_bytes(key: [u8; 32]) | pub fn compact_with_key(
    let destination = out("compact_with_key");
    show(
        name,
        "maintenance::compact_with_key",
        vlite::maintenance::compact_with_key(source, &destination, &key),
    );
    absent(name, "maintenance::compact_with_key", &destination);
    // @end
    // @block pub fn from_bytes(key: [u8; 32]) | pub fn reencrypt(
    for (label, source_key) in [("None", None), ("key", Some(&key))] {
        let destination = out(&format!("reencrypt_{label}"));
        let entry = format!("maintenance::reencrypt({label})");
        show(
            name,
            &entry,
            vlite::maintenance::reencrypt(source, source_key, &destination, Some(&key)),
        );
        absent(name, &entry, &destination);
    }
    // @end
    // @block pub fn from_bytes(key: [u8; 32]) | pub fn restore_to(
    for (label, source_key) in [("None", None), ("key", Some(&key))] {
        let destination = out(&format!("restore_to_{label}"));
        let entry = format!("maintenance::restore_to({label})");
        show(
            name,
            &entry,
            vlite::maintenance::restore_to(
                None,
                source,
                &destination,
                vlite::maintenance::RestoreTarget::Sequence(1),
                source_key,
            ),
        );
        absent(name, &entry, &destination);
    }
    // @end
    // @block pub fn from_bytes(key: [u8; 32]) | pub fn export_log_segment(
    for (label, source_key) in [("None", None), ("key", Some(&key))] {
        show(
            name,
            &format!("maintenance::export_log_segment({label})"),
            vlite::maintenance::export_log_segment(source, source_key, 0, 1 << 20),
        );
    }
    // @end
    // @block pub fn from_bytes(key: [u8; 32]) | pub fn read_log_changes(
    for (label, source_key) in [("None", None), ("key", Some(&key))] {
        let cursor = vlite::changes::FeedCursor {
            lineage: 0,
            sequence: 0,
        };
        show(
            name,
            &format!("changes::read_log_changes({label})"),
            vlite::changes::read_log_changes(source, source_key, &cursor, 16),
        );
    }
    // @end
    // @block pub fn from_bytes(key: [u8; 32]) | pub struct ReplicaFollower | pub fn sync_from_log(
    for (label, source_key) in [("None", None), ("key", Some(&key))] {
        show(
            name,
            &format!("ReplicaFollower::open({label})"),
            vlite::ReplicaFollower::open(source, source_key),
        );
        let segment = vlite::LogSegment {
            lineage: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
            header: bytes[..32].to_vec(),
            base_sequence: 0,
            last_sequence: 2,
            head_sequence: 2,
            frames: bytes[32..].to_vec(),
        };
        let destination = out(&format!("follower_{label}"));
        let entry = format!("ReplicaFollower::create({label})");
        show(
            name,
            &entry,
            vlite::ReplicaFollower::create(&destination, &segment, source_key),
        );
        absent(name, &entry, &destination);
    }
    // Healthy vector-only follower (the plain control fixture) tailing the file.
    if let Ok(control) = std::env::var("PROBE_FOLLOWER_BASE") {
        let follower_path = out("vector_follower");
        let _ = std::fs::remove_file(&follower_path);
        std::fs::copy(control, &follower_path).unwrap();
        let mut follower = vlite::ReplicaFollower::open(&follower_path, None).unwrap();
        show(
            name,
            "ReplicaFollower::sync_from_log",
            follower.sync_from_log(source, 1 << 20),
        );
        let mut header = bytes[..32].to_vec();
        let graph_segment = vlite::LogSegment {
            lineage: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
            header,
            base_sequence: 1,
            last_sequence: 2,
            head_sequence: 2,
            frames: std::fs::read(std::env::var("PROBE_GRAPH_FRAME").unwrap()).unwrap(),
        };
        show(
            name,
            "ReplicaFollower::apply",
            follower.apply(&graph_segment),
        );
        let segment_path = scratch.join("graph.vlseg");
        graph_segment.save(&segment_path).unwrap();
        show(
            name,
            "ReplicaFollower::apply_file",
            follower.apply_file(&segment_path),
        );
    }
    // @end
}
