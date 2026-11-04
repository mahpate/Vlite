use super::*;

fn list<'a>(v: &'a Value, key: &str) -> Result<&'a [Value]> {
    sequence(field(v, key)?)
}
fn identity_bytes(v: &Value) -> Result<Vec<u8>> {
    encode(&Type::Identity, v)
}
fn chunk(v: &Value) -> Result<Option<&Value>> {
    match role(v)? {
        "chunk" => Ok(Some(v)),
        "has_chunk" | "mention" | "relation" => Ok(Some(field(of(v)?, "chunk")?)),
        _ => Ok(None),
    }
}
fn object_identity(op: &Value) -> Result<Option<&Value>> {
    let f = of(op)?;
    Ok(match tag(op)? {
        1 => Some(field(f, "edge")?),
        4 => Some(field(f, "node")?),
        5 | 8 | 10 | 12 => Some(field(field(f, "object")?, "identity")?),
        _ => None,
    })
}
fn endpoints(v: &Value) -> Result<Vec<&Value>> {
    let f = of(v)?;
    let keys: &[&str] = match role(v)? {
        "has_revision" => &["source", "revision"],
        "has_chunk" => &["revision", "chunk"],
        "mention" => &["chunk", "entity"],
        "relation" => &["subject", "object"],
        _ => &[],
    };
    keys.iter().map(|k| field(f, k)).collect()
}
fn phase(op: &Value) -> Result<u8> {
    Ok(match tag(op)? {
        0 | 13 => 0,
        1 => 1,
        2 => 2,
        3 => 3,
        4 => 4,
        5 => {
            if role(object_identity(op)?.ok_or("fields")?)? == "chunk" {
                6
            } else {
                5
            }
        }
        6 => 7,
        7 => 8,
        8 => {
            if role(object_identity(op)?.ok_or("fields")?)? == "relation" {
                10
            } else {
                9
            }
        }
        9 => 11,
        10 => 12,
        11 => 13,
        12 => 14,
        _ => return Err("operation_tag"),
    })
}
fn ordered_ids(rows: &[Value], key: &str, error: &'static str) -> Result<()> {
    for pair in rows.windows(2) {
        if super::super::compare_identities(field(&pair[0], key)?, field(&pair[1], key)?)?
            != std::cmp::Ordering::Less
        {
            return Err(error);
        }
    }
    Ok(())
}
fn pair(v: &Value) -> Result<(u64, u64)> {
    Ok((
        unsigned(field(v, "collection")?, u16::MAX.into())?,
        unsigned(field(v, "record")?, u64::MAX)?,
    ))
}

pub(super) fn validate(plan: &Value) -> Result<()> {
    let captured = list(plan, "captured")?;
    ordered_ids(captured, "object", "captured_order")?;
    let mut accepted_last = None;
    for c in list(plan, "accepted")? {
        let ordinal = number(of(field(c, "chunk")?)?, "ordinal")?;
        if accepted_last.is_some_and(|last| last >= ordinal) {
            return Err("accepted_chunk_order");
        }
        accepted_last = Some(ordinal);
    }
    let batches = list(plan, "batches")?;
    let mut known = BTreeSet::new();
    for row in captured {
        known.insert(identity_bytes(field(row, "object")?)?);
    }
    let mut created_chunks = BTreeMap::new();
    let mut chunk_keys = BTreeMap::new();
    for row in captured {
        let id = field(row, "object")?;
        if role(id)? == "chunk" {
            chunk_keys.insert(super::super::identity_key(id)?, (identity_bytes(id)?, id));
        }
    }
    let mut chunk_last = None;
    let mut heads = 0;
    for (bi, batch) in batches.iter().enumerate() {
        if number(batch, "ordinal")? != bi as u64 + 1 {
            return Err("batch_order");
        }
        for (gi, group) in list(batch, "groups")?.iter().enumerate() {
            if number(group, "ordinal")? != gi as u64 {
                return Err("group_order");
            }
            if list(group, "operations")?.is_empty() {
                return Err("empty_group");
            }
            for op in list(group, "operations")? {
                if tag(op)? == 13 {
                    heads += 1;
                }
                if tag(op)? == 5
                    && let Some(id) = object_identity(op)?
                    && role(id)? == "chunk"
                {
                    let bytes = identity_bytes(id)?;
                    created_chunks.insert(bytes.clone(), (bi, gi));
                    chunk_keys.insert(super::super::identity_key(id)?, (bytes, id));
                }
            }
        }
    }
    if heads > 1 {
        return Err("head_create_count");
    }
    for (bi, batch) in batches.iter().enumerate() {
        let groups = list(batch, "groups")?;
        let ops: Vec<_> = groups
            .iter()
            .map(|g| list(g, "operations"))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect();
        let Some(first) = ops.first() else {
            return Err("batch_guard_first");
        };
        if !matches!(tag(first)?, 0 | 13) || (tag(first)? == 13 && bi != 0) {
            return Err("batch_guard_first");
        }
        let mut duplicates = BTreeSet::new();
        let mut edge_ids = BTreeSet::new();
        for op in &ops {
            if !duplicates.insert(encode(&Type::Operation, op)?) {
                return Err("duplicate_operation");
            }
            if matches!(tag(op)?, 1 | 8)
                && let Some(id) = object_identity(op)?
                && !edge_ids.insert(identity_bytes(id)?)
            {
                return Err("duplicate_edge");
            }
        }
        for (gi, group) in groups.iter().enumerate() {
            let group_ops = list(group, "operations")?;
            let detaches: BTreeMap<_, _> = group_ops
                .iter()
                .filter(|o| tag(o) == Ok(2))
                .map(|o| Ok((pair(of(o)?)?, field(of(o)?, "chunk")?)))
                .collect::<Result<_>>()?;
            let mut member: Option<Vec<u8>> = None;
            for op in group_ops {
                let t = tag(op)?;
                let associated = match t {
                    2 | 7 => Some(field(of(op)?, "chunk")?),
                    3 => Some(
                        *detaches
                            .get(&pair(of(op)?)?)
                            .ok_or("group_chunk_membership")?,
                    ),
                    6 => {
                        let key = text(field(field(of(op)?, "data")?, "chunk_key")?)?;
                        let Some((_, id)) = chunk_keys.get(key) else {
                            return Err("put_names_chunk");
                        };
                        Some(*id)
                    }
                    _ => match object_identity(op)? {
                        Some(id) => chunk(id)?,
                        None => None,
                    },
                };
                if let Some(id) = associated {
                    let bytes = identity_bytes(id)?;
                    if matches!(t, 5..=8)
                        && created_chunks
                            .get(&bytes)
                            .is_some_and(|place| *place != (bi, gi))
                    {
                        return Err("chunk_creation_split");
                    }
                    if let Some(previous) = &member
                        && *previous != bytes
                    {
                        return Err("group_chunk_membership");
                    }
                    member = Some(bytes);
                }
            }
            let mut last_phase = 0;
            let mut last_node: Option<String> = None;
            let mut last_provenance: Option<&Value> = None;
            let mut last_relation: Option<&Value> = None;
            for (oi, op) in group_ops.iter().enumerate() {
                let t = tag(op)?;
                if (t == 0 || t == 13) && (gi != 0 || oi != 0) {
                    return Err("batch_guard_first");
                }
                if t == 12 && (bi != 0 || gi + 1 != groups.len() || oi + 1 != group_ops.len()) {
                    return Err("root_receipt_last");
                }
                if t == 13 {
                    known.insert(identity_bytes(field(of(op)?, "source")?)?);
                }
                if let Some(id) = object_identity(op)? {
                    if t == 5 {
                        if role(id)? == "chunk" {
                            let ordinal = number(of(id)?, "ordinal")?;
                            if chunk_last.is_some_and(|last| last >= ordinal) {
                                return Err("chunk_ordinal_order");
                            }
                            chunk_last = Some(ordinal);
                        } else {
                            let key = super::super::identity_key(id)?;
                            if last_node.as_ref().is_some_and(|last| *last >= key) {
                                return Err("node_key_order");
                            }
                            last_node = Some(key);
                        }
                        known.insert(identity_bytes(id)?);
                    }
                    if t == 8 {
                        for endpoint in endpoints(id)? {
                            if !known.contains(&identity_bytes(endpoint)?) {
                                return Err("edge_endpoint");
                            }
                        }
                        let (previous, error) = if role(id)? == "relation" {
                            (&mut last_relation, "relation_identity_order")
                        } else {
                            (&mut last_provenance, "provenance_identity_order")
                        };
                        if let Some(last) = previous
                            && super::super::compare_identities(last, id)?
                                != std::cmp::Ordering::Less
                        {
                            return Err(error);
                        }
                        *previous = Some(id);
                    }
                }
                let p = phase(op)?;
                if p < last_phase {
                    return Err("operation_order");
                }
                last_phase = p;
            }
        }
    }
    Ok(())
}
