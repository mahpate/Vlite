//! Wire-fixture replay (contract section 13): every accepted
//! `tests/fixtures/agent_memory/wire` case is sent over HTTP to the real
//! memory routes on a fresh in-memory database built from the case view.
//! Receipt IDs and sequences are server-derived, so they are checked for
//! shape (and for identity on replay) and then replaced with the oracle's
//! placeholders; error messages are sanitized as in the oracle. Cases owned
//! by middleware are listed with a reason and covered by server tests;
//! recorded first-slice deviations are listed with the served answer.

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
use crate::memory::{MemoryItem, MemoryKind, MemoryMut, MemoryScope, Provenance, RememberRequest};
use crate::{CollectionSpec, CreateOptions, Database, FormatVersion};

pub(super) const REMEMBER: &str =
    include_str!("../../../tests/fixtures/agent_memory/wire/remember.json");
pub(super) const RECALL: &str =
    include_str!("../../../tests/fixtures/agent_memory/wire/recall.json");

const PRINCIPAL: &str = "abcdefghij27";
const OTHER: &str = "bcdefghij234";

/// `(case, status, code, feature)` as served instead of the oracle body;
/// for a served 200 `code` is the first outcome's action.
pub(super) type Deviation = (&'static str, u16, &'static str, Option<&'static str>);

pub(super) struct Harness {
    router: Router,
    clock: Clock,
    service: Service,
    graph: String,
    tenant: String,
}

fn stored(tenant: &str, name: &str) -> String {
    if tenant == "@root" {
        name.to_owned()
    } else {
        format!("t{tenant}__{name}")
    }
}

/// The database, binding set, credential and router a view describes, or
/// `None` when the binding set is refused at startup.
pub(super) fn harness(view: &Value) -> Option<Harness> {
    let tenant = view["tenant"].as_str().unwrap_or("@root").to_owned();
    let graph = stored(&tenant, "notes_graph");
    let evidence = stored(&tenant, "notes_ev");
    let mut db = Database::create_in_memory(
        2,
        CreateOptions {
            format: Some(FormatVersion::V5),
            ..Default::default()
        },
    )
    .unwrap();
    db.create_graph(&graph).unwrap();
    db.create_collection(
        &evidence,
        CollectionSpec {
            dimensions: 2,
            settings: Default::default(),
        },
    )
    .unwrap();
    let lineage = db.lineage().unwrap();
    let scopes: Vec<Value> = view["scope_grants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|grant| {
            let mut grant = grant.clone();
            let map = grant.as_object_mut().unwrap();
            assert_eq!(map.remove("tenant"), Some(json!(tenant)));
            grant
        })
        .collect();
    let mut grants = vec![json!({"principal": OTHER, "scopes": [{"agent": "other"}]})];
    if !scopes.is_empty() {
        grants.push(json!({"principal": PRINCIPAL, "scopes": scopes}));
    }
    let binding = if view["binding_exists"] == json!(false) {
        "elsewhere"
    } else {
        "notes"
    };
    let mut entry = json!({
        "binding": binding,
        "tenant": tenant,
        "generation": 1,
        "state": "active",
        "graph": "notes_graph",
        "evidence_collection": "notes_ev",
        "schema_version": "memory-schema-v1",
        "ranking_version": "memory-ranking-v1",
        "digest_version": "memory-request-v1",
        "receipt_window_ms": view["receipt_window_ms"],
        "grants": grants,
    });
    // Not a fixture field: server tests retire the binding.
    if let Some(at) = view.get("retired_at_ms") {
        entry["state"] = json!("retired");
        entry["retired_at_ms"] = at.clone();
    }
    let text = json!({"version": "memory-bindings-v1", "bindings": [entry]}).to_string();
    let bindings = crate::memory::binding::parse(text.as_bytes()).ok()?;
    let service = Service::new(db).with_bindings(super::reserved::Bindings::new(bindings));
    super::startup(&service).ok()?;
    let mut grant = credentials::Grant::for_user(
        PRINCIPAL.into(),
        "replay".into(),
        (tenant != "@root").then(|| tenant.clone()),
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
    if view["graph_grant"] == json!(false) {
        grant.graphs = vec!["other_graph".into()];
    }
    if view["collection_grants"] == json!(false) {
        grant.all = false;
        grant.collections.clear();
    }
    let clock = Clock::default();
    let router = super::routes::routes()
        .with_state(service.clone())
        .layer(Extension(admin::Role::Scoped(Arc::new(grant))))
        .layer(Extension(RequestId("replay".into())))
        .layer(Extension(clock.clone()))
        .layer(DefaultBodyLimit::max(MAX_JSON_BYTES));
    Some(Harness {
        router,
        clock,
        service,
        graph,
        tenant,
    })
}

/// `{"$text": s, "count": n}` is `s` repeated `n` times.
fn expand(value: &Value) -> Value {
    match value {
        Value::Object(map) if map.contains_key("$text") => {
            let count = map["count"].as_u64().unwrap() as usize;
            Value::String(map["$text"].as_str().unwrap().repeat(count))
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), expand(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(expand).collect()),
        other => other.clone(),
    }
}

/// The raw request bytes: a string body is sent verbatim and
/// `{"$json_padding": v, "size": n}` is `v` padded with trailing
/// whitespace to exactly `n` bytes.
fn bytes(body: &Value) -> Vec<u8> {
    match body {
        Value::String(raw) => raw.clone().into_bytes(),
        Value::Object(map) if map.contains_key("$json_padding") => {
            let mut bytes = serde_json::to_vec(&expand(&map["$json_padding"])).unwrap();
            let size = map["size"].as_u64().unwrap() as usize;
            assert!(bytes.len() <= size);
            bytes.resize(size, b' ');
            bytes
        }
        other => serde_json::to_vec(&expand(other)).unwrap(),
    }
}

impl Harness {
    fn at(&self, clock_ms: u64) {
        self.clock.now_ms.store(clock_ms, Ordering::SeqCst);
    }

    async fn send(&self, request: &Value) -> (u16, Value) {
        let mut builder = Request::builder()
            .method(request["method"].as_str().unwrap())
            .uri(request["path"].as_str().unwrap())
            .header(header::CONTENT_TYPE, "application/json");
        for (name, value) in request["headers"].as_object().into_iter().flatten() {
            if name != "Authorization" {
                builder = builder.header(name.as_str(), value.as_str().unwrap());
            }
        }
        let request = builder.body(Body::from(bytes(&request["body"]))).unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status().as_u16();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&body).unwrap())
    }

    /// One `pre_state` step through the core, at its own clock. Sources
    /// carry the wire default kind `caller`.
    fn seed(&self, step: &Value) {
        assert_eq!(step["op"], "remember", "unsupported pre_state op");
        let request = &step["request"];
        let scope = &request["scope"];
        assert_eq!(scope["tenant"], json!(self.tenant));
        let items = request["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                let source = item["source"].as_str().unwrap().to_owned();
                MemoryItem {
                    id: item["id"].as_str().unwrap().into(),
                    kind: match item["kind"].as_str().unwrap() {
                        "entity" => MemoryKind::Entity,
                        "episodic" => MemoryKind::Episodic,
                        other => panic!("unsupported seed kind {other}"),
                    },
                    content: item["content"].as_str().unwrap().into(),
                    source: source.clone(),
                    importance: None,
                    observed_at_ms: item["observed_at"].as_i64(),
                    provenance: vec![Provenance {
                        source,
                        kind: Some("caller".into()),
                        uri: None,
                        transformation: None,
                    }],
                    references: Vec::new(),
                    merge_into: None,
                    expected_revision: None,
                    expiry_ms: None,
                    expiry_valid: true,
                    evidence: Vec::new(),
                }
            })
            .collect();
        let request = RememberRequest {
            scope: MemoryScope {
                tenant: self.tenant.clone(),
                agent: scope["agent"].as_str().unwrap().into(),
                user: scope["user"].as_str().map(str::to_owned),
                session: scope["session"].as_str().map(str::to_owned),
            },
            idempotency_key: request["key"].as_str().unwrap().into(),
            receipt_window_ms: request["receipt_window"].as_u64().unwrap(),
            now_ms: step["clock"].as_u64().unwrap(),
            deadline_ms: None,
            allow_future_observations: false,
            items,
        };
        let mut owner = self.service.database.write().unwrap();
        let db = owner.as_mut().unwrap();
        MemoryMut::bind(db, &self.graph, None)
            .and_then(|mut memory| memory.remember(&request))
            .unwrap();
    }

    /// A stored receipt: its original request is executed over HTTP at the
    /// clock that makes its window end at `retry_until_ms`. A merge item's
    /// target is first created at revision 1 from an unrelated source.
    async fn receipt(&self, view: &Value, receipt: &Value, path: &str) -> Value {
        let window = view["receipt_window_ms"].as_u64().unwrap();
        let clock = receipt["retry_until_ms"].as_u64().unwrap() - window;
        let request = &receipt["request"];
        let operation = receipt["operation"].as_str().unwrap();
        let path = format!("{}/{operation}", path.rsplit_once('/').unwrap().0);
        for item in request["items"].as_array().into_iter().flatten() {
            if item["merge"] == "merge_provenance" {
                let mut base = item.clone();
                let map = base.as_object_mut().unwrap();
                map.remove("merge");
                map.remove("expected");
                map.insert(
                    "source".into(),
                    json!({"source_id": "base", "source_kind": "caller"}),
                );
                self.at(clock);
                let (status, _) = self
                    .send(&json!({"method": "POST", "path": path, "body": {
                        "scope": request["scope"], "idempotency_key": "replay-base", "items": [base],
                    }}))
                    .await;
                assert_eq!(status, 200);
            }
        }
        self.at(clock);
        let (status, body) = self
            .send(&json!({"method": "POST", "path": path, "body": request}))
            .await;
        assert_eq!(status, 200, "receipt seed: {body}");
        body
    }
}

fn is_receipt_id(value: &Value) -> bool {
    value.as_str().is_some_and(|id| {
        id.strip_prefix("rcpt:v1:")
            .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
    })
}

fn is_sequence(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|text| text.parse::<u64>().is_ok_and(|n| n.to_string() == text))
}

/// Brings a served body into the oracle's normal form (module docs).
fn normalize(name: &str, actual: &mut Value, expected: &Value, seeded: Option<&Value>) {
    if let Some(receipt) = actual.get_mut("receipt") {
        assert!(is_receipt_id(&receipt["id"]), "{name}: receipt id");
        assert!(is_sequence(&receipt["sequence"]), "{name}: sequence");
        if let Some(seeded) = seeded {
            let same = receipt["id"] == seeded["receipt"]["id"]
                && receipt["sequence"] == seeded["receipt"]["sequence"];
            assert_eq!(
                same,
                expected["replayed"] == json!(true),
                "{name}: receipt identity"
            );
        }
        receipt["id"] = expected["receipt"]["id"].clone();
        receipt["sequence"] = expected["receipt"]["sequence"].clone();
    }
    if let Some(meta) = actual.get_mut("meta") {
        assert!(is_sequence(&meta["sequence"]), "{name}: sequence");
        meta["sequence"] = expected["meta"]["sequence"].clone();
    }
    if let Some(error) = actual.get_mut("error") {
        error["message"] = json!("sanitized");
        if expected["error"].get("details").is_none() {
            error.as_object_mut().unwrap().remove("details");
        }
    }
}

/// Replays every case of one fixture; returns the number replayed.
pub(super) async fn replay(text: &str, skip: &[(&str, &str)], deviations: &[Deviation]) -> usize {
    let fixture: Value = serde_json::from_str(text).unwrap();
    assert_eq!(fixture["schema"], "memory-rest-wire-v1");
    let mut replayed = 0;
    let mut seen = Vec::new();
    for case in fixture["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let view = &case["view"];
        if let Some((_, reason)) = skip.iter().find(|(skipped, _)| *skipped == name) {
            seen.push(name);
            if *reason == "startup refusal" {
                assert!(harness(view).is_none(), "{name}: startup must refuse");
            }
            continue;
        }
        let harness = harness(view).unwrap_or_else(|| panic!("{name}: startup refusal"));
        for step in view["pre_state"].as_array().into_iter().flatten() {
            harness.seed(step);
        }
        let path = case["request"]["path"].as_str().unwrap();
        let mut seeded = None;
        for receipt in view["receipts"].as_array().into_iter().flatten() {
            seeded = Some(harness.receipt(view, receipt, path).await);
        }
        if let Some(elapsed) = view["elapsed_ms"].as_u64() {
            harness.clock.elapsed_ms.store(elapsed, Ordering::SeqCst);
        }
        harness.at(view["clock_ms"].as_u64().unwrap());
        let (status, mut body) = harness.send(&case["request"]).await;
        let expected = &case["expected"];
        if let Some((_, served, code, feature)) = deviations.iter().find(|(d, ..)| *d == name) {
            seen.push(name);
            assert_eq!(status, *served, "{name}: {body}");
            if status == 200 {
                assert_eq!(body["outcomes"][0]["action"], json!(code), "{name}: {body}");
                assert_ne!(
                    body["outcomes"], expected["body"]["outcomes"],
                    "{name}: now matches"
                );
            } else {
                assert_eq!(body["error"]["code"], json!(code), "{name}: {body}");
                assert_eq!(
                    body["error"]["details"]["feature"].as_str(),
                    *feature,
                    "{name}"
                );
                assert_ne!(
                    (status, &body["error"]["code"]),
                    (
                        expected["status"].as_u64().unwrap() as u16,
                        &expected["body"]["error"]["code"]
                    ),
                    "{name}: listed deviation now matches the oracle"
                );
            }
            replayed += 1;
            continue;
        }
        normalize(name, &mut body, &expected["body"], seeded.as_ref());
        assert_eq!(
            status as u64,
            expected["status"].as_u64().unwrap(),
            "{name}: {body}"
        );
        assert_eq!(body, expected["body"], "{name}");
        replayed += 1;
    }
    for (name, _) in skip {
        assert!(seen.contains(name), "unknown skip {name}");
    }
    for (name, ..) in deviations {
        assert!(seen.contains(name), "unknown deviation {name}");
    }
    replayed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Middleware-owned (covered in `tests/server_memory.rs`) or refused at
    /// startup (the window is a binding field, not a request field).
    const REMEMBER_SKIP: [(&str, &str); 8] = [
        ("remember-no-auth", "authentication middleware"),
        ("remember-read-only", "read-only role middleware"),
        ("remember-lineage", "credential lineage middleware"),
        ("remember-tenant-suspended", "tenant state middleware"),
        ("remember-tenant-unavailable", "tenant state middleware"),
        ("remember-missing-retention-policy", "startup refusal"),
        ("remember-window-zero", "startup refusal"),
        ("remember-window-max-plus-one", "startup refusal"),
    ];

    /// Recorded first-slice deviations (mapping sections 1-3).
    const REMEMBER_DEVIATIONS: [Deviation; 5] = [
        // PROVISIONAL: core exact dedup is (scope, kind, digest, source_id);
        // `source_kind` is provenance only, so a second kind deduplicates.
        ("remember-independent-source-kind", 200, "noop", None),
        // A refused field is refused before its value is validated.
        (
            "remember-confidence-over-one",
            409,
            "memory_capability_unavailable",
            Some("confidence"),
        ),
        // Revision pins on retained references are not kept.
        (
            "remember-retained-revision-reference",
            409,
            "memory_capability_unavailable",
            Some("reference_revision_pin"),
        ),
        (
            "remember-retained-reference-revision-guard",
            409,
            "memory_capability_unavailable",
            Some("reference_revision_pin"),
        ),
        // Binding existence is never disclosed.
        ("remember-missing-binding", 403, "forbidden_scope", None),
    ];

    fn view() -> Value {
        json!({
            "tenant": "@root",
            "scope_grants": [{"tenant": "@root", "agent": "a"}],
            "receipt_window_ms": 60_000,
        })
    }

    fn remember_request(item: Value) -> Value {
        let mut base = json!({
            "id": "m1", "kind": "entity", "content": "x", "observed_at_ms": 1,
            "source": {"source_id": "s", "source_kind": "caller"},
        });
        base.as_object_mut()
            .unwrap()
            .extend(item.as_object().unwrap().clone());
        json!({"method": "POST", "path": "/v1/memory/notes/remember", "body": {
            "scope": {"agent": "a"}, "idempotency_key": "k", "items": [base],
        }})
    }

    async fn served(view: &Value, request: &Value) -> (u16, Value) {
        let harness = harness(view).expect("startup");
        harness.at(1_000);
        harness.send(request).await
    }

    /// Relation items are refused with the fact kinds, before validation.
    #[tokio::test]
    async fn remember_relation_kind_is_refused_as_fact_kinds() {
        let (status, body) = served(&view(), &remember_request(json!({"kind": "relation"}))).await;
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["error"]["details"]["feature"], "fact_kinds");
    }

    /// A retired binding refuses Remember before the body is validated.
    #[tokio::test]
    async fn retired_binding_refuses_remember() {
        let mut view = view();
        view["retired_at_ms"] = json!(500);
        let (status, body) = served(&view, &remember_request(json!({}))).await;
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["error"]["code"], "memory_capability_unavailable");
        assert_eq!(body["error"]["details"]["feature"], "binding_retired");
    }

    /// An ungranted evidence collection is the binding 403 before the body
    /// is read, even when the body is malformed.
    #[tokio::test]
    async fn ungranted_evidence_is_refused_before_the_body() {
        let mut view = view();
        view["collection_grants"] = json!(false);
        let mut request = remember_request(json!({}));
        request["body"] = json!("{");
        let (status, body) = served(&view, &request).await;
        assert_eq!(status, 403, "{body}");
        assert_eq!(body["error"]["code"], "forbidden_scope");
    }

    #[tokio::test]
    async fn remember_fixture_cases_replay_over_http() {
        let replayed = replay(REMEMBER, &REMEMBER_SKIP, &REMEMBER_DEVIATIONS).await;
        assert_eq!(replayed, 78 - REMEMBER_SKIP.len());
    }

    /// Recall views that inject planner state or candidate ranks the server
    /// cannot be seeded with (the fixture README: not discovery parity).
    /// Ranking arithmetic is covered by the core ranking tests.
    const RECALL_SKIP: [(&str, &str); 22] = [
        ("recall-default-candidates-200-is-charged", "injected candidate_units"),
        ("recall-default-candidates-inclusive-200", "injected candidate_units"),
        ("recall-source-scan-100001", "injected source_count"),
        ("recall-unbounded-plan-refused", "injected bounded_plan"),
        ("recall-no-lazy-text-index", "injected text_index_ready"),
        ("recall-ranking-golden-default", "injected candidate ranks"),
        ("recall-ranking-golden-id-tie-expanded-R-zero", "injected candidate ranks"),
        ("recall-ranking-golden-weighted-tie", "injected candidate ranks"),
        ("recall-owner-ttl-equality-and-foreign-scope-hidden", "injected candidate ranks"),
        ("recall-vector-exact", "injected candidate ranks"),
        ("recall-vector-mmr", "injected candidate ranks"),
        ("recall-bm25-rrf-preeligible-leg-diagnostics", "injected candidate ranks"),
        ("recall-caller-OR-cannot-replace-mandatory-scope", "injected candidate ranks"),
        ("recall-future-observation-default-refused", "injected candidate ranks"),
        ("recall-future-clamp-explicit", "injected candidate ranks"),
        ("recall-exact-distance-tie-record-before-memory-id", "injected candidate ranks"),
        ("recall-mmr-diversity-not-weighted-score-order", "injected candidate ranks"),
        ("recall-vector-component-budget-before-fetch", "injected candidate ranks"),
        ("recall-mmr-component-budget-before-selection", "injected candidate ranks"),
        ("recall-ranking-golden-scope-before-memory-id", "injected candidate ranks"),
        ("recall-temporal-expansion-gated", "injected temporal_enabled"),
        ("recall-provider-off-even-with-ambient-credential", "injected ambient_credentials"),
    ];

    /// Recorded first-slice deviations (mapping sections 1 and 6).
    const RECALL_DEVIATIONS: [Deviation; 2] = [
        // Historical reads are refused.
        (
            "recall-as-of-keeps-independent-ttl-clock",
            409,
            "memory_capability_unavailable",
            Some("temporal_read"),
        ),
        // An exhausted budget is never a partial answer.
        ("recall-marked-deadline-partial", 504, "deadline_exceeded", None),
    ];

    #[tokio::test]
    async fn recall_fixture_cases_replay_over_http() {
        let replayed = replay(RECALL, &RECALL_SKIP, &RECALL_DEVIATIONS).await;
        assert_eq!(replayed, 44 - RECALL_SKIP.len());
    }

    fn recall_request(body: Value) -> Value {
        json!({"method": "POST", "path": "/v1/memory/notes/recall", "body": body})
    }

    /// Contract section 9 example: remember then seeded recall, exactly.
    #[tokio::test]
    async fn contract_example_seeded_recall() {
        let harness = harness(&view()).unwrap();
        harness.at(100);
        let (status, body) = harness
            .send(&json!({"method": "POST", "path": "/v1/memory/notes/remember", "body": {
                "scope": {"agent": "a"}, "idempotency_key": "k1", "items": [{
                    "id": "e1", "kind": "entity", "content": "Ada", "observed_at_ms": "90",
                    "source": {"source_id": "s1", "source_kind": "caller"},
                }],
            }}))
            .await;
        assert_eq!(status, 200, "{body}");
        let (status, mut body) = harness
            .send(&recall_request(
                json!({"scope": {"agent": "a"}, "entity_seeds": ["e1"], "limit": 1}),
            ))
            .await;
        assert_eq!(status, 200, "{body}");
        assert!(is_sequence(&body["meta"]["sequence"]));
        body["meta"]["sequence"] = json!("7");
        let expected = json!({"memories": [{
            "id": "e1", "revision": "1", "scope": {"tenant": "@root", "agent": "a"},
            "kind": "entity", "content": "Ada", "observed_at_ms": "90",
            "created_at_ms": "100", "updated_at_ms": "100", "importance": 0.5,
            "status": "accepted", "stale": false, "segment": null, "evidence": [],
            "scores": {"R": 0.0, "D": null, "I": 0.5, "S": 0.0},
            "provenance": [{"source_id": "s1", "source_kind": "caller",
                "content_digest": "v1:a39afeed7d3319213be7a235840b3c6d3f09f2810b9c91779f34572b6b36832a"}],
            "references": [], "metadata": {},
        }], "meta": {
            "scopes": [{"tenant": "@root", "agent": "a"}], "as_of_ms": "100",
            "ttl_clock_ms": "100", "sequence": "7", "complete": true, "truncation": [],
            "limitations": ["retained_state_only"], "mode": "bounded_expansion",
            "candidates_examined": "1", "edges_charged": "0", "vector_components": "0",
            "mmr_components": "0", "evidence_bytes": "0",
        }, "ranking_version": "memory-ranking-v1", "selection": "weighted"});
        assert_eq!(body, expected);
    }

    async fn recall_served(view: &Value, body: Value) -> (u16, Value) {
        served(view, &recall_request(body)).await
    }

    /// Every requested scope is authorized before any refusal or lookup.
    #[tokio::test]
    async fn recall_scopes_are_each_authorized_first() {
        let (status, body) = recall_served(
            &view(),
            json!({"scope": {"agent": "a"}, "scopes": [{"agent": "a"}, {"agent": "b"}],
                "filter": {}, "entity_seeds": ["e1"]}),
        )
        .await;
        assert_eq!(status, 403, "{body}");
        assert_eq!(body["error"]["code"], "forbidden_scope");
    }

    /// Field refusals in schema order, before request validation.
    #[tokio::test]
    async fn recall_refusals_precede_validation() {
        for (body, feature) in [
            (json!({"embedding_provider": "x", "kinds": ["fact"]}), "embedding_provider"),
            (json!({"kinds": ["relation"], "filter": {}}), "fact_kinds"),
            (json!({"filter": {}, "as_of_ms": "1"}), "filter"),
            (json!({"as_of_ms": "1", "projection": {"paths": true}}), "temporal_read"),
            (json!({"projection": {"paths": true, "metadata": ["k"]}}), "paths"),
            (json!({"projection": {"metadata": ["k"]}, "limit": 0}), "metadata"),
            (json!({"text": "x".repeat(1025), "limit": 0}), "query_text_length"),
        ] {
            let mut request = body.clone();
            request["scope"] = json!({"agent": "a"});
            let (status, served) = recall_served(&view(), request).await;
            assert_eq!(status, 409, "{body}: {served}");
            assert_eq!(served["error"]["details"]["feature"], feature, "{body}");
        }
        let (status, served) =
            recall_served(&view(), json!({"scope": {"agent": "a"}, "text": "x".repeat(1024)}))
                .await;
        assert_eq!(status, 200, "{served}");
        let (status, served) =
            recall_served(&view(), json!({"scope": {"agent": "a"}, "text": "x".repeat(16_385)}))
                .await;
        assert_eq!(status, 422, "{served}");
        let mut retired = view();
        retired["retired_at_ms"] = json!(500);
        let (status, served) =
            recall_served(&retired, json!({"scope": {"agent": "a"}, "filter": {}})).await;
        assert_eq!(status, 409, "{served}");
        assert_eq!(served["error"]["details"]["feature"], "binding_retired");
    }

    /// Without seeds, text or vector there is nothing to recall.
    #[tokio::test]
    async fn recall_requires_a_query() {
        let (status, body) = recall_served(&view(), json!({"scope": {"agent": "a"}})).await;
        assert_eq!(status, 422, "{body}");
        assert_eq!(body["error"]["details"]["field"], "body");
    }

    /// A missing seed is the core's 404; a seeded recall over a read-only
    /// database is served.
    #[tokio::test]
    async fn recall_missing_seed_is_not_found() {
        let (status, body) =
            recall_served(&view(), json!({"scope": {"agent": "a"}, "entity_seeds": ["zz"]})).await;
        assert_eq!(status, 404, "{body}");
        assert_eq!(body["error"]["code"], "memory_not_found");
    }
}
