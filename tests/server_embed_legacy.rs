#![cfg(feature = "server")]

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde::Deserialize;
use serde_json::Value;
use tower::ServiceExt;
use vlite::{
    CreateOptions, Database,
    server::{Service, router},
};

#[derive(Deserialize)]
struct Case {
    file: String,
    dimensions: usize,
    path: String,
    request: Value,
    status: u16,
}

#[tokio::test]
async fn hashing_embed_matches_released_response_bytes() {
    let cases: Vec<Case> =
        serde_json::from_str(include_str!("fixtures/embed_legacy/cases.json")).unwrap();
    for case in cases {
        if case.status != 200 && case.path != "/v1/embed" {
            continue;
        }
        let db = Database::create_in_memory(case.dimensions, CreateOptions::default()).unwrap();
        let app = router(Service::new(db));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&case.path)
                    .header("content-type", "application/json")
                    .body(Body::from(case.request.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::from_u16(case.status).unwrap(),
            "{}",
            case.file
        );
        let body = axum::body::to_bytes(response.into_body(), 1_048_576)
            .await
            .unwrap();
        let expected = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("fixtures")
                .join("embed_legacy")
                .join(&case.file),
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(body.to_vec()).unwrap(),
            String::from_utf8(expected).unwrap(),
            "{}",
            case.file
        );
    }
}
