use vlite::{Condition, Database, DistanceMetric as M, Error, Metadata, MetadataValue};

#[test]
fn metrics_have_explicit_ordering_zero_rules_filters_and_no_storage_change() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("metrics.vlite");
    let mut db = Database::create(&path, 2).unwrap();
    assert!(matches!(
        db.search_with_metric(&[0.0, 0.0], 0, M::Cosine),
        Err(Error::ZeroNormQuery)
    ));
    assert!(
        db.search_with_metric(&[1.0, 0.0], 9, M::Cosine)
            .unwrap()
            .is_empty()
    );
    for (id, vector) in [
        (0, [0.0, -0.0]),
        (1, [1.0, 0.0]),
        (2, [4.0, 3.0]),
        (3, [-1.0, 0.0]),
        (4, [1.0, 0.0]),
    ] {
        db.put_with_metadata(
            id,
            &vector,
            b"",
            &Metadata::from([("id".into(), MetadataValue::Integer(id as i64))]),
        )
        .unwrap();
    }
    db.close().unwrap();
    let before = std::fs::read(&path).unwrap();
    let db = Database::open(&path).unwrap();
    let l2 = db.search(&[1.0, 0.0], 9).unwrap();
    assert_eq!(
        l2,
        db.search_with_metric(&[1.0, 0.0], 9, M::SquaredL2).unwrap()
    );
    let cosine = db.search_with_metric(&[1.0, 0.0], 9, M::Cosine).unwrap();
    assert_eq!(
        cosine.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        [1, 4, 2, 3]
    );
    assert_eq!(cosine[0].distance, 0.0);
    assert!((cosine[2].distance - 0.2).abs() < 1e-15);
    assert_eq!(cosine[3].distance, 2.0);
    let dot = db
        .search_with_metric(&[1.0, 0.0], 9, M::InnerProduct)
        .unwrap();
    assert_eq!(
        dot.iter()
            .map(|hit| (hit.id, hit.distance))
            .collect::<Vec<_>>(),
        [(2, -4.0), (1, -1.0), (4, -1.0), (0, 0.0), (3, 1.0)]
    );
    for k in [0, 1, 2, usize::MAX] {
        assert!(matches!(
            db.search_with_metric(&[0.0, 0.0], k, M::Cosine),
            Err(Error::ZeroNormQuery)
        ));
        assert_eq!(
            db.search_with_metric(&[1.0, 0.0], k, M::Cosine).unwrap(),
            cosine[..k.min(cosine.len())]
        );
        assert!(matches!(
            db.search_with_metric(&[f32::NAN, 0.0], k, M::InnerProduct),
            Err(Error::NonFiniteVector)
        ));
    }
    let filter = [Condition::Eq {
        field: "id".into(),
        value: MetadataValue::Integer(2),
    }];
    let hits = db
        .search_filtered_with_metric(&[1.0, 0.0], 10, &filter, M::Cosine)
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, 2);
    db.close().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn metrics_remain_finite_for_extreme_and_subnormal_components() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = Database::create(directory.path().join("extreme.vlite"), 2).unwrap();
    for (id, vector) in [
        (1, [f32::MAX, f32::MAX]),
        (2, [-f32::MAX, -f32::MAX]),
        (3, [f32::from_bits(1), 0.0]),
    ] {
        db.put(id, &vector, b"").unwrap();
    }

    for query in [[f32::MAX, f32::MAX], [f32::from_bits(1), 0.0]] {
        for metric in [M::Cosine, M::InnerProduct] {
            let hits = db.search_with_metric(&query, 10, metric).unwrap();
            assert_eq!(hits.len(), 3);
            assert!(hits.iter().all(|hit| hit.distance.is_finite()));
            if metric == M::Cosine {
                assert!(hits.iter().all(|hit| (0.0..=2.0).contains(&hit.distance)));
            }
        }
    }
}

#[test]
fn metric_top_k_matches_full_sort_over_seeded_records_and_filters() {
    for dimensions in [1, 33, 65] {
        let directory = tempfile::tempdir().unwrap();
        let mut db = Database::create(directory.path().join("oracle.vlite"), dimensions).unwrap();
        let mut state = 42u64;
        let mut sample = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((state >> 32) % 101) as f32 - 50.0
        };
        let records: Vec<_> = (0..97)
            .map(|id| {
                let vector: Vec<_> = (0..dimensions).map(|_| sample()).collect();
                db.put_with_metadata(
                    id,
                    &vector,
                    b"",
                    &Metadata::from([("selected".into(), MetadataValue::Boolean(id % 3 == 0))]),
                )
                .unwrap();
                (id, vector)
            })
            .collect();
        for _ in 0..8 {
            let query: Vec<_> = (0..dimensions).map(|_| sample()).collect();
            if query.iter().all(|&value| value == 0.0) {
                continue;
            }
            for metric in [M::Cosine, M::InnerProduct] {
                for filtered in [false, true] {
                    let mut expected: Vec<_> = records
                        .iter()
                        .filter(|(id, _)| !filtered || id % 3 == 0)
                        .filter_map(|(id, vector)| {
                            let a: Vec<f64> = vector.iter().map(|&v| f64::from(v)).collect();
                            let b: Vec<f64> = query.iter().map(|&v| f64::from(v)).collect();
                            let dot: f64 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
                            let distance = if metric == M::Cosine {
                                let an: f64 = a.iter().map(|x| x * x).sum();
                                let bn: f64 = b.iter().map(|x| x * x).sum();
                                if an == 0.0 {
                                    return None;
                                }
                                1.0 - (dot / (an.sqrt() * bn.sqrt())).clamp(-1.0, 1.0)
                            } else if dot == 0.0 {
                                0.0
                            } else {
                                -dot
                            };
                            Some((*id, distance))
                        })
                        .collect();
                    expected.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
                    let filter = if filtered {
                        vec![Condition::Eq {
                            field: "selected".into(),
                            value: MetadataValue::Boolean(true),
                        }]
                    } else {
                        vec![]
                    };
                    for k in [0, 1, 9, 100] {
                        let actual: Vec<_> = db
                            .search_filtered_with_metric(&query, k, &filter, metric)
                            .unwrap()
                            .iter()
                            .map(|hit| (hit.id, hit.distance))
                            .collect();
                        assert_eq!(actual, expected[..k.min(expected.len())]);
                    }
                }
            }
        }
    }
}
