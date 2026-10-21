//! TEST-004: persistent per-record revisions and compare-and-set.

use std::fs;
use vlite::{
    CollectionSettings, CollectionSpec, Database, Error, Expected, FormatVersion, Metadata,
    Mutation, maintenance,
};

fn file_len(path: &std::path::Path) -> u64 {
    fs::metadata(path).unwrap().len()
}

#[test]
fn revision_is_commit_sequence_and_batches_share_it() {
    for format in [FormatVersion::V3, FormatVersion::V4] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("db.vlite");
        let mut db = Database::create_with_format(&path, 2, format).unwrap();
        assert_eq!(db.revision(1).unwrap(), None);
        db.put(1, &[1.0, 0.0], b"").unwrap();
        assert_eq!(db.revision(1).unwrap(), Some(1));
        db.put(1, &[2.0, 0.0], b"").unwrap();
        assert_eq!(db.revision(1).unwrap(), Some(2));
        db.write_batch(&[
            Mutation::Put {
                id: 2,
                vector: vec![0.0, 1.0],
                payload: vec![],
            },
            Mutation::Put {
                id: 3,
                vector: vec![0.0, 2.0],
                payload: vec![],
            },
        ])
        .unwrap();
        assert_eq!(db.revision(2).unwrap(), Some(3));
        assert_eq!(db.revision(3).unwrap(), Some(3));
        assert_eq!(db.revision(1).unwrap(), Some(2));
        assert!(db.delete(2).unwrap());
        assert_eq!(db.revision(2).unwrap(), None);
        db.close().unwrap();

        let db = Database::open(&path).unwrap();
        assert_eq!(db.revision(1).unwrap(), Some(2));
        assert_eq!(db.revision(3).unwrap(), Some(3));
        assert_eq!(db.revision(2).unwrap(), None);
    }
}

#[test]
fn conditional_writes_check_revisions_without_writing_on_mismatch() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("db.vlite");
    let mut db = Database::create_with_format(&path, 2, FormatVersion::V4).unwrap();
    let none = Metadata::new();

    let first = db
        .put_with_metadata_if(1, &[1.0, 0.0], b"a", &none, Expected::Absent)
        .unwrap();
    let len = file_len(&path);
    assert!(matches!(
        db.put_with_metadata_if(1, &[1.0, 0.0], b"b", &none, Expected::Absent),
        Err(Error::RevisionMismatch {
            current: Some(revision)
        }) if revision == first
    ));
    assert!(matches!(
        db.put_with_metadata_if(1, &[1.0, 0.0], b"b", &none, Expected::Revision(first + 9)),
        Err(Error::RevisionMismatch { .. })
    ));
    assert!(matches!(
        db.put_with_metadata_if(2, &[1.0, 0.0], b"b", &none, Expected::Revision(first)),
        Err(Error::RevisionMismatch { current: None })
    ));
    // Input validation precedes the precondition check.
    assert!(matches!(
        db.put_with_metadata_if(1, &[1.0], b"b", &none, Expected::Absent),
        Err(Error::DimensionMismatch { .. })
    ));
    assert!(matches!(
        db.delete_if(1, Expected::Revision(first + 1)),
        Err(Error::RevisionMismatch { .. })
    ));
    assert_eq!(file_len(&path), len);
    assert_eq!(db.get(1).unwrap().unwrap().payload, b"a");

    let second = db
        .put_with_metadata_if(1, &[3.0, 0.0], b"c", &none, Expected::Revision(first))
        .unwrap();
    assert!(second > first);
    let any = db
        .put_with_metadata_if(1, &[4.0, 0.0], b"d", &none, Expected::Any)
        .unwrap();
    assert_eq!(db.revision(1).unwrap(), Some(any));
    assert!(!db.delete_if(7, Expected::Absent).unwrap());
    assert!(!db.delete_if(7, Expected::Any).unwrap());
    assert!(matches!(
        db.delete_if(1, Expected::Absent),
        Err(Error::RevisionMismatch { current: Some(_) })
    ));
    assert!(db.delete_if(1, Expected::Revision(any)).unwrap());
    assert_eq!(db.revision(1).unwrap(), None);
}

#[test]
fn collection_handles_scope_revisions_per_namespace() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("db.vlite");
    let mut db = Database::create_with_format(&path, 2, FormatVersion::V4).unwrap();
    db.create_collection(
        "docs",
        CollectionSpec {
            dimensions: 2,
            settings: CollectionSettings::default(),
        },
    )
    .unwrap();
    let default_revision = db
        .put_with_metadata_if(5, &[1.0, 0.0], b"", &Metadata::new(), Expected::Absent)
        .unwrap();
    let mut docs = db.collection_mut("docs").unwrap();
    let docs_revision = docs
        .put_with_metadata_if(5, &[1.0, 0.0], b"", &Metadata::new(), Expected::Absent)
        .unwrap();
    assert!(matches!(
        docs.delete_if(5, Expected::Revision(default_revision)),
        Err(Error::RevisionMismatch { .. })
    ));
    assert!(
        docs.delete_if(5, Expected::Revision(docs_revision))
            .unwrap()
    );
    assert_eq!(db.revision(5).unwrap(), Some(default_revision));
}

#[test]
fn offline_rewrites_assign_new_lineage_and_new_revisions() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.vlite");
    let mut db = Database::create_with_format(&source, 2, FormatVersion::V4).unwrap();
    for id in 1..=3 {
        db.put(id, &[id as f32, 0.0], b"").unwrap();
    }
    db.put(1, &[9.0, 0.0], b"").unwrap();
    let lineage = db.lineage().unwrap();
    db.close().unwrap();

    let compacted = directory.path().join("compacted.vlite");
    maintenance::compact(&source, &compacted).unwrap();
    let restored = directory.path().join("restored.vlite");
    maintenance::restore(&source, &restored).unwrap();
    let upgraded = directory.path().join("upgraded.vlite");
    maintenance::upgrade(&source, &upgraded).unwrap();
    let mut lineages = vec![lineage];
    for path in [&compacted, &restored, &upgraded] {
        let db = Database::open(path).unwrap();
        let next = db.lineage().unwrap();
        assert!(!lineages.contains(&next));
        lineages.push(next);
        assert_eq!(db.get(1).unwrap().unwrap().vector, [9.0, 0.0]);
        assert_eq!(db.len(), 3);
    }
    // Restore keeps the log (and therefore revisions); compaction renumbers.
    let restored = Database::open(&restored).unwrap();
    assert_eq!(restored.revision(1).unwrap(), Some(4));
    let compacted = Database::open(&compacted).unwrap();
    assert!(compacted.revision(1).unwrap().unwrap() <= 3);
}
