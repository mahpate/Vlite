use super::{
    Reader, Result, Type, Value, Writer, encode, exact, field, fields, framed_hash, hex,
    identity_key, put, role, sequence, text, unsigned,
};
use std::collections::BTreeMap;

pub(crate) fn schema(name: &str) -> Result<Vec<(&'static str, Type)>> {
    use Type::*;
    let optional = |t| Optional(Box::new(t));
    Ok(match name {
        "head_data" => vec![
            ("revision_key", optional(Text)),
            ("revision_digest", optional(Digest)),
            ("pipeline", optional(Digest)),
            ("status", U8),
            ("job_receipt_key", optional(Text)),
            ("last_batch", U32),
        ],
        "revision_data" => vec![
            ("source_key", Text),
            ("uri", optional(Text)),
            ("text_length", U32),
            ("chunk_count", U32),
            ("receipt_key", Text),
        ],
        "chunk_data" => vec![
            ("source_key", Text),
            ("revision_key", Text),
            ("ordinal", U32),
            ("start", U32),
            ("end", U32),
            ("chunk_digest", Digest),
            ("pipeline", Digest),
        ],
        "entity_data" | "link_data" => vec![],
        "mention_data" => vec![
            ("source_key", Text),
            ("revision_key", Text),
            ("chunk_key", Text),
            ("surface", Text),
            ("confidence", optional(F64)),
            ("extractor_version", Text),
            ("attributes", Attributes),
        ],
        "relation_data" => vec![
            ("source_key", Text),
            ("revision_key", Text),
            ("chunk_key", Text),
            ("confidence", optional(F64)),
            ("extractor_version", Text),
        ],
        "receipt_data" => vec![
            ("request_digest", Digest),
            ("plan_digest", Slot),
            ("batch_count", U32),
            ("revision_key", Text),
            ("outcome", U8),
            ("chunks", U32),
            ("entities", U32),
            ("relations", U32),
            ("mentions", U32),
        ],
        "batch_data" => vec![
            ("batch_digest", Slot),
            ("final", Bool),
            ("created_anchors", U32),
            ("reused_anchors", U32),
            ("result_shards", U32),
            ("ownership_shards", U32),
        ],
        "vector_data" => vec![
            ("tenant", Text),
            ("source_key", Text),
            ("revision_key", Text),
            ("chunk_key", Text),
            ("text", Bytes),
            ("vector", List(Box::new(F32))),
        ],
        "entry" => vec![
            ("producing_op", U32),
            ("result_ordinal", U32),
            ("action", U8),
            ("object_kind", U8),
            ("object_key", Text),
            ("id", IdRef),
            ("revision", RevisionRef),
            ("collection", optional(U16)),
            ("record", optional(U64)),
            ("attached_chunk_key", optional(Text)),
            ("retained", Bool),
        ],
        "shard" => vec![
            ("channel", U8),
            ("root_key", Text),
            ("batch", U32),
            ("group", U32),
            ("index", U32),
            ("entries", List(Box::new(Record("entry")))),
        ],
        _ => return super::plan::schema(name),
    })
}

pub(crate) fn label(value: &Value, version: bool, error: &'static str) -> Result<()> {
    let s = text(value).map_err(|_| error)?;
    if s.is_empty()
        || s.len() > 256
        || !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        || (version && (s.eq_ignore_ascii_case("latest") || s.eq_ignore_ascii_case("default")))
    {
        return Err(error);
    }
    Ok(())
}

fn validate(name: &str, value: &Value) -> Result<()> {
    let enumeration = |key, max, error| {
        unsigned(field(value, key)?, max)
            .map(|_| ())
            .map_err(|_| error)
    };
    match name {
        "head_data" => enumeration("status", 3, "enum")?,
        "receipt_data" => {
            enumeration("outcome", 3, "enum")?;
            if unsigned(field(field(value, "plan_digest")?, "tag")?, 2)? != 1 {
                return Err("plan_slot_kind");
            }
        }
        "batch_data" => {
            if unsigned(field(field(value, "batch_digest")?, "tag")?, 2)? != 2 {
                return Err("batch_slot_kind");
            }
        }
        "mention_data" | "relation_data" => {
            label(
                field(value, "extractor_version")?,
                true,
                "extractor_version",
            )?;
            let confidence = field(value, "confidence")?;
            if *confidence != Value::Null
                && !matches!(confidence, Value::Float(f) if f.is_finite() && (0.0..=1.0).contains(f))
            {
                return Err("confidence");
            }
        }
        "vector_data" => {
            let Value::Bytes(bytes) = field(value, "text")? else {
                return Err("bytes");
            };
            std::str::from_utf8(bytes).map_err(|_| "utf8")?;
        }
        "entry" => {
            enumeration("action", 2, "enum")?;
            enumeration("object_kind", 2, "object_kind")?;
            let vector = unsigned(field(value, "object_kind")?, 2)? == 2;
            let present = ["collection", "record", "attached_chunk_key"]
                .map(|k| field(value, k).map(|v| *v != Value::Null));
            let [collection, record, chunk] = present;
            let present = [collection?, record?, chunk?];
            let key = text(field(value, "object_key")?)?;
            if vector {
                if !present.iter().all(|v| *v) || !key.is_empty() {
                    return Err("entry_vector_fields");
                }
                if text(field(value, "attached_chunk_key")?)?.is_empty()
                    || text(field(value, "attached_chunk_key")?)?.len() > 71
                {
                    return Err("entry_vector_fields");
                }
            } else if present.iter().any(|v| *v) || key.is_empty() || key.len() > 71 {
                return Err("entry_graph_fields");
            }
        }
        "shard" => {
            enumeration("channel", 1, "enum")?;
            let entries = sequence(field(value, "entries")?)?;
            if entries.is_empty() {
                return Err("empty_shard");
            }
            for pair in entries.windows(2) {
                if entry_order(&pair[0], &pair[1])? != std::cmp::Ordering::Less {
                    return Err("entry_order");
                }
            }
        }
        "revision_data" | "chunk_data" | "entity_data" | "link_data" => {}
        _ => super::plan::validate(name, value)?,
    }
    Ok(())
}

pub(super) fn put_record(w: &mut Writer, name: &str, value: &Value, depth: usize) -> Result<()> {
    if depth > super::MAX_DEPTH {
        return Err("depth");
    }
    let schema = schema(name)?;
    exact(
        value,
        &schema.iter().map(|(key, _)| *key).collect::<Vec<_>>(),
    )?;
    validate(name, value)?;
    let mut inner = Writer(vec![1]);
    for (key, kind) in schema {
        if name == "shard" && key == "entries" {
            let entries = sequence(field(value, key)?)?;
            inner.count(entries.len())?;
            for entry in entries {
                let bytes = encode(&Type::Record("entry"), entry)?;
                inner.raw(&bytes[1..])?;
            }
        } else {
            super::plan::put_nested(&mut inner, &kind, field(value, key)?, depth + 1)?;
        }
    }
    if name == "shard" && inner.0.len() > 3072 {
        return Err("shard_capacity");
    }
    w.raw(&inner.0)
}

pub(super) fn read_record(r: &mut Reader<'_>, name: &str, depth: usize) -> Result<Value> {
    if depth > super::MAX_DEPTH {
        return Err("depth");
    }
    if r.byte()? != 1 {
        return Err("version");
    }
    read_fields(r, name, depth)
}

fn read_fields(r: &mut Reader<'_>, name: &str, depth: usize) -> Result<Value> {
    let mut map = BTreeMap::new();
    for (key, kind) in schema(name)? {
        let value = if name == "shard" && key == "entries" {
            let count = r.count()?;
            if count > r.bytes.len() {
                return Err("truncated");
            }
            let mut entries = Vec::new();
            for _ in 0..count {
                entries.push(read_fields(r, "entry", depth + 1)?);
            }
            Value::List(entries)
        } else {
            super::plan::read_nested(r, &kind, depth + 1)?
        };
        map.insert(key.into(), value);
    }
    let value = Value::Map(map);
    validate(name, &value)?;
    Ok(value)
}

fn entry_order(a: &Value, b: &Value) -> Result<std::cmp::Ordering> {
    for key in ["producing_op", "result_ordinal", "action", "object_kind"] {
        let order = unsigned(field(a, key)?, u32::MAX.into())?
            .cmp(&unsigned(field(b, key)?, u32::MAX.into())?);
        if !order.is_eq() {
            return Ok(order);
        }
    }
    Ok(text(field(a, "object_key")?)?
        .as_bytes()
        .cmp(text(field(b, "object_key")?)?.as_bytes()))
}

pub(crate) fn split_shards(header: &Value, entries: &[Value]) -> Result<Vec<Value>> {
    exact(header, &["channel", "root_key", "batch", "group"])?;
    let mut result = Vec::new();
    let mut pending = Vec::new();
    let mut last = None;
    for entry in entries {
        if let Some(previous) = last
            && entry_order(previous, entry)? != std::cmp::Ordering::Less
        {
            return Err("entry_order");
        }
        last = Some(entry);
        pending.push(entry.clone());
        let shard = shard_value(header, result.len(), &pending)?;
        match encode(&Type::Record("shard"), &shard) {
            Ok(_) => {}
            Err("shard_capacity") => {
                pending.pop();
                if pending.is_empty() {
                    return Err("shard_capacity");
                }
                result.push(shard_value(header, result.len(), &pending)?);
                pending = vec![entry.clone()];
                encode(
                    &Type::Record("shard"),
                    &shard_value(header, result.len(), &pending)?,
                )?;
            }
            Err(error) => return Err(error),
        }
    }
    if !pending.is_empty() {
        result.push(shard_value(header, result.len(), &pending)?);
    }
    Ok(result)
}

fn shard_value(header: &Value, index: usize, entries: &[Value]) -> Result<Value> {
    let mut map = fields(header)?.clone();
    map.insert(
        "index".into(),
        Value::Unsigned(u32::try_from(index).map_err(|_| "integer_range")?.into()),
    );
    map.insert("entries".into(), Value::List(entries.to_vec()));
    Ok(Value::Map(map))
}

pub(crate) fn validate_shards(header: &Value, shards: &[Value]) -> Result<Vec<Value>> {
    let mut entries = Vec::new();
    for (index, shard) in shards.iter().enumerate() {
        encode(&Type::Record("shard"), shard)?;
        for key in ["channel", "root_key", "batch", "group"] {
            if field(header, key)? != field(shard, key)? {
                return Err("shard_backref");
            }
        }
        if unsigned(field(shard, "index")?, u32::MAX.into())? != index as u64 {
            return Err("shard_index");
        }
        entries.extend_from_slice(sequence(field(shard, "entries")?)?);
    }
    if split_shards(header, &entries)? != shards {
        return Err("shard_packing");
    }
    Ok(entries)
}

pub(crate) fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for part in bytes.chunks(3) {
        let a = part[0];
        let b = part.get(1).copied().unwrap_or(0);
        let c = part.get(2).copied().unwrap_or(0);
        out.push(char::from(TABLE[usize::from(a >> 2)]));
        out.push(char::from(TABLE[usize::from((a & 3) << 4 | b >> 4)]));
        out.push(if part.len() > 1 {
            char::from(TABLE[usize::from((b & 15) << 2 | c >> 6)])
        } else {
            '='
        });
        out.push(if part.len() > 2 {
            char::from(TABLE[usize::from(c & 63)])
        } else {
            '='
        });
    }
    out
}

pub(crate) fn unbase64(s: &str) -> Result<Vec<u8>> {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    if !s.len().is_multiple_of(4) {
        return Err("base64");
    }
    let mut bytes = Vec::new();
    for chunk in s.as_bytes().as_chunks::<4>().0 {
        let mut v = [0u8; 4];
        for (index, &b) in chunk.iter().enumerate() {
            v[index] = if b == b'=' {
                0
            } else {
                TABLE.iter().position(|v| *v == b).ok_or("base64")? as u8
            };
        }
        bytes.push(v[0] << 2 | v[1] >> 4);
        if chunk[2] != b'=' {
            bytes.push(v[1] << 4 | v[2] >> 2);
        }
        if chunk[3] != b'=' {
            bytes.push(v[2] << 6 | v[3]);
        }
    }
    if base64(&bytes) != s {
        return Err("base64");
    }
    Ok(bytes)
}

fn fragments(map: &mut BTreeMap<String, Value>, prefix: &str, bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() {
        map.insert(format!("{prefix}00"), Value::Text(String::new()));
    }
    for (index, part) in bytes.chunks(3072).enumerate() {
        if index > 99 {
            return Err("properties");
        }
        map.insert(format!("{prefix}{index:02}"), Value::Text(base64(part)));
    }
    Ok(())
}

pub(crate) fn read_fragments(map: &BTreeMap<String, Value>, prefix: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut count = 0;
    let mut previous = 0;
    for (key, value) in map.iter().filter(|(k, _)| k.starts_with(prefix)) {
        if *key != format!("{prefix}{count:02}") || count >= 100 {
            return Err("fragment_order");
        }
        let part = unbase64(text(value)?)?;
        if part.len() > 3072 || (count > 0 && (previous != 3072 || part.is_empty())) {
            return Err("fragment_size");
        }
        previous = part.len();
        bytes.extend_from_slice(&part);
        count += 1;
    }
    if count == 0 {
        return Err("fragment_order");
    }
    Ok(bytes)
}

pub(crate) fn data_role(identity: &Value) -> Result<&'static str> {
    Ok(match role(identity)? {
        "source" => "head_data",
        "revision" => "revision_data",
        "chunk" => "chunk_data",
        "entity" => "entity_data",
        "has_revision" | "has_chunk" => "link_data",
        "mention" => "mention_data",
        "relation" => "relation_data",
        "receipt" | "delete_receipt" | "abort_receipt" => "receipt_data",
        "batch_receipt" => "batch_data",
        _ => return Err("object_role"),
    })
}

pub(crate) fn projections(identity: &Value, data: &Value) -> Result<BTreeMap<String, Value>> {
    encode(&Type::Identity, identity)?;
    encode(&Type::Record(data_role(identity)?), data)?;
    let identity_fields = field(identity, "fields")?;
    let mut map = BTreeMap::new();
    let mut copy = |key: &str, from: &Value, field_name: &str| -> Result<()> {
        let v = field(from, field_name)?;
        if *v != Value::Null {
            let native = match v {
                Value::Unsigned(n) => Value::Integer(i64::try_from(*n).map_err(|_| "properties")?),
                Value::Float(n) if *n == 0.0 => Value::Float(0.0),
                _ => v.clone(),
            };
            map.insert(key.into(), native);
        }
        Ok(())
    };
    match role(identity)? {
        "source" => {
            copy("status", data, "status")?;
            copy("last_batch", data, "last_batch")?;
        }
        "revision" => {
            for key in ["uri", "text_length", "chunk_count"] {
                copy(key, data, key)?;
            }
            copy("content_digest", identity_fields, "content")?;
        }
        "chunk" => {
            copy("ordinal", data, "ordinal")?;
            copy("start_byte", data, "start")?;
            copy("end_byte", data, "end")?;
            copy("chunk_digest", data, "chunk_digest")?;
        }
        "entity" => {
            copy("entity_kind", identity_fields, "kind")?;
            copy("normalized_key", identity_fields, "normalized_key")?;
        }
        "mention" | "relation" => {
            copy("span_start", identity_fields, "start")?;
            copy("span_end", identity_fields, "end")?;
            copy("confidence", data, "confidence")?;
            copy("extractor_version", data, "extractor_version")?;
            if role(identity)? == "mention" && text(field(data, "surface")?)?.len() <= 4096 {
                copy("surface", data, "surface")?;
            }
        }
        _ => {}
    }
    Ok(map)
}

pub(crate) fn physical(identity: &Value, data: &Value) -> Result<BTreeMap<String, Value>> {
    let identity_bytes = encode(&Type::Identity, identity)?;
    let data_bytes = encode(&Type::Record(data_role(identity)?), data)?;
    let key = identity_key(identity)?;
    let token = key.split('_').nth(1).ok_or("object_role")?;
    let mut map = projections(identity, data)?;
    map.insert("g7_v".into(), Value::Integer(1));
    map.insert("g7_kind".into(), Value::Text(token.into()));
    fragments(&mut map, "i", &identity_bytes)?;
    fragments(&mut map, "d", &data_bytes)?;
    for name in ["source_key", "revision_key", "chunk_key"] {
        if let Some(v) = fields(data)?.get(name)
            && *v != Value::Null
        {
            map.insert(name.into(), v.clone());
        }
    }
    metadata(&map)?;
    Ok(map)
}

pub(crate) fn validate_physical(
    identity: &Value,
    data: &Value,
    map: &BTreeMap<String, Value>,
) -> Result<()> {
    if read_fragments(map, "i")? != encode(&Type::Identity, identity)? {
        return Err("identity_conflict");
    }
    let expected = physical(identity, data)?;
    if metadata(map)? != metadata(&expected)? {
        return Err("evidence_conflict");
    }
    Ok(())
}

pub(crate) fn metadata(map: &BTreeMap<String, Value>) -> Result<Vec<u8>> {
    if map.len() > 64 {
        return Err("properties");
    }
    let mut w = Writer(Vec::new());
    w.count(map.len())?;
    for (key, value) in map {
        if key.is_empty() || key.len() > 256 {
            return Err("properties");
        }
        w.raw(&(key.len() as u16).to_le_bytes())?;
        w.raw(key.as_bytes())?;
        match value {
            Value::Text(s) if s.len() <= 4096 => {
                w.raw(&[1])?;
                w.bytes(s.as_bytes())?;
            }
            Value::Integer(_) => {
                w.raw(&[2])?;
                put(&mut w, &Type::I64, value, 0)?;
            }
            Value::Float(_) => {
                w.raw(&[3])?;
                put(&mut w, &Type::F64, value, 0)?;
            }
            _ => return Err("properties"),
        }
        if w.0.len() > 65536 {
            return Err("properties");
        }
    }
    Ok(w.0)
}

pub(crate) fn properties_digest(map: &BTreeMap<String, Value>) -> Result<[u8; 32]> {
    framed_hash("vlite.g7.v1.properties", &metadata(map)?)
}

pub(crate) fn vector_record(data: &Value) -> Result<Value> {
    encode(&Type::Record("vector_data"), data)?;
    let mut metadata_map = BTreeMap::from([("g7_v".into(), Value::Integer(1))]);
    for key in ["tenant", "source_key", "revision_key", "chunk_key"] {
        metadata_map.insert(key.into(), field(data, key)?.clone());
    }
    metadata(&metadata_map)?;
    let mut vector = Writer(Vec::new());
    for number in sequence(field(data, "vector")?)? {
        put(&mut vector, &Type::F32, number, 0)?;
    }
    let Value::Bytes(payload) = field(data, "text")? else {
        return Err("bytes");
    };
    Ok(Value::Map(BTreeMap::from([
        ("metadata".into(), Value::Map(metadata_map)),
        ("payload".into(), Value::Text(hex(payload))),
        ("vector".into(), Value::Text(hex(&vector.0))),
        ("expiry".into(), Value::Null),
    ])))
}

pub(crate) fn shard_physical(shard: &Value) -> Result<BTreeMap<String, Value>> {
    let bytes = encode(&Type::Record("shard"), shard)?;
    let map = BTreeMap::from([("data".into(), Value::Text(base64(&bytes)))]);
    metadata(&map)?;
    Ok(map)
}

pub(crate) fn encode_object(identity: &Value, data: &Value) -> Result<Vec<u8>> {
    let mut w = Writer(Vec::new());
    w.bytes(&encode(&Type::Identity, identity)?)?;
    w.bytes(&encode(&Type::Record(data_role(identity)?), data)?)?;
    Ok(w.0)
}

pub(crate) fn decode_object(bytes: &[u8]) -> Result<(Value, Value)> {
    let mut reader = Reader { bytes };
    let identity = super::decode(&Type::Identity, reader.blob()?)?;
    let data = super::decode(&Type::Record(data_role(&identity)?), reader.blob()?)?;
    if !reader.bytes.is_empty() {
        return Err("noncanonical");
    }
    Ok((identity, data))
}
