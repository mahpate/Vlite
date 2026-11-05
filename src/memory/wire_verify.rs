//! Wire-fixture verifier for the first Gap 1 REST slice (mapping section 3).
//!
//! The accepted wire fixtures under `tests/fixtures/agent_memory/wire/` are
//! read-only inputs; nothing here edits or re-derives them. The independent
//! oracle deliberately publishes no digest bytes (OPEN-G1W-01) and injects
//! receipt IDs, sequences and windows (OPEN-G1W-04), so this verifier checks
//! only what the memory core itself decides:
//!
//! - every fixture file carries the accepted schema and operation ID, and the
//!   four refused operations expect exactly the mapped capability refusal;
//! - GetReceipt cases resolve through the receipt accessor, or are one of the
//!   recorded first-slice refusals (remember lookup, fact/erasure receipts);
//! - Forget cases whose outcome the core decides (logical delete of retained
//!   IDs over a remembered pre-state) map to the fixture status, code and
//!   outcome IDs; every other case is named as decided by the adapter before
//!   the core (validation, feed, capability, attachment or replay views).
//!
//! `CORE_DIGESTS` are CORE-GENERATED goldens: the stored request digests this
//! core wrote when replaying the executed cases. They pin core behavior
//! against drift; they are never independent oracle outputs. Remember,
//! Recall and Consolidate wire cases need the adapter's request decoder and
//! are replayed over HTTP by the server tests, not here.

use serde_json::Value;

use super::mapping::{ReceiptOperation, WireMapping, WireOperation, memory_error, unavailable};
use super::{
    ForgetRequest, ForgetTarget, MemoryItem, MemoryKind, MemoryMut, MemoryRef, MemoryScope,
    RememberRequest, operation_receipt_key, string_property,
};
use crate::Database;

const GRAPH: &str = "memory";
const SCHEMA: &str = "memory-rest-wire-v1";

macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!(
            "../../tests/fixtures/agent_memory/wire/",
            $name,
            ".json"
        ))
    };
}

/// (file, operation field, operation ID, wire operation, fixture text).
const FIXTURES: [(&str, &str, WireOperation, &str); 9] = [
    (
        "remember",
        "MemoryRemember",
        WireOperation::Remember,
        fixture!("remember"),
    ),
    (
        "recall",
        "MemoryRecall",
        WireOperation::Recall,
        fixture!("recall"),
    ),
    (
        "facts",
        "MemoryFindFacts",
        WireOperation::FindFacts,
        fixture!("facts"),
    ),
    (
        "correct_facts",
        "MemoryCorrectFacts",
        WireOperation::CorrectFacts,
        fixture!("correct_facts"),
    ),
    (
        "consolidate",
        "MemoryConsolidate",
        WireOperation::Consolidate,
        fixture!("consolidate"),
    ),
    (
        "forget",
        "MemoryForget",
        WireOperation::Forget,
        fixture!("forget"),
    ),
    (
        "receipt",
        "MemoryGetReceipt",
        WireOperation::GetReceipt,
        fixture!("receipt"),
    ),
    (
        "erasure",
        "MemoryGetErasure",
        WireOperation::GetErasure,
        fixture!("erasure"),
    ),
    (
        "resume_erasure",
        "MemoryResumeErasure",
        WireOperation::ResumeErasure,
        fixture!("resume_erasure"),
    ),
];

/// GetReceipt cases the first slice answers differently from the fixture:
/// (case, refusal feature). Remember lookup is refused (coordinator ruling,
/// mapping section 3); fact and erasure receipts are refused capabilities.
const RECEIPT_REFUSALS: [(&str, &str); 6] = [
    ("receipt-live-before-expiry", "remember_receipt_lookup"),
    ("receipt-expiry-equality", "remember_receipt_lookup"),
    ("receipt-absent", "remember_receipt_lookup"),
    ("receipt-read-only", "remember_receipt_lookup"),
    ("receipt-absent-correct_facts-identity", "facts"),
    ("receipt-absent-resume_erasure-identity", "erasure_jobs"),
];

/// GetReceipt cases the adapter decides before any lookup (scope authority).
const RECEIPT_ADAPTER: [&str; 1] = ["receipt-scope-authorized-first"];

/// Forget cases the adapter decides before the core runs.
const FORGET_ADAPTER: [&str; 12] = [
    "forget-zero-retained-revision",
    "forget-expire-requires-deadline",
    "forget-logical-forbids-expiry",
    "forget-complete-feed",
    "forget-replay-after-target-deleted",
    "forget-external-attachment-atomic-refusal",
    "forget-scope-core-absent",
    "forget-session-core-absent",
    "forget-entity-core-absent",
    "forget-series-core-absent",
    "forget-id-fact-core-absent",
    "forget-id-relation-core-absent",
];

/// CORE-GENERATED stored request digests: (case, receipt, digest).
const CORE_DIGESTS: [(&str, &str, &str); 5] = [
    (
        "forget-small-logical",
        "remember:seed",
        "v1:ec808b3f509b2f08c056eea78f41b9f6967a10b6231523ce4ae3ca38b5124027",
    ),
    (
        "forget-small-logical",
        "forget:erase",
        "v1:e7b6ca87999350c295135c662fb30c6135681b3a92dab0cbd54c106ccba6bd3f",
    ),
    (
        "forget-missing-target",
        "remember:seed",
        "v1:ec808b3f509b2f08c056eea78f41b9f6967a10b6231523ce4ae3ca38b5124027",
    ),
    (
        "forget-revision-conflict",
        "remember:seed",
        "v1:ec808b3f509b2f08c056eea78f41b9f6967a10b6231523ce4ae3ca38b5124027",
    ),
    (
        "forget-final-target-guard-atomic",
        "remember:seed",
        "v1:b2ee8b1370dac93d1db2de1591c670154e55d5832241f056098b000c53672cd1",
    ),
];

fn cases(text: &str) -> Vec<Value> {
    let fixture: Value = serde_json::from_str(text).unwrap();
    fixture["cases"].as_array().unwrap().clone()
}

fn text<'a>(value: &'a Value, field: &str) -> &'a str {
    value[field].as_str().unwrap()
}

fn expected_code(case: &Value) -> (u64, Option<&str>) {
    let expected = &case["expected"];
    (
        expected["status"].as_u64().unwrap(),
        expected["body"]["error"]["code"].as_str(),
    )
}

fn scope(value: &Value) -> MemoryScope {
    let optional = |field: &str| value.get(field).and_then(Value::as_str).map(str::to_owned);
    MemoryScope {
        tenant: optional("tenant").unwrap_or_else(|| "@root".into()),
        agent: text(value, "agent").into(),
        user: optional("user"),
        session: optional("session"),
    }
}

fn database(path: &std::path::Path, scope: &MemoryScope) -> Database {
    let mut db = Database::create(path, 2).unwrap();
    db.create_graph(GRAPH).unwrap();
    MemoryMut::bind(&mut db, GRAPH, None)
        .unwrap()
        .provision_scope(scope)
        .unwrap();
    db
}

fn item(id: &str, kind: MemoryKind, content: &str, source: &str, observed: i64) -> MemoryItem {
    MemoryItem {
        id: id.into(),
        kind,
        content: content.into(),
        source: source.into(),
        importance: None,
        observed_at_ms: Some(observed),
        provenance: Vec::new(),
        references: Vec::new(),
        merge_into: None,
        expected_revision: None,
        expiry_ms: None,
        expiry_valid: true,
        evidence: Vec::new(),
    }
}

/// Replays one core-shaped `remember` pre-state step.
fn seed(db: &mut Database, step: &Value, digests: &mut Vec<(String, String)>) {
    assert_eq!(text(step, "op"), "remember");
    let request = &step["request"];
    let items = request["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| {
            assert!(
                value.get("evidence").is_none(),
                "evidence seeds are adapter views"
            );
            let kind = match text(value, "kind") {
                "entity" => MemoryKind::Entity,
                "episodic" => MemoryKind::Episodic,
                other => panic!("unexpected seed kind {other}"),
            };
            item(
                text(value, "id"),
                kind,
                text(value, "content"),
                text(value, "source"),
                value["observed_at"].as_i64().unwrap(),
            )
        })
        .collect();
    let request = RememberRequest {
        scope: scope(&request["scope"]),
        idempotency_key: text(request, "key").into(),
        receipt_window_ms: request["receipt_window"].as_u64().unwrap(),
        now_ms: step["clock"].as_u64().unwrap(),
        deadline_ms: None,
        allow_future_observations: false,
        items,
    };
    MemoryMut::bind(db, GRAPH, None)
        .unwrap()
        .remember(&request)
        .unwrap();
    let key = operation_receipt_key("remember", &request.scope.key(), &request.idempotency_key);
    digests.push((
        format!("remember:{}", request.idempotency_key),
        stored_digest(db, &key),
    ));
}

fn stored_digest(db: &Database, key: &str) -> String {
    let receipt = db
        .graph(GRAPH)
        .unwrap()
        .get_node(key)
        .unwrap()
        .output
        .unwrap();
    string_property(&receipt, "digest").unwrap().to_owned()
}

fn targets(body: &Value) -> Vec<ForgetTarget> {
    body["target"]["ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|target| ForgetTarget {
            id: text(target, "id").into(),
            expected_revision: text(target, "revision").parse().unwrap(),
        })
        .collect()
}

/// Whether the core alone decides this Forget case's outcome.
fn core_decided_forget(case: &Value) -> bool {
    let view = &case["view"];
    let body = &case["request"]["body"];
    let plain_body = body
        .as_object()
        .unwrap()
        .keys()
        .all(|key| ["scope", "idempotency_key", "target", "mode"].contains(&key.as_str()));
    let plain_target = body["target"]
        .as_object()
        .unwrap()
        .keys()
        .eq(["ids"].iter());
    view.get("capabilities").is_none()
        && view["receipts"].is_null()
        && view["pre_state"].as_array().unwrap().iter().all(|step| {
            step["op"] == "remember"
                && step["request"]["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|item| item.get("evidence").is_none())
        })
        && plain_body
        && body["mode"] == "logical_delete"
        && plain_target
        && targets(body)
            .iter()
            .all(|target| target.expected_revision > 0)
}

fn outcome_ids(body: &Value) -> Vec<&str> {
    body["outcomes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|outcome| {
            assert_eq!(text(outcome, "action"), "deleted");
            text(outcome, "id")
        })
        .collect()
}

fn assert_mapped(name: &str, error: &super::MemoryError, case: &Value) {
    let (status, code) = expected_code(case);
    match memory_error(error) {
        WireMapping::Code(wire) => {
            assert_eq!(u64::from(wire.status), status, "{name} status");
            assert_eq!(Some(wire.code), code, "{name} code");
        }
        other => panic!("{name}: {error:?} maps to {other:?}"),
    }
}

#[test]
fn every_fixture_is_the_accepted_wire_schema() {
    for (file, operation_id, operation, text_value) in FIXTURES {
        let fixture: Value = serde_json::from_str(text_value).unwrap();
        assert_eq!(fixture["schema"], SCHEMA, "{file}");
        assert_eq!(fixture["operation_id"], operation_id, "{file}");
        let cases = fixture["cases"].as_array().unwrap();
        assert!(!cases.is_empty(), "{file}");
        let refused = ["facts", "correct_facts", "erasure", "resume_erasure"].contains(&file);
        assert_eq!(operation.refusal().is_some(), refused, "{file}");
        for case in cases {
            assert_eq!(case["operation"], file, "{file}");
            if let Some(refusal) = operation.refusal() {
                assert_eq!(refusal.status, 409, "{file}");
                assert_eq!(expected_code(case), (409, Some(refusal.code)), "{file}");
            }
        }
    }
}

#[test]
fn receipt_cases_match_the_accessor_or_a_recorded_refusal() {
    let mut refused = Vec::new();
    let mut adapter = Vec::new();
    let mut served = 0;
    for case in cases(fixture!("receipt")) {
        let name = text(&case, "name");
        let view = &case["view"];
        let body = &case["request"]["body"];
        if view["scope_grants"].as_array().unwrap().is_empty() {
            assert_eq!(
                expected_code(&case),
                (403, Some("forbidden_scope")),
                "{name}"
            );
            adapter.push(name.to_owned());
            continue;
        }
        let operation = match ReceiptOperation::from_wire(text(body, "operation")) {
            Ok(operation) => operation,
            Err(refusal) => {
                assert_eq!(
                    (refusal.status, refusal.code),
                    (409, "memory_capability_unavailable"),
                    "{name}"
                );
                refused.push((name.to_owned(), refusal.feature.unwrap().to_owned()));
                continue;
            }
        };
        assert_eq!(operation, ReceiptOperation::Forget, "{name}");
        let scope = scope(&view["receipts"][0]["scope"]);
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("receipt.vlite"), &scope);
        let window = view["receipt_window_ms"].as_u64().unwrap();
        for stored in view["receipts"].as_array().unwrap() {
            let request = &stored["request"];
            let at = stored["retry_until_ms"].as_u64().unwrap() - window;
            let key = text(stored, "key");
            // Receipt identity is (operation, scope, key): a stored Remember
            // receipt is seeded under its key; a Forget receipt forgets
            // seeded copies of its targets. Contents are not looked up.
            let (remember_key, ids, forget) = match text(stored, "operation") {
                "remember" => {
                    let ids = request["items"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|value| text(value, "id").to_owned())
                        .collect::<Vec<_>>();
                    (key, ids, None)
                }
                "forget" => {
                    let targets = targets(request);
                    let ids = targets.iter().map(|target| target.id.clone()).collect();
                    ("seed", ids, Some(targets))
                }
                other => panic!("{name}: stored {other} receipt"),
            };
            let items = ids
                .iter()
                .map(|id| item(id, MemoryKind::Entity, "seed", "s1", at as i64))
                .collect();
            MemoryMut::bind(&mut db, GRAPH, None)
                .unwrap()
                .remember(&RememberRequest {
                    scope: scope.clone(),
                    idempotency_key: remember_key.into(),
                    receipt_window_ms: window,
                    now_ms: at,
                    deadline_ms: None,
                    allow_future_observations: false,
                    items,
                })
                .unwrap();
            if let Some(targets) = forget {
                MemoryMut::bind(&mut db, GRAPH, None)
                    .unwrap()
                    .forget(&ForgetRequest {
                        scope: scope.clone(),
                        idempotency_key: key.into(),
                        receipt_window_ms: window,
                        now_ms: at,
                        deadline_ms: None,
                        targets,
                    })
                    .unwrap();
            }
        }
        let lookup = MemoryRef::bind(&db, GRAPH, None)
            .unwrap()
            .receipt(
                operation,
                &scope,
                text(body, "idempotency_key"),
                view["clock_ms"].as_u64().unwrap(),
            )
            .unwrap();
        let expected = &case["expected"];
        match expected["status"].as_u64().unwrap() {
            200 => {
                let lookup = lookup.expect(name);
                let result = &expected["body"]["result"];
                let super::receipt::ReceiptOutcome::Forget { deleted } = &lookup.outcome else {
                    panic!("{name}: {:?}", lookup.outcome);
                };
                assert_eq!(deleted, &outcome_ids(result), "{name}");
                assert_eq!(
                    lookup.retry_until_ms.to_string(),
                    text(&result["receipt"], "retry_until_ms"),
                    "{name}"
                );
            }
            404 => assert!(lookup.is_none(), "{name}"),
            other => panic!("{name}: unexpected status {other}"),
        }
        served += 1;
    }
    let expected: Vec<_> = RECEIPT_REFUSALS
        .iter()
        .map(|(name, feature)| ((*name).to_owned(), (*feature).to_owned()))
        .collect();
    assert_eq!(refused, expected);
    assert_eq!(adapter, RECEIPT_ADAPTER);
    assert_eq!(served, 2);
    assert_eq!(unavailable("remember_receipt_lookup").status, 409);
}

#[test]
fn forget_cases_the_core_decides_match_the_fixture() {
    let mut adapter = Vec::new();
    let mut digests = Vec::new();
    for case in cases(fixture!("forget")) {
        let name = text(&case, "name");
        if !core_decided_forget(&case) {
            adapter.push(name.to_owned());
            continue;
        }
        let view = &case["view"];
        let body = &case["request"]["body"];
        let tenant = text(&view["scope_grants"][0], "tenant");
        let mut scope = scope(&body["scope"]);
        scope.tenant = tenant.into();
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("forget.vlite"), &scope);
        let mut seeded = Vec::new();
        for step in view["pre_state"].as_array().unwrap() {
            seed(&mut db, step, &mut seeded);
        }
        let request = ForgetRequest {
            scope: scope.clone(),
            idempotency_key: text(body, "idempotency_key").into(),
            receipt_window_ms: view["receipt_window_ms"].as_u64().unwrap(),
            now_ms: view["clock_ms"].as_u64().unwrap(),
            deadline_ms: None,
            targets: targets(body),
        };
        let result = MemoryMut::bind(&mut db, GRAPH, None)
            .unwrap()
            .forget(&request);
        match result {
            Ok(result) => {
                assert_eq!(case["expected"]["status"], 200, "{name}");
                assert!(!result.replayed, "{name}");
                assert_eq!(
                    result.deleted,
                    outcome_ids(&case["expected"]["body"]),
                    "{name}"
                );
                let key = operation_receipt_key("forget", &scope.key(), &request.idempotency_key);
                seeded.push((
                    format!("forget:{}", request.idempotency_key),
                    stored_digest(&db, &key),
                ));
            }
            Err(error) => assert_mapped(name, &error, &case),
        }
        for (receipt, digest) in seeded {
            digests.push((name.to_owned(), receipt, digest));
        }
    }
    assert_eq!(adapter, FORGET_ADAPTER);
    let pinned: Vec<_> = CORE_DIGESTS
        .iter()
        .map(|(case, receipt, digest)| {
            (
                (*case).to_owned(),
                (*receipt).to_owned(),
                (*digest).to_owned(),
            )
        })
        .collect();
    assert_eq!(digests, pinned);
}
