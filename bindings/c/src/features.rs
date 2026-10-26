//! Additive feature operations carried by the existing owned JSON ABI.
use super::*;
use vlite::changes::{ChangeKind, FeedCursor};
use vlite::vectors::{DenseSparseQuery, RecordVectors, VectorSpec, VersionedVector};
use vlite::{CreateOptions, EncryptionKey, GeoSearchQuery, GeoSort, RecommendationQuery};

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Request {
    Create {
        path: Option<String>,
        dimensions: usize,
        #[serde(default)]
        options: Options,
    },
    Open {
        path: String,
        #[serde(default)]
        credentials: Credentials,
        #[serde(default)]
        read_only: bool,
        #[serde(default)]
        durability: Option<String>,
        #[serde(default)]
        flush_interval_ms: Option<u64>,
    },
    OpenFromBytes {
        bytes_base64: String,
        #[serde(default)]
        credentials: Credentials,
    },
    ToBytes {},
    EncryptionInfo {},
    PutWithExpiry {
        id: String,
        vector: Vec<f32>,
        #[serde(default)]
        payload_base64: String,
        #[serde(default)]
        metadata: Metadata,
        expires_at_ms: Option<String>,
        collection: Option<String>,
    },
    UpdateMetadata {
        id: String,
        metadata: Metadata,
        collection: Option<String>,
    },
    SearchGrouped {
        vector: Vec<f32>,
        groups: usize,
        hits_per_group: usize,
        field: String,
        #[serde(default)]
        filter: Vec<vlite::Condition>,
        metric: Option<DistanceMetric>,
        collection: Option<String>,
    },
    Recommend {
        k: usize,
        #[serde(default)]
        positive_ids: Vec<String>,
        #[serde(default)]
        negative_ids: Vec<String>,
        #[serde(default)]
        positive_vectors: Vec<Vec<f32>>,
        #[serde(default)]
        negative_vectors: Vec<Vec<f32>>,
        #[serde(default)]
        filter: Vec<vlite::Condition>,
        metric: Option<DistanceMetric>,
        collection: Option<String>,
    },
    Facets {
        field: String,
        #[serde(default)]
        filter: Vec<vlite::Condition>,
        collection: Option<String>,
    },
    NumericStats {
        field: String,
        #[serde(default)]
        filter: Vec<vlite::Condition>,
        collection: Option<String>,
    },
    SearchGeo {
        vector: Vec<f32>,
        k: usize,
        field: String,
        latitude: f64,
        longitude: f64,
        radius_meters: f64,
        #[serde(default)]
        filter: Vec<vlite::Condition>,
        metric: Option<DistanceMetric>,
        #[serde(default)]
        sort: GeoOrder,
        collection: Option<String>,
    },
    ConfigureVector {
        name: String,
        dimensions: usize,
        #[serde(default)]
        metric: DistanceMetric,
        active_version: String,
        #[serde(default)]
        history: usize,
        collection: Option<String>,
    },
    VectorSpecs {
        collection: Option<String>,
    },
    RecordVectors {
        id: String,
        collection: Option<String>,
    },
    PutVectors {
        id: String,
        #[serde(default)]
        dense: BTreeMap<String, Vec<Version>>,
        #[serde(default)]
        sparse: BTreeMap<String, Vec<(u32, f32)>>,
        collection: Option<String>,
    },
    PutVectorVersion {
        id: String,
        name: String,
        version: String,
        vector: Vec<f32>,
        collection: Option<String>,
    },
    PruneVectorHistory {
        name: String,
        history: usize,
        collection: Option<String>,
    },
    SearchNamed {
        name: String,
        version: Option<String>,
        vector: Vec<f32>,
        k: usize,
        collection: Option<String>,
    },
    SearchSparse {
        name: String,
        vector: Vec<(u32, f32)>,
        k: usize,
        collection: Option<String>,
    },
    SearchDenseSparse {
        dense_name: String,
        version: Option<String>,
        dense: Vec<f32>,
        sparse_name: String,
        sparse: Vec<(u32, f32)>,
        k: usize,
        fusion: Option<Fusion>,
        collection: Option<String>,
    },
    Rerank {
        vector: Vec<f32>,
        candidates: Vec<Candidate>,
        limit: usize,
        #[serde(default)]
        method: RerankMethod,
        metric: Option<DistanceMetric>,
        lambda: Option<f64>,
        collection: Option<String>,
    },
    Query {
        query: String,
        #[serde(default)]
        parameters: BTreeMap<String, Vec<f32>>,
        collection: Option<String>,
    },
    FeedCursor {},
    ChangesAfter {
        cursor: Option<String>,
        #[serde(default = "feed_limit")]
        max_events: usize,
    },
    RestoreTo {
        log_path: String,
        destination: String,
        base_backup: Option<String>,
        sequence: Option<String>,
        timestamp_ms: Option<String>,
        #[serde(default)]
        credentials: Credentials,
    },
    BuildDiskAnn {
        path: String,
        metric: Option<DistanceMetric>,
        collection: Option<String>,
    },
    SearchDiskAnn {
        path: String,
        vector: Vec<f32>,
        k: usize,
        #[serde(default = "ef_default")]
        ef_search: usize,
        #[serde(default = "cache_default")]
        cache_bytes: usize,
    },
    SaveAnnSnapshot {
        path: String,
    },
    LoadAnnSnapshot {
        path: String,
    },
}
fn feed_limit() -> usize {
    1000
}
fn ef_default() -> usize {
    100
}
fn cache_default() -> usize {
    vlite::disk_ann::DEFAULT_CACHE_BYTES
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Credentials {
    key_base64: Option<String>,
    passphrase: Option<String>,
}
impl Credentials {
    fn key(self) -> Result<Option<EncryptionKey>> {
        match (self.key_base64, self.passphrase) {
            (Some(_), Some(_)) => Err(invalid("provide key or passphrase, not both")),
            (Some(text), None) => Ok(Some(EncryptionKey::from_bytes(
                decode(&text)?
                    .try_into()
                    .map_err(|_| invalid("key must contain exactly 32 bytes"))?,
            ))),
            (None, Some(text)) => EncryptionKey::from_passphrase(text)
                .map(Some)
                .map_err(failure),
            (None, None) => Ok(None),
        }
    }
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Options {
    format: Option<u32>,
    #[serde(default)]
    credentials: Credentials,
    #[serde(default)]
    commit_timestamps: bool,
    /// `"periodic"` (file default, every 60 s), `"full"`, or `"none"`
    /// (the only mode for in-memory databases).
    #[serde(default)]
    durability: Option<String>,
    /// `Durability::Periodic` flush interval (1..=60000 ms).
    #[serde(default)]
    flush_interval_ms: Option<u64>,
}
/// Validated create options and the requested durability mode (`None` keeps
/// the handle's own mode).
pub(super) fn create_options(
    options: Options,
    in_memory: bool,
) -> Result<(CreateOptions, Option<vlite::Durability>)> {
    let format = match options.format {
        None => None,
        Some(3) => Some(FormatVersion::V3),
        Some(4) => Some(FormatVersion::V4),
        Some(5) => Some(FormatVersion::V5),
        _ => return Err(invalid("format must be 3, 4 or 5")),
    };
    let durability = durability(
        options.durability.as_deref(),
        options.flush_interval_ms,
        in_memory,
    )?;
    Ok((
        CreateOptions {
            format,
            encryption: options.credentials.key()?,
            commit_timestamps: options.commit_timestamps,
        },
        durability,
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Version {
    version: String,
    vector: Vec<f32>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Candidate {
    id: String,
    #[serde(default)]
    distance: f64,
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum GeoOrder {
    #[default]
    VectorDistance,
    GeoDistance,
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum RerankMethod {
    #[default]
    Exact,
    Mmr,
}
fn groups_json(groups: Vec<vlite::SearchGroup>) -> Value {
    json!(
        groups
            .into_iter()
            .map(|group| json!({"value":group.value,"hits":hits_json(group.hits)}))
            .collect::<Vec<_>>()
    )
}
fn facets_json(facets: Vec<vlite::FacetCount>) -> Value {
    json!(
        facets
            .into_iter()
            .map(|facet| json!({"value":facet.value,"count":facet.count}))
            .collect::<Vec<_>>()
    )
}
fn fusion_options(fusion: Option<Fusion>) -> FusionOptions {
    let mut options = FusionOptions::default();
    if let Some(fusion) = fusion {
        options.candidates = fusion.candidates;
        options.rrf_k = fusion.rrf_k.unwrap_or(options.rrf_k);
        options.vector_weight = fusion.vector_weight.unwrap_or(options.vector_weight);
        options.text_weight = fusion.text_weight.unwrap_or(options.text_weight);
    }
    options
}
pub(super) fn insert(state: &mut State, handle: u64, db: Database) -> Result<Value> {
    if handle != 0 {
        return Err(invalid("create/open requires handle 0"));
    }
    let next = state
        .next
        .checked_add(1)
        .ok_or_else(|| invalid("handle space exhausted"))?;
    state.next = next;
    state.databases.insert(next, db);
    Ok(json!({"handle":next.to_string()}))
}
pub(super) fn execute(state: &mut State, handle: u64, request: Request) -> Result<Value> {
    // Check before side effects, including creating files and performing offline restore.
    if matches!(
        request,
        Request::Create { .. }
            | Request::Open { .. }
            | Request::OpenFromBytes { .. }
            | Request::RestoreTo { .. }
    ) && handle != 0
    {
        return Err(invalid("create/open/restore requires handle 0"));
    }
    let request = match request {
        Request::Create {
            path,
            dimensions,
            options,
        } => {
            let (options, durability) = create_options(options, path.is_none())?;
            let db = match path {
                Some(path) => Database::create_with_options(path, dimensions, options),
                None => Database::create_in_memory(dimensions, options),
            }
            .and_then(|db| with_durability(db, durability))
            .map_err(failure)?;
            return insert(state, handle, db);
        }
        Request::Open {
            path,
            credentials,
            read_only,
            durability: mode,
            flush_interval_ms,
        } => {
            let durability = durability(mode.as_deref(), flush_interval_ms, false)?;
            let key = credentials.key()?;
            let db = match (key, read_only) {
                (Some(key), false) => Database::open_encrypted(path, &key),
                (Some(key), true) => Database::open_read_only_encrypted(path, &key),
                (None, false) => Database::open(path),
                (None, true) => Database::open_read_only(path),
            }
            .and_then(|db| with_durability(db, durability))
            .map_err(failure)?;
            return insert(state, handle, db);
        }
        Request::OpenFromBytes {
            bytes_base64,
            credentials,
        } => {
            let key = credentials.key()?;
            let db =
                Database::open_from_bytes(decode(&bytes_base64)?, key.as_ref()).map_err(failure)?;
            return insert(state, handle, db);
        }
        Request::RestoreTo {
            log_path,
            destination,
            base_backup,
            sequence,
            timestamp_ms,
            credentials,
        } => {
            let target = match (sequence, timestamp_ms) {
                (Some(value), None) => {
                    vlite::maintenance::RestoreTarget::Sequence(decimal(&value, "sequence")?)
                }
                (None, Some(value)) => {
                    vlite::maintenance::RestoreTarget::TimestampMs(decimal(&value, "timestamp_ms")?)
                }
                _ => return Err(invalid("provide exactly one of sequence or timestamp_ms")),
            };
            let key = credentials.key()?;
            let report = vlite::maintenance::restore_to(
                base_backup.as_deref().map(std::path::Path::new),
                log_path,
                destination,
                target,
                key.as_ref(),
            )
            .map_err(failure)?;
            return Ok(
                json!({"restored_sequence":report.restored_sequence.to_string(),"source_head":report.source_head.to_string(),"restored_at_ms":report.restored_at_ms.map(|v|v.to_string())}),
            );
        }
        other => other,
    };
    let db = state
        .databases
        .get_mut(&handle)
        .ok_or_else(|| ("invalid_handle", "Unknown or closed handle".into()))?;
    match request {
        Request::ToBytes {} => Ok(
            json!({"bytes_base64":BASE64_STANDARD.encode(db.to_bytes().ok_or_else(|| invalid("to_bytes requires an in-memory database"))?)}),
        ),
        Request::EncryptionInfo {} => Ok(
            json!({"is_encrypted":db.is_encrypted(),"key_id":db.encryption_key_id().map(|v|v.to_string())}),
        ),
        Request::PutWithExpiry {
            id: text,
            vector,
            payload_base64,
            metadata,
            expires_at_ms,
            collection,
        } => {
            let expiry = expires_at_ms
                .map(|text| decimal(&text, "expires_at_ms"))
                .transpose()?;
            let revision = db
                .collection_mut(collection_name(&collection))
                .map_err(failure)?
                .put_with_expiry(
                    id(&text)?,
                    &vector,
                    &payload(&payload_base64)?,
                    &metadata,
                    expiry,
                )
                .map_err(failure)?;
            Ok(json!({"revision":revision.to_string()}))
        }
        Request::UpdateMetadata {
            id: text,
            metadata,
            collection,
        } => {
            let revision = db
                .collection_mut(collection_name(&collection))
                .map_err(failure)?
                .update_metadata(id(&text)?, &metadata)
                .map_err(failure)?;
            Ok(json!({"revision":revision.to_string()}))
        }
        Request::SearchGrouped {
            vector,
            groups,
            hits_per_group,
            field,
            filter,
            metric,
            collection,
        } => {
            let view = db
                .collection(collection_name(&collection))
                .map_err(failure)?;
            if groups > 1000 || hits_per_group > 1000 {
                return Err(invalid("groups and hits_per_group must not exceed 1000"));
            }
            Ok(groups_json(
                view.search_grouped(
                    &vector,
                    groups,
                    hits_per_group,
                    &field,
                    &filter,
                    metric.unwrap_or(view.settings().metric),
                )
                .map_err(failure)?,
            ))
        }
        Request::Recommend {
            k,
            positive_ids,
            negative_ids,
            positive_vectors,
            negative_vectors,
            filter,
            metric,
            collection,
        } => {
            let view = db
                .collection(collection_name(&collection))
                .map_err(failure)?;
            let request = RecommendationQuery {
                positive_ids: positive_ids.iter().map(|v| id(v)).collect::<Result<_>>()?,
                negative_ids: negative_ids.iter().map(|v| id(v)).collect::<Result<_>>()?,
                positive_vectors,
                negative_vectors,
                k,
                conditions: filter,
                metric: metric.unwrap_or(view.settings().metric),
            };
            Ok(hits_json(view.recommend(&request).map_err(failure)?))
        }
        Request::Facets {
            field,
            filter,
            collection,
        } => Ok(facets_json(
            db.collection(collection_name(&collection))
                .map_err(failure)?
                .facets(&field, &filter)
                .map_err(failure)?,
        )),
        Request::NumericStats {
            field,
            filter,
            collection,
        } => {
            let stats = db
                .collection(collection_name(&collection))
                .map_err(failure)?
                .numeric_stats(&field, &filter)
                .map_err(failure)?;
            Ok(
                json!({"count":stats.count,"min":stats.min,"max":stats.max,"sum":stats.sum,"average":stats.average}),
            )
        }
        Request::SearchGeo {
            vector,
            k,
            field,
            latitude,
            longitude,
            radius_meters,
            filter,
            metric,
            sort,
            collection,
        } => {
            let view = db
                .collection(collection_name(&collection))
                .map_err(failure)?;
            let request = GeoSearchQuery {
                vector,
                k,
                field,
                latitude,
                longitude,
                radius_meters,
                conditions: filter,
                metric: metric.unwrap_or(view.settings().metric),
                sort: match sort {
                    GeoOrder::VectorDistance => GeoSort::VectorDistance,
                    GeoOrder::GeoDistance => GeoSort::GeoDistance,
                },
            };
            Ok(json!(view.search_geo(&request).map_err(failure)?.into_iter().map(|h|json!({"id":h.id.to_string(),"vector_distance":h.vector_distance,"geo_distance_meters":h.geo_distance_meters})).collect::<Vec<_>>()))
        }
        Request::ConfigureVector {
            name,
            dimensions,
            metric,
            active_version,
            history,
            collection,
        } => {
            db.collection_mut(collection_name(&collection))
                .map_err(failure)?
                .configure_vector(
                    &name,
                    VectorSpec {
                        dimensions,
                        metric,
                        active_version,
                        history,
                    },
                )
                .map_err(failure)?;
            Ok(Value::Null)
        }
        Request::VectorSpecs { collection } => {
            let view = db
                .collection(collection_name(&collection))
                .map_err(failure)?;
            Ok(Value::Object(view.vector_specs().map_err(failure)?.iter().map(|(name,s)| (name.clone(),json!({"dimensions":s.dimensions,"metric":s.metric,"active_version":s.active_version,"history":s.history}))).collect()))
        }
        Request::RecordVectors {
            id: text,
            collection,
        } => {
            let view = db
                .collection(collection_name(&collection))
                .map_err(failure)?;
            Ok(match view.record_vectors(id(&text)?).map_err(failure)? {
                None => Value::Null,
                Some(v) => {
                    let dense: BTreeMap<_, _> = v
                        .dense
                        .iter()
                        .map(|(name, versions)| {
                            (
                                name,
                                versions
                                    .iter()
                                    .map(|v| json!({"version":v.version,"vector":v.vector}))
                                    .collect::<Vec<_>>(),
                            )
                        })
                        .collect();
                    json!({"dense":dense,"sparse":v.sparse})
                }
            })
        }
        Request::PutVectors {
            id: text,
            dense,
            sparse,
            collection,
        } => {
            let dense = dense
                .into_iter()
                .map(|(name, versions)| {
                    (
                        name,
                        versions
                            .into_iter()
                            .map(|v| VersionedVector {
                                version: v.version,
                                vector: v.vector,
                            })
                            .collect(),
                    )
                })
                .collect();
            db.collection_mut(collection_name(&collection))
                .map_err(failure)?
                .put_vectors(id(&text)?, RecordVectors { dense, sparse })
                .map_err(failure)?;
            Ok(Value::Null)
        }
        Request::PutVectorVersion {
            id: text,
            name,
            version,
            vector,
            collection,
        } => {
            db.collection_mut(collection_name(&collection))
                .map_err(failure)?
                .put_vector_version(id(&text)?, &name, &version, vector)
                .map_err(failure)?;
            Ok(Value::Null)
        }
        Request::PruneVectorHistory {
            name,
            history,
            collection,
        } => {
            db.collection_mut(collection_name(&collection))
                .map_err(failure)?
                .prune_vector_history(&name, history)
                .map_err(failure)?;
            Ok(Value::Null)
        }
        Request::SearchNamed {
            name,
            version,
            vector,
            k,
            collection,
        } => Ok(hits_json(
            db.collection(collection_name(&collection))
                .map_err(failure)?
                .search_named(&name, version.as_deref(), &vector, k)
                .map_err(failure)?,
        )),
        Request::SearchSparse {
            name,
            vector,
            k,
            collection,
        } => Ok(hits_json(
            db.collection(collection_name(&collection))
                .map_err(failure)?
                .search_sparse(&name, &vector, k)
                .map_err(failure)?,
        )),
        Request::SearchDenseSparse {
            dense_name,
            version,
            dense,
            sparse_name,
            sparse,
            k,
            fusion,
            collection,
        } => Ok(hybrid_hits_json(
            db.collection(collection_name(&collection))
                .map_err(failure)?
                .search_dense_sparse(&DenseSparseQuery {
                    dense_name,
                    version,
                    dense,
                    sparse_name,
                    sparse,
                    k,
                    fusion: fusion_options(fusion),
                })
                .map_err(failure)?,
        )),
        Request::Rerank {
            vector,
            candidates,
            limit,
            method,
            metric,
            lambda,
            collection,
        } => {
            let view = db
                .collection(collection_name(&collection))
                .map_err(failure)?;
            let hits = candidates
                .into_iter()
                .map(|h| {
                    Ok(SearchHit {
                        id: id(&h.id)?,
                        distance: h.distance,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let provider: Box<dyn vlite::rerank::Reranker> = match method {
                RerankMethod::Exact => {
                    if lambda.is_some() {
                        return Err(invalid("lambda is only supported for MMR"));
                    }
                    Box::new(vlite::rerank::ExactReranker {
                        metric: metric.unwrap_or(view.settings().metric),
                    })
                }
                RerankMethod::Mmr => {
                    if metric.is_some() {
                        return Err(invalid("MMR always uses cosine; omit metric"));
                    }
                    Box::new(vlite::rerank::MmrReranker {
                        lambda: lambda.unwrap_or(0.5),
                    })
                }
            };
            Ok(hits_json(
                view.rerank(&vector, &hits, limit, provider.as_ref())
                    .map_err(failure)?,
            ))
        }
        Request::Query {
            query,
            parameters,
            collection,
        } => {
            let parsed = vlite::query_language::parse(&query)
                .map_err(|e| ("query_parse_error", e.to_string()))?;
            let view = db
                .collection(collection_name(&collection))
                .map_err(failure)?;
            let output = parsed.execute(view, &parameters).map_err(failure)?;
            Ok(
                json!({"hits":hits_json(output.hits),"groups":groups_json(output.groups),"facets":facets_json(output.facets)}),
            )
        }
        Request::FeedCursor {} => Ok(json!(db.feed_cursor().map_err(failure)?.to_token())),
        Request::ChangesAfter { cursor, max_events } => {
            let cursor = cursor
                .map(|s| FeedCursor::from_token(&s))
                .transpose()
                .map_err(failure)?
                .unwrap_or(FeedCursor::beginning());
            let batch = db.changes_after(&cursor, max_events).map_err(failure)?;
            let events = batch.changes.into_iter().map(|change| {
            let mut event = json!({"sequence":change.sequence.to_string(),"ordinal":change.ordinal,"collection":change.collection,"committed_at_ms":change.committed_at_ms.map(|v|v.to_string())});
            match change.kind {
                ChangeKind::Put { id, record, expires_at_ms } => { event["kind"]=json!("put"); event["id"]=json!(id.to_string()); event["record"]=record_json(&id.to_string(),&record,Some(change.sequence)); event["expires_at_ms"]=json!(expires_at_ms.map(|v|v.to_string())); },
                ChangeKind::Delete { id } => { event["kind"]=json!("delete"); event["id"]=json!(id.to_string()); },
                ChangeKind::MetadataUpdate { id, metadata } => { event["kind"]=json!("metadata_update"); event["id"]=json!(id.to_string()); event["metadata"]=json!(metadata); },
                ChangeKind::CollectionCreated { dimensions, settings } => { event["kind"]=json!("collection_created"); event["dimensions"]=json!(dimensions); event["settings"]=settings_json(&settings); },
                ChangeKind::CollectionDropped => event["kind"]=json!("collection_dropped"),
                ChangeKind::SettingsUpdated { settings } => { event["kind"]=json!("settings_updated"); event["settings"]=settings_json(&settings); },
                ChangeKind::VectorsUpdated => event["kind"]=json!("vectors_updated"),
            } event
        }).collect::<Vec<_>>();
            Ok(
                json!({"changes":events,"next":batch.next.to_token(),"has_more":batch.has_more,"head":batch.head.to_string()}),
            )
        }
        Request::BuildDiskAnn {
            path,
            metric,
            collection,
        } => {
            let view = db
                .collection(collection_name(&collection))
                .map_err(failure)?;
            let source = db
                .build_disk_ann(
                    collection_name(&collection),
                    path,
                    view.settings().hnsw,
                    metric.unwrap_or(view.settings().metric),
                )
                .map_err(failure)?;
            Ok(
                json!({"lineage":source.lineage.to_string(),"sequence":source.sequence.to_string(),"collection_id":source.collection_id,"live_records":source.live_records}),
            )
        }
        Request::SearchDiskAnn {
            path,
            vector,
            k,
            ef_search,
            cache_bytes,
        } => {
            let index = vlite::DiskAnnIndex::open(path, vlite::DiskAnnOptions { cache_bytes })
                .map_err(failure)?;
            Ok(hits_json(
                db.search_disk_ann(&index, &vector, k, ef_search)
                    .map_err(failure)?,
            ))
        }
        Request::SaveAnnSnapshot { path } => {
            let report = db.save_ann_snapshot(path).map_err(failure)?;
            Ok(json!({"indexes":report.indexes,"bytes":report.bytes}))
        }
        Request::LoadAnnSnapshot { path } => {
            let outcome = db.load_ann_snapshot(path).map_err(failure)?;
            let indexes = match outcome {
                vlite::AnnSnapshotLoad::Loaded { indexes } => indexes,
                _ => 0,
            };
            Ok(json!({"status":outcome.code(),"indexes":indexes}))
        }
        Request::Create { .. }
        | Request::Open { .. }
        | Request::OpenFromBytes { .. }
        | Request::RestoreTo { .. } => unreachable!(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn run(state: &mut State, handle: u64, value: Value) -> Result<Value> {
        execute(state, handle, serde_json::from_value(value).unwrap())
    }
    #[test]
    fn feature_dispatch_rejects_bad_fields_and_credentials() {
        assert!(
            serde_json::from_value::<Request>(json!({"op":"to_bytes","unknown":true})).is_err()
        );
        let mut state = State::default();
        assert_eq!(
            run(
                &mut state,
                1,
                json!({"op":"create","dimensions":2,"path":null})
            )
            .unwrap_err()
            .0,
            "invalid_input"
        );
        assert!(state.databases.is_empty());
        assert_eq!(run(&mut state, 0, json!({"op":"create","dimensions":2,"options":{"credentials":{"key_base64":"AA==","passphrase":"x"}}})).unwrap_err().0,"invalid_input");
        assert_eq!(
            run(
                &mut state,
                0,
                json!({"op":"create","dimensions":2,"options":{"format":2}})
            )
            .unwrap_err()
            .0,
            "invalid_input"
        );
    }
    #[test]
    fn feature_handles_share_legacy_reads_and_owned_close() {
        let result = super::super::execute(
            0,
            super::super::Request::Features {
                request: serde_json::from_value(
                    json!({"op":"create","dimensions":2,"options":{"format":5}}),
                )
                .unwrap(),
            },
        )
        .unwrap();
        let handle = result["handle"].as_str().unwrap().parse().unwrap();
        super::super::execute(
            handle,
            super::super::Request::Features {
                request: serde_json::from_value(
                    json!({"op":"put_with_expiry","id":"18446744073709551615","vector":[1,0]}),
                )
                .unwrap(),
            },
        )
        .unwrap();
        let record = super::super::execute(
            handle,
            super::super::Request::Get {
                id: "18446744073709551615".into(),
                collection: None,
            },
        )
        .unwrap();
        assert_eq!(record["vector"], json!([1.0, 0.0]));
        super::super::execute(handle, super::super::Request::Close {}).unwrap();
        assert_eq!(
            super::super::execute(
                handle,
                super::super::Request::Features {
                    request: serde_json::from_value(json!({"op":"to_bytes"})).unwrap()
                }
            )
            .unwrap_err()
            .0,
            "invalid_handle"
        );
    }
    #[test]
    fn ann_snapshot_round_trips_through_reopen_and_rejects_stale_state() {
        let call = |handle: u64, value: Value| {
            super::super::execute(handle, serde_json::from_value(value).unwrap())
        };
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("snap.vlite");
        let sidecar = directory.path().join("snap.vlite.ann");
        let handle = |result: Value| result["handle"].as_str().unwrap().parse::<u64>().unwrap();
        let db = handle(call(0, json!({"op":"create","path":path,"dimensions":2})).unwrap());
        let rows: Vec<Value> = (0..20)
            .map(|id| json!({"op":"put","id":id.to_string(),"vector":[id as f32, 1.0]}))
            .collect();
        call(db, json!({"op":"batch","operations":rows})).unwrap();
        call(db, json!({"op":"build_index"})).unwrap();
        let feature = |request: Value| json!({"op":"features","request":request});
        let saved = call(
            db,
            feature(json!({"op":"save_ann_snapshot","path":sidecar})),
        )
        .unwrap();
        assert_eq!(saved["indexes"], 1);
        assert!(saved["bytes"].as_u64().unwrap() > 0);
        call(db, json!({"op":"close"})).unwrap();

        let db = handle(call(0, json!({"op":"open","path":path})).unwrap());
        let missing = directory.path().join("absent.ann");
        assert_eq!(
            call(
                db,
                feature(json!({"op":"load_ann_snapshot","path":missing}))
            )
            .unwrap(),
            json!({"status":"missing","indexes":0})
        );
        assert_eq!(
            call(
                db,
                feature(json!({"op":"load_ann_snapshot","path":sidecar}))
            )
            .unwrap(),
            json!({"status":"loaded","indexes":1})
        );
        let hits = call(
            db,
            json!({"op":"search_approximate_ready","vector":[3.0,1.0],"k":1,"ef_search":10}),
        )
        .unwrap();
        assert_eq!(hits["hits"][0]["id"], "3");
        call(
            db,
            json!({"op":"batch","operations":[{"op":"put","id":"99","vector":[0.5,0.5]}]}),
        )
        .unwrap();
        let stale = call(
            db,
            feature(json!({"op":"load_ann_snapshot","path":sidecar})),
        )
        .unwrap();
        assert_ne!(stale["status"], "loaded");
        assert_eq!(stale["indexes"], 0);
        call(db, json!({"op":"close"})).unwrap();
    }
}
