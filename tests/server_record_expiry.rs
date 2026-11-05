#![cfg(feature = "server")]
//! The ordinary record routes refuse an expiry that is not later than the
//! server clock: 422 `invalid_input` before anything is written, never a 500
//! after a durable write. gRPC `PutVector` applies the same rule. The
//! key-value routes still accept an already due deadline by contract.

use axum::{Router, body::Body, http::Request};
use base64::{Engine, prelude::BASE64_STANDARD};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use vlite::{
    CreateOptions, Database, FormatVersion, Metadata,
    property_graph::GraphOp,
    server::{BearerToken, ServerOptions, Service, configured_admin_router},
};

const ADMIN: &str = "fake_record_expiry_admin_token_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const READ: &str = "fake_record_expiry_reader_token_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const ROUTES: [&str; 2] = ["/v1/collections/default/vectors", "/v1/vectors"];

struct Reply {
    status: u16,
    etag: Option<String>,
    body: Value,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn database(dimensions: usize, format: FormatVersion) -> Database {
    Database::create_in_memory(
        dimensions,
        CreateOptions {
            format: Some(format),
            ..Default::default()
        },
    )
    .unwrap()
}

fn options() -> ServerOptions {
    ServerOptions {
        admin_token: Some(BearerToken::new(ADMIN).unwrap()),
        read_only_token: Some(BearerToken::new(READ).unwrap()),
        ..Default::default()
    }
}

fn start(db: Database) -> Router {
    configured_admin_router(Service::new(db), options()).unwrap()
}

async fn send(
    app: &Router,
    token: &str,
    method: &str,
    uri: &str,
    if_match: Option<&str>,
    body: Option<Value>,
) -> Reply {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json");
    if let Some(tag) = if_match {
        request = request.header("if-match", tag);
    }
    let body = body.map_or_else(Body::empty, |value| Body::from(value.to_string()));
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let etag = response
        .headers()
        .get("etag")
        .map(|value| value.to_str().unwrap().to_owned());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    Reply { status, etag, body }
}

async fn put(app: &Router, uri: &str, body: Value) -> Reply {
    send(app, ADMIN, "PUT", uri, None, Some(body)).await
}

async fn get(app: &Router, id: u64) -> Reply {
    send(app, ADMIN, "GET", &format!("/v1/vectors/{id}"), None, None).await
}

/// The commit sequence of the file: it advances with every durable write.
async fn head(app: &Router) -> String {
    let reply = send(app, ADMIN, "GET", "/v1/changes?limit=1", None, None).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    reply.body["head"].as_str().unwrap().to_owned()
}

/// The last committed change, which must be a put of record `id`.
async fn last_put(app: &Router, id: u64) -> Value {
    let reply = send(app, ADMIN, "GET", "/v1/changes?limit=10000", None, None).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let change = reply.body["changes"].as_array().unwrap().last().unwrap();
    assert_eq!(change["change"]["type"], "put", "{change}");
    assert_eq!(change["change"]["id"], id.to_string(), "{change}");
    change["change"].clone()
}

fn record(vector: [f32; 2], payload: &str) -> Value {
    json!({
        "vector": vector,
        "payload_base64": BASE64_STANDARD.encode(payload),
        "metadata": {"tag": {"type": "string", "value": payload}}
    })
}

fn with(mut body: Value, fields: &Value) -> Value {
    for (key, value) in fields.as_object().unwrap() {
        body[key] = value.clone();
    }
    body
}

fn due_forms() -> Vec<(&'static str, Value)> {
    let now = now_ms();
    vec![
        ("expires_at_ms 0", json!({"expires_at_ms": 0})),
        ("expires_at_ms 1", json!({"expires_at_ms": 1})),
        ("one second ago", json!({"expires_at_ms": now - 1_000})),
        ("this instant", json!({"expires_at_ms": now})),
        ("ttl_ms 0", json!({"ttl_ms": 0})),
        ("ttl 0", json!({"ttl": 0})),
    ]
}

#[track_caller]
fn assert_refused_as_due(reply: &Reply, context: &str) {
    assert_eq!(reply.status, 422, "{context}: {}", reply.body);
    assert_eq!(reply.body["error"]["code"], "invalid_input", "{context}");
    let message = reply.body["error"]["message"].as_str().unwrap();
    assert!(message.contains("later than the server clock"), "{message}");
    assert_eq!(reply.etag, None, "{context}");
}

#[tokio::test]
async fn a_due_expiry_is_refused_before_anything_is_written() {
    let app = start(database(2, FormatVersion::V5));
    let seeded = put(&app, "/v1/vectors/7", record([1.0, 2.0], "kept")).await;
    assert_eq!(seeded.status, 204);
    let kept = get(&app, 7).await;
    assert_eq!(kept.status, 200);
    assert_eq!(kept.etag, seeded.etag);
    let committed = head(&app).await;

    for route in ROUTES {
        for (name, fields) in due_forms() {
            // The ID that holds a record and an ID that holds none.
            for id in [7, 8] {
                let body = with(record([9.0, 9.0], "replaced"), &fields);
                let reply = put(&app, &format!("{route}/{id}"), body).await;
                assert_refused_as_due(&reply, &format!("{route}/{id} {name}"));
            }
            let after = get(&app, 7).await;
            assert_eq!(after.etag, kept.etag, "{route} {name}");
            assert_eq!(after.body, kept.body, "{route} {name}");
            assert_eq!(get(&app, 8).await.status, 404, "{route} {name}");
            assert_eq!(
                head(&app).await,
                committed,
                "{route} {name}: nothing committed"
            );
        }
    }
}

#[tokio::test]
async fn an_expiry_of_the_wrong_type_is_a_data_error_never_a_server_error() {
    let app = start(database(2, FormatVersion::V5));
    let committed = head(&app).await;
    for route in ROUTES {
        for fields in [
            json!({"ttl_ms": "0"}),
            json!({"ttl": "0"}),
            json!({"expires_at_ms": "0"}),
            json!({"ttl_ms": -1}),
            json!({"ttl_ms": 0.5}),
        ] {
            let reply = put(
                &app,
                &format!("{route}/8"),
                with(record([9.0, 9.0], "x"), &fields),
            )
            .await;
            assert_eq!(reply.status, 422, "{route} {fields}: {}", reply.body);
            assert_eq!(reply.body["error"]["code"], "invalid_json", "{fields}");
        }
        // Both forms together are a shape error, whatever their values.
        let both = json!({"expires_at_ms": 0, "ttl_ms": 0});
        let reply = put(
            &app,
            &format!("{route}/8"),
            with(record([9.0, 9.0], "x"), &both),
        )
        .await;
        assert_eq!(reply.status, 422, "{}", reply.body);
        assert_eq!(reply.body["error"]["code"], "invalid_input");
        let message = reply.body["error"]["message"].as_str().unwrap();
        assert!(message.contains("mutually exclusive"), "{message}");
    }
    assert_eq!(get(&app, 8).await.status, 404);
    assert_eq!(head(&app).await, committed);
}

#[tokio::test]
async fn a_future_expiry_is_accepted_and_the_record_stays_readable() {
    for route in ROUTES {
        let app = start(database(2, FormatVersion::V5));
        let uri = format!("{route}/5");

        let deadline = now_ms() + 60_000;
        let body = with(
            record([3.0, 4.0], "absolute"),
            &json!({"expires_at_ms": deadline}),
        );
        let written = put(&app, &uri, body).await;
        assert_eq!(written.status, 204, "{route}");
        let tag = written.etag.clone().expect("ETag of the written record");
        let read = get(&app, 5).await;
        assert_eq!(read.status, 200, "{route}");
        assert_eq!(read.etag, Some(tag));
        assert_eq!(read.body["vector"], json!([3.0, 4.0]));
        assert_eq!(
            read.body["payload_base64"],
            BASE64_STANDARD.encode("absolute")
        );
        assert_eq!(last_put(&app, 5).await["expires_at_ms"], deadline);

        let before = now_ms();
        let body = with(record([5.0, 6.0], "relative"), &json!({"ttl_ms": 60_000}));
        let written = put(&app, &uri, body).await;
        let after = now_ms();
        assert_eq!(written.status, 204, "{route}");
        let read = get(&app, 5).await;
        assert_eq!(read.status, 200, "{route}");
        assert_eq!(read.etag, written.etag);
        assert_eq!(
            read.body["payload_base64"],
            BASE64_STANDARD.encode("relative")
        );
        let expiry = last_put(&app, 5).await["expires_at_ms"].as_u64().unwrap();
        assert!(
            (before + 60_000..=after + 60_000).contains(&expiry),
            "{expiry}"
        );
    }
}

#[tokio::test]
async fn if_match_is_checked_before_the_expiry_and_a_matching_tag_changes_nothing() {
    let app = start(database(2, FormatVersion::V5));
    let first = put(&app, "/v1/vectors/7", record([1.0, 2.0], "first")).await;
    let current = put(&app, "/v1/vectors/7", record([1.0, 2.0], "second")).await;
    let (stale, current) = (first.etag.unwrap(), current.etag.unwrap());
    assert_ne!(stale, current);
    let committed = head(&app).await;

    let due = with(record([9.0, 9.0], "x"), &json!({"expires_at_ms": 0}));
    for route in ROUTES {
        let uri = format!("{route}/7");
        let reply = send(&app, ADMIN, "PUT", &uri, Some(&stale), Some(due.clone())).await;
        assert_eq!(reply.status, 412, "{route}: {}", reply.body);
        assert_eq!(reply.body["error"]["code"], "precondition_failed");
        let reply = send(&app, ADMIN, "PUT", &uri, Some(&current), Some(due.clone())).await;
        assert_refused_as_due(&reply, route);
    }
    assert_eq!(get(&app, 7).await.etag.as_deref(), Some(current.as_str()));
    assert_eq!(head(&app).await, committed);

    // With a deadline that is later than the clock the same tag succeeds.
    let later = with(record([9.0, 9.0], "later"), &json!({"ttl_ms": 60_000}));
    let reply = send(
        &app,
        ADMIN,
        "PUT",
        "/v1/vectors/7",
        Some(&current),
        Some(later),
    )
    .await;
    assert_eq!(reply.status, 204);
    assert_ne!(reply.etag.as_deref(), Some(current.as_str()));
}

#[tokio::test]
async fn a_pre_v5_file_answers_upgrade_required_before_the_expiry_is_judged() {
    let app = start(database(2, FormatVersion::V4));
    let committed = head(&app).await;
    let mut forms = due_forms();
    forms.push(("far future", json!({"expires_at_ms": now_ms() + 60_000})));
    forms.push(("ttl_ms 60000", json!({"ttl_ms": 60_000})));
    for route in ROUTES {
        for (name, fields) in &forms {
            let body = with(record([9.0, 9.0], "x"), fields);
            let reply = put(&app, &format!("{route}/8"), body).await;
            assert_eq!(reply.status, 409, "{route} {name}: {}", reply.body);
            assert_eq!(reply.body["error"]["code"], "upgrade_required", "{name}");
        }
    }
    assert_eq!(get(&app, 8).await.status, 404);
    assert_eq!(head(&app).await, committed);
}

#[tokio::test]
async fn the_read_only_token_is_refused_before_the_expiry_is_judged() {
    let app = start(database(2, FormatVersion::V5));
    let committed = head(&app).await;
    let mut forms = due_forms();
    forms.push(("ttl_ms 60000", json!({"ttl_ms": 60_000})));
    for route in ROUTES {
        for (name, fields) in &forms {
            let body = with(record([9.0, 9.0], "x"), fields);
            let uri = format!("{route}/8");
            let reply = send(&app, READ, "PUT", &uri, None, Some(body)).await;
            assert_eq!(reply.status, 403, "{route} {name}: {}", reply.body);
            assert_eq!(reply.body["error"]["code"], "read_only", "{name}");
        }
    }
    assert_eq!(head(&app).await, committed);
}

#[tokio::test]
async fn a_read_only_handle_answers_a_due_put_like_any_other_put() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("read-only.vlite");
    let mut db = Database::create_with_format(&path, 2, FormatVersion::V5).unwrap();
    db.put(1, &[1.0, 2.0], b"kept").unwrap();
    db.close().unwrap();
    let app = start(Database::open_read_only(&path).unwrap());

    let answer = |reply: &Reply| (reply.status, reply.body["error"]["code"].clone());
    let plain = put(&app, "/v1/vectors/1", record([9.0, 9.0], "x")).await;
    let future = with(record([9.0, 9.0], "x"), &json!({"ttl_ms": 60_000}));
    let future = put(&app, "/v1/vectors/1", future).await;
    let due = with(record([9.0, 9.0], "x"), &json!({"expires_at_ms": 0}));
    let due = put(&app, "/v1/vectors/1", due).await;
    // The handle refuses every write; the expiry does not change which error.
    assert_ne!(plain.status, 204);
    assert_eq!(answer(&future), answer(&plain));
    assert_eq!(answer(&due), answer(&plain));
    assert_ne!(due.status, 422);
    let kept = get(&app, 1).await;
    assert_eq!(kept.status, 200);
    assert_eq!(kept.body["payload_base64"], BASE64_STANDARD.encode("kept"));
}

#[tokio::test]
async fn a_record_attached_to_a_graph_keeps_its_own_refusal() {
    let mut db = database(2, FormatVersion::V5);
    db.put(1, &[1.0, 1.0], b"attached").unwrap();
    db.create_graph("g").unwrap();
    db.graph_mut("g")
        .unwrap()
        .write_batch(&[
            GraphOp::CreateNode {
                key: "n1".into(),
                labels: vec!["Doc".into()],
                properties: Metadata::new(),
            },
            GraphOp::Attach {
                node: "n1".into(),
                collection: "default".into(),
                record: 1,
            },
        ])
        .unwrap();
    let app = start(db);
    let committed = head(&app).await;
    let mut forms = due_forms();
    forms.push(("ttl_ms 60000", json!({"ttl_ms": 60_000})));
    for route in ROUTES {
        for (name, fields) in &forms {
            let body = with(record([9.0, 9.0], "x"), fields);
            let reply = put(&app, &format!("{route}/1"), body).await;
            assert_eq!(reply.status, 409, "{route} {name}: {}", reply.body);
            assert_eq!(
                reply.body["error"]["code"], "graph_attached_record_expiry",
                "{name}"
            );
        }
    }
    assert_eq!(head(&app).await, committed);
}

#[tokio::test]
async fn the_key_value_route_still_accepts_a_due_deadline() {
    let app = start(database(1, FormatVersion::V5));
    let entry = json!({"namespace":"n","key":"k","value_base64":"YQ==","expires_at_ms":"0"});
    let reply = put(&app, "/v1/collections/default/kv/entry", entry).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.body["expires_at_ms"], "0");
    // The record route of the same collection judges the same deadline differently.
    let reply = put(
        &app,
        "/v1/vectors/1",
        json!({"vector": [0.0], "expires_at_ms": 0}),
    )
    .await;
    assert_refused_as_due(&reply, "record route");
}

#[cfg(feature = "grpc")]
mod grpc {
    use super::*;
    use tonic::{Code, Request, Status, transport::Channel};
    use vlite::server::grpc::{
        ERROR_CODE_TRAILER, grpc_router,
        proto::{self, v_lite_client::VLiteClient},
    };

    struct Harness {
        app: Router,
        client: VLiteClient<Channel>,
        _stop: tokio::sync::oneshot::Sender<()>,
    }

    async fn start_grpc(db: Database) -> Harness {
        let service = Service::new(db);
        let app = configured_admin_router(service.clone(), options()).unwrap();
        let server = grpc_router(service).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(server.serve_with_shutdown(listener, async {
            let _ = stopped.await;
        }));
        let channel = Channel::from_shared(format!("http://{address}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        Harness {
            app,
            client: VLiteClient::new(channel),
            _stop: stop,
        }
    }

    fn request<T>(message: T) -> Request<T> {
        let mut request = Request::new(message);
        request
            .metadata_mut()
            .insert("authorization", format!("Bearer {ADMIN}").parse().unwrap());
        request
    }

    fn put_vector(id: u64, payload: &str, expires_at_ms: Option<u64>) -> proto::PutVectorRequest {
        proto::PutVectorRequest {
            id,
            vector: vec![9.0, 9.0],
            payload: payload.as_bytes().to_vec(),
            expires_at_ms,
            ..Default::default()
        }
    }

    fn get_vector(id: u64) -> proto::GetVectorRequest {
        proto::GetVectorRequest {
            id,
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
    }

    #[tokio::test]
    async fn put_vector_refuses_a_due_expiry_before_anything_is_written() {
        let mut h = start_grpc(database(2, FormatVersion::V5)).await;
        let seeded = h
            .client
            .put_vector(request(put_vector(7, "kept", None)))
            .await
            .unwrap()
            .into_inner();
        let kept = h
            .client
            .get_vector(request(get_vector(7)))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(kept.revision, seeded.revision);
        let committed = head(&h.app).await;

        let now = now_ms();
        for due in [0, 1, now - 1_000, now] {
            for id in [7, 8] {
                let status = h
                    .client
                    .put_vector(request(put_vector(id, "replaced", Some(due))))
                    .await
                    .unwrap_err();
                assert_error(&status, Code::InvalidArgument, "invalid_input");
                assert!(status.message().contains("later than the server clock"));
            }
            let after = h
                .client
                .get_vector(request(get_vector(7)))
                .await
                .unwrap()
                .into_inner();
            assert_eq!(after.revision, seeded.revision, "expiry {due}");
            assert_eq!(after.payload, b"kept");
            assert_eq!(after.expires_at_ms, None);
            let status = h
                .client
                .get_vector(request(get_vector(8)))
                .await
                .unwrap_err();
            assert_eq!(status.code(), Code::NotFound, "expiry {due}");
            assert_eq!(head(&h.app).await, committed, "expiry {due}: no commit");
        }
    }

    #[tokio::test]
    async fn put_vector_accepts_a_future_expiry_and_echoes_it() {
        let mut h = start_grpc(database(2, FormatVersion::V5)).await;
        let deadline = now_ms() + 60_000;
        let written = h
            .client
            .put_vector(request(put_vector(3, "future", Some(deadline))))
            .await
            .unwrap()
            .into_inner();
        assert!(written.created);
        assert_eq!(written.expires_at_ms, Some(deadline));
        let read = h
            .client
            .get_vector(request(get_vector(3)))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(read.revision, written.revision);
        assert_eq!(read.payload, b"future");
        assert_eq!(read.expires_at_ms, Some(deadline));
    }

    #[tokio::test]
    async fn a_stale_precondition_is_judged_before_the_expiry() {
        let mut h = start_grpc(database(2, FormatVersion::V5)).await;
        let seeded = h
            .client
            .put_vector(request(put_vector(7, "kept", None)))
            .await
            .unwrap()
            .into_inner();
        let with_revision = |revision: u64| {
            let mut message = put_vector(7, "replaced", Some(0));
            message.expected = Some(proto::Expected {
                kind: Some(proto::expected::Kind::Revision(revision)),
            });
            message
        };
        let status = h
            .client
            .put_vector(request(with_revision(seeded.revision + 100)))
            .await
            .unwrap_err();
        assert_error(&status, Code::Aborted, "precondition_failed");
        let status = h
            .client
            .put_vector(request(with_revision(seeded.revision)))
            .await
            .unwrap_err();
        assert_error(&status, Code::InvalidArgument, "invalid_input");
        let read = h
            .client
            .get_vector(request(get_vector(7)))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(read.revision, seeded.revision);
    }

    #[tokio::test]
    async fn a_pre_v5_file_answers_failed_precondition_for_any_expiry() {
        let mut h = start_grpc(database(2, FormatVersion::V4)).await;
        let committed = head(&h.app).await;
        for expiry in [0, 1, now_ms() + 60_000] {
            let status = h
                .client
                .put_vector(request(put_vector(8, "x", Some(expiry))))
                .await
                .unwrap_err();
            assert_error(&status, Code::FailedPrecondition, "upgrade_required");
        }
        assert_eq!(head(&h.app).await, committed);
    }
}
