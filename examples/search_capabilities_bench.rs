//! Bounded synthetic search comparison; JSON stdout, diagnostics on stderr.
use std::{error::Error, hint::black_box, time::Instant};
use vlite::{
    AnnOptions, CollectionSettings, Condition, Database, DistanceMetric, HnswConfig, Metadata,
    MetadataIndexKind, MetadataIndexSpec, MetadataValue, Mutation, Quantization,
};

const RECORDS: usize = 2000;
const DIMS: usize = 32;
const QUERIES: usize = 30;
const K: usize = 10;

fn unit(state: &mut u64) -> f32 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    ((*state >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
}

fn percentile(samples: &[f64], fraction: f64) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[((sorted.len() - 1) as f64 * fraction).ceil() as usize]
}

fn distance(query: &[f32], vector: &[f32], metric: DistanceMetric) -> f64 {
    if metric == DistanceMetric::SquaredL2 {
        query
            .iter()
            .zip(vector)
            .map(|(&a, &b)| (f64::from(a) - f64::from(b)).powi(2))
            .sum()
    } else {
        let dot: f64 = query
            .iter()
            .zip(vector)
            .map(|(&a, &b)| f64::from(a) * f64::from(b))
            .sum();
        let q: f64 = query.iter().map(|&v| f64::from(v).powi(2)).sum();
        let v: f64 = vector.iter().map(|&v| f64::from(v).powi(2)).sum();
        (1.0 - dot / (q.sqrt() * v.sqrt())).clamp(0.0, 2.0)
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    if std::env::args().len() != 1 {
        return Err("this bounded benchmark takes no arguments".into());
    }
    let directory = tempfile::tempdir()?;
    let mut db = Database::create(directory.path().join("bench.vlite"), DIMS)?;
    let mut state = 11;
    let centers: Vec<Vec<f32>> = (0..20)
        .map(|_| (0..DIMS).map(|_| unit(&mut state) * 4.0).collect())
        .collect();
    let vectors: Vec<Vec<f32>> = (0..RECORDS)
        .map(|i| {
            centers[i % centers.len()]
                .iter()
                .map(|center| center + unit(&mut state) * 0.5)
                .collect()
        })
        .collect();
    for (batch, chunk) in vectors.chunks(1000).enumerate() {
        let operations: Vec<_> = chunk
            .iter()
            .enumerate()
            .map(|(offset, vector)| {
                let id = batch * 1000 + offset;
                Mutation::PutWithMetadata {
                    id: id as u64,
                    vector: vector.clone(),
                    payload: vec![],
                    metadata: Metadata::from([(
                        "group".to_owned(),
                        MetadataValue::Integer((id % 100) as i64),
                    )]),
                }
            })
            .collect();
        db.write_batch(&operations)?;
    }
    let queries: Vec<Vec<f32>> = (0..QUERIES)
        .map(|i| {
            vectors[i * 37 % RECORDS]
                .iter()
                .map(|component| component + unit(&mut state) * 0.3)
                .collect()
        })
        .collect();
    let config = HnswConfig {
        m: 12,
        ef_construction: 100,
        seed: 3,
    };
    println!(
        "{{\"schema\":1,\"workload\":\"synthetic_clustered\",\"records\":{RECORDS},\"dimensions\":{DIMS},\"queries\":{QUERIES},\"k\":{K},\"ef_search\":100,\"hnsw_m\":12,\"ef_construction\":100,\"graph_seed\":3,\"data_seed\":11,\"os\":\"{}\",\"arch\":\"{}\",\"memory_scope\":\"component bytes, not measured RSS or a total RAM reduction\",\"rows\":[",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    let mut first = true;
    for (metric, metric_name) in [
        (DistanceMetric::SquaredL2, "squared_l2"),
        (DistanceMetric::Cosine, "cosine"),
    ] {
        let mut exact_samples = Vec::new();
        let mut oracle = Vec::new();
        for query in &queries {
            let started = Instant::now();
            let hits = black_box(db.search_with_metric(query, K, metric)?);
            exact_samples.push(started.elapsed().as_secs_f64() * 1000.0);
            let mut expected: Vec<_> = vectors
                .iter()
                .enumerate()
                .map(|(id, vector)| (id as u64, distance(query, vector, metric)))
                .collect();
            expected.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
            expected.truncate(K);
            if hits.len() != expected.len()
                || hits.iter().zip(&expected).any(|(hit, &(id, d))| {
                    hit.id != id || (hit.distance - d).abs() > 1e-9 * d.abs().max(1.0)
                })
            {
                return Err("exact baseline did not match independent full-sort oracle".into());
            }
            oracle.push(expected);
        }
        for (quantization, name) in [
            (Quantization::None, "none"),
            (Quantization::ScalarU8, "scalar_u8"),
        ] {
            let options = AnnOptions {
                quantization,
                rerank_multiplier: 4,
            };
            let started = Instant::now();
            db.build_hnsw_with_options(config, metric, options)?;
            let build_ms = started.elapsed().as_secs_f64() * 1000.0;
            let memory = db.ann_memory(config, metric, options)?;
            black_box(db.search_approximate_ready_with_options(
                &queries[0],
                K,
                100,
                config,
                options,
                &[],
                metric,
            )?);
            let mut samples = Vec::new();
            let (mut common, mut denominator, mut evaluations, mut quantized, mut reranked) =
                (0, 0, 0u64, 0u64, 0u64);
            for (query, expected) in queries.iter().zip(&oracle) {
                let started = Instant::now();
                let (hits, stats) = black_box(db.search_approximate_ready_with_options(
                    query,
                    K,
                    100,
                    config,
                    options,
                    &[],
                    metric,
                )?);
                samples.push(started.elapsed().as_secs_f64() * 1000.0);
                for hit in &hits {
                    let vector = vectors
                        .get(hit.id as usize)
                        .ok_or("ANN returned a missing record")?;
                    let expected_distance = distance(query, vector, metric);
                    if (expected_distance - hit.distance).abs()
                        > 1e-9 * expected_distance.abs().max(1.0)
                    {
                        return Err("returned distance was not exact".into());
                    }
                }
                common += hits
                    .iter()
                    .filter(|hit| expected.iter().any(|&(id, _)| id == hit.id))
                    .count();
                denominator += expected.len();
                evaluations += stats.distance_evaluations;
                quantized += stats.quantized_distance_evaluations;
                reranked += stats.reranked_candidates;
            }
            if !first {
                println!(",");
            }
            first = false;
            println!(
                "{{\"metric\":\"{metric_name}\",\"quantization\":\"{name}\",\"rerank_multiplier\":4,\"index_build_ms\":{build_ms},\"recall_at_k\":{},\"recall_denominator\":{denominator},\"query_p50_ms\":{},\"query_p95_ms\":{},\"exact_query_p50_ms\":{},\"query_samples_ms\":{samples:?},\"exact_samples_ms\":{exact_samples:?},\"graph_link_bytes\":{},\"code_bytes\":{},\"codebook_bytes\":{},\"buffer_vector_bytes\":{},\"resident_vector_bytes\":{},\"exact_distance_evaluations\":{evaluations},\"quantized_distance_evaluations\":{quantized},\"reranked_candidates\":{reranked}}}",
                common as f64 / denominator as f64,
                percentile(&samples, 0.50),
                percentile(&samples, 0.95),
                percentile(&exact_samples, 0.50),
                memory.graph_link_bytes,
                memory.code_bytes,
                memory.codebook_bytes,
                memory.buffer_vector_bytes,
                memory.resident_vector_bytes,
            );
        }
    }
    let filter = [Condition::Eq {
        field: "group".into(),
        value: MetadataValue::Integer(42),
    }];
    let (before, before_stats) = db.collection("default")?.search_with_stats(
        &queries[0],
        K,
        &filter,
        DistanceMetric::SquaredL2,
    )?;
    let mut settings = CollectionSettings::default();
    settings.metadata_indexes = vec![MetadataIndexSpec {
        field: "group".into(),
        kind: MetadataIndexKind::Eq,
    }];
    db.set_collection_settings("default", settings)?;
    let planned =
        db.collection("default")?
            .search_auto_with_settings(&queries[0], K, &filter, 100)?;
    if before != planned.hits || planned.stats.records_scanned >= before_stats.records_scanned {
        return Err("metadata index did not preserve results and narrow candidates".into());
    }
    println!(
        "],\"metadata_index\":{{\"strategy\":\"{}\",\"scan_records\":{},\"indexed_records\":{},\"matching_top_k\":{},\"identical_results\":true}}}}",
        planned.plan.strategy.name(),
        before_stats.records_scanned,
        planned.stats.records_scanned,
        planned.hits.len()
    );
    db.close()?;
    Ok(())
}
