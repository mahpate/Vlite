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
    Database,
    server::{BearerToken, ServerOptions, Service, configured_admin_router, selected_database},
};

const ADMIN: &str = "admin_test_secret_012345678901234567890123456";
const READ: &str = "read_only_secret_012345678901234567890123456";
const ROTATED: &str = "rotated_secret_0123456789012345678901234567";

async fn call(
    app: &Router,
    token: &str,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, body)
}
fn options() -> ServerOptions {
    ServerOptions {
        admin_token: Some(BearerToken::new(ADMIN).unwrap()),
        read_only_token: Some(BearerToken::new(READ).unwrap()),
        ..ServerOptions::default()
    }
}

#[tokio::test]
async fn roles_enforce_every_mutation_and_lazy_ann_without_hiding_reads() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::create(dir.path().join("db"), 2).unwrap();
    db.put(17, &[1., 2.], b"private payload").unwrap();
    let app = configured_admin_router(Service::new(db), options()).unwrap();
    let (code, caps) = call(&app, READ, "GET", "/v1/capabilities", Value::Null).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(caps["role"], "read_only");
    assert_eq!(caps["features"]["backup_restore"], false);
    assert_eq!(
        call(&app, ADMIN, "GET", "/v1/capabilities", Value::Null)
            .await
            .1["role"],
        "admin"
    );
    assert_eq!(
        call(&app, "invalid", "GET", "/v1/capabilities", Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    for (method, path) in [
        ("PUT", "/v1/vectors/17"),
        ("DELETE", "/v1/vectors/17"),
        ("POST", "/v1/batches"),
        ("POST", "/v1/index/hnsw"),
        ("POST", "/v1/index/hnsw/rebuild"),
        ("POST", "/v1/index/hnsw/rebuild/poll"),
        ("POST", "/v1/index/hnsw/rebuild/cancel"),
        ("PUT", "/v1/index/hnsw/maintenance"),
        ("POST", "/v1/index/hnsw/maintenance/tick"),
        ("GET", "/v1/admin/tokens"),
        ("GET", "/v1/admin/audit"),
        ("GET", "/v1/admin/backups"),
        ("POST", "/v1/admin/tokens/reload"),
        ("POST", "/v1/admin/tokens/revoke-read-only"),
        ("POST", "/v1/admin/restores"),
        ("POST", "/v1/admin/restores/select"),
    ] {
        assert_eq!(
            call(&app, READ, method, path, json!({})).await.0,
            StatusCode::FORBIDDEN,
            "{method} {path}"
        );
    }
    assert_eq!(
        call(
            &app,
            READ,
            "POST",
            "/v1/search",
            json!({"vector":[1,2],"k":1,"mode":"approximate"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &app,
            READ,
            "POST",
            "/v1/search",
            json!({"vector":[1,2],"k":1})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, READ, "GET", "/v1/vectors/17", Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, READ, "GET", "/v1/index/hnsw/rebuild", Value::Null)
            .await
            .1["status"]["rebuild"],
        Value::Null
    );
    assert_eq!(
        call(&app, ADMIN, "POST", "/v1/admin/backups", json!({}))
            .await
            .0,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn file_rotation_is_atomic_and_read_only_revocation_does_not_revoke_admin() {
    let dir = tempfile::tempdir().unwrap();
    let admin_file = dir.path().join("admin-token");
    let read_file = dir.path().join("read-token");
    std::fs::write(&admin_file, ADMIN).unwrap();
    std::fs::write(&read_file, READ).unwrap();
    let mut opts = options();
    opts.admin_token_file = Some(admin_file.clone());
    opts.read_only_token_file = Some(read_file.clone());
    let app = configured_admin_router(
        Service::new(Database::create(dir.path().join("db"), 1).unwrap()),
        opts,
    )
    .unwrap();
    std::fs::write(&admin_file, ROTATED).unwrap();
    std::fs::write(&read_file, "bad").unwrap();
    assert_eq!(
        call(&app, ADMIN, "POST", "/v1/admin/tokens/reload", json!({}))
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        call(&app, ADMIN, "GET", "/v1/capabilities", Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, ROTATED, "GET", "/v1/capabilities", Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    std::fs::write(&read_file, READ).unwrap();
    let (code, result) = call(&app, ADMIN, "POST", "/v1/admin/tokens/reload", json!({})).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(result["generation"], 2);
    let text = result.to_string();
    assert!(!text.contains(ROTATED) && !text.contains(READ) && !text.contains(ADMIN));
    assert_eq!(
        call(&app, ADMIN, "GET", "/v1/capabilities", Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&app, ROTATED, "GET", "/v1/capabilities", Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &app,
            ROTATED,
            "POST",
            "/v1/admin/tokens/revoke-read-only",
            json!({})
        )
        .await
        .1["read_only_configured"],
        false
    );
    assert_eq!(
        call(&app, READ, "GET", "/v1/database", Value::Null).await.0,
        StatusCode::UNAUTHORIZED
    );
    std::fs::write(&read_file, "").unwrap();
    assert_eq!(
        call(&app, ROTATED, "POST", "/v1/admin/tokens/reload", json!({}))
            .await
            .1["read_only_configured"],
        false
    );
    std::fs::write(&read_file, ROTATED).unwrap();
    assert_eq!(
        call(&app, ROTATED, "POST", "/v1/admin/tokens/reload", json!({}))
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        call(&app, ROTATED, "GET", "/v1/database", Value::Null)
            .await
            .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn audit_is_bounded_and_never_contains_ids_queries_content_or_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let app = configured_admin_router(
        Service::new(Database::create(dir.path().join("db"), 1).unwrap()),
        options(),
    )
    .unwrap();
    for _ in 0..260 {
        call(
            &app,
            ADMIN,
            "GET",
            "/v1/vectors/918273645?secret=private-query",
            Value::Null,
        )
        .await;
    }
    call(
        &app,
        ADMIN,
        "PUT",
        "/v1/vectors/918273645",
        json!({"vector":[1],"payload_base64":"c2VjcmV0","metadata":{"text":"private-value"}}),
    )
    .await;
    let audit = call(&app, ADMIN, "GET", "/v1/admin/audit", Value::Null)
        .await
        .1;
    assert_eq!(audit["items"].as_array().unwrap().len(), 256);
    assert_eq!(audit["items"][255]["route"], "/v1/vectors/{id}");
    for private in [
        "918273645",
        "private-query",
        "private-value",
        "c2VjcmV0",
        ADMIN,
        READ,
        "secret=",
    ] {
        assert!(!audit.to_string().contains(private), "{private}");
    }
}

#[tokio::test]
async fn background_jobs_policy_and_configured_ready_search_work() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::create(dir.path().join("db"), 2).unwrap();
    db.put(7, &[1., 0.], b"").unwrap();
    let app = configured_admin_router(Service::new(db), options()).unwrap();
    for body in [
        json!({"config":{"m":1}}),
        json!({"config":{"m":30,"ef_construction":2}}),
        json!({"limits":{"max_records":0}}),
        json!({"limits":{"max_snapshot_vector_bytes":1}}),
        json!({"metric":"inner_product"}),
    ] {
        assert!(
            call(&app, ADMIN, "POST", "/v1/index/hnsw/rebuild", body)
                .await
                .0
                .is_client_error()
        );
    }
    let config = json!({"m":8,"ef_construction":32,"seed":"18446744073709551615"});
    assert_eq!(
        call(
            &app,
            ADMIN,
            "PUT",
            "/v1/index/hnsw/maintenance",
            json!({"config":config,"metric":"cosine","rebuild_at_buffered_ids":1})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &app,
            ADMIN,
            "POST",
            "/v1/index/hnsw/rebuild",
            json!({"config":config,"metric":"cosine"})
        )
        .await
        .0,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        call(&app, ADMIN, "POST", "/v1/index/hnsw/rebuild", json!({}))
            .await
            .0,
        StatusCode::CONFLICT
    );
    // GET observes a finished worker without consuming or publishing it.
    for _ in 0..2000 {
        let status = call(&app, READ, "GET", "/v1/index/hnsw/rebuild", Value::Null)
            .await
            .1;
        if status["status"]["rebuild"]["worker_finished"] == true {
            break;
        }
        std::thread::yield_now();
    }
    let observed = call(&app, READ, "GET", "/v1/index/hnsw/rebuild", Value::Null)
        .await
        .1;
    assert!(observed["status"]["rebuild"].is_object());
    assert_eq!(observed["status"]["ready"], false);
    assert_eq!(
        call(
            &app,
            ADMIN,
            "POST",
            "/v1/index/hnsw/rebuild/poll",
            json!({})
        )
        .await
        .1["outcome"],
        "published"
    );
    assert_eq!(call(&app,READ,"POST","/v1/search",json!({"vector":[1,0],"k":1,"mode":"approximate_ready","metric":"cosine","config":config})).await.0,StatusCode::OK);
    assert_eq!(
        call(&app, ADMIN, "GET", "/v1/index/hnsw", Value::Null)
            .await
            .1["config"]["m"],
        8
    );
    assert_eq!(
        call(
            &app,
            ADMIN,
            "PUT",
            "/v1/index/hnsw/maintenance",
            json!({"rebuild_at_buffered_ids":257})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        call(
            &app,
            ADMIN,
            "GET",
            "/v1/index/hnsw/maintenance",
            Value::Null
        )
        .await
        .1["policy"]["config"]["m"],
        8
    );
    assert_eq!(
        call(
            &app,
            ADMIN,
            "POST",
            "/v1/index/hnsw/maintenance/tick",
            json!({})
        )
        .await
        .1["started"],
        false
    );
    assert_eq!(
        call(
            &app,
            ADMIN,
            "POST",
            "/v1/index/hnsw/rebuild",
            json!({"config":config,"metric":"cosine"})
        )
        .await
        .0,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        call(
            &app,
            ADMIN,
            "POST",
            "/v1/index/hnsw/rebuild/cancel",
            json!({})
        )
        .await
        .1["cancelled"],
        true
    );
    for _ in 0..2000 {
        let result = call(
            &app,
            ADMIN,
            "POST",
            "/v1/index/hnsw/rebuild/poll",
            json!({}),
        )
        .await
        .1;
        if result["outcome"] == "cancelled" {
            return;
        }
        std::thread::yield_now();
    }
    panic!("cancelled worker was not reaped");
}

#[tokio::test]
async fn backups_restore_to_new_validated_files_and_selection_never_changes_live_owner() {
    let dir = tempfile::tempdir().unwrap();
    let admin = dir.path().join("private-admin");
    let original = dir.path().join("db");
    let mut db = Database::create(&original, 2).unwrap();
    db.put(7, &[1., 2.], b"snapshot payload").unwrap();
    let service = Service::new(db);
    let mut opts = options();
    opts.admin_directory = Some(admin.clone());
    let app = configured_admin_router(service.clone(), opts).unwrap();
    let (code, backup) = call(&app, ADMIN, "POST", "/v1/admin/backups", json!({})).await;
    assert_eq!(code, StatusCode::CREATED);
    assert_eq!(backup["inspection"]["live_records"], 1);
    let backup_name = backup["name"].as_str().unwrap();
    assert_eq!(
        call(&app, ADMIN, "PUT", "/v1/vectors/8", json!({"vector":[3,4]}))
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    let (code, restored) = call(
        &app,
        ADMIN,
        "POST",
        "/v1/admin/restores",
        json!({"backup":backup_name}),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED);
    assert_eq!(restored["inspection"]["live_records"], 1);
    let restored_name = restored["name"].as_str().unwrap();
    assert_eq!(
        call(
            &app,
            ADMIN,
            "POST",
            "/v1/admin/restores/select",
            json!({"name":restored_name})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, ADMIN, "GET", "/v1/database", Value::Null)
            .await
            .1["records"],
        2
    );
    assert_eq!(
        call(&app, ADMIN, "GET", "/v1/admin/backups", Value::Null)
            .await
            .1["items"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    for bad in [
        "../db",
        "restore-../db.vlite",
        "/etc/passwd",
        "backup-a.vlite/../db",
    ] {
        assert_eq!(
            call(
                &app,
                ADMIN,
                "POST",
                "/v1/admin/restores",
                json!({"backup":bad})
            )
            .await
            .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    let selected = selected_database(&admin).unwrap().unwrap();
    assert_eq!(selected, admin.join(restored_name));
    let restored_db = Database::open(&selected).unwrap();
    assert_eq!(restored_db.len(), 1);
    drop(restored_db);
    // A corrupt copy is never selected; a previously valid marker survives.
    std::fs::write(admin.join("restore-bad.vlite"), b"corrupt").unwrap();
    assert!(
        call(
            &app,
            ADMIN,
            "POST",
            "/v1/admin/restores/select",
            json!({"name":"restore-bad.vlite"})
        )
        .await
        .0
        .is_server_error()
    );
    assert_eq!(selected_database(&admin).unwrap().unwrap(), selected);
    service.close().await.unwrap();
    assert_eq!(Database::open(original).unwrap().len(), 2);
}

#[cfg(unix)]
#[tokio::test]
async fn snapshot_and_startup_symlinks_are_rejected() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let admin = dir.path().join("admin");
    let mut opts = options();
    opts.admin_directory = Some(admin.clone());
    let app = configured_admin_router(
        Service::new(Database::create(dir.path().join("db"), 1).unwrap()),
        opts,
    )
    .unwrap();
    symlink(dir.path().join("db"), admin.join("backup-link.vlite")).unwrap();
    assert_eq!(
        call(
            &app,
            ADMIN,
            "POST",
            "/v1/admin/restores",
            json!({"backup":"backup-link.vlite"})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert!(
        call(&app, ADMIN, "GET", "/v1/admin/backups", Value::Null)
            .await
            .1["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    symlink(dir.path().join("db"), admin.join("next-startup")).unwrap();
    assert!(selected_database(&admin).is_err());
}

#[test]
fn startup_roles_fail_closed_before_creation_and_selected_restore_is_opened() {
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::TcpStream,
        process::{Command, Stdio},
        sync::mpsc,
        time::Duration,
    };
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("db");
    let admin = dir.path().join("admin");
    std::fs::create_dir(&admin).unwrap();
    let command = || {
        let mut c = Command::new(env!("CARGO_BIN_EXE_vlite-server"));
        c.arg(&original)
            .args(["--create-if-missing", "2", "--bind", "127.0.0.1:0"])
            .env_remove("VLITE_AUTH_TOKEN_FILE")
            .env_remove("VLITE_READ_ONLY_TOKEN_FILE")
            .env("VLITE_AUTH_TOKEN", ADMIN)
            .env("VLITE_ADMIN_DIR", &admin);
        c
    };
    let result = command()
        .env("VLITE_READ_ONLY_TOKEN", ADMIN)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(!original.exists());
    assert!(!String::from_utf8_lossy(&result.stderr).contains(ADMIN));
    let mut db = Database::create(&original, 2).unwrap();
    db.put(1, &[1., 2.], b"").unwrap();
    db.close().unwrap();
    let restored = admin.join("restore-startup.vlite");
    vlite::maintenance::backup(&original, &restored).unwrap();
    let mut db = Database::open(&original).unwrap();
    db.put(2, &[2., 3.], b"").unwrap();
    db.close().unwrap();
    std::fs::write(admin.join("next-startup"), "restore-startup.vlite").unwrap();
    let child = command()
        .env("VLITE_READ_ONLY_TOKEN", READ)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Child(child);
    let stdout = child.0.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).unwrap();
        let _ = tx.send(line);
    });
    let line = rx.recv_timeout(Duration::from_secs(15)).unwrap();
    let address = line.trim().strip_prefix("LISTENING http://").unwrap();
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(stream, "GET /v1/database HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nAuthorization: Bearer {READ}\r\n\r\n").unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"));
    let (_, json) = response.split_once("\r\n\r\n").unwrap();
    assert_eq!(serde_json::from_str::<Value>(json).unwrap()["records"], 1);
    drop(child);
    assert_eq!(Database::open(&original).unwrap().len(), 2);
    std::fs::write(admin.join("next-startup"), "../db").unwrap();
    let failed = command()
        .env("VLITE_READ_ONLY_TOKEN", READ)
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert!(!String::from_utf8_lossy(&failed.stdout).contains("LISTENING"));
}
