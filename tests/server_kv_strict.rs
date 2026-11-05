#![cfg(feature = "server")]

use axum::{Router, body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use vlite::{
    CreateOptions, Database, FormatVersion,
    kv::KvPut,
    server::{BearerToken, ServerOptions, Service, configured_admin_router},
};

const ADMIN: &str = "fake_kv_admin_token_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const ENTRY: &str = "/v1/collections/default/kv/entry";
const ENTRIES: &str = "/v1/collections/default/kv/entries";
const INVALID_ENCODINGS: [&str; 9] = [
    "%",
    "%4",
    "%G1",
    "%1G",
    "%FF",
    "%C3",
    "%C0%AF",
    "%ED%A0%80",
    "%E2%82",
];

fn start(directory: &std::path::Path, entries: &[(&str, &str)]) -> Router {
    let mut db = Database::create_in_memory(
        1,
        CreateOptions {
            format: Some(FormatVersion::V5),
            ..Default::default()
        },
    )
    .unwrap();
    for (namespace, key) in entries {
        db.kv_store("default")
            .unwrap()
            .put(namespace, key, b"previous", KvPut::default())
            .unwrap();
    }
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

async fn raw(app: &Router, method: &str, uri: &str, body: String) -> (u16, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("authorization", format!("Bearer {ADMIN}"))
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
    (status, value)
}

fn entry(namespace: &str, key: &str) -> String {
    let url = reqwest::Url::parse_with_params(
        "http://localhost/v1/collections/default/kv/entry",
        &[("namespace", namespace), ("key", key)],
    )
    .unwrap();
    format!("{}?{}", url.path(), url.query().unwrap())
}

async fn previous(app: &Router, namespace: &str, key: &str) -> Value {
    let (status, body) = raw(app, "GET", &entry(namespace, key), String::new()).await;
    assert_eq!(status, 200, "{namespace:?}/{key:?}: {body}");
    assert_eq!(body["key"], key);
    assert_eq!(body["value_base64"], "cHJldmlvdXM=");
    assert_eq!(body["expires_at_ms"], Value::Null);
    body
}

async fn reject_expected(code: &str, cases: &[&str]) {
    let directory = tempfile::tempdir().unwrap();
    let app = start(directory.path(), &[("n", "sentinel")]);
    let before = previous(&app, "n", "sentinel").await;
    for expected in cases {
        // Keep object member order, including an unknown first member, on the wire.
        let input = format!(
            r#"{{"namespace":"n","key":"sentinel","value_base64":"bmV3","expected":{expected}}}"#
        );
        let (status, body) = raw(&app, "PUT", ENTRY, input).await;
        let (read_status, after) = raw(&app, "GET", &entry("n", "sentinel"), String::new()).await;
        assert_eq!(read_status, 200, "expected={expected}: {after}");
        assert_eq!(
            after, before,
            "expected={expected}: rejected PUT wrote state"
        );
        assert_eq!(status, 422, "expected={expected}: {body}");
        assert_eq!(body["error"]["code"], code, "expected={expected}: {body}");
    }
}

// A wrong JSON type for `expected` (not a string or an object) is a shape failure and
// therefore `invalid_json`, like the wrong-typed revision in the wire goldens. A string
// that is not "any" or "absent" has the right type and is invalid input (server_kv.rs).
#[tokio::test]
async fn expected_integers_and_floats_are_invalid_json() {
    reject_expected("invalid_json", &["-1", "0", "42", "0.5", "1e300"]).await;
}

#[tokio::test]
async fn expected_arrays_are_invalid_json() {
    reject_expected(
        "invalid_json",
        &["[]", r#"["any"]"#, r#"[{"revision":"1"}]"#],
    )
    .await;
}

#[tokio::test]
async fn expected_empty_object_is_invalid_json() {
    reject_expected("invalid_json", &["{}"]).await;
}

#[tokio::test]
async fn expected_booleans_and_null_are_invalid_json() {
    reject_expected("invalid_json", &["true", "false", "null"]).await;
}

// A syntax error inside a rejected array keeps the extractor's 400 syntax status.
#[tokio::test]
async fn expected_array_with_a_syntax_error_is_a_syntax_failure() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(directory.path(), &[("n", "sentinel")]);
    let before = previous(&app, "n", "sentinel").await;
    let input = r#"{"namespace":"n","key":"sentinel","value_base64":"bmV3","expected":[1,,2]}"#;
    let (status, body) = raw(&app, "PUT", ENTRY, input.to_owned()).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["code"], "invalid_json", "{body}");
    assert_eq!(previous(&app, "n", "sentinel").await, before);
}

// An object with an unknown member is an unknown JSON field.
#[tokio::test]
async fn expected_unknown_first_key_is_invalid_json() {
    reject_expected(
        "invalid_json",
        &[r#"{"extra":"1"}"#, r#"{"extra":"1","revision":"1"}"#],
    )
    .await;
}

#[tokio::test]
async fn expected_revision_unknown_field_is_invalid_json() {
    reject_expected("invalid_json", &[r#"{"revision":"1","extra":2}"#]).await;
}

async fn reject_queries(method: &str, queries: &[String], list: bool) {
    let directory = tempfile::tempdir().unwrap();
    let entries = [
        ("n", "sentinel"),
        ("n", "%"),
        ("n", "%4"),
        ("n", "%G1"),
        ("n", "%1G"),
        ("n", "\u{fffd}"),
        ("n", "\u{fffd}\u{fffd}"),
        ("n", "\u{fffd}\u{fffd}\u{fffd}"),
        ("n%", "sentinel"),
    ];
    let app = start(directory.path(), &entries);
    let mut before = Vec::new();
    for (namespace, key) in entries {
        before.push(previous(&app, namespace, key).await);
    }
    for query in queries {
        let uri = format!("{}?{query}", if list { ENTRIES } else { ENTRY });
        let (status, body) = raw(&app, method, &uri, String::new()).await;
        for ((namespace, key), expected) in entries.iter().zip(&before) {
            let (read_status, after) =
                raw(&app, "GET", &entry(namespace, key), String::new()).await;
            assert_eq!(read_status, 200, "{method} {uri}: lost {key:?}: {after}");
            assert_eq!(&after, expected, "{method} {uri}: changed {key:?}");
        }
        assert_eq!(status, 400, "{method} {uri}: {body}");
        assert_eq!(body["error"]["code"], "invalid_query", "{method} {uri}");
    }
}

#[tokio::test]
async fn get_rejects_truncated_percent_escapes() {
    reject_queries(
        "GET",
        &[
            "namespace=n&key=%".into(),
            "namespace=n&key=%4".into(),
            "namespace=n%&key=sentinel".into(),
        ],
        false,
    )
    .await;
}

#[tokio::test]
async fn get_rejects_bad_hex_digits() {
    reject_queries(
        "GET",
        &["namespace=n&key=%G1".into(), "namespace=n&key=%1G".into()],
        false,
    )
    .await;
}

#[tokio::test]
async fn lowercase_and_uppercase_hex_address_the_same_keys() {
    let directory = tempfile::tempdir().unwrap();
    let app = start(
        directory.path(),
        &[("n", "/"), ("n", "\u{fffd}"), ("n", "a b")],
    );
    for (key, lower, upper) in [
        ("/", "%2f", "%2F"),
        ("\u{fffd}", "%ef%bf%bd", "%EF%BF%BD"),
        ("a b", "a+b", "a%20b"),
    ] {
        for (write, read) in [(lower, upper), (upper, lower)] {
            let (status, written) = raw(
                &app,
                "PUT",
                ENTRY,
                json!({"namespace":"n","key":key,"value_base64":"YQ=="}).to_string(),
            )
            .await;
            assert_eq!(status, 200, "{key:?}: {written}");
            let mut first = None;
            for encoded in [write, read] {
                let uri = format!("{ENTRY}?namespace=n&key={encoded}");
                let (status, body) = raw(&app, "GET", &uri, String::new()).await;
                assert_eq!(status, 200, "{uri}: {body}");
                assert_eq!(body["key"], key);
                assert_eq!(body["value_base64"], "YQ==");
                assert_eq!(body["version"], written["version"]);
                assert_eq!(body["expires_at_ms"], Value::Null);
                if let Some(first) = &first {
                    assert_eq!(&body, first, "{uri}: GET changed state");
                } else {
                    first = Some(body);
                }
            }
        }
    }
}

#[tokio::test]
async fn get_rejects_invalid_utf8_beyond_ff() {
    let queries = ["%C3", "%C0%AF", "%ED%A0%80", "%E2%82"]
        .map(|encoded| format!("namespace=n&key={encoded}"));
    reject_queries("GET", &queries, false).await;
}

#[tokio::test]
async fn delete_rejects_invalid_encoding_without_deleting() {
    let mut queries: Vec<_> = INVALID_ENCODINGS
        .map(|encoded| format!("namespace=n&key={encoded}"))
        .into();
    queries.push("namespace=n%&key=sentinel".into());
    reject_queries("DELETE", &queries, false).await;
}

#[tokio::test]
async fn list_rejects_invalid_encoding_in_every_component() {
    let queries: Vec<_> = INVALID_ENCODINGS
        .into_iter()
        .flat_map(|encoded| {
            [
                format!("namespace=n&prefix={encoded}"),
                format!("namespace=n&after={encoded}"),
                format!("namespace=n{encoded}"),
            ]
        })
        .collect();
    reject_queries("GET", &queries, true).await;
}
