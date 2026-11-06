use std::path::Path;

use super::*;
use crate::Database;
use crate::memory::{MemoryItem, MemoryKind, MemoryMut, RememberRequest};

fn scope() -> MemoryScope {
    MemoryScope {
        tenant: "@root".into(),
        agent: "a".into(),
        user: None,
        session: None,
    }
}

fn wire(status: u16, code: &'static str) -> WireMapping {
    WireMapping::Code(WireCode {
        status,
        code,
        feature: None,
    })
}

#[test]
fn first_slice_serves_five_operations_and_refuses_four_explicitly() {
    use WireOperation::*;
    for served in [Remember, Recall, Consolidate, Forget, GetReceipt] {
        assert_eq!(served.refusal(), None, "{served:?}");
    }
    for (refused, feature) in [
        (FindFacts, "facts"),
        (CorrectFacts, "facts"),
        (GetErasure, "erasure_jobs"),
        (ResumeErasure, "erasure_jobs"),
    ] {
        assert_eq!(
            refused.refusal(),
            Some(WireCode {
                status: 409,
                code: "memory_capability_unavailable",
                feature: Some(feature),
            }),
            "{refused:?}"
        );
    }
}

#[test]
fn receipt_operations_map_from_wire_names() {
    assert_eq!(
        ReceiptOperation::from_wire("remember"),
        Err(unavailable("remember_receipt_lookup"))
    );
    assert_eq!(
        ReceiptOperation::from_wire("forget"),
        Ok(ReceiptOperation::Forget)
    );
    assert_eq!(
        ReceiptOperation::from_wire("consolidate"),
        Ok(ReceiptOperation::Consolidate)
    );
    assert_eq!(
        ReceiptOperation::from_wire("correct_facts"),
        Err(unavailable("facts"))
    );
    assert_eq!(
        ReceiptOperation::from_wire("resume_erasure"),
        Err(unavailable("erasure_jobs"))
    );
    for unknown in ["", "Remember", "recall", "remember ", "get_receipt"] {
        assert_eq!(
            ReceiptOperation::from_wire(unknown),
            Err(WireCode {
                status: 422,
                code: "invalid_input",
                feature: None,
            }),
            "{unknown:?}"
        );
    }
}

#[test]
fn every_memory_error_has_its_pinned_wire_code() {
    use MemoryError::*;
    let invalid = wire(422, "invalid_input");
    let internal = wire(500, "internal_error");
    let cases = [
        (InvalidScope, wire(409, "memory_not_configured")),
        (InvalidKey, invalid),
        (InvalidClock, internal),
        (InvalidReceiptWindow, internal),
        (InvalidExpiry, invalid),
        (InvalidEvidence, invalid),
        (DeadlineExceeded, wire(504, "deadline_exceeded")),
        (InvalidBundle, internal),
        (DuplicateItem, invalid),
        (OperationLimit, invalid),
        (BodyLimit, invalid),
        (InvalidIdentity, invalid),
        (
            UnsupportedKind,
            WireMapping::Code(unavailable("fact_kinds")),
        ),
        (InvalidContent, invalid),
        (InvalidImportance, invalid),
        (InvalidObservation, invalid),
        (FutureObservation, invalid),
        (InvalidProvenance, invalid),
        (ProvenanceLimit, invalid),
        (InvalidReference, invalid),
        (MissingReference, wire(404, "memory_not_found")),
        (CrossScope, invalid),
        (DigestCollision, internal),
        (IdempotencyConflict, wire(409, "idempotency_conflict")),
        (IdentityConflict, wire(412, "precondition_failed")),
        (MissingMemory, wire(404, "memory_not_found")),
        (MergeIdentityConflict, wire(412, "precondition_failed")),
        (ExpectedRevisionRequired, invalid),
        (RevisionConflict, wire(412, "precondition_failed")),
        (
            RecordAlreadyAttached,
            wire(409, "memory_attachment_conflict"),
        ),
        (ForeignAttachment, wire(409, "memory_attachment_conflict")),
        (InvalidTarget, invalid),
        (DuplicateTarget, invalid),
        (DeletionPending, wire(409, "memory_deletion_pending")),
        (JobConflict, wire(412, "precondition_failed")),
        (
            BindingGeneration,
            WireMapping::Code(unavailable("binding_generation")),
        ),
        (Storage(crate::Error::InvalidQuery("storage")), internal),
        (Storage(crate::Error::GraphNotFound), WireMapping::Delegate),
        (Storage(crate::Error::ReadOnly), WireMapping::Delegate),
    ];
    for (error, expected) in cases {
        assert_eq!(memory_error(&error), expected, "{error:?}");
    }
}

#[test]
fn every_recall_and_ranking_error_has_its_pinned_wire_code() {
    use RankingError::*;
    let invalid = wire(422, "invalid_input");
    let internal = wire(500, "internal_error");
    for request in [
        Weights,
        HalfLifeRequired,
        HalfLife,
        Limit,
        AgeOverflow,
        FutureObservation,
    ] {
        assert_eq!(ranking_error(request), invalid, "{request:?}");
        assert_eq!(
            recall_error(&RecallError::Ranking(request)),
            invalid,
            "{request:?}"
        );
    }
    for fault in [
        CandidateCount,
        Scope,
        Importance,
        Rank,
        DuplicateRank,
        DuplicateIdentity,
        DenseRanks,
        SegmentBounds,
        NonFiniteScore,
    ] {
        assert_eq!(ranking_error(fault), internal, "{fault:?}");
        assert_eq!(
            recall_error(&RecallError::Ranking(fault)),
            internal,
            "{fault:?}"
        );
    }
    let cases = [
        (
            RecallError::Memory(MemoryError::MissingMemory),
            wire(404, "memory_not_found"),
        ),
        (
            RecallError::Memory(MemoryError::Storage(crate::Error::InvalidQuery("s"))),
            internal,
        ),
        (
            RecallError::Storage(crate::Error::InvalidQuery("storage")),
            internal,
        ),
        (
            RecallError::Storage(crate::Error::GraphNotFound),
            WireMapping::Delegate,
        ),
        (RecallError::InvalidLimits, invalid),
        (RecallError::InvalidScopes, invalid),
        (RecallError::InvalidQuery, invalid),
        (
            RecallError::UnsupportedPlan,
            wire(422, "memory_bounded_plan_unsupported"),
        ),
        (RecallError::Budget("graph"), invalid),
        (RecallError::Cancelled, WireMapping::Cancelled),
        (RecallError::Deadline, wire(504, "deadline_exceeded")),
    ];
    for (error, expected) in cases {
        assert_eq!(recall_error(&error), expected, "{error:?}");
    }
}

#[test]
fn receipt_id_is_opaque_and_separates_every_input() {
    let base = receipt_id(ReceiptOperation::Remember, &scope(), "k1", 7).unwrap();
    assert!(base.starts_with("rcpt:v1:"));
    assert_eq!(base.len(), 8 + 64);
    assert!(
        base[8..]
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    );
    assert_eq!(
        receipt_id(ReceiptOperation::Remember, &scope(), "k1", 7).unwrap(),
        base
    );
    let mut other_scope = scope();
    other_scope.agent = "b".into();
    for other in [
        receipt_id(ReceiptOperation::Forget, &scope(), "k1", 7),
        receipt_id(ReceiptOperation::Consolidate, &scope(), "k1", 7),
        receipt_id(ReceiptOperation::BeginErasure, &scope(), "k1", 7),
        receipt_id(ReceiptOperation::ResumeErasure, &scope(), "k1", 7),
        receipt_id(ReceiptOperation::Remember, &other_scope, "k1", 7),
        receipt_id(ReceiptOperation::Remember, &scope(), "k2", 7),
        receipt_id(ReceiptOperation::Remember, &scope(), "k1", 8),
    ] {
        assert_ne!(other.unwrap(), base);
    }
    // The two erasure namespaces never share an ID with each other or with
    // the forget namespace a wire `forget` also names (ES6, ES7).
    let ids: std::collections::BTreeSet<String> = [
        ReceiptOperation::Forget,
        ReceiptOperation::BeginErasure,
        ReceiptOperation::ResumeErasure,
    ]
    .into_iter()
    .map(|operation| receipt_id(operation, &scope(), "k1", 7).unwrap())
    .collect();
    assert_eq!(ids.len(), 3);
    assert_eq!(ReceiptOperation::BeginErasure.name(), "begin-erasure");
    assert_eq!(ReceiptOperation::ResumeErasure.name(), "resume-erasure");
    // Pinned bytes: domain, u32-BE-prefixed receipt node key, u64-BE revision.
    let key = operation_receipt_key("remember", &scope().key(), "k1");
    let mut input = b"vlite.memory.receipt.id.v1\0".to_vec();
    input.extend_from_slice(&(key.len() as u32).to_be_bytes());
    input.extend_from_slice(key.as_bytes());
    input.extend_from_slice(&7u64.to_be_bytes());
    assert_eq!(base, format!("rcpt:v1:{}", hex(&Sha256::digest(input))));
}

#[test]
fn receipt_id_rejects_invalid_inputs() {
    assert!(matches!(
        receipt_id(ReceiptOperation::Remember, &scope(), "k1", 0),
        Err(MemoryError::InvalidBundle)
    ));
    assert!(matches!(
        receipt_id(ReceiptOperation::Remember, &scope(), "", 1),
        Err(MemoryError::InvalidKey)
    ));
    let mut bad = scope();
    bad.session = Some("s".into());
    assert!(matches!(
        receipt_id(ReceiptOperation::Remember, &bad, "k1", 1),
        Err(MemoryError::InvalidScope)
    ));
}

fn database(path: &Path) -> Database {
    let mut db = Database::create(path, 2).unwrap();
    db.create_graph("memory").unwrap();
    MemoryMut::bind(&mut db, "memory", None)
        .unwrap()
        .provision_scope(&scope())
        .unwrap();
    db
}

fn request(now_ms: u64) -> RememberRequest {
    RememberRequest {
        scope: scope(),
        idempotency_key: "k1".into(),
        receipt_window_ms: 100,
        now_ms,
        deadline_ms: None,
        allow_future_observations: false,
        items: vec![MemoryItem {
            id: "e1".into(),
            kind: MemoryKind::Entity,
            content: "Ada".into(),
            source: "s1".into(),
            importance: None,
            observed_at_ms: Some(10),
            provenance: Vec::new(),
            references: Vec::new(),
            merge_into: None,
            expected_revision: None,
            expiry_ms: None,
            expiry_valid: true,
            evidence: Vec::new(),
        }],
    }
}

fn remembered_id(db: &mut Database, request: &RememberRequest) -> (String, bool) {
    let result = MemoryMut::bind(db, "memory", None)
        .unwrap()
        .remember(request)
        .unwrap();
    let id = receipt_id(
        ReceiptOperation::Remember,
        &request.scope,
        &request.idempotency_key,
        result.receipt_revision,
    )
    .unwrap();
    (id, result.replayed)
}

#[test]
fn receipt_id_is_stable_on_replay_and_reopen_and_changes_after_expiry() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mapping.vlite");
    let mut db = database(&path);
    let (first, replayed) = remembered_id(&mut db, &request(10));
    assert!(!replayed);
    let (replay, replayed) = remembered_id(&mut db, &request(10));
    assert!(replayed);
    assert_eq!(replay, first);
    db.close().unwrap();
    let mut db = Database::open(&path).unwrap();
    let (reopened, replayed) = remembered_id(&mut db, &request(10));
    assert!(replayed);
    assert_eq!(reopened, first);
    // The window is 100 ms from clock 10: at 110 the key executes again and
    // rewrites the receipt node, so it is a different receipt.
    let (again, replayed) = remembered_id(&mut db, &request(110));
    assert!(!replayed);
    assert_ne!(again, first);
}
