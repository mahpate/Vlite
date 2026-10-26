#![cfg(feature = "server")]
//! TEST-015 (feature-agent-graph EPIC-007, ITEM-030; AC-028, FM-017): the
//! existing REST, gRPC and webhook surfaces on a vector-only file, an empty
//! graph-enabled file (records, no graphs) and a populated graph-enabled file
//! (records 1..=4 attached to nodes of graph "g", with an edge).
//!
//! - LEG-017: expiring, deleting (single, batch, import) an attached record
//!   fails with an explicit 409 / `FAILED_PRECONDITION`, never 500; unattached
//!   records keep the vector-only results.
//! - LEG-020 (v0.6.0): REST `/v1/changes` and gRPC `Changes` /
//!   `WatchChanges` report `graph_created` / `graph_dropped` / `graph_batch`
//!   events on graph-enabled files after authentication, filtered by the
//!   caller's `graph_read` binding; vector events are unchanged.
//! - LEG-019 (v0.6.0): webhooks register and start on graph-enabled files,
//!   graph events reach only subscriptions that list graph kinds (optionally
//!   bound by `graphs`), and stores written by older servers still load.
//!
//! The every-variant HTTP/gRPC table is unit-tested next to the private
//! mapping (`src\server.rs`, `src\server\grpc.rs`).

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
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
    CreateOptions, Database, FormatVersion, Metadata,
    property_graph::GraphOp,
    server::{BearerToken, ServerOptions, Service, configured_admin_router},
};

const ADMIN: &str = "graph_surfaces_admin_token_0123456789";
const FAR_FUTURE: u64 = 4_102_444_800_000;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Setup {
    VectorOnly,
    GraphEmpty,
    GraphPopulated,
}

const SETUPS: [Setup; 3] = [Setup::VectorOnly, Setup::GraphEmpty, Setup::GraphPopulated];

impl Setup {
    fn graph(self) -> bool {
        self != Setup::VectorOnly
    }
}

/// Records 1..=4 in the default collection. The populated file attaches
/// record `n` to node `n{n}` of graph "g" and links `n1` to `n2`.
fn database(path: &Path, setup: Setup) -> Database {
    let mut db = match setup {
        Setup::VectorOnly => Database::create_with_format(path, 2, FormatVersion::V5).unwrap(),
        _ => Database::create_with_graph(path, 2, CreateOptions::default()).unwrap(),
    };
    for id in 1..=4u64 {
        db.put(id, &[id as f32, 1.0], format!("r{id}").as_bytes())
            .unwrap();
    }
    if setup == Setup::GraphPopulated {
        db.create_graph("g").unwrap();
        let mut ops = Vec::new();
        for id in 1..=4u64 {
            ops.push(GraphOp::CreateNode {
                key: format!("n{id}"),
                labels: vec!["Doc".into()],
                properties: Metadata::new(),
            });
            ops.push(GraphOp::Attach {
                node: format!("n{id}"),
                collection: "default".into(),
                record: id,
            });
        }
        ops.push(GraphOp::CreateEdge {
            key: None,
            source: "n1".into(),
            relationship: "LINKS".into(),
            destination: "n2".into(),
            properties: Metadata::new(),
        });
        db.graph_mut("g").unwrap().write_batch(&ops).unwrap();
    }
    db
}

fn options(admin_directory: Option<&Path>) -> ServerOptions {
    ServerOptions {
        admin_token: Some(BearerToken::new(ADMIN).unwrap()),
        admin_directory: admin_directory.map(Path::to_owned),
        ..ServerOptions::default()
    }
}

async fn call(
    app: &Router,
    token: Option<&str>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let body = match body {
        Some(value) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
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
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

#[track_caller]
fn assert_error(response: &(StatusCode, Value), status: StatusCode, code: &str) {
    assert_eq!(response.0, status, "{}", response.1);
    assert_eq!(response.1["error"]["code"], code, "{}", response.1);
}

fn webhook() -> Value {
    json!({
        "url": "http://127.0.0.1:9/hook",
        "events": ["put"],
        "secret": "correct-horse-battery-staple"
    })
}

async fn wait_for(path: &Path) -> bool {
    let started = Instant::now();
    while !path.exists() && started.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    path.exists()
}

struct Fixture {
    _dir: tempfile::TempDir,
    admin_dir: PathBuf,
    service: Service,
    app: Router,
}

fn fixture(setup: Setup, admin: bool) -> Fixture {
    let dir = tempdir().unwrap();
    let admin_dir = dir.path().join("admin");
    let service = Service::new(database(&dir.path().join("data.vlite"), setup));
    let app =
        configured_admin_router(service.clone(), options(admin.then_some(&*admin_dir))).unwrap();
    Fixture {
        _dir: dir,
        admin_dir,
        service,
        app,
    }
}

#[tokio::test]
async fn leg017_attached_record_writes_fail_explicitly_and_others_match_vector_only() {
    let mut baseline = None;
    for setup in SETUPS {
        let Fixture {
            _dir, service, app, ..
        } = fixture(setup, false);
        let attached = setup == Setup::GraphPopulated;
        // LEG-004: replacing record 1 with an expiry.
        let expire = call(
            &app,
            Some(ADMIN),
            "PUT",
            "/v1/vectors/1",
            Some(json!({"vector": [9.0, 9.0], "expires_at_ms": FAR_FUTURE})),
        )
        .await;
        // LEG-002: batch delete of record 2.
        let batch = call(
            &app,
            Some(ADMIN),
            "POST",
            "/v1/batches",
            Some(json!({"operations": [{"op": "delete", "id": "2"}]})),
        )
        .await;
        // LEG-001: delete of record 3.
        let delete = call(&app, Some(ADMIN), "DELETE", "/v1/vectors/3", None).await;
        // LEG-007: import chunk deleting record 4.
        let (status, job) = call(
            &app,
            Some(ADMIN),
            "POST",
            "/v1/collections/default/imports",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{setup:?}: {job}");
        let job = job["job"].as_str().unwrap();
        let import = call(
            &app,
            Some(ADMIN),
            "PUT",
            &format!("/v1/imports/{job}/chunks/1"),
            Some(json!({"operations": [{"op": "delete", "id": "4"}]})),
        )
        .await;
        let mut present = Vec::new();
        for id in 1..=4 {
            let (status, record) =
                call(&app, Some(ADMIN), "GET", &format!("/v1/vectors/{id}"), None).await;
            present.push((status, record["vector"].clone()));
        }
        if attached {
            assert_error(
                &expire,
                StatusCode::CONFLICT,
                "graph_attached_record_expiry",
            );
            for response in [&batch, &delete, &import] {
                assert_error(response, StatusCode::CONFLICT, "graph_attached_record");
            }
            // Refused writes change nothing.
            for (id, (status, vector)) in (1..=4).zip(&present) {
                assert_eq!(*status, StatusCode::OK, "record {id}");
                assert_eq!(*vector, json!([id as f32, 1.0]), "record {id}");
            }
        } else {
            for response in [&expire, &batch, &delete] {
                assert!(response.0.is_success(), "{setup:?}: {response:?}");
            }
            assert_eq!(import.0, StatusCode::OK, "{setup:?}: {}", import.1);
            assert_eq!(import.1["outcome"], "committed");
            let outcome = (
                [expire.0, batch.0, delete.0, import.0],
                present.clone(),
                import.1["rows_committed"].clone(),
            );
            match &baseline {
                None => baseline = Some(outcome),
                Some(expected) => assert_eq!(&outcome, expected, "{setup:?} differs"),
            }
        }
        service.close().await.unwrap();
    }
}

/// `/v1/changes` events with the record payloads stripped: `(sequence,
/// collection, change.type)`.
fn shapes(changes: &Value) -> Vec<(String, String, String)> {
    changes
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["sequence"].as_str().unwrap().to_owned(),
                c["collection"].as_str().unwrap().to_owned(),
                c["change"]["type"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// The vector events every setup reports: puts of records 1..=4.
fn puts() -> Vec<(String, String, String)> {
    (1..=4)
        .map(|s| (s.to_string(), "default".into(), "put".into()))
        .collect()
}

#[tokio::test]
async fn leg020_rest_change_feed_reports_graph_events() {
    for setup in SETUPS {
        let Fixture {
            _dir, service, app, ..
        } = fixture(setup, false);
        // Authentication runs first on every file.
        let anonymous = call(&app, None, "GET", "/v1/changes", None).await;
        assert_eq!(anonymous.0, StatusCode::UNAUTHORIZED, "{setup:?}");
        let feed = call(&app, Some(ADMIN), "GET", "/v1/changes?limit=20", None).await;
        assert_eq!(feed.0, StatusCode::OK, "{}", feed.1);
        let mut expected = puts();
        if setup == Setup::GraphPopulated {
            expected.push(("5".into(), String::new(), "graph_created".into()));
            expected.push(("6".into(), String::new(), "graph_batch".into()));
        }
        assert_eq!(shapes(&feed.1["changes"]), expected, "{setup:?}");
        if setup == Setup::GraphPopulated {
            let created = &feed.1["changes"][4]["change"];
            assert_eq!(*created, json!({"type": "graph_created", "graph": "g"}));
            let batch = &feed.1["changes"][5]["change"];
            assert_eq!(batch["graph"], "g");
            assert_eq!(batch["revision"], "6");
            let operations = batch["operations"].as_array().unwrap();
            assert_eq!(operations.len(), 9, "{batch}");
            assert_eq!(
                operations[0],
                json!({"op": "node_upserted", "id": "0", "key": "n1", "labels": ["Doc"],
                    "properties": {}, "created": true})
            );
            assert_eq!(
                operations[1],
                json!({"op": "attached", "node": "0", "node_key": "n1",
                    "collection": "default", "record": "1"})
            );
            assert_eq!(operations[8]["op"], "edge_upserted");
            assert_eq!(operations[8]["key"], Value::Null);
            assert_eq!(operations[8]["source_key"], "n1");
            assert_eq!(operations[8]["relationship"], "LINKS");
            assert_eq!(operations[8]["destination_key"], "n2");
        }
        let next = feed.1["next_cursor"].as_str().unwrap();
        let after = call(
            &app,
            Some(ADMIN),
            "GET",
            &format!("/v1/changes?cursor={next}"),
            None,
        )
        .await;
        assert_eq!(after.0, StatusCode::OK, "{}", after.1);
        assert_eq!(after.1["changes"], json!([]));
        // Malformed requests fail exactly as on a vector-only file.
        let invalid = call(&app, Some(ADMIN), "GET", "/v1/changes?cursor=bad", None).await;
        assert_eq!(invalid.0, StatusCode::UNPROCESSABLE_ENTITY, "{}", invalid.1);
        assert_eq!(invalid.1["error"]["code"], "invalid_cursor");
        service.close().await.unwrap();
    }
}

/// Scoped readers see graph events only for graphs their `graph_read`
/// binding allows; vector events follow the collection scope as before.
#[tokio::test]
async fn leg020_scoped_feed_readers_see_only_bound_graphs() {
    let Fixture {
        _dir, service, app, ..
    } = fixture(Setup::GraphPopulated, true);
    let created = call(
        &app,
        Some(ADMIN),
        "POST",
        "/v1/graphs",
        Some(json!({"graph": "other"})),
    )
    .await;
    assert_eq!(created.0, StatusCode::CREATED, "{}", created.1);
    let credential = async |scope: Value| {
        let mut request = scope;
        request["name"] = json!("feed");
        request["expires_unix_ms"] = json!(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64
                + 3_600_000
        );
        let (status, body) = call(
            &app,
            Some(ADMIN),
            "POST",
            "/v1/admin/credentials",
            Some(request),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["token"].as_str().unwrap().to_owned()
    };
    let vector = credential(json!({"collections": ["*"], "operations": ["read"]})).await;
    let bound = credential(
        json!({"collections": ["*"], "operations": ["read", "graph_read"], "graphs": ["g"]}),
    )
    .await;
    let all = credential(
        json!({"collections": ["*"], "operations": ["read", "graph_read"], "graphs": ["*"]}),
    )
    .await;
    let graph_names = async |token: &str| {
        let (status, body) = call(&app, Some(token), "GET", "/v1/changes", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let changes = body["changes"].as_array().unwrap();
        assert_eq!(
            changes
                .iter()
                .filter(|c| c["change"]["type"] == "put")
                .count(),
            4
        );
        changes
            .iter()
            .filter_map(|c| c["change"]["graph"].as_str().map(str::to_owned))
            .collect::<Vec<_>>()
    };
    assert!(graph_names(&vector).await.is_empty());
    assert_eq!(graph_names(&bound).await, ["g", "g"]);
    assert_eq!(graph_names(&all).await, ["g", "g", "other"]);
    assert_eq!(graph_names(ADMIN).await, ["g", "g", "other"]);
    service.close().await.unwrap();
}

/// A minimal webhook receiver: every POST is answered 204 and forwarded as
/// `(path, body)`.
fn receiver() -> (String, std::sync::mpsc::Receiver<(String, Value)>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (sender, received) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0u8; 8192];
            let (header_end, length) = loop {
                let size = stream.read(&mut buffer).unwrap_or(0);
                if size == 0 {
                    break (0, 0);
                }
                bytes.extend_from_slice(&buffer[..size]);
                if let Some(index) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..index]).to_ascii_lowercase();
                    let length = headers
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .map_or(0, |v| v.trim().parse::<usize>().unwrap());
                    break (index + 4, length);
                }
            };
            if header_end == 0 {
                continue;
            }
            while bytes.len() < header_end + length {
                let size = stream.read(&mut buffer).unwrap_or(0);
                if size == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..size]);
            }
            let path = String::from_utf8_lossy(&bytes[..header_end])
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_owned();
            let body = serde_json::from_slice(&bytes[header_end..header_end + length])
                .unwrap_or(Value::Null);
            let _ = stream.write_all(
                b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            if sender.send((path, body)).is_err() {
                return;
            }
        }
    });
    (base, received)
}

#[tokio::test]
async fn leg019_webhooks_deliver_graph_events_to_opted_in_subscriptions() {
    for setup in SETUPS {
        let Fixture {
            _dir,
            admin_dir,
            service,
            app,
        } = fixture(setup, true);
        let caps = call(&app, Some(ADMIN), "GET", "/v1/capabilities", None).await;
        assert_eq!(caps.0, StatusCode::OK, "{}", caps.1);
        assert_eq!(caps.1["features"]["change_feed"], json!(true), "{setup:?}");
        assert_eq!(caps.1["features"]["webhooks"], json!(true), "{setup:?}");
        // Invalid graph filters write nothing.
        for graphs in [json!([""]), json!(["x".repeat(64)])] {
            let mut body = webhook();
            body["events"] = json!(["graph_batch"]);
            body["graphs"] = graphs;
            let refused = call(&app, Some(ADMIN), "POST", "/v1/admin/webhooks", Some(body)).await;
            assert_eq!(refused.0, StatusCode::UNPROCESSABLE_ENTITY, "{}", refused.1);
        }
        assert!(!admin_dir.join("webhooks.json").exists());

        let cursor = admin_dir.join("webhooks.cursor");
        assert!(wait_for(&cursor).await, "{setup:?}: worker never started");
        let (base, received) = receiver();
        let subscribe = async |path: &str, events: Value, graphs: Option<Value>| {
            let mut body = webhook();
            body["url"] = json!(format!("{base}/{path}"));
            body["events"] = events;
            if let Some(graphs) = graphs {
                body["graphs"] = graphs;
            }
            let created = call(&app, Some(ADMIN), "POST", "/v1/admin/webhooks", Some(body)).await;
            assert_eq!(created.0, StatusCode::CREATED, "{}", created.1);
            created.1
        };
        // A pre-v0.6.0 style subscription: vector kinds only, no filter.
        let legacy = subscribe("vector", json!(["put"]), None).await;
        assert_eq!(legacy["graphs"], Value::Null);
        let bound = subscribe(
            "bound",
            json!(["graph_created", "graph_batch"]),
            Some(json!(["g"])),
        )
        .await;
        assert_eq!(bound["graphs"], json!(["g"]));
        subscribe("every", json!(["graph_created", "graph_batch"]), None).await;

        call(
            &app,
            Some(ADMIN),
            "PUT",
            "/v1/vectors/5",
            Some(json!({"vector": [5.0, 1.0]})),
        )
        .await;
        if setup.graph() {
            for graph in ["h", "g"] {
                if graph == "g" && setup == Setup::GraphPopulated {
                    continue;
                }
                let created = call(
                    &app,
                    Some(ADMIN),
                    "POST",
                    "/v1/graphs",
                    Some(json!({"graph": graph})),
                )
                .await;
                assert_eq!(created.0, StatusCode::CREATED, "{}", created.1);
            }
            let batch = call(
                &app,
                Some(ADMIN),
                "POST",
                "/v1/graphs/g/batch",
                Some(json!({"ops": [{"op": "create_node", "key": "n9"}]})),
            )
            .await;
            assert_eq!(batch.0, StatusCode::OK, "{}", batch.1);
        }
        // Events may arrive over several worker ticks: legacy 1, "every" 2
        // or 3, "bound" 1 or 2.
        let wanted = match setup {
            Setup::VectorOnly => 1,
            Setup::GraphEmpty => 6,
            Setup::GraphPopulated => 4,
        };
        let mut deliveries = BTreeMap::<String, Vec<Value>>::new();
        let started = Instant::now();
        while deliveries.values().map(Vec::len).sum::<usize>() < wanted {
            let remaining = Duration::from_secs(30).saturating_sub(started.elapsed());
            let Ok((path, body)) = received.recv_timeout(remaining) else {
                panic!("{setup:?}: deliveries so far {deliveries:?}");
            };
            let changes = body["changes"].as_array().unwrap().clone();
            deliveries.entry(path).or_default().extend(changes);
        }
        // Nothing else arrives.
        assert!(
            received.recv_timeout(Duration::from_millis(1_200)).is_err(),
            "{setup:?}"
        );
        let types = |path: &str| {
            deliveries
                .get(path)
                .map(|changes| {
                    changes
                        .iter()
                        .map(|c| {
                            format!(
                                "{}:{}",
                                c["change"]["type"].as_str().unwrap(),
                                c["change"]["graph"]
                                    .as_str()
                                    .unwrap_or(c["collection"].as_str().unwrap())
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        // The legacy subscription receives the unchanged vector payload only.
        assert_eq!(types("/vector"), ["put:default"], "{setup:?}");
        let put = &deliveries["/vector"][0];
        assert_eq!(put["collection"], "default");
        assert_eq!(put["change"]["id"], "5");
        if setup.graph() {
            let mut every = vec!["graph_created:h".to_owned()];
            if setup == Setup::GraphEmpty {
                every.push("graph_created:g".into());
            }
            every.push("graph_batch:g".into());
            let bound = every
                .iter()
                .filter(|e| e.ends_with(":g"))
                .cloned()
                .collect::<Vec<_>>();
            assert_eq!(types("/every"), every, "{setup:?}");
            assert_eq!(types("/bound"), bound, "{setup:?}");
            let batch = deliveries["/bound"].last().unwrap();
            assert_eq!(batch["collection"], "");
            assert_eq!(batch["change"]["operations"][0]["op"], "node_upserted");
            assert_eq!(batch["change"]["operations"][0]["key"], "n9");
        } else {
            assert!(!deliveries.contains_key("/every"));
            assert!(!deliveries.contains_key("/bound"));
        }
        service.close().await.unwrap();
    }
}

#[tokio::test]
async fn leg019_startup_with_configured_webhooks_starts_on_graph_files() {
    // A webhook store written by a vector-only server (no `graphs` field).
    let source = tempdir().unwrap();
    let configured = {
        let admin_dir = source.path().join("admin");
        let service = Service::new(database(
            &source.path().join("data.vlite"),
            Setup::VectorOnly,
        ));
        let app = configured_admin_router(service.clone(), options(Some(&admin_dir))).unwrap();
        let created = call(
            &app,
            Some(ADMIN),
            "POST",
            "/v1/admin/webhooks",
            Some(webhook()),
        )
        .await;
        assert_eq!(created.0, StatusCode::CREATED, "{}", created.1);
        let configured = fs::read(admin_dir.join("webhooks.json")).unwrap();
        service.close().await.unwrap();
        configured
    };
    assert!(!String::from_utf8_lossy(&configured).contains("graphs"));

    for setup in SETUPS {
        let dir = tempdir().unwrap();
        let admin_dir = dir.path().join("admin");
        fs::create_dir(&admin_dir).unwrap();
        fs::write(admin_dir.join("webhooks.json"), &configured).unwrap();
        let service = Service::new(database(&dir.path().join("data.vlite"), setup));
        let app = configured_admin_router(service.clone(), options(Some(&admin_dir)))
            .unwrap_or_else(|error| panic!("{setup:?}: {error}"));
        let list = call(&app, Some(ADMIN), "GET", "/v1/admin/webhooks", None).await;
        assert_eq!(list.0, StatusCode::OK, "{}", list.1);
        let items = list.1["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "{setup:?}");
        assert_eq!(items[0]["events"], json!(["put"]));
        assert_eq!(items[0]["graphs"], Value::Null);
        assert!(
            wait_for(&admin_dir.join("webhooks.cursor")).await,
            "{setup:?}"
        );
        service.close().await.unwrap();
    }
}
#[cfg(feature = "grpc")]
mod grpc {
    use super::*;
    use tonic::{Code, Request as GrpcRequest, Status, transport::Channel};
    use vlite::server::grpc::{
        ERROR_CODE_TRAILER, grpc_router,
        proto::{self, v_lite_client::VLiteClient},
    };

    fn admin<T>(message: T) -> GrpcRequest<T> {
        let mut request = GrpcRequest::new(message);
        request
            .metadata_mut()
            .insert("authorization", format!("Bearer {ADMIN}").parse().unwrap());
        request
    }

    #[track_caller]
    fn assert_status(status: &Status, code: Code, stable: &str) {
        assert_eq!(status.code(), code, "{status:?}");
        assert_eq!(
            status
                .metadata()
                .get(ERROR_CODE_TRAILER)
                .map(|value| value.to_str().unwrap()),
            Some(stable),
            "{status:?}"
        );
    }

    #[tokio::test]
    async fn leg017_leg020_grpc_feeds_report_graph_events_and_map_attached_records() {
        for setup in SETUPS {
            let Fixture {
                _dir, service, app, ..
            } = fixture(setup, false);
            drop(app);
            let server = grpc_router(service.clone()).await;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
            let serving = tokio::spawn(server.serve_with_shutdown(listener, async {
                let _ = stopped.await;
            }));
            let channel = Channel::from_shared(format!("http://{address}"))
                .unwrap()
                .connect()
                .await
                .unwrap();
            let mut client = VLiteClient::new(channel);

            // Authentication runs first on every file.
            let anonymous = client
                .changes(GrpcRequest::new(proto::ChangesRequest::default()))
                .await
                .unwrap_err();
            assert_eq!(anonymous.code(), Code::Unauthenticated, "{setup:?}");

            let changes = client
                .changes(admin(proto::ChangesRequest {
                    limit: 20,
                    ..Default::default()
                }))
                .await;
            let watch = client
                .watch_changes(admin(proto::WatchChangesRequest {
                    limit: 20,
                    poll_interval_ms: 100,
                    ..Default::default()
                }))
                .await;
            let expected = if setup == Setup::GraphPopulated { 6 } else { 4 };
            let changes = changes.unwrap().into_inner().changes;
            assert_eq!(changes.len(), expected, "{setup:?}");
            let mut stream = watch.unwrap().into_inner();
            let first = tokio::time::timeout(Duration::from_secs(10), stream.message())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(first.changes, changes);
            drop(stream);
            if setup == Setup::GraphPopulated {
                let created = &changes[4];
                assert_eq!(created.kind, proto::ChangeKind::GraphCreated as i32);
                assert_eq!(
                    (created.graph.as_str(), created.collection.as_str()),
                    ("g", "")
                );
                let batch = &changes[5];
                assert_eq!(batch.kind, proto::ChangeKind::GraphBatch as i32);
                assert_eq!((batch.graph.as_str(), batch.revision), ("g", Some(6)));
                let ops = &batch.graph_operations;
                assert_eq!(ops.len(), 9);
                assert_eq!(ops[0].op, proto::GraphChangeOp::NodeUpserted as i32);
                assert_eq!((ops[0].id, ops[0].key.as_deref()), (Some(0), Some("n1")));
                assert_eq!(ops[0].labels, ["Doc"]);
                assert!(ops[0].created);
                assert_eq!(ops[1].op, proto::GraphChangeOp::Attached as i32);
                assert_eq!(
                    (
                        ops[1].node,
                        ops[1].node_key.as_str(),
                        ops[1].collection.as_str()
                    ),
                    (Some(0), "n1", "default")
                );
                assert_eq!(ops[1].record, Some(1));
                assert_eq!(ops[8].op, proto::GraphChangeOp::EdgeUpserted as i32);
                assert_eq!(ops[8].key, None);
                assert_eq!(
                    (ops[8].source_key.as_str(), ops[8].relationship.as_str()),
                    ("n1", "LINKS")
                );
                assert_eq!(ops[8].destination_key, "n2");
            }
            // Malformed requests fail exactly as on a vector-only file.
            let invalid = client
                .changes(admin(proto::ChangesRequest {
                    cursor: "not-a-cursor".into(),
                    limit: 20,
                }))
                .await
                .unwrap_err();
            assert_eq!(invalid.code(), Code::InvalidArgument, "{setup:?}");
            // LEG-017 over gRPC: attached records fail with FAILED_PRECONDITION.
            let expire = client
                .put_vector(admin(proto::PutVectorRequest {
                    id: 1,
                    vector: vec![1.0, 1.0],
                    expires_at_ms: Some(FAR_FUTURE),
                    ..Default::default()
                }))
                .await;
            let delete = client
                .delete_vector(admin(proto::DeleteVectorRequest {
                    id: 3,
                    ..Default::default()
                }))
                .await;
            if setup == Setup::GraphPopulated {
                assert_status(
                    &expire.unwrap_err(),
                    Code::FailedPrecondition,
                    "graph_attached_record_expiry",
                );
                assert_status(
                    &delete.unwrap_err(),
                    Code::FailedPrecondition,
                    "graph_attached_record",
                );
            } else {
                expire.unwrap();
                assert!(delete.unwrap().into_inner().deleted);
            }

            drop(client);
            let _ = stop.send(());
            serving.await.unwrap().unwrap();
            service.close().await.unwrap();
        }
    }
}
