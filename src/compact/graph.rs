//! Private P4 graph source model and immutable CSR codec.
//!
//! This test-only module owns the graph-domain representation and its private
//! checkpoint-block framing candidate.
//!
//! A graph's declared schema is stored losslessly as declarations only. The
//! derived property indexes are not stored: a schema-bearing payload is
//! loaded through the engine's own snapshot path, which validates the
//! declarations, rebuilds the indexes and checks every declared rule.

pub(crate) mod framing;
#[cfg(not(target_arch = "wasm32"))]
mod import;
mod overlay;
mod read;
mod view;

use crate::property_graph::engine::codec::TemporalEntry;
use crate::property_graph::engine::limits::{
    MAX_BATCH_ENTRIES, MAX_FIELD_NAME_BYTES, MAX_LABEL_BYTES, MAX_RELATIONSHIP_TYPE_BYTES,
    MAX_SCHEMA_DESCRIPTION_BYTES, MAX_SCHEMA_ENDPOINTS, MAX_SCHEMA_LABELS, MAX_SCHEMA_PROPERTIES,
    MAX_SCHEMA_RELATIONSHIPS,
};
use crate::property_graph::engine::temporal::{MAX_SEGMENTS, Segment};
use crate::property_graph::engine::{
    EdgeId, Graph, GraphBody, GraphEdge, GraphEntry, GraphId, GraphNode, GraphSchema, GraphState,
    LabelDefinition, Multiplicity, NodeId, Properties, PropertyDefinition, PropertyIndexKind,
    PropertyType, PropertyValue, RelationshipDefinition, SnapshotBuilder, properties,
};
use std::collections::{BTreeMap, BTreeSet};

const MAGIC: &[u8; 8] = b"VLGCSR06";
const TEMPORAL_SECTION: &[u8; 8] = b"VLTEMP01";
const MAX_BYTES: usize = 64 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
enum CodecError {
    Corrupt,
    UnsupportedSource(&'static str),
}

#[derive(Clone, Debug, PartialEq)]
struct SourceNode {
    id: u64,
    key: Option<String>,
    labels: Vec<String>,
    revision: u64,
    deadline: Option<u64>,
    properties: Properties,
    attachments: BTreeMap<u16, u64>,
}

#[derive(Clone, Debug, PartialEq)]
struct SourceEdge {
    id: u64,
    source: u64,
    target: u64,
    relationship: String,
    key: Option<String>,
    revision: u64,
    deadline: Option<u64>,
    properties: Properties,
    segments: Option<Vec<Segment>>,
}

#[derive(Clone, Debug, PartialEq)]
struct GraphSource {
    graph_id: u16,
    name: String,
    sequence: u64,
    next_node_id: u64,
    next_edge_id: u64,
    temporal_enabled: bool,
    schema: GraphSchema,
    nodes: Vec<SourceNode>,
    edges: Vec<SourceEdge>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EdgeRow {
    id: u64,
    source: u32,
    target: u32,
    relationship: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Csr {
    offsets: Vec<u32>,
    edges: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CompactGraph {
    source: GraphSource,
    labels: Vec<String>,
    relationships: Vec<String>,
    property_names: Vec<String>,
    label_postings: Vec<Vec<u32>>,
    attachment_owners: BTreeMap<(u16, u64), u64>,
    node_ordinals: BTreeMap<u64, u32>,
    edge_rows: Vec<EdgeRow>,
    out: Csr,
    incoming: Csr,
}

impl CompactGraph {
    fn postings(&self, label: &str) -> &[u32] {
        self.labels
            .binary_search_by(|candidate| candidate.as_str().cmp(label))
            .map_or(&[], |code| self.label_postings[code].as_slice())
    }

    fn label_scan(&self, label: &str, after: Option<u64>) -> impl Iterator<Item = u64> + '_ {
        let postings = self.postings(label);
        let start = after.map_or(0, |id| {
            postings.partition_point(|ordinal| self.source.nodes[*ordinal as usize].id <= id)
        });
        postings[start..]
            .iter()
            .map(|ordinal| self.source.nodes[*ordinal as usize].id)
    }

    fn label_intersection(
        &self,
        labels: &[&str],
        after: Option<u64>,
        limit: usize,
    ) -> Result<Vec<u64>, CodecError> {
        let mut distinct = BTreeSet::new();
        for label in labels {
            crate::property_graph::engine::model::validate_label(label)
                .map_err(|_| CodecError::Corrupt)?;
            distinct.insert(*label);
            if distinct.len() > 8 {
                return Err(CodecError::Corrupt);
            }
        }
        if distinct.is_empty() || limit == 0 {
            return Err(CodecError::Corrupt);
        }
        let lists: Vec<_> = distinct.iter().map(|label| self.postings(label)).collect();
        let smallest = lists.iter().min_by_key(|list| list.len()).unwrap();
        let start = after.map_or(0, |id| {
            smallest.partition_point(|ordinal| self.source.nodes[*ordinal as usize].id <= id)
        });
        Ok(smallest[start..]
            .iter()
            .filter(|ordinal| lists.iter().all(|list| list.binary_search(ordinal).is_ok()))
            .take(limit)
            .map(|ordinal| self.source.nodes[*ordinal as usize].id)
            .collect())
    }
}

impl GraphSource {
    fn from_graph(graph: &Graph) -> Result<Self, CodecError> {
        let mut nodes = Vec::new();
        for node in graph.nodes() {
            nodes.push(source_node(
                node,
                graph.node_expiry(node.id),
                graph.attachments(node.id).collect(),
            ));
        }
        let mut edges = Vec::new();
        let mut temporal_enabled = false;
        for edge in graph.edges() {
            let segments = graph.segments(edge.id);
            temporal_enabled |= segments.is_some();
            edges.push(source_edge(edge, graph.edge_expiry(edge.id), segments));
        }
        Ok(Self {
            graph_id: graph.id().0,
            name: graph.name().to_owned(),
            sequence: graph.graph_sequence(),
            next_node_id: graph.next_node_id(),
            next_edge_id: graph.next_edge_id(),
            temporal_enabled,
            schema: graph.schema().clone(),
            nodes,
            edges,
        })
    }
}

/// Loads `source` through the engine's snapshot path (`SnapshotBuilder`),
/// which validates its schema, rebuilds the derived property indexes and
/// checks every declared rule, then restores the source revisions. The
/// engine requires node keys, so keyless sources are not representable.
fn engine_state(source: &GraphSource) -> Result<GraphState, CodecError> {
    let graph_id = GraphId(source.graph_id);
    let mut builder = SnapshotBuilder::new(source.sequence);
    let mut entries = Vec::new();
    let mut emit = |entry: GraphEntry| {
        entries.push(entry);
        if entries.len() < MAX_BATCH_ENTRIES {
            return Ok(());
        }
        builder
            .push(GraphBody {
                graph_id,
                entries: std::mem::take(&mut entries),
            })
            .map_err(|_| CodecError::Corrupt)
    };
    emit(GraphEntry::CreateGraph {
        name: source.name.clone(),
    })?;
    if !source.schema.is_empty() {
        emit(GraphEntry::PutSchema {
            schema: source.schema.clone(),
        })?;
    }
    for node in &source.nodes {
        let key = node
            .key
            .clone()
            .ok_or(CodecError::UnsupportedSource("schema with keyless nodes"))?;
        emit(GraphEntry::PutNode {
            node_id: NodeId(node.id),
            key,
            labels: node.labels.iter().cloned().collect(),
            properties: node.properties.clone(),
        })?;
        if let Some(deadline) = node.deadline {
            emit(GraphEntry::NodeExpiry {
                node_id: NodeId(node.id),
                expires_at_ms: Some(deadline),
            })?;
        }
    }
    for edge in &source.edges {
        if let Some(segments) = &edge.segments {
            emit(GraphEntry::Temporal(TemporalEntry::PutEdge {
                edge_id: EdgeId(edge.id),
                revision: edge.revision,
                source: NodeId(edge.source),
                destination: NodeId(edge.target),
                relationship: edge.relationship.clone(),
                key: edge.key.clone(),
                segments: segments.clone(),
            }))?;
        } else {
            emit(GraphEntry::PutEdge {
                edge_id: EdgeId(edge.id),
                source: NodeId(edge.source),
                destination: NodeId(edge.target),
                relationship: edge.relationship.clone(),
                key: edge.key.clone(),
                properties: edge.properties.clone(),
            })?;
        }
        if let Some(deadline) = edge.deadline {
            emit(GraphEntry::EdgeExpiry {
                edge_id: EdgeId(edge.id),
                expires_at_ms: Some(deadline),
            })?;
        }
    }
    for node in &source.nodes {
        for (collection, record) in &node.attachments {
            emit(GraphEntry::Attach {
                node_id: NodeId(node.id),
                collection: *collection,
                record_id: *record,
            })?;
        }
    }
    emit(GraphEntry::GraphHighWater {
        next_node_id: source.next_node_id,
        next_edge_id: source.next_edge_id,
    })?;
    if !entries.is_empty() {
        builder
            .push(GraphBody { graph_id, entries })
            .map_err(|_| CodecError::Corrupt)?;
    }
    let mut state = builder.finish().map_err(|_| CodecError::Corrupt)?;
    if source.temporal_enabled {
        state.set_temporal(true).map_err(|_| CodecError::Corrupt)?;
    }
    let nodes: Vec<_> = source
        .nodes
        .iter()
        .map(|node| (node.id, node.revision))
        .collect();
    let edges: Vec<_> = source
        .edges
        .iter()
        .map(|edge| (edge.id, edge.revision))
        .collect();
    state
        .restore_revisions(graph_id, &nodes, &edges)
        .map_err(|_| CodecError::Corrupt)?;
    Ok(state)
}

fn source_node(
    node: &GraphNode,
    deadline: Option<u64>,
    attachments: BTreeMap<u16, u64>,
) -> SourceNode {
    SourceNode {
        id: node.id.0,
        key: Some(node.key.clone()),
        labels: node.labels.iter().cloned().collect(),
        revision: node.revision,
        deadline,
        properties: node.properties.clone(),
        attachments,
    }
}

fn source_edge(
    edge: &GraphEdge,
    deadline: Option<u64>,
    segments: Option<&[Segment]>,
) -> SourceEdge {
    SourceEdge {
        id: edge.id.0,
        source: edge.source.0,
        target: edge.destination.0,
        relationship: edge.relationship.clone(),
        key: edge.key.clone(),
        revision: edge.revision,
        deadline,
        properties: edge.properties.clone(),
        segments: segments.map(<[Segment]>::to_vec),
    }
}

fn encode(source: &GraphSource) -> Result<Vec<u8>, CodecError> {
    let graph = build(source.clone())?;
    let mut out = Vec::new();
    put_bytes(&mut out, MAGIC)?;
    put_u16(&mut out, graph.source.graph_id)?;
    put_string(&mut out, &graph.source.name)?;
    put_u64(&mut out, graph.source.sequence)?;
    put_u64(&mut out, graph.source.next_node_id)?;
    put_u64(&mut out, graph.source.next_edge_id)?;
    put_schema(&mut out, &graph.source.schema)?;
    put_strings(&mut out, &graph.labels)?;
    put_strings(&mut out, &graph.relationships)?;
    put_strings(&mut out, &graph.property_names)?;
    put_count(&mut out, graph.source.nodes.len())?;
    for node in &graph.source.nodes {
        put_u64(&mut out, node.id)?;
        put_optional_string(&mut out, node.key.as_deref())?;
        put_u64(&mut out, node.revision)?;
        put_optional_u64(&mut out, node.deadline)?;
        let labels = node
            .labels
            .iter()
            .map(|label| {
                graph
                    .labels
                    .binary_search(label)
                    .map_err(|_| CodecError::Corrupt)
            })
            .collect::<Result<Vec<_>, _>>()?;
        put_count(&mut out, labels.len())?;
        for code in labels {
            put_u32(
                &mut out,
                u32::try_from(code).map_err(|_| CodecError::Corrupt)?,
            )?;
        }
        put_properties(&mut out, &node.properties, &graph.property_names)?;
    }
    put_count(&mut out, graph.edge_rows.len())?;
    for edge in &graph.edge_rows {
        put_u64(&mut out, edge.id)?;
        put_u32(&mut out, edge.source)?;
        put_u32(&mut out, edge.target)?;
        put_u32(&mut out, edge.relationship)?;
        let key = graph
            .source
            .edges
            .binary_search_by_key(&edge.id, |row| row.id)
            .map_err(|_| CodecError::Corrupt)?;
        put_optional_string(&mut out, graph.source.edges[key].key.as_deref())?;
        put_u64(&mut out, graph.source.edges[key].revision)?;
        put_optional_u64(&mut out, graph.source.edges[key].deadline)?;
        put_properties(
            &mut out,
            &graph.source.edges[key].properties,
            &graph.property_names,
        )?;
    }
    put_csr(&mut out, &graph.out)?;
    put_csr(&mut out, &graph.incoming)?;
    put_count(&mut out, graph.label_postings.len())?;
    for (code, ordinals) in graph.label_postings.iter().enumerate() {
        put_count(&mut out, code)?;
        put_count(&mut out, ordinals.len())?;
        for ordinal in ordinals {
            put_u32(&mut out, *ordinal)?;
        }
    }
    let attachment_count = graph.source.nodes.iter().try_fold(0usize, |total, node| {
        total
            .checked_add(node.attachments.len())
            .ok_or(CodecError::Corrupt)
    })?;
    put_count(&mut out, attachment_count)?;
    for (ordinal, node) in graph.source.nodes.iter().enumerate() {
        for (collection, record) in &node.attachments {
            put_u32(
                &mut out,
                u32::try_from(ordinal).map_err(|_| CodecError::Corrupt)?,
            )?;
            put_u16(&mut out, *collection)?;
            put_u64(&mut out, *record)?;
        }
    }
    if graph.source.temporal_enabled {
        put_bytes(&mut out, TEMPORAL_SECTION)?;
        let temporal_count = graph
            .source
            .edges
            .iter()
            .filter(|edge| edge.segments.is_some())
            .count();
        put_count(&mut out, temporal_count)?;
        for (ordinal, edge) in graph.source.edges.iter().enumerate() {
            let Some(segments) = &edge.segments else {
                continue;
            };
            put_u32(
                &mut out,
                u32::try_from(ordinal).map_err(|_| CodecError::Corrupt)?,
            )?;
            put_count(&mut out, segments.len())?;
            for segment in segments {
                put_optional_i64(&mut out, segment.valid_from)?;
                put_optional_i64(&mut out, segment.valid_to)?;
                put_properties(&mut out, &segment.properties, &graph.property_names)?;
            }
        }
    }
    if out.len() > MAX_BYTES {
        return Err(CodecError::Corrupt);
    }
    Ok(out)
}

fn decode(bytes: &[u8]) -> Result<CompactGraph, CodecError> {
    if bytes.len() > MAX_BYTES {
        return Err(CodecError::Corrupt);
    }
    let mut reader = Reader::new(bytes);
    if reader.take(MAGIC.len())? != MAGIC {
        return Err(CodecError::Corrupt);
    }
    let graph_id = reader.u16()?;
    let name = reader.string(63)?;
    let sequence = reader.u64()?;
    let next_node_id = reader.u64()?;
    let next_edge_id = reader.u64()?;
    let schema = reader.schema()?;
    let labels = reader.strings(63)?;
    let relationships = reader.strings(63)?;
    let property_names = reader.strings(256)?;
    let node_count = reader.entries(22)?;
    let mut nodes = Vec::new();
    nodes
        .try_reserve_exact(node_count)
        .map_err(|_| CodecError::Corrupt)?;
    for _ in 0..node_count {
        let id = reader.u64()?;
        let key = reader.optional_string()?;
        let revision = reader.u64()?;
        let deadline = reader.optional_u64()?;
        let label_count = reader.entries(4)?;
        if label_count > 8 {
            return Err(CodecError::Corrupt);
        }
        let mut node_labels = Vec::with_capacity(label_count);
        let mut previous = None;
        for _ in 0..label_count {
            let code = reader.u32()?;
            if previous.is_some_and(|prior| code <= prior) {
                return Err(CodecError::Corrupt);
            }
            previous = Some(code);
            node_labels.push(
                labels
                    .get(usize::try_from(code).map_err(|_| CodecError::Corrupt)?)
                    .ok_or(CodecError::Corrupt)?
                    .clone(),
            );
        }
        let properties = reader.properties(&property_names)?;
        let attachments = BTreeMap::new();
        nodes.push(SourceNode {
            id,
            key,
            labels: node_labels,
            revision,
            deadline,
            properties,
            attachments,
        });
    }
    if nodes
        .windows(2)
        .any(|pair: &[SourceNode]| pair[0].id >= pair[1].id)
    {
        return Err(CodecError::Corrupt);
    }
    let edge_count = reader.entries(30)?;
    let mut edges = Vec::new();
    edges
        .try_reserve_exact(edge_count)
        .map_err(|_| CodecError::Corrupt)?;
    for _ in 0..edge_count {
        let id = reader.u64()?;
        let source = reader.u32()?;
        let target = reader.u32()?;
        let relationship = reader.u32()?;
        let key = reader.optional_string()?;
        let revision = reader.u64()?;
        let deadline = reader.optional_u64()?;
        let properties = reader.properties(&property_names)?;
        let source = nodes
            .get(usize::try_from(source).map_err(|_| CodecError::Corrupt)?)
            .ok_or(CodecError::Corrupt)?
            .id;
        let target = nodes
            .get(usize::try_from(target).map_err(|_| CodecError::Corrupt)?)
            .ok_or(CodecError::Corrupt)?
            .id;
        edges.push(SourceEdge {
            id,
            source,
            target,
            relationship: relationships
                .get(usize::try_from(relationship).map_err(|_| CodecError::Corrupt)?)
                .ok_or(CodecError::Corrupt)?
                .clone(),
            key,
            revision,
            deadline,
            properties,
            segments: None,
        });
    }
    if edges
        .windows(2)
        .any(|pair: &[SourceEdge]| pair[0].id >= pair[1].id)
    {
        return Err(CodecError::Corrupt);
    }
    let out = reader.csr()?;
    let incoming = reader.csr()?;
    let label_postings = reader.label_postings(labels.len(), node_count)?;
    let attachments = reader.attachments(&mut nodes)?;
    let temporal_enabled = if reader.is_empty() {
        false
    } else {
        if reader.take(TEMPORAL_SECTION.len())? != TEMPORAL_SECTION {
            return Err(CodecError::Corrupt);
        }
        let temporal_count = reader.entries(8)?;
        if temporal_count > edges.len() {
            return Err(CodecError::Corrupt);
        }
        let mut previous = None;
        for _ in 0..temporal_count {
            let ordinal = usize::try_from(reader.u32()?).map_err(|_| CodecError::Corrupt)?;
            if previous.is_some_and(|prior| prior >= ordinal) {
                return Err(CodecError::Corrupt);
            }
            previous = Some(ordinal);
            let edge = edges.get_mut(ordinal).ok_or(CodecError::Corrupt)?;
            if !edge.properties.is_empty() {
                return Err(CodecError::Corrupt);
            }
            let segment_count = reader.entries(6)?;
            if segment_count > MAX_SEGMENTS {
                return Err(CodecError::Corrupt);
            }
            let mut segments = Vec::new();
            segments
                .try_reserve_exact(segment_count)
                .map_err(|_| CodecError::Corrupt)?;
            for _ in 0..segment_count {
                let segment = Segment {
                    valid_from: reader.optional_i64()?,
                    valid_to: reader.optional_i64()?,
                    properties: reader.properties(&property_names)?,
                };
                segments.push(segment);
            }
            edge.segments = Some(segments);
        }
        true
    };
    if !reader.is_empty() {
        return Err(CodecError::Corrupt);
    }
    let mut graph = build(GraphSource {
        graph_id,
        name,
        sequence,
        next_node_id,
        next_edge_id,
        temporal_enabled,
        schema,
        nodes,
        edges,
    })?;
    if graph.labels != labels
        || graph.relationships != relationships
        || graph.property_names != property_names
        || graph.out != out
        || graph.incoming != incoming
        || graph.label_postings != label_postings
        || graph.attachment_owners != attachments
    {
        return Err(CodecError::Corrupt);
    }
    graph.label_postings = label_postings;
    Ok(graph)
}

fn build(source: GraphSource) -> Result<CompactGraph, CodecError> {
    build_inner(source, true)
}

fn build_inner(
    mut source: GraphSource,
    validate_persisted_metadata: bool,
) -> Result<CompactGraph, CodecError> {
    source.schema.validate().map_err(|_| CodecError::Corrupt)?;
    let mut property_names = BTreeSet::new();
    for node in &source.nodes {
        properties::validate(&node.properties).map_err(|_| CodecError::Corrupt)?;
        property_names.extend(node.properties.keys().cloned());
    }
    let mut has_temporal_series = false;
    for edge in &source.edges {
        properties::validate(&edge.properties).map_err(|_| CodecError::Corrupt)?;
        property_names.extend(edge.properties.keys().cloned());
        if let Some(segments) = &edge.segments {
            if !source.temporal_enabled
                || !edge.properties.is_empty()
                || segments.len() > MAX_SEGMENTS
            {
                return Err(CodecError::Corrupt);
            }
            has_temporal_series = true;
            for segment in segments {
                properties::validate(&segment.properties).map_err(|_| CodecError::Corrupt)?;
                property_names.extend(segment.properties.keys().cloned());
            }
        }
    }
    source.nodes.sort_by_key(|node| node.id);
    source.edges.sort_by_key(|edge| edge.id);
    if source.nodes.len() > u32::MAX as usize || source.edges.len() > u32::MAX as usize {
        return Err(CodecError::Corrupt);
    }
    if source.graph_id >= 64 || !valid_graph_name(&source.name) {
        return Err(CodecError::Corrupt);
    }
    if validate_persisted_metadata {
        let last_node = source.nodes.iter().map(|node| node.id).max();
        let last_edge = source.edges.iter().map(|edge| edge.id).max();
        if last_node.is_some_and(|id| {
            id.checked_add(1)
                .is_none_or(|next| source.next_node_id < next)
        }) || last_edge.is_some_and(|id| {
            id.checked_add(1)
                .is_none_or(|next| source.next_edge_id < next)
        }) || source
            .nodes
            .iter()
            .any(|node| node.revision > source.sequence)
            || source
                .edges
                .iter()
                .any(|edge| edge.revision > source.sequence)
        {
            return Err(CodecError::Corrupt);
        }
    }
    let mut node_ordinals = BTreeMap::new();
    let mut label_set = BTreeSet::new();
    let mut node_keys = BTreeSet::new();
    for (ordinal, node) in source.nodes.iter_mut().enumerate() {
        if node_ordinals
            .insert(
                node.id,
                u32::try_from(ordinal).map_err(|_| CodecError::Corrupt)?,
            )
            .is_some()
        {
            return Err(CodecError::Corrupt);
        }
        if node
            .key
            .as_ref()
            .is_some_and(|key| !node_keys.insert(key.as_str()))
        {
            return Err(CodecError::Corrupt);
        }
        if node.labels.len() > 8
            || node
                .labels
                .iter()
                .any(|label| label.is_empty() || label.len() > 63)
            || node.key.as_ref().is_some_and(|key| !valid_key(key))
        {
            return Err(CodecError::Corrupt);
        }
        node.labels.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        if node.labels.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(CodecError::Corrupt);
        }
        label_set.extend(node.labels.iter().cloned());
    }
    let labels: Vec<_> = label_set.into_iter().collect();
    let mut label_postings = vec![Vec::new(); labels.len()];
    for (ordinal, node) in source.nodes.iter().enumerate() {
        for label in &node.labels {
            let code = labels
                .binary_search(label)
                .map_err(|_| CodecError::Corrupt)?;
            label_postings[code].push(u32::try_from(ordinal).map_err(|_| CodecError::Corrupt)?);
        }
    }
    let mut attachment_owners = BTreeMap::new();
    for node in &source.nodes {
        for (collection, record) in &node.attachments {
            if attachment_owners
                .insert((*collection, *record), node.id)
                .is_some()
            {
                return Err(CodecError::Corrupt);
            }
        }
    }
    let mut relationship_set = BTreeSet::new();
    for edge in &source.edges {
        if !node_ordinals.contains_key(&edge.source) || !node_ordinals.contains_key(&edge.target) {
            return Err(CodecError::Corrupt);
        }
        if edge.relationship.is_empty()
            || edge.relationship.len() > 63
            || edge.key.as_ref().is_some_and(|key| !valid_key(key))
        {
            return Err(CodecError::Corrupt);
        }
        relationship_set.insert(edge.relationship.clone());
    }
    if source.edges.windows(2).any(|pair| pair[0].id == pair[1].id) {
        return Err(CodecError::Corrupt);
    }
    let relationships: Vec<_> = relationship_set.into_iter().collect();
    let property_names: Vec<_> = property_names.into_iter().collect();
    let mut edge_keys = BTreeSet::new();
    if source.edges.iter().any(|edge| {
        edge.key
            .as_ref()
            .is_some_and(|key| !edge_keys.insert(key.as_str()))
    }) {
        return Err(CodecError::Corrupt);
    }
    let relationship_codes: BTreeMap<_, _> = relationships
        .iter()
        .enumerate()
        .map(|(index, relationship)| {
            Ok((
                relationship.as_str(),
                u32::try_from(index).map_err(|_| CodecError::Corrupt)?,
            ))
        })
        .collect::<Result<_, CodecError>>()?;
    let mut edge_rows = Vec::with_capacity(source.edges.len());
    let mut out_rows = vec![Vec::new(); source.nodes.len()];
    let mut in_rows = vec![Vec::new(); source.nodes.len()];
    for (edge_ordinal, edge) in source.edges.iter().enumerate() {
        let row = EdgeRow {
            id: edge.id,
            source: node_ordinals[&edge.source],
            target: node_ordinals[&edge.target],
            relationship: relationship_codes[edge.relationship.as_str()],
        };
        let ordinal = u32::try_from(edge_ordinal).map_err(|_| CodecError::Corrupt)?;
        out_rows[row.source as usize].push((row.relationship, edge.id, ordinal));
        in_rows[row.target as usize].push((row.relationship, edge.id, ordinal));
        edge_rows.push(row);
    }
    let out = make_csr(out_rows)?;
    let incoming = make_csr(in_rows)?;
    if !source.schema.is_empty() || has_temporal_series {
        // Declared rules and the derived indexes stay owned by the engine.
        engine_state(&source)?;
    }
    Ok(CompactGraph {
        source,
        labels,
        relationships,
        property_names,
        label_postings,
        attachment_owners,
        node_ordinals,
        edge_rows,
        out,
        incoming,
    })
}

fn valid_key(key: &str) -> bool {
    (1..=256).contains(&key.len())
}

fn valid_graph_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=63).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(byte))
}

fn make_csr(mut rows: Vec<Vec<(u32, u64, u32)>>) -> Result<Csr, CodecError> {
    let mut offsets = Vec::with_capacity(rows.len() + 1);
    let mut edges = Vec::new();
    offsets.push(0);
    for row in &mut rows {
        row.sort_unstable();
        edges.extend(row.iter().map(|(_, _, ordinal)| *ordinal));
        offsets.push(u32::try_from(edges.len()).map_err(|_| CodecError::Corrupt)?);
    }
    Ok(Csr { offsets, edges })
}

fn put_csr(out: &mut Vec<u8>, csr: &Csr) -> Result<(), CodecError> {
    put_count(out, csr.offsets.len())?;
    for offset in &csr.offsets {
        put_u32(out, *offset)?;
    }
    put_count(out, csr.edges.len())?;
    for edge in &csr.edges {
        put_u32(out, *edge)?;
    }
    Ok(())
}

fn put_properties(
    out: &mut Vec<u8>,
    values: &Properties,
    dictionary: &[String],
) -> Result<(), CodecError> {
    put_count(out, values.len())?;
    for (name, value) in values.iter() {
        let code = dictionary
            .binary_search(name)
            .map_err(|_| CodecError::Corrupt)?;
        put_u32(out, u32::try_from(code).map_err(|_| CodecError::Corrupt)?)?;
        match value {
            PropertyValue::String(value) => {
                put_bytes(out, &[1])?;
                put_string(out, value)?;
            }
            PropertyValue::Integer(value) => {
                put_bytes(out, &[2])?;
                put_bytes(out, &value.to_le_bytes())?;
            }
            PropertyValue::Float(value) => {
                put_bytes(out, &[3])?;
                put_bytes(out, &value.to_le_bytes())?;
            }
            PropertyValue::Boolean(value) => put_bytes(out, &[4, u8::from(*value)])?,
            PropertyValue::GeoPoint {
                latitude,
                longitude,
            } => {
                put_bytes(out, &[5])?;
                put_bytes(out, &latitude.to_le_bytes())?;
                put_bytes(out, &longitude.to_le_bytes())?;
            }
        }
    }
    Ok(())
}

/// Writes declarations in map order without validating them; `build`
/// validates every schema it accepts.
fn put_schema(out: &mut Vec<u8>, schema: &GraphSchema) -> Result<(), CodecError> {
    put_count(out, schema.labels.len())?;
    for (name, label) in &schema.labels {
        put_string(out, name)?;
        put_string(out, &label.description)?;
        put_property_definitions(out, &label.properties)?;
    }
    put_count(out, schema.relationships.len())?;
    for (name, relationship) in &schema.relationships {
        put_string(out, name)?;
        put_string(out, &relationship.description)?;
        let multiplicity = match relationship.multiplicity {
            Multiplicity::Multi => 1,
            Multiplicity::UniqueTriple => 2,
        };
        put_bytes(out, &[multiplicity])?;
        put_count(out, relationship.endpoints.len())?;
        for (source, destination) in &relationship.endpoints {
            put_string(out, source)?;
            put_string(out, destination)?;
        }
        put_property_definitions(out, &relationship.properties)?;
    }
    Ok(())
}

/// Type codes and flag bits match the engine's own schema payload.
fn put_property_definitions(
    out: &mut Vec<u8>,
    properties: &BTreeMap<String, PropertyDefinition>,
) -> Result<(), CodecError> {
    put_count(out, properties.len())?;
    for (name, definition) in properties {
        put_string(out, name)?;
        let value_type = match definition.value_type {
            PropertyType::String => 1,
            PropertyType::Integer => 2,
            PropertyType::Float => 3,
            PropertyType::Boolean => 4,
            PropertyType::GeoPoint => 5,
        };
        let index = match definition.index {
            None => 0,
            Some(PropertyIndexKind::Exact) => 4,
            Some(PropertyIndexKind::Range) => 8,
        };
        let flags = u8::from(definition.required) | (u8::from(definition.unique) << 1) | index;
        put_bytes(out, &[value_type, flags])?;
    }
    Ok(())
}

fn put_strings(out: &mut Vec<u8>, values: &[String]) -> Result<(), CodecError> {
    put_count(out, values.len())?;
    for value in values {
        put_string(out, value)?;
    }
    Ok(())
}

fn put_string(out: &mut Vec<u8>, value: &str) -> Result<(), CodecError> {
    put_count(out, value.len())?;
    put_bytes(out, value.as_bytes())
}

fn put_optional_string(out: &mut Vec<u8>, value: Option<&str>) -> Result<(), CodecError> {
    match value {
        Some(value) => {
            put_bytes(out, &[1])?;
            put_string(out, value)
        }
        None => put_bytes(out, &[0]),
    }
}

fn put_optional_u64(out: &mut Vec<u8>, value: Option<u64>) -> Result<(), CodecError> {
    match value {
        Some(value) => {
            put_bytes(out, &[1])?;
            put_u64(out, value)
        }
        None => put_bytes(out, &[0]),
    }
}

fn put_optional_i64(out: &mut Vec<u8>, value: Option<i64>) -> Result<(), CodecError> {
    match value {
        Some(value) => {
            put_bytes(out, &[1])?;
            put_bytes(out, &value.to_le_bytes())
        }
        None => put_bytes(out, &[0]),
    }
}

fn put_count(out: &mut Vec<u8>, count: usize) -> Result<(), CodecError> {
    put_u32(out, u32::try_from(count).map_err(|_| CodecError::Corrupt)?)
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), CodecError> {
    let end = out
        .len()
        .checked_add(bytes.len())
        .filter(|end| *end <= MAX_BYTES)
        .ok_or(CodecError::Corrupt)?;
    out.try_reserve(bytes.len())
        .map_err(|_| CodecError::Corrupt)?;
    out.extend_from_slice(bytes);
    debug_assert_eq!(out.len(), end);
    Ok(())
}

fn put_u16(out: &mut Vec<u8>, value: u16) -> Result<(), CodecError> {
    put_bytes(out, &value.to_le_bytes())
}

fn put_u32(out: &mut Vec<u8>, value: u32) -> Result<(), CodecError> {
    put_bytes(out, &value.to_le_bytes())
}

fn put_u64(out: &mut Vec<u8>, value: u64) -> Result<(), CodecError> {
    put_bytes(out, &value.to_le_bytes())
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], CodecError> {
        let end = self.at.checked_add(len).ok_or(CodecError::Corrupt)?;
        let bytes = self.bytes.get(self.at..end).ok_or(CodecError::Corrupt)?;
        self.at = end;
        Ok(bytes)
    }

    fn u8(&mut self) -> Result<u8, CodecError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, CodecError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, CodecError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, CodecError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn count(&mut self) -> Result<usize, CodecError> {
        let count = usize::try_from(self.u32()?).map_err(|_| CodecError::Corrupt)?;
        if count > self.bytes.len().saturating_sub(self.at) {
            return Err(CodecError::Corrupt);
        }
        Ok(count)
    }

    fn entries(&mut self, minimum_bytes: usize) -> Result<usize, CodecError> {
        let count = self.count()?;
        if count > self.bytes.len().saturating_sub(self.at) / minimum_bytes {
            return Err(CodecError::Corrupt);
        }
        Ok(count)
    }

    fn string(&mut self, max_len: usize) -> Result<String, CodecError> {
        let len = self.count()?;
        if len > max_len {
            return Err(CodecError::Corrupt);
        }
        let bytes = self.take(len)?;
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(bytes.len())
            .map_err(|_| CodecError::Corrupt)?;
        owned.extend_from_slice(bytes);
        String::from_utf8(owned).map_err(|_| CodecError::Corrupt)
    }

    fn strings(&mut self, max_len: usize) -> Result<Vec<String>, CodecError> {
        let count = self.entries(4)?;
        let mut strings = Vec::new();
        strings
            .try_reserve_exact(count)
            .map_err(|_| CodecError::Corrupt)?;
        for _ in 0..count {
            strings.push(self.string(max_len)?);
        }
        if strings
            .windows(2)
            .any(|pair| pair[0].as_bytes() >= pair[1].as_bytes())
        {
            return Err(CodecError::Corrupt);
        }
        Ok(strings)
    }

    /// Declarations in strictly ascending order within the engine schema
    /// limits; counts are checked against the remaining bytes before any
    /// entry is read.
    fn schema(&mut self) -> Result<GraphSchema, CodecError> {
        let mut schema = GraphSchema::default();
        let label_count = self.entries(12)?;
        if label_count > MAX_SCHEMA_LABELS {
            return Err(CodecError::Corrupt);
        }
        for _ in 0..label_count {
            let name = self.string(MAX_LABEL_BYTES)?;
            if schema
                .labels
                .last_key_value()
                .is_some_and(|(last, _)| *last >= name)
            {
                return Err(CodecError::Corrupt);
            }
            let description = self.string(MAX_SCHEMA_DESCRIPTION_BYTES)?;
            let properties = self.property_definitions()?;
            schema.labels.insert(
                name,
                LabelDefinition {
                    description,
                    properties,
                },
            );
        }
        let relationship_count = self.entries(17)?;
        if relationship_count > MAX_SCHEMA_RELATIONSHIPS {
            return Err(CodecError::Corrupt);
        }
        for _ in 0..relationship_count {
            let name = self.string(MAX_RELATIONSHIP_TYPE_BYTES)?;
            if schema
                .relationships
                .last_key_value()
                .is_some_and(|(last, _)| *last >= name)
            {
                return Err(CodecError::Corrupt);
            }
            let description = self.string(MAX_SCHEMA_DESCRIPTION_BYTES)?;
            let multiplicity = match self.u8()? {
                1 => Multiplicity::Multi,
                2 => Multiplicity::UniqueTriple,
                _ => return Err(CodecError::Corrupt),
            };
            let endpoint_count = self.entries(8)?;
            if endpoint_count > MAX_SCHEMA_ENDPOINTS {
                return Err(CodecError::Corrupt);
            }
            let mut endpoints = BTreeSet::new();
            for _ in 0..endpoint_count {
                let endpoint = (self.string(MAX_LABEL_BYTES)?, self.string(MAX_LABEL_BYTES)?);
                if endpoints.last().is_some_and(|last| *last >= endpoint) {
                    return Err(CodecError::Corrupt);
                }
                endpoints.insert(endpoint);
            }
            let properties = self.property_definitions()?;
            schema.relationships.insert(
                name,
                RelationshipDefinition {
                    description,
                    endpoints,
                    properties,
                    multiplicity,
                },
            );
        }
        Ok(schema)
    }

    fn property_definitions(&mut self) -> Result<BTreeMap<String, PropertyDefinition>, CodecError> {
        let count = self.entries(6)?;
        if count > MAX_SCHEMA_PROPERTIES {
            return Err(CodecError::Corrupt);
        }
        let mut properties = BTreeMap::new();
        for _ in 0..count {
            let name = self.string(MAX_FIELD_NAME_BYTES)?;
            if properties
                .last_key_value()
                .is_some_and(|(last, _): (&String, _)| *last >= name)
            {
                return Err(CodecError::Corrupt);
            }
            let value_type = match self.u8()? {
                1 => PropertyType::String,
                2 => PropertyType::Integer,
                3 => PropertyType::Float,
                4 => PropertyType::Boolean,
                5 => PropertyType::GeoPoint,
                _ => return Err(CodecError::Corrupt),
            };
            let flags = self.u8()?;
            let index = match flags & !3 {
                0 => None,
                4 => Some(PropertyIndexKind::Exact),
                8 => Some(PropertyIndexKind::Range),
                _ => return Err(CodecError::Corrupt),
            };
            properties.insert(
                name,
                PropertyDefinition {
                    value_type,
                    required: flags & 1 != 0,
                    unique: flags & 2 != 0,
                    index,
                },
            );
        }
        Ok(properties)
    }

    fn optional_string(&mut self) -> Result<Option<String>, CodecError> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.string(256)?)),
            _ => Err(CodecError::Corrupt),
        }
    }

    fn optional_u64(&mut self) -> Result<Option<u64>, CodecError> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.u64()?)),
            _ => Err(CodecError::Corrupt),
        }
    }

    fn optional_i64(&mut self) -> Result<Option<i64>, CodecError> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(i64::from_le_bytes(self.array()?))),
            _ => Err(CodecError::Corrupt),
        }
    }

    fn properties(&mut self, dictionary: &[String]) -> Result<Properties, CodecError> {
        let count = self.entries(5)?;
        if count > 64 {
            return Err(CodecError::Corrupt);
        }
        let mut properties = Properties::new();
        let mut previous = None;
        for _ in 0..count {
            let code = self.u32()?;
            if previous.is_some_and(|prior| code <= prior) {
                return Err(CodecError::Corrupt);
            }
            previous = Some(code);
            let name = dictionary
                .get(usize::try_from(code).map_err(|_| CodecError::Corrupt)?)
                .ok_or(CodecError::Corrupt)?
                .clone();
            let value = match self.u8()? {
                1 => PropertyValue::String(self.string(4_096)?),
                2 => PropertyValue::Integer(i64::from_le_bytes(self.array()?)),
                3 => PropertyValue::Float(f64::from_le_bytes(self.array()?)),
                4 => match self.u8()? {
                    0 => PropertyValue::Boolean(false),
                    1 => PropertyValue::Boolean(true),
                    _ => return Err(CodecError::Corrupt),
                },
                5 => PropertyValue::GeoPoint {
                    latitude: f64::from_le_bytes(self.array()?),
                    longitude: f64::from_le_bytes(self.array()?),
                },
                _ => return Err(CodecError::Corrupt),
            };
            properties.insert(name, value);
        }
        properties::validate(&properties).map_err(|_| CodecError::Corrupt)?;
        Ok(properties)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], CodecError> {
        self.take(N)?.try_into().map_err(|_| CodecError::Corrupt)
    }

    fn csr(&mut self) -> Result<Csr, CodecError> {
        let offset_count = self.entries(4)?;
        let mut offsets = Vec::new();
        offsets
            .try_reserve_exact(offset_count)
            .map_err(|_| CodecError::Corrupt)?;
        for _ in 0..offset_count {
            offsets.push(self.u32()?);
        }
        let edge_count = self.entries(4)?;
        let mut edges = Vec::new();
        edges
            .try_reserve_exact(edge_count)
            .map_err(|_| CodecError::Corrupt)?;
        for _ in 0..edge_count {
            edges.push(self.u32()?);
        }
        Ok(Csr { offsets, edges })
    }

    fn label_postings(
        &mut self,
        label_count: usize,
        node_count: usize,
    ) -> Result<Vec<Vec<u32>>, CodecError> {
        let count = self.entries(8)?;
        if count != label_count {
            return Err(CodecError::Corrupt);
        }
        let mut postings = Vec::new();
        postings
            .try_reserve_exact(count)
            .map_err(|_| CodecError::Corrupt)?;
        for code in 0..count {
            if usize::try_from(self.u32()?).map_err(|_| CodecError::Corrupt)? != code {
                return Err(CodecError::Corrupt);
            }
            let members = self.entries(4)?;
            if members == 0 || members > node_count {
                return Err(CodecError::Corrupt);
            }
            let mut ordinals = Vec::new();
            ordinals
                .try_reserve_exact(members)
                .map_err(|_| CodecError::Corrupt)?;
            for _ in 0..members {
                let ordinal = self.u32()?;
                if usize::try_from(ordinal).map_err(|_| CodecError::Corrupt)? >= node_count
                    || ordinals.last().is_some_and(|previous| *previous >= ordinal)
                {
                    return Err(CodecError::Corrupt);
                }
                ordinals.push(ordinal);
            }
            postings.push(ordinals);
        }
        Ok(postings)
    }

    fn attachments(
        &mut self,
        nodes: &mut [SourceNode],
    ) -> Result<BTreeMap<(u16, u64), u64>, CodecError> {
        let count = self.entries(14)?;
        let mut owners = BTreeMap::new();
        let mut previous = None;
        for _ in 0..count {
            let ordinal = self.u32()?;
            let ordinal_index = usize::try_from(ordinal).map_err(|_| CodecError::Corrupt)?;
            let node = nodes.get_mut(ordinal_index).ok_or(CodecError::Corrupt)?;
            let collection = self.u16()?;
            let record = self.u64()?;
            let key = (ordinal, collection);
            if previous.is_some_and(|prior| prior >= key)
                || node.attachments.insert(collection, record).is_some()
                || owners.insert((collection, record), node.id).is_some()
            {
                return Err(CodecError::Corrupt);
            }
            previous = Some(key);
        }
        Ok(owners)
    }

    fn is_empty(&self) -> bool {
        self.at == self.bytes.len()
    }
}

#[cfg(test)]
fn logical_value(graph: &CompactGraph, case: &serde_json::Value) -> serde_json::Value {
    use serde_json::json;

    let clock = case["clock"].as_u64().unwrap();
    let live_nodes: Vec<_> = graph
        .source
        .nodes
        .iter()
        .filter(|node| node.deadline.is_none_or(|deadline| deadline > clock))
        .collect();
    let ordinals: BTreeMap<_, _> = live_nodes
        .iter()
        .enumerate()
        .map(|(ordinal, node)| (node.id, u32::try_from(ordinal).unwrap()))
        .collect();
    let live_edges: Vec<_> = graph
        .source
        .edges
        .iter()
        .filter(|edge| {
            edge.deadline.is_none_or(|deadline| deadline > clock)
                && ordinals.contains_key(&edge.source)
                && ordinals.contains_key(&edge.target)
        })
        .collect();
    let relationships: Vec<_> = live_edges
        .iter()
        .map(|edge| edge.relationship.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let labels: Vec<_> = live_nodes
        .iter()
        .flat_map(|node| node.labels.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let relationship_codes: BTreeMap<_, _> = relationships
        .iter()
        .enumerate()
        .map(|(index, value)| (value.as_str(), index))
        .collect();
    let label_codes: BTreeMap<_, _> = labels
        .iter()
        .enumerate()
        .map(|(index, value)| (value.as_str(), index))
        .collect();
    let edge_ordinals: BTreeMap<_, _> = live_edges
        .iter()
        .enumerate()
        .map(|(index, edge)| (edge.id, u32::try_from(index).unwrap()))
        .collect();
    let node_map: Vec<_> = live_nodes
        .iter()
        .enumerate()
        .map(|(ordinal, node)| {
            let codes: Vec<_> = node
                .labels
                .iter()
                .map(|label| label_codes[label.as_str()])
                .collect();
            json!({"id": node.id, "ordinal": ordinal, "labels": codes})
        })
        .collect();
    let edge_table: Vec<_> = live_edges
        .iter()
        .enumerate()
        .map(|(ordinal, edge)| {
            json!({
                "id": edge.id,
                "ordinal": ordinal,
                "source": ordinals[&edge.source],
                "target": ordinals[&edge.target],
                "relationship": relationship_codes[edge.relationship.as_str()]
            })
        })
        .collect();
    let csr = |source_side: bool| {
        let mut rows = vec![Vec::new(); live_nodes.len()];
        for edge in &live_edges {
            let node = if source_side {
                edge.source
            } else {
                edge.target
            };
            rows[ordinals[&node] as usize].push(edge);
        }
        let mut offsets = vec![0];
        let mut neighbors = Vec::new();
        for row in &mut rows {
            row.sort_by(|left, right| {
                left.relationship
                    .as_bytes()
                    .cmp(right.relationship.as_bytes())
                    .then(left.id.cmp(&right.id))
            });
            neighbors.extend(row.iter().map(|edge| edge_ordinals[&edge.id]));
            offsets.push(neighbors.len());
        }
        json!({"offsets": offsets, "neighbors": neighbors})
    };
    let adjacency = case["queries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|query| {
            let id = query["node"].as_u64().unwrap();
            let direction = query["direction"].as_str().unwrap();
            let filter = query["relationship"].as_array().map(|values| {
                values
                    .iter()
                    .map(|value| value.as_str().unwrap())
                    .collect::<BTreeSet<_>>()
            });
            let cursor = query["after"].as_object().map(|value| {
                (
                    value["relationship"].as_str().unwrap().as_bytes(),
                    value["edge"].as_u64().unwrap(),
                )
            });
            let mut found: Vec<_> = live_edges
                .iter()
                .filter(|edge| {
                    let incident = match direction {
                        "out" => edge.source == id,
                        "in" => edge.target == id,
                        _ => edge.source == id || edge.target == id,
                    };
                    incident
                        && filter
                            .as_ref()
                            .is_none_or(|values| values.contains(edge.relationship.as_str()))
                        && cursor.is_none_or(|(kind, edge_id)| {
                            (edge.relationship.as_bytes(), edge.id) > (kind, edge_id)
                        })
                })
                .collect();
            found.sort_by(|left, right| {
                left.relationship
                    .as_bytes()
                    .cmp(right.relationship.as_bytes())
                    .then(left.id.cmp(&right.id))
            });
            found.truncate(usize::try_from(query["limit"].as_u64().unwrap()).unwrap_or(usize::MAX));
            found
                .iter()
                .map(|edge| json!([edge.relationship, edge.id]))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    json!({
        "node_map": node_map,
        "relationships": relationships,
        "labels": labels,
        "edge_table": edge_table,
        "out_csr": csr(true),
        "in_csr": csr(false),
        "adjacency": adjacency
    })
}

#[cfg(test)]
fn csr_value(csr: &Csr) -> serde_json::Value {
    serde_json::json!({"offsets": csr.offsets, "neighbors": csr.edges})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::property_graph::engine::limits::{
        MAX_FIELD_NAME_BYTES, MAX_INDEXED_PROPERTIES, MAX_LABEL_BYTES, MAX_RELATIONSHIP_TYPE_BYTES,
        MAX_SCHEMA_DESCRIPTION_BYTES, MAX_SCHEMA_ENDPOINTS, MAX_SCHEMA_LABELS,
        MAX_SCHEMA_PROPERTIES, MAX_SCHEMA_RELATIONSHIPS,
    };
    use crate::property_graph::engine::{
        Direction, EdgeId, EdgeView, ExecutionContext, GraphBody, GraphEntry, GraphError, GraphId,
        GraphLimits, GraphQuery, GraphState, IndexChoice, LabelDefinition, Multiplicity, NodeId,
        NodeSelector, PropertyCondition, PropertyDefinition, PropertyIndexKind, PropertyProjection,
        PropertyType, QueryOp, QueryOutput, QueryResult, RelationshipDefinition, execute,
        execute_as_of,
    };
    use serde_json::{Value, json};
    use std::collections::BTreeSet;

    #[test]
    fn attachments_round_trip_and_rebuild_the_owner_index() {
        let mut source = fixture_graph(&json!({
            "nodes": [{"id": 0, "labels": []}, {"id": 7, "labels": []}],
            "edges": []
        }));
        source.nodes[0].attachments = BTreeMap::from([(0, u64::MAX), (u16::MAX, 0)]);
        source.nodes[1].attachments = BTreeMap::from([(1, 42)]);
        let restored = decode(&encode(&source).unwrap()).unwrap();
        assert_eq!(restored.source, source);
        assert_eq!(
            restored.attachment_owners,
            BTreeMap::from([((0, u64::MAX), 0), ((1, 42), 7), ((u16::MAX, 0), 0)])
        );
        assert_eq!(encode(&restored.source).unwrap(), encode(&source).unwrap());

        let mut state = GraphState::new();
        state
            .apply(
                1,
                &GraphBody {
                    graph_id: GraphId(0),
                    entries: vec![
                        GraphEntry::CreateGraph {
                            name: "attachments".into(),
                        },
                        GraphEntry::PutNode {
                            node_id: NodeId(1),
                            key: "n1".into(),
                            labels: BTreeSet::new(),
                            properties: Properties::new(),
                        },
                        GraphEntry::PutNode {
                            node_id: NodeId(2),
                            key: "n2".into(),
                            labels: BTreeSet::new(),
                            properties: Properties::new(),
                        },
                        GraphEntry::Attach {
                            node_id: NodeId(1),
                            collection: u16::MAX,
                            record_id: u64::MAX,
                        },
                        GraphEntry::Attach {
                            node_id: NodeId(1),
                            collection: 0,
                            record_id: 0,
                        },
                    ],
                },
            )
            .unwrap();
        state
            .apply(
                2,
                &GraphBody {
                    graph_id: GraphId(0),
                    entries: vec![GraphEntry::PutNode {
                        node_id: NodeId(1),
                        key: "n1".into(),
                        labels: BTreeSet::new(),
                        properties: Properties::new(),
                    }],
                },
            )
            .unwrap();
        let graph = state.graph(GraphId(0)).unwrap();
        let compact = decode(&encode(&GraphSource::from_graph(graph).unwrap()).unwrap()).unwrap();
        assert_engine_attachment_parity(&compact, graph);
        assert_eq!(
            compact.attachment_owners,
            BTreeMap::from([((0, 0), 1), ((u16::MAX, u64::MAX), 1)])
        );
        state
            .apply(
                3,
                &GraphBody {
                    graph_id: GraphId(0),
                    entries: vec![
                        GraphEntry::DeleteNode { node_id: NodeId(1) },
                        GraphEntry::Attach {
                            node_id: NodeId(2),
                            collection: u16::MAX,
                            record_id: u64::MAX,
                        },
                    ],
                },
            )
            .unwrap();
        let graph = state.graph(GraphId(0)).unwrap();
        let compact = decode(&encode(&GraphSource::from_graph(graph).unwrap()).unwrap()).unwrap();
        assert_engine_attachment_parity(&compact, graph);
        assert_eq!(
            compact.attachment_owners,
            BTreeMap::from([((u16::MAX, u64::MAX), 2)])
        );
        assert_eq!(
            compact.attachment_owners,
            state
                .attached_records()
                .map(|(collection, record)| {
                    (
                        (collection, record),
                        state.attachment_owner(collection, record).unwrap().1.0,
                    )
                })
                .collect()
        );
        assert_eq!(
            state.attachment_owner(u16::MAX, u64::MAX),
            Some((GraphId(0), NodeId(2)))
        );
    }

    fn assert_engine_attachment_parity(compact: &CompactGraph, graph: &Graph) {
        let expected: BTreeMap<_, _> = graph
            .nodes()
            .map(|node| {
                (
                    node.id.0,
                    graph.attachments(node.id).collect::<BTreeMap<_, _>>(),
                )
            })
            .collect();
        let actual: BTreeMap<_, _> = compact
            .source
            .nodes
            .iter()
            .map(|node| (node.id, node.attachments.clone()))
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn attachment_decoder_rejects_bad_ordinals_order_duplicates_and_bounds() {
        let mut source = fixture_graph(&json!({
            "nodes": [{"id": 0, "labels": []}, {"id": 7, "labels": []}],
            "edges": []
        }));
        source.nodes[0].attachments = BTreeMap::from([(0, u64::MAX), (u16::MAX, 0)]);
        source.nodes[1].attachments = BTreeMap::from([(1, 42)]);
        let bytes = encode(&source).unwrap();
        let start = bytes.len() - 4 - 14 * 3;
        assert_eq!(
            u32::from_le_bytes(bytes[start..start + 4].try_into().unwrap()),
            3
        );

        let mut bad_ordinal = bytes.clone();
        bad_ordinal[start + 4..start + 8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(decode(&bad_ordinal), Err(CodecError::Corrupt));
        let mut oversized_count = bytes.clone();
        oversized_count[start..start + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(decode(&oversized_count), Err(CodecError::Corrupt));

        let mut duplicate_collection = bytes.clone();
        duplicate_collection[start + 18 + 4..start + 18 + 6].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(decode(&duplicate_collection), Err(CodecError::Corrupt));
        let mut unsorted_collection = bytes.clone();
        unsorted_collection[start + 8..start + 10].copy_from_slice(&u16::MAX.to_le_bytes());
        unsorted_collection[start + 18 + 4..start + 18 + 6].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(decode(&unsorted_collection), Err(CodecError::Corrupt));
        let mut unsorted_ordinal = bytes.clone();
        unsorted_ordinal[start + 4..].rotate_right(14);
        assert_eq!(decode(&unsorted_ordinal), Err(CodecError::Corrupt));
        let mut duplicate_owner = bytes.clone();
        duplicate_owner[start + 32 + 4..start + 32 + 6].copy_from_slice(&0u16.to_le_bytes());
        duplicate_owner[start + 32 + 6..start + 32 + 14].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(decode(&duplicate_owner), Err(CodecError::Corrupt));
        for end in start..bytes.len() {
            assert_eq!(decode(&bytes[..end]), Err(CodecError::Corrupt));
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert_eq!(decode(&trailing), Err(CodecError::Corrupt));
        for magic in [
            b"VLGCSR01",
            b"VLGCSR02",
            b"VLGCSR03",
            b"VLGCSR04",
            b"VLGCSR05",
        ] {
            let mut old = bytes.clone();
            old[..8].copy_from_slice(magic);
            assert_eq!(decode(&old), Err(CodecError::Corrupt));
        }
    }

    #[test]
    fn label_postings_round_trip_and_queries_preserve_order_and_deadlines() {
        let mut source = fixture_graph(&json!({
            "nodes": [
                {"id": 50, "labels": ["B", "A"]},
                {"id": 0, "labels": ["A"]},
                {"id": 7, "labels": ["A", "B"]},
                {"id": 12, "labels": []}
            ], "edges": []
        }));
        source.nodes[0].deadline = Some(0);
        let graph = decode(&encode(&source).unwrap()).unwrap();
        assert_eq!(graph.label_postings, vec![vec![0, 1, 3], vec![1, 3]]);
        assert_eq!(graph.label_scan("A", None).collect::<Vec<_>>(), [0, 7, 50]);
        assert_eq!(graph.label_scan("A", Some(0)).collect::<Vec<_>>(), [7, 50]);
        assert_eq!(graph.label_scan("missing", None).count(), 0);
        assert_eq!(graph.label_scan("A", Some(u64::MAX)).count(), 0);
        assert_eq!(
            graph.label_intersection(&["B", "A", "A"], None, 1),
            Ok(vec![7])
        );
        assert_eq!(
            graph.label_intersection(&["A", "B"], Some(7), 10),
            Ok(vec![50])
        );
        assert_eq!(
            graph.label_intersection(&["A", "missing"], None, 10),
            Ok(vec![])
        );
        assert_eq!(
            graph.label_intersection(&[], None, 10),
            Err(CodecError::Corrupt)
        );
        assert_eq!(
            graph.label_intersection(&["A"], None, 0),
            Err(CodecError::Corrupt)
        );
        assert_eq!(
            graph.label_intersection(&["A", "B", "C", "D", "E", "F", "G", "H", "I"], None, 1),
            Err(CodecError::Corrupt)
        );
        source.nodes.reverse();
        assert_eq!(encode(&source).unwrap(), encode(&graph.source).unwrap());
        let empty =
            decode(&encode(&fixture_graph(&json!({"nodes": [], "edges": []}))).unwrap()).unwrap();
        assert!(empty.label_postings.is_empty());

        source.nodes.push(SourceNode {
            id: u64::MAX,
            labels: vec!["A".into(), "B".into()],
            ..source.nodes[0].clone()
        });
        let logical = build_inner(source.clone(), false).unwrap();
        assert_eq!(
            logical.label_scan("A", Some(50)).collect::<Vec<_>>(),
            [u64::MAX]
        );
        assert_eq!(
            logical.label_intersection(&["A", "B"], Some(50), 10),
            Ok(vec![u64::MAX])
        );
        assert_eq!(logical.label_scan("A", Some(u64::MAX)).count(), 0);
        assert_eq!(
            logical.label_scan("A", None).collect::<Vec<_>>(),
            [0, 7, 50, u64::MAX]
        );
        assert_eq!(
            logical.label_intersection(&["A", "B"], Some(u64::MAX), 10),
            Ok(vec![])
        );
        assert_eq!(encode(&source), Err(CodecError::Corrupt));
    }

    #[test]
    fn decoder_rejects_label_posting_corruption_and_old_versions() {
        let source = fixture_graph(&json!({
            "nodes": [
                {"id": 0, "labels": ["A"]},
                {"id": 7, "labels": ["A", "B"]},
                {"id": 9, "labels": ["B"]}
            ], "edges": []
        }));
        let bytes = encode(&source).unwrap();
        // Two explicit (code, count, ordinals) rows, preceded by row count.
        let start = bytes.len() - 4 - 36;
        assert_eq!(
            u32::from_le_bytes(bytes[start..start + 4].try_into().unwrap()),
            2
        );
        for (offset, value) in [
            (start, u32::MAX),
            (start + 4, 2),        // out-of-range code
            (start + 20, 0),       // duplicate code
            (start + 8, u32::MAX), // excessive count
            (start + 16, 0),       // duplicate ordinal
            (start + 12, 2),       // descending ordinals
            (start + 16, 3),       // out-of-range ordinal
            (start + 16, 2),       // canonical but wrong membership (extra/missing)
        ] {
            let mut corrupt = bytes.clone();
            corrupt[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            assert_eq!(
                decode(&corrupt),
                Err(CodecError::Corrupt),
                "offset {offset}, value {value}"
            );
        }
        let mut reversed_codes = bytes.clone();
        reversed_codes[start + 4..start + 8].copy_from_slice(&1u32.to_le_bytes());
        reversed_codes[start + 20..start + 24].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(decode(&reversed_codes), Err(CodecError::Corrupt));
        let mut missing = bytes.clone();
        missing[start + 8..start + 12].copy_from_slice(&1u32.to_le_bytes());
        missing.drain(start + 16..start + 20);
        assert_eq!(decode(&missing), Err(CodecError::Corrupt));
        let mut extra = bytes.clone();
        extra[start + 8..start + 12].copy_from_slice(&3u32.to_le_bytes());
        extra.splice(start + 20..start + 20, 2u32.to_le_bytes());
        assert_eq!(decode(&extra), Err(CodecError::Corrupt));
        let mut omitted_label = bytes.clone();
        omitted_label[start..start + 4].copy_from_slice(&1u32.to_le_bytes());
        omitted_label.truncate(start + 20);
        assert_eq!(decode(&omitted_label), Err(CodecError::Corrupt));
        for end in start..bytes.len() {
            assert_eq!(decode(&bytes[..end]), Err(CodecError::Corrupt));
        }
        for magic in [
            b"VLGCSR01",
            b"VLGCSR02",
            b"VLGCSR03",
            b"VLGCSR04",
            b"VLGCSR05",
        ] {
            let mut old = bytes.clone();
            old[..8].copy_from_slice(magic);
            assert_eq!(decode(&old), Err(CodecError::Corrupt));
        }
    }

    #[test]
    fn decoded_label_postings_match_generated_engine_index() {
        let mut state = GraphState::new();
        state
            .apply(
                1,
                &GraphBody {
                    graph_id: GraphId(0),
                    entries: vec![GraphEntry::CreateGraph {
                        name: "labels".into(),
                    }],
                },
            )
            .unwrap();
        let mut entries = Vec::new();
        for index in 0..256u64 {
            let labels = (0..8)
                .filter(|bit| index & (1 << bit) != 0)
                .map(|bit| format!("L{bit}"))
                .collect();
            entries.push(GraphEntry::PutNode {
                node_id: NodeId(index * 17),
                key: format!("node-{index}"),
                labels,
                properties: Default::default(),
            });
            if index % 3 == 0 {
                entries.push(GraphEntry::NodeExpiry {
                    node_id: NodeId(index * 17),
                    expires_at_ms: Some(0),
                });
            }
        }
        state
            .apply(
                2,
                &GraphBody {
                    graph_id: GraphId(0),
                    entries,
                },
            )
            .unwrap();
        let engine = state.graph(GraphId(0)).unwrap();
        let graph = decode(&encode(&GraphSource::from_graph(engine).unwrap()).unwrap()).unwrap();
        assert_eq!(
            graph.label_intersection(&["L0", "L1", "L2", "L3", "L4", "L5", "L6", "L7"], None, 10),
            Ok(vec![255 * 17])
        );
        for label in ["L0", "L1", "L7", "unknown"] {
            for after in [None, Some(0), Some(50), Some(2160), Some(u64::MAX)] {
                assert_eq!(
                    graph.label_scan(label, after).collect::<Vec<_>>(),
                    engine
                        .label_scan(label, after.map(NodeId))
                        .map(|id| id.0)
                        .collect::<Vec<_>>()
                );
                for limit in [1, 5, usize::MAX] {
                    let expected = engine
                        .label_scan(label, after.map(NodeId))
                        .filter(|id| engine.nodes_with_label("L2").any(|other| other == *id))
                        .take(limit)
                        .map(|id| id.0)
                        .collect::<Vec<_>>();
                    assert_eq!(
                        graph.label_intersection(&[label, "L2"], after, limit),
                        Ok(expected)
                    );
                }
            }
        }
    }

    #[test]
    fn csr_codec_matches_the_accepted_logical_oracle() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/compact/graph-csr-cases.json"
        ))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let mut source = fixture_graph(case);
            let clock = case["clock"].as_u64().unwrap();
            source
                .nodes
                .retain(|node| node.deadline.is_none_or(|deadline| deadline > clock));
            let live_node_ids: BTreeSet<_> = source.nodes.iter().map(|node| node.id).collect();
            source.edges.retain(|edge| {
                edge.deadline.is_none_or(|deadline| deadline > clock)
                    && live_node_ids.contains(&edge.source)
                    && live_node_ids.contains(&edge.target)
            });
            let graph = if source.nodes.iter().any(|node| node.id == u64::MAX)
                || source.edges.iter().any(|edge| edge.id == u64::MAX)
            {
                // The logical oracle deliberately uses u64::MAX with placeholder
                // high-water metadata. Exercise only its logical CSR shape;
                // these rows cannot describe a valid persisted graph.
                build_inner(source, false).unwrap()
            } else {
                decode(&encode(&source).unwrap()).unwrap()
            };
            assert_eq!(
                logical_value(&graph, case),
                case["expected"],
                "{}",
                case["name"]
            );
            assert_eq!(
                csr_value(&graph.out),
                case["expected"]["out_csr"],
                "{} out CSR",
                case["name"]
            );
            assert_eq!(
                csr_value(&graph.incoming),
                case["expected"]["in_csr"],
                "{} in CSR",
                case["name"]
            );
        }
    }

    #[test]
    fn codec_rejects_noncanonical_or_incomplete_csr() {
        let graph = fixture_graph(&json!({
            "nodes": [{"id": 1, "labels": []}, {"id": 2, "labels": []}],
            "edges": [{"id": 3, "source": 1, "target": 2, "relationship": "r"}],
        }));
        let bytes = encode(&graph).unwrap();
        let mut truncated = bytes.clone();
        truncated.truncate(truncated.len() - 1);
        assert_eq!(decode(&truncated), Err(CodecError::Corrupt));

        let mut invalid_membership = bytes.clone();
        let incoming_member_end = invalid_membership.len() - 4;
        invalid_membership[incoming_member_end - 1] = 1;
        assert_eq!(decode(&invalid_membership), Err(CodecError::Corrupt));

        let mut trailing = bytes;
        trailing.push(0);
        assert_eq!(decode(&trailing), Err(CodecError::Corrupt));
    }

    #[test]
    fn codec_rejects_high_water_below_live_id_and_revision_after_sequence() {
        let source = identity_source();
        let bytes = encode(&source).unwrap();

        let mut stale_node_high_water = bytes.clone();
        stale_node_high_water[23..31].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(decode(&stale_node_high_water), Err(CodecError::Corrupt));

        let mut stale_edge_high_water = bytes.clone();
        stale_edge_high_water[31..39].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(decode(&stale_edge_high_water), Err(CodecError::Corrupt));

        let mut future_revision = bytes;
        future_revision[15..23].copy_from_slice(&1u64.to_le_bytes());
        assert_eq!(decode(&future_revision), Err(CodecError::Corrupt));
    }

    #[test]
    fn decoder_rejects_duplicate_present_node_and_edge_keys() {
        let mut source = fixture_graph(&json!({
            "nodes": [{"id": 1, "labels": []}, {"id": 2, "labels": []}],
            "edges": [
                {"id": 3, "source": 1, "target": 2, "relationship": "r"},
                {"id": 4, "source": 2, "target": 1, "relationship": "r"}
            ],
        }));
        source.nodes[0].key = Some("a".into());
        source.nodes[1].key = Some("b".into());
        source.edges[0].key = Some("a".into());
        source.edges[1].key = Some("b".into());
        let bytes = encode(&source).unwrap();

        let mut duplicate_node_key = bytes.clone();
        replace_key(&mut duplicate_node_key, true, 1, "a");
        assert_eq!(decode(&duplicate_node_key), Err(CodecError::Corrupt));

        let mut duplicate_edge_key = bytes;
        replace_key(&mut duplicate_edge_key, false, 1, "a");
        assert_eq!(decode(&duplicate_edge_key), Err(CodecError::Corrupt));
    }

    #[test]
    fn typed_properties_round_trip_in_nodes_and_edges_with_exact_float_bits() {
        let node_properties = Properties::from([
            ("bool".into(), PropertyValue::Boolean(false)),
            ("float".into(), PropertyValue::Float(-0.0)),
            (
                "geo".into(),
                PropertyValue::GeoPoint {
                    latitude: -0.0,
                    longitude: 0.0,
                },
            ),
            ("integer".into(), PropertyValue::Integer(i64::MIN)),
            ("string".into(), PropertyValue::String("typed λ".into())),
        ]);
        let edge_properties = Properties::from([
            ("bool".into(), PropertyValue::Boolean(true)),
            ("float".into(), PropertyValue::Float(1.25)),
            (
                "geo".into(),
                PropertyValue::GeoPoint {
                    latitude: 90.0,
                    longitude: -180.0,
                },
            ),
            ("integer".into(), PropertyValue::Integer(i64::MAX)),
            ("string".into(), PropertyValue::String(String::new())),
        ]);
        let mut state = GraphState::new();
        state
            .apply(
                7,
                &GraphBody {
                    graph_id: GraphId(0),
                    entries: vec![
                        GraphEntry::CreateGraph { name: "g".into() },
                        GraphEntry::PutNode {
                            node_id: NodeId(1),
                            key: "n1".into(),
                            labels: BTreeSet::new(),
                            properties: node_properties,
                        },
                        GraphEntry::PutNode {
                            node_id: NodeId(2),
                            key: "n2".into(),
                            labels: BTreeSet::new(),
                            properties: Properties::new(),
                        },
                        GraphEntry::PutEdge {
                            edge_id: crate::property_graph::engine::EdgeId(3),
                            source: NodeId(1),
                            destination: NodeId(2),
                            relationship: "r".into(),
                            key: None,
                            properties: edge_properties,
                        },
                    ],
                },
            )
            .unwrap();
        let source = GraphSource::from_graph(state.graph(GraphId(0)).unwrap()).unwrap();

        let restored = decode(&encode(&source).unwrap()).unwrap();
        assert_eq!(restored.source, source);
        assert_eq!(
            match restored.source.nodes[0].properties.get("float").unwrap() {
                PropertyValue::Float(value) => value.to_bits(),
                value => panic!("expected float, got {value:?}"),
            },
            (-0.0f64).to_bits()
        );
        assert_eq!(
            match restored.source.nodes[0].properties.get("geo").unwrap() {
                PropertyValue::GeoPoint {
                    latitude,
                    longitude,
                } => (latitude.to_bits(), longitude.to_bits()),
                value => panic!("expected geo point, got {value:?}"),
            },
            ((-0.0f64).to_bits(), 0.0f64.to_bits())
        );
    }

    #[test]
    fn property_dictionary_output_is_deterministic_and_empty_maps_round_trip() {
        let mut source = fixture_graph(&json!({
            "nodes": [{"id": 1, "labels": []}, {"id": 2, "labels": []}],
            "edges": [{"id": 3, "source": 1, "target": 2, "relationship": "r"}],
        }));
        source.nodes[0].properties = Properties::from([
            ("z".into(), PropertyValue::Integer(1)),
            ("a".into(), PropertyValue::Integer(2)),
        ]);
        source.edges[0].properties = Properties::new();
        let expected = encode(&source).unwrap();
        let mut shuffled = source.clone();
        shuffled.nodes.reverse();
        shuffled.edges.reverse();
        assert_eq!(encode(&shuffled).unwrap(), expected);
        let decoded = decode(&expected).unwrap();
        assert_eq!(decoded.source.nodes[1].properties, Properties::new());
        assert_eq!(decoded.source.edges[0].properties, Properties::new());
        assert_eq!(encode(&decoded.source).unwrap(), expected);
    }

    #[test]
    fn decoder_rejects_bad_property_dictionaries_references_tags_and_values() {
        let mut source = fixture_graph(&json!({
            "nodes": [{"id": 1, "labels": []}],
            "edges": [],
        }));
        source.nodes[0].properties = Properties::from([
            ("a".into(), PropertyValue::Integer(1)),
            ("b".into(), PropertyValue::Boolean(true)),
            ("c".into(), PropertyValue::Float(1.0)),
        ]);
        let bytes = encode(&source).unwrap();
        let (dictionary, entries) = property_offsets(&bytes, 0);

        let mut duplicate_dictionary = bytes.clone();
        duplicate_dictionary[dictionary[1].1] = b'a';
        assert_eq!(decode(&duplicate_dictionary), Err(CodecError::Corrupt));

        let mut duplicate_reference = bytes.clone();
        duplicate_reference[entries[1].0..entries[1].0 + 4]
            .copy_from_slice(&entries[0].2.to_le_bytes());
        assert_eq!(decode(&duplicate_reference), Err(CodecError::Corrupt));

        let mut unsorted_references = bytes.clone();
        unsorted_references[entries[0].0..entries[0].0 + 4].copy_from_slice(&2u32.to_le_bytes());
        unsorted_references[entries[1].0..entries[1].0 + 4].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(decode(&unsorted_references), Err(CodecError::Corrupt));

        let mut unknown_reference = bytes.clone();
        unknown_reference[entries[0].0..entries[0].0 + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(decode(&unknown_reference), Err(CodecError::Corrupt));

        let mut unknown_tag = bytes.clone();
        unknown_tag[entries[0].1] = 99;
        assert_eq!(decode(&unknown_tag), Err(CodecError::Corrupt));

        let mut invalid_boolean = bytes.clone();
        invalid_boolean[entries[1].3] = 2;
        assert_eq!(decode(&invalid_boolean), Err(CodecError::Corrupt));

        let mut nonfinite_float = bytes.clone();
        nonfinite_float[entries[2].3..entries[2].3 + 8]
            .copy_from_slice(&f64::INFINITY.to_le_bytes());
        assert_eq!(decode(&nonfinite_float), Err(CodecError::Corrupt));

        let mut old_version = bytes;
        old_version[..8].copy_from_slice(b"VLGCSR01");
        assert_eq!(decode(&old_version), Err(CodecError::Corrupt));
    }

    #[test]
    fn property_codec_enforces_existing_engine_field_and_value_limits() {
        let mut source = fixture_graph(&json!({
            "nodes": [{"id": 1, "labels": []}],
            "edges": [],
        }));
        source.nodes[0].properties =
            Properties::from([("text".into(), PropertyValue::String("x".repeat(4_096)))]);
        let bytes = encode(&source).unwrap();
        assert_eq!(
            decode(&bytes).unwrap().source.nodes[0].properties,
            source.nodes[0].properties
        );

        source.nodes[0].properties = (0..64)
            .map(|index| (format!("p{index:02}"), PropertyValue::Integer(index)))
            .collect();
        let bytes = encode(&source).unwrap();
        assert_eq!(
            decode(&bytes).unwrap().source.nodes[0].properties,
            source.nodes[0].properties
        );

        source.nodes[0].properties =
            Properties::from([("x".repeat(256), PropertyValue::Boolean(true))]);
        assert_eq!(
            decode(&encode(&source).unwrap()).unwrap().source.nodes[0].properties,
            source.nodes[0].properties
        );

        source.nodes[0].properties =
            Properties::from([("text".into(), PropertyValue::String("x".repeat(4_097)))]);
        assert_eq!(encode(&source), Err(CodecError::Corrupt));

        source.nodes[0].properties = (0..65)
            .map(|index| (format!("p{index:02}"), PropertyValue::Integer(index)))
            .collect();
        assert_eq!(encode(&source), Err(CodecError::Corrupt));

        source.nodes[0].properties =
            Properties::from([("x".repeat(257), PropertyValue::Boolean(true))]);
        assert_eq!(encode(&source), Err(CodecError::Corrupt));
    }

    #[test]
    fn temporal_series_requires_the_temporal_capability() {
        let mut graph = fixture_graph(&json!({"nodes": [{"id": 1, "labels": []}], "edges": []}));
        graph.edges.push(SourceEdge {
            id: 1,
            source: 1,
            target: 1,
            relationship: "r".into(),
            key: None,
            revision: 0,
            deadline: None,
            properties: Properties::new(),
            segments: Some(vec![Segment {
                valid_from: Some(0),
                valid_to: Some(1),
                properties: Properties::new(),
            }]),
        });
        assert_eq!(encode(&graph), Err(CodecError::Corrupt));
    }

    #[test]
    fn graph_source_preserves_identity_keys_revisions_and_deadlines() {
        let source = identity_source();
        let restored = decode(&encode(&source).unwrap()).unwrap();
        assert_eq!(restored.source, source);
        assert_eq!(restored.source.nodes[0].key.as_deref(), Some("node-3"));
        assert_eq!(restored.source.edges[0].key.as_deref(), Some("edge-4"));
        assert_eq!(restored.source.nodes[1].deadline, Some(50));
        assert_eq!(restored.source.nodes[0].revision, 7);
        assert_eq!(restored.source.edges[0].revision, 7);
        assert_eq!(restored.source.next_node_id, 11);
        assert_eq!(restored.source.next_edge_id, 5);
    }

    fn identity_source() -> GraphSource {
        let mut state = GraphState::new();
        state
            .apply(
                7,
                &GraphBody {
                    graph_id: GraphId(0),
                    entries: vec![
                        GraphEntry::CreateGraph { name: "g".into() },
                        GraphEntry::PutNode {
                            node_id: NodeId(3),
                            key: "node-3".into(),
                            labels: BTreeSet::from(["Person".into()]),
                            properties: Default::default(),
                        },
                        GraphEntry::PutNode {
                            node_id: NodeId(10),
                            key: "node-10".into(),
                            labels: BTreeSet::new(),
                            properties: Default::default(),
                        },
                        GraphEntry::PutEdge {
                            edge_id: crate::property_graph::engine::EdgeId(4),
                            source: NodeId(3),
                            destination: NodeId(10),
                            relationship: "knows".into(),
                            key: Some("edge-4".into()),
                            properties: Default::default(),
                        },
                        GraphEntry::NodeExpiry {
                            node_id: NodeId(10),
                            expires_at_ms: Some(50),
                        },
                    ],
                },
            )
            .unwrap();
        GraphSource::from_graph(state.graph(GraphId(0)).unwrap()).unwrap()
    }

    pub(super) fn full_temporal_state() -> GraphState {
        let max_series = (0..MAX_SEGMENTS)
            .map(|index| {
                let (valid_from, valid_to) = match index {
                    0 => (None, Some(i64::MIN)),
                    1 => (Some(i64::MIN), Some(i64::MIN + 1)),
                    255 => (Some(i64::MAX - 1), Some(i64::MAX)),
                    _ => {
                        let start = i64::MIN + i64::try_from(index).unwrap() * 3;
                        (Some(start), Some(start + 1))
                    }
                };
                Segment {
                    valid_from,
                    valid_to,
                    properties: props([
                        ("ordinal", int(i64::try_from(index).unwrap())),
                        ("source", text("series")),
                    ]),
                }
            })
            .collect();
        let extreme_series = vec![
            Segment {
                valid_from: None,
                valid_to: Some(i64::MIN),
                properties: props([("position", text("below-min"))]),
            },
            Segment {
                valid_from: Some(i64::MIN),
                valid_to: Some(i64::MIN + 1),
                properties: props([("position", text("min"))]),
            },
            Segment {
                valid_from: Some(i64::MAX - 1),
                valid_to: Some(i64::MAX),
                properties: props([("position", text("before-max"))]),
            },
            Segment {
                valid_from: Some(i64::MAX),
                valid_to: None,
                properties: props([("position", text("max-open"))]),
            },
        ];
        let mut state = GraphState::new();
        state.set_temporal(true).unwrap();
        state
            .apply(
                1,
                &GraphBody {
                    graph_id: GraphId(0),
                    entries: vec![
                        GraphEntry::CreateGraph {
                            name: "temporal".into(),
                        },
                        GraphEntry::PutNode {
                            node_id: NodeId(0),
                            key: "source".into(),
                            labels: BTreeSet::new(),
                            properties: Properties::new(),
                        },
                        GraphEntry::NodeExpiry {
                            node_id: NodeId(0),
                            expires_at_ms: Some(100),
                        },
                        GraphEntry::PutNode {
                            node_id: NodeId(1),
                            key: "target".into(),
                            labels: BTreeSet::new(),
                            properties: Properties::new(),
                        },
                        GraphEntry::NodeExpiry {
                            node_id: NodeId(1),
                            expires_at_ms: Some(80),
                        },
                        GraphEntry::PutNode {
                            node_id: NodeId(2),
                            key: "other".into(),
                            labels: BTreeSet::new(),
                            properties: Properties::new(),
                        },
                    ],
                },
            )
            .unwrap();
        state
            .apply(
                2,
                &GraphBody {
                    graph_id: GraphId(0),
                    entries: vec![
                        GraphEntry::PutEdge {
                            edge_id: EdgeId(0),
                            source: NodeId(0),
                            destination: NodeId(2),
                            relationship: "legacy".into(),
                            key: Some("ordinary".into()),
                            properties: props([("weight", int(7))]),
                        },
                        GraphEntry::Temporal(TemporalEntry::PutEdge {
                            edge_id: EdgeId(1),
                            revision: 0,
                            source: NodeId(0),
                            destination: NodeId(1),
                            relationship: "temporal".into(),
                            key: Some("full".into()),
                            segments: max_series,
                        }),
                        GraphEntry::EdgeExpiry {
                            edge_id: EdgeId(1),
                            expires_at_ms: Some(90),
                        },
                        GraphEntry::Temporal(TemporalEntry::PutEdge {
                            edge_id: EdgeId(2),
                            revision: 0,
                            source: NodeId(0),
                            destination: NodeId(2),
                            relationship: "temporal".into(),
                            key: Some("extremes".into()),
                            segments: extreme_series,
                        }),
                        GraphEntry::EdgeExpiry {
                            edge_id: EdgeId(2),
                            expires_at_ms: Some(70),
                        },
                    ],
                },
            )
            .unwrap();
        state
    }

    pub(super) fn temporal_state_with_removed_series() -> GraphState {
        let mut state = GraphState::new();
        state.set_temporal(true).unwrap();
        state
            .apply(
                1,
                &GraphBody {
                    graph_id: GraphId(0),
                    entries: vec![
                        GraphEntry::CreateGraph {
                            name: "temporal".into(),
                        },
                        GraphEntry::PutNode {
                            node_id: NodeId(0),
                            key: "source".into(),
                            labels: BTreeSet::new(),
                            properties: Properties::new(),
                        },
                        GraphEntry::PutNode {
                            node_id: NodeId(1),
                            key: "target".into(),
                            labels: BTreeSet::new(),
                            properties: Properties::new(),
                        },
                    ],
                },
            )
            .unwrap();
        state
            .apply(
                2,
                &GraphBody {
                    graph_id: GraphId(0),
                    entries: vec![
                        GraphEntry::PutEdge {
                            edge_id: EdgeId(0),
                            source: NodeId(0),
                            destination: NodeId(1),
                            relationship: "ordinary".into(),
                            key: Some("ordinary".into()),
                            properties: props([("weight", int(7))]),
                        },
                        GraphEntry::Temporal(TemporalEntry::PutEdge {
                            edge_id: EdgeId(1),
                            revision: 0,
                            source: NodeId(0),
                            destination: NodeId(1),
                            relationship: "temporal".into(),
                            key: Some("retained".into()),
                            segments: vec![
                                Segment {
                                    valid_from: Some(0),
                                    valid_to: Some(10),
                                    properties: props([("state", text("retained"))]),
                                },
                                Segment {
                                    valid_from: Some(10),
                                    valid_to: Some(20),
                                    properties: props([("state", text("touching"))]),
                                },
                            ],
                        }),
                        GraphEntry::Temporal(TemporalEntry::PutEdge {
                            edge_id: EdgeId(2),
                            revision: 0,
                            source: NodeId(0),
                            destination: NodeId(1),
                            relationship: "temporal".into(),
                            key: Some("emptied".into()),
                            segments: vec![Segment {
                                valid_from: Some(20),
                                valid_to: Some(30),
                                properties: props([("state", text("removed"))]),
                            }],
                        }),
                        GraphEntry::EdgeExpiry {
                            edge_id: EdgeId(2),
                            expires_at_ms: Some(100),
                        },
                    ],
                },
            )
            .unwrap();
        state
            .apply(
                3,
                &GraphBody {
                    graph_id: GraphId(0),
                    entries: vec![GraphEntry::Temporal(TemporalEntry::RemoveSegment {
                        edge_id: EdgeId(2),
                        valid_from: Some(20),
                        valid_to: Some(30),
                    })],
                },
            )
            .unwrap();
        state
    }

    fn temporal_neighbor_views(graph: &Graph, as_of_ms: i64, ttl_clock_ms: u64) -> Vec<EdgeView> {
        let query = GraphQuery::new(QueryOp::Neighbors {
            node: NodeSelector::Id(NodeId(0)),
            direction: Direction::Out,
            types: None,
            edge_conditions: Vec::new(),
            limit: Some(20),
            cursor: None,
        });
        temporal_query_edges(graph, &query, as_of_ms, ttl_clock_ms)
    }

    fn temporal_query_edges(
        graph: &Graph,
        query: &GraphQuery,
        as_of_ms: i64,
        ttl_clock_ms: u64,
    ) -> Vec<EdgeView> {
        let clock = || 0;
        let mut context = ExecutionContext::new(graph.graph_sequence(), &clock);
        context.now_ms = ttl_clock_ms;
        let result = match execute_as_of(graph, query, &GraphLimits::default(), &context, as_of_ms)
        {
            Ok(result) => result,
            Err(GraphError::NodeNotFound(_)) => return Vec::new(),
            Err(error) => panic!("temporal neighbor query failed: {error}"),
        };
        let QueryOutput::Neighbors { edges, .. } = result.output else {
            panic!("neighbors query returned the wrong output")
        };
        edges
    }

    pub(super) fn read_case_state(case: &Value) -> GraphState {
        let mut state = GraphState::new();
        state.set_temporal(true).unwrap();
        let mut entries = vec![GraphEntry::CreateGraph {
            name: "readcase".into(),
        }];
        for node in case["input"]["nodes"].as_array().unwrap() {
            let id = node["id"].as_u64().unwrap();
            entries.push(GraphEntry::PutNode {
                node_id: NodeId(id),
                key: format!("n{id}"),
                labels: BTreeSet::new(),
                properties: Properties::new(),
            });
            if let Some(deadline) = node["deadline"].as_u64() {
                entries.push(GraphEntry::NodeExpiry {
                    node_id: NodeId(id),
                    expires_at_ms: Some(deadline),
                });
            }
        }
        for edge in case["input"]["edges"].as_array().unwrap() {
            let edge_id = EdgeId(edge["id"].as_u64().unwrap());
            let source = NodeId(edge["source"].as_u64().unwrap());
            let destination = NodeId(edge["target"].as_u64().unwrap());
            let relationship = edge["relationship"].as_str().unwrap().to_owned();
            let segments = edge.get("segments").and_then(Value::as_array);
            if let Some(segments) = segments {
                entries.push(GraphEntry::Temporal(TemporalEntry::PutEdge {
                    edge_id,
                    revision: 0,
                    source,
                    destination,
                    relationship,
                    key: None,
                    segments: segments
                        .iter()
                        .map(|segment| Segment {
                            valid_from: segment["from"].as_i64(),
                            valid_to: segment["to"].as_i64(),
                            properties: Properties::new(),
                        })
                        .collect(),
                }));
            } else {
                entries.push(GraphEntry::PutEdge {
                    edge_id,
                    source,
                    destination,
                    relationship,
                    key: None,
                    properties: Properties::new(),
                });
            }
            if let Some(deadline) = edge["deadline"].as_u64() {
                entries.push(GraphEntry::EdgeExpiry {
                    edge_id,
                    expires_at_ms: Some(deadline),
                });
            }
        }
        state
            .apply(
                1,
                &GraphBody {
                    graph_id: GraphId(0),
                    entries,
                },
            )
            .unwrap_or_else(|error| panic!("{}: {error:?}", case["name"]));
        state
    }

    pub(super) fn golden_state(case: &Value) -> GraphState {
        let mut state = GraphState::new();
        state.set_temporal(true).unwrap();
        let mut entries = vec![GraphEntry::CreateGraph {
            name: "golden".into(),
        }];
        for (index, endpoint) in [case["source"].clone(), case["target"].clone()]
            .into_iter()
            .enumerate()
        {
            entries.push(GraphEntry::PutNode {
                node_id: NodeId(u64::try_from(index).unwrap()),
                key: format!("n{index}"),
                labels: BTreeSet::new(),
                properties: Properties::new(),
            });
            if let Some(deadline) = endpoint["expires_at_ms"].as_u64() {
                entries.push(GraphEntry::NodeExpiry {
                    node_id: NodeId(u64::try_from(index).unwrap()),
                    expires_at_ms: Some(deadline),
                });
            }
        }
        let segments = case["segments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|segment| Segment {
                valid_from: segment["valid_from"].as_i64(),
                valid_to: segment["valid_to"].as_i64(),
                properties: json_properties(&segment["properties"]),
            })
            .collect();
        entries.push(GraphEntry::Temporal(TemporalEntry::PutEdge {
            edge_id: EdgeId(1),
            revision: 0,
            source: NodeId(0),
            destination: NodeId(1),
            relationship: "rel".into(),
            key: Some("edge".into()),
            segments,
        }));
        if let Some(deadline) = case["edge"]["expires_at_ms"].as_u64() {
            entries.push(GraphEntry::EdgeExpiry {
                edge_id: EdgeId(1),
                expires_at_ms: Some(deadline),
            });
        }
        state
            .apply(
                1,
                &GraphBody {
                    graph_id: GraphId(0),
                    entries,
                },
            )
            .unwrap();
        state
    }

    fn json_properties(value: &Value) -> Properties {
        value
            .as_object()
            .into_iter()
            .flat_map(|fields| fields.iter())
            .filter_map(|(name, value)| {
                let value = if let Some(value) = value.as_str() {
                    PropertyValue::String(value.to_owned())
                } else if let Some(value) = value.as_bool() {
                    PropertyValue::Boolean(value)
                } else if let Some(value) = value.as_i64() {
                    PropertyValue::Integer(value)
                } else {
                    PropertyValue::Float(value.as_f64()?)
                };
                Some((name.clone(), value))
            })
            .collect()
    }

    #[test]
    fn temporal_series_round_trip_preserves_snapshot_state_and_c1_ttl_visibility() {
        let state = full_temporal_state();
        let graph = state.graph(GraphId(0)).unwrap();
        let source = GraphSource::from_graph(graph).unwrap();
        assert!(source.temporal_enabled);
        assert_eq!(
            source.edges[1].segments.as_ref().unwrap().len(),
            MAX_SEGMENTS
        );
        assert!(source.edges[0].segments.is_none());
        assert!(source.edges[0].properties.contains_key("weight"));

        let bytes = encode(&source).unwrap();
        assert_eq!(&bytes[..8], b"VLGCSR06");
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.source, source);
        assert_eq!(encode(&decoded.source).unwrap(), bytes);
        let restored = engine_state(&decoded.source).unwrap();
        assert_eq!(restored, state);

        let restored_graph = restored.graph(GraphId(0)).unwrap();
        for (as_of_ms, ttl_clock_ms, expected_ids) in [
            (i64::MIN, 60, vec![0, 1, 2]),
            (i64::MIN + 1, 60, vec![0]),
            (i64::MIN + 6, 60, vec![0, 1]),
            (i64::MIN + 6, 70, vec![0, 1]),
            (i64::MAX, 60, vec![0, 2]),
            (i64::MAX, 80, vec![0]),
            (i64::MAX, 100, vec![]),
        ] {
            let expected: Vec<_> = temporal_neighbor_views(graph, as_of_ms, ttl_clock_ms)
                .into_iter()
                .map(|edge| edge.id.0)
                .collect();
            let actual: Vec<_> = temporal_neighbor_views(restored_graph, as_of_ms, ttl_clock_ms)
                .into_iter()
                .map(|edge| edge.id.0)
                .collect();
            assert_eq!(expected, expected_ids, "T={as_of_ms}, C={ttl_clock_ms}");
            assert_eq!(actual, expected, "T={as_of_ms}, C={ttl_clock_ms}");
        }

        let views = temporal_neighbor_views(restored_graph, i64::MIN + 6, 60);
        assert_eq!(
            views[1].properties,
            source.edges[1].segments.as_ref().unwrap()[2].properties
        );
    }

    #[test]
    fn temporal_codec_rejects_noncanonical_and_malformed_series_sections() {
        let state = temporal_state_with_removed_series();
        let source = GraphSource::from_graph(state.graph(GraphId(0)).unwrap()).unwrap();
        let bytes = encode(&source).unwrap();
        let section = bytes
            .windows(TEMPORAL_SECTION.len())
            .position(|window| window == TEMPORAL_SECTION)
            .unwrap();
        let property_names = build(source.clone()).unwrap().property_names;
        let mut section_reader = Reader::new(&bytes[section..]);
        section_reader.take(TEMPORAL_SECTION.len()).unwrap();
        let series_count = section_reader.entries(8).unwrap();
        assert_eq!(series_count, 2);
        let mut series_offsets = Vec::new();
        for _ in 0..series_count {
            let ordinal_at = section + section_reader.at;
            let ordinal = section_reader.u32().unwrap();
            let segment_count_at = section + section_reader.at;
            let segment_count = section_reader.entries(6).unwrap();
            let mut segment_starts = Vec::new();
            for _ in 0..segment_count {
                segment_starts.push(section + section_reader.at);
                section_reader.optional_i64().unwrap();
                section_reader.optional_i64().unwrap();
                section_reader.properties(&property_names).unwrap();
            }
            series_offsets.push((
                ordinal_at,
                ordinal,
                segment_count_at,
                segment_count,
                segment_starts,
            ));
        }
        assert!(section_reader.is_empty());
        let (first_ordinal_at, first_ordinal, series_count_at, _, first_segments) =
            &series_offsets[0];
        let (second_ordinal_at, _, _, _, _) = &series_offsets[1];
        let segment_start = first_segments[0];
        let second_segment_start = first_segments[1];
        let empty_series_ordinal_at = *second_ordinal_at;
        let mut mutations = Vec::new();

        let mut too_many_segments = bytes.clone();
        too_many_segments[*series_count_at..*series_count_at + 4]
            .copy_from_slice(&257u32.to_le_bytes());
        mutations.push(("too many segments", too_many_segments));

        let mut empty_interval = bytes.clone();
        empty_interval[segment_start + 10..segment_start + 18].copy_from_slice(&0i64.to_le_bytes());
        mutations.push(("empty interval", empty_interval));

        let mut unsorted = bytes.clone();
        unsorted[second_segment_start + 1..second_segment_start + 9]
            .copy_from_slice(&(-1i64).to_le_bytes());
        mutations.push(("unsorted intervals", unsorted));

        let mut overlapping = bytes.clone();
        overlapping[second_segment_start + 1..second_segment_start + 9]
            .copy_from_slice(&5i64.to_le_bytes());
        mutations.push(("overlapping intervals", overlapping));

        let mut invalid_bound_marker = bytes.clone();
        invalid_bound_marker[segment_start] = 2;
        mutations.push(("invalid bound marker", invalid_bound_marker));

        let mut invalid_edge_ordinal = bytes.clone();
        invalid_edge_ordinal[section + 12..section + 16].copy_from_slice(&u32::MAX.to_le_bytes());
        mutations.push(("invalid edge ordinal", invalid_edge_ordinal));

        let mut bad_section_marker = bytes.clone();
        bad_section_marker[section] ^= 1;
        mutations.push(("invalid section marker", bad_section_marker));

        let mut empty_series_on_edge_with_properties = bytes.clone();
        empty_series_on_edge_with_properties[*first_ordinal_at..*first_ordinal_at + 4]
            .copy_from_slice(&0u32.to_le_bytes());
        empty_series_on_edge_with_properties[*series_count_at..*series_count_at + 4]
            .copy_from_slice(&0u32.to_le_bytes());
        mutations.push((
            "empty temporal series assigned to edge with properties",
            empty_series_on_edge_with_properties,
        ));

        let mut duplicate_ordinal = bytes.clone();
        duplicate_ordinal[empty_series_ordinal_at..empty_series_ordinal_at + 4]
            .copy_from_slice(&first_ordinal.to_le_bytes());
        mutations.push(("duplicate temporal edge ordinal", duplicate_ordinal));

        let mut descending_ordinal = bytes.clone();
        descending_ordinal[empty_series_ordinal_at..empty_series_ordinal_at + 4]
            .copy_from_slice(&0u32.to_le_bytes());
        mutations.push(("descending temporal edge ordinal", descending_ordinal));

        for (what, corrupt) in mutations {
            assert_eq!(decode(&corrupt), Err(CodecError::Corrupt), "{what}");
        }
        for end in section + 1..bytes.len() {
            assert_eq!(
                decode(&bytes[..end]),
                Err(CodecError::Corrupt),
                "truncated at {end}"
            );
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert_eq!(decode(&trailing), Err(CodecError::Corrupt));

        assert_eq!(
            decode(&bytes).unwrap().source.edges[2].segments,
            Some(Vec::new())
        );
    }

    #[test]
    fn empty_temporal_series_round_trips_with_mixed_edges_and_matches_c1() {
        let state = temporal_state_with_removed_series();
        let graph = state.graph(GraphId(0)).unwrap();
        let source = GraphSource::from_graph(graph).unwrap();
        assert!(source.temporal_enabled);
        assert!(source.edges[0].segments.is_none());
        assert!(source.edges[1].segments.as_ref().unwrap().len() == 2);
        assert_eq!(source.edges[2].segments, Some(Vec::new()));

        let bytes = encode(&source).unwrap();
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.source, source);
        assert_eq!(encode(&decoded.source).unwrap(), bytes);
        let restored = engine_state(&decoded.source).unwrap();
        assert_eq!(restored, state);

        let restored_graph = restored.graph(GraphId(0)).unwrap();
        for edge in graph.edges() {
            assert_eq!(
                graph.edge_snapshot_entry(edge, 3),
                restored_graph.edge_snapshot_entry(restored_graph.edge(edge.id).unwrap(), 3)
            );
        }
        for (as_of_ms, ttl_clock_ms) in [(5, 0), (15, 0), (25, 0), (25, 100)] {
            assert_eq!(
                temporal_neighbor_views(graph, as_of_ms, ttl_clock_ms),
                temporal_neighbor_views(restored_graph, as_of_ms, ttl_clock_ms),
                "T={as_of_ms}, C={ttl_clock_ms}"
            );
        }
    }

    #[test]
    fn temporal_interval_goldens_match_decoded_c1_reads_at_fixed_t_and_c() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/temporal_intervals/golden.json"
        ))
        .unwrap();
        let mut covered = 0;
        for case in fixture["series"].as_array().unwrap() {
            if !case["edge"]["retained"].as_bool().unwrap()
                || !case["source"]["retained"].as_bool().unwrap()
                || !case["target"]["retained"].as_bool().unwrap()
            {
                continue;
            }
            covered += 1;
            let state = golden_state(case);
            let graph = state.graph(GraphId(0)).unwrap();
            let source = GraphSource::from_graph(graph).unwrap();
            let decoded = decode(&encode(&source).unwrap()).unwrap();
            let restored = engine_state(&decoded.source).unwrap();
            assert_eq!(restored, state, "{}", case["name"]);
            let restored_graph = restored.graph(GraphId(0)).unwrap();

            for query in case["queries"].as_array().unwrap() {
                let as_of_ms = query["T"].as_i64().unwrap();
                let ttl_clock_ms = query["C"].as_u64().unwrap();
                let original = temporal_neighbor_views(graph, as_of_ms, ttl_clock_ms);
                let actual = temporal_neighbor_views(restored_graph, as_of_ms, ttl_clock_ms);
                assert_eq!(
                    actual, original,
                    "{} at T={as_of_ms}, C={ttl_clock_ms}",
                    case["name"]
                );
                let expected = &query["expected"];
                if expected.is_null() {
                    assert!(
                        actual.is_empty(),
                        "{} at T={as_of_ms}, C={ttl_clock_ms}",
                        case["name"]
                    );
                    continue;
                }
                assert_eq!(
                    actual.len(),
                    1,
                    "{} at T={as_of_ms}, C={ttl_clock_ms}",
                    case["name"]
                );
                assert_eq!(actual[0].id, EdgeId(1));
                let segment = restored_graph
                    .segments(EdgeId(1))
                    .unwrap()
                    .iter()
                    .find(|segment| segment.contains(as_of_ms))
                    .unwrap();
                assert_eq!(segment.valid_from, expected["valid_from"].as_i64());
                assert_eq!(segment.valid_to, expected["valid_to"].as_i64());
                assert_eq!(segment.properties, json_properties(&expected["properties"]));
            }
        }
        assert!(covered >= 8, "expected interval, boundary, and TTL goldens");
    }

    #[test]
    fn temporal_read_case_neighbors_match_decoded_c1_reads() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/temporal_intervals/read-cases.json"
        ))
        .unwrap();
        let case = &fixture["cases"][0];
        let state = read_case_state(case);
        let graph = state.graph(GraphId(0)).unwrap();
        let source = GraphSource::from_graph(graph).unwrap();
        let decoded = decode(&encode(&source).unwrap()).unwrap();
        let restored = engine_state(&decoded.source).unwrap();
        assert_eq!(restored, state);
        let restored_graph = restored.graph(GraphId(0)).unwrap();
        let mut covered = 0;

        for (index, query) in case["input"]["queries"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            if query["op"] != "neighbors" || query.get("after").and_then(Value::as_object).is_some()
            {
                continue;
            }
            let direction = match query["direction"].as_str().unwrap() {
                "out" => Direction::Out,
                "in" => Direction::In,
                "both" => Direction::Both,
                other => panic!("unknown read-case direction: {other}"),
            };
            let types = query["relationship"].as_array().map(|values| {
                values
                    .iter()
                    .map(|value| value.as_str().unwrap().to_owned())
                    .collect()
            });
            let graph_query = GraphQuery::new(QueryOp::Neighbors {
                node: NodeSelector::Id(NodeId(query["node"].as_u64().unwrap())),
                direction,
                types,
                edge_conditions: Vec::new(),
                limit: query["limit"].as_u64(),
                cursor: None,
            });
            let as_of_ms = query["T"].as_i64().unwrap();
            let ttl_clock_ms = query["C"].as_u64().unwrap();
            let expected = case["expected"][index]["edges"]
                .as_array()
                .unwrap()
                .iter()
                .map(|id| id.as_u64().unwrap())
                .collect::<Vec<_>>();
            let original: Vec<_> =
                temporal_query_edges(graph, &graph_query, as_of_ms, ttl_clock_ms)
                    .into_iter()
                    .map(|edge| edge.id.0)
                    .collect();
            let actual: Vec<_> =
                temporal_query_edges(restored_graph, &graph_query, as_of_ms, ttl_clock_ms)
                    .into_iter()
                    .map(|edge| edge.id.0)
                    .collect();
            assert_eq!(original, expected, "read-case query {index}");
            assert_eq!(actual, original, "read-case query {index}");
            covered += 1;
        }
        assert!(covered >= 8);
    }

    fn replace_key(bytes: &mut [u8], nodes: bool, ordinal: usize, value: &str) {
        let mut reader = Reader::new(bytes);
        reader.take(MAGIC.len()).unwrap();
        reader.u16().unwrap();
        reader.string(63).unwrap();
        reader.u64().unwrap();
        reader.u64().unwrap();
        reader.u64().unwrap();
        reader.schema().unwrap();
        reader.strings(63).unwrap();
        reader.strings(63).unwrap();
        let property_names = reader.strings(256).unwrap();
        let node_count = reader.count().unwrap();
        let mut keys = Vec::new();
        for _ in 0..node_count {
            reader.u64().unwrap();
            keys.push(reader.at);
            reader.optional_string().unwrap();
            reader.u64().unwrap();
            reader.optional_u64().unwrap();
            let label_count = reader.count().unwrap();
            reader.take(label_count * 4).unwrap();
            reader.properties(&property_names).unwrap();
        }
        let edge_count = reader.count().unwrap();
        for _ in 0..edge_count {
            reader.u64().unwrap();
            reader.u32().unwrap();
            reader.u32().unwrap();
            reader.u32().unwrap();
            keys.push(reader.at);
            reader.optional_string().unwrap();
            reader.u64().unwrap();
            reader.optional_u64().unwrap();
            reader.properties(&property_names).unwrap();
        }
        let key = keys[if nodes { ordinal } else { node_count + ordinal }];
        assert_eq!(bytes[key], 1, "key must be present");
        let len = u32::from_le_bytes(bytes[key + 1..key + 5].try_into().unwrap()) as usize;
        assert_eq!(len, value.len(), "replacement keeps the encoded width");
        bytes[key + 5..key + 5 + len].copy_from_slice(value.as_bytes());
    }

    type PropertyOffsets = (Vec<(String, usize)>, Vec<(usize, usize, u32, usize)>);

    type SourceMutation = fn(&mut GraphSource);

    fn property_offsets(bytes: &[u8], node_ordinal: usize) -> PropertyOffsets {
        let mut reader = Reader::new(bytes);
        reader.take(MAGIC.len()).unwrap();
        reader.u16().unwrap();
        reader.string(63).unwrap();
        reader.u64().unwrap();
        reader.u64().unwrap();
        reader.u64().unwrap();
        reader.schema().unwrap();
        reader.strings(63).unwrap();
        reader.strings(63).unwrap();
        let dictionary_count = reader.count().unwrap();
        let mut dictionary = Vec::with_capacity(dictionary_count);
        for _ in 0..dictionary_count {
            let len = reader.count().unwrap();
            let offset = reader.at;
            let value = std::str::from_utf8(reader.take(len).unwrap())
                .unwrap()
                .to_owned();
            dictionary.push((value, offset));
        }
        let node_count = reader.count().unwrap();
        let mut selected = Vec::new();
        for ordinal in 0..node_count {
            reader.u64().unwrap();
            reader.optional_string().unwrap();
            reader.u64().unwrap();
            reader.optional_u64().unwrap();
            let label_count = reader.count().unwrap();
            reader.take(label_count * 4).unwrap();
            let count = reader.count().unwrap();
            let mut entries = Vec::with_capacity(count);
            for _ in 0..count {
                let code_offset = reader.at;
                let code = reader.u32().unwrap();
                let tag_offset = reader.at;
                let tag = reader.u8().unwrap();
                let value_offset = reader.at;
                match tag {
                    1 => {
                        let len = reader.count().unwrap();
                        reader.take(len).unwrap();
                    }
                    2 | 3 => {
                        reader.take(8).unwrap();
                    }
                    4 => {
                        reader.take(1).unwrap();
                    }
                    5 => {
                        reader.take(16).unwrap();
                    }
                    _ => panic!("unexpected tag in encoded test fixture"),
                }
                entries.push((code_offset, tag_offset, code, value_offset));
            }
            if ordinal == node_ordinal {
                selected = entries;
            }
        }
        (dictionary, selected)
    }

    fn fixture_graph(case: &Value) -> GraphSource {
        let nodes: Vec<SourceNode> = case["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|node| SourceNode {
                id: node["id"].as_u64().unwrap(),
                key: None,
                labels: node["labels"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|label| label.as_str().unwrap().to_owned())
                    .collect(),
                revision: 0,
                deadline: node.get("deadline").and_then(Value::as_u64),
                properties: Properties::new(),
                attachments: BTreeMap::new(),
            })
            .collect();
        let edges: Vec<SourceEdge> = case["edges"]
            .as_array()
            .unwrap()
            .iter()
            .map(|edge| SourceEdge {
                id: edge["id"].as_u64().unwrap(),
                source: edge["source"].as_u64().unwrap(),
                target: edge["target"].as_u64().unwrap(),
                relationship: edge["relationship"].as_str().unwrap().to_owned(),
                key: None,
                revision: 0,
                deadline: edge.get("deadline").and_then(Value::as_u64),
                properties: Properties::new(),
                segments: None,
            })
            .collect();
        let next_node_id = nodes
            .iter()
            .map(|node: &SourceNode| node.id)
            .max()
            .and_then(|id| id.checked_add(1))
            .unwrap_or_else(|| {
                if nodes.iter().any(|node: &SourceNode| node.id == u64::MAX) {
                    u64::MAX
                } else {
                    0
                }
            });
        let next_edge_id = edges
            .iter()
            .map(|edge: &SourceEdge| edge.id)
            .max()
            .and_then(|id| id.checked_add(1))
            .unwrap_or_else(|| {
                if edges.iter().any(|edge: &SourceEdge| edge.id == u64::MAX) {
                    u64::MAX
                } else {
                    0
                }
            });
        GraphSource {
            graph_id: 0,
            name: "fixture".to_owned(),
            sequence: 0,
            next_node_id,
            next_edge_id,
            temporal_enabled: false,
            schema: GraphSchema::default(),
            nodes,
            edges,
        }
    }

    #[test]
    fn schema_declarations_round_trip_with_engine_index_and_rule_parity() {
        let state = schema_state();
        let graph = state.graph(GraphId(3)).unwrap();
        let source = GraphSource::from_graph(graph).unwrap();
        assert_eq!(source.schema, declared_schema());
        let bytes = encode(&source).unwrap();
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.source, source);
        assert_eq!(encode(&decoded.source).unwrap(), bytes);
        let header = &decoded.source;
        assert_eq!(
            (
                header.graph_id,
                header.sequence,
                header.next_node_id,
                header.next_edge_id
            ),
            (3, 3, 100, 200)
        );

        // The derived property indexes are not stored: the engine snapshot
        // loader rebuilds them, and the rebuilt graph equals the original
        // field for field (indexes, schema, deadlines, attachments).
        let rebuilt = engine_state(&decoded.source).unwrap();
        let restored = rebuilt.graph(GraphId(3)).unwrap();
        assert_eq!(restored, graph);
        assert_eq!(rebuilt, state);

        let cases = [
            (
                Some("Person"),
                vec![equals("email", text("b@x"))],
                vec![2],
                IndexChoice::PropertyExactIndex,
            ),
            (
                Some("Person"),
                vec![equals("team", text("red"))],
                vec![1, 2],
                IndexChoice::PropertyExactIndex,
            ),
            (
                Some("Person"),
                vec![between("age", int(30), int(35))],
                vec![1, 3],
                IndexChoice::PropertyRangeIndex,
            ),
            (
                Some("Person"),
                vec![between("score", float(-1.0), float(0.0))],
                vec![2, 3],
                IndexChoice::PropertyRangeIndex,
            ),
            (
                Some("Company"),
                vec![equals("code", int(7))],
                vec![4],
                IndexChoice::PropertyExactIndex,
            ),
            (
                Some("Company"),
                vec![between("code", int(-5), int(10))],
                vec![4, 6],
                IndexChoice::PropertyRangeIndex,
            ),
            (
                Some("Person"),
                vec![equals("active", boolean(true))],
                vec![1, 3],
                IndexChoice::LabelIndexScan,
            ),
            (
                None,
                Vec::new(),
                vec![1, 2, 3, 4, 5, 6],
                IndexChoice::FullNodeScan,
            ),
        ];
        for (label, conditions, expected, index) in cases {
            for now_ms in [0, 55] {
                for explain in [false, true] {
                    let actual = find(restored, label, &conditions, now_ms, explain);
                    assert_eq!(actual, find(graph, label, &conditions, now_ms, explain));
                    match actual.output {
                        QueryOutput::Explain(explanation) => {
                            assert_eq!(explanation.indexes, vec![index]);
                        }
                        QueryOutput::Nodes(nodes) => {
                            assert_eq!(actual.stats.indexes, vec![index]);
                            if now_ms == 0 {
                                let ids: Vec<_> = nodes.iter().map(|node| node.id.0).collect();
                                assert_eq!(ids, expected);
                            }
                        }
                        output => panic!("unexpected find_nodes output {output:?}"),
                    }
                }
            }
        }
        let late = find(
            restored,
            Some("Person"),
            &[between("age", int(30), int(35))],
            55,
            false,
        );
        assert!(
            matches!(&late.output, QueryOutput::Nodes(nodes) if nodes.iter().map(|node| node.id.0).eq([1]))
        );

        let duplicate_email = put_node(
            100,
            "dup",
            &["Person"],
            props([("active", boolean(true)), ("email", text("b@x"))]),
        );
        let writes = [
            (
                vec![duplicate_email.clone()],
                Err(violation("unique", "Person", Some("email"))),
            ),
            (
                vec![put_node(
                    100,
                    "typed",
                    &["Person"],
                    props([
                        ("active", boolean(true)),
                        ("age", text("old")),
                        ("email", text("d@x")),
                    ]),
                )],
                Err(violation("type", "Person", Some("age"))),
            ),
            (
                vec![put_node(
                    100,
                    "missing",
                    &["Person"],
                    props([("email", text("d@x"))]),
                )],
                Err(violation("required", "Person", Some("active"))),
            ),
            (
                vec![put_node(
                    100,
                    "clash",
                    &["Company"],
                    props([("code", int(7))]),
                )],
                Err(violation("unique", "Company", Some("code"))),
            ),
            (
                vec![put_edge(
                    200,
                    5,
                    4,
                    "WORKS_AT",
                    None,
                    props([("since", int(1))]),
                )],
                Err(violation("endpoints", "WORKS_AT", None)),
            ),
            (
                vec![put_edge(
                    200,
                    1,
                    4,
                    "WORKS_AT",
                    None,
                    props([("since", int(1))]),
                )],
                Err(violation("unique_triple", "WORKS_AT", None)),
            ),
            (
                vec![put_edge(200, 2, 6, "WORKS_AT", None, Properties::new())],
                Err(violation("required", "WORKS_AT", Some("since"))),
            ),
            (
                vec![put_edge(
                    200,
                    1,
                    2,
                    "KNOWS",
                    None,
                    props([("weight", int(1))]),
                )],
                Err(violation("type", "KNOWS", Some("weight"))),
            ),
            (
                vec![
                    put_node(
                        100,
                        "dave",
                        &["Person"],
                        props([("active", boolean(true)), ("email", text("d@x"))]),
                    ),
                    put_edge(200, 100, 6, "WORKS_AT", None, props([("since", int(2024))])),
                ],
                Ok(()),
            ),
        ];
        for (entries, expected) in writes {
            let body = GraphBody {
                graph_id: GraphId(3),
                entries,
            };
            let (mut original, mut reloaded) = (state.clone(), rebuilt.clone());
            assert_eq!(original.apply(4, &body), expected);
            assert_eq!(reloaded.apply(4, &body), expected);
            assert_eq!(reloaded, original);
        }

        // Negative control: the same data without its declarations is a
        // different engine state that accepts what the schema refuses.
        let mut stripped = decoded.source.clone();
        stripped.schema = GraphSchema::default();
        let mut schemaless = engine_state(&stripped).unwrap();
        assert_ne!(schemaless, state);
        let body = GraphBody {
            graph_id: GraphId(3),
            entries: vec![duplicate_email],
        };
        assert_eq!(schemaless.apply(4, &body), Ok(()));
    }

    #[test]
    fn schema_section_layout_is_canonical_and_declaration_order_independent() {
        let state = schema_state();
        let source = GraphSource::from_graph(state.graph(GraphId(3)).unwrap()).unwrap();
        let declared = declared_schema();
        let mut reversed = GraphSchema::default();
        for (name, label) in declared.labels.iter().rev() {
            reversed = reversed.label(name.clone(), label.clone());
        }
        for (name, relationship) in declared.relationships.iter().rev() {
            reversed = reversed.relationship(name.clone(), relationship.clone());
        }
        let mut shuffled = source.clone();
        shuffled.schema = reversed;
        shuffled.nodes.reverse();
        shuffled.edges.reverse();
        assert_eq!(encode(&shuffled).unwrap(), encode(&source).unwrap());

        let mut small = fixture_graph(&json!({"nodes": [], "edges": []}));
        let empty = encode(&small).unwrap();
        assert_eq!(&empty[schema_span(&empty)], &[0u8; 8]);
        small.schema = GraphSchema::default()
            .relationship(
                "R",
                RelationshipDefinition::new("r λ")
                    .endpoint("B", "A")
                    .endpoint("A", "B")
                    .multiplicity(Multiplicity::UniqueTriple)
                    .property(
                        "w",
                        PropertyDefinition::new(PropertyType::GeoPoint).required(),
                    ),
            )
            .label(
                "B",
                LabelDefinition::new("")
                    .property(
                        "q",
                        PropertyDefinition::new(PropertyType::Float)
                            .index(PropertyIndexKind::Range),
                    )
                    .property(
                        "p",
                        PropertyDefinition::new(PropertyType::String)
                            .required()
                            .unique(),
                    ),
            )
            .label(
                "A",
                LabelDefinition::new("a")
                    .property(
                        "x",
                        PropertyDefinition::new(PropertyType::Boolean)
                            .index(PropertyIndexKind::Exact),
                    )
                    .property("y", PropertyDefinition::new(PropertyType::Integer)),
            );
        let bytes = encode(&small).unwrap();
        let expected = raw_schema(
            &[
                RawDefinition {
                    description: b"a".to_vec(),
                    properties: vec![(b"x".to_vec(), 4, 4), (b"y".to_vec(), 2, 0)],
                    ..raw("A")
                },
                RawDefinition {
                    properties: vec![(b"p".to_vec(), 1, 3), (b"q".to_vec(), 3, 8)],
                    ..raw("B")
                },
            ],
            &[RawDefinition {
                description: "r λ".as_bytes().to_vec(),
                multiplicity: 2,
                endpoints: vec![
                    (b"A".to_vec(), b"B".to_vec()),
                    (b"B".to_vec(), b"A".to_vec()),
                ],
                properties: vec![(b"w".to_vec(), 5, 1)],
                ..raw("R")
            }],
        );
        assert_eq!(&bytes[schema_span(&bytes)], expected.as_slice());
        assert_eq!(decode(&bytes).unwrap().source.schema, small.schema);
    }

    #[test]
    fn decoder_rejects_malformed_schema_sections() {
        let base = encode(&fixture_graph(&json!({"nodes": [], "edges": []}))).unwrap();
        let valid = splice_schema(
            &base,
            &raw_schema(
                &[
                    RawDefinition {
                        properties: vec![(b"p".to_vec(), 2, 0)],
                        ..raw("A")
                    },
                    raw("B"),
                ],
                &[RawDefinition {
                    endpoints: vec![(b"A".to_vec(), b"B".to_vec())],
                    properties: vec![(b"w".to_vec(), 3, 1)],
                    ..raw("R")
                }],
            ),
        );
        let decoded = decode(&valid).unwrap();
        assert_eq!(
            decoded.source.schema,
            GraphSchema::default()
                .label(
                    "A",
                    LabelDefinition::new("")
                        .property("p", PropertyDefinition::new(PropertyType::Integer)),
                )
                .label("B", LabelDefinition::new(""))
                .relationship(
                    "R",
                    RelationshipDefinition::new("")
                        .endpoint("A", "B")
                        .property("w", PropertyDefinition::new(PropertyType::Float).required(),),
                )
        );
        assert_eq!(encode(&decoded.source).unwrap(), valid);

        let label_property = |property: (Vec<u8>, u8, u8)| {
            raw_schema(
                &[RawDefinition {
                    properties: vec![property],
                    ..raw("A")
                }],
                &[],
            )
        };
        let relationship = |definition: RawDefinition| raw_schema(&[], &[definition]);
        let pair = |source: &[u8], destination: &[u8]| (source.to_vec(), destination.to_vec());
        let mut cases = Vec::new();
        for code in [0, 6, 255] {
            cases.push(("unknown type", label_property((b"p".to_vec(), code, 0))));
        }
        for flags in [12, 13, 16, 0x80] {
            cases.push(("unknown flags", label_property((b"p".to_vec(), 2, flags))));
        }
        cases.push((
            "unknown relationship property type",
            relationship(RawDefinition {
                properties: vec![(b"w".to_vec(), 9, 0)],
                ..raw("R")
            }),
        ));
        for multiplicity in [0, 3, 255] {
            cases.push((
                "unknown multiplicity",
                relationship(RawDefinition {
                    multiplicity,
                    ..raw("R")
                }),
            ));
        }
        cases.extend([
            ("duplicate labels", raw_schema(&[raw("A"), raw("A")], &[])),
            ("unsorted labels", raw_schema(&[raw("B"), raw("A")], &[])),
            (
                "duplicate relationships",
                raw_schema(&[], &[raw("R"), raw("R")]),
            ),
            (
                "unsorted relationships",
                raw_schema(&[], &[raw("S"), raw("R")]),
            ),
            (
                "duplicate endpoints",
                relationship(RawDefinition {
                    endpoints: vec![pair(b"A", b"B"), pair(b"A", b"B")],
                    ..raw("R")
                }),
            ),
            (
                "unsorted endpoint sources",
                relationship(RawDefinition {
                    endpoints: vec![pair(b"B", b"A"), pair(b"A", b"B")],
                    ..raw("R")
                }),
            ),
            (
                "unsorted endpoint destinations",
                relationship(RawDefinition {
                    endpoints: vec![pair(b"A", b"C"), pair(b"A", b"B")],
                    ..raw("R")
                }),
            ),
            (
                "duplicate label properties",
                raw_schema(
                    &[RawDefinition {
                        properties: vec![(b"p".to_vec(), 2, 0), (b"p".to_vec(), 2, 0)],
                        ..raw("A")
                    }],
                    &[],
                ),
            ),
            (
                "unsorted label properties",
                raw_schema(
                    &[RawDefinition {
                        properties: vec![(b"q".to_vec(), 2, 0), (b"p".to_vec(), 2, 0)],
                        ..raw("A")
                    }],
                    &[],
                ),
            ),
            (
                "duplicate relationship properties",
                relationship(RawDefinition {
                    properties: vec![(b"w".to_vec(), 3, 0), (b"w".to_vec(), 3, 0)],
                    ..raw("R")
                }),
            ),
            (
                "non-UTF-8 label",
                raw_schema(
                    &[RawDefinition {
                        name: vec![0xff],
                        ..raw("A")
                    }],
                    &[],
                ),
            ),
        ]);
        cases.extend([
            (
                "non-UTF-8 description",
                raw_schema(
                    &[RawDefinition {
                        description: vec![0xc3],
                        ..raw("A")
                    }],
                    &[],
                ),
            ),
            (
                "non-UTF-8 property name",
                label_property((vec![0xff], 2, 0)),
            ),
            (
                "non-UTF-8 relationship",
                relationship(RawDefinition {
                    name: vec![0xff],
                    ..raw("R")
                }),
            ),
            (
                "non-UTF-8 endpoint",
                relationship(RawDefinition {
                    endpoints: vec![pair(b"A", &[0xff])],
                    ..raw("R")
                }),
            ),
            (
                "label count overflow",
                with_u32(raw_schema(&[], &[]), 0, u32::MAX),
            ),
            (
                "relationship count overflow",
                with_u32(raw_schema(&[], &[]), 4, u32::MAX),
            ),
            (
                "label name length overflow",
                with_u32(raw_schema(&[raw("A")], &[]), 4, u32::MAX),
            ),
            (
                "description length overflow",
                with_u32(raw_schema(&[raw("A")], &[]), 9, u32::MAX),
            ),
            (
                "property count overflow",
                with_u32(raw_schema(&[raw("A")], &[]), 13, u32::MAX),
            ),
            (
                "property name length overflow",
                with_u32(label_property((b"p".to_vec(), 2, 0)), 17, u32::MAX),
            ),
            (
                "endpoint count overflow",
                with_u32(relationship(raw("R")), 18, u32::MAX),
            ),
        ]);
        for (what, section) in cases {
            assert_eq!(
                decode(&splice_schema(&base, &section)),
                Err(CodecError::Corrupt),
                "{what}"
            );
        }
        for end in 0..valid.len() {
            assert_eq!(decode(&valid[..end]), Err(CodecError::Corrupt), "{end}");
        }
        let mut trailing = valid;
        trailing.push(0);
        assert_eq!(decode(&trailing), Err(CodecError::Corrupt));
    }

    #[test]
    fn schema_codec_enforces_existing_engine_schema_limits() {
        let base = fixture_graph(&json!({"nodes": [], "edges": []}));
        let base_bytes = encode(&base).unwrap();
        let at_limit = limit_schema();
        assert_eq!(at_limit.validate(), Ok(()));
        let mut source = base.clone();
        source.schema = at_limit.clone();
        let bytes = encode(&source).unwrap();
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.source.schema, at_limit);
        assert_eq!(encode(&decoded.source).unwrap(), bytes);

        let long_label = "L".repeat(MAX_LABEL_BYTES);
        let long_relationship = "R".repeat(MAX_RELATIONSHIP_TYPE_BYTES);
        let over_limit = |change: &dyn Fn(&mut GraphSchema)| {
            let mut schema = at_limit.clone();
            change(&mut schema);
            schema
        };
        let label = |name: &str, definition| GraphSchema::default().label(name, definition);
        let relationship =
            |name: &str, definition| GraphSchema::default().relationship(name, definition);
        let property = PropertyDefinition::new;
        let cases = [
            (
                "labels",
                over_limit(&|schema| {
                    schema
                        .labels
                        .insert("L256".into(), LabelDefinition::new(""));
                }),
            ),
            (
                "relationships",
                over_limit(&|schema| {
                    schema
                        .relationships
                        .insert("R256".into(), RelationshipDefinition::new(""));
                }),
            ),
            (
                "label properties",
                over_limit(&|schema| {
                    let wide = schema.labels.get_mut(&long_label).unwrap();
                    wide.properties
                        .insert("extra".into(), property(PropertyType::Boolean));
                }),
            ),
            (
                "relationship properties",
                over_limit(&|schema| {
                    let long = schema.relationships.get_mut(&long_relationship).unwrap();
                    long.properties
                        .insert("extra".into(), property(PropertyType::Boolean));
                }),
            ),
            (
                "indexed properties",
                over_limit(&|schema| {
                    let other = schema.labels.get_mut("L001").unwrap();
                    other
                        .properties
                        .insert("x".into(), property(PropertyType::String).unique());
                }),
            ),
            (
                "endpoints",
                over_limit(&|schema| {
                    let long = schema.relationships.get_mut(&long_relationship).unwrap();
                    long.endpoints.insert(("Z".into(), "Z".into()));
                }),
            ),
            (
                "label description",
                label(
                    "A",
                    LabelDefinition::new("d".repeat(MAX_SCHEMA_DESCRIPTION_BYTES + 1)),
                ),
            ),
            (
                "relationship description",
                relationship(
                    "R",
                    RelationshipDefinition::new("r".repeat(MAX_SCHEMA_DESCRIPTION_BYTES + 1)),
                ),
            ),
            (
                "label name",
                label(&"L".repeat(MAX_LABEL_BYTES + 1), LabelDefinition::new("")),
            ),
            ("empty label name", label("", LabelDefinition::new(""))),
            (
                "label characters",
                label("bad-label", LabelDefinition::new("")),
            ),
            (
                "relationship name",
                relationship(
                    &"R".repeat(MAX_RELATIONSHIP_TYPE_BYTES + 1),
                    RelationshipDefinition::new(""),
                ),
            ),
            (
                "relationship characters",
                relationship("bad rel", RelationshipDefinition::new("")),
            ),
            (
                "endpoint label",
                relationship(
                    "R",
                    RelationshipDefinition::new("").endpoint("A", "L".repeat(MAX_LABEL_BYTES + 1)),
                ),
            ),
            (
                "endpoint characters",
                relationship("R", RelationshipDefinition::new("").endpoint("a-b", "A")),
            ),
            (
                "property name",
                label(
                    "A",
                    LabelDefinition::new("").property(
                        "p".repeat(MAX_FIELD_NAME_BYTES + 1),
                        property(PropertyType::Boolean),
                    ),
                ),
            ),
            (
                "empty property name",
                label(
                    "A",
                    LabelDefinition::new("").property("", property(PropertyType::Boolean)),
                ),
            ),
            (
                "range index on a string",
                label(
                    "A",
                    LabelDefinition::new("").property(
                        "p",
                        property(PropertyType::String).index(PropertyIndexKind::Range),
                    ),
                ),
            ),
            (
                "unique relationship property",
                relationship(
                    "R",
                    RelationshipDefinition::new("")
                        .property("w", property(PropertyType::Integer).unique()),
                ),
            ),
            (
                "indexed relationship property",
                relationship(
                    "R",
                    RelationshipDefinition::new("").property(
                        "w",
                        property(PropertyType::Integer).index(PropertyIndexKind::Exact),
                    ),
                ),
            ),
        ];
        for (what, schema) in cases {
            assert!(
                schema.validate().is_err(),
                "{what} must break an engine rule"
            );
            let mut invalid = base.clone();
            invalid.schema = schema.clone();
            assert_eq!(encode(&invalid), Err(CodecError::Corrupt), "{what}");
            let crafted = splice_schema(&base_bytes, &schema_section(&schema));
            assert_eq!(decode(&crafted), Err(CodecError::Corrupt), "{what}");
        }
    }

    #[test]
    fn schema_codec_refuses_data_violating_declared_rules_via_engine_checks() {
        let state = schema_state();
        let source = GraphSource::from_graph(state.graph(GraphId(3)).unwrap()).unwrap();
        let section = schema_section(&source.schema);
        let mutations: [(&str, SourceMutation); 9] = [
            ("required node property", |source| {
                source.nodes[0].properties.remove("active");
            }),
            ("unique node property", |source| {
                source.nodes[1]
                    .properties
                    .insert("email".into(), text("a@x"));
            }),
            ("node property type", |source| {
                source.nodes[0].properties.insert("age".into(), text("old"));
            }),
            ("unique range-indexed property", |source| {
                source.nodes[5].properties.insert("code".into(), int(7));
            }),
            ("indexed property type", |source| {
                source.nodes[3].properties.insert("code".into(), float(7.0));
            }),
            ("endpoints", |source| source.edges[0].source = 5),
            ("unique triple", |source| source.edges[1].source = 1),
            ("required edge property", |source| {
                source.edges[0].properties = Properties::new();
            }),
            ("edge property type", |source| {
                source.edges[2].properties.insert("weight".into(), int(1));
            }),
        ];
        for (what, mutate) in mutations {
            let mut invalid = source.clone();
            mutate(&mut invalid);
            assert_eq!(encode(&invalid), Err(CodecError::Corrupt), "{what}");
            invalid.schema = GraphSchema::default();
            let schemaless = encode(&invalid).unwrap();
            assert_eq!(
                decode(&splice_schema(&schemaless, &section)),
                Err(CodecError::Corrupt),
                "{what}"
            );
        }

        let mut keyless = fixture_graph(&json!({
            "nodes": [{"id": 1, "labels": ["Person"]}],
            "edges": [],
        }));
        let schema = GraphSchema::default().label("Person", LabelDefinition::new(""));
        let refused = CodecError::UnsupportedSource("schema with keyless nodes");
        let schemaless = encode(&keyless).unwrap();
        keyless.schema = schema.clone();
        assert_eq!(encode(&keyless), Err(refused.clone()));
        assert_eq!(
            decode(&splice_schema(&schemaless, &schema_section(&schema))),
            Err(refused)
        );
    }

    #[test]
    fn schema_engine_check_spans_multiple_snapshot_bodies() {
        let count = MAX_BATCH_ENTRIES as u64 + 10;
        let mut source = fixture_graph(&json!({"nodes": [], "edges": []}));
        source.schema = GraphSchema::default().label(
            "N",
            LabelDefinition::new("").property(
                "k",
                PropertyDefinition::new(PropertyType::Integer)
                    .required()
                    .unique(),
            ),
        );
        source.nodes = (0..count)
            .map(|id| SourceNode {
                id,
                key: Some(format!("n{id}")),
                labels: vec!["N".into()],
                revision: 0,
                deadline: None,
                properties: props([("k", int(id as i64))]),
                attachments: BTreeMap::new(),
            })
            .collect();
        source.next_node_id = count;
        let bytes = encode(&source).unwrap();
        assert_eq!(decode(&bytes).unwrap().source, source);
        let state = engine_state(&source).unwrap();
        assert_eq!(
            state.graph(GraphId(0)).unwrap().nodes().count(),
            count as usize
        );

        // The clash sits past the first full body, so only a check over the
        // whole multi-body load can see it.
        source.nodes.last_mut().unwrap().properties = props([("k", int(1))]);
        assert_eq!(encode(&source), Err(CodecError::Corrupt));
    }

    #[test]
    fn decoder_refuses_prior_payload_versions() {
        let source = fixture_graph(&json!({
            "nodes": [{"id": 1, "labels": ["A"]}],
            "edges": [],
        }));
        let bytes = encode(&source).unwrap();
        assert_eq!(&bytes[..8], b"VLGCSR06");
        for magic in [
            b"VLGCSR01",
            b"VLGCSR02",
            b"VLGCSR03",
            b"VLGCSR04",
            b"VLGCSR05",
        ] {
            let mut old = bytes.clone();
            old[..8].copy_from_slice(magic);
            assert_eq!(decode(&old), Err(CodecError::Corrupt));
        }
        // A genuine version 4 payload: the same bytes without the schema
        // section, which version 4 did not have.
        let span = schema_span(&bytes);
        let mut version_4 = [&bytes[..span.start], &bytes[span.end..]].concat();
        version_4[..8].copy_from_slice(b"VLGCSR04");
        assert_eq!(decode(&version_4), Err(CodecError::Corrupt));
        version_4[..8].copy_from_slice(MAGIC);
        assert_eq!(decode(&version_4), Err(CodecError::Corrupt));
    }

    fn text(value: &str) -> PropertyValue {
        PropertyValue::String(value.to_owned())
    }

    fn int(value: i64) -> PropertyValue {
        PropertyValue::Integer(value)
    }

    fn float(value: f64) -> PropertyValue {
        PropertyValue::Float(value)
    }

    fn boolean(value: bool) -> PropertyValue {
        PropertyValue::Boolean(value)
    }

    fn props<const N: usize>(values: [(&str, PropertyValue); N]) -> Properties {
        values
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect()
    }

    fn put_node(id: u64, key: &str, labels: &[&str], properties: Properties) -> GraphEntry {
        GraphEntry::PutNode {
            node_id: NodeId(id),
            key: key.to_owned(),
            labels: labels.iter().map(|label| (*label).to_owned()).collect(),
            properties,
        }
    }

    fn put_edge(
        id: u64,
        source: u64,
        destination: u64,
        relationship: &str,
        key: Option<&str>,
        properties: Properties,
    ) -> GraphEntry {
        GraphEntry::PutEdge {
            edge_id: EdgeId(id),
            source: NodeId(source),
            destination: NodeId(destination),
            relationship: relationship.to_owned(),
            key: key.map(str::to_owned),
            properties,
        }
    }

    fn equals(field: &str, value: PropertyValue) -> PropertyCondition {
        PropertyCondition::Eq {
            field: field.to_owned(),
            value,
        }
    }

    fn between(field: &str, min: PropertyValue, max: PropertyValue) -> PropertyCondition {
        PropertyCondition::Range {
            field: field.to_owned(),
            min: Some(min),
            max: Some(max),
        }
    }

    fn violation(rule: &'static str, element: &str, property: Option<&str>) -> GraphError {
        GraphError::SchemaViolation {
            rule,
            element: element.to_owned(),
            property: property.map(str::to_owned),
        }
    }

    /// Every declaration kind the engine schema supports: typed, required,
    /// unique, exact- and range-indexed label properties, property-less and
    /// never-carried labels, endpoint pairs, both multiplicities and typed
    /// required relationship properties.
    fn declared_schema() -> GraphSchema {
        let property = PropertyDefinition::new;
        GraphSchema::default()
            .label(
                "Person",
                LabelDefinition::new("people λ")
                    .property("email", property(PropertyType::String).required().unique())
                    .property(
                        "age",
                        property(PropertyType::Integer).index(PropertyIndexKind::Range),
                    )
                    .property(
                        "score",
                        property(PropertyType::Float).index(PropertyIndexKind::Range),
                    )
                    .property(
                        "team",
                        property(PropertyType::String).index(PropertyIndexKind::Exact),
                    )
                    .property("active", property(PropertyType::Boolean).required())
                    .property("home", property(PropertyType::GeoPoint)),
            )
            .label(
                "Company",
                LabelDefinition::new("").property(
                    "code",
                    property(PropertyType::Integer)
                        .unique()
                        .index(PropertyIndexKind::Range),
                ),
            )
            .label("Archived", LabelDefinition::new("soft deleted"))
            .label("Ghost", LabelDefinition::new("declared, never carried"))
            .relationship(
                "WORKS_AT",
                RelationshipDefinition::new("employment")
                    .endpoint("Person", "Company")
                    .multiplicity(Multiplicity::UniqueTriple)
                    .property("since", property(PropertyType::Integer).required()),
            )
            .relationship(
                "KNOWS",
                RelationshipDefinition::new("")
                    .endpoint("Person", "Person")
                    .endpoint("Company", "Person")
                    .property("weight", property(PropertyType::Float)),
            )
            .relationship("UNUSED_REL", RelationshipDefinition::new("never used"))
    }

    /// A schema-bearing graph built only through engine writes: a node
    /// re-put after creation, a deleted indexed node, deadlines, an
    /// attachment, an undeclared relationship and raised high-water marks.
    fn schema_state() -> GraphState {
        let alice = |age| {
            props([
                ("active", boolean(true)),
                ("age", int(age)),
                ("email", text("a@x")),
                (
                    "home",
                    PropertyValue::GeoPoint {
                        latitude: 47.5,
                        longitude: -122.25,
                    },
                ),
                ("score", float(2.5)),
                ("team", text("red")),
            ])
        };
        let bodies = [
            (
                1,
                vec![
                    GraphEntry::CreateGraph {
                        name: "people".into(),
                    },
                    GraphEntry::PutSchema {
                        schema: declared_schema(),
                    },
                    put_node(1, "alice", &["Person"], alice(29)),
                    put_node(
                        2,
                        "bob",
                        &["Person"],
                        props([
                            ("active", boolean(false)),
                            ("age", int(40)),
                            ("email", text("b@x")),
                            ("score", float(-0.0)),
                            ("team", text("red")),
                        ]),
                    ),
                    put_node(
                        3,
                        "carol",
                        &["Archived", "Person"],
                        props([
                            ("active", boolean(true)),
                            ("age", int(30)),
                            ("email", text("c@x")),
                            ("score", float(0.0)),
                        ]),
                    ),
                    GraphEntry::NodeExpiry {
                        node_id: NodeId(3),
                        expires_at_ms: Some(50),
                    },
                    put_node(4, "acme", &["Company"], props([("code", int(7))])),
                    put_node(5, "free", &[], props([("anything", text("goes"))])),
                    put_node(
                        6,
                        "beta",
                        &["Company"],
                        props([("code", int(-3)), ("extra", boolean(true))]),
                    ),
                    put_node(
                        7,
                        "temp",
                        &["Person"],
                        props([
                            ("active", boolean(true)),
                            ("age", int(99)),
                            ("email", text("t@x")),
                        ]),
                    ),
                    put_edge(10, 1, 4, "WORKS_AT", None, props([("since", int(2020))])),
                    put_edge(
                        11,
                        2,
                        4,
                        "WORKS_AT",
                        Some("bob-acme"),
                        props([("since", int(2021))]),
                    ),
                    put_edge(12, 1, 2, "KNOWS", None, props([("weight", float(0.5))])),
                    GraphEntry::EdgeExpiry {
                        edge_id: EdgeId(12),
                        expires_at_ms: Some(60),
                    },
                    put_edge(13, 4, 3, "KNOWS", None, Properties::new()),
                    put_edge(14, 5, 5, "LIKES", None, Properties::new()),
                    GraphEntry::Attach {
                        node_id: NodeId(1),
                        collection: 3,
                        record_id: 9,
                    },
                ],
            ),
            (2, vec![put_node(1, "alice", &["Person"], alice(31))]),
            (
                3,
                vec![
                    GraphEntry::DeleteNode { node_id: NodeId(7) },
                    GraphEntry::GraphHighWater {
                        next_node_id: 100,
                        next_edge_id: 200,
                    },
                ],
            ),
        ];
        let mut state = GraphState::new();
        for (sequence, entries) in bodies {
            state
                .apply(
                    sequence,
                    &GraphBody {
                        graph_id: GraphId(3),
                        entries,
                    },
                )
                .unwrap();
        }
        state
    }

    fn find(
        graph: &Graph,
        label: Option<&str>,
        conditions: &[PropertyCondition],
        now_ms: u64,
        explain: bool,
    ) -> QueryResult {
        let clock = || 0;
        let mut context = ExecutionContext::new(graph.graph_sequence(), &clock);
        context.now_ms = now_ms;
        let query = GraphQuery {
            op: QueryOp::FindNodes {
                label: label.map(str::to_owned),
                conditions: conditions.to_vec(),
                limit: None,
                cursor: None,
            },
            projection: PropertyProjection::All,
            explain,
        };
        execute(graph, &query, &GraphLimits::default(), &context).unwrap()
    }

    /// The byte range of the schema section of an encoded payload.
    fn schema_span(bytes: &[u8]) -> std::ops::Range<usize> {
        let mut reader = Reader::new(bytes);
        reader.take(MAGIC.len()).unwrap();
        reader.u16().unwrap();
        reader.string(63).unwrap();
        reader.u64().unwrap();
        reader.u64().unwrap();
        reader.u64().unwrap();
        let start = reader.at;
        reader.schema().unwrap();
        start..reader.at
    }

    fn splice_schema(bytes: &[u8], section: &[u8]) -> Vec<u8> {
        let span = schema_span(bytes);
        [&bytes[..span.start], section, &bytes[span.end..]].concat()
    }

    fn schema_section(schema: &GraphSchema) -> Vec<u8> {
        let mut section = Vec::new();
        put_schema(&mut section, schema).unwrap();
        section
    }

    /// One label or relationship written byte by byte, independently of
    /// `put_schema`, so tests can pin the layout and craft malformed input.
    struct RawDefinition {
        name: Vec<u8>,
        description: Vec<u8>,
        multiplicity: u8,
        endpoints: Vec<(Vec<u8>, Vec<u8>)>,
        properties: Vec<(Vec<u8>, u8, u8)>,
    }

    fn raw(name: &str) -> RawDefinition {
        RawDefinition {
            name: name.as_bytes().to_vec(),
            description: Vec::new(),
            multiplicity: 1,
            endpoints: Vec::new(),
            properties: Vec::new(),
        }
    }

    fn raw_schema(labels: &[RawDefinition], relationships: &[RawDefinition]) -> Vec<u8> {
        fn count(out: &mut Vec<u8>, len: usize) {
            out.extend_from_slice(&u32::try_from(len).unwrap().to_le_bytes());
        }
        fn bytes(out: &mut Vec<u8>, value: &[u8]) {
            count(out, value.len());
            out.extend_from_slice(value);
        }
        fn properties(out: &mut Vec<u8>, values: &[(Vec<u8>, u8, u8)]) {
            count(out, values.len());
            for (name, value_type, flags) in values {
                bytes(out, name);
                out.extend_from_slice(&[*value_type, *flags]);
            }
        }
        let mut out = Vec::new();
        count(&mut out, labels.len());
        for label in labels {
            bytes(&mut out, &label.name);
            bytes(&mut out, &label.description);
            properties(&mut out, &label.properties);
        }
        count(&mut out, relationships.len());
        for relationship in relationships {
            bytes(&mut out, &relationship.name);
            bytes(&mut out, &relationship.description);
            out.push(relationship.multiplicity);
            count(&mut out, relationship.endpoints.len());
            for (source, destination) in &relationship.endpoints {
                bytes(&mut out, source);
                bytes(&mut out, destination);
            }
            properties(&mut out, &relationship.properties);
        }
        out
    }

    fn with_u32(mut bytes: Vec<u8>, at: usize, value: u32) -> Vec<u8> {
        bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
        bytes
    }

    /// A schema at every engine schema limit at once.
    fn limit_schema() -> GraphSchema {
        let mut wide = LabelDefinition::new("d".repeat(MAX_SCHEMA_DESCRIPTION_BYTES));
        for index in 0..MAX_SCHEMA_PROPERTIES {
            let definition = PropertyDefinition::new(PropertyType::Integer);
            let definition = match index {
                index if index >= MAX_INDEXED_PROPERTIES => definition,
                index if index % 3 == 0 => definition.unique(),
                index if index % 3 == 1 => definition.index(PropertyIndexKind::Exact),
                _ => definition.index(PropertyIndexKind::Range),
            };
            let name = format!("{}{index:02}", "p".repeat(MAX_FIELD_NAME_BYTES - 2));
            wide = wide.property(name, definition);
        }
        let mut schema = GraphSchema::default().label("L".repeat(MAX_LABEL_BYTES), wide);
        for index in 1..MAX_SCHEMA_LABELS {
            schema = schema.label(format!("L{index:03}"), LabelDefinition::new(""));
        }
        let mut long = RelationshipDefinition::new("r".repeat(MAX_SCHEMA_DESCRIPTION_BYTES));
        for index in 0..MAX_SCHEMA_ENDPOINTS {
            long = long.endpoint(format!("S{index:02}"), "D".repeat(MAX_LABEL_BYTES));
        }
        for index in 0..MAX_SCHEMA_PROPERTIES {
            long = long.property(
                format!("w{index:02}"),
                PropertyDefinition::new(PropertyType::Float).required(),
            );
        }
        schema = schema.relationship("R".repeat(MAX_RELATIONSHIP_TYPE_BYTES), long);
        for index in 1..MAX_SCHEMA_RELATIONSHIPS {
            schema = schema.relationship(format!("R{index:03}"), RelationshipDefinition::new(""));
        }
        schema
    }
}
