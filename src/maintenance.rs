//! Offline maintenance: locked validation and non-destructive copy-out operations.
//!
//! No function repairs sources, overwrites destinations, or replaces open files.

use crate::{Error, LockedFile, Result, format, lock};
use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactionReport {
    pub source: Inspection,
    pub output: Inspection,
}

fn source(path: &Path) -> Result<LockedFile> {
    lock(File::open(path)?)
}

fn inspection(loaded: &format::Loaded) -> Inspection {
    Inspection {
        format_version: loaded.version,
        dimensions: loaded.dimensions,
        live_records: loaded.records.len(),
        commits: loaded.sequence,
        file_bytes: loaded.end + loaded.incomplete_bytes,
        valid_bytes: loaded.end,
        incomplete_tail_bytes: loaded.incomplete_bytes,
    }
}

fn require_complete(loaded: &format::Loaded) -> Result<()> {
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
    let mut file = source(path.as_ref())?;
    Ok(inspection(&format::load(&mut file)?))
}

/// Creates a validated, byte-identical snapshot at a previously nonexistent path.
///
/// The source must be complete, valid and not owned by another database handle.
/// Output is staged in its destination directory, synced and published without
/// overwrite. Unix also syncs that directory; Windows directory-entry power-loss
/// durability is not guaranteed. On Unix, a directory-sync error after publication
/// returns PublicationIndeterminate with the already-published destination.
pub fn backup(source_path: impl AsRef<Path>, destination: impl AsRef<Path>) -> Result<Inspection> {
    let mut file = source(source_path.as_ref())?;
    let loaded = format::load(&mut file)?;
    require_complete(&loaded)?;
    let expected = inspection(&loaded);
    publish(destination.as_ref(), |output| {
        file.seek(SeekFrom::Start(0))?;
        let copied = io::copy(&mut file.0, output)?;
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
pub fn restore(backup_path: impl AsRef<Path>, destination: impl AsRef<Path>) -> Result<Inspection> {
    backup(backup_path, destination)
}

/// Writes only live records to a new standalone current-version file (v3).
///
/// The original database remains untouched. IDs, component bits, dimensions and
/// payloads are preserved; tombstones and old values are removed. Log sequences
/// restart at one. Close all handles before choosing the new file as the active
/// database; this function never renames over the source.
pub fn compact(
    source_path: impl AsRef<Path>,
    destination: impl AsRef<Path>,
) -> Result<CompactionReport> {
    let mut file = source(source_path.as_ref())?;
    let loaded = format::load(&mut file)?;
    require_complete(&loaded)?;
    let source = inspection(&loaded);
    let output = publish(destination.as_ref(), |output| {
        output.write_all(&format::header(loaded.dimensions))?;
        let mut sequence = 0u64;
        for (&id, record) in &loaded.records {
            sequence = sequence.checked_add(1).ok_or(Error::SequenceExhausted)?;
            output.write_all(&format::frame(sequence, id, Some(record)))?;
        }
        Ok(())
    })?;
    Ok(CompactionReport { source, output })
}

fn publish(destination: &Path, write: impl FnOnce(&mut File) -> Result<()>) -> Result<Inspection> {
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut staging = NamedTempFile::new_in(parent)?;
    write(staging.as_file_mut())?;
    let loaded = format::load(staging.as_file_mut())?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

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
