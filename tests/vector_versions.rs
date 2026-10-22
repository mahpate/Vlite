mod vector_support;
use vector_support::*;
use vlite::Database;
#[test]
fn bounded_history_explicit_search_and_prune_are_durable() {
    let f = Fixture::new();
    let mut db = f.db();
    seed(&mut db);
    {
        let mut c = db.collection_mut("default").unwrap();
        for (v, x) in [("v2", 8.0), ("v3", 9.0), ("v4", 10.0)] {
            c.put_vector_version(1, "text", v, vec![x, 1.0]).unwrap();
        }
        let r = c.view().record_vectors(1).unwrap().unwrap();
        let vs = &r.dense["text"];
        assert_eq!(vs.len(), 3);
        assert!(!vs.iter().any(|v| v.version == "v2"));
        assert_eq!(
            c.view()
                .search_named("text", Some("v4"), &[10.0, 1.0], 1)
                .unwrap()[0]
                .id,
            1
        );
        assert_eq!(
            c.view().search_named("text", None, &[1.0, 1.0], 1).unwrap()[0].id,
            1
        );
        c.prune_vector_history("text", 0).unwrap();
        assert_eq!(
            c.view().record_vectors(1).unwrap().unwrap().dense["text"].len(),
            1
        );
        assert!(
            c.put_vector_version(1, "text", "bad", vec![f32::NAN, 0.0])
                .is_err()
        );
        assert!(c.prune_vector_history("text", 65).is_err());
    }
    db.close().unwrap();
    let db = Database::open(&f.path).unwrap();
    let c = db.collection("default").unwrap();
    assert_eq!(c.vector_specs().unwrap()["text"].history, 0);
    assert!(
        c.search_named("text", Some("v4"), &[10.0, 1.0], 4)
            .unwrap()
            .is_empty()
    );
}
