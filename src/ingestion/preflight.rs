#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};

use crate::ingestion_canonical::canonical as codec;
use crate::property_graph::engine::{
    EdgeSelector, GraphBody, GraphEntry, GraphError, GraphMutation, MutationOutcome, encode_body,
    model::{Precondition, Properties, PropertyValue},
};
use crate::property_graph::{EdgeRef, GraphOp, NodePatch};
use crate::{Database, Error, Expected, Metadata, MetadataValue, Mutation};

#[derive(Debug)]
enum Failure {
    Database(Error),
    Engine(GraphError),
    Canonical(&'static str),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "{error}"),
            Self::Engine(error) => write!(formatter, "{error}"),
            Self::Canonical(class) => formatter.write_str(class),
        }
    }
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Self::Database(error)
    }
}

impl From<GraphError> for Failure {
    fn from(error: GraphError) -> Self {
        Self::Engine(error)
    }
}

#[derive(Debug)]
struct Admission {
    body_bytes: usize,
    user_operations: usize,
    outcomes: Vec<Option<MutationOutcome>>,
}

enum Lowered {
    Graph(GraphMutation),
    Vector(u16, Mutation),
}

fn canonical_error<T>(result: codec::Result<T>) -> Result<T, Failure> {
    result.map_err(Failure::Canonical)
}

fn cfields(value: &codec::Value) -> Result<&BTreeMap<String, codec::Value>, Failure> {
    match value {
        codec::Value::Map(fields) => Ok(fields),
        _ => Err(Failure::Canonical("fields")),
    }
}

fn cfield<'a>(value: &'a codec::Value, name: &str) -> Result<&'a codec::Value, Failure> {
    cfields(value)?
        .get(name)
        .ok_or(Failure::Canonical("fields"))
}

fn ctext(value: &codec::Value) -> Result<&str, Failure> {
    match value {
        codec::Value::Text(text) => Ok(text),
        _ => Err(Failure::Canonical("text")),
    }
}

fn cbytes(value: &codec::Value) -> Result<&[u8], Failure> {
    match value {
        codec::Value::Bytes(bytes) => Ok(bytes),
        _ => Err(Failure::Canonical("bytes")),
    }
}

fn cu64(value: &codec::Value) -> Result<u64, Failure> {
    match value {
        codec::Value::Unsigned(number) => Ok(*number),
        codec::Value::Integer(number) if *number >= 0 => Ok(*number as u64),
        _ => Err(Failure::Canonical("integer_range")),
    }
}

fn cexpected(value: &codec::Value, previous_revision: Option<u64>) -> Result<Expected, Failure> {
    let tag = cu64(cfield(value, "tag")?)?;
    let value = cfield(value, "value")?;
    match tag {
        0 => Ok(Expected::Absent),
        1 => Ok(Expected::Revision(cu64(value)?)),
        2 => previous_revision
            .map(Expected::Revision)
            .ok_or(Failure::Canonical("symbolic_revision")),
        _ => Err(Failure::Canonical("revision_ref")),
    }
}

fn cmetadata(values: BTreeMap<String, codec::Value>) -> Result<Metadata, Failure> {
    values
        .into_iter()
        .map(|(key, value)| {
            let value = match value {
                codec::Value::Text(value) => MetadataValue::String(value),
                codec::Value::Integer(value) => MetadataValue::Integer(value),
                codec::Value::Float(value) => MetadataValue::Float(value),
                _ => return Err(Failure::Canonical("properties")),
            };
            Ok((key, value))
        })
        .collect()
}

fn cproperties(identity: &codec::Value, data: &codec::Value) -> Result<Metadata, Failure> {
    let physical = canonical_error(codec::stored::physical(identity, data))?;
    cmetadata(physical)
}

fn cidentity_key(identity: &codec::Value) -> Result<String, Failure> {
    canonical_error(codec::identity_key(identity))
}

fn crole(identity: &codec::Value) -> Result<&str, Failure> {
    ctext(cfield(identity, "kind")?)
}

fn node_label(role: &str) -> Result<&'static str, Failure> {
    match role {
        "source" => Ok("G7Source"),
        "revision" => Ok("G7Revision"),
        "chunk" => Ok("G7Chunk"),
        "entity" => Ok("G7Entity"),
        "receipt" => Ok("G7Receipt"),
        "batch_receipt" => Ok("G7BatchReceipt"),
        "result_shard" => Ok("G7ResultShard"),
        "ownership_shard" => Ok("G7OwnershipShard"),
        "delete_receipt" => Ok("G7DeleteReceipt"),
        "abort_receipt" => Ok("G7AbortReceipt"),
        _ => Err(Failure::Canonical("object_role")),
    }
}

fn cnode(
    identity: &codec::Value,
    data: &codec::Value,
    expected: Expected,
) -> Result<GraphOp, Failure> {
    let key = cidentity_key(identity)?;
    let labels = vec![node_label(crole(identity)?)?.to_owned()];
    let properties = cproperties(identity, data)?;
    Ok(GraphOp::UpsertNode {
        key,
        labels,
        properties,
        expected,
    })
}

fn cshard_node(identity: &codec::Value, shard: &codec::Value) -> Result<GraphOp, Failure> {
    let properties = canonical_error(codec::stored::shard_physical(shard))?;
    Ok(GraphOp::UpsertNode {
        key: cidentity_key(identity)?,
        labels: vec![node_label(crole(identity)?)?.to_owned()],
        properties: cmetadata(properties)?,
        expected: Expected::Absent,
    })
}

fn relationship(identity: &codec::Value) -> Result<(String, String, String), Failure> {
    let fields = cfields(cfield(identity, "fields")?)?;
    let role = crole(identity)?;
    let (source, destination) = match role {
        "has_revision" => ("source", "revision"),
        "has_chunk" => ("revision", "chunk"),
        "mention" => ("chunk", "entity"),
        "relation" => ("subject", "object"),
        _ => return Err(Failure::Canonical("object_role")),
    };
    let relationship = match role {
        "has_revision" => "HAS_REVISION".to_owned(),
        "has_chunk" => "HAS_CHUNK".to_owned(),
        "mention" => "MENTIONS".to_owned(),
        "relation" => ctext(
            fields
                .get("predicate")
                .ok_or(Failure::Canonical("fields"))?,
        )?
        .to_owned(),
        _ => unreachable!("the role was checked above"),
    };
    let source = cidentity_key(fields.get(source).ok_or(Failure::Canonical("fields"))?)?;
    let destination = cidentity_key(
        fields
            .get(destination)
            .ok_or(Failure::Canonical("fields"))?,
    )?;
    Ok((relationship, source, destination))
}

fn binding_name(identity: &codec::Value, name: &str) -> Result<String, Failure> {
    let binding = cfield(cfield(identity, "fields")?, "binding")?;
    Ok(ctext(cfield(binding, name)?)?.to_owned())
}

fn canonical_object(
    db: &Database,
    graph: &str,
    identity: &codec::Value,
    data: &codec::Value,
    expected: Expected,
) -> Result<GraphOp, Failure> {
    let role = crole(identity)?;
    if matches!(role, "has_revision" | "has_chunk" | "mention" | "relation") {
        let (relationship, source, destination) = relationship(identity)?;
        return Ok(GraphOp::CreateEdge {
            key: Some(cidentity_key(identity)?),
            source,
            relationship,
            destination,
            properties: cproperties(identity, data)?,
        });
    }
    let expected = if crole(identity)? == "entity" {
        entity_preflight_expected(db, graph, identity, data)?
    } else {
        expected
    };
    cnode(identity, data, expected)
}

fn entity_preflight_expected(
    db: &Database,
    graph_name: &str,
    identity: &codec::Value,
    data: &codec::Value,
) -> Result<Expected, Failure> {
    let key = cidentity_key(identity)?;
    let Some(node) = db
        .graph
        .as_ref()
        .and_then(|state| state.graph_by_name(graph_name))
        .and_then(|graph| graph.node_by_key(&key))
    else {
        return Ok(Expected::Absent);
    };
    let labels = BTreeSet::from([node_label("entity")?.to_owned()]);
    let expected_properties = properties(&cproperties(identity, data)?)?;
    if node.labels != labels || node.properties != expected_properties {
        return Err(Error::InvalidQuery("ingestion identity conflict").into());
    }
    Ok(Expected::Revision(node.revision))
}

fn canonical_operation(
    db: &Database,
    operation: &codec::Value,
    root: &codec::Value,
    batch_ordinal: u64,
    previous_revision: Option<u64>,
) -> Result<Option<GraphOp>, Failure> {
    let tag = cu64(cfield(operation, "tag")?)?;
    let fields = cfield(operation, "fields")?;
    let collection = binding_name(root, "collection")?;
    let graph = binding_name(root, "graph")?;
    let output = match tag {
        0 => None,
        1 => {
            let identity = cfield(fields, "edge")?;
            Some(GraphOp::DeleteEdge {
                edge: EdgeRef::Key(cidentity_key(identity)?),
                expected: cexpected(cfield(fields, "expected")?, previous_revision)?,
            })
        }
        2 => {
            let identity = cfield(fields, "chunk")?;
            let expected = cexpected(cfield(fields, "expected")?, previous_revision)?;
            guard_node(
                db,
                &binding_name(root, "graph")?,
                &cidentity_key(identity)?,
                expected,
            )?;
            Some(GraphOp::Detach {
                node: cidentity_key(identity)?,
                collection,
            })
        }
        3 => Some(GraphOp::Vector {
            collection,
            mutation: Mutation::Delete {
                id: cu64(cfield(fields, "record")?)?,
            },
        }),
        4 => {
            let identity = cfield(fields, "node")?;
            Some(GraphOp::DeleteNode {
                key: cidentity_key(identity)?,
                expected: cexpected(cfield(fields, "expected")?, previous_revision)?,
            })
        }
        5 | 10 | 12 => {
            let object = cfield(fields, "object")?;
            Some(canonical_object(
                db,
                &graph,
                cfield(object, "identity")?,
                cfield(object, "data")?,
                Expected::Absent,
            )?)
        }
        6 => {
            let data = cfield(fields, "data")?;
            let vector = match cfield(data, "vector")? {
                codec::Value::List(values) => values
                    .iter()
                    .map(|value| match value {
                        codec::Value::Float(value) => Ok(*value as f32),
                        _ => Err(Failure::Canonical("float")),
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                _ => return Err(Failure::Canonical("list")),
            };
            let metadata = Metadata::from([
                ("g7_v".into(), MetadataValue::Integer(1)),
                (
                    "g7_tenant".into(),
                    MetadataValue::String(ctext(cfield(data, "tenant")?)?.to_owned()),
                ),
                (
                    "g7_source".into(),
                    MetadataValue::String(ctext(cfield(data, "source_key")?)?.to_owned()),
                ),
                (
                    "g7_revision".into(),
                    MetadataValue::String(ctext(cfield(data, "revision_key")?)?.to_owned()),
                ),
                (
                    "g7_chunk".into(),
                    MetadataValue::String(ctext(cfield(data, "chunk_key")?)?.to_owned()),
                ),
            ]);
            Some(GraphOp::Vector {
                collection,
                mutation: Mutation::PutWithMetadata {
                    id: cu64(cfield(fields, "record")?)?,
                    vector,
                    payload: cbytes(cfield(data, "text")?)?.to_vec(),
                    metadata,
                },
            })
        }
        7 => Some(GraphOp::Attach {
            node: cidentity_key(cfield(fields, "chunk")?)?,
            collection,
            record: cu64(cfield(fields, "record")?)?,
        }),
        8 => {
            let object = cfield(fields, "object")?;
            Some(canonical_object(
                db,
                &graph,
                cfield(object, "identity")?,
                cfield(object, "data")?,
                Expected::Absent,
            )?)
        }
        9 => {
            let shard = cfield(fields, "shard")?;
            let channel = cu64(cfield(shard, "channel")?)?;
            if ctext(cfield(shard, "root_key")?)? != cidentity_key(root)?
                || cu64(cfield(shard, "batch")?)? != batch_ordinal
            {
                return Err(Failure::Canonical("shard_identity"));
            }
            let identity = codec::Value::Map(BTreeMap::from([
                (
                    "kind".into(),
                    codec::Value::Text(if channel == 0 {
                        "result_shard".into()
                    } else {
                        "ownership_shard".into()
                    }),
                ),
                (
                    "fields".into(),
                    codec::Value::Map(BTreeMap::from([
                        ("root".into(), root.clone()),
                        ("batch".into(), codec::Value::Unsigned(batch_ordinal)),
                        ("group".into(), cfield(shard, "group")?.clone()),
                        ("index".into(), cfield(shard, "index")?.clone()),
                    ])),
                ),
            ]));
            Some(cshard_node(&identity, shard)?)
        }
        11 => {
            let identity = cfield(fields, "source")?;
            let key = cidentity_key(identity)?;
            let desired = cproperties(identity, cfield(fields, "data")?)?;
            let graph_name = binding_name(root, "graph")?;
            let current = db
                .graph
                .as_ref()
                .and_then(|state| state.graph_by_name(&graph_name))
                .and_then(|graph| graph.node_by_key(&key))
                .ok_or(Failure::Canonical("missing_head"))?;
            let unset = current
                .properties
                .keys()
                .filter(|name| !desired.contains_key(*name))
                .cloned()
                .collect();
            Some(GraphOp::PatchNode {
                key,
                patch: NodePatch {
                    add_labels: Vec::new(),
                    remove_labels: Vec::new(),
                    set: desired,
                    unset,
                },
                expected: cexpected(cfield(fields, "expected")?, previous_revision)?,
            })
        }
        13 => {
            let identity = cfield(fields, "source")?;
            Some(cnode(identity, cfield(fields, "data")?, Expected::Absent)?)
        }
        _ => return Err(Failure::Canonical("operation_tag")),
    };
    Ok(output)
}

fn guard_node(db: &Database, graph: &str, key: &str, expected: Expected) -> Result<(), Failure> {
    let node = db
        .graph
        .as_ref()
        .and_then(|state| state.graph_by_name(graph))
        .and_then(|graph| graph.node_by_key(key));
    let current = node.map(|node| node.revision);
    let matches = match expected {
        Expected::Any => true,
        Expected::Absent => current.is_none(),
        Expected::Revision(revision) => current == Some(revision),
    };
    if matches {
        Ok(())
    } else {
        Err(Failure::Engine(GraphError::PreconditionFailed { current }))
    }
}

fn lower_canonical_batch(
    db: &Database,
    batch: &codec::Value,
    previous_revision: Option<u64>,
) -> Result<Vec<GraphOp>, Failure> {
    let root = cfield(batch, "root")?;
    let ordinal = cu64(cfield(batch, "ordinal")?)?;
    let graph = binding_name(root, "graph")?;
    let groups = match cfield(batch, "groups")? {
        codec::Value::List(groups) => groups,
        _ => return Err(Failure::Canonical("list")),
    };
    let mut operations = Vec::new();
    for group in groups {
        let items = match cfield(group, "operations")? {
            codec::Value::List(items) => items,
            _ => return Err(Failure::Canonical("list")),
        };
        for item in items {
            let tag = cu64(cfield(item, "tag")?)?;
            let fields = cfield(item, "fields")?;
            if tag == 0 {
                let expected = cexpected(cfield(fields, "expected")?, previous_revision)?;
                guard_node(
                    db,
                    &graph,
                    &cidentity_key(cfield(fields, "source")?)?,
                    expected,
                )?;
                continue;
            }
            if let Some(operation) =
                canonical_operation(db, item, root, ordinal, previous_revision)?
            {
                operations.push(operation);
            }
        }
    }
    Ok(operations)
}

fn preflight_canonical_batch(
    db: &mut Database,
    plan: &codec::Value,
    batch_index: usize,
    previous_revision: Option<u64>,
) -> Result<Admission, Failure> {
    canonical_error(codec::encode(&codec::Type::Record("plan"), plan))?;
    canonical_error(codec::plan::validate_slots(plan))?;
    let batches = match cfield(plan, "batches")? {
        codec::Value::List(batches) => batches,
        _ => return Err(Failure::Canonical("list")),
    };
    let batch = batches
        .get(batch_index)
        .ok_or(Failure::Canonical("batch_index"))?;
    let root = cfield(batch, "root")?;
    let graph = binding_name(root, "graph")?;
    let operations = lower_canonical_batch(db, batch, previous_revision)?;
    preflight_full(db, &graph, &operations)
}

fn properties(metadata: &Metadata) -> Result<Properties, Failure> {
    crate::metadata::validate(metadata)?;
    metadata
        .iter()
        .map(|(key, value)| {
            let value = match value {
                MetadataValue::String(value) => PropertyValue::String(value.clone()),
                MetadataValue::Integer(value) => PropertyValue::Integer(*value),
                MetadataValue::Float(value) => PropertyValue::Float(*value),
                _ => return Err(Error::InvalidQuery("ingestion physical property type").into()),
            };
            Ok((key.clone(), value))
        })
        .collect()
}

fn lower_full(db: &Database, op: &GraphOp) -> Result<Lowered, Failure> {
    let mutation = match op {
        GraphOp::CreateNode {
            key,
            labels,
            properties: values,
        } => GraphMutation::CreateNode {
            key: key.clone(),
            labels: labels.iter().cloned().collect(),
            properties: properties(values)?,
        },
        GraphOp::UpsertNode {
            key,
            labels,
            properties: values,
            expected,
        } => GraphMutation::UpsertNode {
            key: key.clone(),
            labels: labels.iter().cloned().collect(),
            properties: properties(values)?,
            expected: match expected {
                Expected::Any => Precondition::Any,
                Expected::Absent => Precondition::Absent,
                Expected::Revision(revision) => Precondition::Revision(*revision),
            },
        },
        GraphOp::PatchNode {
            key,
            patch,
            expected,
        } => GraphMutation::PatchNode {
            key: key.clone(),
            add_labels: patch.add_labels.iter().cloned().collect(),
            remove_labels: patch.remove_labels.iter().cloned().collect(),
            set: properties(&patch.set)?,
            unset: patch.unset.iter().cloned().collect(),
            expected: match expected {
                Expected::Any => Precondition::Any,
                Expected::Absent => Precondition::Absent,
                Expected::Revision(revision) => Precondition::Revision(*revision),
            },
        },
        GraphOp::DeleteNode { key, expected } => GraphMutation::DeleteNode {
            key: key.clone(),
            expected: match expected {
                Expected::Any => Precondition::Any,
                Expected::Absent => Precondition::Absent,
                Expected::Revision(revision) => Precondition::Revision(*revision),
            },
        },
        GraphOp::CreateEdge {
            key: Some(key),
            source,
            relationship,
            destination,
            properties: values,
        } => GraphMutation::CreateEdge {
            key: Some(key.clone()),
            source: source.clone(),
            relationship: relationship.clone(),
            destination: destination.clone(),
            properties: properties(values)?,
        },
        GraphOp::DeleteEdge { edge, expected } => GraphMutation::DeleteEdge {
            edge: match edge {
                EdgeRef::Id(id) => {
                    EdgeSelector::Id(crate::property_graph::engine::model::EdgeId(*id))
                }
                EdgeRef::Key(key) => EdgeSelector::Key(key.clone()),
            },
            expected: match expected {
                Expected::Any => Precondition::Any,
                Expected::Absent => Precondition::Absent,
                Expected::Revision(revision) => Precondition::Revision(*revision),
            },
        },
        GraphOp::Attach {
            node,
            collection,
            record,
        } => GraphMutation::Attach {
            node: node.clone(),
            collection: db.resolve(collection)?,
            record: *record,
        },
        GraphOp::Detach { node, collection } => GraphMutation::Detach {
            node: node.clone(),
            collection: db.resolve(collection)?,
        },
        GraphOp::Vector {
            collection,
            mutation: mutation @ Mutation::PutWithMetadata { metadata, .. },
        } => {
            properties(metadata)?;
            let collection = db.resolve(collection)?;
            db.validate_batch(collection, std::slice::from_ref(mutation))?;
            let mut stored = mutation.clone();
            if let Mutation::PutWithMetadata { vector, .. } = &mut stored {
                crate::round_stored(db.state(collection).precision, vector)?;
            }
            return Ok(Lowered::Vector(collection, stored));
        }
        GraphOp::Vector {
            collection,
            mutation: mutation @ Mutation::Delete { .. },
        } => {
            let collection = db.resolve(collection)?;
            db.validate_batch(collection, std::slice::from_ref(mutation))?;
            return Ok(Lowered::Vector(collection, mutation.clone()));
        }
        _ => return Err(Error::InvalidQuery("ingestion new-source operation").into()),
    };
    Ok(Lowered::Graph(mutation))
}

fn reserved_body(mut body: GraphBody, now: u64) -> Result<(usize, usize), Failure> {
    if !matches!(body.entries.first(), Some(GraphEntry::Reclaim { .. })) {
        body.entries.insert(0, GraphEntry::Reclaim { at_ms: now });
    }
    let operations = body.entries.len() - 1;
    let bytes = encode_body(&body)?.len();
    Ok((bytes, operations))
}

fn scratch_admission(ops: &[GraphOp]) -> Result<(), Failure> {
    let mut remaining = 64 * 1024 * 1024usize;
    for op in ops {
        let (payload, metadata_bytes, text_bytes) = match op {
            GraphOp::CreateNode {
                key,
                labels,
                properties,
            } => (
                Some(0),
                crate::metadata::validate(properties)?,
                key.len() + labels.iter().map(String::len).sum::<usize>(),
            ),
            GraphOp::UpsertNode {
                key,
                labels,
                properties,
                ..
            } => (
                Some(0),
                crate::metadata::validate(properties)?,
                key.len() + labels.iter().map(String::len).sum::<usize>(),
            ),
            GraphOp::PatchNode { key, patch, .. } => (
                Some(0),
                crate::metadata::validate(&patch.set)?,
                key.len()
                    + patch
                        .add_labels
                        .iter()
                        .chain(&patch.remove_labels)
                        .chain(&patch.unset)
                        .map(String::len)
                        .sum::<usize>(),
            ),
            GraphOp::DeleteNode { key, .. } => (Some(0), 0, key.len()),
            GraphOp::CreateEdge {
                key,
                source,
                relationship,
                destination,
                properties,
            } => (
                Some(0),
                crate::metadata::validate(properties)?,
                key.as_ref().map_or(0, String::len)
                    + source.len()
                    + relationship.len()
                    + destination.len(),
            ),
            GraphOp::DeleteEdge { edge, .. } => (
                Some(0),
                0,
                match edge {
                    EdgeRef::Id(_) => 8,
                    EdgeRef::Key(key) => key.len(),
                },
            ),
            GraphOp::Vector {
                collection,
                mutation:
                    Mutation::PutWithMetadata {
                        vector,
                        payload,
                        metadata,
                        ..
                    },
            } => (
                vector
                    .len()
                    .checked_mul(4)
                    .and_then(|n| n.checked_add(payload.len())),
                crate::metadata::validate(metadata)?,
                collection.len(),
            ),
            GraphOp::Vector {
                collection,
                mutation: Mutation::Delete { .. },
            } => (Some(0), 0, collection.len()),
            GraphOp::Attach {
                node, collection, ..
            } => (Some(0), 0, node.len() + collection.len()),
            GraphOp::Detach { node, collection } => (Some(0), 0, node.len() + collection.len()),
            _ => return Err(Error::InvalidQuery("ingestion new-source operation").into()),
        };
        // Original input, lowered mutations, encoder output and rollback maps.
        // Conservative transient accounting, not part of the canonical schema.
        let charged = payload
            .and_then(|n| n.checked_mul(3))
            .and_then(|n| {
                metadata_bytes
                    .checked_add(text_bytes)
                    .and_then(|m| m.checked_mul(6))
                    .and_then(|m| n.checked_add(m))
            })
            .and_then(|n| n.checked_add(8192))
            .ok_or(Error::InvalidQuery("ingestion document ceiling"))?;
        remaining = remaining
            .checked_sub(charged)
            .ok_or(Error::InvalidQuery("ingestion document ceiling"))?;
    }
    Ok(())
}

fn preflight(db: &mut Database, graph: &str, ops: &[GraphOp]) -> Result<Admission, Failure> {
    preflight_impl(db, graph, ops, false)
}

fn preflight_full(db: &mut Database, graph: &str, ops: &[GraphOp]) -> Result<Admission, Failure> {
    preflight_impl(db, graph, ops, true)
}

fn lower(db: &Database, op: &GraphOp) -> Result<Lowered, Failure> {
    match op {
        GraphOp::UpsertNode {
            expected: Expected::Absent,
            ..
        }
        | GraphOp::CreateEdge { key: Some(_), .. }
        | GraphOp::Attach { .. }
        | GraphOp::Vector {
            mutation: Mutation::PutWithMetadata { .. },
            ..
        } => lower_full(db, op),
        _ => Err(Error::InvalidQuery("ingestion new-source operation").into()),
    }
}

fn preflight_impl(
    db: &mut Database,
    graph: &str,
    ops: &[GraphOp],
    full: bool,
) -> Result<Admission, Failure> {
    if db.read_only {
        return Err(Error::ReadOnly.into());
    }
    if ops.is_empty() || ops.len() > 4096 {
        return Err(Error::BatchTooLarge.into());
    }
    scratch_admission(ops)?;
    let graph_id = db
        .graph
        .as_ref()
        .ok_or(Error::GraphsNotEnabled)?
        .graph_by_name(graph)
        .ok_or(Error::GraphNotFound)?
        .id();
    let lowered = ops
        .iter()
        .map(|op| {
            if full {
                lower_full(db, op)
            } else {
                lower(db, op)
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let sequence = db.next_sequence()?;
    let now = db.graph_clock.max(crate::current_time_ms());
    let collections = &db.collections;
    let mut planner = db
        .graph
        .as_mut()
        .ok_or(Error::GraphsNotEnabled)?
        .planner(sequence, graph_id)?;
    planner.reclaim_expired(now);
    let mut live = BTreeMap::new();
    let mut candidates = BTreeSet::new();
    let mut outcomes = Vec::with_capacity(ops.len());
    for op in lowered {
        let outcome = match op {
            Lowered::Vector(collection, mutation) => {
                let GraphEntry::VectorMutation { entry, .. } =
                    crate::property_graph::frame::vector_entry(collection, &mutation)
                else {
                    unreachable!("vector_entry produces a vector mutation")
                };
                let id = crate::property_graph::frame::mutation_id(&mutation);
                if matches!(mutation, Mutation::PutWithMetadata { .. }) {
                    if collections[&collection].revisions.contains_key(&id)
                        || !candidates.insert((collection, id))
                    {
                        return Err(Error::InvalidQuery("ingestion record id conflict").into());
                    }
                    live.insert((collection, id), true);
                } else {
                    live.insert((collection, id), false);
                }
                planner.vector(collection, entry)?;
                None
            }
            Lowered::Graph(mutation) => {
                if let GraphMutation::Attach {
                    collection, record, ..
                } = &mutation
                    && !live
                        .get(&(*collection, *record))
                        .copied()
                        .unwrap_or_else(|| {
                            collections.get(collection).is_some_and(|target| {
                                target.revisions.contains_key(record)
                                    && !target.expiries.contains_key(record)
                            })
                        })
                {
                    return Err(Error::GraphRecordNotAttachable.into());
                }
                Some(planner.mutate(&mutation)?)
            }
        };
        outcomes.push(outcome);
    }
    let (body_bytes, user_operations) = reserved_body(planner.finish(), now)?;
    Ok(Admission {
        body_bytes,
        user_operations,
        outcomes,
    })
}

#[cfg(test)]
#[path = "preflight/tests.rs"]
mod tests;
