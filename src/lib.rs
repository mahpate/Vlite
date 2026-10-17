//! Embedded, single-file vector storage with synchronous per-operation commits.
//!
//! See the project README for the durability and explicit tail-repair contract.

mod format;

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Seek, SeekFrom, Write};
use std::ops::{Deref, DerefMut};
use std::path::Path;

pub const MAX_DIMENSIONS: usize = 65_536;
pub const MAX_PAYLOAD_BYTES: usize = 1_048_576;
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Locked,
    InvalidDimensions(usize),
    DimensionMismatch { expected: usize, actual: usize },
    NonFiniteVector,
    PayloadTooLarge(usize),
    UnsupportedVersion(u32),
    Corrupt { offset: u64, reason: &'static str },
    IncompleteTail { offset: u64, bytes: u64 },
    CommitIndeterminate(io::Error),
    RepairIndeterminate(io::Error),
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
    dimensions: usize,
    records: BTreeMap<u64, Record>,
    sequence: u64,
    end: u64,
    poisoned: bool,
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
            dimensions,
            records: BTreeMap::new(),
            sequence: 0,
            end: format::HEADER_SIZE,
            poisoned: false,
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
            dimensions: loaded.dimensions,
            records: loaded.records,
            sequence: loaded.sequence,
            end: loaded.end,
            poisoned: false,
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

    fn commit(&mut self, id: u64, record: Option<&Record>) -> Result<()> {
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or(Error::SequenceExhausted)?;
        let frame = format::frame(sequence, id, record);
        let result = append_and_sync(&mut self.file.0, self.end, &frame, File::sync_all);
        if let Err(error) = result {
            self.poisoned = true;
            return Err(Error::CommitIndeterminate(error));
        }
        self.sequence = sequence;
        self.end += frame.len() as u64;
        Ok(())
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
}
