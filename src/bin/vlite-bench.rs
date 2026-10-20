use std::error::Error;
use std::hint::black_box;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;
use vlite::{
    Condition, Database, DistanceMetric, HnswConfig, HnswRebuildLimits, HnswRebuildStatus,
    MAX_BATCH_BYTES, MAX_BATCH_OPERATIONS, MAX_DIMENSIONS, MAX_EF_SEARCH, Metadata, MetadataValue,
    Mutation,
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const USAGE: &str = "usage: vlite-bench [--metric squared_l2|cosine|inner_product] [--vectors <file> --query-vectors <file>] [--update-vectors <file>] [--mixed-writes <updates per query>] [--mixed-deletes <deletes per query>] [--rebuild-mode lazy|background] [--query-interval-ms <0..1000>] [--filter-every <N>] [--ann <m> <ef_construction> <ef_search>] [records=1000 dimensions=32 queries=100 batch_size=100 k=10 seed=42]; background mode requires mixed ANN and reports ready-only rejections without fallback; filtering and ANN are mutually exclusive; inner product is exact-only";
const MAX_DATA_BYTES: usize = 256 * 1024 * 1024;
const MAX_DISTANCE_COMPONENTS: u64 = 500_000_000;

struct Config {
    records: usize,
    dimensions: usize,
    queries: usize,
    batch_size: usize,
    k: usize,
    seed: u64,
    ann: Option<(HnswConfig, usize)>,
    filter_every: Option<usize>,
    input: Option<(PathBuf, PathBuf)>,
    mixed_writes: usize,
    mixed_deletes: usize,
    update_vectors: Option<PathBuf>,
    metric: DistanceMetric,
    background_rebuild: bool,
    query_interval_ms: u64,
}

impl Config {
    fn parse() -> Result<Self> {
        let mut args: Vec<_> = std::env::args().skip(1).collect();
        let mut input_vectors = None;
        let mut input_queries = None;
        let mut mixed_writes = None;
        let mut mixed_deletes = None;
        let mut update_vectors = None;
        let mut metric = None;
        let mut rebuild_mode = None;
        let mut query_interval_ms = None;
        while args.first().is_some_and(|arg| {
            matches!(
                arg.as_str(),
                "--vectors"
                    | "--query-vectors"
                    | "--mixed-writes"
                    | "--mixed-deletes"
                    | "--update-vectors"
                    | "--metric"
                    | "--rebuild-mode"
                    | "--query-interval-ms"
            )
        }) {
            let flag = args[0].clone();
            let value = args.get(1).ok_or(USAGE)?.clone();
            match flag.as_str() {
                "--rebuild-mode" if rebuild_mode.is_none() => {
                    rebuild_mode = Some(match value.as_str() {
                        "background" => true,
                        "lazy" => false,
                        _ => return Err("rebuild mode must be lazy or background".into()),
                    });
                }
                "--query-interval-ms" if query_interval_ms.is_none() => {
                    let interval = value.parse::<u64>()?;
                    if interval > 1000 {
                        return Err("background interval must be 0..=1000 milliseconds".into());
                    }
                    query_interval_ms = Some(interval);
                }
                "--metric" if metric.is_none() => {
                    metric = Some(match value.as_str() {
                        "squared_l2" => DistanceMetric::SquaredL2,
                        "cosine" => DistanceMetric::Cosine,
                        "inner_product" => DistanceMetric::InnerProduct,
                        _ => {
                            return Err("metric must be squared_l2, cosine or inner_product".into());
                        }
                    });
                }
                "--vectors" if input_vectors.is_none() => {
                    input_vectors = Some(PathBuf::from(value))
                }
                "--query-vectors" if input_queries.is_none() => {
                    input_queries = Some(PathBuf::from(value));
                }
                "--update-vectors" if update_vectors.is_none() => {
                    update_vectors = Some(PathBuf::from(value));
                }
                "--mixed-deletes" if mixed_deletes.is_none() => {
                    let count = value.parse::<usize>()?;
                    if !(1..=256).contains(&count) {
                        return Err("mixed deletes must be between 1 and 256".into());
                    }
                    mixed_deletes = Some(count);
                }
                "--mixed-writes" if mixed_writes.is_none() => {
                    let count = value.parse::<usize>()?;
                    if !(1..=256).contains(&count) {
                        return Err("mixed writes must be between 1 and 256".into());
                    }
                    mixed_writes = Some(count);
                }
                _ => return Err("duplicate benchmark option".into()),
            }
            args.drain(..2);
        }
        let input = match (input_vectors, input_queries) {
            (Some(vectors), Some(queries)) => Some((vectors, queries)),
            (None, None) => None,
            _ => return Err("both --vectors and --query-vectors are required".into()),
        };
        let filter_every = if args.first().is_some_and(|arg| arg == "--filter-every") {
            let every = args.get(1).ok_or(USAGE)?.parse::<usize>()?;
            if every == 0 {
                return Err("filter interval must be positive".into());
            }
            args.drain(..2);
            Some(every)
        } else {
            None
        };
        let ann = if args.first().is_some_and(|arg| arg == "--ann") {
            if args.len() < 4 {
                return Err(USAGE.into());
            }
            let m = args[1].parse::<usize>()?;
            let ef_construction = args[2].parse::<usize>()?;
            let ef_search = args[3].parse::<usize>()?;
            if !(2..=64).contains(&m)
                || !(m..=4096).contains(&ef_construction)
                || !(1..=MAX_EF_SEARCH).contains(&ef_search)
            {
                return Err("invalid HNSW parameters".into());
            }
            args.drain(..4);
            Some((
                HnswConfig {
                    m,
                    ef_construction,
                    seed: 42,
                },
                ef_search,
            ))
        } else {
            None
        };
        if filter_every.is_some() && ann.is_some() {
            return Err("filtered approximate search is not supported".into());
        }
        let metric = metric.unwrap_or_default();
        if ann.is_some() && metric == DistanceMetric::InnerProduct {
            return Err("inner_product is exact-only".into());
        }
        if (mixed_writes.is_some() || mixed_deletes.is_some()) && ann.is_none() {
            return Err("mixed mutations require --ann".into());
        }
        if update_vectors.is_some() && (mixed_writes.is_none() || input.is_none()) {
            return Err("--update-vectors requires external input and --mixed-writes".into());
        }
        if input.is_some() && mixed_writes.is_some() && update_vectors.is_none() {
            return Err("external mixed writes require --update-vectors; synthetic replacements are not implicit".into());
        }
        let background_rebuild = rebuild_mode.unwrap_or(false);
        if background_rebuild
            && (ann.is_none() || (mixed_writes.is_none() && mixed_deletes.is_none()))
        {
            return Err("background benchmark requires --ann and mixed mutations".into());
        }
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
        if ann.is_some_and(|(_, ef)| (ef as u64) < k) {
            return Err("ef_search must be at least k".into());
        }
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
        if (background_rebuild || query_interval_ms.is_some())
            && (queries > 200 || records > 100_000 || records * dimensions * 4 > 128 * 1024 * 1024)
        {
            return Err("background workload exceeds 200 queries, 100000 records or 128 MiB snapshot vector bytes".into());
        }
        let mixed_writes = mixed_writes.unwrap_or(0);
        let mixed_deletes = mixed_deletes.unwrap_or(0);
        let mutations = mixed_writes + mixed_deletes;
        if mutations > 256
            || (mutations > 0 && mutations as u64 > records)
            || queries * mutations as u64 > 65_536
            || mixed_writes as u64 * (16 + dimensions * 4) + mixed_deletes as u64 * 16
                > MAX_BATCH_BYTES as u64
        {
            return Err(
                "mixed workload exceeds mutation count, record count, or batch byte limit".into(),
            );
        }
        let bytes = (records
            + queries
            + if update_vectors.is_some() {
                queries * mixed_writes as u64
            } else {
                0
            })
        .checked_mul(dimensions)
        .and_then(|v| v.checked_mul(4));
        let components = (records + mixed_writes as u64)
            .checked_mul(dimensions)
            .and_then(|v| v.checked_mul(queries));
        if bytes.is_none_or(|n| n > MAX_DATA_BYTES as u64)
            || components.is_none_or(|n| n > MAX_DISTANCE_COMPONENTS)
            || batch_size.min(records)
                * (16 + dimensions * 4 + if filter_every.is_some() { 28 } else { 0 })
                > MAX_BATCH_BYTES as u64
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
            ann,
            filter_every,
            input,
            mixed_writes,
            mixed_deletes,
            update_vectors,
            metric,
            background_rebuild,
            query_interval_ms: query_interval_ms.unwrap_or(0),
        })
    }
}

fn load_vectors(path: &PathBuf, count: usize, dimensions: usize) -> Result<Vec<Vec<f32>>> {
    // Bound text input as well as decoded vectors before reading untrusted files.
    let limit = (count * dimensions * 32 + count).min(64 * 1024 * 1024) as u64;
    let mut text = String::new();
    std::fs::File::open(path)?
        .take(limit + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > limit {
        return Err("vector text input exceeds byte limit".into());
    }
    let mut result = Vec::with_capacity(count);
    for line in text.lines() {
        if result.len() == count {
            return Err("vector text has too many rows".into());
        }
        let vector = line
            .split_whitespace()
            .take(dimensions + 1)
            .map(str::parse::<f32>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if vector.len() != dimensions || vector.iter().any(|value| !value.is_finite()) {
            return Err(
                "vector text requires exactly the configured finite components per row".into(),
            );
        }
        result.push(vector);
    }
    if result.len() != count {
        return Err("vector text row count differs from configured count".into());
    }
    Ok(result)
}

fn checksum(vectors: &[Vec<f32>]) -> u64 {
    vectors
        .iter()
        .flatten()
        .fold(0xcbf2_9ce4_8422_2325, |hash, value| {
            value.to_bits().to_le_bytes().iter().fold(hash, |h, &byte| {
                (h ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
            })
        })
}

fn oracle_distance(vector: &[f32], query: &[f32], metric: DistanceMetric) -> Option<f64> {
    match metric {
        DistanceMetric::SquaredL2 => Some(
            vector
                .iter()
                .zip(query)
                .map(|(&a, &b)| (f64::from(a) - f64::from(b)).powi(2))
                .sum(),
        ),
        DistanceMetric::InnerProduct => {
            let dot: f64 = vector
                .iter()
                .zip(query)
                .map(|(&a, &b)| f64::from(a) * f64::from(b))
                .sum();
            Some(if dot == 0.0 { 0.0 } else { -dot })
        }
        DistanceMetric::Cosine => {
            let a: f64 = vector.iter().map(|&v| f64::from(v).powi(2)).sum();
            let b: f64 = query.iter().map(|&v| f64::from(v).powi(2)).sum();
            let dot: f64 = vector
                .iter()
                .zip(query)
                .map(|(&a, &b)| f64::from(a) * f64::from(b))
                .sum();
            (a > 0.0 && b > 0.0).then(|| 1.0 - (dot / (a.sqrt() * b.sqrt())).clamp(-1.0, 1.0))
        }
    }
}

fn ground_truth(dataset: &[Vec<f32>], query: &[f32], metric: DistanceMetric) -> Vec<(u64, f64)> {
    let mut expected: Vec<_> = dataset
        .iter()
        .enumerate()
        .filter_map(|(id, vector)| oracle_distance(vector, query, metric).map(|d| (id as u64, d)))
        .collect();
    expected.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
    expected
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
    let (mut dataset, queries) = if let Some((data, queries)) = &config.input {
        (
            load_vectors(data, config.records, config.dimensions)?,
            load_vectors(queries, config.queries, config.dimensions)?,
        )
    } else {
        (
            vectors(config.records, config.dimensions, &mut state),
            vectors(config.queries, config.dimensions, &mut state),
        )
    };
    let dataset_checksum = checksum(&dataset);
    let query_checksum = checksum(&queries);
    let updates = config
        .update_vectors
        .as_ref()
        .map(|path| {
            load_vectors(
                path,
                config.queries * config.mixed_writes,
                config.dimensions,
            )
        })
        .transpose()?;
    let update_checksum = updates.as_ref().map(|vectors| checksum(vectors));
    let mut live = vec![true; config.records];
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
            .map(|(index, vector)| {
                let id = batch_index * config.batch_size + index;
                if let Some(every) = config.filter_every {
                    Mutation::PutWithMetadata {
                        id: id as u64,
                        vector: vector.clone(),
                        payload: Vec::new(),
                        metadata: Metadata::from([(
                            "selected".into(),
                            MetadataValue::Boolean(id % every == 0),
                        )]),
                    }
                } else {
                    Mutation::Put {
                        id: id as u64,
                        vector: vector.clone(),
                        payload: Vec::new(),
                    }
                }
            })
            .collect();
        db.write_batch(&operations)?;
    }
    let ingest_seconds = start.elapsed().as_secs_f64();
    db.close()?;
    let file_bytes = std::fs::metadata(&path)?.len();
    let start = Instant::now();
    let mut db = Database::open(&path)?;
    let reopen_ms = start.elapsed().as_secs_f64() * 1000.0;
    if db.len() != config.records {
        return Err("reopened database has an incorrect record count".into());
    }
    let filter = config
        .filter_every
        .map(|_| Condition::Eq {
            field: "selected".into(),
            value: MetadataValue::Boolean(true),
        })
        .into_iter()
        .collect::<Vec<_>>();
    let eligible_records = dataset
        .iter()
        .enumerate()
        .filter(|(id, vector)| {
            config
                .filter_every
                .is_none_or(|every| id.is_multiple_of(every))
                && (config.metric != DistanceMetric::Cosine || vector.iter().any(|&v| v != 0.0))
        })
        .count();
    let result_count = config.k.min(eligible_records);
    black_box(db.search_filtered_with_metric(&queries[0], config.k, &filter, config.metric)?);
    let mut times_ms = Vec::with_capacity(config.queries);
    let mut matched = 0usize;
    let mut truth = Vec::new();
    for query in &queries {
        let start = Instant::now();
        let actual = black_box(db.search_filtered_with_metric(
            black_box(query),
            config.k,
            &filter,
            config.metric,
        )?);
        times_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        // The oracle works directly from the generated input, outside timing.
        let expected: Vec<_> = ground_truth(&dataset, query, config.metric)
            .into_iter()
            .filter(|(id, _)| {
                config
                    .filter_every
                    .is_none_or(|every| (*id as usize).is_multiple_of(every))
            })
            .collect();
        let result_count = config.k.min(expected.len());
        for (hit, &(id, distance)) in actual.iter().zip(&expected[..result_count]) {
            if hit.id != id || hit.distance != distance {
                return Err(io::Error::other(
                    "exact search disagrees with independent ground truth",
                )
                .into());
            }
            matched += 1;
        }
        if actual.len() != result_count {
            return Err("exact search returned incorrect result count".into());
        }
        if config.ann.is_some() {
            truth.push(actual);
        }
    }
    let ann_json = if let Some((index_config, ef_search)) = config.ann {
        let start = Instant::now();
        db.build_hnsw_with_metric(index_config, config.metric)?;
        let build_ms = start.elapsed().as_secs_f64() * 1000.0;
        black_box(db.search_approximate_with_metric(
            &queries[0],
            config.k,
            ef_search,
            index_config,
            config.metric,
        )?);
        let mut approximate_times = Vec::new();
        let mut write_times = Vec::new();
        let mut recalled = 0;
        let mut recall_total = 0;
        let mut rebuild_queries = Vec::new();
        let mut recall_samples = Vec::new();
        let mut live_samples = Vec::new();
        let mut not_ready_queries = Vec::new();
        let mut publication_queries = Vec::new();
        let mut overflow_queries = Vec::new();
        let mut start_queries = Vec::new();
        let mut maintenance_times = Vec::new();
        let mut request_times = Vec::new();
        let mut served_times = Vec::new();
        let mut served_recall_total = 0;
        for (query_index, query) in queries.iter().enumerate() {
            std::thread::sleep(std::time::Duration::from_millis(config.query_interval_ms));
            let request_start = Instant::now();
            if config.background_rebuild {
                let start = Instant::now();
                let status = db.poll_hnsw_rebuild()?;
                match status {
                    HnswRebuildStatus::Published => publication_queries.push(query_index + 1),
                    HnswRebuildStatus::CatchUpOverflow => overflow_queries.push(query_index + 1),
                    HnswRebuildStatus::Idle
                    | HnswRebuildStatus::Running
                    | HnswRebuildStatus::Cancelling => {}
                    status => {
                        return Err(
                            format!("unexpected background benchmark state: {status:?}").into()
                        );
                    }
                }
                if matches!(
                    status,
                    HnswRebuildStatus::Idle
                        | HnswRebuildStatus::Published
                        | HnswRebuildStatus::CatchUpOverflow
                ) {
                    db.start_hnsw_rebuild(
                        index_config,
                        config.metric,
                        HnswRebuildLimits::default(),
                    )?;
                    start_queries.push(query_index + 1);
                }
                maintenance_times.push(start.elapsed().as_secs_f64() * 1000.0);
            }
            if config.mixed_writes + config.mixed_deletes > 0 {
                let replacements = if let Some(updates) = &updates {
                    updates
                        [query_index * config.mixed_writes..(query_index + 1) * config.mixed_writes]
                        .to_vec()
                } else {
                    vectors(config.mixed_writes, config.dimensions, &mut state)
                };
                let width = config.mixed_writes + config.mixed_deletes;
                let mut operations: Vec<_> = replacements
                    .into_iter()
                    .enumerate()
                    .map(|(index, vector)| Mutation::Put {
                        id: ((query_index * width + index) % config.records) as u64,
                        vector,
                        payload: Vec::new(),
                    })
                    .collect();
                operations.extend((0..config.mixed_deletes).map(|index| Mutation::Delete {
                    id: ((query_index * width + config.mixed_writes + index) % config.records)
                        as u64,
                }));
                let start = Instant::now();
                db.write_batch(&operations)?;
                write_times.push(start.elapsed().as_secs_f64() * 1000.0);
                for operation in operations {
                    match operation {
                        Mutation::Put { id, vector, .. } => {
                            dataset[id as usize] = vector;
                            live[id as usize] = true;
                        }
                        Mutation::Delete { id } => live[id as usize] = false,
                        _ => unreachable!(),
                    }
                }
            }
            let start = Instant::now();
            let hits = black_box(
                match db.search_approximate_ready_with_metric(
                    black_box(query),
                    config.k,
                    ef_search,
                    index_config,
                    config.metric,
                ) {
                    Ok(hits) => hits,
                    Err(vlite::Error::IndexNotReady) => {
                        if config.background_rebuild {
                            not_ready_queries.push(query_index + 1);
                            Vec::new()
                        } else {
                            rebuild_queries.push(query_index + 1);
                            db.search_approximate_with_metric(
                                query,
                                config.k,
                                ef_search,
                                index_config,
                                config.metric,
                            )?
                        }
                    }
                    Err(error) => return Err(error.into()),
                },
            );
            approximate_times.push(start.elapsed().as_secs_f64() * 1000.0);
            request_times.push(request_start.elapsed().as_secs_f64() * 1000.0);
            let expected = if config.mixed_writes + config.mixed_deletes > 0 {
                let expected: Vec<_> = ground_truth(&dataset, query, config.metric)
                    .into_iter()
                    .filter(|&(id, _)| live[id as usize])
                    .collect();
                let exact = db.search_with_metric(query, config.k, config.metric)?;
                if exact.len() != config.k.min(expected.len())
                    || exact
                        .iter()
                        .zip(&expected)
                        .any(|(hit, &(id, distance))| hit.id != id || hit.distance != distance)
                {
                    return Err(
                        "post-write exact search disagrees with current-state oracle".into(),
                    );
                }
                expected
            } else {
                truth[query_index]
                    .iter()
                    .map(|hit| (hit.id, hit.distance))
                    .collect()
            };
            let mut ids = std::collections::BTreeSet::new();
            let expected_count = config.k.min(expected.len());
            if not_ready_queries.last() != Some(&(query_index + 1)) {
                served_times.push(*approximate_times.last().expect("timed query"));
                served_recall_total += expected_count;
            }
            recall_total += expected_count;
            let before_recalled = recalled;
            live_samples.push(db.len());
            let cutoff = expected
                .iter()
                .take(config.k)
                .next_back()
                .map(|&(_, distance)| distance);
            if hits.len() > expected_count {
                return Err("approximate search returned too many candidates".into());
            }
            for hit in &hits {
                if !ids.insert(hit.id) || hit.id >= config.records as u64 || !live[hit.id as usize]
                {
                    return Err("approximate search returned duplicate or invalid IDs".into());
                }
                let distance = oracle_distance(&dataset[hit.id as usize], query, config.metric);
                if Some(hit.distance) != distance {
                    return Err("approximate candidate distance is incorrect".into());
                }
                recalled += usize::from(cutoff.is_some_and(|distance| hit.distance <= distance));
            }
            recall_samples.push(if expected_count == 0 {
                1.0
            } else {
                (recalled - before_recalled) as f64 / expected_count as f64
            });
            if hits.windows(2).any(|pair| {
                pair[0]
                    .distance
                    .total_cmp(&pair[1].distance)
                    .then(pair[0].id.cmp(&pair[1].id))
                    .is_gt()
            }) {
                return Err("approximate results are not sorted".into());
            }
        }
        let samples = format!("{approximate_times:?}");
        let writes = format!("{write_times:?}");
        let background_json = if config.background_rebuild {
            let interval = config.query_interval_ms;
            let served = served_times.len();
            served_times.sort_by(f64::total_cmp);
            let served_p50 = if served == 0 {
                "null".into()
            } else {
                percentile(&served_times, 50).to_string()
            };
            let served_p99 = if served == 0 {
                "null".into()
            } else {
                percentile(&served_times, 99).to_string()
            };
            format!(
                ",\"background\":{{\"interval_ms\":{interval},\"start_query_numbers\":{start_queries:?},\"published_query_numbers\":{publication_queries:?},\"catchup_overflow_query_numbers\":{overflow_queries:?},\"not_ready_query_numbers\":{not_ready_queries:?},\"served_queries\":{served},\"attempted_queries\":{},\"served_recall_at_k\":{},\"served_query_p50_ms\":{served_p50},\"served_query_p99_ms\":{served_p99},\"maintenance_samples_ms\":{maintenance_times:?},\"request_samples_ms\":{request_times:?}}}",
                config.queries,
                if served_recall_total == 0 {
                    "null".into()
                } else {
                    (recalled as f64 / served_recall_total as f64).to_string()
                },
            )
        } else {
            String::new()
        };
        approximate_times.sort_by(f64::total_cmp);
        format!(
            ",\"hnsw\":{{\"m\":{},\"ef_construction\":{},\"ef_search\":{},\"seed\":{},\"index_build_ms\":{:.6},\"query_p50_ms\":{:.6},\"query_p95_ms\":{:.6},\"query_p99_ms\":{:.6},\"recall_at_k\":{:.6},\"mixed_writes_per_query\":{},\"mixed_deletes_per_query\":{},\"query_samples_ms\":{},\"write_batch_samples_ms\":{},\"rebuild_query_numbers\":{:?},\"recall_samples\":{:?},\"live_records_samples\":{:?},\"query_interval_ms\":{},\"request_samples_ms\":{:?}{}}}",
            index_config.m,
            index_config.ef_construction,
            ef_search,
            index_config.seed,
            build_ms,
            percentile(&approximate_times, 50),
            percentile(&approximate_times, 95),
            percentile(&approximate_times, 99),
            if recall_total == 0 {
                1.0
            } else {
                recalled as f64 / recall_total as f64
            },
            config.mixed_writes,
            config.mixed_deletes,
            samples,
            writes,
            rebuild_queries,
            recall_samples,
            live_samples,
            config.query_interval_ms,
            request_times,
            background_json,
        )
    } else {
        String::new()
    };
    let filter_json = config.filter_every.map_or(String::new(), |every| {
        format!(        ",\"filter_every\":{every},\"eligible_records\":{eligible_records},\"returned_per_query\":{result_count}")
    });
    db.close()?;
    let final_file_bytes = std::fs::metadata(&path)?.len();
    directory.close()?;
    let samples = format!("{times_ms:?}");
    let update_json = update_checksum.map_or(String::new(), |value| {
        format!(",\"update_checksum\":\"{value:016x}\"")
    });
    times_ms.sort_by(f64::total_cmp);
    println!(
        concat!(
            "{{\"schema_version\":1,\"engine_version\":\"{}\",\"format_version\":{},\"os\":\"{}\",\"arch\":\"{}\",\"profile\":\"{}\",",
            "\"records\":{},\"dimensions\":{},\"queries\":{},\"batch_size\":{},\"k\":{},\"seed\":{},\"metric\":\"{}\",",
            "\"dataset_checksum\":\"{:016x}\",\"query_checksum\":\"{:016x}\",\"input\":\"{}\",\"ingest_seconds\":{:.9},\"ingest_vectors_per_second\":{:.3},",
            "\"reopen_ms\":{:.6},\"query_p50_ms\":{:.6},\"query_p95_ms\":{:.6},\"query_p99_ms\":{:.6},\"query_samples_ms\":{},\"final_file_bytes\":{},",
            "\"file_bytes\":{},\"vector_data_bytes\":{},\"query_data_bytes\":{},\"process_peak_rss_bytes\":null,",
            "\"recall_at_k\":{:.6}{}{}{}{}}}"
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
        match config.metric {
            DistanceMetric::SquaredL2 => "squared_l2",
            DistanceMetric::Cosine => "cosine",
            DistanceMetric::InnerProduct => "inner_product",
        },
        dataset_checksum,
        query_checksum,
        if config.input.is_some() {
            "external-f32-text"
        } else {
            "synthetic-splitmix64"
        },
        ingest_seconds,
        config.records as f64 / ingest_seconds.max(f64::MIN_POSITIVE),
        reopen_ms,
        percentile(&times_ms, 50),
        percentile(&times_ms, 95),
        percentile(&times_ms, 99),
        samples,
        final_file_bytes,
        file_bytes,
        config.records * config.dimensions * 4,
        config.queries * config.dimensions * 4,
        if result_count == 0 {
            1.0
        } else {
            matched as f64 / (config.queries * result_count) as f64
        },
        ann_json,
        filter_json,
        update_json,
        format_args!(
            ",\"final_live_records\":{}",
            live.iter().filter(|&&value| value).count()
        )
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
