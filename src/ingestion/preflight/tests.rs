use super::*;
use crate::property_graph::engine::model::GraphId;
use std::fs;

fn database() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::create(dir.path().join("test.vlite"), 2).unwrap();
    db.create_graph("g").unwrap();
    (dir, db)
}

fn node(key: &str) -> GraphOp {
    GraphOp::UpsertNode {
        key: key.into(),
        labels: vec!["G7Chunk".into()],
        properties: Metadata::from([
            ("g7_v".into(), MetadataValue::Integer(1)),
            ("g7_kind".into(), MetadataValue::String("chunk".into())),
        ]),
        expected: Expected::Absent,
    }
}

fn vector(id: u64) -> GraphOp {
    GraphOp::Vector {
        collection: "default".into(),
        mutation: Mutation::PutWithMetadata {
            id,
            vector: vec![1.0, 0.0],
            payload: b"chunk".to_vec(),
            metadata: Metadata::from([
                ("g7_v".into(), MetadataValue::Integer(1)),
                ("g7_tenant".into(), MetadataValue::String("tenant".into())),
                ("g7_source".into(), MetadataValue::String("source".into())),
                (
                    "g7_revision".into(),
                    MetadataValue::String("revision".into()),
                ),
                ("g7_chunk".into(), MetadataValue::String("chunk".into())),
            ]),
        },
    }
}

fn attach() -> GraphOp {
    GraphOp::Attach {
        node: "chunk".into(),
        collection: "default".into(),
        record: 7,
    }
}

#[test]
fn ingestion_preflight_matches_mixed_engine_frame_and_rolls_back() {
    let (dir, mut db) = database();
    let ops = vec![node("chunk"), vector(7), attach()];
    let before = fs::metadata(dir.path().join("test.vlite")).unwrap().len();
    let sequence = db.sequence;
    let admission = preflight(&mut db, "g", &ops).unwrap();
    assert_eq!(admission.user_operations, 3);
    assert_eq!(admission.outcomes.len(), 3);
    assert!(matches!(
        admission.outcomes[0],
        Some(MutationOutcome::Node { created: true, .. })
    ));
    assert!(admission.outcomes[1].is_none());
    assert!(matches!(
        admission.outcomes[2],
        Some(MutationOutcome::Attached(_))
    ));
    assert_eq!(db.sequence, sequence);
    assert_eq!(
        db.graph
            .as_ref()
            .unwrap()
            .graph_by_name("g")
            .unwrap()
            .node_count(),
        0
    );
    assert!(db.get(7).unwrap().is_none());
    assert_eq!(
        fs::metadata(dir.path().join("test.vlite")).unwrap().len(),
        before
    );
    db.graph_mut("g").unwrap().write_batch(&ops).unwrap();
    let actual = fs::metadata(dir.path().join("test.vlite")).unwrap().len() - before;
    assert_eq!(actual as usize, 52 + admission.body_bytes - 16);
    assert_eq!(db.sequence, sequence + 1);
}

#[test]
fn ingestion_preflight_checks_attachment_order_and_record_absence() {
    let (_dir, mut db) = database();
    assert!(matches!(
        preflight(&mut db, "g", &[node("chunk"), attach(), vector(7)]),
        Err(Failure::Database(Error::GraphRecordNotAttachable))
    ));
    assert!(matches!(
        preflight(&mut db, "g", &[vector(7), vector(7)]),
        Err(Failure::Database(Error::InvalidQuery(
            "ingestion record id conflict"
        )))
    ));
    db.graph_mut("g")
        .unwrap()
        .write_batch(&[vector(7)])
        .unwrap();
    assert!(matches!(
        preflight(&mut db, "g", &[vector(7)]),
        Err(Failure::Database(Error::InvalidQuery(
            "ingestion record id conflict"
        )))
    ));
    assert_eq!(db.get(7).unwrap().unwrap().payload, b"chunk");
}

#[test]
fn ingestion_preflight_validates_physical_maps_guards_and_vector_dimensions() {
    let (_dir, mut db) = database();
    let mut invalid = node("chunk");
    if let GraphOp::UpsertNode { properties, .. } = &mut invalid {
        properties.insert("extra".into(), MetadataValue::Boolean(true));
    }
    assert!(matches!(
        preflight(&mut db, "g", &[invalid]),
        Err(Failure::Database(Error::InvalidQuery(
            "ingestion physical property type"
        )))
    ));
    let mut invalid = node("chunk");
    if let GraphOp::UpsertNode { properties, .. } = &mut invalid {
        properties.insert("extra".into(), MetadataValue::String("a".repeat(4097)));
        assert!(super::properties(properties).is_err());
    }
    assert!(matches!(
        preflight(&mut db, "g", &[invalid]),
        Err(Failure::Database(_))
    ));
    let mut invalid = vector(7);
    if let GraphOp::Vector {
        mutation: Mutation::PutWithMetadata { vector, .. },
        ..
    } = &mut invalid
    {
        vector.push(1.0);
    }
    assert!(matches!(
        preflight(&mut db, "g", &[invalid]),
        Err(Failure::Database(_))
    ));
    db.graph_mut("g")
        .unwrap()
        .write_batch(&[node("chunk")])
        .unwrap();
    assert!(matches!(
        preflight(&mut db, "g", &[node("chunk")]),
        Err(Failure::Engine(GraphError::PreconditionFailed { .. }))
    ));
}

fn vector_body(payload: usize) -> GraphBody {
    let mutation = Mutation::Put {
        id: 1,
        vector: vec![1.0],
        payload: vec![b'x'; payload],
    };
    GraphBody {
        graph_id: GraphId(0),
        entries: vec![crate::property_graph::frame::vector_entry(0, &mutation)],
    }
}

#[test]
fn ingestion_preflight_reserved_reclaim_exact_and_plus_one() {
    // Body = 4 + reclaim 16 + entry header 8 + collection/length 4
    //        + Put tag/padding 4 + id 8 + payload length 4
    //        + one F32 4 + payload.
    let payload = 16_777_216 - (4 + 16 + 8 + 4 + 4 + 8 + 4 + 4);
    let exact = vector_body(payload);
    assert_eq!(reserved_body(exact.clone(), 0).unwrap(), (16_777_216, 1));
    let mut with_reclaim = exact;
    with_reclaim
        .entries
        .insert(0, GraphEntry::Reclaim { at_ms: 0 });
    assert_eq!(reserved_body(with_reclaim, 0).unwrap(), (16_777_216, 1));
    assert!(matches!(
        reserved_body(vector_body(payload + 1), 0),
        Err(Failure::Engine(GraphError::BatchSize(_)))
    ));
}

#[test]
fn ingestion_preflight_reclaim_does_not_consume_user_operation_cap() {
    let entry = GraphEntry::DeleteNode {
        node_id: crate::property_graph::engine::model::NodeId(1),
    };
    let body = |n| GraphBody {
        graph_id: GraphId(0),
        entries: vec![entry.clone(); n],
    };
    assert_eq!(reserved_body(body(4096), 0).unwrap().1, 4096);
    assert!(matches!(
        reserved_body(body(4097), 0),
        Err(Failure::Engine(GraphError::BatchSize(_)))
    ));
}

#[test]
fn ingestion_preflight_rejects_non_new_source_operations() {
    let (_dir, mut db) = database();
    let mut invalid = node("chunk");
    if let GraphOp::UpsertNode { expected, .. } = &mut invalid {
        *expected = Expected::Any;
    }
    assert!(matches!(
        preflight(&mut db, "g", &[invalid]),
        Err(Failure::Database(Error::InvalidQuery(
            "ingestion new-source operation"
        )))
    ));
    assert!(matches!(
        preflight(
            &mut db,
            "g",
            &[GraphOp::Vector {
                collection: "default".into(),
                mutation: Mutation::Delete { id: 7 },
            }]
        ),
        Err(Failure::Database(Error::InvalidQuery(
            "ingestion new-source operation"
        )))
    ));
}
fn budget_vectors(body_bytes: usize, reserve: bool) -> Vec<GraphOp> {
    // The five-field metadata in vector() is 109 bytes. Each metadata put
    // contributes entry envelope 8, collection 4, batch prefix 16, F32s 8,
    // payload length 4 and metadata 109 = 149 bytes, excluding payload.
    let payload = body_bytes - 4 - usize::from(reserve) * 16 - 17 * 149;
    (0..17)
        .map(|index| {
            let mut op = vector(index + 1);
            if let GraphOp::Vector {
                mutation:
                    Mutation::PutWithMetadata {
                        payload: bytes,
                        metadata,
                        ..
                    },
                ..
            } = &mut op
            {
                assert_eq!(crate::metadata::encode(metadata).len(), 109);
                *bytes = vec![
                    b'x';
                    if index < 16 {
                        1_000_000
                    } else {
                        payload - 16_000_000
                    }
                ];
            }
            op
        })
        .collect()
}

#[test]
fn ingestion_preflight_exact_adapter_reserved_boundary() {
    let (_dir, mut db) = database();
    assert_eq!(
        preflight(&mut db, "g", &budget_vectors(16_777_216, true))
            .unwrap()
            .body_bytes,
        16_777_216
    );
    assert!(matches!(
        preflight(&mut db, "g", &budget_vectors(16_777_217, true)),
        Err(Failure::Engine(GraphError::BatchSize(_)))
    ));
    assert!(db.get(1).unwrap().is_none());
}

#[test]
fn ingestion_preflight_real_file_envelope_witnesses() {
    for (encrypted, envelope) in [(false, 52), (true, 80)] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frame.vlite");
        let options = crate::CreateOptions {
            encryption: encrypted.then(|| crate::EncryptionKey::from_bytes([7; 32])),
            ..crate::CreateOptions::default()
        };
        let mut db = Database::create_with_options(&path, 2, options).unwrap();
        let before = fs::metadata(&path).unwrap().len();
        db.create_graph("g").unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len() - before, 14 + envelope);
        let before = fs::metadata(&path).unwrap().len();
        db.graph_mut("g")
            .unwrap()
            .write_batch(&budget_vectors(16_777_216, false))
            .unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().len() - before,
            16_777_216 + envelope
        );
        let before = fs::metadata(&path).unwrap().len();
        assert!(
            db.graph_mut("g")
                .unwrap()
                .write_batch(&budget_vectors(16_777_217, false))
                .is_err()
        );
        assert_eq!(fs::metadata(&path).unwrap().len(), before);
    }
}

#[test]
fn ingestion_preflight_caps_scratch_before_cloning_and_rolls_back_late_failure() {
    let (_dir, mut db) = database();
    let mut large = vector(7);
    if let GraphOp::Vector {
        mutation: Mutation::PutWithMetadata { payload, .. },
        ..
    } = &mut large
    {
        *payload = vec![b'x'; 1_000_000];
    }
    let oversized = vec![large; 23];
    assert!(matches!(
        preflight(&mut db, "g", &oversized),
        Err(Failure::Database(Error::InvalidQuery(
            "ingestion document ceiling"
        )))
    ));
    let before = db.graph.clone();
    assert!(matches!(
        preflight(&mut db, "g", &[node("chunk"), vector(7), node("chunk")]),
        Err(Failure::Engine(GraphError::PreconditionFailed { .. }))
    ));
    assert_eq!(db.graph, before);
    assert!(db.get(7).unwrap().is_none());
}

fn edge(key: &str) -> GraphOp {
    GraphOp::CreateEdge {
        key: Some(key.into()),
        source: "source".into(),
        relationship: "MENTIONS".into(),
        destination: "destination".into(),
        properties: Metadata::new(),
    }
}

#[test]
fn ingestion_preflight_keyed_multi_edges_and_reclaim_only_no_append() {
    let (dir, mut db) = database();
    let mut graph = db.graph_mut("g").unwrap();
    graph
        .write_batch(&[node("source"), node("destination"), node("expired")])
        .unwrap();
    let ops = vec![edge("first"), edge("second")];
    assert_eq!(preflight(&mut db, "g", &ops).unwrap().user_operations, 2);
    db.graph_mut("g").unwrap().write_batch(&ops).unwrap();
    assert_eq!(
        db.graph
            .as_ref()
            .unwrap()
            .graph_by_name("g")
            .unwrap()
            .edge_count(),
        2
    );
    let future = crate::current_time_ms() + 1_000_000;
    db.graph_mut("g")
        .unwrap()
        .set_node_expiry("expired", Some(future), Expected::Any)
        .unwrap();
    let sequence = db.sequence;
    let before = fs::metadata(dir.path().join("test.vlite")).unwrap().len();
    let result = db
        .graph_mut("g")
        .unwrap()
        .write_batch_at(&[edge("first")], future)
        .unwrap();
    assert_eq!(result.revision, None);
    assert_eq!(db.sequence, sequence);
    assert_eq!(
        fs::metadata(dir.path().join("test.vlite")).unwrap().len(),
        before
    );
}

#[test]
fn ingestion_preflight_raw_schema_duplicate_witnesses() {
    use crate::property_graph::engine::decode_body;
    fn body(payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![1, 0, 0, 0, 12, 0, 0, 0];
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }
    // Raw payloads rather than a BTreeMap, which would erase duplicates.
    let label = [1, b'A', 0, 0, 0];
    let relation = [1, b'R', 0, 0, 1, 0, 0];
    let property = [1, 0, b'p', 1, 0];
    let endpoint = [1, b'A', 1, b'B'];
    let good = [&[1, 0][..], &label, &[0, 0]].concat();
    assert!(decode_body(&body(&good), 1).is_ok());
    let good = [&[0, 0, 1, 0][..], &relation].concat();
    assert!(decode_body(&body(&good), 1).is_ok());
    for duplicate in [
        [&[2, 0][..], &label, &label, &[0, 0]].concat(),
        [&[0, 0, 2, 0][..], &relation, &relation].concat(),
        [&[1, 0, 1, b'A', 0, 0, 2][..], &property, &property, &[0, 0]].concat(),
        [
            &[0, 0, 1, 0, 1, b'R', 0, 0, 1, 2][..],
            &endpoint,
            &endpoint,
            &[0],
        ]
        .concat(),
    ] {
        assert_eq!(
            decode_body(&body(&duplicate), 1),
            Err(GraphError::Corrupt(
                "schema names must be strictly ascending"
            ))
        );
    }
}

#[test]
fn ingestion_preflight_exact_operation_ceiling_and_read_only() {
    let (dir, mut db) = database();
    let ops = (0..4097)
        .map(|i| node(&format!("node-{i}")))
        .collect::<Vec<_>>();
    assert_eq!(
        preflight(&mut db, "g", &ops[..4096])
            .unwrap()
            .user_operations,
        4096
    );
    assert!(matches!(
        preflight(&mut db, "g", &ops),
        Err(Failure::Database(Error::BatchTooLarge))
    ));
    assert!(matches!(
        preflight(&mut db, "g", &[]),
        Err(Failure::Database(Error::BatchTooLarge))
    ));
    drop(db);
    let mut db = Database::open_read_only(dir.path().join("test.vlite")).unwrap();
    assert!(matches!(
        preflight(&mut db, "g", &[node("chunk")]),
        Err(Failure::Database(Error::ReadOnly))
    ));
}
