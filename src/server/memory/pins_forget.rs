//! Forget pins that need crate-private state: the engine clock and the
//! elapsed-budget seam, evidence records and attachments seeded through the
//! core, the erasure barrier and a granted but unprovisioned scope. Every
//! expectation comes from the serving contract (sections 3, 5, 6), the adapter
//! mapping (sections 1, 2, 6) and the E1 barrier matrix (section 5), never from
//! the adapter code. Receipt and barrier keys are built from the E1 contract's
//! exact bytes (section 3).

use std::sync::{Arc, atomic::Ordering};

use axum::{
    Extension, Router,
    body::Body,
    extract::DefaultBodyLimit,
    http::{Request, header},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use super::super::{MAX_JSON_BYTES, RequestId, Service, admin, credentials};
use super::routes::Clock;
use crate::memory::{
    MemoryEvidence, MemoryItem, MemoryKind, MemoryMut, MemoryScope, Provenance, RememberRequest,
};
use crate::property_graph::{FindNodes, GraphOp, ReadOptions};
use crate::{
    CollectionSpec, CreateOptions, Database, FormatVersion, Metadata, MetadataValue, Mutation,
};

const PRINCIPAL: &str = "abcdefghij27";
const GRAPH: &str = "notes_graph";
const EVIDENCE: &str = "notes_ev";
const WINDOW: u64 = 60_000;

struct Options {
    retired: bool,
    provision: bool,
    scopes: Vec<Value>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            retired: false,
            provision: true,
            scopes: vec![json!({"agent": "a"})],
        }
    }
}

struct Pins {
    router: Router,
    clock: Clock,
    service: Service,
}

fn scope(agent: &str, user: Option<&str>) -> MemoryScope {
    MemoryScope {
        tenant: "@root".into(),
        agent: agent.into(),
        user: user.map(str::to_owned),
        session: None,
    }
}

/// E1 section 3: `ScopeBytes` and `ScopeKey`.
fn scope_bytes(scope: &MemoryScope) -> Vec<u8> {
    let mut bytes = b"scope:v1".to_vec();
    for part in [
        Some(scope.tenant.as_str()),
        Some(scope.agent.as_str()),
        scope.user.as_deref(),
        scope.session.as_deref(),
    ] {
        match part {
            None => bytes.push(0),
            Some(text) => {
                bytes.push(1);
                bytes.extend((text.len() as u32).to_be_bytes());
                bytes.extend(text.as_bytes());
            }
        }
    }
    bytes
}

fn scope_key(scope: &MemoryScope) -> String {
    format!("scope:v1:{:x}", Sha256::digest(scope_bytes(scope)))
}

/// E1 section 3: `R(op, key)`.
fn receipt_key(operation: &str, scope: &MemoryScope, key: &str) -> String {
    let mut input = format!("{operation}:v1\0").into_bytes();
    input.extend(key.as_bytes());
    format!(
        "receipt:v1:{}:{:x}",
        scope_key(scope),
        Sha256::digest(input)
    )
}

fn evidence_record_id(id: &str) -> u64 {
    u64::from_be_bytes(Sha256::digest(id.as_bytes())[..8].try_into().unwrap())
}

fn item(id: &str, evidence: Option<(&str, &MemoryScope)>, references: &[&str]) -> MemoryItem {
    MemoryItem {
        id: id.into(),
        kind: MemoryKind::Entity,
        content: format!("content of {id}"),
        source: "origin".into(),
        importance: None,
        observed_at_ms: Some(1),
        provenance: vec![Provenance {
            source: "origin".into(),
            kind: Some("caller".into()),
            uri: None,
            transformation: None,
        }],
        references: references.iter().map(|id| (*id).to_owned()).collect(),
        merge_into: None,
        expected_revision: None,
        expiry_ms: None,
        expiry_valid: true,
        evidence: evidence
            .map(|(id, scope)| {
                vec![MemoryEvidence {
                    id: id.into(),
                    scope: scope.clone(),
                }]
            })
            .unwrap_or_default(),
    }
}

fn forget_in(scope: Value, key: &str, targets: &[(&str, u64)]) -> Value {
    let ids: Vec<Value> = targets
        .iter()
        .map(|(id, revision)| json!({"id": id, "revision": revision.to_string()}))
        .collect();
    json!({
        "scope": scope, "idempotency_key": key,
        "target": {"ids": ids}, "mode": "logical_delete",
    })
}

fn forget_body(key: &str, targets: &[(&str, u64)]) -> Value {
    forget_in(json!({"agent": "a"}), key, targets)
}

fn assert_error(result: &(u16, Value), status: u16, code: &str) {
    assert_eq!(
        (result.0, &result.1["error"]["code"]),
        (status, &json!(code)),
        "{}",
        result.1
    );
}

impl Pins {
    fn new() -> Self {
        Self::with(Options::default())
    }

    fn with(options: Options) -> Self {
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
            "binding": "notes", "tenant": "@root", "generation": 1, "state": "active",
            "graph": GRAPH, "evidence_collection": EVIDENCE,
            "schema_version": "memory-schema-v1", "ranking_version": "memory-ranking-v1",
            "digest_version": "memory-request-v1", "receipt_window_ms": WINDOW,
            "grants": [{"principal": PRINCIPAL, "scopes": options.scopes}],
        });
        if options.retired {
            entry["state"] = json!("retired");
            entry["retired_at_ms"] = json!(500);
        }
        let text = json!({"version": "memory-bindings-v1", "bindings": [entry]}).to_string();
        let bindings = crate::memory::binding::parse(text.as_bytes()).unwrap();
        let service = Service::new(db).with_bindings(super::reserved::Bindings::new(bindings));
        if options.provision {
            super::startup(&service).unwrap();
        }
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

    fn at(&self, now_ms: u64) {
        self.clock.now_ms.store(now_ms, Ordering::SeqCst);
    }

    /// The elapsed request budget the handler sees (the test seam).
    fn elapsed(&self, elapsed_ms: u64) {
        self.clock.elapsed_ms.store(elapsed_ms, Ordering::SeqCst);
    }

    async fn post(&self, operation: &str, body: &Value) -> (u16, Value) {
        let request = Request::builder()
            .method("POST")
            .uri(format!("/v1/memory/notes/{operation}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(body).unwrap()))
            .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    fn with_db<T>(&self, work: impl FnOnce(&mut Database) -> T) -> T {
        let mut owner = self.service.database.write().unwrap();
        work(owner.as_mut().unwrap())
    }

    /// Core seed at clock 100: one entity per ID, no evidence.
    fn seed(&self, scope: &MemoryScope, ids: &[&str]) {
        let items = ids.iter().map(|id| item(id, None, &[])).collect();
        self.remember(scope, "seed", items);
    }

    fn remember(&self, scope: &MemoryScope, key: &str, items: Vec<MemoryItem>) {
        let request = RememberRequest {
            scope: scope.clone(),
            idempotency_key: key.into(),
            receipt_window_ms: WINDOW,
            now_ms: 100,
            deadline_ms: None,
            allow_future_observations: false,
            items,
        };
        self.with_db(|db| {
            MemoryMut::bind(db, GRAPH, Some(EVIDENCE))
                .unwrap()
                .remember(&request)
                .unwrap()
        });
    }

    /// An evidence record the core may attach, in the bound collection.
    fn put_record(&self, name: &str) -> u64 {
        let record = evidence_record_id(name);
        self.with_db(|db| {
            db.graph_mut(GRAPH)
                .unwrap()
                .write_batch(&[GraphOp::Vector {
                    collection: EVIDENCE.into(),
                    mutation: Mutation::Put {
                        id: record,
                        vector: vec![1.0, 0.0],
                        payload: name.as_bytes().to_vec(),
                    },
                }])
                .unwrap()
        });
        record
    }

    fn record_exists(&self, record: u64) -> bool {
        self.with_db(|db| {
            db.collection(EVIDENCE)
                .unwrap()
                .get(record)
                .unwrap()
                .is_some()
        })
    }

    /// The `(id, node key)` of every live memory node, by ID.
    fn nodes(&self) -> Vec<(String, String)> {
        self.with_db(|db| {
            let found = db
                .graph(GRAPH)
                .unwrap()
                .find_nodes(
                    &FindNodes {
                        label: Some("VliteMemory".into()),
                        limit: Some(1_000),
                        ..FindNodes::default()
                    },
                    &ReadOptions::default(),
                )
                .unwrap();
            let mut nodes: Vec<(String, String)> = found
                .output
                .into_iter()
                .map(|node| match node.properties.get("id") {
                    Some(MetadataValue::String(id)) => (id.clone(), node.key),
                    other => panic!("memory node without an ID: {other:?}"),
                })
                .collect();
            nodes.sort();
            nodes
        })
    }

    fn live(&self) -> Vec<String> {
        self.nodes().into_iter().map(|(id, _)| id).collect()
    }

    fn sequence(&self) -> u64 {
        self.with_db(|db| db.sequence)
    }

    fn has_receipt(&self, scope: &MemoryScope, key: &str) -> bool {
        let key = receipt_key("forget", scope, key);
        self.with_db(|db| {
            db.graph(GRAPH)
                .unwrap()
                .get_node(&key)
                .unwrap()
                .output
                .is_some()
        })
    }

    /// Begins erasure of `scope` as E1 section 4 stores it: the singleton job,
    /// the begin receipt outcome and the begin receipt, in one frame.
    fn bar(&self, scope: &MemoryScope, key: &str, now: u64) {
        let scope_bytes = scope_bytes(scope);
        let scope_key = scope_key(scope);
        let tuple = format!("{:x}", HexBytes(&scope_bytes));
        let scope_revision = self.with_db(|db| {
            db.graph(GRAPH)
                .unwrap()
                .get_node(&scope_key)
                .unwrap()
                .output
                .unwrap()
                .revision
        });
        let mut begin = b"vlite.memory.erasure.begin.v1\0".to_vec();
        begin.extend((scope_bytes.len() as u32).to_be_bytes());
        begin.extend(&scope_bytes);
        begin.push(1);
        begin.extend((EVIDENCE.len() as u32).to_be_bytes());
        begin.extend(EVIDENCE.as_bytes());
        begin.extend(scope_revision.to_be_bytes());
        begin.extend(WINDOW.to_be_bytes());
        let digest = format!("v1:{:x}", Sha256::digest(&begin));
        let job_key = receipt_key("erase-scope-job", scope, "singleton");
        let begin_key = receipt_key("begin-erasure", scope, key);
        let outcome_key = format!(
            "receipt-outcome:v1:{:x}:0000",
            Sha256::digest(begin_key.as_bytes())
        );
        let text = |value: &str| MetadataValue::String(value.to_owned());
        let mut common = Metadata::new();
        common.insert("e1_schema".into(), text("erase-scope:v1"));
        common.insert("scope".into(), text(&scope_key));
        common.insert("e1_scope_tuple".into(), text(&tuple));
        common.insert("e1_collection".into(), text(EVIDENCE));
        let mut job = common.clone();
        job.insert("begin_digest".into(), text(&digest));
        for (name, value) in [
            ("generation", "1"),
            ("state", "pending"),
            ("logical_visibility", "hidden"),
            ("deleted_count", "0"),
            ("remaining", "unknown"),
        ] {
            job.insert(name.into(), text(value));
        }
        job.insert("created_at_ms".into(), MetadataValue::Integer(now as i64));
        let mut outcome = Metadata::new();
        outcome.insert("receipt_key".into(), text(&begin_key));
        outcome.insert(
            "outcome".into(),
            text(&format!("erase-scope:v1\n{job_key}\n1\npending\nunknown\n")),
        );
        outcome.insert("index".into(), MetadataValue::Integer(0));
        let mut receipt = common;
        receipt.insert("digest".into(), text(&digest));
        receipt.insert("expires_at_ms".into(), text(&(now + WINDOW).to_string()));
        receipt.insert("job_key".into(), text(&job_key));
        receipt.insert("outcome_chunks".into(), MetadataValue::Integer(1));
        self.with_db(|db| {
            db.graph_mut(GRAPH)
                .unwrap()
                .write_batch_at(
                    &[
                        GraphOp::CreateNode {
                            key: job_key,
                            labels: vec!["VliteMemoryErasureJob".into()],
                            properties: job,
                        },
                        GraphOp::CreateNode {
                            key: outcome_key,
                            labels: vec!["VliteMemoryReceiptOutcome".into()],
                            properties: outcome,
                        },
                        GraphOp::CreateNode {
                            key: begin_key,
                            labels: vec!["VliteMemoryReceipt".into()],
                            properties: receipt,
                        },
                    ],
                    now,
                )
                .unwrap()
        });
    }
}

struct HexBytes<'a>(&'a [u8]);

impl std::fmt::LowerHex for HexBytes<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0
            .iter()
            .try_for_each(|byte| write!(formatter, "{byte:02x}"))
    }
}

/// Contract section 3: a live matching replay precedes the deadline check,
/// while a fresh request whose budget is spent is 504 and writes nothing.
#[tokio::test]
async fn a_live_matching_replay_precedes_the_deadline_check() {
    let pins = Pins::new();
    let a = scope("a", None);
    pins.seed(&a, &["m1", "m2"]);
    pins.at(1_000);
    let request = forget_body("k", &[("m1", 1)]);
    let (status, first) = pins.post("forget", &request).await;
    assert_eq!(status, 200, "{first}");
    assert_eq!(first["replayed"], false);
    // The receipt window is the binding's 60 s from the engine clock.
    assert_eq!(first["receipt"]["retry_until_ms"], "61000");
    assert_eq!(
        first["receipt"]["scope"],
        json!({"tenant": "@root", "agent": "a"})
    );
    assert_eq!(first["receipt"]["digest_version"], "memory-request-v1");

    // The whole 250 ms default budget is spent before the handler reaches the
    // core, and a retry does not extend the receipt.
    pins.elapsed(300);
    pins.at(1_100);
    let (status, replay) = pins.post("forget", &request).await;
    assert_eq!(status, 200, "{replay}");
    let mut expected = first.clone();
    expected["replayed"] = json!(true);
    assert_eq!(replay, expected);

    let before = pins.sequence();
    let fresh = pins.post("forget", &forget_body("k2", &[("m2", 1)])).await;
    assert_error(&fresh, 504, "deadline_exceeded");
    assert_eq!(pins.sequence(), before, "a deadline writes nothing");
    assert_eq!(pins.live(), ["m2"]);
    assert!(!pins.has_receipt(&a, "k2"));
}

/// Contract section 4: `deadline_ms` is a relative budget of 1..=5,000 ms.
#[tokio::test]
async fn the_deadline_budget_is_relative_and_bounded() {
    let pins = Pins::new();
    pins.at(1_000);
    for (budget, status) in [(0, 422), (5_001, 422), (1, 404), (5_000, 404)] {
        let mut body = forget_body("k", &[("ghost", 1)]);
        body["deadline_ms"] = json!(budget);
        let result = pins.post("forget", &body).await;
        assert_eq!(result.0, status, "{budget}: {}", result.1);
        if status == 422 {
            assert_eq!(result.1["error"]["details"]["field"], "deadline_ms");
        }
    }
}

/// Contract sections 3 and 5: a refused request is atomic. No target is
/// deleted, no receipt is written and the same key stays free to execute.
#[tokio::test]
async fn a_refused_forget_deletes_nothing_and_leaves_no_receipt() {
    let pins = Pins::new();
    let a = scope("a", None);
    pins.seed(&a, &["m1", "m2"]);
    pins.at(1_000);
    let refuse = async |key: &str, targets: &[(&str, u64)], status: u16, code: &str| {
        let before = pins.sequence();
        let result = pins.post("forget", &forget_body(key, targets)).await;
        assert_error(&result, status, code);
        assert_eq!(pins.sequence(), before, "{key}: no frame was committed");
        assert_eq!(pins.live(), ["m1", "m2"], "{key}");
        assert!(!pins.has_receipt(&a, key), "{key}: no receipt");
    };
    refuse(
        "missing-last",
        &[("m1", 1), ("ghost", 1)],
        404,
        "memory_not_found",
    )
    .await;
    refuse(
        "stale-last",
        &[("m1", 1), ("m2", 5)],
        412,
        "precondition_failed",
    )
    .await;
    refuse(
        "missing-first",
        &[("ghost", 1), ("m1", 1)],
        404,
        "memory_not_found",
    )
    .await;
    // The retry under the same key with only the first target executes now.
    let (status, body) = pins
        .post("forget", &forget_body("missing-last", &[("m1", 1)]))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["replayed"], false);
    assert_eq!(pins.live(), ["m2"]);
}

/// Contract section 5: a small logical delete detaches the authorized
/// evidence and deletes unshared records, while evidence a surviving memory
/// still reaches through a reference is retained.
#[tokio::test]
async fn forget_detaches_evidence_and_retains_what_a_survivor_still_shares() {
    let pins = Pins::new();
    let a = scope("a", None);
    let shared = pins.put_record("shared-evidence");
    let unshared = pins.put_record("unshared-evidence");
    pins.remember(
        &a,
        "seed-t",
        vec![item("t", Some(("shared-evidence", &a)), &[])],
    );
    pins.remember(&a, "seed-s", vec![item("s", None, &["t"])]);
    pins.remember(
        &a,
        "seed-u",
        vec![item("u", Some(("unshared-evidence", &a)), &[])],
    );
    assert_eq!(pins.live(), ["s", "t", "u"]);
    pins.at(1_000);
    let (status, body) = pins
        .post("forget", &forget_body("k", &[("u", 1), ("t", 1)]))
        .await;
    assert_eq!(status, 200, "{body}");
    let sequence = body["receipt"]["sequence"].clone();
    assert_eq!(
        body["outcomes"],
        json!([
            {"id": "t", "action": "deleted", "revision": sequence},
            {"id": "u", "action": "deleted", "revision": sequence},
        ])
    );
    assert_eq!(pins.live(), ["s"]);
    assert!(pins.record_exists(shared), "shared evidence is retained");
    assert!(
        !pins.record_exists(unshared),
        "unshared evidence goes with its owner"
    );
}

/// Contract section 5 and mapping section 2: an attachment that is not the
/// memory's own evidence blocks the whole mutation as 409
/// `memory_attachment_conflict`, never detached opportunistically.
#[tokio::test]
async fn a_foreign_attachment_blocks_the_whole_forget() {
    let pins = Pins::new();
    let a = scope("a", None);
    pins.seed(&a, &["m1", "m2"]);
    let record = pins.put_record("foreign");
    let key = pins
        .nodes()
        .into_iter()
        .find(|(id, _)| id == "m2")
        .map(|(_, key)| key)
        .unwrap();
    pins.with_db(|db| {
        db.graph_mut(GRAPH)
            .unwrap()
            .write_batch(&[GraphOp::Attach {
                node: key,
                collection: EVIDENCE.into(),
                record,
            }])
            .unwrap()
    });
    pins.at(1_000);
    let before = pins.sequence();
    let result = pins
        .post("forget", &forget_body("k", &[("m1", 1), ("m2", 1)]))
        .await;
    assert_error(&result, 409, "memory_attachment_conflict");
    assert_eq!(pins.sequence(), before);
    assert_eq!(pins.live(), ["m1", "m2"]);
    assert!(pins.record_exists(record));
    assert!(!pins.has_receipt(&a, "k"));
}

/// E1 section 5 and mapping section 6: on an exactly barred scope a fresh
/// Forget is 409 `memory_deletion_pending` (never absent, never a conflict),
/// only a live matching pre-barrier receipt replays, and the barrier bars
/// nothing but its exact tuple.
#[tokio::test]
async fn a_barred_scope_refuses_fresh_forgets_but_replays_a_prior_receipt() {
    let pins = Pins::with(Options {
        scopes: vec![
            json!({"agent": "a"}),
            json!({"agent": "a", "user": "u"}),
            json!({"agent": "b"}),
        ],
        ..Options::default()
    });
    let a = scope("a", None);
    pins.seed(&a, &["m1", "m2"]);
    pins.seed(&scope("a", Some("u")), &["c1"]);
    pins.seed(&scope("b", None), &["b1"]);
    pins.at(1_000);
    let before = forget_body("pre", &[("m1", 1)]);
    let (status, first) = pins.post("forget", &before).await;
    assert_eq!(status, 200, "{first}");
    pins.bar(&a, "begin", 1_100);

    pins.at(1_200);
    let barred = [
        forget_body("fresh", &[("m2", 1)]),
        forget_body("fresh-absent", &[("ghost", 1)]),
        forget_body("fresh-stale", &[("m2", 9)]),
        // A changed request under the pre-barrier key is still pending.
        forget_body("pre", &[("m2", 1)]),
        forget_body("pre", &[("m1", 1), ("m2", 1)]),
    ];
    for body in &barred {
        let result = pins.post("forget", body).await;
        assert_error(&result, 409, "memory_deletion_pending");
    }
    assert_eq!(pins.live(), ["b1", "c1", "m2"]);
    // The matching live pre-barrier receipt still replays.
    let (status, replay) = pins.post("forget", &before).await;
    assert_eq!(status, 200, "{replay}");
    let mut expected = first.clone();
    expected["replayed"] = json!(true);
    assert_eq!(replay, expected);
    // The barrier bars its exact tuple only: a child and a sibling work.
    for (scope_json, id, key) in [
        (json!({"agent": "a", "user": "u"}), "c1", "child"),
        (json!({"agent": "b"}), "b1", "sibling"),
    ] {
        let (status, body) = pins
            .post("forget", &forget_in(scope_json, key, &[(id, 1)]))
            .await;
        assert_eq!(status, 200, "{key}: {body}");
    }
    assert_eq!(pins.live(), ["m2"]);
    // An expired receipt cannot authorize work behind the barrier either.
    pins.at(1_000 + WINDOW);
    let result = pins.post("forget", &before).await;
    assert_error(&result, 409, "memory_deletion_pending");
}

/// Mapping section 2: an authorized scope that startup never provisioned is
/// 409 `memory_not_configured`, not an absent memory.
#[tokio::test]
async fn an_authorized_but_unprovisioned_scope_is_not_configured() {
    let pins = Pins::with(Options {
        provision: false,
        ..Options::default()
    });
    pins.at(1_000);
    let before = pins.sequence();
    let result = pins.post("forget", &forget_body("k", &[("m1", 1)])).await;
    assert_error(&result, 409, "memory_not_configured");
    assert_eq!(pins.sequence(), before);
}

/// Mapping section 2 multi-refusal priority: the binding state, then
/// `require_complete_feed: true`, then the field refusals in field order.
#[tokio::test]
async fn forget_refusals_follow_the_documented_priority() {
    let series = json!({"series": {"id": "f", "revision": "1"}});
    let combined = |mutate: &dyn Fn(&mut Value)| {
        let mut body = forget_body("k", &[("m1", 1)]);
        mutate(&mut body);
        body
    };
    let active = Pins::new();
    active.at(1_000);
    let feed_cases = [
        combined(&|body| {
            body["require_complete_feed"] = json!(true);
            body["target"] = series.clone();
        }),
        combined(&|body| {
            body["require_complete_feed"] = json!(true);
            body["mode"] = json!("expire");
        }),
        combined(&|body| {
            body["require_complete_feed"] = json!(true);
            body["allow_job"] = json!(true);
        }),
        combined(&|body| {
            body["require_complete_feed"] = json!(true);
            body["target"] = json!({"session": true});
        }),
    ];
    for body in &feed_cases {
        let result = active.post("forget", body).await;
        assert_error(&result, 409, "memory_feed_unsupported");
        assert!(
            result.1["error"]["details"]["feature"].is_null(),
            "{}",
            result.1
        );
    }
    // Without the complete-feed request the field refusals apply in order.
    let fields = [
        (series.clone(), "forget_target"),
        (json!({"session": true}), "erasure_jobs"),
    ];
    for (target, feature) in fields {
        let mut body = forget_body("k", &[("m1", 1)]);
        body["target"] = target;
        body["require_complete_feed"] = json!(false);
        let result = active.post("forget", &body).await;
        assert_error(&result, 409, "memory_capability_unavailable");
        assert_eq!(result.1["error"]["details"]["feature"], feature);
    }

    // A retired binding refuses before every other refusal.
    let retired = Pins::with(Options {
        retired: true,
        ..Options::default()
    });
    retired.at(1_000);
    for body in feed_cases.iter().chain([&forget_body("k", &[("m1", 1)])]) {
        let result = retired.post("forget", body).await;
        assert_error(&result, 409, "memory_capability_unavailable");
        assert_eq!(result.1["error"]["details"]["feature"], "binding_retired");
    }
}
