//! EPIC-004 H-4 integrity matrix (ITEM-017/018, partial TEST-005), built only
//! with the `graph` feature.
//!
//! LEG-001..LEG-008 through the public API: ordinary delete, conditional
//! delete, batch delete, expiry, overwrite, metadata update and import paths
//! on vector-only files, on graph files with unattached records and on
//! attached records, in the default and in named collections. Refused
//! operations write nothing; accepted ones keep their ordinary behavior.

use std::fs;
use std::path::Path;

use tempfile::tempdir;
use vlite::property_graph::{GraphOp, GraphOpOutcome};
use vlite::{
    CollectionSettings, CollectionSpec, CreateOptions, Database, Error, Expected, FormatVersion,
    InvalidRowPolicy, Metadata, MetadataValue, Mutation,
};

const FAR_FUTURE: u64 = 4_102_444_800_000;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Setup {
    VectorOnly,
    GraphUnattached,
    GraphAttached,
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path).unwrap().len()
}

fn put(id: u64, value: f32) -> Mutation {
    Mutation::Put {
        id,
        vector: vec![value, 1.0],
        payload: Vec::new(),
    }
}

/// Creates a file with records 1 and 2 in `collection`; record 1 is attached
/// to node "a" of graph "g" in the attached setup.
fn setup(path: &Path, setup: Setup, collection: &str) -> Database {
    let mut db = match setup {
        Setup::VectorOnly => Database::create_with_format(path, 2, FormatVersion::V5).unwrap(),
        _ => Database::create_with_graph(path, 2, CreateOptions::default()).unwrap(),
    };
    if collection != "default" {
        db.create_collection(
            collection,
            CollectionSpec {
                dimensions: 2,
                settings: CollectionSettings::default(),
            },
        )
        .unwrap();
    }
    let mut handle = db.collection_mut(collection).unwrap();
    handle.put(1, &[1.0, 1.0], b"one").unwrap();
    handle.put(2, &[2.0, 1.0], b"two").unwrap();
    if setup != Setup::VectorOnly {
        db.create_graph("g").unwrap();
        let mut ops = vec![GraphOp::CreateNode {
            key: "a".into(),
            labels: Vec::new(),
            properties: Metadata::new(),
        }];
        if setup == Setup::GraphAttached {
            ops.push(GraphOp::Attach {
                node: "a".into(),
                collection: collection.into(),
                record: 1,
            });
        }
        db.graph_mut("g").unwrap().write_batch(&ops).unwrap();
    }
    db
}

#[track_caller]
fn refused<T: std::fmt::Debug>(
    result: vlite::Result<T>,
    attached: bool,
    error: fn(&Error) -> bool,
) -> bool {
    match result {
        Err(ref actual) if attached => {
            assert!(error(actual), "{actual:?}");
            true
        }
        Ok(_) if !attached => false,
        other => panic!("attached={attached}: unexpected {other:?}"),
    }
}

/// A named ordinary operation on record 1 and the error expected when the
/// record is attached.
type Case = (
    &'static str,
    fn(&mut Database, &str) -> vlite::Result<()>,
    fn(&Error) -> bool,
);

fn attached_record(error: &Error) -> bool {
    matches!(error, Error::GraphAttachedRecord)
}

#[test]
fn ordinary_deletes_and_expiry_keep_attached_records() {
    let dir = tempdir().unwrap();
    for collection in ["default", "docs"] {
        for state in [
            Setup::VectorOnly,
            Setup::GraphUnattached,
            Setup::GraphAttached,
        ] {
            let attached = state == Setup::GraphAttached;
            let cases: [Case; 6] = [
                (
                    "LEG-001 delete",
                    |db, c| db.collection_mut(c)?.delete(1).map(drop),
                    attached_record,
                ),
                (
                    "LEG-003 conditional delete",
                    |db, c| {
                        let revision = db.collection(c)?.revision(1)?;
                        db.collection_mut(c)?
                            .delete_if(1, Expected::Revision(revision.unwrap()))
                            .map(drop)
                    },
                    attached_record,
                ),
                (
                    "LEG-002 batch delete",
                    |db, c| {
                        db.collection_mut(c)?
                            .write_batch(&[put(2, 5.0), Mutation::Delete { id: 1 }])
                    },
                    attached_record,
                ),
                (
                    "LEG-004 expiry",
                    |db, c| {
                        db.collection_mut(c)?
                            .put_with_expiry(
                                1,
                                &[3.0, 1.0],
                                b"",
                                &Metadata::new(),
                                Some(FAR_FUTURE),
                            )
                            .map(drop)
                    },
                    |e| matches!(e, Error::GraphAttachedRecordExpiry),
                ),
                (
                    "LEG-007 import delete",
                    |db, c| {
                        let job = db.next_import_job_id()?;
                        db.collection_mut(c)?
                            .import_chunk(
                                job,
                                1,
                                &[Mutation::Delete { id: 1 }],
                                InvalidRowPolicy::SkipInvalid,
                            )
                            .map(drop)
                    },
                    attached_record,
                ),
                (
                    "LEG-007 import delete with invalid rows",
                    |db, c| {
                        let job = db.next_import_job_id()?;
                        db.collection_mut(c)?
                            .import_chunk(
                                job,
                                1,
                                &[
                                    Mutation::Put {
                                        id: 9,
                                        vector: vec![1.0],
                                        payload: Vec::new(),
                                    },
                                    Mutation::Delete { id: 1 },
                                ],
                                InvalidRowPolicy::RejectChunk,
                            )
                            .map(drop)
                    },
                    attached_record,
                ),
            ];
            for (name, operation, error) in cases {
                let path = dir
                    .path()
                    .join(format!("{collection}-{state:?}-{name}.vlite"));
                let mut db = setup(&path, state, collection);
                let before = file_len(&path);
                let revision = db.collection(collection).unwrap().revision(1).unwrap();
                let was_refused = refused(operation(&mut db, collection), attached, error);
                let label = format!("{collection} {state:?} {name}");
                if was_refused {
                    assert_eq!(file_len(&path), before, "{label}");
                    assert_eq!(
                        db.collection(collection).unwrap().revision(1).unwrap(),
                        revision,
                        "{label}"
                    );
                    assert_eq!(
                        db.collection(collection).unwrap().revision(2).unwrap(),
                        Some(revision.unwrap() + 1),
                        "{label}: batch prefix must not apply"
                    );
                } else if name.contains("invalid") {
                    // RejectChunk rejects the invalid row without writing.
                    assert_eq!(file_len(&path), before, "{label}");
                } else {
                    assert!(file_len(&path) > before, "{label}");
                }
                db.close().unwrap();
                // Whatever was accepted reopens cleanly.
                let db = Database::open(&path).unwrap();
                assert_eq!(
                    db.collection(collection).unwrap().get(1).unwrap().is_some(),
                    was_refused || name.contains("expiry") || name.contains("invalid"),
                    "{label}"
                );
            }
        }
    }
}

#[test]
fn ordinary_overwrites_and_metadata_updates_of_attached_records_stay_allowed() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("updates.vlite");
    let mut db = setup(&path, Setup::GraphAttached, "docs");
    let mut docs = db.collection_mut("docs").unwrap();
    // LEG-005: overwrite without expiry (single, batch and import).
    docs.put(1, &[4.0, 1.0], b"new").unwrap();
    docs.write_batch(&[put(1, 5.0)]).unwrap();
    docs.put_with_expiry(1, &[6.0, 1.0], b"", &Metadata::new(), None)
        .unwrap();
    // LEG-006: metadata update.
    let mut metadata = Metadata::new();
    metadata.insert("k".into(), MetadataValue::Integer(1));
    docs.update_metadata(1, &metadata).unwrap();
    let job = db.next_import_job_id().unwrap();
    let report = db
        .collection_mut("docs")
        .unwrap()
        .import_chunk(job, 1, &[put(1, 7.0)], InvalidRowPolicy::RejectChunk)
        .unwrap();
    assert!(report.revision.is_some());
    db.close().unwrap();
    let db = Database::open(&path).unwrap();
    assert_eq!(
        db.collection("docs")
            .unwrap()
            .get(1)
            .unwrap()
            .unwrap()
            .vector,
        [7.0, 1.0]
    );
}

#[test]
fn detaching_restores_ordinary_delete_and_expiry() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("detach.vlite");
    let mut db = setup(&path, Setup::GraphAttached, "default");
    assert!(matches!(db.delete(1), Err(Error::GraphAttachedRecord)));
    assert_eq!(
        db.graph_mut("g").unwrap().detach("a", "default").unwrap(),
        1
    );
    db.put_with_expiry(1, &[1.0, 1.0], b"", &Metadata::new(), Some(FAR_FUTURE))
        .unwrap();
    // A record with an expiry cannot be attached again.
    assert!(matches!(
        db.graph_mut("g").unwrap().attach("a", "default", 1),
        Err(Error::GraphRecordNotAttachable)
    ));
    assert!(db.delete(1).unwrap());
    db.close().unwrap();
    let db = Database::open(&path).unwrap();
    assert_eq!(db.get(1).unwrap(), None);
}

#[test]
fn graph_batches_carry_vector_writes_and_refuse_attached_deletes() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("batch.vlite");
    let mut db = setup(&path, Setup::GraphAttached, "docs");
    let before = file_len(&path);
    let delete = |id| GraphOp::Vector {
        collection: "docs".into(),
        mutation: Mutation::Delete { id },
    };
    assert!(matches!(
        db.graph_mut("g")
            .unwrap()
            .write_batch(&[delete(2), delete(1)]),
        Err(Error::GraphAttachedRecord)
    ));
    assert_eq!(file_len(&path), before);
    let result = db
        .graph_mut("g")
        .unwrap()
        .write_batch(&[
            delete(2),
            GraphOp::Detach {
                node: "a".into(),
                collection: "docs".into(),
            },
            delete(1),
        ])
        .unwrap();
    assert!(matches!(
        result.outcomes[..],
        [
            GraphOpOutcome::Vector,
            GraphOpOutcome::Detached { record: 1, .. },
            GraphOpOutcome::Vector
        ]
    ));
    db.close().unwrap();
    let db = Database::open(&path).unwrap();
    assert!(db.collection("docs").unwrap().is_empty());
}
