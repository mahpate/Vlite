//! Baseline capture program for feature-agent-graph ITEM-003.
//!
//! NOT compiled by the VLite workspace. `scripts\graph_baseline.py fixtures`
//! copies this file and `scenario.rs` into a throwaway crate that depends on
//! an isolated `git archive` extraction of the baseline commit, runs it, and
//! writes the outputs to `tests\fixtures\graph_baseline\`.
//!
//! Usage: capture OUT_DIR WORK_DIR

mod scenario;

use std::fs;
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let out = PathBuf::from(&args[1]);
    let work = PathBuf::from(&args[2]);
    fs::create_dir_all(&out).unwrap();
    for variant in scenario::VARIANTS {
        let dir = work.join(variant.name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("scratch-full")).unwrap();
        fs::create_dir_all(dir.join("scratch-state")).unwrap();
        let path = dir.join(format!("{}.vlite", variant.name));

        let mut db = variant.create(&path);
        let ops = scenario::populate(&mut db);
        db.close().unwrap();
        let bytes = fs::read(&path).unwrap();

        let key = variant.key();
        let mut db = variant.open(&path);
        let full = scenario::report(&mut db, &dir.join("scratch-full"), true, key.as_ref());
        db.close().unwrap();
        let mut db = variant.open(&path);
        let state = scenario::report(&mut db, &dir.join("scratch-state"), false, key.as_ref());
        db.close().unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes, "report must not write");
        let maintenance_dir = dir.join("maintenance");
        fs::create_dir_all(&maintenance_dir).unwrap();
        let maintenance = scenario::maintenance_report(&variant, &path, &maintenance_dir);

        let mut hex = String::with_capacity(bytes.len() * 2 + bytes.len() / 32 + 1);
        for (index, byte) in bytes.iter().enumerate() {
            hex.push_str(&format!("{byte:02x}"));
            if index % 32 == 31 {
                hex.push('\n');
            }
        }
        hex.push('\n');
        fs::write(out.join(format!("{}.vlite.hex", variant.name)), hex).unwrap();
        fs::write(out.join(format!("{}.ops.txt", variant.name)), ops).unwrap();
        fs::write(out.join(format!("{}.report.txt", variant.name)), full).unwrap();
        fs::write(out.join(format!("{}.state.txt", variant.name)), state).unwrap();
        fs::write(
            out.join(format!("{}.maintenance.txt", variant.name)),
            maintenance,
        )
        .unwrap();
        println!("captured {} ({} bytes)", variant.name, bytes.len());
    }
}
