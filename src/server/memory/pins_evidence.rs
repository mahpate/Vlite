//! Pins for the evidence references of `remember` items (0.7.1 design note
//! section 5). Expectations come from the serving contract (sections 3, 4, 6
//! and 8: `MemoryItem.evidence`, `MemoryEvidenceRef`, the single 403 body, the
//! conflict codes), the adapter mapping and the design note, never from
//! adapter output. The `Clock` seam fixes the engine clock, so no test waits on
//! measured time.

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

use super::super::reserved::Bindings;
use super::super::{MAX_JSON_BYTES, RequestId, Service, admin, credentials};
use super::routes::Clock;
use crate::property_graph::{FindNodes, GraphOp, ReadOptions};
use crate::{
    CollectionSettings, CollectionSpec, CreateOptions, Database, FormatVersion, MetadataValue,
    Mutation,
};

const PRINCIPAL: &str = "abcdefghij27";

/// The bindings `notes` (graph `notes_graph`, evidence collection `notes_ev`)
/// and `bare` (a graph and no evidence collection), both granting the
/// credential the scopes `a` and `b`, with `raw_access` when asked.
struct Rig {
    router: Router,
    service: Service,
}

impl Rig {
    fn new(raw_access: bool) -> Self {
        let mut db = Database::create_in_memory(
            2,
            CreateOptions {
                format: Some(FormatVersion::V5),
                ..Default::default()
            },
        )
        .unwrap();
        db.create_graph("notes_graph").unwrap();
        db.create_graph("bare_graph").unwrap();
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
        let entry = |name: &str, evidence: Value| {
            let mut grant = json!({
                "principal": PRINCIPAL,
                "scopes": [{"agent": "a"}, {"agent": "b"}],
            });
            if raw_access {
                grant["raw_access"] = json!(true);
            }
            json!({
                "binding": name, "tenant": "@root", "generation": 1, "state": "active",
                "graph": format!("{name}_graph"), "evidence_collection": evidence,
                "schema_version": "memory-schema-v1", "ranking_version": "memory-ranking-v1",
                "digest_version": "memory-request-v1", "receipt_window_ms": 60_000,
                "grants": [grant],
            })
        };
        let text = json!({
            "version": "memory-bindings-v1",
            "bindings": [entry("notes", json!("notes_ev")), entry("bare", Value::Null)],
        })
        .to_string();
        let set = crate::memory::binding::parse(text.as_bytes()).unwrap();
        let service = Service::new(db).with_bindings(Bindings::new(set));
        super::startup_at(&service, 0).unwrap();
        let grant = credentials::Grant::for_user(
            PRINCIPAL.into(),
            "evidence-pins".into(),
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
            .layer(Extension(RequestId("evidence-pins".into())))
            .layer(Extension(clock))
            .layer(DefaultBodyLimit::max(MAX_JSON_BYTES));
        Self { router, service }
    }

    async fn post(&self, route: &str, body: &Value) -> (u16, Value) {
        let request = Request::builder()
            .method("POST")
            .uri(format!("/v1/memory/{route}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(body).unwrap()))
            .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    /// An evidence record written through the core: the first REST slice
    /// cannot create one.
    fn record(&self, record: u64, vector: [f32; 2], payload: &[u8]) {
        let mut owner = self.service.database.write().unwrap();
        owner
            .as_mut()
            .unwrap()
            .graph_mut("notes_graph")
            .unwrap()
            .write_batch(&[GraphOp::Vector {
                collection: "notes_ev".into(),
                mutation: Mutation::Put {
                    id: record,
                    vector: vector.to_vec(),
                    payload: payload.to_vec(),
                },
            }])
            .unwrap();
    }

    fn has_record(&self, record: u64) -> bool {
        let owner = self.service.database.read().unwrap();
        owner
            .as_ref()
            .unwrap()
            .collection("notes_ev")
            .unwrap()
            .get_at(record, 100)
            .unwrap()
            .is_some()
    }

    fn sequence(&self) -> u64 {
        self.service
            .database
            .read()
            .unwrap()
            .as_ref()
            .unwrap()
            .sequence
    }

    /// The `evidence` property of every memory node of the bound graph.
    fn stored_evidence(&self) -> Vec<String> {
        let owner = self.service.database.read().unwrap();
        let nodes = owner
            .as_ref()
            .unwrap()
            .graph("notes_graph")
            .unwrap()
            .find_nodes(
                &FindNodes {
                    label: Some("VliteMemory".into()),
                    limit: Some(100),
                    ..FindNodes::default()
                },
                &ReadOptions::default(),
            )
            .unwrap()
            .output;
        nodes
            .iter()
            .map(|node| match node.properties.get("evidence") {
                Some(MetadataValue::String(value)) => value.clone(),
                other => panic!("evidence property is {other:?}"),
            })
            .collect()
    }
}

fn item(id: &str, evidence: Option<Value>) -> Value {
    let mut item = json!({
        "id": id, "kind": "entity", "content": format!("content {id}"),
        "observed_at_ms": "90", "source": {"source_id": "s1", "source_kind": "caller"},
    });
    if let Some(evidence) = evidence {
        item["evidence"] = evidence;
    }
    item
}

/// One reference of the binding `notes`.
fn reference(record: impl Into<Value>) -> Value {
    json!([{"collection": "notes", "record": record.into()}])
}

fn remember(scope: &str, key: &str, items: Vec<Value>) -> Value {
    json!({"scope": {"agent": scope}, "idempotency_key": key, "items": items})
}

fn code(answer: &Value) -> &str {
    answer["error"]["code"].as_str().unwrap_or("<none>")
}

fn assert_forbidden(status: u16, answer: &Value, case: &str) {
    assert_eq!(status, 403, "{case}: {answer}");
    assert_eq!(code(answer), "forbidden_scope", "{case}: {answer}");
    assert!(answer["error"].get("details").is_none(), "{case}: {answer}");
}

fn assert_conflict(status: u16, answer: &Value, case: &str) {
    assert_eq!(status, 409, "{case}: {answer}");
    assert_eq!(
        code(answer),
        "memory_attachment_conflict",
        "{case}: {answer}"
    );
}

fn assert_invalid_evidence(status: u16, answer: &Value, case: &str) {
    assert_eq!(status, 422, "{case}: {answer}");
    assert_eq!(code(answer), "invalid_input", "{case}: {answer}");
    assert_eq!(
        answer["error"]["details"],
        json!({"field": "evidence"}),
        "{case}: {answer}"
    );
}

/// Contract section 3 (`MemoryItem.evidence`): a reference names an existing
/// record of the binding's evidence collection and attaches it to the
/// memory; recall then reports it (as `{collection, record, snippet}`) and
/// finds the memory by the record's vector. A `MemoryU64` is a canonical
/// decimal string or an exact JSON integer.
#[tokio::test]
async fn a_reference_attaches_an_existing_record_and_recall_reports_it() {
    let rig = Rig::new(true);
    rig.record(4242, [1.0, 0.0], b"proof");
    rig.record(4243, [0.0, 1.0], b"other");
    for (key, id, record) in [("k1", "e1", json!("4242")), ("k2", "e2", json!(4243))] {
        let (status, answer) = rig
            .post(
                "notes/remember",
                &remember("a", key, vec![item(id, Some(reference(record)))]),
            )
            .await;
        assert_eq!(status, 200, "{answer}");
        assert_eq!(
            answer["outcomes"],
            json!([{"id": id, "action": "created", "revision": "1"}])
        );
    }
    let seeded = json!({"scope": {"agent": "a"}, "entity_seeds": ["e1"]});
    let (status, recalled) = rig.post("notes/recall", &seeded).await;
    assert_eq!(status, 200, "{recalled}");
    assert_eq!(
        recalled["memories"][0]["evidence"],
        json!([{"collection": "notes", "record": "4242", "snippet": "proof"}])
    );
    let by_vector = json!({"scope": {"agent": "a"}, "vector": [1, 0], "limit": 1});
    let (status, recalled) = rig.post("notes/recall", &by_vector).await;
    assert_eq!(status, 200, "{recalled}");
    assert_eq!(recalled["memories"][0]["id"], "e1");
    assert_eq!(recalled["memories"][0]["evidence"][0]["record"], "4242");
    let by_vector = json!({"scope": {"agent": "a"}, "vector": [0, 1], "limit": 1});
    let (_, recalled) = rig.post("notes/recall", &by_vector).await;
    assert_eq!(recalled["memories"][0]["id"], "e2");
}

/// Design note 5.2: a reference claims a record that no memory owns, so it
/// takes the `raw_access` of the binding grant. Without it every reference
/// (an existing record, a missing one, one another scope owns, a collection
/// that is not the binding's) gets the one 403 body, before any record is
/// looked at and without a write, so the answer does not tell whether a
/// record exists or who owns it.
#[tokio::test]
async fn a_reference_without_raw_access_is_the_single_403_before_any_lookup() {
    let rig = Rig::new(false);
    rig.record(4242, [1.0, 0.0], b"proof");
    let sequence = rig.sequence();
    let wrong_alias = json!([{"collection": "bare", "record": "4242"}]);
    let mut bodies = Vec::new();
    for (case, evidence) in [
        ("existing record", reference("4242")),
        ("missing record", reference("999")),
        ("integer record", reference(4242)),
        ("wrong alias", wrong_alias),
        ("malformed record", reference("007")),
    ] {
        let (status, answer) = rig
            .post(
                "notes/remember",
                &remember("a", case, vec![item("e1", Some(evidence))]),
            )
            .await;
        assert_forbidden(status, &answer, case);
        bodies.push(answer);
    }
    assert!(
        bodies.windows(2).all(|pair| pair[0] == pair[1]),
        "{bodies:?}"
    );
    // The reference may be on any item of the request.
    let (status, answer) = rig
        .post(
            "notes/remember",
            &remember(
                "a",
                "second",
                vec![item("p1", None), item("e1", Some(reference("4242")))],
            ),
        )
        .await;
    assert_forbidden(status, &answer, "second item");
    assert_eq!(answer, bodies[0]);
    assert_eq!(rig.sequence(), sequence, "nothing was written");
    assert!(rig.has_record(4242));
    let (status, answer) = rig
        .post(
            "notes/remember",
            &remember("a", "plain", vec![item("p1", None)]),
        )
        .await;
    assert_eq!(
        status, 200,
        "an item without a reference still works: {answer}"
    );
}

/// A record that cannot be attached (missing, or owned by another memory of
/// the same scope or of another scope) is always 409
/// `memory_attachment_conflict` with the same body and nothing written, so a
/// refusal does not say which.
#[tokio::test]
async fn a_record_that_cannot_be_attached_is_always_the_same_conflict() {
    let rig = Rig::new(true);
    rig.record(4242, [1.0, 0.0], b"proof");
    let (status, answer) = rig
        .post(
            "notes/remember",
            &remember("a", "owner", vec![item("owner", Some(reference("4242")))]),
        )
        .await;
    assert_eq!(status, 200, "{answer}");
    let sequence = rig.sequence();
    let mut bodies = Vec::new();
    for (case, scope, record) in [
        ("missing", "a", "999"),
        ("owned in the same scope", "a", "4242"),
        ("owned in another scope", "b", "4242"),
    ] {
        let (status, answer) = rig
            .post(
                "notes/remember",
                &remember(scope, case, vec![item("thief", Some(reference(record)))]),
            )
            .await;
        assert_conflict(status, &answer, case);
        bodies.push(answer);
        assert_eq!(rig.sequence(), sequence, "{case}: nothing was written");
    }
    assert!(
        bodies.windows(2).all(|pair| pair[0] == pair[1]),
        "{bodies:?}"
    );
}

/// A reference is `{collection, record}` and nothing else, at most one per
/// item (the engine attaches one record to a memory): every other shape is
/// 422 `invalid_input` naming `evidence`, and an empty list is no reference.
#[tokio::test]
async fn reference_shapes_are_validated_and_name_the_evidence_field() {
    let rig = Rig::new(true);
    rig.record(4242, [1.0, 0.0], b"proof");
    for (case, evidence) in [
        ("leading zero", reference("0042")),
        ("plus sign", reference("+42")),
        ("negative", reference("-1")),
        ("fraction", reference(42.5)),
        ("whole float", reference(json!(42.0))),
        ("exponent", reference("1e3")),
        ("padded", reference(" 42")),
        ("overflow", reference("18446744073709551616")),
        ("boolean", reference(true)),
        ("null", reference(Value::Null)),
        ("reserved id", reference("record:42")),
        ("no record", json!([{"collection": "notes"}])),
        ("no collection", json!([{"record": "4242"}])),
        (
            "null collection",
            json!([{"collection": null, "record": "4242"}]),
        ),
        (
            "extra field",
            json!([{"collection": "notes", "record": "4242", "snippet": "x"}]),
        ),
        (
            "two references",
            json!([
                {"collection": "notes", "record": "4242"},
                {"collection": "notes", "record": "4243"},
            ]),
        ),
        (
            "an object",
            json!({"collection": "notes", "record": "4242"}),
        ),
        ("a string", json!("4242")),
        ("an entry that is not an object", json!(["4242"])),
    ] {
        let (status, answer) = rig
            .post(
                "notes/remember",
                &remember("a", "shapes", vec![item("e1", Some(evidence))]),
            )
            .await;
        assert_invalid_evidence(status, &answer, case);
    }
    let (status, answer) = rig
        .post(
            "notes/remember",
            &remember("a", "empty", vec![item("e1", Some(json!([])))]),
        )
        .await;
    assert_eq!(status, 200, "an empty list is no reference: {answer}");
    assert_eq!(rig.stored_evidence(), [""]);
}

/// A collection that is not the binding's alias is not granted: the same 403
/// body as an unknown binding, never a 422 that names what exists.
#[tokio::test]
async fn a_collection_that_is_not_the_binding_alias_is_the_single_403() {
    let rig = Rig::new(true);
    rig.record(4242, [1.0, 0.0], b"proof");
    let (_, unknown) = rig
        .post(
            "absent/remember",
            &remember("a", "k", vec![item("e1", None)]),
        )
        .await;
    for (case, collection) in [
        ("the stored name", "notes_ev"),
        ("another binding", "bare"),
        ("another case", "Notes"),
        ("empty", ""),
    ] {
        let evidence = json!([{"collection": collection, "record": "4242"}]);
        let (status, answer) = rig
            .post(
                "notes/remember",
                &remember("a", case, vec![item("e1", Some(evidence))]),
            )
            .await;
        assert_forbidden(status, &answer, case);
        assert_eq!(answer, unknown, "{case}");
    }
}

/// Mapping section 3 and design note 5.1: the request's own `evidence` list
/// (records to create) is still refused on every binding, and an item's
/// references are refused on a binding without an evidence collection;
/// refusals precede the authorization of a reference.
#[tokio::test]
async fn request_evidence_and_a_binding_without_evidence_stay_refused() {
    for raw_access in [true, false] {
        let rig = Rig::new(raw_access);
        let named = json!([{"collection": "notes", "record": "4242"}]);
        let mut body = remember("a", "request", vec![item("e1", None)]);
        body["evidence"] = named.clone();
        let (status, answer) = rig.post("notes/remember", &body).await;
        assert_eq!(status, 409, "{raw_access}: {answer}");
        assert_eq!(code(&answer), "memory_capability_unavailable");
        assert_eq!(answer["error"]["details"], json!({"feature": "evidence"}));
        let bare = json!([{"collection": "bare", "record": "4242"}]);
        let (status, answer) = rig
            .post(
                "bare/remember",
                &remember("a", "bare", vec![item("e1", Some(bare))]),
            )
            .await;
        assert_eq!(status, 409, "{raw_access}: {answer}");
        assert_eq!(code(&answer), "memory_capability_unavailable");
        assert_eq!(answer["error"]["details"], json!({"feature": "evidence"}));
        // An empty list is no evidence anywhere.
        let (status, answer) = rig
            .post(
                "bare/remember",
                &remember("a", "empty", vec![item("e2", Some(json!([])))]),
            )
            .await;
        assert_eq!(status, 200, "{raw_access}: {answer}");
    }
}

/// The digest hashes the reserved ID: an exact retry replays, and a retry
/// that names another record is `idempotency_conflict` (contract section 3).
#[tokio::test]
async fn a_reference_replays_exactly_and_another_record_conflicts() {
    let rig = Rig::new(true);
    rig.record(4242, [1.0, 0.0], b"proof");
    rig.record(4243, [0.0, 1.0], b"other");
    let body = remember("a", "once", vec![item("e1", Some(reference("4242")))]);
    let (status, first) = rig.post("notes/remember", &body).await;
    assert_eq!(status, 200, "{first}");
    assert_eq!(first["replayed"], false);
    let sequence = rig.sequence();
    let (status, again) = rig.post("notes/remember", &body).await;
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["replayed"], true);
    assert_eq!(again["receipt"], first["receipt"]);
    assert_eq!(rig.sequence(), sequence);
    let other = remember("a", "once", vec![item("e1", Some(reference("4243")))]);
    let (status, answer) = rig.post("notes/remember", &other).await;
    assert_eq!(status, 409, "{answer}");
    assert_eq!(code(&answer), "idempotency_conflict");
    let without = remember("a", "once", vec![item("e1", None)]);
    let (status, answer) = rig.post("notes/remember", &without).await;
    assert_eq!(status, 409, "{answer}");
    assert_eq!(code(&answer), "idempotency_conflict");
}

/// Design note 5.3: forgetting a memory deletes the record it referenced, as
/// it does for every evidence record that no other retained memory uses.
#[tokio::test]
async fn forgetting_a_memory_deletes_the_record_it_referenced() {
    let rig = Rig::new(true);
    rig.record(4242, [1.0, 0.0], b"proof");
    let (status, answer) = rig
        .post(
            "notes/remember",
            &remember("a", "k", vec![item("e1", Some(reference("4242")))]),
        )
        .await;
    assert_eq!(status, 200, "{answer}");
    assert!(rig.has_record(4242));
    let forget = json!({
        "scope": {"agent": "a"}, "idempotency_key": "f1", "mode": "logical_delete",
        "target": {"ids": [{"id": "e1", "revision": "1"}]},
    });
    let (status, answer) = rig.post("notes/forget", &forget).await;
    assert_eq!(status, 200, "{answer}");
    assert!(!rig.has_record(4242));
    let seeded = json!({"scope": {"agent": "a"}, "entity_seeds": ["e1"]});
    let (status, answer) = rig.post("notes/recall", &seeded).await;
    assert_eq!(status, 404, "{answer}");
}

/// The reserved ID is written only for a reference a client sent: a workload
/// of plain items, empty lists, a consolidation and a forget stores none, and
/// one reference stores exactly one, as itself.
#[tokio::test]
async fn a_record_addressed_id_is_stored_only_for_a_reference_the_client_sent() {
    let rig = Rig::new(true);
    rig.record(4242, [1.0, 0.0], b"proof");
    for (key, items) in [
        ("k1", vec![item("p1", None), item("p2", Some(json!([])))]),
        ("k2", vec![item("p3", None)]),
    ] {
        let (status, answer) = rig.post("notes/remember", &remember("a", key, items)).await;
        assert_eq!(status, 200, "{answer}");
    }
    let consolidate = json!({
        "scope": {"agent": "a"}, "idempotency_key": "c1",
        "inputs": [{"id": "p1", "revision": "1"}],
        "output": {
            "id": "sum", "kind": "episodic", "content": "summary", "observed_at_ms": "90",
            "source": {"source_id": "s1", "source_kind": "caller", "transformation_id": "t1"},
        },
    });
    let (status, answer) = rig.post("notes/consolidate", &consolidate).await;
    assert_eq!(status, 200, "{answer}");
    let forget = json!({
        "scope": {"agent": "a"}, "idempotency_key": "f1", "mode": "logical_delete",
        "target": {"ids": [{"id": "p3", "revision": "1"}]},
    });
    let (status, answer) = rig.post("notes/forget", &forget).await;
    assert_eq!(status, 200, "{answer}");
    let stored = rig.stored_evidence();
    assert_eq!(stored.len(), 3, "{stored:?}");
    assert!(stored.iter().all(String::is_empty), "{stored:?}");
    let (status, answer) = rig
        .post(
            "notes/remember",
            &remember("a", "ref", vec![item("e1", Some(reference("4242")))]),
        )
        .await;
    assert_eq!(status, 200, "{answer}");
    let stored = rig.stored_evidence();
    assert_eq!(stored.len(), 4, "{stored:?}");
    assert_eq!(stored.iter().filter(|value| value.is_empty()).count(), 3);
    assert_eq!(
        stored
            .iter()
            .filter(|value| value.as_str() == "record:4242")
            .count(),
        1,
        "{stored:?}"
    );
}
