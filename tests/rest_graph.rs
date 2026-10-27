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
    Database, FormatVersion,
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
        ("GET", "/v1/graphs/g/schema", None, true),
        (
            "POST",
            "/v1/graphs/g/retrieve",
            Some(json!({"pivot": {"collection": "default", "vector": [1.0, 1.0], "k": 2}})),
            true,
        ),
        (
            "POST",
            "/v1/graphs/g/query",
            Some(json!({"query": "MATCH (n:Doc) RETURN n LIMIT 2"})),
            true,
        ),
        (
            "POST",
            "/v1/graphs/g/batch",
            Some(json!({"ops": [{"op": "upsert_node", "key": "n9"}]})),
            false,
        ),
        (
            "PUT",
            "/v1/graphs/g/schema",
            Some(json!({"schema": {}})),
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
        Database::create(path, 2).unwrap()
    } else {
        // New files are graph-enabled; an explicit v4 stays vector-only.
        Database::create_with_format(path, 2, FormatVersion::V4).unwrap()
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
    // Retrieve returns vector records, so readers also hold collection `read`
    // (see `retrieve_needs_collection_read_and_redacts_evidence`).
    let reader_g = credential(
        app,
        json!({"collections": ["*"], "operations": ["graph_read", "read"], "graphs": ["g"]}),
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
        if uri == "/v1/graphs/g/batch" || (method == "PUT" && uri == "/v1/graphs/g/schema") {
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

/// ITEM-041 / REQ-043: retrieve returns vector records, so a scoped caller
/// needs collection `read` on the pivot collection (403 otherwise), and
/// evidence from collections outside its binding is dropped and counted.
#[tokio::test]
async fn retrieve_needs_collection_read_and_redacts_evidence() {
    let f = fixture(true).await;
    let app = &f.app;
    let (status, body) = call(
        app,
        Some(ADMIN),
        "POST",
        "/v1/collections",
        Some(json!({"name": "secret", "dimensions": 2})),
    )
    .await;
    assert!(status.is_success(), "{body}");
    let (status, body) = call(
        app,
        Some(ADMIN),
        "POST",
        "/v1/graphs/g/batch",
        Some(json!({"ops": [
            {"op": "put_vector", "collection": "secret", "id": "1", "vector": [1.0, 1.0],
                "payload": "classified"},
            {"op": "attach", "node": "n2", "collection": "secret", "record": "1"},
        ]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let request = json!({
        "pivot": {"collection": "default", "vector": [1.0, 1.0], "k": 1},
        "expansion": {"direction": "out"},
        "evidence": {"collections": ["default", "secret"]},
        "limits": {"max_hops": 2},
    });
    let retrieve = |token: String| {
        let request = request.clone();
        async move {
            call(
                app,
                Some(&token),
                "POST",
                "/v1/graphs/g/retrieve",
                Some(request),
            )
            .await
        }
    };
    let (status, full) = retrieve(ADMIN.to_owned()).await;
    assert_eq!(status, StatusCode::OK, "{full}");
    assert_eq!(full["pivots"][0]["node_key"], "n1", "{full}");
    let keys: Vec<&str> = full["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["node"]["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["n1", "n2", "n3"], "{full}");
    let secret: Vec<&Value> = full["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["collection"] == "secret")
        .collect();
    assert_eq!(secret.len(), 1, "{full}");
    assert_eq!(secret[0]["payload_excerpt"], "classified");
    assert_eq!(full["redacted"], json!({"count": 0, "collections": []}));
    assert!(
        !full.to_string().contains("\"vector\""),
        "no vectors: {full}"
    );
    let (status, read_only) = retrieve(READ_ONLY.to_owned()).await;
    assert_eq!(status, StatusCode::OK);
    let mask = |mut value: Value| {
        value["stats"]["pivot_ms"] = json!(0);
        value["stats"]["expand_ms"] = json!(0);
        value["stats"]["expansion"]["elapsed_ms"] = json!(0);
        value
    };
    assert_eq!(mask(read_only), mask(full.clone()));

    // Bound to "default" only: the secret evidence is redacted, the graph
    // structure is not.
    let default_reader = credential(
        app,
        json!({"collections": ["default"], "operations": ["graph_read", "read"],
            "graphs": ["g"]}),
    )
    .await;
    let (status, redacted) = retrieve(default_reader).await;
    assert_eq!(status, StatusCode::OK, "{redacted}");
    assert_eq!(
        redacted["redacted"],
        json!({"count": 1, "collections": ["secret"]})
    );
    assert!(!redacted.to_string().contains("classified"), "{redacted}");
    let evidence = redacted["evidence"].as_array().unwrap();
    assert_eq!(
        evidence.len(),
        full["evidence"].as_array().unwrap().len() - 1
    );
    assert!(evidence.iter().all(|item| item["collection"] == "default"));
    assert_eq!(redacted["stats"]["evidence_items"], evidence.len());
    for field in ["pivots", "nodes", "edges", "paths", "complete"] {
        assert_eq!(redacted[field], full[field], "{field}");
    }

    // graph_read alone (no collection `read`) and a binding without the
    // pivot collection are refused; graph-only routes still answer.
    let graph_only = credential(
        app,
        json!({"collections": ["*"], "operations": ["graph_read"], "graphs": ["g"]}),
    )
    .await;
    let secret_reader = credential(
        app,
        json!({"collections": ["secret"], "operations": ["graph_read", "read"],
            "graphs": ["g"]}),
    )
    .await;
    for token in [&graph_only, &secret_reader] {
        let (status, body) = retrieve(token.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(code(&body), "forbidden_scope");
        let (status, body) = call(
            app,
            Some(token),
            "POST",
            "/v1/graphs/g/query",
            Some(json!({"query": "MATCH (n KEY \"n1\") RETURN n"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
}

/// AC-018 / ITEM-052 over REST: schema discovery and declaration, the query
/// text, and their errors.
#[tokio::test]
async fn schema_retrieve_and_query_text_routes() {
    let f = fixture(true).await;
    let app = &f.app;
    let (status, body) = call(app, Some(ADMIN), "GET", "/v1/graphs/g/schema", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["graph"], "g");
    assert_eq!(body["declared"], json!({"labels": {}, "relationships": {}}));
    assert_eq!(body["observed"]["complete"], true);
    assert_eq!(body["observed"]["labels"][0]["name"], "Doc");
    assert_eq!(body["observed"]["labels"][0]["count"], 4);
    assert_eq!(body["observed"]["relationships"][0]["name"], "LINKS");
    let (status, body) = call(
        app,
        Some(ADMIN),
        "GET",
        "/v1/graphs/g/schema?budget=1",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["observed"]["complete"], false, "{body}");
    for uri in [
        "/v1/graphs/g/schema?budget=x",
        "/v1/graphs/g/schema?other=1",
    ] {
        let (status, body) = call(app, Some(ADMIN), "GET", uri, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");
        assert_eq!(code(&body), "invalid_query");
    }

    // Declaring a schema the data violates is refused; a matching one is
    // stored and enables ensure_edge.
    let schema = json!({
        "labels": {"Doc": {"description": "A document", "properties": {
            "rank": {"type": "integer", "required": true, "index": "range"}}}},
        "relationships": {"LINKS": {"description": "Doc links",
            "endpoints": [{"source": "Doc", "destination": "Doc"}],
            "multiplicity": "unique_triple"}},
    });
    let mut bad = schema.clone();
    bad["labels"]["Doc"]["properties"]["title"] = json!({"type": "string", "required": true});
    let (status, body) = call(
        app,
        Some(ADMIN),
        "PUT",
        "/v1/graphs/g/schema",
        Some(json!({"schema": bad})),
    )
    .await;
    assert!(status.is_client_error(), "{body}");
    let (status, body) = call(
        app,
        Some(ADMIN),
        "PUT",
        "/v1/graphs/g/schema",
        Some(json!({"schema": schema})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["graph"], "g");
    assert!(body["revision"].is_string(), "{body}");
    let (_, body) = call(app, Some(ADMIN), "GET", "/v1/graphs/g/schema", None).await;
    assert_eq!(
        body["declared"]["labels"]["Doc"]["properties"]["rank"]["index"], "range",
        "{body}"
    );
    assert_eq!(body["observed"]["labels"][0]["origin"], "both", "{body}");
    // Declared property indexes appear in read stats.
    let (status, body) = call(
        app,
        Some(ADMIN),
        "POST",
        "/v1/graphs/g/nodes/find",
        Some(
            json!({"label": "Doc", "conditions": [{"field": "rank", "op": "range",
            "min": {"type": "integer", "value": "2"}}], "limit": 10}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["stats"]["indexes"],
        json!(["property_range_index"]),
        "{body}"
    );
    let ensure = json!({"ops": [{"op": "ensure_edge", "source": "n1",
        "relationship": "LINKS", "destination": "n2"}]});
    for _ in 0..2 {
        let (status, body) = call(
            app,
            Some(ADMIN),
            "POST",
            "/v1/graphs/g/batch",
            Some(ensure.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (_, body) = call(app, Some(ADMIN), "GET", "/v1/graphs/g", None).await;
    assert_eq!(body["graph"]["edge_count"], 2, "ensure_edge is idempotent");
    let (status, body) = call(
        app,
        Some(ADMIN),
        "POST",
        "/v1/graphs/g/batch",
        Some(json!({"ops": [{"op": "put_schema", "schema": {}}]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["outcomes"], json!(["schema"]));

    // Query text equals the typed route; explain equals typed explain.
    let pairs = [
        (
            json!({"query": "MATCH (n:Doc) WHERE n.rank >= $min RETURN n LIMIT 10",
                "params": {"min": {"type": "integer", "value": "2"}}}),
            "/v1/graphs/g/nodes/find",
            json!({"label": "Doc", "conditions": [{"field": "rank", "op": "range",
                "min": {"type": "integer", "value": "2"}}], "limit": 10}),
        ),
        (
            json!({"query": "MATCH p = (a KEY \"n1\")-[:LINKS*1..2]->(b) RETURN b, p"}),
            "/v1/graphs/g/traverse",
            json!({"start": [{"key": "n1"}], "types": ["LINKS"], "include_paths": true,
                "limits": {"max_hops": 2}}),
        ),
    ];
    for (text, typed_uri, typed) in pairs {
        let (status, by_text) = call(
            app,
            Some(READ_ONLY),
            "POST",
            "/v1/graphs/g/query",
            Some(text.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{text}: {by_text}");
        let (status, by_type) = call(app, Some(READ_ONLY), "POST", typed_uri, Some(typed)).await;
        assert_eq!(status, StatusCode::OK, "{by_type}");
        let mask = |mut value: Value| {
            value["stats"]["elapsed_ms"] = json!(0);
            value
        };
        assert_eq!(mask(by_text), mask(by_type), "{text}");
        let mut explain = text.clone();
        explain["explain"] = json!(true);
        let (status, plan) = call(
            app,
            Some(READ_ONLY),
            "POST",
            "/v1/graphs/g/query",
            Some(explain),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{plan}");
        assert!(plan["explain"]["query"].is_object(), "{plan}");
    }
    let errors = [
        (
            "/v1/graphs/g/query",
            json!({"query": "MATCH (n) DELETE n"}),
            "graph_query_unsupported",
        ),
        (
            "/v1/graphs/g/query",
            json!({"query": "MATCH (n KEY $k) RETURN n"}),
            "graph_query_parameter",
        ),
        (
            "/v1/graphs/g/query",
            json!({"query": "MATCH (n RETURN n"}),
            "graph_query_syntax",
        ),
        (
            "/v1/graphs/g/query",
            json!({"query": "MATCH (n) RETURN n", "graph": "g"}),
            "invalid_json",
        ),
        (
            "/v1/graphs/g/retrieve",
            json!({"pivot": {"collection": "default", "vector": [1.0, 1.0], "k": 101}}),
            "graph_limit_exceeds_ceiling",
        ),
        (
            "/v1/graphs/g/retrieve",
            json!({"pivot": {"collection": "default"}}),
            "invalid_input",
        ),
        (
            "/v1/graphs/g/retrieve",
            json!({"pivot": {"collection": "default", "vector": [1.0, 1.0]}, "extra": 1}),
            "invalid_json",
        ),
    ];
    for (uri, body, expected) in errors {
        let (status, response) = call(app, Some(ADMIN), "POST", uri, Some(body.clone())).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{body}: {response}"
        );
        assert_eq!(code(&response), expected, "{body}: {response}");
    }
    // Query text errors come before the graph lookup; graph errors after.
    let (status, body) = call(
        app,
        Some(ADMIN),
        "POST",
        "/v1/graphs/missing/query",
        Some(json!({"query": "MATCH (n:Doc) RETURN n"})),
    )
    .await;
    assert!(status.is_client_error(), "{body}");
    assert_eq!(code(&body), "graph_not_found");
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

/// GATE-007: `delete_node` removes the node's attachments, so a scoped
/// credential needs collection `write` for every attached collection.
#[tokio::test]
async fn deleting_an_attached_node_needs_collection_write_scope() {
    let f = fixture(true).await;
    let app = &f.app;
    let graph_only = credential(
        app,
        json!({"collections": ["default"], "operations": ["graph_write"], "graphs": ["g"]}),
    )
    .await;
    let read_only = credential(
        app,
        json!({"collections": ["*"], "operations": ["graph_write", "read"], "graphs": ["g"]}),
    )
    .await;
    let writer = credential(
        app,
        json!({"collections": ["default"], "operations": ["graph_write", "write"],
            "graphs": ["g"]}),
    )
    .await;
    // n4 has a record of "default" attached and no incident edges.
    let delete = json!({"ops": [{"op": "delete_node", "key": "n4"}]});
    for token in [&graph_only, &read_only] {
        let (status, body) = call(
            app,
            Some(token),
            "POST",
            "/v1/graphs/g/batch",
            Some(delete.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(code(&body), "forbidden_scope");
    }
    let (status, body) = call(
        app,
        Some(ADMIN),
        "POST",
        "/v1/graphs/g/nodes/get",
        Some(json!({"node": {"key": "n4"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body["node"].is_null(), "refused delete wrote: {body}");
    // A node without attachments needs no collection scope.
    for ops in [
        json!([{"op": "create_node", "key": "z"}]),
        json!([{"op": "delete_node", "key": "z"}]),
    ] {
        let (status, body) = call(
            app,
            Some(&graph_only),
            "POST",
            "/v1/graphs/g/batch",
            Some(json!({"ops": ops})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (status, body) = call(
        app,
        Some(&writer),
        "POST",
        "/v1/graphs/g/batch",
        Some(delete),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
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

// Signed-in Dashboard accounts and tenant credentials (v1.2.20, superseding
// the 2026-10-25 interim rule): graph names are tenant-qualified like
// collections. A tenanted principal's `g` is the stored graph `t{tenant}__g`
// and responses show `g`; untenanted principals and the admin token see
// stored names. Viewers read and editors read and write the graphs in their
// scope, narrowed by the account's `graphs` and `graph_access` knobs. Batches
// and retrieve touching vector records still check collections by exact
// stored name.

const PASSWORD: &str = "correct-horse-battery-staple";

async fn browser(
    app: &Router,
    session: &str,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::COOKIE, format!("vlite_session={session}"))
        .header("sec-fetch-site", "same-origin")
        .header("x-vlite-request", "1")
        .header(header::CONTENT_TYPE, "application/json");
    if body.is_none() {
        request = request.header(header::CONTENT_LENGTH, "0");
    }
    let body = body.map_or_else(Body::empty, |value| Body::from(value.to_string()));
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert_ne!(status, StatusCode::INTERNAL_SERVER_ERROR, "{uri}: {value}");
    (status, value)
}

async fn account_fixture() -> (tempfile::TempDir, Router) {
    let directory = tempdir().unwrap();
    std::fs::create_dir(directory.path().join("admin")).unwrap();
    let service = Service::new(database(&directory.path().join("graph.vlite"), true));
    let options = ServerOptions {
        loopback_only: true,
        ..options(directory.path())
    };
    let app = configured_admin_router(service, options).unwrap();
    let (status, body) = call(
        &app,
        Some(ADMIN),
        "POST",
        "/v1/graphs",
        Some(json!({"graph": "g"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let ops = json!({"ops": [
        {"op": "create_node", "key": "n1"},
        {"op": "create_node", "key": "n4"},
        {"op": "attach", "node": "n4", "collection": "default", "record": "4"},
    ]});
    let (status, body) = call(&app, Some(ADMIN), "POST", "/v1/graphs/g/batch", Some(ops)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    (directory, app)
}

async fn tenant(app: &Router, name: &str) -> String {
    let (status, body) = call(
        app,
        Some(ADMIN),
        "POST",
        "/v1/admin/tenants",
        Some(json!({ "name": name })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["tenant"]["id"].as_str().unwrap().to_owned()
}

/// Creates an account and returns a signed-in session token.
async fn sign_in(app: &Router, username: &str, role: &str, tenant: Option<&str>) -> String {
    sign_in_with(app, username, role, tenant, json!({})).await
}

/// As `sign_in`, with extra account fields (the graph knobs).
async fn sign_in_with(
    app: &Router,
    username: &str,
    role: &str,
    tenant: Option<&str>,
    extra: Value,
) -> String {
    let mut account = json!({"username": username, "password": PASSWORD, "role": role,
        "tenant_id": tenant, "must_change_password": false});
    for (key, value) in extra.as_object().unwrap() {
        account[key] = value.clone();
    }
    let (status, body) = call(app, Some(ADMIN), "POST", "/v1/admin/users", Some(account)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/auth/login")
                .header("sec-fetch-site", "same-origin")
                .header("x-vlite-request", "1")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({"username": username, "password": PASSWORD}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
    let (pair, _) = cookie.split_once(';').unwrap();
    pair.split_once('=').unwrap().1.to_owned()
}

/// A tenanted editor's collection holding record 1; returns its stored name.
async fn tenant_collection(app: &Router, session: &str, tenant: &str) -> String {
    let (status, body) = browser(
        app,
        session,
        "POST",
        "/v1/collections",
        Some(json!({"name": "docs", "dimensions": 2})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = browser(
        app,
        session,
        "PUT",
        "/v1/collections/docs/vectors/1",
        Some(json!({"vector": [1.0, 0.0]})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    format!("t{tenant}__docs")
}

fn graph_names(body: &Value) -> Vec<String> {
    body["graphs"]
        .as_array()
        .unwrap_or_else(|| panic!("no graph list: {body}"))
        .iter()
        .map(|graph| graph["name"].as_str().unwrap().to_owned())
        .collect()
}

/// Admin-token batch, by stored graph name.
async fn admin_batch(app: &Router, graph: &str, ops: Value) {
    let uri = format!("/v1/graphs/{graph}/batch");
    let (status, body) = call(app, Some(ADMIN), "POST", &uri, Some(json!({ "ops": ops }))).await;
    assert_eq!(status, StatusCode::OK, "{graph}: {body}");
}

async fn admin_graph(app: &Router, graph: &str) {
    let (status, body) = call(
        app,
        Some(ADMIN),
        "POST",
        "/v1/graphs",
        Some(json!({ "graph": graph })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{graph}: {body}");
}

#[tokio::test]
async fn signed_in_accounts_get_graph_access_by_role() {
    let (_directory, app) = account_fixture().await;
    let app = &app;
    let acme = tenant(app, "acme").await;
    let viewer = sign_in(app, "vera", "viewer", None).await;
    let tenant_viewer = sign_in(app, "tess", "viewer", Some(&acme)).await;
    let editor = sign_in(app, "eddie", "editor", None).await;
    let root = sign_in(app, "root", "super_admin", None).await;

    // An untenanted viewer reads the untenanted graph.
    let (status, body) = browser(app, &viewer, "GET", "/v1/graphs", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(graph_names(&body), ["g"], "{body}");
    let (status, body) = browser(app, &viewer, "GET", "/v1/graphs/g", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let get = json!({"node": {"key": "n4"}});
    let (status, body) = browser(app, &viewer, "POST", "/v1/graphs/g/nodes/get", Some(get)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body["node"].is_null(), "{body}");
    // A tenanted viewer sees only its tenant's graphs: "g" is acme's own g,
    // which does not exist yet, not the untenanted one.
    let (status, body) = browser(app, &tenant_viewer, "GET", "/v1/graphs", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["graphs"], json!([]), "{body}");
    let (status, body) = browser(app, &tenant_viewer, "GET", "/v1/graphs/g", None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "graph_not_found");

    let write = json!({"ops": [{"op": "create_node", "key": "w"}]});
    for session in [&viewer, &tenant_viewer] {
        for (method, uri, input) in [
            ("POST", "/v1/graphs/g/batch", Some(write.clone())),
            ("POST", "/v1/graphs", Some(json!({"graph": "h"}))),
            ("DELETE", "/v1/graphs/g", None),
        ] {
            let (status, body) = browser(app, session, method, uri, input).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}: {body}");
            assert_eq!(code(&body), "forbidden_scope");
        }
        // The Dashboard session reports the graph scope.
        let (_, caps) = browser(app, session, "GET", "/v1/capabilities", None).await;
        assert_eq!(caps["scope"]["graphs"], json!(["*"]), "{caps}");
        let operations = caps["scope"]["operations"].as_array().unwrap();
        assert!(operations.contains(&json!("graph_read")), "{caps}");
        assert!(!operations.contains(&json!("graph_write")), "{caps}");
    }

    let (_, caps) = browser(app, &editor, "GET", "/v1/capabilities", None).await;
    let operations = caps["scope"]["operations"].as_array().unwrap();
    assert!(operations.contains(&json!("graph_read")), "{caps}");
    assert!(operations.contains(&json!("graph_write")), "{caps}");
    // An untenanted editor covers every untenanted collection, so it may
    // attach to "default"; super admins can do everything.
    for (session, graph, record) in [(&editor, "h", "1"), (&root, "k", "2")] {
        let (status, body) = browser(
            app,
            session,
            "POST",
            "/v1/graphs",
            Some(json!({ "graph": graph })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["graph"]["name"], graph, "{body}");
        let ops = json!({"ops": [
            {"op": "create_node", "key": "a"},
            {"op": "attach", "node": "a", "collection": "default", "record": record},
            {"op": "create_edge", "source": "a", "relationship": "R", "destination": "a"},
        ]});
        let uri = format!("/v1/graphs/{graph}/batch");
        let (status, body) = browser(app, session, "POST", &uri, Some(ops)).await;
        assert_eq!(status, StatusCode::OK, "{graph}: {body}");
        let ops = json!({"ops": [{"op": "upsert_node", "key": "n1"}]});
        let (status, body) = browser(app, session, "POST", "/v1/graphs/g/batch", Some(ops)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        // A graph must be empty before it is dropped.
        let empty = json!({"graph": format!("{graph}-empty")});
        let (status, body) = browser(app, session, "POST", "/v1/graphs", Some(empty)).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let uri = format!("/v1/graphs/{graph}-empty");
        let (status, body) = browser(app, session, "DELETE", &uri, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
}

/// Two tenants each create "g": they get separate stored graphs, list and
/// read only their own under the unqualified name, and cannot reach the
/// other's even by its stored name. The admin token sees stored names.
#[tokio::test]
async fn tenant_accounts_own_isolated_graphs_under_unqualified_names() {
    let (_directory, app) = account_fixture().await;
    let app = &app;
    let acme = tenant(app, "acme").await;
    let beta = tenant(app, "beta").await;
    let alice = sign_in(app, "alice", "editor", Some(&acme)).await;
    let tess = sign_in(app, "tess", "viewer", Some(&acme)).await;
    let bob = sign_in(app, "bob", "editor", Some(&beta)).await;
    let (stored_a, stored_b) = (format!("t{acme}__g"), format!("t{beta}__g"));

    for (session, key) in [(&alice, "a1"), (&bob, "b1")] {
        let (status, body) = browser(
            app,
            session,
            "POST",
            "/v1/graphs",
            Some(json!({"graph": "g"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["graph"]["name"], "g", "{body}");
        let ops = json!({"ops": [{"op": "create_node", "key": key}]});
        let (status, body) = browser(app, session, "POST", "/v1/graphs/g/batch", Some(ops)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["graph"], "g", "{body}");
        let (status, body) = browser(app, session, "GET", "/v1/graphs/g", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["graph"]["name"], "g", "{body}");
        assert_eq!(body["graph"]["node_count"], 1, "{body}");
    }

    let (status, body) = call(app, Some(ADMIN), "GET", "/v1/graphs", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let mut expected = vec!["g".to_owned(), stored_a.clone(), stored_b.clone()];
    expected.sort();
    assert_eq!(graph_names(&body), expected, "{body}");
    for session in [&alice, &tess, &bob] {
        let (status, body) = browser(app, session, "GET", "/v1/graphs", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(graph_names(&body), ["g"], "{body}");
        assert_eq!(body["graphs"][0]["node_count"], 1, "{body}");
    }
    for (session, mine, theirs) in [
        (&alice, "a1", "b1"),
        (&tess, "a1", "b1"),
        (&bob, "b1", "a1"),
    ] {
        for (key, found) in [(mine, true), (theirs, false), ("n4", false)] {
            let get = json!({"node": {"key": key}});
            let (status, body) =
                browser(app, session, "POST", "/v1/graphs/g/nodes/get", Some(get)).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(!body["node"].is_null(), found, "{key}: {body}");
        }
    }
    let uri = format!("/v1/graphs/{stored_a}/nodes/get");
    let get = json!({"node": {"key": "a1"}});
    let (status, body) = call(app, Some(ADMIN), "POST", &uri, Some(get)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body["node"].is_null(), "{body}");

    // A stored name is qualified again, so it never escapes the tenant.
    for graph in [&stored_b, &stored_a] {
        for (method, uri, input) in [
            ("GET", format!("/v1/graphs/{graph}"), None),
            (
                "POST",
                format!("/v1/graphs/{graph}/batch"),
                Some(json!({"ops": [{"op": "create_node", "key": "x"}]})),
            ),
        ] {
            let (status, body) = browser(app, &alice, method, &uri, input).await;
            assert!(
                [StatusCode::CONFLICT, StatusCode::UNPROCESSABLE_ENTITY].contains(&status),
                "{uri}: {status} {body}"
            );
        }
    }
    // Viewers stay read-only inside their tenant.
    let ops = json!({"ops": [{"op": "create_node", "key": "t1"}]});
    let (status, _) = browser(app, &tess, "POST", "/v1/graphs/g/batch", Some(ops)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A tenant graph blocks deleting the tenant, like a tenant collection.
    let uri = format!("/v1/admin/tenants/{beta}");
    let (status, body) = call(app, Some(ADMIN), "DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "tenant_not_empty");
    // Dropping reports the name the caller used and leaves the others.
    let ops = json!({"ops": [{"op": "delete_node", "key": "b1"}]});
    let (status, body) = browser(app, &bob, "POST", "/v1/graphs/g/batch", Some(ops)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = browser(app, &bob, "DELETE", "/v1/graphs/g", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["dropped"], "g", "{body}");
    let (_, body) = call(app, Some(ADMIN), "GET", "/v1/graphs", None).await;
    assert_eq!(graph_names(&body), ["g".to_owned(), stored_a], "{body}");
    let (status, body) = call(app, Some(ADMIN), "DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn tenanted_editors_write_only_their_own_vectors() {
    let (_directory, app) = account_fixture().await;
    let app = &app;
    let acme = tenant(app, "acme").await;
    let beta = tenant(app, "beta").await;
    let alice = sign_in(app, "alice", "editor", Some(&acme)).await;
    let bob = sign_in(app, "bob", "editor", Some(&beta)).await;
    let mine = tenant_collection(app, &alice, &acme).await;
    let theirs = tenant_collection(app, &bob, &beta).await;
    for session in [&alice, &bob] {
        let (status, body) = browser(
            app,
            session,
            "POST",
            "/v1/graphs",
            Some(json!({"graph": "g"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let stored = format!("t{acme}__g");
    // The admin token attaches foreign records inside acme's graph.
    admin_batch(
        app,
        &stored,
        json!([
            {"op": "create_node", "key": "n1"},
            {"op": "create_node", "key": "n4"},
            {"op": "attach", "node": "n4", "collection": "default", "record": "3"},
            {"op": "create_node", "key": "b1"},
            {"op": "attach", "node": "b1", "collection": theirs, "record": "1"},
        ]),
    )
    .await;
    let batch = |session: &str, ops: Value| {
        let session = session.to_owned();
        async move {
            browser(
                app,
                &session,
                "POST",
                "/v1/graphs/g/batch",
                Some(json!({ "ops": ops })),
            )
            .await
        }
    };

    let (status, body) = batch(
        &alice,
        json!([
            {"op": "create_node", "key": "a1"},
            {"op": "create_edge", "source": "a1", "relationship": "R", "destination": "n1"},
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Vector records stay tenant-isolated: another tenant's collection, an
    // untenanted collection and an unqualified name are all refused, for
    // attach, detach and vector mutations alike.
    for collection in [theirs.as_str(), "default", "docs"] {
        for op in [
            json!({"op": "attach", "node": "a1", "collection": collection, "record": "1"}),
            json!({"op": "detach", "node": "n4", "collection": collection}),
            json!({"op": "put_vector", "collection": collection, "id": "9", "vector": [9.0, 1.0]}),
            json!({"op": "delete_vector", "collection": collection, "id": "1"}),
        ] {
            let (status, body) = batch(&alice, json!([op.clone()])).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{op}: {body}");
            assert_eq!(code(&body), "forbidden_scope", "{op}");
        }
    }
    // Its own collection, by stored name, works.
    let (status, body) = batch(
        &alice,
        json!([{"op": "attach", "node": "a1", "collection": mine, "record": "1"}]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = batch(
        &bob,
        json!([
            {"op": "create_node", "key": "b1"},
            {"op": "put_vector", "collection": theirs, "id": "2", "vector": [0.0, 1.0]},
            {"op": "attach", "node": "b1", "collection": theirs, "record": "2"},
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // delete_node drops attachments, so foreign attachments refuse it.
    for key in ["n4", "b1"] {
        let (status, body) = batch(&alice, json!([{"op": "delete_node", "key": key}])).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{key}: {body}");
        assert_eq!(code(&body), "forbidden_scope");
        let (status, body) = call(
            app,
            Some(ADMIN),
            "POST",
            &format!("/v1/graphs/{stored}/nodes/get"),
            Some(json!({"node": {"key": key}})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(!body["node"].is_null(), "refused delete wrote: {body}");
    }
    // Its own attachments do not block delete_node.
    let (status, body) = batch(
        &alice,
        json!([
            {"op": "detach", "node": "a1", "collection": mine},
            {"op": "create_node", "key": "a2"},
            {"op": "attach", "node": "a2", "collection": mine, "record": "1"},
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = batch(&alice, json!([{"op": "delete_node", "key": "a2"}])).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// Retrieve returns vector records, so tenanted accounts are held to the same
/// exact-name collection check as batches inside their own graph: another
/// tenant's (or an untenanted) pivot is refused, and such evidence is
/// redacted, not returned. Another tenant does not reach the graph at all.
#[tokio::test]
async fn tenanted_accounts_retrieve_only_their_own_evidence() {
    let (_directory, app) = account_fixture().await;
    let app = &app;
    let acme = tenant(app, "acme").await;
    let beta = tenant(app, "beta").await;
    let alice = sign_in(app, "alice", "editor", Some(&acme)).await;
    let tess = sign_in(app, "tess", "viewer", Some(&acme)).await;
    let bob = sign_in(app, "bob", "editor", Some(&beta)).await;
    let mine = tenant_collection(app, &alice, &acme).await;
    let theirs = tenant_collection(app, &bob, &beta).await;
    let stored = format!("t{acme}__g");
    admin_graph(app, &stored).await;
    admin_batch(
        app,
        &stored,
        json!([
            {"op": "create_node", "key": "n4"},
            {"op": "attach", "node": "n4", "collection": "default", "record": "3"},
            {"op": "create_node", "key": "a1"},
            {"op": "attach", "node": "a1", "collection": mine, "record": "1"},
            {"op": "create_node", "key": "b1"},
            {"op": "attach", "node": "b1", "collection": theirs, "record": "1"},
            {"op": "create_edge", "source": "a1", "relationship": "R", "destination": "b1"},
            {"op": "create_edge", "source": "a1", "relationship": "R", "destination": "n4"},
        ]),
    )
    .await;
    let request = |pivot: &str| {
        json!({
            "pivot": {"collection": pivot, "vector": [1.0, 0.0], "k": 1},
            "expansion": {"direction": "out"},
            "evidence": {"collections": [mine, theirs, "default"]},
            "limits": {"max_hops": 1},
        })
    };
    let uri = "/v1/graphs/g/retrieve";
    let admin_uri = format!("/v1/graphs/{stored}/retrieve");
    let (status, full) = call(app, Some(ADMIN), "POST", &admin_uri, Some(request(&mine))).await;
    assert_eq!(status, StatusCode::OK, "{full}");
    let collections = |body: &Value| -> Vec<String> {
        body["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["collection"].as_str().unwrap().to_owned())
            .collect()
    };
    let all = collections(&full);
    for name in [&mine, &theirs, &"default".to_owned()] {
        assert!(all.contains(name), "admin sees {name}: {full}");
    }

    for session in [&alice, &tess] {
        let (status, body) = browser(app, session, "POST", uri, Some(request(&mine))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(!body.to_string().contains(&stored), "{body}");
        assert!(collections(&body).iter().all(|c| *c == mine), "{body}");
        assert!(!collections(&body).is_empty(), "{body}");
        let mut foreign = vec!["default".to_owned(), theirs.clone()];
        foreign.sort();
        assert_eq!(body["redacted"]["collections"], json!(foreign), "{body}");
        assert_eq!(
            body["redacted"]["count"].as_u64().unwrap() as usize,
            all.len() - collections(&body).len(),
            "{body}"
        );
        for field in ["pivots", "nodes", "edges", "paths"] {
            assert_eq!(body[field], full[field], "{field}");
        }
        // Another tenant's, an untenanted and an unqualified pivot.
        for pivot in [theirs.as_str(), "default", "docs"] {
            let (status, body) = browser(app, session, "POST", uri, Some(request(pivot))).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{pivot}: {body}");
            assert_eq!(code(&body), "forbidden_scope", "{pivot}");
        }
    }
    // Bob's "g" is beta's graph, which does not exist: acme's stays hidden.
    let (status, body) = browser(app, &bob, "POST", uri, Some(request(&theirs))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "graph_not_found");
}

/// API credentials: a tenant credential's `graphs` binding names graphs
/// inside its tenant, and one without graph operations reaches no graph.
#[tokio::test]
async fn tenant_credentials_bind_graphs_inside_their_tenant() {
    let (_directory, app) = account_fixture().await;
    let app = &app;
    let acme = tenant(app, "acme").await;
    let beta = tenant(app, "beta").await;
    for graph in [
        format!("t{acme}__g"),
        format!("t{acme}__h"),
        format!("t{beta}__g"),
    ] {
        admin_graph(app, &graph).await;
    }
    let token = credential(
        app,
        json!({"collections": ["*"], "operations": ["graph_read", "graph_write"],
            "graphs": ["g"], "tenant_id": acme}),
    )
    .await;
    let token = Some(token.as_str());
    let (status, body) = call(app, token, "GET", "/v1/graphs", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(graph_names(&body), ["g"], "{body}");
    assert_eq!(body["graphs"][0]["node_count"], 0, "{body}");
    let (status, body) = call(app, token, "GET", "/v1/graphs/g", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["graph"]["name"], "g");
    let ops = json!({"ops": [{"op": "create_node", "key": "x"}]});
    let (status, body) = call(app, token, "POST", "/v1/graphs/g/batch", Some(ops)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, body) = call(app, Some(ADMIN), "GET", "/v1/graphs", None).await;
    for graph in body["graphs"].as_array().unwrap() {
        let written = graph["name"] == json!(format!("t{acme}__g"));
        assert_eq!(
            graph["node_count"],
            if written {
                1
            } else if graph["name"] == "g" {
                2
            } else {
                0
            },
            "{body}"
        );
    }
    for (method, uri, input) in [
        ("GET", "/v1/graphs/h", None),
        ("POST", "/v1/graphs", Some(json!({"graph": "k"}))),
    ] {
        let (status, body) = call(app, token, method, uri, input).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {body}");
        assert_eq!(code(&body), "forbidden_scope");
    }
    let every = credential(
        app,
        json!({"collections": ["*"], "operations": ["graph_read"], "graphs": ["*"],
            "tenant_id": acme}),
    )
    .await;
    let (_, body) = call(app, Some(&every), "GET", "/v1/graphs", None).await;
    assert_eq!(graph_names(&body), ["g", "h"], "{body}");

    // Tenant names must still be valid graph names once qualified.
    let long = "a".repeat(29);
    let mut request = json!({"name": "agent", "collections": ["*"],
        "operations": ["graph_read"], "graphs": [long], "tenant_id": acme,
        "expires_unix_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64 + 3_600_000});
    let (status, body) = call(
        app,
        Some(ADMIN),
        "POST",
        "/v1/admin/credentials",
        Some(request.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    request.as_object_mut().unwrap().remove("tenant_id");
    let (status, body) = call(
        app,
        Some(ADMIN),
        "POST",
        "/v1/admin/credentials",
        Some(request),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // Without graph operations a tenant credential reaches no graph.
    let vectors = credential(
        app,
        json!({"collections": ["*"], "operations": ["read", "write"], "tenant_id": acme}),
    )
    .await;
    for (method, uri) in [("GET", "/v1/graphs/g"), ("POST", "/v1/graphs/g/batch")] {
        let (status, body) = call(app, Some(&vectors), method, uri, Some(json!({"ops": []}))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {body}");
    }
}

/// Per-account knobs narrow the role: `graphs: []` removes graph access, a
/// name list restricts it, and `graph_access` caps the operations.
#[tokio::test]
async fn account_graph_knobs_narrow_access() {
    let (_directory, app) = account_fixture().await;
    let app = &app;
    let acme = tenant(app, "acme").await;
    admin_graph(app, &format!("t{acme}__g")).await;
    admin_graph(app, &format!("t{acme}__h")).await;
    let operations = |caps: &Value| -> Vec<String> {
        caps["scope"]["operations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|op| op.as_str().unwrap().to_owned())
            .filter(|op| op.starts_with("graph_"))
            .collect()
    };
    let write = || Some(json!({"ops": [{"op": "create_node", "key": "w"}]}));

    let none = sign_in_with(app, "nora", "editor", Some(&acme), json!({"graphs": []})).await;
    let (status, body) = browser(app, &none, "GET", "/v1/graphs", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(code(&body), "forbidden_scope");
    let (_, caps) = browser(app, &none, "GET", "/v1/capabilities", None).await;
    assert!(operations(&caps).is_empty(), "{caps}");
    assert!(caps["scope"].get("graphs").is_none(), "{caps}");
    // Collection access is unchanged.
    let (status, body) = browser(app, &none, "GET", "/v1/collections", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let bound = sign_in_with(app, "bea", "editor", Some(&acme), json!({"graphs": ["g"]})).await;
    let (_, body) = browser(app, &bound, "GET", "/v1/graphs", None).await;
    assert_eq!(graph_names(&body), ["g"], "{body}");
    let (status, _) = browser(app, &bound, "POST", "/v1/graphs/g/batch", write()).await;
    assert_eq!(status, StatusCode::OK);
    for (method, uri, input) in [
        ("GET", "/v1/graphs/h", None),
        ("POST", "/v1/graphs/h/batch", write()),
        ("POST", "/v1/graphs", Some(json!({"graph": "k"}))),
    ] {
        let (status, body) = browser(app, &bound, method, uri, input).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {body}");
    }
    let (_, caps) = browser(app, &bound, "GET", "/v1/capabilities", None).await;
    assert_eq!(caps["scope"]["graphs"], json!(["g"]), "{caps}");
    let (_, session) = browser(app, &bound, "GET", "/v1/auth/session", None).await;
    assert_eq!(session["user"]["graphs"], json!(["g"]), "{session}");
    assert_eq!(session["user"]["graph_access"], Value::Null, "{session}");

    let reader = sign_in_with(
        app,
        "rita",
        "editor",
        Some(&acme),
        json!({"graph_access": "read"}),
    )
    .await;
    let (_, body) = browser(app, &reader, "GET", "/v1/graphs", None).await;
    assert_eq!(graph_names(&body), ["g", "h"], "{body}");
    for (method, uri, input) in [
        ("POST", "/v1/graphs/g/batch", write()),
        ("POST", "/v1/graphs", Some(json!({"graph": "k"}))),
        ("DELETE", "/v1/graphs/h", None),
    ] {
        let (status, body) = browser(app, &reader, method, uri, input).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {body}");
    }
    let (_, caps) = browser(app, &reader, "GET", "/v1/capabilities", None).await;
    assert_eq!(operations(&caps), ["graph_read"], "{caps}");
    let (_, session) = browser(app, &reader, "GET", "/v1/auth/session", None).await;
    assert_eq!(session["user"]["graph_access"], "read", "{session}");

    let off = sign_in_with(app, "otto", "viewer", None, json!({"graph_access": "none"})).await;
    let (status, _) = browser(app, &off, "GET", "/v1/graphs/g", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A viewer can never be raised to write.
    let (status, body) = call(
        app,
        Some(ADMIN),
        "POST",
        "/v1/admin/users",
        Some(
            json!({"username": "vic", "password": PASSWORD, "role": "viewer",
            "graph_access": "write"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}
