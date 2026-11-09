//! Live grant reload pins that need crate-private state: the database write
//! lock, the request-lifetime pin, the storage file and the gRPC adoption.
//! Every expectation comes from the restart-free memory grants PRD
//! (`docs/projects/feature-memory-live-grants/feature-memory-live-grants.prd.md`,
//! sections 3.4 to 3.7 and TEST-005 to TEST-009), never from the reload code.

use std::sync::{Arc, atomic::Ordering};
use std::time::Duration;

use axum::{
    Extension, Router,
    body::Body,
    extract::DefaultBodyLimit,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::super::{MAX_JSON_BYTES, RequestId, Service, admin, credentials, guard};
use super::reload::{apply, reload};
use super::routes::Clock;
use crate::{CollectionSpec, CreateOptions, Database, FormatVersion};

const PRINCIPAL: &str = "abcdefghij27";
const GRAPH: &str = "notes_graph";
const EVIDENCE: &str = "notes_ev";
const FILE: &str = "bindings.json";
const DATABASE: &str = "pins.vlite";
/// How long these reloads wait for superseded grants (the test hook in place
/// of the 10 s `reserved::DRAIN_TIMEOUT`).
const DRAIN: Duration = Duration::from_millis(50);

struct Pins {
    router: Router,
    service: Service,
    /// Holds the bindings file and, for [`Pins::on_disk`], the database.
    directory: tempfile::TempDir,
}

/// The bindings file: binding `notes` at `generation`, whose one grant holds
/// exactly `scopes`.
fn bindings(generation: u64, scopes: Value) -> String {
    json!({"version": "memory-bindings-v1", "bindings": [{
        "binding": "notes", "tenant": "@root", "generation": generation, "state": "active",
        "graph": GRAPH, "evidence_collection": EVIDENCE,
        "schema_version": "memory-schema-v1", "ranking_version": "memory-ranking-v1",
        "digest_version": "memory-request-v1", "receipt_window_ms": 60_000,
        "grants": [{"principal": PRINCIPAL, "scopes": scopes}],
    }]})
    .to_string()
}

async fn answer(router: Router, request: Request<Body>) -> (u16, Value) {
    let response = router.oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn post_to(router: Router, operation: &str, body: Value) -> (u16, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(format!("/v1/memory/notes/{operation}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    answer(router, request).await
}

fn remember_body(key: &str) -> Value {
    json!({
        "scope": {"agent": "a"}, "idempotency_key": key, "deadline_ms": 5000,
        "items": [{
            "id": "e1", "kind": "entity", "content": "text", "observed_at_ms": "90",
            "source": {"source_id": "s1", "source_kind": "caller"},
        }],
    })
}

fn recall_body() -> Value {
    json!({"scope": {"agent": "a"}, "vector": [1, 0]})
}

impl Pins {
    /// A server over an in-memory database, started from a bindings file at
    /// generation 1 that grants agent `a`.
    fn new() -> Self {
        let db = Database::create_in_memory(
            2,
            CreateOptions {
                format: Some(FormatVersion::V5),
                ..Default::default()
            },
        )
        .unwrap();
        Self::start(db, tempfile::tempdir().unwrap())
    }

    /// The same over a database file, whose handle a pin can swap.
    fn on_disk() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let db = Database::create(directory.path().join(DATABASE), 2).unwrap();
        Self::start(db, directory)
    }

    /// Wired as the server wires it: the bindings read from the file, the
    /// startup checks and writes, then the file kept as the reload source.
    fn start(mut db: Database, directory: tempfile::TempDir) -> Self {
        db.create_graph(GRAPH).unwrap();
        db.create_collection(
            EVIDENCE,
            CollectionSpec {
                dimensions: 2,
                settings: Default::default(),
            },
        )
        .unwrap();
        let lineage = db.lineage().unwrap();
        let file = directory.path().join(FILE);
        std::fs::write(&file, bindings(1, json!([{"agent": "a"}]))).unwrap();
        let service = Service::new(db).with_bindings(admin::read_memory_bindings(&file).unwrap());
        super::startup_at(&service, 0).unwrap();
        let service = service.with_bindings_source(Some(file));
        let grant = credentials::Grant::for_user(
            PRINCIPAL.into(),
            "pins".into(),
            None,
            &[
                credentials::Op::Read,
                credentials::Op::Search,
                credentials::Op::Write,
                credentials::Op::GraphRead,
                credentials::Op::GraphWrite,
            ],
            lineage,
            u64::MAX,
        );
        let clock = Clock::default();
        clock.now_ms.store(100, Ordering::SeqCst);
        let router = super::routes::routes()
            .with_state(service.clone())
            .layer(Extension(admin::Role::Scoped(Arc::new(grant))))
            .layer(Extension(RequestId("pins".into())))
            .layer(Extension(clock))
            .layer(DefaultBodyLimit::max(MAX_JSON_BYTES));
        Self {
            router,
            service,
            directory,
        }
    }

    /// Rewrites the bindings file: generation `generation`, granting exactly
    /// `scopes`.
    fn write(&self, generation: u64, scopes: Value) {
        std::fs::write(
            self.directory.path().join(FILE),
            bindings(generation, scopes),
        )
        .unwrap();
    }

    async fn post(&self, operation: &str, body: Value) -> (u16, Value) {
        post_to(self.router.clone(), operation, body).await
    }

    fn sequence(&self) -> u64 {
        let owner = self.service.database.read().unwrap();
        owner.as_ref().unwrap().sequence
    }
}

/// TEST-005: a `remember` admitted under generation 1 that reaches the write
/// lock only after a reload advanced the record is refused by the generation
/// recheck and writes nothing.
#[tokio::test]
async fn a_remember_admitted_before_a_reload_that_advances_the_record_writes_nothing() {
    let pins = Pins::new();
    pins.write(2, json!([{"agent": "a"}, {"agent": "b"}]));
    let read = admin::read_memory_bindings(&pins.directory.path().join(FILE)).unwrap();
    let idle = pins.service.workers.available_permits();
    // A second thread holds the database write lock until the remember has
    // been admitted, applies the reload under that guard as the reload's
    // worker does, and lets go.
    let (locked_tx, locked_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let service = pins.service.clone();
    let holder = std::thread::spawn(move || {
        let mut owner = service.database.write().unwrap();
        locked_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        let db = owner.as_mut().unwrap();
        let applied = apply(db, &service, read, crate::current_time_ms()).unwrap();
        (applied, db.sequence)
    });
    locked_rx.recv().unwrap();
    let request = tokio::spawn(post_to(pins.router.clone(), "remember", remember_body("k")));
    while pins.service.workers.available_permits() == idle {
        tokio::task::yield_now().await;
    }
    release_tx.send(()).unwrap();
    let (applied, reloaded) = holder.join().unwrap();
    assert_eq!(applied, (1, 1), "one binding, and it changed");
    let (status, stale) = request.await.unwrap();
    assert_eq!(status, 409, "{stale}");
    assert_eq!(
        stale["error"]["code"], "memory_capability_unavailable",
        "{stale}"
    );
    assert_eq!(
        stale["error"]["details"]["feature"], "binding_generation",
        "{stale}"
    );
    assert_eq!(
        pins.sequence(),
        reloaded,
        "the refused remember wrote nothing"
    );
    let (status, served) = pins.post("remember", remember_body("k")).await;
    assert_eq!(
        status, 200,
        "a remember admitted after the reload: {served}"
    );
}

/// TEST-006: a `recall` that consulted the grants before the install keeps
/// them for the rest of the request; the reload reports that it did not drain
/// while that request lasts, and a reload of the unchanged file afterwards
/// changes nothing and drains.
#[tokio::test]
async fn a_recall_pinned_before_the_install_completes_under_the_old_grants() {
    let pins = Pins::new();
    let (pinned_tx, pinned_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let router = pins.router.clone();
    // One request as the pipeline runs it: everything inside shares its
    // context, so both recalls decide with the view the first one pinned.
    let held = tokio::spawn(guard::track_hidden(async move {
        let first = post_to(router.clone(), "recall", recall_body()).await;
        pinned_tx.send(()).unwrap();
        release_rx.await.unwrap();
        let second = post_to(router, "recall", recall_body()).await;
        (first, second)
    }));
    pinned_rx.await.unwrap();
    pins.write(2, json!([{"agent": "b"}]));
    let revoked = reload(&pins.service, DRAIN).await.unwrap().0;
    assert_eq!(
        revoked,
        json!({"bindings": 1, "changed": 1, "drained": false})
    );
    let ((status, fresh), _) = guard::track_hidden(pins.post("recall", recall_body())).await;
    assert_eq!(status, 403, "a new request uses the new grants: {fresh}");
    assert_eq!(fresh["error"]["code"], "forbidden_scope", "{fresh}");
    release_tx.send(()).unwrap();
    let ((first, second), _) = held.await.unwrap();
    assert_eq!(first.0, 200, "{}", first.1);
    assert_eq!(
        second.0, 200,
        "the pinned request keeps agent a: {}",
        second.1
    );
    let unchanged = reload(&pins.service, DRAIN).await.unwrap().0;
    assert_eq!(
        unchanged,
        json!({"bindings": 1, "changed": 0, "drained": true})
    );
}

/// TEST-007: a reload whose provisioning cannot commit answers 503
/// `commit_indeterminate`, counts the failure and keeps the view in force.
#[tokio::test]
async fn a_reload_that_cannot_commit_answers_503_and_keeps_the_served_view() {
    let pins = Pins::on_disk();
    let before = pins.service.memory_bindings.current();
    pins.write(2, json!([{"agent": "a"}, {"agent": "b"}]));
    {
        // A read-only handle: the scope `b` must be provisioned, and its
        // commit fails once it has started.
        let mut owner = pins.service.database.write().unwrap();
        owner.as_mut().unwrap().file = crate::LockedFile::disk(
            std::fs::File::open(pins.directory.path().join(DATABASE)).unwrap(),
        );
    }
    let failed = reload(&pins.service, DRAIN).await.unwrap_err();
    assert_eq!(failed.status, StatusCode::SERVICE_UNAVAILABLE, "{failed:?}");
    assert_eq!(failed.code, "commit_indeterminate", "{failed:?}");
    assert!(
        Arc::ptr_eq(&before, &pins.service.memory_bindings.current()),
        "the view in force is unchanged"
    );
    assert_eq!(
        pins.service.metrics.commit_failures.load(Ordering::Relaxed),
        1
    );
}

/// TEST-008: a reload that starts while another one runs answers 409
/// `memory_reload_in_progress` at once; the first one completes.
#[tokio::test]
async fn a_second_concurrent_reload_answers_409_in_progress() {
    let pins = Pins::new();
    pins.write(2, json!([{"agent": "a"}, {"agent": "b"}]));
    let idle = pins.service.workers.available_permits();
    // A second thread holds the database write lock, so the first reload
    // waits for it while it holds the reload lock.
    let (locked_tx, locked_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let service = pins.service.clone();
    let holder = std::thread::spawn(move || {
        let _owner = service.database.write().unwrap();
        locked_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    locked_rx.recv().unwrap();
    let service = pins.service.clone();
    let first = tokio::spawn(async move { reload(&service, DRAIN).await.map(|answer| answer.0) });
    while pins.service.workers.available_permits() == idle {
        tokio::task::yield_now().await;
    }
    let second = tokio::time::timeout(Duration::from_secs(10), reload(&pins.service, DRAIN))
        .await
        .expect("the second reload answers without waiting for the first")
        .unwrap_err();
    assert_eq!(second.status, StatusCode::CONFLICT, "{second:?}");
    assert_eq!(second.code, "memory_reload_in_progress", "{second:?}");
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    let first = first.await.unwrap().unwrap();
    assert_eq!(first, json!({"bindings": 1, "changed": 1, "drained": true}));
}

#[cfg(feature = "grpc")]
async fn admin_call(routes: &Router, method: &str, uri: &str) -> (u16, Value) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    answer(routes.clone(), request).await
}

/// TEST-009: once a gRPC router adopted the service (`grpc_built`), a reload
/// answers 409 `memory_reload_unavailable` and the capabilities stop offering
/// it.
#[cfg(feature = "grpc")]
#[tokio::test]
async fn a_reload_after_a_grpc_router_was_built_is_unavailable() {
    let pins = Pins::new();
    let routes = admin::routes()
        .with_state(pins.service.clone())
        .layer(Extension(admin::Role::Admin));
    let (status, offered) = admin_call(&routes, "GET", "/v1/capabilities").await;
    assert_eq!(status, 200, "{offered}");
    assert_eq!(offered["features"]["memory_bindings_reload"], true);
    let before = pins.service.memory_bindings.current();
    let mut adopted = pins.service.clone();
    adopted.adopt_published();
    pins.write(2, json!([{"agent": "a"}, {"agent": "b"}]));
    let (status, refused) = admin_call(&routes, "POST", "/v1/admin/memory/bindings/reload").await;
    assert_eq!(status, 409, "{refused}");
    assert_eq!(
        refused["error"]["code"], "memory_reload_unavailable",
        "{refused}"
    );
    assert!(
        Arc::ptr_eq(&before, &pins.service.memory_bindings.current()),
        "the view in force is unchanged"
    );
    let (_, withdrawn) = admin_call(&routes, "GET", "/v1/capabilities").await;
    assert_eq!(withdrawn["features"]["memory_bindings_reload"], false);
}
