use super::{Reader, Result, Type, Value, field, unsigned};

struct Budget {
    remaining: u64,
    decoded: u64,
    entities: u64,
    relations: u64,
    frames: u64,
    operations: u64,
}

impl Budget {
    fn charge(&mut self, bytes: usize) -> Result<()> {
        self.remaining = self
            .remaining
            .checked_sub(bytes as u64)
            .ok_or("prepared_bytes")?;
        Ok(())
    }

    fn scan(&mut self, r: &mut Reader<'_>, kind: &Type, depth: usize, nested: bool) -> Result<()> {
        if depth > super::MAX_DEPTH {
            return Err("depth");
        }
        // Conservative live-tree allowance, including map nodes and allocation overhead.
        // This is transient admission accounting, never a persisted encoding.
        self.charge(256)?;
        match kind {
            Type::U8 | Type::Bool => {
                r.take(1)?;
            }
            Type::U16 => {
                r.take(2)?;
            }
            Type::U32 | Type::F32 => {
                r.take(4)?;
            }
            Type::U64 | Type::I64 | Type::F64 => {
                r.take(8)?;
            }
            Type::Digest => {
                r.take(32)?;
                self.charge(64)?;
            }
            Type::Text | Type::Bytes => {
                self.charge(r.blob()?.len())?;
            }
            Type::Optional(inner) => match r.byte()? {
                0 => {}
                1 => self.scan(r, inner, depth + 1, false)?,
                _ => return Err("presence"),
            },
            Type::List(inner) => {
                let count = r.count()?;
                if count > r.bytes.len() {
                    return Err("truncated");
                }
                for _ in 0..count {
                    self.scan(r, inner, depth + 1, true)?;
                }
            }
            Type::Map(inner) => {
                let count = r.count()?;
                if count > r.bytes.len() {
                    return Err("truncated");
                }
                for _ in 0..count {
                    self.charge(256 + r.blob()?.len())?;
                    self.scan(r, inner, depth + 1, false)?;
                }
            }
            Type::Binding => {
                for t in [Type::Text, Type::Text, Type::Text, Type::Digest] {
                    self.scan(r, &t, depth + 1, false)?;
                }
            }
            Type::Attribute => {
                let t = match r.byte()? {
                    0 => return Ok(()),
                    1 => Type::Bool,
                    2 => Type::I64,
                    3 => Type::U64,
                    4 => Type::F64,
                    5 => Type::Text,
                    _ => return Err("attribute_tag"),
                };
                self.scan(r, &t, depth + 1, false)?;
            }
            Type::Attributes => {
                let bytes = r.blob()?;
                if bytes.len() > 8192 {
                    return Err("attribute_bytes");
                }
                let mut inner = Reader { bytes };
                while !inner.bytes.is_empty() {
                    self.charge(256 + inner.blob()?.len())?;
                    self.scan(&mut inner, &Type::Attribute, depth + 1, false)?;
                }
            }
            Type::IdRef | Type::RevisionRef | Type::Slot => {
                let tag = r.byte()?;
                let width = match (kind, tag) {
                    (Type::Slot, 0..=2) => 32,
                    (Type::RevisionRef, 0) => 0,
                    (Type::IdRef, 0..=1) | (Type::RevisionRef, 1..=2) => 8,
                    _ => return Err("reference_tag"),
                };
                r.take(width)?;
                self.charge(512)?;
            }
            Type::Identity => {
                if nested {
                    let mut inner = Reader { bytes: r.blob()? };
                    self.identity(&mut inner, depth + 1)?;
                    if !inner.bytes.is_empty() {
                        return Err("noncanonical");
                    }
                } else {
                    self.identity(r, depth + 1)?;
                }
            }
            Type::Record(name) => {
                if nested {
                    let mut inner = Reader { bytes: r.blob()? };
                    self.record(&mut inner, name, depth + 1)?;
                    if !inner.bytes.is_empty() {
                        return Err("noncanonical");
                    }
                } else {
                    self.record(r, name, depth + 1)?;
                }
            }
            Type::Operation => {
                for (_, t) in super::plan::operation_schema(r.byte()?.into())? {
                    self.scan(r, &t, depth + 1, true)?;
                }
            }
            Type::Object => {
                let mut identity = Reader { bytes: r.blob()? };
                let role = self.identity(&mut identity, depth + 1)?;
                if !identity.bytes.is_empty() {
                    return Err("noncanonical");
                }
                let name = match role {
                    "source" => "head_data",
                    "revision" => "revision_data",
                    "chunk" => "chunk_data",
                    "entity" => "entity_data",
                    "mention" => "mention_data",
                    "relation" => "relation_data",
                    "has_revision" | "has_chunk" => "link_data",
                    "batch_receipt" => "batch_data",
                    "receipt" | "delete_receipt" | "abort_receipt" => "receipt_data",
                    _ => return Err("object_role"),
                };
                self.scan(r, &Type::Record(name), depth + 1, true)?;
            }
        }
        Ok(())
    }

    fn identity(&mut self, r: &mut Reader<'_>, depth: usize) -> Result<&'static str> {
        if depth > super::MAX_DEPTH {
            return Err("depth");
        }
        if r.byte()? != 1 {
            return Err("version");
        }
        let tag = r.byte()?;
        let role = super::REGISTRY
            .get(usize::from(tag).wrapping_sub(1))
            .ok_or("identity_tag")?
            .0;
        for (key, t) in super::identity_schema(role)? {
            self.charge(256 + key.len())?;
            self.scan(r, t, depth + 1, matches!(t, Type::Identity))?;
        }
        Ok(role)
    }

    fn record(&mut self, r: &mut Reader<'_>, name: &str, depth: usize) -> Result<()> {
        if depth > super::MAX_DEPTH {
            return Err("depth");
        }
        if r.byte()? != 1 {
            return Err("version");
        }
        let mut extraction = 0;
        for (key, t) in super::stored::schema(name)? {
            self.charge(256 + key.len())?;
            let before = r.bytes.len();
            if let Type::List(_) = &t {
                let mut peek = Reader { bytes: r.bytes };
                let count = peek.count()? as u64;
                let limit = match (name, key) {
                    ("accepted_chunk", "entities") => self.entities,
                    ("accepted_chunk", "relations") => self.relations,
                    ("plan", "batches") => self.frames,
                    ("group", "operations") => self.operations,
                    _ => u32::MAX.into(),
                };
                if count > limit {
                    return Err("policy_limit");
                }
            }
            if name == "shard" && key == "entries" {
                let count = r.count()?;
                if count > r.bytes.len() {
                    return Err("truncated");
                }
                for _ in 0..count {
                    for (key, t) in super::stored::schema("entry")? {
                        self.charge(256 + key.len())?;
                        self.scan(r, &t, depth + 1, false)?;
                    }
                }
            } else {
                self.scan(r, &t, depth + 1, true)?;
            }
            if name == "accepted_chunk" && matches!(key, "entities" | "relations") {
                extraction += before - r.bytes.len();
                if extraction as u64 + 1 > self.decoded {
                    return Err("decoded_bytes");
                }
            }
        }
        Ok(())
    }
}

pub(super) fn check(kind: &Type, bytes: &[u8], policy: &Value) -> Result<()> {
    super::encode(&Type::Record("policy"), policy)?;
    let cap = |key| unsigned(field(policy, key)?, 67108864);
    let mut budget = Budget {
        remaining: cap("prepared_bytes")?,
        decoded: cap("decoded_bytes")?,
        entities: cap("chunk_entities")?,
        relations: cap("chunk_relations")?,
        frames: cap("frames")?,
        operations: cap("operations")?.min(4096),
    };
    budget.charge(bytes.len())?;
    let mut r = Reader { bytes };
    budget.scan(&mut r, kind, 0, false)?;
    if !r.bytes.is_empty() {
        return Err("noncanonical");
    }

    Ok(())
}

pub(super) fn check_default(kind: &Type, bytes: &[u8]) -> Result<()> {
    let mut budget = Budget {
        remaining: 67108864,
        decoded: 1048576,
        entities: 256,
        relations: 512,
        frames: 64,
        operations: 4096,
    };
    budget.charge(bytes.len())?;
    let mut r = Reader { bytes };
    budget.scan(&mut r, kind, 0, false)?;
    if !r.bytes.is_empty() {
        return Err("noncanonical");
    }
    Ok(())
}
