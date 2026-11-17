//! ITEM-002: local, synthetic reader-strategy measurements (MEAS-001/002/007).
//! Local files only; not an archive, Blob or ACA qualification.
//! Protocol and measurement boundaries: benchmarks/ha-reader-apply.md.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use serde_json::{Map, Value, json};
use vlite::{
    Database, Durability, FormatVersion, HnswConfig, LogSegment, Mutation, ReplicaFollower,
    maintenance,
};

use super::{BoxResult, command_output, nanos_since, require, timed, verify_equivalence};

pub(crate) const INDEX: HnswConfig = HnswConfig {
    m: 16,
    ef_construction: 64,
    seed: 42,
};
pub(crate) const QUERY_EF: usize = 512;
const PAYLOAD_BYTES: usize = 64;
const INGEST_BATCH: u64 = 1_000;
const MAX_SEGMENT_BYTES: usize = 256 * 1024 * 1024;

const MEASURE_DIMENSIONS: usize = 64;
const MEASURE_SIZES: [u64; 3] = [1_000, 10_000, 100_000];
const MEASURE_DELTAS: [u64; 2] = [100, 1_000];
const MEASURE_REPS: usize = 5;
const STALENESS_RECORDS: u64 = 10_000;
const STALENESS_INTERVALS_MS: [u64; 2] = [250, 1_000];
const STALENESS_DURATION_MS: u64 = 15_000;
const STALENESS_TARGET_RATE: u64 = 200;

const USAGE: &str =
    "usage: cargo run --locked --features json --example ha-reader-apply -- (--smoke | --measure)";

pub(crate) enum Mode {
    Smoke,
    Measure,
}

pub(crate) fn parse_mode(args: &[String]) -> BoxResult<Mode> {
    match args {
        [only] if only == "--smoke" => Ok(Mode::Smoke),
        [only] if only == "--measure" => Ok(Mode::Measure),
        _ => Err(USAGE.into()),
    }
}

// ------------------------------------------------------------------ summaries

pub(crate) struct Summary {
    pub min: u64,
    pub median: u64,
    pub max: u64,
}

/// Nearest-rank percentile of an ascending, non-empty slice.
pub(crate) fn percentile(sorted: &[u64], p: f64) -> u64 {
    let rank = (p * sorted.len() as f64 / 100.0).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// Median is nearest-rank p50 (the lower middle for even counts).
pub(crate) fn summarize(values: &[u64]) -> Option<Summary> {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    Some(Summary {
        min: *sorted.first()?,
        median: percentile(&sorted, 50.0),
        max: *sorted.last()?,
    })
}

pub(crate) fn summary_json(values: &[u64]) -> Value {
    match summarize(values) {
        Some(summary) => json!({
            "raw": values,
            "min": summary.min,
            "median": summary.median,
            "max": summary.max
        }),
        None => json!({"raw": values}),
    }
}

// ------------------------------------------------------------- byte sampling

/// Sum of logical regular-file lengths below `path` (not allocated blocks).
pub(crate) fn dir_bytes(path: &Path) -> BoxResult<u64> {
    let mut total = 0;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        total += if entry.file_type()?.is_dir() {
            dir_bytes(&entry.path())?
        } else {
            // Opens the file: Windows directory entries can lag open writers.
            fs::metadata(entry.path())?.len()
        };
    }
    Ok(total)
}

fn optional_dir_bytes(path: &Path) -> BoxResult<u64> {
    if path.exists() {
        dir_bytes(path)
    } else {
        Ok(0)
    }
}

/// Phase-boundary samples of the reader directory and, separately, the
/// source-side shipping directory (a test colocation cost).
#[derive(Default)]
pub(crate) struct HighWater {
    reader_peak: u64,
    reader_phase: String,
    ship_peak: u64,
    samples: Vec<Value>,
}

impl HighWater {
    pub(crate) fn sample(&mut self, phase: &str, reader: &Path, ship: &Path) -> BoxResult<()> {
        let reader_bytes = optional_dir_bytes(reader)?;
        let ship_bytes = optional_dir_bytes(ship)?;
        if self.samples.is_empty() || reader_bytes > self.reader_peak {
            self.reader_peak = reader_bytes;
            self.reader_phase = phase.to_owned();
        }
        self.ship_peak = self.ship_peak.max(ship_bytes);
        self.samples.push(json!({
            "phase": phase,
            "reader_bytes": reader_bytes,
            "ship_bytes": ship_bytes
        }));
        Ok(())
    }

    pub(crate) fn to_json(&self) -> Value {
        json!({
            "reader_peak_bytes": self.reader_peak,
            "reader_peak_phase": self.reader_phase,
            "ship_peak_bytes": self.ship_peak,
            "samples": self.samples
        })
    }
}

/// Runs `operation` while a background thread polls `directory` about every
/// millisecond; returns the operation's value and the sampled peak bytes.
pub(crate) fn poll_peak<T>(
    directory: &Path,
    operation: impl FnOnce() -> BoxResult<T>,
) -> BoxResult<(T, u64)> {
    let stop = AtomicBool::new(false);
    let peak = AtomicU64::new(dir_bytes(directory)?);
    let result = thread::scope(|scope| {
        scope.spawn(|| {
            while !stop.load(Ordering::Acquire) {
                // Files may vanish mid-scan; a failed scan is just a missed sample.
                if let Ok(bytes) = dir_bytes(directory) {
                    peak.fetch_max(bytes, Ordering::Relaxed);
                }
                thread::sleep(Duration::from_millis(1));
            }
        });
        let result = operation();
        stop.store(true, Ordering::Release);
        result
    });
    let value = result?;
    peak.fetch_max(dir_bytes(directory)?, Ordering::Relaxed);
    Ok((value, peak.load(Ordering::Relaxed)))
}

/// Harness-only work inside an inclusive interval; its time is subtracted.
fn excluded<T>(total: &mut u64, operation: impl FnOnce() -> BoxResult<T>) -> BoxResult<T> {
    let (value, elapsed) = timed(operation)?;
    *total += elapsed;
    Ok(value)
}

// ------------------------------------------------------------------ fixture

type Model = BTreeMap<u64, (Vec<f32>, Vec<u8>)>;

fn splitmix(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

/// Deterministic pseudo-random vectors: distinct per (id, version), so an
/// exact nearest neighbour of a stored vector is unique.
fn vector(id: u64, version: u64, dimensions: usize) -> Vec<f32> {
    (0..dimensions as u64)
        .map(|dimension| {
            let seed = id.wrapping_mul(0x1_0000_0001) ^ (version << 48) ^ dimension;
            (splitmix(seed) >> 40) as f32 / (1u64 << 24) as f32
        })
        .collect()
}

fn payload(id: u64, version: u64) -> Vec<u8> {
    let mut bytes = vec![b'p'; PAYLOAD_BYTES];
    bytes[..8].copy_from_slice(&id.to_le_bytes());
    bytes[8..16].copy_from_slice(&version.to_le_bytes());
    bytes
}

/// The database's head revision (works on read-only and follower handles).
pub(crate) fn head_sequence(database: &Database) -> BoxResult<u64> {
    Ok(database.export_log(0, 1)?.head_sequence)
}

fn fresh_dir(path: PathBuf) -> BoxResult<PathBuf> {
    if path.exists() {
        fs::remove_dir_all(&path)?;
    }
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn ingest(path: &Path, records: u64, dimensions: usize) -> BoxResult<(Database, Model)> {
    let mut database = Database::create_with_format(path, dimensions, FormatVersion::V5)?;
    database.set_durability(Durability::Full)?;
    let mut model = Model::new();
    let mut start = 0;
    while start < records {
        let end = (start + INGEST_BATCH).min(records);
        let batch: Vec<Mutation> = (start..end)
            .map(|id| Mutation::Put {
                id,
                vector: vector(id, 0, dimensions),
                payload: payload(id, 0),
            })
            .collect();
        database.write_batch(&batch)?;
        for id in start..end {
            model.insert(id, (vector(id, 0, dimensions), payload(id, 0)));
        }
        start = end;
    }
    Ok((database, model))
}

/// The latest full baseline (archive stand-in), shared by every repetition.
pub(crate) struct Baseline {
    path: PathBuf,
    records: u64,
    dimensions: usize,
    pub sequence: u64,
    lineage: u64,
    bytes: u64,
    model: Model,
}

pub(crate) fn prepare_baseline(
    directory: &Path,
    records: u64,
    dimensions: usize,
) -> BoxResult<Baseline> {
    let path = directory.join("baseline.vlite");
    let (database, model) = ingest(&path, records, dimensions)?;
    let sequence = head_sequence(&database)?;
    let lineage = database.lineage().ok_or("baseline has no lineage")?;
    database.close()?;
    Ok(Baseline {
        bytes: fs::metadata(&path)?.len(),
        path,
        records,
        dimensions,
        sequence,
        lineage,
        model,
    })
}

/// One repetition: a fresh source at the baseline plus `delta` mutations.
pub(crate) struct Rep {
    root: PathBuf,
    pub source: PathBuf,
    baseline: PathBuf,
    baseline_sequence: u64,
    lineage: u64,
    pub expected_sequence: u64,
    pub id_bound: u64,
    model: Model,
}

impl Rep {
    #[cfg(test)]
    pub(crate) fn corrupt_model_for_test(&mut self) {
        let (_, (_, payload)) = self.model.iter_mut().next().unwrap();
        payload[PAYLOAD_BYTES - 1] ^= 1;
    }
}

/// Mixed delta: for k = i / 3, update ID 2k, delete ID 2k+1, insert ID
/// records+k; each mutation is one independently acknowledged commit.
pub(crate) fn prepare_rep(
    directory: &Path,
    baseline: &Baseline,
    delta: u64,
    rep: usize,
) -> BoxResult<Rep> {
    let records = baseline.records;
    require(
        delta > 0 && 2 * (delta / 3) + 2 <= records,
        "delta too large for fixture",
    )?;
    let root = fresh_dir(directory.join(format!("delta-{delta}-rep-{rep}")))?;
    let source = fresh_dir(root.join("source"))?.join("source.vlite");
    maintenance::backup(&baseline.path, &source)?;
    let mut database = Database::open(&source)?;
    database.set_durability(Durability::Full)?;
    let mut model = baseline.model.clone();
    let version = rep as u64 + 1;
    for index in 0..delta {
        let k = index / 3;
        match index % 3 {
            0 => {
                let (id, vector, payload) = (
                    2 * k,
                    vector(2 * k, version, baseline.dimensions),
                    payload(2 * k, version),
                );
                database.put(id, &vector, &payload)?;
                model.insert(id, (vector, payload));
            }
            1 => {
                require(database.delete(2 * k + 1)?, "delete target missing")?;
                model.remove(&(2 * k + 1));
            }
            _ => {
                let id = records + k;
                let (vector, payload) = (
                    vector(id, version, baseline.dimensions),
                    payload(id, version),
                );
                database.put(id, &vector, &payload)?;
                model.insert(id, (vector, payload));
            }
        }
    }
    let expected_sequence = head_sequence(&database)?;
    require(
        expected_sequence == baseline.sequence + delta,
        "one commit per mutation expected",
    )?;
    database.close()?;
    Ok(Rep {
        root,
        source,
        baseline: baseline.path.clone(),
        baseline_sequence: baseline.sequence,
        lineage: baseline.lineage,
        expected_sequence,
        id_bound: records + delta,
        model,
    })
}

// ------------------------------------------------------------- verification

fn verify_data(rep: &Rep, database: &Database) -> BoxResult<()> {
    require(database.len() == rep.model.len(), "record count mismatch")?;
    for (id, (vector, payload)) in &rep.model {
        let record = database.get(*id)?.ok_or("record missing on reader")?;
        require(
            &record.vector == vector && &record.payload == payload,
            "record content mismatch",
        )?;
    }
    Ok(())
}

fn verify_position(rep: &Rep, database: &Database) -> BoxResult<()> {
    require(database.lineage() == Some(rep.lineage), "lineage mismatch")?;
    require(
        head_sequence(database)? == rep.expected_sequence,
        "reader sequence mismatch",
    )
}

/// ID 0 is always updated by the delta; the index must return it.
fn verify_ann(rep: &Rep, database: &Database) -> BoxResult<()> {
    let (query, _) = rep.model.get(&0).ok_or("query record missing")?;
    let exact = database.search(query, 1)?;
    let approximate = database.search_approximate_ready(query, 1, QUERY_EF, INDEX)?;
    if exact.len() == 1 && exact[0].id == 0 && exact == approximate {
        return Ok(());
    }
    Err(
        format!("ANN query differs from exact result: exact={exact:?} approximate={approximate:?}")
            .into(),
    )
}

fn verify_reader(rep: &Rep, database: &Database) -> BoxResult<()> {
    verify_data(rep, database)?;
    verify_position(rep, database)?;
    verify_ann(rep, database)
}

// --------------------------------------------------------------- strategies

#[derive(Clone, Copy)]
pub(crate) enum Strategy {
    SnapshotRefresh,
    IncrementalApply,
    BaselineReplacement,
}

impl Strategy {
    const ALL: [Self; 3] = [
        Self::SnapshotRefresh,
        Self::IncrementalApply,
        Self::BaselineReplacement,
    ];

    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::SnapshotRefresh => "a_snapshot_refresh",
            Self::IncrementalApply => "b_incremental_apply",
            Self::BaselineReplacement => "c_baseline_replacement",
        }
    }

    fn run(self, rep: &Rep) -> BoxResult<(Database, Value)> {
        match self {
            Self::SnapshotRefresh => snapshot_refresh(rep),
            Self::IncrementalApply => incremental_apply(rep),
            Self::BaselineReplacement => baseline_replacement(rep),
        }
    }
}

fn bytes_report(high_water: &HighWater, reader_file: &Path) -> BoxResult<Value> {
    let reader_file_bytes = fs::metadata(reader_file)?.len();
    let mut report = high_water.to_json();
    report["reader_file_bytes"] = json!(reader_file_bytes);
    report["reader_peak_to_reader_file_ratio_derived"] =
        json!(high_water.reader_peak as f64 / reader_file_bytes as f64);
    Ok(report)
}

fn export_tail(source: &Database, after: u64, rep: &Rep) -> BoxResult<LogSegment> {
    let segment = source.export_log(after, MAX_SEGMENT_BYTES)?;
    require(
        segment.last_sequence == segment.head_sequence
            && segment.last_sequence == rep.expected_sequence,
        "tail did not fit one segment",
    )?;
    Ok(segment)
}

/// (a) Full source backup, reader-side materialization, read-only open,
/// index build, verify, then swap out the old reader.
pub(crate) fn snapshot_refresh(rep: &Rep) -> BoxResult<(Database, Value)> {
    let reader = fresh_dir(rep.root.join("reader-a"))?;
    let ship = fresh_dir(rep.root.join("ship-a"))?;
    let old_path = reader.join("reader-0.vlite");
    maintenance::backup(&rep.baseline, &old_path)?;
    let old = Database::open_read_only(&old_path)?;
    let mut high_water = HighWater::default();
    high_water.sample("prepared_old_reader", &reader, &ship)?;

    let mut harness = 0;
    let start = Instant::now();
    let snapshot = ship.join("snapshot.vlite");
    let (_, source_backup) = timed(|| Ok(maintenance::backup(&rep.source, &snapshot)?))?;
    excluded(&mut harness, || {
        high_water.sample("source_backup", &reader, &ship)
    })?;
    let new_path = reader.join("reader-1.vlite");
    let (_, reader_restore) = timed(|| Ok(maintenance::backup(&snapshot, &new_path)?))?;
    excluded(&mut harness, || {
        high_water.sample("reader_restore", &reader, &ship)
    })?;
    let snapshot_bytes = fs::metadata(&snapshot)?.len();
    let (_, ship_cleanup) = timed(|| Ok(fs::remove_file(&snapshot)?))?;
    let (mut database, open_read_only) = timed(|| Ok(Database::open_read_only(&new_path)?))?;
    excluded(&mut harness, || {
        high_water.sample("open_read_only", &reader, &ship)
    })?;
    let (_, build_hnsw) = timed(|| Ok(database.build_hnsw(INDEX)?))?;
    let verify = excluded(&mut harness, || {
        let (_, verify) = timed(|| verify_reader(rep, &database))?;
        high_water.sample("build_hnsw_verified", &reader, &ship)?;
        Ok(verify)
    })?;
    let (_, swap) = timed(|| {
        drop(old);
        Ok(fs::remove_file(&old_path)?)
    })?;
    let refresh_inclusive = nanos_since(start).saturating_sub(harness);
    high_water.sample("swapped", &reader, &ship)?;
    let report = json!({
        "strategy": Strategy::SnapshotRefresh.name(),
        "verified": true,
        "snapshot_bytes": snapshot_bytes,
        "ns": {
            "source_backup": source_backup,
            "reader_restore": reader_restore,
            "ship_cleanup": ship_cleanup,
            "open_read_only": open_read_only,
            "build_hnsw": build_hnsw,
            "swap": swap,
            "refresh_inclusive": refresh_inclusive,
            "harness_verify": verify
        },
        "bytes": bytes_report(&high_water, &new_path)?
    });
    Ok((database, report))
}

/// (b) Export the tail after the follower position, ship it as a file,
/// apply it, then (detached) reopen read-only and build the index.
pub(crate) fn incremental_apply(rep: &Rep) -> BoxResult<(Database, Value)> {
    let reader = fresh_dir(rep.root.join("reader-b"))?;
    let ship = fresh_dir(rep.root.join("ship-b"))?;
    let follower_path = reader.join("follower.vlite");
    maintenance::backup(&rep.baseline, &follower_path)?;
    let mut follower = ReplicaFollower::open(&follower_path, None)?;
    require(
        follower.applied_sequence() == rep.baseline_sequence,
        "follower not at baseline",
    )?;
    let source = Database::open_read_only(&rep.source)?;
    let mut high_water = HighWater::default();
    high_water.sample("prepared_follower", &reader, &ship)?;

    let mut harness = 0;
    let start = Instant::now();
    let (segment, export) = timed(|| export_tail(&source, follower.applied_sequence(), rep))?;
    let segment_path = reader.join("tail.vseg");
    let (_, save) = timed(|| Ok(segment.save(&segment_path)?))?;
    let serialized_bytes = excluded(&mut harness, || {
        high_water.sample("save", &reader, &ship)?;
        Ok(fs::metadata(&segment_path)?.len())
    })?;
    let (loaded, load_verify) = timed(|| Ok(LogSegment::load(&segment_path)?))?;
    excluded(&mut harness, || {
        require(loaded == segment, "shipping round-trip mismatch")
    })?;
    let (status, apply_inclusive) = timed(|| Ok(follower.apply(&loaded)?))?;
    require(
        status.applied_sequence == rep.expected_sequence && status.lag == 0,
        "applied position mismatch",
    )?;
    excluded(&mut harness, || high_water.sample("apply", &reader, &ship))?;
    let (_, segment_cleanup) = timed(|| Ok(fs::remove_file(&segment_path)?))?;
    let exact_visible_inclusive = nanos_since(start).saturating_sub(harness);
    let verify_exact = excluded(&mut harness, || {
        let (_, verify) = timed(|| {
            verify_data(rep, follower.db())?;
            verify_position(rep, follower.db())
        })?;
        drop(source);
        Ok(verify)
    })?;
    // No mutable follower database exists; the index is built on a detached
    // read-only handle while the follower is closed (not serving).
    let (_, follower_drop) = timed(|| {
        drop(follower);
        Ok(())
    })?;
    let (mut database, read_only_reopen) = timed(|| Ok(Database::open_read_only(&follower_path)?))?;
    let (_, build_hnsw) = timed(|| Ok(database.build_hnsw(INDEX)?))?;
    let index_ready_inclusive = nanos_since(start).saturating_sub(harness);
    let (_, verify_index) = timed(|| verify_reader(rep, &database))?;
    high_water.sample("build_hnsw_verified", &reader, &ship)?;
    let report = json!({
        "strategy": Strategy::IncrementalApply.name(),
        "verified": true,
        "in_place_index_maintenance": "UNAVAILABLE",
        "index_note": "ReplicaFollower drops in-memory indexes on apply and exposes only &Database; index-ready cost is a detached read-only reopen plus full build_hnsw while the follower is closed",
        "segment": {
            "commits": segment.last_sequence - segment.base_sequence,
            "frame_bytes": segment.frames.len(),
            "serialized_bytes": serialized_bytes
        },
        "ns": {
            "export": export,
            "save": save,
            "load_verify": load_verify,
            "apply_inclusive": apply_inclusive,
            "apply_fsync": null,
            "apply_replay": null,
            "segment_cleanup": segment_cleanup,
            "exact_visible_inclusive": exact_visible_inclusive,
            "follower_drop": follower_drop,
            "read_only_reopen": read_only_reopen,
            "build_hnsw": build_hnsw,
            "index_ready_inclusive": index_ready_inclusive,
            "harness_verify": verify_exact + verify_index
        },
        "bytes": bytes_report(&high_water, &follower_path)?
    });
    Ok((database, report))
}

/// (c) Build a brand-new reader from the latest full baseline plus the tail
/// since that baseline, then swap out the old (still open) reader.
pub(crate) fn baseline_replacement(rep: &Rep) -> BoxResult<(Database, Value)> {
    let reader = fresh_dir(rep.root.join("reader-c"))?;
    let ship = fresh_dir(rep.root.join("ship-c"))?;
    let old_path = reader.join("reader-0.vlite");
    maintenance::backup(&rep.baseline, &old_path)?;
    let old = Database::open_read_only(&old_path)?;
    let source = Database::open_read_only(&rep.source)?;
    let mut high_water = HighWater::default();
    high_water.sample("prepared_old_reader", &reader, &ship)?;

    let mut harness = 0;
    let start = Instant::now();
    let new_path = reader.join("reader-1.vlite");
    let (_, baseline_restore) = timed(|| Ok(maintenance::backup(&rep.baseline, &new_path)?))?;
    excluded(&mut harness, || {
        high_water.sample("baseline_restore", &reader, &ship)
    })?;
    let (segment, export) = timed(|| export_tail(&source, rep.baseline_sequence, rep))?;
    let segment_path = reader.join("tail.vseg");
    let (_, save) = timed(|| Ok(segment.save(&segment_path)?))?;
    excluded(&mut harness, || high_water.sample("save", &reader, &ship))?;
    let (mut follower, follower_open) = timed(|| Ok(ReplicaFollower::open(&new_path, None)?))?;
    let (loaded, load_verify) = timed(|| Ok(LogSegment::load(&segment_path)?))?;
    excluded(&mut harness, || {
        require(loaded == segment, "shipping round-trip mismatch")
    })?;
    let (status, apply_inclusive) = timed(|| Ok(follower.apply(&loaded)?))?;
    require(
        status.applied_sequence == rep.expected_sequence,
        "applied position mismatch",
    )?;
    excluded(&mut harness, || high_water.sample("apply", &reader, &ship))?;
    let (_, segment_cleanup) = timed(|| Ok(fs::remove_file(&segment_path)?))?;
    let (_, follower_drop) = timed(|| {
        drop(follower);
        Ok(())
    })?;
    let (mut database, read_only_reopen) = timed(|| Ok(Database::open_read_only(&new_path)?))?;
    let (_, build_hnsw) = timed(|| Ok(database.build_hnsw(INDEX)?))?;
    let verify = excluded(&mut harness, || {
        let (_, verify) = timed(|| verify_reader(rep, &database))?;
        drop(source);
        high_water.sample("build_hnsw_verified", &reader, &ship)?;
        Ok(verify)
    })?;
    let (_, swap) = timed(|| {
        drop(old);
        Ok(fs::remove_file(&old_path)?)
    })?;
    let refresh_inclusive = nanos_since(start).saturating_sub(harness);
    high_water.sample("swapped", &reader, &ship)?;
    let report = json!({
        "strategy": Strategy::BaselineReplacement.name(),
        "verified": true,
        "tail_commits": segment.last_sequence - segment.base_sequence,
        "tail_frame_bytes": segment.frames.len(),
        "ns": {
            "baseline_restore": baseline_restore,
            "export": export,
            "save": save,
            "follower_open": follower_open,
            "load_verify": load_verify,
            "apply_inclusive": apply_inclusive,
            "apply_fsync": null,
            "apply_replay": null,
            "segment_cleanup": segment_cleanup,
            "follower_drop": follower_drop,
            "read_only_reopen": read_only_reopen,
            "build_hnsw": build_hnsw,
            "swap": swap,
            "refresh_inclusive": refresh_inclusive,
            "harness_verify": verify
        },
        "bytes": bytes_report(&high_water, &new_path)?
    });
    Ok((database, report))
}

/// MEAS-007: offline compaction of a reader-size copy of the converged state.
pub(crate) fn measure_compaction(rep: &Rep) -> BoxResult<Value> {
    let directory = fresh_dir(rep.root.join("compaction"))?;
    let input = directory.join("reader.vlite");
    maintenance::backup(&rep.source, &input)?;
    let input_bytes = fs::metadata(&input)?.len();
    let output = directory.join("compacted.vlite");
    let ((report, compact_ns), sampled_peak_bytes) = poll_peak(&directory, || {
        timed(|| Ok(maintenance::compact(&input, &output)?))
    })?;
    let output_bytes = fs::metadata(&output)?.len();
    let bytes_after = dir_bytes(&directory)?;
    let compacted = Database::open_read_only(&output)?;
    verify_data(rep, &compacted)?;
    drop(compacted);
    Ok(json!({
        "input_bytes": input_bytes,
        "output_bytes": output_bytes,
        "input_live_records": report.source.live_records,
        "output_live_records": report.output.live_records,
        "sampled_peak_bytes": sampled_peak_bytes,
        "bytes_after_both_files_present": bytes_after,
        "compact_ns": compact_ns,
        "sampled_peak_to_input_ratio_derived": sampled_peak_bytes as f64 / input_bytes as f64,
        "output_verified": true
    }))
}

/// Runs every strategy for one repetition, rotating the order by `rep` to
/// spread host drift across strategies.
fn run_rep(
    directory: &Path,
    baseline: &Baseline,
    delta: u64,
    rep: usize,
    compact: bool,
) -> BoxResult<Value> {
    let state = prepare_rep(directory, baseline, delta, rep)?;
    let order: Vec<Strategy> = (0..3).map(|i| Strategy::ALL[(rep + i) % 3]).collect();
    let mut strategies = Map::new();
    for strategy in &order {
        let (reader, report) = strategy.run(&state)?;
        drop(reader);
        strategies.insert(strategy.name().to_owned(), report);
    }
    let compaction = if compact {
        measure_compaction(&state)?
    } else {
        Value::Null
    };
    let source_bytes = fs::metadata(&state.source)?.len();
    fs::remove_dir_all(&state.root)?;
    Ok(json!({
        "rep": rep,
        "order": order.iter().map(Strategy::name).collect::<Vec<_>>(),
        "source_bytes_colocated": source_bytes,
        "expected_sequence": state.expected_sequence,
        "id_bound": state.id_bound,
        "strategies": strategies,
        "compaction": compaction
    }))
}

fn summarize_reps(reps: &[Value]) -> Value {
    let mut summary = Map::new();
    for strategy in Strategy::ALL {
        let mut fields = Map::new();
        for group in ["ns", "bytes"] {
            let Some(first) = reps[0]["strategies"][strategy.name()][group].as_object() else {
                continue;
            };
            for key in first.keys() {
                let values: Option<Vec<u64>> = reps
                    .iter()
                    .map(|rep| rep["strategies"][strategy.name()][group][key].as_u64())
                    .collect();
                if let Some(values) = values {
                    fields.insert(format!("{group}.{key}"), summary_json(&values));
                }
            }
        }
        summary.insert(strategy.name().to_owned(), Value::Object(fields));
    }
    Value::Object(summary)
}

// ---------------------------------------------------------------- MEAS-002

/// A writer thread commits at a target rate (Full durability) while this
/// thread ships and applies a segment every `interval_ms` (strategy (b)).
/// Records acknowledgement-to-exact-visible latency for every commit.
pub(crate) fn measure_staleness(
    directory: &Path,
    records: u64,
    interval_ms: u64,
    duration_ms: u64,
    target_rate: u64,
) -> BoxResult<Value> {
    let root = fresh_dir(directory.join(format!("staleness-{interval_ms}ms")))?;
    let source_path = root.join("source.vlite");
    let follower_path = root.join("follower.vlite");
    let (database, _) = ingest(&source_path, records, MEASURE_DIMENSIONS)?;
    database.close()?;
    maintenance::backup(&source_path, &follower_path)?;
    let mut database = Database::open(&source_path)?;
    database.set_durability(Durability::Full)?;
    let base_sequence = head_sequence(&database)?;
    let mut follower = ReplicaFollower::open(&follower_path, None)?;
    require(
        follower.applied_sequence() == base_sequence,
        "follower not at baseline",
    )?;

    let writer = Mutex::new(database);
    let acks: Mutex<Vec<Instant>> = Mutex::new(Vec::new());
    let done = AtomicBool::new(false);
    let period = Duration::from_nanos(1_000_000_000 / target_rate);
    let duration = Duration::from_millis(duration_ms);
    let interval = Duration::from_millis(interval_ms);
    let segment_path = root.join("tail.vseg");

    let (written, latencies, apply_ns, ship_commits) = thread::scope(|scope| {
        let writer_thread = scope.spawn(|| {
            let result = (|| -> Result<(u64, u64), String> {
                let start = Instant::now();
                let mut k = 0u64;
                while start.elapsed() < duration {
                    let due = start + period * k as u32;
                    let now = Instant::now();
                    if due > now {
                        thread::sleep(due - now);
                    }
                    let id = if k.is_multiple_of(2) {
                        (k / 2) % records
                    } else {
                        records + k
                    };
                    let mut database = writer.lock().map_err(|e| e.to_string())?;
                    database
                        .put(
                            id,
                            &vector(id, k + 1, MEASURE_DIMENSIONS),
                            &payload(id, k + 1),
                        )
                        .map_err(|e| e.to_string())?;
                    // Recorded under the writer lock, so every exported
                    // commit already has its acknowledgement instant.
                    acks.lock().map_err(|e| e.to_string())?.push(Instant::now());
                    drop(database);
                    k += 1;
                }
                Ok((k, nanos_since(start)))
            })();
            done.store(true, Ordering::Release);
            result
        });

        let shipper = (|| -> BoxResult<(Vec<u64>, Vec<u64>, Vec<u64>)> {
            let mut latencies = Vec::new();
            let mut apply_ns = Vec::new();
            let mut ship_commits = Vec::new();
            let mut next = Instant::now() + interval;
            loop {
                let now = Instant::now();
                if next > now {
                    thread::sleep(next - now);
                }
                next = (next + interval).max(Instant::now());
                let finished = done.load(Ordering::Acquire);
                let segment = writer
                    .lock()
                    .map_err(|e| e.to_string())?
                    .export_log(follower.applied_sequence(), MAX_SEGMENT_BYTES)?;
                if !segment.is_empty() {
                    segment.save(&segment_path)?;
                    let loaded = LogSegment::load(&segment_path)?;
                    let (status, apply) = timed(|| Ok(follower.apply(&loaded)?))?;
                    let visible = Instant::now();
                    require(
                        status.applied_sequence == segment.last_sequence,
                        "applied position mismatch",
                    )?;
                    fs::remove_file(&segment_path)?;
                    apply_ns.push(apply);
                    ship_commits.push(segment.last_sequence - segment.base_sequence);
                    let acks = acks.lock().map_err(|e| e.to_string())?;
                    for sequence in segment.base_sequence + 1..=segment.last_sequence {
                        let ack = acks[(sequence - base_sequence - 1) as usize];
                        latencies.push(visible.saturating_duration_since(ack).as_nanos() as u64);
                    }
                    drop(acks);
                    // Exact visibility spot check of the newest commit.
                    let k = segment.last_sequence - base_sequence - 1;
                    let id = if k.is_multiple_of(2) {
                        (k / 2) % records
                    } else {
                        records + k
                    };
                    let record = follower.db().get(id)?.ok_or("shipped record missing")?;
                    require(
                        record.vector == vector(id, k + 1, MEASURE_DIMENSIONS)
                            && record.payload == payload(id, k + 1),
                        "shipped record mismatch",
                    )?;
                }
                if finished && segment.last_sequence == segment.head_sequence {
                    return Ok((latencies, apply_ns, ship_commits));
                }
            }
        })();
        let written = writer_thread
            .join()
            .map_err(|_| "writer thread panicked")?
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() });
        let (latencies, apply_ns, ship_commits) = shipper?;
        Ok::<_, Box<dyn std::error::Error>>((written?, latencies, apply_ns, ship_commits))
    })?;
    let (commits, writer_elapsed_ns) = written;
    let database = writer.into_inner().map_err(|e| e.to_string())?;
    require(latencies.len() as u64 == commits, "latency count mismatch")?;
    verify_equivalence(&database, follower.db(), records + commits + 1)?;
    let final_follower_bytes = fs::metadata(&follower_path)?.len();
    drop(follower);
    database.close()?;
    fs::remove_dir_all(&root)?;
    let mut sorted = latencies.clone();
    sorted.sort_unstable();
    Ok(json!({
        "interval_ms": interval_ms,
        "duration_ms": duration_ms,
        "target_commits_per_second": target_rate,
        "baseline_records": records,
        "commits": commits,
        "writer_elapsed_ns": writer_elapsed_ns,
        "achieved_commits_per_second": commits as f64 * 1e9 / writer_elapsed_ns as f64,
        "ships": apply_ns.len(),
        "apply_inclusive_ns": summary_json(&apply_ns),
        "commits_per_ship": summary_json(&ship_commits),
        "latency_ns": {
            "count": sorted.len(),
            "min": sorted.first(),
            "p50": percentile(&sorted, 50.0),
            "p90": percentile(&sorted, 90.0),
            "p99": percentile(&sorted, 99.0),
            "max": sorted.last()
        },
        "final_follower_bytes": final_follower_bytes,
        "final_verified": true
    }))
}

// -------------------------------------------------------------------- report

pub(crate) fn measure() -> BoxResult<()> {
    let started = Instant::now();
    let directory = tempfile::tempdir()?;
    let mut cells = Vec::new();
    let mut compactions = Vec::new();
    for records in MEASURE_SIZES {
        let size_dir = fresh_dir(directory.path().join(format!("records-{records}")))?;
        let (baseline, prepare_ns) =
            timed(|| prepare_baseline(&size_dir, records, MEASURE_DIMENSIONS))?;
        for delta in MEASURE_DELTAS {
            let mut reps = Vec::new();
            for rep in 0..MEASURE_REPS {
                let compact =
                    delta == MEASURE_DELTAS[MEASURE_DELTAS.len() - 1] && rep == MEASURE_REPS - 1;
                let result = run_rep(&size_dir, &baseline, delta, rep, compact)?;
                if compact {
                    compactions.push(json!({
                        "records": records,
                        "delta": delta,
                        "compaction": result["compaction"]
                    }));
                }
                reps.push(result);
                eprintln!(
                    "records={records} delta={delta} rep={rep} elapsed_s={:.1}",
                    started.elapsed().as_secs_f64()
                );
            }
            cells.push(json!({
                "records": records,
                "delta": delta,
                "baseline": {
                    "sequence": baseline.sequence,
                    "bytes": baseline.bytes,
                    "prepare_ns": prepare_ns
                },
                "summary": summarize_reps(&reps),
                "reps": reps
            }));
        }
        drop(baseline);
        fs::remove_dir_all(&size_dir)?;
    }
    let mut staleness = Vec::new();
    for interval in STALENESS_INTERVALS_MS {
        staleness.push(measure_staleness(
            directory.path(),
            STALENESS_RECORDS,
            interval,
            STALENESS_DURATION_MS,
            STALENESS_TARGET_RATE,
        )?);
        eprintln!(
            "staleness interval_ms={interval} elapsed_s={:.1}",
            started.elapsed().as_secs_f64()
        );
    }
    let porcelain = command_output("git", &["status", "--porcelain"])?;
    let report = json!({
        "schema_version": 2,
        "scope": "SYNTHETIC_LOCAL_MEASUREMENT_NOT_ACA_SLO",
        "source": {
            "revision": command_output("git", &["rev-parse", "HEAD"])?,
            "working_tree_dirty": !porcelain.is_empty(),
            "dirty_paths": porcelain.lines().collect::<Vec<_>>(),
            "package_version": env!("CARGO_PKG_VERSION"),
            "rustc": command_output("rustc", &["--version"])?,
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "logical_cpus": thread::available_parallelism()?.get(),
            "build_profile": if cfg!(debug_assertions) { "debug" } else { "release" },
            "temporary_filesystem_location": directory.path().display().to_string(),
            "host_note": "shared developer host; other sessions and builds may run concurrently"
        },
        "parameters": {
            "source_format": "V5 single-file, unencrypted, no commit timestamps",
            "dimensions": MEASURE_DIMENSIONS,
            "payload_bytes": PAYLOAD_BYTES,
            "fixture": "splitmix64 pseudo-random vectors in [0,1); one collection",
            "baseline_ingest": format!("write_batch of {INGEST_BATCH} records per commit, Full durability"),
            "sizes_records": MEASURE_SIZES,
            "deltas_mutations": MEASURE_DELTAS,
            "delta_mix": "per k=i/3: update ID 2k, delete ID 2k+1, insert ID records+k; one Full-durability commit each",
            "reps": MEASURE_REPS,
            "interleaving": "strategy order rotated by rep: (a,b,c), (b,c,a), (c,a,b), ...",
            "hnsw": {"m": INDEX.m, "ef_construction": INDEX.ef_construction, "seed": INDEX.seed,
                "metric": "squared_l2", "quantization": "none", "query_k": 1, "query_ef": QUERY_EF},
            "max_segment_bytes": MAX_SEGMENT_BYTES,
            "staleness": {
                "baseline_records": STALENESS_RECORDS,
                "intervals_ms": STALENESS_INTERVALS_MS,
                "duration_ms": STALENESS_DURATION_MS,
                "target_commits_per_second": STALENESS_TARGET_RATE,
                "writer_durability": "Full",
                "writer_mix": "even k updates ID (k/2) mod records, odd k inserts ID records+k"
            },
            "cache": "uncontrolled warm local OS cache; no cold-cache claim"
        },
        "boundaries": {
            "a.refresh_inclusive": "source maintenance::backup (writer closed) + reader-side maintenance::backup copy + ship cleanup + open_read_only + build_hnsw + swap; excludes harness verification and byte sampling",
            "b.exact_visible_inclusive": "export_log + LogSegment::save + load/verify + ReplicaFollower::apply + segment cleanup; excludes harness work",
            "b.index_ready_inclusive": "exact_visible_inclusive + follower drop + detached open_read_only + full build_hnsw; the follower is closed meanwhile; NOT in-place index maintenance",
            "c.refresh_inclusive": "baseline copy + export_log + save + ReplicaFollower::open + load/verify + apply + cleanup + follower drop + open_read_only + build_hnsw + swap; excludes harness work",
            "reader_restore": "maintenance::backup (byte-identical, keeps lineage) stands in for download+restore; maintenance::restore assigns a new lineage, which the follower in (c) would refuse",
            "apply_fsync_and_replay": "null: inside one engine call, not separately instrumented",
            "bytes": "logical file lengths sampled at phase boundaries (compaction: ~1 ms background polling); NOT allocated blocks, RSS or transient staging between samples",
            "staleness": "local filesystem, one process; ack = put return under the writer lock; visible = apply return; excludes Blob upload, durable archive head, ACA scheduling/network; not an SLO"
        },
        "unmeasured": {
            "blob_upload_and_durable_head": "UNMEASURED: ObjectStore is server-internal; no network/Blob use authorized",
            "aca_staleness_distribution": "UNMEASURED: local filesystem only",
            "aca_capacity_and_vcpu_tiers": "UNMEASURED: no ACA deployment",
            "apply_fsync_and_replay_separately": "UNMEASURED: no engine instrumentation",
            "process_peak_memory": "UNMEASURED: no existing dependency exposes it",
            "in_place_incremental_index_maintenance": "UNAVAILABLE: ReplicaFollower drops indexes on apply and exposes only &Database",
            "staleness_for_a_and_c": "UNMEASURED: no staleness distribution or bound for (a)/(c); refresh cost is not per-commit staleness"
        },
        "meas_001_007_cells": cells,
        "meas_007_compaction": compactions,
        "meas_002_staleness_b": staleness,
        "total_elapsed_ns": nanos_since(started)
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    directory.close()?;
    Ok(())
}
