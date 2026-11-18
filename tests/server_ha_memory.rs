#![cfg(feature = "server")]
//! TEST-011: memory reservations and diagnostics in reader and writer modes.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::Value;
use tempfile::TempDir;
use tower::ServiceExt;
use vlite::{
    CollectionSpec, Database, FormatVersion,
    server::{BearerToken, ServeMode, ServerOptions, Service, configured_admin_router},
};

const ADMIN: &str = "fake_ha_memory_admin_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const READ_ONLY: &str = "fake_ha_memory_readonly_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const BINDINGS: &str = r#"{"version":"memory-bindings-v1","bindings":[{"binding":"notes","tenant":"@root","generation":1,"state":"active","graph":"notes_graph","evidence_collection":"notes_ev","schema_version":"memory-schema-v1","ranking_version":"memory-ranking-v1","digest_version":"memory-request-v1","receipt_window_ms":86400000,"grants":[{"principal":"abcdefghij27","scopes":[{"agent":"a"}],"raw_access":false}]}]}"#;

struct Fixture {
    app: Router,
    _directory: TempDir,
}

fn fixture(mode: ServeMode, bindings: bool) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("memory.vlite");
    let mut db = Database::create_with_format(&path, 2, FormatVersion::V5).unwrap();
    for name in ["notes_ev", "ordinary"] {
        db.create_collection(
            name,
            CollectionSpec {
                dimensions: 2,
                settings: Default::default(),
            },
        )
        .unwrap();
    }
    db.collection_mut("ordinary")
        .unwrap()
        .put(7, &[1.0, 0.0], b"")
        .unwrap();
    for name in ["notes_graph", "ordinary_graph"] {
        db.create_graph(name).unwrap();
    }
    db.close().unwrap();
    let db = match mode {
        ServeMode::Reader => Database::open_read_only(&path).unwrap(),
        ServeMode::Writer => Database::open(&path).unwrap(),
        ServeMode::Standalone => panic!("this fixture requires an HA serve mode"),
    };
    let memory_bindings_file = bindings.then(|| {
        let path = directory.path().join("memory-bindings.json");
        std::fs::write(&path, BINDINGS).unwrap();
        path
    });
    let app = configured_admin_router(
        Service::new(db).with_serve_mode(mode).unwrap(),
        ServerOptions {
            loopback_only: true,
            admin_token: Some(BearerToken::new(ADMIN).unwrap()),
            read_only_token: Some(BearerToken::new(READ_ONLY).unwrap()),
            admin_directory: Some(directory.path().into()),
            memory_bindings_file,
            ..Default::default()
        },
    )
    .unwrap();
    Fixture {
        app,
        _directory: directory,
    }
}

async fn call(
    app: &Router,
    token: &str,
    method: &str,
    path: &str,
    body: &str,
) -> (StatusCode, String) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

fn assert_no_reserved_names(body: &str) {
    for name in ["notes_ev", "notes_graph"] {
        assert!(!body.contains(name), "response reveals {name}: {body}");
    }
}

async fn reserved_names_answer_like_missing(mode: ServeMode) {
    let fixture = fixture(mode, true);
    for token in [ADMIN, READ_ONLY] {
        for (method, template, body, expected_status, expected_code) in [
            (
                "GET",
                "/v1/collections/$c",
                "",
                StatusCode::NOT_FOUND,
                "collection_not_found",
            ),
            (
                "GET",
                "/v1/collections/$c/vectors/7",
                "",
                StatusCode::NOT_FOUND,
                "collection_not_found",
            ),
            (
                "POST",
                "/v1/collections/$c/search",
                r#"{"vector":[1,0],"k":1}"#,
                StatusCode::NOT_FOUND,
                "collection_not_found",
            ),
            (
                "GET",
                "/v1/graphs/$g",
                "",
                StatusCode::CONFLICT,
                "graph_not_found",
            ),
        ] {
            let reserved_path = template
                .replace("$c", "notes_ev")
                .replace("$g", "notes_graph");
            let missing_path = template
                .replace("$c", "absent_ev")
                .replace("$g", "absent_graph");
            let reserved = call(&fixture.app, token, method, &reserved_path, body).await;
            let missing = call(&fixture.app, token, method, &missing_path, body).await;
            assert_eq!(
                reserved.0, expected_status,
                "{mode:?} {template}: {reserved:?}"
            );
            assert_eq!(reserved, missing, "{mode:?} {template}");
            let error: Value = serde_json::from_str(&reserved.1).unwrap();
            assert_eq!(error["error"]["code"], expected_code);
            assert_no_reserved_names(&reserved.1);

            let ordinary_path = template
                .replace("$c", "ordinary")
                .replace("$g", "ordinary_graph");
            let ordinary = call(&fixture.app, token, method, &ordinary_path, body).await;
            assert_eq!(ordinary.0, StatusCode::OK, "{ordinary_path}: {ordinary:?}");
            if method == "POST" {
                let result: Value = serde_json::from_str(&ordinary.1).unwrap();
                assert_eq!(result["hits"][0]["id"], "7", "{result}");
            }
        }

        for (path, field, expected_names) in [
            ("/v1/collections", "items", vec!["default", "ordinary"]),
            ("/v1/graphs", "graphs", vec!["ordinary_graph"]),
        ] {
            let (status, body) = call(&fixture.app, token, "GET", path, "").await;
            assert_eq!(status, StatusCode::OK, "{mode:?} {path}: {body}");
            assert_no_reserved_names(&body);
            let listed: Value = serde_json::from_str(&body).unwrap();
            let mut names: Vec<_> = listed[field]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["name"].as_str().unwrap())
                .collect();
            names.sort_unstable();
            assert_eq!(names, expected_names, "{mode:?} {path}: {listed}");
        }
    }
}

#[tokio::test]
async fn reader_reserved_names_answer_like_missing() {
    reserved_names_answer_like_missing(ServeMode::Reader).await;
}

#[tokio::test]
async fn writer_reserved_names_answer_like_missing() {
    reserved_names_answer_like_missing(ServeMode::Writer).await;
}

#[tokio::test]
async fn reader_mutations_on_reserved_names_are_reader_mode_not_leaks() {
    let fixture = fixture(ServeMode::Reader, true);
    for (method, template, body) in [
        ("PUT", "/v1/collections/$c/vectors/7", r#"{"vector":[0,1]}"#),
        ("DELETE", "/v1/collections/$c", ""),
        ("DELETE", "/v1/graphs/$g", ""),
    ] {
        let reserved_path = template
            .replace("$c", "notes_ev")
            .replace("$g", "notes_graph");
        let missing_path = template
            .replace("$c", "absent_ev")
            .replace("$g", "absent_graph");
        let reserved = call(&fixture.app, ADMIN, method, &reserved_path, body).await;
        let missing = call(&fixture.app, ADMIN, method, &missing_path, body).await;
        assert_eq!(
            reserved.0,
            StatusCode::FORBIDDEN,
            "{template}: {reserved:?}"
        );
        assert_eq!(reserved, missing, "{template}");
        let error: Value = serde_json::from_str(&reserved.1).unwrap();
        assert_eq!(error["error"]["code"], "reader_mode", "{error}");
        assert_no_reserved_names(&reserved.1);
    }
}

#[tokio::test]
async fn metrics_refused_with_bindings_does_not_affect_health_ready_or_status() {
    for mode in [ServeMode::Reader, ServeMode::Writer] {
        let fixture = fixture(mode, true);
        for _ in 0..2 {
            let (status, body) = call(&fixture.app, ADMIN, "GET", "/metrics", "").await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{mode:?}: {body}");
            let error: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(error["error"]["code"], "forbidden_scope", "{error}");
            assert_no_reserved_names(&body);
            for path in ["/health", "/ready", "/v1/admin/serve-status"] {
                let (status, body) = call(&fixture.app, ADMIN, "GET", path, "").await;
                assert_eq!(status, StatusCode::OK, "{mode:?} {path}: {body}");
                assert_no_reserved_names(&body);
                if path == "/v1/admin/serve-status" {
                    let report: Value = serde_json::from_str(&body).unwrap();
                    assert_eq!(report["mode"], mode.as_str(), "{report}");
                    assert_eq!(report.get("sequence"), Some(&Value::Null), "{report}");
                }
            }
        }
    }
}

#[tokio::test]
async fn without_bindings_metrics_and_sequence_are_unchanged() {
    for mode in [ServeMode::Reader, ServeMode::Writer] {
        let fixture = fixture(mode, false);
        let (status, body) = call(&fixture.app, ADMIN, "GET", "/metrics", "").await;
        assert_eq!(status, StatusCode::OK, "{mode:?}: {body}");
        let (status, body) = call(&fixture.app, ADMIN, "GET", "/v1/admin/serve-status", "").await;
        assert_eq!(status, StatusCode::OK, "{mode:?}: {body}");
        let report: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(report["mode"], mode.as_str(), "{report}");
        assert!(report["sequence"].is_u64(), "{report}");
    }
}

// No public in-process gateway router is exposed. Binding-generation fallback
// (x-vlite-binding-generation) is covered in src/server/gateway_mode.rs by
// missing_mismatched_or_invalid_reader_binding_generation_falls_back_to_writer.
