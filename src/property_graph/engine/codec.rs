//! Graph frame body codec (DO-003).
//!
//! This module encodes and decodes the sealed body of an op-12 graph frame.
//! The plaintext frame prefix, sealing, commit footer and the meaning of
//! vector-mutation bytes belong to the `vlite` adapter. All integers are
//! little-endian.
//!
//! Body: `body_version u8 = 1`, `reserved u8 = 0`, `graph_id u16` (< 64),
//! then exactly `count` entries, where `count` comes from the frame prefix.
//!
//! Entry envelope: `tag u8`, `flags u8 = 0`, `reserved u16 = 0`,
//! `payload_len u32`, `payload`. Every payload must be consumed exactly.
//! Label, field-name and key lists are canonical: strictly ascending byte
//! order without duplicates, so `encode(decode(bytes)) == bytes`.

use super::error::GraphError;
use super::limits::{
    MAX_BATCH_BYTES, MAX_BATCH_ENTRIES, MAX_GRAPHS, MAX_LABELS, MAX_PROPERTY_BYTES,
};
use super::model::{
    EdgeId, GraphId, NodeId, Properties, validate_edge_key, validate_graph_name, validate_label,
    validate_labels, validate_node_key, validate_relationship_type,
};
use super::properties;
use super::schema::{self, GraphSchema};
use std::collections::BTreeSet;

/// DO-001: V5 header flag marking a graph-enabled file.
pub const FLAG_GRAPH: u16 = 4;
/// DO-003: frame operation tag of a graph frame.
pub const OP_GRAPH: u8 = 12;
/// DO-003: body layout version.
pub const BODY_VERSION: u8 = 1;
/// Bytes before the first entry.
pub const BODY_HEADER_BYTES: usize = 4;
/// Bytes of the entry envelope before the payload.
pub const ENTRY_HEADER_BYTES: usize = 8;

/// Entry tags (closed set). Tag 12 carries a declared schema (EPIC-008; layout
/// in [`super::schema`]). Tags 13..=15 (Gap 3) appear only in files that use
/// graph expiry.
pub mod tag {
    pub const CREATE_GRAPH: u8 = 1;
    pub const DROP_GRAPH: u8 = 2;
    pub const GRAPH_HIGH_WATER: u8 = 3;
    pub const PUT_NODE: u8 = 4;
    pub const PATCH_NODE: u8 = 5;
    pub const DELETE_NODE: u8 = 6;
    pub const PUT_EDGE: u8 = 7;
    pub const PATCH_EDGE: u8 = 8;
    pub const DELETE_EDGE: u8 = 9;
    pub const ATTACH: u8 = 10;
    pub const DETACH: u8 = 11;
    pub const PUT_SCHEMA: u8 = 12;
    pub const NODE_EXPIRY: u8 = 13;
    pub const EDGE_EXPIRY: u8 = 14;
    pub const RECLAIM: u8 = 15;
    pub const VECTOR_MUTATION: u8 = 64;
}

/// One entry of a graph batch. Graph entries apply to the body's graph.
#[derive(Clone, Debug, PartialEq)]
pub enum GraphEntry {
    CreateGraph {
        name: String,
    },
    /// Allowed only when the graph has no nodes and no edges.
    DropGraph,
    /// Raises the next node/edge IDs (never lowers them).
    GraphHighWater {
        next_node_id: u64,
        next_edge_id: u64,
    },
    /// Creates a node or fully replaces labels and properties of the node with
    /// the same ID and key.
    PutNode {
        node_id: NodeId,
        key: String,
        labels: BTreeSet<String>,
        properties: Properties,
    },
    /// Adds/removes labels and sets/unsets properties. Adding a present label
    /// or removing/unsetting an absent one is a no-op.
    PatchNode {
        node_id: NodeId,
        add_labels: BTreeSet<String>,
        remove_labels: BTreeSet<String>,
        set: Properties,
        unset: BTreeSet<String>,
    },
    DeleteNode {
        node_id: NodeId,
    },
    PutEdge {
        edge_id: EdgeId,
        source: NodeId,
        destination: NodeId,
        relationship: String,
        key: Option<String>,
        properties: Properties,
    },
    PatchEdge {
        edge_id: EdgeId,
        set: Properties,
        unset: BTreeSet<String>,
    },
    DeleteEdge {
        edge_id: EdgeId,
    },
    Attach {
        node_id: NodeId,
        collection: u16,
        record_id: u64,
    },
    Detach {
        node_id: NodeId,
        collection: u16,
    },
    /// Replaces the graph's declared schema; an empty schema removes it.
    PutSchema {
        schema: GraphSchema,
    },
    /// Sets (`Some`) or clears (`None`) a live node's deadline, in ms
    /// since the Unix epoch. Payload: `node_id u64`, then `deadline u64` only
    /// when set.
    NodeExpiry {
        node_id: NodeId,
        expires_at_ms: Option<u64>,
    },
    /// Sets or clears a live edge's own deadline; same layout.
    EdgeExpiry {
        edge_id: EdgeId,
        expires_at_ms: Option<u64>,
    },
    /// Removes every node whose deadline is at or before `at_ms` with
    /// all its edges and attachments, then every edge whose own deadline is
    /// at or before `at_ms`. Allowed as the first entry of a body and
    /// directly after an expiry entry due at `at_ms`; all reclaims of a body
    /// share one `at_ms`.
    Reclaim {
        at_ms: u64,
    },
    /// One `encode_batch_entries` entry for `collection`, carried verbatim.
    /// Only its envelope is checked here; the adapter validates its content.
    VectorMutation {
        collection: u16,
        entry: Vec<u8>,
    },
}

impl GraphEntry {
    /// The entry tag.
    pub fn tag(&self) -> u8 {
        match self {
            Self::CreateGraph { .. } => tag::CREATE_GRAPH,
            Self::DropGraph => tag::DROP_GRAPH,
            Self::GraphHighWater { .. } => tag::GRAPH_HIGH_WATER,
            Self::PutNode { .. } => tag::PUT_NODE,
            Self::PatchNode { .. } => tag::PATCH_NODE,
            Self::DeleteNode { .. } => tag::DELETE_NODE,
            Self::PutEdge { .. } => tag::PUT_EDGE,
            Self::PatchEdge { .. } => tag::PATCH_EDGE,
            Self::DeleteEdge { .. } => tag::DELETE_EDGE,
            Self::Attach { .. } => tag::ATTACH,
            Self::Detach { .. } => tag::DETACH,
            Self::PutSchema { .. } => tag::PUT_SCHEMA,
            Self::NodeExpiry { .. } => tag::NODE_EXPIRY,
            Self::EdgeExpiry { .. } => tag::EDGE_EXPIRY,
            Self::Reclaim { .. } => tag::RECLAIM,
            Self::VectorMutation { .. } => tag::VECTOR_MUTATION,
        }
    }

    /// `(collection, record ID)` touched by a `VectorMutation` whose envelope
    /// has been validated; `None` for every graph entry.
    pub(crate) fn vector_record(&self) -> Option<(u16, u64)> {
        match self {
            Self::VectorMutation { collection, entry } => {
                let id = entry.get(4..12)?.try_into().ok()?;
                Some((*collection, u64::from_le_bytes(id)))
            }
            _ => None,
        }
    }

    /// Checks every rule that does not depend on state.
    pub fn validate(&self) -> Result<(), GraphError> {
        match self {
            Self::CreateGraph { name } => validate_graph_name(name),
            Self::DropGraph
            | Self::GraphHighWater { .. }
            | Self::DeleteNode { .. }
            | Self::DeleteEdge { .. }
            | Self::Attach { .. }
            | Self::Detach { .. }
            | Self::NodeExpiry { .. }
            | Self::EdgeExpiry { .. }
            | Self::Reclaim { .. } => Ok(()),
            Self::PutNode {
                key,
                labels,
                properties,
                ..
            } => {
                validate_node_key(key)?;
                validate_labels(labels)?;
                properties::validate(properties).map(drop)
            }
            Self::PatchNode {
                add_labels,
                remove_labels,
                set,
                unset,
                ..
            } => {
                validate_labels(add_labels)?;
                validate_labels(remove_labels)?;
                if !add_labels.is_disjoint(remove_labels) {
                    return Err(GraphError::InvalidLabel);
                }
                validate_patch(set, unset)
            }
            Self::PutEdge {
                relationship,
                key,
                properties,
                ..
            } => {
                validate_relationship_type(relationship)?;
                if let Some(key) = key {
                    validate_edge_key(key)?;
                }
                properties::validate(properties).map(drop)
            }
            Self::PatchEdge { set, unset, .. } => validate_patch(set, unset),
            Self::PutSchema { schema } => schema.validate(),
            Self::VectorMutation { entry, .. } => validate_vector_entry(entry),
        }
    }
}

fn validate_patch(set: &Properties, unset: &BTreeSet<String>) -> Result<(), GraphError> {
    properties::validate(set)?;
    if unset.len() > super::limits::MAX_PROPERTY_FIELDS {
        return Err(GraphError::InvalidProperties("unset exceeds 64 fields"));
    }
    unset
        .iter()
        .try_for_each(|name| properties::validate_field_name(name))?;
    if unset.iter().any(|name| set.contains_key(name)) {
        return Err(GraphError::InvalidProperties(
            "a field cannot be both set and unset",
        ));
    }
    Ok(())
}

/// Envelope of one `encode_batch_entries` entry (`src\format.rs`):
/// `op u8`, `reserved [0; 3]`, `id u64`, `len u32`, then for
/// Put (1): `dimensions × f32` and `len` payload bytes; Delete (2): nothing
/// and `len = 0`; PutWithMetadata (4): `len` body bytes.
fn validate_vector_entry(entry: &[u8]) -> Result<(), GraphError> {
    let Some((header, body)) = entry.split_at_checked(16) else {
        return Err(GraphError::InvalidVectorMutation("truncated batch entry"));
    };
    if header[1..4] != [0, 0, 0] {
        return Err(GraphError::InvalidVectorMutation("reserved bytes"));
    }
    let len = u32::from_le_bytes([header[12], header[13], header[14], header[15]]) as usize;
    let valid = match header[0] {
        1 => body.len() > len && (body.len() - len).is_multiple_of(4),
        2 => len == 0 && body.is_empty(),
        4 => body.len() == len,
        _ => return Err(GraphError::InvalidVectorMutation("unknown mutation op")),
    };
    if valid {
        Ok(())
    } else {
        Err(GraphError::InvalidVectorMutation("length mismatch"))
    }
}

/// A decoded graph frame body.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphBody {
    pub graph_id: GraphId,
    pub entries: Vec<GraphEntry>,
}

impl GraphBody {
    /// Checks the body-level rules that do not depend on state.
    pub fn validate(&self) -> Result<(), GraphError> {
        if self.graph_id.0 >= MAX_GRAPHS {
            return Err(GraphError::GraphIdUnavailable(self.graph_id));
        }
        if self.entries.is_empty() {
            return Err(GraphError::BatchSize(
                "a graph batch has at least one entry",
            ));
        }
        check_entries(&self.entries)?;
        self.entries.iter().try_for_each(GraphEntry::validate)
    }
}

/// Entries allowed in a body: [`MAX_BATCH_ENTRIES`] counted entries plus
/// the reclaims, which do not count. One reclaim may open the body and one
/// may follow each expiry entry, so reclaiming never shrinks the caller's
/// batch; reclaim bytes do count against [`MAX_BATCH_BYTES`].
pub const MAX_BODY_ENTRIES: usize = 2 * MAX_BATCH_ENTRIES + 1;

/// At most [`MAX_BATCH_ENTRIES`] entries besides reclaims, and each reclaim
/// opens the body or directly follows an expiry entry due at its clock, all
/// at one clock (see [`GraphEntry::Reclaim`]).
fn check_entries(entries: &[GraphEntry]) -> Result<(), GraphError> {
    let reclaim = |entry: &GraphEntry| match entry {
        GraphEntry::Reclaim { at_ms } => Some(*at_ms),
        _ => None,
    };
    let reclaims = entries.iter().filter_map(reclaim).count();
    if entries.len() - reclaims > MAX_BATCH_ENTRIES {
        return Err(GraphError::BatchSize("more than 4096 entries"));
    }
    let mut clock = None;
    for (index, at_ms) in entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| Some((index, reclaim(entry)?)))
    {
        let placed = match index.checked_sub(1).map(|before| &entries[before]) {
            None => true,
            Some(
                GraphEntry::NodeExpiry {
                    expires_at_ms: Some(deadline),
                    ..
                }
                | GraphEntry::EdgeExpiry {
                    expires_at_ms: Some(deadline),
                    ..
                },
            ) => *deadline <= at_ms,
            Some(_) => false,
        };
        if !placed || *clock.get_or_insert(at_ms) != at_ms {
            return Err(GraphError::Corrupt("misplaced reclaim"));
        }
    }
    Ok(())
}

fn put_labels(labels: &BTreeSet<String>, bytes: &mut Vec<u8>) {
    bytes.push(labels.len() as u8);
    for label in labels {
        bytes.push(label.len() as u8);
        bytes.extend_from_slice(label.as_bytes());
    }
}

fn put_properties(properties: &Properties, bytes: &mut Vec<u8>) {
    let start = bytes.len();
    bytes.extend_from_slice(&[0; 4]);
    properties::encode_into(properties, bytes);
    let len = (bytes.len() - start - 4) as u32;
    bytes[start..start + 4].copy_from_slice(&len.to_le_bytes());
}

fn put_field_names(names: &BTreeSet<String>, bytes: &mut Vec<u8>) {
    bytes.push(names.len() as u8);
    for name in names {
        bytes.extend_from_slice(&(name.len() as u16).to_le_bytes());
        bytes.extend_from_slice(name.as_bytes());
    }
}

fn encode_payload(entry: &GraphEntry, bytes: &mut Vec<u8>) {
    match entry {
        GraphEntry::CreateGraph { name } => {
            bytes.push(name.len() as u8);
            bytes.extend_from_slice(name.as_bytes());
        }
        GraphEntry::DropGraph => {}
        GraphEntry::GraphHighWater {
            next_node_id,
            next_edge_id,
        } => {
            bytes.extend_from_slice(&next_node_id.to_le_bytes());
            bytes.extend_from_slice(&next_edge_id.to_le_bytes());
        }
        GraphEntry::PutNode {
            node_id,
            key,
            labels,
            properties,
        } => {
            bytes.extend_from_slice(&node_id.0.to_le_bytes());
            bytes.extend_from_slice(&(key.len() as u16).to_le_bytes());
            bytes.extend_from_slice(key.as_bytes());
            put_labels(labels, bytes);
            put_properties(properties, bytes);
        }
        GraphEntry::PatchNode {
            node_id,
            add_labels,
            remove_labels,
            set,
            unset,
        } => {
            bytes.extend_from_slice(&node_id.0.to_le_bytes());
            put_labels(add_labels, bytes);
            put_labels(remove_labels, bytes);
            put_properties(set, bytes);
            put_field_names(unset, bytes);
        }
        GraphEntry::DeleteNode { node_id } => bytes.extend_from_slice(&node_id.0.to_le_bytes()),
        GraphEntry::PutEdge {
            edge_id,
            source,
            destination,
            relationship,
            key,
            properties,
        } => {
            bytes.extend_from_slice(&edge_id.0.to_le_bytes());
            bytes.extend_from_slice(&source.0.to_le_bytes());
            bytes.extend_from_slice(&destination.0.to_le_bytes());
            bytes.push(relationship.len() as u8);
            bytes.extend_from_slice(relationship.as_bytes());
            let key = key.as_deref().unwrap_or("");
            bytes.extend_from_slice(&(key.len() as u16).to_le_bytes());
            bytes.extend_from_slice(key.as_bytes());
            put_properties(properties, bytes);
        }
        GraphEntry::PatchEdge {
            edge_id,
            set,
            unset,
        } => {
            bytes.extend_from_slice(&edge_id.0.to_le_bytes());
            put_properties(set, bytes);
            put_field_names(unset, bytes);
        }
        GraphEntry::DeleteEdge { edge_id } => bytes.extend_from_slice(&edge_id.0.to_le_bytes()),
        GraphEntry::Attach {
            node_id,
            collection,
            record_id,
        } => {
            bytes.extend_from_slice(&node_id.0.to_le_bytes());
            bytes.extend_from_slice(&collection.to_le_bytes());
            bytes.extend_from_slice(&[0, 0]);
            bytes.extend_from_slice(&record_id.to_le_bytes());
        }
        GraphEntry::Detach {
            node_id,
            collection,
        } => {
            bytes.extend_from_slice(&node_id.0.to_le_bytes());
            bytes.extend_from_slice(&collection.to_le_bytes());
            bytes.extend_from_slice(&[0, 0]);
        }
        GraphEntry::PutSchema { schema } => schema.encode_into(bytes),
        GraphEntry::NodeExpiry {
            node_id: NodeId(id),
            expires_at_ms,
        }
        | GraphEntry::EdgeExpiry {
            edge_id: EdgeId(id),
            expires_at_ms,
        } => {
            bytes.extend_from_slice(&id.to_le_bytes());
            if let Some(deadline) = expires_at_ms {
                bytes.extend_from_slice(&deadline.to_le_bytes());
            }
        }
        GraphEntry::Reclaim { at_ms } => bytes.extend_from_slice(&at_ms.to_le_bytes()),
        GraphEntry::VectorMutation { collection, entry } => {
            bytes.extend_from_slice(&collection.to_le_bytes());
            bytes.extend_from_slice(&[0, 0]);
            bytes.extend_from_slice(entry);
        }
    }
}

fn labels_len(labels: &BTreeSet<String>) -> usize {
    1 + labels.iter().map(|label| 1 + label.len()).sum::<usize>()
}

fn properties_len(properties: &Properties) -> Result<usize, GraphError> {
    properties::validate(properties).map(|len| 4 + len)
}

fn field_names_len(names: &BTreeSet<String>) -> usize {
    1 + names.iter().map(|name| 2 + name.len()).sum::<usize>()
}

/// Exact payload length of a validated entry, computed without encoding it.
/// Only `VectorMutation` is unbounded; every other payload is bounded by the
/// field limits.
fn payload_len(entry: &GraphEntry) -> Result<usize, GraphError> {
    Ok(match entry {
        GraphEntry::CreateGraph { name } => 1 + name.len(),
        GraphEntry::DropGraph => 0,
        GraphEntry::GraphHighWater { .. } => 16,
        GraphEntry::PutNode {
            key,
            labels,
            properties,
            ..
        } => 8 + 2 + key.len() + labels_len(labels) + properties_len(properties)?,
        GraphEntry::PatchNode {
            add_labels,
            remove_labels,
            set,
            unset,
            ..
        } => {
            8 + labels_len(add_labels)
                + labels_len(remove_labels)
                + properties_len(set)?
                + field_names_len(unset)
        }
        GraphEntry::DeleteNode { .. } | GraphEntry::DeleteEdge { .. } => 8,
        GraphEntry::PutEdge {
            relationship,
            key,
            properties,
            ..
        } => {
            24 + 1
                + relationship.len()
                + 2
                + key.as_ref().map_or(0, String::len)
                + properties_len(properties)?
        }
        GraphEntry::PatchEdge { set, unset, .. } => {
            8 + properties_len(set)? + field_names_len(unset)
        }
        GraphEntry::Attach { .. } => 20,
        GraphEntry::Detach { .. } => 12,
        GraphEntry::PutSchema { schema } => schema.encoded_len(),
        GraphEntry::NodeExpiry { expires_at_ms, .. }
        | GraphEntry::EdgeExpiry { expires_at_ms, .. } => {
            8 + if expires_at_ms.is_some() { 8 } else { 0 }
        }
        GraphEntry::Reclaim { .. } => 8,
        GraphEntry::VectorMutation { entry, .. } => entry
            .len()
            .checked_add(4)
            .ok_or(GraphError::BatchSize("more than 16777216 bytes"))?,
    })
}

/// Returns the body length after appending one entry with a payload of
/// `payload_len` bytes to a body of `used` bytes, or `BatchSize` when that
/// overflows or exceeds [`MAX_BATCH_BYTES`].
fn checked_entry_end(used: usize, payload_len: usize) -> Result<usize, GraphError> {
    used.checked_add(ENTRY_HEADER_BYTES)
        .and_then(|len| len.checked_add(payload_len))
        .filter(|&len| len <= MAX_BATCH_BYTES)
        .ok_or(GraphError::BatchSize("more than 16777216 bytes"))
}

/// Exact encoded length of a validated body, checked against
/// [`MAX_BATCH_BYTES`] before anything is allocated or copied.
fn encoded_body_len(body: &GraphBody) -> Result<usize, GraphError> {
    body.entries
        .iter()
        .try_fold(BODY_HEADER_BYTES, |used, entry| {
            checked_entry_end(used, payload_len(entry)?)
        })
}

/// Validates and encodes `body`. The entry count for the frame prefix is
/// `body.entries.len()`. The encoded length is computed and checked against
/// [`MAX_BATCH_BYTES`] before the output is allocated, so an oversized
/// vector payload is never copied.
pub fn encode_body(body: &GraphBody) -> Result<Vec<u8>, GraphError> {
    body.validate()?;
    let total = encoded_body_len(body)?;
    let mut bytes = Vec::with_capacity(total);
    bytes.extend_from_slice(&[BODY_VERSION, 0]);
    bytes.extend_from_slice(&body.graph_id.0.to_le_bytes());
    for entry in &body.entries {
        let start = bytes.len();
        bytes.extend_from_slice(&[entry.tag(), 0, 0, 0, 0, 0, 0, 0]);
        encode_payload(entry, &mut bytes);
        // Cannot fail: the total was checked to be at most 16 MiB.
        let len = (bytes.len() - start - ENTRY_HEADER_BYTES) as u32;
        bytes[start + 4..start + 8].copy_from_slice(&len.to_le_bytes());
    }
    debug_assert_eq!(bytes.len(), total);
    Ok(bytes)
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], GraphError> {
        let (value, rest) = self
            .bytes
            .split_at_checked(n)
            .ok_or(GraphError::Corrupt("truncated entry payload"))?;
        self.bytes = rest;
        Ok(value)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], GraphError> {
        let mut value = [0; N];
        value.copy_from_slice(self.take(N)?);
        Ok(value)
    }
    fn u8(&mut self) -> Result<u8, GraphError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, GraphError> {
        self.array().map(u16::from_le_bytes)
    }
    fn u32(&mut self) -> Result<u32, GraphError> {
        self.array().map(u32::from_le_bytes)
    }
    fn u64(&mut self) -> Result<u64, GraphError> {
        self.array().map(u64::from_le_bytes)
    }
    fn reserved_u16(&mut self) -> Result<(), GraphError> {
        if self.u16()? == 0 {
            Ok(())
        } else {
            Err(GraphError::Corrupt("non-zero reserved bytes"))
        }
    }
    /// An optional trailing `u64`: absent when the payload ends here.
    fn deadline(&mut self) -> Result<Option<u64>, GraphError> {
        if self.bytes.is_empty() {
            Ok(None)
        } else {
            self.u64().map(Some)
        }
    }
    fn string(&mut self, len: usize) -> Result<String, GraphError> {
        std::str::from_utf8(self.take(len)?)
            .map(str::to_owned)
            .map_err(|_| GraphError::Corrupt("invalid UTF-8"))
    }
    /// `count u8`, then `count × (len u8, label)`, strictly ascending.
    fn labels(&mut self) -> Result<BTreeSet<String>, GraphError> {
        let count = self.u8()? as usize;
        if count > MAX_LABELS {
            return Err(GraphError::TooManyLabels);
        }
        let mut labels = BTreeSet::new();
        for _ in 0..count {
            let len = self.u8()? as usize;
            let label = self.string(len)?;
            validate_label(&label)?;
            if labels.last().is_some_and(|last: &String| *last >= label) {
                return Err(GraphError::Corrupt("labels must be strictly ascending"));
            }
            labels.insert(label);
        }
        Ok(labels)
    }
    /// `len u32` in `4..=65536`, then the properties encoding.
    fn properties(&mut self) -> Result<Properties, GraphError> {
        let len = self.u32()? as usize;
        if !(4..=MAX_PROPERTY_BYTES).contains(&len) {
            return Err(GraphError::Corrupt("property length out of range"));
        }
        properties::decode(self.take(len)?)
    }
    /// `count u8`, then `count × (len u16, name)`, strictly ascending.
    fn field_names(&mut self) -> Result<BTreeSet<String>, GraphError> {
        let count = self.u8()? as usize;
        let mut names = BTreeSet::new();
        for _ in 0..count {
            let len = self.u16()? as usize;
            let name = self.string(len)?;
            if names.last().is_some_and(|last: &String| *last >= name) {
                return Err(GraphError::Corrupt(
                    "field names must be strictly ascending",
                ));
            }
            names.insert(name);
        }
        Ok(names)
    }
}

fn decode_payload(tag: u8, payload: &[u8]) -> Result<GraphEntry, GraphError> {
    let mut r = Reader { bytes: payload };
    let entry = match tag {
        tag::CREATE_GRAPH => {
            let len = r.u8()? as usize;
            GraphEntry::CreateGraph {
                name: r.string(len)?,
            }
        }
        tag::DROP_GRAPH => GraphEntry::DropGraph,
        tag::GRAPH_HIGH_WATER => GraphEntry::GraphHighWater {
            next_node_id: r.u64()?,
            next_edge_id: r.u64()?,
        },
        tag::PUT_NODE => {
            let node_id = NodeId(r.u64()?);
            let len = r.u16()? as usize;
            GraphEntry::PutNode {
                node_id,
                key: r.string(len)?,
                labels: r.labels()?,
                properties: r.properties()?,
            }
        }
        tag::PATCH_NODE => GraphEntry::PatchNode {
            node_id: NodeId(r.u64()?),
            add_labels: r.labels()?,
            remove_labels: r.labels()?,
            set: r.properties()?,
            unset: r.field_names()?,
        },
        tag::DELETE_NODE => GraphEntry::DeleteNode {
            node_id: NodeId(r.u64()?),
        },
        tag::PUT_EDGE => {
            let edge_id = EdgeId(r.u64()?);
            let source = NodeId(r.u64()?);
            let destination = NodeId(r.u64()?);
            let len = r.u8()? as usize;
            let relationship = r.string(len)?;
            let len = r.u16()? as usize;
            let key = if len == 0 { None } else { Some(r.string(len)?) };
            GraphEntry::PutEdge {
                edge_id,
                source,
                destination,
                relationship,
                key,
                properties: r.properties()?,
            }
        }
        tag::PATCH_EDGE => GraphEntry::PatchEdge {
            edge_id: EdgeId(r.u64()?),
            set: r.properties()?,
            unset: r.field_names()?,
        },
        tag::DELETE_EDGE => GraphEntry::DeleteEdge {
            edge_id: EdgeId(r.u64()?),
        },
        tag::ATTACH => {
            let node_id = NodeId(r.u64()?);
            let collection = r.u16()?;
            r.reserved_u16()?;
            GraphEntry::Attach {
                node_id,
                collection,
                record_id: r.u64()?,
            }
        }
        tag::DETACH => {
            let node_id = NodeId(r.u64()?);
            let collection = r.u16()?;
            r.reserved_u16()?;
            GraphEntry::Detach {
                node_id,
                collection,
            }
        }
        tag::PUT_SCHEMA => GraphEntry::PutSchema {
            schema: schema::decode(r.take(r.bytes.len())?)?,
        },
        tag::NODE_EXPIRY => GraphEntry::NodeExpiry {
            node_id: NodeId(r.u64()?),
            expires_at_ms: r.deadline()?,
        },
        tag::EDGE_EXPIRY => GraphEntry::EdgeExpiry {
            edge_id: EdgeId(r.u64()?),
            expires_at_ms: r.deadline()?,
        },
        tag::RECLAIM => GraphEntry::Reclaim { at_ms: r.u64()? },
        tag::VECTOR_MUTATION => {
            let collection = r.u16()?;
            r.reserved_u16()?;
            let entry = r.take(r.bytes.len())?.to_vec();
            GraphEntry::VectorMutation { collection, entry }
        }
        other => return Err(GraphError::UnknownEntryTag(other)),
    };
    if !r.bytes.is_empty() {
        return Err(GraphError::Corrupt("trailing entry payload bytes"));
    }
    entry.validate()?;
    Ok(entry)
}

/// Decodes a complete graph frame body holding exactly `count` entries
/// (`count` is the frame prefix field). Any error means the body is invalid;
/// the adapter reports it as corruption for committed history.
pub fn decode_body(bytes: &[u8], count: u32) -> Result<GraphBody, GraphError> {
    if bytes.len() > MAX_BATCH_BYTES {
        return Err(GraphError::BatchSize("more than 16777216 bytes"));
    }
    let count = count as usize;
    if count == 0 {
        return Err(GraphError::BatchSize(
            "a graph batch has at least one entry",
        ));
    }
    if count > MAX_BODY_ENTRIES {
        return Err(GraphError::BatchSize("more than 4096 entries"));
    }
    let Some((header, mut rest)) = bytes.split_at_checked(BODY_HEADER_BYTES) else {
        return Err(GraphError::Corrupt("truncated body header"));
    };
    if header[0] != BODY_VERSION {
        return Err(GraphError::Corrupt("unsupported body version"));
    }
    if header[1] != 0 {
        return Err(GraphError::Corrupt("non-zero reserved bytes"));
    }
    let graph_id = GraphId(u16::from_le_bytes([header[2], header[3]]));
    if graph_id.0 >= MAX_GRAPHS {
        return Err(GraphError::GraphIdUnavailable(graph_id));
    }
    let mut entries = Vec::with_capacity(count);
    while !rest.is_empty() {
        if entries.len() == count {
            return Err(GraphError::Corrupt("more entries than the frame count"));
        }
        let Some((envelope, after)) = rest.split_at_checked(ENTRY_HEADER_BYTES) else {
            return Err(GraphError::Corrupt("truncated entry header"));
        };
        if envelope[1] != 0 {
            return Err(GraphError::Corrupt("non-zero entry flags"));
        }
        if envelope[2..4] != [0, 0] {
            return Err(GraphError::Corrupt("non-zero reserved bytes"));
        }
        let len = u32::from_le_bytes([envelope[4], envelope[5], envelope[6], envelope[7]]);
        let Some((payload, after)) = after.split_at_checked(len as usize) else {
            return Err(GraphError::Corrupt("truncated entry payload"));
        };
        entries.push(decode_payload(envelope[0], payload)?);
        rest = after;
    }
    if entries.len() != count {
        return Err(GraphError::Corrupt("fewer entries than the frame count"));
    }
    check_entries(&entries)?;
    Ok(GraphBody { graph_id, entries })
}

#[cfg(test)]
mod tests {
    use super::super::model::PropertyValue;
    use super::*;

    fn labels(values: &[&str]) -> BTreeSet<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn props() -> Properties {
        Properties::from([
            ("source_ref".into(), PropertyValue::String("doc-1".into())),
            ("weight".into(), PropertyValue::Integer(7)),
        ])
    }

    fn delete_entry(id: u64) -> Vec<u8> {
        let mut bytes = vec![2, 0, 0, 0];
        bytes.extend_from_slice(&id.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes
    }

    fn put_entry(id: u64, vector: &[f32], payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![1, 0, 0, 0];
        bytes.extend_from_slice(&id.to_le_bytes());
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        for value in vector {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(payload);
        bytes
    }

    /// One entry of every closed tag.
    fn every_entry() -> Vec<GraphEntry> {
        vec![
            // Gap 3: a reclaim may open a body.
            GraphEntry::Reclaim {
                at_ms: 1_700_000_000_000,
            },
            GraphEntry::CreateGraph { name: "kg".into() },
            GraphEntry::GraphHighWater {
                next_node_id: 10,
                next_edge_id: 20,
            },
            GraphEntry::PutNode {
                node_id: NodeId(10),
                key: "alice".into(),
                labels: labels(&["Person", "Author"]),
                properties: props(),
            },
            GraphEntry::PatchNode {
                node_id: NodeId(10),
                add_labels: labels(&["Reviewer"]),
                remove_labels: labels(&["Author"]),
                set: Properties::from([("age".into(), PropertyValue::Integer(30))]),
                unset: labels(&["weight"]),
            },
            GraphEntry::PutEdge {
                edge_id: EdgeId(20),
                source: NodeId(10),
                destination: NodeId(10),
                relationship: "KNOWS".into(),
                key: Some("evidence-1".into()),
                properties: props(),
            },
            GraphEntry::PutEdge {
                edge_id: EdgeId(21),
                source: NodeId(10),
                destination: NodeId(10),
                relationship: "KNOWS".into(),
                key: None,
                properties: Properties::new(),
            },
            GraphEntry::PatchEdge {
                edge_id: EdgeId(20),
                set: Properties::from([("ok".into(), PropertyValue::Boolean(true))]),
                unset: labels(&["weight"]),
            },
            GraphEntry::NodeExpiry {
                node_id: NodeId(10),
                expires_at_ms: Some(u64::MAX),
            },
            GraphEntry::NodeExpiry {
                node_id: NodeId(10),
                expires_at_ms: None,
            },
            GraphEntry::EdgeExpiry {
                edge_id: EdgeId(21),
                expires_at_ms: Some(0),
            },
            GraphEntry::EdgeExpiry {
                edge_id: EdgeId(21),
                expires_at_ms: None,
            },
            GraphEntry::Attach {
                node_id: NodeId(10),
                collection: 3,
                record_id: 99,
            },
            GraphEntry::VectorMutation {
                collection: 3,
                entry: put_entry(99, &[1.0, 2.0], b"p"),
            },
            GraphEntry::VectorMutation {
                collection: 0,
                entry: delete_entry(5),
            },
            GraphEntry::Detach {
                node_id: NodeId(10),
                collection: 3,
            },
            GraphEntry::DeleteEdge {
                edge_id: EdgeId(20),
            },
            GraphEntry::DeleteNode {
                node_id: NodeId(11),
            },
            GraphEntry::DropGraph,
        ]
    }

    fn body(entries: Vec<GraphEntry>) -> GraphBody {
        GraphBody {
            graph_id: GraphId(1),
            entries,
        }
    }

    fn encode_one(entry: GraphEntry) -> Vec<u8> {
        encode_body(&body(vec![entry])).unwrap()
    }

    fn decode_one(bytes: &[u8]) -> Result<GraphBody, GraphError> {
        decode_body(bytes, 1)
    }

    /// Re-frames a hand-written payload with a correct envelope.
    fn raw(tag: u8, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![BODY_VERSION, 0, 1, 0, tag, 0, 0, 0];
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn decided_constants() {
        assert_eq!((FLAG_GRAPH, OP_GRAPH, BODY_VERSION), (4, 12, 1));
    }

    /// Byte-identical to `graph_body(1, &[create_graph_entry("g")])` in
    /// `tests\graph_compat.rs`, whose fixture is committed and cross-checked
    /// against the Python encoder.
    #[test]
    fn create_graph_matches_the_tested_compat_fixture_body() {
        let encoded = encode_one(GraphEntry::CreateGraph { name: "g".into() });
        assert_eq!(encoded, [1, 0, 1, 0, 1, 0, 0, 0, 2, 0, 0, 0, 1, b'g']);
        assert_eq!(
            decode_one(&encoded).unwrap(),
            body(vec![GraphEntry::CreateGraph { name: "g".into() }])
        );
    }

    /// Mirrors `do003_vector_mutation_reuses_the_existing_batch_entry_bytes`.
    #[test]
    fn vector_mutation_envelope_matches_the_tested_layout() {
        let entry = delete_entry(7);
        let encoded = encode_one(GraphEntry::VectorMutation {
            collection: 2,
            entry: entry.clone(),
        });
        let mut expected = vec![1, 0, 1, 0, 64, 0, 0, 0, 20, 0, 0, 0, 2, 0, 0, 0];
        expected.extend_from_slice(&entry);
        assert_eq!(encoded, expected);
    }

    #[test]
    fn golden_bytes_for_every_draft_tag() {
        let p = |entry| encode_one(entry)[BODY_HEADER_BYTES..].to_vec();
        let empty = [0u8, 0, 0, 0];
        assert_eq!(p(GraphEntry::DropGraph), [2, 0, 0, 0, 0, 0, 0, 0]);
        let mut hw = vec![3, 0, 0, 0, 16, 0, 0, 0];
        hw.extend_from_slice(&5u64.to_le_bytes());
        hw.extend_from_slice(&6u64.to_le_bytes());
        assert_eq!(
            p(GraphEntry::GraphHighWater {
                next_node_id: 5,
                next_edge_id: 6
            }),
            hw
        );
        // PutNode: id, key_len u16, key, label_count u8, (len u8, label)*, metadata_len u32, metadata.
        let mut put = vec![4, 0, 0, 0, 24, 0, 0, 0];
        put.extend_from_slice(&1u64.to_le_bytes());
        put.extend_from_slice(&[1, 0, b'k', 2, 1, b'A', 1, b'B', 4, 0, 0, 0]);
        put.extend_from_slice(&empty);
        assert_eq!(
            p(GraphEntry::PutNode {
                node_id: NodeId(1),
                key: "k".into(),
                labels: labels(&["B", "A"]),
                properties: Properties::new()
            }),
            put
        );
        // PatchNode: id, add, remove, set_len u32 + metadata, unset_count u8 + (len u16, name)*.
        let mut patch = vec![5, 0, 0, 0, 26, 0, 0, 0];
        patch.extend_from_slice(&1u64.to_le_bytes());
        patch.extend_from_slice(&[1, 1, b'A', 1, 1, b'B', 4, 0, 0, 0]);
        patch.extend_from_slice(&empty);
        patch.extend_from_slice(&[1, 1, 0, b'x']);
        assert_eq!(
            p(GraphEntry::PatchNode {
                node_id: NodeId(1),
                add_labels: labels(&["A"]),
                remove_labels: labels(&["B"]),
                set: Properties::new(),
                unset: labels(&["x"])
            }),
            patch
        );
        let mut delete = vec![6, 0, 0, 0, 8, 0, 0, 0];
        delete.extend_from_slice(&9u64.to_le_bytes());
        assert_eq!(p(GraphEntry::DeleteNode { node_id: NodeId(9) }), delete);
        // PutEdge: id, source, destination, type_len u8, type, key_len u16, key, metadata_len, metadata.
        let mut edge = vec![7, 0, 0, 0, 37, 0, 0, 0];
        edge.extend_from_slice(&3u64.to_le_bytes());
        edge.extend_from_slice(&1u64.to_le_bytes());
        edge.extend_from_slice(&2u64.to_le_bytes());
        edge.extend_from_slice(&[1, b'R', 1, 0, b'e', 4, 0, 0, 0]);
        edge.extend_from_slice(&empty);
        assert_eq!(
            p(GraphEntry::PutEdge {
                edge_id: EdgeId(3),
                source: NodeId(1),
                destination: NodeId(2),
                relationship: "R".into(),
                key: Some("e".into()),
                properties: Properties::new()
            }),
            edge
        );
        let mut patch_edge = vec![8, 0, 0, 0, 17, 0, 0, 0];
        patch_edge.extend_from_slice(&3u64.to_le_bytes());
        patch_edge.extend_from_slice(&[4, 0, 0, 0]);
        patch_edge.extend_from_slice(&empty);
        patch_edge.push(0);
        assert_eq!(
            p(GraphEntry::PatchEdge {
                edge_id: EdgeId(3),
                set: Properties::new(),
                unset: BTreeSet::new()
            }),
            patch_edge
        );
        let mut delete_edge = vec![9, 0, 0, 0, 8, 0, 0, 0];
        delete_edge.extend_from_slice(&3u64.to_le_bytes());
        assert_eq!(
            p(GraphEntry::DeleteEdge { edge_id: EdgeId(3) }),
            delete_edge
        );
        let mut attach = vec![10, 0, 0, 0, 20, 0, 0, 0];
        attach.extend_from_slice(&1u64.to_le_bytes());
        attach.extend_from_slice(&[7, 0, 0, 0]);
        attach.extend_from_slice(&42u64.to_le_bytes());
        assert_eq!(
            p(GraphEntry::Attach {
                node_id: NodeId(1),
                collection: 7,
                record_id: 42
            }),
            attach
        );
        let mut detach = vec![11, 0, 0, 0, 12, 0, 0, 0];
        detach.extend_from_slice(&1u64.to_le_bytes());
        detach.extend_from_slice(&[7, 0, 0, 0]);
        assert_eq!(
            p(GraphEntry::Detach {
                node_id: NodeId(1),
                collection: 7
            }),
            detach
        );
        // Gap 3: NodeExpiry and EdgeExpiry: id, then the deadline only when set.
        for (tag, id) in [(13u8, 4u64), (14, 5)] {
            let entry = |expires_at_ms| match tag {
                13 => GraphEntry::NodeExpiry {
                    node_id: NodeId(id),
                    expires_at_ms,
                },
                _ => GraphEntry::EdgeExpiry {
                    edge_id: EdgeId(id),
                    expires_at_ms,
                },
            };
            let mut set = vec![tag, 0, 0, 0, 16, 0, 0, 0];
            set.extend_from_slice(&id.to_le_bytes());
            set.extend_from_slice(&1_000u64.to_le_bytes());
            assert_eq!(p(entry(Some(1_000))), set);
            let mut clear = vec![tag, 0, 0, 0, 8, 0, 0, 0];
            clear.extend_from_slice(&id.to_le_bytes());
            assert_eq!(p(entry(None)), clear);
        }
        let mut reclaim = vec![15, 0, 0, 0, 8, 0, 0, 0];
        reclaim.extend_from_slice(&77u64.to_le_bytes());
        assert_eq!(p(GraphEntry::Reclaim { at_ms: 77 }), reclaim);
    }

    /// An expiry payload is 8 or 16 bytes. A reclaim opens a body or directly
    /// follows an expiry entry due at its clock, all at one clock, and
    /// reclaims do not count against the entry limit.
    #[test]
    fn expiry_payloads_and_reclaim_placement_are_strict() {
        for tag in [tag::NODE_EXPIRY, tag::EDGE_EXPIRY] {
            for len in [1, 7, 9, 15] {
                assert_eq!(
                    decode_one(&raw(tag, &vec![0; len])),
                    Err(GraphError::Corrupt("truncated entry payload")),
                    "tag {tag}, {len} bytes"
                );
            }
            assert_eq!(
                decode_one(&raw(tag, &[0; 17])),
                Err(GraphError::Corrupt("trailing entry payload bytes"))
            );
        }
        assert_eq!(
            decode_one(&raw(tag::RECLAIM, &[0; 9])),
            Err(GraphError::Corrupt("trailing entry payload bytes"))
        );

        let reclaim = GraphEntry::Reclaim { at_ms: 5 };
        let node_expiry = |deadline| GraphEntry::NodeExpiry {
            node_id: NodeId(1),
            expires_at_ms: deadline,
        };
        let edge_expiry = |deadline| GraphEntry::EdgeExpiry {
            edge_id: EdgeId(1),
            expires_at_ms: deadline,
        };
        let misplaced = GraphError::Corrupt("misplaced reclaim");
        for entries in [
            vec![GraphEntry::DropGraph, reclaim.clone()],
            // After an expiry that is not due, or a cleared one.
            vec![node_expiry(Some(6)), reclaim.clone()],
            vec![edge_expiry(None), reclaim.clone()],
            vec![reclaim.clone(), reclaim.clone()],
            // Another clock.
            vec![
                reclaim.clone(),
                node_expiry(Some(4)),
                GraphEntry::Reclaim { at_ms: 4 },
            ],
        ] {
            let late = body(entries.clone());
            assert_eq!(late.validate(), Err(misplaced.clone()), "{entries:?}");
            assert_eq!(encode_body(&late), Err(misplaced.clone()));
            let mut bytes = encode_one(entries[0].clone());
            for entry in &entries[1..] {
                bytes.extend_from_slice(&encode_one(entry.clone())[BODY_HEADER_BYTES..]);
            }
            assert_eq!(
                decode_body(&bytes, entries.len() as u32),
                Err(misplaced.clone())
            );
        }
        for entries in [
            vec![node_expiry(Some(5)), reclaim.clone()],
            vec![reclaim.clone(), edge_expiry(Some(3)), reclaim.clone()],
        ] {
            let placed = body(entries);
            let encoded = encode_body(&placed).unwrap();
            assert_eq!(
                decode_body(&encoded, placed.entries.len() as u32).unwrap(),
                placed
            );
        }

        // A full batch with a reclaim opening it and after every entry.
        let mut entries = vec![reclaim.clone()];
        for _ in 0..MAX_BATCH_ENTRIES {
            entries.extend([node_expiry(Some(5)), reclaim.clone()]);
        }
        assert_eq!(entries.len(), MAX_BODY_ENTRIES);
        let full = body(entries);
        let encoded = encode_body(&full).unwrap();
        assert_eq!(
            decode_body(&encoded, MAX_BODY_ENTRIES as u32).unwrap(),
            full
        );
        let mut over = full;
        over.entries.push(GraphEntry::DropGraph);
        assert_eq!(
            encode_body(&over),
            Err(GraphError::BatchSize("more than 4096 entries"))
        );
    }

    #[test]
    fn every_tag_round_trips_individually_and_in_one_body() {
        let entries = every_entry();
        let tags: BTreeSet<u8> = entries.iter().map(GraphEntry::tag).collect();
        assert_eq!(
            tags,
            BTreeSet::from([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 14, 15, 64])
        );
        for entry in &entries {
            let encoded = encode_one(entry.clone());
            let decoded = decode_one(&encoded).unwrap();
            assert_eq!(decoded.entries, std::slice::from_ref(entry));
            assert_eq!(encode_body(&decoded).unwrap(), encoded, "canonical");
        }
        let all = body(entries);
        let encoded = encode_body(&all).unwrap();
        let decoded = decode_body(&encoded, all.entries.len() as u32).unwrap();
        assert_eq!(decoded, all);
        assert_eq!(encode_body(&decoded).unwrap(), encoded);
    }

    #[test]
    fn every_truncation_of_every_body_is_rejected() {
        let all = body(every_entry());
        let count = all.entries.len() as u32;
        let encoded = encode_body(&all).unwrap();
        for length in 0..encoded.len() {
            assert!(
                decode_body(&encoded[..length], count).is_err(),
                "prefix {length}"
            );
        }
        for entry in every_entry() {
            let encoded = encode_one(entry);
            for length in 0..encoded.len() {
                assert!(decode_one(&encoded[..length]).is_err());
            }
        }
    }

    #[test]
    fn trailing_bytes_and_count_mismatches_are_rejected() {
        let encoded = encode_one(GraphEntry::DropGraph);
        let mut trailing = encoded.clone();
        trailing.push(0);
        assert_eq!(
            decode_one(&trailing),
            Err(GraphError::Corrupt("more entries than the frame count"))
        );
        assert_eq!(
            decode_body(&trailing, 2),
            Err(GraphError::Corrupt("truncated entry header"))
        );
        let two = encode_body(&body(vec![GraphEntry::DropGraph, GraphEntry::DropGraph])).unwrap();
        assert_eq!(
            decode_body(&two, 1),
            Err(GraphError::Corrupt("more entries than the frame count"))
        );
        assert_eq!(
            decode_body(&encoded, 2),
            Err(GraphError::Corrupt("fewer entries than the frame count"))
        );
        assert_eq!(
            decode_body(&encoded, 0),
            Err(GraphError::BatchSize(
                "a graph batch has at least one entry"
            ))
        );
        // Bytes left inside a payload (envelope length too large for the entry).
        let mut payload = 9u64.to_le_bytes().to_vec();
        payload.push(0);
        assert_eq!(
            decode_one(&raw(tag::DELETE_NODE, &payload)),
            Err(GraphError::Corrupt("trailing entry payload bytes"))
        );
        assert_eq!(
            decode_one(&raw(tag::DROP_GRAPH, &[0])),
            Err(GraphError::Corrupt("trailing entry payload bytes"))
        );
    }

    #[test]
    fn unknown_tags_are_rejected() {
        for tag in (0..=255u8).filter(|t| !matches!(t, 1..=15 | 64)) {
            assert_eq!(
                decode_one(&raw(tag, &[])),
                Err(GraphError::UnknownEntryTag(tag)),
                "tag {tag}"
            );
        }
        assert_eq!(tag::PUT_SCHEMA, 12);
        // An empty tag-12 payload is truncated, not an unknown tag.
        assert!(matches!(
            decode_one(&raw(tag::PUT_SCHEMA, &[])),
            Err(GraphError::Corrupt(_))
        ));
        // Gap 3: so is an empty expiry or reclaim payload.
        for tag in [tag::NODE_EXPIRY, tag::EDGE_EXPIRY, tag::RECLAIM] {
            assert!(
                matches!(decode_one(&raw(tag, &[])), Err(GraphError::Corrupt(_))),
                "tag {tag}"
            );
        }
    }

    #[test]
    fn header_envelope_and_reserved_fields_are_strict() {
        let good = encode_one(GraphEntry::DropGraph);
        let mut bad = good.clone();
        bad[0] = 2;
        assert_eq!(
            decode_one(&bad),
            Err(GraphError::Corrupt("unsupported body version"))
        );
        let mut bad = good.clone();
        bad[1] = 1;
        assert_eq!(
            decode_one(&bad),
            Err(GraphError::Corrupt("non-zero reserved bytes"))
        );
        let mut bad = good.clone();
        bad[2..4].copy_from_slice(&64u16.to_le_bytes());
        assert_eq!(
            decode_one(&bad),
            Err(GraphError::GraphIdUnavailable(GraphId(64)))
        );
        let mut ok = good.clone();
        ok[2..4].copy_from_slice(&63u16.to_le_bytes());
        assert_eq!(decode_one(&ok).unwrap().graph_id, GraphId(63));
        let mut bad = good.clone();
        bad[5] = 1;
        assert_eq!(
            decode_one(&bad),
            Err(GraphError::Corrupt("non-zero entry flags"))
        );
        for index in [6, 7] {
            let mut bad = good.clone();
            bad[index] = 1;
            assert_eq!(
                decode_one(&bad),
                Err(GraphError::Corrupt("non-zero reserved bytes"))
            );
        }
        let attach = encode_one(GraphEntry::Attach {
            node_id: NodeId(1),
            collection: 1,
            record_id: 1,
        });
        let mut bad = attach.clone();
        bad[BODY_HEADER_BYTES + ENTRY_HEADER_BYTES + 10] = 1;
        assert_eq!(
            decode_one(&bad),
            Err(GraphError::Corrupt("non-zero reserved bytes"))
        );
        let detach = encode_one(GraphEntry::Detach {
            node_id: NodeId(1),
            collection: 1,
        });
        let mut bad = detach.clone();
        bad[BODY_HEADER_BYTES + ENTRY_HEADER_BYTES + 11] = 1;
        assert_eq!(
            decode_one(&bad),
            Err(GraphError::Corrupt("non-zero reserved bytes"))
        );
        let vector = encode_one(GraphEntry::VectorMutation {
            collection: 1,
            entry: delete_entry(1),
        });
        let mut bad = vector.clone();
        bad[BODY_HEADER_BYTES + ENTRY_HEADER_BYTES + 2] = 1;
        assert_eq!(
            decode_one(&bad),
            Err(GraphError::Corrupt("non-zero reserved bytes"))
        );
        // An envelope length beyond the body is truncation, not an allocation.
        let mut huge = vec![BODY_VERSION, 0, 1, 0, tag::DROP_GRAPH, 0, 0, 0];
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            decode_one(&huge),
            Err(GraphError::Corrupt("truncated entry payload"))
        );
    }

    #[test]
    fn non_canonical_lists_are_rejected() {
        let mut put = 1u64.to_le_bytes().to_vec();
        put.extend_from_slice(&[1, 0, b'k', 2, 1, b'B', 1, b'A', 4, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            decode_one(&raw(tag::PUT_NODE, &put)),
            Err(GraphError::Corrupt("labels must be strictly ascending"))
        );
        let mut duplicate = 1u64.to_le_bytes().to_vec();
        duplicate.extend_from_slice(&[1, 0, b'k', 2, 1, b'A', 1, b'A', 4, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            decode_one(&raw(tag::PUT_NODE, &duplicate)),
            Err(GraphError::Corrupt("labels must be strictly ascending"))
        );
        let mut unset = 1u64.to_le_bytes().to_vec();
        unset.extend_from_slice(&[4, 0, 0, 0, 0, 0, 0, 0, 2, 1, 0, b'b', 1, 0, b'a']);
        assert_eq!(
            decode_one(&raw(tag::PATCH_EDGE, &unset)),
            Err(GraphError::Corrupt(
                "field names must be strictly ascending"
            ))
        );
        // Properties embedded in entries keep the metadata rules.
        let mut unsorted = 1u64.to_le_bytes().to_vec();
        unsorted.extend_from_slice(&[
            14, 0, 0, 0, 2, 0, 0, 0, 1, 0, b'b', 4, 1, 1, 0, b'a', 4, 1, 0,
        ]);
        assert!(matches!(
            decode_one(&raw(tag::PATCH_EDGE, &unsorted)),
            Err(GraphError::InvalidProperties(_))
        ));
        // metadata_len below 4 (no count) or above 64 KiB is corruption.
        let mut short = 1u64.to_le_bytes().to_vec();
        short.extend_from_slice(&[0, 0, 0, 0, 0]);
        assert_eq!(
            decode_one(&raw(tag::PATCH_EDGE, &short)),
            Err(GraphError::Corrupt("property length out of range"))
        );
        let mut long = 1u64.to_le_bytes().to_vec();
        long.extend_from_slice(&65_537u32.to_le_bytes());
        assert_eq!(
            decode_one(&raw(tag::PATCH_EDGE, &long)),
            Err(GraphError::Corrupt("property length out of range"))
        );
    }

    #[test]
    fn decoded_fields_are_validated() {
        // Graph name rules.
        assert_eq!(
            decode_one(&raw(tag::CREATE_GRAPH, &[0])),
            Err(GraphError::InvalidGraphName)
        );
        assert_eq!(
            decode_one(&raw(tag::CREATE_GRAPH, &[1, b'G'])),
            Err(GraphError::InvalidGraphName)
        );
        assert_eq!(
            decode_one(&raw(tag::CREATE_GRAPH, &[1, 0xFF])),
            Err(GraphError::Corrupt("invalid UTF-8"))
        );
        // Empty and invalid-UTF-8 node keys.
        let mut empty_key = 1u64.to_le_bytes().to_vec();
        empty_key.extend_from_slice(&[0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            decode_one(&raw(tag::PUT_NODE, &empty_key)),
            Err(GraphError::InvalidNodeKey)
        );
        let mut bad_key = 1u64.to_le_bytes().to_vec();
        bad_key.extend_from_slice(&[1, 0, 0xC3, 0, 4, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            decode_one(&raw(tag::PUT_NODE, &bad_key)),
            Err(GraphError::Corrupt("invalid UTF-8"))
        );
        // Label with a forbidden character; nine labels.
        let mut bad_label = 1u64.to_le_bytes().to_vec();
        bad_label.extend_from_slice(&[1, 0, b'k', 1, 1, b'-', 4, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            decode_one(&raw(tag::PUT_NODE, &bad_label)),
            Err(GraphError::InvalidLabel)
        );
        let mut nine = 1u64.to_le_bytes().to_vec();
        nine.extend_from_slice(&[1, 0, b'k', 9]);
        assert_eq!(
            decode_one(&raw(tag::PUT_NODE, &nine)),
            Err(GraphError::TooManyLabels)
        );
        // Relationship type rules.
        let mut bad_type = [1u64, 1, 1]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<_>>();
        bad_type.extend_from_slice(&[0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            decode_one(&raw(tag::PUT_EDGE, &bad_type)),
            Err(GraphError::InvalidRelationshipType)
        );
        // Patch lists that overlap.
        let mut overlap = 1u64.to_le_bytes().to_vec();
        overlap.extend_from_slice(&[1, 1, b'A', 1, 1, b'A', 4, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            decode_one(&raw(tag::PATCH_NODE, &overlap)),
            Err(GraphError::InvalidLabel)
        );
        let mut set_unset = 1u64.to_le_bytes().to_vec();
        set_unset.extend_from_slice(&[9, 0, 0, 0, 1, 0, 0, 0, 1, 0, b'a', 4, 1, 1, 1, 0, b'a']);
        assert_eq!(
            decode_one(&raw(tag::PATCH_EDGE, &set_unset)),
            Err(GraphError::InvalidProperties(
                "a field cannot be both set and unset"
            ))
        );
        let mut empty_unset = 1u64.to_le_bytes().to_vec();
        empty_unset.extend_from_slice(&[4, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0]);
        assert_eq!(
            decode_one(&raw(tag::PATCH_EDGE, &empty_unset)),
            Err(GraphError::InvalidProperties(
                "field names must be 1..256 UTF-8 bytes"
            ))
        );
    }

    #[test]
    fn vector_mutation_entries_are_structurally_checked() {
        let ok = [
            delete_entry(1),
            put_entry(1, &[0.5], b""),
            put_entry(1, &[0.5, 1.5], b"payload"),
            {
                let mut b = vec![4, 0, 0, 0];
                b.extend_from_slice(&1u64.to_le_bytes());
                b.extend_from_slice(&3u32.to_le_bytes());
                b.extend_from_slice(b"abc");
                b
            },
        ];
        for entry in ok {
            let encoded = encode_one(GraphEntry::VectorMutation {
                collection: 1,
                entry: entry.clone(),
            });
            assert_eq!(
                decode_one(&encoded).unwrap().entries,
                [GraphEntry::VectorMutation {
                    collection: 1,
                    entry
                }]
            );
        }
        let mut delete_with_body = delete_entry(1);
        delete_with_body.push(0);
        let mut bad_reserved = delete_entry(1);
        bad_reserved[2] = 1;
        let mut unknown_op = delete_entry(1);
        unknown_op[0] = 3;
        let mut put_unaligned = put_entry(1, &[0.5], b"");
        put_unaligned.push(0);
        let cases = [
            (Vec::new(), "truncated batch entry"),
            (delete_entry(1)[..15].to_vec(), "truncated batch entry"),
            (delete_with_body, "length mismatch"),
            (bad_reserved, "reserved bytes"),
            (unknown_op, "unknown mutation op"),
            (put_entry(1, &[], b"p"), "length mismatch"),
            (put_unaligned, "length mismatch"),
        ];
        for (entry, reason) in cases {
            let entry = GraphEntry::VectorMutation {
                collection: 1,
                entry,
            };
            assert_eq!(
                entry.validate(),
                Err(GraphError::InvalidVectorMutation(reason))
            );
            assert_eq!(
                encode_body(&body(vec![entry.clone()])),
                Err(GraphError::InvalidVectorMutation(reason))
            );
            let mut payload = vec![1, 0, 0, 0];
            if let GraphEntry::VectorMutation { entry, .. } = &entry {
                payload.extend_from_slice(entry);
            }
            assert_eq!(
                decode_one(&raw(tag::VECTOR_MUTATION, &payload)),
                Err(GraphError::InvalidVectorMutation(reason))
            );
        }
    }

    #[test]
    fn maximum_sizes() {
        let key = "k".repeat(256);
        let label_set: BTreeSet<String> =
            (0..8).map(|i| format!("{i}{}", "L".repeat(62))).collect();
        let max_node = GraphEntry::PutNode {
            node_id: NodeId(u64::MAX - 1),
            key: key.clone(),
            labels: label_set.clone(),
            properties: (0..64)
                .map(|i| (format!("{i:0>256}"), PropertyValue::Integer(i)))
                .collect(),
        };
        let encoded = encode_one(max_node.clone());
        assert_eq!(decode_one(&encoded).unwrap().entries, [max_node]);
        let max_edge = GraphEntry::PutEdge {
            edge_id: EdgeId(u64::MAX - 1),
            source: NodeId(0),
            destination: NodeId(u64::MAX),
            relationship: "R".repeat(63),
            key: Some(key.clone()),
            properties: Properties::new(),
        };
        assert_eq!(
            decode_one(&encode_one(max_edge.clone())).unwrap().entries,
            [max_edge]
        );
        let unset: BTreeSet<String> = (0..64).map(|i| format!("{i:0>256}")).collect();
        let max_patch = GraphEntry::PatchEdge {
            edge_id: EdgeId(1),
            set: Properties::new(),
            unset: unset.clone(),
        };
        assert_eq!(
            decode_one(&encode_one(max_patch.clone())).unwrap().entries,
            [max_patch]
        );

        let too_long_key = GraphEntry::PutNode {
            node_id: NodeId(1),
            key: "k".repeat(257),
            labels: BTreeSet::new(),
            properties: Properties::new(),
        };
        assert_eq!(
            encode_body(&body(vec![too_long_key])),
            Err(GraphError::InvalidNodeKey)
        );
        let too_long_edge_key = GraphEntry::PutEdge {
            edge_id: EdgeId(1),
            source: NodeId(1),
            destination: NodeId(1),
            relationship: "R".into(),
            key: Some("k".repeat(257)),
            properties: Properties::new(),
        };
        assert_eq!(
            encode_body(&body(vec![too_long_edge_key])),
            Err(GraphError::InvalidEdgeKey)
        );
        let mut too_many_unset = unset;
        too_many_unset.insert("z".into());
        assert_eq!(
            GraphEntry::PatchEdge {
                edge_id: EdgeId(1),
                set: Properties::new(),
                unset: too_many_unset
            }
            .validate(),
            Err(GraphError::InvalidProperties("unset exceeds 64 fields"))
        );
        let mut nine = label_set;
        nine.insert("Z".into());
        assert_eq!(
            GraphEntry::PatchNode {
                node_id: NodeId(1),
                add_labels: nine,
                remove_labels: BTreeSet::new(),
                set: Properties::new(),
                unset: BTreeSet::new()
            }
            .validate(),
            Err(GraphError::TooManyLabels)
        );
    }

    #[test]
    fn encoded_length_is_checked_before_any_payload_is_copied() {
        // The size guard is pure arithmetic: an impossible payload length is
        // rejected without allocating or copying anything.
        let error = GraphError::BatchSize("more than 16777216 bytes");
        let too_big = Err(error.clone());
        assert_eq!(checked_entry_end(BODY_HEADER_BYTES, usize::MAX), too_big);
        assert_eq!(checked_entry_end(usize::MAX, 0), too_big);
        let room = MAX_BATCH_BYTES - BODY_HEADER_BYTES - ENTRY_HEADER_BYTES;
        assert_eq!(
            checked_entry_end(BODY_HEADER_BYTES, room),
            Ok(MAX_BATCH_BYTES)
        );
        assert_eq!(checked_entry_end(BODY_HEADER_BYTES, room + 1), too_big);

        // The computed length equals the encoded length for every tag.
        for entry in every_entry() {
            let one = body(vec![entry]);
            assert_eq!(encoded_body_len(&one), Ok(encode_body(&one).unwrap().len()));
        }
        let all = body(every_entry());
        assert_eq!(encoded_body_len(&all), Ok(encode_body(&all).unwrap().len()));

        // One vector payload past the limit is rejected by the length
        // calculation that runs before the output buffer is allocated.
        let oversized = body(vec![GraphEntry::VectorMutation {
            collection: 1,
            entry: put_entry(1, &[1.0], &vec![0u8; room - 4 - 16 - 4 + 1]),
        }]);
        assert_eq!(encoded_body_len(&oversized), too_big);
        assert_eq!(encode_body(&oversized), Err(error));
    }

    #[test]
    fn batch_entry_and_byte_limits() {
        let max = body(vec![GraphEntry::DropGraph; MAX_BATCH_ENTRIES]);
        let encoded = encode_body(&max).unwrap();
        assert_eq!(
            decode_body(&encoded, MAX_BATCH_ENTRIES as u32).unwrap(),
            max
        );
        let over = body(vec![GraphEntry::DropGraph; MAX_BATCH_ENTRIES + 1]);
        assert_eq!(
            encode_body(&over),
            Err(GraphError::BatchSize("more than 4096 entries"))
        );
        assert_eq!(
            decode_body(&encoded, MAX_BODY_ENTRIES as u32 + 1),
            Err(GraphError::BatchSize("more than 4096 entries"))
        );
        // A frame count within the body bound still holds 4,096 counted
        // entries at most.
        let mut bytes = vec![BODY_VERSION, 0, 1, 0];
        for _ in 0..=MAX_BATCH_ENTRIES {
            bytes.extend_from_slice(&[tag::DROP_GRAPH, 0, 0, 0, 0, 0, 0, 0]);
        }
        assert_eq!(
            decode_body(&bytes, MAX_BATCH_ENTRIES as u32 + 1),
            Err(GraphError::BatchSize("more than 4096 entries"))
        );
        assert_eq!(
            encode_body(&body(Vec::new())),
            Err(GraphError::BatchSize(
                "a graph batch has at least one entry"
            ))
        );
        assert_eq!(
            encode_body(&GraphBody {
                graph_id: GraphId(64),
                entries: vec![GraphEntry::DropGraph]
            }),
            Err(GraphError::GraphIdUnavailable(GraphId(64)))
        );

        // Fill the body to exactly MAX_BATCH_BYTES with vector mutations
        // (each entry: 8 envelope + 4 collection/reserved + 16 + payload).
        let per_entry_overhead = ENTRY_HEADER_BYTES + 4 + 16 + 4;
        let entries = 4;
        let total_payload = MAX_BATCH_BYTES - BODY_HEADER_BYTES - entries * per_entry_overhead;
        let mut list = Vec::new();
        for i in 0..entries {
            let size = total_payload / entries + usize::from(i < total_payload % entries);
            list.push(GraphEntry::VectorMutation {
                collection: 1,
                entry: put_entry(i as u64, &[1.0], &vec![0u8; size]),
            });
        }
        let exact = body(list);
        let encoded = encode_body(&exact).unwrap();
        assert_eq!(encoded.len(), MAX_BATCH_BYTES);
        assert_eq!(decode_body(&encoded, entries as u32).unwrap(), exact);
        let mut over = exact;
        if let Some(GraphEntry::VectorMutation { entry, .. }) = over.entries.last_mut() {
            entry.extend_from_slice(&[0; 4]);
        }
        assert_eq!(
            encode_body(&over),
            Err(GraphError::BatchSize("more than 16777216 bytes"))
        );
        let mut oversized = encoded;
        oversized.push(0);
        assert_eq!(
            decode_body(&oversized, entries as u32),
            Err(GraphError::BatchSize("more than 16777216 bytes"))
        );
    }
}
