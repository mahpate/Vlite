//! Per-scope read grants on the memory routes: a grant's `read_scopes` are
//! accepted by `recall` and `facts/query` only; every other route accepts
//! `scopes` alone, and refuses a read scope with the single 403 body of an
//! ungranted scope or an unknown binding. One service is served through two
//! routers, a session credential and a publisher credential, sharing one
//! `Clock`. Expectations follow the route matrix of the design, never the
//! handlers.

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
use crate::memory::binding::parse as parse_bindings;
use crate::{CollectionSettings, CollectionSpec, CreateOptions, Database, FormatVersion};

const SESSION: &str = "abcdefghij27";
const PUBLISHER: &str = "bcdefghij234";
const FORBIDDEN: &str = "forbidden_scope";

/// The nine memory routes.
const ROUTES: [&str; 9] = [
    "remember",
    "recall",
    "consolidate",
    "forget",
    "receipts/lookup",
    "facts/query",
    "facts/correct",
    "erasures/get",
    "erasures/resume",
];
/// The routes that accept only `scopes`.
const WRITE_ONLY: [&str; 7] = [
    "remember",
    "consolidate",
    "forget",
    "receipts/lookup",
    "facts/correct",
    "erasures/get",
    "erasures/resume",
];

fn personal() -> Value {
    json!({"agent": "assistant", "user": "ada"})
}

fn shared() -> Value {
    json!({"agent": "assistant"})
}

fn archive() -> Value {
    json!({"agent": "archive"})
}

fn ungranted() -> Value {
    json!({"agent": "nobody"})
}

fn bindings() -> Value {
    json!({"version": "memory-bindings-v1", "bindings": [{
        "binding": "notes",
        "tenant": "@root",
        "generation": 1,
        "state": "active",
        "graph": "notes_graph",
        "evidence_collection": "notes_ev",
        "schema_version": "memory-schema-v1",
        "ranking_version": "memory-ranking-v1",
        "digest_version": "memory-request-v1",
        "receipt_window_ms": 60_000,
        "grants": [
            {"principal": SESSION, "scopes": [personal()],
                "read_scopes": [shared(), archive()]},
            {"principal": PUBLISHER, "scopes": [shared()]},
        ],
    }]})
}

/// The real memory routes over one service, once per credential.
struct Rig {
    session: Router,
    publisher: Router,
    service: Service,
}

fn router(service: &Service, clock: &Clock, principal: &str, lineage: u64) -> Router {
    let grant = credentials::Grant::for_user(
        principal.into(),
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
    super::routes::routes()
        .with_state(service.clone())
        .layer(Extension(admin::Role::Scoped(Arc::new(grant))))
        .layer(Extension(RequestId("pins".into())))
        .layer(Extension(clock.clone()))
        .layer(DefaultBodyLimit::max(MAX_JSON_BYTES))
}

impl Rig {
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
        let set = parse_bindings(bindings().to_string().as_bytes()).expect("valid bindings");
        let service = Service::new(db).with_bindings(Bindings::new(set));
        super::startup_at(&service, 0).expect("startup");
        let clock = Clock::default();
        clock.now_ms.store(1_000, Ordering::SeqCst);
        Self {
            session: router(&service, &clock, SESSION, lineage),
            publisher: router(&service, &clock, PUBLISHER, lineage),
            service,
        }
    }

    /// The database's last commit sequence.
    fn sequence(&self) -> u64 {
        self.service
            .database
            .read()
            .unwrap()
            .as_ref()
            .unwrap()
            .sequence
    }
}

/// The status, the headers and the exact body bytes, as a client sees them.
type Wire = (u16, Vec<(String, Vec<u8>)>, Vec<u8>);

async fn send(router: &Router, path: &str, body: &Value) -> Wire {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let mut headers: Vec<(String, Vec<u8>)> = response
        .headers()
        .iter()
        .map(|(name, value)| (name.as_str().to_owned(), value.as_bytes().to_vec()))
        .collect();
    headers.sort();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, bytes.to_vec())
}

async fn post(router: &Router, route: &str, body: &Value) -> (u16, Value) {
    let (status, _, bytes) = send(router, &path("notes", route), body).await;
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("not JSON ({error}): {bytes:?}"));
    (status, value)
}

fn path(binding: &str, route: &str) -> String {
    format!("/v1/memory/{binding}/{route}")
}

fn item(id: &str) -> Value {
    json!({"id": id, "kind": "entity", "content": format!("text of {id}"),
        "observed_at_ms": "90", "source": {"source_id": "s", "source_kind": "caller"}})
}

/// A request for `route` in `scope` that is valid apart from what the route
/// may refuse after authorization (missing memories, receipts or features).
fn request(route: &str, scope: &Value, key: &str) -> Value {
    match route {
        "remember" => json!({"scope": scope, "idempotency_key": key, "items": [item(key)]}),
        "recall" => json!({"scope": scope, "text": "memory"}),
        "consolidate" => json!({"scope": scope, "idempotency_key": key,
            "inputs": [{"id": "never", "revision": "1"}],
            "output": {"id": format!("sum-{key}"), "kind": "episodic",
                "content": "summary text", "observed_at_ms": "90",
                "source": {"source_id": "s", "source_kind": "caller",
                    "transformation_id": "t"}}}),
        "forget" => json!({"scope": scope, "idempotency_key": key,
            "target": {"ids": [{"id": "never", "revision": "1"}]}, "mode": "logical_delete"}),
        "receipts/lookup" => {
            json!({"scope": scope, "operation": "forget", "idempotency_key": key})
        }
        _ => json!({"scope": scope}),
    }
}

async fn remember(router: &Router, scope: &Value, key: &str, id: &str) -> (u16, Value) {
    let body = json!({"scope": scope, "idempotency_key": key, "items": [item(id)]});
    post(router, "remember", &body).await
}

fn assert_forbidden(status: u16, answer: &Value, case: &str) {
    assert_eq!(status, 403, "{case}: {answer}");
    assert_eq!(answer["error"]["code"], FORBIDDEN, "{case}: {answer}");
}

/// What an authorized request for `route` answers: the route's own outcome
/// for [`request`], never the 403 of an unauthorized one.
fn allowed(route: &str) -> (u16, Option<&'static str>) {
    match route {
        "remember" | "recall" => (200, None),
        "consolidate" | "forget" => (404, Some("memory_not_found")),
        "receipts/lookup" => (404, Some("receipt_not_found")),
        _ => (409, Some("memory_capability_unavailable")),
    }
}

fn assert_allowed(route: &str, status: u16, answer: &Value, case: &str) {
    let (expected, code) = allowed(route);
    assert_eq!(status, expected, "{case}: {answer}");
    assert_eq!(answer["error"]["code"].as_str(), code, "{case}: {answer}");
}

fn ids(answer: &Value) -> Vec<String> {
    let mut ids: Vec<String> = answer["memories"]
        .as_array()
        .unwrap_or_else(|| panic!("no memories: {answer}"))
        .iter()
        .map(|memory| memory["id"].as_str().unwrap().to_owned())
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn each_route_checks_the_right_scope_list() {
    let rig = Rig::new();
    for (n, route) in ROUTES.into_iter().enumerate() {
        let reads = matches!(route, "recall" | "facts/query");
        // The session: its personal write scope everywhere, its read scopes
        // on the two read routes only, and nothing else.
        let key = format!("s{n}");
        let (status, answer) = post(&rig.session, route, &request(route, &personal(), &key)).await;
        assert_allowed(route, status, &answer, &format!("session personal {route}"));
        for scope in [shared(), archive()] {
            let case = format!("session read {scope} {route}");
            let (status, answer) = post(&rig.session, route, &request(route, &scope, &key)).await;
            if reads {
                assert_allowed(route, status, &answer, &case);
            } else {
                assert_forbidden(status, &answer, &case);
            }
        }
        let (status, answer) = post(&rig.session, route, &request(route, &ungranted(), &key)).await;
        assert_forbidden(status, &answer, &format!("session ungranted {route}"));
        // The publisher: the shared scope everywhere, none of the session's.
        let key = format!("p{n}");
        let (status, answer) = post(&rig.publisher, route, &request(route, &shared(), &key)).await;
        assert_allowed(route, status, &answer, &format!("publisher shared {route}"));
        for scope in [personal(), archive()] {
            let (status, answer) = post(&rig.publisher, route, &request(route, &scope, &key)).await;
            assert_forbidden(status, &answer, &format!("publisher {scope} {route}"));
        }
    }
    // A recall naming a read scope among its scopes is a read of each.
    let body = json!({"scope": personal(), "scopes": [personal(), shared(), archive()],
        "text": "memory"});
    let (status, answer) = post(&rig.session, "recall", &body).await;
    assert_eq!(status, 200, "{answer}");
    let body = json!({"scope": personal(), "scopes": [personal(), ungranted()],
        "text": "memory"});
    let (status, answer) = post(&rig.session, "recall", &body).await;
    assert_forbidden(status, &answer, "recall with an ungranted scope");
}

#[tokio::test]
async fn a_read_scope_refusal_is_byte_identical_to_an_ungranted_scope_and_an_unknown_binding() {
    let rig = Rig::new();
    let before = rig.sequence();
    for route in WRITE_ONLY {
        let read = request(route, &shared(), "k");
        let refused = send(&rig.session, &path("notes", route), &read).await;
        assert_eq!(refused.0, 403, "{route}");
        let other = send(
            &rig.session,
            &path("notes", route),
            &request(route, &ungranted(), "k"),
        )
        .await;
        assert_eq!(refused, other, "{route}: ungranted scope");
        let unknown = send(&rig.session, &path("nope", route), &read).await;
        assert_eq!(refused, unknown, "{route}: unknown binding");
        let (_, answer) = post(&rig.session, route, &read).await;
        assert_forbidden(403, &answer, route);
    }
    assert_eq!(rig.sequence(), before, "a refusal writes nothing");
}

#[tokio::test]
async fn a_provisioned_read_scope_nobody_wrote_recalls_empty() {
    let rig = Rig::new();
    for body in [
        json!({"scope": archive(), "text": "memory"}),
        json!({"scope": archive(), "scopes": [archive(), shared()], "text": "memory"}),
    ] {
        let (status, answer) = post(&rig.session, "recall", &body).await;
        assert_eq!(status, 200, "{answer}");
        assert_eq!(answer["memories"], json!([]), "{answer}");
    }
}

#[tokio::test]
async fn a_publisher_writes_shared_memory_and_a_session_recalls_it_with_its_own() {
    let rig = Rig::new();
    let (status, answer) = remember(&rig.publisher, &shared(), "pub", "shared1").await;
    assert_eq!(status, 200, "{answer}");
    let (status, answer) = remember(&rig.session, &personal(), "own", "mine1").await;
    assert_eq!(status, 200, "{answer}");
    let before = rig.sequence();
    let (status, answer) = remember(&rig.session, &shared(), "steal", "mine2").await;
    assert_forbidden(status, &answer, "session remember into the shared scope");
    assert_eq!(rig.sequence(), before);
    // One recall naming both scopes, seeded in each.
    for (scope, other, seed) in [
        (personal(), shared(), "mine1"),
        (shared(), personal(), "shared1"),
    ] {
        let body = json!({"scope": scope, "scopes": [scope, other], "entity_seeds": [seed]});
        let (status, answer) = post(&rig.session, "recall", &body).await;
        assert_eq!(status, 200, "{answer}");
        assert_eq!(ids(&answer), [seed], "{answer}");
    }
    // The publisher cannot read the session's personal memory.
    let body = json!({"scope": personal(), "entity_seeds": ["mine1"]});
    let (status, answer) = post(&rig.publisher, "recall", &body).await;
    assert_forbidden(status, &answer, "publisher recall of the personal scope");
}

#[tokio::test]
async fn facts_query_on_a_read_scope_answers_like_a_writer() {
    let rig = Rig::new();
    let body = json!({"scope": shared()});
    let reader = send(&rig.session, &path("notes", "facts/query"), &body).await;
    let writer = send(&rig.publisher, &path("notes", "facts/query"), &body).await;
    assert_eq!(reader, writer);
    let (status, answer) = post(&rig.session, "facts/query", &body).await;
    assert_eq!(status, 409, "{answer}");
    assert_eq!(answer["error"]["code"], "memory_capability_unavailable");
    assert_eq!(answer["error"]["details"], json!({"feature": "facts"}));
}

#[tokio::test]
async fn a_reader_cannot_look_up_or_replay_a_publishers_receipt() {
    let rig = Rig::new();
    let (status, written) = remember(&rig.publisher, &shared(), "pk", "shared1").await;
    assert_eq!(status, 200, "{written}");
    // Remember receipts are not looked up in this version, so the
    // publisher's forget receipt is the one a reader could try to read.
    let forget = json!({"scope": shared(), "idempotency_key": "fk",
        "target": {"ids": [{"id": "shared1", "revision": "1"}]}, "mode": "logical_delete"});
    let (status, answer) = post(&rig.publisher, "forget", &forget).await;
    assert_eq!(status, 200, "{answer}");
    let lookup = json!({"scope": shared(), "operation": "forget", "idempotency_key": "fk"});
    let (status, answer) = post(&rig.publisher, "receipts/lookup", &lookup).await;
    assert_eq!(status, 200, "{answer}");
    let before = rig.sequence();
    let refused = send(&rig.session, &path("notes", "receipts/lookup"), &lookup).await;
    let mut ungranted_lookup = lookup.clone();
    ungranted_lookup["scope"] = ungranted();
    let other = send(
        &rig.session,
        &path("notes", "receipts/lookup"),
        &ungranted_lookup,
    )
    .await;
    assert_eq!(refused.0, 403);
    assert_eq!(refused, other);
    // A replay of the publisher's exact request is refused before the
    // receipt is read.
    let replay = json!({"scope": shared(), "idempotency_key": "pk", "items": [item("shared1")]});
    let (status, answer) = post(&rig.session, "remember", &replay).await;
    assert_forbidden(status, &answer, "replay");
    assert_eq!(rig.sequence(), before);
    let (status, again) = post(&rig.publisher, "remember", &replay).await;
    let mut expected = written.clone();
    expected["replayed"] = json!(true);
    assert_eq!((status, &again), (200, &expected));
}

#[tokio::test]
async fn naming_a_shared_memory_from_a_personal_scope_answers_like_a_missing_one() {
    let rig = Rig::new();
    let (status, answer) = remember(&rig.publisher, &shared(), "pk", "shared1").await;
    assert_eq!(status, 200, "{answer}");
    for route in ["consolidate", "forget"] {
        let mut answers = Vec::new();
        for (n, id) in ["shared1", "never"].into_iter().enumerate() {
            let mut body = request(route, &personal(), &format!("{route}-{n}"));
            let target = json!([{"id": id, "revision": "1"}]);
            if route == "consolidate" {
                body["inputs"] = target;
                body["output"]["id"] = json!("summary");
            } else {
                body["target"]["ids"] = target;
            }
            let before = rig.sequence();
            let (status, answer) = post(&rig.session, route, &body).await;
            assert!(status >= 400, "{route} {id}: {answer}");
            assert_eq!(rig.sequence(), before, "{route} {id}");
            answers.push((status, answer));
        }
        assert_eq!(answers[0], answers[1], "{route}");
    }
    let body = json!({"scope": shared(), "entity_seeds": ["shared1"]});
    let (status, answer) = post(&rig.publisher, "recall", &body).await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(ids(&answer), ["shared1"]);
}
