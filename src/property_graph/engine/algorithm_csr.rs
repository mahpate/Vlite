use crate::compact::graph::view::CompactGraphView;
use crate::property_graph::engine::algorithm::{EdgeRef, Source};
use crate::property_graph::engine::{Direction, EdgeId, NodeId};

pub(crate) struct CsrSource<'g> {
    view: &'g CompactGraphView,
}

impl<'g> CsrSource<'g> {
    pub(crate) fn new(view: &'g CompactGraphView) -> Self {
        Self { view }
    }
}

impl Source for CsrSource<'_> {
    fn node(&self, id: NodeId) -> Option<NodeId> {
        self.view.physical().node(id).map(|node| node.id)
    }

    fn node_by_key(&self, key: &str) -> Option<NodeId> {
        self.view.algorithm_node_by_key(key).map(|node| node.id)
    }

    fn carries(&self, id: NodeId, labels: &[&str]) -> bool {
        self.view.physical().node(id).is_some_and(|node| {
            labels
                .iter()
                .all(|label| node.labels.iter().any(|stored| stored == label))
        })
    }

    fn edge(&self, id: EdgeId) -> Option<EdgeRef<'_>> {
        self.view.physical().edge(id).map(|edge| EdgeRef {
            id: edge.id,
            source: edge.source,
            destination: edge.destination,
            relationship: edge.relationship,
            properties: edge.properties,
        })
    }

    fn may_have_due(&self, _clock: u64) -> bool {
        true
    }

    fn node_expired(&self, id: NodeId, clock: u64) -> bool {
        self.view.read(clock, None).node_expired(id)
    }

    fn edge_expired(&self, edge: &EdgeRef<'_>, clock: u64) -> bool {
        self.view.read(clock, None).edge_expired(edge.id)
    }

    fn adjacency<'g>(
        &'g self,
        node: NodeId,
        direction: Direction,
        after: Option<(&str, EdgeId)>,
    ) -> impl Iterator<Item = (&'g str, EdgeId)> + 'g {
        self.view.algorithm_adjacency(node, direction, after)
    }
}

#[cfg(test)]
mod contract_tests {
    use super::*;
    use crate::compact::graph::view::history::pinned_views;
    use crate::property_graph::engine::{GraphBody, GraphEntry, GraphId, GraphState, Properties};
    use std::collections::BTreeSet;

    const GRAPH: GraphId = GraphId(0);

    fn apply(state: &mut GraphState, sequence: u64, entries: Vec<GraphEntry>) {
        state
            .apply(
                sequence,
                &GraphBody {
                    graph_id: GRAPH,
                    entries,
                },
            )
            .unwrap();
    }

    fn history() -> Vec<GraphState> {
        let mut state = GraphState::new();
        apply(
            &mut state,
            1,
            vec![
                GraphEntry::CreateGraph {
                    name: "csr_source_contract".into(),
                },
                GraphEntry::PutNode {
                    node_id: NodeId(0),
                    key: "a".into(),
                    labels: BTreeSet::from(["A".into()]),
                    properties: Properties::new(),
                },
                GraphEntry::PutNode {
                    node_id: NodeId(1),
                    key: "b".into(),
                    labels: BTreeSet::from(["B".into()]),
                    properties: Properties::new(),
                },
                GraphEntry::PutEdge {
                    edge_id: EdgeId(0),
                    source: NodeId(0),
                    destination: NodeId(1),
                    relationship: "R".into(),
                    key: None,
                    properties: Properties::new(),
                },
            ],
        );
        let mut states = vec![state.clone()];
        apply(
            &mut state,
            2,
            vec![
                GraphEntry::PutNode {
                    node_id: NodeId(2),
                    key: "c".into(),
                    labels: BTreeSet::from(["A".into()]),
                    properties: Properties::new(),
                },
                GraphEntry::PutEdge {
                    edge_id: EdgeId(1),
                    source: NodeId(0),
                    destination: NodeId(2),
                    relationship: "S".into(),
                    key: None,
                    properties: Properties::new(),
                },
                GraphEntry::NodeExpiry {
                    node_id: NodeId(1),
                    expires_at_ms: Some(3),
                },
                GraphEntry::EdgeExpiry {
                    edge_id: EdgeId(1),
                    expires_at_ms: Some(4),
                },
            ],
        );
        states.push(state);
        states
    }

    fn assert_contract(view: &CompactGraphView, state: &GraphState) {
        let graph = state.graph(GRAPH).unwrap();
        let source = CsrSource::new(view);
        for node in graph.nodes() {
            assert_eq!(source.node(node.id), Some(node.id));
            assert_eq!(source.node_by_key(&node.key), Some(node.id));
            let labels: Vec<_> = node.labels.iter().map(String::as_str).collect();
            assert!(source.carries(node.id, &labels));
            for direction in [Direction::Out, Direction::In, Direction::Both] {
                assert_eq!(
                    source
                        .adjacency(node.id, direction, None)
                        .collect::<Vec<_>>(),
                    graph
                        .adjacency(node.id, direction, None, None)
                        .collect::<Vec<_>>()
                );
            }
        }
        assert_eq!(source.node_by_key("missing"), None);
        for edge in graph.edges() {
            let actual = source.edge(edge.id).unwrap();
            assert_eq!(actual.id, edge.id);
            assert_eq!(actual.source, edge.source);
            assert_eq!(actual.destination, edge.destination);
            assert_eq!(actual.relationship, edge.relationship);
            assert_eq!(actual.properties, &edge.properties);
        }
    }

    #[test]
    fn base_source_matches_the_resident_physical_contract() {
        let states = history();
        let views = pinned_views(GRAPH, &states, 16).unwrap();
        assert_contract(&views[0], &states[0]);
    }

    #[test]
    fn overlay_source_matches_the_resident_physical_and_ttl_contract() {
        let states = history();
        let views = pinned_views(GRAPH, &states, 16).unwrap();
        let source = CsrSource::new(&views[1]);
        assert_contract(&views[1], &states[1]);
        assert!(source.may_have_due(3));
        assert!(!source.node_expired(NodeId(1), 2));
        assert!(source.node_expired(NodeId(1), 3));
        let edge = source.edge(EdgeId(0)).unwrap();
        assert!(!source.edge_expired(&edge, 2));
        assert!(source.edge_expired(&edge, 3));
        let edge = source.edge(EdgeId(1)).unwrap();
        assert!(!source.edge_expired(&edge, 3));
        assert!(source.edge_expired(&edge, 4));
        assert!(source.carries(NodeId(0), &["A"]));
        assert!(!source.carries(NodeId(0), &["A", "B"]));
        assert_eq!(
            source
                .adjacency(NodeId(0), Direction::Out, Some(("R", EdgeId(0))))
                .collect::<Vec<_>>(),
            [("S", EdgeId(1))]
        );
    }
}
