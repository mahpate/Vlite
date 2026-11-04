#![cfg(feature = "server")]

use axum::{Router, body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use vlite::{
    CreateOptions, Database, FormatVersion,
    server::{BearerToken, ServerOptions, Service, configured_admin_router},
};

const ADMIN: &str = "fake_kv_state_admin_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const ENTRY: &str = "/v1/collections/default/kv/entry";

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn database(format: FormatVersion) -> Database {
    Database::create_in_memory(
        1,
        CreateOptions {
            format: Some(format),
            ..Default::default()
        },
    )
    .unwrap()
}

fn start(db: Database, directory: &std::path::Path) -> Router {
    configured_admin_router(
        Service::new(db),
        ServerOptions {
            admin_token: Some(BearerToken::new(ADMIN).unwrap()),
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
    body: Value,
) -> (u16, Value, String) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
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
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(!request_id.is_empty());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, body, request_id)
}

fn put(key: &str) -> Value {
    json!({"namespace":"private-namespace","key":key,"value_base64":"YQ=="})
}

async fn credential(app: &Router, extra: Value) -> (String, String) {
    let mut input = json!({
        "name":"state-test", "collections":["default"], "operations":["read","write"],
        "expires_unix_ms":now_ms() + 600_000
    });
    for (key, value) in extra.as_object().unwrap() {
        input[key] = value.clone();
    }
    let (status, body, _) = raw(app, ADMIN, "POST", "/v1/admin/credentials", input).await;
    assert_eq!(status, 201, "{body}");
    (
        body["token"].as_str().unwrap().into(),
        body["credential"]["id"].as_str().unwrap().into(),
    )
}

fn mutation_events(directory: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(directory.join("audit.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| matches!(event["event"].as_str(), Some("kv_put" | "kv_delete")))
        .collect()
}

#[tokio::test]
async fn past_nonzero_deadlines_are_absent_from_get_and_list() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(FormatVersion::V5), directory.path());
    let past = now_ms().checked_sub(60_000).unwrap();
    assert!(past > 0);
    for (key, expiry) in [
        ("due", Some(past.to_string())),
        ("future", Some(u64::MAX.to_string())),
        ("live", None),
    ] {
        let mut input = put(key);
        input["expires_at_ms"] = json!(expiry);
        let (status, body, _) = raw(&app, ADMIN, "PUT", ENTRY, input).await;
        assert_eq!(status, 200, "{body}");
    }
    for (key, expected_status) in [("due", 404), ("future", 200), ("live", 200)] {
        let uri = format!("{ENTRY}?namespace=private-namespace&key={key}");
        let (status, body, _) = raw(&app, ADMIN, "GET", &uri, Value::Null).await;
        assert_eq!(status, expected_status, "{key}: {body}");
        if expected_status == 404 {
            assert_eq!(body["error"]["code"], "kv_not_found");
        } else {
            assert_eq!(body["key"], key);
        }
    }
    let (status, page, _) = raw(
        &app,
        ADMIN,
        "GET",
        "/v1/collections/default/kv/entries?namespace=private-namespace",
        Value::Null,
    )
    .await;
    assert_eq!(status, 200);
    let keys: Vec<_> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["future", "live"]);
    assert_eq!(page["next_after"], Value::Null);
}

#[tokio::test]
async fn mutation_audit_matches_outcome_identity_collection_and_response_id() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(database(FormatVersion::V5), directory.path());
    let first = credential(&app, json!({})).await;
    let second = credential(&app, json!({})).await;
    assert_ne!(first.1, second.1);
    let (status, tenant, _) = raw(
        &app,
        ADMIN,
        "POST",
        "/v1/admin/tenants",
        json!({"name":"state-tenant"}),
    )
    .await;
    assert_eq!(status, 201, "{tenant}");
    let tenant_id = tenant["tenant"]["id"].as_str().unwrap();
    let tenant_writer = credential(
        &app,
        json!({"tenant_id":tenant_id,"collections":["*"],"operations":["collections","read","write"]}),
    )
    .await;
    let (status, body, _) = raw(
        &app,
        &tenant_writer.0,
        "POST",
        "/v1/collections",
        json!({"name":"items","dimensions":1}),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let mut cas = put("private-key");
    cas["expected"] = json!({"revision":"0"});
    let qualified = format!("t{tenant_id}__items");
    let cases = [
        (
            ADMIN,
            "admin",
            "PUT",
            ENTRY.to_owned(),
            put("private-key"),
            200,
            "ok",
            "default",
        ),
        (
            &first.0,
            first.1.as_str(),
            "PUT",
            ENTRY.to_owned(),
            put("private-key"),
            200,
            "ok",
            "default",
        ),
        (
            &second.0,
            second.1.as_str(),
            "DELETE",
            format!("{ENTRY}?namespace=private-namespace&key=private-key"),
            Value::Null,
            200,
            "ok",
            "default",
        ),
        (
            &first.0,
            first.1.as_str(),
            "PUT",
            ENTRY.to_owned(),
            cas,
            412,
            "precondition_failed",
            "(invalid)",
        ),
        (
            &first.0,
            first.1.as_str(),
            "PUT",
            "/v1/collections/outside-binding/kv/entry".to_owned(),
            put("private-key"),
            403,
            "forbidden_scope",
            "(invalid)",
        ),
        (
            &tenant_writer.0,
            tenant_writer.1.as_str(),
            "PUT",
            "/v1/collections/items/kv/entry".to_owned(),
            put("private-key"),
            200,
            "ok",
            qualified.as_str(),
        ),
    ];
    for (index, (token, principal, method, uri, input, expected_status, outcome, collection)) in
        cases.into_iter().enumerate()
    {
        let (status, body, request_id) = raw(&app, token, method, &uri, input).await;
        assert_eq!(status, expected_status, "{body}");
        if status != 200 {
            assert_eq!(body["error"]["code"], outcome);
        }
        let events = mutation_events(directory.path());
        assert_eq!(events.len(), index + 1);
        let event = &events[index];
        assert_eq!(
            event["event"],
            if method == "PUT" {
                "kv_put"
            } else {
                "kv_delete"
            }
        );
        assert_eq!(event["outcome"], outcome);
        assert_eq!(event["principal"], principal);
        assert_eq!(event["collection"], collection);
        assert_eq!(event["request_id"], request_id);
        for field in [
            "namespace",
            "key",
            "value",
            "value_base64",
            "token",
            "payload",
        ] {
            assert!(event.get(field).is_none(), "{field}: {event}");
        }
        assert!(!event.to_string().contains("private-"));
    }
}

#[tokio::test]
async fn kv_capability_requires_v5_not_v4() {
    for (format, expected) in [(FormatVersion::V4, false), (FormatVersion::V5, true)] {
        let directory = tempfile::tempdir().unwrap();
        let app = start(database(format), directory.path());
        let (status, caps, _) = raw(&app, ADMIN, "GET", "/v1/capabilities", Value::Null).await;
        assert_eq!(status, 200, "{caps}");
        assert_eq!(caps["features"]["kv"], expected, "{format:?}");
    }
}
