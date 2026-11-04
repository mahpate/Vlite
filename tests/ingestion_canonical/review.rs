use super::*;

fn template(name: &str) -> Value {
    if name == "relation" {
        return super::template(name);
    }
    let f = fixture();
    assert!(!f["templates"][name].is_null(), "missing template {name}");
    plan_value(&f["templates"][name], &f["templates"])
}

fn op(tag: u64, fields: Value) -> Value {
    map(&[("tag", Value::Unsigned(tag)), ("fields", fields)])
}

fn group(ordinal: u64, operations: Vec<Value>) -> Value {
    map(&[
        ("ordinal", Value::Unsigned(ordinal)),
        ("operations", Value::List(operations)),
    ])
}

fn batch(ordinal: u64, groups: Vec<Value>) -> Value {
    map(&[
        ("root", template("receipt")),
        ("ordinal", Value::Unsigned(ordinal)),
        ("groups", Value::List(groups)),
    ])
}

fn plan(batches: Vec<Value>) -> Value {
    let mut plan = template("plan");
    set(&mut plan, "batches", Value::List(batches));
    plan
}

fn captured(plan: &mut Value) {
    set(plan, "captured", template("captured_endpoints"));
}

fn encode_plan(plan: &Value) -> canonical::Result<Vec<u8>> {
    canonical::encode(&Type::Record("plan"), plan)
}

fn object_op(tag: u64, identity: Value, data: Value) -> Value {
    op(
        tag,
        map(&[("object", map(&[("identity", identity), ("data", data)]))]),
    )
}

fn remove(edge: Value) -> Value {
    op(
        1,
        map(&[("edge", edge), ("expected", template("revision_ref"))]),
    )
}

fn edge_identity(name: &str) -> Value {
    get(get(&template(name), "fields"), "object").clone()
}

fn observation() -> Value {
    map(&[
        ("local_id", Value::Unsigned(7)),
        ("kind", Value::Text("K".into())),
        ("key", Value::Text("a".into())),
        ("surface", Value::Text("Alpha".into())),
        ("start", Value::Unsigned(1)),
        ("end", Value::Unsigned(2)),
        ("attributes", map(&[])),
        ("confidence", Value::Null),
    ])
}

fn accepted(entities: Vec<Value>, relations: Vec<Value>, vector: Vec<Value>) -> Value {
    map(&[
        ("chunk", template("chunk")),
        ("text", Value::Bytes(b"Alpha".to_vec())),
        ("entities", Value::List(entities)),
        ("relations", Value::List(relations)),
        ("vector", Value::List(vector)),
    ])
}

fn relation_observation() -> Value {
    map(&[
        ("subject_local_id", Value::Unsigned(1)),
        ("predicate", Value::Text("P".into())),
        ("object_local_id", Value::Unsigned(2)),
        ("start", Value::Unsigned(1)),
        ("end", Value::Unsigned(3)),
        ("attributes", map(&[])),
        ("confidence", Value::Null),
    ])
}

#[test]
fn cleanup_membership_and_created_endpoints() {
    let first = get(&edge_identity("has_chunk_edge_operation"), "identity").clone();
    let second = identity(
        "has_chunk",
        map(&[
            ("revision", template("revision_other")),
            ("chunk", template("chunk_other")),
        ]),
    );
    let cleanup = plan(vec![batch(
        1,
        vec![group(
            0,
            vec![
                template("guard_operation"),
                remove(first),
                remove(second),
                template("root_operation"),
            ],
        )],
    )]);
    assert!(encode_plan(&cleanup).is_ok());
    let creation = plan(vec![batch(
        1,
        vec![group(
            0,
            vec![
                template("source_create_operation"),
                template("create_revision_operation"),
                template("has_revision_edge_operation"),
                template("root_operation"),
            ],
        )],
    )]);
    assert!(encode_plan(&creation).is_ok());
}

#[test]
fn survivor_tag3_delete_matches_collection_and_record() {
    let mut detach = template("detach_operation");
    let mut detach_fields = get(&detach, "fields").clone();
    set(&mut detach_fields, "collection", Value::Unsigned(1));
    set(&mut detach_fields, "record", Value::Unsigned(7));
    set(&mut detach, "fields", detach_fields);

    let mut delete = template("delete_operation");
    let mut delete_fields = get(&delete, "fields").clone();
    set(&mut delete_fields, "collection", Value::Unsigned(2));
    set(&mut delete_fields, "record", Value::Unsigned(7));
    set(&mut delete, "fields", delete_fields);

    let plan = plan(vec![batch(
        1,
        vec![group(
            0,
            vec![
                template("guard_operation"),
                detach,
                delete,
                template("root_operation"),
            ],
        )],
    )]);
    assert_eq!(encode_plan(&plan), Err("group_chunk_membership"));
}

#[test]
fn survivor_put_must_resolve_its_chunk() {
    let mut put = template("put_operation");
    let mut fields = get(&put, "fields").clone();
    let mut data = get(&fields, "data").clone();
    set(&mut data, "chunk_key", Value::Text("missing-chunk".into()));
    set(&mut fields, "data", data);
    set(&mut put, "fields", fields);

    let mut plan = plan(vec![batch(
        1,
        vec![group(
            0,
            vec![template("guard_operation"), put, template("root_operation")],
        )],
    )]);
    captured(&mut plan);
    assert_eq!(encode_plan(&plan), Err("put_names_chunk"));
}

#[test]
fn survivor_relations_follow_provenance_edges() {
    let mut plan = plan(vec![batch(
        1,
        vec![group(
            0,
            vec![
                template("guard_operation"),
                template("relation_one_operation"),
                template("has_revision_edge_operation"),
                template("root_operation"),
            ],
        )],
    )]);
    captured(&mut plan);
    assert_eq!(encode_plan(&plan), Err("operation_order"));
}

#[test]
fn survivor_rejects_duplicate_relation_edge_identities() {
    let mut duplicate = template("relation_one_operation");
    let mut fields = get(&duplicate, "fields").clone();
    let mut object = get(&fields, "object").clone();
    let mut data = get(&object, "data").clone();
    set(&mut data, "confidence", Value::Float(0.5));
    set(&mut object, "data", data);
    set(&mut fields, "object", object);
    set(&mut duplicate, "fields", fields);

    let mut plan = plan(vec![batch(
        1,
        vec![group(
            0,
            vec![
                template("guard_operation"),
                template("relation_one_operation"),
                duplicate,
                template("root_operation"),
            ],
        )],
    )]);
    captured(&mut plan);
    assert_eq!(encode_plan(&plan), Err("duplicate_edge"));
}

#[test]
fn survivor_vector_entry_bytes_pin_optional_field_order() {
    let mut entry = template("entry");
    for (key, value) in [
        ("producing_op", Value::Unsigned(0x0102_0304)),
        ("result_ordinal", Value::Unsigned(0x0506_0708)),
        ("action", Value::Unsigned(1)),
        ("object_kind", Value::Unsigned(2)),
        ("object_key", Value::Text(String::new())),
        ("collection", Value::Unsigned(0x1234)),
        ("record", Value::Unsigned(0x3132_3334_3536_3738)),
        ("attached_chunk_key", Value::Text("C".into())),
        ("retained", Value::Bool(true)),
    ] {
        set(&mut entry, key, value);
    }
    set(
        &mut entry,
        "id",
        map(&[
            ("tag", Value::Unsigned(0)),
            ("value", Value::Unsigned(0x1112_1314_1516_1718)),
        ]),
    );
    set(
        &mut entry,
        "revision",
        map(&[
            ("tag", Value::Unsigned(1)),
            ("value", Value::Unsigned(0x2122_2324_2526_2728)),
        ]),
    );
    assert_eq!(
        canonical::hex(&canonical::encode(&Type::Record("entry"), &entry).unwrap()),
        "01040302010807060501020000000000181716151413121101282726252423222101341201383736353433323101010000004301"
    );
}

#[test]
fn survivor_shard_order_compares_action_before_object_kind() {
    let mut kind_zero_action_one = template("entry");
    set(&mut kind_zero_action_one, "action", Value::Unsigned(1));
    set(&mut kind_zero_action_one, "object_kind", Value::Unsigned(0));
    let mut kind_one_action_zero = template("entry");
    set(&mut kind_one_action_zero, "action", Value::Unsigned(0));
    set(&mut kind_one_action_zero, "object_kind", Value::Unsigned(1));

    assert!(
        canonical::stored::split_shards(
            &shard_header(),
            &[kind_one_action_zero.clone(), kind_zero_action_one.clone()],
        )
        .is_ok()
    );
    assert_eq!(
        canonical::stored::split_shards(
            &shard_header(),
            &[kind_zero_action_one, kind_one_action_zero],
        ),
        Err("entry_order")
    );
}

#[test]
fn composite_and_scalar_budget_boundaries() {
    let mut policy = full_policy();
    let text = Value::Text("x".repeat(4096));
    let bytes = canonical::encode(&Type::Text, &text).unwrap();
    let spent = bytes.len() + 256 + 4096;
    set(&mut policy, "prepared_bytes", Value::Unsigned(spent as u64));
    assert_eq!(
        canonical::decode_with_policy(&Type::Text, &bytes, &policy).unwrap(),
        text
    );
    set(
        &mut policy,
        "prepared_bytes",
        Value::Unsigned(spent as u64 - 1),
    );
    assert_eq!(
        canonical::decode_with_policy(&Type::Text, &bytes, &policy),
        Err("prepared_bytes")
    );
    let attrs = map(&[
        (
            "a",
            Value::List(vec![Value::Unsigned(5), Value::Text("x".repeat(4092))]),
        ),
        (
            "b",
            Value::List(vec![Value::Unsigned(5), Value::Text("y".repeat(4080))]),
        ),
    ]);
    let bytes = canonical::encode(&Type::Attributes, &attrs).unwrap();
    assert_eq!(bytes.len(), 8196);
    assert_eq!(canonical::decode(&Type::Attributes, &bytes).unwrap(), attrs);
    let mut excessive = bytes;
    excessive[..4].copy_from_slice(&8193u32.to_le_bytes());
    excessive.push(0);
    assert_eq!(
        canonical::decode(&Type::Attributes, &excessive),
        Err("attribute_bytes")
    );
    for kind in [Type::Text, Type::Bytes] {
        let payload = if matches!(kind, Type::Text) {
            Value::Text("p".repeat(4096))
        } else {
            Value::Bytes(vec![b'p'; 4096])
        };
        let bytes = canonical::encode(&kind, &payload).unwrap();
        let charge = bytes.len() + 256 + 4096;
        let mut policy = full_policy();
        set(
            &mut policy,
            "prepared_bytes",
            Value::Unsigned(charge as u64),
        );
        assert!(canonical::decode_with_policy(&kind, &bytes, &policy).is_ok());
        set(
            &mut policy,
            "prepared_bytes",
            Value::Unsigned(charge as u64 - 1),
        );
        assert_eq!(
            canonical::decode_with_policy(&kind, &bytes, &policy),
            Err("prepared_bytes")
        );
    }
}

#[test]
fn default_budget_admits_512_dense_chunks() {
    let mut rows = Vec::new();
    for ordinal in 0..512 {
        let mut row = accepted(vec![], vec![], vec![Value::Float(1.0); 1536]);
        let mut chunk = template("chunk");
        let mut fields = get(&chunk, "fields").clone();
        set(&mut fields, "ordinal", Value::Unsigned(ordinal));
        set(&mut fields, "start", Value::Unsigned(ordinal));
        set(&mut fields, "end", Value::Unsigned(ordinal + 1));
        set(&mut chunk, "fields", fields);
        set(&mut row, "chunk", chunk);
        rows.push(row);
    }
    let mut plan = plan(vec![]);
    set(&mut plan, "accepted", Value::List(rows));
    let bytes = encode_plan(&plan).unwrap();
    let decoded =
        canonical::decode_with_policy(&Type::Record("plan"), &bytes, &full_policy()).unwrap();
    assert_eq!(decoded, plan);
    assert_eq!(
        canonical::decode(&Type::Record("plan"), &bytes).unwrap(),
        plan
    );
}

#[test]
fn policy_list_equalities_and_default_caps() {
    let row = accepted(vec![observation()], vec![relation_observation()], vec![]);
    let bytes = canonical::encode(&Type::Record("accepted_chunk"), &row).unwrap();
    let mut policy = full_policy();
    for field in ["chunk_entities", "chunk_relations"] {
        set(&mut policy, field, Value::Unsigned(1));
    }
    assert!(
        canonical::decode_with_policy(&Type::Record("accepted_chunk"), &bytes, &policy).is_ok()
    );
    set(&mut policy, "chunk_relations", Value::Unsigned(0));
    assert_eq!(
        canonical::decode_with_policy(&Type::Record("accepted_chunk"), &bytes, &policy),
        Err("policy_limit")
    );
    let row = accepted(
        vec![observation(); 256],
        vec![relation_observation(); 512],
        vec![],
    );
    let bytes = canonical::encode(&Type::Record("accepted_chunk"), &row).unwrap();
    assert!(canonical::decode(&Type::Record("accepted_chunk"), &bytes).is_ok());
    let mut row_over = row;
    set(
        &mut row_over,
        "entities",
        Value::List(vec![observation(); 257]),
    );
    let bytes = canonical::encode(&Type::Record("accepted_chunk"), &row_over).unwrap();
    assert_eq!(
        canonical::decode(&Type::Record("accepted_chunk"), &bytes),
        Err("policy_limit")
    );
    let plan = plan(
        (1..=64)
            .map(|i| batch(i, vec![group(0, vec![template("guard_operation")])]))
            .collect(),
    );
    let bytes = encode_plan(&plan).unwrap();
    assert!(canonical::decode(&Type::Record("plan"), &bytes).is_ok());
    let single = super::review::plan(vec![batch(
        1,
        vec![group(0, vec![template("guard_operation")])],
    )]);
    let bytes = encode_plan(&single).unwrap();
    let mut policy = full_policy();
    set(&mut policy, "frames", Value::Unsigned(1));
    set(&mut policy, "operations", Value::Unsigned(1));
    assert!(canonical::decode_with_policy(&Type::Record("plan"), &bytes, &policy).is_ok());
}

#[test]
fn operation_count_clamp_before_materialization() {
    let mut nodes = (0..4096)
        .map(|i| {
            let entity = change_identity(
                template("entity"),
                "normalized_key",
                Value::Text(format!("entity{i}")),
            );
            (
                canonical::identity_key(&entity).unwrap(),
                object_op(5, entity, map(&[])),
            )
        })
        .collect::<Vec<_>>();
    nodes.sort_by(|a, b| a.0.cmp(&b.0));
    let mut ops = vec![template("guard_operation")];
    ops.extend(nodes.into_iter().map(|(_, op)| op));
    let plan = plan(vec![batch(1, vec![group(0, ops)])]);
    let bytes = encode_plan(&plan).unwrap();
    let mut policy = full_policy();
    set(&mut policy, "operations", Value::Unsigned(131072));
    assert_eq!(
        canonical::decode_with_policy(&Type::Record("plan"), &bytes, &policy),
        Err("policy_limit")
    );
}

#[test]
fn plan_field_orders_have_distinct_byte_witnesses() {
    let fields = [
        "source_bytes",
        "chunks",
        "entities",
        "relations",
        "mentions",
        "chunk_entities",
        "chunk_relations",
        "decoded_bytes",
        "attribute_bytes",
        "attribute_fields",
        "attribute_string_bytes",
        "frames",
        "operations",
    ];
    let mut policy = full_policy();
    let mut expected = vec![1];
    for (index, key) in fields.iter().enumerate() {
        let value = index as u32 + 1;
        set(&mut policy, key, Value::Unsigned(value.into()));
        expected.extend(value.to_le_bytes());
    }
    set(&mut policy, "prepared_bytes", Value::Unsigned(0x01020304));
    expected.extend(0x01020304u64.to_le_bytes());
    assert_eq!(
        canonical::encode(&Type::Record("policy"), &policy).unwrap(),
        expected
    );
    let mut pipeline = template("pipeline");
    set(&mut pipeline, "dimensions", Value::Unsigned(1536));
    set(&mut pipeline, "schema_version", Value::Unsigned(1));
    let encoded = canonical::encode(&Type::Record("pipeline"), &pipeline).unwrap();
    let policy_len = canonical::encode(&Type::Record("policy"), get(&pipeline, "policy"))
        .unwrap()
        .len();
    let tail = encoded.len() - policy_len - 4 - 40;
    let mut expected = 1536u32.to_le_bytes().to_vec();
    expected.extend(
        canonical::unhex(match get(&pipeline, "binding_generation") {
            Value::Text(s) => s,
            _ => panic!("digest"),
        })
        .unwrap(),
    );
    expected.extend(1u32.to_le_bytes());
    assert_eq!(&encoded[tail..tail + 40], expected);
    let mut request = template("request");
    set(&mut request, "text", Value::Bytes(vec![]));
    set(
        &mut request,
        "expected_source",
        map(&[
            ("tag", Value::Unsigned(1)),
            ("value", Value::Unsigned(0x0102030405060708)),
        ]),
    );
    set(&mut request, "mode", Value::Unsigned(1));
    let bytes = canonical::encode(&Type::Record("request"), &request).unwrap();
    assert_eq!(
        &bytes[bytes.len() - 14..],
        canonical::unhex("0108070605040302010100000000").unwrap()
    );
    let observation = observation();
    let obs = canonical::unhex(
        "0107000000010000004b010000006105000000416c70686101000000020000000000000000",
    )
    .unwrap();
    assert_eq!(
        canonical::encode(&Type::Record("entity_observation"), &observation).unwrap(),
        obs
    );
    let row = accepted(
        vec![observation],
        vec![],
        vec![Value::Float(1.0), Value::Float(2.0)],
    );
    let chunk = canonical::encode(&Type::Identity, &template("chunk")).unwrap();
    let mut expected = vec![1];
    expected.extend((chunk.len() as u32).to_le_bytes());
    expected.extend(chunk);
    expected.extend(5u32.to_le_bytes());
    expected.extend(b"Alpha");
    expected.extend(1u32.to_le_bytes());
    expected.extend((obs.len() as u32).to_le_bytes());
    expected.extend(obs);
    expected.extend(0u32.to_le_bytes());
    expected.extend(2u32.to_le_bytes());
    expected.extend(1f32.to_le_bytes());
    expected.extend(2f32.to_le_bytes());
    assert_eq!(
        canonical::encode(&Type::Record("accepted_chunk"), &row).unwrap(),
        expected
    );
}

#[test]
fn pipeline_configuration_and_operation_role_negatives() {
    let base = template("pipeline");
    for (bytes, overlap) in [(1023, 0), (16385, 0), (1024, 257), (8192, 1025)] {
        let mut pipeline = base.clone();
        set(&mut pipeline, "chunk_bytes", Value::Unsigned(bytes));
        set(&mut pipeline, "overlap", Value::Unsigned(overlap));
        assert_eq!(
            canonical::encode(&Type::Record("pipeline"), &pipeline),
            Err("chunk_config")
        );
    }
    for (bytes, overlap) in [(1024, 256), (16384, 1024)] {
        let mut pipeline = base.clone();
        set(&mut pipeline, "chunk_bytes", Value::Unsigned(bytes));
        set(&mut pipeline, "overlap", Value::Unsigned(overlap));
        assert!(canonical::encode(&Type::Record("pipeline"), &pipeline).is_ok());
    }
    let rule = |name: &str| {
        map(&[
            ("rule_id", Value::Text(name.into())),
            ("predicate", Value::Text("P".into())),
            ("subject_kind", Value::Text("K".into())),
            ("object_kind", Value::Text("K".into())),
        ])
    };
    for rules in [vec![rule("b"), rule("a")], vec![rule("a"), rule("a")]] {
        let mut pipeline = base.clone();
        set(&mut pipeline, "relation_rules", Value::List(rules));
        assert_eq!(
            canonical::encode(&Type::Record("pipeline"), &pipeline),
            Err("rule_order")
        );
    }
    let mut request = template("request");
    set(&mut request, "text", Value::Bytes(vec![]));
    set(&mut request, "source", template("entity"));
    assert_eq!(
        canonical::encode(&Type::Record("request"), &request),
        Err("identity_role")
    );
    assert_eq!(
        canonical::encode(&Type::Operation, &remove(template("source"))),
        Err("operation_object_role")
    );
    for name in ["entity_observation", "relation_observation"] {
        let mut row = if name == "entity_observation" {
            observation()
        } else {
            relation_observation()
        };
        set(&mut row, "confidence", Value::Float(-0.5));
        assert_eq!(
            canonical::encode(&Type::Record(name), &row),
            Err("confidence")
        );
    }
    for (name, field, wrong) in [
        ("guard_operation", "source", "entity"),
        ("source_create_operation", "source", "entity"),
        ("detach_operation", "chunk", "entity"),
        ("attach_operation", "chunk", "entity"),
    ] {
        let mut operation = template(name);
        let mut fields = get(&operation, "fields").clone();
        set(&mut fields, field, template(wrong));
        set(&mut operation, "fields", fields);
        assert_eq!(
            canonical::encode(&Type::Operation, &operation),
            Err("operation_object_role"),
            "{name}"
        );
    }
    let patch = op(
        11,
        map(&[
            ("source", template("entity")),
            ("expected", template("revision_ref")),
            ("data", template("head_data")),
        ]),
    );
    assert_eq!(
        canonical::encode(&Type::Operation, &patch),
        Err("operation_object_role")
    );
}

#[test]
fn structural_positions_endpoints_and_creation_split() {
    for ordinal in [0, 1] {
        let batches = if ordinal == 0 {
            vec![batch(0, vec![group(0, vec![template("guard_operation")])])]
        } else {
            vec![
                batch(1, vec![group(0, vec![template("guard_operation")])]),
                batch(1, vec![group(0, vec![template("guard_operation")])]),
            ]
        };
        assert_eq!(encode_plan(&plan(batches)), Err("batch_order"));
    }
    let mut other_guard = template("guard_operation");
    let mut fields = get(&other_guard, "fields").clone();
    set(
        &mut fields,
        "expected",
        map(&[("tag", Value::Unsigned(1)), ("value", Value::Unsigned(2))]),
    );
    set(&mut other_guard, "fields", fields);
    for later in [other_guard, template("source_create_operation")] {
        let p = plan(vec![batch(
            1,
            vec![
                group(0, vec![template("guard_operation")]),
                group(1, vec![later]),
            ],
        )]);
        assert_eq!(encode_plan(&p), Err("batch_guard_first"));
    }
    let p = plan(vec![batch(
        1,
        vec![
            group(
                0,
                vec![template("guard_operation"), template("root_operation")],
            ),
            group(1, vec![template("create_entity_operation")]),
        ],
    )]);
    assert_eq!(encode_plan(&p), Err("root_receipt_last"));
    let p = plan(vec![
        batch(1, vec![group(0, vec![template("guard_operation")])]),
        batch(
            2,
            vec![group(
                0,
                vec![template("guard_operation"), template("root_operation")],
            )],
        ),
    ]);
    assert_eq!(encode_plan(&p), Err("root_receipt_last"));
    let chunk_create = object_op(5, template("chunk"), template("chunk_projection_data"));
    let other_create = object_op(
        5,
        template("chunk_other"),
        template("chunk_projection_data"),
    );
    let p = plan(vec![batch(
        1,
        vec![
            group(0, vec![template("guard_operation"), chunk_create.clone()]),
            group(1, vec![other_create]),
        ],
    )]);
    assert_eq!(encode_plan(&p), Err("chunk_ordinal_order"));
    for separate_batch in [false, true] {
        let first = group(0, vec![template("guard_operation"), chunk_create.clone()]);
        let mut p = if separate_batch {
            plan(vec![
                batch(1, vec![first]),
                batch(
                    2,
                    vec![group(
                        0,
                        vec![
                            template("guard_operation"),
                            template("has_chunk_edge_operation"),
                        ],
                    )],
                ),
            ])
        } else {
            plan(vec![batch(
                1,
                vec![first, group(1, vec![template("has_chunk_edge_operation")])],
            )])
        };
        captured(&mut p);
        assert_eq!(encode_plan(&p), Err("chunk_creation_split"));
    }
    let created_late = group(
        1,
        vec![object_op(
            5,
            template("chunk"),
            template("chunk_projection_data"),
        )],
    );
    let put_first = group(
        0,
        vec![template("guard_operation"), {
            let mut put = template("put_operation");
            let mut fields = get(&put, "fields").clone();
            let mut data = get(&fields, "data").clone();
            set(
                &mut data,
                "chunk_key",
                Value::Text(canonical::identity_key(&template("chunk")).unwrap()),
            );
            set(&mut fields, "data", data);
            set(&mut put, "fields", fields);
            put
        }],
    );
    assert_eq!(
        encode_plan(&plan(vec![batch(1, vec![put_first, created_late])])),
        Err("chunk_creation_split")
    );
    let edge_first = group(
        0,
        vec![
            template("guard_operation"),
            template("has_chunk_edge_operation"),
        ],
    );
    let chunk_later = group(
        1,
        vec![object_op(
            5,
            template("chunk"),
            template("chunk_projection_data"),
        )],
    );
    let mut p = plan(vec![batch(1, vec![edge_first, chunk_later])]);
    captured(&mut p);
    assert_eq!(encode_plan(&p), Err("chunk_creation_split"));
    let edge = identity("relation", {
        let relation = template("relation");
        let mut fields = get(&relation, "fields").clone();
        set(
            &mut fields,
            "object",
            change_identity(
                template("entity"),
                "normalized_key",
                Value::Text("unseen".into()),
            ),
        );
        fields
    });
    let mut p = plan(vec![batch(
        1,
        vec![group(
            0,
            vec![
                template("guard_operation"),
                object_op(8, edge, template("relation_projection_data")),
            ],
        )],
    )]);
    captured(&mut p);
    assert_eq!(encode_plan(&p), Err("edge_endpoint"));
}

#[test]
fn structural_collection_put_phase_and_edge_duplicates() {
    let mut detach = template("detach_operation");
    let mut fields = get(&detach, "fields").clone();
    set(&mut fields, "collection", Value::Unsigned(2));
    set(&mut detach, "fields", fields);
    let mut p = plan(vec![batch(
        1,
        vec![group(
            0,
            vec![
                template("guard_operation"),
                detach,
                template("delete_operation"),
            ],
        )],
    )]);
    captured(&mut p);
    assert_eq!(encode_plan(&p), Err("group_chunk_membership"));
    let mut put = template("put_operation");
    let mut fields = get(&put, "fields").clone();
    let mut data = get(&fields, "data").clone();
    set(&mut data, "chunk_key", Value::Text("unresolved".into()));
    set(&mut fields, "data", data);
    set(&mut put, "fields", fields);
    let mut p = plan(vec![batch(
        1,
        vec![group(0, vec![template("guard_operation"), put])],
    )]);
    captured(&mut p);
    assert_eq!(encode_plan(&p), Err("put_names_chunk"));
    let relation = object_op(
        8,
        template("relation"),
        template("relation_projection_data"),
    );
    let mention = template("mention_edge_operation");
    let mut p = plan(vec![batch(
        1,
        vec![group(
            0,
            vec![template("guard_operation"), relation, mention.clone()],
        )],
    )]);
    captured(&mut p);
    assert_eq!(encode_plan(&p), Err("operation_order"));
    let second = template("mention_edge_other_data_operation");
    let mut p = plan(vec![batch(
        1,
        vec![group(0, vec![template("guard_operation"), mention, second])],
    )]);
    captured(&mut p);
    assert_eq!(encode_plan(&p), Err("duplicate_edge"));
}

#[test]
fn abort_receipt_is_a_valid_object_role() {
    let identity = template("abort_receipt");
    let data = template("receipt_data");
    let object = map(&[("identity", identity), ("data", data)]);
    let bytes = canonical::encode(&Type::Object, &object).unwrap();
    assert_eq!(canonical::decode(&Type::Object, &bytes).unwrap(), object);
}

#[test]
fn typed_slot_masks_and_batch_binding() {
    let mut batches = Vec::new();
    for ordinal in 1..=2 {
        let id = identity(
            "batch_receipt",
            map(&[
                ("root", template("receipt")),
                ("ordinal", Value::Unsigned(ordinal)),
            ]),
        );
        let mut batch = batch(
            ordinal,
            vec![group(
                0,
                vec![
                    template("guard_operation"),
                    object_op(10, id, template("batch_data")),
                ],
            )],
        );
        let digest = canonical::hex(&canonical::plan::digest("batch", &batch).unwrap());
        fn fill(v: &mut Value, digest: &str, signed: bool) {
            match v {
                Value::Map(map) => {
                    if let Some(Value::Map(slot)) = map.get_mut("batch_digest") {
                        slot.insert("value".into(), Value::Text(digest.into()));
                        if signed {
                            slot.insert("tag".into(), Value::Integer(2));
                        }
                    }
                    for child in map.values_mut() {
                        fill(child, digest, signed);
                    }
                }
                Value::List(list) => {
                    for child in list {
                        fill(child, digest, signed);
                    }
                }
                _ => {}
            }
        }
        fill(&mut batch, &digest, false);
        let expected = canonical::plan::digest("batch", &batch).unwrap();
        let mut signed = batch.clone();
        fill(&mut signed, &digest, true);
        assert_eq!(
            canonical::encode(&Type::Record("batch"), &batch).unwrap(),
            canonical::encode(&Type::Record("batch"), &signed).unwrap()
        );
        assert_eq!(canonical::plan::digest("batch", &signed).unwrap(), expected);
        batches.push(signed);
    }
    canonical::plan::validate_slots(&plan(batches)).unwrap();
    let unsigned = plan(vec![batch(
        1,
        vec![group(
            0,
            vec![template("guard_operation"), template("root_operation")],
        )],
    )]);
    let mut signed = unsigned.clone();
    fn signed_plan_slot(value: &mut Value) {
        match value {
            Value::Map(map) => {
                if let Some(Value::Map(slot)) = map.get_mut("plan_digest") {
                    slot.insert("tag".into(), Value::Integer(1));
                }
                for value in map.values_mut() {
                    signed_plan_slot(value);
                }
            }
            Value::List(list) => {
                for value in list {
                    signed_plan_slot(value);
                }
            }
            _ => {}
        }
    }
    signed_plan_slot(&mut signed);
    assert_eq!(
        encode_plan(&unsigned).unwrap(),
        encode_plan(&signed).unwrap()
    );
    assert_eq!(
        canonical::plan::digest("plan", &unsigned).unwrap(),
        canonical::plan::digest("plan", &signed).unwrap()
    );
    for name in ["delete_action", "abort_action"] {
        let action = map(&[
            (
                "root",
                template(if name == "delete_action" {
                    "delete_receipt"
                } else {
                    "abort_receipt"
                }),
            ),
            ("policy_digest", Value::Text("11".repeat(32))),
            ("mode", Value::Unsigned(1)),
        ]);
        let bytes = canonical::encode(&Type::Record(name), &action).unwrap();
        let domain = if name == "delete_action" {
            "vlite.g7.v1.delete-action"
        } else {
            "vlite.g7.v1.abort-action"
        };
        assert_eq!(
            canonical::plan::digest(name, &action).unwrap(),
            canonical::framed_hash(domain, &bytes).unwrap()
        );
        let object = map(&[
            ("identity", get(&action, "root").clone()),
            ("data", template("receipt_data")),
        ]);
        let encoded = canonical::encode(&Type::Object, &object).unwrap();
        assert_eq!(canonical::decode(&Type::Object, &encoded).unwrap(), object);
    }
}

#[test]
fn vector_entry_order_and_slot_error_context() {
    let entry = map(&[
        ("producing_op", Value::Unsigned(3)),
        ("result_ordinal", Value::Unsigned(4)),
        ("action", Value::Unsigned(1)),
        ("object_kind", Value::Unsigned(2)),
        ("object_key", Value::Text(String::new())),
        (
            "id",
            map(&[
                ("tag", Value::Unsigned(1)),
                (
                    "value",
                    Value::List(vec![Value::Unsigned(5), Value::Unsigned(6)]),
                ),
            ]),
        ),
        (
            "revision",
            map(&[
                ("tag", Value::Unsigned(2)),
                (
                    "value",
                    Value::List(vec![Value::Unsigned(7), Value::Unsigned(8)]),
                ),
            ]),
        ),
        ("collection", Value::Unsigned(0x1122)),
        ("record", Value::Unsigned(0x0102030405060708)),
        ("attached_chunk_key", Value::Text("H".into())),
        ("retained", Value::Bool(false)),
    ]);
    let expected = canonical::unhex("01030000000400000001020000000001050000000600000002070000000800000001221101080706050403020101010000004800").unwrap();
    assert_eq!(
        canonical::encode(&Type::Record("entry"), &entry).unwrap(),
        expected
    );
    assert_eq!(
        canonical::decode(&Type::Record("entry"), &expected).unwrap(),
        entry
    );
    for (name, field, error, offset) in [
        ("receipt_data", "plan_digest", "plan_slot_kind", 33),
        ("batch_data", "batch_digest", "batch_slot_kind", 1),
    ] {
        let mut data = template(name);
        let mut bytes = canonical::encode(&Type::Record(name), &data).unwrap();
        bytes[offset] = 3;
        assert_eq!(canonical::decode(&Type::Record(name), &bytes), Err(error));
        set(
            &mut data,
            field,
            map(&[
                ("tag", Value::Unsigned(3)),
                ("value", Value::Text("00".repeat(32))),
            ]),
        );
        assert_eq!(canonical::encode(&Type::Record(name), &data), Err(error));
    }
    let id = source();
    let data = stored_head();
    let mut physical = canonical::stored::physical(&id, &data).unwrap();
    physical.remove("i00");
    assert_eq!(
        canonical::stored::validate_physical(&id, &data, &physical),
        Err("identity_conflict")
    );
    let mut physical = canonical::stored::physical(&id, &data).unwrap();
    physical.insert("i02".into(), Value::Text("AA==".into()));
    assert_eq!(
        canonical::stored::validate_physical(&id, &data, &physical),
        Err("evidence_conflict")
    );
    let mut graph_entry = template("entry");
    set(&mut graph_entry, "action", Value::Unsigned(1));
    set(&mut graph_entry, "object_kind", Value::Unsigned(1));
    set(&mut graph_entry, "producing_op", Value::Unsigned(3));
    set(&mut graph_entry, "result_ordinal", Value::Unsigned(4));
    set(&mut graph_entry, "object_key", Value::Text("graph".into()));
    set(
        &mut graph_entry,
        "id",
        map(&[("tag", Value::Unsigned(0)), ("value", Value::Unsigned(7))]),
    );
    let mut vector_entry = entry.clone();
    set(&mut vector_entry, "action", Value::Unsigned(0));
    set(&mut vector_entry, "object_kind", Value::Unsigned(2));
    assert!(
        canonical::stored::split_shards(
            &shard_header(),
            &[vector_entry.clone(), graph_entry.clone()]
        )
        .is_ok()
    );
    assert_eq!(
        canonical::stored::split_shards(&shard_header(), &[graph_entry, vector_entry]),
        Err("entry_order")
    );
}

#[test]
fn physical_fragment_gaps_are_rejected() {
    let id = source();
    let data = stored_head();
    let physical = canonical::stored::physical(&id, &data).unwrap();
    let mut missing = physical.clone();
    missing.remove("i00");
    assert_eq!(
        canonical::stored::validate_physical(&id, &data, &missing),
        Err("identity_conflict")
    );
    let mut gap = physical;
    gap.remove("i01");
    gap.insert(
        "i00".into(),
        Value::Text(canonical::stored::base64(&vec![1; 3072])),
    );
    gap.insert("i02".into(), Value::Text(canonical::stored::base64(&[2])));
    assert!(canonical::stored::validate_physical(&id, &data, &gap).is_err());
}
