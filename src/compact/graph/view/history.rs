use super::super::*;
use super::CompactGraphView;
use super::overlay::{Change, Commit, Deadline, Overlay};
use crate::property_graph::engine::{GraphBody, GraphEntry};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub(crate) fn pinned_views(
    graph: GraphId,
    states: &[GraphState],
    cap: usize,
) -> Result<Vec<CompactGraphView>, String> {
    let first = states.first().ok_or("history requires a base state")?;
    let mut previous = source(first, graph)?;
    let base = Arc::new(
        decode(&encode(&previous).map_err(|error| format!("{error:?}"))?)
            .map_err(|error| format!("{error:?}"))?,
    );
    let mut overlay = Overlay::new(&base, cap);
    let mut views = vec![CompactGraphView::new(Arc::clone(&base))];
    for state in &states[1..] {
        let current = source(state, graph)?;
        overlay
            .apply(projection(&previous, &current))
            .map_err(|error| format!("{error:?}"))?;
        views.push(CompactGraphView::with_overlay(
            overlay
                .snapshot(Arc::clone(&base))
                .map_err(|error| format!("{error:?}"))?,
        ));
        previous = current;
    }
    Ok(views)
}

fn source(state: &GraphState, graph: GraphId) -> Result<GraphSource, String> {
    GraphSource::from_graph(
        state
            .graph(graph)
            .ok_or("graph missing from history state")?,
    )
    .map_err(|error| format!("{error:?}"))
}

fn projection(before: &GraphSource, after: &GraphSource) -> Commit {
    let old_nodes: BTreeMap<_, _> = before.nodes.iter().map(|row| (row.id, row)).collect();
    let new_nodes: BTreeMap<_, _> = after.nodes.iter().map(|row| (row.id, row)).collect();
    let old_edges: BTreeMap<_, _> = before.edges.iter().map(|row| (row.id, row)).collect();
    let new_edges: BTreeMap<_, _> = after.edges.iter().map(|row| (row.id, row)).collect();
    let node_ids: BTreeSet<_> = old_nodes.keys().chain(new_nodes.keys()).copied().collect();
    let edge_ids: BTreeSet<_> = old_edges.keys().chain(new_edges.keys()).copied().collect();
    let nodes = node_ids
        .into_iter()
        .rev()
        .filter_map(|id| {
            let old = old_nodes.get(&id).copied();
            let new = new_nodes.get(&id).copied();
            if old == new {
                return None;
            }
            let preserve = old.zip(new).is_some_and(|(a, b)| a.deadline == b.deadline);
            let mut row = new.cloned();
            if preserve {
                row.as_mut().unwrap().deadline = Some(123_456);
            }
            Some(Change {
                id,
                row,
                deadline: if preserve {
                    Deadline::Preserve
                } else {
                    Deadline::Set(new.and_then(|row| row.deadline))
                },
            })
        })
        .collect();
    let edges = edge_ids
        .into_iter()
        .rev()
        .filter_map(|id| {
            let old = old_edges.get(&id).copied();
            let new = new_edges.get(&id).copied();
            if old == new {
                return None;
            }
            let preserve = old.zip(new).is_some_and(|(a, b)| a.deadline == b.deadline);
            let mut row = new.cloned();
            if preserve {
                row.as_mut().unwrap().deadline = Some(123_456);
            }
            Some(Change {
                id,
                row,
                deadline: if preserve {
                    Deadline::Preserve
                } else {
                    Deadline::Set(new.and_then(|row| row.deadline))
                },
            })
        })
        .collect();
    Commit {
        sequence: after.sequence,
        nodes,
        edges,
        high_water: Some((after.next_node_id, after.next_edge_id)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::property_graph::engine::{Direction, EdgeId, NodeId, Properties};

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
                    name: "pinned_history".into(),
                },
                GraphEntry::PutNode {
                    node_id: NodeId(0),
                    key: "a".into(),
                    labels: BTreeSet::new(),
                    properties: Properties::new(),
                },
                GraphEntry::PutNode {
                    node_id: NodeId(1),
                    key: "b".into(),
                    labels: BTreeSet::new(),
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
                    labels: BTreeSet::new(),
                    properties: Properties::new(),
                },
                GraphEntry::PutEdge {
                    edge_id: EdgeId(1),
                    source: NodeId(1),
                    destination: NodeId(2),
                    relationship: "S".into(),
                    key: None,
                    properties: Properties::new(),
                },
            ],
        );
        states.push(state.clone());
        apply(
            &mut state,
            3,
            vec![GraphEntry::DeleteEdge { edge_id: EdgeId(0) }],
        );
        states.push(state);
        states
    }

    fn assert_matches(view: &CompactGraphView, state: &GraphState) {
        let graph = state.graph(GRAPH).unwrap();
        for node in graph.nodes() {
            assert_eq!(
                view.algorithm_node_by_key(&node.key).map(|found| found.id),
                Some(node.id)
            );
            for direction in [Direction::Out, Direction::In, Direction::Both] {
                assert_eq!(
                    view.algorithm_adjacency(node.id, direction, None)
                        .collect::<Vec<_>>(),
                    graph
                        .adjacency(node.id, direction, None, None)
                        .collect::<Vec<_>>()
                );
            }
        }
        assert!(view.algorithm_node_by_key("missing").is_none());
    }

    #[test]
    fn pinned_history_uses_the_codec_and_keeps_earlier_views_stable() {
        let states = history();
        let views = pinned_views(GRAPH, &states, 16).unwrap();
        assert_eq!(views.len(), states.len());
        for (view, state) in views.iter().zip(&states) {
            assert_matches(view, state);
        }
        assert_matches(&views[0], &states[0]);
        assert_eq!(
            views[0]
                .algorithm_adjacency(NodeId(0), Direction::Out, None)
                .collect::<Vec<_>>(),
            [("R", EdgeId(0))]
        );
        assert_eq!(
            views[2]
                .algorithm_adjacency(NodeId(0), Direction::Out, None)
                .count(),
            0
        );
    }
}
