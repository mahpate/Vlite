use vlite::{Database, Error, HnswConfig, Mutation};

fn vectors(count: usize, dimensions: usize) -> Vec<Vec<f32>> {
    let mut state = 42u64;
    (0..count)
        .map(|_| {
            (0..dimensions)
                .map(|_| {
                    state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                    (state >> 48) as f32 / 32768.0 - 1.0
                })
                .collect()
        })
        .collect()
}

#[test]
fn recall_distances_ordering_and_deterministic_rebuild_are_measured_against_exact() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("recall.vlite");
    let mut db = Database::create(&path, 16).unwrap();
    let vectors = vectors(1100, 16);
    let operations = vectors[..1000]
        .iter()
        .enumerate()
        .map(|(id, vector)| Mutation::Put {
            id: id as u64,
            vector: vector.clone(),
            payload: Vec::new(),
        })
        .collect::<Vec<_>>();
    db.write_batch(&operations).unwrap();
    let config = HnswConfig::default();
    db.build_hnsw(config).unwrap();
    let mut recalled = 0;
    let mut results = Vec::new();
    for query in &vectors[1000..] {
        let exact = db.search(query, 10).unwrap();
        let approximate = db.search_approximate(query, 10, 100, config).unwrap();
        assert_eq!(approximate.len(), 10);
        let mut seen = std::collections::BTreeSet::new();
        for hit in &approximate {
            assert!(seen.insert(hit.id));
            let expected = vectors[hit.id as usize]
                .iter()
                .zip(query)
                .map(|(&a, &b)| (f64::from(a) - f64::from(b)).powi(2))
                .sum::<f64>();
            assert_eq!(hit.distance, expected);
            recalled += usize::from(exact.iter().any(|reference| reference.id == hit.id));
        }
        assert!(approximate.windows(2).all(|pair| {
            pair[0]
                .distance
                .total_cmp(&pair[1].distance)
                .then(pair[0].id.cmp(&pair[1].id))
                .is_le()
        }));
        results.push(approximate);
    }
    assert!(
        recalled >= 950,
        "recall was {recalled}/1000, required >=0.95 for this fixed workload"
    );
    db.close().unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let mut db = Database::open(&path).unwrap();
    for (query, expected) in vectors[1000..].iter().zip(results) {
        assert_eq!(
            db.search_approximate(query, 10, 100, config).unwrap(),
            expected
        );
    }
    db.close().unwrap();
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn mutations_invalidate_index_without_stale_ids_vectors_or_partial_batches() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("mutations.vlite");
    let mut db = Database::create(&path, 2).unwrap();
    let config = HnswConfig::default();
    db.put(0, &[0.0, 0.0], b"").unwrap();
    db.put(u64::MAX, &[10.0, 10.0], b"").unwrap();
    assert_eq!(
        db.search_approximate(&[0.0, 0.0], 1, 10, config).unwrap()[0].id,
        0
    );
    db.put(0, &[20.0, 20.0], b"updated").unwrap();
    assert_eq!(
        db.search_approximate(&[0.0, 0.0], 1, 10, config).unwrap()[0].id,
        u64::MAX
    );
    assert!(db.delete(u64::MAX).unwrap());
    assert_eq!(
        db.search_approximate(&[0.0, 0.0], 10, 10, config).unwrap()[0].id,
        0
    );
    let invalid = [
        Mutation::Delete { id: 0 },
        Mutation::Put {
            id: 1,
            vector: vec![0.0],
            payload: vec![],
        },
    ];
    assert!(db.write_batch(&invalid).is_err());
    assert_eq!(
        db.search_approximate(&[0.0, 0.0], 1, 10, config).unwrap()[0].id,
        0
    );
    db.write_batch(&[
        Mutation::Put {
            id: 1,
            vector: vec![1.0, 1.0],
            payload: vec![],
        },
        Mutation::Put {
            id: 1,
            vector: vec![0.0, 0.0],
            payload: vec![],
        },
        Mutation::Delete { id: 0 },
    ])
    .unwrap();
    assert_eq!(
        db.search_approximate(&[0.0, 0.0], 10, 10, config).unwrap(),
        db.search(&[0.0, 0.0], 10).unwrap()
    );
    db.write_batch(&[Mutation::Delete { id: 1 }]).unwrap();
    assert!(
        db.search_approximate(&[0.0, 0.0], 1, 10, config)
            .unwrap()
            .is_empty()
    );
    db.close().unwrap();
    let mut db = Database::open(path).unwrap();
    assert!(
        db.search_approximate(&[0.0, 0.0], 1, 10, config)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn configuration_query_validation_extremes_and_ties() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("validation.vlite");
    let mut db = Database::create(&path, 1).unwrap();
    let default = HnswConfig::default();
    for config in [
        HnswConfig { m: 1, ..default },
        HnswConfig { m: 65, ..default },
        HnswConfig {
            ef_construction: 15,
            ..default
        },
        HnswConfig {
            ef_construction: 4097,
            ..default
        },
    ] {
        assert!(matches!(
            db.build_hnsw(config),
            Err(Error::InvalidHnswConfig)
        ));
    }
    assert!(matches!(
        db.search_approximate(&[f32::NAN], 0, 10, default),
        Err(Error::NonFiniteVector)
    ));
    assert!(matches!(
        db.search_approximate(&[], 0, 10, default),
        Err(Error::DimensionMismatch { .. })
    ));
    assert!(matches!(
        db.search_approximate(&[0.0], 0, 0, default),
        Err(Error::InvalidEfSearch)
    ));
    assert!(matches!(
        db.search_approximate(&[0.0], 1, 100001, default),
        Err(Error::InvalidEfSearch)
    ));
    for id in [u64::MAX, 0, 7, 99] {
        db.put(id, &[f32::MAX], b"").unwrap();
    }
    assert!(matches!(
        db.search_approximate(&[0.0], 4, 3, default),
        Err(Error::InvalidEfSearch)
    ));
    assert_eq!(
        db.search_approximate(&[-f32::MAX], 10, 10, default)
            .unwrap(),
        db.search(&[-f32::MAX], 10).unwrap()
    );
    assert!(
        db.search_approximate(&[0.0], 0, 10, default)
            .unwrap()
            .is_empty()
    );
    for config in [
        HnswConfig {
            m: 2,
            ef_construction: 2,
            seed: 0,
        },
        HnswConfig {
            m: 64,
            ef_construction: 4096,
            seed: u64::MAX,
        },
    ] {
        assert_eq!(
            db.search_approximate(&[0.0], 4, 10, config).unwrap(),
            db.search(&[0.0], 4).unwrap()
        );
    }
}
