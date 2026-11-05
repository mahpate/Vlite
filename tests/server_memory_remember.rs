#![cfg(feature = "server")]
//! Full-stack pins for the agent-memory Remember route: real scoped
//! credentials, the shared authentication, permission and body-limit
//! middleware, the read-only node rule and the audit log in front of and
//! around the handler. The wire pins that need a controlled clock live in
//! `src/server/memory/pins_remember.rs`. Expectations come from the serving
//! contract sections 3, 4 and 6 and the adapter mapping sections 1, 2 and 4.

use axum::{Router, body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tower::ServiceExt;
use vlite::{
    CollectionSpec, CreateOptions, Database, FormatVersion,
    server::{BearerToken, ServerOptions, Service, configured_admin_router},
};

const ADMIN: &str = "fake_memory_remember_admin_token_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const ID: &str = "abcdefghij27";
const REMEMBER: &str = "/v1/memory/notes/remember";
const NEEDLE: &str = "sentinel-5d1c9e";

fn options() -> CreateOptions {
    CreateOptions {
        format: Some(FormatVersion::V5),
        ..Default::default()
    }
}

fn provision(db: &mut Database) {
    db.create_collection(
        "notes_ev",
        CollectionSpec {
            dimensions: 2,
            settings: Default::default(),
        },
    )
    .unwrap();
    db.create_graph("notes_graph").unwrap();
}

fn writable() -> Database {
    let mut db = Database::create_in_memory(2, options()).unwrap();
    provision(&mut db);
    db
}

fn read_only(directory: &TempDir) -> Database {
    let path = directory.path().join("memory.vlite");
    let mut db = Database::create_with_options(&path, 2, options()).unwrap();
    provision(&mut db);
    db.close().unwrap();
    Database::open_read_only(&path).unwrap()
}

fn bindings(scopes: &str) -> String {
    format!(
        r#"{{"version":"memory-bindings-v1","bindings":[{{"binding":"notes","tenant":"@root","generation":1,"state":"active","graph":"notes_graph","evidence_collection":"notes_ev","schema_version":"memory-schema-v1","ranking_version":"memory-ranking-v1","digest_version":"memory-request-v1","receipt_window_ms":86400000,"grants":[{{"principal":"{ID}","scopes":{scopes}}}]}}]}}"#
    )
}

/// A scoped `vlk_` credential with every memory operation, granted `scopes`
/// (a JSON array) on the binding `notes`.
fn start(directory: &TempDir, db: Database, scopes: &str) -> (Router, String) {
    let secret = "A".repeat(43);
    let evidence = db
        .collections()
        .unwrap()
        .into_iter()
        .find(|info| info.name == "notes_ev")
        .unwrap()
        .id;
    let lineage = db.lineage().unwrap();
    let credentials = json!({
        "schema": 1,
        "credentials": [{
            "id": ID,
            "name": "fake memory credential",
            "all": false,
            "collections": [{"name": "notes_ev", "id": evidence}],
            "operations": ["read", "write", "graph_read", "graph_write"],
            "graphs": ["notes_graph"],
            "lineage": format!("{lineage:016x}"),
            "secret_sha256": format!("{:x}", Sha256::digest(secret.as_bytes())),
            "created_unix_ms": 0,
            "expires_unix_ms": 9_999_999_999_999u64,
            "revoked_unix_ms": null,
            "rotated_from": null
        }]
    });
    std::fs::write(
        directory.path().join("credentials.json"),
        serde_json::to_vec(&credentials).unwrap(),
    )
    .unwrap();
    let path = directory.path().join("memory-bindings.json");
    std::fs::write(&path, bindings(scopes)).unwrap();
    let app = configured_admin_router(
        Service::new(db),
        ServerOptions {
            admin_token: Some(BearerToken::new(ADMIN).unwrap()),
            admin_directory: Some(directory.path().into()),
            memory_bindings_file: Some(path),
            ..Default::default()
        },
    )
    .unwrap();
    (app, format!("vlk_{ID}_{secret}"))
}

async fn send(app: &Router, token: &str, content_type: &str, body: Vec<u8>) -> (u16, String) {
    let request = Request::builder()
        .method("POST")
        .uri(REMEMBER)
        .header("content-type", content_type)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn post(app: &Router, token: &str, body: &Value) -> (u16, Value) {
    let (status, text) = send(
        app,
        token,
        "application/json",
        serde_json::to_vec(body).unwrap(),
    )
    .await;
    let value = serde_json::from_str(&text).unwrap_or_else(|error| panic!("{error}: {text}"));
    (status, value)
}

fn item(id: &str, content: &str) -> Value {
    json!({
        "id": id,
        "kind": "entity",
        "content": content,
        "observed_at_ms": "90",
        "source": {"source_id": "s1", "source_kind": "caller"},
    })
}

fn with(mut base: Value, extra: Value) -> Value {
    base.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    base
}

/// A generous budget keeps a slow machine from expiring a served request.
fn request(key: &str, items: Vec<Value>) -> Value {
    json!({
        "scope": {"agent": "a"},
        "idempotency_key": key,
        "deadline_ms": 5000,
        "items": items,
    })
}

fn code(answer: &Value) -> &str {
    answer["error"]["code"].as_str().unwrap_or_default()
}

fn audit_events(directory: &TempDir) -> Vec<Value> {
    std::fs::read_to_string(directory.path().join("audit.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["event"] == "memory_request")
        .collect()
}

/// Mapping section 2 check order behind the real middleware: the exact scope
/// (step 2) precedes the writable precheck (step 3), which precedes capability
/// refusals (step 4), which precede request validation (step 5).
#[tokio::test]
async fn the_check_order_holds_behind_the_real_middleware() {
    let refused = request("k", vec![with(item("e1", "x"), json!({"kind": "fact"}))]);
    let invalid = with(request("k", vec![item("e1", "x")]), json!({"unknown": 1}));
    let ungranted = |body: &Value| with(body.clone(), json!({"scope": {"agent": "b"}}));

    let directory = TempDir::new().unwrap();
    let (app, token) = start(&directory, read_only(&directory), r#"[{"agent":"a"}]"#);
    for body in [&refused, &invalid] {
        let (status, answer) = post(&app, &token, body).await;
        assert_eq!((status, code(&answer)), (403, "read_only"), "{answer}");
    }
    // An ungranted scope is refused before the node's mode is considered.
    for body in [ungranted(&refused), ungranted(&invalid)] {
        let (status, answer) = post(&app, &token, &body).await;
        assert_eq!(
            (status, code(&answer)),
            (403, "forbidden_scope"),
            "{answer}"
        );
    }

    let directory = TempDir::new().unwrap();
    let (app, token) = start(&directory, writable(), r#"[{"agent":"a"}]"#);
    let (status, answer) = post(&app, &token, &ungranted(&refused)).await;
    assert_eq!(
        (status, code(&answer)),
        (403, "forbidden_scope"),
        "{answer}"
    );
    // A refusal precedes the validation of the idempotency key and budgets.
    for broken in [
        json!({"idempotency_key": ""}),
        json!({"deadline_ms": 0}),
        json!({"max_references": 257}),
    ] {
        let (status, answer) = post(&app, &token, &with(refused.clone(), broken)).await;
        assert_eq!(status, 409, "{answer}");
        assert_eq!(code(&answer), "memory_capability_unavailable", "{answer}");
        assert_eq!(answer["error"]["details"], json!({"feature": "fact_kinds"}));
    }
    // Validation still runs when nothing is refused.
    let (status, answer) = post(&app, &token, &invalid).await;
    assert_eq!((status, code(&answer)), (422, "invalid_input"), "{answer}");
}

/// Contract section 3: the identity includes the exact scope. With real
/// credentials, two granted scopes sharing a key stay two executions.
#[tokio::test]
async fn exact_scopes_stay_separate_identities_for_a_real_credential() {
    let directory = TempDir::new().unwrap();
    let scopes =
        r#"[{"agent":"a"},{"agent":"a","user":"u"},{"agent":"a","user":"u","session":"s"}]"#;
    let (app, token) = start(&directory, writable(), scopes);
    let granted = [
        json!({"agent": "a"}),
        json!({"agent": "a", "user": "u"}),
        json!({"agent": "a", "user": "u", "session": "s"}),
    ];
    let mut receipts = Vec::new();
    for scope in &granted {
        let body = with(
            request("shared", vec![item("e1", "Ada")]),
            json!({"scope": scope}),
        );
        let (status, answer) = post(&app, &token, &body).await;
        assert_eq!(status, 200, "{scope}: {answer}");
        assert_eq!(answer["replayed"], false, "{scope}");
        let mut expected = scope.clone();
        expected["tenant"] = json!("@root");
        assert_eq!(answer["receipt"]["scope"], expected, "{scope}");
        receipts.push(answer["receipt"].clone());
    }
    assert!(receipts[0]["id"] != receipts[1]["id"]);
    assert!(receipts[1]["id"] != receipts[2]["id"]);
    assert!(receipts[0]["id"] != receipts[2]["id"]);
    for (scope, receipt) in granted.iter().zip(&receipts) {
        let body = with(
            request("shared", vec![item("e1", "Ada")]),
            json!({"scope": scope}),
        );
        let (status, answer) = post(&app, &token, &body).await;
        assert_eq!(status, 200, "{scope}: {answer}");
        assert_eq!(answer["replayed"], true, "{scope}");
        assert_eq!(answer["receipt"], *receipt, "{scope}");
    }
}

/// Contract section 6: the mutation audit records the opaque receipt ID and
/// the committed sequence of a served request and only the outcome code of a
/// failed one, never keys, identifiers or memory text.
#[tokio::test]
async fn the_audit_names_the_receipt_and_the_sequence_and_no_content() {
    let directory = TempDir::new().unwrap();
    let (app, token) = start(&directory, writable(), r#"[{"agent":"a"}]"#);
    let key = format!("{NEEDLE}-key");
    let served = request(
        &key,
        vec![item(&format!("{NEEDLE}-id"), &format!("{NEEDLE}-text"))],
    );
    let (status, first) = post(&app, &token, &served).await;
    assert_eq!(status, 200, "{first}");
    let (status, replay) = post(&app, &token, &served).await;
    assert_eq!(status, 200, "{replay}");
    assert_eq!(replay["replayed"], true);
    let conflicting = request(&key, vec![item(&format!("{NEEDLE}-id"), "other text")]);
    let refused = request(
        &key,
        vec![with(
            item(&format!("{NEEDLE}-id"), "x"),
            json!({"kind": "fact"}),
        )],
    );
    let ungranted = with(served.clone(), json!({"scope": {"agent": NEEDLE}}));
    let invalid = request(
        &key,
        vec![with(
            item(&format!("{NEEDLE}-id"), "x"),
            json!({"kind": NEEDLE}),
        )],
    );
    for (body, expected) in [
        (&conflicting, "idempotency_conflict"),
        (&refused, "memory_capability_unavailable"),
        (&ungranted, "forbidden_scope"),
        (&invalid, "invalid_input"),
    ] {
        let (status, answer) = post(&app, &token, body).await;
        assert_eq!(code(&answer), expected, "{status} {answer}");
        assert!(!answer.to_string().contains(NEEDLE), "{answer}");
    }
    let malformed = format!(r#"{{"scope":{{"agent":"a"}},"idempotency_key":"{NEEDLE}"#);
    let (status, text) = send(&app, &token, "application/json", malformed.into_bytes()).await;
    assert_eq!(status, 400, "{text}");
    assert!(!text.contains(NEEDLE), "{text}");

    let audit = std::fs::read_to_string(directory.path().join("audit.jsonl")).unwrap();
    assert!(!audit.contains(NEEDLE), "the audit leaks request content");
    let events = audit_events(&directory);
    let outcomes: Vec<&str> = events
        .iter()
        .map(|event| event["outcome"].as_str().unwrap())
        .collect();
    assert_eq!(
        outcomes,
        [
            "ok",
            "ok",
            "idempotency_conflict",
            "memory_capability_unavailable",
            "forbidden_scope",
            "invalid_input",
            "invalid_json",
        ]
    );
    for event in &events[..2] {
        assert_eq!(event["operation"], "remember");
        assert_eq!(event["receipt_id"], first["receipt"]["id"]);
        assert_eq!(event["sequence"], first["receipt"]["sequence"]);
    }
    for event in &events[2..] {
        assert!(event.get("receipt_id").is_none(), "{event}");
        assert!(event.get("sequence").is_none(), "{event}");
    }
}

/// Contract sections 3 and 6: a body failure of the shared middleware (415
/// media type, 413 size) and of the strict parser (400) carries a static
/// message and none of the request.
#[tokio::test]
async fn body_failures_never_echo_the_request() {
    let directory = TempDir::new().unwrap();
    let (app, token) = start(&directory, writable(), r#"[{"agent":"a"}]"#);
    let valid = serde_json::to_vec(&request(NEEDLE, vec![item(NEEDLE, NEEDLE)])).unwrap();
    let (status, text) = send(&app, &token, "text/plain", valid.clone()).await;
    assert_eq!(status, 415, "{text}");
    assert!(!text.contains(NEEDLE), "{text}");
    let mut padded = valid.clone();
    padded.resize(8 * 1024 * 1024 + 1, b' ');
    let (status, text) = send(&app, &token, "application/json", padded).await;
    assert_eq!(status, 413, "{text}");
    assert!(!text.contains(NEEDLE), "{text}");
    let duplicate = format!(r#"{{"scope":{{"agent":"a"}},"scope":{{"agent":"{NEEDLE}"}}}}"#);
    for body in [
        duplicate.into_bytes(),
        [valid.as_slice(), b" ", NEEDLE.as_bytes()].concat(),
        [b"{\"x\":\"".as_slice(), NEEDLE.as_bytes(), &[0xff], b"\"}"].concat(),
    ] {
        let (status, text) = send(&app, &token, "application/json", body).await;
        assert_eq!(status, 400, "{text}");
        assert!(!text.contains(NEEDLE), "{text}");
        let answer: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(code(&answer), "invalid_json");
    }
    // The control: the request itself is served.
    let (status, answer) = send(&app, &token, "application/json", valid).await;
    assert_eq!(status, 200, "{answer}");
}
