//! Log-level maintenance: key rotation by verified copy-out, point-in-time
//! restore, and log-segment export for read replicas.
//!
//! Every operation reads its sources through the exclusive OS lock, writes a
//! brand-new file via staged, fully re-validated publication, and never
//! modifies or replaces a source.

use crate::crypto::Cipher;
use crate::format::{self, FLAG_ENCRYPTED, FLAG_TIMESTAMPS, RawFrame};
use crate::maintenance::{Inspection, publish_with, require_complete, source};
use crate::{EncryptionKey, Error, Result};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

/// Rewrites a database into a new file under a different key (key rotation),
/// from unencrypted to encrypted, or from encrypted to unencrypted.
///
/// The complete committed log (all sequences, timestamps and revisions) is
/// preserved: frames are transcoded one by one with fresh nonces and a fresh
/// per-file salt, then the output is fully reloaded and validated before it is
/// published without overwrite. The output gets a new lineage (feeds and
/// replicas must restart from it). The source is never modified; delete the old
/// file and its backups yourself once the new one is verified.
pub fn reencrypt(
    source_path: impl AsRef<Path>,
    source_key: Option<&EncryptionKey>,
    destination: impl AsRef<Path>,
    destination_key: Option<&EncryptionKey>,
) -> Result<Inspection> {
    let mut file = source(source_path.as_ref())?;
    let loaded = format::load_with(&mut file, source_key)?;
    require_complete(&loaded)?;
    let header = format::read_log_header(&mut file, source_key)?;
    if header.version < format::V5 {
        return Err(Error::InvalidEncryptionOptions(
            "re-encryption requires a format v5 source; run maintenance::upgrade first",
        ));
    }
    let cipher = destination_key.map(Cipher::create).transpose()?;
    // Every flag but encryption (commit timestamps, and `FLAG_GRAPH` in
    // graph builds) carries over with the frames that depend on it.
    let mut flags = header.flags & !FLAG_ENCRYPTED;
    if cipher.is_some() {
        flags |= FLAG_ENCRYPTED;
    }
    let lineage = format::new_lineage()?;
    publish_with(destination.as_ref(), destination_key, |output| {
        output.write_all(&format::header_v5_flags(
            header.dimensions,
            lineage,
            header.collection_floor,
            flags,
            cipher.as_ref(),
        ))?;
        let mut offset = header.start;
        while let Some(frame) = format::next_frame(&mut file, &header, offset)? {
            offset += frame.disk.len() as u64;
            match &cipher {
                Some(cipher) => output.write_all(&cipher.seal_frame(&frame.plain)?)?,
                None => output.write_all(&frame.plain)?,
            }
        }
        Ok(())
    })
}

/// What `restore_to` rebuilds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreTarget {
    /// The state after the commit with this revision (sequence); 0 is empty.
    Sequence(u64),
    /// The state after the last commit whose recorded time is at or before
    /// this Unix-millisecond timestamp. Needs a file created with
    /// `CreateOptions::commit_timestamps`.
    TimestampMs(u64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreReport {
    /// Revision (sequence) the output ends at.
    pub restored_sequence: u64,
    /// Revision of the source log's head.
    pub source_head: u64,
    /// Recorded commit time of the last restored commit, if timestamped.
    pub restored_at_ms: Option<u64>,
    pub output: Inspection,
}

/// Point-in-time restore into a new file.
///
/// `log_path` is a copy of the complete committed log (the primary file after
/// it was closed or copied with `backup`, or a replica follower's file).
/// `base_backup`, when given, must be an earlier backup of the same lineage
/// whose bytes are an exact prefix of the log; it proves the log extends that
/// backup and bounds the target from below. Frames up to the target are copied
/// byte for byte; the output gets a new lineage and is fully reloaded and
/// validated before publication. Nothing is modified in the sources.
pub fn restore_to(
    base_backup: Option<&Path>,
    log_path: impl AsRef<Path>,
    destination: impl AsRef<Path>,
    target: RestoreTarget,
    key: Option<&EncryptionKey>,
) -> Result<RestoreReport> {
    let mut log = source(log_path.as_ref())?;
    let loaded = format::load_with(&mut log, key)?;
    require_complete(&loaded)?;
    let header = format::read_log_header(&mut log, key)?;
    let cut = match target {
        RestoreTarget::Sequence(sequence) => {
            if sequence > loaded.sequence {
                return Err(Error::InvalidRestoreTarget(
                    "target revision is beyond the end of the log",
                ));
            }
            if sequence < loaded.history_floor {
                return Err(Error::InvalidRestoreTarget(
                    "target revision is before the log's history floor (a checkpoint restated it)",
                ));
            }
            sequence
        }
        RestoreTarget::TimestampMs(timestamp) => {
            if loaded.flags & FLAG_TIMESTAMPS == 0 {
                return Err(Error::TimestampsUnavailable);
            }
            let first_later = loaded
                .commit_times
                .iter()
                .position(|&(_, time)| time > timestamp);
            match first_later {
                Some(0) => 0,
                Some(index) => loaded.commit_times[index - 1].0,
                None => loaded.sequence,
            }
        }
    };
    if let Some(base_path) = base_backup {
        let mut base = source(base_path)?;
        let base_loaded = format::load_with(&mut base, key)?;
        require_complete(&base_loaded)?;
        if base_loaded.lineage != loaded.lineage || loaded.lineage.is_none() {
            return Err(Error::InvalidRestoreTarget(
                "base backup has a different lineage than the log",
            ));
        }
        if base_loaded.sequence > cut {
            return Err(Error::InvalidRestoreTarget(
                "target is older than the base backup",
            ));
        }
        if !is_prefix(&mut base, &mut log, base_loaded.end)? {
            return Err(Error::InvalidRestoreTarget(
                "base backup is not a prefix of the log",
            ));
        }
    }
    let restored_at_ms = loaded
        .commit_times
        .iter()
        .rev()
        .find(|&&(sequence, _)| sequence <= cut)
        .map(|&(_, time)| time);
    let mut raw = header.raw.clone();
    if header.version >= format::V4 {
        let lineage = format::new_lineage()?;
        raw[16..24].copy_from_slice(&lineage.to_le_bytes());
        let checksum = format::crc32(&raw[..28]);
        raw[28..32].copy_from_slice(&checksum.to_le_bytes());
    }
    let output = publish_with(destination.as_ref(), key, |output| {
        output.write_all(&raw)?;
        let mut offset = header.start;
        while let Some(frame) = format::next_frame(&mut log, &header, offset)? {
            if frame.sequence > cut {
                break;
            }
            offset += frame.disk.len() as u64;
            output.write_all(&frame.disk)?;
        }
        Ok(())
    })?;
    Ok(RestoreReport {
        restored_sequence: cut,
        source_head: loaded.sequence,
        restored_at_ms,
        output,
    })
}

fn is_prefix<A: Read + Seek, B: Read + Seek>(
    short: &mut A,
    long: &mut B,
    bytes: u64,
) -> Result<bool> {
    short.seek(SeekFrom::Start(0))?;
    long.seek(SeekFrom::Start(0))?;
    let mut remaining = bytes;
    let mut left = vec![0u8; 1 << 16];
    let mut right = vec![0u8; 1 << 16];
    while remaining > 0 {
        let chunk = remaining.min(left.len() as u64) as usize;
        short.read_exact(&mut left[..chunk])?;
        if long.read_exact(&mut right[..chunk]).is_err() || left[..chunk] != right[..chunk] {
            return Ok(false);
        }
        remaining -= chunk as u64;
    }
    Ok(true)
}

const SEGMENT_MAGIC: &[u8; 8] = b"VLSEG001";

/// A contiguous run of whole commits from a primary's log, in exact on-disk
/// form (sealed for encrypted databases), shipped to a read replica.
///
/// Segments are authenticated end to end by a SHA-256 digest and by the log's
/// own per-frame checksums and sequence contiguity; they carry no transport
/// security themselves, so ship them over a trusted channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogSegment {
    pub lineage: u64,
    /// Primary file header (and encryption descriptor); lets a follower
    /// bootstrap an empty log.
    pub header: Vec<u8>,
    /// Revision the segment continues from (frames start at `base_sequence + 1`).
    pub base_sequence: u64,
    /// Revision of the last commit in the segment (`base_sequence` if empty).
    pub last_sequence: u64,
    /// The primary's head revision when the segment was cut (for lag).
    pub head_sequence: u64,
    pub frames: Vec<u8>,
}

impl LogSegment {
    pub fn is_empty(&self) -> bool {
        self.last_sequence == self.base_sequence
    }

    /// Writes the serialized segment to a new file (write, fsync, rename);
    /// an existing destination is replaced only by the completed file.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let mut temporary = path.as_os_str().to_owned();
        temporary.push(".part");
        let temporary = std::path::PathBuf::from(temporary);
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(&self.to_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        Ok(())
    }

    /// Reads and authenticates a segment file written by `save`.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_bytes(&std::fs::read(path.as_ref())?)
    }

    fn digest(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(SEGMENT_MAGIC);
        hasher.update(self.lineage.to_le_bytes());
        hasher.update(self.base_sequence.to_le_bytes());
        hasher.update(self.last_sequence.to_le_bytes());
        hasher.update(self.head_sequence.to_le_bytes());
        hasher.update((self.header.len() as u32).to_le_bytes());
        hasher.update(&self.header);
        hasher.update((self.frames.len() as u64).to_le_bytes());
        hasher.update(&self.frames);
        hasher.finalize().into()
    }

    /// Verifies the digest recorded by `to_bytes` (in memory it is recomputed).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.frames.len() + self.header.len() + 96);
        out.extend_from_slice(SEGMENT_MAGIC);
        out.extend_from_slice(&self.lineage.to_le_bytes());
        out.extend_from_slice(&self.base_sequence.to_le_bytes());
        out.extend_from_slice(&self.last_sequence.to_le_bytes());
        out.extend_from_slice(&self.head_sequence.to_le_bytes());
        out.extend_from_slice(&(self.header.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.frames.len() as u64).to_le_bytes());
        out.extend_from_slice(&self.header);
        out.extend_from_slice(&self.frames);
        out.extend_from_slice(&self.digest());
        out
    }

    /// Parses and authenticates a serialized segment.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let bad = |reason| Error::ReplicaDiverged(reason);
        if bytes.len() < 8 + 8 * 4 + 12 + 32 || &bytes[..8] != SEGMENT_MAGIC {
            return Err(bad("not a log segment"));
        }
        let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        let header_len = u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize;
        let frames_len = u64_at(44) as usize;
        let body_end = 52usize
            .checked_add(header_len)
            .and_then(|n| n.checked_add(frames_len))
            .ok_or_else(|| bad("segment lengths overflow"))?;
        if bytes.len() != body_end + 32 {
            return Err(bad("segment length mismatch"));
        }
        let segment = Self {
            lineage: u64_at(8),
            base_sequence: u64_at(16),
            last_sequence: u64_at(24),
            head_sequence: u64_at(32),
            header: bytes[52..52 + header_len].to_vec(),
            frames: bytes[52 + header_len..body_end].to_vec(),
        };
        if segment.digest()[..] != bytes[body_end..] {
            return Err(bad("segment digest mismatch"));
        }
        Ok(segment)
    }
}

/// Cuts a segment of whole commits after `after_sequence` from an open log,
/// stopping once at least `max_bytes` of frames were collected (always at
/// least one commit when any are pending).
pub(crate) fn read_segment<R: Read + Seek>(
    file: &mut R,
    key: Option<&EncryptionKey>,
    known: Option<&crate::crypto::Cipher>,
    after_sequence: u64,
    max_bytes: usize,
) -> Result<LogSegment> {
    let header = format::read_log_header_with(file, key, known)?;
    let lineage = header.lineage.ok_or(Error::ChangeFeedRequiresUpgrade)?;
    let mut offset = header.start;
    let mut head = 0u64;
    let mut frames = Vec::new();
    let mut last = after_sequence;
    let mut pending_anchor: Option<RawFrame> = None;
    let mut full = false;
    let mut check = crate::compact::ImageCheck::new(header.version, header.flags);
    while let Some(frame) = format::next_checked_frame(file, &header, offset, check.as_mut())? {
        let at = offset;
        offset += frame.disk.len() as u64;
        if frame.operation == format::OP_TIME_ANCHOR {
            pending_anchor = Some(frame);
            continue;
        }
        if frame.operation == format::OP_ADVANCE {
            // A follower cannot replay restated history as a suffix.
            if after_sequence < frame.sequence {
                return Err(Error::CursorBeforeHistory {
                    cursor: after_sequence,
                    floor: frame.sequence,
                });
            }
            head = frame.sequence;
            continue;
        }
        if frame.operation == format::OP_COMPACT_BLOCK {
            check
                .as_mut()
                .ok_or_else(|| format::corrupt(at, "invalid operation or reserved record bytes"))?
                .block(frame.collection, frame.id, frame.count, frame.body(), at)?;
        }
        head = head.max(frame.sequence);
        let anchor = pending_anchor.take();
        if frame.sequence <= after_sequence || full {
            continue;
        }
        if let Some(anchor) = anchor {
            frames.extend_from_slice(&anchor.disk);
        }
        frames.extend_from_slice(&frame.disk);
        last = frame.sequence;
        if frames.len() >= max_bytes {
            full = true;
        }
    }
    // The scan read to the end of the log or a frame that does not fit.
    if let Some(check) = &check {
        check.end(offset)?;
    }
    if after_sequence > head {
        return Err(Error::CursorAhead {
            cursor: after_sequence,
            head,
        });
    }
    Ok(LogSegment {
        lineage,
        header: header.raw,
        base_sequence: after_sequence,
        last_sequence: last,
        head_sequence: head,
        frames,
    })
}

/// Cuts a log segment from a closed (or copied) log file. See `read_segment`.
pub fn export_log_segment(
    log_path: impl AsRef<Path>,
    key: Option<&EncryptionKey>,
    after_sequence: u64,
    max_bytes: usize,
) -> Result<LogSegment> {
    let mut file = source(log_path.as_ref())?;
    read_segment(&mut file, key, None, after_sequence, max_bytes)
}
