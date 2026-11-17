#![cfg(feature = "server")]
//! EPIC-002 (FR-006, NFR-004, TEST-001): standalone/writer/reader serve modes.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use vlite::{
    CreateOptions, Database, FormatVersion,
    server::{BearerToken, ServeMode, ServerOptions, Service, configured_admin_router, router},
};

const ADMIN: &str = "serve_mode_admin_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const READ_ONLY: &str = "serve_mode_readonly_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";

async fn call(
    app: &Router,
    token: Option<&str>,
    method: &str,
    uri: &str,
    body: &str,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A file database with record 1, reopened read-only for a reader.
fn file_with_record(directory: &tempfile::TempDir) -> std::path::PathBuf {
    let path = directory.path().join("reader.vlite");
    let mut db = Database::create_with_format(&path, 2, FormatVersion::V5).unwrap();
    db.put(1, &[1.0, 0.0], b"").unwrap();
    db.close().unwrap();
    path
}

fn configured(service: Service) -> Router {
    configured_admin_router(
        service,
        ServerOptions {
            loopback_only: true,
            admin_token: Some(BearerToken::new(ADMIN).unwrap()),
            read_only_token: Some(BearerToken::new(READ_ONLY).unwrap()),
            ..Default::default()
        },
    )
    .unwrap()
}

#[test]
fn serve_modes_parse_and_default_to_standalone() {
    assert_eq!(ServeMode::default(), ServeMode::Standalone);
    for (text, mode) in [
        ("standalone", ServeMode::Standalone),
        ("writer", ServeMode::Writer),
        ("reader", ServeMode::Reader),
    ] {
        assert_eq!(text.parse::<ServeMode>().unwrap(), mode);
        assert_eq!(mode.as_str(), text);
    }
    assert!("Reader".parse::<ServeMode>().is_err());
    assert!("".parse::<ServeMode>().is_err());
}

#[test]
fn reader_needs_a_read_only_database_and_writer_a_writable_one() {
    let directory = tempfile::tempdir().unwrap();
    let path = file_with_record(&directory);
    let writable = Database::open(&path).unwrap();
    let error = Service::new(writable)
        .with_serve_mode(ServeMode::Reader)
        .err()
        .expect("reader on a writable database");
    assert!(error.to_string().contains("read-only"), "{error}");
    let read_only = Database::open_read_only(&path).unwrap();
    let error = Service::new(read_only)
        .with_serve_mode(ServeMode::Writer)
        .err()
        .expect("writer on a read-only database");
    assert!(error.to_string().contains("writable"), "{error}");
}

#[tokio::test]
async fn default_service_reports_standalone_and_is_otherwise_unchanged() {
    let db = Database::create_in_memory(2, CreateOptions::default()).unwrap();
    let service = Service::new(db);
    let app = router(service.clone());
    let (status, body) = call(&app, None, "GET", "/v1/admin/serve-status", "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["mode"], "standalone");
    assert_eq!(body["draining"], false);
    assert!(body["epoch"].is_null());
    assert!(body["sequence"].is_u64(), "{body}");
    assert!(body["lineage"].is_string(), "{body}");
    assert!(body["archive"].is_null(), "{body}");
    assert_eq!(
        call(&app, None, "PUT", "/v1/vectors/1", r#"{"vector":[1,0]}"#)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    // Standalone never drains readiness: shutdown keeps its existing behavior.
    service.begin_drain();
    let (status, body) = call(&app, None, "GET", "/ready", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ready");
}

#[tokio::test]
async fn writer_serves_mutations_and_reports_its_mode() {
    let db = Database::create_in_memory(2, CreateOptions::default()).unwrap();
    let service = Service::new(db).with_serve_mode(ServeMode::Writer).unwrap();
    let app = configured(service.clone());
    assert_eq!(
        call(
            &app,
            Some(ADMIN),
            "PUT",
            "/v1/vectors/1",
            r#"{"vector":[1,0]}"#
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let (status, body) = call(&app, Some(ADMIN), "GET", "/v1/admin/serve-status", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["mode"], "writer");
    service.begin_drain();
    let (status, body) = call(&app, Some(ADMIN), "GET", "/ready", "").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "draining");
    let (_, body) = call(&app, Some(ADMIN), "GET", "/v1/admin/serve-status", "").await;
    assert_eq!(body["draining"], true);
}

#[tokio::test]
async fn serve_status_refuses_scoped_and_tenant_keys() {
    const TENANT: &str = "0123456789abcdef0123456789abcdef";
    let directory = tempfile::tempdir().unwrap();
    let db = Database::create_in_memory(2, CreateOptions::default()).unwrap();
    let lineage = format!("{:016x}", db.lineage().unwrap());
    let secret = "B".repeat(43);
    let credentials: Vec<_> = [
        ("global_all", "abcdefghij45", false),
        ("tenant_all", "abcdefghij46", true),
    ]
    .into_iter()
    .map(|(name, id, tenant)| {
        json!({"id":id,"name":name,"all":true,"collections":[],"graphs":["*"],
                "operations":["read","write","search","diagnostics","backup","graph_read"],
                "lineage":lineage,
                "secret_sha256":format!("{:x}",Sha256::digest(secret.as_bytes())),
                "created_unix_ms":0,"expires_unix_ms":9_999_999_999_999u64,
                "revoked_unix_ms":null,"rotated_from":null,
                "tenant_id":tenant.then_some(TENANT)})
    })
    .collect();
    std::fs::write(
        directory.path().join("credentials.json"),
        json!({"schema":1,"credentials":credentials}).to_string(),
    )
    .unwrap();
    std::fs::write(
        directory.path().join("tenants.json"),
        json!([{"id":TENANT,"name":"serve tenant","max_vectors":null}]).to_string(),
    )
    .unwrap();
    let app = configured_admin_router(
        Service::new(db).with_serve_mode(ServeMode::Writer).unwrap(),
        ServerOptions {
            loopback_only: true,
            admin_token: Some(BearerToken::new(ADMIN).unwrap()),
            admin_directory: Some(directory.path().into()),
            ..Default::default()
        },
    )
    .unwrap();
    for id in ["abcdefghij45", "abcdefghij46"] {
        let key = format!("vlk_{id}_{secret}");
        let (status, body) = call(&app, Some(&key), "GET", "/v1/admin/serve-status", "").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{id}: {body}");
        assert_eq!(body["error"]["code"], "forbidden_scope", "{id}: {body}");
    }
    let (status, body) = call(&app, Some(ADMIN), "GET", "/v1/admin/serve-status", "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["mode"], "writer");
}

#[tokio::test]
async fn reader_refuses_every_mutation_and_writer_only_admin_route() {
    let directory = tempfile::tempdir().unwrap();
    let path = file_with_record(&directory);
    let before = std::fs::read(&path).unwrap();
    let service = Service::new(Database::open_read_only(&path).unwrap())
        .with_serve_mode(ServeMode::Reader)
        .unwrap();
    let app = configured(service.clone());

    // Reads and read-only POSTs are served.
    assert_eq!(
        call(&app, Some(ADMIN), "GET", "/v1/vectors/1", "").await.0,
        StatusCode::OK
    );
    let (status, body) = call(
        &app,
        Some(READ_ONLY),
        "POST",
        "/v1/search",
        r#"{"vector":[1,0],"k":1}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["hits"][0]["id"], "1");
    assert_eq!(
        call(&app, Some(ADMIN), "GET", "/ready", "").await.0,
        StatusCode::OK
    );

    // Authentication still comes first.
    assert_eq!(
        call(&app, None, "PUT", "/v1/vectors/2", r#"{"vector":[0,1]}"#)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );

    for (method, uri, body) in [
        ("PUT", "/v1/vectors/2", r#"{"vector":[0,1]}"#),
        ("DELETE", "/v1/vectors/1", ""),
        ("PATCH", "/v1/vectors/1/metadata", r#"{"metadata":{}}"#),
        ("POST", "/v1/batches", r#"{"operations":[]}"#),
        ("POST", "/v1/collections", r#"{"name":"c","dimensions":2}"#),
        ("POST", "/v1/admin/tokens/reload", ""),
        ("POST", "/v1/admin/cloud-backup/run", ""),
        ("PUT", "/v1/admin/cloud-backup", "{}"),
    ] {
        let (status, error) = call(&app, Some(ADMIN), method, uri, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}: {error}");
        assert_eq!(
            error["error"]["code"], "reader_mode",
            "{method} {uri}: {error}"
        );
    }

    let (status, body) = call(&app, Some(ADMIN), "GET", "/v1/admin/serve-status", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["mode"], "reader");
    assert!(body["note"].as_str().unwrap().contains("write capacity"));
    // Administrator only: the read-only token is refused like other admin routes.
    assert_eq!(
        call(&app, Some(READ_ONLY), "GET", "/v1/admin/serve-status", "")
            .await
            .0,
        StatusCode::FORBIDDEN
    );

    service.begin_drain();
    let (status, body) = call(&app, Some(ADMIN), "GET", "/ready", "").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "draining");

    service.close().await.unwrap();
    drop(app);
    assert_eq!(std::fs::read(&path).unwrap(), before);
}
