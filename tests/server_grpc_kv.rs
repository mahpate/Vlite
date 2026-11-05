#![cfg(feature = "grpc")]

use axum::{Router, body::Body, http::Request as HttpRequest};
use base64::{Engine, prelude::BASE64_STANDARD};
use http_body_util::BodyExt;
use prost::Message;
use serde_json::{Value, json};
use std::path::Path;
use tonic::{Code, Request, Status, transport::Channel};
use tower::ServiceExt;
use vlite::{
    CollectionSpec, CreateOptions, Database, FormatVersion,
    kv::{KvPut, first_slot},
    server::{
        BearerToken, ServerOptions, Service, configured_admin_router,
        grpc::{
            ERROR_CODE_TRAILER, grpc_router,
            proto::{self, kv_client::KvClient},
        },
    },
};

const ADMIN: &str = "fake_grpc_kv_admin_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const READ: &str = "fake_grpc_kv_reader_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";

struct Harness {
    app: Router,
    service: Service,
    client: KvClient<Channel>,
    health: tonic_health::pb::health_client::HealthClient<Channel>,
    health_control: vlite::server::grpc::GrpcHealth,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

fn database(format: FormatVersion) -> Database {
    let mut db = Database::create_in_memory(
        1,
        CreateOptions {
            format: Some(format),
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

async fn start(db: Database, directory: &Path) -> Harness {
    let service = Service::new(db);
    let app = configured_admin_router(
        service.clone(),
        ServerOptions {
            admin_token: Some(BearerToken::new(ADMIN).unwrap()),
            read_only_token: Some(BearerToken::new(READ).unwrap()),
            admin_directory: Some(directory.into()),
            ..Default::default()
        },
    )
    .unwrap();
    let grpc = grpc_router(service.clone()).await;
    let health_control = grpc.health();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(grpc.serve_with_shutdown(listener, async {
        let _ = rx.await;
    }));
    let channel = Channel::from_shared(format!("http://{address}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    Harness {
        app,
        service: service.clone(),
        client: KvClient::new(channel.clone())
            .max_encoding_message_size(64 * 1024 * 1024)
            .max_decoding_message_size(64 * 1024 * 1024),
        health: tonic_health::pb::health_client::HealthClient::new(channel),
        health_control,
        _shutdown: tx,
    }
}

fn request<T>(token: &str, message: T) -> Request<T> {
    let mut request = Request::new(message);
    request
        .metadata_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    request
}

fn address(collection: &str, namespace: &str, key: &str) -> Option<proto::KvAddress> {
    Some(proto::KvAddress {
        collection: collection.into(),
        namespace: namespace.into(),
        key: key.into(),
    })
}

fn put(key: &str) -> proto::KvPutRequest {
    proto::KvPutRequest {
        address: address("default", "n", key),
        value: b"a".to_vec(),
        ..Default::default()
    }
}

fn list(namespace: &str) -> proto::KvListRequest {
    proto::KvListRequest {
        collection: "default".into(),
        namespace: namespace.into(),
        ..Default::default()
    }
}

fn expected(kind: proto::expected::Kind) -> Option<proto::Expected> {
    Some(proto::Expected { kind: Some(kind) })
}

fn absent() -> Option<proto::Expected> {
    expected(proto::expected::Kind::Absent(proto::expected::Absent {}))
}

fn assert_error(status: Status, code: Code, stable: &str) {
    assert_eq!(status.code(), code, "{status:?}");
    assert_eq!(
        status.metadata().get(ERROR_CODE_TRAILER).unwrap(),
        stable,
        "{status:?}"
    );
    assert!(status.metadata().get("x-request-id").is_some());
}

fn entry_json(entry: proto::KvEntry) -> Value {
    json!({
        "key":entry.key,"value_base64":BASE64_STANDARD.encode(entry.value),
        "version":entry.version.to_string(),
        "expires_at_ms":entry.expires_at_ms.map(|n| n.to_string())
    })
}

async fn rest(app: &Router, token: &str, method: &str, uri: &str, body: Value) -> (u16, Value) {
    let response = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .method(method)
                .uri(uri)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

// These JSON-only parser failures have no protobuf representation. Keeping an
// explicit list makes a newly added golden require a deliberate mapping.
const JSON_ONLY: &[&str] = &[
    "unknown body field",
    "duplicate body field",
    "duplicate optional null",
    "duplicate expected revision",
    "integer deadline",
    "integer revision",
    "integer expected",
    "float expected",
    "boolean expected",
    "null expected",
    "array expected",
    "empty object expected",
    "unknown string expected",
    "deadline leading zero",
    "deadline sign",
    "deadline overflow",
    "base64 missing padding",
    "base64 nonzero trailing bits",
    "base64 url alphabet",
    "base64 whitespace",
    "malformed syntax",
    "duplicate query",
    "unknown query",
    "unknown delete expectation",
    "post not allowed",
    "unknown route",
];

fn golden_expected(value: &Value) -> Option<proto::Expected> {
    match value {
        Value::Null => None,
        Value::String(name) if name == "any" => {
            expected(proto::expected::Kind::Any(proto::expected::Any {}))
        }
        Value::String(name) if name == "absent" => absent(),
        Value::Object(object) => expected(proto::expected::Kind::Revision(
            object["revision"].as_str().unwrap().parse().unwrap(),
        )),
        _ => panic!("unmapped Expected golden: {value}"),
    }
}

async fn golden_call(client: &mut KvClient<Channel>, case: &Value) -> Result<Value, Status> {
    let collection = case["collection"].as_str().unwrap_or("default");
    let query: std::collections::HashMap<String, String> = reqwest::Url::parse(&format!(
        "http://localhost/?{}",
        case["query"].as_str().unwrap_or("")
    ))
    .unwrap()
    .query_pairs()
    .into_owned()
    .collect();
    let namespace = query.get("namespace").map(String::as_str).unwrap_or("");
    let key = query.get("key").map(String::as_str).unwrap_or("");
    match (case["method"].as_str().unwrap(), case["list"] == true) {
        ("GET", false) => client
            .get_kv_entry(request(
                ADMIN,
                proto::KvGetRequest {
                    address: address(collection, namespace, key),
                },
            ))
            .await
            .map(|r| entry_json(r.into_inner())),
        ("GET", true) => client
            .list_kv_entries(request(
                ADMIN,
                proto::KvListRequest {
                    collection: collection.into(),
                    namespace: namespace.into(),
                    prefix: query.get("prefix").cloned().unwrap_or_default(),
                    after: query.get("after").cloned(),
                    limit: query.get("limit").map(|n| n.parse().unwrap()),
                },
            ))
            .await
            .map(|r| {
                let page = r.into_inner();
                json!({"items":page.items.into_iter().map(entry_json).collect::<Vec<_>>(),
                    "next_after":page.next_after})
            }),
        ("PUT", false) => {
            let body = &case["body"];
            client
                .put_kv_entry(request(
                    ADMIN,
                    proto::KvPutRequest {
                        address: address(
                            collection,
                            body["namespace"].as_str().unwrap(),
                            body["key"].as_str().unwrap(),
                        ),
                        value: BASE64_STANDARD
                            .decode(body["value_base64"].as_str().unwrap())
                            .unwrap(),
                        expected: golden_expected(&body["expected"]),
                        expires_at_ms: body["expires_at_ms"].as_str().map(|n| n.parse().unwrap()),
                    },
                ))
                .await
                .map(|r| {
                    let result = r.into_inner();
                    json!({"version":result.version.to_string(),
                        "expires_at_ms":result.expires_at_ms.map(|n|n.to_string())})
                })
        }
        ("DELETE", false) => {
            let guard = match query.get("expected").map(String::as_str) {
                None => None,
                Some("absent") => absent(),
                Some(n) => expected(proto::expected::Kind::Revision(n.parse().unwrap())),
            };
            client
                .delete_kv_entry(request(
                    ADMIN,
                    proto::KvDeleteRequest {
                        address: address(collection, namespace, key),
                        expected: guard,
                    },
                ))
                .await
                .map(|r| json!({"deleted":r.into_inner().deleted}))
        }
        _ => panic!("unmapped golden: {case}"),
    }
}

fn resolve(expected: &mut Value, actual: &Value, seed: u64) {
    match expected {
        Value::String(text) if text == "$seed" => *text = seed.to_string(),
        Value::String(text) if text == "$issued" => {
            let version = actual.as_str().unwrap().parse::<u64>().unwrap();
            assert!(version > seed);
            *text = version.to_string();
        }
        Value::Object(object) => {
            for (key, value) in object {
                resolve(value, &actual[key], seed);
            }
        }
        Value::Array(values) => {
            for (index, value) in values.iter_mut().enumerate() {
                resolve(value, &actual[index], seed);
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn grpc_kv_wire_goldens() {
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/kv/wire/ordinary.json")).unwrap();
    for case in cases {
        if JSON_ONLY.contains(&case["name"].as_str().unwrap()) {
            continue;
        }
        let directory = tempfile::tempdir().unwrap();
        let mut db = database(FormatVersion::V5);
        let seed = db
            .kv_store("default")
            .unwrap()
            .put("n", "seed", &[0, 1, 255], KvPut::default())
            .unwrap();
        let mut h = start(db, directory.path()).await;
        let result = golden_call(&mut h.client, &case).await;
        match case["status"].as_u64().unwrap() {
            200 => {
                let actual = result.unwrap_or_else(|e| panic!("{case}: {e}"));
                let mut expected = case.get("response").unwrap_or(&case["body"]).clone();
                resolve(&mut expected, &actual, seed);
                assert_eq!(actual, expected, "{case}");
            }
            status => assert_error(
                result.unwrap_err(),
                match status {
                    404 => Code::NotFound,
                    409 => Code::FailedPrecondition,
                    412 => Code::Aborted,
                    422 => Code::InvalidArgument,
                    _ => panic!("unmapped status {status}"),
                },
                case["code"].as_str().unwrap(),
            ),
        }
    }
}

#[tokio::test]
async fn grpc_kv_collection_validation_precedes_lookup_and_audit() {
    let directory = tempfile::tempdir().unwrap();
    let mut h = start(database(FormatVersion::V5), directory.path()).await;
    let scoped = credential(&h.app, json!({"operations":["read","write"]})).await;
    let audit_path = directory.path().join("audit.jsonl");
    let before = std::fs::metadata(&audit_path).unwrap().len();
    for collection in [
        "".into(),
        "x".repeat(1_048_576),
        "Upper".into(),
        "a/b".into(),
    ] {
        for token in [ADMIN, scoped.as_str()] {
            assert_error(
                h.client
                    .get_kv_entry(request(
                        token,
                        proto::KvGetRequest {
                            address: address(&collection, "n", "k"),
                        },
                    ))
                    .await
                    .unwrap_err(),
                Code::InvalidArgument,
                "invalid_input",
            );
            let mut input = put("k");
            input.address = address(&collection, "n", "k");
            assert_error(
                h.client
                    .put_kv_entry(request(token, input))
                    .await
                    .unwrap_err(),
                Code::InvalidArgument,
                "invalid_input",
            );
            assert_error(
                h.client
                    .delete_kv_entry(request(
                        token,
                        proto::KvDeleteRequest {
                            address: address(&collection, "n", "k"),
                            expected: None,
                        },
                    ))
                    .await
                    .unwrap_err(),
                Code::InvalidArgument,
                "invalid_input",
            );
            let mut input = list("n");
            input.collection = collection.clone();
            assert_error(
                h.client
                    .list_kv_entries(request(token, input))
                    .await
                    .unwrap_err(),
                Code::InvalidArgument,
                "invalid_input",
            );
        }
    }
    assert!(std::fs::metadata(audit_path).unwrap().len() - before <= 512);
}

#[tokio::test]
async fn grpc_kv_oversize_value_rejected_before_json_validation() {
    let directory = tempfile::tempdir().unwrap();
    let mut h = start(database(FormatVersion::V5), directory.path()).await;
    let mut input = put("k");
    input.address.as_mut().unwrap().namespace.clear();
    input.value = vec![0; 1_047_291];
    let error = h
        .client
        .put_kv_entry(request(ADMIN, input))
        .await
        .unwrap_err();
    assert_eq!(error.message(), "KV value exceeds 1047290 decoded bytes");
    assert_error(error, Code::InvalidArgument, "invalid_input");
}

#[tokio::test]
async fn grpc_kv_utf8_and_value_bounds() {
    let directory = tempfile::tempdir().unwrap();
    let mut h = start(database(FormatVersion::V5), directory.path()).await;
    for field in ["namespace", "key"] {
        let maximum = if field == "namespace" { 255 } else { 1024 };
        for size in [0, 1, maximum, maximum + 1] {
            for unicode in [false, true] {
                let text = if unicode {
                    format!("{}{}", "\u{e9}".repeat(size / 2), "x".repeat(size % 2))
                } else {
                    "x".repeat(size)
                };
                let mut input = put("k");
                let address = input.address.as_mut().unwrap();
                if field == "namespace" {
                    address.namespace = text;
                } else {
                    address.key = text;
                }
                let result = h.client.put_kv_entry(request(ADMIN, input)).await;
                if size == 0 || size > maximum {
                    assert_error(result.unwrap_err(), Code::InvalidArgument, "invalid_input");
                } else {
                    assert!(result.unwrap().into_inner().version > 0);
                }
            }
        }
    }
    for size in [0, 1_047_290, 1_047_291] {
        let mut input = put("value");
        input.value = vec![255; size];
        let result = h.client.put_kv_entry(request(ADMIN, input)).await;
        if size == 1_047_291 {
            assert_error(result.unwrap_err(), Code::InvalidArgument, "invalid_input");
        } else {
            result.unwrap();
            let found = h
                .client
                .get_kv_entry(request(
                    ADMIN,
                    proto::KvGetRequest {
                        address: address("default", "n", "value"),
                    },
                ))
                .await
                .unwrap()
                .into_inner();
            assert_eq!(found.value, vec![255; size]);
        }
    }
    for field in ["prefix", "after"] {
        for size in [0, 1, 1024, 1025] {
            let mut input = list("n");
            if field == "prefix" {
                input.prefix = "x".repeat(size);
            } else {
                input.after = Some("x".repeat(size));
            }
            let result = h.client.list_kv_entries(request(ADMIN, input)).await;
            if size > 1024 || (field == "after" && size == 0) {
                assert_error(result.unwrap_err(), Code::InvalidArgument, "invalid_input");
            } else {
                result.unwrap();
            }
        }
    }
}

#[tokio::test]
async fn grpc_kv_expected_expiry_and_cas() {
    let directory = tempfile::tempdir().unwrap();
    let mut h = start(database(FormatVersion::V5), directory.path()).await;
    let mut input = put("k");
    input.expected = absent();
    input.expires_at_ms = Some(u64::MAX);
    let first = h
        .client
        .put_kv_entry(request(ADMIN, input.clone()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(first.expires_at_ms, Some(18_446_744_073_709_551_615));
    input.expected = expected(proto::expected::Kind::Revision(first.version));
    input.expires_at_ms = None;
    let mut other = h.client.clone();
    let (a, b) = tokio::join!(
        h.client.put_kv_entry(request(ADMIN, input.clone())),
        other.put_kv_entry(request(ADMIN, input.clone()))
    );
    let current = match (a, b) {
        (Ok(a), Err(b)) | (Err(b), Ok(a)) => {
            assert_error(b, Code::Aborted, "precondition_failed");
            a.into_inner()
        }
        result => panic!("exactly one CAS must win: {result:?}"),
    };
    assert!(current.version > first.version);
    assert_eq!(current.expires_at_ms, None);
    let found = h
        .client
        .get_kv_entry(request(
            ADMIN,
            proto::KvGetRequest {
                address: input.address.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(found.expires_at_ms, None);
    assert_eq!(found.version, current.version);
    let deletion = proto::KvDeleteRequest {
        address: input.address.clone(),
        expected: expected(proto::expected::Kind::Revision(current.version)),
    };
    assert!(
        h.client
            .delete_kv_entry(request(ADMIN, deletion.clone()))
            .await
            .unwrap()
            .into_inner()
            .deleted
    );
    assert_error(
        h.client
            .delete_kv_entry(request(ADMIN, deletion))
            .await
            .unwrap_err(),
        Code::Aborted,
        "precondition_failed",
    );
    input.expected = absent();
    input.expires_at_ms = Some(0);
    assert_eq!(
        h.client
            .put_kv_entry(request(ADMIN, input.clone()))
            .await
            .unwrap()
            .into_inner()
            .expires_at_ms,
        Some(0)
    );
    assert_error(
        h.client
            .get_kv_entry(request(
                ADMIN,
                proto::KvGetRequest {
                    address: input.address.clone(),
                },
            ))
            .await
            .unwrap_err(),
        Code::NotFound,
        "kv_not_found",
    );
    input.expires_at_ms = None;
    h.client
        .put_kv_entry(request(ADMIN, input.clone()))
        .await
        .unwrap();
    input.expected = expected(proto::expected::Kind::Revision(u64::MAX));
    assert_error(
        h.client
            .put_kv_entry(request(ADMIN, input.clone()))
            .await
            .unwrap_err(),
        Code::Aborted,
        "precondition_failed",
    );
    input.expected = Some(proto::Expected { kind: None });
    h.client
        .put_kv_entry(request(ADMIN, input.clone()))
        .await
        .unwrap();
    for deleted in [true, false] {
        assert_eq!(
            h.client
                .delete_kv_entry(request(
                    ADMIN,
                    proto::KvDeleteRequest {
                        address: input.address.clone(),
                        expected: expected(proto::expected::Kind::Any(proto::expected::Any {})),
                    },
                ))
                .await
                .unwrap()
                .into_inner()
                .deleted,
            deleted
        );
    }
}

#[tokio::test]
async fn grpc_kv_list_order_defaults_and_page_boundary() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = database(FormatVersion::V5);
    let mut store = db.kv_store("default").unwrap();
    for i in 0..101 {
        store
            .put("n", &format!("{i:03}"), b"", KvPut::default())
            .unwrap();
    }
    for key in ["a/b", "a+", "a ", "a\u{e9}", "a\u{1f600}", "b"] {
        store.put("ordered", key, b"", KvPut::default()).unwrap();
    }
    for i in 0..13 {
        store
            .put(
                "big",
                &format!("{i:03}"),
                &vec![255; 1_047_290],
                KvPut::default(),
            )
            .unwrap();
    }
    let mut h = start(db, directory.path()).await;
    let page = h
        .client
        .list_kv_entries(request(ADMIN, list("n")))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(page.items.len(), 100);
    assert_eq!(page.next_after.as_deref(), Some("099"));
    let mut input = list("ordered");
    input.prefix = "a".into();
    input.after = Some("a ".into());
    input.limit = Some(2);
    let page = h
        .client
        .list_kv_entries(request(ADMIN, input.clone()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        page.items
            .iter()
            .map(|e| e.key.as_str())
            .collect::<Vec<_>>(),
        ["a+", "a/b"]
    );
    assert_eq!(page.next_after.as_deref(), Some("a/b"));
    input.after = page.next_after;
    let page = h
        .client
        .list_kv_entries(request(ADMIN, input))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        page.items
            .iter()
            .map(|e| e.key.as_str())
            .collect::<Vec<_>>(),
        ["a\u{e9}", "a\u{1f600}"]
    );
    assert_eq!(page.next_after, None);
    let mut input = list("big");
    input.limit = Some(13);
    let page = h
        .client
        .list_kv_entries(request(ADMIN, input.clone()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(page.items.len(), 12);
    assert_eq!(page.next_after.as_deref(), Some("011"));
    assert!((12_500_000..=16_777_216).contains(&page.encoded_len()));
    for (index, item) in page.items.iter().enumerate() {
        assert_eq!(item.key, format!("{index:03}"));
        assert_eq!(item.value, vec![255; 1_047_290]);
    }
    let canonical = json!({"items":page.items.into_iter().map(entry_json).collect::<Vec<_>>(),
        "next_after":page.next_after});
    let bytes = serde_json::to_vec(&canonical).unwrap().len();
    assert!((16_750_000..=16_777_216).contains(&bytes));
    input.after = Some("011".into());
    let page = h
        .client
        .list_kv_entries(request(ADMIN, input))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].key, "012");
    assert_eq!(page.next_after, None);
}

#[tokio::test]
async fn grpc_kv_exact_page_byte_boundary() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = database(FormatVersion::V5);
    for _ in 0..32 {
        db.kv_store("default")
            .unwrap()
            .put("setup", "revision-width", b"", KvPut::default())
            .unwrap();
    }
    let mut oracle_items = Vec::new();
    for i in 0..12 {
        let key = format!("{i:03}");
        let value = vec![255; 1_047_290];
        let version = db
            .kv_store("default")
            .unwrap()
            .put(
                "exact",
                &key,
                &value,
                KvPut {
                    expires_at_ms: (i == 0).then_some(1_000_000_000_000_000_000),
                    ..Default::default()
                },
            )
            .unwrap();
        oracle_items.push(
            json!({"key":key,"value_base64":BASE64_STANDARD.encode(value),
            "version":version.to_string(),
            "expires_at_ms":(i == 0).then_some("1000000000000000000")}),
        );
    }
    // Reserve a final entry, then fill its value and key using only the
    // contract's JSON encoding. Versions are opaque seeded state, not counts.
    let version = db
        .kv_store("default")
        .unwrap()
        .put("exact", "012", b"", KvPut::default())
        .unwrap();
    let version_width = version.to_string().len();
    let mut final_key = "012".to_owned();
    let mut final_item = json!({"key":final_key,"value_base64":"",
        "version":version.to_string(),"expires_at_ms":null});
    oracle_items.push(final_item.clone());
    let empty_bytes = serde_json::to_vec(&json!({"items":oracle_items,"next_after":final_key}))
        .unwrap()
        .len();
    let remaining = 16_777_216 - empty_bytes;
    // Replacing null (4 bytes) with a quoted 19-digit future deadline (21)
    // flips parity. The final key occurs twice, permitting the other residue.
    let expiry = (remaining % 2 == 1).then_some(1_000_000_000_000_000_000u64);
    let expiry_bytes = if expiry.is_some() { 17 } else { 0 };
    let extra_key_bytes = (remaining - expiry_bytes) % 4 / 2;
    final_key.push_str(&"x".repeat(extra_key_bytes));
    let encoded_bytes = remaining - expiry_bytes - 2 * extra_key_bytes;
    assert_eq!(encoded_bytes % 4, 0);
    let final_value = vec![255; encoded_bytes / 4 * 3];
    let version = db
        .kv_store("default")
        .unwrap()
        .put(
            "exact",
            &final_key,
            &final_value,
            KvPut {
                expires_at_ms: expiry,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(version.to_string().len(), version_width);
    if final_key != "012" {
        db.kv_store("default")
            .unwrap()
            .delete("exact", "012", vlite::Expected::Any)
            .unwrap();
    }
    final_item["key"] = json!(final_key);
    final_item["value_base64"] = json!(BASE64_STANDARD.encode(&final_value));
    final_item["version"] = json!(version.to_string());
    final_item["expires_at_ms"] = json!(expiry.map(|value| value.to_string()));
    *oracle_items.last_mut().unwrap() = final_item;
    db.kv_store("default")
        .unwrap()
        .put("exact", "013", b"", KvPut::default())
        .unwrap();
    let oracle = json!({"items":oracle_items,"next_after":final_key});
    assert_eq!(serde_json::to_vec(&oracle).unwrap().len(), 16_777_216);
    let mut h = start(db, directory.path()).await;
    let page = h
        .client
        .list_kv_entries(request(ADMIN, list("exact")))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(page.items.len(), 13);
    assert_eq!(page.next_after.as_deref(), Some(final_key.as_str()));
    let first_width = oracle["items"][0]["version"].as_str().unwrap().len();
    assert_eq!(
        json!({"items":page.items.into_iter().map(entry_json).collect::<Vec<_>>(),
            "next_after":page.next_after}),
        oracle
    );
    // One more deadline digit makes the same 13 entries exactly one byte too
    // large. Revision width is checked rather than assuming allocation counts.
    let mut input = put("000");
    input.address = address("default", "exact", "000");
    input.value = vec![255; 1_047_290];
    input.expires_at_ms = Some(18_446_744_073_709_551_615);
    let version = h
        .client
        .put_kv_entry(request(ADMIN, input))
        .await
        .unwrap()
        .into_inner()
        .version;
    assert_eq!(version.to_string().len(), first_width);
    let page = h
        .client
        .list_kv_entries(request(ADMIN, list("exact")))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(page.items.len(), 12);
    assert_eq!(page.next_after.as_deref(), Some("011"));
}

#[tokio::test]
async fn grpc_kv_empty_collection_and_missing_address() {
    let directory = tempfile::tempdir().unwrap();
    let mut h = start(database(FormatVersion::V5), directory.path()).await;
    let scoped = credential(&h.app, json!({"operations":["read","write"]})).await;
    for target in [None, address("", "n", "k")] {
        assert_error(
            h.client
                .get_kv_entry(request(
                    &scoped,
                    proto::KvGetRequest {
                        address: target.clone(),
                    },
                ))
                .await
                .unwrap_err(),
            Code::InvalidArgument,
            "invalid_input",
        );
        let mut input = put("k");
        input.address = target.clone();
        assert_error(
            h.client
                .put_kv_entry(request(&scoped, input))
                .await
                .unwrap_err(),
            Code::InvalidArgument,
            "invalid_input",
        );
        assert_error(
            h.client
                .delete_kv_entry(request(
                    &scoped,
                    proto::KvDeleteRequest {
                        address: target,
                        expected: None,
                    },
                ))
                .await
                .unwrap_err(),
            Code::InvalidArgument,
            "invalid_input",
        );
    }
    let mut input = list("n");
    input.collection.clear();
    assert_error(
        h.client
            .list_kv_entries(request(&scoped, input))
            .await
            .unwrap_err(),
        Code::InvalidArgument,
        "invalid_input",
    );
}

#[tokio::test]
async fn grpc_kv_slots_and_upgrade_status() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = database(FormatVersion::V5);
    let slot = first_slot("n", "occupied");
    for i in 0..16 {
        db.put(slot.wrapping_add(i), &[0.0], b"occupied").unwrap();
    }
    let mut h = start(db, directory.path()).await;
    assert_error(
        h.client
            .put_kv_entry(request(ADMIN, put("occupied")))
            .await
            .unwrap_err(),
        Code::FailedPrecondition,
        "kv_slots_exhausted",
    );
    let directory = tempfile::tempdir().unwrap();
    let mut h = start(database(FormatVersion::V4), directory.path()).await;
    let mut input = put("k");
    input.expires_at_ms = Some(1);
    assert_error(
        h.client
            .put_kv_entry(request(ADMIN, input))
            .await
            .unwrap_err(),
        Code::FailedPrecondition,
        "upgrade_required",
    );
}

async fn credential(app: &Router, extra: Value) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let mut input = json!({"name":"grpc-kv","collections":["default"],"operations":["read"],"expires_unix_ms":now+600_000});
    for (key, value) in extra.as_object().unwrap() {
        input[key] = value.clone();
    }
    let (status, body) = rest(app, ADMIN, "POST", "/v1/admin/credentials", input).await;
    assert_eq!(status, 201, "{body}");
    body["token"].as_str().unwrap().into()
}

#[tokio::test]
async fn grpc_kv_scopes_binding_quota_tenancy_and_audit() {
    let directory = tempfile::tempdir().unwrap();
    let mut h = start(database(FormatVersion::V5), directory.path()).await;
    let reader = credential(&h.app, json!({})).await;
    let writer = credential(&h.app, json!({"operations":["write"],"max_vectors":1})).await;
    let graph = credential(&h.app, json!({"operations":["graph_read"],"graphs":["*"]})).await;
    for token in [&reader, READ, &graph] {
        assert_error(
            h.client
                .put_kv_entry(request(token, put("k")))
                .await
                .unwrap_err(),
            Code::PermissionDenied,
            if token == READ {
                "read_only"
            } else {
                "forbidden_scope"
            },
        );
        assert_error(
            h.client
                .delete_kv_entry(request(
                    token,
                    proto::KvDeleteRequest {
                        address: address("default", "n", "k"),
                        expected: None,
                    },
                ))
                .await
                .unwrap_err(),
            Code::PermissionDenied,
            if token == READ {
                "read_only"
            } else {
                "forbidden_scope"
            },
        );
    }
    for _ in 0..2 {
        h.client
            .put_kv_entry(request(&writer, put("k")))
            .await
            .unwrap();
    }
    let quota = h
        .client
        .put_kv_entry(request(&writer, put("second")))
        .await
        .unwrap_err();
    assert_eq!(quota.metadata().get("retry-after").unwrap(), "1");
    assert_error(quota, Code::ResourceExhausted, "quota_exceeded");
    for token in [&reader, READ] {
        h.client
            .get_kv_entry(request(
                token,
                proto::KvGetRequest {
                    address: address("default", "n", "k"),
                },
            ))
            .await
            .unwrap();
        h.client
            .list_kv_entries(request(token, list("n")))
            .await
            .unwrap();
    }
    for token in [&writer, &graph] {
        assert_error(
            h.client
                .get_kv_entry(request(
                    token,
                    proto::KvGetRequest {
                        address: address("default", "n", "k"),
                    },
                ))
                .await
                .unwrap_err(),
            Code::PermissionDenied,
            "forbidden_scope",
        );
        assert_error(
            h.client
                .list_kv_entries(request(token, list("n")))
                .await
                .unwrap_err(),
            Code::PermissionDenied,
            "forbidden_scope",
        );
    }
    for collection in ["wide", "missing"] {
        assert_error(
            h.client
                .get_kv_entry(request(
                    &reader,
                    proto::KvGetRequest {
                        address: address(collection, "n", "secret"),
                    },
                ))
                .await
                .unwrap_err(),
            Code::PermissionDenied,
            "forbidden_scope",
        );
        let mut input = list("n");
        input.collection = collection.into();
        assert_error(
            h.client
                .list_kv_entries(request(&reader, input))
                .await
                .unwrap_err(),
            Code::PermissionDenied,
            "forbidden_scope",
        );
        let mut input = put("k");
        input.address = address(collection, "n", "k");
        assert_error(
            h.client
                .put_kv_entry(request(&writer, input))
                .await
                .unwrap_err(),
            Code::PermissionDenied,
            "forbidden_scope",
        );
        assert_error(
            h.client
                .delete_kv_entry(request(
                    &writer,
                    proto::KvDeleteRequest {
                        address: address(collection, "n", "k"),
                        expected: None,
                    },
                ))
                .await
                .unwrap_err(),
            Code::PermissionDenied,
            "forbidden_scope",
        );
    }
    h.client
        .delete_kv_entry(request(
            &writer,
            proto::KvDeleteRequest {
                address: address("default", "n", "k"),
                expected: None,
            },
        ))
        .await
        .unwrap();
    let mut tenants = Vec::new();
    for name in ["one", "two"] {
        let (status, tenant) = rest(
            &h.app,
            ADMIN,
            "POST",
            "/v1/admin/tenants",
            json!({"name":name}),
        )
        .await;
        assert_eq!(status, 201, "{tenant}");
        let token = credential(&h.app,json!({"tenant_id":tenant["tenant"]["id"],"collections":["*"],"operations":["collections","read","write"]})).await;
        let (status, body) = rest(
            &h.app,
            &token,
            "POST",
            "/v1/collections",
            json!({"name":"items","dimensions":1}),
        )
        .await;
        assert_eq!(status, 201, "{body}");
        assert_error(
            h.client
                .get_kv_entry(request(
                    &token,
                    proto::KvGetRequest {
                        address: address("items", "n", "k"),
                    },
                ))
                .await
                .unwrap_err(),
            Code::NotFound,
            "kv_not_found",
        );
        let mut input = put("k");
        input.address = address("items", "n", "k");
        input.value = name.as_bytes().to_vec();
        h.client.put_kv_entry(request(&token, input)).await.unwrap();
        let found = h
            .client
            .get_kv_entry(request(
                &token,
                proto::KvGetRequest {
                    address: address("items", "n", "k"),
                },
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(found.value, name.as_bytes());
        tenants.push((name, token, found.version));
    }
    for (name, token, version) in tenants {
        let mut input = list("n");
        input.collection = "items".into();
        let page = h
            .client
            .list_kv_entries(request(&token, input))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(page.next_after, None);
        assert_eq!(
            page.items,
            vec![proto::KvEntry {
                key: "k".into(),
                value: name.as_bytes().to_vec(),
                version,
                expires_at_ms: None,
            }]
        );
        let input = proto::KvDeleteRequest {
            address: address("items", "n", "k"),
            expected: expected(proto::expected::Kind::Revision(version)),
        };
        assert!(
            h.client
                .delete_kv_entry(request(&token, input.clone()))
                .await
                .unwrap()
                .into_inner()
                .deleted
        );
        assert_error(
            h.client
                .delete_kv_entry(request(&token, input))
                .await
                .unwrap_err(),
            Code::Aborted,
            "precondition_failed",
        );
    }
    let audit = std::fs::read_to_string(directory.path().join("audit.jsonl")).unwrap();
    let events: Vec<Value> = audit
        .lines()
        .map(|s| serde_json::from_str::<Value>(s).unwrap())
        .filter(|e| matches!(e["event"].as_str(), Some("kv_put" | "kv_delete")))
        .collect();
    for operation in ["kv_put", "kv_delete"] {
        assert!(
            events
                .iter()
                .any(|e| e["event"] == operation && e["outcome"] == "ok")
        );
    }
    assert!(events.iter().any(|e| e["outcome"] == "quota_exceeded"));
    for event in events {
        for field in [
            "request_id",
            "principal",
            "collection",
            "outcome",
            "duration_ns",
        ] {
            assert!(event.get(field).is_some());
        }
        for forbidden in [
            "namespace",
            "key",
            "value",
            "value_base64",
            "token",
            "payload",
        ] {
            assert!(event.get(forbidden).is_none());
        }
    }
}

#[tokio::test]
async fn grpc_kv_nondefault_binding_survives_neither_recreation_nor_new_lineage() {
    let directory = tempfile::tempdir().unwrap();
    let mut h = start(database(FormatVersion::V5), directory.path()).await;
    let (status, body) = rest(
        &h.app,
        ADMIN,
        "POST",
        "/v1/collections",
        json!({"name":"items","dimensions":1}),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let token = credential(
        &h.app,
        json!({"collections":["items"],"operations":["read","write"]}),
    )
    .await;
    let mut input = put("k");
    input.address = address("items", "n", "k");
    input.value = b"bound".to_vec();
    let version = h
        .client
        .put_kv_entry(request(&token, input.clone()))
        .await
        .unwrap()
        .into_inner()
        .version;
    let mut query = list("n");
    query.collection = "items".into();
    let page = h
        .client
        .list_kv_entries(request(&token, query.clone()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(page.next_after, None);
    assert_eq!(
        page.items,
        vec![proto::KvEntry {
            key: "k".into(),
            value: b"bound".to_vec(),
            version,
            expires_at_ms: None,
        }]
    );
    let deletion = proto::KvDeleteRequest {
        address: input.address.clone(),
        expected: expected(proto::expected::Kind::Any(proto::expected::Any {})),
    };
    assert!(
        h.client
            .delete_kv_entry(request(&token, deletion.clone()))
            .await
            .unwrap()
            .into_inner()
            .deleted
    );
    assert!(
        !h.client
            .delete_kv_entry(request(&token, deletion.clone()))
            .await
            .unwrap()
            .into_inner()
            .deleted
    );
    for (method, body, expected_status) in [
        ("DELETE", Value::Null, 200),
        ("POST", json!({"name":"items","dimensions":1}), 201),
    ] {
        let uri = if method == "DELETE" {
            "/v1/collections/items"
        } else {
            "/v1/collections"
        };
        let (status, body) = rest(&h.app, ADMIN, method, uri, body).await;
        assert_eq!(status, expected_status, "{body}");
    }
    h.client
        .put_kv_entry(request(ADMIN, input.clone()))
        .await
        .unwrap();
    for replacement_lineage in [false, true] {
        if replacement_lineage {
            h.service.close().await.unwrap();
            drop(h);
            let mut db = database(FormatVersion::V5);
            db.create_collection(
                "items",
                CollectionSpec {
                    dimensions: 1,
                    settings: Default::default(),
                },
            )
            .unwrap();
            db.kv_store("items")
                .unwrap()
                .put("n", "k", b"replacement", KvPut::default())
                .unwrap();
            h = start(db, directory.path()).await;
        }
        assert_error(
            h.client
                .get_kv_entry(request(
                    &token,
                    proto::KvGetRequest {
                        address: input.address.clone(),
                    },
                ))
                .await
                .unwrap_err(),
            Code::PermissionDenied,
            "forbidden_scope",
        );
        assert_error(
            h.client
                .put_kv_entry(request(&token, input.clone()))
                .await
                .unwrap_err(),
            Code::PermissionDenied,
            "forbidden_scope",
        );
        assert_error(
            h.client
                .list_kv_entries(request(&token, query.clone()))
                .await
                .unwrap_err(),
            Code::PermissionDenied,
            "forbidden_scope",
        );
        assert_error(
            h.client
                .delete_kv_entry(request(&token, deletion.clone()))
                .await
                .unwrap_err(),
            Code::PermissionDenied,
            "forbidden_scope",
        );
        let entry = h
            .client
            .get_kv_entry(request(
                ADMIN,
                proto::KvGetRequest {
                    address: input.address.clone(),
                },
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            entry.value,
            if replacement_lineage {
                b"replacement".as_slice()
            } else {
                b"bound".as_slice()
            }
        );
    }
}

#[tokio::test]
async fn grpc_kv_request_ids_audit_and_route_labels() {
    let directory = tempfile::tempdir().unwrap();
    let mut h = start(database(FormatVersion::V5), directory.path()).await;
    let put_response = h
        .client
        .put_kv_entry(request(ADMIN, put("k")))
        .await
        .unwrap();
    let put_id = put_response
        .metadata()
        .get("x-request-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let get_response = h
        .client
        .get_kv_entry(request(
            ADMIN,
            proto::KvGetRequest {
                address: address("default", "n", "k"),
            },
        ))
        .await
        .unwrap();
    let list_response = h
        .client
        .list_kv_entries(request(ADMIN, list("n")))
        .await
        .unwrap();
    let delete_response = h
        .client
        .delete_kv_entry(request(
            ADMIN,
            proto::KvDeleteRequest {
                address: address("default", "n", "k"),
                expected: None,
            },
        ))
        .await
        .unwrap();
    let delete_id = delete_response
        .metadata()
        .get("x-request-id")
        .unwrap()
        .to_str()
        .unwrap();
    let mut ids = vec![put_id.as_str(), delete_id];
    for response_id in [
        get_response.metadata().get("x-request-id").unwrap(),
        list_response.metadata().get("x-request-id").unwrap(),
    ] {
        ids.push(response_id.to_str().unwrap());
    }
    assert!(ids.iter().all(|id| !id.is_empty()));
    assert_eq!(
        ids.iter().collect::<std::collections::HashSet<_>>().len(),
        4
    );
    let audit = std::fs::read_to_string(directory.path().join("audit.jsonl")).unwrap();
    let events: Vec<Value> = audit
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for (operation, request_id) in [("kv_put", put_id.as_str()), ("kv_delete", delete_id)] {
        let matching: Vec<_> = events.iter().filter(|e| e["event"] == operation).collect();
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0]["request_id"], request_id);
        assert_eq!(matching[0]["collection"], "default");
        assert_eq!(matching[0]["outcome"], "ok");
    }
    let (status, summary) = rest(
        &h.app,
        ADMIN,
        "GET",
        "/v1/admin/metrics/summary",
        Value::Null,
    )
    .await;
    assert_eq!(status, 200, "{summary}");
    let routes = summary["requests"]["by_route"].as_array().unwrap();
    for rpc in ["GetKvEntry", "PutKvEntry", "DeleteKvEntry", "ListKvEntries"] {
        let label = format!("grpc:/vlite.v1.Kv/{rpc}");
        let route = routes.iter().find(|r| r["route"] == label).unwrap();
        assert_eq!(route["method"], "grpc");
        assert_eq!(route["count"], 1);
        assert_eq!(route["errors"], 0);
    }
}

#[tokio::test]
async fn grpc_kv_authentication() {
    let directory = tempfile::tempdir().unwrap();
    let mut h = start(database(FormatVersion::V5), directory.path()).await;
    assert_error(
        h.client
            .get_kv_entry(Request::new(proto::KvGetRequest {
                address: address("default", "n", "k"),
            }))
            .await
            .unwrap_err(),
        Code::Unauthenticated,
        "unauthorized",
    );
    assert_error(
        h.client
            .put_kv_entry(Request::new(put("k")))
            .await
            .unwrap_err(),
        Code::Unauthenticated,
        "unauthorized",
    );
    assert_error(
        h.client
            .delete_kv_entry(Request::new(proto::KvDeleteRequest {
                address: address("default", "n", "k"),
                expected: None,
            }))
            .await
            .unwrap_err(),
        Code::Unauthenticated,
        "unauthorized",
    );
    assert_error(
        h.client
            .list_kv_entries(Request::new(list("n")))
            .await
            .unwrap_err(),
        Code::Unauthenticated,
        "unauthorized",
    );
}

#[tokio::test]
async fn grpc_kv_read_only_handle() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("readonly.vlite");
    Database::create_with_format(&path, 1, FormatVersion::V5)
        .unwrap()
        .close()
        .unwrap();
    let mut h = start(
        Database::open_read_only(&path).unwrap(),
        &directory.path().join("admin"),
    )
    .await;
    assert_error(
        h.client
            .put_kv_entry(request(ADMIN, put("k")))
            .await
            .unwrap_err(),
        Code::PermissionDenied,
        "read_only",
    );
    assert_error(
        h.client
            .delete_kv_entry(request(
                ADMIN,
                proto::KvDeleteRequest {
                    address: address("default", "n", "missing"),
                    expected: None,
                },
            ))
            .await
            .unwrap_err(),
        Code::PermissionDenied,
        "read_only",
    );
    h.client
        .list_kv_entries(request(ADMIN, list("n")))
        .await
        .unwrap();
}

#[tokio::test]
async fn grpc_kv_health_and_transport_limits() {
    let directory = tempfile::tempdir().unwrap();
    let mut h = start(database(FormatVersion::V5), directory.path()).await;
    assert_eq!(
        h.health
            .check(tonic_health::pb::HealthCheckRequest {
                service: "vlite.v1.Kv".into()
            })
            .await
            .unwrap()
            .into_inner()
            .status,
        1
    );
    // KvAddress(default, n, k) occupies 17 bytes including its envelope;
    // the value tag and four-byte length add five more.
    for size in [8_388_608, 8_388_609] {
        let mut input = put("k");
        input.value = vec![0; size - 22];
        assert_eq!(input.encoded_len(), size);
        let error = h
            .client
            .put_kv_entry(request(ADMIN, input))
            .await
            .unwrap_err();
        if size == 8_388_608 {
            assert_error(error, Code::InvalidArgument, "invalid_input");
        } else {
            assert_eq!(error.code(), Code::OutOfRange);
            assert!(error.metadata().get(ERROR_CODE_TRAILER).is_none());
        }
    }
    h.health_control.set_not_serving().await;
    for service in ["", "vlite.v1.VLite", "vlite.v1.Kv"] {
        assert_eq!(
            h.health
                .check(tonic_health::pb::HealthCheckRequest {
                    service: service.into()
                })
                .await
                .unwrap()
                .into_inner()
                .status,
            2
        );
    }
    h.client
        .put_kv_entry(request(ADMIN, put("k")))
        .await
        .unwrap();
}

#[tokio::test]
async fn grpc_kv_rest_parity_trace() {
    let rest_dir = tempfile::tempdir().unwrap();
    let grpc_dir = tempfile::tempdir().unwrap();
    let a = start(database(FormatVersion::V5), rest_dir.path()).await;
    let mut b = start(database(FormatVersion::V5), grpc_dir.path()).await;
    // Deadlines 0 and MAX are clock-independent; both databases have identical
    // initial collections. Versions are opaque, compared by equality relations.
    let trace = json!([
        {"method":"PUT","body":{"namespace":"n","key":"k","value_base64":"YQ==","expected":"absent","expires_at_ms":"18446744073709551615"}},
        {"method":"GET","query":"namespace=n&key=k"},
        {"method":"PUT","body":{"namespace":"n","key":"k","value_base64":"","expected":"absent"}},
        {"method":"PUT","body":{"namespace":"n","key":"k","value_base64":""}},
        {"method":"GET","list":true,"query":"namespace=n&limit=1"},
        {"method":"DELETE","query":"namespace=n&key=k&expected=0"},
        {"method":"DELETE","query":"namespace=n&key=k"},
        {"method":"GET","query":"namespace=n&key=k"},
        {"method":"PUT","body":{"namespace":"n","key":"k","value_base64":"","expires_at_ms":"0"}},
        {"method":"GET","list":true,"query":"namespace=n"},
        {"method":"DELETE","query":"namespace=n&key=k&expected=absent"}
    ]);
    let mut rest_versions = Vec::new();
    let mut grpc_versions = Vec::new();
    for case in trace.as_array().unwrap() {
        let suffix = if case["list"] == true {
            "entries"
        } else {
            "entry"
        };
        let uri = format!(
            "/v1/collections/default/kv/{suffix}{}",
            case["query"]
                .as_str()
                .map(|s| format!("?{s}"))
                .unwrap_or_default()
        );
        let (status, mut rest_result) = rest(
            &a.app,
            ADMIN,
            case["method"].as_str().unwrap(),
            &uri,
            case["body"].clone(),
        )
        .await;
        match golden_call(&mut b.client, case).await {
            Ok(mut grpc_result) => {
                assert_eq!(status, 200, "{rest_result}");
                fn normalize(value: &mut Value, versions: &mut Vec<String>) {
                    if let Some(version) = value.get_mut("version") {
                        let version = version.as_str().unwrap().to_owned();
                        assert!(version.parse::<u64>().unwrap() > 0);
                        let index =
                            versions
                                .iter()
                                .position(|v| v == &version)
                                .unwrap_or_else(|| {
                                    versions.push(version);
                                    versions.len() - 1
                                });
                        value["version"] = json!(format!("v{index}"));
                    }
                    if let Some(items) = value.get_mut("items").and_then(Value::as_array_mut) {
                        for item in items {
                            normalize(item, versions);
                        }
                    }
                }
                normalize(&mut rest_result, &mut rest_versions);
                normalize(&mut grpc_result, &mut grpc_versions);
                assert_eq!(rest_result, grpc_result, "{case}");
            }
            Err(error) => {
                let code = match status {
                    404 => Code::NotFound,
                    412 => Code::Aborted,
                    _ => panic!("{status}: {rest_result}"),
                };
                assert_error(error, code, rest_result["error"]["code"].as_str().unwrap());
            }
        }
    }
}

#[test]
fn grpc_kv_wire_tags_and_manifest() {
    let source = include_str!("../proto/vlite/v1/vlite.proto");
    let server = include_str!("../src/server/grpc.rs");
    let registration = server
        .split("let kv = proto::kv_server::KvServer::new(adapter.clone())")
        .nth(1)
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    assert!(registration.contains(".max_decoding_message_size(MAX_DECODING_MESSAGE_BYTES)"));
    assert!(registration.contains(".max_encoding_message_size(MAX_ENCODING_MESSAGE_BYTES)"));
    assert_eq!(
        registration.matches(".max_encoding_message_size(").count(),
        1
    );
    let kv = source
        .split("service Kv {")
        .nth(1)
        .unwrap()
        .split('}')
        .next()
        .unwrap();
    assert_eq!(kv.matches("rpc ").count(), 4);
    for rpc in ["GetKvEntry", "PutKvEntry", "DeleteKvEntry", "ListKvEntries"] {
        assert_eq!(source.matches(&format!("rpc {rpc}(")).count(), 1);
    }
    let names: Vec<_> = source
        .lines()
        .filter_map(|line| line.trim().strip_prefix("rpc "))
        .map(|line| line.split('(').next().unwrap())
        .collect();
    assert_eq!(
        names.len(),
        names.iter().collect::<std::collections::HashSet<_>>().len()
    );
    for name in [
        "KvAddress",
        "KvGetRequest",
        "KvEntry",
        "KvPutRequest",
        "KvPutResponse",
        "KvDeleteRequest",
        "KvDeleteResponse",
        "KvListRequest",
        "KvListResponse",
    ] {
        let body = source
            .split(&format!("message {name} {{"))
            .nth(1)
            .unwrap()
            .split('}')
            .next()
            .unwrap();
        assert!(body.contains("reserved 100 to 199;"), "{name}");
    }
    let address = proto::KvAddress {
        collection: "c".into(),
        namespace: "n".into(),
        key: "k".into(),
    };
    assert_eq!(
        address.encode_to_vec(),
        [10, 1, b'c', 18, 1, b'n', 26, 1, b'k']
    );
    let nested = vec![10, 9, 10, 1, b'c', 18, 1, b'n', 26, 1, b'k'];
    assert_eq!(
        proto::KvGetRequest {
            address: Some(address.clone())
        }
        .encode_to_vec(),
        nested
    );
    let mut put_bytes = nested.clone();
    put_bytes.extend([18, 1, b'v', 26, 2, 24, 7, 32, 0]);
    assert_eq!(
        proto::KvPutRequest {
            address: Some(address.clone()),
            value: b"v".to_vec(),
            expected: expected(proto::expected::Kind::Revision(7)),
            expires_at_ms: Some(0)
        }
        .encode_to_vec(),
        put_bytes
    );
    let mut delete_bytes = nested;
    delete_bytes.extend([18, 2, 24, 7]);
    assert_eq!(
        proto::KvDeleteRequest {
            address: Some(address),
            expected: expected(proto::expected::Kind::Revision(7))
        }
        .encode_to_vec(),
        delete_bytes
    );
    assert_eq!(
        proto::KvEntry {
            key: "k".into(),
            value: b"v".to_vec(),
            version: 7,
            expires_at_ms: Some(0)
        }
        .encode_to_vec(),
        [10, 1, b'k', 18, 1, b'v', 24, 7, 32, 0]
    );
    assert_eq!(
        proto::KvPutResponse {
            version: 7,
            expires_at_ms: Some(0)
        }
        .encode_to_vec(),
        [8, 7, 16, 0]
    );
    for version in [0, 18_446_744_073_709_551_615] {
        let response = proto::KvEntry {
            version,
            expires_at_ms: Some(version),
            ..Default::default()
        };
        assert_eq!(
            proto::KvEntry::decode(response.encode_to_vec().as_slice()).unwrap(),
            response
        );
    }
    assert_eq!(
        proto::KvDeleteResponse { deleted: true }.encode_to_vec(),
        [8, 1]
    );
    assert_eq!(
        proto::KvListRequest {
            collection: "c".into(),
            namespace: "n".into(),
            prefix: "p".into(),
            after: Some("a".into()),
            limit: Some(1)
        }
        .encode_to_vec(),
        [10, 1, b'c', 18, 1, b'n', 26, 1, b'p', 34, 1, b'a', 40, 1]
    );
    assert_eq!(
        proto::KvListResponse {
            items: vec![proto::KvEntry {
                key: "k".into(),
                ..Default::default()
            }],
            next_after: Some("k".into())
        }
        .encode_to_vec(),
        [10, 3, 10, 1, b'k', 18, 1, b'k']
    );
}
