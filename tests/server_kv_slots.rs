#![cfg(feature = "server")]
//! A key-value put whose key has no free probe slot fails with 409
//! `kv_slots_exhausted` on every transport. Each failed request is counted
//! once in the `kv_slots_exhausted_total` metric and audited with that
//! outcome; a slot held by an expired entry is reused, while a slot held by a
//! live entry or by an ordinary record is not.

use axum::{Router, body::Body, http::Request};
use base64::{Engine, prelude::BASE64_STANDARD};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use vlite::{
    CollectionSpec, CreateOptions, Database, FormatVersion,
    kv::first_slot,
    server::{BearerToken, ServerOptions, Service, configured_admin_router},
};

const ADMIN: &str = "fake_slots_admin_token_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const ENTRY: &str = "/v1/collections/default/kv/entry";
const ENTRIES: &str = "/v1/collections/default/kv/entries?namespace=n";

// Namespace "n" keys that share the first slot u64::MAX - 2 (see tests/kv.rs).
const SHARED: [&str; 4] = [
    "wrap-1e3h8xptc3rmwn",
    "wrap-1mr1oejil0ss3d",
    "wrap-rcxazt3ti43gt3",
    "wrap-zh8a7z15oq0wpx",
];

fn database() -> Database {
    let mut db = Database::create_in_memory(
        1,
        CreateOptions {
            format: Some(FormatVersion::V5),
            ..Default::default()
        },
    )
    .unwrap();
    db.create_collection(
        "wide",
        CollectionSpec {
            dimensions: 2,
            settings: Default::default(),
        },
    )
    .unwrap();
    db
}

fn options(directory: &std::path::Path) -> ServerOptions {
    ServerOptions {
        admin_token: Some(BearerToken::new(ADMIN).unwrap()),
        admin_directory: Some(directory.into()),
        ..Default::default()
    }
}

fn start(db: Database, directory: &std::path::Path) -> Router {
    configured_admin_router(Service::new(db), options(directory)).unwrap()
}

async fn raw(app: &Router, token: &str, method: &str, uri: &str, body: Value) -> (u16, String) {
    let body = if body.is_null() {
        Body::empty()
    } else {
        Body::from(body.to_string())
    };
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(body)
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn call(app: &Router, token: &str, method: &str, uri: &str, body: Value) -> (u16, Value) {
    let (status, text) = raw(app, token, method, uri, body).await;
    let value = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap()
    };
    (status, value)
}

fn put(key: &str) -> Value {
    json!({"namespace":"n","key":key,"value_base64":"YQ=="})
}

fn entry(key: &str) -> String {
    format!("{ENTRY}?namespace=n&key={key}")
}

/// The payload `kv` stores: "VKV1", namespace length, key length (u16 little
/// endian), namespace, key, value.
fn kv_shaped(namespace: &str, key: &str, value: &str) -> String {
    let mut payload = b"VKV1".to_vec();
    payload.push(namespace.len() as u8);
    payload.extend_from_slice(&(key.len() as u16).to_le_bytes());
    payload.extend_from_slice(namespace.as_bytes());
    payload.extend_from_slice(key.as_bytes());
    payload.extend_from_slice(value.as_bytes());
    BASE64_STANDARD.encode(payload)
}

/// An ordinary record at `id`, which any writer of the collection may place.
async fn plant(app: &Router, id: u64, payload: &str) {
    let uri = format!("/v1/collections/default/vectors/{id}");
    let body = json!({"vector":[0.0],"payload_base64":BASE64_STANDARD.encode(payload)});
    let (status, response) = call(app, ADMIN, "PUT", &uri, body).await;
    assert_eq!(status, 204, "{response}");
}

/// Fills all 16 slots of `key` in namespace "n" with ordinary records.
async fn fill_window(app: &Router, key: &str) {
    let base = first_slot("n", key);
    for offset in 0..16 {
        plant(app, base.wrapping_add(offset), "ordinary occupied record").await;
    }
}

async fn counter(app: &Router) -> u64 {
    let (status, metrics) = call(app, ADMIN, "GET", "/metrics", Value::Null).await;
    assert_eq!(status, 200, "{metrics}");
    metrics["kv_slots_exhausted_total"].as_u64().unwrap()
}

async fn head(app: &Router) -> String {
    let (status, feed) = call(app, ADMIN, "GET", "/v1/changes?limit=1", Value::Null).await;
    assert_eq!(status, 200, "{feed}");
    feed["head"].as_str().unwrap().to_owned()
}

#[track_caller]
fn assert_exhausted(reply: &(u16, Value)) {
    assert_eq!(reply.0, 409, "{}", reply.1);
    assert_eq!(
        reply.1["error"]["code"], "kv_slots_exhausted",
        "{}",
        reply.1
    );
}

async fn credential(app: &Router, operations: Value) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let input = json!({"name":"kv-slots","collections":["default"],"operations":operations,
        "expires_unix_ms":now + 600_000});
    let (status, body) = call(app, ADMIN, "POST", "/v1/admin/credentials", input).await;
    assert_eq!(status, 201, "{body}");
    body["token"].as_str().unwrap().into()
}

#[tokio::test]
async fn slot_exhaustion_is_counted_once_per_failed_request_and_nothing_else_is() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(), directory.path());
    assert_eq!(counter(&app).await, 0);

    // A writer takes every slot of one key with ordinary records; those
    // successful puts are not counted.
    fill_window(&app, "occupied").await;
    assert_eq!(counter(&app).await, 0);

    // Successful requests and every other failure leave the counter alone.
    assert_eq!(call(&app, ADMIN, "PUT", ENTRY, put("free")).await.0, 200);
    let free = entry("free");
    assert_eq!(call(&app, ADMIN, "GET", &free, Value::Null).await.0, 200);
    let absent = entry("absent");
    assert_eq!(call(&app, ADMIN, "GET", &absent, Value::Null).await.0, 404);
    assert_eq!(call(&app, ADMIN, "GET", ENTRIES, Value::Null).await.0, 200);
    let mut conditional = put("free");
    conditional["expected"] = json!("absent");
    assert_eq!(call(&app, ADMIN, "PUT", ENTRY, conditional).await.0, 412);
    let (status, body) = call(
        &app,
        ADMIN,
        "PUT",
        "/v1/collections/wide/kv/entry",
        put("k"),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["code"], "not_kv_collection");
    let mut empty = put("k");
    empty["namespace"] = json!("");
    assert_eq!(call(&app, ADMIN, "PUT", ENTRY, empty).await.0, 422);
    assert_eq!(counter(&app).await, 0);

    // Each failed put adds exactly one, and writes nothing.
    let committed = head(&app).await;
    for attempt in 1..=3 {
        assert_exhausted(&call(&app, ADMIN, "PUT", ENTRY, put("occupied")).await);
        assert_eq!(counter(&app).await, attempt);
    }
    assert_eq!(head(&app).await, committed, "no commit");

    // Reading or deleting the same key does not count, and the records stay.
    let occupied = entry("occupied");
    let (status, body) = call(&app, ADMIN, "GET", &occupied, Value::Null).await;
    assert_eq!(
        (status, &body["error"]["code"]),
        (404, &json!("kv_not_found"))
    );
    let (status, body) = call(&app, ADMIN, "DELETE", &occupied, Value::Null).await;
    assert_eq!((status, body), (200, json!({"deleted":false})));
    let base = first_slot("n", "occupied");
    for offset in 0..16 {
        let uri = format!(
            "/v1/collections/default/vectors/{}",
            base.wrapping_add(offset)
        );
        let (status, record) = call(&app, ADMIN, "GET", &uri, Value::Null).await;
        assert_eq!(status, 200, "{record}");
        assert_eq!(
            record["payload_base64"],
            BASE64_STANDARD.encode("ordinary occupied record")
        );
    }
    assert_eq!(counter(&app).await, 3);

    // Prometheus exposes the same total as a counter.
    let (status, text) = raw(
        &app,
        ADMIN,
        "GET",
        "/metrics?format=prometheus",
        Value::Null,
    )
    .await;
    assert_eq!(status, 200);
    assert!(text.contains("# TYPE vlite_kv_slots_exhausted_total counter\n"));
    assert!(
        text.contains("\nvlite_kv_slots_exhausted_total 3\n"),
        "{text}"
    );
}

#[tokio::test]
async fn a_failed_put_through_a_scoped_credential_adds_to_the_one_global_counter() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(), directory.path());
    fill_window(&app, "occupied").await;
    let reader = credential(&app, json!(["read"])).await;
    let writer = credential(&app, json!(["write"])).await;
    let other = credential(&app, json!(["write"])).await;

    // A credential that may not write is refused before the store is reached.
    let (status, body) = call(&app, &reader, "PUT", ENTRY, put("occupied")).await;
    assert_eq!(
        (status, &body["error"]["code"]),
        (403, &json!("forbidden_scope"))
    );
    assert_eq!(counter(&app).await, 0);

    assert_exhausted(&call(&app, &writer, "PUT", ENTRY, put("occupied")).await);
    assert_eq!(counter(&app).await, 1);
    assert_exhausted(&call(&app, &other, "PUT", ENTRY, put("occupied")).await);
    assert_eq!(counter(&app).await, 2);
    assert_exhausted(&call(&app, ADMIN, "PUT", ENTRY, put("occupied")).await);
    assert_eq!(counter(&app).await, 3);
    // A successful scoped put is not counted.
    assert_eq!(call(&app, &writer, "PUT", ENTRY, put("free")).await.0, 200);
    assert_eq!(counter(&app).await, 3);
}

#[tokio::test]
async fn the_audit_event_of_a_failed_put_names_the_slot_outcome() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(), directory.path());
    fill_window(&app, "occupied").await;
    assert_eq!(call(&app, ADMIN, "PUT", ENTRY, put("free")).await.0, 200);
    assert_exhausted(&call(&app, ADMIN, "PUT", ENTRY, put("occupied")).await);

    let audit = std::fs::read_to_string(directory.path().join("audit.jsonl")).unwrap();
    let events: Vec<Value> = audit
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["event"] == "kv_put")
        .collect();
    let outcomes: Vec<&Value> = events.iter().map(|event| &event["outcome"]).collect();
    assert_eq!(outcomes, [&json!("ok"), &json!("kv_slots_exhausted")]);
    let failed = &events[1];
    assert_eq!(failed["principal"], "admin");
    assert!(failed.get("request_id").is_some());
    for forbidden in ["namespace", "key", "value", "value_base64"] {
        assert!(failed.get(forbidden).is_none(), "{forbidden}");
    }
}

#[tokio::test]
async fn an_expired_slot_is_reused_over_rest_but_live_and_foreign_slots_are_not() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(), directory.path());
    let [a, b, c, d] = SHARED;
    let base = first_slot("n", a);
    let id = |offset: u64| base.wrapping_add(offset);

    // A deadline in the past is accepted by contract: a is stored, already
    // expired, in the first slot of the window.
    let mut due = put(a);
    due["expires_at_ms"] = json!("1");
    assert_eq!(call(&app, ADMIN, "PUT", ENTRY, due).await.0, 200);
    assert_eq!(
        call(&app, ADMIN, "GET", &entry(a), Value::Null).await.0,
        404
    );
    // Fifteen ordinary records take the rest of the window.
    for offset in 1..16 {
        plant(&app, id(offset), "foreign").await;
    }

    // The next key takes the expired entry's slot, and only that one.
    let mut next = put(b);
    next["value_base64"] = json!(BASE64_STANDARD.encode("bv"));
    assert_eq!(call(&app, ADMIN, "PUT", ENTRY, next).await.0, 200);
    let slot = |offset: u64| format!("/v1/collections/default/vectors/{}", id(offset));
    let (status, record) = call(&app, ADMIN, "GET", &slot(0), Value::Null).await;
    assert_eq!(status, 200, "{record}");
    assert_eq!(record["payload_base64"], kv_shaped("n", b, "bv"));
    let (status, found) = call(&app, ADMIN, "GET", &entry(b), Value::Null).await;
    assert_eq!(status, 200, "{found}");
    assert_eq!(found["value_base64"], BASE64_STANDARD.encode("bv"));
    assert_eq!(
        call(&app, ADMIN, "GET", &entry(a), Value::Null).await.0,
        404
    );
    assert_eq!(counter(&app).await, 0);

    // The window is full again: b is live and fifteen records are not entries.
    let mut before = Vec::new();
    for offset in 0..16 {
        let (status, body) = call(&app, ADMIN, "GET", &slot(offset), Value::Null).await;
        assert_eq!(status, 200, "{body}");
        before.push(body);
    }
    let committed = head(&app).await;
    assert_exhausted(&call(&app, ADMIN, "PUT", ENTRY, put(c)).await);
    assert_eq!(counter(&app).await, 1);
    assert_eq!(head(&app).await, committed, "the refusal wrote nothing");
    for (offset, expected) in before.iter().enumerate() {
        let (_, body) = call(&app, ADMIN, "GET", &slot(offset as u64), Value::Null).await;
        assert_eq!(&body, expected, "slot {offset}");
    }

    // Removing the live entry frees its slot, and only that one, for c.
    let (status, deleted) = call(&app, ADMIN, "DELETE", &entry(b), Value::Null).await;
    assert_eq!((status, deleted), (200, json!({"deleted":true})));
    assert_eq!(call(&app, ADMIN, "PUT", ENTRY, put(c)).await.0, 200);
    let (_, record) = call(&app, ADMIN, "GET", &slot(0), Value::Null).await;
    assert_eq!(record["payload_base64"], kv_shaped("n", c, "a"));
    assert_exhausted(&call(&app, ADMIN, "PUT", ENTRY, put(d)).await);
    assert_eq!(counter(&app).await, 2);
    for offset in 1..16 {
        let (_, body) = call(&app, ADMIN, "GET", &slot(offset), Value::Null).await;
        assert_eq!(body, before[offset as usize], "foreign slot {offset}");
    }
}

#[cfg(feature = "grpc")]
mod grpc {
    use super::*;
    use tonic::{Code, Request, Status, transport::Channel};
    use vlite::server::grpc::{
        ERROR_CODE_TRAILER, grpc_router,
        proto::{self, kv_client::KvClient},
    };

    fn request<T>(message: T) -> Request<T> {
        let mut request = Request::new(message);
        request
            .metadata_mut()
            .insert("authorization", format!("Bearer {ADMIN}").parse().unwrap());
        request
    }

    fn address(collection: &str, key: &str) -> Option<proto::KvAddress> {
        Some(proto::KvAddress {
            collection: collection.into(),
            namespace: "n".into(),
            key: key.into(),
        })
    }

    fn put_request(collection: &str, key: &str) -> proto::KvPutRequest {
        proto::KvPutRequest {
            address: address(collection, key),
            value: b"a".to_vec(),
            ..Default::default()
        }
    }

    #[track_caller]
    fn assert_error(status: &Status, code: Code, stable: &str) {
        assert_eq!(status.code(), code, "{status:?}");
        assert_eq!(
            status.metadata().get(ERROR_CODE_TRAILER).unwrap(),
            stable,
            "{status:?}"
        );
        assert!(status.metadata().get("x-request-id").is_some());
    }

    #[tokio::test]
    async fn put_kv_entry_counts_slot_exhaustion_in_the_same_counter() {
        let directory = tempfile::tempdir().unwrap();
        let mut db = database();
        let base = first_slot("n", "occupied");
        for offset in 0..16 {
            db.put(base.wrapping_add(offset), &[0.0], b"occupied")
                .unwrap();
        }
        let service = Service::new(db);
        let app = configured_admin_router(service.clone(), options(directory.path())).unwrap();
        let server = grpc_router(service).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address_of_server = listener.local_addr().unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(server.serve_with_shutdown(listener, async {
            let _ = stopped.await;
        }));
        let channel = Channel::from_shared(format!("http://{address_of_server}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = KvClient::new(channel);
        assert_eq!(counter(&app).await, 0);

        // Successful calls and other failures are not counted.
        client
            .put_kv_entry(request(put_request("default", "free")))
            .await
            .unwrap();
        client
            .get_kv_entry(request(proto::KvGetRequest {
                address: address("default", "free"),
            }))
            .await
            .unwrap();
        client
            .list_kv_entries(request(proto::KvListRequest {
                collection: "default".into(),
                namespace: "n".into(),
                ..Default::default()
            }))
            .await
            .unwrap();
        let status = client
            .put_kv_entry(request(put_request("wide", "k")))
            .await
            .unwrap_err();
        assert_error(&status, Code::FailedPrecondition, "not_kv_collection");
        let status = client
            .get_kv_entry(request(proto::KvGetRequest {
                address: address("default", "absent"),
            }))
            .await
            .unwrap_err();
        assert_error(&status, Code::NotFound, "kv_not_found");
        assert_eq!(counter(&app).await, 0);

        // Each failed PutKvEntry adds one, with the stable trailer code.
        for attempt in 1..=2 {
            let status = client
                .put_kv_entry(request(put_request("default", "occupied")))
                .await
                .unwrap_err();
            assert_error(&status, Code::FailedPrecondition, "kv_slots_exhausted");
            assert_eq!(counter(&app).await, attempt);
        }
        // REST and gRPC failures share one total.
        assert_exhausted(&call(&app, ADMIN, "PUT", ENTRY, put("occupied")).await);
        assert_eq!(counter(&app).await, 3);
        let status = client
            .put_kv_entry(request(put_request("default", "occupied")))
            .await
            .unwrap_err();
        assert_error(&status, Code::FailedPrecondition, "kv_slots_exhausted");
        assert_eq!(counter(&app).await, 4);
        let _ = stop.send(());
    }
}
