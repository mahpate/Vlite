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
    for key in ["i01", "i000", "i0a"] {
        assert_eq!(
            stored::read_fragments(&BTreeMap::from([(key.into(), Value::Text("".into()))]), "i"),
            Err("fragment_order")
        );
    }
    assert_eq!(
        stored::read_fragments(&BTreeMap::new(), "i"),
        Err("fragment_order")
    );
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
        let mut bad = vector.clone();
        set(
            &mut bad,
            "attached_chunk_key",
            Value::Text("c".repeat(length)),
        );
        assert_eq!(
            canonical::encode(&Type::Record("entry"), &bad),
            Err("entry_vector_fields")
        );
        let mut graph = template("entry");
        set(&mut graph, "object_key", Value::Text("k".repeat(length)));
        assert_eq!(
            canonical::encode(&Type::Record("entry"), &graph),
            Err("entry_graph_fields")
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
