//! Offline maintenance: locked validation and non-destructive copy-out operations.
//!
//! No function repairs sources, overwrites destinations, or replaces open files.

use crate::crypto::Cipher;
pub use crate::logops::{
    LogSegment, RestoreReport, RestoreTarget, export_log_segment, reencrypt, restore_to,
};
#[doc(no_inline)]
pub use crate::property_graph::{
    GraphCounts, GraphInspection, GraphVerification, StripGraphReport, enable_graph, inspect_graph,
    strip_graph, verify_graph,
};
use crate::{Database, EncryptionKey, Error, LockedFile, Result, format, import, lock};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::NamedTempFile;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inspection {
    pub format_version: u32,
    pub dimensions: usize,
    pub live_records: usize,
    /// Validated log frames, not a lifetime mutation counter after compaction.
    pub commits: u64,
    pub file_bytes: u64,
    pub valid_bytes: u64,
    pub incomplete_tail_bytes: u64,
}

/// `inspect` plus v4 structure. `inspection.live_records` counts all collections.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectionDetail {
    pub inspection: Inspection,
    /// Nonzero v4 lineage; None for v1–v3.
    pub lineage: Option<u64>,
    /// Live collections including `default` (always 1 for v1–v3).
    pub collections: usize,
    pub open_imports: usize,
}

/// Result of `verify`: full validation plus a whole-file IEEE CRC-32.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verification {
    pub inspection: Inspection,
    pub bytes: u64,
    pub crc32: u32,
}

/// Result of a successful `drill`. The scratch file has already been deleted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DrillReport {
    pub backup: Inspection,
    /// Lineage assigned to the scratch restore (v4 only).
    pub scratch_lineage: Option<u64>,
    pub collections: usize,
    /// Non-empty collections whose smallest ID was found by exact k=1 search.
    pub probes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactionReport {
    pub source: Inspection,
    pub output: Inspection,
}

pub(crate) fn source(path: &Path) -> Result<LockedFile> {
    lock(File::open(path)?)
}

pub(crate) fn inspection(loaded: &format::Loaded) -> Inspection {
    Inspection {
        format_version: loaded.version,
        dimensions: loaded.dimensions,
        live_records: loaded.live_records(),
        commits: loaded.sequence,
        file_bytes: loaded.end + loaded.incomplete_bytes,
        valid_bytes: loaded.end,
        incomplete_tail_bytes: loaded.incomplete_bytes,
    }
}

pub(crate) fn require_complete(loaded: &format::Loaded) -> Result<()> {
    if loaded.incomplete_bytes != 0 {
        return Err(Error::IncompleteTail {
            offset: loaded.end,
            bytes: loaded.incomplete_bytes,
        });
    }
    Ok(())
}

/// Fully validates a closed source while holding its exclusive OS lock.
///
/// An incomplete tail is reported, not repaired. Corruption is returned as an
/// error. The source is opened read-only and its bytes are never modified.
pub fn inspect(path: impl AsRef<Path>) -> Result<Inspection> {
    inspect_inner(path.as_ref(), None)
}

/// `inspect` for an encrypted file; validates every frame's authentication tag.
pub fn inspect_with_key(path: impl AsRef<Path>, key: &EncryptionKey) -> Result<Inspection> {
    inspect_inner(path.as_ref(), Some(key))
}

fn inspect_inner(path: &Path, key: Option<&EncryptionKey>) -> Result<Inspection> {
    let mut file = source(path)?;
    Ok(inspection(&format::load_with(&mut file, key)?))
}

/// Like `inspect`, also reporting lineage, collections and open import jobs.
pub fn inspect_detail(path: impl AsRef<Path>) -> Result<InspectionDetail> {
    let mut file = source(path.as_ref())?;
    let loaded = format::load(&mut file)?;
    Ok(InspectionDetail {
        inspection: inspection(&loaded),
        lineage: loaded.lineage,
        collections: loaded.collections.len(),
        open_imports: import::Registry::from_markers(&loaded.imports)
            .open_jobs()
            .count(),
    })
}

/// Fully validates a complete closed file and computes a CRC-32 over all of
/// its bytes. An incomplete tail is an error. The file is never modified.
pub fn verify(path: impl AsRef<Path>) -> Result<Verification> {
    verify_inner(path.as_ref(), None)
}

/// `verify` for an encrypted file.
pub fn verify_with_key(path: impl AsRef<Path>, key: &EncryptionKey) -> Result<Verification> {
    verify_inner(path.as_ref(), Some(key))
}

fn verify_inner(path: &Path, key: Option<&EncryptionKey>) -> Result<Verification> {
    verify_loaded(path, key).map(|(verification, _)| verification)
}

/// `verify` that also returns the replayed file.
pub(crate) fn verify_loaded(
    path: &Path,
    key: Option<&EncryptionKey>,
) -> Result<(Verification, format::Loaded)> {
    let mut file = source(path)?;
    let loaded = format::load_with(&mut file, key)?;
    require_complete(&loaded)?;
    let inspection = inspection(&loaded);
    file.seek(SeekFrom::Start(0))?;
    let mut crc = 0u32;
    let mut bytes = 0u64;
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        crc = format::crc32_update(crc, &buffer[..read]);
        bytes += read as u64;
    }
    if bytes != inspection.file_bytes {
        return Err(io::Error::other("file size changed during verification").into());
    }
    let verification = Verification {
        inspection,
        bytes,
        crc32: crc,
    };
    Ok((verification, loaded))
}

/// Creates a validated, byte-identical snapshot at a previously nonexistent path.
///
/// The source must be complete, valid and not owned by another database handle.
/// Output is staged in its destination directory, synced and published without
/// overwrite. Unix also syncs that directory; Windows directory-entry power-loss
/// durability is not guaranteed. On Unix, a directory-sync error after publication
/// returns PublicationIndeterminate with the already-published destination.
pub fn backup(source_path: impl AsRef<Path>, destination: impl AsRef<Path>) -> Result<Inspection> {
    backup_inner(source_path.as_ref(), destination.as_ref(), None)
}

/// `backup` of an encrypted file: a byte-identical copy that stays encrypted
/// under the same key.
pub fn backup_with_key(
    source_path: impl AsRef<Path>,
    destination: impl AsRef<Path>,
    key: &EncryptionKey,
) -> Result<Inspection> {
    backup_inner(source_path.as_ref(), destination.as_ref(), Some(key))
}

fn backup_inner(
    source_path: &Path,
    destination: &Path,
    key: Option<&EncryptionKey>,
) -> Result<Inspection> {
    let mut file = source(source_path)?;
    let loaded = format::load_with(&mut file, key)?;
    require_complete(&loaded)?;
    let expected = inspection(&loaded);
    publish_with(destination, key, |output| {
        file.seek(SeekFrom::Start(0))?;
        let copied = io::copy(&mut file, output)?;
        if copied != expected.file_bytes {
            return Err(io::Error::other("source size changed during backup").into());
        }
        Ok(())
    })
}

/// Restores a valid backup into a new path. Existing paths are never replaced.
///
/// This is the same validated byte-copy contract as backup, not an in-place
/// disaster-recovery overwrite or an implicit tail repair.
/// v1–v3 backups are restored byte-identically. v4 restores are identical
/// except for a fresh lineage in the header (bytes 16..24 and header CRC), so a
/// restored copy never shares an identity with the database it came from.
pub fn restore(backup_path: impl AsRef<Path>, destination: impl AsRef<Path>) -> Result<Inspection> {
    restore_inner(backup_path.as_ref(), destination.as_ref(), None)
}

/// `restore` of an encrypted backup (same key; the restored copy keeps its
/// salt and key ID but gets a new lineage).
pub fn restore_with_key(
    backup_path: impl AsRef<Path>,
    destination: impl AsRef<Path>,
    key: &EncryptionKey,
) -> Result<Inspection> {
    restore_inner(backup_path.as_ref(), destination.as_ref(), Some(key))
}

fn restore_inner(
    backup_path: &Path,
    destination: &Path,
    key: Option<&EncryptionKey>,
) -> Result<Inspection> {
    let mut file = source(backup_path)?;
    let loaded = format::load_with(&mut file, key)?;
    require_complete(&loaded)?;
    let expected = inspection(&loaded);
    let lineage = if loaded.version >= format::V4 {
        Some(format::new_lineage()?)
    } else {
        None
    };
    publish_with(destination, key, |output| {
        file.seek(SeekFrom::Start(0))?;
        let copied = io::copy(&mut file, output)?;
        if copied != expected.file_bytes {
            return Err(io::Error::other("source size changed during restore").into());
        }
        if let Some(lineage) = lineage {
            output.seek(SeekFrom::Start(0))?;
            output.write_all(&if loaded.version >= format::V5 {
                format::header_v5_flags(
                    loaded.dimensions,
                    lineage,
                    loaded.collection_floor,
                    loaded.flags,
                    None,
                )
            } else {
                format::header_v4(loaded.dimensions, lineage, loaded.collection_floor)
            })?;
        }
        Ok(())
    })
}

/// Writes only live records to a new standalone file: v3 for v1–v3 sources,
/// v4 (with a new lineage) for v4 sources.
///
/// v4 output contains, in order: collection create frames (original IDs, names,
/// dimensions and settings), a `default` settings frame if one was persisted,
/// live records per collection, and one snapshot checkpoint per open import
/// job (cumulative row counts, last digest). Closed import jobs are not carried.
/// The original database remains untouched. IDs, component bits, dimensions and
/// payloads are preserved; tombstones and old values are removed. Log sequences
/// restart at one. Close all handles before choosing the new file as the active
/// database; this function never renames over the source.
pub fn compact(
    source_path: impl AsRef<Path>,
    destination: impl AsRef<Path>,
) -> Result<CompactionReport> {
    compact_at(source_path, destination, crate::current_time_ms())
}

/// Compacts at an explicit Unix-millisecond clock; v5 records expired at or before `now_ms` are omitted.
pub fn compact_at(
    source_path: impl AsRef<Path>,
    destination: impl AsRef<Path>,
    now_ms: u64,
) -> Result<CompactionReport> {
    compact_inner(source_path.as_ref(), destination.as_ref(), now_ms, None)
}

/// Compacts an encrypted v5 file into a new encrypted file under the same key
/// (fresh salt, so every ciphertext differs from the source).
pub fn compact_with_key(
    source_path: impl AsRef<Path>,
    destination: impl AsRef<Path>,
    key: &EncryptionKey,
) -> Result<CompactionReport> {
    compact_inner(
        source_path.as_ref(),
        destination.as_ref(),
        crate::current_time_ms(),
        Some(key),
    )
}

fn compact_inner(
    source_path: &Path,
    destination: &Path,
    now_ms: u64,
    key: Option<&EncryptionKey>,
) -> Result<CompactionReport> {
    let mut file = source(source_path)?;
    let loaded = format::load_with(&mut file, key)?;
    require_complete(&loaded)?;
    let source = inspection(&loaded);
    let cipher = key.map(Cipher::create).transpose()?;
    // Graph-enabled sources keep `FLAG_GRAPH` and their graph state; like
    // vector files, the rewrite carries no commit timestamps.
    let graph_flags = loaded.flags & format::FLAG_GRAPH;
    let output = if loaded.version >= format::V4 {
        let lineage = format::new_lineage()?;
        publish_with(destination, key, |output| {
            if loaded.version >= format::V5 {
                write_v5(
                    output,
                    &loaded,
                    lineage,
                    now_ms,
                    cipher.as_ref(),
                    graph_flags,
                )
            } else {
                write_v4(output, &loaded, lineage)
            }
        })?
    } else {
        publish(destination, |output| {
            output.write_all(&format::header(loaded.dimensions))?;
            let mut sequence = 0u64;
            for (&id, record) in loaded.default_records() {
                sequence = sequence.checked_add(1).ok_or(Error::SequenceExhausted)?;
                output.write_all(&format::frame(sequence, id, Some(record)))?;
            }
            Ok(())
        })?
    };
    Ok(CompactionReport { source, output })
}

/// Explicit offline upgrade of a closed v1–v4 file to v4; v5 sources stay v5.
///
/// IDs, vector bits, payloads and metadata are preserved in the `default`
/// collection; the output gets a new lineage. The source is never modified and
/// the destination must not exist. A v4 source is compacted as by `compact`;
/// a graph-enabled source (always v5) is copied like a plain v5 source and
/// keeps `FLAG_GRAPH`, its graph state and its node/edge high-water marks.
pub fn upgrade(
    source_path: impl AsRef<Path>,
    destination: impl AsRef<Path>,
) -> Result<CompactionReport> {
    let mut file = source(source_path.as_ref())?;
    let loaded = format::load(&mut file)?;
    require_complete(&loaded)?;
    let source = inspection(&loaded);
    let graph_flags = loaded.flags & format::FLAG_GRAPH;
    let lineage = format::new_lineage()?;
    let output = publish(destination.as_ref(), |output| {
        if loaded.version >= format::V5 {
            write_v5(
                output,
                &loaded,
                lineage,
                crate::current_time_ms(),
                None,
                graph_flags,
            )
        } else {
            write_v4(output, &loaded, lineage)
        }
    })?;
    Ok(CompactionReport { source, output })
}

fn write_v4(output: &mut File, loaded: &format::Loaded, lineage: u64) -> Result<()> {
    let mut sequence = 0u64;
    let mut next = || -> Result<u64> {
        sequence = sequence.checked_add(1).ok_or(Error::SequenceExhausted)?;
        Ok(sequence)
    };
    // The new lineage inherits the source's collection-ID high-water mark, so
    // IDs of dropped collections are never assigned again after a rewrite.
    output.write_all(&format::header_v4(
        loaded.dimensions,
        lineage,
        loaded.collection_high,
    ))?;
    for (&id, collection) in loaded.collections.range(1..) {
        output.write_all(&format::collection_create_frame(
            next()?,
            id,
            &collection.name,
            collection.dimensions,
            &collection.settings,
        ))?;
    }
    let default = &loaded.collections[&0];
    if default.settings_persisted {
        output.write_all(&format::settings_frame(next()?, 0, &default.settings))?;
    }
    for (&collection_id, collection) in &loaded.collections {
        for (&id, record) in &collection.records {
            output.write_all(&format::record_frame(
                next()?,
                collection_id,
                id,
                Some(record),
            ))?;
        }
    }
    let registry = import::Registry::from_markers(&loaded.imports);
    for (job, state) in registry.open_jobs() {
        let marker = import::snapshot_marker(job, state);
        output.write_all(&format::batch_frame_v4(
            next()?,
            state.collection,
            &[],
            Some(&marker),
        ))?;
    }
    Ok(())
}

fn drill_failure(reason: &'static str) -> Error {
    Error::Corrupt { offset: 0, reason }
}

/// Restore drill: restores `backup_path` to a new `drill-<ms>-<random>.vlite`
/// in `workdir` (fresh lineage for v4), opens it with full validation,
/// compares collections and live counts with the backup, and for every
/// non-empty collection runs an exact k=1 squared-L2 search with the vector of
/// its smallest ID, expecting that ID at distance 0. The scratch file is always
/// deleted. Mismatches are reported as `Corrupt` with offset 0. Neither the
/// backup nor any other file is modified.
pub fn drill(backup_path: impl AsRef<Path>, workdir: impl AsRef<Path>) -> Result<DrillReport> {
    let (expected, same_graph) = {
        let mut file = source(backup_path.as_ref())?;
        let loaded = format::load(&mut file)?;
        require_complete(&loaded)?;
        let expected: Vec<(String, usize)> = loaded
            .collections
            .values()
            .map(|collection| (collection.name.clone(), collection.records.len()))
            .collect();
        (expected, graph_check(loaded))
    };
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    let scratch: PathBuf = workdir.as_ref().join(format!(
        "drill-{millis}-{:016x}.vlite",
        format::new_lineage()?
    ));
    let backup = restore(backup_path, &scratch)?;
    let result = probe(&scratch, &expected, same_graph);
    let removed = std::fs::remove_file(&scratch);
    let (scratch_lineage, probes) = result?;
    removed?;
    Ok(DrillReport {
        backup,
        scratch_lineage,
        collections: expected.len(),
        probes,
    })
}

/// The drill's graph check: a graph-enabled backup must reopen to exactly
/// its replayed graph state (LEG-011).
fn graph_check(loaded: format::Loaded) -> impl FnOnce(&Database) -> bool {
    let graph = loaded.graph;
    move |db| db.graph == graph
}

fn probe(
    path: &Path,
    expected: &[(String, usize)],
    same_graph: impl FnOnce(&Database) -> bool,
) -> Result<(Option<u64>, usize)> {
    let db = Database::open(path)?;
    let infos = db.collections()?;
    if infos.len() != expected.len()
        || infos
            .iter()
            .zip(expected)
            .any(|(info, (name, live))| info.name != *name || info.live_records != *live)
    {
        return Err(drill_failure(
            "drill collection names or live counts differ",
        ));
    }
    if !same_graph(&db) {
        return Err(drill_failure("drill graph state differs"));
    }
    let mut probes = 0;
    for info in &infos {
        let collection = db.collection(&info.name)?;
        let Some(smallest) = collection.first_id() else {
            continue;
        };
        let record = collection
            .get(smallest)?
            .ok_or_else(|| drill_failure("drill smallest ID is missing"))?;
        let hits = collection.search(&record.vector, 1)?;
        if hits.len() != 1 || hits[0].id != smallest || hits[0].distance != 0.0 {
            return Err(drill_failure(
                "drill exact self-search did not return the probe",
            ));
        }
        probes += 1;
    }
    let lineage = db.lineage();
    db.close()?;
    Ok((lineage, probes))
}

fn publish(destination: &Path, write: impl FnOnce(&mut File) -> Result<()>) -> Result<Inspection> {
    publish_with(destination, None, write)
}

pub(crate) fn publish_with(
    destination: &Path,
    key: Option<&EncryptionKey>,
    write: impl FnOnce(&mut File) -> Result<()>,
) -> Result<Inspection> {
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut staging = NamedTempFile::new_in(parent)?;
    write(staging.as_file_mut())?;
    let loaded = format::load_with(staging.as_file_mut(), key)?;
    require_complete(&loaded)?;
    let report = inspection(&loaded);
    staging.as_file().sync_all()?;
    let published = staging
        .persist_noclobber(destination)
        .map_err(|error| Error::Io(error.error))?;
    // Keep the staged file's restrictive permissions and never recreate output
    // via a truncating open. A final name is visible only after a complete sync.
    drop(published);
    #[cfg(unix)]
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| Error::PublicationIndeterminate {
            destination: destination.to_path_buf(),
            source,
        })?;
    Ok(report)
}

/// Writes frames to a file, sealing them when the output is encrypted.
struct Sink<'a> {
    file: &'a mut File,
    cipher: Option<&'a Cipher>,
}

impl Sink<'_> {
    fn write_all(&mut self, frame: &[u8]) -> Result<()> {
        match self.cipher {
            Some(cipher) => self.file.write_all(&cipher.seal_frame(frame)?)?,
            None => self.file.write_all(frame)?,
        }
        Ok(())
    }
}

/// Writes a compacted v5 file: `extra_flags` are added to the header flags
/// (the encryption flag follows `cipher`), and with `FLAG_GRAPH` the source's
/// graph state follows the vector data.
pub(crate) fn write_v5(
    file: &mut File,
    loaded: &format::Loaded,
    lineage: u64,
    now_ms: u64,
    cipher: Option<&Cipher>,
    extra_flags: u16,
) -> Result<()> {
    write_v5_image(file, loaded, lineage, now_ms, cipher, extra_flags, 0)
}

/// `write_v5` with a history `floor`: when nonzero the output has
/// `FLAG_HISTORY` and starts with an advance frame at `floor`, the
/// directory-store checkpoint image. Records keep their original sequences
/// (revisions, ascending) so compare-and-set tokens survive the checkpoint;
/// collection frames restate at sequence 1 and vector, import and graph
/// frames at `floor`. Without a floor, sequences restart at one.
pub(crate) fn write_v5_image(
    file: &mut File,
    loaded: &format::Loaded,
    lineage: u64,
    now_ms: u64,
    cipher: Option<&Cipher>,
    extra_flags: u16,
    floor: u64,
) -> Result<()> {
    let mut sequence = 0u64;
    let mut next = |revision: u64| -> Result<u64> {
        if floor != 0 {
            return Ok(revision);
        }
        sequence = sequence.checked_add(1).ok_or(Error::SequenceExhausted)?;
        Ok(sequence)
    };
    let mut flags = if cipher.is_some() {
        format::FLAG_ENCRYPTED | extra_flags
    } else {
        extra_flags
    };
    if floor != 0 {
        flags |= format::FLAG_HISTORY;
    }
    file.write_all(&format::header_v5_flags(
        loaded.dimensions,
        lineage,
        loaded.collection_high,
        flags,
        cipher,
    ))?;
    let mut output = Sink { file, cipher };
    if floor != 0 {
        output.write_all(&format::advance_frame(floor))?;
    }
    for (&id, collection) in loaded.collections.range(1..) {
        output.write_all(&format::collection_create_frame(
            next(1)?,
            id,
            &collection.name,
            collection.dimensions,
            &collection.settings,
        ))?;
    }
    let default = &loaded.collections[&0];
    if default.settings_persisted {
        output.write_all(&format::settings_frame(next(1)?, 0, &default.settings))?;
    }
    let mut live = Vec::new();
    for (&collection_id, collection) in &loaded.collections {
        for &id in collection.records.keys() {
            if collection
                .expiries
                .get(&id)
                .is_some_and(|expiry| *expiry <= now_ms)
            {
                continue;
            }
            let revision = collection.revisions.get(&id).copied().unwrap_or(1);
            live.push((revision.clamp(1, floor.max(1)), collection_id, id));
        }
    }
    if floor != 0 {
        live.sort_unstable();
    }
    for (revision, collection_id, id) in live {
        let collection = &loaded.collections[&collection_id];
        output.write_all(&format::record_frame_v5(
            next(revision)?,
            collection_id,
            id,
            &collection.records[&id],
            collection.expiries.get(&id).copied(),
        ))?;
    }
    for (&collection_id, collection) in &loaded.collections {
        let mut data = collection.vectors.clone();
        data.records.retain(|id, _| {
            !collection
                .expiries
                .get(id)
                .is_some_and(|expiry| *expiry <= now_ms)
        });
        let bytes = data.encode()?;
        output.write_all(&format::vector_frame(next(floor)?, collection_id, &bytes))?;
    }
    let registry = import::Registry::from_markers(&loaded.imports);
    for (job, state) in registry.open_jobs() {
        let marker = import::snapshot_marker(job, state);
        output.write_all(&format::batch_frame_v4(
            next(floor)?,
            state.collection,
            &[],
            Some(&marker),
        ))?;
    }
    if extra_flags & format::FLAG_GRAPH != 0
        && let Some(state) = &loaded.graph
    {
        crate::property_graph::frame::compacted_bodies(state, |count, body| {
            output.write_all(&format::graph_frame(next(floor)?, count, body))
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn history_image_round_trip(key: Option<&crate::EncryptionKey>) {
        use crate::{CreateOptions, Database, Error, Expected, Metadata};
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.vlite");
        let options = CreateOptions {
            encryption: key.cloned(),
            ..CreateOptions::default()
        };
        let mut db = Database::create_with_options(&source, 2, options).unwrap();
        db.put(1, &[1.0, 0.0], b"a").unwrap();
        db.put(2, &[0.0, 1.0], b"b").unwrap();
        db.put(1, &[1.0, 1.0], b"c").unwrap();
        db.put(3, &[0.5, 0.5], b"d").unwrap();
        db.delete(3).unwrap();
        let lineage = db.lineage().unwrap();
        db.close().unwrap();

        let loaded = format::load_with(&mut File::open(&source).unwrap(), key).unwrap();
        assert_eq!(loaded.sequence, 5);
        let image = directory.path().join("image.vlite");
        let mut output = File::create_new(&image).unwrap();
        write_v5_image(
            &mut output,
            &loaded,
            lineage,
            0,
            loaded.cipher.as_ref(),
            loaded.flags & format::FLAG_GRAPH,
            loaded.sequence,
        )
        .unwrap();
        drop(output);

        let reloaded = format::load_with(&mut File::open(&image).unwrap(), key).unwrap();
        assert_eq!(reloaded.sequence, 5);
        assert_eq!(reloaded.history_floor, 5);
        assert_ne!(reloaded.flags & format::FLAG_HISTORY, 0);

        let open = |path: &Path| match key {
            Some(key) => Database::open_encrypted(path, key),
            None => Database::open(path),
        };
        let mut db = open(&image).unwrap();
        assert_eq!(db.lineage(), Some(lineage));
        assert_eq!(db.revision(1).unwrap(), Some(3));
        assert_eq!(db.revision(2).unwrap(), Some(2));
        assert_eq!(db.revision(3).unwrap(), None);
        assert!(matches!(
            db.put_with_metadata_if(
                2,
                &[0.0, 1.0],
                b"x",
                &Metadata::new(),
                Expected::Revision(1)
            ),
            Err(Error::RevisionMismatch { .. })
        ));
        let revision = db
            .put_with_metadata_if(
                2,
                &[0.0, 1.0],
                b"x",
                &Metadata::new(),
                Expected::Revision(2),
            )
            .unwrap();
        assert_eq!(revision, 6);
        let before = crate::changes::FeedCursor {
            lineage,
            sequence: 4,
        };
        assert!(matches!(
            db.changes_after(&before, 10),
            Err(Error::CursorBeforeHistory {
                cursor: 4,
                floor: 5
            })
        ));
        let at_floor = crate::changes::FeedCursor {
            lineage,
            sequence: 5,
        };
        let batch = db.changes_after(&at_floor, 10).unwrap();
        assert_eq!(batch.changes.len(), 1);
        assert_eq!(batch.head, 6);
        assert!(matches!(
            db.export_log(0, 1 << 20),
            Err(Error::CursorBeforeHistory {
                cursor: 0,
                floor: 5
            })
        ));
        assert!(db.export_log(5, 1 << 20).is_ok());
        db.close().unwrap();

        let db = open(&image).unwrap();
        assert_eq!(db.revision(1).unwrap(), Some(3));
        assert_eq!(db.revision(2).unwrap(), Some(6));
        db.close().unwrap();

        let restored = directory.path().join("restored.vlite");
        assert!(matches!(
            crate::logops::restore_to(
                None,
                &image,
                &restored,
                crate::logops::RestoreTarget::Sequence(4),
                key
            ),
            Err(Error::InvalidRestoreTarget(_))
        ));
        assert!(!restored.exists());
        crate::logops::restore_to(
            None,
            &image,
            &restored,
            crate::logops::RestoreTarget::Sequence(5),
            key,
        )
        .unwrap();
        let db = open(&restored).unwrap();
        assert_eq!(db.revision(2).unwrap(), Some(2));
        assert_ne!(db.lineage(), Some(lineage));
        db.close().unwrap();

        // Compaction drops the history flag and renumbers from one.
        let compacted = directory.path().join("compacted.vlite");
        match key {
            Some(key) => compact_with_key(&image, &compacted, key).map(|_| ()),
            None => compact(&image, &compacted).map(|_| ()),
        }
        .unwrap();
        let loaded = format::load_with(&mut File::open(&compacted).unwrap(), key).unwrap();
        assert_eq!(loaded.flags & format::FLAG_HISTORY, 0);
        assert_eq!(loaded.history_floor, 0);
    }

    #[test]
    fn history_image_preserves_revisions_and_rejects_cursors_below_floor() {
        history_image_round_trip(None);
    }

    #[test]
    fn encrypted_history_image_preserves_revisions() {
        let key = crate::EncryptionKey::from_bytes([7; 32]);
        history_image_round_trip(Some(&key));
    }

    #[test]
    fn history_image_keeps_graphs_and_open_imports_writable() {
        use crate::{Database, ImportChunkOutcome, InvalidRowPolicy, Metadata, Mutation};
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.vlite");
        let row = |id| Mutation::Put {
            id,
            vector: vec![1.0, 0.0],
            payload: Vec::new(),
        };
        let mut db = Database::create(&source, 2).unwrap();
        db.create_graph("g").unwrap();
        {
            let mut graph = db.graph_mut("g").unwrap();
            graph
                .create_node("a", &["Person"], &Metadata::new())
                .unwrap();
            graph
                .create_node("b", &["Person"], &Metadata::new())
                .unwrap();
            graph
                .create_edge(None, "a", "KNOWS", "b", &Metadata::new())
                .unwrap();
        }
        db.import_chunk(1, 1, &[row(10)], InvalidRowPolicy::RejectChunk)
            .unwrap();
        let lineage = db.lineage().unwrap();
        db.close().unwrap();

        let loaded = format::load(&mut File::open(&source).unwrap()).unwrap();
        let floor = loaded.sequence;
        let image = directory.path().join("image.vlite");
        let mut output = File::create_new(&image).unwrap();
        write_v5_image(
            &mut output,
            &loaded,
            lineage,
            0,
            None,
            loaded.flags & format::FLAG_GRAPH,
            floor,
        )
        .unwrap();
        drop(output);

        let mut db = Database::open(&image).unwrap();
        let info = db.graph("g").unwrap().info();
        assert_eq!((info.node_count, info.edge_count), (2, 1));
        assert_eq!(db.import_status(1).unwrap().last_chunk, 1);
        assert_eq!(
            db.import_chunk(1, 1, &[row(10)], InvalidRowPolicy::RejectChunk)
                .unwrap()
                .outcome,
            ImportChunkOutcome::AlreadyCommitted
        );
        db.import_chunk(1, 2, &[row(11)], InvalidRowPolicy::RejectChunk)
            .unwrap();
        db.graph_mut("g")
            .unwrap()
            .create_node("c", &["Person"], &Metadata::new())
            .unwrap();
        db.close().unwrap();

        let db = Database::open(&image).unwrap();
        assert_eq!(db.graph("g").unwrap().info().node_count, 3);
        assert_eq!(db.import_status(1).unwrap().last_chunk, 2);
        assert_eq!(db.len(), 2);
        db.close().unwrap();
    }

    #[test]
    fn failed_staging_write_or_validation_never_publishes_output() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("output.vlite");
        let result = publish(&destination, |file| {
            file.write_all(b"partial")?;
            Err(io::Error::other("injected disk full").into())
        });
        assert!(result.is_err());
        assert!(!destination.exists());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        assert!(
            publish(&destination, |file| {
                file.write_all(b"invalid file")?;
                Ok(())
            })
            .is_err()
        );
        assert!(!destination.exists());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn every_interrupted_frame_prefix_is_rejected_before_publication() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("output.vlite");
        let record = crate::Record {
            vector: vec![1.0],
            payload: b"payload".to_vec(),
            metadata: crate::Metadata::new(),
        };
        let frame = format::frame(1, 1, Some(&record));
        for cut in 1..frame.len() {
            let result = publish(&destination, |file| {
                file.write_all(&format::header(1))?;
                file.write_all(&frame[..cut])?;
                Ok(())
            });
            assert!(result.is_err(), "incomplete frame accepted at {cut}");
            assert!(!destination.exists());
            assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        }
    }

    #[test]
    fn publication_never_replaces_a_destination_created_during_staging() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("output.vlite");
        let result = publish(&destination, |file| {
            file.write_all(&format::header(1))?;
            fs::write(&destination, b"racing writer")?;
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"racing writer");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn interrupted_stage_child() {
        let Some(destination) = std::env::var_os("VLITE_INTERRUPTED_STAGE") else {
            return;
        };
        let _: Result<Inspection> = publish(Path::new(&destination), |file| {
            file.write_all(&format::header(1))?;
            file.sync_all()?;
            std::process::exit(73);
        });
        panic!("child should exit before publication");
    }

    #[test]
    fn process_exit_before_publication_does_not_expose_partial_output_or_damage_source() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.vlite");
        let destination = directory.path().join("output.vlite");
        crate::Database::create(&source, 1)
            .unwrap()
            .close()
            .unwrap();
        let original = fs::read(&source).unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "maintenance::tests::interrupted_stage_child"])
            .env("VLITE_INTERRUPTED_STAGE", &destination)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(73));
        assert!(!destination.exists());
        assert_eq!(fs::read(&source).unwrap(), original);
        // The unlinked staging name can survive process termination. Never
        // silently delete it on startup; the original remains authoritative.
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
        compact(&source, &destination).unwrap();
        crate::Database::open(destination).unwrap().close().unwrap();
    }
}
