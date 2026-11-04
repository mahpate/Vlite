#![cfg(feature = "server")]
//! Agent-memory REST adapter: the bindings file is loaded and checked before
//! any listener serves, and memory routes run behind the shared
//! authentication, permission and body-limit middleware. Per-case wire
//! behavior is replayed from the accepted fixtures in the library tests.

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

const ADMIN: &str = "fake_memory_rest_admin_token_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const READ_ONLY: &str = "fake_memory_rest_readonly_token_0123456789_ABCDEFGHIJKLMNOP";
const ID: &str = "abcdefghij27";
const BINDINGS: &str = r#"{"version":"memory-bindings-v1","bindings":[{"binding":"notes","tenant":"@root","generation":1,"state":"active","graph":"notes_graph","evidence_collection":"notes_ev","schema_version":"memory-schema-v1","ranking_version":"memory-ranking-v1","digest_version":"memory-request-v1","receipt_window_ms":86400000,"grants":[{"principal":"abcdefghij27","scopes":[{"agent":"a"}]}]}]}"#;

fn database(graph: &str) -> Database {
    let mut db = Database::create_in_memory(
        2,
        CreateOptions {
            format: Some(FormatVersion::V5),
            ..Default::default()
        },
    )
    .unwrap();
    db.create_collection(
        "notes_ev",
        CollectionSpec {
            dimensions: 2,
            settings: Default::default(),
        },
    )
    .unwrap();
    db.create_graph(graph).unwrap();
    db
}

fn start(directory: &TempDir, db: Database, bindings: Option<&[u8]>) -> std::io::Result<Router> {
    let memory_bindings_file = bindings.map(|bytes| {
        let path = directory.path().join("memory-bindings.json");
        std::fs::write(&path, bytes).unwrap();
        path
    });
    configured_admin_router(
        Service::new(db),
        ServerOptions {
            admin_token: Some(BearerToken::new(ADMIN).unwrap()),
            admin_directory: Some(directory.path().into()),
            memory_bindings_file,
            ..Default::default()
        },
    )
}

fn refusal(result: std::io::Result<Router>) -> String {
    match result {
        Ok(_) => panic!("startup must be refused"),
        Err(error) => error.to_string(),
    }
}

async fn status(app: &Router, method: &str, path: &str) -> u16 {
    app.clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", format!("Bearer {ADMIN}"))
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
        .as_u16()
}

const MEMORY_PATHS: [&str; 3] = [
    "/v1/memory/notes/remember",
    "/v1/memory/notes/recall",
    "/v1/memory/notes/receipts/lookup",
];

/// Routes not mounted yet in this slice.
const UNMOUNTED: [&str; 2] = [
    "/v1/memory/notes/recall",
    "/v1/memory/notes/receipts/lookup",
];

#[tokio::test]
async fn without_a_bindings_file_memory_routes_are_not_mounted() {
    let directory = TempDir::new().unwrap();
    let app = start(&directory, database("notes_graph"), None).unwrap();
    for path in MEMORY_PATHS {
        assert_eq!(status(&app, "POST", path).await, 404, "{path}");
    }
    assert_eq!(status(&app, "GET", "/v1/graphs/notes_graph").await, 200);
}

#[tokio::test]
async fn a_valid_bindings_file_mounts_only_the_served_routes() {
    let directory = TempDir::new().unwrap();
    let app = start(
        &directory,
        database("notes_graph"),
        Some(BINDINGS.as_bytes()),
    )
    .unwrap();
    // The admin role holds no memory grant: mounted, then refused.
    assert_eq!(status(&app, "POST", MEMORY_PATHS[0]).await, 403);
    for path in UNMOUNTED {
        assert_eq!(status(&app, "POST", path).await, 404, "{path}");
    }
}

#[test]
fn an_invalid_bindings_file_refuses_startup_without_echoing_it() {
    let directory = TempDir::new().unwrap();
    let secret = BINDINGS.replace("memory-bindings-v1", "vlk_secret_material");
    let message = refusal(start(
        &directory,
        database("notes_graph"),
        Some(secret.as_bytes()),
    ));
    assert_eq!(message, "memory bindings file is invalid");
}

#[test]
fn a_missing_bound_graph_refuses_startup() {
    let directory = TempDir::new().unwrap();
    let message = refusal(start(
        &directory,
        database("other_graph"),
        Some(BINDINGS.as_bytes()),
    ));
    assert_eq!(message, "a memory binding graph does not exist");
}

#[test]
fn an_ordinary_bound_graph_refuses_startup() {
    let directory = TempDir::new().unwrap();
    let mut db = database("notes_graph");
    db.graph_mut("notes_graph")
        .unwrap()
        .write_batch(&[vlite::property_graph::GraphOp::CreateNode {
            key: "ordinary".into(),
            labels: vec!["Person".into()],
            properties: Default::default(),
        }])
        .unwrap();
    let message = refusal(start(&directory, db, Some(BINDINGS.as_bytes())));
    assert_eq!(
        message,
        "a memory binding resource already holds ordinary data"
    );
}

/// A scoped `vlk_` credential bound to the evidence collection and graph.
fn start_scoped(
    directory: &TempDir,
    operations: &[&str],
    lineage: Option<u64>,
) -> (Router, String) {
    start_scoped_db(directory, database("notes_graph"), operations, lineage)
}

fn start_scoped_db(
    directory: &TempDir,
    db: Database,
    operations: &[&str],
    lineage: Option<u64>,
) -> (Router, String) {
    let secret = "A".repeat(43);
    let evidence = db
        .collections()
        .unwrap()
        .into_iter()
        .find(|info| info.name == "notes_ev")
        .unwrap()
        .id;
    let lineage = lineage.unwrap_or_else(|| db.lineage().unwrap());
    let credentials = json!({
        "schema": 1,
        "credentials": [{
            "id": ID,
            "name": "fake memory credential",
            "all": false,
            "collections": [{"name": "notes_ev", "id": evidence}],
            "operations": operations,
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
    std::fs::write(&path, BINDINGS).unwrap();
    let app = configured_admin_router(
        Service::new(db),
        ServerOptions {
            admin_token: Some(BearerToken::new(ADMIN).unwrap()),
            read_only_token: Some(BearerToken::new(READ_ONLY).unwrap()),
            admin_directory: Some(directory.path().into()),
            memory_bindings_file: Some(path),
            ..Default::default()
        },
    )
    .unwrap();
    (app, format!("vlk_{ID}_{secret}"))
}

const ALL_OPS: [&str; 4] = ["read", "write", "graph_read", "graph_write"];

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    content_type: &str,
    body: Vec<u8>,
) -> (u16, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", content_type);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

fn remember_body(key: &str) -> Value {
    json!({
        "scope": {"agent": "a"},
        "idempotency_key": key,
        "items": [{
            "id": "e1",
            "kind": "entity",
            "content": "secret memory text",
            "observed_at_ms": "90",
            "source": {"source_id": "s1", "source_kind": "caller"}
        }]
    })
}

async fn remember(app: &Router, token: Option<&str>, body: &Value) -> (u16, Value) {
    call(
        app,
        "POST",
        MEMORY_PATHS[0],
        token,
        "application/json",
        serde_json::to_vec(body).unwrap(),
    )
    .await
}

#[tokio::test]
async fn a_granted_scope_remembers_and_replays_with_a_content_free_audit() {
    let directory = TempDir::new().unwrap();
    let (app, token) = start_scoped(&directory, &ALL_OPS, None);
    let (status, first) = remember(&app, Some(&token), &remember_body("key-1")).await;
    assert_eq!(status, 200, "{first}");
    assert_eq!(first["replayed"], false);
    assert_eq!(
        first["outcomes"],
        json!([{"id": "e1", "action": "created", "revision": "1"}])
    );
    assert_eq!(first["receipt"]["operation"], "remember");
    assert_eq!(
        first["receipt"]["scope"],
        json!({"tenant": "@root", "agent": "a"})
    );
    let (status, again) = remember(&app, Some(&token), &remember_body("key-1")).await;
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["replayed"], true);
    assert_eq!(again["receipt"], first["receipt"]);
    let audit = std::fs::read_to_string(directory.path().join("audit.jsonl")).unwrap();
    let events: Vec<Value> = audit
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["event"] == "memory_request")
        .collect();
    assert_eq!(events.len(), 2);
    for event in &events {
        assert_eq!(event["operation"], "remember");
        assert_eq!(event["binding"], "notes");
        assert_eq!(event["principal"], ID);
        assert_eq!(event["outcome"], "ok");
        assert_eq!(event["receipt_id"], first["receipt"]["id"]);
    }
    for secret in ["secret memory text", "key-1", "\"e1\"", "\"s1\"", &token] {
        assert!(!audit.contains(secret), "audit leaks {secret}");
    }
}

#[tokio::test]
async fn refusals_are_audited_by_code_only() {
    let directory = TempDir::new().unwrap();
    let (app, token) = start_scoped(&directory, &ALL_OPS, None);
    let mut body = remember_body("key-2");
    body["scope"] = json!({"agent": "b"});
    let (status, error) = remember(&app, Some(&token), &body).await;
    assert_eq!(status, 403, "{error}");
    assert_eq!(error["error"]["code"], "forbidden_scope");
    let audit = std::fs::read_to_string(directory.path().join("audit.jsonl")).unwrap();
    let event = audit
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|event| event["event"] == "memory_request")
        .unwrap();
    assert_eq!(event["outcome"], "forbidden_scope");
    assert!(event.get("receipt_id").is_none());
    assert!(!audit.contains("key-2"));
}

#[tokio::test]
async fn an_unknown_binding_is_indistinguishable_from_an_ungranted_scope() {
    let directory = TempDir::new().unwrap();
    let (app, token) = start_scoped(&directory, &ALL_OPS, None);
    let mut ungranted = remember_body("k");
    ungranted["scope"] = json!({"agent": "b"});
    let (status, ungranted) = remember(&app, Some(&token), &ungranted).await;
    let (unknown_status, unknown) = call(
        &app,
        "POST",
        "/v1/memory/absent/remember",
        Some(&token),
        "application/json",
        serde_json::to_vec(&remember_body("k")).unwrap(),
    )
    .await;
    assert_eq!((status, unknown_status), (403, 403));
    assert_eq!(ungranted, unknown);
}

#[tokio::test]
async fn middleware_refusals_precede_the_memory_handler() {
    let directory = TempDir::new().unwrap();
    let (app, _) = start_scoped(&directory, &ALL_OPS, None);
    let body = remember_body("k");
    assert_eq!(remember(&app, None, &body).await.0, 401);
    let (status, admin) = remember(&app, Some(ADMIN), &body).await;
    assert_eq!(
        (status, &admin["error"]["code"]),
        (403, &json!("forbidden_scope"))
    );
    let (status, read_only) = remember(&app, Some(READ_ONLY), &body).await;
    assert_eq!(status, 403, "{read_only}");
    let directory = TempDir::new().unwrap();
    let (app, reader) = start_scoped(&directory, &["read", "graph_read"], None);
    assert_eq!(remember(&app, Some(&reader), &body).await.0, 403);
    let directory = TempDir::new().unwrap();
    let (app, foreign) = start_scoped(&directory, &ALL_OPS, Some(7));
    assert_eq!(remember(&app, Some(&foreign), &body).await.0, 403);
}

#[tokio::test]
async fn method_media_type_and_size_are_enforced() {
    let directory = TempDir::new().unwrap();
    let (app, token) = start_scoped(&directory, &ALL_OPS, None);
    let body = serde_json::to_vec(&remember_body("k")).unwrap();
    let (status, _) = call(
        &app,
        "GET",
        MEMORY_PATHS[0],
        Some(&token),
        "application/json",
        Vec::new(),
    )
    .await;
    assert_eq!(status, 405);
    let (status, error) = call(
        &app,
        "POST",
        MEMORY_PATHS[0],
        Some(&token),
        "text/plain",
        body.clone(),
    )
    .await;
    assert_eq!(
        (status, &error["error"]["code"]),
        (415, &json!("invalid_json"))
    );
    let mut padded = body.clone();
    padded.resize(8 * 1024 * 1024 + 1, b' ');
    let (status, _) = call(
        &app,
        "POST",
        MEMORY_PATHS[0],
        Some(&token),
        "application/json",
        padded,
    )
    .await;
    assert_eq!(status, 413);
    let mut padded = body;
    padded.resize(8 * 1024 * 1024, b' ');
    let (status, ok) = call(
        &app,
        "POST",
        MEMORY_PATHS[0],
        Some(&token),
        "application/json",
        padded,
    )
    .await;
    assert_eq!(status, 200, "{ok}");
}

#[tokio::test]
async fn hostile_bodies_never_panic() {
    let directory = TempDir::new().unwrap();
    let (app, token) = start_scoped(&directory, &ALL_OPS, None);
    let cases: [&[u8]; 8] = [
        b"",
        b"null",
        b"[]",
        b"{\"scope\":",
        b"{\"scope\":{\"agent\":\"a\"},\"scope\":{\"agent\":\"a\"}}",
        b"\xff\xfe",
        b"{\"scope\":{\"agent\":\"a\"},\"idempotency_key\":\"k\",\"items\":[[[[[[]]]]]]}",
        b"{\"scope\":{\"agent\":\"a\"},\"idempotency_key\":\"k\",\"items\":[{\"id\":\"e\",\"kind\":\"entity\",\"content\":\"x\",\"observed_at_ms\":\"99999999999999999999999\",\"source\":{\"source_id\":\"s\",\"source_kind\":\"c\"}}]}",
    ];
    for body in cases {
        let (status, error) = call(
            &app,
            "POST",
            MEMORY_PATHS[0],
            Some(&token),
            "application/json",
            body.to_vec(),
        )
        .await;
        assert!(matches!(status, 400 | 422), "{status} {error}");
    }
}

#[tokio::test]
async fn the_evidence_collection_needs_its_own_write_grant() {
    let directory = TempDir::new().unwrap();
    let (app, token) = start_scoped(&directory, &["read", "graph_read", "graph_write"], None);
    let (status, error) = remember(&app, Some(&token), &remember_body("k")).await;
    assert_eq!(
        (status, &error["error"]["code"]),
        (403, &json!("forbidden_scope"))
    );
}

#[tokio::test]
async fn a_new_reference_must_name_an_item_of_the_same_request() {
    let directory = TempDir::new().unwrap();
    let (app, token) = start_scoped(&directory, &ALL_OPS, None);
    let mut body = remember_body("k");
    body["items"][0]["references"] = json!([{"id": "elsewhere", "revision": "0"}]);
    let (status, error) = remember(&app, Some(&token), &body).await;
    assert_eq!(
        (status, &error["error"]["details"]),
        (422, &json!({"field": "references"}))
    );
    body["items"][0]["references"] = json!([{"id": "e1", "revision": "0"}]);
    let (status, ok) = remember(&app, Some(&token), &body).await;
    assert_eq!(status, 200, "{ok}");
}

#[tokio::test]
async fn a_read_only_database_refuses_writes_before_validation() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("memory.vlite");
    let mut db = Database::create_with_options(
        &path,
        2,
        CreateOptions {
            format: Some(FormatVersion::V5),
            ..Default::default()
        },
    )
    .unwrap();
    db.create_collection(
        "notes_ev",
        CollectionSpec {
            dimensions: 2,
            settings: Default::default(),
        },
    )
    .unwrap();
    db.create_graph("notes_graph").unwrap();
    db.close().unwrap();
    let db = Database::open_read_only(&path).unwrap();
    let (app, token) = start_scoped_db(&directory, db, &ALL_OPS, None);
    let (status, error) = remember(&app, Some(&token), &json!({"scope": {"agent": "a"}})).await;
    assert_eq!(
        (status, &error["error"]["code"]),
        (403, &json!("read_only")),
        "{error}"
    );
}
