#![cfg(feature = "server")]

use axum::{Router, body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tower::ServiceExt;
use vlite::{
    CollectionSpec, CreateOptions, Database, FormatVersion,
    server::{BearerToken, ServerOptions, Service, configured_admin_router},
};

const ADMIN: &str = "fake_memory_guard_admin_token_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const READ_ONLY: &str = "fake_memory_guard_readonly_token_0123456789_ABCDEFGHIJKLMNOP";
const BINDINGS: &str = r#"{"version":"memory-bindings-v1","bindings":[{"binding":"notes","tenant":"@root","generation":1,"state":"active","graph":"notes_graph","evidence_collection":"notes_ev","schema_version":"memory-schema-v1","ranking_version":"memory-ranking-v1","digest_version":"memory-request-v1","receipt_window_ms":86400000,"grants":[{"principal":"abcdefghij27","scopes":[{"agent":"a"}],"raw_access":false}]}]}"#;

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
    db.create_collection(
        "ordinary",
        CollectionSpec {
            dimensions: 2,
            settings: Default::default(),
        },
    )
    .unwrap();
    db.create_graph("notes_graph").unwrap();
    db.create_graph("ordinary_graph").unwrap();
    db
}

fn start(
    directory: &TempDir,
    bindings: Option<&str>,
    grpc_enabled: bool,
) -> std::io::Result<Router> {
    let memory_bindings_file = bindings.map(|text| {
        let path = directory.path().join("memory-bindings.json");
        std::fs::write(&path, text).unwrap();
        path
    });
    configured_admin_router(
        Service::new(database()),
        ServerOptions {
            admin_token: Some(BearerToken::new(ADMIN).unwrap()),
            read_only_token: Some(BearerToken::new(READ_ONLY).unwrap()),
            admin_directory: Some(directory.path().into()),
            memory_bindings_file,
            grpc_enabled,
            ..Default::default()
        },
    )
}

async fn get(app: &Router, path: &str) -> (u16, Value) {
    call(app, "GET", path, Body::empty()).await
}

async fn call(app: &Router, method: &str, path: &str, body: Body) -> (u16, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", format!("Bearer {ADMIN}"))
                .header("content-type", "application/json")
                .body(body)
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let value = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    (status, value)
}

async fn call_token(
    app: &Router,
    method: &str,
    path: &str,
    body: Body,
    token: &str,
) -> (u16, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(body)
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let value = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    (status, value)
}

fn start_scoped(directory: &TempDir, raw_access: bool, collection: &str) -> (Router, String) {
    start_scoped_with_operations(directory, raw_access, collection, &["read", "write"])
}

fn start_scoped_with_operations(
    directory: &TempDir,
    raw_access: bool,
    collection: &str,
    operations: &[&str],
) -> (Router, String) {
    const ID: &str = "abcdefghij27";
    let secret = "A".repeat(43);
    let hash = format!("{:x}", Sha256::digest(secret.as_bytes()));
    let db = database();
    let collection_id = db
        .collections()
        .unwrap()
        .into_iter()
        .find(|info| info.name == collection)
        .unwrap()
        .id;
    let lineage = db.lineage().unwrap();
    let credentials = serde_json::json!({
        "schema": 1,
        "credentials": [{
            "id": ID,
            "name": "fake test credential",
            "all": false,
            "collections": [{"name": collection, "id": collection_id}],
            "operations": operations,
            "lineage": format!("{lineage:016x}"),
            "secret_sha256": hash,
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
    let bindings = BINDINGS.replace(
        "\"raw_access\":false",
        if raw_access {
            "\"raw_access\":true"
        } else {
            "\"raw_access\":false"
        },
    );
    let path = directory.path().join("memory-bindings.json");
    std::fs::write(&path, bindings).unwrap();
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

#[tokio::test]
async fn reserved_graph_and_collection_are_denied_and_lists_filter_them() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(&directory, Some(BINDINGS), false).unwrap();

    let (status, error) = get(&app, "/v1/graphs/notes_graph").await;
    assert_eq!(status, 403);
    assert_eq!(error["error"]["code"], "forbidden_scope");
    assert_eq!(
        error["error"]["message"],
        "credential scope does not permit this operation or collection"
    );
    assert_eq!(get(&app, "/v1/graphs/ordinary_graph").await.0, 200);

    let (status, error) = get(&app, "/v1/collections/notes_ev").await;
    assert_eq!(status, 403);
    assert_eq!(error["error"]["code"], "forbidden_scope");
    assert_eq!(
        error["error"]["message"],
        "credential scope does not permit this operation or collection"
    );
    assert_eq!(get(&app, "/v1/collections/ordinary").await.0, 200);
    assert_eq!(
        call(&app, "DELETE", "/v1/graphs/notes_graph", Body::empty())
            .await
            .0,
        403
    );
    assert_eq!(
        call(
            &app,
            "PUT",
            "/v1/collections/notes_ev/vectors/7",
            Body::from(r#"{"vector":[0.1,0.2],"payload_base64":"","metadata":{}}"#)
        )
        .await
        .0,
        403
    );
    assert_eq!(
        call(
            &app,
            "POST",
            "/v1/admin/webhooks",
            Body::from(r#"{"url":"http://127.0.0.1:8080/hook","secret":"fake-secret-for-test","collections":["notes_ev"]}"#)
        )
        .await
        .0,
        403
    );
    assert_eq!(get(&app, "/v1/database").await.0, 403);
    for (method, path, body) in [
        ("GET", "/v1/imports/1", Body::empty()),
        ("GET", "/v1/admin/replication/export", Body::empty()),
        ("GET", "/v1/admin/maintenance/auto", Body::empty()),
        ("GET", "/v1/admin/webhooks", Body::empty()),
        ("GET", "/v1/admin/webhooks/not-a-real-id", Body::empty()),
        ("PUT", "/v1/admin/maintenance/auto", Body::from("{}")),
        ("POST", "/v1/admin/maintenance/auto/tick", Body::empty()),
    ] {
        let (status, error) = call(&app, method, path, body).await;
        assert_eq!(status, 403, "{method} {path}");
        assert_eq!(error["error"]["code"], "forbidden_scope", "{method} {path}");
    }

    let (_, feed) = get(&app, "/v1/changes?limit=1000").await;
    assert!(feed["changes"].as_array().unwrap().iter().all(|change| {
        change["collection"] != "notes_ev" && change["change"]["graph"] != "notes_graph"
    }));
    assert!(feed["next_cursor"].as_str().is_some());

    let (_, graphs) = get(&app, "/v1/graphs").await;
    assert_eq!(graphs["graphs"].as_array().unwrap().len(), 1);
    assert_eq!(graphs["graphs"][0]["name"], "ordinary_graph");
    let (_, collections) = get(&app, "/v1/collections").await;
    let names = collections["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(names.contains(&"ordinary"));
    assert!(!names.contains(&"notes_ev"));
}

#[tokio::test]
async fn no_bindings_preserves_raw_access_and_nonempty_bindings_fail_closed_for_grpc() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(&directory, None, false).unwrap();
    assert_eq!(get(&app, "/v1/graphs/notes_graph").await.0, 200);
    assert_eq!(get(&app, "/v1/collections/notes_ev").await.0, 200);

    let directory = tempfile::tempdir().unwrap();
    let error = match start(&directory, Some(BINDINGS), true) {
        Ok(_) => panic!("gRPC listener must refuse configured bindings"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("gRPC listener is disabled"));

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("memory-bindings.json");
    std::fs::write(&path, BINDINGS).unwrap();
    let error = configured_admin_router(
        Service::new(database()),
        ServerOptions {
            admin_token: Some(BearerToken::new(ADMIN).unwrap()),
            admin_directory: Some(directory.path().into()),
            memory_bindings_file: Some(path),
            auto_maintenance_interval: Some(std::time::Duration::from_secs(60)),
            ..Default::default()
        },
    )
    .err()
    .expect("scheduled maintenance must refuse reserved resources");
    assert!(error.to_string().contains("automatic index maintenance"));
}

#[tokio::test]
async fn empty_bindings_are_noop_and_invalid_files_fail_without_echoing_content() {
    let directory = tempfile::tempdir().unwrap();
    let empty = r#"{"version":"memory-bindings-v1","bindings":[]}"#;
    let app = start(&directory, Some(empty), true).unwrap();
    assert_eq!(get(&app, "/v1/graphs/notes_graph").await.0, 200);
    assert_eq!(get(&app, "/v1/collections/notes_ev").await.0, 200);

    let no_file_directory = tempfile::tempdir().unwrap();
    let no_file = start(&no_file_directory, None, false).unwrap();
    for path in ["/v1/graphs", "/v1/collections", "/v1/changes?limit=100"] {
        let (empty_status, mut empty_response) = get(&app, path).await;
        let (default_status, mut default_response) = get(&no_file, path).await;
        assert_eq!(empty_status, default_status, "{path}");
        if path.starts_with("/v1/changes") {
            empty_response["next_cursor"] = Value::Null;
            default_response["next_cursor"] = Value::Null;
        }
        assert_eq!(empty_response, default_response, "{path}");
    }

    let directory = tempfile::tempdir().unwrap();
    let marker = "fake-secret-that-must-not-be-echoed";
    let invalid =
        format!(r#"{{"version":"memory-bindings-v1","secret":"{marker}","bindings":[]}}"#);
    let error = match start(&directory, Some(&invalid), false) {
        Ok(_) => panic!("unknown binding fields must reject startup"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("memory bindings file is invalid")
    );
    assert!(!error.to_string().contains(marker));
}

#[tokio::test]
async fn api_raw_access_requires_the_binding_grant_and_does_not_replace_collection_scope() {
    let admin_directory = tempfile::tempdir().unwrap();
    let admin = start(&admin_directory, Some(BINDINGS), false).unwrap();
    let (admin_status, admin_error) = get(&admin, "/v1/collections/notes_ev").await;
    assert_eq!(admin_status, 403);

    let directory = tempfile::tempdir().unwrap();
    let (app, token) = start_scoped(&directory, true, "notes_ev");
    assert_eq!(
        call_token(
            &app,
            "GET",
            "/v1/collections/notes_ev",
            Body::empty(),
            &token
        )
        .await
        .0,
        200
    );

    let directory = tempfile::tempdir().unwrap();
    let (app, token) = start_scoped(&directory, false, "notes_ev");
    let (status, error) = call_token(
        &app,
        "GET",
        "/v1/collections/notes_ev",
        Body::empty(),
        &token,
    )
    .await;
    assert_eq!(status, 403);
    assert_eq!(error, admin_error);

    let directory = tempfile::tempdir().unwrap();
    let (app, token) = start_scoped(&directory, true, "ordinary");
    assert_eq!(
        call_token(
            &app,
            "GET",
            "/v1/collections/notes_ev",
            Body::empty(),
            &token
        )
        .await
        .0,
        403
    );

    let directory = tempfile::tempdir().unwrap();
    let (app, token) = start_scoped_with_operations(&directory, true, "notes_ev", &["read"]);
    assert_eq!(
        call_token(
            &app,
            "GET",
            "/v1/collections/notes_ev",
            Body::empty(),
            &token
        )
        .await
        .0,
        200
    );
    assert_eq!(
        call_token(
            &app,
            "PUT",
            "/v1/collections/notes_ev/vectors/7",
            Body::from(r#"{"vector":[0.1,0.2],"payload_base64":"","metadata":{}}"#),
            &token
        )
        .await
        .0,
        403
    );
}

#[tokio::test]
async fn read_only_role_cannot_raw_access_a_reserved_collection() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(&directory, Some(BINDINGS), false).unwrap();
    assert_eq!(
        call_token(
            &app,
            "GET",
            "/v1/collections/notes_ev",
            Body::empty(),
            READ_ONLY
        )
        .await
        .0,
        403
    );
    assert_eq!(
        call_token(
            &app,
            "GET",
            "/v1/collections/ordinary",
            Body::empty(),
            READ_ONLY
        )
        .await
        .0,
        200
    );
}

#[test]
fn permission_resource_routes_are_classified_for_reservation_guards() {
    let server = include_str!("../src/server.rs");
    let start = server.find("fn permission(").expect("permission policy");
    let end = server[start..]
        .find("\nfn authorize(")
        .map(|offset| start + offset)
        .expect("authorize policy follows permission");
    let policy = &server[start..end];
    let mut classified = std::collections::BTreeSet::new();
    for (index, literal) in policy.split('"').enumerate() {
        if index % 2 == 0 || !literal.starts_with("/v1/") {
            continue;
        }
        let candidate = literal.contains('{')
            && !literal.starts_with("/v1/admin/backups")
            && !literal.starts_with("/v1/admin/tenants")
            && !literal.starts_with("/v1/admin/users")
            && !literal.starts_with("/v1/admin/credentials")
            || matches!(
                literal,
                "/v1/collections"
                    | "/v1/vectors"
                    | "/v1/database"
                    | "/v1/search"
                    | "/v1/batches"
                    | "/v1/changes"
                    | "/v1/admin/replication/export"
                    | "/v1/index/hnsw"
                    | "/v1/index/hnsw/quality"
                    | "/v1/index/hnsw/maintenance"
                    | "/v1/index/hnsw/maintenance/tick"
                    | "/v1/admin/maintenance/auto"
                    | "/v1/admin/maintenance/auto/tick"
                    | "/v1/admin/diagnostics/slow-queries"
            );
        if !candidate {
            continue;
        }
        let family = if literal.starts_with("/v1/collections")
            || literal.starts_with("/v1/vectors")
            || matches!(literal, "/v1/database" | "/v1/search" | "/v1/batches")
        {
            "collection"
        } else if literal.starts_with("/v1/graphs") {
            "graph"
        } else if literal == "/v1/changes" {
            "change-feed"
        } else if literal.starts_with("/v1/imports/") {
            "import-job"
        } else if literal == "/v1/admin/replication/export" {
            "replication-export"
        } else if literal.starts_with("/v1/admin/maintenance/auto") {
            "auto-maintenance"
        } else if literal.starts_with("/v1/index/hnsw") {
            "default-collection-index"
        } else if literal == "/v1/admin/diagnostics/slow-queries" {
            "observability"
        } else {
            panic!("resource route requires a reservation-guard family: {literal}");
        };
        classified.insert(family);
    }
    let graph = include_str!("../src/server/graph.rs");
    let graph_routes = graph
        .split('"')
        .filter(|literal| literal.starts_with("/v1/graphs"))
        .collect::<Vec<_>>();
    assert!(
        !graph_routes.is_empty(),
        "graph routes must remain inventoried"
    );
    classified.insert("graph");
    for family in [
        "collection",
        "graph",
        "change-feed",
        "import-job",
        "replication-export",
        "auto-maintenance",
        "default-collection-index",
    ] {
        assert!(classified.contains(family), "missing route family {family}");
    }
}
