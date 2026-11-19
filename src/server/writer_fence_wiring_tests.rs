use super::authority::{AuthorityStore, Record};
use super::cloud_backup::fake::{Fault, Memory, Op};
use super::writer_fence::{State as FenceState, WriterFence};
use super::*;
use axum::{body::Body, http::Request};
use tower::ServiceExt;

const KEY: &str = "test/authority.json";

fn service() -> Service {
    Service::new(Database::create_in_memory(1, crate::CreateOptions::default()).unwrap())
}

fn count_gets(memory: &Arc<Memory>, count: Arc<AtomicU64>) {
    let weak = Arc::downgrade(memory);
    memory.hook(Op::Get, KEY, move || {
        count.fetch_add(1, Ordering::SeqCst);
        if let Some(memory) = weak.upgrade() {
            count_gets(&memory, count);
        }
    });
}

fn setup() -> (Arc<Memory>, WriterFence, Arc<AtomicU64>) {
    let memory = Arc::new(Memory::default());
    let store = AuthorityStore::new(memory.clone(), "test/".into());
    let prepared = store
        .create("installation", "scope", "0123456789abcdef")
        .unwrap();
    let (claim, _) = store.claim(&prepared, 1).unwrap();
    let fence = WriterFence::new(store, claim);
    fence.transition(FenceState::Recovering).unwrap();
    fence.transition(FenceState::Serving).unwrap();
    let gets = Arc::new(AtomicU64::new(0));
    count_gets(&memory, gets.clone());
    (memory, fence, gets)
}

fn writer(fence: WriterFence, group: bool) -> Service {
    let service = service().with_serve_mode(ServeMode::Writer).unwrap();
    let service = if group {
        service
            .with_group_commit(GroupCommitConfig::default())
            .unwrap()
    } else {
        service
    };
    service.with_writer_fence(fence).unwrap()
}

fn change(memory: &Memory) {
    let mut record: Record = serde_json::from_slice(&memory.raw(KEY).unwrap()).unwrap();
    record.epoch += 1;
    memory.set_raw(KEY, &serde_json::to_vec(&record).unwrap());
}

async fn call(
    router: &Router,
    method: &str,
    path: &str,
    body: &str,
) -> (StatusCode, HeaderMap, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        headers,
        serde_json::from_slice(&body).unwrap_or(Value::Null),
    )
}

fn error(response: &(StatusCode, HeaderMap, Value), status: StatusCode, code: &str) {
    assert_eq!(response.0, status, "{:?}", response.2);
    assert_eq!(response.2["error"]["code"], code, "{:?}", response.2);
    assert!(response.1.contains_key("x-vlite-epoch"));
}

#[tokio::test]
async fn disabled_fence_adds_no_header_and_no_store_reads() {
    let (_memory, _unused_fence, gets) = setup();
    for mode in [ServeMode::Standalone, ServeMode::Writer] {
        let router = router(service().with_serve_mode(mode).unwrap());
        for (method, path, body) in [
            ("PUT", "/v1/vectors/1", r#"{"vector":[1.0]}"#),
            ("GET", "/v1/admin/serve-status", ""),
            ("GET", "/v1/vectors/1", ""),
        ] {
            let (status, headers, body) = call(&router, method, path, body).await;
            assert!(status.is_success(), "{body}");
            assert!(!headers.contains_key("x-vlite-epoch"));
            if path.ends_with("serve-status") {
                assert!(body["epoch"].is_null());
            }
        }
    }
    assert_eq!(gets.load(Ordering::SeqCst), 0);
}

#[test]
fn with_writer_fence_refuses_standalone_and_reader() {
    let (_, fence, _) = setup();
    for mode in [ServeMode::Standalone, ServeMode::Reader] {
        let mut service = service();
        Arc::get_mut(&mut service.serve).unwrap().mode = mode;
        let error = service.with_writer_fence(fence.clone()).err().unwrap();
        assert_eq!(
            error.to_string(),
            "writer authority needs --serve-mode writer"
        );
    }
}

#[test]
fn successor_shares_the_latched_fence() {
    let (_, fence, _) = setup();
    let service = writer(fence.clone(), false);
    let next = service
        .successor(Database::create_in_memory(1, crate::CreateOptions::default()).unwrap())
        .unwrap();
    fence.transition(FenceState::Fenced).unwrap();
    assert_eq!(next.writer_fence.state(), FenceState::Fenced);
    assert_eq!(next.writer_exit_code(), Some(1));
}

#[tokio::test]
async fn serving_fence_stamps_epoch_on_success_and_error() {
    let (_, fence, _) = setup();
    let epoch = fence.epoch().unwrap().to_string();
    let router = authenticated_router(
        writer(fence, false),
        BearerToken::new("writer-test-token-0123456789abcdef").unwrap(),
    );
    for path in ["/health", "/v1/vectors/1", "/ready"] {
        let (_, headers, _) = call(&router, "GET", path, "").await;
        assert_eq!(headers["x-vlite-epoch"], epoch);
    }
}

async fn mutation_checks(group: bool) {
    let (_, fence, gets) = setup();
    let router = router(writer(fence, group));
    for id in 1..=2 {
        let response = call(
            &router,
            "PUT",
            &format!("/v1/vectors/{id}"),
            r#"{"vector":[1.0]}"#,
        )
        .await;
        assert!(response.0.is_success(), "{:?}", response.2);
        assert_eq!(gets.load(Ordering::SeqCst), id * 2);
    }
}

#[tokio::test]
async fn mutation_reads_authority_before_commit_and_before_ack() {
    mutation_checks(false).await;
}

#[tokio::test]
async fn group_commit_mutation_is_gated() {
    mutation_checks(true).await;
}

#[tokio::test]
async fn unavailable_before_commit_refuses_without_mutation() {
    for group in [false, true] {
        let (memory, fence, _) = setup();
        let router = router(writer(fence, group));
        memory.fault(Op::Get, KEY, Fault::Refuse, 0, 1);
        let response = call(&router, "PUT", "/v1/vectors/1", r#"{"vector":[1.0]}"#).await;
        error(
            &response,
            StatusCode::SERVICE_UNAVAILABLE,
            "writer_authority_unavailable",
        );
        assert_eq!(
            call(&router, "GET", "/v1/vectors/1", "").await.0,
            StatusCode::NOT_FOUND
        );
    }
}

#[tokio::test]
async fn unavailable_after_commit_answers_write_outcome_unknown() {
    for group in [false, true] {
        let (memory, fence, gets) = setup();
        let service = writer(fence.clone(), group);
        let router = router(service.clone());
        memory.fault(Op::Get, KEY, Fault::Refuse, 1, 1);
        let response = call(&router, "PUT", "/v1/vectors/1", r#"{"vector":[1.0]}"#).await;
        error(&response, StatusCode::BAD_GATEWAY, "write_outcome_unknown");
        assert_eq!(gets.load(Ordering::SeqCst), 2);
        assert!(
            service
                .run_read(|db| Ok(db.get(1)?.is_some()))
                .await
                .unwrap()
        );
        assert_eq!(fence.exit_code(), Some(1));
    }
}

#[tokio::test]
async fn higher_epoch_fences_and_refuses_everything() {
    let (memory, fence, gets) = setup();
    let service = writer(fence.clone(), false);
    let router = router(service.clone());
    let stop = fence.stop_signal().unwrap();
    change(&memory);
    let response = call(&router, "PUT", "/v1/vectors/1", r#"{"vector":[1.0]}"#).await;
    error(
        &response,
        StatusCode::SERVICE_UNAVAILABLE,
        "writer_authority_unavailable",
    );
    for path in ["/v1/vectors/1", "/v1/database", "/ready"] {
        error(
            &call(&router, "GET", path, "").await,
            StatusCode::SERVICE_UNAVAILABLE,
            "writer_authority_unavailable",
        );
    }
    assert!(service.run_read(|_| Ok(())).await.is_err());
    assert!(*stop.borrow());
    assert_eq!(fence.exit_code(), Some(1));
    assert_eq!(gets.load(Ordering::SeqCst), 1);
    for path in ["/health", "/metrics", "/v1/admin/serve-status"] {
        let response = call(&router, "GET", path, "").await;
        assert!(response.0.is_success(), "{path}: {:?}", response.2);
    }
}

fn bypass_router<H, T>(service: Service, handler: H) -> Router
where
    H: axum::handler::Handler<T, Service>,
    T: 'static,
{
    Router::new()
        .route("/v1/batches", post(handler))
        .layer(middleware::from_fn_with_state(service.clone(), observe))
        .with_state(service)
}

#[tokio::test]
async fn fenced_latch_suppresses_success_without_receipt() {
    let (_, fence, _) = setup();
    let service = writer(fence.clone(), false);
    let router = bypass_router(service, move || async move {
        fence.transition(FenceState::Fenced).unwrap();
        StatusCode::OK
    });
    error(
        &call(&router, "POST", "/v1/batches", "").await,
        StatusCode::BAD_GATEWAY,
        "write_outcome_unknown",
    );
}

#[tokio::test]
async fn response_barrier_checks_routes_without_receipts() {
    let (memory, fence, gets) = setup();
    let router = bypass_router(writer(fence, false), || async { StatusCode::OK });
    assert!(
        call(&router, "POST", "/v1/batches", "")
            .await
            .0
            .is_success()
    );
    assert_eq!(gets.load(Ordering::SeqCst), 1);
    memory.fault(Op::Get, KEY, Fault::Refuse, 0, 1);
    error(
        &call(&router, "POST", "/v1/batches", "").await,
        StatusCode::BAD_GATEWAY,
        "write_outcome_unknown",
    );
}

#[tokio::test]
async fn fenced_latch_suppresses_success_with_receipt() {
    let (_, fence, gets) = setup();
    let service = writer(fence.clone(), false);
    let router = bypass_router(service.clone(), move || {
        let service = service.clone();
        let fence = fence.clone();
        async move {
            service.run(|_| Ok(())).await.unwrap();
            fence.transition(FenceState::Fenced).unwrap();
            StatusCode::OK
        }
    });
    error(
        &call(&router, "POST", "/v1/batches", "").await,
        StatusCode::BAD_GATEWAY,
        "write_outcome_unknown",
    );
    assert_eq!(gets.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn signal_drain_does_not_reopen_fenced_gate() {
    let (_, fence, _) = setup();
    let service = writer(fence.clone(), false);
    fence.transition(FenceState::Fenced).unwrap();
    service.begin_drain();
    assert_eq!(fence.state(), FenceState::Fenced);
    service.begin_writer_drain().unwrap();
    assert_eq!(fence.state(), FenceState::Draining);
    let router = router(service.clone());
    error(
        &call(&router, "GET", "/ready", "").await,
        StatusCode::SERVICE_UNAVAILABLE,
        "writer_authority_unavailable",
    );
    service.writer_stopped().unwrap();
    assert_eq!(fence.state(), FenceState::Stopped);
    assert_eq!(service.writer_exit_code(), Some(1));
}

#[tokio::test]
async fn validation_errors_are_preserved() {
    for group in [false, true] {
        let (_, fence, _) = setup();
        let router = router(writer(fence, group));
        let plain = router_for_validation();
        for body in [r#"{"vector":[]}"#, "{"] {
            let baseline = call(&plain, "PUT", "/v1/vectors/1", body).await;
            let response = call(&router, "PUT", "/v1/vectors/1", body).await;
            assert!(response.0.is_client_error());
            assert_eq!(response.0, baseline.0);
            assert_eq!(response.2["error"]["code"], baseline.2["error"]["code"]);
        }
    }
}

fn router_for_validation() -> Router {
    router(service())
}

#[tokio::test]
async fn read_routes_do_not_read_authority_while_serving() {
    let (_, fence, gets) = setup();
    let epoch = fence.epoch().unwrap();
    let router = router(writer(fence, false));
    for (method, path, body) in [
        ("GET", "/health", ""),
        ("GET", "/ready", ""),
        ("GET", "/v1/database", ""),
        ("GET", "/v1/admin/serve-status", ""),
        ("POST", "/v1/search", r#"{"vector":[1.0],"k":1}"#),
    ] {
        let response = call(&router, method, path, body).await;
        assert!(response.0.is_success(), "{path}: {:?}", response.2);
        if path.ends_with("serve-status") {
            assert_eq!(response.2["epoch"], epoch);
        }
    }
    assert_eq!(gets.load(Ordering::SeqCst), 0);
}
