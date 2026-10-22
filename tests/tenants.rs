#![cfg(feature = "server")]
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use vlite::{
    Database, FormatVersion,
    server::{BearerToken, ServerOptions, Service, configured_admin_router},
};
const ADMIN: &str = "admin_secret_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
async fn call(
    app: &Router,
    token: &str,
    method: &str,
    path: &str,
    input: Value,
) -> (StatusCode, Value, bool) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(if input.is_null() {
                    Body::empty()
                } else {
                    Body::from(input.to_string())
                })
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let retry = response.headers().contains_key("retry-after");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        retry,
    )
}
fn options(path: &std::path::Path) -> ServerOptions {
    ServerOptions {
        admin_token: Some(BearerToken::new(ADMIN).unwrap()),
        admin_directory: Some(path.into()),
        ..ServerOptions::default()
    }
}
async fn credential(app: &Router, extra: Value) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let mut body = json!({"name":"test", "collections":["*"], "operations":["read","write","collections","import","diagnostics"], "expires_unix_ms":now+3600000});
    for (key, value) in extra.as_object().unwrap() {
        body[key] = value.clone();
    }
    let (status, value, _) = call(app, ADMIN, "POST", "/v1/admin/credentials", body).await;
    assert_eq!(status, StatusCode::CREATED, "{value}");
    value["token"].as_str().unwrap().into()
}

#[tokio::test]
async fn tenant_namespaces_quotas_persistence_and_delete_fail_closed() {
    let dir = tempfile::tempdir_in(".").unwrap();
    let path = dir.path().join("data.vlite");
    let service = Service::new(Database::create_with_format(&path, 2, FormatVersion::V4).unwrap());
    let app = configured_admin_router(service.clone(), options(dir.path())).unwrap();
    let (_, a, _) = call(
        &app,
        ADMIN,
        "POST",
        "/v1/admin/tenants",
        json!({"name":"a", "max_vectors":1}),
    )
    .await;
    let (_, b, _) = call(
        &app,
        ADMIN,
        "POST",
        "/v1/admin/tenants",
        json!({"name":"b"}),
    )
    .await;
    let aid = a["tenant"]["id"].as_str().unwrap();
    let bid = b["tenant"]["id"].as_str().unwrap();
    let at = credential(&app, json!({"tenant_id":aid})).await;
    let bt = credential(&app, json!({"tenant_id":bid})).await;
    for token in [&at, &bt] {
        let (status, value, _) = call(
            &app,
            token,
            "POST",
            "/v1/collections",
            json!({"name":"docs","dimensions":2}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{value}");
        assert_eq!(value["name"], "docs");
    }
    let put = json!({"vector":[1.0,0.0]});
    assert_eq!(
        call(
            &app,
            &at,
            "PUT",
            "/v1/collections/docs/vectors/1",
            put.clone()
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(
            &app,
            &bt,
            "GET",
            "/v1/collections/docs/vectors/1",
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (status, value, retry) = call(
        &app,
        &at,
        "PUT",
        "/v1/collections/docs/vectors/2",
        put.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{value}");
    assert!(retry);
    assert_eq!(value["error"]["code"], "quota_exceeded");
    let shared = credential(&app, json!({"tenant_id":aid})).await;
    assert_eq!(
        call(
            &app,
            &shared,
            "PUT",
            "/v1/collections/docs/vectors/2",
            put.clone()
        )
        .await
        .0,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        call(
            &app,
            ADMIN,
            "PUT",
            &format!("/v1/collections/t{aid}__docs/vectors/2"),
            put.clone()
        )
        .await
        .0,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        call(
            &app,
            &at,
            "PUT",
            "/v1/collections/docs/settings",
            json!({"metric":"cosine"})
        )
        .await
        .0,
        StatusCode::OK
    );

    assert_eq!(
        call(&app, &at, "GET", "/v1/vectors/1", Value::Null).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &app,
            &at,
            "GET",
            &format!("/v1/collections/t{bid}__docs"),
            Value::Null
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let (_, listing, _) = call(&app, &at, "GET", "/v1/collections", Value::Null).await;
    assert_eq!(listing["items"].as_array().unwrap().len(), 1);
    assert_eq!(listing["items"][0]["name"], "docs");
    assert_eq!(
        call(
            &app,
            ADMIN,
            "DELETE",
            &format!("/v1/admin/tenants/{aid}"),
            Value::Null
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        call(
            &app,
            &at,
            "DELETE",
            "/v1/collections/docs/vectors/1",
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, &at, "DELETE", "/v1/collections/docs", Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &app,
            ADMIN,
            "DELETE",
            &format!("/v1/admin/tenants/{aid}"),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, &at, "GET", "/v1/collections", Value::Null)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    drop(app);
    service.close().await.unwrap();
    let service = Service::new(Database::open(&path).unwrap());
    let app = configured_admin_router(service.clone(), options(dir.path())).unwrap();
    assert_eq!(
        call(&app, &at, "GET", "/v1/collections", Value::Null)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&app, &bt, "GET", "/v1/collections/docs", Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    service.close().await.unwrap();
}

#[tokio::test]
async fn credential_rate_limits_and_atomic_vector_quotas() {
    let dir = tempfile::tempdir_in(".").unwrap();
    let path = dir.path().join("data.vlite");
    let service = Service::new(Database::create_with_format(&path, 2, FormatVersion::V4).unwrap());
    let app = configured_admin_router(service.clone(), options(dir.path())).unwrap();
    let token = credential(&app, json!({"requests_per_second":1})).await;
    assert_eq!(
        call(&app, &token, "GET", "/v1/collections", Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    let (status, value, retry) = call(&app, &token, "GET", "/v1/collections", Value::Null).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(value["error"]["code"], "rate_limited");
    assert!(retry);
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    assert_eq!(
        call(&app, &token, "GET", "/v1/collections", Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    let quota = credential(
        &app,
        json!({"max_vectors":1,"max_vectors_per_collection":1}),
    )
    .await;
    let body = json!({"operations":[{"op":"put","id":"1","vector":[1.0,0.0]},{"op":"put","id":"2","vector":[1.0,0.0]}]});
    let (status, value, _) = call(&app, &quota, "POST", "/v1/batches", body).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{value}");
    assert_eq!(
        call(&app, &quota, "GET", "/v1/vectors/1", Value::Null)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &app,
            &quota,
            "PUT",
            "/v1/vectors/1",
            json!({"vector":[1.0,0.0]})
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(
            &app,
            &quota,
            "PUT",
            "/v1/vectors/1",
            json!({"vector":[0.0,1.0]})
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let (_, metrics, _) = call(&app, ADMIN, "GET", "/metrics", Value::Null).await;
    assert_eq!(metrics["rate_limited_total"], 1);
    assert_eq!(metrics["quota_exceeded_total"], 1);
    let (status, reservation, _) = call(
        &app,
        &quota,
        "POST",
        "/v1/collections/default/imports",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{reservation}");
    let job = reservation["job"].as_str().unwrap();
    let chunk = json!({"operations":[{"op":"put","id":"2","vector":[1.0,0.0]}]});
    let (status, value, _) = call(
        &app,
        &quota,
        "PUT",
        &format!("/v1/imports/{job}/chunks/1"),
        chunk,
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{value}");
    assert_eq!(
        call(&app, &quota, "GET", "/v1/vectors/2", Value::Null)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    drop(app);
    service.close().await.unwrap();
    let service = Service::new(Database::open(&path).unwrap());
    let app = configured_admin_router(service.clone(), options(dir.path())).unwrap();
    assert_eq!(
        call(
            &app,
            &quota,
            "PUT",
            "/v1/vectors/2",
            json!({"vector":[1.0,0.0]})
        )
        .await
        .0,
        StatusCode::TOO_MANY_REQUESTS
    );
    service.close().await.unwrap();
}
