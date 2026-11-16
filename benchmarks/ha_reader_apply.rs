//! Bounded, synthetic local correctness smoke; not an HA benchmark campaign.
//! Protocol and measurement boundaries: benchmarks/ha-reader-apply.md.

use std::{fs, path::Path, process::Command, time::Instant};

use serde_json::{Value, json};
use vlite::{
    Database, Durability, Error, FormatVersion, HnswConfig, LogSegment, ReplicaFollower,
    maintenance,
};

type BoxResult<T> = Result<T, Box<dyn std::error::Error>>;

const DIMENSIONS: usize = 8;
const ROUNDS: u64 = 3;
const MAX_SEGMENT_BYTES: usize = 1024 * 1024;
const INDEX: HnswConfig = HnswConfig {
    m: 4,
    ef_construction: 16,
    seed: 42,
};

fn timed<T>(operation: impl FnOnce() -> BoxResult<T>) -> BoxResult<(T, u64)> {
    let start = Instant::now();
    let value = operation()?;
    Ok((value, nanos_since(start)))
}

fn nanos_since(start: Instant) -> u64 {
    start.elapsed().as_nanos() as u64
}

fn require(condition: bool, message: &str) -> BoxResult<()> {
    if !condition {
        return Err(message.into());
    }
    Ok(())
}

fn vector(id: u64) -> Vec<f32> {
    (0..DIMENSIONS)
        .map(|dimension| ((id * 17 + dimension as u64 * 13) % 101) as f32 / 101.0)
        .collect()
}

fn verify_equivalence(source: &Database, reader: &Database, id_bound: u64) -> BoxResult<()> {
    require(source.lineage() == reader.lineage(), "lineage mismatch")?;
    require(source.len() == reader.len(), "record count mismatch")?;
    let mut seen = 0;
    for id in 0..id_bound {
        let expected = source.get(id)?;
        seen += usize::from(expected.is_some());
        require(expected == reader.get(id)?, "record content mismatch")?;
        require(
            source.revision(id)? == reader.revision(id)?,
            "revision mismatch",
        )?;
    }
    require(
        seen == source.len(),
        "fixture IDs exceed verification bound",
    )
}

fn check_rejections(
    mut follower: ReplicaFollower,
    segment: &LogSegment,
    shipped: &Path,
) -> BoxResult<ReplicaFollower> {
    let path = follower.path().to_owned();
    let position = (follower.status().lineage, follower.applied_sequence());
    // Windows mandatory locks also exclude raw reads through another handle.
    drop(follower);
    let before = fs::read(&path)?;
    follower = ReplicaFollower::open(&path, None)?;
    // The duplicate is real; the gap and lineage faults change one envelope field.
    let gap = LogSegment {
        base_sequence: follower.applied_sequence() + 1,
        ..segment.clone()
    };
    let foreign = LogSegment {
        lineage: segment.lineage ^ 1,
        ..segment.clone()
    };
    for invalid in [segment, &gap, &foreign] {
        require(
            matches!(follower.apply(invalid), Err(Error::ReplicaDiverged(_))),
            "duplicate, gap or foreign lineage was not refused",
        )?;
        require(
            (follower.status().lineage, follower.applied_sequence()) == position,
            "rejected apply changed applied position",
        )?;
        drop(follower);
        require(fs::read(&path)? == before, "rejected apply changed file")?;
        follower = ReplicaFollower::open(&path, None)?;
    }
    let original = fs::read(shipped)?;
    let mut corrupt = original.clone();
    let middle = corrupt.len() / 2;
    corrupt[middle] ^= 0x40;
    for invalid in [&corrupt[..], &original[..original.len() - 1]] {
        // The shipping decoder is the same one used by the measured successful path.
        require(
            matches!(
                LogSegment::from_bytes(invalid),
                Err(Error::ReplicaDiverged(_))
            ),
            "corrupt or truncated shipped segment was not refused",
        )?;
    }
    drop(follower);
    require(fs::read(&path)? == before, "rejected file changed follower")?;
    Ok(ReplicaFollower::open(path, None)?)
}

fn run_case(directory: &Path, baseline_records: u64) -> BoxResult<Value> {
    let source_path = directory.join("source.vlite");
    let baseline_path = directory.join("baseline.vlite");
    let follower_path = directory.join("follower.vlite");
    let mut source = Database::create_with_format(&source_path, DIMENSIONS, FormatVersion::V5)?;
    source.set_durability(Durability::Full)?;
    require(
        source.durability() == Durability::Full,
        "expected full durability",
    )?;
    for id in 0..baseline_records {
        source.put(id, &vector(id), &[b'b'; 64])?;
    }
    source.close()?;
    let (_, backup_ns) = timed(|| Ok(maintenance::backup(&source_path, &baseline_path)?))?;
    let (_, restore_ns) = timed(|| Ok(maintenance::backup(&baseline_path, &follower_path)?))?;
    let baseline_bytes = fs::metadata(&baseline_path)?.len();
    let mut source = Database::open(&source_path)?;
    source.set_durability(Durability::Full)?;
    let (mut follower, baseline_open_ns) =
        timed(|| Ok(ReplicaFollower::open(&follower_path, None)?))?;
    verify_equivalence(&source, follower.db(), baseline_records)?;
    let baseline_status = follower.status();
    require(
        baseline_status.applied_sequence == baseline_records,
        "baseline sequence mismatch",
    )?;
    let mut samples = Vec::new();
    let mut retained_segment_bytes = 0;
    for round in 0..ROUNDS {
        let base_sequence = follower.applied_sequence();
        source.put(0, &vector(baseline_records + round), &[b'u'; 64])?;
        // Oldest acknowledgement in this three-commit window, not commit-start time.
        let oldest_ack = Instant::now();
        source.delete(round + 1)?;
        source.put(baseline_records + round, &vector(round), &[b'n'; 64])?;
        let (segment, export_ns) =
            timed(|| Ok(source.export_log(base_sequence, MAX_SEGMENT_BYTES)?))?;
        require(
            segment.last_sequence == base_sequence + 3
                && segment.last_sequence == segment.head_sequence,
            "smoke delta did not fit one segment",
        )?;
        let shipped = directory.join(format!("segment-{round}.vseg"));
        let (_, save_ns) = timed(|| Ok(segment.save(&shipped)?))?;
        let serialized_bytes = fs::metadata(&shipped)?.len();
        retained_segment_bytes += serialized_bytes;
        let (loaded, load_verify_ns) = timed(|| Ok(LogSegment::load(&shipped)?))?;
        require(loaded == segment, "shipping round-trip mismatch")?;
        let (status, apply_ns) = timed(|| Ok(follower.apply(&loaded)?))?;
        require(
            status.applied_sequence == segment.last_sequence
                && status.lineage == baseline_status.lineage
                && status.lag == 0,
            "applied position mismatch",
        )?;
        verify_equivalence(&source, follower.db(), baseline_records + round + 1)?;
        let exact_visible_ns = nanos_since(oldest_ack);
        drop(follower);

        // This detached read-only handle is NOT a continuously serving follower.
        let (mut indexed, read_only_reopen_ns) =
            timed(|| Ok(Database::open_read_only(&follower_path)?))?;
        let (_, index_build_ns) = timed(|| Ok(indexed.build_hnsw(INDEX)?))?;
        let query = vector(round);
        let exact = indexed.search(&query, 1)?;
        let approximate = indexed.search_approximate_ready(&query, 1, 64, INDEX)?;
        require(
            !exact.is_empty() && exact == approximate,
            "smoke ANN query differs from exact result",
        )?;
        let index_ready_ns = nanos_since(oldest_ack);
        verify_equivalence(&source, &indexed, baseline_records + round + 1)?;
        drop(indexed);

        let (reopened, follower_reopen_ns) =
            timed(|| Ok(ReplicaFollower::open(&follower_path, None)?))?;
        follower = reopened;
        require(follower.status() == status, "reopened position mismatch")?;
        verify_equivalence(&source, follower.db(), baseline_records + round + 1)?;
        follower = check_rejections(follower, &loaded, &shipped)?;
        let source_bytes = fs::metadata(&source_path)?.len();
        let follower_bytes = fs::metadata(&follower_path)?.len();
        let staging_bytes = baseline_bytes + retained_segment_bytes;
        let sampled_total = source_bytes + follower_bytes + staging_bytes;
        let actual_total = fs::read_dir(directory)?.try_fold(0u64, |sum, entry| {
            Ok::<_, std::io::Error>(sum + entry?.metadata()?.len())
        })?;
        require(sampled_total == actual_total, "file accounting mismatch")?;
        samples.push(json!({
            "round": round,
            "base_sequence": base_sequence,
            "last_sequence": segment.last_sequence,
            "applied_sequence": status.applied_sequence,
            "reopened_sequence": follower.applied_sequence(),
            "lineage": status.lineage,
            "lag": status.lag,
            "export_ns": export_ns,
            "save_ns": save_ns,
            "load_verify_ns": load_verify_ns,
            "apply_inclusive_ns": apply_ns,
            "apply_fsync_ns": null,
            "apply_replay_ns": null,
            "read_only_reopen_ns": read_only_reopen_ns,
            "detached_index_build_ns": index_build_ns,
            "follower_reopen_ns": follower_reopen_ns,
            "oldest_ack_to_exact_visible_ns": exact_visible_ns,
            "oldest_ack_to_detached_index_ready_ns": index_ready_ns,
            "frame_bytes": segment.frames.len(),
            "serialized_segment_bytes": serialized_bytes,
            "files": {
                "source_bytes": source_bytes,
                "follower_bytes": follower_bytes,
                "retained_staging_bytes": staging_bytes,
                "sampled_total_bytes": sampled_total
            },
            "data_equivalent": true,
            "rejections_leave_file_unchanged": true,
            "rejected_cases": [
                "duplicate", "gap", "foreign_lineage", "corrupt_file", "truncated_file"
            ]
        }));
    }
    let empty = source.export_log(follower.applied_sequence(), MAX_SEGMENT_BYTES)?;
    require(empty.is_empty(), "expected empty tail")?;
    let status = follower.status();
    drop(follower);
    let before = fs::read(&follower_path)?;
    let mut follower = ReplicaFollower::open(&follower_path, None)?;
    require(
        follower.apply(&empty)? == status,
        "empty tail changed position",
    )?;
    drop(follower);
    require(
        fs::read(&follower_path)? == before,
        "empty tail changed file",
    )?;
    source.close()?;
    require(
        fs::read(&source_path)? == fs::read(&follower_path)?,
        "final source and follower files differ",
    )?;
    maintenance::verify(&follower_path)?;
    Ok(json!({
        "baseline_records": baseline_records,
        "baseline": {
            "sequence": baseline_status.applied_sequence,
            "lineage": baseline_status.lineage,
            "bytes": baseline_bytes,
            "backup_inclusive_ns": backup_ns,
            "restore_backup_inclusive_ns": restore_ns,
            "follower_open_ns": baseline_open_ns
        },
        "samples": samples,
        "final": {
            "sequence": status.applied_sequence,
            "byte_identical": true,
            "empty_tail_verified": true
        }
    }))
}

fn command_output(program: &str, arguments: &[&str]) -> BoxResult<String> {
    let output = Command::new(program).args(arguments).output()?;
    require(
        output.status.success(),
        &String::from_utf8_lossy(&output.stderr),
    )?;
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn main() -> BoxResult<()> {
    require(
        std::env::args().skip(1).collect::<Vec<_>>() == ["--smoke"],
        "usage: cargo run --locked --features json --example ha-reader-apply -- --smoke",
    )?;
    let directory = tempfile::tempdir()?;
    let mut cases = Vec::new();
    for records in [8, 32] {
        let path = directory.path().join(format!("records-{records}"));
        fs::create_dir(&path)?;
        cases.push(run_case(&path, records)?);
    }
    let report = json!({
        "schema_version": 1,
        "scope": "SYNTHETIC_LOCAL_CORRECTNESS_SMOKE_NOT_ACA_SLO",
        "source": {
            "revision": command_output("git", &["rev-parse", "HEAD"])?,
            "working_tree_dirty": !command_output("git", &["status", "--porcelain"])?.is_empty(),
            "package_version": env!("CARGO_PKG_VERSION"),
            "rustc": command_output("rustc", &["--version"])?,
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "debug_assertions": cfg!(debug_assertions),
            "temporary_filesystem_location": directory.path().display().to_string()
        },
        "settings": {
            "source_format": "V5 single-file, unencrypted, no commit timestamps",
            "segment_format": "VLSEG001 exported through Database::export_log",
            "durability": "Full (source and follower)",
            "fixture": "deterministic modular vectors; one collection; 64-byte payloads",
            "dimensions": DIMENSIONS,
            "rounds": ROUNDS,
            "commits_per_round": 3,
            "mutations_per_round": ["update", "delete", "insert"],
            "max_segment_bytes": MAX_SEGMENT_BYTES,
            "hnsw": {"m": INDEX.m, "ef_construction": INDEX.ef_construction,
                "seed": INDEX.seed, "metric": "squared_l2", "quantization": "none",
                "query_k": 1, "query_ef": 64},
            "cache": "uncontrolled warm local OS cache; no cold-cache claim",
            "concurrency": "one serial process; no background writes or serving load",
            "staging": "baseline and all serialized deltas retained until case cleanup"
        },
        "boundaries": {
            "apply_inclusive_ns": "append + fsync + full replay; not individually instrumented",
            "detached_index_build_ns": "build_hnsw on separately reopened read-only Database",
            "ack_intervals": "oldest successful put return to verified local visibility; includes subsequent commits and local shipping; NOT durable-archive staleness",
            "file_bytes": "logical regular-file lengths sampled after each round, including source; NOT peak allocation, RSS, compaction headroom or ACA capacity",
            "baseline_backup": "maintenance::backup inclusive verification, staging, sync and publication",
            "index_strategy": "follower dropped, read-only handle opened/built/dropped, follower reopened; no runtime reader strategy implemented"
        },
        "unmeasured": {
            "blob_upload_latency": "UNMEASURED: requires EPIC-PITR-001 verified archive path",
            "aca_staleness_distribution": "UNMEASURED: requires archive, poll scheduling and deployed reader load",
            "aca_capacity_ceiling": "UNMEASURED: requires tier, allocation and compaction-headroom qualification",
            "apply_fsync_and_replay_separately": "UNMEASURED: no engine instrumentation in this slice",
            "incremental_apply": "UNMEASURED: not implemented",
            "snapshot_refresh_serving": "UNMEASURED: not implemented",
            "baseline_replacement_serving": "UNMEASURED: not implemented",
            "in_place_follower_index_rebuild": "UNMEASURED: follower exposes only an immutable Database",
            "epoch_and_archive_continuity": "UNMEASURED: exported segments carry no fenced epoch or verified archive head"
        },
        "cases": cases
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    directory.close()?;
    Ok(())
}

#[cfg(test)]
#[path = "ha_reader_apply_tests.rs"]
mod tests;
