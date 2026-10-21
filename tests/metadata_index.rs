//! TEST-006: metadata indexes return results identical to unindexed exact
//! filtered search across mixed types, ±0.0, i64 extremes, updates, deletes
//! and reopen.

use vlite::{
    CollectionSettings, Condition, Database, DistanceMetric, FormatVersion, Metadata,
    MetadataIndexKind, MetadataIndexSpec, MetadataValue, Mutation,
};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const SPECIAL_INTS: [i64; 5] = [i64::MIN, -1, 0, 1, i64::MAX];
const SPECIAL_FLOATS: [f64; 5] = [-0.0, 0.0, 1.0, -2.5, 1e300];

fn value(rng: &mut Rng) -> Option<MetadataValue> {
    Some(match rng.below(9) {
        0 => return None,
        1 => MetadataValue::Integer(SPECIAL_INTS[rng.below(5) as usize]),
        2 | 3 => MetadataValue::Integer(rng.below(40) as i64 - 20),
        4 => MetadataValue::Float(SPECIAL_FLOATS[rng.below(5) as usize]),
        5 | 6 => MetadataValue::Float((rng.below(40) as f64 - 20.0) / 2.0),
        7 => MetadataValue::String(format!("s{}", rng.below(6))),
        _ => MetadataValue::Boolean(rng.below(2) == 0),
    })
}

fn metadata(rng: &mut Rng) -> Metadata {
    let mut metadata = Metadata::new();
    for field in ["eq", "range", "both", "plain"] {
        if let Some(value) = value(rng) {
            metadata.insert(field.into(), value);
        }
    }
    metadata
}

fn condition(rng: &mut Rng) -> Condition {
    let field = ["eq", "range", "both", "plain"][rng.below(4) as usize].to_owned();
    if rng.below(2) == 0 {
        let value = value(rng).unwrap_or(MetadataValue::Integer(0));
        return Condition::Eq { field, value };
    }
    let (min, max) = if rng.below(2) == 0 {
        let a = rng.below(40) as i64 - 20;
        let b = a + rng.below(20) as i64;
        let pick = |v: i64, rng: &mut Rng| (rng.below(4) != 0).then_some(MetadataValue::Integer(v));
        let (mut min, mut max) = (pick(a, rng), pick(b, rng));
        if rng.below(10) == 0 {
            min = Some(MetadataValue::Integer(i64::MIN));
            max = Some(MetadataValue::Integer(i64::MAX));
        }
        (min, max)
    } else {
        let a = (rng.below(40) as f64 - 20.0) / 2.0;
        let b = a + rng.below(20) as f64 / 2.0;
        let a = if a == 0.0 && rng.below(2) == 0 {
            -0.0
        } else {
            a
        };
        let pick = |v: f64, rng: &mut Rng| (rng.below(4) != 0).then_some(MetadataValue::Float(v));
        (pick(a, rng), pick(b, rng))
    };
    let min = if min.is_none() && max.is_none() {
        Some(MetadataValue::Integer(0))
    } else {
        min
    };
    let max = match (&min, max) {
        (Some(MetadataValue::Integer(_)), Some(MetadataValue::Float(_))) => None,
        (_, max) => max,
    };
    Condition::Range { field, min, max }
}

fn indexed_settings() -> CollectionSettings {
    let mut settings = CollectionSettings::default();
    settings.metadata_indexes = vec![
        MetadataIndexSpec {
            field: "eq".into(),
            kind: MetadataIndexKind::Eq,
        },
        MetadataIndexSpec {
            field: "range".into(),
            kind: MetadataIndexKind::Range,
        },
        MetadataIndexSpec {
            field: "both".into(),
            kind: MetadataIndexKind::EqRange,
        },
    ];
    settings
}

fn compare(indexed: &Database, plain: &Database, seed: u64, queries: usize) -> u64 {
    let mut rng = Rng(seed);
    let mut used_index = 0;
    for _ in 0..queries {
        let conditions: Vec<Condition> =
            (0..1 + rng.below(3)).map(|_| condition(&mut rng)).collect();
        let query = [
            rng.below(100) as f32 / 10.0,
            rng.below(100) as f32 / 10.0 + 0.1,
        ];
        let k = 1 + rng.below(30) as usize;
        for metric in [DistanceMetric::SquaredL2, DistanceMetric::Cosine] {
            let expected = plain
                .search_filtered_with_metric(&query, k, &conditions, metric)
                .unwrap();
            let (actual, stats) = indexed
                .collection("default")
                .unwrap()
                .search_with_stats(&query, k, &conditions, metric)
                .unwrap();
            assert_eq!(actual, expected, "conditions {conditions:?}");
            if stats.index_candidates > 0 {
                used_index += 1;
                assert!(stats.records_scanned <= stats.index_candidates);
            }
        }
    }
    used_index
}

#[test]
fn indexed_filters_equal_unindexed_scans_through_mutations_and_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let indexed_path = directory.path().join("indexed.vlite");
    let plain_path = directory.path().join("plain.vlite");
    let mut indexed = Database::create_with_format(&indexed_path, 2, FormatVersion::V4).unwrap();
    let mut plain = Database::create_with_format(&plain_path, 2, FormatVersion::V4).unwrap();
    indexed
        .set_collection_settings("default", indexed_settings())
        .unwrap();
    let mut rng = Rng(7);
    let operations: Vec<Mutation> = (0..2000u64)
        .map(|id| Mutation::PutWithMetadata {
            id,
            vector: vec![
                rng.below(100) as f32 / 10.0,
                rng.below(100) as f32 / 10.0 + 0.1,
            ],
            payload: vec![],
            metadata: metadata(&mut rng),
        })
        .collect();
    for chunk in operations.chunks(1000) {
        indexed.write_batch(chunk).unwrap();
        plain.write_batch(chunk).unwrap();
    }
    assert!(compare(&indexed, &plain, 1, 250) > 50);

    // Updates (including metadata removal and type changes) and deletes.
    for id in (0..2000u64).step_by(7) {
        let vector = [rng.below(100) as f32 / 10.0, 1.0];
        if id % 3 == 0 {
            indexed.delete(id).unwrap();
            plain.delete(id).unwrap();
        } else {
            let metadata = metadata(&mut rng);
            indexed
                .put_with_metadata(id, &vector, b"", &metadata)
                .unwrap();
            plain
                .put_with_metadata(id, &vector, b"", &metadata)
                .unwrap();
        }
    }
    assert!(compare(&indexed, &plain, 2, 150) > 30);

    drop(indexed);
    let indexed = Database::open(&indexed_path).unwrap();
    assert_eq!(
        indexed
            .collection("default")
            .unwrap()
            .settings()
            .metadata_indexes,
        indexed_settings().metadata_indexes
    );
    assert!(compare(&indexed, &plain, 3, 100) > 20);
}

#[test]
fn numeric_types_and_negative_zero_are_exact_through_the_index() {
    let directory = tempfile::tempdir().unwrap();
    let mut db =
        Database::create_with_format(directory.path().join("db.vlite"), 1, FormatVersion::V4)
            .unwrap();
    db.set_collection_settings("default", indexed_settings())
        .unwrap();
    let values = [
        MetadataValue::Integer(1),
        MetadataValue::Float(1.0),
        MetadataValue::Float(-0.0),
        MetadataValue::Float(0.0),
        MetadataValue::Integer(0),
        MetadataValue::Integer(i64::MIN),
        MetadataValue::Integer(i64::MAX),
        MetadataValue::String("1".into()),
    ];
    for (id, value) in values.iter().enumerate() {
        let metadata = Metadata::from([("both".into(), value.clone())]);
        db.put_with_metadata(id as u64, &[id as f32], b"", &metadata)
            .unwrap();
    }
    let ids = |condition: Condition| {
        let (hits, stats) = db
            .collection("default")
            .unwrap()
            .search_with_stats(&[0.0], 100, &[condition], DistanceMetric::SquaredL2)
            .unwrap();
        let mut ids: Vec<u64> = hits.into_iter().map(|hit| hit.id).collect();
        ids.sort_unstable();
        (ids, stats.index_candidates)
    };
    let eq = |value| Condition::Eq {
        field: "both".into(),
        value,
    };
    assert_eq!(ids(eq(MetadataValue::Integer(1))), (vec![0], 1));
    assert_eq!(ids(eq(MetadataValue::Float(1.0))), (vec![1], 1));
    assert_eq!(ids(eq(MetadataValue::Float(0.0))), (vec![2, 3], 2));
    assert_eq!(ids(eq(MetadataValue::Float(-0.0))), (vec![2, 3], 2));
    let range = |min, max| Condition::Range {
        field: "both".into(),
        min,
        max,
    };
    assert_eq!(
        ids(range(
            Some(MetadataValue::Integer(i64::MIN)),
            Some(MetadataValue::Integer(0))
        )),
        (vec![4, 5], 2)
    );
    assert_eq!(
        ids(range(Some(MetadataValue::Float(-0.0)), None)),
        (vec![1, 2, 3], 3)
    );
    assert_eq!(
        ids(range(None, Some(MetadataValue::Integer(i64::MAX)))).0,
        vec![0, 4, 5, 6]
    );
}
