use super::{
    Reader, Result, Type, Value, Writer, bounded_text, encode, exact, field, identifier,
    normalized, put, read, role, sequence, text, unsigned,
};
use std::collections::{BTreeMap, BTreeSet};

mod structure;

pub(crate) fn schema(name: &str) -> Result<Vec<(&'static str, Type)>> {
    use Type::*;
    let list = |t| List(Box::new(t));
    Ok(match name {
        "policy" => vec![
            ("source_bytes", U32),
            ("chunks", U32),
            ("entities", U32),
            ("relations", U32),
            ("mentions", U32),
            ("chunk_entities", U32),
            ("chunk_relations", U32),
            ("decoded_bytes", U32),
            ("attribute_bytes", U32),
            ("attribute_fields", U32),
            ("attribute_string_bytes", U32),
            ("frames", U32),
            ("operations", U32),
            ("prepared_bytes", U64),
        ],
        "entity_rule" => vec![
            ("rule_id", Text),
            ("literal", Text),
            ("kind", Text),
            ("key", Text),
        ],
        "relation_rule" => vec![
            ("rule_id", Text),
            ("predicate", Text),
            ("subject_kind", Text),
            ("object_kind", Text),
        ],
        "pipeline" => vec![
            ("chunker", U8),
            ("chunk_bytes", U32),
            ("overlap", U32),
            ("extractor_alias", Text),
            ("extractor_version", Text),
            ("entity_rules", list(Record("entity_rule"))),
            ("relation_rules", list(Record("relation_rule"))),
            ("normalization", Text),
            ("embedder_alias", Text),
            ("model_version", Text),
            ("dimensions", U32),
            ("binding_generation", Digest),
            ("schema_version", U32),
            ("policy", Record("policy")),
        ],
        "request" => vec![
            ("source", Identity),
            ("content_digest", Digest),
            ("uri", Optional(Box::new(Text))),
            ("pipeline", Record("pipeline")),
            ("expected_source", RevisionRef),
            ("mode", U8),
            ("text", Bytes),
        ],
        "entity_observation" => vec![
            ("local_id", U32),
            ("kind", Text),
            ("key", Text),
            ("surface", Text),
            ("start", U32),
            ("end", U32),
            ("attributes", Attributes),
            ("confidence", Optional(Box::new(F64))),
        ],
        "relation_observation" => vec![
            ("subject_local_id", U32),
            ("predicate", Text),
            ("object_local_id", U32),
            ("start", U32),
            ("end", U32),
            ("attributes", Attributes),
            ("confidence", Optional(Box::new(F64))),
        ],
        "accepted_chunk" => vec![
            ("chunk", Identity),
            ("text", Bytes),
            ("entities", list(Record("entity_observation"))),
            ("relations", list(Record("relation_observation"))),
            ("vector", list(F32)),
        ],
        "captured" => vec![
            ("object", Identity),
            ("expected", RevisionRef),
            ("properties_digest", Digest),
        ],
        "group" => vec![("ordinal", U32), ("operations", list(Operation))],
        "batch" => vec![
            ("root", Identity),
            ("ordinal", U32),
            ("groups", list(Record("group"))),
        ],
        "plan" => vec![
            ("request_digest", Digest),
            ("captured", list(Record("captured"))),
            ("accepted", list(Record("accepted_chunk"))),
            ("batches", list(Record("batch"))),
        ],
        "delete_action" | "abort_action" => {
            vec![("root", Identity), ("policy_digest", Digest), ("mode", U8)]
        }
        _ => return Err("record"),
    })
}

pub(super) fn put_nested(w: &mut Writer, kind: &Type, v: &Value, depth: usize) -> Result<()> {
    match kind {
        Type::Identity | Type::Record(_) => {
            let mut inner = Writer(Vec::new());
            put(&mut inner, kind, v, depth + 1)?;
            w.bytes(&inner.0)
        }
        Type::List(inner) => {
            w.count(sequence(v)?.len())?;
            for value in sequence(v)? {
                put_nested(w, inner, value, depth + 1)?;
            }
            Ok(())
        }
        _ => put(w, kind, v, depth + 1),
    }
}

pub(super) fn read_nested(r: &mut Reader<'_>, kind: &Type, depth: usize) -> Result<Value> {
    if depth > super::MAX_DEPTH {
        return Err("depth");
    }
    match kind {
        Type::Identity | Type::Record(_) => {
            let mut nested = Reader { bytes: r.blob()? };
            let value = read(&mut nested, kind, depth + 1)?;
            if !nested.bytes.is_empty() {
                return Err("noncanonical");
            }
            Ok(value)
        }
        Type::List(inner) => {
            let count = r.count()?;
            if count > r.bytes.len() {
                return Err("truncated");
            }
            let mut values = Vec::new();
            for _ in 0..count {
                values.push(read_nested(r, inner, depth + 1)?);
            }
            Ok(Value::List(values))
        }
        _ => read(r, kind, depth + 1),
    }
}

fn number(v: &Value, key: &str) -> Result<u64> {
    unsigned(field(v, key)?, u32::MAX.into())
}
fn tag(v: &Value) -> Result<u64> {
    unsigned(field(v, "tag")?, 13).map_err(|_| "operation_tag")
}
fn of(v: &Value) -> Result<&Value> {
    field(v, "fields")
}
fn byte_text(v: &Value) -> Result<&[u8]> {
    let Value::Bytes(bytes) = field(v, "text")? else {
        return Err("bytes");
    };
    std::str::from_utf8(bytes).map_err(|_| "utf8")?;
    Ok(bytes)
}

pub(super) fn validate(name: &str, v: &Value) -> Result<()> {
    match name {
        "policy" => {
            for (key, ceiling) in [
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
                if unsigned(field(v, key)?, ceiling).is_err() {
                    return Err("policy");
                }
            }
        }
        "entity_rule" | "relation_rule" => {
            bounded_text(field(v, "rule_id")?, 1, super::MAX_BYTES, "rule_id")?;
            if name == "entity_rule" {
                bounded_text(field(v, "literal")?, 1, super::MAX_BYTES, "entity_rule")?;
                if !identifier(field(v, "kind")?) {
                    return Err("entity_rule_identifier");
                }
                bounded_text(field(v, "key")?, 1, super::MAX_BYTES, "entity_rule")?;
            } else {
                for key in ["predicate", "subject_kind", "object_kind"] {
                    if !identifier(field(v, key)?) {
                        return Err("relation_rule_identifier");
                    }
                }
            }
        }
        "pipeline" => {
            if number(v, "chunker")? > 1 {
                return Err("enum");
            }
            if number(v, "schema_version")? != 1 || number(v, "dimensions")? == 0 {
                return Err("pipeline_version");
            }
            if text(field(v, "normalization")?)? != "ascii-key-v1" {
                return Err("normalization");
            }
            for (key, version, error) in [
                ("extractor_alias", false, "pipeline_label"),
                ("embedder_alias", false, "pipeline_label"),
                ("model_version", true, "pipeline_label"),
                ("extractor_version", true, "extractor_version"),
            ] {
                super::stored::label(field(v, key)?, version, error)?;
            }
            let bytes = number(v, "chunk_bytes")?;
            if !(1024..=16384).contains(&bytes) || number(v, "overlap")? > 1024.min(bytes / 4) {
                return Err("chunk_config");
            }
            for key in ["entity_rules", "relation_rules"] {
                for pair in sequence(field(v, key)?)?.windows(2) {
                    if text(field(&pair[0], "rule_id")?)?.as_bytes()
                        >= text(field(&pair[1], "rule_id")?)?.as_bytes()
                    {
                        return Err("rule_order");
                    }
                }
            }
        }
        "request" => {
            if number(v, "mode")? > 1 {
                return Err("enum");
            }
            if role(field(v, "source")?)? != "source" {
                return Err("identity_role");
            }
            if number(field(v, "expected_source")?, "tag")? > 1 {
                return Err("request_revision_ref");
            }
            if *field(v, "uri")? != Value::Null {
                bounded_text(field(v, "uri")?, 0, 2048, "uri")?;
            }
            let bytes = byte_text(v)?;
            use sha2::{Digest, Sha256};
            if text(field(v, "content_digest")?)? != super::hex(&Sha256::digest(bytes)) {
                return Err("request_content_digest");
            }
            let source = of(field(v, "source")?)?;
            if field(field(source, "binding")?, "generation")?
                != field(field(v, "pipeline")?, "binding_generation")?
            {
                return Err("binding_generation");
            }
        }
        "entity_observation" | "relation_observation" => {
            if name == "entity_observation" {
                if !identifier(field(v, "kind")?) {
                    return Err("entity_kind");
                }
                if !normalized(field(v, "key")?) {
                    return Err("normalized_key");
                }
            } else {
                if !identifier(field(v, "predicate")?) {
                    return Err("predicate");
                }
            }
            if number(v, "start")? >= number(v, "end")? {
                return Err("evidence_span");
            }
            let c = field(v, "confidence")?;
            if *c != Value::Null
                && !matches!(c,Value::Float(f) if f.is_finite() && (0.0..=1.0).contains(f))
            {
                return Err("confidence");
            }
        }
        "accepted_chunk" => {
            byte_text(v)?;
            if role(field(v, "chunk")?)? != "chunk" {
                return Err("identity_role");
            }
        }
        "delete_action" | "abort_action" => {
            if number(v, "mode")? > 1 {
                return Err("enum");
            }
            let expected = if name == "delete_action" {
                "delete_receipt"
            } else {
                "abort_receipt"
            };
            if role(field(v, "root")?)? != expected {
                return Err("identity_role");
            }
        }
        "plan" => structure::validate(v)?,
        "captured" | "group" | "batch" => {}
        _ => return Err("record"),
    }
    Ok(())
}

pub(super) fn operation_schema(tag: u64) -> Result<Vec<(&'static str, Type)>> {
    use Type::*;
    Ok(match tag {
        0 => vec![("source", Identity), ("expected", RevisionRef)],
        1 => vec![("edge", Identity), ("expected", RevisionRef)],
        2 => vec![
            ("chunk", Identity),
            ("collection", U16),
            ("record", U64),
            ("expected", RevisionRef),
        ],
        3 => vec![("collection", U16), ("record", U64)],
        4 => vec![("node", Identity), ("expected", RevisionRef)],
        5 | 8 | 10 | 12 => vec![("object", Object)],
        6 => vec![
            ("collection", U16),
            ("record", U64),
            ("data", Record("vector_data")),
        ],
        7 => vec![("chunk", Identity), ("collection", U16), ("record", U64)],
        9 => vec![("shard", Record("shard"))],
        11 => vec![
            ("source", Identity),
            ("expected", RevisionRef),
            ("data", Record("head_data")),
        ],
        13 => vec![("source", Identity), ("data", Record("head_data"))],
        _ => return Err("operation_tag"),
    })
}

fn validate_operation(v: &Value) -> Result<()> {
    let t = tag(v)?;
    let f = of(v)?;
    if t == 1
        && !matches!(
            role(field(f, "edge")?)?,
            "has_revision" | "has_chunk" | "mention" | "relation"
        )
    {
        return Err("operation_object_role");
    }
    if t == 4 && !matches!(role(field(f, "node")?)?, "source" | "revision" | "chunk") {
        return Err("operation_object_role");
    }
    if matches!(t, 5 | 8 | 10 | 12) {
        let r = role(field(field(f, "object")?, "identity")?)?;
        let valid = match t {
            5 => matches!(r, "revision" | "entity" | "chunk"),
            8 => matches!(r, "has_revision" | "has_chunk" | "mention" | "relation"),
            10 => r == "batch_receipt",
            12 => matches!(r, "receipt" | "delete_receipt" | "abort_receipt"),
            _ => false,
        };
        if !valid {
            return Err("operation_object_role");
        }
    }
    if t == 11 && number(field(f, "expected")?, "tag")? == 0 {
        return Err("head_patch_expected");
    }
    if matches!(t, 0 | 11 | 13) && role(field(f, "source")?)? != "source" {
        return Err("operation_object_role");
    }
    if matches!(t, 2 | 7) && role(field(f, "chunk")?)? != "chunk" {
        return Err("operation_object_role");
    }
    Ok(())
}

pub(super) fn put_object(w: &mut Writer, v: &Value, depth: usize) -> Result<()> {
    exact(v, &["identity", "data"])?;
    let identity = field(v, "identity")?;
    put_nested(w, &Type::Identity, identity, depth + 1)?;
    put_nested(
        w,
        &Type::Record(super::stored::data_role(identity)?),
        field(v, "data")?,
        depth + 1,
    )
}

pub(super) fn read_object(r: &mut Reader<'_>, depth: usize) -> Result<Value> {
    let identity = read_nested(r, &Type::Identity, depth + 1)?;
    let data = read_nested(
        r,
        &Type::Record(super::stored::data_role(&identity)?),
        depth + 1,
    )?;
    Ok(Value::Map(BTreeMap::from([
        ("identity".into(), identity),
        ("data".into(), data),
    ])))
}

pub(super) fn put_operation(w: &mut Writer, v: &Value, depth: usize) -> Result<()> {
    exact(v, &["tag", "fields"])?;
    let t = tag(v)?;
    let schema = operation_schema(t)?;
    exact(of(v)?, &schema.iter().map(|(k, _)| *k).collect::<Vec<_>>())?;
    validate_operation(v)?;
    w.raw(&[t as u8])?;
    for (key, kind) in schema {
        put_nested(w, &kind, field(of(v)?, key)?, depth + 1)?;
    }
    Ok(())
}

pub(super) fn read_operation(r: &mut Reader<'_>, depth: usize) -> Result<Value> {
    let t = r.byte()?;
    let mut map = BTreeMap::new();
    for (key, kind) in operation_schema(t.into())? {
        map.insert(key.into(), read_nested(r, &kind, depth + 1)?);
    }
    let v = Value::Map(BTreeMap::from([
        ("tag".into(), Value::Unsigned(t.into())),
        ("fields".into(), Value::Map(map)),
    ]));
    validate_operation(&v)?;
    Ok(v)
}

pub(crate) fn frame_length(body: u64, encrypted: bool) -> Result<u32> {
    if body > 16777216 {
        return Err("body_limit");
    }
    u32::try_from(
        body.checked_add(if encrypted { 80 } else { 52 })
            .ok_or("body_limit")?,
    )
    .map_err(|_| "body_limit")
}

fn mask(v: &mut Value) {
    match v {
        Value::Map(map) => {
            for (key, value) in map {
                if matches!(key.as_str(), "plan_digest" | "batch_digest")
                    && let Value::Map(slot) = value
                    && matches!(
                        slot.get("tag"),
                        Some(Value::Unsigned(1 | 2) | Value::Integer(1 | 2))
                    )
                {
                    slot.insert("value".into(), Value::Text("00".repeat(32)));
                } else {
                    mask(value);
                }
            }
        }
        Value::List(items) => {
            for item in items {
                mask(item);
            }
        }
        _ => {}
    }
}

pub(crate) fn digest(name: &'static str, v: &Value) -> Result<[u8; 32]> {
    encode(&Type::Record(name), v)?;
    let mut masked = v.clone();
    if matches!(name, "batch" | "plan") {
        mask(&mut masked);
    }
    let domain = match name {
        "pipeline" => "vlite.g7.v1.pipeline",
        "request" => "vlite.g7.v1.request",
        "policy" => "vlite.g7.v1.policy",
        "batch" => "vlite.g7.v1.batch",
        "plan" => "vlite.g7.v1.plan",
        "delete_action" => "vlite.g7.v1.delete-action",
        "abort_action" => "vlite.g7.v1.abort-action",
        _ => return Err("digest_domain"),
    };
    super::framed_hash(domain, &encode(&Type::Record(name), &masked)?)
}

pub(crate) fn validate_slots(plan: &Value) -> Result<()> {
    let plan_digest = super::hex(&digest("plan", plan)?);
    for batch in sequence(field(plan, "batches")?)? {
        let batch_digest = super::hex(&digest("batch", batch)?);
        for group in sequence(field(batch, "groups")?)? {
            for op in sequence(field(group, "operations")?)? {
                let t = tag(op)?;
                if matches!(t, 10 | 12) {
                    let data = field(field(of(op)?, "object")?, "data")?;
                    let (key, expected) = if t == 10 {
                        ("batch_digest", &batch_digest)
                    } else {
                        ("plan_digest", &plan_digest)
                    };
                    if text(field(field(data, key)?, "value")?)? != expected {
                        return Err("digest_slot");
                    }
                }
            }
        }
    }
    Ok(())
}
