#![cfg(feature = "grpc")]

use std::collections::HashMap;
use tonic::{Request, transport::Channel};
use vlite::{
    CreateOptions, Database, EncryptionKey, FormatVersion, Metadata, MetadataValue,
    server::{
        BearerToken, ServerOptions, Service, configured_admin_router,
        grpc::{
            grpc_router,
            proto::{self, v_lite_client::VLiteClient},
        },
    },
};

const ADMIN: &str = "grpc_features_admin_token_0123456789";

fn admin<T>(message: T) -> Request<T> {
    let mut request = Request::new(message);
    request
        .metadata_mut()
        .insert("authorization", format!("Bearer {ADMIN}").parse().unwrap());
    request
}

fn typed_string(value: &str) -> proto::MetadataValue {
    proto::MetadataValue {
        kind: Some(proto::metadata_value::Kind::StringValue(value.into())),
    }
}

async fn start(
    db: Database,
) -> (
    VLiteClient<Channel>,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let service = Service::new(db);
    let _router = configured_admin_router(
        service.clone(),
        ServerOptions {
            admin_token: Some(BearerToken::new(ADMIN).unwrap()),
            ..ServerOptions::default()
        },
    )
    .unwrap();
    let grpc = grpc_router(service).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        grpc.serve_with_shutdown(listener, async {
            let _ = stopped.await;
        })
        .await
        .unwrap();
    });
    let channel = Channel::from_shared(format!("http://{address}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    (VLiteClient::new(channel), stop, task)
}

#[tokio::test]
async fn exposes_v5_search_vectors_change_feed_disk_ann_and_info() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("feature.vlite");
    let mut db = Database::create_with_format(&path, 2, FormatVersion::V5).unwrap();
    let mut first = Metadata::new();
    first.insert("label".into(), MetadataValue::String("a".into()));
    first.insert("score".into(), MetadataValue::Integer(7));
    first.insert(
        "location".into(),
        MetadataValue::GeoPoint {
            latitude: 47.0,
            longitude: -122.0,
        },
    );
    db.put_with_metadata(1, &[1.0, 0.0], b"one", &first)
        .unwrap();
    let mut second = Metadata::new();
    second.insert("label".into(), MetadataValue::String("b".into()));
    second.insert("score".into(), MetadataValue::Integer(9));
    second.insert(
        "location".into(),
        MetadataValue::GeoPoint {
            latitude: 47.001,
            longitude: -122.0,
        },
    );
    db.put_with_metadata(2, &[0.0, 1.0], b"two", &second)
        .unwrap();
    let (mut client, stop, server) = start(db).await;

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let mut ttl_put = proto::PutVectorRequest {
        id: 3,
        vector: vec![0.7, 0.7],
        expires_at_ms: Some(now_ms + 600_000),
        ..Default::default()
    };
    ttl_put.metadata.insert("label".into(), typed_string("a"));
    let expected_expiry = ttl_put.expires_at_ms;
    let inserted = client
        .put_vector(admin(ttl_put))
        .await
        .unwrap()
        .into_inner();
    assert!(inserted.created);
    assert_eq!(inserted.expires_at_ms, expected_expiry);
    let read_back = client
        .get_vector(admin(proto::GetVectorRequest {
            id: 3,
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(read_back.expires_at_ms, expected_expiry);
    let updated = client
        .update_metadata(admin(proto::UpdateMetadataRequest {
            id: 3,
            metadata: HashMap::from([("label".into(), typed_string("c"))]),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(updated.revision > inserted.revision);

    let groups = client
        .search_grouped(admin(proto::GroupedSearchRequest {
            vector: vec![1.0, 0.0],
            groups: 4,
            hits_per_group: 2,
            field: "label".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(!groups.groups.is_empty());
    let recommendations = client
        .recommend(admin(proto::RecommendRequest {
            positive_ids: vec![1],
            k: 2,
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(recommendations.hits.len(), 2);
    let facets = client
        .facets(admin(proto::FacetsRequest {
            field: "label".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(facets.facets.iter().map(|facet| facet.count).sum::<u64>() >= 3);
    let stats = client
        .numeric_stats(admin(proto::NumericStatsRequest {
            field: "score".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(stats.count, 2);
    let geo = client
        .geo_search(admin(proto::GeoSearchRequest {
            vector: vec![1.0, 0.0],
            k: 3,
            field: "location".into(),
            latitude: 47.0,
            longitude: -122.0,
            radius_meters: 500.0,
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(geo.hits.len(), 2);

    let reranked = client
        .search(admin(proto::SearchRequest {
            vector: vec![1.0, 0.0],
            k: 2,
            rerank_mode: proto::RerankMode::Exact as i32,
            rerank_candidates: Some(3),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(reranked.hits.first().unwrap().id, 1);
    let mmr = client
        .search(admin(proto::SearchRequest {
            vector: vec![1.0, 0.0],
            k: 2,
            rerank_mode: proto::RerankMode::Mmr as i32,
            mmr_lambda: Some(0.4),
            rerank_candidates: Some(3),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(mmr.hits.len(), 2);

    client
        .configure_named_vector(admin(proto::ConfigureNamedVectorRequest {
            spec: Some(proto::NamedVectorSpec {
                name: "text".into(),
                dimensions: 2,
                metric: proto::Metric::SquaredL2 as i32,
                active_version: "v1".into(),
                history: 2,
            }),
            ..Default::default()
        }))
        .await
        .unwrap();
    client
        .put_named_vectors(admin(proto::PutNamedVectorsRequest {
            id: 1,
            dense: vec![proto::NamedVector {
                name: "text".into(),
                version: "v1".into(),
                vector: vec![1.0, 0.0],
            }],
            sparse: vec![proto::SparseVector {
                name: "terms".into(),
                indices: vec![10, 20],
                values: vec![1.0, 0.5],
            }],
            ..Default::default()
        }))
        .await
        .unwrap();
    client
        .put_vector_version(admin(proto::PutVectorVersionRequest {
            id: 2,
            name: "text".into(),
            version: "v1".into(),
            vector: vec![0.0, 1.0],
            ..Default::default()
        }))
        .await
        .unwrap();
    assert_eq!(
        client
            .search_named(admin(proto::SearchNamedRequest {
                name: "text".into(),
                vector: vec![1.0, 0.0],
                k: 3,
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner()
            .hits[0]
            .id,
        1
    );
    assert_eq!(
        client
            .search_sparse(admin(proto::SearchSparseRequest {
                name: "terms".into(),
                indices: vec![10],
                values: vec![1.0],
                k: 3,
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner()
            .hits[0]
            .id,
        1
    );
    let dense_sparse = client
        .search_dense_sparse(admin(proto::SearchDenseSparseRequest {
            dense_name: "text".into(),
            dense: vec![1.0, 0.0],
            sparse_name: "terms".into(),
            sparse_indices: vec![10],
            sparse_values: vec![1.0],
            k: 2,
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(!dense_sparse.hybrid_hits.is_empty());

    let query = client
        .query(admin(proto::QueryRequest {
            query: "SEARCH default NEAR $q K 2".into(),
            parameters: HashMap::from([(
                "q".into(),
                proto::VectorInput {
                    vector: vec![1.0, 0.0],
                },
            )]),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(query.hits.first().unwrap().id, 1);

    let built = client
        .build_disk_ann(admin(proto::BuildDiskAnnRequest {
            path: directory
                .path()
                .join("index.dann")
                .to_string_lossy()
                .into_owned(),
            metric: proto::Metric::SquaredL2 as i32,
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(built.live_records >= 3);
    let disk_hits = client
        .search_disk_ann(admin(proto::SearchDiskAnnRequest {
            path: directory
                .path()
                .join("index.dann")
                .to_string_lossy()
                .into_owned(),
            vector: vec![1.0, 0.0],
            k: 2,
            ef_search: 8,
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(!disk_hits.hits.is_empty());

    let changes = client
        .changes(admin(proto::ChangesRequest {
            limit: 100,
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(changes.changes.iter().any(
        |change| change.kind == proto::ChangeKind::Put as i32 && change.expires_at_ms.is_some()
    ));
    client
        .update_metadata(admin(proto::UpdateMetadataRequest {
            id: 1,
            metadata: HashMap::from([("label".into(), typed_string("watch"))]),
            ..Default::default()
        }))
        .await
        .unwrap();
    let mut watch = client
        .watch_changes(admin(proto::WatchChangesRequest {
            cursor: changes.cursor,
            limit: 100,
            poll_interval_ms: 100,
        }))
        .await
        .unwrap()
        .into_inner();
    let watched = tokio::time::timeout(std::time::Duration::from_secs(10), watch.message())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        watched
            .changes
            .iter()
            .any(|change| change.kind == proto::ChangeKind::MetadataUpdate as i32)
    );
    drop(watch);
    let info = client
        .info(admin(proto::InfoRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(info.format_version, 5);
    assert!(!info.encrypted);
    assert!(!info.is_replica);

    drop(client);
    let _ = stop.send(());
    server.await.unwrap();
}

#[tokio::test]
async fn info_reports_encryption_without_exposing_key_material() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::create_with_options(
        directory.path().join("encrypted.vlite"),
        2,
        CreateOptions {
            format: Some(FormatVersion::V5),
            encryption: Some(EncryptionKey::from_bytes([0x5a; 32])),
            commit_timestamps: false,
        },
    )
    .unwrap();
    let (mut client, stop, server) = start(database).await;
    let info = client
        .info(admin(proto::InfoRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert!(info.encrypted);
    assert!(info.encryption_key_id.is_some());
    drop(client);
    let _ = stop.send(());
    server.await.unwrap();
}
