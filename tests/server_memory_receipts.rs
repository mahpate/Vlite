#![cfg(feature = "server")]
//! Agent-memory REST adapter: receipt lookup and the four capability-refusal
//! routes (`facts/query`, `facts/correct`, `erasures/get`, `erasures/resume`)
//! behind the full server stack: authentication, route permission, binding and
//! exact-scope authorization, read-only nodes and the audit log. The receipt
//! window and expiry are driven through the clock seam in the library tests
//! (`src/server/memory/pins_receipts.rs`); the window here is one day, so the
//! real clock never matters.

use std::path::Path;

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
const ALL_OPS: [&str; 4] = ["read", "write", "graph_read", "graph_write"];

const LOOKUP: &str = "/v1/memory/notes/receipts/lookup";
/// The four always-refusing routes and the feature each names.
const REFUSING: [(&str, &str); 4] = [
    ("/v1/memory/notes/facts/query", "facts"),
    ("/v1/memory/notes/facts/correct", "facts"),
    ("/v1/memory/notes/erasures/get", "erasure_jobs"),
    ("/v1/memory/notes/erasures/resume", "erasure_jobs"),
];

fn create(path: &Path) -> Database {
    let mut db = Database::create_with_options(
        path,
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
    db
}

/// The database lineage and the evidence collection ID a credential binds.
fn identity(db: &Database) -> (u64, Value) {
    let evidence = db
        .collections()
        .unwrap()
        .into_iter()
        .find(|info| info.name == "notes_ev")
        .unwrap()
        .id;
    (db.lineage().unwrap(), json!(evidence))
}

/// A router over `service` plus a scoped `vlk_` credential holding
/// `operations` on the evidence collection and the memory graph.
fn start(
    directory: &TempDir,
    service: &Service,
    identity: &(u64, Value),
    operations: &[&str],
) -> (Router, String) {
    let secret = "A".repeat(43);
    let credentials = json!({
        "schema": 1,
        "credentials": [{
            "id": ID,
            "name": "fake memory credential",
            "all": false,
            "collections": [{"name": "notes_ev", "id": identity.1}],
            "operations": operations,
            "graphs": ["notes_graph"],
            "lineage": format!("{:016x}", identity.0),
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
        service.clone(),
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

/// A fresh in-memory deployment with one credential holding `operations`.
fn deployment(directory: &TempDir, operations: &[&str]) -> (Router, String) {
    let db = create(&directory.path().join("memory.vlite"));
    let identity = identity(&db);
    start(directory, &Service::new(db), &identity, operations)
}

async fn post(app: &Router, path: &str, token: Option<&str>, body: &Value) -> (u16, Value) {
    let mut request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let body = Body::from(serde_json::to_vec(body).unwrap());
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn lookup_body(operation: &str, key: &str) -> Value {
    json!({"scope": {"agent": "a"}, "operation": operation, "idempotency_key": key})
}

fn scope_body() -> Value {
    json!({"scope": {"agent": "a"}})
}

/// Remembers two entities, forgets `e2` (key `f-1`) and consolidates `e1`
/// into `sum1` (key `c-1`); returns both answers.
async fn seed(app: &Router, token: &str) -> (Value, Value) {
    let items: Vec<Value> = ["e1", "e2"]
        .iter()
        .map(|id| {
            json!({"id": id, "kind": "entity", "content": format!("secret text of {id}"),
                "observed_at_ms": "90", "source": {"source_id": "s1", "source_kind": "caller"}})
        })
        .collect();
    let remember = json!({"scope": {"agent": "a"}, "idempotency_key": "r-1", "items": items});
    let (status, body) = post(app, "/v1/memory/notes/remember", Some(token), &remember).await;
    assert_eq!(status, 200, "{body}");
    let forget = json!({"scope": {"agent": "a"}, "idempotency_key": "f-1",
        "target": {"ids": [{"id": "e2", "revision": "1"}]}, "mode": "logical_delete"});
    let (status, forgot) = post(app, "/v1/memory/notes/forget", Some(token), &forget).await;
    assert_eq!(status, 200, "{forgot}");
    let consolidate = json!({"scope": {"agent": "a"}, "idempotency_key": "c-1",
        "inputs": [{"id": "e1", "revision": "1"}],
        "output": {"id": "sum1", "kind": "episodic", "content": "secret summary text",
            "observed_at_ms": "90",
            "source": {"source_id": "s2", "source_kind": "caller", "transformation_id": "t1"}}});
    let (status, created) = post(
        app,
        "/v1/memory/notes/consolidate",
        Some(token),
        &consolidate,
    )
    .await;
    assert_eq!(status, 200, "{created}");
    (forgot, created)
}

fn code(response: &(u16, Value)) -> (u16, Option<&str>) {
    (response.0, response.1["error"]["code"].as_str())
}

/// Contract section 6: a receipt lookup is a read and a retained receipt
/// survives a restart. A read-only node serves it (no writable precheck), and
/// still refuses the two write routes with 403 `read_only` before their
/// capability refusal.
#[tokio::test]
async fn retained_receipts_are_served_after_a_restart_on_a_read_only_node() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("memory.vlite");
    let db = create(&path);
    let identity = identity(&db);
    let service = Service::new(db);
    let (app, token) = start(&directory, &service, &identity, &ALL_OPS);
    let (forgot, created) = seed(&app, &token).await;
    drop(app);
    service.close().await.unwrap();
    drop(service);

    let service = Service::new(Database::open_read_only(&path).unwrap());
    let (app, token) = start(&directory, &service, &identity, &ALL_OPS);
    // The node really is read-only.
    let write = json!({"scope": {"agent": "a"}, "idempotency_key": "r-2", "items": []});
    let response = post(&app, "/v1/memory/notes/remember", Some(&token), &write).await;
    assert_eq!(code(&response), (403, Some("read_only")), "{}", response.1);
    for (operation, key, original) in [("forget", "f-1", &forgot), ("consolidate", "c-1", &created)]
    {
        let body = lookup_body(operation, key);
        let (status, found) = post(&app, LOOKUP, Some(&token), &body).await;
        assert_eq!(status, 200, "{operation}: {found}");
        let mut expected = original.clone();
        expected["replayed"] = json!(true);
        assert_eq!(found, json!({"result": expected}), "{operation}");
    }
    let absent = post(&app, LOOKUP, Some(&token), &lookup_body("forget", "absent")).await;
    assert_eq!(
        code(&absent),
        (404, Some("receipt_not_found")),
        "{}",
        absent.1
    );
    let remember = post(&app, LOOKUP, Some(&token), &lookup_body("remember", "r-1")).await;
    assert_eq!(
        code(&remember),
        (409, Some("memory_capability_unavailable"))
    );
    assert_eq!(
        remember.1["error"]["details"],
        json!({"feature": "remember_receipt_lookup"})
    );
    for (path, feature) in REFUSING {
        let response = post(&app, path, Some(&token), &scope_body()).await;
        if path.ends_with("/correct") || path.ends_with("/resume") {
            assert_eq!(code(&response), (403, Some("read_only")), "{path}");
        } else {
            assert_eq!(
                code(&response),
                (409, Some("memory_capability_unavailable")),
                "{path}"
            );
            assert_eq!(response.1["error"]["details"], json!({"feature": feature}));
        }
    }
}

/// Contract section 6: GetReceipt and GetErasure need `graph_read` and the
/// exact scope only (no evidence content); FindFacts needs `graph_read` plus
/// `read`; CorrectFacts and ResumeErasure need `graph_write` plus `write` on
/// the evidence collection. Authentication comes first, and an admin or
/// read-only token holds no memory scope.
#[tokio::test]
async fn route_permissions_follow_the_contract_table() {
    // Expected (status, code) per route, in the order lookup, facts/query,
    // facts/correct, erasures/get, erasures/resume. A 403 may come from the
    // route permission or from the binding check, so its code is not pinned.
    let absent = (404, Some("receipt_not_found"));
    let refused = (409, Some("memory_capability_unavailable"));
    let denied = (403, None);
    let cases = [
        (
            vec!["graph_read"],
            [absent, denied, denied, refused, denied],
        ),
        (
            vec!["read", "search", "graph_read"],
            [absent, refused, denied, refused, denied],
        ),
        (
            vec!["graph_write"],
            [denied, denied, denied, denied, denied],
        ),
        (
            vec!["read", "write", "graph_write"],
            [denied, denied, refused, denied, refused],
        ),
    ];
    let paths = [
        LOOKUP,
        REFUSING[0].0,
        REFUSING[1].0,
        REFUSING[2].0,
        REFUSING[3].0,
    ];
    for (operations, expected) in cases {
        let directory = TempDir::new().unwrap();
        let (app, token) = deployment(&directory, &operations);
        for (path, (status, name)) in paths.iter().zip(expected) {
            let body = if *path == LOOKUP {
                lookup_body("forget", "k")
            } else {
                scope_body()
            };
            let response = post(&app, path, Some(&token), &body).await;
            assert_eq!(response.0, status, "{operations:?} {path}: {}", response.1);
            if let Some(name) = name {
                assert_eq!(response.1["error"]["code"], name, "{operations:?} {path}");
            }
        }
    }
    let directory = TempDir::new().unwrap();
    let (app, _) = deployment(&directory, &ALL_OPS);
    for path in paths {
        let body = if path == LOOKUP {
            lookup_body("forget", "k")
        } else {
            scope_body()
        };
        assert_eq!(post(&app, path, None, &body).await.0, 401, "{path}");
        let response = post(&app, path, Some(ADMIN), &body).await;
        assert_eq!(response.0, 403, "{path} admin: {}", response.1);
        let response = post(&app, path, Some(READ_ONLY), &body).await;
        assert_eq!(response.0, 403, "{path} read-only: {}", response.1);
    }
}

/// Contract section 6: one audit event per request carrying the operation,
/// binding, principal and outcome code only: never the request's keys, job
/// IDs, slots or text, whether the request was refused or looked up.
#[tokio::test]
async fn refusals_and_lookups_are_audited_by_code_only() {
    let directory = TempDir::new().unwrap();
    let (app, token) = deployment(&directory, &ALL_OPS);
    let secret = json!({"scope": {"agent": "a"}, "idempotency_key": "secret-key-9",
        "job_id": "secret-job-9", "slot": {"subject_id": "secret-subject-9"},
        "resume_token": "secret-token-9"});
    for (path, _) in REFUSING {
        let response = post(&app, path, Some(&token), &secret).await;
        assert_eq!(response.0, 409, "{path}: {}", response.1);
    }
    let lookup = lookup_body("forget", "secret-key-9");
    let response = post(&app, LOOKUP, Some(&token), &lookup).await;
    assert_eq!(code(&response), (404, Some("receipt_not_found")));
    let mut other = secret.clone();
    other["scope"] = json!({"agent": "b"});
    let response = post(&app, REFUSING[0].0, Some(&token), &other).await;
    assert_eq!(code(&response), (403, Some("forbidden_scope")));
    let audit = std::fs::read_to_string(directory.path().join("audit.jsonl")).unwrap();
    let events: Vec<Value> = audit
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["event"] == "memory_request")
        .collect();
    let outcomes: Vec<&str> = events
        .iter()
        .map(|event| event["outcome"].as_str().unwrap())
        .collect();
    assert_eq!(
        outcomes,
        [
            "memory_capability_unavailable",
            "memory_capability_unavailable",
            "memory_capability_unavailable",
            "memory_capability_unavailable",
            "receipt_not_found",
            "forbidden_scope",
        ]
    );
    for event in &events {
        assert_eq!(event["binding"], "notes");
        assert_eq!(event["principal"], ID);
        assert!(event.get("receipt_id").is_none(), "{event}");
    }
    assert!(!audit.contains("secret-"), "audit leaks request content");
}
