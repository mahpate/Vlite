#![cfg(feature = "server")]

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

const ADMIN: &str = "fake_kv_admin_token_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const READ: &str = "fake_kv_reader_token_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";

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

async fn raw(
    app: &Router,
    token: &str,
    method: &str,
    uri: &str,
    body: String,
) -> (u16, Value, usize) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value, bytes.len())
}

async fn call(app: &Router, token: &str, method: &str, uri: &str, body: Value) -> (u16, Value) {
    let (status, value, _) = raw(app, token, method, uri, body.to_string()).await;
    (status, value)
}

fn put(key: &str) -> Value {
    json!({"namespace":"n","key":key,"value_base64":"YQ=="})
}

fn entry(key: &str) -> String {
    let query = reqwest::Url::parse_with_params(
        "http://localhost/v1/collections/default/kv/entry",
        &[("namespace", "n"), ("key", key)],
    )
    .unwrap();
    format!("{}?{}", query.path(), query.query().unwrap())
}

fn canonical_version(value: &Value) {
    let text = value.as_str().unwrap();
    let parsed = text.parse::<u64>().unwrap();
    assert!(parsed > 0);
    assert_eq!(parsed.to_string(), text);
}

fn resolve(expected: &mut Value, actual: &Value, seed: u64) {
    match expected {
        Value::String(value) if value == "$seed" => *value = seed.to_string(),
        Value::String(value) if value == "$issued" => {
            canonical_version(actual);
            *value = actual.as_str().unwrap().into();
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
async fn kv_contract_wire_goldens() {
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/kv/wire/ordinary.json")).unwrap();
    for case in cases {
        let directory = tempfile::tempdir().unwrap();
        let mut db = database(FormatVersion::V5);
        let seed = db
            .kv_store("default")
            .unwrap()
            .put("n", "seed", &[0, 1, 255], KvPut::default())
            .unwrap();
        let app = start(db, directory.path());
        let collection = case["collection"].as_str().unwrap_or("default");
        let suffix = if case["list"] == true {
            "entries"
        } else {
            "entry"
        };
        let path = format!(
            "/v1/collections/{collection}/kv/{suffix}{}{}",
            case["suffix"].as_str().unwrap_or(""),
            case["query"]
                .as_str()
                .map(|q| format!("?{q}"))
                .unwrap_or_default()
        );
        let method = case["method"].as_str().unwrap();
        let body = case["raw"].as_str().map(str::to_owned).unwrap_or_else(|| {
            if method == "PUT" {
                case["body"].to_string()
            } else {
                String::new()
            }
        });
        let (status, result, _) = raw(&app, ADMIN, method, &path, body).await;
        assert_eq!(
            status,
            case["status"].as_u64().unwrap() as u16,
            "{case}: {result}"
        );
        if let Some(code) = case["code"].as_str() {
            assert_eq!(result["error"]["code"], code, "{case}");
        } else {
            let mut expected = case.get("response").unwrap_or(&case["body"]).clone();
            resolve(&mut expected, &result, seed);
            assert_eq!(result, expected, "{case}");
        }
    }
}

#[tokio::test]
async fn kv_put_expected_and_query_encoding_are_strict() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = database(FormatVersion::V5);
    {
        let mut store = db.kv_store("default").unwrap();
        store
            .put("n", "\u{fffd}", b"replacement", KvPut::default())
            .unwrap();
        store.put("n", "a b", b"space", KvPut::default()).unwrap();
    }
    let app = start(db, directory.path());
    for (expected, code) in [
        (json!("5"), "invalid_input"),
        (json!(""), "invalid_input"),
        (Value::Null, "invalid_json"),
        (json!(5), "invalid_json"),
        (json!(false), "invalid_json"),
        (json!([]), "invalid_json"),
        (json!({}), "invalid_json"),
        (json!({"revision":"1","extra":2}), "invalid_json"),
    ] {
        let mut input = put("expected");
        input["expected"] = expected;
        let (status, body) = call(
            &app,
            ADMIN,
            "PUT",
            "/v1/collections/default/kv/entry",
            input,
        )
        .await;
        assert_eq!(status, 422, "{body}");
        assert_eq!(body["error"]["code"], code, "{body}");
    }
    let (status, body) = call(
        &app,
        ADMIN,
        "PUT",
        "/v1/collections/default/kv/entry",
        json!({"namespace":"n","key":"missing-value"}),
    )
    .await;
    assert_eq!(status, 422);
    assert_eq!(body["error"]["code"], "invalid_json");

    for uri in [
        "/v1/collections/default/kv/entry?namespace=n&key=expected&unexpected=x",
        "/v1/collections/default/kv/entries?namespace=n&unexpected=x",
        "/v1/collections/default/kv/entries?namespace=n&limit=+5",
        "/v1/collections/default/kv/entries?namespace=n&limit=05",
    ] {
        let (status, body) = call(&app, ADMIN, "GET", uri, Value::Null).await;
        assert_eq!(
            status,
            if uri.contains("limit=") { 422 } else { 400 },
            "{uri}: {body}"
        );
        assert_eq!(
            body["error"]["code"],
            if uri.contains("limit=") {
                "invalid_input"
            } else {
                "invalid_query"
            }
        );
    }
    let (status, body) = call(
        &app,
        ADMIN,
        "DELETE",
        "/v1/collections/default/kv/entry?namespace=n&key=missing&unexpected=x",
        Value::Null,
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["code"], "invalid_query");

    let (status, body) = call(
        &app,
        ADMIN,
        "GET",
        "/v1/collections/default/kv/entry?namespace=n&key=%FF",
        Value::Null,
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["code"], "invalid_query");
    let (status, body) = call(
        &app,
        ADMIN,
        "GET",
        "/v1/collections/default/kv/entry?namespace=n&key=a+b",
        Value::Null,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["key"], "a b");
    assert_eq!(body["value_base64"], BASE64_STANDARD.encode(b"space"));
}

#[tokio::test]
async fn kv_entries_rejects_mutation_methods() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(FormatVersion::V5), directory.path());
    for method in ["PUT", "DELETE", "POST"] {
        let (status, response) = call(
            &app,
            ADMIN,
            method,
            "/v1/collections/default/kv/entries?namespace=n",
            json!({"namespace":"n","key":"k","value_base64":"YQ=="}),
        )
        .await;
        assert_eq!(status, 405, "{method}: {response}");
    }
}

#[tokio::test]
async fn kv_capabilities_advertise_supported_routes_and_format() {
    let directory = tempfile::tempdir().unwrap();
    for (format, expected) in [(FormatVersion::V5, true), (FormatVersion::V3, false)] {
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
        let app = start(db, directory.path());
        let (status, caps) = call(&app, ADMIN, "GET", "/v1/capabilities", Value::Null).await;
        assert_eq!(status, 200);
        assert_eq!(caps["features"]["kv"], expected);
        let routes = caps["routes"].as_array().unwrap();
        for route in [
            "/v1/collections/{collection}/kv/entry",
            "/v1/collections/{collection}/kv/entries",
        ] {
            assert_eq!(
                routes
                    .iter()
                    .filter(|item| item.as_str() == Some(route))
                    .count(),
                1
            );
        }
    }
}

#[tokio::test]
async fn kv_deadline_round_trips_and_due_entries_are_hidden_from_get_and_list() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(FormatVersion::V5), directory.path());
    let deadline = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 60_000;
    let mut input = put("live");
    input["expires_at_ms"] = json!(deadline.to_string());
    let (status, inserted) = call(
        &app,
        ADMIN,
        "PUT",
        "/v1/collections/default/kv/entry",
        input,
    )
    .await;
    assert_eq!(status, 200, "{inserted}");
    assert_eq!(inserted["expires_at_ms"], deadline.to_string());
    let (status, current) = call(&app, ADMIN, "GET", &entry("live"), Value::Null).await;
    assert_eq!(status, 200, "{current}");
    assert_eq!(current["expires_at_ms"], deadline.to_string());

    let mut expired = put("due");
    expired["expires_at_ms"] = json!("0");
    let (status, inserted) = call(
        &app,
        ADMIN,
        "PUT",
        "/v1/collections/default/kv/entry",
        expired,
    )
    .await;
    assert_eq!(status, 200, "{inserted}");
    assert_eq!(inserted["expires_at_ms"], "0");
    let (status, missing) = call(&app, ADMIN, "GET", &entry("due"), Value::Null).await;
    assert_eq!(status, 404, "{missing}");
    assert_eq!(missing["error"]["code"], "kv_not_found");
    let (status, page) = call(
        &app,
        ADMIN,
        "GET",
        "/v1/collections/default/kv/entries?namespace=n",
        Value::Null,
    )
    .await;
    assert_eq!(status, 200, "{page}");
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["items"][0]["key"], "live");
    assert_eq!(page["next_after"], Value::Null);
}

#[tokio::test]
async fn kv_large_list_limits_preserve_order_and_continuation() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = database(FormatVersion::V5);
    let mut store = db.kv_store("default").unwrap();
    for i in 0..2_000 {
        store
            .put("bulk", &format!("{i:05}"), b"x", KvPut::default())
            .unwrap();
    }
    let app = start(db, directory.path());
    for (limit, expected_len, next) in [(1_000, 1_000, Some("00999")), (10_000, 2_000, None)] {
        let uri = format!("/v1/collections/default/kv/entries?namespace=bulk&limit={limit}");
        let (status, page) = call(&app, ADMIN, "GET", &uri, Value::Null).await;
        assert_eq!(status, 200);
        assert_eq!(page["items"].as_array().unwrap().len(), expected_len);
        assert_eq!(page["items"][0]["key"], "00000");
        assert_eq!(
            page["items"][expected_len - 1]["key"],
            next.unwrap_or("01999")
        );
        assert_eq!(
            page["next_after"],
            next.map(Value::from).unwrap_or(Value::Null)
        );
    }
}

#[tokio::test]
async fn kv_put_audit_records_safe_identity_and_expiry() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(FormatVersion::V5), directory.path());
    let mut input = put("audit");
    input["expires_at_ms"] = json!("18446744073709551615");
    let (status, body) = call(
        &app,
        ADMIN,
        "PUT",
        "/v1/collections/default/kv/entry",
        input,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let audit = std::fs::read_to_string(directory.path().join("audit.jsonl")).unwrap();
    let event = audit
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|event| event["event"] == "kv_put")
        .unwrap();
    assert_eq!(event["outcome"], "ok");
    assert_eq!(event["principal"], "admin");
    assert_eq!(event["collection"], "default");
    assert_eq!(event["expires_at_ms"], "18446744073709551615");

    let (status, response) = call(
        &app,
        ADMIN,
        "PUT",
        "/v1/collections/missing/kv/entry",
        put("rejected"),
    )
    .await;
    assert_eq!(status, 404, "{response}");
    let audit = std::fs::read_to_string(directory.path().join("audit.jsonl")).unwrap();
    let rejected = audit
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|event| event["event"] == "kv_put" && event["outcome"] == "collection_not_found")
        .unwrap();
    assert_eq!(rejected["collection"], "(invalid)");
}

#[tokio::test]
async fn kv_utf8_address_and_value_bounds() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(FormatVersion::V5), directory.path());
    for field in ["namespace", "key"] {
        let maximum = if field == "namespace" { 255 } else { 1024 };
        for size in [0, 1, maximum, maximum + 1] {
            for unicode in [false, true] {
                let mut body = put("k");
                body[field] = json!(if unicode {
                    format!("{}{}", "\u{e9}".repeat(size / 2), "x".repeat(size % 2))
                } else {
                    "x".repeat(size)
                });
                let (status, response) =
                    call(&app, ADMIN, "PUT", "/v1/collections/default/kv/entry", body).await;
                let valid = size > 0 && size <= maximum;
                assert_eq!(
                    status,
                    if valid { 200 } else { 422 },
                    "{field} {size}: {response}"
                );
                if !valid {
                    assert_eq!(response["error"]["code"], "invalid_input");
                }
            }
        }
    }
    for size in [0, 1_047_290, 1_047_291] {
        let mut body = put("value");
        body["value_base64"] = json!(BASE64_STANDARD.encode(vec![255; size]));
        let (status, response) =
            call(&app, ADMIN, "PUT", "/v1/collections/default/kv/entry", body).await;
        assert_eq!(
            status,
            if size <= 1_047_290 { 200 } else { 422 },
            "{response}"
        );
        if status == 422 {
            assert_eq!(response["error"]["code"], "invalid_input");
        }
    }
    for field in ["prefix", "after"] {
        for size in [1024, 1025] {
            let uri = format!(
                "/v1/collections/default/kv/entries?namespace=n&{field}={}",
                "x".repeat(size)
            );
            let (status, response) = call(&app, ADMIN, "GET", &uri, Value::Null).await;
            assert_eq!(status, if size == 1024 { 200 } else { 422 }, "{response}");
        }
        for size in [512, 513] {
            let value = "é".repeat(size);
            let url = reqwest::Url::parse_with_params(
                "http://localhost/v1/collections/default/kv/entries",
                &[("namespace", "n"), (field, value.as_str())],
            )
            .unwrap();
            let uri = format!("{}?{}", url.path(), url.query().unwrap());
            let (status, response) = call(&app, ADMIN, "GET", &uri, Value::Null).await;
            assert_eq!(status, if size == 512 { 200 } else { 422 }, "{response}");
            if size == 513 {
                assert_eq!(response["error"]["code"], "invalid_input");
            }
        }
    }
    let (status, response, _) = raw(
        &app,
        ADMIN,
        "PUT",
        "/v1/collections/default/kv/entry",
        " ".repeat(8_388_609),
    )
    .await;
    assert_eq!(status, 413);
    assert_eq!(response["error"]["code"], "invalid_json");
}

#[tokio::test]
async fn kv_cas_expiry_clear_delete_and_races() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(FormatVersion::V5), directory.path());
    let mut input = put("k");
    input["expires_at_ms"] = json!("18446744073709551615");
    let (status, created) = call(&app, ADMIN, "PUT", &entry("k"), input.clone()).await;
    // PUT has no query parameters.
    assert_eq!(status, 400, "{created}");
    let path = "/v1/collections/default/kv/entry";
    let (_, created) = call(&app, ADMIN, "PUT", path, input.clone()).await;
    let version = created["version"].clone();
    input["expected"] = json!({"revision":version});
    input["expires_at_ms"] = Value::Null;
    let (a, b) = tokio::join!(
        call(&app, ADMIN, "PUT", path, input.clone()),
        call(&app, ADMIN, "PUT", path, input.clone())
    );
    assert_eq!([a.0.min(b.0), a.0.max(b.0)], [200, 412]);
    let (_, current) = call(&app, ADMIN, "GET", &entry("k"), Value::Null).await;
    assert_eq!(current["expires_at_ms"], Value::Null);
    assert_ne!(current["version"], version);
    let uri = format!(
        "{}&expected={}",
        entry("k"),
        current["version"].as_str().unwrap()
    );
    assert_eq!(
        call(&app, ADMIN, "DELETE", &uri, Value::Null).await,
        (200, json!({"deleted":true}))
    );
    assert_eq!(call(&app, ADMIN, "DELETE", &uri, Value::Null).await.0, 412);
    input["expected"] = json!("absent");
    input["expires_at_ms"] = json!("0");
    assert_eq!(call(&app, ADMIN, "PUT", path, input.clone()).await.0, 200);
    let (status, response) = call(&app, ADMIN, "GET", &entry("k"), Value::Null).await;
    assert_eq!(status, 404);
    assert_eq!(response["error"]["code"], "kv_not_found");
    input["expires_at_ms"] = Value::Null;
    let (a, b) = tokio::join!(
        call(&app, ADMIN, "PUT", path, input.clone()),
        call(&app, ADMIN, "PUT", path, input.clone())
    );
    assert_eq!([a.0.min(b.0), a.0.max(b.0)], [200, 412]);
}

#[tokio::test]
async fn kv_list_order_defaults_and_exact_size_boundary() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = database(FormatVersion::V5);
    let mut store = db.kv_store("default").unwrap();
    for i in 0..101 {
        store
            .put("n", &format!("{i:03}"), b"", KvPut::default())
            .unwrap();
    }
    for key in ["a/b", "a+", "a ", "a\u{e9}", "a\u{1f600}"] {
        store.put("ordered", key, b"", KvPut::default()).unwrap();
    }
    let value = vec![255; 1_047_290];
    for i in 0..13 {
        store
            .put("big", &format!("{i:03}"), &value, KvPut::default())
            .unwrap();
    }
    let app = start(db, directory.path());
    let (_, page) = call(
        &app,
        ADMIN,
        "GET",
        "/v1/collections/default/kv/entries?namespace=n",
        Value::Null,
    )
    .await;
    assert_eq!(page["items"].as_array().unwrap().len(), 100);
    assert_eq!(page["next_after"], "099");
    let (_, page) = call(
        &app,
        ADMIN,
        "GET",
        "/v1/collections/default/kv/entries?namespace=ordered&prefix=a&after=a%20&limit=2",
        Value::Null,
    )
    .await;
    assert_eq!(page["items"][0]["key"], "a+");
    assert_eq!(page["items"][1]["key"], "a/b");
    assert_eq!(page["next_after"], "a/b");
    let (_, page) = call(
        &app,
        ADMIN,
        "GET",
        "/v1/collections/default/kv/entries?namespace=ordered&after=a%2Fb",
        Value::Null,
    )
    .await;
    assert_eq!(page["items"][0]["key"], "a\u{e9}");
    assert_eq!(page["items"][1]["key"], "a\u{1f600}");
    assert_eq!(page["next_after"], Value::Null);
    let (status, page, bytes) = raw(
        &app,
        ADMIN,
        "GET",
        "/v1/collections/default/kv/entries?namespace=big&limit=13",
        String::new(),
    )
    .await;
    assert_eq!(status, 200);
    assert!(bytes <= 16_777_216);
    assert_eq!(page["items"].as_array().unwrap().len(), 12);
    assert_eq!(page["next_after"], "011");
    let (_, tail) = call(
        &app,
        ADMIN,
        "GET",
        "/v1/collections/default/kv/entries?namespace=big&after=011",
        Value::Null,
    )
    .await;
    assert_eq!(tail["items"].as_array().unwrap().len(), 1);
    assert_eq!(tail["next_after"], Value::Null);
}

#[tokio::test]
async fn kv_page_canonical_byte_boundary_is_inclusive() {
    for boundary in [16_777_215, 16_777_216, 16_777_217] {
        let directory = tempfile::tempdir().unwrap();
        let mut db = database(FormatVersion::V5);
        let mut store = db.kv_store("default").unwrap();
        let mut expected = Vec::new();
        let bytes = vec![255; 1_047_290];
        for i in 0..12 {
            let key = if i == 0 {
                "00\"".to_owned()
            } else {
                format!("{i:02}")
            };
            let version = store.put("edge", &key, &bytes, KvPut::default()).unwrap();
            expected.push(
                json!({"key":key,"value_base64":BASE64_STANDARD.encode(&bytes),
                "version":version.to_string(),"expires_at_ms":null}),
            );
        }
        let version = store.put("edge", "m", b"", KvPut::default()).unwrap();
        store.put("edge", "z", b"", KvPut::default()).unwrap();
        // Build the oracle from the specified JSON envelope, not a served page.
        // Only opaque engine versions are read from the fixture.
        let empty =
            json!({"key":"m","value_base64":"","version":version.to_string(),"expires_at_ms":null});
        let mut oracle = expected.clone();
        oracle.push(empty);
        let mut overhead = serde_json::to_vec(&json!({"items":oracle,"next_after":"m"}))
            .unwrap()
            .len();
        for _ in 0..3 {
            if (boundary - overhead).is_multiple_of(2) {
                break;
            }
            let old = expected[0]["key"].as_str().unwrap();
            let key = format!("{old}x");
            store.delete("edge", old, vlite::Expected::Any).unwrap();
            let version = store.put("edge", &key, &bytes, KvPut::default()).unwrap();
            expected[0]["key"] = json!(key);
            expected[0]["version"] = json!(version.to_string());
            oracle[0] = expected[0].clone();
            overhead = serde_json::to_vec(&json!({"items":oracle,"next_after":"m"}))
                .unwrap()
                .len();
        }
        let remaining = boundary - overhead;
        // A last-key byte appears twice; base64 grows in four-byte units.
        let key = if remaining % 4 == 2 { "mx" } else { "m" };
        let encoded_size = remaining - if key == "mx" { 2 } else { 0 };
        let value = vec![1; encoded_size / 4 * 3];
        store.delete("edge", "m", vlite::Expected::Any).unwrap();
        let version = store.put("edge", key, &value, KvPut::default()).unwrap();
        expected.push(
            json!({"key":key,"value_base64":BASE64_STANDARD.encode(value),
            "version":version.to_string(),"expires_at_ms":null}),
        );
        let oracle = json!({"items":expected,"next_after":key});
        let oracle_bytes = serde_json::to_vec(&oracle).unwrap().len();
        assert_eq!(
            oracle_bytes, boundary,
            "independent fixture must hit exact boundary"
        );
        let app = start(db, directory.path());
        let (status, page, size) = raw(
            &app,
            ADMIN,
            "GET",
            "/v1/collections/default/kv/entries?namespace=edge&limit=13",
            String::new(),
        )
        .await;
        assert_eq!(status, 200);
        if boundary <= 16_777_216 {
            assert_eq!(size, boundary);
            assert_eq!(page, oracle);
        } else {
            assert!(size <= 16_777_216);
            assert_eq!(page["items"].as_array().unwrap().len(), 12);
            assert_eq!(page["next_after"], "11");
        }
    }
}

async fn credential(app: &Router, extra: Value) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let mut input = json!({"name":"kv-test","collections":["default"],"operations":["read"],"expires_unix_ms":now + 600_000});
    for (key, value) in extra.as_object().unwrap() {
        input[key] = value.clone();
    }
    let (status, body) = call(app, ADMIN, "POST", "/v1/admin/credentials", input).await;
    assert_eq!(status, 201, "{body}");
    body["token"].as_str().unwrap().into()
}

#[tokio::test]
async fn kv_scopes_binding_tenancy_quota_and_audit() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(FormatVersion::V5), directory.path());
    let reader = credential(&app, json!({})).await;
    let writer = credential(&app, json!({"operations":["write"],"max_vectors":1})).await;
    let bound_reader_writer = credential(&app, json!({"operations":["read","write"]})).await;
    let graph = credential(&app, json!({"operations":["graph_read"],"graphs":["*"]})).await;
    for token in [&reader, READ, &graph] {
        let (status, response) = call(
            &app,
            token,
            "PUT",
            "/v1/collections/default/kv/entry",
            put("k"),
        )
        .await;
        assert_eq!(status, 403, "{response}");
    }
    assert_eq!(
        call(
            &app,
            &writer,
            "PUT",
            "/v1/collections/default/kv/entry",
            put("k")
        )
        .await
        .0,
        200
    );
    assert_eq!(
        call(
            &app,
            &writer,
            "PUT",
            "/v1/collections/default/kv/entry",
            put("k")
        )
        .await
        .0,
        200
    );
    let (status, response) = call(
        &app,
        &writer,
        "PUT",
        "/v1/collections/default/kv/entry",
        put("second"),
    )
    .await;
    assert_eq!(status, 429);
    assert_eq!(response["error"]["code"], "quota_exceeded");
    for token in [&reader, READ] {
        assert_eq!(
            call(&app, token, "GET", &entry("k"), Value::Null).await.0,
            200
        );
        assert_eq!(
            call(
                &app,
                token,
                "GET",
                "/v1/collections/default/kv/entries?namespace=n",
                Value::Null
            )
            .await
            .0,
            200
        );
        assert_eq!(
            call(&app, token, "HEAD", &entry("k"), Value::Null).await.0,
            200
        );
        assert_eq!(
            call(&app, token, "DELETE", &entry("k"), Value::Null)
                .await
                .0,
            403
        );
    }
    for collection in ["wide", "missing"] {
        let uri = format!("/v1/collections/{collection}/kv/entry?namespace=n&key=secret");
        let (status, response) = call(&app, &reader, "GET", &uri, Value::Null).await;
        assert_eq!(status, 403);
        assert_eq!(response["error"]["code"], "forbidden_scope");
    }
    for (method, uri, body) in [
        ("PUT", "/v1/collections/wide/kv/entry", put("bound-denied")),
        (
            "DELETE",
            "/v1/collections/wide/kv/entry?namespace=n&key=secret",
            Value::Null,
        ),
        (
            "GET",
            "/v1/collections/wide/kv/entries?namespace=n",
            Value::Null,
        ),
    ] {
        let (status, response) = call(&app, &bound_reader_writer, method, uri, body).await;
        assert_eq!(status, 403, "{method}: {response}");
        assert_eq!(response["error"]["code"], "forbidden_scope");
    }
    assert_eq!(
        call(&app, &writer, "GET", &entry("k"), Value::Null).await.0,
        403
    );
    assert_eq!(
        call(&app, &writer, "DELETE", &entry("k"), Value::Null)
            .await
            .0,
        200
    );
    let mut tenant_tokens = Vec::new();
    for name in ["one", "two"] {
        let (_, tenant) = call(
            &app,
            ADMIN,
            "POST",
            "/v1/admin/tenants",
            json!({"name":name}),
        )
        .await;
        let token = credential(&app, json!({"tenant_id":tenant["tenant"]["id"],"collections":["*"],"operations":["collections","read","write"]})).await;
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
        let uri = "/v1/collections/items/kv/entry?namespace=n&key=k";
        let (status, missing) = call(&app, &token, "GET", uri, Value::Null).await;
        assert_eq!(status, 404, "{missing}");
        assert_eq!(
            call(
                &app,
                &token,
                "PUT",
                "/v1/collections/items/kv/entry",
                put("k")
            )
            .await
            .0,
            200
        );
        let (status, read) = call(&app, &token, "GET", uri, Value::Null).await;
        assert_eq!(status, 200, "{read}");
        assert_eq!(read["key"], "k");
        assert_eq!(read["value_base64"], "YQ==");
        let (status, page) = call(
            &app,
            &token,
            "GET",
            "/v1/collections/items/kv/entries?namespace=n",
            Value::Null,
        )
        .await;
        assert_eq!(status, 200, "{page}");
        assert_eq!(page["items"].as_array().unwrap().len(), 1);
        assert_eq!(page["items"][0]["key"], "k");
        tenant_tokens.push(token);
    }
    let tenant_uri = "/v1/collections/items/kv/entry?namespace=n&key=k";
    for token in &tenant_tokens {
        let (status, read) = call(&app, token, "GET", tenant_uri, Value::Null).await;
        assert_eq!(status, 200, "{read}");
        assert_eq!(read["value_base64"], "YQ==");
    }
    let (status, deleted) = call(&app, &tenant_tokens[0], "DELETE", tenant_uri, Value::Null).await;
    assert_eq!(status, 200, "{deleted}");
    assert_eq!(deleted, json!({"deleted":true}));
    assert_eq!(
        call(&app, &tenant_tokens[0], "GET", tenant_uri, Value::Null)
            .await
            .0,
        404
    );
    assert_eq!(
        call(&app, &tenant_tokens[1], "GET", tenant_uri, Value::Null)
            .await
            .0,
        200
    );
    let (status, deleted) = call(&app, &tenant_tokens[1], "DELETE", tenant_uri, Value::Null).await;
    assert_eq!(status, 200, "{deleted}");
    assert_eq!(deleted, json!({"deleted":true}));
    let audit = std::fs::read_to_string(directory.path().join("audit.jsonl")).unwrap();
    let mutations: Vec<Value> = audit
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| matches!(event["event"].as_str(), Some("kv_put" | "kv_delete")))
        .collect();
    assert!(
        mutations
            .iter()
            .any(|event| event["event"] == "kv_put" && event["outcome"] == "ok")
    );
    assert!(
        mutations
            .iter()
            .any(|event| event["event"] == "kv_delete" && event["outcome"] == "ok")
    );
    for event in mutations {
        assert!(event.get("request_id").is_some());
        assert!(event.get("principal").is_some());
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
async fn kv_read_only_handle_refuses_even_missing_delete() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("read-only.vlite");
    Database::create_with_format(&path, 1, FormatVersion::V5)
        .unwrap()
        .close()
        .unwrap();
    let app = start(
        Database::open_read_only(&path).unwrap(),
        &directory.path().join("admin"),
    );
    for (method, uri, body) in [
        (
            "PUT",
            "/v1/collections/default/kv/entry".to_owned(),
            put("k"),
        ),
        ("DELETE", entry("missing"), Value::Null),
    ] {
        let (status, response) = call(&app, ADMIN, method, &uri, body).await;
        assert_eq!(status, 403);
        assert_eq!(response["error"]["code"], "read_only");
    }
}

#[tokio::test]
async fn kv_configured_file_durability_survives_orderly_restart() {
    for durability in [
        vlite::Durability::Full,
        vlite::Durability::periodic_ms(60_000).unwrap(),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("kv.vlite");
        let mut db = Database::create_with_format(&path, 1, FormatVersion::V5).unwrap();
        db.set_durability(durability).unwrap();
        let service = Service::new(db);
        let app = configured_admin_router(
            service.clone(),
            ServerOptions {
                admin_token: Some(BearerToken::new(ADMIN).unwrap()),
                ..Default::default()
            },
        )
        .unwrap();
        let (status, response) = call(
            &app,
            ADMIN,
            "PUT",
            "/v1/collections/default/kv/entry",
            put("persisted"),
        )
        .await;
        assert_eq!(status, 200, "{response}");
        canonical_version(&response["version"]);
        let version = response["version"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap();
        service.close().await.unwrap();
        let db = Database::open(&path).unwrap();
        let entry = db
            .kv_view("default")
            .unwrap()
            .get("n", "persisted")
            .unwrap()
            .unwrap();
        assert_eq!(entry.version, version);
        assert_eq!(entry.value, b"a");
    }
}
#[tokio::test]
async fn kv_slots_exhaustion_and_upgrade_errors() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = database(FormatVersion::V5);
    let base = first_slot("n", "occupied");
    for i in 0..16 {
        db.put(base.wrapping_add(i), &[0.0], b"ordinary occupied record")
            .unwrap();
    }
    let app = start(db, directory.path());
    let (status, body) = call(
        &app,
        ADMIN,
        "PUT",
        "/v1/collections/default/kv/entry",
        put("occupied"),
    )
    .await;
    assert_eq!(status, 409);
    assert_eq!(body["error"]["code"], "kv_slots_exhausted");
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(FormatVersion::V4), directory.path());
    let mut input = put("k");
    input["expires_at_ms"] = json!("1");
    let (status, body) = call(
        &app,
        ADMIN,
        "PUT",
        "/v1/collections/default/kv/entry",
        input,
    )
    .await;
    assert_eq!(status, 409);
    assert_eq!(body["error"]["code"], "upgrade_required");
}

fn precondition_body(message: &str) -> Value {
    json!({"error":{"code":"precondition_failed","message":message}})
}

// A caller with `write` but not `read` must not learn from a failed
// compare-and-set whether a key exists, or its revision.
#[tokio::test]
async fn kv_precondition_failures_reveal_neither_existence_nor_revision() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(FormatVersion::V5), directory.path());
    let writer = credential(&app, json!({"operations":["write"]})).await;
    let readwrite = credential(&app, json!({"operations":["read","write"]})).await;
    let path = "/v1/collections/default/kv/entry";
    let (status, first) = call(&app, ADMIN, "PUT", path, put("exists")).await;
    assert_eq!(status, 200, "{first}");
    let (status, current) = call(&app, ADMIN, "PUT", path, put("exists")).await;
    assert_eq!(status, 200, "{current}");
    let stale = first["version"].as_str().unwrap().to_owned();
    assert_ne!(stale, current["version"].as_str().unwrap());
    // The old revision of a deleted key is stale for a missing key.
    let (_, gone) = call(&app, ADMIN, "PUT", path, put("gone")).await;
    let deleted = gone["version"].as_str().unwrap().to_owned();
    assert_eq!(
        call(&app, ADMIN, "DELETE", &entry("gone"), Value::Null).await,
        (200, json!({"deleted":true}))
    );
    // The premise: one credential cannot read, the other can. The message is
    // fixed for every caller (docs/API.md), so both must see the same text.
    assert_eq!(
        call(&app, &writer, "GET", &entry("exists"), Value::Null)
            .await
            .0,
        403
    );
    assert_eq!(
        call(&app, &readwrite, "GET", &entry("exists"), Value::Null)
            .await
            .0,
        200
    );

    let cases = [
        ("exists", vec!["0", stale.as_str(), "absent"]),
        ("never-written", vec!["0", "1"]),
        ("gone", vec!["0", deleted.as_str()]),
    ];
    for token in [ADMIN, writer.as_str(), readwrite.as_str()] {
        for (key, expectations) in &cases {
            for text in expectations {
                let mut input = put(key);
                input["expected"] = if *text == "absent" {
                    json!("absent")
                } else {
                    json!({"revision":text})
                };
                let (status, body) = call(&app, token, "PUT", path, input).await;
                assert_eq!(status, 412, "PUT {key} expected={text}: {body}");
                assert_eq!(
                    body,
                    precondition_body("revision precondition failed"),
                    "PUT {key} expected={text}"
                );
                let uri = format!("{}&expected={text}", entry(key));
                let (status, body) = call(&app, token, "DELETE", &uri, Value::Null).await;
                assert_eq!(status, 412, "DELETE {key} expected={text}: {body}");
                assert_eq!(
                    body,
                    precondition_body("revision precondition failed"),
                    "DELETE {key} expected={text}"
                );
            }
        }
    }
    // The failed requests wrote and removed nothing.
    let (status, kept) = call(&app, ADMIN, "GET", &entry("exists"), Value::Null).await;
    assert_eq!(status, 200, "{kept}");
    assert_eq!(kept["version"], current["version"]);
    for key in ["never-written", "gone"] {
        let (status, _) = call(&app, ADMIN, "GET", &entry(key), Value::Null).await;
        assert_eq!(status, 404, "{key}");
    }
}

async fn ordinary(
    app: &Router,
    method: &str,
    uri: &str,
    if_match: Option<&str>,
    body: Value,
) -> (u16, Option<String>, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {ADMIN}"))
        .header("content-type", "application/json");
    if let Some(tag) = if_match {
        request = request.header("if-match", tag);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let etag = response
        .headers()
        .get("etag")
        .map(|value| value.to_str().unwrap().to_owned());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, etag, value)
}

// Only the KV routes use the constant message. Ordinary record routes and graph
// routes keep the 412 text they had in 0.6.1.
#[tokio::test]
async fn non_kv_routes_keep_their_precondition_messages() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(FormatVersion::V5), directory.path());

    let record = "/v1/collections/default/vectors/1";
    let (status, stale, _) = ordinary(&app, "PUT", record, None, json!({"vector":[0.0]})).await;
    assert_eq!(status, 204);
    let (status, _, _) = ordinary(&app, "PUT", record, None, json!({"vector":[1.0]})).await;
    assert_eq!(status, 204);
    let (status, _, body) = ordinary(
        &app,
        "PUT",
        record,
        stale.as_deref(),
        json!({"vector":[2.0]}),
    )
    .await;
    assert_eq!(status, 412, "{body}");
    assert_eq!(
        body,
        precondition_body(
            "record changed, was removed, or the tag is from another database or service instance; GET it again"
        )
    );

    let (status, body) = call(&app, ADMIN, "POST", "/v1/graphs", json!({"graph":"g"})).await;
    assert_eq!(status, 201, "{body}");
    let batch = "/v1/graphs/g/batch";
    let (status, body) = call(
        &app,
        ADMIN,
        "POST",
        batch,
        json!({"ops":[{"op":"create_node","key":"n1"}]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (status, node) = call(
        &app,
        ADMIN,
        "POST",
        "/v1/graphs/g/nodes/get",
        json!({"node":{"key":"n1"}}),
    )
    .await;
    assert_eq!(status, 200, "{node}");
    let revision = node["node"]["revision"].as_str().unwrap();
    let (status, body) = call(
        &app,
        ADMIN,
        "POST",
        batch,
        json!({"ops":[{"op":"upsert_node","key":"n1","expected":"absent"}]}),
    )
    .await;
    assert_eq!(status, 412, "{body}");
    assert_eq!(
        body,
        precondition_body(&format!(
            "revision precondition failed; current revision is {revision}"
        ))
    );
    let (status, body) = call(
        &app,
        ADMIN,
        "POST",
        batch,
        json!({"ops":[{"op":"delete_node","key":"missing","expected":{"revision":"1"}}]}),
    )
    .await;
    assert_eq!(status, 412, "{body}");
    assert_eq!(
        body,
        precondition_body("revision precondition failed; record does not exist")
    );
}

/// The payload `kv` stores: "VKV1", namespace length, key length (u16 little
/// endian), namespace, key, value. Anyone who may write ordinary records can
/// forge one at any record ID.
fn kv_shaped(namespace: &str, key: &str, value: &str) -> String {
    let mut payload = b"VKV1".to_vec();
    payload.push(namespace.len() as u8);
    payload.extend_from_slice(&(key.len() as u16).to_le_bytes());
    payload.extend_from_slice(namespace.as_bytes());
    payload.extend_from_slice(key.as_bytes());
    payload.extend_from_slice(value.as_bytes());
    BASE64_STANDARD.encode(payload)
}

async fn forge(app: &Router, token: &str, id: u64, key: &str, value: &str) {
    let uri = format!("/v1/collections/default/vectors/{id}");
    let body = json!({"vector":[0.0],"payload_base64":kv_shaped("n", key, value)});
    let (status, response) = call(app, token, "PUT", &uri, body).await;
    assert_eq!(status, 204, "{response}");
}

fn items(page: &Value) -> Vec<(String, String)> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                item["key"].as_str().unwrap().to_owned(),
                item["value_base64"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
    items
        .iter()
        .map(|(key, value)| ((*key).to_owned(), BASE64_STANDARD.encode(value)))
        .collect()
}

async fn entries(app: &Router, query: &str) -> Value {
    let uri = format!("/v1/collections/default/kv/entries?namespace=n{query}");
    let (status, page) = call(app, ADMIN, "GET", &uri, Value::Null).await;
    assert_eq!(status, 200, "{page}");
    page
}

// A caller that may write ordinary records can forge an entry-shaped record at
// any record ID. It must not change what the list route returns for a real key.
#[tokio::test]
async fn kv_list_ignores_look_alikes_planted_through_the_record_route() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(FormatVersion::V5), directory.path());
    let attacker = credential(&app, json!({"operations":["write"]})).await;
    let path = "/v1/collections/default/kv/entry";
    for (key, value) in [("g", "g"), ("h", "real"), ("i", "i")] {
        let body = json!({"namespace":"n","key":key,"value_base64":BASE64_STANDARD.encode(value)});
        let (status, response) = call(&app, ADMIN, "PUT", path, body).await;
        assert_eq!(status, 200, "{response}");
    }
    let base = first_slot("n", "h");
    // The report's record 7, one past the window of h and one before it.
    let outside = [7, base.wrapping_add(16), base.wrapping_sub(1)];
    for id in outside {
        assert!(id.wrapping_sub(base) >= 16, "{id} is in the window");
        forge(&app, &attacker, id, "h", "SPOOF").await;
    }
    let (status, entry_h) = call(&app, ADMIN, "GET", &entry("h"), Value::Null).await;
    assert_eq!(status, 200, "{entry_h}");
    assert_eq!(entry_h["value_base64"], BASE64_STANDARD.encode("real"));

    let page = entries(&app, "").await;
    assert_eq!(
        items(&page),
        pairs(&[("g", "g"), ("h", "real"), ("i", "i")])
    );
    assert_eq!(page["next_after"], Value::Null);
    // One entry per page: the real h, and the next page starts after it.
    let page = entries(&app, "&limit=1&after=g").await;
    assert_eq!(items(&page), pairs(&[("h", "real")]));
    assert_eq!(page["next_after"], "h");
    let page = entries(&app, "&limit=1&after=h").await;
    assert_eq!(items(&page), pairs(&[("i", "i")]));
    assert_eq!(page["next_after"], Value::Null);

    // Deleting the key leaves no phantom; the stray records are still there.
    assert_eq!(
        call(&app, ADMIN, "DELETE", &entry("h"), Value::Null).await,
        (200, json!({"deleted":true}))
    );
    assert_eq!(
        items(&entries(&app, "").await),
        pairs(&[("g", "g"), ("i", "i")])
    );
    assert_eq!(
        call(&app, ADMIN, "GET", &entry("h"), Value::Null).await.0,
        404
    );
    for id in outside {
        let uri = format!("/v1/collections/default/vectors/{id}");
        assert_eq!(call(&app, ADMIN, "GET", &uri, Value::Null).await.0, 200);
    }
}

// Namespace "n" keys whose first slot lies within 15 of u64::MAX, so the
// 16-slot window wraps past the last record ID; all four share u64::MAX - 2.
// Found offline by a meet-in-the-middle search over the public slot hash.
const WRAPPED: [&str; 4] = [
    "wrap-1e3h8xptc3rmwn",
    "wrap-1mr1oejil0ss3d",
    "wrap-rcxazt3ti43gt3",
    "wrap-zh8a7z15oq0wpx",
];

#[tokio::test]
async fn kv_list_handles_probe_windows_that_wrap_past_the_last_record_id() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(FormatVersion::V5), directory.path());
    let attacker = credential(&app, json!({"operations":["write"]})).await;
    let path = "/v1/collections/default/kv/entry";
    for key in WRAPPED {
        assert_eq!(first_slot("n", key), u64::MAX - 2, "{key}");
    }
    // Put in this order, the keys fill u64::MAX - 2, u64::MAX - 1, u64::MAX
    // and then ID 0, past the wrap.
    let placed = [
        (WRAPPED[3], u64::MAX - 2),
        (WRAPPED[1], u64::MAX - 1),
        (WRAPPED[2], u64::MAX),
        (WRAPPED[0], 0),
    ];
    for (key, id) in placed {
        let body = json!({"namespace":"n","key":key,"value_base64":BASE64_STANDARD.encode(key)});
        let (status, response) = call(&app, ADMIN, "PUT", path, body).await;
        assert_eq!(status, 200, "{response}");
        let uri = format!("/v1/collections/default/vectors/{id}");
        let (status, record) = call(&app, ADMIN, "GET", &uri, Value::Null).await;
        assert_eq!(status, 200, "{key} at {id}: {record}");
        assert_eq!(record["payload_base64"], kv_shaped("n", key, key), "{key}");
    }
    // Look-alikes just before the window, just past it, and one inside it.
    forge(&app, &attacker, u64::MAX - 3, WRAPPED[1], "before").await;
    forge(&app, &attacker, 13, WRAPPED[2], "after").await;
    forge(&app, &attacker, 5, WRAPPED[0], "inside").await;

    let mut sorted = WRAPPED;
    sorted.sort_unstable();
    let expected: Vec<(String, String)> = sorted
        .iter()
        .map(|key| ((*key).to_owned(), BASE64_STANDARD.encode(key)))
        .collect();
    let page = entries(&app, "").await;
    assert_eq!(items(&page), expected);
    assert_eq!(page["next_after"], Value::Null);
    // Page through them one at a time.
    let mut paged = Vec::new();
    let mut after = String::new();
    loop {
        let query = format!("&limit=1{after}");
        let page = entries(&app, &query).await;
        let Some(item) = items(&page).pop() else {
            break;
        };
        after = format!("&after={}", item.0);
        paged.push(item);
        if page["next_after"].is_null() {
            break;
        }
    }
    assert_eq!(paged, expected);
}
