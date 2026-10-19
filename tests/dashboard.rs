#![cfg(feature = "server")]

use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use tower::ServiceExt;
use vlite::{
    Database,
    server::{BearerToken, Service, authenticated_router},
};

#[tokio::test]
async fn public_dashboard_assets_are_hardened_but_do_not_exempt_data_routes() {
    let directory = tempfile::tempdir().unwrap();
    let service =
        Service::new(Database::create(directory.path().join("dashboard.vlite"), 3).unwrap());
    let token = "secret_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let app = authenticated_router(service.clone(), BearerToken::new(token).unwrap());
    for (uri, content_type, marker) in [
        ("/dashboard", "text/html", "VLite dashboard"),
        ("/dashboard/app.js", "text/javascript", "confirmOperation"),
        ("/dashboard/style.css", "text/css", "color-scheme"),
    ] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert!(
            response.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with(content_type)
        );
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(response.headers()["referrer-policy"], "no-referrer");
        let csp = response.headers()["content-security-policy"]
            .to_str()
            .unwrap();
        assert!(csp.contains("frame-ancestors 'none'") && csp.contains("connect-src 'self'"));
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let content = std::str::from_utf8(&bytes).unwrap();
        assert!(content.contains(marker));
        assert!(!content.contains(token));
    }
    for uri in [
        "/v1/database",
        "/metrics",
        "/ready",
        "/openapi.json",
        "/dashboard/../metrics",
        "/dashboard/private",
        "/dashboard/app.js/other",
    ] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 401, "{uri}");
    }
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/index/hnsw")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    service.close().await.unwrap();
}
