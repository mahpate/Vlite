use std::path::Path;

use super::super::consolidate::{ConsolidateInput, ConsolidateRequest, ConsolidateSummary};
use super::super::erasure::EraseScopeRequest;
use super::super::{
    ForgetRequest, ForgetTarget, MemoryItem, MemoryKind, MemoryMut, RememberRequest,
    operation_receipt_key, receipt_outcome_key,
};
use super::*;
use crate::{Database, Expected, Metadata, property_graph::GraphOp};

const GRAPH: &str = "memory";

fn scope(agent: &str) -> MemoryScope {
    MemoryScope {
        tenant: "@root".into(),
        agent: agent.into(),
        user: None,
        session: None,
    }
}

fn database(path: &Path) -> Database {
    let mut db = Database::create(path, 2).unwrap();
    db.create_graph(GRAPH).unwrap();
    for agent in ["a", "b"] {
        MemoryMut::bind(&mut db, GRAPH, None)
            .unwrap()
            .provision_scope(&scope(agent))
            .unwrap();
    }
    db
}

fn remember(db: &mut Database, ids: &[&str], now_ms: u64) {
    let items = ids
        .iter()
        .map(|id| MemoryItem {
            id: (*id).into(),
            kind: MemoryKind::Entity,
            content: format!("content {id}"),
            source: "s1".into(),
            importance: None,
            observed_at_ms: Some(now_ms as i64),
            provenance: Vec::new(),
            references: Vec::new(),
            merge_into: None,
            expected_revision: None,
            expiry_ms: None,
            expiry_valid: true,
            evidence: Vec::new(),
        })
        .collect();
    MemoryMut::bind(db, GRAPH, None)
        .unwrap()
        .remember(&RememberRequest {
            scope: scope("a"),
            idempotency_key: format!("r-{}", ids.join("-")),
            receipt_window_ms: 100,
            now_ms,
            deadline_ms: None,
            allow_future_observations: false,
            items,
        })
        .unwrap();
}

/// Remembers `e2`, `e1` and forgets them (input order `e2`, `e1`) under key
/// `f1` at clock 20 with a 100 ms window; returns the forget receipt revision.
fn forgotten(db: &mut Database) -> u64 {
    remember(db, &["e2", "e1"], 10);
    let result = MemoryMut::bind(db, GRAPH, None)
        .unwrap()
        .forget(&ForgetRequest {
            scope: scope("a"),
            idempotency_key: "f1".into(),
            receipt_window_ms: 100,
            now_ms: 20,
            deadline_ms: None,
            targets: ["e2", "e1"]
                .into_iter()
                .map(|id| ForgetTarget {
                    id: id.into(),
                    expected_revision: 1,
                })
                .collect(),
        })
        .unwrap();
    assert!(!result.replayed);
    result.receipt_revision
}

fn lookup(
    db: &Database,
    operation: ReceiptOperation,
    agent: &str,
    key: &str,
    now_ms: u64,
) -> MemoryResult<Option<ReceiptLookup>> {
    MemoryRef::bind(db, GRAPH, None)
        .unwrap()
        .receipt(operation, &scope(agent), key, now_ms)
}

fn forget_lookup(revision: u64) -> Option<ReceiptLookup> {
    Some(ReceiptLookup {
        outcome: ReceiptOutcome::Forget {
            deleted: vec!["e1".into(), "e2".into()],
        },
        receipt_revision: revision,
        retry_until_ms: 120,
    })
}

fn error_name(result: MemoryResult<Option<ReceiptLookup>>) -> String {
    match result {
        Err(MemoryError::Storage(_)) => "Storage".into(),
        Err(error) => format!("{error:?}"),
        Ok(found) => format!("Ok({found:?})"),
    }
}

fn node(db: &Database, key: &str) -> NodeView {
    db.graph(GRAPH)
        .unwrap()
        .get_node(key)
        .unwrap()
        .output
        .unwrap()
}

fn rewrite(db: &mut Database, key: &str, edit: impl FnOnce(&mut Metadata)) {
    let node = node(db, key);
    let mut properties = node.properties.clone();
    edit(&mut properties);
    db.graph_mut(GRAPH)
        .unwrap()
        .write_batch(&[GraphOp::UpsertNode {
            key: node.key,
            labels: node.labels.into_iter().collect(),
            properties,
            expected: Expected::Revision(node.revision),
        }])
        .unwrap();
}

fn forget_key() -> String {
    operation_receipt_key("forget", &scope("a").key(), "f1")
}

#[test]
fn forget_receipt_returns_stored_outcome_until_expiry_and_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("receipt.vlite");
    let mut db = database(&path);
    let revision = forgotten(&mut db);
    for now in [20, 21, 119] {
        assert_eq!(
            lookup(&db, ReceiptOperation::Forget, "a", "f1", now).unwrap(),
            forget_lookup(revision),
            "clock {now}"
        );
    }
    // Expiry equality: at retry_until_ms the receipt is not found.
    assert_eq!(
        lookup(&db, ReceiptOperation::Forget, "a", "f1", 120).unwrap(),
        None
    );
    assert_eq!(
        lookup(&db, ReceiptOperation::Forget, "a", "f1", u64::MAX >> 1).unwrap(),
        None
    );
    db.close().unwrap();
    let db = Database::open(&path).unwrap();
    assert_eq!(
        lookup(&db, ReceiptOperation::Forget, "a", "f1", 119).unwrap(),
        forget_lookup(revision)
    );
}

#[test]
fn lookup_identity_is_operation_scope_and_key() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(&dir.path().join("receipt.vlite"));
    forgotten(&mut db);
    assert!(
        lookup(&db, ReceiptOperation::Forget, "a", "f1", 50)
            .unwrap()
            .is_some()
    );
    for (operation, agent, key) in [
        (ReceiptOperation::Forget, "a", "f2"),
        (ReceiptOperation::Forget, "b", "f1"),
        (ReceiptOperation::Consolidate, "a", "f1"),
    ] {
        assert_eq!(
            lookup(&db, operation, agent, key, 50).unwrap(),
            None,
            "{operation:?} {agent} {key}"
        );
    }
    // The remember receipt for the seeded items exists, but remember lookup
    // is never served by this accessor.
    assert_eq!(
        error_name(lookup(&db, ReceiptOperation::Remember, "a", "r-e2-e1", 50)),
        "InvalidBundle"
    );
}

#[test]
fn validation_precedes_reads_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let db = database(&dir.path().join("receipt.vlite"));
    let bound = MemoryRef::bind(&db, GRAPH, None).unwrap();
    let mut invalid = scope("a");
    invalid.session = Some("s".into());
    let forget = ReceiptOperation::Forget;
    let name = |result| error_name(result);
    assert_eq!(
        name(bound.receipt(forget, &invalid, "", u64::MAX)),
        "InvalidScope"
    );
    assert_eq!(
        name(bound.receipt(forget, &scope("a"), "", u64::MAX)),
        "InvalidKey"
    );
    let long = "k".repeat(257);
    assert_eq!(
        name(bound.receipt(forget, &scope("a"), &long, 1)),
        "InvalidKey"
    );
    assert!(
        bound
            .receipt(forget, &scope("a"), &"k".repeat(256), 1)
            .is_ok()
    );
    assert_eq!(
        name(bound.receipt(forget, &scope("a"), "k", i64::MAX as u64 + 1)),
        "InvalidClock"
    );
    assert_eq!(
        bound
            .receipt(forget, &scope("a"), "k", i64::MAX as u64)
            .unwrap(),
        None
    );
    // An unprovisioned scope is not configured, never a silent not-found.
    assert_eq!(
        name(bound.receipt(forget, &scope("c"), "k", 1)),
        "InvalidScope"
    );
}

#[test]
fn wrongly_seeded_scope_tuple_is_a_digest_collision() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(&dir.path().join("receipt.vlite"));
    let tuple = hex(&scope("b").encode());
    rewrite(&mut db, &scope("a").key(), |properties| {
        properties.insert("tuple".into(), MetadataValue::String(tuple));
    });
    assert_eq!(
        error_name(lookup(&db, ReceiptOperation::Forget, "a", "f1", 1)),
        "DigestCollision"
    );
}

#[test]
fn tampered_forget_receipts_are_faults_live_or_expired() {
    type Edit = fn(&mut Metadata);
    let cases: [(&str, Edit, &str); 9] = [
        (
            "extra property",
            |p| {
                p.insert("extra".into(), MetadataValue::Integer(1));
            },
            "InvalidBundle",
        ),
        (
            "missing digest",
            |p| {
                p.remove("digest");
            },
            "InvalidBundle",
        ),
        (
            "foreign scope",
            |p| {
                p.insert("scope".into(), MetadataValue::String(scope("b").key()));
            },
            "DigestCollision",
        ),
        (
            "integer digest",
            |p| {
                p.insert("digest".into(), MetadataValue::Integer(1));
            },
            "InvalidBundle",
        ),
        (
            "digest shape",
            |p| {
                p.insert("digest".into(), MetadataValue::String("v1:00".into()));
            },
            "InvalidBundle",
        ),
        (
            "leading zero expiry",
            |p| {
                p.insert("expires_at_ms".into(), MetadataValue::String("0120".into()));
            },
            "InvalidBundle",
        ),
        (
            "integer expiry",
            |p| {
                p.insert("expires_at_ms".into(), MetadataValue::Integer(120));
            },
            "InvalidBundle",
        ),
        (
            "string chunk count",
            |p| {
                p.insert("outcome_chunks".into(), MetadataValue::String("1".into()));
            },
            "InvalidBundle",
        ),
        (
            "zero chunks",
            |p| {
                p.insert("outcome_chunks".into(), MetadataValue::Integer(0));
            },
            "InvalidBundle",
        ),
    ];
    for (label, edit, expected) in cases {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("receipt.vlite"));
        forgotten(&mut db);
        rewrite(&mut db, &forget_key(), edit);
        for now in [50, 500] {
            assert_eq!(
                error_name(lookup(&db, ReceiptOperation::Forget, "a", "f1", now)),
                expected,
                "{label} at {now}"
            );
        }
    }
}

#[test]
fn an_extra_receipt_label_is_a_fault() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(&dir.path().join("receipt.vlite"));
    forgotten(&mut db);
    let node = node(&db, &forget_key());
    let mut labels: Vec<String> = node.labels.into_iter().collect();
    labels.push("Extra".into());
    db.graph_mut(GRAPH)
        .unwrap()
        .write_batch(&[GraphOp::UpsertNode {
            key: node.key,
            labels,
            properties: node.properties,
            expected: Expected::Revision(node.revision),
        }])
        .unwrap();
    assert_eq!(
        error_name(lookup(&db, ReceiptOperation::Forget, "a", "f1", 50)),
        "InvalidBundle"
    );
}

#[test]
fn tampered_forget_outcomes_are_faults() {
    let id = |value: &str| hex(value.as_bytes());
    let cases = [
        ("missing prefix", format!("{};{}", id("e1"), id("e2"))),
        ("empty list", "deleted:".into()),
        ("empty segment", format!("deleted:{};", id("e1"))),
        ("unsorted", format!("deleted:{};{}", id("e2"), id("e1"))),
        ("duplicate", format!("deleted:{};{}", id("e1"), id("e1"))),
        (
            "uppercase hex",
            format!("deleted:{}", id("zz").to_uppercase()),
        ),
        ("odd hex", "deleted:651".into()),
        ("not utf8", "deleted:ff".into()),
        ("too long id", format!("deleted:{}", id(&"x".repeat(257)))),
    ];
    for (label, outcome) in cases {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("receipt.vlite"));
        forgotten(&mut db);
        rewrite(&mut db, &receipt_outcome_key(&forget_key(), 0), |p| {
            p.insert("outcome".into(), MetadataValue::String(outcome));
        });
        assert_eq!(
            error_name(lookup(&db, ReceiptOperation::Forget, "a", "f1", 50)),
            "InvalidBundle",
            "{label}"
        );
    }
}

#[test]
fn deleted_list_bounds_are_exact() {
    let ids = (0..MAX_OPS)
        .map(|index| format!("{index:05}"))
        .collect::<Vec<_>>();
    let encode = |ids: &[String]| {
        format!(
            "deleted:{}",
            ids.iter()
                .map(|id| hex(id.as_bytes()))
                .collect::<Vec<_>>()
                .join(";")
        )
    };
    assert_eq!(decode_deleted(&encode(&ids)).unwrap(), ids);
    let mut over = ids.clone();
    over.push("99999".into());
    assert!(matches!(
        decode_deleted(&encode(&over)),
        Err(MemoryError::InvalidBundle)
    ));
    let longest = "x".repeat(256);
    assert_eq!(
        decode_deleted(&encode(std::slice::from_ref(&longest))).unwrap(),
        vec![longest]
    );
}

#[test]
fn erasure_barrier_does_not_hide_a_retained_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(&dir.path().join("receipt.vlite"));
    let revision = forgotten(&mut db);
    let expected_scope_revision = MemoryRef::bind(&db, GRAPH, None)
        .unwrap()
        .scope_revision(&scope("a"))
        .unwrap();
    MemoryMut::bind(&mut db, GRAPH, None)
        .unwrap()
        .begin_erasure(&EraseScopeRequest {
            scope: scope("a"),
            idempotency_key: "erase".into(),
            expected_scope_revision,
            now_ms: 30,
            receipt_window_ms: 100,
            deadline_ms: None,
        })
        .unwrap();
    // The barrier is real: a write to the scope is refused.
    let write = MemoryMut::bind(&mut db, GRAPH, None)
        .unwrap()
        .forget(&ForgetRequest {
            scope: scope("a"),
            idempotency_key: "f9".into(),
            receipt_window_ms: 100,
            now_ms: 40,
            deadline_ms: None,
            targets: vec![ForgetTarget {
                id: "e1".into(),
                expected_revision: 1,
            }],
        });
    assert!(matches!(write, Err(MemoryError::DeletionPending)));
    assert_eq!(
        lookup(&db, ReceiptOperation::Forget, "a", "f1", 40).unwrap(),
        forget_lookup(revision)
    );
}

fn consolidate_request(key: &str) -> ConsolidateRequest {
    ConsolidateRequest {
        scope: scope("a"),
        idempotency_key: key.into(),
        now_ms: 20,
        clock_valid: true,
        receipt_window_ms: 100,
        deadline_ms: None,
        input_budget: None,
        inputs: vec![ConsolidateInput {
            id: "e1".into(),
            expected_memory_revision: Some(1),
            retain: true,
        }],
        summary: ConsolidateSummary {
            id: "summary1".into(),
            content: "summary".into(),
            source: "s2".into(),
            observed_at_ms: Some(20),
            importance: None,
            expiry_ms: None,
            expiry_valid: true,
            provenance: Vec::new(),
        },
        transformation: "summarize".into(),
        evidence: None,
        allow_future_observations: false,
    }
}

#[test]
fn consolidate_receipt_returns_summary_and_pins_the_bound_collection() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(&dir.path().join("receipt.vlite"));
    db.create_collection(
        "c",
        crate::CollectionSpec {
            dimensions: 2,
            settings: crate::CollectionSettings::default(),
        },
    )
    .unwrap();
    remember(&mut db, &["e1"], 10);
    let result = MemoryMut::bind(&mut db, GRAPH, None)
        .unwrap()
        .consolidate(&consolidate_request("c1"))
        .unwrap();
    let expected = Some(ReceiptLookup {
        outcome: ReceiptOutcome::Consolidate {
            summary_id: "summary1".into(),
        },
        receipt_revision: result.receipt_revision,
        retry_until_ms: 120,
    });
    assert_eq!(
        lookup(&db, ReceiptOperation::Consolidate, "a", "c1", 119).unwrap(),
        expected
    );
    assert_eq!(
        lookup(&db, ReceiptOperation::Consolidate, "a", "c1", 120).unwrap(),
        None
    );
    assert_eq!(
        lookup(&db, ReceiptOperation::Forget, "a", "c1", 50).unwrap(),
        None
    );
    // The receipt was written without an evidence collection; a binding with
    // one is a different binding, and the stored receipt says so.
    let other = MemoryRef::bind(&db, GRAPH, Some("c")).unwrap().receipt(
        ReceiptOperation::Consolidate,
        &scope("a"),
        "c1",
        50,
    );
    assert_eq!(error_name(other), "InvalidBundle");
    // Expired receipts still decode strictly.
    let key = operation_receipt_key("consolidate", &scope("a").key(), "c1");
    rewrite(&mut db, &key, |p| {
        p.insert("c1_schema".into(), MetadataValue::String("x".into()));
    });
    assert_eq!(
        error_name(lookup(&db, ReceiptOperation::Consolidate, "a", "c1", 500)),
        "InvalidBundle"
    );
}
