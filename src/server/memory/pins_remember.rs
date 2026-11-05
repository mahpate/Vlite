//! Mutation-closure pins for the Remember route and the shared mutation
//! pipeline it uses (serving contract sections 3, 4, 6 and 8; adapter
//! mapping sections 1-4). Every expectation is derived from those documents
//! and the golden fixtures, never from the handlers. Time is driven through
//! the `Clock` seam, so no test depends on measured elapsed time.

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
use crate::memory::binding::{BindingError, parse as parse_bindings};
use crate::memory::{MemoryScope, ReceiptOperation, receipt_id};
use crate::{CollectionSpec, CreateOptions, Database, FormatVersion};

const PRINCIPAL: &str = "abcdefghij27";
const OTHER: &str = "bcdefghij234";
const TENANT: &str = "0123456789abcdef0123456789abcdef";
const REMEMBER: &str = "/v1/memory/notes/remember";
const RECALL: &str = "/v1/memory/notes/recall";
const FORBIDDEN: &str = "forbidden_scope";

fn options() -> CreateOptions {
    CreateOptions {
        format: Some(FormatVersion::V5),
        ..Default::default()
    }
}

fn spec() -> CollectionSpec {
    CollectionSpec {
        dimensions: 2,
        settings: Default::default(),
    }
}

fn stored(tenant: &str, name: &str) -> String {
    if tenant == "@root" {
        name.to_owned()
    } else {
        format!("t{tenant}__{name}")
    }
}

/// One binding whose graph and evidence collection are `{binding}_graph` and
/// `{binding}_ev`.
fn entry(tenant: &str, binding: &str, window: u64, grants: Value) -> Value {
    json!({
        "binding": binding,
        "tenant": tenant,
        "generation": 1,
        "state": "active",
        "graph": format!("{binding}_graph"),
        "evidence_collection": format!("{binding}_ev"),
        "schema_version": "memory-schema-v1",
        "ranking_version": "memory-ranking-v1",
        "digest_version": "memory-request-v1",
        "receipt_window_ms": window,
        "grants": grants,
    })
}

/// The binding `notes` of the root tenant and what the credential is granted
/// on it.
struct Spec {
    window: u64,
    scopes: Vec<Value>,
    retired_at: Option<u64>,
    read_only: bool,
}

impl Default for Spec {
    fn default() -> Self {
        Self {
            window: 60_000,
            scopes: vec![json!({"agent": "a"})],
            retired_at: None,
            read_only: false,
        }
    }
}

/// The real memory router over a fresh database, behind a scoped credential
/// and the `Clock` seam (engine clock 100 ms, no elapsed time).
struct Rig {
    router: Router,
    clock: Clock,
    service: Service,
    _dir: tempfile::TempDir,
}

impl Rig {
    fn new(spec: &Spec) -> Self {
        let mut grants = vec![json!({"principal": OTHER, "scopes": [{"agent": "other"}]})];
        if !spec.scopes.is_empty() {
            grants.push(json!({"principal": PRINCIPAL, "scopes": spec.scopes}));
        }
        let mut notes = entry("@root", "notes", spec.window, Value::Array(grants));
        if let Some(at) = spec.retired_at {
            notes["state"] = json!("retired");
            notes["retired_at_ms"] = json!(at);
        }
        Self::start(vec![notes], None, spec.read_only)
    }

    /// Creates the stored graph and collection of every binding, starts the
    /// service on them and acts as `PRINCIPAL` of `credential_tenant`.
    fn start(bindings: Vec<Value>, credential_tenant: Option<&str>, read_only: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pins.vlite");
        let mut db = if read_only {
            Database::create_with_options(&path, 2, options()).unwrap()
        } else {
            Database::create_in_memory(2, options()).unwrap()
        };
        for binding in &bindings {
            let tenant = binding["tenant"].as_str().unwrap();
            db.create_graph(&stored(tenant, binding["graph"].as_str().unwrap()))
                .unwrap();
            if let Some(name) = binding["evidence_collection"].as_str() {
                db.create_collection(&stored(tenant, name), spec()).unwrap();
            }
        }
        if read_only {
            db.close().unwrap();
            db = Database::open_read_only(&path).unwrap();
        }
        let lineage = db.lineage().unwrap();
        let document = json!({"version": "memory-bindings-v1", "bindings": bindings}).to_string();
        let set = parse_bindings(document.as_bytes()).expect("valid bindings");
        let service = Service::new(db).with_bindings(Bindings::new(set));
        super::startup_at(&service, 0).expect("startup");
        let grant = credentials::Grant::for_user(
            PRINCIPAL.into(),
            "pins".into(),
            credential_tenant.map(str::to_owned),
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
            .layer(Extension(clock.clone()))
            .layer(DefaultBodyLimit::max(MAX_JSON_BYTES));
        Self {
            router,
            clock,
            service,
            _dir: dir,
        }
    }

    fn at(&self, now_ms: u64) {
        self.clock.now_ms.store(now_ms, Ordering::SeqCst);
    }

    fn elapsed(&self, elapsed_ms: u64) {
        self.clock.elapsed_ms.store(elapsed_ms, Ordering::SeqCst);
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

    async fn send(&self, path: &str, bytes: Vec<u8>) -> (u16, String) {
        let request = Request::builder()
            .method("POST")
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(bytes))
            .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status().as_u16();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    async fn post_to(&self, path: &str, body: &Value) -> (u16, Value) {
        let (status, text) = self.send(path, serde_json::to_vec(body).unwrap()).await;
        (status, parse(&text))
    }

    async fn post(&self, body: &Value) -> (u16, Value) {
        self.post_to(REMEMBER, body).await
    }

    async fn post_text(&self, text: &str) -> (u16, Value) {
        let (status, text) = self.send(REMEMBER, text.as_bytes().to_vec()).await;
        (status, parse(&text))
    }

    /// Recall of one memory by entity seed, in the exact scope `{agent: a}`.
    async fn recall(&self, id: &str) -> (u16, Value) {
        let body = json!({"scope": {"agent": "a"}, "entity_seeds": [id]});
        self.post_to(RECALL, &body).await
    }
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|error| panic!("not JSON ({error}): {text}"))
}

/// A bounded rendering of a response for failure messages.
fn short(value: &Value) -> String {
    value.to_string().chars().take(300).collect()
}

fn source() -> Value {
    json!({"source_id": "s1", "source_kind": "caller"})
}

fn item(id: &str, content: &str) -> Value {
    json!({
        "id": id,
        "kind": "entity",
        "content": content,
        "observed_at_ms": "90",
        "source": source(),
    })
}

/// `base` with the members of `extra` set or replaced.
fn with(mut base: Value, extra: Value) -> Value {
    base.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    base
}

fn request(key: &str, items: Vec<Value>) -> Value {
    json!({"scope": {"agent": "a"}, "idempotency_key": key, "items": items})
}

/// The request text of one entity item, for exact control over the raw JSON
/// of its observation (`observed`, a complete JSON value) and of any extra
/// members (`extra`, which starts with a comma).
fn text(key: &str, id: &str, observed: &str, extra: &str) -> String {
    format!(
        r#"{{"scope":{{"agent":"a"}},"idempotency_key":"{key}","items":[{{"id":"{id}","kind":"entity","content":"c-{id}","observed_at_ms":{observed},"source":{{"source_id":"s1","source_kind":"caller"}}{extra}}}]}}"#
    )
}

/// A head item and the tail item it references (the reference's revision is
/// the raw JSON `revision`); the key names both items.
fn linked(key: &str, revision: &str) -> String {
    format!(
        r#"{{"scope":{{"agent":"a"}},"idempotency_key":"{key}","items":[{{"id":"h-{key}","kind":"entity","content":"head {key}","observed_at_ms":"90","source":{{"source_id":"s1","source_kind":"caller"}},"references":[{{"id":"t-{key}","revision":{revision}}}]}},{{"id":"t-{key}","kind":"entity","content":"tail {key}","observed_at_ms":"90","source":{{"source_id":"s1","source_kind":"caller"}}}}]}}"#
    )
}

/// One head item that references `refs` tail items of its own bundle.
fn bundle(key: &str, refs: usize, extra: Value) -> Value {
    let head: Vec<Value> = (0..refs)
        .map(|i| json!({"id": format!("t{i}-{key}"), "revision": "0"}))
        .collect();
    let mut items = vec![with(
        item(&format!("h-{key}"), &format!("head {key}")),
        json!({"references": head}),
    )];
    items.extend((0..refs).map(|i| item(&format!("t{i}-{key}"), &format!("tail {i} {key}"))));
    with(request(key, items), extra)
}

fn assert_invalid(status: u16, answer: &Value, field: &str, case: &str) {
    assert_eq!(status, 422, "{case}: {answer}");
    assert_eq!(answer["error"]["code"], "invalid_input", "{case}: {answer}");
    assert_eq!(
        answer["error"]["details"],
        json!({"field": field}),
        "{case}: {answer}"
    );
}

fn assert_refused(status: u16, answer: &Value, feature: &str, case: &str) {
    assert_eq!(status, 409, "{case}: {answer}");
    assert_eq!(
        answer["error"]["code"], "memory_capability_unavailable",
        "{case}: {answer}"
    );
    assert_eq!(
        answer["error"]["details"],
        json!({"feature": feature}),
        "{case}: {answer}"
    );
}

fn assert_forbidden(status: u16, answer: &Value, case: &str) {
    assert_eq!(status, 403, "{case}: {answer}");
    assert_eq!(answer["error"]["code"], FORBIDDEN, "{case}: {answer}");
    assert!(answer["error"].get("details").is_none(), "{case}: {answer}");
}

fn assert_code(status: u16, answer: &Value, expected: (u16, &str), case: &str) {
    assert_eq!(
        (status, answer["error"]["code"].as_str().unwrap_or_default()),
        expected,
        "{case}: {answer}"
    );
}

fn created(id: &str) -> Value {
    json!({"id": id, "action": "created", "revision": "1"})
}

// ------------------------------------------------------------ check order

/// Mapping section 2 check order, step 3 before step 4: a read-only node
/// answers 403 `read_only` for every request, whatever refusal, invalid
/// field or retired binding it would otherwise meet. Pins
/// g1r-order-writable-after-refusal.
#[tokio::test]
async fn a_read_only_node_answers_read_only_before_refusals_and_validation() {
    let evidence = json!([{"collection": "notes_ev", "record": "1"}]);
    let one = |extra: Value| request("k", vec![with(item("e1", "x"), extra)]);
    let plain = request("k", vec![item("e1", "x")]);
    let bodies = [
        one(json!({"kind": "fact"})),
        one(json!({"kind": "relation"})),
        one(json!({"confidence": 0.5})),
        one(json!({"references": [{"id": "e2", "revision": "1"}]})),
        one(json!({"evidence": evidence})),
        one(json!({"metadata": {}})),
        one(json!({"assertion": {}})),
        one(json!({"expected": {"revision": "1"}})),
        with(plain.clone(), json!({"evidence": evidence})),
        with(plain.clone(), json!({"require_complete_feed": true})),
        with(plain.clone(), json!({"unknown": 1})),
        json!({"scope": {"agent": "a"}}),
        plain,
    ];
    for retired_at in [None, Some(500)] {
        let rig = Rig::new(&Spec {
            read_only: true,
            retired_at,
            ..Spec::default()
        });
        for body in &bodies {
            let (status, answer) = rig.post(body).await;
            assert_code(
                status,
                &answer,
                (403, "read_only"),
                &format!("{retired_at:?} {body}"),
            );
        }
    }
}

/// Mapping section 2: a capability refusal (step 4) precedes request
/// validation (step 5), so an invalid or absent idempotency key or an invalid
/// budget never hides it; alone each is a 422 on its own field. Pins
/// g1r-order-validation-before-refusal.
#[tokio::test]
async fn capability_refusals_precede_idempotency_key_and_budget_validation() {
    let rig = Rig::new(&Spec::default());
    let refused = request("k", vec![with(item("e1", "x"), json!({"kind": "fact"}))]);
    let clean = request("k", vec![item("e1", "x")]);
    let invalid: [(&str, Option<Value>); 22] = [
        ("idempotency_key", None),
        ("idempotency_key", Some(json!(""))),
        ("idempotency_key", Some(json!("k".repeat(257)))),
        ("idempotency_key", Some(json!("bad\u{1}key"))),
        ("idempotency_key", Some(json!(7))),
        ("idempotency_key", Some(Value::Null)),
        ("max_references", Some(json!(0))),
        ("max_references", Some(json!(257))),
        ("max_references", Some(json!("64"))),
        ("max_references", Some(json!(-1))),
        ("max_references", Some(json!(true))),
        ("max_references", Some(Value::Null)),
        ("max_references", Some(json!(1.5))),
        ("deadline_ms", Some(json!(0))),
        ("deadline_ms", Some(json!(5001))),
        ("deadline_ms", Some(json!("250"))),
        ("deadline_ms", Some(json!(false))),
        ("deadline_ms", Some(Value::Null)),
        ("deadline_ms", Some(json!(250.5))),
        ("require_complete_feed", Some(json!("yes"))),
        ("require_complete_feed", Some(json!(1))),
        ("require_complete_feed", Some(Value::Null)),
    ];
    for (field, value) in &invalid {
        let patch = |mut base: Value| {
            match value {
                Some(value) => base[*field] = value.clone(),
                None => {
                    base.as_object_mut().unwrap().remove(*field);
                }
            }
            base
        };
        let case = format!("{field}={value:?}");
        let (status, answer) = rig.post(&patch(refused.clone())).await;
        assert_refused(status, &answer, "fact_kinds", &case);
        let (status, answer) = rig.post(&patch(clean.clone())).await;
        assert_invalid(status, &answer, field, &case);
    }
    // The refusal priority: the complete-feed refusal precedes field refusals
    // and, like them, request validation.
    let both = with(refused, json!({"require_complete_feed": true}));
    let (status, answer) = rig.post(&both).await;
    assert_code(status, &answer, (409, "memory_feed_unsupported"), "feed");
    assert!(answer["error"].get("details").is_none(), "{answer}");
    let broken = with(both, json!({"idempotency_key": ""}));
    let (status, answer) = rig.post(&broken).await;
    assert_code(
        status,
        &answer,
        (409, "memory_feed_unsupported"),
        "feed, key",
    );
}

/// Mapping section 2: the exact-scope authorization (step 2) precedes every
/// refusal and every validation of the rest of the body, on a granted binding
/// and on a retired one; there is no parent or child fallback. Pins
/// g1r-order-scope-after-refusal.
#[tokio::test]
async fn an_ungranted_scope_precedes_refusals_and_validation() {
    let plain = request("k", vec![item("e1", "x")]);
    let refused = request("k", vec![with(item("e1", "x"), json!({"kind": "fact"}))]);
    let evidence = json!([{"collection": "notes_ev", "record": "1"}]);
    let bodies = [
        plain.clone(),
        refused,
        with(plain.clone(), json!({"require_complete_feed": true})),
        with(plain.clone(), json!({"evidence": evidence})),
        with(plain.clone(), json!({"idempotency_key": ""})),
        with(plain.clone(), json!({"unknown": 1})),
        json!({"scope": {"agent": "a"}}),
        request("k", Vec::new()),
    ];
    let ungranted = [
        json!({"agent": "b"}),
        json!({"agent": "a", "user": "u"}),
        json!({"agent": "a", "user": "u", "session": "s"}),
    ];
    for retired_at in [None, Some(500)] {
        let rig = Rig::new(&Spec {
            retired_at,
            ..Spec::default()
        });
        let mut reference: Option<Value> = None;
        for scope in &ungranted {
            for body in &bodies {
                let sent = with(body.clone(), json!({"scope": scope}));
                let (status, answer) = rig.post(&sent).await;
                let case = format!("{retired_at:?} {sent}");
                assert_forbidden(status, &answer, &case);
                // One body for every denial.
                assert_eq!(
                    &answer,
                    reference.get_or_insert_with(|| answer.clone()),
                    "{case}"
                );
            }
        }
    }
    // The control: the granted scope reaches the refusal of a retired binding.
    let rig = Rig::new(&Spec {
        retired_at: Some(500),
        ..Spec::default()
    });
    let (status, answer) = rig.post(&plain).await;
    assert_refused(status, &answer, "binding_retired", "granted scope");
    // Neither a granted child nor a granted parent stands in for the scope.
    let rig = Rig::new(&Spec {
        scopes: vec![json!({"agent": "a", "user": "u"})],
        ..Spec::default()
    });
    let (status, answer) = rig.post(&plain).await;
    assert_forbidden(status, &answer, "parent of the granted scope");
    let child = with(plain, json!({"scope": {"agent": "a", "user": "u"}}));
    let (status, answer) = rig.post(&child).await;
    assert_eq!(status, 200, "{answer}");
}

/// Mapping section 2 step 2: an unknown binding, another tenant's binding, a
/// binding without a grant for the credential, a stored graph or collection
/// name used as a binding and an ungranted scope are one 403 body. Pins
/// g1r-denial-reserved-physical-name (and the unknown and ungranted forms).
#[tokio::test]
async fn every_binding_denial_has_one_403_body() {
    let own = json!([
        {"principal": PRINCIPAL, "scopes": [{"agent": "a"}]},
        {"principal": OTHER, "scopes": [{"agent": "other"}]},
    ]);
    let rig = Rig::start(
        vec![
            entry("@root", "notes", 60_000, own.clone()),
            entry(
                "@root",
                "shared",
                60_000,
                json!([{"principal": OTHER, "scopes": [{"agent": "a"}]}]),
            ),
            entry(TENANT, "notes", 60_000, own),
        ],
        None,
        false,
    );
    let body = request("k", vec![item("e1", "x")]);
    let denied = [
        "/v1/memory/absent/remember".to_owned(),
        "/v1/memory/shared/remember".to_owned(),
        "/v1/memory/notes_graph/remember".to_owned(),
        "/v1/memory/notes_ev/remember".to_owned(),
        "/v1/memory/shared_graph/remember".to_owned(),
        "/v1/memory/shared_ev/remember".to_owned(),
        format!("/v1/memory/t{TENANT}__notes_graph/remember"),
        format!("/v1/memory/t{TENANT}__notes_ev/remember"),
        "/v1/memory/%FF/remember".to_owned(),
    ];
    let ungranted = with(body.clone(), json!({"scope": {"agent": "b"}}));
    let (status, ungranted) = rig.post(&ungranted).await;
    assert_forbidden(status, &ungranted, "ungranted scope");
    for path in denied {
        let (status, answer) = rig.post_to(&path, &body).await;
        assert_forbidden(status, &answer, &path);
        assert_eq!(answer, ungranted, "{path}");
    }
    // The control: the granted binding and scope remember.
    let (status, answer) = rig.post(&body).await;
    assert_eq!(status, 200, "{answer}");
}

// --------------------------------------------------------- strict integers

/// Contract section 3: a `MemoryU64` input is a canonical decimal string or
/// an exact JSON integer in range; integers are not booleans. Pins
/// g1r-wire-u64-leading-zero and g1r-wire-u64-boolean.
#[tokio::test]
async fn unsigned_inputs_are_canonical_strings_or_exact_integers() {
    let rig = Rig::new(&Spec::default());
    let valid = [
        r#""1000""#,
        "1000",
        r#""0""#,
        "0",
        r#""18446744073709551615""#,
        "18446744073709551615",
    ];
    for (index, raw) in valid.iter().enumerate() {
        let extra = format!(r#","expires_at_ms":{raw}"#);
        let body = text(
            &format!("u-{index}"),
            &format!("m{index}"),
            r#""90""#,
            &extra,
        );
        let (status, answer) = rig.post_text(&body).await;
        assert_eq!(status, 200, "{raw}: {answer}");
        assert_eq!(
            answer["outcomes"],
            json!([created(&format!("m{index}"))]),
            "{raw}"
        );
    }
    let invalid = [
        r#""01""#,
        r#""00""#,
        r#""+1""#,
        r#"" 1""#,
        r#""1 ""#,
        r#""-1""#,
        r#""-0""#,
        r#""1.0""#,
        r#""1e3""#,
        r#""""#,
        r#""0x10""#,
        "\"\u{ff11}\"",
        r#""18446744073709551616""#,
        "-1",
        "18446744073709551616",
        "true",
        "false",
        "null",
        "[]",
        "{}",
    ];
    for raw in invalid {
        let extra = format!(r#","expires_at_ms":{raw}"#);
        let (status, answer) = rig.post_text(&text("u-bad", "m", r#""90""#, &extra)).await;
        assert_invalid(status, &answer, "expires_at_ms", raw);
    }
}

/// The same grammar governs a bundle reference's revision: only 0, naming an
/// item of the same bundle, is served; a non-canonical form never reads as
/// one, and every nonzero revision is a pin the first slice refuses by name.
/// Pins g1r-wire-u64-boolean and g1r-wire-u64-leading-zero.
#[tokio::test]
async fn a_reference_revision_is_a_strict_unsigned_input() {
    let rig = Rig::new(&Spec::default());
    for (index, revision) in [r#""0""#, "0"].into_iter().enumerate() {
        let (status, answer) = rig
            .post_text(&linked(&format!("ok{index}"), revision))
            .await;
        assert_eq!(status, 200, "{revision}: {answer}");
        assert_eq!(
            answer["outcomes"].as_array().unwrap().len(),
            2,
            "{revision}"
        );
    }
    let invalid = [
        r#""00""#,
        r#""-0""#,
        r#""+0""#,
        r#"" 0""#,
        r#""0 ""#,
        r#""""#,
        r#""01""#,
        r#""18446744073709551616""#,
        "false",
        "true",
        "[]",
        "{}",
        "-1",
    ];
    for (index, revision) in invalid.into_iter().enumerate() {
        let (status, answer) = rig
            .post_text(&linked(&format!("bad{index}"), revision))
            .await;
        assert_invalid(status, &answer, "references", revision);
    }
    let pins = [
        r#""1""#,
        "1",
        r#""18446744073709551615""#,
        "18446744073709551615",
    ];
    for (index, revision) in pins.into_iter().enumerate() {
        let (status, answer) = rig
            .post_text(&linked(&format!("pin{index}"), revision))
            .await;
        assert_refused(status, &answer, "reference_revision_pin", revision);
    }
}

/// The merge guard `expected.revision` is a `MemoryU64` as well: an invalid
/// form is a 422 on `merge`, a valid one reaches the core (no such memory).
/// Pins g1r-wire-u64-boolean on the merge path.
#[tokio::test]
async fn the_merge_revision_is_a_strict_unsigned_input() {
    let rig = Rig::new(&Spec::default());
    let merge = |revision: &str| {
        let extra = format!(r#","merge":"merge_provenance","expected":{{"revision":{revision}}}"#);
        text("m-key", "m0", r#""90""#, &extra)
    };
    for revision in [
        r#""01""#, r#""+1""#, r#"" 1""#, "true", "false", "-1", r#""""#, "[]",
    ] {
        let (status, answer) = rig.post_text(&merge(revision)).await;
        assert_invalid(status, &answer, "merge", revision);
    }
    for revision in [r#""1""#, "1", r#""0""#, "0"] {
        let (status, answer) = rig.post_text(&merge(revision)).await;
        assert_code(status, &answer, (404, "memory_not_found"), revision);
    }
}

/// Contract section 3: a `MemoryI64` input is a canonical signed decimal
/// string (no `-0`, no leading zero, no whitespace) or an exact JSON integer,
/// while ordinary JSON layout whitespace is untouched. Pins
/// g1r-wire-i64-padded-string.
#[tokio::test]
async fn signed_inputs_are_canonical_strings_or_exact_integers() {
    let rig = Rig::new(&Spec::default());
    let valid = [
        r#""90""#,
        "90",
        r#""0""#,
        "0",
        r#""-1""#,
        "-1",
        r#""-9007199254740993""#,
        "-9007199254740993",
        r#""-9223372036854775808""#,
        "-9223372036854775808",
    ];
    for (index, raw) in valid.iter().enumerate() {
        let body = text(&format!("i-{index}"), &format!("m{index}"), raw, "");
        let (status, answer) = rig.post_text(&body).await;
        assert_eq!(status, 200, "{raw}: {answer}");
        assert_eq!(
            answer["outcomes"],
            json!([created(&format!("m{index}"))]),
            "{raw}"
        );
    }
    let invalid = [
        r#""01""#,
        r#""-01""#,
        r#""-0""#,
        r#""+1""#,
        r#"" 90""#,
        r#""90 ""#,
        r#""9 0""#,
        r#""\t90""#,
        r#""90\n""#,
        r#""\u00a090""#,
        r#""""#,
        r#""-""#,
        r#""1e2""#,
        r#""90.0""#,
        r#""9223372036854775808""#,
        r#""-9223372036854775809""#,
        "9223372036854775808",
        "-9223372036854775809",
        "true",
        "false",
        "null",
        "[]",
        "{}",
    ];
    for raw in invalid {
        let (status, answer) = rig.post_text(&text("i-bad", "m", raw, "")).await;
        assert_invalid(status, &answer, "observed_at_ms", raw);
    }
    // Layout whitespace between and around tokens is not part of any value.
    let pretty = serde_json::to_string_pretty(&request("ws", vec![item("m-ws", "c-ws")])).unwrap();
    let (status, answer) = rig.post_text(&pretty).await;
    assert_eq!(status, 200, "{answer}");
    let padded = format!("\r\n\t {} \n", pretty.replace('\n', "\r\n\t"));
    let (status, answer) = rig.post_text(&padded).await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["replayed"], true);
}

/// Contract section 3 and wire decision 12: full-range inputs are exact, never
/// rounded through binary64; an integer and its canonical string are one
/// request identity and the neighbouring value is another. Pins
/// g1r-wire-i64-rounded-integer and g1r-wire-u64-rounded-integer.
#[tokio::test]
async fn integers_beyond_two_to_the_53_are_exact() {
    let rig = Rig::new(&Spec::default());
    // MemoryI64: -(2^53 + 1).
    let integer = text("exact-i", "e1", "-9007199254740993", "");
    let string = text("exact-i", "e1", r#""-9007199254740993""#, "");
    let neighbour = text("exact-i", "e1", r#""-9007199254740992""#, "");
    let (status, first) = rig.post_text(&integer).await;
    assert_eq!(status, 200, "{first}");
    assert_eq!(first["replayed"], false);
    let (status, again) = rig.post_text(&string).await;
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["replayed"], true);
    assert_eq!(again["receipt"], first["receipt"]);
    let (status, conflict) = rig.post_text(&neighbour).await;
    assert_code(
        status,
        &conflict,
        (409, "idempotency_conflict"),
        "neighbour",
    );
    // MemoryU64: 2^53 + 1 as an expiry.
    let tail = |value: &str| format!(r#","expires_at_ms":{value}"#);
    let integer = text("exact-u", "e2", r#""90""#, &tail("9007199254740993"));
    let string = text("exact-u", "e2", r#""90""#, &tail(r#""9007199254740993""#));
    let neighbour = text("exact-u", "e2", r#""90""#, &tail(r#""9007199254740992""#));
    let (status, first) = rig.post_text(&integer).await;
    assert_eq!(status, 200, "{first}");
    assert_eq!(first["replayed"], false);
    let (status, again) = rig.post_text(&string).await;
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["replayed"], true);
    assert_eq!(again["receipt"], first["receipt"]);
    let (status, conflict) = rig.post_text(&neighbour).await;
    assert_code(
        status,
        &conflict,
        (409, "idempotency_conflict"),
        "neighbour",
    );
}

// ------------------------------------------------------------------ limits

/// Contract section 4: 1..=4,096 items. The ceiling is independent of the
/// frame's capacity, so the witness is a bundle of exact-dedup no-ops of one
/// retained memory. Pins g1r-limit-items-max.
#[tokio::test]
async fn four_thousand_ninety_six_items_are_served_and_one_more_is_not() {
    let rig = Rig::new(&Spec::default());
    let (status, seeded) = rig
        .post(&request("seed", vec![item("seed-memory", "x")]))
        .await;
    assert_eq!(status, 200, "{seeded}");
    let many = |count: usize, key: &str| {
        let items = (0..count).map(|i| item(&format!("n{i}"), "x")).collect();
        request(key, items)
    };
    let (status, answer) = rig.post(&many(4096, "max")).await;
    let shown = short(&answer);
    assert_eq!(status, 200, "{shown}");
    let noop = json!({"id": "seed-memory", "action": "noop", "revision": "1"});
    let outcomes = answer["outcomes"].as_array().unwrap();
    assert_eq!(outcomes.len(), 4096);
    assert!(outcomes.iter().all(|outcome| *outcome == noop));
    let (status, answer) = rig.post(&many(4097, "over")).await;
    assert_invalid(status, &answer, "items", "4097 items");
    let (status, answer) = rig.post(&request("none", Vec::new())).await;
    assert_invalid(status, &answer, "items", "no items");
}

/// Contract section 4: `max_references` (1..=256, default 64) caps each
/// item's references; a caller's smaller budget is never replaced by the
/// ceiling and the ceiling itself is legal. Pins g1r-limit-reference-max,
/// g1r-limit-explicit-reference-cap and g1r-limit-reference-default.
#[tokio::test]
async fn references_are_capped_per_item_by_the_default_and_by_max_references() {
    let rig = Rig::new(&Spec::default());
    // (max_references, references per item, served)
    let cases: [(Option<u64>, usize, bool); 12] = [
        (None, 64, true),
        (None, 65, false),
        (Some(1), 1, true),
        (Some(1), 2, false),
        (Some(2), 2, true),
        (Some(2), 3, false),
        (Some(64), 64, true),
        (Some(64), 65, false),
        (Some(100), 65, true),
        (Some(100), 101, false),
        (Some(256), 256, true),
        (Some(256), 257, false),
    ];
    for (cap, refs, served) in cases {
        let label = cap.map_or_else(|| "default".to_owned(), |cap| cap.to_string());
        let key = format!("c-{label}-{refs}");
        let extra = cap.map_or_else(|| json!({}), |cap| json!({"max_references": cap}));
        let (status, answer) = rig.post(&bundle(&key, refs, extra)).await;
        let case = format!("{label} {refs}");
        if served {
            let shown = short(&answer);
            assert_eq!(status, 200, "{case}: {shown}");
            assert_eq!(
                answer["outcomes"].as_array().unwrap().len(),
                refs + 1,
                "{case}"
            );
        } else {
            assert_invalid(status, &answer, "references", &case);
        }
    }
    // The budget itself is an integer in 1..=256.
    for cap in [0_u64, 257] {
        let (status, answer) = rig
            .post(&bundle(
                &format!("cap-{cap}"),
                1,
                json!({"max_references": cap}),
            ))
            .await;
        assert_invalid(status, &answer, "max_references", &cap.to_string());
    }
    for cap in [1_u64, 256] {
        let (status, answer) = rig
            .post(&bundle(
                &format!("cap-{cap}"),
                1,
                json!({"max_references": cap}),
            ))
            .await;
        assert_eq!(status, 200, "{cap}: {answer}");
    }
}

/// Contract sections 3-4: provenance holds at most 64 entries within 4,096
/// encoded bytes; overflow is rejected, never dropped. Pins
/// g1r-limit-provenance-count and g1r-limit-provenance-byte-truncation.
#[tokio::test]
async fn provenance_is_bounded_in_count_and_bytes_and_never_truncated() {
    let rig = Rig::new(&Spec::default());
    let entries = |count: usize, width: usize| -> Value {
        Value::Array(
            (0..count)
                .map(|i| json!({"source_id": format!("{i:0>width$}"), "source_kind": "k"}))
                .collect(),
        )
    };
    let one = |key: &str, provenance: Value| {
        let member = with(
            item(&format!("m-{key}"), key),
            json!({"provenance": provenance}),
        );
        request(key, vec![member])
    };
    // Count: 64 entries are served, 65 are not.
    let (status, answer) = rig.post(&one("p64", entries(64, 3))).await;
    assert_eq!(status, 200, "{answer}");
    let (status, answer) = rig.post(&one("p65", entries(65, 3))).await;
    assert_invalid(status, &answer, "provenance", "65 entries");
    // Bytes: eight 256-byte sources are served and all of them are kept.
    let (status, answer) = rig.post(&one("p8", entries(8, 256))).await;
    assert_eq!(status, 200, "{answer}");
    let (status, recalled) = rig.recall("m-p8").await;
    assert_eq!(status, 200, "{recalled}");
    let memory = recalled["memories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|memory| memory["id"] == "m-p8")
        .unwrap();
    let kept: Vec<&str> = memory["provenance"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["source_id"].as_str().unwrap())
        .collect();
    let expected: Vec<String> = (0..8).map(|i| format!("{i:0>256}")).collect();
    assert_eq!(kept, expected);
    // Sixty-four of them exceed the 4,096 encoded bytes: rejected whole, and
    // nothing of the item is stored.
    let (status, answer) = rig.post(&one("p64x", entries(64, 256))).await;
    assert_code(
        status,
        &answer,
        (422, "invalid_input"),
        "oversized provenance",
    );
    let (status, absent) = rig.recall("m-p64x").await;
    assert_code(status, &absent, (404, "memory_not_found"), "nothing stored");
}

/// Contract section 4: a source URI is at most 256 UTF-8 bytes, on the item's
/// source and on every provenance entry. Pins g1r-limit-uri-max.
#[tokio::test]
async fn source_uris_are_bounded_at_256_utf8_bytes() {
    let rig = Rig::new(&Spec::default());
    let cases = [
        ("u".to_owned(), true),
        ("u".repeat(256), true),
        ("u".repeat(257), false),
        ("\u{e9}".repeat(128), true),
        ("\u{e9}".repeat(129), false),
    ];
    for (index, (uri, served)) in cases.iter().enumerate() {
        let case = format!("{} bytes", uri.len());
        let item_source = json!({"source_id": "s1", "source_kind": "caller", "uri": uri});
        let member = with(
            item(&format!("u{index}"), &format!("c{index}")),
            json!({"source": item_source}),
        );
        let (status, answer) = rig
            .post(&request(&format!("uri-{index}"), vec![member]))
            .await;
        if *served {
            assert_eq!(status, 200, "source {case}: {answer}");
        } else {
            assert_invalid(status, &answer, "source", &case);
        }
        let entry = json!([{"source_id": "p", "source_kind": "k", "uri": uri}]);
        let member = with(
            item(&format!("pu{index}"), &format!("pc{index}")),
            json!({"provenance": entry}),
        );
        let (status, answer) = rig
            .post(&request(&format!("puri-{index}"), vec![member]))
            .await;
        if *served {
            assert_eq!(status, 200, "provenance {case}: {answer}");
        } else {
            assert_invalid(status, &answer, "provenance", &case);
        }
    }
}

// ---------------------------------------------------------------- identity

/// Contract section 3: the identity is `(binding, exact scope, operation,
/// key)`. Three authorized scopes sharing a key and a payload stay three
/// executions with three receipts, each in its own effective scope. Pins
/// g1r-identity-exact-scope.
#[tokio::test]
async fn each_exact_scope_is_its_own_identity() {
    let scopes = [
        json!({"agent": "a"}),
        json!({"agent": "a", "user": "u"}),
        json!({"agent": "a", "user": "u", "session": "s"}),
    ];
    let effective = [
        json!({"tenant": "@root", "agent": "a"}),
        json!({"tenant": "@root", "agent": "a", "user": "u"}),
        json!({"tenant": "@root", "agent": "a", "user": "u", "session": "s"}),
    ];
    let rig = Rig::new(&Spec {
        scopes: scopes.to_vec(),
        ..Spec::default()
    });
    let mut receipts: Vec<Value> = Vec::new();
    for (scope, effective) in scopes.iter().zip(&effective) {
        let body = with(
            request("shared", vec![item("e1", "Ada")]),
            json!({"scope": scope}),
        );
        let (status, answer) = rig.post(&body).await;
        assert_eq!(status, 200, "{scope}: {answer}");
        assert_eq!(answer["replayed"], false, "{scope}");
        assert_eq!(answer["outcomes"], json!([created("e1")]), "{scope}");
        assert_eq!(answer["receipt"]["scope"], *effective, "{scope}");
        receipts.push(answer["receipt"].clone());
    }
    let ids: Vec<&Value> = receipts.iter().map(|receipt| &receipt["id"]).collect();
    assert!(
        ids[0] != ids[1] && ids[1] != ids[2] && ids[0] != ids[2],
        "{ids:?}"
    );
    for (scope, receipt) in scopes.iter().zip(&receipts) {
        let body = with(
            request("shared", vec![item("e1", "Ada")]),
            json!({"scope": scope}),
        );
        let (status, again) = rig.post(&body).await;
        assert_eq!(status, 200, "{scope}: {again}");
        assert_eq!(again["replayed"], true, "{scope}");
        assert_eq!(again["receipt"], *receipt, "{scope}");
    }
}

/// Contract section 3: `deadline_ms` is excluded from the request identity,
/// so a retry with another budget replays the original receipt. Pins
/// g1r-identity-deadline-key.
#[tokio::test]
async fn the_deadline_budget_is_not_part_of_the_request_identity() {
    let rig = Rig::new(&Spec::default());
    let base = request("k", vec![item("e1", "Ada")]);
    let (status, first) = rig
        .post(&with(base.clone(), json!({"deadline_ms": 250})))
        .await;
    assert_eq!(status, 200, "{first}");
    assert_eq!(first["replayed"], false);
    for budget in [
        json!({"deadline_ms": 5000}),
        json!({"deadline_ms": 1}),
        json!({}),
    ] {
        let (status, again) = rig.post(&with(base.clone(), budget.clone())).await;
        assert_eq!(status, 200, "{budget}: {again}");
        assert_eq!(again["replayed"], true, "{budget}");
        assert_eq!(again["receipt"], first["receipt"], "{budget}");
        assert_eq!(again["outcomes"], first["outcomes"], "{budget}");
    }
}

/// Contract section 3: the key is part of the identity, so different keys
/// are independent executions with their own receipts, whatever they carry.
/// Pins g1r-identity-distinct-keys.
#[tokio::test]
async fn distinct_keys_are_independent_receipts() {
    let rig = Rig::new(&Spec::default());
    let (status, first) = rig.post(&request("k1", vec![item("e1", "Ada")])).await;
    assert_eq!(status, 200, "{first}");
    let (status, second) = rig.post(&request("k2", vec![item("e2", "Grace")])).await;
    assert_eq!(status, 200, "{second}");
    assert_eq!(second["replayed"], false);
    assert_eq!(second["outcomes"], json!([created("e2")]));
    // The payload of the first key under a new key is a fresh execution whose
    // outcome is the exact-dedup no-op, not a replay.
    let (status, third) = rig.post(&request("k3", vec![item("e1", "Ada")])).await;
    assert_eq!(status, 200, "{third}");
    assert_eq!(third["replayed"], false);
    assert_eq!(
        third["outcomes"],
        json!([{"id": "e1", "action": "noop", "revision": "1"}])
    );
    let ids = [
        &first["receipt"]["id"],
        &second["receipt"]["id"],
        &third["receipt"]["id"],
    ];
    assert!(
        ids[0] != ids[1] && ids[1] != ids[2] && ids[0] != ids[2],
        "{ids:?}"
    );
}

/// Contract section 3 and mapping section 3: the receipt names the stored
/// commit sequence, its ID is the accessor's value for that stored revision
/// (never a predicted one), and a replay at a later clock keeps both and the
/// original retry window. Pins g1r-receipt-predicted-revision,
/// g1r-receipt-sequence-offset and g1r-receipt-refresh-window.
#[tokio::test]
async fn the_receipt_names_the_stored_commit_sequence_and_window() {
    let rig = Rig::new(&Spec::default());
    let scope = MemoryScope {
        tenant: "@root".into(),
        agent: "a".into(),
        user: None,
        session: None,
    };
    let id = |key: &str, sequence: u64| {
        receipt_id(ReceiptOperation::Remember, &scope, key, sequence).unwrap()
    };
    let (status, first) = rig.post(&request("k1", vec![item("e1", "Ada")])).await;
    assert_eq!(status, 200, "{first}");
    let sequence = rig.sequence();
    assert_eq!(
        first["receipt"],
        json!({
            "id": id("k1", sequence),
            "operation": "remember",
            "scope": {"tenant": "@root", "agent": "a"},
            "sequence": sequence.to_string(),
            "retry_until_ms": "60100",
            "digest_version": "memory-request-v1",
        })
    );
    // The recall view of the committed state reports the same sequence.
    let (status, recalled) = rig.recall("e1").await;
    assert_eq!(status, 200, "{recalled}");
    assert_eq!(recalled["meta"]["sequence"], sequence.to_string());
    // Another commit moves the database on; the first receipt does not move.
    let (status, second) = rig.post(&request("k2", vec![item("e2", "Grace")])).await;
    assert_eq!(status, 200, "{second}");
    let later = rig.sequence();
    assert!(later > sequence, "{later} {sequence}");
    assert_eq!(second["receipt"]["sequence"], later.to_string());
    assert_eq!(second["receipt"]["id"], id("k2", later));
    rig.at(5_000);
    let (status, replay) = rig.post(&request("k1", vec![item("e1", "Ada")])).await;
    assert_eq!(status, 200, "{replay}");
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["receipt"], first["receipt"]);
    assert_eq!(rig.sequence(), later, "a replay commits nothing");
}

/// Contract sections 3-4: the deadline is a relative budget (default 250 ms,
/// at most 5,000 ms) over the total elapsed time; the client's budget wins and
/// is never extended; an expired request leaves nothing behind and a live
/// matching replay precedes the check. Pins g1r-limit-deadline-default,
/// g1r-limit-deadline-max and g1r-identity-elapsed-deadline.
#[tokio::test]
async fn the_deadline_is_a_relative_budget_over_total_elapsed_time() {
    let rig = Rig::new(&Spec::default());
    // (elapsed ms, deadline_ms, served)
    let cases: [(u64, Option<u64>, bool); 11] = [
        (0, None, true),
        (249, None, true),
        (250, None, false),
        (251, None, false),
        (250, Some(251), true),
        (100, Some(100), false),
        (100, Some(101), true),
        (0, Some(1), true),
        (4_999, Some(5_000), true),
        (5_000, Some(5_000), false),
        (6_000, Some(5_000), false),
    ];
    for (index, (elapsed, budget, served)) in cases.into_iter().enumerate() {
        rig.elapsed(elapsed);
        let mut body = request(
            &format!("d-{index}"),
            vec![item(&format!("m{index}"), &format!("c{index}"))],
        );
        if let Some(budget) = budget {
            body["deadline_ms"] = json!(budget);
        }
        let (status, answer) = rig.post(&body).await;
        let case = format!("elapsed {elapsed} budget {budget:?}");
        if served {
            assert_eq!(status, 200, "{case}: {answer}");
            assert_eq!(
                answer["outcomes"],
                json!([created(&format!("m{index}"))]),
                "{case}"
            );
        } else {
            assert_code(status, &answer, (504, "deadline_exceeded"), &case);
        }
    }
    // The request of case 2 expired before its commit: nothing was written,
    // so the same key executes afresh once time allows.
    rig.elapsed(0);
    let retry = request("d-2", vec![item("m2", "c2")]);
    let (status, answer) = rig.post(&retry).await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["replayed"], false);
    assert_eq!(answer["outcomes"], json!([created("m2")]));
    // A live matching replay precedes the deadline check.
    rig.elapsed(9_000);
    let (status, answer) = rig.post(&retry).await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["replayed"], true);
}

// ---------------------------------------------------------------- refusals

/// Mapping section 1: fields with no first-slice mapping are refused by name
/// (409 `memory_capability_unavailable`) before their values are read, never
/// silently dropped or recast as unknown fields. Pins g1r-refusal-metadata,
/// g1r-refusal-assertion, g1r-refusal-confidence and g1r-refusal-reference-one.
#[tokio::test]
async fn unmapped_item_fields_are_refused_by_name_before_their_values_are_read() {
    let rig = Rig::new(&Spec::default());
    let assertion = json!({
        "subject_id": "e1",
        "predicate": "p",
        "slot_key": "k",
        "destination_id": "e2",
        "interval": {"valid_from_ms": null, "valid_to_ms": null},
        "slot_expected": {"absent": true},
    });
    let cases: Vec<(&str, Value, &str)> = vec![
        ("fact", json!({"kind": "fact"}), "fact_kinds"),
        ("relation", json!({"kind": "relation"}), "fact_kinds"),
        (
            "fact with invalid members",
            json!({"kind": "fact", "content": 7, "observed_at_ms": "x"}),
            "fact_kinds",
        ),
        ("confidence", json!({"confidence": 0.5}), "confidence"),
        (
            "confidence out of range",
            json!({"confidence": 9}),
            "confidence",
        ),
        ("confidence null", json!({"confidence": null}), "confidence"),
        (
            "pinned reference",
            json!({"references": [{"id": "e2", "revision": "1"}]}),
            "reference_revision_pin",
        ),
        (
            "pinned integer reference",
            json!({"references": [{"id": "e2", "revision": 1}]}),
            "reference_revision_pin",
        ),
        (
            "pinned reference to a stranger",
            json!({"references": [{"id": "elsewhere", "revision": "2"}]}),
            "reference_revision_pin",
        ),
        (
            "evidence",
            json!({"evidence": [{"collection": "notes_ev", "record": "1"}]}),
            "evidence",
        ),
        ("metadata", json!({"metadata": {}}), "metadata"),
        (
            "metadata scalars",
            json!({"metadata": {"k": {"type": "string", "value": "v"}}}),
            "metadata",
        ),
        ("metadata null", json!({"metadata": null}), "metadata"),
        ("assertion", json!({"assertion": {}}), "assertion"),
        (
            "assertion body",
            json!({"assertion": assertion}),
            "assertion",
        ),
        ("assertion null", json!({"assertion": null}), "assertion"),
        (
            "expected absent",
            json!({"expected": {"absent": true}}),
            "expected",
        ),
        (
            "expected revision",
            json!({"expected": {"revision": "1"}}),
            "expected",
        ),
        (
            "expected beside merge none",
            json!({"expected": {"revision": "1"}, "merge": "none"}),
            "expected",
        ),
    ];
    for (name, extra, feature) in cases {
        let body = request("rf", vec![with(item("e1", "x"), extra)]);
        let (status, answer) = rig.post(&body).await;
        assert_refused(status, &answer, feature, name);
    }
    let named = json!([{"collection": "notes_ev", "record": "1"}]);
    let body = with(
        request("rf", vec![item("e1", "x")]),
        json!({"evidence": named}),
    );
    let (status, answer) = rig.post(&body).await;
    assert_refused(status, &answer, "evidence", "request evidence");
    // Nothing was written by any refused request.
    let (status, absent) = rig.recall("e1").await;
    assert_code(status, &absent, (404, "memory_not_found"), "nothing stored");
}

/// Mapping section 1 and contract section 4: `entity` and `episodic` items
/// are served; any other kind value is a 422 on `kind`. Pins
/// g1r-kind-episodic-refused.
#[tokio::test]
async fn entity_and_episodic_kinds_are_served() {
    let rig = Rig::new(&Spec::default());
    for kind in ["entity", "episodic"] {
        let member = with(item(&format!("{kind}-1"), kind), json!({"kind": kind}));
        let (status, answer) = rig.post(&request(&format!("k-{kind}"), vec![member])).await;
        assert_eq!(status, 200, "{kind}: {answer}");
        assert_eq!(
            answer["outcomes"],
            json!([created(&format!("{kind}-1"))]),
            "{kind}"
        );
    }
    for kind in [
        json!("Entity"),
        json!("episode"),
        json!(""),
        json!("fact "),
        json!(5),
        Value::Null,
    ] {
        let member = with(item("bad-kind", "x"), json!({"kind": kind}));
        let (status, answer) = rig.post(&request("k-bad", vec![member])).await;
        assert_invalid(status, &answer, "kind", &kind.to_string());
    }
}

// ----------------------------------------------------------------- privacy

/// Contract section 6: errors never carry request bodies, keys, identifiers
/// or memory text, whatever the failure. Pins g1r-privacy-json-error-content.
#[tokio::test]
async fn errors_never_echo_request_content() {
    let rig = Rig::new(&Spec::default());
    let needle = "sentinel-7f3a9c";
    let raw: Vec<(&str, Vec<u8>)> = vec![
        (
            "truncated",
            format!(
                r#"{{"scope":{{"agent":"a"}},"idempotency_key":"{needle}-key","items":[{{"id":"{needle}-id","content":"{needle}-text"#
            )
            .into_bytes(),
        ),
        (
            "duplicate",
            format!(
                r#"{{"scope":{{"agent":"a"}},"idempotency_key":"{needle}-one","idempotency_key":"{needle}-two"}}"#
            )
            .into_bytes(),
        ),
        (
            "trailing",
            format!(r#"{{"scope":{{"agent":"a"}}}} {needle}"#).into_bytes(),
        ),
        ("bare", needle.as_bytes().to_vec()),
        (
            "not utf-8",
            [b"{\"x\":\"".as_slice(), needle.as_bytes(), &[0xff], b"\"}"].concat(),
        ),
    ];
    for (case, bytes) in raw {
        let (status, answer) = rig.send(REMEMBER, bytes).await;
        assert_eq!(status, 400, "{case}: {answer}");
        assert!(!answer.to_lowercase().contains(needle), "{case}: {answer}");
        assert_eq!(parse(&answer)["error"]["code"], "invalid_json", "{case}");
    }
    let mut unknown = request("k", vec![item("e1", "x")]);
    unknown[needle] = json!(1);
    let mut controlled = request("k", vec![item("e1", "x")]);
    controlled["scope"] = json!({"agent": format!("{needle}\u{1}")});
    let mut ungranted = request("k", vec![item("e1", "x")]);
    ungranted["scope"] = json!({"agent": needle});
    let bodies = [
        (
            "kind",
            request(
                needle,
                vec![with(item(needle, needle), json!({"kind": needle}))],
            ),
        ),
        ("unknown member", unknown),
        ("control character", controlled),
        ("ungranted scope", ungranted),
        (
            "refused field",
            request(
                needle,
                vec![with(
                    item(needle, needle),
                    json!({"metadata": {needle: needle}}),
                )],
            ),
        ),
    ];
    for (case, body) in bodies {
        let (status, answer) = rig.post(&body).await;
        assert!(
            status == 403 || status == 409 || status == 422,
            "{case}: {status}"
        );
        assert!(
            !answer.to_string().to_lowercase().contains(needle),
            "{case}: {answer}"
        );
    }
    // A conflicting retry names neither the key nor the text.
    let first = request(needle, vec![item(needle, needle)]);
    assert_eq!(rig.post(&first).await.0, 200);
    let (status, answer) = rig
        .post(&request(needle, vec![item(needle, "other")]))
        .await;
    assert_code(status, &answer, (409, "idempotency_conflict"), "conflict");
    assert!(
        !answer.to_string().to_lowercase().contains(needle),
        "{answer}"
    );
    let (status, answer) = rig
        .post_to(&format!("/v1/memory/{needle}/remember"), &first)
        .await;
    assert_forbidden(status, &answer, "unknown binding");
    assert!(
        !answer.to_string().to_lowercase().contains(needle),
        "{answer}"
    );
}

// ----------------------------------------------------------- field forwarding

type Change = fn(&mut Value);

/// Contract section 3: the core digest covers each item's content, kind,
/// source, provenance, references, importance, observation and expiry; each
/// of them, changed alone under a retained key, is a 409
/// `idempotency_conflict`, and the unchanged request still replays.
#[tokio::test]
async fn every_hashed_item_field_reaches_the_request_digest() {
    let rig = Rig::new(&Spec::default());
    let plain = |key: &str| {
        let first = with(
            item("e1", "Ada"),
            json!({
                "importance": 0.25,
                "expires_at_ms": "5000",
                "source": {
                    "source_id": "s1",
                    "source_kind": "caller",
                    "uri": "u://x",
                    "transformation_id": "t1",
                },
            }),
        );
        request(key, vec![first, item("e2", "Grace")])
    };
    let explicit = |key: &str| {
        let first = with(
            item("e1", "Ada"),
            json!({
                "provenance": [
                    {"source_id": "p1", "source_kind": "k1", "uri": "u://p", "transformation_id": "t2"},
                    {"source_id": "p2", "source_kind": "k2"},
                ],
                "references": [{"id": "e2", "revision": "0"}, {"id": "e3", "revision": "0"}],
            }),
        );
        request(key, vec![first, item("e2", "Grace"), item("e3", "Hopper")])
    };
    let plain_changes: [(&str, Change); 18] = [
        ("content", |b| b["items"][0]["content"] = json!("Ada!")),
        ("kind", |b| b["items"][0]["kind"] = json!("episodic")),
        ("id", |b| b["items"][0]["id"] = json!("e1b")),
        ("observation", |b| {
            b["items"][0]["observed_at_ms"] = json!("89")
        }),
        ("importance", |b| b["items"][0]["importance"] = json!(0.75)),
        ("default importance", |b| {
            b["items"][0].as_object_mut().unwrap().remove("importance");
        }),
        ("expiry", |b| b["items"][0]["expires_at_ms"] = json!("5001")),
        ("no expiry", |b| {
            b["items"][0]
                .as_object_mut()
                .unwrap()
                .remove("expires_at_ms");
        }),
        ("source id", |b| {
            b["items"][0]["source"]["source_id"] = json!("s2")
        }),
        ("source kind", |b| {
            b["items"][0]["source"]["source_kind"] = json!("agent");
        }),
        ("source uri", |b| {
            b["items"][0]["source"]["uri"] = json!("u://y")
        }),
        ("no source uri", |b| {
            b["items"][0]["source"]
                .as_object_mut()
                .unwrap()
                .remove("uri");
        }),
        ("source transformation", |b| {
            b["items"][0]["source"]["transformation_id"] = json!("t9");
        }),
        ("no source transformation", |b| {
            b["items"][0]["source"]
                .as_object_mut()
                .unwrap()
                .remove("transformation_id");
        }),
        ("item order", |b| {
            b["items"].as_array_mut().unwrap().swap(0, 1)
        }),
        ("fewer items", |b| {
            b["items"].as_array_mut().unwrap().pop();
        }),
        ("more items", |b| {
            b["items"]
                .as_array_mut()
                .unwrap()
                .push(item("e3", "Hopper"));
        }),
        ("a reference", |b| {
            b["items"][0]["references"] = json!([{"id": "e2", "revision": "0"}]);
        }),
    ];
    let explicit_changes: [(&str, Change); 14] = [
        ("provenance source", |b| {
            b["items"][0]["provenance"][0]["source_id"] = json!("p9");
        }),
        ("provenance kind", |b| {
            b["items"][0]["provenance"][0]["source_kind"] = json!("k9");
        }),
        ("provenance uri", |b| {
            b["items"][0]["provenance"][0]["uri"] = json!("u://q");
        }),
        ("no provenance uri", |b| {
            b["items"][0]["provenance"][0]
                .as_object_mut()
                .unwrap()
                .remove("uri");
        }),
        ("provenance transformation", |b| {
            b["items"][0]["provenance"][0]["transformation_id"] = json!("t9");
        }),
        ("no provenance transformation", |b| {
            b["items"][0]["provenance"][0]
                .as_object_mut()
                .unwrap()
                .remove("transformation_id");
        }),
        ("provenance order", |b| {
            b["items"][0]["provenance"]
                .as_array_mut()
                .unwrap()
                .swap(0, 1);
        }),
        ("fewer provenance entries", |b| {
            b["items"][0]["provenance"].as_array_mut().unwrap().pop();
        }),
        ("more provenance entries", |b| {
            b["items"][0]["provenance"]
                .as_array_mut()
                .unwrap()
                .push(json!({"source_id": "p3", "source_kind": "k3"}));
        }),
        ("source of the item", |b| {
            b["items"][0]["source"]["source_id"] = json!("s2");
        }),
        ("reference order", |b| {
            b["items"][0]["references"]
                .as_array_mut()
                .unwrap()
                .swap(0, 1);
        }),
        ("fewer references", |b| {
            b["items"][0]["references"].as_array_mut().unwrap().pop();
        }),
        ("another reference", |b| {
            b["items"][0]["references"] =
                json!([{"id": "e3", "revision": "0"}, {"id": "e3", "revision": "0"}]);
        }),
        ("no references", |b| {
            b["items"][0].as_object_mut().unwrap().remove("references");
        }),
    ];
    let mut index = 0;
    for (changes, build) in [
        (&plain_changes[..], &plain as &dyn Fn(&str) -> Value),
        (&explicit_changes[..], &explicit as &dyn Fn(&str) -> Value),
    ] {
        for (name, change) in changes {
            index += 1;
            let key = format!("f-{index}");
            let (status, first) = rig.post(&build(&key)).await;
            assert_eq!(status, 200, "{name}: {first}");
            let mut changed = build(&key);
            change(&mut changed);
            let (status, answer) = rig.post(&changed).await;
            assert_code(status, &answer, (409, "idempotency_conflict"), name);
            let (status, again) = rig.post(&build(&key)).await;
            assert_eq!(status, 200, "{name}: {again}");
            assert_eq!(again["replayed"], true, "{name}");
            assert_eq!(again["receipt"], first["receipt"], "{name}");
        }
    }
}

// ------------------------------------------------------------ core failures

/// Mapping section 2: a core failure reaches the wire with its mapped status
/// and code, and a failed request writes nothing.
#[tokio::test]
async fn core_failures_reach_the_wire_with_their_mapped_codes() {
    let rig = Rig::new(&Spec::default());
    let (status, first) = rig.post(&request("k", vec![item("e1", "Ada")])).await;
    assert_eq!(status, 200, "{first}");
    let before = rig.sequence();
    let merged = |key: &str, content: &str, revision: &str| {
        let member = with(
            item("e1", content),
            json!({
                "source": {"source_id": "s2", "source_kind": "caller"},
                "merge": "merge_provenance",
                "expected": {"revision": revision},
            }),
        );
        request(key, vec![member])
    };
    let cases = [
        (
            "the same ID with other content",
            request("c1", vec![item("e1", "Other")]),
            (412, "precondition_failed"),
        ),
        (
            "a merge into an unknown memory",
            request(
                "c2",
                vec![with(
                    item("zz", "Ada"),
                    json!({
                        "source": {"source_id": "s2", "source_kind": "caller"},
                        "merge": "merge_provenance",
                        "expected": {"revision": "1"},
                    }),
                )],
            ),
            (404, "memory_not_found"),
        ),
        (
            "a merge with another content",
            merged("c3", "Different", "1"),
            (412, "precondition_failed"),
        ),
        (
            "a merge at a stale revision",
            merged("c4", "Ada", "7"),
            (412, "precondition_failed"),
        ),
        (
            "an observation after the clock",
            request(
                "c5",
                vec![with(item("e9", "x"), json!({"observed_at_ms": "101"}))],
            ),
            (422, "invalid_input"),
        ),
        (
            "a repeated item ID",
            request("c6", vec![item("e8", "x"), item("e8", "y")]),
            (422, "invalid_input"),
        ),
    ];
    for (name, body, expected) in cases {
        let (status, answer) = rig.post(&body).await;
        assert_code(status, &answer, expected, name);
    }
    assert_eq!(rig.sequence(), before, "a failed request commits nothing");
    // The control: the merge at the stored revision is served.
    let (status, answer) = rig.post(&merged("c7", "Ada", "1")).await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(
        answer["outcomes"],
        json!([{"id": "e1", "action": "merged", "revision": "2"}])
    );
}

/// Contract section 8: `MemoryItem.provenance` has no minimum length and
/// "sources default provenance to [source]". Whatever an explicit empty list
/// means, an accepted write must stay readable: the Recall of the memory never
/// answers 500 and presents every provenance entry with its required
/// `source_kind` (the core's own default entry for an empty list has none).
#[tokio::test]
async fn an_explicit_empty_provenance_list_never_makes_the_memory_unreadable() {
    let rig = Rig::new(&Spec::default());
    let member = with(item("e1", "Ada"), json!({"provenance": []}));
    let (status, answer) = rig.post(&request("k", vec![member])).await;
    if status != 200 {
        assert_invalid(status, &answer, "provenance", "refused empty list");
        return;
    }
    let (status, recalled) = rig.recall("e1").await;
    assert_eq!(status, 200, "{recalled}");
    let memory = recalled["memories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|memory| memory["id"] == "e1")
        .unwrap();
    for entry in memory["provenance"].as_array().unwrap() {
        let kind = entry["source_kind"].as_str();
        assert!(kind.is_some_and(|kind| !kind.is_empty()), "{entry}");
    }
}

/// Contract section 3: hostile nesting is a 400 (the parser's depth limit),
/// never an exhausted stack; nesting inside the limit is an ordinary shape
/// error of its member.
#[tokio::test]
async fn deeply_nested_bodies_are_refused_without_exhausting_the_stack() {
    let rig = Rig::new(&Spec::default());
    for depth in [200_usize, 100_000] {
        let (status, text) = rig.send(REMEMBER, "[".repeat(depth).into_bytes()).await;
        assert_eq!(status, 400, "{depth}: {text}");
        assert_eq!(parse(&text)["error"]["code"], "invalid_json", "{depth}");
    }
    let nested = format!("{}1{}", "[".repeat(100), "]".repeat(100));
    let body = format!(
        r#"{{"scope":{{"agent":"a"}},"idempotency_key":"k","items":[{{"id":"e1","kind":"entity","content":{nested},"observed_at_ms":"90","source":{{"source_id":"s","source_kind":"c"}}}}]}}"#
    );
    let (status, answer) = rig.post_text(&body).await;
    assert_invalid(status, &answer, "content", "nested content");
}

// ---------------------------------------------------- binding configuration

const BINDINGS: &str = r#"{"version":"memory-bindings-v1","bindings":[{"binding":"notes","tenant":"@root","generation":1,"state":"active","graph":"notes_graph","evidence_collection":"notes_ev","schema_version":"memory-schema-v1","ranking_version":"memory-ranking-v1","digest_version":"memory-request-v1","receipt_window_ms":86400000,"grants":[{"principal":"abcdefghij27","scopes":[{"agent":"a"}]}]}]}"#;

/// Contract section 3 decision 70 and mapping section 4: the receipt window is
/// 1..=2,592,000,000 ms and the bindings file at most 1 MiB, both inclusive.
/// Pins g1r-config-window-max-plus-one and g1r-config-file-byte-boundary.
#[test]
fn the_receipt_window_and_the_file_cap_are_the_policy_literals() {
    for (window, served) in [
        (0_u64, false),
        (1, true),
        (2_592_000_000, true),
        (2_592_000_001, false),
    ] {
        let document = BINDINGS.replace("86400000", &window.to_string());
        assert_eq!(
            parse_bindings(document.as_bytes()).is_ok(),
            served,
            "{window}"
        );
    }
    let padded = |bytes: usize| format!("{BINDINGS}{}", " ".repeat(bytes - BINDINGS.len()));
    assert!(parse_bindings(padded(1_048_576).as_bytes()).is_ok());
    assert_eq!(
        parse_bindings(padded(1_048_577).as_bytes()).err(),
        Some(BindingError::TooLarge)
    );
}
