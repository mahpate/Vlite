//! Record puts under the owner lock: the refusal of an expiry that is already
//! due, and the ETag read-back, both judged against the single clock reading
//! of the request.

use super::*;

/// 422 unless `expires_at_ms` is strictly later than `now_ms`.
pub(super) fn ensure_expiry_after(expires_at_ms: u64, now_ms: u64) -> Result<(), ApiError> {
    if expires_at_ms <= now_ms {
        return Err(unprocessable(
            "expires_at_ms must be later than the server clock; ttl_ms must be at least 1",
        ));
    }
    Ok(())
}

/// Refuses, before anything is written, a put whose expiry is not later than
/// `now_ms`. The engine accepts such a put and reads the record as absent,
/// which a record route cannot acknowledge with an ETag; the key-value routes
/// accept a due deadline by contract and never call this. What the engine
/// refuses ahead of the deadline keeps its answer: an unusable handle, a file
/// older than v5, a read-only handle, a failed precondition and a record
/// attached to a graph. Vector, payload and metadata validation stays with the
/// engine and also answers 422 `invalid_input`.
pub(super) fn refuse_due_expiry(
    db: &Database,
    collection: u16,
    id: u64,
    expected: Expected,
    expires_at_ms: u64,
    now_ms: u64,
) -> Result<(), ApiError> {
    db.ensure_mutable_in(collection)?;
    if db.format_version() < crate::format::V5 {
        return Err(Error::ExpiryRequiresUpgrade.into());
    }
    if db.is_read_only() {
        return Err(Error::ReadOnly.into());
    }
    expected.check(db.revision_in(collection, id)?)?;
    if db.is_attached(collection, id) {
        return Err(Error::GraphAttachedRecordExpiry.into());
    }
    ensure_expiry_after(expires_at_ms, now_ms)
}

/// Puts one record and reads its ETag back. `now_ms` is the request's only
/// clock reading: it judges the expiry and also dates the read-back, so a
/// short lifetime cannot lapse between the commit and the read-back.
#[allow(clippy::too_many_arguments)]
pub(super) fn put_record_at(
    db: &mut Database,
    ident: &str,
    collection: u16,
    id: u64,
    expected: Expected,
    vector: &[f32],
    payload: &[u8],
    metadata: &crate::Metadata,
    expires_at_ms: Option<u64>,
    now_ms: u64,
) -> Result<Written, ApiError> {
    let created = db.revision_in(collection, id)?.is_none();
    if let Some(expires_at_ms) = expires_at_ms {
        refuse_due_expiry(db, collection, id, expected, expires_at_ms, now_ms)?;
        db.collection_mut_in(collection)?.put_with_expiry(
            id,
            vector,
            payload,
            metadata,
            Some(expires_at_ms),
        )?;
    } else {
        db.put_in(collection, id, vector, payload, metadata, expected)?;
    }
    let (etag, revision) = current_etag_at(db, ident, collection, id, now_ms)?
        .ok_or_else(|| internal("acknowledged put is not readable under the same lock"))?;
    Ok(Written {
        revision,
        created,
        etag,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CreateOptions, FormatVersion, Metadata, property_graph::GraphOp};

    const NOW: u64 = 1_000;

    fn database(format: FormatVersion) -> Database {
        Database::create_in_memory(
            1,
            CreateOptions {
                format: Some(format),
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn put(
        db: &mut Database,
        expected: Expected,
        expires: Option<u64>,
    ) -> Result<Written, ApiError> {
        put_record_at(
            db,
            "ident",
            0,
            1,
            expected,
            &[1.0],
            b"payload",
            &Metadata::new(),
            expires,
            NOW,
        )
    }

    #[track_caller]
    fn refusal(db: &mut Database, expected: Expected, expires: Option<u64>) -> ApiError {
        let Err(error) = put(db, expected, expires) else {
            panic!("the put must be refused");
        };
        error
    }

    #[track_caller]
    fn assert_refusal(error: &ApiError, status: StatusCode, code: &str) {
        assert_eq!((error.status, error.code), (status, code), "{error:?}");
    }

    #[test]
    fn an_expiry_must_be_strictly_later_than_the_clock() {
        for (expires, now) in [
            (0, 0),
            (0, 1),
            (999, 1_000),
            (1_000, 1_000),
            (u64::MAX, u64::MAX),
        ] {
            let error = ensure_expiry_after(expires, now).unwrap_err();
            assert_refusal(&error, StatusCode::UNPROCESSABLE_ENTITY, "invalid_input");
            assert!(error.message.contains("later than the server clock"));
        }
        for (expires, now) in [
            (1, 0),
            (1_001, 1_000),
            (u64::MAX, u64::MAX - 1),
            (u64::MAX, 0),
        ] {
            ensure_expiry_after(expires, now).unwrap();
        }
    }

    #[test]
    fn a_deadline_just_past_the_clock_is_stored_and_read_back_at_that_clock() {
        let mut db = database(FormatVersion::V5);
        let written = put(&mut db, Expected::Any, Some(NOW + 1)).unwrap();
        // The wall clock is long past the deadline, so a read-back that asked
        // it would find nothing and fail an acknowledged write.
        assert!(db.get(1).unwrap().is_none());
        assert_eq!(db.revision(1).unwrap(), Some(written.revision));
        assert_eq!(db.expires_at(1).unwrap(), Some(NOW + 1));
        assert!(written.created);
        assert!(
            written.etag.starts_with("\"r2-ident-0-1-"),
            "{}",
            written.etag
        );
    }

    #[test]
    fn a_deadline_at_or_before_the_clock_is_refused_and_nothing_is_written() {
        let mut db = database(FormatVersion::V5);
        db.put(1, &[0.0], b"kept").unwrap();
        let revision = db.revision(1).unwrap();
        for expires in [0, 1, NOW - 1, NOW] {
            let error = refusal(&mut db, Expected::Any, Some(expires));
            assert_refusal(&error, StatusCode::UNPROCESSABLE_ENTITY, "invalid_input");
        }
        assert_eq!(db.revision(1).unwrap(), revision);
        assert_eq!(db.get(1).unwrap().unwrap().payload, b"kept");
    }

    #[test]
    fn a_put_without_an_expiry_reads_back_at_any_clock() {
        let mut db = database(FormatVersion::V5);
        let written = put(&mut db, Expected::Any, None).unwrap();
        assert_eq!(db.revision(1).unwrap(), Some(written.revision));
        assert!(!written.etag.is_empty());
    }

    #[test]
    fn what_the_engine_refuses_ahead_of_the_deadline_keeps_its_answer() {
        // A file older than format v5 cannot hold any expiry.
        let mut old = database(FormatVersion::V4);
        for expires in [0, NOW, NOW + 1] {
            let error = refusal(&mut old, Expected::Any, Some(expires));
            assert_refusal(&error, StatusCode::CONFLICT, "upgrade_required");
        }
        assert_eq!(old.revision(1).unwrap(), None);

        // A failed precondition outranks the deadline; a satisfied one does not.
        let mut db = database(FormatVersion::V5);
        db.put(1, &[0.0], b"kept").unwrap();
        let revision = db.revision(1).unwrap().unwrap();
        for expected in [Expected::Revision(revision + 1), Expected::Absent] {
            let error = refusal(&mut db, expected, Some(0));
            assert_refusal(
                &error,
                StatusCode::PRECONDITION_FAILED,
                "precondition_failed",
            );
        }
        let error = refusal(&mut db, Expected::Revision(revision), Some(0));
        assert_refusal(&error, StatusCode::UNPROCESSABLE_ENTITY, "invalid_input");
        assert_eq!(db.revision(1).unwrap(), Some(revision));

        // An attached record keeps the engine's own refusal of any expiry.
        db.create_graph("g").unwrap();
        db.graph_mut("g")
            .unwrap()
            .write_batch(&[
                GraphOp::CreateNode {
                    key: "n1".into(),
                    labels: vec!["Doc".into()],
                    properties: Metadata::new(),
                },
                GraphOp::Attach {
                    node: "n1".into(),
                    collection: "default".into(),
                    record: 1,
                },
            ])
            .unwrap();
        for expires in [0, NOW + 1] {
            let error = refusal(&mut db, Expected::Any, Some(expires));
            assert_refusal(&error, StatusCode::CONFLICT, "graph_attached_record_expiry");
        }
    }

    #[test]
    fn a_read_only_handle_refuses_before_the_deadline_is_judged() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("read-only.vlite");
        let mut db = Database::create(&path, 1).unwrap();
        db.put(1, &[0.0], b"kept").unwrap();
        db.close().unwrap();
        let mut db = Database::open_read_only(&path).unwrap();
        let due = refusal(&mut db, Expected::Any, Some(0));
        let later = refusal(&mut db, Expected::Any, Some(NOW + 1));
        let plain = refusal(&mut db, Expected::Any, None);
        assert_ne!(due.code, "invalid_input");
        assert_eq!((due.status, due.code), (later.status, later.code));
        assert_eq!((due.status, due.code), (plain.status, plain.code));
        assert_eq!(db.get(1).unwrap().unwrap().payload, b"kept");
    }
}
