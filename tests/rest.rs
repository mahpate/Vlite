use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use vlite::{
    Database,
    server::{Service, router},
};

#[tokio::test]
async fn snapshot_and_lock_held_exact_search_are_identical_and_policy_precedes_cloning() {
    fn populated_database() -> Database {
        let mut db = Database::create_in_memory(2, Default::default()).unwrap();
        db.put(9, &[1.0, 0.0], b"first").unwrap();
        db.put(3, &[0.0, 1.0], b"second").unwrap();
        db.put(5, &[-1.0, 0.0], b"third").unwrap();
        db
    }

    let snapshot_service = Service::new(populated_database())
        .with_snapshot_reads(true)
        .unwrap();
    let lock_held_service = Service::new(populated_database())
        .with_snapshot_reads(false)
        .unwrap();
    let snapshot_app = router(snapshot_service.clone());
    let lock_held_app = router(lock_held_service.clone());
    let search_body = json!({"vector":[0.75,0.25],"k":3,"metric":"cosine"});
    let (snapshot_status, snapshot_response) =
        request(&snapshot_app, "POST", "/v1/search", search_body.clone()).await;
    let (lock_held_status, lock_held_response) =
        request(&lock_held_app, "POST", "/v1/search", search_body).await;
    assert_eq!(snapshot_status, StatusCode::OK);
    assert_eq!(lock_held_status, StatusCode::OK);
    assert_eq!(snapshot_response, lock_held_response);

    let shared = Service::new(populated_database());
    let clone = shared.clone();
    assert!(
        clone.with_snapshot_reads(false).is_err(),
        "read policy must be finalized before the service is cloned"
    );
}

async fn request(app: &Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    raw(app, method, path, &body.to_string(), true).await
}

#[tokio::test]
async fn exact_metrics_and_unsupported_ann_combinations_are_explicit() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = Database::create_with_format(
        directory.path().join("metrics.vlite"),
        2,
        vlite::FormatVersion::V3,
    )
    .unwrap();
    db.put(1, &[1.0, 0.0], b"").unwrap();
    db.put(2, &[4.0, 3.0], b"").unwrap();
    let app = router(Service::new(db));
    for (metric, id) in [("cosine", "1"), ("inner_product", "2"), ("squared_l2", "1")] {
        let (status, result) = request(
            &app,
            "POST",
            "/v1/search",
            json!({"vector":[1,0],"k":2,"metric":metric}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(result["hits"][0]["id"], id);
    }
    for (body, code) in [
        (
            json!({"vector":[0,0],"k":0,"metric":"cosine"}),
            "invalid_input",
        ),
        (
            json!({"vector":[1,0],"k":1,"metric":"unknown"}),
            "invalid_json",
        ),
        (
            json!({"vector":[1,0],"k":1,"metric":"inner_product","mode":"approximate"}),
            "invalid_input",
        ),
        (
            json!({"vector":[1,0],"k":1,"metric":"inner_product","mode":"approximate_ready"}),
            "invalid_input",
        ),
    ] {
        let (status, result) = request(&app, "POST", "/v1/search", body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(result["error"]["code"], code);
    }
    let (_, status) = request(&app, "GET", "/v1/index/hnsw", Value::Null).await;
    assert_eq!(status["metric"], Value::Null);
    assert_eq!(
        request(&app, "POST", "/v1/index/hnsw", json!({"metric":"cosine"}))
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    let (_, status) = request(&app, "GET", "/v1/index/hnsw", Value::Null).await;
    assert_eq!(status["metric"], "cosine");
    assert_eq!(status["ready"], true);
    let (code, hits) = request(
        &app,
        "POST",
        "/v1/search",
        json!({"vector":[1,0],"k":2,"metric":"cosine","mode":"approximate_ready"}),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(hits["hits"][0], json!({"id":"1","distance":0.0}));
    assert_eq!(
        request(
            &app,
            "POST",
            "/v1/search",
            json!({"vector":[1,0],"k":1,"mode":"approximate_ready"})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/v1/index/hnsw",
            json!({"metric":"inner_product"})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let (_, status) = request(&app, "GET", "/v1/index/hnsw", Value::Null).await;
    assert_eq!(
        status["metric"], "cosine",
        "rejected build must preserve index"
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/v1/search",
            json!({"vector":[1,0],"k":0,"mode":"approximate"})
        )
        .await
        .0,
        StatusCode::OK
    );
    let (_, status) = request(&app, "GET", "/v1/index/hnsw", Value::Null).await;
    assert_eq!(
        status["metric"], "cosine",
        "k=0 must not replace mismatched index"
    );
    let (_, metrics) = request(&app, "GET", "/metrics", Value::Null).await;
    assert_eq!(metrics["hnsw_rebuilds_total"], 1);
    assert_eq!(
        request(
            &app,
            "POST",
            "/v1/search",
            json!({"vector":[1,0],"k":1,"mode":"approximate"})
        )
        .await
        .0,
        StatusCode::OK
    );
    let (_, status) = request(&app, "GET", "/v1/index/hnsw", Value::Null).await;
    assert_eq!(status["metric"], "squared_l2");
    let (_, metrics) = request(&app, "GET", "/metrics", Value::Null).await;
    assert_eq!(metrics["hnsw_rebuilds_total"], 2);
}

async fn raw(
    app: &Router,
    method: &str,
    path: &str,
    body: &str,
    content_type: bool,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(path);
    if content_type {
        builder = builder.header("content-type", "application/json");
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if status == StatusCode::NO_CONTENT {
        assert!(bytes.is_empty());
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

#[tokio::test]
async fn authentication_metrics_and_request_ids_are_enforced_without_mutations() {
    use vlite::server::{BearerToken, authenticated_router};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("auth.vlite");
    let service =
        Service::new(Database::create_with_format(&path, 1, vlite::FormatVersion::V3).unwrap());
    let token = "test_secret_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let app = authenticated_router(service.clone(), BearerToken::new(token).unwrap());
    for value in ["", "Basic abc", "Bearer wrong", &format!("Bearer {token}x")] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/v1/vectors/1")
                    .header("authorization", value)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"vector":[1]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
        assert_eq!(response.headers()["www-authenticate"], "Bearer");
        assert!(response.headers().contains_key("x-request-id"));
    }
    assert_eq!(raw(&app, "GET", "/health", "", false).await.0, 200);
    assert_eq!(raw(&app, "GET", "/openapi.json", "", false).await.0, 401);
    let duplicate = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/database")
                .header("authorization", format!("Bearer {token}"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(duplicate.status(), 401);
    let mut ids = std::collections::BTreeSet::new();
    for uri in ["/ready", "/metrics"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("authorization", format!("Bearer {token}"))
                    .header("x-request-id", "untrusted-client-id")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let id = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        assert_ne!(id, "untrusted-client-id");
        assert!(ids.insert(id));
        if uri == "/metrics" {
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let value: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["unauthorized_total"], 6);
            assert_eq!(value["live_records"], 0);
            assert_eq!(value["database_bytes"], 32);
            assert_eq!(value["commit_failures_total"], 0);
        }
    }
    service.close().await.unwrap();
    assert_eq!(std::fs::metadata(path).unwrap().len(), 32);
    for value in [
        "short",
        &"x".repeat(257),
        &format!("{} ", "x".repeat(32)),
        &"é".repeat(32),
    ] {
        assert!(BearerToken::new(value).is_err());
    }
}

#[tokio::test]
async fn crud_search_batch_and_reopen_preserve_binary_payloads_and_full_u64_ids() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("api.vlite");
    let service =
        Service::new(Database::create_with_format(&path, 2, vlite::FormatVersion::V3).unwrap());
    let app = router(service.clone());
    assert_eq!(
        request(&app, "GET", "/health", Value::Null).await.1["status"],
        "ok"
    );
    assert_eq!(request(&app, "GET", "/ready", Value::Null).await.0, 200);
    let record = json!({"vector":[1.0,0.0],"payload_base64":"AP+A"});
    assert_eq!(
        request(&app, "PUT", "/v1/vectors/18446744073709551615", record)
            .await
            .0,
        204
    );
    let fetched = request(&app, "GET", "/v1/vectors/18446744073709551615", Value::Null).await;
    assert_eq!(fetched.1["id"], "18446744073709551615");
    assert_eq!(fetched.1["payload_base64"], "AP+A");
    assert_eq!(
        request(
            &app,
            "POST",
            "/v1/batches",
            json!({"operations":[
                {"op":"put","id":"0","vector":[0,1]},
                {"op":"put","id":"7","vector":[2,0],"payload_base64":"YQ=="},
                {"op":"put","id":"7","vector":[0,1],"payload_base64":"Yg=="},
                {"op":"delete","id":"99"}
            ]})
        )
        .await
        .0,
        204
    );
    let result = request(&app, "POST", "/v1/search", json!({"vector":[0,1],"k":10})).await;
    assert_eq!(result.0, 200);
    assert_eq!(
        result.1["hits"],
        json!([
            {"id":"0","distance":0.0},
            {"id":"7","distance":0.0},
            {"id":"18446744073709551615","distance":2.0}
        ])
    );
    assert_eq!(
        request(&app, "DELETE", "/v1/vectors/0", Value::Null)
            .await
            .1["deleted"],
        true
    );
    assert_eq!(
        request(&app, "DELETE", "/v1/vectors/0", Value::Null)
            .await
            .1["deleted"],
        false
    );
    assert_eq!(
        request(&app, "GET", "/v1/vectors/0", Value::Null).await.0,
        404
    );
    assert_eq!(
        request(&app, "GET", "/v1/database", Value::Null).await.1,
        json!({"dimensions":2,"format_version":3,"records":2,"is_encrypted":false,"encryption_key_id":null})
    );
    service.close().await.unwrap();
    assert_eq!(request(&app, "GET", "/health", Value::Null).await.0, 200);
    assert_eq!(request(&app, "GET", "/ready", Value::Null).await.0, 503);
    let db = Database::open(&path).unwrap();
    assert_eq!(db.get(7).unwrap().unwrap().payload, b"b");
    assert_eq!(db.get(u64::MAX).unwrap().unwrap().payload, [0, 255, 128]);
    assert_eq!(db.len(), 2);
}

#[tokio::test]
async fn invalid_batch_is_atomic_and_errors_are_structured() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("api.vlite");
    Database::create_with_format(&path, 2, vlite::FormatVersion::V3)
        .unwrap()
        .close()
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    let service = Service::new(Database::open(&path).unwrap());
    let app = router(service.clone());
    for operations in [
        json!([{"op":"put","id":"1","vector":[1,2]},{"op":"put","id":"2","vector":[1]}]),
        json!([{"op":"put","id":"1","vector":[1,2]},{"op":"put","id":"2","vector":[1,2],"payload_base64":"!"}]),
        json!([{"op":"put","id":"1","vector":[1,2]},{"op":"delete","id":"01"}]),
        json!([{"op":"put","id":"1","vector":[1,2]},{"op":"delete","id":2}]),
    ] {
        let (status, error) = request(
            &app,
            "POST",
            "/v1/batches",
            json!({"operations":operations}),
        )
        .await;
        assert!(status.is_client_error(), "{error}");
        assert!(error["error"]["code"].is_string());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), before.len() as u64);
        assert_eq!(
            request(&app, "GET", "/v1/database", Value::Null).await.1["records"],
            0
        );
    }
    for id in ["01", "-1", "18446744073709551616", "%FF", "+2"] {
        let (status, error) = request(&app, "GET", &format!("/v1/vectors/{id}"), Value::Null).await;
        assert_eq!(status, 400, "{id}: {error}");
        assert_eq!(error["error"]["code"], "invalid_id");
    }
    assert_eq!(
        request(&app, "POST", "/v1/batches", json!({"operations":[]}))
            .await
            .0,
        204
    );
    service.close().await.unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[tokio::test]
async fn json_transport_and_search_limits_fail_without_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("api.vlite");
    let service =
        Service::new(Database::create_with_format(&path, 2, vlite::FormatVersion::V3).unwrap());
    let app = router(service.clone());
    for (method, path, text, content_type, expected) in [
        ("PUT", "/v1/vectors/1", "{", true, 400),
        ("PUT", "/v1/vectors/1", "{}", false, 415),
        (
            "PUT",
            "/v1/vectors/1",
            r#"{"vector":[1,2],"typo":1}"#,
            true,
            422,
        ),
        ("PUT", "/v1/vectors/1", r#"{"vector":[1e100,2]}"#, true, 422),
        ("PUT", "/v1/vectors/1", r#"{"vector":[NaN,2]}"#, true, 400),
        (
            "POST",
            "/v1/search",
            r#"{"vector":[1,2],"k":10001}"#,
            true,
            422,
        ),
        ("POST", "/v1/search", r#"{"vector":[1],"k":0}"#, true, 422),
        (
            "POST",
            "/v1/search",
            r#"{"vector":[1,2],"k":-1}"#,
            true,
            422,
        ),
        ("GET", "/unknown", "", true, 404),
        ("POST", "/health", "", true, 405),
    ] {
        let (status, error) = raw(&app, method, path, text, content_type).await;
        assert_eq!(status.as_u16(), expected, "{text}: {error}");
        assert!(error["error"]["code"].is_string());
    }
    let oversized = format!(
        r#"{{"vector":[1,2],"payload_base64":"{}"}}"#,
        "A".repeat(vlite::server::MAX_JSON_BYTES)
    );
    assert_eq!(
        raw(&app, "PUT", "/v1/vectors/1", &oversized, true).await.0,
        413
    );
    let operations = vec![json!({"op":"delete","id":"0"}); 4097];
    assert_eq!(
        request(
            &app,
            "POST",
            "/v1/batches",
            json!({"operations":operations})
        )
        .await
        .0,
        422
    );
    assert_eq!(
        request(&app, "POST", "/v1/search", json!({"vector":[1,2],"k":0}))
            .await
            .1,
        json!({"hits":[]})
    );
    service.close().await.unwrap();
    assert!(Database::open(&path).unwrap().is_empty());
}

#[tokio::test]
async fn contract_is_served_as_json_and_covers_routes_and_limits() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(
        Database::create_with_format(
            directory.path().join("api.vlite"),
            1,
            vlite::FormatVersion::V3,
        )
        .unwrap(),
    );
    let app = router(service.clone());
    let (status, contract) = request(&app, "GET", "/openapi.json", Value::Null).await;
    assert_eq!(status, 200);
    assert_eq!(contract["openapi"], "3.1.0");
    fn check_references(value: &Value, contract: &Value) {
        match value {
            Value::Object(fields) => {
                if let Some(Value::String(reference)) = fields.get("$ref") {
                    assert!(
                        contract
                            .pointer(reference.strip_prefix('#').unwrap())
                            .is_some(),
                        "{reference}"
                    );
                }
                for child in fields.values() {
                    check_references(child, contract);
                }
            }
            Value::Array(values) => {
                for child in values {
                    check_references(child, contract);
                }
            }
            _ => {}
        }
    }
    check_references(&contract, &contract);
    for route in [
        "/health",
        "/ready",
        "/v1/database",
        "/v1/vectors/{id}",
        "/v1/batches",
        "/v1/search",
        "/openapi.json",
    ] {
        assert!(contract["paths"][route].is_object(), "{route}");
    }
    service.close().await.unwrap();
}

#[tokio::test]
async fn legacy_files_require_explicit_upgrade_for_batches_but_allow_single_writes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("v1.vlite");
    let hex = include_str!("fixtures/v1.hex")
        .split_whitespace()
        .collect::<String>();
    let bytes = hex
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect::<Vec<_>>();
    std::fs::write(&path, &bytes).unwrap();
    let service = Service::new(Database::open(&path).unwrap());
    let app = router(service.clone());
    let (status, error) = request(
        &app,
        "POST",
        "/v1/batches",
        json!({"operations":[{"op":"delete","id":"9"}]}),
    )
    .await;
    assert_eq!(status, 409);
    assert_eq!(error["error"]["code"], "upgrade_required");
    assert_eq!(
        request(&app, "GET", "/v1/vectors/9", Value::Null).await.0,
        200
    );
    assert_eq!(
        request(&app, "POST", "/v1/batches", json!({"operations":[]}))
            .await
            .0,
        204
    );
    assert_eq!(
        request(&app, "PUT", "/v1/vectors/0", json!({"vector":[1,2]}))
            .await
            .0,
        204
    );
    service.close().await.unwrap();
    let db = Database::open(path).unwrap();
    assert_eq!(db.format_version(), 1);
    assert_eq!(db.len(), 2);
}

#[tokio::test]
async fn payload_limits_and_canonical_base64_are_enforced() {
    use base64::{Engine, prelude::BASE64_STANDARD};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("payload.vlite");
    let service =
        Service::new(Database::create_with_format(&path, 1, vlite::FormatVersion::V3).unwrap());
    let app = router(service.clone());
    let valid = BASE64_STANDARD.encode(vec![255; vlite::MAX_PAYLOAD_BYTES]);
    assert_eq!(
        request(
            &app,
            "PUT",
            "/v1/vectors/1",
            json!({"vector":[1],"payload_base64":valid})
        )
        .await
        .0,
        204
    );
    let over = BASE64_STANDARD.encode(vec![255; vlite::MAX_PAYLOAD_BYTES + 1]);
    assert_eq!(
        request(
            &app,
            "PUT",
            "/v1/vectors/1",
            json!({"vector":[1],"payload_base64":over})
        )
        .await
        .0,
        422
    );
    for encoded in ["YQ", "YR==", "YQ==\n", "_w=="] {
        assert_eq!(
            request(
                &app,
                "PUT",
                "/v1/vectors/1",
                json!({"vector":[1],"payload_base64":encoded})
            )
            .await
            .0,
            422,
            "{encoded}"
        );
    }
    service.close().await.unwrap();
    let db = Database::open(path).unwrap();
    assert_eq!(
        db.get(1).unwrap().unwrap().payload,
        vec![255; vlite::MAX_PAYLOAD_BYTES]
    );
}

#[tokio::test]
async fn typed_metadata_filters_and_atomic_rejection_are_exposed_over_rest() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("metadata.vlite");
    let service =
        Service::new(Database::create_with_format(&path, 1, vlite::FormatVersion::V3).unwrap());
    let app = router(service.clone());
    let metadata = json!({
        "category":{"type":"string","value":"docs"},
        "count":{"type":"integer","value":"9223372036854775807"},
        "score":{"type":"float","value":1.5},
        "active":{"type":"boolean","value":true}
    });
    assert_eq!(
        request(
            &app,
            "PUT",
            "/v1/vectors/7",
            json!({"vector":[7],"metadata":metadata})
        )
        .await
        .0,
        204
    );
    assert_eq!(
        request(&app, "PUT", "/v1/vectors/0", json!({"vector":[0]}))
            .await
            .0,
        204
    );
    assert_eq!(
        request(&app, "GET", "/v1/vectors/7", Value::Null).await.1["metadata"],
        metadata
    );
    let filter = json!([
        {"op":"eq","field":"category","value":{"type":"string","value":"docs"}},
        {"op":"range","field":"count","min":{"type":"integer","value":"9223372036854775807"}}
    ]);
    assert_eq!(
        request(
            &app,
            "POST",
            "/v1/search",
            json!({"vector":[0],"k":1,"filter":filter})
        )
        .await
        .1["hits"][0]["id"],
        "7"
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/v1/search",
            json!({"vector":[0],"k":1,"mode":"approximate","filter":filter})
        )
        .await
        .1["hits"],
        json!([{"id":"7","distance":49.0}])
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/v1/search",
            json!({"vector":[0],"k":2,"mode":"approximate_ready","ef_search":2,"filter":filter})
        )
        .await
        .1["hits"],
        json!([{"id":"7","distance":49.0}])
    );
    for (mode, body) in [
        ("approximate", json!([{"op":"range","field":"count"}])),
        (
            "approximate_ready",
            json!([{"op":"range","field":"count","min":{"type":"string","value":"a"}}]),
        ),
        (
            "approximate_ready",
            json!([{"op":"range","field":"count","min":{"type":"integer","value":"2"},"max":{"type":"integer","value":"1"}}]),
        ),
    ] {
        for k in [0, 1] {
            let (status, error) = request(
                &app,
                "POST",
                "/v1/search",
                json!({"vector":[0],"k":k,"mode":mode,"filter":body}),
            )
            .await;
            assert_eq!(status, 422, "{error}");
            assert_eq!(error["error"]["code"], "invalid_input");
        }
    }
    assert_eq!(
        request(
            &app,
            "POST",
            "/v1/search",
            json!({"vector":[0],"k":1,"mode":"approximate_ready","filter":[{"op":"eq","field":"category","value":{"type":"string","value":"none"}}]})
        )
        .await
        .1["hits"],
        json!([])
    );
    for metadata in [
        json!({"x":{"type":"integer","value":9007199254740993u64}}),
        json!({"x":{"type":"integer","value":"01"}}),
        json!({"x":{"type":"integer","value":"9223372036854775808"}}),
        json!({"x":{"type":"string","value":["a"]}}),
        json!({"x":{"type":"unknown","value":true}}),
        json!({"x":{"type":"boolean","value":true,"typo":1}}),
        json!({"":{"type":"boolean","value":true}}),
    ] {
        let (status, error) = request(
            &app,
            "POST",
            "/v1/batches",
            json!({"operations":[
                {"op":"delete","id":"7"},
                {"op":"put","id":"8","vector":[8],"metadata":metadata}
            ]}),
        )
        .await;
        assert!(status.is_client_error(), "{error}");
        assert_eq!(
            request(&app, "GET", "/v1/vectors/7", Value::Null).await.0,
            200
        );
    }
    assert_eq!(
        request(
            &app,
            "POST",
            "/v1/batches",
            json!({"operations":[
                {"op":"put","id":"8","vector":[8],"metadata":metadata},
                {"op":"delete","id":"7"}
            ]})
        )
        .await
        .0,
        204
    );
    service.close().await.unwrap();
    let service = Service::new(Database::open(&path).unwrap());
    let app = router(service.clone());
    assert_eq!(
        request(&app, "GET", "/v1/vectors/8", Value::Null).await.1["metadata"],
        metadata
    );
    assert_eq!(
        request(&app, "PUT", "/v1/vectors/8", json!({"vector":[8]}))
            .await
            .0,
        204
    );
    assert_eq!(
        request(&app, "GET", "/v1/vectors/8", Value::Null).await.1["metadata"],
        json!({})
    );
    service.close().await.unwrap();
}

#[tokio::test]
async fn metadata_requires_explicit_upgrade_over_rest_without_changing_legacy_files() {
    let directory = tempfile::tempdir().unwrap();
    for (version, hex) in [
        (1, include_str!("fixtures/v1.hex")),
        (2, include_str!("fixtures/v2.hex")),
    ] {
        let path = directory.path().join(format!("v{version}.vlite"));
        let hex = hex.split_whitespace().collect::<String>();
        let bytes = hex
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect::<Vec<_>>();
        std::fs::write(&path, &bytes).unwrap();
        let service = Service::new(Database::open(&path).unwrap());
        let app = router(service.clone());
        let metadata = json!({"category":{"type":"string","value":"docs"}});
        for (route, input) in [
            ("/v1/vectors/9", json!({"vector":[1,2],"metadata":metadata})),
            (
                "/v1/batches",
                json!({"operations":[
                    {"op":"delete","id":"9"},
                    {"op":"put","id":"7","vector":[1,2],"metadata":metadata}
                ]}),
            ),
        ] {
            let method = if route == "/v1/batches" {
                "POST"
            } else {
                "PUT"
            };
            let (status, error) = request(&app, method, route, input).await;
            assert_eq!(status, 409);
            assert_eq!(error["error"]["code"], "upgrade_required");
            assert_eq!(
                request(&app, "GET", "/v1/vectors/9", Value::Null).await.1["metadata"],
                json!({})
            );
        }
        service.close().await.unwrap();
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
}

#[tokio::test]
async fn parallel_mutations_are_serialized_without_lost_writes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("api.vlite");
    let service =
        Service::new(Database::create_with_format(&path, 1, vlite::FormatVersion::V3).unwrap());
    let app = router(service.clone());
    let mut tasks = Vec::new();
    for id in 0..8 {
        let app = app.clone();
        tasks.push(tokio::spawn(async move {
            request(
                &app,
                "PUT",
                &format!("/v1/vectors/{id}"),
                json!({"vector":[id as f32]}),
            )
            .await
            .0
        }));
    }

    for task in tasks {
        assert_eq!(task.await.unwrap(), 204);
    }
    service.close().await.unwrap();
    assert_eq!(Database::open(path).unwrap().len(), 8);
}

#[tokio::test]
async fn approximate_mode_is_explicit_and_merges_mutations() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(
        Database::create_with_format(
            directory.path().join("ann.vlite"),
            2,
            vlite::FormatVersion::V3,
        )
        .unwrap(),
    );
    let app = router(service.clone());
    for (id, vector) in [
        ("0", json!([0, 0])),
        ("1", json!([1, 1])),
        ("2", json!([2, 2])),
    ] {
        assert_eq!(
            request(
                &app,
                "PUT",
                &format!("/v1/vectors/{id}"),
                json!({"vector":vector})
            )
            .await
            .0,
            204
        );
    }
    let approximate = json!({"vector":[0,0],"k":2,"mode":"approximate","ef_search":10});
    assert_eq!(
        request(&app, "POST", "/v1/search", approximate.clone())
            .await
            .1,
        request(&app, "POST", "/v1/search", json!({"vector":[0,0],"k":2}))
            .await
            .1
    );
    assert_eq!(
        request(&app, "DELETE", "/v1/vectors/0", Value::Null)
            .await
            .0,
        200
    );
    assert_eq!(
        request(&app, "POST", "/v1/search", approximate.clone())
            .await
            .1["hits"][0]["id"],
        "1"
    );
    assert_eq!(
        request(&app, "PUT", "/v1/vectors/1", json!({"vector":[9,9]}))
            .await
            .0,
        204
    );
    assert_eq!(
        request(&app, "POST", "/v1/search", approximate.clone())
            .await
            .1["hits"][0]["id"],
        "2"
    );
    for input in [
        json!({"vector":[0,0],"k":2,"ef_search":10}),
        json!({"vector":[0,0],"k":2,"mode":"approximate","ef_search":1}),
        json!({"vector":[0,0],"k":2,"mode":"approximate","ef_search":100001}),
        // `auto` and `hybrid` are valid Wave2 modes; unknown names stay 422.
        json!({"vector":[0,0],"k":2,"mode":"fuzzy"}),
        json!({"vector":[0,0],"k":2,"mode":null}),
        json!({"vector":[0,0],"k":2,"mode":"approximate","m":4}),
    ] {
        let (status, error) = request(&app, "POST", "/v1/search", input).await;
        assert_eq!(status, 422, "{error}");
    }
    assert_eq!(
        request(
            &app,
            "POST",
            "/v1/search",
            json!({"vector":[0,0],"k":2,"mode":"approximate"})
        )
        .await
        .0,
        200
    );
    service.close().await.unwrap();
}
