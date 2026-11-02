//! Private pinned read model; no executor, WAL or checkpoint integration.

use super::overlay::OverlaySnapshot;
use super::*;
use crate::property_graph::engine::{Direction, PropertyProjection};
use std::sync::Arc;

mod algorithm;
pub(crate) mod history;

#[cfg(test)]
pub(crate) fn algorithm_test_take_visits() -> usize {
    algorithm::take_visits()
}

#[derive(Clone)]
pub(crate) struct CompactGraphView {
    base: Arc<CompactGraph>,
    overlay: Option<Arc<OverlaySnapshot>>,
    algorithm: Arc<algorithm::Index>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct NodeRef<'g> {
    pub(crate) id: NodeId,
    pub(crate) key: Option<&'g str>,
    pub(crate) labels: &'g [String],
    pub(crate) properties: &'g Properties,
    pub(crate) revision: u64,
    pub(crate) expires_at_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct EdgeRef<'g> {
    pub(crate) id: EdgeId,
    pub(crate) source: NodeId,
    pub(crate) destination: NodeId,
    pub(crate) relationship: &'g str,
    pub(crate) key: Option<&'g str>,
    pub(crate) properties: &'g Properties,
    pub(crate) revision: u64,
    pub(crate) expires_at_ms: Option<u64>,
    pub(crate) segment: Option<&'g Segment>,
}

/// All accessors on one read use the same retention and validity selectors.
#[derive(Clone, Copy)]
pub(crate) struct ReadView<'g> {
    view: &'g CompactGraphView,
    clock: Option<u64>,
    as_of: Option<i64>,
}

impl CompactGraphView {
    pub(crate) fn new(base: Arc<CompactGraph>) -> Self {
        let mut view = Self {
            base,
            overlay: None,
            algorithm: Arc::new(algorithm::Index::empty()),
        };
        view.algorithm = Arc::new(algorithm::Index::build(&view));
        view
    }

    pub(super) fn with_overlay(snapshot: OverlaySnapshot) -> Self {
        let mut view = Self {
            base: Arc::clone(&snapshot.base),
            overlay: Some(Arc::new(snapshot)),
            algorithm: Arc::new(algorithm::Index::empty()),
        };
        view.algorithm = Arc::new(algorithm::Index::build(&view));
        view
    }

    pub(crate) fn id(&self) -> GraphId {
        GraphId(self.base.source.graph_id)
    }

    pub(crate) fn name(&self) -> &str {
        &self.base.source.name
    }

    pub(crate) fn next_node_id(&self) -> u64 {
        self.overlay
            .as_ref()
            .map_or(self.base.source.next_node_id, |delta| delta.next_node_id)
    }

    pub(crate) fn next_edge_id(&self) -> u64 {
        self.overlay
            .as_ref()
            .map_or(self.base.source.next_edge_id, |delta| delta.next_edge_id)
    }

    pub(crate) fn graph_sequence(&self) -> u64 {
        self.overlay
            .as_ref()
            .map_or(self.base.source.sequence, |delta| delta.sequence)
    }

    pub(crate) fn schema(&self) -> &GraphSchema {
        &self.base.source.schema
    }

    /// Physical counts, matching Graph; ReadView::live_counts applies retention.
    pub(crate) fn node_count(&self) -> usize {
        self.node_ids()
            .filter(|&id| self.raw_node(id).is_some())
            .count()
    }

    pub(crate) fn edge_count(&self) -> usize {
        self.edge_ids()
            .filter(|&id| self.raw_edge(id).is_some())
            .count()
    }

    pub(crate) fn read(&self, clock: u64, as_of: Option<i64>) -> ReadView<'_> {
        ReadView {
            view: self,
            clock: Some(clock),
            as_of,
        }
    }

    /// Physical records for snapshot/planning seams; not a user-visible read.
    pub(crate) fn physical(&self) -> ReadView<'_> {
        ReadView {
            view: self,
            clock: None,
            as_of: None,
        }
    }

    fn raw_node(&self, id: u64) -> Option<&SourceNode> {
        if let Some(row) = self.overlay.as_ref().and_then(|delta| delta.nodes.get(&id)) {
            return row.as_ref();
        }
        self.base
            .node_ordinals
            .get(&id)
            .map(|&at| &self.base.source.nodes[at as usize])
    }

    fn raw_edge(&self, id: u64) -> Option<&SourceEdge> {
        if let Some(row) = self.overlay.as_ref().and_then(|delta| delta.edges.get(&id)) {
            return row.as_ref();
        }
        self.base
            .source
            .edges
            .binary_search_by_key(&id, |edge| edge.id)
            .ok()
            .map(|at| &self.base.source.edges[at])
    }

    fn node_ids(&self) -> impl Iterator<Item = u64> + '_ {
        merge_ids(
            self.base.source.nodes.iter().map(|node| node.id),
            self.overlay
                .iter()
                .flat_map(|delta| delta.nodes.keys().copied()),
        )
    }

    fn edge_ids(&self) -> impl Iterator<Item = u64> + '_ {
        merge_ids(
            self.base.source.edges.iter().map(|edge| edge.id),
            self.overlay
                .iter()
                .flat_map(|delta| delta.edges.keys().copied()),
        )
    }
}

fn merge_ids(
    left: impl Iterator<Item = u64>,
    right: impl Iterator<Item = u64>,
) -> impl Iterator<Item = u64> {
    let mut left = left.peekable();
    let mut right = right.peekable();
    std::iter::from_fn(move || match (left.peek(), right.peek()) {
        (Some(a), Some(b)) => match a.cmp(b) {
            std::cmp::Ordering::Less => left.next(),
            std::cmp::Ordering::Greater => right.next(),
            std::cmp::Ordering::Equal => {
                right.next();
                left.next()
            }
        },
        (Some(_), None) => left.next(),
        (None, Some(_)) => right.next(),
        (None, None) => None,
    })
}

fn project(properties: &Properties, projection: &PropertyProjection) -> Properties {
    match projection {
        PropertyProjection::All => properties.clone(),
        PropertyProjection::None => Properties::new(),
        PropertyProjection::Only(fields) => properties
            .iter()
            .filter(|(name, _)| fields.contains(*name))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
    }
}

impl<'g> ReadView<'g> {
    pub(crate) fn node(&self, id: NodeId) -> Option<NodeRef<'g>> {
        let node = self.view.raw_node(id.0)?;
        if self.node_expired(id) {
            return None;
        }
        Some(NodeRef {
            id,
            key: node.key.as_deref(),
            labels: &node.labels,
            properties: &node.properties,
            revision: node.revision,
            expires_at_ms: node.deadline,
        })
    }

    pub(crate) fn node_by_key(&self, key: &str) -> Option<NodeRef<'g>> {
        self.nodes().find(|node| node.key == Some(key))
    }

    pub(crate) fn edge(&self, id: EdgeId) -> Option<EdgeRef<'g>> {
        let edge = self.view.raw_edge(id.0)?;
        if self.edge_expired(id) {
            return None;
        }
        let segment = match (self.as_of, edge.segments.as_deref()) {
            (Some(time), Some(series)) => Some(
                crate::property_graph::engine::temporal::segment_at(series, time)?,
            ),
            _ => None,
        };
        Some(EdgeRef {
            id,
            source: NodeId(edge.source),
            destination: NodeId(edge.target),
            relationship: &edge.relationship,
            key: edge.key.as_deref(),
            properties: segment.map_or(&edge.properties, |segment| &segment.properties),
            revision: edge.revision,
            expires_at_ms: edge.deadline,
            segment,
        })
    }

    pub(crate) fn edge_by_key(&self, key: &str) -> Option<EdgeRef<'g>> {
        self.edges().find(|edge| edge.key == Some(key))
    }

    pub(crate) fn nodes(&self) -> impl Iterator<Item = NodeRef<'g>> + '_ {
        self.node_scan(None)
    }

    pub(crate) fn edges(&self) -> impl Iterator<Item = EdgeRef<'g>> + '_ {
        self.view.edge_ids().filter_map(|id| self.edge(EdgeId(id)))
    }

    pub(crate) fn node_scan(
        &self,
        after: Option<NodeId>,
    ) -> impl Iterator<Item = NodeRef<'g>> + '_ {
        self.view
            .node_ids()
            .filter(move |&id| after.is_none_or(|after| id > after.0))
            .filter_map(|id| self.node(NodeId(id)))
    }

    pub(crate) fn nodes_with_label(&self, label: &str) -> impl Iterator<Item = NodeId> + '_ {
        self.label_scan(label, None)
    }

    pub(crate) fn label_scan(
        &self,
        label: &str,
        after: Option<NodeId>,
    ) -> impl Iterator<Item = NodeId> + '_ {
        let label = label.to_owned();
        self.node_scan(after)
            .filter(move |node| node.labels.binary_search(&label).is_ok())
            .map(|node| node.id)
    }

    pub(crate) fn node_properties(
        &self,
        id: NodeId,
        projection: &PropertyProjection,
    ) -> Option<Properties> {
        self.node(id)
            .map(|node| project(node.properties, projection))
    }

    pub(crate) fn edge_properties(
        &self,
        id: EdgeId,
        projection: &PropertyProjection,
    ) -> Option<Properties> {
        self.edge(id)
            .map(|edge| project(edge.properties, projection))
    }

    pub(crate) fn attachments(&self, id: NodeId) -> impl Iterator<Item = (u16, u64)> + '_ {
        self.node(id)
            .and_then(|_| self.view.raw_node(id.0))
            .into_iter()
            .flat_map(|node| {
                node.attachments
                    .iter()
                    .map(|(&collection, &record)| (collection, record))
            })
    }

    /// Timelines ignore as-of selection but still enforce retention.
    pub(crate) fn segments(&self, id: EdgeId) -> Option<&'g [Segment]> {
        if self.edge_expired(id) {
            return None;
        }
        self.view.raw_edge(id.0)?.segments.as_deref()
    }

    pub(crate) fn node_expiry(&self, id: NodeId) -> Option<u64> {
        self.view.raw_node(id.0)?.deadline
    }

    pub(crate) fn edge_expiry(&self, id: EdgeId) -> Option<u64> {
        self.view.raw_edge(id.0)?.deadline
    }

    pub(crate) fn node_expired(&self, id: NodeId) -> bool {
        self.clock.is_some_and(|clock| {
            self.node_expiry(id)
                .is_some_and(|deadline| clock >= deadline)
        })
    }

    pub(crate) fn edge_expired(&self, id: EdgeId) -> bool {
        self.view.raw_edge(id.0).is_some_and(|edge| {
            self.clock
                .is_some_and(|clock| edge.deadline.is_some_and(|deadline| clock >= deadline))
                || self.node(NodeId(edge.source)).is_none()
                || self.node(NodeId(edge.target)).is_none()
        })
    }

    pub(crate) fn has_due(&self) -> bool {
        self.view.node_ids().any(|id| self.node_expired(NodeId(id)))
            || self.view.edge_ids().any(|id| {
                self.clock.is_some_and(|clock| {
                    self.edge_expiry(EdgeId(id))
                        .is_some_and(|deadline| clock >= deadline)
                })
            })
    }

    /// As Graph::live_counts, validity gaps do not change retention counts.
    pub(crate) fn live_counts(&self) -> (usize, usize) {
        let retained = ReadView {
            as_of: None,
            ..*self
        };
        (retained.nodes().count(), retained.edges().count())
    }

    pub(crate) fn adjacency(
        &self,
        node: NodeId,
        direction: Direction,
        types: Option<&BTreeSet<String>>,
        after: Option<(&str, EdgeId)>,
    ) -> impl Iterator<Item = (&'g str, EdgeId)> + use<'g> {
        let mut ids = BTreeSet::new();
        if self.node(node).is_some() {
            if let Some(&ordinal) = self.view.base.node_ordinals.get(&node.0) {
                for (csr, enabled) in [
                    (&self.view.base.out, direction != Direction::In),
                    (&self.view.base.incoming, direction != Direction::Out),
                ] {
                    if enabled {
                        let at = ordinal as usize;
                        for &edge in
                            &csr.edges[csr.offsets[at] as usize..csr.offsets[at + 1] as usize]
                        {
                            ids.insert(self.view.base.edge_rows[edge as usize].id);
                        }
                    }
                }
            }
            if let Some(additions) = self
                .view
                .overlay
                .as_ref()
                .and_then(|delta| delta.adjacency.get(&node.0))
            {
                ids.extend(additions);
            }
        }
        let mut entries: Vec<_> = ids
            .into_iter()
            .filter_map(|id| self.edge(EdgeId(id)))
            .filter(|edge| {
                (direction != Direction::In && edge.source == node
                    || direction != Direction::Out && edge.destination == node)
                    && types.is_none_or(|types| types.contains(edge.relationship))
                    && after.is_none_or(|after| (edge.relationship, edge.id) > after)
            })
            .map(|edge| (edge.relationship, edge.id))
            .collect();
        entries.sort_unstable();
        entries.into_iter()
    }

    pub(crate) fn edges_of(&self, node: NodeId, direction: Direction) -> Vec<EdgeId> {
        self.adjacency(node, direction, None, None)
            .map(|(_, id)| id)
            .collect()
    }

    pub(crate) fn edges_between(
        &self,
        source: NodeId,
        relationship: &str,
        destination: NodeId,
    ) -> impl Iterator<Item = EdgeId> + use<'g> {
        let types = BTreeSet::from([relationship.to_owned()]);
        self.adjacency(source, Direction::Out, Some(&types), None)
            .filter(move |&(_, id)| {
                self.edge(id)
                    .is_some_and(|edge| edge.destination == destination)
            })
            .map(|(_, id)| id)
            .collect::<Vec<_>>()
            .into_iter()
    }
}

#[cfg(test)]
mod tests;
