use super::super::super::testkit::{TestGraph, edge, node};
use super::super::super::{Direction, EdgeId, GraphError, NodeId, Properties};
use super::super::{Selector, resolve};
use super::{EdgeRef, Source};
use std::cell::Cell;

#[test]
fn adjacency_order_matches_the_resident_contract() {
    let mut graph = TestGraph::new();
    graph.run(&[
        node("a", &[], Properties::new()),
        node("b", &[], Properties::new()),
        edge(None, "a", "S", "b", Properties::new()),
        edge(None, "a", "R", "b", Properties::new()),
        edge(None, "a", "R", "b", Properties::new()),
    ]);

    let entries: Vec<_> =
        Source::adjacency(graph.graph(), NodeId(0), Direction::Out, None).collect();
    assert_eq!(
        entries,
        vec![("R", EdgeId(1)), ("R", EdgeId(2)), ("S", EdgeId(0))]
    );
}

struct KeyProbe {
    lookups: Cell<u64>,
}

impl Source for KeyProbe {
    fn node(&self, _id: NodeId) -> Option<NodeId> {
        None
    }

    fn node_by_key(&self, _key: &str) -> Option<NodeId> {
        self.lookups.set(self.lookups.get() + 1);
        None
    }

    fn carries(&self, _id: NodeId, _labels: &[&str]) -> bool {
        false
    }

    fn edge(&self, _id: EdgeId) -> Option<EdgeRef<'_>> {
        None
    }

    fn may_have_due(&self, _clock: u64) -> bool {
        false
    }

    fn node_expired(&self, _id: NodeId, _clock: u64) -> bool {
        false
    }

    fn edge_expired(&self, _edge: &EdgeRef<'_>, _clock: u64) -> bool {
        false
    }

    fn adjacency<'g>(
        &'g self,
        _node: NodeId,
        _direction: Direction,
        _after: Option<(&str, EdgeId)>,
    ) -> impl Iterator<Item = (&'g str, EdgeId)> + 'g {
        std::iter::empty()
    }
}

#[test]
fn invalid_node_key_is_rejected_before_source_lookup() {
    let source = KeyProbe {
        lookups: Cell::new(0),
    };
    assert_eq!(
        resolve(&source, Selector::Key(""), None),
        Err(GraphError::InvalidNodeKey)
    );
    assert_eq!(source.lookups.get(), 0);
}
