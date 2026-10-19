use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use vlite::{
    Database, Error, HnswConfig, MAX_HNSW_BUFFER_IDS, Metadata, MetadataValue, Mutation, Record,
    SearchHit,
};

fn random(state: &mut u64) -> u64 {
    *state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
    *state
}

fn record(state: &mut u64, step: i64) -> Record {
    Record {
        vector: (0..8)
            .map(|_| (random(state) >> 48) as f32 / 32768.0 - 1.0)
            .collect(),
        payload: step.to_le_bytes().to_vec(),
        metadata: Metadata::from([("generation".into(), MetadataValue::Integer(step))]),
    }
}

fn put(id: u64, record: &Record) -> Mutation {
    Mutation::PutWithMetadata {
        id,
        vector: record.vector.clone(),
        payload: record.payload.clone(),
        metadata: record.metadata.clone(),
    }
}

fn oracle(model: &BTreeMap<u64, Record>, query: &[f32], k: usize) -> Vec<SearchHit> {
    let mut hits: Vec<_> = model
        .iter()
        .map(|(&id, record)| SearchHit {
            id,
            distance: record
                .vector
                .iter()
                .zip(query)
                .map(|(&value, &query)| {
                    let difference = f64::from(value) - f64::from(query);
                    difference * difference
                })
                .sum(),
        })
        .collect();
    hits.sort_by(|a, b| a.distance.total_cmp(&b.distance).then(a.id.cmp(&b.id)));
    hits.truncate(k);
    hits
}

fn check(
    db: &Database,
    model: &BTreeMap<u64, Record>,
    queries: &[Vec<f32>],
    config: HnswConfig,
) -> Vec<Vec<SearchHit>> {
    assert_eq!(db.len(), model.len());
    for (&id, record) in model {
        assert_eq!(
            db.get(id).unwrap(),
            Some(record),
            "committed state for {id}"
        );
    }
    let mut recalled = 0;
    let mut total = 0;
    let results = queries
        .iter()
        .map(|query| {
            let exact = oracle(model, query, 7);
            assert_eq!(db.search(query, 7).unwrap(), exact);
            let hits = db.search_approximate_ready(query, 7, 1000, config).unwrap();
            assert_eq!(hits.len(), exact.len());
            let mut seen = BTreeSet::new();
            for hit in &hits {
                assert!(seen.insert(hit.id), "duplicate candidate");
                let record = model
                    .get(&hit.id)
                    .expect("deleted or uncommitted ID leaked");
                let distance: f64 = record
                    .vector
                    .iter()
                    .zip(query)
                    .map(|(&value, &query)| {
                        let difference = f64::from(value) - f64::from(query);
                        difference * difference
                    })
                    .sum();
                assert_eq!(hit.distance, distance, "stale vector for {}", hit.id);
                recalled += usize::from(exact.iter().any(|expected| expected.id == hit.id));
            }
            assert!(hits.windows(2).all(|pair| {
                pair[0]
                    .distance
                    .total_cmp(&pair[1].distance)
                    .then(pair[0].id.cmp(&pair[1].id))
                    .is_le()
            }));
            total += exact.len();
            hits
        })
        .collect();
    assert!(
        recalled * 100 >= total * 95,
        "fixed-workload recall {recalled}/{total} below 95%; not a general ANN guarantee"
    );
    results
}

fn not_ready(db: &Database, query: &[f32], config: HnswConfig) {
    for _ in 0..2 {
        assert!(matches!(
            db.search_approximate_ready(query, 7, 1000, config),
            Err(Error::IndexNotReady)
        ));
    }
}

#[test]
fn mixed_ann_history_matches_committed_model_across_overflow_rebuild_and_reopen() {
    for seed in [42, 1234, 0xdead_beef] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ann-history.vlite");
        let mut state = seed;
        let mut db = Database::create(&path, 8).unwrap();
        let config = HnswConfig {
            seed,
            ..HnswConfig::default()
        };
        let mut model: BTreeMap<_, _> = (0..80)
            .chain([u64::MAX])
            .map(|id| (id, record(&mut state, 0)))
            .collect();
        let queries: Vec<_> = (0..12).map(|_| record(&mut state, 0).vector).collect();
        db.write_batch(
            &model
                .iter()
                .map(|(&id, record)| put(id, record))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        not_ready(&db, &queries[0], config);
        db.build_hnsw(config).unwrap();
        check(&db, &model, &queries, config);
        for step in 1..=60 {
            let id = random(&mut state) % 80;
            match step % 4 {
                0 => {
                    let mut replacement = model.get(&u64::MAX).unwrap().clone();
                    replacement
                        .metadata
                        .insert("generation".into(), MetadataValue::Integer(step));
                    db.put_with_metadata(
                        u64::MAX,
                        &replacement.vector,
                        &replacement.payload,
                        &replacement.metadata,
                    )
                    .unwrap();
                    model.insert(u64::MAX, replacement);
                }
                1 => {
                    let replacement = record(&mut state, step);
                    db.write_batch(&[
                        Mutation::Delete { id },
                        put(id, &record(&mut state, -1)),
                        put(id, &replacement),
                    ])
                    .unwrap();
                    model.insert(id, replacement);
                }
                2 => {
                    assert_eq!(db.delete(id).unwrap(), model.remove(&id).is_some());
                    assert!(db.get(id).unwrap().is_none());
                }
                _ => {
                    let replacement = record(&mut state, step);
                    db.put(id, &replacement.vector, &replacement.payload)
                        .unwrap();
                    model.insert(
                        id,
                        Record {
                            metadata: Metadata::new(),
                            ..replacement
                        },
                    );
                }
            }
            if step % 10 == 0 {
                let before = check(&db, &model, &queries, config);
                let invalid = [
                    Mutation::Delete { id: u64::MAX },
                    Mutation::Put {
                        id: 999,
                        vector: vec![0.0],
                        payload: vec![],
                    },
                ];
                assert!(db.write_batch(&invalid).is_err());
                assert_eq!(check(&db, &model, &queries, config), before);
                assert!(db.get(999).unwrap().is_none());
            }
        }
        // Exactly 256 distinct IDs remain ready; one more explicitly invalidates.
        db.build_hnsw(config).unwrap();
        let changes: Vec<_> = (1000..1000 + MAX_HNSW_BUFFER_IDS as u64)
            .map(|id| {
                let replacement = record(&mut state, 61);
                model.insert(id, replacement.clone());
                put(id, &replacement)
            })
            .collect();
        db.write_batch(&changes).unwrap();
        check(&db, &model, &queries, config);
        let replacement = record(&mut state, 62);
        db.put_with_metadata(
            2000,
            &replacement.vector,
            &replacement.payload,
            &replacement.metadata,
        )
        .unwrap();
        model.insert(2000, replacement);
        not_ready(&db, &queries[0], config);
        assert_eq!(
            db.search(&queries[0], 7).unwrap(),
            oracle(&model, &queries[0], 7)
        );
        db.build_hnsw(config).unwrap();
        check(&db, &model, &queries, config);
        db.close().unwrap();
        let persisted = fs::read(&path).unwrap();
        db = Database::open(&path).unwrap();
        not_ready(&db, &queries[0], config);
        db.build_hnsw(config).unwrap();
        let first = check(&db, &model, &queries, config);
        db.build_hnsw(config).unwrap();
        assert_eq!(check(&db, &model, &queries, config), first);
        let mismatch = HnswConfig {
            seed: seed + 1,
            ..config
        };
        not_ready(&db, &queries[0], mismatch);
        assert_eq!(check(&db, &model, &queries, config), first);
        db.close().unwrap();
        assert_eq!(
            fs::read(&path).unwrap(),
            persisted,
            "index maintenance wrote database bytes"
        );
        db = Database::open(&path).unwrap();
        not_ready(&db, &queries[0], config);
        db.build_hnsw(config).unwrap();
        assert_eq!(check(&db, &model, &queries, config), first);
        db.write_batch(
            &model
                .keys()
                .map(|&id| Mutation::Delete { id })
                .collect::<Vec<_>>(),
        )
        .unwrap();
        model.clear();
        assert!(db.is_empty());
        assert!(
            db.search_approximate_ready(&queries[0], 7, 1000, config)
                .unwrap()
                .is_empty()
        );
        let fresh = record(&mut state, 63);
        db.put(0, &fresh.vector, &fresh.payload).unwrap();
        model.insert(
            0,
            Record {
                metadata: Metadata::new(),
                ..fresh
            },
        );
        not_ready(&db, &queries[0], config);
        db.build_hnsw(config).unwrap();
        check(&db, &model, &queries, config);
        db.close().unwrap();
    }
}
