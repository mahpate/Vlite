#![cfg(feature = "server")]
//! Conditional REST writes: per-record strong ETags bound to the database
//! identity (v4 lineage or a per-process nonce for v1-v3), collection, record
//! ID, per-record revision and a full SHA-256 of the record content.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use vlite::{
    Database,
    server::{BearerToken, ServerOptions, Service, configured_admin_router, router},
};

async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    if_match: &[&[u8]],
    body: Option<Value>,
) -> (StatusCode, Option<String>, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    for value in if_match {
        builder = builder.header(header::IF_MATCH, *value);
    }
    let body = match body {
        Some(value) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let etag = response
        .headers()
        .get(header::ETAG)
        .map(|value| value.to_str().unwrap().to_owned());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, etag, value)
}

async fn tag(app: &Router, id: &str) -> String {
    let (status, etag, _) = send(app, "GET", &format!("/v1/vectors/{id}"), &[], None).await;
    assert_eq!(status, StatusCode::OK);
    etag.expect("GET returns ETag")
}

async fn put(app: &Router, id: &str, tag: Option<&str>, value: f32) -> (StatusCode, Value) {
    let tags: Vec<&[u8]> = tag.iter().map(|t| t.as_bytes()).collect();
    let (status, _, body) = send(
        app,
        "PUT",
        &format!("/v1/vectors/{id}"),
        &tags,
        Some(json!({"vector":[value]})),
    )
    .await;
    (status, body)
}

async fn delete(app: &Router, id: &str, tag: Option<&str>) -> (StatusCode, Value) {
    let tags: Vec<&[u8]> = tag.iter().map(|t| t.as_bytes()).collect();
    let (status, _, body) = send(app, "DELETE", &format!("/v1/vectors/{id}"), &tags, None).await;
    (status, body)
}

async fn vector(app: &Router, id: &str) -> Option<Value> {
    let (status, _, body) = send(app, "GET", &format!("/v1/vectors/{id}"), &[], None).await;
    (status == StatusCode::OK).then(|| body["vector"][0].clone())
}

fn assert_precondition_failed((status, body): (StatusCode, Value)) {
    assert_eq!(status, StatusCode::PRECONDITION_FAILED, "{body}");
    assert_eq!(body["error"]["code"], "precondition_failed");
}

fn app_with(records: &[u64]) -> (tempfile::TempDir, Service, Router) {
    let directory = tempfile::tempdir().unwrap();
    let mut db = Database::create_with_format(
        directory.path().join("cas.vlite"),
        1,
        vlite::FormatVersion::V3,
    )
    .unwrap();
    for &id in records {
        db.put(id, &[id as f32], b"p").unwrap();
    }
    let service = Service::new(db);
    let app = router(service.clone());
    (directory, service, app)
}

#[tokio::test]
async fn get_returns_strong_opaque_etag_without_changing_body() {
    let (_dir, _service, app) = app_with(&[u64::MAX]);
    let id = u64::MAX.to_string();
    let (status, etag, body) = send(&app, "GET", &format!("/v1/vectors/{id}"), &[], None).await;
    assert_eq!(status, StatusCode::OK);
    // Additive: `revision` is the record's decimal per-record revision.
    let revision = body["revision"].as_str().expect("decimal revision string");
    assert!(revision.parse::<u64>().unwrap() > 0);
    assert_eq!(
        body,
        json!({"id":id,"vector":[u64::MAX as f32],"payload_base64":"cA==","metadata":{},
            "revision":revision})
    );
    let etag = etag.unwrap();
    assert!(etag.len() > 2 && etag.starts_with('"') && etag.ends_with('"'));
    assert!(!etag.starts_with("W/"));
    // Stable while nothing commits.
    assert_eq!(tag(&app, &id).await, etag);
    let (status, etag, _) = send(&app, "GET", "/v1/vectors/5", &[], None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(etag.is_none());
}

#[tokio::test]
async fn matching_tag_updates_and_deletes_once_competitors_get_412() {
    let (_dir, _service, app) = app_with(&[1]);
    let seen = tag(&app, "1").await;
    assert_eq!(
        put(&app, "1", Some(&seen), 10.0).await.0,
        StatusCode::NO_CONTENT
    );
    assert_precondition_failed(put(&app, "1", Some(&seen), 20.0).await);
    assert_eq!(vector(&app, "1").await, Some(json!(10.0)));

    let seen = tag(&app, "1").await;
    assert_eq!(
        delete(&app, "1", Some(&seen)).await,
        (StatusCode::OK, json!({"deleted":true}))
    );
    assert_precondition_failed(delete(&app, "1", Some(&seen)).await);
    assert_precondition_failed(put(&app, "1", Some(&seen), 30.0).await);
    assert_eq!(vector(&app, "1").await, None, "412 must not create");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_conditional_writers_have_exactly_one_winner() {
    let (_dir, _service, app) = app_with(&[1, 2]);
    for round in 0..5 {
        let seen = tag(&app, "1").await;
        let tasks = (0..8)
            .map(|writer| {
                let app = app.clone();
                let seen = seen.clone();
                tokio::spawn(async move {
                    let value = (round * 100 + writer) as f32;
                    (put(&app, "1", Some(&seen), value).await.0, value)
                })
            })
            .collect::<Vec<_>>();
        let mut winners = Vec::new();
        for task in tasks {
            let (status, value) = task.await.unwrap();
            match status {
                StatusCode::NO_CONTENT => winners.push(value),
                StatusCode::PRECONDITION_FAILED => {}
                other => panic!("unexpected {other}"),
            }
        }
        assert_eq!(winners.len(), 1, "round {round}");
        assert_eq!(vector(&app, "1").await, Some(json!(winners[0])));
    }
    let seen = tag(&app, "2").await;
    let tasks = (0..8)
        .map(|_| {
            let app = app.clone();
            let seen = seen.clone();
            tokio::spawn(async move { delete(&app, "2", Some(&seen)).await })
        })
        .collect::<Vec<_>>();
    let mut deleted = 0;
    for task in tasks {
        match task.await.unwrap() {
            (StatusCode::OK, body) => {
                assert_eq!(body, json!({"deleted":true}));
                deleted += 1;
            }
            failed => assert_precondition_failed(failed),
        }
    }
    assert_eq!(deleted, 1);
}

#[tokio::test]
async fn delete_and_identical_recreate_invalidates_old_tag() {
    let (_dir, _service, app) = app_with(&[1]);
    let seen = tag(&app, "1").await;
    assert_eq!(delete(&app, "1", None).await.0, StatusCode::OK);
    assert_eq!(put(&app, "1", None, 1.0).await.0, StatusCode::NO_CONTENT);
    assert_precondition_failed(put(&app, "1", Some(&seen), 9.0).await);
    assert_precondition_failed(delete(&app, "1", Some(&seen)).await);
    assert_eq!(vector(&app, "1").await, Some(json!(1.0)));
}

#[tokio::test]
async fn tags_are_per_record_unrelated_writes_and_batches_keep_them_valid() {
    let (_dir, _service, app) = app_with(&[1, 2]);
    let seen = tag(&app, "1").await;
    // A write to another record does not invalidate record 1's tag.
    assert_eq!(put(&app, "2", None, 5.0).await.0, StatusCode::NO_CONTENT);
    assert_eq!(tag(&app, "1").await, seen);
    let (status, _, _) = send(
        &app,
        "POST",
        "/v1/batches",
        &[],
        Some(json!({"operations":[{"op":"put","id":"3","vector":[3]}]})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(tag(&app, "1").await, seen);

    // A batch that touches record 1 invalidates it.
    let (status, _, _) = send(
        &app,
        "POST",
        "/v1/batches",
        &[],
        Some(json!({"operations":[{"op":"put","id":"1","vector":[1]}]})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_precondition_failed(delete(&app, "1", Some(&seen)).await);

    // Missing-ID delete and empty batches do not commit, so tags remain valid.
    let seen = tag(&app, "1").await;
    assert_eq!(
        delete(&app, "404", None).await,
        (StatusCode::OK, json!({"deleted":false}))
    );
    let (status, _, _) = send(
        &app,
        "POST",
        "/v1/batches",
        &[],
        Some(json!({"operations":[]})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // A tag is bound to its record ID.
    assert_precondition_failed(put(&app, "2", Some(&seen), 9.0).await);
    assert_eq!(
        put(&app, "1", Some(&seen), 9.0).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(vector(&app, "1").await, Some(json!(9.0)));
}

#[tokio::test]
async fn same_batch_records_get_distinct_independent_tags() {
    let (_dir, _service, app) = app_with(&[]);
    let (status, _, _) = send(
        &app,
        "POST",
        "/v1/batches",
        &[],
        Some(json!({"operations":[
            {"op":"put","id":"1","vector":[4]},
            {"op":"put","id":"2","vector":[4]}]})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (one, two) = (tag(&app, "1").await, tag(&app, "2").await);
    assert_ne!(
        one, two,
        "identical content in one batch still differs by ID"
    );
    assert_precondition_failed(put(&app, "2", Some(&one), 1.0).await);
    assert_eq!(
        put(&app, "1", Some(&one), 1.0).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        put(&app, "2", Some(&two), 2.0).await.0,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn v4_tags_survive_restart_and_divergent_raw_clones_fail_closed() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("v4.vlite");
    let mut db = Database::create_with_format(&path, 1, vlite::FormatVersion::V4).unwrap();
    db.put(1, &[1.0], b"").unwrap();
    let service = Service::new(db);
    let seen = tag(&router(service.clone()), "1").await;
    service.close().await.unwrap();

    // Same lineage and content: the tag is stable across restart.
    let service = Service::new(Database::open(&path).unwrap());
    let app = router(service.clone());
    assert_eq!(tag(&app, "1").await, seen);
    service.close().await.unwrap();

    // A raw byte clone shares the lineage; divergent content at the same
    // revision must still produce different tags.
    let clone = directory.path().join("clone.vlite");
    std::fs::copy(&path, &clone).unwrap();
    let original = Service::new(Database::open(&path).unwrap());
    let copy = Service::new(Database::open(&clone).unwrap());
    let (a, b) = (router(original.clone()), router(copy.clone()));
    assert_eq!(put(&a, "1", None, 10.0).await.0, StatusCode::NO_CONTENT);
    assert_eq!(put(&b, "1", None, 20.0).await.0, StatusCode::NO_CONTENT);
    let (tag_a, tag_b) = (tag(&a, "1").await, tag(&b, "1").await);
    assert_ne!(tag_a, tag_b);
    assert_precondition_failed(put(&b, "1", Some(&tag_a), 30.0).await);
    assert_eq!(vector(&b, "1").await, Some(json!(20.0)));
    original.close().await.unwrap();
    copy.close().await.unwrap();
}

#[tokio::test]
async fn reopened_service_rejects_tags_from_previous_instance() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("reopen.vlite");
    let mut db = Database::create_with_format(&path, 1, vlite::FormatVersion::V3).unwrap();
    db.put(1, &[1.0], b"").unwrap();
    let service = Service::new(db);
    let app = router(service.clone());
    let seen = tag(&app, "1").await;
    service.close().await.unwrap();

    // No commits happened: the sequence is identical, only the nonce differs.
    let service = Service::new(Database::open(&path).unwrap());
    let app = router(service.clone());
    let fresh = tag(&app, "1").await;
    assert_ne!(fresh, seen);
    assert_precondition_failed(put(&app, "1", Some(&seen), 2.0).await);
    assert_precondition_failed(delete(&app, "1", Some(&seen)).await);
    assert_eq!(vector(&app, "1").await, Some(json!(1.0)));
    assert_eq!(
        put(&app, "1", Some(&fresh), 2.0).await.0,
        StatusCode::NO_CONTENT
    );
    service.close().await.unwrap();
}

#[tokio::test]
async fn malformed_weak_wildcard_and_multiple_validators_are_rejected_without_mutation() {
    let (_dir, _service, app) = app_with(&[1]);
    let seen = tag(&app, "1").await;
    let weak = format!("W/{seen}");
    let list = format!("{seen}, {seen}");
    let padded = format!(" {seen}");
    let bad: Vec<Vec<&[u8]>> = vec![
        vec![b"*"],
        vec![weak.as_bytes()],
        vec![list.as_bytes()],
        vec![seen.as_bytes(), seen.as_bytes()],
        vec![b""],
        vec![b"\""],
        vec![b"unquoted"],
        vec![b"\"a\"b"],
        vec![b"\"a b\""],
        vec![b"\"caf\xc3\xa9\""],
        vec![padded.as_bytes()],
    ];
    for headers in &bad {
        for (method, body) in [("PUT", Some(json!({"vector":[7]}))), ("DELETE", None)] {
            for id in ["1", "404"] {
                let (status, _, response) = send(
                    &app,
                    method,
                    &format!("/v1/vectors/{id}"),
                    headers,
                    body.clone(),
                )
                .await;
                assert_eq!(status, StatusCode::BAD_REQUEST, "{method} {headers:?}");
                assert_eq!(response["error"]["code"], "invalid_if_match");
            }
        }
    }
    assert_eq!(tag(&app, "1").await, seen, "no commit happened");
    assert_eq!(vector(&app, "1").await, Some(json!(1.0)));
    assert_eq!(vector(&app, "404").await, None);
}

#[tokio::test]
async fn unconditional_writes_keep_existing_semantics() {
    let (_dir, _service, app) = app_with(&[]);
    let (status, body) = put(&app, "7", None, 1.0).await;
    assert_eq!((status, body), (StatusCode::NO_CONTENT, Value::Null));
    assert_eq!(
        delete(&app, "7", None).await,
        (StatusCode::OK, json!({"deleted":true}))
    );
    assert_eq!(
        delete(&app, "7", None).await,
        (StatusCode::OK, json!({"deleted":false}))
    );
    // A well-formed tag for a missing record never creates or reports a no-op delete.
    assert_precondition_failed(put(&app, "7", Some("\"anything\""), 1.0).await);
    assert_precondition_failed(delete(&app, "7", Some("\"anything\"")).await);
    assert_eq!(vector(&app, "7").await, None);
}

#[tokio::test]
async fn capabilities_advertise_conditional_writes_for_sdk_preflight() {
    let (_dir, _service, app) = app_with(&[]);
    let (status, _, caps) = send(&app, "GET", "/v1/capabilities", &[], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(caps["features"]["conditional_writes"], json!(true));
}

#[tokio::test]
async fn authentication_and_read_only_checks_precede_preconditions() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = Database::create_with_format(
        directory.path().join("auth.vlite"),
        1,
        vlite::FormatVersion::V3,
    )
    .unwrap();
    db.put(1, &[1.0], b"").unwrap();
    let admin = "admin_secret_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let reader = "reader_secret_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let service = Service::new(db);
    let app = configured_admin_router(
        service.clone(),
        ServerOptions {
            admin_token: Some(BearerToken::new(admin).unwrap()),
            read_only_token: Some(BearerToken::new(reader).unwrap()),
            ..ServerOptions::default()
        },
    )
    .unwrap();
    let call = |method: &'static str, token: Option<&'static str>, if_match: &'static str| {
        let app = app.clone();
        async move {
            let mut builder = Request::builder()
                .method(method)
                .uri("/v1/vectors/1")
                .header(header::IF_MATCH, if_match)
                .header(header::CONTENT_TYPE, "application/json");
            if let Some(token) = token {
                builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
            }
            let body = if method == "PUT" {
                Body::from(json!({"vector":[5]}).to_string())
            } else {
                Body::empty()
            };
            let response = app.oneshot(builder.body(body).unwrap()).await.unwrap();
            (
                response.status(),
                response
                    .headers()
                    .get(header::ETAG)
                    .map(|v| v.to_str().unwrap().to_owned()),
            )
        }
    };
    for method in ["PUT", "DELETE"] {
        assert_eq!(call(method, None, "*").await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(
            call(method, None, "\"x\"").await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    let (status, seen) = call("GET", Some(admin), "\"ignored\"").await;
    assert_eq!(status, StatusCode::OK, "If-Match is ignored on GET");
    let seen: &'static str = Box::leak(seen.unwrap().into_boxed_str());
    assert_eq!(
        call("GET", Some(reader), "*").await.1.as_deref(),
        Some(seen)
    );
    for method in ["PUT", "DELETE"] {
        assert_eq!(call(method, None, seen).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(
            call(method, Some(reader), seen).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(method, Some(reader), "*").await.0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        call("GET", Some(admin), "\"x\"").await.1.as_deref(),
        Some(seen)
    );
    assert_eq!(
        call("PUT", Some(admin), seen).await.0,
        StatusCode::NO_CONTENT
    );
}
