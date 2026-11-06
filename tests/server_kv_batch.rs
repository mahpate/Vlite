#![cfg(feature = "server")]
//! `POST …/kv/batch` and `POST …/kv/entries/get` over REST: the wire shape, the
//! checks that come before any write, the engine errors, the page bound of
//! get-many, scopes, tenancy, quota, audit and the capabilities.

use axum::{Router, body::Body, http::Request};
use base64::{Engine, prelude::BASE64_STANDARD};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use vlite::{
    CollectionSpec, CreateOptions, Database, FormatVersion,
    kv::{KvPut, first_slot},
    server::{BearerToken, ServerOptions, Service, configured_admin_router},
};

const ADMIN: &str = "fake_kv_batch_admin_token_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const READ: &str = "fake_kv_batch_reader_token_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const BATCH: &str = "/v1/collections/default/kv/batch";
const MANY: &str = "/v1/collections/default/kv/entries/get";
const ENTRY: &str = "/v1/collections/default/kv/entry";
const PAGE: usize = 16_777_216;
const MAX_VALUE: usize = 1_047_290;
// Namespace "n" keys that share the first slot u64::MAX - 2 (see tests/kv.rs).
const SHARED: [&str; 4] = [
    "wrap-1e3h8xptc3rmwn",
    "wrap-1mr1oejil0ss3d",
    "wrap-rcxazt3ti43gt3",
    "wrap-zh8a7z15oq0wpx",
];

fn database_of(format: FormatVersion) -> Database {
    let mut db = Database::create_in_memory(
        1,
        CreateOptions {
            format: Some(format),
            ..Default::default()
        },
    )
    .unwrap();
    if format == FormatVersion::V5 {
        db.create_collection(
            "wide",
            CollectionSpec {
                dimensions: 2,
                settings: Default::default(),
            },
        )
        .unwrap();
    }
    db
}

fn database() -> Database {
    database_of(FormatVersion::V5)
}

fn start(db: Database, directory: &std::path::Path) -> Router {
    configured_admin_router(
        Service::new(db),
        ServerOptions {
            admin_token: Some(BearerToken::new(ADMIN).unwrap()),
            read_only_token: Some(BearerToken::new(READ).unwrap()),
            admin_directory: Some(directory.into()),
            ..Default::default()
        },
    )
    .unwrap()
}

async fn send(
    app: &Router,
    token: &str,
    method: &str,
    uri: &str,
    body: String,
    content_type: Option<&str>,
) -> (u16, Vec<u8>) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"));
    if let Some(content_type) = content_type {
        request = request.header("content-type", content_type);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, bytes.to_vec())
}

async fn raw(
    app: &Router,
    token: &str,
    method: &str,
    uri: &str,
    body: String,
) -> (u16, Value, usize) {
    let (status, bytes) = send(app, token, method, uri, body, Some("application/json")).await;
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value, bytes.len())
}

async fn call(app: &Router, token: &str, method: &str, uri: &str, body: Value) -> (u16, Value) {
    let text = if body.is_null() {
        String::new()
    } else {
        body.to_string()
    };
    let (status, value, _) = raw(app, token, method, uri, text).await;
    (status, value)
}

fn b64(bytes: &[u8]) -> String {
    BASE64_STANDARD.encode(bytes)
}

fn put(key: &str) -> Value {
    json!({"namespace":"n","key":key,"value_base64":"YQ=="})
}

fn put_op(key: &str, value: &[u8]) -> Value {
    json!({"op":"put","namespace":"n","key":key,"value_base64":b64(value)})
}

fn delete_op(key: &str) -> Value {
    json!({"op":"delete","namespace":"n","key":key})
}

fn entry(key: &str) -> String {
    format!("{ENTRY}?namespace=n&key={key}")
}

fn version_of(value: &Value) -> u64 {
    let text = value.as_str().unwrap();
    let parsed = text.parse::<u64>().unwrap();
    assert!(parsed > 0);
    assert_eq!(parsed.to_string(), text);
    parsed
}

async fn batch(app: &Router, operations: Vec<Value>) -> (u16, Value) {
    call(
        app,
        ADMIN,
        "POST",
        BATCH,
        json!({ "operations": operations }),
    )
    .await
}

async fn many(app: &Router, keys: &[(&str, &str)]) -> (u16, Value) {
    let keys: Vec<Value> = keys
        .iter()
        .map(|(namespace, key)| json!({"namespace":namespace,"key":key}))
        .collect();
    call(app, ADMIN, "POST", MANY, json!({ "keys": keys })).await
}

/// The values of `n/keys` as the get-many route answers them.
async fn values(app: &Router, keys: &[&str]) -> Vec<Option<String>> {
    let pairs: Vec<(&str, &str)> = keys.iter().map(|key| ("n", *key)).collect();
    let (status, page) = many(app, &pairs).await;
    assert_eq!(status, 200, "{page}");
    assert_eq!(page["next_index"], Value::Null);
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["value_base64"].as_str().map(str::to_owned))
        .collect()
}

async fn head(app: &Router) -> String {
    let (status, feed) = call(app, ADMIN, "GET", "/v1/changes?limit=1", Value::Null).await;
    assert_eq!(status, 200, "{feed}");
    feed["head"].as_str().unwrap().to_owned()
}

#[track_caller]
fn assert_error(reply: &(u16, Value), status: u16, code: &str) {
    assert_eq!(reply.0, status, "{}", reply.1);
    assert_eq!(reply.1["error"]["code"], code, "{}", reply.1);
}

fn message(reply: &(u16, Value)) -> &str {
    reply.1["error"]["message"].as_str().unwrap_or_default()
}

async fn credential(app: &Router, extra: Value) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let mut input = json!({"name":"kv-batch-test","collections":["default"],"operations":["read"],
        "expires_unix_ms":now + 600_000});
    for (key, value) in extra.as_object().unwrap() {
        input[key] = value.clone();
    }
    let (status, body) = call(app, ADMIN, "POST", "/v1/admin/credentials", input).await;
    assert_eq!(status, 201, "{body}");
    body["token"].as_str().unwrap().into()
}

async fn counter(app: &Router) -> u64 {
    let (status, metrics) = call(app, ADMIN, "GET", "/metrics", Value::Null).await;
    assert_eq!(status, 200, "{metrics}");
    metrics["kv_slots_exhausted_total"].as_u64().unwrap()
}

#[tokio::test]
async fn a_batch_commits_every_operation_as_one_commit_with_one_version() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(), directory.path());
    let mut expiring = put("old");
    expiring["expires_at_ms"] = json!("18446744073709551615");
    assert_eq!(call(&app, ADMIN, "PUT", ENTRY, expiring).await.0, 200);
    let (status, gone) = call(&app, ADMIN, "PUT", ENTRY, put("gone")).await;
    assert_eq!(status, 200, "{gone}");
    let last = version_of(&gone["version"]);

    let (status, body) = call(
        &app,
        ADMIN,
        "POST",
        BATCH,
        json!({"operations":[
            {"op":"put","namespace":"n","key":"a","value_base64":"AQID","expected":"absent"},
            {"op":"put","namespace":"n","key":"old","value_base64":"YQ=="},
            {"op":"delete","namespace":"n","key":"gone","expected":"any"},
            {"op":"delete","namespace":"n","key":"never"},
            {"op":"put","namespace":"m","key":"a","value_base64":""},
        ]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    // One commit: the version is the one after the last write.
    let version = (last + 1).to_string();
    assert_eq!(
        body,
        json!({"results":[
            {"version":version},{"version":version},{"deleted":true},
            {"deleted":false},{"version":version}
        ]})
    );
    for (namespace, key, value) in [("n", "a", "AQID"), ("n", "old", "YQ=="), ("m", "a", "")] {
        let uri = format!("{ENTRY}?namespace={namespace}&key={key}");
        let (status, read) = call(&app, ADMIN, "GET", &uri, Value::Null).await;
        assert_eq!(status, 200, "{read}");
        // The batch put cleared the deadline `old` had.
        assert_eq!(
            read,
            json!({"key":key,"value_base64":value,"version":version,"expires_at_ms":null})
        );
    }
    assert_eq!(
        call(&app, ADMIN, "GET", &entry("gone"), Value::Null)
            .await
            .0,
        404
    );
    let (_, next) = call(&app, ADMIN, "PUT", ENTRY, put("next")).await;
    assert_eq!(version_of(&next["version"]), last + 2);
}

#[tokio::test]
async fn a_batch_that_changes_no_record_commits_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(), directory.path());
    let (_, first) = call(&app, ADMIN, "PUT", ENTRY, put("k")).await;
    let last = version_of(&first["version"]);
    let committed = head(&app).await;
    assert_eq!(
        batch(&app, vec![]).await,
        (200, json!({"results":[]})),
        "an empty batch is a successful no-op"
    );
    assert_eq!(
        batch(&app, vec![delete_op("absent"), delete_op("also-absent")]).await,
        (
            200,
            json!({"results":[{"deleted":false},{"deleted":false}]})
        )
    );
    let mut absent = delete_op("absent");
    absent["expected"] = json!("absent");
    assert_eq!(
        batch(&app, vec![absent]).await,
        (200, json!({"results":[{"deleted":false}]}))
    );
    assert_eq!(head(&app).await, committed);
    let (_, next) = call(&app, ADMIN, "PUT", ENTRY, put("next")).await;
    assert_eq!(version_of(&next["version"]), last + 1);
}

#[tokio::test]
async fn a_failed_condition_writes_nothing_and_reveals_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(), directory.path());
    let (_, live) = call(&app, ADMIN, "PUT", ENTRY, put("live")).await;
    let live_version = version_of(&live["version"]);
    let mut due = put("due");
    due["expires_at_ms"] = json!("1");
    assert_eq!(call(&app, ADMIN, "PUT", ENTRY, due).await.0, 200);
    let committed = head(&app).await;

    let conditional = |op: &str, key: &str, expected: Value| {
        let mut operation = if op == "put" {
            put_op(key, b"x")
        } else {
            delete_op(key)
        };
        operation["expected"] = expected;
        operation
    };
    let stale = json!({"revision":(live_version + 1000).to_string()});
    let mut bodies = Vec::new();
    for (case, operations) in [
        (
            "a stale revision on a put",
            vec![
                put_op("first", b"x"),
                conditional("put", "live", stale.clone()),
            ],
        ),
        (
            "absent over a live key",
            vec![
                put_op("first", b"x"),
                conditional("put", "live", json!("absent")),
            ],
        ),
        (
            "a stale revision on a delete",
            vec![
                put_op("first", b"x"),
                conditional("delete", "live", stale.clone()),
            ],
        ),
        (
            "a revision on a key that is absent",
            vec![
                put_op("first", b"x"),
                conditional("put", "nothing-here", stale.clone()),
            ],
        ),
        (
            "a revision on a due key",
            vec![
                put_op("first", b"x"),
                conditional("delete", "due", stale.clone()),
            ],
        ),
        (
            "the failing operation comes first",
            vec![
                conditional("put", "live", stale.clone()),
                put_op("first", b"x"),
            ],
        ),
    ] {
        let reply = batch(&app, operations).await;
        assert_error(&reply, 412, "precondition_failed");
        assert_eq!(message(&reply), "revision precondition failed", "{case}");
        bodies.push((case, reply.1));
        assert_eq!(head(&app).await, committed, "{case}: a commit happened");
        assert_eq!(
            call(&app, ADMIN, "GET", &entry("first"), Value::Null)
                .await
                .0,
            404,
            "{case}: an earlier operation was kept"
        );
    }
    // The message names no operation, revision or presence: every refusal reads alike.
    for (case, body) in &bodies {
        assert_eq!(body, &bodies[0].1, "{case}");
    }

    // The conditions that hold run, and see the entries as they stand.
    let (status, body) = batch(
        &app,
        vec![
            put_op("first", b"x"),
            conditional("put", "live", json!({"revision":live_version.to_string()})),
            conditional("put", "due", json!("absent")),
            conditional("delete", "nothing-here", json!("absent")),
        ],
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let version = body["results"][0]["version"].as_str().unwrap().to_owned();
    assert_eq!(
        body["results"],
        json!([
            {"version":version},{"version":version},{"version":version},{"deleted":false}
        ])
    );
}

#[tokio::test]
async fn the_whole_request_is_checked_before_any_operation_runs() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(), directory.path());
    call(&app, ADMIN, "PUT", ENTRY, put("live")).await;
    let committed = head(&app).await;
    // Operation 0 would fail its condition (412) if it were run first; the
    // request is refused as a whole (422) before that.
    let failing = json!({"op":"put","namespace":"n","key":"live","value_base64":"",
        "expected":{"revision":"999999"}});
    let wire = |operation: Value| json!({"operations":[failing, put_op("fine", b"x"), operation]});
    let cases = [
        (
            "an empty namespace",
            wire(json!({"op":"delete","namespace":"","key":"k"})),
            "invalid_input",
            "operations[2]",
        ),
        (
            "an empty key",
            wire(json!({"op":"delete","namespace":"n","key":""})),
            "invalid_input",
            "operations[2]",
        ),
        (
            "a namespace of 256 bytes",
            wire(json!({"op":"delete","namespace":"x".repeat(256),"key":"k"})),
            "invalid_input",
            "operations[2]",
        ),
        (
            "a key of 1025 bytes",
            wire(json!({"op":"delete","namespace":"n","key":"x".repeat(1025)})),
            "invalid_input",
            "operations[2]",
        ),
        (
            "base64 without padding",
            wire(json!({"op":"put","namespace":"n","key":"k","value_base64":"AQ"})),
            "invalid_input",
            "operations[2]",
        ),
        (
            "base64 with whitespace",
            wire(json!({"op":"put","namespace":"n","key":"k","value_base64":"AQID\n"})),
            "invalid_input",
            "operations[2]",
        ),
        (
            "url-safe base64",
            wire(json!({"op":"put","namespace":"n","key":"k","value_base64":"-_-_"})),
            "invalid_input",
            "operations[2]",
        ),
        (
            "an expectation that is not any or absent",
            wire(json!({"op":"delete","namespace":"n","key":"k","expected":"later"})),
            "invalid_input",
            "operations[2]",
        ),
        (
            "a revision beyond u64",
            wire(json!({"op":"delete","namespace":"n","key":"k",
                "expected":{"revision":"18446744073709551616"}})),
            "invalid_input",
            "operations[2]",
        ),
        (
            "a value of one byte more than a key-value entry takes",
            wire(json!({"op":"put","namespace":"n","key":"k",
                "value_base64":b64(&vec![0; MAX_VALUE + 1])})),
            "invalid_input",
            "operations[2]",
        ),
        (
            "a key named twice",
            wire(put_op("fine", b"y")),
            "invalid_input",
            "operations[2]: names the same namespace and key as operations[1]",
        ),
        (
            "a key named twice, by a put and a delete",
            wire(delete_op("live")),
            "invalid_input",
            "operations[2]: names the same namespace and key as operations[0]",
        ),
    ];
    for (case, body, code, expected) in cases {
        let reply = call(&app, ADMIN, "POST", BATCH, body).await;
        assert_error(&reply, 422, code);
        assert!(message(&reply).starts_with(expected), "{case}: {reply:?}");
        assert_eq!(head(&app).await, committed, "{case}: a commit happened");
    }
    // The count is checked first, without naming an operation.
    let many_ops: Vec<Value> = (0..1001).map(|i| delete_op(&format!("k{i}"))).collect();
    let reply = batch(&app, many_ops).await;
    assert_error(&reply, 422, "invalid_input");
    assert_eq!(message(&reply), "a batch takes at most 1000 operations");
    assert_eq!(head(&app).await, committed);
}

#[tokio::test]
async fn the_request_bounds_are_exact() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(), directory.path());

    // 1,000 operations is the most; their results are in order and share a version.
    let keys: Vec<String> = (0..1000).map(|i| format!("k{i:04}")).collect();
    let operations: Vec<Value> = keys.iter().map(|key| put_op(key, b"v")).collect();
    let (status, body) = batch(&app, operations).await;
    assert_eq!(status, 200);
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 1000);
    assert!(results.iter().all(|result| result == &results[0]));
    // The same 1,000 keys are the most one get-many takes, and all are live.
    let pairs: Vec<(&str, &str)> = keys.iter().map(|key| ("n", key.as_str())).collect();
    let (status, page) = many(&app, &pairs).await;
    assert_eq!(status, 200);
    assert_eq!(page["items"].as_array().unwrap().len(), 1000);
    assert_eq!(page["next_index"], Value::Null);
    assert!(
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["version"] == results[0]["version"])
    );
    let mut more = pairs.clone();
    more.push(("n", "one-more"));
    let reply = many(&app, &more).await;
    assert_error(&reply, 422, "invalid_input");
    assert_eq!(message(&reply), "a get-many takes at most 1000 keys");

    // The values of a batch total 4,194,304 bytes at most.
    let committed = head(&app).await;
    let part = vec![7u8; 1_000_000];
    let tail = |extra: usize| vec![9u8; 194_304 + extra];
    let batch_of = |last: Vec<u8>| {
        vec![
            put_op("v0", &part),
            put_op("v1", &part),
            put_op("v2", &part),
            put_op("v3", &part),
            put_op("v4", &last),
        ]
    };
    let reply = batch(&app, batch_of(tail(1))).await;
    assert_error(&reply, 422, "invalid_input");
    assert!(message(&reply).starts_with("operations[4]: "), "{reply:?}");
    assert!(message(&reply).contains("4194304"), "{reply:?}");
    assert_eq!(head(&app).await, committed);
    let (status, body) = batch(&app, batch_of(tail(0))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["results"].as_array().unwrap().len(), 5);
    let (status, read) = call(&app, ADMIN, "GET", &entry("v4"), Value::Null).await;
    assert_eq!(status, 200);
    assert_eq!(read["value_base64"], b64(&tail(0)));

    // One value is as large as a single put takes, and not one byte more.
    let (status, body) = batch(&app, vec![put_op("big", &vec![1; MAX_VALUE])]).await;
    assert_eq!(status, 200, "{body}");
    let reply = batch(&app, vec![put_op("bigger", &vec![1; MAX_VALUE + 1])]).await;
    assert_error(&reply, 422, "invalid_input");
}

#[tokio::test]
async fn a_batch_request_is_strict_about_its_shape() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(), directory.path());
    let committed = head(&app).await;
    let put_with = |extra: &str| {
        format!(
            r#"{{"operations":[{{"op":"put","namespace":"n","key":"a","value_base64":""{extra}}}]}}"#
        )
    };
    let cases: Vec<(&str, String, u16, &str)> = vec![
        ("no operations member", "{}".into(), 422, "invalid_json"),
        (
            "an unknown member",
            r#"{"operations":[],"extra":1}"#.into(),
            422,
            "invalid_json",
        ),
        (
            "a repeated member",
            r#"{"operations":[],"operations":[]}"#.into(),
            422,
            "invalid_json",
        ),
        (
            "operations that are not an array",
            r#"{"operations":{}}"#.into(),
            422,
            "invalid_json",
        ),
        (
            "an operation that is not an object",
            r#"{"operations":["put"]}"#.into(),
            422,
            "invalid_json",
        ),
        (
            "an unknown kind of operation",
            r#"{"operations":[{"op":"swap","namespace":"n","key":"a"}]}"#.into(),
            422,
            "invalid_json",
        ),
        (
            "an operation with no kind",
            r#"{"operations":[{"namespace":"n","key":"a"}]}"#.into(),
            422,
            "invalid_json",
        ),
        (
            "a put without a value",
            r#"{"operations":[{"op":"put","namespace":"n","key":"a"}]}"#.into(),
            422,
            "invalid_json",
        ),
        (
            "a delete with a value",
            r#"{"operations":[{"op":"delete","namespace":"n","key":"a","value_base64":""}]}"#
                .into(),
            422,
            "invalid_json",
        ),
        (
            "a deadline on a put",
            put_with(r#","expires_at_ms":"1""#),
            422,
            "invalid_json",
        ),
        (
            "a null deadline on a put",
            put_with(r#","expires_at_ms":null"#),
            422,
            "invalid_json",
        ),
        (
            "an integer expectation",
            put_with(r#","expected":5"#),
            422,
            "invalid_json",
        ),
        (
            "an integer revision",
            put_with(r#","expected":{"revision":5}"#),
            422,
            "invalid_json",
        ),
        (
            "a null expectation",
            put_with(r#","expected":null"#),
            422,
            "invalid_json",
        ),
        (
            "an unknown member of an operation",
            put_with(r#","extra":true"#),
            422,
            "invalid_json",
        ),
        (
            "a repeated member of an operation",
            put_with(r#","key":"b""#),
            422,
            "invalid_json",
        ),
        (
            "a syntax error",
            r#"{"operations":["#.into(),
            400,
            "invalid_json",
        ),
    ];
    for (case, body, status, code) in cases {
        let reply = raw(&app, ADMIN, "POST", BATCH, body).await;
        assert_eq!(reply.0, status, "{case}: {}", reply.1);
        assert_eq!(reply.1["error"]["code"], code, "{case}: {}", reply.1);
        assert_eq!(head(&app).await, committed, "{case}: a commit happened");
    }
    for (query, case) in [
        ("?x=1", "an unknown query"),
        ("?namespace=n", "a namespace"),
    ] {
        let reply = call(
            &app,
            ADMIN,
            "POST",
            &format!("{BATCH}{query}"),
            json!({"operations":[]}),
        )
        .await;
        assert_error(&reply, 400, "invalid_query");
        assert_eq!(head(&app).await, committed, "{case}");
    }
    for method in ["GET", "PUT", "DELETE", "PATCH"] {
        let reply = call(&app, ADMIN, method, BATCH, json!({"operations":[]})).await;
        assert_eq!(reply.0, 405, "{method}: {}", reply.1);
    }
    let (status, _) = send(
        &app,
        ADMIN,
        "POST",
        BATCH,
        r#"{"operations":[]}"#.into(),
        Some("text/plain"),
    )
    .await;
    assert_eq!(status, 415);
    let (status, _) = send(
        &app,
        ADMIN,
        "POST",
        BATCH,
        r#"{"operations":[]}"#.into(),
        None,
    )
    .await;
    assert_eq!(status, 415);
    assert_eq!(head(&app).await, committed);
}

#[tokio::test]
async fn get_many_answers_each_key_in_order_and_null_for_absent_and_due_keys() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(), directory.path());
    let mut deadline = put("later");
    deadline["expires_at_ms"] = json!("18446744073709551615");
    let mut due = put("due");
    due["expires_at_ms"] = json!("1");
    for body in [put("a"), put("b"), deadline, due] {
        assert_eq!(call(&app, ADMIN, "PUT", ENTRY, body).await.0, 200);
    }
    let (status, other) = call(
        &app,
        ADMIN,
        "PUT",
        ENTRY,
        json!({"namespace":"m","key":"a","value_base64":"Yg=="}),
    )
    .await;
    assert_eq!(status, 200, "{other}");

    let keys = [
        ("n", "b"),
        ("n", "missing"),
        ("m", "a"),
        ("n", "a"),
        ("n", "b"),
        ("n", "due"),
        ("n", "later"),
        ("other", "a"),
    ];
    let (status, page) = many(&app, &keys).await;
    assert_eq!(status, 200, "{page}");
    assert_eq!(page["next_index"], Value::Null);
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), keys.len());
    // Every item is what the single-key route answers for that key.
    for (index, (namespace, key)) in keys.iter().enumerate() {
        let uri = format!("{ENTRY}?namespace={namespace}&key={key}");
        let (single_status, single) = call(&app, ADMIN, "GET", &uri, Value::Null).await;
        if single_status == 404 {
            assert_eq!(items[index], Value::Null, "{namespace}/{key}");
        } else {
            assert_eq!(single_status, 200);
            assert_eq!(items[index], single, "{namespace}/{key}");
        }
    }
    assert_eq!(items[0]["key"], "b");
    assert_eq!(items[0], items[4], "a key may be named twice");
    assert_eq!(items[2]["value_base64"], "Yg==");
    assert_eq!(items[6]["expires_at_ms"], "18446744073709551615");

    // No keys, no items.
    assert_eq!(
        many(&app, &[]).await,
        (200, json!({"items":[],"next_index":null}))
    );
    // Readers use it; it writes nothing.
    let committed = head(&app).await;
    let reply = call(
        &app,
        READ,
        "POST",
        MANY,
        json!({"keys":[{"namespace":"n","key":"a"}]}),
    )
    .await;
    assert_eq!(reply.0, 200, "{}", reply.1);
    assert_eq!(reply.1["items"][0]["value_base64"], "YQ==");
    assert_eq!(head(&app).await, committed);
}

#[tokio::test]
async fn a_get_many_request_is_strict_and_names_the_key_that_is_wrong() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(), directory.path());
    let good = json!({"namespace":"n","key":"a"});
    for (case, body) in [
        (
            "an empty namespace",
            json!({"keys":[good, {"namespace":"","key":"a"}]}),
        ),
        (
            "an empty key",
            json!({"keys":[good, good, {"namespace":"n","key":""}]}),
        ),
        (
            "a namespace of 256 bytes",
            json!({"keys":[good, {"namespace":"x".repeat(256),"key":"a"}]}),
        ),
        (
            "a key of 1025 bytes",
            json!({"keys":[good, {"namespace":"n","key":"x".repeat(1025)}]}),
        ),
    ] {
        let reply = call(&app, ADMIN, "POST", MANY, body.clone()).await;
        assert_error(&reply, 422, "invalid_input");
        let index = body["keys"].as_array().unwrap().len() - 1;
        assert!(
            message(&reply).starts_with(&format!("keys[{index}]: ")),
            "{case}: {reply:?}"
        );
    }
    for (case, body) in [
        ("no keys member", "{}"),
        ("an unknown member", r#"{"keys":[],"limit":1}"#),
        ("a repeated member", r#"{"keys":[],"keys":[]}"#),
        ("keys that are not an array", r#"{"keys":"a"}"#),
        ("a key that is not an object", r#"{"keys":["a"]}"#),
        (
            "an unknown member of a key",
            r#"{"keys":[{"namespace":"n","key":"a","extra":1}]}"#,
        ),
        ("a key without a namespace", r#"{"keys":[{"key":"a"}]}"#),
        (
            "a repeated member of a key",
            r#"{"keys":[{"namespace":"n","key":"a","key":"b"}]}"#,
        ),
    ] {
        let reply = raw(&app, ADMIN, "POST", MANY, body.into()).await;
        assert_eq!(reply.0, 422, "{case}: {}", reply.1);
        assert_eq!(reply.1["error"]["code"], "invalid_json", "{case}");
    }
    let reply = raw(&app, ADMIN, "POST", MANY, r#"{"keys":["#.into()).await;
    assert_eq!(reply.0, 400, "{}", reply.1);
    for query in ["?x=1", "?namespace=n"] {
        let reply = call(
            &app,
            ADMIN,
            "POST",
            &format!("{MANY}{query}"),
            json!({"keys":[]}),
        )
        .await;
        assert_error(&reply, 400, "invalid_query");
    }
    for method in ["GET", "PUT", "DELETE", "PATCH"] {
        let reply = call(&app, ADMIN, method, MANY, json!({"keys":[]})).await;
        assert_eq!(reply.0, 405, "{method}: {}", reply.1);
    }
    let (status, _) = send(
        &app,
        ADMIN,
        "POST",
        MANY,
        r#"{"keys":[]}"#.into(),
        Some("text/plain"),
    )
    .await;
    assert_eq!(status, 415);
}

/// The item of a get-many for `key` of namespace "edge", as it is specified,
/// with an opaque engine version.
fn item(key: &str, value: &[u8], version: u64) -> Value {
    json!({"key":key,"value_base64":b64(value),"version":version.to_string(),"expires_at_ms":null})
}

#[tokio::test]
async fn a_get_many_response_is_inclusive_at_sixteen_mebibytes() {
    // The same boundary with and without an absent key among the twelve: it
    // stays in the response as `null`, which has bytes of its own.
    for (with_null, boundary) in [false, true]
        .into_iter()
        .flat_map(|with_null| [PAGE - 1, PAGE, PAGE + 1].map(|boundary| (with_null, boundary)))
    {
        let directory = tempfile::tempdir().unwrap();
        let mut db = database();
        let mut store = db.kv_store("default").unwrap();
        let big = vec![255u8; MAX_VALUE];
        let mut expected = Vec::new();
        let mut last = 0;
        // Twelve entries of the largest size fill a page; the absent key is
        // one more.
        let count = if with_null { 13 } else { 12 };
        for i in 0..count {
            let key = format!("{i:02}");
            if with_null && i == 0 {
                expected.push(Value::Null);
                continue;
            }
            last = store.put("edge", &key, &big, KvPut::default()).unwrap();
            expected.push(item(&key, &big, last));
        }
        // The thirteenth entry brings the whole response to `boundary` bytes:
        // the twelve items and their envelope, a comma, then its own item.
        let head_bytes = serde_json::to_vec(&json!({"items":expected,"next_index":null}))
            .unwrap()
            .len();
        let version = last + 1;
        let fixed = serde_json::to_vec(&item("", b"", version)).unwrap().len();
        let need = boundary - head_bytes - 1 - fixed;
        // The key takes one to four bytes so that the base64 text fills whole
        // groups of four.
        let key = format!("m{}", "x".repeat((need - 1) % 4));
        let value = vec![1u8; (need - key.len()) / 4 * 3];
        assert_eq!(
            store.put("edge", &key, &value, KvPut::default()).unwrap(),
            version
        );
        expected.push(item(&key, &value, version));
        let oracle = json!({"items":expected,"next_index":null});
        assert_eq!(
            serde_json::to_vec(&oracle).unwrap().len(),
            boundary,
            "the independent fixture must hit the boundary"
        );

        let app = start(db, directory.path());
        let mut keys: Vec<Value> = (0..count)
            .map(|i| json!({"namespace":"edge","key":format!("{i:02}")}))
            .collect();
        keys.push(json!({"namespace":"edge","key":key}));
        let (status, page, size) = raw(
            &app,
            ADMIN,
            "POST",
            MANY,
            json!({"keys":keys.clone()}).to_string(),
        )
        .await;
        assert_eq!(status, 200);
        if boundary <= PAGE {
            assert_eq!(size, boundary);
            assert_eq!(page, oracle);
        } else {
            // One byte over: the last key is left for the next call.
            assert!(size <= PAGE, "{with_null}: {size} bytes");
            assert_eq!(page["items"].as_array().unwrap().len(), count);
            assert_eq!(page["next_index"], count);
            assert_eq!(page["items"], json!(&expected[..count]));
            let (status, tail, _) = raw(
                &app,
                ADMIN,
                "POST",
                MANY,
                json!({"keys":&keys[count..]}).to_string(),
            )
            .await;
            assert_eq!(status, 200);
            assert_eq!(tail, json!({"items":[expected[count]],"next_index":null}));
        }
    }
}

#[tokio::test]
async fn get_many_pages_are_maximal_and_cover_every_key_once_in_order() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = database();
    let mut store = db.kv_store("default").unwrap();
    let big = vec![200u8; MAX_VALUE];
    let mut keys: Vec<(String, String)> = Vec::new();
    for i in 0..30 {
        let key = format!("{i:02}");
        match i % 5 {
            // A live entry of the largest size, an absent key, a small entry.
            0 | 1 | 3 => {
                store.put("big", &key, &big, KvPut::default()).unwrap();
            }
            2 => {}
            _ => {
                store.put("big", &key, b"small", KvPut::default()).unwrap();
            }
        }
        keys.push(("big".into(), key));
    }
    let app = start(db, directory.path());

    let envelope = |next: Option<usize>| {
        serde_json::to_vec(&json!({"items":[],"next_index":next}))
            .unwrap()
            .len()
    };
    let size_of = |item: &Value| serde_json::to_vec(item).unwrap().len();
    let mut offset = 0;
    let mut seen: Vec<Value> = Vec::new();
    let mut pages = 0;
    while offset < keys.len() {
        let rest: Vec<Value> = keys[offset..]
            .iter()
            .map(|(namespace, key)| json!({"namespace":namespace,"key":key}))
            .collect();
        let (status, page, size) =
            raw(&app, ADMIN, "POST", MANY, json!({"keys":rest}).to_string()).await;
        assert_eq!(status, 200);
        pages += 1;
        let items = page["items"].as_array().unwrap();
        assert!(!items.is_empty());
        assert!(size <= PAGE, "page {pages} has {size} bytes");
        let next = page["next_index"].as_u64().map(|next| next as usize);
        let total = |count: usize, items: &[Value], next: Option<usize>| {
            envelope(next)
                + items[..count].iter().map(size_of).sum::<usize>()
                + count.saturating_sub(1)
        };
        assert_eq!(size, total(items.len(), items, next), "page {pages}");
        match next {
            // A partial page ends exactly where the next call must begin, and
            // one more key would have passed the bound.
            Some(next) => {
                assert_eq!(next, items.len());
                assert!(next >= 1 && offset + next < keys.len());
                let (_, following, _) = raw(
                    &app,
                    ADMIN,
                    "POST",
                    MANY,
                    json!({"keys":[json!({"namespace":keys[offset + next].0,"key":keys[offset + next].1})]})
                        .to_string(),
                )
                .await;
                let following = &following["items"][0];
                let mut longer = items.to_vec();
                longer.push(following.clone());
                let after = (offset + next + 1 < keys.len()).then_some(next + 1);
                assert!(
                    total(longer.len(), &longer, after) > PAGE,
                    "page {pages} stopped early: {} bytes plus the next item",
                    size
                );
            }
            None => assert_eq!(offset + items.len(), keys.len()),
        }
        seen.extend(items.iter().cloned());
        offset += items.len();
    }
    assert!(pages > 1, "the keys must not fit one response");
    assert_eq!(seen.len(), keys.len());
    for (index, ((_, key), item)) in keys.iter().zip(&seen).enumerate() {
        match index % 5 {
            2 => assert_eq!(item, &Value::Null, "{key}"),
            0 | 1 | 3 => assert_eq!(item["key"], key.as_str()),
            _ => assert_eq!(item["value_base64"], "c21hbGw="),
        }
    }
}

#[tokio::test]
async fn collection_errors_match_the_single_key_routes() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(), directory.path());
    let scoped = credential(&app, json!({"operations":["read","write"]})).await;
    for token in [ADMIN, scoped.as_str()] {
        for (name, case) in [
            ("missing", "a collection that does not exist"),
            ("wide", "a collection of two dimensions"),
            ("Upper", "an upper-case name"),
            ("a%20b", "a name with a space"),
            ("a%2Fb", "a name with a slash"),
            ("-dash", "a name that starts with a dash"),
        ] {
            let single_put = call(
                &app,
                token,
                "PUT",
                &format!("/v1/collections/{name}/kv/entry"),
                put("k"),
            )
            .await;
            let batched = call(
                &app,
                token,
                "POST",
                &format!("/v1/collections/{name}/kv/batch"),
                json!({"operations":[put_op("k", b"a")]}),
            )
            .await;
            assert_eq!(
                (batched.0, &batched.1["error"]["code"]),
                (single_put.0, &single_put.1["error"]["code"]),
                "{case} for a batch: {batched:?} against {single_put:?}"
            );
            let single_get = call(
                &app,
                token,
                "GET",
                &format!("/v1/collections/{name}/kv/entry?namespace=n&key=k"),
                Value::Null,
            )
            .await;
            let read = call(
                &app,
                token,
                "POST",
                &format!("/v1/collections/{name}/kv/entries/get"),
                json!({"keys":[{"namespace":"n","key":"k"}]}),
            )
            .await;
            // A key that is absent answers 404 `kv_not_found` on the single
            // route; the collection errors are the same on both.
            if single_get.1["error"]["code"] != "kv_not_found" {
                assert_eq!(
                    (read.0, &read.1["error"]["code"]),
                    (single_get.0, &single_get.1["error"]["code"]),
                    "{case} for get-many: {read:?} against {single_get:?}"
                );
            }
        }
    }
    let missing = call(
        &app,
        ADMIN,
        "POST",
        "/v1/collections/missing/kv/batch",
        json!({"operations":[]}),
    )
    .await;
    assert_error(&missing, 404, "collection_not_found");
    let wide = call(
        &app,
        ADMIN,
        "POST",
        "/v1/collections/wide/kv/entries/get",
        json!({"keys":[]}),
    )
    .await;
    assert_error(&wide, 409, "not_kv_collection");
}

#[tokio::test]
async fn a_version_3_file_serves_a_batch_like_a_single_write() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database_of(FormatVersion::V3), directory.path());
    let single = call(&app, ADMIN, "PUT", ENTRY, put("k")).await;
    assert_eq!(single.0, 200, "{single:?}");
    let batched = batch(&app, vec![put_op("b", b"a"), delete_op("k")]).await;
    assert_eq!(batched.0, 200, "{batched:?}");
    assert_eq!(batched.1["results"][1], json!({"deleted":true}));
    assert_eq!(values(&app, &["k", "b"]).await, [None, Some(b64(b"a"))]);
    // The capability follows the format, as `kv` does for the single calls.
    let (status, caps) = call(&app, ADMIN, "GET", "/v1/capabilities", Value::Null).await;
    assert_eq!(status, 200);
    assert_eq!(caps["features"]["kv"], false);
    assert_eq!(caps["features"]["kv_batch"], false);
}

#[tokio::test]
async fn slots_are_taken_in_order_within_a_batch_and_a_refusal_is_counted_once() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(), directory.path());
    let base = first_slot("n", SHARED[0]);
    for key in SHARED {
        assert_eq!(first_slot("n", key), base);
    }
    // Thirteen ordinary records take the first slots of the window: three
    // slots are free for four keys that probe from the same one.
    for offset in 0..13 {
        let uri = format!(
            "/v1/collections/default/vectors/{}",
            base.wrapping_add(offset)
        );
        let (status, body) = call(
            &app,
            ADMIN,
            "PUT",
            &uri,
            json!({"vector":[0.0],"payload_base64":b64(b"ordinary occupied record")}),
        )
        .await;
        assert_eq!(status, 204, "{body}");
    }
    let committed = head(&app).await;
    assert_eq!(counter(&app).await, 0);
    let all: Vec<Value> = SHARED
        .iter()
        .map(|key| put_op(key, key.as_bytes()))
        .collect();
    let reply = batch(&app, all.clone()).await;
    assert_error(&reply, 409, "kv_slots_exhausted");
    assert_eq!(counter(&app).await, 1, "a refused batch is counted once");
    assert_eq!(head(&app).await, committed, "the refusal wrote nothing");
    for key in SHARED {
        assert_eq!(
            call(&app, ADMIN, "GET", &entry(key), Value::Null).await.0,
            404
        );
    }
    for offset in 13..16 {
        let uri = format!(
            "/v1/collections/default/vectors/{}",
            base.wrapping_add(offset)
        );
        assert_eq!(call(&app, ADMIN, "GET", &uri, Value::Null).await.0, 404);
    }

    // Three keys fit: each takes its own slot, none overwrites another.
    let (status, body) = batch(&app, all[..3].to_vec()).await;
    assert_eq!(status, 200, "{body}");
    for (index, key) in SHARED[..3].iter().enumerate() {
        let (status, read) = call(&app, ADMIN, "GET", &entry(key), Value::Null).await;
        assert_eq!(status, 200, "{read}");
        assert_eq!(read["value_base64"], b64(key.as_bytes()), "{index}");
        let uri = format!(
            "/v1/collections/default/vectors/{}",
            base.wrapping_add(13 + index as u64)
        );
        assert_eq!(call(&app, ADMIN, "GET", &uri, Value::Null).await.0, 200);
    }
    // The fourth has no slot left, by batch or by single put.
    assert_error(
        &call(&app, ADMIN, "PUT", ENTRY, put(SHARED[3])).await,
        409,
        "kv_slots_exhausted",
    );
    assert_eq!(counter(&app).await, 2);
    assert_error(
        &batch(&app, all[3..].to_vec()).await,
        409,
        "kv_slots_exhausted",
    );
    assert_eq!(counter(&app).await, 3);
    // A batch that does not touch the full window is not counted.
    assert_eq!(batch(&app, vec![put_op("free", b"x")]).await.0, 200);
    assert_eq!(counter(&app).await, 3);
}

#[tokio::test]
async fn quota_is_judged_on_the_net_change_of_the_whole_batch() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(), directory.path());
    let writer = credential(&app, json!({"operations":["write"],"max_vectors":2})).await;
    let run = |operations: Vec<Value>| {
        let app = app.clone();
        let writer = writer.clone();
        async move {
            call(
                &app,
                &writer,
                "POST",
                BATCH,
                json!({"operations":operations}),
            )
            .await
        }
    };
    assert_eq!(run(vec![put_op("a", b"1"), put_op("b", b"1")]).await.0, 200);
    let committed = head(&app).await;
    let over = run(vec![put_op("c", b"1")]).await;
    assert_error(&over, 429, "quota_exceeded");
    assert_eq!(head(&app).await, committed);
    // Replacing two live entries adds nothing.
    assert_eq!(run(vec![put_op("a", b"2"), put_op("b", b"2")]).await.0, 200);
    // One in, one out is no growth, in whatever order the batch names them.
    assert_eq!(run(vec![put_op("c", b"1"), delete_op("a")]).await.0, 200);
    assert_eq!(run(vec![delete_op("b"), put_op("a", b"1")]).await.0, 200);
    // Two in, none out is growth; the refusal leaves the earlier state.
    let committed = head(&app).await;
    assert_error(
        &run(vec![put_op("d", b"1"), put_op("e", b"1")]).await,
        429,
        "quota_exceeded",
    );
    assert_eq!(head(&app).await, committed);
    // Two out and one in shrinks it.
    assert_eq!(
        run(vec![delete_op("a"), delete_op("c"), put_op("x", b"1")])
            .await
            .0,
        200
    );
    assert_eq!(values(&app, &["a", "b", "c", "x"]).await.len(), 4);
}

#[tokio::test]
async fn scopes_decide_who_may_read_or_batch() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(), directory.path());
    assert_eq!(batch(&app, vec![put_op("k", b"v")]).await.0, 200);
    let reader = credential(&app, json!({})).await;
    let writer = credential(&app, json!({"operations":["write"]})).await;
    let both = credential(&app, json!({"operations":["read","write"]})).await;
    let graph = credential(&app, json!({"operations":["graph_read"],"graphs":["*"]})).await;
    let batch_body = json!({"operations":[put_op("k", b"w")]});
    let many_body = json!({"keys":[{"namespace":"n","key":"k"}]});
    let committed = head(&app).await;

    for token in [&reader, READ, &graph] {
        let reply = call(&app, token, "POST", BATCH, batch_body.clone()).await;
        assert_eq!(reply.0, 403, "{reply:?}");
    }
    assert_eq!(head(&app).await, committed);
    for token in [&reader, READ, &both] {
        let reply = call(&app, token, "POST", MANY, many_body.clone()).await;
        assert_eq!(reply.0, 200, "{reply:?}");
        assert_eq!(reply.1["items"][0]["value_base64"], "dg==");
    }
    for token in [&writer, &graph] {
        let reply = call(&app, token, "POST", MANY, many_body.clone()).await;
        assert_error(&reply, 403, "forbidden_scope");
    }
    assert_eq!(
        call(&app, &writer, "POST", BATCH, batch_body.clone())
            .await
            .0,
        200
    );
    assert_eq!(
        call(&app, &both, "POST", BATCH, batch_body.clone()).await.0,
        200
    );

    // A credential bound to `default` reaches no other collection, whether it
    // exists or not, and is not told which.
    for collection in ["wide", "missing"] {
        for (route, body) in [("kv/batch", &batch_body), ("kv/entries/get", &many_body)] {
            let uri = format!("/v1/collections/{collection}/{route}");
            let reply = call(&app, &both, "POST", &uri, body.clone()).await;
            assert_error(&reply, 403, "forbidden_scope");
        }
    }
    // The read-only token still finds a missing collection, as for any read.
    let reply = call(
        &app,
        READ,
        "POST",
        "/v1/collections/missing/kv/entries/get",
        many_body.clone(),
    )
    .await;
    assert_error(&reply, 404, "collection_not_found");
}

#[tokio::test]
async fn a_read_only_database_answers_get_many_and_refuses_a_batch() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("read-only.vlite");
    let mut db = Database::create_with_format(&path, 1, FormatVersion::V5).unwrap();
    db.kv_store("default")
        .unwrap()
        .put("n", "k", b"v", KvPut::default())
        .unwrap();
    db.close().unwrap();
    let app = start(
        Database::open_read_only(&path).unwrap(),
        &directory.path().join("admin"),
    );
    let reply = many(&app, &[("n", "k")]).await;
    assert_eq!(reply.0, 200, "{reply:?}");
    assert_eq!(reply.1["items"][0]["value_base64"], "dg==");
    for operations in [vec![put_op("k", b"w")], vec![delete_op("absent")], vec![]] {
        let reply = batch(&app, operations).await;
        assert_error(&reply, 403, "read_only");
    }
}

#[tokio::test]
async fn tenants_batch_and_read_only_their_own_collections() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(), directory.path());
    // A root collection of the same name, with an entry under the same key.
    assert_eq!(
        call(
            &app,
            ADMIN,
            "POST",
            "/v1/collections",
            json!({"name":"items","dimensions":1})
        )
        .await
        .0,
        201
    );
    let items_batch = "/v1/collections/items/kv/batch";
    let items_many = "/v1/collections/items/kv/entries/get";
    let write = |value: &'static [u8]| json!({"operations":[put_op("k", value)]});
    assert_eq!(
        call(&app, ADMIN, "POST", items_batch, write(b"root"))
            .await
            .0,
        200
    );
    let mut tokens = Vec::new();
    for (name, value) in [("one", &b"one"[..]), ("two", &b"two"[..])] {
        let (_, tenant) = call(
            &app,
            ADMIN,
            "POST",
            "/v1/admin/tenants",
            json!({"name":name}),
        )
        .await;
        let token = credential(
            &app,
            json!({"tenant_id":tenant["tenant"]["id"],"collections":["*"],
                "operations":["collections","read","write"]}),
        )
        .await;
        assert_eq!(
            call(
                &app,
                &token,
                "POST",
                "/v1/collections",
                json!({"name":"items","dimensions":1})
            )
            .await
            .0,
            201
        );
        let reply = call(
            &app,
            &token,
            "POST",
            items_batch,
            json!({"operations":[put_op("k", value)]}),
        )
        .await;
        assert_eq!(reply.0, 200, "{reply:?}");
        tokens.push((token, value));
    }
    let read = |token: &str| {
        let token = token.to_owned();
        let app = app.clone();
        async move {
            let reply = call(
                &app,
                &token,
                "POST",
                items_many,
                json!({"keys":[{"namespace":"n","key":"k"}]}),
            )
            .await;
            assert_eq!(reply.0, 200, "{reply:?}");
            reply.1["items"][0]["value_base64"]
                .as_str()
                .map(str::to_owned)
        }
    };
    assert_eq!(read(ADMIN).await, Some(b64(b"root")));
    for (token, value) in &tokens {
        assert_eq!(read(token).await, Some(b64(value)));
    }
    // A tenant's delete reaches its own entry only.
    let deleted = call(
        &app,
        &tokens[0].0,
        "POST",
        items_batch,
        json!({"operations":[delete_op("k")]}),
    )
    .await;
    assert_eq!(deleted, (200, json!({"results":[{"deleted":true}]})));
    assert_eq!(read(&tokens[0].0).await, None);
    assert_eq!(read(&tokens[1].0).await, Some(b64(b"two")));
    assert_eq!(read(ADMIN).await, Some(b64(b"root")));
}

#[tokio::test]
async fn a_batch_is_audited_by_its_counts_and_never_by_its_content() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(), directory.path());
    call(&app, ADMIN, "PUT", ENTRY, put("existing")).await;
    let (status, body) = call(
        &app,
        ADMIN,
        "POST",
        BATCH,
        json!({"operations":[
            put_op("secret-key-1", b"secret-value-1"),
            put_op("secret-key-2", b"secret-value-2"),
            delete_op("existing"),
        ]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let mut stale = put_op("secret-key-3", b"secret-value-3");
    stale["expected"] = json!({"revision":"999999"});
    let refused = call(
        &app,
        ADMIN,
        "POST",
        BATCH,
        json!({"operations":[stale, delete_op("secret-key-1")]}),
    )
    .await;
    assert_error(&refused, 412, "precondition_failed");
    let missing = call(
        &app,
        ADMIN,
        "POST",
        "/v1/collections/nowhere/kv/batch",
        json!({"operations":[delete_op("secret-key-1")]}),
    )
    .await;
    assert_error(&missing, 404, "collection_not_found");
    // Refused before any lookup: no event.
    assert_eq!(
        batch(&app, vec![delete_op("k"), delete_op("k")]).await.0,
        422
    );

    let log = std::fs::read_to_string(directory.path().join("audit.jsonl")).unwrap();
    let events: Vec<Value> = log
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["event"] == "kv_batch")
        .collect();
    assert_eq!(events.len(), 3, "{log}");
    for (event, outcome, collection, puts, deletes) in [
        (&events[0], "ok", "default", 2, 1),
        (&events[1], "precondition_failed", "(invalid)", 1, 1),
        (&events[2], "collection_not_found", "(invalid)", 0, 1),
    ] {
        assert_eq!(event["outcome"], outcome);
        assert_eq!(event["collection"], collection);
        assert_eq!(event["principal"], "admin");
        assert_eq!(event["actor"], "admin_token");
        assert_eq!(event["puts"], puts);
        assert_eq!(event["deletes"], deletes);
        // The fields of the other key-value events, less the deadline that a
        // batch put does not have, plus the counts.
        let mut keys: Vec<&str> = event
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "actor",
                "collection",
                "deletes",
                "duration_ns",
                "event",
                "outcome",
                "principal",
                "puts",
                "request_id",
                "unix_ms"
            ]
        );
    }
    for content in [
        "secret-key",
        "secret-value",
        &b64(b"secret-value-1"),
        "existing",
        "\"namespace\"",
    ] {
        assert!(
            !log.lines()
                .filter(|line| line.contains("kv_batch"))
                .any(|line| line.contains(content)),
            "a kv_batch event holds {content}"
        );
    }
}

#[tokio::test]
async fn capabilities_advertise_the_batch_routes_with_the_format() {
    let directory = tempfile::tempdir().unwrap();
    for (format, expected) in [(FormatVersion::V5, true), (FormatVersion::V3, false)] {
        let app = start(database_of(format), directory.path());
        let (status, caps) = call(&app, ADMIN, "GET", "/v1/capabilities", Value::Null).await;
        assert_eq!(status, 200);
        assert_eq!(caps["features"]["kv"], expected);
        assert_eq!(caps["features"]["kv_batch"], expected);
        let routes = caps["routes"].as_array().unwrap();
        for route in [
            "/v1/collections/{collection}/kv/batch",
            "/v1/collections/{collection}/kv/entries/get",
        ] {
            assert_eq!(
                routes
                    .iter()
                    .filter(|item| item.as_str() == Some(route))
                    .count(),
                1,
                "{route}"
            );
        }
    }
}

#[tokio::test]
async fn a_batch_is_one_atomic_step_for_concurrent_readers() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(), directory.path());
    // Two counters that always move together: a reader that sees one without
    // the other has seen half a batch.
    assert_eq!(
        batch(&app, vec![put_op("left", b"0"), put_op("right", b"0")])
            .await
            .0,
        200
    );
    let writer = {
        let app = app.clone();
        tokio::spawn(async move {
            for round in 1..=40u32 {
                let value = round.to_string();
                let (status, body) = batch(
                    &app,
                    vec![
                        put_op("left", value.as_bytes()),
                        put_op("right", value.as_bytes()),
                    ],
                )
                .await;
                assert_eq!(status, 200, "{body}");
            }
        })
    };
    let reader = {
        let app = app.clone();
        tokio::spawn(async move {
            for _ in 0..200 {
                let (status, page) = many(&app, &[("n", "left"), ("n", "right")]).await;
                assert_eq!(status, 200, "{page}");
                assert_eq!(
                    page["items"][0]["value_base64"], page["items"][1]["value_base64"],
                    "{page}"
                );
                assert_eq!(page["items"][0]["version"], page["items"][1]["version"]);
                tokio::task::yield_now().await;
            }
        })
    };
    writer.await.unwrap();
    reader.await.unwrap();
}
