//! Embedded, single-file vector storage with synchronous per-operation commits.
//!
//! See the project README for the durability and explicit tail-repair contract.

mod ann_snapshot;
pub mod changes;
mod collection;
mod crypto;
pub mod disk_ann;
#[cfg(feature = "gpu")]
pub mod gpu;
mod storage;
pub use disk_ann::{DiskAnnIndex, DiskAnnMemory, DiskAnnOptions, DiskAnnSource};
#[cfg(feature = "gpu")]
pub use gpu::GpuExactSearcher;
pub mod replica;
pub use replica::{ReplicaFollower, ReplicaStatus};
mod logops;
pub use crypto::{EncryptionKey, KdfParams};
pub use logops::LogSegment;
pub mod embedding;
mod format;
mod hnsw;
pub mod hybrid;
pub mod import;
mod metadata;
pub mod metadata_index;
pub mod planner;
#[cfg(feature = "graph")]
pub mod property_graph;
pub mod quality;
pub mod quantize;
pub mod query_language;
mod rebuild;
pub mod records;
pub mod rerank;
mod stats;
pub mod text_index;
pub mod vectors;
pub use ann_snapshot::{
    AnnSnapshotLoad, AnnSnapshotRejection, AnnSnapshotReport, MAX_ANN_SNAPSHOT_BYTES,
};
pub use collection::{
    CollectionInfo, CollectionMut, CollectionRef, CollectionSettings, CollectionSpec,
    SettingsPersistence,
};
pub use hnsw::{HnswConfig, MAX_EF_SEARCH, MAX_HNSW_BUFFER_IDS, MAX_HNSW_BUFFER_VECTOR_BYTES};
pub use import::{
    ImportChunkOutcome, ImportChunkReport, ImportJobId, ImportState, ImportStatus, InvalidRow,
    InvalidRowPolicy, MAX_REPORTED_INVALID_ROWS,
};
pub use metadata::{
    Condition, MAX_FILTER_CONDITIONS, MAX_METADATA_BYTES, MAX_METADATA_FIELDS, Metadata,
    MetadataValue,
};
pub use metadata_index::{MetadataIndexKind, MetadataIndexSpec};
pub use planner::{PlannedSearch, PlannerPolicy};
pub use quantize::{AnnOptions, Quantization};
pub use rebuild::{
    HnswBufferCapacity, HnswMaintenancePolicy, HnswMaintenanceReport, HnswMaintenanceStatus,
    HnswPressure, HnswRebuildLimits, HnswRebuildProgress, HnswRebuildStatus,
    MAX_HNSW_RETAINED_RECORD_BYTES,
};
pub use records::{
    BrowseHit, BrowsePage, FacetCount, GeoSearchHit, GeoSearchQuery, GeoSort, NumericStats,
    ReadSnapshot, RecommendationQuery, SearchGroup, SnapshotRetentionStats,
};
pub use stats::{PlanStrategy, QueryPlan, SearchStats};
pub mod maintenance;
#[cfg(feature = "server")]
pub mod server;

use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::fmt;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use collection::Collection;

pub const MAX_DIMENSIONS: usize = 65_536;
pub const MAX_PAYLOAD_BYTES: usize = 1_048_576;
pub const MAX_BATCH_OPERATIONS: usize = 4_096;
/// Maximum encoded batch body, including per-operation metadata.
pub const MAX_BATCH_BYTES: usize = 16_777_216;
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn current_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(target_arch = "wasm32")]
static WASM_CLOCK_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// WebAssembly has no system clock; the host (for example `Date.now()`) sets
/// the current Unix-millisecond time used for TTL expiry and commit timestamps.
#[cfg(target_arch = "wasm32")]
pub fn set_wasm_clock_ms(now_ms: u64) {
    WASM_CLOCK_MS.store(now_ms, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn current_time_ms() -> u64 {
    WASM_CLOCK_MS.load(std::sync::atomic::Ordering::Relaxed)
}

fn vector_norm(vector: &[f32]) -> f64 {
    vector
        .iter()
        .map(|&value| f64::from(value).powi(2))
        .sum::<f64>()
        .sqrt()
}

fn validate_ann_metric(metric: DistanceMetric) -> Result<()> {
    if metric == DistanceMetric::InnerProduct {
        Err(Error::UnsupportedAnnMetric)
    } else {
        Ok(())
    }
}

fn validate_metric_query(query: &[f32], metric: DistanceMetric) -> Result<f64> {
    let norm = if metric == DistanceMetric::Cosine {
        vector_norm(query)
    } else {
        0.0
    };
    if metric == DistanceMetric::Cosine && norm == 0.0 {
        Err(Error::ZeroNormQuery)
    } else {
        Ok(norm)
    }
}

fn metric_distance(
    a: &[f32],
    b: &[f32],
    metric: DistanceMetric,
    a_norm: f64,
    b_norm: f64,
) -> Option<f64> {
    if metric == DistanceMetric::SquaredL2 {
        return Some(
            a.iter()
                .zip(b)
                .map(|(&a, &b)| {
                    let difference = f64::from(a) - f64::from(b);
                    difference * difference
                })
                .sum(),
        );
    }
    let dot: f64 = a
        .iter()
        .zip(b)
        .map(|(&a, &b)| f64::from(a) * f64::from(b))
        .sum();
    if metric == DistanceMetric::InnerProduct {
        return Some(if dot == 0.0 { 0.0 } else { -dot });
    }
    if a_norm == 0.0 || b_norm == 0.0 {
        None
    } else {
        Some(1.0 - (dot / (a_norm * b_norm)).clamp(-1.0, 1.0))
    }
}

#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    Io(io::Error),
    Locked,
    InvalidDimensions(usize),
    DimensionMismatch {
        expected: usize,
        actual: usize,
    },
    NonFiniteVector,
    ZeroNormQuery,
    PayloadTooLarge(usize),
    BatchTooLarge,
    BatchRequiresUpgrade,
    InvalidHnswConfig,
    UnsupportedAnnMetric,
    InvalidEfSearch,
    IndexNotReady,
    InvalidHnswRebuildLimits,
    HnswRebuildTooLarge,
    HnswRebuildBusy,
    HnswRebuildFailed,
    InvalidHnswMaintenancePolicy,
    InvalidMetadata(&'static str),
    InvalidFilter(&'static str),
    MetadataRequiresUpgrade,
    ExpiryRequiresUpgrade,
    UnsupportedVersion(u32),
    Corrupt {
        offset: u64,
        reason: &'static str,
    },
    IncompleteTail {
        offset: u64,
        bytes: u64,
    },
    CommitIndeterminate(io::Error),
    RepairIndeterminate(io::Error),
    PublicationIndeterminate {
        destination: PathBuf,
        source: io::Error,
    },
    Poisoned,
    SequenceExhausted,
    /// Named collections need format v4; upgrade a closed copy offline.
    CollectionsRequireUpgrade,
    CollectionNotFound,
    CollectionExists,
    InvalidCollectionName,
    /// Live-collection limit (64 including `default`) reached, or the
    /// lineage's never-reused collection ID space (1..=65535) is exhausted.
    TooManyCollections,
    /// Drop refused: live records or open import jobs.
    CollectionNotEmpty,
    DefaultCollectionImmutable,
    InvalidCollectionSettings(&'static str),
    /// A compare-and-set precondition failed; nothing was written.
    RevisionMismatch {
        current: Option<u64>,
    },
    ImportsRequireUpgrade,
    ImportConflict(&'static str),
    ImportNotFound,
    TooManyImports,
    InvalidQuery(&'static str),
    /// The file is encrypted and no key was supplied.
    EncryptionKeyRequired,
    /// The supplied key does not match the file; `key_id` is the file's key ID.
    EncryptionKeyMismatch {
        key_id: u64,
    },
    /// A key was supplied but the file is not encrypted.
    NotEncrypted,
    InvalidEncryptionOptions(&'static str),
    /// The header declares optional features this build does not understand.
    UnsupportedFeature(u16),
    /// A derived sidecar cannot be written for this database (for example an
    /// encrypted one, whose sidecars would contain plaintext vectors).
    SidecarUnavailable(&'static str),
    ChangeFeedRequiresUpgrade,
    /// The cursor belongs to an earlier file lineage (the log was compacted,
    /// restored or replaced); take a fresh snapshot and restart the feed.
    CursorCompacted {
        cursor_lineage: u64,
        current_lineage: u64,
    },
    /// The cursor is past the end of the log (for example after tail repair).
    CursorAhead {
        cursor: u64,
        head: u64,
    },
    InvalidCursor(&'static str),
    /// A timestamp restore needs a file created with commit timestamps.
    TimestampsUnavailable,
    InvalidRestoreTarget(&'static str),
    /// Writes are refused by a read-only replica follower.
    ReadOnlyReplica,
    /// The handle was opened with `open_read_only`; commits are refused
    /// before any byte is written, so the handle stays healthy.
    ReadOnly,
    /// A shipped log segment does not continue this follower's log.
    ReplicaDiverged(&'static str),
    /// A disk index was built for a different committed state.
    StaleIndex,
    /// The GPU path failed; callers may fall back to the CPU search.
    GpuUnavailable(&'static str),
    /// The file was not created with `create_with_graph`.
    #[cfg(feature = "graph")]
    GraphsNotEnabled,
    /// Graph-enabled files need format V5.
    #[cfg(feature = "graph")]
    GraphsRequireV5,
    #[cfg(feature = "graph")]
    InvalidGraphName,
    #[cfg(feature = "graph")]
    GraphExists,
    #[cfg(feature = "graph")]
    GraphNotFound,
    /// Drop refused: the graph still has nodes or edges.
    #[cfg(feature = "graph")]
    GraphNotEmpty,
    /// At most 64 graphs may be live at once.
    #[cfg(feature = "graph")]
    TooManyGraphs,
    /// `create_node` on a key that already names a live node.
    #[cfg(feature = "graph")]
    GraphKeyExists,
    /// An edge key exists with a different source, relationship type,
    /// destination or properties (REQ-009).
    #[cfg(feature = "graph")]
    GraphKeyConflict,
    /// No live node has the given key.
    #[cfg(feature = "graph")]
    GraphNodeNotFound,
    /// No live edge has the given ID or key.
    #[cfg(feature = "graph")]
    GraphEdgeNotFound,
    /// Node delete refused: `incident` edges still reference the node.
    #[cfg(feature = "graph")]
    GraphNodeHasEdges {
        incident: usize,
    },
    /// The node already has an attachment in this collection, or the record
    /// is already attached to a node.
    #[cfg(feature = "graph")]
    GraphAttachmentConflict,
    /// The node has no attachment in this collection.
    #[cfg(feature = "graph")]
    GraphAttachmentNotFound,
    /// Only live records without an expiry can be attached.
    #[cfg(feature = "graph")]
    GraphRecordNotAttachable,
    /// Delete refused: the record is attached to a graph node; detach it first.
    #[cfg(feature = "graph")]
    GraphAttachedRecord,
    /// Expiry refused: attached records cannot expire.
    #[cfg(feature = "graph")]
    GraphAttachedRecordExpiry,
    /// A graph read limit is above its ceiling; limits are never clamped.
    #[cfg(feature = "graph")]
    GraphLimitExceedsCeiling {
        limit: &'static str,
        requested: u64,
        ceiling: u64,
    },
    /// The graph changed after the page cursor was issued; restart the read.
    #[cfg(feature = "graph")]
    GraphCursorStale,
    /// Change feeds do not cover graph-enabled files (REQ-053).
    #[cfg(feature = "graph")]
    GraphChangeFeedUnsupported,
    /// The maintenance operation does not accept graph-enabled files
    /// (LEG-010: `maintenance::upgrade`); use `maintenance::strip_graph` or
    /// `maintenance::enable_graph` instead.
    #[cfg(feature = "graph")]
    GraphUnsupportedOperation,
    /// Invalid graph maintenance options, such as `strip_graph` without
    /// `confirm_graph_data_loss` or `enable_graph` on an unsuitable source.
    #[cfg(feature = "graph")]
    GraphInvalidOptions(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "database I/O error: {error}"),
            Self::Locked => write!(f, "database is already locked"),
            Self::InvalidDimensions(n) => {
                write!(f, "dimensions {n} must be between 1 and {MAX_DIMENSIONS}")
            }
            Self::DimensionMismatch { expected, actual } => {
                write!(
                    f,
                    "expected {expected} vector dimensions, received {actual}"
                )
            }
            Self::NonFiniteVector => write!(f, "vector components must be finite"),
            Self::ZeroNormQuery => write!(f, "cosine search requires a nonzero query vector"),
            Self::PayloadTooLarge(n) => {
                write!(f, "payload is {n} bytes; maximum is {MAX_PAYLOAD_BYTES}")
            }
            Self::BatchTooLarge => write!(
                f,
                "batch exceeds {MAX_BATCH_OPERATIONS} operations or {MAX_BATCH_BYTES} encoded body bytes"
            ),
            Self::BatchRequiresUpgrade => write!(
                f,
                "atomic batches require format v2; compact the closed v1 database to a new path to upgrade"
            ),
            Self::InvalidHnswConfig => write!(
                f,
                "HNSW m must be 2..=64 and ef_construction must be m..=4096"
            ),
            Self::UnsupportedAnnMetric => {
                write!(f, "HNSW supports squared_l2 and cosine, not inner_product")
            }
            Self::InvalidEfSearch => write!(
                f,
                "ef_search must be 1..={MAX_EF_SEARCH} and at least min(k, live records)"
            ),
            Self::IndexNotReady => write!(
                f,
                "HNSW index is not ready for this configuration; call build_hnsw explicitly"
            ),
            Self::InvalidHnswRebuildLimits => write!(
                f,
                "background rebuild limits require 1..=1000000 records and 1..=268435456 snapshot vector bytes"
            ),
            Self::HnswRebuildTooLarge => write!(
                f,
                "background HNSW snapshot exceeds configured record/vector-byte limits or the retained-record-body limit"
            ),
            Self::HnswRebuildBusy => write!(
                f,
                "a background HNSW rebuild is pending; poll it to completion before starting another"
            ),
            Self::HnswRebuildFailed => write!(
                f,
                "background HNSW worker failed; current index is unchanged"
            ),
            Self::InvalidHnswMaintenancePolicy => write!(
                f,
                "HNSW maintenance high-water thresholds must be 1..=256 IDs and 1..=8388608 vector bytes"
            ),
            Self::InvalidMetadata(reason) => write!(f, "invalid metadata: {reason}"),
            Self::InvalidFilter(reason) => write!(f, "invalid filter: {reason}"),
            Self::MetadataRequiresUpgrade => write!(
                f,
                "metadata requires format v3; compact the closed database to a new path"
            ),
            Self::ExpiryRequiresUpgrade => write!(
                f,
                "record expiry and metadata-only updates require opt-in format v5"
            ),
            Self::UnsupportedVersion(version) => write!(f, "unsupported format version {version}"),
            Self::Corrupt { offset, reason } => {
                write!(f, "database corruption at byte {offset}: {reason}")
            }
            Self::IncompleteTail { offset, bytes } => write!(
                f,
                "incomplete trailing write at byte {offset} ({bytes} bytes); \
                 explicit repair is required and may discard a truncated commit"
            ),
            Self::CommitIndeterminate(error) => write!(
                f,
                "commit outcome is indeterminate: {error}; handle poisoned, close and reopen"
            ),
            Self::RepairIndeterminate(error) => write!(
                f,
                "tail repair outcome is indeterminate: {error}; inspect the file before retrying"
            ),
            Self::PublicationIndeterminate {
                destination,
                source,
            } => write!(
                f,
                "output published at {}, but directory sync failed: {source}; \
                 inspect that path before retrying",
                destination.display()
            ),
            Self::Poisoned => write!(f, "handle poisoned by failed commit; close and reopen"),
            Self::SequenceExhausted => write!(f, "commit sequence exhausted"),
            Self::CollectionsRequireUpgrade => write!(
                f,
                "named collections require format v4; upgrade a closed copy to a new path"
            ),
            Self::CollectionNotFound => write!(f, "collection not found"),
            Self::CollectionExists => write!(f, "collection already exists"),
            Self::InvalidCollectionName => {
                write!(f, "collection names must match ^[a-z][a-z0-9_-]{{0,62}}$")
            }
            Self::TooManyCollections => write!(
                f,
                "at most 64 live collections, including default, and at most 65535 collection IDs per lineage"
            ),
            Self::CollectionNotEmpty => write!(
                f,
                "collection has live records or open import jobs; empty it first"
            ),
            Self::DefaultCollectionImmutable => {
                write!(f, "the default collection cannot be created or dropped")
            }
            Self::InvalidCollectionSettings(reason) => {
                write!(f, "invalid collection settings: {reason}")
            }
            Self::RevisionMismatch {
                current: Some(revision),
            } => write!(
                f,
                "revision precondition failed; current revision is {revision}"
            ),
            Self::RevisionMismatch { current: None } => {
                write!(f, "revision precondition failed; record does not exist")
            }
            Self::ImportsRequireUpgrade => write!(
                f,
                "import jobs require format v4; upgrade a closed copy to a new path"
            ),
            Self::ImportConflict(reason) => write!(f, "import conflict: {reason}"),
            Self::ImportNotFound => write!(f, "import job not found"),
            Self::TooManyImports => write!(f, "at most 64 import jobs may be open"),
            Self::InvalidQuery(reason) => write!(f, "invalid query: {reason}"),
            Self::EncryptionKeyRequired => {
                write!(f, "database is encrypted; an encryption key is required")
            }
            Self::EncryptionKeyMismatch { key_id } => write!(
                f,
                "encryption key does not match this database (file key ID {key_id:016x})"
            ),
            Self::NotEncrypted => write!(f, "database is not encrypted"),
            Self::InvalidEncryptionOptions(reason) => {
                write!(f, "invalid encryption options: {reason}")
            }
            Self::UnsupportedFeature(flags) => write!(
                f,
                "database header declares unsupported feature flags {flags:#06x}"
            ),
            Self::SidecarUnavailable(reason) => write!(f, "sidecar unavailable: {reason}"),
            Self::ChangeFeedRequiresUpgrade => {
                write!(f, "the change feed requires format v4 or newer")
            }
            Self::CursorCompacted {
                cursor_lineage,
                current_lineage,
            } => write!(
                f,
                "cursor compacted away: cursor lineage {cursor_lineage:016x} is not the current lineage {current_lineage:016x}; take a new snapshot and restart the feed"
            ),
            Self::CursorAhead { cursor, head } => write!(
                f,
                "cursor sequence {cursor} is past the end of the log ({head}); the log was truncated or replaced"
            ),
            Self::InvalidCursor(reason) => write!(f, "invalid cursor: {reason}"),
            Self::TimestampsUnavailable => write!(
                f,
                "this database was not created with commit timestamps; restore by sequence instead"
            ),
            Self::InvalidRestoreTarget(reason) => write!(f, "invalid restore target: {reason}"),
            Self::ReadOnlyReplica => write!(f, "read replica followers refuse writes"),
            Self::ReadOnly => write!(f, "database was opened read-only"),
            Self::ReplicaDiverged(reason) => write!(f, "replica log diverged: {reason}"),
            Self::StaleIndex => write!(
                f,
                "disk index was built for a different committed state; rebuild it"
            ),
            Self::GpuUnavailable(reason) => write!(f, "GPU search unavailable: {reason}"),
            #[cfg(feature = "graph")]
            Self::GraphsNotEnabled => write!(
                f,
                "this database is not graph-enabled; create it with create_with_graph"
            ),
            #[cfg(feature = "graph")]
            Self::GraphsRequireV5 => write!(f, "graph-enabled databases require format V5"),
            #[cfg(feature = "graph")]
            Self::InvalidGraphName => {
                write!(f, "graph names must match ^[a-z][a-z0-9_-]{{0,62}}$")
            }
            #[cfg(feature = "graph")]
            Self::GraphExists => write!(f, "graph already exists"),
            #[cfg(feature = "graph")]
            Self::GraphNotFound => write!(f, "graph not found"),
            #[cfg(feature = "graph")]
            Self::GraphNotEmpty => write!(f, "graph has nodes or edges; empty it first"),
            #[cfg(feature = "graph")]
            Self::TooManyGraphs => write!(f, "at most 64 graphs may be live at once"),
            #[cfg(feature = "graph")]
            Self::GraphKeyExists => write!(f, "graph node key already exists"),
            #[cfg(feature = "graph")]
            Self::GraphKeyConflict => write!(
                f,
                "graph edge key already exists with different endpoints, type or properties"
            ),
            #[cfg(feature = "graph")]
            Self::GraphNodeNotFound => write!(f, "graph node not found"),
            #[cfg(feature = "graph")]
            Self::GraphEdgeNotFound => write!(f, "graph edge not found"),
            #[cfg(feature = "graph")]
            Self::GraphNodeHasEdges { incident } => write!(
                f,
                "graph node has {incident} incident edges; delete them first"
            ),
            #[cfg(feature = "graph")]
            Self::GraphAttachmentConflict => write!(
                f,
                "the node already has an attachment in this collection, or the record is attached elsewhere"
            ),
            #[cfg(feature = "graph")]
            Self::GraphAttachmentNotFound => {
                write!(f, "the node has no attachment in this collection")
            }
            #[cfg(feature = "graph")]
            Self::GraphRecordNotAttachable => {
                write!(f, "only live records without an expiry can be attached")
            }
            #[cfg(feature = "graph")]
            Self::GraphAttachedRecord => write!(
                f,
                "record is attached to a graph node; detach it before deleting"
            ),
            #[cfg(feature = "graph")]
            Self::GraphAttachedRecordExpiry => {
                write!(f, "records attached to a graph node cannot expire")
            }
            #[cfg(feature = "graph")]
            Self::GraphLimitExceedsCeiling {
                limit,
                requested,
                ceiling,
            } => write!(
                f,
                "graph limit {limit} = {requested} exceeds its ceiling {ceiling}"
            ),
            #[cfg(feature = "graph")]
            Self::GraphCursorStale => write!(
                f,
                "the graph changed after the cursor was issued; restart the read"
            ),
            #[cfg(feature = "graph")]
            Self::GraphChangeFeedUnsupported => {
                write!(f, "change feeds are not supported on graph-enabled files")
            }
            #[cfg(feature = "graph")]
            Self::GraphUnsupportedOperation => write!(
                f,
                "operation not supported on graph-enabled files; use maintenance::strip_graph or maintenance::enable_graph"
            ),
            #[cfg(feature = "graph")]
            Self::GraphInvalidOptions(reason) => write!(f, "invalid graph options: {reason}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error)
            | Self::CommitIndeterminate(error)
            | Self::RepairIndeterminate(error) => Some(error),
            Self::PublicationIndeterminate { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub vector: Vec<f32>,
    pub payload: Vec<u8>,
    pub metadata: Metadata,
}

/// One operation in an atomic batch. Operations execute in slice order.
#[derive(Clone, Debug, PartialEq)]
pub enum Mutation {
    PutWithMetadata {
        id: u64,
        vector: Vec<f32>,
        payload: Vec<u8>,
        metadata: Metadata,
    },
    Put {
        id: u64,
        vector: Vec<f32>,
        payload: Vec<u8>,
    },
    /// A missing ID is a no-op, but remains part of the batch commit.
    Delete { id: u64 },
}

#[derive(Clone, Debug, PartialEq)]
pub struct SearchHit {
    pub id: u64,
    /// Metric distance accumulated in f64. Smaller is nearer.
    pub distance: f64,
}

/// Exact search metric. Storage and HNSW retain their existing L2 contracts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "json", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "json", serde(rename_all = "snake_case"))]
pub enum DistanceMetric {
    #[default]
    SquaredL2,
    Cosine,
    InnerProduct,
}

struct RankedHit(SearchHit);

impl PartialEq for RankedHit {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for RankedHit {}

impl PartialOrd for RankedHit {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RankedHit {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0
            .distance
            .total_cmp(&other.0.distance)
            .then_with(|| self.0.id.cmp(&other.0.id))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepairReport {
    pub discarded_bytes: u64,
    pub retained_commits: u64,
}

/// On-disk format selected by `Database::create_with_format`.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatVersion {
    /// The default for `Database::create`; readable by older VLite releases.
    V3,
    /// Adds lineage, named collections, persisted settings and import
    /// checkpoints. Not readable by releases that predate it.
    V4,
    /// Adds record expiry, metadata-only updates, geospatial metadata and
    /// named/sparse vector histories with re-embedding checkpoints.
    /// Not readable by releases that predate it.
    V5,
}

/// Compare-and-set precondition on a record's current revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expected {
    /// No precondition.
    Any,
    /// The ID must not currently exist.
    Absent,
    /// The record must exist with exactly this revision.
    Revision(u64),
}

impl Expected {
    fn check(self, current: Option<u64>) -> Result<()> {
        let matches = match self {
            Self::Any => true,
            Self::Absent => current.is_none(),
            Self::Revision(revision) => current == Some(revision),
        };
        if matches {
            Ok(())
        } else {
            Err(Error::RevisionMismatch { current })
        }
    }
}

/// One exclusive database owner. All operations are serialized by the caller.
///
/// Reads use a rebuilt in-memory map. The datafile is the only source of truth:
/// in-memory HNSW indexes are derived, and the only optional persisted copy is
/// an explicitly saved, fully revalidated ANN snapshot sidecar (see
/// save_ann_snapshot). No lock sidecar exists.
pub struct Database {
    file: LockedFile,
    version: u32,
    lineage: Option<u64>,
    sequence: u64,
    end: u64,
    poisoned: bool,
    /// Internal server-only durability scope; never exposed to embedded callers.
    group_active: bool,
    snapshot_readers: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    snapshot_retained_bytes: std::sync::Arc<std::sync::atomic::AtomicU64>,
    #[cfg(test)]
    fail_next_group_sync: bool,
    /// Live collections by ID; ID 0 (`default`) is always present.
    collections: BTreeMap<u16, Collection>,
    /// At most one background rebuild per database, owned by one collection.
    rebuild: Option<(u16, rebuild::Job)>,
    imports: import::Registry,
    /// Highest collection ID ever assigned in this lineage history; new
    /// collections get the next ID, so IDs are never reused.
    collection_high: u16,
    flags: u16,
    cipher: Option<crypto::Cipher>,
    /// `(sequence, ms)` per commit for files created with commit timestamps.
    commit_times: Vec<(u64, u64)>,
    clock: Option<CommitClock>,
    /// Opened with `open_read_only`: shared OS lock, read-only file handle.
    read_only: bool,
    /// Named graphs; `Some` exactly for graph-enabled (`FLAG_GRAPH`) files.
    #[cfg(feature = "graph")]
    graph: Option<property_graph::engine::GraphState>,
}

/// Source of commit timestamps (milliseconds since the Unix epoch).
pub type CommitClock = Box<dyn FnMut() -> u64 + Send + Sync>;

/// Optional features for `Database::create_with_options`.
#[derive(Debug, Default)]
pub struct CreateOptions {
    /// Defaults to V3 unless a feature below requires V5.
    pub format: Option<FormatVersion>,
    /// Encrypt frame bodies at rest (requires, and implies, format V5).
    pub encryption: Option<EncryptionKey>,
    /// Record a wall-clock timestamp per commit so the file supports
    /// timestamp point-in-time restore (requires, and implies, format V5).
    pub commit_timestamps: bool,
}

enum Backing {
    Disk(File),
    Memory(storage::MemFile),
}

struct LockedFile(Backing);

/// Minimal stand-in for `fs::Metadata` that works for every backing.
#[allow(dead_code)]
struct BackingLen(u64);

#[allow(dead_code)]
impl BackingLen {
    fn len(&self) -> u64 {
        self.0
    }
}

#[allow(dead_code)]
impl Backing {
    fn metadata(&self) -> io::Result<BackingLen> {
        match self {
            Backing::Disk(file) => Ok(BackingLen(file.metadata()?.len())),
            Backing::Memory(memory) => Ok(BackingLen(memory.len())),
        }
    }
}

impl io::Read for Backing {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Backing::Disk(file) => file.read(buf),
            Backing::Memory(memory) => memory.read(buf),
        }
    }
}

impl Seek for Backing {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        match self {
            Backing::Disk(file) => file.seek(position),
            Backing::Memory(memory) => memory.seek(position),
        }
    }
}

impl LockedFile {
    fn disk(file: File) -> Self {
        Self(Backing::Disk(file))
    }

    fn memory(bytes: Vec<u8>) -> Self {
        Self(Backing::Memory(storage::MemFile::from_bytes(bytes)))
    }

    fn sync_all(&self) -> io::Result<()> {
        match &self.0 {
            Backing::Disk(file) => file.sync_all(),
            Backing::Memory(_) => Ok(()),
        }
    }

    fn set_len(&self, length: u64) -> io::Result<()> {
        match &self.0 {
            Backing::Disk(file) => file.set_len(length),
            Backing::Memory(memory) => memory.set_len(length),
        }
    }

    #[cfg(test)]
    fn len(&self) -> io::Result<u64> {
        match &self.0 {
            Backing::Disk(file) => Ok(file.metadata()?.len()),
            Backing::Memory(memory) => Ok(memory.len()),
        }
    }

    fn unlock(&self) -> io::Result<()> {
        match &self.0 {
            Backing::Disk(file) => file.unlock(),
            Backing::Memory(_) => Ok(()),
        }
    }

    #[cfg(test)]
    fn try_clone(&self) -> io::Result<File> {
        match &self.0 {
            Backing::Disk(file) => file.try_clone(),
            Backing::Memory(_) => Err(io::Error::other("in-memory storage")),
        }
    }

    fn memory_bytes(&self) -> Option<Vec<u8>> {
        match &self.0 {
            Backing::Disk(_) => None,
            Backing::Memory(memory) => Some(memory.to_bytes()),
        }
    }
}

impl io::Read for &LockedFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &self.0 {
            Backing::Disk(file) => (&*file).read(buf),
            Backing::Memory(memory) => memory.read(buf),
        }
    }
}

impl io::Read for LockedFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&*self).read(buf)
    }
}

impl Seek for &LockedFile {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        match &self.0 {
            Backing::Disk(file) => (&*file).seek(position),
            Backing::Memory(memory) => memory.seek(position),
        }
    }
}

impl Seek for LockedFile {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        (&*self).seek(position)
    }
}

impl Write for LockedFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &self.0 {
            Backing::Disk(file) => (&*file).write(buf),
            Backing::Memory(memory) => memory.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for LockedFile {
    fn drop(&mut self) {
        // Explicit unlock also releases Unix locks briefly inherited by a
        // concurrent fork/exec. File close alone can leave those locks alive.
        // Destructors cannot return errors; close() reports unlock failures.
        let _ = self.unlock();
    }
}

fn lock(file: File) -> Result<LockedFile> {
    match file.try_lock() {
        Ok(()) => Ok(LockedFile::disk(file)),
        Err(TryLockError::WouldBlock) => Err(Error::Locked),
        Err(TryLockError::Error(error)) => Err(Error::Io(error)),
    }
}

fn open_locked(path: &Path) -> Result<LockedFile> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    lock(file)
}

/// Read-only handle under a shared OS lock: any number of shared holders
/// coexist (across processes too), but never alongside an exclusive writer.
fn open_shared(path: &Path) -> Result<LockedFile> {
    let file = OpenOptions::new().read(true).open(path)?;
    match file.try_lock_shared() {
        Ok(()) => Ok(LockedFile::disk(file)),
        Err(TryLockError::WouldBlock) => Err(Error::Locked),
        Err(TryLockError::Error(error)) => Err(Error::Io(error)),
    }
}

fn validate_dimensions(dimensions: usize) -> Result<()> {
    if !(1..=MAX_DIMENSIONS).contains(&dimensions) {
        return Err(Error::InvalidDimensions(dimensions));
    }
    Ok(())
}

fn validate_vector(dimensions: usize, vector: &[f32]) -> Result<()> {
    if vector.len() != dimensions {
        return Err(Error::DimensionMismatch {
            expected: dimensions,
            actual: vector.len(),
        });
    }
    if vector.iter().any(|value| !value.is_finite()) {
        return Err(Error::NonFiniteVector);
    }
    Ok(())
}

fn validate_settings(settings: &CollectionSettings) -> Result<()> {
    settings
        .validate()
        .map_err(Error::InvalidCollectionSettings)
}

impl Database {
    /// Creates a new format-v3 database without overwriting any existing path.
    ///
    /// The file contents are synced before returning. The caller is responsible
    /// for platform-specific parent-directory durability after creation.
    pub fn create(path: impl AsRef<Path>, dimensions: usize) -> Result<Self> {
        Self::create_with_format(path, dimensions, FormatVersion::V3)
    }

    /// Creates a new database in an explicit format. `dimensions` applies to
    /// the `default` collection. V4/V5 draw a random nonzero lineage from the OS.
    pub fn create_with_format(
        path: impl AsRef<Path>,
        dimensions: usize,
        format: FormatVersion,
    ) -> Result<Self> {
        Self::create_with_options(
            path,
            dimensions,
            CreateOptions {
                format: Some(format),
                ..CreateOptions::default()
            },
        )
    }

    /// Creates a new database with optional encryption and commit timestamps.
    /// Either feature needs format V5 (the default when one is requested; an
    /// explicit older format is rejected).
    pub fn create_with_options(
        path: impl AsRef<Path>,
        dimensions: usize,
        options: CreateOptions,
    ) -> Result<Self> {
        Self::create_file(path.as_ref(), dimensions, options, 0)
    }

    /// `extra_flags` is 0, or `FLAG_GRAPH` from `create_with_graph`.
    fn create_file(
        path: &Path,
        dimensions: usize,
        options: CreateOptions,
        extra_flags: u16,
    ) -> Result<Self> {
        Self::create_backed(dimensions, options, extra_flags, |header| {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(path)?;
            let mut file = lock(file)?;
            file.write_all(header)?;
            file.sync_all()?;
            Ok(file)
        })
    }

    /// Creates a database held entirely in memory (no file, no lock): the
    /// WebAssembly backend. Commits are durable only as far as the caller
    /// persists `to_bytes()`.
    pub fn create_in_memory(dimensions: usize, options: CreateOptions) -> Result<Self> {
        Self::create_backed(dimensions, options, 0, |header| {
            Ok(LockedFile::memory(header.to_vec()))
        })
    }

    /// Opens a database from a complete byte image (as produced by `to_bytes`,
    /// `backup` or a closed file). Corrupt or incomplete images are rejected.
    pub fn open_from_bytes(bytes: Vec<u8>, key: Option<&EncryptionKey>) -> Result<Self> {
        Self::open_backed(LockedFile::memory(bytes), key)
    }

    /// The full committed byte image of an in-memory database, or `None` for a
    /// file-backed one. Opening it with `open_from_bytes` reproduces the state.
    pub fn to_bytes(&self) -> Option<Vec<u8>> {
        self.file.memory_bytes()
    }

    fn create_backed(
        dimensions: usize,
        options: CreateOptions,
        extra_flags: u16,
        make: impl FnOnce(&[u8]) -> Result<LockedFile>,
    ) -> Result<Self> {
        validate_dimensions(dimensions)?;
        let featured =
            options.encryption.is_some() || options.commit_timestamps || extra_flags != 0;
        let format = match (options.format, featured) {
            (None, false) => FormatVersion::V3,
            (None, true) => FormatVersion::V5,
            (Some(FormatVersion::V5), _) => FormatVersion::V5,
            (Some(_), true) => {
                return Err(Error::InvalidEncryptionOptions(
                    "encryption and commit timestamps require format V5",
                ));
            }
            (Some(format), false) => format,
        };
        let mut flags = 0u16;
        let mut cipher = None;
        if let Some(key) = &options.encryption {
            flags |= format::FLAG_ENCRYPTED;
            cipher = Some(crypto::Cipher::create(key)?);
        }
        if options.commit_timestamps {
            flags |= format::FLAG_TIMESTAMPS;
        }
        flags |= extra_flags;
        let (version, lineage, header) = match format {
            FormatVersion::V3 => (format::CURRENT_VERSION, None, format::header(dimensions)),
            FormatVersion::V4 => {
                let lineage = format::new_lineage()?;
                (
                    format::V4,
                    Some(lineage),
                    format::header_v4(dimensions, lineage, 0),
                )
            }
            FormatVersion::V5 => {
                let lineage = format::new_lineage()?;
                (
                    format::V5,
                    Some(lineage),
                    format::header_v5_flags(dimensions, lineage, 0, flags, cipher.as_ref()),
                )
            }
        };
        let file = make(&header)?;
        Ok(Self {
            file,
            version,
            lineage,
            sequence: 0,
            end: header.len() as u64,
            poisoned: false,
            group_active: false,
            snapshot_readers: Default::default(),
            snapshot_retained_bytes: Default::default(),
            #[cfg(test)]
            fail_next_group_sync: false,
            collections: BTreeMap::from([(
                0,
                Collection::empty("default".into(), dimensions, CollectionSettings::default()),
            )]),
            rebuild: None,
            imports: import::Registry::default(),
            collection_high: 0,
            flags,
            cipher,
            commit_times: Vec::new(),
            clock: None,
            read_only: false,
            #[cfg(feature = "graph")]
            graph: (flags & format::FLAG_GRAPH != 0).then(property_graph::engine::GraphState::new),
        })
    }

    /// Opens an existing database, validating every record without modifying it.
    ///
    /// Corrupt files and incomplete trailing writes are rejected, not truncated.
    /// Encrypted files need `open_encrypted`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_inner(path.as_ref(), None)
    }

    /// Opens an encrypted database. A wrong key is `EncryptionKeyMismatch`
    /// (never garbage data); an unencrypted file is `NotEncrypted`.
    pub fn open_encrypted(path: impl AsRef<Path>, key: &EncryptionKey) -> Result<Self> {
        Self::open_inner(path.as_ref(), Some(key))
    }

    fn open_inner(path: &Path, key: Option<&EncryptionKey>) -> Result<Self> {
        Self::open_backed(open_locked(path)?, key)
    }

    /// Opens an existing database for reading only, under a shared OS lock.
    ///
    /// Any number of read-only handles may be open at once, in this process
    /// or in others, and each serves concurrent `&self` reads from multiple
    /// threads. While at least one is open, `open` (the exclusive writer)
    /// returns `Locked`; while a writer is open, this returns `Locked`. Each
    /// handle sees the committed state at open time; reopen to observe later
    /// commits. Validation matches `open`: an incomplete tail is rejected and
    /// never repaired. Commits return `ReadOnly` without touching the file.
    /// In-memory ANN indexes may still be built; the datafile is never written.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_read_only_inner(path.as_ref(), None)
    }

    /// Encrypted variant of `open_read_only`.
    pub fn open_read_only_encrypted(path: impl AsRef<Path>, key: &EncryptionKey) -> Result<Self> {
        Self::open_read_only_inner(path.as_ref(), Some(key))
    }

    fn open_read_only_inner(path: &Path, key: Option<&EncryptionKey>) -> Result<Self> {
        let mut database = Self::open_backed(open_shared(path)?, key)?;
        database.read_only = true;
        Ok(database)
    }

    /// Whether this handle came from `open_read_only`.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn open_backed(mut file: LockedFile, key: Option<&EncryptionKey>) -> Result<Self> {
        let loaded = format::load_with(&mut file, key)?;
        if loaded.incomplete_bytes != 0 {
            return Err(Error::IncompleteTail {
                offset: loaded.end,
                bytes: loaded.incomplete_bytes,
            });
        }
        let imports = import::Registry::from_markers(&loaded.imports);
        Ok(Self {
            file,
            version: loaded.version,
            lineage: loaded.lineage,
            sequence: loaded.sequence,
            end: loaded.end,
            poisoned: false,
            group_active: false,
            snapshot_readers: Default::default(),
            snapshot_retained_bytes: Default::default(),
            #[cfg(test)]
            fail_next_group_sync: false,
            collections: loaded
                .collections
                .into_iter()
                .map(|(id, collection)| (id, Collection::from_loaded(collection)))
                .collect(),
            rebuild: None,
            imports,
            collection_high: loaded.collection_high,
            flags: loaded.flags,
            cipher: loaded.cipher,
            commit_times: loaded.commit_times,
            clock: None,
            read_only: false,
            #[cfg(feature = "graph")]
            graph: loaded.graph,
        })
    }

    /// Destructively discards only a structurally incomplete trailing frame.
    ///
    /// Back up the closed file first. Truncation of an acknowledged last commit
    /// cannot be distinguished from an interrupted write. Calling this method
    /// explicitly accepts that potential data loss. Complete corrupt frames
    /// are never repaired. A failed repair sync has an indeterminate outcome.
    /// Encrypted files need `discard_incomplete_tail_encrypted`.
    pub fn discard_incomplete_tail(path: impl AsRef<Path>) -> Result<RepairReport> {
        Self::discard_tail_inner(path.as_ref(), None)
    }

    pub fn discard_incomplete_tail_encrypted(
        path: impl AsRef<Path>,
        key: &EncryptionKey,
    ) -> Result<RepairReport> {
        Self::discard_tail_inner(path.as_ref(), Some(key))
    }

    fn discard_tail_inner(path: &Path, key: Option<&EncryptionKey>) -> Result<RepairReport> {
        let mut file = open_locked(path)?;
        let loaded = format::load_with(&mut file, key)?;
        let report = RepairReport {
            discarded_bytes: loaded.incomplete_bytes,
            retained_commits: loaded.sequence,
        };
        if loaded.incomplete_bytes != 0 {
            file.set_len(loaded.end)
                .and_then(|()| file.sync_all())
                .map_err(Error::RepairIndeterminate)?;
        }
        Ok(report)
    }

    /// Dimensions of the `default` collection.
    pub fn dimensions(&self) -> usize {
        self.default_state().dimensions
    }

    pub fn format_version(&self) -> u32 {
        self.version
    }

    /// Random nonzero file-family identity of a v4 database; None for v1–v3.
    /// Compaction, upgrade and restore assign a new lineage.
    pub fn lineage(&self) -> Option<u64> {
        self.lineage
    }

    /// Live records in the `default` collection.
    pub fn len(&self) -> usize {
        self.default_state().records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.default_state().records.is_empty()
    }

    pub(crate) fn default_state(&self) -> &Collection {
        self.state(0)
    }

    /// Collection handles guarantee that their ID is live.
    fn state(&self, collection: u16) -> &Collection {
        &self.collections[&collection]
    }

    fn state_mut(&mut self, collection: u16) -> &mut Collection {
        self.collections
            .get_mut(&collection)
            .expect("collection handles reference live collections")
    }

    fn ensure_healthy(&self) -> Result<()> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        Ok(())
    }

    fn next_sequence(&self) -> Result<u64> {
        self.sequence.checked_add(1).ok_or(Error::SequenceExhausted)
    }

    /// Returns a record, or None when its ID does not exist.
    pub fn get(&self, id: u64) -> Result<Option<&Record>> {
        self.get_in(0, id)
    }

    fn get_in(&self, collection: u16, id: u64) -> Result<Option<&Record>> {
        self.get_at_in(collection, id, current_time_ms())
    }

    fn get_at_in(&self, collection: u16, id: u64, now_ms: u64) -> Result<Option<&Record>> {
        self.ensure_healthy()?;
        let state = self.state(collection);
        Ok(state.records.get(&id).filter(|_| {
            !state
                .expiries
                .get(&id)
                .is_some_and(|expiry| *expiry <= now_ms)
        }))
    }

    pub fn get_at(&self, id: u64, now_ms: u64) -> Result<Option<&Record>> {
        self.get_at_in(0, id, now_ms)
    }

    /// Current revision of a live record, or None when it does not exist.
    ///
    /// A revision is the commit sequence that last put the record. It is
    /// derived from the log on open, so it survives reopen on every format,
    /// is strictly increasing per ID, and is never reused by a later put.
    /// It changes when a file is compacted, upgraded or restored.
    pub fn revision(&self, id: u64) -> Result<Option<u64>> {
        self.revision_in(0, id)
    }

    fn revision_in(&self, collection: u16, id: u64) -> Result<Option<u64>> {
        self.ensure_healthy()?;
        Ok(self.state(collection).revisions.get(&id).copied())
    }

    /// Durably replaces one record's vector and opaque payload, clearing any metadata.
    pub fn put(&mut self, id: u64, vector: &[f32], payload: &[u8]) -> Result<()> {
        self.put_with_metadata(id, vector, payload, &Metadata::new())
    }

    /// Replaces the entire vector, payload and metadata map. Nonempty metadata requires v3.
    pub fn put_with_metadata(
        &mut self,
        id: u64,
        vector: &[f32],
        payload: &[u8],
        metadata: &Metadata,
    ) -> Result<()> {
        self.put_in(0, id, vector, payload, metadata, Expected::Any)
            .map(|_| ())
    }

    /// Durably puts a record with an optional absolute Unix-millisecond expiry. Requires v5.
    pub fn put_with_expiry(
        &mut self,
        id: u64,
        vector: &[f32],
        payload: &[u8],
        metadata: &Metadata,
        expires_at_ms: Option<u64>,
    ) -> Result<u64> {
        self.put_with_expiry_in(
            0,
            id,
            vector,
            payload,
            metadata,
            expires_at_ms,
            Expected::Any,
        )
    }

    /// Expiry of an existing record, or None for missing/non-expiring records.
    pub fn expires_at(&self, id: u64) -> Result<Option<u64>> {
        self.ensure_healthy()?;
        Ok(self.default_state().expiries.get(&id).copied())
    }

    /// Updates only metadata; vector and payload bytes are not rewritten. Requires v5.
    pub fn update_metadata(&mut self, id: u64, metadata: &Metadata) -> Result<u64> {
        self.update_metadata_in(0, id, metadata, Expected::Any)
    }

    /// Exact search using a caller-supplied Unix-millisecond clock.
    pub fn search_filtered_with_metric_at(
        &self,
        query: &[f32],
        k: usize,
        conditions: &[Condition],
        metric: DistanceMetric,
        now_ms: u64,
    ) -> Result<Vec<SearchHit>> {
        self.search_in_at(
            0,
            query,
            k,
            conditions,
            metric,
            now_ms,
            &mut SearchStats::default(),
        )
    }

    /// Conditional put. Input validation happens first; a failed precondition
    /// then returns `RevisionMismatch { current }` without writing. On success
    /// returns the record's new revision, acknowledged only after sync.
    pub fn put_with_metadata_if(
        &mut self,
        id: u64,
        vector: &[f32],
        payload: &[u8],
        metadata: &Metadata,
        expected: Expected,
    ) -> Result<u64> {
        self.put_in(0, id, vector, payload, metadata, expected)
    }

    #[allow(clippy::too_many_arguments)]
    fn put_with_expiry_in(
        &mut self,
        collection: u16,
        id: u64,
        vector: &[f32],
        payload: &[u8],
        metadata: &Metadata,
        expiry: Option<u64>,
        expected: Expected,
    ) -> Result<u64> {
        self.ensure_healthy()?;
        if self.version < format::V5 {
            return Err(Error::ExpiryRequiresUpgrade);
        }
        let state = self.state(collection);
        validate_vector(state.dimensions, vector)?;
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(Error::PayloadTooLarge(payload.len()));
        }
        metadata::validate(metadata)?;
        if metadata::contains_geo(metadata) && self.version < format::V5 {
            return Err(Error::ExpiryRequiresUpgrade);
        }
        expected.check(state.revisions.get(&id).copied())?;
        #[cfg(feature = "graph")]
        if expiry.is_some() && self.is_attached(collection, id) {
            return Err(Error::GraphAttachedRecordExpiry);
        }
        let record = Record {
            vector: vector.to_vec(),
            payload: payload.to_vec(),
            metadata: metadata.clone(),
        };
        let sequence = self.next_sequence()?;
        let frame = format::record_frame_v5(sequence, collection, id, &record, expiry);
        self.commit_frame(sequence, &frame)?;
        self.apply(collection, id, Some(record), sequence, expiry);
        Ok(sequence)
    }

    fn update_metadata_in(
        &mut self,
        collection: u16,
        id: u64,
        metadata: &Metadata,
        expected: Expected,
    ) -> Result<u64> {
        self.ensure_healthy()?;
        if self.version < format::V5 {
            return Err(Error::ExpiryRequiresUpgrade);
        }
        metadata::validate(metadata)?;
        if metadata::contains_geo(metadata) && self.version < format::V5 {
            return Err(Error::ExpiryRequiresUpgrade);
        }
        let current = self.state(collection).revisions.get(&id).copied();
        expected.check(current)?;
        if current.is_none() {
            return Err(Error::RevisionMismatch { current: None });
        }
        let sequence = self.next_sequence()?;
        let frame = format::metadata_frame_v5(sequence, collection, id, metadata);
        self.commit_frame(sequence, &frame)?;
        self.state_mut(collection)
            .apply_metadata_committed(id, metadata.clone(), sequence);
        #[cfg(feature = "graph")]
        self.touch_graph_record(collection, id, sequence);
        Ok(sequence)
    }

    fn put_in(
        &mut self,
        collection: u16,
        id: u64,
        vector: &[f32],
        payload: &[u8],
        metadata: &Metadata,
        expected: Expected,
    ) -> Result<u64> {
        self.ensure_healthy()?;
        let state = self.state(collection);
        validate_vector(state.dimensions, vector)?;
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(Error::PayloadTooLarge(payload.len()));
        }
        metadata::validate(metadata)?;
        if metadata::contains_geo(metadata) && self.version < format::V5 {
            return Err(Error::ExpiryRequiresUpgrade);
        }
        if !metadata.is_empty() && self.version < 3 {
            return Err(Error::MetadataRequiresUpgrade);
        }
        expected.check(state.revisions.get(&id).copied())?;
        let record = Record {
            vector: vector.to_vec(),
            payload: payload.to_vec(),
            metadata: metadata.clone(),
        };
        let sequence = self.next_sequence()?;
        let frame = format::record_frame(sequence, collection, id, Some(&record));
        self.commit_frame(sequence, &frame)?;
        self.apply(collection, id, Some(record), sequence, None);
        Ok(sequence)
    }

    /// Durably deletes one record. Missing IDs return false without a commit.
    pub fn delete(&mut self, id: u64) -> Result<bool> {
        self.delete_in(0, id, Expected::Any)
    }

    /// Conditional delete. A failed precondition returns `RevisionMismatch`
    /// without writing; `Expected::Absent` on a missing ID returns false.
    pub fn delete_if(&mut self, id: u64, expected: Expected) -> Result<bool> {
        self.delete_in(0, id, expected)
    }

    fn delete_in(&mut self, collection: u16, id: u64, expected: Expected) -> Result<bool> {
        self.ensure_healthy()?;
        let current = self.state(collection).revisions.get(&id).copied();
        expected.check(current)?;
        if current.is_none() {
            return Ok(false);
        }
        #[cfg(feature = "graph")]
        if self.is_attached(collection, id) {
            return Err(Error::GraphAttachedRecord);
        }
        let sequence = self.next_sequence()?;
        let frame = format::record_frame(sequence, collection, id, None);
        self.commit_frame(sequence, &frame)?;
        self.apply(collection, id, None, sequence, None);
        Ok(true)
    }

    /// Validates all operations, appends one atomic frame, then syncs once.
    ///
    /// No operation becomes visible until the entire frame is durably committed.
    /// Duplicate IDs execute in order; an empty batch is a no-op on either format.
    /// Any write/sync failure is indeterminate for the entire batch and poisons
    /// this handle. A complete batch may reappear after an unacknowledged failure.
    pub fn write_batch(&mut self, operations: &[Mutation]) -> Result<()> {
        self.write_batch_in(0, operations)
    }

    fn write_batch_in(&mut self, collection: u16, operations: &[Mutation]) -> Result<()> {
        self.ensure_healthy()?;
        if operations.is_empty() {
            return Ok(());
        }
        if self.version < 2 {
            return Err(Error::BatchRequiresUpgrade);
        }
        let body_bytes = self.validate_batch(collection, operations)?;
        #[cfg(feature = "graph")]
        self.ensure_no_attached_delete(collection, operations)?;
        let sequence = self.next_sequence()?;
        let frame = if collection == 0 {
            format::batch_frame(sequence, operations, body_bytes)
        } else {
            format::batch_frame_v4(sequence, collection, operations, None)
        };
        self.commit_frame(sequence, &frame)?;
        self.apply_batch(collection, operations, sequence);
        Ok(())
    }

    /// Returns the encoded batch body length.
    fn validate_batch(&self, collection: u16, operations: &[Mutation]) -> Result<usize> {
        if operations.len() > MAX_BATCH_OPERATIONS {
            return Err(Error::BatchTooLarge);
        }
        let dimensions = self.state(collection).dimensions;
        let mut body_bytes = 0usize;
        for operation in operations {
            let bytes = match operation {
                Mutation::PutWithMetadata {
                    vector,
                    payload,
                    metadata,
                    ..
                } => {
                    validate_vector(dimensions, vector)?;
                    if payload.len() > MAX_PAYLOAD_BYTES {
                        return Err(Error::PayloadTooLarge(payload.len()));
                    }
                    let metadata_bytes = metadata::validate(metadata)?;
                    if metadata::contains_geo(metadata) && self.version < format::V5 {
                        return Err(Error::ExpiryRequiresUpgrade);
                    }
                    if self.version < 3 {
                        return Err(Error::MetadataRequiresUpgrade);
                    }
                    16 + vector.len() * 4 + 4 + payload.len() + metadata_bytes
                }
                Mutation::Put {
                    vector, payload, ..
                } => {
                    validate_vector(dimensions, vector)?;
                    if payload.len() > MAX_PAYLOAD_BYTES {
                        return Err(Error::PayloadTooLarge(payload.len()));
                    }
                    16 + vector.len() * 4 + payload.len()
                }
                Mutation::Delete { .. } => 16,
            };
            body_bytes = body_bytes.checked_add(bytes).ok_or(Error::BatchTooLarge)?;
            if body_bytes > MAX_BATCH_BYTES {
                return Err(Error::BatchTooLarge);
            }
        }
        Ok(body_bytes)
    }

    fn apply_batch(&mut self, collection: u16, operations: &[Mutation], sequence: u64) {
        for operation in operations {
            let (id, record) = match operation {
                Mutation::Put {
                    id,
                    vector,
                    payload,
                } => (
                    *id,
                    Some(Record {
                        vector: vector.clone(),
                        payload: payload.clone(),
                        metadata: Metadata::new(),
                    }),
                ),
                Mutation::PutWithMetadata {
                    id,
                    vector,
                    payload,
                    metadata,
                } => (
                    *id,
                    Some(Record {
                        vector: vector.clone(),
                        payload: payload.clone(),
                        metadata: metadata.clone(),
                    }),
                ),
                Mutation::Delete { id } => (*id, None),
            };
            self.apply(collection, id, record, sequence, None);
        }
    }

    /// Atomically commits a chunk of rows plus its import checkpoint marker in
    /// one v4 batch frame, syncs once, then applies both. Callers (EPIC-008)
    /// validate rows, job state and contiguity first; the loader re-validates.
    pub(crate) fn commit_import_frame(
        &mut self,
        collection: u16,
        operations: &[Mutation],
        marker: &format::ImportMarker,
    ) -> Result<u64> {
        self.ensure_healthy()?;
        if self.version < format::V4 {
            return Err(Error::ImportsRequireUpgrade);
        }
        self.validate_batch(collection, operations)?;
        let sequence = self.next_sequence()?;
        let frame = format::batch_frame_v4(sequence, collection, operations, Some(marker));
        self.commit_frame(sequence, &frame)?;
        self.apply_batch(collection, operations, sequence);
        let name = &self.collections[&collection].name;
        self.imports.apply(collection, name, marker);
        Ok(sequence)
    }

    fn commit_frame(&mut self, sequence: u64, frame: &[u8]) -> Result<()> {
        if self.group_active {
            self.commit_frame_with_sync(sequence, frame, |_| Ok(()))
        } else {
            self.commit_frame_with_sync(sequence, frame, LockedFile::sync_all)
        }
    }

    /// Internal owner-locked group of record mutations. Only put, delete,
    /// metadata update and batch operations may run inside this scope. The
    /// server must not release the owner lock or acknowledge any result until
    /// this returns successfully. Individual frames retain their recovery
    /// boundaries; a group is not one atomic transaction.
    #[cfg(any(feature = "server", test))]
    pub(crate) fn durable_group<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> Result<T> {
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_group_sync) {
            return self.durable_group_with_sync(f, |_| {
                Err(io::Error::other("injected group sync failure"))
            });
        }
        self.durable_group_with_sync(f, LockedFile::sync_all)
    }

    #[cfg(any(feature = "server", test))]
    fn durable_group_with_sync<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> T,
        sync: impl FnOnce(&LockedFile) -> io::Result<()>,
    ) -> Result<T> {
        self.ensure_healthy()?;
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        if self.group_active {
            return Err(Error::InvalidQuery(
                "nested durability groups are not supported",
            ));
        }
        let sequence = self.sequence;
        self.group_active = true;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(self)));
        self.group_active = false;
        let value = match result {
            Ok(value) => value,
            Err(panic) => {
                self.poisoned = true;
                std::panic::resume_unwind(panic);
            }
        };
        self.ensure_healthy()?;
        if self.sequence != sequence
            && let Err(error) = sync(&self.file)
        {
            self.poisoned = true;
            return Err(Error::CommitIndeterminate(error));
        }
        Ok(value)
    }

    fn commit_frame_with_sync(
        &mut self,
        sequence: u64,
        frame: &[u8],
        sync: impl FnOnce(&LockedFile) -> io::Result<()>,
    ) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        let mut bytes = Vec::new();
        let mut stamp = None;
        if self.flags & format::FLAG_TIMESTAMPS != 0 {
            let now = match &mut self.clock {
                Some(clock) => clock(),
                None => current_time_ms(),
            };
            stamp = Some(now);
            bytes.extend_from_slice(&format::time_anchor_frame(sequence, now));
        }
        bytes.extend_from_slice(frame);
        let sealed;
        let output: &[u8] = if let Some(cipher) = &self.cipher {
            let mut out = Vec::new();
            let anchor_len = bytes.len() - frame.len();
            if anchor_len > 0 {
                out.extend_from_slice(&cipher.seal_frame(&bytes[..anchor_len])?);
            }
            out.extend_from_slice(&cipher.seal_frame(frame)?);
            sealed = out;
            &sealed
        } else {
            &bytes
        };
        let result = append_and_sync(&mut self.file, self.end, output, sync);
        if let Err(error) = result {
            self.poisoned = true;
            return Err(Error::CommitIndeterminate(error));
        }
        self.sequence = sequence;
        self.end += output.len() as u64;
        if let Some(now) = stamp {
            self.commit_times.push((sequence, now));
        }
        Ok(())
    }

    /// Key ID (non-secret fingerprint) of the encryption key, if encrypted.
    pub fn encryption_key_id(&self) -> Option<u64> {
        self.cipher.as_ref().map(|cipher| cipher.descriptor.key_id)
    }

    pub fn is_encrypted(&self) -> bool {
        self.cipher.is_some()
    }

    /// Whether commits carry wall-clock timestamps (timestamp restore).
    pub fn has_commit_timestamps(&self) -> bool {
        self.flags & format::FLAG_TIMESTAMPS != 0
    }

    /// Replaces the commit timestamp source (deterministic tests, simulated time).
    pub fn set_commit_clock(&mut self, clock: CommitClock) {
        self.clock = Some(clock);
    }

    /// Cursor positioned at the current head: `changes_after` returns only
    /// commits made after this call. Needs format v4 or newer.
    pub fn feed_cursor(&self) -> Result<changes::FeedCursor> {
        #[cfg(feature = "graph")]
        if self.graph.is_some() {
            return Err(Error::GraphChangeFeedUnsupported);
        }
        Ok(changes::FeedCursor {
            lineage: self.lineage.ok_or(Error::ChangeFeedRequiresUpgrade)?,
            sequence: self.sequence,
        })
    }

    /// Reads up to about `max_events` committed changes after `cursor`, cut on
    /// commit boundaries (a batch commit is never split). Reads the durable log
    /// with a separate handle, so it observes exactly what a crash would keep.
    /// Cost is one sequential scan of the log per call.
    pub fn changes_after(
        &self,
        cursor: &changes::FeedCursor,
        max_events: usize,
    ) -> Result<changes::ChangeBatch> {
        #[cfg(feature = "graph")]
        if self.graph.is_some() {
            return Err(Error::GraphChangeFeedUnsupported);
        }
        self.lineage.ok_or(Error::ChangeFeedRequiresUpgrade)?;
        let mut file = &self.file;
        changes::read_changes(&mut file, None, self.cipher.as_ref(), cursor, max_events)
    }

    /// Cuts a log segment of commits after `after_sequence` for replication
    /// (see `ReplicaFollower`). `max_bytes` bounds the segment softly: whole
    /// commits are included until it is reached.
    pub fn export_log(&self, after_sequence: u64, max_bytes: usize) -> Result<LogSegment> {
        let mut file = &self.file;
        logops::read_segment(
            &mut file,
            None,
            self.cipher.as_ref(),
            after_sequence,
            max_bytes,
        )
    }

    /// Replica apply step: appends whole, already-authenticated commits to this
    /// follower's own log, then replays the entire file to rebuild state.
    /// On any failure the file is truncated back and the old in-memory state
    /// is kept.
    pub(crate) fn replica_extend(
        &mut self,
        bytes: &[u8],
        expected_last: u64,
        key: Option<&EncryptionKey>,
    ) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        let previous = self.end;
        let outcome = (|| -> Result<format::Loaded> {
            append_and_sync(&mut self.file, previous, bytes, |file| file.sync_all())?;
            self.file.seek(SeekFrom::Start(0))?;
            let loaded = format::load_with(&mut self.file, key)?;
            if loaded.incomplete_bytes != 0
                || loaded.sequence != expected_last
                || loaded.lineage != self.lineage
                || loaded.flags != self.flags
            {
                return Err(Error::ReplicaDiverged(
                    "segment does not extend the follower log",
                ));
            }
            Ok(loaded)
        })();
        match outcome {
            Ok(loaded) => {
                self.sequence = loaded.sequence;
                self.end = loaded.end;
                self.collections = loaded
                    .collections
                    .into_iter()
                    .map(|(id, collection)| (id, Collection::from_loaded(collection)))
                    .collect();
                self.rebuild = None;
                self.imports = import::Registry::from_markers(&loaded.imports);
                self.collection_high = loaded.collection_high;
                self.commit_times = loaded.commit_times;
                // Graph followers replay graph frames too (REQ-054); the
                // flags check above keeps graph and vector files apart.
                #[cfg(feature = "graph")]
                {
                    self.graph = loaded.graph;
                }
                Ok(())
            }
            Err(error) => {
                if self.file.set_len(previous).is_err() || self.file.sync_all().is_err() {
                    self.poisoned = true;
                }
                Err(error)
            }
        }
    }

    /// Single post-commit hook: rebuild catch-up, ANN buffer, derived indexes,
    /// revisions and records, in that order (PAT-002).
    fn apply(
        &mut self,
        collection: u16,
        id: u64,
        record: Option<Record>,
        sequence: u64,
        expires_at_ms: Option<u64>,
    ) {
        let vector_bytes = record.as_ref().map_or(0, |record| record.vector.len() * 4);
        #[cfg(feature = "graph")]
        self.touch_graph_record(collection, id, sequence);
        let state = self
            .collections
            .get_mut(&collection)
            .expect("committed collection is live");
        if let Some((owner, job)) = &mut self.rebuild
            && *owner == collection
        {
            job.track(id, state.records.get(&id), vector_bytes);
            job.set_vector_sequence(sequence);
        }
        state.apply_committed(id, record, sequence, vector_bytes, expires_at_ms);
    }

    // ---------------------------------------------------------------- collections

    fn resolve(&self, name: &str) -> Result<u16> {
        self.ensure_healthy()?;
        if name != "default" && !format::valid_collection_name(name) {
            return Err(Error::InvalidCollectionName);
        }
        self.collections
            .iter()
            .find(|(_, collection)| collection.name == name)
            .map(|(&id, _)| id)
            .ok_or(Error::CollectionNotFound)
    }

    fn info(&self, id: u16) -> CollectionInfo {
        let state = self.state(id);
        CollectionInfo {
            id,
            name: state.name.clone(),
            dimensions: state.dimensions,
            live_records: state.records.len(),
            settings: state.settings.clone(),
            settings_persisted: state.settings_persisted,
        }
    }

    /// Read-only handle to a collection. `default` is always available.
    pub fn collection(&self, name: &str) -> Result<CollectionRef<'_>> {
        let id = self.resolve(name)?;
        Ok(CollectionRef { db: self, id })
    }

    /// Mutable handle to a collection. `default` is always available.
    pub fn collection_mut(&mut self, name: &str) -> Result<CollectionMut<'_>> {
        let id = self.resolve(name)?;
        Ok(CollectionMut { db: self, id })
    }

    /// Live collections in ID order, including `default`.
    pub fn collections(&self) -> Result<Vec<CollectionInfo>> {
        self.ensure_healthy()?;
        Ok(self.collections.keys().map(|&id| self.info(id)).collect())
    }

    /// Durably creates a named collection (v4 only). Names match
    /// `^[a-z][a-z0-9_-]{0,62}$`; `default` is reserved. At most 64 live
    /// collections including `default`. IDs are assigned monotonically and are
    /// never reused within a lineage history, including across `compact`,
    /// `upgrade` and `restore`. Once ID 65,535 has been assigned, every later
    /// create returns `TooManyCollections` without writing.
    pub fn create_collection(
        &mut self,
        name: &str,
        spec: CollectionSpec,
    ) -> Result<CollectionInfo> {
        self.ensure_healthy()?;
        if self.version < format::V4 {
            return Err(Error::CollectionsRequireUpgrade);
        }
        if !format::valid_collection_name(name) {
            return Err(Error::InvalidCollectionName);
        }
        if name == "default" {
            return Err(Error::DefaultCollectionImmutable);
        }
        validate_dimensions(spec.dimensions)?;
        validate_settings(&spec.settings)?;
        if self.collections.values().any(|c| c.name == name) {
            return Err(Error::CollectionExists);
        }
        if self.collections.len() >= format::MAX_COLLECTIONS {
            return Err(Error::TooManyCollections);
        }
        // IDs are never reused within a lineage history (they back ETags,
        // scoped credentials and import identity), so exhausting the u16 ID
        // space is permanent for this lineage.
        let id = self
            .collection_high
            .checked_add(1)
            .ok_or(Error::TooManyCollections)?;
        let sequence = self.next_sequence()?;
        let frame =
            format::collection_create_frame(sequence, id, name, spec.dimensions, &spec.settings);
        self.commit_frame(sequence, &frame)?;
        self.collection_high = id;
        let mut collection = Collection::empty(name.into(), spec.dimensions, spec.settings);
        collection.settings_persisted = true;
        self.collections.insert(id, collection);
        Ok(self.info(id))
    }

    /// Durably drops an empty named collection (v4 only). A collection with
    /// live records or open import jobs returns `CollectionNotEmpty`.
    pub fn drop_collection(&mut self, name: &str) -> Result<()> {
        self.ensure_healthy()?;
        if self.version < format::V4 {
            return Err(Error::CollectionsRequireUpgrade);
        }
        if name == "default" {
            return Err(Error::DefaultCollectionImmutable);
        }
        let id = self.resolve(name)?;
        if !self.state(id).records.is_empty() || self.imports.has_open_in(id) {
            return Err(Error::CollectionNotEmpty);
        }
        let sequence = self.next_sequence()?;
        self.commit_frame(sequence, &format::collection_drop_frame(sequence, id))?;
        if self.rebuild.as_ref().is_some_and(|(owner, _)| *owner == id) {
            let (_, mut job) = self.rebuild.take().expect("owned job");
            job.stop(HnswRebuildStatus::Cancelled);
            // The collection is gone; a worker panic has nothing left to report to.
            let _ = job.finish();
        }
        self.collections.remove(&id);
        self.imports.collection_dropped(id);
        Ok(())
    }

    /// Validates and replaces a collection's settings, then rebuilds its
    /// derived indexes. v4 commits a synced settings frame (`Persisted`);
    /// on v1–v3 only `default` exists and the change is `MemoryOnly`.
    pub fn set_collection_settings(
        &mut self,
        name: &str,
        settings: CollectionSettings,
    ) -> Result<SettingsPersistence> {
        let id = self.resolve(name)?;
        validate_settings(&settings)?;
        let persistence = if self.version >= format::V4 {
            let sequence = self.next_sequence()?;
            self.commit_frame(sequence, &format::settings_frame(sequence, id, &settings))?;
            SettingsPersistence::Persisted
        } else {
            SettingsPersistence::MemoryOnly
        };
        let state = self.state_mut(id);
        state.settings = settings;
        state.settings_persisted = persistence == SettingsPersistence::Persisted;
        state.rebuild_derived();
        Ok(persistence)
    }

    /// Durable status of one import job (v4 only).
    pub fn import_status(&self, job: u64) -> Result<ImportStatus> {
        self.ensure_healthy()?;
        if self.version < format::V4 {
            return Err(Error::ImportsRequireUpgrade);
        }
        let state = self.imports.get(job).ok_or(Error::ImportNotFound)?;
        // The name is the one recorded when the job started; a recreated
        // collection may carry the same name under a new ID.
        Ok(ImportStatus {
            job,
            collection: self.imports.collection_name(job).to_owned(),
            collection_dropped: state.collection_dropped,
            state: state.state,
            last_chunk: state.last_chunk,
            rows_committed: state.rows_committed,
            rows_skipped: state.rows_skipped,
            last_digest: state.last_digest,
        })
    }

    // ---------------------------------------------------------------- HNSW

    /// Builds/replaces the optional in-memory HNSW index from committed live records.
    ///
    /// Clears the mutation buffer. Nothing is written to the database file.
    pub fn build_hnsw(&mut self, config: HnswConfig) -> Result<()> {
        self.build_hnsw_with_metric(config, DistanceMetric::SquaredL2)
    }

    /// Replaces the one in-memory index with a squared-L2 or cosine index.
    /// Cosine excludes zero-norm records. Inner product is not supported.
    pub fn build_hnsw_with_metric(
        &mut self,
        config: HnswConfig,
        metric: DistanceMetric,
    ) -> Result<()> {
        self.build_hnsw_in(0, config, metric)
    }

    fn build_hnsw_in(
        &mut self,
        collection: u16,
        config: HnswConfig,
        metric: DistanceMetric,
    ) -> Result<()> {
        self.build_hnsw_with_options_in(collection, config, metric, AnnOptions::default())
    }

    fn build_hnsw_with_options_in(
        &mut self,
        collection: u16,
        config: HnswConfig,
        metric: DistanceMetric,
        options: AnnOptions,
    ) -> Result<()> {
        self.ensure_healthy()?;
        config.validate()?;
        validate_ann_metric(metric)?;
        options.validate()?;
        if let Some((owner, job)) = &mut self.rebuild
            && *owner == collection
        {
            job.stop(HnswRebuildStatus::Superseded);
        }
        let state = self.state_mut(collection);
        state.index = Some(hnsw::Index::build(&state.records, config, metric, options));
        Ok(())
    }

    /// Captures a bounded committed-vector generation, then builds on one owned thread.
    ///
    /// Unquantized capture shares immutable record bodies in O(1); the first
    /// subsequent write copies the ID tree only. ScalarU8 training/encoding
    /// remains synchronous. Shared record bodies are capped at 256 MiB,
    /// in addition to the supplied vector and record limits. The current
    /// graph is retained until normal mutation-buffer overflow. No automatic
    /// fallback, retry, polling or publication occurs in search/write methods.
    /// Use ready-only queries and poll explicitly to avoid synchronous builds.
    /// One job exists per database across all collections.
    pub fn start_hnsw_rebuild(
        &mut self,
        config: HnswConfig,
        metric: DistanceMetric,
        limits: HnswRebuildLimits,
    ) -> Result<()> {
        self.start_rebuild_in(0, config, metric, limits)
    }

    fn start_rebuild_in(
        &mut self,
        collection: u16,
        config: HnswConfig,
        metric: DistanceMetric,
        limits: HnswRebuildLimits,
    ) -> Result<()> {
        self.start_rebuild_with_options_in(
            collection,
            config,
            metric,
            AnnOptions::default(),
            limits,
        )
    }

    fn start_rebuild_with_options_in(
        &mut self,
        collection: u16,
        config: HnswConfig,
        metric: DistanceMetric,
        options: AnnOptions,
        limits: HnswRebuildLimits,
    ) -> Result<()> {
        self.ensure_healthy()?;
        config.validate()?;
        validate_ann_metric(metric)?;
        options.validate()?;
        let state = self.state(collection);
        limits.validate_for(state.records.len(), state.dimensions, options)?;
        if options.quantization == Quantization::None
            && state.live_body_bytes > rebuild::MAX_HNSW_RETAINED_RECORD_BYTES
        {
            return Err(Error::HnswRebuildTooLarge);
        }
        if self.rebuild.is_some() {
            return Err(Error::HnswRebuildBusy);
        }
        let mut job = rebuild::Job::start(&state.records, config, metric, options)?;
        job.set_vector_sequence(state.vector_sequence);
        self.rebuild = Some((collection, job));
        Ok(())
    }

    /// Nonblocking poll. A completed worker is joined and atomically published
    /// with bounded committed-mutation catch-up, or explicitly discarded.
    /// Catch-up has the same 256-ID / 8-MiB vector limits as the active graph.
    /// Overflow never rejects durable writes or publishes stale snapshot data.
    pub fn poll_hnsw_rebuild(&mut self) -> Result<HnswRebuildStatus> {
        self.poll_rebuild_in(0)
    }

    fn poll_rebuild_in(&mut self, collection: u16) -> Result<HnswRebuildStatus> {
        self.ensure_healthy()?;
        match &self.rebuild {
            Some((owner, job)) if *owner == collection => {
                if !job.finished() {
                    return Ok(job.status());
                }
            }
            _ => return Ok(HnswRebuildStatus::Idle),
        }
        let (_, mut job) = self.rebuild.take().expect("pending job");
        if !job.covers_vector_sequence(self.state(collection).vector_sequence) {
            job.stop(HnswRebuildStatus::Superseded);
        }
        let (status, index) = job.finish()?;
        if let Some(index) = index {
            self.state_mut(collection).index = Some(index);
        }
        Ok(status)
    }

    /// Requests cooperative cancellation. Poll until Cancelled (or another
    /// already-selected terminal reason). Returns false if no job is pending.
    /// Cancellation never waits or replaces the active graph.
    pub fn cancel_hnsw_rebuild(&mut self) -> bool {
        self.cancel_rebuild_in(0)
    }

    fn cancel_rebuild_in(&mut self, collection: u16) -> bool {
        if let Some((owner, job)) = &mut self.rebuild
            && *owner == collection
        {
            job.stop(HnswRebuildStatus::Cancelled);
            true
        } else {
            false
        }
    }

    /// Observe matching-index capacity and worker progress without polling,
    /// publishing, starting, cancelling, or changing any state.
    pub fn hnsw_maintenance_status(
        &self,
        config: HnswConfig,
        metric: DistanceMetric,
        policy: HnswMaintenancePolicy,
    ) -> Result<HnswMaintenanceStatus> {
        self.maintenance_status_in(0, config, metric, policy)
    }

    fn maintenance_status_in(
        &self,
        collection: u16,
        config: HnswConfig,
        metric: DistanceMetric,
        policy: HnswMaintenancePolicy,
    ) -> Result<HnswMaintenanceStatus> {
        self.maintenance_status_with_options_in(
            collection,
            config,
            metric,
            AnnOptions::default(),
            policy,
        )
    }

    fn maintenance_status_with_options_in(
        &self,
        collection: u16,
        config: HnswConfig,
        metric: DistanceMetric,
        options: AnnOptions,
        policy: HnswMaintenancePolicy,
    ) -> Result<HnswMaintenanceStatus> {
        self.ensure_healthy()?;
        config.validate()?;
        validate_ann_metric(metric)?;
        options.validate()?;
        policy.validate()?;
        let active_buffer = self
            .state(collection)
            .index
            .as_ref()
            .filter(|index| index.ready_for(config, metric, options.quantization))
            .map(HnswBufferCapacity::of);
        let rebuild = self
            .rebuild
            .as_ref()
            .filter(|(owner, _)| *owner == collection)
            .map(|(_, job)| job.progress());
        let pressure = if rebuild
            .is_some_and(|job| job.terminal_reason == Some(HnswRebuildStatus::CatchUpOverflow))
        {
            HnswPressure::CatchUpOverflow
        } else if active_buffer.is_none() {
            HnswPressure::Unavailable
        } else if active_buffer.is_some_and(HnswBufferCapacity::exhausted)
            || rebuild
                .and_then(|job| job.catchup_buffer)
                .is_some_and(HnswBufferCapacity::exhausted)
        {
            HnswPressure::CapacityExhausted
        } else if active_buffer.is_some_and(|buffer| buffer.high(policy)) {
            HnswPressure::RebuildRecommended
        } else {
            HnswPressure::Healthy
        };
        Ok(HnswMaintenanceStatus {
            ready: active_buffer.is_some(),
            pressure,
            active_buffer,
            rebuild,
        })
    }

    /// Explicit maintenance tick: reap/publish one matching job, then start a
    /// replacement if the matching index is missing or either high-water mark
    /// is reached. Never waits for construction or silently falls back.
    ///
    /// Catch-up overflow is reported in `outcome` even if a fresh retry starts.
    /// Cancellation/supersession suppress restarting for that tick. Call before
    /// mutation/query batches and inspect status; arbitrary write rates cannot
    /// be guaranteed to maintain readiness.
    pub fn maintain_hnsw(
        &mut self,
        config: HnswConfig,
        metric: DistanceMetric,
        policy: HnswMaintenancePolicy,
    ) -> Result<HnswMaintenanceReport> {
        self.maintain_in(0, config, metric, policy)
    }

    fn maintain_in(
        &mut self,
        collection: u16,
        config: HnswConfig,
        metric: DistanceMetric,
        policy: HnswMaintenancePolicy,
    ) -> Result<HnswMaintenanceReport> {
        self.maintain_with_options_in(collection, config, metric, AnnOptions::default(), policy)
    }

    fn maintain_with_options_in(
        &mut self,
        collection: u16,
        config: HnswConfig,
        metric: DistanceMetric,
        options: AnnOptions,
        policy: HnswMaintenancePolicy,
    ) -> Result<HnswMaintenanceReport> {
        let status_of = |db: &Self| {
            db.maintenance_status_with_options_in(collection, config, metric, options, policy)
        };
        let before = status_of(self)?;
        if self.rebuild.as_ref().is_some_and(|(owner, job)| {
            *owner != collection || job.options().quantization != options.quantization
        }) || before
            .rebuild
            .is_some_and(|job| job.config != config || job.metric != metric)
        {
            return Err(Error::HnswRebuildBusy);
        }
        let outcome = self.poll_rebuild_in(collection)?;
        let status = status_of(self)?;
        let start = status.rebuild.is_none()
            && !matches!(
                outcome,
                HnswRebuildStatus::Cancelled | HnswRebuildStatus::Superseded
            )
            && (!status.ready
                || status
                    .active_buffer
                    .is_some_and(|buffer| buffer.high(policy)));
        if start {
            self.start_rebuild_with_options_in(collection, config, metric, options, policy.limits)?;
        }
        Ok(HnswMaintenanceReport {
            outcome,
            started: start,
            status: status_of(self)?,
        })
    }

    /// Approximate squared-L2 search; exact search remains the default.
    ///
    /// Lazily rebuilds after buffer overflow, reopen, or construction-parameter change.
    /// The first query includes the full rebuild cost. Returned distances are exact
    /// for the visited candidates; nearest-neighbor recall is not guaranteed.
    pub fn search_approximate(
        &mut self,
        query: &[f32],
        k: usize,
        ef_search: usize,
        config: HnswConfig,
    ) -> Result<Vec<SearchHit>> {
        self.search_approximate_with_metric(query, k, ef_search, config, DistanceMetric::SquaredL2)
    }

    /// Approximate search that lazily rebuilds on metric or configuration mismatch.
    pub fn search_approximate_with_metric(
        &mut self,
        query: &[f32],
        k: usize,
        ef_search: usize,
        config: HnswConfig,
        metric: DistanceMetric,
    ) -> Result<Vec<SearchHit>> {
        self.search_approximate_filtered_with_metric(query, k, ef_search, config, &[], metric)
    }

    /// Filter-aware approximate search that lazily rebuilds like
    /// `search_approximate_with_metric`. Conditions are the same typed AND
    /// predicates as `search_filtered_with_metric`, validated before any
    /// zero-k/empty return or index build.
    ///
    /// Filtering participates in base-layer traversal: nonmatching and
    /// buffered graph nodes remain navigation bridges, while only current
    /// records whose current metadata matches enter the result heap. Buffered
    /// updates are scored from current records, never from stale graph
    /// vectors or metadata. `ef_search` bounds eligible candidates retained,
    /// not nodes visited; selective or empty-match filters may visit every
    /// reachable node. Returns up to k eligible hits with exact distances,
    /// ordered by distance then ID. Recall and filling k are not guaranteed,
    /// and there is no exact fallback. Empty conditions are unfiltered search.
    pub fn search_approximate_filtered_with_metric(
        &mut self,
        query: &[f32],
        k: usize,
        ef_search: usize,
        config: HnswConfig,
        conditions: &[Condition],
        metric: DistanceMetric,
    ) -> Result<Vec<SearchHit>> {
        self.search_approximate_in(0, query, k, ef_search, config, conditions, metric)
    }

    #[allow(clippy::too_many_arguments)]
    fn search_approximate_in(
        &mut self,
        collection: u16,
        query: &[f32],
        k: usize,
        ef_search: usize,
        config: HnswConfig,
        conditions: &[Condition],
        metric: DistanceMetric,
    ) -> Result<Vec<SearchHit>> {
        self.validate_hnsw_query(collection, query, k, ef_search, config, conditions, metric)?;
        let state = self.state(collection);
        if k == 0 || state.records.is_empty() {
            return Ok(Vec::new());
        }
        if state
            .index
            .as_ref()
            .is_none_or(|index| !index.ready_for(config, metric, Quantization::None))
        {
            self.build_hnsw_in(collection, config, metric)?;
        }
        let state = self.state(collection);
        Ok(state
            .index
            .as_ref()
            .expect("built index")
            .search_filtered_with_expiry(
                query,
                k,
                ef_search,
                AnnOptions::default(),
                &state.records,
                &state.expiries,
                current_time_ms(),
                conditions,
                &mut SearchStats::default(),
            ))
    }

    /// Searches an existing matching HNSW index without building one.
    ///
    /// Returns `IndexNotReady` after reopen, buffer overflow, or a configuration
    /// mismatch. Call `build_hnsw` separately during a maintenance window.
    /// Buffered updates remain visible. Empty/k=0 queries validate inputs but
    /// need no index. This avoids rebuild work, not traversal or queue latency.
    pub fn search_approximate_ready(
        &self,
        query: &[f32],
        k: usize,
        ef_search: usize,
        config: HnswConfig,
    ) -> Result<Vec<SearchHit>> {
        self.search_approximate_ready_with_metric(
            query,
            k,
            ef_search,
            config,
            DistanceMetric::SquaredL2,
        )
    }

    /// Ready-only search; a metric mismatch returns `IndexNotReady`, never rebuilds.
    pub fn search_approximate_ready_with_metric(
        &self,
        query: &[f32],
        k: usize,
        ef_search: usize,
        config: HnswConfig,
        metric: DistanceMetric,
    ) -> Result<Vec<SearchHit>> {
        self.search_approximate_ready_filtered_with_metric(query, k, ef_search, config, &[], metric)
    }

    /// Ready-only filter-aware search; never builds. Malformed filters are
    /// rejected before readiness and zero-k checks. Traversal, buffering and
    /// result semantics match `search_approximate_filtered_with_metric`.
    pub fn search_approximate_ready_filtered_with_metric(
        &self,
        query: &[f32],
        k: usize,
        ef_search: usize,
        config: HnswConfig,
        conditions: &[Condition],
        metric: DistanceMetric,
    ) -> Result<Vec<SearchHit>> {
        self.search_ready_in(
            0,
            query,
            k,
            ef_search,
            config,
            conditions,
            metric,
            &mut SearchStats::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn search_ready_in(
        &self,
        collection: u16,
        query: &[f32],
        k: usize,
        ef_search: usize,
        config: HnswConfig,
        conditions: &[Condition],
        metric: DistanceMetric,
        stats: &mut SearchStats,
    ) -> Result<Vec<SearchHit>> {
        self.search_ready_with_options_in(
            collection,
            query,
            k,
            ef_search,
            config,
            AnnOptions::default(),
            conditions,
            metric,
            stats,
        )
    }

    /// Ready-only search of the index identified by `(config, metric,
    /// options.quantization)`; the index's own rerank multiplier is replaced
    /// by `options.rerank_multiplier` for this query only.
    #[allow(clippy::too_many_arguments)]
    fn search_ready_with_options_in(
        &self,
        collection: u16,
        query: &[f32],
        k: usize,
        ef_search: usize,
        config: HnswConfig,
        options: AnnOptions,
        conditions: &[Condition],
        metric: DistanceMetric,
        stats: &mut SearchStats,
    ) -> Result<Vec<SearchHit>> {
        self.validate_hnsw_query(collection, query, k, ef_search, config, conditions, metric)?;
        options.validate()?;
        let state = self.state(collection);
        if k == 0 || state.records.is_empty() {
            return Ok(Vec::new());
        }
        let index = state
            .index
            .as_ref()
            .filter(|index| index.ready_for(config, metric, options.quantization))
            .ok_or(Error::IndexNotReady)?;
        Ok(index.search_filtered_with_expiry(
            query,
            k,
            ef_search,
            options,
            &state.records,
            &state.expiries,
            current_time_ms(),
            conditions,
            stats,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn validate_hnsw_query(
        &self,
        collection: u16,
        query: &[f32],
        k: usize,
        ef_search: usize,
        config: HnswConfig,
        conditions: &[Condition],
        metric: DistanceMetric,
    ) -> Result<()> {
        self.ensure_healthy()?;
        let state = self.state(collection);
        validate_vector(state.dimensions, query)?;
        metadata::validate_filter(conditions)?;
        config.validate()?;
        validate_ann_metric(metric)?;
        validate_metric_query(query, metric)?;
        if !(1..=MAX_EF_SEARCH).contains(&ef_search) || ef_search < k.min(state.records.len()) {
            return Err(Error::InvalidEfSearch);
        }
        Ok(())
    }

    // ---------------------------------------------------------------- exact search

    /// Exact squared-L2 nearest neighbors, ordered by distance then ascending ID.
    ///
    /// k=0 returns an empty result; k above the live count returns all records.
    /// Query vectors are validated even when k=0 or the database is empty.
    pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<SearchHit>> {
        self.search_filtered(query, k, &[])
    }

    /// Exact search at a caller-supplied Unix-millisecond time.
    pub fn search_at(&self, query: &[f32], k: usize, now_ms: u64) -> Result<Vec<SearchHit>> {
        self.search_in_at(
            0,
            query,
            k,
            &[],
            DistanceMetric::SquaredL2,
            now_ms,
            &mut SearchStats::default(),
        )
    }

    /// Exact top-k over records satisfying every condition; no HNSW post-filtering.
    /// Retains at most min(k, live count) candidates, not one hit per matching record.
    pub fn search_filtered(
        &self,
        query: &[f32],
        k: usize,
        conditions: &[Condition],
    ) -> Result<Vec<SearchHit>> {
        self.search_filtered_with_metric(query, k, conditions, DistanceMetric::SquaredL2)
    }

    /// Exact search with explicit metric; lower distance is better for every metric.
    pub fn search_with_metric(
        &self,
        query: &[f32],
        k: usize,
        metric: DistanceMetric,
    ) -> Result<Vec<SearchHit>> {
        self.search_filtered_with_metric(query, k, &[], metric)
    }

    /// Cosine rejects zero queries even for k=0, and excludes zero-norm records.
    /// Inner product returns negative dot product; cosine returns 1 - similarity.
    pub fn search_filtered_with_metric(
        &self,
        query: &[f32],
        k: usize,
        conditions: &[Condition],
        metric: DistanceMetric,
    ) -> Result<Vec<SearchHit>> {
        self.search_in(0, query, k, conditions, metric, &mut SearchStats::default())
    }

    fn search_in(
        &self,
        collection: u16,
        query: &[f32],
        k: usize,
        conditions: &[Condition],
        metric: DistanceMetric,
        stats: &mut SearchStats,
    ) -> Result<Vec<SearchHit>> {
        self.search_in_at(
            collection,
            query,
            k,
            conditions,
            metric,
            current_time_ms(),
            stats,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn search_in_at(
        &self,
        collection: u16,
        query: &[f32],
        k: usize,
        conditions: &[Condition],
        metric: DistanceMetric,
        now_ms: u64,
        stats: &mut SearchStats,
    ) -> Result<Vec<SearchHit>> {
        self.ensure_healthy()?;
        let state = self.state(collection);
        validate_vector(state.dimensions, query)?;
        metadata::validate_filter(conditions)?;
        let query_norm = validate_metric_query(query, metric)?;
        // Declared metadata indexes narrow the scan to a re-verified superset;
        // exact ordering makes the result identical to a full scan.
        let lookup = state
            .metadata_indexes
            .lookup(conditions, state.records.len() / 2);
        let candidates = lookup.and_then(|lookup| lookup.ids);
        if let Some(ids) = &candidates {
            stats.index_candidates = stats.index_candidates.saturating_add(ids.len() as u64);
        }
        Ok(state.exact_search_candidates(
            query,
            query_norm,
            k,
            conditions,
            metric,
            candidates.as_ref(),
            now_ms,
            stats,
        ))
    }

    /// Consumes the handle and releases its OS lock. Commits are already synced.
    ///
    /// Dropping the handle is also safe: there is no buffered uncommitted state.
    pub fn close(mut self) -> Result<()> {
        self.ensure_healthy()?;
        if let Some((_, mut job)) = self.rebuild.take() {
            job.stop(HnswRebuildStatus::Cancelled);
            job.finish()?;
        }
        self.file.unlock()?;
        Ok(())
    }

    // ---------------------------------------------------------------- ANN snapshot

    /// Conventional sidecar path for a datafile: `<datafile>.ann`.
    pub fn ann_snapshot_path(datafile: impl AsRef<Path>) -> PathBuf {
        ann_snapshot::sidecar_path(datafile.as_ref())
    }

    /// Atomically writes every in-memory HNSW index (graph, entry, norms,
    /// mutation buffer, quantizer and codes) with this database's identity and
    /// per-collection content digests. The datafile is never written.
    ///
    /// The snapshot is the active indexes at the current committed state; a
    /// pending background rebuild is not included. The bytes are staged under
    /// a random `create_new` name in the destination directory, re-validated
    /// with the loader, synced and renamed over `path`. An existing `path` is
    /// replaced only if it is a regular file starting with the sidecar magic,
    /// so a datafile, symlink or unrelated file is refused with `Io`
    /// (`InvalidInput`). Any error before the rename leaves `path` untouched;
    /// `PublicationIndeterminate` (Unix directory sync) means `path` already
    /// holds the complete new snapshot but its durability is uncertain.
    /// The sidecar contains vector data; protect it like the datafile.
    pub fn save_ann_snapshot(&self, path: impl AsRef<Path>) -> Result<AnnSnapshotReport> {
        self.ensure_healthy()?;
        if self.cipher.is_some() {
            return Err(Error::SidecarUnavailable(
                "ANN snapshots hold plaintext vectors and are not written for encrypted databases; rebuild indexes after open",
            ));
        }
        ann_snapshot::save(self, path.as_ref())
    }

    /// Validates a sidecar against the current committed state and, only if
    /// every section passes, installs its indexes (replacing those
    /// collections' indexes). Every sidecar problem is an explicit
    /// `Missing`/`Rejected` outcome that leaves all state unchanged; the file
    /// is only read. Returns `Err` only for `Poisoned` or `HnswRebuildBusy`
    /// (a background rebuild is pending).
    pub fn load_ann_snapshot(&mut self, path: impl AsRef<Path>) -> Result<AnnSnapshotLoad> {
        self.ensure_healthy()?;
        if self.rebuild.is_some() {
            return Err(Error::HnswRebuildBusy);
        }
        Ok(ann_snapshot::load(self, path.as_ref()))
    }
}

/// Exact bounded top-k scan; ordering is distance then ascending ID.
///
/// With `candidates`, only those IDs (if live) are scored; results are
/// identical to a full scan restricted to that set. Inputs are prevalidated.
#[allow(clippy::too_many_arguments)]
fn exact_search(
    records: &records::RecordMap,
    expiries: &BTreeMap<u64, u64>,
    query: &[f32],
    query_norm: f64,
    k: usize,
    conditions: &[Condition],
    metric: DistanceMetric,
    candidates: Option<&BTreeSet<u64>>,
    now_ms: u64,
    stats: &mut SearchStats,
) -> Vec<SearchHit> {
    let limit = k.min(
        candidates
            .map_or(records.len(), BTreeSet::len)
            .min(records.len()),
    );
    if limit == 0 {
        return Vec::new();
    }
    let scan: Box<dyn Iterator<Item = (&u64, &Record)>> = match candidates {
        None => Box::new(records.iter()),
        Some(ids) => Box::new(ids.iter().filter_map(|id| records.get_key_value(id))),
    };
    #[cfg(not(target_arch = "wasm32"))]
    {
        let rows = candidates.map_or(records.len(), BTreeSet::len);
        if rows >= PARALLEL_MIN_ROWS
            && rows.saturating_mul(query.len()) >= PARALLEL_MIN_WORK
            && rayon::current_num_threads() > 1
        {
            return parallel_exact_search(
                scan.collect(),
                expiries,
                query,
                query_norm,
                limit,
                conditions,
                metric,
                now_ms,
                stats,
            );
        }
    }
    exact_scan(
        scan, expiries, query, query_norm, limit, conditions, metric, now_ms, stats,
    )
    .into_sorted_vec()
    .into_iter()
    .map(|hit| hit.0)
    .collect()
}

/// Minimum rows and rows×dimensions before an exact scan is split across the
/// shared rayon pool; below this, task overhead outweighs the parallel win.
#[cfg(not(target_arch = "wasm32"))]
const PARALLEL_MIN_ROWS: usize = 4096;
#[cfg(not(target_arch = "wasm32"))]
const PARALLEL_MIN_WORK: usize = 1 << 19;

/// Splits the scan into chunks, keeps a bounded top-k per chunk and merges.
/// `RankedHit` is a total order (distance, then ID), so the merged result is
/// identical to the serial scan; scan statistics are summed per chunk.
#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::too_many_arguments)]
fn parallel_exact_search(
    rows: Vec<(&u64, &Record)>,
    expiries: &BTreeMap<u64, u64>,
    query: &[f32],
    query_norm: f64,
    limit: usize,
    conditions: &[Condition],
    metric: DistanceMetric,
    now_ms: u64,
    stats: &mut SearchStats,
) -> Vec<SearchHit> {
    use rayon::prelude::*;
    let chunk = rows
        .len()
        .div_ceil(rayon::current_num_threads() * 4)
        .max(1024);
    let partials: Vec<(BinaryHeap<RankedHit>, SearchStats)> = rows
        .par_chunks(chunk)
        .map(|part| {
            let mut local = SearchStats::default();
            let heap = exact_scan(
                part.iter().copied(),
                expiries,
                query,
                query_norm,
                limit,
                conditions,
                metric,
                now_ms,
                &mut local,
            );
            (heap, local)
        })
        .collect();
    let mut merged: BinaryHeap<RankedHit> = BinaryHeap::with_capacity(limit + 1);
    for (heap, local) in partials {
        stats.records_scanned += local.records_scanned;
        stats.distance_evaluations += local.distance_evaluations;
        for hit in heap {
            if merged.len() < limit {
                merged.push(hit);
            } else if let Some(mut worst) = merged.peek_mut()
                && hit < *worst
            {
                *worst = hit;
            }
        }
    }
    merged
        .into_sorted_vec()
        .into_iter()
        .map(|hit| hit.0)
        .collect()
}

/// Serial bounded top-k over `scan`; returns the unsorted heap.
#[allow(clippy::too_many_arguments)]
fn exact_scan<'a>(
    scan: impl Iterator<Item = (&'a u64, &'a Record)>,
    expiries: &BTreeMap<u64, u64>,
    query: &[f32],
    query_norm: f64,
    limit: usize,
    conditions: &[Condition],
    metric: DistanceMetric,
    now_ms: u64,
    stats: &mut SearchStats,
) -> BinaryHeap<RankedHit> {
    let mut hits: BinaryHeap<RankedHit> = BinaryHeap::with_capacity(limit);
    'records: for (&id, record) in scan {
        stats.records_scanned += 1;
        if expiries.get(&id).is_some_and(|expires| *expires <= now_ms)
            || !metadata::matches(&record.metadata, conditions)
        {
            continue;
        }
        stats.distance_evaluations += 1;
        let distance = if metric != DistanceMetric::SquaredL2 {
            let norm = if metric == DistanceMetric::Cosine {
                vector_norm(&record.vector)
            } else {
                0.0
            };
            let Some(distance) = metric_distance(&record.vector, query, metric, norm, query_norm)
            else {
                continue;
            };
            distance
        } else if query.len() <= 32 {
            record
                .vector
                .iter()
                .zip(query)
                .map(|(&a, &b)| {
                    let difference = f64::from(a) - f64::from(b);
                    difference * difference
                })
                .sum()
        } else {
            let cutoff = if hits.len() == limit {
                hits.peek().expect("nonempty bounded heap").0.distance
            } else {
                f64::INFINITY
            };
            let mut distance = 0.0;
            for (values, query) in record.vector.chunks(32).zip(query.chunks(32)) {
                for (&a, &b) in values.iter().zip(query) {
                    let difference = f64::from(a) - f64::from(b);
                    distance += difference * difference;
                }
                // Nonnegative terms cannot bring a rejected candidate below the cutoff.
                if distance > cutoff {
                    continue 'records;
                }
            }
            distance
        };
        let hit = RankedHit(SearchHit { id, distance });
        if hits.len() < limit {
            hits.push(hit);
        } else if let Some(mut worst) = hits.peek_mut()
            && hit < *worst
        {
            *worst = hit;
        }
    }
    hits
}

fn append_and_sync<W: Write + Seek>(
    file: &mut W,
    end: u64,
    frame: &[u8],
    sync: impl FnOnce(&W) -> io::Result<()>,
) -> io::Result<()> {
    file.seek(SeekFrom::Start(end))?;
    file.write_all(frame)?;
    sync(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn committed_mutations_keep_graph_until_buffer_overflow_and_failed_inputs_do_not_change_it() {
        let directory = tempfile::tempdir().unwrap();
        let mut db = Database::create(directory.path().join("buffer.vlite"), 1).unwrap();
        db.put(0, &[0.0], b"").unwrap();
        db.build_hnsw(HnswConfig::default()).unwrap();
        for id in 0..MAX_HNSW_BUFFER_IDS as u64 {
            db.put(id, &[id as f32], b"").unwrap();
            assert_eq!(
                db.default_state().index.as_ref().unwrap().buffer_usage().0,
                id as usize + 1
            );
        }
        let before = db.default_state().index.as_ref().unwrap().buffer_usage();
        assert!(db.put(999, &[], b"").is_err());
        db.write_batch(&[]).unwrap();
        assert!(!db.delete(999).unwrap());
        assert_eq!(
            db.default_state().index.as_ref().unwrap().buffer_usage(),
            before
        );
        db.put(0, &[-1.0], b"").unwrap();
        assert_eq!(
            db.default_state().index.as_ref().unwrap().buffer_usage(),
            before
        );
        db.put(1000, &[-2.0], b"").unwrap();
        assert!(db.default_state().index.is_none());
        assert_eq!(
            db.search_approximate(&[-2.0], 1, 100, HnswConfig::default())
                .unwrap()[0]
                .id,
            1000
        );
        assert_eq!(
            db.default_state().index.as_ref().unwrap().buffer_usage(),
            (0, 0)
        );
        db.delete(1000).unwrap();
        db.build_hnsw(HnswConfig::default()).unwrap();
        assert_eq!(
            db.default_state().index.as_ref().unwrap().buffer_usage(),
            (0, 0)
        );
        db.file = LockedFile::disk(File::open(directory.path().join("buffer.vlite")).unwrap());
        assert!(matches!(
            db.put(999, &[0.0], b""),
            Err(Error::CommitIndeterminate(_))
        ));
        assert_eq!(
            db.default_state().index.as_ref().unwrap().buffer_usage(),
            (0, 0)
        );
        assert!(matches!(
            db.search_approximate(&[0.0], 1, 100, HnswConfig::default()),
            Err(Error::Poisoned)
        ));
        assert!(matches!(
            db.search_approximate_ready(&[0.0], 1, 100, HnswConfig::default()),
            Err(Error::Poisoned)
        ));
    }
    use std::io::Cursor;

    struct FailingWriter {
        bytes: Cursor<Vec<u8>>,
        remaining: usize,
    }

    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::Error::other("injected disk full"));
            }
            let written = self
                .bytes
                .write(&bytes[..bytes.len().min(self.remaining)])?;
            self.remaining -= written;
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Seek for FailingWriter {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            self.bytes.seek(position)
        }
    }

    #[test]
    fn short_writes_and_sync_errors_are_not_successful_commits() {
        let record = Record {
            vector: vec![1.0],
            payload: b"data".to_vec(),
            metadata: Metadata::new(),
        };
        let frame = format::frame(1, 42, Some(&record));
        for cut in 0..frame.len() {
            let mut writer = FailingWriter {
                bytes: Cursor::new(Vec::new()),
                remaining: cut,
            };
            let synced = Cell::new(false);
            let result = append_and_sync(&mut writer, 0, &frame, |_| {
                synced.set(true);
                Ok(())
            });
            assert!(result.is_err());
            assert!(!synced.get());
            assert_eq!(writer.bytes.into_inner(), frame[..cut]);
        }
        let mut cursor = Cursor::new(Vec::new());
        let result = append_and_sync(&mut cursor, 0, &frame, |writer| {
            assert_eq!(writer.get_ref(), &frame);
            Err(io::Error::other("injected sync failure"))
        });
        assert!(result.is_err());
        assert_eq!(cursor.into_inner(), frame);
    }

    #[test]
    fn failed_commit_poisoning_prevents_stale_reads_and_further_writes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("poison.vlite");
        let mut db = Database::create(&path, 1).unwrap();
        db.put(1, &[1.0], b"original").unwrap();
        db.build_hnsw(HnswConfig::default()).unwrap();
        // A read-only descriptor deterministically fails the next disk write.
        db.file = LockedFile::disk(File::open(&path).unwrap());
        assert!(matches!(
            db.put(1, &[2.0], b"replacement"),
            Err(Error::CommitIndeterminate(_))
        ));
        assert_eq!(db.sequence, 1);
        assert_eq!(
            db.default_state().records.get(&1).unwrap().payload,
            b"original"
        );
        assert!(matches!(db.get(1), Err(Error::Poisoned)));
        assert!(matches!(db.search(&[1.0], 1), Err(Error::Poisoned)));
        assert!(matches!(
            db.search_approximate(&[1.0], 1, 10, HnswConfig::default()),
            Err(Error::Poisoned)
        ));
        assert!(matches!(
            db.build_hnsw(HnswConfig::default()),
            Err(Error::Poisoned)
        ));
        assert!(matches!(db.put(2, &[1.0], b""), Err(Error::Poisoned)));
        assert!(matches!(db.delete(1), Err(Error::Poisoned)));
        assert!(matches!(db.close(), Err(Error::Poisoned)));
        assert_eq!(
            Database::open(path)
                .unwrap()
                .get(1)
                .unwrap()
                .unwrap()
                .payload,
            b"original"
        );
    }

    #[test]
    fn exhausted_sequence_rejects_mutation_without_poisoning_or_writing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sequence.vlite");
        let mut db = Database::create(&path, 1).unwrap();
        db.sequence = u64::MAX;
        assert!(matches!(
            db.put(1, &[1.0], b""),
            Err(Error::SequenceExhausted)
        ));
        assert!(matches!(
            db.write_batch(&[Mutation::Delete { id: 1 }]),
            Err(Error::SequenceExhausted)
        ));
        db.write_batch(&[]).unwrap();
        assert!(!db.poisoned);
        assert!(db.get(1).unwrap().is_none());
        assert_eq!(db.file.len().unwrap(), format::HEADER_SIZE);
    }

    #[test]
    fn close_and_drop_unlock_even_when_a_descriptor_is_duplicated() {
        let directory = tempfile::tempdir().unwrap();
        for explicitly_close in [true, false] {
            let path = directory.path().join(format!("{explicitly_close}.vlite"));
            let db = Database::create(&path, 1).unwrap();
            // A Unix fork inherits the same open file description, just as
            // try_clone does; its lifetime must not extend database ownership.
            let duplicate = db.file.try_clone().unwrap();
            if explicitly_close {
                db.close().unwrap();
            } else {
                drop(db);
            }
            Database::open(&path).unwrap().close().unwrap();
            drop(duplicate);
        }
    }

    #[test]
    fn failed_batch_write_poisoning_preserves_all_in_memory_records() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("batch-poison.vlite");
        let mut db = Database::create(&path, 1).unwrap();
        db.put(1, &[1.0], b"old").unwrap();
        db.file = LockedFile::disk(File::open(&path).unwrap());
        assert!(matches!(
            db.write_batch(&[
                Mutation::Delete { id: 1 },
                Mutation::Put {
                    id: 2,
                    vector: vec![2.0],
                    payload: Vec::new()
                },
            ]),
            Err(Error::CommitIndeterminate(_))
        ));
        assert_eq!(db.default_state().records.len(), 1);
        assert_eq!(db.default_state().records.get(&1).unwrap().payload, b"old");
        assert!(matches!(db.write_batch(&[]), Err(Error::Poisoned)));
        drop(db);
        let db = Database::open(&path).unwrap();
        assert_eq!(db.len(), 1);
        assert!(db.get(2).unwrap().is_none());
    }

    #[test]
    fn batch_sync_is_once_and_failure_may_leave_a_complete_atomic_commit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("batch-sync.vlite");
        let mut db = Database::create(&path, 1).unwrap();
        db.put(1, &[1.0], b"old").unwrap();
        let operations = [
            Mutation::Delete { id: 1 },
            Mutation::Put {
                id: 2,
                vector: vec![2.0],
                payload: Vec::new(),
            },
        ];
        let frame = format::batch_frame(2, &operations, 36);
        let sync_count = Cell::new(0);
        let result = db.commit_frame_with_sync(2, &frame, |file| {
            sync_count.set(sync_count.get() + 1);
            file.sync_all()?;
            Err(io::Error::other("injected failure after flush"))
        });
        assert!(matches!(result, Err(Error::CommitIndeterminate(_))));
        assert_eq!(sync_count.get(), 1);
        assert_eq!(db.sequence, 1);
        assert!(matches!(db.get(1), Err(Error::Poisoned)));
        drop(db);
        let db = Database::open(&path).unwrap();
        assert_eq!(db.len(), 1);
        assert!(db.get(1).unwrap().is_none());
        assert_eq!(db.get(2).unwrap().unwrap().vector, [2.0]);
    }

    #[test]
    fn poisoned_database_refuses_ann_snapshot_save_and_load() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("poisoned-ann.vlite");
        let sidecar = Database::ann_snapshot_path(&path);
        let mut db = Database::create(&path, 1).unwrap();
        db.put(1, &[1.0], b"").unwrap();
        db.build_hnsw(HnswConfig::default()).unwrap();
        db.save_ann_snapshot(&sidecar).unwrap();
        let saved = std::fs::read(&sidecar).unwrap();
        let record = Record {
            vector: vec![2.0],
            payload: Vec::new(),
            metadata: Metadata::new(),
        };
        let frame = format::frame(2, 2, Some(&record));
        let result = db.commit_frame_with_sync(2, &frame, |_| {
            Err(io::Error::other("injected failure after flush"))
        });
        assert!(matches!(result, Err(Error::CommitIndeterminate(_))));
        assert!(matches!(
            db.save_ann_snapshot(&sidecar),
            Err(Error::Poisoned)
        ));
        assert!(matches!(
            db.load_ann_snapshot(&sidecar),
            Err(Error::Poisoned)
        ));
        assert_eq!(std::fs::read(&sidecar).unwrap(), saved);
    }

    #[test]
    fn short_batch_writes_never_sync_and_success_syncs_once() {
        let operations = [Mutation::Delete { id: 1 }, Mutation::Delete { id: 2 }];
        let frame = format::batch_frame(1, &operations, 32);
        for cut in 0..frame.len() {
            let mut writer = FailingWriter {
                bytes: Cursor::new(Vec::new()),
                remaining: cut,
            };
            let sync_count = Cell::new(0);
            assert!(
                append_and_sync(&mut writer, 0, &frame, |_| {
                    sync_count.set(sync_count.get() + 1);
                    Ok(())
                })
                .is_err()
            );
            assert_eq!(sync_count.get(), 0);
        }
        let sync_count = Cell::new(0);
        let mut writer = Cursor::new(Vec::new());
        append_and_sync(&mut writer, 0, &frame, |_| {
            sync_count.set(sync_count.get() + 1);
            Ok(())
        })
        .unwrap();
        assert_eq!(sync_count.get(), 1);
        assert_eq!(writer.into_inner(), frame);
    }
}

#[cfg(test)]
mod v4_tests {
    use super::*;
    use crate::format::{ImportMarker, MARKER_ABORTED, MARKER_FINISHED};
    use std::fs;

    fn decode_hex(text: &str) -> Vec<u8> {
        let digits: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
        digits
            .chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    fn chunk(job: u64, chunk: u64, rows: &[Mutation], skipped: u32) -> ImportMarker {
        ImportMarker {
            job,
            chunk,
            rows_committed: rows.len() as u64,
            rows_skipped: u64::from(skipped),
            flags: 0,
            digest: import::chunk_digest(rows, InvalidRowPolicy::RejectChunk),
        }
    }

    fn close_marker(job: u64, chunk: u64, flags: u32) -> ImportMarker {
        ImportMarker {
            job,
            chunk,
            rows_committed: 0,
            rows_skipped: 0,
            flags,
            digest: [0; 32],
        }
    }

    fn put(id: u64, vector: &[f32], payload: &[u8]) -> Mutation {
        Mutation::Put {
            id,
            vector: vector.to_vec(),
            payload: payload.to_vec(),
        }
    }

    #[test]
    fn v4_writer_matches_independently_encoded_golden_bytes() {
        let expected = decode_hex(include_str!("../tests/fixtures/v4.hex"));
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("v4.vlite");
        fs::write(&path, &expected[..32]).unwrap();
        let mut db = Database::open(&path).unwrap();
        assert_eq!(db.lineage(), Some(0x0123_4567_89ab_cdef));
        let info = db
            .create_collection(
                "docs",
                CollectionSpec {
                    dimensions: 3,
                    settings: CollectionSettings::default(),
                },
            )
            .unwrap();
        assert_eq!(info.id, 1);
        let settings = CollectionSettings {
            text_fields: vec!["title".into()],
            ..CollectionSettings::default()
        };
        assert_eq!(
            db.set_collection_settings("default", settings).unwrap(),
            SettingsPersistence::Persisted
        );
        db.put(9, &[0.0, 2.0], b"b").unwrap();
        assert_eq!(
            db.collection_mut("docs")
                .unwrap()
                .put(5, &[1.0, 2.0, 3.0], b"x")
                .unwrap(),
            4
        );
        let rows = [put(6, &[4.0, 5.0, 6.0], b"y")];
        assert_eq!(db.imports.next_job_id(), Some(1));
        db.commit_import_frame(1, &rows, &chunk(1, 1, &rows, 0))
            .unwrap();
        db.close().unwrap();
        assert_eq!(fs::read(&path).unwrap(), expected);

        let db = Database::open(&path).unwrap();
        let status = db.import_status(1).unwrap();
        assert_eq!(status.collection, "docs");
        assert_eq!(status.state, ImportState::Open);
        assert_eq!((status.last_chunk, status.rows_committed), (1, 1));
        assert_eq!(status.last_digest, chunk(1, 1, &rows, 0).digest);
        assert_eq!(db.collection("docs").unwrap().revision(6).unwrap(), Some(5));
        assert_eq!(db.imports.next_job_id(), Some(2));
    }

    #[test]
    fn chunk_digest_is_full_sha256_of_canonical_entries_and_policy() {
        let rows = [put(6, &[4.0, 5.0, 6.0], b"y")];
        let digest = import::chunk_digest(&rows, InvalidRowPolicy::RejectChunk);
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "59bb806719e92c3ca406492e13ea608f12455d914fb71a23f3298b293875b1c8"
        );
        assert_ne!(
            import::chunk_digest(&rows, InvalidRowPolicy::SkipInvalid),
            digest
        );
    }

    fn v4_with_docs(path: &Path) -> Database {
        let mut db = Database::create_with_format(path, 2, FormatVersion::V4).unwrap();
        db.create_collection(
            "docs",
            CollectionSpec {
                dimensions: 2,
                settings: CollectionSettings::default(),
            },
        )
        .unwrap();
        db
    }

    #[test]
    fn import_hook_requires_v4_and_open_jobs_block_drop_until_closed() {
        let directory = tempfile::tempdir().unwrap();
        let mut v3 = Database::create(directory.path().join("v3.vlite"), 2).unwrap();
        let rows = [put(1, &[1.0, 1.0], b"")];
        assert!(matches!(
            v3.commit_import_frame(0, &rows, &chunk(1, 1, &rows, 0)),
            Err(Error::ImportsRequireUpgrade)
        ));
        assert!(matches!(
            v3.import_status(1),
            Err(Error::ImportsRequireUpgrade)
        ));

        let path = directory.path().join("v4.vlite");
        let mut db = v4_with_docs(&path);
        db.commit_import_frame(1, &rows, &chunk(7, 1, &rows, 2))
            .unwrap();
        db.collection_mut("docs").unwrap().delete(1).unwrap();
        assert!(matches!(
            db.drop_collection("docs"),
            Err(Error::CollectionNotEmpty)
        ));
        db.commit_import_frame(1, &[], &close_marker(7, 1, MARKER_FINISHED))
            .unwrap();
        let status = db.import_status(7).unwrap();
        assert_eq!(status.state, ImportState::Finished);
        assert_eq!((status.rows_committed, status.rows_skipped), (1, 2));
        db.drop_collection("docs").unwrap();
        assert!(matches!(db.import_status(8), Err(Error::ImportNotFound)));
        db.close().unwrap();
        let db = Database::open(&path).unwrap();
        assert_eq!(db.import_status(7).unwrap().state, ImportState::Finished);
        assert_eq!(db.collections().unwrap().len(), 1);
    }

    #[test]
    fn invalid_marker_sequences_written_by_a_buggy_caller_fail_closed_on_open() {
        let directory = tempfile::tempdir().unwrap();
        let rows = [put(1, &[1.0, 1.0], b"")];
        let cases: Vec<(&str, Vec<ImportMarker>)> = vec![
            ("gap", vec![chunk(1, 2, &rows, 0)]),
            (
                "rows",
                vec![ImportMarker {
                    rows_committed: 2,
                    ..chunk(1, 1, &rows, 0)
                }],
            ),
            (
                "after-close",
                vec![
                    chunk(1, 1, &rows, 0),
                    close_marker(1, 1, MARKER_ABORTED),
                    chunk(1, 2, &rows, 0),
                ],
            ),
            (
                "both-flags",
                vec![close_marker(1, 0, MARKER_ABORTED | MARKER_FINISHED)],
            ),
            ("close-chunk", vec![close_marker(1, 3, MARKER_FINISHED)]),
        ];
        for (name, markers) in cases {
            let path = directory.path().join(format!("{name}.vlite"));
            let mut db = v4_with_docs(&path);
            for marker in &markers {
                let data: &[Mutation] = if marker.flags == 0 { &rows } else { &[] };
                db.commit_import_frame(1, data, marker).unwrap();
            }
            drop(db);
            assert!(
                matches!(Database::open(&path), Err(Error::Corrupt { .. })),
                "{name} accepted"
            );
        }
    }

    #[test]
    fn v4_compaction_carries_open_jobs_as_snapshots_and_drops_closed_jobs() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.vlite");
        let mut db = v4_with_docs(&source);
        let first = [put(1, &[1.0, 1.0], b"a")];
        let second = [put(2, &[2.0, 2.0], b"b"), put(3, &[3.0, 3.0], b"c")];
        db.commit_import_frame(1, &first, &chunk(3, 1, &first, 1))
            .unwrap();
        db.commit_import_frame(1, &second, &chunk(3, 2, &second, 0))
            .unwrap();
        db.commit_import_frame(1, &first, &chunk(4, 1, &first, 0))
            .unwrap();
        db.commit_import_frame(1, &[], &close_marker(4, 1, MARKER_ABORTED))
            .unwrap();
        let lineage = db.lineage();
        db.close().unwrap();
        let output = directory.path().join("output.vlite");
        let report = maintenance::compact(&source, &output).unwrap();
        assert_eq!(report.output.format_version, 4);
        let detail = maintenance::inspect_detail(&output).unwrap();
        assert_eq!((detail.collections, detail.open_imports), (2, 1));
        assert_ne!(detail.lineage, lineage);
        let mut db = Database::open(&output).unwrap();
        let status = db.import_status(3).unwrap();
        assert_eq!(
            (
                status.state,
                status.last_chunk,
                status.rows_committed,
                status.rows_skipped
            ),
            (ImportState::Open, 2, 3, 1)
        );
        assert_eq!(status.last_digest, chunk(3, 2, &second, 0).digest);
        assert!(matches!(db.import_status(4), Err(Error::ImportNotFound)));
        let third = [put(4, &[4.0, 4.0], b"d")];
        db.commit_import_frame(1, &third, &chunk(3, 3, &third, 0))
            .unwrap();
        db.close().unwrap();
        let db = Database::open(&output).unwrap();
        assert_eq!(db.import_status(3).unwrap().rows_committed, 4);
        assert_eq!(db.collection("docs").unwrap().len(), 4);
    }

    #[test]
    fn candidate_restricted_exact_search_matches_full_scan_on_the_subset() {
        let directory = tempfile::tempdir().unwrap();
        let mut db = Database::create(directory.path().join("c.vlite"), 1).unwrap();
        for id in 0..20u64 {
            db.put(id, &[id as f32], b"").unwrap();
        }
        let subset: BTreeSet<u64> = [3, 7, 11, 99].into();
        let mut stats = SearchStats::default();
        let hits = db.default_state().exact_search_candidates(
            &[10.0],
            0.0,
            2,
            &[],
            DistanceMetric::SquaredL2,
            Some(&subset),
            current_time_ms(),
            &mut stats,
        );
        assert_eq!(hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), [11, 7]);
        assert_eq!((stats.records_scanned, stats.distance_evaluations), (3, 3));
    }
}
