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
use crate::memory::{
    ForgetRequest, ForgetTarget, MemoryItem, MemoryKind, MemoryMut, MemoryScope, Provenance,
    RememberRequest,
};
use crate::{CollectionSettings, CollectionSpec, CreateOptions, Database, FormatVersion};

pub(super) const REMEMBER: &str =
    include_str!("../../../tests/fixtures/agent_memory/wire/remember.json");
pub(super) const RECALL: &str =
    include_str!("../../../tests/fixtures/agent_memory/wire/recall.json");
pub(super) const CONSOLIDATE: &str =
    include_str!("../../../tests/fixtures/agent_memory/wire/consolidate.json");
pub(super) const FORGET: &str =
    include_str!("../../../tests/fixtures/agent_memory/wire/forget.json");
pub(super) const RECEIPT: &str =
    include_str!("../../../tests/fixtures/agent_memory/wire/receipt.json");
pub(super) const FACTS: &str = include_str!("../../../tests/fixtures/agent_memory/wire/facts.json");
pub(super) const CORRECT_FACTS: &str =
    include_str!("../../../tests/fixtures/agent_memory/wire/correct_facts.json");

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
            // The fixture views assume a ready lexical index (`text_index_ready`
            // defaults true); an operator declares it with the collection.
            settings: CollectionSettings {
                text_fields: vec!["text".into()],
                ..Default::default()
            },
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

/// `{"$text": s, "count": n}` is `s` repeated `n` times;
/// `{"$repeat": v, "count": n, "enumerate": [k]}` is `n` copies of `v`
/// with the index appended to each string field `k`.
fn expand(value: &Value) -> Value {
    match value {
        Value::Object(map) if map.contains_key("$text") => {
            let count = map["count"].as_u64().unwrap() as usize;
            Value::String(map["$text"].as_str().unwrap().repeat(count))
        }
        Value::Object(map) if map.contains_key("$repeat") => {
            let count = map["count"].as_u64().unwrap();
            let fields = map.get("enumerate").cloned().unwrap_or(json!([]));
            Value::Array(
                (0..count)
                    .map(|index| {
                        let mut item = expand(&map["$repeat"]);
                        for field in fields.as_array().unwrap() {
                            let field = field.as_str().unwrap();
                            let text = format!("{}{index}", item[field].as_str().unwrap());
                            item[field] = json!(text);
                        }
                        item
                    })
                    .collect(),
            )
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

    /// The C1 consolidation setup through the core at clock 1: the view's
    /// `consolidation.sources` (default one entity `m`, as the C1
    /// reference) plus every input a seeded receipt names. Each source gets
    /// distinct content so exact dedup cannot fold them. Returns the
    /// receipt-only IDs, forgotten once the receipts are seeded.
    fn consolidation(&self, view: &Value) -> Vec<String> {
        let sources = view
            .get("consolidation")
            .map(|setup| {
                assert!(setup.as_object().unwrap().keys().all(|k| k == "sources"));
                expand(&setup["sources"])
            })
            .unwrap_or(json!([{"id": "m"}]));
        let mut seeds = Vec::new();
        for source in sources.as_array().unwrap() {
            let map = source.as_object().unwrap();
            assert!(map.keys().all(|k| k == "id" || k == "expiry_ms"));
            seeds.push((
                source["id"].as_str().unwrap().to_owned(),
                source["expiry_ms"].as_u64(),
            ));
        }
        let mut lost = Vec::new();
        for receipt in view["receipts"].as_array().into_iter().flatten() {
            for input in expand(&receipt["request"]["inputs"]).as_array().unwrap() {
                let id = input["id"].as_str().unwrap().to_owned();
                if !seeds.iter().any(|(seed, _)| *seed == id) {
                    seeds.push((id.clone(), None));
                    lost.push(id);
                }
            }
        }
        if seeds.is_empty() {
            return lost;
        }
        let items = seeds
            .into_iter()
            .map(|(id, expiry_ms)| MemoryItem {
                content: format!("source {id}"),
                id,
                kind: MemoryKind::Entity,
                source: "origin".into(),
                importance: None,
                observed_at_ms: Some(1),
                provenance: vec![Provenance {
                    source: "origin".into(),
                    kind: Some("caller".into()),
                    uri: None,
                    transformation: None,
                }],
                references: Vec::new(),
                merge_into: None,
                expected_revision: None,
                expiry_ms,
                expiry_valid: true,
                evidence: Vec::new(),
            })
            .collect();
        let request = RememberRequest {
            scope: self.scope(view),
            idempotency_key: "c1-sources".into(),
            receipt_window_ms: view["receipt_window_ms"].as_u64().unwrap(),
            now_ms: 1,
            deadline_ms: None,
            allow_future_observations: false,
            items,
        };
        let mut owner = self.service.database.write().unwrap();
        let db = owner.as_mut().unwrap();
        MemoryMut::bind(db, &self.graph, None)
            .and_then(|mut memory| memory.remember(&request))
            .unwrap();
        lost
    }

    /// Forgets receipt-only sources at the view clock (source loss after
    /// the receipt was written).
    fn lose(&self, view: &Value, ids: Vec<String>) {
        if ids.is_empty() {
            return;
        }
        let request = ForgetRequest {
            scope: self.scope(view),
            idempotency_key: "c1-source-loss".into(),
            receipt_window_ms: view["receipt_window_ms"].as_u64().unwrap(),
            now_ms: view["clock_ms"].as_u64().unwrap(),
            deadline_ms: None,
            targets: ids
                .into_iter()
                .map(|id| ForgetTarget {
                    id,
                    expected_revision: 1,
                })
                .collect(),
        };
        let mut owner = self.service.database.write().unwrap();
        let db = owner.as_mut().unwrap();
        MemoryMut::bind(db, &self.graph, None)
            .and_then(|mut memory| memory.forget(&request))
            .unwrap();
    }

    /// The first granted scope, the one the C1 fixtures write.
    fn scope(&self, view: &Value) -> MemoryScope {
        let scope = &view["scope_grants"][0];
        MemoryScope {
            tenant: self.tenant.clone(),
            agent: scope["agent"].as_str().unwrap().into(),
            user: scope["user"].as_str().map(str::to_owned),
            session: scope["session"].as_str().map(str::to_owned),
        }
    }

    /// A stored receipt: its original request is executed over HTTP at the
    /// clock that makes its window end at `retry_until_ms`. A merge item's
    /// target is first created at revision 1 from an unrelated source.
    async fn receipt(&self, view: &Value, receipt: &Value, path: &str) -> Value {
        let window = view["receipt_window_ms"].as_u64().unwrap();
        let clock = receipt["retry_until_ms"].as_u64().unwrap() - window;
        let request = &receipt["request"];
        let operation = receipt["operation"].as_str().unwrap();
        let prefix = path.splitn(5, '/').take(4).collect::<Vec<_>>().join("/");
        let path = format!("{prefix}/{operation}");
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
        if operation == "forget" {
            // The forgotten targets existed when the receipt was written.
            let items: Vec<Value> = request["target"]["ids"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|target| {
                    json!({"id": target["id"], "kind": "entity", "content": target["id"],
                        "observed_at_ms": 0, "source": {"source_id": "base", "source_kind": "caller"}})
                })
                .collect();
            let remember = format!("{prefix}/remember");
            let (status, body) = self
                .send(&json!({"method": "POST", "path": remember, "body": {
                    "scope": request["scope"], "idempotency_key": "replay-targets", "items": items,
                }}))
                .await;
            assert_eq!(status, 200, "receipt targets: {body}");
        }
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
/// Forget outcomes are at the receipt sequence, so they follow it. The
/// oracle's injected `manifest_id` is copied like a receipt ID after
/// checking the served notice omits it (the disclosed E4 gap).
fn normalize(name: &str, actual: &mut Value, expected: &Value, seeded: Option<&Value>) {
    // A receipt lookup wraps the mutation response in `result`.
    if let (Some(inner), Some(expected)) = (actual.get_mut("result"), expected.get("result")) {
        return normalize(name, inner, expected, seeded);
    }
    if let Some(receipt) = actual.get("receipt") {
        let sequence = receipt["sequence"].clone();
        for outcome in actual["outcomes"].as_array_mut().into_iter().flatten() {
            if outcome["action"] == "deleted" {
                assert_eq!(outcome["revision"], sequence, "{name}: deleted revision");
                outcome["revision"] = expected["receipt"]["sequence"].clone();
            }
        }
    }
    if let Some(notice) = actual.get_mut("erasure_notice") {
        assert!(notice.get("manifest_id").is_none(), "{name}: manifest");
        if let Some(manifest) = expected["erasure_notice"].get("manifest_id") {
            notice["manifest_id"] = manifest.clone();
        }
    }
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
        let consolidate = case["operation"] == "consolidate";
        let lost = if consolidate {
            harness.consolidation(view)
        } else {
            Vec::new()
        };
        let path = case["request"]["path"].as_str().unwrap();
        let mut seeded = None;
        // A view granting no scope cannot write its receipts over HTTP;
        // its request is refused before any lookup.
        let granted = view["scope_grants"] != json!([]);
        for receipt in view["receipts"].as_array().into_iter().flatten() {
            if granted {
                seeded = Some(harness.receipt(view, receipt, path).await);
            }
        }
        harness.lose(view, lost);
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
        (
            "recall-default-candidates-200-is-charged",
            "injected candidate_units",
        ),
        (
            "recall-default-candidates-inclusive-200",
            "injected candidate_units",
        ),
        ("recall-source-scan-100001", "injected source_count"),
        ("recall-unbounded-plan-refused", "injected bounded_plan"),
        ("recall-no-lazy-text-index", "injected text_index_ready"),
        ("recall-ranking-golden-default", "injected candidate ranks"),
        (
            "recall-ranking-golden-id-tie-expanded-R-zero",
            "injected candidate ranks",
        ),
        (
            "recall-ranking-golden-weighted-tie",
            "injected candidate ranks",
        ),
        (
            "recall-owner-ttl-equality-and-foreign-scope-hidden",
            "injected candidate ranks",
        ),
        ("recall-vector-exact", "injected candidate ranks"),
        ("recall-vector-mmr", "injected candidate ranks"),
        (
            "recall-bm25-rrf-preeligible-leg-diagnostics",
            "injected candidate ranks",
        ),
        (
            "recall-caller-OR-cannot-replace-mandatory-scope",
            "injected candidate ranks",
        ),
        (
            "recall-future-observation-default-refused",
            "injected candidate ranks",
        ),
        ("recall-future-clamp-explicit", "injected candidate ranks"),
        (
            "recall-exact-distance-tie-record-before-memory-id",
            "injected candidate ranks",
        ),
        (
            "recall-mmr-diversity-not-weighted-score-order",
            "injected candidate ranks",
        ),
        (
            "recall-vector-component-budget-before-fetch",
            "injected candidate ranks",
        ),
        (
            "recall-mmr-component-budget-before-selection",
            "injected candidate ranks",
        ),
        (
            "recall-ranking-golden-scope-before-memory-id",
            "injected candidate ranks",
        ),
        (
            "recall-temporal-expansion-gated",
            "injected temporal_enabled",
        ),
        (
            "recall-provider-off-even-with-ambient-credential",
            "injected ambient_credentials",
        ),
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
        (
            "recall-marked-deadline-partial",
            504,
            "deadline_exceeded",
            None,
        ),
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
            .send(
                &json!({"method": "POST", "path": "/v1/memory/notes/remember", "body": {
                    "scope": {"agent": "a"}, "idempotency_key": "k1", "items": [{
                        "id": "e1", "kind": "entity", "content": "Ada", "observed_at_ms": "90",
                        "source": {"source_id": "s1", "source_kind": "caller"},
                    }],
                }}),
            )
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
            (
                json!({"embedding_provider": "x", "kinds": ["fact"]}),
                "embedding_provider",
            ),
            (json!({"kinds": ["relation"], "filter": {}}), "fact_kinds"),
            (json!({"filter": {}, "as_of_ms": "1"}), "filter"),
            (
                json!({"as_of_ms": "1", "projection": {"paths": true}}),
                "temporal_read",
            ),
            (
                json!({"projection": {"paths": true, "metadata": ["k"]}}),
                "paths",
            ),
            (
                json!({"projection": {"metadata": ["k"]}, "limit": 0}),
                "metadata",
            ),
            (
                json!({"text": "x".repeat(1025), "limit": 0}),
                "query_text_length",
            ),
        ] {
            let mut request = body.clone();
            request["scope"] = json!({"agent": "a"});
            let (status, served) = recall_served(&view(), request).await;
            assert_eq!(status, 409, "{body}: {served}");
            assert_eq!(served["error"]["details"]["feature"], feature, "{body}");
        }
        let (status, served) = recall_served(
            &view(),
            json!({"scope": {"agent": "a"}, "text": "ada ".repeat(256)}),
        )
        .await;
        assert_eq!(status, 200, "{served}");
        let (status, served) = recall_served(
            &view(),
            json!({"scope": {"agent": "a"}, "text": "x".repeat(16_385)}),
        )
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
        let (status, body) = recall_served(
            &view(),
            json!({"scope": {"agent": "a"}, "entity_seeds": ["zz"]}),
        )
        .await;
        assert_eq!(status, 404, "{body}");
        assert_eq!(body["error"]["code"], "memory_not_found");
    }

    /// `target_kinds` injects fact/relation inputs, which the first-slice
    /// core cannot store; its `UnsupportedKind` 409 is a mapping test.
    const CONSOLIDATE_SKIP: [(&str, &str); 2] = [
        (
            "consolidate-input-fact-core-absent",
            "injected target_kinds",
        ),
        (
            "consolidate-input-relation-core-absent",
            "injected target_kinds",
        ),
    ];

    #[tokio::test]
    async fn consolidate_fixture_cases_replay_over_http() {
        let replayed = replay(CONSOLIDATE, &CONSOLIDATE_SKIP, &[]).await;
        assert_eq!(replayed, 12 - CONSOLIDATE_SKIP.len());
    }

    fn consolidate_request(output: Value, extra: Value) -> Value {
        let mut item = json!({
            "id": "s1", "kind": "episodic", "content": "summary", "observed_at_ms": 1,
            "source": {"source_id": "s", "source_kind": "caller", "transformation_id": "t"},
        });
        item.as_object_mut()
            .unwrap()
            .extend(output.as_object().unwrap().clone());
        let mut body = json!({
            "scope": {"agent": "a"}, "idempotency_key": "k",
            "inputs": [{"id": "m", "revision": "1"}], "output": item,
        });
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        json!({"method": "POST", "path": "/v1/memory/notes/consolidate", "body": body})
    }

    /// Field refusals win over validation in schema field order, and every
    /// refusal names its feature.
    #[tokio::test]
    async fn consolidate_refusals_precede_validation() {
        for (output, extra, feature) in [
            (
                json!({"kind": "relation", "content": 7}),
                json!({"dry_run": true}),
                "fact_kinds",
            ),
            (json!({"confidence": 2}), json!({}), "confidence"),
            (
                json!({"references": [{"id": "m", "revision": "1"}]}),
                json!({}),
                "references",
            ),
            (json!({"evidence": []}), json!({}), "evidence"),
            (json!({"metadata": {}}), json!({}), "metadata"),
            (json!({"assertion": {}}), json!({}), "assertion"),
            (
                json!({"expected": {"revision": "1"}}),
                json!({}),
                "expected",
            ),
            (json!({"merge": "merge_provenance"}), json!({}), "merge"),
            (json!({}), json!({"evidence": []}), "evidence"),
            (json!({}), json!({"dry_run": true}), "consolidate_plan"),
            (
                json!({"kind": "fact"}),
                json!({"inputs": [{"id": "m", "revision": "0", "disposition": "logical_delete"}]}),
                "consolidate_delete",
            ),
        ] {
            let (status, body) = served(&view(), &consolidate_request(output, extra)).await;
            assert_eq!(status, 409, "{feature}: {body}");
            assert_eq!(body["error"]["details"]["feature"], feature, "{body}");
        }
    }

    /// Validation: inputs within `min(max_references, 64)`, nonzero
    /// revisions, a known disposition, and an episodic output whose source
    /// names its transformation.
    #[tokio::test]
    async fn consolidate_validates_inputs_and_output() {
        let inputs = |n: usize| {
            json!(
                (0..n)
                    .map(|i| json!({"id": format!("m{i}"), "revision": "1"}))
                    .collect::<Vec<_>>()
            )
        };
        for (output, extra) in [
            (json!({}), json!({"inputs": []})),
            (json!({}), json!({"inputs": inputs(3), "max_references": 2})),
            (
                json!({}),
                json!({"inputs": inputs(65), "max_references": 256}),
            ),
            (json!({}), json!({"inputs": [{"id": "m", "revision": "0"}]})),
            (
                json!({}),
                json!({"inputs": [{"id": "m", "revision": "1", "disposition": "keep"}]}),
            ),
            (
                json!({"source": {"source_id": "s", "source_kind": "caller"}}),
                json!({}),
            ),
            (json!({"kind": "entity"}), json!({})),
            (json!({}), json!({"dry_run": "no"})),
            (json!({}), json!({"extra": 1})),
        ] {
            let (status, body) = served(&view(), &consolidate_request(output, extra)).await;
            assert_eq!(status, 422, "{body}");
            assert_eq!(body["error"]["code"], "invalid_input");
        }
    }

    /// Injected `target_kinds` and `attach` pre-state steps describe state
    /// the first-slice core cannot create; their 409s are mapping tests.
    const FORGET_SKIP: [(&str, &str); 3] = [
        (
            "forget-external-attachment-atomic-refusal",
            "injected attach pre_state",
        ),
        ("forget-id-fact-core-absent", "injected target_kinds"),
        ("forget-id-relation-core-absent", "injected target_kinds"),
    ];

    /// Refusals precede validation (mapping section 2): `expire` without
    /// `expires_at_ms` is refused, not rejected as malformed.
    const FORGET_DEVIATIONS: [Deviation; 1] = [(
        "forget-expire-requires-deadline",
        409,
        "memory_capability_unavailable",
        Some("forget_expire"),
    )];

    #[tokio::test]
    async fn forget_fixture_cases_replay_over_http() {
        let replayed = replay(FORGET, &FORGET_SKIP, &FORGET_DEVIATIONS).await;
        assert_eq!(replayed, 16 - FORGET_SKIP.len());
    }

    fn forget_request(body: Value) -> Value {
        let mut base = json!({
            "scope": {"agent": "a"}, "idempotency_key": "k",
            "target": {"ids": [{"id": "m", "revision": "1"}]}, "mode": "logical_delete",
        });
        base.as_object_mut()
            .unwrap()
            .extend(body.as_object().unwrap().clone());
        json!({"method": "POST", "path": "/v1/memory/notes/forget", "body": base})
    }

    #[tokio::test]
    async fn forget_refusals_precede_validation() {
        for (body, feature) in [
            (
                json!({"target": {"series": {"id": "x"}}, "mode": "expire"}),
                "forget_target",
            ),
            (json!({"target": {"entity": 1}}), "forget_target"),
            (json!({"target": {"session": true}}), "erasure_jobs"),
            (
                json!({"target": {"scope": "bad"}, "allow_job": 3}),
                "erasure_jobs",
            ),
            (
                json!({"mode": "expire", "allow_job": true}),
                "forget_expire",
            ),
            (json!({"allow_job": true, "mode": 1}), "erasure_jobs"),
        ] {
            let (status, body) = served(&view(), &forget_request(body)).await;
            assert_eq!(status, 409, "{feature}: {body}");
            assert_eq!(body["error"]["details"]["feature"], feature, "{body}");
        }
    }

    #[tokio::test]
    async fn forget_validates_targets_and_mode() {
        for body in [
            json!({"target": {"ids": []}}),
            json!({"target": {"ids": [{"id": "m", "revision": "0"}]}}),
            json!({"target": {"ids": [{"id": "m", "revision": "1", "x": 1}]}}),
            json!({"target": {"ids": [{"id": "m", "revision": "1"}], "x": 1}}),
            json!({"target": {}}),
            json!({"mode": "purge"}),
            json!({"expires_at_ms": "5"}),
            json!({"allow_job": "no"}),
            json!({"extra": 1}),
            json!({"target": {"ids": [{"id": "m", "revision": "1"}, {"id": "m", "revision": "1"}]}}),
        ] {
            let (status, body) = served(&view(), &forget_request(body)).await;
            assert_eq!(status, 422, "{body}");
            assert_eq!(body["error"]["code"], "invalid_input");
        }
    }

    /// Outcomes are sorted by ID, each at the receipt revision, with the
    /// logical state and the manifest-less notice.
    #[tokio::test]
    async fn forget_reports_sorted_deletions_at_the_receipt_revision() {
        let harness = harness(&view()).unwrap();
        harness.at(100);
        let remember = json!({"method": "POST", "path": "/v1/memory/notes/remember", "body": {
            "scope": {"agent": "a"}, "idempotency_key": "seed", "items": [
                {"id": "b", "kind": "entity", "content": "B", "observed_at_ms": 1,
                 "source": {"source_id": "s", "source_kind": "caller"}},
                {"id": "a", "kind": "entity", "content": "A", "observed_at_ms": 1,
                 "source": {"source_id": "s", "source_kind": "caller"}}]}});
        assert_eq!(harness.send(&remember).await.0, 200);
        let request = forget_request(json!({"target": {"ids": [
            {"id": "b", "revision": "1"}, {"id": "a", "revision": "1"}]}}));
        let (status, body) = harness.send(&request).await;
        assert_eq!(status, 200, "{body}");
        let sequence = body["receipt"]["sequence"].clone();
        assert_eq!(
            body["outcomes"],
            json!([{"id": "a", "action": "deleted", "revision": sequence},
                   {"id": "b", "action": "deleted", "revision": sequence}])
        );
        assert_eq!(body["state"], "logical");
        assert_eq!(body["receipt"]["operation"], "forget");
        assert_eq!(
            body["erasure_notice"],
            json!({"live_state_only": true, "secure_erasure": false,
                   "external_copies": "operator_managed", "restore_requires_manifest": true,
                   "backup_history_purge": "not_verified"})
        );
        let (status, again) = harness.send(&request).await;
        assert_eq!(status, 200, "{again}");
        assert_eq!(again["replayed"], true);
        assert_eq!(again["outcomes"], body["outcomes"]);
    }

    /// Remember lookup is the persisted-format blocker and correct_facts /
    /// resume_erasure have no first-slice receipts: each is refused (mapping
    /// section 3) where the oracle answers from its own store.
    const RECEIPT_DEVIATIONS: [Deviation; 6] = [
        (
            "receipt-live-before-expiry",
            409,
            "memory_capability_unavailable",
            Some("remember_receipt_lookup"),
        ),
        (
            "receipt-expiry-equality",
            409,
            "memory_capability_unavailable",
            Some("remember_receipt_lookup"),
        ),
        (
            "receipt-absent",
            409,
            "memory_capability_unavailable",
            Some("remember_receipt_lookup"),
        ),
        (
            "receipt-read-only",
            409,
            "memory_capability_unavailable",
            Some("remember_receipt_lookup"),
        ),
        (
            "receipt-absent-correct_facts-identity",
            409,
            "memory_capability_unavailable",
            Some("facts"),
        ),
        (
            "receipt-absent-resume_erasure-identity",
            409,
            "memory_capability_unavailable",
            Some("erasure_jobs"),
        ),
    ];

    #[tokio::test]
    async fn receipt_fixture_cases_replay_over_http() {
        assert_eq!(replay(RECEIPT, &[], &RECEIPT_DEVIATIONS).await, 9);
    }

    fn receipt_request(operation: Value, key: Value) -> Value {
        json!({"method": "POST", "path": "/v1/memory/notes/receipts/lookup", "body": {
            "scope": {"agent": "a"}, "operation": operation, "idempotency_key": key,
        }})
    }

    fn seed_request(id: &str) -> Value {
        json!({"method": "POST", "path": "/v1/memory/notes/remember", "body": {
            "scope": {"agent": "a"}, "idempotency_key": format!("seed-{id}"), "items": [
                {"id": id, "kind": "entity", "content": id, "observed_at_ms": 1,
                 "source": {"source_id": "s", "source_kind": "caller"}}]}})
    }

    /// A forget receipt reads back as the replay of the original response,
    /// until its window ends.
    #[tokio::test]
    async fn forget_receipt_lookup_replays_until_expiry() {
        let harness = harness(&view()).unwrap();
        harness.at(100);
        for id in ["b", "a"] {
            assert_eq!(harness.send(&seed_request(id)).await.0, 200);
        }
        let forget = forget_request(json!({"target": {"ids": [
            {"id": "b", "revision": "1"}, {"id": "a", "revision": "1"}]}}));
        let (status, forgot) = harness.send(&forget).await;
        assert_eq!(status, 200, "{forgot}");
        harness.at(200);
        let (status, body) = harness
            .send(&receipt_request(json!("forget"), json!("k")))
            .await;
        assert_eq!(status, 200, "{body}");
        let mut expected = forgot.clone();
        expected["replayed"] = json!(true);
        assert_eq!(body, json!({"result": expected}));
        harness.at(100 + 60_000 - 1);
        assert_eq!(
            harness
                .send(&receipt_request(json!("forget"), json!("k")))
                .await
                .0,
            200
        );
        harness.at(100 + 60_000);
        let (status, body) = harness
            .send(&receipt_request(json!("forget"), json!("k")))
            .await;
        assert_eq!(status, 404, "{body}");
        assert_eq!(body["error"]["code"], "receipt_not_found");
        // Operation is part of the identity.
        harness.at(200);
        let (status, _) = harness
            .send(&receipt_request(json!("consolidate"), json!("k")))
            .await;
        assert_eq!(status, 404);
    }

    #[tokio::test]
    async fn consolidate_receipt_lookup_replays_the_summary() {
        let harness = harness(&view()).unwrap();
        harness.at(100);
        assert_eq!(harness.send(&seed_request("m")).await.0, 200);
        let (status, created) = harness
            .send(&consolidate_request(json!({}), json!({})))
            .await;
        assert_eq!(status, 200, "{created}");
        let (status, body) = harness
            .send(&receipt_request(json!("consolidate"), json!("k")))
            .await;
        assert_eq!(status, 200, "{body}");
        let mut expected = created.clone();
        expected["replayed"] = json!(true);
        assert_eq!(body, json!({"result": expected}));
    }

    /// Scope authorization, then refusals, then validation; an authorized
    /// lookup without a receipt is 404.
    #[tokio::test]
    async fn receipt_lookup_checks_in_order() {
        let mut other = receipt_request(json!("forget"), json!("k"));
        other["body"]["scope"] = json!({"agent": "b"});
        let (status, body) = served(&view(), &other).await;
        assert_eq!(
            (status, &body["error"]["code"]),
            (403, &json!("forbidden_scope"))
        );
        for (operation, feature) in [
            ("remember", "remember_receipt_lookup"),
            ("correct_facts", "facts"),
            ("resume_erasure", "erasure_jobs"),
        ] {
            let (status, body) =
                served(&view(), &receipt_request(json!(operation), json!(""))).await;
            assert_eq!(status, 409, "{body}");
            assert_eq!(body["error"]["details"]["feature"], feature);
        }
        let mut extra = receipt_request(json!("forget"), json!("k"));
        extra["body"]["x"] = json!(1);
        for request in [
            receipt_request(json!("recall"), json!("k")),
            receipt_request(json!(1), json!("k")),
            receipt_request(json!("forget"), json!("")),
            receipt_request(json!("forget"), json!(7)),
            extra,
        ] {
            let (status, body) = served(&view(), &request).await;
            assert_eq!(status, 422, "{body}");
            assert_eq!(body["error"]["code"], "invalid_input");
        }
        let (status, body) = served(&view(), &receipt_request(json!("forget"), json!("k"))).await;
        assert_eq!(status, 404, "{body}");
        assert_eq!(body["error"]["code"], "receipt_not_found");
    }

    /// A retired binding keeps serving its receipts (mapping section 4).
    #[tokio::test]
    async fn retired_binding_serves_receipts() {
        let mut view = view();
        view["retired_at_ms"] = json!(500);
        let (status, body) = served(&view, &receipt_request(json!("forget"), json!("k"))).await;
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["error"]["code"], "memory_not_configured");
        let (status, body) = served(&view, &receipt_request(json!("remember"), json!("k"))).await;
        assert_eq!(status, 409, "{body}");
        assert_eq!(
            body["error"]["details"]["feature"],
            "remember_receipt_lookup"
        );
    }

    /// The refusal routes: binding and scope authorization first, then the
    /// refusal before any other field is read; `feature` names the gap.
    async fn refused_in_order(fixture: &str, path: &str, feature: &str) {
        assert_eq!(replay(fixture, &[], &[]).await, 1);
        let request = |body: Value| json!({"method": "POST", "path": path, "body": body});
        let malformed = request(json!({"scope": {"agent": "a"}, "x": [1], "deadline_ms": -1}));
        let (status, body) = served(&view(), &malformed).await;
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["error"]["code"], "memory_capability_unavailable");
        assert_eq!(body["error"]["details"]["feature"], feature);
        let (status, body) = served(&view(), &request(json!({"scope": {"agent": "b"}}))).await;
        assert_eq!(
            (status, &body["error"]["code"]),
            (403, &json!("forbidden_scope"))
        );
        let mut retired = view();
        retired["retired_at_ms"] = json!(500);
        let (status, body) = served(&retired, &malformed).await;
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["error"]["details"]["feature"], "binding_retired");
        let mut ungranted = view();
        ungranted["graph_grant"] = json!(false);
        let (status, body) = served(&ungranted, &malformed).await;
        assert_eq!(
            (status, &body["error"]["code"]),
            (403, &json!("forbidden_scope"))
        );
    }

    #[tokio::test]
    async fn find_facts_is_refused_after_authorization() {
        refused_in_order(FACTS, "/v1/memory/notes/facts/query", "facts").await;
    }

    #[tokio::test]
    async fn correct_facts_is_refused_after_authorization() {
        refused_in_order(CORRECT_FACTS, "/v1/memory/notes/facts/correct", "facts").await;
    }
}
