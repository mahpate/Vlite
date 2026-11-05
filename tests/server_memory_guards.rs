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

async fn call_with_response(
    app: &Router,
    method: &str,
    path: &str,
    body: Body,
    token: &str,
) -> (u16, axum::http::HeaderMap, Vec<u8>) {
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
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, body.to_vec())
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

async fn assert_hidden_response(app: &Router, token: &str, method: &str, path: &str, body: &str) {
    let response = call_token(app, method, path, Body::from(body.to_owned()), token).await;
    if method == "POST" && matches!(path, "/v1/graphs" | "/v1/collections") {
        if response.0 != 403 {
            assert_eq!(response.0, 409, "{method} {path}: {response:?}");
            assert_eq!(
                response.1["error"]["code"],
                if path == "/v1/graphs" {
                    "graph_exists"
                } else {
                    "collection_exists"
                }
            );
        }
    } else if path.contains("notes_ev")
        || path.contains("notes_graph")
        || body.contains("notes_ev")
        || body.contains("notes_graph")
    {
        let absent = |text: &str| {
            text.replace("notes_ev", "absent_ev")
                .replace("notes_graph", "absent_graph")
        };
        let missing = call_token(app, method, &absent(path), Body::from(absent(body)), token).await;
        assert_eq!(response, missing, "{method} {path}");
    } else {
        assert_eq!(response.0, 403, "{method} {path}: {response:?}");
        assert!(matches!(
            response.1["error"]["code"].as_str(),
            Some("forbidden_scope" | "read_only")
        ));
    }
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

fn start_wildcard_scoped(
    directory: &TempDir,
    raw_access: bool,
    operations: &[&str],
) -> (Router, String) {
    const ID: &str = "abcdefghij27";
    let secret = "A".repeat(43);
    let db = database();
    let lineage = db.lineage().unwrap();
    let credentials = serde_json::json!({
        "schema": 1,
        "credentials": [{
            "id": ID,
            "name": "fake wildcard test credential",
            "all": true,
            "collections": [],
            "graphs": ["*"],
            "operations": operations,
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
    let bindings = BINDINGS.replace(
        "\"raw_access\":false",
        if raw_access {
            "\"raw_access\":true"
        } else {
            "\"raw_access\":false"
        },
    );
    std::fs::write(&path, bindings).unwrap();
    let app = configured_admin_router(
        Service::new(db),
        ServerOptions {
            loopback_only: true,
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
async fn node_deletion_checks_only_its_own_attachments_for_every_role() {
    const TENANT: &str = "0123456789abcdef0123456789abcdef";
    let operations = ["read", "write", "collections", "graph_read", "graph_write"];
    for tenanted in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let (app, raw) = if tenanted {
            start_tenant_scoped(&directory, true, &operations)
        } else {
            start_wildcard_scoped(&directory, true, &operations)
        };
        let (status, issued) = call(
            &app,
            "POST",
            "/v1/admin/credentials",
            Body::from(
                serde_json::json!({
                    "name": "fake deletion test key",
                    "collections": ["*"],
                    "graphs": ["*"],
                    "operations": operations,
                    "tenant_id": tenanted.then_some(TENANT),
                    "expires_unix_ms": u64::try_from(std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()).unwrap()
                        + 60_000
                })
                .to_string(),
            ),
        )
        .await;
        assert_eq!(status, 201, "{issued}");
        let no_raw = issued["token"].as_str().unwrap();
        let mut actors = vec![
            ("wildcard", no_raw.to_owned(), false, false, false),
            ("raw_access", raw.clone(), false, true, false),
        ];
        if !tenanted {
            let password = "fake-correct-horse-battery-staple";
            let (status, body) = call(
                &app,
                "POST",
                "/v1/admin/users",
                Body::from(
                    serde_json::json!({
                        "username": "node-editor", "password": password, "role": "editor",
                        "must_change_password": false
                    })
                    .to_string(),
                ),
            )
            .await;
            assert_eq!(status, 201, "{body}");
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/auth/login")
                        .header("content-type", "application/json")
                        .header("sec-fetch-site", "same-origin")
                        .header("x-vlite-request", "1")
                        .body(Body::from(
                            serde_json::json!({"username": "node-editor", "password": password})
                                .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            let cookie = response.headers()["set-cookie"]
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .to_owned();
            actors.extend([
                ("admin", ADMIN.to_owned(), false, false, false),
                ("session_editor", cookie, true, false, false),
                ("read_only", READ_ONLY.to_owned(), false, false, true),
            ]);
        }
        let evidence = if tenanted {
            format!("t{TENANT}__notes_ev")
        } else {
            "notes_ev".into()
        };
        let ordinary = if tenanted {
            format!("t{TENANT}__ordinary")
        } else {
            "ordinary".into()
        };
        for (index, (role, token, session, raw_access, read_only)) in actors.into_iter().enumerate()
        {
            let mut setup = Vec::new();
            for kind in ["plain", "ordinary", "reserved", "mixed"] {
                let key = format!("{role}-{kind}");
                setup.push(serde_json::json!({"op": "create_node", "key": key}));
                for (offset, collection) in [(0, &ordinary), (1, &evidence)] {
                    if kind == "plain"
                        || (kind == "ordinary" && offset == 1)
                        || (kind == "reserved" && offset == 0)
                    {
                        continue;
                    }
                    let id = (index * 10 + setup.len() * 2 + offset + 1).to_string();
                    let (status, body) = call_token(
                        &app,
                        "PUT",
                        &format!(
                            "/v1/collections/{}/vectors/{id}",
                            if offset == 0 { "ordinary" } else { "notes_ev" }
                        ),
                        Body::from(r#"{"vector":[0,1]}"#),
                        &raw,
                    )
                    .await;
                    assert_eq!(status, 204, "{body}");
                    setup.push(serde_json::json!({
                        "op": "attach", "node": key, "collection": collection, "record": id
                    }));
                }
            }
            let (status, body) = call_token(
                &app,
                "POST",
                "/v1/graphs/ordinary_graph/batch",
                Body::from(serde_json::json!({"ops": setup}).to_string()),
                &raw,
            )
            .await;
            assert_eq!(status, 200, "{body}");
            let mut denied = Vec::new();
            for kind in ["plain", "ordinary", "reserved", "mixed"] {
                let key = format!("{role}-{kind}");
                let request = Request::builder()
                    .method("POST")
                    .uri("/v1/graphs/ordinary_graph/batch")
                    .header("content-type", "application/json")
                    .header("sec-fetch-site", "same-origin")
                    .header("x-vlite-request", "1");
                let request = if session {
                    request.header("cookie", &token)
                } else {
                    request.header("authorization", format!("Bearer {token}"))
                };
                let response = app
                    .clone()
                    .oneshot(
                        request
                            .body(Body::from(
                                serde_json::json!({"ops": [{"op": "delete_node", "key": key}]})
                                    .to_string(),
                            ))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let status = response.status().as_u16();
                let mut headers = response.headers().clone();
                assert!(headers.remove("x-request-id").is_some());
                let bytes = response.into_body().collect().await.unwrap().to_bytes();
                let allowed = !read_only && (raw_access || matches!(kind, "plain" | "ordinary"));
                assert_eq!(
                    status,
                    if allowed { 200 } else { 403 },
                    "{role}/{kind}: {bytes:?}"
                );
                if !allowed {
                    let error: Value = serde_json::from_slice(&bytes).unwrap();
                    if !read_only {
                        assert_eq!(error["error"]["code"], "forbidden_scope");
                    }
                    denied.push((headers, bytes));
                }
                let (status, result) = call_token(
                    &app,
                    "POST",
                    "/v1/graphs/ordinary_graph/nodes/get",
                    Body::from(serde_json::json!({"node": {"key": key}}).to_string()),
                    &raw,
                )
                .await;
                assert_eq!(status, 200, "{result}");
                assert_eq!(result["node"].is_null(), allowed, "{role}/{kind}: {result}");
            }
            for pair in denied.windows(2) {
                assert_eq!(pair[0], pair[1], "{role}: denials differ by attachments");
            }
        }
    }
}

fn start_tenant_scoped(
    directory: &TempDir,
    raw_access: bool,
    operations: &[&str],
) -> (Router, String) {
    const TENANT: &str = "0123456789abcdef0123456789abcdef";
    const ID: &str = "abcdefghij27";
    let secret = "A".repeat(43);
    let mut db = Database::create_in_memory(
        2,
        CreateOptions {
            format: Some(FormatVersion::V5),
            ..Default::default()
        },
    )
    .unwrap();
    for name in ["notes_ev", "ordinary"] {
        db.create_collection(
            &format!("t{TENANT}__{name}"),
            CollectionSpec {
                dimensions: 2,
                settings: Default::default(),
            },
        )
        .unwrap();
    }
    db.create_graph(&format!("t{TENANT}__notes_graph")).unwrap();
    db.create_graph(&format!("t{TENANT}__ordinary_graph"))
        .unwrap();
    let lineage = db.lineage().unwrap();
    let credentials = serde_json::json!({
        "schema": 1,
        "credentials": [{
            "id": ID,
            "name": "fake tenant credential",
            "all": true,
            "collections": [],
            "graphs": ["*"],
            "operations": operations,
            "lineage": format!("{lineage:016x}"),
            "secret_sha256": format!("{:x}", Sha256::digest(secret.as_bytes())),
            "created_unix_ms": 0,
            "expires_unix_ms": 9_999_999_999_999u64,
            "revoked_unix_ms": null,
            "rotated_from": null,
            "tenant_id": TENANT
        }]
    });
    std::fs::write(
        directory.path().join("credentials.json"),
        serde_json::to_vec(&credentials).unwrap(),
    )
    .unwrap();
    std::fs::write(
        directory.path().join("tenants.json"),
        serde_json::to_vec(&serde_json::json!([{
            "id": TENANT,
            "name": "test tenant",
            "max_vectors": null
        }]))
        .unwrap(),
    )
    .unwrap();
    let mut bindings: Value = serde_json::from_str(BINDINGS).unwrap();
    let binding = &mut bindings["bindings"][0];
    binding["tenant"] = serde_json::json!(TENANT);
    binding["graph"] = serde_json::json!("notes_graph");
    binding["evidence_collection"] = serde_json::json!("notes_ev");
    binding["grants"][0]["raw_access"] = serde_json::json!(raw_access);
    binding["grants"][0]["principal"] = serde_json::json!(ID);
    let path = directory.path().join("memory-bindings.json");
    std::fs::write(&path, serde_json::to_vec(&bindings).unwrap()).unwrap();
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

fn start_tenant_scoped_empty_memory_graph(directory: &TempDir) -> (Router, String, String) {
    const TENANT: &str = "0123456789abcdef0123456789abcdef";
    const RAW_ID: &str = "abcdefghij27";
    const NO_RAW_ID: &str = "abcdefghij26";
    let raw_secret = "A".repeat(43);
    let no_raw_secret = "B".repeat(43);
    let mut db = Database::create_in_memory(
        2,
        CreateOptions {
            format: Some(FormatVersion::V5),
            ..Default::default()
        },
    )
    .unwrap();
    for name in ["notes_ev", "ordinary"] {
        db.create_collection(
            &format!("t{TENANT}__{name}"),
            CollectionSpec {
                dimensions: 2,
                settings: Default::default(),
            },
        )
        .unwrap();
    }
    db.create_graph(&format!("t{TENANT}__notes_graph")).unwrap();
    db.create_graph(&format!("t{TENANT}__ordinary_graph"))
        .unwrap();
    let lineage = db.lineage().unwrap();
    let credential = |id: &str, name: &str, secret: &str| {
        serde_json::json!({
            "id": id,
            "name": name,
            "all": true,
            "collections": [],
            "graphs": ["*"],
            "operations": ["read", "search", "collections", "graph_read", "graph_write"],
            "lineage": format!("{lineage:016x}"),
            "secret_sha256": format!("{:x}", Sha256::digest(secret.as_bytes())),
            "created_unix_ms": 0,
            "expires_unix_ms": 9_999_999_999_999u64,
            "revoked_unix_ms": null,
            "rotated_from": null,
            "tenant_id": TENANT
        })
    };
    let credentials = serde_json::json!({
        "schema": 1,
        "credentials": [
            credential(RAW_ID, "fake raw-access test credential", &raw_secret),
            credential(NO_RAW_ID, "fake no-raw-access test credential", &no_raw_secret)
        ]
    });
    std::fs::write(
        directory.path().join("credentials.json"),
        serde_json::to_vec(&credentials).unwrap(),
    )
    .unwrap();
    std::fs::write(
        directory.path().join("tenants.json"),
        serde_json::to_vec(&serde_json::json!([{
            "id": TENANT,
            "name": "test tenant",
            "max_vectors": null
        }]))
        .unwrap(),
    )
    .unwrap();
    let mut bindings: Value = serde_json::from_str(BINDINGS).unwrap();
    let binding = &mut bindings["bindings"][0];
    binding["tenant"] = serde_json::json!(TENANT);
    binding["graph"] = serde_json::json!("notes_graph");
    binding["evidence_collection"] = serde_json::json!("notes_ev");
    binding["grants"] = serde_json::json!([
        {"principal": RAW_ID, "scopes": [{"agent": "a"}], "raw_access": true},
        {"principal": NO_RAW_ID, "scopes": [{"agent": "b"}], "raw_access": false}
    ]);
    let path = directory.path().join("memory-bindings.json");
    std::fs::write(&path, serde_json::to_vec(&bindings).unwrap()).unwrap();
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
    (
        app,
        format!("vlk_{RAW_ID}_{raw_secret}"),
        format!("vlk_{NO_RAW_ID}_{no_raw_secret}"),
    )
}

#[tokio::test]
async fn reserved_graph_and_collection_are_denied_and_lists_filter_them() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(&directory, Some(BINDINGS), false).unwrap();

    let (status, error) = get(&app, "/v1/graphs/notes_graph").await;
    assert_eq!(status, 409);
    assert_eq!(error["error"]["code"], "graph_not_found");
    assert_eq!(get(&app, "/v1/graphs/ordinary_graph").await.0, 200);

    let (status, error) = get(&app, "/v1/collections/notes_ev").await;
    assert_eq!(status, 404);
    assert_eq!(error["error"]["code"], "collection_not_found");
    assert_eq!(get(&app, "/v1/collections/ordinary").await.0, 200);
    assert_eq!(
        call(&app, "DELETE", "/v1/graphs/notes_graph", Body::empty())
            .await
            .0,
        409
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
        404
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
        201
    );
    assert_eq!(
        call(
            &app,
            "POST",
            "/v1/admin/webhooks",
            Body::from(r#"{"url":"http://127.0.0.1:8080/hook","secret":"fake-secret-for-test","graphs":["notes_graph"]}"#)
        )
        .await
        .0,
        201
    );
    let (info_status, info) = get(&app, "/v1/database").await;
    assert_eq!(info_status, 200);
    assert_eq!(info["records"], 0);
    let default_bindings = BINDINGS.replace("\"notes_ev\"", "\"default\"");
    let default_directory = tempfile::tempdir().unwrap();
    let error = match start(&default_directory, Some(&default_bindings), false) {
        Ok(_) => panic!("default evidence collection must refuse startup"),
        Err(error) => error,
    };
    assert_eq!(error.to_string(), "memory bindings file is invalid");
    for (method, path, body) in [
        ("GET", "/v1/imports/1", Body::empty()),
        ("GET", "/v1/admin/replication/export", Body::empty()),
        ("GET", "/v1/admin/maintenance/auto", Body::empty()),
        ("GET", "/v1/admin/webhooks", Body::empty()),
        ("GET", "/v1/admin/webhooks/not-a-real-id", Body::empty()),
        ("GET", "/v1/admin/metrics/summary", Body::empty()),
        ("GET", "/metrics", Body::empty()),
        ("GET", "/v1/admin/graph/stats", Body::empty()),
        ("PUT", "/v1/admin/maintenance/auto", Body::from("{}")),
        ("POST", "/v1/admin/maintenance/auto/tick", Body::empty()),
    ] {
        let (status, error) = call(&app, method, path, body).await;
        assert_eq!(status, 403, "{method} {path}");
        assert_eq!(error["error"]["code"], "forbidden_scope", "{method} {path}");
    }
    let (missing_webhook_status, missing_webhook_error) = call(
        &app,
        "DELETE",
        "/v1/admin/webhooks/not-a-real-id",
        Body::empty(),
    )
    .await;
    assert_eq!(missing_webhook_status, 404);
    assert_eq!(missing_webhook_error["error"]["code"], "webhook_not_found");

    let (_, diagnostics) = get(&app, "/v1/admin/diagnostics/slow-queries").await;
    assert!(
        diagnostics["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| { item.to_string().find("notes_ev").is_none() })
    );

    let (_, first_page) = get(&app, "/v1/changes?limit=1").await;
    assert!(first_page["changes"].as_array().unwrap().is_empty());
    assert_eq!(first_page["has_more"], true);
    let first_cursor = first_page["next_cursor"].as_str().unwrap();
    let (_, second_page) = get(&app, &format!("/v1/changes?limit=1&cursor={first_cursor}")).await;
    assert_eq!(second_page["changes"][0]["collection"], "ordinary");
    let second_cursor = second_page["next_cursor"].as_str().unwrap();
    assert_ne!(first_cursor, second_cursor);
    let (_, remaining_pages) = get(
        &app,
        &format!("/v1/changes?limit=1000&cursor={second_cursor}"),
    )
    .await;
    assert!(
        remaining_pages["changes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|change| {
                change["collection"] != "notes_ev" && change["change"]["graph"] != "notes_graph"
            })
    );
    assert!(remaining_pages["next_cursor"].as_str().is_some());

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

#[test]
fn startup_refuses_bindings_with_a_missing_evidence_collection() {
    let missing_bindings = BINDINGS.replace("\"notes_ev\"", "\"missing_ev\"");
    let directory = tempfile::tempdir().unwrap();
    let error = start(&directory, Some(&missing_bindings), false)
        .expect_err("startup must refuse a binding with no evidence collection");
    assert!(
        error
            .to_string()
            .contains("a memory binding evidence collection does not exist"),
        "unexpected startup refusal: {error}"
    );
}

#[test]
fn startup_rejects_tenant_qualified_binding_resources_without_echoing_names() {
    const TENANT: &str = "0123456789abcdef0123456789abcdef";
    for tenant in ["@root", TENANT] {
        for field in ["graph", "evidence_collection"] {
            let directory = tempfile::tempdir().unwrap();
            let mut bindings: Value = serde_json::from_str(BINDINGS).unwrap();
            bindings["bindings"][0]["tenant"] = serde_json::json!(tenant);
            bindings["bindings"][0][field] = serde_json::json!(format!("t{TENANT}__private_spoof"));
            let error = start(&directory, Some(&bindings.to_string()), false)
                .expect_err("stored tenant names must not be accepted as binding-local names");
            assert_eq!(error.to_string(), "memory bindings file is invalid");
        }
    }
}

#[tokio::test]
async fn reserved_resource_routes_deny_before_resource_lookup_across_families() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(&directory, Some(BINDINGS), false).unwrap();
    let cases = [
        ("GET", "/v1/collections/notes_ev", ""),
        (
            "POST",
            "/v1/collections",
            r#"{"name":"notes_ev","dimensions":2}"#,
        ),
        ("PUT", "/v1/collections/notes_ev/settings", "{}"),
        ("DELETE", "/v1/collections/notes_ev", ""),
        ("GET", "/v1/collections/notes_ev/vectors", ""),
        ("GET", "/v1/collections/notes_ev/vectors/7", ""),
        (
            "GET",
            "/v1/collections/notes_ev/kv/entry?namespace=n&key=k",
            "",
        ),
        (
            "GET",
            "/v1/collections/notes_ev/kv/entries?namespace=n&limit=1",
            "",
        ),
        (
            "PUT",
            "/v1/collections/notes_ev/kv/entry",
            r#"{"namespace":"n","key":"k","value_base64":"AQ=="}"#,
        ),
        (
            "DELETE",
            "/v1/collections/notes_ev/kv/entry?namespace=n&key=k",
            "",
        ),
        (
            "DELETE",
            "/v1/collections/notes_ev/kv/entry?namespace=n&key=missing",
            "",
        ),
        (
            "PATCH",
            "/v1/collections/notes_ev/vectors/7/metadata",
            r#"{"metadata":{}}"#,
        ),
        ("PUT", "/v1/collections/notes_ev/vectors/7/named", "{}"),
        ("DELETE", "/v1/collections/notes_ev/vectors/7", ""),
        (
            "POST",
            "/v1/collections/notes_ev/search",
            r#"{"vector":[0,0],"k":1}"#,
        ),
        (
            "POST",
            "/v1/collections/notes_ev/search/grouped",
            r#"{"vector":[0,0],"field":"tag","groups":1,"hits_per_group":1}"#,
        ),
        (
            "POST",
            "/v1/collections/notes_ev/recommend",
            r#"{"positive_ids":[],"k":1}"#,
        ),
        (
            "POST",
            "/v1/collections/notes_ev/facets",
            r#"{"field":"tag"}"#,
        ),
        (
            "POST",
            "/v1/collections/notes_ev/stats/numeric",
            r#"{"field":"score"}"#,
        ),
        (
            "POST",
            "/v1/collections/notes_ev/search/geo",
            r#"{"vector":[0,0],"k":1,"field":"location","latitude":0,"longitude":0,"radius_meters":1}"#,
        ),
        (
            "POST",
            "/v1/collections/notes_ev/vectors/configure",
            r#"{"name":"v","dimensions":2,"metric":"cosine","active_version":"v1"}"#,
        ),
        (
            "POST",
            "/v1/collections/notes_ev/search/named",
            r#"{"name":"v","vector":[0,0],"k":1}"#,
        ),
        (
            "POST",
            "/v1/collections/notes_ev/search/sparse",
            r#"{"name":"v","vector":[],"k":1}"#,
        ),
        (
            "POST",
            "/v1/collections/notes_ev/search/dense-sparse",
            r#"{"dense_name":"v","dense":[0,0],"sparse_name":"s","sparse":[],"k":1}"#,
        ),
        ("POST", "/v1/collections/notes_ev/query", r#"{"query":"*"}"#),
        ("POST", "/v1/collections/notes_ev/embed", r#"{"text":"x"}"#),
        ("POST", "/v1/collections/notes_ev/index/disk-ann", "{}"),
        (
            "POST",
            "/v1/collections/notes_ev/search/disk-ann",
            r#"{"vector":[0,0],"k":1,"ef_search":1}"#,
        ),
        ("GET", "/v1/collections/notes_ev/index/hnsw", ""),
        ("GET", "/v1/collections/notes_ev/index/hnsw/maintenance", ""),
        ("GET", "/v1/collections/notes_ev/index/hnsw/rebuild", ""),
        (
            "POST",
            "/v1/collections/notes_ev/index/hnsw/maintenance/tick",
            "",
        ),
        ("POST", "/v1/collections/notes_ev/index/hnsw", "{}"),
        ("POST", "/v1/collections/notes_ev/index/hnsw/quality", "{}"),
        ("POST", "/v1/collections/notes_ev/index/hnsw/rebuild", "{}"),
        (
            "POST",
            "/v1/collections/notes_ev/index/hnsw/rebuild/cancel",
            "{}",
        ),
        (
            "POST",
            "/v1/collections/notes_ev/index/hnsw/rebuild/poll",
            "{}",
        ),
        (
            "POST",
            "/v1/collections/notes_ev/batches",
            r#"{"operations":[]}"#,
        ),
        ("POST", "/v1/collections/notes_ev/imports", ""),
        ("POST", "/v1/collections/ordinary/imports", ""),
        ("GET", "/v1/imports/not-a-job", ""),
        ("PUT", "/v1/imports/1/chunks/0", "{}"),
        ("POST", "/v1/imports/not-a-job/finish", ""),
        ("POST", "/v1/imports/not-a-job/abort", ""),
        ("POST", "/v1/graphs", r#"{"graph":"notes_graph"}"#),
        ("GET", "/v1/graphs/notes_graph", ""),
        ("GET", "/v1/graphs/notes_graph/schema", ""),
        ("POST", "/v1/graphs/notes_graph/batch", "{}"),
        ("POST", "/v1/graphs/notes_graph/nodes/get", "{}"),
        ("POST", "/v1/graphs/notes_graph/edges/get", "{}"),
        ("POST", "/v1/graphs/notes_graph/nodes/find", "{}"),
        ("POST", "/v1/graphs/notes_graph/neighbors", "{}"),
        ("POST", "/v1/graphs/notes_graph/traverse", "{}"),
        ("POST", "/v1/graphs/notes_graph/shortest-path", "{}"),
        ("POST", "/v1/graphs/notes_graph/explain", "{}"),
        ("POST", "/v1/graphs/notes_graph/retrieve", "{}"),
        ("POST", "/v1/graphs/notes_graph/query", "{}"),
        ("PUT", "/v1/graphs/notes_graph/schema", "{}"),
        ("DELETE", "/v1/graphs/notes_graph", ""),
    ];

    for (method, path, body) in cases {
        assert_hidden_response(&app, ADMIN, method, path, body).await;
    }

    for token in [READ_ONLY] {
        for (method, path, body) in cases {
            assert_hidden_response(&app, token, method, path, body).await;
        }
    }

    let all_operations = [
        "read",
        "search",
        "search_build",
        "write",
        "import",
        "index",
        "collections",
        "diagnostics",
        "backup",
        "graph_read",
        "graph_write",
    ];
    for operations in [
        &["read", "search", "diagnostics", "graph_read"][..],
        &[
            "read",
            "search",
            "search_build",
            "write",
            "import",
            "index",
            "collections",
            "diagnostics",
            "backup",
            "graph_read",
            "graph_write",
        ][..],
    ] {
        let scoped_directory = tempfile::tempdir().unwrap();
        let (scoped, scoped_token) = start_wildcard_scoped(&scoped_directory, false, operations);
        for (method, path, body) in cases {
            assert_hidden_response(&scoped, &scoped_token, method, path, body).await;
        }
    }

    let ordinary_directory = tempfile::tempdir().unwrap();
    let (ordinary_app, ordinary_token) =
        start_wildcard_scoped(&ordinary_directory, false, &all_operations);
    for (method, path, body) in cases {
        if path.contains("/imports") {
            continue;
        }
        let path = path
            .replace("notes_ev", "ordinary")
            .replace("notes_graph", "ordinary_graph");
        let body = body
            .replace("notes_ev", "ordinary")
            .replace("notes_graph", "ordinary_graph");
        let (status, error) = call_token(
            &ordinary_app,
            method,
            &path,
            Body::from(body),
            &ordinary_token,
        )
        .await;
        assert!(
            status != 403 || error["error"]["code"] != "forbidden_scope",
            "{method} {path}: {error}"
        );
    }

    let raw_directory = tempfile::tempdir().unwrap();
    let (raw_app, raw_token) = start_wildcard_scoped(&raw_directory, true, &all_operations);
    for (method, path, body) in cases {
        if path.contains("/imports") {
            continue;
        }
        if !path.contains("notes_ev")
            && !path.contains("notes_graph")
            && !body.contains("notes_ev")
            && !body.contains("notes_graph")
        {
            continue;
        }
        let (status, error) =
            call_token(&raw_app, method, path, Body::from(body), &raw_token).await;
        assert!(
            status != 403 || error["error"]["code"] != "forbidden_scope",
            "{method} {path}: {error}"
        );
    }
    for (method, path, body) in [
        ("POST", "/v1/collections/notes_ev/imports", ""),
        ("POST", "/v1/collections/ordinary/imports", ""),
        ("GET", "/v1/imports/not-a-job", ""),
        ("PUT", "/v1/imports/1/chunks/0", "{}"),
        ("POST", "/v1/imports/not-a-job/finish", ""),
        ("POST", "/v1/imports/not-a-job/abort", ""),
    ] {
        let (status, error) =
            call_token(&raw_app, method, path, Body::from(body), &raw_token).await;
        assert_eq!(status, 403, "{method} {path}: {error}");
        assert_eq!(error["error"]["code"], "forbidden_scope", "{method} {path}");
    }

    for raw_access in [false, true] {
        let tenant_directory = tempfile::tempdir().unwrap();
        let (tenant_app, tenant_token) =
            start_tenant_scoped(&tenant_directory, raw_access, &all_operations);
        for (method, path, body) in cases {
            if path.contains("/imports") {
                continue;
            }
            let reserved = path.contains("notes_ev")
                || path.contains("notes_graph")
                || body.contains("notes_ev")
                || body.contains("notes_graph");
            if !reserved {
                continue;
            }
            let (status, error) =
                call_token(&tenant_app, method, path, Body::from(body), &tenant_token).await;
            if raw_access {
                assert!(
                    status != 403 || error["error"]["code"] != "forbidden_scope",
                    "tenant {method} {path}: {error}"
                );
            } else {
                assert_hidden_response(&tenant_app, &tenant_token, method, path, body).await;
            }
        }

        let ordinary_directory = tempfile::tempdir().unwrap();
        let (ordinary_app, ordinary_token) =
            start_tenant_scoped(&ordinary_directory, raw_access, &all_operations);
        for (method, path, body) in cases {
            if path.contains("/imports") {
                continue;
            }
            let path = path
                .replace("notes_ev", "ordinary")
                .replace("notes_graph", "ordinary_graph");
            let body = body
                .replace("notes_ev", "ordinary")
                .replace("notes_graph", "ordinary_graph");
            let (status, error) = call_token(
                &ordinary_app,
                method,
                &path,
                Body::from(body),
                &ordinary_token,
            )
            .await;
            assert!(
                status != 403 || error["error"]["code"] != "forbidden_scope",
                "tenant {method} {path}: {error}"
            );
        }
    }

    for (method, path, body) in [
        (
            "POST",
            "/v1/graphs/ordinary_graph/batch",
            r#"{"ops":[{"op":"attach","node":"node","collection":"notes_ev","record":"1"}]}"#,
        ),
        (
            "POST",
            "/v1/graphs/ordinary_graph/retrieve",
            r#"{"pivot":{"collection":"notes_ev","vector":[0,0],"k":1}}"#,
        ),
    ] {
        assert_hidden_response(&app, ADMIN, method, path, body).await;
    }

    for (method, path, body) in [
        ("PUT", "/v1/collections/notes_ev/settings", "{}"),
        ("DELETE", "/v1/collections/notes_ev", ""),
        (
            "POST",
            "/v1/collections",
            r#"{"name":"notes_ev","dimensions":2}"#,
        ),
    ] {
        assert_hidden_response(&app, ADMIN, method, path, body).await;
    }

    let legacy_bindings = BINDINGS.replace("\"notes_ev\"", "\"default\"");
    let legacy_directory = tempfile::tempdir().unwrap();
    assert!(start(&legacy_directory, Some(&legacy_bindings), false).is_err());
    let legacy_app = start(&legacy_directory, Some(BINDINGS), false).unwrap();
    for (method, path, body) in [
        ("GET", "/v1/vectors/7", ""),
        ("PUT", "/v1/vectors/7", r#"{"vector":[0,0]}"#),
        ("DELETE", "/v1/vectors/7", ""),
        ("PATCH", "/v1/vectors/7/metadata", r#"{"metadata":{}}"#),
        ("GET", "/v1/vectors", ""),
        ("POST", "/v1/search", r#"{"vector":[0,0],"k":1}"#),
        ("POST", "/v1/batches", r#"{"operations":[]}"#),
        ("GET", "/v1/index/hnsw", ""),
        ("POST", "/v1/index/hnsw", "{}"),
        ("POST", "/v1/index/hnsw/quality", "{}"),
        ("GET", "/v1/index/hnsw/maintenance", ""),
        ("PUT", "/v1/index/hnsw/maintenance", "{}"),
        ("POST", "/v1/index/hnsw/maintenance/tick", ""),
        ("GET", "/v1/index/hnsw/rebuild", ""),
        ("POST", "/v1/index/hnsw/rebuild", "{}"),
        ("POST", "/v1/index/hnsw/rebuild/cancel", "{}"),
        ("POST", "/v1/index/hnsw/rebuild/poll", ""),
    ] {
        let (status, error) = call(&legacy_app, method, path, Body::from(body)).await;
        assert_ne!(
            error["error"]["code"], "collection_not_found",
            "{method} {path}: {status}"
        );
    }
}

#[tokio::test]
async fn tenant_qualified_reservations_follow_scoped_credentials_and_raw_access() {
    const TENANT: &str = "0123456789abcdef0123456789abcdef";
    let build = start_tenant_scoped;

    let no_raw_directory = tempfile::tempdir().unwrap();
    let (no_raw, token) = build(
        &no_raw_directory,
        false,
        &["read", "write", "collections", "graph_read", "graph_write"],
    );
    for path in [
        format!("/v1/collections/t{TENANT}__notes_ev"),
        format!("/v1/graphs/t{TENANT}__notes_graph"),
    ] {
        for token in [ADMIN, READ_ONLY] {
            let (status, error) = call_token(&no_raw, "GET", &path, Body::empty(), token).await;
            assert_eq!(
                status,
                if path.contains("/graphs/") { 409 } else { 404 },
                "{path}: {error}"
            );
            assert_eq!(
                error["error"]["code"],
                if path.contains("/graphs/") {
                    "graph_not_found"
                } else {
                    "collection_not_found"
                },
                "{path}"
            );
        }
    }
    let (tenant_list_status, tenant_list) = get(&no_raw, "/v1/admin/tenants").await;
    assert_eq!(tenant_list_status, 200);
    assert!(
        tenant_list["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["id"] != TENANT || item["collections"] == 1)
    );
    let (delete_tenant_status, delete_tenant_error) = call_token(
        &no_raw,
        "DELETE",
        &format!("/v1/admin/tenants/{TENANT}"),
        Body::empty(),
        ADMIN,
    )
    .await;
    assert_eq!(delete_tenant_status, 403);
    assert_eq!(delete_tenant_error["error"]["code"], "forbidden_scope");
    let (status, error) = call_token(
        &no_raw,
        "GET",
        "/v1/collections/notes_ev",
        Body::empty(),
        &token,
    )
    .await;
    assert_eq!(status, 404);
    assert_eq!(error["error"]["code"], "collection_not_found");
    let (status, error) = call_token(
        &no_raw,
        "GET",
        "/v1/graphs/notes_graph",
        Body::empty(),
        &token,
    )
    .await;
    assert_eq!(status, 409);
    assert_eq!(error["error"]["code"], "graph_not_found");
    assert_eq!(
        call_token(
            &no_raw,
            "GET",
            "/v1/graphs/ordinary_graph",
            Body::empty(),
            &token,
        )
        .await
        .0,
        200
    );
    assert_eq!(
        call_token(
            &no_raw,
            "DELETE",
            "/v1/graphs/notes_graph",
            Body::empty(),
            &token,
        )
        .await
        .0,
        409
    );
    assert_eq!(
        call_token(
            &no_raw,
            "GET",
            "/v1/collections/ordinary",
            Body::empty(),
            &token,
        )
        .await
        .0,
        200
    );
    let (_, list) = call_token(&no_raw, "GET", "/v1/collections", Body::empty(), &token).await;
    let listed = list["items"].as_array().unwrap();
    assert!(listed.iter().any(|item| item["name"] == "ordinary"));
    assert!(listed.iter().all(|item| item["name"] != "notes_ev"));
    let (_, feed) = call_token(
        &no_raw,
        "GET",
        "/v1/changes?limit=100",
        Body::empty(),
        ADMIN,
    )
    .await;
    assert!(
        feed["changes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|change| { change["collection"] != format!("t{TENANT}__notes_ev") })
    );
    let (_, usage) = call_token(
        &no_raw,
        "GET",
        &format!("/v1/admin/tenants/{TENANT}/usage"),
        Body::empty(),
        ADMIN,
    )
    .await;
    assert!(usage["collections"].as_array().unwrap().iter().all(|item| {
        item["name"] != "notes_ev" && item["physical_name"] != format!("t{TENANT}__notes_ev")
    }));
    assert!(
        usage["collections"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["name"] == "ordinary")
    );

    let full_tenant_operations = [
        "read",
        "search",
        "search_build",
        "write",
        "import",
        "index",
        "collections",
        "diagnostics",
        "backup",
        "graph_read",
        "graph_write",
    ];
    for (raw_access, reserved_status) in [(false, 404), (true, 200)] {
        let directory = tempfile::tempdir().unwrap();
        let (app, token) = build(&directory, raw_access, &full_tenant_operations);
        for (method, path, body, expected) in [
            ("GET", "/v1/collections/notes_ev", "", reserved_status),
            ("GET", "/v1/collections/ordinary", "", 200),
            (
                "PUT",
                "/v1/collections/notes_ev/vectors/7",
                r#"{"vector":[0.1,0.2],"payload_base64":"","metadata":{}}"#,
                if raw_access { 204 } else { 404 },
            ),
            (
                "PUT",
                "/v1/collections/ordinary/vectors/7",
                r#"{"vector":[0.1,0.2],"payload_base64":"","metadata":{}}"#,
                204,
            ),
            (
                "GET",
                "/v1/graphs/notes_graph",
                "",
                if raw_access { 200 } else { 409 },
            ),
            ("GET", "/v1/graphs/ordinary_graph", "", 200),
            ("DELETE", "/v1/graphs/notes_graph", "", 409),
            ("DELETE", "/v1/graphs/ordinary_graph", "", 200),
        ] {
            let (status, error) = call_token(&app, method, path, Body::from(body), &token).await;
            assert_eq!(status, expected, "{method} {path}: {error}");
            if expected == 403 {
                assert_eq!(error["error"]["code"], "forbidden_scope", "{method} {path}");
                assert_eq!(
                    error["error"]["message"],
                    "credential scope does not permit this operation or collection",
                    "{method} {path}"
                );
            } else if expected == 409 {
                assert_eq!(
                    error["error"]["code"],
                    if raw_access {
                        "graph_not_empty"
                    } else {
                        "graph_not_found"
                    },
                    "{method} {path}"
                );
            }
        }
    }

    let raw_read_directory = tempfile::tempdir().unwrap();
    let (raw_read, token) = build(
        &raw_read_directory,
        true,
        &["read", "collections", "graph_read"],
    );
    assert_eq!(
        call_token(
            &raw_read,
            "GET",
            "/v1/collections/notes_ev",
            Body::empty(),
            &token,
        )
        .await
        .0,
        200
    );
    let (_, raw_collections) =
        call_token(&raw_read, "GET", "/v1/collections", Body::empty(), &token).await;
    assert!(
        raw_collections["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["name"] == "notes_ev")
    );
    assert_eq!(
        call_token(
            &raw_read,
            "GET",
            "/v1/graphs/notes_graph",
            Body::empty(),
            &token,
        )
        .await
        .0,
        200
    );
    let (_, raw_graphs) = call_token(&raw_read, "GET", "/v1/graphs", Body::empty(), &token).await;
    assert!(
        raw_graphs["graphs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|graph| graph["name"] == "notes_graph")
    );
    assert_eq!(
        call_token(
            &raw_read,
            "DELETE",
            "/v1/graphs/notes_graph",
            Body::empty(),
            &token,
        )
        .await
        .0,
        403
    );
    assert_eq!(
        call_token(
            &raw_read,
            "PUT",
            "/v1/collections/notes_ev/vectors/7",
            Body::from(r#"{"vector":[0.1,0.2],"payload_base64":"","metadata":{}}"#),
            &token,
        )
        .await
        .0,
        403
    );

    let raw_write_directory = tempfile::tempdir().unwrap();
    let (raw_write, token) = build(
        &raw_write_directory,
        true,
        &["read", "write", "graph_read", "graph_write"],
    );
    assert_eq!(
        call_token(
            &raw_write,
            "PUT",
            "/v1/collections/notes_ev/vectors/7",
            Body::from(r#"{"vector":[0.1,0.2],"payload_base64":"","metadata":{}}"#),
            &token,
        )
        .await
        .0,
        204
    );
    let (status, error) = call_token(
        &raw_write,
        "DELETE",
        "/v1/graphs/notes_graph",
        Body::empty(),
        &token,
    )
    .await;
    assert_eq!(status, 409);
    assert_eq!(error["error"]["code"], "graph_not_empty");
    assert_eq!(
        call_token(
            &raw_write,
            "DELETE",
            "/v1/graphs/ordinary_graph",
            Body::empty(),
            &token,
        )
        .await
        .0,
        200
    );
}

#[tokio::test]
async fn deleted_reserved_resources_remain_indistinguishable_from_existing_resources() {
    let directory = tempfile::tempdir().unwrap();
    let (app, raw_access, no_raw_access) = start_tenant_scoped_empty_memory_graph(&directory);
    for agent in ["a", "b"] {
        let mut scope_key = b"scope:v1".to_vec();
        for value in [
            Some("0123456789abcdef0123456789abcdef"),
            Some(agent),
            None,
            None,
        ] {
            match value {
                None => scope_key.push(0),
                Some(value) => {
                    scope_key.push(1);
                    scope_key.extend_from_slice(&(value.len() as u32).to_be_bytes());
                    scope_key.extend_from_slice(value.as_bytes());
                }
            }
        }
        let scope_key = format!("scope:v1:{:x}", Sha256::digest(scope_key));
        assert_eq!(
            call_token(
                &app,
                "POST",
                "/v1/graphs/notes_graph/batch",
                Body::from(format!(
                    r#"{{"ops":[{{"op":"delete_node","key":"{scope_key}"}}]}}"#
                )),
                &raw_access,
            )
            .await
            .0,
            200,
            "remove each provisioned scope node to make the reserved graph empty"
        );
    }

    let actors = [(no_raw_access.as_str(), false), (ADMIN, true)];
    let mut before = Vec::new();
    for (token, admin) in actors {
        let collection = if admin {
            "t0123456789abcdef0123456789abcdef__notes_ev"
        } else {
            "notes_ev"
        };
        let graph = if admin {
            "t0123456789abcdef0123456789abcdef__notes_graph"
        } else {
            "notes_graph"
        };
        let routes = [
            ("GET", format!("/v1/collections/{collection}"), ""),
            (
                "POST",
                format!("/v1/collections/{collection}/search"),
                r#"{"vector":[0,0],"k":1}"#,
            ),
            ("DELETE", format!("/v1/collections/{collection}"), ""),
            ("GET", format!("/v1/graphs/{graph}"), ""),
            ("DELETE", format!("/v1/graphs/{graph}"), ""),
        ];
        let mut responses = Vec::new();
        for (method, path, body) in &routes {
            responses.push(call_with_response(&app, method, path, Body::from(*body), token).await);
        }
        for ((_, path, _), (status, _, body)) in routes.iter().zip(&responses) {
            assert_eq!(*status, if path.contains("/graphs/") { 409 } else { 404 });
            let error: Value = serde_json::from_slice(body).unwrap();
            assert_eq!(
                error["error"]["code"],
                if path.contains("/graphs/") {
                    "graph_not_found"
                } else {
                    "collection_not_found"
                }
            );
        }
        before.push((routes, responses));
    }

    assert_eq!(
        call_token(
            &app,
            "DELETE",
            "/v1/graphs/notes_graph",
            Body::empty(),
            &raw_access,
        )
        .await
        .0,
        200,
        "raw_access must permit dropping an empty reserved graph"
    );
    assert_eq!(
        call_token(
            &app,
            "DELETE",
            "/v1/collections/notes_ev",
            Body::empty(),
            &raw_access,
        )
        .await
        .0,
        200,
        "raw_access must permit dropping an empty reserved collection"
    );

    for ((token, _), (routes, responses)) in actors.into_iter().zip(before) {
        for ((method, path, body), (expected_status, expected_headers, expected_body)) in
            routes.iter().zip(responses)
        {
            let (actual_status, mut actual_headers, actual_body) =
                call_with_response(&app, method, path, Body::from(*body), token).await;
            let mut expected_headers = expected_headers;
            assert!(actual_headers.contains_key("x-request-id"));
            assert!(expected_headers.contains_key("x-request-id"));
            // The middleware assigns a unique request ID; all other headers must match.
            actual_headers.remove("x-request-id");
            expected_headers.remove("x-request-id");
            assert_eq!(
                (actual_status, actual_headers, actual_body),
                (expected_status, expected_headers, expected_body),
                "{method} {path} after the reserved resources were deleted"
            );
        }
        for (path, body, code) in [
            (
                "/v1/collections",
                r#"{"name":"notes_ev","dimensions":2}"#,
                "collection_exists",
            ),
            ("/v1/graphs", r#"{"graph":"notes_graph"}"#, "graph_exists"),
        ] {
            let body = if token == ADMIN {
                body.replace("notes_ev", "t0123456789abcdef0123456789abcdef__notes_ev")
                    .replace(
                        "notes_graph",
                        "t0123456789abcdef0123456789abcdef__notes_graph",
                    )
            } else {
                body.to_owned()
            };
            let (status, error) = call_token(&app, "POST", path, Body::from(body), token).await;
            assert_eq!(
                status, 409,
                "a reservation continues to occupy its deleted resource name"
            );
            assert_eq!(error["error"]["code"], code);
        }
    }
}

#[tokio::test]
async fn webhook_dispatcher_never_delivers_reserved_resource_events() {
    use std::{io::ErrorKind, net::TcpListener, time::Duration};

    let directory = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}/hook", listener.local_addr().unwrap());
    let webhook_store = serde_json::json!({
        "subscriptions": {
            "fake-subscription-id": {
                "id": "fake-subscription-id",
                "url": endpoint,
                "collections": ["notes_ev"],
                "events": ["collection_created"],
                "graphs": null,
                "secret": "fake-webhook-secret"
            },
            "fake-graph-subscription-id": {
                "id": "fake-graph-subscription-id",
                "url": endpoint,
                "collections": null,
                "events": ["graph_created"],
                "graphs": ["notes_graph"],
                "secret": "fake-graph-webhook-secret"
            }
        },
        "deliveries": {}
    });
    std::fs::write(
        directory.path().join("webhooks.json"),
        serde_json::to_vec(&webhook_store).unwrap(),
    )
    .unwrap();
    std::fs::write(
        directory.path().join("webhooks.cursor"),
        vlite::changes::FeedCursor::beginning().to_token(),
    )
    .unwrap();
    let bindings_path = directory.path().join("memory-bindings.json");
    std::fs::write(&bindings_path, BINDINGS).unwrap();
    let service = Service::new(database());
    let _app = configured_admin_router(
        service.clone(),
        ServerOptions {
            admin_token: Some(BearerToken::new(ADMIN).unwrap()),
            admin_directory: Some(directory.path().into()),
            memory_bindings_file: Some(bindings_path),
            ..Default::default()
        },
    )
    .unwrap();

    let until = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < until {
        match listener.accept() {
            Ok((_, _)) => panic!("reserved resource event was delivered"),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(error) => panic!("webhook listener failed: {error}"),
        }
    }
    service.close().await.unwrap();
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
    );
    let error = match error {
        Ok(_) => panic!("scheduled maintenance must refuse reserved resources"),
        Err(error) => error,
    };
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
    assert_eq!(admin_status, 404);

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
    assert_eq!(status, 404);
    assert_eq!(error, admin_error);

    let directory = tempfile::tempdir().unwrap();
    let (app, token) = start_scoped(&directory, true, "ordinary");
    let (reserved_status, reserved_error) = call_token(
        &app,
        "GET",
        "/v1/collections/notes_ev",
        Body::empty(),
        &token,
    )
    .await;
    let (missing_status, missing_error) = call_token(
        &app,
        "GET",
        "/v1/collections/not-a-collection",
        Body::empty(),
        &token,
    )
    .await;
    assert_eq!(reserved_status, 403);
    assert_eq!(missing_status, 403);
    assert_eq!(reserved_error, missing_error);
    assert_eq!(
        call_token(
            &app,
            "GET",
            "/v1/collections/ordinary",
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
            "/v1/collections/ordinary/vectors/7",
            Body::from(r#"{"vector":[0.1,0.2],"payload_base64":"","metadata":{}}"#),
            &token,
        )
        .await
        .0,
        204
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
        404
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
    assert_eq!(
        call_token(
            &app,
            "PUT",
            "/v1/collections/ordinary/vectors/7",
            Body::from(r#"{"vector":[0.1,0.2],"payload_base64":"","metadata":{}}"#),
            READ_ONLY,
        )
        .await
        .0,
        403
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
