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

async fn request(app: &Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    raw(app, method, path, &body.to_string(), true).await
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
async fn crud_search_batch_and_reopen_preserve_binary_payloads_and_full_u64_ids() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("api.vlite");
    let service = Service::new(Database::create(&path, 2).unwrap());
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
        json!({"dimensions":2,"format_version":2,"records":2})
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
    Database::create(&path, 2).unwrap().close().unwrap();
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
    let service = Service::new(Database::create(&path, 2).unwrap());
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
    let service = Service::new(Database::create(directory.path().join("api.vlite"), 1).unwrap());
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
    let service = Service::new(Database::create(&path, 1).unwrap());
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
async fn parallel_mutations_are_serialized_without_lost_writes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("api.vlite");
    let service = Service::new(Database::create(&path, 1).unwrap());
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
