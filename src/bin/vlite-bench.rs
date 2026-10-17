use std::error::Error;
use std::hint::black_box;
use std::io;
use std::process::ExitCode;
use std::time::Instant;
use vlite::{Database, MAX_BATCH_BYTES, MAX_BATCH_OPERATIONS, MAX_DIMENSIONS, Mutation};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const USAGE: &str =
    "usage: vlite-bench [records=1000 dimensions=32 queries=100 batch_size=100 k=10 seed=42]";
const MAX_DATA_BYTES: usize = 256 * 1024 * 1024;
const MAX_DISTANCE_COMPONENTS: u64 = 500_000_000;

struct Config {
    records: usize,
    dimensions: usize,
    queries: usize,
    batch_size: usize,
    k: usize,
    seed: u64,
}

impl Config {
    fn parse() -> Result<Self> {
        let args: Vec<_> = std::env::args().skip(1).collect();
        if args.len() > 6 {
            return Err(USAGE.into());
        }
        let defaults = [1000u64, 32, 100, 100, 10, 42];
        let mut values = defaults;
        for (position, arg) in args.iter().enumerate() {
            values[position] = arg
                .parse()
                .map_err(|_| format!("invalid integer {arg:?}; {USAGE}"))?;
        }
        let [records, dimensions, queries, batch_size, k, seed] = values;
        let bounded = records > 0
            && records <= 1_000_000
            && dimensions > 0
            && dimensions <= MAX_DIMENSIONS as u64
            && queries > 0
            && queries <= 10_000
            && batch_size > 0
            && batch_size <= MAX_BATCH_OPERATIONS as u64
            && k > 0
            && k <= records;
        if !bounded {
            return Err(format!("invalid workload bounds; {USAGE}").into());
        }
        let bytes = (records + queries)
            .checked_mul(dimensions)
            .and_then(|v| v.checked_mul(4));
        let components = records
            .checked_mul(dimensions)
            .and_then(|v| v.checked_mul(queries));
        if bytes.is_none_or(|n| n > MAX_DATA_BYTES as u64)
            || components.is_none_or(|n| n > MAX_DISTANCE_COMPONENTS)
            || batch_size.min(records) * (16 + dimensions * 4) > MAX_BATCH_BYTES as u64
        {
            return Err("workload exceeds 256 MiB of vector data, 500 million distance components, or batch byte limit".into());
        }
        Ok(Self {
            records: usize::try_from(records)?,
            dimensions: usize::try_from(dimensions)?,
            queries: usize::try_from(queries)?,
            batch_size: usize::try_from(batch_size)?,
            k: usize::try_from(k)?,
            seed,
        })
    }
}

// SplitMix64 with fixed-point components: reproducible across OSes and CPUs.
fn vectors(count: usize, dimensions: usize, state: &mut u64) -> Vec<Vec<f32>> {
    (0..count)
        .map(|_| {
            (0..dimensions)
                .map(|_| {
                    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
                    let mut value = *state;
                    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
                    value ^= value >> 31;
                    ((value >> 48) as i32 - 32768) as f32 / 32768.0
                })
                .collect()
        })
        .collect()
}

fn percentile(sorted_ms: &[f64], percentile: usize) -> f64 {
    sorted_ms[(sorted_ms.len() * percentile).div_ceil(100) - 1]
}

fn run() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--help") {
        println!("{USAGE}");
        return Ok(());
    }
    let config = Config::parse()?;
    let mut state = config.seed;
    let dataset = vectors(config.records, config.dimensions, &mut state);
    let queries = vectors(config.queries, config.dimensions, &mut state);
    let checksum = dataset
        .iter()
        .flatten()
        .fold(0xcbf2_9ce4_8422_2325u64, |hash, value| {
            value.to_bits().to_le_bytes().iter().fold(hash, |h, &byte| {
                (h ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
            })
        });
    let parent = std::env::var_os("VLITE_BENCH_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    let directory = tempfile::tempdir_in(parent)?;
    let path = directory.path().join("benchmark.vlite");
    let mut db = Database::create(&path, config.dimensions)?;
    let format_version = db.format_version();
    let start = Instant::now();
    for (batch_index, chunk) in dataset.chunks(config.batch_size).enumerate() {
        let operations: Vec<_> = chunk
            .iter()
            .enumerate()
            .map(|(index, vector)| Mutation::Put {
                id: (batch_index * config.batch_size + index) as u64,
                vector: vector.clone(),
                payload: Vec::new(),
            })
            .collect();
        db.write_batch(&operations)?;
    }
    let ingest_seconds = start.elapsed().as_secs_f64();
    db.close()?;
    let file_bytes = std::fs::metadata(&path)?.len();
    let start = Instant::now();
    let db = Database::open(&path)?;
    let reopen_ms = start.elapsed().as_secs_f64() * 1000.0;
    if db.len() != config.records {
        return Err("reopened database has an incorrect record count".into());
    }
    black_box(db.search(&queries[0], config.k)?);
    let mut times_ms = Vec::with_capacity(config.queries);
    let mut matched = 0usize;
    for query in &queries {
        let start = Instant::now();
        let actual = black_box(db.search(black_box(query), config.k)?);
        times_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        // The oracle works directly from the generated input, outside timing.
        let mut expected: Vec<_> = dataset
            .iter()
            .enumerate()
            .map(|(id, vector)| {
                let mut distance = 0.0f64;
                for index in 0..config.dimensions {
                    distance += (f64::from(vector[index]) - f64::from(query[index])).powi(2);
                }
                (id as u64, distance)
            })
            .collect();
        expected.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        for (hit, &(id, distance)) in actual.iter().zip(&expected[..config.k]) {
            if hit.id != id || hit.distance != distance {
                return Err(io::Error::other(
                    "exact search disagrees with independent ground truth",
                )
                .into());
            }
            matched += 1;
        }
        if actual.len() != config.k {
            return Err("exact search returned incorrect result count".into());
        }
    }
    db.close()?;
    directory.close()?;
    times_ms.sort_by(f64::total_cmp);
    println!(
        concat!(
            "{{\"schema_version\":1,\"engine_version\":\"{}\",\"format_version\":{},\"os\":\"{}\",\"arch\":\"{}\",\"profile\":\"{}\",",
            "\"records\":{},\"dimensions\":{},\"queries\":{},\"batch_size\":{},\"k\":{},\"seed\":{},",
            "\"dataset_checksum\":\"{:016x}\",\"ingest_seconds\":{:.9},\"ingest_vectors_per_second\":{:.3},",
            "\"reopen_ms\":{:.6},\"query_p50_ms\":{:.6},\"query_p95_ms\":{:.6},",
            "\"file_bytes\":{},\"vector_data_bytes\":{},\"query_data_bytes\":{},\"process_peak_rss_bytes\":null,",
            "\"recall_at_k\":{:.6}}}"
        ),
        env!("CARGO_PKG_VERSION"),
        format_version,
        std::env::consts::OS,
        std::env::consts::ARCH,
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        config.records,
        config.dimensions,
        config.queries,
        config.batch_size,
        config.k,
        config.seed,
        checksum,
        ingest_seconds,
        config.records as f64 / ingest_seconds.max(f64::MIN_POSITIVE),
        reopen_ms,
        percentile(&times_ms, 50),
        percentile(&times_ms, 95),
        file_bytes,
        config.records * config.dimensions * 4,
        config.queries * config.dimensions * 4,
        matched as f64 / (config.queries * config.k) as f64
    );
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("FAIL: {error}");
            ExitCode::FAILURE
        }
    }
}
