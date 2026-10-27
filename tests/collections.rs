//! TEST-003: named collections on format v4.

use std::fs;
use vlite::{
    CollectionSettings, CollectionSpec, Condition, Database, DistanceMetric, Error, FormatVersion,
    HnswConfig, HnswRebuildLimits, HnswRebuildStatus, Metadata, MetadataIndexKind,
    MetadataIndexSpec, MetadataValue, Mutation, SettingsPersistence, maintenance,
};

fn spec(dimensions: usize) -> CollectionSpec {
    CollectionSpec {
        dimensions,
        settings: CollectionSettings::default(),
    }
}

fn v4(path: &std::path::Path, dimensions: usize) -> Database {
    Database::create_with_format(path, dimensions, FormatVersion::V4).unwrap()
}

fn vector(dimensions: usize, seed: f32) -> Vec<f32> {
    (0..dimensions).map(|i| seed + i as f32 * 0.001).collect()
}

#[test]
fn collections_with_different_dimensions_coexist_and_survive_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("db.vlite");
    let mut db = v4(&path, 2);
    let small = db.create_collection("small", spec(3)).unwrap();
    let large = db.create_collection("large", spec(768)).unwrap();
    assert_eq!((small.id, large.id), (1, 2));
    assert_eq!((small.live_records, small.settings_persisted), (0, true));

    // The same ID is independent in each namespace.
    db.put(7, &[0.5, 0.5], b"default").unwrap();
    let small_revision = db
        .collection_mut("small")
        .unwrap()
        .put(7, &[1.0, 0.0, 0.0], b"small")
        .unwrap();
    db.collection_mut("large")
        .unwrap()
        .put(7, &vector(768, 0.25), b"large")
        .unwrap();
    db.collection_mut("large")
        .unwrap()
        .put(8, &vector(768, 9.0), b"far")
        .unwrap();
    assert_eq!(
        db.collection_mut("small")
            .unwrap()
            .put(1, &[1.0, 2.0], b"")
            .unwrap_err()
            .to_string(),
        Error::DimensionMismatch {
            expected: 3,
            actual: 2
        }
        .to_string()
    );
    db.close().unwrap();

    let db = Database::open(&path).unwrap();
    let names: Vec<_> = db
        .collections()
        .unwrap()
        .into_iter()
        .map(|info| (info.id, info.name, info.dimensions, info.live_records))
        .collect();
    assert_eq!(
        names,
        [
            (0, "default".to_owned(), 2, 1),
            (1, "small".to_owned(), 3, 1),
            (2, "large".to_owned(), 768, 2)
        ]
    );
    assert_eq!(db.len(), 1);
    assert_eq!(db.get(7).unwrap().unwrap().payload, b"default");
    let small = db.collection("small").unwrap();
    assert_eq!(small.get(7).unwrap().unwrap().payload, b"small");
    assert_eq!(small.revision(7).unwrap(), Some(small_revision));
    assert_eq!(small.get(8).unwrap(), None);
    let hits = db
        .collection("large")
        .unwrap()
        .search(&vector(768, 0.25), 1)
        .unwrap();
    assert_eq!((hits[0].id, hits[0].distance), (7, 0.0));
    let hits = db.search(&[0.5, 0.5], 5).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(matches!(
        small.search(&[1.0, 0.0], 1),
        Err(Error::DimensionMismatch { .. })
    ));
}

#[test]
fn collection_names_limits_and_lifecycle_are_validated() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("db.vlite");
    let mut db = v4(&path, 2);
    for name in ["", "Docs", "-a", "a b", "é", &"a".repeat(65)] {
        assert!(
            matches!(
                db.create_collection(name, spec(2)),
                Err(Error::InvalidCollectionName)
            ),
            "{name}"
        );
    }
    assert!(matches!(
        db.create_collection("default", spec(2)),
        Err(Error::DefaultCollectionImmutable)
    ));
    assert!(matches!(
        db.drop_collection("default"),
        Err(Error::DefaultCollectionImmutable)
    ));
    assert!(matches!(
        db.create_collection("zero", spec(0)),
        Err(Error::InvalidDimensions(0))
    ));
    let mut bad = CollectionSettings::default();
    bad.rerank_multiplier = 0;
    assert!(matches!(
        db.create_collection(
            "bad",
            CollectionSpec {
                dimensions: 2,
                settings: bad
            }
        ),
        Err(Error::InvalidCollectionSettings(_))
    ));
    assert!(matches!(
        db.collection("missing"),
        Err(Error::CollectionNotFound)
    ));
    assert!(matches!(
        db.drop_collection("missing"),
        Err(Error::CollectionNotFound)
    ));
    assert!(matches!(
        db.collection("Bad Name"),
        Err(Error::InvalidCollectionName)
    ));

    db.create_collection("a_1-x", spec(2)).unwrap();
    assert!(matches!(
        db.create_collection("a_1-x", spec(2)),
        Err(Error::CollectionExists)
    ));
    db.collection_mut("a_1-x")
        .unwrap()
        .put(1, &[1.0, 1.0], b"")
        .unwrap();
    assert!(matches!(
        db.drop_collection("a_1-x"),
        Err(Error::CollectionNotEmpty)
    ));
    assert!(db.collection_mut("a_1-x").unwrap().delete(1).unwrap());
    db.drop_collection("a_1-x").unwrap();
    assert!(matches!(
        db.collection("a_1-x"),
        Err(Error::CollectionNotFound)
    ));

    // `default` counts toward the 64-collection limit; IDs are never reused
    // (`a_1-x` used ID 1 before it was dropped).
    for index in 1..64 {
        let info = db.create_collection(&format!("c{index}"), spec(2)).unwrap();
        assert_eq!(info.id, index + 1);
    }
    assert!(matches!(
        db.create_collection("overflow", spec(2)),
        Err(Error::TooManyCollections)
    ));
    db.drop_collection("c5").unwrap();
    assert_eq!(db.create_collection("again", spec(2)).unwrap().id, 65);
    let before = fs::metadata(&path).unwrap().len();
    assert!(db.create_collection("overflow", spec(2)).is_err());
    assert_eq!(fs::metadata(&path).unwrap().len(), before);
    db.close().unwrap();

    let db = Database::open(&path).unwrap();
    assert_eq!(db.collections().unwrap().len(), 64);
    assert_eq!(db.collection("again").unwrap().id(), 65);
    assert!(matches!(
        db.collection("c5"),
        Err(Error::CollectionNotFound)
    ));
}

#[test]
fn collection_ids_are_never_reused_across_reopen_compaction_upgrade_and_restore() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("db.vlite");
    let mut db = v4(&path, 2);
    let id = |db: &Database, name: &str| db.collection(name).unwrap().id();
    assert_eq!(db.create_collection("a", spec(2)).unwrap().id, 1);
    assert_eq!(db.create_collection("b", spec(2)).unwrap().id, 2);
    db.drop_collection("b").unwrap();
    // Same name and a different name both get fresh IDs.
    assert_eq!(db.create_collection("b", spec(2)).unwrap().id, 3);
    db.drop_collection("b").unwrap();
    db.close().unwrap();

    let mut db = Database::open(&path).unwrap();
    assert_eq!(db.create_collection("c", spec(2)).unwrap().id, 4);
    db.drop_collection("c").unwrap();
    db.close().unwrap();
    let mut db = Database::open(&path).unwrap();
    assert_eq!(db.create_collection("c", spec(2)).unwrap().id, 5);
    db.drop_collection("c").unwrap();
    db.collection_mut("a")
        .unwrap()
        .put(1, &[1.0, 2.0], b"a")
        .unwrap();
    db.close().unwrap();

    // Rewrites start a new lineage but inherit the high-water mark (5).
    let compacted = directory.path().join("compacted.vlite");
    maintenance::compact(&path, &compacted).unwrap();
    let upgraded = directory.path().join("upgraded.vlite");
    maintenance::upgrade(&path, &upgraded).unwrap();
    let backup = directory.path().join("backup.vlite");
    maintenance::backup(&path, &backup).unwrap();
    let restored = directory.path().join("restored.vlite");
    maintenance::restore(&backup, &restored).unwrap();
    for (rewritten, next) in [(&compacted, 6), (&upgraded, 6), (&restored, 6)] {
        let mut db = Database::open(rewritten).unwrap();
        assert_eq!(id(&db, "a"), 1, "{rewritten:?}");
        assert_eq!(
            db.collection("a").unwrap().get(1).unwrap().unwrap().payload,
            b"a"
        );
        assert_eq!(db.create_collection("c", spec(2)).unwrap().id, next);
        db.close().unwrap();
        let db = Database::open(rewritten).unwrap();
        assert_eq!(id(&db, "c"), next, "{rewritten:?}");
    }

    // A second compaction of a compacted file still carries the mark.
    let twice = directory.path().join("twice.vlite");
    maintenance::compact(&compacted, &twice).unwrap();
    let mut db = Database::open(&twice).unwrap();
    assert_eq!(
        (id(&db, "a"), id(&db, "c")),
        (1, 6),
        "live IDs are restated unchanged"
    );
    assert_eq!(db.create_collection("d", spec(2)).unwrap().id, 7);
}

#[test]
fn settings_persist_on_v4_and_are_memory_only_on_v3() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("v4.vlite");
    let mut db = v4(&path, 2);
    db.create_collection("docs", spec(2)).unwrap();
    let mut settings = CollectionSettings::default();
    settings.metric = DistanceMetric::Cosine;
    settings.rerank_multiplier = 8;
    settings.text_fields = vec!["title".into(), "body".into()];
    settings.metadata_indexes = vec![MetadataIndexSpec {
        field: "year".into(),
        kind: MetadataIndexKind::EqRange,
    }];
    assert_eq!(
        db.set_collection_settings("docs", settings.clone())
            .unwrap(),
        SettingsPersistence::Persisted
    );
    let mut duplicate = settings.clone();
    duplicate.text_fields.push("title".into());
    assert!(matches!(
        db.set_collection_settings("docs", duplicate),
        Err(Error::InvalidCollectionSettings(_))
    ));
    db.close().unwrap();
    let db = Database::open(&path).unwrap();
    assert_eq!(db.collection("docs").unwrap().settings(), &settings);
    let info = db.collections().unwrap();
    assert!(info[1].settings_persisted);
    assert!(!info[0].settings_persisted);
    drop(db);

    let path = directory.path().join("v3.vlite");
    let mut db = Database::create_with_format(&path, 2, FormatVersion::V3).unwrap();
    assert_eq!(db.format_version(), 3);
    assert!(matches!(
        db.create_collection("docs", spec(2)),
        Err(Error::CollectionsRequireUpgrade)
    ));
    assert!(matches!(
        db.drop_collection("docs"),
        Err(Error::CollectionsRequireUpgrade)
    ));
    let before = fs::metadata(&path).unwrap().len();
    assert_eq!(
        db.set_collection_settings("default", settings.clone())
            .unwrap(),
        SettingsPersistence::MemoryOnly
    );
    assert_eq!(db.collection("default").unwrap().settings(), &settings);
    assert_eq!(fs::metadata(&path).unwrap().len(), before);
    db.close().unwrap();
    assert_eq!(fs::read(&path).unwrap().len() as u64, before);
    let db = Database::open(&path).unwrap();
    assert_eq!(
        db.collection("default").unwrap().settings(),
        &CollectionSettings::default()
    );
    assert_eq!(db.collections().unwrap().len(), 1);
}

#[test]
fn handles_mirror_database_writes_and_filtered_search() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("db.vlite");
    let mut db = v4(&path, 2);
    db.create_collection("docs", spec(2)).unwrap();
    let tagged = |tag: &str| -> Metadata {
        [("tag".to_owned(), MetadataValue::String(tag.to_owned()))].into()
    };
    {
        let mut docs = db.collection_mut("docs").unwrap();
        docs.put_with_metadata(1, &[0.0, 0.0], b"a", &tagged("x"))
            .unwrap();
        docs.write_batch(&[
            Mutation::PutWithMetadata {
                id: 2,
                vector: vec![1.0, 0.0],
                payload: b"b".to_vec(),
                metadata: tagged("y"),
            },
            Mutation::Put {
                id: 3,
                vector: vec![2.0, 0.0],
                payload: b"c".to_vec(),
            },
            Mutation::Delete { id: 1 },
        ])
        .unwrap();
        assert_eq!(docs.view().len(), 2);
    }
    assert!(db.is_empty());
    let docs = db.collection("docs").unwrap();
    let filter = [Condition::Eq {
        field: "tag".into(),
        value: MetadataValue::String("y".into()),
    }];
    let hits = docs.search_filtered(&[0.0, 0.0], 5, &filter).unwrap();
    assert_eq!(hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), [2]);
    let (hits, stats) = docs
        .search_with_stats(&[0.0, 0.0], 5, &[], DistanceMetric::SquaredL2)
        .unwrap();
    assert_eq!(hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), [2, 3]);
    assert_eq!((stats.records_scanned, stats.distance_evaluations), (2, 2));
    assert_eq!(
        docs.search_filtered_with_metric(&[1.0, 0.0], 5, &filter, DistanceMetric::Cosine)
            .unwrap()[0]
            .id,
        2
    );
    drop(db);
    let db = Database::open(&path).unwrap();
    assert_eq!(db.collection("docs").unwrap().len(), 2);
}

#[test]
fn single_rebuild_job_is_shared_across_collections() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("db.vlite");
    let mut db = v4(&path, 2);
    db.create_collection("docs", spec(2)).unwrap();
    for id in 0..20 {
        db.put(id, &[id as f32, 1.0], b"").unwrap();
        db.collection_mut("docs")
            .unwrap()
            .put(id, &[1.0, id as f32], b"")
            .unwrap();
    }
    let limits = HnswRebuildLimits {
        max_records: 1000,
        max_snapshot_vector_bytes: 1 << 20,
    };
    let config = HnswConfig::default();
    db.collection_mut("docs")
        .unwrap()
        .start_hnsw_rebuild(config, DistanceMetric::SquaredL2, limits)
        .unwrap();
    assert!(matches!(
        db.start_hnsw_rebuild(config, DistanceMetric::SquaredL2, limits),
        Err(Error::HnswRebuildBusy)
    ));
    assert_eq!(db.poll_hnsw_rebuild().unwrap(), HnswRebuildStatus::Idle);
    assert!(!db.cancel_hnsw_rebuild());
    let status = loop {
        let status = db
            .collection_mut("docs")
            .unwrap()
            .poll_hnsw_rebuild()
            .unwrap();
        if status != HnswRebuildStatus::Running {
            break status;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    assert_eq!(status, HnswRebuildStatus::Published);
    let hits = db
        .collection("docs")
        .unwrap()
        .search_approximate_ready_filtered_with_metric(
            &[1.0, 3.0],
            1,
            32,
            config,
            &[],
            DistanceMetric::SquaredL2,
        )
        .unwrap();
    assert_eq!(hits[0].id, 3);
    // The default collection's index is untouched.
    assert!(matches!(
        db.search_approximate_ready(&[3.0, 1.0], 1, 32, config),
        Err(Error::IndexNotReady)
    ));
    db.start_hnsw_rebuild(config, DistanceMetric::SquaredL2, limits)
        .unwrap();
    assert!(db.cancel_hnsw_rebuild());
}
