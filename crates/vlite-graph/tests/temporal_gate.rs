//! Gap 4: until temporal reads exist (slice C), an ordinary build of the
//! standalone engine admits no temporal edge. Integration tests link the
//! library built without `cfg(test)`, like any dependent crate, so these
//! tests see what such a build does.

use std::collections::BTreeSet;

use vlite_graph::codec::TemporalEntry;
use vlite_graph::temporal::{Segment, reason};
use vlite_graph::{
    EdgeId, GraphBody, GraphEntry, GraphError, GraphId, GraphState, NodeId, Properties,
    TemporalMutation,
};

const G: GraphId = GraphId(0);

fn body(entries: Vec<GraphEntry>) -> GraphBody {
    GraphBody {
        graph_id: G,
        entries,
    }
}

fn node() -> GraphEntry {
    GraphEntry::PutNode {
        node_id: NodeId(0),
        key: "a".into(),
        labels: BTreeSet::new(),
        properties: Properties::new(),
    }
}

/// A temporal edge entry: `revision` zero in a log body, restated in a
/// snapshot.
fn temporal_edge(revision: u64) -> GraphEntry {
    GraphEntry::Temporal(TemporalEntry::PutEdge {
        edge_id: EdgeId(0),
        revision,
        source: NodeId(0),
        destination: NodeId(0),
        relationship: "R".into(),
        key: Some("e".into()),
        segments: vec![Segment {
            valid_from: None,
            valid_to: None,
            properties: Properties::new(),
        }],
    })
}

fn create() -> TemporalMutation {
    TemporalMutation::CreateEdge {
        key: Some("e".into()),
        source: "a".into(),
        relationship: "R".into(),
        destination: "a".into(),
        segments: Vec::new(),
    }
}

#[test]
fn a_snapshot_holding_a_temporal_edge_is_refused() {
    let snapshot = |edge: Option<GraphEntry>| {
        let mut entries = vec![GraphEntry::CreateGraph { name: "g".into() }, node()];
        entries.extend(edge);
        entries.push(GraphEntry::GraphHighWater {
            next_node_id: 1,
            next_edge_id: 1,
        });
        GraphState::from_snapshot(5, [body(entries)])
    };
    assert!(!snapshot(None).unwrap().is_temporal());
    assert_eq!(
        snapshot(Some(temporal_edge(1))),
        Err(GraphError::InvalidTemporal(reason::CAPABILITY))
    );
}

#[test]
fn no_state_plans_or_applies_temporal_entries() {
    let mut state = GraphState::new();
    state
        .apply(
            1,
            &body(vec![GraphEntry::CreateGraph { name: "g".into() }, node()]),
        )
        .unwrap();
    let refused = || GraphError::InvalidTemporal(reason::CAPABILITY);
    assert_eq!(state.set_temporal(true), Err(refused()));
    assert_eq!(state.set_temporal(false), Ok(()));
    assert!(!state.is_temporal());
    assert!(state.apply(2, &body(vec![temporal_edge(0)])).is_err());
    let mut planner = state.planner(2, G).unwrap();
    assert_eq!(planner.temporal(&create()), Err(refused()));
    assert!(planner.is_empty());
}
