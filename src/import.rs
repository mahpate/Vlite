//! Resumable chunked import (format v4 only, REQ-009/REQ-010).
//!
//! Each chunk commits its accepted rows plus one checkpoint marker (batch
//! entry op 8) in a single atomic batch frame that is synced once. The marker
//! records the job, a contiguous chunk number, row counts and the full
//! SHA-256 identity of the submitted chunk, so a resubmitted chunk is
//! recognized after a crash or reopen and never applied twice.

use std::borrow::Cow;
use std::collections::{BTreeMap, VecDeque};

use sha2::{Digest, Sha256};

use crate::collection::CollectionMut;
use crate::format::{self, ImportMarker, LoadedImportMarker};
use crate::{Database, Error, MAX_BATCH_OPERATIONS, MAX_PAYLOAD_BYTES, Mutation, Result, metadata};

/// Maximum simultaneously open import jobs per database.
pub const MAX_OPEN_IMPORTS: usize = 64;
/// Closed jobs whose status remains queryable on one handle.
pub const MAX_CLOSED_IMPORTS: usize = 1024;

/// How invalid rows in a chunk are handled. Encoded as the digest policy byte.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InvalidRowPolicy {
    /// Any invalid row rejects the whole chunk without a commit.
    #[default]
    RejectChunk,
    /// Invalid rows are counted in `rows_skipped`; valid rows commit.
    SkipInvalid,
}

impl InvalidRowPolicy {
    pub(crate) fn code(self) -> u8 {
        match self {
            Self::RejectChunk => 0,
            Self::SkipInvalid => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportState {
    Open,
    Finished,
    Aborted,
}

/// Durable progress of one import job, rebuilt from checkpoint markers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportStatus {
    pub job: u64,
    /// Name of the collection the job was bound to by its first chunk. It is
    /// historical: a later collection may reuse the name (never the ID).
    pub collection: String,
    /// True once that collection was dropped (only possible after the job
    /// closed). Its name may now belong to a different collection (with a
    /// different ID), which this job never touched.
    pub collection_dropped: bool,
    pub state: ImportState,
    /// Last committed chunk number (0 when none).
    pub last_chunk: u64,
    /// Cumulative rows committed (saturating).
    pub rows_committed: u64,
    /// Cumulative rows skipped (saturating).
    pub rows_skipped: u64,
    /// SHA-256 identity of the last committed chunk; zero when none.
    pub last_digest: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct JobState {
    pub collection: u16,
    /// The bound collection was dropped; `collection` is history only and
    /// must never be matched against a live collection with the same ID.
    pub collection_dropped: bool,
    pub state: ImportState,
    pub last_chunk: u64,
    pub rows_committed: u64,
    pub rows_skipped: u64,
    pub last_digest: [u8; 32],
}

/// In-memory registry of import jobs. Bounded: closed jobs beyond
/// `MAX_CLOSED_IMPORTS` are forgotten oldest-first.
#[derive(Debug, Default)]
pub(crate) struct Registry {
    jobs: BTreeMap<u64, JobState>,
    /// Collection name at job creation, evicted together with the job.
    names: BTreeMap<u64, String>,
    closed_order: VecDeque<u64>,
    /// Highest job ID ever observed; new IDs are allocated above it.
    max_job: u64,
}

impl Registry {
    pub(crate) fn from_markers(markers: &[LoadedImportMarker]) -> Self {
        let mut registry = Self::default();
        for event in markers {
            match event {
                LoadedImportMarker::Marker {
                    collection,
                    name,
                    marker,
                } => registry.apply(*collection, name, marker),
                LoadedImportMarker::CollectionDropped(collection) => {
                    registry.collection_dropped(*collection);
                }
            }
        }
        registry
    }

    /// Applies one already-validated (load) or just-committed marker.
    /// `name` is the name of `collection` at that point in the log.
    pub(crate) fn apply(&mut self, collection: u16, name: &str, marker: &ImportMarker) {
        self.max_job = self.max_job.max(marker.job);
        if !self.jobs.contains_key(&marker.job) {
            self.names.insert(marker.job, name.to_owned());
        }
        let job = self.jobs.entry(marker.job).or_insert(JobState {
            collection,
            collection_dropped: false,
            state: ImportState::Open,
            last_chunk: 0,
            rows_committed: 0,
            rows_skipped: 0,
            last_digest: [0; 32],
        });
        if marker.flags & format::MARKER_FINISHED != 0 {
            job.state = ImportState::Finished;
        } else if marker.flags & format::MARKER_ABORTED != 0 {
            job.state = ImportState::Aborted;
        } else {
            job.last_chunk = marker.chunk;
            // Callers validated the sum (loader: Corrupt; import_chunk:
            // ImportConflict before writing), so this never clamps.
            job.rows_committed = job
                .rows_committed
                .checked_add(marker.rows_committed)
                .expect("validated import row count");
            job.rows_skipped = job
                .rows_skipped
                .checked_add(marker.rows_skipped)
                .expect("validated import row count");
            job.last_digest = marker.digest;
        }
        if job.state != ImportState::Open {
            self.closed_order.push_back(marker.job);
            if self.closed_order.len() > MAX_CLOSED_IMPORTS {
                let evicted = self.closed_order.pop_front().expect("nonempty");
                self.jobs.remove(&evicted);
                self.names.remove(&evicted);
            }
        }
    }

    /// Detaches every job bound to a just-dropped collection (all closed:
    /// open jobs block drops), so a later collection reusing the ID is never
    /// attributed to them.
    pub(crate) fn collection_dropped(&mut self, collection: u16) {
        for job in self.jobs.values_mut() {
            if job.collection == collection {
                job.collection_dropped = true;
            }
        }
    }

    pub(crate) fn get(&self, job: u64) -> Option<&JobState> {
        self.jobs.get(&job)
    }

    /// Bound collection's name at job creation.
    pub(crate) fn collection_name(&self, job: u64) -> &str {
        self.names.get(&job).map_or("", String::as_str)
    }

    /// Whether `job` is bound to the live collection `collection`.
    fn bound_to(state: &JobState, collection: u16) -> bool {
        !state.collection_dropped && state.collection == collection
    }

    pub(crate) fn open_jobs(&self) -> impl Iterator<Item = (u64, &JobState)> {
        self.jobs
            .iter()
            .filter(|(_, job)| job.state == ImportState::Open)
            .map(|(&id, job)| (id, job))
    }

    pub(crate) fn has_open_in(&self, collection: u16) -> bool {
        self.open_jobs()
            .any(|(_, job)| Self::bound_to(job, collection))
    }

    /// Next unused job ID, or None when IDs are exhausted.
    pub(crate) fn next_job_id(&self) -> Option<u64> {
        self.max_job.checked_add(1)
    }

    /// Only IDs above every ID ever observed are provably unused: closed jobs
    /// may be evicted from memory, while the log still holds their markers.
    fn is_new_job(&self, job: u64) -> bool {
        job > self.max_job
    }

    fn open_count(&self) -> usize {
        self.open_jobs().count()
    }
}

/// Exactly-once identity of a chunk whose rows are all valid: full SHA-256
/// over the canonical batch-entry encoding followed by the policy byte.
#[cfg(test)]
pub(crate) fn chunk_digest(operations: &[crate::Mutation], policy: InvalidRowPolicy) -> [u8; 32] {
    format::import_digest(operations, policy.code())
}

/// Cumulative open-job state as a compaction snapshot marker (60-byte layout:
/// full u64 row counts, never clamped).
pub(crate) fn snapshot_marker(job: u64, state: &JobState) -> ImportMarker {
    ImportMarker {
        job,
        chunk: state.last_chunk,
        rows_committed: state.rows_committed,
        rows_skipped: state.rows_skipped,
        flags: format::MARKER_SNAPSHOT,
        digest: state.last_digest,
    }
}

/// Caller- or server-chosen import job identifier. Must be nonzero; a new
/// job must use an ID above every job ID previously used in the file
/// (`Database::next_import_job_id`).
pub type ImportJobId = u64;

/// At most this many invalid rows are listed per chunk report; the total
/// is always reported in `ImportChunkReport::invalid_rows`.
pub const MAX_REPORTED_INVALID_ROWS: usize = 100;

/// Result of one `import_chunk` call.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportChunkOutcome {
    /// The chunk's accepted rows and its checkpoint were durably committed.
    Committed,
    /// The identical chunk (same rows and policy) is the job's last durable
    /// chunk. Nothing was written.
    AlreadyCommitted,
    /// `RejectChunk` found invalid rows. Nothing was written and the job did
    /// not advance; fix the rows and resubmit the same chunk number.
    Rejected,
}

/// One invalid input row. `message` never contains vectors, payloads or
/// metadata values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidRow {
    /// Zero-based index in the submitted operations.
    pub row: u32,
    /// Stable machine-readable code: `dimension_mismatch`,
    /// `non_finite_vector`, `payload_too_large` or `invalid_metadata`.
    pub code: &'static str,
    pub message: String,
}

/// Per-chunk import report (REQ-010).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportChunkReport {
    pub outcome: ImportChunkOutcome,
    pub job: ImportJobId,
    pub chunk: u64,
    /// Rows applied by this chunk (0 when `Rejected`).
    pub rows_committed: u32,
    /// Invalid rows skipped under `SkipInvalid` (0 under `RejectChunk`).
    pub rows_skipped: u32,
    /// Total invalid rows in the submission.
    pub invalid_rows: u32,
    /// The first `MAX_REPORTED_INVALID_ROWS` invalid rows in input order.
    pub invalid: Vec<InvalidRow>,
    /// Commit sequence (the revision of every row put by this chunk) when
    /// `Committed`; None otherwise.
    pub revision: Option<u64>,
}

fn invalid_code(error: &Error) -> &'static str {
    match error {
        Error::DimensionMismatch { .. } => "dimension_mismatch",
        Error::NonFiniteVector => "non_finite_vector",
        Error::PayloadTooLarge(_) => "payload_too_large",
        _ => "invalid_metadata",
    }
}

/// The same per-row checks `write_batch` applies, including v5-only geo values.
fn validate_row(dimensions: usize, operation: &Mutation, version: u32) -> Result<()> {
    let (vector, payload, metadata) = match operation {
        Mutation::Put {
            vector, payload, ..
        } => (vector, payload, None),
        Mutation::PutWithMetadata {
            vector,
            payload,
            metadata,
            ..
        } => (vector, payload, Some(metadata)),
        Mutation::Delete { .. } => return Ok(()),
    };
    crate::validate_vector(dimensions, vector)?;
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(Error::PayloadTooLarge(payload.len()));
    }
    if let Some(metadata) = metadata {
        metadata::validate(metadata)?;
        if metadata::contains_geo(metadata) && version < format::V5 {
            return Err(Error::ExpiryRequiresUpgrade);
        }
    }
    Ok(())
}

/// Escape entry for an invalid row in the digest stream. Op 0xFF never
/// occurs in a canonical entry, and every length is a u64, so the stream
/// stays injective even for rows whose canonical encoding would not be
/// (wrong vector length, oversized fields).
const INVALID_ENTRY_OP: u8 = 0xff;

fn encode_invalid_row(operation: &Mutation, bytes: &mut Vec<u8>) {
    fn length(bytes: &mut Vec<u8>, len: usize) {
        bytes.extend_from_slice(&(len as u64).to_le_bytes());
    }
    bytes.extend_from_slice(&[INVALID_ENTRY_OP, 0, 0, 0]);
    let (kind, id, vector, payload, metadata) = match operation {
        Mutation::Put {
            id,
            vector,
            payload,
        } => (1u8, id, vector, payload, None),
        Mutation::PutWithMetadata {
            id,
            vector,
            payload,
            metadata,
        } => (4, id, vector, payload, Some(metadata)),
        Mutation::Delete { id } => {
            bytes.push(2);
            bytes.extend_from_slice(&id.to_le_bytes());
            return;
        }
    };
    bytes.push(kind);
    bytes.extend_from_slice(&id.to_le_bytes());
    length(bytes, vector.len());
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    length(bytes, payload.len());
    bytes.extend_from_slice(payload);
    let Some(metadata) = metadata else { return };
    length(bytes, metadata.len());
    for (key, value) in metadata {
        length(bytes, key.len());
        bytes.extend_from_slice(key.as_bytes());
        match value {
            crate::MetadataValue::String(text) => {
                bytes.push(1);
                length(bytes, text.len());
                bytes.extend_from_slice(text.as_bytes());
            }
            crate::MetadataValue::Integer(value) => {
                bytes.push(2);
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            crate::MetadataValue::Float(value) => {
                bytes.push(3);
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            crate::MetadataValue::Boolean(value) => bytes.extend_from_slice(&[4, u8::from(*value)]),
            crate::MetadataValue::GeoPoint {
                latitude,
                longitude,
            } => {
                bytes.push(5);
                bytes.extend_from_slice(&latitude.to_le_bytes());
                bytes.extend_from_slice(&longitude.to_le_bytes());
            }
        }
    }
}

/// Full SHA-256 chunk identity over every submitted operation and the policy
/// byte. With all rows valid it is exactly the canonical REQ-009 digest;
/// invalid rows contribute an injective escape entry instead.
fn submitted_digest(operations: &[Mutation], valid: &[bool], policy: InvalidRowPolicy) -> [u8; 32] {
    if valid.iter().all(|&valid| valid) {
        return format::import_digest(operations, policy.code());
    }
    let mut hasher = Sha256::new();
    let mut bytes = Vec::new();
    for (operation, &valid) in operations.iter().zip(valid) {
        bytes.clear();
        if valid {
            format::encode_batch_entries(std::slice::from_ref(operation), &mut bytes);
        } else {
            encode_invalid_row(operation, &mut bytes);
        }
        hasher.update(&bytes);
    }
    hasher.update([policy.code()]);
    hasher.finalize().into()
}

impl Database {
    fn ensure_imports(&self) -> Result<()> {
        self.ensure_healthy()?;
        if self.version < format::V4 {
            return Err(Error::ImportsRequireUpgrade);
        }
        Ok(())
    }

    /// Smallest job ID a new import may use: one above every job ID already
    /// used in this file (closed and evicted jobs included). v4 only.
    pub fn next_import_job_id(&self) -> Result<ImportJobId> {
        self.ensure_imports()?;
        self.imports
            .next_job_id()
            .ok_or(Error::ImportConflict("import job IDs are exhausted"))
    }

    /// Imports one chunk into the `default` collection; see
    /// [`CollectionMut::import_chunk`].
    pub fn import_chunk(
        &mut self,
        job: ImportJobId,
        chunk: u64,
        operations: &[Mutation],
        policy: InvalidRowPolicy,
    ) -> Result<ImportChunkReport> {
        self.import_chunk_in(0, job, chunk, operations, policy)
    }

    /// Finishes a `default`-collection job; see [`CollectionMut::finish_import`].
    pub fn finish_import(&mut self, job: ImportJobId) -> Result<ImportStatus> {
        self.close_import_in(0, job, ImportState::Finished)
    }

    /// Aborts a `default`-collection job; see [`CollectionMut::abort_import`].
    pub fn abort_import(&mut self, job: ImportJobId) -> Result<ImportStatus> {
        self.close_import_in(0, job, ImportState::Aborted)
    }

    fn import_chunk_in(
        &mut self,
        collection: u16,
        job: ImportJobId,
        chunk: u64,
        operations: &[Mutation],
        policy: InvalidRowPolicy,
    ) -> Result<ImportChunkReport> {
        self.ensure_imports()?;
        if job == 0 {
            return Err(Error::ImportConflict("import job IDs must be nonzero"));
        }
        if chunk == 0 {
            return Err(Error::ImportConflict("import chunk numbers start at 1"));
        }
        if operations.len() > MAX_BATCH_OPERATIONS {
            return Err(Error::BatchTooLarge);
        }
        let dimensions = self.state(collection).dimensions;
        let mut valid = Vec::with_capacity(operations.len());
        let mut invalid = Vec::new();
        let mut invalid_rows = 0u32;
        for (row, operation) in operations.iter().enumerate() {
            let result = validate_row(dimensions, operation, self.version);
            if let Err(error) = &result {
                invalid_rows += 1;
                if invalid.len() < MAX_REPORTED_INVALID_ROWS {
                    invalid.push(InvalidRow {
                        row: row as u32,
                        code: invalid_code(error),
                        message: error.to_string(),
                    });
                }
            }
            valid.push(result.is_ok());
        }
        let digest = submitted_digest(operations, &valid, policy);
        let accepted = operations.len() as u32 - invalid_rows;
        let mut report = ImportChunkReport {
            outcome: ImportChunkOutcome::Committed,
            job,
            chunk,
            rows_committed: 0,
            rows_skipped: 0,
            invalid_rows,
            invalid,
            revision: None,
        };
        match self.imports.get(job).copied() {
            Some(state) => {
                if !Registry::bound_to(&state, collection) {
                    return Err(Error::ImportConflict(
                        "import job belongs to another collection",
                    ));
                }
                if chunk == state.last_chunk && digest == state.last_digest {
                    // A committed chunk never had invalid rows under RejectChunk.
                    report.outcome = ImportChunkOutcome::AlreadyCommitted;
                    report.rows_committed = accepted;
                    report.rows_skipped = invalid_rows;
                    return Ok(report);
                }
                if state.state != ImportState::Open {
                    return Err(Error::ImportConflict("import job is closed"));
                }
                if chunk == state.last_chunk {
                    return Err(Error::ImportConflict(
                        "chunk differs from the committed chunk with this number",
                    ));
                }
                if Some(chunk) != state.last_chunk.checked_add(1) {
                    return Err(Error::ImportConflict(
                        "chunk is not the next chunk of this job",
                    ));
                }
                if state
                    .rows_committed
                    .checked_add(u64::from(accepted))
                    .is_none()
                    || state
                        .rows_skipped
                        .checked_add(u64::from(invalid_rows))
                        .is_none()
                {
                    return Err(Error::ImportConflict(
                        "import job row counts would overflow u64",
                    ));
                }
            }
            None => {
                if !self.imports.is_new_job(job) {
                    return Err(Error::ImportConflict(
                        "import job ID was already used in this file",
                    ));
                }
                if chunk != 1 {
                    return Err(Error::ImportConflict("a new import job starts at chunk 1"));
                }
                if self.imports.open_count() >= MAX_OPEN_IMPORTS {
                    return Err(Error::TooManyImports);
                }
            }
        }
        if invalid_rows != 0 && policy == InvalidRowPolicy::RejectChunk {
            report.outcome = ImportChunkOutcome::Rejected;
            return Ok(report);
        }
        let rows: Cow<'_, [Mutation]> = if invalid_rows == 0 {
            Cow::Borrowed(operations)
        } else {
            Cow::Owned(
                operations
                    .iter()
                    .zip(&valid)
                    .filter(|(_, valid)| **valid)
                    .map(|(operation, _)| operation.clone())
                    .collect(),
            )
        };
        let marker = ImportMarker {
            job,
            chunk,
            rows_committed: u64::from(accepted),
            rows_skipped: u64::from(invalid_rows),
            flags: 0,
            digest,
        };
        let sequence = self.commit_import_frame(collection, &rows, &marker)?;
        report.rows_committed = accepted;
        report.rows_skipped = invalid_rows;
        report.revision = Some(sequence);
        Ok(report)
    }

    fn close_import_in(
        &mut self,
        collection: u16,
        job: ImportJobId,
        target: ImportState,
    ) -> Result<ImportStatus> {
        self.ensure_imports()?;
        let state = self
            .imports
            .get(job)
            .copied()
            .ok_or(Error::ImportNotFound)?;
        if !Registry::bound_to(&state, collection) {
            return Err(Error::ImportConflict(
                "import job belongs to another collection",
            ));
        }
        if state.state == target {
            return self.import_status(job);
        }
        if state.state != ImportState::Open {
            return Err(Error::ImportConflict("import job is closed"));
        }
        let marker = ImportMarker {
            job,
            chunk: state.last_chunk,
            rows_committed: 0,
            rows_skipped: 0,
            flags: if target == ImportState::Finished {
                format::MARKER_FINISHED
            } else {
                format::MARKER_ABORTED
            },
            digest: [0; 32],
        };
        self.commit_import_frame(collection, &[], &marker)?;
        self.import_status(job)
    }
}

impl CollectionMut<'_> {
    /// Commits one chunk of an import job into this collection (v4 only).
    ///
    /// Bounds: ≤4,096 operations (`BatchTooLarge`), accepted rows ≤16 MiB
    /// encoded; at most 64 open jobs (`TooManyImports`). A job is bound to
    /// the collection of its first chunk; chunks are numbered from 1.
    ///
    /// - `chunk == last + 1` validates every row; with `SkipInvalid` the valid
    ///   rows and a checkpoint commit in one atomic frame synced once, with
    ///   `RejectChunk` any invalid row returns `Rejected` and writes nothing.
    /// - `chunk == last` with the identical rows and policy (full SHA-256)
    ///   returns `AlreadyCommitted` without writing, also after reopen,
    ///   compaction of an open job, or finish/abort.
    /// - Anything else returns `ImportConflict` without writing.
    ///
    /// A write or sync failure returns `CommitIndeterminate` and poisons the
    /// handle; after reopen, resubmitting the chunk is safe either way.
    pub fn import_chunk(
        &mut self,
        job: ImportJobId,
        chunk: u64,
        operations: &[Mutation],
        policy: InvalidRowPolicy,
    ) -> Result<ImportChunkReport> {
        self.db
            .import_chunk_in(self.id, job, chunk, operations, policy)
    }

    /// Durably marks an open job finished with a marker-only frame. Repeating
    /// it returns the status without writing; an aborted job conflicts.
    pub fn finish_import(&mut self, job: ImportJobId) -> Result<ImportStatus> {
        self.db.close_import_in(self.id, job, ImportState::Finished)
    }

    /// Durably marks an open job aborted. Committed chunks are not rolled
    /// back. Repeating it returns the status without writing.
    pub fn abort_import(&mut self, job: ImportJobId) -> Result<ImportStatus> {
        self.db.close_import_in(self.id, job, ImportState::Aborted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FormatVersion, LockedFile};
    use std::fs::File;

    fn put(id: u64, vector: &[f32]) -> Mutation {
        Mutation::Put {
            id,
            vector: vector.to_vec(),
            payload: Vec::new(),
        }
    }

    #[test]
    fn all_valid_digest_is_the_canonical_digest_and_invalid_rows_are_escaped() {
        let rows = [put(1, &[1.0, 2.0]), Mutation::Delete { id: 3 }];
        let policy = InvalidRowPolicy::SkipInvalid;
        let canonical = chunk_digest(&rows, policy);
        assert_eq!(submitted_digest(&rows, &[true, true], policy), canonical);
        assert_ne!(submitted_digest(&rows, &[false, true], policy), canonical);
        assert_ne!(
            submitted_digest(&rows, &[true, true], InvalidRowPolicy::RejectChunk),
            canonical
        );
        // A 4-d put on a 2-d collection whose tail spells `Delete { id: 3 }`
        // has the same raw canonical bytes as `rows`; the escape separates it.
        let tail = |b: [u8; 4]| f32::from_le_bytes(b);
        let crafted = [put(
            1,
            &[
                1.0,
                2.0,
                tail([2, 0, 0, 0]),
                tail([3, 0, 0, 0]),
                tail([0; 4]),
                tail([0; 4]),
            ],
        )];
        assert_eq!(chunk_digest(&crafted, policy), canonical);
        assert_ne!(submitted_digest(&crafted, &[false], policy), canonical);
    }

    #[test]
    fn failed_chunk_write_poisons_the_handle_and_commits_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("poison.vlite");
        let mut db = Database::create_with_format(&path, 2, FormatVersion::V4).unwrap();
        let first = [put(1, &[1.0, 1.0])];
        db.import_chunk(1, 1, &first, InvalidRowPolicy::RejectChunk)
            .unwrap();
        let durable = std::fs::metadata(&path).unwrap().len();
        // A read-only descriptor deterministically fails the next write.
        db.file = LockedFile::disk(File::open(&path).unwrap());
        let second = [put(2, &[2.0, 2.0])];
        assert!(matches!(
            db.import_chunk(1, 2, &second, InvalidRowPolicy::RejectChunk),
            Err(Error::CommitIndeterminate(_))
        ));
        // Neither the rows nor the checkpoint were applied in memory.
        assert_eq!(db.imports.get(1).unwrap().last_chunk, 1);
        assert!(!db.default_state().records.contains_key(&2));
        assert!(matches!(db.import_status(1), Err(Error::Poisoned)));
        assert!(matches!(
            db.import_chunk(1, 2, &second, InvalidRowPolicy::RejectChunk),
            Err(Error::Poisoned)
        ));
        assert!(matches!(db.finish_import(1), Err(Error::Poisoned)));
        assert!(matches!(db.next_import_job_id(), Err(Error::Poisoned)));
        drop(db);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), durable);
        let mut db = Database::open(&path).unwrap();
        assert_eq!(db.import_status(1).unwrap().last_chunk, 1);
        let report = db
            .import_chunk(1, 2, &second, InvalidRowPolicy::RejectChunk)
            .unwrap();
        assert_eq!(report.outcome, ImportChunkOutcome::Committed);
        assert_eq!(db.len(), 2);
    }

    #[test]
    fn closed_jobs_evicted_from_memory_still_block_id_reuse() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("evict.vlite");
        let mut db = Database::create_with_format(&path, 1, FormatVersion::V4).unwrap();
        for job in 1..=(MAX_CLOSED_IMPORTS as u64 + 1) {
            db.import_chunk(job, 1, &[], InvalidRowPolicy::RejectChunk)
                .unwrap();
            db.finish_import(job).unwrap();
        }
        assert!(db.imports.get(1).is_none(), "oldest closed job evicted");
        assert!(matches!(db.import_status(1), Err(Error::ImportNotFound)));
        // Reusing it would append a marker the loader rejects after closure.
        assert!(matches!(
            db.import_chunk(1, 1, &[], InvalidRowPolicy::RejectChunk),
            Err(Error::ImportConflict(_))
        ));
        drop(db);
        assert!(Database::open(&path).is_ok());
    }
}
