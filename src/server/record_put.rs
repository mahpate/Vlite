//! Record puts under the owner lock: the refusal of an expiry that is already
//! due, and the ETag read-back, both judged against the single clock reading
//! of the request.

use super::*;

/// 422 unless `expires_at_ms` is strictly later than `now_ms`.
pub(super) fn ensure_expiry_after(expires_at_ms: u64, now_ms: u64) -> Result<(), ApiError> {
    if expires_at_ms <= now_ms {
        return Err(unprocessable(
            "expires_at_ms must be later than the server clock when the write is applied",
        ));
    }
    Ok(())
}

/// Refuses, before anything is written, a put whose expiry is not later than
/// `now_ms`. The engine accepts such a put and reads the record as absent,
/// which a record route cannot acknowledge with an ETag; the key-value routes
/// accept a due deadline by contract and never call this.
///
/// A later deadline is never judged here: the engine alone answers that put.
/// A due one is refused only once the checks the engine makes ahead of its
/// commit, replayed in the engine's order (`Database::put_with_expiry_in`),
/// have found nothing, so a request with another fault keeps the answer the
/// engine gives it. Keep the two in step; the tests compare them.
#[allow(clippy::too_many_arguments)]
pub(super) fn refuse_due_expiry(
    db: &Database,
    collection: u16,
    id: u64,
    expected: Expected,
    vector: &[f32],
    payload: &[u8],
    metadata: &crate::Metadata,
    expires_at_ms: u64,
    now_ms: u64,
) -> Result<(), ApiError> {
    if expires_at_ms > now_ms {
        return Ok(());
    }
    db.ensure_mutable_in(collection)?;
    if db.format_version() < crate::format::V5 {
        return Err(Error::ExpiryRequiresUpgrade.into());
    }
    let state = db.state(collection);
    crate::validate_stored(state.dimensions, state.precision, vector)?;
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(Error::PayloadTooLarge(payload.len()).into());
    }
    crate::metadata::validate(metadata)?;
    expected.check(db.revision_in(collection, id)?)?;
    if db.is_attached(collection, id) {
        return Err(Error::GraphAttachedRecordExpiry.into());
    }
    if db.is_read_only() {
        return Err(Error::ReadOnly.into());
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
        refuse_due_expiry(
            db,
            collection,
            id,
            expected,
            vector,
            payload,
            metadata,
            expires_at_ms,
            now_ms,
        )?;
        db.put_with_expiry_in(
            collection,
            id,
            vector,
            payload,
            metadata,
            Some(expires_at_ms),
            expected,
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
    use crate::{CreateOptions, FormatVersion, Metadata, MetadataValue, property_graph::GraphOp};

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

    /// Attaches record 1 of the default collection to a node of a new graph.
    fn attach_record_one(db: &mut Database) {
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
    }

    /// A way for a put of record 1 to be refused ahead of its commit.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Fault {
        Poisoned,
        OldFile,
        WrongDimensions,
        OversizedPayload,
        BadMetadata,
        StalePrecondition,
        AttachedRecord,
        ReadOnlyHandle,
    }

    /// The faults in the order in which the engine reports them.
    const ENGINE_ORDER: [Fault; 8] = [
        Fault::Poisoned,
        Fault::OldFile,
        Fault::WrongDimensions,
        Fault::OversizedPayload,
        Fault::BadMetadata,
        Fault::StalePrecondition,
        Fault::AttachedRecord,
        Fault::ReadOnlyHandle,
    ];

    /// A put of record 1 into a one-dimensional database that has every one
    /// of the given faults.
    struct Case {
        db: Database,
        expected: Expected,
        vector: Vec<f32>,
        payload: Vec<u8>,
        metadata: Metadata,
        // Declared last, so the file is closed before its directory goes.
        _directory: Option<tempfile::TempDir>,
    }

    impl Case {
        fn new(faults: &[Fault]) -> Self {
            let has = |fault| faults.contains(&fault);
            let format = if has(Fault::OldFile) {
                FormatVersion::V4
            } else {
                FormatVersion::V5
            };
            // Only a file can be reopened read-only.
            let directory = has(Fault::ReadOnlyHandle).then(|| tempfile::tempdir().unwrap());
            let path = directory
                .as_ref()
                .map(|directory| directory.path().join("case.vlite"));
            let mut db = match &path {
                Some(path) => Database::create_with_format(path, 1, format).unwrap(),
                None => database(format),
            };
            db.put(1, &[0.0], b"kept").unwrap();
            let revision = db.revision(1).unwrap().unwrap();
            if has(Fault::AttachedRecord) {
                attach_record_one(&mut db);
            }
            if let Some(path) = &path {
                db.close().unwrap();
                db = Database::open_read_only(path).unwrap();
            }
            db.poisoned = has(Fault::Poisoned);
            let mut metadata = Metadata::new();
            if has(Fault::BadMetadata) {
                metadata.insert(String::new(), MetadataValue::Boolean(true));
            }
            Self {
                db,
                expected: if has(Fault::StalePrecondition) {
                    Expected::Revision(revision + 1)
                } else {
                    Expected::Any
                },
                vector: if has(Fault::WrongDimensions) {
                    vec![1.0, 2.0]
                } else {
                    vec![1.0]
                },
                payload: if has(Fault::OversizedPayload) {
                    vec![0; MAX_PAYLOAD_BYTES + 1]
                } else {
                    b"payload".to_vec()
                },
                metadata,
                _directory: directory,
            }
        }

        fn put(&mut self, expires_at_ms: Option<u64>) -> Result<Written, ApiError> {
            put_record_at(
                &mut self.db,
                "ident",
                0,
                1,
                self.expected,
                &self.vector,
                &self.payload,
                &self.metadata,
                expires_at_ms,
                NOW,
            )
        }

        fn refuse(&self, expires_at_ms: u64) -> Result<(), ApiError> {
            refuse_due_expiry(
                &self.db,
                0,
                1,
                self.expected,
                &self.vector,
                &self.payload,
                &self.metadata,
                expires_at_ms,
                NOW,
            )
        }
    }

    fn answer<T>(result: &Result<T, ApiError>) -> Option<(StatusCode, &'static str)> {
        result
            .as_ref()
            .err()
            .map(|error| (error.status, error.code))
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
            // One wording for REST and gRPC; gRPC has no ttl field.
            assert!(error.message.contains("later than the server clock"));
            assert!(error.message.contains("when the write is applied"));
            assert!(!error.message.contains("ttl_ms"), "{}", error.message);
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
        attach_record_one(&mut db);
        for expires in [0, NOW + 1] {
            let error = refusal(&mut db, Expected::Any, Some(expires));
            assert_refusal(&error, StatusCode::CONFLICT, "graph_attached_record_expiry");
        }
    }

    #[test]
    fn an_attached_record_is_judged_after_the_precondition_like_the_engine_does() {
        for expires in [0, NOW, NOW + 1] {
            let mut case = Case::new(&[Fault::AttachedRecord, Fault::StalePrecondition]);
            let result = case.put(Some(expires));
            let error = result.err().expect("the put must be refused");
            assert_refusal(
                &error,
                StatusCode::PRECONDITION_FAILED,
                "precondition_failed",
            );
        }
        // With a satisfied precondition the attachment is what refuses.
        let mut case = Case::new(&[Fault::AttachedRecord]);
        for expires in [0, NOW, NOW + 1] {
            let result = case.put(Some(expires));
            let error = result.err().expect("the put must be refused");
            assert_refusal(&error, StatusCode::CONFLICT, "graph_attached_record_expiry");
        }
    }

    #[test]
    fn a_later_deadline_is_never_judged_here() {
        // Whatever is wrong with the put, the engine alone answers a put that
        // is not due.
        let case = Case::new(&[
            Fault::Poisoned,
            Fault::OldFile,
            Fault::WrongDimensions,
            Fault::OversizedPayload,
            Fault::BadMetadata,
            Fault::StalePrecondition,
        ]);
        case.refuse(NOW + 1).unwrap();
        case.refuse(u64::MAX).unwrap();
        assert!(case.refuse(NOW).is_err());
    }

    #[test]
    fn an_unusable_handle_is_refused_before_anything_else_is_looked_at() {
        let case = Case::new(&[Fault::Poisoned, Fault::OldFile]);
        let error = case.refuse(NOW).unwrap_err();
        assert_refusal(&error, StatusCode::SERVICE_UNAVAILABLE, "poisoned");
    }

    #[test]
    fn a_due_put_is_refused_exactly_like_the_same_put_with_a_later_deadline() {
        let read_only = answer(&Case::new(&[Fault::ReadOnlyHandle]).put(Some(NOW + 1)));
        assert_ne!(
            read_only,
            Some((StatusCode::UNPROCESSABLE_ENTITY, "invalid_input"))
        );
        let engine_answer = |fault| match fault {
            Fault::Poisoned => (StatusCode::SERVICE_UNAVAILABLE, "poisoned"),
            Fault::OldFile => (StatusCode::CONFLICT, "upgrade_required"),
            Fault::WrongDimensions | Fault::OversizedPayload | Fault::BadMetadata => {
                (StatusCode::UNPROCESSABLE_ENTITY, "invalid_input")
            }
            Fault::StalePrecondition => (StatusCode::PRECONDITION_FAILED, "precondition_failed"),
            Fault::AttachedRecord => (StatusCode::CONFLICT, "graph_attached_record_expiry"),
            Fault::ReadOnlyHandle => read_only.expect("a read-only handle refuses the put"),
        };

        let mut states = 0;
        for subset in 0..(1_u32 << ENGINE_ORDER.len()) {
            let faults: Vec<Fault> = ENGINE_ORDER
                .into_iter()
                .enumerate()
                .filter(|&(bit, _)| (subset >> bit) & 1 == 1)
                .map(|(_, fault)| fault)
                .collect();
            let has = |fault| faults.contains(&fault);
            // A v4 file holds no graph. A read-only case needs a file, so it
            // is tried alone and next to each other fault.
            if (has(Fault::OldFile) && has(Fault::AttachedRecord))
                || (has(Fault::ReadOnlyHandle) && faults.len() > 2)
            {
                continue;
            }
            states += 1;
            let later = Case::new(&faults).put(Some(NOW + 1));
            let due = Case::new(&faults).put(Some(NOW));
            let Some(&first) = faults.first() else {
                assert!(later.is_ok());
                let error = due.err().expect("a due deadline must be refused");
                assert_refusal(&error, StatusCode::UNPROCESSABLE_ENTITY, "invalid_input");
                assert!(error.message.contains("later than the server clock"));
                continue;
            };
            let expected = Some(engine_answer(first));
            assert_eq!(answer(&later), expected, "{faults:?}: later deadline");
            assert_eq!(answer(&due), expected, "{faults:?}: due deadline");
            let error = due.err().unwrap();
            assert!(
                !error.message.contains("later than the server clock"),
                "{faults:?}: {}",
                error.message
            );
        }
        assert_eq!(states, 96 + 8);
    }

    #[test]
    fn a_read_only_handle_gives_every_deadline_the_answer_of_a_put_without_one() {
        let mut case = Case::new(&[Fault::ReadOnlyHandle]);
        let plain = case.put(None);
        let later = case.put(Some(NOW + 1));
        let due = case.put(Some(0));
        assert!(plain.is_err());
        assert_ne!(
            answer(&due),
            Some((StatusCode::UNPROCESSABLE_ENTITY, "invalid_input"))
        );
        assert_eq!(answer(&due), answer(&later));
        assert_eq!(answer(&due), answer(&plain));
        assert_eq!(case.db.get(1).unwrap().unwrap().payload, b"kept");
    }

    #[test]
    fn a_read_only_handle_judges_a_failed_precondition_before_its_mode() {
        let mut case = Case::new(&[Fault::ReadOnlyHandle, Fault::StalePrecondition]);
        for expires in [None, Some(NOW + 1), Some(0)] {
            let result = case.put(expires);
            let error = result.err().expect("the put must be refused");
            assert_refusal(
                &error,
                StatusCode::PRECONDITION_FAILED,
                "precondition_failed",
            );
        }
    }
}
