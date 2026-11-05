#![cfg(feature = "server")]
//! Forget (agent-memory REST) pins over the real credential, binding and
//! middleware stack. Every expectation is derived from the serving contract
//! (sections 3-6, 8), the adapter mapping (sections 1-2, 6) and the wire
//! fixtures, never from the adapter code: exact-scope authorization with no
//! parent, child or sibling fallback, one indistinguishable 403 body, the
//! read-only precheck before refusals, tenant isolation, receipt identity
//! (scope, key, request), strict JSON and integer types, and the explicit
//! target bounds. State is observed through Recall, never assumed.

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

const ADMIN: &str = "fake_memory_forget_admin_token_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const READ_ONLY: &str = "fake_memory_forget_readonly_token_0123456789_ABCDEFGHIJKLMNOP";
const TENANT: &str = "0123456789abcdef0123456789abcdef";
const OPS: [&str; 5] = ["read", "search", "write", "graph_read", "graph_write"];

/// A credential and its token: `vlk_{id}_{secret}`, the secret being 43
/// copies of `secret`.
struct Who {
    id: &'static str,
    secret: char,
    tenant: Option<&'static str>,
    operations: &'static [&'static str],
}

impl Who {
    fn token(&self) -> String {
        format!("vlk_{}_{}", self.id, self.secret.to_string().repeat(43))
    }
}

const PARENT: Who = Who {
    id: "abcdefghij27",
    secret: 'A',
    tenant: None,
    operations: &OPS,
};
const CHILD: Who = Who {
    id: "bcdefghij234",
    secret: 'B',
    tenant: None,
    operations: &OPS,
};
const BOTH: Who = Who {
    id: "cdefghij2345",
    secret: 'C',
    tenant: None,
    operations: &OPS,
};
const NONE: Who = Who {
    id: "defghij23456",
    secret: 'D',
    tenant: None,
    operations: &OPS,
};
const TENANTED: Who = Who {
    id: "efghij234567",
    secret: 'E',
    tenant: Some(TENANT),
    operations: &OPS,
};

fn a() -> Value {
    json!({"agent": "a"})
}

fn au() -> Value {
    json!({"agent": "a", "user": "u"})
}

fn aus() -> Value {
    json!({"agent": "a", "user": "u", "session": "s"})
}

fn b() -> Value {
    json!({"agent": "b"})
}

fn stored(tenant: &str, name: &str) -> String {
    if tenant == "@root" {
        name.to_owned()
    } else {
        format!("t{tenant}__{name}")
    }
}

/// An in-memory graph database holding `{binding}_graph` and `{binding}_ev`
/// for each `(tenant, binding)`, under their stored names.
fn database(bindings: &[(&str, &str)]) -> Database {
    let mut db = Database::create_in_memory(
        2,
        CreateOptions {
            format: Some(FormatVersion::V5),
            ..Default::default()
        },
    )
    .unwrap();
    for (tenant, binding) in bindings {
        db.create_collection(
            &stored(tenant, &format!("{binding}_ev")),
            CollectionSpec {
                dimensions: 2,
                settings: Default::default(),
            },
        )
        .unwrap();
        db.create_graph(&stored(tenant, &format!("{binding}_graph")))
            .unwrap();
    }
    db
}

fn binding(name: &str, tenant: &str, grants: Value) -> Value {
    json!({
        "binding": name, "tenant": tenant, "generation": 1, "state": "active",
        "graph": format!("{name}_graph"), "evidence_collection": format!("{name}_ev"),
        "schema_version": "memory-schema-v1", "ranking_version": "memory-ranking-v1",
        "digest_version": "memory-request-v1", "receipt_window_ms": 86_400_000,
        "grants": grants,
    })
}

fn grant(who: &Who, scopes: &[Value]) -> Value {
    json!({"principal": who.id, "scopes": scopes})
}

fn credential(who: &Who, lineage: u64, evidence: Option<u16>) -> Value {
    let secret = who.secret.to_string().repeat(43);
    let mut entry = json!({
        "id": who.id,
        "name": "fake forget credential",
        "operations": who.operations,
        "lineage": format!("{lineage:016x}"),
        "secret_sha256": format!("{:x}", Sha256::digest(secret.as_bytes())),
        "created_unix_ms": 0,
        "expires_unix_ms": 9_999_999_999_999u64,
        "revoked_unix_ms": null,
        "rotated_from": null,
    });
    match who.tenant {
        Some(tenant) => {
            entry["all"] = json!(true);
            entry["collections"] = json!([]);
            entry["graphs"] = json!(["*"]);
            entry["tenant_id"] = json!(tenant);
        }
        None => {
            entry["all"] = json!(false);
            entry["collections"] = json!([{"name": "notes_ev", "id": evidence.unwrap()}]);
            entry["graphs"] = json!(["notes_graph"]);
        }
    }
    entry
}

fn start(directory: &TempDir, db: Database, who: &[&Who], bindings: &[Value]) -> Router {
    let evidence = db
        .collections()
        .unwrap()
        .into_iter()
        .find(|info| info.name == "notes_ev")
        .map(|info| info.id);
    let lineage = db.lineage().unwrap();
    let credentials: Vec<Value> = who
        .iter()
        .map(|who| credential(who, lineage, evidence))
        .collect();
    std::fs::write(
        directory.path().join("credentials.json"),
        serde_json::to_vec(&json!({"schema": 1, "credentials": credentials})).unwrap(),
    )
    .unwrap();
    if who.iter().any(|who| who.tenant.is_some()) {
        std::fs::write(
            directory.path().join("tenants.json"),
            serde_json::to_vec(
                &json!([{"id": TENANT, "name": "test tenant", "max_vectors": null}]),
            )
            .unwrap(),
        )
        .unwrap();
    }
    let path = directory.path().join("memory-bindings.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({"version": "memory-bindings-v1", "bindings": bindings}))
            .unwrap(),
    )
    .unwrap();
    configured_admin_router(
        Service::new(db),
        ServerOptions {
            admin_token: Some(BearerToken::new(ADMIN).unwrap()),
            read_only_token: Some(BearerToken::new(READ_ONLY).unwrap()),
            admin_directory: Some(directory.path().into()),
            memory_bindings_file: Some(path),
            ..Default::default()
        },
    )
    .unwrap()
}

async fn send(app: &Router, path: &str, token: Option<&str>, raw: Vec<u8>) -> (u16, Value) {
    let mut request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(raw)).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn post(app: &Router, path: &str, who: &Who, body: &Value) -> (u16, Value) {
    send(
        app,
        path,
        Some(&who.token()),
        serde_json::to_vec(body).unwrap(),
    )
    .await
}

fn route(binding: &str, operation: &str) -> String {
    format!("/v1/memory/{binding}/{operation}")
}

async fn remember(app: &Router, who: &Who, binding: &str, scope: &Value, id: &str) {
    let body = json!({
        "scope": scope,
        "idempotency_key": format!("seed-{id}"),
        "deadline_ms": 5000,
        "items": [{
            "id": id, "kind": "entity", "content": format!("content of {id}"),
            "observed_at_ms": "90",
            "source": {"source_id": "s1", "source_kind": "caller"},
        }],
    });
    let (status, response) = post(app, &route(binding, "remember"), who, &body).await;
    assert_eq!(status, 200, "{response}");
    assert_eq!(response["outcomes"][0]["action"], "created", "{response}");
}

/// The status of Recall with `id` as the only entity seed: 200 when the
/// memory is live in exactly `scope`, 404 when it is not.
async fn recalled(app: &Router, who: &Who, binding: &str, scope: &Value, id: &str) -> u16 {
    let body = json!({
        "scope": scope, "entity_seeds": [id], "limits": {"deadline_ms": 5000},
    });
    post(app, &route(binding, "recall"), who, &body).await.0
}

fn forget_body(scope: &Value, key: &str, targets: &[(&str, u64)]) -> Value {
    let ids: Vec<Value> = targets
        .iter()
        .map(|(id, revision)| json!({"id": id, "revision": revision.to_string()}))
        .collect();
    json!({
        "scope": scope, "idempotency_key": key, "deadline_ms": 5000,
        "target": {"ids": ids}, "mode": "logical_delete",
    })
}

async fn forget(
    app: &Router,
    who: &Who,
    binding: &str,
    scope: &Value,
    key: &str,
    targets: &[(&str, u64)],
) -> (u16, Value) {
    let body = forget_body(scope, key, targets);
    post(app, &route(binding, "forget"), who, &body).await
}

fn assert_error(result: &(u16, Value), status: u16, code: &str) {
    assert_eq!(
        (result.0, &result.1["error"]["code"]),
        (status, &json!(code)),
        "{}",
        result.1
    );
}

/// The refusal body of every binding or scope authorization failure.
fn assert_forbidden(result: &(u16, Value)) {
    assert_error(result, 403, "forbidden_scope");
}

const NO_GRAPH_WRITE: [&str; 3] = ["read", "write", "graph_read"];
const NO_EVIDENCE_WRITE: [&str; 3] = ["read", "graph_read", "graph_write"];
const GRAPH_READ_ONLY: [&str; 1] = ["graph_read"];

#[tokio::test]
async fn forget_is_a_graph_write_with_its_own_evidence_write_grant() {
    let body = forget_body(&a(), "k", &[("m1", 1)]);
    let path = route("notes", "forget");
    let weak: [&'static [&'static str]; 3] =
        [&NO_GRAPH_WRITE, &NO_EVIDENCE_WRITE, &GRAPH_READ_ONLY];
    for operations in weak {
        let directory = TempDir::new().unwrap();
        let who = Who {
            operations,
            ..PARENT
        };
        let app = start(
            &directory,
            database(&[("@root", "notes")]),
            &[&who],
            &[binding("notes", "@root", json!([grant(&who, &[a()])]))],
        );
        let result = post(&app, &path, &who, &body).await;
        assert_forbidden(&result);
    }
    let directory = TempDir::new().unwrap();
    let app = start(
        &directory,
        database(&[("@root", "notes")]),
        &[&PARENT],
        &[binding("notes", "@root", json!([grant(&PARENT, &[a()])]))],
    );
    let raw = serde_json::to_vec(&body).unwrap();
    assert_eq!(send(&app, &path, None, raw.clone()).await.0, 401);
    let result = send(&app, &path, Some(READ_ONLY), raw.clone()).await;
    assert_error(&result, 403, "read_only");
    // An administrator holds no memory grant and never synthesizes one.
    let result = send(&app, &path, Some(ADMIN), raw).await;
    assert_forbidden(&result);
}

#[tokio::test]
async fn forget_reaches_only_the_exactly_granted_scope() {
    let directory = TempDir::new().unwrap();
    let app = start(
        &directory,
        database(&[("@root", "notes")]),
        &[&PARENT, &CHILD],
        &[binding(
            "notes",
            "@root",
            json!([grant(&PARENT, &[a()]), grant(&CHILD, &[au(), aus(), b()])]),
        )],
    );
    // The same ID at the same revision 1 lives in four provisioned scopes.
    remember(&app, &PARENT, "notes", &a(), "m1").await;
    for scope in [au(), aus(), b()] {
        remember(&app, &CHILD, "notes", &scope, "m1").await;
    }
    // A parent grant reaches no child (user, session) and no sibling agent.
    for scope in [au(), aus(), b()] {
        let result = forget(&app, &PARENT, "notes", &scope, "k", &[("m1", 1)]).await;
        assert_forbidden(&result);
    }
    // A child grant reaches no parent either.
    let result = forget(&app, &CHILD, "notes", &a(), "k", &[("m1", 1)]).await;
    assert_forbidden(&result);
    // A sibling user that no grant names, provisioned or not.
    let sibling = json!({"agent": "a", "user": "x"});
    let result = forget(&app, &CHILD, "notes", &sibling, "k", &[("m1", 1)]).await;
    assert_forbidden(&result);
    for (who, scope) in [
        (&PARENT, a()),
        (&CHILD, au()),
        (&CHILD, aus()),
        (&CHILD, b()),
    ] {
        assert_eq!(
            recalled(&app, who, "notes", &scope, "m1").await,
            200,
            "{scope}"
        );
    }
    // The exact grant deletes in its own scope only.
    let (status, body) = forget(&app, &PARENT, "notes", &a(), "k", &[("m1", 1)]).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["outcomes"][0]["id"], "m1");
    assert_eq!(recalled(&app, &PARENT, "notes", &a(), "m1").await, 404);
    for (who, scope) in [(&CHILD, au()), (&CHILD, aus()), (&CHILD, b())] {
        assert_eq!(
            recalled(&app, who, "notes", &scope, "m1").await,
            200,
            "{scope}"
        );
    }
}

#[tokio::test]
async fn every_binding_and_scope_refusal_is_the_same_403_body() {
    let directory = TempDir::new().unwrap();
    let app = start(
        &directory,
        database(&[
            ("@root", "notes"),
            (TENANT, "notes"),
            (TENANT, "tenantonly"),
        ]),
        &[&PARENT, &NONE, &TENANTED],
        &[
            binding(
                "notes",
                "@root",
                json!([grant(&PARENT, &[a()]), grant(&TENANTED, &[a()])]),
            ),
            binding("notes", TENANT, json!([grant(&TENANTED, &[a()])])),
            binding("tenantonly", TENANT, json!([grant(&TENANTED, &[a()])])),
        ],
    );
    let valid = forget_body(&a(), "k", &[("m1", 1)]);
    let ungranted = forget_body(&b(), "k", &[("m1", 1)]);
    let refusals = [
        // An unknown binding.
        post(&app, &route("absent", "forget"), &PARENT, &valid).await,
        // A binding that exists only in another tenant, and the reverse.
        post(&app, &route("tenantonly", "forget"), &PARENT, &valid).await,
        post(&app, &route("absent", "forget"), &TENANTED, &valid).await,
        // A binding without any grant for this credential.
        post(&app, &route("notes", "forget"), &NONE, &valid).await,
        // A granted credential on a scope it was not granted.
        post(&app, &route("notes", "forget"), &PARENT, &ungranted).await,
        post(&app, &route("notes", "forget"), &TENANTED, &ungranted).await,
    ];
    for refusal in &refusals {
        assert_forbidden(refusal);
        assert_eq!(
            refusal, &refusals[0],
            "bodies must not tell the cases apart"
        );
    }
}

#[tokio::test]
async fn a_read_only_node_refuses_forget_before_any_refusal_or_validation() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("memory.vlite");
    let mut db = Database::create_with_options(
        &path,
        2,
        CreateOptions {
            format: Some(FormatVersion::V5),
            ..Default::default()
        },
    )
    .unwrap();
    db.create_collection(
        "notes_ev",
        CollectionSpec {
            dimensions: 2,
            settings: Default::default(),
        },
    )
    .unwrap();
    db.create_graph("notes_graph").unwrap();
    db.close().unwrap();
    let db = Database::open_read_only(&path).unwrap();
    let app = start(
        &directory,
        db,
        &[&PARENT],
        &[binding("notes", "@root", json!([grant(&PARENT, &[a()])]))],
    );
    let path = route("notes", "forget");
    let valid = forget_body(&a(), "k", &[("m1", 1)]);
    let mut series = valid.clone();
    series["target"] = json!({"series": {"id": "f", "revision": "1"}});
    let mut expire = valid.clone();
    expire["mode"] = json!("expire");
    let mut job = valid.clone();
    job["allow_job"] = json!(true);
    let mut feed = valid.clone();
    feed["require_complete_feed"] = json!(true);
    let malformed = json!({"scope": {"agent": "a"}});
    for body in [valid, series, expire, job, feed, malformed] {
        let result = post(&app, &path, &PARENT, &body).await;
        assert_error(&result, 403, "read_only");
    }
    // Authorization of the exact scope still comes first.
    let result = post(&app, &path, &PARENT, &forget_body(&b(), "k", &[("m1", 1)])).await;
    assert_forbidden(&result);
}

#[tokio::test]
async fn a_tenant_credential_resolves_only_bindings_of_its_own_tenant() {
    let directory = TempDir::new().unwrap();
    // The tenant credential is also named in the root binding of the same
    // name, so only tenant resolution keeps the two apart.
    let app = start(
        &directory,
        database(&[("@root", "notes"), (TENANT, "notes")]),
        &[&PARENT, &TENANTED],
        &[
            binding(
                "notes",
                "@root",
                json!([grant(&PARENT, &[a()]), grant(&TENANTED, &[a()])]),
            ),
            binding("notes", TENANT, json!([grant(&TENANTED, &[a()])])),
        ],
    );
    remember(&app, &PARENT, "notes", &a(), "root-only").await;
    remember(&app, &PARENT, "notes", &a(), "shared").await;
    remember(&app, &TENANTED, "notes", &a(), "shared").await;
    // An ID that only the root binding holds is absent in the tenant's.
    let result = forget(&app, &TENANTED, "notes", &a(), "k0", &[("root-only", 1)]).await;
    assert_error(&result, 404, "memory_not_found");
    assert_eq!(
        recalled(&app, &PARENT, "notes", &a(), "root-only").await,
        200
    );
    // The tenant's own binding is served, under the tenant's effective scope.
    let (status, body) = forget(&app, &TENANTED, "notes", &a(), "k1", &[("shared", 1)]).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["receipt"]["scope"],
        json!({"tenant": TENANT, "agent": "a"})
    );
    assert_eq!(
        recalled(&app, &TENANTED, "notes", &a(), "shared").await,
        404
    );
    // The root memory of the same ID, binding name and scope is untouched,
    // and the root credential resolves the root binding.
    assert_eq!(recalled(&app, &PARENT, "notes", &a(), "shared").await, 200);
    let (status, body) = forget(&app, &PARENT, "notes", &a(), "k1", &[("shared", 1)]).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["receipt"]["scope"],
        json!({"tenant": "@root", "agent": "a"})
    );
    assert_eq!(body["replayed"], false);
}

#[tokio::test]
async fn a_forget_identity_is_exact_in_key_scope_and_request() {
    let directory = TempDir::new().unwrap();
    let app = start(
        &directory,
        database(&[("@root", "notes")]),
        &[&BOTH],
        &[binding(
            "notes",
            "@root",
            json!([grant(&BOTH, &[a(), au()])]),
        )],
    );
    for id in ["p1", "p2", "p3", "p4", "s1"] {
        remember(&app, &BOTH, "notes", &a(), id).await;
    }
    remember(&app, &BOTH, "notes", &au(), "s1").await;

    // Opaque keys are never case folded: `k` and `K` are two identities.
    let (status, lower) = forget(&app, &BOTH, "notes", &a(), "k", &[("p1", 1)]).await;
    assert_eq!(status, 200, "{lower}");
    assert_eq!(lower["replayed"], false);
    // The exact first-slice response: sorted `deleted` outcomes at the receipt
    // revision, the logical state and the five-field notice without manifest.
    let sequence = lower["receipt"]["sequence"].clone();
    assert!(
        sequence
            .as_str()
            .is_some_and(|text| text.parse::<u64>().is_ok_and(|n| n.to_string() == text))
    );
    assert_eq!(
        lower["outcomes"],
        json!([{"id": "p1", "action": "deleted", "revision": sequence}])
    );
    assert_eq!(lower["receipt"]["operation"], "forget");
    assert_eq!(lower["state"], "logical");
    assert_eq!(lower["remaining_work"], "0");
    assert_eq!(
        lower["feed"],
        json!({"mode": "ordinary_changes", "temporal_complete": false,
               "ttl_complete": false, "consumer_purge_acknowledged": false})
    );
    assert_eq!(
        lower["erasure_notice"],
        json!({"live_state_only": true, "secure_erasure": false,
               "external_copies": "operator_managed", "restore_requires_manifest": true,
               "backup_history_purge": "not_verified"})
    );
    let (status, upper) = forget(&app, &BOTH, "notes", &a(), "K", &[("p2", 1)]).await;
    assert_eq!(status, 200, "{upper}");
    assert_eq!(upper["replayed"], false);
    assert_eq!(upper["outcomes"][0]["id"], "p2");
    assert_ne!(upper["receipt"]["id"], lower["receipt"]["id"]);
    // The operation is part of the identity: Remember already used the key
    // `seed-p4` for this ID in this scope, and Forget still executes.
    let (status, other) = forget(&app, &BOTH, "notes", &a(), "seed-p4", &[("p4", 1)]).await;
    assert_eq!(status, 200, "{other}");
    assert_eq!(other["replayed"], false);
    assert_eq!(other["outcomes"][0]["id"], "p4");

    // A retained matching retry returns the original result, with
    // `replayed:true` only in the envelope; an exact JSON integer revision is
    // the same request as its decimal string.
    let mut by_integer = forget_body(&a(), "k", &[("p1", 1)]);
    by_integer["target"]["ids"][0]["revision"] = json!(1);
    let (status, again) = post(&app, &route("notes", "forget"), &BOTH, &by_integer).await;
    assert_eq!(status, 200, "{again}");
    let mut original = lower.clone();
    original["replayed"] = json!(true);
    assert_eq!(again, original);

    // The same retained identity with another request is a 409 conflict, never
    // a 412 or a 404, and it deletes nothing.
    for targets in [vec![("p3", 1)], vec![("p1", 2)], vec![("p1", 1), ("p3", 1)]] {
        let result = forget(&app, &BOTH, "notes", &a(), "k", &targets).await;
        assert_error(&result, 409, "idempotency_conflict");
    }
    assert_eq!(recalled(&app, &BOTH, "notes", &a(), "p3").await, 200);

    // The exact scope is part of the identity: the agent-shared scope holds
    // its own `s1` at the same revision under the same key.
    let (status, user) = forget(&app, &BOTH, "notes", &au(), "s", &[("s1", 1)]).await;
    assert_eq!(status, 200, "{user}");
    assert_eq!(
        user["receipt"]["scope"],
        json!({"tenant": "@root", "agent": "a", "user": "u"})
    );
    assert_eq!(recalled(&app, &BOTH, "notes", &au(), "s1").await, 404);
    assert_eq!(recalled(&app, &BOTH, "notes", &a(), "s1").await, 200);
    let (status, shared) = forget(&app, &BOTH, "notes", &a(), "s", &[("s1", 1)]).await;
    assert_eq!(status, 200, "{shared}");
    assert_eq!(shared["replayed"], false);
    assert_eq!(
        shared["receipt"]["scope"],
        json!({"tenant": "@root", "agent": "a"})
    );
    assert_ne!(shared["receipt"]["id"], user["receipt"]["id"]);
    assert_eq!(recalled(&app, &BOTH, "notes", &a(), "s1").await, 404);
    let (status, again) = forget(&app, &BOTH, "notes", &au(), "s", &[("s1", 1)]).await;
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["replayed"], true);
    assert_eq!(again["receipt"], user["receipt"]);
}

#[tokio::test]
async fn duplicate_keys_and_inexact_revisions_never_reach_the_core() {
    let directory = TempDir::new().unwrap();
    let app = start(
        &directory,
        database(&[("@root", "notes")]),
        &[&PARENT],
        &[binding("notes", "@root", json!([grant(&PARENT, &[a()])]))],
    );
    remember(&app, &PARENT, "notes", &a(), "m1").await;
    let path = route("notes", "forget");
    let token = PARENT.token();

    // A duplicate key at any depth is malformed JSON, not "the last one wins".
    let duplicates = [
        r#"{"scope":{"agent":"a"},"idempotency_key":"d","target":{"ids":[{"id":"m1","revision":"1","revision":"1"}]},"mode":"logical_delete"}"#,
        r#"{"scope":{"agent":"a"},"idempotency_key":"d","target":{"ids":[{"id":"absent","revision":"1"}]},"target":{"ids":[{"id":"m1","revision":"1"}]},"mode":"logical_delete"}"#,
        r#"{"scope":{"agent":"a"},"idempotency_key":"d","target":{"ids":[{"id":"m1","revision":"1"}]},"mode":"expire","mode":"logical_delete"}"#,
        r#"{"scope":{"agent":"a"},"scope":{"agent":"a"},"idempotency_key":"d","target":{"ids":[{"id":"m1","revision":"1"}]},"mode":"logical_delete"}"#,
        r#"{"scope":{"agent":"a","agent":"a"},"idempotency_key":"d","target":{"ids":[{"id":"m1","revision":"1"}]},"mode":"logical_delete"}"#,
    ];
    for text in duplicates {
        let result = send(&app, &path, Some(&token), text.as_bytes().to_vec()).await;
        assert_error(&result, 400, "invalid_json");
    }
    assert_eq!(recalled(&app, &PARENT, "notes", &a(), "m1").await, 200);

    // Integers are not booleans, a fraction is never truncated and a decimal
    // string is canonical or invalid: `revision` 1 is the only live value.
    let invalid = [
        json!(true),
        json!(1.5),
        json!("1.5"),
        json!("01"),
        json!("+1"),
        json!(-1),
        json!("-1"),
        json!("18446744073709551616"),
    ];
    for revision in invalid {
        let mut body = forget_body(&a(), "r", &[("m1", 1)]);
        body["target"]["ids"][0]["revision"] = revision.clone();
        let result = post(&app, &path, &PARENT, &body).await;
        assert_error(&result, 422, "invalid_input");
        assert_eq!(
            result.1["error"]["details"]["field"], "target",
            "{revision}"
        );
    }
    assert_eq!(recalled(&app, &PARENT, "notes", &a(), "m1").await, 200);

    // The exact integer 1 is the live revision.
    let mut body = forget_body(&a(), "r", &[("m1", 1)]);
    body["target"]["ids"][0]["revision"] = json!(1);
    let (status, forgot) = post(&app, &path, &PARENT, &body).await;
    assert_eq!(status, 200, "{forgot}");
    assert_eq!(forgot["outcomes"][0]["id"], "m1");
    assert_eq!(recalled(&app, &PARENT, "notes", &a(), "m1").await, 404);
}

#[tokio::test]
async fn explicit_targets_are_a_bounded_array_and_the_mode_is_required() {
    let directory = TempDir::new().unwrap();
    let app = start(
        &directory,
        database(&[("@root", "notes")]),
        &[&PARENT],
        &[binding("notes", "@root", json!([grant(&PARENT, &[a()])]))],
    );
    remember(&app, &PARENT, "notes", &a(), "m1").await;
    let path = route("notes", "forget");
    let many = |count: usize| {
        (0..count)
            .map(|index| json!({"id": format!("t{index}"), "revision": "1"}))
            .collect::<Vec<_>>()
    };

    // 4,096 distinct targets pass the adapter's count bound; the core then
    // finds none of them: 404, not the adapter's 422.
    let mut body = forget_body(&a(), "max", &[]);
    body["target"]["ids"] = json!(many(4_096));
    let result = post(&app, &path, &PARENT, &body).await;
    assert_error(&result, 404, "memory_not_found");
    // 4,097 exceed it, and the bound is named.
    body["target"]["ids"] = json!(many(4_097));
    let result = post(&app, &path, &PARENT, &body).await;
    assert_error(&result, 422, "invalid_input");
    assert_eq!(result.1["error"]["details"]["field"], "target");
    // At least one target is required.
    body["target"]["ids"] = json!([]);
    let result = post(&app, &path, &PARENT, &body).await;
    assert_error(&result, 422, "invalid_input");
    assert_eq!(result.1["error"]["details"]["field"], "target");

    // `ids` is an array of references: one reference object is not an array.
    let mut single = forget_body(&a(), "single", &[("m1", 1)]);
    single["target"]["ids"] = json!({"id": "m1", "revision": "1"});
    let result = post(&app, &path, &PARENT, &single).await;
    assert_error(&result, 422, "invalid_input");
    assert_eq!(result.1["error"]["details"]["field"], "target");

    // `mode` is required: an absent or null mode is never `logical_delete`.
    for mode in [None, Some(Value::Null)] {
        let mut body = forget_body(&a(), "mode", &[("m1", 1)]);
        match mode {
            None => {
                body.as_object_mut().unwrap().remove("mode");
            }
            Some(value) => body["mode"] = value,
        }
        let result = post(&app, &path, &PARENT, &body).await;
        assert_error(&result, 422, "invalid_input");
        assert_eq!(result.1["error"]["details"]["field"], "mode");
    }

    // Target IDs are opaque keys: 1..=256 UTF-8 bytes, no control character.
    for id in ["x".repeat(256), "é".repeat(128)] {
        let result = forget(&app, &PARENT, "notes", &a(), "id-ok", &[(id.as_str(), 1)]).await;
        assert_error(&result, 404, "memory_not_found");
    }
    let invalid_ids = [
        json!(""),
        json!("x".repeat(257)),
        json!("é".repeat(129)),
        json!("a\u{0007}b"),
        json!(7),
    ];
    for id in invalid_ids {
        let mut body = forget_body(&a(), "id-bad", &[("m1", 1)]);
        body["target"]["ids"][0]["id"] = id.clone();
        let result = post(&app, &path, &PARENT, &body).await;
        assert_error(&result, 422, "invalid_input");
        assert_eq!(result.1["error"]["details"]["field"], "target", "{id}");
    }

    // The common mutation fields keep their contract bounds, and `logical_delete`
    // forbids `expires_at_ms`.
    let invalid = [
        ("deadline_ms", json!(0)),
        ("deadline_ms", json!(5001)),
        ("deadline_ms", json!("250")),
        ("max_references", json!(0)),
        ("max_references", json!(257)),
        ("require_complete_feed", json!("yes")),
        ("allow_job", json!("no")),
        ("expires_at_ms", json!("150")),
    ];
    for (name, value) in invalid {
        let mut body = forget_body(&a(), "common", &[("m1", 1)]);
        body[name] = value.clone();
        let result = post(&app, &path, &PARENT, &body).await;
        assert_error(&result, 422, "invalid_input");
        assert_eq!(result.1["error"]["details"]["field"], name, "{value}");
    }
    let served = [
        ("deadline_ms", json!(5000)),
        ("max_references", json!(1)),
        ("max_references", json!(256)),
        ("require_complete_feed", json!(false)),
        ("allow_job", json!(false)),
    ];
    for (name, value) in served {
        let mut body = forget_body(&a(), "edge", &[("ghost", 1)]);
        body[name] = value.clone();
        let result = post(&app, &path, &PARENT, &body).await;
        assert_error(&result, 404, "memory_not_found");
    }
    assert_eq!(recalled(&app, &PARENT, "notes", &a(), "m1").await, 200);
}

#[tokio::test]
async fn an_id_of_another_scope_is_absent_and_not_disclosed() {
    let directory = TempDir::new().unwrap();
    let app = start(
        &directory,
        database(&[("@root", "notes")]),
        &[&PARENT, &CHILD],
        &[binding(
            "notes",
            "@root",
            json!([grant(&PARENT, &[a()]), grant(&CHILD, &[au(), b()])]),
        )],
    );
    remember(&app, &CHILD, "notes", &b(), "secret").await;
    remember(&app, &CHILD, "notes", &au(), "secret").await;
    remember(&app, &PARENT, "notes", &a(), "mine").await;
    let missing = forget(&app, &PARENT, "notes", &a(), "k1", &[("never", 1)]).await;
    assert_error(&missing, 404, "memory_not_found");
    // Another scope's ID, at the right or a wrong revision, is the same
    // authorized-absent answer as an ID that never existed.
    for (key, revision) in [("k2", 1), ("k3", 2)] {
        let foreign = forget(&app, &PARENT, "notes", &a(), key, &[("secret", revision)]).await;
        assert_eq!(foreign, missing);
    }
    // A bundle with one foreign ID deletes nothing at all.
    let mixed = forget(
        &app,
        &PARENT,
        "notes",
        &a(),
        "k4",
        &[("mine", 1), ("secret", 1)],
    )
    .await;
    assert_eq!(mixed, missing);
    assert_eq!(recalled(&app, &PARENT, "notes", &a(), "mine").await, 200);
    for scope in [au(), b()] {
        assert_eq!(recalled(&app, &CHILD, "notes", &scope, "secret").await, 200);
    }
}

#[tokio::test]
async fn forget_guards_the_current_revision_of_a_merged_memory() {
    let directory = TempDir::new().unwrap();
    let app = start(
        &directory,
        database(&[("@root", "notes")]),
        &[&PARENT],
        &[binding("notes", "@root", json!([grant(&PARENT, &[a()])]))],
    );
    remember(&app, &PARENT, "notes", &a(), "m1").await;
    // A provenance merge from a second source makes revision 2.
    let merge = json!({
        "scope": a(), "idempotency_key": "merge", "deadline_ms": 5000,
        "items": [{
            "id": "m1", "kind": "entity", "content": "content of m1", "observed_at_ms": "90",
            "source": {"source_id": "s2", "source_kind": "caller"},
            "expected": {"revision": "1"}, "merge": "merge_provenance",
        }],
    });
    let (status, merged) = post(&app, &route("notes", "remember"), &PARENT, &merge).await;
    assert_eq!(status, 200, "{merged}");
    assert_eq!(
        merged["outcomes"],
        json!([{"id": "m1", "action": "merged", "revision": "2"}])
    );
    // The stale revision is a 412, deletes nothing and leaves no receipt: the
    // same key then executes with the live revision.
    let stale = forget(&app, &PARENT, "notes", &a(), "g", &[("m1", 1)]).await;
    assert_error(&stale, 412, "precondition_failed");
    assert_eq!(recalled(&app, &PARENT, "notes", &a(), "m1").await, 200);
    let (status, body) = forget(&app, &PARENT, "notes", &a(), "g", &[("m1", 2)]).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["replayed"], false);
    assert_eq!(recalled(&app, &PARENT, "notes", &a(), "m1").await, 404);
}
