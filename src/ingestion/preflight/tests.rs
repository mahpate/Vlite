use super::*;
use crate::ingestion_canonical::canonical::{self, Value};
use crate::property_graph::engine::model::GraphId;
use sha2::Digest;
use std::fs;

fn database() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::create(dir.path().join("test.vlite"), 2).unwrap();
    db.create_graph("g").unwrap();
    (dir, db)
}

fn canonical_source_plan() -> Value {
    let fixture = crate::ingestion_canonical::fixture();
    let templates = &fixture["templates"];
    let binding = serde_json::json!({
        "graph": "g",
        "collection": "default",
        "tenant": "t",
        "generation": {"ref": "zero"}
    });
    let source_json = serde_json::json!({
        "kind": "source",
        "fields": {"binding": binding, "tenant": "t", "source_id": "s"}
    });
    let receipt_json = serde_json::json!({
        "kind": "receipt",
        "fields": {
            "binding": binding,
            "tenant": "t",
            "source_id": "s",
            "content": {"ref": "zero"},
            "pipeline": {"ref": "zero"}
        }
    });
    let source = crate::ingestion_canonical::plan_value(&source_json, templates);
    let receipt = crate::ingestion_canonical::plan_value(&receipt_json, templates);
    let source_key = canonical::identity_key(&source).unwrap();
    let receipt_key = canonical::identity_key(&receipt).unwrap();
    let entity_json = serde_json::json!({
        "kind": "entity",
        "fields": {
            "binding": binding,
            "tenant": "t",
            "kind": "person",
            "normalized_key": "alice"
        }
    });
    let revision_json = serde_json::json!({
        "kind": "revision",
        "fields": {
            "source": source_json,
            "content": {"ref": "zero"},
            "pipeline": {"ref": "zero"}
        }
    });
    let revision = crate::ingestion_canonical::plan_value(&revision_json, templates);
    let revision_key = canonical::identity_key(&revision).unwrap();
    let chunk_json = serde_json::json!({
        "kind": "chunk",
        "fields": {
            "revision": revision_json,
            "ordinal": 0,
            "start": 0,
            "end": 1
        }
    });
    let chunk = crate::ingestion_canonical::plan_value(&chunk_json, templates);
    let chunk_key = canonical::identity_key(&chunk).unwrap();
    let chunk_digest = canonical::hex(&sha2::Sha256::digest(b"c"));
    let batch_receipt_json = serde_json::json!({
        "kind": "batch_receipt",
        "fields": {"root": receipt_json, "ordinal": 1}
    });
    let input = serde_json::json!({
        "request_digest": {"ref": "zero"},
        "captured": [],
        "accepted": [{
            "chunk": chunk_json,
            "text": "63",
            "entities": [],
            "relations": [],
            "vector": [1.0, 0.0]
        }],
        "batches": [{
            "root": receipt_json,
            "ordinal": 1,
            "groups": [{
                "ordinal": 0,
                "operations": [
                    {"tag": 13, "fields": {
                        "source": source_json,
                        "data": {
                            "revision_key": revision_key,
                            "revision_digest": {"ref": "zero"},
                            "pipeline": {"ref": "zero"},
                            "status": 1,
                            "job_receipt_key": receipt_key,
                            "last_batch": 1
                        }
                    }},
                    {"tag": 5, "fields": {"object": {
                        "identity": entity_json,
                        "data": {}
                    }}},
                    {"tag": 5, "fields": {"object": {
                        "identity": revision_json,
                        "data": {
                            "source_key": source_key,
                            "uri": null,
                            "text_length": 1,
                            "chunk_count": 1,
                            "receipt_key": receipt_key
                        }
                    }}},
                    {"tag": 5, "fields": {"object": {
                        "identity": chunk_json,
                        "data": {
                            "source_key": source_key,
                            "revision_key": revision_key,
                            "ordinal": 0,
                            "start": 0,
                            "end": 1,
                            "chunk_digest": chunk_digest,
                            "pipeline": {"ref": "zero"}
                        }
                    }}},
                    {"tag": 6, "fields": {
                        "collection": 0,
                        "record": 7,
                        "data": {
                            "tenant": "t",
                            "source_key": source_key,
                            "revision_key": revision_key,
                            "chunk_key": chunk_key,
                            "text": "63",
                            "vector": [1.0, 0.0]
                        }
                    }},
                    {"tag": 7, "fields": {
                        "chunk": chunk_json,
                        "collection": 0,
                        "record": 7
                    }},
                    {"tag": 8, "fields": {"object": {
                        "identity": {
                            "kind": "has_revision",
                            "fields": {"source": source_json, "revision": revision_json}
                        },
                        "data": {}
                    }}},
                    {"tag": 8, "fields": {"object": {
                        "identity": {
                            "kind": "has_chunk",
                            "fields": {"revision": revision_json, "chunk": chunk_json}
                        },
                        "data": {}
                    }}},
                    {"tag": 10, "fields": {"object": {
                        "identity": batch_receipt_json,
                        "data": {
                            "batch_digest": {"tag": 2, "value": {"ref": "zero"}},
                            "final": true,
                            "created_anchors": 0,
                            "reused_anchors": 0,
                            "result_shards": 0,
                            "ownership_shards": 0
                        }
                    }}},
                    {"tag": 12, "fields": {"object": {
                        "identity": receipt_json,
                        "data": {
                            "request_digest": {"ref": "zero"},
                            "plan_digest": {"tag": 1, "value": {"ref": "zero"}},
                            "batch_count": 1,
                            "revision_key": revision_key,
                            "outcome": 0,
                            "chunks": 1,
                            "entities": 0,
                            "relations": 0,
                            "mentions": 0
                        }
                    }}}
                ]
            }]
        }]
    });
    let mut plan = crate::ingestion_canonical::plan_value(&input, templates);
    let batch = match super::cfield(&plan, "batches").unwrap() {
        Value::List(batches) => &batches[0],
        _ => panic!("batches"),
    };
    let batch_digest = canonical::hex(&canonical::plan::digest("batch", batch).unwrap());
    fn fill(value: &mut Value, field: &str, digest: &str) {
        match value {
            Value::Map(map) => {
                if let Some(Value::Map(slot)) = map.get_mut(field) {
                    slot.insert("value".into(), Value::Text(digest.into()));
                }
                for value in map.values_mut() {
                    fill(value, field, digest);
                }
            }
            Value::List(items) => {
                for value in items {
                    fill(value, field, digest);
                }
            }
            _ => {}
        }
    }
    fill(&mut plan, "batch_digest", &batch_digest);
    let digest = canonical::hex(&canonical::plan::digest("plan", &plan).unwrap());
    fill(&mut plan, "plan_digest", &digest);
    plan
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

fn canonical_entity_op(db: &Database, plan: &Value) -> GraphOp {
    let batch = match super::cfield(plan, "batches").unwrap() {
        Value::List(batches) => &batches[0],
        _ => panic!("batches"),
    };
    let root = super::cfield(batch, "root").unwrap();
    let groups = match super::cfield(batch, "groups").unwrap() {
        Value::List(groups) => groups,
        _ => panic!("groups"),
    };
    let operations = match super::cfield(&groups[0], "operations").unwrap() {
        Value::List(operations) => operations,
        _ => panic!("operations"),
    };
    let operation = operations
        .iter()
        .find(|operation| {
            super::cu64(super::cfield(operation, "tag").unwrap()).unwrap() == 5
                && super::crole(
                    super::cfield(
                        super::cfield(super::cfield(operation, "fields").unwrap(), "object")
                            .unwrap(),
                        "identity",
                    )
                    .unwrap(),
                )
                .is_ok_and(|role| role == "entity")
        })
        .unwrap();
    super::canonical_operation(db, operation, root, 1, None)
        .unwrap()
        .unwrap()
}

fn canonical_operation_value(tag: u64, fields: BTreeMap<String, Value>) -> Value {
    Value::Map(BTreeMap::from([
        ("tag".into(), Value::Unsigned(tag)),
        ("fields".into(), Value::Map(fields)),
    ]))
}

fn expected_value(tag: u64, value: Value) -> Value {
    Value::Map(BTreeMap::from([
        ("tag".into(), Value::Unsigned(tag)),
        ("value".into(), value),
    ]))
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
fn ingestion_preflight_scratch_budget_accepts_canonical_cleanup_operations() {
    let ops = vec![
        GraphOp::DeleteEdge {
            edge: EdgeRef::Key("edge".into()),
            expected: Expected::Any,
        },
        GraphOp::Detach {
            node: "chunk".into(),
            collection: "default".into(),
        },
        GraphOp::Vector {
            collection: "default".into(),
            mutation: Mutation::Delete { id: 7 },
        },
        GraphOp::DeleteNode {
            key: "chunk".into(),
            expected: Expected::Any,
        },
        GraphOp::PatchNode {
            key: "source".into(),
            patch: NodePatch {
                add_labels: Vec::new(),
                remove_labels: Vec::new(),
                set: Metadata::from([("g7_status".into(), MetadataValue::Integer(1))]),
                unset: vec!["g7_old".into()],
            },
            expected: Expected::Any,
        },
    ];
    assert!(scratch_admission(&ops).is_ok());
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

#[test]
fn ingestion_preflight_lowers_and_measures_a_canonical_new_source_batch() {
    let (_dir, mut db) = database();
    let plan = canonical_source_plan();
    let entity = canonical_entity_op(&db, &plan);
    db.graph_mut("g").unwrap().write_batch(&[entity]).unwrap();
    let admission = preflight_canonical_batch(&mut db, &plan, 0, None).unwrap();
    assert_eq!(admission.user_operations, 10);
    assert_eq!(admission.outcomes.len(), 10);
    for index in [0, 2, 3, 8, 9] {
        assert!(matches!(
            admission.outcomes[index],
            Some(MutationOutcome::Node { created: true, .. })
        ));
    }
    assert!(matches!(
        admission.outcomes[1],
        Some(MutationOutcome::Node { created: false, .. })
    ));
    assert!(admission.outcomes[4].is_none());
    assert!(matches!(
        admission.outcomes[5],
        Some(MutationOutcome::Attached(_))
    ));
    for index in [6, 7] {
        assert!(matches!(
            admission.outcomes[index],
            Some(MutationOutcome::Edge { created: true, .. })
        ));
    }
    assert_eq!(
        db.graph
            .as_ref()
            .unwrap()
            .graph_by_name("g")
            .unwrap()
            .node_count(),
        1
    );
}

#[test]
fn ingestion_preflight_lowers_canonical_cleanup_and_head_patch_tags() {
    let (_dir, mut db) = database();
    let plan = canonical_source_plan();
    let batch = match super::cfield(&plan, "batches").unwrap() {
        Value::List(batches) => &batches[0],
        _ => panic!("batches"),
    };
    let root = super::cfield(batch, "root").unwrap();
    let groups = match super::cfield(batch, "groups").unwrap() {
        Value::List(groups) => groups,
        _ => panic!("groups"),
    };
    let operations = match super::cfield(&groups[0], "operations").unwrap() {
        Value::List(operations) => operations,
        _ => panic!("operations"),
    };
    let source_operation = operations
        .iter()
        .find(|operation| super::cu64(super::cfield(operation, "tag").unwrap()).unwrap() == 13)
        .unwrap();
    let source =
        super::cfield(super::cfield(source_operation, "fields").unwrap(), "source").unwrap();
    let source_data =
        super::cfield(super::cfield(source_operation, "fields").unwrap(), "data").unwrap();
    let chunk_operation = operations
        .iter()
        .find(|operation| {
            if super::cu64(super::cfield(operation, "tag").unwrap()).unwrap() != 5 {
                return false;
            }
            let identity = super::cfield(
                super::cfield(super::cfield(operation, "fields").unwrap(), "object").unwrap(),
                "identity",
            )
            .unwrap();
            super::crole(identity).is_ok_and(|role| role == "chunk")
        })
        .unwrap();
    let chunk_object = super::cfield(
        super::cfield(super::cfield(chunk_operation, "fields").unwrap(), "object").unwrap(),
        "identity",
    )
    .unwrap();
    let chunk_data = super::cfield(
        super::cfield(super::cfield(chunk_operation, "fields").unwrap(), "object").unwrap(),
        "data",
    )
    .unwrap();
    let edge_operation = operations
        .iter()
        .find(|operation| super::cu64(super::cfield(operation, "tag").unwrap()).unwrap() == 8)
        .unwrap();
    let edge = super::cfield(
        super::cfield(super::cfield(edge_operation, "fields").unwrap(), "object").unwrap(),
        "identity",
    )
    .unwrap()
    .clone();
    db.graph_mut("g")
        .unwrap()
        .write_batch(&[
            cnode(source, source_data, Expected::Absent).unwrap(),
            cnode(chunk_object, chunk_data, Expected::Absent).unwrap(),
        ])
        .unwrap();

    let graph = db.graph.as_ref().unwrap().graph_by_name("g").unwrap();
    let source_revision = graph
        .node_by_key(&cidentity_key(source).unwrap())
        .unwrap()
        .revision;
    let chunk_revision = graph
        .node_by_key(&cidentity_key(chunk_object).unwrap())
        .unwrap()
        .revision;
    let expected_source = expected_value(1, Value::Unsigned(source_revision));
    let expected_chunk = expected_value(1, Value::Unsigned(chunk_revision));
    let edge_delete = canonical_operation_value(
        1,
        BTreeMap::from([
            ("edge".into(), edge),
            ("expected".into(), expected_value(0, Value::Null)),
        ]),
    );
    assert!(matches!(
        canonical_operation(&db, &edge_delete, root, 1, None).unwrap(),
        Some(GraphOp::DeleteEdge { .. })
    ));

    let detach = canonical_operation_value(
        2,
        BTreeMap::from([
            ("chunk".into(), chunk_object.clone()),
            ("expected".into(), expected_chunk.clone()),
        ]),
    );
    assert!(matches!(
        canonical_operation(&db, &detach, root, 1, None).unwrap(),
        Some(GraphOp::Detach { .. })
    ));
    let delete_vector =
        canonical_operation_value(3, BTreeMap::from([("record".into(), Value::Unsigned(7))]));
    assert!(matches!(
        canonical_operation(&db, &delete_vector, root, 1, None).unwrap(),
        Some(GraphOp::Vector {
            mutation: Mutation::Delete { id: 7 },
            ..
        })
    ));
    let delete_node = canonical_operation_value(
        4,
        BTreeMap::from([
            ("node".into(), chunk_object.clone()),
            ("expected".into(), expected_chunk),
        ]),
    );
    assert!(matches!(
        canonical_operation(&db, &delete_node, root, 1, None).unwrap(),
        Some(GraphOp::DeleteNode { .. })
    ));
    let patch = canonical_operation_value(
        11,
        BTreeMap::from([
            ("source".into(), source.clone()),
            ("data".into(), source_data.clone()),
            ("expected".into(), expected_source),
        ]),
    );
    assert!(matches!(
        canonical_operation(&db, &patch, root, 1, None).unwrap(),
        Some(GraphOp::PatchNode { .. })
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
