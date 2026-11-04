#![cfg(feature = "server")]
//! Agent-memory REST adapter. Startup skeleton milestone: the bindings file
//! is loaded and checked before any listener serves; no memory route is
//! mounted yet.

use axum::{Router, body::Body, http::Request};
use tempfile::TempDir;
use tower::ServiceExt;
use vlite::{
    CollectionSpec, CreateOptions, Database, FormatVersion,
    server::{BearerToken, ServerOptions, Service, configured_admin_router},
};

const ADMIN: &str = "fake_memory_rest_admin_token_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
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
    "/v1/memory/notes/receipts/forget/key",
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
async fn a_valid_bindings_file_starts_and_mounts_no_route_yet() {
    let directory = TempDir::new().unwrap();
    let app = start(
        &directory,
        database("notes_graph"),
        Some(BINDINGS.as_bytes()),
    )
    .unwrap();
    for path in MEMORY_PATHS {
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
