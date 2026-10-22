use tempfile::tempdir;
use vlite::{
    Condition, Database, DistanceMetric, Metadata, MetadataValue, RecommendationQuery, SearchHit,
};

fn metadata(group: &str, score: i64) -> Metadata {
    Metadata::from([
        ("doc_id".into(), MetadataValue::String(group.into())),
        ("score".into(), MetadataValue::Integer(score)),
    ])
}

#[test]
fn groups_are_ranked_by_best_hit_and_cap_each_group() {
    let directory = tempdir().unwrap();
    let mut db = Database::create(directory.path().join("groups.vlite"), 2).unwrap();
    for (id, vector, group) in [
        (1, [0.0, 0.0], "a"),
        (2, [0.1, 0.0], "a"),
        (3, [0.2, 0.0], "a"),
        (4, [1.0, 0.0], "b"),
        (5, [2.0, 0.0], "c"),
    ] {
        db.put_with_metadata(id, &vector, b"", &metadata(group, id as i64))
            .unwrap();
    }
    let groups = db
        .search_grouped(&[0.0, 0.0], 2, 2, "doc_id", &[], DistanceMetric::SquaredL2)
        .unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].value, MetadataValue::String("a".into()));
    assert_eq!(
        groups[0].hits.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(groups[1].value, MetadataValue::String("b".into()));
    assert_eq!(
        groups[1].hits,
        [SearchHit {
            id: 4,
            distance: 1.0
        }]
    );
}

#[test]
fn recommendations_average_examples_subtract_negatives_and_exclude_ids() {
    let directory = tempdir().unwrap();
    let mut db = Database::create(directory.path().join("recommend.vlite"), 2).unwrap();
    db.put(1, &[1.0, 0.0], b"").unwrap();
    db.put(2, &[0.0, 1.0], b"").unwrap();
    db.put(3, &[1.0, -1.0], b"").unwrap();
    db.put(4, &[-1.0, 1.0], b"").unwrap();
    let result = db
        .recommend(&RecommendationQuery {
            positive_ids: vec![1],
            negative_ids: vec![2],
            positive_vectors: vec![],
            negative_vectors: vec![],
            k: 10,
            conditions: vec![],
            metric: DistanceMetric::SquaredL2,
        })
        .unwrap();
    assert_eq!(result.iter().map(|hit| hit.id).collect::<Vec<_>>(), [3, 4]);
    let vectors = db
        .recommend(&RecommendationQuery {
            positive_ids: vec![],
            negative_ids: vec![],
            positive_vectors: vec![vec![1.0, 0.0]],
            negative_vectors: vec![vec![0.0, 1.0]],
            k: 1,
            conditions: vec![],
            metric: DistanceMetric::SquaredL2,
        })
        .unwrap();
    assert_eq!(vectors[0].id, 3);
    assert!(
        db.recommend(&RecommendationQuery {
            positive_ids: vec![],
            negative_ids: vec![],
            positive_vectors: vec![],
            negative_vectors: vec![],
            k: 5,
            conditions: vec![],
            metric: DistanceMetric::SquaredL2,
        })
        .is_err()
    );
}

#[test]
fn grouping_filter_is_applied_before_group_limits() {
    let directory = tempdir().unwrap();
    let mut db = Database::create(directory.path().join("filtered-groups.vlite"), 1).unwrap();
    db.put_with_metadata(1, &[0.0], b"", &metadata("a", 1))
        .unwrap();
    db.put_with_metadata(2, &[0.1], b"", &metadata("a", 2))
        .unwrap();
    db.put_with_metadata(3, &[0.2], b"", &metadata("b", 3))
        .unwrap();
    let conditions = [Condition::Range {
        field: "score".into(),
        min: Some(MetadataValue::Integer(2)),
        max: None,
    }];
    let groups = db
        .search_grouped(
            &[0.0],
            10,
            1,
            "doc_id",
            &conditions,
            DistanceMetric::SquaredL2,
        )
        .unwrap();
    assert_eq!(
        groups
            .iter()
            .map(|group| group.value.clone())
            .collect::<Vec<_>>(),
        [
            MetadataValue::String("a".into()),
            MetadataValue::String("b".into())
        ]
    );
}
