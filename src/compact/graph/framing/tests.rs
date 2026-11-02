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
                    GraphEntry::Attach {
                        node_id: NodeId(42),
                        collection: 0,
                        record_id: 4,
                    },
                    GraphEntry::GraphHighWater {
                        next_node_id: 102,
                        next_edge_id: 13,
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
                        edge_id: EdgeId(13),
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
                        next_edge_id: 14,
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
        actual_graph.graph(GraphId(0)).unwrap().segments(EdgeId(13)),
        reference_graph
            .graph(GraphId(0))
            .unwrap()
            .segments(EdgeId(13))
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
    for (at, value) in [
        (0, 1),
        (2, 0),
        (4, 2),
        (6, 0),
        (8, 1),
        (16, 1),
        (24, 41),
        (28, 1),
        (32, 41),
        (36, 0xff),
    ] {
        let mut bad = bytes.clone();
        bad[at] = value;
        assert!(
            crate::compact::Envelope::parse(0, u64::from(KIND), &bad, 0).is_err(),
            "header byte {at}"
        );
    }
    assert!(crate::compact::Envelope::parse(1, u64::from(KIND), &bytes, 0).is_err());
    let envelope = crate::compact::Envelope::parse(0, u64::from(KIND), &bytes, 0).unwrap();
    assert!(graph_keys(1, 0, envelope.sections[0], 1, 0).is_err());
    assert!(graph_keys(0, 1, envelope.sections[0], 1, 0).is_err());

    let source = loaded();
    let compact = image(&source, true);
    let _precision = Activation::on();
    let _graph_blocks = GraphBlocks::on();
    assert!(matches!(
        format::load_with(&mut Cursor::new(&compact[..compact.len() - 1]), None),
        Err(crate::Error::Corrupt { .. })
    ));
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
    for image in [duplicate, reordered, undeclared, orphaned_graph] {
        assert!(
            matches!(
                format::load_with(&mut Cursor::new(image), None),
                Err(crate::Error::Corrupt { .. })
            ),
            "malformed graph-block image was accepted"
        );
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
    assert!(matches!(result, Err(crate::Error::Corrupt { .. })));
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
