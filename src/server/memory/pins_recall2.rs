//! Recall pins, second half of the mutation closure: response shape, ranking
//! policy, partial and barrier reporting, and core error mapping. Expected
//! values come from the serving contract (sections 3, 4, 6, 8), the adapter
//! mapping (sections 1, 2, 6 and the 0.7.0 freeze addendum), the E1 barrier
//! contract and `docs/API.md`, never from adapter output. Nothing here waits
//! on measured time: the `Clock` seam fixes the engine clock, and every
//! request asks for the 5,000 ms ceiling so a slow machine cannot trip the
//! core's own wall-clock timer.

use sha2::{Digest, Sha256};

use super::*;
use crate::memory::MemoryEvidence;
use crate::property_graph::{FindNodes, GraphOp, NodePatch, ReadOptions};
use crate::server::credentials::Grant;
use crate::{
    CollectionSettings, CollectionSpec, Condition, CreateOptions, Expected, FormatVersion,
    Metadata, MetadataValue, Mutation,
};

use axum::{body::Body, extract::DefaultBodyLimit, http::Request};
use http_body_util::BodyExt;
use tower::ServiceExt;

const PRINCIPAL: &str = "abcdefghij27";

fn scope(agent: &str) -> MemoryScope {
    MemoryScope {
        tenant: "@root".into(),
        agent: agent.into(),
        user: None,
        session: None,
    }
}

fn hex(digest: impl std::fmt::LowerHex) -> String {
    format!("{digest:x}")
}

/// `ScopeKey` of the E1 contract: `scope:v1:` plus the hex SHA-256 of
/// ASCII `scope:v1` followed by O(tenant), O(agent), O(user), O(session).
fn scope_key(scope: &MemoryScope) -> String {
    let mut bytes = b"scope:v1".to_vec();
    for value in [
        Some(scope.tenant.as_str()),
        Some(scope.agent.as_str()),
        scope.user.as_deref(),
        scope.session.as_deref(),
    ] {
        match value {
            None => bytes.push(0),
            Some(value) => {
                bytes.push(1);
                bytes.extend_from_slice(&(value.len() as u32).to_be_bytes());
                bytes.extend_from_slice(value.as_bytes());
            }
        }
    }
    format!("scope:v1:{}", hex(Sha256::digest(bytes)))
}

/// `JobKey = R("erase-scope-job", "singleton")` of the E1 contract, whose
/// presence is the permanent exact-scope barrier.
fn job_key(scope_key: &str) -> String {
    let mut input = b"erase-scope-job:v1\0".to_vec();
    input.extend_from_slice(b"singleton");
    format!("receipt:v1:{scope_key}:{}", hex(Sha256::digest(input)))
}

/// The evidence record ID of a memory's evidence reference: the first eight
/// bytes of SHA-256 of the reference ID, big endian (core test
/// `evidence_record_id_is_pinned`: `"e"` is `0x3f79bb7b435b0532`).
fn record_id(id: &str) -> u64 {
    u64::from_be_bytes(Sha256::digest(id.as_bytes())[..8].try_into().unwrap())
}

struct Env {
    router: Router,
    clock: Clock,
    service: Service,
}

impl Env {
    /// One binding `notes` (graph `notes_graph`, evidence `notes_ev` of two
    /// dimensions) whose credential holds scopes `a` and `b`, at clock 100.
    fn new() -> Self {
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
                    text_fields: vec!["text".into()],
                    ..Default::default()
                },
            },
        )
        .unwrap();
        let lineage = db.lineage().unwrap();
        let entry = json!({
            "binding": "notes", "tenant": "@root", "generation": 1, "state": "active",
            "graph": "notes_graph", "evidence_collection": "notes_ev",
            "schema_version": "memory-schema-v1", "ranking_version": "memory-ranking-v1",
            "digest_version": "memory-request-v1", "receipt_window_ms": 86_400_000,
            "grants": [{"principal": PRINCIPAL, "scopes": [{"agent": "a"}, {"agent": "b"}]}],
        });
        let text = json!({"version": "memory-bindings-v1", "bindings": [entry]}).to_string();
        let bindings = crate::memory::binding::parse(text.as_bytes()).unwrap();
        let service =
            Service::new(db).with_bindings(crate::server::reserved::Bindings::new(bindings));
        crate::server::memory::startup(&service).unwrap();
        let grant = Grant::for_user(
            PRINCIPAL.into(),
            "recall2".into(),
            None,
            &[
                Op::Read,
                Op::Search,
                Op::Write,
                Op::GraphRead,
                Op::GraphWrite,
            ],
            lineage,
            u64::MAX,
        );
        let clock = Clock::default();
        let router = super::super::routes()
            .with_state(service.clone())
            .layer(Extension(admin::Role::Scoped(Arc::new(grant))))
            .layer(Extension(RequestId("recall2".into())))
            .layer(Extension(clock.clone()))
            .layer(DefaultBodyLimit::max(crate::server::MAX_JSON_BYTES));
        let env = Self {
            router,
            clock,
            service,
        };
        env.at(100);
        env
    }

    fn at(&self, now_ms: u64) {
        self.clock.now_ms.store(now_ms, Ordering::SeqCst);
    }

    async fn post(&self, route: &str, body: &Value) -> (u16, Value) {
        let request = Request::builder()
            .method("POST")
            .uri(format!("/v1/memory/notes/{route}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(body).unwrap()))
            .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    /// Remembers `items` in scope `a` through the real route.
    async fn remember(&self, key: &str, items: Vec<Value>) {
        let body = json!({"scope": {"agent": "a"}, "idempotency_key": key, "items": items});
        let (status, response) = self.post("remember", &body).await;
        assert_eq!(status, 200, "{response}");
    }

    /// A recall that asks for the deadline ceiling unless the body names one.
    async fn recalled(&self, mut body: Value) -> (u16, Value) {
        let limits = body
            .as_object_mut()
            .unwrap()
            .entry("limits")
            .or_insert_with(|| json!({}));
        limits
            .as_object_mut()
            .unwrap()
            .entry("deadline_ms")
            .or_insert(json!(5_000));
        self.post("recall", &body).await
    }

    fn database<R>(&self, work: impl FnOnce(&mut Database) -> R) -> R {
        let mut owner = self.service.database.write().unwrap();
        work(owner.as_mut().unwrap())
    }

    /// An entity in scope `a` whose single evidence record carries `vector`
    /// and `payload`, written through the core (the first REST slice cannot
    /// attach evidence).
    fn seed_evidence(&self, id: &str, vector: [f32; 2], payload: &[u8]) {
        let owner = scope("a");
        self.database(|db| {
            db.graph_mut("notes_graph")
                .unwrap()
                .write_batch(&[GraphOp::Vector {
                    collection: "notes_ev".into(),
                    mutation: Mutation::PutWithMetadata {
                        id: record_id(id),
                        vector: vector.to_vec(),
                        payload: payload.to_vec(),
                        metadata: Metadata::new(),
                    },
                }])
                .unwrap();
            let request = RememberRequest {
                scope: owner.clone(),
                idempotency_key: format!("seed-{id}"),
                receipt_window_ms: 1_000,
                now_ms: 100,
                deadline_ms: None,
                allow_future_observations: false,
                items: vec![MemoryItem {
                    id: id.into(),
                    kind: MemoryKind::Entity,
                    content: id.into(),
                    source: "s1".into(),
                    importance: None,
                    observed_at_ms: Some(90),
                    provenance: vec![Provenance {
                        source: "s1".into(),
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
                        id: id.into(),
                        scope: owner.clone(),
                    }],
                }],
            };
            MemoryMut::bind(db, "notes_graph", Some("notes_ev"))
                .unwrap()
                .remember(&request)
                .unwrap();
        });
    }

    /// Commits the exact-scope erasure barrier of the E1 contract: the
    /// singleton job node. Recall only probes the job key for presence.
    fn bar(&self, barred: &MemoryScope) {
        let scope_key = scope_key(barred);
        let string = |value: &str| MetadataValue::String(value.to_owned());
        let mut properties = Metadata::new();
        for (name, value) in [
            ("e1_schema", "erase-scope:v1"),
            ("scope", scope_key.as_str()),
            ("e1_collection", "notes_ev"),
            ("generation", "1"),
            ("state", "pending"),
            ("logical_visibility", "hidden"),
            ("deleted_count", "0"),
            ("remaining", "unknown"),
        ] {
            properties.insert(name.into(), string(value));
        }
        properties.insert("created_at_ms".into(), MetadataValue::Integer(100));
        self.database(|db| {
            let provisioned = db
                .graph("notes_graph")
                .unwrap()
                .get_node(&scope_key)
                .unwrap()
                .output;
            assert!(provisioned.is_some(), "ScopeKey names the scope node");
            db.graph_mut("notes_graph")
                .unwrap()
                .write_batch(&[GraphOp::CreateNode {
                    key: job_key(&scope_key),
                    labels: vec!["VliteMemoryErasureJob".into()],
                    properties,
                }])
                .unwrap();
        });
    }

    /// Overwrites stored properties of the memory node `id` (an integrity
    /// fault the REST routes can never produce).
    fn corrupt(&self, id: &str, set: Metadata) {
        self.database(|db| {
            let found = db
                .graph("notes_graph")
                .unwrap()
                .find_nodes(
                    &FindNodes {
                        label: Some("VliteMemory".into()),
                        conditions: vec![Condition::Eq {
                            field: "id".into(),
                            value: MetadataValue::String(id.into()),
                        }],
                        limit: Some(2),
                        ..FindNodes::default()
                    },
                    &ReadOptions::default(),
                )
                .unwrap()
                .output;
            assert_eq!(found.len(), 1);
            db.graph_mut("notes_graph")
                .unwrap()
                .write_batch(&[GraphOp::PatchNode {
                    key: found[0].key.clone(),
                    patch: NodePatch {
                        set,
                        ..Default::default()
                    },
                    expected: Expected::Any,
                }])
                .unwrap();
        });
    }
}

fn item(id: &str, content: &str) -> Value {
    json!({
        "id": id, "kind": "entity", "content": content, "observed_at_ms": "90",
        "source": {"source_id": "s1", "source_kind": "caller"},
    })
}

fn seeds(ids: &[&str]) -> Value {
    json!({"scope": {"agent": "a"}, "entity_seeds": ids})
}

fn ids(body: &Value) -> Vec<&str> {
    body["memories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|memory| memory["id"].as_str().unwrap())
        .collect()
}

/// `(status, code, details.field)` of an error body.
fn failure(status: u16, body: &Value) -> (u16, &str, Option<&str>) {
    (
        status,
        body["error"]["code"].as_str().unwrap_or("none"),
        body["error"]["details"]["field"].as_str(),
    )
}

// ------------------------------------------------------------------ errors

fn mapped(error: RecallError, allow_partial: bool) -> (u16, &'static str, Option<&'static str>) {
    let fail = recall_fail(error, allow_partial);
    (fail.status.as_u16(), fail.code, fail.field)
}

/// Mapping section 2: the defensive core variants keep their contract codes,
/// whatever the client's `allow_partial`.
#[test]
fn defensive_core_errors_keep_the_mapped_codes() {
    use crate::Error;
    for allow_partial in [false, true] {
        for (error, status, code) in [
            (RecallError::InvalidLimits, 422, "invalid_input"),
            (RecallError::InvalidScopes, 422, "invalid_input"),
            (RecallError::InvalidQuery, 422, "invalid_input"),
            (
                RecallError::UnsupportedPlan,
                422,
                "memory_bounded_plan_unsupported",
            ),
            (RecallError::Deadline, 504, "deadline_exceeded"),
            (
                RecallError::Storage(Error::InvalidQuery("stored")),
                500,
                "internal_error",
            ),
            (
                RecallError::Memory(MemoryError::Storage(Error::InvalidQuery("stored"))),
                500,
                "internal_error",
            ),
            (
                RecallError::Memory(MemoryError::MissingMemory),
                404,
                "memory_not_found",
            ),
            (
                RecallError::Memory(MemoryError::DeletionPending),
                409,
                "memory_deletion_pending",
            ),
        ] {
            let label = format!("{error:?} allow_partial={allow_partial}");
            let (got_status, got_code, _) = mapped(error, allow_partial);
            assert_eq!((got_status, got_code), (status, code), "{label}");
        }
    }
}

/// Wire conventions 2.2 and mapping section 2: cancellation has no wire code.
/// It is never a success or a partial, and never the deadline code.
#[test]
fn cancellation_is_an_error_without_the_deadline_code() {
    for allow_partial in [false, true] {
        let fail = recall_fail(RecallError::Cancelled, allow_partial);
        assert!(fail.status.is_server_error(), "{:?}", fail.status);
        assert_ne!(fail.code, "deadline_exceeded");
        assert_ne!(fail.status, StatusCode::GATEWAY_TIMEOUT);
    }
}

/// Mapping section 2: an exhausted budget names its bound in `details.field`;
/// section 6: with `allow_partial: true` the bounded plan is refused.
#[test]
fn an_exhausted_budget_names_its_bound_or_refuses_the_plan() {
    for bound in [
        "candidates",
        "vector_components",
        "evidence_bytes",
        "mmr_components",
        "source_records",
        "source_scan",
        "graph",
    ] {
        assert_eq!(
            mapped(RecallError::Budget(bound), false),
            (422, "invalid_input", Some(bound))
        );
        assert_eq!(
            mapped(RecallError::Budget(bound), true),
            (422, "memory_bounded_plan_unsupported", None),
            "{bound}"
        );
    }
}

/// Mapping section 2: ranking input built from stored state that violates
/// the ranking contract (here an importance outside 0..=1) is an integrity
/// fault, 500, never a client 422.
#[tokio::test]
async fn a_stored_importance_outside_the_range_is_an_internal_error() {
    let env = Env::new();
    env.remember("k1", vec![item("e1", "Ada")]).await;
    let (status, body) = env.recalled(seeds(&["e1"])).await;
    assert_eq!(status, 200, "{body}");
    let mut set = Metadata::new();
    set.insert("importance".into(), MetadataValue::Float(2.0));
    env.corrupt("e1", set);
    let (status, body) = env.recalled(seeds(&["e1"])).await;
    assert_eq!(
        failure(status, &body),
        (500, "internal_error", None),
        "{body}"
    );
}

/// Mapping section 2: a stored property that is missing or mistyped is an
/// integrity fault, 500, not the mapper's client 422.
#[tokio::test]
async fn a_mistyped_stored_property_is_an_internal_error() {
    let env = Env::new();
    env.remember("k1", vec![item("e1", "Ada")]).await;
    let mut set = Metadata::new();
    set.insert("sources".into(), MetadataValue::Integer(7));
    env.corrupt("e1", set);
    let (status, body) = env.recalled(seeds(&["e1"])).await;
    assert_eq!(
        failure(status, &body),
        (500, "internal_error", None),
        "{body}"
    );
}

/// Contract section 4 candidate budget: more seeds than candidates is an
/// exhausted work budget, 422 `invalid_input` naming `candidates`; with
/// `allow_partial: true` the first slice refuses the plan (mapping section 6,
/// `docs/API.md`: partial results are not served).
#[tokio::test]
async fn candidate_exhaustion_names_the_bound_and_never_turns_partial() {
    let env = Env::new();
    env.remember("k1", vec![item("e1", "Ada"), item("e2", "Bob")])
        .await;
    let mut request = seeds(&["e1", "e2"]);
    request["limit"] = json!(1);
    request["limits"] = json!({"candidates": 1});
    let (status, body) = env.recalled(request.clone()).await;
    assert_eq!(
        failure(status, &body),
        (422, "invalid_input", Some("candidates")),
        "{body}"
    );
    request["allow_partial"] = json!(false);
    let (status, body) = env.recalled(request.clone()).await;
    assert_eq!(
        failure(status, &body),
        (422, "invalid_input", Some("candidates")),
        "{body}"
    );
    request["allow_partial"] = json!(true);
    let (status, body) = env.recalled(request).await;
    assert_eq!(
        failure(status, &body),
        (422, "memory_bounded_plan_unsupported", None),
        "{body}"
    );
}

// ----------------------------------------------------------------- partial

/// Contract sections 4 and 9: without `allow_partial` a graph budget that
/// stops the expansion is 422 `invalid_input`, never a partial 200; the
/// first slice also refuses `allow_partial: true` (`docs/API.md`).
#[tokio::test]
async fn an_exhausted_graph_budget_is_refused_not_answered_partially() {
    let env = Env::new();
    let mut source = item("e1", "Ada");
    source["references"] = json!([{"id": "e2", "revision": "0"}]);
    env.remember("k1", vec![source, item("e2", "Bob")]).await;

    // The unrestricted expansion reaches the referenced entity.
    let (status, body) = env.recalled(seeds(&["e1"])).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(ids(&body), ["e1", "e2"]);
    assert_eq!(body["meta"]["complete"], true);
    assert_eq!(body["meta"]["truncation"], json!([]));

    // One visited node leaves no room for the neighbour. The bound is named,
    // whatever its vocabulary.
    let mut request = seeds(&["e1"]);
    request["limits"] = json!({"max_visited_nodes": 1});
    for partial in [None, Some(false)] {
        if let Some(partial) = partial {
            request["allow_partial"] = json!(partial);
        }
        let (status, body) = env.recalled(request.clone()).await;
        let (status, code, field) = failure(status, &body);
        assert_eq!((status, code), (422, "invalid_input"), "{body}");
        assert!(field.is_some_and(|field| !field.is_empty()), "{body}");
    }
    request["allow_partial"] = json!(true);
    let (status, body) = env.recalled(request).await;
    assert_eq!(
        failure(status, &body),
        (422, "memory_bounded_plan_unsupported", None),
        "{body}"
    );
}

// ----------------------------------------------------------------- barrier

/// Mapping sections 1 and 6, E1 section 5: a recall over a barred scope
/// omits it, answers 200 with `complete: false` and `deletion_barrier`, and
/// never looks like a complete empty read; a barred seed is 409.
#[tokio::test]
async fn a_barred_scope_is_omitted_and_reported() {
    let env = Env::new();
    env.remember("k1", vec![item("e1", "Ada")]).await;
    let broad = json!({
        "scope": {"agent": "a"}, "scopes": [{"agent": "a"}, {"agent": "b"}],
        "entity_seeds": ["e1"],
    });
    let (status, body) = env.recalled(broad.clone()).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["meta"]["complete"], true);
    assert_eq!(body["meta"]["truncation"], json!([]));

    env.bar(&scope("b"));
    let (status, body) = env.recalled(broad).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(ids(&body), ["e1"]);
    assert_eq!(body["meta"]["complete"], false);
    assert_eq!(body["meta"]["truncation"], json!(["deletion_barrier"]));
    assert_eq!(body["meta"]["limitations"], json!(["retained_state_only"]));

    // Only the barred scope is requested: empty, yet explicitly incomplete.
    let (status, body) = env
        .recalled(json!({"scope": {"agent": "b"}, "vector": [1, 0]}))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["memories"], json!([]));
    assert_eq!(body["meta"]["complete"], false);
    assert_eq!(body["meta"]["truncation"], json!(["deletion_barrier"]));

    let (status, body) = env
        .recalled(json!({"scope": {"agent": "b"}, "entity_seeds": ["e2"]}))
        .await;
    assert_eq!(
        failure(status, &body),
        (409, "memory_deletion_pending", None),
        "{body}"
    );
}

// ---------------------------------------------------------------- response

/// Contract section 4 and 8: hits keep the weighted final order (descending
/// S, then canonical scope and memory ID) and the requested `limit` bounds the
/// ranked output.
#[tokio::test]
async fn hits_keep_the_final_order_within_the_requested_limit() {
    let env = Env::new();
    let items = [("a1", 0.2), ("b1", 0.9), ("c1", 0.5), ("d1", 0.5)]
        .into_iter()
        .map(|(id, importance)| {
            let mut value = item(id, id);
            value["importance"] = json!(importance);
            value
        })
        .collect();
    env.remember("k1", items).await;
    let all = ["a1", "b1", "c1", "d1"];

    // Seeds are expanded-only (R = 0), so with the default weights every S is
    // zero and the canonical order decides.
    let (status, body) = env.recalled(seeds(&all)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(ids(&body), ["a1", "b1", "c1", "d1"]);

    let mut request = seeds(&all);
    request["ranking"] = json!({"weights": [0, 0, 1]});
    let (status, body) = env.recalled(request.clone()).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(ids(&body), ["b1", "c1", "d1", "a1"]);
    let scores: Vec<f64> = body["memories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|memory| memory["scores"]["S"].as_f64().unwrap())
        .collect();
    assert_eq!(scores, [0.9, 0.5, 0.5, 0.2]);

    for (limit, expected) in [
        (2, vec!["b1", "c1"]),
        (3, vec!["b1", "c1", "d1"]),
        (4, vec!["b1", "c1", "d1", "a1"]),
    ] {
        request["limit"] = json!(limit);
        let (status, body) = env.recalled(request.clone()).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(ids(&body), expected, "limit {limit}");
    }
}

/// Contract sections 4 and 8: provenance is part of the default view and only
/// `projection.provenance: false` omits it. The digest is the section 8
/// example's for content `Ada`.
#[tokio::test]
async fn provenance_follows_its_projection_switch() {
    let env = Env::new();
    env.remember("k1", vec![item("e1", "Ada")]).await;
    let provenance = json!([{
        "source_id": "s1", "source_kind": "caller",
        "content_digest": "v1:a39afeed7d3319213be7a235840b3c6d3f09f2810b9c91779f34572b6b36832a",
    }]);
    for projection in [
        json!(null),
        json!({}),
        json!({"provenance": true}),
        json!({"snippets": false}),
    ] {
        let mut request = seeds(&["e1"]);
        if !projection.is_null() {
            request["projection"] = projection.clone();
        }
        let (status, body) = env.recalled(request).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            body["memories"][0]["provenance"], provenance,
            "{projection}"
        );
    }
    for projection in [
        json!({"provenance": false}),
        json!({"provenance": false, "snippets": true}),
    ] {
        let mut request = seeds(&["e1"]);
        request["projection"] = projection.clone();
        let (status, body) = env.recalled(request).await;
        assert_eq!(status, 200, "{body}");
        let view = body["memories"][0].as_object().unwrap();
        assert!(!view.contains_key("provenance"), "{projection}: {view:?}");
        assert_eq!(view["content"], "Ada");
        assert_eq!(view["id"], "e1");
    }
}

/// Contract section 4 (shared content bound) and section 8 (`MemoryView`):
/// content of exactly 4,096 bytes is valid and comes back whole.
#[tokio::test]
async fn content_of_the_maximum_length_is_returned_intact() {
    let env = Env::new();
    let content = format!("{}z", "y".repeat(4_095));
    assert_eq!(content.len(), 4_096);
    env.remember("k1", vec![item("e1", &content)]).await;
    let (status, body) = env.recalled(seeds(&["e1"])).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["memories"][0]["content"], content.as_str());
}

/// Contract sections 4 and 8: each evidence snippet may reach 4,096 bytes and
/// `projection.snippets` (default true) only controls the snippet text, never
/// the evidence identity. `collection` is an alias of the binding, not a
/// stored name (section 3). OPEN: the contract and OpenAPI only type it as a
/// `MemoryBinding`; the adapter reports the binding name while mapping
/// section 4 calls the configured collection name caller-visible, so either
/// is accepted and an invented name is not.
#[tokio::test]
async fn evidence_views_follow_the_snippet_projection() {
    assert_eq!(record_id("e"), 0x3f79bb7b435b0532);
    let env = Env::new();
    let payload = format!("{}z", "y".repeat(4_095));
    assert_eq!(payload.len(), 4_096);
    env.seed_evidence("m1", [1.0, 0.0], payload.as_bytes());
    let record = record_id("m1").to_string();
    let query = |projection: Value| {
        let mut body = json!({"scope": {"agent": "a"}, "vector": [1, 0]});
        if !projection.is_null() {
            body["projection"] = projection;
        }
        body
    };

    for (projection, snippet) in [
        (json!(null), true),
        (json!({}), true),
        (json!({"snippets": true}), true),
        (json!({"provenance": false}), true),
        (json!({"snippets": false}), false),
        (json!({"snippets": false, "provenance": true}), false),
    ] {
        let (status, body) = env.recalled(query(projection.clone())).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(ids(&body), ["m1"]);
        let evidence = body["memories"][0]["evidence"].as_array().unwrap();
        assert_eq!(evidence.len(), 1, "{projection}");
        let view = evidence[0].as_object().unwrap();
        assert!(["notes", "notes_ev"].contains(&view["collection"].as_str().unwrap()));
        assert_eq!(view["record"], record.as_str(), "{projection}");
        assert_eq!(
            view.get("snippet").map(|snippet| snippet.as_str().unwrap()),
            snippet.then_some(payload.as_str()),
            "{projection}"
        );
        assert_eq!(view.len(), 2 + usize::from(snippet), "{projection}");
    }
}

// ----------------------------------------------------------------- ranking

/// Contract section 4: `exact` and `mmr` are vector-only selections; text,
/// entity seeds, other weights or a half-life are refused as
/// `memory_unsupported_ranking` (`docs/API.md` first-slice limits). A
/// weighted selection keeps accepting a half-life.
#[tokio::test]
async fn vector_selections_refuse_every_blend() {
    let env = Env::new();
    for selection in ["exact", "mmr"] {
        let ranking = |extra: Value| {
            let mut ranking = json!({"selection": selection});
            ranking
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            ranking
        };
        for (blend, mut body) in [
            (
                "a half-life",
                json!({"vector": [1, 0], "ranking": ranking(json!({"half_life_ms": 1000}))}),
            ),
            (
                "other weights",
                json!({"vector": [1, 0], "ranking": ranking(json!({"weights": [0, 0, 1]}))}),
            ),
            (
                "text",
                json!({"vector": [1, 0], "text": "Ada", "ranking": ranking(json!({}))}),
            ),
            (
                "entity seeds",
                json!({"vector": [1, 0], "entity_seeds": ["e1"], "ranking": ranking(json!({}))}),
            ),
            (
                "no vector",
                json!({"entity_seeds": ["e1"], "ranking": ranking(json!({}))}),
            ),
        ] {
            body["scope"] = json!({"agent": "a"});
            let (status, response) = env.recalled(body).await;
            assert_eq!(
                (status, response["error"]["code"].as_str()),
                (422, Some("memory_unsupported_ranking")),
                "{selection} with {blend}: {response}"
            );
        }
        let bare =
            json!({"scope": {"agent": "a"}, "vector": [1, 0], "ranking": ranking(json!({}))});
        let (status, response) = env.recalled(bare).await;
        assert_eq!(status, 200, "{selection}: {response}");
        assert_eq!(response["selection"], selection);
    }
    let weighted = json!({
        "scope": {"agent": "a"}, "vector": [1, 0], "ranking": {"half_life_ms": 1000},
    });
    let (status, response) = env.recalled(weighted).await;
    assert_eq!(status, 200, "{response}");
    assert_eq!(response["selection"], "weighted");
}

/// Contract section 4: MMR lambda defaults to 0.5. Candidates `a`, `b` and `c`
/// lie at 18.4, 26.6 and -45 degrees from the query (cosines .9487, .8944,
/// .7071; `a` and `b` are mutually .990 similar). Lambda 0.5 picks `a`, then
/// `c` (score .130 against -.048 for `b`); relevance alone (lambda 1) keeps
/// the order `a`, `b`, `c`.
#[tokio::test]
async fn mmr_defaults_to_a_balanced_lambda() {
    let env = Env::new();
    env.seed_evidence("a", [3.0, 1.0], b"p");
    env.seed_evidence("b", [2.0, 1.0], b"p");
    env.seed_evidence("c", [1.0, -1.0], b"p");
    let query =
        |ranking: Value| json!({"scope": {"agent": "a"}, "vector": [1, 0], "ranking": ranking});
    for (ranking, expected) in [
        (json!({"selection": "mmr"}), ["a", "c", "b"]),
        (json!({"selection": "mmr", "lambda": 0.5}), ["a", "c", "b"]),
        (json!({"selection": "mmr", "lambda": 1.0}), ["a", "b", "c"]),
    ] {
        let (status, body) = env.recalled(query(ranking.clone())).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["selection"], "mmr");
        assert_eq!(ids(&body), expected, "{ranking}");
    }
}

/// Contract section 4: future observations default to `reject` (422, also
/// when `ranking` is present without the field) and an explicit `clamp`
/// treats the age as zero.
#[tokio::test]
async fn future_observations_reject_by_default_and_clamp_on_request() {
    let env = Env::new();
    env.at(1_000);
    env.remember("k1", vec![item("e1", "Ada")]).await;
    // Observed at 90, read at clock 50: the observation is in the future.
    env.at(50);
    for ranking in [
        json!(null),
        json!({}),
        json!({"selection": "weighted"}),
        json!({"future_observations": "reject"}),
    ] {
        let mut request = seeds(&["e1"]);
        if !ranking.is_null() {
            request["ranking"] = ranking.clone();
        }
        let (status, body) = env.recalled(request).await;
        assert_eq!(
            (status, body["error"]["code"].as_str()),
            (422, Some("invalid_input")),
            "{ranking}: {body}"
        );
    }
    let mut request = seeds(&["e1"]);
    request["ranking"] = json!({"future_observations": "clamp"});
    let (status, body) = env.recalled(request.clone()).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(ids(&body), ["e1"]);
    // Clamped age 0 gives D = 1 whatever the half-life.
    request["ranking"] = json!({
        "future_observations": "clamp", "weights": [0, 1, 0], "half_life_ms": 1000,
    });
    let (status, body) = env.recalled(request).await;
    assert_eq!(status, 200, "{body}");
    let scores = &body["memories"][0]["scores"];
    assert_eq!(
        (scores["D"].as_f64(), scores["S"].as_f64()),
        (Some(1.0), Some(1.0))
    );
}

/// Contract section 4: `D = 1 / (1 + age / half_life_ms)` with the half-life
/// in milliseconds, so age 1,000 ms against half-lives of 1,000 and 250 ms
/// gives D of 0.5 and 0.2.
#[tokio::test]
async fn half_life_is_in_milliseconds() {
    let env = Env::new();
    env.at(1_000);
    let mut remembered = item("e1", "Ada");
    remembered["observed_at_ms"] = json!("0");
    env.remember("k1", vec![remembered]).await;
    for (half_life, expected) in [(1_000, 0.5), (250, 0.2)] {
        let mut request = seeds(&["e1"]);
        request["ranking"] = json!({"weights": [0, 1, 0], "half_life_ms": half_life});
        let (status, body) = env.recalled(request).await;
        assert_eq!(status, 200, "{body}");
        let scores = &body["memories"][0]["scores"];
        assert_eq!(
            (scores["D"].as_f64(), scores["S"].as_f64()),
            (Some(expected), Some(expected)),
            "half-life {half_life}"
        );
    }
}
