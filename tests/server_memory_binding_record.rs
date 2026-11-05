#![cfg(feature = "server")]
//! Binding records across restarts, through the real startup path
//! (`configured_admin_router`) on a file database: the receipt window,
//! generation and names a binding was given are kept in the bound graph and
//! checked against the file at each start (mapping section 4), a graph is
//! adopted only for the binding it was recorded for, and a database that holds
//! memory is never served raw because its bindings file is gone. Expectations
//! come from the 0.7.1 design note
//! (`docs/projects/roadmap/m71-memory-followups.md`, section 1), the adapter
//! mapping and the serving contract.

use std::{path::PathBuf, time::Duration};

use axum::{Router, body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tower::ServiceExt;
use vlite::{
    CollectionSpec, Database, Expected, Metadata, MetadataValue,
    property_graph::GraphOp,
    server::{BearerToken, ServerOptions, Service, configured_admin_router},
};

const ADMIN: &str = "fake_binding_record_admin_token_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const ID: &str = "abcdefghij27";
const SECRET: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const WINDOW: u64 = 86_400_000;
const CHANGED_WINDOW: &str =
    "a memory binding changed its graph, evidence collection or receipt window";
const NOT_ADVANCED: &str = "a memory binding changed without a higher generation";
const IN_USE: &str = "a memory binding resource already holds ordinary data";
const INVALID_RECORD: &str = "a memory binding record in the database is not valid";

fn bindings(generation: u64, window: u64, agent: &str) -> String {
    format!(
        r#"{{"version":"memory-bindings-v1","bindings":[{{"binding":"notes","tenant":"@root","generation":{generation},"state":"active","graph":"notes_graph","evidence_collection":"notes_ev","schema_version":"memory-schema-v1","ranking_version":"memory-ranking-v1","digest_version":"memory-request-v1","receipt_window_ms":{window},"grants":[{{"principal":"{ID}","scopes":[{{"agent":"{agent}"}}]}}]}}]}}"#
    )
}

struct Rig {
    directory: TempDir,
    path: PathBuf,
}

impl Rig {
    /// A file database with the bound graph, the evidence collection, one
    /// unrelated graph, and a scoped credential for the bound resources.
    fn new() -> Self {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("memory.vlite");
        let mut db = Database::create(&path, 2).unwrap();
        db.create_graph("notes_graph").unwrap();
        db.create_graph("plain_graph").unwrap();
        db.create_collection(
            "notes_ev",
            CollectionSpec {
                dimensions: 2,
                settings: Default::default(),
            },
        )
        .unwrap();
        let evidence = db
            .collections()
            .unwrap()
            .into_iter()
            .find(|info| info.name == "notes_ev")
            .unwrap()
            .id;
        let credentials = json!({
            "schema": 1,
            "credentials": [{
                "id": ID,
                "name": "fake binding record credential",
                "all": false,
                "collections": [{"name": "notes_ev", "id": evidence}],
                "operations": ["read", "search", "write", "graph_read", "graph_write"],
                "graphs": ["notes_graph"],
                "lineage": format!("{:016x}", db.lineage().unwrap()),
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
        db.close().unwrap();
        Self { directory, path }
    }

    fn open(&self) -> Database {
        Database::open(&self.path).unwrap()
    }

    fn size(&self) -> u64 {
        std::fs::metadata(&self.path).unwrap().len()
    }

    /// One server start on the file; `file` is the bindings file text.
    async fn start(
        &self,
        file: Option<&str>,
        tweak: impl FnOnce(&mut ServerOptions),
    ) -> std::io::Result<(Router, Service)> {
        let memory_bindings_file = file.map(|text| {
            let path = self.directory.path().join("memory-bindings.json");
            std::fs::write(&path, text).unwrap();
            path
        });
        let service = Service::new(self.open());
        let mut options = ServerOptions {
            admin_token: Some(BearerToken::new(ADMIN).unwrap()),
            admin_directory: Some(self.directory.path().into()),
            memory_bindings_file,
            ..Default::default()
        };
        tweak(&mut options);
        match configured_admin_router(service.clone(), options) {
            Ok(router) => Ok((router, service)),
            Err(error) => {
                // A refused start leaves the file open until it is closed.
                service.close().await.unwrap();
                Err(error)
            }
        }
    }

    async fn run<T>(&self, file: Option<&str>, work: impl AsyncFnOnce(&Router) -> T) -> T {
        let (router, service) = self
            .start(file, |_| {})
            .await
            .expect("the start is accepted");
        let result = work(&router).await;
        service.close().await.unwrap();
        result
    }

    async fn refused(&self, file: Option<&str>) -> String {
        self.refused_with(file, |_| {}).await
    }

    async fn refused_with(
        &self,
        file: Option<&str>,
        tweak: impl FnOnce(&mut ServerOptions),
    ) -> String {
        match self.start(file, tweak).await {
            Ok(_) => panic!("the start must be refused"),
            Err(error) => error.to_string(),
        }
    }
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
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn credential() -> String {
    format!("vlk_{ID}_{SECRET}")
}

async fn memory(app: &Router, operation: &str, body: &Value) -> (u16, Value) {
    call(
        app,
        "POST",
        &format!("/v1/memory/notes/{operation}"),
        &credential(),
        Some(body),
    )
    .await
}

fn remember_body(agent: &str, key: &str) -> Value {
    json!({
        "scope": {"agent": agent}, "idempotency_key": key, "deadline_ms": 5000,
        "items": [{
            "id": "e1", "kind": "entity", "content": "kept across restarts", "observed_at_ms": "90",
            "source": {"source_id": "s1", "source_kind": "caller"},
        }],
    })
}

fn record(db: &Database) -> Option<vlite::property_graph::NodeView> {
    db.graph("notes_graph")
        .unwrap()
        .get_node("binding:v1")
        .unwrap()
        .output
}

fn text(value: &str) -> MetadataValue {
    MetadataValue::String(value.into())
}

#[tokio::test]
async fn the_first_start_records_the_binding_and_a_restart_changes_nothing() {
    let rig = Rig::new();
    let file = bindings(1, WINDOW, "a");
    rig.run(Some(&file), async |_| {}).await;
    let db = rig.open();
    let node = record(&db).expect("the record");
    assert_eq!(
        node.labels.iter().collect::<Vec<_>>(),
        ["VliteMemoryBinding"]
    );
    assert_eq!(node.properties["binding"], text("notes"));
    assert_eq!(node.properties["generation"], text("1"));
    assert_eq!(
        node.properties["receipt_window_ms"],
        text(&WINDOW.to_string())
    );
    assert_eq!(node.properties["graph"], text("notes_graph"));
    assert_eq!(node.properties["evidence"], text("notes_ev"));
    db.close().unwrap();
    // Opening the file at the same point, a restart with the same file
    // writes nothing: the record node is exactly the same node.
    rig.run(Some(&file), async |_| {}).await;
    let again = rig.open();
    assert_eq!(record(&again), Some(node));
    again.close().unwrap();
}

#[tokio::test]
async fn a_retry_after_a_restart_still_replays_and_a_changed_window_is_refused() {
    let rig = Rig::new();
    let file = bindings(1, WINDOW, "a");
    let first = rig
        .run(Some(&file), async |app| {
            let (status, body) = memory(app, "remember", &remember_body("a", "k1")).await;
            assert_eq!(status, 200, "{body}");
            assert_eq!(body["replayed"], false);
            body
        })
        .await;
    // The retry after the restart is inside its window and answers from the receipt.
    rig.run(Some(&file), async |app| {
        let (status, retry) = memory(app, "remember", &remember_body("a", "k1")).await;
        assert_eq!(status, 200, "{retry}");
        assert_eq!(retry["replayed"], true);
        assert_eq!(retry["receipt"], first["receipt"]);
    })
    .await;
    // A changed window would turn that retry into idempotency_conflict, so the
    // start is refused, names nothing, and leaves the file as it was.
    let size = rig.size();
    for window in [WINDOW + 1, WINDOW - 1, 1] {
        let message = rig.refused(Some(&bindings(1, window, "a"))).await;
        assert_eq!(message, CHANGED_WINDOW);
        assert!(!message.contains("notes"));
        assert_eq!(rig.size(), size, "a refused start writes nothing");
    }
    rig.run(Some(&file), async |app| {
        let (status, retry) = memory(app, "remember", &remember_body("a", "k1")).await;
        assert_eq!(status, 200, "{retry}");
        assert_eq!(retry["replayed"], true);
    })
    .await;
}

#[tokio::test]
async fn new_grants_need_a_higher_generation_and_the_old_scope_stops_working() {
    let rig = Rig::new();
    rig.run(Some(&bindings(1, WINDOW, "a")), async |app| {
        let (status, body) = memory(app, "remember", &remember_body("a", "k1")).await;
        assert_eq!(status, 200, "{body}");
    })
    .await;
    // Different grants under the same generation are refused.
    assert_eq!(
        rig.refused(Some(&bindings(1, WINDOW, "b"))).await,
        NOT_ADVANCED
    );
    // Under the next generation the new scope is provisioned and the old one is gone.
    rig.run(Some(&bindings(2, WINDOW, "b")), async |app| {
        let (status, body) = memory(app, "remember", &remember_body("b", "k2")).await;
        assert_eq!(status, 200, "{body}");
        let (status, body) = memory(app, "remember", &remember_body("a", "k3")).await;
        assert_eq!(status, 403, "{body}");
        assert_eq!(body["error"]["code"], "forbidden_scope");
    })
    .await;
    // A generation that goes back is refused, with its grants or without.
    assert_eq!(
        rig.refused(Some(&bindings(1, WINDOW, "b"))).await,
        "a memory binding changed its generation without another change"
    );
    assert_eq!(
        rig.refused(Some(&bindings(1, WINDOW, "a"))).await,
        NOT_ADVANCED
    );
    let db = rig.open();
    assert_eq!(record(&db).unwrap().properties["generation"], text("2"));
    db.close().unwrap();
}

#[tokio::test]
async fn removing_the_bindings_file_keeps_the_memory_reserved() {
    let rig = Rig::new();
    let file = bindings(1, WINDOW, "a");
    rig.run(Some(&file), async |app| {
        let (status, body) = memory(app, "remember", &remember_body("a", "k1")).await;
        assert_eq!(status, 200, "{body}");
    })
    .await;
    // No file: the database still holds a record, so its graph and evidence
    // collection stay reserved, with no grants, and no memory route is served.
    rig.run(None, async |app| {
        let (status, body) = call(app, "GET", "/v1/graphs/notes_graph", ADMIN, None).await;
        assert_eq!(
            (status, &body["error"]["code"]),
            (409, &json!("graph_not_found")),
            "{body}"
        );
        let (status, body) = call(app, "GET", "/v1/collections/notes_ev", ADMIN, None).await;
        assert_eq!(
            (status, &body["error"]["code"]),
            (404, &json!("collection_not_found")),
            "{body}"
        );
        let (status, body) = call(app, "GET", "/v1/graphs", ADMIN, None).await;
        assert_eq!(status, 200, "{body}");
        let names: Vec<&str> = body["graphs"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|graph| graph["name"].as_str())
            .collect();
        assert!(!names.contains(&"notes_graph"), "{names:?}");
        assert!(names.contains(&"plain_graph"), "{names:?}");
        let (status, _) = call(app, "GET", "/v1/graphs/plain_graph", ADMIN, None).await;
        assert_eq!(status, 200);
        let recall = json!({"scope": {"agent": "a"}, "entity_seeds": ["e1"]});
        let (status, _) = call(app, "POST", "/v1/memory/notes/recall", ADMIN, Some(&recall)).await;
        assert_eq!(status, 404, "memory routes are not mounted without a file");
    })
    .await;
    // The listeners that cannot enforce the reservation refuse to start.
    let grpc = rig
        .refused_with(None, |options| options.grpc_enabled = true)
        .await;
    assert_eq!(
        grpc,
        "gRPC listener is disabled while the database holds reserved memory resources"
    );
    let maintenance = rig
        .refused_with(None, |options| {
            options.auto_maintenance_interval = Some(Duration::from_secs(60));
        })
        .await;
    assert_eq!(
        maintenance,
        "automatic index maintenance is disabled while the database holds reserved memory resources"
    );
    // With the file again everything is where it was.
    rig.run(Some(&file), async |app| {
        let (status, body) = memory(
            app,
            "recall",
            &json!({"scope": {"agent": "a"}, "entity_seeds": ["e1"]}),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["memories"][0]["id"], "e1");
        assert_eq!(body["memories"][0]["content"], "kept across restarts");
    })
    .await;
}

#[tokio::test]
async fn a_database_that_never_held_memory_starts_as_before_with_any_listener() {
    let rig = Rig::new();
    let (router, service) = rig
        .start(None, |options| {
            options.auto_maintenance_interval = Some(Duration::from_secs(60));
        })
        .await
        .expect("no records, no reservation");
    let (status, _) = call(&router, "GET", "/v1/graphs/notes_graph", ADMIN, None).await;
    assert_eq!(status, 200);
    service.close().await.unwrap();
    let (router, service) = rig
        .start(None, |options| options.grpc_enabled = true)
        .await
        .expect("no records, no reservation");
    drop(router);
    service.close().await.unwrap();
}

#[tokio::test]
async fn a_corrupt_record_refuses_startup_with_or_without_a_file() {
    let rig = Rig::new();
    let file = bindings(1, WINDOW, "a");
    rig.run(Some(&file), async |_| {}).await;
    let mut db = rig.open();
    let node = record(&db).unwrap();
    let mut properties: Metadata = node.properties.clone();
    properties.insert("extra".into(), text("x"));
    db.graph_mut("notes_graph")
        .unwrap()
        .write_batch(&[GraphOp::UpsertNode {
            key: "binding:v1".into(),
            labels: vec!["VliteMemoryBinding".into()],
            properties,
            expected: Expected::Revision(node.revision),
        }])
        .unwrap();
    db.close().unwrap();
    assert_eq!(rig.refused(Some(&file)).await, INVALID_RECORD);
    assert_eq!(rig.refused(None).await, INVALID_RECORD);
}

#[tokio::test]
async fn a_forged_scope_label_does_not_make_an_ordinary_graph_adoptable() {
    let rig = Rig::new();
    let mut db = rig.open();
    db.graph_mut("notes_graph")
        .unwrap()
        .write_batch(&[
            GraphOp::CreateNode {
                key: "ordinary".into(),
                labels: vec!["Person".into()],
                properties: Metadata::new(),
            },
            GraphOp::CreateNode {
                key: "scope:v1:forged".into(),
                labels: vec!["VliteMemoryScope".into()],
                properties: Metadata::new(),
            },
        ])
        .unwrap();
    db.close().unwrap();
    assert_eq!(rig.refused(Some(&bindings(1, WINDOW, "a"))).await, IN_USE);
}

#[tokio::test]
async fn a_graph_adopted_before_records_existed_is_adopted_once_and_gets_its_record() {
    let rig = Rig::new();
    let file = bindings(1, WINDOW, "a");
    rig.run(Some(&file), async |app| {
        let (status, body) = memory(app, "remember", &remember_body("a", "k1")).await;
        assert_eq!(status, 200, "{body}");
    })
    .await;
    // What the first release left behind: memory nodes, no record.
    let mut db = rig.open();
    let node = record(&db).unwrap();
    db.graph_mut("notes_graph")
        .unwrap()
        .delete_node("binding:v1", Expected::Revision(node.revision))
        .unwrap();
    assert!(record(&db).is_none());
    // Evidence loaded before it was attached does not stop the upgrade.
    db.collection_mut("notes_ev")
        .unwrap()
        .put(7, &[1.0, 0.0], b"loaded before it was attached")
        .unwrap();
    db.close().unwrap();
    rig.run(Some(&file), async |app| {
        let (status, body) = memory(app, "remember", &remember_body("a", "k1")).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            body["replayed"], true,
            "the memory is the one that was kept"
        );
    })
    .await;
    let db = rig.open();
    let recorded = record(&db).expect("the upgrade wrote the record");
    assert_eq!(recorded.properties["binding"], text("notes"));
    assert_eq!(recorded.properties["generation"], text("1"));
    db.close().unwrap();
}
