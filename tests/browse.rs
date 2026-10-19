#![cfg(feature = "server")]

use axum::{Router, body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use vlite::{
    Database, Metadata, MetadataValue,
    server::{BearerToken, Service, authenticated_router, router},
};

async fn get(app: &Router, uri: &str) -> (u16, Value, usize) {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap(), bytes.len())
}

#[tokio::test]
async fn cursor_pages_follow_numeric_ids_without_vector_payload_or_cursor_overflow() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("browse.vlite");
    let mut db = Database::create(&path, 1).unwrap();
    for id in [0, 2, 10, u64::MAX] {
        db.put_with_metadata(
            id,
            &[1.0],
            b"private",
            &Metadata::from([("count".into(), MetadataValue::Integer(i64::MAX))]),
        )
        .unwrap();
    }
    let service = Service::new(db);
    let app = router(service.clone());
    let (status, page, _) = get(&app, "/v1/vectors?limit=2").await;
    assert_eq!(status, 200);
    assert_eq!(page["items"][0]["id"], "0");
    assert_eq!(page["items"][1]["id"], "2");
    assert_eq!(page["next_after"], "2");
    assert_eq!(
        page["items"][0]["metadata"]["count"]["value"],
        i64::MAX.to_string()
    );
    assert!(page["items"][0].get("vector").is_none());
    assert!(page["items"][0].get("payload_base64").is_none());
    let (_, page, _) = get(&app, "/v1/vectors?limit=2&after=2").await;
    assert_eq!(page["items"][0]["id"], "10");
    assert_eq!(page["items"][1]["id"], u64::MAX.to_string());
    assert!(page["next_after"].is_null());
    let (_, page, _) = get(&app, "/v1/vectors?after=18446744073709551615").await;
    assert_eq!(page, json!({"items":[], "next_after":null}));
    // Cursor is an exclusive bound, not required to name an existing record.
    assert_eq!(
        get(&app, "/v1/vectors?after=3&limit=1").await.1["items"][0]["id"],
        "10"
    );
    service.close().await.unwrap();
    let reopened = Service::new(Database::open(path).unwrap());
    assert_eq!(
        get(&router(reopened.clone()), "/v1/vectors").await.1["items"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn browsing_is_bounded_validated_and_protected() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(Database::create(directory.path().join("empty.vlite"), 1).unwrap());
    let app = router(service.clone());
    assert_eq!(
        get(&app, "/v1/vectors").await.1,
        json!({"items":[], "next_after":null})
    );
    for uri in ["/v1/vectors?limit=0", "/v1/vectors?limit=101"] {
        assert_eq!(get(&app, uri).await.0, 422, "{uri}");
    }
    for uri in [
        "/v1/vectors?after=01",
        "/v1/vectors?after=-1",
        "/v1/vectors?after=",
        "/v1/vectors?after=18446744073709551616",
        "/v1/vectors?limit=x",
        "/v1/vectors?limit=1&limit=2",
        "/v1/vectors?after=0&after=1",
        "/v1/vectors?unknown=1",
        "/v1/vectors?after=%2B1",
    ] {
        assert_eq!(get(&app, uri).await.0, 400, "{uri}");
    }
    let protected =
        authenticated_router(service.clone(), BearerToken::new(&"a".repeat(32)).unwrap());
    for uri in ["/v1/vectors", "/v1/index/hnsw"] {
        assert_eq!(get(&protected, uri).await.0, 401);
    }
    let (_, state, _) = get(&app, "/v1/index/hnsw").await;
    assert_eq!(state["ready"], false);
    assert_eq!(state["buffered_ids"], 0);
    assert_eq!(state["max_buffer_ids"], 256);
    service.close().await.unwrap();
    assert_eq!(get(&app, "/v1/vectors").await.0, 503);
    assert_eq!(get(&app, "/v1/index/hnsw").await.0, 503);
}

#[tokio::test]
async fn escaped_metadata_pages_obey_response_byte_budget_and_always_advance() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = Database::create(directory.path().join("large.vlite"), 1).unwrap();
    let metadata: Metadata = (0..15)
        .map(|index| {
            (
                format!("field{index}"),
                MetadataValue::String("\0".repeat(4_000)),
            )
        })
        .collect();
    for id in 0..5 {
        db.put_with_metadata(id, &[0.0], b"", &metadata).unwrap();
    }
    let service = Service::new(db);
    let app = router(service.clone());
    let mut cursor = None;
    let mut ids = Vec::new();
    loop {
        let uri = cursor
            .as_ref()
            .map_or("/v1/vectors?limit=100".to_owned(), |after| {
                format!("/v1/vectors?limit=100&after={after}")
            });
        let (status, page, bytes) = get(&app, &uri).await;
        assert_eq!(status, 200);
        assert!(bytes <= 1_048_576);
        let items = page["items"].as_array().unwrap();
        assert!(!items.is_empty());
        assert!(items.len() <= 2);
        ids.extend(
            items
                .iter()
                .map(|item| item["id"].as_str().unwrap().to_owned()),
        );
        cursor = page["next_after"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(ids, ["0", "1", "2", "3", "4"]);
    service.close().await.unwrap();
}

#[tokio::test]
async fn index_status_reflects_buffered_graph_without_building() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = Database::create(directory.path().join("index.vlite"), 1).unwrap();
    db.put(0, &[0.0], b"").unwrap();
    db.build_hnsw(Default::default()).unwrap();
    db.put(0, &[1.0], b"").unwrap();
    let service = Service::new(db);
    let app = router(service.clone());
    let (_, state, _) = get(&app, "/v1/index/hnsw").await;
    assert_eq!(state["ready"], true);
    assert_eq!(state["buffered_ids"], 1);
    assert_eq!(state["buffer_vector_bytes"], 8);
    assert_eq!(get(&app, "/metrics").await.1["hnsw_rebuilds_total"], 0);
    service.close().await.unwrap();
}
