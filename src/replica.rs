//! Read replicas: a follower that replays a primary's committed log segments.
//!
//! The follower's file is a byte-identical prefix of the primary log (same
//! lineage, same encryption descriptor), so it can be opened, verified and
//! backed up with the normal tools, and promoted to a primary. Segments come
//! from `Database::export_log` (live primary, in process) or
//! `maintenance::export_log_segment` (closed or copied log files).

use crate::crypto::EncryptionKey;
use crate::logops::{LogSegment, export_log_segment};
use crate::{Database, Error, Result};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Replication progress. `lag` is `primary_head - applied_sequence` as of the
/// newest segment seen; it is zero until a primary head is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplicaStatus {
    pub lineage: u64,
    pub applied_sequence: u64,
    pub primary_head: u64,
    pub lag: u64,
}

/// A read-only follower. Reads go through `db()`, which exposes only `&Database`
/// (every write needs `&mut`); the `put`/`delete`/`write_batch` methods here
/// exist to give a clear `ReadOnlyReplica` error.
///
/// Applying a segment appends its frames, fsyncs, and replays the whole file,
/// so one apply costs O(database size) and drops in-memory ANN indexes; batch
/// commits into larger segments for large databases.
pub struct ReplicaFollower {
    path: PathBuf,
    key: Option<EncryptionKey>,
    db: Database,
    primary_head: u64,
}

impl ReplicaFollower {
    /// Bootstraps a follower from the first segment of a primary
    /// (`base_sequence == 0`, which carries the file header). The destination
    /// must not exist.
    pub fn create(
        path: impl AsRef<Path>,
        segment: &LogSegment,
        key: Option<&EncryptionKey>,
    ) -> Result<Self> {
        let path = path.as_ref();
        if segment.base_sequence != 0 {
            return Err(Error::ReplicaDiverged(
                "bootstrap segment must start at revision 0",
            ));
        }
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        let written = file
            .write_all(&segment.header)
            .and_then(|()| file.write_all(&segment.frames))
            .and_then(|()| file.sync_all());
        drop(file);
        let opened = written
            .map_err(Error::from)
            .and_then(|()| Self::open(path, key))
            .and_then(|follower| {
                if follower.db.sequence == segment.last_sequence
                    && follower.db.lineage == Some(segment.lineage)
                {
                    Ok(follower)
                } else {
                    Err(Error::ReplicaDiverged("bootstrap segment is inconsistent"))
                }
            });
        match opened {
            Ok(mut follower) => {
                follower.primary_head = segment.head_sequence.max(segment.last_sequence);
                Ok(follower)
            }
            Err(error) => {
                let _ = std::fs::remove_file(path);
                Err(error)
            }
        }
    }

    /// Opens an existing follower file or a byte copy of a primary
    /// (`maintenance::backup` keeps the lineage). An incomplete tail left by a
    /// crash during apply is discarded: it is unacknowledged replica data the
    /// primary still has.
    pub fn open(path: impl AsRef<Path>, key: Option<&EncryptionKey>) -> Result<Self> {
        let path = path.as_ref();
        let db = match Database::open_inner(path, key) {
            Err(Error::IncompleteTail { .. }) => {
                match key {
                    Some(key) => Database::discard_incomplete_tail_encrypted(path, key)?,
                    None => Database::discard_incomplete_tail(path)?,
                };
                Database::open_inner(path, key)?
            }
            other => other?,
        };
        if db.lineage.is_none() {
            return Err(Error::ChangeFeedRequiresUpgrade);
        }
        Ok(Self {
            path: path.to_path_buf(),
            key: key.cloned(),
            primary_head: db.sequence,
            db,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read-only view of the replicated database.
    pub fn db(&self) -> &Database {
        &self.db
    }

    pub fn applied_sequence(&self) -> u64 {
        self.db.sequence
    }

    pub fn status(&self) -> ReplicaStatus {
        let applied = self.db.sequence;
        ReplicaStatus {
            lineage: self.db.lineage.unwrap_or(0),
            applied_sequence: applied,
            primary_head: self.primary_head.max(applied),
            lag: self.primary_head.saturating_sub(applied),
        }
    }

    /// Applies the next segment in order. A segment that does not continue
    /// exactly from `applied_sequence`, belongs to another lineage or header,
    /// or fails any checksum, authentication or replay check is rejected and
    /// leaves the follower unchanged.
    pub fn apply(&mut self, segment: &LogSegment) -> Result<ReplicaStatus> {
        let lineage = self.db.lineage.ok_or(Error::ChangeFeedRequiresUpgrade)?;
        if segment.lineage != lineage {
            return Err(Error::ReplicaDiverged("segment is from another lineage"));
        }
        if segment.base_sequence != self.db.sequence {
            return Err(Error::ReplicaDiverged(
                "segment does not continue from the applied revision",
            ));
        }
        self.check_header(&segment.header)?;
        self.primary_head = self.primary_head.max(segment.head_sequence);
        if segment.is_empty() {
            return Ok(self.status());
        }
        self.db
            .replica_extend(&segment.frames, segment.last_sequence, self.key.as_ref())?;
        Ok(self.status())
    }

    /// File-shipping mode: reads a segment file written by `LogSegment::save`.
    pub fn apply_file(&mut self, segment_path: impl AsRef<Path>) -> Result<ReplicaStatus> {
        self.apply(&LogSegment::load(segment_path)?)
    }

    /// Same-machine tail of a closed or copied primary log: cuts the next
    /// segment from `primary_log` and applies it. A live primary holds an
    /// exclusive file lock; ship `Database::export_log` segments instead.
    pub fn sync_from_log(
        &mut self,
        primary_log: impl AsRef<Path>,
        max_bytes: usize,
    ) -> Result<ReplicaStatus> {
        let segment =
            export_log_segment(primary_log, self.key.as_ref(), self.db.sequence, max_bytes)?;
        self.apply(&segment)
    }

    /// Replicas are read-only.
    pub fn put(&mut self, _id: u64, _vector: &[f32], _payload: &[u8]) -> Result<u64> {
        Err(Error::ReadOnlyReplica)
    }

    pub fn delete(&mut self, _id: u64) -> Result<bool> {
        Err(Error::ReadOnlyReplica)
    }

    pub fn write_batch(&mut self, _operations: &[crate::Mutation]) -> Result<()> {
        Err(Error::ReadOnlyReplica)
    }

    /// Failover: turns the follower into a writable database. The replication
    /// link must already be stopped; the old primary must not be written again
    /// with the same lineage or the two histories diverge.
    pub fn promote(self) -> Database {
        self.db
    }

    fn check_header(&self, header: &[u8]) -> Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = &self.db.file;
        let mut current = vec![0u8; header.len()];
        file.seek(SeekFrom::Start(0))?;
        if file.read_exact(&mut current).is_err() || current != header {
            return Err(Error::ReplicaDiverged(
                "segment header differs from follower",
            ));
        }
        Ok(())
    }
}
