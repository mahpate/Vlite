#![cfg(feature = "server")]
//! The raw graph write route and the names the memory core reserves: a batch
//! that would create or change a node whose label starts with `VliteMemory`
//! or whose key is under a memory prefix is refused as a whole with a fixed
//! 422, on every graph, so no raw writer can plant a node that looks like
//! memory. Expectations come from the 0.7.1 design note
//! (`docs/projects/roadmap/m71-memory-followups.md`, section 2) and the REST
//! graph contract, not from the guard.

use axum::{Router, body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tower::ServiceExt;
use vlite::{
    CollectionSpec, Database, Metadata,
    server::{BearerToken, ServerOptions, Service, configured_admin_router},
};

const ADMIN: &str = "fake_raw_names_admin_token_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const MEMORY_ID: &str = "abcdefghij27";
const RAW_ID: &str = "abcdefghij22";
const PLAIN_ID: &str = "abcdefghij23";
const NARROW_ID: &str = "abcdefghij24";
const SECRET: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const RESERVED: &str = "a node key or label is reserved for agent memory";

/// The documented reserved labels and key prefixes, and the one reserved key.
const LABELS: [&str; 7] = [
    "VliteMemory",
    "VliteMemoryScope",
    "VliteMemoryReceipt",
    "VliteMemoryReceiptOutcome",
    "VliteMemoryContent",
    "VliteMemoryErasureJob",
    "VliteMemoryBinding",
];
const PREFIXES: [&str; 5] = [
    "scope:v1:",
    "mem:v1:",
    "receipt:v1:",
    "receipt-outcome:v1:",
    "content:v1:",
];

fn bindings() -> String {
    format!(
        r#"{{"version":"memory-bindings-v1","bindings":[{{"binding":"notes","tenant":"@root","generation":1,"state":"active","graph":"notes_graph","evidence_collection":"notes_ev","schema_version":"memory-schema-v1","ranking_version":"memory-ranking-v1","digest_version":"memory-request-v1","receipt_window_ms":86400000,"grants":[{{"principal":"{MEMORY_ID}","scopes":[{{"agent":"a"}}],"raw_access":false}},{{"principal":"{RAW_ID}","scopes":[{{"agent":"b"}}],"raw_access":true}}]}}]}}"#
    )
}

struct Rig {
    directory: TempDir,
    app: Router,
    service: Service,
}

impl Rig {
    /// A file database with a bound graph, an unbound graph, an evidence
    /// collection and three scoped credentials; `bound` also starts the
    /// server with a bindings file.
    async fn new(bound: bool, plant: impl FnOnce(&mut Database)) -> Self {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("raw.vlite");
        let mut db = Database::create(&path, 2).unwrap();
        db.create_graph("notes_graph").unwrap();
        db.create_graph("plain_graph").unwrap();
        db.create_collection(
            "notes_ev",
            CollectionSpec {
                dimensions: 2,
                settings: Default::default(),
            },
        )
        .unwrap();
        let evidence = db
            .collections()
            .unwrap()
            .into_iter()
            .find(|info| info.name == "notes_ev")
            .unwrap()
            .id;
        let lineage = format!("{:016x}", db.lineage().unwrap());
        let credential = |id: &str, operations: Value, graphs: Value| {
            json!({
                "id": id,
                "name": "fake raw names credential",
                "all": false,
                "collections": [{"name": "notes_ev", "id": evidence}],
                "operations": operations,
                "graphs": graphs,
                "lineage": lineage,
                "secret_sha256": format!("{:x}", Sha256::digest(SECRET.as_bytes())),
                "created_unix_ms": 0,
                "expires_unix_ms": 9_999_999_999_999u64,
                "revoked_unix_ms": null,
                "rotated_from": null
            })
        };
        let graph_operations = json!(["read", "write", "graph_read", "graph_write"]);
        let both = json!(["notes_graph", "plain_graph"]);
        let document = json!({
            "schema": 1,
            "credentials": [
                credential(
                    MEMORY_ID,
                    json!(["read", "search", "write", "graph_read", "graph_write"]),
                    both.clone(),
                ),
                credential(RAW_ID, graph_operations.clone(), both.clone()),
                credential(PLAIN_ID, graph_operations.clone(), both),
                credential(NARROW_ID, graph_operations, json!(["plain_graph"])),
            ]
        });
        std::fs::write(
            directory.path().join("credentials.json"),
            serde_json::to_vec(&document).unwrap(),
        )
        .unwrap();
        plant(&mut db);
        db.close().unwrap();
        let memory_bindings_file = bound.then(|| {
            let file = directory.path().join("memory-bindings.json");
            std::fs::write(&file, bindings()).unwrap();
            file
        });
        let service = Service::new(Database::open(&path).unwrap());
        let options = ServerOptions {
            admin_token: Some(BearerToken::new(ADMIN).unwrap()),
            admin_directory: Some(directory.path().into()),
            memory_bindings_file,
            ..Default::default()
        };
        let app = configured_admin_router(service.clone(), options).expect("the start is accepted");
        Self {
            directory,
            app,
            service,
        }
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Option<&Value>,
    ) -> (u16, Value) {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(
                body.map(|body| serde_json::to_vec(body).unwrap())
                    .unwrap_or_default(),
            ))
            .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn batch(&self, graph: &str, token: &str, ops: Value) -> (u16, Value) {
        self.call(
            "POST",
            &format!("/v1/graphs/{graph}/batch"),
            token,
            Some(&json!({"ops": ops})),
        )
        .await
    }

    async fn nodes(&self, graph: &str) -> u64 {
        let (status, body) = self
            .call("GET", &format!("/v1/graphs/{graph}"), ADMIN, None)
            .await;
        assert_eq!(status, 200, "{body}");
        body["graph"]["node_count"].as_u64().unwrap()
    }

    async fn has_node(&self, graph: &str, key: &str) -> bool {
        let (status, body) = self
            .call(
                "POST",
                &format!("/v1/graphs/{graph}/nodes/get"),
                ADMIN,
                Some(&json!({"node": {"key": key}})),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        !body["node"].is_null()
    }

    async fn close(self) -> TempDir {
        self.service.close().await.unwrap();
        self.directory
    }
}

fn token(id: &str) -> String {
    format!("vlk_{id}_{SECRET}")
}

fn create(key: &str, labels: &[&str]) -> Value {
    json!({"op": "create_node", "key": key, "labels": labels})
}

/// Every request the guard must refuse, each with the strings it must never
/// echo.
fn refused_requests() -> Vec<(String, Value, Vec<String>)> {
    let mut cases = Vec::new();
    for label in LABELS {
        cases.push((
            format!("create_node with label {label}"),
            create("k", &["Person", label]),
            vec![label.to_owned()],
        ));
        cases.push((
            format!("upsert_node with label {label}"),
            json!({"op": "upsert_node", "key": "k", "labels": [label]}),
            vec![label.to_owned()],
        ));
        cases.push((
            format!("patch_node adding label {label}"),
            json!({"op": "patch_node", "key": "person:1", "add_labels": [label]}),
            vec![label.to_owned()],
        ));
    }
    for prefix in PREFIXES {
        let key = format!("{prefix}0123abcd");
        cases.push((
            format!("create_node with key {key}"),
            create(&key, &[]),
            vec![key.clone()],
        ));
        cases.push((
            format!("upsert_node with key {key}"),
            json!({"op": "upsert_node", "key": key, "labels": ["Person"]}),
            vec![key.clone()],
        ));
        cases.push((
            format!("patch_node of key {key}"),
            json!({"op": "patch_node", "key": key, "set": {"x": {"type": "string", "value": "y"}}}),
            vec![key.clone()],
        ));
    }
    cases.push((
        "create_node with the record key".into(),
        create("binding:v1", &[]),
        vec!["binding:v1".into()],
    ));
    cases.push((
        "upsert_node with the record key".into(),
        json!({"op": "upsert_node", "key": "binding:v1", "labels": ["Person"]}),
        vec!["binding:v1".into()],
    ));
    cases.push((
        "patch_node of the record key".into(),
        json!({"op": "patch_node", "key": "binding:v1", "set": {"x": {"type": "string", "value": "y"}}}),
        vec!["binding:v1".into()],
    ));
    cases
}

fn assert_reserved(result: &(u16, Value), what: &str, secrets: &[String]) {
    assert_eq!(result.0, 422, "{what}: {}", result.1);
    assert_eq!(result.1["error"]["code"], "invalid_input", "{what}");
    assert_eq!(result.1["error"]["message"], RESERVED, "{what}");
    let text = result.1.to_string();
    for secret in secrets {
        assert!(!text.contains(secret.as_str()), "{what} echoed {secret}");
    }
}

async fn every_reserved_request_is_refused_and_writes_nothing(bound: bool) {
    let rig = Rig::new(bound, |_| {}).await;
    let (status, body) = rig
        .batch(
            "plain_graph",
            ADMIN,
            json!([create("person:1", &["Person"])]),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let nodes = rig.nodes("plain_graph").await;
    for (what, op, secrets) in refused_requests() {
        let refused = rig.batch("plain_graph", ADMIN, json!([op])).await;
        assert_reserved(&refused, &what, &secrets);
        assert_eq!(rig.nodes("plain_graph").await, nodes, "{what} wrote");
    }
    rig.close().await;
}

#[tokio::test]
async fn reserved_names_are_refused_on_an_unbound_graph_with_a_bindings_file() {
    every_reserved_request_is_refused_and_writes_nothing(true).await;
}

#[tokio::test]
async fn reserved_names_are_refused_when_no_memory_is_configured_at_all() {
    every_reserved_request_is_refused_and_writes_nothing(false).await;
}

#[tokio::test]
async fn one_refused_operation_refuses_the_whole_batch() {
    let rig = Rig::new(false, |_| {}).await;
    let refused = rig
        .batch(
            "plain_graph",
            ADMIN,
            json!([
                create("first", &["Person"]),
                create("second", &["Person"]),
                create("scope:v1:third", &[]),
                create("fourth", &["Person"]),
            ]),
        )
        .await;
    assert_reserved(&refused, "a batch with one reserved key", &[]);
    assert_eq!(rig.nodes("plain_graph").await, 0);
    for key in ["first", "second", "fourth"] {
        assert!(!rig.has_node("plain_graph", key).await, "{key}");
    }
    let accepted = rig
        .batch(
            "plain_graph",
            ADMIN,
            json!([create("first", &["Person"]), create("fourth", &["Person"])]),
        )
        .await;
    assert_eq!(accepted.0, 200, "{}", accepted.1);
    rig.close().await;
}

#[tokio::test]
async fn other_names_edges_and_cleanup_of_planted_nodes_stay_allowed() {
    let rig = Rig::new(false, |db| {
        // Nodes an earlier release let a raw writer plant.
        let mut graph = db.graph_mut("plain_graph").unwrap();
        graph
            .create_node("scope:v1:planted", &["VliteMemoryScope"], &Metadata::new())
            .unwrap();
        graph
            .create_node("legacy", &["VliteMemoryScope", "Person"], &Metadata::new())
            .unwrap();
    })
    .await;
    assert_eq!(rig.nodes("plain_graph").await, 2);
    let ordinary = rig
        .batch(
            "plain_graph",
            ADMIN,
            json!([
                create("person:1", &["Person", "Memory", "VliteMemor", "vlitememory"]),
                create("binding:v1:x", &["Person"]),
                create("mem:v2:x", &["Person"]),
                create("scope:v1", &["Person"]),
                {"op": "create_edge", "key": "mem:v1:edge", "source": "person:1",
                 "relationship": "VliteMemoryRelated", "destination": "legacy"},
                {"op": "patch_node", "key": "person:1", "add_labels": ["Employee"],
                 "set": {"VliteMemory": {"type": "string", "value": "x"}}},
            ]),
        )
        .await;
    assert_eq!(ordinary.0, 200, "{}", ordinary.1);
    // A planted node can be cleaned up through the raw route.
    let cleaned = rig
        .batch(
            "plain_graph",
            ADMIN,
            json!([
                {"op": "delete_node", "key": "scope:v1:planted"},
                {"op": "patch_node", "key": "legacy", "remove_labels": ["VliteMemoryScope"]},
            ]),
        )
        .await;
    assert_eq!(cleaned.0, 200, "{}", cleaned.1);
    assert!(!rig.has_node("plain_graph", "scope:v1:planted").await);
    let (status, body) = rig
        .call(
            "POST",
            "/v1/graphs/plain_graph/nodes/get",
            ADMIN,
            Some(&json!({"node": {"key": "legacy"}})),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["node"]["labels"], json!(["Person"]));
    rig.close().await;
}

#[tokio::test]
async fn a_bound_graph_refuses_reserved_names_even_to_a_raw_access_credential() {
    let rig = Rig::new(true, |_| {}).await;
    let raw = token(RAW_ID);
    let plain = token(PLAIN_ID);
    for (what, op, secrets) in refused_requests() {
        // The answer depends on the request alone: the holder of raw access,
        // everyone else and a graph that does not exist all get the same 422,
        // so it reveals nothing about which graphs are reserved.
        for (who, token) in [
            ("raw", raw.as_str()),
            ("plain", plain.as_str()),
            ("admin", ADMIN),
        ] {
            let refused = rig.batch("notes_graph", token, json!([op.clone()])).await;
            assert_reserved(
                &refused,
                &format!("{who} on the bound graph: {what}"),
                &secrets,
            );
        }
        let missing = rig.batch("no_such_graph", ADMIN, json!([op.clone()])).await;
        assert_reserved(&missing, &format!("a missing graph: {what}"), &secrets);
    }
    // An ordinary batch is unchanged: the holder of raw access writes, every
    // other caller still meets the reservation as the 409 graph_not_found a
    // missing graph gives.
    let ordinary = rig
        .batch(
            "notes_graph",
            &raw,
            json!([create("person:1", &["Person"])]),
        )
        .await;
    assert_eq!(ordinary.0, 200, "{}", ordinary.1);
    for token in [plain.as_str(), ADMIN] {
        let masked = rig
            .batch(
                "notes_graph",
                token,
                json!([create("person:2", &["Person"])]),
            )
            .await;
        assert_eq!(masked.0, 409, "{}", masked.1);
        assert_eq!(masked.1["error"]["code"], "graph_not_found");
    }
    rig.close().await;
}

/// The documented order: the credential's graph scope is checked before the
/// guard looks at the request, so a credential outside the graph meets the
/// same 403 for a reserved and for an ordinary operation, and learns nothing.
#[tokio::test]
async fn a_credential_outside_the_graph_scope_meets_its_403_before_the_guard() {
    let rig = Rig::new(false, |_| {}).await;
    let narrow = token(NARROW_ID);
    let inside = rig
        .batch("plain_graph", &narrow, json!([create("scope:v1:x", &[])]))
        .await;
    assert_reserved(&inside, "inside the credential's graphs", &[]);
    let reserved = rig
        .batch("notes_graph", &narrow, json!([create("scope:v1:x", &[])]))
        .await;
    assert_eq!(reserved.0, 403, "{}", reserved.1);
    assert_eq!(reserved.1["error"]["code"], "forbidden_scope");
    let ordinary = rig
        .batch(
            "notes_graph",
            &narrow,
            json!([create("person:1", &["Person"])]),
        )
        .await;
    assert_eq!(ordinary.0, 403, "{}", ordinary.1);
    assert_eq!(
        ordinary.1, reserved.1,
        "the guard leaked through the scope check"
    );
    rig.close().await;
}

#[tokio::test]
async fn memory_writes_its_own_nodes_beside_the_guard() {
    let rig = Rig::new(true, |_| {}).await;
    let (status, body) = rig
        .call(
            "POST",
            "/v1/memory/notes/remember",
            &token(MEMORY_ID),
            Some(&json!({
                "scope": {"agent": "a"}, "idempotency_key": "k1", "deadline_ms": 5000,
                "items": [{
                    "id": "e1", "kind": "entity", "content": "written by the core",
                    "observed_at_ms": "90",
                    "source": {"source_id": "s1", "source_kind": "caller"},
                }],
            })),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let directory = rig.close().await;
    let path = directory.path().join("raw.vlite");
    let db = Database::open(&path).unwrap();
    let graph = db.graph("notes_graph").unwrap();
    let mut keys = Vec::new();
    let mut cursor = None;
    loop {
        let page = graph
            .find_nodes(
                &vlite::property_graph::FindNodes {
                    label: None,
                    conditions: Vec::new(),
                    limit: Some(1000),
                    cursor,
                },
                &vlite::property_graph::ReadOptions {
                    limits: vlite::property_graph::GraphLimits::CEILING,
                    projection: vlite::property_graph::PropertyProjection::None,
                    cancel: None,
                },
            )
            .unwrap();
        keys.extend(page.output.iter().map(|node| node.key.clone()));
        match page.cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    for prefix in ["mem:v1:", "scope:v1:", "receipt:v1:"] {
        assert!(
            keys.iter().any(|key| key.starts_with(prefix)),
            "the core wrote no {prefix} node: {keys:?}"
        );
    }
    assert!(keys.iter().any(|key| key == "binding:v1"));
    db.close().unwrap();
}
