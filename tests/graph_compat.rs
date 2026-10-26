//! TEST-002 / DO-001 / DO-003 (feature-agent-graph EPIC-002, in-tree half).
//!
//! Proves, with the default feature set and no graph code, that every
//! graph-unaware library entry point listed in REQ-055 rejects a synthetic
//! V5 file whose header carries the proposed `FLAG_GRAPH = 4` bit (with a
//! valid header CRC and one op-12 frame encoded with the DO-003 draft
//! layout), and that op 12 is not accepted by the current replay allow-list.
//! In `--features graph` builds the same calls follow the EPIC-006 contract
//! instead (graph-aware maintenance, replicas and log export; `upgrade` and
//! change feeds reject explicitly), and every source stays byte-identical.
//! The release-tag half (C-4) and the C ABI/Python/Node entry points are run by
//! `scripts\graph_compat_check.py` against isolated `git archive` builds.
//!
//! The deterministic fixtures are also committed as hex text under
//! `tests\fixtures\graph_compat\`, generated independently by
//! `python scripts\graph_compat_check.py fixtures`; this test asserts that the
//! Rust and Python encoders agree byte for byte.

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::tempdir;
use vlite::changes::{FeedCursor, read_log_changes};
use vlite::maintenance::{self, RestoreTarget};
use vlite::{
    CreateOptions, Database, EncryptionKey, Error, FormatVersion, LogSegment, Mutation,
    ReplicaFollower,
};

// ---------------------------------------------------------------------------
// Fixture constants (mirrored in scripts/graph_compat_check.py).
// ---------------------------------------------------------------------------

const DIMS: u32 = 2;
const LINEAGE: u64 = 0x4752_4150_4801_0001;
const RECORD_ID: u64 = 7;
const VECTOR: [f32; 2] = [1.0, 2.0];
const PAYLOAD: &[u8] = b"probe";
const T0_MS: u64 = 1_700_000_000_000;
const KEY: [u8; 32] = [0x42; 32];

const FLAG_ENCRYPTED: u16 = 1;
const FLAG_TIMESTAMPS: u16 = 2;
/// Proposed by DO-001; not defined anywhere in `src`.
const FLAG_GRAPH: u16 = 4;
/// Proposed by DO-003: lowest operation tag not in the replay allow-list.
const OP_GRAPH: u8 = 12;
const OP_PUT: u8 = 1;
const OP_TIME_ANCHOR: u8 = 11;

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

fn header_v5(flags: u16) -> Vec<u8> {
    let mut bytes = b"VLITEDB\0".to_vec();
    bytes.extend_from_slice(&5u32.to_le_bytes());
    bytes.extend_from_slice(&DIMS.to_le_bytes());
    bytes.extend_from_slice(&LINEAGE.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&flags.to_le_bytes());
    let checksum = crc32(&bytes);
    bytes.extend_from_slice(&checksum.to_le_bytes());
    bytes
}

/// Rewrites header flags of an existing file and recomputes the header CRC.
fn set_header_flags(bytes: &mut [u8], flags: u16) {
    bytes[26..28].copy_from_slice(&flags.to_le_bytes());
    let checksum = crc32(&bytes[..28]);
    bytes[28..32].copy_from_slice(&checksum.to_le_bytes());
}

/// `format::wrap` layout: 36-byte prefix, body, 16-byte commit footer.
fn frame(op: u8, sequence: u64, id: u64, count: u32, payload_len: u32, body: &[u8]) -> Vec<u8> {
    let mut bytes = b"VREC".to_vec();
    bytes.push(op);
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&sequence.to_le_bytes());
    bytes.extend_from_slice(&id.to_le_bytes());
    bytes.extend_from_slice(&count.to_le_bytes());
    bytes.extend_from_slice(&payload_len.to_le_bytes());
    let checksum = crc32(&bytes);
    bytes.extend_from_slice(&checksum.to_le_bytes());
    bytes.extend_from_slice(body);
    let checksum = crc32(&bytes);
    bytes.extend_from_slice(b"VCMT");
    bytes.extend_from_slice(&checksum.to_le_bytes());
    bytes.extend_from_slice(&sequence.to_le_bytes());
    bytes
}

fn put_frame(sequence: u64) -> Vec<u8> {
    let mut body = Vec::new();
    for value in VECTOR {
        body.extend_from_slice(&value.to_le_bytes());
    }
    body.extend_from_slice(PAYLOAD);
    frame(
        OP_PUT,
        sequence,
        RECORD_ID,
        DIMS,
        PAYLOAD.len() as u32,
        &body,
    )
}

fn anchor_frame(next_sequence: u64, timestamp_ms: u64) -> Vec<u8> {
    frame(
        OP_TIME_ANCHOR,
        next_sequence,
        0,
        0,
        8,
        &timestamp_ms.to_le_bytes(),
    )
}

// ---------------------------------------------------------------------------
// DO-003 draft graph frame body (see decisions.md, "DO-003"). Test-only.
// ---------------------------------------------------------------------------

const GRAPH_BODY_VERSION: u8 = 1;
const TAG_CREATE_GRAPH: u8 = 1;
const TAG_VECTOR_MUTATION: u8 = 64;

fn graph_entry(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![tag, 0];
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

fn graph_body(graph_id: u16, entries: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = vec![GRAPH_BODY_VERSION, 0];
    bytes.extend_from_slice(&graph_id.to_le_bytes());
    for entry in entries {
        bytes.extend_from_slice(entry);
    }
    bytes
}

fn create_graph_entry(name: &str) -> Vec<u8> {
    let mut payload = vec![name.len() as u8];
    payload.extend_from_slice(name.as_bytes());
    graph_entry(TAG_CREATE_GRAPH, &payload)
}

/// One op-12 frame creating graph 1 named "g".
fn graph_frame(sequence: u64) -> Vec<u8> {
    let body = graph_body(1, &[create_graph_entry("g")]);
    frame(OP_GRAPH, sequence, 0, 1, body.len() as u32, &body)
}

/// Independent re-implementation of the batch-entry encoding the draft reuses
/// for `VectorMutation` (only the variants without metadata, whose bytes do
/// not depend on the private metadata encoder).
fn batch_entry(mutation: &Mutation) -> Vec<u8> {
    let mut bytes = Vec::new();
    match mutation {
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
        Mutation::PutWithMetadata { .. } => unreachable!("not used by this test"),
    }
    bytes
}

fn vector_mutation_entry(collection: u16, encoded_batch_entry: &[u8]) -> Vec<u8> {
    let mut payload = collection.to_le_bytes().to_vec();
    payload.extend_from_slice(&0u16.to_le_bytes());
    payload.extend_from_slice(encoded_batch_entry);
    graph_entry(TAG_VECTOR_MUTATION, &payload)
}

// ---------------------------------------------------------------------------
// Fixtures.
// ---------------------------------------------------------------------------

/// V5, flags = FLAG_GRAPH, put (seq 1), op-12 frame (seq 2).
fn graph_plain() -> Vec<u8> {
    let mut bytes = header_v5(FLAG_GRAPH);
    bytes.extend(put_frame(1));
    bytes.extend(graph_frame(2));
    bytes
}

/// V5, flags = FLAG_TIMESTAMPS | FLAG_GRAPH; every commit preceded by an anchor.
fn graph_timestamps() -> Vec<u8> {
    let mut bytes = header_v5(FLAG_TIMESTAMPS | FLAG_GRAPH);
    bytes.extend(anchor_frame(1, T0_MS));
    bytes.extend(put_frame(1));
    bytes.extend(anchor_frame(2, T0_MS + 1));
    bytes.extend(graph_frame(2));
    bytes
}

/// Control: identical to `graph_plain` without the flag bit and the op-12 frame.
fn control_plain() -> Vec<u8> {
    let mut bytes = header_v5(0);
    bytes.extend(put_frame(1));
    bytes
}

/// Control: identical to `graph_timestamps` without the flag bit and op 12.
fn control_timestamps() -> Vec<u8> {
    let mut bytes = header_v5(FLAG_TIMESTAMPS);
    bytes.extend(anchor_frame(1, T0_MS));
    bytes.extend(put_frame(1));
    bytes
}

/// Control: no flag bit but an op-12 frame; proves op 12 is outside the
/// current allow-list (DO-003 "lowest unused").
fn control_op12_without_flag() -> Vec<u8> {
    let mut bytes = control_plain();
    bytes.extend(graph_frame(2));
    bytes
}

fn key() -> EncryptionKey {
    EncryptionKey::from_bytes(KEY)
}

/// Encrypted V5 file written by the public API, then the header gets
/// `FLAG_GRAPH` with a recomputed CRC. No op-12 frame is appended: sealing
/// needs the private cipher, and the header flag check precedes the
/// encryption descriptor (format::read_log_header), so the frame is never
/// reached by a graph-unaware reader.
fn graph_encrypted(dir: &Path) -> Vec<u8> {
    let path = dir.join("encrypted-base.vlite");
    let options = CreateOptions {
        format: Some(FormatVersion::V5),
        encryption: Some(key()),
        commit_timestamps: false,
    };
    let mut db = Database::create_with_options(&path, DIMS as usize, options).unwrap();
    db.put(RECORD_ID, &VECTOR, PAYLOAD).unwrap();
    db.close().unwrap();
    // Sanity: the unmodified base is a valid encrypted file.
    let reopened = Database::open_encrypted(&path, &key()).unwrap();
    assert_eq!(reopened.get(RECORD_ID).unwrap().unwrap().payload, PAYLOAD);
    drop(reopened);
    let mut bytes = fs::read(&path).unwrap();
    let flags = u16::from_le_bytes([bytes[26], bytes[27]]);
    assert_eq!(flags, FLAG_ENCRYPTED);
    set_header_flags(&mut bytes, flags | FLAG_GRAPH);
    bytes
}

fn decode_hex(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    assert!(digits.len().is_multiple_of(2));
    digits
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn committed_fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("graph_compat")
        .join(format!("{name}.hex"));
    decode_hex(&fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "{} missing ({error}); run `python scripts\\graph_compat_check.py fixtures`",
            path.display()
        )
    }))
}

// ---------------------------------------------------------------------------
// Assertions.
// ---------------------------------------------------------------------------

/// What a `--features graph` build returns for a graph fixture when the
/// call's key matches the file (EPIC-006).
#[derive(Clone, Copy, Debug)]
enum GraphBuild {
    /// Graph-aware: LEG-009, LEG-011, LEG-012, LEG-014, LEG-021..023.
    Accepts,
    /// LEG-010: `Error::GraphUnsupportedOperation`.
    Upgrade,
    /// LEG-013: `Error::GraphChangeFeedUnsupported`.
    Feed,
    /// A vector-only follower refuses a graph primary's segment header with
    /// `Error::ReplicaDiverged` (LEG-014).
    Diverged,
}

/// Graph-unaware builds (REQ-055, TEST-002) reject every graph fixture at the
/// header with `UnsupportedFeature(4)`. Graph builds return `graph` when the
/// key matches the file (`keyed` exactly for the encrypted fixture) and
/// otherwise the key error any vector-only file gives. Returns whether the
/// call was rejected.
#[track_caller]
fn assert_outcome(
    entry_point: &str,
    encrypted: bool,
    keyed: bool,
    graph: GraphBuild,
    result: vlite::Result<()>,
) -> bool {
    let rejected = result.is_err();
    match (encrypted, keyed, result) {
        (true, false, Err(Error::EncryptionKeyRequired))
        | (false, true, Err(Error::NotEncrypted)) => {}
        (true, false, result) | (false, true, result) => {
            panic!("{entry_point}: expected the key error of a vector-only file, got {result:?}")
        }
        (_, _, result) => match (graph, result) {
            (GraphBuild::Accepts, Ok(()))
            | (GraphBuild::Upgrade, Err(Error::GraphUnsupportedOperation))
            | (GraphBuild::Feed, Err(Error::GraphChangeFeedUnsupported))
            | (GraphBuild::Diverged, Err(Error::ReplicaDiverged(_))) => {}
            (graph, result) => panic!("{entry_point}: expected {graph:?}, got {result:?}"),
        },
    }
    rejected
}

#[track_caller]
fn assert_absent(entry_point: &str, path: &Path) {
    assert!(
        !path.exists(),
        "{entry_point}: rejected input must not publish {}",
        path.display()
    );
}

struct Fixture {
    name: &'static str,
    path: PathBuf,
    bytes: Vec<u8>,
    encrypted: bool,
    /// Committed sequences (graph builds bootstrap a follower from them).
    commits: u64,
}

/// Runs every REQ-055 library entry point against one graph-flagged file.
/// Returns the number of entry-point invocations asserted.
fn probe_all(fixture: &Fixture, scratch: &Path, follower_base: &Path) -> usize {
    use GraphBuild::{Accepts, Diverged, Feed, Upgrade};
    let source = fixture.path.as_path();
    let key = key();
    let out = |name: &str| scratch.join(format!("{}-{name}.vlite", fixture.name));
    let mut checked = 0usize;
    // `Database` open/repair entry points become graph-aware in
    // `--features graph` builds (feature-agent-graph ITEM-010); the other
    // entry points follow in EPIC-006 (`assert_outcome`).
    let mut database_check = |name: &str, result: vlite::Result<()>| {
        if let Err(Error::UnsupportedFeature(FLAG_GRAPH)) = result {
            panic!("{} {name}: graph build rejected a graph file", fixture.name);
        }
        checked += 1;
    };

    database_check("Database::open", Database::open(source).map(drop));
    database_check(
        "Database::open_encrypted",
        Database::open_encrypted(source, &key).map(drop),
    );
    database_check(
        "Database::open_read_only",
        Database::open_read_only(source).map(drop),
    );
    database_check(
        "Database::open_read_only_encrypted",
        Database::open_read_only_encrypted(source, &key).map(drop),
    );
    database_check(
        "Database::open_from_bytes(None)",
        Database::open_from_bytes(fixture.bytes.clone(), None).map(drop),
    );
    database_check(
        "Database::open_from_bytes(key)",
        Database::open_from_bytes(fixture.bytes.clone(), Some(&key)).map(drop),
    );
    database_check(
        "Database::discard_incomplete_tail",
        Database::discard_incomplete_tail(source).map(drop),
    );
    database_check(
        "Database::discard_incomplete_tail_encrypted",
        Database::discard_incomplete_tail_encrypted(source, &key).map(drop),
    );
    // Every other entry point: see `assert_outcome`. A destination is
    // asserted absent after every rejection.
    let encrypted = fixture.encrypted;
    let mut check = |name: &str, keyed: bool, graph: GraphBuild, result: vlite::Result<()>| {
        checked += 1;
        assert_outcome(
            &format!("{} {name}", fixture.name),
            encrypted,
            keyed,
            graph,
            result,
        )
    };
    check(
        "maintenance::inspect",
        false,
        Accepts,
        maintenance::inspect(source).map(drop),
    );
    check(
        "maintenance::inspect_with_key",
        true,
        Accepts,
        maintenance::inspect_with_key(source, &key).map(drop),
    );
    check(
        "maintenance::inspect_detail",
        false,
        Accepts,
        maintenance::inspect_detail(source).map(drop),
    );
    check(
        "maintenance::verify",
        false,
        Accepts,
        maintenance::verify(source).map(drop),
    );
    check(
        "maintenance::verify_with_key",
        true,
        Accepts,
        maintenance::verify_with_key(source, &key).map(drop),
    );

    let destination = out("backup");
    if check(
        "maintenance::backup",
        false,
        Accepts,
        maintenance::backup(source, &destination).map(drop),
    ) {
        assert_absent("maintenance::backup", &destination);
    }
    let destination = out("backup-key");
    if check(
        "maintenance::backup_with_key",
        true,
        Accepts,
        maintenance::backup_with_key(source, &destination, &key).map(drop),
    ) {
        assert_absent("maintenance::backup_with_key", &destination);
    }
    let destination = out("restore");
    if check(
        "maintenance::restore",
        false,
        Accepts,
        maintenance::restore(source, &destination).map(drop),
    ) {
        assert_absent("maintenance::restore", &destination);
    }
    let destination = out("restore-key");
    if check(
        "maintenance::restore_with_key",
        true,
        Accepts,
        maintenance::restore_with_key(source, &destination, &key).map(drop),
    ) {
        assert_absent("maintenance::restore_with_key", &destination);
    }
    let destination = out("compact");
    if check(
        "maintenance::compact",
        false,
        Accepts,
        maintenance::compact(source, &destination).map(drop),
    ) {
        assert_absent("maintenance::compact", &destination);
    }
    let destination = out("compact-at");
    if check(
        "maintenance::compact_at",
        false,
        Accepts,
        maintenance::compact_at(source, &destination, T0_MS).map(drop),
    ) {
        assert_absent("maintenance::compact_at", &destination);
    }
    let destination = out("compact-key");
    if check(
        "maintenance::compact_with_key",
        true,
        Accepts,
        maintenance::compact_with_key(source, &destination, &key).map(drop),
    ) {
        assert_absent("maintenance::compact_with_key", &destination);
    }
    let destination = out("upgrade");
    if check(
        "maintenance::upgrade",
        false,
        Upgrade,
        maintenance::upgrade(source, &destination).map(drop),
    ) {
        assert_absent("maintenance::upgrade", &destination);
    }
    let workdir = scratch.join(format!("{}-drill", fixture.name));
    fs::create_dir_all(&workdir).unwrap();
    check(
        "maintenance::drill",
        false,
        Accepts,
        maintenance::drill(source, &workdir).map(drop),
    );
    assert_eq!(
        fs::read_dir(&workdir).unwrap().count(),
        0,
        "drill must leave no scratch output"
    );
    for (label, source_key) in [("None", None), ("key", Some(&key))] {
        let keyed = source_key.is_some();
        let destination = out(&format!("reencrypt-{label}"));
        if check(
            &format!("maintenance::reencrypt(src {label})"),
            keyed,
            Accepts,
            maintenance::reencrypt(source, source_key, &destination, Some(&key)).map(drop),
        ) {
            assert_absent("maintenance::reencrypt", &destination);
        }
        let destination = out(&format!("restore-to-{label}"));
        if check(
            &format!("maintenance::restore_to(src {label})"),
            keyed,
            Accepts,
            maintenance::restore_to(
                None,
                source,
                &destination,
                RestoreTarget::Sequence(1),
                source_key,
            )
            .map(drop),
        ) {
            assert_absent("maintenance::restore_to", &destination);
        }
        check(
            &format!("maintenance::export_log_segment({label})"),
            keyed,
            Accepts,
            maintenance::export_log_segment(source, source_key, 0, 1 << 20).map(drop),
        );
        check(
            &format!("changes::read_log_changes({label})"),
            keyed,
            Feed,
            read_log_changes(
                source,
                source_key,
                &FeedCursor {
                    lineage: 0,
                    sequence: 0,
                },
                16,
            )
            .map(drop),
        );
        check(
            &format!("ReplicaFollower::open({label})"),
            keyed,
            Accepts,
            ReplicaFollower::open(source, source_key).map(drop),
        );
    }

    // Bootstrap segment carrying the graph header and all frames verbatim.
    let segment = LogSegment {
        lineage: u64::from_le_bytes(fixture.bytes[16..24].try_into().unwrap()),
        header: fixture.bytes[..32].to_vec(),
        base_sequence: 0,
        last_sequence: fixture.commits,
        head_sequence: fixture.commits,
        frames: fixture.bytes[32..].to_vec(),
    };
    for (label, segment_key) in [("None", None), ("key", Some(&key))] {
        let destination = out(&format!("follower-{label}"));
        if check(
            &format!("ReplicaFollower::create({label})"),
            segment_key.is_some(),
            Accepts,
            ReplicaFollower::create(&destination, &segment, segment_key).map(drop),
        ) {
            assert_absent("ReplicaFollower::create", &destination);
        }
    }

    // A healthy vector-only follower must refuse to tail a graph file.
    let follower_path = out("vector-follower");
    fs::copy(follower_base, &follower_path).unwrap();
    let before = fs::read(&follower_path).unwrap();
    let mut follower = ReplicaFollower::open(&follower_path, None).unwrap();
    check(
        "ReplicaFollower::sync_from_log",
        false,
        Diverged,
        follower.sync_from_log(source, 1 << 20).map(drop),
    );
    drop(follower);
    assert_eq!(fs::read(&follower_path).unwrap(), before);
    checked
}

#[test]
fn committed_hex_fixtures_match_the_rust_encoder() {
    for (name, bytes) in [
        ("v5_graph_plain", graph_plain()),
        ("v5_graph_timestamps", graph_timestamps()),
        ("v5_control_plain", control_plain()),
        ("v5_control_timestamps", control_timestamps()),
        ("v5_control_op12_without_flag", control_op12_without_flag()),
    ] {
        assert_eq!(
            committed_fixture(name),
            bytes,
            "{name}: Python and Rust fixture encoders disagree"
        );
    }
}

#[test]
fn control_fixtures_are_otherwise_valid_and_op12_is_unused() {
    let dir = tempdir().unwrap();
    for (name, bytes) in [
        ("plain", control_plain()),
        ("timestamps", control_timestamps()),
    ] {
        let path = dir.path().join(format!("control-{name}.vlite"));
        fs::write(&path, bytes).unwrap();
        let db = Database::open(&path).unwrap();
        assert_eq!(db.format_version(), 5, "{name}");
        assert_eq!(db.lineage(), Some(LINEAGE), "{name}");
        let record = db.get(RECORD_ID).unwrap().unwrap();
        assert_eq!(record.vector, VECTOR, "{name}");
        assert_eq!(record.payload, PAYLOAD, "{name}");
        drop(db);
        maintenance::verify(&path).unwrap();
    }

    let path = dir.path().join("control-op12.vlite");
    fs::write(&path, control_op12_without_flag()).unwrap();
    let frame_offset = (32 + put_frame(1).len()) as u64;
    match Database::open(&path) {
        Err(Error::Corrupt { offset, reason }) => {
            assert_eq!(offset, frame_offset);
            assert_eq!(reason, "invalid operation or reserved record bytes");
        }
        Err(other) => panic!("op 12 without flag: unexpected {other:?}"),
        Ok(_) => panic!("op 12 is already accepted by replay; DO-003 must pick another tag"),
    }
    for op in 12..=u8::MAX {
        let mut bytes = control_plain();
        bytes.extend(frame(op, 2, 0, 1, 4, &[1, 0, 1, 0]));
        assert!(
            matches!(
                Database::open_from_bytes(bytes, None),
                Err(Error::Corrupt { .. })
            ),
            "op {op} must not be in the replay allow-list"
        );
    }
}

/// REQ-055 probe: 35 calls per fixture. Default builds expect
/// `UnsupportedFeature(4)` from every call with no output left behind; graph
/// builds expect the per-call EPIC-006 outcome named in `probe_all`. The
/// source stays byte-identical in both builds.
#[test]
fn every_library_entry_point_rejects_graph_flagged_files() {
    let dir = tempdir().unwrap();
    let scratch = dir.path().join("out");
    fs::create_dir_all(&scratch).unwrap();
    let follower_base = dir.path().join("follower-base.vlite");
    fs::write(&follower_base, control_plain()).unwrap();

    let mut fixtures = Vec::new();
    for (name, bytes, encrypted, commits) in [
        ("plain", graph_plain(), false, 2),
        ("timestamps", graph_timestamps(), false, 2),
        ("encrypted", graph_encrypted(dir.path()), true, 1),
    ] {
        let path = dir.path().join(format!("graph-{name}.vlite"));
        fs::write(&path, &bytes).unwrap();
        fixtures.push(Fixture {
            name,
            path,
            bytes,
            encrypted,
            commits,
        });
    }

    let mut total = 0;
    for fixture in &fixtures {
        let original = fs::read(&fixture.path).unwrap();
        total += probe_all(fixture, &scratch, &follower_base);
        assert_eq!(
            fs::read(&fixture.path).unwrap(),
            original,
            "{}: an entry point modified the source file",
            fixture.name
        );
    }
    assert_eq!(total, fixtures.len() * 35);
}

/// In `--features graph` builds the DO-003 draft fixtures are valid graph
/// files: the op-12 frame replays into graph "g" and the vector record stays.
#[test]
fn graph_builds_open_the_draft_fixtures() {
    let dir = tempdir().unwrap();
    for (name, bytes) in [("plain", graph_plain()), ("timestamps", graph_timestamps())] {
        let path = dir.path().join(format!("{name}.vlite"));
        fs::write(&path, &bytes).unwrap();
        let db = Database::open(&path).unwrap();
        assert!(db.is_graph_enabled(), "{name}");
        let names: Vec<String> = db
            .list_graphs()
            .unwrap()
            .into_iter()
            .map(|g| g.name)
            .collect();
        assert_eq!(names, ["g"], "{name}");
        assert_eq!(
            db.get(RECORD_ID).unwrap().unwrap().payload,
            PAYLOAD,
            "{name}"
        );
        drop(db);
        assert_eq!(
            fs::read(&path).unwrap(),
            bytes,
            "{name}: open modified the file"
        );
    }
    let db = Database::open_from_bytes(graph_encrypted(dir.path()), Some(&key())).unwrap();
    assert!(db.is_graph_enabled());
    assert!(db.list_graphs().unwrap().is_empty());
}

#[test]
fn replica_apply_rejects_graph_segments_explicitly() {
    let dir = tempdir().unwrap();
    let follower_path = dir.path().join("follower.vlite");
    fs::write(&follower_path, control_plain()).unwrap();
    let before = fs::read(&follower_path).unwrap();
    let mut follower = ReplicaFollower::open(&follower_path, None).unwrap();
    assert_eq!(follower.applied_sequence(), 1);
    let graph_frame_bytes = graph_frame(2);

    // Segment cut from a graph file: header differs from the follower's.
    let graph_segment = LogSegment {
        lineage: LINEAGE,
        header: header_v5(FLAG_GRAPH),
        base_sequence: 1,
        last_sequence: 2,
        head_sequence: 2,
        frames: graph_frame_bytes.clone(),
    };
    match follower.apply(&graph_segment) {
        Err(Error::ReplicaDiverged(reason)) => {
            assert_eq!(reason, "segment header differs from follower")
        }
        other => panic!("ReplicaFollower::apply(graph header): {other:?}"),
    }
    let segment_path = dir.path().join("graph.vlseg");
    graph_segment.save(&segment_path).unwrap();
    match follower.apply_file(&segment_path) {
        Err(Error::ReplicaDiverged(reason)) => {
            assert_eq!(reason, "segment header differs from follower")
        }
        other => panic!("ReplicaFollower::apply_file(graph header): {other:?}"),
    }

    // Even with a matching header, the op-12 frame itself is refused.
    let smuggled = LogSegment {
        header: header_v5(0),
        ..graph_segment
    };
    assert!(
        matches!(follower.apply(&smuggled), Err(Error::Corrupt { .. })),
        "op-12 frame in a vector-only segment must be corruption"
    );
    assert_eq!(follower.applied_sequence(), 1);
    drop(follower);
    assert_eq!(fs::read(&follower_path).unwrap(), before);
}

#[test]
fn do003_vector_mutation_reuses_the_existing_batch_entry_bytes() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("batch.vlite");
    let mut db = Database::create_with_format(&path, 2, FormatVersion::V5).unwrap();
    let operations = [
        Mutation::Put {
            id: 11,
            vector: vec![0.5, -1.5],
            payload: b"vec".to_vec(),
        },
        Mutation::Delete { id: 11 },
    ];
    db.write_batch(&operations).unwrap();
    db.close().unwrap();

    let bytes = fs::read(&path).unwrap();
    let mut offset = 32;
    let body = loop {
        assert!(offset + 36 <= bytes.len(), "no batch frame written");
        let op = bytes[offset + 4];
        let payload_len =
            u32::from_le_bytes(bytes[offset + 28..offset + 32].try_into().unwrap()) as usize;
        let body = &bytes[offset + 36..offset + 36 + payload_len];
        if op == 3 {
            break body.to_vec();
        }
        offset += 36 + payload_len + 16;
    };

    let entries: Vec<Vec<u8>> = operations.iter().map(batch_entry).collect();
    assert_eq!(body, entries.concat(), "batch entry encoding drifted");

    // The draft VectorMutation entry embeds exactly one batch entry verbatim.
    let entry = vector_mutation_entry(0, &entries[0]);
    assert_eq!(entry[0], TAG_VECTOR_MUTATION);
    assert_eq!(&entry[1..4], &[0, 0, 0]);
    assert_eq!(
        u32::from_le_bytes(entry[4..8].try_into().unwrap()) as usize,
        4 + entries[0].len()
    );
    assert_eq!(&entry[8..12], &[0, 0, 0, 0]);
    assert_eq!(&entry[12..], entries[0].as_slice());
    let body = graph_body(1, &[entry]);
    assert_eq!(&body[..4], &[GRAPH_BODY_VERSION, 0, 1, 0]);
}
