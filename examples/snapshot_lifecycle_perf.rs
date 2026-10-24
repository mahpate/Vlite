//! Bounded snapshot save/load timings (including full validation).
//! Run with `cargo run --release --example snapshot_lifecycle_perf -- LABEL`.
//! Uses a project-local scratch directory and verifies bit-identical results.
use std::{hint::black_box, time::Instant};

use sha2::{Digest, Sha256};
use vlite::{
    AnnOptions, AnnSnapshotLoad, Database, DistanceMetric, HnswConfig, Mutation, Quantization,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let label = std::env::args().nth(1).unwrap_or_else(|| "run".into());
    let directory = tempfile::Builder::new()
        .prefix("snapshot-lifecycle-perf-")
        .tempdir_in("target")?;
    let config = HnswConfig {
        m: 4,
        ef_construction: 24,
        ..HnswConfig::default()
    };
    let dimensions = 512;
    let query = vec![0.25; dimensions];
    for quantization in [Quantization::None, Quantization::ScalarU8] {
        let options = AnnOptions {
            quantization,
            ..AnnOptions::default()
        };
        let path = directory.path().join(format!("{quantization:?}.vlite"));
        let snapshot = Database::ann_snapshot_path(&path);
        let mut db = Database::create(&path, dimensions)?;
        let mut seed = 7u64;
        let mut vector = || {
            (0..dimensions)
                .map(|_| {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    (seed >> 40) as f32 / (1u64 << 23) as f32 - 1.0
                })
                .collect()
        };
        let batch: Vec<_> = (0..1024)
            .map(|id| Mutation::Put {
                id,
                vector: vector(),
                payload: vec![],
            })
            .collect();
        db.write_batch(&batch)?;
        db.build_hnsw_with_options(config, DistanceMetric::Cosine, options)?;
        let changes: Vec<_> = (0..128)
            .map(|id| {
                if id % 4 == 0 {
                    Mutation::Delete { id }
                } else {
                    Mutation::Put {
                        id,
                        vector: vector(),
                        payload: vec![],
                    }
                }
            })
            .collect();
        db.write_batch(&changes)?;
        let before = db.search_approximate_ready_with_options(
            &query,
            10,
            40,
            config,
            options,
            &[],
            DistanceMetric::Cosine,
        )?;
        let bits = |hits: &[vlite::SearchHit]| {
            hits.iter()
                .map(|hit| (hit.id, hit.distance.to_bits()))
                .collect::<Vec<_>>()
        };
        let mut saves = Vec::new();
        for _ in 0..9 {
            let start = Instant::now();
            black_box(db.save_ann_snapshot(&snapshot)?);
            saves.push(start.elapsed().as_nanos());
        }
        db.close()?;
        let mut loads = Vec::new();
        let mut opens = Vec::new();
        for _ in 0..9 {
            let start = Instant::now();
            let mut db = Database::open(&path)?;
            opens.push(start.elapsed().as_nanos());
            let start = Instant::now();
            let outcome = black_box(db.load_ann_snapshot(&snapshot)?);
            loads.push(start.elapsed().as_nanos());
            assert_eq!(outcome, AnnSnapshotLoad::Loaded { indexes: 1 });
            let after = db.search_approximate_ready_with_options(
                &query,
                10,
                40,
                config,
                options,
                &[],
                DistanceMetric::Cosine,
            )?;
            assert_eq!(bits(&before.0), bits(&after.0));
            assert_eq!(before.1, after.1);
            db.close()?;
        }
        let digest = Sha256::digest(std::fs::read(&snapshot)?);
        println!(
            "label={label} mode={quantization:?} records=1024 dims={dimensions} buffered=128 trials=9 snapshot_bytes={} snapshot_sha256={digest:x} save_ns={saves:?} reopen_ns={opens:?} load_ns={loads:?} hits_and_stats_identical=true",
            std::fs::metadata(snapshot)?.len()
        );
    }
    Ok(())
}
