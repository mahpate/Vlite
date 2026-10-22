mod vector_support;
use vector_support::*;
use vlite::{
    Database,
    hybrid::FusionOptions,
    vectors::{DenseSparseQuery, RecordVectors},
};
#[test]
fn sparse_dot_negative_zero_overlap_fusion_and_reopen() {
    let f = Fixture::new();
    let mut db = f.db();
    seed(&mut db);
    {
        let mut c = db.collection_mut("default").unwrap();
        for (id, vs) in [
            (1, vec![(0, 2.0), (u32::MAX, 1.0)]),
            (2, vec![(0, -3.0)]),
            (3, vec![(7, 2.0)]),
            (4, vec![]),
        ] {
            let mut r = c.view().record_vectors(id).unwrap().cloned().unwrap();
            r.sparse.insert("terms".into(), vs);
            c.put_vectors(id, r).unwrap();
        }
        let hits = c.view().search_sparse("terms", &[(0, 1.0)], 10).unwrap();
        assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), [1, 3, 4, 2]);
        assert_eq!(hits[0].distance, -2.0);
        let fused = c
            .view()
            .search_dense_sparse(&DenseSparseQuery {
                dense_name: "text".into(),
                version: None,
                dense: vec![2.0, 1.0],
                sparse_name: "terms".into(),
                sparse: vec![(0, 1.0)],
                k: 4,
                fusion: FusionOptions::default(),
            })
            .unwrap();
        assert_eq!(fused.len(), 4);
        assert!(fused.iter().all(|h| h.score.is_finite()));
        assert!(
            c.view()
                .search_sparse("terms", &[(1, f32::INFINITY)], 1)
                .is_err()
        );
        assert!(
            c.view()
                .search_sparse("terms", &[(1, 1.0), (1, 2.0)], 1)
                .is_err()
        );
        let bad = RecordVectors {
            sparse: std::collections::BTreeMap::from([("terms".into(), vec![(2, 1.0), (1, 2.0)])]),
            ..Default::default()
        };
        assert!(c.put_vectors(1, bad).is_err());
    }
    db.close().unwrap();
    let db = Database::open(&f.path).unwrap();
    assert_eq!(
        db.collection("default")
            .unwrap()
            .search_sparse("terms", &[(u32::MAX, 2.0)], 1)
            .unwrap()[0]
            .distance,
        -2.0
    );
}
