//! TEST-009: ScalarU8 quantized ANN with exact rerank.

use std::time::{Duration, Instant};
use vlite::{
    AnnOptions, Database, DistanceMetric, Error, HnswConfig, HnswMaintenancePolicy,
    HnswRebuildLimits, HnswRebuildStatus, Quantization, SearchHit,
};

const DIMS: usize = 32;
const LIVE: u64 = 2000;

struct Rng(u64);

impl Rng {
    fn unit(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
    }
}

fn clustered(count: u64, seed: u64) -> Vec<Vec<f32>> {
    let mut rng = Rng(seed);
    let centers: Vec<Vec<f32>> = (0..20)
        .map(|_| (0..DIMS).map(|_| rng.unit() * 4.0).collect())
        .collect();
    (0..count)
        .map(|i| {
            centers[i as usize % centers.len()]
                .iter()
                .map(|c| c + rng.unit() * 0.5)
                .collect()
        })
        .collect()
}

fn config() -> HnswConfig {
    HnswConfig {
        m: 12,
        ef_construction: 100,
        seed: 3,
    }
}

fn u8_options() -> AnnOptions {
    AnnOptions {
        quantization: Quantization::ScalarU8,
        rerank_multiplier: 4,
    }
}

fn load(path: &std::path::Path, vectors: &[Vec<f32>]) -> Database {
    let mut db = Database::create(path, DIMS).unwrap();
    let operations: Vec<vlite::Mutation> = vectors
        .iter()
        .enumerate()
        .map(|(id, vector)| vlite::Mutation::Put {
            id: id as u64,
            vector: vector.clone(),
            payload: vec![],
        })
        .collect();
    for chunk in operations.chunks(1000) {
        db.write_batch(chunk).unwrap();
    }
    db
}

fn squared_l2(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(&x, &y)| (f64::from(x) - f64::from(y)).powi(2))
        .sum()
}

fn assert_exact_distances(db: &Database, query: &[f32], hits: &[SearchHit]) {
    for hit in hits {
        let record = db.get(hit.id).unwrap().unwrap();
        let exact = squared_l2(&record.vector, query);
        assert!(
            (hit.distance - exact).abs() <= 1e-9 * exact.max(1.0),
            "{hit:?} vs {exact}"
        );
    }
}

#[test]
fn scalar_u8_recall_exact_rerank_readiness_and_memory() {
    let directory = tempfile::tempdir().unwrap();
    let vectors = clustered(LIVE, 11);
    let mut db = load(&directory.path().join("db.vlite"), &vectors);
    let metric = DistanceMetric::SquaredL2;
    db.build_hnsw_with_options(config(), metric, u8_options())
        .unwrap();

    let mut rng = Rng(99);
    let (mut found, mut total) = (0, 0);
    for q in 0..50 {
        let query: Vec<f32> = vectors[(q * 37) % vectors.len()]
            .iter()
            .map(|v| v + rng.unit() * 0.3)
            .collect();
        let (hits, stats) = db
            .search_approximate_ready_with_options(
                &query,
                10,
                100,
                config(),
                u8_options(),
                &[],
                metric,
            )
            .unwrap();
        assert_eq!(hits.len(), 10);
        assert!(hits.windows(2).all(|w| w[0].distance <= w[1].distance));
        assert_exact_distances(&db, &query, &hits);
        assert!(stats.quantized_distance_evaluations > 0);
        assert_eq!(stats.reranked_candidates, 40);
        assert_eq!(stats.distance_evaluations, 40);
        let exact = db.search(&query, 10).unwrap();
        found += hits
            .iter()
            .filter(|h| exact.iter().any(|e| e.id == h.id))
            .count();
        total += exact.len();
    }
    let recall = found as f64 / total as f64;
    assert!(recall >= 0.80, "recall@10 {recall}");

    // Per-query rerank width = min(ef, k × multiplier), never below k.
    let query = &vectors[5];
    for (multiplier, ef, expected) in [(1, 100, 10), (16, 100, 100), (16, 64, 64)] {
        let options = AnnOptions {
            rerank_multiplier: multiplier,
            ..u8_options()
        };
        let (hits, stats) = db
            .search_approximate_ready_with_options(query, 10, ef, config(), options, &[], metric)
            .unwrap();
        assert_eq!(stats.reranked_candidates, expected);
        assert_eq!(
            hits[0],
            SearchHit {
                id: 5,
                distance: 0.0
            }
        );
    }
    for multiplier in [0, 17] {
        let options = AnnOptions {
            rerank_multiplier: multiplier,
            ..u8_options()
        };
        assert!(matches!(
            db.search_approximate_ready_with_options(query, 10, 64, config(), options, &[], metric),
            Err(Error::InvalidQuery(_))
        ));
    }

    // Readiness identity includes quantization; legacy APIs see no index.
    assert!(matches!(
        db.search_approximate_ready(query, 10, 64, config()),
        Err(Error::IndexNotReady)
    ));
    assert!(matches!(
        db.search_approximate_ready_with_options(
            query,
            10,
            64,
            config(),
            AnnOptions::default(),
            &[],
            metric
        ),
        Err(Error::IndexNotReady)
    ));
    let policy = HnswMaintenancePolicy::default();
    assert!(
        db.hnsw_maintenance_status_with_options(config(), metric, u8_options(), policy)
            .unwrap()
            .ready
    );
    assert!(
        !db.hnsw_maintenance_status(config(), metric, policy)
            .unwrap()
            .ready
    );

    // Component bytes only; resident f32 vectors are reported separately.
    let memory = db.ann_memory(config(), metric, u8_options()).unwrap();
    assert_eq!(memory.graph_nodes, LIVE);
    assert_eq!(memory.code_bytes, LIVE * DIMS as u64);
    assert_eq!(memory.codebook_bytes, DIMS as u64 * 16);
    assert_eq!(memory.buffer_vector_bytes, 0);
    assert_eq!(memory.resident_vector_bytes, LIVE * DIMS as u64 * 4);
    assert!(memory.graph_link_bytes > 0);
    assert!(matches!(
        db.ann_memory(config(), metric, AnnOptions::default()),
        Err(Error::IndexNotReady)
    ));

    // The default (unquantized) build is unchanged and stores no codes.
    db.build_hnsw_with_metric(config(), metric).unwrap();
    let plain = db
        .ann_memory(config(), metric, AnnOptions::default())
        .unwrap();
    assert_eq!((plain.code_bytes, plain.codebook_bytes), (0, 0));
    assert_eq!(plain.graph_nodes, LIVE);
    let (hits, stats) = db
        .search_approximate_ready_with_options(
            query,
            10,
            64,
            config(),
            AnnOptions::default(),
            &[],
            metric,
        )
        .unwrap();
    assert_eq!(
        hits,
        db.search_approximate_ready(query, 10, 64, config())
            .unwrap()
    );
    assert_eq!(
        (
            stats.quantized_distance_evaluations,
            stats.reranked_candidates
        ),
        (0, 0)
    );
}

#[test]
fn buffered_mutations_background_rebuild_and_snapshot_limits() {
    let directory = tempfile::tempdir().unwrap();
    let vectors = clustered(LIVE, 5);
    let mut db = load(&directory.path().join("db.vlite"), &vectors);
    let metric = DistanceMetric::SquaredL2;

    // ScalarU8 snapshots count one byte per component against the limit.
    let limits = HnswRebuildLimits {
        max_records: 100_000,
        max_snapshot_vector_bytes: LIVE as usize * DIMS,
    };
    assert!(matches!(
        db.start_hnsw_rebuild(config(), metric, limits),
        Err(Error::HnswRebuildTooLarge)
    ));
    db.start_hnsw_rebuild_with_options(config(), metric, u8_options(), limits)
        .unwrap();
    // Writes during the build are caught up into the published index.
    let far = vec![50.0; DIMS];
    db.put(9000, &far, b"").unwrap();
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        match db.poll_hnsw_rebuild().unwrap() {
            HnswRebuildStatus::Published => break,
            HnswRebuildStatus::Running => {}
            other => panic!("unexpected {other:?}"),
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    let search = |db: &Database, query: &[f32], k: usize| {
        db.search_approximate_ready_with_options(query, k, 64, config(), u8_options(), &[], metric)
            .unwrap()
            .0
    };
    assert_eq!(
        search(&db, &far, 1),
        [SearchHit {
            id: 9000,
            distance: 0.0
        }]
    );
    let health = db.hnsw_health(config(), metric, u8_options()).unwrap();
    assert_eq!(
        (health.graph_nodes, health.stale_nodes, health.buffered_ids),
        (LIVE, 0, 1)
    );

    // Replaced and deleted graph nodes become stale; results stay exact.
    let moved = vec![-50.0; DIMS];
    db.put(0, &moved, b"").unwrap();
    db.delete(1).unwrap();
    assert_eq!(
        search(&db, &moved, 1),
        [SearchHit {
            id: 0,
            distance: 0.0
        }]
    );
    let near_one = search(&db, &vectors[1], 10);
    assert!(near_one.iter().all(|hit| hit.id != 1 && hit.id != 0));
    assert_exact_distances(&db, &vectors[1], &near_one);
    let health = db.hnsw_health(config(), metric, u8_options()).unwrap();
    assert_eq!((health.stale_nodes, health.buffered_ids), (2, 3));
    let memory = db.ann_memory(config(), metric, u8_options()).unwrap();
    assert_eq!(memory.buffer_vector_bytes, 2 * DIMS as u64 * 4);
    assert_eq!(memory.resident_vector_bytes, LIVE * DIMS as u64 * 4);
}

#[test]
fn unbounded_k_on_small_graph_is_capped_for_both_quantizations() {
    use vlite::{Condition, Metadata, MetadataValue, Mutation};
    let metric = DistanceMetric::SquaredL2;
    let filter = [Condition::Eq {
        field: "even".into(),
        value: MetadataValue::Boolean(true),
    }];
    for options in [AnnOptions::default(), u8_options()] {
        let directory = tempfile::tempdir().unwrap();
        let mut db = Database::create(directory.path().join("db.vlite"), DIMS).unwrap();
        let vectors = clustered(24, 77);
        let operations: Vec<Mutation> = vectors
            .iter()
            .enumerate()
            .map(|(id, vector)| Mutation::PutWithMetadata {
                id: id as u64,
                vector: vector.clone(),
                payload: vec![],
                metadata: Metadata::from([("even".into(), MetadataValue::Boolean(id % 2 == 0))]),
            })
            .collect();
        db.write_batch(&operations).unwrap();
        db.build_hnsw_with_options(config(), metric, options)
            .unwrap();
        // Buffered changes: one replaced graph node and one new record.
        let even = Metadata::from([("even".into(), MetadataValue::Boolean(true))]);
        db.put_with_metadata(2, &vectors[3], b"", &even).unwrap();
        db.put_with_metadata(100, &vectors[5], b"", &even).unwrap();
        let live = 25;
        let query = &vectors[0];
        for conditions in [&filter[..], &[]] {
            // ef must cover min(k, live); k itself is unbounded.
            let (hits, stats) = db
                .search_approximate_ready_with_options(
                    query,
                    usize::MAX,
                    live,
                    config(),
                    options,
                    conditions,
                    metric,
                )
                .unwrap();
            let exact = db
                .search_filtered_with_metric(query, live, conditions, metric)
                .unwrap();
            assert_eq!(hits, exact, "{options:?} {conditions:?}");
            assert_eq!(hits.len(), if conditions.is_empty() { 25 } else { 13 });
            assert_eq!(stats.buffered_records_merged, 2);
            if options.quantization == Quantization::ScalarU8 {
                assert!(stats.reranked_candidates > 0);
            }
        }
    }
}

#[test]
fn cosine_scalar_u8_excludes_zero_vectors_and_reranks_exactly() {
    let directory = tempfile::tempdir().unwrap();
    let mut vectors = clustered(500, 21);
    vectors[3] = vec![0.0; DIMS];
    let mut db = load(&directory.path().join("db.vlite"), &vectors);
    let metric = DistanceMetric::Cosine;
    db.build_hnsw_with_options(config(), metric, u8_options())
        .unwrap();
    assert_eq!(
        db.ann_memory(config(), metric, u8_options())
            .unwrap()
            .graph_nodes,
        499
    );
    let (mut found, mut total) = (0, 0);
    for q in (0..500).step_by(25) {
        let query = &vectors[q + 1];
        let (hits, _) = db
            .search_approximate_ready_with_options(
                query,
                10,
                64,
                config(),
                u8_options(),
                &[],
                metric,
            )
            .unwrap();
        assert!(hits.iter().all(|hit| hit.id != 3));
        let exact = db.search_with_metric(query, 10, metric).unwrap();
        for hit in &hits {
            if let Some(e) = exact.iter().find(|e| e.id == hit.id) {
                assert!((e.distance - hit.distance).abs() < 1e-12);
                found += 1;
            }
        }
        total += exact.len();
    }
    assert!(found as f64 / total as f64 >= 0.8);
    assert!(matches!(
        db.build_hnsw_with_options(config(), DistanceMetric::InnerProduct, u8_options()),
        Err(Error::UnsupportedAnnMetric)
    ));
}
