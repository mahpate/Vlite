//! TEST-011: resumable chunked import (REQ-009, REQ-010).

use std::fs::{self, OpenOptions};
use std::path::Path;

use vlite::{
    CollectionSettings, CollectionSpec, Database, Error, FormatVersion, ImportChunkOutcome,
    ImportState, InvalidRowPolicy, MAX_BATCH_BYTES, MAX_BATCH_OPERATIONS, MAX_PAYLOAD_BYTES,
    MAX_REPORTED_INVALID_ROWS, Metadata, MetadataValue, Mutation, maintenance,
};

const REJECT: InvalidRowPolicy = InvalidRowPolicy::RejectChunk;
const SKIP: InvalidRowPolicy = InvalidRowPolicy::SkipInvalid;

fn put(id: u64, vector: &[f32], payload: &[u8]) -> Mutation {
    Mutation::Put {
        id,
        vector: vector.to_vec(),
        payload: payload.to_vec(),
    }
}

fn rows(range: std::ops::Range<u64>) -> Vec<Mutation> {
    range.map(|id| put(id, &[id as f32, 1.0], b"p")).collect()
}

fn v4(path: &Path) -> Database {
    Database::create_with_format(path, 2, FormatVersion::V4).unwrap()
}

fn v4_with_docs(path: &Path) -> Database {
    let mut db = v4(path);
    db.create_collection(
        "docs",
        CollectionSpec {
            dimensions: 3,
            settings: CollectionSettings::default(),
        },
    )
    .unwrap();
    db
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path).unwrap().len()
}

#[test]
fn imports_require_v4_and_write_nothing_on_older_formats() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("v3.vlite");
    let mut db = Database::create_with_format(&path, 2, FormatVersion::V3).unwrap();
    let before = file_len(&path);
    assert!(matches!(
        db.import_chunk(1, 1, &rows(0..2), REJECT),
        Err(Error::ImportsRequireUpgrade)
    ));
    assert!(matches!(
        db.finish_import(1),
        Err(Error::ImportsRequireUpgrade)
    ));
    assert!(matches!(
        db.abort_import(1),
        Err(Error::ImportsRequireUpgrade)
    ));
    assert!(matches!(
        db.next_import_job_id(),
        Err(Error::ImportsRequireUpgrade)
    ));
    assert!(db.is_empty());
    assert_eq!(file_len(&path), before);
}

#[test]
fn chunk_rows_and_checkpoint_commit_in_one_frame_and_survive_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("db.vlite");
    let mut db = v4(&path);
    assert_eq!(db.next_import_job_id().unwrap(), 1);
    let report = db.import_chunk(1, 1, &rows(0..3), REJECT).unwrap();
    assert_eq!(report.outcome, ImportChunkOutcome::Committed);
    assert_eq!((report.job, report.chunk), (1, 1));
    assert_eq!((report.rows_committed, report.rows_skipped), (3, 0));
    assert_eq!(report.invalid_rows, 0);
    assert!(report.invalid.is_empty());
    // One frame: every row shares the chunk's commit sequence.
    assert_eq!(report.revision, Some(1));
    for id in 0..3 {
        assert_eq!(db.revision(id).unwrap(), Some(1));
    }
    assert_eq!(db.next_import_job_id().unwrap(), 2);
    let second = db.import_chunk(1, 2, &rows(3..5), REJECT).unwrap();
    assert_eq!(second.revision, Some(2));
    // Simulated crash: drop without close, then reopen.
    drop(db);
    let db = Database::open(&path).unwrap();
    assert_eq!(db.len(), 5);
    let status = db.import_status(1).unwrap();
    assert_eq!(status.collection, "default");
    assert_eq!(status.state, ImportState::Open);
    assert_eq!((status.last_chunk, status.rows_committed), (2, 5));
    assert_eq!(db.revision(4).unwrap(), Some(2));
    assert_eq!(db.next_import_job_id().unwrap(), 2);
}

#[test]
fn identical_resubmission_is_exactly_once_and_conflicts_write_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("db.vlite");
    let mut db = v4(&path);
    let first = rows(0..2);
    db.import_chunk(5, 1, &first, REJECT).unwrap();
    let after_first = file_len(&path);

    let replay = db.import_chunk(5, 1, &first, REJECT).unwrap();
    assert_eq!(replay.outcome, ImportChunkOutcome::AlreadyCommitted);
    assert_eq!((replay.rows_committed, replay.rows_skipped), (2, 0));
    assert_eq!(replay.revision, None);
    assert_eq!(file_len(&path), after_first);

    let mut changed = first.clone();
    changed[1] = put(1, &[1.0, 1.0], b"q");
    for (chunk, operations, policy) in [
        (1, &changed, REJECT), // same chunk, different payload byte
        (1, &first, SKIP),     // same rows, different policy byte
        (3, &first, REJECT),   // gap
    ] {
        assert!(
            matches!(
                db.import_chunk(5, chunk, operations, policy),
                Err(Error::ImportConflict(_))
            ),
            "chunk {chunk} accepted"
        );
        assert_eq!(file_len(&path), after_first);
    }

    let second = rows(2..4);
    db.import_chunk(5, 2, &second, REJECT).unwrap();
    let after_second = file_len(&path);
    // Only the last chunk is replayable; earlier chunks conflict.
    assert!(matches!(
        db.import_chunk(5, 1, &first, REJECT),
        Err(Error::ImportConflict(_))
    ));
    drop(db);

    let mut db = Database::open(&path).unwrap();
    let replay = db.import_chunk(5, 2, &second, REJECT).unwrap();
    assert_eq!(replay.outcome, ImportChunkOutcome::AlreadyCommitted);
    assert_eq!(file_len(&path), after_second);
    assert_eq!(db.import_status(5).unwrap().rows_committed, 4);
    assert_eq!(db.len(), 4);
}

#[test]
fn rows_with_colliding_canonical_bytes_are_distinguished_by_the_digest() {
    // `crafted` has six components on a 2-d collection. Its canonical entry
    // bytes equal those of `valid` (a 2-d put followed by `Delete { id: 7 }`),
    // so a digest over raw canonical bytes would replay it as a duplicate.
    let tail = |bytes: [u8; 4]| f32::from_le_bytes(bytes);
    let crafted = [put(
        1,
        &[
            0.5,
            0.25,
            tail([2, 0, 0, 0]),
            tail([7, 0, 0, 0]),
            tail([0, 0, 0, 0]),
            tail([0, 0, 0, 0]),
        ],
        b"",
    )];
    let valid = [put(1, &[0.5, 0.25], b""), Mutation::Delete { id: 7 }];
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("db.vlite");
    let mut db = v4(&path);
    let committed = db.import_chunk(1, 1, &valid, SKIP).unwrap();
    assert_eq!(committed.rows_committed, 2);
    let before = file_len(&path);
    assert!(matches!(
        db.import_chunk(1, 1, &crafted, SKIP),
        Err(Error::ImportConflict(_))
    ));
    assert_eq!(file_len(&path), before);
}

#[test]
fn reject_policy_writes_nothing_and_reports_invalid_rows() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("db.vlite");
    let mut db = v4(&path);
    let before = file_len(&path);
    let mut operations = rows(0..4);
    operations[1] = put(1, &[1.0], b"secret-payload");
    operations[3] = put(3, &[f32::NAN, 1.0], b"");
    let report = db.import_chunk(1, 1, &operations, REJECT).unwrap();
    assert_eq!(report.outcome, ImportChunkOutcome::Rejected);
    assert_eq!((report.rows_committed, report.rows_skipped), (0, 0));
    assert_eq!(report.invalid_rows, 2);
    assert_eq!(report.revision, None);
    let codes: Vec<_> = report.invalid.iter().map(|r| (r.row, r.code)).collect();
    assert_eq!(codes, [(1, "dimension_mismatch"), (3, "non_finite_vector")]);
    assert!(
        report
            .invalid
            .iter()
            .all(|row| !row.message.contains("secret"))
    );
    assert_eq!(file_len(&path), before);
    assert!(db.is_empty());
    // The job does not exist until a chunk commits; chunk 1 is retried.
    assert!(matches!(db.import_status(1), Err(Error::ImportNotFound)));
    let fixed = rows(0..4);
    let report = db.import_chunk(1, 1, &fixed, REJECT).unwrap();
    assert_eq!(report.outcome, ImportChunkOutcome::Committed);
    assert_eq!(db.len(), 4);
}

#[test]
fn skip_policy_commits_valid_rows_and_counts_skipped_rows_durably() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("db.vlite");
    let mut db = v4(&path);
    let mut bad_metadata = Metadata::new();
    bad_metadata.insert("score".into(), MetadataValue::Float(f64::INFINITY));
    let operations = vec![
        put(0, &[0.0, 0.0], b"a"),
        put(1, &[1.0, 1.0], &vec![0; MAX_PAYLOAD_BYTES + 1]),
        Mutation::PutWithMetadata {
            id: 2,
            vector: vec![2.0, 2.0],
            payload: Vec::new(),
            metadata: bad_metadata,
        },
        Mutation::Delete { id: 99 },
    ];
    let report = db.import_chunk(1, 1, &operations, SKIP).unwrap();
    assert_eq!(report.outcome, ImportChunkOutcome::Committed);
    assert_eq!((report.rows_committed, report.rows_skipped), (2, 2));
    let codes: Vec<_> = report.invalid.iter().map(|r| (r.row, r.code)).collect();
    assert_eq!(codes, [(1, "payload_too_large"), (2, "invalid_metadata")]);
    // Replays report the same counts without writing.
    let len = file_len(&path);
    let replay = db.import_chunk(1, 1, &operations, SKIP).unwrap();
    assert_eq!(replay.outcome, ImportChunkOutcome::AlreadyCommitted);
    assert_eq!((replay.rows_committed, replay.rows_skipped), (2, 2));
    assert_eq!(replay.invalid, report.invalid);
    assert_eq!(file_len(&path), len);
    db.close().unwrap();
    let db = Database::open(&path).unwrap();
    assert_eq!(db.len(), 1);
    assert!(db.get(0).unwrap().is_some());
    let status = db.import_status(1).unwrap();
    assert_eq!((status.rows_committed, status.rows_skipped), (2, 2));
}

#[test]
fn skipping_every_row_commits_a_marker_only_checkpoint() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("db.vlite");
    let mut db = v4(&path);
    let operations = vec![put(0, &[1.0], b""), put(1, &[1.0, 2.0, 3.0], b"")];
    let report = db.import_chunk(1, 1, &operations, SKIP).unwrap();
    assert_eq!(report.outcome, ImportChunkOutcome::Committed);
    assert_eq!((report.rows_committed, report.rows_skipped), (0, 2));
    assert_eq!(report.revision, Some(1));
    drop(db);
    let mut db = Database::open(&path).unwrap();
    assert!(db.is_empty());
    let status = db.import_status(1).unwrap();
    assert_eq!((status.last_chunk, status.rows_skipped), (1, 2));
    let replay = db.import_chunk(1, 1, &operations, SKIP).unwrap();
    assert_eq!(replay.outcome, ImportChunkOutcome::AlreadyCommitted);
    assert_eq!(
        db.import_chunk(1, 2, &rows(0..1), REJECT).unwrap().outcome,
        ImportChunkOutcome::Committed
    );
}

#[test]
fn invalid_row_reports_are_bounded_but_counted_in_full() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = v4(&directory.path().join("db.vlite"));
    let operations: Vec<_> = (0..150).map(|id| put(id, &[1.0], b"")).collect();
    for policy in [REJECT, SKIP] {
        let report = db.import_chunk(1, 1, &operations, policy).unwrap();
        assert_eq!(report.invalid_rows, 150);
        assert_eq!(report.invalid.len(), MAX_REPORTED_INVALID_ROWS);
        assert_eq!(report.invalid.last().unwrap().row, 99);
    }
    assert_eq!(db.import_status(1).unwrap().rows_skipped, 150);
}

#[test]
fn chunk_size_limits_and_argument_validation_write_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("db.vlite");
    let mut db = v4(&path);
    let before = file_len(&path);
    let too_many = rows(0..MAX_BATCH_OPERATIONS as u64 + 1);
    assert!(matches!(
        db.import_chunk(1, 1, &too_many, SKIP),
        Err(Error::BatchTooLarge)
    ));
    let rows_needed = MAX_BATCH_BYTES / MAX_PAYLOAD_BYTES + 1;
    let too_many_bytes: Vec<_> = (0..rows_needed as u64)
        .map(|id| put(id, &[0.0, 0.0], &vec![1; MAX_PAYLOAD_BYTES]))
        .collect();
    assert!(matches!(
        db.import_chunk(1, 1, &too_many_bytes, REJECT),
        Err(Error::BatchTooLarge)
    ));
    assert!(matches!(
        db.import_chunk(0, 1, &rows(0..1), REJECT),
        Err(Error::ImportConflict(_))
    ));
    assert!(matches!(
        db.import_chunk(1, 0, &rows(0..1), REJECT),
        Err(Error::ImportConflict(_))
    ));
    assert!(matches!(
        db.import_chunk(1, 2, &rows(0..1), REJECT),
        Err(Error::ImportConflict(_))
    ));
    assert_eq!(file_len(&path), before);
    assert!(matches!(db.import_status(1), Err(Error::ImportNotFound)));
    // Exactly the limit succeeds.
    let report = db
        .import_chunk(1, 1, &rows(0..MAX_BATCH_OPERATIONS as u64), REJECT)
        .unwrap();
    assert_eq!(report.rows_committed as usize, MAX_BATCH_OPERATIONS);
}

#[test]
fn open_job_limit_and_job_ids_are_never_reused() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("db.vlite");
    let mut db = v4(&path);
    for job in 1..=64 {
        db.import_chunk(job * 2, 1, &rows(job..job + 1), REJECT)
            .unwrap();
    }
    let before = file_len(&path);
    assert!(matches!(
        db.import_chunk(1000, 1, &rows(0..1), REJECT),
        Err(Error::TooManyImports)
    ));
    assert_eq!(file_len(&path), before);
    db.finish_import(2).unwrap();
    // An unused ID below the highest used ID is refused: closed jobs can be
    // forgotten in memory, so only strictly new IDs are provably unused.
    assert!(matches!(
        db.import_chunk(3, 1, &rows(0..1), REJECT),
        Err(Error::ImportConflict(_))
    ));
    assert_eq!(db.next_import_job_id().unwrap(), 129);
    db.import_chunk(129, 1, &rows(0..1), REJECT).unwrap();
    drop(db);
    let db = Database::open(&path).unwrap();
    assert_eq!(db.next_import_job_id().unwrap(), 130);
}

#[test]
fn finish_and_abort_close_jobs_without_rolling_back_chunks() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("db.vlite");
    let mut db = v4(&path);
    assert!(matches!(db.finish_import(1), Err(Error::ImportNotFound)));
    let last = rows(0..2);
    db.import_chunk(1, 1, &last, REJECT).unwrap();
    let finished = db.finish_import(1).unwrap();
    assert_eq!(finished.state, ImportState::Finished);
    assert_eq!((finished.last_chunk, finished.rows_committed), (1, 2));
    let len = file_len(&path);
    // Repeating the same close is idempotent and writes nothing.
    assert_eq!(db.finish_import(1).unwrap(), finished);
    assert!(matches!(db.abort_import(1), Err(Error::ImportConflict(_))));
    assert!(matches!(
        db.import_chunk(1, 2, &rows(2..3), REJECT),
        Err(Error::ImportConflict(_))
    ));
    // A client retrying its last chunk after finish still sees it committed.
    assert_eq!(
        db.import_chunk(1, 1, &last, REJECT).unwrap().outcome,
        ImportChunkOutcome::AlreadyCommitted
    );
    assert_eq!(file_len(&path), len);

    db.import_chunk(2, 1, &rows(10..13), REJECT).unwrap();
    let aborted = db.abort_import(2).unwrap();
    assert_eq!(aborted.state, ImportState::Aborted);
    assert!(matches!(db.finish_import(2), Err(Error::ImportConflict(_))));
    assert_eq!(db.len(), 5, "abort keeps committed chunks");
    drop(db);
    let db = Database::open(&path).unwrap();
    assert_eq!(db.import_status(1).unwrap().state, ImportState::Finished);
    assert_eq!(db.import_status(2).unwrap().state, ImportState::Aborted);
    assert_eq!(db.len(), 5);
}

#[test]
fn jobs_are_scoped_to_the_collection_of_their_first_chunk() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("db.vlite");
    let mut db = v4_with_docs(&path);
    let docs_rows = vec![put(1, &[1.0, 2.0, 3.0], b"d")];
    let report = db
        .collection_mut("docs")
        .unwrap()
        .import_chunk(9, 1, &docs_rows, REJECT)
        .unwrap();
    assert_eq!(report.outcome, ImportChunkOutcome::Committed);
    assert_eq!(db.import_status(9).unwrap().collection, "docs");
    assert!(db.is_empty());
    assert_eq!(db.collection("docs").unwrap().len(), 1);
    // The same job through another collection conflicts, even as a replay.
    assert!(matches!(
        db.import_chunk(9, 1, &docs_rows, REJECT),
        Err(Error::ImportConflict(_))
    ));
    assert!(matches!(db.finish_import(9), Err(Error::ImportConflict(_))));
    // Dimensions are validated against the job's collection.
    let report = db
        .collection_mut("docs")
        .unwrap()
        .import_chunk(9, 2, &rows(5..6), REJECT)
        .unwrap();
    assert_eq!(report.outcome, ImportChunkOutcome::Rejected);
    // An open job keeps its collection alive even when it has no records.
    db.collection_mut("docs").unwrap().delete(1).unwrap();
    assert!(matches!(
        db.drop_collection("docs"),
        Err(Error::CollectionNotEmpty)
    ));
    let status = db.collection_mut("docs").unwrap().abort_import(9).unwrap();
    assert_eq!(status.state, ImportState::Aborted);
    db.drop_collection("docs").unwrap();
}

#[test]
fn compaction_carries_open_jobs_so_replay_and_resume_still_work() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.vlite");
    let mut db = v4(&source);
    let last = rows(2..4);
    db.import_chunk(1, 1, &rows(0..2), REJECT).unwrap();
    db.import_chunk(1, 2, &last, REJECT).unwrap();
    db.import_chunk(2, 1, &rows(10..11), REJECT).unwrap();
    db.finish_import(2).unwrap();
    db.close().unwrap();
    let output = directory.path().join("compacted.vlite");
    maintenance::compact(&source, &output).unwrap();
    let mut db = Database::open(&output).unwrap();
    assert_eq!(
        db.import_chunk(1, 2, &last, REJECT).unwrap().outcome,
        ImportChunkOutcome::AlreadyCommitted
    );
    assert!(matches!(
        db.import_chunk(1, 2, &rows(4..6), REJECT),
        Err(Error::ImportConflict(_))
    ));
    db.import_chunk(1, 3, &rows(4..6), REJECT).unwrap();
    // Closed jobs are not carried forward by compaction (D8).
    assert!(matches!(db.import_status(2), Err(Error::ImportNotFound)));
    drop(db);
    let db = Database::open(&output).unwrap();
    let status = db.import_status(1).unwrap();
    assert_eq!((status.last_chunk, status.rows_committed), (3, 6));
    assert_eq!(db.len(), 7);
}

#[test]
fn torn_chunk_frame_is_not_committed_and_the_chunk_can_be_resubmitted() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("db.vlite");
    let mut db = v4(&path);
    db.import_chunk(1, 1, &rows(0..2), REJECT).unwrap();
    let durable = file_len(&path);
    let second = rows(2..4);
    db.import_chunk(1, 2, &second, REJECT).unwrap();
    drop(db);
    // Simulate a crash mid-append of chunk 2: only a prefix reached disk.
    OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(durable + 20)
        .unwrap();
    assert!(matches!(
        Database::open(&path),
        Err(Error::IncompleteTail { .. })
    ));
    Database::discard_incomplete_tail(&path).unwrap();
    let mut db = Database::open(&path).unwrap();
    assert_eq!(db.import_status(1).unwrap().last_chunk, 1);
    assert_eq!(db.len(), 2, "no row of the torn chunk is visible");
    assert_eq!(
        db.import_chunk(1, 2, &second, REJECT).unwrap().outcome,
        ImportChunkOutcome::Committed
    );
    assert_eq!(db.len(), 4);
}

#[test]
fn closed_job_status_keeps_its_dropped_collection_identity() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("db.vlite");
    let mut db = v4_with_docs(&path);
    let id_of = |db: &Database, name: &str| {
        db.collections()
            .unwrap()
            .into_iter()
            .find(|info| info.name == name)
            .unwrap()
            .id
    };
    let docs_id = id_of(&db, "docs");
    let last = vec![put(1, &[1.0, 2.0, 3.0], b"d")];
    let mut docs = db.collection_mut("docs").unwrap();
    docs.import_chunk(1, 1, &last, REJECT).unwrap();
    docs.finish_import(1).unwrap();
    docs.delete(1).unwrap();
    db.drop_collection("docs").unwrap();
    let spec = || CollectionSpec {
        dimensions: 3,
        settings: CollectionSettings::default(),
    };
    // A later collection never reuses the dropped collection's ID.
    let other = db.create_collection("other", spec()).unwrap();
    assert_ne!(other.id, docs_id);

    let check = |db: &mut Database| {
        let status = db.import_status(1).unwrap();
        assert_eq!(status.collection, "docs");
        assert!(status.collection_dropped);
        assert_eq!(status.state, ImportState::Finished);
        let mut other = db.collection_mut("other").unwrap();
        // Neither a replay of the job's last chunk nor a close may be
        // attributed to a later collection.
        assert!(matches!(
            other.import_chunk(1, 1, &last, REJECT),
            Err(Error::ImportConflict(_))
        ));
        assert!(matches!(
            other.finish_import(1),
            Err(Error::ImportConflict(_))
        ));
        assert!(other.view().is_empty());
    };
    check(&mut db);
    let len = file_len(&path);
    check(&mut db);
    assert_eq!(file_len(&path), len);
    db.close().unwrap();
    let mut db = Database::open(&path).unwrap();
    check(&mut db);

    // Recreating the same name is a different collection, too.
    db.drop_collection("other").unwrap();
    let recreated = db.create_collection("docs", spec()).unwrap();
    assert!(recreated.id != docs_id && recreated.id != other.id);
    let status = db.import_status(1).unwrap();
    assert_eq!(
        (status.collection.as_str(), status.collection_dropped),
        ("docs", true)
    );
    assert!(matches!(
        db.collection_mut("docs")
            .unwrap()
            .import_chunk(1, 1, &last, REJECT),
        Err(Error::ImportConflict(_))
    ));
    drop(db);
    let db = Database::open(&path).unwrap();
    assert!(db.import_status(1).unwrap().collection_dropped);
}

#[test]
fn live_collection_jobs_report_their_name_and_are_not_marked_dropped() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("db.vlite");
    let mut db = v4_with_docs(&path);
    db.collection_mut("docs")
        .unwrap()
        .import_chunk(1, 1, &[put(1, &[1.0, 2.0, 3.0], b"")], REJECT)
        .unwrap();
    db.import_chunk(2, 1, &rows(0..1), REJECT).unwrap();
    db.finish_import(2).unwrap();
    drop(db);
    let db = Database::open(&path).unwrap();
    let docs = db.import_status(1).unwrap();
    let default = db.import_status(2).unwrap();
    assert_eq!(
        (docs.collection.as_str(), docs.collection_dropped),
        ("docs", false)
    );
    assert_eq!(
        (default.collection.as_str(), default.collection_dropped),
        ("default", false)
    );
}
