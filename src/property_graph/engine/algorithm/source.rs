use super::super::model::{Direction, EdgeId, GraphEdge, GraphNode, NodeId, Properties};
use super::super::state::Graph;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct EdgeRef<'g> {
    pub(crate) id: EdgeId,
    pub(crate) source: NodeId,
    pub(crate) destination: NodeId,
    pub(crate) relationship: &'g str,
    pub(crate) properties: &'g Properties,
}

impl<'g> From<&'g GraphEdge> for EdgeRef<'g> {
    fn from(edge: &'g GraphEdge) -> Self {
        Self {
            id: edge.id,
            source: edge.source,
            destination: edge.destination,
            relationship: &edge.relationship,
            properties: &edge.properties,
        }
    }
}

pub(crate) trait Source {
    fn node(&self, id: NodeId) -> Option<NodeId>;
    fn node_by_key(&self, key: &str) -> Option<NodeId>;
    fn carries(&self, id: NodeId, labels: &[&str]) -> bool;
    fn edge(&self, id: EdgeId) -> Option<EdgeRef<'_>>;
    fn may_have_due(&self, clock: u64) -> bool;
    fn node_expired(&self, id: NodeId, clock: u64) -> bool;
    fn edge_expired(&self, edge: &EdgeRef<'_>, clock: u64) -> bool;
    fn adjacency<'g>(
        &'g self,
        node: NodeId,
        direction: Direction,
        after: Option<(&str, EdgeId)>,
    ) -> impl Iterator<Item = (&'g str, EdgeId)> + 'g;
}

impl Source for Graph {
    fn node(&self, id: NodeId) -> Option<NodeId> {
        Graph::node(self, id).map(|node| node.id)
    }

    fn node_by_key(&self, key: &str) -> Option<NodeId> {
        Graph::node_by_key(self, key).map(|node| node.id)
    }

    fn carries(&self, id: NodeId, labels: &[&str]) -> bool {
        Graph::node(self, id)
            .is_some_and(|node: &GraphNode| labels.iter().all(|label| node.labels.contains(*label)))
    }

    fn edge(&self, id: EdgeId) -> Option<EdgeRef<'_>> {
        Graph::edge(self, id).map(EdgeRef::from)
    }

    fn may_have_due(&self, clock: u64) -> bool {
        self.has_due(clock)
    }

    fn node_expired(&self, id: NodeId, clock: u64) -> bool {
        Graph::node_expired(self, id, clock)
    }

    fn edge_expired(&self, edge: &EdgeRef<'_>, clock: u64) -> bool {
        Graph::edge_expiry(self, edge.id).is_some_and(|deadline| deadline <= clock)
            || Graph::node_expired(self, edge.source, clock)
            || Graph::node_expired(self, edge.destination, clock)
    }

    fn adjacency<'g>(
        &'g self,
        node: NodeId,
        direction: Direction,
        after: Option<(&str, EdgeId)>,
    ) -> impl Iterator<Item = (&'g str, EdgeId)> + 'g {
        Graph::adjacency(self, node, direction, None, after)
    }
}

#[cfg(test)]
mod tests;
