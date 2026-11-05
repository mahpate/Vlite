#![cfg(feature = "server")]
//! The audit `actor` on agent-memory requests: every `memory_request` event
//! names its caller with the shared encoding on every memory route, next to
//! the unchanged `principal` role label. The expected values come from the
//! documented encodings (`docs/SECURITY.md`, "Audit actors"), not from the
//! code: the credential ID, `admin_token` and `user:<account id>`.

use axum::{
    Router,
    body::Body,
    http::{Request, header},
};
use base64::{
    Engine,
    prelude::{BASE64_STANDARD, BASE64_URL_SAFE_NO_PAD},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tower::ServiceExt;
use vlite::{
    CollectionSpec, CreateOptions, Database, FormatVersion,
    server::{BearerToken, ServerOptions, Service, configured_admin_router},
};

const ADMIN: &str = "fake_memory_actor_admin_token_0123456789_ABCDEFGHIJKLMNOPQRSTUV";
const ID: &str = "abcdefghij27";
const SECRET: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const PASSWORD: &str = "correct-horse-battery-staple";
const BINDINGS: &str = r#"{"version":"memory-bindings-v1","bindings":[{"binding":"notes","tenant":"@root","generation":1,"state":"active","graph":"notes_graph","evidence_collection":"notes_ev","schema_version":"memory-schema-v1","ranking_version":"memory-ranking-v1","digest_version":"memory-request-v1","receipt_window_ms":86400000,"grants":[{"principal":"abcdefghij27","scopes":[{"agent":"a"}]}]}]}"#;

/// Every memory route with the operation name its audit event records.
const ROUTES: [(&str, &str); 9] = [
    ("/v1/memory/notes/remember", "remember"),
    ("/v1/memory/notes/recall", "recall"),
    ("/v1/memory/notes/consolidate", "consolidate"),
    ("/v1/memory/notes/forget", "forget"),
    ("/v1/memory/notes/receipts/lookup", "receipt"),
    ("/v1/memory/notes/facts/query", "find_facts"),
    ("/v1/memory/notes/facts/correct", "correct_facts"),
    ("/v1/memory/notes/erasures/get", "get_erasure"),
    ("/v1/memory/notes/erasures/resume", "resume_erasure"),
];

/// How a request authenticates.
#[derive(Clone)]
enum Auth {
    /// `Authorization: Bearer`: the bootstrap token or a scoped credential.
    Bearer(String),
    /// A dashboard cookie session on a same-origin request.
    Session(String),
}

struct Reply {
    status: u16,
    body: Value,
    request_id: String,
    cookies: Vec<String>,
}

fn database() -> Database {
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
    db.create_graph("notes_graph").unwrap();
    db
}

/// A server with one binding and one stored credential, `ID`, granted on it.
fn start(directory: &TempDir) -> Router {
    let db = database();
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
            "secret_sha256": format!("{:x}", Sha256::digest(SECRET.as_bytes())),
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
    let bindings = directory.path().join("memory-bindings.json");
    std::fs::write(&bindings, BINDINGS).unwrap();
    configured_admin_router(
        Service::new(db),
        ServerOptions {
            admin_token: Some(BearerToken::new(ADMIN).unwrap()),
            admin_directory: Some(directory.path().into()),
            memory_bindings_file: Some(bindings),
            loopback_only: true,
            ..Default::default()
        },
    )
    .unwrap()
}

async fn request(app: &Router, headers: &[(&str, String)], path: &str, input: &Value) -> Reply {
    let mut request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, value.as_str());
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(input.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let request_id = response
        .headers()
        .get("x-request-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let cookies = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap().to_owned())
        .collect();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        request_id,
        cookies,
    }
}

async fn send(app: &Router, auth: &Auth, path: &str, input: &Value) -> Reply {
    match auth {
        Auth::Bearer(token) => {
            let bearer = format!("Bearer {token}");
            request(app, &[("authorization", bearer)], path, input).await
        }
        Auth::Session(session) => {
            let headers = [
                ("cookie", format!("vlite_session={session}")),
                ("sec-fetch-site", "same-origin".to_owned()),
                ("x-vlite-request", "1".to_owned()),
            ];
            request(app, &headers, path, input).await
        }
    }
}

fn admin() -> Auth {
    Auth::Bearer(ADMIN.to_owned())
}

fn credential() -> Auth {
    Auth::Bearer(format!("vlk_{ID}_{SECRET}"))
}

/// Creates a dashboard account through the admin API and returns its ID.
async fn create_account(app: &Router, username: &str, role: &str) -> String {
    let input = json!({"username": username, "password": PASSWORD, "role": role,
        "must_change_password": false});
    let reply = send(app, &admin(), "/v1/admin/users", &input).await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    reply.body["user"]["id"].as_str().unwrap().to_owned()
}

/// Signs in like the dashboard page and returns the new session's cookie value.
async fn sign_in(app: &Router, username: &str) -> String {
    let headers = [
        ("sec-fetch-site", "same-origin".to_owned()),
        ("x-vlite-request", "1".to_owned()),
    ];
    let input = json!({"username": username, "password": PASSWORD});
    let reply = request(app, &headers, "/v1/auth/login", &input).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    reply
        .cookies
        .iter()
        .find_map(|cookie| {
            let (pair, _) = cookie.split_once(';')?;
            let (name, value) = pair.split_once('=')?;
            (name == "vlite_session" && !value.is_empty()).then(|| value.to_owned())
        })
        .expect("a session cookie")
}

fn remember_body() -> Value {
    json!({
        "scope": {"agent": "a"},
        "idempotency_key": "actor-key-1",
        "deadline_ms": 5000,
        "items": [{
            "id": "e1",
            "kind": "entity",
            "content": "private memory text",
            "observed_at_ms": "90",
            "source": {"source_id": "s1", "source_kind": "caller"}
        }]
    })
}

/// A body that reaches the route's own checks. Only `remember` needs a valid
/// one; with a granted scope the facts and erasure routes answer their 409
/// capability refusal and the others a validation error, all audited.
fn body_for(operation: &str) -> Value {
    if operation == "remember" {
        remember_body()
    } else {
        json!({"scope": {"agent": "a"}})
    }
}

fn audit_log(directory: &TempDir) -> String {
    std::fs::read_to_string(directory.path().join("audit.jsonl")).unwrap()
}

/// The single `memory_request` event written for one request.
fn memory_event(log: &str, request_id: &str) -> Value {
    let matching: Vec<Value> = log
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["event"] == "memory_request" && event["request_id"] == request_id)
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "one memory event for request {request_id}"
    );
    matching.into_iter().next().unwrap()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Every rendering of a session cookie that could leak: the cookie value and
/// the digest the server keeps as the session key.
fn renderings(session: &str) -> Vec<String> {
    let key = Sha256::digest(session.as_bytes());
    vec![
        session.to_owned(),
        hex(&key),
        hex(&key).to_uppercase(),
        BASE64_STANDARD.encode(key),
        BASE64_URL_SAFE_NO_PAD.encode(key),
    ]
}

#[tokio::test]
async fn every_memory_route_names_the_scoped_credential_as_the_actor() {
    let directory = TempDir::new().unwrap();
    let app = start(&directory);
    let mut sent = Vec::new();
    for (path, operation) in ROUTES {
        let reply = send(&app, &credential(), path, &body_for(operation)).await;
        sent.push((path, operation, reply));
    }
    let log = audit_log(&directory);
    for (path, operation, reply) in &sent {
        let event = memory_event(&log, &reply.request_id);
        assert_eq!(event["operation"], *operation, "{path}");
        // A credential's actor is its ID, which is also its `principal`.
        assert_eq!(event["actor"], ID, "{path}");
        assert_eq!(event["principal"], ID, "{path}");
    }
    // A served request and a refused one carry the actor alike.
    let remembered = &sent[0].2;
    assert_eq!(remembered.status, 200, "{}", remembered.body);
    let event = memory_event(&log, &remembered.request_id);
    assert_eq!(event["outcome"], "ok");
    let refused = sent
        .iter()
        .find(|(_, operation, _)| *operation == "find_facts")
        .unwrap();
    assert_eq!(refused.2.status, 409, "{}", refused.2.body);
    assert_ne!(memory_event(&log, &refused.2.request_id)["outcome"], "ok");
}

#[tokio::test]
async fn the_bootstrap_token_and_each_super_admin_session_are_told_apart() {
    let directory = TempDir::new().unwrap();
    let app = start(&directory);
    let root_a = create_account(&app, "root.a", "super_admin").await;
    let root_b = create_account(&app, "root.b", "super_admin").await;
    let a = sign_in(&app, "root.a").await;
    let a_again = sign_in(&app, "root.a").await;
    let b = sign_in(&app, "root.b").await;
    assert_ne!(a, a_again, "two sessions of one account");

    let callers = [
        ("bootstrap token", admin(), "admin_token".to_owned()),
        ("root.a", Auth::Session(a.clone()), format!("user:{root_a}")),
        (
            "root.a again",
            Auth::Session(a_again.clone()),
            format!("user:{root_a}"),
        ),
        ("root.b", Auth::Session(b.clone()), format!("user:{root_b}")),
    ];
    let mut sent = Vec::new();
    for (path, operation) in ROUTES {
        for (label, auth, actor) in &callers {
            let reply = send(&app, auth, path, &body_for(operation)).await;
            sent.push((path, operation, *label, actor, reply));
        }
    }
    let log = audit_log(&directory);
    for (path, operation, label, actor, reply) in &sent {
        // A memory route serves scoped credentials only.
        assert_eq!(reply.status, 403, "{label} {path}: {}", reply.body);
        let event = memory_event(&log, &reply.request_id);
        assert_eq!(event["operation"], *operation, "{label} {path}");
        assert_eq!(event["outcome"], "forbidden_scope", "{label} {path}");
        // `principal` still cannot tell them apart; `actor` can.
        assert_eq!(event["principal"], "admin", "{label} {path}");
        assert_eq!(event["actor"], actor.as_str(), "{label} {path}");
    }
    let distinct: std::collections::BTreeSet<&str> =
        callers.iter().map(|(_, _, actor)| actor.as_str()).collect();
    assert_eq!(distinct.len(), 3, "one actor per identity, not per session");
    assert_ne!(root_a, root_b);
}

#[tokio::test]
async fn a_signed_in_editor_is_audited_by_its_account() {
    let directory = TempDir::new().unwrap();
    let app = start(&directory);
    let editor = create_account(&app, "eddie", "editor").await;
    let session = sign_in(&app, "eddie").await;
    let mut sent = Vec::new();
    for (path, operation) in ROUTES {
        let auth = Auth::Session(session.clone());
        let reply = send(&app, &auth, path, &body_for(operation)).await;
        // No binding grants an account: the same 403 as for any caller.
        assert_eq!(reply.status, 403, "{path}: {}", reply.body);
        sent.push((path, operation, reply));
    }
    let log = audit_log(&directory);
    let actor = format!("user:{editor}");
    for (path, operation, reply) in &sent {
        let event = memory_event(&log, &reply.request_id);
        assert_eq!(event["operation"], *operation, "{path}");
        // An editor's grant ID is already its account encoding, so the
        // role-derived `principal` and the `actor` agree.
        assert_eq!(event["principal"], actor.as_str(), "{path}");
        assert_eq!(event["actor"], actor.as_str(), "{path}");
    }
}

#[tokio::test]
async fn the_actor_holds_no_secret() {
    let directory = TempDir::new().unwrap();
    let app = start(&directory);
    create_account(&app, "root.a", "super_admin").await;
    let session = sign_in(&app, "root.a").await;
    for (path, operation) in ROUTES {
        for auth in [admin(), Auth::Session(session.clone()), credential()] {
            send(&app, &auth, path, &body_for(operation)).await;
        }
    }
    let log = audit_log(&directory);
    let memory: String = log
        .lines()
        .filter(|line| line.contains("\"memory_request\""))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(memory.lines().count(), ROUTES.len() * 3);
    let mut secrets = renderings(&session);
    secrets.extend([
        ADMIN.to_owned(),
        SECRET.to_owned(),
        PASSWORD.to_owned(),
        "root.a".to_owned(),
        "private memory text".to_owned(),
        "actor-key-1".to_owned(),
    ]);
    for secret in &secrets {
        assert!(
            !memory.contains(secret.as_str()),
            "a memory event holds {secret}"
        );
    }
}
