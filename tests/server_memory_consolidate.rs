#![cfg(feature = "server")]
//! Consolidate route pins over HTTP. Every expectation is derived from the
//! serving memory contract (sections 3-6 and 8), the adapter mapping
//! (sections 1, 2 and 6) and the frozen C1 contract (sections 2-3), not from
//! the adapter's behavior: check order, refusal priority, strict input
//! shapes, limits, exact-scope inputs and which request fields the core
//! digest must see. Clock-seam and credential-incarnation pins live in
//! `src/server/memory/pins_consolidate.rs`.

use axum::{Router, body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tower::ServiceExt;
use vlite::{
    CollectionSpec, CreateOptions, Database, FormatVersion,
    server::{BearerToken, ServerOptions, Service, configured_admin_router},
};

const ADMIN: &str = "fake_consolidate_pins_admin_token_0123456789_ABCDEFGHIJKLMNOP";
const ID: &str = "abcdefghij27";
const CONSOLIDATE: &str = "/v1/memory/notes/consolidate";
const REMEMBER: &str = "/v1/memory/notes/remember";
const ALL_OPS: [&str; 4] = ["read", "write", "graph_read", "graph_write"];
const SCOPES: &str =
    r#"[{"agent":"a"},{"agent":"a","user":"u"},{"agent":"a","user":"u","session":"s"}]"#;

fn spec() -> CollectionSpec {
    CollectionSpec {
        dimensions: 2,
        settings: Default::default(),
    }
}

fn options() -> CreateOptions {
    CreateOptions {
        format: Some(FormatVersion::V5),
        ..Default::default()
    }
}

fn database() -> Database {
    let mut db = Database::create_in_memory(2, options()).unwrap();
    db.create_collection("notes_ev", spec()).unwrap();
    db.create_graph("notes_graph").unwrap();
    db
}

/// A database file reopened read-only; `directory` must outlive it.
fn read_only_database(directory: &TempDir) -> Database {
    let path = directory.path().join("memory.vlite");
    let mut db = Database::create_with_options(&path, 2, options()).unwrap();
    db.create_collection("notes_ev", spec()).unwrap();
    db.create_graph("notes_graph").unwrap();
    db.close().unwrap();
    Database::open_read_only(&path).unwrap()
}

/// The binding `notes`, granting the principal `scopes`.
fn bindings(scopes: &str, retired: bool) -> String {
    let state = if retired {
        r#""state":"retired","retired_at_ms":4102444800000"#
    } else {
        r#""state":"active""#
    };
    format!(
        r#"{{"version":"memory-bindings-v1","bindings":[{{"binding":"notes","tenant":"@root","generation":1,{state},"graph":"notes_graph","evidence_collection":"notes_ev","schema_version":"memory-schema-v1","ranking_version":"memory-ranking-v1","digest_version":"memory-request-v1","receipt_window_ms":86400000,"grants":[{{"principal":"{ID}","scopes":{scopes}}}]}}]}}"#
    )
}

struct Rig {
    app: Router,
    token: String,
    _directory: TempDir,
}

fn rig_with(db: Database, bindings: &str, operations: &[&str]) -> Rig {
    let directory = TempDir::new().unwrap();
    let secret = "A".repeat(43);
    let evidence = db
        .collections()
        .unwrap()
        .into_iter()
        .find(|info| info.name == "notes_ev")
        .unwrap()
        .id;
    let credentials = json!({
        "schema": 1,
        "credentials": [{
            "id": ID,
            "name": "fake consolidate credential",
            "all": false,
            "collections": [{"name": "notes_ev", "id": evidence}],
            "operations": operations,
            "graphs": ["notes_graph"],
            "lineage": format!("{:016x}", db.lineage().unwrap()),
            "secret_sha256": format!("{:x}", Sha256::digest(secret.as_bytes())),
            "created_unix_ms": 0,
            "expires_unix_ms": 9_999_999_999_999u64,
            "revoked_unix_ms": null,
            "rotated_from": null
        }]
    });
    std::fs::write(
        directory.path().join("credentials.json"),
        serde_json::to_vec(&credentials).unwrap(),
    )
    .unwrap();
    let path = directory.path().join("memory-bindings.json");
    std::fs::write(&path, bindings).unwrap();
    let app = configured_admin_router(
        Service::new(db),
        ServerOptions {
            admin_token: Some(BearerToken::new(ADMIN).unwrap()),
            admin_directory: Some(directory.path().into()),
            memory_bindings_file: Some(path),
            ..Default::default()
        },
    )
    .unwrap();
    Rig {
        app,
        token: format!("vlk_{ID}_{secret}"),
        _directory: directory,
    }
}

fn rig() -> Rig {
    rig_with(database(), &bindings(SCOPES, false), &ALL_OPS)
}

impl Rig {
    async fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        let request = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", self.token))
            .body(Body::from(serde_json::to_vec(body).unwrap()))
            .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// Remembers one entity per id in `scope`, each created at revision 1.
    async fn seed(&self, scope: &Value, ids: &[String]) {
        let items: Vec<Value> = ids
            .iter()
            .map(|id| {
                json!({
                    "id": id, "kind": "entity", "content": format!("source {id}"),
                    "observed_at_ms": "90",
                    "source": {"source_id": "origin", "source_kind": "caller"}
                })
            })
            .collect();
        let body = json!({
            "scope": scope, "idempotency_key": format!("seed-{}", ids[0]),
            "items": items, "deadline_ms": 5000
        });
        let (status, seeded) = self.post(REMEMBER, &body).await;
        assert_eq!(status, 200, "{seeded}");
    }
}

fn ids(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn summary_output() -> Value {
    json!({
        "id": "sum1",
        "kind": "episodic",
        "content": "summary text",
        "observed_at_ms": "90",
        "source": {"source_id": "s2", "source_kind": "caller", "transformation_id": "t1"}
    })
}

fn consolidate_body(key: &str, inputs: Value) -> Value {
    json!({
        "scope": {"agent": "a"},
        "idempotency_key": key,
        "inputs": inputs,
        "output": summary_output(),
        "deadline_ms": 5000
    })
}

/// A request whose key, summary ID and content are unique to `n`.
fn consolidate_n(n: usize, inputs: Value) -> Value {
    let mut body = consolidate_body(&format!("k{n}"), inputs);
    body["output"]["id"] = json!(format!("sum{n}"));
    body["output"]["content"] = json!(format!("summary text {n}"));
    body
}

fn one(id: &str) -> Value {
    json!([{"id": id, "revision": "1"}])
}

fn refused(result: &(u16, Value), feature: &str, context: &str) {
    let (status, body) = result;
    assert_eq!(
        (*status, &body["error"]["code"], &body["error"]["details"]),
        (
            409,
            &json!("memory_capability_unavailable"),
            &json!({"feature": feature})
        ),
        "{context}: {body}"
    );
}

fn invalid(result: &(u16, Value), field: &str, context: &str) {
    let (status, body) = result;
    assert_eq!(
        (*status, &body["error"]["code"], &body["error"]["details"]),
        (422, &json!("invalid_input"), &json!({"field": field})),
        "{context}: {body}"
    );
}

fn named_evidence() -> Value {
    json!([{"collection": "notes_ev", "record": "1"}])
}

/// The field refusals in contract field order (`inputs`, then the output
/// item's schema order, then request `evidence`, then `dry_run`), each with
/// the request change that triggers it.
fn probes() -> Vec<(&'static str, Change)> {
    vec![
        ("consolidate_delete", |body| {
            body["inputs"][0]["disposition"] = json!("logical_delete")
        }),
        ("fact_kinds", |body| body["output"]["kind"] = json!("fact")),
        ("confidence", |body| {
            body["output"]["confidence"] = json!(0.5)
        }),
        ("references", |body| {
            body["output"]["references"] = json!([{"id": "m1", "revision": "1"}])
        }),
        ("evidence", |body| {
            body["output"]["evidence"] = named_evidence()
        }),
        ("metadata", |body| {
            body["output"]["metadata"] = json!({"k": "v"})
        }),
        ("assertion", |body| {
            body["output"]["assertion"] = json!({"subject_id": "m1"})
        }),
        ("expected", |body| {
            body["output"]["expected"] = json!({"revision": "1"})
        }),
        ("merge", |body| {
            body["output"]["merge"] = json!("merge_provenance")
        }),
        ("evidence", |body| body["evidence"] = named_evidence()),
        ("consolidate_plan", |body| body["dry_run"] = json!(true)),
    ]
}

/// Mapping section 2: when several field refusals apply, the first in
/// request field order names `details.feature`. Every ordered pair is tried.
#[tokio::test]
async fn field_refusals_follow_the_contract_field_order() {
    let rig = rig();
    let probes = probes();
    for (index, (feature, apply)) in probes.iter().enumerate() {
        let mut body = consolidate_body("k", one("m1"));
        apply(&mut body);
        refused(&rig.post(CONSOLIDATE, &body).await, feature, feature);
        for (later_feature, later) in &probes[index + 1..] {
            if later_feature == feature {
                continue;
            }
            let mut body = consolidate_body("k", one("m1"));
            apply(&mut body);
            later(&mut body);
            let context = format!("{feature} before {later_feature}");
            refused(&rig.post(CONSOLIDATE, &body).await, feature, &context);
        }
    }
}

/// Mapping section 1: a `logical_delete` disposition is the
/// `consolidate_delete` refusal on whichever input carries it.
#[tokio::test]
async fn a_logical_delete_is_refused_on_any_input() {
    let rig = rig();
    for position in 0..3 {
        let mut inputs: Vec<Value> = (0..3)
            .map(|i| json!({"id": format!("m{i}"), "revision": "1"}))
            .collect();
        inputs[position]["disposition"] = json!("logical_delete");
        let body = consolidate_n(position, Value::Array(inputs));
        let context = format!("input {position}");
        refused(
            &rig.post(CONSOLIDATE, &body).await,
            "consolidate_delete",
            &context,
        );
    }
}

/// Serving sections 4 and 8: `dry_run: false`, `merge: none`, `disposition:
/// retain` and an empty `references` or `evidence` list are the defaults or
/// absent, so they are served and are the same request as one without them.
#[tokio::test]
async fn default_valued_fields_are_served_not_refused() {
    let rig = rig();
    rig.seed(&json!({"agent": "a"}), &ids(&["m1"])).await;
    let mut body = consolidate_n(0, one("m1"));
    body["dry_run"] = json!(false);
    body["require_complete_feed"] = json!(false);
    body["evidence"] = json!([]);
    body["inputs"][0]["disposition"] = json!("retain");
    body["output"]["merge"] = json!("none");
    body["output"]["references"] = json!([]);
    body["output"]["evidence"] = json!([]);
    let (status, first) = rig.post(CONSOLIDATE, &body).await;
    assert_eq!(status, 200, "{first}");
    assert_eq!(first["replayed"], false);
    let (status, again) = rig.post(CONSOLIDATE, &consolidate_n(0, one("m1"))).await;
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["replayed"], true);
    assert_eq!(again["receipt"], first["receipt"]);
}

/// Mapping section 2 check order: capability refusals (4) precede request
/// validation (5), so a refused request needs no valid key, inputs or output.
#[tokio::test]
async fn a_refusal_is_decided_before_request_validation() {
    let rig = rig();
    let cases = [
        (
            json!({"scope": {"agent": "a"}, "dry_run": true}),
            "consolidate_plan",
        ),
        (
            json!({"scope": {"agent": "a"}, "idempotency_key": "", "dry_run": true}),
            "consolidate_plan",
        ),
        (
            json!({"scope": {"agent": "a"}, "idempotency_key": 7, "inputs": "x",
                "output": 1, "dry_run": true}),
            "consolidate_plan",
        ),
        (
            json!({"scope": {"agent": "a"}, "unknown": 1, "evidence": named_evidence()}),
            "evidence",
        ),
        (
            json!({"scope": {"agent": "a"}, "max_references": 0, "deadline_ms": 0,
                "inputs": [{"id": "m1", "revision": "1", "disposition": "logical_delete"}]}),
            "consolidate_delete",
        ),
    ];
    for (body, feature) in cases {
        refused(
            &rig.post(CONSOLIDATE, &body).await,
            feature,
            &body.to_string(),
        );
    }
    // Without a refusal the same malformed shapes are validation failures.
    let (status, error) = rig
        .post(CONSOLIDATE, &json!({"scope": {"agent": "a"}}))
        .await;
    assert_eq!(
        (status, &error["error"]["code"]),
        (422, &json!("invalid_input"))
    );
}

/// Mapping section 2 check order: scope authorization (2) and the writable
/// precheck (3) precede every capability refusal (4), even `dry_run: true`.
#[tokio::test]
async fn authorization_and_read_only_precede_every_refusal() {
    let rig = rig();
    let body = json!({"scope": {"agent": "b"}, "dry_run": true});
    let (status, error) = rig.post(CONSOLIDATE, &body).await;
    assert_eq!(
        (status, &error["error"]["code"]),
        (403, &json!("forbidden_scope")),
        "{error}"
    );

    let directory = TempDir::new().unwrap();
    let rig = rig_with(
        read_only_database(&directory),
        &bindings(SCOPES, false),
        &ALL_OPS,
    );
    for body in [
        json!({"scope": {"agent": "a"}, "dry_run": true}),
        json!({"scope": {"agent": "a"}, "evidence": named_evidence()}),
        consolidate_body("k", one("m1")),
    ] {
        let (status, error) = rig.post(CONSOLIDATE, &body).await;
        assert_eq!(
            (status, &error["error"]["code"]),
            (403, &json!("read_only")),
            "{body}: {error}"
        );
    }
}

/// Mapping section 2 multi-refusal priority: `binding_retired`, then
/// `memory_feed_unsupported`, then field refusals.
#[tokio::test]
async fn binding_state_and_feed_requests_precede_field_refusals() {
    let body = json!({"scope": {"agent": "a"}, "require_complete_feed": true, "dry_run": true});
    let retired = rig_with(database(), &bindings(SCOPES, true), &ALL_OPS);
    refused(
        &retired.post(CONSOLIDATE, &body).await,
        "binding_retired",
        "retired binding",
    );

    let rig = rig();
    let (status, error) = rig.post(CONSOLIDATE, &body).await;
    assert_eq!(
        (status, &error["error"]["code"]),
        (409, &json!("memory_feed_unsupported")),
        "{error}"
    );

    // A complete feed is refused before any write, so the key stays unused.
    rig.seed(&json!({"agent": "a"}), &ids(&["m1"])).await;
    let mut valid = consolidate_body("k-feed", one("m1"));
    valid["require_complete_feed"] = json!(true);
    let (status, error) = rig.post(CONSOLIDATE, &valid).await;
    assert_eq!(
        (status, &error["error"]["code"]),
        (409, &json!("memory_feed_unsupported")),
        "{error}"
    );
    valid["require_complete_feed"] = json!(false);
    let (status, done) = rig.post(CONSOLIDATE, &valid).await;
    assert_eq!(status, 200, "{done}");
    assert_eq!(done["replayed"], false);
}

/// Serving section 3: a mutation targets one exact scope, with no parent,
/// child or sibling fallback; consolidation inputs must remain in it.
#[tokio::test]
async fn inputs_resolve_only_in_the_exact_requested_scope() {
    let rig = rig();
    let scopes = [
        json!({"agent": "a"}),
        json!({"agent": "a", "user": "u"}),
        json!({"agent": "a", "user": "u", "session": "s"}),
    ];
    let sources = ["m-agent", "m-user", "m-session"];
    for (scope, source) in scopes.iter().zip(sources) {
        rig.seed(scope, &ids(&[source])).await;
    }
    for (level, scope) in scopes.iter().enumerate() {
        for (owner, source) in sources.iter().enumerate() {
            let mut body = consolidate_n(level * 10 + owner, one(source));
            body["scope"] = scope.clone();
            let (status, result) = rig.post(CONSOLIDATE, &body).await;
            if level == owner {
                assert_eq!(status, 200, "{scope} {source}: {result}");
                let mut effective = scope.clone();
                effective["tenant"] = json!("@root");
                assert_eq!(result["receipt"]["scope"], effective);
            } else {
                assert_eq!(
                    (status, &result["error"]["code"]),
                    (404, &json!("memory_not_found")),
                    "{scope} {source}: {result}"
                );
            }
        }
    }
}

/// Serving section 6: Consolidate needs graph permissions and the evidence
/// collection's write grant; an evidence `read` or `search` grant is not
/// part of its requirement.
#[tokio::test]
async fn consolidate_needs_no_evidence_read_grant() {
    let rig = rig_with(
        database(),
        &bindings(SCOPES, false),
        &["write", "graph_read", "graph_write"],
    );
    rig.seed(&json!({"agent": "a"}), &ids(&["m1"])).await;
    let (status, done) = rig
        .post(CONSOLIDATE, &consolidate_body("k", one("m1")))
        .await;
    assert_eq!(status, 200, "{done}");
    assert_eq!(done["receipt"]["operation"], "consolidate");
}

/// Serving sections 4 and 8: `max_references` is an integer in 1..=256 and
/// `deadline_ms` an integer in 1..=5000, each named by `details.field`.
#[tokio::test]
async fn mutation_budgets_are_validated_on_their_own_fields() {
    let rig = rig();
    rig.seed(&json!({"agent": "a"}), &ids(&["m1"])).await;
    for value in [
        json!(0),
        json!(257),
        json!(-1),
        json!(1.5),
        json!("64"),
        json!(true),
        json!(u64::MAX),
    ] {
        let mut body = consolidate_n(0, one("m1"));
        body["max_references"] = value.clone();
        invalid(
            &rig.post(CONSOLIDATE, &body).await,
            "max_references",
            &value.to_string(),
        );
    }
    for value in [json!(0), json!(5001), json!("250"), json!(false)] {
        let mut body = consolidate_n(0, one("m1"));
        body["deadline_ms"] = value.clone();
        invalid(
            &rig.post(CONSOLIDATE, &body).await,
            "deadline_ms",
            &value.to_string(),
        );
    }
    // Both bounds are inclusive.
    for (n, references) in [(1, 1), (2, 256)] {
        let mut body = consolidate_n(n, one("m1"));
        body["max_references"] = json!(references);
        body["deadline_ms"] = json!(5000);
        let (status, done) = rig.post(CONSOLIDATE, &body).await;
        assert_eq!(status, 200, "max_references {references}: {done}");
    }
    // The refused requests wrote nothing: their key and summary are unused.
    let (status, done) = rig.post(CONSOLIDATE, &consolidate_n(0, one("m1"))).await;
    assert_eq!(status, 200, "{done}");
    assert_eq!(done["replayed"], false);
}

/// Serving sections 4 and 8 and decision 74: 1..=256 wire inputs, at most
/// `min(max_references, 64)` of them; every excess is 422, never a 500.
#[tokio::test]
async fn the_input_count_is_bounded_by_the_serving_budget() {
    let rig = rig();
    let names: Vec<String> = (0..64).map(|i| format!("s{i:03}")).collect();
    rig.seed(&json!({"agent": "a"}), &names).await;
    let inputs = |count: usize| {
        Value::Array(
            (0..count)
                .map(|i| json!({"id": format!("s{i:03}"), "revision": "1"}))
                .collect(),
        )
    };
    for (count, references) in [
        (0, None),
        (65, None),
        (65, Some(256)),
        (256, Some(256)),
        (257, Some(256)),
        (3, Some(2)),
        (2, Some(1)),
    ] {
        let mut body = consolidate_n(count, inputs(count));
        if let Some(references) = references {
            body["max_references"] = json!(references);
        }
        invalid(
            &rig.post(CONSOLIDATE, &body).await,
            "inputs",
            &format!("{count} inputs, max_references {references:?}"),
        );
    }
    let mut body = consolidate_n(1, json!("not an array"));
    invalid(&rig.post(CONSOLIDATE, &body).await, "inputs", "scalar");
    body["inputs"] = json!({"id": "s000", "revision": "1"});
    invalid(&rig.post(CONSOLIDATE, &body).await, "inputs", "object");

    // The inclusive boundaries are served: 64 by default, and the explicit
    // budget when it is lower.
    let (status, done) = rig.post(CONSOLIDATE, &consolidate_n(2, inputs(64))).await;
    assert_eq!(status, 200, "{done}");
    let mut body = consolidate_n(3, inputs(2));
    body["max_references"] = json!(2);
    let (status, done) = rig.post(CONSOLIDATE, &body).await;
    assert_eq!(status, 200, "{done}");
}

/// Serving section 3: JSON objects reject unknown keys at every depth, and
/// an input cannot select another scope.
#[tokio::test]
async fn inputs_are_closed_objects() {
    let rig = rig();
    rig.seed(&json!({"agent": "a"}), &ids(&["m1"])).await;
    let extras = [
        ("note", json!(1)),
        ("scope", json!({"agent": "a"})),
        ("scope", json!({"agent": "b"})),
        ("scope", json!({"agent": "a", "user": "u"})),
        ("tenant", json!("@root")),
    ];
    for (n, (name, extra)) in extras.into_iter().enumerate() {
        let mut input = json!({"id": "m1", "revision": "1"});
        input[name] = extra.clone();
        let (status, error) = rig
            .post(CONSOLIDATE, &consolidate_n(n, json!([input])))
            .await;
        assert_eq!(
            (status, &error["error"]["code"]),
            (422, &json!("invalid_input")),
            "{name} {extra}: {error}"
        );
    }
    // The same request without the extra key is served.
    let (status, done) = rig.post(CONSOLIDATE, &consolidate_n(9, one("m1"))).await;
    assert_eq!(status, 200, "{done}");
}

/// Serving section 4 and C1 section 1: input IDs form a set; duplicates
/// reject and are never collapsed, whatever their revisions.
#[tokio::test]
async fn duplicate_input_ids_are_rejected_not_collapsed() {
    let rig = rig();
    rig.seed(&json!({"agent": "a"}), &ids(&["m1", "m2"])).await;
    let duplicates = [
        json!([{"id": "m1", "revision": "1"}, {"id": "m1", "revision": "1"}]),
        json!([{"id": "m1", "revision": "1"}, {"id": "m2", "revision": "1"},
            {"id": "m1", "revision": "1"}]),
        json!([{"id": "m1", "revision": "1"}, {"id": "m1", "revision": "2"}]),
    ];
    for (n, inputs) in duplicates.into_iter().enumerate() {
        let (status, error) = rig
            .post(CONSOLIDATE, &consolidate_n(n, inputs.clone()))
            .await;
        assert_eq!(
            (status, &error["error"]["code"]),
            (422, &json!("invalid_input")),
            "{inputs}: {error}"
        );
    }
    // Nothing was written: the first key and summary are still unused.
    let inputs = json!([{"id": "m1", "revision": "1"}, {"id": "m2", "revision": "1"}]);
    let (status, done) = rig.post(CONSOLIDATE, &consolidate_n(0, inputs)).await;
    assert_eq!(status, 200, "{done}");
    assert_eq!(done["replayed"], false);
}

/// Serving section 4: commit rechecks the whole retained input set, so a
/// stale revision on any input (first, middle or last) is 412.
#[tokio::test]
async fn every_input_revision_is_rechecked() {
    let rig = rig();
    rig.seed(&json!({"agent": "a"}), &ids(&["m1", "m2", "m3"]))
        .await;
    let stale = [
        json!([{"id": "m1", "revision": "2"}, {"id": "m2", "revision": "1"}]),
        json!([{"id": "m1", "revision": "1"}, {"id": "m2", "revision": "2"}]),
        json!([{"id": "m1", "revision": "1"}, {"id": "m2", "revision": "1"},
            {"id": "m3", "revision": "2"}]),
        json!([{"id": "m3", "revision": "1"}, {"id": "m1", "revision": "1"},
            {"id": "m2", "revision": "7"}]),
    ];
    for (n, inputs) in stale.into_iter().enumerate() {
        let (status, error) = rig
            .post(CONSOLIDATE, &consolidate_n(n, inputs.clone()))
            .await;
        assert_eq!(
            (status, &error["error"]["code"]),
            (412, &json!("precondition_failed")),
            "{inputs}: {error}"
        );
    }
    // Nothing was written: the same keys serve the correct revisions.
    let inputs = json!([{"id": "m1", "revision": "1"}, {"id": "m2", "revision": "1"},
        {"id": "m3", "revision": "1"}]);
    let (status, done) = rig.post(CONSOLIDATE, &consolidate_n(0, inputs)).await;
    assert_eq!(status, 200, "{done}");
    assert_eq!(done["replayed"], false);
}

fn provenance(entries: &[(&str, Option<&str>)]) -> Value {
    Value::Array(
        entries
            .iter()
            .map(|(source, uri)| {
                let mut entry = json!({"source_id": source, "source_kind": "caller"});
                if let Some(uri) = uri {
                    entry["uri"] = json!(uri);
                }
                entry
            })
            .collect(),
    )
}

type Change = fn(&mut Value);

/// `(field, baseline, change)`: the output item with `baseline` applied,
/// then with only `change` on top of it.
fn forwarded_fields() -> Vec<(&'static str, Change, Change)> {
    vec![
        (
            "content",
            |_| {},
            |output| output["content"] = json!("another summary text"),
        ),
        ("summary id", |_| {}, |output| output["id"] = json!("sum2")),
        (
            "summary source",
            |_| {},
            |output| output["source"]["source_id"] = json!("s3"),
        ),
        (
            "observed_at_ms",
            |_| {},
            |output| output["observed_at_ms"] = json!("91"),
        ),
        (
            "importance",
            |output| output["importance"] = json!(0.9),
            |output| output["importance"] = json!(0.8),
        ),
        (
            "expires_at_ms",
            |output| output["expires_at_ms"] = json!("9000000000000"),
            |output| output["expires_at_ms"] = json!("9000000000001"),
        ),
        // The default provenance already carries the transformation, so an
        // explicit one isolates the transformation passed to the core.
        (
            "transformation_id",
            |output| output["provenance"] = provenance(&[("p1", None)]),
            |output| output["source"]["transformation_id"] = json!("t2"),
        ),
        (
            "provenance entry added",
            |output| output["provenance"] = provenance(&[("p1", None)]),
            |output| output["provenance"] = provenance(&[("p1", None), ("p2", None)]),
        ),
        (
            "provenance order",
            |output| output["provenance"] = provenance(&[("p1", None), ("p2", None)]),
            |output| output["provenance"] = provenance(&[("p2", None), ("p1", None)]),
        ),
        (
            "provenance uri",
            |output| output["provenance"] = provenance(&[("p1", None)]),
            |output| {
                output["provenance"] = provenance(&[("p1", Some("https://example.invalid/a"))])
            },
        ),
    ]
}

/// Serving section 3 and C1 section 3: the core digest covers the summary
/// ID, content, source, observation, importance, expiry, ordered provenance
/// and transformation, so changing any one of them under a retained key is a
/// 409 `idempotency_conflict` (never a 412 or a silent replay), and the
/// retained request still replays unchanged.
#[tokio::test]
async fn a_changed_request_under_a_retained_key_is_an_idempotency_conflict() {
    for (field, baseline, change) in forwarded_fields() {
        let rig = rig();
        rig.seed(&json!({"agent": "a"}), &ids(&["m1"])).await;
        let mut original = consolidate_body("k", one("m1"));
        baseline(&mut original["output"]);
        let (status, first) = rig.post(CONSOLIDATE, &original).await;
        assert_eq!(status, 200, "{field}: {first}");
        assert_eq!(first["replayed"], false, "{field}");

        let mut changed = original.clone();
        change(&mut changed["output"]);
        assert_ne!(changed, original, "{field}: the row must change something");
        let (status, error) = rig.post(CONSOLIDATE, &changed).await;
        assert_eq!(
            (status, &error["error"]["code"]),
            (409, &json!("idempotency_conflict")),
            "{field}: {error}"
        );

        let (status, again) = rig.post(CONSOLIDATE, &original).await;
        assert_eq!(status, 200, "{field}: {again}");
        assert_eq!(again["replayed"], true, "{field}");
        assert_eq!(again["receipt"], first["receipt"], "{field}");
    }
}
