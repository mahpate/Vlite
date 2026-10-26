//! EPIC-004 H-4 integrity matrix and the TEST-005 legacy-path matrix
//! (ITEM-017/018).
//!
//! LEG-001..LEG-008 through the public API: ordinary delete, conditional
//! delete, batch delete, expiry, overwrite, metadata update and import paths
//! on vector-only files, on graph files with unattached records and on
//! attached records, in the default and in named collections. Refused
//! operations write nothing; accepted ones keep their ordinary behavior.
//!
//! LEG-009..LEG-016 (maintenance, point-in-time restore, change feeds,
//! replicas, ANN derivatives, query language) on the same three setups and
//! collections. "Nothing written" means equal bytes and `inspect` view of the
//! closed file before and after. LEG-018 is the default build of
//! `tests\graph_compat.rs`.

mod graph_support;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use graph_support::{
    FLAG_ENCRYPTED, FLAG_GRAPH, Snapshot, assert_counts, header_flags, key, live_bytes, snapshot,
    snapshot_bytes,
};
use tempfile::tempdir;
use vlite::changes::{Change, ChangeKind, FeedCursor, read_log_changes};
use vlite::maintenance::{self, Inspection, RestoreTarget};
use vlite::property_graph::{GraphOp, GraphOpOutcome};
use vlite::query_language::parse;
use vlite::{
    AnnSnapshotLoad, CollectionSettings, CollectionSpec, CreateOptions, Database, DiskAnnIndex,
    DiskAnnOptions, DistanceMetric, EncryptionKey, Error, Expected, FormatVersion, HnswConfig,
    InvalidRowPolicy, LogSegment, Metadata, MetadataValue, Mutation, ReplicaFollower, SearchHit,
};

const FAR_FUTURE: u64 = 4_102_444_800_000;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Setup {
    VectorOnly,
    GraphUnattached,
    GraphAttached,
}

const STATES: [Setup; 3] = [
    Setup::VectorOnly,
    Setup::GraphUnattached,
    Setup::GraphAttached,
];

const COLLECTIONS: [&str; 2] = ["default", "docs"];

impl Setup {
    fn graph(self) -> bool {
        self != Setup::VectorOnly
    }

    /// `[graphs, nodes, edges, attachments]` right after `setup`.
    fn counts(self) -> [usize; 4] {
        match self {
            Setup::VectorOnly => [0; 4],
            Setup::GraphUnattached => [1, 1, 0, 0],
            Setup::GraphAttached => [1, 1, 0, 1],
        }
    }
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
    setup_with(path, setup, collection, CreateOptions::default())
}

/// `setup` with explicit create options (vector-only files are always V5).
fn setup_with(path: &Path, setup: Setup, collection: &str, options: CreateOptions) -> Database {
    let mut db = match setup {
        Setup::VectorOnly => Database::create_with_options(
            path,
            2,
            CreateOptions {
                format: Some(FormatVersion::V5),
                ..options
            },
        )
        .unwrap(),
        _ => Database::create_with_graph(path, 2, options).unwrap(),
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

/// Closed `setup_with` file `{name}-{collection}-{state:?}.vlite` in `dir`.
fn closed(
    dir: &Path,
    name: &str,
    state: Setup,
    collection: &str,
    options: CreateOptions,
) -> PathBuf {
    let path = dir.join(format!("{name}-{collection}-{state:?}.vlite"));
    setup_with(&path, state, collection, options)
        .close()
        .unwrap();
    path
}

fn options(key: Option<&EncryptionKey>) -> CreateOptions {
    CreateOptions {
        encryption: key.cloned(),
        ..CreateOptions::default()
    }
}

fn inspect(path: &Path, key: Option<&EncryptionKey>) -> Inspection {
    match key {
        Some(key) => maintenance::inspect_with_key(path, key),
        None => maintenance::inspect(path),
    }
    .unwrap()
}

/// Bytes and `inspect*` view of a closed file ("nothing written" evidence).
fn fingerprint(path: &Path, key: Option<&EncryptionKey>) -> (Vec<u8>, Inspection) {
    (fs::read(path).unwrap(), inspect(path, key))
}

/// `FLAG_GRAPH` in the header and `inspect_graph` agree with `state`.
#[track_caller]
fn assert_graph(path: &Path, key: Option<&EncryptionKey>, state: Setup, counts: [usize; 4]) {
    let inspection = maintenance::inspect_graph(path, key).unwrap();
    assert_eq!(inspection.inspection, inspect(path, key));
    assert_eq!(inspection.graph_enabled, state.graph(), "{path:?}");
    assert_eq!(
        header_flags(path) & FLAG_GRAPH != 0,
        state.graph(),
        "{path:?}"
    );
    assert_counts(&inspection.counts, counts);
}

/// Whether each of `ids` is live in `collection` of a closed file.
fn present(path: &Path, key: Option<&EncryptionKey>, collection: &str, ids: &[u64]) -> Vec<bool> {
    let db = graph_support::open(path, key);
    let handle = db.collection(collection).unwrap();
    ids.iter()
        .map(|&id| handle.get(id).unwrap().is_some())
        .collect()
}

/// Full snapshot (including attachments) of an open database.
fn image(db: &Database) -> Snapshot {
    snapshot_bytes(live_bytes(db), None)
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

#[test]
fn compaction_copies_graph_state_flag_and_id_high_water() {
    // LEG-009
    let dir = tempdir().unwrap();
    for collection in COLLECTIONS {
        for state in STATES {
            for secret in [None, Some(key())] {
                let key = secret.as_ref();
                let label = format!("{collection} {state:?} encrypted={}", key.is_some());
                let src = dir.path().join(format!(
                    "compact-{}-{collection}-{state:?}.vlite",
                    key.is_some()
                ));
                let mut db = setup_with(&src, state, collection, options(key));
                if state.graph() {
                    let mut graph = db.graph_mut("g").unwrap();
                    let tmp = graph.create_node("tmp", &[], &Metadata::new()).unwrap();
                    assert_eq!(tmp.id, 1, "{label}: node IDs start at 0");
                    graph.delete_node("tmp", Expected::Any).unwrap();
                }
                db.close().unwrap();
                let before = fingerprint(&src, key);
                let expected = snapshot(&src, key).without_revisions();
                let output = |name: &str| src.with_extension(format!("{name}.vlite"));
                let reports = match key {
                    None => vec![
                        (
                            output("compact"),
                            maintenance::compact(&src, output("compact")),
                        ),
                        (
                            output("compact-at"),
                            maintenance::compact_at(&src, output("compact-at"), 1),
                        ),
                    ],
                    Some(key) => vec![(
                        output("compact-key"),
                        maintenance::compact_with_key(&src, output("compact-key"), key),
                    )],
                };
                for (dst, report) in reports {
                    let report = report.unwrap();
                    assert_eq!(report.source, before.1, "{label}");
                    assert_eq!(report.output, inspect(&dst, key), "{label}");
                    assert_graph(&dst, key, state, state.counts());
                    assert_eq!(snapshot(&dst, key).without_revisions(), expected, "{label}");
                    let mut db = graph_support::open(&dst, key);
                    if state.graph() {
                        let node = db
                            .graph_mut("g")
                            .unwrap()
                            .create_node("next", &[], &Metadata::new())
                            .unwrap();
                        assert_eq!(node.id, 2, "{label}: {dst:?} reused deleted node ID 1");
                    }
                    db.close().unwrap();
                }
                assert_eq!(fingerprint(&src, key), before, "{label}");
            }
        }
    }
}

#[test]
fn compaction_expiry_omits_expired_records_but_never_attached_ones() {
    // LEG-009; attached records cannot carry an expiry (LEG-004).
    let dir = tempdir().unwrap();
    for collection in COLLECTIONS {
        for state in STATES {
            for secret in [None, Some(key())] {
                let key = secret.as_ref();
                let label = format!("{collection} {state:?} encrypted={}", key.is_some());
                let src = dir.path().join(format!(
                    "expiry-{}-{collection}-{state:?}.vlite",
                    key.is_some()
                ));
                let mut db = setup_with(&src, state, collection, options(key));
                db.collection_mut(collection)
                    .unwrap()
                    .put_with_expiry(
                        3,
                        &[3.0, 1.0],
                        b"three",
                        &Metadata::new(),
                        Some(FAR_FUTURE - 1),
                    )
                    .unwrap();
                db.close().unwrap();
                let before = fingerprint(&src, key);
                let output = |name: &str| src.with_extension(format!("{name}.vlite"));
                let reports = match key {
                    None => vec![
                        (
                            output("late"),
                            maintenance::compact_at(&src, output("late"), FAR_FUTURE),
                            [true, true, false],
                        ),
                        (
                            output("now"),
                            maintenance::compact(&src, output("now")),
                            [true; 3],
                        ),
                    ],
                    Some(key) => vec![(
                        output("now"),
                        maintenance::compact_with_key(&src, output("now"), key),
                        [true; 3],
                    )],
                };
                for (dst, report, live) in reports {
                    report.unwrap();
                    assert_eq!(
                        present(&dst, key, collection, &[1, 2, 3]),
                        live,
                        "{label} {dst:?}"
                    );
                    assert_graph(&dst, key, state, state.counts());
                }
                assert_eq!(fingerprint(&src, key), before, "{label}");
            }
        }
    }
}

#[test]
fn upgrade_refuses_graph_files_and_still_upgrades_vector_files() {
    // LEG-010
    let dir = tempdir().unwrap();
    for collection in COLLECTIONS {
        for state in STATES {
            let label = format!("{collection} {state:?}");
            let src = closed(
                dir.path(),
                "upgrade",
                state,
                collection,
                CreateOptions::default(),
            );
            let before = fingerprint(&src, None);
            let dst = src.with_extension("upgraded.vlite");
            match maintenance::upgrade(&src, &dst) {
                Err(Error::GraphUnsupportedOperation) if state.graph() => {
                    assert!(!dst.exists(), "{label}");
                }
                Ok(report) if !state.graph() => {
                    assert_eq!(report.source, before.1, "{label}");
                    assert_eq!(report.output, inspect(&dst, None), "{label}");
                    assert_eq!(report.output.format_version, 5, "{label}");
                    assert_graph(&dst, None, state, [0; 4]);
                    assert_eq!(
                        snapshot(&dst, None).without_revisions(),
                        snapshot(&src, None).without_revisions(),
                        "{label}"
                    );
                }
                other => panic!("{label}: {other:?}"),
            }
            assert_eq!(fingerprint(&src, None), before, "{label}");
        }
    }
    let src = dir.path().join("upgrade-v4.vlite");
    let mut db = Database::create_with_format(&src, 2, FormatVersion::V4).unwrap();
    db.put(1, &[1.0, 1.0], b"one").unwrap();
    db.put(2, &[2.0, 1.0], b"two").unwrap();
    db.close().unwrap();
    let before = fingerprint(&src, None);
    let dst = dir.path().join("upgrade-v4.upgraded.vlite");
    let report = maintenance::upgrade(&src, &dst).unwrap();
    assert_eq!(
        (report.source.format_version, report.output.format_version),
        (4, 4)
    );
    assert_eq!(report.output, inspect(&dst, None));
    assert_eq!(
        snapshot(&dst, None).without_revisions(),
        snapshot(&src, None).without_revisions()
    );
    assert!(!graph_support::open(&dst, None).is_graph_enabled());
    assert_eq!(fingerprint(&src, None), before);
}

#[test]
fn inspection_verification_backup_restore_and_drill_accept_graph_files() {
    // LEG-011
    let dir = tempdir().unwrap();
    for collection in COLLECTIONS {
        for state in STATES {
            let label = format!("{collection} {state:?}");
            let src = closed(
                dir.path(),
                "copy",
                state,
                collection,
                CreateOptions::default(),
            );
            let (bytes, inspection) = fingerprint(&src, None);
            let length = bytes.len() as u64;
            assert_eq!(
                (
                    inspection.format_version,
                    inspection.live_records,
                    inspection.file_bytes,
                    inspection.valid_bytes,
                    inspection.incomplete_tail_bytes
                ),
                (5, 2, length, length, 0),
                "{label}"
            );
            let detail = maintenance::inspect_detail(&src).unwrap();
            assert_eq!(detail.inspection, inspection, "{label}");
            assert_eq!(
                detail.collections,
                if collection == "default" { 1 } else { 2 },
                "{label}"
            );
            let verification = maintenance::verify(&src).unwrap();
            assert_eq!(verification.inspection, inspection, "{label}");
            assert_eq!(verification.bytes, length, "{label}");
            assert_eq!(
                maintenance::verify_graph(&src, None).unwrap().verification,
                verification,
                "{label}"
            );
            assert_graph(&src, None, state, state.counts());

            let backup = src.with_extension("backup.vlite");
            assert_eq!(
                maintenance::backup(&src, &backup).unwrap(),
                inspection,
                "{label}"
            );
            assert_eq!(fs::read(&backup).unwrap(), bytes, "{label}");
            let restored = src.with_extension("restored.vlite");
            assert_eq!(
                maintenance::restore(&backup, &restored).unwrap(),
                inspection,
                "{label}"
            );
            assert_eq!(header_flags(&restored), header_flags(&src), "{label}");
            assert_graph(&restored, None, state, state.counts());
            assert_eq!(snapshot(&restored, None), snapshot(&src, None), "{label}");
            let lineage = maintenance::inspect_detail(&restored).unwrap().lineage;
            assert!(lineage.is_some() && lineage != detail.lineage, "{label}");
            let drill = maintenance::drill(&backup, dir.path()).unwrap();
            assert_eq!(
                (drill.backup, drill.collections, drill.probes),
                (inspection.clone(), detail.collections, 1),
                "{label}"
            );
            assert_eq!(
                fingerprint(&src, None),
                (bytes.clone(), inspection),
                "{label}"
            );
            assert_eq!(fs::read(&backup).unwrap(), bytes, "{label}");

            if state.graph() {
                // A flipped bit in the last commit, a graph (op-12) frame,
                // is the existing corruption error; nothing is published.
                let start = bytes
                    .windows(4)
                    .rposition(|window| window == b"VREC")
                    .unwrap();
                assert_eq!(bytes[start + 4], 12, "{label}");
                let at = bytes.len() - 20;
                assert!(at >= start + 36, "{label}");
                let mut damaged_bytes = bytes.clone();
                damaged_bytes[at] ^= 0x10;
                let damaged = src.with_extension("damaged.vlite");
                fs::write(&damaged, &damaged_bytes).unwrap();
                assert!(
                    matches!(maintenance::inspect(&damaged), Err(Error::Corrupt { .. })),
                    "{label}"
                );
                assert!(
                    matches!(maintenance::verify(&damaged), Err(Error::Corrupt { .. })),
                    "{label}"
                );
                let copy = src.with_extension("damaged-backup.vlite");
                assert!(
                    matches!(
                        maintenance::backup(&damaged, &copy),
                        Err(Error::Corrupt { .. })
                    ),
                    "{label}"
                );
                assert!(!copy.exists(), "{label}");
                assert_eq!(fs::read(&damaged).unwrap(), damaged_bytes, "{label}");
            }
        }
    }
}

#[test]
fn reencryption_and_point_in_time_restore_carry_graph_frames() {
    // LEG-012
    let dir = tempdir().unwrap();
    let secret = key();
    for collection in COLLECTIONS {
        for state in STATES {
            let label = format!("{collection} {state:?}");
            let src = closed(
                dir.path(),
                "pitr",
                state,
                collection,
                CreateOptions::default(),
            );
            let before = fingerprint(&src, None);
            let expected = snapshot(&src, None);

            let encrypted = src.with_extension("encrypted.vlite");
            let report = maintenance::reencrypt(&src, None, &encrypted, Some(&secret)).unwrap();
            assert_eq!(report, inspect(&encrypted, Some(&secret)), "{label}");
            assert_eq!(
                header_flags(&encrypted),
                header_flags(&src) | FLAG_ENCRYPTED,
                "{label}"
            );
            assert_eq!(snapshot(&encrypted, Some(&secret)), expected, "{label}");
            let plain = src.with_extension("decrypted.vlite");
            maintenance::reencrypt(&encrypted, Some(&secret), &plain, None).unwrap();
            assert_eq!(header_flags(&plain), header_flags(&src), "{label}");
            assert_graph(&plain, None, state, state.counts());
            assert_eq!(snapshot(&plain, None), expected, "{label}");

            let head = before.1.commits;
            let commits = match (collection, state.graph()) {
                ("default", false) => 2,
                ("default", true) => 4,
                (_, false) => 3,
                (_, true) => 5,
            };
            assert_eq!(head, commits, "{label}");
            let empty = src.with_extension("header.vlite");
            maintenance::restore_to(None, &src, &empty, RestoreTarget::Sequence(0), None).unwrap();
            let header = file_len(&empty) as usize;
            for (back, counts) in [(2, [0; 4]), (1, [1, 0, 0, 0]), (0, state.counts())] {
                let target = head - back;
                let label = format!("{label} at {target}");
                let dst = src.with_extension(format!("at-{target}.vlite"));
                let report = maintenance::restore_to(
                    None,
                    &src,
                    &dst,
                    RestoreTarget::Sequence(target),
                    None,
                )
                .unwrap();
                assert_eq!(
                    (
                        report.restored_sequence,
                        report.source_head,
                        report.restored_at_ms
                    ),
                    (target, head, None),
                    "{label}"
                );
                assert_eq!(report.output, inspect(&dst, None), "{label}");
                assert_eq!(report.output.commits, target, "{label}");
                assert_eq!(header_flags(&dst), header_flags(&src), "{label}");
                let restored = fs::read(&dst).unwrap();
                assert_eq!(
                    restored[header..],
                    before.0[header..restored.len()],
                    "{label}: whole frames, byte for byte"
                );
                let counts = if state.graph() { counts } else { [0; 4] };
                assert_graph(&dst, None, state, counts);
                let live = if state.graph() {
                    [true; 2]
                } else {
                    [back < 2, back < 1]
                };
                assert_eq!(present(&dst, None, collection, &[1, 2]), live, "{label}");
                if back == 0 {
                    assert_eq!(snapshot(&dst, None), expected, "{label}");
                }
            }
            assert_eq!(fingerprint(&src, None), before, "{label}");
        }
    }
}

#[test]
fn change_feeds_refuse_graph_files_and_write_nothing() {
    // LEG-013
    let dir = tempdir().unwrap();
    let unsupported = |error: &Error| matches!(error, Error::GraphChangeFeedUnsupported);
    for collection in COLLECTIONS {
        for state in STATES {
            let label = format!("{collection} {state:?}");
            let path = dir
                .path()
                .join(format!("feed-{collection}-{state:?}.vlite"));
            let mut db = setup(&path, state, collection);
            if state.graph() {
                let committed = live_bytes(&db);
                assert!(unsupported(&db.feed_cursor().unwrap_err()), "{label}");
                let error = db.changes_after(&FeedCursor::beginning(), 100).unwrap_err();
                assert!(unsupported(&error), "{label}: {error:?}");
                assert_eq!(live_bytes(&db), committed, "{label}");
                db.close().unwrap();
                let before = fingerprint(&path, None);
                let lineage = maintenance::inspect_detail(&path).unwrap().lineage.unwrap();
                let cursors = [
                    FeedCursor::beginning(),
                    FeedCursor {
                        lineage,
                        sequence: 0,
                    },
                    FeedCursor {
                        lineage,
                        sequence: before.1.commits,
                    },
                ];
                for cursor in cursors {
                    let error = read_log_changes(&path, None, &cursor, 100).unwrap_err();
                    assert!(unsupported(&error), "{label} {cursor:?}: {error:?}");
                }
                let db = Database::open(&path).unwrap();
                for cursor in cursors {
                    let error = db.changes_after(&cursor, 100).unwrap_err();
                    assert!(unsupported(&error), "{label} {cursor:?}: {error:?}");
                }
                db.close().unwrap();
                assert_eq!(fingerprint(&path, None), before, "{label}");
            } else {
                let all = db.changes_after(&FeedCursor::beginning(), 100).unwrap();
                let cursor = db.feed_cursor().unwrap();
                assert_eq!(
                    (all.changes.len() as u64, all.head, all.next),
                    (cursor.sequence, cursor.sequence, cursor),
                    "{label}"
                );
                db.collection_mut(collection)
                    .unwrap()
                    .put(3, &[3.0, 1.0], b"three")
                    .unwrap();
                let next = db.changes_after(&cursor, 100).unwrap();
                assert!(
                    matches!(
                        next.changes[..],
                        [Change {
                            kind: ChangeKind::Put { id: 3, .. },
                            ..
                        }]
                    ),
                    "{label}: {:?}",
                    next.changes
                );
                db.close().unwrap();
                let before = fingerprint(&path, None);
                let logged = read_log_changes(&path, None, &FeedCursor::beginning(), 100).unwrap();
                assert_eq!(
                    logged.changes,
                    [all.changes, next.changes].concat(),
                    "{label}"
                );
                assert_eq!(fingerprint(&path, None), before, "{label}");
            }
        }
    }
}

/// One more commit on `collection` (record `id`) and, on graph files, one
/// more on graph "g" (node `n{id}`).
fn grow(db: &mut Database, state: Setup, collection: &str, id: u64) {
    db.collection_mut(collection)
        .unwrap()
        .put(id, &[id as f32, 1.0], b"")
        .unwrap();
    if state.graph() {
        db.graph_mut("g")
            .unwrap()
            .create_node(&format!("n{id}"), &[], &Metadata::new())
            .unwrap();
    }
}

#[test]
fn replicas_bootstrap_apply_sync_and_promote_graph_files() {
    // LEG-014; graph-unaware builds are LEG-018 in tests\graph_compat.rs.
    let dir = tempdir().unwrap();
    for collection in COLLECTIONS {
        for state in STATES {
            let label = format!("{collection} {state:?}");
            let name = |role: &str| {
                dir.path()
                    .join(format!("{role}-{collection}-{state:?}.vlite"))
            };
            let mut primary = setup(&name("primary"), state, collection);
            let bootstrap = primary.export_log(0, usize::MAX).unwrap();
            let mut follower = ReplicaFollower::create(name("follower"), &bootstrap, None).unwrap();
            assert_eq!(follower.db().is_graph_enabled(), state.graph(), "{label}");
            assert_eq!(live_bytes(follower.db()), live_bytes(&primary), "{label}");
            assert_eq!(image(follower.db()), image(&primary), "{label}");

            grow(&mut primary, state, collection, 3);
            let segment = primary
                .export_log(follower.applied_sequence(), usize::MAX)
                .unwrap();
            let status = follower.apply(&segment).unwrap();
            assert_eq!(
                (status.applied_sequence, status.lag),
                (segment.last_sequence, 0),
                "{label}"
            );
            assert_eq!(live_bytes(follower.db()), live_bytes(&primary), "{label}");

            grow(&mut primary, state, collection, 4);
            let segment = primary
                .export_log(follower.applied_sequence(), usize::MAX)
                .unwrap();
            let shipped = dir
                .path()
                .join(format!("segment-{collection}-{state:?}.bin"));
            segment.save(&shipped).unwrap();
            assert_eq!(LogSegment::load(&shipped).unwrap(), segment, "{label}");
            follower.apply_file(&shipped).unwrap();
            assert_eq!(live_bytes(follower.db()), live_bytes(&primary), "{label}");

            if state.graph() {
                let vector_primary = setup(&name("vector-primary"), Setup::VectorOnly, collection);
                let bootstrap = vector_primary.export_log(0, usize::MAX).unwrap();
                vector_primary.close().unwrap();
                let mut vector_follower =
                    ReplicaFollower::create(name("vector-follower"), &bootstrap, None).unwrap();
                let untouched = live_bytes(vector_follower.db());
                let segment = primary
                    .export_log(vector_follower.applied_sequence(), usize::MAX)
                    .unwrap();
                let error = vector_follower.apply(&segment).unwrap_err();
                assert!(
                    matches!(error, Error::ReplicaDiverged(_)),
                    "{label}: {error:?}"
                );
                assert_eq!(live_bytes(vector_follower.db()), untouched, "{label}");
                drop(vector_follower);
                assert_eq!(
                    fs::read(name("vector-follower")).unwrap(),
                    untouched,
                    "{label}"
                );
            }

            grow(&mut primary, state, collection, 5);
            primary.close().unwrap();
            follower.sync_from_log(name("primary"), usize::MAX).unwrap();
            assert_eq!(
                live_bytes(follower.db()),
                fs::read(name("primary")).unwrap(),
                "{label}"
            );
            let expected = snapshot(&name("primary"), None);
            assert_eq!(image(follower.db()), expected, "{label}");
            let [graphs, nodes, edges, attachments] = state.counts();
            let grown = |added| {
                if state.graph() {
                    [graphs, nodes + added, edges, attachments]
                } else {
                    [0; 4]
                }
            };
            assert_eq!(expected.counts(), grown(3), "{label}");
            assert!(
                matches!(
                    follower.put(9, &[9.0, 1.0], b""),
                    Err(Error::ReadOnlyReplica)
                ),
                "{label}"
            );

            let mut promoted = follower.promote();
            grow(&mut promoted, state, collection, 9);
            promoted.close().unwrap();
            assert_eq!(
                present(&name("follower"), None, collection, &[9]),
                [true],
                "{label}"
            );
            assert_eq!(
                snapshot(&name("follower"), None).counts(),
                grown(4),
                "{label}"
            );
        }
    }
}

/// HNSW search that never builds: the database-level API on `default`, the
/// collection handle elsewhere.
fn ready(
    db: &Database,
    collection: &str,
    query: &[f32],
    config: HnswConfig,
) -> vlite::Result<Vec<SearchHit>> {
    if collection == "default" {
        db.search_approximate_ready(query, 10, 64, config)
    } else {
        db.collection(collection)?
            .search_approximate_ready_filtered_with_metric(
                query,
                10,
                64,
                config,
                &[],
                DistanceMetric::SquaredL2,
            )
    }
}

#[test]
fn ann_indexes_snapshots_and_disk_ann_ignore_graph_state() {
    // LEG-015
    let dir = tempdir().unwrap();
    let config = HnswConfig::default();
    let query = [3.0, 2.0];
    let extra: Vec<Mutation> = (0..64u64)
        .map(|i| Mutation::Put {
            id: 100 + i,
            vector: vec![(i % 8) as f32 * 0.5, (i / 8) as f32 * 0.5],
            payload: Vec::new(),
        })
        .collect();
    for collection in COLLECTIONS {
        let mut results = Vec::new();
        for state in STATES {
            let label = format!("{collection} {state:?}");
            let path = dir.path().join(format!("ann-{collection}-{state:?}.vlite"));
            let mut db = setup(&path, state, collection);
            db.collection_mut(collection)
                .unwrap()
                .write_batch(&extra)
                .unwrap();
            db.close().unwrap();
            let bytes = fs::read(&path).unwrap();

            let mut db = Database::open(&path).unwrap();
            if collection == "default" {
                db.build_hnsw(config).unwrap();
            } else {
                db.collection_mut(collection)
                    .unwrap()
                    .build_hnsw_with_metric(config, DistanceMetric::SquaredL2)
                    .unwrap();
            }
            let hnsw = ready(&db, collection, &query, config).unwrap();
            let sidecar = path.with_extension("ann");
            assert_eq!(
                db.save_ann_snapshot(&sidecar).unwrap().indexes,
                1,
                "{label}"
            );
            let disk = path.with_extension("diskann");
            db.build_disk_ann(collection, &disk, config, DistanceMetric::SquaredL2)
                .unwrap();
            let index = DiskAnnIndex::open(&disk, DiskAnnOptions::default()).unwrap();
            let disk_hits = db.search_disk_ann(&index, &query, 10, 64).unwrap();
            assert_eq!(live_bytes(&db), bytes, "{label}");
            db.close().unwrap();

            let mut db = Database::open(&path).unwrap();
            let load = db.load_ann_snapshot(&sidecar).unwrap();
            assert!(
                matches!(load, AnnSnapshotLoad::Loaded { indexes: 1 }),
                "{label}: {load:?}"
            );
            assert_eq!(
                ready(&db, collection, &query, config).unwrap(),
                hnsw,
                "{label}"
            );
            assert_eq!(
                db.search_disk_ann(&index, &query, 10, 64).unwrap(),
                disk_hits,
                "{label}"
            );
            db.close().unwrap();
            assert_eq!(fs::read(&path).unwrap(), bytes, "{label}");
            results.push((hnsw, disk_hits));
        }
        assert_eq!(results[0].0.len(), 10, "{collection}");
        assert!(
            results.iter().all(|result| result == &results[0]),
            "{collection}: {results:?}"
        );
    }
}

#[test]
fn query_language_has_no_graph_syntax_and_unchanged_results() {
    // LEG-016; REG-005 in tests\graph_regression.rs pins the parse goldens
    // against the graph-off baseline.
    for statement in [
        "MATCH (n) RETURN n",
        "SEARCH NODE n",
        "CREATE GRAPH g",
        "SEARCH text NEAR $q TRAVERSE g",
        "SEARCH default NEAR $q TRAVERSE g",
        "SEARCH default NEAR $q MATCH (n)-[:KNOWS]->(m)",
    ] {
        assert!(parse(statement).is_err(), "{statement}");
    }
    let query = parse("SEARCH default NEAR $q K 2").unwrap();
    let parameters = BTreeMap::from([("q".to_string(), vec![3.0, 2.0])]);
    let dir = tempdir().unwrap();
    for collection in COLLECTIONS {
        let mut results = Vec::new();
        for state in STATES {
            let path = dir
                .path()
                .join(format!("query-{collection}-{state:?}.vlite"));
            let db = setup(&path, state, collection);
            let committed = live_bytes(&db);
            let output = query
                .execute(db.collection(collection).unwrap(), &parameters)
                .unwrap();
            assert_eq!(live_bytes(&db), committed, "{collection} {state:?}");
            results.push(output.hits);
            db.close().unwrap();
        }
        assert_eq!(
            results[0].iter().map(|hit| hit.id).collect::<Vec<_>>(),
            [2, 1],
            "{collection}"
        );
        assert!(
            results.iter().all(|hits| hits == &results[0]),
            "{collection}: {results:?}"
        );
    }
}
