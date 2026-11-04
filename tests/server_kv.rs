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
        assert_eq!(call(&app, &token, "GET", uri, Value::Null).await.0, 404);
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
