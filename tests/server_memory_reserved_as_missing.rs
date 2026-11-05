#![cfg(feature = "server")]

use axum::{
    Router,
    body::Body,
    http::{HeaderMap, Request},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use vlite::{
    CollectionSpec, CreateOptions, Database,
    server::{BearerToken, ServerOptions, Service, configured_admin_router},
};

const ADMIN: &str = "fake_reserved_missing_admin_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const READ_ONLY: &str = "fake_reserved_missing_readonly_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const TENANT: &str = "0123456789abcdef0123456789abcdef";
const OPS: &[&str] = &[
    "read",
    "write",
    "search",
    "search_build",
    "collections",
    "index",
    "import",
    "diagnostics",
    "backup",
    "graph_read",
    "graph_write",
];

struct Fixture {
    app: Router,
    _directory: tempfile::TempDir,
    tokens: Vec<(&'static str, String, bool)>,
}

fn fixture() -> Fixture {
    fixture_with_bindings(true)
}

fn fixture_with_bindings(enabled: bool) -> Fixture {
    fixture_with(enabled, Value::Null)
}

fn fixture_with(enabled: bool, max_vectors: Value) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let mut db = Database::create_in_memory(2, CreateOptions::default()).unwrap();
    for prefix in ["".to_owned(), format!("t{TENANT}__")] {
        for name in ["notes_ev", "other_ev", "ordinary"] {
            db.create_collection(
                &format!("{prefix}{name}"),
                CollectionSpec {
                    dimensions: 2,
                    settings: Default::default(),
                },
            )
            .unwrap();
        }
        for name in ["notes_graph", "other_graph", "ordinary_graph"] {
            db.create_graph(&format!("{prefix}{name}")).unwrap();
        }
    }
    let mut tokens = Vec::new();
    let credentials: Vec<_> = [
        ("wildcard", "abcdefghij22", false, true),
        ("memory_only", "abcdefghij23", false, false),
        ("raw_other", "abcdefghij24", false, true),
        ("raw", "abcdefghij25", false, true),
        ("tenant", "abcdefghij26", true, true),
        ("tenant_raw", "abcdefghij27", true, true),
    ].into_iter().map(|(actor, id, tenant, all)| {
        let secret = "A".repeat(43);
        tokens.push((actor, format!("vlk_{id}_{secret}"), tenant));
        json!({"id":id,"name":actor,"all":all,"collections":if all { json!([]) } else { json!([{"name":"ordinary","id":db.collection("ordinary").unwrap().id()}]) },
            "graphs":if all { vec!["*"] } else { vec!["ordinary_graph"] }, "operations":OPS,
            "lineage":format!("{:016x}",db.lineage().unwrap()),
            "secret_sha256":format!("{:x}",Sha256::digest(secret.as_bytes())),
            "created_unix_ms":0,"expires_unix_ms":9_999_999_999_999u64,
            "revoked_unix_ms":null,"rotated_from":null,
            "tenant_id":tenant.then_some(TENANT)})
    }).collect();
    std::fs::write(
        directory.path().join("credentials.json"),
        json!({"schema":1,"credentials":credentials}).to_string(),
    )
    .unwrap();
    std::fs::write(
        directory.path().join("tenants.json"),
        json!([{"id":TENANT,"name":"fake tenant","max_vectors":max_vectors}]).to_string(),
    )
    .unwrap();
    let bindings: Vec<_> = [
        ("notes", "@root", "notes_graph", "notes_ev", "abcdefghij25"),
        ("other", "@root", "other_graph", "other_ev", "abcdefghij24"),
        ("notes", TENANT, "notes_graph", "notes_ev", "abcdefghij27"),
    ]
    .into_iter()
    .map(|(binding, tenant, graph, evidence, raw)| {
        json!({
            "binding":binding,"tenant":tenant,"generation":1,"state":"active",
            "graph":graph,"evidence_collection":evidence,"schema_version":"memory-schema-v1",
            "ranking_version":"memory-ranking-v1","digest_version":"memory-request-v1",
            "receipt_window_ms":86400000,
            "grants":[{"principal":raw,"scopes":[{"agent":"a"}],"raw_access":true},
                {"principal":"abcdefghij23","scopes":[{"agent":"a"}],"raw_access":false}]
        })
    })
    .collect();
    let path = directory.path().join("bindings.json");
    std::fs::write(
        &path,
        json!({"version":"memory-bindings-v1","bindings":bindings}).to_string(),
    )
    .unwrap();
    std::fs::write(
        directory.path().join("webhooks.cursor"),
        db.feed_cursor().unwrap().to_token(),
    )
    .unwrap();
    let app = configured_admin_router(
        Service::new(db),
        ServerOptions {
            loopback_only: true,
            admin_token: Some(BearerToken::new(ADMIN).unwrap()),
            read_only_token: Some(BearerToken::new(READ_ONLY).unwrap()),
            admin_directory: Some(directory.path().into()),
            memory_bindings_file: enabled.then_some(path),
            ..Default::default()
        },
    )
    .unwrap();
    Fixture {
        app,
        _directory: directory,
        tokens,
    }
}

async fn request(
    app: &Router,
    actor: &str,
    method: &str,
    path: &str,
    body: &str,
) -> (u16, HeaderMap, Vec<u8>) {
    let builder = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("sec-fetch-site", "same-origin")
        .header("x-vlite-request", "1");
    let builder = if actor.starts_with("vlite_session=") {
        builder.header("cookie", actor)
    } else {
        builder.header("authorization", format!("Bearer {actor}"))
    };
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let mut headers = response.headers().clone();
    headers.remove("x-request-id");
    headers.remove("date");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, bytes.to_vec())
}

/// Polls the rebuild of `collection` until no worker is left running. The
/// rebuild slot belongs to the whole database and a finished worker keeps it
/// until it is polled, so a cancelled worker that has not been joined makes
/// the next request for any other collection answer `rebuild_busy`, or not,
/// depending only on how fast the worker exited.
async fn join_rebuild_worker(app: &Router, collection: &str) {
    let poll = format!("/v1/collections/{collection}/index/hnsw/rebuild/poll");
    for _ in 0..100_000 {
        let polled = request(app, ADMIN, "POST", &poll, "{}").await;
        assert_eq!(polled.0, 200, "{polled:?}");
        let outcome = serde_json::from_slice::<Value>(&polled.2).unwrap()["outcome"].clone();
        if !matches!(outcome.as_str(), Some("running" | "cancelling")) {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the cancelled rebuild worker of {collection} never finished");
}

const CASES: &[(&str, &str, &str)] = &[
    ("GET", "/v1/collections/$c", ""),
    ("DELETE", "/v1/collections/$c", ""),
    ("GET", "/v1/collections/$c/vectors", ""),
    ("GET", "/v1/collections/$c/vectors/7", ""),
    ("PUT", "/v1/collections/$c/vectors/7", r#"{"vector":[0,1]}"#),
    ("DELETE", "/v1/collections/$c/vectors/7", ""),
    (
        "PATCH",
        "/v1/collections/$c/vectors/7/metadata",
        r#"{"metadata":{}}"#,
    ),
    ("PUT", "/v1/collections/$c/vectors/7/named", "{}"),
    ("POST", "/v1/collections/$c/batches", r#"{"operations":[]}"#),
    (
        "POST",
        "/v1/collections/$c/search",
        r#"{"vector":[0,1],"k":1}"#,
    ),
    (
        "POST",
        "/v1/collections/$c/search/grouped",
        r#"{"vector":[0,1],"field":"tag","groups":1,"hits_per_group":1}"#,
    ),
    (
        "POST",
        "/v1/collections/$c/search/geo",
        r#"{"vector":[0,1],"k":1,"field":"location","latitude":0,"longitude":0,"radius_meters":1}"#,
    ),
    (
        "POST",
        "/v1/collections/$c/recommend",
        r#"{"positive_ids":[],"k":1}"#,
    ),
    ("POST", "/v1/collections/$c/facets", r#"{"field":"tag"}"#),
    (
        "POST",
        "/v1/collections/$c/stats/numeric",
        r#"{"field":"score"}"#,
    ),
    ("POST", "/v1/collections/$c/query", r#"{"query":"*"}"#),
    (
        "POST",
        "/v1/collections/$c/embed",
        r#"{"text":"fake text"}"#,
    ),
    (
        "POST",
        "/v1/collections/$c/search/named",
        r#"{"name":"v","vector":[0,1],"k":1}"#,
    ),
    (
        "POST",
        "/v1/collections/$c/search/sparse",
        r#"{"name":"v","vector":[],"k":1}"#,
    ),
    (
        "POST",
        "/v1/collections/$c/search/dense-sparse",
        r#"{"dense_name":"v","dense":[0,1],"sparse_name":"s","sparse":[],"k":1}"#,
    ),
    (
        "POST",
        "/v1/collections/$c/vectors/configure",
        r#"{"name":"v","dimensions":2,"metric":"cosine","active_version":"v1"}"#,
    ),
    ("GET", "/v1/collections/$c/settings", ""),
    ("PATCH", "/v1/collections/$c/settings", "{}"),
    ("PUT", "/v1/collections/$c/settings", "{}"),
    ("GET", "/v1/collections/$c/kv/entry?namespace=n&key=k", ""),
    (
        "PUT",
        "/v1/collections/$c/kv/entry?namespace=n&key=k",
        r#"{"value_base64":""}"#,
    ),
    (
        "DELETE",
        "/v1/collections/$c/kv/entry?namespace=n&key=k",
        "",
    ),
    ("GET", "/v1/collections/$c/kv/entries?namespace=n", ""),
    ("GET", "/v1/collections/$c/index/hnsw", ""),
    ("POST", "/v1/collections/$c/index/hnsw", "{}"),
    ("GET", "/v1/collections/$c/index/hnsw/maintenance", ""),
    ("POST", "/v1/collections/$c/index/hnsw/maintenance/tick", ""),
    ("GET", "/v1/collections/$c/index/hnsw/rebuild", ""),
    ("POST", "/v1/collections/$c/index/hnsw/rebuild", "{}"),
    ("POST", "/v1/collections/$c/index/hnsw/rebuild/cancel", "{}"),
    ("POST", "/v1/collections/$c/index/hnsw/rebuild/poll", "{}"),
    ("POST", "/v1/collections/$c/index/hnsw/quality", "{}"),
    ("POST", "/v1/collections/$c/index/disk-ann", "{}"),
    (
        "POST",
        "/v1/collections/$c/search/disk-ann",
        r#"{"vector":[0,1],"k":1,"ef_search":1}"#,
    ),
    ("POST", "/v1/collections/$c/imports", ""),
    ("GET", "/v1/graphs/$g", ""),
    ("DELETE", "/v1/graphs/$g", ""),
    ("GET", "/v1/graphs/$g/schema", ""),
    ("PUT", "/v1/graphs/$g/schema", r#"{"schema":{}}"#),
    ("POST", "/v1/graphs/$g/nodes/get", r#"{"node":{"key":"n"}}"#),
    ("POST", "/v1/graphs/$g/edges/get", r#"{"edge":{"key":"e"}}"#),
    ("POST", "/v1/graphs/$g/nodes/find", "{}"),
    ("POST", "/v1/graphs/$g/neighbors", r#"{"node":{"key":"n"}}"#),
    (
        "POST",
        "/v1/graphs/$g/traverse",
        r#"{"start":[{"key":"n"}]}"#,
    ),
    (
        "POST",
        "/v1/graphs/$g/shortest-path",
        r#"{"source":{"key":"n"},"destination":{"key":"m"}}"#,
    ),
    (
        "POST",
        "/v1/graphs/$g/explain",
        r#"{"operation":"find-nodes"}"#,
    ),
    (
        "POST",
        "/v1/graphs/$g/query",
        r#"{"query":"MATCH (n) RETURN n LIMIT 1"}"#,
    ),
    (
        "POST",
        "/v1/graphs/$g/retrieve",
        r#"{"pivot":{"collection":"ordinary","vector":[0,1],"k":1}}"#,
    ),
    ("POST", "/v1/graphs/$g/batch", r#"{"ops":[]}"#),
    (
        "POST",
        "/v1/graphs/$g/batch",
        r#"{"ops":[{"op":"delete_node","key":"a"}]}"#,
    ),
    (
        "POST",
        "/v1/graphs/$g/batch",
        r#"{"ops":[{"op":"create_node","key":"n"}]}"#,
    ),
    ("POST", "/v1/graphs/$g/batch", "{}"),
    ("POST", "/v1/graphs/$g/nodes/get", "{}"),
    ("POST", "/v1/graphs/$g/query", r#"{"query":"bad query"}"#),
    ("GET", "/v1/graphs/$g/schema?budget=bad", ""),
];

#[test]
fn every_name_addressed_openapi_operation_has_a_differential_case() {
    use std::collections::BTreeSet;

    fn normalize(method: &str, path: &str) -> (String, String) {
        let path = path.split('?').next().unwrap();
        let segments: Vec<_> = path
            .split('/')
            .map(|segment| match segment {
                "$c" | "{c}" | "{collection}" => "{collection}",
                "$g" | "{g}" | "{graph}" => "{graph}",
                segment
                    if (segment.starts_with('{') && segment.ends_with('}'))
                        || (!segment.is_empty()
                            && segment.bytes().all(|byte| byte.is_ascii_digit())) =>
                {
                    "{id}"
                }
                segment => segment,
            })
            .collect();
        (method.to_ascii_uppercase(), segments.join("/"))
    }

    let spec: Value = serde_json::from_str(include_str!("../src/openapi.json")).unwrap();
    let mut inventory = BTreeSet::new();
    for (path, item) in spec["paths"].as_object().unwrap() {
        if !path
            .split('/')
            .any(|segment| matches!(segment, "{c}" | "{collection}" | "{g}" | "{graph}"))
        {
            continue;
        }
        for method in item.as_object().unwrap().keys() {
            if matches!(
                method.as_str(),
                "get" | "post" | "put" | "patch" | "delete" | "head" | "options" | "trace"
            ) {
                inventory.insert(normalize(method, path));
            }
        }
    }
    assert!(
        !inventory.is_empty(),
        "the raw route inventory must not be empty"
    );
    let cases: BTreeSet<_> = CASES
        .iter()
        .map(|(method, path, _)| normalize(method, path))
        .collect();
    let uncovered: Vec<_> = inventory.difference(&cases).collect();
    assert!(
        uncovered.is_empty(),
        "a new raw route naming a collection or graph must be added to CASES \
         (reserved-versus-missing differential) before it ships: {uncovered:?}"
    );

    // Removing coverage must fail even when several request bodies share a route.
    for omitted in &inventory {
        let mutant_cases: BTreeSet<_> = CASES
            .iter()
            .map(|(method, path, _)| normalize(method, path))
            .filter(|operation| operation != omitted)
            .collect();
        assert_eq!(
            inventory.difference(&mutant_cases).collect::<Vec<_>>(),
            vec![omitted],
            "inventory must detect removal of CASES coverage for {omitted:?}"
        );
    }
}

async fn add_sessions(fixture: &mut Fixture) {
    for tenant in [false, true] {
        for role in ["viewer", "editor"] {
            let username = format!("{role}-{tenant}");
            let password = "fake-testing-password-not-real";
            let (status, _, body) = request(
                &fixture.app,
                ADMIN,
                "POST",
                "/v1/admin/users",
                &json!({
                    "username":username,"password":password,"role":role,
                    "tenant_id":tenant.then_some(TENANT),"must_change_password":false
                })
                .to_string(),
            )
            .await;
            assert_eq!(status, 201, "{body:?}");
            let response = fixture
                .app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/auth/login")
                        .header("content-type", "application/json")
                        .header("sec-fetch-site", "same-origin")
                        .header("x-vlite-request", "1")
                        .body(Body::from(
                            json!({"username":username,"password":password}).to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            let cookie = response.headers()["set-cookie"]
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .to_owned();
            fixture.tokens.push((role, cookie, tenant));
        }
    }
}

#[tokio::test]
async fn retrieve_masks_collections_at_their_normal_resolution_stage() {
    let mut fixture = fixture();
    let raw = &fixture
        .tokens
        .iter()
        .find(|(actor, _, _)| *actor == "raw")
        .unwrap()
        .1;
    assert_eq!(request(&fixture.app, raw, "POST", "/v1/graphs/notes_graph/batch",
        r#"{"ops":[{"op":"create_node","key":"a"},{"op":"put_vector","collection":"notes_ev","id":"1","vector":[0,1]},{"op":"attach","node":"a","collection":"notes_ev","record":"1"}]}"#).await.0, 200);
    let wildcard = &fixture.tokens[0].1;
    let deletion = r#"{"ops":[{"op":"delete_node","key":"a"}]}"#;
    let hidden = request(
        &fixture.app,
        wildcard,
        "POST",
        "/v1/graphs/notes_graph/batch",
        deletion,
    )
    .await;
    assert_eq!(hidden.0, 409);
    assert_eq!(
        hidden,
        request(
            &fixture.app,
            wildcard,
            "POST",
            "/v1/graphs/absent_graph/batch",
            deletion
        )
        .await
    );
    fixture.tokens.extend([
        ("admin", ADMIN.into(), false),
        ("read_only", READ_ONLY.into(), false),
    ]);
    add_sessions(&mut fixture).await;
    for (actor, token, tenant) in &fixture.tokens {
        if matches!(*actor, "raw" | "tenant_raw" | "memory_only") {
            continue;
        }
        let prefix = if *tenant {
            format!("t{TENANT}__")
        } else {
            String::new()
        };
        for graph in ["absent_graph", "notes_graph", "ordinary_graph"] {
            for evidence in [false, true] {
                let body = |collection: &str| {
                    if evidence {
                    json!({"pivot":{"collection":format!("{prefix}ordinary"),"vector":[0,1],"k":1},
                        "evidence":{"collections":[format!("{prefix}{collection}")]}})
                } else {
                    json!({"pivot":{"collection":format!("{prefix}{collection}"),"vector":[0,1],"k":1}})
                }.to_string()
                };
                let path = format!("/v1/graphs/{graph}/retrieve");
                let reserved = request(&fixture.app, token, "POST", &path, &body("notes_ev")).await;
                let missing = request(&fixture.app, token, "POST", &path, &body("absent_ev")).await;
                assert_eq!(reserved, missing, "{actor}/{graph}/evidence={evidence}");
            }
        }
    }
}

#[tokio::test]
async fn ordinary_missing_references_and_redaction_do_not_depend_on_bindings() {
    let bound = fixture();
    let unbound = fixture_with_bindings(false);
    for fixture in [&bound, &unbound] {
        assert_eq!(
            request(
                &fixture.app,
                ADMIN,
                "POST",
                "/v1/collections",
                r#"{"name":"secret","dimensions":2}"#
            )
            .await
            .0,
            201
        );
        assert_eq!(request(&fixture.app, ADMIN, "POST", "/v1/graphs/ordinary_graph/batch",
            r#"{"ops":[{"op":"create_node","key":"n"},{"op":"put_vector","collection":"ordinary","id":"1","vector":[0,1]},{"op":"put_vector","collection":"secret","id":"1","vector":[0,1]},{"op":"attach","node":"n","collection":"ordinary","record":"1"},{"op":"attach","node":"n","collection":"secret","record":"1"}]}"#).await.0, 200);
    }
    let scoped = &bound
        .tokens
        .iter()
        .find(|(actor, _, _)| *actor == "memory_only")
        .unwrap()
        .1;
    let body = r#"{"pivot":{"collection":"ordinary","vector":[0,1],"k":1},"evidence":{"collections":["secret"]}}"#;
    for fixture in [&bound, &unbound] {
        let response = request(
            &fixture.app,
            scoped,
            "POST",
            "/v1/graphs/ordinary_graph/retrieve",
            body,
        )
        .await;
        assert_eq!(response.0, 200, "{response:?}");
        let value: Value = serde_json::from_slice(&response.2).unwrap();
        assert_eq!(
            value["redacted"],
            json!({"count":1,"collections":["secret"]})
        );
        assert_eq!(value["evidence"], json!([]));
    }
    for (actor, token, _) in
        bound
            .tokens
            .iter()
            .chain(std::iter::once(&("admin", ADMIN.into(), false)))
    {
        for graph in ["ordinary_graph", "absent_graph"] {
            for body in [
                r#"{"pivot":{"collection":"absent_ev","vector":[0,1],"k":1}}"#,
                r#"{"pivot":{"collection":"ordinary","vector":[0,1],"k":1},"evidence":{"collections":["absent_ev"]}}"#,
                r#"{"ops":[{"op":"attach","node":"missing","collection":"absent_ev","record":"1"}]}"#,
                r#"{"ops":[{"op":"create_node","key":""},{"op":"detach","node":"missing","collection":"absent_ev"}]}"#,
                r#"{"ops":[{"op":"put_vector","collection":"ordinary","id":"7","vector":[0]},{"op":"detach","node":"missing","collection":"absent_ev"}]}"#,
            ] {
                let path = format!(
                    "/v1/graphs/{graph}/{}",
                    if body.contains("\"ops\"") {
                        "batch"
                    } else {
                        "retrieve"
                    }
                );
                assert_eq!(
                    request(&bound.app, token, "POST", &path, body).await,
                    request(&unbound.app, token, "POST", &path, body).await,
                    "{actor}/{path}/{body}"
                );
            }
        }
    }
}

#[tokio::test]
async fn batch_masks_collections_at_their_normal_resolution_stage() {
    let mut fixture = fixture();
    fixture.tokens.push(("admin", ADMIN.into(), false));
    add_sessions(&mut fixture).await;
    for (actor, token, tenant) in &fixture.tokens {
        if matches!(*actor, "raw" | "tenant_raw") {
            continue;
        }
        let prefix = if *tenant {
            format!("t{TENANT}__")
        } else {
            String::new()
        };
        for graph in ["ordinary_graph", "absent_graph", "notes_graph"] {
            for operation in ["attach", "detach", "put_vector"] {
                for invalid_first in [false, true] {
                    let body = |name| {
                        let collection = format!("{prefix}{name}");
                        let op = match operation {
                            "attach" => {
                                json!({"op":"attach","node":"n","collection":collection,"record":"1"})
                            }
                            "detach" => json!({"op":"detach","node":"n","collection":collection}),
                            _ => {
                                json!({"op":"put_vector","collection":collection,"id":"1","vector":[0,1]})
                            }
                        };
                        let mut ops = Vec::new();
                        if invalid_first {
                            ops.push(json!({"op":"put_vector","collection":format!("{prefix}ordinary"),"id":"7","vector":[0]}));
                        }
                        ops.push(op);
                        json!({"ops":ops}).to_string()
                    };
                    let path = format!("/v1/graphs/{graph}/batch");
                    assert_eq!(
                        request(&fixture.app, token, "POST", &path, &body("notes_ev")).await,
                        request(&fixture.app, token, "POST", &path, &body("absent_ev")).await,
                        "{actor}/{graph}/{operation}/invalid_first={invalid_first}"
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn nested_reserved_changes_are_filtered_from_feed_and_webhooks() {
    let fixture = fixture();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/hook", listener.local_addr().unwrap());
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let app = Router::new().route(
        "/hook",
        axum::routing::post(move |axum::Json(value): axum::Json<Value>| {
            let sender = sender.clone();
            async move {
                sender.send(value).unwrap();
                axum::http::StatusCode::NO_CONTENT
            }
        }),
    );
    let listener_task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let subscription = json!({"url":endpoint,"secret":"fake-secret-test-only"});
    assert_eq!(
        request(
            &fixture.app,
            ADMIN,
            "POST",
            "/v1/admin/webhooks",
            &subscription.to_string()
        )
        .await
        .0,
        201
    );
    let before = request(&fixture.app, ADMIN, "GET", "/v1/changes?limit=1000", "").await;
    let before: Value = serde_json::from_slice(&before.2).unwrap();
    for (actor, _, tenant) in fixture
        .tokens
        .iter()
        .filter(|(actor, _, _)| matches!(*actor, "raw" | "tenant_raw"))
    {
        let raw = &fixture
            .tokens
            .iter()
            .find(|(name, _, _)| name == actor)
            .unwrap()
            .1;
        let prefix = if *tenant {
            format!("t{TENANT}__")
        } else {
            String::new()
        };
        let ops = json!({"ops":[{"op":"create_node","key":"hidden_ref"},
            {"op":"put_vector","collection":format!("{prefix}notes_ev"),"id":"17","vector":[0,1]},
            {"op":"attach","node":"hidden_ref","collection":format!("{prefix}notes_ev"),"record":"17"}]});
        assert_eq!(
            request(
                &fixture.app,
                raw,
                "POST",
                "/v1/graphs/ordinary_graph/batch",
                &ops.to_string()
            )
            .await
            .0,
            200
        );
        for op in ["detach", "attach"] {
            let ops = json!({"ops":[{"op":op,"node":"hidden_ref","collection":format!("{prefix}notes_ev"),"record":"17"}]});
            let ops = if op == "detach" {
                json!({"ops":[{"op":"detach","node":"hidden_ref","collection":format!("{prefix}notes_ev")}]})
            } else {
                ops
            };
            assert_eq!(
                request(
                    &fixture.app,
                    raw,
                    "POST",
                    "/v1/graphs/ordinary_graph/batch",
                    &ops.to_string()
                )
                .await
                .0,
                200
            );
        }
    }
    let after = request(
        &fixture.app,
        ADMIN,
        "GET",
        &format!(
            "/v1/changes?limit=1000&cursor={}",
            before["next_cursor"].as_str().unwrap()
        ),
        "",
    )
    .await;
    assert_eq!(after.0, 200);
    let after: Value = serde_json::from_slice(&after.2).unwrap();
    assert_ne!(before["next_cursor"], after["next_cursor"]);
    let cursor =
        vlite::changes::FeedCursor::from_token(after["next_cursor"].as_str().unwrap()).unwrap();
    assert_eq!(cursor.sequence.to_string(), after["head"].as_str().unwrap());
    assert!(!after.to_string().contains("notes_ev"), "{after}");
    let mut visible_graph_batches = 0;
    for event in after["changes"].as_array().unwrap() {
        if event["change"]["type"] == "graph_batch" {
            visible_graph_batches += 1;
            assert!(!event["change"]["operations"].as_array().unwrap().is_empty());
        }
    }
    assert_eq!(visible_graph_batches, 2, "{after}");
    assert_eq!(
        request(
            &fixture.app,
            ADMIN,
            "PUT",
            "/v1/collections/ordinary/vectors/999",
            r#"{"vector":[0,1]}"#
        )
        .await
        .0,
        204
    );
    let until = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut root_delivered = false;
    let mut tenant_delivered = false;
    loop {
        let event = tokio::time::timeout_at(until, receiver.recv())
            .await
            .unwrap()
            .unwrap();
        let text = event.to_string();
        assert!(!text.contains("notes_ev"), "{event}");
        root_delivered |= text.contains("\"ordinary_graph\"");
        tenant_delivered |= text.contains(&format!("\"t{TENANT}__ordinary_graph\""));
        if text.contains("\"999\"") {
            break;
        }
    }
    assert!(root_delivered && tenant_delivered);
    listener_task.abort();
}

fn token<'a>(fixture: &'a Fixture, actor: &str) -> &'a str {
    &fixture
        .tokens
        .iter()
        .find(|(name, _, _)| *name == actor)
        .unwrap()
        .1
}

async fn feed(fixture: &Fixture, actor: &str, cursor: &str) -> Value {
    let path = if cursor.is_empty() {
        "/v1/changes?limit=1000".to_owned()
    } else {
        format!("/v1/changes?limit=1000&cursor={cursor}")
    };
    let page = request(&fixture.app, actor, "GET", &path, "").await;
    assert_eq!(page.0, 200);
    serde_json::from_slice(&page.2).unwrap()
}

/// Attach and detach operations that name `notes_ev`, per type, in every
/// graph batch event of a feed page.
fn evidence_operations(page: &Value) -> (usize, usize) {
    let mut attached = 0;
    let mut detached = 0;
    for event in page["changes"].as_array().unwrap() {
        if event["change"]["type"] != "graph_batch" {
            continue;
        }
        for operation in event["change"]["operations"].as_array().unwrap() {
            if operation["collection"] != "notes_ev" {
                continue;
            }
            match operation["op"].as_str() {
                Some("attached") => attached += 1,
                Some("detached") => detached += 1,
                _ => {}
            }
        }
    }
    (attached, detached)
}

#[tokio::test]
async fn raw_holders_see_their_own_attach_and_detach_operations_in_the_feed() {
    let fixture = fixture();
    let raw = token(&fixture, "raw");
    let start = feed(&fixture, ADMIN, "").await["next_cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let first = r#"{"ops":[{"op":"create_node","key":"n"},{"op":"put_vector","collection":"notes_ev","id":"17","vector":[0,1]},{"op":"attach","node":"n","collection":"notes_ev","record":"17"}]}"#;
    let second = r#"{"ops":[{"op":"detach","node":"n","collection":"notes_ev"}]}"#;
    for body in [first, second] {
        assert_eq!(
            request(
                &fixture.app,
                raw,
                "POST",
                "/v1/graphs/ordinary_graph/batch",
                body
            )
            .await
            .0,
            200
        );
    }

    let own = feed(&fixture, raw, &start).await;
    assert_eq!(evidence_operations(&own), (1, 1), "{own}");
    assert!(own.to_string().contains("notes_ev"));
    for actor in ["raw_other", "wildcard"] {
        let page = feed(&fixture, token(&fixture, actor), &start).await;
        assert_eq!(evidence_operations(&page), (0, 0), "{actor}: {page}");
        assert!(!page.to_string().contains("notes_ev"), "{actor}: {page}");
    }
    let admin = feed(&fixture, ADMIN, &start).await;
    assert_eq!(evidence_operations(&admin), (0, 0), "{admin}");
    assert!(!admin.to_string().contains("notes_ev"), "{admin}");
}

async fn admin_batch(app: &Router, operation: &str, name: &str) -> (u16, String) {
    let body = format!(r#"{{"ops":[{}]}}"#, operation.replace("$c", name));
    let answer = request(app, ADMIN, "POST", "/v1/graphs/ordinary_graph/batch", &body).await;
    (
        answer.0,
        String::from_utf8(answer.2).unwrap().replace(name, "$c"),
    )
}

#[tokio::test]
async fn graph_batch_quota_answers_do_not_reveal_a_reserved_tenant_collection() {
    // A tenant at its vector quota: the quota check must not run before the
    // reserved collection is masked, or the administrator can tell it from a
    // missing one by the 429.
    let fixture = fixture_with(true, json!(0));
    let reserved = format!("t{TENANT}__notes_ev");
    let missing = format!("t{TENANT}__absent_ev");
    let ordinary = format!("t{TENANT}__ordinary");
    for (operation, over_quota) in [
        (
            r#"{"op":"put_vector","collection":"$c","id":"6","vector":[0,1]}"#,
            true,
        ),
        (
            r#"{"op":"delete_vector","collection":"$c","id":"6"}"#,
            false,
        ),
    ] {
        let control = admin_batch(&fixture.app, operation, &ordinary).await;
        if over_quota {
            assert_eq!(control.0, 429, "the quota path must be live: {control:?}");
        }
        let hidden = admin_batch(&fixture.app, operation, &reserved).await;
        let absent = admin_batch(&fixture.app, operation, &missing).await;
        assert_ne!(absent.0, 429, "{absent:?}");
        assert_eq!(hidden, absent, "{operation}");
    }
}

#[tokio::test]
async fn credential_creation_masks_reserved_collection_names() {
    let fixture = fixture();
    let expires = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 3_600_000;
    for tenant in [None, Some(TENANT)] {
        let body = |name| {
            json!({"name":"fake-probe","collections":[name],"operations":["read"],
            "tenant_id":tenant,"expires_unix_ms":expires})
            .to_string()
        };
        let reserved = request(
            &fixture.app,
            ADMIN,
            "POST",
            "/v1/admin/credentials",
            &body("notes_ev"),
        )
        .await;
        let missing = request(
            &fixture.app,
            ADMIN,
            "POST",
            "/v1/admin/credentials",
            &body("absent_ev"),
        )
        .await;
        assert_eq!(reserved, missing);
        assert_eq!(reserved.0, 404);
        assert_eq!(
            request(
                &fixture.app,
                ADMIN,
                "POST",
                "/v1/admin/credentials",
                &body("*")
            )
            .await
            .0,
            201
        );
    }
    // A name reserved only at root is an ordinary name for a tenant, and the
    // other way round: the check runs on the qualified name.
    let credential = |tenant: Option<&str>, name: &str| {
        json!({"name":"fake-probe","collections":[name],"operations":["read"],
            "tenant_id":tenant,"expires_unix_ms":expires})
        .to_string()
    };
    for (tenant, name, status) in [
        (Some(TENANT), "other_ev", 201),
        (None, "other_ev", 404),
        (Some(TENANT), "notes_ev", 404),
    ] {
        assert_eq!(
            request(
                &fixture.app,
                ADMIN,
                "POST",
                "/v1/admin/credentials",
                &credential(tenant, name)
            )
            .await
            .0,
            status,
            "{tenant:?} {name}"
        );
    }
}

#[tokio::test]
async fn bound_tenant_deletion_matches_an_ordinary_nonempty_tenant() {
    let fixture = fixture();
    let unknown = request(
        &fixture.app,
        ADMIN,
        "DELETE",
        "/v1/admin/tenants/ffffffffffffffffffffffffffffffff",
        "",
    )
    .await;
    assert_eq!(unknown.0, 404);
    // Existence is checked before bindings: the untenanted root is not a
    // tenant, whatever bindings it holds.
    assert_eq!(
        request(&fixture.app, ADMIN, "DELETE", "/v1/admin/tenants/@root", "").await,
        unknown
    );
    let created = request(
        &fixture.app,
        ADMIN,
        "POST",
        "/v1/admin/tenants",
        r#"{"name":"ordinary deletion comparison"}"#,
    )
    .await;
    assert_eq!(created.0, 201);
    let created: Value = serde_json::from_slice(&created.2).unwrap();
    let ordinary = created["tenant"]["id"].as_str().unwrap();
    assert_eq!(
        request(
            &fixture.app,
            ADMIN,
            "POST",
            "/v1/collections",
            &json!({"name":format!("t{ordinary}__ordinary"),"dimensions":2}).to_string()
        )
        .await
        .0,
        201
    );
    let expected = request(
        &fixture.app,
        ADMIN,
        "DELETE",
        &format!("/v1/admin/tenants/{ordinary}"),
        "",
    )
    .await;
    assert_eq!(expected.0, 409);
    let error: Value = serde_json::from_slice(&expected.2).unwrap();
    assert_eq!(error["error"]["code"], "tenant_not_empty");
    assert_eq!(
        error["error"]["message"],
        "drop tenant collections before deleting the tenant"
    );
    let bound_path = format!("/v1/admin/tenants/{TENANT}");
    assert_eq!(
        request(&fixture.app, ADMIN, "DELETE", &bound_path, "").await,
        expected
    );
    let raw = &fixture
        .tokens
        .iter()
        .find(|(actor, _, _)| *actor == "tenant_raw")
        .unwrap()
        .1;
    for (kind, names) in [
        ("collections", ["notes_ev", "other_ev", "ordinary"]),
        ("graphs", ["notes_graph", "other_graph", "ordinary_graph"]),
    ] {
        for name in names {
            if kind == "graphs" {
                let nodes = request(
                    &fixture.app,
                    raw,
                    "POST",
                    &format!("/v1/graphs/{name}/nodes/find"),
                    "{}",
                )
                .await;
                assert_eq!(nodes.0, 200);
                let nodes: Value = serde_json::from_slice(&nodes.2).unwrap();
                for node in nodes["nodes"].as_array().unwrap() {
                    let deletion = json!({"ops":[{"op":"delete_node","key":node["key"]}]});
                    assert_eq!(
                        request(
                            &fixture.app,
                            raw,
                            "POST",
                            &format!("/v1/graphs/{name}/batch"),
                            &deletion.to_string()
                        )
                        .await
                        .0,
                        200
                    );
                }
            }
            let response = request(
                &fixture.app,
                raw,
                "DELETE",
                &format!("/v1/{kind}/{name}"),
                "",
            )
            .await;
            assert!(response.0 < 300, "{kind}/{name}: {response:?}");
        }
    }
    let usage = request(
        &fixture.app,
        ADMIN,
        "GET",
        &format!("{bound_path}/usage"),
        "",
    )
    .await;
    assert_eq!(usage.0, 200);
    let usage: Value = serde_json::from_slice(&usage.2).unwrap();
    assert_eq!(usage["collections"], json!([]));
    assert_eq!(
        request(&fixture.app, ADMIN, "DELETE", &bound_path, "").await,
        expected
    );
    let missing = request(
        &fixture.app,
        ADMIN,
        "DELETE",
        "/v1/admin/tenants/ffffffffffffffffffffffffffffffff",
        "",
    )
    .await;
    assert_eq!(missing.0, 404);
}

#[tokio::test]
async fn reserved_names_match_missing_names_for_raw_routes() {
    let mut fixture = fixture();
    fixture.tokens.extend([
        ("admin", ADMIN.into(), false),
        ("read_only", READ_ONLY.into(), false),
    ]);
    add_sessions(&mut fixture).await;
    for (actor, token, tenant) in &fixture.tokens {
        if matches!(*actor, "raw" | "tenant_raw") {
            assert_eq!(
                request(&fixture.app, token, "GET", "/v1/collections/notes_ev", "")
                    .await
                    .0,
                200
            );
            assert_eq!(
                request(&fixture.app, token, "GET", "/v1/graphs/notes_graph", "")
                    .await
                    .0,
                200
            );
            continue;
        }
        assert_eq!(
            request(&fixture.app, token, "GET", "/v1/collections/ordinary", "")
                .await
                .0,
            200
        );
        assert_eq!(
            request(&fixture.app, token, "GET", "/v1/graphs/ordinary_graph", "")
                .await
                .0,
            200
        );
        for prefix in if *tenant {
            vec![String::new()]
        } else {
            vec![String::new(), format!("t{TENANT}__")]
        } {
            for (method, path, body) in CASES {
                let render = |text: &str, missing| {
                    text.replace(
                        "$c",
                        &format!("{prefix}{}", if missing { "absent_ev" } else { "notes_ev" }),
                    )
                    .replace(
                        "$g",
                        &format!(
                            "{prefix}{}",
                            if missing {
                                "absent_graph"
                            } else {
                                "notes_graph"
                            }
                        ),
                    )
                };
                let reserved = request(
                    &fixture.app,
                    token,
                    method,
                    &render(path, false),
                    &render(body, false),
                )
                .await;
                let missing = request(
                    &fixture.app,
                    token,
                    method,
                    &render(path, true),
                    &render(body, true),
                )
                .await;
                assert_eq!(
                    reserved, missing,
                    "{actor}/tenant={tenant}: {method} {path}"
                );
                if *method == "GET" && *path == "/v1/collections/$c" {
                    assert_eq!(missing.0, if *actor == "memory_only" { 403 } else { 404 });
                }
                if *method == "GET" && *path == "/v1/graphs/$g" {
                    assert_eq!(missing.0, if *actor == "memory_only" { 403 } else { 409 });
                }
            }
        }
    }
}

#[tokio::test]
async fn ordinary_neighbours_preserve_raw_route_outcomes_with_bindings() {
    for principal in 0..12 {
        let mut bound = fixture();
        let mut unbound = fixture_with_bindings(false);
        for fixture in [&mut bound, &mut unbound] {
            fixture.tokens.extend([
                ("admin", ADMIN.into(), false),
                ("read_only", READ_ONLY.into(), false),
            ]);
            if principal >= 8 {
                add_sessions(fixture).await;
            }
        }
        let (actor, token, tenant) = &bound.tokens[principal];
        let (_, reference_token, _) = &unbound.tokens[principal];
        for prefix in if *tenant {
            vec![String::new()]
        } else {
            vec![String::new(), format!("t{TENANT}__")]
        } {
            for (method, path, body) in CASES
                .iter()
                .filter(|case| case.0 != "DELETE")
                .chain(CASES.iter().filter(|case| case.0 == "DELETE"))
            {
                let render = |text: &str| {
                    text.replace("$c", &format!("{prefix}ordinary"))
                        .replace("$g", &format!("{prefix}ordinary_graph"))
                };
                let stored_collection = if *tenant {
                    format!("t{TENANT}__ordinary")
                } else {
                    format!("{prefix}ordinary")
                };
                // Independent workers can finish at different times between requests.
                for app in [&bound.app, &unbound.app] {
                    let cancelled = request(
                        app,
                        ADMIN,
                        "POST",
                        &format!("/v1/collections/{stored_collection}/index/hnsw/rebuild/cancel"),
                        "",
                    )
                    .await;
                    assert!(matches!(cancelled.0, 200 | 404), "{cancelled:?}");
                    if cancelled.0 == 200 {
                        join_rebuild_worker(app, &stored_collection).await;
                    }
                }
                let actual = request(&bound.app, token, method, &render(path), &render(body)).await;
                let reference = request(
                    &unbound.app,
                    reference_token,
                    method,
                    &render(path),
                    &render(body),
                )
                .await;
                if path.ends_with("/imports") {
                    assert_eq!(actual.0, 403, "binding-wide import refusal is unchanged");
                    continue;
                }
                assert_eq!(
                    actual.0, reference.0,
                    "{actor}/tenant={tenant}: {method} {path}: {:?} vs {:?}",
                    actual.2, reference.2,
                );
                if actual.2.is_empty() || reference.2.is_empty() {
                    assert_eq!(actual.2, reference.2, "{actor}: {method} {path}");
                } else if actual.0 >= 400 {
                    let error = |bytes: &[u8]| {
                        serde_json::from_slice::<Value>(bytes).unwrap()["error"].clone()
                    };
                    assert_eq!(
                        error(&actual.2),
                        error(&reference.2),
                        "{actor}/tenant={tenant}: {method} {path}",
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn reserved_creates_are_occupied_and_subscriptions_accept_hidden_filters() {
    let mut fixture = fixture();
    fixture.tokens.extend([
        ("admin", ADMIN.into(), false),
        ("read_only", READ_ONLY.into(), false),
    ]);
    add_sessions(&mut fixture).await;
    for (actor, token, _) in &fixture.tokens {
        for (path, field, reserved, ordinary) in [
            ("/v1/collections", "name", "notes_ev", "ordinary"),
            ("/v1/graphs", "graph", "notes_graph", "ordinary_graph"),
        ] {
            let body = |name| {
                if field == "name" {
                    json!({"name":name,"dimensions":2})
                } else {
                    json!({"graph":name})
                }
            };
            let reserved = request(
                &fixture.app,
                token,
                "POST",
                path,
                &body(reserved).to_string(),
            )
            .await;
            let occupied = request(
                &fixture.app,
                token,
                "POST",
                path,
                &body(ordinary).to_string(),
            )
            .await;
            if *actor == "memory_only" {
                assert_eq!(reserved.0, 403);
            } else {
                assert_eq!(reserved, occupied, "{actor}/{path}");
                assert_eq!(
                    reserved.0,
                    if matches!(*actor, "viewer" | "read_only") {
                        403
                    } else {
                        409
                    },
                    "{actor}/{path}"
                );
            }
        }
    }
    for (actor, token, _) in &fixture.tokens {
        for field in ["collections", "graphs"] {
            let mut denied = Vec::new();
            for name in if field == "collections" {
                ["notes_ev", "absent_ev"]
            } else {
                ["notes_graph", "absent_graph"]
            } {
                let body = json!({"url":"http://127.0.0.1:9/fake-hook","secret":"fake-secret-test-only",field:[name]});
                let response = request(
                    &fixture.app,
                    token,
                    "POST",
                    "/v1/admin/webhooks",
                    &body.to_string(),
                )
                .await;
                if *actor == "admin" {
                    assert_eq!(response.0, 201, "{response:?}");
                    let value: Value = serde_json::from_slice(&response.2).unwrap();
                    assert_eq!(value[field], json!([name]));
                } else {
                    assert_eq!(response.0, 403, "{actor}: {response:?}");
                    denied.push(response);
                }
            }
            if !denied.is_empty() {
                assert_eq!(denied[0], denied[1], "{actor}/{field}");
            }
        }
    }
}

#[tokio::test]
async fn body_named_reserved_evidence_matches_missing_with_grants_first() {
    let mut fixture = fixture();
    fixture.tokens.extend([
        ("admin", ADMIN.into(), false),
        ("read_only", READ_ONLY.into(), false),
    ]);
    add_sessions(&mut fixture).await;
    for (actor, token, tenant) in &fixture.tokens {
        let prefix = if *tenant {
            format!("t{TENANT}__")
        } else {
            String::new()
        };
        for template in [
            r#"{"ops":[{"op":"attach","node":"n","collection":"$c","record":"1"}]}"#,
            r#"{"ops":[{"op":"detach","node":"n","collection":"$c"}]}"#,
            r#"{"pivot":{"collection":"$c","vector":[0,1],"k":1}}"#,
            r#"{"pivot":{"collection":"$o","vector":[0,1],"k":1},"evidence":{"collections":["$c"]}}"#,
        ] {
            let path = if template.contains("\"ops\"") {
                "/v1/graphs/ordinary_graph/batch"
            } else {
                "/v1/graphs/ordinary_graph/retrieve"
            };
            let render = |name| {
                template
                    .replace("$c", &format!("{prefix}{name}"))
                    .replace("$o", &format!("{prefix}ordinary"))
            };
            let reserved = request(&fixture.app, token, "POST", path, &render("notes_ev")).await;
            let missing = request(&fixture.app, token, "POST", path, &render("absent_ev")).await;
            if !matches!(*actor, "raw" | "tenant_raw") {
                assert_eq!(reserved, missing, "{actor}/{path}/{template}");
                assert_eq!(
                    reserved.0,
                    if (*actor == "memory_only" && !template.contains("\"evidence\""))
                        || (template.contains("\"ops\"")
                            && matches!(*actor, "viewer" | "read_only"))
                    {
                        403
                    } else {
                        404
                    },
                    "{reserved:?}"
                );
            } else {
                assert_ne!(
                    reserved.0, 404,
                    "raw holder must reach evidence: {reserved:?}"
                );
            }
        }
    }
}

#[tokio::test]
async fn deleted_reserved_names_stay_occupied_until_recreated_by_raw_holder() {
    let fixture = fixture();
    for (actor, raw, tenant) in fixture
        .tokens
        .iter()
        .filter(|(actor, _, _)| matches!(*actor, "raw" | "tenant_raw"))
    {
        let (_, _, nodes) = request(
            &fixture.app,
            raw,
            "POST",
            "/v1/graphs/notes_graph/nodes/find",
            "{}",
        )
        .await;
        let nodes: Value = serde_json::from_slice(&nodes).unwrap();
        let ops: Vec<_> = nodes["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|node| json!({"op":"delete_node","key":node["key"]}))
            .collect();
        assert_eq!(
            request(
                &fixture.app,
                raw,
                "POST",
                "/v1/graphs/notes_graph/batch",
                &json!({"ops":ops}).to_string()
            )
            .await
            .0,
            200
        );
        assert_eq!(
            request(&fixture.app, raw, "DELETE", "/v1/graphs/notes_graph", "")
                .await
                .0,
            200
        );
        assert_eq!(
            request(&fixture.app, raw, "DELETE", "/v1/collections/notes_ev", "")
                .await
                .0,
            200
        );
        let no_raw = &fixture
            .tokens
            .iter()
            .find(|(name, _, _)| *name == if *tenant { "tenant" } else { "wildcard" })
            .unwrap()
            .1;
        for token in [no_raw.as_str(), ADMIN] {
            let prefix = if *tenant && token == ADMIN {
                format!("t{TENANT}__")
            } else {
                String::new()
            };
            for (path, body, ordinary) in [
                (
                    "/v1/collections",
                    json!({"name":format!("{prefix}notes_ev"),"dimensions":2}),
                    json!({"name":format!("{prefix}ordinary"),"dimensions":2}),
                ),
                (
                    "/v1/graphs",
                    json!({"graph":format!("{prefix}notes_graph")}),
                    json!({"graph":format!("{prefix}ordinary_graph")}),
                ),
            ] {
                let reserved = request(&fixture.app, token, "POST", path, &body.to_string()).await;
                let occupied =
                    request(&fixture.app, token, "POST", path, &ordinary.to_string()).await;
                assert_eq!(reserved, occupied, "{actor}/{path} deleted reservation");
                assert_eq!(reserved.0, 409);
            }
        }
        assert_eq!(
            request(
                &fixture.app,
                raw,
                "POST",
                "/v1/collections",
                r#"{"name":"notes_ev","dimensions":2}"#
            )
            .await
            .0,
            201
        );
        assert_eq!(
            request(
                &fixture.app,
                raw,
                "POST",
                "/v1/graphs",
                r#"{"graph":"notes_graph"}"#
            )
            .await
            .0,
            201
        );
    }
}

/// Requests whose bodies change state, beside the empty-bodied ones in CASES.
const EXTRA_CASES: &[(&str, &str, &str)] = &[
    (
        "POST",
        "/v1/collections/$c/batches",
        r#"{"operations":[{"op":"put","id":"9","vector":[0,1]}]}"#,
    ),
    (
        "PATCH",
        "/v1/collections/$c/settings",
        r#"{"hnsw":{"ef_construction":123}}"#,
    ),
    (
        "PUT",
        "/v1/collections/$c/settings",
        r#"{"hnsw":{"ef_construction":124}}"#,
    ),
];

fn scrub(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("view_sequence");
            map.remove("elapsed_ms");
            for inner in map.values_mut() {
                scrub(inner);
            }
        }
        Value::Array(items) => {
            for inner in items {
                scrub(inner);
            }
        }
        _ => {}
    }
}

/// Everything a root principal can read about the root namesakes of the
/// names a tenant uses, including the reserved ones through their raw
/// holders. Database-wide counters are left out: a tenant's own writes move
/// them.
async fn root_digest(fixture: &Fixture, raw: &str, raw_other: &str) -> String {
    let mut out = String::new();
    for (actor, name) in [
        (ADMIN, "ordinary"),
        (raw_other, "other_ev"),
        (raw, "notes_ev"),
        (ADMIN, "scratch"),
    ] {
        for path in [
            format!("/v1/collections/{name}"),
            format!("/v1/collections/{name}/vectors?limit=100"),
            format!("/v1/collections/{name}/vectors/7"),
            format!("/v1/collections/{name}/vectors/8"),
        ] {
            let answer = request(&fixture.app, actor, "GET", &path, "").await;
            let mut body: Value = serde_json::from_slice(&answer.2).unwrap_or(Value::Null);
            scrub(&mut body);
            out += &format!("{path} {} {body}\n", answer.0);
        }
    }
    for (actor, name) in [
        (ADMIN, "ordinary_graph"),
        (raw_other, "other_graph"),
        (raw, "notes_graph"),
        (ADMIN, "scratch_graph"),
    ] {
        for (method, path, body) in [
            ("GET", format!("/v1/graphs/{name}"), ""),
            ("GET", format!("/v1/graphs/{name}/schema"), ""),
            ("POST", format!("/v1/graphs/{name}/nodes/find"), "{}"),
        ] {
            let answer = request(&fixture.app, actor, method, &path, body).await;
            let mut body: Value = serde_json::from_slice(&answer.2).unwrap_or(Value::Null);
            scrub(&mut body);
            out += &format!("{path} {} {body}\n", answer.0);
        }
    }
    out
}

/// Every name-addressed operation, run by a tenant credential against names
/// that also exist at root (two of them reserved there), must leave every root
/// namesake byte-identical and return none of its data. Comparing status
/// codes alone cannot see a write that lands in the wrong collection.
#[tokio::test]
async fn tenant_requests_never_change_or_read_a_root_namesake() {
    for actor in ["tenant", "tenant_raw"] {
        let fixture = fixture();
        let raw = token(&fixture, "raw").to_owned();
        let raw_other = token(&fixture, "raw_other").to_owned();
        let credential = token(&fixture, actor).to_owned();
        let record = r#"{"vector":[1,0],"payload_base64":"Uk9PVFNFQ1JFVA==","metadata":{"owner":{"type":"string","value":"rootsecret"}}}"#;
        for (who, name) in [
            (ADMIN.to_owned(), "ordinary"),
            (raw_other.clone(), "other_ev"),
            (raw.clone(), "notes_ev"),
        ] {
            for id in [7, 8] {
                let put = request(
                    &fixture.app,
                    &who,
                    "PUT",
                    &format!("/v1/collections/{name}/vectors/{id}"),
                    record,
                )
                .await;
                assert_eq!(put.0, 204, "seed {name}/{id}");
            }
        }
        for (who, name) in [
            (ADMIN.to_owned(), "ordinary_graph"),
            (raw_other.clone(), "other_graph"),
            (raw.clone(), "notes_graph"),
        ] {
            let seeded = request(
                &fixture.app,
                &who,
                "POST",
                &format!("/v1/graphs/{name}/batch"),
                r#"{"ops":[{"op":"create_node","key":"a"},{"op":"create_node","key":"n"},{"op":"create_node","key":"m"}]}"#,
            )
            .await;
            assert_eq!(seeded.0, 200, "seed {name}");
        }
        // The tenant's own namesakes, so that its writes can succeed.
        for name in ["ordinary", "other_ev", "notes_ev"] {
            for id in [7, 8] {
                request(
                    &fixture.app,
                    &credential,
                    "PUT",
                    &format!("/v1/collections/{name}/vectors/{id}"),
                    r#"{"vector":[0,1],"payload_base64":"VEVOQU5U"}"#,
                )
                .await;
            }
        }
        // Empty namesakes the tenant may drop and a root principal could lose.
        for who in [ADMIN.to_owned(), credential.clone()] {
            for (path, body) in [
                ("/v1/collections", r#"{"name":"scratch","dimensions":2}"#),
                ("/v1/graphs", r#"{"graph":"scratch_graph"}"#),
            ] {
                let created = request(&fixture.app, &who, "POST", path, body).await;
                assert_eq!(created.0, 201, "{path} for {who}");
            }
        }
        let baseline = root_digest(&fixture, &raw, &raw_other).await;
        for path in ["/v1/collections/scratch", "/v1/graphs/scratch_graph"] {
            let dropped = request(&fixture.app, &credential, "DELETE", path, "").await;
            assert_eq!(dropped.0, 200, "{actor} DELETE {path}");
            assert_eq!(
                root_digest(&fixture, &raw, &raw_other).await,
                baseline,
                "{actor} DELETE {path} changed a root namesake"
            );
        }
        // Dropping the tenant's own collection or graph goes last, so every
        // other route still has a namesake to act on.
        let mut cases: Vec<&(&str, &str, &str)> = CASES.iter().chain(EXTRA_CASES).collect();
        cases.sort_by_key(|(method, path, _)| {
            *method == "DELETE" && !path.contains("/vectors/") && !path.contains("/kv/")
        });
        let mut executed = 0;
        for (method, path, body) in cases {
            let names: Vec<&str> = if path.contains("$c") {
                vec!["ordinary", "other_ev", "notes_ev"]
            } else if path.contains("$g") {
                vec!["ordinary_graph", "other_graph", "notes_graph"]
            } else {
                vec![""]
            };
            for name in names {
                let concrete = path.replace("$c", name).replace("$g", name);
                let answer = request(&fixture.app, &credential, method, &concrete, body).await;
                let text = String::from_utf8_lossy(&answer.2).to_string();
                assert!(
                    !text.contains("ROOTSECRET")
                        && !text.contains("Uk9PVFNFQ1JFVA")
                        && !text.contains("rootsecret"),
                    "{actor} {method} {concrete} returned root data: {text}"
                );
                assert_eq!(
                    root_digest(&fixture, &raw, &raw_other).await,
                    baseline,
                    "{actor} {method} {concrete} ({}) changed a root namesake",
                    answer.0
                );
                executed += 1;
            }
        }
        assert!(executed >= 150, "{actor}: only {executed} requests ran");
    }
}

#[tokio::test]
async fn tenant_named_and_sparse_search_use_the_guarded_collection() {
    let fixture = fixture();
    assert_eq!(
        request(
            &fixture.app,
            ADMIN,
            "POST",
            "/v1/collections",
            r#"{"name":"root_only","dimensions":2}"#
        )
        .await
        .0,
        201
    );
    let raw_other = &fixture
        .tokens
        .iter()
        .find(|(actor, _, _)| *actor == "raw_other")
        .unwrap()
        .1;
    for (name, token, id) in [
        ("ordinary".to_owned(), ADMIN, 111),
        ("other_ev".to_owned(), raw_other.as_str(), 111),
        ("root_only".to_owned(), ADMIN, 111),
        (format!("t{TENANT}__ordinary"), ADMIN, 222),
        (format!("t{TENANT}__other_ev"), ADMIN, 222),
    ] {
        for (method, path, body) in [
            (
                "POST",
                format!("/v1/collections/{name}/vectors/configure"),
                r#"{"name":"v","dimensions":2,"metric":"cosine","active_version":"v1"}"#.to_owned(),
            ),
            (
                "PUT",
                format!("/v1/collections/{name}/vectors/{id}"),
                r#"{"vector":[0,1]}"#.to_owned(),
            ),
            (
                "PUT",
                format!("/v1/collections/{name}/vectors/{id}/named"),
                r#"{"dense":{"v":[{"version":"v1","vector":[0,1]}]},"sparse":{"s":[[1,1]]}}"#
                    .to_owned(),
            ),
        ] {
            let result = request(&fixture.app, token, method, &path, &body).await;
            assert!(result.0 < 300, "{path}: {result:?}");
        }
    }
    let tenant = &fixture
        .tokens
        .iter()
        .find(|(actor, _, _)| *actor == "tenant")
        .unwrap()
        .1;
    for (suffix, body) in [
        ("named", r#"{"name":"v","vector":[0,1],"k":1}"#),
        ("sparse", r#"{"name":"s","vector":[[1,1]],"k":1}"#),
    ] {
        for name in ["ordinary", "other_ev", "root_only"] {
            let result = request(
                &fixture.app,
                tenant,
                "POST",
                &format!("/v1/collections/{name}/search/{suffix}"),
                body,
            )
            .await;
            if name == "root_only" {
                assert_eq!(
                    result,
                    request(
                        &fixture.app,
                        tenant,
                        "POST",
                        &format!("/v1/collections/absent_ev/search/{suffix}"),
                        body
                    )
                    .await
                );
                assert_eq!(result.0, 404);
            } else {
                assert_eq!(result.0, 200, "{result:?}");
                let value: Value = serde_json::from_slice(&result.2).unwrap();
                assert_eq!(value["hits"][0]["id"], "222", "{name}/{suffix}: {value}");
            }
        }
    }
}
