use std::collections::{BTreeMap, BTreeSet};
use vlite::{Database, DistanceMetric as M, Error, HnswConfig, Mutation};

fn score(a: &[f32], b: &[f32]) -> Option<f64> {
    let dot: f64 = a
        .iter()
        .zip(b)
        .map(|(&a, &b)| f64::from(a) * f64::from(b))
        .sum();
    let an: f64 = a.iter().map(|&v| f64::from(v).powi(2)).sum();
    let bn: f64 = b.iter().map(|&v| f64::from(v).powi(2)).sum();
    (an > 0.0 && bn > 0.0).then(|| 1.0 - (dot / (an.sqrt() * bn.sqrt())).clamp(-1.0, 1.0))
}

fn verify(db: &Database, model: &BTreeMap<u64, Vec<f32>>, queries: &[Vec<f32>]) {
    let mut recalled = 0;
    let mut total = 0;
    for query in queries {
        let mut expected: Vec<_> = model
            .iter()
            .filter_map(|(&id, v)| score(v, query).map(|d| (id, d)))
            .collect();
        expected.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        expected.truncate(10);
        let exact = db.search_with_metric(query, 10, M::Cosine).unwrap();
        assert_eq!(
            exact.iter().map(|h| (h.id, h.distance)).collect::<Vec<_>>(),
            expected
        );
        let hits = db
            .search_approximate_ready_with_metric(query, 10, 1000, HnswConfig::default(), M::Cosine)
            .unwrap();
        assert_eq!(hits.len(), expected.len());
        let mut ids = BTreeSet::new();
        for hit in &hits {
            assert!(ids.insert(hit.id));
            assert_eq!(
                Some(hit.distance),
                score(model.get(&hit.id).expect("stale ID"), query)
            );
            recalled += usize::from(expected.iter().any(|&(id, _)| id == hit.id));
        }
        assert!(hits.windows(2).all(|h| {
            h[0].distance
                .total_cmp(&h[1].distance)
                .then(h[0].id.cmp(&h[1].id))
                .is_le()
        }));
        total += expected.len();
    }
    assert!(
        recalled * 100 >= total * 95,
        "fixed-workload recall {recalled}/{total} below 95%"
    );
}

#[test]
fn seeded_cosine_histories_preserve_current_scores_readiness_and_file_bytes() {
    for seed in [42, 1234, 0xdead_beef] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cosine.vlite");
        let mut db = Database::create(&path, 33).unwrap();
        let mut state: u64 = seed;
        let mut vector = || {
            (0..33)
                .map(|_| {
                    state = state.wrapping_mul(6364136223846793005u64).wrapping_add(1);
                    ((state >> 32) % 101) as f32 - 50.0
                })
                .collect::<Vec<_>>()
        };
        let mut model: BTreeMap<_, _> = (0..180).map(|id| (id, vector())).collect();
        model.insert(u64::MAX, vec![0.0; 33]);
        let queries: Vec<_> = (0..12).map(|_| vector()).collect();
        db.write_batch(
            &model
                .iter()
                .map(|(&id, v)| Mutation::Put {
                    id,
                    vector: v.clone(),
                    payload: vec![],
                })
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let config = HnswConfig::default();
        db.build_hnsw_with_metric(config, M::Cosine).unwrap();
        verify(&db, &model, &queries);
        for step in 0..45 {
            let id = step % 15;
            let replacement = if step % 3 == 0 {
                vec![0.0; 33]
            } else {
                vector()
            };
            db.write_batch(&[
                Mutation::Delete { id },
                Mutation::Put {
                    id,
                    vector: vector(),
                    payload: vec![],
                },
                Mutation::Put {
                    id,
                    vector: replacement.clone(),
                    payload: vec![],
                },
            ])
            .unwrap();
            model.insert(id, replacement);
            if step % 5 == 0 {
                let id = 100 + step;
                db.delete(id).unwrap();
                model.remove(&id);
                let invalid = [
                    Mutation::Delete { id: 0 },
                    Mutation::Put {
                        id: 9999,
                        vector: vec![1.0],
                        payload: vec![],
                    },
                ];
                assert!(db.write_batch(&invalid).is_err());
                verify(&db, &model, &queries);
            }
        }
        // An excluded zero record can become a live buffered cosine candidate.
        let fresh = queries[0].clone();
        db.put(u64::MAX, &fresh, b"").unwrap();
        model.insert(u64::MAX, fresh);
        verify(&db, &model, &queries);
        assert!(matches!(
            db.search_approximate_ready(&queries[0], 10, 1000, config),
            Err(Error::IndexNotReady)
        ));
        assert!(matches!(
            db.search_approximate_ready_with_metric(
                &queries[0],
                10,
                1000,
                HnswConfig { seed: 43, ..config },
                M::Cosine
            ),
            Err(Error::IndexNotReady)
        ));
        assert!(matches!(
            db.build_hnsw_with_metric(config, M::InnerProduct),
            Err(Error::UnsupportedAnnMetric)
        ));
        verify(&db, &model, &queries);
        // Reopen requires an explicit rebuild; none of the graph is persisted.
        db.close().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let mut db = Database::open(&path).unwrap();
        assert!(matches!(
            db.search_approximate_ready_with_metric(&queries[0], 10, 1000, config, M::Cosine),
            Err(Error::IndexNotReady)
        ));
        db.build_hnsw_with_metric(config, M::Cosine).unwrap();
        verify(&db, &model, &queries);
        db.close().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let mut db = Database::open(&path).unwrap();
        db.build_hnsw_with_metric(config, M::Cosine).unwrap();
        for id in 1000..1256 {
            db.put(id, &queries[0], b"").unwrap();
        }
        assert!(
            db.search_approximate_ready_with_metric(&queries[0], 1, 1000, config, M::Cosine)
                .is_ok()
        );
        db.put(1256, &queries[0], b"").unwrap();
        assert!(matches!(
            db.search_approximate_ready_with_metric(&queries[0], 1, 1000, config, M::Cosine),
            Err(Error::IndexNotReady)
        ));
        assert!(
            !db.search_approximate_with_metric(&queries[0], 1, 1000, config, M::Cosine)
                .unwrap()
                .is_empty()
        );
        assert!(db.search_approximate(&queries[0], 1, 1000, config).is_ok());
        assert!(matches!(
            db.search_approximate_ready_with_metric(&queries[0], 1, 1000, config, M::Cosine),
            Err(Error::IndexNotReady)
        ));
    }
}

#[test]
fn cosine_zero_extreme_ties_and_empty_index_transitions_are_explicit() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::create(dir.path().join("zero.vlite"), 2).unwrap();
    let c = HnswConfig::default();
    for k in [0, 1] {
        assert!(matches!(
            db.search_approximate_with_metric(&[0.0, 0.0], k, 10, c, M::Cosine),
            Err(Error::ZeroNormQuery)
        ));
        assert!(matches!(
            db.search_approximate_ready_with_metric(&[0.0, 0.0], k, 10, c, M::Cosine),
            Err(Error::ZeroNormQuery)
        ));
        assert!(matches!(
            db.search_approximate_ready_with_metric(&[1.0, 0.0], k, 10, c, M::InnerProduct),
            Err(Error::UnsupportedAnnMetric)
        ));
    }
    db.put(0, &[0.0, 0.0], b"").unwrap();
    db.build_hnsw_with_metric(c, M::Cosine).unwrap();
    assert!(
        db.search_approximate_ready_with_metric(&[1.0, 0.0], 10, 10, c, M::Cosine)
            .unwrap()
            .is_empty()
    );
    for (id, v) in [
        (1, [f32::MAX, 0.0]),
        (2, [f32::from_bits(1), 0.0]),
        (3, [-f32::MAX, 0.0]),
    ] {
        db.put(id, &v, b"").unwrap();
    }
    let model = BTreeMap::from([
        (0, vec![0.0, 0.0]),
        (1, vec![f32::MAX, 0.0]),
        (2, vec![f32::from_bits(1), 0.0]),
        (3, vec![-f32::MAX, 0.0]),
    ]);
    verify(&db, &model, &[vec![1.0, 0.0]]);
    db.build_hnsw_with_metric(c, M::Cosine).unwrap();
    verify(
        &db,
        &model,
        &[vec![f32::MAX, 0.0], vec![f32::from_bits(1), 0.0]],
    );
    db.put(1, &[0.0, 0.0], b"").unwrap();
    db.delete(2).unwrap();
    db.delete(3).unwrap();
    assert!(
        db.search_approximate_ready_with_metric(&[1.0, 0.0], 10, 1000, c, M::Cosine)
            .unwrap()
            .is_empty()
    );
}
