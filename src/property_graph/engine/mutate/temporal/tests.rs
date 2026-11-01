//! Gap 4: planning temporal mutations.

use super::super::super::codec::{decode_body, encode_body};
use super::super::super::limits::MAX_BATCH_ENTRIES;
use super::*;

const G: GraphId = GraphId(0);

#[derive(Clone)]
enum Op {
    Graph(GraphMutation),
    Temporal(TemporalMutation),
}

fn seg(valid_from: Option<i64>, valid_to: Option<i64>, v: i64) -> Segment {
    Segment {
        valid_from,
        valid_to,
        properties: Properties::from([("v".into(), PropertyValue::Integer(v))]),
    }
}

fn node(key: &str) -> Op {
    Op::Graph(GraphMutation::CreateNode {
        key: key.into(),
        labels: BTreeSet::new(),
        properties: Properties::new(),
    })
}

fn create_to(key: Option<&str>, destination: &str, segments: Vec<Segment>) -> Op {
    Op::Temporal(TemporalMutation::CreateEdge {
        key: key.map(str::to_owned),
        source: "a".into(),
        relationship: "R".into(),
        destination: destination.into(),
        segments,
    })
}

fn create(key: &str, segments: Vec<Segment>) -> Op {
    create_to(Some(key), "b", segments)
}

fn selector(key: &str) -> EdgeSelector {
    EdgeSelector::Key(key.into())
}

fn append(edge: &str, segment: Segment, expected: Precondition) -> Op {
    Op::Temporal(TemporalMutation::AppendSegment {
        edge: selector(edge),
        segment,
        expected,
    })
}

fn close(edge: &str, bounds: (Option<i64>, Option<i64>), at: i64, expected: u64) -> Op {
    Op::Temporal(TemporalMutation::CloseSegment {
        edge: selector(edge),
        valid_from: bounds.0,
        valid_to: bounds.1,
        at,
        expected: Precondition::Revision(expected),
    })
}

fn legacy_edge(key: &str, properties: Properties) -> Op {
    Op::Graph(GraphMutation::CreateEdge {
        key: Some(key.into()),
        source: "a".into(),
        relationship: "R".into(),
        destination: "b".into(),
        properties,
    })
}

fn edge_outcome(id: u64, created: bool) -> MutationOutcome {
    MutationOutcome::Edge {
        id: EdgeId(id),
        created,
    }
}

/// Plans and commits batches with the temporal capability; committed
/// bodies are replayed from their bytes into `replica`, and a failed plan
/// must leave the state unchanged.
struct Db {
    state: GraphState,
    replica: GraphState,
    sequence: u64,
}

impl Db {
    fn new() -> Self {
        let mut db = Self {
            state: GraphState::new(),
            replica: GraphState::new(),
            sequence: 1,
        };
        let create = GraphBody {
            graph_id: G,
            entries: vec![GraphEntry::CreateGraph { name: "g".into() }],
        };
        for state in [&mut db.state, &mut db.replica] {
            state.set_temporal(true);
            state.apply(1, &create).unwrap();
        }
        db.ok(&[node("a"), node("b"), node("c")]);
        db
    }

    fn run_at(&mut self, now: Option<u64>, ops: &[Op]) -> Result<Vec<MutationOutcome>, GraphError> {
        self.sequence += 1;
        let before = self.state.clone();
        let sequence = self.sequence;
        let state = &mut self.state;
        let planned = (|| -> Result<_, GraphError> {
            let mut planner = state.planner(sequence, G)?;
            if let Some(now) = now {
                planner.reclaim_expired(now);
            }
            let outcomes = ops
                .iter()
                .map(|op| match op {
                    Op::Graph(mutation) => planner.mutate(mutation),
                    Op::Temporal(mutation) => planner.temporal(mutation),
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok((outcomes, planner.finish()))
        })();
        assert_eq!(self.state, before, "planning must never change state");
        let (outcomes, body) = planned?;
        if !body.entries.is_empty() {
            self.state.apply(self.sequence, &body).unwrap();
            let bytes = encode_body(&body).unwrap();
            let decoded = decode_body(&bytes, body.entries.len() as u32).unwrap();
            self.replica.apply(self.sequence, &decoded).unwrap();
            assert_eq!(self.state, self.replica, "replay must rebuild equal state");
        }
        self.state.check_invariants().unwrap();
        Ok(outcomes)
    }

    fn ok(&mut self, ops: &[Op]) -> Vec<MutationOutcome> {
        self.run_at(None, ops).unwrap()
    }

    fn err(&mut self, ops: &[Op]) -> GraphError {
        self.run_at(None, ops).unwrap_err()
    }

    fn graph(&self) -> &Graph {
        self.state.graph(G).unwrap()
    }

    fn series(&self, edge: &str) -> Vec<Segment> {
        let id = self.graph().edge_by_key(edge).unwrap().id;
        self.graph().segments(id).unwrap().to_vec()
    }

    fn revision(&self, edge: &str) -> u64 {
        self.graph().edge_by_key(edge).unwrap().revision
    }
}

#[test]
fn temporal_mutations_need_the_capability() {
    let mut state = GraphState::new();
    let create_graph = GraphBody {
        graph_id: G,
        entries: vec![GraphEntry::CreateGraph { name: "g".into() }],
    };
    state.apply(1, &create_graph).unwrap();
    let mut planner = state.planner(2, G).unwrap();
    let Op::Temporal(mutation) = create("e", vec![]) else {
        unreachable!()
    };
    assert_eq!(
        planner.temporal(&mutation),
        Err(GraphError::InvalidTemporal(reason::CAPABILITY))
    );
    assert!(planner.is_empty());
}

#[test]
fn create_is_idempotent_for_an_identical_keyed_series() {
    let mut db = Db::new();
    let series = vec![seg(Some(0), Some(10), 1), seg(Some(10), None, 2)];
    assert_eq!(
        db.ok(&[create("e", series.clone())]),
        [edge_outcome(0, true)]
    );
    let revision = db.revision("e");
    assert_eq!(
        db.ok(&[create("e", series.clone())]),
        [edge_outcome(0, false)]
    );
    assert_eq!(db.revision("e"), revision, "a retry plans nothing");
    let mut changed = series.clone();
    changed[1] = seg(Some(10), None, 3);
    for op in [
        create("e", changed),
        create("e", series[..1].to_vec()),
        create_to(Some("e"), "c", series.clone()),
        legacy_edge("e", Properties::new()),
    ] {
        assert_eq!(db.err(&[op]), GraphError::EdgeKeyConflict("e".into()));
    }
    db.ok(&[create("empty", vec![])]);
    assert_eq!(db.ok(&[create("empty", vec![])]), [edge_outcome(1, false)]);
    // A legacy create never matches a temporal series, nor the reverse.
    assert_eq!(
        db.err(&[legacy_edge("empty", Properties::new())]),
        GraphError::EdgeKeyConflict("empty".into())
    );
    db.ok(&[legacy_edge("legacy", Properties::new())]);
    assert_eq!(
        db.err(&[create("legacy", vec![])]),
        GraphError::EdgeKeyConflict("legacy".into())
    );
    // Invalid input fails as such, never as a conflict.
    assert_eq!(
        db.err(&[create("e", vec![seg(Some(5), Some(5), 1)])]),
        GraphError::InvalidTemporal(reason::BOUNDS)
    );
    assert_eq!(
        db.err(&[create_to(None, "zz", vec![])]),
        GraphError::UnknownNodeKey("zz".into())
    );
}

#[test]
fn series_changes_require_the_exact_series_revision() {
    let mut db = Db::new();
    db.ok(&[create("e", vec![seg(Some(0), None, 1)])]);
    let revision = db.revision("e");
    for expected in [Precondition::Any, Precondition::Absent] {
        assert_eq!(
            db.err(&[append("e", seg(None, Some(0), 0), expected)]),
            GraphError::InvalidTemporal(reason::EXPECTED)
        );
    }
    assert_eq!(
        db.err(&[append(
            "e",
            seg(None, Some(0), 0),
            Precondition::Revision(revision + 1)
        )]),
        GraphError::PreconditionFailed {
            current: Some(revision)
        }
    );
    assert_eq!(
        db.err(&[append(
            "missing",
            seg(None, Some(0), 0),
            Precondition::Revision(1)
        )]),
        GraphError::PreconditionFailed { current: None }
    );
    // Supersede in one batch: close at 50 and append the successor, both
    // checked against the revision the caller read.
    db.ok(&[
        close("e", (Some(0), None), 50, revision),
        append(
            "e",
            seg(Some(50), None, 2),
            Precondition::Revision(revision),
        ),
    ]);
    assert_eq!(
        db.series("e"),
        [seg(Some(0), Some(50), 1), seg(Some(50), None, 2)]
    );
    let corrected = db.revision("e");
    assert!(corrected > revision);
    // A stale retry of the same supersession fails.
    assert_eq!(
        db.err(&[close("e", (Some(50), None), 60, revision)]),
        GraphError::PreconditionFailed {
            current: Some(corrected)
        }
    );
    // Closing at the existing end is an exact no-op after its checks.
    assert_eq!(
        db.ok(&[close("e", (Some(0), Some(50)), 50, corrected)]),
        [edge_outcome(0, false)]
    );
    assert_eq!(db.revision("e"), corrected);
    assert_eq!(
        db.err(&[close("e", (Some(1), Some(50)), 50, corrected)]),
        GraphError::InvalidTemporal(reason::NO_SEGMENT)
    );
    assert_eq!(
        db.err(&[close("e", (Some(0), Some(50)), 50, revision)]),
        GraphError::PreconditionFailed {
            current: Some(corrected)
        }
    );
    // Replace and remove address exact bounds.
    db.ok(&[Op::Temporal(TemporalMutation::ReplaceSegment {
        edge: selector("e"),
        valid_from: Some(0),
        valid_to: Some(50),
        segment: seg(Some(-10), Some(50), 9),
        expected: Precondition::Revision(corrected),
    })]);
    let replaced = db.revision("e");
    db.ok(&[Op::Temporal(TemporalMutation::RemoveSegment {
        edge: EdgeSelector::Id(EdgeId(0)),
        valid_from: Some(-10),
        valid_to: Some(50),
        expected: Precondition::Revision(replaced),
    })]);
    assert_eq!(db.series("e"), [seg(Some(50), None, 2)]);
    // Ordinary patches of a temporal edge fail.
    let patch = Op::Graph(GraphMutation::PatchEdge {
        edge: selector("e"),
        set: Properties::new(),
        unset: BTreeSet::new(),
        expected: Precondition::Any,
    });
    assert_eq!(
        db.err(&[patch]),
        GraphError::InvalidTemporal(reason::TEMPORAL_EDGE)
    );
}

#[test]
fn promotion_keeps_the_edge_and_moves_its_properties() {
    let mut db = Db::new();
    let properties = Properties::from([("w".into(), PropertyValue::Float(-0.0))]);
    db.ok(&[legacy_edge("e", properties.clone())]);
    let revision = db.revision("e");
    let promote = |expected| {
        Op::Temporal(TemporalMutation::Promote {
            edge: selector("e"),
            expected,
        })
    };
    assert_eq!(
        db.err(&[promote(Precondition::Any)]),
        GraphError::InvalidTemporal(reason::EXPECTED)
    );
    assert_eq!(
        db.err(&[append(
            "e",
            seg(None, None, 0),
            Precondition::Revision(revision)
        )]),
        GraphError::InvalidTemporal(reason::NOT_TEMPORAL)
    );
    // Promote and close the all-time segment in one batch.
    assert_eq!(
        db.ok(&[
            promote(Precondition::Revision(revision)),
            close("e", (None, None), 100, revision),
        ]),
        [edge_outcome(0, false), edge_outcome(0, false)]
    );
    let edge = db.graph().edge_by_key("e").unwrap();
    assert_eq!((edge.id, edge.properties.len()), (EdgeId(0), 0));
    let series = db.series("e");
    assert_eq!(series.len(), 1);
    assert!(series[0].has_bounds(None, Some(100)));
    assert!(same_properties(&series[0].properties, &properties));
    assert_eq!(
        db.err(&[promote(Precondition::Revision(db.revision("e")))]),
        GraphError::InvalidTemporal(reason::ALREADY_TEMPORAL)
    );
}

#[test]
fn temporal_mutations_count_toward_the_batch_limit_and_reclaim_first() {
    let mut db = Db::new();
    db.ok(&[create("e", vec![])]);
    let revision = db.revision("e");
    // 256 appends in one batch fill the series; one more is the cap.
    let ops: Vec<Op> = (0..256)
        .map(|i| {
            append(
                "e",
                seg(Some(i), Some(i + 1), i),
                Precondition::Revision(revision),
            )
        })
        .collect();
    db.ok(&ops);
    assert_eq!(db.series("e").len(), 256);
    let revision = db.revision("e");
    let one_more = append(
        "e",
        seg(Some(999), None, 0),
        Precondition::Revision(revision),
    );
    assert_eq!(
        db.err(std::slice::from_ref(&one_more)),
        GraphError::InvalidTemporal(reason::SEGMENT_LIMIT)
    );
    // 4,096 counted operations, temporal ones included, fit; 4,097 do not.
    let mut ops: Vec<Op> = (0..MAX_BATCH_ENTRIES - 1)
        .map(|i| node(&format!("n{i}")))
        .collect();
    ops.push(create("f", vec![seg(None, None, 1)]));
    let mut over = ops.clone();
    over.push(one_more);
    assert_eq!(
        db.err(&over),
        GraphError::BatchSize("more than 4096 entries")
    );
    db.ok(&ops);

    // Due entities are reclaimed first and do not count; a failure rolls
    // the reclaim back with the rest of the plan.
    db.ok(&[Op::Graph(GraphMutation::SetEdgeExpiry {
        edge: selector("f"),
        expires_at_ms: Some(100),
        expected: Precondition::Any,
    })]);
    let f = db.revision("f");
    let before = db.state.clone();
    assert_eq!(
        db.run_at(
            Some(100),
            &[append(
                "f",
                seg(Some(0), None, 1),
                Precondition::Revision(f)
            )]
        ),
        Err(GraphError::PreconditionFailed { current: None })
    );
    assert_eq!(db.state, before);
    db.run_at(Some(100), &[create("f", vec![seg(None, None, 2)])])
        .unwrap();
    assert_eq!(db.series("f"), [seg(None, None, 2)]);
    assert_eq!(db.graph().edge_by_key("f").unwrap().id, EdgeId(2));
}
