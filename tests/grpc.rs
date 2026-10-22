#![cfg(feature = "grpc")]
//! TEST-005..007: gRPC transport parity, policy and limits. Every RPC runs
//! against the same `Service` as the REST router, so REST is the oracle.

use axum::{
    Router,
    body::Body,
    http::{Request as HttpRequest, StatusCode, header},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::{collections::HashMap, path::Path};
use tonic::{Code, Request, Status, transport::Channel};
use tower::ServiceExt;
use vlite::{
    Database, FormatVersion,
    server::{
        BearerToken, ServerOptions, Service, configured_admin_router,
        grpc::{
            ERROR_CODE_TRAILER, grpc_router,
            proto::{self, v_lite_client::VLiteClient},
        },
    },
};

const ADMIN: &str = "admin_secret_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const READER: &str = "reader_secret_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";

struct Harness {
    app: Router,
    client: VLiteClient<Channel>,
    health: tonic_health::pb::health_client::HealthClient<Channel>,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

async fn start(db: Database, admin_dir: Option<&Path>) -> Harness {
    let options = ServerOptions {
        admin_token: Some(BearerToken::new(ADMIN).unwrap()),
        read_only_token: Some(BearerToken::new(READER).unwrap()),
        admin_directory: admin_dir.map(Path::to_owned),
        ..ServerOptions::default()
    };
    let service = Service::new(db);
    let app = configured_admin_router(service.clone(), options).unwrap();
    let grpc = grpc_router(service).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(grpc.serve_with_shutdown(listener, async {
        let _ = rx.await;
    }));
    let channel = Channel::from_shared(format!("http://{address}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    Harness {
        app,
        client: VLiteClient::new(channel.clone())
            .max_encoding_message_size(64 * 1024 * 1024)
            .max_decoding_message_size(64 * 1024 * 1024),
        health: tonic_health::pb::health_client::HealthClient::new(channel),
        _shutdown: tx,
    }
}

fn v4(path: &Path) -> Database {
    Database::create_with_format(path, 2, FormatVersion::V4).unwrap()
}

fn req<T>(token: Option<&str>, message: T) -> Request<T> {
    let mut request = Request::new(message);
    if let Some(token) = token {
        request
            .metadata_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
    }
    request
}

fn admin<T>(message: T) -> Request<T> {
    req(Some(ADMIN), message)
}

/// Asserts the gRPC code and the stable REST code in the trailer.
fn assert_error(status: &Status, code: Code, stable: &str) {
    assert_eq!(status.code(), code, "{status:?}");
    assert_eq!(
        status
            .metadata()
            .get(ERROR_CODE_TRAILER)
            .map(|value| value.to_str().unwrap()),
        Some(stable),
        "{status:?}"
    );
    assert!(status.metadata().get("x-request-id").is_some());
}

async fn rest(
    app: &Router,
    token: Option<&str>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = HttpRequest::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let body = match body {
        Some(value) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
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

async fn rest_ok(app: &Router, method: &str, uri: &str, body: Option<Value>) -> Value {
    let (status, value) = rest(app, Some(ADMIN), method, uri, body).await;
    assert!(status.is_success(), "{method} {uri}: {status} {value}");
    value
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

async fn metric(app: &Router, name: &str) -> u64 {
    rest_ok(app, "GET", "/metrics", None).await[name]
        .as_u64()
        .unwrap()
}

fn string(value: &str) -> proto::MetadataValue {
    proto::MetadataValue {
        kind: Some(proto::metadata_value::Kind::StringValue(value.into())),
    }
}

fn integer(value: i64) -> proto::MetadataValue {
    proto::MetadataValue {
        kind: Some(proto::metadata_value::Kind::IntegerValue(value)),
    }
}

fn absent() -> proto::Expected {
    proto::Expected {
        kind: Some(proto::expected::Kind::Absent(proto::expected::Absent {})),
    }
}

fn revision(revision: u64) -> proto::Expected {
    proto::Expected {
        kind: Some(proto::expected::Kind::Revision(revision)),
    }
}

fn put(collection: &str, id: u64, vector: Vec<f32>) -> proto::PutVectorRequest {
    proto::PutVectorRequest {
        collection: collection.into(),
        id,
        vector,
        ..Default::default()
    }
}

fn put_op(id: u64, vector: Vec<f32>) -> proto::Operation {
    proto::Operation {
        kind: Some(proto::operation::Kind::Put(proto::operation::Put {
            id,
            vector,
            ..Default::default()
        })),
    }
}

fn batch(collection: &str, operations: Vec<proto::Operation>) -> proto::WriteBatchRequest {
    proto::WriteBatchRequest {
        collection: collection.into(),
        operations,
    }
}

fn get(collection: &str, id: u64) -> proto::GetVectorRequest {
    proto::GetVectorRequest {
        collection: collection.into(),
        id,
    }
}

fn delete(
    collection: &str,
    id: u64,
    expected: Option<proto::Expected>,
) -> proto::DeleteVectorRequest {
    proto::DeleteVectorRequest {
        collection: collection.into(),
        id,
        expected,
    }
}

fn search(collection: &str, vector: Vec<f32>, k: u32) -> proto::SearchRequest {
    proto::SearchRequest {
        collection: collection.into(),
        vector,
        k,
        ..Default::default()
    }
}

fn create(name: &str, settings_json: Option<&str>) -> proto::CreateCollectionRequest {
    proto::CreateCollectionRequest {
        name: name.into(),
        dimensions: 2,
        settings_json: settings_json.map(str::to_owned),
    }
}

fn rest_hits(value: &Value) -> Vec<(u64, f64)> {
    value["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| {
            (
                hit["id"].as_str().unwrap().parse().unwrap(),
                hit["distance"].as_f64().unwrap(),
            )
        })
        .collect()
}

fn grpc_hits(response: &proto::SearchResponse) -> Vec<(u64, f64)> {
    response
        .hits
        .iter()
        .map(|hit| (hit.id, hit.distance))
        .collect()
}

#[tokio::test]
async fn every_rpc_matches_rest_on_the_same_service() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = start(v4(&dir.path().join("db.vlite")), None).await;

    // Health is public and SERVING for "" and the service name.
    for service in ["", "vlite.v1.VLite"] {
        let response = h
            .health
            .check(tonic_health::pb::HealthCheckRequest {
                service: service.into(),
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(response.status, 1, "SERVING for {service:?}");
    }

    // Collections.
    let created = h
        .client
        .create_collection(admin(create("docs", Some(r#"{"metric":"cosine"}"#))))
        .await
        .unwrap();
    assert!(created.metadata().get("x-request-id").is_some());
    let created = created.into_inner();
    let rest_docs = rest_ok(&h.app, "GET", "/v1/collections/docs", None).await;
    assert_eq!(created.name, "docs");
    assert_eq!(created.dimensions, 2);
    assert_eq!(u64::from(created.id), rest_docs["id"].as_u64().unwrap());
    assert_eq!(
        serde_json::from_str::<Value>(&created.settings_json).unwrap(),
        rest_docs["settings"]
    );
    assert!(created.settings_persisted);
    let fetched = h
        .client
        .get_collection(admin(proto::GetCollectionRequest {
            name: "docs".into(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(fetched, created);
    let listed = h
        .client
        .list_collections(admin(proto::ListCollectionsRequest {}))
        .await
        .unwrap()
        .into_inner();
    let rest_list = rest_ok(&h.app, "GET", "/v1/collections", None).await;
    let names = listed
        .items
        .iter()
        .map(|item| item.name.as_str())
        .collect::<Vec<_>>();
    let rest_names = rest_list["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, rest_names);
    assert_eq!(names, ["default", "docs"]);
    assert_eq!(
        u64::from(listed.format_version),
        rest_list["format_version"].as_u64().unwrap()
    );
    let error = h
        .client
        .create_collection(admin(create("docs", None)))
        .await
        .unwrap_err();
    assert_error(&error, Code::AlreadyExists, "collection_exists");
    let error = h
        .client
        .create_collection(admin(create("bad", Some("{"))))
        .await
        .unwrap_err();
    assert_error(&error, Code::InvalidArgument, "invalid_json");
    let error = h
        .client
        .get_collection(admin(proto::GetCollectionRequest {
            name: "missing".into(),
        }))
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::NotFound);

    // Vectors: put via gRPC, read via REST; put via REST, read via gRPC.
    let mut request = put("docs", 7, vec![1., 0.]);
    request.payload = b"\x00\xffpayload".to_vec();
    request.metadata = HashMap::from([("tag".into(), string("a")), ("n".into(), integer(3))]);
    request.expected = Some(absent());
    let written = h
        .client
        .put_vector(admin(request))
        .await
        .unwrap()
        .into_inner();
    assert!(written.created);
    let rest_vector = rest_ok(&h.app, "GET", "/v1/collections/docs/vectors/7", None).await;
    assert_eq!(rest_vector["revision"], written.revision.to_string());
    assert_eq!(rest_vector["vector"], json!([1.0, 0.0]));
    assert_eq!(rest_vector["payload_base64"], "AP9wYXlsb2Fk");
    assert_eq!(
        rest_vector["metadata"],
        json!({"tag":{"type":"string","value":"a"},"n":{"type":"integer","value":"3"}})
    );
    let vector = h.client.get_vector(admin(get("docs", 7))).await.unwrap();
    assert!(vector.metadata().get("x-request-id").is_some());
    let vector = vector.into_inner();
    assert_eq!(vector.id, 7);
    assert_eq!(vector.vector, [1., 0.]);
    assert_eq!(vector.payload, b"\x00\xffpayload");
    assert_eq!(vector.metadata["tag"], string("a"));
    assert_eq!(vector.metadata["n"], integer(3));
    assert_eq!(vector.revision, written.revision);
    rest_ok(
        &h.app,
        "PUT",
        "/v1/collections/docs/vectors/8",
        Some(json!({"vector":[0,1]})),
    )
    .await;
    let other = h.client.get_vector(admin(get("docs", 8))).await.unwrap();
    assert_eq!(other.into_inner().vector, [0., 1.]);

    // Replace with a revision: not created; REST observes the new revision.
    let mut request = put("docs", 7, vec![0.6, 0.8]);
    request.expected = Some(revision(written.revision));
    let replaced = h
        .client
        .put_vector(admin(request))
        .await
        .unwrap()
        .into_inner();
    assert!(!replaced.created);
    assert_ne!(replaced.revision, written.revision);
    let rest_vector = rest_ok(&h.app, "GET", "/v1/collections/docs/vectors/7", None).await;
    assert_eq!(rest_vector["revision"], replaced.revision.to_string());
    assert_eq!(rest_vector["metadata"], json!({}));

    // Exact search (the unspecified mode) matches REST hits.
    let grpc = h
        .client
        .search(admin(search("docs", vec![1., 0.], 10)))
        .await
        .unwrap()
        .into_inner();
    let rest_search = rest_ok(
        &h.app,
        "POST",
        "/v1/collections/docs/search",
        Some(json!({"vector":[1,0],"k":10})),
    )
    .await;
    assert_eq!(grpc_hits(&grpc), rest_hits(&rest_search));
    assert_eq!(grpc.hits.len(), 2);

    // Filtered search through the shared filter validation.
    rest_ok(
        &h.app,
        "PUT",
        "/v1/collections/docs/vectors/9",
        Some(json!({"vector":[1,0],"metadata":{"tag":{"type":"string","value":"b"},"n":{"type":"integer","value":"5"}}})),
    )
    .await;
    let mut request = search("docs", vec![1., 0.], 10);
    request.filter = vec![
        proto::Condition {
            kind: Some(proto::condition::Kind::Eq(proto::condition::Eq {
                field: "tag".into(),
                value: Some(string("b")),
            })),
        },
        proto::Condition {
            kind: Some(proto::condition::Kind::Range(proto::condition::Range {
                field: "n".into(),
                min: Some(integer(4)),
                max: None,
            })),
        },
    ];
    let grpc = h.client.search(admin(request)).await.unwrap().into_inner();
    let rest_search = rest_ok(
        &h.app,
        "POST",
        "/v1/collections/docs/search",
        Some(json!({"vector":[1,0],"k":10,"filter":[
            {"op":"eq","field":"tag","value":{"type":"string","value":"b"}},
            {"op":"range","field":"n","min":{"type":"integer","value":"4"}}]})),
    )
    .await;
    assert_eq!(grpc_hits(&grpc), rest_hits(&rest_search));
    assert_eq!(grpc.hits.len(), 1);
    assert_eq!(grpc.hits[0].id, 9);

    // Batch then delete, observed through REST.
    let remove = proto::Operation {
        kind: Some(proto::operation::Kind::Delete(proto::operation::Delete {
            id: 8,
        })),
    };
    h.client
        .write_batch(admin(batch(
            "docs",
            vec![put_op(10, vec![0.5, 0.5]), remove],
        )))
        .await
        .unwrap();
    assert_eq!(
        rest(
            &h.app,
            Some(ADMIN),
            "GET",
            "/v1/collections/docs/vectors/8",
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    rest_ok(&h.app, "GET", "/v1/collections/docs/vectors/10", None).await;
    let deleted = h
        .client
        .delete_vector(admin(delete("docs", 10, None)))
        .await;
    assert!(deleted.unwrap().into_inner().deleted);
    let deleted = h
        .client
        .delete_vector(admin(delete("docs", 10, None)))
        .await;
    assert!(!deleted.unwrap().into_inner().deleted);

    // Legacy default via an empty collection, same as /v1/vectors.
    h.client
        .put_vector(admin(put("", 1, vec![3., 4.])))
        .await
        .unwrap();
    let legacy = rest_ok(&h.app, "GET", "/v1/vectors/1", None).await;
    assert_eq!(legacy["vector"], json!([3.0, 4.0]));
    let missing = h.client.get_vector(admin(get("", 99))).await.unwrap_err();
    assert_error(&missing, Code::NotFound, "not_found");

    // Drop requires an empty collection, like REST.
    let error = h
        .client
        .drop_collection(admin(proto::DropCollectionRequest {
            name: "docs".into(),
        }))
        .await
        .unwrap_err();
    assert_error(&error, Code::FailedPrecondition, "collection_not_empty");
    let removals = [7, 9]
        .map(|id| proto::Operation {
            kind: Some(proto::operation::Kind::Delete(proto::operation::Delete {
                id,
            })),
        })
        .to_vec();
    h.client
        .write_batch(admin(batch("docs", removals)))
        .await
        .unwrap();
    let dropped = h
        .client
        .drop_collection(admin(proto::DropCollectionRequest {
            name: "docs".into(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(dropped.deleted);
    assert_eq!(
        rest(&h.app, Some(ADMIN), "GET", "/v1/collections/docs", None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );

    // gRPC calls and failures are recorded in the shared metrics.
    assert!(metric(&h.app, "requests_total").await >= 20);
    assert!(metric(&h.app, "request_failures_total").await >= 4);
}

#[tokio::test]
async fn hybrid_search_and_resumable_import_match_rest() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = start(v4(&dir.path().join("db.vlite")), None).await;
    let created = h
        .client
        .create_collection(admin(create("docs", Some(r#"{"text_fields":["title"]}"#))))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(created.name, "docs");

    for (id, vector, title) in [
        (1, vec![1.0, 0.0], "rust vector database"),
        (2, vec![0.0, 1.0], "python vector search"),
    ] {
        h.client
            .put_vector(admin(proto::PutVectorRequest {
                collection: "docs".into(),
                id,
                vector,
                metadata: HashMap::from([("title".into(), string(title))]),
                ..Default::default()
            }))
            .await
            .unwrap();
    }

    let request = proto::SearchRequest {
        collection: "docs".into(),
        vector: vec![0.0, 1.0],
        vector_enabled: true,
        text: Some("rust".into()),
        k: 2,
        mode: proto::SearchMode::Hybrid as i32,
        fusion: Some(proto::FusionOptions {
            candidates: Some(2),
            rrf_k: Some(60),
            vector_weight: Some(1.0),
            text_weight: Some(1.0),
        }),
        ..Default::default()
    };
    let grpc = h.client.search(admin(request)).await.unwrap().into_inner();
    assert!(grpc.hits.is_empty());
    let rest_hybrid = rest_ok(
        &h.app,
        "POST",
        "/v1/collections/docs/search",
        Some(json!({"vector":[0,1],"text":"rust","k":2,"mode":"hybrid",
            "fusion":{"candidates":2,"rrf_k":60,"vector_weight":1,"text_weight":1}})),
    )
    .await;
    assert_eq!(
        grpc.hybrid_hits.len(),
        rest_hybrid["hits"].as_array().unwrap().len()
    );
    for (grpc_hit, rest_hit) in grpc
        .hybrid_hits
        .iter()
        .zip(rest_hybrid["hits"].as_array().unwrap())
    {
        assert_eq!(
            grpc_hit.id,
            rest_hit["id"].as_str().unwrap().parse::<u64>().unwrap()
        );
        assert_eq!(grpc_hit.score, rest_hit["score"].as_f64().unwrap());
        assert_eq!(grpc_hit.distance, rest_hit["distance"].as_f64());
        assert_eq!(grpc_hit.bm25, rest_hit["bm25"].as_f64());
        assert_eq!(
            grpc_hit.vector_rank.map(u64::from),
            rest_hit["vector_rank"].as_u64()
        );
        assert_eq!(
            grpc_hit.text_rank.map(u64::from),
            rest_hit["text_rank"].as_u64()
        );
    }

    let text_only = h
        .client
        .search(admin(proto::SearchRequest {
            collection: "docs".into(),
            text: Some("python".into()),
            k: 2,
            mode: proto::SearchMode::Hybrid as i32,
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(text_only.hybrid_hits[0].distance.is_none());
    assert_eq!(text_only.hybrid_hits[0].id, 2);

    let reserved = h
        .client
        .reserve_import(admin(proto::ReserveImportRequest {
            collection: "docs".into(),
        }))
        .await
        .unwrap()
        .into_inner();
    let reserved: Value = serde_json::from_str(&reserved.json).unwrap();
    let job = reserved["job"].as_str().unwrap().parse::<u64>().unwrap();
    assert_eq!(reserved["state"], "open");
    assert_eq!(reserved["durable"], false);

    let status = h
        .client
        .get_import_status(admin(proto::ImportJobRequest { job }))
        .await
        .unwrap()
        .into_inner();
    let status: Value = serde_json::from_str(&status.json).unwrap();
    assert_eq!(status["job"], job.to_string());
    assert_eq!(status["durable"], false);

    let chunk = proto::ImportChunkRequest {
        job,
        chunk: 1,
        operations: vec![put_op(99, vec![0.5, 0.5])],
        policy: proto::ImportPolicy::RejectChunk as i32,
    };
    let result = h
        .client
        .import_chunk(admin(chunk.clone()))
        .await
        .unwrap()
        .into_inner();
    let result: Value = serde_json::from_str(&result.json).unwrap();
    assert_eq!(result["outcome"], "committed");
    assert_eq!(result["rows_committed"], 1);
    assert_eq!(result["status"]["durable"], true);

    let replay = h
        .client
        .import_chunk(admin(chunk))
        .await
        .unwrap()
        .into_inner();
    let replay: Value = serde_json::from_str(&replay.json).unwrap();
    assert_eq!(replay["outcome"], "already_committed");
    assert_eq!(
        rest_ok(&h.app, "GET", "/v1/collections/docs/vectors/99", None).await["vector"],
        json!([0.5, 0.5])
    );

    let finished = h
        .client
        .finish_import(admin(proto::ImportJobRequest { job }))
        .await
        .unwrap()
        .into_inner();
    let finished: Value = serde_json::from_str(&finished.json).unwrap();
    assert_eq!(finished["state"], "finished");
    assert_eq!(finished["durable"], true);

    let status = h
        .client
        .get_import_status(admin(proto::ImportJobRequest { job }))
        .await
        .unwrap()
        .into_inner();
    let status: Value = serde_json::from_str(&status.json).unwrap();
    assert_eq!(status["state"], "finished");

    let rejected_job = h
        .client
        .reserve_import(admin(proto::ReserveImportRequest {
            collection: "docs".into(),
        }))
        .await
        .unwrap()
        .into_inner();
    let rejected_job: Value = serde_json::from_str(&rejected_job.json).unwrap();
    let rejected_job = rejected_job["job"]
        .as_str()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    let invalid = h
        .client
        .import_chunk(admin(proto::ImportChunkRequest {
            job: rejected_job,
            chunk: 1,
            operations: vec![put_op(88, vec![1.0])],
            policy: proto::ImportPolicy::RejectChunk as i32,
        }))
        .await
        .unwrap_err();
    assert_error(&invalid, Code::InvalidArgument, "import_rows_invalid");
    let details = invalid
        .metadata()
        .get("vlite-error-details-base64")
        .unwrap()
        .to_str()
        .unwrap();
    let details: Value = serde_json::from_slice(&STANDARD.decode(details).unwrap()).unwrap();
    assert_eq!(details["outcome"], "rejected");
    assert_eq!(details["invalid_rows"], 1);
    assert!(details["invalid"].is_array());
    let aborted = h
        .client
        .abort_import(admin(proto::ImportJobRequest { job: rejected_job }))
        .await
        .unwrap()
        .into_inner();
    let aborted: Value = serde_json::from_str(&aborted.json).unwrap();
    assert_eq!(aborted["state"], "aborted");
    assert_eq!(aborted["durable"], false);

    // The added operations participate in the same authorization path.
    let denied = h
        .client
        .reserve_import(req(
            Some(READER),
            proto::ReserveImportRequest {
                collection: "docs".into(),
            },
        ))
        .await
        .unwrap_err();
    assert_error(&denied, Code::PermissionDenied, "read_only");
}

#[tokio::test]
async fn expected_preconditions_are_atomic_and_scoped_to_the_collection() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = start(v4(&dir.path().join("db.vlite")), None).await;
    for name in ["a", "b"] {
        h.client
            .create_collection(admin(create(name, None)))
            .await
            .unwrap();
    }
    let mut request = put("a", 1, vec![1., 0.]);
    request.expected = Some(absent());
    let first = h
        .client
        .put_vector(admin(request))
        .await
        .unwrap()
        .into_inner();

    // Absent is per collection: the same id in `b` is absent.
    let mut request = put("b", 1, vec![0., 1.]);
    request.expected = Some(absent());
    let in_b = h
        .client
        .put_vector(admin(request))
        .await
        .unwrap()
        .into_inner();
    assert!(in_b.created);

    // A second absent put in `a` fails without changing the record.
    let mut request = put("a", 1, vec![5., 5.]);
    request.expected = Some(absent());
    let error = h.client.put_vector(admin(request)).await.unwrap_err();
    assert_error(&error, Code::Aborted, "precondition_failed");

    // Only `a`'s own current revision satisfies a guard in `a`.
    for stale in [in_b.revision, first.revision.wrapping_add(1000)] {
        if stale == first.revision {
            continue;
        }
        let mut request = put("a", 1, vec![5., 5.]);
        request.expected = Some(revision(stale));
        let error = h.client.put_vector(admin(request)).await.unwrap_err();
        assert_error(&error, Code::Aborted, "precondition_failed");
    }
    let stored = rest_ok(&h.app, "GET", "/v1/collections/a/vectors/1", None).await;
    assert_eq!(stored["vector"], json!([1.0, 0.0]));

    // A revision guard on an absent record fails rather than creating it.
    let mut request = put("a", 2, vec![5., 5.]);
    request.expected = Some(revision(first.revision));
    let error = h.client.put_vector(admin(request)).await.unwrap_err();
    assert_error(&error, Code::Aborted, "precondition_failed");

    // Delete guarded by a stale revision fails; by the current one succeeds.
    let stale = Some(revision(first.revision.wrapping_add(1000)));
    let error = h
        .client
        .delete_vector(admin(delete("a", 1, stale)))
        .await
        .unwrap_err();
    assert_error(&error, Code::Aborted, "precondition_failed");
    let deleted = h
        .client
        .delete_vector(admin(delete("a", 1, Some(revision(first.revision)))))
        .await
        .unwrap();
    assert!(deleted.into_inner().deleted);

    // Concurrent absent puts: exactly one wins.
    let mut tasks = Vec::new();
    for i in 0..8 {
        let mut client = h.client.clone();
        tasks.push(tokio::spawn(async move {
            let mut request = put("a", 42, vec![i as f32, 1.]);
            request.expected = Some(absent());
            client.put_vector(admin(request)).await
        }));
    }
    let mut wins = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => wins += 1,
            Err(status) => assert_error(&status, Code::Aborted, "precondition_failed"),
        }
    }
    assert_eq!(wins, 1);
}

#[tokio::test]
async fn authentication_roles_and_scoped_credentials_match_rest() {
    let dir = tempfile::tempdir().unwrap();
    let admin_dir = dir.path().join("admin");
    let mut h = start(v4(&dir.path().join("db.vlite")), Some(&admin_dir)).await;
    for name in ["docs", "secret"] {
        rest_ok(
            &h.app,
            "POST",
            "/v1/collections",
            Some(json!({"name":name,"dimensions":2})),
        )
        .await;
        rest_ok(
            &h.app,
            "PUT",
            &format!("/v1/collections/{name}/vectors/1"),
            Some(json!({"vector":[1,0]})),
        )
        .await;
    }

    // Missing and invalid tokens: Unauthenticated, recorded.
    let before = metric(&h.app, "unauthorized_total").await;
    for token in [None, Some("not-a-valid-token")] {
        let error = h
            .client
            .list_collections(req(token, proto::ListCollectionsRequest {}))
            .await
            .unwrap_err();
        assert_error(&error, Code::Unauthenticated, "unauthorized");
    }
    assert_eq!(metric(&h.app, "unauthorized_total").await - before, 2);

    // Read-only: reads and exact search allowed; every mutation denied.
    let reader = Some(READER);
    h.client
        .get_vector(req(reader, get("docs", 1)))
        .await
        .unwrap();
    h.client
        .search(req(reader, search("docs", vec![1., 0.], 1)))
        .await
        .unwrap();
    let denials = [
        h.client
            .put_vector(req(reader, put("docs", 2, vec![0., 1.])))
            .await
            .unwrap_err(),
        h.client
            .delete_vector(req(reader, delete("docs", 1, None)))
            .await
            .unwrap_err(),
        h.client
            .write_batch(req(reader, batch("docs", vec![put_op(3, vec![1., 1.])])))
            .await
            .unwrap_err(),
        h.client
            .create_collection(req(reader, create("x", None)))
            .await
            .unwrap_err(),
        h.client
            .drop_collection(req(
                reader,
                proto::DropCollectionRequest {
                    name: "docs".into(),
                },
            ))
            .await
            .unwrap_err(),
    ];
    for error in &denials {
        assert_error(error, Code::PermissionDenied, "read_only");
    }
    rest_ok(&h.app, "GET", "/v1/collections/docs/vectors/1", None).await;

    // Scoped credential bound to `docs` with read+search.
    let (status, created) = rest(
        &h.app,
        Some(ADMIN),
        "POST",
        "/v1/admin/credentials",
        Some(
            json!({"name":"svc","collections":["docs"],"operations":["read","search"],
            "expires_unix_ms": now_ms() + 600_000}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let token = created["token"].as_str().unwrap().to_owned();
    let scoped = Some(token.as_str());
    h.client
        .get_vector(req(scoped, get("docs", 1)))
        .await
        .unwrap();
    h.client
        .search(req(scoped, search("docs", vec![1., 0.], 1)))
        .await
        .unwrap();
    let listed = h
        .client
        .list_collections(req(scoped, proto::ListCollectionsRequest {}))
        .await
        .unwrap()
        .into_inner();
    let names = listed
        .items
        .iter()
        .map(|item| item.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["docs"]);
    let before = metric(&h.app, "credential_denials_total").await;
    let scoped_denials = [
        // Other collection, unknown collection, legacy default.
        h.client
            .get_vector(req(scoped, get("secret", 1)))
            .await
            .unwrap_err(),
        h.client
            .get_collection(req(
                scoped,
                proto::GetCollectionRequest {
                    name: "missing".into(),
                },
            ))
            .await
            .unwrap_err(),
        h.client
            .get_vector(req(scoped, get("", 1)))
            .await
            .unwrap_err(),
        // Operations outside the grant.
        h.client
            .put_vector(req(scoped, put("docs", 2, vec![0., 1.])))
            .await
            .unwrap_err(),
        h.client
            .create_collection(req(scoped, create("x", None)))
            .await
            .unwrap_err(),
    ];
    for error in &scoped_denials {
        assert_eq!(error.code(), Code::PermissionDenied, "{error:?}");
        assert!(error.metadata().get(ERROR_CODE_TRAILER).is_some());
    }
    let after = metric(&h.app, "credential_denials_total").await;
    assert!(after - before >= 5, "{before} -> {after}");
    let audit = rest_ok(&h.app, "GET", "/v1/admin/audit/persistent?limit=1000", None).await;
    let text = audit.to_string();
    assert!(text.contains("credential_denied"));
    assert!(!text.contains(&token[17..]), "secret never audited");

    // Health stays public.
    let response = h
        .health
        .check(tonic_health::pb::HealthCheckRequest {
            service: "vlite.v1.VLite".into(),
        })
        .await
        .unwrap();
    assert_eq!(response.into_inner().status, 1);
}

#[tokio::test]
async fn ann_search_is_ready_only_and_never_builds() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = start(v4(&dir.path().join("db.vlite")), None).await;
    let operations = (0..64)
        .map(|i| put_op(i, vec![i as f32, 1.]))
        .collect::<Vec<_>>();
    h.client
        .write_batch(admin(batch("", operations)))
        .await
        .unwrap();
    let mut ready = search("", vec![3., 1.], 3);
    ready.mode = proto::SearchMode::ApproximateReady as i32;
    for _ in 0..2 {
        let error = h.client.search(admin(ready.clone())).await.unwrap_err();
        assert_error(&error, Code::FailedPrecondition, "index_not_ready");
    }
    // gRPC never triggered a build.
    let status = rest_ok(&h.app, "GET", "/v1/index/hnsw", None).await;
    assert_ne!(status["status"], "ready", "{status}");

    rest_ok(&h.app, "POST", "/v1/index/hnsw", Some(json!({}))).await;
    let grpc = h.client.search(admin(ready)).await.unwrap().into_inner();
    let rest_search = rest_ok(
        &h.app,
        "POST",
        "/v1/search",
        Some(json!({"vector":[3,1],"k":3,"mode":"approximate_ready"})),
    )
    .await;
    assert_eq!(grpc_hits(&grpc), rest_hits(&rest_search));
    assert_eq!(grpc.hits[0].id, 3);

    // Unknown enum values are rejected rather than defaulted.
    let mut bad = search("", vec![3., 1.], 3);
    bad.mode = 99;
    let error = h.client.search(admin(bad)).await.unwrap_err();
    assert_eq!(error.code(), Code::InvalidArgument);
    assert!(error.metadata().get(ERROR_CODE_TRAILER).is_some());
}

#[tokio::test]
async fn limits_and_validation_commit_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = start(v4(&dir.path().join("db.vlite")), None).await;
    let app = h.app.clone();
    let live = || {
        let app = app.clone();
        async move {
            rest_ok(&app, "GET", "/v1/collections/default", None).await["live_records"]
                .as_u64()
                .unwrap()
        }
    };

    // 4097 operations.
    let operations = (0..4097).map(|i| put_op(i, vec![1., 1.])).collect();
    let error = h
        .client
        .write_batch(admin(batch("", operations)))
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::InvalidArgument, "{error:?}");
    assert!(error.metadata().get(ERROR_CODE_TRAILER).is_some());
    assert_eq!(live().await, 0);

    // An invalid last operation (wrong dimensions) rejects the whole batch.
    let mut operations = (0..10).map(|i| put_op(i, vec![1., 1.])).collect::<Vec<_>>();
    operations.push(put_op(10, vec![1., 1., 1.]));
    let error = h
        .client
        .write_batch(admin(batch("", operations)))
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::InvalidArgument, "{error:?}");
    assert_eq!(live().await, 0);

    // So does an unset oneof in the last operation.
    let operations = vec![put_op(1, vec![1., 1.]), proto::Operation { kind: None }];
    let error = h
        .client
        .write_batch(admin(batch("", operations)))
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::InvalidArgument);
    assert_eq!(live().await, 0);

    // Messages over 8 MiB are rejected by the decoder before any work.
    let mut request = put("", 1, vec![1., 1.]);
    request.payload = vec![7; 9 * 1024 * 1024];
    let error = h.client.put_vector(admin(request)).await.unwrap_err();
    assert_eq!(error.code(), Code::OutOfRange, "{error:?}");
    assert_eq!(live().await, 0);

    // A payload over the per-record limit (under the message limit).
    let mut request = put("", 1, vec![1., 1.]);
    request.payload = vec![7; 1024 * 1024 + 1];
    let error = h.client.put_vector(admin(request)).await.unwrap_err();
    assert_eq!(error.code(), Code::InvalidArgument, "{error:?}");
    assert_eq!(live().await, 0);

    // k bounds, dimension mismatch and non-finite vectors.
    h.client
        .put_vector(admin(put("", 1, vec![1., 1.])))
        .await
        .unwrap();
    for k in [10_001, u32::MAX] {
        let error = h
            .client
            .search(admin(search("", vec![1., 1.], k)))
            .await
            .unwrap_err();
        assert_eq!(error.code(), Code::InvalidArgument, "k={k}: {error:?}");
    }
    let error = h
        .client
        .search(admin(search("", vec![1., 1., 1.], 1)))
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::InvalidArgument);
    let error = h
        .client
        .put_vector(admin(put("", 2, vec![f32::NAN, 1.])))
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::InvalidArgument);
    assert_eq!(live().await, 1);

    // A metadata value without a kind.
    let mut request = put("", 3, vec![1., 1.]);
    request.metadata = HashMap::from([("x".into(), proto::MetadataValue { kind: None })]);
    let error = h.client.put_vector(admin(request)).await.unwrap_err();
    assert_eq!(error.code(), Code::InvalidArgument);
    assert_eq!(live().await, 1);
}
