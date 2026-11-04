#[allow(dead_code)]
#[path = "../src/ingestion/canonical/mod.rs"]
mod canonical;

use canonical::{Type, Value};
use serde_json::Value as Json;
use std::collections::BTreeMap;

fn fixture() -> Json {
    serde_json::from_str(include_str!("fixtures/ingestion/schema-v1/vectors.json")).unwrap()
}

fn expand(value: &Json, templates: &Json) -> Json {
    match value {
        Json::Object(map) if map.contains_key("ref") => {
            assert_eq!(map.len(), 1);
            expand(&templates[value["ref"].as_str().unwrap()], templates)
        }
        Json::Object(map) if map.contains_key("repeat") => {
            assert_eq!(map.len(), 2);
            Json::String(
                value["repeat"]
                    .as_str()
                    .unwrap()
                    .repeat(value["count"].as_u64().unwrap() as usize),
            )
        }
        Json::Object(map) => Json::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), expand(value, templates)))
                .collect(),
        ),
        Json::Array(values) => Json::Array(
            values
                .iter()
                .map(|value| expand(value, templates))
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn value(input: &Json) -> Value {
    match input {
        Json::Null => Value::Null,
        Json::Bool(v) => Value::Bool(*v),
        Json::Number(v) if v.is_f64() => Value::Float(v.as_f64().unwrap()),
        Json::Number(v) if v.is_u64() => Value::Unsigned(v.as_u64().unwrap()),
        Json::Number(v) => Value::Integer(v.as_i64().unwrap()),
        Json::String(v) => Value::Text(v.clone()),
        Json::Array(v) => Value::List(v.iter().map(value).collect()),
        Json::Object(v) => Value::Map(v.iter().map(|(key, v)| (key.clone(), value(v))).collect()),
    }
}

fn kind(input: &Json) -> Type {
    if let Some(pair) = input.as_array() {
        let inner = Box::new(kind(&pair[1]));
        return match pair[0].as_str().unwrap() {
            "map" => Type::Map(inner),
            "optional" => Type::Optional(inner),
            "list" => Type::List(inner),
            other => panic!("unhandled type {other}"),
        };
    }
    match input.as_str().unwrap() {
        "u8" => Type::U8,
        "u16" => Type::U16,
        "u32" => Type::U32,
        "u64" => Type::U64,
        "i64" => Type::I64,
        "f32" => Type::F32,
        "f64" => Type::F64,
        "bool" => Type::Bool,
        "text" => Type::Text,
        "bytes" => Type::Bytes,
        "digest" => Type::Digest,
        "binding" => Type::Binding,
        "attribute" => Type::Attribute,
        "attributes" => Type::Attributes,
        "id_ref" => Type::IdRef,
        "revision_ref" => Type::RevisionRef,
        "slot" => Type::Slot,
        "head_data" => Type::Record("head_data"),
        "revision_data" => Type::Record("revision_data"),
        "chunk_data" => Type::Record("chunk_data"),
        "entity_data" => Type::Record("entity_data"),
        "link_data" => Type::Record("link_data"),
        "mention_data" => Type::Record("mention_data"),
        "relation_data" => Type::Record("relation_data"),
        "receipt_data" => Type::Record("receipt_data"),
        "batch_data" => Type::Record("batch_data"),
        "vector_data" => Type::Record("vector_data"),
        "entry" => Type::Record("entry"),
        "operation" => Type::Operation,
        "policy" => Type::Record("policy"),
        "pipeline" => Type::Record("pipeline"),
        "request" => Type::Record("request"),
        "entity_rule" => Type::Record("entity_rule"),
        "relation_rule" => Type::Record("relation_rule"),
        "entity_observation" => Type::Record("entity_observation"),
        "relation_observation" => Type::Record("relation_observation"),
        "accepted_chunk" => Type::Record("accepted_chunk"),
        "captured" => Type::Record("captured"),
        "group" => Type::Record("group"),
        "batch" => Type::Record("batch"),
        "plan" => Type::Record("plan"),
        "delete_action" => Type::Record("delete_action"),
        "abort_action" => Type::Record("abort_action"),
        other => panic!("unhandled type {other}"),
    }
}

fn expected_hex(value: &Json, atoms: &Json) -> Vec<u8> {
    match value {
        Json::String(s) if s.starts_with('@') => expected_hex(&atoms[&s[1..]], atoms),
        Json::String(s) => canonical::unhex(s).unwrap(),
        Json::Array(values) => values.iter().flat_map(|v| expected_hex(v, atoms)).collect(),
        Json::Object(map) if map.contains_key("repeat_hex") => {
            canonical::unhex(value["repeat_hex"].as_str().unwrap())
                .unwrap()
                .repeat(value["count"].as_u64().unwrap() as usize)
        }
        _ => panic!("unsupported hex fixture {value}"),
    }
}

fn encode_case(case: &Json, templates: &Json) -> canonical::Result<Vec<u8>> {
    let expanded = expand(&case["input"], templates);
    let t = if case["category"] == "identity" {
        Type::Identity
    } else if case["category"] == "attribute_key" {
        return canonical::encode(
            &Type::Attributes,
            &map(&[(
                expanded.as_str().unwrap(),
                Value::List(vec![Value::Unsigned(0), Value::Null]),
            )]),
        );
    } else if case["category"] == "shard" {
        Type::Record("shard")
    } else {
        kind(&case["type"])
    };
    let mut v = if matches!(t, Type::Bytes) {
        Value::Bytes(canonical::unhex(expanded.as_str().unwrap())?)
    } else {
        value(&expanded)
    };
    if matches!(t, Type::Record("vector_data")) {
        vector_text(&mut v)?;
    }
    canonical::encode(&t, &v)
}

fn vector_text(v: &mut Value) -> canonical::Result<()> {
    let Value::Map(map) = v else {
        panic!("vector map")
    };
    let Value::Text(s) = &map["text"] else {
        panic!("vector text")
    };
    let bytes = canonical::unhex(s)?;
    map.insert("text".into(), Value::Bytes(bytes));
    Ok(())
}

fn stored_selected(case: &Json) -> bool {
    matches!(
        case["category"].as_str(),
        Some("projection" | "vector_record" | "shard" | "collision")
    ) || (case["category"] == "record"
        && matches!(
            case["type"].as_str(),
            Some(
                "head_data"
                    | "revision_data"
                    | "chunk_data"
                    | "entity_data"
                    | "link_data"
                    | "mention_data"
                    | "relation_data"
                    | "receipt_data"
                    | "batch_data"
                    | "vector_data"
                    | "entry"
            )
        ))
}

fn selected(case: &Json) -> bool {
    matches!(
        case["category"].as_str(),
        Some("primitive" | "identity" | "ordering" | "attribute_key")
    ) || (case.get("decode_hex").is_some() && case["type"] != "operation")
}

#[test]
fn ingestion_canonical_foundation_vectors() {
    let fixture = fixture();
    assert_eq!(fixture["schema"], "vlite.g7.canonical-v1.proposed");
    let templates = &fixture["templates"];
    let atoms = &fixture["atoms"];
    let mut positive = 0;
    let mut negative = 0;
    for case in fixture["vectors"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| selected(case))
    {
        let name = case["name"].as_str().unwrap();
        if case["category"] == "ordering" {
            let expanded = expand(&case["input"], templates);
            let mut rows: Vec<_> = expanded
                .as_object()
                .unwrap()
                .iter()
                .map(|(key, v)| (key, value(v)))
                .collect();
            rows.sort_by(|a, b| canonical::compare_identities(&a.1, &b.1).unwrap());
            assert_eq!(
                serde_json::json!(rows.iter().map(|row| row.0).collect::<Vec<_>>()),
                case["order"],
                "{name}"
            );
            let mut actual = Vec::new();
            for (_, row) in rows {
                let bytes = canonical::encode(&Type::Identity, &row).unwrap();
                actual.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                actual.extend_from_slice(&bytes);
            }

            assert_eq!(actual, expected_hex(&case["hex"], atoms), "{name}");
        } else {
            let actual =
                encode_case(case, templates).unwrap_or_else(|error| panic!("{name}: {error}"));
            assert_eq!(actual, expected_hex(&case["hex"], atoms), "{name}");
            let t = if case["category"] == "identity" {
                Type::Identity
            } else if case["category"] == "attribute_key" {
                Type::Attributes
            } else {
                kind(&case["type"])
            };
            let decoded =
                canonical::decode(&t, &actual).unwrap_or_else(|error| panic!("{name}: {error}"));
            assert_eq!(canonical::encode(&t, &decoded).unwrap(), actual, "{name}");
            if let Some(key) = case["key"].as_str() {
                assert_eq!(canonical::identity_key(&decoded).unwrap(), key, "{name}");
            }
        }
        positive += 1;
    }
    for case in fixture["negatives"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| selected(case))
    {
        let name = case["name"].as_str().unwrap();
        let error = if let Some(bytes) = case.get("decode_hex") {
            canonical::decode(&kind(&case["type"]), &expected_hex(bytes, atoms)).unwrap_err()
        } else {
            encode_case(case, templates).unwrap_err()
        };
        assert_eq!(error, case["error"].as_str().unwrap(), "{name}");
        negative += 1;
    }
    assert_eq!(
        (positive, negative),
        (45, 28),
        "bounded foundation coverage; remaining records, physical maps, plan validation and frame vectors are NOT implemented"
    );
    assert_eq!(fixture["vectors"].as_array().unwrap().len(), 113);
    assert_eq!(fixture["negatives"].as_array().unwrap().len(), 110);
}

fn json(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Unsigned(n) => Json::from(*n),
        Value::Integer(n) => Json::from(*n),
        Value::Float(n) => Json::from(*n),
        Value::Text(s) => Json::String(s.clone()),
        Value::Bytes(b) => Json::String(canonical::hex(b)),
        Value::List(values) => Json::Array(values.iter().map(json).collect()),
        Value::Map(map) => Json::Object(map.iter().map(|(k, v)| (k.clone(), json(v))).collect()),
    }
}

fn projection_case(case: &Json, templates: &Json) -> canonical::Result<Json> {
    let input = expand(&case["input"], templates);
    let identity = value(&input["identity"]);
    let data = value(&input["data"]);
    let expected = canonical::stored::projections(&identity, &data)?;
    if let Some(change) = input["change"].as_str() {
        let mut physical = canonical::stored::physical(&identity, &data)?;
        match change {
            "missing" => {
                physical.remove(input["property"].as_str().unwrap());
            }
            "mismatch" => {
                physical.insert(
                    input["property"].as_str().unwrap().into(),
                    Value::Integer(input["value"].as_i64().unwrap()),
                );
            }
            "extra" => {
                physical.insert("unexpected".into(), Value::Integer(1));
            }
            _ => panic!("fixture change"),
        }
        canonical::stored::validate_physical(&identity, &data, &physical)?;
    }
    Ok(json(&Value::Map(expected)))
}

#[test]
fn ingestion_canonical_stored_vectors() {
    let fixture = fixture();
    let templates = &fixture["templates"];
    let mut counts = [0, 0];
    for (index, group) in ["vectors", "negatives"].iter().enumerate() {
        for case in fixture[*group]
            .as_array()
            .unwrap()
            .iter()
            .filter(|case| stored_selected(case))
        {
            let name = case["name"].as_str().unwrap();
            let actual: canonical::Result<Json> = match case["category"].as_str().unwrap() {
                "projection" => projection_case(case, templates),
                "vector_record" => {
                    let mut data = value(&expand(&case["input"], templates));
                    vector_text(&mut data).unwrap();
                    canonical::stored::vector_record(&data).map(|v| json(&v))
                }
                "collision" => {
                    let input = expand(&case["input"], templates);
                    let left = value(&input["left"]);
                    let right = value(&input["right"]);
                    let data = map(&[
                        ("revision_key", Value::Null),
                        ("revision_digest", Value::Null),
                        ("pipeline", Value::Null),
                        ("status", Value::Unsigned(0)),
                        ("job_receipt_key", Value::Null),
                        ("last_batch", Value::Unsigned(0)),
                    ]);
                    let stored = canonical::stored::physical(&right, &data).unwrap();
                    canonical::stored::validate_physical(&left, &data, &stored).map(|_| Json::Null)
                }
                _ => encode_case(case, templates).and_then(|bytes| {
                    let t = if case["category"] == "shard" {
                        Type::Record("shard")
                    } else {
                        kind(&case["type"])
                    };
                    let decoded = canonical::decode(&t, &bytes)?;
                    assert_eq!(canonical::encode(&t, &decoded)?, bytes, "{name}");
                    Ok(Json::String(canonical::hex(&bytes)))
                }),
            };
            if index == 0 {
                let expected = if let Some(hex) = case.get("hex") {
                    Json::String(canonical::hex(&expected_hex(hex, &fixture["atoms"])))
                } else {
                    expand(&case["expected"], templates)
                };
                assert_eq!(
                    actual.unwrap_or_else(|e| panic!("{name}: {e}")),
                    expected,
                    "{name}"
                );
            } else {
                assert_eq!(
                    actual.unwrap_err(),
                    case["error"].as_str().unwrap(),
                    "{name}"
                );
            }
            counts[index] += 1;
        }
    }
    assert_eq!(counts, [27, 21], "stored layer vector coverage");
}
fn map(items: &[(&str, Value)]) -> Value {
    Value::Map(
        items
            .iter()
            .map(|(k, v)| ((*k).into(), v.clone()))
            .collect(),
    )
}

fn source() -> Value {
    value(&expand(
        &fixture()["templates"]["source"],
        &fixture()["templates"],
    ))
}

fn plan_value(input: &Json, templates: &Json) -> Value {
    fn convert(value: &mut Value) {
        match value {
            Value::Map(map) => {
                if map.contains_key("text")
                    && let Some(Value::Text(s)) = map.get("text")
                {
                    map.insert("text".into(), Value::Bytes(canonical::unhex(s).unwrap()));
                }
                for v in map.values_mut() {
                    convert(v);
                }
            }
            Value::List(items) => {
                for v in items {
                    convert(v);
                }
            }
            _ => {}
        }
    }
    let mut v = value(&expand(input, templates));
    convert(&mut v);
    v
}

fn full_policy() -> Value {
    map(&[
        ("source_bytes", Value::Unsigned(1048576)),
        ("chunks", Value::Unsigned(512)),
        ("entities", Value::Unsigned(4096)),
        ("relations", Value::Unsigned(8192)),
        ("mentions", Value::Unsigned(16384)),
        ("chunk_entities", Value::Unsigned(256)),
        ("chunk_relations", Value::Unsigned(512)),
        ("decoded_bytes", Value::Unsigned(1048576)),
        ("attribute_bytes", Value::Unsigned(8192)),
        ("attribute_fields", Value::Unsigned(16)),
        ("attribute_string_bytes", Value::Unsigned(4096)),
        ("frames", Value::Unsigned(64)),
        ("operations", Value::Unsigned(4096)),
        ("prepared_bytes", Value::Unsigned(67108864)),
    ])
}

#[test]
fn ingestion_plan_budget_before_materialization() {
    let mut policy = full_policy();
    set(&mut policy, "prepared_bytes", Value::Unsigned(257));
    assert_eq!(
        canonical::decode_with_policy(&Type::Bool, &[1], &policy).unwrap(),
        Value::Bool(true)
    );
    set(&mut policy, "prepared_bytes", Value::Unsigned(256));
    assert_eq!(
        canonical::decode_with_policy(&Type::Bool, &[1], &policy),
        Err("prepared_bytes")
    );
    // Invalid Bool would yield "boolean" if any Value were materialized before the full budget scan.
    set(&mut policy, "prepared_bytes", Value::Unsigned(256));
    assert_eq!(
        canonical::decode_with_policy(&Type::Bool, &[2], &policy),
        Err("prepared_bytes")
    );
    let mut large = 300000u32.to_le_bytes().to_vec();
    large.extend_from_slice(&vec![2; 300000]);
    assert_eq!(
        canonical::decode(&Type::List(Box::new(Type::Bool)), &large),
        Err("prepared_bytes")
    );
    let mut policy = full_policy();
    set(&mut policy, "prepared_bytes", Value::Unsigned(67108865));
    assert_eq!(
        canonical::decode_with_policy(&Type::Bool, &[1], &policy),
        Err("policy")
    );
    let chunk = identity(
        "chunk",
        match template("chunk") {
            Value::Map(mut m) => m.remove("fields").unwrap(),
            _ => unreachable!(),
        },
    );
    let observation = map(&[
        ("local_id", Value::Unsigned(0)),
        ("kind", Value::Text("K".into())),
        ("key", Value::Text("a".into())),
        ("surface", Value::Text("a".into())),
        ("start", Value::Unsigned(0)),
        ("end", Value::Unsigned(1)),
        ("attributes", map(&[])),
        ("confidence", Value::Null),
    ]);
    let accepted = map(&[
        ("chunk", chunk),
        ("text", Value::Bytes(b"a".to_vec())),
        ("entities", Value::List(vec![observation])),
        ("relations", Value::List(vec![])),
        ("vector", Value::List(vec![])),
    ]);
    let bytes = canonical::encode(&Type::Record("accepted_chunk"), &accepted).unwrap();
    let mut policy = full_policy();
    set(&mut policy, "decoded_bytes", Value::Unsigned(46));
    assert!(
        canonical::decode_with_policy(&Type::Record("accepted_chunk"), &bytes, &policy).is_ok()
    );
    set(&mut policy, "decoded_bytes", Value::Unsigned(45));
    assert_eq!(
        canonical::decode_with_policy(&Type::Record("accepted_chunk"), &bytes, &policy),
        Err("decoded_bytes")
    );
    set(&mut policy, "decoded_bytes", Value::Unsigned(46));
    set(&mut policy, "chunk_entities", Value::Unsigned(0));
    assert_eq!(
        canonical::decode_with_policy(&Type::Record("accepted_chunk"), &bytes, &policy),
        Err("policy_limit")
    );
    let mut policy = full_policy();
    set(&mut policy, "frames", Value::Unsigned(0));
    let f = fixture();
    let plan = plan_value(&f["templates"]["plan_with_slots"], &f["templates"]);
    let bytes = canonical::encode(&Type::Record("plan"), &plan).unwrap();
    assert_eq!(
        canonical::decode_with_policy(&Type::Record("plan"), &bytes, &policy),
        Err("policy_limit")
    );
    let mut policy = full_policy();
    set(&mut policy, "operations", Value::Unsigned(1));
    assert_eq!(
        canonical::decode_with_policy(&Type::Record("plan"), &bytes, &policy),
        Err("policy_limit")
    );
}

fn filled_plan() -> Value {
    let f = fixture();
    let mut plan = plan_value(&f["templates"]["plan_with_slots"], &f["templates"]);
    let plan_hash = canonical::hex(&canonical::plan::digest("plan", &plan).unwrap());
    fn fill(value: &mut Value, plan_hash: &str) {
        match value {
            Value::Map(map) => {
                if let Some(Value::Map(slot)) = map.get_mut("plan_digest") {
                    slot.insert("value".into(), Value::Text(plan_hash.into()));
                }
                for v in map.values_mut() {
                    fill(v, plan_hash);
                }
            }
            Value::List(items) => {
                for v in items {
                    fill(v, plan_hash);
                }
            }
            _ => {}
        }
    }
    fill(&mut plan, &plan_hash);
    plan
}

#[test]
fn ingestion_plan_digests_and_typed_slots() {
    let plan = filled_plan();
    canonical::plan::validate_slots(&plan).unwrap();
    let f = fixture();
    let unfilled = plan_value(&f["templates"]["plan_with_slots"], &f["templates"]);
    assert_eq!(
        canonical::plan::digest("plan", &plan).unwrap(),
        canonical::plan::digest("plan", &unfilled).unwrap()
    );
    assert_eq!(
        canonical::plan::validate_slots(&unfilled),
        Err("digest_slot")
    );
    let mut changed = plan.clone();
    set(&mut changed, "request_digest", Value::Text("01".repeat(32)));
    assert_ne!(
        canonical::plan::digest("plan", &plan).unwrap(),
        canonical::plan::digest("plan", &changed).unwrap()
    );
    assert_eq!(
        canonical::plan::validate_slots(&changed),
        Err("digest_slot")
    );
    for (name, input) in [
        ("pipeline", template("pipeline")),
        ("policy", full_policy()),
        (
            "request",
            plan_value(&f["templates"]["request"], &f["templates"]),
        ),
    ] {
        let bytes = canonical::encode(&Type::Record(name), &input).unwrap();
        assert_eq!(
            canonical::plan::digest(name, &input).unwrap(),
            canonical::framed_hash(&format!("vlite.g7.v1.{name}"), &bytes).unwrap()
        );
    }
    assert_eq!(
        canonical::plan::frame_length(16777216, false).unwrap(),
        16777268
    );
    assert_eq!(
        canonical::plan::frame_length(16777216, true).unwrap(),
        16777296
    );
    assert_eq!(
        canonical::plan::frame_length(16777217, false),
        Err("body_limit")
    );
    assert_eq!(
        canonical::plan::frame_length(u64::MAX, true),
        Err("body_limit")
    );
    let mut extended = filled_plan();
    let batch_op = f["vectors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "op-create-batch")
        .unwrap();
    let batch_op = plan_value(&batch_op["input"], &f["templates"]);
    let Value::Map(root) = &mut extended else {
        panic!("plan")
    };
    let Value::List(batches) = root.get_mut("batches").unwrap() else {
        panic!("batches")
    };
    let Value::Map(batch) = &mut batches[0] else {
        panic!("batch")
    };
    let Value::List(groups) = batch.get_mut("groups").unwrap() else {
        panic!("groups")
    };
    let Value::Map(group) = &mut groups[0] else {
        panic!("group")
    };
    let Value::List(ops) = group.get_mut("operations").unwrap() else {
        panic!("operations")
    };
    ops.insert(ops.len() - 1, batch_op);
    fn fill(value: &mut Value, key: &str, digest: &str) {
        match value {
            Value::Map(map) => {
                if let Some(Value::Map(slot)) = map.get_mut(key) {
                    slot.insert("value".into(), Value::Text(digest.into()));
                }
                for item in map.values_mut() {
                    fill(item, key, digest);
                }
            }
            Value::List(items) => {
                for item in items {
                    fill(item, key, digest);
                }
            }
            _ => {}
        }
    }
    let original = canonical::plan::digest("plan", &extended).unwrap();
    let Value::Map(root) = &extended else {
        panic!("plan")
    };
    let Value::List(batches) = &root["batches"] else {
        panic!("batches")
    };
    let batch_hash = canonical::plan::digest("batch", &batches[0]).unwrap();
    fill(&mut extended, "batch_digest", &canonical::hex(&batch_hash));
    fill(&mut extended, "plan_digest", &canonical::hex(&original));
    canonical::plan::validate_slots(&extended).unwrap();
    assert_eq!(
        canonical::plan::digest("plan", &extended).unwrap(),
        original
    );
    fill(&mut extended, "batch_digest", &"ff".repeat(32));
    assert_eq!(
        canonical::plan::validate_slots(&extended),
        Err("digest_slot")
    );
    assert_eq!(
        canonical::plan::digest("plan", &extended).unwrap(),
        original
    );
}

fn plan_selected(case: &Json) -> bool {
    !selected(case) && !stored_selected(case)
}

#[test]
fn ingestion_plan_config_and_record_guards() {
    let f = fixture();
    let mut pipeline = template("pipeline");
    set(&mut pipeline, "schema_version", Value::Unsigned(2));
    assert_eq!(
        canonical::encode(&Type::Record("pipeline"), &pipeline),
        Err("pipeline_version")
    );
    let mut policy = full_policy();
    for (key, cap) in [
        ("source_bytes", 1048576),
        ("chunks", 512),
        ("entities", 4096),
        ("relations", 8192),
        ("mentions", 16384),
        ("chunk_entities", 256),
        ("chunk_relations", 512),
        ("decoded_bytes", 1048576),
        ("attribute_bytes", 8192),
        ("attribute_fields", 16),
        ("attribute_string_bytes", 4096),
        ("frames", 64),
        ("operations", 131072),
        ("prepared_bytes", 67108864),
    ] {
        set(&mut policy, key, Value::Unsigned(cap));
        assert!(
            canonical::encode(&Type::Record("policy"), &policy).is_ok(),
            "{key}"
        );
        set(&mut policy, key, Value::Unsigned(cap + 1));
        assert_eq!(
            canonical::encode(&Type::Record("policy"), &policy),
            Err("policy"),
            "{key}"
        );
        set(&mut policy, key, Value::Unsigned(1));
    }
    let row = f["vectors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "entity-observation")
        .unwrap();
    let observation = plan_value(&row["input"], &f["templates"]);
    for (key, bad, error) in [
        ("key", Value::Text("UPPER".into()), "normalized_key"),
        ("end", Value::Unsigned(0), "evidence_span"),
        ("kind", Value::Text("k-".into()), "entity_kind"),
    ] {
        let mut bad_row = observation.clone();
        set(&mut bad_row, key, bad);
        assert_eq!(
            canonical::encode(&Type::Record("entity_observation"), &bad_row),
            Err(error)
        );
    }
    let mut plan = filled_plan();
    let Value::Map(root) = &mut plan else {
        panic!("plan")
    };
    let Value::List(batches) = root.get_mut("batches").unwrap() else {
        panic!("batches")
    };
    set(&mut batches[0], "ordinal", Value::Unsigned(2));
    assert_eq!(
        canonical::encode(&Type::Record("plan"), &plan),
        Err("batch_order")
    );
    let mut plan = filled_plan();
    let Value::Map(root) = &mut plan else {
        panic!("plan")
    };
    let Value::List(batches) = root.get_mut("batches").unwrap() else {
        panic!("batches")
    };
    let Value::Map(batch) = &mut batches[0] else {
        panic!("batch")
    };
    let Value::List(groups) = batch.get_mut("groups").unwrap() else {
        panic!("groups")
    };
    let Value::Map(group) = &mut groups[0] else {
        panic!("group")
    };
    let Value::List(ops) = group.get_mut("operations").unwrap() else {
        panic!("ops")
    };
    let create_entity = plan_value(&f["templates"]["create_entity_operation"], &f["templates"]);
    ops.push(create_entity);
    assert_eq!(
        canonical::encode(&Type::Record("plan"), &plan),
        Err("root_receipt_last")
    );
    let mut rule = map(&[
        ("rule_id", Value::Text("r".into())),
        ("literal", Value::Text("A".into())),
        ("kind", Value::Text("K".into())),
        ("key", Value::Text(" A ".into())),
    ]);
    assert!(canonical::encode(&Type::Record("entity_rule"), &rule).is_ok());
    set(&mut rule, "rule_id", Value::Text("".into()));
    assert_eq!(
        canonical::encode(&Type::Record("entity_rule"), &rule),
        Err("rule_id")
    );
}

fn remaining_case(case: &Json, templates: &Json) -> canonical::Result<(Vec<u8>, Type, Value)> {
    let category = case["category"].as_str().unwrap_or("decode");
    if category == "frame" {
        let input = &case["input"];
        return canonical::plan::frame_length(
            input["body"].as_u64().unwrap(),
            input["encrypted"].as_bool().unwrap(),
        )
        .map(|n| {
            (
                n.to_le_bytes().to_vec(),
                Type::U32,
                Value::Unsigned(n.into()),
            )
        });
    }
    let mut input = expand(&case["input"], templates);
    let t = match category {
        "operation" | "decode" => Type::Operation,
        "plan_order" => Type::Record("plan"),
        "pipeline_label" => {
            let field = input["field"].as_str().unwrap().to_string();
            input["pipeline"][&field] = input["value"].clone();
            input = input["pipeline"].clone();
            Type::Record("pipeline")
        }
        _ => kind(&case["type"]),
    };
    if let Some(hex) = case["decode_hex"].as_str() {
        let bytes = canonical::unhex(hex)?;
        return canonical::decode(&t, &bytes).map(|v| (bytes, t, v));
    }
    let v = plan_value(&input, templates);
    canonical::encode(&t, &v).map(|bytes| (bytes, t, v))
}

#[test]
fn ingestion_canonical_complete_plan_vectors() {
    let fixture = fixture();
    let mut counts = [0, 0];
    for (index, group) in ["vectors", "negatives"].iter().enumerate() {
        for case in fixture[*group]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| plan_selected(c))
        {
            let name = case["name"].as_str().unwrap();
            let actual = remaining_case(case, &fixture["templates"]);
            if index == 1 {
                assert_eq!(
                    actual.unwrap_err(),
                    case["error"].as_str().unwrap(),
                    "{name}"
                );
            } else {
                let (bytes, t, v) = actual.unwrap_or_else(|e| panic!("{name}: {e}"));
                if case["category"] == "plan_order" {
                    let tags: Vec<_> = json(&v)["batches"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|b| {
                            b["groups"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .map(|g| {
                                    g["operations"]
                                        .as_array()
                                        .unwrap()
                                        .iter()
                                        .map(|o| o["tag"].clone())
                                        .collect::<Vec<_>>()
                                })
                                .collect::<Vec<_>>()
                        })
                        .collect();
                    assert_eq!(serde_json::json!(tags), case["expected"], "{name}");
                } else {
                    assert_eq!(
                        bytes,
                        expected_hex(&case["hex"], &fixture["atoms"]),
                        "{name}"
                    );
                }
                let decoded =
                    canonical::decode(&t, &bytes).unwrap_or_else(|e| panic!("{name} decode: {e}"));
                assert_eq!(
                    canonical::encode(&t, &decoded).unwrap(),
                    bytes,
                    "{name} roundtrip"
                );
                if let Some(expected) = case["framed_sha256"].as_str() {
                    let Type::Record(record) = t else {
                        panic!("digest fixture")
                    };
                    assert_eq!(
                        canonical::hex(&canonical::plan::digest(record, &v).unwrap()),
                        expected,
                        "{name} digest"
                    );
                }
            }
            counts[index] += 1;
        }
    }
    assert_eq!(counts, [41, 61]);
}

fn template(name: &str) -> Value {
    let f = fixture();
    if name == "relation" {
        let case = f["vectors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "projection-relation")
            .unwrap();
        return value(&expand(&case["input"]["identity"], &f["templates"]));
    }
    value(&expand(&f["templates"][name], &f["templates"]))
}

fn set(v: &mut Value, key: &str, item: Value) {
    let Value::Map(map) = v else { panic!("map") };
    map.insert(key.into(), item);
}

fn get<'a>(v: &'a Value, key: &str) -> &'a Value {
    let Value::Map(map) = v else { panic!("map") };
    &map[key]
}

fn stored_head() -> Value {
    template("head_data")
}

#[test]
fn ingestion_stored_record_guards_and_object_framing() {
    use canonical::stored;
    let receipt = map(&[
        ("request_digest", Value::Text("11".repeat(32))),
        (
            "plan_digest",
            map(&[
                ("tag", Value::Unsigned(1)),
                ("value", Value::Text("22".repeat(32))),
            ]),
        ),
        ("batch_count", Value::Unsigned(0x01020304)),
        ("revision_key", Value::Text("r".into())),
        ("outcome", Value::Unsigned(3)),
        ("chunks", Value::Unsigned(2)),
        ("entities", Value::Unsigned(3)),
        ("relations", Value::Unsigned(5)),
        ("mentions", Value::Unsigned(7)),
    ]);
    assert_eq!(
        canonical::hex(&canonical::encode(&Type::Record("receipt_data"), &receipt).unwrap()),
        format!(
            "01{}01{}0403020101000000720302000000030000000500000007000000",
            "11".repeat(32),
            "22".repeat(32)
        )
    );
    for (name, key, mut data, maximum) in [
        ("head_data", "status", stored_head(), 3),
        ("receipt_data", "outcome", template("receipt_data"), 3),
        ("entry", "action", template("entry"), 2),
    ] {
        set(&mut data, key, Value::Unsigned(maximum));
        assert!(canonical::encode(&Type::Record(name), &data).is_ok());
        set(&mut data, key, Value::Unsigned(maximum + 1));
        assert_eq!(canonical::encode(&Type::Record(name), &data), Err("enum"));
    }
    for (name, mut data) in [
        ("head_data", stored_head()),
        ("receipt_data", template("receipt_data")),
        ("batch_data", template("batch_data")),
        ("entry", template("entry")),
    ] {
        let bytes = canonical::encode(&Type::Record(name), &data).unwrap();
        let mut bad = bytes.clone();
        bad[0] = 2;
        assert_eq!(canonical::decode(&Type::Record(name), &bad), Err("version"));
        for end in 0..bytes.len() {
            assert!(canonical::decode(&Type::Record(name), &bytes[..end]).is_err());
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert_eq!(
            canonical::decode(&Type::Record(name), &trailing),
            Err("noncanonical")
        );
        set(&mut data, "unexpected", Value::Null);
        assert_eq!(canonical::encode(&Type::Record(name), &data), Err("fields"));
    }
    let source = source();
    let data = stored_head();
    let bytes = stored::encode_object(&source, &data).unwrap();
    let id = canonical::encode(&Type::Identity, &source).unwrap();
    assert_eq!(&bytes[..4], &59u32.to_le_bytes());
    assert_eq!(&bytes[4..63], id);
    assert_eq!(&bytes[63..67], &10u32.to_le_bytes());
    assert_eq!(&bytes[67..], &[1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    let (decoded_id, decoded_data) = stored::decode_object(&bytes).unwrap();
    assert_eq!(
        stored::encode_object(&decoded_id, &decoded_data).unwrap(),
        bytes
    );
    let mut extra = bytes;
    extra.push(0);
    assert_eq!(stored::decode_object(&extra), Err("noncanonical"));
    for name in ["mention_data", "relation_data"] {
        let mut data = template(if name == "mention_data" {
            "mention_projection_data"
        } else {
            "relation_projection_data"
        });
        for invalid in ["", "LATEST", "Default", "v 1", "é", "v\0"] {
            set(&mut data, "extractor_version", Value::Text(invalid.into()));
            assert_eq!(
                canonical::encode(&Type::Record(name), &data),
                Err("extractor_version")
            );
        }
        for len in [1, 256, 257] {
            set(&mut data, "extractor_version", Value::Text("x".repeat(len)));
            assert_eq!(
                canonical::encode(&Type::Record(name), &data).is_ok(),
                len <= 256
            );
        }
        set(
            &mut data,
            "extractor_version",
            Value::Text("A_z-1.0".into()),
        );
        for confidence in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1e-9, 1.00001] {
            set(&mut data, "confidence", Value::Float(confidence));
            assert_eq!(
                canonical::encode(&Type::Record(name), &data),
                Err("confidence")
            );
        }
        set(&mut data, "confidence", Value::Float(-0.0));
        let negative = canonical::encode(&Type::Record(name), &data).unwrap();
        set(&mut data, "confidence", Value::Float(0.0));
        assert_eq!(
            negative,
            canonical::encode(&Type::Record(name), &data).unwrap()
        );
    }
    let mut vector = template("vector_projection_data");
    vector_text(&mut vector).unwrap();
    set(
        &mut vector,
        "vector",
        Value::List(vec![Value::Float(-0.0), Value::Float(1.0 + f64::EPSILON)]),
    );
    let record = stored::vector_record(&vector).unwrap();
    let Value::Map(record) = record else {
        panic!("vector")
    };
    assert_eq!(record["vector"], Value::Text("000000000000803f".into()));
    assert_eq!(record["expiry"], Value::Null);
    set(
        &mut vector,
        "vector",
        Value::List(vec![Value::Float(f64::MAX)]),
    );
    assert_eq!(stored::vector_record(&vector), Err("float"));
}

#[test]
fn ingestion_stored_physical_maps_and_native_metadata() {
    use canonical::stored;
    let physical = stored::physical(&source(), &stored_head()).unwrap();
    assert_eq!(physical.len(), 6);
    assert_eq!(physical["g7_v"], Value::Integer(1));
    assert_eq!(physical["g7_kind"], Value::Text("src".into()));
    assert_eq!(physical["status"], Value::Integer(0));
    assert_eq!(physical["last_batch"], Value::Integer(0));
    assert_eq!(physical["d00"], Value::Text("AQAAAAAAAAAAAA==".into()));
    assert_eq!(
        stored::read_fragments(&physical, "i").unwrap(),
        canonical::encode(&Type::Identity, &source()).unwrap()
    );
    stored::validate_physical(&source(), &stored_head(), &physical).unwrap();
    let mut changed = physical.clone();
    changed.insert("d00".into(), Value::Text("AgAAAAAAAAAAAA==".into()));
    assert_eq!(
        stored::validate_physical(&source(), &stored_head(), &changed),
        Err("evidence_conflict")
    );
    let mut present = stored_head();
    set(&mut present, "revision_key", Value::Text("r".into()));
    set(&mut present, "job_receipt_key", Value::Text("job".into()));
    let present = stored::physical(&source(), &present).unwrap();
    assert_eq!(present["revision_key"], Value::Text("r".into()));
    assert!(!present.contains_key("job_receipt_key"));
    let mut metadata = BTreeMap::from([
        ("b".into(), Value::Float(-0.0)),
        ("a".into(), Value::Integer(-2)),
        ("é".into(), Value::Text("x".into())),
    ]);
    assert_eq!(
        canonical::hex(&stored::metadata(&metadata).unwrap()),
        "0300000001006102feffffffffffffff0100620300000000000000000200c3a9010100000078"
    );
    let digest = stored::properties_digest(&metadata).unwrap();
    assert_eq!(
        digest,
        canonical::framed_hash(
            "vlite.g7.v1.properties",
            &stored::metadata(&metadata).unwrap()
        )
        .unwrap()
    );
    for invalid in [
        Value::Null,
        Value::Bool(true),
        Value::Unsigned(1),
        Value::Float(f64::NAN),
        Value::Float(f64::INFINITY),
    ] {
        metadata.insert("a".into(), invalid);
        assert!(stored::metadata(&metadata).is_err());
    }
    for (key, val, accepted) in [
        ("".into(), "".into(), false),
        ("k".repeat(256), "x".repeat(4096), true),
        ("k".repeat(257), "".into(), false),
        ("k".into(), "x".repeat(4097), false),
    ] {
        assert_eq!(
            stored::metadata(&BTreeMap::from([(key, Value::Text(val))])).is_ok(),
            accepted
        );
    }
    let mut fields: BTreeMap<_, _> = (0..64)
        .map(|i| (format!("k{i:02}"), Value::Integer(0)))
        .collect();
    assert!(stored::metadata(&fields).is_ok());
    fields.insert("last".into(), Value::Integer(0));
    assert_eq!(stored::metadata(&fields), Err("properties"));
    // Count 4 + 16 * (U16 name length + 3 name bytes + tag + U32 string length)
    // + 15 * 4096 + 3932 = 65536.
    let mut fields: BTreeMap<_, _> = (0..15)
        .map(|i| (format!("k{i:02}"), Value::Text("v".repeat(4096))))
        .collect();
    fields.insert("k15".into(), Value::Text("v".repeat(3932)));
    assert_eq!(stored::metadata(&fields).unwrap().len(), 65536);
    fields.insert("k15".into(), Value::Text("v".repeat(3933)));
    assert_eq!(stored::metadata(&fields), Err("properties"));
}

#[test]
fn ingestion_stored_base64_fragment_boundaries() {
    use canonical::stored;
    for (raw, encoded) in [
        (b"".as_slice(), ""),
        (b"f", "Zg=="),
        (b"fo", "Zm8="),
        (b"foo", "Zm9v"),
        (&[251, 255, 255], "+///"),
    ] {
        assert_eq!(stored::base64(raw), encoded);
        assert_eq!(stored::unbase64(encoded).unwrap(), raw);
    }
    for bad in [
        "Zg", "Zg==\n", "Zg===", "Zh==", "Zm9=", "====", "=m9v", "Zg==Zg==", "-___",
    ] {
        assert_eq!(stored::unbase64(bad), Err("base64"), "{bad}");
    }
    for length in [0, 1, 3071, 3072, 3073, 6144] {
        let raw = vec![b'x'; length];
        let mut map = BTreeMap::new();
        for (i, part) in raw.chunks(3072).enumerate() {
            map.insert(format!("i{i:02}"), Value::Text(stored::base64(part)));
        }
        if length == 0 {
            map.insert("i00".into(), Value::Text(String::new()));
        }
        assert_eq!(stored::read_fragments(&map, "i").unwrap(), raw);
    }
    for pieces in [
        vec![vec![], vec![1]],
        vec![vec![1], vec![2]],
        vec![vec![1; 3072], vec![]],
        vec![vec![1; 3073]],
    ] {
        let map = pieces
            .iter()
            .enumerate()
            .map(|(i, part)| (format!("i{i:02}"), Value::Text(stored::base64(part))))
            .collect();
        assert_eq!(stored::read_fragments(&map, "i"), Err("fragment_size"));
    }
}

fn shard_header() -> Value {
    map(&[
        ("channel", Value::Unsigned(0)),
        ("root_key", Value::Text("r".repeat(71))),
        ("batch", Value::Unsigned(1)),
        ("group", Value::Unsigned(0)),
    ])
}

fn identity(kind: &str, fields: Value) -> Value {
    map(&[("kind", Value::Text(kind.into())), ("fields", fields)])
}

#[test]
fn ingestion_stored_maximum_mention_and_fragment_writer() {
    use canonical::stored;
    let binding = map(&[
        ("graph", Value::Text("g".repeat(63))),
        ("collection", Value::Text("c".repeat(63))),
        ("tenant", Value::Text("t".repeat(128))),
        ("generation", Value::Text("00".repeat(32))),
    ]);
    let source = identity(
        "source",
        map(&[
            ("binding", binding.clone()),
            ("tenant", Value::Text("t".repeat(128))),
            ("source_id", Value::Text("s".repeat(256))),
        ]),
    );
    let revision = identity(
        "revision",
        map(&[
            ("source", source),
            ("content", Value::Text("00".repeat(32))),
            ("pipeline", Value::Text("00".repeat(32))),
        ]),
    );
    let chunk = identity(
        "chunk",
        map(&[
            ("revision", revision),
            ("ordinal", Value::Unsigned(0)),
            ("start", Value::Unsigned(0)),
            ("end", Value::Unsigned(16384)),
        ]),
    );
    let entity = identity(
        "entity",
        map(&[
            ("binding", binding),
            ("tenant", Value::Text("t".repeat(128))),
            ("kind", Value::Text("K".repeat(63))),
            ("normalized_key", Value::Text("n".repeat(256))),
        ]),
    );
    let mention = identity(
        "mention",
        map(&[
            ("chunk", chunk),
            ("entity", entity),
            ("start", Value::Unsigned(0)),
            ("end", Value::Unsigned(16384)),
        ]),
    );
    let attributes = map(&[
        (
            "a",
            Value::List(vec![Value::Unsigned(5), Value::Text("a".repeat(4096))]),
        ),
        (
            "b",
            Value::List(vec![Value::Unsigned(5), Value::Text("b".repeat(4076))]),
        ),
    ]);
    let mut data = map(&[
        ("source_key", Value::Text("s".repeat(71))),
        ("revision_key", Value::Text("r".repeat(71))),
        ("chunk_key", Value::Text("c".repeat(71))),
        ("surface", Value::Text("s".repeat(16384))),
        ("confidence", Value::Float(1.0)),
        ("extractor_version", Value::Text("v".repeat(256))),
        ("attributes", attributes),
    ]);
    assert_eq!(
        canonical::encode(&Type::Identity, &mention).unwrap().len(),
        1557
    );
    let bytes = canonical::encode(&Type::Record("mention_data"), &data).unwrap();
    assert_eq!(bytes.len(), 25079);
    let properties = stored::physical(&mention, &data).unwrap();
    assert_eq!(properties.len(), 19);
    assert_eq!(stored::metadata(&properties).unwrap().len(), 36258);
    assert!(!properties.contains_key("surface"));
    assert_eq!(stored::read_fragments(&properties, "d").unwrap(), bytes);
    assert_eq!(
        properties
            .iter()
            .filter(|(k, _)| k.starts_with('d'))
            .count(),
        9
    );
    stored::validate_physical(&mention, &data, &properties).unwrap();
    // Data fixed overhead is 8695; a 521-byte surface makes exactly 3 raw fragments.
    set(&mut data, "surface", Value::Text("s".repeat(521)));
    let bytes = canonical::encode(&Type::Record("mention_data"), &data).unwrap();
    assert_eq!(bytes.len(), 9216);
    let properties = stored::physical(&mention, &data).unwrap();
    assert!(properties.contains_key("d02"));
    assert!(!properties.contains_key("d03"));
    assert_eq!(stored::read_fragments(&properties, "d").unwrap(), bytes);
    set(&mut data, "surface", Value::Text("s".repeat(522)));
    let properties = stored::physical(&mention, &data).unwrap();
    assert_eq!(properties["d03"], Value::Text("Yg==".into()));
}

#[test]
fn ingestion_stored_entry_fields_and_order() {
    use canonical::stored;
    let mut vector = template("entry");
    set(&mut vector, "object_kind", Value::Unsigned(2));
    set(&mut vector, "object_key", Value::Text("".into()));
    set(&mut vector, "collection", Value::Unsigned(u16::MAX.into()));
    set(&mut vector, "record", Value::Unsigned(u64::MAX));
    set(
        &mut vector,
        "attached_chunk_key",
        Value::Text("c".repeat(71)),
    );
    let bytes = canonical::encode(&Type::Record("entry"), &vector).unwrap();
    assert_eq!(
        canonical::encode(
            &Type::Record("entry"),
            &canonical::decode(&Type::Record("entry"), &bytes).unwrap()
        )
        .unwrap(),
        bytes
    );
    for key in ["collection", "record", "attached_chunk_key"] {
        let mut bad = vector.clone();
        set(&mut bad, key, Value::Null);
        assert_eq!(
            canonical::encode(&Type::Record("entry"), &bad),
            Err("entry_vector_fields")
        );
    }
    for length in [0, 72] {
        let mut valid = vector.clone();
        set(
            &mut valid,
            "attached_chunk_key",
            Value::Text("c".repeat(length)),
        );
        assert!(canonical::encode(&Type::Record("entry"), &valid).is_ok());
        let mut graph = template("entry");
        set(&mut graph, "object_key", Value::Text("k".repeat(length)));
        assert_eq!(
            canonical::encode(&Type::Record("entry"), &graph).is_ok(),
            length != 0
        );
    }
    for key in ["record", "attached_chunk_key"] {
        let mut graph = template("entry");
        set(
            &mut graph,
            key,
            if key == "record" {
                Value::Unsigned(1)
            } else {
                Value::Text("c".into())
            },
        );
        assert_eq!(
            canonical::encode(&Type::Record("entry"), &graph),
            Err("entry_graph_fields")
        );
    }
    for (key, first, second) in [
        ("producing_op", Value::Unsigned(1), Value::Unsigned(256)),
        ("result_ordinal", Value::Unsigned(1), Value::Unsigned(256)),
        ("action", Value::Unsigned(0), Value::Unsigned(2)),
        ("object_kind", Value::Unsigned(0), Value::Unsigned(1)),
        (
            "object_key",
            Value::Text("a".into()),
            Value::Text("b".into()),
        ),
    ] {
        let mut a = template("entry");
        let mut b = a.clone();
        set(&mut a, key, first);
        set(&mut b, key, second);
        assert!(stored::split_shards(&shard_header(), &[a.clone(), b.clone()]).is_ok());
        assert_eq!(
            stored::split_shards(&shard_header(), &[b, a]),
            Err("entry_order"),
            "{key}"
        );
    }
}

fn shard_entry(ordinal: u64, key_len: usize) -> Value {
    let mut entry = template("entry");
    set(&mut entry, "producing_op", Value::Unsigned(ordinal));
    set(&mut entry, "object_key", Value::Text("k".repeat(key_len)));
    set(
        &mut entry,
        "revision",
        map(&[
            ("tag", Value::Unsigned(1)),
            ("value", Value::Unsigned(u64::MAX)),
        ]),
    );
    entry
}

#[test]
fn ingestion_stored_shard_exact_greedy_packing() {
    use canonical::stored;
    let header = shard_header();
    let mut entries: Vec<_> = (0..27).map(|i| shard_entry(i, 71)).collect();
    entries.push(shard_entry(27, 54));
    assert_eq!(
        canonical::encode(&Type::Record("entry"), &entries[0])
            .unwrap()
            .len()
            - 1,
        107
    );
    let shards = stored::split_shards(&header, &entries).unwrap();
    assert_eq!(shards.len(), 1);
    let encoded = canonical::encode(&Type::Record("shard"), &shards[0]).unwrap();
    assert_eq!(encoded.len(), 3072);
    assert_eq!(encoded[0..2], [1, 0]);
    assert_eq!(&encoded[89..93], &28u32.to_le_bytes());
    let properties = stored::shard_physical(&shards[0]).unwrap();
    assert_eq!(properties.len(), 1);
    assert_eq!(stored::metadata(&properties).unwrap().len(), 4111);
    let Value::Text(base64) = &properties["data"] else {
        panic!("data")
    };
    assert_eq!(base64.len(), 4096);
    assert_eq!(stored::unbase64(base64).unwrap(), encoded);
    assert_eq!(stored::validate_shards(&header, &shards).unwrap(), entries);
    set(&mut entries[27], "object_key", Value::Text("k".repeat(55)));
    let over = stored::split_shards(&header, &entries).unwrap();
    assert_eq!(over.len(), 2);
    assert_eq!(stored::validate_shards(&header, &over).unwrap(), entries);
    let mut bad = shards[0].clone();
    set(&mut bad, "entries", Value::List(entries.clone()));
    assert_eq!(
        canonical::encode(&Type::Record("shard"), &bad),
        Err("shard_capacity")
    );
    let mut duplicate = entries.clone();
    duplicate.push(entries[27].clone());
    assert_eq!(
        stored::split_shards(&header, &duplicate),
        Err("entry_order")
    );
    let mut direct_duplicate = shards[0].clone();
    set(
        &mut direct_duplicate,
        "entries",
        Value::List(vec![entries[0].clone(), entries[0].clone()]),
    );
    assert_eq!(
        canonical::encode(&Type::Record("shard"), &direct_duplicate),
        Err("entry_order")
    );
    let mut reversed = entries.clone();
    reversed.swap(0, 1);
    assert_eq!(stored::split_shards(&header, &reversed), Err("entry_order"));
    assert!(stored::split_shards(&header, &[]).unwrap().is_empty());
    for key in ["channel", "root_key", "batch", "group", "index"] {
        let mut bad = over.clone();
        set(
            &mut bad[0],
            key,
            if key == "root_key" {
                Value::Text("wrong".into())
            } else {
                Value::Unsigned(1)
            },
        );
        if key == "batch" {
            set(&mut bad[0], key, Value::Unsigned(2));
        }
        assert_eq!(
            stored::validate_shards(&header, &bad),
            Err(if key == "index" {
                "shard_index"
            } else {
                "shard_backref"
            })
        );
    }
    let mut nonmax = over.clone();
    set(
        &mut nonmax[0],
        "entries",
        Value::List(entries[..26].to_vec()),
    );
    set(
        &mut nonmax[1],
        "entries",
        Value::List(entries[26..].to_vec()),
    );
    assert_eq!(
        stored::validate_shards(&header, &nonmax),
        Err("shard_packing")
    );
    let mut empty = over[0].clone();
    set(&mut empty, "entries", Value::List(vec![]));
    assert_eq!(
        canonical::encode(&Type::Record("shard"), &empty),
        Err("empty_shard")
    );
    let mut oversized_header = header;
    set(
        &mut oversized_header,
        "root_key",
        Value::Text("r".repeat(3072)),
    );
    assert_eq!(
        stored::split_shards(&oversized_header, &entries[..1]),
        Err("shard_capacity")
    );
    let mut literal = entries[0].clone();
    set(
        &mut literal,
        "id",
        map(&[
            ("tag", Value::Unsigned(0)),
            ("value", Value::Unsigned(u64::MAX)),
        ]),
    );
    assert_eq!(
        canonical::encode(&Type::Record("entry"), &literal)
            .unwrap()
            .len(),
        canonical::encode(&Type::Record("entry"), &entries[0])
            .unwrap()
            .len()
    );
}

#[test]
fn ingestion_canonical_strict_identity_boundaries() {
    let original = canonical::encode(&Type::Identity, &source()).unwrap();
    assert_eq!(original.len(), 59);
    for (offset, replacement, expected) in [
        (0, 2, "version"),
        (1, 0, "identity_tag"),
        (1, 255, "identity_tag"),
    ] {
        let mut bytes = original.clone();
        bytes[offset] = replacement;
        assert_eq!(
            canonical::decode(&Type::Identity, &bytes).unwrap_err(),
            expected
        );
    }
    for length in 0..original.len() {
        assert!(
            canonical::decode(&Type::Identity, &original[..length]).is_err(),
            "prefix {length}"
        );
    }
    let mut extra = original;
    extra.push(0);
    assert_eq!(
        canonical::decode(&Type::Identity, &extra).unwrap_err(),
        "noncanonical"
    );
    let mut invalid = source();
    if let Value::Map(root) = &mut invalid
        && let Value::Map(fields) = root.get_mut("fields").unwrap()
    {
        fields.insert("tenant".into(), Value::Text("different".into()));
    }
    assert_eq!(
        canonical::encode(&Type::Identity, &invalid).unwrap_err(),
        "binding"
    );
    let extra_field = map(&[
        ("kind", Value::Text("source".into())),
        (
            "fields",
            match source() {
                Value::Map(mut root) => root.remove("fields").unwrap(),
                _ => unreachable!(),
            },
        ),
        ("lineage", Value::Unsigned(1)),
    ]);
    assert_eq!(
        canonical::encode(&Type::Identity, &extra_field).unwrap_err(),
        "fields"
    );
}

#[test]
fn ingestion_canonical_attribute_byte_ceiling_and_numeric_tags() {
    let attrs = |length| {
        map(&[
            (
                "k",
                Value::List(vec![Value::Unsigned(5), Value::Text("x".repeat(length))]),
            ),
            (
                "z",
                Value::List(vec![Value::Unsigned(5), Value::Text("y".repeat(4080))]),
            ),
        ])
    };
    // Two S(key) + (tag + S(value)) headers total 20 bytes.
    assert_eq!(
        canonical::encode(&Type::Attributes, &attrs(4092))
            .unwrap()
            .len(),
        8196
    );
    assert_eq!(
        canonical::encode(&Type::Attributes, &attrs(4093)).unwrap_err(),
        "attribute_bytes"
    );
    for (tag, v) in [
        (0, Value::Null),
        (1, Value::Bool(false)),
        (2, Value::Integer(0)),
        (3, Value::Unsigned(0)),
        (4, Value::Float(0.0)),
        (5, Value::Text(String::new())),
    ] {
        let encoded = canonical::encode(
            &Type::Attribute,
            &Value::List(vec![Value::Unsigned(tag), v]),
        )
        .unwrap();
        assert_eq!(u64::from(encoded[0]), tag);
    }
}

#[test]
fn ingestion_stored_review_distinct_field_bytes() {
    let head = map(&[
        ("revision_key", Value::Text("r".into())),
        ("revision_digest", Value::Text("11".repeat(32))),
        ("pipeline", Value::Text("22".repeat(32))),
        ("status", Value::Unsigned(2)),
        ("job_receipt_key", Value::Text("j".into())),
        ("last_batch", Value::Unsigned(0x33445566)),
    ]);
    let revision = map(&[
        ("source_key", Value::Text("S".into())),
        ("uri", Value::Text("U".into())),
        ("text_length", Value::Unsigned(0x11223344)),
        ("chunk_count", Value::Unsigned(0x55667788)),
        ("receipt_key", Value::Text("R".into())),
    ]);
    let chunk = map(&[
        ("source_key", Value::Text("S".into())),
        ("revision_key", Value::Text("R".into())),
        ("ordinal", Value::Unsigned(0x01020304)),
        ("start", Value::Unsigned(0x05060708)),
        ("end", Value::Unsigned(0x090a0b0c)),
        ("chunk_digest", Value::Text("33".repeat(32))),
        ("pipeline", Value::Text("44".repeat(32))),
    ]);
    let batch = map(&[
        (
            "batch_digest",
            map(&[
                ("tag", Value::Unsigned(2)),
                ("value", Value::Text("55".repeat(32))),
            ]),
        ),
        ("final", Value::Bool(true)),
        ("created_anchors", Value::Unsigned(0x01020304)),
        ("reused_anchors", Value::Unsigned(0x05060708)),
        ("result_shards", Value::Unsigned(0x090a0b0c)),
        ("ownership_shards", Value::Unsigned(0x0d0e0f10)),
    ]);
    let mut entry = template("entry");
    for (key, n) in [
        ("producing_op", 0x01020304),
        ("result_ordinal", 0x05060708),
        ("object_kind", 1),
    ] {
        set(&mut entry, key, Value::Unsigned(n));
    }
    set(&mut entry, "object_key", Value::Text("K".into()));
    set(
        &mut entry,
        "id",
        map(&[
            ("tag", Value::Unsigned(0)),
            ("value", Value::Unsigned(0x1112131415161718)),
        ]),
    );
    set(
        &mut entry,
        "revision",
        map(&[
            ("tag", Value::Unsigned(1)),
            ("value", Value::Unsigned(0x2122232425262728)),
        ]),
    );
    set(&mut entry, "retained", Value::Bool(true));
    // Literal witnesses from the appendix field order and fixed LE widths.
    let entry_hex = "0104030201080706050001010000004b00181716151413121101282726252423222100000001";
    let shard = map(&[
        ("channel", Value::Unsigned(1)),
        ("root_key", Value::Text("S".into())),
        ("batch", Value::Unsigned(0x01020304)),
        ("group", Value::Unsigned(0x05060708)),
        ("index", Value::Unsigned(0x090a0b0c)),
        ("entries", Value::List(vec![entry.clone()])),
    ]);
    for (name, data, expected) in [
        (
            "head_data",
            head,
            format!(
                "0101010000007201{}01{}0201010000006a66554433",
                "11".repeat(32),
                "22".repeat(32)
            ),
        ),
        (
            "revision_data",
            revision,
            "01010000005301010000005544332211887766550100000052".into(),
        ),
        (
            "chunk_data",
            chunk,
            format!(
                "010100000053010000005204030201080706050c0b0a09{}{}",
                "33".repeat(32),
                "44".repeat(32)
            ),
        ),
        (
            "batch_data",
            batch,
            format!("0102{}0104030201080706050c0b0a09100f0e0d", "55".repeat(32)),
        ),
        ("entry", entry, entry_hex.into()),
        (
            "shard",
            shard,
            format!(
                "0101010000005304030201080706050c0b0a0901000000{}",
                &entry_hex[2..]
            ),
        ),
    ] {
        let expected = canonical::unhex(&expected).unwrap();
        assert_eq!(
            canonical::encode(&Type::Record(name), &data).unwrap(),
            expected,
            "{name}"
        );
        assert_eq!(
            canonical::decode(&Type::Record(name), &expected).unwrap(),
            data,
            "{name}"
        );
    }
}

#[test]
fn ingestion_stored_review_negative_fields_and_projection_overflow() {
    use canonical::stored;
    let mut entry = template("entry");
    set(&mut entry, "object_kind", Value::Unsigned(2));
    set(&mut entry, "collection", Value::Unsigned(1));
    set(&mut entry, "record", Value::Unsigned(2));
    set(
        &mut entry,
        "attached_chunk_key",
        Value::Text("chunk".into()),
    );
    assert_eq!(
        canonical::encode(&Type::Record("entry"), &entry),
        Err("entry_vector_fields")
    );
    let mention = get(get(&template("mention_edge_operation"), "fields"), "object").clone();
    let mut id = get(&mention, "identity").clone();
    let mut fields = get(&id, "fields").clone();
    set(&mut fields, "end", Value::Unsigned(0));
    set(&mut id, "fields", fields);
    assert_eq!(
        canonical::encode(&Type::Identity, &id),
        Err("evidence_span")
    );
    let id = get(&mention, "identity").clone();
    let mut data = get(&mention, "data").clone();
    set(&mut data, "surface", Value::Text("a".repeat(50_000)));
    assert_eq!(stored::physical(&id, &data), Err("properties"));
    let mut vector = template("vector_projection_data");
    vector_text(&mut vector).unwrap();
    set(&mut vector, "tenant", Value::Text("t".repeat(5_000)));
    assert_eq!(stored::vector_record(&vector), Err("properties"));
}

#[test]
fn ingestion_stored_review_physical_roles() {
    use canonical::stored;
    for (name, token) in [
        ("has_revision_edge_operation", "hr"),
        ("has_chunk_edge_operation", "hc"),
    ] {
        let op = template(name);
        let object = get(get(&op, "fields"), "object");
        let id = get(object, "identity");
        let data = get(object, "data");
        let physical = stored::physical(id, data).unwrap();
        assert_eq!(physical["g7_kind"], Value::Text(token.into()));
        stored::validate_physical(id, data, &physical).unwrap();
    }
    let batch = identity(
        "batch_receipt",
        map(&[
            ("root", template("receipt")),
            ("ordinal", Value::Unsigned(1)),
        ]),
    );
    let physical = stored::physical(&batch, &template("batch_data")).unwrap();
    assert_eq!(physical["g7_kind"], Value::Text("bat".into()));
    stored::validate_physical(&batch, &template("batch_data"), &physical).unwrap();
    for (name, token) in [("delete_receipt", "del"), ("abort_receipt", "abt")] {
        let id = template(name);
        let mut data = template("receipt_data");
        set(&mut data, "revision_key", Value::Text(String::new()));
        let physical = stored::physical(&id, &data).unwrap();
        assert_eq!(physical["g7_kind"], Value::Text(token.into()));
        assert_eq!(physical["revision_key"], Value::Text(String::new()));
        stored::validate_physical(&id, &data, &physical).unwrap();
    }
    for role in ["result_shard", "ownership_shard"] {
        let id = identity(
            role,
            map(&[
                ("root", template("receipt")),
                ("batch", Value::Unsigned(1)),
                ("group", Value::Unsigned(2)),
                ("index", Value::Unsigned(3)),
            ]),
        );
        assert_eq!(stored::physical(&id, &map(&[])), Err("object_role"));
    }
}

#[test]
fn ingestion_stored_strictness_ruling() {
    for (name, key) in [
        ("receipt_data", "plan_digest"),
        ("batch_data", "batch_digest"),
    ] {
        let mut data = template(name);
        set(
            &mut data,
            key,
            map(&[
                ("tag", Value::Unsigned(3)),
                ("value", Value::Text("00".repeat(32))),
            ]),
        );
        assert_eq!(
            canonical::encode(&Type::Record(name), &data),
            Err("reference_tag")
        );
    }
    let mut revision = template("revision_projection_data");
    let mut request = template("request");
    // The request fixture's source bytes use the JSON hex representation.
    let Value::Map(fields) = &mut request else {
        panic!("request")
    };
    if let Value::Text(hex) = &fields["text"] {
        fields.insert("text".into(), Value::Bytes(canonical::unhex(hex).unwrap()));
    }
    for (name, data) in [("revision_data", &mut revision), ("request", &mut request)] {
        for uri in [
            Value::Null,
            Value::Text(String::new()),
            Value::Text("é".repeat(1024)),
        ] {
            set(data, "uri", uri);
            let bytes = canonical::encode(&Type::Record(name), data).unwrap();
            assert_eq!(
                canonical::decode(&Type::Record(name), &bytes).unwrap(),
                *data
            );
        }
        for uri in ["u".repeat(2049), "valid\0uri".into()] {
            set(data, "uri", Value::Text(uri));
            assert_eq!(
                canonical::encode(&Type::Record(name), data),
                Err("uri"),
                "{name}"
            );
        }
    }
    for key in ["revision_key", "job_receipt_key"] {
        let mut head = template("head_data_present");
        set(&mut head, key, Value::Text("valid\0key".into()));
        assert_eq!(
            canonical::encode(&Type::Record("head_data"), &head),
            Err(key)
        );
        set(&mut head, key, Value::Text(String::new()));
        let bytes = canonical::encode(&Type::Record("head_data"), &head).unwrap();
        assert_eq!(
            canonical::decode(&Type::Record("head_data"), &bytes).unwrap(),
            head
        );
        set(&mut head, key, Value::Null);
        assert!(canonical::encode(&Type::Record("head_data"), &head).is_ok());
    }
    let mut vector = template("vector_projection_data");
    vector_text(&mut vector).unwrap();
    set(&mut vector, "text", Value::Bytes(b"a\0b".to_vec()));
    assert!(canonical::stored::vector_record(&vector).is_ok());
    for key in ["tenant", "source_key", "revision_key", "chunk_key"] {
        let mut invalid = vector.clone();
        set(&mut invalid, key, Value::Text("a\0b".into()));
        assert_eq!(
            canonical::encode(&Type::Record("vector_data"), &invalid),
            Err("vector_metadata")
        );
        set(&mut invalid, key, Value::Text(String::new()));
        let bytes = canonical::encode(&Type::Record("vector_data"), &invalid).unwrap();
        assert_eq!(
            canonical::decode(&Type::Record("vector_data"), &bytes).unwrap(),
            invalid
        );
    }
    set(&mut vector, "vector", Value::List(vec![]));
    assert_eq!(
        canonical::encode(&Type::Record("vector_data"), &vector),
        Err("vector_dimensions")
    );
    set(
        &mut vector,
        "vector",
        Value::List(vec![Value::Text("1".into())]),
    );
    assert_eq!(
        canonical::encode(&Type::Record("vector_data"), &vector),
        Err("float")
    );
}

#[test]
fn ingestion_canonical_primitives_fail_closed() {
    for (t, value, error) in [
        (Type::U8, Value::Unsigned(256), "integer_range"),
        (Type::U16, Value::Unsigned(65536), "integer_range"),
        (Type::I64, Value::Unsigned(u64::MAX), "integer_range"),
        (Type::F32, Value::Float(f64::MAX), "float"),
        (Type::F64, Value::Float(f64::NAN), "float"),
        (Type::F32, Value::Float(f64::INFINITY), "float"),
        (Type::Bool, Value::Unsigned(1), "boolean"),
        (
            Type::Attribute,
            Value::List(vec![Value::Unsigned(6), Value::Null]),
            "attribute_tag",
        ),
        (
            Type::Attribute,
            Value::List(vec![Value::Unsigned(0), Value::Bool(false)]),
            "attribute_tag",
        ),
    ] {
        assert_eq!(canonical::encode(&t, &value).unwrap_err(), error);
    }
    assert_eq!(canonical::unhex("0").unwrap_err(), "hex");
    assert_eq!(canonical::unhex("AB").unwrap_err(), "hex");
    for t in [Type::IdRef, Type::RevisionRef, Type::Slot] {
        let invalid = map(&[("tag", Value::Unsigned(3)), ("value", Value::Null)]);
        assert_eq!(
            canonical::encode(&t, &invalid).unwrap_err(),
            "reference_tag"
        );
    }
    let list_type = Type::List(Box::new(Type::U16));
    let list = Value::List(vec![Value::Unsigned(1), Value::Unsigned(258)]);
    let bytes = canonical::encode(&list_type, &list).unwrap();
    assert_eq!(canonical::hex(&bytes), "0200000001000201");
    assert_eq!(canonical::decode(&list_type, &bytes).unwrap(), list);
    let optional = Type::Optional(Box::new(Type::Text));
    assert_eq!(canonical::encode(&optional, &Value::Null).unwrap(), [0]);
    assert_eq!(
        canonical::hex(&canonical::encode(&optional, &Value::Text(String::new())).unwrap()),
        "0100000000"
    );
    assert_eq!(canonical::decode(&optional, &[0]).unwrap(), Value::Null);
    assert_eq!(
        canonical::decode(&optional, &[1, 0, 0, 0, 0]).unwrap(),
        Value::Text(String::new())
    );
}

fn change_identity(mut identity: Value, name: &str, value: Value) -> Value {
    let Value::Map(root) = &mut identity else {
        panic!("identity")
    };
    let Value::Map(fields) = root.get_mut("fields").unwrap() else {
        panic!("fields")
    };
    fields.insert(name.into(), value);
    identity
}

#[test]
fn ingestion_canonical_identity_roles_and_field_bounds() {
    let fixture = fixture();
    let template = |name| value(&expand(&fixture["templates"][name], &fixture["templates"]));
    for count in [1, 256] {
        assert!(
            canonical::encode(
                &Type::Identity,
                &change_identity(source(), "source_id", Value::Text("x".repeat(count)))
            )
            .is_ok()
        );
    }
    for bad in ["", "x\0"] {
        assert_eq!(
            canonical::encode(
                &Type::Identity,
                &change_identity(source(), "source_id", Value::Text(bad.into()))
            )
            .unwrap_err(),
            "source_id"
        );
    }
    for bad in ["A", " a", "a ", "a  b", "a\tb", "a\rb", "a\nb", ""] {
        assert_eq!(
            canonical::encode(
                &Type::Identity,
                &change_identity(
                    template("entity"),
                    "normalized_key",
                    Value::Text(bad.into())
                )
            )
            .unwrap_err(),
            "entity_identity"
        );
    }
    for count in [1, 256] {
        assert!(
            canonical::encode(
                &Type::Identity,
                &change_identity(
                    template("entity"),
                    "normalized_key",
                    Value::Text("a".repeat(count))
                )
            )
            .is_ok()
        );
    }
    assert_eq!(
        canonical::encode(
            &Type::Identity,
            &change_identity(
                template("entity"),
                "normalized_key",
                Value::Text("a".repeat(257))
            )
        )
        .unwrap_err(),
        "entity_identity"
    );
    assert_eq!(
        canonical::encode(
            &Type::Identity,
            &change_identity(template("revision"), "source", template("entity"))
        )
        .unwrap_err(),
        "identity_role"
    );
    let mut extra = source();
    let Value::Map(root) = &mut extra else {
        panic!("source")
    };
    let Value::Map(fields) = root.get_mut("fields").unwrap() else {
        panic!("fields")
    };
    fields.insert("lineage".into(), Value::Unsigned(1));
    assert_eq!(
        canonical::encode(&Type::Identity, &extra).unwrap_err(),
        "fields"
    );
    let mut binding = template("binding");
    let Value::Map(values) = &mut binding else {
        panic!("binding")
    };
    values.insert("graph".into(), Value::Text("g".repeat(64)));
    assert_eq!(
        canonical::encode(&Type::Binding, &binding).unwrap_err(),
        "binding"
    );
}

#[test]
fn ingestion_canonical_attribute_fields_and_string_bounds() {
    let attr = |s: String| Value::List(vec![Value::Unsigned(5), Value::Text(s)]);
    for length in [0, 4096] {
        assert!(canonical::encode(&Type::Attribute, &attr("s".repeat(length))).is_ok());
    }

    assert_eq!(
        canonical::encode(&Type::Attribute, &attr("s".repeat(4097))).unwrap_err(),
        "attribute_string_limit"
    );
    let entries = |count| {
        Value::Map(
            (0..count)
                .map(|i| {
                    (
                        format!("k{i}"),
                        Value::List(vec![Value::Unsigned(0), Value::Null]),
                    )
                })
                .collect(),
        )
    };
    assert!(canonical::encode(&Type::Attributes, &entries(16)).is_ok());
    assert_eq!(
        canonical::encode(&Type::Attributes, &entries(17)).unwrap_err(),
        "attribute_fields"
    );
    assert_eq!(
        canonical::encode(&Type::Attributes, &map(&[("", attr(String::new()))])).unwrap_err(),
        "attribute_key"
    );
    assert_eq!(
        canonical::encode(
            &Type::Attributes,
            &map(&[(&"k".repeat(4097), attr(String::new()))])
        )
        .unwrap_err(),
        "attribute_string"
    );
}

#[test]
fn ingestion_foundation_review_decode_semantics() {
    let encode_identity = |v: &Value| canonical::encode(&Type::Identity, v).unwrap();
    let mut chunk = encode_identity(&template("chunk"));
    let len = chunk.len();
    chunk[len - 4..].copy_from_slice(&0u32.to_le_bytes());
    assert_eq!(
        canonical::decode(&Type::Identity, &chunk),
        Err("chunk_span")
    );
    let mut entity = encode_identity(&template("entity"));
    *entity.last_mut().unwrap() = b'N';
    assert_eq!(
        canonical::decode(&Type::Identity, &entity),
        Err("entity_identity")
    );
    let mut wrong_role = vec![1, 2];
    let entity = encode_identity(&template("entity"));
    wrong_role.extend_from_slice(&(entity.len() as u32).to_le_bytes());
    wrong_role.extend_from_slice(&entity);
    wrong_role.extend_from_slice(&[0; 64]);
    assert_eq!(
        canonical::decode(&Type::Identity, &wrong_role),
        Err("identity_role")
    );
    let outer = encode_identity(&template("abort_receipt"));
    let prefix = outer.len() - encode_identity(&template("receipt")).len() - 4;
    let mut recursive = outer[..prefix].to_vec();
    recursive.extend_from_slice(&(outer.len() as u32).to_le_bytes());
    recursive.extend_from_slice(&outer);
    assert_eq!(
        canonical::decode(&Type::Identity, &recursive),
        Err("abort_of_abort")
    );
    let original = encode_identity(&source());
    let mut mismatch = original.clone();
    mismatch[53] = b'u';
    assert_eq!(
        canonical::decode(&Type::Identity, &mismatch),
        Err("binding")
    );
    let mut trailing = original.clone();
    trailing.push(0);
    assert_eq!(
        canonical::decode(&Type::Identity, &trailing),
        Err("noncanonical")
    );
    // No trailing bytes: the reader succeeds, and canonical revalidation rejects the tenant mismatch.
    assert_eq!(mismatch.len(), original.len());
    let mut minus_zero = 0f64.to_le_bytes();
    minus_zero[7] = 128;
    assert_eq!(canonical::decode(&Type::F64, &minus_zero), Err("float"));
    assert_eq!(
        canonical::decode(
            &Type::Map(Box::new(Type::U8)),
            &[2, 0, 0, 0, 1, 0, 0, 0, b'b', 0, 1, 0, 0, 0, b'a', 0]
        ),
        Err("map_order")
    );
}

#[test]
fn ingestion_foundation_review_boundaries_and_refs() {
    let null = Value::List(vec![Value::Unsigned(0), Value::Null]);
    let attrs = map(&[(&"k".repeat(4096), null)]);
    let encoded = canonical::encode(&Type::Attributes, &attrs).unwrap();
    assert_eq!(encoded.len(), 4105);
    assert_eq!(
        canonical::decode(&Type::Attributes, &encoded).unwrap(),
        attrs
    );
    for (length, expected) in [(0, "attribute_key"), (4097, "attribute_string")] {
        let inner = 4 + length + 1;
        let mut bytes = (inner as u32).to_le_bytes().to_vec();
        bytes.extend_from_slice(&(length as u32).to_le_bytes());
        bytes.extend_from_slice(&vec![b'k'; length]);
        bytes.push(0);
        assert_eq!(canonical::decode(&Type::Attributes, &bytes), Err(expected));
    }
    let mut binding = template("binding");
    set(&mut binding, "graph", Value::Text("g".repeat(63)));
    let bytes = canonical::encode(&Type::Binding, &binding).unwrap();
    assert_eq!(canonical::decode(&Type::Binding, &bytes).unwrap(), binding);
    for key in ["graph", "tenant"] {
        let mut bad = template("binding");
        set(&mut bad, key, Value::Text("x\0".into()));
        assert_eq!(canonical::encode(&Type::Binding, &bad), Err("binding"));
    }
    assert_eq!(
        canonical::encode(
            &Type::Identity,
            &change_identity(source(), "tenant", Value::Text("t\0".into()))
        ),
        Err("binding")
    );
    assert_eq!(
        canonical::encode(
            &Type::IdRef,
            &map(&[("tag", Value::Unsigned(2)), ("value", Value::Unsigned(1))])
        ),
        Err("reference_tag")
    );
    let slot = map(&[
        ("tag", Value::Unsigned(2)),
        ("value", Value::Text("11".repeat(32))),
    ]);
    let mut expected = vec![2];
    expected.extend_from_slice(&[17; 32]);
    assert_eq!(canonical::encode(&Type::Slot, &slot).unwrap(), expected);
    assert_eq!(canonical::decode(&Type::Slot, &expected).unwrap(), slot);
    assert_eq!(
        canonical::encode(
            &Type::RevisionRef,
            &map(&[("tag", Value::Unsigned(0)), ("value", Value::Unsigned(1))])
        ),
        Err("reference_tag")
    );
    let mut relation = template("relation");
    for predicate in ["", "p\0", "p-"] {
        relation = change_identity(relation, "predicate", Value::Text(predicate.into()));
        assert_eq!(
            canonical::encode(&Type::Identity, &relation),
            Err("relation_identity")
        );
    }
    let relation = change_identity(template("relation"), "end", Value::Unsigned(0));
    assert_eq!(
        canonical::encode(&Type::Identity, &relation),
        Err("evidence_span")
    );
}

#[test]
fn ingestion_foundation_review_semantic_order() {
    use std::cmp::Ordering;
    let order = |a: &Value, b: &Value| {
        assert_eq!(canonical::compare_identities(a, b).unwrap(), Ordering::Less);
        assert_eq!(
            canonical::compare_identities(b, a).unwrap(),
            Ordering::Greater
        );
        assert_eq!(
            canonical::compare_identities(a, a).unwrap(),
            Ordering::Equal
        );
    };
    order(&source(), &template("revision"));
    order(
        &change_identity(source(), "source_id", Value::Text("z".into())),
        &change_identity(source(), "source_id", Value::Text("é".into())),
    );
    let mut binding = template("binding");
    set(&mut binding, "graph", Value::Text("h".into()));
    order(&source(), &change_identity(source(), "binding", binding));
    let mut binding = template("binding");
    set(&mut binding, "generation", Value::Text("01".repeat(32)));
    order(&source(), &change_identity(source(), "binding", binding));
    order(
        &template("revision"),
        &change_identity(
            template("revision"),
            "content",
            Value::Text("01".repeat(32)),
        ),
    );
    let attr = |n| {
        map(&[(
            "a",
            Value::List(vec![Value::Unsigned(3), Value::Unsigned(n)]),
        )])
    };
    // Attribute order uses canonical bytes, not numeric semantic order: U64 256 precedes U64 1.
    order(
        &change_identity(template("relation"), "attributes", attr(256)),
        &change_identity(template("relation"), "attributes", attr(1)),
    );
}

#[test]
fn ingestion_foundation_review_domain_registry() {
    assert_eq!(
        canonical::framed_hash("vlite.g7.v1.not-registered", b"x"),
        Err("hash_domain")
    );
    assert_eq!(
        canonical::framed_hash("vlite.g7.v1.source\0", b"x"),
        Err("hash_domain")
    );
    for domain in [
        "source",
        "revision",
        "chunk",
        "entity",
        "has-revision",
        "has-chunk",
        "mention",
        "relation",
        "receipt",
        "batch-receipt",
        "result-shard",
        "ownership-shard",
        "delete-receipt",
        "abort-receipt",
        "pipeline",
        "request",
        "plan",
        "batch",
        "delete-action",
        "abort-action",
        "policy",
        "properties",
    ] {
        assert!(canonical::framed_hash(&format!("vlite.g7.v1.{domain}"), b"x").is_ok());
    }
}
