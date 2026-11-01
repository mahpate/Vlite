//! Private P4 graph source model and immutable CSR codec.
//!
//! This test-only module deliberately does not participate in checkpoint
//! framing. It owns only the graph-domain representation; the shared storage
//! owner must wire any future block dispatch.

use crate::property_graph::engine::{
    Graph, GraphEdge, GraphNode, Properties, PropertyValue, properties,
};
use std::collections::{BTreeMap, BTreeSet};

const MAGIC: &[u8; 8] = b"VLGCSR04";
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
    is_temporal: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct GraphSource {
    graph_id: u16,
    name: String,
    sequence: u64,
    next_node_id: u64,
    next_edge_id: u64,
    has_schema: bool,
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
struct CompactGraph {
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
        if !graph.schema().is_empty() {
            return Err(CodecError::UnsupportedSource("schema"));
        }
        let mut nodes = Vec::new();
        for node in graph.nodes() {
            nodes.push(source_node(
                node,
                graph.node_expiry(node.id),
                graph.attachments(node.id).collect(),
            ));
        }
        let mut edges = Vec::new();
        for edge in graph.edges() {
            if graph.segments(edge.id).is_some() {
                return Err(CodecError::UnsupportedSource("temporal segments"));
            }
            edges.push(source_edge(edge, graph.edge_expiry(edge.id), false));
        }
        Ok(Self {
            graph_id: graph.id().0,
            name: graph.name().to_owned(),
            sequence: graph.graph_sequence(),
            next_node_id: graph.next_node_id(),
            next_edge_id: graph.next_edge_id(),
            has_schema: false,
            nodes,
            edges,
        })
    }
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

fn source_edge(edge: &GraphEdge, deadline: Option<u64>, is_temporal: bool) -> SourceEdge {
    SourceEdge {
        id: edge.id.0,
        source: edge.source.0,
        target: edge.destination.0,
        relationship: edge.relationship.clone(),
        key: edge.key.clone(),
        revision: edge.revision,
        deadline,
        properties: edge.properties.clone(),
        is_temporal,
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
            is_temporal: false,
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
    if !reader.is_empty() {
        return Err(CodecError::Corrupt);
    }
    let mut graph = build(GraphSource {
        graph_id,
        name,
        sequence,
        next_node_id,
        next_edge_id,
        has_schema: false,
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
    if source.has_schema {
        return Err(CodecError::UnsupportedSource("schema"));
    }
    let mut property_names = BTreeSet::new();
    for node in &source.nodes {
        properties::validate(&node.properties).map_err(|_| CodecError::Corrupt)?;
        property_names.extend(node.properties.keys().cloned());
    }
    for edge in &source.edges {
        properties::validate(&edge.properties).map_err(|_| CodecError::Corrupt)?;
        property_names.extend(edge.properties.keys().cloned());
    }
    if source.edges.iter().any(|edge| edge.is_temporal) {
        return Err(CodecError::UnsupportedSource("temporal segments"));
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
    use crate::property_graph::engine::{GraphBody, GraphEntry, GraphId, GraphState, NodeId};
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
        for magic in [b"VLGCSR01", b"VLGCSR02", b"VLGCSR03"] {
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
        for magic in [b"VLGCSR01", b"VLGCSR02", b"VLGCSR03"] {
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
    fn graph_source_refuses_fields_not_yet_encoded() {
        let mut graph = fixture_graph(&json!({"nodes": [{"id": 1, "labels": []}], "edges": []}));
        graph.has_schema = true;
        assert_eq!(encode(&graph), Err(CodecError::UnsupportedSource("schema")));

        graph.has_schema = false;
        graph.edges.push(SourceEdge {
            id: 1,
            source: 1,
            target: 1,
            relationship: "r".into(),
            key: None,
            revision: 0,
            deadline: None,
            properties: Properties::new(),
            is_temporal: true,
        });
        assert_eq!(
            encode(&graph),
            Err(CodecError::UnsupportedSource("temporal segments"))
        );
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

    fn replace_key(bytes: &mut [u8], nodes: bool, ordinal: usize, value: &str) {
        let mut reader = Reader::new(bytes);
        reader.take(MAGIC.len()).unwrap();
        reader.u16().unwrap();
        reader.string(63).unwrap();
        reader.u64().unwrap();
        reader.u64().unwrap();
        reader.u64().unwrap();
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

    fn property_offsets(bytes: &[u8], node_ordinal: usize) -> PropertyOffsets {
        let mut reader = Reader::new(bytes);
        reader.take(MAGIC.len()).unwrap();
        reader.u16().unwrap();
        reader.string(63).unwrap();
        reader.u64().unwrap();
        reader.u64().unwrap();
        reader.u64().unwrap();
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
                is_temporal: false,
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
            has_schema: false,
            nodes,
            edges,
        }
    }
}
