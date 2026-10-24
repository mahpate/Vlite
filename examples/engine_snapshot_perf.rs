//! Bounded deterministic rebuild-capture and replay measurements.
//! `cargo run --release --example engine_snapshot_perf -- LABEL`
use std::{hint::black_box, time::Instant};
use vlite::{Database, DistanceMetric, HnswConfig, HnswRebuildLimits, Mutation};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let label = std::env::args().nth(1).unwrap_or_else(|| "run".into());
    let directory = tempfile::Builder::new()
        .prefix("engine-perf-")
        .tempdir_in("target")?;
    let path = directory.path().join("data.vlite");
    let dimensions = 512;
    let mut db = Database::create(&path, dimensions)?;
    let rows: Vec<_> = (0..2048)
        .map(|id| Mutation::Put {
            id,
            vector: (0..dimensions)
                .map(|d| ((id * 31 + d as u64 * 7) % 997) as f32 / 997.0)
                .collect(),
            payload: vec![],
        })
        .collect();
    db.write_batch(&rows)?;
    let mut captures = Vec::new();
    for _ in 0..11 {
        let start = Instant::now();
        db.start_hnsw_rebuild(
            HnswConfig::default(),
            DistanceMetric::SquaredL2,
            HnswRebuildLimits::default(),
        )?;
        captures.push(start.elapsed().as_nanos());
        db.cancel_hnsw_rebuild();
        while db.poll_hnsw_rebuild()? == vlite::HnswRebuildStatus::Cancelling {
            std::thread::yield_now();
        }
    }
    db.close()?;
    let mut opens = Vec::new();
    for _ in 0..21 {
        let start = Instant::now();
        let db = Database::open(&path)?;
        opens.push(start.elapsed().as_nanos());
        assert_eq!(db.len(), 2048);
        black_box(db.get(2047)?);
        db.close()?;
    }
    println!(
        "label={label} records=2048 dimensions={dimensions} capture_ns={captures:?} reopen_ns={opens:?}"
    );
    Ok(())
}
