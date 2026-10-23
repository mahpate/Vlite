#![cfg(feature = "server")]

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;
use vlite::{
    Database,
    server::{BearerToken, ServerOptions, Service, configured_admin_router},
};

const ADMIN: &str = "logs_admin_secret_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const READER: &str = "logs_reader_secret_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";

async fn request(app: &Router, token: Option<&str>, uri: &str) -> (StatusCode, Value) {
    let mut builder = Request::builder().uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, body)
}

fn app() -> (tempfile::TempDir, Router) {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::create(directory.path().join("logs.vlite"), 3).unwrap();
    let options = ServerOptions {
        admin_token: Some(BearerToken::new(ADMIN).unwrap()),
        read_only_token: Some(BearerToken::new(READER).unwrap()),
        log_buffer_capacity: Some(128),
        ..ServerOptions::default()
    };
    let app = configured_admin_router(Service::new(db), options).unwrap();
    (directory, app)
}

#[tokio::test]
async fn logs_are_filtered_paged_sanitized_and_admin_only() {
    let (_directory, app) = app();
    let query_secret = "url-query-secret-must-not-be-logged";
    let (status, _) = request(
        &app,
        Some(ADMIN),
        &format!("/v1/database?credential={query_secret}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    for _ in 0..2 {
        let (status, _) = request(
            &app,
            Some(ADMIN),
            "/v1/collections/logs-test-filter-marker/vectors",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    assert_eq!(
        request(&app, None, "/v1/admin/logs").await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(&app, Some(READER), "/v1/admin/logs").await.0,
        StatusCode::FORBIDDEN
    );

    let (status, page) = request(
        &app,
        Some(ADMIN),
        "/v1/admin/logs?since=0&level=warn&q=LOGS-TEST-FILTER-MARKER&limit=1",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["entries"].as_array().unwrap().len(), 1);
    let first_seq = page["entries"][0]["seq"].as_u64().unwrap();
    assert_eq!(page["entries"][0]["level"], "warn");
    assert_eq!(
        page["entries"][0]["fields"]["path"],
        "/v1/collections/logs-test-filter-marker/vectors"
    );
    assert_eq!(page["entries"][0]["fields"]["status"], 404);
    assert_eq!(page["next_seq"], first_seq);

    let (status, next) = request(
        &app,
        Some(ADMIN),
        &format!("/v1/admin/logs?since={first_seq}&q=logs-test-filter-marker&limit=1"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(next["entries"].as_array().unwrap().len(), 1);
    assert!(next["entries"][0]["seq"].as_u64().unwrap() > first_seq);
    assert_eq!(next["entries"][0]["level"], "warn");
    let (status, tail) = request(
        &app,
        Some(ADMIN),
        "/v1/admin/logs?q=logs-test-filter-marker&limit=1",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(tail["entries"][0]["seq"], next["entries"][0]["seq"]);

    let (status, filtered) = request(
        &app,
        Some(ADMIN),
        "/v1/admin/logs?q=%2Fv1%2Fdatabase&level=info&limit=1000",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let entry = filtered["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["fields"]["path"] == "/v1/database")
        .unwrap();
    assert_eq!(entry["fields"]["method"], "GET");
    assert!(entry["fields"].get("authorization").is_none());
    let response_text = filtered.to_string();
    assert!(!response_text.contains(ADMIN));
    assert!(!response_text.contains(query_secret));

    assert_eq!(
        request(&app, Some(ADMIN), "/v1/admin/logs?limit=0").await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(&app, Some(ADMIN), "/v1/admin/logs?level=verbose")
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(&app, Some(ADMIN), "/v1/admin/logs?unknown=x")
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn metrics_summary_and_observability_asset_are_available() {
    let (_directory, app) = app();
    request(&app, Some(ADMIN), "/v1/database").await;
    let (status, summary) = request(&app, Some(ADMIN), "/v1/admin/metrics/summary").await;
    assert_eq!(status, StatusCode::OK);
    for field in [
        "ts_ms", "uptime_s", "requests", "search", "database", "logs",
    ] {
        assert!(summary.get(field).is_some(), "missing {field}: {summary}");
    }
    for field in [
        "total",
        "errors",
        "in_flight",
        "by_route",
        "p50_ms",
        "p95_ms",
    ] {
        assert!(
            summary["requests"].get(field).is_some(),
            "missing requests.{field}"
        );
    }
    for field in ["vectors", "collections", "file_bytes", "read_only"] {
        assert!(
            summary["database"].get(field).is_some(),
            "missing database.{field}"
        );
    }
    for field in ["capacity", "dropped", "last_seq"] {
        assert!(summary["logs"].get(field).is_some(), "missing logs.{field}");
    }
    assert!(
        summary["requests"]["by_route"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["route"] == "/v1/database" && row.get("p99_ms").is_some())
    );

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/dashboard/observability.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/javascript")
    );
    assert!(
        !response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .is_empty()
    );
}
