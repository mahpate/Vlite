//! Adapter tests for EPIC-004 graph batches (ITEM-015/016/017): orchestration
//! against a real file, atomicity of failed batches, replay equivalence of
//! the live state including `graph_sequence`, and the H-4 hooks of ordinary
//! vector writes.

use std::fs;
use std::path::Path;

use super::*;
use crate::{InvalidRowPolicy, MetadataValue};

const FAR_FUTURE: u64 = 4_102_444_800_000;

fn graph_db(path: &Path) -> Database {
    let mut db = Database::create(path, 2).unwrap();
    db.create_graph("g").unwrap();
    db
}

fn state(db: &Database) -> &GraphState {
    db.graph.as_ref().unwrap()
}

fn g(db: &Database) -> &Graph {
    state(db).graph_by_name("g").unwrap()
}

fn node(key: &str) -> GraphOp {
    GraphOp::CreateNode {
        key: key.into(),
        labels: vec!["Doc".into()],
        properties: Metadata::new(),
    }
}

fn edge(key: Option<&str>, source: &str, destination: &str) -> GraphOp {
    GraphOp::CreateEdge {
        key: key.map(str::to_owned),
        source: source.into(),
        relationship: "LINKS".into(),
        destination: destination.into(),
        properties: Metadata::new(),
    }
}

fn attach(node: &str, record: u64) -> GraphOp {
    GraphOp::Attach {
        node: node.into(),
        collection: "default".into(),
        record,
    }
}

fn put(id: u64) -> GraphOp {
    GraphOp::Vector {
        collection: "default".into(),
        mutation: Mutation::Put {
            id,
            vector: vec![id as f32, 1.0],
            payload: Vec::new(),
        },
    }
}

fn delete(id: u64) -> GraphOp {
    GraphOp::Vector {
        collection: "default".into(),
        mutation: Mutation::Delete { id },
    }
}

fn len(path: &Path) -> u64 {
    fs::metadata(path).unwrap().len()
}

fn reopened(path: &Path) -> Database {
    Database::open(path).unwrap()
}

#[test]
fn mixed_batches_commit_in_one_frame_and_replay_identically() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mixed.vlite");
    let mut db = graph_db(&path);
    let result = db
        .graph_mut("g")
        .unwrap()
        .write_batch(&[
            put(7),
            node("a"),
            node("b"),
            edge(Some("ab"), "a", "b"),
            attach("a", 7),
        ])
        .unwrap();
    let revision = result.revision.unwrap();
    assert_eq!(
        result.outcomes,
        [
            GraphOpOutcome::Vector,
            GraphOpOutcome::Node(GraphWrite {
                id: 0,
                created: true,
                revision
            }),
            GraphOpOutcome::Node(GraphWrite {
                id: 1,
                created: true,
                revision
            }),
            GraphOpOutcome::Edge(GraphWrite {
                id: 0,
                created: true,
                revision
            }),
            GraphOpOutcome::Attached { node: 0 },
        ]
    );
    // Every write of the batch shares one commit sequence.
    assert_eq!(db.revision(7).unwrap(), Some(revision));
    assert_eq!(g(&db).graph_sequence(), revision);
    assert_eq!(db.graph("g").unwrap().info().edge_count, 1);
    let live = state(&db).clone();
    db.close().unwrap();
    let db = reopened(&path);
    assert_eq!(state(&db), &live);
    assert_eq!(db.get(7).unwrap().unwrap().vector, [7.0, 1.0]);
    state(&db).check_invariants().unwrap();
}

#[test]
fn failed_batches_write_and_change_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("atomic.vlite");
    let mut db = graph_db(&path);
    db.graph_mut("g")
        .unwrap()
        .write_batch(&[put(1), node("a")])
        .unwrap();
    let before = (len(&path), state(&db).clone(), db.revision(1).unwrap());
    type Failure<'a> = (&'a [GraphOp], fn(&Error) -> bool);
    let failures: [Failure<'_>; 8] = [
        (&[node("b"), node("a")], |e| {
            matches!(e, Error::GraphKeyExists)
        }),
        (&[put(2), edge(None, "a", "zz")], |e| {
            matches!(e, Error::GraphNodeNotFound)
        }),
        (
            &[GraphOp::PatchNode {
                key: "a".into(),
                patch: NodePatch {
                    add_labels: vec!["X".into()],
                    remove_labels: vec!["X".into()],
                    ..NodePatch::default()
                },
                expected: Expected::Any,
            }],
            |e| matches!(e, Error::InvalidQuery(_)),
        ),
        (&[attach("a", 1), delete(1)], |e| {
            matches!(e, Error::GraphAttachedRecord)
        }),
        (&[attach("a", 99)], |e| {
            matches!(e, Error::GraphRecordNotAttachable)
        }),
        (&[node("d"), put(1), delete(1), attach("d", 1)], |e| {
            matches!(e, Error::GraphRecordNotAttachable)
        }),
        (
            &[GraphOp::Vector {
                collection: "missing".into(),
                mutation: Mutation::Delete { id: 1 },
            }],
            |e| matches!(e, Error::CollectionNotFound),
        ),
        (
            &[GraphOp::Vector {
                collection: "default".into(),
                mutation: Mutation::Put {
                    id: 3,
                    vector: vec![1.0],
                    payload: Vec::new(),
                },
            }],
            |e| matches!(e, Error::DimensionMismatch { .. }),
        ),
    ];
    for (index, (ops, expected)) in failures.into_iter().enumerate() {
        let error = db.graph_mut("g").unwrap().write_batch(ops).unwrap_err();
        assert!(expected(&error), "{index}: {error:?}");
        assert_eq!(len(&path), before.0, "{index}");
        assert_eq!(state(&db), &before.1, "{index}");
        assert_eq!(db.revision(1).unwrap(), before.2, "{index}");
    }
    // Planning never leaks tentative state: the same keys still work.
    db.graph_mut("g")
        .unwrap()
        .write_batch(&[node("b"), attach("b", 1)])
        .unwrap();
    db.close().unwrap();
    state(&reopened(&path)).check_invariants().unwrap();
}

#[test]
fn edge_retries_are_idempotent_and_conflicts_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("edges.vlite");
    let mut db = graph_db(&path);
    let mut graph = db.graph_mut("g").unwrap();
    graph.write_batch(&[node("a"), node("b")]).unwrap();
    let mut properties = Metadata::new();
    properties.insert("w".into(), MetadataValue::Float(0.5));
    let first = graph
        .create_edge(Some("e"), "a", "LINKS", "b", &properties)
        .unwrap();
    assert!(first.created);
    let end = len(&path);
    let mut graph = db.graph_mut("g").unwrap();
    let retry = graph
        .create_edge(Some("e"), "a", "LINKS", "b", &properties)
        .unwrap();
    assert_eq!(
        retry,
        GraphWrite {
            created: false,
            ..first
        }
    );
    let only_retry = graph.write_batch(&[edge(None, "a", "b")][..0]).unwrap();
    assert_eq!(only_retry.revision, None);
    let mut different = properties.clone();
    different.insert("w".into(), MetadataValue::Float(0.25));
    for result in [
        graph.create_edge(Some("e"), "a", "LINKS", "b", &different),
        graph.create_edge(Some("e"), "b", "LINKS", "a", &properties),
        graph.create_edge(Some("e"), "a", "OTHER", "b", &properties),
    ] {
        assert!(matches!(result, Err(Error::GraphKeyConflict)), "{result:?}");
    }
    assert_eq!(len(&path), end);
    // Keyless edges are parallel edges.
    let parallel = graph
        .create_edge(None, "a", "LINKS", "b", &properties)
        .unwrap();
    assert_ne!(parallel.id, first.id);
    assert_eq!(graph.info().edge_count, 2);
    assert!(matches!(
        graph.delete_node("a", Expected::Any),
        Err(Error::GraphNodeHasEdges { incident: 2 })
    ));
}

#[test]
fn expected_revisions_guard_every_conditional_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("expected.vlite");
    let mut db = graph_db(&path);
    let mut graph = db.graph_mut("g").unwrap();
    let a = graph
        .upsert_node("a", &["Doc"], &Metadata::new(), Expected::Absent)
        .unwrap();
    assert!(a.created);
    assert!(matches!(
        graph.upsert_node("a", &[], &Metadata::new(), Expected::Absent),
        Err(Error::RevisionMismatch { current: Some(r) }) if r == a.revision
    ));
    let patch = NodePatch {
        add_labels: vec!["Topic".into()],
        ..NodePatch::default()
    };
    assert!(matches!(
        graph.patch_node("a", &patch, Expected::Revision(a.revision + 1)),
        Err(Error::RevisionMismatch { current: Some(r) }) if r == a.revision
    ));
    assert!(matches!(
        graph.patch_node("missing", &patch, Expected::Revision(1)),
        Err(Error::RevisionMismatch { current: None })
    ));
    let patched = graph
        .patch_node("a", &patch, Expected::Revision(a.revision))
        .unwrap();
    assert!(patched.revision > a.revision);
    graph.create_node("b", &[], &Metadata::new()).unwrap();
    let e = graph
        .create_edge(Some("e"), "a", "LINKS", "b", &Metadata::new())
        .unwrap();
    let mut set = Metadata::new();
    set.insert("n".into(), MetadataValue::Integer(1));
    assert!(matches!(
        graph.patch_edge(EdgeRef::Key("e".into()), &set, &[], Expected::Absent),
        Err(Error::RevisionMismatch { current: Some(_) })
    ));
    let edited = graph
        .patch_edge(EdgeRef::Id(e.id), &set, &[], Expected::Revision(e.revision))
        .unwrap();
    assert!(matches!(
        graph.delete_edge(EdgeRef::Key("e".into()), Expected::Revision(e.revision)),
        Err(Error::RevisionMismatch { current: Some(r) }) if r == edited.revision
    ));
    graph
        .delete_edge(
            EdgeRef::Key("e".into()),
            Expected::Revision(edited.revision),
        )
        .unwrap();
    assert!(matches!(
        graph.delete_edge(EdgeRef::Id(e.id), Expected::Any),
        Err(Error::GraphEdgeNotFound)
    ));
    assert!(matches!(
        graph.delete_node("a", Expected::Revision(a.revision)),
        Err(Error::RevisionMismatch { .. })
    ));
    graph
        .delete_node("a", Expected::Revision(patched.revision))
        .unwrap();
    assert!(matches!(
        graph.delete_node("a", Expected::Any),
        Err(Error::GraphNodeNotFound)
    ));
}

#[test]
fn batch_limits_and_read_only_handles_write_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("limits.vlite");
    let mut db = graph_db(&path);
    let end = len(&path);
    let too_many: Vec<_> = (0..=MAX_BATCH_ENTRIES)
        .map(|i| node(&format!("n{i}")))
        .collect();
    assert!(matches!(
        db.graph_mut("g").unwrap().write_batch(&too_many),
        Err(Error::BatchTooLarge)
    ));
    // 4096 nodes with 8 KiB of properties exceed the 16 MiB body limit.
    let mut big = Metadata::new();
    big.insert("a".into(), MetadataValue::String("x".repeat(4096)));
    big.insert("b".into(), MetadataValue::String("y".repeat(4096)));
    let too_big: Vec<_> = (0..MAX_BATCH_ENTRIES)
        .map(|i| GraphOp::CreateNode {
            key: format!("n{i}"),
            labels: Vec::new(),
            properties: big.clone(),
        })
        .collect();
    let error = db
        .graph_mut("g")
        .unwrap()
        .write_batch(&too_big)
        .unwrap_err();
    assert!(matches!(error, Error::BatchTooLarge), "{error:?}");
    assert_eq!(len(&path), end);
    let empty = db.graph_mut("g").unwrap().write_batch(&[]).unwrap();
    assert_eq!((empty.revision, empty.outcomes.len()), (None, 0));
    let full = db
        .graph_mut("g")
        .unwrap()
        .write_batch(&too_many[..MAX_BATCH_ENTRIES])
        .unwrap();
    assert_eq!(full.outcomes.len(), MAX_BATCH_ENTRIES);
    db.close().unwrap();

    let before = fs::read(&path).unwrap();
    let mut db = Database::open_read_only(&path).unwrap();
    assert!(matches!(
        db.write_graph_batch(GraphId(0), &[node("x")]),
        Err(Error::ReadOnly)
    ));
    drop(db);
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn attachments_require_live_records_without_expiry() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("attach.vlite");
    let mut db = graph_db(&path);
    db.put(1, &[1.0, 1.0], b"").unwrap();
    db.put_with_expiry(2, &[1.0, 1.0], b"", &Metadata::new(), Some(FAR_FUTURE))
        .unwrap();
    let mut graph = db.graph_mut("g").unwrap();
    graph.write_batch(&[node("a"), node("b")]).unwrap();
    for record in [2, 3] {
        assert!(matches!(
            graph.attach("a", "default", record),
            Err(Error::GraphRecordNotAttachable)
        ));
    }
    assert!(matches!(
        graph.attach("a", "missing", 1),
        Err(Error::CollectionNotFound)
    ));
    graph.attach("a", "default", 1).unwrap();
    assert!(matches!(
        graph.attach("b", "default", 1),
        Err(Error::GraphAttachmentConflict)
    ));
    // A record put earlier in the same batch is attachable.
    graph.write_batch(&[put(3), attach("b", 3)]).unwrap();
    assert!(matches!(
        graph.detach("a", "other"),
        Err(Error::CollectionNotFound)
    ));
    assert_eq!(graph.detach("b", "default").unwrap(), 3);
    assert!(matches!(
        graph.detach("b", "default"),
        Err(Error::GraphAttachmentNotFound)
    ));
    // Detach then delete in one batch is allowed.
    graph
        .write_batch(&[
            GraphOp::Detach {
                node: "a".into(),
                collection: "default".into(),
            },
            delete(1),
        ])
        .unwrap();
    assert_eq!(db.get(1).unwrap(), None);
    db.close().unwrap();
    state(&reopened(&path)).check_invariants().unwrap();
}

#[test]
fn ordinary_writes_keep_attached_records_and_touch_the_owning_graph() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hooks.vlite");
    let mut db = graph_db(&path);
    db.create_graph("other").unwrap();
    db.put(1, &[1.0, 1.0], b"").unwrap();
    db.put(2, &[1.0, 1.0], b"").unwrap();
    db.graph_mut("g")
        .unwrap()
        .write_batch(&[node("a"), attach("a", 1)])
        .unwrap();
    let other = state(&db).graph_by_name("other").unwrap().graph_sequence();
    let end = len(&path);
    // LEG-001..LEG-004 and LEG-007: refused without writing.
    assert!(matches!(db.delete(1), Err(Error::GraphAttachedRecord)));
    assert!(matches!(
        db.delete_if(1, Expected::Any),
        Err(Error::GraphAttachedRecord)
    ));
    assert!(matches!(
        db.write_batch(&[Mutation::Delete { id: 2 }, Mutation::Delete { id: 1 }]),
        Err(Error::GraphAttachedRecord)
    ));
    assert!(matches!(
        db.put_with_expiry(1, &[2.0, 2.0], b"", &Metadata::new(), Some(FAR_FUTURE)),
        Err(Error::GraphAttachedRecordExpiry)
    ));
    let job = db.next_import_job_id().unwrap();
    assert!(matches!(
        db.import_chunk(
            job,
            1,
            &[Mutation::Delete { id: 1 }],
            InvalidRowPolicy::SkipInvalid
        ),
        Err(Error::GraphAttachedRecord)
    ));
    assert_eq!(len(&path), end);
    assert!(db.get(1).unwrap().is_some());
    // Unattached records keep their ordinary behavior.
    assert!(db.delete(2).unwrap());
    // LEG-005/LEG-006: ordinary changes advance only the owner's sequence.
    db.put(1, &[3.0, 3.0], b"new").unwrap();
    let put_at = db.revision(1).unwrap().unwrap();
    assert_eq!(g(&db).graph_sequence(), put_at);
    let mut metadata = Metadata::new();
    metadata.insert("k".into(), MetadataValue::Integer(1));
    let metadata_at = db.update_metadata(1, &metadata).unwrap();
    assert_eq!(g(&db).graph_sequence(), metadata_at);
    db.write_batch(&[Mutation::Put {
        id: 1,
        vector: vec![4.0, 4.0],
        payload: Vec::new(),
    }])
    .unwrap();
    let batch_at = db.revision(1).unwrap().unwrap();
    assert_eq!(g(&db).graph_sequence(), batch_at);
    let job = db.next_import_job_id().unwrap();
    let rows = [Mutation::Put {
        id: 1,
        vector: vec![5.0, 5.0],
        payload: Vec::new(),
    }];
    let report = db
        .import_chunk(job, 1, &rows, InvalidRowPolicy::RejectChunk)
        .unwrap();
    assert_eq!(g(&db).graph_sequence(), report.revision.unwrap());
    db.put(3, &[1.0, 1.0], b"").unwrap();
    assert_eq!(g(&db).graph_sequence(), report.revision.unwrap());
    assert_eq!(
        state(&db).graph_by_name("other").unwrap().graph_sequence(),
        other
    );
    // An already-committed chunk stays idempotent even though it would now
    // be checked against attachments.
    let retry = db
        .import_chunk(job, 1, &rows, InvalidRowPolicy::RejectChunk)
        .unwrap();
    assert_eq!(retry.outcome, crate::ImportChunkOutcome::AlreadyCommitted);
    let live = state(&db).clone();
    db.close().unwrap();
    // Replay reproduces every touch.
    assert_eq!(state(&reopened(&path)), &live);
}

/// Deterministic xorshift generator (no external dependency).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn random_op(rng: &mut Rng) -> GraphOp {
    let key = |rng: &mut Rng| format!("n{}", rng.below(12));
    match rng.below(10) {
        0 => node(&key(rng)),
        1 => GraphOp::UpsertNode {
            key: key(rng),
            labels: vec![format!("L{}", rng.below(3))],
            properties: Metadata::new(),
            expected: Expected::Any,
        },
        2 => GraphOp::DeleteNode {
            key: key(rng),
            expected: Expected::Any,
        },
        3 => edge(
            (rng.below(2) == 0)
                .then(|| format!("e{}", rng.below(8)))
                .as_deref(),
            &key(rng),
            &key(rng),
        ),
        4 => GraphOp::DeleteEdge {
            edge: EdgeRef::Key(format!("e{}", rng.below(8))),
            expected: Expected::Any,
        },
        5 => attach(&key(rng), rng.below(10)),
        6 => GraphOp::Detach {
            node: key(rng),
            collection: "default".into(),
        },
        7 => delete(rng.below(10)),
        _ => put(rng.below(10)),
    }
}

/// Random graph batches interleaved with ordinary vector writes: every
/// failure leaves the file and state unchanged, and every reopen reproduces
/// the live graph state, `graph_sequence` included. Each seed runs 10,000
/// steps against its own file and reopens it every 500 steps (TEST-003);
/// seeds run in parallel because every commit is synced.
#[test]
fn random_batches_are_atomic_and_replay_identically() {
    const SEEDS: u64 = 4;
    const STEPS: u64 = 10_000;
    const REOPEN_EVERY: u64 = 500;
    let dir = tempfile::tempdir().unwrap();
    std::thread::scope(|scope| {
        for seed in 1..=SEEDS {
            let path = dir.path().join(format!("random-{seed}.vlite"));
            scope.spawn(move || {
                let mut db = graph_db(&path);
                let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
                let (mut committed, mut failed, mut reopens) = (0, 0, 0);
                for step in 0..STEPS {
                    let before = (len(&path), state(&db).clone());
                    let result = if rng.below(5) == 0 {
                        let id = rng.below(10);
                        if rng.below(2) == 0 {
                            db.put(id, &[1.0, id as f32], b"").map(drop)
                        } else {
                            db.delete(id).map(drop)
                        }
                    } else {
                        let ops: Vec<_> =
                            (0..1 + rng.below(4)).map(|_| random_op(&mut rng)).collect();
                        db.graph_mut("g").unwrap().write_batch(&ops).map(drop)
                    };
                    match result {
                        Ok(()) => committed += 1,
                        Err(_) => {
                            failed += 1;
                            assert_eq!(len(&path), before.0, "seed {seed} step {step}");
                            assert_eq!(state(&db), &before.1, "seed {seed} step {step}");
                        }
                    }
                    state(&db).check_invariants().unwrap();
                    if step % REOPEN_EVERY == REOPEN_EVERY - 1 {
                        let live = state(&db).clone();
                        db.close().unwrap();
                        db = reopened(&path);
                        reopens += 1;
                        assert_eq!(state(&db), &live, "seed {seed} step {step}");
                    }
                }
                assert!(
                    committed > 1_000 && failed > 1_000,
                    "seed {seed}: {committed}/{failed}"
                );
                assert_eq!(reopens, STEPS / REOPEN_EVERY, "seed {seed}");
            });
        }
    });
}
