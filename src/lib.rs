//! Embedded, single-file vector storage with synchronous per-operation commits.
//!
//! See the project README for the durability and explicit tail-repair contract.

mod format;
mod hnsw;
pub use hnsw::{HnswConfig, MAX_EF_SEARCH};
pub mod maintenance;
#[cfg(feature = "server")]
pub mod server;

use std::collections::BTreeMap;
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
    PayloadTooLarge(usize),
    BatchTooLarge,
    BatchRequiresUpgrade,
    InvalidHnswConfig,
    InvalidEfSearch,
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
            Self::InvalidEfSearch => write!(
                f,
                "ef_search must be 1..={MAX_EF_SEARCH} and at least min(k, live records)"
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
}

/// One operation in an atomic batch. Operations execute in slice order.
#[derive(Clone, Debug, PartialEq)]
pub enum Mutation {
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
    /// Squared Euclidean distance, accumulated in f64. Smaller is nearer.
    pub distance: f64,
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

    /// Durably inserts or replaces one record, including its opaque payload.
    pub fn put(&mut self, id: u64, vector: &[f32], payload: &[u8]) -> Result<()> {
        self.ensure_healthy()?;
        self.validate_vector(vector)?;
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(Error::PayloadTooLarge(payload.len()));
        }
        let record = Record {
            vector: vector.to_vec(),
            payload: payload.to_vec(),
        };
        self.commit(id, Some(&record))?;
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
        self.index = None;
        Ok(())
    }

    /// Builds/replaces the optional in-memory HNSW index from committed live records.
    ///
    /// Successful mutations invalidate it. Nothing is written to the database file.
    pub fn build_hnsw(&mut self, config: HnswConfig) -> Result<()> {
        self.ensure_healthy()?;
        config.validate()?;
        self.index = Some(hnsw::Index::build(&self.records, config));
        Ok(())
    }

    /// Approximate squared-L2 search; exact search remains the default.
    ///
    /// Lazily rebuilds after a mutation, reopen, or construction-parameter change.
    /// The first query includes the full rebuild cost. Returned distances are exact
    /// for the visited candidates; nearest-neighbor recall is not guaranteed.
    pub fn search_approximate(
        &mut self,
        query: &[f32],
        k: usize,
        ef_search: usize,
        config: HnswConfig,
    ) -> Result<Vec<SearchHit>> {
        self.ensure_healthy()?;
        self.validate_vector(query)?;
        config.validate()?;
        if !(1..=MAX_EF_SEARCH).contains(&ef_search) || ef_search < k.min(self.records.len()) {
            return Err(Error::InvalidEfSearch);
        }
        if k == 0 || self.records.is_empty() {
            return Ok(Vec::new());
        }
        if self
            .index
            .as_ref()
            .is_none_or(|index| index.config != config)
        {
            self.build_hnsw(config)?;
        }
        Ok(self
            .index
            .as_ref()
            .expect("built index")
            .search(query, k, ef_search, &self.records))
    }

    /// Exact squared-L2 nearest neighbors, ordered by distance then ascending ID.
    ///
    /// k=0 returns an empty result; k above the live count returns all records.
    /// Query vectors are validated even when k=0 or the database is empty.
    pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<SearchHit>> {
        self.ensure_healthy()?;
        self.validate_vector(query)?;
        if k == 0 {
            return Ok(Vec::new());
        }
        let mut hits: Vec<SearchHit> = self
            .records
            .iter()
            .map(|(&id, record)| SearchHit {
                id,
                distance: record
                    .vector
                    .iter()
                    .zip(query)
                    .map(|(&a, &b)| {
                        let difference = f64::from(a) - f64::from(b);
                        difference * difference
                    })
                    .sum(),
            })
            .collect();
        hits.sort_by(|a, b| {
            a.distance
                .total_cmp(&b.distance)
                .then_with(|| a.id.cmp(&b.id))
        });
        hits.truncate(k);
        Ok(hits)
    }

    /// Consumes the handle and releases its OS lock. Commits are already synced.
    ///
    /// Dropping the handle is also safe: there is no buffered uncommitted state.
    pub fn close(self) -> Result<()> {
        self.ensure_healthy()?;
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
