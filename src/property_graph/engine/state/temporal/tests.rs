//! Gap 4: temporal entries applied to the graph state. Every commit goes
//! through a harness that checks validate/apply agreement, replay of the
//! encoded body into a replica, unchanged state after a failure and the
//! state invariants.

use super::super::super::codec::{decode_body, encode_body};
use super::super::super::limits::MAX_PROPERTY_BYTES;
use super::super::super::model::PropertyValue;
use super::super::super::schema::{
    Multiplicity, PropertyDefinition, PropertyType, RelationshipDefinition,
};
use super::super::super::temporal::MAX_SEGMENTS;
use super::*;

const G: GraphId = GraphId(0);

fn seg(valid_from: Option<i64>, valid_to: Option<i64>, v: i64) -> Segment {
    Segment {
        valid_from,
        valid_to,
        properties: Properties::from([("v".into(), PropertyValue::Integer(v))]),
    }
}

fn put_node(id: u64, key: &str) -> GraphEntry {
    GraphEntry::PutNode {
        node_id: NodeId(id),
        key: key.into(),
        labels: BTreeSet::new(),
        properties: Properties::new(),
    }
}

fn legacy_edge(id: u64, key: &str, properties: Properties) -> GraphEntry {
    GraphEntry::PutEdge {
        edge_id: EdgeId(id),
        source: NodeId(0),
        destination: NodeId(1),
        relationship: "R".into(),
        key: Some(key.into()),
        properties,
    }
}

fn temporal_edge(id: u64, key: &str, segments: Vec<Segment>) -> GraphEntry {
    GraphEntry::Temporal(TemporalEntry::PutEdge {
        edge_id: EdgeId(id),
        source: NodeId(0),
        destination: NodeId(1),
        relationship: "R".into(),
        key: Some(key.into()),
        segments,
    })
}

fn append(id: u64, segment: Segment) -> GraphEntry {
    GraphEntry::Temporal(TemporalEntry::AppendSegment {
        edge_id: EdgeId(id),
        segment,
    })
}

fn replace(id: u64, from: Option<i64>, to: Option<i64>, segment: Segment) -> GraphEntry {
    GraphEntry::Temporal(TemporalEntry::ReplaceSegment {
        edge_id: EdgeId(id),
        valid_from: from,
        valid_to: to,
        segment,
    })
}

fn close(id: u64, from: Option<i64>, to: Option<i64>, at: i64) -> GraphEntry {
    GraphEntry::Temporal(TemporalEntry::CloseSegment {
        edge_id: EdgeId(id),
        valid_from: from,
        valid_to: to,
        at,
    })
}

fn remove(id: u64, from: Option<i64>, to: Option<i64>) -> GraphEntry {
    GraphEntry::Temporal(TemporalEntry::RemoveSegment {
        edge_id: EdgeId(id),
        valid_from: from,
        valid_to: to,
    })
}

fn invalid(reason: &'static str) -> GraphError {
    GraphError::InvalidTemporal(reason)
}

/// A state with graph `G` and nodes 0..3, plus a replica replaying each
/// committed body from its encoded bytes.
struct Harness {
    state: GraphState,
    replica: GraphState,
    sequence: u64,
}

impl Harness {
    fn new() -> Self {
        Self::with_capability(true)
    }

    fn with_capability(temporal: bool) -> Self {
        let mut harness = Self {
            state: GraphState::new(),
            replica: GraphState::new(),
            sequence: 0,
        };
        harness.state.set_temporal(temporal);
        harness.replica.set_temporal(temporal);
        harness.ok(vec![
            GraphEntry::CreateGraph { name: "g".into() },
            put_node(0, "a"),
            put_node(1, "b"),
            put_node(2, "c"),
        ]);
        harness
    }

    fn commit(&mut self, entries: Vec<GraphEntry>) -> Result<(), GraphError> {
        self.sequence += 1;
        let body = GraphBody {
            graph_id: G,
            entries,
        };
        let before = self.state.clone();
        let validated = self.state.validate(self.sequence, &body);
        assert_eq!(self.state, before, "validate must not change state");
        let applied = self.state.apply(self.sequence, &body);
        assert_eq!(validated, applied, "validate and apply must agree");
        match &applied {
            Ok(()) => {
                let bytes = encode_body(&body).unwrap();
                let decoded = decode_body(&bytes, body.entries.len() as u32).unwrap();
                assert_eq!(decoded, body);
                self.replica.apply(self.sequence, &decoded).unwrap();
                assert_eq!(self.state, self.replica, "replay must rebuild equal state");
            }
            Err(_) => assert_eq!(self.state, before, "a failed batch must change nothing"),
        }
        self.state.check_invariants().unwrap();
        applied
    }

    fn ok(&mut self, entries: Vec<GraphEntry>) {
        self.commit(entries).unwrap();
    }

    fn err(&mut self, entries: Vec<GraphEntry>) -> GraphError {
        self.commit(entries).unwrap_err()
    }

    fn graph(&self) -> &Graph {
        self.state.graph(G).unwrap()
    }

    fn series(&self, id: u64) -> Vec<Segment> {
        self.graph().segments(EdgeId(id)).unwrap().to_vec()
    }

    fn revision(&self, id: u64) -> u64 {
        self.graph().edge(EdgeId(id)).unwrap().revision
    }
}

#[test]
fn temporal_entries_need_the_capability() {
    let mut h = Harness::with_capability(false);
    assert!(!h.state.is_temporal());
    for entry in [
        temporal_edge(0, "e", vec![seg(None, None, 1)]),
        append(0, seg(None, None, 1)),
        remove(0, None, None),
    ] {
        assert_eq!(
            h.err(vec![entry]),
            GraphError::Corrupt("temporal entry without the temporal capability")
        );
    }
    // The capability alone changes no graph.
    let mut legacy = Harness::with_capability(false);
    let mut capable = Harness::new();
    for h in [&mut legacy, &mut capable] {
        h.ok(vec![legacy_edge(0, "e", Properties::new())]);
    }
    assert_eq!(legacy.graph(), capable.graph());
}

#[test]
fn series_mutations_apply_in_order_and_advance_the_revision() {
    let mut h = Harness::new();
    h.ok(vec![temporal_edge(
        0,
        "e",
        vec![seg(Some(100), Some(200), 1)],
    )]);
    let graph = h.graph();
    let edge = graph.edge(EdgeId(0)).unwrap();
    assert!(edge.properties.is_empty(), "properties live in segments");
    assert_eq!(edge.revision, h.sequence);
    assert_eq!(graph.edge_by_key("e").map(|e| e.id), Some(EdgeId(0)));
    assert_eq!(graph.edge_count(), 1);
    assert_eq!(graph.edges_of(NodeId(0), Direction::Out), vec![EdgeId(0)]);
    assert_eq!(graph.next_edge_id(), 1);

    // Backfill, touching and gaps are allowed; order follows valid_from.
    h.ok(vec![
        append(0, seg(Some(300), None, 3)),
        append(0, seg(None, Some(100), 0)),
    ]);
    assert_eq!(h.revision(0), h.sequence);
    assert_eq!(
        h.series(0),
        vec![
            seg(None, Some(100), 0),
            seg(Some(100), Some(200), 1),
            seg(Some(300), None, 3)
        ]
    );
    // A correction may change bounds and properties.
    h.ok(vec![replace(
        0,
        Some(100),
        Some(200),
        seg(Some(100), Some(250), 2),
    )]);
    assert_eq!(h.series(0)[1], seg(Some(100), Some(250), 2));
    // Close the open-ended segment and supersede it in the same batch.
    h.ok(vec![
        close(0, Some(300), None, 400),
        append(0, seg(Some(400), None, 4)),
    ]);
    assert_eq!(
        h.series(0)[2..],
        [seg(Some(300), Some(400), 3), seg(Some(400), None, 4)]
    );
    // Closing at the existing end changes no segment but still advances
    // the revision (planners never emit it).
    let series = h.series(0);
    h.ok(vec![close(0, Some(300), Some(400), 400)]);
    assert_eq!((h.series(0), h.revision(0)), (series, h.sequence));
    h.ok(vec![remove(0, None, Some(100))]);
    assert_eq!(h.series(0).len(), 3);
    // Zero remaining segments keep the retained edge, its key and revision.
    h.ok(vec![
        remove(0, Some(100), Some(250)),
        remove(0, Some(300), Some(400)),
        remove(0, Some(400), None),
    ]);
    assert_eq!(h.series(0), Vec::<Segment>::new());
    assert_eq!(h.revision(0), h.sequence);
    assert_eq!(h.graph().edge_by_key("e").map(|e| e.id), Some(EdgeId(0)));
}

#[test]
fn invalid_series_changes_are_rejected_without_mutation() {
    let mut h = Harness::new();
    h.ok(vec![
        temporal_edge(
            0,
            "e",
            vec![seg(Some(0), Some(10), 1), seg(Some(20), None, 2)],
        ),
        legacy_edge(1, "legacy", Properties::new()),
    ]);
    let cases = [
        (
            append(0, seg(Some(5), Some(15), 9)),
            invalid(reason::OVERLAP),
        ),
        (append(0, seg(Some(9), Some(9), 9)), invalid(reason::BOUNDS)),
        (
            append(9, seg(None, None, 9)),
            GraphError::EdgeNotFound(EdgeId(9)),
        ),
        (append(1, seg(None, None, 9)), invalid(reason::NOT_TEMPORAL)),
        (
            replace(0, Some(0), Some(11), seg(Some(0), Some(10), 9)),
            invalid(reason::NO_SEGMENT),
        ),
        (
            replace(0, Some(0), Some(10), seg(Some(0), Some(25), 9)),
            invalid(reason::OVERLAP),
        ),
        (close(0, Some(0), Some(10), 0), invalid(reason::CLOSE)),
        (close(0, Some(0), Some(10), 11), invalid(reason::CLOSE)),
        (close(0, Some(1), Some(10), 5), invalid(reason::NO_SEGMENT)),
        (remove(0, Some(20), Some(30)), invalid(reason::NO_SEGMENT)),
        (remove(1, None, None), invalid(reason::NOT_TEMPORAL)),
        (
            GraphEntry::PatchEdge {
                edge_id: EdgeId(0),
                set: Properties::from([("v".into(), PropertyValue::Integer(9))]),
                unset: BTreeSet::new(),
            },
            invalid(reason::TEMPORAL_EDGE),
        ),
    ];
    for (entry, error) in cases {
        assert_eq!(h.err(vec![entry]), error);
    }
    // A failure in the final entry undoes every earlier change, in order.
    let revision = h.revision(0);
    assert_eq!(
        h.err(vec![
            append(0, seg(Some(10), Some(20), 3)),
            replace(0, Some(0), Some(10), seg(Some(-5), Some(10), 4)),
            close(0, Some(20), None, 30),
            remove(0, Some(10), Some(20)),
            temporal_edge(2, "f", vec![seg(None, None, 5)]),
            legacy_edge(3, "promote", Properties::new()),
            temporal_edge(3, "promote", vec![seg(None, None, 6)]),
            GraphEntry::DeleteEdge { edge_id: EdgeId(0) },
            append(9, seg(None, None, 9)),
        ]),
        GraphError::EdgeNotFound(EdgeId(9))
    );
    assert_eq!(h.revision(0), revision);
    assert_eq!(h.graph().next_edge_id(), 2);
}

#[test]
fn put_edge_creates_or_promotes_with_a_stable_identity() {
    let mut h = Harness::new();
    let properties = Properties::from([("v".into(), PropertyValue::Integer(7))]);
    h.ok(vec![legacy_edge(0, "e", properties.clone())]);
    let promoted = |segments| temporal_edge(0, "e", segments);
    let changed = GraphEntry::Temporal(TemporalEntry::PutEdge {
        edge_id: EdgeId(0),
        source: NodeId(0),
        destination: NodeId(2),
        relationship: "R".into(),
        key: Some("e".into()),
        segments: vec![seg(None, None, 7)],
    });
    assert_eq!(
        h.err(vec![changed]),
        GraphError::EdgeIdUnavailable(EdgeId(0))
    );
    // A promotion that fails later in its batch gives the properties back.
    assert_eq!(
        h.err(vec![
            promoted(vec![seg(None, None, 7)]),
            append(9, seg(None, None, 0))
        ]),
        GraphError::EdgeNotFound(EdgeId(9))
    );
    assert_eq!(h.graph().edge(EdgeId(0)).unwrap().properties, properties);
    h.ok(vec![promoted(vec![Segment {
        valid_from: None,
        valid_to: None,
        properties: properties.clone(),
    }])]);
    let edge = h.graph().edge(EdgeId(0)).unwrap();
    assert!(edge.properties.is_empty());
    assert_eq!(edge.revision, h.sequence);
    assert_eq!(h.series(0)[0].properties, properties);
    assert_eq!(
        h.err(vec![promoted(vec![])]),
        invalid(reason::ALREADY_TEMPORAL)
    );
    // New series: fresh IDs, live endpoints and unique keys, like legacy.
    let new = |id, key: &str, source| {
        GraphEntry::Temporal(TemporalEntry::PutEdge {
            edge_id: EdgeId(id),
            source: NodeId(source),
            destination: NodeId(1),
            relationship: "R".into(),
            key: Some(key.into()),
            segments: vec![],
        })
    };
    assert_eq!(
        h.err(vec![new(1, "e", 0)]),
        GraphError::EdgeKeyExists("e".into())
    );
    assert_eq!(
        h.err(vec![new(1, "f", 9)]),
        GraphError::NodeNotFound(NodeId(9))
    );
    h.ok(vec![new(1, "f", 0)]);
    assert_eq!(h.series(1), Vec::<Segment>::new());
    h.ok(vec![GraphEntry::DeleteEdge { edge_id: EdgeId(1) }]);
    assert_eq!(
        h.err(vec![new(1, "g", 0)]),
        GraphError::EdgeIdUnavailable(EdgeId(1))
    );
    // A deleted series is gone; its key starts a new edge and series.
    h.ok(vec![new(2, "f", 0)]);
    assert_eq!(h.series(2), Vec::<Segment>::new());
    assert!(h.graph().segments(EdgeId(1)).is_none());
}

#[test]
fn ttl_deletes_whole_series_and_rollback_restores_them() {
    let mut h = Harness::new();
    h.ok(vec![
        temporal_edge(
            0,
            "own",
            vec![seg(Some(100), Some(200), 1), seg(Some(200), None, 2)],
        ),
        GraphEntry::Temporal(TemporalEntry::PutEdge {
            edge_id: EdgeId(1),
            source: NodeId(2),
            destination: NodeId(1),
            relationship: "R".into(),
            key: Some("cascade".into()),
            segments: vec![seg(Some(100), Some(200), 3)],
        }),
        temporal_edge(2, "kept", vec![seg(None, Some(100), 4)]),
        GraphEntry::EdgeExpiry {
            edge_id: EdgeId(0),
            expires_at_ms: Some(500),
        },
        GraphEntry::NodeExpiry {
            node_id: NodeId(2),
            expires_at_ms: Some(500),
        },
    ]);
    // Closed history is retained until TTL removes it.
    assert_eq!(h.graph().live_counts(499), (3, 3));
    assert_eq!(h.graph().live_counts(500), (2, 1));
    // A failing batch rolls its reclaim back with the series.
    let before = h.state.clone();
    assert_eq!(
        h.err(vec![
            GraphEntry::Reclaim { at_ms: 500 },
            append(9, seg(None, None, 0))
        ]),
        GraphError::EdgeNotFound(EdgeId(9))
    );
    assert_eq!(h.state, before);
    // Edge TTL deletes its whole series; node TTL cascades to its edges.
    h.ok(vec![GraphEntry::Reclaim { at_ms: 500 }]);
    let graph = h.graph();
    assert_eq!((graph.node_count(), graph.edge_count()), (2, 1));
    assert!(graph.segments(EdgeId(0)).is_none() && graph.segments(EdgeId(1)).is_none());
    assert_eq!(h.series(2), vec![seg(None, Some(100), 4)]);
    // Nothing brings reclaimed history back: the ID stays used and segment
    // operations on it fail.
    assert_eq!(
        h.err(vec![append(0, seg(Some(100), Some(200), 1))]),
        GraphError::EdgeNotFound(EdgeId(0))
    );
    assert_eq!(
        h.err(vec![temporal_edge(0, "own", vec![])]),
        GraphError::EdgeIdUnavailable(EdgeId(0))
    );
    // Explicit deletion and its rollback.
    assert_eq!(
        h.err(vec![
            GraphEntry::DeleteEdge { edge_id: EdgeId(2) },
            append(9, seg(None, None, 0))
        ]),
        GraphError::EdgeNotFound(EdgeId(9))
    );
    assert_eq!(h.series(2), vec![seg(None, Some(100), 4)]);
    h.ok(vec![GraphEntry::DeleteEdge { edge_id: EdgeId(2) }]);
    assert!(h.graph().segments(EdgeId(2)).is_none());
}

/// Properties whose encoding is exactly `MAX_PROPERTY_BYTES`.
fn max_properties() -> Properties {
    let mut properties: Properties = (0..15)
        .map(|f| (format!("f{f:02}"), PropertyValue::String("x".repeat(4096))))
        .collect();
    properties.insert("f15".into(), PropertyValue::String("x".repeat(3932)));
    assert_eq!(properties::validate(&properties), Ok(MAX_PROPERTY_BYTES));
    properties
}

#[test]
fn segment_cap_and_series_bytes_are_enforced() {
    let mut h = Harness::new();
    let full: Vec<Segment> = (0..MAX_SEGMENTS as i64)
        .map(|i| seg(Some(i), Some(i + 1), i))
        .collect();
    let mut over = full.clone();
    over.push(seg(Some(1000), None, 0));
    assert_eq!(
        h.err(vec![temporal_edge(0, "e", over)]),
        invalid(reason::SEGMENT_LIMIT)
    );
    h.ok(vec![temporal_edge(0, "e", full)]);
    assert_eq!(
        h.err(vec![append(0, seg(Some(1000), None, 0))]),
        invalid(reason::SEGMENT_LIMIT)
    );
    // Replacing within a full series is allowed.
    h.ok(vec![replace(
        0,
        Some(255),
        Some(256),
        seg(Some(255), None, 1),
    )]);

    // With maximum-size properties the whole series must still fit one
    // graph body, so the 256th such segment is rejected by its bytes.
    let big = |i: i64| Segment {
        valid_from: Some(i),
        valid_to: Some(i + 1),
        properties: max_properties(),
    };
    let series: Vec<Segment> = (0..255).map(big).collect();
    check_series_bytes("R", Some("big"), &series).unwrap();
    let mut too_big = series.clone();
    too_big.push(big(255));
    assert_eq!(
        check_series_bytes("R", Some("big"), &too_big),
        Err(invalid(reason::SERIES_BYTES))
    );
    assert_eq!(
        h.err(vec![temporal_edge(1, "big", too_big)]),
        invalid(reason::SERIES_BYTES)
    );
    h.ok(vec![temporal_edge(1, "big", series)]);
    assert_eq!(
        h.err(vec![append(1, big(255))]),
        invalid(reason::SERIES_BYTES)
    );
    assert_eq!(h.series(1).len(), 255);
}

#[test]
fn schema_rules_apply_to_every_segment() {
    let mut h = Harness::new();
    let mut relationship = RelationshipDefinition::new("");
    relationship
        .properties
        .insert("v".into(), PropertyDefinition::new(PropertyType::Integer));
    relationship.multiplicity = Multiplicity::UniqueTriple;
    let mut schema = GraphSchema::default();
    schema.relationships.insert("R".into(), relationship);
    h.ok(vec![GraphEntry::PutSchema { schema }]);
    let text = |from, to| Segment {
        valid_from: from,
        valid_to: to,
        properties: Properties::from([("v".into(), PropertyValue::String("x".into()))]),
    };
    let violation = |error| matches!(error, GraphError::SchemaViolation { .. });
    assert!(violation(h.err(vec![temporal_edge(
        0,
        "e",
        vec![seg(None, Some(0), 1), text(Some(0), None)]
    )])));
    h.ok(vec![temporal_edge(0, "e", vec![seg(None, Some(0), 1)])]);
    assert!(violation(h.err(vec![append(0, text(Some(0), None))])));
    assert!(violation(h.err(vec![replace(
        0,
        None,
        Some(0),
        text(None, Some(0))
    )])));
    // UniqueTriple counts one logical series, however many segments.
    h.ok(vec![append(0, seg(Some(0), None, 2))]);
    assert!(violation(h.err(vec![temporal_edge(
        1,
        "f",
        vec![seg(None, None, 3)]
    )])));
    assert!(violation(h.err(vec![legacy_edge(
        1,
        "f",
        Properties::new()
    )])));
}

#[test]
fn invariants_cover_the_series() {
    let mut h = Harness::new();
    h.ok(vec![temporal_edge(0, "e", vec![seg(None, None, 1)])]);
    let broken = |change: &dyn Fn(&mut Graph)| {
        let mut state = h.state.clone();
        change(state.graphs.get_mut(&G).unwrap());
        state.check_invariants().unwrap_err()
    };
    let mut legacy = h.state.clone();
    legacy.set_temporal(false);
    assert_eq!(
        legacy.check_invariants(),
        Err(GraphError::InvariantViolation(
            "temporal edge without the temporal capability"
        ))
    );
    assert_eq!(
        broken(&|graph| {
            graph.segments.insert(EdgeId(9), vec![]);
        }),
        GraphError::InvariantViolation("dangling series")
    );
    assert_eq!(
        broken(&|graph| {
            graph.edges.get_mut(&EdgeId(0)).unwrap().properties =
                Properties::from([("v".into(), PropertyValue::Integer(1))]);
        }),
        GraphError::InvariantViolation("temporal edge with edge properties")
    );
    assert_eq!(
        broken(&|graph| {
            graph
                .segments
                .get_mut(&EdgeId(0))
                .unwrap()
                .push(seg(Some(0), None, 2));
        }),
        GraphError::InvariantViolation("invalid series")
    );
}

/// A snapshot restates each series in one temporal `PutEdge`, followed by
/// its deadline; loading it equals replaying it and implies the capability.
#[test]
fn snapshot_restates_series_and_equals_replay() {
    let mut h = Harness::new();
    h.ok(vec![
        legacy_edge(0, "legacy", Properties::new()),
        temporal_edge(1, "t", vec![seg(None, Some(0), 1), seg(Some(5), None, 2)]),
        temporal_edge(2, "empty", vec![]),
        GraphEntry::EdgeExpiry {
            edge_id: EdgeId(1),
            expires_at_ms: Some(900),
        },
    ]);
    let graph = h.graph();
    let mut entries = vec![GraphEntry::CreateGraph { name: "g".into() }];
    entries.extend([put_node(0, "a"), put_node(1, "b"), put_node(2, "c")]);
    for edge in graph.edges() {
        entries.push(graph.edge_snapshot_entry(edge));
        if let Some(deadline) = graph.edge_expiry(edge.id) {
            entries.push(GraphEntry::EdgeExpiry {
                edge_id: edge.id,
                expires_at_ms: Some(deadline),
            });
        }
    }
    entries.push(GraphEntry::GraphHighWater {
        next_node_id: 3,
        next_edge_id: 3,
    });
    assert!(matches!(entries[4], GraphEntry::PutEdge { .. }));
    assert!(matches!(
        entries[5],
        GraphEntry::Temporal(TemporalEntry::PutEdge { .. })
    ));
    let bodies: Vec<GraphBody> = entries
        .chunks(3)
        .map(|entries| GraphBody {
            graph_id: G,
            entries: entries.to_vec(),
        })
        .collect();
    let loaded = GraphState::from_snapshot(50, bodies.clone()).unwrap();
    assert!(loaded.is_temporal());
    loaded.check_invariants().unwrap();
    let mut replayed = GraphState::new();
    replayed.set_temporal(true);
    for body in &bodies {
        replayed.apply_at(50, body, true).unwrap();
    }
    assert_eq!(loaded, replayed);
    let graph = loaded.graph(G).unwrap();
    assert_eq!(graph.segments(EdgeId(1)), h.graph().segments(EdgeId(1)));
    assert_eq!(graph.segments(EdgeId(2)), Some(&[][..]));
    assert_eq!(graph.edge_expiry(EdgeId(1)), Some(900));

    // Invalid or non-snapshot temporal entries are refused.
    let refused = |edge: GraphEntry| {
        let mut entries = entries.clone();
        entries[5] = edge;
        GraphState::from_snapshot(
            50,
            [GraphBody {
                graph_id: G,
                entries,
            }],
        )
        .unwrap_err()
    };
    assert_eq!(
        refused(append(1, seg(Some(9), None, 0))),
        GraphError::Corrupt("non-canonical graph snapshot")
    );
    assert!(matches!(
        refused(temporal_edge(
            1,
            "t",
            vec![seg(None, Some(5), 1), seg(Some(0), None, 2)]
        )),
        GraphError::InvalidTemporal(_)
    ));
    assert_eq!(
        refused(temporal_edge(0, "t", vec![])),
        GraphError::EdgeIdUnavailable(EdgeId(0))
    );
}
