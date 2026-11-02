//! Private source reads over decoded CSR. Index construction is separate from
//! reads; neither requires rebuilding the engine graph.

use super::{CompactGraph, Csr, Properties, SourceEdge, SourceNode};
use crate::property_graph::engine::Direction;
use std::collections::{BTreeMap, BTreeSet};

struct CsrReader<'g> {
    graph: &'g CompactGraph,
    node_keys: BTreeMap<&'g str, u32>,
    edge_keys: BTreeMap<&'g str, u32>,
    edge_ordinals: BTreeMap<u64, u32>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ReadEdge<'g> {
    pub(super) edge: &'g SourceEdge,
    pub(super) properties: &'g Properties,
}

impl<'g> CsrReader<'g> {
    fn new(graph: &'g CompactGraph) -> Self {
        Self {
            graph,
            node_keys: graph
                .source
                .nodes
                .iter()
                .zip(0u32..)
                .filter_map(|(node, ordinal)| node.key.as_deref().map(|key| (key, ordinal)))
                .collect(),
            edge_keys: graph
                .source
                .edges
                .iter()
                .zip(0u32..)
                .filter_map(|(edge, ordinal)| edge.key.as_deref().map(|key| (key, ordinal)))
                .collect(),
            edge_ordinals: graph
                .edge_rows
                .iter()
                .zip(0u32..)
                .map(|(edge, ordinal)| (edge.id, ordinal))
                .collect(),
        }
    }

    fn node(&self, id: u64, clock: u64) -> Option<&'g SourceNode> {
        self.node_at(*self.graph.node_ordinals.get(&id)?, clock)
    }

    fn node_by_key(&self, key: &str, clock: u64) -> Option<&'g SourceNode> {
        self.node_at(*self.node_keys.get(key)?, clock)
    }

    fn node_at(&self, ordinal: u32, clock: u64) -> Option<&'g SourceNode> {
        let node = &self.graph.source.nodes[ordinal as usize];
        node.deadline
            .is_none_or(|deadline| clock < deadline)
            .then_some(node)
    }

    /// `None` selects ordinary engine-read semantics (retention only).
    fn edge(&self, id: u64, clock: u64, as_of: Option<i64>) -> Option<ReadEdge<'g>> {
        self.edge_at(*self.edge_ordinals.get(&id)?, clock, as_of)
    }

    fn edge_by_key(&self, key: &str, clock: u64, as_of: Option<i64>) -> Option<ReadEdge<'g>> {
        self.edge_at(*self.edge_keys.get(key)?, clock, as_of)
    }

    fn edge_at(&self, ordinal: u32, clock: u64, as_of: Option<i64>) -> Option<ReadEdge<'g>> {
        let edge = &self.graph.source.edges[ordinal as usize];
        let row = &self.graph.edge_rows[ordinal as usize];
        if edge.deadline.is_some_and(|deadline| clock >= deadline)
            || self.node_at(row.source, clock).is_none()
            || self.node_at(row.target, clock).is_none()
        {
            return None;
        }
        let properties = match (as_of, &edge.segments) {
            (Some(time), Some(segments)) => {
                let end = segments
                    .partition_point(|segment| segment.valid_from.is_none_or(|from| from <= time));
                let segment = segments.get(end.checked_sub(1)?)?;
                if segment.valid_to.is_some_and(|to| time >= to) {
                    return None;
                }
                &segment.properties
            }
            _ => &edge.properties,
        };
        Some(ReadEdge { edge, properties })
    }

    /// Merge the two canonical rows in linear degree work, deduplicating only
    /// the identical ordinal of a self-loop. Temporal selection is logarithmic
    /// in the series length, and endpoint checks use dense ordinals.
    fn neighbors(
        &self,
        node: u64,
        direction: Direction,
        types: Option<&BTreeSet<String>>,
        clock: u64,
        as_of: Option<i64>,
    ) -> Vec<ReadEdge<'g>> {
        let Some(&ordinal) = self.graph.node_ordinals.get(&node) else {
            return Vec::new();
        };
        if self.node_at(ordinal, clock).is_none() {
            return Vec::new();
        }
        let row = |csr: &'g Csr, enabled: bool| -> &'g [u32] {
            if !enabled {
                return &[];
            }
            let index = ordinal as usize;
            &csr.edges[csr.offsets[index] as usize..csr.offsets[index + 1] as usize]
        };
        let mut out = row(&self.graph.out, direction != Direction::In)
            .iter()
            .peekable();
        let mut incoming = row(&self.graph.incoming, direction != Direction::Out)
            .iter()
            .peekable();
        let key = |ordinal: u32| {
            let row = &self.graph.edge_rows[ordinal as usize];
            (row.relationship, row.id)
        };
        let mut edges = Vec::new();
        loop {
            let ordinal = match (out.peek(), incoming.peek()) {
                (Some(&&left), Some(&&right)) => match key(left).cmp(&key(right)) {
                    std::cmp::Ordering::Less => *out.next().unwrap(),
                    std::cmp::Ordering::Greater => *incoming.next().unwrap(),
                    std::cmp::Ordering::Equal => {
                        incoming.next();
                        *out.next().unwrap()
                    }
                },
                (Some(_), None) => *out.next().unwrap(),
                (None, Some(_)) => *incoming.next().unwrap(),
                (None, None) => break,
            };
            let edge = &self.graph.source.edges[ordinal as usize];
            if types.is_some_and(|types| !types.contains(&edge.relationship)) {
                continue;
            }
            if let Some(edge) = self.edge_at(ordinal, clock, as_of) {
                edges.push(edge);
            }
        }
        edges
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod seeded;
