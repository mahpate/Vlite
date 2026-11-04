#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};

use crate::property_graph::GraphOp;
use crate::property_graph::engine::{
    GraphBody, GraphEntry, GraphError, GraphMutation, MutationOutcome, encode_body,
    model::{Precondition, Properties, PropertyValue},
};
use crate::{Database, Error, Expected, Metadata, MetadataValue, Mutation};

#[derive(Debug)]
enum Failure {
    Database(Error),
    Engine(GraphError),
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

fn lower(db: &Database, op: &GraphOp) -> Result<Lowered, Failure> {
    let mutation = match op {
        GraphOp::UpsertNode {
            key,
            labels,
            properties: values,
            expected: Expected::Absent,
        } => GraphMutation::UpsertNode {
            key: key.clone(),
            labels: labels.iter().cloned().collect(),
            properties: properties(values)?,
            expected: Precondition::Absent,
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
        GraphOp::Attach {
            node,
            collection,
            record,
        } => GraphMutation::Attach {
            node: node.clone(),
            collection: db.resolve(collection)?,
            record: *record,
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
            GraphOp::Attach {
                node, collection, ..
            } => (Some(0), 0, node.len() + collection.len()),
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
        .map(|op| lower(db, op))
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
                let id = crate::property_graph::frame::mutation_id(&mutation);
                if collections[&collection].revisions.contains_key(&id)
                    || !candidates.insert((collection, id))
                {
                    return Err(Error::InvalidQuery("ingestion record id conflict").into());
                }
                let GraphEntry::VectorMutation { entry, .. } =
                    crate::property_graph::frame::vector_entry(collection, &mutation)
                else {
                    unreachable!("vector_entry produces a vector mutation")
                };
                planner.vector(collection, entry)?;
                live.insert((collection, id), true);
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
