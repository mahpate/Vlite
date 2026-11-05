#![cfg(feature = "server")]
//! The audit `actor`: one stable, non-secret identity per caller on the
//! audited events, whichever way the caller authenticated, recorded next to
//! the unchanged `principal` role label.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use base64::{
    Engine,
    prelude::{BASE64_STANDARD, BASE64_URL_SAFE_NO_PAD},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::Path;
use tower::ServiceExt;
use vlite::{
    CreateOptions, Database, FormatVersion,
    server::{BearerToken, ServerOptions, Service, configured_admin_router},
};

const ADMIN: &str = "fake_actor_admin_token_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const PASSWORD: &str = "correct-horse-battery-staple";
const NEW_PASSWORD: &str = "maple-orbit-violet-canyon-42";
// Request content that must never reach the audit log.
const NAMESPACE: &str = "ns-private-4f1c";
const KEY: &str = "key-private-9b3d";
const VALUE: &str = "value-private-e7a2";

/// How a request authenticates.
#[derive(Clone)]
enum Caller {
    /// No credentials: insecure local mode.
    Open,
    /// `Authorization: Bearer`: the bootstrap admin token or a scoped credential.
    Bearer(String),
    /// A dashboard cookie session on a same-origin request.
    Session(String),
}

struct Reply {
    status: StatusCode,
    body: Value,
    request_id: String,
    cookies: Vec<String>,
}

impl Reply {
    fn code(&self) -> &str {
        self.body["error"]["code"].as_str().unwrap_or("")
    }
}

fn database() -> Database {
    Database::create_in_memory(
        1,
        CreateOptions {
            format: Some(FormatVersion::V5),
            ..Default::default()
        },
    )
    .unwrap()
}

fn options(directory: &Path, bootstrap_token: bool) -> ServerOptions {
    ServerOptions {
        admin_token: bootstrap_token.then(|| BearerToken::new(ADMIN).unwrap()),
        admin_directory: Some(directory.into()),
        loopback_only: true,
        ..Default::default()
    }
}

fn start(directory: &Path, bootstrap_token: bool) -> Router {
    configured_admin_router(
        Service::new(database()),
        options(directory, bootstrap_token),
    )
    .unwrap()
}

async fn request(
    app: &Router,
    headers: &[(&str, &str)],
    method: &str,
    path: &str,
    input: Value,
) -> Reply {
    let mut request = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let body = if input.is_null() {
        Body::empty()
    } else {
        request = request.header("content-type", "application/json");
        Body::from(input.to_string())
    };
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
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

async fn send(app: &Router, caller: &Caller, method: &str, path: &str, input: Value) -> Reply {
    match caller {
        Caller::Open => request(app, &[], method, path, input).await,
        Caller::Bearer(token) => {
            let bearer = format!("Bearer {token}");
            request(app, &[("authorization", &bearer)], method, path, input).await
        }
        Caller::Session(session) => {
            let cookie = format!("vlite_session={session}");
            let headers = [
                ("cookie", cookie.as_str()),
                ("sec-fetch-site", "same-origin"),
                ("x-vlite-request", "1"),
            ];
            request(app, &headers, method, path, input).await
        }
    }
}

fn admin() -> Caller {
    Caller::Bearer(ADMIN.to_owned())
}

/// Creates a dashboard account through the admin API and returns its ID.
async fn create_account(app: &Router, caller: &Caller, username: &str, role: &str) -> String {
    let input = json!({"username": username, "password": PASSWORD, "role": role,
        "must_change_password": false});
    let reply = send(app, caller, "POST", "/v1/admin/users", input).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
    reply.body["user"]["id"].as_str().unwrap().to_owned()
}

/// Signs in like the dashboard page and returns the new session's cookie value.
async fn sign_in(app: &Router, username: &str) -> String {
    let headers = [("sec-fetch-site", "same-origin"), ("x-vlite-request", "1")];
    let input = json!({"username": username, "password": PASSWORD});
    let reply = request(app, &headers, "POST", "/v1/auth/login", input).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
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

/// Issues a scoped credential and returns its ID and its secret token.
async fn create_credential(
    app: &Router,
    collections: Value,
    operations: Value,
) -> (String, String) {
    let expires = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 600_000;
    let input = json!({"name": "actor-test", "collections": collections,
        "operations": operations, "expires_unix_ms": expires});
    let reply = send(app, &admin(), "POST", "/v1/admin/credentials", input).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
    (
        reply.body["credential"]["id"].as_str().unwrap().to_owned(),
        reply.body["token"].as_str().unwrap().to_owned(),
    )
}

async fn kv_put(app: &Router, caller: &Caller, collection: &str) -> Reply {
    let input = json!({"namespace": NAMESPACE, "key": KEY,
        "value_base64": BASE64_STANDARD.encode(VALUE)});
    let path = format!("/v1/collections/{collection}/kv/entry");
    send(app, caller, "PUT", &path, input).await
}

async fn kv_delete(app: &Router, caller: &Caller, collection: &str) -> Reply {
    let path = format!("/v1/collections/{collection}/kv/entry?namespace={NAMESPACE}&key={KEY}");
    send(app, caller, "DELETE", &path, Value::Null).await
}

fn audit_log(directory: &Path) -> String {
    std::fs::read_to_string(directory.join("audit.jsonl")).unwrap()
}

fn events(log: &str) -> Vec<Value> {
    log.lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn kv_events(events: &[Value]) -> Vec<&Value> {
    events
        .iter()
        .filter(|event| matches!(event["event"].as_str(), Some("kv_put" | "kv_delete")))
        .collect()
}

/// The single `name` event written for one request.
fn event_of<'a>(events: &'a [Value], name: &str, request_id: &str) -> &'a Value {
    let matching: Vec<_> = events
        .iter()
        .filter(|event| event["event"] == name && event["request_id"] == request_id)
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "one {name} event for request {request_id}"
    );
    matching[0]
}

/// The single `name` event about `collection`.
fn collection_event<'a>(events: &'a [Value], name: &str, collection: &str) -> &'a Value {
    let matching: Vec<_> = events
        .iter()
        .filter(|event| event["event"] == name && event["collection"] == collection)
        .collect();
    assert_eq!(matching.len(), 1, "one {name} event for {collection}");
    matching[0]
}

/// The single `name` event about the account `user_id`.
fn user_event<'a>(events: &'a [Value], name: &str, user_id: &str) -> &'a Value {
    let matching: Vec<_> = events
        .iter()
        .filter(|event| event["event"] == name && event["user_id"] == user_id)
        .collect();
    assert_eq!(matching.len(), 1, "one {name} event for account {user_id}");
    matching[0]
}

fn sorted_keys(event: &Value) -> Vec<&str> {
    let mut keys: Vec<_> = event
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
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

/// The key-value address and value of the tests are never audited, nor is any
/// credential: the whole file is scanned.
fn assert_no_secrets(log: &str, secrets: &[String]) {
    for secret in secrets {
        assert!(
            !log.contains(secret.as_str()),
            "the audit log holds {secret}"
        );
    }
    let value = BASE64_STANDARD.encode(VALUE);
    for content in [NAMESPACE, KEY, VALUE, value.as_str(), PASSWORD, ADMIN] {
        assert!(!log.contains(content), "the audit log holds {content}");
    }
}

/// One caller identity and what its audited requests must record.
struct Who {
    label: &'static str,
    caller: Caller,
    actor: String,
    principal: String,
    /// Status and code answered for a collection that does not exist.
    missing: (u16, &'static str),
}

struct Sent<'a> {
    who: &'a Who,
    event: &'static str,
    reply: Reply,
    outcome: String,
    collection: &'static str,
}

#[tokio::test]
async fn kv_events_carry_one_stable_actor_per_caller_identity() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(directory.path(), true);
    let root_a = create_account(&app, &admin(), "root.a", "super_admin").await;
    let root_b = create_account(&app, &admin(), "root.b", "super_admin").await;
    let editor = create_account(&app, &admin(), "eddie", "editor").await;
    let (credential_id, credential_token) =
        create_credential(&app, json!(["default"]), json!(["read", "write"])).await;
    let a = sign_in(&app, "root.a").await;
    let a_again = sign_in(&app, "root.a").await;
    let b = sign_in(&app, "root.b").await;
    let eddie = sign_in(&app, "eddie").await;

    let absent = (404, "collection_not_found");
    let entry = |label, caller, actor: &str, principal: &str, missing| Who {
        label,
        caller,
        actor: actor.to_owned(),
        principal: principal.to_owned(),
        missing,
    };
    let callers = [
        entry("bootstrap token", admin(), "admin_token", "admin", absent),
        entry(
            "super admin A",
            Caller::Session(a.clone()),
            &format!("user:{root_a}"),
            "admin",
            absent,
        ),
        // The actor belongs to the account, not to the session.
        entry(
            "super admin A, second session",
            Caller::Session(a_again.clone()),
            &format!("user:{root_a}"),
            "admin",
            absent,
        ),
        entry(
            "super admin B",
            Caller::Session(b.clone()),
            &format!("user:{root_b}"),
            "admin",
            absent,
        ),
        entry(
            "scoped credential",
            Caller::Bearer(credential_token.clone()),
            &credential_id,
            &credential_id,
            // Bound to `default`: any other collection is out of scope.
            (403, "forbidden_scope"),
        ),
        entry(
            "signed-in editor",
            Caller::Session(eddie.clone()),
            &format!("user:{editor}"),
            &format!("user:{editor}"),
            absent,
        ),
    ];

    let mut sent = Vec::new();
    for who in &callers {
        let put = kv_put(&app, &who.caller, "default").await;
        assert_eq!(put.status, StatusCode::OK, "{}: {}", who.label, put.body);
        let delete = kv_delete(&app, &who.caller, "default").await;
        assert_eq!(
            delete.status,
            StatusCode::OK,
            "{}: {}",
            who.label,
            delete.body
        );
        assert_eq!(delete.body, json!({"deleted": true}), "{}", who.label);
        let missing = kv_put(&app, &who.caller, "nowhere").await;
        assert_eq!(
            (missing.status.as_u16(), missing.code()),
            who.missing,
            "{}: {}",
            who.label,
            missing.body
        );
        let outcome = missing.code().to_owned();
        sent.push(Sent {
            who,
            event: "kv_put",
            reply: put,
            outcome: "ok".into(),
            collection: "default",
        });
        sent.push(Sent {
            who,
            event: "kv_delete",
            reply: delete,
            outcome: "ok".into(),
            collection: "default",
        });
        sent.push(Sent {
            who,
            event: "kv_put",
            reply: missing,
            outcome,
            // A rejected request never records the name it addressed.
            collection: "(invalid)",
        });
    }

    let log = audit_log(directory.path());
    let events = events(&log);
    assert_eq!(
        kv_events(&events).len(),
        sent.len(),
        "one event per request"
    );
    for sent in &sent {
        let event = event_of(&events, sent.event, &sent.reply.request_id);
        let label = sent.who.label;
        assert_eq!(event["event"], sent.event, "{label}");
        assert_eq!(event["actor"], sent.who.actor.as_str(), "{label}");
        assert_eq!(event["principal"], sent.who.principal.as_str(), "{label}");
        assert_eq!(event["outcome"], sent.outcome.as_str(), "{label}");
        assert_eq!(event["collection"], sent.collection, "{label}");
        // Only the actor is new: every other field is as it was.
        assert_eq!(
            sorted_keys(event),
            [
                "actor",
                "collection",
                "duration_ns",
                "event",
                "expires_at_ms",
                "outcome",
                "principal",
                "request_id",
                "unix_ms"
            ],
            "{label}"
        );
        // Neither an account's user name nor anything secret is in the event.
        let line = event.to_string();
        for name in ["root.a", "root.b", "eddie"] {
            assert!(!line.contains(name), "{label}: {line}");
        }
    }
    // Five identities, five actors, whichever session a request came in on.
    let mut actors: Vec<_> = callers.iter().map(|who| who.actor.as_str()).collect();
    actors.sort_unstable();
    actors.dedup();
    assert_eq!(actors.len(), 5, "{actors:?}");

    let mut secrets = vec![credential_token.clone()];
    secrets.push(credential_token[credential_token.len() - 43..].to_owned());
    for session in [&a, &a_again, &b, &eddie] {
        secrets.extend(renderings(session));
    }
    assert_no_secrets(&log, &secrets);
}

#[tokio::test]
async fn insecure_local_mode_records_the_admin_token_actor() {
    let directory = tempfile::tempdir().unwrap();
    // No admin token: every caller is the local administrator.
    let app = start(directory.path(), false);
    let put = kv_put(&app, &Caller::Open, "default").await;
    assert_eq!(put.status, StatusCode::OK, "{}", put.body);
    let delete = kv_delete(&app, &Caller::Open, "default").await;
    assert_eq!(delete.status, StatusCode::OK, "{}", delete.body);
    let missing = kv_put(&app, &Caller::Open, "nowhere").await;
    assert_eq!(
        (missing.status.as_u16(), missing.code()),
        (404, "collection_not_found")
    );

    let log = audit_log(directory.path());
    let events = events(&log);
    assert_eq!(kv_events(&events).len(), 3);
    for (reply, name) in [
        (&put, "kv_put"),
        (&delete, "kv_delete"),
        (&missing, "kv_put"),
    ] {
        let event = event_of(&events, name, &reply.request_id);
        assert_eq!(event["actor"], "admin_token");
        assert_eq!(event["principal"], "admin");
    }
    assert_no_secrets(&log, &[]);
}

#[tokio::test]
async fn collection_events_carry_the_shared_actor() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(directory.path(), true);
    let root_a = create_account(&app, &admin(), "root.a", "super_admin").await;
    let root_b = create_account(&app, &admin(), "root.b", "super_admin").await;
    let editor = create_account(&app, &admin(), "eddie", "editor").await;
    let (credential_id, credential_token) =
        create_credential(&app, json!(["*"]), json!(["collections"])).await;
    let a = sign_in(&app, "root.a").await;
    let b = sign_in(&app, "root.b").await;
    let eddie = sign_in(&app, "eddie").await;

    // Each identity creates, changes and drops a collection of its own.
    let callers = [
        ("by-token", admin(), "admin_token".to_owned()),
        ("by-a", Caller::Session(a.clone()), format!("user:{root_a}")),
        ("by-b", Caller::Session(b.clone()), format!("user:{root_b}")),
        (
            "by-credential",
            Caller::Bearer(credential_token.clone()),
            credential_id.clone(),
        ),
        (
            "by-editor",
            Caller::Session(eddie.clone()),
            format!("user:{editor}"),
        ),
    ];
    let mut persisted = Vec::new();
    for (name, caller, _) in &callers {
        let input = json!({"name": name, "dimensions": 1});
        let created = send(&app, caller, "POST", "/v1/collections", input).await;
        assert_eq!(
            created.status,
            StatusCode::CREATED,
            "{name}: {}",
            created.body
        );
        let path = format!("/v1/collections/{name}");
        let changed = send(&app, caller, "PUT", &format!("{path}/settings"), json!({})).await;
        assert_eq!(changed.status, StatusCode::OK, "{name}: {}", changed.body);
        persisted.push(changed.body["persisted"].clone());
        let dropped = send(&app, caller, "DELETE", &path, Value::Null).await;
        assert_eq!(dropped.status, StatusCode::OK, "{name}: {}", dropped.body);
    }

    let log = audit_log(directory.path());
    let events = events(&log);
    let lifecycle = events
        .iter()
        .filter(|event| event["event"].as_str().unwrap().starts_with("collection_"))
        .count();
    assert_eq!(lifecycle, 3 * callers.len(), "three events per identity");
    for ((name, _, actor), persisted) in callers.iter().zip(&persisted) {
        let created = collection_event(&events, "collection_created", name);
        let changed = collection_event(&events, "collection_settings_changed", name);
        let dropped = collection_event(&events, "collection_dropped", name);
        for event in [created, changed, dropped] {
            assert_eq!(event["actor"], actor.as_str(), "{name}");
        }
        assert_eq!(changed["persisted"], *persisted, "{name}");
        // Only the value of the existing `actor` field changed.
        let lifecycle = ["actor", "collection", "collection_id", "event", "unix_ms"];
        assert_eq!(sorted_keys(created), lifecycle, "{name}");
        assert_eq!(sorted_keys(dropped), lifecycle, "{name}");
        assert_eq!(
            sorted_keys(changed),
            [
                "actor",
                "collection",
                "collection_id",
                "event",
                "persisted",
                "unix_ms"
            ],
            "{name}"
        );
    }

    let mut secrets = vec![credential_token.clone()];
    secrets.push(credential_token[credential_token.len() - 43..].to_owned());
    for session in [&a, &b, &eddie] {
        secrets.extend(renderings(session));
    }
    assert_no_secrets(&log, &secrets);
}

#[tokio::test]
async fn denied_requests_record_the_actor_beside_the_credential_id() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(directory.path(), true);
    let viewer = create_account(&app, &admin(), "vera", "viewer").await;
    let (credential_id, credential_token) =
        create_credential(&app, json!(["default"]), json!(["read"])).await;
    let vera = sign_in(&app, "vera").await;

    // Neither holds `write`, so the request is refused before any handler
    // runs. A signed-in viewer's grant id is `user:<account id>`.
    let callers = [
        (
            "read-only credential",
            Caller::Bearer(credential_token.clone()),
            credential_id.clone(),
        ),
        (
            "signed-in viewer",
            Caller::Session(vera.clone()),
            format!("user:{viewer}"),
        ),
    ];
    let mut denied = Vec::new();
    for (label, caller, actor) in &callers {
        let reply = kv_put(&app, caller, "default").await;
        let status = (reply.status.as_u16(), reply.code());
        assert_eq!(status, (403, "forbidden_scope"), "{label}: {}", reply.body);
        denied.push((label, actor, reply));
    }
    // The bootstrap token is never denied.
    assert_eq!(
        kv_put(&app, &admin(), "default").await.status,
        StatusCode::OK
    );

    let log = audit_log(directory.path());
    let events = events(&log);
    for (label, actor, reply) in &denied {
        let event = event_of(&events, "credential_denied", &reply.request_id);
        assert_eq!(event["credential_id"], actor.as_str(), "{label}");
        assert_eq!(event["actor"], actor.as_str(), "{label}");
        assert_eq!(event["method"], "PUT", "{label}");
        assert_eq!(event["route"], "/v1/collections/{collection}/kv/entry");
        assert_eq!(
            sorted_keys(event),
            [
                "actor",
                "credential_id",
                "event",
                "method",
                "request_id",
                "route",
                "unix_ms"
            ],
            "{label}"
        );
    }
    // Only the bootstrap token's put reached the key-value audit.
    assert_eq!(kv_events(&events).len(), 1);

    let mut secrets = renderings(&vera);
    secrets.push(credential_token);
    assert_no_secrets(&log, &secrets);
}

#[tokio::test]
async fn account_events_use_the_shared_encoding() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(directory.path(), true);
    let root_a = create_account(&app, &admin(), "root.a", "super_admin").await;
    let root_b = create_account(&app, &admin(), "root.b", "super_admin").await;
    let a = sign_in(&app, "root.a").await;
    let b = sign_in(&app, "root.b").await;
    let (by_a, by_b) = (Caller::Session(a.clone()), Caller::Session(b.clone()));

    // Super admin A creates an account, B changes it, the bootstrap token
    // resets its password, and A deletes it.
    let pat = create_account(&app, &by_a, "pat", "viewer").await;
    let path = format!("/v1/admin/users/{pat}");
    let updated = send(&app, &by_b, "PUT", &path, json!({"role": "editor"})).await;
    assert_eq!(updated.status, StatusCode::OK, "{}", updated.body);
    let input = json!({"password": NEW_PASSWORD, "must_change_password": false});
    let reset = send(&app, &admin(), "POST", &format!("{path}/password"), input).await;
    assert_eq!(reset.status, StatusCode::OK, "{}", reset.body);
    let removed = send(&app, &by_a, "DELETE", &path, Value::Null).await;
    assert_eq!(removed.status, StatusCode::OK, "{}", removed.body);

    let log = audit_log(directory.path());
    let events = events(&log);
    for (name, account, actor) in [
        ("user_created", &root_a, "admin_token".to_owned()),
        ("user_created", &root_b, "admin_token".to_owned()),
        ("user_created", &pat, format!("user:{root_a}")),
        ("user_updated", &pat, format!("user:{root_b}")),
        ("user_password_reset", &pat, "admin_token".to_owned()),
        ("user_deleted", &pat, format!("user:{root_a}")),
    ] {
        let event = user_event(&events, name, account);
        assert_eq!(event["actor"], actor.as_str(), "{name} {account}");
    }
    let mut secrets = [renderings(&a), renderings(&b)].concat();
    secrets.push(NEW_PASSWORD.to_owned());
    assert_no_secrets(&log, &secrets);
}

#[cfg(feature = "grpc")]
mod grpc {
    use super::*;
    use tonic::{Code, Request as GrpcRequest, transport::Channel};
    use vlite::server::grpc::{
        grpc_router,
        proto::{self, kv_client::KvClient, v_lite_client::VLiteClient},
    };

    fn address() -> Option<proto::KvAddress> {
        Some(proto::KvAddress {
            collection: "default".into(),
            namespace: NAMESPACE.into(),
            key: KEY.into(),
        })
    }

    fn with_credentials<T>(
        message: T,
        bearer: Option<&str>,
        session: Option<&str>,
    ) -> GrpcRequest<T> {
        let mut request = GrpcRequest::new(message);
        if let Some(token) = bearer {
            let value = format!("Bearer {token}");
            request
                .metadata_mut()
                .insert("authorization", value.parse().unwrap());
        }
        if let Some(session) = session {
            let value = format!("vlite_session={session}");
            request
                .metadata_mut()
                .insert("cookie", value.parse().unwrap());
        }
        request
    }

    fn put(bearer: Option<&str>, session: Option<&str>) -> GrpcRequest<proto::KvPutRequest> {
        let message = proto::KvPutRequest {
            address: address(),
            value: VALUE.as_bytes().to_vec(),
            ..Default::default()
        };
        with_credentials(message, bearer, session)
    }

    fn delete(bearer: Option<&str>, session: Option<&str>) -> GrpcRequest<proto::KvDeleteRequest> {
        let message = proto::KvDeleteRequest {
            address: address(),
            expected: None,
        };
        with_credentials(message, bearer, session)
    }

    fn request_id<T>(response: &tonic::Response<T>) -> String {
        response
            .metadata()
            .get("x-request-id")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned()
    }

    /// Serves gRPC for `service` on a loopback port; dropping the returned
    /// sender stops the server.
    async fn connect(service: Service) -> (Channel, tokio::sync::oneshot::Sender<()>) {
        let grpc = grpc_router(service).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bound = listener.local_addr().unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(grpc.serve_with_shutdown(listener, async {
            let _ = stopped.await;
        }));
        let channel = Channel::from_shared(format!("http://{bound}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        (channel, stop)
    }

    #[tokio::test]
    async fn grpc_kv_events_record_the_token_actor_and_never_a_session() {
        let directory = tempfile::tempdir().unwrap();
        let service = Service::new(database());
        let app =
            configured_admin_router(service.clone(), options(directory.path(), true)).unwrap();
        create_account(&app, &admin(), "root.a", "super_admin").await;
        let session = sign_in(&app, "root.a").await;
        let (credential_id, credential_token) =
            create_credential(&app, json!(["default"]), json!(["read", "write"])).await;

        let (channel, _stop) = connect(service).await;
        let mut client = KvClient::new(channel);

        // The bootstrap token, once alone and once beside the live session
        // cookie of a super admin (gRPC reads only the bearer token), and a
        // scoped credential.
        let attempts = [
            (ADMIN, None, "admin_token", "admin"),
            (ADMIN, Some(session.as_str()), "admin_token", "admin"),
            (
                credential_token.as_str(),
                None,
                credential_id.as_str(),
                credential_id.as_str(),
            ),
        ];
        let mut sent = Vec::new();
        for (token, cookie, actor, principal) in attempts {
            let stored = client.put_kv_entry(put(Some(token), cookie)).await.unwrap();
            sent.push(("kv_put", request_id(&stored), actor, principal));
            let removed = client
                .delete_kv_entry(delete(Some(token), cookie))
                .await
                .unwrap();
            sent.push(("kv_delete", request_id(&removed), actor, principal));
        }
        // A session cookie alone authenticates nobody over gRPC.
        let refused = client
            .put_kv_entry(put(None, Some(&session)))
            .await
            .unwrap_err();
        assert_eq!(refused.code(), Code::Unauthenticated);

        let log = audit_log(directory.path());
        let events = events(&log);
        assert_eq!(kv_events(&events).len(), sent.len());
        for (name, id, actor, principal) in sent {
            let event = event_of(&events, name, &id);
            assert_eq!(event["event"], name);
            assert_eq!(event["actor"], actor);
            assert_eq!(event["principal"], principal);
            assert_eq!(event["outcome"], "ok");
            assert_eq!(event["collection"], "default");
        }
        let mut secrets = renderings(&session);
        secrets.push(credential_token);
        assert_no_secrets(&log, &secrets);
    }

    #[tokio::test]
    async fn grpc_collection_and_denial_events_record_the_actor() {
        let directory = tempfile::tempdir().unwrap();
        let service = Service::new(database());
        let app =
            configured_admin_router(service.clone(), options(directory.path(), true)).unwrap();
        create_account(&app, &admin(), "root.a", "super_admin").await;
        let session = sign_in(&app, "root.a").await;
        let (wide_id, wide_token) =
            create_credential(&app, json!(["*"]), json!(["collections"])).await;
        let (read_id, read_token) =
            create_credential(&app, json!(["default"]), json!(["read"])).await;

        let (channel, _stop) = connect(service).await;
        let mut collections = VLiteClient::new(channel.clone());
        let mut client = KvClient::new(channel);

        // The bootstrap token, once alone and once beside a live session
        // cookie, and a wildcard credential each create and drop a collection.
        let attempts = [
            ("by-token", ADMIN, None, "admin_token"),
            ("by-cookie", ADMIN, Some(session.as_str()), "admin_token"),
            ("by-credential", wide_token.as_str(), None, wide_id.as_str()),
        ];
        for (name, token, cookie, _) in attempts {
            let create = proto::CreateCollectionRequest {
                name: name.into(),
                dimensions: 1,
                settings_json: None,
            };
            let created = with_credentials(create, Some(token), cookie);
            collections.create_collection(created).await.unwrap();
            let removal = proto::DropCollectionRequest { name: name.into() };
            let dropped = with_credentials(removal, Some(token), cookie);
            collections.drop_collection(dropped).await.unwrap();
        }
        // A credential without `write` is refused, and the refusal names it.
        let denied = client
            .put_kv_entry(put(Some(&read_token), None))
            .await
            .unwrap_err();
        assert_eq!(denied.code(), Code::PermissionDenied);
        let denied_id = denied.metadata().get("x-request-id").unwrap();

        let log = audit_log(directory.path());
        let events = events(&log);
        for (name, _, _, actor) in attempts {
            for event in ["collection_created", "collection_dropped"] {
                let found = collection_event(&events, event, name);
                assert_eq!(found["actor"], actor, "{event} {name}");
            }
        }
        let event = event_of(&events, "credential_denied", denied_id.to_str().unwrap());
        assert_eq!(event["credential_id"], read_id.as_str());
        assert_eq!(event["actor"], read_id.as_str());
        assert_eq!(event["method"], "grpc");
        assert_eq!(event["route"], "grpc:/vlite.v1.Kv/PutKvEntry");
        let mut secrets = renderings(&session);
        secrets.extend([wide_token, read_token]);
        assert_no_secrets(&log, &secrets);
    }
}
