//! Receipt lookup and capability-refusal pins: `receipts/lookup` and the four
//! always-refusing routes (`facts/query`, `facts/correct`, `erasures/get`,
//! `erasures/resume`). Every expectation is derived by hand from the serving
//! contract (sections 3, 5, 6 and 8) and the adapter mapping (sections 1-3 and
//! 6), never from the handlers. Receipt windows and expiry are driven through
//! the `Clock` seam; nothing sleeps.

use std::sync::{Arc, atomic::Ordering};

use axum::{
    Extension, Router,
    body::Body,
    extract::DefaultBodyLimit,
    http::{Request, header},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use super::super::{
    MAX_JSON_BYTES, RequestId, Service, admin,
    credentials::{self, Op},
};
use super::routes::Clock;
use crate::memory::{MemoryRef, MemoryScope, ReceiptOperation};
use crate::property_graph::GraphOp;
use crate::{
    CollectionSpec, CreateOptions, Database, Expected, FormatVersion, Metadata, MetadataValue,
};

const PRINCIPAL: &str = "abcdefghij27";
const WINDOW_MS: u64 = 60_000;
const ALL: [Op; 5] = [
    Op::Read,
    Op::Search,
    Op::Write,
    Op::GraphRead,
    Op::GraphWrite,
];
const LOOKUP: &str = "/v1/memory/notes/receipts/lookup";
/// The four always-refusing routes and the feature each names (mapping
/// section 1).
const REFUSING: [(&str, &str); 4] = [
    ("facts/query", "facts"),
    ("facts/correct", "facts"),
    ("erasures/get", "erasure_jobs"),
    ("erasures/resume", "erasure_jobs"),
];

struct Rig {
    router: Router,
    clock: Clock,
    service: Service,
}

fn memory_binding(name: &str, graph: &str, evidence: Option<&str>, scopes: Value) -> Value {
    json!({
        "binding": name,
        "tenant": "@root",
        "generation": 1,
        "state": "active",
        "graph": graph,
        "evidence_collection": evidence,
        "schema_version": "memory-schema-v1",
        "ranking_version": "memory-ranking-v1",
        "digest_version": "memory-request-v1",
        "receipt_window_ms": WINDOW_MS,
        "grants": [{"principal": PRINCIPAL, "scopes": scopes}],
    })
}

/// The granted scopes: two shared agent scopes, a user scope and one session
/// scope under it. `a` is the first grant.
fn scopes() -> Value {
    json!([
        {"agent": "a"},
        {"agent": "b"},
        {"agent": "a", "user": "u"},
        {"agent": "a", "user": "u", "session": "s"},
    ])
}

fn notes() -> Value {
    memory_binding("notes", "notes_graph", Some("notes_ev"), scopes())
}

fn database(graphs: &[&str], collections: &[&str]) -> Database {
    let mut db = Database::create_in_memory(
        2,
        CreateOptions {
            format: Some(FormatVersion::V5),
            ..Default::default()
        },
    )
    .unwrap();
    for graph in graphs {
        db.create_graph(graph).unwrap();
    }
    for name in collections {
        db.create_collection(
            name,
            CollectionSpec {
                dimensions: 2,
                settings: Default::default(),
            },
        )
        .unwrap();
    }
    db
}

fn rig_for(
    db: Database,
    bindings: &[Value],
    ops: &[Op],
    narrow: impl FnOnce(&mut credentials::Grant),
) -> Rig {
    let lineage = db.lineage().expect("a version 5 database has a lineage");
    let text = json!({"version": "memory-bindings-v1", "bindings": bindings}).to_string();
    let set = crate::memory::binding::parse(text.as_bytes()).unwrap();
    let service = Service::new(db).with_bindings(super::reserved::Bindings::new(set));
    super::startup_at(&service, 0).unwrap();
    let mut grant = credentials::Grant::for_user(
        PRINCIPAL.into(),
        "pins".into(),
        None,
        ops,
        lineage,
        u64::MAX,
    );
    narrow(&mut grant);
    let clock = Clock::default();
    let router = super::routes::routes()
        .with_state(service.clone())
        .layer(Extension(admin::Role::Scoped(Arc::new(grant))))
        .layer(Extension(RequestId("pins".into())))
        .layer(Extension(clock.clone()))
        .layer(DefaultBodyLimit::max(MAX_JSON_BYTES));
    Rig {
        router,
        clock,
        service,
    }
}

fn std_rig() -> Rig {
    rig_for(
        database(&["notes_graph"], &["notes_ev"]),
        &[notes()],
        &ALL,
        |_| {},
    )
}

impl Rig {
    fn at(&self, ms: u64) {
        self.clock.now_ms.store(ms, Ordering::SeqCst);
    }

    async fn raw(&self, path: &str, body: Vec<u8>) -> (u16, Value) {
        let request = Request::builder()
            .method("POST")
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        self.raw(path, serde_json::to_vec(body).unwrap()).await
    }

    fn with_db<T>(&self, work: impl FnOnce(&mut Database) -> T) -> T {
        let mut owner = self.service.database.write().unwrap();
        work(owner.as_mut().unwrap())
    }

    /// The commit sequence and the node count of `graph`: anything a request
    /// writes moves at least one of them.
    fn state(&self, graph: &str) -> (u64, usize) {
        self.with_db(|db| (db.sequence, db.graph(graph).unwrap().info().node_count))
    }
}

fn route(binding: &str, name: &str) -> String {
    format!("/v1/memory/{binding}/{name}")
}

fn query(scope: &Value, operation: &str, key: &str) -> Value {
    json!({"scope": scope, "operation": operation, "idempotency_key": key})
}

async fn lookup(rig: &Rig, body: &Value) -> (u16, Value) {
    rig.post(LOOKUP, body).await
}

/// Remembers `ids` (revision 1) in `scope` through the real route.
async fn remember(rig: &Rig, binding: &str, scope: &Value, key: &str, ids: &[&str]) {
    let items: Vec<Value> = ids
        .iter()
        .map(|id| {
            json!({"id": id, "kind": "entity", "content": format!("text of {id}"),
                "observed_at_ms": 1, "source": {"source_id": "s", "source_kind": "caller"}})
        })
        .collect();
    let body = json!({"scope": scope, "idempotency_key": key, "items": items});
    let (status, response) = rig.post(&route(binding, "remember"), &body).await;
    assert_eq!(status, 200, "{response}");
}

/// Remembers `ids`, then forgets them under `key`; returns the forget answer.
async fn forget(rig: &Rig, binding: &str, scope: &Value, key: &str, ids: &[&str]) -> Value {
    remember(rig, binding, scope, &format!("seed-{key}"), ids).await;
    let targets: Vec<Value> = ids
        .iter()
        .map(|id| json!({"id": id, "revision": "1"}))
        .collect();
    let body = json!({"scope": scope, "idempotency_key": key,
        "target": {"ids": targets}, "mode": "logical_delete"});
    let (status, response) = rig.post(&route(binding, "forget"), &body).await;
    assert_eq!(status, 200, "{response}");
    response
}

/// Remembers `input`, then consolidates it into the episodic `summary`.
async fn consolidate(
    rig: &Rig,
    binding: &str,
    scope: &Value,
    key: &str,
    input: &str,
    summary: &str,
) -> Value {
    remember(rig, binding, scope, &format!("seed-{key}"), &[input]).await;
    let body = json!({"scope": scope, "idempotency_key": key,
        "inputs": [{"id": input, "revision": "1"}],
        "output": {"id": summary, "kind": "episodic", "content": "summary text",
            "observed_at_ms": 1,
            "source": {"source_id": "s", "source_kind": "caller", "transformation_id": "t"}}});
    let (status, response) = rig.post(&route(binding, "consolidate"), &body).await;
    assert_eq!(status, 200, "{response}");
    response
}

/// Status, error code and the expected `error.details` entries: each named
/// entry must match, other entries are not pinned (`Value::Null` pins none).
#[track_caller]
fn fault(response: &(u16, Value), status: u16, code: &str, details: Value) {
    assert_eq!(
        (response.0, response.1["error"]["code"].as_str()),
        (status, Some(code)),
        "{}",
        response.1
    );
    for (name, expected) in details.as_object().into_iter().flatten() {
        assert_eq!(
            &response.1["error"]["details"][name], expected,
            "{}",
            response.1
        );
    }
}

fn unavailable(feature: &str) -> Value {
    json!({"feature": feature})
}

fn is_receipt_id(value: &Value) -> bool {
    value.as_str().is_some_and(|id| {
        id.strip_prefix("rcpt:v1:").is_some_and(|hex| {
            hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        })
    })
}

fn keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

// The graph keys below restate the core's key derivation so a test can reach
// a stored node. Each use asserts the node exists first, so a drift in the
// derivation fails loudly instead of passing vacuously.

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn root(agent: &str) -> MemoryScope {
    MemoryScope {
        tenant: "@root".into(),
        agent: agent.into(),
        user: None,
        session: None,
    }
}

fn scope_bytes(scope: &MemoryScope) -> Vec<u8> {
    let mut bytes = b"scope:v1".to_vec();
    for value in [
        Some(scope.tenant.as_str()),
        Some(scope.agent.as_str()),
        scope.user.as_deref(),
        scope.session.as_deref(),
    ] {
        match value {
            None => bytes.push(0),
            Some(value) => {
                bytes.push(1);
                bytes.extend_from_slice(&(value.len() as u32).to_be_bytes());
                bytes.extend_from_slice(value.as_bytes());
            }
        }
    }
    bytes
}

fn scope_key(scope: &MemoryScope) -> String {
    format!("scope:v1:{}", hex(&Sha256::digest(scope_bytes(scope))))
}

fn node_key(operation: &str, scope_key: &str, idempotency_key: &str) -> String {
    let mut input = format!("{operation}:v1\0").into_bytes();
    input.extend_from_slice(idempotency_key.as_bytes());
    format!("receipt:v1:{scope_key}:{}", hex(&Sha256::digest(input)))
}

fn node_exists(db: &Database, graph: &str, key: &str) -> bool {
    db.graph(graph)
        .unwrap()
        .get_node(key)
        .unwrap()
        .output
        .is_some()
}

/// Replaces one stored node's properties at its current revision.
fn rewrite(db: &mut Database, graph: &str, key: &str, edit: impl FnOnce(&mut Metadata)) {
    let node = db
        .graph(graph)
        .unwrap()
        .get_node(key)
        .unwrap()
        .output
        .expect("the stored node exists");
    let mut properties = node.properties.clone();
    edit(&mut properties);
    db.graph_mut(graph)
        .unwrap()
        .write_batch(&[GraphOp::UpsertNode {
            key: node.key,
            labels: node.labels.into_iter().collect(),
            properties,
            expected: Expected::Revision(node.revision),
        }])
        .unwrap();
}

fn text(value: &str) -> MetadataValue {
    MetadataValue::String(value.into())
}

/// An erasure barrier on `scope`: the singleton job node the E1 core writes
/// (its presence is the barrier).
fn begin_erasure(db: &mut Database, graph: &str, scope: &MemoryScope, collection: &str) {
    let scope_key = scope_key(scope);
    let key = node_key("erase-scope-job", &scope_key, "singleton");
    let mut properties = Metadata::new();
    for (name, value) in [
        ("e1_schema", "erase-scope:v1".to_owned()),
        ("scope", scope_key),
        ("e1_scope_tuple", hex(&scope_bytes(scope))),
        ("e1_collection", collection.to_owned()),
        ("begin_digest", format!("v1:{}", "0".repeat(64))),
        ("generation", "1".into()),
        ("state", "pending".into()),
        ("logical_visibility", "hidden".into()),
        ("deleted_count", "0".into()),
        ("remaining", "unknown".into()),
    ] {
        properties.insert(name.into(), text(&value));
    }
    properties.insert("created_at_ms".into(), MetadataValue::Integer(150));
    db.graph_mut(graph)
        .unwrap()
        .write_batch(&[GraphOp::CreateNode {
            key,
            labels: vec!["VliteMemoryErasureJob".into()],
            properties,
        }])
        .unwrap();
}

// ------------------------------------------------------------ operation

/// Contract section 8 `MemoryOperation` is a closed lowercase enum: no case
/// folding and no trimming, and other values are `invalid_input` naming
/// `operation`, not a capability refusal and not a lookup.
#[tokio::test]
async fn receipt_operation_names_are_the_exact_lowercase_vocabulary() {
    let rig = std_rig();
    rig.at(1_000);
    let scope = json!({"agent": "a"});
    for operation in [
        json!("FORGET"),
        json!("Forget"),
        json!("CONSOLIDATE"),
        json!("Consolidate"),
        json!("REMEMBER"),
        json!("Correct_Facts"),
        json!("RESUME_ERASURE"),
        json!(" forget"),
        json!("forget "),
        json!("\tforget"),
        json!("forget\n"),
        json!(" consolidate"),
        json!("remember "),
        json!(" correct_facts"),
        json!("resume_erasure\t"),
        json!(""),
        json!("recall"),
        json!("forget\u{0}"),
        json!(1),
        json!(true),
        json!(null),
        json!(["forget"]),
        json!({"name": "forget"}),
    ] {
        let body = json!({"scope": scope, "operation": operation, "idempotency_key": "k"});
        let response = lookup(&rig, &body).await;
        fault(
            &response,
            422,
            "invalid_input",
            json!({"field": "operation"}),
        );
    }
    let missing = json!({"scope": scope, "idempotency_key": "k"});
    let response = lookup(&rig, &missing).await;
    fault(
        &response,
        422,
        "invalid_input",
        json!({"field": "operation"}),
    );
    // The exact names stay served or refused as the mapping says.
    for operation in ["forget", "consolidate"] {
        let response = lookup(&rig, &query(&scope, operation, "k")).await;
        fault(&response, 404, "receipt_not_found", Value::Null);
    }
    for (operation, feature) in [
        ("remember", "remember_receipt_lookup"),
        ("correct_facts", "facts"),
        ("resume_erasure", "erasure_jobs"),
    ] {
        let response = lookup(&rig, &query(&scope, operation, "k")).await;
        fault(
            &response,
            409,
            "memory_capability_unavailable",
            unavailable(feature),
        );
    }
}

/// Mapping sections 1-3: a lookup of a `remember`, `correct_facts` or
/// `resume_erasure` receipt is refused 409 after exact-scope authorization and
/// before any other request validation (key, unknown fields).
#[tokio::test]
async fn unsupported_receipt_operations_are_refused_after_authorization_and_before_validation() {
    let rig = std_rig();
    rig.at(1_000);
    for (operation, feature) in [
        ("remember", "remember_receipt_lookup"),
        ("correct_facts", "facts"),
        ("resume_erasure", "erasure_jobs"),
    ] {
        let scope = json!({"agent": "a"});
        let overlong = "x".repeat(257);
        for body in [
            json!({"scope": scope, "operation": operation, "idempotency_key": "k"}),
            json!({"scope": scope, "operation": operation, "idempotency_key": ""}),
            json!({"scope": scope, "operation": operation, "idempotency_key": 7}),
            json!({"scope": scope, "operation": operation, "idempotency_key": null}),
            json!({"scope": scope, "operation": operation, "idempotency_key": overlong}),
            json!({"scope": scope, "operation": operation}),
            json!({"scope": scope, "operation": operation, "idempotency_key": "k", "x": 1}),
        ] {
            let response = lookup(&rig, &body).await;
            fault(
                &response,
                409,
                "memory_capability_unavailable",
                unavailable(feature),
            );
        }
        // Authorization first: an ungranted scope never learns the feature.
        let ungranted = json!({"scope": {"agent": "zz"}, "operation": operation,
            "idempotency_key": "k"});
        let response = lookup(&rig, &ungranted).await;
        fault(&response, 403, "forbidden_scope", Value::Null);
    }
}

// ------------------------------------------------------------ strict JSON

/// Contract section 3 `MemoryKey`: 1-256 UTF-8 bytes (the maximum is
/// allowed), no normalization or case folding, no control characters, and
/// only a string (never a number, boolean or null).
#[tokio::test]
async fn receipt_keys_are_opaque_utf8_strings_of_one_to_256_bytes() {
    let rig = std_rig();
    rig.at(1_000);
    let scope = json!({"agent": "a"});
    let boundary_multibyte = "é".repeat(128);
    for key in [
        "k".repeat(256),
        boundary_multibyte.clone(),
        "日".repeat(85),
        "🙂".repeat(64),
        "clé-日本-🙂".to_owned(),
        "k".to_owned(),
        "a b".to_owned(),
        " k".to_owned(),
        "k ".to_owned(),
    ] {
        let response = lookup(&rig, &query(&scope, "forget", &key)).await;
        fault(&response, 404, "receipt_not_found", Value::Null);
    }
    for key in [
        "k".repeat(257),
        format!("{boundary_multibyte}a"),
        "é".repeat(129),
        "日".repeat(86),
        "🙂".repeat(65),
        String::new(),
        "a\tb".into(),
        "a\nb".into(),
        "a\u{0}b".into(),
        "a\u{1f}b".into(),
        "a\u{7f}b".into(),
        "a\u{80}b".into(),
        "a\u{9f}b".into(),
    ] {
        let response = lookup(&rig, &query(&scope, "forget", &key)).await;
        fault(
            &response,
            422,
            "invalid_input",
            json!({"field": "idempotency_key"}),
        );
    }
    for key in [
        json!(7),
        json!(0),
        json!(1.5),
        json!(true),
        json!(false),
        json!(null),
        json!([]),
        json!(["k"]),
        json!({}),
        json!({"key": "k"}),
    ] {
        let body = json!({"scope": scope, "operation": "forget", "idempotency_key": key});
        let response = lookup(&rig, &body).await;
        fault(
            &response,
            422,
            "invalid_input",
            json!({"field": "idempotency_key"}),
        );
    }
    let missing = json!({"scope": scope, "operation": "forget"});
    let response = lookup(&rig, &missing).await;
    fault(
        &response,
        422,
        "invalid_input",
        json!({"field": "idempotency_key"}),
    );
}

/// Contract section 3: a request scope is `{agent, user?, session?}`. A client
/// tenant, an explicit null, a session without a user, an unknown field and
/// any other shape are rejected as `invalid_input`/`scope`; nothing is
/// silently dropped, defaulted or widened before authorization.
#[tokio::test]
async fn the_request_scope_is_validated_strictly_before_authorization() {
    let rig = std_rig();
    rig.at(1_000);
    for scope in [
        json!({"agent": "a", "tenant": "@root"}),
        json!({"agent": "a", "tenant": "other"}),
        json!({"agent": "a", "tenant": null}),
        json!({"agent": "a", "user": null}),
        json!({"agent": "a", "user": "u", "session": null}),
        json!({"agent": "a", "session": "s"}),
        json!({"agent": "a", "session": "s", "user": null}),
        json!({"agent": "a", "extra": true}),
        json!({"agent": "a", "user": ""}),
        json!({"agent": "a", "user": 1}),
        json!({}),
        json!({"user": "u"}),
        json!({"agent": ""}),
        json!({"agent": 1}),
        json!({"agent": null}),
        json!({"agent": "a\u{0}"}),
        json!({"agent": "x".repeat(257)}),
        json!("a"),
        json!(null),
        json!([]),
        json!(["a"]),
    ] {
        let response = lookup(&rig, &query(&scope, "forget", "k")).await;
        fault(&response, 422, "invalid_input", json!({"field": "scope"}));
    }
    let missing = json!({"operation": "forget", "idempotency_key": "k"});
    let response = lookup(&rig, &missing).await;
    fault(&response, 422, "invalid_input", json!({"field": "scope"}));
}

/// Contract section 3: duplicate keys at any depth are `invalid_json`
/// (400), never last-value-wins; unknown top-level fields are rejected.
#[tokio::test]
async fn the_lookup_body_is_a_closed_object_with_unique_keys() {
    let rig = std_rig();
    rig.at(1_000);
    for body in [
        r#"{"scope":{"agent":"a"},"operation":"forget","idempotency_key":"k","idempotency_key":"k2"}"#,
        r#"{"scope":{"agent":"a","agent":"b"},"operation":"forget","idempotency_key":"k"}"#,
        r#"{"scope":{"agent":"a"},"scope":{"agent":"a"},"operation":"forget","idempotency_key":"k"}"#,
        r#"{"scope":{"agent":"a"},"operation":"forget","operation":"consolidate","idempotency_key":"k"}"#,
        r#"{"scope":{"agent":"a"},"operation":"remember","operation":"forget","idempotency_key":"k"}"#,
    ] {
        let response = rig.raw(LOOKUP, body.as_bytes().to_vec()).await;
        fault(&response, 400, "invalid_json", Value::Null);
    }
    for extra in [json!(1), json!(null), json!("x"), json!({}), json!([])] {
        let mut body = query(&json!({"agent": "a"}), "forget", "k");
        body["unknown"] = extra;
        let response = lookup(&rig, &body).await;
        fault(&response, 422, "invalid_input", Value::Null);
        assert!(
            response.1["error"]["details"]["field"].is_string(),
            "an unknown field names the schema field: {}",
            response.1
        );
    }
}

// ------------------------------------------------------------ result

/// Contract sections 3, 5 and 8 (forget exchange): a forget receipt reads
/// back as its replay: `{result: ...}` with `replayed:true`, the deleted IDs
/// sorted ascending (UTF-8 byte order, mapping section 6) each at the receipt
/// sequence, the logical state and the erasure notice, string-encoded
/// sequence and expiry, and no echo of the request's key or of forgotten
/// text.
#[tokio::test]
async fn a_forget_receipt_reads_back_as_its_replay_with_ids_in_byte_order() {
    let rig = std_rig();
    rig.at(100);
    let scope = json!({"agent": "a"});
    let forgot = forget(&rig, "notes", &scope, "needle-key", &["é", "a", "Z", "B"]).await;
    let before = rig.state("notes_graph");
    rig.at(200);
    let (status, body) = lookup(&rig, &query(&scope, "forget", "needle-key")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(keys(&body), ["result"]);
    let result = &body["result"];
    assert_eq!(
        keys(result),
        [
            "erasure_notice",
            "feed",
            "outcomes",
            "receipt",
            "remaining_work",
            "replayed",
            "state"
        ]
    );
    let sequence = forgot["receipt"]["sequence"].clone();
    let sequence_text = sequence.as_str().expect("the sequence is a string");
    assert!(
        sequence_text
            .parse::<u64>()
            .is_ok_and(|n| n > 1 && n.to_string() == sequence_text),
        "{sequence}"
    );
    assert!(is_receipt_id(&result["receipt"]["id"]), "{result}");
    assert_eq!(
        result["receipt"],
        json!({
            "id": forgot["receipt"]["id"],
            "operation": "forget",
            "scope": {"tenant": "@root", "agent": "a"},
            "sequence": sequence,
            "retry_until_ms": "60100",
            "digest_version": "memory-request-v1",
        })
    );
    let deleted = |id: &str| json!({"id": id, "action": "deleted", "revision": sequence});
    assert_eq!(
        result["outcomes"],
        json!([deleted("B"), deleted("Z"), deleted("a"), deleted("é")])
    );
    assert_eq!(result["replayed"], json!(true));
    assert_eq!(result["state"], json!("logical"));
    assert_eq!(result["remaining_work"], json!("0"));
    assert_eq!(
        result["feed"],
        json!({"mode": "ordinary_changes", "temporal_complete": false,
            "ttl_complete": false, "consumer_purge_acknowledged": false})
    );
    // Every caveat is present with its fixed value; `manifest_id` is the one
    // field the first slice discloses as omitted (mapping section 1), so it
    // is neither required nor forbidden here.
    let mut notice = result["erasure_notice"].clone();
    notice.as_object_mut().unwrap().remove("manifest_id");
    assert_eq!(
        notice,
        json!({"live_state_only": true, "secure_erasure": false,
            "external_copies": "operator_managed", "restore_requires_manifest": true,
            "backup_history_purge": "not_verified"})
    );
    let rendered = body.to_string();
    for leaked in ["needle-key", "idempotency_key", "text of "] {
        assert!(
            !rendered.contains(leaked),
            "{leaked} leaked into {rendered}"
        );
    }
    let mut again = forgot.clone();
    again["replayed"] = json!(true);
    assert_eq!(body, json!({"result": again}));
    assert_eq!(rig.state("notes_graph"), before, "a lookup is a read");
}

/// Mapping sections 3 and 6: a consolidate receipt yields its one `created`
/// summary at memory revision 1 (not at the receipt sequence), and carries
/// neither a forget state nor an erasure notice.
#[tokio::test]
async fn a_consolidate_receipt_yields_the_summary_at_revision_one() {
    let rig = std_rig();
    rig.at(100);
    let scope = json!({"agent": "a"});
    let created = consolidate(&rig, "notes", &scope, "needle-c", "m", "s1").await;
    rig.at(200);
    let (status, body) = lookup(&rig, &query(&scope, "consolidate", "needle-c")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(keys(&body), ["result"]);
    let result = &body["result"];
    assert_eq!(
        keys(result),
        ["feed", "outcomes", "receipt", "remaining_work", "replayed"]
    );
    let sequence = created["receipt"]["sequence"].clone();
    assert_ne!(sequence, json!("1"), "the receipt is not the first commit");
    assert_eq!(
        result["receipt"],
        json!({
            "id": created["receipt"]["id"],
            "operation": "consolidate",
            "scope": {"tenant": "@root", "agent": "a"},
            "sequence": sequence,
            "retry_until_ms": "60100",
            "digest_version": "memory-request-v1",
        })
    );
    assert!(is_receipt_id(&result["receipt"]["id"]), "{result}");
    assert_eq!(
        result["outcomes"],
        json!([{"id": "s1", "action": "created", "revision": "1"}])
    );
    assert_eq!(result["replayed"], json!(true));
    assert_eq!(result["remaining_work"], json!("0"));
    let rendered = body.to_string();
    for leaked in ["needle-c", "idempotency_key", "summary text"] {
        assert!(
            !rendered.contains(leaked),
            "{leaked} leaked into {rendered}"
        );
    }
    let mut again = created.clone();
    again["replayed"] = json!(true);
    assert_eq!(body, json!({"result": again}));
}

// ------------------------------------------------------------ expiry

/// Contract section 3 retention and mapping section 3: a receipt is live
/// while `now < retry_until_ms` and not found from `now >= retry_until_ms`,
/// for forget and consolidate alike, on the injected clock.
#[tokio::test]
async fn receipts_are_live_until_retry_until_and_not_found_from_it_on() {
    let rig = std_rig();
    rig.at(100);
    let scope = json!({"agent": "a"});
    forget(&rig, "notes", &scope, "k", &["m"]).await;
    consolidate(&rig, "notes", &scope, "c", "n", "s1").await;
    for (operation, key) in [("forget", "k"), ("consolidate", "c")] {
        for clock in [100, 200, 60_098, 60_099] {
            rig.at(clock);
            let (status, body) = lookup(&rig, &query(&scope, operation, key)).await;
            assert_eq!(status, 200, "{operation} at {clock}: {body}");
            assert_eq!(body["result"]["receipt"]["retry_until_ms"], json!("60100"));
            assert_eq!(body["result"]["replayed"], json!(true));
        }
        for clock in [60_100, 60_101, 1_000_000, i64::MAX as u64] {
            rig.at(clock);
            let response = lookup(&rig, &query(&scope, operation, key)).await;
            assert_eq!(response.0, 404, "{operation} at {clock}: {}", response.1);
            fault(&response, 404, "receipt_not_found", Value::Null);
        }
    }
}

/// Mapping section 2: `InvalidClock` is a server clock fault, 500
/// `internal_error`, never a 422 attributed to the caller; the largest valid
/// clock is still served.
#[tokio::test]
async fn an_engine_clock_above_i64_max_is_a_server_fault() {
    let rig = std_rig();
    let scope = json!({"agent": "a"});
    for clock in [i64::MAX as u64 + 1, u64::MAX] {
        rig.at(clock);
        let response = lookup(&rig, &query(&scope, "forget", "k")).await;
        fault(&response, 500, "internal_error", Value::Null);
    }
    rig.at(i64::MAX as u64);
    let response = lookup(&rig, &query(&scope, "forget", "k")).await;
    fault(&response, 404, "receipt_not_found", Value::Null);
}

/// Mapping section 3 (`retry_until`): the response's `retry_until_ms` is the
/// stored expiry. Receipt nodes carry no graph deadline, so reading them at
/// any clock yields the same value; the clock captured once per request is
/// therefore the only one that can matter (this is the premise that makes a
/// second clock sample unobservable).
#[tokio::test]
async fn a_stored_receipt_expiry_does_not_depend_on_the_read_clock() {
    let rig = std_rig();
    rig.at(100);
    let scope = json!({"agent": "a"});
    forget(&rig, "notes", &scope, "k", &["m"]).await;
    consolidate(&rig, "notes", &scope, "c", "n", "s1").await;
    let owner = root("a");
    let scope_key = scope_key(&owner);
    rig.with_db(|db| {
        let memory = MemoryRef::bind(db, "notes_graph", Some("notes_ev")).unwrap();
        for (operation, name, key) in [
            (ReceiptOperation::Forget, "forget", "k"),
            (ReceiptOperation::Consolidate, "consolidate", "c"),
        ] {
            let stored = node_key(name, &scope_key, key);
            let node = db
                .graph("notes_graph")
                .unwrap()
                .get_node(&stored)
                .unwrap()
                .output;
            let node = node.expect("the receipt node exists");
            assert_eq!(
                node.expires_at_ms, None,
                "{name}: a receipt node has no graph deadline"
            );
            for clock in [
                0,
                1,
                99,
                100,
                60_099,
                60_100,
                60_101,
                i64::MAX as u64,
                u64::MAX,
            ] {
                assert_eq!(
                    memory.retry_until(operation, &owner, key, clock).unwrap(),
                    Some(60_100),
                    "{name} at {clock}"
                );
            }
        }
    });
}

// ------------------------------------------------------------ identity

/// Contract section 3: a receipt's identity is its exact scope (no parent,
/// child or sibling fallback), operation and key; authorization is exact too.
#[tokio::test]
async fn receipts_belong_to_their_exact_scope_with_no_fallback() {
    let rig = std_rig();
    rig.at(100);
    forget(&rig, "notes", &json!({"agent": "a"}), "ka", &["ma"]).await;
    forget(&rig, "notes", &json!({"agent": "b"}), "kb", &["mb"]).await;
    let user = json!({"agent": "a", "user": "u"});
    forget(&rig, "notes", &user, "kp", &["mp"]).await;
    let session = json!({"agent": "a", "user": "u", "session": "s"});
    forget(&rig, "notes", &session, "ks", &["ms"]).await;
    rig.at(200);
    // Each granted scope sees only its own receipts.
    for (scope, key, effective, memory) in [
        (
            json!({"agent": "a"}),
            "ka",
            json!({"tenant": "@root", "agent": "a"}),
            "ma",
        ),
        (
            json!({"agent": "b"}),
            "kb",
            json!({"tenant": "@root", "agent": "b"}),
            "mb",
        ),
        (
            user.clone(),
            "kp",
            json!({"tenant": "@root", "agent": "a", "user": "u"}),
            "mp",
        ),
        (
            session.clone(),
            "ks",
            json!({"tenant": "@root", "agent": "a", "user": "u", "session": "s"}),
            "ms",
        ),
    ] {
        let (status, body) = lookup(&rig, &query(&scope, "forget", key)).await;
        assert_eq!(status, 200, "{key}: {body}");
        assert_eq!(body["result"]["receipt"]["scope"], effective);
        assert_eq!(body["result"]["outcomes"][0]["id"], json!(memory));
    }
    for (scope, key) in [
        (json!({"agent": "b"}), "ka"),
        (json!({"agent": "a"}), "kb"),
        (json!({"agent": "a"}), "kp"),
        (json!({"agent": "a"}), "ks"),
        (user.clone(), "ka"),
        (user.clone(), "ks"),
        (session.clone(), "kp"),
        (session.clone(), "ka"),
    ] {
        let response = lookup(&rig, &query(&scope, "forget", key)).await;
        fault(&response, 404, "receipt_not_found", Value::Null);
    }
    // A scope that is not an exact grant is refused even under a granted
    // parent, and before any lookup.
    for scope in [
        json!({"agent": "a", "user": "u", "session": "s2"}),
        json!({"agent": "a", "user": "u2"}),
        json!({"agent": "c"}),
        json!({"agent": "b", "user": "u"}),
    ] {
        for key in ["kp", "absent"] {
            let response = lookup(&rig, &query(&scope, "forget", key)).await;
            fault(&response, 403, "forbidden_scope", Value::Null);
        }
    }
    // The operation is part of the identity.
    let response = lookup(&rig, &query(&json!({"agent": "a"}), "consolidate", "ka")).await;
    fault(&response, 404, "receipt_not_found", Value::Null);
}

/// Contract section 3 and mapping section 3: a lookup reads the graph of the
/// binding it was sent to; another authorized binding never answers for it.
#[tokio::test]
async fn a_lookup_reads_the_graph_of_its_own_binding() {
    let other = memory_binding("other", "other_graph", None, json!([{"agent": "a"}]));
    let rig = rig_for(
        database(&["notes_graph", "other_graph"], &["notes_ev"]),
        &[notes(), other],
        &ALL,
        |_| {},
    );
    rig.at(100);
    let scope = json!({"agent": "a"});
    forget(&rig, "notes", &scope, "kn", &["mn"]).await;
    forget(&rig, "other", &scope, "ko", &["mo"]).await;
    rig.at(200);
    for (binding, key, expected) in [("notes", "kn", "mn"), ("other", "ko", "mo")] {
        let path = route(binding, "receipts/lookup");
        let (status, found) = rig.post(&path, &query(&scope, "forget", key)).await;
        assert_eq!(status, 200, "{binding}/{key}: {found}");
        assert_eq!(found["result"]["outcomes"][0]["id"], json!(expected));
    }
    for (binding, key) in [("notes", "ko"), ("other", "kn")] {
        let path = route(binding, "receipts/lookup");
        let response = rig.post(&path, &query(&scope, "forget", key)).await;
        fault(&response, 404, "receipt_not_found", Value::Null);
    }
}

// ------------------------------------------------------------ barrier

/// Contract section 5 and mapping section 6: a deletion barrier hides
/// memories, never receipts: a receipt written before the barrier is still
/// served (200) and an absent one is still 404, not 409 `deletion_pending`.
#[tokio::test]
async fn an_erasure_barrier_never_hides_a_retained_receipt() {
    let rig = std_rig();
    rig.at(100);
    let scope = json!({"agent": "a"});
    let forgot = forget(&rig, "notes", &scope, "k", &["m"]).await;
    let created = consolidate(&rig, "notes", &scope, "c", "n", "s1").await;
    rig.with_db(|db| {
        begin_erasure(db, "notes_graph", &root("a"), "notes_ev");
    });
    // The barrier is real: a write to the scope is refused.
    let write = json!({"scope": scope, "idempotency_key": "after", "items": [
        {"id": "late", "kind": "entity", "content": "late", "observed_at_ms": 1,
         "source": {"source_id": "s", "source_kind": "caller"}}]});
    let response = rig.post(&route("notes", "remember"), &write).await;
    fault(&response, 409, "memory_deletion_pending", Value::Null);
    rig.at(200);
    for (operation, key, original) in [("forget", "k", &forgot), ("consolidate", "c", &created)] {
        let (status, body) = lookup(&rig, &query(&scope, operation, key)).await;
        assert_eq!(status, 200, "{operation}: {body}");
        let mut expected = original.clone();
        expected["replayed"] = json!(true);
        assert_eq!(body, json!({"result": expected}));
    }
    let response = lookup(&rig, &query(&scope, "forget", "absent")).await;
    fault(&response, 404, "receipt_not_found", Value::Null);
    // Expiry still applies behind the barrier.
    rig.at(60_100);
    let response = lookup(&rig, &query(&scope, "forget", "k")).await;
    fault(&response, 404, "receipt_not_found", Value::Null);
}

// ------------------------------------------------------------ faults

/// Mapping sections 2-3: a present receipt that does not decode strictly is
/// an integrity fault, 500 `internal_error`, whether it is live or expired;
/// it is never `receipt_not_found`.
#[tokio::test]
async fn an_undecodable_stored_receipt_is_a_server_fault_live_or_expired() {
    let scope = json!({"agent": "a"});
    let foreign = scope_key(&root("b"));
    for case in 0..5 {
        let rig = std_rig();
        rig.at(100);
        forget(&rig, "notes", &scope, "k", &["m"]).await;
        consolidate(&rig, "notes", &scope, "c", "n", "s1").await;
        let owner = scope_key(&root("a"));
        let forget_key = node_key("forget", &owner, "k");
        let consolidate_key = node_key("consolidate", &owner, "c");
        rig.with_db(|db| {
            assert!(node_exists(db, "notes_graph", &forget_key));
            assert!(node_exists(db, "notes_graph", &consolidate_key));
            match case {
                0 => rewrite(db, "notes_graph", &forget_key, |properties| {
                    properties.insert("extra".into(), MetadataValue::Integer(1));
                }),
                1 => rewrite(db, "notes_graph", &forget_key, |properties| {
                    properties.insert("scope".into(), text(&foreign));
                }),
                2 => rewrite(db, "notes_graph", &forget_key, |properties| {
                    properties.insert("expires_at_ms".into(), text("060100"));
                }),
                3 => rewrite(db, "notes_graph", &consolidate_key, |properties| {
                    properties.insert("c1_schema".into(), text("x"));
                }),
                _ => rewrite(db, "notes_graph", &consolidate_key, |properties| {
                    properties.insert("c1_scope_tuple".into(), text("00"));
                }),
            }
        });
        let (operation, key) = if case < 3 {
            ("forget", "k")
        } else {
            ("consolidate", "c")
        };
        for clock in [200, 70_000] {
            rig.at(clock);
            let response = lookup(&rig, &query(&scope, operation, key)).await;
            assert_eq!(response.0, 500, "case {case} at {clock}: {}", response.1);
            fault(&response, 500, "internal_error", Value::Null);
        }
        // The sibling receipt is untouched.
        rig.at(200);
        let (operation, key) = if case < 3 {
            ("consolidate", "c")
        } else {
            ("forget", "k")
        };
        let (status, body) = lookup(&rig, &query(&scope, operation, key)).await;
        assert_eq!(status, 200, "case {case}: {body}");
    }
}

/// Mapping section 2: the memory core raises `InvalidQuery` only for a
/// missing or mistyped stored property, an integrity fault: 500
/// `internal_error`, never the shared mapper's 422.
#[tokio::test]
async fn a_mistyped_stored_property_is_an_internal_error_not_invalid_input() {
    let rig = std_rig();
    rig.at(100);
    let scope = json!({"agent": "a"});
    forget(&rig, "notes", &scope, "k", &["m"]).await;
    let key = scope_key(&root("a"));
    rig.with_db(|db| {
        assert!(node_exists(db, "notes_graph", &key));
        rewrite(db, "notes_graph", &key, |properties| {
            properties.insert("tuple".into(), MetadataValue::Integer(1));
        });
    });
    rig.at(200);
    let response = lookup(&rig, &query(&scope, "forget", "k")).await;
    fault(&response, 500, "internal_error", Value::Null);
}

// ------------------------------------------------------------ refusing routes

fn refusing_body(extra: &Value) -> Value {
    let mut body = json!({"scope": {"agent": "a"}});
    body.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    body
}

/// Mapping sections 1-2: after authentication, binding resolution and
/// exact-scope authorization the four routes answer 409
/// `memory_capability_unavailable` naming `facts` or `erasure_jobs`, before
/// any validation of the rest of the body (including `require_complete_feed`,
/// whose own refusal ranks below the route refusal), and write nothing.
#[tokio::test]
async fn the_four_refusing_routes_name_their_feature_whatever_the_body() {
    let rig = std_rig();
    rig.at(1_000);
    let before = rig.state("notes_graph");
    for (name, feature) in REFUSING {
        let path = route("notes", name);
        for extra in [
            json!({}),
            json!({"slot": 7, "job_id": [], "x": null, "deadline_ms": -1, "max_references": "many"}),
            json!({"slot": {"subject_id": "e1", "predicate": "role", "slot_key": "primary"},
                "job_id": "j1", "idempotency_key": "k", "revision": "8", "generation": "1",
                "resume_token": "t"}),
            json!({"require_complete_feed": true}),
            json!({"require_complete_feed": false}),
            json!({"require_complete_feed": "yes"}),
            json!({"require_complete_feed": true, "deadline_ms": 0, "unknown": [1]}),
            json!({"idempotency_key": "", "job_id": "", "slot": null}),
        ] {
            let body = refusing_body(&extra);
            let response = rig.post(&path, &body).await;
            assert_eq!(response.0, 409, "{name} {body}: {}", response.1);
            fault(
                &response,
                409,
                "memory_capability_unavailable",
                unavailable(feature),
            );
        }
    }
    assert_eq!(rig.state("notes_graph"), before, "a refusal writes nothing");
    let (status, body) = lookup(&rig, &query(&json!({"agent": "a"}), "forget", "k")).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (404, Some("receipt_not_found"))
    );
    assert_eq!(rig.state("notes_graph"), before, "a lookup writes nothing");
}

/// Contract section 6 and mapping section 2: binding resolution and exact
/// scope authorization come first for every route of the family; an unknown
/// binding, an ungranted graph and an ungranted scope are the same 403 body.
#[tokio::test]
async fn binding_and_exact_scope_authorization_precede_every_refusal_and_lookup() {
    let rig = std_rig();
    rig.at(1_000);
    let mut routes: Vec<&str> = REFUSING.iter().map(|(name, _)| *name).collect();
    routes.push("receipts/lookup");
    for name in routes {
        let body = |scope: Value| {
            json!({"scope": scope, "operation": "forget", "idempotency_key": "k",
                "slot": {}, "job_id": "j"})
        };
        let reference = rig
            .post(&route("notes", name), &body(json!({"agent": "zz"})))
            .await;
        fault(&reference, 403, "forbidden_scope", Value::Null);
        for scope in [
            json!({"agent": "a", "user": "u2"}),
            json!({"agent": "a", "user": "u", "session": "s2"}),
            json!({"agent": "b", "user": "u"}),
        ] {
            let response = rig.post(&route("notes", name), &body(scope)).await;
            assert_eq!(response, reference, "{name}");
        }
        let unknown = rig
            .post(&route("ghost", name), &body(json!({"agent": "a"})))
            .await;
        assert_eq!(
            unknown, reference,
            "{name}: an unknown binding is not disclosed"
        );
    }
    // A credential whose graph grant excludes the bound graph is refused the
    // same way, before the route's own answer.
    let narrowed = rig_for(
        database(&["notes_graph"], &["notes_ev"]),
        &[notes()],
        &ALL,
        |grant| grant.graphs = vec!["other_graph".into()],
    );
    narrowed.at(1_000);
    let mut routes: Vec<&str> = REFUSING.iter().map(|(name, _)| *name).collect();
    routes.push("receipts/lookup");
    for name in routes {
        let body = json!({"scope": {"agent": "a"}, "operation": "forget", "idempotency_key": "k"});
        let response = narrowed.post(&route("notes", name), &body).await;
        fault(&response, 403, "forbidden_scope", Value::Null);
    }
}

/// Contract section 6: GetReceipt and GetErasure need only the graph grant
/// (receipts contain no evidence content); FindFacts additionally needs
/// `read` and CorrectFacts/ResumeErasure need `write` on the evidence
/// collection.
#[tokio::test]
async fn evidence_permissions_follow_the_contract_table() {
    let lookup_body = query(&json!({"agent": "a"}), "forget", "k");
    let scope = json!({"agent": "a"});
    for (ops, expected) in [
        (
            vec![Op::GraphRead],
            [
                ("receipts/lookup", 404, "receipt_not_found"),
                ("erasures/get", 409, "memory_capability_unavailable"),
                ("facts/query", 403, "forbidden_scope"),
                ("facts/correct", 403, "forbidden_scope"),
                ("erasures/resume", 403, "forbidden_scope"),
            ],
        ),
        (
            vec![Op::Read, Op::GraphRead],
            [
                ("receipts/lookup", 404, "receipt_not_found"),
                ("erasures/get", 409, "memory_capability_unavailable"),
                ("facts/query", 409, "memory_capability_unavailable"),
                ("facts/correct", 403, "forbidden_scope"),
                ("erasures/resume", 403, "forbidden_scope"),
            ],
        ),
        (
            vec![Op::Write, Op::GraphWrite],
            [
                ("receipts/lookup", 404, "receipt_not_found"),
                ("erasures/get", 409, "memory_capability_unavailable"),
                ("facts/query", 403, "forbidden_scope"),
                ("facts/correct", 409, "memory_capability_unavailable"),
                ("erasures/resume", 409, "memory_capability_unavailable"),
            ],
        ),
    ] {
        let rig = rig_for(
            database(&["notes_graph"], &["notes_ev"]),
            &[notes()],
            &ops,
            |_| {},
        );
        rig.at(1_000);
        for (name, status, code) in expected {
            let body = if name == "receipts/lookup" {
                lookup_body.clone()
            } else {
                json!({"scope": scope})
            };
            let response = rig.post(&route("notes", name), &body).await;
            assert_eq!(
                (response.0, response.1["error"]["code"].as_str()),
                (status, Some(code)),
                "{ops:?} {name}: {}",
                response.1
            );
        }
    }
}

// ------------------------------------------------------------ read-only

/// Contract section 6: a receipt lookup is a read and stays available on a
/// read-only database (no writable precheck), as do the unsupported-operation
/// refusals and the read refusals; the two write refusals answer 403
/// `read_only` first.
#[tokio::test]
async fn receipt_lookup_is_served_by_a_read_only_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pins.vlite");
    let mut db = Database::create_with_options(
        &path,
        2,
        CreateOptions {
            format: Some(FormatVersion::V5),
            ..Default::default()
        },
    )
    .unwrap();
    db.create_graph("notes_graph").unwrap();
    db.create_collection(
        "notes_ev",
        CollectionSpec {
            dimensions: 2,
            settings: Default::default(),
        },
    )
    .unwrap();
    let writable = rig_for(db, &[notes()], &ALL, |_| {});
    writable.at(100);
    let scope = json!({"agent": "a"});
    let forgot = forget(&writable, "notes", &scope, "k", &["m"]).await;
    let created = consolidate(&writable, "notes", &scope, "c", "n", "s1").await;
    writable.service.close().await.unwrap();
    drop(writable);

    let db = Database::open_read_only(&path).unwrap();
    let rig = rig_for(db, &[notes()], &ALL, |_| {});
    rig.at(200);
    // The database really is read-only.
    let write = json!({"scope": scope, "idempotency_key": "x", "items": []});
    let response = rig.post(&route("notes", "remember"), &write).await;
    fault(&response, 403, "read_only", Value::Null);
    for (operation, key, original) in [("forget", "k", &forgot), ("consolidate", "c", &created)] {
        let (status, body) = lookup(&rig, &query(&scope, operation, key)).await;
        assert_eq!(status, 200, "{operation}: {body}");
        let mut expected = original.clone();
        expected["replayed"] = json!(true);
        assert_eq!(body, json!({"result": expected}));
    }
    let response = lookup(&rig, &query(&scope, "forget", "absent")).await;
    fault(&response, 404, "receipt_not_found", Value::Null);
    let response = lookup(&rig, &query(&scope, "remember", "k")).await;
    fault(
        &response,
        409,
        "memory_capability_unavailable",
        unavailable("remember_receipt_lookup"),
    );
    rig.at(60_100);
    let response = lookup(&rig, &query(&scope, "forget", "k")).await;
    fault(&response, 404, "receipt_not_found", Value::Null);
    let body = json!({"scope": scope});
    for (name, feature) in REFUSING {
        let response = rig.post(&route("notes", name), &body).await;
        match name {
            "facts/correct" | "erasures/resume" => {
                fault(&response, 403, "read_only", Value::Null);
            }
            _ => fault(
                &response,
                409,
                "memory_capability_unavailable",
                unavailable(feature),
            ),
        }
    }
    // Authorization still comes first: an ungranted scope or an unknown
    // binding is the one 403 `forbidden_scope`, never `read_only`.
    for name in ["facts/correct", "erasures/resume"] {
        let ungranted = json!({"scope": {"agent": "zz"}});
        let ungranted = rig.post(&route("notes", name), &ungranted).await;
        fault(&ungranted, 403, "forbidden_scope", Value::Null);
        let unknown = rig.post(&route("ghost", name), &body).await;
        assert_eq!(unknown, ungranted, "{name}");
    }
}
