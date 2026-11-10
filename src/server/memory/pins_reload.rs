//! Live grant reload pins that need crate-private state: the database write
//! lock, the request-lifetime pin, the storage file, the gRPC adoption and the
//! audit log. Every expectation comes from the restart-free memory grants PRD
//! (`docs/projects/feature-memory-live-grants/feature-memory-live-grants.prd.md`,
//! sections 3.4 to 3.7, TEST-005 to TEST-009 and TEST-019), never from the
//! reload code.

use std::path::Path;
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

use super::super::{
    ApiError, MAX_JSON_BYTES, RequestId, Service, admin, audit, credentials, guard, reserved,
};
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

/// The audit event of a successful reload of the one binding: counts and the
/// outcome only.
fn reloaded(changed: u64, drained: bool) -> Value {
    json!({
        "event": "memory_bindings_reloaded", "bindings": 1, "changed": changed,
        "drained": drained, "outcome": "ok",
    })
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

    /// [`Pins::new`] whose reloads append to an audit log next to the
    /// bindings file.
    fn audited() -> Self {
        let mut pins = Self::new();
        let directory = pins.directory.path().to_owned();
        pins.audit_in(&directory);
        pins
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

    /// Gives the service that reloads run with an audit log in `directory`.
    /// The router keeps the service it was built with, which audits nothing,
    /// so the log holds only what reloads append.
    fn audit_in(&mut self, directory: &Path) {
        let mut state = admin::AdminState::default();
        state.audit_log = Some(Arc::new(audit::Log::open(directory).unwrap()));
        self.service.admin = Arc::new(state);
    }

    /// The entries of the [`Pins::audited`] log, each without the time the
    /// log adds to every entry. None names a binding, tenant, principal,
    /// scope or path.
    fn audit_entries(&self) -> Vec<Value> {
        let text = std::fs::read_to_string(self.directory.path().join("audit.jsonl")).unwrap();
        let directory = self.directory.path().file_name().unwrap().to_str().unwrap();
        for named in ["notes", "@root", PRINCIPAL, "agent", FILE, directory] {
            assert!(!text.contains(named), "the audit log names {named}: {text}");
        }
        text.lines()
            .map(|line| {
                let mut entry: Value = serde_json::from_str(line).unwrap();
                let at = entry.as_object_mut().unwrap().remove("unix_ms");
                assert!(at.is_some_and(|at| at.is_u64()), "{line}");
                entry
            })
            .collect()
    }

    /// A reload that starts now is refused: one still runs.
    async fn assert_reloading(&self) {
        let refused = reload(&self.service, DRAIN)
            .await
            .map(|answer| answer.0)
            .expect_err("a concurrent reload is refused while one runs");
        assert_eq!(refused.status, StatusCode::CONFLICT, "{refused:?}");
        assert_eq!(refused.code, "memory_reload_in_progress", "{refused:?}");
    }

    /// Waits until no reload holds the reload lock, which a reload keeps
    /// until it has reported.
    async fn finished(&self) {
        while self.service.memory_bindings.try_reload().is_none() {
            tokio::task::yield_now().await;
        }
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

/// TEST-019: a reload whose caller goes away while its apply waits for the
/// database write lock completes without the caller: a concurrent reload is
/// refused meanwhile, the new grants are installed, and one counts-only audit
/// event is appended.
#[tokio::test]
async fn a_reload_dropped_while_its_apply_waits_for_the_lock_completes_and_audits_once() {
    let pins = Pins::audited();
    pins.write(2, json!([{"agent": "a"}, {"agent": "b"}]));
    let before = pins.service.memory_bindings.current();
    let idle = pins.service.workers.available_permits();
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
    let caller = tokio::spawn(async move {
        reload(&service, reserved::DRAIN_TIMEOUT)
            .await
            .map(|answer| answer.0)
    });
    // The reload's worker holds a permit: the apply waits for the lock.
    while pins.service.workers.available_permits() == idle {
        tokio::task::yield_now().await;
    }
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    pins.assert_reloading().await;
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    pins.finished().await;
    assert!(
        !Arc::ptr_eq(&before, &pins.service.memory_bindings.current()),
        "the new grants are in force"
    );
    assert_eq!(pins.audit_entries(), [reloaded(1, true)]);
}

/// TEST-019: a reload whose caller goes away after the install, while a
/// request pinned to the superseded grants holds up the drain, keeps the
/// reload lock until the drain ends, then appends one counts-only audit event
/// that reports the drain.
#[tokio::test]
async fn a_reload_dropped_while_it_drains_keeps_the_lock_and_audits_once() {
    let pins = Pins::audited();
    let (pinned_tx, pinned_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let router = pins.router.clone();
    let held = tokio::spawn(guard::track_hidden(async move {
        let recalled = post_to(router, "recall", recall_body()).await;
        pinned_tx.send(()).unwrap();
        release_rx.await.unwrap();
        recalled
    }));
    pinned_rx.await.unwrap();
    let before = pins.service.memory_bindings.current();
    let idle = pins.service.workers.available_permits();
    pins.write(2, json!([{"agent": "b"}]));
    let service = pins.service.clone();
    let caller = tokio::spawn(async move {
        reload(&service, reserved::DRAIN_TIMEOUT)
            .await
            .map(|answer| answer.0)
    });
    // Installed and off the worker: what remains is the drain, which waits
    // for the pinned request.
    while Arc::ptr_eq(&before, &pins.service.memory_bindings.current())
        || pins.service.workers.available_permits() != idle
    {
        tokio::task::yield_now().await;
    }
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    pins.assert_reloading().await;
    release_tx.send(()).unwrap();
    let ((status, recalled), _) = held.await.unwrap();
    assert_eq!(status, 200, "{recalled}");
    pins.finished().await;
    assert_eq!(pins.audit_entries(), [reloaded(1, true)]);
}

/// TEST-019: each successful reload appends exactly one audit event with only
/// `event`, `bindings`, `changed`, `drained` and `outcome` (and the time the
/// log adds to every entry). Run as the pipeline runs it, the reload's own
/// request pins nothing, so its drain never waits for itself.
#[tokio::test]
async fn each_successful_reload_appends_one_counts_only_audit_event() {
    let pins = Pins::audited();
    pins.write(2, json!([{"agent": "a"}, {"agent": "b"}]));
    let (changed, hidden) = guard::track_hidden(reload(&pins.service, DRAIN)).await;
    assert_eq!(
        changed.unwrap().0,
        json!({"bindings": 1, "changed": 1, "drained": true})
    );
    assert!(!hidden, "a reload hides nothing");
    let unchanged = reload(&pins.service, DRAIN).await.unwrap().0;
    assert_eq!(
        unchanged,
        json!({"bindings": 1, "changed": 0, "drained": true})
    );
    assert_eq!(pins.audit_entries(), [reloaded(1, true), reloaded(0, true)]);
    assert_eq!(
        pins.service.metrics.audit_failures.load(Ordering::Relaxed),
        0
    );
}

/// The audit event is best effort (PRD section 3.4, step 6): one that cannot
/// be appended is counted, and the reload still answers.
#[tokio::test]
async fn a_reload_whose_audit_event_cannot_be_appended_counts_it_and_answers() {
    let mut pins = Pins::new();
    // A log at the 16 MiB at which it rotates (`src/server/audit.rs`), whose
    // rotated file is a directory: the next append must rotate, and cannot.
    let directory = pins.directory.path().join("audit");
    std::fs::create_dir_all(directory.join("audit.1.jsonl")).unwrap();
    std::fs::File::create(directory.join("audit.jsonl"))
        .unwrap()
        .set_len(16 * 1024 * 1024)
        .unwrap();
    pins.audit_in(&directory);
    pins.write(2, json!([{"agent": "a"}, {"agent": "b"}]));
    let answer = reload(&pins.service, DRAIN).await.unwrap().0;
    assert_eq!(
        answer,
        json!({"bindings": 1, "changed": 1, "drained": true})
    );
    assert_eq!(
        pins.service.metrics.audit_failures.load(Ordering::Relaxed),
        1
    );
}

/// The read-only preflight and status of a reload
/// (`docs/projects/feature-memory-preflight/feature-memory-preflight.prd.md`,
/// sections 3.2 to 3.4 and TEST-A01 to TEST-A08): every expectation comes
/// from that PRD and from what reload itself answers on the same state.
const PREFLIGHT: &str = "/v1/admin/memory/bindings/preflight";
const STATUS: &str = "/v1/admin/memory/bindings/status";

/// The bindings file of [`bindings`] with `field` of the one binding set to
/// `value`.
fn edited(generation: u64, scopes: Value, field: &str, value: Value) -> String {
    let mut file: Value = serde_json::from_str(&bindings(generation, scopes)).unwrap();
    file["bindings"][0][field] = value;
    file.to_string()
}

/// Reload's error as preflight reports it.
fn refusal(error: &ApiError) -> Value {
    json!({"code": error.code, "message": error.message})
}

/// The admin routes over `service`, called as an administrator.
fn admin_routes(service: &Service) -> Router {
    admin::routes()
        .with_state(service.clone())
        .layer(Extension(admin::Role::Admin))
}

/// Calls `uri` as an administrator, expects 200, and checks that the answer
/// names no binding, tenant, principal, scope, graph, collection or path.
async fn admin_ok(service: &Service, method: &str, uri: &str, directory: &Path) -> Value {
    let (status, body) = admin_call(&admin_routes(service), method, uri).await;
    assert_eq!(status, 200, "{body}");
    let text = body.to_string();
    let directory = directory.file_name().unwrap().to_str().unwrap();
    for named in [
        "notes", "@root", PRINCIPAL, "agent", GRAPH, EVIDENCE, FILE, directory,
    ] {
        assert!(!text.contains(named), "{uri} names {named}: {text}");
    }
    body
}

impl Pins {
    fn write_text(&self, text: &str) {
        std::fs::write(self.directory.path().join(FILE), text).unwrap();
    }

    async fn preflight(&self) -> Value {
        admin_ok(&self.service, "POST", PREFLIGHT, self.directory.path()).await
    }

    async fn status(&self) -> Value {
        admin_ok(&self.service, "GET", STATUS, self.directory.path()).await
    }

    /// A preflight that answers `outcome` with reload's own refusal, and
    /// changes nothing: the same view, no write (scopes or records) and no
    /// audit entry. Reload then refuses with that same code and message.
    async fn assert_refused_alike(&self, outcome: &str) {
        let before = self.service.memory_bindings.current();
        let sequence = self.sequence();
        let audited = self.audit_entries().len();
        let predicted = self.preflight().await;
        assert!(Arc::ptr_eq(
            &before,
            &self.service.memory_bindings.current()
        ));
        assert_eq!(self.sequence(), sequence, "a preflight writes nothing");
        assert_eq!(
            self.audit_entries().len(),
            audited,
            "a preflight audits nothing"
        );
        let refused = reload(&self.service, DRAIN)
            .await
            .map(|answer| answer.0)
            .expect_err("reload refuses");
        assert_eq!(refused.status, StatusCode::CONFLICT, "{refused:?}");
        assert_eq!(
            predicted,
            json!({
                "outcome": outcome, "bindings": 1, "changed": null, "records": null,
                "error": refusal(&refused),
            })
        );
    }
}

/// TEST-A01: preflight predicts a grants-only reload with reload's own counts,
/// writes nothing, and afterwards reports the file unchanged.
#[tokio::test]
async fn a_preflight_predicts_a_grants_only_reload_and_then_reports_unchanged() {
    let pins = Pins::new();
    pins.write(2, json!([{"agent": "a"}, {"agent": "b"}]));
    let before = pins.service.memory_bindings.current();
    let sequence = pins.sequence();
    let predicted = pins.preflight().await;
    assert_eq!(
        predicted,
        json!({"outcome": "reload", "bindings": 1, "changed": 1, "records": 1, "error": null})
    );
    assert!(Arc::ptr_eq(
        &before,
        &pins.service.memory_bindings.current()
    ));
    assert_eq!(pins.sequence(), sequence, "a preflight writes nothing");
    let reloaded = reload(&pins.service, DRAIN).await.unwrap().0;
    assert_eq!(reloaded["changed"], predicted["changed"]);
    assert_eq!(reloaded["bindings"], predicted["bindings"]);
    assert_eq!(
        pins.preflight().await,
        json!({"outcome": "unchanged", "bindings": 1, "changed": 0, "records": 0, "error": null})
    );
}

/// TEST-A02: for every refusal, preflight reports reload's own 409 code and
/// message on the same state, and changes nothing.
#[tokio::test]
async fn a_preflight_reports_each_refusal_as_reload_does_and_changes_nothing() {
    let pins = Pins::audited();
    let scopes = || json!([{"agent": "a"}, {"agent": "b"}]);
    pins.write_text(&edited(2, scopes(), "binding", json!("renamed")));
    pins.assert_refused_alike("restart_required").await;
    pins.write_text(&edited(
        2,
        scopes(),
        "evidence_collection",
        json!("other_ev"),
    ));
    pins.assert_refused_alike("restart_required").await;
    pins.write_text(&edited(2, scopes(), "receipt_window_ms", json!(120_000)));
    pins.assert_refused_alike("restart_required").await;
    // The same generation with other grants: a refusal of the records plan.
    pins.write(1, scopes());
    pins.assert_refused_alike("refused").await;
    // A bound resource missing from the database. The bound graph holds the
    // binding record, so it cannot be dropped; the evidence collection can.
    pins.write(2, scopes());
    {
        let mut owner = pins.service.database.write().unwrap();
        owner.as_mut().unwrap().drop_collection(EVIDENCE).unwrap();
    }
    pins.assert_refused_alike("refused").await;
    assert!(
        pins.audit_entries().is_empty(),
        "nothing reloaded or audited"
    );
}

/// TEST-A03: with no bindings file preflight is unavailable; a missing,
/// directory or invalid file is `invalid` with reload's own 422 message, and
/// status reports it `unreadable`. No answer names the path.
#[tokio::test]
async fn a_preflight_without_a_readable_file_is_unavailable_or_invalid() {
    let db = Database::create_in_memory(2, CreateOptions::default()).unwrap();
    let unconfigured = Service::new(db);
    let nowhere = tempfile::tempdir().unwrap();
    let predicted = admin_ok(&unconfigured, "POST", PREFLIGHT, nowhere.path()).await;
    let refused = reload(&unconfigured, DRAIN)
        .await
        .map(|answer| answer.0)
        .expect_err("reload is unavailable");
    assert_eq!(refused.code, "memory_reload_unavailable", "{refused:?}");
    assert_eq!(
        predicted,
        json!({
            "outcome": "unavailable", "bindings": null, "changed": null, "records": null,
            "error": refusal(&refused),
        })
    );
    assert_eq!(
        admin_ok(&unconfigured, "GET", STATUS, nowhere.path()).await,
        json!({
            "configured": false, "reloadable": false, "bindings": 0, "file": "none",
            "drained": true,
        })
    );

    let pins = Pins::new();
    let file = pins.directory.path().join(FILE);
    let unreadable: [&dyn Fn(); 3] = [
        &|| std::fs::remove_file(&file).unwrap(),
        &|| std::fs::create_dir(&file).unwrap(),
        &|| {
            std::fs::remove_dir(&file).unwrap();
            std::fs::write(&file, "{").unwrap();
        },
    ];
    for make in unreadable {
        make();
        let predicted = pins.preflight().await;
        let refused = reload(&pins.service, DRAIN)
            .await
            .map(|answer| answer.0)
            .expect_err("reload refuses the file");
        assert_eq!(
            refused.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{refused:?}"
        );
        assert_eq!(
            predicted,
            json!({
                "outcome": "invalid", "bindings": null, "changed": null, "records": null,
                "error": refusal(&refused),
            })
        );
        assert_eq!(predicted["error"]["code"], "invalid_input");
        assert_eq!(pins.status().await["file"], "unreadable");
    }
}

/// TEST-A05: preflight is advisory. A file changed after it said `reload`
/// gets reload's own verdict, and nothing is installed.
#[tokio::test]
async fn a_file_changed_after_a_preflight_gets_reloads_own_verdict() {
    let pins = Pins::new();
    pins.write(2, json!([{"agent": "a"}, {"agent": "b"}]));
    assert_eq!(pins.preflight().await["outcome"], "reload");
    pins.write_text(&edited(
        2,
        json!([{"agent": "b"}]),
        "binding",
        json!("renamed"),
    ));
    let before = pins.service.memory_bindings.current();
    let refused = reload(&pins.service, DRAIN)
        .await
        .map(|answer| answer.0)
        .expect_err("reload refuses the renamed binding");
    assert_eq!(refused.status, StatusCode::CONFLICT, "{refused:?}");
    assert_eq!(refused.code, "memory_restart_required", "{refused:?}");
    assert!(Arc::ptr_eq(
        &before,
        &pins.service.memory_bindings.current()
    ));
}

/// TEST-A06: neither route touches the reload lock. A held reload lock does
/// not change their answers, and a reload issued while a preflight waits for
/// the database lock completes rather than answering
/// `memory_reload_in_progress`; the preflight reports the state before or
/// after it.
#[tokio::test]
async fn preflight_and_status_neither_take_nor_heed_the_reload_lock() {
    let pins = Pins::new();
    pins.write(2, json!([{"agent": "a"}, {"agent": "b"}]));
    let free = (pins.preflight().await, pins.status().await);
    let held = pins.service.memory_bindings.try_reload().unwrap();
    assert_eq!((pins.preflight().await, pins.status().await), free);
    drop(held);

    let idle = pins.service.workers.available_permits();
    let (locked_tx, locked_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let service = pins.service.clone();
    let holder = std::thread::spawn(move || {
        let _owner = service.database.write().unwrap();
        locked_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    locked_rx.recv().unwrap();
    let routes = admin_routes(&pins.service);
    let parked = tokio::spawn(async move { admin_call(&routes, "POST", PREFLIGHT).await });
    while pins.service.workers.available_permits() == idle {
        tokio::task::yield_now().await;
    }
    let service = pins.service.clone();
    let reloading =
        tokio::spawn(async move { reload(&service, DRAIN).await.map(|answer| answer.0) });
    while pins.service.workers.available_permits() + 2 > idle {
        tokio::task::yield_now().await;
    }
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    assert_eq!(
        reloading.await.unwrap().unwrap(),
        json!({"bindings": 1, "changed": 1, "drained": true})
    );
    let (status, parked) = parked.await.unwrap();
    assert_eq!(status, 200, "{parked}");
    let after =
        json!({"outcome": "unchanged", "bindings": 1, "changed": 0, "records": 0, "error": null});
    assert!(parked == free.0 || parked == after, "{parked}");
}

/// TEST-A07: preflight under a saturated worker queue answers reload's
/// read-path 503 `busy`; status takes no database lock and still answers.
#[tokio::test]
async fn a_preflight_under_a_full_queue_is_busy_and_status_still_answers() {
    let pins = Pins::new();
    let all = pins.service.workers.available_permits();
    let _saturated = pins
        .service
        .workers
        .clone()
        .try_acquire_many_owned(u32::try_from(all).unwrap())
        .unwrap();
    let (status, busy) = admin_call(&admin_routes(&pins.service), "POST", PREFLIGHT).await;
    assert_eq!(status, 503, "{busy}");
    assert_eq!(busy["error"]["code"], "busy", "{busy}");
    assert_eq!(
        pins.status().await,
        json!({
            "configured": true, "reloadable": true, "bindings": 1, "file": "agrees",
            "drained": true,
        })
    );
}

/// TEST-A08: a request pinned to the grants a reload superseded keeps status
/// at `drained:false` until it finishes.
#[tokio::test]
async fn status_is_undrained_while_a_superseded_view_is_pinned() {
    let pins = Pins::new();
    let (pinned_tx, pinned_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let router = pins.router.clone();
    let held = tokio::spawn(guard::track_hidden(async move {
        let recalled = post_to(router, "recall", recall_body()).await;
        pinned_tx.send(()).unwrap();
        release_rx.await.unwrap();
        recalled
    }));
    pinned_rx.await.unwrap();
    pins.write(2, json!([{"agent": "b"}]));
    assert_eq!(pins.status().await["file"], "differs");
    let reloaded = reload(&pins.service, DRAIN).await.unwrap().0;
    assert_eq!(reloaded["drained"], false);
    let undrained = pins.status().await;
    assert_eq!(undrained["drained"], false, "{undrained}");
    assert_eq!(undrained["file"], "agrees", "{undrained}");
    release_tx.send(()).unwrap();
    let ((status, recalled), _) = held.await.unwrap();
    assert_eq!(status, 200, "{recalled}");
    assert_eq!(pins.status().await["drained"], true);
}

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

/// TEST-A04 (feature-memory-preflight PRD): once a gRPC router adopted the
/// service, preflight reports reload's AFTER_GRPC refusal with no counts, and
/// status stops calling it reloadable, as the capabilities do.
#[cfg(feature = "grpc")]
#[tokio::test]
async fn a_preflight_after_a_grpc_router_was_built_is_unavailable() {
    let pins = Pins::new();
    let mut adopted = pins.service.clone();
    adopted.adopt_published();
    pins.write(2, json!([{"agent": "a"}, {"agent": "b"}]));
    let predicted = pins.preflight().await;
    let refused = reload(&pins.service, DRAIN)
        .await
        .map(|answer| answer.0)
        .expect_err("reload is unavailable");
    assert_eq!(refused.code, "memory_reload_unavailable", "{refused:?}");
    assert_eq!(
        predicted,
        json!({
            "outcome": "unavailable", "bindings": null, "changed": null, "records": null,
            "error": refusal(&refused),
        })
    );
    let status = pins.status().await;
    assert_eq!(status["reloadable"], false, "{status}");
    let (_, offered) = admin_call(&admin_routes(&pins.service), "GET", "/v1/capabilities").await;
    assert_eq!(
        offered["features"]["memory_bindings_reload"],
        status["reloadable"]
    );
}
