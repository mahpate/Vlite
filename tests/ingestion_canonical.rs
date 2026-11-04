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
    } else {
        kind(&case["type"])
    };
    let v = if matches!(t, Type::Bytes) {
        Value::Bytes(canonical::unhex(expanded.as_str().unwrap())?)
    } else {
        value(&expanded)
    };
    canonical::encode(&t, &v)
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
        ("fields", Value::Map(BTreeMap::new())),
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
