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
        json!([{"id":TENANT,"name":"fake tenant","max_vectors":null}]).to_string(),
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
                    if *actor == "memory_only"
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
