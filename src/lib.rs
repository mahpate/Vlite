//! Embedded, single-file vector storage with synchronous per-operation commits.
//!
//! See the project README for the durability and explicit tail-repair contract.

mod format;
mod hnsw;
mod metadata;
mod rebuild;
pub use hnsw::{HnswConfig, MAX_EF_SEARCH, MAX_HNSW_BUFFER_IDS, MAX_HNSW_BUFFER_VECTOR_BYTES};
pub use metadata::{
    Condition, MAX_FILTER_CONDITIONS, MAX_METADATA_BYTES, MAX_METADATA_FIELDS, Metadata,
    MetadataValue,
};
pub use rebuild::{
    HnswBufferCapacity, HnswMaintenancePolicy, HnswMaintenanceReport, HnswMaintenanceStatus,
    HnswPressure, HnswRebuildLimits, HnswRebuildProgress, HnswRebuildStatus,
};
pub mod maintenance;
#[cfg(feature = "server")]
pub mod server;

use std::collections::{BTreeMap, BinaryHeap};
use std::fmt;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Seek, SeekFrom, Write};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};

pub const MAX_DIMENSIONS: usize = 65_536;
pub const MAX_PAYLOAD_BYTES: usize = 1_048_576;
pub const MAX_BATCH_OPERATIONS: usize = 4_096;
/// Maximum encoded batch body, including per-operation metadata.
pub const MAX_BATCH_BYTES: usize = 16_777_216;
pub type Result<T> = std::result::Result<T, Error>;

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
                "background HNSW snapshot exceeds configured record or vector-byte limit"
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

/// One exclusive database owner. All operations are serialized by the caller.
///
/// Reads use a rebuilt in-memory map. No persistent index or lock sidecar exists.
pub struct Database {
    file: LockedFile,
    version: u32,
    dimensions: usize,
    records: BTreeMap<u64, Record>,
    sequence: u64,
    end: u64,
    poisoned: bool,
    index: Option<hnsw::Index>,
    rebuild: Option<rebuild::Job>,
}

struct LockedFile(File);

impl Deref for LockedFile {
    type Target = File;

    fn deref(&self) -> &File {
        &self.0
    }
}

impl DerefMut for LockedFile {
    fn deref_mut(&mut self) -> &mut File {
        &mut self.0
    }
}

impl Drop for LockedFile {
    fn drop(&mut self) {
        // Explicit unlock also releases Unix locks briefly inherited by a
        // concurrent fork/exec. File close alone can leave those locks alive.
        // Destructors cannot return errors; close() reports unlock failures.
        let _ = self.0.unlock();
    }
}

fn lock(file: File) -> Result<LockedFile> {
    match file.try_lock() {
        Ok(()) => Ok(LockedFile(file)),
        Err(TryLockError::WouldBlock) => Err(Error::Locked),
        Err(TryLockError::Error(error)) => Err(Error::Io(error)),
    }
}

fn open_locked(path: &Path) -> Result<LockedFile> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    lock(file)
}

fn validate_dimensions(dimensions: usize) -> Result<()> {
    if !(1..=MAX_DIMENSIONS).contains(&dimensions) {
        return Err(Error::InvalidDimensions(dimensions));
    }
    Ok(())
}

impl Database {
    /// Creates a new database without overwriting any existing path.
    ///
    /// The file contents are synced before returning. The caller is responsible
    /// for platform-specific parent-directory durability after creation.
    pub fn create(path: impl AsRef<Path>, dimensions: usize) -> Result<Self> {
        validate_dimensions(dimensions)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        let mut file = lock(file)?;
        file.write_all(&format::header(dimensions))?;
        file.sync_all()?;
        Ok(Self {
            file,
            version: format::CURRENT_VERSION,
            dimensions,
            records: BTreeMap::new(),
            sequence: 0,
            end: format::HEADER_SIZE,
            poisoned: false,
            index: None,
            rebuild: None,
        })
    }

    /// Opens an existing database, validating every record without modifying it.
    ///
    /// Corrupt files and incomplete trailing writes are rejected, not truncated.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let mut file = open_locked(path.as_ref())?;
        let loaded = format::load(&mut file)?;
        if loaded.incomplete_bytes != 0 {
            return Err(Error::IncompleteTail {
                offset: loaded.end,
                bytes: loaded.incomplete_bytes,
            });
        }
        Ok(Self {
            file,
            version: loaded.version,
            dimensions: loaded.dimensions,
            records: loaded.records,
            sequence: loaded.sequence,
            end: loaded.end,
            poisoned: false,
            index: None,
            rebuild: None,
        })
    }

    /// Destructively discards only a structurally incomplete trailing frame.
    ///
    /// Back up the closed file first. Truncation of an acknowledged last commit
    /// cannot be distinguished from an interrupted write. Calling this method
    /// explicitly accepts that potential data loss. Complete corrupt frames
    /// are never repaired. A failed repair sync has an indeterminate outcome.
    pub fn discard_incomplete_tail(path: impl AsRef<Path>) -> Result<RepairReport> {
        let mut file = open_locked(path.as_ref())?;
        let loaded = format::load(&mut file)?;
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

    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    pub fn format_version(&self) -> u32 {
        self.version
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    fn ensure_healthy(&self) -> Result<()> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        Ok(())
    }

    fn validate_vector(&self, vector: &[f32]) -> Result<()> {
        if vector.len() != self.dimensions {
            return Err(Error::DimensionMismatch {
                expected: self.dimensions,
                actual: vector.len(),
            });
        }
        if vector.iter().any(|value| !value.is_finite()) {
            return Err(Error::NonFiniteVector);
        }
        Ok(())
    }

    /// Returns a record, or None when its ID does not exist.
    pub fn get(&self, id: u64) -> Result<Option<&Record>> {
        self.ensure_healthy()?;
        Ok(self.records.get(&id))
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
        self.ensure_healthy()?;
        self.validate_vector(vector)?;
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(Error::PayloadTooLarge(payload.len()));
        }
        metadata::validate(metadata)?;
        if !metadata.is_empty() && self.version < 3 {
            return Err(Error::MetadataRequiresUpgrade);
        }
        let record = Record {
            vector: vector.to_vec(),
            payload: payload.to_vec(),
            metadata: metadata.clone(),
        };
        self.commit(id, Some(&record))?;
        self.track_index_mutation(id, vector.len() * 4);
        self.records.insert(id, record);
        Ok(())
    }

    /// Durably deletes one record. Missing IDs return false without a commit.
    pub fn delete(&mut self, id: u64) -> Result<bool> {
        self.ensure_healthy()?;
        if !self.records.contains_key(&id) {
            return Ok(false);
        }
        self.commit(id, None)?;
        self.track_index_mutation(id, 0);
        self.records.remove(&id);
        Ok(true)
    }

    /// Validates all operations, appends one atomic frame, then syncs once.
    ///
    /// No operation becomes visible until the entire frame is durably committed.
    /// Duplicate IDs execute in order; an empty batch is a no-op on either format.
    /// Any write/sync failure is indeterminate for the entire batch and poisons
    /// this handle. A complete batch may reappear after an unacknowledged failure.
    pub fn write_batch(&mut self, operations: &[Mutation]) -> Result<()> {
        self.ensure_healthy()?;
        if operations.is_empty() {
            return Ok(());
        }
        if self.version < 2 {
            return Err(Error::BatchRequiresUpgrade);
        }
        if operations.len() > MAX_BATCH_OPERATIONS {
            return Err(Error::BatchTooLarge);
        }
        let mut body_bytes = 0usize;
        for operation in operations {
            let bytes = match operation {
                Mutation::PutWithMetadata {
                    vector,
                    payload,
                    metadata,
                    ..
                } => {
                    self.validate_vector(vector)?;
                    if payload.len() > MAX_PAYLOAD_BYTES {
                        return Err(Error::PayloadTooLarge(payload.len()));
                    }
                    let metadata_bytes = metadata::validate(metadata)?;
                    if self.version < 3 {
                        return Err(Error::MetadataRequiresUpgrade);
                    }
                    16 + vector.len() * 4 + 4 + payload.len() + metadata_bytes
                }
                Mutation::Put {
                    vector, payload, ..
                } => {
                    self.validate_vector(vector)?;
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
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or(Error::SequenceExhausted)?;
        let frame = format::batch_frame(sequence, operations, body_bytes);
        self.commit_frame(sequence, &frame)?;
        for operation in operations {
            let (id, bytes) = match operation {
                Mutation::Put { id, vector, .. } | Mutation::PutWithMetadata { id, vector, .. } => {
                    (*id, vector.len() * 4)
                }
                Mutation::Delete { id } => (*id, 0),
            };
            self.track_index_mutation(id, bytes);
            match operation {
                Mutation::Put {
                    id,
                    vector,
                    payload,
                } => {
                    self.records.insert(
                        *id,
                        Record {
                            vector: vector.clone(),
                            payload: payload.clone(),
                            metadata: Metadata::new(),
                        },
                    );
                }
                Mutation::PutWithMetadata {
                    id,
                    vector,
                    payload,
                    metadata,
                } => {
                    self.records.insert(
                        *id,
                        Record {
                            vector: vector.clone(),
                            payload: payload.clone(),
                            metadata: metadata.clone(),
                        },
                    );
                }
                Mutation::Delete { id } => {
                    self.records.remove(id);
                }
            }
        }
        Ok(())
    }

    fn commit(&mut self, id: u64, record: Option<&Record>) -> Result<()> {
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or(Error::SequenceExhausted)?;
        let frame = format::frame(sequence, id, record);
        self.commit_frame(sequence, &frame)
    }

    fn commit_frame(&mut self, sequence: u64, frame: &[u8]) -> Result<()> {
        self.commit_frame_with_sync(sequence, frame, File::sync_all)
    }

    fn commit_frame_with_sync(
        &mut self,
        sequence: u64,
        frame: &[u8],
        sync: impl FnOnce(&File) -> io::Result<()>,
    ) -> Result<()> {
        let result = append_and_sync(&mut self.file.0, self.end, frame, sync);
        if let Err(error) = result {
            self.poisoned = true;
            return Err(Error::CommitIndeterminate(error));
        }
        self.sequence = sequence;
        self.end += frame.len() as u64;
        Ok(())
    }

    fn track_index_mutation(&mut self, id: u64, current_bytes: usize) {
        if let Some(job) = &mut self.rebuild {
            job.track(id, self.records.get(&id), current_bytes);
        }
        if let Some(index) = &mut self.index
            && !index.track(id, self.records.get(&id), current_bytes)
        {
            self.index = None;
        }
    }

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
        self.ensure_healthy()?;
        config.validate()?;
        validate_ann_metric(metric)?;
        if let Some(job) = &mut self.rebuild {
            job.stop(HnswRebuildStatus::Superseded);
        }
        self.index = Some(hnsw::Index::build(&self.records, config, metric));
        Ok(())
    }

    /// Copies a bounded committed-vector snapshot, then builds on one owned thread.
    ///
    /// Snapshot copying is synchronous; graph construction is not. The current
    /// graph is retained until normal mutation-buffer overflow. No automatic
    /// fallback, retry, polling or publication occurs in search/write methods.
    /// Use ready-only queries and poll explicitly to avoid synchronous builds.
    pub fn start_hnsw_rebuild(
        &mut self,
        config: HnswConfig,
        metric: DistanceMetric,
        limits: HnswRebuildLimits,
    ) -> Result<()> {
        self.ensure_healthy()?;
        config.validate()?;
        validate_ann_metric(metric)?;
        limits.validate(self.records.len(), self.dimensions)?;
        if self.rebuild.is_some() {
            return Err(Error::HnswRebuildBusy);
        }
        self.rebuild = Some(rebuild::Job::start(&self.records, config, metric)?);
        Ok(())
    }

    /// Nonblocking poll. A completed worker is joined and atomically published
    /// with bounded committed-mutation catch-up, or explicitly discarded.
    /// Catch-up has the same 256-ID / 8-MiB vector limits as the active graph.
    /// Overflow never rejects durable writes or publishes stale snapshot data.
    pub fn poll_hnsw_rebuild(&mut self) -> Result<HnswRebuildStatus> {
        self.ensure_healthy()?;
        let Some(job) = &self.rebuild else {
            return Ok(HnswRebuildStatus::Idle);
        };
        if !job.finished() {
            return Ok(job.status());
        }
        let (status, index) = self.rebuild.take().expect("pending job").finish()?;
        if let Some(index) = index {
            self.index = Some(index);
        }
        Ok(status)
    }

    /// Requests cooperative cancellation. Poll until Cancelled (or another
    /// already-selected terminal reason). Returns false if no job is pending.
    /// Cancellation never waits or replaces the active graph.
    pub fn cancel_hnsw_rebuild(&mut self) -> bool {
        if let Some(job) = &mut self.rebuild {
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
        self.ensure_healthy()?;
        config.validate()?;
        validate_ann_metric(metric)?;
        policy.validate()?;
        let active_buffer = self
            .index
            .as_ref()
            .filter(|index| index.config == config && index.metric == metric)
            .map(HnswBufferCapacity::of);
        let rebuild = self.rebuild.as_ref().map(rebuild::Job::progress);
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
        let before = self.hnsw_maintenance_status(config, metric, policy)?;
        if before
            .rebuild
            .is_some_and(|job| job.config != config || job.metric != metric)
        {
            return Err(Error::HnswRebuildBusy);
        }
        let outcome = self.poll_hnsw_rebuild()?;
        let status = self.hnsw_maintenance_status(config, metric, policy)?;
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
            self.start_hnsw_rebuild(config, metric, policy.limits)?;
        }
        Ok(HnswMaintenanceReport {
            outcome,
            started: start,
            status: self.hnsw_maintenance_status(config, metric, policy)?,
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
        self.validate_hnsw_query(query, k, ef_search, config, conditions, metric)?;
        if k == 0 || self.records.is_empty() {
            return Ok(Vec::new());
        }
        if self
            .index
            .as_ref()
            .is_none_or(|index| index.config != config || index.metric != metric)
        {
            self.build_hnsw_with_metric(config, metric)?;
        }
        Ok(self.index.as_ref().expect("built index").search_filtered(
            query,
            k,
            ef_search,
            &self.records,
            conditions,
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
        self.validate_hnsw_query(query, k, ef_search, config, conditions, metric)?;
        if k == 0 || self.records.is_empty() {
            return Ok(Vec::new());
        }
        let index = self
            .index
            .as_ref()
            .filter(|index| index.config == config && index.metric == metric)
            .ok_or(Error::IndexNotReady)?;
        Ok(index.search_filtered(query, k, ef_search, &self.records, conditions))
    }

    fn validate_hnsw_query(
        &self,
        query: &[f32],
        k: usize,
        ef_search: usize,
        config: HnswConfig,
        conditions: &[Condition],
        metric: DistanceMetric,
    ) -> Result<()> {
        self.ensure_healthy()?;
        self.validate_vector(query)?;
        metadata::validate_filter(conditions)?;
        config.validate()?;
        validate_ann_metric(metric)?;
        validate_metric_query(query, metric)?;
        if !(1..=MAX_EF_SEARCH).contains(&ef_search) || ef_search < k.min(self.records.len()) {
            return Err(Error::InvalidEfSearch);
        }
        Ok(())
    }

    /// Exact squared-L2 nearest neighbors, ordered by distance then ascending ID.
    ///
    /// k=0 returns an empty result; k above the live count returns all records.
    /// Query vectors are validated even when k=0 or the database is empty.
    pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<SearchHit>> {
        self.search_filtered(query, k, &[])
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
        self.ensure_healthy()?;
        self.validate_vector(query)?;
        metadata::validate_filter(conditions)?;
        let query_norm = validate_metric_query(query, metric)?;
        if k == 0 {
            return Ok(Vec::new());
        }
        let limit = k.min(self.records.len());
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut hits: BinaryHeap<RankedHit> = BinaryHeap::with_capacity(limit);
        'records: for (&id, record) in &self.records {
            if !metadata::matches(&record.metadata, conditions) {
                continue;
            }
            let distance = if metric != DistanceMetric::SquaredL2 {
                let norm = if metric == DistanceMetric::Cosine {
                    vector_norm(&record.vector)
                } else {
                    0.0
                };
                let Some(distance) =
                    metric_distance(&record.vector, query, metric, norm, query_norm)
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
        Ok(hits
            .into_sorted_vec()
            .into_iter()
            .map(|hit| hit.0)
            .collect())
    }

    /// Consumes the handle and releases its OS lock. Commits are already synced.
    ///
    /// Dropping the handle is also safe: there is no buffered uncommitted state.
    pub fn close(mut self) -> Result<()> {
        self.ensure_healthy()?;
        if let Some(mut job) = self.rebuild.take() {
            job.stop(HnswRebuildStatus::Cancelled);
            job.finish()?;
        }
        self.file.unlock()?;
        Ok(())
    }
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
            assert_eq!(db.index.as_ref().unwrap().buffer_usage().0, id as usize + 1);
        }
        let before = db.index.as_ref().unwrap().buffer_usage();
        assert!(db.put(999, &[], b"").is_err());
        db.write_batch(&[]).unwrap();
        assert!(!db.delete(999).unwrap());
        assert_eq!(db.index.as_ref().unwrap().buffer_usage(), before);
        db.put(0, &[-1.0], b"").unwrap();
        assert_eq!(db.index.as_ref().unwrap().buffer_usage(), before);
        db.put(1000, &[-2.0], b"").unwrap();
        assert!(db.index.is_none());
        assert_eq!(
            db.search_approximate(&[-2.0], 1, 100, HnswConfig::default())
                .unwrap()[0]
                .id,
            1000
        );
        assert_eq!(db.index.as_ref().unwrap().buffer_usage(), (0, 0));
        db.delete(1000).unwrap();
        db.build_hnsw(HnswConfig::default()).unwrap();
        assert_eq!(db.index.as_ref().unwrap().buffer_usage(), (0, 0));
        db.file = LockedFile(File::open(directory.path().join("buffer.vlite")).unwrap());
        assert!(matches!(
            db.put(999, &[0.0], b""),
            Err(Error::CommitIndeterminate(_))
        ));
        assert_eq!(db.index.as_ref().unwrap().buffer_usage(), (0, 0));
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
        db.file = LockedFile(File::open(&path).unwrap());
        assert!(matches!(
            db.put(1, &[2.0], b"replacement"),
            Err(Error::CommitIndeterminate(_))
        ));
        assert_eq!(db.sequence, 1);
        assert_eq!(db.records.get(&1).unwrap().payload, b"original");
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
        assert_eq!(db.file.metadata().unwrap().len(), format::HEADER_SIZE);
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
        db.file = LockedFile(File::open(&path).unwrap());
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
        assert_eq!(db.records.len(), 1);
        assert_eq!(db.records.get(&1).unwrap().payload, b"old");
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
