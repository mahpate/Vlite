use crate::crypto::{Cipher, DESCRIPTOR_SIZE, Descriptor, EncryptionKey, SEAL_OVERHEAD};
use crate::{
    CollectionSettings, DistanceMetric, Error, HnswConfig, MAX_BATCH_BYTES, MAX_BATCH_OPERATIONS,
    MAX_DIMENSIONS, MAX_PAYLOAD_BYTES, Mutation, Record, Result,
    metadata_index::{MAX_METADATA_INDEXES, MetadataIndexKind, MetadataIndexSpec},
    planner::PlannerPolicy,
    quantize::Quantization,
    text_index::MAX_TEXT_FIELDS,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Seek, SeekFrom};

pub(crate) const HEADER_SIZE: u64 = 32;
/// Header flag (v5 bytes 26..28): frame bodies are sealed; a 64-byte
/// encryption descriptor follows the header.
pub(crate) const FLAG_ENCRYPTED: u16 = 1;
/// Header flag: every commit is preceded by a time-anchor frame (op 11).
pub(crate) const FLAG_TIMESTAMPS: u16 = 2;
pub(crate) const KNOWN_FLAGS: u16 = FLAG_ENCRYPTED | FLAG_TIMESTAMPS;
/// Header flag (v5, DO-001): the file stores named property graphs in op-12
/// frames. Every reader of a graph-aware build (all builds since v0.5.1) accepts it
/// (paths that cannot serve graph files, such as change feeds and `upgrade`,
/// reject them explicitly); older builds reject it with
/// `UnsupportedFeature(FLAG_GRAPH)`.
pub(crate) const FLAG_GRAPH: u16 = crate::property_graph::engine::codec::FLAG_GRAPH;
/// Graph frame operation (DO-003), valid only in files carrying `FLAG_GRAPH`.
const OP_GRAPH: u8 = crate::property_graph::engine::codec::OP_GRAPH;
/// Header flag (v5, Speed P2): the log may start with an advance frame (op
/// 13) that restates history up to a checkpoint sequence. Written by
/// directory-store checkpoints and kept by `pack`; never combined with
/// `FLAG_TIMESTAMPS`.
pub(crate) const FLAG_HISTORY: u16 = 8;
/// Header flags this build accepts in addition to `KNOWN_FLAGS`.
const GRAPH_READER_FLAGS: u16 = FLAG_GRAPH | FLAG_HISTORY;
pub(crate) const OP_TIME_ANCHOR: u8 = 11;
/// Advance frame (Speed P2): valid only as the first frame of a
/// `FLAG_HISTORY` log. Its sequence `S` is the checkpoint the following
/// restated frames describe; they carry their original sequences (record
/// revisions), non-decreasing and at most `S`, until the first frame at `S + 1`.
pub(crate) const OP_ADVANCE: u8 = 13;
/// Record block (Speed P2b): live records of one collection restated by a
/// checkpoint image, IDs strictly ascending, each with its revision and
/// expiry. Valid only in a `FLAG_HISTORY` log at the checkpoint sequence and
/// before any graph frame; it replaces one op-9 frame per record.
pub(crate) const OP_RECORD_BLOCK: u8 = 14;
/// Graph snapshot (Speed P2b): the canonical graph bodies of a checkpoint
/// image (`property_graph::frame::compacted_bodies`), loaded directly into
/// graph state instead of being replayed entry by entry. Valid only in a
/// `FLAG_HISTORY | FLAG_GRAPH` log at the checkpoint sequence, into an empty
/// graph state.
pub(crate) const OP_GRAPH_SNAPSHOT: u8 = 15;
/// Per-entry header of a record block: ID, revision, expiry flag, expiry and
/// extended-body length.
const BLOCK_ENTRY_HEADER: usize = 29;
/// Writers close a record block once its body reaches this many bytes.
const RECORD_BLOCK_TARGET: usize = 1 << 20;
/// Maximum entries of one record block.
const MAX_BLOCK_RECORDS: usize = 4096;
/// Largest valid record-block body: the last entry may start just below the
/// target.
const MAX_RECORD_BLOCK: usize = RECORD_BLOCK_TARGET + BLOCK_ENTRY_HEADER + MAX_EXTENDED_BODY;
pub(crate) const PREFIX_SIZE: usize = 36;
pub(crate) const FOOTER_SIZE: usize = 16;
pub(crate) const MAGIC: &[u8; 8] = b"VLITEDB\0";
/// Version written by `Database::create` and by compaction of v1–v3 sources.
pub(crate) const CURRENT_VERSION: u32 = 3;
/// Opt-in format with lineage, collections and import checkpoints.
pub(crate) const V4: u32 = 4;
/// Opt-in v5 record expiry and geospatial metadata format.
pub(crate) const V5: u32 = 5;

/// Maximum encoded settings blob (op 5 tail and op 7 body).
pub(crate) const MAX_SETTINGS_BLOB: usize = 16_384;
/// Maximum live collections, including the default collection.
pub(crate) const MAX_COLLECTIONS: usize = 64;
/// Exact body length of a batch entry op 8 chunk or close checkpoint.
pub(crate) const IMPORT_MARKER_LEN: usize = 52;
/// Exact body length of a batch entry op 8 snapshot checkpoint: the 52-byte
/// layout (row counts hold the low 32 bits) followed by the high 32 bits of
/// `rows_committed` and `rows_skipped`, so cumulative u64 counts survive
/// compaction exactly.
pub(crate) const IMPORT_SNAPSHOT_LEN: usize = 60;
const IMPORT_SNAPSHOT_ENTRY: usize = 16 + IMPORT_SNAPSHOT_LEN;
pub(crate) const MARKER_FINISHED: u32 = 1;
pub(crate) const MARKER_ABORTED: u32 = 2;
/// Deviation from the PRD layout: compaction/upgrade carry an open job forward
/// with a marker-only snapshot whose chunk need not start at 1. Snapshot
/// markers (and only they) use the 60-byte layout.
pub(crate) const MARKER_SNAPSHOT: u32 = 4;
const MAX_COLLECTION_NAME: usize = 63;
const MAX_CREATE_BODY: usize = 1 + MAX_COLLECTION_NAME + 4 + MAX_SETTINGS_BLOB;

pub(crate) const MAX_EXTENDED_BODY: usize =
    MAX_DIMENSIONS * 4 + 4 + MAX_PAYLOAD_BYTES + crate::MAX_METADATA_BYTES;

pub(crate) fn extended_body(vector: &[f32], payload: &[u8], metadata: &crate::Metadata) -> Vec<u8> {
    let mut body = Vec::new();
    push_extended(&mut body, vector, payload, metadata);
    body
}

fn push_extended(body: &mut Vec<u8>, vector: &[f32], payload: &[u8], metadata: &crate::Metadata) {
    for value in vector {
        body.extend_from_slice(&value.to_le_bytes());
    }
    body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    body.extend_from_slice(payload);
    body.extend_from_slice(&crate::metadata::encode(metadata));
}

/// Builds op-14 record blocks of one collection. Callers push records in
/// ascending ID order and emit the block when it is full.
pub(crate) struct RecordBlock {
    collection: u16,
    count: u32,
    body: Vec<u8>,
}

impl RecordBlock {
    pub(crate) fn new(collection: u16) -> Self {
        Self {
            collection,
            count: 0,
            body: Vec::new(),
        }
    }

    /// Appends one entry: ID, revision, expiry marker (flag byte and value,
    /// 0 without expiry), extended-body length and the extended body.
    pub(crate) fn push(&mut self, id: u64, revision: u64, expiry: Option<u64>, record: &Record) {
        self.body.extend_from_slice(&id.to_le_bytes());
        self.body.extend_from_slice(&revision.to_le_bytes());
        self.body.push(u8::from(expiry.is_some()));
        self.body
            .extend_from_slice(&expiry.unwrap_or(0).to_le_bytes());
        let length_at = self.body.len();
        self.body.extend_from_slice(&[0; 4]);
        push_extended(
            &mut self.body,
            &record.vector,
            &record.payload,
            &record.metadata,
        );
        let length = (self.body.len() - length_at - 4) as u32;
        self.body[length_at..length_at + 4].copy_from_slice(&length.to_le_bytes());
        self.count += 1;
    }

    pub(crate) fn is_full(&self) -> bool {
        self.count as usize >= MAX_BLOCK_RECORDS || self.body.len() >= RECORD_BLOCK_TARGET
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The op-14 frame at `sequence` holding the pushed entries; the block
    /// is empty again afterwards.
    pub(crate) fn take_frame(&mut self, sequence: u64) -> Vec<u8> {
        let frame = wrap(
            OP_RECORD_BLOCK,
            self.collection,
            sequence,
            0,
            self.count,
            self.body.len() as u32,
            &self.body,
        );
        self.count = 0;
        self.body.clear();
        frame
    }
}

/// Body bytes the record occupies in a single-record frame (used for
/// reclaimable-space estimates; excludes the 52-byte prefix+footer).
pub(crate) fn record_body_len(record: &Record) -> u64 {
    let vector = record.vector.len() as u64 * 4;
    if record.metadata.is_empty() {
        vector + record.payload.len() as u64
    } else {
        let metadata = crate::metadata::validate(&record.metadata).unwrap_or(0) as u64;
        vector + 4 + record.payload.len() as u64 + metadata
    }
}

pub(crate) fn decode_extended(
    body: &[u8],
    dimensions: usize,
    offset: u64,
    allow_geo: bool,
) -> Result<Record> {
    let vector_end = dimensions * 4;
    if body.len() < vector_end + 8 {
        return Err(corrupt(offset, "truncated metadata record"));
    }
    let vector = decode_vector(&body[..vector_end], offset)?;
    let payload_len = u32_at(body, vector_end) as usize;
    if payload_len > MAX_PAYLOAD_BYTES {
        return Err(corrupt(offset, "oversized metadata record payload"));
    }
    let payload_end = vector_end + 4 + payload_len;
    let payload = body
        .get(vector_end + 4..payload_end)
        .ok_or_else(|| corrupt(offset, "truncated metadata payload"))?
        .to_vec();
    let metadata = crate::metadata::decode(
        body.get(payload_end..)
            .ok_or_else(|| corrupt(offset, "missing metadata"))?,
    )
    .map_err(|_| corrupt(offset, "invalid typed metadata encoding"))?;
    if !allow_geo && crate::metadata::contains_geo(&metadata) {
        return Err(corrupt(offset, "geo metadata requires format v5"));
    }
    Ok(Record {
        vector,
        payload,
        metadata,
    })
}

/// One v4 import checkpoint (batch entry op 8). Chunk and close markers use
/// the 52-byte body and carry per-chunk counts (at most 4,096, so they fit
/// the u32 fields); snapshot markers use the 60-byte body and carry the full
/// cumulative u64 counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ImportMarker {
    pub job: u64,
    pub chunk: u64,
    pub rows_committed: u64,
    pub rows_skipped: u64,
    pub flags: u32,
    pub digest: [u8; 32],
}

impl ImportMarker {
    fn is_snapshot(&self) -> bool {
        self.flags & MARKER_SNAPSHOT != 0
    }

    /// Body length of this marker's layout.
    fn len(&self) -> usize {
        if self.is_snapshot() {
            IMPORT_SNAPSHOT_LEN
        } else {
            IMPORT_MARKER_LEN
        }
    }

    fn encode(&self) -> Vec<u8> {
        let split = |count: u64| (count as u32, (count >> 32) as u32);
        let (committed_low, committed_high) = split(self.rows_committed);
        let (skipped_low, skipped_high) = split(self.rows_skipped);
        // Never truncate silently: only snapshots may carry high bits.
        assert!(
            self.is_snapshot() || (committed_high == 0 && skipped_high == 0),
            "chunk/close import marker counts exceed u32"
        );
        let mut bytes = Vec::with_capacity(self.len());
        bytes.extend_from_slice(&self.chunk.to_le_bytes());
        bytes.extend_from_slice(&committed_low.to_le_bytes());
        bytes.extend_from_slice(&skipped_low.to_le_bytes());
        bytes.extend_from_slice(&self.flags.to_le_bytes());
        bytes.extend_from_slice(&self.digest);
        if self.is_snapshot() {
            bytes.extend_from_slice(&committed_high.to_le_bytes());
            bytes.extend_from_slice(&skipped_high.to_le_bytes());
        }
        bytes
    }

    /// Decodes a 52- or 60-byte body; `decode_batch` checks that the length
    /// matches the snapshot flag.
    fn decode(job: u64, body: &[u8]) -> Self {
        let high = |offset: usize| {
            body.get(offset..offset + 4)
                .map_or(0, |bytes| u64::from(u32_at(bytes, 0)) << 32)
        };
        Self {
            job,
            chunk: u64_at(body, 0),
            rows_committed: u64::from(u32_at(body, 8)) | high(52),
            rows_skipped: u64::from(u32_at(body, 12)) | high(56),
            flags: u32_at(body, 16),
            digest: body[20..52].try_into().unwrap(),
        }
    }
}

/// Import-relevant log event in log order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LoadedImportMarker {
    /// A checkpoint with the collection (ID and name at that point in the
    /// log) of its batch frame.
    Marker {
        collection: u16,
        name: String,
        marker: ImportMarker,
    },
    /// A collection drop. Its ID is never reused, but jobs bound to the
    /// dropped collection keep only their history.
    CollectionDropped(u16),
}

pub(crate) struct LoadedCollection {
    pub vectors: crate::vectors::VectorData,
    pub name: String,
    pub dimensions: usize,
    pub settings: CollectionSettings,
    /// True once a create (op 5) or settings (op 7) frame stored the settings.
    pub settings_persisted: bool,
    pub records: crate::records::RecordMap,
    pub expiries: BTreeMap<u64, u64>,
    /// Sequence of the frame that last put each live record.
    pub revisions: BTreeMap<u64, u64>,
    pub live_body_bytes: u64,
}

impl LoadedCollection {
    pub(crate) fn new(name: String, dimensions: usize, settings: CollectionSettings) -> Self {
        Self {
            vectors: crate::vectors::VectorData::default(),
            name,
            dimensions,
            settings,
            settings_persisted: false,
            records: crate::records::RecordMap::new(),
            expiries: BTreeMap::new(),
            revisions: BTreeMap::new(),
            live_body_bytes: 0,
        }
    }

    pub(crate) fn put(
        &mut self,
        id: u64,
        record: Record,
        sequence: u64,
        expires_at_ms: Option<u64>,
    ) {
        self.vectors.records.remove(&id);
        self.live_body_bytes += record_body_len(&record);
        if let Some(previous) = self.records.insert(id, record) {
            self.live_body_bytes -= record_body_len(&previous);
        }
        self.revisions.insert(id, sequence);
        if let Some(expiry) = expires_at_ms {
            self.expiries.insert(id, expiry);
        } else {
            self.expiries.remove(&id);
        }
    }

    pub(crate) fn delete(&mut self, id: u64) {
        self.vectors.records.remove(&id);
        if let Some(previous) = self.records.remove(&id) {
            self.live_body_bytes -= record_body_len(&previous);
        }
        self.revisions.remove(&id);
        self.expiries.remove(&id);
    }

    fn update_metadata(
        &mut self,
        id: u64,
        metadata: crate::Metadata,
        sequence: u64,
        offset: u64,
    ) -> Result<()> {
        let Some(record) = self.records.get_mut(&id) else {
            return Err(corrupt(offset, "metadata update references missing record"));
        };
        self.vectors.invalidate_staged(id);
        self.live_body_bytes -= record_body_len(record);
        record.metadata = metadata;
        self.live_body_bytes += record_body_len(record);
        self.revisions.insert(id, sequence);
        Ok(())
    }
}

pub(crate) struct Loaded {
    pub version: u32,
    /// Default-collection (ID 0) dimensions from the header.
    pub dimensions: usize,
    /// Nonzero v4 header lineage; None for v1–v3.
    pub lineage: Option<u64>,
    /// Header collection-ID floor (v4 bytes 24..26; 0 for v1–v3).
    pub collection_floor: u16,
    /// Highest collection ID ever assigned in this lineage history: the
    /// maximum of `collection_floor` and every create (op 5) in the log.
    /// Collection IDs are never reused, so the next one is this plus one.
    pub collection_high: u16,
    /// Live collections by ID; ID 0 (`default`) is always present.
    pub collections: BTreeMap<u16, LoadedCollection>,
    /// Import markers and collection drops in log order.
    pub imports: Vec<LoadedImportMarker>,
    pub sequence: u64,
    pub end: u64,
    pub incomplete_bytes: u64,
    pub flags: u16,
    /// Present for encrypted files.
    pub cipher: Option<Cipher>,
    /// Per-commit wall-clock milliseconds `(sequence, timestamp)`, ascending,
    /// for files created with commit timestamps.
    pub commit_times: Vec<(u64, u64)>,
    /// Replayed named graphs; `Some` exactly when the header has `FLAG_GRAPH`.
    pub graph: Option<crate::property_graph::engine::GraphState>,
    /// Checkpoint sequence of a leading advance frame (0 when the log holds
    /// its full history): sequences at or below it were restated, so change
    /// feeds and log segments cannot start before it.
    pub history_floor: u64,
}

impl Loaded {
    /// Live records across all collections.
    pub(crate) fn live_records(&self) -> usize {
        self.collections
            .values()
            .map(|collection| collection.records.len())
            .sum()
    }

    /// Records of the default collection.
    pub(crate) fn default_records(&self) -> &crate::records::RecordMap {
        &self.collections[&0].records
    }
}

/// Streaming IEEE CRC-32: `crc32_update(crc32_update(0, a), b) == crc32(a ++ b)`.
pub(crate) fn crc32_update(previous: u32, bytes: &[u8]) -> u32 {
    let mut crc = !previous;
    let (chunks, tail) = bytes.as_chunks::<8>();
    for chunk in chunks {
        let low = u32::from_le_bytes(chunk[..4].try_into().unwrap()) ^ crc;
        crc = CRC32_TABLE[7][low as u8 as usize]
            ^ CRC32_TABLE[6][(low >> 8) as u8 as usize]
            ^ CRC32_TABLE[5][(low >> 16) as u8 as usize]
            ^ CRC32_TABLE[4][(low >> 24) as usize]
            ^ CRC32_TABLE[3][chunk[4] as usize]
            ^ CRC32_TABLE[2][chunk[5] as usize]
            ^ CRC32_TABLE[1][chunk[6] as usize]
            ^ CRC32_TABLE[0][chunk[7] as usize];
    }
    for &byte in tail {
        crc = (crc >> 8) ^ CRC32_TABLE[0][((crc as u8) ^ byte) as usize];
    }
    !crc
}

const CRC32_TABLE: [[u32; 256]; 8] = {
    let mut table = [[0; 256]; 8];
    let mut byte = 0;
    while byte < 256 {
        let mut crc = byte as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
            bit += 1;
        }
        table[0][byte] = crc;
        byte += 1;
    }
    let mut slice = 1;
    while slice < 8 {
        byte = 0;
        while byte < 256 {
            let crc = table[slice - 1][byte];
            table[slice][byte] = (crc >> 8) ^ table[0][(crc & 255) as usize];
            byte += 1;
        }
        slice += 1;
    }
    table
};

// IEEE CRC-32, reflected polynomial. Checksums detect accidents, not tampering.
pub(crate) fn crc32(bytes: &[u8]) -> u32 {
    crc32_update(0, bytes)
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

pub(crate) fn corrupt(offset: u64, reason: &'static str) -> Error {
    Error::Corrupt { offset, reason }
}

/// Returns a random nonzero lineage from operating-system randomness.
pub(crate) fn new_lineage() -> Result<u64> {
    loop {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).map_err(|error| {
            Error::Io(std::io::Error::other(format!(
                "operating-system randomness unavailable: {error}"
            )))
        })?;
        let lineage = u64::from_le_bytes(bytes);
        if lineage != 0 {
            return Ok(lineage);
        }
    }
}

/// `^[a-z][a-z0-9_-]{0,62}$`.
pub(crate) fn valid_collection_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=MAX_COLLECTION_NAME).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes[1..]
            .iter()
            .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

pub(crate) fn header(dimensions: usize) -> Vec<u8> {
    header_with(CURRENT_VERSION, dimensions, 0, 0)
}

/// v4 header; `lineage` MUST be nonzero. `collection_floor` (bytes 24..26) is
/// the highest collection ID ever assigned by the history this file was
/// rewritten from (0 for a new file); IDs at or below it are never assigned
/// again (see `load`).
pub(crate) fn header_v4(dimensions: usize, lineage: u64, collection_floor: u16) -> Vec<u8> {
    header_with(V4, dimensions, lineage, collection_floor)
}

/// v5 header with feature flags and, for encrypted files, the descriptor.
pub(crate) fn header_v5_flags(
    dimensions: usize,
    lineage: u64,
    collection_floor: u16,
    flags: u16,
    cipher: Option<&Cipher>,
) -> Vec<u8> {
    let mut bytes = header_with(V5, dimensions, lineage, collection_floor);
    bytes[26..28].copy_from_slice(&flags.to_le_bytes());
    let checksum = crc32(&bytes[..28]);
    bytes[28..32].copy_from_slice(&checksum.to_le_bytes());
    if let Some(cipher) = cipher {
        bytes.extend_from_slice(&cipher.descriptor.encode());
    }
    bytes
}

/// A time-anchor frame: written immediately before the commit frame that has
/// `next_sequence`; it consumes no sequence number.
pub(crate) fn time_anchor_frame(next_sequence: u64, timestamp_ms: u64) -> Vec<u8> {
    wrap(
        OP_TIME_ANCHOR,
        0,
        next_sequence,
        0,
        0,
        8,
        &timestamp_ms.to_le_bytes(),
    )
}

fn header_with(version: u32, dimensions: usize, lineage: u64, collection_floor: u16) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER_SIZE as usize);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&version.to_le_bytes());
    bytes.extend_from_slice(&(dimensions as u32).to_le_bytes());
    bytes.extend_from_slice(&lineage.to_le_bytes());
    bytes.extend_from_slice(&collection_floor.to_le_bytes());
    bytes.extend_from_slice(&[0; 2]);
    bytes.extend_from_slice(&crc32(&bytes).to_le_bytes());
    bytes
}

/// Wraps a body with prefix and footer. `collection` occupies bytes 5..7.
fn wrap(
    operation: u8,
    collection: u16,
    sequence: u64,
    id: u64,
    count: u32,
    payload_len: u32,
    body: &[u8],
) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(PREFIX_SIZE + body.len() + FOOTER_SIZE);
    bytes.extend_from_slice(b"VREC");
    bytes.push(operation);
    bytes.extend_from_slice(&collection.to_le_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&sequence.to_le_bytes());
    bytes.extend_from_slice(&id.to_le_bytes());
    bytes.extend_from_slice(&count.to_le_bytes());
    bytes.extend_from_slice(&payload_len.to_le_bytes());
    bytes.extend_from_slice(&crc32(&bytes).to_le_bytes());
    bytes.extend_from_slice(body);
    let checksum = crc32(&bytes);
    bytes.extend_from_slice(b"VCMT");
    bytes.extend_from_slice(&checksum.to_le_bytes());
    bytes.extend_from_slice(&sequence.to_le_bytes());
    bytes
}

/// Advance frame (op 13) declaring checkpoint sequence `sequence`; it has an
/// empty body and zero collection, ID and count fields.
pub(crate) fn advance_frame(sequence: u64) -> Vec<u8> {
    wrap(OP_ADVANCE, 0, sequence, 0, 0, 0, &[])
}

/// Graph frame (op 12, DO-003) carrying one encoded graph body of `count`
/// entries. Collection, reserved and ID prefix fields are zero.
pub(crate) fn graph_frame(sequence: u64, count: u32, body: &[u8]) -> Vec<u8> {
    wrap(OP_GRAPH, 0, sequence, 0, count, body.len() as u32, body)
}

/// Graph snapshot frame (op 15): one canonical checkpoint graph body of
/// `count` entries, laid out like an op-12 frame.
pub(crate) fn graph_snapshot_frame(sequence: u64, count: u32, body: &[u8]) -> Vec<u8> {
    wrap(
        OP_GRAPH_SNAPSHOT,
        0,
        sequence,
        0,
        count,
        body.len() as u32,
        body,
    )
}

/// Single put/delete frame for the default collection (byte-identical to v1–v3).
pub(crate) fn frame(sequence: u64, id: u64, record: Option<&Record>) -> Vec<u8> {
    record_frame(sequence, 0, id, record)
}

/// Single put (op 1/4) or delete (op 2) frame tagged with a collection ID.
/// Collection 0 produces exactly the legacy encoding.
pub(crate) fn record_frame(
    sequence: u64,
    collection: u16,
    id: u64,
    record: Option<&Record>,
) -> Vec<u8> {
    match record {
        None => wrap(2, collection, sequence, id, 0, 0, &[]),
        Some(record) if !record.metadata.is_empty() => {
            let body = extended_body(&record.vector, &record.payload, &record.metadata);
            wrap(
                4,
                collection,
                sequence,
                id,
                record.vector.len() as u32,
                body.len() as u32,
                &body,
            )
        }
        Some(record) => {
            let mut body = Vec::with_capacity(record.vector.len() * 4 + record.payload.len());
            for value in &record.vector {
                body.extend_from_slice(&value.to_le_bytes());
            }
            body.extend_from_slice(&record.payload);
            wrap(
                1,
                collection,
                sequence,
                id,
                record.vector.len() as u32,
                record.payload.len() as u32,
                &body,
            )
        }
    }
}

/// V5 full-record frame with a nullable absolute Unix-millisecond expiry.
pub(crate) fn record_frame_v5(
    sequence: u64,
    collection: u16,
    id: u64,
    record: &Record,
    expiry: Option<u64>,
) -> Vec<u8> {
    let mut body = Vec::new();
    body.push(u8::from(expiry.is_some()));
    body.extend_from_slice(&expiry.unwrap_or(0).to_le_bytes());
    body.extend_from_slice(&extended_body(
        &record.vector,
        &record.payload,
        &record.metadata,
    ));
    wrap(
        9,
        collection,
        sequence,
        id,
        record.vector.len() as u32,
        body.len() as u32,
        &body,
    )
}

/// V5 metadata-only frame. The record's vector, payload and expiry are retained.
pub(crate) fn metadata_frame_v5(
    sequence: u64,
    collection: u16,
    id: u64,
    metadata: &crate::Metadata,
) -> Vec<u8> {
    let body = crate::metadata::encode(metadata);
    wrap(8, collection, sequence, id, 0, body.len() as u32, &body)
}

/// Canonical batch-entry encoding shared by batch frames and import digests.
pub(crate) fn encode_batch_entries(operations: &[Mutation], bytes: &mut Vec<u8>) {
    for operation in operations {
        match operation {
            Mutation::PutWithMetadata {
                id,
                vector,
                payload,
                metadata,
            } => {
                let body = extended_body(vector, payload, metadata);
                bytes.extend_from_slice(&[4, 0, 0, 0]);
                bytes.extend_from_slice(&id.to_le_bytes());
                bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
                bytes.extend_from_slice(&body);
            }
            Mutation::Put {
                id,
                vector,
                payload,
            } => {
                bytes.extend_from_slice(&[1, 0, 0, 0]);
                bytes.extend_from_slice(&id.to_le_bytes());
                bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                for value in vector {
                    bytes.extend_from_slice(&value.to_le_bytes());
                }
                bytes.extend_from_slice(payload);
            }
            Mutation::Delete { id } => {
                bytes.extend_from_slice(&[2, 0, 0, 0]);
                bytes.extend_from_slice(&id.to_le_bytes());
                bytes.extend_from_slice(&0u32.to_le_bytes());
            }
        }
    }
}

pub(crate) fn batch_frame(sequence: u64, operations: &[Mutation], body_bytes: usize) -> Vec<u8> {
    let mut body = Vec::with_capacity(body_bytes);
    encode_batch_entries(operations, &mut body);
    debug_assert_eq!(body.len(), body_bytes);
    wrap(
        3,
        0,
        sequence,
        0,
        operations.len() as u32,
        body.len() as u32,
        &body,
    )
}

/// v4 batch frame for one collection, optionally ending with an import marker.
pub(crate) fn batch_frame_v4(
    sequence: u64,
    collection: u16,
    operations: &[Mutation],
    marker: Option<&ImportMarker>,
) -> Vec<u8> {
    let mut body = Vec::new();
    encode_batch_entries(operations, &mut body);
    if let Some(marker) = marker {
        body.extend_from_slice(&[8, 0, 0, 0]);
        body.extend_from_slice(&marker.job.to_le_bytes());
        body.extend_from_slice(&(marker.len() as u32).to_le_bytes());
        body.extend_from_slice(&marker.encode());
    }
    let count = operations.len() + usize::from(marker.is_some());
    wrap(
        3,
        collection,
        sequence,
        0,
        count as u32,
        body.len() as u32,
        &body,
    )
}

/// Full SHA-256 over the canonical batch-entry encoding of `operations`
/// followed by one policy byte (REQ-009 exactly-once chunk identity).
pub(crate) fn import_digest(operations: &[Mutation], policy: u8) -> [u8; 32] {
    let mut bytes = Vec::new();
    encode_batch_entries(operations, &mut bytes);
    bytes.push(policy);
    Sha256::digest(&bytes).into()
}

pub(crate) fn collection_create_frame(
    sequence: u64,
    id: u16,
    name: &str,
    dimensions: usize,
    settings: &CollectionSettings,
) -> Vec<u8> {
    let mut body = Vec::new();
    body.push(name.len() as u8);
    body.extend_from_slice(name.as_bytes());
    body.extend_from_slice(&(dimensions as u32).to_le_bytes());
    body.extend_from_slice(&encode_settings(settings));
    wrap(5, 0, sequence, u64::from(id), 0, body.len() as u32, &body)
}

pub(crate) fn collection_drop_frame(sequence: u64, id: u16) -> Vec<u8> {
    wrap(6, 0, sequence, u64::from(id), 0, 0, &[])
}

pub(crate) fn settings_frame(sequence: u64, id: u16, settings: &CollectionSettings) -> Vec<u8> {
    let body = encode_settings(settings);
    wrap(7, 0, sequence, u64::from(id), 0, body.len() as u32, &body)
}

fn metric_code(metric: DistanceMetric) -> u8 {
    match metric {
        DistanceMetric::SquaredL2 => 0,
        DistanceMetric::Cosine => 1,
        DistanceMetric::InnerProduct => 2,
    }
}

/// Settings blob v1. Callers MUST validate settings first.
pub(crate) fn encode_settings(settings: &CollectionSettings) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.push(metric_code(settings.metric));
    bytes.extend_from_slice(&(settings.hnsw.m as u16).to_le_bytes());
    bytes.extend_from_slice(&(settings.hnsw.ef_construction as u16).to_le_bytes());
    bytes.extend_from_slice(&settings.hnsw.seed.to_le_bytes());
    bytes.push(settings.quantization.code());
    bytes.push(settings.rerank_multiplier);
    bytes.extend_from_slice(&settings.planner.exact_max_candidates.to_le_bytes());
    bytes.extend_from_slice(&settings.planner.exact_max_selectivity_ppm.to_le_bytes());
    bytes.push(settings.metadata_indexes.len() as u8);
    for spec in &settings.metadata_indexes {
        bytes.push(spec.kind.code());
        bytes.extend_from_slice(&(spec.field.len() as u16).to_le_bytes());
        bytes.extend_from_slice(spec.field.as_bytes());
    }
    bytes.push(settings.text_fields.len() as u8);
    for field in &settings.text_fields {
        bytes.extend_from_slice(&(field.len() as u16).to_le_bytes());
        bytes.extend_from_slice(field.as_bytes());
    }
    bytes
}

/// Decodes and validates settings blob v1; errors are static reasons.
pub(crate) fn decode_settings(
    bytes: &[u8],
) -> std::result::Result<CollectionSettings, &'static str> {
    const TRUNCATED: &str = "truncated collection settings";
    fn take<'a>(bytes: &mut &'a [u8], n: usize) -> std::result::Result<&'a [u8], &'static str> {
        let value = bytes.get(..n).ok_or(TRUNCATED)?;
        *bytes = &bytes[n..];
        Ok(value)
    }
    fn field(bytes: &mut &[u8]) -> std::result::Result<String, &'static str> {
        let length = u16_at(take(bytes, 2)?, 0) as usize;
        let text = std::str::from_utf8(take(bytes, length)?)
            .map_err(|_| "settings field names must be UTF-8")?;
        Ok(text.to_owned())
    }
    if bytes.len() > MAX_SETTINGS_BLOB {
        return Err("collection settings exceed 16384 bytes");
    }
    let mut rest = bytes;
    if u16_at(take(&mut rest, 2)?, 0) != 1 {
        return Err("unsupported collection settings version");
    }
    let metric = match take(&mut rest, 1)?[0] {
        0 => DistanceMetric::SquaredL2,
        1 => DistanceMetric::Cosine,
        2 => DistanceMetric::InnerProduct,
        _ => return Err("invalid collection settings metric"),
    };
    let fixed = take(&mut rest, 2 + 2 + 8)?;
    let hnsw = HnswConfig {
        m: usize::from(u16_at(fixed, 0)),
        ef_construction: usize::from(u16_at(fixed, 2)),
        seed: u64_at(fixed, 4),
    };
    let quantization = Quantization::from_code(take(&mut rest, 1)?[0])
        .ok_or("invalid collection settings quantization")?;
    let rerank_multiplier = take(&mut rest, 1)?[0];
    let planner_bytes = take(&mut rest, 8)?;
    let planner = PlannerPolicy {
        exact_max_candidates: u32_at(planner_bytes, 0),
        exact_max_selectivity_ppm: u32_at(planner_bytes, 4),
    };
    let index_count = usize::from(take(&mut rest, 1)?[0]);
    if index_count > MAX_METADATA_INDEXES {
        return Err("collection settings declare more than 16 metadata indexes");
    }
    let mut metadata_indexes = Vec::with_capacity(index_count);
    for _ in 0..index_count {
        let kind = MetadataIndexKind::from_code(take(&mut rest, 1)?[0])
            .ok_or("invalid metadata index kind")?;
        metadata_indexes.push(MetadataIndexSpec {
            field: field(&mut rest)?,
            kind,
        });
    }
    let text_count = usize::from(take(&mut rest, 1)?[0]);
    if text_count > MAX_TEXT_FIELDS {
        return Err("collection settings declare more than 8 text fields");
    }
    let mut text_fields = Vec::with_capacity(text_count);
    for _ in 0..text_count {
        text_fields.push(field(&mut rest)?);
    }
    if !rest.is_empty() {
        return Err("trailing collection settings bytes");
    }
    let settings = CollectionSettings {
        metric,
        hnsw,
        quantization,
        rerank_multiplier,
        planner,
        metadata_indexes,
        text_fields,
    };
    settings.validate()?;
    Ok(settings)
}

pub(crate) fn decode_vector(bytes: &[u8], offset: u64) -> Result<Vec<f32>> {
    let vector: Vec<_> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| f32::from_le_bytes(*chunk))
        .collect();
    if vector.iter().any(|value| !value.is_finite()) {
        return Err(corrupt(offset, "non-finite stored vector component"));
    }
    Ok(vector)
}

pub(crate) fn decode_batch(
    body: &[u8],
    count: usize,
    dimensions: usize,
    offset: u64,
    version: u32,
) -> Result<(Vec<Mutation>, Option<ImportMarker>)> {
    let mut operations = Vec::with_capacity(count);
    let mut marker = None;
    let mut position = 0usize;
    for index in 0..count {
        let entry = body
            .get(position..position + 16)
            .ok_or_else(|| corrupt(offset, "truncated batch entry header"))?;
        let operation = entry[0];
        if !(matches!(operation, 1 | 2)
            || (operation == 4 && version >= 3)
            || (operation == 8 && version >= V4))
            || entry[1..4] != [0; 3]
        {
            return Err(corrupt(
                offset,
                "invalid batch entry operation or reserved bytes",
            ));
        }
        let id = u64_at(entry, 4);
        let payload_len = u32_at(entry, 12) as usize;
        if operation == 8 {
            if index + 1 != count {
                return Err(corrupt(
                    offset,
                    "import marker must be the last batch entry",
                ));
            }
            if id == 0 || !matches!(payload_len, IMPORT_MARKER_LEN | IMPORT_SNAPSHOT_LEN) {
                return Err(corrupt(offset, "invalid import marker job or length"));
            }
        }
        if (matches!(operation, 1 | 2) && payload_len > MAX_PAYLOAD_BYTES)
            || (operation == 4 && payload_len > MAX_EXTENDED_BODY)
            || (operation == 2 && payload_len != 0)
        {
            return Err(corrupt(offset, "invalid batch entry payload length"));
        }
        position += 16;
        if operation == 8 {
            let end = position + payload_len;
            let data = body
                .get(position..end)
                .ok_or_else(|| corrupt(offset, "truncated import marker"))?;
            let decoded = ImportMarker::decode(id, data);
            if decoded.len() != payload_len {
                return Err(corrupt(
                    offset,
                    "import marker length does not match its snapshot flag",
                ));
            }
            marker = Some(decoded);
            position = end;
        } else if operation == 4 {
            let end = position + payload_len;
            let record = decode_extended(
                body.get(position..end)
                    .ok_or_else(|| corrupt(offset, "truncated extended batch entry"))?,
                dimensions,
                offset,
                version >= V5,
            )?;
            operations.push(Mutation::PutWithMetadata {
                id,
                vector: record.vector,
                payload: record.payload,
                metadata: record.metadata,
            });
            position = end;
        } else if operation == 1 {
            let end = position + dimensions * 4 + payload_len;
            let data = body
                .get(position..end)
                .ok_or_else(|| corrupt(offset, "truncated batch entry body"))?;
            let vector = decode_vector(&data[..dimensions * 4], offset)?;
            operations.push(Mutation::Put {
                id,
                vector,
                payload: data[dimensions * 4..].to_vec(),
            });
            position = end;
        } else {
            operations.push(Mutation::Delete { id });
        }
    }
    if position != body.len() {
        return Err(corrupt(offset, "extra bytes after batch entries"));
    }
    // v4 prefixes admit one extra entry/76 bytes for a marker; data alone must
    // still respect the ordinary batch limits.
    let data_bytes = body.len() - marker.map_or(0, |marker| 16 + marker.len());
    if operations.len() > MAX_BATCH_OPERATIONS || data_bytes > MAX_BATCH_BYTES {
        return Err(corrupt(
            offset,
            "batch data exceeds operation or byte limits",
        ));
    }
    Ok((operations, marker))
}

#[derive(Clone, Copy)]
struct JobReplay {
    collection: u16,
    last_chunk: u64,
    closed: bool,
    rows_committed: u64,
    rows_skipped: u64,
}

/// Validates one marker against per-job log state (contiguity and closure).
fn replay_marker(
    jobs: &mut HashMap<u64, JobReplay>,
    collection: u16,
    marker: &ImportMarker,
    data_entries: usize,
    offset: u64,
) -> Result<()> {
    if marker.flags & !(MARKER_FINISHED | MARKER_ABORTED | MARKER_SNAPSHOT) != 0
        || marker.flags & (MARKER_FINISHED | MARKER_ABORTED) == (MARKER_FINISHED | MARKER_ABORTED)
    {
        return Err(corrupt(offset, "invalid import marker flags"));
    }
    let previous = jobs.get(&marker.job).copied();
    if let Some(previous) = previous {
        if previous.closed {
            return Err(corrupt(offset, "import marker after job closed"));
        }
        if previous.collection != collection {
            return Err(corrupt(offset, "import job collection mismatch"));
        }
    }
    let last = previous.map_or(0, |job| job.last_chunk);
    let closing = marker.flags & (MARKER_FINISHED | MARKER_ABORTED) != 0;
    if marker.flags & MARKER_SNAPSHOT != 0 {
        if previous.is_some() || closing || marker.chunk == 0 || data_entries != 0 {
            return Err(corrupt(offset, "invalid import snapshot marker"));
        }
    } else if closing {
        if marker.chunk != last
            || data_entries != 0
            || marker.rows_committed != 0
            || marker.rows_skipped != 0
            || marker.digest != [0; 32]
        {
            return Err(corrupt(offset, "invalid import finish or abort marker"));
        }
    } else if Some(marker.chunk) != last.checked_add(1) {
        return Err(corrupt(offset, "non-contiguous import chunk number"));
    } else if marker.rows_committed != data_entries as u64
        || marker.rows_committed + marker.rows_skipped > MAX_BATCH_OPERATIONS as u64
    {
        return Err(corrupt(
            offset,
            "import marker row counts do not match chunk",
        ));
    }
    // Cumulative counts are exact u64 values; overflow is corruption, never
    // a silent clamp. (Close markers carry zero counts.)
    let previous_committed = previous.map_or(0, |job| job.rows_committed);
    let previous_skipped = previous.map_or(0, |job| job.rows_skipped);
    let (Some(rows_committed), Some(rows_skipped)) = (
        previous_committed.checked_add(marker.rows_committed),
        previous_skipped.checked_add(marker.rows_skipped),
    ) else {
        return Err(corrupt(offset, "import row counts overflow"));
    };
    jobs.insert(
        marker.job,
        JobReplay {
            collection,
            last_chunk: marker.chunk,
            closed: closing,
            rows_committed,
            rows_skipped,
        },
    );
    Ok(())
}

pub(crate) fn decode_create(
    body: &[u8],
    offset: u64,
) -> Result<(String, usize, CollectionSettings)> {
    let name_len = usize::from(
        *body
            .first()
            .ok_or_else(|| corrupt(offset, "truncated collection create"))?,
    );
    let name = body
        .get(1..1 + name_len)
        .and_then(|name| std::str::from_utf8(name).ok())
        .filter(|name| valid_collection_name(name) && *name != "default")
        .ok_or_else(|| corrupt(offset, "invalid collection name"))?
        .to_owned();
    let dims = body
        .get(1 + name_len..5 + name_len)
        .ok_or_else(|| corrupt(offset, "truncated collection create"))?;
    let dimensions = u32_at(dims, 0) as usize;
    if !(1..=MAX_DIMENSIONS).contains(&dimensions) {
        return Err(corrupt(offset, "invalid collection dimensions"));
    }
    let settings =
        decode_settings(&body[5 + name_len..]).map_err(|reason| corrupt(offset, reason))?;
    Ok((name, dimensions, settings))
}

/// Parsed file header (plus encryption descriptor) of a log.
pub(crate) struct LogHeader {
    pub version: u32,
    pub dimensions: usize,
    pub lineage: Option<u64>,
    pub collection_floor: u16,
    pub flags: u16,
    pub cipher: Option<Cipher>,
    /// Offset of the first frame.
    pub start: u64,
    /// Exact header (and descriptor) bytes.
    pub raw: Vec<u8>,
    pub length: u64,
}

impl LogHeader {
    pub(crate) fn seal(&self) -> usize {
        if self.cipher.is_some() {
            SEAL_OVERHEAD
        } else {
            0
        }
    }
}

pub(crate) fn read_log_header<R: Read + Seek>(
    file: &mut R,
    key: Option<&EncryptionKey>,
) -> Result<LogHeader> {
    read_log_header_with(file, key, None)
}

/// Like `read_log_header`, but an already-open cipher may stand in for the key.
pub(crate) fn read_log_header_with<R: Read + Seek>(
    file: &mut R,
    key: Option<&EncryptionKey>,
    known: Option<&Cipher>,
) -> Result<LogHeader> {
    let length = file.seek(SeekFrom::End(0))?;
    file.seek(SeekFrom::Start(0))?;
    if length < HEADER_SIZE {
        return Err(corrupt(0, "missing or truncated database header"));
    }
    let mut bytes = [0; HEADER_SIZE as usize];
    file.read_exact(&mut bytes)?;
    if &bytes[..8] != MAGIC {
        return Err(corrupt(0, "invalid database magic"));
    }
    if crc32(&bytes[..28]) != u32_at(&bytes, 28) {
        return Err(corrupt(0, "database header checksum mismatch"));
    }
    let version = u32_at(&bytes, 8);
    if !matches!(version, 1..=V5) {
        return Err(Error::UnsupportedVersion(version));
    }
    let mut flags = 0u16;
    let (lineage, collection_floor) = if version >= V4 {
        if version >= V5 {
            flags = u16_at(&bytes, 26);
            let known_flags = KNOWN_FLAGS | GRAPH_READER_FLAGS;
            if flags & !known_flags != 0 {
                return Err(Error::UnsupportedFeature(flags & !known_flags));
            }
            if flags & FLAG_HISTORY != 0 && flags & FLAG_TIMESTAMPS != 0 {
                return Err(corrupt(0, "history flag combined with commit timestamps"));
            }
        } else if bytes[26..28] != [0; 2] {
            return Err(corrupt(0, "nonzero reserved database header bytes"));
        }
        let lineage = u64_at(&bytes, 16);
        if lineage == 0 {
            return Err(corrupt(0, "zero database lineage"));
        }
        (Some(lineage), u16_at(&bytes, 24))
    } else {
        if bytes[16..28] != [0; 12] {
            return Err(corrupt(0, "nonzero reserved database header bytes"));
        }
        (None, 0)
    };
    let dimensions = u32_at(&bytes, 12) as usize;
    if !(1..=MAX_DIMENSIONS).contains(&dimensions) {
        return Err(corrupt(0, "invalid database dimensions"));
    }
    let mut raw = bytes.to_vec();
    let mut start = HEADER_SIZE;
    let mut cipher = None;
    if flags & FLAG_ENCRYPTED != 0 {
        if length < HEADER_SIZE + DESCRIPTOR_SIZE as u64 {
            return Err(corrupt(HEADER_SIZE, "truncated encryption descriptor"));
        }
        let mut descriptor_bytes = [0u8; DESCRIPTOR_SIZE];
        file.read_exact(&mut descriptor_bytes)?;
        let descriptor = Descriptor::decode(&descriptor_bytes)?;
        cipher = Some(match known {
            Some(known) if known.descriptor == descriptor => known.clone(),
            Some(_) => return Err(corrupt(HEADER_SIZE, "encryption descriptor changed")),
            None => Cipher::open(key.ok_or(Error::EncryptionKeyRequired)?, &descriptor)?,
        });
        raw.extend_from_slice(&descriptor_bytes);
        start += DESCRIPTOR_SIZE as u64;
    } else if key.is_some() || known.is_some() {
        return Err(Error::NotEncrypted);
    }
    Ok(LogHeader {
        version,
        dimensions,
        lineage,
        collection_floor,
        flags,
        cipher,
        start,
        raw,
        length,
    })
}

/// Body length implied by a frame prefix (plaintext), without validating it.
pub(crate) fn plain_body_len(operation: u8, count: u32, payload_len: u32) -> usize {
    match operation {
        1 => count as usize * 4 + payload_len as usize,
        2 => 0,
        _ => payload_len as usize,
    }
}

/// A structurally verified frame read from a log.
pub(crate) struct RawFrame {
    pub operation: u8,
    pub collection: u16,
    pub sequence: u64,
    pub id: u64,
    pub count: u32,
    /// Exact on-disk bytes (sealed for encrypted logs).
    pub disk: Vec<u8>,
    /// Plaintext `wrap` layout (equals `disk` for unencrypted logs).
    pub plain: Vec<u8>,
}

impl RawFrame {
    pub(crate) fn body(&self) -> &[u8] {
        &self.plain[PREFIX_SIZE..self.plain.len() - FOOTER_SIZE]
    }
}

/// Reads the next frame at `offset`, verifying checksums and authentication.
/// `None` means a clean end or an incomplete trailing frame. Callers validate
/// semantics; use `load` for full replay validation.
pub(crate) fn next_frame<R: Read + Seek>(
    file: &mut R,
    header: &LogHeader,
    offset: u64,
) -> Result<Option<RawFrame>> {
    if header.length.saturating_sub(offset) < PREFIX_SIZE as u64 {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut prefix = [0; PREFIX_SIZE];
    file.read_exact(&mut prefix)?;
    if &prefix[..4] != b"VREC" {
        return Err(corrupt(offset, "invalid record magic"));
    }
    if crc32(&prefix[..32]) != u32_at(&prefix, 32) {
        return Err(corrupt(offset, "record header checksum mismatch"));
    }
    let operation = prefix[4];
    let count = u32_at(&prefix, 24);
    let payload_len = u32_at(&prefix, 28);
    let body_len = plain_body_len(operation, count, payload_len);
    if body_len > MAX_FRAME_BODY {
        return Err(corrupt(offset, "implausible frame length"));
    }
    let disk_len = PREFIX_SIZE + body_len + header.seal() + FOOTER_SIZE;
    if header.length - offset < disk_len as u64 {
        return Ok(None);
    }
    let mut disk = prefix.to_vec();
    disk.resize(disk_len, 0);
    file.read_exact(&mut disk[PREFIX_SIZE..])?;
    let sequence = u64_at(&prefix, 8);
    let plain = verified_frame(&disk, header.cipher.as_ref(), sequence, offset)?.into_owned();
    Ok(Some(RawFrame {
        operation,
        collection: u16_at(&prefix, 5),
        sequence,
        id: u64_at(&prefix, 16),
        count,
        disk,
        plain,
    }))
}

const MAX_FRAME_BODY: usize = 1 << 28;

pub(crate) fn load<R: Read + Seek>(file: &mut R) -> Result<Loaded> {
    load_with(file, None)
}

/// Full replay. It also accepts graph-enabled files
/// and replays their op-12 frames into `Loaded::graph`.
pub(crate) fn load_with<R: Read + Seek>(
    file: &mut R,
    key: Option<&EncryptionKey>,
) -> Result<Loaded> {
    load_inner(file, key, None)
}

/// Like `load_with`, with an already-open cipher (`None` for plaintext logs)
/// standing in for the key; the descriptor must match.
pub(crate) fn load_known<R: Read + Seek>(file: &mut R, cipher: Option<&Cipher>) -> Result<Loaded> {
    load_inner(file, None, cipher)
}

fn load_inner<R: Read + Seek>(
    file: &mut R,
    key: Option<&EncryptionKey>,
    known: Option<&Cipher>,
) -> Result<Loaded> {
    let header = read_log_header_with(file, key, known)?;
    let length = header.length;
    let LogHeader {
        version,
        dimensions,
        lineage,
        collection_floor,
        flags,
        cipher,
        start,
        ..
    } = header;
    file.seek(SeekFrom::Start(start))?;
    let seal = if cipher.is_some() { SEAL_OVERHEAD } else { 0 };
    let mut loaded = Loaded {
        version,
        dimensions,
        lineage,
        collection_floor,
        collection_high: collection_floor,
        collections: BTreeMap::from([(
            0,
            LoadedCollection::new("default".into(), dimensions, CollectionSettings::default()),
        )]),
        imports: Vec::new(),
        sequence: 0,
        end: start,
        incomplete_bytes: 0,
        flags,
        cipher: cipher.clone(),
        commit_times: Vec::new(),
        graph: (flags & FLAG_GRAPH != 0).then(crate::property_graph::engine::GraphState::new),
        history_floor: 0,
    };
    let mut jobs: HashMap<u64, JobReplay> = HashMap::new();
    // Highest collection ID created by an op 5 in this log so far, and how
    // many create frames and commit frames (excluding the advance) were seen.
    let mut created_high = 0u16;
    let mut creates_seen = 0u64;
    let mut commits_seen = 0u64;
    // Sequence of the last restated frame while frames after an advance may
    // still restate history (sequence at most the floor); `None` otherwise.
    let mut restating_from: Option<u64> = None;
    // A time anchor read ahead of its commit frame: (timestamp, frame bytes).
    let mut anchor: Option<(u64, u64)> = None;
    // Checkpoint-image snapshot frames not yet applied (Speed P2b): the run
    // of record blocks being read, and the graph snapshot bodies with the
    // offset of their first frame.
    let mut blocks: Option<BlockRun> = None;
    let mut snapshot: Option<(u64, Vec<crate::property_graph::engine::GraphBody>)> = None;
    let mut disk = Vec::new();
    loop {
        let anchor_len = anchor.map_or(0, |(_, len)| len);
        let offset = loaded.end + anchor_len;
        if offset >= length {
            loaded.incomplete_bytes = anchor_len;
            break;
        }
        let remaining = length - offset;
        if remaining < PREFIX_SIZE as u64 {
            let mut partial = vec![0; remaining as usize];
            file.read_exact(&mut partial)?;
            let magic_len = partial.len().min(4);
            if partial[..magic_len] != b"VREC"[..magic_len] {
                return Err(corrupt(offset, "invalid trailing record magic"));
            }
            loaded.incomplete_bytes = remaining + anchor_len;
            break;
        }
        let mut prefix = [0; PREFIX_SIZE];
        file.read_exact(&mut prefix)?;
        if &prefix[..4] != b"VREC" {
            return Err(corrupt(offset, "invalid record magic"));
        }
        if crc32(&prefix[..32]) != u32_at(&prefix, 32) {
            return Err(corrupt(offset, "record header checksum mismatch"));
        }
        let operation = prefix[4];
        if operation == OP_TIME_ANCHOR {
            let valid = flags & FLAG_TIMESTAMPS != 0
                && anchor.is_none()
                && prefix[5..8] == [0; 3]
                && u32_at(&prefix, 24) == 0
                && u64_at(&prefix, 16) == 0
                && u32_at(&prefix, 28) == 8
                && loaded.sequence.checked_add(1) == Some(u64_at(&prefix, 8));
            if !valid {
                return Err(corrupt(offset, "invalid time anchor frame"));
            }
            let length_on_disk = (PREFIX_SIZE + 8 + seal + FOOTER_SIZE) as u64;
            if remaining < length_on_disk {
                loaded.incomplete_bytes = remaining;
                break;
            }
            disk.clear();
            disk.extend_from_slice(&prefix);
            disk.resize(length_on_disk as usize, 0);
            file.read_exact(&mut disk[PREFIX_SIZE..])?;
            let plain = verified_frame(&disk, cipher.as_ref(), u64_at(&prefix, 8), offset)?;
            anchor = Some((u64_at(&plain, PREFIX_SIZE), length_on_disk));
            continue;
        }
        if flags & FLAG_TIMESTAMPS != 0 && anchor.is_none() {
            return Err(corrupt(offset, "commit without a time anchor"));
        }
        if operation == OP_ADVANCE {
            let floor = u64_at(&prefix, 8);
            let valid = flags & FLAG_HISTORY != 0
                && offset == start
                && prefix[5..8] == [0; 3]
                && u64_at(&prefix, 16) == 0
                && u32_at(&prefix, 24) == 0
                && u32_at(&prefix, 28) == 0
                && floor >= 1;
            if !valid {
                return Err(corrupt(offset, "invalid advance frame"));
            }
            let length_on_disk = (PREFIX_SIZE + seal + FOOTER_SIZE) as u64;
            if remaining < length_on_disk {
                loaded.incomplete_bytes = remaining;
                break;
            }
            disk.clear();
            disk.extend_from_slice(&prefix);
            disk.resize(length_on_disk as usize, 0);
            file.read_exact(&mut disk[PREFIX_SIZE..])?;
            verified_frame(&disk, cipher.as_ref(), floor, offset)?;
            loaded.sequence = floor;
            loaded.history_floor = floor;
            loaded.end += length_on_disk;
            restating_from = Some(1);
            continue;
        }
        if flags & FLAG_HISTORY != 0 && offset == start {
            return Err(corrupt(offset, "history image without an advance frame"));
        }
        let allowed = match operation {
            1 | 2 => true,
            3 => version >= 2,
            4 => version >= 3,
            5..=7 => version >= V4,
            8 => version >= V5,
            9 | 10 => version >= V5,
            // FLAG_GRAPH only passes the header check on v5.
            OP_GRAPH => version >= V5 && flags & FLAG_GRAPH != 0,
            OP_RECORD_BLOCK => version >= V5 && flags & FLAG_HISTORY != 0,
            OP_GRAPH_SNAPSHOT => {
                version >= V5 && flags & FLAG_HISTORY != 0 && flags & FLAG_GRAPH != 0
            }
            _ => false,
        };
        let reserved_clear =
            if version >= V4 && matches!(operation, 1..=4 | 8 | 9 | 10 | OP_RECORD_BLOCK) {
                prefix[7] == 0
            } else {
                prefix[5..8] == [0; 3]
            };
        if !allowed || !reserved_clear {
            return Err(corrupt(
                offset,
                "invalid operation or reserved record bytes",
            ));
        }
        let collection_id = u16_at(&prefix, 5);
        let sequence = u64_at(&prefix, 8);
        // A pending snapshot or run of record blocks ends at the first frame
        // that does not continue it.
        if operation != OP_GRAPH_SNAPSHOT
            && let Some((first, bodies)) = snapshot.take()
        {
            let floor = loaded.history_floor;
            let state = crate::property_graph::frame::load_snapshot(
                &loaded.collections,
                floor,
                bodies,
                first,
            )?;
            loaded.graph = Some(state);
        }
        if blocks
            .as_ref()
            .is_some_and(|run| operation != OP_RECORD_BLOCK || run.collection != collection_id)
        {
            let run = blocks.take().expect("pending record blocks");
            run.apply(&mut loaded, cipher.as_ref())?;
        }
        if matches!(operation, OP_RECORD_BLOCK | OP_GRAPH_SNAPSHOT)
            && (restating_from.is_none() || sequence != loaded.history_floor)
        {
            return Err(corrupt(offset, "snapshot frame outside a checkpoint image"));
        }
        let restated = match restating_from {
            Some(last) if (last..=loaded.history_floor).contains(&sequence) => true,
            _ => {
                if loaded.sequence.checked_add(1) != Some(sequence) {
                    return Err(corrupt(offset, "non-contiguous commit sequence"));
                }
                restating_from = None;
                false
            }
        };
        let id = u64_at(&prefix, 16);
        let count = u32_at(&prefix, 24) as usize;
        let payload_len = u32_at(&prefix, 28) as usize;
        if operation == OP_GRAPH
            && !crate::property_graph::valid_frame_prefix(id, count, payload_len)
        {
            return Err(corrupt(
                offset,
                "invalid graph frame count, reserved ID or body length",
            ));
        }
        // A snapshot loads into an empty graph state: before any graph
        // frame, unless it continues the snapshot being read.
        let graph_started = loaded
            .graph
            .as_ref()
            .is_some_and(|graph| graph.last_sequence() != 0);
        if operation == OP_GRAPH_SNAPSHOT
            && (!crate::property_graph::valid_frame_prefix(id, count, payload_len)
                || (snapshot.is_none() && graph_started))
        {
            return Err(corrupt(offset, "invalid graph snapshot"));
        }
        let target_dimensions =
            if operation <= 4 || operation == 9 || operation == OP_RECORD_BLOCK {
                loaded
                    .collections
                    .get(&collection_id)
                    .ok_or_else(|| corrupt(offset, "record references unknown collection"))?
                    .dimensions
            } else if operation == 8 || operation == 10 {
                loaded.collections.get(&collection_id).ok_or_else(|| {
                    corrupt(offset, "metadata update references unknown collection")
                })?;
                0
            } else {
                0
            };
        let body_len = if operation == 3 {
            let (max_count, max_bytes) = if version >= V4 {
                (
                    MAX_BATCH_OPERATIONS + 1,
                    MAX_BATCH_BYTES + IMPORT_SNAPSHOT_ENTRY,
                )
            } else {
                (MAX_BATCH_OPERATIONS, MAX_BATCH_BYTES)
            };
            if id != 0
                || !(1..=max_count).contains(&count)
                || payload_len > max_bytes
                || payload_len < count * 16
            {
                return Err(corrupt(
                    offset,
                    "invalid batch count, reserved ID or body length",
                ));
            }
            payload_len
        } else if operation == 4 {
            if count != target_dimensions
                || payload_len > MAX_EXTENDED_BODY
                || payload_len < target_dimensions * 4 + 8
            {
                return Err(corrupt(offset, "invalid extended record length"));
            }
            payload_len
        } else if (5..=7).contains(&operation) {
            let valid = count == 0
                && id <= u64::from(u16::MAX)
                && match operation {
                    5 => id != 0 && (6..=MAX_CREATE_BODY).contains(&payload_len),
                    6 => id != 0 && payload_len == 0,
                    _ => payload_len <= MAX_SETTINGS_BLOB,
                };
            if !valid {
                return Err(corrupt(offset, "invalid collection frame fields"));
            }
            payload_len
        } else if operation == 10 {
            if count != 0 || id != 0 || payload_len > crate::MAX_BATCH_BYTES {
                return Err(corrupt(offset, "invalid vector extension length"));
            }
            payload_len
        } else if operation == 8 {
            if count != 0 || !(4..=crate::MAX_METADATA_BYTES).contains(&payload_len) {
                return Err(corrupt(offset, "invalid metadata-only update length"));
            }
            payload_len
        } else if operation == 9 {
            if version < V5
                || count != target_dimensions
                || payload_len > MAX_EXTENDED_BODY + 9
                || payload_len < target_dimensions * 4 + 17
            {
                return Err(corrupt(offset, "invalid expiring record length"));
            }
            payload_len
        } else if operation == OP_RECORD_BLOCK {
            // Records are restated before every graph frame, so no
            // attachment can refer to them yet.
            let min_entry = BLOCK_ENTRY_HEADER + target_dimensions * 4 + 8;
            if id != 0
                || !(1..=MAX_BLOCK_RECORDS).contains(&count)
                || payload_len > MAX_RECORD_BLOCK
                || payload_len < count * min_entry
                || graph_started
            {
                return Err(corrupt(offset, "invalid record block"));
            }
            payload_len
        } else if operation <= 2 {
            if (operation == 1 && count != target_dimensions)
                || (operation == 2 && (count != 0 || payload_len != 0))
                || payload_len > MAX_PAYLOAD_BYTES
            {
                return Err(corrupt(offset, "invalid vector or payload length"));
            }
            count * 4 + payload_len
        } else {
            // Ops 12 and 15 in graph builds; their prefix was validated above.
            payload_len
        };
        let frame_len = (PREFIX_SIZE + body_len + seal + FOOTER_SIZE) as u64;
        if remaining < frame_len {
            loaded.incomplete_bytes = remaining + anchor_len;
            break;
        }
        disk.clear();
        disk.extend_from_slice(&prefix);
        disk.resize(frame_len as usize, 0);
        file.read_exact(&mut disk[PREFIX_SIZE..])?;
        if operation == OP_RECORD_BLOCK {
            // Verified and decoded with the rest of the run (restated, so
            // only the read position and commit count advance here).
            blocks
                .get_or_insert_with(|| BlockRun::new(&loaded, collection_id))
                .push(offset, std::mem::take(&mut disk), cipher.as_ref())?;
            restating_from = Some(sequence);
            commits_seen += 1;
            loaded.end += anchor_len + frame_len;
            continue;
        }
        let frame = verified_frame(&disk, cipher.as_ref(), sequence, offset)?;
        let body = &frame[PREFIX_SIZE..PREFIX_SIZE + body_len];
        match operation {
            10 => {
                let data = crate::vectors::VectorData::decode(body)
                    .map_err(|_| corrupt(offset, "invalid vector extension"))?;
                let target = collection_mut(&mut loaded, collection_id);
                if data
                    .records
                    .keys()
                    .any(|id| !target.records.contains_key(id))
                {
                    return Err(corrupt(
                        offset,
                        "vector extension references missing record",
                    ));
                }
                target.vectors = data;
            }
            1 => {
                let vector = decode_vector(&body[..count * 4], offset)?;
                let record = Record {
                    vector,
                    payload: body[count * 4..].to_vec(),
                    metadata: crate::Metadata::new(),
                };
                collection_mut(&mut loaded, collection_id).put(id, record, sequence, None);
                touch_attached(&mut loaded, collection_id, id, sequence);
            }
            4 => {
                let record = decode_extended(body, target_dimensions, offset, version >= V5)?;
                collection_mut(&mut loaded, collection_id).put(id, record, sequence, None);
                touch_attached(&mut loaded, collection_id, id, sequence);
            }
            9 => {
                let expiry_flag = body[0];
                if expiry_flag > 1 || (expiry_flag == 0 && u64_at(body, 1) != 0) {
                    return Err(corrupt(offset, "invalid expiry marker"));
                }
                let expiry = (expiry_flag == 1).then(|| u64_at(body, 1));
                ensure_attached_record_survives(
                    &loaded,
                    collection_id,
                    id,
                    expiry.is_some(),
                    offset,
                )?;
                let record = decode_extended(&body[9..], target_dimensions, offset, true)?;
                collection_mut(&mut loaded, collection_id).put(id, record, sequence, expiry);
                touch_attached(&mut loaded, collection_id, id, sequence);
            }
            8 => {
                let metadata = crate::metadata::decode(body)
                    .map_err(|_| corrupt(offset, "invalid metadata update"))?;
                collection_mut(&mut loaded, collection_id)
                    .update_metadata(id, metadata, sequence, offset)?;
                touch_attached(&mut loaded, collection_id, id, sequence);
            }
            2 => {
                ensure_attached_record_survives(&loaded, collection_id, id, true, offset)?;
                collection_mut(&mut loaded, collection_id).delete(id);
            }
            3 => {
                let (operations, marker) =
                    decode_batch(body, count, target_dimensions, offset, version)?;
                if let Some(marker) = &marker {
                    replay_marker(&mut jobs, collection_id, marker, operations.len(), offset)?;
                }
                for operation in &operations {
                    match operation {
                        Mutation::Delete { id } => {
                            ensure_attached_record_survives(
                                &loaded,
                                collection_id,
                                *id,
                                true,
                                offset,
                            )?;
                        }
                        Mutation::Put { id, .. } | Mutation::PutWithMetadata { id, .. } => {
                            touch_attached(&mut loaded, collection_id, *id, sequence);
                        }
                    }
                }
                let collection = collection_mut(&mut loaded, collection_id);
                for operation in operations {
                    match operation {
                        Mutation::Put {
                            id,
                            vector,
                            payload,
                        } => collection.put(
                            id,
                            Record {
                                vector,
                                payload,
                                metadata: crate::Metadata::new(),
                            },
                            sequence,
                            None,
                        ),
                        Mutation::PutWithMetadata {
                            id,
                            vector,
                            payload,
                            metadata,
                        } => collection.put(
                            id,
                            Record {
                                vector,
                                payload,
                                metadata,
                            },
                            sequence,
                            None,
                        ),
                        Mutation::Delete { id } => collection.delete(id),
                    }
                }
                if let Some(marker) = marker {
                    loaded.imports.push(LoadedImportMarker::Marker {
                        collection: collection_id,
                        name: loaded.collections[&collection_id].name.clone(),
                        marker,
                    });
                }
            }
            5 => {
                let collection = id as u16;
                let (name, dimensions, settings) = decode_create(body, offset)?;
                // Collection IDs are never reused within a lineage history:
                // creates are strictly increasing, and an ID at or below the
                // header floor is only valid in the leading run of create
                // frames that compaction/upgrade write to restate live
                // collections (every frame so far is such a create).
                let restating = creates_seen == commits_seen;
                if collection <= created_high
                    || (collection <= loaded.collection_floor && !restating)
                {
                    return Err(corrupt(offset, "collection ID reused"));
                }
                if loaded.collections.contains_key(&collection) {
                    return Err(corrupt(offset, "collection ID already in use"));
                }
                if loaded.collections.values().any(|c| c.name == name) {
                    return Err(corrupt(offset, "duplicate collection name"));
                }
                if loaded.collections.len() >= MAX_COLLECTIONS {
                    return Err(corrupt(offset, "too many live collections"));
                }
                let mut created = LoadedCollection::new(name, dimensions, settings);
                created.settings_persisted = true;
                loaded.collections.insert(collection, created);
                created_high = collection;
                creates_seen += 1;
                loaded.collection_high = loaded.collection_high.max(collection);
            }
            6 => {
                let collection = id as u16;
                let existing = loaded
                    .collections
                    .get(&collection)
                    .ok_or_else(|| corrupt(offset, "drop of unknown collection"))?;
                if !existing.records.is_empty()
                    || jobs
                        .values()
                        .any(|job| job.collection == collection && !job.closed)
                {
                    return Err(corrupt(offset, "drop of non-empty collection"));
                }
                loaded.collections.remove(&collection);
                loaded
                    .imports
                    .push(LoadedImportMarker::CollectionDropped(collection));
            }
            OP_GRAPH => crate::property_graph::frame::replay(
                loaded
                    .graph
                    .get_or_insert_with(crate::property_graph::engine::GraphState::new),
                &mut loaded.collections,
                sequence,
                count as u32,
                body,
                offset,
                restated,
            )?,
            OP_GRAPH_SNAPSHOT => {
                let body = crate::property_graph::engine::decode_body(body, count as u32)
                    .map_err(|_| corrupt(offset, "invalid graph snapshot"))?;
                snapshot
                    .get_or_insert_with(|| (offset, Vec::new()))
                    .1
                    .push(body);
            }
            _ => {
                let settings = decode_settings(body).map_err(|reason| corrupt(offset, reason))?;
                let collection = loaded
                    .collections
                    .get_mut(&(id as u16))
                    .ok_or_else(|| corrupt(offset, "settings for unknown collection"))?;
                collection.settings = settings;
                collection.settings_persisted = true;
            }
        }
        if restated {
            restating_from = Some(sequence);
        } else {
            loaded.sequence = sequence;
        }
        commits_seen += 1;
        loaded.end += anchor_len + frame_len;
        if let Some((timestamp, _)) = anchor.take() {
            loaded.commit_times.push((sequence, timestamp));
        }
    }
    if let Some(run) = blocks {
        run.apply(&mut loaded, cipher.as_ref())?;
    }
    if let Some((first, bodies)) = snapshot {
        let state = crate::property_graph::frame::load_snapshot(
            &loaded.collections,
            loaded.history_floor,
            bodies,
            first,
        )?;
        loaded.graph = Some(state);
    }
    Ok(loaded)
}

/// One decoded record-block entry.
struct BlockEntry {
    id: u64,
    revision: u64,
    expiry: Option<u64>,
    record: Record,
    body_bytes: u64,
}

/// Pending record blocks are verified and decoded once they hold this many
/// bytes, bounding the undecoded copy kept in memory.
const BLOCK_BATCH_BYTES: usize = 64 << 20;

/// A run of record blocks (op 14) of one collection, read but not yet
/// applied (Speed P2b). Blocks are verified and decoded in batches, in
/// parallel off wasm; the entries are added to the collection in bulk when
/// the run ends.
struct BlockRun {
    collection: u16,
    dimensions: usize,
    /// The checkpoint sequence the blocks restate.
    floor: u64,
    /// Complete frames not yet verified: offset and on-disk bytes.
    frames: Vec<(u64, Vec<u8>)>,
    frame_bytes: usize,
    /// Highest record ID so far; every later ID must exceed it.
    last_id: Option<u64>,
    entries: Vec<BlockEntry>,
}

impl BlockRun {
    fn new(loaded: &Loaded, collection: u16) -> Self {
        let target = &loaded.collections[&collection];
        Self {
            collection,
            dimensions: target.dimensions,
            floor: loaded.history_floor,
            frames: Vec::new(),
            frame_bytes: 0,
            last_id: target.records.keys().next_back().copied(),
            entries: Vec::new(),
        }
    }

    fn push(&mut self, offset: u64, disk: Vec<u8>, cipher: Option<&Cipher>) -> Result<()> {
        self.frame_bytes += disk.len();
        self.frames.push((offset, disk));
        if self.frame_bytes >= BLOCK_BATCH_BYTES {
            self.decode(cipher)?;
        }
        Ok(())
    }

    /// Verifies and decodes the pending frames and appends their entries,
    /// which must continue the strictly ascending ID order.
    fn decode(&mut self, cipher: Option<&Cipher>) -> Result<()> {
        let frames = std::mem::take(&mut self.frames);
        self.frame_bytes = 0;
        let (dimensions, floor) = (self.dimensions, self.floor);
        let decode = |(offset, disk): &(u64, Vec<u8>)| {
            let frame = verified_frame(disk, cipher, floor, *offset)?;
            decode_block(&frame, dimensions, floor)
                .ok_or_else(|| corrupt(*offset, "invalid record block"))
        };
        #[cfg(not(target_arch = "wasm32"))]
        let decoded: Vec<Vec<BlockEntry>> = {
            use rayon::prelude::*;
            frames.par_iter().map(decode).collect::<Result<_>>()?
        };
        #[cfg(target_arch = "wasm32")]
        let decoded: Vec<Vec<BlockEntry>> = frames.iter().map(decode).collect::<Result<_>>()?;
        for ((offset, _), block) in frames.iter().zip(decoded) {
            if self.last_id.is_some_and(|last| block[0].id <= last) {
                return Err(corrupt(*offset, "invalid record block"));
            }
            self.last_id = block.last().map(|entry| entry.id);
            self.entries.extend(block);
        }
        Ok(())
    }

    /// Adds every entry to the collection, as restating each record in its
    /// own frame would. All IDs exceed the collection's existing records, so
    /// no record is replaced and no staged vector data refers to them.
    fn apply(mut self, loaded: &mut Loaded, cipher: Option<&Cipher>) -> Result<()> {
        self.decode(cipher)?;
        let target = collection_mut(loaded, self.collection);
        let entries = self.entries;
        target.live_body_bytes += entries.iter().map(|entry| entry.body_bytes).sum::<u64>();
        extend_ascending(
            &mut target.revisions,
            entries.iter().map(|entry| (entry.id, entry.revision)),
        );
        extend_ascending(
            &mut target.expiries,
            entries
                .iter()
                .filter_map(|entry| Some((entry.id, entry.expiry?))),
        );
        let records = entries.into_iter().map(|entry| (entry.id, entry.record));
        if target.records.is_empty() {
            target.records = records.collect();
        } else {
            for (id, record) in records {
                target.records.insert(id, record);
            }
        }
        Ok(())
    }
}

/// Adds entries whose keys all exceed the map's, building it in bulk when
/// it is empty.
fn extend_ascending<V>(map: &mut BTreeMap<u64, V>, entries: impl Iterator<Item = (u64, V)>) {
    if map.is_empty() {
        *map = entries.collect();
    } else {
        map.extend(entries);
    }
}

/// Decodes the verified plaintext frame of a record block: exactly `count`
/// entries with strictly ascending IDs, revisions in `1..=floor`, canonical
/// expiry markers and extended bodies that fill the body exactly.
fn decode_block(frame: &[u8], dimensions: usize, floor: u64) -> Option<Vec<BlockEntry>> {
    let count = u32_at(frame, 24) as usize;
    let body = &frame[PREFIX_SIZE..frame.len() - FOOTER_SIZE];
    let mut entries: Vec<BlockEntry> = Vec::with_capacity(count);
    let mut at = 0;
    for _ in 0..count {
        let header = body.get(at..at + BLOCK_ENTRY_HEADER)?;
        let (id, revision) = (u64_at(header, 0), u64_at(header, 8));
        let (flag, expiry) = (header[16], u64_at(header, 17));
        let length = u32_at(header, 25) as usize;
        if flag > 1
            || (flag == 0 && expiry != 0)
            || !(1..=floor).contains(&revision)
            || entries.last().is_some_and(|last| last.id >= id)
        {
            return None;
        }
        at += BLOCK_ENTRY_HEADER;
        let record_body = body.get(at..at.checked_add(length)?)?;
        let record = decode_extended(record_body, dimensions, 0, true).ok()?;
        at += length;
        entries.push(BlockEntry {
            id,
            revision,
            expiry: (flag == 1).then_some(expiry),
            body_bytes: record_body_len(&record),
            record,
        });
    }
    (at == body.len()).then_some(entries)
}

/// Verifies a complete on-disk frame (footer, checksum, and for encrypted files
/// the authentication tag) and returns it in plaintext `wrap` layout.
pub(crate) fn verified_frame<'a>(
    disk: &'a [u8],
    cipher: Option<&Cipher>,
    sequence: u64,
    offset: u64,
) -> Result<std::borrow::Cow<'a, [u8]>> {
    let footer = &disk[disk.len() - FOOTER_SIZE..];
    if &footer[..4] != b"VCMT"
        || u64_at(footer, 8) != sequence
        || u32_at(footer, 4) != crc32(&disk[..disk.len() - FOOTER_SIZE])
    {
        return Err(corrupt(offset, "commit footer or record checksum mismatch"));
    }
    match cipher {
        None => Ok(std::borrow::Cow::Borrowed(disk)),
        Some(cipher) => cipher
            .open_frame(disk)
            .map(std::borrow::Cow::Owned)
            .ok_or_else(|| corrupt(offset, "frame authentication failed")),
    }
}

fn collection_mut(loaded: &mut Loaded, id: u16) -> &mut LoadedCollection {
    loaded
        .collections
        .get_mut(&id)
        .expect("collection existence checked before decoding")
}

fn ensure_attached_record_survives(
    loaded: &Loaded,
    collection: u16,
    record: u64,
    deleting_or_expiring: bool,
    offset: u64,
) -> Result<()> {
    if deleting_or_expiring
        && loaded
            .graph
            .as_ref()
            .is_some_and(|graph| graph.attachment_owner(collection, record).is_some())
    {
        return Err(corrupt(
            offset,
            "graph attachment references a missing or expiring record",
        ));
    }
    Ok(())
}

/// Live writes advance the owning graph's `graph_sequence` when an attached
/// record changes (Section 3.5); replay must rebuild the same value.
fn touch_attached(loaded: &mut Loaded, collection: u16, record: u64, sequence: u64) {
    if let Some(graph) = loaded.graph.as_mut() {
        graph.touch_record(collection, record, sequence);
    }
}

pub(crate) fn vector_frame(sequence: u64, collection: u16, body: &[u8]) -> Vec<u8> {
    wrap(10, collection, sequence, 0, 0, body.len() as u32, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history_log(flags: u16, frames: &[Vec<u8>]) -> Result<Loaded> {
        let mut bytes = header_v5_flags(2, 9, 0, flags, None);
        for frame in frames {
            bytes.extend_from_slice(frame);
        }
        load(&mut std::io::Cursor::new(bytes))
    }

    fn history_record(sequence: u64, id: u64) -> Vec<u8> {
        let record = Record {
            vector: vec![1.0, 0.0],
            payload: Vec::new(),
            metadata: Default::default(),
        };
        record_frame_v5(sequence, 0, id, &record, None)
    }

    fn rejected(result: Result<Loaded>, reason: &str) {
        match result {
            Err(Error::Corrupt { reason: actual, .. }) => assert_eq!(actual, reason),
            Err(other) => panic!("expected corrupt({reason}), got {other:?}"),
            Ok(_) => panic!("expected corrupt({reason}), loaded"),
        }
    }

    #[test]
    fn history_images_restate_up_to_the_floor_then_resume_contiguously() {
        let history = FLAG_GRAPH | FLAG_HISTORY;
        let loaded = history_log(
            history,
            &[
                advance_frame(3),
                history_record(1, 1),
                history_record(1, 2),
                history_record(3, 3),
                history_record(4, 1),
                history_record(5, 2),
            ],
        )
        .unwrap();
        assert_eq!(loaded.sequence, 5);
        assert_eq!(loaded.history_floor, 3);
        let revisions = &loaded.collections[&0].revisions;
        assert_eq!(revisions[&1], 4);
        assert_eq!(revisions[&2], 5);
        assert_eq!(revisions[&3], 3);

        let contiguous = "non-contiguous commit sequence";
        rejected(
            history_log(
                history,
                &[advance_frame(3), history_record(3, 1), history_record(2, 2)],
            ),
            contiguous,
        );
        rejected(
            history_log(history, &[advance_frame(3), history_record(5, 1)]),
            contiguous,
        );
        rejected(
            history_log(
                history,
                &[advance_frame(3), history_record(4, 1), history_record(3, 2)],
            ),
            contiguous,
        );
        rejected(
            history_log(history, &[history_record(1, 1)]),
            "history image without an advance frame",
        );
        rejected(
            history_log(history, &[advance_frame(2), advance_frame(2)]),
            "invalid advance frame",
        );
        rejected(
            history_log(FLAG_GRAPH, &[advance_frame(2)]),
            "invalid advance frame",
        );
        rejected(
            history_log(history, &[advance_frame(0)]),
            "invalid advance frame",
        );
        assert!(matches!(
            history_log(history | FLAG_TIMESTAMPS, &[]),
            Err(Error::Corrupt { .. })
        ));
    }

    fn block_record(id: u64) -> Record {
        let mut metadata = crate::Metadata::new();
        if id % 2 == 1 {
            metadata.insert("id".into(), crate::MetadataValue::Integer(id as i64));
        }
        Record {
            vector: vec![id as f32, 1.0],
            payload: id.to_le_bytes()[..(id % 9) as usize].to_vec(),
            metadata,
        }
    }

    fn block(collection: u16, entries: &[(u64, u64, Option<u64>)]) -> RecordBlock {
        let mut block = RecordBlock::new(collection);
        for &(id, revision, expiry) in entries {
            block.push(id, revision, expiry, &block_record(id));
        }
        block
    }

    #[test]
    fn record_blocks_load_like_restated_record_frames() {
        let history = FLAG_HISTORY;
        let tail = record_frame_v5(6, 0, 9, &block_record(9), None);
        let blocks = history_log(
            history,
            &[
                advance_frame(5),
                block(0, &[(1, 2, None), (4, 5, Some(9_000))]).take_frame(5),
                block(0, &[(7, 1, None), (8, 5, None)]).take_frame(5),
                tail.clone(),
            ],
        )
        .unwrap();
        let frames = history_log(
            history,
            &[
                advance_frame(5),
                record_frame_v5(1, 0, 7, &block_record(7), None),
                record_frame_v5(2, 0, 1, &block_record(1), None),
                record_frame_v5(5, 0, 4, &block_record(4), Some(9_000)),
                record_frame_v5(5, 0, 8, &block_record(8), None),
                tail,
            ],
        )
        .unwrap();
        assert_eq!(blocks.sequence, 6);
        assert_eq!(blocks.history_floor, 5);
        let (actual, expected) = (&blocks.collections[&0], &frames.collections[&0]);
        assert_eq!(actual.records, expected.records);
        assert_eq!(
            actual.records.keys().copied().collect::<Vec<_>>(),
            [1, 4, 7, 8, 9]
        );
        assert_eq!(actual.revisions, expected.revisions);
        assert_eq!(actual.revisions[&7], 1);
        assert_eq!(actual.expiries, BTreeMap::from([(4, 9_000)]));
        assert_eq!(actual.expiries, expected.expiries);
        assert_eq!(actual.live_body_bytes, expected.live_body_bytes);

        // A torn final block is an incomplete tail, like any other frame.
        let torn = block(0, &[(3, 1, None)]).take_frame(5);
        let loaded = history_log(
            history,
            &[
                advance_frame(5),
                block(0, &[(2, 1, None)]).take_frame(5),
                torn[..torn.len() - 3].to_vec(),
            ],
        )
        .unwrap();
        assert_eq!(loaded.collections[&0].records.len(), 1);
        assert_eq!(loaded.incomplete_bytes, torn.len() as u64 - 3);
    }

    #[test]
    fn record_blocks_are_rejected_outside_canonical_checkpoint_images() {
        let history = FLAG_HISTORY;
        let invalid = "invalid record block";
        let outside = "snapshot frame outside a checkpoint image";
        let one = |revision| block(0, &[(1, revision, None)]);
        let image = |frame: Vec<u8>| history_log(history, &[advance_frame(5), frame]);
        rejected(
            history_log(FLAG_GRAPH, &[one(1).take_frame(1)]),
            "invalid operation or reserved record bytes",
        );
        rejected(image(one(1).take_frame(4)), outside);
        rejected(image(one(1).take_frame(6)), outside);
        rejected(
            history_log(
                history,
                &[advance_frame(5), history_record(3, 2), one(1).take_frame(3)],
            ),
            outside,
        );
        for entries in [
            [(4, 1, None), (2, 1, None)],
            [(4, 1, None), (4, 1, None)],
            [(4, 0, None), (5, 1, None)],
            [(4, 1, None), (5, 6, None)],
        ] {
            rejected(image(block(0, &entries).take_frame(5)), invalid);
        }
        // IDs ascend across the blocks of a collection and past records
        // restated before them.
        rejected(
            history_log(
                history,
                &[
                    advance_frame(5),
                    block(0, &[(4, 1, None)]).take_frame(5),
                    block(0, &[(4, 1, None)]).take_frame(5),
                ],
            ),
            invalid,
        );
        rejected(
            history_log(
                history,
                &[advance_frame(5), history_record(2, 9), one(1).take_frame(5)],
            ),
            invalid,
        );
        rejected(
            image(block(3, &[(1, 1, None)]).take_frame(5)),
            "record references unknown collection",
        );
        let mut flag = one(1);
        flag.body[16] = 2;
        let mut value = one(1);
        value.body[17] = 1;
        let mut trailing = one(1);
        trailing.body.push(0);
        let mut missing = one(1);
        missing.count = 2;
        let mut empty = one(1);
        empty.count = 0;
        let mut long_entry = one(1);
        long_entry.body[25] += 1;
        let mut short_entry = one(1);
        short_entry.body[25] -= 1;
        for mut bad in [
            flag,
            value,
            trailing,
            missing,
            empty,
            long_entry,
            short_entry,
        ] {
            rejected(image(bad.take_frame(5)), invalid);
        }
        let mut frame = one(1).take_frame(5);
        frame[PREFIX_SIZE + 3] ^= 1;
        rejected(image(frame), "commit footer or record checksum mismatch");
    }

    #[test]
    fn graph_snapshots_load_like_restated_graph_frames() {
        use crate::property_graph::engine::{
            GraphBody, GraphEntry, GraphId, GraphState, NodeId, encode_body,
        };
        let history = FLAG_HISTORY | FLAG_GRAPH;
        let node = |id: u64, key: &str| GraphEntry::PutNode {
            node_id: NodeId(id),
            key: key.into(),
            labels: Default::default(),
            properties: Default::default(),
        };
        let encode = |graph: u16, entries: &[GraphEntry]| {
            let body = GraphBody {
                graph_id: GraphId(graph),
                entries: entries.to_vec(),
            };
            (entries.len() as u32, encode_body(&body).unwrap())
        };
        let entries = [
            GraphEntry::CreateGraph { name: "g".into() },
            node(0, "a"),
            node(3, "b"),
            GraphEntry::Attach {
                node_id: NodeId(0),
                collection: 0,
                record_id: 1,
            },
            GraphEntry::GraphHighWater {
                next_node_id: 7,
                next_edge_id: 2,
            },
        ];
        let records = || block(0, &[(1, 2, None)]).take_frame(5);
        // One snapshot may span several frames.
        let (first_count, first) = encode(0, &entries[..2]);
        let (second_count, second) = encode(0, &entries[2..]);
        let loaded = history_log(
            history,
            &[
                advance_frame(5),
                records(),
                graph_snapshot_frame(5, first_count, &first),
                graph_snapshot_frame(5, second_count, &second),
            ],
        )
        .unwrap();
        let (count, whole) = encode(0, &entries);
        let replayed = history_log(
            history,
            &[advance_frame(5), records(), graph_frame(5, count, &whole)],
        )
        .unwrap();
        let mut expected = GraphState::new();
        expected
            .apply_at(
                5,
                &GraphBody {
                    graph_id: GraphId(0),
                    entries: entries.to_vec(),
                },
                true,
            )
            .unwrap();
        assert_eq!(loaded.graph.as_ref(), Some(&expected));
        assert_eq!(loaded.graph, replayed.graph);

        // Later graph frames apply on top of the snapshot.
        let (one, delete) = encode(0, &[GraphEntry::DeleteNode { node_id: NodeId(3) }]);
        let loaded = history_log(
            history,
            &[
                advance_frame(5),
                records(),
                graph_snapshot_frame(5, count, &whole),
                graph_frame(6, one, &delete),
            ],
        )
        .unwrap();
        assert_eq!(loaded.sequence, 6);
        let graph = loaded.graph.unwrap();
        assert!(graph.graph(GraphId(0)).unwrap().node(NodeId(3)).is_none());

        let invalid = "invalid graph snapshot";
        let snapshot = || graph_snapshot_frame(5, count, &whole);
        rejected(
            history_log(FLAG_HISTORY, &[advance_frame(5), records(), snapshot()]),
            "invalid operation or reserved record bytes",
        );
        rejected(
            history_log(FLAG_GRAPH, &[graph_snapshot_frame(1, count, &whole)]),
            "invalid operation or reserved record bytes",
        );
        rejected(
            history_log(
                history,
                &[
                    advance_frame(5),
                    records(),
                    graph_snapshot_frame(4, count, &whole),
                ],
            ),
            "snapshot frame outside a checkpoint image",
        );
        let missing = "graph attachment references a missing or expiring record";
        rejected(
            history_log(history, &[advance_frame(5), snapshot()]),
            missing,
        );
        rejected(
            history_log(
                history,
                &[
                    advance_frame(5),
                    block(0, &[(1, 2, Some(9_000))]).take_frame(5),
                    snapshot(),
                ],
            ),
            missing,
        );
        let (unclosed_count, unclosed) = encode(0, &entries[..4]);
        rejected(
            history_log(
                history,
                &[
                    advance_frame(5),
                    records(),
                    graph_snapshot_frame(5, unclosed_count, &unclosed),
                ],
            ),
            invalid,
        );
        rejected(
            history_log(
                history,
                &[
                    advance_frame(5),
                    records(),
                    graph_snapshot_frame(5, count - 1, &whole),
                ],
            ),
            invalid,
        );
        // A snapshot loads only into an empty graph state, and record blocks
        // precede every graph frame.
        let (other_count, other) = encode(
            1,
            &[
                GraphEntry::CreateGraph { name: "h".into() },
                GraphEntry::GraphHighWater {
                    next_node_id: 0,
                    next_edge_id: 0,
                },
            ],
        );
        rejected(
            history_log(
                history,
                &[
                    advance_frame(5),
                    records(),
                    graph_frame(5, other_count, &other),
                    snapshot(),
                ],
            ),
            invalid,
        );
        rejected(
            history_log(
                history,
                &[
                    advance_frame(5),
                    graph_snapshot_frame(5, other_count, &other),
                    records(),
                ],
            ),
            "invalid record block",
        );
    }

    fn reference_crc32(previous: u32, bytes: &[u8]) -> u32 {
        let mut crc = !previous;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
            }
        }
        !crc
    }

    #[test]
    fn slicing_crc_matches_bitwise_reference_for_lengths_offsets_and_streaming() {
        let bytes: Vec<_> = (0..4104).map(|i| ((i * 37 + i / 19) & 255) as u8).collect();
        for offset in 0..8 {
            for length in 0..=4096 {
                let data = &bytes[offset..offset + length];
                let expected = reference_crc32(0, data);
                assert_eq!(crc32(data), expected);
                for split in [0, length / 2, length] {
                    assert_eq!(
                        crc32_update(crc32(&data[..split]), &data[split..]),
                        expected
                    );
                }
            }
        }
    }

    #[test]
    #[ignore = "bounded component timing; run explicitly in release mode"]
    fn replay_component_profile() {
        use std::{hint::black_box, time::Instant};
        let bytes: Vec<_> = (0..2048 * 512)
            .flat_map(|i| ((i % 997) as f32 / 997.0).to_le_bytes())
            .collect();
        let mut scalar = Vec::new();
        let mut sliced = Vec::new();
        let mut decode = Vec::new();
        for trial in 0..9 {
            let mut measure_crc = |reference: bool| {
                let start = Instant::now();
                let crc = if reference {
                    reference_crc32(0, black_box(&bytes))
                } else {
                    crc32(black_box(&bytes))
                };
                if reference {
                    scalar.push(start.elapsed().as_nanos());
                } else {
                    sliced.push(start.elapsed().as_nanos());
                }
                black_box(crc)
            };
            let (a, b) = if trial % 2 == 0 {
                (measure_crc(true), measure_crc(false))
            } else {
                (measure_crc(false), measure_crc(true))
            };
            assert_eq!(a, b);
            let start = Instant::now();
            black_box(decode_vector(black_box(&bytes), 0).unwrap());
            decode.push(start.elapsed().as_nanos());
        }
        println!(
            "bytes={} scalar_crc_ns={scalar:?} slicing_crc_ns={sliced:?} decode_ns={decode:?}",
            bytes.len()
        );
    }

    #[test]
    fn standard_crc32_check_vector() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn header_has_stable_portable_encoding() {
        let bytes = header(3);
        assert_eq!(bytes.len(), 32);
        assert_eq!(&bytes[..16], b"VLITEDB\0\x03\0\0\0\x03\0\0\0");
        assert_eq!(u32_at(&bytes, 28), crc32(&bytes[..28]));
    }

    fn load_bytes(bytes: &[u8]) -> Result<Loaded> {
        use std::io::Write;
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(bytes).unwrap();
        load(&mut file)
    }

    fn refresh_header_checksum(bytes: &mut [u8]) {
        let checksum = crc32(&bytes[..28]);
        bytes[28..32].copy_from_slice(&checksum.to_le_bytes());
    }

    fn refresh_record_checksums(bytes: &mut [u8]) {
        let checksum = crc32(&bytes[..32]);
        bytes[32..36].copy_from_slice(&checksum.to_le_bytes());
        let end = bytes.len() - FOOTER_SIZE;
        let checksum = crc32(&bytes[..end]);
        bytes[end + 4..end + 8].copy_from_slice(&checksum.to_le_bytes());
    }

    #[test]
    fn metadata_validation_cannot_be_bypassed_with_valid_checksums() {
        let record = Record {
            vector: vec![1.0],
            payload: vec![],
            metadata: std::collections::BTreeMap::from([(
                "x".into(),
                crate::MetadataValue::Boolean(true),
            )]),
        };
        let original = frame(1, 7, Some(&record));
        let tag_offset = PREFIX_SIZE + 4 + 4 + 4 + 2 + 1;
        for (position, value) in [
            (tag_offset, 99),
            (tag_offset + 1, 2),
            (PREFIX_SIZE + 4, 255),
        ] {
            let mut frame = original.clone();
            frame[position] = value;
            refresh_record_checksums(&mut frame);
            let mut bytes = header(1);
            bytes.extend_from_slice(&frame);
            assert!(matches!(load_bytes(&bytes), Err(Error::Corrupt { .. })));
        }
        for version in [1u32, 2] {
            let mut bytes = header(1);
            bytes[8..12].copy_from_slice(&version.to_le_bytes());
            refresh_header_checksum(&mut bytes);
            bytes.extend_from_slice(&original);
            assert!(matches!(load_bytes(&bytes), Err(Error::Corrupt { .. })));
        }
        let operations = [Mutation::PutWithMetadata {
            id: 7,
            vector: record.vector,
            payload: record.payload,
            metadata: record.metadata,
        }];
        let original = batch_frame(1, &operations, 16 + 4 + 4 + 9);
        let mut frame = original.clone();
        frame[tag_offset + 16] = 99;
        refresh_record_checksums(&mut frame);
        let mut bytes = header(1);
        bytes.extend_from_slice(&frame);
        assert!(matches!(load_bytes(&bytes), Err(Error::Corrupt { .. })));
        let mut bytes = header(1);
        bytes[8..12].copy_from_slice(&2u32.to_le_bytes());
        refresh_header_checksum(&mut bytes);
        bytes.extend_from_slice(&original);
        assert!(matches!(load_bytes(&bytes), Err(Error::Corrupt { .. })));
    }

    #[test]
    fn valid_checksums_do_not_bypass_header_validation() {
        let mut bytes = header(1);
        bytes[8..12].copy_from_slice(&99u32.to_le_bytes());
        refresh_header_checksum(&mut bytes);
        assert!(matches!(
            load_bytes(&bytes),
            Err(Error::UnsupportedVersion(99))
        ));
        for dimensions in [0u32, MAX_DIMENSIONS as u32 + 1, u32::MAX] {
            let mut bytes = header(1);
            bytes[12..16].copy_from_slice(&dimensions.to_le_bytes());
            refresh_header_checksum(&mut bytes);
            assert!(matches!(load_bytes(&bytes), Err(Error::Corrupt { .. })));
        }
        let mut bytes = header(1);
        bytes[16] = 1;
        refresh_header_checksum(&mut bytes);
        assert!(matches!(load_bytes(&bytes), Err(Error::Corrupt { .. })));
    }

    #[test]
    fn valid_checksums_do_not_bypass_record_validation() {
        let record = Record {
            vector: vec![1.0],
            payload: Vec::new(),
            metadata: crate::Metadata::new(),
        };
        let original = frame(1, 0x0807_0605_0403_0201, Some(&record));
        assert_eq!(&original[16..24], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(&original[PREFIX_SIZE..PREFIX_SIZE + 4], &[0, 0, 128, 63]);
        let cases: Vec<(usize, Vec<u8>)> = vec![
            (4, vec![3]),
            (5, vec![1]),
            (8, 2u64.to_le_bytes().to_vec()),
            (24, u32::MAX.to_le_bytes().to_vec()),
            (28, u32::MAX.to_le_bytes().to_vec()),
            (PREFIX_SIZE, f32::NAN.to_le_bytes().to_vec()),
            (PREFIX_SIZE, f32::INFINITY.to_le_bytes().to_vec()),
        ];
        for (offset, replacement) in cases {
            let mut frame = original.clone();
            frame[offset..offset + replacement.len()].copy_from_slice(&replacement);
            refresh_record_checksums(&mut frame);
            let mut bytes = header(1);
            bytes.extend_from_slice(&frame);
            assert!(
                matches!(load_bytes(&bytes), Err(Error::Corrupt { .. })),
                "invalid field at offset {offset}"
            );
        }
        let mut invalid_delete = original;
        invalid_delete[4] = 2;
        refresh_record_checksums(&mut invalid_delete);
        let mut bytes = header(1);
        bytes.extend_from_slice(&invalid_delete);
        assert!(matches!(load_bytes(&bytes), Err(Error::Corrupt { .. })));
    }

    #[test]
    fn complete_frame_after_a_sequence_gap_is_not_salvaged() {
        let record = Record {
            vector: vec![1.0],
            payload: Vec::new(),
            metadata: crate::Metadata::new(),
        };
        let mut bytes = header(1);
        bytes.extend_from_slice(&frame(1, 1, Some(&record)));
        bytes.extend_from_slice(&frame(3, 2, Some(&record)));
        assert!(matches!(load_bytes(&bytes), Err(Error::Corrupt { .. })));
    }

    #[test]
    fn valid_checksums_cannot_hide_malformed_batch_entries() {
        let operations = [
            Mutation::Put {
                id: 1,
                vector: vec![1.0],
                payload: vec![42],
            },
            Mutation::Delete { id: 2 },
        ];
        let original = batch_frame(1, &operations, 37);
        let cases = [
            (16, 1u64.to_le_bytes().to_vec()),
            (24, 0u32.to_le_bytes().to_vec()),
            (24, 1u32.to_le_bytes().to_vec()),
            (24, 3u32.to_le_bytes().to_vec()),
            (24, (MAX_BATCH_OPERATIONS as u32 + 1).to_le_bytes().to_vec()),
            (28, (MAX_BATCH_BYTES as u32 + 1).to_le_bytes().to_vec()),
            (PREFIX_SIZE, vec![3]),
            (PREFIX_SIZE + 1, vec![1]),
            (PREFIX_SIZE + 12, u32::MAX.to_le_bytes().to_vec()),
            (PREFIX_SIZE + 16, f32::NAN.to_le_bytes().to_vec()),
            (PREFIX_SIZE + 16, f32::INFINITY.to_le_bytes().to_vec()),
            (PREFIX_SIZE + 21 + 12, 1u32.to_le_bytes().to_vec()),
        ];
        for (position, replacement) in cases {
            let mut frame = original.clone();
            frame[position..position + replacement.len()].copy_from_slice(&replacement);
            refresh_record_checksums(&mut frame);
            let mut bytes = header(1);
            bytes.extend_from_slice(&frame);
            assert!(
                matches!(load_bytes(&bytes), Err(Error::Corrupt { .. })),
                "field at {position}"
            );
        }
        // Keep total body bytes unchanged but declare a larger entry payload.
        let mut frame = original.clone();
        frame[PREFIX_SIZE + 12..PREFIX_SIZE + 16].copy_from_slice(&32u32.to_le_bytes());
        refresh_record_checksums(&mut frame);
        let mut bytes = header(1);
        bytes.extend_from_slice(&frame);
        assert!(matches!(load_bytes(&bytes), Err(Error::Corrupt { .. })));

        let mut legacy = header(1);
        legacy[8..12].copy_from_slice(&1u32.to_le_bytes());
        refresh_header_checksum(&mut legacy);
        legacy.extend_from_slice(&original);
        assert!(matches!(load_bytes(&legacy), Err(Error::Corrupt { .. })));
    }
}
