//! Shared temporal edge visibility: validity at T, retention at clock C.

use super::model::GraphEdge;
use super::state::Graph;
use super::temporal::{Segment, segment_at};

/// Outer `None` means hidden. `Some(None)` is a visible legacy edge;
/// `Some(Some(segment))` identifies the unique valid temporal segment.
pub(crate) fn visible_edge<'g>(
    graph: &'g Graph,
    edge: &GraphEdge,
    as_of_ms: i64,
    ttl_clock_ms: u64,
) -> Option<Option<&'g Segment>> {
    if !edge_retained_at(graph, edge, ttl_clock_ms) {
        return None;
    }

    match graph.segments(edge.id) {
        Some(series) => segment_at(series, as_of_ms).map(Some),
        None => Some(None),
    }
}

/// Retention only, shared with timeline inspection that ignores validity.
pub(crate) fn edge_retained_at(graph: &Graph, edge: &GraphEdge, ttl_clock_ms: u64) -> bool {
    !graph.edge_expired(edge, ttl_clock_ms)
        && graph.node(edge.source).is_some()
        && graph.node(edge.destination).is_some()
}

#[cfg(test)]
mod tests {
    use super::super::codec::{GraphBody, GraphEntry, TemporalEntry};
    use super::super::model::{EdgeId, NodeId, Properties};
    use super::super::testkit::{G, TestGraph, edge, node};
    use super::*;

    fn fixture() -> TestGraph {
        let mut db = TestGraph::new();
        db.state.set_temporal(true).unwrap();
        db.run(&[
            node("a", &[], Properties::new()),
            node("b", &[], Properties::new()),
            edge(Some("legacy"), "a", "R", "b", Properties::new()),
        ]);
        db.commit(GraphBody {
            graph_id: G,
            entries: vec![GraphEntry::Temporal(TemporalEntry::PutEdge {
                edge_id: EdgeId(1),
                revision: 0,
                source: NodeId(0),
                destination: NodeId(1),
                relationship: "R".into(),
                key: Some("temporal".into()),
                segments: vec![
                    Segment {
                        valid_from: None,
                        valid_to: Some(-10),
                        properties: Properties::new(),
                    },
                    Segment {
                        valid_from: Some(0),
                        valid_to: Some(10),
                        properties: Properties::new(),
                    },
                    Segment {
                        valid_from: Some(10),
                        valid_to: Some(20),
                        properties: Properties::new(),
                    },
                    Segment {
                        valid_from: Some(30),
                        valid_to: None,
                        properties: Properties::new(),
                    },
                ],
            })],
        });
        db
    }

    #[test]
    fn selection_and_legacy_visibility_are_distinct() {
        let db = fixture();
        let graph = db.graph();
        let edge = graph.edge(EdgeId(1)).unwrap();
        for (t, bounds) in [
            (i64::MIN, Some((None, Some(-10)))),
            (-11, Some((None, Some(-10)))),
            (-10, None),
            (-1, None),
            (0, Some((Some(0), Some(10)))),
            (9, Some((Some(0), Some(10)))),
            (10, Some((Some(10), Some(20)))),
            (19, Some((Some(10), Some(20)))),
            (20, None),
            (29, None),
            (30, Some((Some(30), None))),
            (i64::MAX, Some((Some(30), None))),
        ] {
            assert_eq!(
                visible_edge(graph, edge, t, u64::MAX)
                    .flatten()
                    .map(|s| (s.valid_from, s.valid_to)),
                bounds,
                "{t}"
            );
            assert_eq!(
                visible_edge(graph, graph.edge(EdgeId(0)).unwrap(), t, u64::MAX),
                Some(None)
            );
        }
    }

    #[test]
    fn edge_and_both_node_deadlines_use_c_not_t() {
        for entry in [
            GraphEntry::EdgeExpiry {
                edge_id: EdgeId(1),
                expires_at_ms: Some(500),
            },
            GraphEntry::NodeExpiry {
                node_id: NodeId(0),
                expires_at_ms: Some(500),
            },
            GraphEntry::NodeExpiry {
                node_id: NodeId(1),
                expires_at_ms: Some(500),
            },
        ] {
            let mut db = fixture();
            db.commit(GraphBody {
                graph_id: G,
                entries: vec![entry],
            });
            let graph = db.graph();
            let edge = graph.edge(EdgeId(1)).unwrap();
            for t in [i64::MIN, 5, i64::MAX] {
                assert!(visible_edge(graph, edge, t, 499).is_some());
                assert!(visible_edge(graph, edge, t, 500).is_none());
                assert!(visible_edge(graph, edge, t, 501).is_none());
            }
        }
    }

    #[test]
    fn missing_endpoints_are_hidden_even_without_any_due_ttl() {
        let db = fixture();
        let graph = db.graph();
        for id in [EdgeId(0), EdgeId(1)] {
            for source in [false, true] {
                let mut edge = graph.edge(id).unwrap().clone();
                if source {
                    edge.source = NodeId(u64::MAX);
                } else {
                    edge.destination = NodeId(u64::MAX);
                }
                assert!(visible_edge(graph, &edge, 5, 0).is_none());
            }
        }
    }
}
