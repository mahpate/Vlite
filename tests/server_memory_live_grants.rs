#![cfg(feature = "server")]

//! Independent REST checks of TEST-010 through TEST-014 in the live-grants PRD.

use std::path::PathBuf;

use axum::{Router, body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tower::ServiceExt;
use vlite::{
    CollectionSpec, Database,
    property_graph::NodeView,
    server::{BearerToken, ServerOptions, Service, configured_admin_router},
};

const ADMIN: &str = "fake_live_grants_admin_token_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const READ_ONLY: &str = "fake_live_grants_read_token_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const ID: &str = "abcdefghij27";
const OTHER_ID: &str = "abcdefghij26";
const SECRET: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const WINDOW: u64 = 86_400_000;
const RELOAD: &str = "/v1/admin/memory/bindings/reload";

fn bindings(generation: u64, agent: &str, raw_access: bool) -> Value {
    json!({
        "version": "memory-bindings-v1",
        "bindings": [{
            "binding": "notes", "tenant": "@root", "generation": generation,
            "state": "active", "graph": "notes_graph", "evidence_collection": "notes_ev",
            "schema_version": "memory-schema-v1", "ranking_version": "memory-ranking-v1",
            "digest_version": "memory-request-v1", "receipt_window_ms": WINDOW,
            "grants": [{"principal": ID, "scopes": [{"agent": agent}], "raw_access": raw_access}]
        }]
    })
}

fn two_bindings(generation: u64, agent: &str) -> Value {
    let mut file = bindings(generation, agent, true);
    let mut second = file["bindings"][0].clone();
    second["binding"] = json!("tasks");
    second["graph"] = json!("tasks_graph");
    second["evidence_collection"] = json!("tasks_ev");
    file["bindings"].as_array_mut().unwrap().push(second);
    file
}

// Adapted from server_memory_binding_record.rs: file writer, file-backed
// startup/close/restart, synthetic credentials, and binding-record inspection.
struct Rig {
    directory: TempDir,
    path: PathBuf,
}

impl Rig {
    fn new() -> Self {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("memory.vlite");
        let mut db = Database::create(&path, 2).unwrap();
        for graph in ["notes_graph", "tasks_graph", "plain_graph"] {
            db.create_graph(graph).unwrap();
        }
        for collection in ["notes_ev", "tasks_ev"] {
            db.create_collection(
                collection,
                CollectionSpec {
                    dimensions: 2,
                    settings: Default::default(),
                },
            )
            .unwrap();
        }
        let collections: Vec<Value> = db
            .collections()
            .unwrap()
            .into_iter()
            .filter(|info| matches!(info.name.as_str(), "notes_ev" | "tasks_ev"))
            .map(|info| json!({"name": info.name, "id": info.id}))
            .collect();
        let credentials: Vec<Value> = [ID, OTHER_ID]
            .into_iter()
            .map(|id| {
                json!({
                    "id": id, "name": "fake live grants credential", "all": false,
                    "collections": collections,
                    "operations": ["read", "search", "write", "graph_read", "graph_write"],
                    "graphs": ["notes_graph", "tasks_graph"],
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
        let path = self.directory.path().join("memory-bindings.json");
        std::fs::write(&path, text).unwrap();
        path
    }

    fn write_bindings(&self, file: &Value) -> PathBuf {
        self.write_text(&file.to_string())
    }

    fn size(&self) -> u64 {
        std::fs::metadata(&self.path).unwrap().len()
    }

    fn options(&self, file: Option<&Value>) -> ServerOptions {
        ServerOptions {
            admin_token: Some(BearerToken::new(ADMIN).unwrap()),
            read_only_token: Some(BearerToken::new(READ_ONLY).unwrap()),
            admin_directory: Some(self.directory.path().into()),
            memory_bindings_file: file.map(|file| self.write_bindings(file)),
            ..Default::default()
        }
    }

    async fn start(
        &self,
        file: Option<&Value>,
        read_only: bool,
    ) -> std::io::Result<(Router, Service)> {
        let db = if read_only {
            Database::open_read_only(&self.path).unwrap()
        } else {
            Database::open(&self.path).unwrap()
        };
        let service = Service::new(db);
        match configured_admin_router(service.clone(), self.options(file)) {
            Ok(router) => Ok((router, service)),
            Err(error) => {
                service.close().await.unwrap();
                Err(error)
            }
        }
    }

    async fn run<T>(&self, file: &Value, work: impl AsyncFnOnce(&Router) -> T) -> T {
        let (app, service) = self.start(Some(file), false).await.unwrap();
        let result = work(&app).await;
        service.close().await.unwrap();
        result
    }

    async fn refused(&self, file: &Value) -> String {
        match self.start(Some(file), false).await {
            Ok((_, service)) => {
                service.close().await.unwrap();
                panic!("startup with the pre-reload file must be refused");
            }
            Err(error) => error.to_string(),
        }
    }

    fn records(&self) -> Vec<Option<NodeView>> {
        let db = Database::open_read_only(&self.path).unwrap();
        let records = ["notes_graph", "tasks_graph"]
            .into_iter()
            .map(|graph| record(&db, graph))
            .collect();
        db.close().unwrap();
        records
    }
}

fn record(db: &Database, graph: &str) -> Option<NodeView> {
    db.graph(graph)
        .unwrap()
        .get_node("binding:v1")
        .unwrap()
        .output
}

fn credential(id: &str) -> String {
    format!("vlk_{id}_{SECRET}")
}

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    token: &str,
    body: Option<&Value>,
) -> (u16, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(
            body.map(|body| serde_json::to_vec(body).unwrap())
                .unwrap_or_default(),
        ))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("a REST response must be JSON")
    };
    (status, body)
}

fn assert_error(answer: (u16, Value), status: u16, code: &str) {
    assert_eq!(answer.0, status, "{}", answer.1);
    assert_eq!(answer.1["error"]["code"], code, "{}", answer.1);
}

async fn reload(app: &Router, count: usize, changed: usize) -> Value {
    let (status, body) = call(app, "POST", RELOAD, ADMIN, None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        json!({"bindings": count, "changed": changed, "drained": true}),
        "an idle reload has exactly the three public fields"
    );
    body
}

async fn memory(
    app: &Router,
    binding: &str,
    operation: &str,
    id: &str,
    body: &Value,
) -> (u16, Value) {
    call(
        app,
        "POST",
        &format!("/v1/memory/{binding}/{operation}"),
        &credential(id),
        Some(body),
    )
    .await
}

fn remember_body(agent: &str, key: &str) -> Value {
    json!({
        "scope": {"agent": agent}, "idempotency_key": key, "deadline_ms": 5000,
        "items": [{
            "id": "e1", "kind": "entity", "content": "kept across live grant changes",
            "observed_at_ms": "90", "source": {"source_id": "s1", "source_kind": "caller"}
        }]
    })
}

fn recall_body(agent: &str) -> Value {
    // A vector query avoids the distinct missing-entity-seed refusal.
    json!({"scope": {"agent": agent}, "vector": [1.0, 0.0], "limits": {"deadline_ms": 5000}})
}

async fn empty_recall(app: &Router, binding: &str, id: &str, agent: &str) {
    let (status, body) = memory(app, binding, "recall", id, &recall_body(agent)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["memories"], json!([]), "{body}");
}

async fn raw_record(app: &Router, graph: &str) -> Value {
    let (status, body) = call(
        app,
        "POST",
        &format!("/v1/graphs/{graph}/nodes/get"),
        &credential(ID),
        Some(&json!({"node": {"key": "binding:v1"}})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    body.get("node").expect("node field").clone()
}

async fn raw_records(app: &Router) -> Vec<Value> {
    let mut records = Vec::new();
    for graph in ["notes_graph", "tasks_graph"] {
        records.push(raw_record(app, graph).await);
    }
    records
}

async fn assert_refused_unchanged(rig: &Rig, app: &Router, token: &str, status: u16, code: &str) {
    let records = raw_records(app).await;
    let size = rig.size();
    let answer = call(app, "POST", RELOAD, token, None).await;
    assert_error(answer, status, code);
    assert_eq!(raw_records(app).await, records, "refusal changed a record");
    assert_eq!(rig.size(), size, "refusal appended a commit");
}

#[tokio::test]
async fn live_grants_test_010_partial_state_converges() {
    let rig = Rig::new();
    let old = two_bindings(1, "old_scope");
    let new = two_bindings(2, "new_scope");
    let (app, service) = rig.start(Some(&old), false).await.unwrap();

    let old_a = raw_record(&app, "notes_graph").await;
    let old_b = raw_record(&app, "tasks_graph").await;
    // Startup on a second REST router pre-writes only A's authentic new
    // record. RD-010 leaves the original router serving its old grants;
    // reserved record keys cannot be written through the raw REST API.
    let mut partial = old.clone();
    partial["bindings"][0] = new["bindings"][0].clone();
    let writer = configured_admin_router(service.clone(), rig.options(Some(&partial))).unwrap();
    let advanced = raw_record(&writer, "notes_graph").await;
    assert_ne!(advanced, old_a);
    assert_eq!(
        advanced["properties"]["generation"],
        json!({"type": "string", "value": "2"})
    );
    assert_eq!(raw_record(&app, "notes_graph").await, advanced);
    assert_eq!(raw_record(&app, "tasks_graph").await, old_b);
    drop(writer);
    rig.write_bindings(&new);
    let size = rig.size();
    let stale_write = memory(
        &app,
        "notes",
        "remember",
        ID,
        &remember_body("old_scope", "partial-write"),
    )
    .await;
    // PRD-ambiguity: "409 binding_generation" names the capability; G3
    // preserves the existing error code plus details.feature wire envelope.
    assert_eq!(
        stale_write.1["error"]["details"],
        json!({"feature": "binding_generation"})
    );
    assert_error(stale_write, 409, "memory_capability_unavailable");
    assert_eq!(rig.size(), size, "a stale memory write must not commit");
    empty_recall(&app, "notes", ID, "old_scope").await;
    assert_error(
        memory(&app, "notes", "recall", ID, &recall_body("new_scope")).await,
        403,
        "forbidden_scope",
    );

    // Even the still-served file must be rechecked against partially advanced
    // records; an unchanged-file reload cannot falsely report convergence.
    rig.write_bindings(&old);
    assert_refused_unchanged(&rig, &app, ADMIN, 409, "memory_bindings_conflict").await;
    rig.write_bindings(&new);
    reload(&app, 2, 2).await;
    assert_error(
        memory(&app, "notes", "recall", ID, &recall_body("old_scope")).await,
        403,
        "forbidden_scope",
    );
    for binding in ["notes", "tasks"] {
        empty_recall(&app, binding, ID, "new_scope").await;
        let (status, body) = memory(
            &app,
            binding,
            "remember",
            ID,
            &remember_body("new_scope", "converged-write"),
        )
        .await;
        assert_eq!(status, 200, "{body}");
    }
    service.close().await.unwrap();
    let records = rig.records();
    let size = rig.size();
    rig.run(&new, async |_| {}).await;
    assert_eq!(rig.size(), size, "converged restart adds no commit");
    assert_eq!(rig.records(), records);
}

#[tokio::test]
async fn live_grants_test_011_scopes_and_read_scopes_apply_live() {
    let rig = Rig::new();
    let old = bindings(1, "revoked_scope", false);
    let (app, service) = rig.start(Some(&old), false).await.unwrap();
    let (status, body) = memory(
        &app,
        "notes",
        "remember",
        ID,
        &remember_body("revoked_scope", "before-revoke"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let mut new = bindings(2, "added_scope", false);
    new["bindings"][0]["grants"][0]["read_scopes"] = json!([{"agent": "added_read_scope"}]);
    rig.write_bindings(&new);
    reload(&app, 1, 1).await;
    assert_error(
        memory(&app, "notes", "recall", ID, &recall_body("revoked_scope")).await,
        403,
        "forbidden_scope",
    );
    assert_error(
        memory(
            &app,
            "notes",
            "remember",
            ID,
            &remember_body("revoked_scope", "after-revoke"),
        )
        .await,
        403,
        "forbidden_scope",
    );
    for agent in ["added_scope", "added_read_scope"] {
        empty_recall(&app, "notes", ID, agent).await;
    }
    assert_error(
        memory(
            &app,
            "notes",
            "remember",
            ID,
            &remember_body("added_read_scope", "read-scope-write"),
        )
        .await,
        403,
        "forbidden_scope",
    );
    let (status, body) = memory(
        &app,
        "notes",
        "remember",
        ID,
        &remember_body("added_scope", "new-scope-write"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    new["bindings"][0]["generation"] = json!(3);
    new["bindings"][0]["grants"][0]["read_scopes"] = json!([]);
    rig.write_bindings(&new);
    reload(&app, 1, 1).await;
    assert_error(
        memory(
            &app,
            "notes",
            "recall",
            ID,
            &recall_body("added_read_scope"),
        )
        .await,
        403,
        "forbidden_scope",
    );
    service.close().await.unwrap();
}

#[tokio::test]
async fn live_grants_test_011_adding_and_replacing_principals_apply_live() {
    let rig = Rig::new();
    let mut file = bindings(1, "shared_scope", false);
    let (app, service) = rig.start(Some(&file), false).await.unwrap();
    assert_error(
        memory(
            &app,
            "notes",
            "recall",
            OTHER_ID,
            &recall_body("shared_scope"),
        )
        .await,
        403,
        "forbidden_scope",
    );
    file["bindings"][0]["generation"] = json!(2);
    file["bindings"][0]["grants"]
        .as_array_mut()
        .unwrap()
        .push(json!({"principal": OTHER_ID, "scopes": [{"agent": "shared_scope"}]}));
    rig.write_bindings(&file);
    reload(&app, 1, 1).await;
    for id in [ID, OTHER_ID] {
        empty_recall(&app, "notes", id, "shared_scope").await;
    }
    file["bindings"][0]["generation"] = json!(3);
    file["bindings"][0]["grants"]
        .as_array_mut()
        .unwrap()
        .remove(0);
    rig.write_bindings(&file);
    reload(&app, 1, 1).await;
    assert_error(
        memory(&app, "notes", "recall", ID, &recall_body("shared_scope")).await,
        403,
        "forbidden_scope",
    );
    empty_recall(&app, "notes", OTHER_ID, "shared_scope").await;
    let (status, body) = memory(
        &app,
        "notes",
        "remember",
        OTHER_ID,
        &remember_body("shared_scope", "replacement-principal-write"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    service.close().await.unwrap();
}

#[tokio::test]
async fn live_grants_test_011_raw_access_revocation_and_restoration_apply_live() {
    let rig = Rig::new();
    let mut file = bindings(1, "raw_scope", true);
    let (app, service) = rig.start(Some(&file), false).await.unwrap();
    for (generation, allowed) in [(1, true), (2, false), (3, true)] {
        if generation > 1 {
            file["bindings"][0]["generation"] = json!(generation);
            file["bindings"][0]["grants"][0]["raw_access"] = json!(allowed);
            rig.write_bindings(&file);
            reload(&app, 1, 1).await;
        }
        for (path, missing_status, code) in [
            ("/v1/graphs/notes_graph", 409, "graph_not_found"),
            ("/v1/collections/notes_ev", 404, "collection_not_found"),
        ] {
            let answer = call(&app, "GET", path, &credential(ID), None).await;
            if allowed {
                assert_eq!(answer.0, 200, "{path}: {}", answer.1);
            } else {
                assert_error(answer, missing_status, code);
            }
        }
        empty_recall(&app, "notes", ID, "raw_scope").await;
    }
    service.close().await.unwrap();
}

#[tokio::test]
async fn live_grants_test_012_generation_and_lifecycle_refusals_write_nothing() {
    let rig = Rig::new();
    let old = bindings(2, "current_scope", true);
    let (app, service) = rig.start(Some(&old), false).await.unwrap();
    // PRD-ambiguity: TEST-012's "same generation" means changed grants;
    // REQ-010 accepts an identical file, exercised by TEST-013.
    for file in [
        bindings(2, "different_scope", true),
        bindings(1, "different_scope", true),
        bindings(1, "current_scope", true),
        bindings(3, "current_scope", true),
    ] {
        rig.write_bindings(&file);
        assert_refused_unchanged(&rig, &app, ADMIN, 409, "memory_bindings_conflict").await;
        empty_recall(&app, "notes", ID, "current_scope").await;
        assert_error(
            memory(&app, "notes", "recall", ID, &recall_body("different_scope")).await,
            403,
            "forbidden_scope",
        );
    }
    let mut changes = Vec::new();
    for (field, value) in [
        ("receipt_window_ms", json!(WINDOW + 1)),
        ("graph", json!("plain_graph")),
        ("evidence_collection", json!("tasks_ev")),
        ("tenant", json!("0123456789abcdef0123456789abcdef")),
        ("binding", json!("renamed_notes")),
    ] {
        let mut file = old.clone();
        file["bindings"][0]["generation"] = json!(3);
        file["bindings"][0][field] = value;
        changes.push(file);
    }
    let mut retired = old.clone();
    retired["bindings"][0]["generation"] = json!(3);
    retired["bindings"][0]["state"] = json!("retired");
    retired["bindings"][0]["retired_at_ms"] = json!(1);
    changes.push(retired);
    let mut added = two_bindings(2, "current_scope");
    added["bindings"][1]["generation"] = json!(1);
    changes.push(added);
    let mut removed = old.clone();
    removed["bindings"] = json!([]);
    changes.push(removed);
    for file in changes {
        rig.write_bindings(&file);
        assert_refused_unchanged(&rig, &app, ADMIN, 409, "memory_restart_required").await;
        empty_recall(&app, "notes", ID, "current_scope").await;
    }
    service.close().await.unwrap();
}

#[tokio::test]
async fn live_grants_test_012_invalid_files_and_callers_write_nothing() {
    let rig = Rig::new();
    let old = bindings(1, "current_scope", true);
    let (app, service) = rig.start(Some(&old), false).await.unwrap();
    let mut unknown_field = old.clone();
    unknown_field["unexpected"] = json!(true);
    for text in [
        "not JSON".to_owned(),
        r#"{"version":"memory-bindings-v1","bindings":["#.to_owned(),
        unknown_field.to_string(),
    ] {
        rig.write_text(&text);
        assert_refused_unchanged(&rig, &app, ADMIN, 422, "invalid_input").await;
        empty_recall(&app, "notes", ID, "current_scope").await;
    }
    let missing = rig.directory.path().join("memory-bindings.json");
    std::fs::remove_file(&missing).unwrap();
    assert_refused_unchanged(&rig, &app, ADMIN, 422, "invalid_input").await;
    rig.write_bindings(&bindings(2, "unauthorized_scope", true));
    for token in [READ_ONLY.to_owned(), credential(ID)] {
        let records = raw_records(&app).await;
        let size = rig.size();
        let (status, body) = call(&app, "POST", RELOAD, &token, None).await;
        assert_eq!(status, 403, "{body}");
        if token != READ_ONLY {
            assert_eq!(body["error"]["code"], "forbidden_scope", "{body}");
        }
        assert_eq!(raw_records(&app).await, records);
        assert_eq!(rig.size(), size);
        empty_recall(&app, "notes", ID, "current_scope").await;
        assert_error(
            memory(
                &app,
                "notes",
                "recall",
                ID,
                &recall_body("unauthorized_scope"),
            )
            .await,
            403,
            "forbidden_scope",
        );
    }
    service.close().await.unwrap();
}

#[tokio::test]
async fn live_grants_test_012_no_configured_file_is_unavailable() {
    for file in [
        None,
        Some(json!({"version": "memory-bindings-v1", "bindings": []})),
    ] {
        let rig = Rig::new();
        let (app, service) = rig.start(file.as_ref(), false).await.unwrap();
        assert_refused_unchanged(&rig, &app, ADMIN, 409, "memory_reload_unavailable").await;
        let (status, capabilities) = call(&app, "GET", "/v1/capabilities", ADMIN, None).await;
        assert_eq!(status, 200, "{capabilities}");
        assert_eq!(capabilities["features"]["memory_bindings_reload"], false);
        assert!(
            capabilities["routes"]
                .as_array()
                .unwrap()
                .contains(&json!(RELOAD))
        );
        service.close().await.unwrap();
        assert_eq!(rig.records(), vec![None, None]);
    }
}

#[tokio::test]
async fn live_grants_test_013_response_capabilities_and_restart_persistence() {
    let rig = Rig::new();
    let old = bindings(1, "private_old_scope", true);
    let new = bindings(2, "private_new_scope", true);
    let (app, service) = rig.start(Some(&old), false).await.unwrap();
    let (status, capabilities) = call(&app, "GET", "/v1/capabilities", ADMIN, None).await;
    assert_eq!(status, 200, "{capabilities}");
    assert_eq!(capabilities["features"]["memory_bindings_reload"], true);
    assert!(
        capabilities["routes"]
            .as_array()
            .unwrap()
            .contains(&json!(RELOAD))
    );
    rig.write_bindings(&new);
    let body = reload(&app, 1, 1).await;
    let serialized = body.to_string();
    for private in [
        "notes",
        "@root",
        ID,
        "private_old_scope",
        "private_new_scope",
    ] {
        assert!(!serialized.contains(private), "reload leaked {private}");
    }
    assert_eq!(
        std::fs::read_to_string(rig.directory.path().join("memory-bindings.json")).unwrap(),
        new.to_string(),
        "reload never rewrites its source file"
    );
    let records = raw_records(&app).await;
    let size = rig.size();
    reload(&app, 1, 0).await;
    assert_eq!(rig.size(), size, "an unchanged reload adds no commit");
    assert_eq!(raw_records(&app).await, records);
    service.close().await.unwrap();
    let records = rig.records();
    let size = rig.size();
    rig.run(&new, async |app| {
        empty_recall(app, "notes", ID, "private_new_scope").await;
    })
    .await;
    assert_eq!(rig.size(), size, "a same-file restart adds no commit");
    assert_eq!(rig.records(), records);
    assert_eq!(
        rig.refused(&old).await,
        "a memory binding changed without a higher generation"
    );
    assert_eq!(rig.size(), size, "a reverted restart writes nothing");
    assert_eq!(rig.records(), records);
}

#[tokio::test]
async fn live_grants_test_014_read_only_reopen_installs_grants_without_writing() {
    let rig = Rig::new();
    let mut old = bindings(1, "first_scope", false);
    old["bindings"][0]["grants"]
        .as_array_mut()
        .unwrap()
        .push(json!({"principal": OTHER_ID, "scopes": [{"agent": "second_scope"}]}));
    // Both scopes are provisioned by the primary; a read-only reload must
    // neither provision scopes nor update primary-owned records.
    rig.run(&old, async |_| {}).await;
    let records = rig.records();
    let size = rig.size();
    let (app, service) = rig.start(Some(&old), true).await.unwrap();
    empty_recall(&app, "notes", ID, "first_scope").await;
    assert_error(
        memory(&app, "notes", "recall", ID, &recall_body("second_scope")).await,
        403,
        "forbidden_scope",
    );
    assert_error(
        memory(
            &app,
            "notes",
            "remember",
            ID,
            &remember_body("first_scope", "read-only-before"),
        )
        .await,
        403,
        "read_only",
    );
    let mut new = bindings(2, "second_scope", false);
    new["bindings"][0]["grants"]
        .as_array_mut()
        .unwrap()
        .push(json!({"principal": OTHER_ID, "scopes": [{"agent": "first_scope"}]}));
    rig.write_bindings(&new);
    reload(&app, 1, 1).await;
    assert_error(
        memory(&app, "notes", "recall", ID, &recall_body("first_scope")).await,
        403,
        "forbidden_scope",
    );
    empty_recall(&app, "notes", ID, "second_scope").await;
    empty_recall(&app, "notes", OTHER_ID, "first_scope").await;
    assert_error(
        memory(
            &app,
            "notes",
            "remember",
            ID,
            &remember_body("second_scope", "read-only-after"),
        )
        .await,
        403,
        "read_only",
    );
    assert_eq!(rig.size(), size, "read-only reload appends nothing");
    assert_eq!(
        rig.records(),
        records,
        "read-only records stay primary-owned"
    );
    service.close().await.unwrap();
    assert_eq!(rig.size(), size);
    assert_eq!(rig.records(), records);
}

// Preflight and status of a reload (feature-memory-preflight PRD, sections
// 3.3 and 3.4, TEST-A09 to TEST-A12 and TEST-A14).

const PREFLIGHT: &str = "/v1/admin/memory/bindings/preflight";
const STATUS: &str = "/v1/admin/memory/bindings/status";
/// The scope sentinel of the confidentiality sweep; the tenant, binding,
/// principal, graph and evidence names of [`bindings`] are the others.
const SENTINEL_SCOPE: &str = "sentinel_scope_a09";

/// The newest structured log sequence number.
async fn log_seq(app: &Router) -> u64 {
    let (status, body) = call(app, "GET", "/v1/admin/logs?limit=1", ADMIN, None).await;
    assert_eq!(status, 200, "{body}");
    body["next_seq"].as_u64().unwrap()
}

/// The audit ring's entries other than the router's generic per-request
/// access records (method, route, status, role, timing), which every admin
/// request gets: what a handler itself appends.
async fn audit_events(app: &Router) -> Vec<Value> {
    let (status, body) = call(app, "GET", "/v1/admin/audit", ADMIN, None).await;
    assert_eq!(status, 200, "{body}");
    body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item.get("route").is_none())
        .cloned()
        .collect()
}

/// Calls preflight or status as the administrator and expects 200 with a
/// body, an audit and log entries that name none of the sentinels. The log
/// buffer is shared by every test in this process, so the sweep checks what
/// new entries disclose; that the handlers write no log or audit event at all
/// is a separate static check
/// ([`preflight_and_status_write_no_log_or_audit_event`]).
async fn swept(rig: &Rig, app: &Router, method: &str, path: &str) -> Value {
    let since = log_seq(app).await;
    let audit = audit_events(app).await;
    let (status, body) = call(app, method, path, ADMIN, None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        audit_events(app).await,
        audit,
        "{path} appends no audit event"
    );
    let (_, ring) = call(app, "GET", "/v1/admin/audit", ADMIN, None).await;
    let (_, logs) = call(
        app,
        "GET",
        &format!("/v1/admin/logs?since={since}&limit=1000"),
        ADMIN,
        None,
    )
    .await;
    let directory = rig.directory.path().to_str().unwrap().to_owned();
    let leaf = rig.directory.path().file_name().unwrap().to_str().unwrap();
    let own = [SENTINEL_SCOPE, directory.as_str(), leaf];
    let shared = [
        "notes",
        "@root",
        ID,
        "notes_graph",
        "notes_ev",
        "memory-bindings.json",
    ];
    for text in [body.to_string(), ring["items"].to_string()] {
        for named in own.iter().chain(&shared) {
            assert!(!text.contains(named), "{path} disclosed {named}: {text}");
        }
    }
    // Other tests in this process log the shared fixture names, so the shared
    // buffer is swept for the sentinels only this test uses.
    let logged = logs["entries"].to_string();
    for named in own {
        assert!(!logged.contains(named), "{path} logged {named}: {logged}");
    }
    body
}

fn verdict(outcome: &str, bindings: Value, code: &str) -> Value {
    json!({"outcome": outcome, "bindings": bindings, "changed": null, "records": null,
           "error": {"code": code}})
}

/// The preflight body without the error message, which the in-crate pins
/// compare with reload's own.
fn without_message(mut body: Value) -> Value {
    if let Some(error) = body["error"].as_object_mut() {
        error.remove("message");
    }
    body
}

/// TEST-A09: every preflight outcome and status names no tenant, binding,
/// principal, scope, graph, evidence collection or path, and audits nothing.
#[tokio::test]
async fn preflight_test_a09_no_outcome_or_status_discloses_a_sentinel() {
    let rig = Rig::new();
    let old = bindings(1, SENTINEL_SCOPE, true);
    let (app, service) = rig.start(Some(&old), false).await.unwrap();
    assert_eq!(swept(&rig, &app, "GET", STATUS).await["file"], "agrees");
    rig.write_bindings(&bindings(2, "sentinel_scope_a09_b", true));
    assert_eq!(
        swept(&rig, &app, "POST", PREFLIGHT).await,
        json!({"outcome": "reload", "bindings": 1, "changed": 1, "records": 1, "error": null})
    );
    assert_eq!(swept(&rig, &app, "GET", STATUS).await["file"], "differs");
    reload(&app, 1, 1).await;
    assert_eq!(
        swept(&rig, &app, "POST", PREFLIGHT).await,
        json!({"outcome": "unchanged", "bindings": 1, "changed": 0, "records": 0, "error": null})
    );
    let mut renamed = bindings(3, SENTINEL_SCOPE, true);
    renamed["bindings"][0]["binding"] = json!("renamed");
    rig.write_bindings(&renamed);
    assert_eq!(
        without_message(swept(&rig, &app, "POST", PREFLIGHT).await),
        verdict("restart_required", json!(1), "memory_restart_required")
    );
    rig.write_bindings(&bindings(2, SENTINEL_SCOPE, true));
    assert_eq!(
        without_message(swept(&rig, &app, "POST", PREFLIGHT).await),
        verdict("refused", json!(1), "memory_bindings_conflict")
    );
    rig.write_text("not JSON");
    assert_eq!(
        without_message(swept(&rig, &app, "POST", PREFLIGHT).await),
        verdict("invalid", Value::Null, "invalid_input")
    );
    assert_eq!(swept(&rig, &app, "GET", STATUS).await["file"], "unreadable");
    service.close().await.unwrap();

    let (app, service) = rig.start(None, false).await.unwrap();
    assert_eq!(
        without_message(swept(&rig, &app, "POST", PREFLIGHT).await),
        verdict("unavailable", Value::Null, "memory_reload_unavailable")
    );
    assert_eq!(
        swept(&rig, &app, "GET", STATUS).await,
        json!({"configured": false, "reloadable": false, "bindings": 0, "file": "none",
               "drained": true})
    );
    service.close().await.unwrap();
}

/// The preflight and status handlers, and the check they share with reload,
/// contain no log or audit write: their non-disclosure in TEST-A09 is not
/// taken as proof that nothing is logged.
#[test]
fn preflight_and_status_write_no_log_or_audit_event() {
    let source = include_str!("../src/server/memory/reload.rs").replace("\r\n", "\n");
    for function in [
        "fn check(",
        "fn changed(",
        "fn preflight_bindings(",
        "fn verdict(",
        "fn bindings_status(",
    ] {
        let start = source
            .find(function)
            .unwrap_or_else(|| panic!("{function} exists"));
        let end = start + source[start..].find("\n}\n").unwrap();
        let body = &source[start..end];
        for write in [
            "logs::",
            "audit",
            ".append(",
            ".record(",
            "eprintln!",
            "println!",
        ] {
            assert!(!body.contains(write), "{function} writes through {write}");
        }
    }
}

/// TEST-A10: both routes are admin-only.
#[tokio::test]
async fn preflight_test_a10_both_routes_are_admin_only() {
    let rig = Rig::new();
    let (app, service) = rig
        .start(Some(&bindings(1, "scope", true)), false)
        .await
        .unwrap();
    for (method, path) in [("POST", PREFLIGHT), ("GET", STATUS)] {
        assert_error(
            call(&app, method, path, READ_ONLY, None).await,
            403,
            "read_only",
        );
        assert_error(
            call(&app, method, path, &credential(ID), None).await,
            403,
            "forbidden_scope",
        );
        let request = Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status().as_u16(), 401, "{path} without a token");
    }
    service.close().await.unwrap();
}

/// TEST-A11: on a read-only database preflight counts no record to write,
/// and status answers.
#[tokio::test]
async fn preflight_test_a11_a_read_only_database_writes_no_record() {
    let rig = Rig::new();
    let old = bindings(1, "first_scope", true);
    rig.run(&old, async |_| {}).await;
    let (app, service) = rig.start(Some(&old), true).await.unwrap();
    rig.write_bindings(&bindings(2, "first_scope_b", true));
    let (status, body) = call(&app, "POST", PREFLIGHT, ADMIN, None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        json!({"outcome": "reload", "bindings": 1, "changed": 1, "records": 0, "error": null})
    );
    let (status, body) = call(&app, "GET", STATUS, ADMIN, None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["file"], "differs", "{body}");
    service.close().await.unwrap();
}

async fn file_status(app: &Router) -> Value {
    let (status, body) = call(app, "GET", STATUS, ADMIN, None).await;
    assert_eq!(status, 200, "{body}");
    body["file"].clone()
}

/// TEST-A12: status compares the served grants with the file as reload counts
/// `changed`: the order of bindings does not matter, the order of grants does.
#[tokio::test]
async fn preflight_test_a12_status_compares_the_file_as_reload_counts_changes() {
    let rig = Rig::new();
    let grants = |generation: u64| {
        let mut file = two_bindings(generation, "scope");
        file["bindings"][0]["grants"] = json!([
            {"principal": ID, "scopes": [{"agent": "scope"}]},
            {"principal": OTHER_ID, "scopes": [{"agent": "scope"}]},
        ]);
        // Only the first binding's grants change, so only it advances.
        file["bindings"][1]["generation"] = json!(1);
        file
    };
    let old = grants(1);
    let (app, service) = rig.start(Some(&old), false).await.unwrap();
    assert_eq!(file_status(&app).await, "agrees");
    let mut new = grants(2);
    new["bindings"][0]["grants"][0]["scopes"] = json!([{"agent": "other"}]);
    rig.write_bindings(&new);
    assert_eq!(file_status(&app).await, "differs");
    reload(&app, 2, 1).await;
    assert_eq!(file_status(&app).await, "agrees");
    let mut reordered = new.clone();
    reordered["bindings"].as_array_mut().unwrap().reverse();
    rig.write_bindings(&reordered);
    assert_eq!(file_status(&app).await, "agrees", "binding order is free");
    let mut regranted = new.clone();
    regranted["bindings"][0]["grants"]
        .as_array_mut()
        .unwrap()
        .reverse();
    rig.write_bindings(&regranted);
    assert_eq!(file_status(&app).await, "differs", "grant order counts");
    service.close().await.unwrap();
}

/// TEST-A14: capabilities list both routes, and the reload feature flag
/// equals status `reloadable` in every configuration.
#[tokio::test]
async fn preflight_test_a14_capabilities_list_both_routes_and_agree_with_status() {
    let rig = Rig::new();
    let file = bindings(1, "scope", true);
    rig.run(&file, async |_| {}).await;
    let empty = json!({"version": "memory-bindings-v1", "bindings": []});
    for (file, read_only, reloadable) in [
        (Some(&file), false, true),
        (Some(&file), true, true),
        (None, false, false),
        (Some(&empty), false, false),
    ] {
        let (app, service) = rig.start(file, read_only).await.unwrap();
        let (status, capabilities) = call(&app, "GET", "/v1/capabilities", ADMIN, None).await;
        assert_eq!(status, 200, "{capabilities}");
        let routes = capabilities["routes"].as_array().unwrap();
        for path in [RELOAD, PREFLIGHT, STATUS] {
            assert!(routes.contains(&json!(path)), "{path} listed");
        }
        let (status, body) = call(&app, "GET", STATUS, ADMIN, None).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["reloadable"], reloadable, "{body}");
        assert_eq!(
            capabilities["features"]["memory_bindings_reload"],
            body["reloadable"]
        );
        service.close().await.unwrap();
    }
}
