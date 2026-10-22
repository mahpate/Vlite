#![cfg(feature = "gpu")]

use tempfile::tempdir;
use vlite::{Database, GpuExactSearcher};

const DIMS: usize = 33;

fn vector(seed: u64) -> Vec<f32> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xABCD;
    (0..DIMS)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            ((state >> 11) as f64 / (1u64 << 53) as f64) as f32 * 4.0 - 2.0
        })
        .collect()
}

#[test]
fn gpu_matches_cpu_or_skips_without_adapter() {
    let directory = tempdir().unwrap();
    let mut db = Database::create(directory.path().join("db.vlite"), DIMS).unwrap();
    for id in 0..700u64 {
        db.put(id, &vector(id + 1), b"").unwrap();
    }
    db.delete(5).unwrap();

    let searcher = GpuExactSearcher::new();
    let Some(gpu) = searcher.as_ref() else {
        eprintln!("no GPU adapter; verifying CPU fallback only");
        let query = vector(99_999);
        let fallback = GpuExactSearcher::search_or_cpu(None, &db, &query, 5).unwrap();
        assert_eq!(fallback, db.search(&query, 5).unwrap());
        return;
    };
    eprintln!("GPU adapter: {}", gpu.adapter_name());
    for q in 0..10u64 {
        let query = vector(50_000 + q);
        let cpu = db.search(&query, 10).unwrap();
        let hits = gpu.search(&db, &query, 10).unwrap();
        assert_eq!(hits.len(), cpu.len());
        for (g, c) in hits.iter().zip(&cpu) {
            // f32 GPU ranking may swap near-ties; distances are re-scored in f64.
            assert!((g.distance - c.distance).abs() <= 1e-3 * (1.0 + c.distance));
        }
        assert!(hits.iter().all(|hit| hit.id != 5));
    }
    assert!(gpu.search(&db, &vector(1), 0).unwrap().is_empty());
    assert!(gpu.search(&db, &[1.0; 3], 5).is_err());
}
