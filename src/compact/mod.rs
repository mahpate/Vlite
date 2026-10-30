//! Compact checkpoint images (`docs/projects/roadmap/compact-storage-contract.md`).
//!
//! A `FLAG_COMPACT` image restates its checkpoint with op-16 blocks after a
//! capability table. Slice 0 checks the table and every block envelope; it
//! has no block codec yet. A critical or unassigned kind refuses the whole
//! image with `UnsupportedFeature(FLAG_COMPACT)` once the table is fully
//! validated, and an authentic block of a declared derived kind is skipped.
//! Every other irregularity is `Corrupt`; nothing is treated as a torn tail.

use crate::format::{
    FLAG_COMPACT, LogHeader, OP_COMPACT_BLOCK, PREFIX_SIZE, corrupt, u16_at, u32_at, u64_at,
};
use crate::{Error, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, SeekFrom};

/// Kind of the capability table, the first op-16 frame of an image.
const KIND_TABLE: u16 = 1;
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

/// Checks the frames of a `FLAG_COMPACT` image in log order: the advance,
/// then each frame prefix (`frame`) and op-16 body (`block`), and `finish`
/// at the first frame after the image or at the end of the log.
pub(crate) struct ImageCheck {
    floor: Option<u64>,
    /// Collections created before the table; `default` always exists.
    collections: BTreeSet<u16>,
    /// The capability table: (kind, version) to (flags, used).
    entries: Option<BTreeMap<(u16, u16), (u16, bool)>>,
    /// The run of blocks being read (owner, kind, next partition), and runs
    /// that ended and may not resume.
    run: Option<(u16, u16, u64)>,
    ended: BTreeSet<(u16, u16)>,
    done: bool,
}

impl ImageCheck {
    pub(crate) fn new(flags: u16) -> Option<Self> {
        (flags & FLAG_COMPACT != 0).then(|| Self {
            floor: None,
            collections: BTreeSet::from([0]),
            entries: None,
            run: None,
            ended: BTreeSet::new(),
            done: false,
        })
    }

    pub(crate) fn advance(&mut self, floor: u64, offset: u64) -> Result<()> {
        if self.floor.replace(floor).is_some() {
            return Err(corrupt(offset, "invalid advance frame"));
        }
        Ok(())
    }

    /// Checks a frame prefix before its body is read.
    pub(crate) fn frame(
        &mut self,
        operation: u8,
        collection: u16,
        sequence: u64,
        id: u64,
        body_len: usize,
        offset: u64,
    ) -> Result<()> {
        let Some(floor) = self.floor else {
            return Err(corrupt(offset, "history image without an advance frame"));
        };
        if sequence > floor {
            if operation == OP_COMPACT_BLOCK {
                return Err(corrupt(offset, "snapshot frame outside a checkpoint image"));
            }
            return self.finish(offset);
        }
        if self.done {
            return Err(corrupt(
                offset,
                "restated frame after the compact checkpoint image",
            ));
        }
        if operation != OP_COMPACT_BLOCK {
            return match (self.entries.is_some(), operation) {
                (false, 5) => {
                    if let Ok(id) = u16::try_from(id) {
                        self.collections.insert(id);
                    }
                    Ok(())
                }
                (false, 7) => Ok(()),
                (false, _) => Err(corrupt(
                    offset,
                    "checkpoint frame before the capability table",
                )),
                (true, 5..=7) => Err(corrupt(
                    offset,
                    "collection frame after the capability table",
                )),
                (true, _) => {
                    self.end_run();
                    Ok(())
                }
            };
        }
        if sequence != floor {
            return Err(corrupt(offset, "snapshot frame outside a checkpoint image"));
        }
        if !(MIN_BODY..=MAX_BODY).contains(&body_len) {
            return Err(corrupt(offset, "invalid compact block length"));
        }
        let kind = id as u16;
        let partition = id >> 16;
        if id >> 48 != 0 || kind == 0 {
            return Err(corrupt(offset, "invalid compact block prefix"));
        }
        match (self.entries.is_some(), kind == KIND_TABLE) {
            (false, false) => Err(corrupt(offset, "compact image without a capability table")),
            (false, true) if collection != 0 || partition != 0 => {
                Err(corrupt(offset, "invalid capability table"))
            }
            (false, true) => Ok(()),
            (true, true) => Err(corrupt(offset, "duplicate capability table")),
            (true, false) => {
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
                Ok(())
            }
        }
    }

    /// Checks the verified (and decrypted) body of an op-16 frame whose
    /// prefix `frame` accepted.
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
        if kind == KIND_TABLE {
            return self.table(count, &block, offset);
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
        // this authentic, declared block is skipped.
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
            if kind <= KIND_TABLE
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

    /// The log ends inside the frame at `offset`, whose first bytes (at most
    /// a prefix) are `bytes`. A torn write keeps the start of a frame, so its
    /// sequence tells a torn image frame (corruption, never repaired) from a
    /// torn first log frame after the image. Until the image has ended,
    /// fewer than 16 bytes cannot tell them apart, and the image fails closed.
    pub(crate) fn torn(&self, bytes: &[u8], offset: u64) -> Result<()> {
        let after_image = bytes
            .get(8..16)
            .zip(self.floor)
            .is_some_and(|(sequence, floor)| u64_at(sequence, 0) > floor);
        if !self.done && !after_image {
            return Err(corrupt(offset, "torn compact checkpoint image"));
        }
        Ok(())
    }

    /// Ends the image: the table exists and every entry was used.
    pub(crate) fn finish(&mut self, offset: u64) -> Result<()> {
        if self.done {
            return Ok(());
        }
        let Some(entries) = &self.entries else {
            return Err(corrupt(offset, "compact image without a capability table"));
        };
        if entries.values().any(|&(_, used)| !used) {
            return Err(corrupt(offset, "capability table entry is unused"));
        }
        self.done = true;
        self.end_run();
        Ok(())
    }

    fn end_run(&mut self) {
        if let Some((owner, kind, _)) = self.run.take() {
            self.ended.insert((owner, kind));
        }
    }
}

/// Ends a raw frame scan (change feed, segment export) stopped at `offset`
/// by the end of the log, a full batch or a frame that does not fit.
pub(crate) fn end_scan<R: Read + Seek>(
    check: Option<ImageCheck>,
    file: &mut R,
    header: &LogHeader,
    offset: u64,
) -> Result<()> {
    let Some(mut check) = check else {
        return Ok(());
    };
    // A full batch stops only after the image ended; otherwise the scan
    // reached the end of the log, and any bytes left start a torn frame.
    let left = (header.length - offset).min(PREFIX_SIZE as u64) as usize;
    if !check.done && left != 0 {
        let mut bytes = vec![0; left];
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut bytes)?;
        check.torn(&bytes, offset)?;
    }
    check.finish(offset)
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
