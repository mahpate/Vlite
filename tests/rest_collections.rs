#![cfg(feature = "server")]
//! REST collections, collection-scoped data/index routes and resumable imports.

use axum::{
    Router,
    body::Body,
    http::{HeaderMap, Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use vlite::{
    Database, FormatVersion,
    server::{BearerToken, ServerOptions, Service, configured_admin_router, router},
};

const ADMIN: &str = "admin_secret_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const READ: &str = "reader_secret_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";

async fn call(
    app: &Router,
    token: Option<&str>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
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
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes)))
    };
    (status, headers, value)
}

async fn ok(app: &Router, method: &str, uri: &str, body: Option<Value>) -> Value {
    let (status, _, value) = call(app, None, method, uri, body).await;
    assert!(status.is_success(), "{method} {uri}: {status} {value}");
    value
}

fn v4(dimensions: usize) -> (tempfile::TempDir, Service, Router) {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::create_with_format(
        directory.path().join("c.vlite"),
        dimensions,
        FormatVersion::V4,
    )
    .unwrap();
    let service = Service::new(db);
    let app = router(service.clone());
    (directory, service, app)
}

#[tokio::test]
async fn collections_lifecycle_settings_and_isolated_data_routes() {
    let (_dir, service, app) = v4(2);
    let listed = ok(&app, "GET", "/v1/collections", None).await;
    assert_eq!(listed["format_version"], 4);
    assert_eq!(listed["items"].as_array().unwrap().len(), 1);
    assert_eq!(listed["items"][0]["name"], "default");
    assert_eq!(listed["items"][0]["id"], 0);

    let (status, _, created) = call(
        &app,
        None,
        "POST",
        "/v1/collections",
        Some(json!({"name":"docs","dimensions":3,"settings":{
            "metric":"cosine","hnsw":{"m":8,"ef_construction":32,"seed":"7"},
            "quantization":"scalar_u8","rerank_multiplier":2,
            "planner":{"exact_max_candidates":10,"exact_max_selectivity_ppm":5},
            "metadata_indexes":[{"field":"lang","kind":"eq"},{"field":"year","kind":"eq_range"}],
            "text_fields":["title"]}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["name"], "docs");
    assert_eq!(created["dimensions"], 3);
    assert_eq!(created["live_records"], 0);
    assert_eq!(created["settings_persisted"], true);
    let settings = &created["settings"];
    assert_eq!(settings["metric"], "cosine");
    assert_eq!(
        settings["hnsw"],
        json!({"m":8,"ef_construction":32,"seed":"7"})
    );
    assert_eq!(settings["quantization"], "scalar_u8");
    assert_eq!(settings["rerank_multiplier"], 2);
    assert_eq!(
        settings["metadata_indexes"],
        json!([{"field":"lang","kind":"eq"},{"field":"year","kind":"eq_range"}])
    );
    assert_eq!(settings["text_fields"], json!(["title"]));
    let id = created["id"].as_u64().unwrap();
    assert!(id > 0);

    let (status, _, error) = call(
        &app,
        None,
        "POST",
        "/v1/collections",
        Some(json!({"name":"docs","dimensions":3})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error["error"]["code"], "collection_exists");
    for body in [
        json!({"name":"Bad Name","dimensions":3}),
        json!({"name":"x","dimensions":0}),
        json!({"name":"x","dimensions":3,"settings":{"rerank_multiplier":0}}),
        json!({"name":"x","dimensions":3,"unknown":1}),
    ] {
        let (status, _, _) = call(&app, None, "POST", "/v1/collections", Some(body)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    // Settings merge: omitted fields keep current values.
    let updated = ok(
        &app,
        "PUT",
        "/v1/collections/docs/settings",
        Some(json!({"rerank_multiplier":5})),
    )
    .await;
    assert_eq!(updated["persisted"], true);
    assert_eq!(updated["collection"]["settings"]["rerank_multiplier"], 5);
    assert_eq!(updated["collection"]["settings"]["metric"], "cosine");
    assert_eq!(
        updated["collection"]["settings"]["text_fields"],
        json!(["title"])
    );

    // Same record ID in two collections is independent.
    ok(
        &app,
        "PUT",
        "/v1/collections/docs/vectors/1",
        Some(json!({"vector":[1,0,0],"metadata":{"title":{"type":"string","value":"hello world"},"lang":{"type":"string","value":"en"}}})),
    )
    .await;
    ok(&app, "PUT", "/v1/vectors/1", Some(json!({"vector":[5,5]}))).await;
    let docs = ok(&app, "GET", "/v1/collections/docs/vectors/1", None).await;
    assert_eq!(docs["vector"], json!([1.0, 0.0, 0.0]));
    let default = ok(&app, "GET", "/v1/collections/default/vectors/1", None).await;
    assert_eq!(default["vector"], json!([5.0, 5.0]));
    assert_eq!(
        ok(&app, "GET", "/v1/vectors/1", None).await["vector"],
        json!([5.0, 5.0])
    );
    ok(
        &app,
        "POST",
        "/v1/collections/docs/batches",
        Some(json!({"operations":[{"op":"put","id":"2","vector":[0,1,0]},{"op":"put","id":"3","vector":[0,0,1]}]})),
    )
    .await;
    let page = ok(&app, "GET", "/v1/collections/docs/vectors?limit=2", None).await;
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    assert_eq!(page["next_after"], "2");
    let page = ok(
        &app,
        "GET",
        "/v1/collections/docs/vectors?limit=2&after=2",
        None,
    )
    .await;
    assert_eq!(page["items"][0]["id"], "3");
    // Collection routes default to the collection's metric (cosine).
    let hits = ok(
        &app,
        "POST",
        "/v1/collections/docs/search",
        Some(json!({"vector":[2,0,0],"k":1})),
    )
    .await;
    assert_eq!(hits["hits"][0]["id"], "1");
    assert!(hits["hits"][0]["distance"].as_f64().unwrap().abs() < 1e-9);
    let got = ok(&app, "GET", "/v1/collections/docs", None).await;
    assert_eq!(got["live_records"], 3);
    assert_eq!(got["text_index"]["declared_fields"], 1);
    assert_eq!(got["text_index"]["documents"], 1);
    assert_eq!(
        ok(&app, "GET", "/v1/database", None).await["records"],
        1,
        "legacy database info stays on default"
    );

    // Dimension mismatches are reported per collection.
    let (status, _, _) = call(
        &app,
        None,
        "PUT",
        "/v1/collections/docs/vectors/9",
        Some(json!({"vector":[1,2]})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Unknown collections never fall back to default.
    for (method, uri, body) in [
        ("GET", "/v1/collections/nope", None),
        ("GET", "/v1/collections/nope/vectors", None),
        ("GET", "/v1/collections/nope/vectors/1", None),
        (
            "PUT",
            "/v1/collections/nope/vectors/1",
            Some(json!({"vector":[1,2]})),
        ),
        (
            "POST",
            "/v1/collections/nope/search",
            Some(json!({"vector":[1,2],"k":1})),
        ),
        ("GET", "/v1/collections/nope/index/hnsw", None),
        ("POST", "/v1/collections/nope/index/hnsw", Some(json!({}))),
        ("POST", "/v1/collections/nope/imports", None),
    ] {
        let (status, _, error) = call(&app, None, method, uri, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
        assert_eq!(error["error"]["code"], "collection_not_found");
    }

    // Drops require an empty collection; default is immutable.
    let (status, _, error) = call(&app, None, "DELETE", "/v1/collections/docs", None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error["error"]["code"], "collection_not_empty");
    let (status, _, error) = call(&app, None, "DELETE", "/v1/collections/default", None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error["error"]["code"], "default_collection_immutable");
    for id in ["1", "2", "3"] {
        ok(
            &app,
            "DELETE",
            &format!("/v1/collections/docs/vectors/{id}"),
            None,
        )
        .await;
    }
    assert_eq!(
        ok(&app, "DELETE", "/v1/collections/docs", None).await,
        json!({"deleted":true})
    );
    let recreated = ok(
        &app,
        "POST",
        "/v1/collections",
        Some(json!({"name":"docs","dimensions":2})),
    )
    .await;
    assert!(
        recreated["id"].as_u64().unwrap() > id,
        "IDs are never reused"
    );
    service.close().await.unwrap();
}

#[tokio::test]
async fn collection_etags_are_bound_to_collection_identity() {
    let (_dir, service, app) = v4(1);
    ok(
        &app,
        "POST",
        "/v1/collections",
        Some(json!({"name":"a","dimensions":1})),
    )
    .await;
    ok(&app, "PUT", "/v1/vectors/1", Some(json!({"vector":[1]}))).await;
    ok(
        &app,
        "PUT",
        "/v1/collections/a/vectors/1",
        Some(json!({"vector":[1]})),
    )
    .await;
    let (_, default_headers, default_body) = call(&app, None, "GET", "/v1/vectors/1", None).await;
    let (_, a_headers, _) = call(&app, None, "GET", "/v1/collections/a/vectors/1", None).await;
    let default_tag = default_headers[header::ETAG].to_str().unwrap().to_owned();
    let a_tag = a_headers[header::ETAG].to_str().unwrap().to_owned();
    assert_ne!(default_tag, a_tag);
    assert_eq!(default_body["revision"], "2");
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/v1/collections/a/vectors/1")
                .header(header::IF_MATCH, &default_tag)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json!({"vector":[2]}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/v1/collections/a/vectors/1")
                .header(header::IF_MATCH, &a_tag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    service.close().await.unwrap();
}

#[tokio::test]
async fn collection_index_routes_use_collection_identity_and_never_touch_default() {
    let (_dir, service, app) = v4(2);
    ok(
        &app,
        "POST",
        "/v1/collections",
        Some(json!({"name":"img","dimensions":2,"settings":{"quantization":"scalar_u8","hnsw":{"m":4,"ef_construction":16}}})),
    )
    .await;
    let operations = (1..=40)
        .map(|i| json!({"op":"put","id":i.to_string(),"vector":[i as f32, (i % 7) as f32]}))
        .collect::<Vec<_>>();
    ok(
        &app,
        "POST",
        "/v1/collections/img/batches",
        Some(json!({ "operations": operations })),
    )
    .await;
    let status = ok(&app, "GET", "/v1/collections/img/index/hnsw", None).await;
    assert_eq!(status["ready"], false);
    assert_eq!(status["settings_ready"], false);
    let (code, _, _) = call(
        &app,
        None,
        "POST",
        "/v1/collections/img/index/hnsw",
        Some(json!({})),
    )
    .await;
    assert_eq!(code, StatusCode::NO_CONTENT);
    let status = ok(&app, "GET", "/v1/collections/img/index/hnsw", None).await;
    assert_eq!(status["ready"], true);
    assert_eq!(status["settings_ready"], true);
    assert_eq!(status["quantization"], "scalar_u8");
    assert_eq!(status["config"]["m"], 4);
    assert_eq!(status["graph_nodes"], 40);
    assert!(status["memory"]["code_bytes"].as_u64().unwrap() > 0);
    assert_eq!(
        ok(&app, "GET", "/v1/index/hnsw", None).await["ready"],
        false,
        "default untouched"
    );
    let ready = ok(
        &app,
        "POST",
        "/v1/collections/img/search",
        Some(json!({"vector":[3,3],"k":2,"mode":"approximate_ready","explain":true})),
    )
    .await;
    assert_eq!(ready["hits"][0]["id"], "3");
    assert!(
        ready["stats"]["quantized_distance_evaluations"]
            .as_u64()
            .unwrap()
            > 0
    );
    let quality = ok(
        &app,
        "POST",
        "/v1/collections/img/index/hnsw/quality",
        Some(json!({"samples":4,"k":3})),
    )
    .await;
    assert_eq!(quality["samples_used"], 4);
    assert!(quality["recall_at_k"].as_f64().unwrap() > 0.0);
    let maintenance = ok(
        &app,
        "GET",
        "/v1/collections/img/index/hnsw/maintenance",
        None,
    )
    .await;
    assert_eq!(maintenance["status"]["ready"], true);
    let (code, _, started) = call(
        &app,
        None,
        "POST",
        "/v1/collections/img/index/hnsw/rebuild",
        Some(json!({})),
    )
    .await;
    assert_eq!(code, StatusCode::ACCEPTED, "{started}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
    let outcome = loop {
        let polled = ok(
            &app,
            "POST",
            "/v1/collections/img/index/hnsw/rebuild/poll",
            None,
        )
        .await;
        let outcome = polled["outcome"].as_str().unwrap().to_owned();
        if outcome != "running" {
            break outcome;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "rebuild did not finish"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    assert_eq!(outcome, "published");
    assert_eq!(
        ok(
            &app,
            "POST",
            "/v1/collections/img/index/hnsw/rebuild/cancel",
            None
        )
        .await,
        json!({"cancelled":false})
    );
    let tick = ok(
        &app,
        "POST",
        "/v1/collections/img/index/hnsw/maintenance/tick",
        None,
    )
    .await;
    assert!(tick["outcome"].is_string());
    service.close().await.unwrap();
}

#[tokio::test]
async fn v3_files_reject_collection_creation_but_serve_the_default_collection() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::create_with_format(directory.path().join("v3.vlite"), 2, FormatVersion::V3)
        .unwrap();
    let service = Service::new(db);
    let app = router(service.clone());
    let (status, _, error) = call(
        &app,
        None,
        "POST",
        "/v1/collections",
        Some(json!({"name":"a","dimensions":2})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error["error"]["code"], "requires_format_v4");
    let listed = ok(&app, "GET", "/v1/collections", None).await;
    assert_eq!(listed["format_version"], 3);
    ok(
        &app,
        "PUT",
        "/v1/collections/default/vectors/4",
        Some(json!({"vector":[1,2]})),
    )
    .await;
    assert_eq!(
        ok(&app, "GET", "/v1/vectors/4", None).await["vector"],
        json!([1.0, 2.0])
    );
    let updated = ok(
        &app,
        "PUT",
        "/v1/collections/default/settings",
        Some(json!({"metric":"cosine"})),
    )
    .await;
    assert_eq!(updated["persisted"], false);
    let (status, _, error) =
        call(&app, None, "POST", "/v1/collections/default/imports", None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error["error"]["code"], "requires_format_v4");
    service.close().await.unwrap();
}

#[tokio::test]
async fn read_only_role_is_enforced_on_collection_routes() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::create_with_format(directory.path().join("r.vlite"), 2, FormatVersion::V4)
        .unwrap();
    let service = Service::new(db);
    let app = configured_admin_router(
        service.clone(),
        ServerOptions {
            admin_token: Some(BearerToken::new(ADMIN).unwrap()),
            read_only_token: Some(BearerToken::new(READ).unwrap()),
            ..ServerOptions::default()
        },
    )
    .unwrap();
    let (status, _, _) = call(
        &app,
        Some(ADMIN),
        "POST",
        "/v1/collections",
        Some(json!({"name":"a","dimensions":2})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    call(
        &app,
        Some(ADMIN),
        "PUT",
        "/v1/collections/a/vectors/1",
        Some(json!({"vector":[1,1]})),
    )
    .await;
    for (method, uri, body, expected) in [
        ("GET", "/v1/collections", None, StatusCode::OK),
        ("GET", "/v1/collections/a", None, StatusCode::OK),
        ("GET", "/v1/collections/a/vectors/1", None, StatusCode::OK),
        (
            "POST",
            "/v1/collections/a/search",
            Some(json!({"vector":[1,1],"k":1,"mode":"auto"})),
            StatusCode::OK,
        ),
        (
            "POST",
            "/v1/collections/a/search",
            Some(json!({"vector":[1,1],"k":1,"mode":"approximate"})),
            StatusCode::FORBIDDEN,
        ),
        (
            "POST",
            "/v1/collections",
            Some(json!({"name":"b","dimensions":2})),
            StatusCode::FORBIDDEN,
        ),
        (
            "PUT",
            "/v1/collections/a/vectors/2",
            Some(json!({"vector":[1,1]})),
            StatusCode::FORBIDDEN,
        ),
        ("DELETE", "/v1/collections/a", None, StatusCode::FORBIDDEN),
        (
            "PUT",
            "/v1/collections/a/settings",
            Some(json!({})),
            StatusCode::FORBIDDEN,
        ),
        (
            "POST",
            "/v1/collections/a/imports",
            None,
            StatusCode::FORBIDDEN,
        ),
        (
            "POST",
            "/v1/collections/a/index/hnsw",
            Some(json!({})),
            StatusCode::FORBIDDEN,
        ),
    ] {
        let (status, _, value) = call(&app, Some(READ), method, uri, body).await;
        assert_eq!(status, expected, "{method} {uri} {value}");
    }
    service.close().await.unwrap();
}

fn put_op(id: u64, vector: Value) -> Value {
    json!({"op":"put","id":id.to_string(),"vector":vector})
}

#[tokio::test]
async fn imports_commit_replay_reject_skip_finish_and_abort() {
    let (_dir, service, app) = v4(2);
    ok(
        &app,
        "POST",
        "/v1/collections",
        Some(json!({"name":"bulk","dimensions":2})),
    )
    .await;
    let (status, _, job) = call(&app, None, "POST", "/v1/collections/bulk/imports", None).await;
    assert_eq!(status, StatusCode::CREATED, "{job}");
    assert_eq!(job["state"], "open");
    assert_eq!(job["durable"], false);
    assert_eq!(job["collection"], "bulk");
    let id = job["job"].as_str().unwrap().to_owned();
    let status = ok(&app, "GET", &format!("/v1/imports/{id}"), None).await;
    assert_eq!(status["durable"], false);

    let chunk1 = json!({"operations":[put_op(1, json!([1,1])), put_op(2, json!([2,2]))]});
    let report = ok(
        &app,
        "PUT",
        &format!("/v1/imports/{id}/chunks/1"),
        Some(chunk1.clone()),
    )
    .await;
    assert_eq!(report["outcome"], "committed");
    assert_eq!(report["rows_committed"], 2);
    assert_eq!(report["rows_skipped"], 0);
    assert!(report["revision"].is_string());
    let replay = ok(
        &app,
        "PUT",
        &format!("/v1/imports/{id}/chunks/1"),
        Some(chunk1.clone()),
    )
    .await;
    assert_eq!(replay["outcome"], "already_committed");

    // RejectChunk writes nothing and reports invalid rows.
    let bad = json!({"operations":[put_op(3, json!([1,1])), put_op(4, json!([1,2,3]))]});
    let (status, _, rejected) = call(
        &app,
        None,
        "PUT",
        &format!("/v1/imports/{id}/chunks/2"),
        Some(bad.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(rejected["error"]["code"], "import_rows_invalid");
    assert_eq!(rejected["outcome"], "rejected");
    assert_eq!(rejected["invalid_rows"], 1);
    assert_eq!(rejected["invalid"][0]["row"], 1);
    assert_eq!(rejected["invalid"][0]["code"], "dimension_mismatch");
    let (status, _, _) = call(&app, None, "GET", "/v1/collections/bulk/vectors/3", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let mut skip = bad.clone();
    skip["policy"] = json!("skip_invalid");
    let skipped = ok(
        &app,
        "PUT",
        &format!("/v1/imports/{id}/chunks/2"),
        Some(skip),
    )
    .await;
    assert_eq!(skipped["outcome"], "committed");
    assert_eq!(skipped["rows_committed"], 1);
    assert_eq!(skipped["rows_skipped"], 1);

    // Out-of-order chunks conflict.
    let (status, _, error) = call(
        &app,
        None,
        "PUT",
        &format!("/v1/imports/{id}/chunks/5"),
        Some(chunk1.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error["error"]["code"], "import_conflict");

    let status = ok(&app, "GET", &format!("/v1/imports/{id}"), None).await;
    assert_eq!(status["durable"], true);
    assert_eq!(status["last_chunk"], 2);
    assert_eq!(status["rows_committed"], 3);
    assert_eq!(status["rows_skipped"], 1);
    assert_eq!(status["last_digest"].as_str().unwrap().len(), 64);
    let finished = ok(&app, "POST", &format!("/v1/imports/{id}/finish"), None).await;
    assert_eq!(finished["state"], "finished");
    assert_eq!(
        ok(&app, "POST", &format!("/v1/imports/{id}/finish"), None).await["state"],
        "finished"
    );
    let (status, _, _) = call(&app, None, "POST", &format!("/v1/imports/{id}/abort"), None).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // A reserved-only job cannot finish; abort releases it.
    let job = ok(&app, "POST", "/v1/collections/bulk/imports", None).await;
    let other = job["job"].as_str().unwrap().to_owned();
    assert_ne!(other, id);
    let (status, _, error) = call(
        &app,
        None,
        "POST",
        &format!("/v1/imports/{other}/finish"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error["error"]["code"], "import_conflict");
    let aborted = ok(&app, "POST", &format!("/v1/imports/{other}/abort"), None).await;
    assert_eq!(aborted["state"], "aborted");
    let (status, _, _) = call(&app, None, "GET", &format!("/v1/imports/{other}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    for bad in ["0", "01", "x", "18446744073709551616"] {
        let (status, _, _) = call(&app, None, "GET", &format!("/v1/imports/{bad}"), None).await;
        assert!(
            status == StatusCode::BAD_REQUEST || status == StatusCode::NOT_FOUND,
            "{bad}"
        );
    }
    service.close().await.unwrap();
}

#[tokio::test]
async fn dropped_collection_jobs_cannot_act_through_a_recreated_collection() {
    let (_dir, service, app) = v4(2);
    ok(
        &app,
        "POST",
        "/v1/collections",
        Some(json!({"name":"tmp","dimensions":2})),
    )
    .await;
    let job = ok(&app, "POST", "/v1/collections/tmp/imports", None).await;
    let id = job["job"].as_str().unwrap().to_owned();
    let chunk = json!({"operations":[put_op(1, json!([1,1]))]});
    ok(
        &app,
        "PUT",
        &format!("/v1/imports/{id}/chunks/1"),
        Some(chunk.clone()),
    )
    .await;
    ok(&app, "POST", &format!("/v1/imports/{id}/finish"), None).await;
    ok(&app, "DELETE", "/v1/collections/tmp/vectors/1", None).await;
    ok(&app, "DELETE", "/v1/collections/tmp", None).await;
    ok(
        &app,
        "POST",
        "/v1/collections",
        Some(json!({"name":"tmp","dimensions":2})),
    )
    .await;
    let status = ok(&app, "GET", &format!("/v1/imports/{id}"), None).await;
    assert_eq!(status["collection"], "tmp");
    assert_eq!(status["collection_dropped"], true);
    for (method, uri, body) in [
        (
            "PUT",
            format!("/v1/imports/{id}/chunks/1"),
            Some(chunk.clone()),
        ),
        (
            "PUT",
            format!("/v1/imports/{id}/chunks/2"),
            Some(chunk.clone()),
        ),
        ("POST", format!("/v1/imports/{id}/finish"), None),
        ("POST", format!("/v1/imports/{id}/abort"), None),
    ] {
        let (status, _, error) = call(&app, None, method, &uri, body).await;
        assert_eq!(status, StatusCode::CONFLICT, "{method} {uri} {error}");
        assert_eq!(error["error"]["code"], "import_conflict");
    }
    assert_eq!(
        ok(&app, "GET", "/v1/collections/tmp", None).await["live_records"],
        0
    );

    // A reservation whose collection was dropped and recreated also conflicts.
    let job = ok(&app, "POST", "/v1/collections/tmp/imports", None).await;
    let reserved = job["job"].as_str().unwrap().to_owned();
    ok(&app, "DELETE", "/v1/collections/tmp", None).await;
    ok(
        &app,
        "POST",
        "/v1/collections",
        Some(json!({"name":"tmp","dimensions":2})),
    )
    .await;
    let (status, _, _) = call(
        &app,
        None,
        "PUT",
        &format!("/v1/imports/{reserved}/chunks/1"),
        Some(chunk),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        ok(&app, "GET", "/v1/collections/tmp", None).await["live_records"],
        0
    );
    service.close().await.unwrap();
}
#[tokio::test]
async fn auto_hybrid_and_explain_search_modes_over_rest() {
    let (_dir, service, app) = v4(2);
    ok(
        &app,
        "POST",
        "/v1/collections",
        Some(json!({"name":"docs","dimensions":2,
        "settings":{"text_fields":["title"],"metadata_indexes":[{"field":"lang","kind":"eq"}]}})),
    )
    .await;
    let docs = [
        (1, [1.0, 0.0], "rust vector database", "en"),
        (2, [0.0, 1.0], "python vector search", "en"),
        (3, [0.9, 0.1], "bonjour le monde", "fr"),
    ];
    for (id, vector, title, lang) in docs {
        ok(&app, "PUT", &format!("/v1/collections/docs/vectors/{id}"), Some(json!({
            "vector": vector,
            "metadata": {"title":{"type":"string","value":title},"lang":{"type":"string","value":lang}}
        }))).await;
    }
    // Auto: planner chooses a strategy and explains it.
    let auto = ok(
        &app,
        "POST",
        "/v1/collections/docs/search",
        Some(json!({"vector":[1,0],"k":2,
        "mode":"auto","explain":true,
        "filter":[{"field":"lang","op":"eq","value":{"type":"string","value":"fr"}}]})),
    )
    .await;
    assert_eq!(auto["hits"][0]["id"], "3", "{auto}");
    assert_eq!(auto["hits"].as_array().unwrap().len(), 1);
    assert!(
        ["exact_scan", "exact_indexed", "filtered_ann"]
            .contains(&auto["plan"]["strategy"].as_str().unwrap()),
        "{auto}"
    );
    assert!(auto["plan"]["reason"].is_string());
    assert!(auto["stats"]["distance_evaluations"].is_u64());
    // Without explain there is no plan/stats.
    let plain = ok(
        &app,
        "POST",
        "/v1/collections/docs/search",
        Some(json!({"vector":[1,0],"k":1,"mode":"auto"})),
    )
    .await;
    assert!(plain.get("plan").is_none() && plain.get("stats").is_none());
    // Hybrid: text and vector legs fused with RRF, per-leg ranks reported.
    let hybrid = ok(&app, "POST", "/v1/collections/docs/search", Some(json!({"vector":[0,1],"text":"python",
        "k":3,"mode":"hybrid","explain":true,"fusion":{"rrf_k":60,"vector_weight":1,"text_weight":1}}))).await;
    let top = &hybrid["hits"][0];
    assert_eq!(top["id"], "2", "{hybrid}");
    assert_eq!(top["vector_rank"], 1);
    assert_eq!(top["text_rank"], 1);
    assert!(top["bm25"].as_f64().unwrap() > 0.0);
    assert!(top["score"].as_f64().unwrap() > hybrid["hits"][1]["score"].as_f64().unwrap());
    // Text-only hybrid.
    let text = ok(
        &app,
        "POST",
        "/v1/collections/docs/search",
        Some(json!({"text":"rust","k":3,"mode":"hybrid"})),
    )
    .await;
    assert_eq!(text["hits"].as_array().unwrap().len(), 1);
    assert_eq!(text["hits"][0]["id"], "1");
    assert!(text["hits"][0]["vector_rank"].is_null());
    // Invalid combinations are explicit 422s.
    for body in [
        json!({"vector":[1,0],"k":1,"text":"rust"}),
        json!({"vector":[1,0],"k":1,"mode":"exact","fusion":{"rrf_k":1}}),
        json!({"k":1,"mode":"auto"}),
        json!({"vector":[1,0],"k":1,"mode":"fuzzy"}),
    ] {
        let (status, _, error) = call(
            &app,
            None,
            "POST",
            "/v1/collections/docs/search",
            Some(body.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body} {error}");
    }
    // Hybrid on the legacy default collection without text fields fails explicitly.
    let (status, _, error) = call(
        &app,
        None,
        "POST",
        "/v1/search",
        Some(json!({"text":"rust","k":1,"mode":"hybrid"})),
    )
    .await;
    assert!(status.is_client_error(), "{error}");
    service.close().await.unwrap();
}

#[tokio::test]
async fn reserved_imports_commit_in_order_and_never_strand_an_earlier_job() {
    let (_dir, service, app) = v4(2);
    ok(
        &app,
        "POST",
        "/v1/collections",
        Some(json!({"name":"bulk","dimensions":2})),
    )
    .await;
    let reserve = || async {
        ok(&app, "POST", "/v1/collections/bulk/imports", None).await["job"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let (first, second) = (reserve().await, reserve().await);
    let chunk = |id: u64| json!({"operations":[put_op(id, json!([1,1]))]});
    // The later job cannot commit first: explicit 409, nothing written.
    let (status, _, error) = call(
        &app,
        None,
        "PUT",
        &format!("/v1/imports/{second}/chunks/1"),
        Some(chunk(20)),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(error["error"]["code"], "import_conflict");
    assert!(error["error"]["message"].as_str().unwrap().contains(&first));
    assert_eq!(
        ok(&app, "GET", "/v1/collections/bulk", None).await["live_records"],
        0
    );
    // After the earlier job commits, both proceed and finish.
    ok(
        &app,
        "PUT",
        &format!("/v1/imports/{first}/chunks/1"),
        Some(chunk(10)),
    )
    .await;
    ok(
        &app,
        "PUT",
        &format!("/v1/imports/{second}/chunks/1"),
        Some(chunk(20)),
    )
    .await;
    for job in [&first, &second] {
        let done = ok(&app, "POST", &format!("/v1/imports/{job}/finish"), None).await;
        assert_eq!(done["state"], "finished", "{done}");
    }
    // Aborting an earlier reservation unblocks the later one.
    let (third, fourth) = (reserve().await, reserve().await);
    ok(&app, "POST", &format!("/v1/imports/{third}/abort"), None).await;
    ok(
        &app,
        "PUT",
        &format!("/v1/imports/{fourth}/chunks/1"),
        Some(chunk(40)),
    )
    .await;
    ok(&app, "POST", &format!("/v1/imports/{fourth}/finish"), None).await;
    // A reservation whose collection was dropped never blocks.
    ok(
        &app,
        "POST",
        "/v1/collections",
        Some(json!({"name":"gone","dimensions":2})),
    )
    .await;
    let orphan = ok(&app, "POST", "/v1/collections/gone/imports", None).await["job"]
        .as_str()
        .unwrap()
        .to_owned();
    ok(&app, "DELETE", "/v1/collections/gone", None).await;
    let fifth = reserve().await;
    assert!(fifth.parse::<u64>().unwrap() > orphan.parse::<u64>().unwrap());
    ok(
        &app,
        "PUT",
        &format!("/v1/imports/{fifth}/chunks/1"),
        Some(chunk(50)),
    )
    .await;
    assert_eq!(
        ok(&app, "GET", "/v1/collections/bulk", None).await["live_records"],
        4
    );
    service.close().await.unwrap();
}
