//! TEST-012 (feature-agent-graph EPIC-011, ITEM-040..ITEM-042, ITEM-044;
//! AC-020, REQ-043): REST graph routes on `vlite-server` built with
//! `server,graph`.
//!
//! - Every graph route answers for admin, read-only and scoped credentials
//!   exactly as the documented scope model says (`graph_read` /
//!   `graph_write` operations plus a `graphs` name binding); credentials
//!   without graph operations never reach graph data, whatever their
//!   collection binding.
//! - Batches that touch vector records (attach, detach, put_vector,
//!   delete_vector) also need `write` on every such collection, and live
//!   vector quotas apply.
//! - Limits above `GraphLimits::CEILING`, oversized bodies, malformed input
//!   and graph errors are explicit 4xx answers, never 500.
//! - Vector-only files answer 409 `graphs_not_enabled` on every graph route.
//! - Concurrent readers and writers share the single owner; read-guard hold
//!   time p50/p99 is recorded (`GET /v1/admin/graph/stats`).
//! - The served OpenAPI document is the unchanged `src/openapi.json` plus the
//!   `src/openapi_graph.json` paths (RD-013).
//! - Reads match `vlite-graph-cli` output for the same file.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tempfile::tempdir;
use tower::ServiceExt;
use vlite::{
    CreateOptions, Database, FormatVersion,
    server::{BearerToken, ServerOptions, Service, configured_admin_router},
};

const ADMIN: &str = "rest_graph_admin_token_0123456789abcdef";
const READ_ONLY: &str = "rest_graph_read_only_token_0123456789ab";

/// Every graph route: method, concrete URI for graph "g", body, read route.
fn routes() -> Vec<(&'static str, &'static str, Option<Value>, bool)> {
    vec![
        ("GET", "/v1/graphs", None, true),
        ("GET", "/v1/graphs/g", None, true),
        (
            "POST",
            "/v1/graphs/g/nodes/get",
            Some(json!({"node": {"key": "n1"}})),
            true,
        ),
        (
            "POST",
            "/v1/graphs/g/edges/get",
            Some(json!({"edge": {"id": "0"}})),
            true,
        ),
        ("POST", "/v1/graphs/g/nodes/find", Some(json!({})), true),
        (
            "POST",
            "/v1/graphs/g/neighbors",
            Some(json!({"node": {"key": "n1"}})),
            true,
        ),
        (
            "POST",
            "/v1/graphs/g/traverse",
            Some(json!({"start": [{"key": "n1"}]})),
            true,
        ),
        (
            "POST",
            "/v1/graphs/g/shortest-path",
            Some(json!({"source": {"key": "n1"}, "destination": {"key": "n2"}})),
            true,
        ),
        (
            "POST",
            "/v1/graphs/g/explain",
            Some(json!({"operation": "traverse", "start": [{"key": "n1"}]})),
            true,
        ),
        (
            "POST",
            "/v1/graphs/g/batch",
            Some(json!({"ops": [{"op": "upsert_node", "key": "n9"}]})),
            false,
        ),
        ("POST", "/v1/graphs", Some(json!({"graph": "h"})), false),
        ("DELETE", "/v1/graphs/empty", None, false),
    ]
}

/// Records 1..=4 in `default`; graph "g" has nodes n1..n4 (record n attached
/// to node n) and edges n1-LINKS->n2-LINKS->n3; graph "empty" has no nodes.
fn database(path: &Path, graph: bool) -> Database {
    let mut db = if graph {
        Database::create_with_graph(path, 2, CreateOptions::default()).unwrap()
    } else {
        Database::create_with_format(path, 2, FormatVersion::V5).unwrap()
    };
    for id in 1..=4u64 {
        db.put(id, &[id as f32, 1.0], format!("r{id}").as_bytes())
            .unwrap();
    }
    db
}

fn options(directory: &Path) -> ServerOptions {
    ServerOptions {
        admin_token: Some(BearerToken::new(ADMIN).unwrap()),
        read_only_token: Some(BearerToken::new(READ_ONLY).unwrap()),
        admin_directory: Some(directory.join("admin")),
        ..ServerOptions::default()
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    path: PathBuf,
    service: Service,
    app: Router,
}

async fn fixture(graph: bool) -> Fixture {
    let directory = tempdir().unwrap();
    let path = directory.path().join("graph.vlite");
    std::fs::create_dir(directory.path().join("admin")).unwrap();
    let service = Service::new(database(&path, graph));
    let app = configured_admin_router(service.clone(), options(directory.path())).unwrap();
    let fixture = Fixture {
        _directory: directory,
        path,
        service,
        app,
    };
    if graph {
        let app = &fixture.app;
        for name in ["g", "empty"] {
            let (status, body) = call(
                app,
                Some(ADMIN),
                "POST",
                "/v1/graphs",
                Some(json!({"graph": name})),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
        }
        let mut ops = Vec::new();
        for id in 1..=4u64 {
            ops.push(
                json!({"op": "create_node", "key": format!("n{id}"), "labels": ["Doc"],
                "properties": {"rank": {"type": "integer", "value": id.to_string()}}}),
            );
            ops.push(
                json!({"op": "attach", "node": format!("n{id}"), "collection": "default",
                "record": id.to_string()}),
            );
        }
        ops.push(json!({"op": "create_edge", "key": "e12", "source": "n1",
            "relationship": "LINKS", "destination": "n2"}));
        ops.push(
            json!({"op": "create_edge", "source": "n2", "relationship": "LINKS",
            "destination": "n3"}),
        );
        let (status, body) = call(
            app,
            Some(ADMIN),
            "POST",
            "/v1/graphs/g/batch",
            Some(json!({"ops": ops})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    fixture
}

async fn send(
    app: &Router,
    token: Option<&str>,
    method: &str,
    uri: &str,
    body: Body,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    request = request.header(header::CONTENT_TYPE, "application/json");
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    assert_ne!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{method} {uri}: {value}"
    );
    (status, value)
}

async fn call(
    app: &Router,
    token: Option<&str>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let body = body.map_or_else(Body::empty, |value| Body::from(value.to_string()));
    send(app, token, method, uri, body).await
}

fn code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("")
}

async fn credential(app: &Router, request: Value) -> String {
    let mut request = request;
    request["name"] = json!("agent");
    request["expires_unix_ms"] = json!(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 3_600_000
    );
    let (status, body) = call(
        app,
        Some(ADMIN),
        "POST",
        "/v1/admin/credentials",
        Some(request),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["token"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn admin_crud_reads_explain_and_batch_follow_the_cli_contract() {
    let f = fixture(true).await;
    let app = &f.app;
    let (status, body) = call(app, Some(ADMIN), "GET", "/v1/graphs", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({"graphs": [
            {"name": "empty", "node_count": 0, "edge_count": 0},
            {"name": "g", "node_count": 4, "edge_count": 2},
        ]})
    );
    let (status, body) = call(app, Some(ADMIN), "GET", "/v1/graphs/g", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["graph"]["node_count"], 4);

    let (status, body) = call(
        app,
        Some(ADMIN),
        "POST",
        "/v1/graphs/g/nodes/get",
        Some(json!({"node": {"key": "n1"}, "projection": {"only": ["rank"]}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["graph"], "g");
    assert_eq!(body["node"]["key"], "n1");
    assert!(body["node"]["id"].is_string(), "u64 IDs are strings");
    assert!(body["view_sequence"].is_string());
    assert_eq!(body["complete"], true);

    let (status, body) = call(
        app,
        Some(ADMIN),
        "POST",
        "/v1/graphs/g/edges/get",
        Some(json!({"edge": {"key": "e12"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["edge"]["source_key"], "n1");

    let (status, body) = call(
        app,
        Some(ADMIN),
        "POST",
        "/v1/graphs/g/nodes/find",
        Some(json!({"label": "Doc", "limit": 2})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["nodes"].as_array().unwrap().len(), 2);
    let cursor = body["cursor"].as_str().unwrap().to_owned();
    let (status, body) = call(
        app,
        Some(ADMIN),
        "POST",
        "/v1/graphs/g/nodes/find",
        Some(json!({"label": "Doc", "limit": 2, "cursor": cursor})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["nodes"][0]["key"], "n3");

    let (status, body) = call(
        app,
        Some(ADMIN),
        "POST",
        "/v1/graphs/g/neighbors",
        Some(json!({"node": {"key": "n2"}, "direction": "both", "types": ["LINKS"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["edges"].as_array().unwrap().len(), 2);

    let (status, body) = call(
        app,
        Some(ADMIN),
        "POST",
        "/v1/graphs/g/traverse",
        Some(json!({"start": [{"key": "n1"}], "include_paths": true,
            "limits": {"max_hops": 2}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["nodes"].as_array().unwrap().len(), 3);
    assert_eq!(body["paths"].as_array().unwrap().len(), 2);

    let (status, body) = call(
        app,
        Some(ADMIN),
        "POST",
        "/v1/graphs/g/shortest-path",
        Some(json!({"source": {"key": "n1"}, "destination": {"key": "n3"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["shortest_path"]["outcome"], "found");
    assert_eq!(
        body["shortest_path"]["path"]["edges"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let (status, body) = call(
        app,
        Some(ADMIN),
        "POST",
        "/v1/graphs/g/explain",
        Some(json!({"operation": "find-nodes", "label": "Doc"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["explain"]["query"]["operation"], "find-nodes");
    assert_eq!(body["explain"]["indexes"], json!(["label_index_scan"]));
    for bad in [json!({"label": "Doc"}), json!({"operation": "batch"})] {
        let (status, body) =
            call(app, Some(ADMIN), "POST", "/v1/graphs/g/explain", Some(bad)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    }

    // Batch: atomic; an error writes nothing.
    let (status, body) = call(
        app,
        Some(ADMIN),
        "POST",
        "/v1/graphs/g/batch",
        Some(json!({"ops": [
            {"op": "create_node", "key": "n5"},
            {"op": "create_node", "key": "n1"},
        ]})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "graph_key_exists");
    let (status, body) = call(
        app,
        Some(ADMIN),
        "POST",
        "/v1/graphs/g/batch",
        Some(json!({"ops": [
            {"op": "create_node", "key": "n5"},
            {"op": "put_vector", "collection": "default", "id": "5", "vector": [5.0, 1.0]},
            {"op": "attach", "node": "n5", "collection": "default", "record": 5},
            {"op": "patch_node", "key": "n5", "add_labels": ["Doc"], "set": {"rank": {"type": "integer", "value": "5"}}},
        ]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["revision"].is_string());
    assert_eq!(body["outcomes"][1], "vector");
    assert_eq!(
        body["outcomes"][2]["attached"]["node"],
        body["outcomes"][0]["node"]["id"]
    );

    // Graph lifecycle errors are explicit.
    for (method, uri, body, status, expected) in [
        (
            "POST",
            "/v1/graphs",
            Some(json!({"graph": "g"})),
            409,
            "graph_exists",
        ),
        (
            "POST",
            "/v1/graphs",
            Some(json!({"graph": "Bad"})),
            422,
            "invalid_graph_name",
        ),
        ("DELETE", "/v1/graphs/g", None, 409, "graph_not_empty"),
        ("DELETE", "/v1/graphs/zz", None, 409, "graph_not_found"),
        ("GET", "/v1/graphs/zz", None, 409, "graph_not_found"),
        (
            "POST",
            "/v1/graphs/zz/traverse",
            Some(json!({"start": [{"key": "n1"}]})),
            409,
            "graph_not_found",
        ),
        (
            "POST",
            "/v1/graphs/g/batch",
            Some(
                json!({"ops": [{"op": "create_edge", "source": "n1", "relationship": "X",
                "destination": "nope"}]}),
            ),
            409,
            "graph_node_not_found",
        ),
    ] {
        let (actual, response) = call(app, Some(ADMIN), method, uri, body).await;
        assert_eq!(actual.as_u16(), status, "{method} {uri}: {response}");
        assert_eq!(code(&response), expected, "{method} {uri}");
    }
    let (status, body) = call(app, Some(ADMIN), "DELETE", "/v1/graphs/empty", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"dropped": "empty"}));
}

#[tokio::test]
async fn vector_only_files_answer_graphs_not_enabled_on_every_route() {
    let f = fixture(false).await;
    for (method, uri, body, _) in routes() {
        let (status, response) = call(&f.app, Some(ADMIN), method, uri, body).await;
        assert_eq!(status, StatusCode::CONFLICT, "{method} {uri}: {response}");
        assert_eq!(code(&response), "graphs_not_enabled", "{method} {uri}");
    }
    // Vector routes are unchanged.
    let (status, body) = call(&f.app, Some(ADMIN), "GET", "/v1/vectors/1", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = call(&f.app, Some(READ_ONLY), "GET", "/v1/graphs", None).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn authorization_matrix_per_role_operation_and_graph_binding() {
    let f = fixture(true).await;
    let app = &f.app;
    let vector_all = credential(
        app,
        json!({"collections": ["*"], "operations": ["read", "search", "write", "collections"]}),
    )
    .await;
    let reader_g = credential(
        app,
        json!({"collections": ["*"], "operations": ["graph_read"], "graphs": ["g"]}),
    )
    .await;
    let writer_g = credential(
        app,
        json!({"collections": ["default"], "operations": ["graph_write", "write"],
            "graphs": ["g"]}),
    )
    .await;
    let full = credential(
        app,
        json!({"collections": ["*"], "operations": ["graph_read", "graph_write", "write"],
            "graphs": ["*"]}),
    )
    .await;

    for (method, uri, body, read) in routes() {
        // Unauthenticated.
        let (status, _) = call(app, None, method, uri, body.clone()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri}");
        // Read-only token: graph reads only.
        let (status, response) = call(app, Some(READ_ONLY), method, uri, body.clone()).await;
        if read {
            assert!(status.is_success(), "read-only {method} {uri}: {response}");
        } else {
            assert_eq!(status, StatusCode::FORBIDDEN, "read-only {method} {uri}");
            assert_eq!(code(&response), "read_only");
        }
        // Collection scope never implies graph scope.
        let (status, response) = call(app, Some(&vector_all), method, uri, body.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "vector-only {method} {uri}");
        assert_eq!(code(&response), "forbidden_scope");
        // graph_read bound to "g".
        let (status, response) = call(app, Some(&reader_g), method, uri, body.clone()).await;
        if read {
            assert!(status.is_success(), "reader {method} {uri}: {response}");
        } else {
            assert_eq!(status, StatusCode::FORBIDDEN, "reader {method} {uri}");
        }
        // graph_write without graph_read: writes on "g" only.
        let (status, response) = call(app, Some(&writer_g), method, uri, body.clone()).await;
        if uri == "/v1/graphs/g/batch" {
            assert_eq!(status, StatusCode::OK, "writer {method} {uri}: {response}");
        } else {
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "writer {method} {uri}: {response}"
            );
        }
    }

    // Name binding: graphs outside the binding are indistinguishable from
    // missing ones and are filtered from the list.
    for uri in ["/v1/graphs/empty", "/v1/graphs/zz"] {
        let (status, body) = call(app, Some(&reader_g), "GET", uri, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
        assert_eq!(code(&body), "forbidden_scope");
    }
    let (_, body) = call(app, Some(&reader_g), "GET", "/v1/graphs", None).await;
    assert_eq!(
        body["graphs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["g"]
    );
    let (_, body) = call(app, Some(READ_ONLY), "GET", "/v1/graphs", None).await;
    assert_eq!(body["graphs"].as_array().unwrap().len(), 2);
    let (status, body) = call(
        app,
        Some(&writer_g),
        "POST",
        "/v1/graphs",
        Some(json!({"graph": "other"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = call(
        app,
        Some(&full),
        "POST",
        "/v1/graphs",
        Some(json!({"graph": "other"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, _) = call(app, Some(&full), "DELETE", "/v1/graphs/other", None).await;
    assert_eq!(status, StatusCode::OK);

    // Scoped credentials never see the admin hold-time statistics.
    for token in [&reader_g, &full] {
        let (status, _) = call(app, Some(token), "GET", "/v1/admin/graph/stats", None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    let (status, _) = call(app, Some(READ_ONLY), "GET", "/v1/admin/graph/stats", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // The credential view reports the binding.
    let (_, body) = call(app, Some(ADMIN), "GET", "/v1/admin/credentials", None).await;
    let items = body["items"].as_array().unwrap();
    assert!(items.iter().any(|item| item["graphs"] == json!(["g"])));
    assert!(items.iter().any(|item| item["graphs"].is_null()));
}

#[tokio::test]
async fn credential_graph_scope_is_validated() {
    let f = fixture(true).await;
    for request in [
        // Graph operations need a graph binding and vice versa.
        json!({"collections": ["*"], "operations": ["graph_read"]}),
        json!({"collections": ["*"], "operations": ["read"], "graphs": ["g"]}),
        json!({"collections": ["*"], "operations": ["graph_read"], "graphs": []}),
        json!({"collections": ["*"], "operations": ["graph_read"], "graphs": ["*", "g"]}),
        json!({"collections": ["*"], "operations": ["graph_read"], "graphs": ["Bad"]}),
        json!({"collections": ["*"], "operations": ["graph_read"], "graphs": ["g", "g"]}),
        json!({"collections": ["*"], "operations": ["graph_admin"], "graphs": ["g"]}),
    ] {
        let mut request = request;
        request["name"] = json!("agent");
        request["expires_unix_ms"] = json!(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64
                + 3_600_000
        );
        let (status, body) = call(
            &f.app,
            Some(ADMIN),
            "POST",
            "/v1/admin/credentials",
            Some(request.clone()),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{request}: {body}"
        );
    }
    // A binding may name a graph that does not exist yet (bindings are by
    // name; graph IDs are reused after a drop).
    credential(
        &f.app,
        json!({"collections": ["*"], "operations": ["graph_read"], "graphs": ["later"]}),
    )
    .await;
}

#[tokio::test]
async fn batches_touching_vectors_need_collection_write_scope_and_quota() {
    let f = fixture(true).await;
    let app = &f.app;
    let graph_only = credential(
        app,
        json!({"collections": ["default"], "operations": ["graph_write"], "graphs": ["g"]}),
    )
    .await;
    let other_collection = credential(
        app,
        json!({"collections": ["*"], "operations": ["graph_write", "read"], "graphs": ["g"]}),
    )
    .await;
    let quota = credential(
        app,
        json!({"collections": ["default"], "operations": ["graph_write", "write"],
            "graphs": ["g"], "max_vectors_per_collection": 4}),
    )
    .await;
    let attach = json!({"ops": [
        {"op": "create_node", "key": "x"},
        {"op": "detach", "node": "n4", "collection": "default"},
        {"op": "attach", "node": "x", "collection": "default", "record": "4"},
    ]});
    let put = json!({"ops": [
        {"op": "put_vector", "collection": "default", "id": "9", "vector": [9.0, 1.0]},
    ]});
    let missing = json!({"ops": [
        {"op": "put_vector", "collection": "nope", "id": "9", "vector": [9.0, 1.0]},
    ]});
    for token in [&graph_only, &other_collection] {
        for body in [&attach, &put, &missing] {
            let (status, response) = call(
                app,
                Some(token),
                "POST",
                "/v1/graphs/g/batch",
                Some(body.clone()),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{body}: {response}");
            assert_eq!(code(&response), "forbidden_scope");
        }
    }
    // Pure graph ops need no collection scope.
    let (status, body) = call(
        app,
        Some(&graph_only),
        "POST",
        "/v1/graphs/g/batch",
        Some(json!({"ops": [{"op": "create_node", "key": "y"}]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(
        app,
        Some(&quota),
        "POST",
        "/v1/graphs/g/batch",
        Some(attach),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(app, Some(&quota), "POST", "/v1/graphs/g/batch", Some(put)).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(code(&body), "quota_exceeded");
    let (status, body) = call(
        app,
        Some(&quota),
        "POST",
        "/v1/graphs/g/batch",
        Some(missing),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    // Nothing from the refused batches was written.
    let (_, body) = call(app, Some(ADMIN), "GET", "/v1/vectors/9", None).await;
    assert_eq!(code(&body), "not_found");
}

#[tokio::test]
async fn limits_bodies_and_malformed_input_are_explicit() {
    let f = fixture(true).await;
    let app = &f.app;
    let traverse = "/v1/graphs/g/traverse";
    let cases = [
        (
            json!({"start": [{"key": "n1"}], "limits": {"max_hops": 5}}),
            422,
            "graph_limit_exceeds_ceiling",
        ),
        (
            json!({"start": [{"key": "n1"}], "limits": {"deadline_ms": 5001}}),
            422,
            "graph_limit_exceeds_ceiling",
        ),
        (
            json!({"start": [{"key": "n1"}], "limits": {"max_visited_nodes": 0}}),
            422,
            "invalid_input",
        ),
        (
            json!({"start": [{"key": "n1"}], "graph": "g"}),
            422,
            "invalid_json",
        ),
        (
            json!({"start": [{"key": "n1"}], "limits": {"hops": 1}}),
            422,
            "invalid_json",
        ),
        (json!({"start": [{"id": "01"}]}), 422, "invalid_json"),
        (
            json!({"start": [{"id": "18446744073709551616"}]}),
            422,
            "invalid_json",
        ),
        (json!({"start": [{"id": -1}]}), 422, "invalid_json"),
        (json!({"start": []}), 422, "invalid_input"),
    ];
    for (body, status, expected) in cases {
        let (actual, response) = call(app, Some(ADMIN), "POST", traverse, Some(body.clone())).await;
        assert_eq!(actual.as_u16(), status, "{body}: {response}");
        assert_eq!(code(&response), expected, "{body}: {response}");
    }
    let (status, body) = call(
        app,
        Some(ADMIN),
        "POST",
        "/v1/graphs/g/nodes/find",
        Some(json!({"cursor": "ZZ"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(code(&body), "invalid_input");
    let (status, body) = send(app, Some(ADMIN), "POST", traverse, Body::from("{not json")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(code(&body), "invalid_json");

    // Read bodies are capped at 1 MiB; batches keep the server-wide cap.
    let padding = "x".repeat(1024 * 1024);
    let (status, body) = call(
        app,
        Some(ADMIN),
        "POST",
        traverse,
        Some(json!({"start": [{"key": padding}]})),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    let ops: Vec<Value> = (0..4097)
        .map(|i| json!({"op": "upsert_node", "key": format!("k{i}")}))
        .collect();
    let (status, body) = call(
        app,
        Some(ADMIN),
        "POST",
        "/v1/graphs/g/batch",
        Some(json!({"ops": ops})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(code(&body), "invalid_input");
    let (status, body) = call(
        app,
        Some(ADMIN),
        "POST",
        "/v1/graphs/g/batch",
        Some(json!({"ops": [{"op": "drop_everything"}]})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, _) = call(app, Some(ADMIN), "PUT", "/v1/graphs/g/batch", None).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_readers_and_writers_share_the_owner_and_hold_time_is_recorded() {
    let f = fixture(true).await;
    // A 2,000-node star plus chain so traversals examine real adjacency.
    let mut ops = vec![json!({"op": "create_node", "key": "hub"})];
    for i in 0..2_000 {
        ops.push(json!({"op": "create_node", "key": format!("s{i}"), "labels": ["Spoke"]}));
        ops.push(
            json!({"op": "create_edge", "source": "hub", "relationship": "HAS",
            "destination": format!("s{i}")}),
        );
        if i > 0 {
            ops.push(json!({"op": "create_edge", "source": format!("s{}", i - 1),
                "relationship": "NEXT", "destination": format!("s{i}")}));
        }
    }
    let (status, body) = call(
        &f.app,
        Some(ADMIN),
        "POST",
        "/v1/graphs/g/batch",
        Some(json!({"ops": ops[..3000].to_vec()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(
        &f.app,
        Some(ADMIN),
        "POST",
        "/v1/graphs/g/batch",
        Some(json!({"ops": ops[3000..].to_vec()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let mut tasks = Vec::new();
    for reader in 0..8 {
        let app = f.app.clone();
        tasks.push(tokio::spawn(async move {
            for _ in 0..25 {
                let (status, body) = call(
                    &app,
                    Some(READ_ONLY),
                    "POST",
                    "/v1/graphs/g/traverse",
                    Some(json!({"start": [{"key": "hub"}], "limits": {"max_hops": 2,
                        "max_output_nodes": 1000, "max_output_edges": 2000}})),
                )
                .await;
                assert!(
                    status == StatusCode::OK || status == StatusCode::SERVICE_UNAVAILABLE,
                    "reader {reader}: {body}"
                );
                if status == StatusCode::OK {
                    // One view: a writer's batch is never half visible.
                    let keys = body["nodes"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|n| n["node"]["key"].as_str().unwrap().starts_with("w"))
                        .count();
                    assert_eq!(keys, 0, "writer nodes are never edges of hub");
                }
            }
        }));
    }
    for writer in 0..2 {
        let app = f.app.clone();
        tasks.push(tokio::spawn(async move {
            let mut written = 0;
            while written < 10 {
                let ops: Vec<Value> = (0..10)
                    .map(
                        |i| json!({"op": "create_node", "key": format!("w{writer}-{written}-{i}")}),
                    )
                    .collect();
                let (status, body) = call(
                    &app,
                    Some(ADMIN),
                    "POST",
                    "/v1/graphs/g/batch",
                    Some(json!({"ops": ops})),
                )
                .await;
                match status {
                    StatusCode::OK => written += 1,
                    StatusCode::SERVICE_UNAVAILABLE => tokio::task::yield_now().await,
                    _ => panic!("writer {writer}: {body}"),
                }
            }
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let (_, body) = call(&f.app, Some(ADMIN), "GET", "/v1/graphs/g", None).await;
    assert_eq!(body["graph"]["node_count"], 4 + 1 + 2_000 + 200);

    let (status, stats) = call(&f.app, Some(ADMIN), "GET", "/v1/admin/graph/stats", None).await;
    assert_eq!(status, StatusCode::OK, "{stats}");
    let read = &stats["read_guard"];
    let samples = read["samples"].as_u64().unwrap();
    assert!(samples >= 100, "{stats}");
    let (p50, p99, max) = (
        read["p50_us"].as_u64().unwrap(),
        read["p99_us"].as_u64().unwrap(),
        read["max_us"].as_u64().unwrap(),
    );
    assert!(p50 <= p99 && p99 <= max, "{stats}");
    // The engine deadline bounds the expansion (default 250 ms); the guard
    // is held only for the read itself.
    assert!(p99 < 2_000_000, "read guard p99 {p99} us");
    assert!(stats["write_guard"]["samples"].as_u64().unwrap() >= 20);
    assert!(stats["requests"]["read"].as_u64().unwrap() >= samples);
    eprintln!(
        "TEST-012 read-guard hold time over {samples} graph reads: p50 {p50} us, p99 {p99} us, max {max} us; write guard {}",
        stats["write_guard"]
    );
}

#[tokio::test]
async fn served_openapi_is_the_base_document_plus_the_graph_fragment() {
    let f = fixture(true).await;
    let response = f
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/openapi.json")
                .header(header::AUTHORIZATION, format!("Bearer {ADMIN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let served = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    let base_text = include_str!("../src/openapi.json");
    let fragment: Value = serde_json::from_str(include_str!("../src/openapi_graph.json")).unwrap();
    let document: Value = serde_json::from_str(&served).expect("merged document parses");
    let base: Value = serde_json::from_str(base_text).unwrap();

    // Removing the inserted whole lines gives back the base bytes exactly.
    let graph_line = |line: &str| {
        line.starts_with("    \"/v1/graphs") || line.starts_with("    \"/v1/admin/graph/")
    };
    let stripped: String = served
        .split_inclusive('\n')
        .filter(|line| !graph_line(line))
        .collect();
    assert_eq!(stripped, base_text, "existing bytes are untouched");
    assert!(
        base_text
            .split_inclusive('\n')
            .all(|line| !graph_line(line))
    );

    // Every base path is byte-identical in value; every other top-level key too.
    for (key, value) in base.as_object().unwrap() {
        if key != "paths" {
            assert_eq!(&document[key], value, "{key}");
        }
    }
    let paths = document["paths"].as_object().unwrap();
    for (path, item) in base["paths"].as_object().unwrap() {
        assert_eq!(&paths[path], item, "{path}");
    }
    let fragment_paths = fragment["paths"].as_object().unwrap();
    assert_eq!(
        fragment.as_object().unwrap().len(),
        1,
        "fragment has only paths"
    );
    assert_eq!(
        paths.len(),
        base["paths"].as_object().unwrap().len() + fragment_paths.len(),
        "no fragment path collides with a base path"
    );
    for (path, item) in fragment_paths {
        assert_eq!(&paths[path], item, "{path}");
    }

    // Every graph route is documented with its method.
    let mut documented = vec![("GET", "/v1/admin/graph/stats".to_owned())];
    for (method, uri, _, _) in routes() {
        let template = match uri {
            "/v1/graphs" => uri.to_owned(),
            "/v1/graphs/empty" => "/v1/graphs/{graph}".to_owned(),
            _ => uri.replacen("/v1/graphs/g", "/v1/graphs/{graph}", 1),
        };
        documented.push((method, template));
    }
    for (method, template) in &documented {
        let operation = &paths
            .get(template)
            .unwrap_or_else(|| panic!("{template} is undocumented"))[method.to_lowercase()];
        assert!(
            operation["responses"].is_object(),
            "{method} {template} is undocumented"
        );
    }
    let mut operations = 0;
    for (path, item) in fragment_paths {
        assert!(path.starts_with("/v1/graphs") || path.starts_with("/v1/admin/graph/"));
        let expected: Vec<&str> = path
            .split('/')
            .filter_map(|part| part.strip_prefix('{')?.strip_suffix('}'))
            .collect();
        for (method, operation) in item.as_object().unwrap() {
            if method == "parameters" {
                continue;
            }
            operations += 1;
            assert!(
                documented
                    .iter()
                    .any(|(m, t)| m.eq_ignore_ascii_case(method) && t == path),
                "{method} {path} is documented but not served"
            );
            assert!(operation["responses"].is_object(), "{method} {path}");
            let declared: Vec<&str> = item["parameters"]
                .as_array()
                .into_iter()
                .flatten()
                .chain(operation["parameters"].as_array().into_iter().flatten())
                .filter(|p| p["in"] == "path")
                .inspect(|p| assert_eq!(p["required"], true, "{method} {path}"))
                .map(|p| p["name"].as_str().unwrap())
                .collect();
            assert_eq!(declared, expected, "{method} {path} path parameters");
        }
    }
    assert_eq!(operations, documented.len());

    // Every reference resolves inside the merged document.
    fn refs<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
        match value {
            Value::Object(map) => {
                if let Some(Value::String(reference)) = map.get("$ref") {
                    out.push(reference);
                }
                map.values().for_each(|v| refs(v, out));
            }
            Value::Array(items) => items.iter().for_each(|v| refs(v, out)),
            _ => {}
        }
    }
    let mut found = Vec::new();
    refs(&fragment, &mut found);
    for reference in found {
        let pointer = reference.strip_prefix('#').expect("bundled reference");
        assert!(document.pointer(pointer).is_some(), "{reference} resolves");
    }
}

#[tokio::test]
async fn rest_reads_match_the_graph_cli_on_the_same_file() {
    let f = fixture(true).await;
    let requests = [
        (
            "get-node",
            "/v1/graphs/g/nodes/get",
            json!({"node": {"key": "n2"}}),
        ),
        (
            "traverse",
            "/v1/graphs/g/traverse",
            json!({"start": [{"key": "n1"}], "include_paths": true}),
        ),
        (
            "shortest-path",
            "/v1/graphs/g/shortest-path",
            json!({"source": {"key": "n1"}, "destination": {"key": "n3"}}),
        ),
    ];
    let mut rest = Vec::new();
    for (_, uri, body) in &requests {
        let (status, response) = call(&f.app, Some(ADMIN), "POST", uri, Some(body.clone())).await;
        assert_eq!(status, StatusCode::OK, "{response}");
        rest.push(response);
    }
    let (_, explained) = call(
        &f.app,
        Some(ADMIN),
        "POST",
        "/v1/graphs/g/explain",
        Some(json!({"operation": "traverse", "start": [{"key": "n1"}]})),
    )
    .await;
    // The server is the single owner; release the file before the CLI opens it.
    f.service.close().await.unwrap();
    let cli = |args: &[&str], request: &Value| -> Value {
        let mut request = request.clone();
        request["graph"] = json!("g");
        let output = Command::new(env!("CARGO_BIN_EXE_vlite-graph-cli"))
            .arg("--db")
            .arg(&f.path)
            .args(args)
            .arg(request.to_string())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };
    let mask = |mut value: Value| {
        value["stats"]["elapsed_ms"] = json!(0);
        value
    };
    for ((command, _, body), rest) in requests.iter().zip(rest) {
        assert_eq!(mask(cli(&[command], body)), mask(rest), "{command}");
    }
    assert_eq!(
        cli(
            &["--explain", "traverse"],
            &json!({"start": [{"key": "n1"}]})
        ),
        explained
    );
}
