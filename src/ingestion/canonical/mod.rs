#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

mod budget;
pub(crate) mod plan;
pub(crate) mod stored;

pub(crate) type Result<T> = std::result::Result<T, &'static str>;
const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_DEPTH: usize = 32;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Value {
    Null,
    Bool(bool),
    Unsigned(u64),
    Integer(i64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
}

#[derive(Clone, Debug)]
pub(crate) enum Type {
    U8,
    U16,
    U32,
    U64,
    I64,
    F32,
    F64,
    Bool,
    Text,
    Bytes,
    Digest,
    Binding,
    Attribute,
    Attributes,
    IdRef,
    RevisionRef,
    Slot,
    Optional(Box<Type>),
    List(Box<Type>),
    Map(Box<Type>),
    Identity,
    Record(&'static str),
    Operation,
    Object,
}

pub(crate) fn unhex(text: &str) -> Result<Vec<u8>> {
    if !text.len().is_multiple_of(2)
        || !text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("hex");
    }
    let nibble = |b: u8| if b <= b'9' { b - b'0' } else { b - b'a' + 10 };
    Ok(text
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| nibble(pair[0]) * 16 + nibble(pair[1]))
        .collect())
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|b| {
            [
                char::from(DIGITS[usize::from(b >> 4)]),
                char::from(DIGITS[usize::from(b & 15)]),
            ]
        })
        .collect()
}

struct Writer(Vec<u8>);

impl Writer {
    fn raw(&mut self, bytes: &[u8]) -> Result<()> {
        let length = self
            .0
            .len()
            .checked_add(bytes.len())
            .ok_or("prepared_bytes")?;
        if length > MAX_BYTES {
            return Err("prepared_bytes");
        }
        self.0.extend_from_slice(bytes);
        Ok(())
    }

    fn count(&mut self, count: usize) -> Result<()> {
        self.raw(
            &u32::try_from(count)
                .map_err(|_| "integer_range")?
                .to_le_bytes(),
        )
    }

    fn bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.count(bytes.len())?;
        self.raw(bytes)
    }
}

fn unsigned(value: &Value, max: u64) -> Result<u64> {
    match value {
        Value::Unsigned(v) if *v <= max => Ok(*v),
        Value::Integer(v) if *v >= 0 && (*v as u64) <= max => Ok(*v as u64),
        _ => Err("integer_range"),
    }
}

fn text(value: &Value) -> Result<&str> {
    match value {
        Value::Text(v) => Ok(v),
        _ => Err("text"),
    }
}

fn fields(value: &Value) -> Result<&BTreeMap<String, Value>> {
    match value {
        Value::Map(v) => Ok(v),
        _ => Err("fields"),
    }
}

fn field<'a>(value: &'a Value, name: &str) -> Result<&'a Value> {
    fields(value)?.get(name).ok_or("fields")
}

fn exact(value: &Value, names: &[&str]) -> Result<()> {
    let map = fields(value)?;
    if map.len() != names.len() || names.iter().any(|name| !map.contains_key(*name)) {
        return Err("fields");
    }
    Ok(())
}

fn bounded_text(value: &Value, min: usize, max: usize, error: &'static str) -> Result<()> {
    let s = text(value).map_err(|_| error)?;
    if s.len() < min || s.len() > max || s.as_bytes().contains(&0) {
        return Err(error);
    }
    Ok(())
}

fn identifier(value: &Value) -> bool {
    text(value).is_ok_and(|s| {
        !s.is_empty() && s.len() <= 63 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    })
}

fn normalized(value: &Value) -> bool {
    text(value).is_ok_and(|s| {
        !s.is_empty()
            && s.len() <= 256
            && !s.bytes().any(|b| b.is_ascii_uppercase())
            && !s.bytes().any(|b| matches!(b, b'\t' | b'\r' | b'\n'))
            && !s.starts_with(' ')
            && !s.ends_with(' ')
            && !s.contains("  ")
    })
}

fn sequence(value: &Value) -> Result<&[Value]> {
    match value {
        Value::List(v) => Ok(v),
        _ => Err("list"),
    }
}

pub(crate) fn encode(kind: &Type, value: &Value) -> Result<Vec<u8>> {
    let mut writer = Writer(Vec::new());
    put(&mut writer, kind, value, 0)?;
    Ok(writer.0)
}

fn put(w: &mut Writer, kind: &Type, v: &Value, depth: usize) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err("depth");
    }
    match kind {
        Type::U8 => w.raw(&[unsigned(v, u8::MAX.into())? as u8]),
        Type::U16 => w.raw(&(unsigned(v, u16::MAX.into())? as u16).to_le_bytes()),
        Type::U32 => w.raw(&(unsigned(v, u32::MAX.into())? as u32).to_le_bytes()),
        Type::U64 => w.raw(&unsigned(v, u64::MAX)?.to_le_bytes()),
        Type::I64 => {
            let value = match v {
                Value::Integer(n) => *n,
                Value::Unsigned(n) => i64::try_from(*n).map_err(|_| "integer_range")?,
                _ => return Err("integer_range"),
            };
            w.raw(&value.to_le_bytes())
        }
        Type::F32 | Type::F64 => {
            let Value::Float(value) = v else {
                return Err("float");
            };
            if !value.is_finite() {
                return Err("float");
            }
            if matches!(kind, Type::F32) {
                let f = *value as f32;
                if !f.is_finite() {
                    return Err("float");
                }
                w.raw(&(if f == 0.0 { 0.0f32 } else { f }).to_le_bytes())
            } else {
                w.raw(&(if *value == 0.0 { 0.0 } else { *value }).to_le_bytes())
            }
        }
        Type::Bool => match v {
            Value::Bool(b) => w.raw(&[u8::from(*b)]),
            _ => Err("boolean"),
        },
        Type::Text => w.bytes(text(v)?.as_bytes()),
        Type::Bytes => match v {
            Value::Bytes(b) => w.bytes(b),
            _ => Err("bytes"),
        },
        Type::Digest => {
            let bytes = unhex(text(v)?)?;
            if bytes.len() != 32 {
                return Err("digest_width");
            }
            w.raw(&bytes)
        }
        Type::Optional(inner) => {
            if *v == Value::Null {
                w.raw(&[0])
            } else {
                w.raw(&[1])?;
                put(w, inner, v, depth + 1)
            }
        }
        Type::List(inner) => {
            let values = sequence(v)?;
            w.count(values.len())?;
            for value in values {
                put(w, inner, value, depth + 1)?;
            }
            Ok(())
        }
        Type::Map(inner) => {
            let values = fields(v)?;
            w.count(values.len())?;
            for (key, value) in values {
                w.bytes(key.as_bytes())?;
                put(w, inner, value, depth + 1)?;
            }
            Ok(())
        }
        Type::Binding => {
            exact(v, &["graph", "collection", "tenant", "generation"])?;
            for (name, cap) in [("graph", 63), ("collection", 63), ("tenant", 128)] {
                bounded_text(field(v, name)?, 1, cap, "binding")?;
                put(w, &Type::Text, field(v, name)?, depth + 1)?;
            }
            put(w, &Type::Digest, field(v, "generation")?, depth + 1)
        }
        Type::Attribute => {
            let pair = sequence(v)?;
            if pair.len() != 2 {
                return Err("attribute_tag");
            }
            let tag = unsigned(&pair[0], 5).map_err(|_| "attribute_tag")?;
            w.raw(&[tag as u8])?;
            match tag {
                0 if pair[1] == Value::Null => Ok(()),
                0 => Err("attribute_tag"),
                1 => put(w, &Type::Bool, &pair[1], depth + 1),
                2 => put(w, &Type::I64, &pair[1], depth + 1),
                3 => put(w, &Type::U64, &pair[1], depth + 1),
                4 => put(w, &Type::F64, &pair[1], depth + 1),
                _ => {
                    if text(&pair[1])?.len() > 4096 {
                        return Err("attribute_string_limit");
                    }
                    put(w, &Type::Text, &pair[1], depth + 1)
                }
            }
        }
        Type::Attributes => {
            let mut inner = Writer(Vec::new());
            let map = fields(v)?;
            if map.len() > 16 {
                return Err("attribute_fields");
            }
            for (key, value) in map {
                if key.is_empty() {
                    return Err("attribute_key");
                }
                if key.len() > 4096 {
                    return Err("attribute_string");
                }
                inner.bytes(key.as_bytes())?;
                put(&mut inner, &Type::Attribute, value, depth + 1)?;
            }
            if inner.0.len() > 8192 {
                return Err("attribute_bytes");
            }
            w.bytes(&inner.0)
        }
        Type::IdRef | Type::RevisionRef | Type::Slot => {
            exact(v, &["tag", "value"])?;
            let tag = unsigned(field(v, "tag")?, 2).map_err(|_| "reference_tag")?;
            w.raw(&[tag as u8])?;
            let value = field(v, "value")?;
            match kind {
                Type::Slot => put(w, &Type::Digest, value, depth + 1),
                Type::IdRef if tag == 0 => put(w, &Type::U64, value, depth + 1),
                Type::RevisionRef if tag == 0 && *value == Value::Null => Ok(()),
                Type::RevisionRef if tag == 1 => put(w, &Type::U64, value, depth + 1),
                Type::IdRef if tag == 1 => put_pair(w, value, depth),
                Type::RevisionRef if tag == 2 => put_pair(w, value, depth),
                _ => Err("reference_tag"),
            }
        }
        Type::Identity => put_identity(w, v, depth + 1),
        Type::Record(name) => stored::put_record(w, name, v, depth + 1),
        Type::Operation => plan::put_operation(w, v, depth + 1),
        Type::Object => plan::put_object(w, v, depth + 1),
    }
}

fn put_pair(w: &mut Writer, value: &Value, depth: usize) -> Result<()> {
    let pair = sequence(value)?;
    if pair.len() != 2 {
        return Err("reference_tag");
    }
    for v in pair {
        put(w, &Type::U32, v, depth + 1)?;
    }
    Ok(())
}

const REGISTRY: [(&str, &str, &str); 14] = [
    ("source", "source", "src"),
    ("revision", "revision", "rev"),
    ("chunk", "chunk", "chk"),
    ("entity", "entity", "ent"),
    ("has_revision", "has-revision", "hr"),
    ("has_chunk", "has-chunk", "hc"),
    ("mention", "mention", "men"),
    ("relation", "relation", "rel"),
    ("receipt", "receipt", "rec"),
    ("batch_receipt", "batch-receipt", "bat"),
    ("result_shard", "result-shard", "ids"),
    ("ownership_shard", "ownership-shard", "own"),
    ("delete_receipt", "delete-receipt", "del"),
    ("abort_receipt", "abort-receipt", "abt"),
];

fn role(v: &Value) -> Result<&str> {
    text(field(v, "kind")?)
}

fn nested(w: &mut Writer, value: &Value, allowed: &[&str], depth: usize) -> Result<()> {
    if !allowed.contains(&role(value)?) {
        return Err("identity_role");
    }
    let mut inner = Writer(Vec::new());
    put(&mut inner, &Type::Identity, value, depth + 1)?;
    w.bytes(&inner.0)
}

fn put_identity(w: &mut Writer, value: &Value, depth: usize) -> Result<()> {
    exact(value, &["kind", "fields"])?;
    let name = role(value)?;
    let index = REGISTRY
        .iter()
        .position(|row| row.0 == name)
        .ok_or("identity_tag")?;
    let v = field(value, "fields")?;
    w.raw(&[1, (index + 1) as u8])?;
    let schema = identity_schema(name)?;
    exact(v, &schema.iter().map(|(name, _)| *name).collect::<Vec<_>>())?;
    if fields(v)?.contains_key("binding")
        && field(v, "tenant")? != field(field(v, "binding")?, "tenant")?
    {
        return Err("binding");
    }
    if fields(v)?.contains_key("source_id") {
        bounded_text(field(v, "source_id")?, 1, 256, "source_id")?;
    }
    if name == "entity"
        && (!identifier(field(v, "kind")?) || !normalized(field(v, "normalized_key")?))
    {
        return Err("entity_identity");
    }
    if name == "relation" && !identifier(field(v, "predicate")?) {
        return Err("relation_identity");
    }
    if fields(v)?.contains_key("start")
        && unsigned(field(v, "start")?, u32::MAX.into())?
            >= unsigned(field(v, "end")?, u32::MAX.into())?
    {
        return Err(if name == "chunk" {
            "chunk_span"
        } else {
            "evidence_span"
        });
    }
    for (field_name, kind) in schema {
        let item = field(v, field_name)?;
        if matches!(kind, Type::Identity) {
            let allowed: &[&str] = match *field_name {
                "source" => &["source"],
                "revision" => &["revision"],
                "chunk" => &["chunk"],
                "entity" | "subject" | "object" => &["entity"],
                "aborted" => {
                    if role(item)? == "abort_receipt" {
                        return Err("abort_of_abort");
                    }
                    &["receipt", "delete_receipt"]
                }
                _ => &["receipt", "delete_receipt", "abort_receipt"],
            };
            nested(w, item, allowed, depth)?;
        } else {
            put(w, kind, item, depth + 1)?;
        }
    }
    Ok(())
}

fn identity_schema(name: &str) -> Result<&'static [(&'static str, Type)]> {
    Ok(match name {
        "source" => &[
            ("binding", Type::Binding),
            ("tenant", Type::Text),
            ("source_id", Type::Text),
        ],
        "revision" => &[
            ("source", Type::Identity),
            ("content", Type::Digest),
            ("pipeline", Type::Digest),
        ],
        "chunk" => &[
            ("revision", Type::Identity),
            ("ordinal", Type::U32),
            ("start", Type::U32),
            ("end", Type::U32),
        ],
        "entity" => &[
            ("binding", Type::Binding),
            ("tenant", Type::Text),
            ("kind", Type::Text),
            ("normalized_key", Type::Text),
        ],
        "has_revision" => &[("source", Type::Identity), ("revision", Type::Identity)],
        "has_chunk" => &[("revision", Type::Identity), ("chunk", Type::Identity)],
        "mention" => &[
            ("chunk", Type::Identity),
            ("entity", Type::Identity),
            ("start", Type::U32),
            ("end", Type::U32),
        ],
        "relation" => &[
            ("revision", Type::Identity),
            ("chunk", Type::Identity),
            ("subject", Type::Identity),
            ("predicate", Type::Text),
            ("object", Type::Identity),
            ("start", Type::U32),
            ("end", Type::U32),
            ("attributes", Type::Attributes),
        ],
        "receipt" => &[
            ("binding", Type::Binding),
            ("tenant", Type::Text),
            ("source_id", Type::Text),
            ("content", Type::Digest),
            ("pipeline", Type::Digest),
        ],
        "batch_receipt" => &[("root", Type::Identity), ("ordinal", Type::U32)],
        "result_shard" | "ownership_shard" => &[
            ("root", Type::Identity),
            ("batch", Type::U32),
            ("group", Type::U32),
            ("index", Type::U32),
        ],
        "delete_receipt" => &[
            ("binding", Type::Binding),
            ("tenant", Type::Text),
            ("source_id", Type::Text),
            ("expected", Type::U64),
        ],
        "abort_receipt" => &[
            ("binding", Type::Binding),
            ("tenant", Type::Text),
            ("source_id", Type::Text),
            ("aborted", Type::Identity),
        ],
        _ => return Err("identity_tag"),
    })
}

pub(crate) fn framed_hash(domain: &str, payload: &[u8]) -> Result<[u8; 32]> {
    let suffix = domain.strip_prefix("vlite.g7.v1.").ok_or("hash_domain")?;
    if !REGISTRY.iter().any(|row| row.1 == suffix)
        && ![
            "pipeline",
            "request",
            "plan",
            "batch",
            "delete-action",
            "abort-action",
            "policy",
            "properties",
        ]
        .contains(&suffix)
    {
        return Err("hash_domain");
    }
    let mut w = Writer(vec![1]);
    w.bytes(domain.as_bytes())?;
    w.bytes(payload)?;
    Ok(Sha256::digest(&w.0).into())
}

pub(crate) fn identity_key(value: &Value) -> Result<String> {
    let row = REGISTRY
        .iter()
        .find(|row| Ok(row.0) == role(value))
        .ok_or("identity_tag")?;
    let digest = framed_hash(
        &format!("vlite.g7.v1.{}", row.1),
        &encode(&Type::Identity, value)?,
    )?;
    Ok(format!("g7_{}_{}", row.2, hex(&digest)))
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let bytes = self.bytes.get(..count).ok_or("truncated")?;
        self.bytes = &self.bytes[count..];
        Ok(bytes)
    }
    fn number<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.take(N)?.try_into().map_err(|_| "truncated")
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.number::<1>()?[0])
    }
    fn count(&mut self) -> Result<usize> {
        Ok(u32::from_le_bytes(self.number()?) as usize)
    }
    fn blob(&mut self) -> Result<&'a [u8]> {
        let count = self.count()?;
        self.take(count)
    }
}

pub(crate) fn decode(kind: &Type, bytes: &[u8]) -> Result<Value> {
    if bytes.len() > MAX_BYTES {
        return Err("prepared_bytes");
    }
    budget::check_default(kind, bytes)?;

    let mut r = Reader { bytes };
    let value = read(&mut r, kind, 0)?;
    if !r.bytes.is_empty() || encode(kind, &value)? != bytes {
        return Err("noncanonical");
    }
    Ok(value)
}

pub(crate) fn decode_with_policy(kind: &Type, bytes: &[u8], policy: &Value) -> Result<Value> {
    budget::check(kind, bytes, policy)?;
    decode(kind, bytes)
}

fn read(r: &mut Reader<'_>, kind: &Type, depth: usize) -> Result<Value> {
    if depth > MAX_DEPTH {
        return Err("depth");
    }
    let value = match kind {
        Type::U8 => Value::Unsigned(r.byte()?.into()),
        Type::U16 => Value::Unsigned(u16::from_le_bytes(r.number()?).into()),
        Type::U32 => Value::Unsigned(u32::from_le_bytes(r.number()?).into()),
        Type::U64 => Value::Unsigned(u64::from_le_bytes(r.number()?)),
        Type::I64 => Value::Integer(i64::from_le_bytes(r.number()?)),
        Type::F32 => {
            let value = f32::from_le_bytes(r.number()?);
            if !value.is_finite() || (value == 0.0 && value.is_sign_negative()) {
                return Err("float");
            }
            Value::Float(value.into())
        }
        Type::F64 => {
            let value = f64::from_le_bytes(r.number()?);
            if !value.is_finite() || (value == 0.0 && value.is_sign_negative()) {
                return Err("float");
            }
            Value::Float(value)
        }
        Type::Bool => Value::Bool(match r.byte()? {
            0 => false,
            1 => true,
            _ => return Err("boolean"),
        }),
        Type::Text => Value::Text(
            std::str::from_utf8(r.blob()?)
                .map_err(|_| "utf8")?
                .to_owned(),
        ),
        Type::Bytes => Value::Bytes(r.blob()?.to_vec()),
        Type::Digest => Value::Text(hex(r.take(32)?)),
        Type::Optional(inner) => match r.byte()? {
            0 => Value::Null,
            1 => read(r, inner, depth + 1)?,
            _ => return Err("presence"),
        },
        Type::List(inner) => {
            let count = r.count()?;
            if count > r.bytes.len() {
                return Err("truncated");
            }
            let mut values = Vec::new();
            for _ in 0..count {
                values.push(read(r, inner, depth + 1)?);
            }
            Value::List(values)
        }
        Type::Map(inner) => {
            let count = r.count()?;
            let mut map = BTreeMap::new();
            for _ in 0..count {
                let key = text(&read(r, &Type::Text, depth + 1)?)?.to_owned();
                if map
                    .last_key_value()
                    .is_some_and(|(previous, _)| previous >= &key)
                {
                    return Err("map_order");
                }
                map.insert(key, read(r, inner, depth + 1)?);
            }
            Value::Map(map)
        }
        Type::Binding => {
            let mut map = BTreeMap::new();
            for (name, t) in [
                ("graph", Type::Text),
                ("collection", Type::Text),
                ("tenant", Type::Text),
                ("generation", Type::Digest),
            ] {
                map.insert(name.into(), read(r, &t, depth + 1)?);
            }
            Value::Map(map)
        }
        Type::Attribute => {
            let tag = r.byte()?;
            let item = match tag {
                0 => Value::Null,
                1 => read(r, &Type::Bool, depth + 1)?,
                2 => read(r, &Type::I64, depth + 1)?,
                3 => read(r, &Type::U64, depth + 1)?,
                4 => read(r, &Type::F64, depth + 1)?,
                5 => read(r, &Type::Text, depth + 1)?,
                _ => return Err("attribute_tag"),
            };
            Value::List(vec![Value::Unsigned(tag.into()), item])
        }
        Type::Attributes => {
            let mut inner = Reader { bytes: r.blob()? };
            let mut map = BTreeMap::new();
            while !inner.bytes.is_empty() {
                let key = text(&read(&mut inner, &Type::Text, depth + 1)?)?.to_owned();
                if map
                    .last_key_value()
                    .is_some_and(|(previous, _)| previous >= &key)
                {
                    return Err("map_order");
                }
                map.insert(key, read(&mut inner, &Type::Attribute, depth + 1)?);
            }
            Value::Map(map)
        }
        Type::IdRef | Type::RevisionRef | Type::Slot => {
            let tag = r.byte()?;
            let item = match (kind, tag) {
                (Type::Slot, 0..=2) => read(r, &Type::Digest, depth + 1)?,
                (Type::IdRef, 0) | (Type::RevisionRef, 1) => read(r, &Type::U64, depth + 1)?,
                (Type::RevisionRef, 0) => Value::Null,
                (Type::IdRef, 1) | (Type::RevisionRef, 2) => Value::List(vec![
                    read(r, &Type::U32, depth + 1)?,
                    read(r, &Type::U32, depth + 1)?,
                ]),
                _ => return Err("reference_tag"),
            };
            Value::Map(BTreeMap::from([
                ("tag".into(), Value::Unsigned(tag.into())),
                ("value".into(), item),
            ]))
        }
        Type::Identity => {
            if r.byte()? != 1 {
                return Err("version");
            }
            let tag = r.byte()?;
            let name = REGISTRY
                .get(usize::from(tag).wrapping_sub(1))
                .ok_or("identity_tag")?
                .0;
            let mut values = BTreeMap::new();
            for (field_name, kind) in identity_schema(name)? {
                let item = if matches!(kind, Type::Identity) {
                    let mut nested = Reader { bytes: r.blob()? };
                    let item = read(&mut nested, kind, depth + 1)?;
                    if !nested.bytes.is_empty() {
                        return Err("noncanonical");
                    }
                    item
                } else {
                    read(r, kind, depth + 1)?
                };
                values.insert((*field_name).into(), item);
            }
            Value::Map(BTreeMap::from([
                ("kind".into(), Value::Text(name.into())),
                ("fields".into(), Value::Map(values)),
            ]))
        }
        Type::Record(name) => stored::read_record(r, name, depth + 1)?,
        Type::Operation => plan::read_operation(r, depth + 1)?,
        Type::Object => plan::read_object(r, depth + 1)?,
    };
    Ok(value)
}

pub(crate) fn compare_identities(left: &Value, right: &Value) -> Result<std::cmp::Ordering> {
    encode(&Type::Identity, left)?;
    encode(&Type::Identity, right)?;
    compare_identity(left, right)
}

fn compare_identity(left: &Value, right: &Value) -> Result<std::cmp::Ordering> {
    let tag = |v| {
        REGISTRY
            .iter()
            .position(|row| Ok(row.0) == role(v))
            .ok_or("identity_tag")
    };
    let order = tag(left)?.cmp(&tag(right)?);
    if !order.is_eq() {
        return Ok(order);
    }
    for (name, kind) in identity_schema(role(left)?)? {
        let a = field(field(left, "fields")?, name)?;
        let b = field(field(right, "fields")?, name)?;
        let order = match kind {
            Type::Identity => compare_identity(a, b)?,
            Type::U32 | Type::U64 => unsigned(a, u64::MAX)?.cmp(&unsigned(b, u64::MAX)?),
            Type::Text | Type::Digest => text(a)?.as_bytes().cmp(text(b)?.as_bytes()),
            Type::Attributes => encode(kind, a)?.get(4..).cmp(&encode(kind, b)?.get(4..)),
            Type::Binding => {
                let mut order = std::cmp::Ordering::Equal;
                for key in ["graph", "collection", "tenant", "generation"] {
                    order = text(field(a, key)?)?
                        .as_bytes()
                        .cmp(text(field(b, key)?)?.as_bytes());
                    if !order.is_eq() {
                        break;
                    }
                }
                order
            }
            _ => return Err("identity_type"),
        };
        if !order.is_eq() {
            return Ok(order);
        }
    }
    Ok(std::cmp::Ordering::Equal)
}
