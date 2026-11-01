//! Local before/after equivalence probe for exact scoring (P3d), not a benchmark.
//! Prints one FNV-1a fingerprint of `(id, distance bits)` per scenario; run the
//! same source on two revisions with `cargo run --release --example
//! exact_kernel_fingerprint` and compare the lines.
use std::error::Error;
use vlite::rerank::{ExactReranker, MmrReranker};
use vlite::{
    AnnOptions, Condition, CreateOptions, Database, DistanceMetric, HnswConfig, Metadata,
    MetadataValue, Mutation, Quantization, SearchHit,
};

fn component(state: &mut u64) -> f32 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    ((*state >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
}

fn fingerprint(hits: &[SearchHit]) -> u64 {
    let mut value = 0xcbf2_9ce4_8422_2325u64;
    for hit in hits {
        for byte in hit
            .id
            .to_le_bytes()
            .into_iter()
            .chain(hit.distance.to_bits().to_le_bytes())
        {
            value = (value ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3);
        }
    }
    value
}

fn database(dims: usize, records: u64, seed: u64) -> Result<Database, Box<dyn Error>> {
    let mut db = Database::create_in_memory(dims, CreateOptions::default())?;
    let mut state = seed;
    let mut previous: Vec<f32> = Vec::new();
    let operations: Vec<Mutation> = (0..records)
        .map(|id| {
            // Every seventh row repeats its predecessor (exact ties); some
            // rows are coarse multiples of 1/8 so equal distances recur.
            let vector: Vec<f32> = if id % 7 == 6 {
                previous.clone()
            } else if id % 5 == 0 {
                (0..dims)
                    .map(|_| (component(&mut state) * 8.0).round() / 8.0)
                    .collect()
            } else {
                (0..dims)
                    .map(|dim| component(&mut state) * (1.0 + (dim % 3) as f32 * 100.0))
                    .collect()
            };
            previous = vector.clone();
            Mutation::PutWithMetadata {
                id,
                vector,
                payload: Vec::new(),
                metadata: Metadata::from([(
                    "group".into(),
                    MetadataValue::Integer((id % 3) as i64),
                )]),
            }
        })
        .collect();
    for chunk in operations.chunks(1000) {
        db.write_batch(chunk)?;
    }
    Ok(db)
}

fn main() -> Result<(), Box<dyn Error>> {
    let metrics = [
        DistanceMetric::SquaredL2,
        DistanceMetric::Cosine,
        DistanceMetric::InnerProduct,
    ];
    let filter = [Condition::Eq {
        field: "group".into(),
        value: MetadataValue::Integer(1),
    }];
    for (dims, records) in [(5usize, 3000u64), (37, 3000), (384, 3000), (64, 20_000)] {
        let db = database(dims, records, dims as u64)?;
        let mut state = 1000 + dims as u64;
        for query_number in 0..4 {
            let query: Vec<f32> = (0..dims).map(|_| component(&mut state)).collect();
            for metric in metrics {
                for k in [1usize, 10, 100] {
                    let all = db.search_with_metric(&query, k, metric)?;
                    let filtered = db.search_filtered_with_metric(&query, k, &filter, metric)?;
                    println!(
                        "exact dims={dims} records={records} q={query_number} {metric:?} k={k} all={:016x} filtered={:016x}",
                        fingerprint(&all),
                        fingerprint(&filtered)
                    );
                }
                let hits = db.search_with_metric(&query, 50, metric)?;
                let collection = db.collection("default")?;
                let exact = collection.rerank(&query, &hits, 20, &ExactReranker { metric })?;
                println!(
                    "rerank dims={dims} q={query_number} {metric:?} exact={:016x}",
                    fingerprint(&exact)
                );
                if metric == DistanceMetric::Cosine {
                    let mmr = collection.rerank(&query, &hits, 20, &MmrReranker { lambda: 0.5 })?;
                    println!(
                        "rerank dims={dims} q={query_number} mmr={:016x}",
                        fingerprint(&mmr)
                    );
                }
            }
        }
    }
    let config = HnswConfig {
        m: 12,
        ef_construction: 80,
        seed: 3,
    };
    for (dims, records) in [(37usize, 3000u64), (128, 3000)] {
        for metric in [DistanceMetric::SquaredL2, DistanceMetric::Cosine] {
            let mut db = database(dims, records, 7 + dims as u64)?;
            let mut state = 2000 + dims as u64;
            let queries: Vec<Vec<f32>> = (0..8)
                .map(|_| (0..dims).map(|_| component(&mut state)).collect())
                .collect();
            for (number, query) in queries.iter().enumerate() {
                let hits = db.search_approximate_with_metric(query, 10, 64, config, metric)?;
                let filtered = db.search_approximate_filtered_with_metric(
                    query, 10, 64, config, &filter, metric,
                )?;
                println!(
                    "hnsw dims={dims} {metric:?} q={number} all={:016x} filtered={:016x}",
                    fingerprint(&hits),
                    fingerprint(&filtered)
                );
            }
            // Buffered updates are merged with exact scores.
            for id in 0..20u64 {
                let vector: Vec<f32> = (0..dims).map(|_| component(&mut state)).collect();
                db.put(id * 11, &vector, b"")?;
            }
            for (number, query) in queries.iter().enumerate() {
                let hits =
                    db.search_approximate_ready_with_metric(query, 10, 64, config, metric)?;
                println!(
                    "hnsw-buffered dims={dims} {metric:?} q={number} hits={:016x}",
                    fingerprint(&hits)
                );
            }
            let options = AnnOptions {
                quantization: Quantization::ScalarU8,
                rerank_multiplier: 4,
            };
            let mut db = database(dims, records, 9 + dims as u64)?;
            db.build_hnsw_with_options(config, metric, options)?;
            for (number, query) in queries.iter().enumerate() {
                let (hits, _) = db.search_approximate_ready_with_options(
                    query,
                    10,
                    64,
                    config,
                    options,
                    &[],
                    metric,
                )?;
                let (filtered, _) = db.search_approximate_ready_with_options(
                    query, 10, 64, config, options, &filter, metric,
                )?;
                println!(
                    "scalar-u8 dims={dims} {metric:?} q={number} all={:016x} filtered={:016x}",
                    fingerprint(&hits),
                    fingerprint(&filtered)
                );
            }
        }
    }
    Ok(())
}
