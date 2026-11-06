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

/// The same properties over gRPC (lane F3, slice S1), in a module of its own.
#[cfg(feature = "grpc")]
#[path = "server_guard_differential/grpc_leg.rs"]
mod grpc_leg;

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
    /// The service the router was built over, for the gRPC leg.
    #[cfg(feature = "grpc")]
    service: Service,
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
    let service = Service::new(db);
    let app = configured_admin_router(
        service.clone(),
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
        #[cfg(feature = "grpc")]
        service,
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

/// What every seeded record carries: the marker as text in the fields the
/// facet and group routes read, its id as the number the statistics read, and
/// a point every geo search finds.
fn seed_metadata(marker: &str) -> Value {
    let text = marker.to_ascii_lowercase();
    json!({
        "owner": {"type": "string", "value": text},
        "tag": {"type": "string", "value": text},
        "score": {"type": "float", "value": marker_id(marker) as f64},
        "location": {"type": "geopoint", "value": {"latitude": 0.0, "longitude": 0.0}},
    })
}

async fn seed(world: &World) {
    for (bound_writer, unbound_writer, collection, graph, marker) in SEEDS {
        let writer = world.token(if world.bound {
            bound_writer
        } else {
            unbound_writer
        });
        for id in [7, 8, marker_id(marker)] {
            let put = request(
                &world.app,
                writer,
                "PUT",
                &format!("/v1/collections/{collection}/vectors/{id}"),
                &json!({"vector":[1,0],"payload_base64":STANDARD.encode(marker),
                    "metadata":seed_metadata(marker)})
                .to_string(),
            )
            .await;
            assert_eq!(put.0, 204, "seed {collection}/{id}: {}", describe(&put));
        }
        let space = request(
            &world.app,
            writer,
            "POST",
            &format!("/v1/collections/{collection}/vectors/configure"),
            SEED_SPACE,
        )
        .await;
        assert_eq!(
            space.0,
            200,
            "seed {collection} space: {}",
            describe(&space)
        );
        let named = request(
            &world.app,
            writer,
            "PUT",
            &format!(
                "/v1/collections/{collection}/vectors/{}/named",
                marker_id(marker)
            ),
            SEED_NAMED,
        )
        .await;
        assert_eq!(
            named.0,
            200,
            "seed {collection} named: {}",
            describe(&named)
        );
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
    // The legacy routes address the root `default` collection. A tenant that
    // creates its own `default` gets a qualified one, so a legacy route that
    // reached a tenant would resolve and answer instead of failing early.
    let root_default = request(
        &world.app,
        ADMIN,
        "PUT",
        "/v1/vectors/7",
        &json!({"vector":[1,0],"payload_base64":STANDARD.encode("ROOTORDINARY"),
            "metadata":seed_metadata("ROOTORDINARY")})
        .to_string(),
    )
    .await;
    assert_eq!(root_default.0, 204, "{}", describe(&root_default));
    let tenant_default = request(
        &world.app,
        world.token("tenant"),
        "POST",
        "/v1/collections",
        r#"{"name":"default","dimensions":2}"#,
    )
    .await;
    assert_eq!(tenant_default.0, 201, "{}", describe(&tenant_default));
    let tenant_record = request(
        &world.app,
        world.token("tenant"),
        "PUT",
        "/v1/collections/default/vectors/7",
        &json!({"vector":[1,0],"payload_base64":STANDARD.encode("TENANTORDINARY"),
            "metadata":seed_metadata("TENANTORDINARY")})
        .to_string(),
    )
    .await;
    assert_eq!(tenant_record.0, 204, "{}", describe(&tenant_record));
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
    /// The request must succeed and its answer must carry something only the
    /// resource it names holds (a marker or the id of its marker record), or a
    /// read of the wrong namespace would look like any other answer.
    Carries,
}

/// `$c` and `$g` are the names the request is about, `$o` and `$og` an
/// ordinary collection and graph of the same namespace, `$n` a counter and
/// `$expires` a future time. A collection named in a request body takes `$C`
/// and `$O` instead: the same collections by their stored names, which differ
/// from `$c` and `$o` for a tenant only.
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

/// A read whose answer carries the marker or the marker record's id of the
/// resource it names.
const fn carrying(method: &'static str, path: &'static str, body: &'static str) -> Row {
    Row {
        method,
        path,
        body,
        kind: Kind::Reference,
        effect: Effect::Carries,
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
const CONFIGURE: &str = r#"{"name":"w","dimensions":2,"metric":"cosine","active_version":"v1"}"#;
/// The vector space every seeded collection already has, so a named write has
/// something to fill.
const SEED_SPACE: &str = r#"{"name":"v","dimensions":2,"metric":"cosine","active_version":"v1"}"#;
/// The marker record's named vectors: the only record the named, sparse and
/// fused searches can answer with before a request writes another.
const SEED_NAMED: &str =
    r#"{"dense":{"v":[{"version":"v1","vector":[1,0]}]},"sparse":{"s":[[1,1]]}}"#;
const NAMED: &str = r#"{"dense":{"v":[{"version":"v1","vector":[0,1]}]},"sparse":{"s":[[1,1]]}}"#;
/// Key-value writes only succeed on a collection of one dimension (`kvstore`).
const KV_PUT: &str = r#"{"namespace":"n","key":"k","value_base64":"UFJPQkU="}"#;

/// Deletions run last, in table order, so every other row still has its
/// resource to act on.
const ROWS: &[Row] = &[
    // Collections, named in the path.
    effective("GET", "/v1/collections/$c", ""),
    carrying("GET", "/v1/collections/$c/vectors", ""),
    row("GET", "/v1/collections/$c/vectors/7", ""),
    effective("PUT", "/v1/collections/$c/vectors/7", RECORD),
    effective("PATCH", "/v1/collections/$c/vectors/7/metadata", PATCH),
    effective("POST", "/v1/collections/$c/vectors/configure", CONFIGURE),
    effective("PUT", "/v1/collections/$c/vectors/7/named", NAMED),
    effective(
        "POST",
        "/v1/collections/$c/batches",
        r#"{"operations":[{"op":"put","id":"9","vector":[0,1]}]}"#,
    ),
    carrying(
        "POST",
        "/v1/collections/$c/search",
        r#"{"vector":[1,0],"k":5}"#,
    ),
    carrying(
        "POST",
        "/v1/collections/$c/search/grouped",
        r#"{"vector":[1,0],"field":"tag","groups":3,"hits_per_group":3}"#,
    ),
    carrying(
        "POST",
        "/v1/collections/$c/search/geo",
        r#"{"vector":[1,0],"k":5,"field":"location","latitude":0,"longitude":0,"radius_meters":1}"#,
    ),
    carrying(
        "POST",
        "/v1/collections/$c/recommend",
        r#"{"positive_ids":["8"],"k":5}"#,
    ),
    carrying("POST", "/v1/collections/$c/facets", r#"{"field":"tag"}"#),
    carrying(
        "POST",
        "/v1/collections/$c/stats/numeric",
        r#"{"field":"score"}"#,
    ),
    carrying(
        "POST",
        "/v1/collections/$c/query",
        r#"{"query":"SEARCH default NEAR $q K 5","vectors":{"q":[1,0]}}"#,
    ),
    row(
        "POST",
        "/v1/collections/$c/embed",
        r#"{"text":"fake text"}"#,
    ),
    carrying(
        "POST",
        "/v1/collections/$c/search/named",
        r#"{"name":"v","vector":[1,0],"k":5}"#,
    ),
    carrying(
        "POST",
        "/v1/collections/$c/search/sparse",
        r#"{"name":"s","vector":[[1,1]],"k":5}"#,
    ),
    carrying(
        "POST",
        "/v1/collections/$c/search/dense-sparse",
        r#"{"dense_name":"v","dense":[1,0],"sparse_name":"s","sparse":[[1,1]],"k":5}"#,
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
        r#"{"pivot":{"collection":"$O","vector":[0,1],"k":1}}"#,
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
        r#"{"pivot":{"collection":"$C","vector":[0,1],"k":1}}"#,
    ),
    row(
        "POST",
        "/v1/graphs/$og/retrieve",
        r#"{"pivot":{"collection":"$O","vector":[0,1],"k":1},"evidence":{"collections":["$C"]}}"#,
    ),
    row(
        "POST",
        "/v1/graphs/$og/batch",
        r#"{"ops":[{"op":"attach","node":"n","collection":"$C","record":"7"}]}"#,
    ),
    row(
        "POST",
        "/v1/graphs/$og/batch",
        r#"{"ops":[{"op":"detach","node":"n","collection":"$C"}]}"#,
    ),
    row(
        "POST",
        "/v1/graphs/$og/batch",
        r#"{"ops":[{"op":"put_vector","collection":"$C","id":"11","vector":[0,1]}]}"#,
    ),
    row(
        "POST",
        "/v1/graphs/$og/batch",
        r#"{"ops":[{"op":"delete_vector","collection":"$C","id":"7"}]}"#,
    ),
    // The graph is the name that varies, the collection is ordinary.
    row(
        "POST",
        "/v1/graphs/$g/batch",
        r#"{"ops":[{"op":"attach","node":"n","collection":"$O","record":"7"}]}"#,
    ),
    row(
        "POST",
        "/v1/graphs/$g/batch",
        r#"{"ops":[{"op":"detach","node":"n","collection":"$O"}]}"#,
    ),
    row(
        "POST",
        "/v1/graphs/$g/batch",
        r#"{"ops":[{"op":"put_vector","collection":"$O","id":"12","vector":[0,1]}]}"#,
    ),
    row(
        "POST",
        "/v1/graphs/$g/batch",
        r#"{"ops":[{"op":"delete_vector","collection":"$O","id":"8"}]}"#,
    ),
    // Both vary together.
    row(
        "POST",
        "/v1/graphs/$g/batch",
        r#"{"ops":[{"op":"attach","node":"n","collection":"$C","record":"7"}]}"#,
    ),
    row(
        "POST",
        "/v1/graphs/$g/batch",
        r#"{"ops":[{"op":"detach","node":"n","collection":"$C"}]}"#,
    ),
    row(
        "POST",
        "/v1/graphs/$g/batch",
        r#"{"ops":[{"op":"put_vector","collection":"$C","id":"13","vector":[0,1]}]}"#,
    ),
    row(
        "POST",
        "/v1/graphs/$g/batch",
        r#"{"ops":[{"op":"delete_vector","collection":"$C","id":"8"}]}"#,
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
    /// The stored names of `c` and `o`, which a request body must carry: a
    /// tenant addresses its own collections by `t{tenant}__{name}` there,
    /// while a path takes the name the tenant knows.
    sc: String,
    so: String,
}

fn names(prefix: &str, collection: &str, graph: &str) -> Names {
    let c = format!("{prefix}{collection}");
    let o = format!("{prefix}ordinary");
    Names {
        sc: c.clone(),
        so: o.clone(),
        c,
        g: format!("{prefix}{graph}"),
        o,
        og: format!("{prefix}ordinary_graph"),
    }
}

/// The names as a tenant's request body spells them: its collections by their
/// stored names, so that a body-named collection reaches the guard rather than
/// the grant check that refuses a name outside the tenant's namespace.
fn stored(mut names: Names) -> Names {
    names.sc = format!("t{TENANT}__{}", names.c);
    names.so = format!("t{TENANT}__{}", names.o);
    names
}

fn fill(text: &str, names: &Names) -> String {
    let mut out = text
        .replace("$og", &names.og)
        .replace("$o", &names.o)
        .replace("$c", &names.c)
        .replace("$g", &names.g)
        .replace("$C", &names.sc)
        .replace("$O", &names.so);
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

fn variant(prefix: &str, collection: &str, graph: &str, held: bool, tenant: bool) -> Variant {
    let spell = |names: Names| if tenant { stored(names) } else { names };
    let reserved = spell(names(prefix, collection, graph));
    let occupied = Names {
        c: reserved.o.clone(),
        g: reserved.og.clone(),
        o: reserved.o.clone(),
        og: reserved.og.clone(),
        sc: reserved.so.clone(),
        so: reserved.so.clone(),
    };
    Variant {
        missing: spell(names(prefix, "absent_ev", "absent_graph")),
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
            true,
        )]
    } else {
        vec![
            variant(
                "",
                "notes_ev",
                "notes_graph",
                holds("reserved.root.notes"),
                false,
            ),
            variant(
                "",
                "other_ev",
                "other_graph",
                holds("reserved.root.other"),
                false,
            ),
            variant(
                &format!("t{TENANT}__"),
                "notes_ev",
                "notes_graph",
                false,
                false,
            ),
        ]
    }
}

/// Names a tenant shares with root resources, one reserved there and one
/// ordinary: whatever it does with them must stay in its own namespace. A
/// request body takes stored names, so a name a tenant spells there without its
/// prefix is the root resource's: its grant must refuse it before anything is
/// looked up, and no root resource may answer or change.
fn namesakes(principal: &Principal) -> Vec<Names> {
    if principal.tenant {
        const SHARED: [(&str, &str); 3] = [
            ("other_ev", "other_graph"),
            ("ordinary", "ordinary_graph"),
            ("kvstore", "absent_graph"),
        ];
        let prefixed = SHARED.iter().map(|(c, g)| stored(names("", c, g)));
        let unprefixed = SHARED.iter().map(|(c, g)| names("", c, g));
        prefixed.chain(unprefixed).collect()
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
            (stored(names("", "ordinary", "ordinary_graph")), true),
            (stored(names("", "other_ev", "other_graph")), true),
            (stored(names("", "kvstore", "absent_graph")), true),
            (stored(names("", "absent_ev", "absent_graph")), false),
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
        format!("/v1/collections/{collection}/index/hnsw"),
    ] {
        out += &rendered(&path, &request(&world.app, token, "GET", &path, "").await);
    }
    // The named vector spaces a write may have configured and filled: `v` is
    // seeded and filled by a named put, `w` is the one a configure adds.
    for space in ["v", "w"] {
        let named = format!("/v1/collections/{collection}/search/named");
        out += &rendered(
            &format!("{named} {space}"),
            &request(
                &world.app,
                token,
                "POST",
                &named,
                &json!({"name":space,"vector":[1,0],"k":5}).to_string(),
            )
            .await,
        );
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
/// root namespace; nothing else. Without bindings nothing is reserved, so only
/// a tenant has anything to stay out of.
fn protected(principal: &Principal, label: &str, bound: bool) -> bool {
    if principal.held.contains(&label) {
        return false;
    }
    (bound && label.starts_with("reserved.")) || principal.tenant
}

fn forms(marker: &str) -> Vec<String> {
    vec![
        marker.to_owned(),
        marker.to_ascii_lowercase(),
        STANDARD.encode(marker).trim_end_matches('=').to_owned(),
    ]
}

/// The id of the record a resource keeps for nothing but being recognised:
/// search routes answer with ids and distances only, so an id is how a read
/// of the wrong namespace shows.
fn marker_id(marker: &str) -> u64 {
    match marker {
        "ROOTORDINARY" => 7_770_001,
        "RSVROOTNOTES" => 7_770_002,
        "RSVROOTOTHER" => 7_770_003,
        "RSVTENANTNOTES" => 7_770_004,
        "TENANTORDINARY" => 7_770_005,
        other => panic!("no marker id for {other}"),
    }
}

/// Everything in an answer that shows a read of the resource behind `marker`.
fn identities(marker: &str) -> Vec<String> {
    let mut out = forms(marker);
    out.push(marker_id(marker).to_string());
    out
}

/// What a principal must never read back: the markers of reserved resources
/// it does not hold (only while they are reserved), and, for a tenant, the
/// markers of everything in the root namespace.
fn forbidden_markers(principal: &Principal, bound: bool) -> Vec<String> {
    let mut out = Vec::new();
    for (label, _, _, _, marker) in RESERVED {
        let root = label.starts_with("reserved.root.");
        let forbidden = if bound {
            !principal.held.contains(&label)
        } else {
            principal.tenant && root
        };
        if forbidden {
            out.extend(identities(marker));
        }
    }
    if principal.tenant {
        out.extend(identities("ROOTORDINARY"));
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
            markers: forbidden_markers(principal, world.bound),
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
                if protected(self.principal, label, self.world.bound) && self.last[label] != *now {
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
        for (graph, collection) in [("$og", "$C"), ("$g", "$O"), ("$g", "$C")] {
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
        if row.effect == Effect::Unchecked {
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
        } else if row.effect == Effect::Carries {
            let text = String::from_utf8_lossy(&answer.2);
            if !identities("ROOTORDINARY")
                .iter()
                .any(|form| text.contains(form.as_str()))
            {
                failures.push(format!(
                    "{} {path} carries nothing of the resource it names: {}",
                    row.method,
                    describe(&answer)
                ));
            }
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
            (
                "POST",
                format!("/v1/collections/{collection}/vectors/configure"),
                CONFIGURE.to_owned(),
            ),
            (
                "PUT",
                format!("/v1/collections/{collection}/vectors/7/named"),
                NAMED.to_owned(),
            ),
            (
                "POST",
                format!("/v1/collections/{collection}/index/hnsw"),
                "{}".to_owned(),
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
        for (what, path, body) in [
            ("list", format!("/v1/collections/{collection}/vectors"), ""),
            (
                "named search",
                format!("/v1/collections/{collection}/search/named"),
                r#"{"name":"v","vector":[1,0],"k":5}"#,
            ),
        ] {
            let method = if body.is_empty() { "GET" } else { "POST" };
            let answer = request(&world.app, world.token(holder), method, &path, body).await;
            assert!(
                String::from_utf8_lossy(&answer.2).contains(&marker_id(marker).to_string()),
                "{holder} must read back the id of {marker} in the {what} of {collection}: {}",
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
/// in a list, in the change feed or in the slow-query log. Logs and the feed
/// carry stored names, so a tenant's own reserved names appear qualified.
fn hidden_names(principal: &Principal) -> Vec<String> {
    let holds = |label: &str| principal.held.contains(&label);
    let mut out = Vec::new();
    if principal.tenant {
        if !holds("reserved.tenant.notes") {
            out.extend([
                "notes_ev".to_owned(),
                "notes_graph".to_owned(),
                format!("t{TENANT}__notes_ev"),
                format!("t{TENANT}__notes_graph"),
            ]);
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

/// A tenant spells the collections a request body names by their stored names,
/// `t{tenant}__{name}`, which is what puts them in front of the guard instead of
/// the grant check that refuses every other name. The matrix sends those names
/// for every tenant principal; this test pins what they must do, so that the
/// matrix cannot pass because a body name never got that far: the tenant's
/// ordinary collection takes the write (and only the tenant's does), its
/// reserved collection answers like a missing one, a root name is refused.
#[tokio::test]
async fn a_tenants_body_names_are_stored_names_that_reach_the_guard() {
    let tenant = &PRINCIPALS[4];
    let world = prepared(true, tenant).await;
    let token = world.token(tenant.name);
    let tenanted = |name: &str| format!("t{TENANT}__{name}");
    let put = |collection: &str, id: u32| {
        format!(
            r#"{{"ops":[{{"op":"put_vector","collection":"{collection}","id":"{id}","vector":[0,1]}}]}}"#
        )
    };
    let batch = "/v1/graphs/ordinary_graph/batch";
    let send = |body: String| {
        let world = &world;
        async move { request(&world.app, token, "POST", batch, &body).await }
    };
    let own = send(put(&tenanted("ordinary"), 31)).await;
    assert_eq!(
        own.0,
        200,
        "its own collection by its stored name: {}",
        describe(&own)
    );
    let hidden = send(put(&tenanted("notes_ev"), 32)).await;
    let missing = send(put(&tenanted("absent_ev"), 32)).await;
    assert_eq!(
        hidden.0,
        404,
        "its reserved collection: {}",
        describe(&hidden)
    );
    assert_eq!(
        (hidden.0, error_of(&hidden)),
        (missing.0, error_of(&missing)),
        "a reserved collection must answer like a missing one"
    );
    let outside = send(put("ordinary", 33)).await;
    assert_eq!(outside.0, 403, "a root name: {}", describe(&outside));
    // The write landed in the tenant's collection and in no other.
    let read = |actor: &'static str, id: u32| {
        let world = &world;
        async move {
            request(
                &world.app,
                if actor == "admin" { ADMIN } else { token },
                "GET",
                &format!("/v1/collections/ordinary/vectors/{id}"),
                "",
            )
            .await
        }
    };
    let landed = read("tenant", 31).await;
    assert_eq!(
        landed.0,
        200,
        "the tenant reads it back: {}",
        describe(&landed)
    );
    for id in [31, 32, 33] {
        let root = read("admin", id).await;
        assert_eq!(
            root.0,
            404,
            "root `ordinary` must not hold {id}: {}",
            describe(&root)
        );
    }
    let reserved = request(
        &world.app,
        world.token("tenant_raw"),
        "GET",
        "/v1/collections/notes_ev/vectors/32",
        "",
    )
    .await;
    assert_eq!(
        reserved.0,
        404,
        "the reserved collection took no write: {}",
        describe(&reserved)
    );
}

/// A name outside a credential's grant is refused with `403` before anything
/// is looked up (SECURITY.md: an out-of-scope name retains `403
/// forbidden_scope`), so it must not matter whether the name is reserved,
/// missing or an ordinary collection that exists. Without the grant check the
/// later checks would answer `404` for the first two and `403` for the third,
/// and a credential could tell which collections exist outside its grant. The
/// matrix compares reserved with missing names only; this adds the third and
/// holds every row that names a resource to the documented status.
#[tokio::test]
async fn a_name_outside_a_credentials_grant_is_refused_whatever_it_names() {
    let tenanted = format!("t{TENANT}__");
    // The credential limited to `ordinary` and `ordinary_graph`: every name,
    // in a path or a body, is outside its grant, even the ordinary names a row
    // mixes in.
    let limited = |collection: &str, graph: &str| {
        let mut spelled = names("", collection, graph);
        spelled.o = format!("{tenanted}ordinary");
        spelled.og = format!("{tenanted}ordinary_graph");
        spelled.so = spelled.o.clone();
        spelled
    };
    // A tenant: a path name is always its own, so only the names a body spells
    // without its prefix, which are the root's, are outside its grant.
    let tenant = |root_collection: &str| Names {
        c: "ordinary".to_owned(),
        g: "ordinary_graph".to_owned(),
        o: "ordinary".to_owned(),
        og: "ordinary_graph".to_owned(),
        sc: root_collection.to_owned(),
        so: root_collection.to_owned(),
    };
    let cases = [
        (
            1,
            &["$c", "$g", "$o", "$og", "$C", "$O"][..],
            [
                ("a reserved name", limited("notes_ev", "notes_graph")),
                ("a missing name", limited("absent_ev", "absent_graph")),
                (
                    "an ordinary name that exists",
                    limited(
                        &format!("{tenanted}ordinary"),
                        &format!("{tenanted}ordinary_graph"),
                    ),
                ),
            ],
        ),
        (
            4,
            &["$C", "$O"][..],
            [
                ("a reserved root name", tenant("notes_ev")),
                ("a missing root name", tenant("absent_ev")),
                ("an ordinary root name that exists", tenant("ordinary")),
            ],
        ),
    ];
    for (index, placeholders, spellings) in cases {
        let principal = &PRINCIPALS[index];
        let world = prepared(true, principal).await;
        let token = world.token(principal.name);
        let mut compared = 0;
        for row in ordered() {
            if !placeholders
                .iter()
                .any(|placeholder| row.path.contains(placeholder) || row.body.contains(placeholder))
            {
                continue;
            }
            for (what, spelled) in &spellings {
                let (path, body) = row.fill(spelled);
                let answer = exec(&world, token, row.method, &path, &body).await;
                assert_eq!(
                    answer.0,
                    403,
                    "{}: {} {path} with {what}: {}",
                    principal.name,
                    row.method,
                    describe(&answer)
                );
            }
            compared += 1;
        }
        assert!(
            compared >= 10,
            "{}: only {compared} rows name a resource",
            principal.name
        );
    }
}

#[tokio::test]
async fn lists_and_the_feed_never_name_a_reserved_resource_to_a_caller_that_may_not_reach_it() {
    let mut world = build(true);
    for principal in &PRINCIPALS {
        if let Some(role) = principal.session {
            sign_in(&mut world, principal.name, role, principal.tenant).await;
        }
    }
    seed(&world).await;
    // A root-only collection and graph no tenant has, so a tenant's list that
    // fell back to the whole database would show them.
    let root_only = request(
        &world.app,
        ADMIN,
        "POST",
        "/v1/collections",
        r#"{"name":"rootonly_ev","dimensions":2}"#,
    )
    .await;
    assert_eq!(root_only.0, 201, "{}", describe(&root_only));
    let root_only = request(
        &world.app,
        ADMIN,
        "POST",
        "/v1/graphs",
        r#"{"graph":"rootonly_graph"}"#,
    )
    .await;
    assert_eq!(root_only.0, 201, "{}", describe(&root_only));
    let mut failures = Vec::new();
    for principal in &PRINCIPALS {
        let token = world.token(principal.name);
        let mut hidden = hidden_names(principal);
        if principal.tenant {
            hidden.extend(["rootonly_ev".to_owned(), "rootonly_graph".to_owned()]);
        }
        let forbidden = forbidden_markers(principal, true);
        for path in [
            "/v1/collections",
            "/v1/graphs",
            "/v1/changes?limit=1000",
            "/v1/admin/diagnostics/slow-queries",
        ] {
            let answer = request(&world.app, token, "GET", path, "").await;
            let text = String::from_utf8_lossy(&answer.2).into_owned();
            for name in &hidden {
                if text.contains(&format!("\"{name}\"")) {
                    failures.push(format!(
                        "{}: GET {path} names the hidden {name}: {text}",
                        principal.name
                    ));
                }
            }
            for marker in &forbidden {
                if text.contains(marker.as_str()) {
                    failures.push(format!(
                        "{}: GET {path} carries {marker}: {text}",
                        principal.name
                    ));
                }
            }
        }
    }
    // The checks are able to fail: a holder's lists do carry its own names, and
    // a tenant's carry its own collections.
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
    let list = request(
        &world.app,
        world.token("tenant"),
        "GET",
        "/v1/collections",
        "",
    )
    .await;
    let text = String::from_utf8_lossy(&list.2).into_owned();
    if !text.contains("\"ordinary\"") || !text.contains("\"default\"") {
        failures.push(format!(
            "tenant: its own collections must be listed: {}",
            describe(&list)
        ));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// ------------------------------------------- routes that name no resource

/// Routes that span the database or address `default` without naming it.
/// SECURITY.md: a tenant credential's wildcard "grants only that tenant's
/// namespace, not global administration, backups, default collection access or
/// other tenants". Nothing in these paths names a collection, so the matrix
/// above cannot probe them with a reserved name; they are run by every
/// principal instead, under the marker scan and the digest.
const SPANNING: &[Row] = &[
    row("GET", "/v1/changes?limit=1000", ""),
    row("GET", "/v1/admin/replication/export", ""),
    row("GET", "/v1/admin/backups", ""),
    row("GET", "/v1/admin/backups/schedule", ""),
    row("GET", "/v1/admin/maintenance/auto", ""),
    row("GET", "/v1/database", ""),
    row("GET", "/v1/vectors", ""),
    row("GET", "/v1/vectors/7", ""),
    row("PUT", "/v1/vectors/7", RECORD),
    row("PATCH", "/v1/vectors/7/metadata", PATCH),
    row("POST", "/v1/search", r#"{"vector":[1,0],"k":5}"#),
    row(
        "POST",
        "/v1/batches",
        r#"{"operations":[{"op":"put","id":"9","vector":[0,1]}]}"#,
    ),
    row("GET", "/v1/index/hnsw", ""),
    row("POST", "/v1/index/hnsw", "{}"),
    row("DELETE", "/v1/vectors/7", ""),
];

/// What the spanning routes share with the rest of the server's global routes
/// that any principal may call: lists, the slow-query log and the reports of
/// the administrator.
const GLOBAL: &[Row] = &[
    row("GET", "/v1/collections", ""),
    row("GET", "/v1/graphs", ""),
    row("GET", "/v1/capabilities", ""),
    row("GET", "/v1/admin/diagnostics/slow-queries", ""),
    row("GET", "/v1/admin/tenants", ""),
    row("GET", "/v1/admin/audit", ""),
    row("GET", "/v1/admin/audit/persistent", ""),
    row("GET", "/v1/admin/credentials", ""),
    row("GET", "/v1/admin/webhooks", ""),
];

#[tokio::test]
async fn a_tenant_is_refused_every_route_that_spans_the_database() {
    let outside = names("", "ordinary", "ordinary_graph");
    let mut failures = Vec::new();
    for bound in [false, true] {
        for principal in PRINCIPALS.iter().filter(|principal| principal.tenant) {
            let world = prepared(bound, principal).await;
            let token = world.token(principal.name);
            let backup = Row {
                method: "POST",
                path: "/v1/admin/backups",
                body: "{}",
                kind: Kind::Reference,
                effect: Effect::Unchecked,
                lockstep: false,
            };
            for row in SPANNING.iter().chain([&backup]) {
                let (path, body) = row.fill(&outside);
                let answer = request(&world.app, token, row.method, &path, &body).await;
                let code = error_of(&answer)["code"].as_str().map(str::to_owned);
                if answer.0 != 403 || code.as_deref() != Some("forbidden_scope") {
                    failures.push(format!(
                        "{} (bindings {bound}): {} {path} must be refused with forbidden_scope: {}",
                        principal.name,
                        row.method,
                        describe(&answer)
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn routes_that_name_no_resource_leak_nothing_and_change_nothing() {
    let outside = names("", "ordinary", "ordinary_graph");
    let mut failures = Vec::new();
    let mut sent = 0;
    for bound in [true, false] {
        for principal in &PRINCIPALS {
            let world = prepared(bound, principal).await;
            let mut run = Run::new(&world, principal).await;
            for row in SPANNING.iter().chain(GLOBAL) {
                let answer = run.send(row, &outside).await;
                sent += 1;
                if answer.0 >= 500 {
                    failures.push(format!(
                        "{} (bindings {bound}): {} {} answered a server error: {}",
                        principal.name,
                        row.method,
                        row.path,
                        describe(&answer)
                    ));
                }
            }
            failures.append(&mut run.report);
        }
    }
    assert!(
        sent >= 2 * PRINCIPALS.len() * (SPANNING.len() + GLOBAL.len()),
        "only {sent} requests ran"
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// An import job names a collection through its ID, so the job routes carry
/// no name the matrix could probe. SECURITY.md: "Import job IDs are allocated
/// for the whole file, so a request about another tenant's job answers `403`
/// while an unused ID answers `404`". Imports are refused wholesale while
/// bindings exist, so this runs without them.
#[tokio::test]
async fn import_jobs_answer_only_to_the_credentials_that_reach_their_collection() {
    let world = prepared(false, &PRINCIPALS[6]).await;
    let reserve = |actor: &'static str, collection: &'static str| {
        let world = &world;
        async move {
            let answer = request(
                &world.app,
                world.token(actor),
                "POST",
                &format!("/v1/collections/{collection}/imports"),
                "",
            )
            .await;
            assert_eq!(
                answer.0,
                201,
                "{actor} reserves on {collection}: {}",
                describe(&answer)
            );
            serde_json::from_slice::<Value>(&answer.2).unwrap()["job"]
                .as_str()
                .unwrap()
                .parse::<u64>()
                .unwrap()
        }
    };
    let root_job = reserve("admin", "ordinary").await;
    let tenant_job = reserve("tenant", "ordinary").await;
    let unused = root_job.max(tenant_job) + 1_000;
    // The root job turns durable with a first chunk, so both the reserved and
    // the durable path of a job are probed.
    let chunk = request(
        &world.app,
        ADMIN,
        "PUT",
        &format!("/v1/imports/{root_job}/chunks/1"),
        r#"{"operations":[{"op":"put","id":"41","vector":[0,1]}]}"#,
    )
    .await;
    assert_eq!(chunk.0, 200, "{}", describe(&chunk));
    let second = reserve("admin", "ordinary").await;
    let before = snapshot(&world).await;
    let mut failures = Vec::new();
    // (who, job, status of a read of the job)
    for (who, job, expected) in [
        ("tenant", root_job, 403),
        ("tenant", second, 403),
        ("tenant", tenant_job, 200),
        ("tenant", unused, 404),
        ("tenant_raw", root_job, 403),
        ("tenant_raw", tenant_job, 200),
        ("memory_only", root_job, 200),
        ("memory_only", tenant_job, 403),
        ("wildcard", root_job, 200),
        ("admin", root_job, 200),
        ("admin", tenant_job, 200),
        ("admin", unused, 404),
    ] {
        let answer = request(
            &world.app,
            world.token(who),
            "GET",
            &format!("/v1/imports/{job}"),
            "",
        )
        .await;
        if answer.0 != expected {
            failures.push(format!(
                "{who} reading job {job}: expected {expected}: {}",
                describe(&answer)
            ));
        }
    }
    // Acting on a job of another namespace is refused as well and changes
    // nothing: not the job, and not the collection it would have written to.
    for who in ["tenant", "tenant_raw"] {
        for job in [root_job, second] {
            for (method, path, body) in [
                (
                    "PUT",
                    format!("/v1/imports/{job}/chunks/2"),
                    r#"{"operations":[{"op":"put","id":"42","vector":[0,1]}]}"#,
                ),
                ("POST", format!("/v1/imports/{job}/finish"), ""),
                ("POST", format!("/v1/imports/{job}/abort"), ""),
            ] {
                let answer = request(&world.app, world.token(who), method, &path, body).await;
                if answer.0 != 403 {
                    failures.push(format!(
                        "{who}: {method} {path} must be refused: {}",
                        describe(&answer)
                    ));
                }
            }
        }
    }
    let after = snapshot(&world).await;
    for (label, text) in &before {
        if after[label] != *text {
            failures.push(format!("{label} changed under a foreign job"));
        }
    }
    for job in [root_job, second] {
        let answer = request(&world.app, ADMIN, "GET", &format!("/v1/imports/{job}"), "").await;
        let state = serde_json::from_slice::<Value>(&answer.2)
            .map(|value| value["state"].clone())
            .unwrap_or(Value::Null);
        if state != "open" {
            failures.push(format!(
                "job {job} must still be open: {}",
                describe(&answer)
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
