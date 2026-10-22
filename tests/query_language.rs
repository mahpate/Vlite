mod vector_support;
use std::collections::BTreeMap;
use vector_support::*;
use vlite::{CollectionSettings, Database, Metadata, MetadataValue as V, query_language::parse};
#[test]
fn named_vector_filters_groups_facets_order_and_reopen() {
    let f = Fixture::new();
    let mut db = f.db();
    seed(&mut db);
    for id in 1..=4 {
        let m = Metadata::from([
            (
                "lang".into(),
                V::String(if id == 4 { "fr" } else { "en" }.into()),
            ),
            ("price".into(), V::Integer(id as i64 * 5)),
            ("doc_id".into(), V::Integer((id % 2) as i64)),
            (
                "place".into(),
                V::GeoPoint {
                    latitude: 0.0,
                    longitude: 0.0,
                },
            ),
        ]);
        db.update_metadata(id, &m).unwrap();
    }
    let params = BTreeMap::from([("q".into(), vec![1.0, 1.0])]);
    let query=parse(r#"SEARCH text NEAR $q K 10 WHERE lang = "en" AND price < 20 AND doc_id IN (0,1) AND EXISTS place AND GEO(place,0,0,100) GROUP BY doc_id FACET lang ORDER BY price DESC LIMIT 2"#).unwrap();
    let out = query
        .execute(db.collection("default").unwrap(), &params)
        .unwrap();
    assert_eq!(out.hits.iter().map(|h| h.id).collect::<Vec<_>>(), [3, 2]);
    assert_eq!(out.groups.len(), 2);
    assert_eq!(out.facets[0].count, 3);
    assert!(
        query
            .execute(db.collection("default").unwrap(), &BTreeMap::new())
            .is_err()
    );
    db.close().unwrap();
    let db = Database::open(&f.path).unwrap();
    assert_eq!(
        query
            .execute(db.collection("default").unwrap(), &params)
            .unwrap()
            .hits,
        out.hits
    );
}
#[test]
fn lexical_and_dense_fusion_respect_filters() {
    let f = Fixture::new();
    let mut db = f.db();
    seed(&mut db);
    let mut settings = CollectionSettings::default();
    settings.text_fields = vec!["body".into()];
    db.set_collection_settings("default", settings).unwrap();
    for id in 1..=4 {
        db.update_metadata(
            id,
            &Metadata::from([
                (
                    "body".into(),
                    V::String(if id < 3 { "rust database" } else { "python" }.into()),
                ),
                ("keep".into(), V::Boolean(id == 2)),
            ]),
        )
        .unwrap();
    }
    let params = BTreeMap::from([("q".into(), vec![1.0, 1.0])]);
    let c = db.collection("default").unwrap();
    let text = parse(r#"SEARCH TEXT MATCH "rust" WHERE keep = true"#)
        .unwrap()
        .execute(c, &params)
        .unwrap();
    assert_eq!(text.hits.len(), 1);
    assert_eq!(text.hits[0].id, 2);
    let mixed = parse(r#"SEARCH text NEAR $q TEXT "rust" WHERE keep = true"#)
        .unwrap()
        .execute(c, &params)
        .unwrap();
    assert_eq!(mixed.hits.len(), 1);
    assert_eq!(mixed.hits[0].id, 2);
}
#[test]
fn errors_are_positioned_and_no_other_statements_execute() {
    for input in [
        "",
        "DELETE all",
        "SEARCH text NEAR q",
        "SEARCH text NEAR $q; DELETE all",
        "SEARCH text NEAR $q WHERE a = \"unterminated",
        "SEARCH text NEAR $q WHERE a IN ()",
        "SEARCH text NEAR $q WHERE a > NaN",
        "SEARCH text NEAR $q LIMIT 1001",
        "SEARCH text NEAR $q WHERE GEO(p,91,0,1)",
        "SEARCH text NEAR $q OR 1 = 1",
    ] {
        let e = parse(input).unwrap_err();
        assert!(e.position <= input.len());
        assert!(!e.message.is_empty());
    }
    let e = parse("SEARCH text NEAR q").unwrap_err();
    assert_eq!(e.position, 17);
    assert!(parse("SEARCH text NEAR $q LIMIT 0").is_ok());
    assert!(parse(r#"SEARCH TEXT MATCH "日本語 \"quoted\"""#).is_ok());
}
