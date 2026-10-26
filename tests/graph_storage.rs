//! EPIC-003 graph storage foundation (ITEM-009/010/011/012/013/014), built
//! only with the `graph` feature.
//!
//! Covers graph-enabled create/reopen in plain, encrypted and commit-timestamp
//! files, strict op-12 replay (checksums, prefix fields, body decoding,
//! vector-mutation and attachment entries interleaved with graph entries,
//! attachment record checks and atomic state application), the
//! `graph`/`graph_mut` handles, TEST-009 plaintext leakage, ordinary-file
//! behavior in graph builds, and the EPIC-006 maintenance, replica and feed
//! contract for graph files.

use std::fs;

use tempfile::tempdir;
use vlite::changes::{FeedCursor, read_log_changes};
use vlite::maintenance;
use vlite::property_graph::GraphInfo;
use vlite::{
    CollectionSettings, CollectionSpec, CreateOptions, Database, EncryptionKey, Error,
    FormatVersion, Metadata, MetadataValue, Mutation, ReplicaFollower,
};

// The same engine source `vlite` compiles, used here to craft frame bodies.
#[allow(dead_code, unused_imports)]
#[path = "../src/property_graph/engine/mod.rs"]
mod engine;

use engine::{GraphBody, GraphEntry, GraphId, NodeId, encode_body};

const FLAG_ENCRYPTED: u16 = 1;
const FLAG_TIMESTAMPS: u16 = 2;
const FLAG_GRAPH: u16 = 4;
const OP_GRAPH: u8 = 12;
const HEADER: usize = 32;
const PREFIX: usize = 36;
const FOOTER: usize = 16;

fn key() -> EncryptionKey {
    EncryptionKey::from_bytes([7; 32])
}

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

/// Plaintext frame in the `format::wrap` layout.
fn frame(
    op: u8,
    collection: u16,
    sequence: u64,
    id: u64,
    count: u32,
    payload_len: u32,
    body: &[u8],
) -> Vec<u8> {
    let mut bytes = b"VREC".to_vec();
    bytes.push(op);
    bytes.extend_from_slice(&collection.to_le_bytes());
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

fn body(graph: u16, entries: Vec<GraphEntry>) -> (u32, Vec<u8>) {
    let count = entries.len() as u32;
    let body = GraphBody {
        graph_id: GraphId(graph),
        entries,
    };
    (count, encode_body(&body).unwrap())
}

fn graph_frame(sequence: u64, graph: u16, entries: Vec<GraphEntry>) -> Vec<u8> {
    let (count, bytes) = body(graph, entries);
    frame(OP_GRAPH, 0, sequence, 0, count, bytes.len() as u32, &bytes)
}

fn create(name: &str) -> GraphEntry {
    GraphEntry::CreateGraph { name: name.into() }
}

fn put_node(id: u64, key: &str) -> GraphEntry {
    GraphEntry::PutNode {
        node_id: NodeId(id),
        key: key.into(),
        labels: Default::default(),
        properties: Default::default(),
    }
}

fn header_flags(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[26], bytes[27]])
}

/// Canonical batch-entry bytes of one mutation, taken from a real op-3 frame
/// written by `write_batch` (the encoding vector entries must reuse).
fn batch_entry(dimensions: usize, mutation: Mutation) -> Vec<u8> {
    let dir = tempdir().unwrap();
    let path = dir.path().join("batch.vlite");
    let mut db = Database::create_with_format(&path, dimensions, FormatVersion::V5).unwrap();
    db.write_batch(&[mutation]).unwrap();
    db.close().unwrap();
    let bytes = fs::read(&path).unwrap();
    let mut offset = HEADER;
    loop {
        let op = bytes[offset + 4];
        let count = u32::from_le_bytes(bytes[offset + 24..offset + 28].try_into().unwrap());
        let payload = u32::from_le_bytes(bytes[offset + 28..offset + 32].try_into().unwrap());
        let body_len = match op {
            1 => count as usize * 4 + payload as usize,
            2 => 0,
            _ => payload as usize,
        };
        if op == 3 {
            return bytes[offset + PREFIX..offset + PREFIX + body_len].to_vec();
        }
        offset += PREFIX + body_len + FOOTER;
    }
}

fn expiring_record_frame(sequence: u64, id: u64) -> Vec<u8> {
    let dir = tempdir().unwrap();
    let path = dir.path().join("expiry.vlite");
    let mut db = Database::create_with_format(&path, 2, FormatVersion::V5).unwrap();
    db.put_with_expiry(
        id,
        &[1.0, 2.0],
        b"one",
        &Metadata::new(),
        Some(u64::MAX / 2),
    )
    .unwrap();
    db.close().unwrap();
    let bytes = fs::read(path).unwrap();
    let offset = HEADER;
    assert_eq!(bytes[offset + 4], 9);
    let payload_len =
        u32::from_le_bytes(bytes[offset + 28..offset + 32].try_into().unwrap()) as usize;
    let body = &bytes[offset + PREFIX..offset + PREFIX + payload_len];
    frame(9, 0, sequence, id, 2, payload_len as u32, body)
}

fn vector(collection: u16, dimensions: usize, mutation: Mutation) -> GraphEntry {
    GraphEntry::VectorMutation {
        collection,
        entry: batch_entry(dimensions, mutation),
    }
}

fn put(id: u64, vector: &[f32], payload: &[u8]) -> Mutation {
    Mutation::Put {
        id,
        vector: vector.to_vec(),
        payload: payload.to_vec(),
    }
}

fn attach(node: u64, collection: u16, record: u64) -> GraphEntry {
    GraphEntry::Attach {
        node_id: NodeId(node),
        collection,
        record_id: record,
    }
}

fn detach(node: u64, collection: u16) -> GraphEntry {
    GraphEntry::Detach {
        node_id: NodeId(node),
        collection,
    }
}

/// Op-12 frames `(offset, graph_id, body)` of an unencrypted file without
/// commit timestamps or with them (anchors are skipped).
fn graph_frames(bytes: &[u8]) -> Vec<(usize, u16, Vec<u8>)> {
    let mut frames = Vec::new();
    let mut offset = HEADER;
    while offset < bytes.len() {
        let op = bytes[offset + 4];
        let count = u32::from_le_bytes(bytes[offset + 24..offset + 28].try_into().unwrap());
        let payload = u32::from_le_bytes(bytes[offset + 28..offset + 32].try_into().unwrap());
        let body_len = match op {
            1 => count as usize * 4 + payload as usize,
            2 => 0,
            _ => payload as usize,
        };
        let body = &bytes[offset + PREFIX..offset + PREFIX + body_len];
        if op == OP_GRAPH {
            frames.push((
                offset,
                u16::from_le_bytes([body[2], body[3]]),
                body.to_vec(),
            ));
        }
        offset += PREFIX + body_len + FOOTER;
    }
    frames
}

fn names(db: &Database) -> Vec<String> {
    db.list_graphs()
        .unwrap()
        .into_iter()
        .map(|info| info.name)
        .collect()
}

#[track_caller]
fn assert_corrupt<T>(result: vlite::Result<T>, offset: usize, reason: &str) {
    match result {
        Err(Error::Corrupt {
            offset: actual,
            reason: actual_reason,
        }) => {
            assert_eq!(actual, offset as u64, "{actual_reason}");
            assert_eq!(actual_reason, reason);
        }
        Err(other) => panic!("expected corruption ({reason}), got {other:?}"),
        Ok(_) => panic!("expected corruption ({reason}), but the file opened"),
    }
}

/// Plain V5 graph file with one vector record (seq 1) and graph "g" (seq 2).
fn base_graph_file() -> Vec<u8> {
    let dir = tempdir().unwrap();
    let path = dir.path().join("base.vlite");
    let mut db = Database::create_with_graph(&path, 2, CreateOptions::default()).unwrap();
    db.put(1, &[1.0, 2.0], b"one").unwrap();
    db.create_graph("g").unwrap();
    db.close().unwrap();
    fs::read(&path).unwrap()
}

#[test]
fn graph_files_round_trip_in_plain_encrypted_and_timestamp_formats() {
    let dir = tempdir().unwrap();
    for (label, encrypted, timestamps) in [
        ("plain", false, false),
        ("encrypted", true, false),
        ("timestamps", false, true),
        ("encrypted-timestamps", true, true),
    ] {
        let path = dir.path().join(format!("{label}.vlite"));
        let options = CreateOptions {
            format: None,
            encryption: encrypted.then(key),
            commit_timestamps: timestamps,
        };
        let mut db = Database::create_with_graph(&path, 2, options).unwrap();
        assert!(db.is_graph_enabled(), "{label}");
        assert_eq!(db.format_version(), 5, "{label}");
        assert_eq!(db.list_graphs().unwrap(), Vec::new(), "{label}");
        db.put(1, &[1.0, 2.0], b"one").unwrap();
        let info = db.create_graph("alpha").unwrap();
        assert_eq!(
            (info.name.as_str(), info.node_count, info.edge_count),
            ("alpha", 0, 0)
        );
        db.create_graph("beta").unwrap();
        db.drop_graph("alpha").unwrap();
        db.create_graph("gamma").unwrap();
        db.put(2, &[3.0, 4.0], b"two").unwrap();
        assert_eq!(names(&db), ["beta", "gamma"], "{label}");
        db.close().unwrap();

        let bytes = fs::read(&path).unwrap();
        let mut expected_flags = FLAG_GRAPH;
        if encrypted {
            expected_flags |= FLAG_ENCRYPTED;
        }
        if timestamps {
            expected_flags |= FLAG_TIMESTAMPS;
        }
        assert_eq!(header_flags(&bytes), expected_flags, "{label}");

        let reopen = |read_only: bool| match (encrypted, read_only) {
            (false, false) => Database::open(&path),
            (true, false) => Database::open_encrypted(&path, &key()),
            (false, true) => Database::open_read_only(&path),
            (true, true) => Database::open_read_only_encrypted(&path, &key()),
        };
        for read_only in [false, true] {
            let db = reopen(read_only).unwrap();
            assert!(db.is_graph_enabled(), "{label}");
            assert_eq!(db.has_commit_timestamps(), timestamps, "{label}");
            assert_eq!(names(&db), ["beta", "gamma"], "{label}");
            assert_eq!(db.get(1).unwrap().unwrap().payload, b"one", "{label}");
            assert_eq!(db.get(2).unwrap().unwrap().payload, b"two", "{label}");
            assert_eq!(db.len(), 2, "{label}");
        }
        let from_bytes =
            Database::open_from_bytes(bytes.clone(), encrypted.then(key).as_ref()).unwrap();
        assert_eq!(names(&from_bytes), ["beta", "gamma"], "{label}");

        // Writes after reopen continue the same sequence and survive again.
        let mut db = reopen(false).unwrap();
        db.drop_graph("beta").unwrap();
        db.close().unwrap();
        assert_eq!(names(&reopen(false).unwrap()), ["gamma"], "{label}");
    }
}

#[test]
fn read_only_handles_refuse_graph_writes_without_touching_the_file() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("ro.vlite");
    fs::write(&path, base_graph_file()).unwrap();
    let before = fs::read(&path).unwrap();
    let mut db = Database::open_read_only(&path).unwrap();
    assert!(matches!(db.create_graph("h"), Err(Error::ReadOnly)));
    assert!(matches!(db.drop_graph("g"), Err(Error::ReadOnly)));
    assert_eq!(names(&db), ["g"]);
    drop(db);
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn sixty_four_live_graphs_and_dropped_ids_are_reused() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("limit.vlite");
    let mut db = Database::create_with_graph(&path, 2, CreateOptions::default()).unwrap();
    for index in 0..64 {
        db.create_graph(&format!("g{index:02}")).unwrap();
    }
    let length = fs::metadata(&path).unwrap().len();
    assert!(matches!(db.create_graph("late"), Err(Error::TooManyGraphs)));
    // Name errors still take precedence over the limit.
    assert!(matches!(db.create_graph("g05"), Err(Error::GraphExists)));
    assert!(matches!(
        db.create_graph("Bad"),
        Err(Error::InvalidGraphName)
    ));
    assert_eq!(fs::metadata(&path).unwrap().len(), length);

    db.drop_graph("g10").unwrap();
    db.create_graph("late").unwrap();
    assert!(matches!(
        db.create_graph("later"),
        Err(Error::TooManyGraphs)
    ));
    // Repeated drop/create cycles never exhaust IDs (no lifetime limit).
    for round in 0..100 {
        db.drop_graph("late").unwrap();
        db.create_graph("late").unwrap();
        assert_eq!(db.list_graphs().unwrap().len(), 64, "round {round}");
    }
    db.close().unwrap();

    let bytes = fs::read(&path).unwrap();
    let frames = graph_frames(&bytes);
    assert_eq!(frames.len(), 64 + 2 + 200);
    let ids: Vec<u16> = frames[..64].iter().map(|frame| frame.1).collect();
    assert_eq!(ids, (0..64).collect::<Vec<_>>(), "lowest free IDs");
    // drop g10, then "late" takes ID 10 again on every cycle.
    assert!(frames[64..].iter().all(|frame| frame.1 == 10));

    let db = Database::open(&path).unwrap();
    let listed = names(&db);
    assert_eq!(listed.len(), 64);
    assert!(listed.contains(&"late".to_string()));
    assert!(!listed.contains(&"g10".to_string()));
    assert_eq!(listed, {
        let mut sorted = listed.clone();
        sorted.sort();
        sorted
    });
}

#[test]
fn names_follow_collection_rules_and_failures_write_nothing() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("names.vlite");
    let mut db = Database::create_with_graph(&path, 2, CreateOptions::default()).unwrap();
    db.create_graph("a").unwrap();
    db.create_graph(&format!("z{}", "9".repeat(62))).unwrap();
    db.create_graph("default").unwrap();
    db.create_graph("x_y-z").unwrap();
    let length = fs::metadata(&path).unwrap().len();
    for bad in [
        "",
        "A",
        "1a",
        "_a",
        "-a",
        "a b",
        "a.b",
        "é",
        &format!("a{}", "b".repeat(63)),
    ] {
        assert!(
            matches!(db.create_graph(bad), Err(Error::InvalidGraphName)),
            "{bad:?}"
        );
        assert!(
            matches!(db.drop_graph(bad), Err(Error::InvalidGraphName)),
            "{bad:?}"
        );
    }
    assert!(matches!(db.create_graph("a"), Err(Error::GraphExists)));
    assert!(matches!(
        db.drop_graph("missing"),
        Err(Error::GraphNotFound)
    ));
    assert_eq!(fs::metadata(&path).unwrap().len(), length);
    assert_eq!(db.list_graphs().unwrap().len(), 4);
    assert!(Error::GraphExists.to_string().contains("already exists"));
}

#[test]
fn ordinary_files_stay_vector_only_in_graph_builds() {
    let dir = tempdir().unwrap();
    for (label, format) in [
        ("v3", FormatVersion::V3),
        ("v4", FormatVersion::V4),
        ("v5", FormatVersion::V5),
    ] {
        let path = dir.path().join(format!("{label}.vlite"));
        let mut db = Database::create_with_format(&path, 2, format).unwrap();
        assert!(!db.is_graph_enabled());
        assert!(matches!(db.create_graph("g"), Err(Error::GraphsNotEnabled)));
        assert!(matches!(db.drop_graph("g"), Err(Error::GraphsNotEnabled)));
        assert!(matches!(db.list_graphs(), Err(Error::GraphsNotEnabled)));
        db.put(1, &[1.0, 2.0], b"x").unwrap();
        db.close().unwrap();
        let db = Database::open(&path).unwrap();
        assert!(!db.is_graph_enabled(), "{label}");
        drop(db);
        if format == FormatVersion::V5 {
            assert_eq!(header_flags(&fs::read(&path).unwrap()), 0);
        }

        let refused = dir.path().join(format!("{label}-graph.vlite"));
        if format != FormatVersion::V5 {
            let options = CreateOptions {
                format: Some(format),
                ..CreateOptions::default()
            };
            assert!(matches!(
                Database::create_with_graph(&refused, 2, options),
                Err(Error::GraphsRequireV5)
            ));
            assert!(!refused.exists());
        }
    }
    let options = CreateOptions {
        format: Some(FormatVersion::V5),
        ..CreateOptions::default()
    };
    let db = Database::create_with_graph(dir.path().join("explicit.vlite"), 2, options).unwrap();
    assert!(db.is_graph_enabled());
    let mut memory = Database::create_in_memory(2, CreateOptions::default()).unwrap();
    assert!(!memory.is_graph_enabled());
    assert!(matches!(
        memory.create_graph("g"),
        Err(Error::GraphsNotEnabled)
    ));
}

#[test]
fn vector_frames_are_byte_identical_in_graph_files() {
    let dir = tempdir().unwrap();
    let write = |name: &str, graph: bool| {
        let path = dir.path().join(name);
        let mut db = if graph {
            Database::create_with_graph(&path, 2, CreateOptions::default()).unwrap()
        } else {
            Database::create_with_format(&path, 2, FormatVersion::V5).unwrap()
        };
        db.put(1, &[1.0, 2.0], b"one").unwrap();
        db.write_batch(&[
            vlite::Mutation::Put {
                id: 2,
                vector: vec![0.5, 0.25],
                payload: b"two".to_vec(),
            },
            vlite::Mutation::Delete { id: 1 },
        ])
        .unwrap();
        db.delete(2).unwrap();
        db.close().unwrap();
        fs::read(&path).unwrap()
    };
    let plain = write("plain.vlite", false);
    let graph = write("graph.vlite", true);
    assert_eq!(header_flags(&plain), 0);
    assert_eq!(header_flags(&graph), FLAG_GRAPH);
    assert_eq!(plain[HEADER..], graph[HEADER..], "vector frames changed");

    // Interleaved graph frames leave each vector frame's body and layout as is.
    let path = dir.path().join("mixed.vlite");
    let mut db = Database::create_with_graph(&path, 2, CreateOptions::default()).unwrap();
    db.create_graph("g").unwrap();
    db.put(1, &[1.0, 2.0], b"one").unwrap();
    db.close().unwrap();
    let mixed = fs::read(&path).unwrap();
    let graph_len = graph_frames(&mixed)[0].2.len() + PREFIX + FOOTER;
    let put = &mixed[HEADER + graph_len..];
    let expected = frame(1, 0, 2, 1, 2, 3, &{
        let mut body = Vec::new();
        for value in [1.0f32, 2.0] {
            body.extend_from_slice(&value.to_le_bytes());
        }
        body.extend_from_slice(b"one");
        body
    });
    assert_eq!(put, expected.as_slice());
}

#[test]
fn written_graph_frames_match_the_engine_encoding() {
    let bytes = base_graph_file();
    let frames = graph_frames(&bytes);
    assert_eq!(frames.len(), 1);
    let (offset, graph_id, body_bytes) = &frames[0];
    assert_eq!(*graph_id, 0);
    let expected = graph_frame(2, 0, vec![create("g")]);
    assert_eq!(&bytes[*offset..], expected.as_slice());
    let decoded = engine::decode_body(body_bytes, 1).unwrap();
    assert_eq!(decoded.entries, vec![create("g")]);
}

#[test]
fn replay_applies_complete_valid_graph_frames() {
    let dir = tempdir().unwrap();
    let base = base_graph_file();
    // A frame written by a later graph build: one node in "g", one new graph.
    let mut bytes = base.clone();
    bytes.extend(graph_frame(
        3,
        0,
        vec![put_node(0, "alice"), put_node(4, "bob")],
    ));
    bytes.extend(graph_frame(4, 1, vec![create("h")]));
    let path = dir.path().join("nodes.vlite");
    fs::write(&path, &bytes).unwrap();
    let mut db = Database::open(&path).unwrap();
    let listed = db.list_graphs().unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!((listed[0].name.as_str(), listed[0].node_count), ("g", 2));
    assert_eq!((listed[1].name.as_str(), listed[1].node_count), ("h", 0));
    assert!(matches!(db.drop_graph("g"), Err(Error::GraphNotEmpty)));
    db.drop_graph("h").unwrap();
    // The lowest free ID (1) is reused; the replayed node IDs stay in "g".
    db.create_graph("i").unwrap();
    db.close().unwrap();
    let after = fs::read(&path).unwrap();
    assert_eq!(graph_frames(&after).last().unwrap().1, 1);
    let db = Database::open(&path).unwrap();
    let listed: Vec<GraphInfo> = db.list_graphs().unwrap();
    assert_eq!(
        listed
            .iter()
            .map(|info| (info.name.as_str(), info.node_count))
            .collect::<Vec<_>>(),
        [("g", 2), ("i", 0)]
    );
}

#[test]
fn malformed_graph_frames_fail_the_whole_open() {
    let base = base_graph_file();
    let at = base.len();
    let open = |frame: Vec<u8>| {
        let mut bytes = base.clone();
        bytes.extend(frame);
        Database::open_from_bytes(bytes, None)
    };
    let (count, valid) = body(0, vec![put_node(0, "a")]);
    let reserved = "invalid operation or reserved record bytes";
    let prefix = "invalid graph frame count, reserved ID or body length";
    let invalid_body = "invalid graph frame body";
    let violates = "graph frame violates graph state";

    // Checksums: record header CRC, then commit footer CRC.
    let mut bad = graph_frame(3, 0, vec![put_node(0, "a")]);
    bad[33] ^= 1;
    assert_corrupt(open(bad), at, "record header checksum mismatch");
    let mut bad = graph_frame(3, 0, vec![put_node(0, "a")]);
    bad[PREFIX + 5] ^= 1;
    assert_corrupt(open(bad), at, "commit footer or record checksum mismatch");
    let mut bad = graph_frame(3, 0, vec![put_node(0, "a")]);
    let footer = bad.len() - 8;
    bad[footer] ^= 1;
    assert_corrupt(open(bad), at, "commit footer or record checksum mismatch");
    // Sequence must continue the log.
    assert_corrupt(
        open(graph_frame(4, 0, vec![put_node(0, "a")])),
        at,
        "non-contiguous commit sequence",
    );

    // Prefix fields.
    let len = valid.len() as u32;
    assert_corrupt(
        open(frame(OP_GRAPH, 1, 3, 0, count, len, &valid)),
        at,
        reserved,
    );
    let mut bad = frame(OP_GRAPH, 0, 3, 0, count, len, &valid);
    bad[7] = 1;
    let checksum = crc32(&bad[..32]);
    bad[32..36].copy_from_slice(&checksum.to_le_bytes());
    assert_corrupt(open(bad), at, reserved);
    assert_corrupt(
        open(frame(OP_GRAPH, 0, 3, 9, count, len, &valid)),
        at,
        prefix,
    );
    assert_corrupt(open(frame(OP_GRAPH, 0, 3, 0, 0, len, &valid)), at, prefix);
    assert_corrupt(
        open(frame(OP_GRAPH, 0, 3, 0, 4097, len, &valid)),
        at,
        prefix,
    );
    assert_corrupt(
        open(frame(OP_GRAPH, 0, 3, 0, 1, 11, &valid[..11])),
        at,
        prefix,
    );
    assert_corrupt(
        open(frame(OP_GRAPH, 0, 3, 0, 1, (16 << 20) + 1, &[])),
        at,
        prefix,
    );

    // Body decoding: count mismatch, trailing bytes, version, graph ID, tags.
    let (_, two) = body(0, vec![put_node(0, "a"), put_node(1, "b")]);
    let two_len = two.len() as u32;
    assert_corrupt(
        open(frame(OP_GRAPH, 0, 3, 0, 1, two_len, &two)),
        at,
        invalid_body,
    );
    assert_corrupt(
        open(frame(
            OP_GRAPH,
            0,
            3,
            0,
            3,
            two_len + 16,
            &[two.clone(), vec![0; 16]].concat(),
        )),
        at,
        invalid_body,
    );
    let mut trailing = valid.clone();
    trailing.push(0);
    assert_corrupt(
        open(frame(OP_GRAPH, 0, 3, 0, 1, len + 1, &trailing)),
        at,
        invalid_body,
    );
    let mut version = valid.clone();
    version[0] = 2;
    assert_corrupt(
        open(frame(OP_GRAPH, 0, 3, 0, 1, len, &version)),
        at,
        invalid_body,
    );
    let mut graph_id = valid.clone();
    graph_id[2..4].copy_from_slice(&64u16.to_le_bytes());
    assert_corrupt(
        open(frame(OP_GRAPH, 0, 3, 0, 1, len, &graph_id)),
        at,
        invalid_body,
    );
    for tag in [0u8, 13, 63, 65, 255] {
        let mut tagged = valid.clone();
        tagged[4] = tag;
        assert_corrupt(
            open(frame(OP_GRAPH, 0, 3, 0, 1, len, &tagged)),
            at,
            invalid_body,
        );
    }

    // Semantic violations, including a failure after valid earlier entries.
    for entries in [
        vec![create("g")],
        vec![GraphEntry::DropGraph, GraphEntry::DropGraph],
        vec![put_node(0, "a"), put_node(1, "a")],
        vec![put_node(0, "a"), GraphEntry::DropGraph],
    ] {
        assert_corrupt(open(graph_frame(3, 0, entries)), at, violates);
    }
    assert_corrupt(
        open(graph_frame(3, 5, vec![put_node(0, "a")])),
        at,
        violates,
    );
    // A corrupt later frame fails the open even though the earlier one is valid.
    let mut bytes = base.clone();
    bytes.extend(graph_frame(3, 0, vec![put_node(0, "a")]));
    let second = bytes.len();
    bytes.extend(graph_frame(4, 0, vec![put_node(1, "a")]));
    assert_corrupt(Database::open_from_bytes(bytes, None), second, violates);
}

#[test]
fn vector_and_attachment_entries_replay_interleaved_with_graph_entries() {
    let metadata = Metadata::from([("kind".to_string(), MetadataValue::String("doc".into()))]);
    let mut bytes = base_graph_file();
    bytes.extend(graph_frame(
        3,
        0,
        vec![
            vector(0, 2, put(5, &[3.0, 4.0], b"five")),
            put_node(0, "a"),
            attach(0, 0, 5),
            vector(
                0,
                2,
                Mutation::PutWithMetadata {
                    id: 6,
                    vector: vec![5.0, 6.0],
                    payload: Vec::new(),
                    metadata: metadata.clone(),
                },
            ),
            put_node(1, "b"),
            vector(0, 2, Mutation::Delete { id: 1 }),
        ],
    ));
    // Re-embedding an attached record keeps its attachment (LEG-005).
    bytes.extend(graph_frame(
        4,
        0,
        vec![vector(0, 2, put(5, &[7.0, 8.0], b"5b")), attach(1, 0, 6)],
    ));
    let after_four = bytes.clone();
    // Detach first, then the record may be deleted in the same body.
    bytes.extend(graph_frame(
        5,
        0,
        vec![detach(0, 0), vector(0, 2, Mutation::Delete { id: 5 })],
    ));

    let db = Database::open_from_bytes(after_four.clone(), None).unwrap();
    let five = db.get(5).unwrap().unwrap();
    assert_eq!(
        (five.vector.as_slice(), five.payload.as_slice()),
        (&[7.0, 8.0][..], &b"5b"[..])
    );
    assert_eq!(db.revision(5).unwrap(), Some(4));
    assert!(db.get(1).unwrap().is_none());
    drop(db);

    let dir = tempdir().unwrap();
    let path = dir.path().join("replay.vlite");
    fs::write(&path, &bytes).unwrap();
    let mut db = Database::open(&path).unwrap();
    assert!(db.get(5).unwrap().is_none());
    let six = db.get(6).unwrap().unwrap();
    assert_eq!(six.vector, [5.0, 6.0]);
    assert_eq!(six.metadata, metadata);
    assert_eq!(db.revision(6).unwrap(), Some(3));
    assert_eq!(db.len(), 1);
    let info = db.graph("g").unwrap().info();
    assert_eq!((info.node_count, info.edge_count), (2, 0));
    // Ordinary commits continue the replayed sequence.
    db.put(9, &[1.0, 1.0], b"nine").unwrap();
    db.create_graph("h").unwrap();
    db.close().unwrap();
    let db = Database::open(&path).unwrap();
    assert_eq!(db.revision(9).unwrap(), Some(6));
    assert_eq!(names(&db), ["g", "h"]);
    assert_eq!(db.revision(6).unwrap(), Some(3));

    // Attachments persist across frames: node 1 still owns record 6 and node
    // 0 was detached, so record 6 cannot be deleted or attached again.
    let at = bytes.len();
    let open = |frame: Vec<u8>| {
        let mut extended = bytes.clone();
        extended.extend(frame);
        Database::open_from_bytes(extended, None)
    };
    let dangling = "graph attachment references a missing or expiring record";
    assert_corrupt(
        open(graph_frame(
            6,
            0,
            vec![vector(0, 2, Mutation::Delete { id: 6 })],
        )),
        at,
        dangling,
    );
    assert_corrupt(
        open(graph_frame(6, 0, vec![attach(0, 0, 6)])),
        at,
        "graph frame violates graph state",
    );
    open(graph_frame(
        6,
        0,
        vec![detach(1, 0), vector(0, 2, Mutation::Delete { id: 6 })],
    ))
    .unwrap();
}

#[test]
fn ordinary_frames_cannot_delete_or_expire_attached_records() {
    let mut attached = base_graph_file();
    attached.extend(graph_frame(3, 0, vec![put_node(0, "a"), attach(0, 0, 1)]));

    let delete_batch = batch_entry(2, Mutation::Delete { id: 1 });
    let invalid_frames = [
        frame(2, 0, 4, 1, 0, 0, &[]),
        frame(3, 0, 4, 0, 1, delete_batch.len() as u32, &delete_batch),
        expiring_record_frame(4, 1),
    ];

    for invalid in invalid_frames {
        let mut bytes = attached.clone();
        let offset = bytes.len();
        bytes.extend(invalid);
        assert_corrupt(
            Database::open_from_bytes(bytes, None),
            offset,
            "graph attachment references a missing or expiring record",
        );
    }
}

#[test]
fn vector_entries_target_named_collections_and_keep_their_dimensions() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("collections.vlite");
    let mut db = Database::create_with_graph(&path, 2, CreateOptions::default()).unwrap();
    let docs = db
        .create_collection(
            "docs",
            CollectionSpec {
                dimensions: 3,
                settings: CollectionSettings::default(),
            },
        )
        .unwrap()
        .id;
    db.create_graph("g").unwrap();
    db.close().unwrap();
    let base = fs::read(&path).unwrap();
    let at = base.len();
    let open = |frame: Vec<u8>| {
        let mut bytes = base.clone();
        bytes.extend(frame);
        Database::open_from_bytes(bytes, None)
    };

    let db = open(graph_frame(
        3,
        0,
        vec![
            put_node(0, "a"),
            vector(docs, 3, put(1, &[1.0, 2.0, 3.0], b"d")),
            attach(0, docs, 1),
        ],
    ))
    .unwrap();
    let collection = db.collection("docs").unwrap();
    assert_eq!(collection.get(1).unwrap().unwrap().vector, [1.0, 2.0, 3.0]);
    assert_eq!(collection.revision(1).unwrap(), Some(3));
    assert!(db.get(1).unwrap().is_none(), "default collection untouched");
    drop(db);

    // The entry is decoded with the target collection's dimensions.
    assert_corrupt(
        open(graph_frame(
            3,
            0,
            vec![vector(docs, 2, put(1, &[1.0, 2.0], b""))],
        )),
        at,
        "truncated batch entry body",
    );
    assert_corrupt(
        open(graph_frame(
            3,
            0,
            vec![vector(0, 3, put(1, &[1.0, 2.0, 3.0], b""))],
        )),
        at,
        "extra bytes after batch entries",
    );
    assert_corrupt(
        open(graph_frame(
            3,
            0,
            vec![vector(7, 2, put(1, &[1.0, 2.0], b""))],
        )),
        at,
        "graph vector entry references unknown collection",
    );
}

#[test]
fn malformed_vector_and_attachment_entries_fail_the_whole_open() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("expiring.vlite");
    let mut db = Database::create_with_graph(&path, 2, CreateOptions::default()).unwrap();
    db.put(1, &[1.0, 2.0], b"one").unwrap();
    db.put_with_expiry(2, &[1.0, 2.0], b"two", &Metadata::new(), Some(u64::MAX / 2))
        .unwrap();
    db.create_graph("g").unwrap();
    db.close().unwrap();
    let base = fs::read(&path).unwrap();
    let at = base.len();
    let open = |frame: Vec<u8>| {
        let mut bytes = base.clone();
        bytes.extend(frame);
        Database::open_from_bytes(bytes, None)
    };
    let dangling = "graph attachment references a missing or expiring record";
    let violates = "graph frame violates graph state";

    // REQ-013: the record must exist without expiry once the body applied.
    for entries in [
        vec![put_node(0, "a"), attach(0, 0, 9)],
        vec![put_node(0, "a"), attach(0, 0, 2)],
        vec![
            put_node(0, "a"),
            attach(0, 0, 1),
            vector(0, 2, Mutation::Delete { id: 1 }),
        ],
        vec![
            put_node(0, "a"),
            vector(0, 2, Mutation::Delete { id: 1 }),
            attach(0, 0, 1),
        ],
    ] {
        assert_corrupt(open(graph_frame(4, 0, entries)), at, dangling);
    }
    // A record created earlier in the same body may be attached.
    open(graph_frame(
        4,
        0,
        vec![
            put_node(0, "a"),
            vector(0, 2, put(3, &[0.0, 1.0], b"")),
            attach(0, 0, 3),
        ],
    ))
    .unwrap();
    // Replacing an expiring record without expiry makes it attachable.
    open(graph_frame(
        4,
        0,
        vec![
            put_node(0, "a"),
            vector(0, 2, put(2, &[0.0, 1.0], b"")),
            attach(0, 0, 2),
        ],
    ))
    .unwrap();

    // Engine rules: unknown node, duplicate attachment, missing detach,
    // vector entries addressing a missing graph.
    for (graph, entries) in [
        (0, vec![attach(0, 0, 1)]),
        (
            0,
            vec![
                put_node(0, "a"),
                put_node(1, "b"),
                attach(0, 0, 1),
                attach(1, 0, 1),
            ],
        ),
        (0, vec![put_node(0, "a"), detach(0, 0)]),
        (5, vec![vector(0, 2, put(3, &[0.0, 1.0], b""))]),
    ] {
        assert_corrupt(open(graph_frame(4, graph, entries)), at, violates);
    }

    // Vector entries are single op-1/2/4 batch entries; import markers and
    // non-canonical envelopes are body corruption. The engine encoder refuses
    // both, so the encoded body is patched.
    let (count, valid) = body(
        0,
        vec![GraphEntry::VectorMutation {
            collection: 0,
            entry: batch_entry(2, Mutation::Delete { id: 1 }),
        }],
    );
    let op = engine::codec::BODY_HEADER_BYTES + engine::codec::ENTRY_HEADER_BYTES + 4;
    assert_eq!(valid[op..op + 4], [2, 0, 0, 0], "batch-entry header");
    for (index, value) in [(op, 8u8), (op + 1, 1)] {
        let mut patched = valid.clone();
        patched[index] = value;
        assert_corrupt(
            open(frame(
                OP_GRAPH,
                0,
                4,
                0,
                count,
                patched.len() as u32,
                &patched,
            )),
            at,
            "invalid graph frame body",
        );
    }
    let mut nan = batch_entry(2, put(3, &[0.0, 1.0], b""));
    nan[16..20].copy_from_slice(&f32::NAN.to_le_bytes());
    assert!(matches!(
        open(graph_frame(
            4,
            0,
            vec![GraphEntry::VectorMutation {
                collection: 0,
                entry: nan,
            }],
        )),
        Err(Error::Corrupt { offset, .. }) if offset == at as u64
    ));
}

#[test]
fn graph_handles_open_live_graphs_and_respect_read_only() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("handles.vlite");
    fs::write(&path, base_graph_file()).unwrap();
    let mut db = Database::open(&path).unwrap();
    let graph = db.graph("g").unwrap();
    assert_eq!(graph.name(), "g");
    assert_eq!(graph.info(), db.list_graphs().unwrap()[0]);
    assert!(matches!(db.graph("missing"), Err(Error::GraphNotFound)));
    assert!(matches!(db.graph("Bad"), Err(Error::InvalidGraphName)));
    let handle = db.graph_mut("g").unwrap();
    assert_eq!(handle.name(), "g");
    assert_eq!(handle.info().node_count, 0);
    assert!(matches!(db.graph_mut("missing"), Err(Error::GraphNotFound)));
    assert!(matches!(db.graph_mut("Bad"), Err(Error::InvalidGraphName)));
    db.close().unwrap();

    let before = fs::read(&path).unwrap();
    let mut db = Database::open_read_only(&path).unwrap();
    assert_eq!(db.graph("g").unwrap().name(), "g");
    assert!(matches!(db.graph_mut("g"), Err(Error::ReadOnly)));
    drop(db);
    assert_eq!(fs::read(&path).unwrap(), before);

    let plain = dir.path().join("plain.vlite");
    let mut db = Database::create_with_format(&plain, 2, FormatVersion::V5).unwrap();
    assert!(matches!(db.graph("g"), Err(Error::GraphsNotEnabled)));
    assert!(matches!(db.graph_mut("g"), Err(Error::GraphsNotEnabled)));
}

#[test]
fn op12_and_graph_flag_are_rejected_outside_graph_enabled_v5_files() {
    let dir = tempdir().unwrap();
    for format in [FormatVersion::V4, FormatVersion::V5] {
        let path = dir.path().join(format!("op12-{format:?}.vlite"));
        let mut db = Database::create_with_format(&path, 2, format).unwrap();
        db.put(1, &[1.0, 2.0], b"x").unwrap();
        db.close().unwrap();
        let mut bytes = fs::read(&path).unwrap();
        let at = bytes.len();
        bytes.extend(graph_frame(2, 0, vec![create("g")]));
        assert_corrupt(
            Database::open_from_bytes(bytes, None),
            at,
            "invalid operation or reserved record bytes",
        );
    }

    // FLAG_GRAPH in a v4 header is reserved-byte corruption.
    let path = dir.path().join("v4.vlite");
    Database::create_with_format(&path, 2, FormatVersion::V4)
        .unwrap()
        .close()
        .unwrap();
    let mut bytes = fs::read(&path).unwrap();
    bytes[26..28].copy_from_slice(&FLAG_GRAPH.to_le_bytes());
    let checksum = crc32(&bytes[..28]);
    bytes[28..32].copy_from_slice(&checksum.to_le_bytes());
    assert_corrupt(
        Database::open_from_bytes(bytes, None),
        0,
        "nonzero reserved database header bytes",
    );

    // Unknown flags beside FLAG_GRAPH stay unsupported.
    let mut bytes = base_graph_file();
    bytes[26..28].copy_from_slice(&(FLAG_GRAPH | 8).to_le_bytes());
    let checksum = crc32(&bytes[..28]);
    bytes[28..32].copy_from_slice(&checksum.to_le_bytes());
    assert!(matches!(
        Database::open_from_bytes(bytes, None),
        Err(Error::UnsupportedFeature(8))
    ));

    // A graph header with a corrupted header checksum.
    let mut bytes = base_graph_file();
    bytes[29] ^= 1;
    assert_corrupt(
        Database::open_from_bytes(bytes, None),
        0,
        "database header checksum mismatch",
    );
}

#[test]
fn empty_graph_files_are_strictly_verified() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("empty.vlite");
    Database::create_with_graph(&path, 2, CreateOptions::default())
        .unwrap()
        .close()
        .unwrap();
    let bytes = fs::read(&path).unwrap();
    assert_eq!(bytes.len(), HEADER);
    let db = Database::open(&path).unwrap();
    assert_eq!(db.list_graphs().unwrap(), Vec::new());
    drop(db);
    for byte in 0..HEADER {
        let mut bad = bytes.clone();
        bad[byte] ^= 0x10;
        assert!(
            Database::open_from_bytes(bad, None).is_err(),
            "header byte {byte} flip accepted"
        );
    }
    // Clearing the flag (with a valid CRC) yields an ordinary file.
    let mut plain = bytes.clone();
    plain[26..28].copy_from_slice(&0u16.to_le_bytes());
    let checksum = crc32(&plain[..28]);
    plain[28..32].copy_from_slice(&checksum.to_le_bytes());
    assert!(
        !Database::open_from_bytes(plain, None)
            .unwrap()
            .is_graph_enabled()
    );
}

#[test]
fn encrypted_graph_frames_are_sealed_and_authenticated() {
    let dir = tempdir().unwrap();
    let name = "confidentialgraphname";
    for encrypted in [false, true] {
        let path = dir.path().join(format!("leak-{encrypted}.vlite"));
        let options = CreateOptions {
            encryption: encrypted.then(key),
            ..CreateOptions::default()
        };
        let mut db = Database::create_with_graph(&path, 2, options).unwrap();
        db.create_graph(name).unwrap();
        db.close().unwrap();
        let bytes = fs::read(&path).unwrap();
        let leaked = bytes
            .windows(name.len())
            .any(|window| window == name.as_bytes());
        // TEST-009: the name is visible only in the unencrypted control.
        assert_eq!(leaked, !encrypted);
        if !encrypted {
            continue;
        }
        assert!(matches!(
            Database::open(&path),
            Err(Error::EncryptionKeyRequired)
        ));
        assert!(matches!(
            Database::open_encrypted(&path, &EncryptionKey::from_bytes([8; 32])),
            Err(Error::EncryptionKeyMismatch { .. })
        ));
        // Tamper with the sealed body and repair the footer CRC: the
        // authentication tag still rejects the frame.
        let start = HEADER + 64;
        let mut bad = bytes.clone();
        bad[start + PREFIX + 3] ^= 1;
        let end = bad.len() - FOOTER;
        let checksum = crc32(&bad[start..end]);
        bad[end + 4..end + 8].copy_from_slice(&checksum.to_le_bytes());
        assert_corrupt(
            Database::open_from_bytes(bad, Some(&key())),
            start,
            "frame authentication failed",
        );
    }
}

#[test]
fn incomplete_graph_tail_is_rejected_then_explicitly_repaired() {
    let dir = tempdir().unwrap();
    let base = base_graph_file();
    let mut bytes = base.clone();
    let frame = graph_frame(3, 0, vec![put_node(0, "a")]);
    bytes.extend_from_slice(&frame[..frame.len() - 1]);
    let path = dir.path().join("tail.vlite");
    fs::write(&path, &bytes).unwrap();
    assert!(matches!(
        Database::open(&path),
        Err(Error::IncompleteTail { .. })
    ));
    let report = Database::discard_incomplete_tail(&path).unwrap();
    assert_eq!(report.discarded_bytes, frame.len() as u64 - 1);
    assert_eq!(report.retained_commits, 2);
    assert_eq!(fs::read(&path).unwrap(), base);
    let db = Database::open(&path).unwrap();
    assert_eq!(db.list_graphs().unwrap()[0].node_count, 0);
}

/// EPIC-006: the formerly graph-unaware paths carry graph files (LEG-009,
/// LEG-011, LEG-014, LEG-022, LEG-023) or reject them explicitly (LEG-010,
/// LEG-013), and the source never changes. TEST-008
/// (`tests\graph_maintenance.rs`) covers each path in depth.
#[test]
fn maintenance_replicas_and_feeds_handle_graph_files_explicitly() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("source.vlite");
    let bytes = base_graph_file();
    fs::write(&path, &bytes).unwrap();
    let out = |name: &str| dir.path().join(format!("{name}.vlite"));

    assert_eq!(maintenance::inspect(&path).unwrap().commits, 2);
    assert_eq!(
        maintenance::verify(&path).unwrap().bytes,
        bytes.len() as u64
    );
    maintenance::backup(&path, out("backup")).unwrap();
    assert_eq!(fs::read(out("backup")).unwrap(), bytes);
    maintenance::compact(&path, out("compact")).unwrap();
    assert_eq!(header_flags(&fs::read(out("compact")).unwrap()), FLAG_GRAPH);
    let compacted = Database::open(out("compact")).unwrap();
    assert_eq!(compacted.list_graphs().unwrap()[0].name, "g");
    drop(compacted);
    assert!(matches!(
        maintenance::upgrade(&path, out("upgrade")),
        Err(Error::GraphUnsupportedOperation)
    ));
    assert!(!out("upgrade").exists());
    let segment = maintenance::export_log_segment(&path, None, 0, 1 << 20).unwrap();
    assert_eq!([segment.header.as_slice(), &segment.frames].concat(), bytes);
    let start = FeedCursor {
        lineage: 0,
        sequence: 0,
    };
    assert!(matches!(
        read_log_changes(&path, None, &start, 16),
        Err(Error::GraphChangeFeedUnsupported)
    ));
    drop(ReplicaFollower::open(&path, None).unwrap());

    let db = Database::open(&path).unwrap();
    assert_eq!(db.export_log(0, 1 << 20).unwrap(), segment);
    assert!(matches!(
        db.feed_cursor(),
        Err(Error::GraphChangeFeedUnsupported)
    ));
    assert!(matches!(
        db.changes_after(&start, 16),
        Err(Error::GraphChangeFeedUnsupported)
    ));
    drop(db);

    // A follower discards a torn graph frame like any torn frame (LEG-022).
    let tail = out("tail");
    let frame = graph_frame(3, 0, vec![put_node(0, "a")]);
    fs::write(&tail, [bytes.clone(), frame[..10].to_vec()].concat()).unwrap();
    let follower = ReplicaFollower::open(&tail, None).unwrap();
    assert_eq!(follower.applied_sequence(), 2);
    drop(follower);
    assert_eq!(fs::read(&tail).unwrap(), bytes);
    assert_eq!(fs::read(&path).unwrap(), bytes);
}
