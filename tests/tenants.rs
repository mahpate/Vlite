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
    server::{BearerToken, ServerOptions, Service, configured_admin_router, router},
};
const ADMIN: &str = "admin_secret_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const READ: &str = "read_secret_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
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

/// Creates a credential and returns `(id, token)`.
async fn issued(app: &Router, extra: Value) -> (String, String) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let mut body = json!({"name":"test", "collections":["*"], "operations":["read","write","collections","diagnostics"], "expires_unix_ms":now+3600000});
    for (key, value) in extra.as_object().unwrap() {
        body[key] = value.clone();
    }
    let (status, value, _) = call(app, ADMIN, "POST", "/v1/admin/credentials", body).await;
    assert_eq!(status, StatusCode::CREATED, "{value}");
    (
        value["credential"]["id"].as_str().unwrap().into(),
        value["token"].as_str().unwrap().into(),
    )
}

#[tokio::test]
async fn capabilities_identify_the_caller_without_exposing_secrets() {
    let dir = tempfile::tempdir_in(".").unwrap();
    let path = dir.path().join("data.vlite");
    let service = Service::new(Database::create_with_format(&path, 2, FormatVersion::V4).unwrap());
    let mut configured = options(dir.path());
    configured.read_only_token = Some(BearerToken::new(READ).unwrap());
    let app = configured_admin_router(service.clone(), configured).unwrap();

    let (status, caps, _) = call(&app, ADMIN, "GET", "/v1/capabilities", Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{caps}");
    assert_eq!(caps["identity"]["kind"], "super_admin");
    assert_eq!(caps["role"], "admin");
    let (_, caps, _) = call(&app, READ, "GET", "/v1/capabilities", Value::Null).await;
    assert_eq!(caps["identity"]["kind"], "read_only");
    assert_eq!(caps["identity"]["label"], "Read-only token");

    let (_, tenant, _) = call(
        &app,
        ADMIN,
        "POST",
        "/v1/admin/tenants",
        json!({"name":"Acme docs"}),
    )
    .await;
    let tid = tenant["tenant"]["id"].as_str().unwrap();
    let (cid, token) = issued(&app, json!({"name":"ingest", "tenant_id":tid})).await;
    let (status, caps, _) = call(&app, &token, "GET", "/v1/capabilities", Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{caps}");
    let identity = &caps["identity"];
    assert_eq!(identity["kind"], "scoped");
    assert_eq!(identity["credential_id"], cid.as_str());
    assert_eq!(identity["credential_name"], "ingest");
    assert_eq!(identity["tenant_id"], tid);
    assert_eq!(identity["tenant_name"], "Acme docs");
    let label = identity["label"].as_str().unwrap();
    assert!(
        label.contains("\"ingest\"") && label.contains("\"Acme docs\""),
        "{label}"
    );
    let text = caps.to_string();
    assert!(!text.contains(&token) && !text.contains(ADMIN) && !text.contains(READ));

    let (_, global) = issued(&app, json!({"name":"global"})).await;
    let (_, caps, _) = call(&app, &global, "GET", "/v1/capabilities", Value::Null).await;
    assert_eq!(caps["identity"]["kind"], "scoped");
    assert!(caps["identity"]["tenant_id"].is_null());
    assert!(
        !caps["identity"]["label"]
            .as_str()
            .unwrap()
            .contains("tenant")
    );
    drop(app);
    service.close().await.unwrap();

    let open = Service::new(Database::create(dir.path().join("open.vlite"), 2).unwrap());
    let app = router(open.clone());
    let (status, caps, _) = call(&app, "", "GET", "/v1/capabilities", Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{caps}");
    assert_eq!(caps["identity"]["kind"], "open");
    drop(app);
    open.close().await.unwrap();
}

#[tokio::test]
async fn tenant_lifecycle_usage_settings_suspend_resume_and_delete() {
    let dir = tempfile::tempdir_in(".").unwrap();
    let path = dir.path().join("data.vlite");
    let service = Service::new(Database::create_with_format(&path, 2, FormatVersion::V4).unwrap());
    let app = configured_admin_router(service.clone(), options(dir.path())).unwrap();
    let (status, created, _) = call(
        &app,
        ADMIN,
        "POST",
        "/v1/admin/tenants",
        json!({"name":"Team A", "max_vectors":3}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["tenant"]["state"], "active");
    let tid = created["tenant"]["id"].as_str().unwrap().to_owned();
    let base = format!("/v1/admin/tenants/{tid}");
    let token = credential(&app, json!({"tenant_id":tid})).await;
    let (revoked, _) = issued(&app, json!({"tenant_id":tid})).await;
    assert_eq!(
        call(
            &app,
            ADMIN,
            "POST",
            &format!("/v1/admin/credentials/{revoked}/revoke"),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    let (_, global) = issued(&app, json!({"name":"global"})).await;
    assert_eq!(
        call(
            &app,
            &token,
            "POST",
            "/v1/collections",
            json!({"name":"docs","dimensions":2})
        )
        .await
        .0,
        StatusCode::CREATED
    );
    for id in [1, 2] {
        assert_eq!(
            call(
                &app,
                &token,
                "PUT",
                &format!("/v1/collections/docs/vectors/{id}"),
                json!({"vector":[1.0,0.0]})
            )
            .await
            .0,
            StatusCode::NO_CONTENT
        );
    }

    let (status, usage, _) = call(&app, ADMIN, "GET", &format!("{base}/usage"), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{usage}");
    assert_eq!(usage["tenant"]["name"], "Team A");
    assert_eq!(usage["vectors"], 2);
    assert_eq!(usage["max_vectors"], 3);
    assert_eq!(usage["remaining_vectors"], 1);
    assert_eq!(usage["over_quota"], false);
    assert_eq!(usage["collections"][0]["name"], "docs");
    assert_eq!(
        usage["collections"][0]["physical_name"],
        format!("t{tid}__docs")
    );
    assert_eq!(usage["collections"][0]["vectors"], 2);
    assert_eq!(
        usage["credentials"],
        json!({"active":1,"expired":0,"revoked":1})
    );
    let (_, listing, _) = call(&app, ADMIN, "GET", "/v1/admin/tenants", Value::Null).await;
    let item = &listing["items"][0];
    assert_eq!(item["id"], tid.as_str());
    assert_eq!(item["state"], "active");
    assert_eq!(item["collections"], 1);
    assert_eq!(item["vectors"], 2);

    let (status, updated, _) = call(
        &app,
        ADMIN,
        "PUT",
        &format!("{base}/settings"),
        json!({"name":"Team Alpha","max_vectors":1}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["tenant"]["name"], "Team Alpha");
    assert_eq!(updated["tenant"]["max_vectors"], 1);
    let (_, usage, _) = call(&app, ADMIN, "GET", &format!("{base}/usage"), Value::Null).await;
    assert_eq!(usage["over_quota"], true);
    assert_eq!(usage["remaining_vectors"], 0);
    let (status, value, _) = call(
        &app,
        &token,
        "PUT",
        "/v1/collections/docs/vectors/3",
        json!({"vector":[0.0,1.0]}),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{value}");
    let (_, updated, _) = call(
        &app,
        ADMIN,
        "PUT",
        &format!("{base}/settings"),
        json!({"max_vectors":null}),
    )
    .await;
    assert!(updated["tenant"]["max_vectors"].is_null());
    assert_eq!(updated["tenant"]["name"], "Team Alpha");
    let (_, usage, _) = call(&app, ADMIN, "GET", &format!("{base}/usage"), Value::Null).await;
    assert!(usage["remaining_vectors"].is_null());
    assert_eq!(usage["over_quota"], false);
    for invalid in [
        json!({}),
        json!({"name":""}),
        json!({"name":"line\nbreak"}),
        json!({"name":"x".repeat(65)}),
        json!({"name":"ok","unknown":1}),
        json!({"max_vectors":-1}),
    ] {
        let (status, value, _) = call(
            &app,
            ADMIN,
            "PUT",
            &format!("{base}/settings"),
            invalid.clone(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{invalid} {value}"
        );
    }
    let missing = "0".repeat(32);
    for (method, suffix, input) in [
        ("GET", "usage", Value::Null),
        ("PUT", "settings", json!({"name":"x"})),
        ("POST", "suspend", Value::Null),
        ("POST", "resume", Value::Null),
    ] {
        let (status, value, _) = call(
            &app,
            ADMIN,
            method,
            &format!("/v1/admin/tenants/{missing}/{suffix}"),
            input,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{suffix} {value}");
        assert_eq!(value["error"]["code"], "tenant_not_found");
        for caller in [&token, &global] {
            assert_eq!(
                call(
                    &app,
                    caller,
                    method,
                    &format!("{base}/{suffix}"),
                    json!({"name":"x"})
                )
                .await
                .0,
                StatusCode::FORBIDDEN,
                "{suffix}"
            );
        }
    }

    let (status, suspended, _) =
        call(&app, ADMIN, "POST", &format!("{base}/suspend"), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{suspended}");
    assert_eq!(suspended["tenant"]["state"], "suspended");
    let since = suspended["tenant"]["suspended_unix_ms"].as_u64().unwrap();
    let (_, again, _) = call(&app, ADMIN, "POST", &format!("{base}/suspend"), Value::Null).await;
    assert_eq!(again["tenant"]["suspended_unix_ms"], since);
    let (status, value, _) = call(&app, &token, "GET", "/v1/collections", Value::Null).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{value}");
    assert_eq!(value["error"]["code"], "tenant_suspended");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let (status, value, _) = call(
        &app,
        ADMIN,
        "POST",
        "/v1/admin/credentials",
        json!({"name":"late", "collections":["*"], "operations":["read"], "expires_unix_ms":now+3600000, "tenant_id":tid}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{value}");
    assert_eq!(value["error"]["code"], "tenant_suspended");
    assert_eq!(
        call(
            &app,
            ADMIN,
            "GET",
            &format!("/v1/collections/t{tid}__docs/vectors/1"),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &app,
            ADMIN,
            "PUT",
            &format!("/v1/collections/t{tid}__docs/vectors/3"),
            json!({"vector":[0.0,1.0]})
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );

    drop(app);
    service.close().await.unwrap();
    let service = Service::new(Database::open(&path).unwrap());
    let app = configured_admin_router(service.clone(), options(dir.path())).unwrap();
    let (_, value, _) = call(&app, &token, "GET", "/v1/collections", Value::Null).await;
    assert_eq!(value["error"]["code"], "tenant_suspended");
    let (_, listing, _) = call(&app, ADMIN, "GET", "/v1/admin/tenants", Value::Null).await;
    assert_eq!(listing["items"][0]["state"], "suspended");
    assert_eq!(listing["items"][0]["vectors"], 3);

    let (status, resumed, _) =
        call(&app, ADMIN, "POST", &format!("{base}/resume"), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{resumed}");
    assert_eq!(resumed["tenant"]["state"], "active");
    assert!(resumed["tenant"]["suspended_unix_ms"].is_null());
    let stored = std::fs::read_to_string(dir.path().join("tenants.json")).unwrap();
    assert!(!stored.contains("suspended_unix_ms"), "{stored}");
    let (status, value, _) = call(&app, &token, "GET", "/v1/collections", Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{value}");

    assert_eq!(
        call(&app, ADMIN, "POST", &format!("{base}/suspend"), Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    for id in [1, 2, 3] {
        assert_eq!(
            call(
                &app,
                ADMIN,
                "DELETE",
                &format!("/v1/collections/t{tid}__docs/vectors/{id}"),
                Value::Null
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    assert_eq!(
        call(
            &app,
            ADMIN,
            "DELETE",
            &format!("/v1/collections/t{tid}__docs"),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, ADMIN, "DELETE", &base, Value::Null).await.0,
        StatusCode::OK
    );
    for (method, suffix) in [("GET", "usage"), ("POST", "resume")] {
        assert_eq!(
            call(
                &app,
                ADMIN,
                method,
                &format!("{base}/{suffix}"),
                Value::Null
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
    }
    let (_, value, _) = call(&app, &token, "GET", "/v1/collections", Value::Null).await;
    assert_eq!(value["error"]["code"], "tenant_unavailable");
    drop(app);
    service.close().await.unwrap();
}
