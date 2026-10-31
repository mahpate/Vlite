//! Compact checkpoint images (`docs/projects/roadmap/compact-storage-contract.md`).
//!
//! A `FLAG_COMPACT` image restates its checkpoint after the advance frame:
//! collection frames, the capability table, legacy and op-16 frames, and
//! last the image end block, which counts and digests the prefix of every
//! frame before it. Every reader checks each prefix here before reading its
//! body, and each op-16 body once it is verified. Slice 0 has no block codec:
//! a critical or unassigned kind refuses the whole image with
//! `UnsupportedFeature(FLAG_COMPACT)` once the table is fully validated, and
//! an authentic block of a declared derived kind is skipped and counted for
//! reporting. Every other irregularity is `Corrupt`, including a log that
//! ends anywhere before the end block: only bytes after a complete image can
//! be a torn WAL tail.

use crate::format::{
    FLAG_COMPACT, OP_ADVANCE, OP_COMPACT_BLOCK, OP_GRAPH_SNAPSHOT, OP_RECORD_BLOCK, PREFIX_SIZE,
    corrupt, frame_allowed, u16_at, u32_at, u64_at,
};
use crate::{Error, Result};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// Kind of the capability table, the first op-16 frame of an image.
const KIND_TABLE: u16 = 1;
/// Kind of the image end block, the last frame of an image.
const KIND_END: u16 = 2;
/// The end block's only section: the number of image frames before it
/// (`u64`), then the SHA-256 of their prefixes in log order.
const END_SECTION: usize = 8 + 32;
/// The end block body: its header, one section entry and the section, with
/// no padding.
const END_BODY: usize = HEADER_LEN + 8 + END_SECTION;
/// Block flag bit 0: source data that must be decoded, never skipped.
const CRITICAL: u16 = 1;
/// Block header, then one `(offset, length)` entry per section.
const HEADER_LEN: usize = 32;
const MAX_SECTIONS: usize = 64;
/// A block body holds at least its header and one section entry, and at most
/// one 64 MiB allocation.
const MIN_BODY: usize = HEADER_LEN + 8;
const MAX_BODY: usize = 64 << 20;
/// Zero padding before a section only aligns it to 64 file bytes.
const MAX_PADDING: usize = 63;

/// Criticality the registry fixes for `kind`, or `None` for kinds no build
/// reads yet (storage-reserved, unallocated and test-only).
fn registered(kind: u16) -> Option<bool> {
    match kind >> 8 {
        0x01 | 0x03 => Some(true),
        0x02 | 0x04 => Some(false),
        _ => None,
    }
}

/// Authentic, declared derived blocks of one owner, kind and codec version
/// that this build skipped: nothing was decoded or rebuilt from them.
#[cfg_attr(not(feature = "server"), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Skipped {
    pub collection: u16,
    pub kind: u16,
    pub version: u16,
    pub blocks: u64,
}

/// Checks the frames of a `FLAG_COMPACT` image in log order: every frame
/// prefix (`prefix`) before its body is read, each verified op-16 body
/// (`block`), and `end` where the log or the scan ends.
pub(crate) struct ImageCheck {
    version: u32,
    flags: u16,
    floor: Option<u64>,
    /// Collections created before the table; `default` always exists.
    collections: BTreeSet<u16>,
    /// The capability table: (kind, version) to (flags, used).
    entries: Option<BTreeMap<(u16, u16), (u16, bool)>>,
    /// The run of blocks being read (owner, kind, next partition), and runs
    /// that ended and may not resume.
    run: Option<(u16, u16, u64)>,
    ended: BTreeSet<(u16, u16)>,
    /// Image frames before the end block, from the advance on, and the
    /// SHA-256 of their prefixes.
    frames: u64,
    digest: Sha256,
    /// Skipped derived blocks by (owner, kind, version).
    skipped: BTreeMap<(u16, u16, u16), u64>,
    /// The end block matched: the image is complete.
    done: bool,
}

impl ImageCheck {
    pub(crate) fn new(version: u32, flags: u16) -> Option<Self> {
        (flags & FLAG_COMPACT != 0).then(|| Self {
            version,
            flags,
            floor: None,
            collections: BTreeSet::from([0]),
            entries: None,
            run: None,
            ended: BTreeSet::new(),
            frames: 0,
            digest: Sha256::new(),
            skipped: BTreeMap::new(),
            done: false,
        })
    }

    /// Checks a frame prefix whose magic and checksum were verified, before
    /// any of its body is read or allocated.
    pub(crate) fn prefix(&mut self, prefix: &[u8; PREFIX_SIZE], offset: u64) -> Result<()> {
        let operation = prefix[4];
        let collection = u16_at(prefix, 5);
        let sequence = u64_at(prefix, 8);
        let id = u64_at(prefix, 16);
        let body_len = u32_at(prefix, 28) as usize;
        let Some(floor) = self.floor else {
            if operation != OP_ADVANCE {
                return Err(corrupt(offset, "history image without an advance frame"));
            }
            if prefix[5..8] != [0; 3]
                || id != 0
                || u32_at(prefix, 24) != 0
                || body_len != 0
                || sequence == 0
            {
                return Err(corrupt(offset, "invalid advance frame"));
            }
            self.floor = Some(sequence);
            self.hash(prefix);
            return Ok(());
        };
        if operation == OP_ADVANCE {
            return Err(corrupt(offset, "invalid advance frame"));
        }
        // Time anchors are not allowed either: images never have
        // `FLAG_TIMESTAMPS`.
        if !frame_allowed(self.version, self.flags, prefix) {
            return Err(corrupt(
                offset,
                "invalid operation or reserved record bytes",
            ));
        }
        if matches!(
            operation,
            OP_RECORD_BLOCK | OP_GRAPH_SNAPSHOT | OP_COMPACT_BLOCK
        ) && sequence != floor
        {
            return Err(corrupt(offset, "snapshot frame outside a checkpoint image"));
        }
        if sequence > floor {
            return self.end(offset);
        }
        if self.done {
            return Err(corrupt(
                offset,
                "restated frame after the compact checkpoint image",
            ));
        }
        if operation != OP_COMPACT_BLOCK {
            match (self.entries.is_some(), operation) {
                (false, 5) => {
                    if let Ok(id) = u16::try_from(id) {
                        self.collections.insert(id);
                    }
                }
                (false, 7) => {}
                (false, _) => {
                    return Err(corrupt(
                        offset,
                        "checkpoint frame before the capability table",
                    ));
                }
                (true, 5..=7) => {
                    return Err(corrupt(
                        offset,
                        "collection frame after the capability table",
                    ));
                }
                (true, _) => self.end_run(),
            }
            self.hash(prefix);
            return Ok(());
        }
        if !(MIN_BODY..=MAX_BODY).contains(&body_len) {
            return Err(corrupt(offset, "invalid compact block length"));
        }
        let kind = id as u16;
        let partition = id >> 16;
        if id >> 48 != 0 || kind == 0 {
            return Err(corrupt(offset, "invalid compact block prefix"));
        }
        match (self.entries.is_some(), kind) {
            (false, KIND_TABLE) if collection != 0 || partition != 0 => {
                return Err(corrupt(offset, "invalid capability table"));
            }
            (false, KIND_TABLE) => {}
            (false, _) => {
                return Err(corrupt(offset, "compact image without a capability table"));
            }
            (true, KIND_TABLE) => return Err(corrupt(offset, "duplicate capability table")),
            (true, KIND_END) if collection != 0 || partition != 0 => {
                return Err(corrupt(offset, "invalid compact image end"));
            }
            // The end block is not part of its own digest.
            (true, KIND_END) => {
                self.end_run();
                return Ok(());
            }
            (true, _) => {
                // Blocks of one owner and kind are consecutive frames with
                // partitions 0, 1, 2, ...
                let next = match self.run {
                    Some((owner, run_kind, next)) if (owner, run_kind) == (collection, kind) => {
                        Some(next)
                    }
                    _ => {
                        self.end_run();
                        (!self.ended.contains(&(collection, kind))).then_some(0)
                    }
                };
                if next != Some(partition) {
                    return Err(corrupt(offset, "non-canonical compact block partition"));
                }
                self.run = Some((collection, kind, partition + 1));
            }
        }
        self.hash(prefix);
        Ok(())
    }

    /// Checks the verified (and decrypted) body of an op-16 frame whose
    /// prefix `prefix` accepted.
    pub(crate) fn block(
        &mut self,
        collection: u16,
        id: u64,
        count: u32,
        body: &[u8],
        offset: u64,
    ) -> Result<()> {
        let block = Envelope::parse(collection, id, body, offset)?;
        let kind = id as u16;
        match kind {
            KIND_TABLE => return self.table(count, &block, offset),
            KIND_END => return self.end_block(count, &block, body.len(), offset),
            _ => {}
        }
        let Some(entries) = self.entries.as_mut() else {
            return Err(corrupt(offset, "compact image without a capability table"));
        };
        match entries.get_mut(&(kind, block.version)) {
            Some((flags, used)) if *flags == block.flags => *used = true,
            _ => return Err(corrupt(offset, "undeclared compact block")),
        }
        if matches!(kind >> 8, 0x01 | 0x02) && !self.collections.contains(&collection) {
            return Err(corrupt(offset, "compact block for unknown collection"));
        }
        // The table admits only derived kinds while no codec exists, so
        // this authentic, declared block is skipped, and reported.
        *self
            .skipped
            .entry((collection, kind, block.version))
            .or_default() += 1;
        Ok(())
    }

    /// Validates the whole table before deciding support, so damage is
    /// never reported as a missing capability.
    fn table(&mut self, count: u32, block: &Envelope<'_>, offset: u64) -> Result<()> {
        if block.version != 1 {
            return Err(Error::UnsupportedFeature(FLAG_COMPACT));
        }
        let data = block.sections[0];
        let (table, rest) = data.as_chunks::<8>();
        if block.flags != CRITICAL
            || block.sections.len() != 1
            || table.is_empty()
            || !rest.is_empty()
            || table.len() != count as usize
        {
            return Err(corrupt(offset, "invalid capability table"));
        }
        let mut entries = BTreeMap::new();
        let mut previous = None;
        let mut readable = true;
        for entry in table {
            let (kind, version, flags) = (u16_at(entry, 0), u16_at(entry, 2), u16_at(entry, 4));
            let class = registered(kind);
            // The table lists neither itself nor the end block.
            if kind <= KIND_END
                || version == 0
                || flags & !CRITICAL != 0
                || u16_at(entry, 6) != 0
                || class.is_some_and(|critical| critical != (flags == CRITICAL))
            {
                return Err(corrupt(offset, "invalid capability table entry"));
            }
            if previous.is_some_and(|previous| previous >= (kind, version)) {
                return Err(corrupt(
                    offset,
                    "unsorted or duplicate capability table entries",
                ));
            }
            previous = Some((kind, version));
            // No codec exists yet: only declared derived kinds can be skipped.
            readable &= class == Some(false);
            entries.insert((kind, version), (flags, false));
        }
        if !readable {
            return Err(Error::UnsupportedFeature(FLAG_COMPACT));
        }
        self.entries = Some(entries);
        Ok(())
    }

    /// Completes the image: the end block's count and digest match every
    /// frame before it, and every table entry was used.
    fn end_block(
        &mut self,
        count: u32,
        block: &Envelope<'_>,
        body_len: usize,
        offset: u64,
    ) -> Result<()> {
        if block.version != 1 {
            return Err(Error::UnsupportedFeature(FLAG_COMPACT));
        }
        let [section] = block.sections[..] else {
            return Err(corrupt(offset, "invalid compact image end"));
        };
        if block.flags != CRITICAL
            || count != 1
            || section.len() != END_SECTION
            || body_len != END_BODY
        {
            return Err(corrupt(offset, "invalid compact image end"));
        }
        if u64_at(section, 0) != self.frames || section[8..] != self.digest.clone().finalize()[..] {
            return Err(corrupt(
                offset,
                "compact image end does not match the image",
            ));
        }
        let Some(entries) = &self.entries else {
            return Err(corrupt(offset, "compact image without a capability table"));
        };
        if entries.values().any(|&(_, used)| !used) {
            return Err(corrupt(offset, "capability table entry is unused"));
        }
        self.done = true;
        Ok(())
    }

    /// The log or scan ends at `offset`, cleanly or inside a frame, or the
    /// first frame after the image starts there. Only a complete image may
    /// end: anything cut before its end block is corruption, never a torn
    /// tail to repair.
    pub(crate) fn end(&self, offset: u64) -> Result<()> {
        if !self.done {
            return Err(corrupt(offset, "incomplete compact checkpoint image"));
        }
        Ok(())
    }

    /// Skipped derived blocks, by owner, kind and codec version.
    pub(crate) fn skipped(&self) -> Vec<Skipped> {
        self.skipped
            .iter()
            .map(|(&(collection, kind, version), &blocks)| Skipped {
                collection,
                kind,
                version,
                blocks,
            })
            .collect()
    }

    fn hash(&mut self, prefix: &[u8; PREFIX_SIZE]) {
        self.frames += 1;
        self.digest.update(prefix);
    }

    fn end_run(&mut self) {
        if let Some((owner, kind, _)) = self.run.take() {
            self.ended.insert((owner, kind));
        }
    }
}

/// A checked block header and its sections.
struct Envelope<'a> {
    version: u16,
    flags: u16,
    sections: Vec<&'a [u8]>,
}

impl<'a> Envelope<'a> {
    fn parse(collection: u16, id: u64, body: &'a [u8], offset: u64) -> Result<Self> {
        let version = u16_at(body, 2);
        let flags = u16_at(body, 4);
        let count = usize::from(u16_at(body, 6));
        let object = u64_at(body, 8);
        let digest = u64_at(body, 16);
        if version == 0
            || flags & !CRITICAL != 0
            || !(1..=MAX_SECTIONS).contains(&count)
            || object >> 48 != 0
            || (flags & CRITICAL != 0 && digest != 0)
            || u32_at(body, 28) != 0
        {
            return Err(corrupt(offset, "invalid compact block header"));
        }
        if u16_at(body, 0) != id as u16 || object != u64::from(collection) | (id >> 16) << 16 {
            return Err(corrupt(
                offset,
                "compact block header does not match its frame",
            ));
        }
        // Sections ascend without overlap, each after at most 63 zero
        // padding bytes, and the last one ends the body.
        let invalid = || corrupt(offset, "invalid compact block sections");
        let mut end = HEADER_LEN + 8 * count;
        if body.len() < end {
            return Err(invalid());
        }
        let mut sections = Vec::with_capacity(count);
        for index in 0..count {
            let entry = HEADER_LEN + 8 * index;
            let start = u32_at(body, entry) as usize;
            let length = u32_at(body, entry + 4) as usize;
            let stop = start
                .checked_add(length)
                .filter(|&stop| start >= end && stop <= body.len())
                .ok_or_else(invalid)?;
            if (index == 0 && start != u32_at(body, 24) as usize)
                || start - end > MAX_PADDING
                || body[end..start].iter().any(|&byte| byte != 0)
            {
                return Err(invalid());
            }
            sections.push(&body[start..stop]);
            end = stop;
        }
        if end != body.len() {
            return Err(invalid());
        }
        Ok(Self {
            version,
            flags,
            sections,
        })
    }
}
