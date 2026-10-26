#![cfg(all(feature = "server", feature = "graph"))]
//! TEST-015 (feature-agent-graph EPIC-007, ITEM-030; AC-028, FM-017): the
//! existing REST, gRPC and webhook surfaces on a vector-only file, an empty
//! graph-enabled file (records, no graphs) and a populated graph-enabled file
//! (records 1..=4 attached to nodes of graph "g", with an edge).
//!
//! - LEG-017: expiring, deleting (single, batch, import) an attached record
//!   fails with an explicit 409 / `FAILED_PRECONDITION`, never 500; unattached
//!   records keep the vector-only results.
//! - LEG-020: REST `/v1/changes` answers 501 `graph_change_feed_unsupported`
//!   and gRPC `Changes` / `WatchChanges` answer `UNIMPLEMENTED` (before any
//!   stream opens) on graph-enabled files, after authentication; vector-only
//!   files keep the feed.
//! - LEG-019: webhook registration and startup with configured webhooks fail
//!   with `GraphChangeFeedUnsupported` and write nothing; graph-enabled files
//!   never start the delivery worker.
//!
//! The every-variant HTTP/gRPC table is unit-tested next to the private
//! mapping (`src\server.rs`, `src\server\grpc.rs`).

use std::{
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
    CreateOptions, Database, Error, FormatVersion, Metadata,
    property_graph::GraphOp,
    server::{BearerToken, ServerOptions, Service, configured_admin_router},
};

const ADMIN: &str = "graph_surfaces_admin_token_0123456789";
const FAR_FUTURE: u64 = 4_102_444_800_000;
const FEED_UNSUPPORTED: &str = "graph_change_feed_unsupported";
/// The delivery worker ticks every 500 ms; three ticks without a cursor
/// file mean it never started.
const WORKER_TICKS: Duration = Duration::from_millis(1_500);

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

/// Sorted file names in `directory`.
fn listing(directory: &Path) -> Vec<String> {
    let mut names = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
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

#[tokio::test]
async fn leg020_rest_change_feed_is_rejected_on_graph_files() {
    for setup in SETUPS {
        let Fixture {
            _dir, service, app, ..
        } = fixture(setup, false);
        // Authentication runs first on every file.
        let anonymous = call(&app, None, "GET", "/v1/changes", None).await;
        assert_eq!(anonymous.0, StatusCode::UNAUTHORIZED, "{setup:?}");
        let feed = call(&app, Some(ADMIN), "GET", "/v1/changes?limit=20", None).await;
        if setup.graph() {
            assert_error(&feed, StatusCode::NOT_IMPLEMENTED, FEED_UNSUPPORTED);
            // The rejection precedes query, cursor and limit parsing: never
            // 400/422/500 on a graph-enabled file.
            for uri in [
                "/v1/changes?cursor=not-a-cursor",
                "/v1/changes?limit=0",
                "/v1/changes?unknown=1",
            ] {
                let response = call(&app, Some(ADMIN), "GET", uri, None).await;
                assert_error(&response, StatusCode::NOT_IMPLEMENTED, FEED_UNSUPPORTED);
            }
        } else {
            assert_eq!(feed.0, StatusCode::OK, "{}", feed.1);
            assert_eq!(feed.1["changes"].as_array().unwrap().len(), 4);
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
        }
        service.close().await.unwrap();
    }
}

#[tokio::test]
async fn leg019_webhook_registration_is_rejected_on_graph_files_without_side_effects() {
    for setup in SETUPS {
        let Fixture {
            _dir,
            admin_dir,
            service,
            app,
        } = fixture(setup, true);
        // The capability document must not advertise what the file rejects.
        let caps = call(&app, Some(ADMIN), "GET", "/v1/capabilities", None).await;
        assert_eq!(caps.0, StatusCode::OK, "{}", caps.1);
        assert_eq!(
            caps.1["features"]["change_feed"],
            json!(!setup.graph()),
            "{setup:?}"
        );
        assert_eq!(
            caps.1["features"]["webhooks"],
            json!(!setup.graph()),
            "{setup:?}"
        );
        let created = call(
            &app,
            Some(ADMIN),
            "POST",
            "/v1/admin/webhooks",
            Some(webhook()),
        )
        .await;
        let list = call(&app, Some(ADMIN), "GET", "/v1/admin/webhooks", None).await;
        assert_eq!(list.0, StatusCode::OK, "{}", list.1);
        let cursor = admin_dir.join("webhooks.cursor");
        if setup.graph() {
            assert_error(&created, StatusCode::NOT_IMPLEMENTED, FEED_UNSUPPORTED);
            assert_eq!(list.1["items"], json!([]));
            assert!(!admin_dir.join("webhooks.json").exists());
            // No delivery worker: it would write its cursor on the first tick.
            tokio::time::sleep(WORKER_TICKS).await;
            assert!(!cursor.exists());
        } else {
            assert_eq!(created.0, StatusCode::CREATED, "{}", created.1);
            assert_eq!(list.1["items"].as_array().unwrap().len(), 1);
            assert!(admin_dir.join("webhooks.json").exists());
            assert!(wait_for(&cursor).await, "vector-only worker never started");
        }
        service.close().await.unwrap();
    }
}

#[tokio::test]
async fn leg019_startup_with_configured_webhooks_fails_on_graph_files_without_side_effects() {
    // Webhook stores written by a vector-only server: one subscription, and
    // the same store after that subscription is removed.
    let source = tempdir().unwrap();
    let (configured, emptied) = {
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
        let id = created.1["id"].as_str().unwrap();
        let removed = call(
            &app,
            Some(ADMIN),
            "DELETE",
            &format!("/v1/admin/webhooks/{id}"),
            None,
        )
        .await;
        assert_eq!(removed.0, StatusCode::OK, "{}", removed.1);
        let emptied = fs::read(admin_dir.join("webhooks.json")).unwrap();
        service.close().await.unwrap();
        (configured, emptied)
    };

    for setup in SETUPS {
        let dir = tempdir().unwrap();
        let admin_dir = dir.path().join("admin");
        fs::create_dir(&admin_dir).unwrap();
        fs::write(admin_dir.join("webhooks.json"), &configured).unwrap();
        let service = Service::new(database(&dir.path().join("data.vlite"), setup));
        match configured_admin_router(service.clone(), options(Some(&admin_dir))) {
            Ok(app) if !setup.graph() => {
                let list = call(&app, Some(ADMIN), "GET", "/v1/admin/webhooks", None).await;
                assert_eq!(list.0, StatusCode::OK, "{}", list.1);
                assert_eq!(list.1["items"].as_array().unwrap().len(), 1);
                assert!(wait_for(&admin_dir.join("webhooks.cursor")).await);
            }
            Ok(_) => panic!("{setup:?}: startup with configured webhooks succeeded"),
            Err(error) if setup.graph() => {
                let inner = error.get_ref().and_then(|e| e.downcast_ref::<Error>());
                assert!(
                    matches!(inner, Some(Error::GraphChangeFeedUnsupported)),
                    "{setup:?}: {error}"
                );
                // Rejected before the audit log, credential stores, scheduler
                // or delivery cursor touch the admin directory.
                assert_eq!(listing(&admin_dir), ["webhooks.json"]);
                assert_eq!(
                    fs::read(admin_dir.join("webhooks.json")).unwrap(),
                    configured
                );
            }
            Err(error) => panic!("{setup:?}: {error}"),
        }
        service.close().await.unwrap();
    }

    // A store without subscriptions does not block a graph-enabled file, and
    // still no delivery worker starts.
    for setup in [Setup::GraphEmpty, Setup::GraphPopulated] {
        let dir = tempdir().unwrap();
        let admin_dir = dir.path().join("admin");
        fs::create_dir(&admin_dir).unwrap();
        fs::write(admin_dir.join("webhooks.json"), &emptied).unwrap();
        let service = Service::new(database(&dir.path().join("data.vlite"), setup));
        let app = configured_admin_router(service.clone(), options(Some(&admin_dir)))
            .unwrap_or_else(|error| panic!("{setup:?}: {error}"));
        let list = call(&app, Some(ADMIN), "GET", "/v1/admin/webhooks", None).await;
        assert_eq!(list.1["items"], json!([]), "{setup:?}");
        tokio::time::sleep(WORKER_TICKS).await;
        assert!(!admin_dir.join("webhooks.cursor").exists(), "{setup:?}");
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
    async fn leg017_leg020_grpc_rejects_feeds_before_streaming_and_maps_attached_records() {
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
            if setup.graph() {
                assert_status(&changes.unwrap_err(), Code::Unimplemented, FEED_UNSUPPORTED);
                match watch {
                    Ok(_) => panic!("{setup:?}: WatchChanges opened a stream"),
                    Err(status) => assert_status(&status, Code::Unimplemented, FEED_UNSUPPORTED),
                }
                // Rejected before cursor and limit parsing.
                let invalid = client
                    .changes(admin(proto::ChangesRequest {
                        cursor: "not-a-cursor".into(),
                        limit: 1_000_000,
                    }))
                    .await
                    .unwrap_err();
                assert_status(&invalid, Code::Unimplemented, FEED_UNSUPPORTED);
            } else {
                assert_eq!(changes.unwrap().into_inner().changes.len(), 4);
                let mut stream = watch.unwrap().into_inner();
                let first = tokio::time::timeout(Duration::from_secs(10), stream.message())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                assert_eq!(first.changes.len(), 4);
            }

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
