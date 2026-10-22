use std::fs;
use vlite::{Database, Error, FormatVersion, Metadata, MetadataValue, maintenance};

#[test]
fn expiry_is_invisible_at_the_boundary_and_reclaimed_by_compaction() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("ttl.vlite");
    let compacted = directory.path().join("ttl-compact.vlite");
    let mut db = Database::create_with_format(&source, 2, FormatVersion::V5).unwrap();
    db.put_with_expiry(1, &[0.0, 0.0], b"keep", &Metadata::new(), None)
        .unwrap();
    db.put_with_expiry(2, &[1.0, 0.0], b"expire", &Metadata::new(), Some(100))
        .unwrap();
    assert!(db.get_at(2, 99).unwrap().is_some());
    assert!(db.get_at(2, 100).unwrap().is_none());
    assert_eq!(
        db.search_at(&[1.0, 0.0], 10, 100)
            .unwrap()
            .iter()
            .map(|hit| hit.id)
            .collect::<Vec<_>>(),
        [1]
    );
    let page = db.browse_at(None, 10, 100).unwrap();
    assert_eq!(page.hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), [1]);
    assert_eq!(page.next_after, None);
    assert_eq!(db.expires_at(2).unwrap(), Some(100));
    db.close().unwrap();

    let db = Database::open(&source).unwrap();
    assert_eq!(db.format_version(), 5);
    assert!(db.get_at(2, 100).unwrap().is_none());
    drop(db);
    let report = maintenance::compact_at(&source, &compacted, 100).unwrap();
    assert_eq!(report.output.format_version, 5);
    let reopened = Database::open(compacted).unwrap();
    assert!(reopened.get_at(2, 0).unwrap().is_none());
    assert!(reopened.get(1).unwrap().is_some());
}

#[test]
fn metadata_only_update_preserves_vector_payload_and_expiry_across_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("metadata-only.vlite");
    let mut db = Database::create_with_format(&path, 32, FormatVersion::V5).unwrap();
    db.put_with_expiry(7, &[3.0; 32], b"payload", &Metadata::new(), Some(1234))
        .unwrap();
    let before = fs::metadata(&path).unwrap().len();
    let metadata = Metadata::from([("tag".into(), MetadataValue::String("updated".into()))]);
    let revision = db.update_metadata(7, &metadata).unwrap();
    let after = fs::metadata(&path).unwrap().len();
    assert!(
        after - before < 32 * 4,
        "metadata update rewrote vector bytes"
    );
    assert_eq!(db.revision(7).unwrap(), Some(revision));
    let record = db.get_at(7, 0).unwrap().unwrap();
    assert_eq!(record.vector, [3.0; 32]);
    assert_eq!(record.payload, b"payload");
    assert_eq!(record.metadata, metadata);
    assert_eq!(db.expires_at(7).unwrap(), Some(1234));
    db.close().unwrap();
    let reopened = Database::open(path).unwrap();
    assert_eq!(
        reopened.get_at(7, 0).unwrap().unwrap().metadata["tag"],
        MetadataValue::String("updated".into())
    );
    assert_eq!(reopened.expires_at(7).unwrap(), Some(1234));
}

#[test]
fn v5_tail_recovery_and_checksums_cover_expiry_fields() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("recovery.vlite");
    let mut db = Database::create_with_format(&path, 1, FormatVersion::V5).unwrap();
    db.put_with_expiry(1, &[1.0], b"", &Metadata::new(), Some(50))
        .unwrap();
    db.close().unwrap();
    let complete = fs::read(&path).unwrap();
    let mut truncated = complete.clone();
    truncated.truncate(truncated.len() - 5);
    fs::write(&path, &truncated).unwrap();
    assert!(matches!(
        Database::open(&path),
        Err(Error::IncompleteTail { .. })
    ));
    assert!(
        Database::discard_incomplete_tail(&path)
            .unwrap()
            .discarded_bytes
            > 0
    );
    assert!(Database::open(&path).unwrap().get(1).unwrap().is_none());

    fs::write(&path, &complete).unwrap();
    let mut corrupt = complete;
    let last = corrupt.len() - 1;
    corrupt[last] ^= 0x80;
    fs::write(&path, corrupt).unwrap();
    assert!(matches!(Database::open(&path), Err(Error::Corrupt { .. })));
}

#[test]
fn new_persisted_features_require_opt_in_v5() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("legacy.vlite");
    let mut db = Database::create(&path, 1).unwrap();
    db.put(1, &[1.0], b"x").unwrap();
    assert!(matches!(
        db.put_with_expiry(1, &[1.0], b"x", &Metadata::new(), Some(3)),
        Err(Error::ExpiryRequiresUpgrade)
    ));
    assert!(matches!(
        db.update_metadata(1, &Metadata::new()),
        Err(Error::ExpiryRequiresUpgrade)
    ));
    db.close().unwrap();
}

#[test]
fn expired_records_are_omitted_from_ready_ann_results() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("ann-ttl.vlite");
    let mut db = Database::create_with_format(&path, 1, FormatVersion::V5).unwrap();
    db.put_with_expiry(1, &[0.0], b"expired", &Metadata::new(), Some(0))
        .unwrap();
    db.put(2, &[1.0], b"live").unwrap();
    let config = vlite::HnswConfig::default();
    db.build_hnsw(config).unwrap();
    let hits = db
        .search_approximate_ready_filtered_with_metric(
            &[0.0],
            2,
            10,
            config,
            &[],
            vlite::DistanceMetric::SquaredL2,
        )
        .unwrap();
    assert_eq!(hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), [2]);
}
