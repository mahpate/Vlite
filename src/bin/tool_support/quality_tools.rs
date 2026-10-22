use crate::tool_support::{Result, records};
use serde_json::{Value, json};
use std::{collections::BTreeSet, time::Instant};
use vlite::quality::QualitySampleOptions;
use vlite::{AnnOptions, Database, DistanceMetric, HnswConfig, SearchHit};
pub fn sample(db: &Database, collection: &str, count: usize) -> Result<Vec<Vec<f32>>> {
    let rows = records(db.collection(collection)?)?;
    if rows.is_empty() {
        return Err("cannot evaluate an empty collection".into());
    }
    let mut positions = BTreeSet::new();
    let mut state = 0u64;
    while positions.len() < count.min(rows.len()) {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        positions.insert((z % rows.len() as u64) as usize);
    }
    Ok(positions
        .into_iter()
        .map(|i| rows[i].1.vector.clone())
        .collect())
}
// Different binaries consume the metrics and result lists independently.
#[allow(dead_code)]
pub struct Evaluation {
    pub report: Value,
    pub hits: Vec<Vec<SearchHit>>,
    pub recall: f64,
    pub latency: f64,
    pub memory: u64,
}
pub fn overlap(a: &[SearchHit], b: &[SearchHit]) -> (f64, f64, Option<f64>) {
    let pairs: Vec<_> = a
        .iter()
        .enumerate()
        .filter_map(|(i, h)| {
            b.iter()
                .position(|other| other.id == h.id)
                .map(|j| (i as f64, j as f64))
        })
        .collect();
    let recall = if a.is_empty() {
        1.0
    } else {
        pairs.len() as f64 / a.len() as f64
    };
    let union = a.len() + b.len() - pairs.len();
    let jaccard = if union == 0 {
        1.0
    } else {
        pairs.len() as f64 / union as f64
    };
    // Pearson correlation of original ranks on shared IDs (Spearman with no score ties).
    let n = pairs.len() as f64;
    let mean_a = pairs.iter().map(|p| p.0).sum::<f64>() / n;
    let mean_b = pairs.iter().map(|p| p.1).sum::<f64>() / n;
    let covariance = pairs
        .iter()
        .map(|(a, b)| (a - mean_a) * (b - mean_b))
        .sum::<f64>();
    let variance = (pairs.iter().map(|p| (p.0 - mean_a).powi(2)).sum::<f64>()
        * pairs.iter().map(|p| (p.1 - mean_b).powi(2)).sum::<f64>())
    .sqrt();
    (
        recall,
        jaccard,
        if pairs.len() < 2 || variance == 0.0 {
            None
        } else {
            Some(covariance / variance)
        },
    )
}
pub fn evaluate(
    db: &mut Database,
    collection: &str,
    queries: &[Vec<f32>],
    config: HnswConfig,
    ef: usize,
    k: usize,
) -> Result<Evaluation> {
    let start = Instant::now();
    db.collection_mut(collection)?
        .build_hnsw_with_metric(config, DistanceMetric::SquaredL2)?;
    let build = start.elapsed().as_secs_f64() * 1000.0;
    let c = db.collection(collection)?;
    // Reuse the engine's budgeted quality and component-memory APIs, not a second index implementation.
    let library = c.ann_quality(
        config,
        DistanceMetric::SquaredL2,
        AnnOptions::default(),
        QualitySampleOptions {
            samples: queries.len().min(32),
            k,
            ef_search: ef,
            ..Default::default()
        },
    )?;
    let memory = c.ann_memory(config, DistanceMetric::SquaredL2, AnnOptions::default())?;
    let memory_bytes = memory.graph_link_bytes
        + memory.code_bytes
        + memory.codebook_bytes
        + memory.buffer_vector_bytes
        + memory.resident_vector_bytes;
    let mut hits = Vec::new();
    let (mut latencies, mut recall, mut intersection, mut ranks) =
        (Vec::new(), 0.0, 0.0, Vec::new());
    for query in queries {
        let exact = c.search(query, k)?;
        let start = Instant::now();
        let approximate = c.search_approximate_ready_filtered_with_metric(
            query,
            k,
            ef,
            config,
            &[],
            DistanceMetric::SquaredL2,
        )?;
        latencies.push(start.elapsed().as_secs_f64() * 1_000_000.0);
        let (r, o, s) = overlap(&exact, &approximate);
        recall += r;
        intersection += o;
        if let Some(s) = s {
            ranks.push(s);
        }
        hits.push(approximate);
    }
    let n = queries.len() as f64;
    let latency = latencies.iter().sum::<f64>() / n;
    recall /= n;
    latencies.sort_by(f64::total_cmp);
    Ok(Evaluation {
        report: json!({"m":config.m,"ef_construction":config.ef_construction,"ef_search":ef,"seed":config.seed,"samples":queries.len(),"k":k,"recall_at_k":recall,"overlap_jaccard":intersection/n,"rank_correlation":if ranks.is_empty(){None}else{Some(ranks.iter().sum::<f64>()/ranks.len() as f64)},"query_mean_us":latency,"query_p95_us":latencies[((latencies.len()*95).div_ceil(100)-1).min(latencies.len()-1)],"build_ms":build,"memory_bytes_estimate":memory_bytes,"library_sample_recall_at_k":library.recall_at_k,"library_sample_skipped_reason":library.skipped_reason}),
        hits,
        recall,
        latency,
        memory: memory_bytes,
    })
}
fn human_candidate(label: &str, report: &Value) {
    println!(
        "{label}: M={} ef_construction={} ef_search={} recall@{}={} Jaccard={} rank-correlation={}",
        report["m"],
        report["ef_construction"],
        report["ef_search"],
        report["k"],
        report["recall_at_k"],
        report["overlap_jaccard"],
        report["rank_correlation"]
    );
    println!(
        "  mean={} us p95={} us build={} ms component-memory={} bytes samples={}",
        report["query_mean_us"],
        report["query_p95_us"],
        report["build_ms"],
        report["memory_bytes_estimate"],
        report["samples"]
    );
}
pub fn print_report(report: &Value, json_output: bool) {
    if json_output {
        println!("{report}");
    } else if let Some(candidates) = report["candidates"].as_array() {
        println!(
            "L2 HNSW tuning: target recall={} (stored-vector sample, seed 0)",
            report["target_recall"]
        );
        println!(
            "Cost: mean query us + {} * component MiB",
            report["memory_weight"]
        );
        for candidate in candidates {
            human_candidate("Candidate", candidate);
        }
        if report["recommended"].is_null() {
            println!("No configuration meets target recall.");
        } else {
            human_candidate("Recommended", &report["recommended"]);
        }
    } else {
        println!("L2 ANN quality (stored-vector sample, seed 0)");
        human_candidate("Primary", &report["primary"]);
        if !report["secondary"].is_null() {
            human_candidate("Secondary", &report["secondary"]);
            println!(
                "Comparison: overlap@k={} Jaccard={} rank-correlation={}",
                report["comparison"]["overlap_at_k"],
                report["comparison"]["overlap_jaccard"],
                report["comparison"]["rank_correlation"]
            );
        }
    }
}
pub fn validate(samples: usize, k: usize) -> Result<()> {
    if !(1..=32).contains(&samples) || !(1..=100).contains(&k) {
        return Err("samples must be 1..32 and k 1..100".into());
    }
    Ok(())
}
