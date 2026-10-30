//! C ABI: owned UTF-8 JSON responses, opaque numeric handles, no network runtime.

mod features;
mod graph;
mod typed;

use base64::{Engine, prelude::BASE64_STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    ffi::{CString, c_char},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Mutex, OnceLock},
};
use vlite::{
    AnnOptions, CollectionInfo, CollectionSettings, CollectionSpec, Database, DistanceMetric,
    Expected, FormatVersion, ImportChunkOutcome, ImportChunkReport, ImportState, InvalidRowPolicy,
    Metadata, MetadataIndexKind, MetadataIndexSpec, Mutation, Quantization, QueryPlan, Record,
    SearchHit, SearchStats, SettingsPersistence,
    hybrid::{FusionOptions, HybridHit, HybridQuery, VectorLeg},
    quality::QualitySampleOptions,
};

/// `ef_search` used by `auto`, hybrid ready legs and quality sampling when
/// omitted; the same default the Node and Python wrappers apply.
const DEFAULT_EF_SEARCH: usize = 100;

#[derive(Default)]
struct State {
    next: u64,
    databases: BTreeMap<u64, Database>,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Features {
        request: features::Request,
    },
    /// Property-graph operations (`graph.rs`).
    Graph {
        request: Value,
    },
    /// Optional capabilities compiled into this library.
    Capabilities {},
    Create {
        path: String,
        dimensions: usize,
        /// 3 (default) or 4. Only v4 supports named collections and imports.
        #[serde(default)]
        format: Option<u32>,
        /// `"periodic"` (default, every 60 s) or `"full"`; see `durability`.
        #[serde(default)]
        durability: Option<String>,
        /// `Durability::Periodic` flush interval (1..=60000 ms).
        #[serde(default)]
        flush_interval_ms: Option<u64>,
    },
    Open {
        path: String,
        /// Shared-lock read-only handle (`Database::open_read_only`).
        #[serde(default)]
        read_only: bool,
        /// `"periodic"` (default, every 60 s) or `"full"`; see `durability`.
        #[serde(default)]
        durability: Option<String>,
        /// `Durability::Periodic` flush interval (1..=60000 ms).
        #[serde(default)]
        flush_interval_ms: Option<u64>,
    },
    Close {},
    /// Forces acknowledged commits to stable storage (`Database::sync`).
    Sync {},
    Info {},
    Get {
        id: String,
        #[serde(default)]
        collection: Option<String>,
    },
    GetMany {
        ids: Vec<String>,
        #[serde(default)]
        collection: Option<String>,
    },
    Put {
        id: String,
        vector: Vec<f32>,
        #[serde(default)]
        payload_base64: String,
        #[serde(default)]
        metadata: Metadata,
        #[serde(default)]
        collection: Option<String>,
        #[serde(default)]
        expected_revision: Option<String>,
    },
    Delete {
        id: String,
        #[serde(default)]
        collection: Option<String>,
        #[serde(default)]
        expected_revision: Option<String>,
    },
    Batch {
        operations: Vec<Operation>,
        #[serde(default)]
        collection: Option<String>,
    },
    Revision {
        id: String,
        #[serde(default)]
        collection: Option<String>,
    },
    Search {
        #[serde(default)]
        vector: Option<Vec<f32>>,
        k: usize,
        #[serde(default)]
        filter: Vec<vlite::Condition>,
        #[serde(default)]
        metric: Option<DistanceMetric>,
        #[serde(default)]
        collection: Option<String>,
        #[serde(default)]
        mode: SearchMode,
        #[serde(default)]
        explain: bool,
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        fusion: Option<Fusion>,
        #[serde(default)]
        vector_leg: Option<LegKind>,
        #[serde(default)]
        ef_search: Option<usize>,
        #[serde(default)]
        quantization: Option<QuantizationName>,
        #[serde(default)]
        rerank_multiplier: Option<u8>,
    },
    BuildIndex {
        #[serde(default)]
        metric: Option<DistanceMetric>,
        #[serde(default)]
        collection: Option<String>,
        #[serde(default)]
        quantization: Option<QuantizationName>,
        #[serde(default)]
        rerank_multiplier: Option<u8>,
    },
    SearchApproximateReady {
        #[serde(default)]
        vector: Option<Vec<f32>>,
        k: usize,
        ef_search: usize,
        #[serde(default)]
        metric: Option<DistanceMetric>,
        #[serde(default)]
        collection: Option<String>,
        #[serde(default)]
        quantization: Option<QuantizationName>,
        #[serde(default)]
        rerank_multiplier: Option<u8>,
    },
    ListCollections {},
    CreateCollection {
        name: String,
        dimensions: usize,
        #[serde(default)]
        settings: Option<Settings>,
    },
    DropCollection {
        name: String,
    },
    SetCollectionSettings {
        name: String,
        settings: Settings,
    },
    NextImportJobId {},
    ImportChunk {
        job: String,
        chunk: String,
        operations: Vec<Operation>,
        #[serde(default)]
        collection: Option<String>,
        #[serde(default)]
        policy: Policy,
    },
    ImportStatus {
        job: String,
    },
    ImportFinish {
        job: String,
        #[serde(default)]
        collection: Option<String>,
    },
    ImportAbort {
        job: String,
        #[serde(default)]
        collection: Option<String>,
    },
    AnnQuality {
        #[serde(default)]
        collection: Option<String>,
        #[serde(default)]
        metric: Option<DistanceMetric>,
        #[serde(default)]
        quantization: Option<QuantizationName>,
        #[serde(default)]
        rerank_multiplier: Option<u8>,
        #[serde(default)]
        samples: Option<usize>,
        #[serde(default)]
        k: Option<usize>,
        #[serde(default)]
        ef_search: Option<usize>,
        #[serde(default)]
        seed: Option<String>,
        #[serde(default)]
        max_distance_evaluations: Option<u64>,
    },
    AnnMemory {
        #[serde(default)]
        collection: Option<String>,
        #[serde(default)]
        metric: Option<DistanceMetric>,
        #[serde(default)]
        quantization: Option<QuantizationName>,
        #[serde(default)]
        rerank_multiplier: Option<u8>,
    },
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    Put {
        id: String,
        vector: Vec<f32>,
        #[serde(default)]
        payload_base64: String,
        #[serde(default)]
        metadata: Metadata,
    },
    Delete {
        id: String,
    },
}

#[derive(Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum SearchMode {
    #[default]
    Exact,
    Auto,
    Hybrid,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LegKind {
    Exact,
    ApproximateReady,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum QuantizationName {
    None,
    ScalarU8,
}

impl From<QuantizationName> for Quantization {
    fn from(value: QuantizationName) -> Self {
        match value {
            QuantizationName::None => Self::None,
            QuantizationName::ScalarU8 => Self::ScalarU8,
        }
    }
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
struct Fusion {
    #[serde(default)]
    candidates: Option<usize>,
    #[serde(default)]
    rrf_k: Option<u32>,
    #[serde(default)]
    vector_weight: Option<f64>,
    #[serde(default)]
    text_weight: Option<f64>,
}

/// Complete collection settings. Omitted fields take engine defaults, so
/// `set_collection_settings` is a full replacement, never a merge.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    #[serde(default)]
    metric: Option<DistanceMetric>,
    #[serde(default)]
    hnsw: Option<Hnsw>,
    #[serde(default)]
    quantization: Option<QuantizationName>,
    #[serde(default)]
    rerank_multiplier: Option<u8>,
    #[serde(default)]
    planner: Option<Planner>,
    #[serde(default)]
    metadata_indexes: Vec<IndexDeclaration>,
    #[serde(default)]
    text_fields: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Hnsw {
    #[serde(default)]
    m: Option<usize>,
    #[serde(default)]
    ef_construction: Option<usize>,
    #[serde(default)]
    seed: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Planner {
    #[serde(default)]
    exact_max_candidates: Option<u32>,
    #[serde(default)]
    exact_max_selectivity_ppm: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexDeclaration {
    field: String,
    kind: IndexKind,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum IndexKind {
    Eq,
    Range,
    EqRange,
}

type Failure = (&'static str, String);
type Result<T> = std::result::Result<T, Failure>;

fn failure(error: vlite::Error) -> Failure {
    let code = match &error {
        vlite::Error::CommitIndeterminate(_) => "commit_indeterminate",
        vlite::Error::Poisoned => "poisoned",
        vlite::Error::Locked => "locked",
        vlite::Error::ReadOnly => "read_only",
        vlite::Error::Corrupt { .. } => "corrupt",
        vlite::Error::IncompleteTail { .. } => "incomplete_tail",
        vlite::Error::IndexNotReady => "index_not_ready",
        vlite::Error::Io(_) => "io_error",
        vlite::Error::UnsupportedVersion(_)
        | vlite::Error::MetadataRequiresUpgrade
        | vlite::Error::BatchRequiresUpgrade => "format_error",
        vlite::Error::SequenceExhausted => "sequence_exhausted",
        vlite::Error::CollectionNotFound => "collection_not_found",
        vlite::Error::ImportNotFound => "import_not_found",
        vlite::Error::CollectionExists => "collection_exists",
        vlite::Error::TooManyCollections => "collection_limit",
        vlite::Error::CollectionNotEmpty => "collection_not_empty",
        vlite::Error::DefaultCollectionImmutable => "default_collection_immutable",
        vlite::Error::CollectionsRequireUpgrade | vlite::Error::ImportsRequireUpgrade => {
            "requires_format_v4"
        }
        vlite::Error::ImportConflict(_) => "import_conflict",
        vlite::Error::TooManyImports => "import_limit",
        vlite::Error::RevisionMismatch { .. } => "precondition_failed",
        vlite::Error::EncryptionKeyRequired => "encryption_key_required",
        vlite::Error::EncryptionKeyMismatch { .. } => "encryption_key_mismatch",
        vlite::Error::NotEncrypted => "not_encrypted",
        vlite::Error::InvalidEncryptionOptions(_) => "invalid_encryption_options",
        vlite::Error::InvalidDurability(_) => "invalid_durability",
        vlite::Error::ExpiryRequiresUpgrade => "requires_format_v5",
        vlite::Error::ChangeFeedRequiresUpgrade => "change_feed_requires_upgrade",
        vlite::Error::CursorCompacted { .. } => "cursor_compacted",
        vlite::Error::CursorAhead { .. } => "cursor_ahead",
        vlite::Error::CursorBeforeHistory { .. } => "cursor_before_history",
        vlite::Error::InvalidCursor(_) => "invalid_cursor",
        vlite::Error::TimestampsUnavailable => "timestamps_unavailable",
        vlite::Error::InvalidRestoreTarget(_) => "invalid_restore_target",
        vlite::Error::StaleIndex => "stale_index",
        vlite::Error::SidecarUnavailable(_) => "sidecar_unavailable",
        vlite::Error::PublicationIndeterminate { .. } => "publication_indeterminate",
        _ => "invalid_input",
    };
    (code, error.to_string())
}

/// `Durability` for the optional `durability` (`"periodic"`, `"full"` or
/// `"none"`) and `flush_interval_ms` request fields. File handles default to
/// `Durability::default()` (periodic, 60 s); in-memory handles are always
/// `none`. `None` means "keep the handle's own mode".
fn durability(
    mode: Option<&str>,
    flush_interval_ms: Option<u64>,
    in_memory: bool,
) -> Result<Option<vlite::Durability>> {
    let rejected = |message: &str| ("invalid_durability", message.to_owned());
    match (mode, flush_interval_ms, in_memory) {
        (None | Some("none"), None, true) => Ok(None),
        (_, _, true) => Err(rejected(
            "in-memory databases are never persisted; only durability \"none\" applies",
        )),
        (Some("none"), _, false) => Err(rejected(
            "durability \"none\" is only available for in-memory databases",
        )),
        (Some("full"), Some(_), false) => Err(rejected(
            "flush_interval_ms requires durability \"periodic\"",
        )),
        (Some("full"), None, false) => Ok(Some(vlite::Durability::Full)),
        (None | Some("periodic"), Some(ms), false) => vlite::Durability::periodic_ms(ms)
            .map(Some)
            .map_err(failure),
        (None | Some("periodic"), None, false) => Ok(Some(vlite::Durability::default())),
        (Some(_), _, false) => Err(rejected(
            "durability must be \"periodic\", \"full\" or \"none\"",
        )),
    }
}

/// Applies `durability` unless the handle keeps its own mode.
fn with_durability(db: Database, durability: Option<vlite::Durability>) -> vlite::Result<Database> {
    match durability {
        Some(durability) => db.with_durability(durability),
        None => Ok(db),
    }
}

fn invalid(message: &str) -> Failure {
    ("invalid_input", message.to_owned())
}

fn id(text: &str) -> Result<u64> {
    let value: u64 = text.parse().map_err(|_| invalid("invalid u64 ID"))?;
    if value.to_string() != text {
        return Err(invalid("ID must be a canonical decimal u64 string"));
    }
    Ok(value)
}

/// Parses a canonical decimal u64 string field such as a job, chunk or seed.
fn decimal(text: &str, field: &str) -> Result<u64> {
    match text.parse::<u64>() {
        Ok(value) if value.to_string() == text => Ok(value),
        _ => Err((
            "invalid_input",
            format!("{field} must be a canonical decimal u64 string"),
        )),
    }
}

/// `None` → no precondition; `"absent"` → the ID must not exist; a
/// canonical decimal string → the record must have exactly that revision.
fn expected(text: Option<&str>) -> Result<Expected> {
    match text {
        None => Ok(Expected::Any),
        Some("absent") => Ok(Expected::Absent),
        Some(text) => match text.parse::<u64>() {
            Ok(value) if value.to_string() == text => Ok(Expected::Revision(value)),
            _ => Err(invalid(
                "expected_revision must be \"absent\" or a canonical decimal u64 string",
            )),
        },
    }
}

fn decode(text: &str) -> Result<Vec<u8>> {
    BASE64_STANDARD
        .decode(text)
        .map_err(|_| invalid("payload must be canonical padded base64"))
}

fn payload(text: &str) -> Result<Vec<u8>> {
    if text.len() > vlite::MAX_PAYLOAD_BYTES.div_ceil(3) * 4 {
        return Err(invalid("payload too large"));
    }
    let bytes = decode(text)?;
    if bytes.len() > vlite::MAX_PAYLOAD_BYTES {
        return Err(invalid("payload too large"));
    }
    Ok(bytes)
}

/// Converts one batch/import operation. Import rows leave the payload size
/// check to the engine so oversize rows are reported per row; the 8 MiB
/// request bound still limits decoding.
fn mutation(operation: Operation, bounded_payload: bool) -> Result<Mutation> {
    Ok(match operation {
        Operation::Put {
            id: text,
            vector,
            payload_base64,
            metadata,
        } => Mutation::PutWithMetadata {
            id: id(&text)?,
            vector,
            payload: if bounded_payload {
                payload(&payload_base64)?
            } else {
                decode(&payload_base64)?
            },
            metadata,
        },
        Operation::Delete { id: text } => Mutation::Delete { id: id(&text)? },
    })
}

fn collection_name(collection: &Option<String>) -> &str {
    collection.as_deref().unwrap_or("default")
}

/// Explicit request values override the collection's declared defaults.
fn ann_options(
    settings: &CollectionSettings,
    quantization: Option<QuantizationName>,
    rerank_multiplier: Option<u8>,
) -> AnnOptions {
    AnnOptions {
        quantization: quantization.map_or(settings.quantization, Quantization::from),
        rerank_multiplier: rerank_multiplier.unwrap_or(settings.rerank_multiplier),
    }
}

fn reject_fields(mode: &str, fields: &[(&str, bool)]) -> Result<()> {
    match fields.iter().find(|(_, present)| *present) {
        Some((field, _)) => Err((
            "invalid_input",
            format!("{field} is not accepted by {mode} search"),
        )),
        None => Ok(()),
    }
}

fn settings(input: Settings) -> Result<CollectionSettings> {
    let mut settings = CollectionSettings::default();
    if let Some(metric) = input.metric {
        settings.metric = metric;
    }
    if let Some(hnsw) = input.hnsw {
        if let Some(m) = hnsw.m {
            settings.hnsw.m = m;
        }
        if let Some(ef_construction) = hnsw.ef_construction {
            settings.hnsw.ef_construction = ef_construction;
        }
        if let Some(seed) = hnsw.seed {
            settings.hnsw.seed = decimal(&seed, "hnsw.seed")?;
        }
    }
    if let Some(quantization) = input.quantization {
        settings.quantization = quantization.into();
    }
    if let Some(rerank_multiplier) = input.rerank_multiplier {
        settings.rerank_multiplier = rerank_multiplier;
    }
    if let Some(planner) = input.planner {
        if let Some(candidates) = planner.exact_max_candidates {
            settings.planner.exact_max_candidates = candidates;
        }
        if let Some(ppm) = planner.exact_max_selectivity_ppm {
            settings.planner.exact_max_selectivity_ppm = ppm;
        }
    }
    settings.metadata_indexes = input
        .metadata_indexes
        .into_iter()
        .map(|declaration| MetadataIndexSpec {
            field: declaration.field,
            kind: match declaration.kind {
                IndexKind::Eq => MetadataIndexKind::Eq,
                IndexKind::Range => MetadataIndexKind::Range,
                IndexKind::EqRange => MetadataIndexKind::EqRange,
            },
        })
        .collect();
    settings.text_fields = input.text_fields;
    // Bounds and duplicates are validated by the engine before any commit.
    Ok(settings)
}

fn quantization_name(quantization: Quantization) -> &'static str {
    match quantization {
        Quantization::None => "none",
        Quantization::ScalarU8 => "scalar_u8",
    }
}

fn settings_json(settings: &CollectionSettings) -> Value {
    json!({
        "metric": settings.metric,
        "hnsw": {
            "m": settings.hnsw.m,
            "ef_construction": settings.hnsw.ef_construction,
            "seed": settings.hnsw.seed.to_string(),
        },
        "quantization": quantization_name(settings.quantization),
        "rerank_multiplier": settings.rerank_multiplier,
        "planner": {
            "exact_max_candidates": settings.planner.exact_max_candidates,
            "exact_max_selectivity_ppm": settings.planner.exact_max_selectivity_ppm,
        },
        "metadata_indexes": settings.metadata_indexes.iter().map(|spec| json!({
            "field": spec.field,
            "kind": match spec.kind {
                MetadataIndexKind::Eq => "eq",
                MetadataIndexKind::Range => "range",
                MetadataIndexKind::EqRange => "eq_range",
            },
        })).collect::<Vec<_>>(),
        "text_fields": settings.text_fields,
    })
}

fn collection_json(info: &CollectionInfo) -> Value {
    json!({
        "id": info.id,
        "name": info.name,
        "dimensions": info.dimensions,
        "live_records": info.live_records,
        "settings": settings_json(&info.settings),
        "settings_persisted": info.settings_persisted,
    })
}

fn record_json(id: &str, record: &Record, revision: Option<u64>) -> Value {
    json!({
        "id": id,
        "vector": record.vector,
        "payload_base64": BASE64_STANDARD.encode(&record.payload),
        "metadata": record.metadata,
        "revision": revision.map(|revision| revision.to_string()),
    })
}

fn hits_json(hits: Vec<SearchHit>) -> Value {
    json!(
        hits.into_iter()
            .map(|h| json!({"id":h.id.to_string(),"distance":h.distance}))
            .collect::<Vec<_>>()
    )
}

fn hybrid_hits_json(hits: Vec<HybridHit>) -> Value {
    json!(
        hits.into_iter()
            .map(|h| json!({
                "id": h.id.to_string(),
                "score": h.score,
                "distance": h.distance,
                "bm25": h.bm25,
                "vector_rank": h.vector_rank,
                "text_rank": h.text_rank,
            }))
            .collect::<Vec<_>>()
    )
}

fn stats_json(stats: &SearchStats) -> Value {
    json!({
        "records_scanned": stats.records_scanned,
        "distance_evaluations": stats.distance_evaluations,
        "quantized_distance_evaluations": stats.quantized_distance_evaluations,
        "graph_nodes_visited": stats.graph_nodes_visited,
        "buffered_records_merged": stats.buffered_records_merged,
        "reranked_candidates": stats.reranked_candidates,
        "index_candidates": stats.index_candidates,
    })
}

fn plan_json(plan: &QueryPlan) -> Value {
    json!({
        "strategy": plan.strategy.name(),
        "estimated_candidates": plan.estimated_candidates,
        "live_records": plan.live_records,
        "index_field": plan.index_field,
        "reason": plan.reason,
    })
}

fn chunk_report_json(report: &ImportChunkReport) -> Value {
    let outcome = match report.outcome {
        ImportChunkOutcome::Committed => "committed",
        ImportChunkOutcome::AlreadyCommitted => "already_committed",
        ImportChunkOutcome::Rejected => "rejected",
        // The enum is non_exhaustive across crates; this crate is always built
        // against its workspace engine, so a new variant fails this test suite.
        other => unreachable!("unmapped import outcome {other:?}"),
    };
    json!({
        "outcome": outcome,
        "job": report.job.to_string(),
        "chunk": report.chunk.to_string(),
        "rows_committed": report.rows_committed,
        "rows_skipped": report.rows_skipped,
        "invalid_rows": report.invalid_rows,
        "invalid": report.invalid.iter().map(|row| json!({
            "row": row.row,
            "code": row.code,
            "message": row.message,
        })).collect::<Vec<_>>(),
        "revision": report.revision.map(|revision| revision.to_string()),
    })
}

fn import_status_json(status: &vlite::ImportStatus) -> Value {
    let state = match status.state {
        ImportState::Open => "open",
        ImportState::Finished => "finished",
        ImportState::Aborted => "aborted",
    };
    let digest: String = status
        .last_digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    json!({
        "job": status.job.to_string(),
        "collection": status.collection,
        "collection_dropped": status.collection_dropped,
        "state": state,
        "last_chunk": status.last_chunk.to_string(),
        "rows_committed": status.rows_committed,
        "rows_skipped": status.rows_skipped,
        "last_digest": digest,
    })
}

fn execute(handle: u64, request: Request) -> Result<Value> {
    let mut state = STATE
        .get_or_init(Mutex::default)
        .lock()
        .map_err(|_| ("panic", "Native owner poisoned; restart the process".into()))?;
    if let Request::Features { request } = request {
        let result = features::execute(&mut state, handle, request)?;
        if result.to_string().len() > 8 * 1024 * 1024 - 1024 {
            return Err((
                "response_too_large",
                "Feature response exceeds 8 MiB; request fewer results or a smaller byte image"
                    .into(),
            ));
        }
        return Ok(result);
    }
    match request {
        Request::Capabilities {} => return Ok(json!({"graph": true})),
        Request::Graph { request } => {
            let result = graph::execute(&mut state, handle, request)?;
            if result.to_string().len() > 8 * 1024 * 1024 - 1024 {
                return Err((
                    "response_too_large",
                    "Graph response exceeds 8 MiB; lower the limits or page with a cursor".into(),
                ));
            }
            return Ok(result);
        }
        _ => {}
    }
    match request {
        Request::Create { .. } | Request::Open { .. } => {
            if handle != 0 {
                return Err(invalid("create/open requires handle 0"));
            }
            let next = state
                .next
                .checked_add(1)
                .ok_or_else(|| invalid("handle space exhausted"))?;
            let database = match request {
                Request::Create {
                    path,
                    dimensions,
                    format,
                    durability: mode,
                    flush_interval_ms,
                } => {
                    let durability = durability(mode.as_deref(), flush_interval_ms, false)?;
                    // The default is a graph-enabled V5 file; an explicit
                    // 3 or 4 creates a plain vector file.
                    let format = match format {
                        None | Some(5) => FormatVersion::V5,
                        Some(3) => FormatVersion::V3,
                        Some(4) => FormatVersion::V4,
                        Some(_) => return Err(invalid("format must be 3, 4 or 5")),
                    };
                    Database::create_with_format(path, dimensions, format)
                        .and_then(|db| with_durability(db, durability))
                }
                Request::Open {
                    path,
                    read_only,
                    durability: mode,
                    flush_interval_ms,
                } => {
                    let durability = durability(mode.as_deref(), flush_interval_ms, false)?;
                    if read_only {
                        Database::open_read_only(path)
                    } else {
                        Database::open(path)
                    }
                    .and_then(|db| with_durability(db, durability))
                }
                _ => unreachable!(),
            }
            .map_err(failure)?;
            state.next = next;
            state.databases.insert(next, database);
            return Ok(json!({"handle":next.to_string()}));
        }
        Request::Close {} => {
            let database = state
                .databases
                .remove(&handle)
                .ok_or_else(|| ("invalid_handle", "Unknown or closed handle".into()))?;
            database.close().map_err(failure)?;
            return Ok(Value::Null);
        }
        _ => {}
    }
    let db = state
        .databases
        .get_mut(&handle)
        .ok_or_else(|| ("invalid_handle", "Unknown or closed handle".into()))?;
    match request {
        Request::Sync {} => {
            db.sync().map_err(failure)?;
            Ok(Value::Null)
        }
        Request::Info {} => {
            db.get(0).map_err(failure)?;
            Ok(
                json!({"dimensions":db.dimensions(),"records":db.len(),"format_version":db.format_version()}),
            )
        }
        Request::Get {
            id: text,
            collection,
        } => {
            let value = id(&text)?;
            let view = db
                .collection(collection_name(&collection))
                .map_err(failure)?;
            let record = view
                .get(value)
                .map_err(failure)?
                .ok_or_else(|| ("not_found", "Vector not found".into()))?;
            Ok(record_json(
                &text,
                record,
                view.revision(value).map_err(failure)?,
            ))
        }
        Request::GetMany { ids, collection } => {
            let view = db
                .collection(collection_name(&collection))
                .map_err(failure)?;
            view.get(0).map_err(failure)?;
            if ids.len() > 100 {
                return Err(invalid("get_many accepts at most 100 IDs"));
            }
            let ids = ids
                .into_iter()
                .map(|text| id(&text).map(|value| (text, value)))
                .collect::<Result<Vec<_>>>()?;
            let mut records = Vec::with_capacity(ids.len());
            let mut bytes = 1024;
            // Bound the complete JSON response, including escaping and the envelope.
            for (text, id) in ids {
                let item = match view.get(id).map_err(failure)? {
                    Some(record) => record_json(&text, record, view.revision(id).map_err(failure)?),
                    None => Value::Null,
                };
                bytes += item.to_string().len() + 1;
                if bytes > 8 * 1024 * 1024 {
                    return Err((
                        "response_too_large",
                        "get_many exceeds 8 MiB; request fewer IDs".into(),
                    ));
                }
                records.push(item);
            }
            Ok(json!({"records":records}))
        }
        Request::Put {
            id: text,
            vector,
            payload_base64,
            metadata,
            collection,
            expected_revision,
        } => {
            let value = id(&text)?;
            let payload = payload(&payload_base64)?;
            let precondition = expected(expected_revision.as_deref())?;
            let revision = db
                .collection_mut(collection_name(&collection))
                .map_err(failure)?
                .put_with_metadata_if(value, &vector, &payload, &metadata, precondition)
                .map_err(failure)?;
            // Unconditional puts keep the original null result.
            Ok(match expected_revision {
                Some(_) => json!({"revision":revision.to_string()}),
                None => Value::Null,
            })
        }
        Request::Delete {
            id: text,
            collection,
            expected_revision,
        } => {
            let value = id(&text)?;
            let precondition = expected(expected_revision.as_deref())?;
            let deleted = db
                .collection_mut(collection_name(&collection))
                .map_err(failure)?
                .delete_if(value, precondition)
                .map_err(failure)?;
            Ok(json!({"deleted":deleted}))
        }
        Request::Batch {
            operations,
            collection,
        } => {
            if operations.len() > vlite::MAX_BATCH_OPERATIONS {
                return Err(invalid("batch exceeds 4096 operations"));
            }
            let operations = operations
                .into_iter()
                .map(|operation| mutation(operation, true))
                .collect::<Result<Vec<_>>>()?;
            db.collection_mut(collection_name(&collection))
                .map_err(failure)?
                .write_batch(&operations)
                .map_err(failure)?;
            Ok(Value::Null)
        }
        Request::Revision {
            id: text,
            collection,
        } => {
            let value = id(&text)?;
            let revision = db
                .collection(collection_name(&collection))
                .map_err(failure)?
                .revision(value)
                .map_err(failure)?;
            Ok(json!({"revision":revision.map(|revision| revision.to_string())}))
        }
        Request::Search {
            vector,
            k,
            filter,
            metric,
            collection,
            mode,
            explain,
            text,
            fusion,
            vector_leg,
            ef_search,
            quantization,
            rerank_multiplier,
        } => {
            let view = db
                .collection(collection_name(&collection))
                .map_err(failure)?;
            let declared = view.settings();
            let metric = metric.unwrap_or(declared.metric);
            match mode {
                SearchMode::Exact => {
                    reject_fields(
                        "exact",
                        &[
                            ("text", text.is_some()),
                            ("fusion", fusion.is_some()),
                            ("vector_leg", vector_leg.is_some()),
                            ("ef_search", ef_search.is_some()),
                            ("quantization", quantization.is_some()),
                            ("rerank_multiplier", rerank_multiplier.is_some()),
                        ],
                    )?;
                    let vector = vector.ok_or_else(|| invalid("exact search requires vector"))?;
                    if explain {
                        let (hits, stats) = view
                            .search_with_stats(&vector, k, &filter, metric)
                            .map_err(failure)?;
                        Ok(json!({"hits":hits_json(hits),"stats":stats_json(&stats)}))
                    } else {
                        let hits = view
                            .search_filtered_with_metric(&vector, k, &filter, metric)
                            .map_err(failure)?;
                        Ok(json!({"hits":hits_json(hits)}))
                    }
                }
                SearchMode::Auto => {
                    reject_fields(
                        "auto",
                        &[
                            ("text", text.is_some()),
                            ("fusion", fusion.is_some()),
                            ("vector_leg", vector_leg.is_some()),
                        ],
                    )?;
                    let vector = vector.ok_or_else(|| invalid("auto search requires vector"))?;
                    let options = ann_options(declared, quantization, rerank_multiplier);
                    let planned = view
                        .search_auto(
                            &vector,
                            k,
                            &filter,
                            metric,
                            declared.hnsw,
                            options,
                            ef_search.unwrap_or(DEFAULT_EF_SEARCH),
                        )
                        .map_err(failure)?;
                    let mut result = json!({"hits":hits_json(planned.hits)});
                    if explain {
                        result["plan"] = plan_json(&planned.plan);
                        result["stats"] = stats_json(&planned.stats);
                    }
                    Ok(result)
                }
                SearchMode::Hybrid => {
                    let text = text.ok_or_else(|| invalid("hybrid search requires text"))?;
                    let leg = match vector_leg.unwrap_or(LegKind::Exact) {
                        LegKind::Exact => {
                            reject_fields(
                                "hybrid exact-leg",
                                &[
                                    ("ef_search", ef_search.is_some()),
                                    ("quantization", quantization.is_some()),
                                    ("rerank_multiplier", rerank_multiplier.is_some()),
                                ],
                            )?;
                            VectorLeg::Exact
                        }
                        LegKind::ApproximateReady => VectorLeg::ApproximateReady {
                            config: declared.hnsw,
                            ef_search: ef_search.unwrap_or(DEFAULT_EF_SEARCH),
                            options: ann_options(declared, quantization, rerank_multiplier),
                        },
                    };
                    let mut options = FusionOptions::default();
                    if let Some(fusion) = fusion {
                        options.candidates = fusion.candidates;
                        options.rrf_k = fusion.rrf_k.unwrap_or(options.rrf_k);
                        options.vector_weight =
                            fusion.vector_weight.unwrap_or(options.vector_weight);
                        options.text_weight = fusion.text_weight.unwrap_or(options.text_weight);
                    }
                    let mut query = HybridQuery::new(vector, text, k);
                    query.conditions = filter;
                    query.metric = metric;
                    query.vector_leg = leg;
                    query.fusion = options;
                    let (hits, stats) = view.search_hybrid(&query).map_err(failure)?;
                    let mut result = json!({"hits":hybrid_hits_json(hits)});
                    if explain {
                        result["stats"] = stats_json(&stats);
                    }
                    Ok(result)
                }
            }
        }
        Request::BuildIndex {
            metric,
            collection,
            quantization,
            rerank_multiplier,
        } => {
            let mut target = db
                .collection_mut(collection_name(&collection))
                .map_err(failure)?;
            let declared = target.view().settings().clone();
            target
                .build_hnsw_with_options(
                    declared.hnsw,
                    metric.unwrap_or(declared.metric),
                    ann_options(&declared, quantization, rerank_multiplier),
                )
                .map_err(failure)?;
            Ok(Value::Null)
        }
        Request::SearchApproximateReady {
            vector,
            k,
            ef_search,
            metric,
            collection,
            quantization,
            rerank_multiplier,
        } => {
            let vector =
                vector.ok_or_else(|| invalid("search_approximate_ready requires vector"))?;
            let view = db
                .collection(collection_name(&collection))
                .map_err(failure)?;
            let declared = view.settings();
            let (hits, _) = view
                .search_approximate_ready_with_options(
                    &vector,
                    k,
                    ef_search,
                    declared.hnsw,
                    ann_options(declared, quantization, rerank_multiplier),
                    &[],
                    metric.unwrap_or(declared.metric),
                )
                .map_err(failure)?;
            Ok(json!({"hits":hits_json(hits)}))
        }
        Request::ListCollections {} => {
            let collections = db.collections().map_err(failure)?;
            Ok(json!({"collections":collections.iter().map(collection_json).collect::<Vec<_>>()}))
        }
        Request::CreateCollection {
            name,
            dimensions,
            settings: input,
        } => {
            let settings = match input {
                Some(input) => settings(input)?,
                None => CollectionSettings::default(),
            };
            let info = db
                .create_collection(
                    &name,
                    CollectionSpec {
                        dimensions,
                        settings,
                    },
                )
                .map_err(failure)?;
            Ok(collection_json(&info))
        }
        Request::DropCollection { name } => {
            db.drop_collection(&name).map_err(failure)?;
            Ok(Value::Null)
        }
        Request::SetCollectionSettings {
            name,
            settings: input,
        } => {
            let persistence = db
                .set_collection_settings(&name, settings(input)?)
                .map_err(failure)?;
            Ok(json!({"persisted":persistence == SettingsPersistence::Persisted}))
        }
        Request::NextImportJobId {} => {
            let job = db.next_import_job_id().map_err(failure)?;
            Ok(json!({"job":job.to_string()}))
        }
        Request::ImportChunk {
            job,
            chunk,
            operations,
            collection,
            policy,
        } => {
            let job = decimal(&job, "job")?;
            let chunk = decimal(&chunk, "chunk")?;
            let operations = operations
                .into_iter()
                .map(|operation| mutation(operation, false))
                .collect::<Result<Vec<_>>>()?;
            let policy = match policy {
                Policy::RejectChunk => InvalidRowPolicy::RejectChunk,
                Policy::SkipInvalid => InvalidRowPolicy::SkipInvalid,
            };
            let report = db
                .collection_mut(collection_name(&collection))
                .map_err(failure)?
                .import_chunk(job, chunk, &operations, policy)
                .map_err(failure)?;
            Ok(chunk_report_json(&report))
        }
        Request::ImportStatus { job } => {
            let status = db.import_status(decimal(&job, "job")?).map_err(failure)?;
            Ok(import_status_json(&status))
        }
        Request::ImportFinish { job, collection } => {
            let job = decimal(&job, "job")?;
            let status = db
                .collection_mut(collection_name(&collection))
                .map_err(failure)?
                .finish_import(job)
                .map_err(failure)?;
            Ok(import_status_json(&status))
        }
        Request::ImportAbort { job, collection } => {
            let job = decimal(&job, "job")?;
            let status = db
                .collection_mut(collection_name(&collection))
                .map_err(failure)?
                .abort_import(job)
                .map_err(failure)?;
            Ok(import_status_json(&status))
        }
        Request::AnnQuality {
            collection,
            metric,
            quantization,
            rerank_multiplier,
            samples,
            k,
            ef_search,
            seed,
            max_distance_evaluations,
        } => {
            let mut sample = QualitySampleOptions::default();
            if let Some(samples) = samples {
                sample.samples = samples;
            }
            if let Some(k) = k {
                sample.k = k;
            }
            if let Some(ef_search) = ef_search {
                sample.ef_search = ef_search;
            }
            if let Some(seed) = seed {
                sample.seed = decimal(&seed, "seed")?;
            }
            if let Some(limit) = max_distance_evaluations {
                sample.max_distance_evaluations = limit;
            }
            let view = db
                .collection(collection_name(&collection))
                .map_err(failure)?;
            let declared = view.settings();
            let report = view
                .ann_quality(
                    declared.hnsw,
                    metric.unwrap_or(declared.metric),
                    ann_options(declared, quantization, rerank_multiplier),
                    sample,
                )
                .map_err(failure)?;
            Ok(json!({
                "recall_at_k": report.recall_at_k,
                "samples_used": report.samples_used,
                "distance_evaluations": report.distance_evaluations,
                "skipped_reason": report.skipped_reason,
            }))
        }
        Request::AnnMemory {
            collection,
            metric,
            quantization,
            rerank_multiplier,
        } => {
            let view = db
                .collection(collection_name(&collection))
                .map_err(failure)?;
            let declared = view.settings();
            let report = view
                .ann_memory(
                    declared.hnsw,
                    metric.unwrap_or(declared.metric),
                    ann_options(declared, quantization, rerank_multiplier),
                )
                .map_err(failure)?;
            Ok(json!({
                "graph_nodes": report.graph_nodes,
                "graph_link_bytes": report.graph_link_bytes,
                "code_bytes": report.code_bytes,
                "codebook_bytes": report.codebook_bytes,
                "buffer_vector_bytes": report.buffer_vector_bytes,
                "resident_vector_bytes": report.resident_vector_bytes,
            }))
        }
        Request::Create { .. }
        | Request::Open { .. }
        | Request::Close {}
        | Request::Features { .. }
        | Request::Graph { .. }
        | Request::Capabilities {} => unreachable!(),
    }
}

/// Returns the major ABI version supported by this library.
#[unsafe(no_mangle)]
pub extern "C" fn vlite_abi_version() -> u32 {
    1
}

/// Execute one UTF-8 JSON request. Returns an owned NUL-terminated JSON envelope.
///
/// # Safety
/// `input` must reference `length` readable bytes for the duration of this call.
/// A null pointer is rejected. Free the returned pointer exactly once with
/// `vlite_response_free`; never use a foreign allocator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vlite_execute(
    handle: u64,
    input: *const u8,
    length: usize,
) -> *mut c_char {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if input.is_null() || length == 0 || length > 8 * 1024 * 1024 {
            return Err(invalid("request must contain 1..8388608 UTF-8 JSON bytes"));
        }
        // The caller owns the input; decoding produces fully owned request fields.
        let bytes = unsafe { std::slice::from_raw_parts(input, length) };
        let request =
            serde_json::from_slice(bytes).map_err(|e| ("invalid_input", e.to_string()))?;
        execute(handle, request)
    }))
    .unwrap_or_else(|_| {
        Err((
            "panic",
            "Native panic; owner may be poisoned; restart the process".into(),
        ))
    });
    response(result)
}

fn response(result: Result<Value>) -> *mut c_char {
    let envelope = match result {
        Ok(value) => json!({"ok":true,"result":value}),
        Err((code, message)) => json!({"ok":false,"error":{"code":code,"message":message}}),
    };
    CString::new(envelope.to_string())
        .expect("JSON escapes NUL")
        .into_raw()
}

/// Release a response allocated by `vlite_execute`. NULL is a no-op.
///
/// # Safety
/// A non-null pointer must be an unfreed response returned by this same library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vlite_response_free(response: *mut c_char) {
    if !response.is_null() {
        drop(unsafe { CString::from_raw(response) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(handle: u64, request: &[u8]) -> Value {
        unsafe {
            let response = vlite_execute(handle, request.as_ptr(), request.len());
            let value =
                serde_json::from_slice(std::ffi::CStr::from_ptr(response).to_bytes()).unwrap();
            vlite_response_free(response);
            value
        }
    }

    #[test]
    fn abi_rejects_malformed_requests_and_stale_handles() {
        assert_eq!(vlite_abi_version(), 1);
        for request in [
            b"[".as_slice(),
            b"\xff",
            b"{}",
            b"{\"op\":\"info\",\"extra\":true}",
        ] {
            assert_eq!(call(0, request)["error"]["code"], "invalid_input");
        }
        assert_eq!(
            call(0, b"{\"op\":\"info\"}")["error"]["code"],
            "invalid_handle"
        );
        unsafe {
            let response = vlite_execute(0, std::ptr::null(), 0);
            assert!(!response.is_null());
            vlite_response_free(response);
            vlite_response_free(std::ptr::null_mut());
        }
    }

    #[test]
    fn abi_handles_close_and_reopen_without_reusing_ids() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("abi.vlite");
        let create = json!({"op":"create","path":path,"dimensions":2}).to_string();
        let first = call(0, create.as_bytes());
        assert_eq!(first["ok"], true);
        let first: u64 = first["result"]["handle"].as_str().unwrap().parse().unwrap();
        assert_eq!(
            call(first, b"{\"op\":\"put\",\"id\":\"1\",\"vector\":[1,0]}")["ok"],
            true
        );
        assert_eq!(call(first, b"{\"op\":\"close\"}")["ok"], true);
        assert_eq!(
            call(first, b"{\"op\":\"close\"}")["error"]["code"],
            "invalid_handle"
        );
        let open = json!({"op":"open","path":path}).to_string();
        let second = call(0, open.as_bytes());
        let second: u64 = second["result"]["handle"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_ne!(first, second);
        assert_eq!(
            call(second, b"{\"op\":\"get\",\"id\":\"1\"}")["result"]["vector"],
            json!([1.0, 0.0])
        );
        assert_eq!(call(second, b"{\"op\":\"close\"}")["ok"], true);
    }

    fn request(handle: u64, value: Value) -> Value {
        call(handle, value.to_string().as_bytes())
    }

    fn ok(handle: u64, value: Value) -> Value {
        let response = request(handle, value.clone());
        assert_eq!(response["ok"], true, "{value} -> {response}");
        response["result"].clone()
    }

    fn code(handle: u64, value: Value) -> String {
        let response = request(handle, value.clone());
        assert_eq!(response["ok"], false, "{value} -> {response}");
        response["error"]["code"].as_str().unwrap().to_owned()
    }

    fn create(path: &std::path::Path, dimensions: usize, format: Option<u32>) -> u64 {
        let mut value = json!({"op":"create","path":path,"dimensions":dimensions});
        if let Some(format) = format {
            value["format"] = json!(format);
        }
        ok(0, value)["handle"].as_str().unwrap().parse().unwrap()
    }

    fn open(path: &std::path::Path) -> u64 {
        ok(0, json!({"op":"open","path":path}))["handle"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap()
    }

    #[test]
    fn periodic_is_the_default_and_durability_modes_are_validated() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let path = directory.path().join("periodic.vlite");
        let feature = |value| json!({"op":"features","request":value});
        for (index, bad) in [json!(0), json!(60_001)].into_iter().enumerate() {
            let missing = directory.path().join(format!("bad-{index}.vlite"));
            assert_eq!(
                code(
                    0,
                    json!({"op":"create","path":missing,"dimensions":2,"flush_interval_ms":bad})
                ),
                "invalid_durability"
            );
            assert_eq!(
                code(
                    0,
                    feature(json!({"op":"create","path":missing,"dimensions":2,
                        "options":{"flush_interval_ms":bad}}))
                ),
                "invalid_durability"
            );
            assert!(!missing.exists());
        }
        let missing = directory.path().join("bad-mode.vlite");
        for options in [
            json!({"durability":"none"}),
            json!({"durability":"sometimes"}),
            json!({"durability":"full","flush_interval_ms":10}),
        ] {
            let mut create = json!({"op":"create","path":missing,"dimensions":2});
            for (key, value) in options.as_object().unwrap() {
                create[key] = value.clone();
            }
            assert_eq!(code(0, create), "invalid_durability");
            assert_eq!(
                code(
                    0,
                    feature(json!({"op":"create","path":missing,"dimensions":2,
                        "options":options}))
                ),
                "invalid_durability"
            );
            assert!(!missing.exists());
        }
        for options in [
            json!({"durability":"full"}),
            json!({"durability":"periodic"}),
            json!({"flush_interval_ms":5}),
        ] {
            assert_eq!(
                code(
                    0,
                    feature(json!({"op":"create","dimensions":2,"options":options}))
                ),
                "invalid_durability"
            );
        }
        let memory: u64 = ok(
            0,
            feature(json!({"op":"create","dimensions":2,"options":{"durability":"none"}})),
        )["handle"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        ok(memory, json!({"op":"put","id":"1","vector":[1,0]}));
        ok(memory, json!({"op":"sync"}));
        ok(memory, json!({"op":"close"}));
        let full: u64 = ok(
            0,
            json!({"op":"create","path":directory.path().join("full.vlite"),
                "dimensions":2,"durability":"full"}),
        )["handle"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        ok(full, json!({"op":"put","id":"1","vector":[1,0]}));
        ok(full, json!({"op":"close"}));
        let db: u64 = ok(
            0,
            json!({"op":"create","path":path,"dimensions":2,"flush_interval_ms":60_000}),
        )["handle"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        ok(db, json!({"op":"put","id":"1","vector":[1,0]}));
        assert_eq!(ok(db, json!({"op":"sync"})), Value::Null);
        ok(db, json!({"op":"put","id":"2","vector":[0,1]}));
        ok(db, json!({"op":"close"}));
        let db: u64 = ok(
            0,
            feature(json!({"op":"open","path":path,"flush_interval_ms":5})),
        )["handle"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(ok(db, json!({"op":"info"}))["records"], 2);
        ok(db, json!({"op":"sync"}));
        ok(db, json!({"op":"close"}));
        let db = open(&path);
        ok(db, json!({"op":"sync"}));
        ok(db, json!({"op":"close"}));
    }

    #[test]
    fn read_only_handles_share_locks_reject_writes_and_remain_usable() {
        let directory = tempfile::tempdir_in(".").unwrap();
        for encrypted in [false, true] {
            let path = directory.path().join(if encrypted {
                "encrypted.vlite"
            } else {
                "plain.vlite"
            });
            let credentials = json!({"key_base64":"BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc="});
            let feature = |value| json!({"op":"features","request":value});
            let writer = if encrypted {
                ok(0, feature(json!({"op":"create","path":path,"dimensions":2,"options":{"credentials":credentials}})))["handle"]
                    .as_str().unwrap().parse().unwrap()
            } else {
                create(&path, 2, None)
            };
            ok(writer, json!({"op":"put","id":"1","vector":[1,0]}));
            let open_request = |read_only: Option<bool>| {
                let mut value = json!({"op":"open","path":path});
                if let Some(read_only) = read_only {
                    value["read_only"] = json!(read_only);
                }
                if encrypted {
                    value["credentials"] = credentials.clone();
                    feature(value)
                } else {
                    value
                }
            };
            assert_eq!(code(0, open_request(Some(true))), "locked");
            ok(writer, json!({"op":"close"}));
            let first: u64 = ok(0, open_request(Some(true)))["handle"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap();
            let second: u64 = ok(0, open_request(Some(true)))["handle"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(code(0, open_request(None)), "locked");
            for mutation in [
                json!({"op":"put","id":"2","vector":[0,1]}),
                json!({"op":"delete","id":"1"}),
                json!({"op":"batch","operations":[{"op":"put","id":"2","vector":[0,1]},{"op":"delete","id":"1"}]}),
            ] {
                assert_eq!(code(first, mutation), "read_only");
                for reader in [first, second] {
                    assert_eq!(
                        ok(reader, json!({"op":"get","id":"1"}))["vector"],
                        json!([1.0, 0.0])
                    );
                    assert_eq!(
                        ok(reader, json!({"op":"search","vector":[1,0],"k":1}))["hits"][0]["id"],
                        "1"
                    );
                    assert_eq!(ok(reader, json!({"op":"info"}))["records"], 1);
                }
            }
            ok(first, json!({"op":"close"}));
            assert_eq!(code(0, open_request(None)), "locked");
            ok(second, json!({"op":"close"}));
            // Omitting read_only retains exclusive, writable open behavior on both routes.
            let writer: u64 = ok(0, open_request(None))["handle"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap();
            ok(writer, json!({"op":"put","id":"2","vector":[0,1]}));
            assert_eq!(code(0, open_request(Some(true))), "locked");
            ok(writer, json!({"op":"close"}));
            let reopened: u64 = ok(0, open_request(Some(true)))["handle"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(ok(reopened, json!({"op":"get","id":"2"}))["id"], "2");
            ok(reopened, json!({"op":"close"}));
        }
    }

    fn tag(value: &str) -> Value {
        json!({"tag":{"type":"string","value":value}})
    }

    #[test]
    fn explicit_v3_contract_is_unchanged_and_v4_is_opt_in() {
        let directory = tempfile::tempdir().unwrap();
        let v3 = create(&directory.path().join("v3.vlite"), 2, Some(3));
        assert_eq!(
            ok(v3, json!({"op":"info"})),
            json!({"dimensions":2,"records":0,"format_version":3})
        );
        // Plain put keeps its null result; get additively reports the revision.
        assert_eq!(
            ok(v3, json!({"op":"put","id":"7","vector":[1,0]})),
            Value::Null
        );
        let record = ok(v3, json!({"op":"get","id":"7"}));
        let revision = record["revision"].as_str().unwrap().to_owned();
        assert_eq!(
            ok(v3, json!({"op":"revision","id":"7"}))["revision"],
            revision
        );
        assert_eq!(
            ok(v3, json!({"op":"get_many","ids":["7"]}))["records"][0],
            record
        );
        assert_eq!(
            ok(v3, json!({"op":"get","id":"7","collection":"default"})),
            record
        );
        for value in [
            json!({"op":"create_collection","name":"docs","dimensions":2}),
            json!({"op":"drop_collection","name":"docs"}),
            json!({"op":"next_import_job_id"}),
            json!({"op":"import_status","job":"1"}),
            json!({"op":"import_chunk","job":"1","chunk":"1","operations":[]}),
        ] {
            assert_eq!(code(v3, value), "requires_format_v4");
        }
        assert_eq!(
            code(v3, json!({"op":"get","id":"7","collection":"docs"})),
            "collection_not_found"
        );
        assert_eq!(
            ok(
                v3,
                json!({"op":"set_collection_settings","name":"default","settings":{}})
            ),
            json!({"persisted":false})
        );
        ok(v3, json!({"op":"close"}));
        for format in [0, 2, 6] {
            assert_eq!(
                code(
                    0,
                    json!({"op":"create","path":directory.path().join(format!("f{format}.vlite")),"dimensions":2,"format":format})
                ),
                "invalid_input"
            );
        }
        let v5 = create(&directory.path().join("v5.vlite"), 2, Some(5));
        assert_eq!(ok(v5, json!({"op":"info"}))["format_version"], 5);
        ok(v5, json!({"op":"close"}));
        let v4 = create(&directory.path().join("v4.vlite"), 2, Some(4));
        assert_eq!(ok(v4, json!({"op":"info"}))["format_version"], 4);
        ok(v4, json!({"op":"close"}));
        let explicit = create(&directory.path().join("v3-explicit.vlite"), 2, Some(3));
        assert_eq!(ok(explicit, json!({"op":"info"}))["format_version"], 3);
        ok(explicit, json!({"op":"close"}));
    }

    #[test]
    fn collections_settings_and_scoped_records_persist() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("collections.vlite");
        let db = create(&path, 3, Some(4));
        let settings = json!({
            "metric":"cosine",
            "hnsw":{"m":8,"ef_construction":64,"seed":"18446744073709551615"},
            "quantization":"scalar_u8",
            "rerank_multiplier":2,
            "planner":{"exact_max_candidates":10,"exact_max_selectivity_ppm":0},
            "metadata_indexes":[{"field":"tag","kind":"eq_range"}],
            "text_fields":["title"]
        });
        let info = ok(
            db,
            json!({"op":"create_collection","name":"docs","dimensions":2,"settings":settings}),
        );
        assert_eq!(info["name"], "docs");
        assert_eq!(info["id"], 1);
        assert_eq!(info["dimensions"], 2);
        assert_eq!(info["live_records"], 0);
        assert_eq!(info["settings_persisted"], true);
        assert_eq!(info["settings"], settings);
        assert_eq!(
            code(
                db,
                json!({"op":"create_collection","name":"docs","dimensions":2})
            ),
            "collection_exists"
        );
        for name in ["Docs", "default", ""] {
            let failure = code(
                db,
                json!({"op":"create_collection","name":name,"dimensions":2}),
            );
            assert!(
                failure == "invalid_input" || failure == "default_collection_immutable",
                "{name}: {failure}"
            );
        }
        for bad in [
            json!({"rerank_multiplier":0}),
            json!({"unknown":true}),
            json!({"metadata_indexes":[{"field":"a","kind":"eq"},{"field":"a","kind":"range"}]}),
            json!({"hnsw":{"seed":"01"}}),
            json!({"quantization":"pq"}),
        ] {
            assert_eq!(
                code(
                    db,
                    json!({"op":"create_collection","name":"bad","dimensions":2,"settings":bad})
                ),
                "invalid_input"
            );
        }
        // Same ID in two collections, each with its own dimensions.
        ok(db, json!({"op":"put","id":"1","vector":[1,0,0]}));
        ok(
            db,
            json!({"op":"put","id":"1","vector":[0,1],"collection":"docs","metadata":tag("a")}),
        );
        assert_eq!(
            code(
                db,
                json!({"op":"put","id":"2","vector":[0,1,0],"collection":"docs"})
            ),
            "invalid_input"
        );
        assert_eq!(
            ok(db, json!({"op":"get","id":"1","collection":"docs"}))["vector"],
            json!([0.0, 1.0])
        );
        assert_eq!(
            ok(db, json!({"op":"get","id":"1"}))["vector"],
            json!([1.0, 0.0, 0.0])
        );
        ok(
            db,
            json!({"op":"batch","collection":"docs","operations":[
                {"op":"put","id":"2","vector":[1,1]},{"op":"delete","id":"9"}
            ]}),
        );
        let listed = ok(db, json!({"op":"list_collections"}))["collections"].clone();
        assert_eq!(listed.as_array().unwrap().len(), 2);
        assert_eq!(listed[0]["name"], "default");
        assert_eq!(listed[0]["live_records"], 1);
        assert_eq!(listed[1]["live_records"], 2);
        assert_eq!(
            code(db, json!({"op":"get","id":"1","collection":"missing"})),
            "collection_not_found"
        );
        assert_eq!(
            code(db, json!({"op":"drop_collection","name":"docs"})),
            "collection_not_empty"
        );
        assert_eq!(
            code(db, json!({"op":"drop_collection","name":"default"})),
            "default_collection_immutable"
        );
        assert_eq!(
            ok(
                db,
                json!({"op":"set_collection_settings","name":"docs","settings":{"text_fields":["body"]}})
            ),
            json!({"persisted":true})
        );
        ok(db, json!({"op":"close"}));
        let db = open(&path);
        let listed = ok(db, json!({"op":"list_collections"}))["collections"].clone();
        // Settings are a full replacement: omitted fields return to defaults.
        assert_eq!(listed[1]["settings"]["text_fields"], json!(["body"]));
        assert_eq!(listed[1]["settings"]["metric"], "squared_l2");
        assert_eq!(listed[1]["settings"]["hnsw"]["seed"], "42");
        for id in ["1", "2"] {
            ok(db, json!({"op":"delete","id":id,"collection":"docs"}));
        }
        assert_eq!(
            ok(db, json!({"op":"drop_collection","name":"docs"})),
            Value::Null
        );
        assert_eq!(
            code(db, json!({"op":"get","id":"1","collection":"docs"})),
            "collection_not_found"
        );
        ok(db, json!({"op":"close"}));
    }

    #[test]
    fn revision_preconditions_are_compare_and_set() {
        let directory = tempfile::tempdir().unwrap();
        let db = create(&directory.path().join("cas.vlite"), 2, Some(4));
        let first = ok(
            db,
            json!({"op":"put","id":"18446744073709551615","vector":[1,0],"expected_revision":"absent"}),
        )["revision"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            code(
                db,
                json!({"op":"put","id":"18446744073709551615","vector":[0,1],"expected_revision":"absent"})
            ),
            "precondition_failed"
        );
        let second = ok(
            db,
            json!({"op":"put","id":"18446744073709551615","vector":[0,1],"expected_revision":first}),
        )["revision"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_ne!(first, second);
        assert_eq!(
            code(
                db,
                json!({"op":"put","id":"18446744073709551615","vector":[1,1],"expected_revision":first})
            ),
            "precondition_failed"
        );
        // A failed precondition writes nothing.
        let record = ok(db, json!({"op":"get","id":"18446744073709551615"}));
        assert_eq!(record["vector"], json!([0.0, 1.0]));
        assert_eq!(record["revision"], second);
        for bad in ["any", "01", "-1", ""] {
            assert_eq!(
                code(
                    db,
                    json!({"op":"put","id":"1","vector":[1,0],"expected_revision":bad})
                ),
                "invalid_input"
            );
        }
        assert_eq!(
            code(
                db,
                json!({"op":"delete","id":"18446744073709551615","expected_revision":first})
            ),
            "precondition_failed"
        );
        assert_eq!(
            ok(
                db,
                json!({"op":"delete","id":"18446744073709551615","expected_revision":second})
            ),
            json!({"deleted":true})
        );
        assert_eq!(
            ok(
                db,
                json!({"op":"delete","id":"5","expected_revision":"absent"})
            ),
            json!({"deleted":false})
        );
        assert_eq!(
            ok(db, json!({"op":"revision","id":"18446744073709551615"})),
            json!({"revision":null})
        );
        ok(db, json!({"op":"close"}));
    }

    #[test]
    fn planned_hybrid_and_quantized_search_report_plans_and_quality() {
        let directory = tempfile::tempdir().unwrap();
        let db = create(&directory.path().join("search.vlite"), 2, Some(4));
        ok(
            db,
            json!({"op":"create_collection","name":"docs","dimensions":2,"settings":{
                "metadata_indexes":[{"field":"tag","kind":"eq"}],"text_fields":["title"]
            }}),
        );
        let operations = (0..40)
            .map(|i| {
                let angle = i as f32 / 7.0;
                json!({"op":"put","id":i.to_string(),"vector":[angle.cos(),angle.sin()],"metadata":{
                    "tag":{"type":"string","value":if i == 3 {"rare"} else {"common"}},
                    "title":{"type":"string","value":if i == 5 {"red apple"} else {"green pear"}}
                }})
            })
            .collect::<Vec<_>>();
        ok(
            db,
            json!({"op":"batch","collection":"docs","operations":operations}),
        );
        let base = json!({"op":"search","collection":"docs","vector":[1,0],"k":3});
        // Default exact mode is unchanged: hits only.
        let exact = ok(db, base.clone());
        assert_eq!(exact.as_object().unwrap().len(), 1);
        assert_eq!(exact["hits"][0]["id"], "0");
        let mut explained = base.clone();
        explained["explain"] = json!(true);
        let explained = ok(db, explained);
        assert_eq!(explained["hits"], exact["hits"]);
        assert_eq!(explained["stats"]["records_scanned"], 40);
        for (field, value) in [
            ("text", json!("apple")),
            ("ef_search", json!(10)),
            ("quantization", json!("none")),
            ("vector_leg", json!("exact")),
        ] {
            let mut bad = base.clone();
            bad[field] = value;
            assert_eq!(code(db, bad), "invalid_input", "{field}");
        }
        let mut auto = base.clone();
        auto["mode"] = json!("auto");
        auto["explain"] = json!(true);
        let planned = ok(db, auto.clone());
        assert_eq!(planned["plan"]["strategy"], "exact_scan");
        assert_eq!(planned["hits"], exact["hits"]);
        auto["filter"] =
            json!([{"op":"eq","field":"tag","value":{"type":"string","value":"rare"}}]);
        let planned = ok(db, auto.clone());
        assert_eq!(planned["plan"]["strategy"], "exact_indexed");
        assert_eq!(planned["plan"]["index_field"], "tag");
        assert_eq!(planned["plan"]["estimated_candidates"], 1);
        assert_eq!(planned["stats"]["index_candidates"], 1);
        assert_eq!(planned["hits"].as_array().unwrap().len(), 1);
        assert_eq!(planned["hits"][0]["id"], "3");
        let mut quiet = auto.clone();
        quiet["explain"] = json!(false);
        assert!(ok(db, quiet).get("plan").is_none());

        // Ready-only ANN in a collection, then quantized ANN with exact rerank.
        assert_eq!(
            code(db, json!({"op":"ann_memory","collection":"docs"})),
            "index_not_ready"
        );
        ok(db, json!({"op":"build_index","collection":"docs"}));
        auto.as_object_mut().unwrap().remove("filter");
        let planned = ok(db, auto.clone());
        assert_eq!(planned["plan"]["strategy"], "ann");
        let memory = ok(db, json!({"op":"ann_memory","collection":"docs"}));
        assert_eq!(memory["graph_nodes"], 40);
        assert_eq!(memory["code_bytes"], 0);
        assert_eq!(memory["resident_vector_bytes"], 40 * 2 * 4);
        let mut quantized = auto.clone();
        quantized["quantization"] = json!("scalar_u8");
        assert_eq!(ok(db, quantized.clone())["plan"]["strategy"], "exact_scan");
        ok(
            db,
            json!({"op":"build_index","collection":"docs","quantization":"scalar_u8","rerank_multiplier":4}),
        );
        let planned = ok(db, quantized);
        assert_eq!(planned["plan"]["strategy"], "ann");
        assert!(
            planned["stats"]["quantized_distance_evaluations"]
                .as_u64()
                .unwrap()
                > 0
        );
        assert!(planned["stats"]["reranked_candidates"].as_u64().unwrap() > 0);
        assert_eq!(planned["hits"][0]["id"], "0");
        assert_eq!(planned["hits"][0]["distance"], 0.0);
        let ready = ok(
            db,
            json!({"op":"search_approximate_ready","collection":"docs","vector":[1,0],"k":1,"ef_search":20,"quantization":"scalar_u8"}),
        );
        assert_eq!(ready["hits"][0]["id"], "0");
        assert_eq!(
            code(
                db,
                json!({"op":"search_approximate_ready","collection":"docs","vector":[1,0],"k":1,"ef_search":20})
            ),
            "index_not_ready"
        );
        let memory = ok(
            db,
            json!({"op":"ann_memory","collection":"docs","quantization":"scalar_u8"}),
        );
        assert_eq!(memory["code_bytes"], 40 * 2);
        assert!(memory["codebook_bytes"].as_u64().unwrap() > 0);
        let quality = ok(
            db,
            json!({"op":"ann_quality","collection":"docs","quantization":"scalar_u8","samples":4,"k":3,"ef_search":40,"seed":"18446744073709551615"}),
        );
        assert_eq!(quality["samples_used"], 4);
        assert!(quality["recall_at_k"].as_f64().unwrap() > 0.5);
        assert_eq!(quality["skipped_reason"], Value::Null);
        let skipped = ok(
            db,
            json!({"op":"ann_quality","collection":"docs","quantization":"scalar_u8","max_distance_evaluations":1}),
        );
        assert_eq!(skipped["skipped_reason"], "budget");
        assert_eq!(skipped["recall_at_k"], Value::Null);
        assert_eq!(
            code(
                db,
                json!({"op":"ann_quality","collection":"docs","quantization":"scalar_u8","samples":0})
            ),
            "invalid_input"
        );

        // Hybrid: BM25 over declared text fields fused with the vector leg.
        let hybrid = ok(
            db,
            json!({"op":"search","collection":"docs","mode":"hybrid","text":"apple","k":2,
                "fusion":{"vector_weight":0,"text_weight":1},"explain":true}),
        );
        assert_eq!(hybrid["hits"][0]["id"], "5");
        assert_eq!(hybrid["hits"][0]["text_rank"], 1);
        assert!(hybrid["hits"][0]["bm25"].as_f64().unwrap() > 0.0);
        assert_eq!(hybrid["hits"][0]["distance"], Value::Null);
        assert!(hybrid.get("stats").is_some());
        let fused = ok(
            db,
            json!({"op":"search","collection":"docs","mode":"hybrid","text":"apple","vector":[1,0],"k":3}),
        );
        assert_eq!(fused["hits"].as_array().unwrap().len(), 3);
        // Record 5 is ranked by both legs, so fusion places it first.
        assert_eq!(fused["hits"][0]["id"], "5");
        assert_eq!(fused["hits"][0]["text_rank"], 1);
        assert!(fused["hits"][0]["vector_rank"].as_u64().unwrap() > 1);
        assert_eq!(fused["hits"][1]["id"], "0");
        assert_eq!(fused["hits"][1]["text_rank"], Value::Null);
        let approximate = ok(
            db,
            json!({"op":"search","collection":"docs","mode":"hybrid","text":"apple","vector":[1,0],"k":3,
                "vector_leg":"approximate_ready","quantization":"scalar_u8","ef_search":60}),
        );
        assert_eq!(approximate["hits"].as_array().unwrap().len(), 3);
        assert_eq!(
            code(
                db,
                json!({"op":"search","collection":"docs","mode":"hybrid","text":"apple","vector":[1,0],"k":3,
                    "vector_leg":"approximate_ready"})
            ),
            "index_not_ready"
        );
        assert_eq!(
            code(
                db,
                json!({"op":"search","collection":"docs","mode":"hybrid","vector":[1,0],"k":3})
            ),
            "invalid_input"
        );
        assert_eq!(
            code(
                db,
                json!({"op":"search","collection":"docs","mode":"hybrid","text":"apple","k":3,"ef_search":5})
            ),
            "invalid_input"
        );
        // The default collection declares no text fields.
        assert_eq!(
            code(
                db,
                json!({"op":"search","mode":"hybrid","text":"apple","k":3})
            ),
            "invalid_input"
        );
        ok(db, json!({"op":"close"}));
    }

    #[test]
    fn resumable_imports_are_exactly_once_and_durable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("imports.vlite");
        let db = create(&path, 2, Some(4));
        ok(
            db,
            json!({"op":"create_collection","name":"docs","dimensions":2}),
        );
        let job = ok(db, json!({"op":"next_import_job_id"}))["job"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(job, "1");
        let rows =
            json!([{"op":"put","id":"1","vector":[1,0]},{"op":"put","id":"2","vector":[0,1]}]);
        let chunk = json!({"op":"import_chunk","job":job,"chunk":"1","collection":"docs","operations":rows});
        let report = ok(db, chunk.clone());
        assert_eq!(report["outcome"], "committed");
        assert_eq!(report["job"], "1");
        assert_eq!(report["chunk"], "1");
        assert_eq!(report["rows_committed"], 2);
        assert_eq!(report["rows_skipped"], 0);
        assert!(report["revision"].as_str().is_some());
        let again = ok(db, chunk.clone());
        assert_eq!(again["outcome"], "already_committed");
        assert_eq!(again["revision"], Value::Null);
        let mut changed = chunk.clone();
        changed["operations"][0]["vector"] = json!([0.5, 0.5]);
        assert_eq!(code(db, changed), "import_conflict");
        let mut other_collection = chunk.clone();
        other_collection
            .as_object_mut()
            .unwrap()
            .remove("collection");
        assert_eq!(code(db, other_collection), "import_conflict");
        // Reject policy (default) writes nothing and lists invalid rows.
        let bad_rows =
            json!([{"op":"put","id":"3","vector":[1,0]},{"op":"put","id":"4","vector":[1,0,0]}]);
        let rejected = ok(
            db,
            json!({"op":"import_chunk","job":job,"chunk":"2","collection":"docs","operations":bad_rows}),
        );
        assert_eq!(rejected["outcome"], "rejected");
        assert_eq!(rejected["invalid_rows"], 1);
        assert_eq!(rejected["invalid"][0]["row"], 1);
        assert_eq!(rejected["invalid"][0]["code"], "dimension_mismatch");
        assert_eq!(
            code(db, json!({"op":"get","id":"3","collection":"docs"})),
            "not_found"
        );
        let skipped = ok(
            db,
            json!({"op":"import_chunk","job":job,"chunk":"2","collection":"docs","operations":bad_rows,"policy":"skip_invalid"}),
        );
        assert_eq!(skipped["outcome"], "committed");
        assert_eq!(skipped["rows_committed"], 1);
        assert_eq!(skipped["rows_skipped"], 1);
        for (field, value) in [("job", "01"), ("chunk", "x"), ("job", "")] {
            let mut bad = chunk.clone();
            bad[field] = json!(value);
            assert_eq!(code(db, bad), "invalid_input");
        }
        assert_eq!(
            code(
                db,
                json!({"op":"import_chunk","job":job,"chunk":"3","collection":"docs","operations":[],"policy":"maybe"})
            ),
            "invalid_input"
        );
        assert_eq!(
            code(db, json!({"op":"drop_collection","name":"docs"})),
            "collection_not_empty"
        );
        ok(db, json!({"op":"close"}));
        let db = open(&path);
        let status = ok(db, json!({"op":"import_status","job":job}));
        assert_eq!(status["state"], "open");
        assert_eq!(status["collection"], "docs");
        assert_eq!(status["collection_dropped"], false);
        assert_eq!(status["last_chunk"], "2");
        assert_eq!(status["rows_committed"], 3);
        assert_eq!(status["rows_skipped"], 1);
        assert_eq!(status["last_digest"].as_str().unwrap().len(), 64);
        // The exactly-once identity survives reopen.
        assert_eq!(
            ok(
                db,
                json!({"op":"import_chunk","job":job,"chunk":"2","collection":"docs","operations":bad_rows,"policy":"skip_invalid"})
            )["outcome"],
            "already_committed"
        );
        assert_eq!(
            code(db, json!({"op":"import_finish","job":job})),
            "import_conflict"
        );
        let finished = ok(
            db,
            json!({"op":"import_finish","job":job,"collection":"docs"}),
        );
        assert_eq!(finished["state"], "finished");
        assert_eq!(
            ok(
                db,
                json!({"op":"import_finish","job":job,"collection":"docs"})
            )["state"],
            "finished"
        );
        assert_eq!(
            code(
                db,
                json!({"op":"import_abort","job":job,"collection":"docs"})
            ),
            "import_conflict"
        );
        assert_eq!(
            code(db, json!({"op":"import_status","job":"99"})),
            "import_not_found"
        );
        let next = ok(db, json!({"op":"next_import_job_id"}))["job"].clone();
        assert_eq!(next, "2");
        ok(
            db,
            json!({"op":"import_chunk","job":"2","chunk":"1","operations":[{"op":"put","id":"8","vector":[1,1]}]}),
        );
        let aborted = ok(db, json!({"op":"import_abort","job":"2"}));
        assert_eq!(aborted["state"], "aborted");
        assert_eq!(aborted["collection"], "default");
        // Abort never rolls back committed chunks.
        assert_eq!(
            ok(db, json!({"op":"get","id":"8"}))["vector"],
            json!([1.0, 1.0])
        );
        ok(db, json!({"op":"close"}));
    }
}
