//! TEST-010: deterministic ANN quality sampling, budget skip and stale nodes.

use vlite::quality::QualitySampleOptions;
use vlite::{
    AnnOptions, CollectionSettings, CollectionSpec, Database, DistanceMetric, Error, FormatVersion,
    HnswConfig, Mutation, Quantization,
};

const DIMS: usize = 16;
const LIVE: u64 = 1000;

fn vector(id: u64) -> Vec<f32> {
    (0..DIMS)
        .map(|d| ((id as f32 * 0.618 + d as f32 * 1.7).sin() * 3.0) + (id % 7) as f32)
        .collect()
}

fn config() -> HnswConfig {
    HnswConfig {
        m: 12,
        ef_construction: 80,
        seed: 9,
    }
}

fn load(db: &mut Database) {
    let operations: Vec<Mutation> = (0..LIVE)
        .map(|id| Mutation::Put {
            id,
            vector: vector(id),
            payload: vec![],
        })
        .collect();
    db.write_batch(&operations).unwrap();
}

#[test]
fn sampling_is_deterministic_bounded_and_ready_only() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = Database::create(directory.path().join("db.vlite"), DIMS).unwrap();
    load(&mut db);
    let metric = DistanceMetric::SquaredL2;
    let sample = QualitySampleOptions {
        seed: 42,
        ..QualitySampleOptions::default()
    };
    for options in [
        AnnOptions::default(),
        AnnOptions {
            quantization: Quantization::ScalarU8,
            rerank_multiplier: 4,
        },
    ] {
        assert!(matches!(
            db.ann_quality(config(), metric, options, sample),
            Err(Error::IndexNotReady)
        ));
        db.build_hnsw_with_options(config(), metric, options)
            .unwrap();
        let first = db.ann_quality(config(), metric, options, sample).unwrap();
        assert_eq!(
            first,
            db.ann_quality(config(), metric, options, sample).unwrap()
        );
        assert_eq!(first.samples_used, 8);
        assert_eq!(first.skipped_reason, None);
        assert!(first.recall_at_k.unwrap() >= 0.8, "{first:?}");
        assert!(first.recall_at_k.unwrap() <= 1.0);
        // Exact leg alone scores every record once per sample.
        assert!(first.distance_evaluations >= 8 * LIVE);
        if options.quantization == Quantization::None {
            assert!(
                db.ann_quality(
                    config(),
                    metric,
                    options,
                    QualitySampleOptions { seed: 7, ..sample }
                )
                .unwrap()
                .recall_at_k
                .is_some()
            );
        }
    }

    // Budget: samples × live × dims must not exceed the cap.
    let options = AnnOptions {
        quantization: Quantization::ScalarU8,
        rerank_multiplier: 4,
    };
    let exact_budget = 8 * LIVE * DIMS as u64;
    let skipped = db
        .ann_quality(
            config(),
            metric,
            options,
            QualitySampleOptions {
                max_distance_evaluations: exact_budget - 1,
                ..sample
            },
        )
        .unwrap();
    assert_eq!(skipped.skipped_reason, Some("budget"));
    assert_eq!(
        (
            skipped.recall_at_k,
            skipped.samples_used,
            skipped.distance_evaluations
        ),
        (None, 0, 0)
    );
    assert!(
        db.ann_quality(
            config(),
            metric,
            options,
            QualitySampleOptions {
                max_distance_evaluations: exact_budget,
                ..sample
            }
        )
        .unwrap()
        .recall_at_k
        .is_some()
    );

    // Samples never exceed live records.
    let all = db
        .ann_quality(
            config(),
            metric,
            options,
            QualitySampleOptions {
                samples: 32,
                k: 100,
                ef_search: 100,
                ..sample
            },
        )
        .unwrap();
    assert_eq!(all.samples_used, 32);

    for bad in [
        QualitySampleOptions {
            samples: 0,
            ..sample
        },
        QualitySampleOptions {
            samples: 33,
            ..sample
        },
        QualitySampleOptions { k: 0, ..sample },
        QualitySampleOptions { k: 101, ..sample },
    ] {
        assert!(matches!(
            db.ann_quality(config(), metric, options, bad),
            Err(Error::InvalidQuery(_))
        ));
    }
    assert!(matches!(
        db.ann_quality(
            config(),
            metric,
            options,
            QualitySampleOptions {
                ef_search: 5,
                ..sample
            }
        ),
        Err(Error::InvalidEfSearch)
    ));
}

#[test]
fn stale_nodes_count_replaced_and_deleted_graph_members() {
    let directory = tempfile::tempdir().unwrap();
    let mut db =
        Database::create_with_format(directory.path().join("db.vlite"), DIMS, FormatVersion::V4)
            .unwrap();
    db.create_collection(
        "items",
        CollectionSpec {
            dimensions: DIMS,
            settings: CollectionSettings::default(),
        },
    )
    .unwrap();
    let mut items = db.collection_mut("items").unwrap();
    items
        .write_batch(
            &(0..200)
                .map(|id| Mutation::Put {
                    id,
                    vector: vector(id),
                    payload: vec![],
                })
                .collect::<Vec<_>>(),
        )
        .unwrap();
    let metric = DistanceMetric::SquaredL2;
    let options = AnnOptions::default();
    items
        .build_hnsw_with_options(config(), metric, options)
        .unwrap();
    let health = items.hnsw_health(config(), metric, options).unwrap();
    assert_eq!(
        (health.graph_nodes, health.stale_nodes, health.buffered_ids),
        (200, 0, 0)
    );
    for id in 0..10 {
        items.delete(id).unwrap();
    }
    for id in 10..15 {
        items.put(id, &vector(id + 1000), b"").unwrap();
    }
    items.put(10, &vector(10), b"").unwrap();
    items.put(5000, &vector(5000), b"").unwrap();
    items.delete(4242).unwrap();
    let health = items.hnsw_health(config(), metric, options).unwrap();
    assert_eq!(
        (health.graph_nodes, health.stale_nodes, health.buffered_ids),
        (200, 15, 16)
    );
    let report = items
        .ann_quality(config(), metric, options, QualitySampleOptions::default())
        .unwrap();
    assert_eq!(report.samples_used, 8);
    // Rebuilding clears staleness.
    items
        .build_hnsw_with_options(config(), metric, options)
        .unwrap();
    let health = items.hnsw_health(config(), metric, options).unwrap();
    assert_eq!(
        (health.graph_nodes, health.stale_nodes, health.buffered_ids),
        (191, 0, 0)
    );
    // The default collection has no index.
    assert!(matches!(
        db.hnsw_health(config(), metric, options),
        Err(Error::IndexNotReady)
    ));
}
