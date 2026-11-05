//! Binding-record pins that need crate-private state: the engine clock seam,
//! the record node itself and the write lock. Every expectation comes from
//! the 0.7.1 design note (`docs/projects/roadmap/m71-memory-followups.md`,
//! section 1.5) and the adapter mapping (section 4: before every memory write
//! the handler rechecks, under the same write guard, that the graph's record
//! still has the generation it serves, otherwise 409
//! `memory_capability_unavailable` with `details.feature`
//! `binding_generation`), never from the adapter code.

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
use crate::memory::binding::{RECORD_KEY, RECORD_LABEL};
use crate::property_graph::{GraphOp, NodeView};
use crate::{
    CollectionSpec, CreateOptions, Database, Expected, FormatVersion, Metadata, MetadataValue,
};

const PRINCIPAL: &str = "abcdefghij27";
const GRAPH: &str = "notes_graph";
const EVIDENCE: &str = "notes_ev";
const WINDOW: u64 = 60_000;
const OPERATIONS: [&str; 3] = ["remember", "forget", "consolidate"];

struct Pins {
    router: Router,
    service: Service,
    /// The record as startup wrote it.
    original: Metadata,
}

fn post_to(router: Router, operation: &str, body: &Value) -> impl Future<Output = (u16, Value)> {
    let request = Request::builder()
        .method("POST")
        .uri(format!("/v1/memory/notes/{operation}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap();
    async move {
        let response = router.oneshot(request).await.unwrap();
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }
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

fn forget_body(key: &str) -> Value {
    json!({
        "scope": {"agent": "a"}, "idempotency_key": key, "deadline_ms": 5000,
        "target": {"ids": [{"id": "e1", "revision": "1"}]}, "mode": "logical_delete",
    })
}

fn consolidate_body(key: &str) -> Value {
    json!({
        "scope": {"agent": "a"}, "idempotency_key": key, "deadline_ms": 5000,
        "inputs": [{"id": "e1", "revision": "1"}],
        "output": {
            "id": "sum1", "kind": "episodic", "content": "summary", "observed_at_ms": "90",
            "source": {"source_id": "s2", "source_kind": "caller", "transformation_id": "t1"},
        },
    })
}

fn body_of(operation: &str, key: &str) -> Value {
    match operation {
        "remember" => remember_body(key),
        "forget" => forget_body(key),
        "consolidate" => consolidate_body(key),
        other => panic!("unknown operation {other}"),
    }
}

fn assert_stale(result: &(u16, Value), context: &str) {
    assert_eq!(result.0, 409, "{context}: {}", result.1);
    assert_eq!(
        result.1["error"]["code"], "memory_capability_unavailable",
        "{context}: {}",
        result.1
    );
    assert_eq!(
        result.1["error"]["details"]["feature"], "binding_generation",
        "{context}: {}",
        result.1
    );
}

fn text(value: &str) -> MetadataValue {
    MetadataValue::String(value.into())
}

fn record(db: &Database) -> Option<NodeView> {
    db.graph(GRAPH)
        .unwrap()
        .get_node(RECORD_KEY)
        .unwrap()
        .output
}

/// Replaces the record's properties, or deletes the record for `None`.
fn set_record(db: &mut Database, properties: Option<Metadata>) {
    let op = match (record(db), properties) {
        (Some(node), Some(properties)) => GraphOp::UpsertNode {
            key: RECORD_KEY.into(),
            labels: vec![RECORD_LABEL.into()],
            properties,
            expected: Expected::Revision(node.revision),
        },
        (Some(node), None) => GraphOp::DeleteNode {
            key: RECORD_KEY.into(),
            expected: Expected::Revision(node.revision),
        },
        (None, Some(properties)) => GraphOp::CreateNode {
            key: RECORD_KEY.into(),
            labels: vec![RECORD_LABEL.into()],
            properties,
        },
        (None, None) => return,
    };
    db.graph_mut(GRAPH).unwrap().write_batch(&[op]).unwrap();
}

impl Pins {
    fn new(retired: bool) -> Self {
        Self::with(retired, 1)
    }

    /// A started server at validation clock 0 (so a retired entry at 500 is
    /// "first seen" in the future), with the engine clock at 100.
    fn with(retired: bool, generation: u64) -> Self {
        let mut db = Database::create_in_memory(
            2,
            CreateOptions {
                format: Some(FormatVersion::V5),
                ..Default::default()
            },
        )
        .unwrap();
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
        let mut entry = json!({
            "binding": "notes", "tenant": "@root", "generation": generation, "state": "active",
            "graph": GRAPH, "evidence_collection": EVIDENCE,
            "schema_version": "memory-schema-v1", "ranking_version": "memory-ranking-v1",
            "digest_version": "memory-request-v1", "receipt_window_ms": WINDOW,
            "grants": [{"principal": PRINCIPAL, "scopes": [{"agent": "a"}]}],
        });
        if retired {
            entry["state"] = json!("retired");
            entry["retired_at_ms"] = json!(500);
        }
        let text = json!({"version": "memory-bindings-v1", "bindings": [entry]}).to_string();
        let set = crate::memory::binding::parse(text.as_bytes()).unwrap();
        let service = Service::new(db).with_bindings(super::reserved::Bindings::new(set));
        super::startup_at(&service, 0).unwrap();
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
        let original = {
            let owner = service.database.read().unwrap();
            record(owner.as_ref().unwrap())
                .expect("startup wrote the record")
                .properties
        };
        Self {
            router,
            service,
            original,
        }
    }

    fn with_db<T>(&self, work: impl FnOnce(&mut Database) -> T) -> T {
        let mut owner = self.service.database.write().unwrap();
        work(owner.as_mut().unwrap())
    }

    async fn post(&self, operation: &str, body: &Value) -> (u16, Value) {
        post_to(self.router.clone(), operation, body).await
    }

    fn sequence(&self) -> u64 {
        self.with_db(|db| db.sequence)
    }

    fn set_record(&self, properties: Option<Metadata>) {
        self.with_db(|db| set_record(db, properties));
    }

    fn change_record(&self, change: impl FnOnce(&mut Metadata)) {
        let mut properties = self.original.clone();
        change(&mut properties);
        self.set_record(Some(properties));
    }

    fn restore_record(&self) {
        self.set_record(Some(self.original.clone()));
    }

    /// A first remember, so forget and consolidate name something that exists.
    async fn seed(&self) {
        let seeded = self.post("remember", &remember_body("seed")).await;
        assert_eq!(seeded.0, 200, "{}", seeded.1);
    }
}

#[tokio::test]
async fn a_started_server_holds_the_record_of_its_binding() {
    let pins = Pins::new(false);
    let node = pins.with_db(|db| record(db)).expect("the record exists");
    assert_eq!(node.labels.iter().collect::<Vec<_>>(), [RECORD_LABEL]);
    assert_eq!(node.properties["binding"], text("notes"));
    assert_eq!(node.properties["tenant"], text("@root"));
    assert_eq!(node.properties["generation"], text("1"));
    assert_eq!(node.properties["state"], text("active"));
    assert_eq!(node.properties["graph"], text(GRAPH));
    assert_eq!(node.properties["evidence"], text(EVIDENCE));
    assert_eq!(node.properties["receipt_window_ms"], text("60000"));
    assert!(!node.properties.contains_key("retired_at_ms"));
    let retired = Pins::new(true);
    let node = retired.with_db(|db| record(db)).unwrap();
    assert_eq!(node.properties["state"], text("retired"));
    assert_eq!(node.properties["retired_at_ms"], text("500"));
}

#[tokio::test]
async fn mutations_refuse_a_lower_or_higher_recorded_generation_and_write_nothing() {
    for operation in OPERATIONS {
        for recorded in ["2", "4"] {
            let pins = Pins::with(false, 3);
            pins.seed().await;
            pins.change_record(|properties| {
                properties.insert("generation".into(), text(recorded));
            });
            let before = pins.sequence();
            let stale = pins.post(operation, &body_of(operation, "k1")).await;
            assert_stale(&stale, &format!("{operation} against {recorded}"));
            assert_eq!(pins.sequence(), before, "{operation} wrote nothing");
            // The refusal is about the record: put it back and the request runs.
            pins.restore_record();
            let served = pins.post(operation, &body_of(operation, "k1")).await;
            assert_eq!(served.0, 200, "{operation}: {}", served.1);
        }
    }
}

#[tokio::test]
async fn a_missing_corrupt_or_foreign_record_refuses_every_mutation() {
    for operation in OPERATIONS {
        let pins = Pins::new(false);
        pins.seed().await;
        let with = |name: &str, value: &str| {
            let mut properties = pins.original.clone();
            properties.insert(name.into(), text(value));
            Some(properties)
        };
        let cases: [(&str, Option<Metadata>); 4] = [
            ("deleted", None),
            ("extra property", with("extra", "x")),
            ("another binding's record", with("binding", "other")),
            (
                "unknown schema",
                with("record_schema", "memory-binding-record:v2"),
            ),
        ];
        for (name, properties) in cases {
            pins.set_record(properties);
            let before = pins.sequence();
            let stale = pins.post(operation, &body_of(operation, "k2")).await;
            assert_stale(&stale, &format!("{operation} with {name}"));
            assert_eq!(pins.sequence(), before, "{operation} with {name}");
            pins.restore_record();
        }
    }
}

#[tokio::test]
async fn reads_do_not_recheck() {
    let pins = Pins::new(false);
    pins.seed().await;
    let recall = json!({"scope": {"agent": "a"}, "entity_seeds": ["e1"], "limit": 1});
    let lookup = json!({"scope": {"agent": "a"}, "operation": "forget", "idempotency_key": "none"});
    for record in [Some("9"), None] {
        match record {
            Some(generation) => pins.change_record(|properties| {
                properties.insert("generation".into(), text(generation));
            }),
            None => pins.set_record(None),
        }
        let recalled = pins.post("recall", &recall).await;
        assert_eq!(recalled.0, 200, "{}", recalled.1);
        let looked_up = pins.post("receipts/lookup", &lookup).await;
        assert_eq!(looked_up.0, 404, "{}", looked_up.1);
        assert_eq!(looked_up.1["error"]["code"], "receipt_not_found");
    }
}

#[tokio::test]
async fn the_recheck_precedes_field_refusals_and_validation() {
    let pins = Pins::new(false);
    pins.change_record(|properties| {
        properties.insert("generation".into(), text("2"));
    });
    let mut refused = remember_body("k");
    refused["items"][0]["confidence"] = json!(0.5);
    assert_stale(&pins.post("remember", &refused).await, "a refused field");
    let invalid = json!({"scope": {"agent": "a"}, "idempotency_key": "k", "items": []});
    assert_stale(&pins.post("remember", &invalid).await, "an invalid body");
    let mut unknown = remember_body("k");
    unknown["unknown_field"] = json!(1);
    assert_stale(&pins.post("remember", &unknown).await, "an unknown field");
}

#[tokio::test]
async fn a_retired_binding_with_a_changed_record_answers_the_generation_first() {
    let pins = Pins::new(true);
    let intact = pins.post("remember", &remember_body("k")).await;
    assert_eq!(intact.0, 409, "{}", intact.1);
    assert_eq!(intact.1["error"]["details"]["feature"], "binding_retired");
    pins.change_record(|properties| {
        properties.insert("generation".into(), text("2"));
    });
    assert_stale(&pins.post("remember", &remember_body("k")).await, "retired");
}

#[tokio::test]
async fn the_recheck_reads_the_record_when_the_write_runs_not_when_it_was_admitted() {
    let pins = Pins::new(false);
    let idle = pins.service.workers.available_permits();
    // A second thread holds the database write lock until the request has
    // been admitted, changes the record while the request waits, and lets go.
    let (locked_tx, locked_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let service = pins.service.clone();
    let original = pins.original.clone();
    let holder = std::thread::spawn(move || {
        let mut owner = service.database.write().unwrap();
        locked_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        let mut properties = original;
        properties.insert("generation".into(), text("2"));
        set_record(owner.as_mut().unwrap(), Some(properties));
    });
    locked_rx.recv().unwrap();
    let router = pins.router.clone();
    let request =
        tokio::spawn(async move { post_to(router, "remember", &remember_body("k")).await });
    while pins.service.workers.available_permits() == idle {
        tokio::task::yield_now().await;
    }
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    let stale = request.await.unwrap();
    assert_stale(
        &stale,
        "a record changed while the request waited for the lock",
    );
    pins.restore_record();
    let served = pins.post("remember", &remember_body("k")).await;
    assert_eq!(served.0, 200, "{}", served.1);
}

#[tokio::test]
async fn a_matching_record_adds_no_operation_to_the_batch() {
    // The recheck is a read: with an intact record a mutation is exactly one
    // commit and a replay is none.
    let pins = Pins::new(false);
    let before = pins.sequence();
    let served = pins.post("remember", &remember_body("k")).await;
    assert_eq!(served.0, 200, "{}", served.1);
    assert_eq!(pins.sequence(), before + 1);
    let replayed = pins.post("remember", &remember_body("k")).await;
    assert_eq!(replayed.1["replayed"], true);
    assert_eq!(pins.sequence(), before + 1, "a replay writes nothing");
}
