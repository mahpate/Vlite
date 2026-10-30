//! Compact checkpoint images, slice 0 of
//! `docs/projects/roadmap/compact-storage-contract.md`.
//!
//! `FLAG_COMPACT` (16) and op-16 blocks are accepted only behind a complete
//! capability table and block envelope check. This build has no compact
//! codec: a critical (or undefined) kind refuses the whole image with
//! `UnsupportedFeature(16)` before anything is written, while an authentic,
//! declared derived block is skipped. Images are encoded independently here
//! so the tests pin the on-disk contract, not the implementation.

use std::fs;
use std::path::Path;

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
        let id = u64::from(self.kind) | (u64::from(self.partition) << 16);
        compact_frame(self.owner, FLOOR, id, count, &self.body())
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
    let mut body = entry(1, 2, [1.0, 0.0], b"a");
    body.extend_from_slice(&entry(2, 4, [0.0, 1.0], b"b"));
    frame(OP_RECORD_BLOCK, 0, FLOOR, 0, 2, &body)
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

/// The golden image: two restated records plus three derived blocks of two
/// declared derived kinds (no codec in this build, so all are skipped).
fn derived_image() -> Vec<u8> {
    let mut frames = vec![
        advance(),
        table_frame(&[(0x02FF, 1, 0), (0x04FF, 2, 0)]),
        records(),
    ];
    frames.extend(derived_blocks().iter().map(|block| block.frame(1)));
    image(FLAG_HISTORY | FLAG_COMPACT, &frames)
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
    image(
        FLAG_HISTORY | FLAG_COMPACT,
        &[
            advance(),
            table_frame(&[(kind, 1, flags)]),
            records(),
            block.frame(1),
        ],
    )
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
    // A compact header needs an advance frame and a table.
    let flags = FLAG_HISTORY | FLAG_COMPACT;
    assert_corrupt(
        open(&header(flags)),
        "compact image without a capability table",
    );
    assert_corrupt(
        open(&image(flags, &[advance()])),
        "compact image without a capability table",
    );
    assert_corrupt(
        open(&image(flags, &[advance(), records()])),
        "checkpoint frame before the capability table",
    );
    // An empty table cannot carry the flag: the image holds no block.
    assert_corrupt(
        open(&image(flags, &[advance(), table_frame(&[]), records()])),
        "invalid capability table",
    );
    // A table whose declared kinds never appear is incomplete.
    assert_corrupt(
        open(&image(
            flags,
            &[advance(), table_frame(&[(0x02FF, 1, 0)]), records()],
        )),
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
    let with_table = |table: Vec<u8>| {
        open(&image(
            flags,
            &[advance(), table, records(), block.frame(1)],
        ))
    };
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
    let flags = FLAG_HISTORY | FLAG_COMPACT;
    let entries = [(0x02FF, 1, 0), (0x04FF, 2, 0)];
    let load = |blocks: Vec<Vec<u8>>| {
        let mut frames = vec![advance(), table_frame(&entries), records()];
        frames.extend(blocks);
        open(&image(flags, &frames))
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
        ("storage reserved", image_with(0x0002, 0)),
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
    let malformed = image(
        FLAG_HISTORY | FLAG_COMPACT,
        &[
            advance(),
            table_frame(&[(0x0300, 1, 1), (0x0100, 1, 1)]),
            records(),
        ],
    );
    assert_corrupt(
        open(&malformed),
        "unsorted or duplicate capability table entries",
    );
}

#[test]
fn torn_or_damaged_compact_images_are_never_repaired() {
    let golden = derived_image();
    for length in 32..golden.len() {
        reason(open(&golden[..length]));
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
        assert_corrupt(Database::open(&path), "");
        assert_corrupt(Database::discard_incomplete_tail(&path), "");
        assert_corrupt(ReplicaFollower::open(&path, None), "");
        assert_eq!(fs::read(&path).unwrap(), torn, "cut at {cut}");
    }

    // Even a tear inside the prefix of a block whose kind is already in use
    // (so every table entry is used) is corruption: a torn write keeps the
    // frame's first bytes, and below 16 of them the image fails closed.
    let blocks = derived_blocks();
    let last = blocks[1].frame(1);
    let run = image(
        FLAG_HISTORY | FLAG_COMPACT,
        &[
            advance(),
            table_frame(&[(0x02FF, 1, 0)]),
            records(),
            blocks[0].frame(1),
            last.clone(),
        ],
    );
    assert_golden_state(&open(&run).unwrap());
    let start = run.len() - last.len();
    for cut in start + 1..run.len() {
        assert_corrupt(open(&run[..cut]), "torn compact checkpoint image");
    }
    for cut in [start + 10, start + 40] {
        fs::write(&path, &run[..cut]).unwrap();
        let cursor = FeedCursor {
            lineage: LINEAGE,
            sequence: FLOOR,
        };
        assert_corrupt(
            read_log_changes(&path, None, &cursor, 10),
            "torn compact checkpoint image",
        );
        assert_corrupt(
            maintenance::export_log_segment(&path, None, FLOOR, 1 << 20),
            "torn compact checkpoint image",
        );
    }

    // A log frame torn after a complete image is still an incomplete tail
    // once its sequence shows it is not part of the image.
    let mut primary = Database::open_from_bytes(golden.clone(), None).unwrap();
    primary.put(3, &[3.0, 3.0], b"c").unwrap();
    let full = primary.to_bytes().unwrap();
    assert!(full.len() > golden.len() + 36);
    for cut in [golden.len() + 1, golden.len() + 15] {
        fs::write(&path, &full[..cut]).unwrap();
        assert_corrupt(Database::open(&path), "torn compact checkpoint image");
        assert_corrupt(
            Database::discard_incomplete_tail(&path),
            "torn compact checkpoint image",
        );
        assert_corrupt(
            read_log_changes(
                &path,
                None,
                &FeedCursor {
                    lineage: LINEAGE,
                    sequence: FLOOR,
                },
                10,
            ),
            "torn compact checkpoint image",
        );
    }
    for cut in [golden.len() + 16, golden.len() + 20, full.len() - 1] {
        let torn = &full[..cut];
        fs::write(&path, torn).unwrap();
        assert!(
            matches!(Database::open(&path), Err(Error::IncompleteTail { .. })),
            "cut at {cut}"
        );
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
        4
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
    for cut in (32 + 64..bytes.len()).step_by(7) {
        reason(Database::open_from_bytes(bytes[..cut].to_vec(), Some(&key)));
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
