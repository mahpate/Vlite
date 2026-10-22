use tempfile::tempdir;
use vlite::{Condition, Database, Metadata, MetadataValue};

#[test]
fn facets_and_numeric_stats_apply_typed_filters() {
    let directory = tempdir().unwrap();
    let mut db = Database::create(directory.path().join("facets.vlite"), 1).unwrap();
    for (id, category, score, active) in [
        (1, "a", 2, true),
        (2, "a", 4, true),
        (3, "b", 8, false),
        (4, "a", 6, false),
    ] {
        db.put_with_metadata(
            id,
            &[id as f32],
            b"",
            &Metadata::from([
                ("category".into(), MetadataValue::String(category.into())),
                ("score".into(), MetadataValue::Integer(score)),
                ("active".into(), MetadataValue::Boolean(active)),
            ]),
        )
        .unwrap();
    }
    let filters = [Condition::Eq {
        field: "active".into(),
        value: MetadataValue::Boolean(true),
    }];
    assert_eq!(
        db.facets("category", &filters).unwrap(),
        [vlite::FacetCount {
            value: MetadataValue::String("a".into()),
            count: 2
        },]
    );
    assert_eq!(
        db.numeric_stats("score", &filters).unwrap(),
        vlite::NumericStats {
            count: 2,
            min: Some(2.0),
            max: Some(4.0),
            sum: 6.0,
            average: Some(3.0),
        }
    );
    assert_eq!(db.facets("missing", &[]).unwrap(), []);
    assert_eq!(db.numeric_stats("category", &[]).unwrap().count, 0);
    assert!(db.facets("", &[]).is_err());
}
