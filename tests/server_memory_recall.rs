#![cfg(feature = "server")]
//! Agent-memory recall through the real server stack: a credentials file, the
//! route-permission middleware and a bindings file (mutation-closure pins for
//! the `recall` route). Per-field wire behavior is pinned by the library tests
//! in `src/server/memory/pins_recall.rs`; this file pins what only the full
//! stack decides: recall authorization is the intersection of the credential's
//! graph, evidence-collection and operation grants with the binding's exact
//! scope grants, and every denial is one indistinguishable 403.

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

const ADMIN: &str = "fake_memory_recall_admin_token_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const ID: &str = "abcdefghij27";
const RECALL: &str = "/v1/memory/notes/recall";
const BINDINGS: &str = r#"{"version":"memory-bindings-v1","bindings":[{"binding":"notes","tenant":"@root","generation":1,"state":"active","graph":"notes_graph","evidence_collection":"notes_ev","schema_version":"memory-schema-v1","ranking_version":"memory-ranking-v1","digest_version":"memory-request-v1","receipt_window_ms":86400000,"grants":[{"principal":"abcdefghij27","scopes":[{"agent":"a"}]}]}]}"#;

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

/// One credential: its operations, graph names (`None` for a credential
/// without graph operations, which the loader requires) and whether it names
/// the evidence collection `notes_ev` (otherwise it names only `default`).
fn start(
    directory: &TempDir,
    operations: &[&str],
    graphs: Option<&[&str]>,
    names_evidence: bool,
) -> (Router, String) {
    let db = database();
    let evidence = db
        .collections()
        .unwrap()
        .into_iter()
        .find(|info| info.name == "notes_ev")
        .unwrap()
        .id;
    let lineage = db.lineage().unwrap();
    let secret = "A".repeat(43);
    let collections = if names_evidence {
        json!([{"name": "notes_ev", "id": evidence}])
    } else {
        json!([{"name": "default", "id": 0}])
    };
    let mut credential = json!({
        "id": ID,
        "name": "fake recall credential",
        "all": false,
        "collections": collections,
        "operations": operations,
        "lineage": format!("{lineage:016x}"),
        "secret_sha256": format!("{:x}", Sha256::digest(secret.as_bytes())),
        "created_unix_ms": 0,
        "expires_unix_ms": 9_999_999_999_999u64,
        "revoked_unix_ms": null,
        "rotated_from": null
    });
    if let Some(graphs) = graphs {
        credential["graphs"] = json!(graphs);
    }
    let credentials = json!({"schema": 1, "credentials": [credential]});
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
            admin_directory: Some(directory.path().into()),
            memory_bindings_file: Some(path),
            ..Default::default()
        },
    )
    .unwrap();
    (app, format!("vlk_{ID}_{secret}"))
}

async fn post(app: &Router, path: &str, token: &str, body: &Value) -> (u16, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("<none>")
}

/// Contract 6: recall needs `graph_read` on the bound graph, `read` and
/// `search` on the evidence collection, and the collection named by the
/// credential; dropping any one is the same 403 `forbidden_scope`, and the
/// complete set (without any write grant) reaches the handler (the absent
/// seed is the authorized 404).
#[tokio::test]
async fn recall_authorization_is_the_intersection_of_every_grant() {
    // The 250 ms default budget is real time: a cold or loaded runner can spend it
    // before the handler plans, and the authorized request then answers 504.
    let limits = json!({"deadline_ms": 5000});
    let body = json!({"scope": {"agent": "a"}, "entity_seeds": ["absent"], "limits": limits});
    let directory = TempDir::new().unwrap();
    let (app, token) = start(
        &directory,
        &["read", "search", "graph_read"],
        Some(&["notes_graph"]),
        true,
    );
    let (status, authorized) = post(&app, RECALL, &token, &body).await;
    assert_eq!((status, code(&authorized)), (404, "memory_not_found"));

    let mut denials = Vec::new();
    for (name, operations, graphs, names_evidence) in [
        (
            "no read",
            vec!["search", "graph_read"],
            Some(vec!["notes_graph"]),
            true,
        ),
        (
            "no search",
            vec!["read", "graph_read"],
            Some(vec!["notes_graph"]),
            true,
        ),
        ("no graph_read", vec!["read", "search"], None, true),
        (
            "another graph",
            vec!["read", "search", "graph_read"],
            Some(vec!["other_graph"]),
            true,
        ),
        (
            "evidence not named",
            vec!["read", "search", "graph_read"],
            Some(vec!["notes_graph"]),
            false,
        ),
    ] {
        let directory = TempDir::new().unwrap();
        let (app, token) = start(&directory, &operations, graphs.as_deref(), names_evidence);
        let (status, denied) = post(&app, RECALL, &token, &body).await;
        assert_eq!(status, 403, "{name}: {denied}");
        assert_eq!(code(&denied), "forbidden_scope", "{name}");
        denials.push(denied);
    }
    let (status, unknown) = post(&app, "/v1/memory/absent/recall", &token, &body).await;
    assert_eq!(status, 403);
    let (status, ungranted) = post(
        &app,
        RECALL,
        &token,
        &json!({"scope": {"agent": "b"}, "entity_seeds": ["absent"], "limits": limits}),
    )
    .await;
    assert_eq!(status, 403);
    for denied in denials.iter().chain([&ungranted]) {
        assert_eq!(denied, &unknown, "one 403 body for every denial");
    }
}
