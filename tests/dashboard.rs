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
        (
            "/dashboard/navigation.js",
            "text/javascript",
            "VLiteNavigation",
        ),
        (
            "/dashboard/observability.js",
            "text/javascript",
            "parsePrometheus",
        ),
        ("/dashboard/style.css", "text/css", "color-scheme"),
        (
            "/dashboard/console-tools.js",
            "text/javascript",
            "function advancedTools",
        ),
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
        assert!(
            csp.contains("frame-ancestors 'none'")
                && csp.contains("connect-src 'self'")
                && csp.contains("font-src 'self'")
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let content = std::str::from_utf8(&bytes).unwrap();
        assert!(content.contains(marker));
        assert!(!content.contains(token));
        if uri == "/dashboard" {
            for control in [
                "record-ttl",
                "patch-metadata",
                "rerank-mode",
                "group-search",
                "named-search",
                "dense-sparse-search",
                "geo-search",
                "recommend-run",
                "facets-run",
                "stats-run",
                "language-run",
                "feed-cursor",
                "feed-next",
                "webhook-create",
                "webhook-delete",
                "webhook-status",
                "disk-build",
                "disk-probe",
                "pitr-run",
                "encryption-badge",
            ] {
                assert!(content.contains(&format!("id=\"{control}\"")), "{control}");
            }
            assert!(content.contains("id=\"webhook-secret\" type=\"password\""));
            assert!(content.contains("id=\"pitr-run\" type=\"button\" data-admin"));
            assert!(!content.contains("onclick="));
        }
    }
    for uri in [
        "/v1/database",
        "/metrics",
        "/ready",
        "/openapi.json",
        "/dashboard/../metrics",
        "/dashboard/private",
        "/dashboard/app.js/other",
        "/v1/changes",
        "/v1/admin/webhooks",
        "/v1/collections/default/query",
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

/// The graph explorer assets are public, hardened and served in every server
/// build; graph.js hides its own controls when the graph routes are absent.
#[tokio::test]
async fn graph_dashboard_assets_are_public_and_hardened() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(Database::create(directory.path().join("graph.vlite"), 3).unwrap());
    let token = "secret_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let app = authenticated_router(service.clone(), BearerToken::new(token).unwrap());
    for (uri, content_type, marker) in [
        ("/dashboard/graph.js", "text/javascript", "VLiteGraphTools"),
        ("/dashboard/graph.css", "text/css", "#graph"),
        ("/dashboard/graph3d.js", "text/javascript", "forceLayout"),
        ("/dashboard/ambience.js", "text/javascript", "WebGLRenderer"),
        (
            "/dashboard/three.min.js",
            "text/javascript",
            "WebGLRenderer",
        ),
        ("/dashboard/three.LICENSE", "text/plain", "MIT License"),
        ("/dashboard/theme.css", "text/css", "--vlite-background"),
        (
            "/dashboard/fonts/OFL-Outfit.txt",
            "text/plain",
            "SIL OPEN FONT LICENSE",
        ),
        (
            "/dashboard/fonts/OFL-JetBrainsMono.txt",
            "text/plain",
            "SIL OPEN FONT LICENSE",
        ),
    ] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{uri}");
        let headers = response.headers();
        assert!(
            headers["content-type"]
                .to_str()
                .unwrap()
                .starts_with(content_type)
        );
        assert_eq!(headers["cache-control"], "no-store");
        assert_eq!(headers["x-content-type-options"], "nosniff");
        let csp = headers["content-security-policy"].to_str().unwrap();
        assert!(csp.contains("frame-ancestors 'none'") && csp.contains("connect-src 'self'"));
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert!(std::str::from_utf8(&bytes).unwrap().contains(marker));
    }
    for uri in [
        "/dashboard/fonts/outfit-latin-wght-normal.woff2",
        "/dashboard/fonts/jetbrains-mono-latin-wght-normal.woff2",
    ] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{uri}");
        assert_eq!(response.headers()["content-type"], "font/woff2");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert!(
            response.headers()["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("font-src 'self'")
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert!(bytes.starts_with(b"wOF2"), "{uri}");
        assert!(bytes.len() > 1000, "{uri}");
    }
    service.close().await.unwrap();
}
