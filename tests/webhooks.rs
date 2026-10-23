#![cfg(feature = "server")]

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use tower::ServiceExt;
use vlite::{
    Database, FormatVersion,
    server::{BearerToken, ServerOptions, Service, configured_admin_router},
};

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn signature(secret: &[u8], body: &[u8]) -> String {
    let mut key = [0u8; 64];
    if secret.len() > 64 {
        key[..32].copy_from_slice(&Sha256::digest(secret));
    } else {
        key[..secret.len()].copy_from_slice(secret);
    }
    let mut inner = key.iter().map(|byte| byte ^ 0x36).collect::<Vec<_>>();
    inner.extend_from_slice(body);
    let digest = Sha256::digest(inner);
    let mut outer = key.iter().map(|byte| byte ^ 0x5c).collect::<Vec<_>>();
    outer.extend_from_slice(&digest);
    Sha256::digest(outer)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn start_receiver(
    listener: TcpListener,
    sender: mpsc::Sender<(Vec<u8>, String)>,
    count: Arc<AtomicUsize>,
) {
    thread::spawn(move || {
        for _ in 0..2 {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0u8; 4096];
            let header_end = loop {
                let Ok(size) = stream.read(&mut buffer) else {
                    return;
                };
                if size == 0 {
                    return;
                }
                bytes.extend_from_slice(&buffer[..size]);
                if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let headers = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
            let length = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            while bytes.len() < header_end + length {
                let Ok(size) = stream.read(&mut buffer) else {
                    return;
                };
                if size == 0 {
                    return;
                }
                bytes.extend_from_slice(&buffer[..size]);
            }
            let body = bytes[header_end..header_end + length].to_vec();
            let signature = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(name, _)| name.eq_ignore_ascii_case("x-vlite-signature"))
                        .map(|(_, value)| value.trim().to_owned())
                })
                .unwrap_or_default();
            let status = if count.fetch_add(1, Ordering::SeqCst) == 0 {
                "503 Service Unavailable"
            } else {
                "204 No Content"
            };
            let response =
                format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = stream.write_all(response.as_bytes());
            let _ = sender.send((body, signature));
        }
    });
}

async fn call(
    app: &axum::Router,
    method: &str,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> axum::http::Response<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"));
    let bytes = body
        .map(|value| serde_json::to_vec(&value).unwrap())
        .unwrap_or_default();
    if !bytes.is_empty() {
        request = request.header("content-type", "application/json");
    }
    app.clone()
        .oneshot(request.body(Body::from(bytes)).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn webhook_signs_payload_retries_failure_and_reports_delivery_state() {
    let work = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(format!(".webhook-test-{}", std::process::id()));
    let _cleanup = Cleanup(work.clone());
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).unwrap();
    let database_path = work.join("data.vlite");
    let database = Database::create_with_format(&database_path, 2, FormatVersion::V5).unwrap();
    let service = Service::try_new(database).unwrap();
    let token = "webhook_test_token_0123456789012345";
    let admin_dir = work.join("admin");
    let app = configured_admin_router(
        service.clone(),
        ServerOptions {
            admin_token: Some(BearerToken::new(token).unwrap()),
            admin_directory: Some(admin_dir.clone()),
            database_path: Some(database_path),
            ..Default::default()
        },
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/receive", listener.local_addr().unwrap());
    let (sender, receiver) = mpsc::channel();
    let count = Arc::new(AtomicUsize::new(0));
    start_receiver(listener, sender, count.clone());
    let deliveries = tokio::task::spawn_blocking(move || {
        [
            receiver.recv_timeout(Duration::from_secs(20)).unwrap(),
            receiver.recv_timeout(Duration::from_secs(20)).unwrap(),
        ]
    });
    let cursor_path = admin_dir.join("webhooks.cursor");
    let started = Instant::now();
    while !cursor_path.exists() && started.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(cursor_path.exists());
    let secret = "correct-horse-battery-staple";
    let created = call(
        &app,
        "POST",
        "/v1/admin/webhooks",
        token,
        Some(json!({
            "url": endpoint, "collections": ["default"], "events": ["put"], "secret": secret
        })),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let created: Value = serde_json::from_slice(
        &axum::body::to_bytes(created.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    let id = created["id"].as_str().unwrap();
    assert!(created.get("secret").is_none());
    let put = call(
        &app,
        "PUT",
        "/v1/vectors/9",
        token,
        Some(json!({"vector": [1.0, 0.0], "metadata": {}})),
    )
    .await;
    assert_eq!(put.status(), StatusCode::NO_CONTENT);
    let deliveries = deliveries.await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 2);
    assert_eq!(deliveries[0].0, deliveries[1].0);
    for (body, header) in deliveries {
        assert_eq!(
            header,
            format!("sha256={}", signature(secret.as_bytes(), &body))
        );
    }
    let started = Instant::now();
    let status = loop {
        let response = call(
            &app,
            "GET",
            &format!("/v1/admin/webhooks/{id}"),
            token,
            None,
        )
        .await;
        let value: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        if value["last_success_ms"].is_number() || started.elapsed() >= Duration::from_secs(10) {
            break value;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(status["attempts"].as_u64().unwrap_or(0) >= 2);
    assert!(status["last_success_ms"].is_number());
    assert!(status["last_error"].is_null());
    service.close().await.unwrap();
}
