mod vector_support;
use vector_support::*;
use vlite::{
    CollectionSettings, CollectionSpec, Database, DistanceMetric, Error, HnswConfig,
    vectors::{RecordVectors, VersionedVector},
};
#[test]
fn independent_spaces_ann_reopen_compaction_and_legacy_guard() {
    let f = Fixture::new();
    let mut db = f.db();
    seed(&mut db);
    {
        let mut c = db.collection_mut("default").unwrap();
        c.configure_vector("image", spec(3, DistanceMetric::Cosine))
            .unwrap();
        c.put_vector_version(2, "image", "v1", vec![1.0, 0.0, 0.0])
            .unwrap();
        c.rebuild_named_hnsw("text", None, HnswConfig::default())
            .unwrap();
        assert_eq!(
            c.view()
                .search_named_ann("text", None, &[2.0, 1.0], 2, 10)
                .unwrap(),
            c.view().search_named("text", None, &[2.0, 1.0], 2).unwrap()
        );
        assert_eq!(
            c.view()
                .search_named("image", None, &[1.0, 0.0, 0.0], 4)
                .unwrap()[0]
                .id,
            2
        );
        assert!(
            c.view()
                .search_named("image", None, &[1.0, 0.0], 1)
                .is_err()
        );
        assert!(
            c.put_vector_version(2, "image", "v2", vec![f32::NAN, 0.0, 0.0])
                .is_err()
        );
    }
    db.create_collection(
        "other",
        CollectionSpec {
            dimensions: 1,
            settings: CollectionSettings::default(),
        },
    )
    .unwrap();
    {
        let mut c = db.collection_mut("other").unwrap();
        c.put(2, &[1.0], b"").unwrap();
        c.configure_vector("text", spec(1, DistanceMetric::InnerProduct))
            .unwrap();
        c.put_vector_version(2, "text", "v1", vec![8.0]).unwrap();
        assert_eq!(
            c.view().search_named("text", None, &[2.0], 1).unwrap()[0].distance,
            -16.0
        );
        assert!(
            c.rebuild_named_hnsw("text", None, HnswConfig::default())
                .is_err()
        );
    }
    db.close().unwrap();
    let db = Database::open(&f.path).unwrap();
    let c = db.collection("default").unwrap();
    assert_eq!(
        c.search_named("text", None, &[2.0, 1.0], 1).unwrap()[0].id,
        2
    );
    assert!(matches!(
        c.search_named_ann("text", None, &[2.0, 1.0], 1, 10),
        Err(Error::IndexNotReady)
    ));
    db.close().unwrap();
    let output = Fixture::new();
    vlite::maintenance::compact(&f.path, &output.path).unwrap();
    let db = Database::open(&output.path).unwrap();
    assert_eq!(
        db.collection("default")
            .unwrap()
            .search_named("image", None, &[1.0, 0.0, 0.0], 1)
            .unwrap()[0]
            .id,
        2
    );
    db.close().unwrap();
    let legacy = Fixture::new();
    let mut db = Database::create_with_format(&legacy.path, 2, vlite::FormatVersion::V3).unwrap();
    let before = std::fs::metadata(&legacy.path).unwrap().len();
    assert!(
        db.collection_mut("default")
            .unwrap()
            .configure_vector("x", spec(2, DistanceMetric::SquaredL2))
            .is_err()
    );
    assert_eq!(std::fs::metadata(&legacy.path).unwrap().len(), before);
}
#[test]
fn validation_and_base_replacement_do_not_resurrect_vectors() {
    let f = Fixture::new();
    let mut db = f.db();
    seed(&mut db);
    let before = std::fs::metadata(&f.path).unwrap().len();
    let bad = RecordVectors {
        dense: std::collections::BTreeMap::from([(
            "missing".into(),
            vec![VersionedVector {
                version: "v1".into(),
                vector: vec![1.0],
            }],
        )]),
        ..Default::default()
    };
    assert!(
        db.collection_mut("default")
            .unwrap()
            .put_vectors(1, bad)
            .is_err()
    );
    assert_eq!(std::fs::metadata(&f.path).unwrap().len(), before);
    db.update_metadata(1, &Default::default()).unwrap();
    assert!(
        db.collection("default")
            .unwrap()
            .record_vectors(1)
            .unwrap()
            .is_some()
    );
    db.put(1, &[9.0, 9.0], b"changed").unwrap();
    assert!(
        db.collection("default")
            .unwrap()
            .record_vectors(1)
            .unwrap()
            .is_none()
    );
    db.delete(2).unwrap();
    db.close().unwrap();
    let db = Database::open(&f.path).unwrap();
    let c = db.collection("default").unwrap();
    assert!(c.record_vectors(1).unwrap().is_none());
    assert!(c.record_vectors(2).unwrap().is_none());
}
#[test]
fn every_interrupted_vector_frame_is_recoverable_and_corruption_is_not() {
    use std::io::Write;
    let f = Fixture::new();
    let mut db = f.db();
    db.put(1, &[1.0, 0.0], b"").unwrap();
    db.close().unwrap();
    let mut db = Database::open(&f.path).unwrap();
    db.collection_mut("default")
        .unwrap()
        .configure_vector("text", spec(2, DistanceMetric::SquaredL2))
        .unwrap();
    db.close().unwrap();
    let previous = std::fs::read(&f.path).unwrap();
    let mut db = Database::open(&f.path).unwrap();
    db.collection_mut("default")
        .unwrap()
        .put_vector_version(1, "text", "v1", vec![1.0, 0.0])
        .unwrap();
    db.close().unwrap();
    let bytes = std::fs::read(&f.path).unwrap();
    let frame = &bytes[previous.len()..];
    let scratch = Fixture::new();
    for cut in 1..frame.len() {
        std::fs::write(&scratch.path, &previous).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&scratch.path)
            .unwrap()
            .write_all(&frame[..cut])
            .unwrap();
        assert!(Database::open(&scratch.path).is_err());
        Database::discard_incomplete_tail(&scratch.path).unwrap();
        let db = Database::open(&scratch.path).unwrap();
        let collection = db.collection("default").unwrap();
        assert!(collection.vector_specs().unwrap().contains_key("text"));
        assert!(collection.record_vectors(1).unwrap().is_none());
        db.close().unwrap();
    }
    let mut corrupt = bytes;
    let last = corrupt.len() - 17;
    corrupt[last] ^= 1;
    std::fs::write(&scratch.path, &corrupt).unwrap();
    assert!(Database::discard_incomplete_tail(&scratch.path).is_err());
}
#[test]
fn expiration_never_leaks_into_named_ann_or_sparse_after_compaction() {
    let f = Fixture::new();
    let out = Fixture::new();
    let mut db = f.db();
    db.put_with_expiry(1, &[1.0, 1.0], b"", &Default::default(), Some(1))
        .unwrap();
    db.put(2, &[2.0, 1.0], b"").unwrap();
    {
        let mut c = db.collection_mut("default").unwrap();
        c.configure_vector("text", spec(2, DistanceMetric::SquaredL2))
            .unwrap();
        c.put_vector_version(2, "text", "v1", vec![2.0, 1.0])
            .unwrap();
        c.rebuild_named_hnsw("text", None, HnswConfig::default())
            .unwrap();
        assert_eq!(
            c.view()
                .search_named_ann("text", None, &[2.0, 1.0], 10, 10)
                .unwrap()
                .len(),
            1
        );
        assert!(
            c.put_vector_version(1, "text", "v1", vec![1.0, 1.0])
                .is_err()
        );
    }
    db.close().unwrap();
    vlite::maintenance::compact_at(&f.path, &out.path, 2).unwrap();
    let db = Database::open(&out.path).unwrap();
    assert_eq!(
        db.collection("default")
            .unwrap()
            .search_named("text", None, &[1.0, 1.0], 10)
            .unwrap()
            .len(),
        1
    );
}
#[test]
fn ready_graph_can_outlive_record_expiry_without_panicking() {
    let f = Fixture::new();
    let mut db = f.db();
    db.put_with_expiry(1, &[1.0, 1.0], b"", &Default::default(), Some(u64::MAX))
        .unwrap();
    let mut c = db.collection_mut("default").unwrap();
    c.configure_vector("text", spec(2, DistanceMetric::SquaredL2))
        .unwrap();
    c.put_vector_version(1, "text", "v1", vec![1.0, 1.0])
        .unwrap();
    c.rebuild_named_hnsw("text", None, HnswConfig::default())
        .unwrap();
    assert_eq!(
        c.view()
            .search_named_ann_at("text", None, &[1.0, 1.0], 1, 10, 0)
            .unwrap()
            .len(),
        1
    );
    assert!(
        c.view()
            .search_named_ann_at("text", None, &[1.0, 1.0], 1, 10, u64::MAX)
            .unwrap()
            .is_empty()
    );
    assert!(
        c.view()
            .search_named_at("text", None, &[1.0, 1.0], 1, u64::MAX)
            .unwrap()
            .is_empty()
    );
}
