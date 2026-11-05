#![cfg(feature = "server")]

//! The reserved-resource differential, generated instead of listed (G71 S5).
//!
//! The operations come from the OpenAPI document the server serves, and the
//! request bodies from `ROWS`, because a schema cannot say which body is worth
//! sending. A name-bearing operation without a row fails the inventory, so a
//! new route cannot ship unchecked. For each of twelve principals the matrix
//! holds three things:
//!
//! * a reserved name answers byte for byte like a missing one (like an
//!   occupied one when the request creates the name), at root and in the
//!   tenant's namespace;
//! * with bindings configured, ordinary and missing names answer like the same
//!   request on a database without bindings;
//! * after every request that is not a GET, every reserved resource the
//!   principal does not hold (and, for a tenant, every root namesake of the
//!   names it uses) reads back byte for byte what it read before. Status
//!   codes alone cannot see a write that lands in the wrong collection.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::{
    Router,
    body::Body,
    http::{HeaderMap, Request},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use vlite::{
    CollectionSpec, CreateOptions, Database,
    server::{BearerToken, ServerOptions, Service, configured_admin_router},
};

const ADMIN: &str = "fake_guard_differential_admin_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const READ_ONLY: &str = "fake_guard_differential_readonly_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const TENANT: &str = "0123456789abcdef0123456789abcdef";
const PROBE_PASSWORD: &str = "fake-testing-password-not-real";
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

type Answer = (u16, HeaderMap, Vec<u8>);

struct Principal {
    name: &'static str,
    tenant: bool,
    /// The reserved resources (labels of `RESERVED`) whose raw access it holds.
    held: &'static [&'static str],
    /// The console role when it signs in with a session rather than a token.
    session: Option<&'static str>,
}

const PRINCIPALS: [Principal; 12] = [
    Principal {
        name: "wildcard",
        tenant: false,
        held: &[],
        session: None,
    },
    Principal {
        name: "memory_only",
        tenant: false,
        held: &[],
        session: None,
    },
    Principal {
        name: "raw_other",
        tenant: false,
        held: &["reserved.root.other"],
        session: None,
    },
    Principal {
        name: "raw",
        tenant: false,
        held: &["reserved.root.notes"],
        session: None,
    },
    Principal {
        name: "tenant",
        tenant: true,
        held: &[],
        session: None,
    },
    Principal {
        name: "tenant_raw",
        tenant: true,
        held: &["reserved.tenant.notes"],
        session: None,
    },
    Principal {
        name: "admin",
        tenant: false,
        held: &[],
        session: None,
    },
    Principal {
        name: "read_only",
        tenant: false,
        held: &[],
        session: None,
    },
    Principal {
        name: "viewer",
        tenant: false,
        held: &[],
        session: Some("viewer"),
    },
    Principal {
        name: "editor",
        tenant: false,
        held: &[],
        session: Some("editor"),
    },
    Principal {
        name: "tenant_viewer",
        tenant: true,
        held: &[],
        session: Some("viewer"),
    },
    Principal {
        name: "tenant_editor",
        tenant: true,
        held: &[],
        session: Some("editor"),
    },
];

/// The reserved resources: `(label, raw holder, collection, graph, marker)`.
/// Names are the holder's own, so the tenant's `notes_ev` is unqualified.
const RESERVED: [(&str, &str, &str, &str, &str); 3] = [
    (
        "reserved.root.notes",
        "raw",
        "notes_ev",
        "notes_graph",
        "RSVROOTNOTES",
    ),
    (
        "reserved.root.other",
        "raw_other",
        "other_ev",
        "other_graph",
        "RSVROOTOTHER",
    ),
    (
        "reserved.tenant.notes",
        "tenant_raw",
        "notes_ev",
        "notes_graph",
        "RSVTENANTNOTES",
    ),
];

/// What every world starts with: `(writer with bindings, writer without,
/// collection, graph, marker)`. A marker is text no other principal may ever
/// read back.
const SEEDS: [(&str, &str, &str, &str, &str); 6] = [
    (
        "admin",
        "admin",
        "ordinary",
        "ordinary_graph",
        "ROOTORDINARY",
    ),
    ("raw", "admin", "notes_ev", "notes_graph", "RSVROOTNOTES"),
    (
        "raw_other",
        "admin",
        "other_ev",
        "other_graph",
        "RSVROOTOTHER",
    ),
    (
        "tenant_raw",
        "tenant",
        "notes_ev",
        "notes_graph",
        "RSVTENANTNOTES",
    ),
    (
        "tenant",
        "tenant",
        "ordinary",
        "ordinary_graph",
        "TENANTORDINARY",
    ),
    (
        "tenant",
        "tenant",
        "other_ev",
        "other_graph",
        "TENANTORDINARY",
    ),
];

struct World {
    app: Router,
    bound: bool,
    tokens: BTreeMap<&'static str, String>,
    _directory: tempfile::TempDir,
}

impl World {
    fn token(&self, principal: &str) -> &str {
        self.tokens
            .get(principal)
            .unwrap_or_else(|| panic!("no credential for {principal}"))
    }
}

fn build(bound: bool) -> World {
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
        // Key-value routes need a collection of exactly one dimension.
        db.create_collection(
            &format!("{prefix}kvstore"),
            CollectionSpec {
                dimensions: 1,
                settings: Default::default(),
            },
        )
        .unwrap();
    }
    let mut tokens: BTreeMap<&'static str, String> = BTreeMap::new();
    tokens.insert("admin", ADMIN.to_owned());
    tokens.insert("read_only", READ_ONLY.to_owned());
    let credentials: Vec<Value> = [
        ("wildcard", "abcdefghij22", false, true),
        ("memory_only", "abcdefghij23", false, false),
        ("raw_other", "abcdefghij24", false, true),
        ("raw", "abcdefghij25", false, true),
        ("tenant", "abcdefghij26", true, true),
        ("tenant_raw", "abcdefghij27", true, true),
    ]
    .into_iter()
    .map(|(actor, id, tenant, all)| {
        let secret = "A".repeat(43);
        tokens.insert(actor, format!("vlk_{id}_{secret}"));
        json!({"id":id,"name":actor,"all":all,
            "collections":if all { json!([]) } else { json!([{"name":"ordinary","id":db.collection("ordinary").unwrap().id()}]) },
            "graphs":if all { vec!["*"] } else { vec!["ordinary_graph"] },"operations":OPS,
            "lineage":format!("{:016x}",db.lineage().unwrap()),
            "secret_sha256":format!("{:x}",Sha256::digest(secret.as_bytes())),
            "created_unix_ms":0,"expires_unix_ms":9_999_999_999_999u64,
            "revoked_unix_ms":null,"rotated_from":null,
            "tenant_id":tenant.then_some(TENANT)})
    })
    .collect();
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
            memory_bindings_file: bound.then_some(path),
            ..Default::default()
        },
    )
    .unwrap();
    World {
        app,
        bound,
        tokens,
        _directory: directory,
    }
}

async fn request(app: &Router, actor: &str, method: &str, path: &str, body: &str) -> Answer {
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

async fn sign_in(world: &mut World, name: &'static str, role: &str, tenant: bool) {
    let username = format!("{}-user", name.replace('_', "-"));
    let created = request(
        &world.app,
        ADMIN,
        "POST",
        "/v1/admin/users",
        &json!({"username":username,"password":PROBE_PASSWORD,"role":role,
            "tenant_id":tenant.then_some(TENANT),"must_change_password":false})
        .to_string(),
    )
    .await;
    assert_eq!(created.0, 201, "{created:?}");
    let response = world
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
                    json!({"username":username,"password":PROBE_PASSWORD}).to_string(),
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
    world.tokens.insert(name, cookie);
}

async fn seed(world: &World) {
    for (bound_writer, unbound_writer, collection, graph, marker) in SEEDS {
        let writer = world.token(if world.bound {
            bound_writer
        } else {
            unbound_writer
        });
        for id in [7, 8] {
            let put = request(
                &world.app,
                writer,
                "PUT",
                &format!("/v1/collections/{collection}/vectors/{id}"),
                &json!({"vector":[1,0],"payload_base64":STANDARD.encode(marker),
                    "metadata":{"owner":{"type":"string","value":marker.to_ascii_lowercase()}}})
                .to_string(),
            )
            .await;
            assert_eq!(put.0, 204, "seed {collection}/{id}: {}", describe(&put));
        }
        let nodes = request(
            &world.app,
            writer,
            "POST",
            &format!("/v1/graphs/{graph}/batch"),
            &json!({"ops":[{"op":"create_node","key":"a"},{"op":"create_node","key":"n"},
                {"op":"create_node","key":"m"},
                {"op":"create_node","key":marker.to_ascii_lowercase()}]})
            .to_string(),
        )
        .await;
        assert_eq!(nodes.0, 200, "seed {graph}: {}", describe(&nodes));
    }
    for (writer, marker) in [("admin", "ROOTORDINARY"), ("tenant", "TENANTORDINARY")] {
        let entry = request(
            &world.app,
            world.token(writer),
            "PUT",
            "/v1/collections/kvstore/kv/entry",
            &json!({"namespace":"n","key":"k","value_base64":STANDARD.encode(marker)}).to_string(),
        )
        .await;
        assert!(
            (200..300).contains(&entry.0),
            "seed kv {writer}: {}",
            describe(&entry)
        );
    }
}

async fn prepared(bound: bool, principal: &Principal) -> World {
    let mut world = build(bound);
    if let Some(role) = principal.session {
        sign_in(&mut world, principal.name, role, principal.tenant).await;
    }
    seed(&world).await;
    world
}

/// A finished rebuild worker keeps the database-wide rebuild slot until it is
/// polled, so a request that starts one is followed by a join; otherwise the
/// next request for any other collection answers `rebuild_busy` or not,
/// depending only on how fast the worker exited.
async fn join_rebuild_worker(world: &World, token: &str, base: &str) {
    let _ = request(&world.app, token, "POST", &format!("{base}/cancel"), "{}").await;
    for _ in 0..100_000 {
        let polled = request(&world.app, token, "POST", &format!("{base}/poll"), "{}").await;
        let outcome = serde_json::from_slice::<Value>(&polled.2)
            .ok()
            .and_then(|value| value["outcome"].as_str().map(str::to_owned));
        if polled.0 != 200 || !matches!(outcome.as_deref(), Some("running" | "cancelling")) {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the rebuild worker behind {base} never finished");
}

async fn exec(world: &World, token: &str, method: &str, path: &str, body: &str) -> Answer {
    let answer = request(&world.app, token, method, path, body).await;
    if method == "POST" && (200..300).contains(&answer.0) {
        let bare = path.split('?').next().unwrap_or(path);
        if let Some(at) = bare.find("/index/hnsw/") {
            let tail = &bare[at..];
            if tail == "/index/hnsw/rebuild" || tail == "/index/hnsw/maintenance/tick" {
                join_rebuild_worker(world, token, &format!("{}/index/hnsw/rebuild", &bare[..at]))
                    .await;
            }
        }
    }
    answer
}

// ---------------------------------------------------------------- the rows

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// Reads or writes the named resource: a hidden name answers like a
    /// missing one.
    Reference,
    /// Creates the named resource: a hidden name answers like an occupied one.
    Create,
    /// Names a resource only as a filter or grant pattern: a hidden name is
    /// accepted or refused exactly like a missing one, by status.
    Pattern,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Effect {
    Unchecked,
    /// The request must succeed when nothing is hidden, or its digest checks
    /// prove nothing.
    Succeeds,
}

/// `$c` and `$g` are the names the request is about, `$o` and `$og` an
/// ordinary collection and graph of the same namespace, `$n` a counter and
/// `$expires` a future time.
struct Row {
    method: &'static str,
    path: &'static str,
    body: &'static str,
    kind: Kind,
    effect: Effect,
    lockstep: bool,
}

const fn row(method: &'static str, path: &'static str, body: &'static str) -> Row {
    Row {
        method,
        path,
        body,
        kind: Kind::Reference,
        effect: Effect::Unchecked,
        lockstep: true,
    }
}

const fn effective(method: &'static str, path: &'static str, body: &'static str) -> Row {
    Row {
        method,
        path,
        body,
        kind: Kind::Reference,
        effect: Effect::Succeeds,
        lockstep: true,
    }
}

const fn creating(method: &'static str, path: &'static str, body: &'static str) -> Row {
    Row {
        method,
        path,
        body,
        kind: Kind::Create,
        effect: Effect::Succeeds,
        lockstep: true,
    }
}

const fn pattern(method: &'static str, path: &'static str, body: &'static str) -> Row {
    Row {
        method,
        path,
        body,
        kind: Kind::Pattern,
        effect: Effect::Succeeds,
        lockstep: true,
    }
}

/// With bindings configured every import is refused, ordinary names included,
/// so bound and unbound answers legitimately differ.
const fn refused_while_bound(method: &'static str, path: &'static str, body: &'static str) -> Row {
    Row {
        method,
        path,
        body,
        kind: Kind::Reference,
        effect: Effect::Unchecked,
        lockstep: false,
    }
}

const RECORD: &str = r#"{"vector":[0,1],"payload_base64":"UFJPQkU=","metadata":{"probe":{"type":"string","value":"touched"}}}"#;
const PATCH: &str = r#"{"metadata":{"probe":{"type":"string","value":"patched"}}}"#;
/// Key-value writes only succeed on a collection of one dimension (`kvstore`).
const KV_PUT: &str = r#"{"namespace":"n","key":"k","value_base64":"UFJPQkU="}"#;

/// Deletions run last, in table order, so every other row still has its
/// resource to act on.
const ROWS: &[Row] = &[
    // Collections, named in the path.
    effective("GET", "/v1/collections/$c", ""),
    row("GET", "/v1/collections/$c/vectors", ""),
    row("GET", "/v1/collections/$c/vectors/7", ""),
    effective("PUT", "/v1/collections/$c/vectors/7", RECORD),
    effective("PATCH", "/v1/collections/$c/vectors/7/metadata", PATCH),
    row(
        "POST",
        "/v1/collections/$c/vectors/configure",
        r#"{"name":"v","dimensions":2,"metric":"cosine","active_version":"v1"}"#,
    ),
    row(
        "PUT",
        "/v1/collections/$c/vectors/7/named",
        r#"{"dense":{"v":[{"version":"v1","vector":[0,1]}]},"sparse":{"s":[[1,1]]}}"#,
    ),
    effective(
        "POST",
        "/v1/collections/$c/batches",
        r#"{"operations":[{"op":"put","id":"9","vector":[0,1]}]}"#,
    ),
    row(
        "POST",
        "/v1/collections/$c/search",
        r#"{"vector":[0,1],"k":1}"#,
    ),
    row(
        "POST",
        "/v1/collections/$c/search/grouped",
        r#"{"vector":[0,1],"field":"tag","groups":1,"hits_per_group":1}"#,
    ),
    row(
        "POST",
        "/v1/collections/$c/search/geo",
        r#"{"vector":[0,1],"k":1,"field":"location","latitude":0,"longitude":0,"radius_meters":1}"#,
    ),
    row(
        "POST",
        "/v1/collections/$c/recommend",
        r#"{"positive_ids":[],"k":1}"#,
    ),
    row("POST", "/v1/collections/$c/facets", r#"{"field":"tag"}"#),
    row(
        "POST",
        "/v1/collections/$c/stats/numeric",
        r#"{"field":"score"}"#,
    ),
    row("POST", "/v1/collections/$c/query", r#"{"query":"*"}"#),
    row(
        "POST",
        "/v1/collections/$c/embed",
        r#"{"text":"fake text"}"#,
    ),
    row(
        "POST",
        "/v1/collections/$c/search/named",
        r#"{"name":"v","vector":[0,1],"k":1}"#,
    ),
    row(
        "POST",
        "/v1/collections/$c/search/sparse",
        r#"{"name":"v","vector":[],"k":1}"#,
    ),
    row(
        "POST",
        "/v1/collections/$c/search/dense-sparse",
        r#"{"dense_name":"v","dense":[0,1],"sparse_name":"s","sparse":[],"k":1}"#,
    ),
    effective(
        "PUT",
        "/v1/collections/$c/settings",
        r#"{"hnsw":{"ef_construction":124}}"#,
    ),
    row("GET", "/v1/collections/$c/kv/entry?namespace=n&key=k", ""),
    row("PUT", "/v1/collections/$c/kv/entry", KV_PUT),
    row("GET", "/v1/collections/$c/kv/entries?namespace=n", ""),
    row("GET", "/v1/collections/$c/index/hnsw", ""),
    row("POST", "/v1/collections/$c/index/hnsw", "{}"),
    row("GET", "/v1/collections/$c/index/hnsw/maintenance", ""),
    row("POST", "/v1/collections/$c/index/hnsw/maintenance/tick", ""),
    row("GET", "/v1/collections/$c/index/hnsw/rebuild", ""),
    row("POST", "/v1/collections/$c/index/hnsw/rebuild", "{}"),
    row("POST", "/v1/collections/$c/index/hnsw/rebuild/cancel", "{}"),
    row("POST", "/v1/collections/$c/index/hnsw/rebuild/poll", "{}"),
    row("POST", "/v1/collections/$c/index/hnsw/quality", "{}"),
    row("POST", "/v1/collections/$c/index/disk-ann", "{}"),
    row(
        "POST",
        "/v1/collections/$c/search/disk-ann",
        r#"{"vector":[0,1],"k":1,"ef_search":1}"#,
    ),
    refused_while_bound("POST", "/v1/collections/$c/imports", ""),
    row(
        "DELETE",
        "/v1/collections/$c/kv/entry?namespace=n&key=k",
        "",
    ),
    row("DELETE", "/v1/collections/$c/vectors/7", ""),
    row("DELETE", "/v1/collections/$c", ""),
    // Graphs, named in the path.
    effective("GET", "/v1/graphs/$g", ""),
    row("GET", "/v1/graphs/$g/schema", ""),
    row("PUT", "/v1/graphs/$g/schema", r#"{"schema":{}}"#),
    row(
        "POST",
        "/v1/graphs/$g/nodes/get",
        r#"{"node":{"key":"n"},"limits":{"deadline_ms":5000}}"#,
    ),
    row(
        "POST",
        "/v1/graphs/$g/edges/get",
        r#"{"edge":{"key":"e"},"limits":{"deadline_ms":5000}}"#,
    ),
    row(
        "POST",
        "/v1/graphs/$g/nodes/find",
        r#"{"limits":{"deadline_ms":5000}}"#,
    ),
    row(
        "POST",
        "/v1/graphs/$g/neighbors",
        r#"{"node":{"key":"n"},"limits":{"deadline_ms":5000}}"#,
    ),
    row(
        "POST",
        "/v1/graphs/$g/traverse",
        r#"{"start":[{"key":"n"}],"limits":{"deadline_ms":5000}}"#,
    ),
    row(
        "POST",
        "/v1/graphs/$g/shortest-path",
        r#"{"source":{"key":"n"},"destination":{"key":"m"},"limits":{"deadline_ms":5000}}"#,
    ),
    row(
        "POST",
        "/v1/graphs/$g/explain",
        r#"{"operation":"find-nodes"}"#,
    ),
    row(
        "POST",
        "/v1/graphs/$g/query",
        r#"{"query":"MATCH (n) RETURN n LIMIT 1"}"#,
    ),
    row(
        "POST",
        "/v1/graphs/$g/retrieve",
        r#"{"pivot":{"collection":"$o","vector":[0,1],"k":1}}"#,
    ),
    effective(
        "POST",
        "/v1/graphs/$g/batch",
        r#"{"ops":[{"op":"create_node","key":"probe_node"}]}"#,
    ),
    row(
        "POST",
        "/v1/graphs/$g/batch",
        r#"{"ops":[{"op":"delete_node","key":"a"}]}"#,
    ),
    row("POST", "/v1/graphs/$g/batch", r#"{"ops":[]}"#),
    row("POST", "/v1/graphs/$g/batch", "{}"),
    row("POST", "/v1/graphs/$g/nodes/get", "{}"),
    row("POST", "/v1/graphs/$g/query", r#"{"query":"bad query"}"#),
    row("GET", "/v1/graphs/$g/schema?budget=bad", ""),
    row("DELETE", "/v1/graphs/$g", ""),
    // An ordinary graph in the path, a collection in the body.
    row(
        "POST",
        "/v1/graphs/$og/retrieve",
        r#"{"pivot":{"collection":"$c","vector":[0,1],"k":1}}"#,
    ),
    row(
        "POST",
        "/v1/graphs/$og/retrieve",
        r#"{"pivot":{"collection":"$o","vector":[0,1],"k":1},"evidence":{"collections":["$c"]}}"#,
    ),
    row(
        "POST",
        "/v1/graphs/$og/batch",
        r#"{"ops":[{"op":"attach","node":"n","collection":"$c","record":"7"}]}"#,
    ),
    row(
        "POST",
        "/v1/graphs/$og/batch",
        r#"{"ops":[{"op":"detach","node":"n","collection":"$c"}]}"#,
    ),
    row(
        "POST",
        "/v1/graphs/$og/batch",
        r#"{"ops":[{"op":"put_vector","collection":"$c","id":"11","vector":[0,1]}]}"#,
    ),
    row(
        "POST",
        "/v1/graphs/$og/batch",
        r#"{"ops":[{"op":"delete_vector","collection":"$c","id":"7"}]}"#,
    ),
    // The graph is the name that varies, the collection is ordinary.
    row(
        "POST",
        "/v1/graphs/$g/batch",
        r#"{"ops":[{"op":"attach","node":"n","collection":"$o","record":"7"}]}"#,
    ),
    row(
        "POST",
        "/v1/graphs/$g/batch",
        r#"{"ops":[{"op":"detach","node":"n","collection":"$o"}]}"#,
    ),
    row(
        "POST",
        "/v1/graphs/$g/batch",
        r#"{"ops":[{"op":"put_vector","collection":"$o","id":"12","vector":[0,1]}]}"#,
    ),
    row(
        "POST",
        "/v1/graphs/$g/batch",
        r#"{"ops":[{"op":"delete_vector","collection":"$o","id":"8"}]}"#,
    ),
    // Both vary together.
    row(
        "POST",
        "/v1/graphs/$g/batch",
        r#"{"ops":[{"op":"attach","node":"n","collection":"$c","record":"7"}]}"#,
    ),
    row(
        "POST",
        "/v1/graphs/$g/batch",
        r#"{"ops":[{"op":"detach","node":"n","collection":"$c"}]}"#,
    ),
    row(
        "POST",
        "/v1/graphs/$g/batch",
        r#"{"ops":[{"op":"put_vector","collection":"$c","id":"13","vector":[0,1]}]}"#,
    ),
    row(
        "POST",
        "/v1/graphs/$g/batch",
        r#"{"ops":[{"op":"delete_vector","collection":"$c","id":"8"}]}"#,
    ),
    // Names in bodies that create or grant.
    creating("POST", "/v1/collections", r#"{"name":"$c","dimensions":2}"#),
    creating("POST", "/v1/graphs", r#"{"graph":"$g"}"#),
    row(
        "POST",
        "/v1/admin/credentials",
        r#"{"name":"fake-probe","collections":["$c"],"operations":["read"],"expires_unix_ms":$expires}"#,
    ),
    pattern(
        "POST",
        "/v1/admin/webhooks",
        r#"{"url":"http://127.0.0.1:9/fake-hook","secret":"fake-secret-test-only","collections":["$c"]}"#,
    ),
    pattern(
        "POST",
        "/v1/admin/webhooks",
        r#"{"url":"http://127.0.0.1:9/fake-hook","secret":"fake-secret-test-only","graphs":["$g"]}"#,
    ),
    pattern(
        "POST",
        "/v1/admin/users",
        r#"{"username":"probe-$n","password":"fake-testing-password-not-real","role":"viewer","graphs":["$g"],"must_change_password":false}"#,
    ),
];

/// Operations the OpenAPI document marks as naming a collection or graph that
/// stay out of the matrix on purpose, each with the reason. Paths are in the
/// normalized form of `normalize`: every parameter but the collection and the
/// graph reads `{id}`.
const EXEMPT: &[(&str, &str, &str)] = &[
    (
        "PUT",
        "/v1/admin/users/{id}",
        "its graphs are grant patterns checked for shape only, exactly like POST /v1/admin/users, which the matrix covers",
    ),
    (
        "POST",
        "/v1/memory/{id}/remember",
        "evidence collections are the binding's own and resolved from the binding; tests/server_memory_*.rs own these routes",
    ),
    (
        "POST",
        "/v1/memory/{id}/consolidate",
        "evidence collections are the binding's own and resolved from the binding; tests/server_memory_*.rs own these routes",
    ),
    (
        "POST",
        "/v1/memory/{id}/facts/correct",
        "evidence collections are the binding's own and resolved from the binding; tests/server_memory_*.rs own these routes",
    ),
];

/// Routes the router serves that `openapi.json` does not document yet; none
/// today. A row for one belongs here, so that it is neither an orphan nor
/// forgotten once the document describes it.
const UNDOCUMENTED: &[(&str, &str)] = &[];

/// Operations that name the resource they create in a body field the walker
/// does not treat as a reference.
const CREATING: &[(&str, &str)] = &[("POST", "/v1/collections"), ("POST", "/v1/graphs")];

fn ordered() -> impl Iterator<Item = &'static Row> {
    ROWS.iter()
        .filter(|row| row.method != "DELETE")
        .chain(ROWS.iter().filter(|row| row.method == "DELETE"))
}

static PROBES: AtomicUsize = AtomicUsize::new(0);

fn expires() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 3_600_000
}

#[derive(Clone)]
struct Names {
    c: String,
    g: String,
    o: String,
    og: String,
}

fn names(prefix: &str, collection: &str, graph: &str) -> Names {
    Names {
        c: format!("{prefix}{collection}"),
        g: format!("{prefix}{graph}"),
        o: format!("{prefix}ordinary"),
        og: format!("{prefix}ordinary_graph"),
    }
}

fn fill(text: &str, names: &Names) -> String {
    let mut out = text
        .replace("$og", &names.og)
        .replace("$o", &names.o)
        .replace("$c", &names.c)
        .replace("$g", &names.g);
    if out.contains("$expires") {
        out = out.replace("$expires", &expires().to_string());
    }
    if out.contains("$n") {
        out = out.replace("$n", &PROBES.fetch_add(1, Ordering::Relaxed).to_string());
    }
    out
}

impl Row {
    fn fill(&self, names: &Names) -> (String, String) {
        (fill(self.path, names), fill(self.body, names))
    }
}

/// The names one reserved resource is probed under, and the names that must
/// answer for it.
struct Variant {
    reserved: Names,
    missing: Names,
    occupied: Names,
    held: bool,
}

fn variant(prefix: &str, collection: &str, graph: &str, held: bool) -> Variant {
    let reserved = names(prefix, collection, graph);
    let occupied = Names {
        c: reserved.o.clone(),
        g: reserved.og.clone(),
        o: reserved.o.clone(),
        og: reserved.og.clone(),
    };
    Variant {
        missing: names(prefix, "absent_ev", "absent_graph"),
        occupied,
        reserved,
        held,
    }
}

fn variants(principal: &Principal) -> Vec<Variant> {
    let holds = |label: &str| principal.held.contains(&label);
    if principal.tenant {
        vec![variant(
            "",
            "notes_ev",
            "notes_graph",
            holds("reserved.tenant.notes"),
        )]
    } else {
        vec![
            variant("", "notes_ev", "notes_graph", holds("reserved.root.notes")),
            variant("", "other_ev", "other_graph", holds("reserved.root.other")),
            variant(&format!("t{TENANT}__"), "notes_ev", "notes_graph", false),
        ]
    }
}

/// Names a tenant shares with root resources, one reserved there and one
/// ordinary: whatever it does with them must stay in its own namespace.
fn namesakes(principal: &Principal) -> Vec<Names> {
    if principal.tenant {
        vec![
            names("", "other_ev", "other_graph"),
            names("", "ordinary", "ordinary_graph"),
            names("", "kvstore", "absent_graph"),
        ]
    } else {
        Vec::new()
    }
}

/// Ordinary and missing names for the bound-versus-unbound comparison; the
/// flag says the name exists, so creating it is refused in both worlds.
fn lockstep_names(principal: &Principal) -> Vec<(Names, bool)> {
    let tenant_prefix = format!("t{TENANT}__");
    if principal.tenant {
        vec![
            (names("", "ordinary", "ordinary_graph"), true),
            (names("", "other_ev", "other_graph"), true),
            (names("", "kvstore", "absent_graph"), true),
            (names("", "absent_ev", "absent_graph"), false),
        ]
    } else {
        vec![
            (names("", "ordinary", "ordinary_graph"), true),
            (names(&tenant_prefix, "ordinary", "ordinary_graph"), true),
            (names("", "kvstore", "absent_graph"), true),
            (names(&tenant_prefix, "kvstore", "absent_graph"), true),
            (names("", "absent_ev", "absent_graph"), false),
            (names(&tenant_prefix, "absent_ev", "absent_graph"), false),
        ]
    }
}

fn describe(answer: &Answer) -> String {
    format!("{} {}", answer.0, String::from_utf8_lossy(&answer.2))
}

/// Whether the principal's grants name the collection or graph; only the
/// credential bound to `ordinary` and `ordinary_graph` is limited to names.
fn granted(principal: &Principal, name: &str) -> bool {
    principal.name != "memory_only" || name == "ordinary" || name == "ordinary_graph"
}

fn error_of(answer: &Answer) -> Value {
    serde_json::from_slice::<Value>(&answer.2)
        .map(|value| value["error"].clone())
        .unwrap_or(Value::Null)
}

/// Same status and, for a failure, the same error; a success body is not
/// compared because counters and timings differ legitimately.
fn same_outcome(actual: &Answer, reference: &Answer) -> bool {
    if actual.0 != reference.0 {
        return false;
    }
    if actual.2.is_empty() || reference.2.is_empty() {
        return actual.2 == reference.2;
    }
    actual.0 < 400 || error_of(actual) == error_of(reference)
}

// ---------------------------------------------------------------- the digest

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

fn rendered(label: &str, answer: &Answer) -> String {
    let mut body = serde_json::from_slice::<Value>(&answer.2)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&answer.2).into_owned()));
    scrub(&mut body);
    format!("{label} {} {body}\n", answer.0)
}

/// Everything a holder can read about one collection and graph.
async fn read_resource(world: &World, reader: &str, collection: &str, graph: &str) -> String {
    let token = world.token(reader);
    let mut out = String::new();
    for path in [
        format!("/v1/collections/{collection}"),
        format!("/v1/collections/{collection}/vectors?limit=100"),
        format!("/v1/collections/{collection}/vectors/7"),
        format!("/v1/collections/{collection}/vectors/8"),
        format!("/v1/collections/{collection}/kv/entries?namespace=n"),
    ] {
        out += &rendered(&path, &request(&world.app, token, "GET", &path, "").await);
    }
    for (method, path, body) in [
        ("GET", format!("/v1/graphs/{graph}"), ""),
        ("GET", format!("/v1/graphs/{graph}/schema"), ""),
        (
            "POST",
            format!("/v1/graphs/{graph}/nodes/find"),
            r#"{"limits":{"deadline_ms":5000}}"#,
        ),
    ] {
        out += &rendered(
            &path,
            &request(&world.app, token, method, &path, body).await,
        );
    }
    out
}

/// The key-value entries of a one-dimensional collection.
async fn read_kv(world: &World, reader: &str, collection: &str) -> String {
    let token = world.token(reader);
    let mut out = String::new();
    for path in [
        format!("/v1/collections/{collection}"),
        format!("/v1/collections/{collection}/kv/entries?namespace=n"),
        format!("/v1/collections/{collection}/kv/entry?namespace=n&key=k"),
    ] {
        out += &rendered(&path, &request(&world.app, token, "GET", &path, "").await);
    }
    out
}

fn collect_names(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(name)) = map.get("name") {
                out.insert(name.clone());
            }
            for inner in map.values() {
                collect_names(inner, out);
            }
        }
        Value::Array(items) => {
            for inner in items {
                collect_names(inner, out);
            }
        }
        _ => {}
    }
}

/// The root namespace's collection and graph names, without the tenant's.
async fn root_lists(world: &World) -> String {
    let mut out = String::new();
    for path in ["/v1/collections", "/v1/graphs"] {
        let answer = request(&world.app, ADMIN, "GET", path, "").await;
        let mut found = BTreeSet::new();
        collect_names(
            &serde_json::from_slice::<Value>(&answer.2).unwrap_or(Value::Null),
            &mut found,
        );
        found.retain(|name| !name.starts_with(&format!("t{TENANT}__")));
        out += &format!("{path} {} {found:?}\n", answer.0);
    }
    out
}

/// The protected state, per label: the reserved resources through their raw
/// holders, and the root namesakes a tenant's names could reach.
async fn snapshot(world: &World) -> BTreeMap<&'static str, String> {
    let mut out = BTreeMap::new();
    for (label, holder, collection, graph, _) in RESERVED {
        out.insert(label, read_resource(world, holder, collection, graph).await);
    }
    out.insert(
        "root.ordinary",
        read_resource(world, "admin", "ordinary", "ordinary_graph").await,
    );
    out.insert("root.kvstore", read_kv(world, "admin", "kvstore").await);
    out.insert("root.lists", root_lists(world).await);
    out
}

/// A principal may change what it holds, and a root principal may change the
/// root namespace; nothing else.
fn protected(principal: &Principal, label: &str) -> bool {
    if principal.held.contains(&label) {
        return false;
    }
    label.starts_with("reserved.") || principal.tenant
}

fn forms(marker: &str) -> Vec<String> {
    vec![
        marker.to_owned(),
        marker.to_ascii_lowercase(),
        STANDARD.encode(marker).trim_end_matches('=').to_owned(),
    ]
}

fn forbidden_markers(principal: &Principal) -> Vec<String> {
    let mut out = Vec::new();
    for (label, _, _, _, marker) in RESERVED {
        if !principal.held.contains(&label) {
            out.extend(forms(marker));
        }
    }
    if principal.tenant {
        out.extend(forms("ROOTORDINARY"));
    }
    out
}

// ---------------------------------------------------------------- the runs

struct Run<'a> {
    world: &'a World,
    principal: &'a Principal,
    token: String,
    markers: Vec<String>,
    last: BTreeMap<&'static str, String>,
    report: Vec<String>,
    executed: usize,
}

impl<'a> Run<'a> {
    async fn new(world: &'a World, principal: &'a Principal) -> Run<'a> {
        Run {
            world,
            principal,
            token: world.token(principal.name).to_owned(),
            markers: forbidden_markers(principal),
            last: snapshot(world).await,
            report: Vec::new(),
            executed: 0,
        }
    }

    async fn send(&mut self, row: &Row, names: &Names) -> Answer {
        let (path, body) = row.fill(names);
        let answer = exec(self.world, &self.token, row.method, &path, &body).await;
        self.executed += 1;
        let text = String::from_utf8_lossy(&answer.2).into_owned();
        for marker in &self.markers {
            if text.contains(marker.as_str()) {
                self.report.push(format!(
                    "{}: {} {path} returned data it must not see ({marker}): {text}",
                    self.principal.name, row.method
                ));
            }
        }
        if row.method != "GET" {
            let after = snapshot(self.world).await;
            for (label, now) in &after {
                if protected(self.principal, label) && self.last[label] != *now {
                    self.report.push(format!(
                        "{}: {} {path} (answered {}) changed {label}",
                        self.principal.name, row.method, answer.0
                    ));
                }
            }
            self.last = after;
        }
        answer
    }

    async fn probe(&mut self, row: &Row, variant: &Variant) {
        let hidden = self.send(row, &variant.reserved).await;
        // A credential limited to one name is refused every other name before
        // anything is looked up, so a name it may not create answers like any
        // other name it may not create, and nothing is created.
        let twin_names = if row.kind == Kind::Create && self.principal.name != "memory_only" {
            &variant.occupied
        } else {
            &variant.missing
        };
        let twin = self.send(row, twin_names).await;
        if hidden.0 >= 500 || twin.0 >= 500 {
            self.report.push(format!(
                "{}: {} {} answered a server error: [{}] / [{}]",
                self.principal.name,
                row.method,
                row.path,
                describe(&hidden),
                describe(&twin)
            ));
        }
        if variant.held {
            if row.method == "GET"
                && (row.path == "/v1/collections/$c" || row.path == "/v1/graphs/$g")
                && hidden.0 != 200
            {
                self.report.push(format!(
                    "{}: a raw holder must reach {}: {}",
                    self.principal.name,
                    variant.reserved.c,
                    describe(&hidden)
                ));
            }
            return;
        }
        let same = if row.kind == Kind::Pattern {
            hidden.0 == twin.0
        } else {
            hidden == twin
        };
        if !same {
            self.report.push(format!(
                "{}: {} {} with {} answered [{}] but with {} answered [{}]",
                self.principal.name,
                row.method,
                row.path,
                variant.reserved.c,
                describe(&hidden),
                if row.kind == Kind::Create {
                    "an occupied name"
                } else {
                    "a missing name"
                },
                describe(&twin)
            ));
        }
        if self.principal.name == "admin"
            && row.kind == Kind::Pattern
            && row.path == "/v1/admin/webhooks"
            && (hidden.0 != 201 || twin.0 != 201)
        {
            self.report.push(format!(
                "admin: a webhook filter naming a hidden or a missing resource is accepted: {} / {}",
                describe(&hidden),
                describe(&twin)
            ));
        }
    }
}

/// Ordinary and missing names, with bindings and without: the same outcome.
async fn lockstep(principal: &Principal, report: &mut Vec<String>) -> usize {
    let bound = prepared(true, principal).await;
    let unbound = prepared(false, principal).await;
    let mut pairs = 0;
    for (names, exists) in lockstep_names(principal) {
        for row in ordered() {
            if row.kind == Kind::Create && !exists {
                continue;
            }
            let (path, body) = row.fill(&names);
            let actual = exec(
                &bound,
                bound.token(principal.name),
                row.method,
                &path,
                &body,
            )
            .await;
            if !row.lockstep {
                if exists && actual.0 != 403 {
                    report.push(format!(
                        "{}: {} {path} must stay refused while bindings exist: {}",
                        principal.name,
                        row.method,
                        describe(&actual)
                    ));
                }
                continue;
            }
            let reference = exec(
                &unbound,
                unbound.token(principal.name),
                row.method,
                &path,
                &body,
            )
            .await;
            pairs += 1;
            if actual.0 >= 500 || reference.0 >= 500 {
                report.push(format!(
                    "{}: {} {path} answered a server error: with bindings [{}], without [{}]",
                    principal.name,
                    row.method,
                    describe(&actual),
                    describe(&reference)
                ));
            }
            let agree = if row.kind == Kind::Pattern {
                actual.0 == reference.0
            } else {
                same_outcome(&actual, &reference)
            };
            if !agree {
                report.push(format!(
                    "{}: {} {path}: with bindings [{}], without [{}]",
                    principal.name,
                    row.method,
                    describe(&actual),
                    describe(&reference)
                ));
            }
            if exists
                && row.method == "GET"
                && ((row.path == "/v1/collections/$c" && granted(principal, &names.c))
                    || (row.path == "/v1/graphs/$g"
                        && names.g.ends_with("ordinary_graph")
                        && granted(principal, &names.g)))
                && actual.0 != 200
            {
                report.push(format!(
                    "{}: an ordinary name must stay reachable: GET {path}: {}",
                    principal.name,
                    describe(&actual)
                ));
            }
        }
    }
    pairs
}

async fn run_principal(index: usize) {
    let principal = &PRINCIPALS[index];
    let world = prepared(true, principal).await;
    let mut run = Run::new(&world, principal).await;
    for variant in variants(principal) {
        for row in ordered() {
            run.probe(row, &variant).await;
        }
    }
    for namesake in namesakes(principal) {
        for row in ordered() {
            run.send(row, &namesake).await;
        }
    }
    let mut report = std::mem::take(&mut run.report);
    let executed = run.executed;
    let pairs = lockstep(principal, &mut report).await;
    assert!(
        report.is_empty(),
        "{} mismatches for {}:\n{}",
        report.len(),
        principal.name,
        report
            .iter()
            .take(30)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        executed >= ROWS.len() * 3 && pairs >= ROWS.len() * 2,
        "{}: only {executed} probes and {pairs} pairs ran",
        principal.name
    );
}

macro_rules! principal_tests {
    ($($name:ident => $index:expr),* $(,)?) => {
        $(
            #[tokio::test]
            async fn $name() {
                run_principal($index).await;
            }
        )*
    };
}

principal_tests! {
    matrix_wildcard => 0,
    matrix_memory_only => 1,
    matrix_raw_other => 2,
    matrix_raw => 3,
    matrix_tenant => 4,
    matrix_tenant_raw => 5,
    matrix_admin => 6,
    matrix_read_only => 7,
    matrix_viewer => 8,
    matrix_editor => 9,
    matrix_tenant_viewer => 10,
    matrix_tenant_editor => 11,
}

#[test]
fn the_matrix_covers_twelve_distinct_principals() {
    let names: BTreeSet<_> = PRINCIPALS.iter().map(|principal| principal.name).collect();
    assert_eq!(names.len(), 12);
    assert_eq!(PRINCIPALS.iter().filter(|p| p.tenant).count(), 4);
    assert_eq!(PRINCIPALS.iter().filter(|p| p.session.is_some()).count(), 4);
    for principal in &PRINCIPALS {
        for held in principal.held {
            assert!(RESERVED.iter().any(|(label, ..)| label == held));
        }
    }
}

// ------------------------------------------------------------- the inventory

fn normalize(path: &str) -> String {
    path.split('?')
        .next()
        .unwrap_or(path)
        .split('/')
        .map(|segment| match segment {
            "$c" | "{c}" | "{collection}" => "{collection}",
            "$g" | "$og" | "{g}" | "{graph}" => "{graph}",
            segment
                if (segment.starts_with('{') && segment.ends_with('}'))
                    || (!segment.is_empty()
                        && segment.bytes().all(|byte| byte.is_ascii_digit())) =>
            {
                "{id}"
            }
            segment => segment,
        })
        .collect::<Vec<_>>()
        .join("/")
}

const NAME_FIELDS: [&str; 4] = ["collection", "collections", "graph", "graphs"];

fn body_names(
    schemas: &Value,
    schema: &Value,
    trail: &str,
    seen: &mut Vec<String>,
    out: &mut BTreeSet<String>,
) {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let name = reference.rsplit('/').next().unwrap_or(reference).to_owned();
        if seen.contains(&name) {
            return;
        }
        seen.push(name.clone());
        body_names(schemas, &schemas[name.as_str()], trail, seen, out);
        seen.pop();
        return;
    }
    for combinator in ["allOf", "oneOf", "anyOf"] {
        for part in schema
            .get(combinator)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            body_names(schemas, part, trail, seen, out);
        }
    }
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (name, inner) in properties {
            if NAME_FIELDS.contains(&name.as_str()) {
                out.insert(format!("{trail}{name}"));
            }
            body_names(schemas, inner, &format!("{trail}{name}."), seen, out);
        }
    }
    if let Some(items) = schema.get("items") {
        body_names(schemas, items, &format!("{trail}[]."), seen, out);
    }
    if let Some(extra) = schema
        .get("additionalProperties")
        .filter(|extra| extra.is_object())
    {
        body_names(schemas, extra, &format!("{trail}*."), seen, out);
    }
}

/// Every operation of the served document that names a collection or a graph,
/// in its path or anywhere in its request body, with where.
fn name_bearing(spec: &Value) -> BTreeMap<(String, String), BTreeSet<String>> {
    let schemas = &spec["components"]["schemas"];
    let mut found = BTreeMap::new();
    for (path, item) in spec["paths"].as_object().unwrap() {
        for (method, operation) in item.as_object().unwrap() {
            if !matches!(method.as_str(), "get" | "post" | "put" | "patch" | "delete") {
                continue;
            }
            let mut names = BTreeSet::new();
            for segment in path.split('/') {
                match segment {
                    "{c}" | "{collection}" => {
                        names.insert("path:collection".to_owned());
                    }
                    "{g}" | "{graph}" => {
                        names.insert("path:graph".to_owned());
                    }
                    _ => {}
                }
            }
            let mut fields = BTreeSet::new();
            if let Some(content) = operation["requestBody"]["content"].as_object() {
                for media in content.values() {
                    body_names(schemas, &media["schema"], "", &mut Vec::new(), &mut fields);
                }
            }
            names.extend(fields.into_iter().map(|field| format!("body:{field}")));
            if !names.is_empty() {
                found.insert((method.to_ascii_uppercase(), normalize(path)), names);
            }
        }
    }
    found
}

async fn served_spec() -> Value {
    let world = build(true);
    let answer = request(&world.app, ADMIN, "GET", "/openapi.json", "").await;
    assert_eq!(answer.0, 200, "{answer:?}");
    serde_json::from_slice(&answer.2).unwrap()
}

fn snake_case(name: &str) -> String {
    let mut out = String::new();
    for (index, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if index > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// The graph batch operations that carry a `collection` field, read from the
/// wire enum so a new one cannot ship without a row.
fn batch_operations_naming_a_collection() -> BTreeSet<String> {
    let source = include_str!("../src/server/graph.rs");
    let mut operations = BTreeSet::new();
    let mut variant: Option<String> = None;
    let mut inside = false;
    for line in source.lines() {
        let line = line.trim_end();
        if !inside {
            inside = line == "enum OpIn {";
            continue;
        }
        if line == "}" {
            break;
        }
        if let Some(rest) = line.strip_prefix("    ")
            && !rest.starts_with(' ')
            && let Some(name) = rest.strip_suffix(" {")
        {
            variant = Some(name.to_owned());
            continue;
        }
        if line.starts_with("        collection: ")
            && let Some(name) = &variant
        {
            operations.insert(snake_case(name));
        }
    }
    operations
}

fn row_operations() -> BTreeSet<(String, String)> {
    ROWS.iter()
        .map(|row| (row.method.to_owned(), normalize(row.path)))
        .collect()
}

fn uncovered(
    detected: &BTreeSet<(String, String)>,
    exempt: &BTreeSet<(String, String)>,
    covered: &BTreeSet<(String, String)>,
) -> Vec<(String, String)> {
    detected
        .iter()
        .filter(|operation| !covered.contains(*operation) && !exempt.contains(*operation))
        .cloned()
        .collect()
}

#[tokio::test]
async fn every_name_bearing_operation_has_generated_coverage() {
    let spec = served_spec().await;
    let found = name_bearing(&spec);
    let detected: BTreeSet<_> = found.keys().cloned().collect();
    assert!(
        detected.len() >= 50,
        "the walker found only {} operations",
        detected.len()
    );

    // A name in a body is found the way a name in a path is.
    for ((method, path), field) in [
        (("POST", "/v1/admin/credentials"), "collections"),
        (("POST", "/v1/admin/webhooks"), "collections"),
        (("POST", "/v1/admin/webhooks"), "graphs"),
        (("POST", "/v1/admin/users"), "graphs"),
        (("POST", "/v1/graphs"), "graph"),
        (("POST", "/v1/graphs/{graph}/retrieve"), "pivot.collection"),
        (
            ("POST", "/v1/graphs/{graph}/retrieve"),
            "evidence.collections",
        ),
    ] {
        let names = found.get(&(method.to_owned(), path.to_owned()));
        assert!(
            names.is_some_and(|names| names.contains(&format!("body:{field}"))),
            "{method} {path} must be found to name `{field}` in its body: {names:?}"
        );
    }

    let exempt: BTreeSet<_> = EXEMPT
        .iter()
        .map(|(method, path, _)| (method.to_string(), path.to_string()))
        .collect();
    let undocumented: BTreeSet<_> = UNDOCUMENTED
        .iter()
        .map(|(method, path)| (method.to_string(), path.to_string()))
        .collect();
    let creating: BTreeSet<_> = CREATING
        .iter()
        .map(|(method, path)| (method.to_string(), path.to_string()))
        .collect();
    let covered = row_operations();

    let missing = uncovered(&detected, &exempt, &covered);
    assert!(
        missing.is_empty(),
        "a route naming a collection or graph needs a row in ROWS (or an EXEMPT entry with its reason) \
         before it ships: {missing:?}"
    );
    let stale: Vec<_> = exempt.difference(&detected).collect();
    assert!(
        stale.is_empty(),
        "EXEMPT entries without a route: {stale:?}"
    );
    let known: BTreeSet<_> = detected
        .iter()
        .chain(&undocumented)
        .chain(&creating)
        .cloned()
        .collect();
    let orphans: Vec<_> = covered.difference(&known).collect();
    assert!(
        orphans.is_empty(),
        "rows for routes the document does not describe (add them to UNDOCUMENTED once they exist): {orphans:?}"
    );
    let documented_now: Vec<_> = undocumented.intersection(&detected).collect();
    assert!(
        documented_now.is_empty(),
        "now documented, remove from UNDOCUMENTED: {documented_now:?}"
    );
    for (method, path) in &creating {
        assert!(
            spec["paths"]
                .as_object()
                .unwrap()
                .iter()
                .any(|(candidate, item)| normalize(candidate) == *path
                    && item.get(method.to_ascii_lowercase()).is_some()),
            "{method} {path} is not in the served document"
        );
        assert!(covered.contains(&(method.clone(), path.clone())));
    }

    // Losing the rows of one operation must be seen, however many rows share it.
    for omitted in detected.difference(&exempt) {
        let mutant: BTreeSet<_> = ROWS
            .iter()
            .map(|row| (row.method.to_owned(), normalize(row.path)))
            .filter(|operation| operation != omitted)
            .collect();
        assert_eq!(
            uncovered(&detected, &exempt, &mutant),
            vec![omitted.clone()],
            "the inventory must notice a missing row for {omitted:?}"
        );
    }

    // Graph batch operations that name a collection, from the wire enum: each
    // gets a row with the collection varying under an ordinary graph, with an
    // ordinary collection under the varying graph, and with both varying.
    let operations = batch_operations_naming_a_collection();
    for expected in ["attach", "detach", "put_vector", "delete_vector"] {
        assert!(
            operations.contains(expected),
            "the wire enum scan lost `{expected}`: {operations:?}"
        );
    }
    for operation in &operations {
        for (graph, collection) in [("$og", "$c"), ("$g", "$o"), ("$g", "$c")] {
            let path = format!("/v1/graphs/{graph}/batch");
            let has_row = ROWS.iter().any(|row| {
                row.method == "POST"
                    && row.path == path
                    && row.body.contains(&format!("\"op\":\"{operation}\""))
                    && row
                        .body
                        .contains(&format!("\"collection\":\"{collection}\""))
            });
            assert!(
                has_row,
                "graph batch `{operation}` needs a row for {graph} with {collection}"
            );
        }
    }
}

#[tokio::test]
async fn generated_requests_take_effect_when_nothing_is_hidden() {
    let admin = &PRINCIPALS[6];
    let world = prepared(false, admin).await;
    let ordinary = names("", "ordinary", "ordinary_graph");
    let fresh = names("", "fresh_ev", "fresh_graph");
    let mut failures = Vec::new();
    let mut checked = 0;
    for row in ordered() {
        if row.effect != Effect::Succeeds {
            continue;
        }
        let names = if row.kind == Kind::Reference {
            &ordinary
        } else {
            &fresh
        };
        let (path, body) = row.fill(names);
        let answer = exec(&world, ADMIN, row.method, &path, &body).await;
        checked += 1;
        if !(200..300).contains(&answer.0) {
            failures.push(format!("{} {path}: {}", row.method, describe(&answer)));
        }
    }
    assert!(
        checked >= 10,
        "only {checked} rows are meant to take effect"
    );
    assert!(
        failures.is_empty(),
        "rows meant to take effect did not:\n{}",
        failures.join("\n")
    );
}

#[tokio::test]
async fn the_digest_sees_a_write_to_every_protected_resource() {
    let world = prepared(true, &PRINCIPALS[6]).await;
    let labels: Vec<_> = snapshot(&world).await.into_keys().collect();
    assert_eq!(
        labels,
        [
            "reserved.root.notes",
            "reserved.root.other",
            "reserved.tenant.notes",
            "root.kvstore",
            "root.lists",
            "root.ordinary"
        ],
        "the digest must cover every protected resource"
    );
    let mut failures = Vec::new();
    let mut resources: Vec<(&str, &str, &str, &str)> = RESERVED
        .iter()
        .map(|(label, holder, collection, graph, _)| (*label, *holder, *collection, *graph))
        .collect();
    resources.push(("root.ordinary", "admin", "ordinary", "ordinary_graph"));
    for (label, writer, collection, graph) in resources {
        for (method, path, body) in [
            (
                "PUT",
                format!("/v1/collections/{collection}/vectors/21"),
                r#"{"vector":[0,1]}"#.to_owned(),
            ),
            (
                "PUT",
                format!("/v1/collections/{collection}/vectors/7"),
                RECORD.to_owned(),
            ),
            (
                "PUT",
                format!("/v1/collections/{collection}/settings"),
                r#"{"hnsw":{"ef_construction":321}}"#.to_owned(),
            ),
            (
                "POST",
                format!("/v1/graphs/{graph}/batch"),
                r#"{"ops":[{"op":"create_node","key":"digest_probe"}]}"#.to_owned(),
            ),
        ] {
            let before = snapshot(&world).await;
            let answer = request(&world.app, world.token(writer), method, &path, &body).await;
            assert!(
                (200..300).contains(&answer.0),
                "{writer} {method} {path}: {}",
                describe(&answer)
            );
            let after = snapshot(&world).await;
            for (name, now) in &after {
                let changed = before[name] != *now;
                if (*name == label) != changed {
                    failures.push(format!(
                        "{method} {path}: {name} {} but {label} was written",
                        if changed { "changed" } else { "did not change" }
                    ));
                }
            }
        }
    }
    {
        let before = snapshot(&world).await;
        let answer = request(
            &world.app,
            ADMIN,
            "PUT",
            "/v1/collections/kvstore/kv/entry",
            KV_PUT,
        )
        .await;
        assert!(
            (200..300).contains(&answer.0),
            "PUT kvstore/kv/entry: {}",
            describe(&answer)
        );
        let after = snapshot(&world).await;
        for (name, now) in &after {
            if (*name == "root.kvstore") != (before[name] != *now) {
                failures.push(format!(
                    "PUT kvstore/kv/entry: {name} is not the only label that changed"
                ));
            }
        }
    }
    for (path, body) in [
        ("/v1/collections", r#"{"name":"listed_ev","dimensions":2}"#),
        ("/v1/graphs", r#"{"graph":"listed_graph"}"#),
    ] {
        let before = snapshot(&world).await;
        let answer = request(&world.app, ADMIN, "POST", path, body).await;
        assert_eq!(answer.0, 201, "{}", describe(&answer));
        let after = snapshot(&world).await;
        for (name, now) in &after {
            if (*name == "root.lists") != (before[name] != *now) {
                failures.push(format!(
                    "POST {path}: {name} is not the only label that changed"
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn markers_reach_their_holders_so_a_leak_would_show() {
    let world = prepared(true, &PRINCIPALS[6]).await;
    for (_, holder, collection, graph, marker) in RESERVED {
        let record = request(
            &world.app,
            world.token(holder),
            "GET",
            &format!("/v1/collections/{collection}/vectors/7"),
            "",
        )
        .await;
        let nodes = request(
            &world.app,
            world.token(holder),
            "POST",
            &format!("/v1/graphs/{graph}/nodes/find"),
            r#"{"limits":{"deadline_ms":5000}}"#,
        )
        .await;
        for (what, answer) in [("record", record), ("nodes", nodes)] {
            let text = String::from_utf8_lossy(&answer.2).into_owned();
            assert!(
                forms(marker)
                    .iter()
                    .any(|form| text.contains(form.as_str())),
                "{holder} must read back {marker} in the {what} of {collection}: {}",
                describe(&answer)
            );
        }
    }
    for (reader, marker) in [("admin", "ROOTORDINARY"), ("tenant", "TENANTORDINARY")] {
        let entry = request(
            &world.app,
            world.token(reader),
            "GET",
            "/v1/collections/kvstore/kv/entry?namespace=n&key=k",
            "",
        )
        .await;
        let text = String::from_utf8_lossy(&entry.2).into_owned();
        assert!(
            forms(marker)
                .iter()
                .any(|form| text.contains(form.as_str())),
            "{reader} must read back {marker} from kvstore: {}",
            describe(&entry)
        );
    }
}

/// The reserved names a principal that does not hold them must never find
/// in a list or in the change feed.
fn hidden_names(principal: &Principal) -> Vec<String> {
    let holds = |label: &str| principal.held.contains(&label);
    let mut out = Vec::new();
    if principal.tenant {
        if !holds("reserved.tenant.notes") {
            out.extend(["notes_ev".to_owned(), "notes_graph".to_owned()]);
        }
    } else {
        if !holds("reserved.root.notes") {
            out.extend(["notes_ev".to_owned(), "notes_graph".to_owned()]);
        }
        if !holds("reserved.root.other") {
            out.extend(["other_ev".to_owned(), "other_graph".to_owned()]);
        }
        out.extend([
            format!("t{TENANT}__notes_ev"),
            format!("t{TENANT}__notes_graph"),
        ]);
    }
    out
}

/// Lists and the feed carry no name in the request, so the matrix cannot send
/// a reserved name to them; what it can check is that none comes back.
#[tokio::test]
async fn lists_and_the_feed_never_name_a_reserved_resource_to_a_caller_that_may_not_reach_it() {
    let mut world = build(true);
    for principal in &PRINCIPALS {
        if let Some(role) = principal.session {
            sign_in(&mut world, principal.name, role, principal.tenant).await;
        }
    }
    seed(&world).await;
    let mut failures = Vec::new();
    for principal in &PRINCIPALS {
        let token = world.token(principal.name);
        let hidden = hidden_names(principal);
        for path in ["/v1/collections", "/v1/graphs", "/v1/changes?limit=1000"] {
            let answer = request(&world.app, token, "GET", path, "").await;
            let text = String::from_utf8_lossy(&answer.2).into_owned();
            for name in &hidden {
                if text.contains(&format!("\"{name}\"")) {
                    failures.push(format!(
                        "{}: GET {path} names the reserved {name}: {text}",
                        principal.name
                    ));
                }
            }
        }
    }
    // The check is able to fail: a holder's lists do carry its own names.
    for (holder, collection, graph) in [
        ("raw", "notes_ev", "notes_graph"),
        ("raw_other", "other_ev", "other_graph"),
        ("tenant_raw", "notes_ev", "notes_graph"),
    ] {
        for (path, name) in [("/v1/collections", collection), ("/v1/graphs", graph)] {
            let answer = request(&world.app, world.token(holder), "GET", path, "").await;
            let text = String::from_utf8_lossy(&answer.2).into_owned();
            if !text.contains(&format!("\"{name}\"")) {
                failures.push(format!("{holder}: GET {path} must list {name}: {text}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
