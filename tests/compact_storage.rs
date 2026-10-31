//! Compact checkpoint images, slice 0 of
//! `docs/projects/roadmap/compact-storage-contract.md`.
//!
//! `FLAG_COMPACT` (16) and op-16 blocks are accepted only behind a complete
//! capability table and block envelope check, in an image closed by a
//! matching end block. This build has no compact
//! codec: a critical (or undefined) kind refuses the whole image with
//! `UnsupportedFeature(16)` before anything is written, while an authentic,
//! declared derived block is skipped. Images are encoded independently here
//! so the tests pin the on-disk contract, not the implementation.

use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};
use tempfile::tempdir;
use vlite::changes::{ChangeKind, FeedCursor, read_log_changes};
use vlite::maintenance::{self, RestoreTarget};
use vlite::property_graph::{enable_graph, strip_graph};
use vlite::{Database, EncryptionKey, Error, ReplicaFollower};

const FLAG_ENCRYPTED: u16 = 1;
const FLAG_HISTORY: u16 = 8;
const FLAG_COMPACT: u16 = 16;
const OP_ADVANCE: u8 = 13;
const OP_RECORD_BLOCK: u8 = 14;
const OP_COMPACT_BLOCK: u8 = 16;
const LINEAGE: u64 = 0x5EED_0000_0000_0001;
const FLOOR: u64 = 4;
const GOLDEN: &str = include_str!("fixtures/compact/derived-only-v5.hex");

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

fn decode_hex(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text.bytes().filter(u8::is_ascii_hexdigit).collect();
    digits
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn header(flags: u16) -> Vec<u8> {
    let mut bytes = b"VLITEDB\0".to_vec();
    bytes.extend_from_slice(&5u32.to_le_bytes());
    bytes.extend_from_slice(&2u32.to_le_bytes());
    bytes.extend_from_slice(&LINEAGE.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&flags.to_le_bytes());
    let checksum = crc32(&bytes);
    bytes.extend_from_slice(&checksum.to_le_bytes());
    bytes
}

fn frame(
    operation: u8,
    collection: u16,
    sequence: u64,
    id: u64,
    count: u32,
    body: &[u8],
) -> Vec<u8> {
    let mut bytes = b"VREC".to_vec();
    bytes.push(operation);
    bytes.extend_from_slice(&collection.to_le_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&sequence.to_le_bytes());
    bytes.extend_from_slice(&id.to_le_bytes());
    bytes.extend_from_slice(&count.to_le_bytes());
    bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
    let checksum = crc32(&bytes);
    bytes.extend_from_slice(&checksum.to_le_bytes());
    bytes.extend_from_slice(body);
    let checksum = crc32(&bytes);
    bytes.extend_from_slice(b"VCMT");
    bytes.extend_from_slice(&checksum.to_le_bytes());
    bytes.extend_from_slice(&sequence.to_le_bytes());
    bytes
}

/// A block body: the 32-byte header, the section table, then each section
/// after `gap` zero padding bytes.
#[derive(Clone)]
struct Block {
    kind: u16,
    version: u16,
    flags: u16,
    owner: u16,
    partition: u32,
    digest: u64,
    sections: Vec<(usize, Vec<u8>)>,
}

impl Block {
    fn derived(kind: u16, version: u16, partition: u32, sections: &[&[u8]]) -> Self {
        Self {
            kind,
            version,
            flags: 0,
            owner: 0,
            partition,
            digest: 0x1111_1111_1111_1111,
            sections: sections.iter().map(|data| (0, data.to_vec())).collect(),
        }
    }

    fn table(entries: &[Entry]) -> Self {
        let mut data = Vec::new();
        for &(kind, version, flags) in entries {
            for field in [kind, version, flags, 0] {
                data.extend_from_slice(&field.to_le_bytes());
            }
        }
        Self {
            kind: 1,
            version: 1,
            flags: 1,
            owner: 0,
            partition: 0,
            digest: 0,
            sections: vec![(0, data)],
        }
    }

    fn body(&self) -> Vec<u8> {
        let first = 32 + 8 * self.sections.len();
        let mut table = Vec::new();
        let mut data = Vec::new();
        let mut at = first;
        let mut offsets = Vec::new();
        for (gap, section) in &self.sections {
            at += gap;
            offsets.push(at);
            table.extend_from_slice(&(at as u32).to_le_bytes());
            table.extend_from_slice(&(section.len() as u32).to_le_bytes());
            data.extend(std::iter::repeat_n(0, *gap));
            data.extend_from_slice(section);
            at += section.len();
        }
        let object = u64::from(self.owner) | (u64::from(self.partition) << 16);
        let mut body = Vec::new();
        for field in [
            self.kind,
            self.version,
            self.flags,
            self.sections.len() as u16,
        ] {
            body.extend_from_slice(&field.to_le_bytes());
        }
        body.extend_from_slice(&object.to_le_bytes());
        body.extend_from_slice(&self.digest.to_le_bytes());
        body.extend_from_slice(&(offsets[0] as u32).to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&table);
        body.extend_from_slice(&data);
        body
    }

    fn frame(&self, count: u32) -> Vec<u8> {
        self.frame_at(FLOOR, count)
    }

    fn frame_at(&self, sequence: u64, count: u32) -> Vec<u8> {
        let id = u64::from(self.kind) | (u64::from(self.partition) << 16);
        compact_frame(self.owner, sequence, id, count, &self.body())
    }
}

fn compact_frame(owner: u16, sequence: u64, id: u64, count: u32, body: &[u8]) -> Vec<u8> {
    frame(OP_COMPACT_BLOCK, owner, sequence, id, count, body)
}

/// A capability table entry: kind, codec version, flags.
type Entry = (u16, u16, u16);

fn table_frame(entries: &[Entry]) -> Vec<u8> {
    Block::table(entries).frame(entries.len() as u32)
}

fn advance() -> Vec<u8> {
    frame(OP_ADVANCE, 0, FLOOR, 0, 0, &[])
}

/// One op-14 record-block entry: ID, revision, no expiry, extended body.
fn entry(id: u64, revision: u64, vector: [f32; 2], payload: &[u8]) -> Vec<u8> {
    let mut extended = Vec::new();
    for value in vector {
        extended.extend_from_slice(&value.to_le_bytes());
    }
    extended.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    extended.extend_from_slice(payload);
    extended.extend_from_slice(&0u32.to_le_bytes());
    let mut bytes = id.to_le_bytes().to_vec();
    bytes.extend_from_slice(&revision.to_le_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.extend_from_slice(&(extended.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&extended);
    bytes
}

fn records() -> Vec<u8> {
    records_at(FLOOR)
}

fn records_at(floor: u64) -> Vec<u8> {
    let mut body = entry(1, 2, [1.0, 0.0], b"a");
    body.extend_from_slice(&entry(2, 4, [0.0, 1.0], b"b"));
    frame(OP_RECORD_BLOCK, 0, floor, 0, 2, &body)
}

/// The image end block over `frames` (advance first): their count and the
/// SHA-256 of their 36-byte prefixes, in log order.
fn end_frame(frames: &[Vec<u8>]) -> Vec<u8> {
    let mut digest = Sha256::new();
    for frame in frames {
        digest.update(&frame[..36]);
    }
    let mut section = (frames.len() as u64).to_le_bytes().to_vec();
    section.extend_from_slice(&digest.finalize());
    let sequence = u64::from_le_bytes(frames[0][8..16].try_into().unwrap());
    Block {
        kind: 2,
        version: 1,
        flags: 1,
        owner: 0,
        partition: 0,
        digest: 0,
        sections: vec![(0, section)],
    }
    .frame_at(sequence, 1)
}

/// Recomputes both checksums of the plaintext frame at `at` after an edit.
fn reseal(bytes: &mut [u8], at: usize) {
    let checksum = crc32(&bytes[at..at + 32]);
    bytes[at + 32..at + 36].copy_from_slice(&checksum.to_le_bytes());
    let payload = u32::from_le_bytes(bytes[at + 28..at + 32].try_into().unwrap()) as usize;
    let footer = at + 36 + payload;
    if footer + 16 <= bytes.len() {
        let checksum = crc32(&bytes[at..footer]);
        bytes[footer + 4..footer + 8].copy_from_slice(&checksum.to_le_bytes());
    }
}

fn derived_blocks() -> Vec<Block> {
    let mut first = Block::derived(0x02FF, 1, 0, &[b"abcd", &[7; 8]]);
    first.sections[1].0 = 4;
    let second = Block::derived(0x02FF, 1, 1, &[b"xyz"]);
    let mut statistics = Block::derived(0x04FF, 2, 0, &[b"s"]);
    statistics.digest = 0x2222_2222_2222_2222;
    vec![first, second, statistics]
}

/// Assembles a history image from its frames.
fn image(flags: u16, frames: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = header(flags);
    for frame in frames {
        bytes.extend_from_slice(frame);
    }
    bytes
}

/// A complete compact image: `frames` (advance first), then their end block.
fn compact(frames: &[Vec<u8>]) -> Vec<u8> {
    let mut frames = frames.to_vec();
    frames.push(end_frame(&frames));
    image(FLAG_HISTORY | FLAG_COMPACT, &frames)
}

/// The golden image: two restated records plus three derived blocks of two
/// declared derived kinds (no codec in this build, so all are skipped).
fn derived_image() -> Vec<u8> {
    let mut frames = vec![
        advance(),
        table_frame(&[(0x02FF, 1, 0), (0x04FF, 2, 0)]),
        records(),
    ];
    frames.extend(derived_blocks().iter().map(|block| block.frame(1)));
    compact(&frames)
}

/// The same checkpoint without compact blocks: what a legacy image holds.
fn legacy_image() -> Vec<u8> {
    image(FLAG_HISTORY, &[advance(), records()])
}

/// A compact image declaring and holding one block of `kind`.
fn image_with(kind: u16, flags: u16) -> Vec<u8> {
    let mut block = Block::derived(kind, 1, 0, &[b"payload"]);
    block.flags = flags;
    if flags & 1 != 0 {
        block.digest = 0;
    }
    compact(&[
        advance(),
        table_frame(&[(kind, 1, flags)]),
        records(),
        block.frame(1),
    ])
}

/// The `Corrupt` reason of a failed result (panics on anything else).
fn reason<T>(result: vlite::Result<T>) -> &'static str {
    match result.map(drop) {
        Err(Error::Corrupt { reason, .. }) => reason,
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

fn assert_corrupt<T>(result: vlite::Result<T>, expected: &str) {
    let actual = reason(result);
    assert!(
        expected.is_empty() || actual == expected,
        "{actual} != {expected}"
    );
}

fn assert_unsupported<T>(result: vlite::Result<T>) {
    match result.map(drop) {
        Err(Error::UnsupportedFeature(FLAG_COMPACT)) => {}
        other => panic!("expected UnsupportedFeature(16), got {other:?}"),
    }
}

fn open(bytes: &[u8]) -> vlite::Result<Database> {
    Database::open_from_bytes(bytes.to_vec(), None)
}

fn header_flags(path: &Path) -> u16 {
    let bytes = fs::read(path).unwrap();
    u16::from_le_bytes([bytes[26], bytes[27]])
}

fn assert_golden_state(db: &Database) {
    assert_eq!(db.len(), 2);
    assert_eq!(db.get(1).unwrap().unwrap().payload, b"a");
    assert_eq!(db.get(2).unwrap().unwrap().vector, [0.0, 1.0]);
    assert_eq!(db.revision(1).unwrap(), Some(2));
    assert_eq!(db.revision(2).unwrap(), Some(4));
}

/// Every reader, in memory and on a closed file, refuses `bytes` with
/// `expected`; nothing is repaired, copied or applied.
fn assert_refused_everywhere(bytes: &[u8], floor: u64, expected: &str) {
    assert_corrupt(open(bytes), expected);
    let root = tempdir().unwrap();
    let path = root.path().join("image.vlite");
    fs::write(&path, bytes).unwrap();
    let follower_path = root.path().join("follower.vlite");
    let legacy = image(
        FLAG_HISTORY,
        &[frame(OP_ADVANCE, 0, floor, 0, 0, &[]), records_at(floor)],
    );
    fs::write(&follower_path, &legacy).unwrap();
    let mut follower = ReplicaFollower::open(&follower_path, None).unwrap();
    let cursor = FeedCursor {
        lineage: LINEAGE,
        sequence: floor,
    };
    let backup = root.path().join("backup.vlite");
    assert_corrupt(Database::open(&path), expected);
    assert_corrupt(Database::open_read_only(&path), expected);
    assert_corrupt(Database::discard_incomplete_tail(&path), expected);
    assert_corrupt(ReplicaFollower::open(&path, None), expected);
    assert_corrupt(maintenance::inspect(&path), expected);
    assert_corrupt(maintenance::backup(&path, &backup), expected);
    assert_corrupt(read_log_changes(&path, None, &cursor, 10), expected);
    assert_corrupt(
        maintenance::export_log_segment(&path, None, floor, 1 << 20),
        expected,
    );
    assert_corrupt(follower.sync_from_log(&path, 1 << 20), expected);
    assert_eq!(follower.applied_sequence(), floor);
    drop(follower);
    assert_eq!(fs::read(&follower_path).unwrap(), legacy);
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert!(!backup.exists());
}

#[test]
fn golden_image_matches_independent_encoding_and_loads_like_its_legacy_form() {
    let bytes = derived_image();
    assert_eq!(bytes, decode_hex(GOLDEN), "golden fixture drifted");
    let compact = open(&bytes).unwrap();
    let legacy = open(&legacy_image()).unwrap();
    assert_golden_state(&compact);
    assert_golden_state(&legacy);
    assert_eq!(
        compact.feed_cursor().unwrap(),
        legacy.feed_cursor().unwrap()
    );

    let root = tempdir().unwrap();
    let path = root.path().join("golden.vlite");
    fs::write(&path, &bytes).unwrap();
    assert_golden_state(&Database::open_read_only(&path).unwrap());
    let verified = maintenance::verify(&path).unwrap();
    assert_eq!(verified.inspection.valid_bytes, bytes.len() as u64);
    assert_eq!(verified.inspection.incomplete_tail_bytes, 0);
    assert_eq!(maintenance::inspect(&path).unwrap().commits, FLOOR);
    assert_eq!(fs::read(&path).unwrap(), bytes);
}

#[test]
fn compact_flag_needs_a_history_image_and_a_capability_table() {
    let golden = derived_image();
    // The flag without FLAG_HISTORY is never valid, even with no frames.
    assert_corrupt(
        open(&header(FLAG_COMPACT)),
        "compact flag without a history image",
    );
    let mut bytes = golden.clone();
    bytes[..32].copy_from_slice(&header(FLAG_COMPACT));
    assert_corrupt(open(&bytes), "compact flag without a history image");
    // Unknown flags keep their old refusal.
    assert!(matches!(
        open(&header(FLAG_HISTORY | 32)),
        Err(Error::UnsupportedFeature(32))
    ));
    // A compact header needs an advance frame, a table and an end block.
    let flags = FLAG_HISTORY | FLAG_COMPACT;
    assert_corrupt(open(&header(flags)), "incomplete compact checkpoint image");
    assert_corrupt(
        open(&image(flags, &[advance()])),
        "incomplete compact checkpoint image",
    );
    assert_corrupt(
        open(&compact(&[advance()])),
        "compact image without a capability table",
    );
    assert_corrupt(
        open(&compact(&[advance(), records()])),
        "checkpoint frame before the capability table",
    );
    // An empty table cannot carry the flag: the image holds no block.
    assert_corrupt(
        open(&compact(&[advance(), table_frame(&[]), records()])),
        "invalid capability table",
    );
    // A table whose declared kinds never appear is incomplete.
    assert_corrupt(
        open(&compact(&[
            advance(),
            table_frame(&[(0x02FF, 1, 0)]),
            records(),
        ])),
        "capability table entry is unused",
    );
}

#[test]
fn op16_is_corrupt_without_the_flag_or_outside_the_checkpoint() {
    let mut bytes = derived_image();
    bytes[..32].copy_from_slice(&header(FLAG_HISTORY));
    assert_corrupt(open(&bytes), "invalid operation or reserved record bytes");
    // Even a valid-looking block at the end of a plain v5 log.
    let mut plain = header(0);
    plain.extend_from_slice(&Block::table(&[(0x02FF, 1, 0)]).frame(1)[..]);
    assert_corrupt(open(&plain), "");

    // Op 16 must sit at the checkpoint sequence inside the restated prefix.
    let blocks = derived_blocks();
    let late = blocks[2].body();
    let flags = FLAG_HISTORY | FLAG_COMPACT;
    let early = compact_frame(0, FLOOR - 1, 0x04FF, 1, &late);
    let mut frames = vec![
        advance(),
        table_frame(&[(0x02FF, 1, 0), (0x04FF, 2, 0)]),
        records(),
        blocks[0].frame(1),
        blocks[1].frame(1),
    ];
    frames.push(early);
    assert_corrupt(
        open(&image(flags, &frames)),
        "snapshot frame outside a checkpoint image",
    );
    frames.pop();
    frames.push(compact_frame(0, FLOOR + 1, 0x04FF, 1, &late));
    assert_corrupt(
        open(&image(flags, &frames)),
        "snapshot frame outside a checkpoint image",
    );
}

#[test]
fn capability_tables_are_strict() {
    let flags = FLAG_HISTORY | FLAG_COMPACT;
    let block = Block::derived(0x02FF, 1, 0, &[b"payload"]);
    let with_table =
        |table: Vec<u8>| open(&compact(&[advance(), table, records(), block.frame(1)]));
    let tweak = |change: &dyn Fn(&mut Block)| {
        let mut table = Block::table(&[(0x02FF, 1, 0)]);
        change(&mut table);
        table.frame(1)
    };
    let invalid_table: Vec<(&str, Vec<u8>)> = vec![
        ("table flags", tweak(&|table| table.flags = 0)),
        ("table owner", tweak(&|table| table.owner = 1)),
        (
            "two sections",
            tweak(&|table| table.sections.push((0, vec![0; 8]))),
        ),
        (
            "ragged entries",
            tweak(&|table| table.sections[0].1.push(0)),
        ),
        (
            "count mismatch",
            compact_frame(0, FLOOR, 1, 2, &Block::table(&[(0x02FF, 1, 0)]).body()),
        ),
        (
            "nonzero partition",
            compact_frame(0, FLOOR, 1 | 1 << 16, 1, &{
                let mut table = Block::table(&[(0x02FF, 1, 0)]);
                table.partition = 1;
                table.body()
            }),
        ),
    ];
    for (case, table) in invalid_table {
        assert_eq!(
            reason(with_table(table)),
            "invalid capability table",
            "{case}"
        );
    }
    let invalid_entry: Vec<(&str, Vec<Entry>)> = vec![
        ("kind 0", vec![(0, 1, 0), (0x02FF, 1, 0)]),
        ("lists itself", vec![(1, 1, 1), (0x02FF, 1, 0)]),
        ("lists the end block", vec![(2, 1, 1), (0x02FF, 1, 0)]),
        ("version 0", vec![(0x02FF, 0, 0)]),
        ("unknown flag bit", vec![(0x02FF, 1, 2)]),
        ("derived marked critical", vec![(0x02FF, 1, 1)]),
        (
            "critical marked derived",
            vec![(0x0100, 1, 0), (0x02FF, 1, 0)],
        ),
        ("graph marked derived", vec![(0x02FF, 1, 0), (0x0300, 1, 0)]),
    ];
    for (case, entries) in invalid_entry {
        assert_eq!(
            reason(with_table(table_frame(&entries))),
            "invalid capability table entry",
            "{case}"
        );
    }
    let mut reserved = Block::table(&[(0x02FF, 1, 0)]);
    reserved.sections[0].1[6] = 1;
    assert_corrupt(
        with_table(reserved.frame(1)),
        "invalid capability table entry",
    );
    for entries in [
        vec![(0x04FF, 1, 0), (0x02FF, 1, 0)],
        vec![(0x02FF, 1, 0), (0x02FF, 1, 0)],
        vec![(0x02FF, 2, 0), (0x02FF, 1, 0)],
    ] {
        assert_corrupt(
            with_table(table_frame(&entries)),
            "unsorted or duplicate capability table entries",
        );
    }
    // Critical blocks, the table included, carry no source digest.
    assert_corrupt(
        with_table(tweak(&|table| table.digest = 1)),
        "invalid compact block header",
    );
    // Only table version 1 exists; 0 is malformed, later ones unsupported.
    assert_corrupt(
        with_table(tweak(&|table| table.version = 0)),
        "invalid compact block header",
    );
    assert_unsupported(with_table(tweak(&|table| table.version = 2)));
    // The table is the first op-16 frame, exactly once.
    assert_corrupt(
        open(&image(
            flags,
            &[
                advance(),
                table_frame(&[(0x02FF, 1, 0)]),
                table_frame(&[(0x02FF, 1, 0)]),
                block.frame(1),
            ],
        )),
        "duplicate capability table",
    );
    assert_corrupt(
        open(&image(flags, &[advance(), block.frame(1)])),
        "compact image without a capability table",
    );
    // Collection frames precede it; checkpoint data follows it.
    let create = {
        let mut body = 4u16.to_le_bytes().to_vec();
        body.extend_from_slice(b"docs");
        body.extend_from_slice(&2u32.to_le_bytes());
        frame(5, 0, FLOOR, 1, 0, &body)
    };
    assert_corrupt(
        open(&image(
            flags,
            &[
                advance(),
                table_frame(&[(0x02FF, 1, 0)]),
                create,
                block.frame(1),
            ],
        )),
        "collection frame after the capability table",
    );
}

#[test]
fn block_envelopes_are_checked_before_a_derived_block_is_skipped() {
    let entries = [(0x02FF, 1, 0), (0x04FF, 2, 0)];
    let load = |blocks: Vec<Vec<u8>>| {
        let mut frames = vec![advance(), table_frame(&entries), records()];
        frames.extend(blocks);
        open(&compact(&frames))
    };
    let stats = derived_blocks()[2].frame(1);
    let base = || Block::derived(0x02FF, 1, 0, &[b"abcd", b"efgh"]);
    let raw = |block: &Block, mutate: &dyn Fn(&mut Vec<u8>)| {
        let mut body = block.body();
        mutate(&mut body);
        let id = u64::from(block.kind) | (u64::from(block.partition) << 16);
        compact_frame(block.owner, FLOOR, id, 1, &body)
    };
    assert_golden_state(&load(vec![base().frame(1), stats.clone()]).unwrap());

    let header_cases: Vec<(&str, Vec<u8>)> = vec![
        ("version 0", raw(&base(), &|body| body[2..4].fill(0))),
        ("flag bit 1", raw(&base(), &|body| body[4] = 2)),
        ("no sections", raw(&base(), &|body| body[6..8].fill(0))),
        ("65 sections", raw(&base(), &|body| body[6] = 65)),
        ("object high bits", raw(&base(), &|body| body[15] = 1)),
        ("reserved", raw(&base(), &|body| body[28] = 1)),
    ];
    for (case, block) in header_cases {
        assert_eq!(
            reason(load(vec![block, stats.clone()])),
            "invalid compact block header",
            "{case}"
        );
    }
    let mismatch_cases: Vec<(&str, Vec<u8>)> = vec![
        ("kind", raw(&base(), &|body| body[0] = 0xFE)),
        ("object owner", raw(&base(), &|body| body[8] = 1)),
        ("object partition", raw(&base(), &|body| body[10] = 1)),
    ];
    for (case, block) in mismatch_cases {
        assert_eq!(
            reason(load(vec![block, stats.clone()])),
            "compact block header does not match its frame",
            "{case}"
        );
    }
    let section_cases: Vec<(&str, Vec<u8>)> = vec![
        ("first offset", raw(&base(), &|body| body[24] = 49)),
        ("overlap", raw(&base(), &|body| body[40] = 50)),
        ("past the end", raw(&base(), &|body| body[44] = 9)),
        ("trailing bytes", raw(&base(), &|body| body.push(0))),
        ("into the table", {
            let mut block = base();
            block.sections.truncate(1);
            raw(&block, &|body| {
                body[24] = 36;
                body[32] = 36;
            })
        }),
        ("nonzero padding", {
            let mut block = base();
            block.sections[1].0 = 4;
            raw(&block, &|body| body[52] = 1)
        }),
        ("padding of 64 bytes", {
            let mut block = base();
            block.sections[1].0 = 64;
            block.frame(1)
        }),
    ];
    for (case, block) in section_cases {
        assert_eq!(
            reason(load(vec![block, stats.clone()])),
            "invalid compact block sections",
            "{case}"
        );
    }
    let mut undeclared_version = base();
    undeclared_version.version = 2;
    let mut critical = base();
    critical.flags = 1;
    critical.digest = 0;
    let mut undeclared_kind = base();
    undeclared_kind.kind = 0x02FE;
    for (case, block) in [
        ("version", undeclared_version),
        ("flags", critical),
        ("kind", undeclared_kind),
    ] {
        assert_eq!(
            reason(load(vec![block.frame(1), stats.clone()])),
            "undeclared compact block",
            "{case}"
        );
    }
    // Collection-owned kinds name an existing collection.
    let mut stranger = base();
    stranger.owner = 7;
    assert_corrupt(
        load(vec![stranger.frame(1), stats.clone()]),
        "compact block for unknown collection",
    );
    // Partitions of one owner and kind run 0, 1, 2, ... without repeats.
    let partition = |number| {
        let mut block = base();
        block.partition = number;
        block.frame(1)
    };
    for blocks in [
        vec![partition(1), stats.clone()],
        vec![partition(0), partition(2), stats.clone()],
        vec![partition(0), partition(0), stats.clone()],
        vec![partition(0), stats.clone(), partition(1)],
    ] {
        assert_corrupt(load(blocks), "non-canonical compact block partition");
    }
    // Prefix bounds are checked before the body is read or allocated.
    let mut high = base().frame(1);
    high[22] = 1;
    let checksum = crc32(&high[..32]);
    high[32..36].copy_from_slice(&checksum.to_le_bytes());
    assert_corrupt(
        load(vec![high, stats.clone()]),
        "invalid compact block prefix",
    );
    for length in [39u32, (64 << 20) + 1] {
        let mut huge = base().frame(1);
        huge[28..32].copy_from_slice(&length.to_le_bytes());
        let checksum = crc32(&huge[..32]);
        huge[32..36].copy_from_slice(&checksum.to_le_bytes());
        huge.truncate(36);
        assert_corrupt(load(vec![huge]), "invalid compact block length");
    }
}

#[test]
fn unsupported_kinds_refuse_every_entry_point_before_writing() {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("P3 source", image_with(0x0100, 1)),
        ("P4 graph", image_with(0x0300, 1)),
        ("storage reserved", image_with(0x0003, 0)),
        ("unallocated", image_with(0x0500, 1)),
        ("test-only", image_with(0xF000, 0)),
    ];
    let root = tempdir().unwrap();
    // A healthy follower that is pointed at an unsupported primary log.
    let follower_path = root.path().join("follower.vlite");
    fs::write(&follower_path, legacy_image()).unwrap();
    let mut follower = ReplicaFollower::open(&follower_path, None).unwrap();
    for (case, bytes) in cases {
        let dir = root.path().join(case.replace(' ', "-"));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("source.vlite");
        fs::write(&path, &bytes).unwrap();
        let out = |name: &str| dir.join(name);
        assert_unsupported(open(&bytes));
        assert_unsupported(Database::open(&path));
        assert_unsupported(Database::open_read_only(&path));
        assert_unsupported(Database::discard_incomplete_tail(&path));
        assert_unsupported(ReplicaFollower::open(&path, None));
        assert_unsupported(maintenance::inspect(&path));
        assert_unsupported(maintenance::verify(&path));
        assert_unsupported(maintenance::backup(&path, out("backup")));
        assert_unsupported(maintenance::restore(&path, out("restore")));
        assert_unsupported(maintenance::compact(&path, out("compact")));
        assert_unsupported(maintenance::upgrade(&path, out("upgrade")));
        assert_unsupported(maintenance::convert_to_directory(&path, out("dir")));
        assert_unsupported(maintenance::drill(&path, out("drill")));
        assert_unsupported(enable_graph(&path, out("graph"), None));
        assert_unsupported(strip_graph(&path, out("stripped"), None, true));
        assert_unsupported(maintenance::inspect_detail(&path));
        let key = EncryptionKey::from_bytes([3; 32]);
        assert_unsupported(maintenance::reencrypt(&path, None, out("enc"), Some(&key)));
        assert_unsupported(maintenance::restore_to(
            None,
            &path,
            out("pitr"),
            RestoreTarget::Sequence(FLOOR),
            None,
        ));
        let cursor = FeedCursor {
            lineage: LINEAGE,
            sequence: FLOOR,
        };
        assert_unsupported(read_log_changes(&path, None, &cursor, 10));
        assert_unsupported(maintenance::export_log_segment(&path, None, FLOOR, 1 << 20));
        assert_unsupported(follower.sync_from_log(&path, 1 << 20));
        assert_eq!(fs::read(&path).unwrap(), bytes, "{case}");
        let mut names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, ["source.vlite"], "{case}: nothing may be written");
    }
    assert_eq!(follower.applied_sequence(), FLOOR);
    drop(follower);
    assert_eq!(fs::read(&follower_path).unwrap(), legacy_image());
    // Structure is checked before support: a malformed table with a
    // critical entry is corrupt, not unsupported.
    let malformed = compact(&[
        advance(),
        table_frame(&[(0x0300, 1, 1), (0x0100, 1, 1)]),
        records(),
    ]);
    assert_corrupt(
        open(&malformed),
        "unsorted or duplicate capability table entries",
    );
}

#[test]
fn torn_or_damaged_compact_images_are_never_repaired() {
    const INCOMPLETE: &str = "incomplete compact checkpoint image";
    let golden = derived_image();
    for length in 32..golden.len() {
        assert_corrupt(open(&golden[..length]), INCOMPLETE);
    }
    for at in 0..golden.len() {
        let mut bytes = golden.clone();
        bytes[at] ^= 0x10;
        reason(open(&bytes));
    }
    // The file-backed repair paths refuse the same damage and keep the bytes.
    let root = tempdir().unwrap();
    let path = root.path().join("torn.vlite");
    for cut in [33, 100, 140, 300, golden.len() - 20, golden.len() - 1] {
        let torn = &golden[..cut];
        fs::write(&path, torn).unwrap();
        assert_corrupt(Database::open(&path), INCOMPLETE);
        assert_corrupt(Database::discard_incomplete_tail(&path), INCOMPLETE);
        assert_corrupt(ReplicaFollower::open(&path, None), INCOMPLETE);
        assert_eq!(fs::read(&path).unwrap(), torn, "cut at {cut}");
    }

    // Even after every table entry was used, a cut anywhere before the end
    // of the end block (frame boundaries included) is corruption.
    let blocks = derived_blocks();
    let last = blocks[1].frame(1);
    let frames = [
        advance(),
        table_frame(&[(0x02FF, 1, 0)]),
        records(),
        blocks[0].frame(1),
        last.clone(),
    ];
    let run = compact(&frames);
    assert_golden_state(&open(&run).unwrap());
    let start = run.len() - end_frame(&frames).len() - last.len();
    for cut in start..run.len() {
        assert_corrupt(open(&run[..cut]), INCOMPLETE);
    }
    let cursor = FeedCursor {
        lineage: LINEAGE,
        sequence: FLOOR,
    };
    for cut in [
        start,
        start + 10,
        start + 40,
        start + last.len(),
        run.len() - 1,
    ] {
        fs::write(&path, &run[..cut]).unwrap();
        assert_corrupt(read_log_changes(&path, None, &cursor, 10), INCOMPLETE);
        assert_corrupt(
            maintenance::export_log_segment(&path, None, FLOOR, 1 << 20),
            INCOMPLETE,
        );
    }

    // After a complete image, any torn log frame is an incomplete tail,
    // however short, and repair restores exactly the image.
    let mut primary = Database::open_from_bytes(golden.clone(), None).unwrap();
    primary.put(3, &[3.0, 3.0], b"c").unwrap();
    let full = primary.to_bytes().unwrap();
    assert!(full.len() > golden.len() + 36);
    for cut in [
        golden.len() + 1,
        golden.len() + 15,
        golden.len() + 16,
        golden.len() + 40,
        full.len() - 1,
    ] {
        let torn = &full[..cut];
        fs::write(&path, torn).unwrap();
        assert!(
            matches!(Database::open(&path), Err(Error::IncompleteTail { .. })),
            "cut at {cut}"
        );
        let batch = read_log_changes(&path, None, &cursor, 10).unwrap();
        assert!(batch.changes.is_empty(), "cut at {cut}");
        let report = Database::discard_incomplete_tail(&path).unwrap();
        assert_eq!(report.discarded_bytes, (cut - golden.len()) as u64);
        assert_eq!(fs::read(&path).unwrap(), golden);
        assert_golden_state(&Database::open(&path).unwrap());
    }
}

/// Walks the frames of an encrypted file, yielding (offset, length, op).
fn sealed_frames(bytes: &[u8]) -> Vec<(usize, usize, u8)> {
    let mut frames = Vec::new();
    let mut at = 32 + 64;
    while at < bytes.len() {
        let payload = u32::from_le_bytes(bytes[at + 28..at + 32].try_into().unwrap()) as usize;
        let length = 36 + payload + 28 + 16;
        frames.push((at, length, bytes[at + 4]));
        at += length;
    }
    frames
}

#[test]
fn encrypted_compact_images_authenticate_every_block() {
    let root = tempdir().unwrap();
    let plain = root.path().join("plain.vlite");
    fs::write(&plain, derived_image()).unwrap();
    let key = EncryptionKey::from_bytes([9; 32]);
    let sealed = root.path().join("sealed.vlite");
    maintenance::reencrypt(&plain, None, &sealed, Some(&key)).unwrap();
    assert_eq!(
        header_flags(&sealed),
        FLAG_ENCRYPTED | FLAG_HISTORY | FLAG_COMPACT
    );
    assert_golden_state(&Database::open_encrypted(&sealed, &key).unwrap());
    let bytes = fs::read(&sealed).unwrap();
    let frames = sealed_frames(&bytes);
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame.2 == OP_COMPACT_BLOCK)
            .count(),
        5
    );

    // A ciphertext change with recomputed checksums fails authentication.
    for &(at, length, operation) in &frames {
        if operation != OP_COMPACT_BLOCK {
            continue;
        }
        let mut damaged = bytes.clone();
        damaged[at + 40] ^= 1;
        let checksum = crc32(&damaged[at..at + length - 16]);
        damaged[at + length - 12..at + length - 8].copy_from_slice(&checksum.to_le_bytes());
        assert_corrupt(
            Database::open_from_bytes(damaged, Some(&key)),
            "frame authentication failed",
        );
    }
    // Truncation anywhere in the image is corruption, never a torn tail.
    for cut in 32 + 64..bytes.len() {
        assert_corrupt(
            Database::open_from_bytes(bytes[..cut].to_vec(), Some(&key)),
            "incomplete compact checkpoint image",
        );
    }
    // And back to plaintext: the frames are carried unchanged.
    let clear = root.path().join("clear.vlite");
    maintenance::reencrypt(&sealed, Some(&key), &clear, None).unwrap();
    assert_eq!(fs::read(&clear).unwrap()[32..], derived_image()[32..]);
    assert_eq!(header_flags(&clear), FLAG_HISTORY | FLAG_COMPACT);
}

#[test]
fn derived_only_images_survive_every_copy_rewrite_and_feed_path() {
    let root = tempdir().unwrap();
    let path = root.path().join("primary.vlite");
    fs::write(&path, derived_image()).unwrap();
    let follower_path = root.path().join("follower.vlite");
    fs::copy(&path, &follower_path).unwrap();

    // Writable open appends ordinary log frames after the image.
    let mut primary = Database::open(&path).unwrap();
    primary.put(3, &[3.0, 3.0], b"c").unwrap();
    let cursor = FeedCursor {
        lineage: LINEAGE,
        sequence: FLOOR,
    };
    let batch = primary.changes_after(&cursor, 10).unwrap();
    assert_eq!(batch.changes.len(), 1);
    assert!(matches!(
        batch.changes[0].kind,
        ChangeKind::Put { id: 3, .. }
    ));
    let segment = primary.export_log(FLOOR, 1 << 20).unwrap();
    assert_eq!(segment.last_sequence, FLOOR + 1);
    primary.close().unwrap();
    assert_eq!(header_flags(&path), FLAG_HISTORY | FLAG_COMPACT);
    let reopened = Database::open(&path).unwrap();
    assert_eq!(reopened.len(), 3);
    drop(reopened);

    // Closed-file feed and segment export skip restated blocks.
    let batch = read_log_changes(&path, None, &cursor, 10).unwrap();
    assert_eq!(batch.changes.len(), 1);
    assert_eq!(batch.changes[0].sequence, FLOOR + 1);
    assert!(matches!(
        read_log_changes(&path, None, &FeedCursor::beginning(), 10),
        Err(Error::CursorBeforeHistory { .. })
    ));
    let exported = maintenance::export_log_segment(&path, None, FLOOR, 1 << 20).unwrap();
    assert_eq!(exported.frames, segment.frames);

    // A follower bootstrapped from a byte copy applies later segments.
    let mut follower = ReplicaFollower::open(&follower_path, None).unwrap();
    assert_eq!(follower.applied_sequence(), FLOOR);
    follower.apply(&segment).unwrap();
    assert_eq!(follower.db().len(), 3);
    drop(follower);

    // Byte copies keep the flag and the blocks.
    let backup = root.path().join("backup.vlite");
    maintenance::backup(&path, &backup).unwrap();
    assert_eq!(fs::read(&backup).unwrap(), fs::read(&path).unwrap());
    let restored = root.path().join("restored.vlite");
    maintenance::restore(&backup, &restored).unwrap();
    assert_eq!(header_flags(&restored), FLAG_HISTORY | FLAG_COMPACT);
    assert_eq!(Database::open(&restored).unwrap().len(), 3);
    let drill = root.path().join("drill");
    fs::create_dir(&drill).unwrap();
    maintenance::drill(&backup, &drill).unwrap();
    for (target, len) in [(FLOOR, 2), (FLOOR + 1, 3)] {
        let pitr = root.path().join(format!("pitr-{target}.vlite"));
        maintenance::restore_to(None, &path, &pitr, RestoreTarget::Sequence(target), None).unwrap();
        assert_eq!(header_flags(&pitr), FLAG_HISTORY | FLAG_COMPACT);
        assert_eq!(Database::open(&pitr).unwrap().len(), len);
    }

    // Rewrites from loaded state keep every value; derived blocks are
    // rebuildable, so their legacy output drops them and the flag.
    let compacted = root.path().join("compacted.vlite");
    maintenance::compact(&path, &compacted).unwrap();
    let upgraded = root.path().join("upgraded.vlite");
    maintenance::upgrade(&path, &upgraded).unwrap();
    let graph = root.path().join("graph.vlite");
    enable_graph(&path, &graph, None).unwrap();
    for output in [&compacted, &upgraded, &graph] {
        assert_eq!(header_flags(output) & (FLAG_HISTORY | FLAG_COMPACT), 0);
        let db = Database::open(output).unwrap();
        assert_eq!(db.len(), 3);
        assert_eq!(db.get(1).unwrap().unwrap().payload, b"a");
    }
    let dir = root.path().join("store");
    maintenance::convert_to_directory(&path, &dir).unwrap();
    let db = Database::open(&dir).unwrap();
    assert_eq!(db.len(), 3);
    assert_eq!(db.revision(3).unwrap(), Some(FLOOR + 1));
}

#[test]
fn directory_checkpoints_rewrite_a_compact_image_faithfully() {
    let root = tempdir().unwrap();
    let file = root.path().join("legacy.vlite");
    fs::write(&file, legacy_image()).unwrap();
    let dir = root.path().join("store");
    maintenance::convert_to_directory(&file, &dir).unwrap();
    // Swap in the compact form of the same checkpoint (same lineage, floor
    // and WAL header); the manifest lists file names only.
    let image = dir.join("image-1.vlite");
    assert!(image.exists());
    fs::write(&image, derived_image()).unwrap();

    assert_golden_state(&Database::open_read_only(&dir).unwrap());
    // Packing the store into one file keeps every value.
    let packed = root.path().join("packed-derived.vlite");
    maintenance::backup(&dir, &packed).unwrap();
    assert_eq!(fs::read(&packed).unwrap(), derived_image());
    assert_golden_state(&Database::open(&packed).unwrap());
    let mut db = Database::open(&dir).unwrap();
    assert_golden_state(&db);
    db.put(3, &[3.0, 3.0], b"c").unwrap();
    // The feed and segment export read through the image's restated blocks.
    let cursor = FeedCursor {
        lineage: LINEAGE,
        sequence: FLOOR,
    };
    let batch = db.changes_after(&cursor, 10).unwrap();
    assert_eq!(batch.changes.len(), 1);
    assert_eq!(batch.changes[0].sequence, FLOOR + 1);
    assert_eq!(
        db.export_log(FLOOR, 1 << 20).unwrap().last_sequence,
        FLOOR + 1
    );
    db.checkpoint().unwrap();
    db.close().unwrap();
    assert!(!image.exists());
    let db = Database::open(&dir).unwrap();
    assert_eq!(db.len(), 3);
    assert_eq!(db.revision(2).unwrap(), Some(4));
    drop(db);
    let images: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("image-")
        })
        .collect();
    assert_eq!(images.len(), 1);
    assert_eq!(header_flags(&images[0]) & FLAG_COMPACT, 0);

    // An image this build cannot serve refuses the whole store.
    let dir = root.path().join("critical");
    maintenance::convert_to_directory(&file, &dir).unwrap();
    let image = dir.join("image-1.vlite");
    fs::write(&image, image_with(0x0100, 1)).unwrap();
    assert_unsupported(Database::open(&dir));
    assert_unsupported(Database::open_read_only(&dir));
    assert_unsupported(maintenance::backup(&dir, root.path().join("packed")));
    assert!(!root.path().join("packed").exists());
    assert_eq!(fs::read(&image).unwrap(), image_with(0x0100, 1));
}

/// HOLD 1 on e27a4e0: at checkpoint S6, `docs` is created before the table
/// and its only source frame is the last frame of the image.
const S6: u64 = 6;

fn s6_frames() -> Vec<Vec<u8>> {
    let mut create = vec![4];
    create.extend_from_slice(b"docs");
    create.extend_from_slice(&2u32.to_le_bytes());
    // Collection settings v1: L2, HNSW 16/100/42, no quantization, rerank 4,
    // planner 4096/20000, no metadata indexes.
    create.extend_from_slice(&1u16.to_le_bytes());
    create.push(0);
    create.extend_from_slice(&16u16.to_le_bytes());
    create.extend_from_slice(&100u16.to_le_bytes());
    create.extend_from_slice(&42u64.to_le_bytes());
    create.extend_from_slice(&[0, 4]);
    create.extend_from_slice(&4096u32.to_le_bytes());
    create.extend_from_slice(&20_000u32.to_le_bytes());
    create.extend_from_slice(&[0, 0]);
    let mut docs = entry(1, 5, [1.0, 1.0], b"c");
    docs.extend_from_slice(&entry(2, 6, [0.5, 0.5], b"d"));
    vec![
        frame(OP_ADVANCE, 0, S6, 0, 0, &[]),
        frame(5, 0, S6, 1, 0, &create),
        Block::table(&[(0x02FF, 1, 0)]).frame_at(S6, 1),
        records_at(S6),
        Block::derived(0x02FF, 1, 0, &[b"abcd"]).frame_at(S6, 1),
        frame(OP_RECORD_BLOCK, 1, S6, 0, 2, &docs),
    ]
}

#[test]
fn a_compact_image_cut_at_any_frame_boundary_is_corrupt() {
    let mut frames = s6_frames();
    let source = frames[frames.len() - 1].len();
    frames.push(end_frame(&frames));
    let full = image(FLAG_HISTORY | FLAG_COMPACT, &frames);
    // The reviewer's cut: at the start of the last source frame, after
    // every table entry was used.
    let cut = full.len() - frames[frames.len() - 1].len() - source;
    assert_refused_everywhere(&full[..cut], S6, "incomplete compact checkpoint image");
    // Every other frame boundary before the end block, too.
    let mut boundary = 32;
    for frame in &frames {
        assert_corrupt(
            open(&full[..boundary]),
            "incomplete compact checkpoint image",
        );
        boundary += frame.len();
    }
    assert_eq!(boundary, full.len());
    let db = open(&full).unwrap();
    assert_eq!(db.len(), 2);
    assert_eq!(db.collection("docs").unwrap().len(), 2);
    drop(db);
    // The same for the sealed image: re-encryption keeps every prefix, so
    // the end block still matches.
    let root = tempdir().unwrap();
    let plain = root.path().join("plain.vlite");
    fs::write(&plain, &full).unwrap();
    let sealed = root.path().join("sealed.vlite");
    let key = EncryptionKey::from_bytes([5; 32]);
    maintenance::reencrypt(&plain, None, &sealed, Some(&key)).unwrap();
    let bytes = fs::read(&sealed).unwrap();
    let open_sealed = |bytes: &[u8]| Database::open_from_bytes(bytes.to_vec(), Some(&key));
    assert_eq!(
        open_sealed(&bytes)
            .unwrap()
            .collection("docs")
            .unwrap()
            .len(),
        2
    );
    for (at, _, _) in sealed_frames(&bytes) {
        assert_corrupt(
            open_sealed(&bytes[..at]),
            "incomplete compact checkpoint image",
        );
    }
    let docs = sealed_frames(&bytes)[5].0;
    fs::write(&sealed, &bytes[..docs]).unwrap();
    assert_corrupt(
        Database::open_encrypted(&sealed, &key),
        "incomplete compact checkpoint image",
    );
    assert_corrupt(
        Database::discard_incomplete_tail_encrypted(&sealed, &key),
        "incomplete compact checkpoint image",
    );
    assert_eq!(fs::read(&sealed).unwrap(), &bytes[..docs]);
}

#[test]
fn image_end_blocks_are_strict() {
    let frames = vec![
        advance(),
        table_frame(&[(0x02FF, 1, 0)]),
        records(),
        Block::derived(0x02FF, 1, 0, &[b"abcd"]).frame(1),
    ];
    assert_golden_state(&open(&compact(&frames)).unwrap());
    let with_end = |end: Vec<u8>| {
        let mut all = frames.clone();
        all.push(end);
        open(&image(FLAG_HISTORY | FLAG_COMPACT, &all))
    };
    let end = || {
        let mut section = (frames.len() as u64).to_le_bytes().to_vec();
        let mut digest = Sha256::new();
        for frame in &frames {
            digest.update(&frame[..36]);
        }
        section.extend_from_slice(&digest.finalize());
        Block {
            kind: 2,
            version: 1,
            flags: 1,
            owner: 0,
            partition: 0,
            digest: 0,
            sections: vec![(0, section)],
        }
    };
    assert_eq!(end().frame(1), end_frame(&frames));
    const MISMATCH: &str = "compact image end does not match the image";
    let tweak = |change: &dyn Fn(&mut Block)| {
        let mut block = end();
        change(&mut block);
        block.frame(1)
    };
    assert_corrupt(with_end(tweak(&|end| end.sections[0].1[0] += 1)), MISMATCH);
    assert_corrupt(with_end(tweak(&|end| end.sections[0].1[39] ^= 1)), MISMATCH);
    for (case, end) in [
        ("derived", tweak(&|end| end.flags = 0)),
        (
            "short",
            tweak(&|end| {
                end.sections[0].1.pop();
            }),
        ),
        ("long", tweak(&|end| end.sections[0].1.push(0))),
        (
            "two sections",
            tweak(&|end| end.sections.push((0, vec![0]))),
        ),
        ("padded", tweak(&|end| end.sections[0].0 = 8)),
        ("count", end().frame(2)),
        ("owner", {
            let mut block = end();
            block.owner = 1;
            block.frame(1)
        }),
        ("partition", {
            let mut block = end();
            block.partition = 1;
            block.frame(1)
        }),
    ] {
        assert_eq!(reason(with_end(end)), "invalid compact image end", "{case}");
    }
    assert_corrupt(
        with_end(tweak(&|end| end.digest = 1)),
        "invalid compact block header",
    );
    // A later end block version is unsupported, once its envelope checks.
    assert_unsupported(with_end(tweak(&|end| end.version = 2)));
    // Every restated frame counts, in order: dropping a legacy record block
    // or swapping two frames no longer matches.
    let mut dropped = frames.clone();
    dropped.remove(2);
    let mut swapped = frames.clone();
    swapped.swap(2, 3);
    for mut altered in [dropped, swapped] {
        altered.push(end_frame(&frames));
        assert_corrupt(
            open(&image(FLAG_HISTORY | FLAG_COMPACT, &altered)),
            MISMATCH,
        );
    }
    // The end block follows the table and closes the image exactly once.
    assert_corrupt(
        open(&image(
            FLAG_HISTORY | FLAG_COMPACT,
            &[advance(), end_frame(&[advance()])],
        )),
        "compact image without a capability table",
    );
    let mut twice = compact(&frames);
    twice.extend_from_slice(&end_frame(&frames));
    assert_corrupt(
        open(&twice),
        "restated frame after the compact checkpoint image",
    );
    let mut restated = compact(&frames);
    restated.extend_from_slice(&records());
    assert_corrupt(
        open(&restated),
        "restated frame after the compact checkpoint image",
    );
    // A later commit before the end block leaves the image incomplete.
    let mut early = image(FLAG_HISTORY | FLAG_COMPACT, &frames);
    early.extend_from_slice(&frame(2, 0, FLOOR + 1, 1, 0, &[]));
    assert_refused_everywhere(&early, FLOOR, "incomplete compact checkpoint image");
}

#[test]
fn compact_prefixes_are_checked_by_every_reader_before_a_body_is_read() {
    let golden = derived_image();
    let table = table_frame(&[(0x02FF, 1, 0), (0x04FF, 2, 0)]);
    let derived_at = 32 + advance().len() + table.len() + records().len();
    assert_eq!(derived_at, 336);
    // A reserved prefix byte, with both checksums recomputed.
    let mut reserved = golden.clone();
    reserved[derived_at + 7] = 1;
    reseal(&mut reserved, derived_at);
    assert_refused_everywhere(
        &reserved,
        FLOOR,
        "invalid operation or reserved record bytes",
    );
    // An advance frame with a nonzero ID.
    let mut advance_id = golden.clone();
    advance_id[32 + 16] = 1;
    reseal(&mut advance_id, 32);
    assert_refused_everywhere(&advance_id, FLOOR, "invalid advance frame");
    // A time anchor inside the image: history images never carry them.
    let mut anchored = golden[..derived_at].to_vec();
    anchored.extend_from_slice(&frame(11, 0, FLOOR, 0, 0, &7u64.to_le_bytes()));
    anchored.extend_from_slice(&golden[derived_at..]);
    assert_refused_everywhere(
        &anchored,
        FLOOR,
        "invalid operation or reserved record bytes",
    );
    // An oversized block is refused from its prefix alone: the body is never
    // read or allocated (here it is not even present).
    for length in [(64u32 << 20) + 1, 255 << 20] {
        let mut huge = golden[..derived_at + 36].to_vec();
        huge[derived_at + 28..derived_at + 32].copy_from_slice(&length.to_le_bytes());
        reseal(&mut huge, derived_at);
        assert_refused_everywhere(&huge, FLOOR, "invalid compact block length");
    }
}

/// A test logger that keeps the `vlite` records of the calling thread only,
/// so tests running in parallel never see each other's records. It is
/// installed once for this test binary, where nothing else installs one.
mod capture {
    use log::{Level, LevelFilter, Log, Metadata, Record};
    use std::cell::RefCell;
    use std::sync::Once;

    pub type Logged = Vec<(Level, String, String)>;

    thread_local! {
        static RECORDS: RefCell<Option<Logged>> = const { RefCell::new(None) };
    }

    struct Capture;

    impl Log for Capture {
        fn enabled(&self, metadata: &Metadata) -> bool {
            metadata.target().starts_with("vlite")
        }

        fn log(&self, record: &Record) {
            if self.enabled(record.metadata()) {
                RECORDS.with(|records| {
                    if let Some(records) = records.borrow_mut().as_mut() {
                        records.push((
                            record.level(),
                            record.target().to_owned(),
                            record.args().to_string(),
                        ));
                    }
                });
            }
        }

        fn flush(&self) {}
    }

    /// Runs `body` and returns it with what it logged on this thread.
    pub fn logged<T>(body: impl FnOnce() -> T) -> (T, Logged) {
        static INSTALL: Once = Once::new();
        static LOGGER: Capture = Capture;
        INSTALL.call_once(|| {
            log::set_logger(&LOGGER).expect("the only logger in this test binary");
            log::set_max_level(LevelFilter::Trace);
        });
        RECORDS.with(|records| *records.borrow_mut() = Some(Vec::new()));
        let value = body();
        let records = RECORDS.with(|records| records.borrow_mut().take());
        (value, records.unwrap_or_default())
    }
}

/// The groups of the golden image's skip report.
const GOLDEN_GROUPS: &str = r#""blocks":3,"groups":[{"collection":0,"kind":767,"version":1,"blocks":2},{"collection":0,"kind":1279,"version":2,"blocks":1}],"more_groups":0"#;

/// The one warning a reader logs for the golden image.
fn golden_report(reader: &str) -> capture::Logged {
    vec![(
        log::Level::Warn,
        "vlite::compact".to_owned(),
        format!(
            r#"{{"event":"compact_block_skip","outcome":"unknown_kind","reader":"{reader}","rebuild":"none",{GOLDEN_GROUPS}}}"#
        ),
    )]
}

/// HOLD 3 on e27a4e0, embedded and offline: a reader that validated a
/// complete image reports its skipped derived blocks once, through the `log`
/// facade to the host's logger, without the server and without claiming a
/// rebuild. Refused images report nothing, and re-reads of a log whose
/// image a handle or operation already reported do not repeat it.
#[test]
fn skipped_derived_blocks_are_reported_to_the_host_logger() {
    use capture::logged;
    let (db, records) = logged(|| open(&legacy_image()));
    assert_golden_state(&db.unwrap());
    assert!(records.is_empty(), "{records:?}");
    let (db, records) = logged(|| open(&derived_image()));
    assert_golden_state(&db.unwrap());
    assert_eq!(records, golden_report("load"));

    // Closed-file readers: open, inspection, feed, export, copies.
    let root = tempdir().unwrap();
    let path = root.path().join("image.vlite");
    fs::write(&path, derived_image()).unwrap();
    let cursor = FeedCursor {
        lineage: LINEAGE,
        sequence: FLOOR,
    };
    let backup = root.path().join("backup.vlite");
    let sealed = root.path().join("sealed.vlite");
    let key = EncryptionKey::from_bytes([5; 32]);
    let readers: [(&str, &dyn Fn() -> vlite::Result<()>); 8] = [
        ("load", &|| Database::open(&path).map(drop)),
        ("load", &|| Database::open_read_only(&path).map(drop)),
        ("load", &|| maintenance::inspect(&path).map(drop)),
        ("load", &|| {
            Database::discard_incomplete_tail(&path).map(drop)
        }),
        ("change_feed", &|| {
            read_log_changes(&path, None, &cursor, 10).map(drop)
        }),
        ("log_export", &|| {
            maintenance::export_log_segment(&path, None, FLOOR, 1 << 20).map(drop)
        }),
        // A copy is validated again before it is published; that check
        // does not repeat the source's report.
        ("load", &|| maintenance::backup(&path, &backup).map(drop)),
        ("load", &|| {
            maintenance::reencrypt(&path, None, &sealed, Some(&key)).map(drop)
        }),
    ];
    for (reader, read) in readers {
        let (result, records) = logged(read);
        result.unwrap();
        assert_eq!(records, golden_report(reader));
    }
    let (db, records) = logged(|| Database::open_encrypted(&sealed, &key));
    assert_golden_state(&db.unwrap());
    assert_eq!(records, golden_report("load"));

    // A handle's own feed and export re-read the image its open reported.
    let (db, records) = logged(|| Database::open(&path));
    let mut db = db.unwrap();
    assert_eq!(records, golden_report("load"));
    let (result, records) = logged(|| {
        db.put(3, &[3.0, 3.0], b"c")?;
        db.changes_after(&cursor, 10)?;
        db.export_log(FLOOR, 1 << 20).map(drop)
    });
    result.unwrap();
    assert!(records.is_empty(), "{records:?}");
    db.close().unwrap();

    // A follower reports its own open and the primary log it reads, not the
    // replay of its log after appending a segment.
    let follower_path = root.path().join("follower.vlite");
    fs::write(&follower_path, derived_image()).unwrap();
    let (follower, records) = logged(|| ReplicaFollower::open(&follower_path, None));
    let mut follower = follower.unwrap();
    assert_eq!(records, golden_report("load"));
    let (status, records) = logged(|| follower.sync_from_log(&path, 1 << 20));
    assert_eq!(status.unwrap().applied_sequence, FLOOR + 1);
    assert_eq!(records, golden_report("log_export"));
    drop(follower);

    // A complete image followed by a torn WAL tail is still a complete
    // image: the loader reports it, and the open refuses the tail.
    let mut torn = derived_image();
    torn.extend_from_slice(&frame(2, 0, FLOOR + 1, 1, 0, &[])[..20]);
    let (result, records) = logged(|| open(&torn));
    assert!(matches!(result, Err(Error::IncompleteTail { .. })));
    assert_eq!(records, golden_report("load"));

    // One bounded record however many kinds were skipped.
    let kinds: Vec<u16> = (0x0200..0x0214).collect();
    let entries: Vec<Entry> = kinds.iter().map(|&kind| (kind, 1, 0)).collect();
    let mut frames = vec![advance(), table_frame(&entries), records_at(FLOOR)];
    frames.extend(
        kinds
            .iter()
            .map(|&kind| Block::derived(kind, 1, 0, &[b"x"]).frame(1)),
    );
    let (db, records) = logged(|| open(&compact(&frames)));
    assert_golden_state(&db.unwrap());
    let groups: Vec<String> = kinds[..16]
        .iter()
        .map(|kind| format!(r#"{{"collection":0,"kind":{kind},"version":1,"blocks":1}}"#))
        .collect();
    assert_eq!(
        records,
        [(
            log::Level::Warn,
            "vlite::compact".to_owned(),
            format!(
                r#"{{"event":"compact_block_skip","outcome":"unknown_kind","reader":"load","rebuild":"none","blocks":20,"groups":[{}],"more_groups":4}}"#,
                groups.join(",")
            ),
        )]
    );

    // Refused images report nothing from any reader.
    let mut s6 = s6_frames();
    s6.push(end_frame(&s6));
    let full = image(FLAG_HISTORY | FLAG_COMPACT, &s6);
    let cut = full.len() - s6[6].len() - s6[5].len();
    let mut reserved = derived_image();
    reserved[336 + 7] = 1;
    reseal(&mut reserved, 336);
    for (bytes, floor, expected) in [
        (&full[..cut], S6, "incomplete compact checkpoint image"),
        (
            &reserved[..],
            FLOOR,
            "invalid operation or reserved record bytes",
        ),
    ] {
        let ((), records) = logged(|| assert_refused_everywhere(bytes, floor, expected));
        assert!(records.is_empty(), "{records:?}");
    }
    let critical = image_with(0x0100, 1);
    let critical_path = root.path().join("critical.vlite");
    fs::write(&critical_path, &critical).unwrap();
    let ((), records) = logged(|| {
        assert_unsupported(open(&critical));
        assert_unsupported(read_log_changes(&critical_path, None, &cursor, 10));
        assert_unsupported(maintenance::export_log_segment(
            &critical_path,
            None,
            FLOOR,
            1 << 20,
        ));
    });
    assert!(records.is_empty(), "{records:?}");
    // A complete image whose restated records fail to decode after the
    // image check is refused without a report.
    let mut unsorted = entry(2, 4, [0.0, 1.0], b"b");
    unsorted.extend_from_slice(&entry(1, 2, [1.0, 0.0], b"a"));
    let mut frames = vec![
        advance(),
        table_frame(&[(0x02FF, 1, 0), (0x04FF, 2, 0)]),
        frame(OP_RECORD_BLOCK, 0, FLOOR, 0, 2, &unsorted),
    ];
    frames.extend(derived_blocks().iter().map(|block| block.frame(1)));
    let (result, records) = logged(|| open(&compact(&frames)));
    assert_corrupt(result, "invalid record block");
    assert!(records.is_empty(), "{records:?}");
}

/// Skipped derived blocks are reported in the server's structured log
/// (stderr and `/v1/admin/logs`), which states that nothing was rebuilt.
#[cfg(feature = "server")]
#[tokio::test]
async fn skipped_derived_blocks_are_reported_without_claiming_a_rebuild() {
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tower::ServiceExt;
    use vlite::server::{BearerToken, ServerOptions, Service, configured_admin_router};

    const ADMIN: &str = "compact_admin_secret_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    async fn reports(db: Database) -> Vec<(Value, Value)> {
        let options = ServerOptions {
            admin_token: Some(BearerToken::new(ADMIN).unwrap()),
            ..ServerOptions::default()
        };
        let app = configured_admin_router(Service::new(db), options).unwrap();
        let request = axum::http::Request::builder()
            .uri("/v1/admin/logs?q=compact_block_skip&limit=1000")
            .header("authorization", format!("Bearer {ADMIN}"))
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let page: Value = serde_json::from_slice(&bytes).unwrap();
        page["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| (entry["level"].clone(), entry["fields"].clone()))
            .collect()
    }
    // A legacy image skips nothing.
    assert!(reports(open(&legacy_image()).unwrap()).await.is_empty());
    // The same single bounded record the library logs, kept once.
    let expected: Value = serde_json::from_str(&golden_report("load")[0].2).unwrap();
    assert_eq!(expected["outcome"], "unknown_kind");
    assert_eq!(
        reports(open(&derived_image()).unwrap()).await,
        [(json!("warn"), expected)]
    );
}
