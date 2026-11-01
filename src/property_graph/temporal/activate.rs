//! Gap 4: copy-out activation of the temporal capability (user-approved
//! decision replacing enabling it with the first temporal write).
//!
//! [`enable_temporal`] copies a closed graph-enabled file (or directory
//! store) into a new single file at `destination` whose header also carries
//! [`FLAG_TEMPORAL`], under a new lineage, like every other copy-out
//! (`logops::reencrypt`): every committed frame is copied unchanged (an
//! encrypted source is resealed under the same key with a fresh salt), so
//! sequences, revisions, absolute deadlines, precision and imports are kept
//! and nothing is compacted or reclaimed. The output is fully reloaded and
//! validated before it is published without overwrite; the source is read
//! under its exclusive lock and never modified, so its backups, restore
//! and replicas continue on its own lineage. Feeds and replicas of the copy
//! start from it (another lineage).
//!
//! Not released: gated with the rest of the temporal capability (only tests
//! enable it), with no public API until the gated surface is qualified.

use std::io::{self, Write};
use std::path::Path;

use crate::crypto::Cipher;
use crate::format::{self, FLAG_TEMPORAL};
use crate::maintenance::{Inspection, publish_with, require_complete, source};
use crate::{EncryptionKey, Error, Result};

/// Copies `source` (decrypted with `key`) into a new temporal-enabled file
/// at `destination`, which must not exist, and reports the copy.
///
/// Refuses, publishing nothing, when this build lacks the capability
/// (`UnsupportedFeature(64)`), the source is open (`Locked`), has an
/// incomplete tail, is not graph-enabled (`GraphsNotEnabled`) or already
/// has the capability (`GraphInvalidOptions`), or `destination` exists.
/// A failure after the copy's name appeared is `PublicationIndeterminate`
/// with a complete copy.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn enable_temporal(
    source_path: impl AsRef<Path>,
    destination: impl AsRef<Path>,
    key: Option<&EncryptionKey>,
) -> Result<Inspection> {
    if !super::supported() {
        return Err(Error::UnsupportedFeature(FLAG_TEMPORAL));
    }
    let mut file = source(source_path.as_ref())?;
    let loaded = format::load_with(&mut file, key)?;
    require_complete(&loaded)?;
    if loaded.graph.is_none() {
        return Err(Error::GraphsNotEnabled);
    }
    if loaded.flags & FLAG_TEMPORAL != 0 {
        return Err(Error::GraphInvalidOptions(
            "the source already has the temporal capability",
        ));
    }
    let header = format::read_log_header(&mut file, key)?;
    // A fresh salt under the same key: the copy is its own cipher domain.
    let cipher = key.map(Cipher::create).transpose()?;
    let lineage = format::new_lineage()?;
    publish_with(destination.as_ref(), key, |output| {
        put(
            output,
            &format::header_v5_flags(
                header.dimensions,
                lineage,
                header.collection_floor,
                header.flags | FLAG_TEMPORAL,
                cipher.as_ref(),
            ),
        )?;
        let mut offset = header.start;
        while let Some(frame) = format::next_frame(&mut file, &header, offset)? {
            offset += frame.disk.len() as u64;
            match &cipher {
                Some(cipher) => put(output, &cipher.seal_frame(&frame.plain)?)?,
                None => put(output, &frame.plain)?,
            }
        }
        Ok(())
    })
}

/// Writes `bytes` to the staged copy; a test [`FAULT`] cuts it short.
fn put(output: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    #[cfg(test)]
    if let Some(left) = FAULT.with(std::cell::Cell::get) {
        let written = left.min(bytes.len() as u64);
        output.write_all(&bytes[..written as usize])?;
        FAULT.with(|fault| fault.set(Some(left - written)));
        if written < bytes.len() as u64 {
            return Err(io::Error::other("injected staged write failure"));
        }
        return Ok(());
    }
    output.write_all(bytes)
}

#[cfg(test)]
thread_local! {
    /// Bytes the copies on this thread may still write before every later
    /// write fails; `None` writes everything.
    pub(crate) static FAULT: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}
