use super::*;
use crate::compact::{Activation, GraphBlocks};
use crate::crypto::{Cipher, EncryptionKey};
use crate::format::{self, FLAG_GRAPH, FLAG_PRECISION};
use crate::property_graph::engine::codec::TemporalEntry;
use crate::property_graph::engine::temporal::Segment;
use crate::property_graph::engine::{
    Direction, EdgeId, GraphEntry, GraphId, GraphSchema, LabelDefinition, Multiplicity, NodeId,
    Properties, PropertyDefinition, PropertyIndexKind, PropertyType, PropertyValue,
    RelationshipDefinition,
};
use crate::property_graph::{Neighbors, NodeSelector, ReadOptions};
use crate::{Database, Metadata};
use sha2::Digest;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;

const LINEAGE: u64 = 0x5EED_0000_0000_0042;
const FLOOR: u64 = 4;

fn loaded() -> format::Loaded {
    let header = format::header_v5_flags(2, LINEAGE, 0, FLAG_GRAPH, None);
    let mut loaded = format::load_with(&mut Cursor::new(header), None).unwrap();
    loaded.sequence = FLOOR;
    loaded.collections.get_mut(&0).unwrap().precision =
        Some(crate::compact::catalog::Precision::Float32);
    let records = loaded.collections.get_mut(&0).unwrap();
    records.put(
        4,
        crate::Record {
            vector: vec![1.0, 0.0],
            payload: b"attached".to_vec(),
            metadata: Metadata::new(),
        },
        1,
        None,
    );

    let mut graph = crate::property_graph::engine::GraphState::new();
    let properties =
        |value| Properties::from([("rank".to_string(), PropertyValue::Integer(value))]);
    graph
        .apply(
            1,
            &crate::property_graph::engine::GraphBody {
                graph_id: GraphId(0),
                entries: vec![
                    GraphEntry::CreateGraph {
                        name: "sparse".into(),
                    },
                    GraphEntry::PutNode {
                        node_id: NodeId(0),
                        key: "n0".into(),
                        labels: BTreeSet::from(["Person".to_string()]),
                        properties: properties(1),
                    },
                    GraphEntry::PutNode {
                        node_id: NodeId(42),
                        key: "n42".into(),
                        labels: BTreeSet::from(["Person".to_string()]),
                        properties: properties(42),
                    },
                    GraphEntry::NodeExpiry {
                        node_id: NodeId(42),
                        expires_at_ms: Some(100),
                    },
                    GraphEntry::PutNode {
                        node_id: NodeId(100),
                        key: "expired".into(),
                        labels: BTreeSet::from(["Expired".to_string()]),
                        properties: Properties::new(),
                    },
                    GraphEntry::NodeExpiry {
                        node_id: NodeId(100),
                        expires_at_ms: Some(5),
                    },
                    GraphEntry::PutNode {
                        node_id: NodeId(101),
                        key: "zero-deadline".into(),
                        labels: BTreeSet::from(["Expired".to_string()]),
                        properties: Properties::new(),
                    },
                    GraphEntry::NodeExpiry {
                        node_id: NodeId(101),
                        expires_at_ms: Some(0),
                    },
                    GraphEntry::PutEdge {
                        edge_id: EdgeId(7),
                        source: NodeId(0),
                        destination: NodeId(42),
                        relationship: "R".into(),
                        key: Some("parallel-a".into()),
                        properties: Properties::new(),
                    },
                    GraphEntry::PutEdge {
                        edge_id: EdgeId(8),
                        source: NodeId(0),
                        destination: NodeId(42),
                        relationship: "R".into(),
                        key: Some("parallel-b".into()),
                        properties: Properties::new(),
                    },
                    GraphEntry::PutEdge {
                        edge_id: EdgeId(9),
                        source: NodeId(42),
                        destination: NodeId(42),
                        relationship: "R".into(),
                        key: Some("self".into()),
                        properties: Properties::new(),
                    },
                    GraphEntry::PutEdge {
                        edge_id: EdgeId(10),
                        source: NodeId(100),
                        destination: NodeId(42),
                        relationship: "R".into(),
                        key: Some("cascade".into()),
                        properties: Properties::new(),
                    },
                    GraphEntry::PutEdge {
                        edge_id: EdgeId(11),
                        source: NodeId(0),
                        destination: NodeId(42),
                        relationship: "R".into(),
                        key: Some("expired-edge".into()),
                        properties: Properties::new(),
                    },
                    GraphEntry::EdgeExpiry {
                        edge_id: EdgeId(11),
                        expires_at_ms: Some(5),
                    },
                    GraphEntry::PutEdge {
                        edge_id: EdgeId(12),
                        source: NodeId(101),
                        destination: NodeId(42),
                        relationship: "R".into(),
                        key: Some("zero-deadline".into()),
                        properties: Properties::new(),
                    },
                    GraphEntry::EdgeExpiry {
                        edge_id: EdgeId(12),
                        expires_at_ms: Some(0),
                    },
                    GraphEntry::PutEdge {
                        edge_id: EdgeId(14),
                        source: NodeId(0),
                        destination: NodeId(100),
                        relationship: "R".into(),
                        key: Some("expired-destination".into()),
                        properties: Properties::new(),
                    },
                    GraphEntry::Attach {
                        node_id: NodeId(42),
                        collection: 0,
                        record_id: 4,
                    },
                    GraphEntry::GraphHighWater {
                        next_node_id: 102,
                        next_edge_id: 15,
                    },
                ],
            },
        )
        .unwrap();
    graph
        .apply(
            2,
            &crate::property_graph::engine::GraphBody {
                graph_id: GraphId(3),
                entries: vec![
                    GraphEntry::CreateGraph {
                        name: "empty".into(),
                    },
                    GraphEntry::PutSchema {
                        schema: GraphSchema::default()
                            .label(
                                "Person",
                                LabelDefinition::new("people").property(
                                    "rank",
                                    PropertyDefinition::new(PropertyType::Integer)
                                        .required()
                                        .index(PropertyIndexKind::Range),
                                ),
                            )
                            .relationship(
                                "R",
                                RelationshipDefinition::new("relationship")
                                    .endpoint("Person", "Person")
                                    .multiplicity(Multiplicity::Multi),
                            ),
                    },
                    GraphEntry::GraphHighWater {
                        next_node_id: 0,
                        next_edge_id: 0,
                    },
                ],
            },
        )
        .unwrap();
    loaded.graph = Some(graph);
    loaded
}

fn loaded_temporal() -> format::Loaded {
    let mut loaded = loaded();
    let mut graph = loaded.graph.take().unwrap();
    graph.set_temporal(true).unwrap();
    graph
        .apply(
            3,
            &crate::property_graph::engine::GraphBody {
                graph_id: GraphId(0),
                entries: vec![
                    GraphEntry::Temporal(TemporalEntry::PutEdge {
                        edge_id: EdgeId(15),
                        revision: 0,
                        source: NodeId(0),
                        destination: NodeId(42),
                        relationship: "R".into(),
                        key: Some("temporal".into()),
                        segments: vec![
                            Segment {
                                valid_from: None,
                                valid_to: Some(10),
                                properties: Properties::from([(
                                    "rank".into(),
                                    PropertyValue::Integer(1),
                                )]),
                            },
                            Segment {
                                valid_from: Some(10),
                                valid_to: None,
                                properties: Properties::from([(
                                    "rank".into(),
                                    PropertyValue::Integer(2),
                                )]),
                            },
                        ],
                    }),
                    GraphEntry::GraphHighWater {
                        next_node_id: 102,
                        next_edge_id: 16,
                    },
                ],
            },
        )
        .unwrap();
    loaded.graph = Some(graph);
    loaded
}

fn image_with_flags(source: &format::Loaded, graph_blocks: bool, extra_flags: u16) -> Vec<u8> {
    image_with_graph_clock(source, graph_blocks, extra_flags, Some(5))
}

fn image_with_graph_clock(
    source: &format::Loaded,
    graph_blocks: bool,
    extra_flags: u16,
    graph_now_ms: Option<u64>,
) -> Vec<u8> {
    let _precision = Activation::on();
    let _graph_blocks = GraphBlocks::set(graph_blocks);
    let _temporal = crate::property_graph::temporal::Activation::on();
    let mut bytes = Vec::new();
    crate::maintenance::write_v5_image(
        &mut bytes,
        source,
        LINEAGE,
        None,
        graph_now_ms,
        None,
        FLAG_GRAPH | FLAG_PRECISION | extra_flags,
        FLOOR,
    )
    .unwrap();
    bytes
}

fn image(source: &format::Loaded, graph_blocks: bool) -> Vec<u8> {
    image_with_flags(source, graph_blocks, 0)
}

fn encrypted_image(source: &format::Loaded, graph_blocks: bool, key: &EncryptionKey) -> Vec<u8> {
    let _precision = Activation::on();
    let _graph_blocks = GraphBlocks::set(graph_blocks);
    let cipher = Cipher::create(key).unwrap();
    let mut bytes = Vec::new();
    crate::maintenance::write_v5_image(
        &mut bytes,
        source,
        LINEAGE,
        None,
        Some(5),
        Some(&cipher),
        FLAG_GRAPH | FLAG_PRECISION,
        FLOOR,
    )
    .unwrap();
    bytes
}

fn open(path: &std::path::Path) -> Database {
    Database::open(path).unwrap()
}

fn split_image(bytes: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
    let _precision = Activation::on();
    let _graph_blocks = GraphBlocks::on();
    let mut input = Cursor::new(bytes);
    let header = format::read_log_header(&mut input, None).unwrap();
    let mut offset = header.start;
    let mut frames = Vec::new();
    while let Some(frame) = format::next_frame(&mut input, &header, offset).unwrap() {
        offset += frame.disk.len() as u64;
        frames.push(frame.disk);
    }
    assert_eq!(offset, header.length);
    (bytes[..header.start as usize].to_vec(), frames)
}

fn complete_image(header: &[u8], mut frames: Vec<Vec<u8>>) -> Vec<u8> {
    if let Some(end) = frames.iter().position(|frame| {
        frame[4] == format::OP_COMPACT_BLOCK
            && u64::from_le_bytes(frame[16..24].try_into().unwrap()) as u16
                == crate::compact::KIND_END
    }) {
        frames.truncate(end);
    }
    let mut digest = sha2::Sha256::new();
    for frame in &frames {
        digest.update(&frame[..format::PREFIX_SIZE]);
    }
    frames.push(crate::compact::end_frame(
        FLOOR,
        frames.len() as u64,
        digest.finalize().into(),
    ));
    let mut image = header.to_vec();
    for frame in frames {
        image.extend_from_slice(&frame);
    }
    image
}

fn fix_frame_checksums(frame: &mut [u8]) {
    let header_crc = format::crc32(&frame[..32]);
    frame[32..36].copy_from_slice(&header_crc.to_le_bytes());
    let footer_crc = format::crc32(&frame[..frame.len() - 16]);
    let footer_at = frame.len() - 12;
    frame[footer_at..footer_at + 4].copy_from_slice(&footer_crc.to_le_bytes());
}

fn assert_corrupt<T>(result: Result<T>, expected: &str) {
    match result {
        Err(Error::Corrupt { reason, .. }) => assert_eq!(reason, expected),
        Err(other) => panic!("expected Corrupt({expected:?}), got {other:?}"),
        Ok(_) => panic!("expected Corrupt({expected:?}), got success"),
    }
}

fn is_graph_block(frame: &[u8]) -> bool {
    frame[4] == format::OP_COMPACT_BLOCK
        && u64::from_le_bytes(frame[16..24].try_into().unwrap()) as u16 == KIND
}

fn golden() -> Vec<u8> {
    include_str!("../../../../tests/fixtures/compact/graph-base-v1.hex")
        .split_whitespace()
        .map(|byte| u8::from_str_radix(byte, 16).unwrap())
        .collect()
}

fn golden_state() -> GraphState {
    let bytes = golden();
    let envelope = crate::compact::Envelope::parse(0, u64::from(KIND), &bytes, 0).unwrap();
    let mut pending = frame::PendingSnapshot::new(1, 0);
    for body in graph_keys(0, 0, envelope.sections[0], 1, 0).unwrap() {
        pending.push(body);
    }
    pending
        .finish(&BTreeMap::<u16, format::LoadedCollection>::new())
        .unwrap()
}

// Stop at the first graph base, before any later SnapshotBuilder can mask
// the graph-base admission check under test.
fn check_through_first_graph_base(bytes: &[u8]) -> Result<()> {
    let mut input = Cursor::new(bytes);
    let header = format::read_log_header(&mut input, None)?;
    let mut check = crate::compact::ImageCheck::new(
        header.version,
        header.flags,
        header.dimensions,
        header.collection_floor,
    )
    .unwrap();
    let mut offset = header.start;
    while let Some(frame) =
        format::next_checked_frame(&mut input, &header, offset, Some(&mut check))?
    {
        check.frame(&frame, offset)?;
        if is_graph_block(&frame.disk) {
            return Ok(());
        }
        offset += frame.disk.len() as u64;
    }
    panic!("image has no graph base");
}

#[test]
fn directory_worker_checkpoints_block_and_op15_sources_with_read_parity() {
    directory_worker_parity(true);
}

#[test]
fn s3_directory_gate_off() {
    directory_worker_parity(false);
}

fn directory_worker_parity(checkpoint_gate: bool) {
    let _precision = Activation::on();
    let _graph_blocks = GraphBlocks::on();
    let mut source = format::load_with(&mut Cursor::new(image(&loaded(), true)), None).unwrap();
    source
        .graph
        .as_mut()
        .unwrap()
        .apply_at(
            FLOOR,
            &GraphBody {
                graph_id: GraphId(0),
                entries: vec![GraphEntry::NodeExpiry {
                    node_id: NodeId(42),
                    expires_at_ms: None,
                }],
            },
            true,
        )
        .unwrap();
    for blocks_at_creation in [true, false]
        .into_iter()
        .filter(|blocks| checkpoint_gate || !blocks)
    {
        let directory = tempfile::tempdir().unwrap();
        let store = directory.path().join("store");
        {
            let _gate = GraphBlocks::set(blocks_at_creation);
            let chain = crate::directory::create_converted(&store, &source).unwrap();
            chain.unlock().unwrap();
        }
        let initial = std::fs::read(store.join("image-1.vlite")).unwrap();
        assert_eq!(
            split_image(&initial).1.iter().any(|f| is_graph_block(f)),
            blocks_at_creation
        );
        let mut db = open(&store);
        db.set_checkpoint_threshold(None);
        db.graph_mut("sparse")
            .unwrap()
            .create_node("tail", &["Person"], &Metadata::new())
            .unwrap();
        let before_state = db.graph.clone();
        let initial_state = std::mem::replace(&mut source.graph, before_state);
        let initial_sequence = source.sequence;
        source.sequence = db.sequence;
        let mut reference = Vec::new();
        {
            let _gate = GraphBlocks::set(false);
            crate::maintenance::write_v5_image(
                &mut reference,
                &source,
                LINEAGE,
                None,
                None,
                None,
                FLAG_GRAPH | FLAG_PRECISION,
                db.sequence,
            )
            .unwrap();
        }
        source.graph = initial_state;
        source.sequence = initial_sequence;
        let reference_state = format::load_with(&mut Cursor::new(reference), None)
            .unwrap()
            .graph;
        let before_node = db.graph("sparse").unwrap().get_node("n42").unwrap().output;
        let mut neighbors = Neighbors::new(NodeSelector::Id(42));
        neighbors.direction = Direction::Both;
        let before_neighbors = db
            .graph("sparse")
            .unwrap()
            .neighbors(&neighbors, &ReadOptions::default())
            .unwrap()
            .output;
        let traverse = crate::property_graph::Traverse::new(vec![NodeSelector::Id(0)]);
        let before_traverse = db
            .graph("sparse")
            .unwrap()
            .traverse(&traverse, &ReadOptions::default())
            .unwrap()
            .output;
        {
            let _gate = GraphBlocks::set(checkpoint_gate);
            assert!(db.checkpoint().unwrap().is_some());
        }
        db.close().unwrap();
        let manifest = std::fs::read_to_string(store.join("MANIFEST")).unwrap();
        let image_name = manifest
            .lines()
            .find_map(|line| line.strip_prefix("file image-"))
            .unwrap();
        let image = std::fs::read(store.join(format!("image-{image_name}"))).unwrap();
        assert_eq!(
            split_image(&image).1.iter().any(|f| is_graph_block(f)),
            checkpoint_gate
        );
        if !checkpoint_gate {
            assert!(
                split_image(&image)
                    .1
                    .iter()
                    .any(|f| f[4] == format::OP_GRAPH_SNAPSHOT)
            );
        }
        assert_eq!(
            format::load_with(&mut Cursor::new(image), None)
                .unwrap()
                .graph,
            reference_state
        );
        let reopened = open(&store);
        assert_eq!(reopened.graph, reference_state);
        assert_eq!(
            reopened
                .graph("sparse")
                .unwrap()
                .get_node("n42")
                .unwrap()
                .output,
            before_node
        );
        assert_eq!(
            reopened
                .graph("sparse")
                .unwrap()
                .neighbors(&neighbors, &ReadOptions::default())
                .unwrap()
                .output,
            before_neighbors
        );
        assert_eq!(
            reopened
                .graph("sparse")
                .unwrap()
                .traverse(&traverse, &ReadOptions::default())
                .unwrap()
                .output,
            before_traverse
        );
        assert!(
            reopened
                .graph("sparse")
                .unwrap()
                .get_node("tail")
                .unwrap()
                .output
                .is_some()
        );
    }
}

#[test]
fn s1_later_graph_preflight() {
    let mut source = loaded();
    let mut entries = (0..32)
        .map(|id| GraphEntry::PutNode {
            node_id: NodeId(id),
            key: format!("later-{id}"),
            labels: BTreeSet::new(),
            properties: Properties::new(),
        })
        .collect::<Vec<_>>();
    entries.push(GraphEntry::GraphHighWater {
        next_node_id: 32,
        next_edge_id: 0,
    });
    source
        .graph
        .as_mut()
        .unwrap()
        .apply(
            3,
            &GraphBody {
                graph_id: GraphId(3),
                entries,
            },
        )
        .unwrap();
    let state = source.graph.as_ref().unwrap();
    let first = encode_graph(state.graph(GraphId(0)).unwrap(), Some(5))
        .unwrap()
        .1
        .len();
    let later = encode_graph(state.graph(GraphId(3)).unwrap(), Some(5))
        .unwrap()
        .1
        .len();
    assert!(first < later);
    let _precision = Activation::on();
    let _gate = GraphBlocks::on();
    let write = |output: &mut Vec<u8>| {
        crate::maintenance::write_v5_image(
            output,
            &source,
            LINEAGE,
            None,
            Some(5),
            None,
            FLAG_GRAPH | FLAG_PRECISION,
            FLOOR,
        )
    };
    {
        let _limit = BodyLimit::set(later);
        let mut output = Vec::new();
        write(&mut output).unwrap();
        let actual = format::load_with(&mut Cursor::new(output), None).unwrap();
        let reference = format::load_with(&mut Cursor::new(image(&source, false)), None).unwrap();
        assert_eq!(actual.graph, reference.graph);
    }
    let _limit = BodyLimit::set(later - 1);
    let mut output = Vec::new();
    assert_corrupt(write(&mut output), "graph base block exceeds body cap");
    assert!(output.is_empty(), "later graph must fail before any output");
}

#[test]
fn s2_graph_admission_clauses() {
    let mut source = loaded();
    let flags = FLAG_PRECISION | format::FLAG_HISTORY | FLAG_GRAPH;
    let _precision = Activation::on();
    let _gate = GraphBlocks::on();
    assert!(crate::compact::graph_blocks_for(&source, flags));
    for missing in [FLAG_PRECISION, format::FLAG_HISTORY, FLAG_GRAPH] {
        assert!(
            !crate::compact::graph_blocks_for(&source, flags & !missing),
            "admitted missing flag {missing}"
        );
    }
    {
        let _precision = Activation::set(false);
        assert!(
            !crate::compact::graph_blocks_for(&source, flags),
            "precision activation off"
        );
    }
    {
        let _gate = GraphBlocks::set(false);
        assert!(
            !crate::compact::graph_blocks_for(&source, flags),
            "graph gate off"
        );
    }
    let mut reference = Vec::new();
    {
        let _gate = GraphBlocks::set(false);
        crate::maintenance::write_v5_image(
            &mut reference,
            &source,
            LINEAGE,
            None,
            Some(5),
            None,
            FLAG_GRAPH,
            FLOOR,
        )
        .unwrap();
    }
    {
        let _limit = BodyLimit::set(1);
        let mut output = Vec::new();
        crate::maintenance::write_v5_image(
            &mut output,
            &source,
            LINEAGE,
            None,
            Some(5),
            None,
            FLAG_GRAPH,
            FLOOR,
        )
        .unwrap();
        let frames = split_image(&output).1;
        assert!(!frames.iter().any(|f| is_graph_block(f)));
        assert!(frames.iter().any(|f| f[4] == format::OP_GRAPH_SNAPSHOT));
        let actual = format::load_with(&mut Cursor::new(output), None).unwrap();
        let reference = format::load_with(&mut Cursor::new(reference), None).unwrap();
        assert_eq!(actual.graph, reference.graph);
    }
    source.graph = Some(GraphState::new());
    assert!(
        !crate::compact::graph_blocks_for(&source, flags),
        "zero graphs"
    );
    source.graph = None;
    assert!(
        !crate::compact::graph_blocks_for(&source, flags),
        "no graph state"
    );
}

#[test]
fn s4_reader_body_cap() {
    let mut payload = vec![0; MAX_BODY - HEADER_LEN - SECTION_ENTRY_LEN];
    assert_corrupt(
        graph_keys(0, 0, &payload, FLOOR, 0),
        "invalid graph base payload",
    );
    payload.push(0);
    assert_corrupt(
        graph_keys(0, 0, &payload, FLOOR, 0),
        "graph base block exceeds body cap",
    );
}

#[test]
fn s5_op15_after_base() {
    let _precision = Activation::on();
    let _gate = GraphBlocks::on();
    let source = loaded();
    let (header, mut frames) = split_image(&image(&source, true));
    let snapshot = split_image(&image(&source, false))
        .1
        .into_iter()
        .find(|f| f[4] == format::OP_GRAPH_SNAPSHOT)
        .unwrap();
    let first = frames.iter().position(|f| is_graph_block(f)).unwrap();
    frames.insert(first + 1, snapshot);
    assert_corrupt(
        format::load_with(&mut Cursor::new(complete_image(&header, frames)), None),
        "op-15 snapshot duplicates a graph base block",
    );
}

#[test]
fn s6_other_kind_refused() {
    let _precision = Activation::on();
    let _gate = GraphBlocks::on();
    let (header, mut frames) = split_image(&image(&loaded(), true));
    for frame in &mut frames {
        if frame[4] != format::OP_COMPACT_BLOCK {
            continue;
        }
        let kind = u64::from_le_bytes(frame[16..24].try_into().unwrap()) as u16;
        if kind == crate::compact::KIND_TABLE {
            let end = frame.len() - 16;
            let entry = frame[format::PREFIX_SIZE + 40..end]
                .as_chunks_mut::<8>()
                .0
                .iter_mut()
                .find(|entry| u16::from_le_bytes(entry[..2].try_into().unwrap()) == KIND)
                .unwrap();
            entry[..2].copy_from_slice(&(KIND + 1).to_le_bytes());
            fix_frame_checksums(frame);
        } else if kind == KIND {
            frame[16..24].copy_from_slice(&u64::from(KIND + 1).to_le_bytes());
            frame[format::PREFIX_SIZE..format::PREFIX_SIZE + 2]
                .copy_from_slice(&(KIND + 1).to_le_bytes());
            fix_frame_checksums(frame);
        }
    }
    match format::load_with(&mut Cursor::new(complete_image(&header, frames)), None) {
        Err(Error::UnsupportedFeature(flag)) => assert_eq!(flag, format::FLAG_COMPACT),
        Err(error) => panic!("expected unsupported critical kind, got {error:?}"),
        Ok(_) => panic!("accepted another kind as graph blocks"),
    }
}

#[test]
fn graph_block_requires_graph_and_history_flags_plain_and_encrypted() {
    let _precision = Activation::on();
    let _graph_blocks = GraphBlocks::on();
    let key = EncryptionKey::from_bytes([0x42; 32]);
    for encrypted in [false, true] {
        let bytes = if encrypted {
            encrypted_image(&loaded(), true, &key)
        } else {
            image(&loaded(), true)
        };
        for (flag, reason) in [
            (
                FLAG_GRAPH,
                "graph base block requires history and graph flags",
            ),
            (format::FLAG_HISTORY, "compact flag without a history image"),
        ] {
            let mut bad = bytes.clone();
            let flags = u16::from_le_bytes(bad[26..28].try_into().unwrap()) & !flag;
            bad[26..28].copy_from_slice(&flags.to_le_bytes());
            let crc = format::crc32(&bad[..28]);
            bad[28..32].copy_from_slice(&crc.to_le_bytes());
            assert_corrupt(
                format::load_with(&mut Cursor::new(&bad), encrypted.then_some(&key)),
                reason,
            );
        }
    }
}

#[test]
fn graph_block_rejects_a_valid_extra_section() {
    let _precision = Activation::on();
    let _graph_blocks = GraphBlocks::on();
    let (header, mut frames) = split_image(&image(&loaded(), true));
    let index = frames.iter().position(|f| is_graph_block(f)).unwrap();
    let original = &frames[index];
    let owner = u16::from_le_bytes(original[5..7].try_into().unwrap());
    let count = u32::from_le_bytes(original[24..28].try_into().unwrap());
    let body = &original[format::PREFIX_SIZE..original.len() - 16];
    let payload = crate::compact::Envelope::parse(owner, u64::from(KIND), body, 0)
        .unwrap()
        .sections[0];
    let mut extra = body[..32].to_vec();
    extra[6..8].copy_from_slice(&2u16.to_le_bytes());
    extra[24..28].copy_from_slice(&48u32.to_le_bytes());
    for value in [48, payload.len() as u32, 48 + payload.len() as u32, 1] {
        extra.extend_from_slice(&value.to_le_bytes());
    }
    extra.extend_from_slice(payload);
    extra.push(0x7f);
    assert_eq!(
        crate::compact::Envelope::parse(owner, u64::from(KIND), &extra, 0)
            .unwrap()
            .sections
            .len(),
        2
    );
    frames[index] = format::compact_block_frame(FLOOR, owner, u64::from(KIND), count, &extra);
    assert_corrupt(
        format::load_with(&mut Cursor::new(complete_image(&header, frames)), None),
        "graph base block requires exactly one section",
    );
}

#[test]
fn graph_enabled_zero_graphs_has_no_graph_block_declaration() {
    let mut source = loaded();
    source.graph = Some(GraphState::new());
    let bytes = image(&source, true);
    let reference = image(&source, false);
    let _precision = Activation::on();
    let _graph_blocks = GraphBlocks::on();
    assert_eq!(bytes, reference);
    assert!(!split_image(&bytes).1.iter().any(|f| is_graph_block(f)));
    assert_eq!(
        format::load_with(&mut Cursor::new(bytes), None)
            .unwrap()
            .graph,
        format::load_with(&mut Cursor::new(reference), None)
            .unwrap()
            .graph
    );
}

#[test]
fn graph_preflight_checks_first_graph_and_uses_checkpoint_expiry_clock() {
    let source = loaded();
    let state = source.graph.as_ref().unwrap();
    let first = encode_graph(state.graph(GraphId(0)).unwrap(), Some(5))
        .unwrap()
        .1
        .len();
    let later = encode_graph(state.graph(GraphId(3)).unwrap(), Some(5))
        .unwrap()
        .1
        .len();
    let unpruned = encode_graph(state.graph(GraphId(0)).unwrap(), None)
        .unwrap()
        .1
        .len();
    assert!(139 < later && later < first && first < unpruned);
    let _precision = Activation::on();
    let _graph_blocks = GraphBlocks::on();
    let write = |output: &mut Vec<u8>| {
        crate::maintenance::write_v5_image(
            output,
            &source,
            LINEAGE,
            None,
            Some(5),
            None,
            FLAG_GRAPH | FLAG_PRECISION,
            FLOOR,
        )
    };
    {
        let _limit = BodyLimit::set(later);
        let mut output = Vec::new();
        assert_corrupt(write(&mut output), "graph base block exceeds body cap");
        assert!(output.is_empty(), "first graph must fail before any output");
    }
    let _limit = BodyLimit::set(first);
    let mut output = Vec::new();
    write(&mut output).unwrap();
    let actual = format::load_with(&mut Cursor::new(output), None).unwrap();
    let reference = format::load_with(&mut Cursor::new(image(&source, false)), None).unwrap();
    assert_eq!(actual.graph, reference.graph);
}

#[test]
fn graph_body_limit_accepts_exact_golden_boundary_and_constants_match() {
    assert_eq!(MAX_BODY, 64 << 20);
    assert_eq!(MAX_BODY, crate::compact::MAX_BODY);
    assert_eq!(MAX_BODY, super::super::MAX_BYTES);
    let bytes = golden();
    assert_eq!(bytes.len(), 139);
    let state = golden_state();
    let graph = state.graph(GraphId(0)).unwrap();
    {
        let _limit = BodyLimit::set(bytes.len());
        assert_eq!(encode_graph(graph, None).unwrap().1, bytes);
    }
    let _limit = BodyLimit::set(bytes.len() - 1);
    assert_corrupt(
        encode_graph(graph, None),
        "graph base block exceeds body cap",
    );
}

#[test]
fn graph_kind_codec_version_two_is_unsupported() {
    let _precision = Activation::on();
    let _graph_blocks = GraphBlocks::on();
    let (header, mut frames) = split_image(&image(&loaded(), true));
    let table = frames
        .iter_mut()
        .find(|f| {
            f[4] == format::OP_COMPACT_BLOCK
                && u64::from_le_bytes(f[16..24].try_into().unwrap()) as u16
                    == crate::compact::KIND_TABLE
        })
        .unwrap();
    let start = format::PREFIX_SIZE + 40;
    let end = table.len() - 16;
    let entry = table[start..end]
        .as_chunks_mut::<8>()
        .0
        .iter_mut()
        .find(|entry| u16::from_le_bytes(entry[..2].try_into().unwrap()) == KIND)
        .unwrap();
    entry[2..4].copy_from_slice(&2u16.to_le_bytes());
    fix_frame_checksums(table);
    assert!(matches!(
        format::load_with(&mut Cursor::new(complete_image(&header, frames)), None),
        Err(Error::UnsupportedFeature(format::FLAG_COMPACT))
    ));
}

#[test]
fn mixed_op15_and_graph_base_is_rejected_by_image_check_before_builder() {
    let _precision = Activation::on();
    let _graph_blocks = GraphBlocks::on();
    let source = loaded();
    let (header, mut frames) = split_image(&image(&source, true));
    let (_, reference) = split_image(&image(&source, false));
    let snapshot = reference
        .into_iter()
        .find(|f| f[4] == format::OP_GRAPH_SNAPSHOT)
        .unwrap();
    let index = frames.iter().position(|f| is_graph_block(f)).unwrap();
    frames.insert(index, snapshot);
    assert_corrupt(
        check_through_first_graph_base(&complete_image(&header, frames)),
        "duplicate or out-of-order graph base block",
    );
}

#[test]
fn graph_block_header_count_cannot_understate_entities() {
    let state = loaded().graph.unwrap();
    let (count, body) = encode_graph(state.graph(GraphId(0)).unwrap(), Some(5)).unwrap();
    assert!(count > 0);
    let payload = crate::compact::Envelope::parse(0, u64::from(KIND), &body, 0)
        .unwrap()
        .sections[0];
    assert_corrupt(
        graph_keys(0, count - 1, payload, FLOOR, 0),
        "graph base entity count mismatch",
    );
    assert_corrupt(
        graph_keys(3, count, payload, FLOOR, 0),
        "graph base owner mismatch",
    );
}

#[test]
fn graph_gate_off_and_nonprecision_writers_keep_op15_without_graph_blocks() {
    let source = loaded();
    let _precision = Activation::on();
    let _gate = GraphBlocks::set(false);
    let legacy = image(&source, false);
    assert!(!split_image(&legacy).1.iter().any(|f| is_graph_block(f)));
    assert!(
        split_image(&legacy)
            .1
            .iter()
            .any(|f| f[4] == format::OP_GRAPH_SNAPSHOT)
    );
    format::load_with(&mut Cursor::new(&legacy), None).unwrap();
    let _gate = GraphBlocks::on();
    for flags in [FLAG_GRAPH, 0, FLAG_PRECISION] {
        let mut output = Vec::new();
        crate::maintenance::write_v5_image(
            &mut output,
            &source,
            LINEAGE,
            None,
            Some(5),
            None,
            flags,
            FLOOR,
        )
        .unwrap();
        assert!(!split_image(&output).1.iter().any(|f| is_graph_block(f)));
        let actual = format::load_with(&mut Cursor::new(output), None).unwrap();
        if flags & FLAG_GRAPH == 0 {
            assert!(actual.graph.is_none());
        }
    }
}

#[test]
fn multi_body_graph_load_public_reads_and_feed_seed_all_graphs_keys_and_attachments() {
    let mut source = loaded();
    for id in [5, 6] {
        source.collections.get_mut(&0).unwrap().put(
            id,
            crate::Record {
                vector: vec![1.0, 0.0],
                payload: vec![],
                metadata: Metadata::new(),
            },
            1,
            None,
        );
    }
    let state = source.graph.as_mut().unwrap();
    let mut entries = vec![GraphEntry::NodeExpiry {
        node_id: NodeId(42),
        expires_at_ms: None,
    }];
    entries.extend(
        (102..=4300)
            .map(|id| GraphEntry::PutNode {
                node_id: NodeId(id),
                key: format!("n{id}"),
                labels: BTreeSet::from(["Person".into()]),
                properties: Properties::new(),
            })
            .collect::<Vec<_>>(),
    );
    entries.extend([
        GraphEntry::PutEdge {
            edge_id: EdgeId(15),
            source: NodeId(42),
            destination: NodeId(4300),
            relationship: "R".into(),
            key: Some("last-edge".into()),
            properties: Properties::new(),
        },
        GraphEntry::Attach {
            node_id: NodeId(4300),
            collection: 0,
            record_id: 5,
        },
        GraphEntry::GraphHighWater {
            next_node_id: 4301,
            next_edge_id: 16,
        },
    ]);
    for entries in entries.chunks(4096) {
        state
            .apply_at(
                3,
                &GraphBody {
                    graph_id: GraphId(0),
                    entries: entries.to_vec(),
                },
                true,
            )
            .unwrap();
    }
    let properties = || Properties::from([("rank".into(), PropertyValue::Integer(3))]);
    state
        .apply(
            4,
            &GraphBody {
                graph_id: GraphId(3),
                entries: vec![
                    GraphEntry::PutNode {
                        node_id: NodeId(0),
                        key: "third-a".into(),
                        labels: BTreeSet::from(["Person".into()]),
                        properties: properties(),
                    },
                    GraphEntry::PutNode {
                        node_id: NodeId(1),
                        key: "third-b".into(),
                        labels: BTreeSet::from(["Person".into()]),
                        properties: properties(),
                    },
                    GraphEntry::PutEdge {
                        edge_id: EdgeId(0),
                        source: NodeId(0),
                        destination: NodeId(1),
                        relationship: "R".into(),
                        key: Some("third-edge".into()),
                        properties: Properties::new(),
                    },
                    GraphEntry::Attach {
                        node_id: NodeId(1),
                        collection: 0,
                        record_id: 6,
                    },
                    GraphEntry::GraphHighWater {
                        next_node_id: 2,
                        next_edge_id: 1,
                    },
                ],
            },
        )
        .unwrap();
    let (count, body) = encode_graph(state.graph(GraphId(0)).unwrap(), Some(5)).unwrap();
    let payload = crate::compact::Envelope::parse(0, u64::from(KIND), &body, 0)
        .unwrap()
        .sections[0];
    assert!(graph_keys(0, count, payload, FLOOR, 0).unwrap().len() > 1);
    let candidate = image(&source, true);
    let reference = image(&source, false);
    let _precision = Activation::on();
    let _gate = GraphBlocks::on();
    assert_eq!(
        format::load_with(&mut Cursor::new(&candidate), None)
            .unwrap()
            .graph,
        format::load_with(&mut Cursor::new(&reference), None)
            .unwrap()
            .graph
    );
    let directory = tempfile::tempdir().unwrap();
    let mut databases = Vec::new();
    for (name, bytes) in [
        ("candidate.vlite", candidate),
        ("reference.vlite", reference),
    ] {
        let path = directory.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        databases.push(open(&path));
    }
    for name in ["sparse", "empty"] {
        let a = databases[0].graph(name).unwrap().with_clock(5);
        let b = databases[1].graph(name).unwrap().with_clock(5);
        assert_eq!(a.info(), b.info());
        let (node, edge) = if name == "sparse" {
            ("n4300", "last-edge")
        } else {
            ("third-b", "third-edge")
        };
        assert_eq!(
            a.get_node(node).unwrap().output,
            b.get_node(node).unwrap().output
        );
        assert_eq!(
            a.get_edge_by_key(edge).unwrap().output,
            b.get_edge_by_key(edge).unwrap().output
        );
        let request = Neighbors::new(NodeSelector::Key(node.into()));
        assert_eq!(
            a.neighbors(&request, &ReadOptions::default())
                .unwrap()
                .output,
            b.neighbors(&request, &ReadOptions::default())
                .unwrap()
                .output
        );
    }
    for db in &mut databases {
        for (name, node, edge) in [
            ("sparse", "n4300", "last-edge"),
            ("empty", "third-b", "third-edge"),
        ] {
            let mut graph = db.graph_mut(name).unwrap();
            graph.detach(node, "default").unwrap();
            graph
                .delete_edge(
                    crate::property_graph::EdgeRef::Key(edge.into()),
                    crate::Expected::Any,
                )
                .unwrap();
        }
    }
    let cursor = crate::changes::FeedCursor {
        lineage: LINEAGE,
        sequence: FLOOR,
    };
    let actual = databases[0].changes_after(&cursor, 100).unwrap();
    let expected = databases[1].changes_after(&cursor, 100).unwrap();
    assert_eq!(actual, expected);
    assert_eq!(actual.changes.len(), 4);
    for (index, graph, node, key, record, edge, edge_key) in [
        (0, "sparse", 4300, "n4300", 5, 15, "last-edge"),
        (2, "empty", 1, "third-b", 6, 0, "third-edge"),
    ] {
        assert_eq!(
            actual.changes[index].kind,
            crate::changes::ChangeKind::GraphBatch {
                graph: graph.into(),
                revision: FLOOR + index as u64 + 1,
                operations: vec![crate::changes::GraphChange::Detached {
                    node,
                    node_key: key.into(),
                    collection: "default".into(),
                    record,
                }],
            }
        );
        assert_eq!(
            actual.changes[index + 1].kind,
            crate::changes::ChangeKind::GraphBatch {
                graph: graph.into(),
                revision: FLOOR + index as u64 + 2,
                operations: vec![crate::changes::GraphChange::EdgeDeleted {
                    id: edge,
                    key: Some(edge_key.into()),
                }],
            }
        );
    }
}

#[test]
fn graph_block_matches_op15_snapshot_and_seeds_change_feed_keys() {
    let source = loaded();
    let snapshot = image(&source, false);
    let compact = image(&source, true);

    let _precision = Activation::on();
    let _graph_blocks = GraphBlocks::on();
    let reference = format::load_with(&mut Cursor::new(&snapshot), None).unwrap();
    let actual = format::load_with(&mut Cursor::new(&compact), None).unwrap();
    assert_eq!(actual.graph, reference.graph);
    let graph = actual.graph.as_ref().unwrap();
    assert_eq!(graph.graphs().count(), 2);
    graph.check_invariants().unwrap();
    assert_eq!(
        graph
            .graph(GraphId(0))
            .unwrap()
            .node_by_key("n42")
            .unwrap()
            .id
            .0,
        42
    );
    assert!(
        graph
            .graph(GraphId(0))
            .unwrap()
            .node_by_key("expired")
            .is_none()
    );
    assert!(
        graph
            .graph(GraphId(0))
            .unwrap()
            .edge_by_key("cascade")
            .is_none()
    );
    assert!(
        graph
            .graph(GraphId(0))
            .unwrap()
            .edge_by_key("expired-edge")
            .is_none()
    );
    assert!(
        graph
            .graph(GraphId(0))
            .unwrap()
            .node_by_key("zero-deadline")
            .is_none()
    );
    assert!(
        graph
            .graph(GraphId(0))
            .unwrap()
            .edge_by_key("zero-deadline")
            .is_none()
    );
    assert_eq!(
        graph.graph(GraphId(3)).unwrap().schema(),
        source
            .graph
            .as_ref()
            .unwrap()
            .graph(GraphId(3))
            .unwrap()
            .schema()
    );
    assert_eq!(
        graph
            .graph(GraphId(0))
            .unwrap()
            .attachments(NodeId(42))
            .count(),
        1
    );

    let directory = tempfile::tempdir().unwrap();
    let candidate = directory.path().join("graph-block.vlite");
    let legacy = directory.path().join("op15.vlite");
    std::fs::write(&candidate, &compact).unwrap();
    std::fs::write(&legacy, &snapshot).unwrap();
    let mut candidate_db = open(&candidate);
    let mut legacy_db = open(&legacy);
    for db in [&candidate_db, &legacy_db] {
        let graph = db.graph("sparse").unwrap().with_clock(5);
        assert_eq!(graph.info().node_count, 2);
        assert_eq!(graph.info().edge_count, 3);
        assert_eq!(graph.get_node_by_id(42).unwrap().output.unwrap().key, "n42");
        let mut request = Neighbors::new(NodeSelector::Id(42));
        request.direction = Direction::Both;
        let result = graph
            .neighbors(&request, &ReadOptions::default())
            .unwrap()
            .output;
        assert_eq!(
            result.edges.iter().map(|edge| edge.id).collect::<Vec<_>>(),
            [7, 8, 9]
        );
    }

    let lineage = candidate_db.lineage().unwrap();
    for db in [&mut candidate_db, &mut legacy_db] {
        let mut graph = db.graph_mut("sparse").unwrap();
        graph
            .create_node("tail", &["Person"], &Metadata::new())
            .unwrap();
        graph
            .create_edge(Some("after-image"), "n0", "R", "tail", &Metadata::new())
            .unwrap();
    }
    let cursor = crate::changes::FeedCursor {
        lineage,
        sequence: FLOOR,
    };
    assert_eq!(
        candidate_db.changes_after(&cursor, 100).unwrap(),
        legacy_db.changes_after(&cursor, 100).unwrap()
    );

    let _disabled = GraphBlocks::set(false);
    assert!(matches!(
        format::load_with(&mut Cursor::new(&compact), None),
        Err(crate::Error::UnsupportedFeature(format::FLAG_COMPACT))
    ));
}

#[test]
fn graph_block_preserves_temporal_series_against_op15_snapshot() {
    let source = loaded_temporal();
    let snapshot = image_with_flags(&source, false, format::FLAG_TEMPORAL);
    let compact = image_with_flags(&source, true, format::FLAG_TEMPORAL);

    let _precision = Activation::on();
    let _graph_blocks = GraphBlocks::on();
    let _temporal = crate::property_graph::temporal::Activation::on();
    let reference = format::load_with(&mut Cursor::new(snapshot), None).unwrap();
    let actual = format::load_with(&mut Cursor::new(compact), None).unwrap();
    assert_eq!(actual.graph, reference.graph);
    let actual_graph = actual.graph.as_ref().unwrap();
    let reference_graph = reference.graph.as_ref().unwrap();
    assert!(actual_graph.is_temporal());
    assert_eq!(
        actual_graph.graph(GraphId(0)).unwrap().segments(EdgeId(15)),
        reference_graph
            .graph(GraphId(0))
            .unwrap()
            .segments(EdgeId(15))
    );
}

#[test]
fn encrypted_graph_block_matches_op15_snapshot() {
    let source = loaded();
    let key = EncryptionKey::from_bytes([0x5a; 32]);
    let destination_key = EncryptionKey::from_bytes([0xa5; 32]);
    let snapshot = encrypted_image(&source, false, &key);
    let compact = encrypted_image(&source, true, &key);
    let _precision = Activation::on();
    let _graph_blocks = GraphBlocks::on();
    let reference = format::load_with(&mut Cursor::new(snapshot), Some(&key)).unwrap();
    let actual = format::load_with(&mut Cursor::new(compact), Some(&key)).unwrap();
    assert_eq!(actual.graph, reference.graph);

    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("encrypted.vlite");
    let destination_path = directory.path().join("reencrypted.vlite");
    std::fs::write(&source_path, encrypted_image(&source, true, &key)).unwrap();
    crate::maintenance::reencrypt(
        &source_path,
        Some(&key),
        &destination_path,
        Some(&destination_key),
    )
    .unwrap();
    let reencrypted = format::load_with(
        &mut Cursor::new(std::fs::read(destination_path).unwrap()),
        Some(&destination_key),
    )
    .unwrap();
    assert_eq!(reencrypted.graph, reference.graph);
}

#[test]
fn directory_checkpoint_writes_and_reopens_graph_blocks() {
    let mut source = loaded();
    source.sequence = FLOOR;
    source.flags |= FLAG_PRECISION;
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("store");
    let _precision = Activation::on();
    let _graph_blocks = GraphBlocks::on();
    let converted = crate::directory::create_converted(&store, &source).unwrap();
    converted.unlock().unwrap();
    drop(converted);

    let image = std::fs::read(store.join("image-1.vlite")).unwrap();
    let _precision = Activation::on();
    let _graph_blocks = GraphBlocks::on();
    let reopened = format::load_with(&mut Cursor::new(image), None).unwrap();
    let reference = image_with_graph_clock(&source, false, 0, None);
    let reference = format::load_with(&mut Cursor::new(reference), None).unwrap();
    assert_eq!(reopened.graph, reference.graph);
    let database = open(&store);
    let graph = database.graph("sparse").unwrap().with_clock(5);
    assert_eq!(graph.info().node_count, 2);
    assert_eq!(graph.info().edge_count, 3);
}

#[test]
fn precision_conversion_and_byte_copy_maintenance_preserve_graph_blocks() {
    let source = loaded();
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("source.vlite");
    let candidate_path = directory.path().join("candidate.vlite");
    let backup_path = directory.path().join("backup.vlite");
    let restored_path = directory.path().join("restored.vlite");
    let mut source_bytes = Vec::new();
    crate::maintenance::write_v5_image(
        &mut source_bytes,
        &source,
        LINEAGE,
        None,
        None,
        None,
        FLAG_GRAPH,
        0,
    )
    .unwrap();
    std::fs::write(&source_path, source_bytes).unwrap();

    let _precision = Activation::on();
    let _graph_blocks = GraphBlocks::on();
    crate::maintenance::convert_precision(
        &source_path,
        &candidate_path,
        None,
        &[("default", crate::compact::catalog::Precision::Float32)],
        false,
    )
    .unwrap();

    let candidate_bytes = std::fs::read(&candidate_path).unwrap();
    let candidate = format::load_with(&mut Cursor::new(&candidate_bytes), None).unwrap();
    let mut source_loaded =
        format::load_with(&mut Cursor::new(std::fs::read(&source_path).unwrap()), None).unwrap();
    source_loaded.collections.get_mut(&0).unwrap().precision =
        Some(crate::compact::catalog::Precision::Float32);
    let reference_bytes = image_with_graph_clock(&source_loaded, false, 0, None);
    let reference = format::load_with(&mut Cursor::new(reference_bytes), None).unwrap();
    assert_eq!(candidate.graph, reference.graph);

    crate::maintenance::backup(&candidate_path, &backup_path).unwrap();
    assert_eq!(std::fs::read(&backup_path).unwrap(), candidate_bytes);
    crate::maintenance::restore(&backup_path, &restored_path).unwrap();
    let restored_bytes = std::fs::read(&restored_path).unwrap();
    let mut candidate_input = Cursor::new(candidate_bytes);
    let candidate_header = format::read_log_header(&mut candidate_input, None).unwrap();
    assert_eq!(
        &restored_bytes[candidate_header.start as usize..],
        &candidate_input.get_ref()[candidate_header.start as usize..]
    );
    let restored = format::load_with(&mut Cursor::new(restored_bytes), None).unwrap();
    assert_eq!(restored.graph, reference.graph);

    let primary = open(&candidate_path);
    assert!(matches!(
        primary.export_log(0, 1 << 20),
        Err(crate::Error::CursorBeforeHistory {
            cursor: 0,
            floor: FLOOR
        })
    ));
    drop(primary);

    let compact_result =
        crate::maintenance::compact_at(&candidate_path, directory.path().join("compact.vlite"), 5);
    assert!(
        matches!(
            compact_result,
            Err(crate::Error::UnsupportedFeature(FLAG_PRECISION))
        ),
        "{compact_result:?}"
    );
    assert!(matches!(
        crate::property_graph::strip_graph(
            &candidate_path,
            directory.path().join("stripped.vlite"),
            None,
            true
        ),
        Err(crate::Error::UnsupportedFeature(FLAG_PRECISION))
    ));
    assert!(matches!(
        crate::property_graph::enable_graph(
            &candidate_path,
            directory.path().join("enabled.vlite"),
            None
        ),
        Err(crate::Error::GraphInvalidOptions(_))
    ));
}

#[test]
fn graph_block_rejects_malformed_headers_owner_and_torn_images() {
    let bytes = include_str!("../../../../tests/fixtures/compact/graph-base-v1.hex")
        .split_whitespace()
        .map(|byte| u8::from_str_radix(byte, 16).unwrap())
        .collect::<Vec<_>>();
    for (at, value, reason) in [
        (0, 1, "compact block header does not match its frame"),
        (2, 0, "invalid compact block header"),
        (4, 2, "invalid compact block header"),
        (6, 0, "invalid compact block header"),
        (8, 1, "compact block header does not match its frame"),
        (16, 1, "invalid compact block header"),
        (24, 41, "invalid compact block sections"),
        (28, 1, "invalid compact block header"),
        (32, 41, "invalid compact block sections"),
        (36, 0xff, "invalid compact block sections"),
    ] {
        let mut bad = bytes.clone();
        bad[at] = value;
        assert_corrupt(
            crate::compact::Envelope::parse(0, u64::from(KIND), &bad, 0),
            reason,
        );
    }
    assert_corrupt(
        crate::compact::Envelope::parse(1, u64::from(KIND), &bytes, 0),
        "compact block header does not match its frame",
    );
    let envelope = crate::compact::Envelope::parse(0, u64::from(KIND), &bytes, 0).unwrap();
    assert_corrupt(
        graph_keys(1, 0, envelope.sections[0], 1, 0),
        "graph base owner mismatch",
    );
    assert_corrupt(
        graph_keys(0, 1, envelope.sections[0], 1, 0),
        "graph base entity count mismatch",
    );

    let source = loaded();
    let compact = image(&source, true);
    let _precision = Activation::on();
    let _graph_blocks = GraphBlocks::on();
    assert_corrupt(
        format::load_with(&mut Cursor::new(&compact[..compact.len() - 1]), None),
        "incomplete compact checkpoint image",
    );
}

#[test]
fn duplicate_out_of_order_and_undeclared_graph_blocks_are_corrupt() {
    let compact = image(&loaded(), true);
    let (header, frames) = split_image(&compact);
    let graph_blocks: Vec<_> = frames
        .iter()
        .enumerate()
        .filter(|(_, frame)| {
            frame[4] == format::OP_COMPACT_BLOCK
                && u64::from_le_bytes(frame[16..24].try_into().unwrap()) as u16 == KIND
        })
        .map(|(index, _)| index)
        .collect();
    assert_eq!(graph_blocks.len(), 2);

    let first = graph_blocks[0];
    let mut duplicate = frames.clone();
    duplicate.insert(first + 1, duplicate[first].clone());
    let duplicate = complete_image(&header, duplicate);

    let mut reordered = frames
        .iter()
        .enumerate()
        .filter(|(_, frame)| {
            let kind = u64::from_le_bytes(frame[16..24].try_into().unwrap()) as u16;
            frame[4] != format::OP_COMPACT_BLOCK
                || !matches!(kind, KIND | crate::compact::KIND_END)
                    && kind != crate::compact::catalog::KIND_REVISIONS
        })
        .map(|(_, frame)| frame.clone())
        .collect::<Vec<_>>();
    let graph_run = |owner: u16| {
        frames
            .iter()
            .filter(|frame| {
                frame[4] == format::OP_COMPACT_BLOCK
                    && u16::from_le_bytes(frame[5..7].try_into().unwrap()) == owner
                    && matches!(
                        u64::from_le_bytes(frame[16..24].try_into().unwrap()) as u16,
                        KIND | crate::compact::catalog::KIND_REVISIONS
                    )
            })
            .cloned()
            .collect::<Vec<_>>()
    };
    reordered.extend(graph_run(3));
    reordered.extend(graph_run(0));
    let reordered = complete_image(&header, reordered);

    let mut undeclared = frames.clone();
    let frame = &mut undeclared[first];
    let id = u64::from_le_bytes(frame[16..24].try_into().unwrap());
    frame[16..24].copy_from_slice(&(id + 1).to_le_bytes());
    let body_kind_at = format::PREFIX_SIZE;
    frame[body_kind_at..body_kind_at + 2].copy_from_slice(&(KIND + 1).to_le_bytes());
    fix_frame_checksums(frame);
    let undeclared = complete_image(&header, undeclared);

    let orphaned_graph = frames
        .iter()
        .filter(|frame| {
            frame[4] != format::OP_COMPACT_BLOCK
                || u64::from_le_bytes(frame[16..24].try_into().unwrap()) as u16 != KIND
                || u16::from_le_bytes(frame[5..7].try_into().unwrap()) != 0
        })
        .cloned()
        .collect::<Vec<_>>();
    let orphaned_graph = complete_image(&header, orphaned_graph);

    let _precision = Activation::on();
    let _graph_blocks = GraphBlocks::on();
    for (image, reason) in [
        (duplicate, "non-canonical compact block partition"),
        (reordered, "duplicate or out-of-order graph base block"),
        (undeclared, "undeclared compact block"),
        (orphaned_graph, "precision image frame out of order"),
    ] {
        assert_corrupt(format::load_with(&mut Cursor::new(image), None), reason);
    }
}

#[test]
fn graph_block_size_limit_fails_before_writer_output() {
    let source = loaded();
    let _precision = Activation::on();
    let _graph_blocks = GraphBlocks::on();
    let _limit = BodyLimit::set(64);
    let mut output = Vec::new();
    let result = crate::maintenance::write_v5_image(
        &mut output,
        &source,
        LINEAGE,
        None,
        Some(5),
        None,
        FLAG_GRAPH | FLAG_PRECISION,
        FLOOR,
    );
    assert_corrupt(result, "graph base block exceeds body cap");
    assert!(output.is_empty());
}

#[test]
fn graph_block_golden_envelope_decodes_and_reencodes_exactly() {
    let bytes = include_str!("../../../../tests/fixtures/compact/graph-base-v1.hex")
        .split_whitespace()
        .map(|byte| u8::from_str_radix(byte, 16).unwrap())
        .collect::<Vec<_>>();
    let envelope = crate::compact::Envelope::parse(0, u64::from(KIND), &bytes, 0).unwrap();
    assert_eq!(envelope.version, VERSION);
    assert_eq!(envelope.flags, CRITICAL);
    let bodies = graph_keys(0, 0, envelope.sections[0], 1, 0).unwrap();
    let mut pending = frame::PendingSnapshot::new(1, 0);
    for body in bodies {
        pending.push(body);
    }
    let records: BTreeMap<u16, crate::format::LoadedCollection> = BTreeMap::new();
    let state = pending.finish(&records).unwrap();
    let (count, encoded) = encode_graph(
        state
            .graph(crate::property_graph::engine::GraphId(0))
            .unwrap(),
        None,
    )
    .unwrap();
    assert_eq!(count, 0);
    assert_eq!(encoded, bytes);
}
