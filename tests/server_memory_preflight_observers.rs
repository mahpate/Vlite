#![cfg(feature = "server")]

//! TEST-A09 observers (memory-preflight PRD v1.7): what preflight and status
//! leave in the structured log buffer, the admin access ring and the
//! persistent audit file, on every outcome, denial and error path.
//!
//! The log buffer is process-wide, so this file holds a single test: in its
//! own process every entry after a marker comes from the requests below. That
//! makes the check observable rather than a source scan: any event a handler
//! or a helper it calls writes is caught, whatever it names. The only allowed
//! entries are the server's generic request records, whose `credential_id` is
//! the caller's own identity, never a name read from the bindings file.

use std::path::PathBuf;

use axum::{Router, body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tower::ServiceExt;
use vlite::{
    CollectionSpec, Database,
    server::{BearerToken, ServerOptions, Service, configured_admin_router},
};

const ADMIN: &str = "fake_observer_admin_token_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const READ_ONLY: &str = "fake_observer_read_token_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const SECRET: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const PREFLIGHT: &str = "/v1/admin/memory/bindings/preflight";
const STATUS: &str = "/v1/admin/memory/bindings/status";

/// Every sensitive dimension of the fixture carries a name used nowhere else.
const BINDING: &str = "sentinel_binding_obs";
const GRAPH: &str = "sentinel_graph_obs";
const EVIDENCE: &str = "sentinel_evidence_obs";
const SCOPE: &str = "sentinel_scope_obs";
const FILE: &str = "sentinel-bindings-obs.json";
/// The binding's principal: a credential ID that also calls the routes, so
/// that its own request records carry it as the caller's identity.
const PRINCIPAL: &str = "sentinelpr27";
/// A scoped credential the bindings file does not name.
const CALLER: &str = "callerdenied";

/// The fields of the generic per-request log line (`src/server.rs`, `observe`).
const REQUEST_FIELDS: [&str; 7] = [
    "event",
    "request_id",
    "method",
    "route",
    "status",
    "duration_ns",
    "credential_id",
];
/// The fields of the generic per-request access record in the audit ring.
const ACCESS_FIELDS: [&str; 8] = [
    "request_id",
    "method",
    "route",
    "status",
    "role",
    "credential_id",
    "duration_ns",
    "timestamp_unix_ms",
];

fn bindings(generation: u64, scope: &str, binding: &str) -> Value {
    json!({
        "version": "memory-bindings-v1",
        "bindings": [{
            "binding": binding, "tenant": "@root", "generation": generation,
            "state": "active", "graph": GRAPH, "evidence_collection": EVIDENCE,
            "schema_version": "memory-schema-v1", "ranking_version": "memory-ranking-v1",
            "digest_version": "memory-request-v1", "receipt_window_ms": 86_400_000u64,
            "grants": [{"principal": PRINCIPAL, "scopes": [{"agent": scope}], "raw_access": true}]
        }]
    })
}

struct Rig {
    directory: TempDir,
    path: PathBuf,
}

impl Rig {
    fn new() -> Self {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("observers.vlite");
        let mut db = Database::create(&path, 2).unwrap();
        db.create_graph(GRAPH).unwrap();
        db.create_collection(
            EVIDENCE,
            CollectionSpec {
                dimensions: 2,
                settings: Default::default(),
            },
        )
        .unwrap();
        let collections: Vec<Value> = db
            .collections()
            .unwrap()
            .into_iter()
            .filter(|info| info.name == EVIDENCE)
            .map(|info| json!({"name": info.name, "id": info.id}))
            .collect();
        let credentials: Vec<Value> = [PRINCIPAL, CALLER]
            .into_iter()
            .map(|id| {
                json!({
                    "id": id, "name": "fake observer credential", "all": false,
                    "collections": collections,
                    "operations": ["read", "search", "write", "graph_read", "graph_write"],
                    "graphs": [GRAPH],
                    "lineage": format!("{:016x}", db.lineage().unwrap()),
                    "secret_sha256": format!("{:x}", Sha256::digest(SECRET.as_bytes())),
                    "created_unix_ms": 0, "expires_unix_ms": 9_999_999_999_999u64,
                    "revoked_unix_ms": null, "rotated_from": null
                })
            })
            .collect();
        std::fs::write(
            directory.path().join("credentials.json"),
            serde_json::to_vec(&json!({"schema": 1, "credentials": credentials})).unwrap(),
        )
        .unwrap();
        db.close().unwrap();
        Self { directory, path }
    }

    fn write_text(&self, text: &str) -> PathBuf {
        let path = self.directory.path().join(FILE);
        std::fs::write(&path, text).unwrap();
        path
    }

    fn start(&self, file: Option<&Value>) -> (Router, Service) {
        let service = Service::new(Database::open(&self.path).unwrap());
        let options = ServerOptions {
            admin_token: Some(BearerToken::new(ADMIN).unwrap()),
            read_only_token: Some(BearerToken::new(READ_ONLY).unwrap()),
            admin_directory: Some(self.directory.path().into()),
            memory_bindings_file: file.map(|file| self.write_text(&file.to_string())),
            ..Default::default()
        };
        (
            configured_admin_router(service.clone(), options).unwrap(),
            service,
        )
    }

    /// The sentinels no observer may hold outside a caller-identity field.
    fn sentinels(&self) -> Vec<String> {
        let directory = self.directory.path();
        vec![
            BINDING.into(),
            "@root".into(),
            PRINCIPAL.into(),
            GRAPH.into(),
            EVIDENCE.into(),
            SCOPE.into(),
            FILE.into(),
            directory.to_str().unwrap().into(),
            directory.file_name().unwrap().to_str().unwrap().into(),
        ]
    }
}

async fn call(app: &Router, method: &str, path: &str, token: Option<&str>) -> (u16, Value) {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn scoped(id: &str) -> String {
    format!("vlk_{id}_{SECRET}")
}

/// The newest structured log sequence number.
async fn log_seq(app: &Router) -> u64 {
    let (status, body) = call(app, "GET", "/v1/admin/logs?limit=1", Some(ADMIN)).await;
    assert_eq!(status, 200, "{body}");
    body["next_seq"].as_u64().unwrap()
}

/// Every log entry after `since`.
async fn logs_since(app: &Router, since: u64) -> Vec<Value> {
    let (status, body) = call(
        app,
        "GET",
        &format!("/v1/admin/logs?since={since}&limit=1000"),
        Some(ADMIN),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    body["entries"].as_array().unwrap().clone()
}

/// `value` with the caller-identity fields removed: a credential's own ID in
/// the record of its own request is not a disclosure from the bindings file.
fn without_identity(mut value: Value) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.remove("credential_id");
        object.remove("actor");
    }
    value
}

fn assert_names_nothing(rig: &Rig, observer: &str, value: &Value) {
    let text = without_identity(value.clone()).to_string();
    for named in rig.sentinels() {
        assert!(!text.contains(&named), "{observer} names {named}: {value}");
    }
}

/// The fields of the generic per-request HTTP access log entry
/// (`src/server.rs`, `record_http_access`).
const HTTP_FIELDS: [&str; 5] = ["method", "path", "status", "duration_ms", "collection"];

/// Every log entry is one of the server's two generic per-request records:
/// the `server` request line, whose `credential_id` is the caller's own
/// (null for tokens), or the `http` access entry, which names the request
/// path. Any other entry, from a handler or any helper it calls, fails.
fn assert_only_request_records(rig: &Rig, entries: &[Value]) {
    for entry in entries {
        let fields = entry["fields"].as_object().unwrap();
        let message = entry["message"].as_str().unwrap();
        match entry["target"].as_str() {
            Some("server") => {
                assert_eq!(
                    fields.get("event"),
                    Some(&json!("request")),
                    "only the generic request line is logged: {entry}"
                );
                for key in fields.keys() {
                    assert!(
                        REQUEST_FIELDS.contains(&key.as_str()),
                        "unexpected request field {key}: {entry}"
                    );
                }
                assert!(
                    [Value::Null, json!(PRINCIPAL), json!(CALLER)]
                        .contains(&fields["credential_id"]),
                    "{entry}"
                );
                let line: Value = serde_json::from_str(message).unwrap();
                assert_names_nothing(rig, "log line", &line);
            }
            Some("http") => {
                for key in fields.keys() {
                    assert!(
                        HTTP_FIELDS.contains(&key.as_str()),
                        "unexpected access field {key}: {entry}"
                    );
                }
                assert_eq!(fields["collection"], Value::Null, "{entry}");
                assert_eq!(
                    message,
                    format!(
                        "{} {} {}",
                        fields["method"].as_str().unwrap(),
                        fields["path"].as_str().unwrap(),
                        fields["status"]
                    ),
                    "{entry}"
                );
                assert!(
                    !message.contains(PRINCIPAL) && !message.contains(CALLER),
                    "{entry}"
                );
            }
            _ => panic!("only generic request records are logged: {entry}"),
        }
        assert_names_nothing(rig, "log entry", &entry["fields"]);
    }
}

/// Calls both routes with `token` and expects `expected` from each.
async fn exercise(app: &Router, token: Option<&str>, expected: u16) {
    for (method, path) in [("POST", PREFLIGHT), ("GET", STATUS)] {
        let (status, body) = call(app, method, path, token).await;
        assert_eq!(status, expected, "{method} {path}: {body}");
    }
}

#[tokio::test]
async fn preflight_and_status_observers_hold_only_generic_caller_records() {
    let rig = Rig::new();
    let first = bindings(1, SCOPE, BINDING);
    let (app, service) = rig.start(Some(&first));
    let since = log_seq(&app).await;
    let (_, ring) = call(&app, "GET", "/v1/admin/audit", Some(ADMIN)).await;
    let ring_before = ring["items"].as_array().unwrap().len();

    // Every outcome, as the administrator.
    let mut outcomes = Vec::new();
    let edits = [
        None,
        Some(bindings(2, "sentinel_scope_obs_b", BINDING).to_string()),
        Some(bindings(3, SCOPE, "sentinel_binding_obs_renamed").to_string()),
        Some(bindings(1, "sentinel_scope_obs_c", BINDING).to_string()),
        Some("not JSON".to_owned()),
    ];
    for edit in edits {
        if let Some(text) = edit {
            rig.write_text(&text);
        }
        let (status, preflight) = call(&app, "POST", PREFLIGHT, Some(ADMIN)).await;
        assert_eq!(status, 200, "{preflight}");
        assert_names_nothing(&rig, "preflight body", &preflight);
        outcomes.push(preflight["outcome"].as_str().unwrap().to_owned());
        let (status, report) = call(&app, "GET", STATUS, Some(ADMIN)).await;
        assert_eq!(status, 200, "{report}");
        assert_names_nothing(&rig, "status body", &report);
    }
    assert_eq!(
        outcomes,
        [
            "unchanged",
            "reload",
            "restart_required",
            "refused",
            "invalid"
        ]
    );

    // Denied callers: the read-only token, the binding's own principal, a
    // credential the file does not name, and no token.
    exercise(&app, Some(READ_ONLY), 403).await;
    exercise(&app, Some(&scoped(PRINCIPAL)), 403).await;
    exercise(&app, Some(&scoped(CALLER)), 403).await;
    exercise(&app, None, 401).await;

    let entries = logs_since(&app, since).await;
    assert_only_request_records(&rig, &entries);
    for (id, route) in [(PRINCIPAL, PREFLIGHT), (CALLER, STATUS)] {
        assert!(
            entries
                .iter()
                .any(|entry| entry["fields"]["credential_id"] == id
                    && entry["fields"]["route"] == route
                    && entry["fields"]["status"] == 403),
            "the denied caller's own request record carries its identity"
        );
    }

    // The access ring gains generic per-request records only.
    let (_, ring) = call(&app, "GET", "/v1/admin/audit", Some(ADMIN)).await;
    for item in &ring["items"].as_array().unwrap()[ring_before..] {
        for key in item.as_object().unwrap().keys() {
            assert!(
                ACCESS_FIELDS.contains(&key.as_str()),
                "only generic access records are added: {item}"
            );
        }
        assert_names_nothing(&rig, "access record", item);
    }

    // A closed database: preflight is 503 `unavailable`, status still
    // answers, and the error path logs only request records too.
    rig.write_text(&first.to_string());
    let since = log_seq(&app).await;
    service.close().await.unwrap();
    let (status, closed) = call(&app, "POST", PREFLIGHT, Some(ADMIN)).await;
    assert_eq!(status, 503, "{closed}");
    assert_eq!(closed["error"]["code"], "unavailable", "{closed}");
    assert_names_nothing(&rig, "503 body", &closed);
    let (status, report) = call(&app, "GET", STATUS, Some(ADMIN)).await;
    assert_eq!(status, 200, "{report}");
    assert_only_request_records(&rig, &logs_since(&app, since).await);

    // The persistent audit file: the existing denial events of the scoped
    // callers name the caller and the route, never a binding-file name, and
    // nothing else in it names one either.
    let audit = std::fs::read_to_string(rig.directory.path().join("audit.jsonl")).unwrap();
    let mut denials = Vec::new();
    for line in audit.lines() {
        let event: Value = serde_json::from_str(line).unwrap();
        assert_names_nothing(&rig, "audit line", &event);
        if event["event"] == "credential_denied" {
            denials.push(event["credential_id"].clone());
        }
    }
    assert!(
        denials.contains(&json!(PRINCIPAL)) && denials.contains(&json!(CALLER)),
        "{audit}"
    );

    // With no bindings file, the `unavailable` outcome.
    let (app, service) = rig.start(None);
    let since = log_seq(&app).await;
    let (status, unavailable) = call(&app, "POST", PREFLIGHT, Some(ADMIN)).await;
    assert_eq!(status, 200, "{unavailable}");
    assert_eq!(unavailable["outcome"], "unavailable");
    assert_names_nothing(&rig, "preflight body", &unavailable);
    exercise(&app, Some(ADMIN), 200).await;
    assert_only_request_records(&rig, &logs_since(&app, since).await);
    service.close().await.unwrap();
}
