//! Named collections and resumable imports over REST.
use super::*;
use crate::{
    CollectionInfo, CollectionMut, CollectionSettings, CollectionSpec, ImportChunkOutcome,
    ImportState, ImportStatus, InvalidRowPolicy, MetadataIndexKind, MetadataIndexSpec,
    SettingsPersistence,
};

/// Bound on reserved plus open import jobs.
const MAX_ACTIVE_IMPORTS: usize = 64;

pub(super) fn routes() -> Router<Service> {
    Router::new()
        .route("/v1/collections", get(list).post(create))
        .route("/v1/collections/{collection}", get(fetch).delete(drop))
        .route("/v1/collections/{collection}/settings", put(settings))
        .route("/v1/collections/{collection}/imports", post(reserve))
        .route("/v1/imports/{job}", get(status))
        .route("/v1/imports/{job}/chunks/{chunk}", put(chunk))
        .route("/v1/imports/{job}/finish", post(finish))
        .route("/v1/imports/{job}/abort", post(abort))
}

#[derive(Clone, Copy)]
pub(super) struct QuantizationName(pub Quantization);

impl<'de> Deserialize<'de> for QuantizationName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match String::deserialize(deserializer)?.as_str() {
            "none" => Ok(Self(Quantization::None)),
            "scalar_u8" => Ok(Self(Quantization::ScalarU8)),
            _ => Err(serde::de::Error::custom(
                "quantization must be none or scalar_u8",
            )),
        }
    }
}

pub(super) fn quantization_name(quantization: Quantization) -> &'static str {
    match quantization {
        Quantization::ScalarU8 => "scalar_u8",
        _ => "none",
    }
}

fn kind_name(kind: MetadataIndexKind) -> &'static str {
    match kind {
        MetadataIndexKind::Eq => "eq",
        MetadataIndexKind::Range => "range",
        _ => "eq_range",
    }
}

#[derive(Clone, Copy)]
struct KindName(MetadataIndexKind);

impl<'de> Deserialize<'de> for KindName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match String::deserialize(deserializer)?.as_str() {
            "eq" => Ok(Self(MetadataIndexKind::Eq)),
            "range" => Ok(Self(MetadataIndexKind::Range)),
            "eq_range" => Ok(Self(MetadataIndexKind::EqRange)),
            _ => Err(serde::de::Error::custom(
                "kind must be eq, range or eq_range",
            )),
        }
    }
}

pub(super) fn settings_json(settings: &CollectionSettings) -> Value {
    json!({
        "metric": settings.metric,
        "hnsw": {"m": settings.hnsw.m, "ef_construction": settings.hnsw.ef_construction,
            "seed": settings.hnsw.seed.to_string()},
        "quantization": quantization_name(settings.quantization),
        "rerank_multiplier": settings.rerank_multiplier,
        "planner": {"exact_max_candidates": settings.planner.exact_max_candidates,
            "exact_max_selectivity_ppm": settings.planner.exact_max_selectivity_ppm},
        "metadata_indexes": settings.metadata_indexes.iter()
            .map(|spec| json!({"field": spec.field, "kind": kind_name(spec.kind)}))
            .collect::<Vec<_>>(),
        "text_fields": settings.text_fields,
    })
}

pub(super) fn info_json(info: &CollectionInfo) -> Value {
    json!({"id": info.id, "name": info.name, "dimensions": info.dimensions,
        "live_records": info.live_records, "settings": settings_json(&info.settings),
        "settings_persisted": info.settings_persisted})
}

#[derive(Deserialize)]
struct Seed(#[serde(deserialize_with = "admin::seed")] u64);

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct HnswInput {
    m: Option<usize>,
    ef_construction: Option<usize>,
    seed: Option<Seed>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlannerInput {
    exact_max_candidates: Option<u32>,
    exact_max_selectivity_ppm: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexSpecInput {
    field: String,
    kind: KindName,
}

/// Partial settings: omitted fields keep their current (or default) values.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsInput {
    metric: Option<DistanceMetric>,
    hnsw: Option<HnswInput>,
    quantization: Option<QuantizationName>,
    rerank_multiplier: Option<u8>,
    planner: Option<PlannerInput>,
    metadata_indexes: Option<Vec<IndexSpecInput>>,
    text_fields: Option<Vec<String>>,
}

impl SettingsInput {
    fn merge(self, mut settings: CollectionSettings) -> CollectionSettings {
        if let Some(metric) = self.metric {
            settings.metric = metric;
        }
        if let Some(hnsw) = self.hnsw {
            settings.hnsw.m = hnsw.m.unwrap_or(settings.hnsw.m);
            settings.hnsw.ef_construction = hnsw
                .ef_construction
                .unwrap_or(settings.hnsw.ef_construction);
            settings.hnsw.seed = hnsw.seed.map_or(settings.hnsw.seed, |seed| seed.0);
        }
        if let Some(quantization) = self.quantization {
            settings.quantization = quantization.0;
        }
        if let Some(rerank) = self.rerank_multiplier {
            settings.rerank_multiplier = rerank;
        }
        if let Some(planner) = self.planner {
            settings.planner.exact_max_candidates = planner
                .exact_max_candidates
                .unwrap_or(settings.planner.exact_max_candidates);
            settings.planner.exact_max_selectivity_ppm = planner
                .exact_max_selectivity_ppm
                .unwrap_or(settings.planner.exact_max_selectivity_ppm);
        }
        if let Some(indexes) = self.metadata_indexes {
            settings.metadata_indexes = indexes
                .into_iter()
                .map(|spec| MetadataIndexSpec {
                    field: spec.field,
                    kind: spec.kind.0,
                })
                .collect();
        }
        if let Some(fields) = self.text_fields {
            settings.text_fields = fields;
        }
        settings
    }
}

/// Non-blocking persistent audit for collection lifecycle events; a failure
/// is counted (`audit_failures_total`) and never undoes the committed change.
fn audit_event(service: &Service, event: Value) {
    if let Some(log) = &service.admin.audit_log
        && log.append(event, false).is_err()
    {
        service
            .metrics
            .audit_failures
            .fetch_add(1, Ordering::Relaxed);
    }
}

fn principal(role: &admin::Role) -> Value {
    match role {
        admin::Role::Scoped(grant) => json!(grant.id),
        other => json!(other.name()),
    }
}

async fn list(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
) -> Result<Json<Value>, ApiError> {
    service
        .run(move |db| {
            let lineage = db.lineage();
            let items = db
                .collections()?
                .iter()
                .filter(|info| match &role {
                    admin::Role::Scoped(grant) => grant.covers(lineage, info.id, &info.name),
                    _ => true,
                })
                .map(info_json)
                .collect::<Vec<_>>();
            Ok(Json(
                json!({"items": items, "format_version": db.format_version()}),
            ))
        })
        .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    name: String,
    dimensions: usize,
    #[serde(default)]
    settings: SettingsInput,
}

async fn create(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    input: Result<Json<Create>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let input = body(input)?;
    let actor = principal(&role);
    let info = service
        .run(move |db| {
            Ok(db.create_collection(
                &input.name,
                CollectionSpec {
                    dimensions: input.dimensions,
                    settings: input.settings.merge(CollectionSettings::default()),
                },
            )?)
        })
        .await?;
    audit_event(
        &service,
        json!({"event":"collection_created","collection":info.name,"collection_id":info.id,"actor":actor}),
    );
    Ok((StatusCode::CREATED, Json(info_json(&info))))
}

async fn fetch(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    target: Result<Path<String>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let target = collection_path(target)?;
    service
        .run(move |db| {
            let collection = open(db, &role, &target)?;
            let info = db.info(collection);
            let text = CollectionRef {
                db: &*db,
                id: collection,
            }
            .text_index_status();
            let mut value = info_json(&info);
            value["text_index"] = json!({"declared_fields": text.declared_fields,
                "available": text.available, "documents": text.documents,
                "terms": text.terms, "postings": text.postings});
            Ok(Json(value))
        })
        .await
}

async fn drop(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    target: Result<Path<String>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let target = collection_path(target)?;
    let actor = principal(&role);
    let (name, id) = service
        .run(move |db| {
            let collection = open(db, &role, &target)?;
            db.drop_collection(target.name())?;
            Ok((target.name().to_owned(), collection))
        })
        .await?;
    audit_event(
        &service,
        json!({"event":"collection_dropped","collection":name,"collection_id":id,"actor":actor}),
    );
    Ok(Json(json!({"deleted": true})))
}

async fn settings(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    target: Result<Path<String>, PathRejection>,
    input: Result<Json<SettingsInput>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let target = collection_path(target)?;
    let input = body(input)?;
    let actor = principal(&role);
    let (info, persisted) = service
        .run(move |db| {
            let collection = open(db, &role, &target)?;
            let merged = input.merge(db.state(collection).settings.clone());
            let persistence = db.set_collection_settings(target.name(), merged)?;
            Ok((
                db.info(collection),
                persistence == SettingsPersistence::Persisted,
            ))
        })
        .await?;
    audit_event(
        &service,
        json!({"event":"collection_settings_changed","collection":info.name,
        "collection_id":info.id,"persisted":persisted,"actor":actor}),
    );
    Ok(Json(
        json!({"collection": info_json(&info), "persisted": persisted}),
    ))
}

/// In-memory job reservations: a job becomes durable with its first chunk.
#[derive(Default)]
pub(super) struct Reservations {
    jobs: BTreeMap<u64, (u16, String)>,
}

fn status_json(status: &ImportStatus, durable: bool) -> Value {
    let digest = status
        .last_digest
        .iter()
        .any(|&byte| byte != 0)
        .then(|| hex(&status.last_digest));
    json!({"job": status.job.to_string(), "collection": status.collection,
        "collection_dropped": status.collection_dropped,
        "state": match status.state {
            ImportState::Open => "open",
            ImportState::Finished => "finished",
            _ => "aborted",
        },
        "last_chunk": status.last_chunk, "rows_committed": status.rows_committed,
        "rows_skipped": status.rows_skipped, "last_digest": digest, "durable": durable})
}

fn reserved_json(job: u64, name: &str, state: &str) -> Value {
    json!({"job": job.to_string(), "collection": name, "collection_dropped": false,
        "state": state, "last_chunk": 0, "rows_committed": 0, "rows_skipped": 0,
        "last_digest": null, "durable": false})
}

fn conflict(message: &'static str) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "import_conflict", message)
}

fn job_path(input: Result<Path<String>, PathRejection>) -> Result<u64, ApiError> {
    let job = path(input)?;
    if job == 0 {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "import_not_found",
            "import job not found",
        ));
    }
    Ok(job)
}

async fn reserve(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    target: Result<Path<String>, PathRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let target = collection_path(target)?;
    let reservations = service.imports.clone();
    service
        .run(move |db| {
            open(db, &role, &target)?;
            let next = db.next_import_job_id()?;
            let collection = db.resolve(target.name())?;
            let mut reserved = reservations.lock().map_err(internal)?;
            if db.imports.open_jobs().count() + reserved.jobs.len() >= MAX_ACTIVE_IMPORTS {
                return Err(Error::TooManyImports.into());
            }
            let job = reserved
                .jobs
                .keys()
                .next_back()
                .map_or(Some(next), |&last| last.checked_add(1).map(|n| n.max(next)))
                .ok_or(Error::ImportConflict("import job IDs are exhausted"))?;
            reserved
                .jobs
                .insert(job, (collection, target.name().to_owned()));
            Ok((
                StatusCode::CREATED,
                Json(reserved_json(job, target.name(), "open")),
            ))
        })
        .await
}

enum Job {
    /// Durable job bound to a live collection.
    Durable(u16),
    /// Reserved job whose collection is still the same live collection.
    Reserved(u16),
}

/// Locates a job and checks scope. Dropped collections never resolve: a
/// recreated collection with the same name has a different ID.
fn locate(
    db: &Database,
    reservations: &Reservations,
    role: &admin::Role,
    job: u64,
) -> Result<Job, ApiError> {
    let lineage = db.lineage();
    let allowed = |collection: u16, name: &str| match role {
        admin::Role::Scoped(grant) => grant.covers(lineage, collection, name),
        _ => true,
    };
    if db.format_version() < 4 {
        return Err(Error::ImportsRequireUpgrade.into());
    }
    if let Some(state) = db.imports.get(job) {
        if !allowed(state.collection, db.imports.collection_name(job)) {
            return Err(forbidden_scope());
        }
        if state.collection_dropped {
            return Err(conflict(
                "the job's collection was dropped; it cannot act on a recreated collection",
            ));
        }
        return Ok(Job::Durable(state.collection));
    }
    match reservations.jobs.get(&job) {
        Some((collection, name)) => {
            if !allowed(*collection, name) {
                return Err(forbidden_scope());
            }
            if db
                .collections
                .get(collection)
                .is_none_or(|state| &state.name != name)
            {
                return Err(conflict(
                    "the reserved collection was dropped; create a new import",
                ));
            }
            Ok(Job::Reserved(*collection))
        }
        None => Err(Error::ImportNotFound.into()),
    }
}

/// Lowest reserved (not yet durable) job below `job` whose collection is
/// still live. Reservations of dropped collections can never commit and
/// therefore never block.
fn earliest_pending(db: &Database, reservations: &Reservations, job: u64) -> Option<u64> {
    reservations
        .jobs
        .range(..job)
        .find(|(_, (collection, name))| {
            db.collections
                .get(collection)
                .is_some_and(|state| &state.name == name)
        })
        .map(|(&id, _)| id)
}

async fn status(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    job: Result<Path<String>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let job = job_path(job)?;
    let reservations = service.imports.clone();
    service
        .run(move |db| {
            let reserved = reservations.lock().map_err(internal)?;
            match locate(db, &reserved, &role, job) {
                Ok(Job::Durable(_)) => Ok(Json(status_json(&db.import_status(job)?, true))),
                // Dropped durable jobs remain observable (historical name).
                Err(error) if error.code == "import_conflict" && db.imports.get(job).is_some() => {
                    Ok(Json(status_json(&db.import_status(job)?, true)))
                }
                Ok(Job::Reserved(_)) => {
                    let (_, name) = &reserved.jobs[&job];
                    Ok(Json(reserved_json(job, name, "open")))
                }
                Err(error) if error.code == "import_conflict" => {
                    let (_, name) = &reserved.jobs[&job];
                    let mut value = reserved_json(job, name, "open");
                    value["collection_dropped"] = json!(true);
                    Ok(Json(value))
                }
                Err(error) => Err(error),
            }
        })
        .await
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Policy {
    #[default]
    RejectChunk,
    SkipInvalid,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Chunk {
    operations: Vec<Operation>,
    #[serde(default)]
    policy: Policy,
}

async fn chunk(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    params: Result<Path<(String, String)>, PathRejection>,
    input: Result<Json<Chunk>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Path((job, number)) = params.map_err(|error| {
        ApiError::new(StatusCode::BAD_REQUEST, "invalid_path", error.body_text())
    })?;
    let job = job_path(Ok(Path(job)))?;
    let number = id(&number)?;
    let input = body(input)?;
    let policy = match input.policy {
        Policy::RejectChunk => InvalidRowPolicy::RejectChunk,
        Policy::SkipInvalid => InvalidRowPolicy::SkipInvalid,
    };
    let operations = mutations(input.operations, false)?;
    let reservations = service.imports.clone();
    let metrics = service.metrics.clone();
    service
        .run(move |db| {
            let mut reserved = reservations.lock().map_err(internal)?;
            let collection = match locate(db, &reserved, &role, job)? {
                Job::Durable(collection) => collection,
                Job::Reserved(collection) => {
                    // The engine's durable job high-water mark means a later
                    // job committing first would strand every earlier
                    // reservation. Commit reservations in ID order instead:
                    // refuse explicitly (nothing written) until earlier live
                    // reservations commit their first chunk or are aborted.
                    if let Some(earlier) = earliest_pending(db, &reserved, job) {
                        return Err(ApiError::new(
                            StatusCode::CONFLICT,
                            "import_conflict",
                            format!(
                                "import job {earlier} was reserved earlier and has not committed \
                                 a chunk; retry after it commits its first chunk or is aborted"
                            ),
                        ));
                    }
                    collection
                }
            };
            let report = CollectionMut { db: &mut *db, id: collection }
                .import_chunk(job, number, &operations, policy)?;
            if db.imports.get(job).is_some() {
                reserved.jobs.remove(&job);
            }
            metrics.import_chunks.fetch_add(1, Ordering::Relaxed);
            metrics
                .import_rows_skipped
                .fetch_add(report.rows_skipped as u64, Ordering::Relaxed);
            let mut value = json!({
                "outcome": match report.outcome {
                    ImportChunkOutcome::Committed => "committed",
                    ImportChunkOutcome::AlreadyCommitted => "already_committed",
                    _ => "rejected",
                },
                "job": report.job.to_string(), "chunk": report.chunk,
                "rows_committed": report.rows_committed, "rows_skipped": report.rows_skipped,
                "invalid_rows": report.invalid_rows,
                "invalid": report.invalid.iter()
                    .map(|row| json!({"row": row.row, "code": row.code, "message": row.message}))
                    .collect::<Vec<_>>(),
                "revision": report.revision.map(|revision| revision.to_string()),
            });
            if matches!(
                report.outcome,
                ImportChunkOutcome::Committed | ImportChunkOutcome::AlreadyCommitted
            ) {
                value["status"] = status_json(&db.import_status(job)?, true);
                Ok(Json(value).into_response())
            } else {
                value["error"] = json!({"code": "import_rows_invalid",
                    "message": "chunk rejected; nothing was written; fix the rows or use policy skip_invalid"});
                let mut response = (StatusCode::UNPROCESSABLE_ENTITY, Json(value)).into_response();
                response
                    .extensions_mut()
                    .insert(ErrorCode("import_rows_invalid"));
                Ok(response)
            }
        })
        .await
}

async fn finish(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    job: Result<Path<String>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    close(service, role, job, false).await
}

async fn abort(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    job: Result<Path<String>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    close(service, role, job, true).await
}

async fn close(
    service: Service,
    role: admin::Role,
    job: Result<Path<String>, PathRejection>,
    abort: bool,
) -> Result<Json<Value>, ApiError> {
    let job = job_path(job)?;
    let reservations = service.imports.clone();
    service
        .run(move |db| {
            let mut reserved = reservations.lock().map_err(internal)?;
            let located = locate(db, &reserved, &role, job);
            match located {
                Ok(Job::Durable(collection)) => {
                    let mut handle = CollectionMut {
                        db: &mut *db,
                        id: collection,
                    };
                    let status = if abort {
                        handle.abort_import(job)?
                    } else {
                        handle.finish_import(job)?
                    };
                    Ok(Json(status_json(&status, true)))
                }
                Ok(Job::Reserved(_)) if !abort => Err(conflict(
                    "no chunk has been committed; abort the reservation instead",
                )),
                Ok(Job::Reserved(_)) => {
                    let (_, name) = reserved.jobs.remove(&job).expect("located reservation");
                    Ok(Json(reserved_json(job, &name, "aborted")))
                }
                // A reservation for a dropped collection can only be released.
                Err(error)
                    if abort
                        && error.code == "import_conflict"
                        && db.imports.get(job).is_none() =>
                {
                    let (_, name) = reserved.jobs.remove(&job).expect("located reservation");
                    let mut value = reserved_json(job, &name, "aborted");
                    value["collection_dropped"] = json!(true);
                    Ok(Json(value))
                }
                Err(error) => Err(error),
            }
        })
        .await
}
