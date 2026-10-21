#![cfg(feature = "server")]
//! Wave2 operational surfaces: scoped credentials (F9), persistent audit,
//! scheduled backups with manifests/verify/drill/retention (F7), automatic
//! maintenance (F6) and Prometheus/slow-query diagnostics (F8).

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
use tower::ServiceExt;
use vlite::{
    Database, FormatVersion,
    server::{BearerToken, ServerOptions, Service, configured_admin_router},
};

const ADMIN: &str = "admin_secret_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const READER: &str = "reader_secret_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";

async fn raw(
    app: &Router,
    token: Option<&str>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, String, String) {
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
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        content_type,
        String::from_utf8(bytes.to_vec()).unwrap(),
    )
}

async fn call(
    app: &Router,
    token: Option<&str>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let (status, _, text) = raw(app, token, method, uri, body).await;
    let value = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::String(text))
    };
    (status, value)
}

async fn admin(app: &Router, method: &str, uri: &str, body: Option<Value>) -> Value {
    let (status, value) = call(app, Some(ADMIN), method, uri, body).await;
    assert!(status.is_success(), "{method} {uri}: {status} {value}");
    value
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn options(directory: &Path) -> ServerOptions {
    ServerOptions {
        admin_token: Some(BearerToken::new(ADMIN).unwrap()),
        read_only_token: Some(BearerToken::new(READER).unwrap()),
        admin_directory: Some(directory.to_owned()),
        ..ServerOptions::default()
    }
}

fn start(db: Database, options: ServerOptions) -> (Service, Router) {
    let service = Service::new(db);
    let app = configured_admin_router(service.clone(), options).unwrap();
    (service, app)
}

fn v4(path: &Path) -> Database {
    Database::create_with_format(path, 2, FormatVersion::V4).unwrap()
}

async fn credential(
    app: &Router,
    collections: &[&str],
    operations: &[&str],
    expires: u64,
) -> (String, String) {
    let (status, created) = call(
        app,
        Some(ADMIN),
        "POST",
        "/v1/admin/credentials",
        Some(
            json!({"name":"svc","collections":collections,"operations":operations,
            "expires_unix_ms":expires}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    (
        created["credential"]["id"].as_str().unwrap().to_owned(),
        created["token"].as_str().unwrap().to_owned(),
    )
}

#[tokio::test]
async fn scoped_credentials_enforce_collection_and_operation_scope_without_leaks() {
    let dir = tempfile::tempdir().unwrap();
    let admin_dir = dir.path().join("admin");
    let (service, app) = start(v4(&dir.path().join("db.vlite")), options(&admin_dir));
    for name in ["docs", "secret"] {
        admin(
            &app,
            "POST",
            "/v1/collections",
            Some(json!({"name":name,"dimensions":2})),
        )
        .await;
        admin(
            &app,
            "PUT",
            &format!("/v1/collections/{name}/vectors/1"),
            Some(json!({"vector":[1,0]})),
        )
        .await;
    }
    let (id, token) = credential(&app, &["docs"], &["read", "search"], now_ms() + 600_000).await;
    assert!(token.starts_with(&format!("vlk_{id}_")));
    let token = token.as_str();

    // Capabilities report the scope (and never the secret).
    let (status, caps) = call(&app, Some(token), "GET", "/v1/capabilities", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(caps["role"], "scoped");
    assert_eq!(caps["scope"]["collections"], json!(["docs"]));
    assert!(!caps.to_string().contains(&token[17..]));

    // Allowed: read and search in the bound collection.
    let (status, _) = call(
        &app,
        Some(token),
        "GET",
        "/v1/collections/docs/vectors/1",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, hits) = call(
        &app,
        Some(token),
        "POST",
        "/v1/collections/docs/search",
        Some(json!({"vector":[1,0],"k":1})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{hits}");

    // Denied: other collection, legacy default, writes, admin routes,
    // lazy-build search, and existence probing of unknown names.
    for (method, uri, body) in [
        ("GET", "/v1/collections/secret/vectors/1", None),
        ("GET", "/v1/collections/missing/vectors/1", None),
        ("GET", "/v1/vectors/1", None),
        ("GET", "/v1/database", None),
        (
            "PUT",
            "/v1/collections/docs/vectors/2",
            Some(json!({"vector":[0,1]})),
        ),
        (
            "POST",
            "/v1/collections",
            Some(json!({"name":"x","dimensions":2})),
        ),
        ("GET", "/v1/admin/credentials", None),
        ("GET", "/v1/admin/audit/persistent", None),
        ("POST", "/v1/admin/backups", None),
        ("GET", "/v1/index/hnsw/rebuild", None),
        (
            "POST",
            "/v1/collections/docs/search",
            Some(json!({"vector":[1,0],"k":1,"mode":"approximate"})),
        ),
    ] {
        let (status, error) = call(&app, Some(token), method, uri, body).await;
        assert!(
            status == StatusCode::FORBIDDEN,
            "{method} {uri}: {status} {error}"
        );
    }

    // Collection list is filtered to the grant.
    let (status, list) = call(&app, Some(token), "GET", "/v1/collections", None).await;
    assert_eq!(status, StatusCode::OK);
    let names = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, ["docs"]);

    // Metrics need the diagnostics operation; without it they are denied.
    let (status, _) = call(&app, Some(token), "GET", "/metrics", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (_, diag) = credential(&app, &["docs"], &["diagnostics"], now_ms() + 600_000).await;
    let (status, metrics) = call(&app, Some(&diag), "GET", "/metrics", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(metrics["scoped"], true);
    assert!(
        metrics.get("requests_total").is_none(),
        "no global counters"
    );
    let names = metrics["collections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, ["docs"]);

    // Denials are counted and audited (rate limited) without secrets.
    let metrics = admin(&app, "GET", "/metrics", None).await;
    assert!(metrics["credential_denials_total"].as_u64().unwrap() >= 10);
    let audit = admin(&app, "GET", "/v1/admin/audit/persistent?limit=1000", None).await;
    assert_eq!(audit["enabled"], true);
    let text = audit.to_string();
    assert!(text.contains("credential_created"));
    assert!(text.contains("credential_denied"));
    assert!(!text.contains(&token[17..]), "secret never audited");

    // Secrets are stored only as hashes.
    let stored = std::fs::read_to_string(admin_dir.join("credentials.json")).unwrap();
    assert!(stored.contains(&id));
    assert!(!stored.contains(&token[17..]));
    assert!(!stored.contains(&diag[17..]));
    service.close().await.unwrap();
}

#[tokio::test]
async fn credentials_expire_revoke_rotate_and_persist_across_restart() {
    let dir = tempfile::tempdir().unwrap();
    let admin_dir = dir.path().join("admin");
    let path = dir.path().join("db.vlite");
    let (service, app) = start(v4(&path), options(&admin_dir));

    // Mandatory, bounded expiry.
    for expires in [0, now_ms() - 1, now_ms() + 400 * 86_400_000] {
        let (status, error) = call(
            &app,
            Some(ADMIN),
            "POST",
            "/v1/admin/credentials",
            Some(json!({"name":"x","collections":["*"],"operations":["read"],
                "expires_unix_ms":expires})),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    }
    let (status, _) = call(
        &app,
        Some(ADMIN),
        "POST",
        "/v1/admin/credentials",
        Some(json!({"name":"x","collections":["*"],"operations":["read"]})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    // Read-only principals cannot manage credentials.
    let (status, _) = call(&app, Some(READER), "GET", "/v1/admin/credentials", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (_, short) = credential(&app, &["*"], &["read"], now_ms() + 300).await;
    let (keep_id, keep) = credential(&app, &["default"], &["read"], now_ms() + 600_000).await;
    let (revoke_id, revoked) = credential(&app, &["*"], &["read"], now_ms() + 600_000).await;
    assert_eq!(
        call(&app, Some(&short), "GET", "/v1/database", None)
            .await
            .0,
        StatusCode::OK
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    let (status, error) = call(&app, Some(&short), "GET", "/v1/database", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(error["error"]["code"], "credential_expired");

    admin(
        &app,
        "POST",
        &format!("/v1/admin/credentials/{revoke_id}/revoke"),
        None,
    )
    .await;
    assert_eq!(
        call(&app, Some(&revoked), "GET", "/v1/database", None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    // Wrong secret for a real ID and garbage tokens are plain 401.
    let forged = format!("{}{}", &keep[..17], "A".repeat(43));
    for bad in [forged.as_str(), "vlk_short", "vlk_aaaaaaaaaaaa_bad"] {
        assert_eq!(
            call(&app, Some(bad), "GET", "/v1/database", None).await.0,
            StatusCode::UNAUTHORIZED
        );
    }

    // Rotation: new token works; old one only for the grace period.
    let rotated = admin(
        &app,
        "POST",
        &format!("/v1/admin/credentials/{keep_id}/rotate"),
        Some(json!({"grace_seconds":0})),
    )
    .await;
    let fresh = rotated["token"].as_str().unwrap().to_owned();
    assert_eq!(rotated["credential"]["rotated_from"], keep_id.as_str());
    assert_eq!(
        call(&app, Some(&fresh), "GET", "/v1/database", None)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, Some(&keep), "GET", "/v1/database", None).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (status, error) = call(
        &app,
        Some(ADMIN),
        "POST",
        &format!("/v1/admin/credentials/{revoke_id}/rotate"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error["error"]["code"], "credential_inactive");
    service.close().await.unwrap();

    // Restart with the same database and admin directory.
    let (service, app) = start(Database::open(&path).unwrap(), options(&admin_dir));
    assert_eq!(
        call(&app, Some(&fresh), "GET", "/v1/database", None)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, Some(&revoked), "GET", "/v1/database", None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let list = admin(&app, "GET", "/v1/admin/credentials", None).await;
    let states = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["state"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(states.contains(&"revoked") && states.contains(&"expired"));
    assert!(!list.to_string().contains("secret_sha256"));
    let audit = admin(&app, "GET", "/v1/admin/audit/persistent", None).await;
    let events = audit["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["event"].as_str())
        .collect::<Vec<_>>();
    for event in [
        "credential_created",
        "credential_revoked",
        "credential_rotated",
    ] {
        assert!(events.contains(&event), "{events:?}");
    }
    service.close().await.unwrap();
}

#[tokio::test]
async fn scopes_fail_closed_on_recreated_collections_and_switched_databases() {
    let dir = tempfile::tempdir().unwrap();
    let admin_dir = dir.path().join("admin");
    let (service, app) = start(v4(&dir.path().join("a.vlite")), options(&admin_dir));
    admin(
        &app,
        "POST",
        "/v1/collections",
        Some(json!({"name":"docs","dimensions":2})),
    )
    .await;
    let (_, token) = credential(&app, &["docs"], &["read", "write"], now_ms() + 600_000).await;
    let (_, everything) = credential(&app, &["*"], &["read"], now_ms() + 600_000).await;
    assert_eq!(
        call(&app, Some(&token), "GET", "/v1/collections/docs", None)
            .await
            .0,
        StatusCode::OK
    );
    // Drop and recreate with the same name: new collection ID, no access.
    admin(&app, "DELETE", "/v1/collections/docs", None).await;
    admin(
        &app,
        "POST",
        "/v1/collections",
        Some(json!({"name":"docs","dimensions":2})),
    )
    .await;
    for (method, uri, body) in [
        ("GET", "/v1/collections/docs", None),
        (
            "PUT",
            "/v1/collections/docs/vectors/1",
            Some(json!({"vector":[1,1]})),
        ),
    ] {
        let (status, error) = call(&app, Some(&token), method, uri, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
        assert_eq!(error["error"]["code"], "forbidden_scope");
    }
    let (_, list) = call(&app, Some(&token), "GET", "/v1/collections", None).await;
    assert_eq!(list["items"], json!([]));
    service.close().await.unwrap();

    // Same admin directory, different database lineage: every scoped token
    // (including `*`) fails closed.
    let (service, app) = start(v4(&dir.path().join("b.vlite")), options(&admin_dir));
    for token in [&token, &everything] {
        let (status, error) = call(&app, Some(token), "GET", "/v1/database", None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
        let (status, _) = call(&app, Some(token), "GET", "/v1/collections", None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    service.close().await.unwrap();
}

#[tokio::test]
async fn credentials_require_admin_token_directory_and_v4() {
    let dir = tempfile::tempdir().unwrap();
    // No admin directory.
    let (service, app) = start(
        v4(&dir.path().join("a.vlite")),
        ServerOptions {
            admin_token: Some(BearerToken::new(ADMIN).unwrap()),
            ..ServerOptions::default()
        },
    );
    let (status, error) = call(&app, Some(ADMIN), "GET", "/v1/admin/credentials", None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error["error"]["code"], "credentials_disabled");
    let caps = admin(&app, "GET", "/v1/capabilities", None).await;
    assert_eq!(caps["features"]["scoped_credentials"], false);
    service.close().await.unwrap();
    // v3 database: creation fails explicitly.
    let (service, app) = start(
        Database::create(dir.path().join("v3.vlite"), 2).unwrap(),
        options(&dir.path().join("admin3")),
    );
    let (status, error) = call(
        &app,
        Some(ADMIN),
        "POST",
        "/v1/admin/credentials",
        Some(json!({"name":"x","collections":["*"],"operations":["read"],
            "expires_unix_ms":now_ms() + 60_000})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(error["error"]["code"], "requires_format_v4");
    service.close().await.unwrap();
    // A corrupt credential store fails startup rather than dropping grants.
    let bad = dir.path().join("bad");
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(bad.join("credentials.json"), b"{not json").unwrap();
    let service = Service::new(v4(&dir.path().join("c.vlite")));
    assert!(configured_admin_router(service.clone(), options(&bad)).is_err());
    service.close().await.unwrap();
}

#[tokio::test]
async fn backups_have_manifests_verify_drill_and_bounded_retention() {
    let dir = tempfile::tempdir().unwrap();
    let admin_dir = dir.path().join("admin");
    let mut db = v4(&dir.path().join("db.vlite"));
    db.put(1, &[1.0, 2.0], b"").unwrap();
    let (service, app) = start(db, options(&admin_dir));

    // Off by default; validated; read-only cannot change it.
    let schedule = admin(&app, "GET", "/v1/admin/backups/schedule", None).await;
    assert_eq!(schedule["config"]["enabled"], false);
    assert_eq!(schedule["config"]["retention_enabled"], false);
    for body in [
        json!({"interval_secs":1}),
        json!({"retain_last":0}),
        json!({"unknown":1}),
    ] {
        let (status, _) = call(
            &app,
            Some(ADMIN),
            "PUT",
            "/v1/admin/backups/schedule",
            Some(body),
        )
        .await;
        assert!(status.is_client_error());
    }
    let (status, _) = call(
        &app,
        Some(READER),
        "PUT",
        "/v1/admin/backups/schedule",
        Some(json!({"enabled":true})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Manual backup writes a verified manifest.
    let manual = admin(&app, "POST", "/v1/admin/backups", None).await;
    let first = manual["name"].as_str().unwrap().to_owned();
    assert_eq!(manual["manifest"]["live_records"], 1);
    assert_eq!(manual["manifest"]["format_version"], 4);
    assert!(manual["manifest"]["lineage"].is_string());
    assert!(admin_dir.join(format!("{first}.manifest.json")).exists());

    // Scheduled run: backup + verify + drill.
    admin(
        &app,
        "PUT",
        "/v1/admin/backups/schedule",
        Some(json!({"retention_enabled":true,"retain_last":1,"drill_every":1})),
    )
    .await;
    let run = admin(&app, "POST", "/v1/admin/backups/schedule/run", None).await;
    assert_eq!(run["manifest"]["drill"]["status"], "passed", "{run}");
    let second = run["name"].as_str().unwrap().to_owned();
    // retain_last 1 keeps the newest; the older manual backup is deleted.
    assert_eq!(run["retention_deleted"], json!([first]));
    assert!(!admin_dir.join(&first).exists());
    assert!(admin_dir.join(&second).exists());

    // Explicit verify and drill.
    let verified = admin(
        &app,
        "POST",
        &format!("/v1/admin/backups/{second}/verify"),
        None,
    )
    .await;
    assert_eq!(verified["verified"], true);
    let drilled = admin(
        &app,
        "POST",
        &format!("/v1/admin/backups/{second}/drill"),
        None,
    )
    .await;
    assert_eq!(drilled["drill"]["status"], "passed");
    assert!(drilled["probes"].as_u64().unwrap() >= 1);

    // Corruption is detected, counted and reported, never hidden.
    let mut bytes = std::fs::read(admin_dir.join(&second)).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    std::fs::write(admin_dir.join(&second), &bytes).unwrap();
    let (status, error) = call(
        &app,
        Some(ADMIN),
        "POST",
        &format!("/v1/admin/backups/{second}/verify"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error["error"]["code"], "verification_failed");
    let (status, error) = call(
        &app,
        Some(ADMIN),
        "POST",
        &format!("/v1/admin/backups/{second}/drill"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error["error"]["code"], "drill_failed");
    // Path traversal and unknown names are rejected.
    let (status, _) = call(
        &app,
        Some(ADMIN),
        "POST",
        "/v1/admin/backups/..%2Fdb.vlite/verify",
        None,
    )
    .await;
    assert!(status.is_client_error());

    let metrics = admin(&app, "GET", "/metrics", None).await;
    assert_eq!(metrics["backup_runs_total"], 1);
    assert_eq!(metrics["backup_verify_failures_total"], 1);
    assert_eq!(metrics["backup_drill_failures_total"], 1);
    assert_eq!(metrics["backup_retention_deleted_total"], 1);
    let audit = admin(&app, "GET", "/v1/admin/audit/persistent?limit=1000", None).await;
    let text = audit.to_string();
    for event in [
        "backup_created",
        "backup_scheduled",
        "backup_retention_delete",
        "backup_verify_failed",
        "backup_drill",
    ] {
        assert!(text.contains(event), "{event}");
    }
    let list = admin(&app, "GET", "/v1/admin/backups", None).await;
    assert!(list["items"][0]["manifest"].is_object());
    service.close().await.unwrap();
}

#[tokio::test]
async fn backup_errors_are_counted_and_scheduling_requires_admin_directory() {
    let dir = tempfile::tempdir().unwrap();
    let (service, app) = start(
        v4(&dir.path().join("db.vlite")),
        ServerOptions {
            admin_token: Some(BearerToken::new(ADMIN).unwrap()),
            ..ServerOptions::default()
        },
    );
    let (status, error) = call(
        &app,
        Some(ADMIN),
        "PUT",
        "/v1/admin/backups/schedule",
        Some(json!({"enabled":true})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error["error"]["code"], "admin_directory_disabled");
    let (status, _) = call(
        &app,
        Some(ADMIN),
        "POST",
        "/v1/admin/backups/schedule/run",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let metrics = admin(&app, "GET", "/metrics", None).await;
    assert_eq!(metrics["backup_failures_total"], 1);
    assert!(metrics["backup_last_failure_unix_ms"].is_u64());
    service.close().await.unwrap();
    // Startup validation rejects a schedule without a directory.
    let service = Service::new(v4(&dir.path().join("other.vlite")));
    assert!(
        configured_admin_router(
            service.clone(),
            ServerOptions {
                admin_token: Some(BearerToken::new(ADMIN).unwrap()),
                backup_interval: Some(Duration::from_secs(3600)),
                ..ServerOptions::default()
            },
        )
        .is_err()
    );
    service.close().await.unwrap();
}

#[tokio::test]
async fn auto_maintenance_is_off_by_default_bounded_and_rebuilds_stale_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = v4(&dir.path().join("db.vlite"));
    for id in 0..40u64 {
        db.put(id, &[id as f32, 1.0], b"").unwrap();
    }
    let (service, app) = start(db, options(&dir.path().join("admin")));
    let auto = admin(&app, "GET", "/v1/admin/maintenance/auto", None).await;
    assert_eq!(auto["config"]["enabled"], false);
    assert_eq!(auto["state"]["worker_running"], false);
    // No index yet: nothing to do.
    let tick = admin(&app, "POST", "/v1/admin/maintenance/auto/tick", None).await;
    assert_eq!(tick["result"]["action"], "idle");
    admin(&app, "POST", "/v1/index/hnsw", Some(json!({}))).await;
    let tick = admin(&app, "POST", "/v1/admin/maintenance/auto/tick", None).await;
    assert_eq!(tick["result"]["action"], "healthy", "{tick}");
    // Delete half the records: the stale ratio triggers a background rebuild
    // of the same identity, which a later tick publishes.
    for id in 0..20u64 {
        admin(&app, "DELETE", &format!("/v1/vectors/{id}"), None).await;
    }
    let tick = admin(&app, "POST", "/v1/admin/maintenance/auto/tick", None).await;
    assert_eq!(tick["result"]["action"], "rebuild_started", "{tick}");
    assert_eq!(tick["result"]["reason"], "stale_ratio");
    let mut published = false;
    for _ in 0..200 {
        let tick = admin(&app, "POST", "/v1/admin/maintenance/auto/tick", None).await;
        if tick["result"]["action"] == "published" {
            published = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(published);
    let metrics = admin(&app, "GET", "/metrics", None).await;
    assert_eq!(metrics["hnsw_background_rebuilds_started_total"], 1);
    assert_eq!(metrics["hnsw_background_rebuilds_published_total"], 1);
    assert!(metrics["auto_maintenance_ticks_total"].as_u64().unwrap() >= 4);
    let status = admin(&app, "GET", "/v1/index/hnsw", None).await;
    assert_eq!(status["stale_nodes"], 0);

    // Validation and enabling start the worker; close stops and joins it.
    for body in [
        json!({"interval_secs":5}),
        json!({"stale_ratio":0}),
        json!({"min_recall":2}),
    ] {
        let (status, _) = call(
            &app,
            Some(ADMIN),
            "PUT",
            "/v1/admin/maintenance/auto",
            Some(body),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }
    let auto = admin(
        &app,
        "PUT",
        "/v1/admin/maintenance/auto",
        Some(json!({"enabled":true,"interval_secs":10})),
    )
    .await;
    assert_eq!(auto["state"]["worker_running"], true);
    let started = std::time::Instant::now();
    service.close().await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "worker joined promptly"
    );
}

#[tokio::test]
async fn scheduled_workers_start_from_options_and_stop_on_close() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = v4(&dir.path().join("db.vlite"));
    db.put(1, &[1.0, 2.0], b"").unwrap();
    let service = Service::new(db);
    let app = configured_admin_router(
        service.clone(),
        ServerOptions {
            auto_maintenance_interval: Some(Duration::from_secs(10)),
            backup_interval: Some(Duration::from_secs(300)),
            ..options(&dir.path().join("admin"))
        },
    )
    .unwrap();
    let auto = admin(&app, "GET", "/v1/admin/maintenance/auto", None).await;
    assert_eq!(auto["config"]["enabled"], true);
    assert_eq!(auto["state"]["worker_running"], true);
    let schedule = admin(&app, "GET", "/v1/admin/backups/schedule", None).await;
    assert_eq!(schedule["config"]["enabled"], true);
    assert_eq!(schedule["config"]["interval_secs"], 300);
    let started = std::time::Instant::now();
    service.close().await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    // Closed: the database is unavailable and nothing restarts the worker.
    let (status, _) = call(&app, Some(ADMIN), "GET", "/v1/database", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let auto = admin(&app, "GET", "/v1/admin/maintenance/auto", None).await;
    assert_eq!(auto["state"]["worker_running"], false);
}

#[tokio::test]
async fn prometheus_metrics_and_slow_query_log_expose_counters_not_values() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = v4(&dir.path().join("db.vlite"));
    db.put(1, &[1.0, 2.0], b"").unwrap();
    let (service, app) = start(
        db,
        ServerOptions {
            slow_query_threshold: Some(Duration::from_nanos(1)),
            ..options(&dir.path().join("admin"))
        },
    );
    let (status, _) = call(
        &app,
        Some(READER),
        "POST",
        "/v1/search",
        Some(
            json!({"vector":[0.123,0.456],"k":1,"filter":[{"field":"lang","op":"eq",
            "value":{"type":"string","value":"needle-value"}}]}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let slow = admin(&app, "GET", "/v1/admin/diagnostics/slow-queries", None).await;
    assert_eq!(slow["capacity"], 256);
    let item = &slow["items"][0];
    assert_eq!(item["route"], "/v1/search");
    assert_eq!(item["mode"], "exact");
    assert_eq!(item["filter_fields"], json!(["lang"]));
    let text = slow.to_string();
    assert!(!text.contains("needle-value") && !text.contains("0.123"));
    // Read-only cannot read admin diagnostics.
    let (status, _) = call(
        &app,
        Some(READER),
        "GET",
        "/v1/admin/diagnostics/slow-queries",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, content_type, body) =
        raw(&app, Some(ADMIN), "GET", "/metrics?format=prometheus", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.starts_with("text/plain; version=0.0.4"));
    for needle in [
        "# TYPE vlite_requests_total counter",
        "vlite_slow_queries_total 1",
        "vlite_database_healthy 1",
        "vlite_searches_total{strategy=\"exact\"} 1",
        "vlite_collection_live_records{collection=\"default\"} 1",
        "vlite_format_version 4",
    ] {
        assert!(body.contains(needle), "{needle}\n{body}");
    }
    let (status, _) = call(&app, Some(ADMIN), "GET", "/metrics?format=xml", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let json = admin(&app, "GET", "/metrics", None).await;
    assert_eq!(json["slow_queries_total"], 1);
    assert_eq!(json["searches_by_strategy"]["exact"], 1);
    assert!(json["reclaimable_bytes_estimate"].is_u64());
    service.close().await.unwrap();
}

#[tokio::test]
async fn slow_query_diagnostics_use_the_collection_binding_not_the_name() {
    let dir = tempfile::tempdir().unwrap();
    let (service, app) = start(
        v4(&dir.path().join("db.vlite")),
        ServerOptions {
            slow_query_threshold: Some(Duration::from_nanos(1)),
            ..options(&dir.path().join("admin"))
        },
    );
    admin(
        &app,
        "POST",
        "/v1/collections",
        Some(json!({"name":"docs","dimensions":2})),
    )
    .await;
    let old_id = admin(&app, "GET", "/v1/collections/docs", None).await["id"].clone();
    let (_, token) = credential(
        &app,
        &["docs"],
        &["search", "diagnostics"],
        now_ms() + 600_000,
    )
    .await;
    let search = json!({"vector":[1,0],"k":1});
    let (status, _) = call(
        &app,
        Some(&token),
        "POST",
        "/v1/collections/docs/search",
        Some(search.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Drop and recreate with the same name; the admin searches the new one.
    admin(&app, "DELETE", "/v1/collections/docs", None).await;
    admin(
        &app,
        "POST",
        "/v1/collections",
        Some(json!({"name":"docs","dimensions":2})),
    )
    .await;
    admin(&app, "POST", "/v1/collections/docs/search", Some(search)).await;
    let all = admin(&app, "GET", "/v1/admin/diagnostics/slow-queries", None).await;
    assert_eq!(all["items"].as_array().unwrap().len(), 2);
    let (status, scoped) = call(
        &app,
        Some(&token),
        "GET",
        "/v1/admin/diagnostics/slow-queries",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{scoped}");
    let items = scoped["items"].as_array().unwrap();
    assert_eq!(
        items.len(),
        1,
        "recreated collection diagnostics never leak: {scoped}"
    );
    assert_eq!(items[0]["collection_id"], old_id);
    service.close().await.unwrap();
}

#[tokio::test]
async fn verification_rejects_manifests_whose_derived_metadata_was_altered() {
    let dir = tempfile::tempdir().unwrap();
    let admin_dir = dir.path().join("admin");
    let mut db = v4(&dir.path().join("db.vlite"));
    db.put(1, &[1.0, 2.0], b"").unwrap();
    let (service, app) = start(db, options(&admin_dir));
    let name = admin(&app, "POST", "/v1/admin/backups", None).await["name"]
        .as_str()
        .unwrap()
        .to_owned();
    let path = admin_dir.join(format!("{name}.manifest.json"));
    let original = std::fs::read_to_string(&path).unwrap();
    let uri = format!("/v1/admin/backups/{name}/verify");
    assert_eq!(admin(&app, "POST", &uri, None).await["verified"], true);
    let mut failures = 0;
    for (field, value) in [
        ("lineage", json!("00000000deadbeef")),
        ("format_version", json!(3)),
        ("dimensions", json!(3)),
        ("live_records", json!(2)),
        ("commits", json!(99)),
        ("collections", json!([])),
    ] {
        let mut manifest: Value = serde_json::from_str(&original).unwrap();
        manifest[field] = value;
        std::fs::write(&path, manifest.to_string()).unwrap();
        let (status, error) = call(&app, Some(ADMIN), "POST", &uri, None).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{field}: {error}");
        assert_eq!(error["error"]["code"], "verification_failed");
        failures += 1;
        // The altered manifest is left as-is, never re-marked verified.
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after[field], manifest[field]);
    }
    std::fs::write(&path, b"{not json").unwrap();
    let (status, _) = call(&app, Some(ADMIN), "POST", &uri, None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    failures += 1;
    std::fs::write(&path, &original).unwrap();
    assert_eq!(admin(&app, "POST", &uri, None).await["verified"], true);
    let metrics = admin(&app, "GET", "/metrics", None).await;
    assert_eq!(metrics["backup_verify_failures_total"], failures);
    service.close().await.unwrap();
}
