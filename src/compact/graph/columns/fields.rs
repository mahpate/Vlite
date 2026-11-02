use super::*;
use crate::property_graph::engine::{EdgeView, NodeView};

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

struct Keys {
    rows: Bytes,
    order: Box<[u32]>,
}

impl Keys {
    fn build(keys: &[Option<&str>]) -> Result<Self> {
        let rows = Bytes::build(keys.iter().map(|key| key.unwrap_or("").as_bytes().to_vec()))?;
        let mut order: Vec<_> = keys
            .iter()
            .enumerate()
            .filter_map(|(at, key)| key.map(|_| at as u32))
            .collect();
        order.sort_unstable_by_key(|&at| keys[at as usize]);
        Ok(Self {
            rows,
            order: order.into_boxed_slice(),
        })
    }

    fn get(&self, at: usize) -> Result<Option<&str>> {
        let bytes = self.rows.get(at)?;
        if bytes.is_empty() {
            return Ok(None);
        }
        Ok(Some(
            std::str::from_utf8(bytes).map_err(|_| CodecError::Corrupt)?,
        ))
    }

    fn find(&self, key: &str) -> Option<usize> {
        self.order
            .binary_search_by(|&at| {
                // Admitted key rows are UTF-8 and offsets are already checked.
                self.rows
                    .get(at as usize)
                    .expect("admitted key row")
                    .cmp(key.as_bytes())
            })
            .ok()
            .map(|at| self.order[at] as usize)
    }

    fn heap_bytes(&self) -> usize {
        self.rows.heap_bytes() + 4 * self.order.len()
    }
}

/// Fields only: no retained GraphSource, Properties, per-row String/map or topology.
struct PackedFields {
    graph_id: u16,
    sequence: u64,
    nodes: Table,
    edges: Table,
    segments: Table,
    segment_offsets: Box<[u32]>,
    node_keys: Keys,
    edge_keys: Keys,
    attachments: Bytes,
    owners: Box<[(u16, u64, u64)]>,
}

impl PackedFields {
    fn build(source: &GraphSource) -> Result<Self> {
        // Existing admission owns schemas, keys, endpoints, segments and owners.
        // This temporary image is discarded; VLGCSR06 is not modified.
        encode(source)?;
        if source.nodes.windows(2).any(|p| p[0].id >= p[1].id)
            || source.edges.windows(2).any(|p| p[0].id >= p[1].id)
        {
            return corrupt();
        }
        let nodes: Vec<_> = source
            .nodes
            .iter()
            .map(|node| (node.id, &node.properties))
            .collect();
        let edges: Vec<_> = source
            .edges
            .iter()
            .map(|edge| (edge.id, &edge.properties))
            .collect();
        let mut segments = Vec::new();
        let mut segment_offsets = vec![0];
        for edge in &source.edges {
            for segment in edge.segments.iter().flatten() {
                segments.push((segments.len() as u64, &segment.properties));
            }
            segment_offsets.push(u32_of(segments.len())?);
        }
        let node_keys: Vec<_> = source
            .nodes
            .iter()
            .map(|node| node.key.as_deref())
            .collect();
        let edge_keys: Vec<_> = source
            .edges
            .iter()
            .map(|edge| edge.key.as_deref())
            .collect();
        let attachments = Bytes::build(source.nodes.iter().map(|node| {
            let mut out = Vec::new();
            for (collection, record) in &node.attachments {
                out.extend_from_slice(&collection.to_le_bytes());
                out.extend_from_slice(&record.to_le_bytes());
            }
            out
        }))?;
        let mut owners: Vec<_> = source
            .nodes
            .iter()
            .flat_map(|node| {
                node.attachments
                    .iter()
                    .map(move |(&collection, &record)| (collection, record, node.id))
            })
            .collect();
        owners.sort_unstable();
        let result = Self {
            graph_id: source.graph_id,
            sequence: source.sequence,
            nodes: Table::build(&nodes)?,
            edges: Table::build(&edges)?,
            segments: Table::build(&segments)?,
            segment_offsets: segment_offsets.into_boxed_slice(),
            node_keys: Keys::build(&node_keys)?,
            edge_keys: Keys::build(&edge_keys)?,
            attachments,
            owners: owners.into_boxed_slice(),
        };
        bounded(result.payload_bytes()?)?;
        Ok(result)
    }

    fn node_by_key(&self, key: &str) -> Option<u64> {
        self.node_keys.find(key).map(|at| self.nodes.ids[at])
    }

    fn edge_by_key(&self, key: &str) -> Option<u64> {
        self.edge_keys.find(key).map(|at| self.edges.ids[at])
    }

    fn attachments(&self, id: u64) -> Result<Option<BTreeMap<u16, u64>>> {
        let Ok(at) = self.nodes.ids.binary_search(&id) else {
            return Ok(None);
        };
        let mut cursor = Cursor(self.attachments.get(at)?);
        let mut result = BTreeMap::new();
        while !cursor.0.is_empty() {
            let collection = u16::from_le_bytes(cursor.array()?);
            let record = u64::from_le_bytes(cursor.array()?);
            if result
                .last_key_value()
                .is_some_and(|(&last, _)| last >= collection)
            {
                return corrupt();
            }
            result.insert(collection, record);
        }
        Ok(Some(result))
    }

    fn owner(&self, collection: u16, record: u64) -> Option<u64> {
        self.owners
            .binary_search_by_key(&(collection, record), |row| (row.0, row.1))
            .ok()
            .map(|at| self.owners[at].2)
    }

    /// Explicit owned projection from packed fields plus unpacked metadata.
    /// Retention/as-of selection belongs to a future consumer, not this model.
    fn physical_node_view(
        &self,
        metadata: &SourceNode,
        projection: &PropertyProjection,
    ) -> Result<NodeView> {
        let at = self
            .nodes
            .ids
            .binary_search(&metadata.id)
            .map_err(|_| CodecError::Corrupt)?;
        Ok(NodeView {
            id: NodeId(metadata.id),
            key: self
                .node_keys
                .get(at)?
                .ok_or(CodecError::UnsupportedSource("keyless node view"))?
                .into(),
            labels: metadata.labels.iter().cloned().collect(),
            properties: self
                .nodes
                .project(metadata.id, projection)?
                .ok_or(CodecError::Corrupt)?,
            revision: metadata.revision,
            expires_at_ms: metadata.deadline,
        })
    }

    fn physical_edge_view(
        &self,
        metadata: &SourceEdge,
        projection: &PropertyProjection,
        segment: Option<usize>,
    ) -> Result<EdgeView> {
        let at = self
            .edges
            .ids
            .binary_search(&metadata.id)
            .map_err(|_| CodecError::Corrupt)?;
        let endpoint_key = |id| {
            let at = self
                .nodes
                .ids
                .binary_search(&id)
                .map_err(|_| CodecError::Corrupt)?;
            self.node_keys
                .get(at)?
                .map(str::to_owned)
                .ok_or(CodecError::UnsupportedSource("keyless edge endpoint view"))
        };
        let properties = match segment {
            None => self.edges.project(metadata.id, projection)?,
            Some(index) => {
                let start = self.segment_offsets[at] as usize;
                let end = self.segment_offsets[at + 1] as usize;
                if index >= end - start {
                    return corrupt();
                }
                self.segments.project((start + index) as u64, projection)?
            }
        }
        .ok_or(CodecError::Corrupt)?;
        Ok(EdgeView {
            id: EdgeId(metadata.id),
            key: self.edge_keys.get(at)?.map(str::to_owned),
            relationship: metadata.relationship.clone(),
            source: NodeId(metadata.source),
            target: NodeId(metadata.target),
            source_key: endpoint_key(metadata.source)?,
            target_key: endpoint_key(metadata.target)?,
            properties,
            revision: metadata.revision,
            expires_at_ms: metadata.deadline,
        })
    }

    /// Explicit reconstruction for parity/admission, not serving or a borrowed view.
    fn restore(&self, shape: &GraphSource) -> Result<GraphSource> {
        if self.graph_id != shape.graph_id
            || self.sequence != shape.sequence
            || !self
                .nodes
                .ids
                .iter()
                .copied()
                .eq(shape.nodes.iter().map(|node| node.id))
            || !self
                .edges
                .ids
                .iter()
                .copied()
                .eq(shape.edges.iter().map(|edge| edge.id))
        {
            return corrupt();
        }
        let mut result = shape.clone();
        for (at, node) in result.nodes.iter_mut().enumerate() {
            node.properties = self
                .nodes
                .project(node.id, &PropertyProjection::All)?
                .ok_or(CodecError::Corrupt)?;
            node.key = self.node_keys.get(at)?.map(str::to_owned);
            node.attachments = self.attachments(node.id)?.ok_or(CodecError::Corrupt)?;
        }
        for (at, edge) in result.edges.iter_mut().enumerate() {
            edge.properties = self
                .edges
                .project(edge.id, &PropertyProjection::All)?
                .ok_or(CodecError::Corrupt)?;
            edge.key = self.edge_keys.get(at)?.map(str::to_owned);
            let start = self.segment_offsets[at] as usize;
            let end = self.segment_offsets[at + 1] as usize;
            if end - start != edge.segments.as_ref().map_or(0, Vec::len) {
                return corrupt();
            }
            for (index, segment) in edge.segments.iter_mut().flatten().enumerate() {
                segment.properties = self
                    .segments
                    .project((start + index) as u64, &PropertyProjection::All)?
                    .ok_or(CodecError::Corrupt)?;
            }
        }
        // Rebuild/check the same engine schema/unique indexes, never bypass them.
        encode(&result)?;
        Ok(result)
    }

    fn payload_bytes(&self) -> Result<usize> {
        // A component ledger, not a registered combined wire format. Include
        // identity scalars and actual owner-array padding, not a hypothetical
        // serialized owner representation.
        Ok(2 + 8
            + self.nodes.encode()?.len()
            + self.edges.encode()?.len()
            + self.segments.encode()?.len()
            + self.segment_offsets.len() * 4
            + self.node_keys.rows.heap_bytes()
            + self.node_keys.order.len() * 4
            + self.edge_keys.rows.heap_bytes()
            + self.edge_keys.order.len() * 4
            + self.attachments.heap_bytes()
            + self.owners.len() * std::mem::size_of::<(u16, u64, u64)>())
    }

    fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.nodes.heap_bytes()
            + self.edges.heap_bytes()
            + self.segments.heap_bytes()
            + self.segment_offsets.len() * 4
            + self.node_keys.heap_bytes()
            + self.edge_keys.heap_bytes()
            + self.attachments.heap_bytes()
            + self.owners.len() * std::mem::size_of::<(u16, u64, u64)>()
    }
}
