//! Restart measurement: reopen + `load_ann_snapshot` versus reopen + rebuild.
//!
//! ```text
//! cargo run --release --example ann_restart_bench -- \
//!     [--vectors vectors.txt --queries queries.txt --count N --query-count Q --dims D] \
//!     [--manifest dataset-manifest.json] [--metric cosine|l2] [--quantization none|u8] \
//!     [--k 10] [--ef 100] [--m 16] [--ef-construction 100] [--rebuild-trials 3] [--workdir DIR]
//! ```
//!
//! Input files are whitespace-separated f32 rows (the `benchmarks/prepare_glove.py`
//! export). Rows, dimensions and total bytes are bounded and checked before
//! use; no rows are filtered. Without `--vectors`, deterministic synthetic data
//! is generated. All files are created in a fresh temporary directory (inside
//! `--workdir` if given) and removed afterwards. Prints one JSON object with
//! raw nanosecond timings, SHA-256 provenance of every input file, and whether
//! post-load hits are bit-identical (IDs and distances) to the pre-save hits.
//! Exits nonzero if the snapshot is not loaded or any hit differs.

use std::{
    fmt::Write as _,
    fs::File,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    time::Instant,
};

use sha2::{Digest, Sha256};
use vlite::{
    AnnOptions, AnnSnapshotLoad, Database, DistanceMetric, HnswConfig, MAX_BATCH_BYTES,
    MAX_BATCH_OPERATIONS, MAX_DIMENSIONS, Mutation, Quantization,
};

type BoxResult<T> = Result<T, Box<dyn std::error::Error>>;

const MAX_RECORDS: usize = 2_000_000;
const MAX_QUERIES: usize = 100_000;
const MAX_TRIALS: usize = 20;
/// Total text bytes accepted per input file.
const MAX_TEXT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Decoded vector bytes accepted for records plus queries.
const MAX_VECTOR_BYTES: usize = 4 * 1024 * 1024 * 1024;

struct Args {
    vectors: Option<PathBuf>,
    queries: Option<PathBuf>,
    manifest: Option<PathBuf>,
    workdir: Option<PathBuf>,
    count: usize,
    query_count: usize,
    dims: usize,
    metric: DistanceMetric,
    quantization: Quantization,
    k: usize,
    ef: usize,
    config: HnswConfig,
    trials: usize,
}

fn parse_args() -> BoxResult<Args> {
    let mut args = Args {
        vectors: None,
        queries: None,
        manifest: None,
        workdir: None,
        count: 5_000,
        query_count: 100,
        dims: 32,
        metric: DistanceMetric::Cosine,
        quantization: Quantization::None,
        k: 10,
        ef: 100,
        config: HnswConfig::default(),
        trials: 3,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(flag) = iter.next() {
        let value = iter
            .next()
            .ok_or_else(|| format!("{flag} requires a value"))?;
        let number = || {
            value
                .parse::<usize>()
                .map_err(|_| format!("{flag}: invalid number"))
        };
        match flag.as_str() {
            "--vectors" => args.vectors = Some(value.into()),
            "--queries" => args.queries = Some(value.into()),
            "--manifest" => args.manifest = Some(value.into()),
            "--workdir" => args.workdir = Some(value.into()),
            "--count" => args.count = number()?,
            "--query-count" => args.query_count = number()?,
            "--dims" => args.dims = number()?,
            "--k" => args.k = number()?,
            "--ef" => args.ef = number()?,
            "--m" => args.config.m = number()?,
            "--ef-construction" => args.config.ef_construction = number()?,
            "--rebuild-trials" => args.trials = number()?,
            "--metric" => {
                args.metric = match value.as_str() {
                    "cosine" => DistanceMetric::Cosine,
                    "l2" => DistanceMetric::SquaredL2,
                    _ => return Err("--metric must be cosine or l2".into()),
                }
            }
            "--quantization" => {
                args.quantization = match value.as_str() {
                    "none" => Quantization::None,
                    "u8" => Quantization::ScalarU8,
                    _ => return Err("--quantization must be none or u8".into()),
                }
            }
            _ => return Err(format!("unknown argument {flag}").into()),
        }
    }
    if args.vectors.is_some() != args.queries.is_some() {
        return Err("--vectors and --queries must be given together".into());
    }
    if !(1..=MAX_RECORDS).contains(&args.count)
        || !(1..=MAX_QUERIES).contains(&args.query_count)
        || !(1..=MAX_DIMENSIONS).contains(&args.dims)
        || !(1..=MAX_TRIALS).contains(&args.trials)
        || args.k == 0
    {
        return Err("count, query-count, dims, k or rebuild-trials out of range".into());
    }
    if (args.count + args.query_count)
        .checked_mul(args.dims * 4)
        .is_none_or(|bytes| bytes > MAX_VECTOR_BYTES)
    {
        return Err("decoded vectors exceed the 4 GiB bound".into());
    }
    Ok(args)
}

/// Reads exactly `count` rows of `dims` finite f32 values, bounded in bytes.
fn load_vectors(path: &Path, count: usize, dims: usize) -> BoxResult<Vec<Vec<f32>>> {
    let limit = ((count * dims * 32 + count) as u64).min(MAX_TEXT_BYTES);
    let mut reader = BufReader::new(File::open(path)?.take(limit + 1));
    let mut rows = Vec::with_capacity(count);
    let mut line = String::new();
    let mut total = 0u64;
    loop {
        line.clear();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > limit {
            return Err(format!("{}: exceeds {limit} byte bound", path.display()).into());
        }
        if line.trim().is_empty() {
            continue;
        }
        if rows.len() == count {
            return Err(format!("{}: more than {count} rows", path.display()).into());
        }
        let row = line
            .split_whitespace()
            .take(dims + 1)
            .map(str::parse::<f32>)
            .collect::<Result<Vec<_>, _>>()?;
        if row.len() != dims || row.iter().any(|value| !value.is_finite()) {
            return Err(format!(
                "{}: row {} is not {dims} finite values",
                path.display(),
                rows.len()
            )
            .into());
        }
        rows.push(row);
    }
    if rows.len() != count {
        return Err(format!("{}: {} rows, expected {count}", path.display(), rows.len()).into());
    }
    Ok(rows)
}

fn synthetic(count: usize, dims: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut state = seed;
    let mut next = move || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) >> 40) as f32 / (1u64 << 23) as f32 - 1.0
    };
    (0..count)
        .map(|_| (0..dims).map(|_| next()).collect())
        .collect()
}

/// `(sha256 hex, bytes)` of a file, streamed.
fn file_sha256(path: &Path) -> BoxResult<(String, u64)> {
    let mut hasher = Sha256::new();
    let bytes = std::io::copy(&mut File::open(path)?, &mut hasher)?;
    let hex = hasher
        .finalize()
        .iter()
        .fold(String::new(), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        });
    Ok((hex, bytes))
}

fn json_string(value: &str) -> String {
    let mut out = String::from('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn nanos(start: Instant) -> u128 {
    start.elapsed().as_nanos()
}

type Hits = Vec<Vec<(u64, u64)>>;

fn run_queries(
    db: &Database,
    args: &Args,
    options: AnnOptions,
    queries: &[Vec<f32>],
) -> BoxResult<(Hits, u128)> {
    let start = Instant::now();
    let mut all = Vec::with_capacity(queries.len());
    for query in queries {
        let (hits, _) = db.search_approximate_ready_with_options(
            query,
            args.k,
            args.ef,
            args.config,
            options,
            &[],
            args.metric,
        )?;
        all.push(
            hits.iter()
                .map(|hit| (hit.id, hit.distance.to_bits()))
                .collect(),
        );
    }
    Ok((all, nanos(start)))
}

fn main() -> BoxResult<()> {
    let args = parse_args()?;
    let options = AnnOptions {
        quantization: args.quantization,
        ..AnnOptions::default()
    };

    let mut provenance = Vec::new();
    let start = Instant::now();
    let (vectors, queries, source) = match (&args.vectors, &args.queries) {
        (Some(vectors), Some(queries)) => {
            for (role, path) in [("vectors", vectors), ("queries", queries)] {
                provenance.push((role, path.clone(), file_sha256(path)?));
            }
            (
                load_vectors(vectors, args.count, args.dims)?,
                load_vectors(queries, args.query_count, args.dims)?,
                "files",
            )
        }
        _ => (
            synthetic(args.count, args.dims, 1),
            synthetic(args.query_count, args.dims, 2),
            "synthetic",
        ),
    };
    if let Some(manifest) = &args.manifest {
        provenance.push(("manifest", manifest.clone(), file_sha256(manifest)?));
    }
    let input_ns = nanos(start);

    let directory = match &args.workdir {
        Some(dir) => tempfile::tempdir_in(dir)?,
        None => tempfile::tempdir()?,
    };
    let path = directory.path().join("restart.vlite");
    let snapshot = Database::ann_snapshot_path(&path);

    let mut db = Database::create(&path, args.dims)?;
    let start = Instant::now();
    let per_batch = MAX_BATCH_OPERATIONS
        .min(MAX_BATCH_BYTES / (args.dims * 4 + 256))
        .max(1);
    for (chunk_index, chunk) in vectors.chunks(per_batch).enumerate() {
        let batch: Vec<Mutation> = chunk
            .iter()
            .enumerate()
            .map(|(offset, vector)| Mutation::Put {
                id: (chunk_index * per_batch + offset) as u64,
                vector: vector.clone(),
                payload: Vec::new(),
            })
            .collect();
        db.write_batch(&batch)?;
    }
    let insert_ns = nanos(start);

    let start = Instant::now();
    db.build_hnsw_with_options(args.config, args.metric, options)?;
    let build_ns = nanos(start);
    let (before, pre_query_ns) = run_queries(&db, &args, options, &queries)?;

    let start = Instant::now();
    let report = db.save_ann_snapshot(&snapshot)?;
    let save_ns = nanos(start);
    let start = Instant::now();
    db.close()?;
    let close_ns = nanos(start);

    let start = Instant::now();
    let mut db = Database::open(&path)?;
    let reopen_ns = nanos(start);
    let start = Instant::now();
    let outcome = db.load_ann_snapshot(&snapshot)?;
    let load_ns = nanos(start);
    let (after, post_query_ns) = match outcome {
        AnnSnapshotLoad::Loaded { .. } => run_queries(&db, &args, options, &queries)?,
        _ => (Vec::new(), 0),
    };
    let mismatched = before.iter().zip(&after).filter(|(a, b)| a != b).count()
        + before.len().abs_diff(after.len());
    db.close()?;

    let mut rebuilds = Vec::with_capacity(args.trials);
    let mut rebuild_hits_equal = true;
    for _ in 0..args.trials {
        let start = Instant::now();
        let mut db = Database::open(&path)?;
        let open_ns = nanos(start);
        let start = Instant::now();
        db.build_hnsw_with_options(args.config, args.metric, options)?;
        let build_ns = nanos(start);
        let (hits, _) = run_queries(&db, &args, options, &queries)?;
        rebuild_hits_equal &= hits == before;
        db.close()?;
        rebuilds.push((open_ns, build_ns));
    }

    let mut json = String::from("{");
    let _ = write!(
        json,
        "\"benchmark\":\"ann_restart\",\"vlite_version\":{},\"profile\":{},\"os\":{},\"arch\":{},",
        json_string(env!("CARGO_PKG_VERSION")),
        json_string(if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }),
        json_string(std::env::consts::OS),
        json_string(std::env::consts::ARCH),
    );
    let _ = write!(
        json,
        "\"source\":{},\"provenance\":[{}],",
        json_string(source),
        provenance
            .iter()
            .map(|(role, path, (sha, bytes))| format!(
                "{{\"role\":{},\"path\":{},\"sha256\":{},\"bytes\":{bytes}}}",
                json_string(role),
                json_string(&path.display().to_string()),
                json_string(sha)
            ))
            .collect::<Vec<_>>()
            .join(",")
    );
    let _ = write!(
        json,
        "\"records\":{},\"queries\":{},\"dimensions\":{},\"metric\":{},\"quantization\":{},\"k\":{},\"ef_search\":{},\"m\":{},\"ef_construction\":{},\"seed\":{},",
        args.count,
        args.query_count,
        args.dims,
        json_string(if args.metric == DistanceMetric::Cosine {
            "cosine"
        } else {
            "l2"
        }),
        json_string(if args.quantization == Quantization::ScalarU8 {
            "u8"
        } else {
            "none"
        }),
        args.k,
        args.ef,
        args.config.m,
        args.config.ef_construction,
        args.config.seed,
    );
    let _ = write!(
        json,
        "\"snapshot_bytes\":{},\"load_outcome\":{},\"exact_hits_match\":{},\"mismatched_queries\":{mismatched},\"rebuild_hits_equal_pre_save\":{rebuild_hits_equal},",
        report.bytes,
        json_string(&match outcome {
            AnnSnapshotLoad::Rejected(rejection) => rejection.to_string(),
            other => other.code().to_string(),
        }),
        matches!(outcome, AnnSnapshotLoad::Loaded { .. }) && mismatched == 0,
    );
    let _ = write!(
        json,
        "\"timings_ns\":{{\"input\":{input_ns},\"insert\":{insert_ns},\"initial_build\":{build_ns},\"pre_save_queries\":{pre_query_ns},\"save\":{save_ns},\"close\":{close_ns},\"reopen\":{reopen_ns},\"load\":{load_ns},\"post_load_queries\":{post_query_ns},\"restart_with_snapshot\":{},\"rebuild_trials\":[{}]}}",
        reopen_ns + load_ns,
        rebuilds
            .iter()
            .map(|(open, build)| format!(
                "{{\"reopen\":{open},\"build\":{build},\"restart_with_rebuild\":{}}}",
                open + build
            ))
            .collect::<Vec<_>>()
            .join(","),
    );
    json.push('}');
    println!("{json}");

    if !matches!(outcome, AnnSnapshotLoad::Loaded { .. }) || mismatched != 0 {
        return Err("snapshot restart did not reproduce the pre-save hits exactly".into());
    }
    Ok(())
}
