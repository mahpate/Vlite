//! Compact checkpoint images (`docs/projects/roadmap/compact-storage-contract.md`).
//!
//! A `FLAG_COMPACT` image restates its checkpoint after the advance frame:
//! collection frames, the capability table, legacy and op-16 frames, and
//! last the image end block, which counts and digests the prefix of every
//! frame before it. Every reader checks each prefix here before reading its
//! body, and each op-16 body once it is verified; in a `FLAG_PRECISION`
//! image also the collection creates and graph snapshots that its catalog,
//! import registry and revision blocks must match (P3a), so full replay,
//! the change feed and segment export accept the same images. Slice 0 has
//! no block codec: a critical or unassigned kind refuses the whole image with
//! `UnsupportedFeature(FLAG_COMPACT)` once the table is fully validated, and
//! an authentic block of a declared derived kind is skipped and counted.
//! A reader that validated a complete image reports the skipped blocks in
//! one bounded `log` warning (`report`). Every other irregularity is
//! `Corrupt`, including a log that ends anywhere before the end block: only
//! bytes after a complete image can be a torn WAL tail.

use crate::format::{
    FLAG_COMPACT, FLAG_PRECISION, MAX_COLLECTIONS, OP_ADVANCE, OP_COLLECTION_PRECISE,
    OP_COMPACT_BLOCK, OP_GRAPH_SNAPSHOT, OP_RECORD_BLOCK, PREFIX_SIZE, RawFrame, corrupt,
    frame_allowed, u16_at, u32_at, u64_at,
};
use crate::property_graph::engine::{GraphBody, GraphEntry};
use crate::{Error, Result};
use catalog::{KIND_CATALOG, KIND_IMPORTS, KIND_REVISIONS, Precision};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// P3a precision policy storage: the catalog, import registry and graph
/// revision kinds of a `FLAG_PRECISION` image.
pub(crate) mod catalog;
/// P3a canonical float16 conversion, shared by the dense rows codec and
/// the replay check of float16 collections.
pub(crate) mod half;
/// P3a dense rows codec. Preactivation: compiled for tests only, so no
/// reader accepts its kind and no writer emits it until its write, read,
/// replay and maintenance paths are qualified (contract decision 7).
#[cfg(test)]
mod vectors;

/// Whether this build reads and writes `FLAG_PRECISION` stores (P3a).
/// Preactivation: release builds refuse them with
/// `UnsupportedFeature(FLAG_PRECISION)`; only this crate's tests switch
/// support on, per thread, with an `Activation`, until every write, read,
/// replay and maintenance path is qualified.
pub(crate) fn precision_supported() -> bool {
    #[cfg(test)]
    {
        PRECISION.with(std::cell::Cell::get)
    }
    #[cfg(not(test))]
    {
        false
    }
}

#[cfg(test)]
thread_local! {
    static PRECISION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Test-only switch of `precision_supported` on this thread, restored when
/// dropped.
#[cfg(test)]
pub(crate) struct Activation(bool);

#[cfg(test)]
impl Activation {
    pub(crate) fn on() -> Self {
        Self::set(true)
    }

    pub(crate) fn set(supported: bool) -> Self {
        Self(PRECISION.with(|cell| cell.replace(supported)))
    }
}

#[cfg(test)]
impl Drop for Activation {
    fn drop(&mut self) {
        PRECISION.with(|cell| cell.set(self.0));
    }
}

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
const ALIGN: u64 = 64;

/// A critical version-1 block body of `kind` for `owner` and `partition`
/// with `sections` in order and no source digest. Given the body's file
/// offset, each section starts on a 64-byte file boundary; without it
/// there is no padding.
fn envelope(
    kind: u16,
    owner: u16,
    partition: u64,
    sections: &[&[u8]],
    body_offset: Option<u64>,
) -> Vec<u8> {
    let mut body = Vec::new();
    for field in [kind, 1, CRITICAL, sections.len() as u16] {
        body.extend_from_slice(&field.to_le_bytes());
    }
    let object = u64::from(owner) | partition << 16;
    body.extend_from_slice(&object.to_le_bytes());
    // No source digest; the first section's offset is filled in below.
    body.resize(HEADER_LEN + 8 * sections.len(), 0);
    for (index, section) in sections.iter().enumerate() {
        if let Some(offset) = body_offset {
            let padding = offset.wrapping_add(body.len() as u64).wrapping_neg() % ALIGN;
            body.resize(body.len() + padding as usize, 0);
        }
        // Every block is far below 4 GiB (`MAX_BODY`).
        let start = (body.len() as u32).to_le_bytes();
        let entry = HEADER_LEN + 8 * index;
        body[entry..entry + 4].copy_from_slice(&start);
        body[entry + 4..entry + 8].copy_from_slice(&(section.len() as u32).to_le_bytes());
        if index == 0 {
            body[24..28].copy_from_slice(&start);
        }
        body.extend_from_slice(section);
    }
    body
}

/// An op-16 frame at `sequence` with an unpadded critical version-1 block.
fn block_frame(
    sequence: u64,
    kind: u16,
    owner: u16,
    partition: u64,
    count: u32,
    sections: &[&[u8]],
) -> Vec<u8> {
    let body = envelope(kind, owner, partition, sections, None);
    crate::format::compact_block_frame(
        sequence,
        owner,
        u64::from(kind) | partition << 16,
        count,
        &body,
    )
}

/// The image end block over `frames` image frames with prefix `digest`.
pub(crate) fn end_frame(floor: u64, frames: u64, digest: [u8; 32]) -> Vec<u8> {
    let mut section = Vec::with_capacity(END_SECTION);
    section.extend_from_slice(&frames.to_le_bytes());
    section.extend_from_slice(&digest);
    block_frame(floor, KIND_END, 0, 0, 1, &[&section])
}

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

/// The parts of a `FLAG_PRECISION` image after its capability table, in
/// order (P3a): the catalog, the import registry, record blocks, vector
/// frames, then per graph its snapshot frames and revision run, derived
/// blocks and the end block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Table,
    Catalog,
    Imports,
    Records,
    Vectors,
    Snapshot,
    Revisions,
    Derived,
}

/// The graph revision run being read: its graph, the last node and edge IDs
/// so far, and whether its first block was empty (an empty graph's only
/// block).
struct RevisionRun {
    graph: u16,
    node: Option<u64>,
    edge: Option<u64>,
    empty: bool,
}

/// The node and edge IDs one graph's snapshot frames put, or that its
/// revision run lists, counted and digested in ascending order.
#[derive(Default)]
struct Ids {
    count: u64,
    digest: Sha256,
}

impl Ids {
    fn add(&mut self, id: u64) {
        self.count += 1;
        self.digest.update(id.to_le_bytes());
    }

    fn matches(&self, other: &Self) -> bool {
        self.count == other.count
            && self.digest.clone().finalize() == other.digest.clone().finalize()
    }
}

/// How far a precision image restated one graph (P3a, Decision P): the
/// nodes and edges of its snapshot frames, and whether its revision run
/// began and what it listed so far. The run must list exactly those nodes
/// and edges, which bounded state checks without holding the snapshot.
struct Coverage {
    graph: u16,
    listed: bool,
    /// Nodes and edges put by the snapshot, then listed by the run.
    snapshot: [Ids; 2],
    revisions: [Ids; 2],
}

/// What a block of a precision image restates, for the loader (P3a).
pub(crate) enum Supplement {
    Catalog(Vec<catalog::Entry>),
    Imports(Box<crate::import::Registry>),
    Revisions {
        graph: u16,
        partition: u64,
        nodes: catalog::Pairs,
        edges: catalog::Pairs,
    },
}

/// A live collection of a precision log: its name, dimensions and stored
/// precision, which the catalog gives the collections an image restates
/// and op 17 the ones created after it; and its live records, each noted
/// with whether it expires, as frames restate, write and delete them.
struct Live {
    name: String,
    dimensions: usize,
    precision: Option<Precision>,
    records: BTreeMap<u64, bool>,
}

/// Checks the frames of a `FLAG_COMPACT` image in log order: every frame
/// prefix (`prefix`) before its body is read, each verified op-16 body
/// (`block`), and `end` where the log or the scan ends.
pub(crate) struct ImageCheck {
    version: u32,
    flags: u16,
    /// A `FLAG_PRECISION` image (P3a), its current part, revision run and
    /// the coverage of the graph being restated.
    precise: bool,
    stage: Stage,
    revisions: Option<RevisionRun>,
    coverage: Option<Coverage>,
    floor: Option<u64>,
    /// Collections created before the table; `default` always exists.
    collections: BTreeSet<u16>,
    /// In a precision log, each live collection: those restated before the
    /// table, then those that frames after the image create or drop; and
    /// the highest collection ID the log has used (the header floor or a
    /// restated or created ID). The catalog and import registry must match
    /// the restated ones, and every dense value written into one must be
    /// exactly a value of its precision.
    described: BTreeMap<u16, Live>,
    collection_high: u16,
    /// In a precision log, its import jobs, followed as replay follows
    /// them; and the graph snapshot bodies read so far with the offset of
    /// the first, loaded as replay loads them where the snapshot ends.
    jobs: crate::format::ImportJobs,
    pending: Option<(u64, Vec<GraphBody>)>,
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
    /// A check for a log whose header has `version`, `flags`, default
    /// collection `dimensions` and `collection_floor`, if it is compact.
    pub(crate) fn new(
        version: u32,
        flags: u16,
        dimensions: usize,
        collection_floor: u16,
    ) -> Option<Self> {
        (flags & FLAG_COMPACT != 0).then(|| Self {
            version,
            flags,
            precise: flags & FLAG_PRECISION != 0,
            stage: Stage::Table,
            revisions: None,
            coverage: None,
            floor: None,
            collections: BTreeSet::from([0]),
            described: BTreeMap::from([(
                0,
                Live {
                    name: "default".to_string(),
                    dimensions,
                    precision: None,
                    records: BTreeMap::new(),
                },
            )]),
            collection_high: collection_floor,
            jobs: Default::default(),
            pending: None,
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
        // A precision log creates collections only with op 17, and only
        // after its image, which restates them with op 5 and its catalog.
        if self.precise {
            if operation == OP_COLLECTION_PRECISE
                && (sequence <= floor || !crate::format::precise_prefix_valid(prefix))
            {
                return Err(corrupt(offset, "invalid precise collection create"));
            }
            if operation == 5 && sequence > floor {
                return Err(corrupt(offset, "collection create without a precision"));
            }
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
                (true, _) => {
                    self.end_run();
                    if self.precise {
                        self.next_stage(operation, 0, 0, offset)?;
                    }
                }
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
        if self.precise && self.entries.is_some() && kind != KIND_TABLE {
            self.next_stage(OP_COMPACT_BLOCK, kind, partition, offset)?;
            let storage = matches!(kind, KIND_CATALOG | KIND_IMPORTS | KIND_REVISIONS);
            if (storage && body_len > catalog::max_body(kind))
                || (matches!(kind, KIND_CATALOG | KIND_IMPORTS)
                    && (collection != 0 || partition != 0))
            {
                return Err(corrupt(offset, "invalid precision image block"));
            }
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

    /// Checks the verified (and decrypted) body of any frame whose prefix
    /// `prefix` accepted, for readers that do not replay the log: op-16
    /// blocks (`block`); and in a precision log every frame as full replay
    /// checks it. The collection creates (`created`) and graph snapshots
    /// (`snapshot`) must match its catalog, import registry and revision
    /// runs, and each snapshot loads as replay loads it, with its
    /// attachments checked against the live records, where it ends
    /// (`load_snapshot`). Every other frame passes replay's checks of its
    /// count, ID, length, target collection and body, and every dense value
    /// it writes must be exactly a value of its collection's precision
    /// (`format::check_frame_writes`); then the collections, records and
    /// import jobs it changes are followed as replay follows them, so a
    /// drop must name a live collection with no records or open import job,
    /// settings a live collection, a record block records above the
    /// collection's, and an import chunk must continue its job. Full replay
    /// decodes those frames itself and makes the same checks.
    pub(crate) fn frame(&mut self, frame: &RawFrame, offset: u64) -> Result<Option<Supplement>> {
        let body = frame.body();
        // In a precision image, each graph's revision blocks continue its
        // snapshot frames.
        if self.precise
            && frame.operation != OP_GRAPH_SNAPSHOT
            && (frame.operation != OP_COMPACT_BLOCK || frame.id as u16 != KIND_REVISIONS)
        {
            self.load_snapshot()?;
        }
        match frame.operation {
            OP_COMPACT_BLOCK => {
                let supplement =
                    self.block(frame.collection, frame.id, frame.count, body, offset)?;
                if let Some(Supplement::Imports(registry)) = &supplement {
                    self.jobs.restate(registry);
                }
                Ok(supplement)
            }
            // The prefix check admits op 17 only after the image.
            OP_COLLECTION_PRECISE => {
                let (precision, name, dimensions, _) = crate::format::decode_precise(body, offset)?;
                self.created(frame.id as u16, name, dimensions, Some(precision), offset)
                    .map(|()| None)
            }
            OP_GRAPH_SNAPSHOT if self.precise => {
                let snapshot = crate::property_graph::frame::decode_frame(
                    true,
                    frame.id,
                    frame.count,
                    body,
                    offset,
                )?;
                self.snapshot(&snapshot, offset)?;
                self.pending
                    .get_or_insert_with(|| (offset, Vec::new()))
                    .1
                    .push(snapshot);
                Ok(None)
            }
            _ if self.precise => {
                let floor = self.floor.expect("frames follow the advance");
                let described = &self.described;
                let writes = crate::format::check_frame_writes(
                    frame,
                    self.version,
                    floor,
                    offset,
                    |id| {
                        described
                            .get(&id)
                            .map(|live| (live.dimensions, live.precision))
                    },
                    |id, record| {
                        described
                            .get(&id)
                            .is_some_and(|live| live.records.contains_key(&record))
                    },
                )?;
                // The ID of a collection frame that passed is a collection
                // ID.
                let collection = frame.id as u16;
                match frame.operation {
                    // The prefix check admits op 5 in a precision log only
                    // in its image, before the table.
                    5 => {
                        let (name, dimensions, _) = crate::format::decode_create(body, offset)?;
                        self.created(collection, name, dimensions, None, offset)?;
                    }
                    6 => {
                        let live = self
                            .described
                            .get(&collection)
                            .ok_or_else(|| corrupt(offset, "drop of unknown collection"))?;
                        if !live.records.is_empty() || self.jobs.open_in(collection) {
                            return Err(corrupt(offset, "drop of non-empty collection"));
                        }
                        self.described.remove(&collection);
                    }
                    7 if !self.described.contains_key(&collection) => {
                        return Err(corrupt(offset, "settings for unknown collection"));
                    }
                    // A record block restates records above every record
                    // its collection holds.
                    OP_RECORD_BLOCK => {
                        let last = self.described[&frame.collection].records.last_key_value();
                        if let (Some((_, first, _)), Some((last, _))) =
                            (writes.records.first(), last)
                            && first <= last
                        {
                            return Err(corrupt(offset, "invalid record block"));
                        }
                    }
                    _ => {}
                }
                if let Some((marker, entries)) = &writes.marker {
                    self.jobs
                        .marker(frame.collection, marker, *entries, offset)?;
                }
                for (collection, record, put) in writes.records {
                    let records = &mut self
                        .described
                        .get_mut(&collection)
                        .expect("the frame check found the collection")
                        .records;
                    match put {
                        Some(expires) => records.insert(record, expires),
                        None => records.remove(&record),
                    };
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// Loads the graph snapshot of a precision image read so far, if any,
    /// as replay loads it (`frame::load_snapshot`): canonical, and attaching
    /// only records that are live without an expiry. Replay keeps the
    /// state; this check drops it.
    fn load_snapshot(&mut self) -> Result<()> {
        let Some((first, bodies)) = self.pending.take() else {
            return Ok(());
        };
        let floor = self.floor.expect("snapshots follow the advance");
        let described = &self.described;
        crate::property_graph::frame::load_snapshot(
            |collection, record| {
                described
                    .get(&collection)
                    .is_some_and(|live| live.records.get(&record) == Some(&false))
            },
            floor,
            bodies,
            first,
        )
        .map(drop)
    }

    /// Notes a collection of a precision log: one its image restates (op 5,
    /// which `prefix` admitted), whose `precision` the catalog then gives,
    /// in ascending ID order; or one created after the image (op 17) under
    /// an ID the log never used. At most `MAX_COLLECTIONS` live.
    pub(crate) fn created(
        &mut self,
        id: u16,
        name: String,
        dimensions: usize,
        precision: Option<Precision>,
        offset: u64,
    ) -> Result<()> {
        if !self.precise {
            return Ok(());
        }
        let reused = match precision {
            None => self
                .described
                .last_key_value()
                .is_some_and(|(last, _)| *last >= id),
            Some(_) => id <= self.collection_high,
        };
        if reused {
            return Err(corrupt(offset, "collection ID reused"));
        }
        if self.described.values().any(|live| live.name == name) {
            return Err(corrupt(offset, "duplicate collection name"));
        }
        if self.described.len() >= MAX_COLLECTIONS {
            return Err(corrupt(offset, "too many live collections"));
        }
        self.described.insert(
            id,
            Live {
                name,
                dimensions,
                precision,
                records: BTreeMap::new(),
            },
        );
        self.collection_high = self.collection_high.max(id);
        Ok(())
    }

    /// Notes a verified graph snapshot body of a precision image: each
    /// graph's snapshot frames are followed by its revision run, which
    /// lists exactly the nodes and edges they put, before the next graph's.
    pub(crate) fn snapshot(&mut self, body: &GraphBody, offset: u64) -> Result<()> {
        if !self.precise {
            return Ok(());
        }
        let graph = body.graph_id.0;
        if self
            .coverage
            .as_ref()
            .is_none_or(|coverage| coverage.graph != graph)
        {
            self.covered(offset)?;
            self.coverage = Some(Coverage {
                graph,
                listed: false,
                snapshot: Default::default(),
                revisions: Default::default(),
            });
        }
        let coverage = self.coverage.as_mut().expect("started above");
        if coverage.listed {
            return Err(corrupt(offset, "graph snapshot after its revisions"));
        }
        for entry in &body.entries {
            match entry {
                GraphEntry::PutNode { node_id, .. } => coverage.snapshot[0].add(node_id.0),
                GraphEntry::PutEdge { edge_id, .. } => coverage.snapshot[1].add(edge_id.0),
                _ => {}
            }
        }
        Ok(())
    }

    /// Ends the coverage of the graph being restated, if any: its revision
    /// run must have listed exactly its snapshot's nodes and edges.
    fn covered(&mut self, offset: u64) -> Result<()> {
        match self.coverage.take() {
            Some(coverage) if !coverage.listed => {
                Err(corrupt(offset, "graph snapshot without its revisions"))
            }
            Some(coverage)
                if !(0..2).all(|at| coverage.snapshot[at].matches(&coverage.revisions[at])) =>
            {
                Err(corrupt(offset, "graph revisions do not match the snapshot"))
            }
            _ => Ok(()),
        }
    }

    /// Checks the verified (and decrypted) body of an op-16 frame whose
    /// prefix `prefix` accepted. A block of a precision image's own kinds
    /// returns what it restates.
    pub(crate) fn block(
        &mut self,
        collection: u16,
        id: u64,
        count: u32,
        body: &[u8],
        offset: u64,
    ) -> Result<Option<Supplement>> {
        let block = Envelope::parse(collection, id, body, offset)?;
        let kind = id as u16;
        match kind {
            KIND_TABLE => return self.table(count, &block, offset).map(|()| None),
            KIND_END => {
                return self
                    .end_block(count, &block, body.len(), offset)
                    .map(|()| None);
            }
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
        if self.precise && matches!(kind, KIND_CATALOG | KIND_IMPORTS | KIND_REVISIONS) {
            return self
                .supplement(kind, collection, id >> 16, count, &block, offset)
                .map(Some);
        }
        if matches!(kind >> 8, 0x01 | 0x02) && !self.collections.contains(&collection) {
            return Err(corrupt(offset, "compact block for unknown collection"));
        }
        // The table admits only derived kinds while no codec exists, so
        // this authentic, declared block is skipped, and reported.
        self.skips.add(collection, kind, block.version, first);
        Ok(None)
    }

    /// Decodes a block of a precision image's own kinds. The catalog lists
    /// exactly the collections restated before the table, with their
    /// dimensions; the registry binds each job to a restated collection by
    /// name, or a dropped job to a collection ID used before and not live;
    /// revision runs are canonical across their blocks and cover their
    /// graph's snapshot.
    fn supplement(
        &mut self,
        kind: u16,
        owner: u16,
        partition: u64,
        count: u32,
        block: &Envelope<'_>,
        offset: u64,
    ) -> Result<Supplement> {
        let single = || match block.sections[..] {
            [section] => Ok(section),
            _ => Err(corrupt(offset, "invalid precision image block")),
        };
        match kind {
            KIND_CATALOG => {
                let entries = catalog::decode_catalog(single()?, count, offset)?;
                if !entries
                    .iter()
                    .map(|entry| entry.collection)
                    .eq(self.collections.iter().copied())
                {
                    return Err(corrupt(
                        offset,
                        "precision catalog does not list the collections",
                    ));
                }
                if entries.iter().any(|entry| {
                    self.described
                        .get(&entry.collection)
                        .map(|live| live.dimensions)
                        != Some(entry.dimensions)
                }) {
                    return Err(corrupt(offset, "precision catalog dimensions mismatch"));
                }
                for entry in &entries {
                    let live = self.described.get_mut(&entry.collection);
                    live.expect("listed above").precision = Some(entry.precision);
                }
                Ok(Supplement::Catalog(entries))
            }
            KIND_IMPORTS => {
                let registry = crate::import::Registry::decode_image(single()?, count)
                    .map_err(|reason| corrupt(offset, reason))?;
                for (job, state) in registry.jobs() {
                    let collection = state.collection;
                    let live = self.described.get(&collection);
                    let bound = if state.collection_dropped {
                        collection != 0 && collection <= self.collection_high && live.is_none()
                    } else {
                        live.is_some_and(|live| live.name == registry.collection_name(job))
                    };
                    if !bound {
                        return Err(corrupt(offset, "import job bound to an unknown collection"));
                    }
                }
                Ok(Supplement::Imports(Box::new(registry)))
            }
            _ => {
                let floor = self.floor.expect("image blocks follow the advance");
                let (nodes, edges) =
                    catalog::decode_revisions(&block.sections, count, floor, offset)?;
                let coverage = match self.coverage.as_mut() {
                    Some(coverage)
                        if coverage.graph == owner && coverage.listed == (partition != 0) =>
                    {
                        coverage
                    }
                    _ => return Err(corrupt(offset, "graph revisions without their snapshot")),
                };
                coverage.listed = true;
                for &(id, _) in &nodes {
                    coverage.revisions[0].add(id);
                }
                for &(id, _) in &edges {
                    coverage.revisions[1].add(id);
                }
                self.revision_run(owner, partition, &nodes, &edges, offset)?;
                Ok(Supplement::Revisions {
                    graph: owner,
                    partition,
                    nodes,
                    edges,
                })
            }
        }
    }

    /// Continues the revision run of `graph`: across its blocks, in
    /// partition order, nodes and edges each continue in ascending ID order
    /// (Decision P). A block holds at most `MAX_REVISIONS` entries but need
    /// not be full; only an empty graph has an empty block, its only one.
    fn revision_run(
        &mut self,
        graph: u16,
        partition: u64,
        nodes: &[(u64, u64)],
        edges: &[(u64, u64)],
        offset: u64,
    ) -> Result<()> {
        let invalid = || corrupt(offset, "non-canonical graph revisions");
        let entries = nodes.len() + edges.len();
        let run = if partition == 0 {
            self.revisions.insert(RevisionRun {
                graph,
                node: None,
                edge: None,
                empty: entries == 0,
            })
        } else {
            match self.revisions.as_mut() {
                Some(run) if run.graph == graph && !run.empty && entries != 0 => run,
                _ => return Err(invalid()),
            }
        };
        let after = |last: Option<u64>, pairs: &[(u64, u64)]| {
            pairs
                .first()
                .is_none_or(|&(id, _)| last.is_none_or(|last| id > last))
        };
        if !after(run.node, nodes) || !after(run.edge, edges) {
            return Err(invalid());
        }
        run.node = nodes.last().map_or(run.node, |&(id, _)| Some(id));
        run.edge = edges.last().map_or(run.edge, |&(id, _)| Some(id));
        Ok(())
    }

    /// Moves a precision image to the part that a frame of `operation` (and
    /// block `kind` and `partition`) belongs to; frames out of that order
    /// are corruption.
    fn next_stage(&mut self, operation: u8, kind: u16, partition: u64, offset: u64) -> Result<()> {
        use Stage::{Catalog, Derived, Imports, Records, Revisions, Snapshot, Table, Vectors};
        let stage = self.stage;
        self.stage = match (operation, kind) {
            (OP_COMPACT_BLOCK, KIND_CATALOG) if stage == Table => Catalog,
            (OP_COMPACT_BLOCK, KIND_IMPORTS) if stage == Catalog => Imports,
            (OP_RECORD_BLOCK, _) if matches!(stage, Imports | Records) => Records,
            (10, _) if matches!(stage, Imports | Records | Vectors) => Vectors,
            (OP_GRAPH_SNAPSHOT, _)
                if matches!(stage, Imports | Records | Vectors | Snapshot | Revisions) =>
            {
                Snapshot
            }
            (OP_COMPACT_BLOCK, KIND_REVISIONS)
                if (partition == 0 && stage == Snapshot)
                    || (partition != 0 && stage == Revisions) =>
            {
                Revisions
            }
            (OP_COMPACT_BLOCK, KIND_CATALOG | KIND_IMPORTS | KIND_REVISIONS) => {
                return Err(corrupt(offset, "precision image frame out of order"));
            }
            (OP_COMPACT_BLOCK, _)
                if matches!(stage, Imports | Records | Vectors | Revisions | Derived) =>
            {
                Derived
            }
            _ => return Err(corrupt(offset, "precision image frame out of order")),
        };
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
        // A precision image always has its catalog and import registry.
        let mut storage = [false; 2];
        for entry in table {
            let (kind, version, flags) = (u16_at(entry, 0), u16_at(entry, 2), u16_at(entry, 4));
            let class = registered(kind);
            let own = self.precise && matches!(kind, KIND_CATALOG | KIND_IMPORTS | KIND_REVISIONS);
            // The table lists neither itself nor the end block.
            if kind <= KIND_END
                || version == 0
                || flags & !CRITICAL != 0
                || u16_at(entry, 6) != 0
                || class.is_some_and(|critical| critical != (flags == CRITICAL))
                || (own && flags != CRITICAL)
            {
                return Err(corrupt(offset, "invalid capability table entry"));
            }
            match kind {
                KIND_CATALOG => storage[0] = true,
                KIND_IMPORTS => storage[1] = true,
                _ => {}
            }
            if previous.is_some_and(|previous| previous >= (kind, version)) {
                return Err(corrupt(
                    offset,
                    "unsorted or duplicate capability table entries",
                ));
            }
            previous = Some((kind, version));
            // Besides a precision image's own kinds, no codec exists yet:
            // only declared derived kinds can be skipped.
            readable &= class == Some(false) || (own && version == 1);
            entries.insert((kind, version), (flags, None));
        }
        if self.precise && storage != [true; 2] {
            return Err(corrupt(
                offset,
                "precision image without its catalog and import registry",
            ));
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
        self.covered(offset)?;
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
    pub(super) const FLOOR: u64 = 4;
    const KIND: u16 = 0x0200;
    const VERSIONS: u16 = 17;

    /// A plaintext frame at the checkpoint `FLOOR`.
    pub(super) fn frame(owner: u16, id: u64, count: u32, body: &[u8]) -> Vec<u8> {
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
    pub(super) fn block(
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
        let mut check = ImageCheck::new(
            header.version,
            header.flags,
            header.dimensions,
            header.collection_floor,
        )
        .unwrap();
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
