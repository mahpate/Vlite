//! Optional ANN snapshot sidecar (format v1): a derived, disposable cache of
//! in-memory HNSW indexes. The datafile stays the only source of truth; this
//! module never opens it for writing and never reads it.
//!
//! Load validates, in order: file type and size, magic and version, a full
//! SHA-256 pass, database identity, per-collection content digests, bounded
//! structure, and graph/buffer/quantizer invariants recomputed from current
//! records. A second hashing pass must match the same trailer. Indexes are
//! installed only when every section passes. Save writes a randomized
//! `create_new` staging file beside the destination, re-validates it with the
//! same reader, syncs, and atomically renames it over the destination.

use std::{
    fmt,
    fs::{self, File},
    io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use crate::{Database, Error, Result, collection::Collection, hnsw::Index};

/// Largest sidecar that `load_ann_snapshot` reads or `save_ann_snapshot` writes.
pub const MAX_ANN_SNAPSHOT_BYTES: u64 = 4 * 1024 * 1024 * 1024;

const MAGIC: [u8; 8] = *b"VLITEANN";
const VERSION: u32 = 1;
const HEADER_BYTES: u64 = 64;
const TRAILER_BYTES: u64 = 32;
/// Collection names are at most 63 bytes (`default` is 7).
const MAX_NAME_BYTES: usize = 64;

/// Result of a successful `Database::save_ann_snapshot`.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnnSnapshotReport {
    /// Collection indexes written (collections without an index are skipped).
    pub indexes: usize,
    /// Sidecar size in bytes, including the SHA-256 trailer.
    pub bytes: u64,
}

/// Outcome of `Database::load_ann_snapshot`. Only `Loaded` changes state.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnnSnapshotLoad {
    /// Every section validated; this many indexes were installed.
    Loaded { indexes: usize },
    /// No file exists at the path.
    Missing,
    /// The sidecar cannot be used; nothing was installed or modified.
    Rejected(AnnSnapshotRejection),
}

impl AnnSnapshotLoad {
    /// Stable outcome code: `loaded`, `missing`, or the rejection code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Loaded { .. } => "loaded",
            Self::Missing => "missing",
            Self::Rejected(rejection) => rejection.code(),
        }
    }
}

/// Why a sidecar was not installed. The detail strings are stable,
/// non-sensitive labels (for example `identity`, `content`, `checksum`).
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnnSnapshotRejection {
    /// Valid file for a different committed state (writes, compaction,
    /// upgrade, restore, or a different database).
    Stale(&'static str),
    /// Malformed, truncated, checksum-mismatched or semantically invalid.
    Corrupt(&'static str),
    /// A sidecar format version this build does not read.
    UnsupportedVersion(u32),
    /// File length above `MAX_ANN_SNAPSHOT_BYTES`.
    TooLarge(u64),
    /// Not a regular file, or an I/O error other than not-found.
    Unreadable(io::ErrorKind),
}

impl AnnSnapshotRejection {
    /// One of `stale`, `corrupt`, `unsupported_version`, `too_large`, `unreadable`.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Stale(_) => "stale",
            Self::Corrupt(_) => "corrupt",
            Self::UnsupportedVersion(_) => "unsupported_version",
            Self::TooLarge(_) => "too_large",
            Self::Unreadable(_) => "unreadable",
        }
    }
}

impl fmt::Display for AnnSnapshotRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stale(detail) | Self::Corrupt(detail) => write!(f, "{}: {detail}", self.code()),
            Self::UnsupportedVersion(version) => write!(f, "{}: {version}", self.code()),
            Self::TooLarge(bytes) => write!(f, "{}: {bytes} bytes", self.code()),
            Self::Unreadable(kind) => write!(f, "{}: {kind}", self.code()),
        }
    }
}

pub(crate) type Parse<T> = std::result::Result<T, AnnSnapshotRejection>;

pub(crate) fn corrupt(detail: &'static str) -> AnnSnapshotRejection {
    AnnSnapshotRejection::Corrupt(detail)
}

fn read_error(error: io::Error) -> AnnSnapshotRejection {
    if error.kind() == io::ErrorKind::UnexpectedEof {
        corrupt("truncated")
    } else {
        AnnSnapshotRejection::Unreadable(error.kind())
    }
}

/// Hashing reader over the checksummed body. Reads past the body are
/// `Corrupt("truncated")`, so every allocation is bounded by unread bytes.
pub(crate) struct Source<R> {
    inner: R,
    hasher: Sha256,
    remaining: u64,
}

impl<R: Read> Source<R> {
    fn new(inner: R, body_bytes: u64) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            remaining: body_bytes,
        }
    }

    /// Unread checksummed body bytes.
    pub fn remaining(&self) -> u64 {
        self.remaining
    }

    fn fill(&mut self, buffer: &mut [u8]) -> Parse<()> {
        if buffer.len() as u64 > self.remaining {
            return Err(corrupt("truncated"));
        }
        self.inner.read_exact(buffer).map_err(read_error)?;
        self.hasher.update(&*buffer);
        self.remaining -= buffer.len() as u64;
        Ok(())
    }

    fn array<const N: usize>(&mut self) -> Parse<[u8; N]> {
        let mut bytes = [0; N];
        self.fill(&mut bytes)?;
        Ok(bytes)
    }

    pub fn u8(&mut self) -> Parse<u8> {
        Ok(self.array::<1>()?[0])
    }

    pub fn u16(&mut self) -> Parse<u16> {
        self.array().map(u16::from_le_bytes)
    }

    pub fn u32(&mut self) -> Parse<u32> {
        self.array().map(u32::from_le_bytes)
    }

    pub fn u64(&mut self) -> Parse<u64> {
        self.array().map(u64::from_le_bytes)
    }

    /// Exactly `length` bytes; checked against unread bytes before allocating.
    pub fn bytes(&mut self, length: usize) -> Parse<Vec<u8>> {
        if length as u64 > self.remaining {
            return Err(corrupt("truncated"));
        }
        let mut bytes = vec![0; length];
        self.fill(&mut bytes)?;
        Ok(bytes)
    }

    pub fn f32s(&mut self, count: usize) -> Parse<Vec<f32>> {
        self.numbers(count, f32::from_le_bytes)
    }

    pub fn f64s(&mut self, count: usize) -> Parse<Vec<f64>> {
        self.numbers(count, f64::from_le_bytes)
    }

    /// Decode into the final allocation using bounded stack scratch space,
    /// rather than retaining a second, equally large byte vector.
    fn numbers<const WIDTH: usize, T>(
        &mut self,
        count: usize,
        decode: impl Fn([u8; WIDTH]) -> T,
    ) -> Parse<Vec<T>> {
        let bytes = count
            .checked_mul(WIDTH)
            .ok_or_else(|| corrupt("truncated"))?;
        if bytes as u64 > self.remaining {
            return Err(corrupt("truncated"));
        }
        let mut values = Vec::with_capacity(count);
        let mut buffer = [0; 4096];
        while values.len() < count {
            let length = (count - values.len()).min(buffer.len() / WIDTH) * WIDTH;
            self.fill(&mut buffer[..length])?;
            values.extend(
                buffer[..length]
                    .as_chunks::<WIDTH>()
                    .0
                    .iter()
                    .map(|&chunk| decode(chunk)),
            );
        }
        Ok(values)
    }
}

/// Hashes and counts written bytes; refuses to exceed the format cap.
struct Sink<W> {
    inner: W,
    hasher: Sha256,
    written: u64,
}

impl<W: Write> Write for Sink<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.written + buffer.len() as u64 > MAX_ANN_SNAPSHOT_BYTES - TRAILER_BYTES {
            return Err(io::Error::other(
                "ANN snapshot would exceed MAX_ANN_SNAPSHOT_BYTES",
            ));
        }
        let written = self.inner.write(buffer)?;
        self.hasher.update(&buffer[..written]);
        self.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// SHA-256 over every live record in ascending ID order:
/// `id u64 LE ‖ revision u64 LE ‖ dimensions × f32 bits LE`.
fn content_digest(collection: &Collection) -> [u8; 32] {
    let mut hasher = Sha256::new();
    let mut buffer = [0; 4096];
    for (&id, record) in &collection.records {
        hasher.update(id.to_le_bytes());
        let revision = collection.revisions.get(&id).copied().unwrap_or(0);
        hasher.update(revision.to_le_bytes());
        // Preserve the v1 byte stream, batching SHA updates instead of
        // invoking the hasher once for every four-byte component.
        for values in record.vector.chunks(buffer.len() / 4) {
            for (value, bytes) in values.iter().zip(buffer.as_chunks_mut::<4>().0.iter_mut()) {
                bytes.copy_from_slice(&value.to_bits().to_le_bytes());
            }
            hasher.update(&buffer[..values.len() * 4]);
        }
    }
    hasher.finalize().into()
}

/// `(datafile version, lineage or 0, sequence, end, default dimensions)`.
fn identity(db: &Database) -> (u32, u64, u64, u64, u32) {
    (
        db.version,
        db.lineage.unwrap_or(0),
        db.sequence,
        db.end,
        db.default_state().dimensions as u32,
    )
}

fn write_body(db: &Database, w: &mut impl Write) -> io::Result<usize> {
    let sections: Vec<_> = db
        .collections
        .iter()
        .filter_map(|(&id, collection)| {
            collection
                .index
                .as_ref()
                .map(|index| (id, collection, index))
        })
        .collect();
    let (version, lineage, sequence, end, dimensions) = identity(db);
    w.write_all(&MAGIC)?;
    w.write_all(&VERSION.to_le_bytes())?;
    w.write_all(&0u32.to_le_bytes())?;
    w.write_all(&version.to_le_bytes())?;
    w.write_all(&0u32.to_le_bytes())?;
    w.write_all(&lineage.to_le_bytes())?;
    w.write_all(&sequence.to_le_bytes())?;
    w.write_all(&end.to_le_bytes())?;
    w.write_all(&dimensions.to_le_bytes())?;
    w.write_all(&(sections.len() as u32).to_le_bytes())?;
    w.write_all(&[0; 8])?;
    for &(id, collection, index) in &sections {
        w.write_all(&id.to_le_bytes())?;
        w.write_all(&(collection.name.len() as u16).to_le_bytes())?;
        w.write_all(collection.name.as_bytes())?;
        w.write_all(&(collection.dimensions as u32).to_le_bytes())?;
        w.write_all(&(collection.records.len() as u64).to_le_bytes())?;
        w.write_all(&content_digest(collection))?;
        index.write_snapshot(w)?;
    }
    Ok(sections.len())
}

/// Parses and fully validates the checksummed body (second pass).
fn parse<R: Read>(db: &Database, reader: R, length: u64) -> Parse<Vec<(u16, Index)>> {
    let mut source = Source::new(reader, length - TRAILER_BYTES);
    if source.array::<8>()? != MAGIC {
        return Err(corrupt("magic"));
    }
    let version = source.u32()?;
    if version != VERSION {
        return Err(AnnSnapshotRejection::UnsupportedVersion(version));
    }
    let flags = source.u32()?;
    let datafile_version = source.u32()?;
    let reserved = source.u32()?;
    let stored = (
        datafile_version,
        source.u64()?,
        source.u64()?,
        source.u64()?,
        source.u32()?,
    );
    let section_count = source.u32()? as usize;
    if flags != 0 || reserved != 0 || source.array::<8>()? != [0; 8] {
        return Err(corrupt("reserved"));
    }
    if stored != identity(db) {
        return Err(AnnSnapshotRejection::Stale("identity"));
    }
    if section_count > db.collections.len() {
        return Err(corrupt("section count"));
    }
    let mut indexes: Vec<(u16, Index)> = Vec::with_capacity(section_count);
    for _ in 0..section_count {
        let id = source.u16()?;
        if indexes.last().is_some_and(|&(last, _)| last >= id) {
            return Err(corrupt("section order"));
        }
        let name_length = usize::from(source.u16()?);
        if name_length > MAX_NAME_BYTES {
            return Err(corrupt("collection name"));
        }
        let name = source.bytes(name_length)?;
        let dimensions = source.u32()?;
        let live_records = source.u64()?;
        let digest = source.array::<32>()?;
        let collection = db
            .collections
            .get(&id)
            .filter(|c| c.name.as_bytes() == name && c.dimensions as u64 == u64::from(dimensions))
            .ok_or(AnnSnapshotRejection::Stale("collection"))?;
        if live_records != collection.records.len() as u64 || digest != content_digest(collection) {
            return Err(AnnSnapshotRejection::Stale("content"));
        }
        let index = Index::read_snapshot(&mut source, &collection.records, collection.dimensions)?;
        indexes.push((id, index));
    }
    if source.remaining() != 0 {
        return Err(corrupt("trailing bytes"));
    }
    let mut trailer = [0; TRAILER_BYTES as usize];
    source.inner.read_exact(&mut trailer).map_err(read_error)?;
    if <[u8; 32]>::from(source.hasher.finalize()) != trailer {
        return Err(corrupt("checksum"));
    }
    match source.inner.read(&mut [0]) {
        Ok(0) => Ok(indexes),
        Ok(_) => Err(corrupt("trailing bytes")),
        Err(error) => Err(read_error(error)),
    }
}

/// Size, version and whole-file checksum first, then the validating parse.
fn read_validated(db: &Database, file: &mut File, length: u64) -> Parse<Vec<(u16, Index)>> {
    if length > MAX_ANN_SNAPSHOT_BYTES {
        return Err(AnnSnapshotRejection::TooLarge(length));
    }
    if length < HEADER_BYTES + TRAILER_BYTES {
        return Err(corrupt("truncated"));
    }
    let mut head = [0; 12];
    file.seek(SeekFrom::Start(0)).map_err(read_error)?;
    file.read_exact(&mut head).map_err(read_error)?;
    if head[..8] != MAGIC {
        return Err(corrupt("magic"));
    }
    let version = u32::from_le_bytes(head[8..].try_into().expect("4 bytes"));
    if version != VERSION {
        return Err(AnnSnapshotRejection::UnsupportedVersion(version));
    }
    file.seek(SeekFrom::Start(0)).map_err(read_error)?;
    let mut reader = BufReader::with_capacity(1 << 16, &mut *file);
    let mut hasher = Sha256::new();
    let body = length - TRAILER_BYTES;
    let hashed = io::copy(&mut (&mut reader).take(body), &mut hasher).map_err(read_error)?;
    if hashed != body {
        return Err(corrupt("truncated"));
    }
    let mut trailer = [0; TRAILER_BYTES as usize];
    reader.read_exact(&mut trailer).map_err(read_error)?;
    if <[u8; 32]>::from(hasher.finalize()) != trailer {
        return Err(corrupt("checksum"));
    }
    drop(reader);
    file.seek(SeekFrom::Start(0)).map_err(read_error)?;
    parse(db, BufReader::with_capacity(1 << 16, &mut *file), length)
}

/// Opens only an existing regular file; never creates, truncates or writes.
fn read(db: &Database, path: &Path) -> std::result::Result<Vec<(u16, Index)>, AnnSnapshotLoad> {
    let unreadable = |error: io::Error| match error.kind() {
        io::ErrorKind::NotFound => AnnSnapshotLoad::Missing,
        kind => AnnSnapshotLoad::Rejected(AnnSnapshotRejection::Unreadable(kind)),
    };
    let not_file = AnnSnapshotLoad::Rejected(AnnSnapshotRejection::Unreadable(
        io::ErrorKind::InvalidInput,
    ));
    // Checked before open so FIFOs and devices are never opened.
    if !fs::metadata(path).map_err(unreadable)?.is_file() {
        return Err(not_file);
    }
    let mut file = File::open(path).map_err(unreadable)?;
    let metadata = file.metadata().map_err(unreadable)?;
    if !metadata.is_file() {
        return Err(not_file);
    }
    read_validated(db, &mut file, metadata.len()).map_err(AnnSnapshotLoad::Rejected)
}

pub(crate) fn load(db: &mut Database, path: &Path) -> AnnSnapshotLoad {
    match read(db, path) {
        Ok(indexes) => {
            let count = indexes.len();
            for (id, mut index) in indexes {
                let collection = db
                    .collections
                    .get_mut(&id)
                    .expect("validated sections reference live collections");
                index.set_background(collection.background_hnsw.is_some());
                collection.index = Some(index);
            }
            AnnSnapshotLoad::Loaded { indexes: count }
        }
        Err(outcome) => outcome,
    }
}

fn invalid_destination(message: &'static str) -> Error {
    Error::Io(io::Error::new(io::ErrorKind::InvalidInput, message))
}

/// Replaces only a regular file that already starts with the sidecar magic,
/// so a datafile (open or closed), directory, symlink or unrelated file is
/// never overwritten. A locked open datafile also fails the probe read.
fn check_destination(path: &Path) -> Result<()> {
    if path.file_name().is_none() {
        return Err(invalid_destination("ANN snapshot path must name a file"));
    }
    let metadata = match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        result => result?,
    };
    if !metadata.file_type().is_file() {
        return Err(invalid_destination(
            "ANN snapshot destination exists and is not a regular file",
        ));
    }
    let mut magic = [0; 8];
    match File::open(path)?.read_exact(&mut magic) {
        Ok(()) if magic == MAGIC => Ok(()),
        Ok(()) => Err(invalid_destination(
            "refusing to replace a file that is not a VLite ANN snapshot",
        )),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Err(invalid_destination(
            "refusing to replace a file that is not a VLite ANN snapshot",
        )),
        Err(error) => Err(error.into()),
    }
}

/// Writes body and trailer; returns `(indexes, total bytes)`.
fn write_staged(db: &Database, file: &mut File) -> io::Result<(usize, u64)> {
    let mut sink = Sink {
        inner: BufWriter::new(file),
        hasher: Sha256::new(),
        written: 0,
    };
    let indexes = write_body(db, &mut sink)?;
    let Sink {
        mut inner,
        hasher,
        written,
    } = sink;
    inner.write_all(&hasher.finalize())?;
    inner.flush()?;
    Ok((indexes, written + TRAILER_BYTES))
}

pub(crate) fn save(db: &Database, path: &Path) -> Result<AnnSnapshotReport> {
    check_destination(path)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    // Randomized create_new name: a pre-existing `<path>.tmp` or symlink is
    // never followed, truncated or removed. Dropped on every error path.
    let mut staging = tempfile::Builder::new()
        .prefix(".vlite-ann-")
        .suffix(".tmp")
        .tempfile_in(parent)?;
    let (indexes, bytes) = write_staged(db, staging.as_file_mut())?;
    // Self-check with the loader: never publish a sidecar that would not load.
    match read_validated(db, staging.as_file_mut(), bytes) {
        Ok(parsed) if parsed.len() == indexes => {}
        Ok(_) => return Err(io::Error::other("ANN snapshot self-check section mismatch").into()),
        Err(rejection) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("ANN snapshot self-check failed ({rejection})"),
            )
            .into());
        }
    }
    staging.as_file().sync_all()?;
    // Before this rename succeeds the destination is untouched.
    staging
        .persist(path)
        .map_err(|error| Error::Io(error.error))?;
    // After it, the destination holds the new complete snapshot; only its
    // directory-entry durability can still be uncertain.
    #[cfg(unix)]
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| Error::PublicationIndeterminate {
            destination: path.to_path_buf(),
            source,
        })?;
    Ok(AnnSnapshotReport { indexes, bytes })
}

/// `<datafile>.ann`.
pub(crate) fn sidecar_path(datafile: &Path) -> PathBuf {
    let mut path = datafile.as_os_str().to_owned();
    path.push(".ann");
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_decoding_preserves_bits_and_hashes_across_scratch_boundaries() {
        for count in [0, 1, 511, 512, 513, 1023, 1024, 1025, 4097] {
            let f32_bits = [0, 0x8000_0000, 1, 0x7f7f_ffff, 0xff7f_ffff, 0x7fc0_0042];
            let mut bytes = Vec::new();
            for position in 0..count {
                bytes.extend_from_slice(&u32::to_le_bytes(f32_bits[position % f32_bits.len()]));
            }
            bytes.push(0xa5);
            let mut source = Source::new(bytes.as_slice(), bytes.len() as u64);
            let values = source.f32s(count).unwrap();
            assert_eq!(values.len(), count);
            for (position, value) in values.iter().enumerate() {
                assert_eq!(value.to_bits(), f32_bits[position % f32_bits.len()]);
            }
            assert_eq!(source.remaining(), 1);
            assert_eq!(source.u8().unwrap(), 0xa5);
            assert_eq!(source.hasher.finalize(), Sha256::digest(&bytes));

            let f64_bits = [
                0,
                0x8000_0000_0000_0000,
                1,
                0x7fef_ffff_ffff_ffff,
                0xffef_ffff_ffff_ffff,
                0x7ff8_0000_0000_0042,
            ];
            bytes.clear();
            for position in 0..count {
                bytes.extend_from_slice(&u64::to_le_bytes(f64_bits[position % f64_bits.len()]));
            }
            bytes.push(0x5a);
            let mut source = Source::new(bytes.as_slice(), bytes.len() as u64);
            let values = source.f64s(count).unwrap();
            assert_eq!(values.len(), count);
            for (position, value) in values.iter().enumerate() {
                assert_eq!(value.to_bits(), f64_bits[position % f64_bits.len()]);
            }
            assert_eq!(source.remaining(), 1);
            assert_eq!(source.u8().unwrap(), 0x5a);
            assert_eq!(source.hasher.finalize(), Sha256::digest(&bytes));
        }
    }

    #[test]
    fn numeric_decoding_rejects_oversized_counts_and_truncation() {
        for count in [1, 1025, usize::MAX] {
            let mut source = Source::new(io::empty(), 3);
            assert_eq!(source.f32s(count), Err(corrupt("truncated")));
            assert_eq!(source.remaining(), 3);
            assert_eq!(source.f64s(count), Err(corrupt("truncated")));
            assert_eq!(source.remaining(), 3);
        }
        let mut source = Source::new(&[0; 4096][..], 4100);
        assert_eq!(source.f32s(1025), Err(corrupt("truncated")));
        let mut source = Source::new(&[0; 4096][..], 4104);
        assert_eq!(source.f64s(513), Err(corrupt("truncated")));
    }

    #[test]
    fn batched_content_digest_matches_v1_component_stream() {
        for dimensions in [1, 511, 512, 513, 1023, 1024, 1025, 4097] {
            let mut db = Database::create_in_memory(dimensions, Default::default()).unwrap();
            for id in [9, 2, 37] {
                let vector: Vec<_> = (0..dimensions)
                    .map(|component| match component % 7 {
                        0 => -0.0,
                        1 => f32::MIN_POSITIVE,
                        2 => f32::MAX,
                        3 => f32::MIN,
                        4 => f32::from_bits(1),
                        _ => (component as f32 - 17.0) / 13.0,
                    })
                    .collect();
                db.put(id, &vector, b"not included in the digest").unwrap();
            }
            db.delete(9).unwrap();
            db.put(2, &vec![0.25; dimensions], b"new revision").unwrap();
            let collection = db.default_state();
            let mut reference = Sha256::new();
            for (&id, record) in &collection.records {
                reference.update(id.to_le_bytes());
                reference.update(
                    collection
                        .revisions
                        .get(&id)
                        .copied()
                        .unwrap_or(0)
                        .to_le_bytes(),
                );
                for value in &record.vector {
                    reference.update(value.to_bits().to_le_bytes());
                }
            }
            assert_eq!(
                content_digest(collection),
                <[u8; 32]>::from(reference.finalize())
            );
        }
    }

    #[test]
    fn size_checks_precede_reading_and_codes_are_stable() {
        let directory = tempfile::tempdir().unwrap();
        let db = Database::create(directory.path().join("data.vlite"), 2).unwrap();
        let mut file = tempfile::tempfile().unwrap();
        assert_eq!(
            read_validated(&db, &mut file, MAX_ANN_SNAPSHOT_BYTES + 1).err(),
            Some(AnnSnapshotRejection::TooLarge(MAX_ANN_SNAPSHOT_BYTES + 1))
        );
        assert_eq!(
            read_validated(&db, &mut file, 95).err(),
            Some(corrupt("truncated"))
        );
        let codes = [
            AnnSnapshotRejection::Stale("x").code(),
            corrupt("x").code(),
            AnnSnapshotRejection::UnsupportedVersion(2).code(),
            AnnSnapshotRejection::TooLarge(1).code(),
            AnnSnapshotRejection::Unreadable(io::ErrorKind::Other).code(),
        ];
        assert_eq!(
            codes,
            [
                "stale",
                "corrupt",
                "unsupported_version",
                "too_large",
                "unreadable"
            ]
        );
        assert_eq!(corrupt("checksum").to_string(), "corrupt: checksum");
    }
}
