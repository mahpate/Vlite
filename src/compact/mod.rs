//! Compact checkpoint images (`docs/projects/roadmap/compact-storage-contract.md`).
//!
//! A `FLAG_COMPACT` image restates its checkpoint after the advance frame:
//! collection frames, the capability table, legacy and op-16 frames, and
//! last the image end block, which counts and digests the prefix of every
//! frame before it. Every reader checks each prefix here before reading its
//! body, and each op-16 body once it is verified. Slice 0 has no block codec:
//! a critical or unassigned kind refuses the whole image with
//! `UnsupportedFeature(FLAG_COMPACT)` once the table is fully validated, and
//! an authentic block of a declared derived kind is skipped and counted.
//! A reader that validated a complete image reports the skipped blocks in
//! one bounded `log` warning (`report`). Every other irregularity is
//! `Corrupt`, including a log that ends anywhere before the end block: only
//! bytes after a complete image can be a torn WAL tail.

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Skipped {
    pub collection: u16,
    pub kind: u16,
    pub version: u16,
    pub blocks: u64,
}

/// Groups kept, and listed in one skip report; the rest are only counted.
const REPORT_GROUPS: usize = 16;

/// What an image skipped, in bounded space: every skipped block and every
/// distinct (owner, kind, version) group are counted, and only the first
/// `REPORT_GROUPS` groups in image order are kept, with their block counts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Skips {
    pub blocks: u64,
    pub groups: u64,
    pub listed: Vec<Skipped>,
}

impl Skips {
    /// Counts one skipped block; `first` when it is the first of its group.
    fn add(&mut self, collection: u16, kind: u16, version: u16, first: bool) {
        self.blocks += 1;
        if first {
            self.groups += 1;
            if self.listed.len() < REPORT_GROUPS {
                if self.listed.is_empty() {
                    self.listed.reserve_exact(REPORT_GROUPS);
                }
                self.listed.push(Skipped {
                    collection,
                    kind,
                    version,
                    blocks: 1,
                });
            }
        } else if let Some(group) = self.listed.iter_mut().find(|group| {
            (group.collection, group.kind, group.version) == (collection, kind, version)
        }) {
            group.blocks += 1;
        }
    }
}

/// Reports the derived blocks that `reader` skipped in an image it fully
/// validated: one `warn` record for target `vlite::compact` through the
/// `log` facade, whose logger and sink the host chooses. Nothing is logged
/// when nothing was skipped.
pub(crate) fn report(skips: &Skips, reader: &'static str) {
    if let Some(line) = skip_report(skips, reader) {
        log::warn!(target: "vlite::compact", "{line}");
    }
}

/// The skip report as one bounded JSON object: totals, then the kept groups.
/// This build has no derived codec, so every skipped kind is unknown to it
/// (`unknown_kind`); `unsupported_version` is left for a kind whose codec
/// exists but lacks that version. Nothing is rebuilt.
pub(crate) fn skip_report(skips: &Skips, reader: &'static str) -> Option<String> {
    use std::fmt::Write;
    if skips.blocks == 0 {
        return None;
    }
    let blocks = skips.blocks;
    let mut line = format!(
        r#"{{"event":"compact_block_skip","outcome":"unknown_kind","reader":"{reader}","rebuild":"none","blocks":{blocks},"groups":["#
    );
    for (index, group) in skips.listed.iter().enumerate() {
        let separator = if index == 0 { "" } else { "," };
        let Skipped {
            collection,
            kind,
            version,
            blocks,
        } = group;
        let _ = write!(
            line,
            r#"{separator}{{"collection":{collection},"kind":{kind},"version":{version},"blocks":{blocks}}}"#
        );
    }
    let more = skips.groups - skips.listed.len() as u64;
    let _ = write!(line, r#"],"more_groups":{more}}}"#);
    Some(line)
}

/// The capability table: (kind, version) to (flags, owner of the last block
/// that used the entry). Blocks of one owner and kind form one run that
/// never resumes, so a block whose entry last served another owner (or
/// none) starts a new (owner, kind, version) group: distinct groups are
/// counted exactly without a set of them.
type Table = BTreeMap<(u16, u16), (u16, Option<u16>)>;

/// Checks the frames of a `FLAG_COMPACT` image in log order: every frame
/// prefix (`prefix`) before its body is read, each verified op-16 body
/// (`block`), and `end` where the log or the scan ends.
pub(crate) struct ImageCheck {
    version: u32,
    flags: u16,
    floor: Option<u64>,
    /// Collections created before the table; `default` always exists.
    collections: BTreeSet<u16>,
    entries: Option<Table>,
    /// The run of blocks being read (owner, kind, next partition), and runs
    /// that ended and may not resume.
    run: Option<(u16, u16, u64)>,
    ended: BTreeSet<(u16, u16)>,
    /// Image frames before the end block, from the advance on, and the
    /// SHA-256 of their prefixes.
    frames: u64,
    digest: Sha256,
    /// Skipped derived blocks, bounded.
    skips: Skips,
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
            skips: Skips::default(),
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
        let first = match entries.get_mut(&(kind, block.version)) {
            Some((flags, owner)) if *flags == block.flags => {
                owner.replace(collection) != Some(collection)
            }
            _ => return Err(corrupt(offset, "undeclared compact block")),
        };
        if matches!(kind >> 8, 0x01 | 0x02) && !self.collections.contains(&collection) {
            return Err(corrupt(offset, "compact block for unknown collection"));
        }
        // The table admits only derived kinds while no codec exists, so
        // this authentic, declared block is skipped, and reported.
        self.skips.add(collection, kind, block.version, first);
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
            entries.insert((kind, version), (flags, None));
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
        if entries.values().any(|(_, owner)| owner.is_none()) {
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

    /// The skipped derived blocks, once `end` accepted the image.
    pub(crate) fn into_skips(self) -> Skips {
        self.skips
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::{FeedCursor, read_changes};
    use crate::format::{self, FLAG_HISTORY};
    use std::io::Cursor;

    const LINEAGE: u64 = 0x5EED_0000_0000_0002;
    const FLOOR: u64 = 4;
    const KIND: u16 = 0x0200;
    const VERSIONS: u16 = 17;

    /// A plaintext frame at the checkpoint `FLOOR`.
    fn frame(owner: u16, id: u64, count: u32, body: &[u8]) -> Vec<u8> {
        let mut bytes = b"VREC".to_vec();
        bytes.push(OP_COMPACT_BLOCK);
        bytes.extend_from_slice(&owner.to_le_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&FLOOR.to_le_bytes());
        bytes.extend_from_slice(&id.to_le_bytes());
        bytes.extend_from_slice(&count.to_le_bytes());
        bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&format::crc32(&bytes).to_le_bytes());
        bytes.extend_from_slice(body);
        let checksum = format::crc32(&bytes);
        bytes.extend_from_slice(b"VCMT");
        bytes.extend_from_slice(&checksum.to_le_bytes());
        bytes.extend_from_slice(&FLOOR.to_le_bytes());
        bytes
    }

    /// An op-16 frame whose block has one unpadded section.
    fn block(
        kind: u16,
        version: u16,
        flags: u16,
        owner: u16,
        partition: u64,
        count: u32,
        section: &[u8],
    ) -> Vec<u8> {
        let start = (HEADER_LEN + 8) as u32;
        let mut body = Vec::new();
        for field in [kind, version, flags, 1] {
            body.extend_from_slice(&field.to_le_bytes());
        }
        body.extend_from_slice(&(u64::from(owner) | partition << 16).to_le_bytes());
        body.extend_from_slice(&0u64.to_le_bytes());
        for field in [start, 0, start, section.len() as u32] {
            body.extend_from_slice(&field.to_le_bytes());
        }
        body.extend_from_slice(section);
        frame(owner, u64::from(kind) | partition << 16, count, &body)
    }

    /// Two owners, `default` and `docs`, each with one run of kind
    /// `KIND` that holds every declared version in turn and then each again
    /// in reverse: 68 blocks in 34 distinct (owner, kind, version) groups.
    fn image() -> Vec<u8> {
        let mut frames = vec![
            format::advance_frame(FLOOR),
            format::collection_create_frame(
                FLOOR,
                1,
                "docs",
                2,
                &crate::CollectionSettings::default(),
            ),
        ];
        let mut table = Vec::new();
        for version in 1..=VERSIONS {
            for field in [KIND, version, 0, 0] {
                table.extend_from_slice(&field.to_le_bytes());
            }
        }
        frames.push(block(
            KIND_TABLE,
            1,
            CRITICAL,
            0,
            0,
            u32::from(VERSIONS),
            &table,
        ));
        for owner in [0, 1] {
            let versions = (1..=VERSIONS).chain((1..=VERSIONS).rev());
            for (partition, version) in (0..).zip(versions) {
                frames.push(block(KIND, version, 0, owner, partition, 1, b"x"));
            }
        }
        let mut digest = Sha256::new();
        for frame in &frames {
            digest.update(&frame[..PREFIX_SIZE]);
        }
        let mut end = (frames.len() as u64).to_le_bytes().to_vec();
        end.extend_from_slice(&digest.finalize());
        frames.push(block(KIND_END, 1, CRITICAL, 0, 0, 1, &end));
        let mut bytes = format::header_v5_flags(2, LINEAGE, 0, FLAG_HISTORY | FLAG_COMPACT, None);
        for frame in frames {
            bytes.extend_from_slice(&frame);
        }
        bytes
    }

    /// The report: every block, the first 16 groups (owner 0, versions 1 to
    /// 16, two blocks each) and the 18 other distinct groups, counted once.
    const REPORT: &str = concat!(
        r#"{"event":"compact_block_skip","outcome":"unknown_kind","reader":"load","rebuild":"none","blocks":68,"groups":["#,
        r#"{"collection":0,"kind":512,"version":1,"blocks":2},{"collection":0,"kind":512,"version":2,"blocks":2},"#,
        r#"{"collection":0,"kind":512,"version":3,"blocks":2},{"collection":0,"kind":512,"version":4,"blocks":2},"#,
        r#"{"collection":0,"kind":512,"version":5,"blocks":2},{"collection":0,"kind":512,"version":6,"blocks":2},"#,
        r#"{"collection":0,"kind":512,"version":7,"blocks":2},{"collection":0,"kind":512,"version":8,"blocks":2},"#,
        r#"{"collection":0,"kind":512,"version":9,"blocks":2},{"collection":0,"kind":512,"version":10,"blocks":2},"#,
        r#"{"collection":0,"kind":512,"version":11,"blocks":2},{"collection":0,"kind":512,"version":12,"blocks":2},"#,
        r#"{"collection":0,"kind":512,"version":13,"blocks":2},{"collection":0,"kind":512,"version":14,"blocks":2},"#,
        r#"{"collection":0,"kind":512,"version":15,"blocks":2},{"collection":0,"kind":512,"version":16,"blocks":2}"#,
        r#"],"more_groups":18}"#,
    );

    /// Review of b95a0f3: the diagnostic state that a check builds, that
    /// every reader returns and that a handle keeps stays within
    /// `REPORT_GROUPS` groups throughout, while the report still counts
    /// every block and every distinct group exactly once.
    #[test]
    fn skip_state_is_bounded_and_counts_distinct_groups_once() {
        let bytes = image();
        let mut file = Cursor::new(bytes.clone());
        let header = format::read_log_header(&mut file, None).unwrap();
        let mut check = ImageCheck::new(header.version, header.flags).unwrap();
        let mut offset = header.start;
        while let Some(frame) =
            format::next_checked_frame(&mut file, &header, offset, Some(&mut check)).unwrap()
        {
            if frame.operation == OP_COMPACT_BLOCK {
                check
                    .block(
                        frame.collection,
                        frame.id,
                        frame.count,
                        frame.body(),
                        offset,
                    )
                    .unwrap();
            }
            offset += frame.disk.len() as u64;
            kept(&check.skips);
        }
        check.end(offset).unwrap();
        assert_eq!((check.skips.blocks, check.skips.groups), (68, 34));

        let bounded = |skips: &Skips| {
            kept(skips);
            assert_eq!(skips.listed.len(), REPORT_GROUPS);
            assert_eq!(skip_report(skips, "load").as_deref(), Some(REPORT));
        };
        bounded(
            &format::load(&mut Cursor::new(bytes.clone()))
                .unwrap()
                .compact_skipped,
        );
        let db = crate::Database::open_from_bytes(bytes.clone(), None).unwrap();
        bounded(db.compact_skipped());
        let cursor = FeedCursor {
            lineage: LINEAGE,
            sequence: FLOOR,
        };
        let (_, skips) =
            read_changes(&mut Cursor::new(bytes.clone()), None, None, &cursor, 10).unwrap();
        bounded(&skips);
        let (_, skips) =
            crate::logops::read_segment(&mut Cursor::new(bytes), None, None, FLOOR, 1 << 20)
                .unwrap();
        bounded(&skips);
    }

    /// The groups a summary keeps, and their storage, never exceed
    /// `REPORT_GROUPS`, and every kept group was counted.
    fn kept(skips: &Skips) {
        assert!(
            skips.listed.len() <= REPORT_GROUPS,
            "{}",
            skips.listed.len()
        );
        assert!(
            skips.listed.capacity() <= REPORT_GROUPS,
            "{}",
            skips.listed.capacity()
        );
        assert!(skips.listed.len() as u64 <= skips.groups);
        assert!(skips.groups <= skips.blocks);
    }
}
