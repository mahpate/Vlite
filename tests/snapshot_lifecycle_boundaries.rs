//! Restart coverage across numeric-decoder and digest scratch boundaries.
use vlite::{AnnOptions, AnnSnapshotLoad, Database, DistanceMetric, HnswConfig, Quantization};

#[test]
fn mixed_write_snapshots_restart_across_numeric_scratch_boundaries() {
    let directory = tempfile::Builder::new()
        .prefix("snapshot-lifecycle-boundaries-")
        .tempdir_in("target")
        .unwrap();
    let config = HnswConfig {
        m: 2,
        ef_construction: 8,
        ..HnswConfig::default()
    };
    for dimensions in [511, 512, 513, 1023, 1024, 1025, 4097] {
        for metric in [DistanceMetric::SquaredL2, DistanceMetric::Cosine] {
            for quantization in [Quantization::None, Quantization::ScalarU8] {
                let options = AnnOptions {
                    quantization,
                    ..AnnOptions::default()
                };
                let path = directory
                    .path()
                    .join(format!("{dimensions}-{metric:?}-{quantization:?}.vlite"));
                let snapshot = Database::ann_snapshot_path(&path);
                let mut db = Database::create(&path, dimensions).unwrap();
                let vector = |id: u64| {
                    (0..dimensions)
                        .map(|component| ((component as u64 * 13 + id * 7) % 31) as f32 - 15.0)
                        .collect::<Vec<_>>()
                };
                for id in 0..8 {
                    db.put(id, &vector(id), b"").unwrap();
                }
                db.build_hnsw_with_options(config, metric, options).unwrap();
                db.put(0, &vector(20), b"replaced").unwrap();
                db.delete(1).unwrap();
                db.put(2, &vec![0.0; dimensions], b"zero transition")
                    .unwrap();
                db.put(9, &vector(9), b"inserted").unwrap();
                let query = vector(12);
                let before = db
                    .search_approximate_ready_with_options(
                        &query,
                        8,
                        32,
                        config,
                        options,
                        &[],
                        metric,
                    )
                    .unwrap();
                db.save_ann_snapshot(&snapshot).unwrap();
                let bytes = std::fs::read(&snapshot).unwrap();
                db.save_ann_snapshot(&snapshot).unwrap();
                assert_eq!(std::fs::read(&snapshot).unwrap(), bytes);
                db.close().unwrap();

                let mut db = Database::open(&path).unwrap();
                assert_eq!(
                    db.load_ann_snapshot(&snapshot).unwrap(),
                    AnnSnapshotLoad::Loaded { indexes: 1 }
                );
                let after = db
                    .search_approximate_ready_with_options(
                        &query,
                        8,
                        32,
                        config,
                        options,
                        &[],
                        metric,
                    )
                    .unwrap();
                let bits = |hits: &[vlite::SearchHit]| {
                    hits.iter()
                        .map(|hit| (hit.id, hit.distance.to_bits()))
                        .collect::<Vec<_>>()
                };
                assert_eq!(bits(&before.0), bits(&after.0));
                assert_eq!(before.1, after.1);
                db.close().unwrap();
            }
        }
    }
}
