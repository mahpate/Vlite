#![cfg(feature = "server")]

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::Value;
use tower::ServiceExt;
use vlite::{
    CreateOptions, Database, FormatVersion,
    server::{Service, router},
};

#[tokio::test]
async fn ttl_metadata_updates_grouping_facets_and_change_feed_are_exposed() {
    let db = Database::create_in_memory(
        2,
        CreateOptions {
            format: Some(FormatVersion::V5),
            ..Default::default()
        },
    )
    .unwrap();
    let app = router(Service::new(db));
    let put = app.clone().oneshot(Request::builder().method("PUT").uri("/v1/vectors/7")
        .header("content-type","application/json").body(Body::from(r#"{"vector":[1.0,0.0],"ttl_ms":100000,"metadata":{"category":{"type":"string","value":"poppy"}}}"#)).unwrap()).await.unwrap();
    assert_eq!(put.status(), StatusCode::NO_CONTENT);
    let patch = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/v1/vectors/7/metadata")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"metadata":{"category":{"type":"string","value":"marigold"}}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(patch.status(), StatusCode::NO_CONTENT);
    let group = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/collections/default/search/grouped")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"vector":[1.0,0.0],"field":"category","groups":2,"hits_per_group":2}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(group.status(), StatusCode::OK);
    let payload = axum::body::to_bytes(group.into_body(), usize::MAX)
        .await
        .unwrap();
    let groups: Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(groups["groups"][0]["value"]["value"], "marigold");
    let changes = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/changes?limit=20")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(changes.status(), StatusCode::OK);
    let payload = axum::body::to_bytes(changes.into_body(), usize::MAX)
        .await
        .unwrap();
    let changes: Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(changes["changes"].as_array().unwrap().len(), 2);
    assert!(changes["changes"][0]["change"]["expires_at_ms"].is_number());
    let embedding = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/collections/default/embed")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"text":"searchable phrase"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(embedding.status(), StatusCode::OK);
    let payload = axum::body::to_bytes(embedding.into_body(), usize::MAX)
        .await
        .unwrap();
    let embedding: Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(embedding["dimensions"], 2);
    assert_eq!(embedding["vector"].as_array().unwrap().len(), 2);
    let reranked = app.oneshot(Request::builder().method("POST").uri("/v1/search")
        .header("content-type", "application/json").body(Body::from(r#"{"vector":[1.0,0.0],"k":1,"rerank":{"mode":"mmr","candidates":1,"lambda":0.7}}"#)).unwrap()).await.unwrap();
    assert_eq!(reranked.status(), StatusCode::OK);
    let payload = axum::body::to_bytes(reranked.into_body(), usize::MAX)
        .await
        .unwrap();
    let reranked: Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(reranked["hits"].as_array().unwrap().len(), 1);
}
