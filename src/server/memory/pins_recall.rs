//! Recall pins (mutation closure for the `recall` route). Every expectation
//! below is derived from the serving contract (sections 3, 4, 6, 8, 9), the
//! adapter mapping (sections 1, 2, 6 and the 0.7.0 freeze addendum) and
//! `docs/API.md` "First-slice limits", never from what the adapter returned.
//! The harness mirrors `replay.rs` but is parameterized by credential, binding
//! and clock, so one request can break exactly one rule.

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
use crate::property_graph::GraphOp;
use crate::{
    CollectionSettings, CollectionSpec, CreateOptions, Database, FormatVersion, Metadata,
    MetadataValue, Mutation,
};

const PRINCIPAL: &str = "abcdefghij27";
const OTHER: &str = "bcdefghij234";
const RECALL: &str = "/v1/memory/notes/recall";
const REMEMBER: &str = "/v1/memory/notes/remember";

/// One binding, one credential and one database a pin runs against.
#[derive(Clone)]
struct Setup {
    /// The exact scopes the binding grants the credential.
    scopes: Vec<Value>,
    /// Whether the binding owns an evidence collection.
    evidence: bool,
    /// Whether the evidence collection declares a lexical text field.
    text_index: bool,
    ops: Vec<credentials::Op>,
    /// Replaces the credential's graph names when set.
    graphs: Option<Vec<&'static str>>,
    /// Whether the credential names the evidence collection.
    collections: bool,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            scopes: vec![json!({"agent": "a"})],
            evidence: true,
            text_index: true,
            ops: vec![
                credentials::Op::Read,
                credentials::Op::Search,
                credentials::Op::Write,
                credentials::Op::GraphRead,
                credentials::Op::GraphWrite,
            ],
            graphs: None,
            collections: true,
        }
    }
}

struct Rig {
    router: Router,
    clock: Clock,
    service: Service,
}

fn rig(setup: &Setup) -> Rig {
    let mut db = Database::create_in_memory(
        2,
        CreateOptions {
            format: Some(FormatVersion::V5),
            ..Default::default()
        },
    )
    .unwrap();
    db.create_graph("notes_graph").unwrap();
    db.create_collection(
        "notes_ev",
        CollectionSpec {
            dimensions: 2,
            settings: CollectionSettings {
                text_fields: if setup.text_index {
                    vec!["text".into()]
                } else {
                    Vec::new()
                },
                ..Default::default()
            },
        },
    )
    .unwrap();
    let lineage = db.lineage().unwrap();
    let entry = json!({
        "binding": "notes",
        "tenant": "@root",
        "generation": 1,
        "state": "active",
        "graph": "notes_graph",
        "evidence_collection": if setup.evidence { json!("notes_ev") } else { Value::Null },
        "schema_version": "memory-schema-v1",
        "ranking_version": "memory-ranking-v1",
        "digest_version": "memory-request-v1",
        "receipt_window_ms": 60_000,
        "grants": [
            {"principal": OTHER, "scopes": [{"agent": "other"}]},
            {"principal": PRINCIPAL, "scopes": setup.scopes},
        ],
    });
    let text = json!({"version": "memory-bindings-v1", "bindings": [entry]}).to_string();
    let bindings = crate::memory::binding::parse(text.as_bytes()).unwrap();
    let service = Service::new(db).with_bindings(super::reserved::Bindings::new(bindings));
    super::startup(&service).unwrap();
    let mut grant = credentials::Grant::for_user(
        PRINCIPAL.into(),
        "pins".into(),
        None,
        &setup.ops,
        lineage,
        u64::MAX,
    );
    if let Some(graphs) = &setup.graphs {
        grant.graphs = graphs.iter().map(|graph| (*graph).to_owned()).collect();
    }
    if !setup.collections {
        grant.all = false;
        grant.collections.clear();
    }
    let clock = Clock::default();
    clock.now_ms.store(100, Ordering::SeqCst);
    let router = super::routes::routes()
        .with_state(service.clone())
        .layer(Extension(admin::Role::Scoped(Arc::new(grant))))
        .layer(Extension(RequestId("pins".into())))
        .layer(Extension(clock.clone()))
        .layer(DefaultBodyLimit::max(MAX_JSON_BYTES));
    Rig {
        router,
        clock,
        service,
    }
}

impl Rig {
    fn at(&self, now_ms: u64) {
        self.clock.now_ms.store(now_ms, Ordering::SeqCst);
    }

    async fn raw(&self, path: &str, bytes: Vec<u8>) -> (u16, Value) {
        let request = Request::builder()
            .method("POST")
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(bytes))
            .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status().as_u16();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&body).unwrap())
    }

    async fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        self.raw(path, serde_json::to_vec(body).unwrap()).await
    }

    async fn recall(&self, body: Value) -> (u16, Value) {
        self.post(RECALL, &body).await
    }

    /// One remember bundle through the real route; it must commit.
    async fn remember(&self, scope: Value, key: &str, items: Vec<Value>) {
        self.remember_with(scope, key, items, json!({})).await
    }

    async fn remember_with(&self, scope: Value, key: &str, items: Vec<Value>, extra: Value) {
        let mut body = json!({"scope": scope, "idempotency_key": key, "items": items});
        for (name, value) in extra.as_object().unwrap() {
            body[name] = value.clone();
        }
        let (status, response) = self.post(REMEMBER, &body).await;
        assert_eq!(status, 200, "remember {key}: {response}");
    }

    /// Runs `ops` as one graph batch on the bound graph.
    fn write(&self, ops: &[GraphOp]) {
        let mut owner = self.service.database.write().unwrap();
        let db = owner.as_mut().unwrap();
        db.graph_mut("notes_graph")
            .unwrap()
            .write_batch(ops)
            .unwrap();
    }

    /// Memories each backed by one evidence record, written through the core
    /// at `now` (the REST first slice refuses evidence on remember, so this
    /// is the only way a text or vector query finds anything).
    fn evidence(&self, scope: &MemoryScope, now: u64, memories: Vec<Evidenced>) {
        let ops: Vec<GraphOp> = memories
            .iter()
            .map(|memory| {
                let mut metadata = Metadata::new();
                metadata.insert("text".into(), MetadataValue::String(memory.text.clone()));
                GraphOp::Vector {
                    collection: "notes_ev".into(),
                    mutation: Mutation::PutWithMetadata {
                        id: record_id(&memory.id),
                        vector: memory.vector.to_vec(),
                        payload: memory.payload.clone(),
                        metadata,
                    },
                }
            })
            .collect();
        self.write(&ops);
        let request = RememberRequest {
            scope: scope.clone(),
            idempotency_key: format!("evidence-{}-{}", scope.agent, memories[0].id),
            receipt_window_ms: 60_000,
            now_ms: now,
            deadline_ms: None,
            allow_future_observations: false,
            items: memories
                .iter()
                .map(|memory| MemoryItem {
                    id: memory.id.clone(),
                    kind: MemoryKind::Entity,
                    content: format!("content {}", memory.id),
                    source: "s".into(),
                    importance: None,
                    observed_at_ms: Some(now as i64 - 10),
                    provenance: vec![Provenance {
                        source: "s".into(),
                        kind: Some("caller".into()),
                        uri: None,
                        transformation: None,
                    }],
                    references: Vec::new(),
                    merge_into: None,
                    expected_revision: None,
                    expiry_ms: None,
                    expiry_valid: true,
                    evidence: vec![MemoryEvidence {
                        id: memory.id.clone(),
                        scope: scope.clone(),
                    }],
                })
                .collect(),
        };
        let mut owner = self.service.database.write().unwrap();
        let db = owner.as_mut().unwrap();
        MemoryMut::bind(db, "notes_graph", Some("notes_ev"))
            .and_then(|mut memory| memory.remember(&request))
            .unwrap();
    }
}

/// One memory with its evidence record: vector, indexed text and snippet bytes.
struct Evidenced {
    id: String,
    vector: [f32; 2],
    text: String,
    payload: Vec<u8>,
}

fn evidenced(id: &str, vector: [f32; 2], payload: &[u8]) -> Evidenced {
    Evidenced {
        id: id.into(),
        vector,
        text: format!("memory {id}"),
        payload: payload.to_vec(),
    }
}

/// The core's evidence record number: the first eight SHA-256 bytes of the ID.
fn record_id(id: &str) -> u64 {
    u64::from_be_bytes(Sha256::digest(id.as_bytes())[..8].try_into().unwrap())
}

fn scope(agent: &str) -> MemoryScope {
    MemoryScope {
        tenant: "@root".into(),
        agent: agent.into(),
        user: None,
        session: None,
    }
}

fn item(id: &str, kind: &str, content: &str) -> Value {
    json!({
        "id": id,
        "kind": kind,
        "content": content,
        "observed_at_ms": "90",
        "source": {"source_id": "s", "source_kind": "caller"},
    })
}

/// `item` with extra top-level fields (importance, references, ...).
fn with(mut base: Value, extra: Value) -> Value {
    for (name, value) in extra.as_object().unwrap() {
        base[name] = value.clone();
    }
    base
}

fn scope_body(extra: Value) -> Value {
    let mut body = json!({"scope": {"agent": "a"}});
    for (name, value) in extra.as_object().unwrap() {
        body[name] = value.clone();
    }
    body
}

fn error(body: &Value) -> (&str, Option<&str>, Option<&str>) {
    (
        body["error"]["code"].as_str().unwrap_or("<none>"),
        body["error"]["details"]["field"].as_str(),
        body["error"]["details"]["feature"].as_str(),
    )
}

fn ids(body: &Value) -> Vec<&str> {
    body["memories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|memory| memory["id"].as_str().unwrap())
        .collect()
}

// ------------------------------------------------------------ Authorization

/// The seed is absent, so an authorized recall answers 404 and a denied one
/// answers the single 403 body.
fn probe() -> Value {
    scope_body(json!({"entity_seeds": ["absent"]}))
}

/// An unknown binding is the one 403 body every authorization denial shares.
async fn unknown_binding() -> Value {
    let (status, body) = rig(&Setup::default())
        .post("/v1/memory/absent/recall", &probe())
        .await;
    assert_eq!(status, 403, "{body}");
    body
}

async fn assert_denied(name: &str, setup: Setup, unknown: &Value) {
    let (status, body) = rig(&setup).recall(probe()).await;
    assert_eq!(status, 403, "{name}: {body}");
    assert_eq!(error(&body).0, "forbidden_scope", "{name}");
    assert_eq!(&body, unknown, "{name}: one 403 body for every denial");
}

/// Contract 6: recall needs `read` AND `search` on the evidence collection,
/// and the credential must name it; no write grant is needed to read.
#[tokio::test]
async fn recall_needs_read_and_search_on_the_evidence_collection() {
    use credentials::Op::*;
    for ops in [
        vec![Read, Search, GraphRead],
        vec![Read, Search, GraphRead, Write, GraphWrite],
    ] {
        let (status, body) = rig(&Setup {
            ops,
            ..Setup::default()
        })
        .recall(probe())
        .await;
        assert_eq!(status, 404, "{body}");
        assert_eq!(error(&body).0, "memory_not_found");
    }
    let unknown = unknown_binding().await;
    for (name, setup) in [
        (
            "no read",
            Setup {
                ops: vec![Search, GraphRead],
                ..Setup::default()
            },
        ),
        (
            "no search",
            Setup {
                ops: vec![Read, GraphRead],
                ..Setup::default()
            },
        ),
        (
            "collection not named",
            Setup {
                collections: false,
                ..Setup::default()
            },
        ),
    ] {
        assert_denied(name, setup, &unknown).await;
    }
}

/// Contract 6 and mapping 6 (authorization is the intersection): the bound
/// graph needs its own grant; evidence-collection and exact-scope grants
/// never replace it.
#[tokio::test]
async fn recall_needs_the_graph_grant_on_the_bound_graph() {
    use credentials::Op::*;
    let unknown = unknown_binding().await;
    for (name, setup) in [
        (
            "no graph operation",
            Setup {
                ops: vec![Read, Search],
                ..Setup::default()
            },
        ),
        (
            "another graph",
            Setup {
                graphs: Some(vec!["other_graph"]),
                ..Setup::default()
            },
        ),
    ] {
        assert_denied(name, setup, &unknown).await;
    }
}

// ------------------------------------------------------------------ Fixtures

fn merge(body: &mut Value, patch: Value) {
    for (name, value) in patch.as_object().unwrap() {
        body[name] = value.clone();
    }
}

fn m(n: usize) -> String {
    format!("m{n:02}")
}

/// `m00` references `m01..m24`: 25 entity memories in scope `a`, all with the
/// default importance; seeding `m00` reaches every one of them.
async fn family() -> Rig {
    let rig = rig(&Setup::default());
    let references: Vec<Value> = (1..=24)
        .map(|n| json!({"id": m(n), "revision": "0"}))
        .collect();
    let mut items = vec![with(
        item("m00", "entity", "root"),
        json!({"references": references}),
    )];
    items.extend((1..=24).map(|n| item(&m(n), "entity", &format!("leaf {n}"))));
    rig.remember(json!({"agent": "a"}), "family", items).await;
    rig
}

/// A seeded recall of the family; the path ceiling is raised so 24 expanded
/// memories stay within the declared work bounds, and the deadline budget is
/// generous so a loaded machine never turns real elapsed time into a 504.
fn family_recall(extra: Value) -> Value {
    let mut body = scope_body(json!({
        "entity_seeds": ["m00"],
        "limits": {"max_output_paths": 100, "deadline_ms": 5_000},
    }));
    merge(&mut body, extra);
    body
}

// ------------------------------------------------------ Strict JSON and bounds

/// Each case is one rejected request. The store is empty, so a defect that
/// let the request through would surface as a 404 (or 200), never as the 422.
/// With a field name the 422 must also name it in `details.field`; without
/// one only the status and code are pinned (the contract names no field).
macro_rules! rejects {
    ($name:ident, $patch:expr) => {
        #[tokio::test]
        async fn $name() {
            let rig = rig(&Setup::default());
            let mut body = scope_body(json!({"entity_seeds": ["m00"]}));
            merge(&mut body, $patch);
            let (status, response) = rig.recall(body).await;
            assert_eq!(status, 422, "{response}");
            assert_eq!(error(&response).0, "invalid_input", "{response}");
        }
    };
    ($name:ident, $patch:expr, $field:expr) => {
        #[tokio::test]
        async fn $name() {
            let rig = rig(&Setup::default());
            let mut body = scope_body(json!({"entity_seeds": ["m00"]}));
            merge(&mut body, $patch);
            let (status, response) = rig.recall(body).await;
            assert_eq!(status, 422, "{response}");
            assert_eq!(
                error(&response),
                ("invalid_input", Some($field), None),
                "{response}"
            );
        }
    };
}

// Contract 4: `limit` is 1..=100 and an exact JSON integer (contract 3).
rejects!(
    limit_zero_is_rejected_not_clamped,
    json!({"limit": 0}),
    "limit"
);
rejects!(limit_101_is_rejected, json!({"limit": 101}), "limit");
rejects!(
    limit_true_is_not_an_integer,
    json!({"limit": true}),
    "limit"
);
rejects!(
    limit_fraction_is_not_an_integer,
    json!({"limit": 1.5}),
    "limit"
);
rejects!(
    limit_string_is_not_an_integer,
    json!({"limit": "5"}),
    "limit"
);
// Contract 3: required and optional fields are never `null`; objects are
// closed; budgets stay JSON integers and booleans are booleans.
rejects!(
    ranking_null_is_not_omitted,
    json!({"ranking": null}),
    "ranking"
);
rejects!(
    limits_null_is_not_omitted,
    json!({"limits": null}),
    "limits"
);
rejects!(
    projection_null_is_not_omitted,
    json!({"projection": null}),
    "projection"
);
rejects!(kinds_null_is_not_omitted, json!({"kinds": null}), "kinds");
rejects!(
    seeds_null_is_not_omitted,
    json!({"entity_seeds": null}),
    "entity_seeds"
);
rejects!(
    budget_decimal_string_is_not_an_integer,
    json!({"limits": {"deadline_ms": "250"}})
);
rejects!(
    budget_below_its_minimum_is_rejected,
    json!({"limits": {"deadline_ms": 0}})
);
rejects!(
    ranking_is_a_closed_object,
    json!({"ranking": {"selection": "weighted", "bogus": 1}})
);
rejects!(limits_is_a_closed_object, json!({"limits": {"bogus": 1}}));
rejects!(unknown_top_level_key_is_rejected, json!({"bogus": 1}));
rejects!(
    allow_partial_integer_is_not_a_boolean,
    json!({"allow_partial": 1}),
    "allow_partial"
);
// Contract 4: `text` is 1..=16384 bytes, so empty text names its field.
rejects!(empty_text_names_its_field, json!({"text": ""}), "text");

fn seed_ids(count: usize) -> Value {
    json!((0..count).map(|n| format!("d{n:02}")).collect::<Vec<_>>())
}

// Contract 4 shared limits: at most 64 entity seeds, rejected before any
// discovery even though the candidate budget (200) would hold them.
rejects!(
    sixty_five_seeds_are_rejected_before_discovery,
    json!({"entity_seeds": seed_ids(65)}),
    "entity_seeds"
);

/// Contract 4: 64 seeds are supported, each charged once as a candidate.
#[tokio::test]
async fn recall_accepts_sixty_four_seeds() {
    let rig = rig(&Setup::default());
    let items = (0..64)
        .map(|n| item(&format!("d{n:02}"), "entity", &format!("seed {n}")))
        .collect();
    rig.remember(json!({"agent": "a"}), "seeds", items).await;
    let (status, body) = rig
        .recall(scope_body(json!({
            "entity_seeds": seed_ids(64),
            "limits": {"max_output_paths": 100, "deadline_ms": 5_000},
        })))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["meta"]["candidates_examined"], "64");
    assert_eq!(body["memories"].as_array().unwrap().len(), 20);
}

/// Contract 4: `entity_seeds: []` contributes nothing, so text or a vector
/// still serves; seeds alone, an empty list alone and nothing at all do not.
#[tokio::test]
async fn recall_accepts_zero_seeds_beside_text() {
    let rig = rig(&Setup::default());
    let (status, body) = rig
        .recall(scope_body(json!({"text": "memory", "entity_seeds": []})))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["memories"], json!([]));
    assert_eq!(body["meta"]["complete"], true);
    assert_eq!(body["meta"]["mode"], "exact_enumeration");
    for request in [json!({"entity_seeds": []}), json!({})] {
        let (status, body) = rig.recall(scope_body(request)).await;
        assert_eq!(status, 422, "{body}");
        assert_eq!(error(&body).0, "invalid_input", "{body}");
    }
}

// ----------------------------------------------------------------- Limit

/// Asks the family for `extra` and checks the first `expected` IDs in order:
/// every score ties at zero under the default weights, so the order is the
/// ascending memory ID (contract 4, weighted final ordering).
async fn family_hits(extra: Value, expected: usize) {
    let rig = family().await;
    let (status, body) = rig.recall(family_recall(extra.clone())).await;
    assert_eq!(status, 200, "{extra}: {body}");
    let all: Vec<String> = (0..=24).map(m).collect();
    let wanted: Vec<&str> = all[..expected].iter().map(String::as_str).collect();
    assert_eq!(ids(&body), wanted, "{extra}");
}

/// Contract 4: `limit` defaults to 20.
#[tokio::test]
async fn recall_limit_defaults_to_twenty() {
    family_hits(json!({}), 20).await;
}

/// Contract 4: the requested limit caps the ranked output.
#[tokio::test]
async fn recall_limit_caps_the_ranked_output() {
    family_hits(json!({"limit": 3}), 3).await;
    family_hits(json!({"limit": 1}), 1).await;
}

/// Contract 4: 100 is a valid limit (ceiling inclusive).
#[tokio::test]
async fn recall_limit_one_hundred_is_accepted() {
    family_hits(json!({"limit": 100}), 25).await;
}

// ----------------------------------------------------------------------- Text

fn query_length_refusal(status: u16, body: &Value) {
    assert_eq!(status, 409, "{body}");
    assert_eq!(
        error(body),
        (
            "memory_capability_unavailable",
            None,
            Some("query_text_length")
        )
    );
}

/// Mapping 6: 16384 decoded bytes still name the query-length capability
/// refusal; one byte more is invalid input.
#[tokio::test]
async fn text_of_16384_bytes_is_the_query_length_refusal() {
    let rig = rig(&Setup::default());
    let (status, body) = rig
        .recall(scope_body(json!({"text": "x".repeat(16_384)})))
        .await;
    query_length_refusal(status, &body);
    let (status, body) = rig
        .recall(scope_body(json!({"text": "x".repeat(16_385)})))
        .await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(error(&body).1, Some("text"));
}

/// Contract 3 and mapping 6: the 1024-byte core cap and the 16384-byte
/// contract cap count decoded UTF-8 bytes, not characters.
#[tokio::test]
async fn text_length_thresholds_count_utf8_bytes() {
    let rig = rig(&Setup::default());
    // 600 characters, 1200 bytes: beyond the core's 1024.
    let (status, body) = rig
        .recall(scope_body(json!({"text": "é".repeat(600)})))
        .await;
    query_length_refusal(status, &body);
    // 8192 characters, 16384 bytes: still the capability refusal.
    let (status, body) = rig
        .recall(scope_body(json!({"text": "é".repeat(8_192)})))
        .await;
    query_length_refusal(status, &body);
    // 8193 characters, 16386 bytes: invalid input.
    let (status, body) = rig
        .recall(scope_body(json!({"text": "é".repeat(8_193)})))
        .await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(error(&body).1, Some("text"));
    // Exactly 1024 bytes is served (an empty store has nothing to find);
    // 1025 bytes is refused.
    let (status, body) = rig
        .recall(scope_body(json!({"text": "ada ".repeat(256)})))
        .await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = rig
        .recall(scope_body(
            json!({"text": format!("{}a", "ada ".repeat(256))}),
        ))
        .await;
    query_length_refusal(status, &body);
}

// -------------------------------------------------------------------- Refusals

/// A request carrying the named unsupported fields and an invalid `limit`, so
/// a refusal that did not run before validation would surface as a 422.
fn refused(features: &[&str]) -> Value {
    let mut body = scope_body(json!({"limit": 0}));
    for feature in features {
        match *feature {
            "embedding_provider" => body["embedding_provider"] = json!("remote"),
            "fact_kinds" => body["kinds"] = json!(["entity", "relation"]),
            "filter" => body["filter"] = json!({"all": []}),
            "temporal_read" => body["as_of_ms"] = json!("5"),
            "paths" => body["projection"]["paths"] = json!(true),
            "metadata" => body["projection"]["metadata"] = json!(["k"]),
            "text" => body["text"] = json!("x".repeat(2_000)),
            "short_text" => body["text"] = json!("memory"),
            "vector" => body["vector"] = json!([1, 0]),
            other => panic!("unknown refusal {other}"),
        }
    }
    body
}

/// The refusals only: each case names the feature and the fields it combines.
/// Only combinations whose winner does not depend on how a refusal order is
/// read (JSON text order, key order or schema order; OPEN-G1R-02) are pinned.
async fn assert_refused(rig: &Rig, cases: &[(&[&str], &str)]) {
    for (features, named) in cases {
        let (status, body) = rig.recall(refused(features)).await;
        assert_eq!(status, 409, "{features:?}: {body}");
        assert_eq!(
            error(&body),
            ("memory_capability_unavailable", None, Some(*named)),
            "{features:?}"
        );
    }
}

/// Mapping 1/2/6, API first-slice limits: each unsupported field is refused
/// by name after authorization and before validation (`limit: 0` is never
/// reached).
#[tokio::test]
async fn refusals_name_their_feature_before_validation() {
    let rig = rig(&Setup::default());
    assert_refused(
        &rig,
        &[
            (&["embedding_provider"], "embedding_provider"),
            (&["fact_kinds"], "fact_kinds"),
            (&["filter"], "filter"),
            (&["temporal_read"], "temporal_read"),
            (&["paths"], "paths"),
            (&["metadata"], "metadata"),
            (&["text"], "query_text_length"),
        ],
    )
    .await;
}

/// Mapping 2: when both refusals apply, `embedding_provider` wins over
/// `filter` and over the later projection and text refusals.
#[tokio::test]
async fn embedding_provider_is_refused_before_filter_and_the_rest() {
    let rig = rig(&Setup::default());
    assert_refused(
        &rig,
        &[
            (&["embedding_provider", "filter"], "embedding_provider"),
            (
                &["embedding_provider", "paths", "text"],
                "embedding_provider",
            ),
            (&["filter", "paths", "text"], "filter"),
            (&["fact_kinds", "metadata", "text"], "fact_kinds"),
        ],
    )
    .await;
}

/// Mapping 6: text or a vector on a binding without an evidence collection
/// is `evidence_recall` (not the query-length refusal, whatever the length)
/// and follows the projection refusals; seeds alone need no evidence
/// collection.
#[tokio::test]
async fn text_or_vector_without_an_evidence_collection_is_evidence_recall() {
    let rig = rig(&Setup {
        evidence: false,
        ..Setup::default()
    });
    assert_refused(
        &rig,
        &[
            (&["text"], "evidence_recall"),
            (&["short_text"], "evidence_recall"),
            (&["vector"], "evidence_recall"),
            (&["metadata", "text"], "metadata"),
            (&["paths", "vector"], "paths"),
        ],
    )
    .await;
    let (status, body) = rig
        .recall(scope_body(json!({"entity_seeds": ["absent"]})))
        .await;
    assert_eq!(status, 404, "{body}");
}

/// Mapping 6: an unrefused query of any length up to the core cap reaches
/// validation, where the invalid limit is a 422.
#[tokio::test]
async fn text_within_the_core_cap_is_validated_not_refused() {
    let rig = rig(&Setup::default());
    let (status, body) = rig
        .recall(scope_body(json!({"text": "x".repeat(1_024), "limit": 0})))
        .await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(error(&body).1, Some("limit"));
}

// --------------------------------------------------------------------- Scopes

fn effective(agent: &str, user: Option<&str>) -> Value {
    let mut scope = json!({"tenant": "@root", "agent": agent});
    if let Some(user) = user {
        scope["user"] = json!(user);
    }
    scope
}

/// Contract 3/4: an absent `scopes` is the single required scope; no parent
/// or shared scope is added, even one the credential is also granted.
#[tokio::test]
async fn recall_scopes_default_to_the_single_required_scope() {
    let rig = rig(&Setup {
        scopes: vec![json!({"agent": "a"}), json!({"agent": "a", "user": "u"})],
        ..Setup::default()
    });
    rig.remember(
        json!({"agent": "a"}),
        "parent",
        vec![item("p1", "entity", "shared")],
    )
    .await;
    rig.remember(
        json!({"agent": "a", "user": "u"}),
        "child",
        vec![item("c1", "entity", "private")],
    )
    .await;
    let (status, body) = rig
        .recall(json!({"scope": {"agent": "a", "user": "u"}, "entity_seeds": ["c1"]}))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["meta"]["scopes"], json!([effective("a", Some("u"))]));
    assert_eq!(ids(&body), ["c1"]);
}

fn agents(count: usize) -> Vec<Value> {
    (1..=count)
        .map(|n| json!({"agent": format!("s{n:02}")}))
        .collect()
}

/// Contract 4: 1-16 distinct scopes; all of them are the read allowlist.
#[tokio::test]
async fn recall_accepts_sixteen_scopes() {
    let rig = rig(&Setup {
        scopes: agents(17),
        ..Setup::default()
    });
    rig.remember(
        json!({"agent": "s01"}),
        "one",
        vec![item("x1", "entity", "x")],
    )
    .await;
    let (status, body) = rig
        .recall(json!({"scope": {"agent": "s01"}, "scopes": agents(16), "entity_seeds": ["x1"]}))
        .await;
    assert_eq!(status, 200, "{body}");
    let mut listed: Vec<String> = body["meta"]["scopes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|scope| scope["agent"].as_str().unwrap().to_owned())
        .collect();
    listed.sort();
    let wanted: Vec<String> = (1..=16).map(|n| format!("s{n:02}")).collect();
    assert_eq!(listed, wanted);
}

/// Contract 4/9: 17 scopes are rejected before execution, never clamped.
#[tokio::test]
async fn recall_rejects_seventeen_scopes_instead_of_clamping() {
    let rig = rig(&Setup {
        scopes: agents(17),
        ..Setup::default()
    });
    let (status, body) = rig
        .recall(json!({"scope": {"agent": "s01"}, "scopes": agents(17), "entity_seeds": ["x1"]}))
        .await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(error(&body).1, Some("scopes"));
}

// ---------------------------------------------------------------------- Kinds

/// `k0` (entity) references `k1` (episodic) and `k2` (entity).
async fn kinds_rig() -> Rig {
    let rig = rig(&Setup::default());
    rig.remember(
        json!({"agent": "a"}),
        "kinds",
        vec![
            with(
                item("k0", "entity", "root"),
                json!({"references": [
                    {"id": "k1", "revision": "0"},
                    {"id": "k2", "revision": "0"},
                ]}),
            ),
            item("k1", "episodic", "summary"),
            item("k2", "entity", "leaf"),
        ],
    )
    .await;
    rig
}

async fn kinds_ids(kinds: Option<Value>) -> Vec<String> {
    let rig = kinds_rig().await;
    let mut body = scope_body(json!({"entity_seeds": ["k0"]}));
    if let Some(kinds) = kinds {
        body["kinds"] = kinds;
    }
    let (status, response) = rig.recall(body).await;
    assert_eq!(status, 200, "{response}");
    ids(&response).into_iter().map(str::to_owned).collect()
}

/// Contract 4/8, mapping 1: an absent `kinds` is every served kind.
#[tokio::test]
async fn recall_kinds_default_to_every_served_kind() {
    assert_eq!(kinds_ids(None).await, ["k0", "k1", "k2"]);
}

/// Contract 4 limits: both served kinds fit the four-kind ceiling.
#[tokio::test]
async fn recall_accepts_both_served_kinds() {
    let both = json!(["entity", "episodic"]);
    assert_eq!(kinds_ids(Some(both)).await, ["k0", "k1", "k2"]);
}

/// Contract 4: an entity-only request returns no episodic memory, even one
/// an entity references.
#[tokio::test]
async fn recall_kinds_filter_what_the_request_names() {
    assert_eq!(kinds_ids(Some(json!(["entity"]))).await, ["k0", "k2"]);
}

// -------------------------------------------------------------------- Ranking

/// `w0` (importance 0.5) references `w1` (0.9) and `w2` (0.1).
async fn weighted_rig() -> Rig {
    let rig = rig(&Setup::default());
    let refs = json!([
        {"id": "w1", "revision": "0"},
        {"id": "w2", "revision": "0"},
    ]);
    rig.remember(
        json!({"agent": "a"}),
        "weights",
        vec![
            with(
                item("w0", "entity", "root"),
                json!({"importance": 0.5, "references": refs}),
            ),
            with(item("w1", "entity", "high"), json!({"importance": 0.9})),
            with(item("w2", "entity", "low"), json!({"importance": 0.1})),
        ],
    )
    .await;
    rig
}

fn number(value: &Value) -> f64 {
    value.as_f64().unwrap_or(f64::NAN)
}

/// Contract 4: weights are `[R, D, I]` with no renormalization, so weights
/// `[0, 0, 1]` rank by importance and `S` is exactly `I`; expanded-only
/// candidates have `R = 0` and no decay without a half-life.
#[tokio::test]
async fn ranking_weights_apply_in_relevance_decay_importance_order() {
    let rig = weighted_rig().await;
    let (status, body) = rig
        .recall(scope_body(json!({
            "entity_seeds": ["w0"],
            "ranking": {"weights": [0, 0, 1]},
        })))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(ids(&body), ["w1", "w0", "w2"]);
    for (memory, importance) in body["memories"]
        .as_array()
        .unwrap()
        .iter()
        .zip([0.9, 0.5, 0.1])
    {
        let scores = &memory["scores"];
        assert_eq!(number(&scores["S"]), importance);
        assert_eq!(number(&scores["I"]), importance);
        assert_eq!(number(&scores["R"]), 0.0);
        assert_eq!(scores["D"], Value::Null);
    }
}

/// Contract 4: under the default weights `[1, 0, 0]` every expanded-only
/// candidate scores zero, so the final order is the ascending memory ID.
#[tokio::test]
async fn ranking_default_weights_order_ties_by_memory_id() {
    let rig = weighted_rig().await;
    let (status, body) = rig
        .recall(scope_body(json!({"entity_seeds": ["w0"]})))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(ids(&body), ["w0", "w1", "w2"]);
}

/// Contract 4: `D = 1/(1 + age/half_life_ms)` with `age = T - observed_at`
/// in milliseconds: observed 90, T = 100, half-life 10 gives `D = 1/2`.
#[tokio::test]
async fn ranking_half_life_is_in_milliseconds() {
    let rig = rig(&Setup::default());
    rig.remember(
        json!({"agent": "a"}),
        "decay",
        vec![item("h0", "entity", "x")],
    )
    .await;
    let (status, body) = rig
        .recall(scope_body(json!({
            "entity_seeds": ["h0"],
            "ranking": {"weights": [0, 1, 0], "half_life_ms": 10},
        })))
        .await;
    assert_eq!(status, 200, "{body}");
    let scores = &body["memories"][0]["scores"];
    assert_eq!(number(&scores["D"]), 0.5);
    assert_eq!(number(&scores["S"]), 0.5);
    assert_eq!(number(&scores["I"]), 0.5);
    assert_eq!(body["meta"]["as_of_ms"], "100");
    assert_eq!(body["meta"]["ttl_clock_ms"], "100");
}

/// Contract 4: a future observation (`observed_at > T`) is rejected by
/// default and by `reject`; explicit `clamp` treats its age as zero.
#[tokio::test]
async fn ranking_future_observations_reject_by_default_and_clamp_on_request() {
    let rig = rig(&Setup::default());
    rig.at(1_000);
    rig.remember(
        json!({"agent": "a"}),
        "future",
        vec![with(
            item("f0", "entity", "x"),
            json!({"observed_at_ms": "900"}),
        )],
    )
    .await;
    rig.at(500);
    for ranking in [json!({}), json!({"future_observations": "reject"})] {
        let (status, body) = rig
            .recall(scope_body(
                json!({"entity_seeds": ["f0"], "ranking": ranking}),
            ))
            .await;
        assert_eq!(status, 422, "{ranking}: {body}");
        assert_eq!(error(&body), ("invalid_input", None, None), "{ranking}");
    }
    let (status, body) = rig
        .recall(scope_body(json!({"entity_seeds": ["f0"]})))
        .await;
    assert_eq!(status, 422, "{body}");
    let (status, body) = rig
        .recall(scope_body(json!({
            "entity_seeds": ["f0"],
            "ranking": {
                "weights": [0, 1, 0],
                "half_life_ms": 10,
                "future_observations": "clamp",
            },
        })))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(number(&body["memories"][0]["scores"]["D"]), 1.0);
}

/// Contract 4: `exact` and `mmr` are vector-only and refuse a decay
/// half-life (and every other blend) as `memory_unsupported_ranking`.
#[tokio::test]
async fn ranking_exact_and_mmr_refuse_a_half_life() {
    let rig = rig(&Setup::default());
    for selection in ["exact", "mmr"] {
        let (status, body) = rig
            .recall(scope_body(json!({
                "vector": [1, 0],
                "ranking": {"selection": selection, "half_life_ms": 10},
            })))
            .await;
        assert_eq!(status, 422, "{selection}: {body}");
        assert_eq!(
            error(&body),
            ("memory_unsupported_ranking", None, None),
            "{selection}"
        );
    }
}

/// Three memories whose vectors sit at 10, 12 and -60 degrees from the query
/// `[1, 0]`: `ma` and `mb` are near duplicates, `mc` is diverse.
fn angled() -> Vec<Evidenced> {
    vec![
        evidenced("ma", [0.98481, 0.17365], b"a"),
        evidenced("mb", [0.97815, 0.20791], b"b"),
        evidenced("mc", [0.5, -0.86603], b"c"),
    ]
}

/// Contract 4: MMR `lambda` defaults to 0.5. By hand with
/// `MMR = lambda*cos(d,q) - (1-lambda)*max cos(d,selected)`: after `ma`
/// (cosines 0.98481, 0.97815, 0.5 to the query), at 0.5 `mb` scores
/// 0.5*0.97815 - 0.5*0.99939 < 0 and `mc` scores 0.5*0.5 - 0.5*0.34202 > 0,
/// so `mc` precedes `mb`; at lambda 1 pure relevance keeps `mb` second.
#[tokio::test]
async fn ranking_mmr_lambda_defaults_to_one_half() {
    let rig = rig(&Setup::default());
    rig.evidence(&scope("a"), 100, angled());
    for (ranking, expected) in [
        (json!({"selection": "mmr"}), ["ma", "mc", "mb"]),
        (
            json!({"selection": "mmr", "lambda": 0.5}),
            ["ma", "mc", "mb"],
        ),
        (json!({"selection": "mmr", "lambda": 1}), ["ma", "mb", "mc"]),
    ] {
        let (status, body) = rig
            .recall(scope_body(json!({"vector": [1, 0], "ranking": ranking})))
            .await;
        assert_eq!(status, 200, "{ranking}: {body}");
        assert_eq!(ids(&body), expected, "{ranking}");
        assert_eq!(body["selection"], "mmr");
    }
}

// -------------------------------------------------------------------- Budgets

/// Contract 4/6, mapping 2: an exhausted graph budget is 422 `invalid_input`
/// naming the bound, never a partial answer; `allow_partial: true` still
/// refuses (docs/API.md first-slice limits) as `memory_bounded_plan_unsupported`.
#[tokio::test]
async fn graph_budget_exhaustion_names_its_bound_and_never_returns_partial() {
    let rig = family().await;
    let limits = json!({"max_visited_nodes": 1, "max_output_paths": 100});
    for allow_partial in [false, true] {
        let (status, body) = rig
            .recall(family_recall(
                json!({"limits": limits, "allow_partial": allow_partial}),
            ))
            .await;
        assert_eq!(status, 422, "{allow_partial}: {body}");
        let expected = if allow_partial {
            ("memory_bounded_plan_unsupported", None, None)
        } else {
            ("invalid_input", Some("graph"), None)
        };
        assert_eq!(error(&body), expected, "{allow_partial}: {body}");
    }
}

/// Mapping 2: the candidate budget names `candidates` (two seeds against a
/// budget of one); the same refusal under `allow_partial` is the bounded-plan
/// refusal.
#[tokio::test]
async fn candidate_budget_exhaustion_names_its_bound() {
    let rig = family().await;
    for (allow_partial, expected) in [
        (false, ("invalid_input", Some("candidates"), None)),
        (true, ("memory_bounded_plan_unsupported", None, None)),
    ] {
        let (status, body) = rig
            .recall(scope_body(json!({
                "entity_seeds": ["m00", "m01"],
                "limit": 1,
                "limits": {"candidates": 1},
                "allow_partial": allow_partial,
            })))
            .await;
        assert_eq!(status, 422, "{allow_partial}: {body}");
        assert_eq!(error(&body), expected, "{allow_partial}: {body}");
    }
}

// ---------------------------------------------------- Candidates and scopes

/// Contract 4: the candidate union defaults to 200 and duplicate
/// examinations are charged across legs. A vector leg and a text leg over
/// 100 evidence-backed memories examine exactly 200: the default holds it,
/// a budget of 199 (or 100) does not, and 200 is an accepted budget.
#[tokio::test]
async fn recall_candidates_default_to_two_hundred_charged_per_leg() {
    let rig = rig(&Setup::default());
    let memories = (0..100)
        .map(|n| Evidenced {
            id: format!("c{n:03}"),
            vector: [1.0, n as f32 / 100.0],
            text: format!("memory c{n:03}"),
            payload: b"p".to_vec(),
        })
        .collect();
    rig.evidence(&scope("a"), 100, memories);
    let query = scope_body(json!({
        "vector": [1, 0],
        "text": "memory",
        "limits": {"deadline_ms": 5_000},
    }));
    let (status, body) = rig.recall(query.clone()).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["meta"]["candidates_examined"], "200");
    assert_eq!(body["memories"].as_array().unwrap().len(), 20);
    for (budget, status, field) in [
        (200, 200, None),
        (1_000, 200, None),
        (199, 422, Some("candidates")),
        (100, 422, Some("candidates")),
    ] {
        let mut limited = query.clone();
        limited["limits"] = json!({"candidates": budget, "deadline_ms": 5_000});
        let (served, body) = rig.recall(limited).await;
        assert_eq!(served, status, "{budget}: {body}");
        if status == 422 {
            assert_eq!(error(&body), ("invalid_input", field, None), "{budget}");
        }
    }
}

/// Contract 3/4: every scope of `scopes` is read, not just the first; a
/// single-scope request reads only that scope.
#[tokio::test]
async fn recall_reads_every_requested_scope() {
    let rig = rig(&Setup {
        scopes: vec![json!({"agent": "a"}), json!({"agent": "b"})],
        ..Setup::default()
    });
    rig.evidence(&scope("a"), 100, vec![evidenced("ea", [1.0, 0.0], b"a")]);
    rig.evidence(&scope("b"), 100, vec![evidenced("eb", [0.9, 0.4359], b"b")]);
    let (status, body) = rig.recall(scope_body(json!({"vector": [1, 0]}))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(ids(&body), ["ea"]);
    for (scope, scopes) in [
        (
            json!({"agent": "a"}),
            json!([{"agent": "a"}, {"agent": "b"}]),
        ),
        (
            json!({"agent": "b"}),
            json!([{"agent": "b"}, {"agent": "a"}]),
        ),
    ] {
        let (status, body) = rig
            .recall(json!({"scope": scope, "scopes": scopes, "vector": [1, 0]}))
            .await;
        assert_eq!(status, 200, "{body}");
        // Relevance ranks the nearer vector first; each hit names its scope.
        assert_eq!(ids(&body), ["ea", "eb"]);
        assert_eq!(body["memories"][0]["scope"], effective("a", None));
        assert_eq!(body["memories"][1]["scope"], effective("b", None));
        let mut listed = body["meta"]["scopes"].as_array().unwrap().clone();
        listed.sort_by_key(|scope| scope["agent"].as_str().unwrap().to_owned());
        assert_eq!(listed, [effective("a", None), effective("b", None)]);
    }
}

/// The seeding helper numbers evidence records as the core does (the core
/// pins `evidence_record_id("e")` to this value), so a changed derivation
/// fails here instead of as an opaque seeding error.
#[test]
fn evidence_record_ids_follow_the_core_definition() {
    assert_eq!(record_id("e"), 0x3f79_bb7b_435b_0532);
}
