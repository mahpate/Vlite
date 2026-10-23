use super::*;
use crate::{
    changes::{Change, ChangeKind, FeedCursor},
    records::{GeoSearchQuery, GeoSort, RecommendationQuery},
    vectors::{DenseSparseQuery, RecordVectors, VectorSpec, VersionedVector},
};
use serde::Deserialize;
use std::collections::BTreeMap;

pub(super) fn routes() -> Router<Service> {
    Router::new()
        .route("/v1/changes", get(changes))
        .route("/v1/collections/{collection}/search/grouped", post(grouped))
        .route("/v1/collections/{collection}/recommend", post(recommend))
        .route("/v1/collections/{collection}/facets", post(facets))
        .route(
            "/v1/collections/{collection}/stats/numeric",
            post(numeric_stats),
        )
        .route("/v1/collections/{collection}/search/geo", post(geo_search))
        .route(
            "/v1/collections/{collection}/vectors/configure",
            post(configure_vector),
        )
        .route(
            "/v1/collections/{collection}/vectors/{id}/named",
            put(put_named_vectors),
        )
        .route(
            "/v1/collections/{collection}/search/named",
            post(search_named),
        )
        .route(
            "/v1/collections/{collection}/search/sparse",
            post(search_sparse),
        )
        .route(
            "/v1/collections/{collection}/search/dense-sparse",
            post(search_dense_sparse),
        )
        .route("/v1/collections/{collection}/query", post(query_language))
        .route("/v1/collections/{collection}/embed", post(embed_text))
        .route(
            "/v1/collections/{collection}/index/disk-ann",
            post(build_disk_ann),
        )
        .route(
            "/v1/collections/{collection}/search/disk-ann",
            post(search_disk_ann),
        )
        .route("/v1/admin/replication/export", get(replication_export))
}

fn collection_ref<'a>(db: &'a Database, collection: u16) -> Result<CollectionRef<'a>, ApiError> {
    Ok(CollectionRef { db, id: collection })
}

fn hits_json(hits: Vec<crate::SearchHit>) -> Vec<Value> {
    hits.into_iter()
        .map(|hit| json!({"id":hit.id.to_string(),"distance":hit.distance}))
        .collect()
}

fn change_json(change: Change) -> Value {
    let kind = match change.kind {
        ChangeKind::Put {
            id,
            record,
            expires_at_ms,
        } => {
            json!({"type":"put","id":id.to_string(),"vector":record.vector,"payload_base64":BASE64_STANDARD.encode(record.payload),"metadata":record.metadata,"expires_at_ms":expires_at_ms})
        }
        ChangeKind::Delete { id } => json!({"type":"delete","id":id.to_string()}),
        ChangeKind::MetadataUpdate { id, metadata } => {
            json!({"type":"metadata_update","id":id.to_string(),"metadata":metadata})
        }
        ChangeKind::CollectionCreated { dimensions, .. } => {
            json!({"type":"collection_created","dimensions":dimensions})
        }
        ChangeKind::CollectionDropped => json!({"type":"collection_dropped"}),
        ChangeKind::SettingsUpdated { .. } => json!({"type":"settings_updated"}),
        ChangeKind::VectorsUpdated => json!({"type":"vectors_updated"}),
    };
    json!({"sequence":change.sequence.to_string(),"ordinal":change.ordinal,"collection":change.collection,"committed_at_ms":change.committed_at_ms,"change":kind})
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChangeQuery {
    cursor: Option<String>,
    limit: Option<usize>,
}

async fn changes(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    query: Result<Query<ChangeQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(query) = query
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, "invalid_query", e.body_text()))?;
    let cursor = query
        .cursor
        .map(|token| FeedCursor::from_token(&token))
        .transpose()?;
    let limit = query.limit.unwrap_or(1000);
    if !(1..=10_000).contains(&limit) {
        return Err(unprocessable("limit must be 1..=10000"));
    }
    let result = service
        .run(move |db| {
            open(db, &role, &Target::Legacy)?;
            let cursor = match cursor {
                Some(cursor) => cursor,
                None => FeedCursor::beginning(),
            };
            db.changes_after(&cursor, limit).map_err(ApiError::from)
        })
        .await?;
    Ok(Json(
        json!({"changes":result.changes.into_iter().map(change_json).collect::<Vec<_>>(),"next_cursor":result.next.to_token(),"has_more":result.has_more,"head":result.head.to_string()}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GroupRequest {
    vector: Vec<f32>,
    field: String,
    groups: usize,
    hits_per_group: usize,
    #[serde(default)]
    filter: Vec<crate::Condition>,
    metric: Option<DistanceMetric>,
}
async fn grouped(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    target: Result<Path<String>, PathRejection>,
    input: Result<Json<GroupRequest>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let target = collection_path(target)?;
    let request = body(input)?;
    let result = service
        .run(move |db| {
            let id = open(db, &role, &target)?;
            let r = collection_ref(db, id)?;
            let metric = request.metric.unwrap_or(identity(db, id, &target).0);
            let groups = r.search_grouped(
                &request.vector,
                request.groups,
                request.hits_per_group,
                &request.field,
                &request.filter,
                metric,
            )?;
            Ok(groups)
        })
        .await?;
    Ok(Json(
        json!({"groups":result.into_iter().map(|g|json!({"value":g.value,"hits":hits_json(g.hits)})).collect::<Vec<_>>()}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecommendRequest {
    positive_ids: Vec<String>,
    #[serde(default)]
    negative_ids: Vec<String>,
    #[serde(default)]
    positive_vectors: Vec<Vec<f32>>,
    #[serde(default)]
    negative_vectors: Vec<Vec<f32>>,
    k: usize,
    #[serde(default)]
    filter: Vec<crate::Condition>,
    metric: Option<DistanceMetric>,
}
async fn recommend(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    target: Result<Path<String>, PathRejection>,
    input: Result<Json<RecommendRequest>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let target = collection_path(target)?;
    let request = body(input)?;
    let positive_ids = request
        .positive_ids
        .iter()
        .map(|s| id(s))
        .collect::<Result<Vec<_>, _>>()?;
    let negative_ids = request
        .negative_ids
        .iter()
        .map(|s| id(s))
        .collect::<Result<Vec<_>, _>>()?;
    let result = service
        .run(move |db| {
            let cid = open(db, &role, &target)?;
            let metric = request.metric.unwrap_or(identity(db, cid, &target).0);
            let query = RecommendationQuery {
                positive_ids,
                negative_ids,
                positive_vectors: request.positive_vectors,
                negative_vectors: request.negative_vectors,
                k: request.k,
                conditions: request.filter,
                metric,
            };
            collection_ref(db, cid)?
                .recommend(&query)
                .map_err(ApiError::from)
        })
        .await?;
    Ok(Json(json!({"hits":hits_json(result)})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldRequest {
    field: String,
    #[serde(default)]
    filter: Vec<crate::Condition>,
}
async fn facets(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    target: Result<Path<String>, PathRejection>,
    input: Result<Json<FieldRequest>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let target = collection_path(target)?;
    let request = body(input)?;
    let result = service
        .run(move |db| {
            let cid = open(db, &role, &target)?;
            collection_ref(db, cid)?
                .facets(&request.field, &request.filter)
                .map_err(ApiError::from)
        })
        .await?;
    Ok(Json(
        json!({"facets":result.into_iter().map(|f|json!({"value":f.value,"count":f.count})).collect::<Vec<_>>()}),
    ))
}
async fn numeric_stats(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    target: Result<Path<String>, PathRejection>,
    input: Result<Json<FieldRequest>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let target = collection_path(target)?;
    let request = body(input)?;
    let result = service
        .run(move |db| {
            let cid = open(db, &role, &target)?;
            collection_ref(db, cid)?
                .numeric_stats(&request.field, &request.filter)
                .map_err(ApiError::from)
        })
        .await?;
    Ok(Json(
        json!({"count":result.count,"min":result.min,"max":result.max,"sum":result.sum,"average":result.average}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GeoRequest {
    vector: Vec<f32>,
    k: usize,
    field: String,
    latitude: f64,
    longitude: f64,
    radius_meters: f64,
    #[serde(default)]
    filter: Vec<crate::Condition>,
    metric: Option<DistanceMetric>,
    #[serde(default)]
    sort: GeoSortName,
}
#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum GeoSortName {
    #[default]
    VectorDistance,
    GeoDistance,
}
async fn geo_search(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    target: Result<Path<String>, PathRejection>,
    input: Result<Json<GeoRequest>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let target = collection_path(target)?;
    let request = body(input)?;
    let result = service
        .run(move |db| {
            let cid = open(db, &role, &target)?;
            let metric = request.metric.unwrap_or(identity(db, cid, &target).0);
            let query = GeoSearchQuery {
                vector: request.vector,
                k: request.k,
                field: request.field,
                latitude: request.latitude,
                longitude: request.longitude,
                radius_meters: request.radius_meters,
                conditions: request.filter,
                metric,
                sort: match request.sort {
                    GeoSortName::VectorDistance => GeoSort::VectorDistance,
                    GeoSortName::GeoDistance => GeoSort::GeoDistance,
                },
            };
            collection_ref(db, cid)?
                .search_geo(&query)
                .map_err(ApiError::from)
        })
        .await?;
    Ok(Json(
        json!({"hits":result.into_iter().map(|h|json!({"id":h.id.to_string(),"vector_distance":h.vector_distance,"geo_distance_meters":h.geo_distance_meters})).collect::<Vec<_>>()}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigureRequest {
    name: String,
    dimensions: usize,
    metric: DistanceMetric,
    active_version: String,
    #[serde(default)]
    history: usize,
}
async fn configure_vector(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    target: Result<Path<String>, PathRejection>,
    input: Result<Json<ConfigureRequest>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let target = collection_path(target)?;
    let request = body(input)?;
    let spec = VectorSpec {
        dimensions: request.dimensions,
        metric: request.metric,
        active_version: request.active_version.clone(),
        history: request.history,
    };
    let name = request.name.clone();
    service
        .run(move |db| {
            let cid = open(db, &role, &target)?;
            crate::CollectionMut { db, id: cid }.configure_vector(&name, spec)?;
            Ok(())
        })
        .await?;
    Ok(Json(
        json!({"configured":request.name,"dimensions":request.dimensions,"metric":request.metric,"active_version":request.active_version,"history":request.history}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NamedPut {
    dense: Option<BTreeMap<String, Vec<NamedVersion>>>,
    sparse: Option<BTreeMap<String, Vec<(u32, f32)>>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NamedVersion {
    version: String,
    vector: Vec<f32>,
}
async fn put_named_vectors(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    path_input: Result<Path<(String, String)>, PathRejection>,
    input: Result<Json<NamedPut>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let (target, record_id) = record_path(path_input)?;
    let request = body(input)?;
    let dense = request
        .dense
        .unwrap_or_default()
        .into_iter()
        .map(|(name, items)| {
            (
                name,
                items
                    .into_iter()
                    .map(|v| VersionedVector {
                        version: v.version,
                        vector: v.vector,
                    })
                    .collect(),
            )
        })
        .collect();
    let vectors = RecordVectors {
        dense,
        sparse: request.sparse.unwrap_or_default(),
    };
    service
        .run(move |db| {
            let cid = open(db, &role, &target)?;
            crate::CollectionMut { db, id: cid }.put_vectors(record_id, vectors)?;
            Ok(())
        })
        .await?;
    Ok(Json(json!({"updated":true})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NamedSearch {
    name: String,
    version: Option<String>,
    vector: Vec<f32>,
    k: usize,
}
async fn search_named(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    target: Result<Path<String>, PathRejection>,
    input: Result<Json<NamedSearch>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let target = collection_path(target)?;
    let request = body(input)?;
    let hits = service
        .run(move |db| {
            let _cid = open(db, &role, &target)?;
            db.collection(target.name())?
                .search_named(
                    &request.name,
                    request.version.as_deref(),
                    &request.vector,
                    request.k,
                )
                .map_err(ApiError::from)
        })
        .await?;
    Ok(Json(json!({"hits":hits_json(hits)})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SparseSearch {
    name: String,
    vector: Vec<(u32, f32)>,
    k: usize,
}
async fn search_sparse(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    target: Result<Path<String>, PathRejection>,
    input: Result<Json<SparseSearch>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let target = collection_path(target)?;
    let request = body(input)?;
    let hits = service
        .run(move |db| {
            let _cid = open(db, &role, &target)?;
            db.collection(target.name())?
                .search_sparse(&request.name, &request.vector, request.k)
                .map_err(ApiError::from)
        })
        .await?;
    Ok(Json(json!({"hits":hits_json(hits)})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DenseSparseSearch {
    dense_name: String,
    version: Option<String>,
    dense: Vec<f32>,
    sparse_name: String,
    sparse: Vec<(u32, f32)>,
    k: usize,
    fusion: Option<FusionRequest>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FusionRequest {
    candidates: Option<usize>,
    rrf_k: Option<u32>,
    vector_weight: Option<f64>,
    text_weight: Option<f64>,
}
async fn search_dense_sparse(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    target: Result<Path<String>, PathRejection>,
    input: Result<Json<DenseSparseSearch>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let target = collection_path(target)?;
    let request = body(input)?;
    let hits = service
        .run(move |db| {
            let cid = open(db, &role, &target)?;
            let defaults = crate::hybrid::FusionOptions::default();
            let supplied = request.fusion;
            let fusion = crate::hybrid::FusionOptions {
                candidates: supplied.as_ref().and_then(|f| f.candidates),
                rrf_k: supplied
                    .as_ref()
                    .and_then(|f| f.rrf_k)
                    .unwrap_or(defaults.rrf_k),
                vector_weight: supplied
                    .as_ref()
                    .and_then(|f| f.vector_weight)
                    .unwrap_or(defaults.vector_weight),
                text_weight: supplied
                    .as_ref()
                    .and_then(|f| f.text_weight)
                    .unwrap_or(defaults.text_weight),
            };
            let query = DenseSparseQuery {
                dense_name: request.dense_name,
                version: request.version,
                dense: request.dense,
                sparse_name: request.sparse_name,
                sparse: request.sparse,
                k: request.k,
                fusion,
            };
            collection_ref(db, cid)?
                .search_dense_sparse(&query)
                .map_err(ApiError::from)
        })
        .await?;
    Ok(Json(
        json!({"hits":hits.into_iter().map(|h|json!({"id":h.id.to_string(),"score":h.score,"distance":h.distance,"bm25":h.bm25,"vector_rank":h.vector_rank,"text_rank":h.text_rank})).collect::<Vec<_>>()}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LanguageRequest {
    query: String,
    #[serde(default)]
    vectors: BTreeMap<String, Vec<f32>>,
}
async fn query_language(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    target: Result<Path<String>, PathRejection>,
    input: Result<Json<LanguageRequest>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let target = collection_path(target)?;
    let request = body(input)?;
    let (hits, groups, facets) = service
        .run(move |db| {
            let cid = open(db, &role, &target)?;
            let parsed = crate::query_language::parse(&request.query)
                .map_err(|e| unprocessable(e.to_string()))?;
            let output = parsed
                .execute(collection_ref(db, cid)?, &request.vectors)
                .map_err(ApiError::from)?;
            Ok((output.hits, output.groups, output.facets))
        })
        .await?;
    Ok(Json(
        json!({"hits":hits_json(hits),"groups":groups.into_iter().map(|g|json!({"value":g.value,"hits":hits_json(g.hits)})).collect::<Vec<_>>(),"facets":facets.into_iter().map(|f|json!({"value":f.value,"count":f.count})).collect::<Vec<_>>()}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmbedRequest {
    text: String,
    provider: Option<String>,
}

async fn embed_text(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    target: Result<Path<String>, PathRejection>,
    input: Result<Json<EmbedRequest>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let target = collection_path(target)?;
    let request = body(input)?;
    if request.text.len() > 16_384 {
        return Err(unprocessable("text must be at most 16384 UTF-8 bytes"));
    }
    let provider = request.provider.as_deref().unwrap_or("hashing").to_owned();
    let target_for_dimensions = target.clone();
    let dimensions = service
        .run(move |db| {
            let collection = open(db, &role, &target_for_dimensions)?;
            db.collections()?
                .into_iter()
                .find(|info| info.id == collection)
                .map(|info| info.dimensions)
                .ok_or_else(|| ApiError::from(Error::CollectionNotFound))
        })
        .await?;
    let embedder: Box<dyn crate::embedding::Embedder> = match provider.as_str() {
        "hashing" => {
            Box::new(crate::embedding::HashEmbedder::new(dimensions).map_err(ApiError::from)?)
        }
        "http" => {
            #[cfg(feature = "http-embedder")]
            {
                Box::new(crate::embedding::HttpEmbedder::from_env().map_err(ApiError::from)?)
            }
            #[cfg(not(feature = "http-embedder"))]
            {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "http_embedder_disabled",
                    "build with the http-embedder feature to enable the HTTP provider",
                ));
            }
        }
        _ => return Err(unprocessable("provider must be hashing or http")),
    };
    let text = request.text;
    let vector = tokio::task::spawn_blocking(move || embedder.embed_text(&text))
        .await
        .map_err(internal)?
        .map_err(ApiError::from)?;
    if vector.len() != dimensions {
        return Err(unprocessable(
            "embedding dimensions do not match the collection",
        ));
    }
    Ok(Json(
        json!({"vector":vector,"dimensions":dimensions,"provider":provider}),
    ))
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct DiskBuildRequest {
    metric: DistanceMetric,
    config: admin::IndexConfig,
}
async fn build_disk_ann(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    target: Result<Path<String>, PathRejection>,
    input: Result<Json<DiskBuildRequest>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let target = collection_path(target)?;
    let request = body(input)?;
    let config = request.config.validated()?;
    let dir = service.admin.directory.clone().ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "admin_directory_disabled",
            "disk ANN requires VLITE_ADMIN_DIR",
        )
    })?;
    let result=service.run(move|db|{let cid=open(db,&role,&target)?;let name=db.collections()?.into_iter().find(|c|c.id==cid).map(|c|c.name).ok_or(Error::CollectionNotFound)?;let path=dir.join(format!("disk-ann-{cid}.dann"));let source=db.build_disk_ann(&name,&path,config,request.metric)?;Ok(json!({"lineage":source.lineage.to_string(),"sequence":source.sequence.to_string(),"collection_id":source.collection_id,"live_records":source.live_records,"path_name":path.file_name().and_then(|n|n.to_str()).unwrap_or_default()}))}).await?;
    Ok(Json(result))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskSearchRequest {
    vector: Vec<f32>,
    k: usize,
    ef_search: usize,
}
async fn search_disk_ann(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    target: Result<Path<String>, PathRejection>,
    input: Result<Json<DiskSearchRequest>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let target = collection_path(target)?;
    let request = body(input)?;
    let dir = service.admin.directory.clone().ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "admin_directory_disabled",
            "disk ANN requires VLITE_ADMIN_DIR",
        )
    })?;
    let hits = service
        .run(move |db| {
            let cid = open(db, &role, &target)?;
            let path = dir.join(format!("disk-ann-{cid}.dann"));
            let index = crate::DiskAnnIndex::open(path, crate::DiskAnnOptions::default())?;
            db.search_disk_ann(&index, &request.vector, request.k, request.ef_search)
                .map_err(ApiError::from)
        })
        .await?;
    Ok(Json(json!({"hits":hits_json(hits)})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportQuery {
    after_sequence: Option<String>,
    max_bytes: Option<usize>,
}
async fn replication_export(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    query: Result<Query<ExportQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(query) = query
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, "invalid_query", e.body_text()))?;
    let after = query
        .after_sequence
        .as_deref()
        .map(id)
        .transpose()?
        .unwrap_or(0);
    let max = query.max_bytes.unwrap_or(1 << 20);
    if !(1..=16 * 1024 * 1024).contains(&max) {
        return Err(unprocessable("max_bytes must be 1..=16777216"));
    }
    let segment = service
        .run(move |db| {
            open(db, &role, &Target::Legacy)?;
            db.export_log(after, max).map_err(ApiError::from)
        })
        .await?;
    let bytes = segment.to_bytes();
    Ok(Json(
        json!({"segment_base64":BASE64_STANDARD.encode(bytes),"lineage":segment.lineage.to_string(),"base_sequence":segment.base_sequence.to_string(),"last_sequence":segment.last_sequence.to_string(),"head_sequence":segment.head_sequence.to_string()}),
    ))
}
