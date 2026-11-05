//! Consolidate pins that need the `Clock` seam or the credential's collection
//! incarnation (mutation closure for the adapter). Expectations come from the
//! serving memory contract (sections 3 and 6), the adapter mapping (section
//! 2) and the frozen C1 contract (section 6): a spent budget is a 504, and a
//! credential for a dropped evidence collection is refused first. HTTP-level
//! pins are in `tests/server_memory_consolidate.rs`.

use std::sync::{Arc, atomic::Ordering};

use axum::{
    Extension, Router,
    body::Body,
    extract::DefaultBodyLimit,
    http::{Request, header},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::super::{MAX_JSON_BYTES, RequestId, Service, admin, credentials};
use super::routes::Clock;
use crate::{CollectionSpec, CreateOptions, Database, FormatVersion};

const PRINCIPAL: &str = "abcdefghij27";
const BINDINGS: &str = r#"{"version":"memory-bindings-v1","bindings":[{"binding":"notes","tenant":"@root","generation":1,"state":"active","graph":"notes_graph","evidence_collection":"notes_ev","schema_version":"memory-schema-v1","ranking_version":"memory-ranking-v1","digest_version":"memory-request-v1","receipt_window_ms":86400000,"grants":[{"principal":"abcdefghij27","scopes":[{"agent":"a"}]}]}]}"#;
const CONSOLIDATE: &str = "/v1/memory/notes/consolidate";
const REMEMBER: &str = "/v1/memory/notes/remember";

fn spec() -> CollectionSpec {
    CollectionSpec {
        dimensions: 2,
        settings: Default::default(),
    }
}

struct Rig {
    router: Router,
    clock: Clock,
    service: Service,
}

impl Rig {
    /// With `named`, the credential covers the evidence collection by name
    /// and ID, as an issued scoped credential does, instead of every
    /// collection. The engine clock starts at 100 ms.
    fn new(named: bool) -> Self {
        let mut db = Database::create_in_memory(
            2,
            CreateOptions {
                format: Some(FormatVersion::V5),
                ..Default::default()
            },
        )
        .unwrap();
        db.create_graph("notes_graph").unwrap();
        db.create_collection("notes_ev", spec()).unwrap();
        let lineage = db.lineage().unwrap();
        let evidence = db.resolve("notes_ev").unwrap();
        let bindings = crate::memory::binding::parse(BINDINGS.as_bytes()).unwrap();
        let service = Service::new(db).with_bindings(super::reserved::Bindings::new(bindings));
        super::startup(&service).unwrap();
        let mut grant = credentials::Grant::for_user(
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
        if named {
            grant.all = false;
            grant.collections = vec![("notes_ev".into(), evidence)];
        }
        let clock = Clock::default();
        clock.now_ms.store(100, Ordering::SeqCst);
        let router = super::routes::routes()
            .with_state(service.clone())
            .layer(Extension(admin::Role::Scoped(Arc::new(grant))))
            .layer(Extension(RequestId("pins".into())))
            .layer(Extension(clock.clone()))
            .layer(DefaultBodyLimit::max(MAX_JSON_BYTES));
        Self {
            router,
            clock,
            service,
        }
    }

    /// The elapsed request budget the next requests report.
    fn elapse(&self, elapsed_ms: u64) {
        self.clock.elapsed_ms.store(elapsed_ms, Ordering::SeqCst);
    }

    async fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        let request = Request::builder()
            .method("POST")
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(body).unwrap()))
            .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    /// One entity `m1` at revision 1.
    async fn seed(&self) {
        let body = json!({
            "scope": {"agent": "a"}, "idempotency_key": "seed", "items": [{
                "id": "m1", "kind": "entity", "content": "source m1",
                "observed_at_ms": "90",
                "source": {"source_id": "origin", "source_kind": "caller"}
            }]
        });
        let (status, seeded) = self.post(REMEMBER, &body).await;
        assert_eq!(status, 200, "{seeded}");
    }

    /// The evidence collection dropped and created again: same name, new ID.
    fn recreate_evidence(&self) {
        let mut owner = self.service.database.write().unwrap();
        let db = owner.as_mut().unwrap();
        db.drop_collection("notes_ev").unwrap();
        db.create_collection("notes_ev", spec()).unwrap();
    }
}

/// A valid request whose key and summary are named after `name`.
fn consolidate(name: &str) -> Value {
    json!({
        "scope": {"agent": "a"},
        "idempotency_key": name,
        "inputs": [{"id": "m1", "revision": "1"}],
        "output": {
            "id": name,
            "kind": "episodic",
            "content": format!("summary {name}"),
            "observed_at_ms": "90",
            "source": {"source_id": "s2", "source_kind": "caller", "transformation_id": "t1"}
        }
    })
}

/// Mapping section 2: a spent budget (`DeadlineExceeded`) is 504
/// `deadline_exceeded`, never an input error. The budget is total elapsed
/// time, so it expires exactly when the elapsed time reaches it (C1 section
/// 6: `C >= deadline`), and nothing is written by an expired request.
#[tokio::test]
async fn a_spent_budget_is_a_504_and_writes_nothing() {
    let rig = Rig::new(false);
    rig.seed().await;
    for (budget, name) in [(None, "default"), (Some(5), "tight"), (Some(5000), "loose")] {
        let mut body = consolidate(name);
        let budget_ms = budget.unwrap_or(250);
        if let Some(budget) = budget {
            body["deadline_ms"] = json!(budget);
        }
        for elapsed in [budget_ms, budget_ms + 750] {
            rig.elapse(elapsed);
            let (status, error) = rig.post(CONSOLIDATE, &body).await;
            assert_eq!(
                (status, &error["error"]["code"]),
                (504, &json!("deadline_exceeded")),
                "{name} after {elapsed} ms: {error}"
            );
        }
        // One millisecond left is served, so the 504s wrote nothing.
        rig.elapse(budget_ms - 1);
        let (status, done) = rig.post(CONSOLIDATE, &body).await;
        assert_eq!(status, 200, "{name}: {done}");
        assert_eq!(done["replayed"], false, "{name}");

        // A live matching replay precedes the deadline check.
        rig.elapse(budget_ms + 750);
        let (status, again) = rig.post(CONSOLIDATE, &body).await;
        assert_eq!(status, 200, "{name}: {again}");
        assert_eq!(again["replayed"], true, "{name}");
        assert_eq!(again["receipt"], done["receipt"], "{name}");
    }
}

/// C1 sections 2 and 6 and mapping section 2: an observation later than the
/// execution clock is 422 `invalid_input` (`FutureObservation`), one at the
/// clock is served, and the refused request wrote nothing.
#[tokio::test]
async fn an_observation_after_the_clock_is_refused() {
    let rig = Rig::new(false);
    rig.seed().await;
    let mut body = consolidate("future");
    for (observed, expected) in [("101", 422), ("100", 200)] {
        body["output"]["observed_at_ms"] = json!(observed);
        let (status, answer) = rig.post(CONSOLIDATE, &body).await;
        assert_eq!(status, expected, "observed {observed}: {answer}");
        if status == 422 {
            assert_eq!(answer["error"]["code"], "invalid_input", "{answer}");
        }
    }
}

/// Serving section 6: access needs the credential's grant on the collection
/// as it is now. A dropped and recreated evidence collection has a new ID, so
/// the stale grant is refused with the one uniform 403 before any refusal,
/// validation, receipt replay or lookup (mapping section 2, check order).
#[tokio::test]
async fn a_recreated_evidence_collection_is_refused_before_anything_else() {
    let rig = Rig::new(true);
    rig.seed().await;
    let (status, done) = rig.post(CONSOLIDATE, &consolidate("before")).await;
    assert_eq!(status, 200, "{done}");

    rig.recreate_evidence();
    let evidence = json!([{"collection": "notes_ev", "record": "1"}]);
    let bodies = [
        consolidate("after"),
        consolidate("before"),
        json!({"scope": {"agent": "a"}, "dry_run": true}),
        json!({"scope": {"agent": "a"}, "evidence": evidence}),
        json!({"scope": {"agent": "a"}, "inputs": [{"id": "m1", "revision": "1",
            "disposition": "logical_delete"}]}),
        json!({"scope": {"agent": "a"}}),
    ];
    for body in bodies {
        let (status, error) = rig.post(CONSOLIDATE, &body).await;
        assert_eq!(
            (status, &error["error"]["code"]),
            (403, &json!("forbidden_scope")),
            "{body}: {error}"
        );
    }
}
