//! Gap 4 slice B: temporal writes and storage through the public batch API
//! (capability enabling, atomic corrections, limits, TTL, persistence round
//! trips, encryption and the temporal change feed). Series are read from
//! the engine state: public temporal reads are slice C.

use std::fs;
use std::io::Cursor;
use std::path::Path;

use super::super::{Activation, FAIL_HEADER, segment_in, segment_out};
use super::{header_flags, set_header_flags};
use crate::changes::{
    ChangeBatch, ChangeKind, FeedCursor, GraphChange, TemporalChange, read_log_changes,
};
use crate::format::{self, FLAG_ENCRYPTED, FLAG_GRAPH, FLAG_PRECISION, FLAG_TEMPORAL};
use crate::maintenance;
use crate::property_graph::engine::Graph;
use crate::property_graph::engine::codec::check_series_bytes;
use crate::property_graph::{
    EdgeRef, GraphBatchResult, GraphOp, GraphOpOutcome, GraphWrite, TemporalOp, TemporalSegment,
    verify_graph,
};
use crate::{
    CreateOptions, Database, EncryptionKey, Error, Expected, Metadata, MetadataValue, Mutation,
    Result,
};

/// Expired at any wall-clock time the tests run.
const PAST: u64 = 1;

fn props(value: i64) -> Metadata {
    Metadata::from([("v".to_owned(), MetadataValue::Integer(value))])
}

fn seg(valid_from: Option<i64>, valid_to: Option<i64>, value: i64) -> TemporalSegment {
    TemporalSegment {
        valid_from,
        valid_to,
        properties: props(value),
    }
}

fn edge(key: &str) -> EdgeRef {
    EdgeRef::Key(key.into())
}

/// Temporal edge `key` (or a keyless one) from node `source` to `destination`.
fn create_between(
    key: Option<&str>,
    source: &str,
    destination: &str,
    segments: Vec<TemporalSegment>,
) -> GraphOp {
    GraphOp::Temporal(TemporalOp::CreateEdge {
        key: key.map(str::to_owned),
        source: source.into(),
        relationship: "R".into(),
        destination: destination.into(),
        segments,
    })
}

/// Temporal edge `key`, from the node named by its first character to the
/// node named by its second.
fn create(key: &str, segments: Vec<TemporalSegment>) -> GraphOp {
    let (source, destination) = key.split_at(1);
    create_between(Some(key), source, destination, segments)
}

fn legacy(key: &str, value: i64) -> GraphOp {
    let (source, destination) = key.split_at(1);
    GraphOp::CreateEdge {
        key: Some(key.into()),
        source: source.into(),
        relationship: "R".into(),
        destination: destination.into(),
        properties: props(value),
    }
}

fn close(key: &str, from: Option<i64>, to: Option<i64>, at: i64, expected: Expected) -> GraphOp {
    GraphOp::Temporal(TemporalOp::CloseSegment {
        edge: edge(key),
        valid_from: from,
        valid_to: to,
        at,
        expected,
    })
}

fn append(key: &str, segment: TemporalSegment, expected: Expected) -> GraphOp {
    GraphOp::Temporal(TemporalOp::AppendSegment {
        edge: edge(key),
        segment,
        expected,
    })
}

fn replace(
    key: &str,
    from: Option<i64>,
    to: Option<i64>,
    segment: TemporalSegment,
    expected: Expected,
) -> GraphOp {
    GraphOp::Temporal(TemporalOp::ReplaceSegment {
        edge: edge(key),
        valid_from: from,
        valid_to: to,
        segment,
        expected,
    })
}

fn remove(key: &str, from: Option<i64>, to: Option<i64>, expected: Expected) -> GraphOp {
    GraphOp::Temporal(TemporalOp::RemoveSegment {
        edge: edge(key),
        valid_from: from,
        valid_to: to,
        expected,
    })
}

fn promote(key: &str, revision: u64) -> GraphOp {
    GraphOp::Temporal(TemporalOp::PromoteEdge {
        edge: edge(key),
        expected: Expected::Revision(revision),
    })
}

fn expire_node(key: &str) -> GraphOp {
    GraphOp::SetNodeExpiry {
        key: key.into(),
        expires_at_ms: Some(PAST),
        expected: Expected::Any,
    }
}

fn expire_edge(key: &str) -> GraphOp {
    GraphOp::SetEdgeExpiry {
        edge: edge(key),
        expires_at_ms: Some(PAST),
        expected: Expected::Any,
    }
}

fn put_vector(id: u64) -> GraphOp {
    GraphOp::Vector {
        collection: "default".into(),
        mutation: Mutation::Put {
            id,
            vector: vec![1.0, 0.0],
            payload: Vec::new(),
        },
    }
}

/// Graph "g" with nodes a, b, c and d and the ordinary edge "ac" (v = 7).
fn populate(db: &mut Database) {
    db.create_graph("g").unwrap();
    let mut graph = db.graph_mut("g").unwrap();
    for key in ["a", "b", "c", "d"] {
        graph.create_node(key, &["N"], &Metadata::new()).unwrap();
    }
    graph
        .create_edge(Some("ac"), "a", "R", "c", &props(7))
        .unwrap();
}

fn write(db: &mut Database, ops: &[GraphOp]) -> Result<GraphBatchResult> {
    db.graph_mut("g")?.write_batch(ops)
}

fn graph(db: &Database) -> &Graph {
    db.graph.as_ref().unwrap().graph_by_name("g").unwrap()
}

/// Every stored edge of graph "g", hidden ones not yet reclaimed included:
/// key, ID and series (`None` for an ordinary edge).
type Stored = Vec<(Option<String>, u64, Option<Vec<TemporalSegment>>)>;

fn stored(db: &Database) -> Stored {
    let graph = graph(db);
    graph
        .edges()
        .map(|edge| {
            let series = graph
                .segments(edge.id)
                .map(|series| series.iter().map(segment_out).collect());
            (edge.key.clone(), edge.id.0, series)
        })
        .collect()
}

fn series(db: &Database, key: &str) -> Vec<TemporalSegment> {
    let graph = graph(db);
    let edge = graph.edge_by_key(key).unwrap();
    graph
        .segments(edge.id)
        .unwrap()
        .iter()
        .map(segment_out)
        .collect()
}

fn revision(db: &Database, key: &str) -> u64 {
    graph(db).edge_by_key(key).unwrap().revision
}

fn edge_id(db: &Database, key: &str) -> u64 {
    graph(db).edge_by_key(key).unwrap().id.0
}

fn node_id(db: &Database, key: &str) -> u64 {
    graph(db).node_by_key(key).unwrap().id.0
}

fn is_temporal(db: &Database) -> bool {
    db.graph.as_ref().unwrap().is_temporal()
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path).unwrap().len()
}

/// The file's bytes, read while no handle holds its lock, and a reopened
/// handle.
fn bytes_of(db: Database, path: &Path) -> (Vec<u8>, Database) {
    drop(db);
    let bytes = fs::read(path).unwrap();
    (bytes, Database::open(path).unwrap())
}

#[test]
fn builds_without_support_refuse_temporal_operations_and_change_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.vlite");
    let mut db = Database::create(&path, 2).unwrap();
    populate(&mut db);
    let (before, mut db) = bytes_of(db, &path);
    let _off = Activation::set(false);
    assert!(matches!(
        write(&mut db, &[create("ab", vec![seg(None, None, 1)])]),
        Err(Error::UnsupportedFeature(FLAG_TEMPORAL))
    ));
    assert!(matches!(
        db.temporal_changes_after(&db.feed_cursor().unwrap(), 10),
        Err(Error::UnsupportedFeature(FLAG_TEMPORAL))
    ));
    assert!(!is_temporal(&db));
    assert_eq!(bytes_of(db, &path).0, before);
}

#[test]
fn the_first_temporal_write_enables_the_capability_with_its_frame() {
    let _on = Activation::on();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.vlite");
    let mut db = Database::create(&path, 2).unwrap();
    populate(&mut db);
    let legacy_flags = db.flags;
    assert_eq!(legacy_flags & FLAG_TEMPORAL, 0);
    let (before, mut db) = bytes_of(db, &path);
    // Rejected batches with temporal operations change nothing, the header
    // included: invalid series, unknown endpoints, and a later failure.
    assert!(matches!(
        write(&mut db, &[create("ab", vec![seg(Some(5), Some(1), 1)])]),
        Err(Error::InvalidQuery(_))
    ));
    assert!(matches!(
        write(&mut db, &[create("ax", Vec::new())]),
        Err(Error::GraphNodeNotFound)
    ));
    assert!(matches!(
        write(
            &mut db,
            &[
                create("ab", vec![seg(None, None, 1)]),
                GraphOp::DeleteNode {
                    key: "zz".into(),
                    expected: Expected::Any,
                },
            ]
        ),
        Err(Error::GraphNodeNotFound)
    ));
    assert!(!is_temporal(&db));
    let (bytes, mut db) = bytes_of(db, &path);
    assert_eq!(bytes, before);
    // Ordinary writes never enable it.
    write(&mut db, &[legacy("bc", 1)]).unwrap();
    assert_eq!(db.flags, legacy_flags);

    let result = write(
        &mut db,
        &[create(
            "ab",
            vec![seg(None, Some(10), 1), seg(Some(10), None, 2)],
        )],
    )
    .unwrap();
    let id = edge_id(&db, "ab");
    assert_eq!(
        result.outcomes,
        [GraphOpOutcome::Edge(GraphWrite {
            id,
            created: true,
            revision: result.revision.unwrap(),
        })]
    );
    assert_eq!(db.flags, legacy_flags | FLAG_TEMPORAL);
    assert!(is_temporal(&db));
    let expected = stored(&db);
    drop(db);
    assert_eq!(header_flags(&path), legacy_flags | FLAG_TEMPORAL);

    {
        let _off = Activation::set(false);
        assert!(matches!(
            Database::open(&path),
            Err(Error::UnsupportedFeature(FLAG_TEMPORAL))
        ));
    }
    let db = Database::open(&path).unwrap();
    assert_eq!(stored(&db), expected);
    assert_eq!(
        series(&db, "ab"),
        [seg(None, Some(10), 1), seg(Some(10), None, 2)]
    );
    assert!(is_temporal(&db));
}

#[test]
fn corrections_are_atomic_and_revision_checked_against_the_batch_start() {
    let _on = Activation::on();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.vlite");
    let mut db = Database::create(&path, 2).unwrap();
    populate(&mut db);
    write(&mut db, &[create("ab", vec![seg(None, None, 1)])]).unwrap();
    let r0 = revision(&db, "ab");
    let id = edge_id(&db, "ab");

    // Supersession: close the open segment and append its successor in one
    // batch, both checked against the revision before the batch.
    let result = write(
        &mut db,
        &[
            close("ab", None, None, 10, Expected::Revision(r0)),
            append("ab", seg(Some(10), None, 2), Expected::Revision(r0)),
        ],
    )
    .unwrap();
    let r1 = result.revision.unwrap();
    assert!(r1 > r0);
    let written = GraphOpOutcome::Edge(GraphWrite {
        id,
        created: false,
        revision: r1,
    });
    assert_eq!(result.outcomes, [written, written]);
    assert_eq!(revision(&db, "ab"), r1);
    let superseded = [seg(None, Some(10), 1), seg(Some(10), None, 2)];
    assert_eq!(series(&db, "ab"), superseded);

    // A stale revision fails; a failing later operation rolls the whole
    // batch back, its vector mutation included.
    let len = file_len(&path);
    assert!(matches!(
        write(&mut db, &[close("ab", Some(10), None, 20, Expected::Revision(r0))]),
        Err(Error::RevisionMismatch { current }) if current == Some(r1)
    ));
    assert!(matches!(
        write(
            &mut db,
            &[
                put_vector(9),
                close("ab", Some(10), None, 20, Expected::Revision(r1)),
                append("ab", seg(Some(15), Some(30), 3), Expected::Revision(r1)),
            ]
        ),
        Err(Error::InvalidQuery(_))
    ));
    assert_eq!(file_len(&path), len);
    assert_eq!(series(&db, "ab"), superseded);
    assert_eq!(revision(&db, "ab"), r1);
    assert!(db.get(9).unwrap().is_none());

    // Closing at the finite end changes nothing and commits nothing.
    let result = write(
        &mut db,
        &[close("ab", None, Some(10), 10, Expected::Revision(r1))],
    )
    .unwrap();
    assert_eq!((result.revision, result.outcomes), (None, vec![written]));
    assert_eq!(file_len(&path), len);

    // Replace and remove address segments by their exact bounds.
    let r2 = write(
        &mut db,
        &[replace(
            "ab",
            Some(10),
            None,
            seg(Some(10), Some(40), 4),
            Expected::Revision(r1),
        )],
    )
    .unwrap()
    .revision
    .unwrap();
    assert!(matches!(
        write(
            &mut db,
            &[remove("ab", None, Some(9), Expected::Revision(r2))]
        ),
        Err(Error::InvalidQuery(_))
    ));
    write(
        &mut db,
        &[remove("ab", None, Some(10), Expected::Revision(r2))],
    )
    .unwrap();
    assert_eq!(series(&db, "ab"), [seg(Some(10), Some(40), 4)]);

    // Ordinary patches cannot touch a temporal edge.
    assert!(matches!(
        write(
            &mut db,
            &[GraphOp::PatchEdge {
                edge: edge("ab"),
                set: props(5),
                unset: Vec::new(),
                expected: Expected::Any,
            }]
        ),
        Err(Error::InvalidQuery(_))
    ));

    // Promotion keeps the ordinary edge's identity; its properties become
    // one all-time segment. Corrections and promotion need a revision.
    let ac = edge_id(&db, "ac");
    assert!(matches!(
        write(
            &mut db,
            &[GraphOp::Temporal(TemporalOp::PromoteEdge {
                edge: edge("ac"),
                expected: Expected::Any,
            })]
        ),
        Err(Error::InvalidQuery(_))
    ));
    let ac_revision = revision(&db, "ac");
    let result = write(&mut db, &[promote("ac", ac_revision)]).unwrap();
    assert!(matches!(
        result.outcomes[..],
        [GraphOpOutcome::Edge(GraphWrite { id, created: false, .. })] if id == ac
    ));
    assert_eq!(series(&db, "ac"), [seg(None, None, 7)]);

    let expected = stored(&db);
    drop(db);
    assert_eq!(stored(&Database::open(&path).unwrap()), expected);
}

/// A segment with about 60 KiB of properties.
fn large_segment() -> TemporalSegment {
    let mut properties = props(0);
    for field in 0..15 {
        properties.insert(
            format!("f{field}"),
            MetadataValue::String("x".repeat(4_000)),
        );
    }
    TemporalSegment {
        valid_from: None,
        valid_to: None,
        properties,
    }
}

#[test]
fn limits_roll_back_the_whole_plan_including_tentative_ttl_cleanup() {
    let _on = Activation::on();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.vlite");
    let mut db = Database::create(&path, 2).unwrap();
    populate(&mut db);
    // "bc" is due: the next committed write reclaims it.
    write(&mut db, &[legacy("bc", 1), expire_edge("bc")]).unwrap();
    let due = stored(&db);
    assert!(due.iter().any(|(key, ..)| key.as_deref() == Some("bc")));
    let flags = db.flags;
    let len = file_len(&path);

    // The operation cap counts temporal operations.
    let many: Vec<GraphOp> = (0..4_097)
        .map(|_| create_between(None, "a", "b", Vec::new()))
        .collect();
    assert!(matches!(write(&mut db, &many), Err(Error::BatchTooLarge)));
    // The encoded byte limit is checked before anything is written.
    let heavy: Vec<GraphOp> = (0..300)
        .map(|_| create_between(None, "a", "b", vec![large_segment()]))
        .collect();
    assert!(matches!(write(&mut db, &heavy), Err(Error::BatchTooLarge)));
    // Neither enabled the capability nor ran the tentative TTL cleanup.
    assert_eq!((db.flags, file_len(&path)), (flags, len));
    assert_eq!(stored(&db), due);
    assert!(!is_temporal(&db));

    // The 257th segment of a series fails and rolls back its batch.
    let full: Vec<TemporalSegment> = (0..256).map(|i| seg(Some(i), Some(i + 1), 0)).collect();
    write(&mut db, &[put_vector(2), create("ab", full)]).unwrap();
    assert_eq!(db.flags, flags | FLAG_TEMPORAL);
    // That write reclaimed "bc" in the same frame.
    let at_cap = stored(&db);
    assert!(!at_cap.iter().any(|(key, ..)| key.as_deref() == Some("bc")));
    let len = file_len(&path);
    let r = revision(&db, "ab");
    write(&mut db, &[legacy("bd", 1), expire_edge("bd")]).unwrap();
    let due = stored(&db);
    let len_due = file_len(&path);
    assert!(len_due > len);
    assert!(matches!(
        write(
            &mut db,
            &[
                put_vector(3),
                append("ab", seg(Some(300), None, 0), Expected::Revision(r)),
            ]
        ),
        Err(Error::InvalidQuery(_))
    ));
    assert_eq!(file_len(&path), len_due);
    assert_eq!(stored(&db), due);
    assert!(db.get(3).unwrap().is_none());

    // One committed frame holds the TTL cleanup, the temporal change and
    // the vector mutation.
    let result = write(
        &mut db,
        &[
            put_vector(4),
            remove("ab", Some(0), Some(1), Expected::Revision(r)),
        ],
    )
    .unwrap();
    assert!(result.revision.is_some());
    assert_eq!(stored(&db).len(), at_cap.len());
    assert_eq!(series(&db, "ab").len(), 255);
    let expected = stored(&db);
    drop(db);
    let db = Database::open(&path).unwrap();
    assert_eq!(stored(&db), expected);
    assert!(db.get(2).unwrap().is_some() && db.get(4).unwrap().is_some());
}

/// Segment `[from, to)` whose properties hold `chars` string bytes, in
/// 4,096-byte values: 65,372 make the largest properties allowed.
fn sized_segment(chars: usize, from: i64, to: Option<i64>) -> TemporalSegment {
    let mut properties: Metadata = (0..chars / 4096)
        .map(|f| (format!("f{f:02}"), MetadataValue::String("x".repeat(4096))))
        .collect();
    properties.insert(
        "fzz".into(),
        MetadataValue::String("x".repeat(chars % 4096)),
    );
    TemporalSegment {
        valid_from: Some(from),
        valid_to: to,
        properties,
    }
}

/// Closing an open-ended segment adds its end (8 bytes) to the series, so
/// a close that would outgrow one graph body fails and rolls back its
/// whole batch, the tentative TTL cleanup and vector mutation included.
#[test]
fn a_close_that_outgrows_the_series_bytes_rolls_back_its_batch() {
    let _on = Activation::on();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.vlite");
    let mut db = Database::create(&path, 2).unwrap();
    populate(&mut db);
    // 255 maximum-size segments and an open last one fill one graph body
    // exactly.
    let full: Vec<TemporalSegment> = (0..255)
        .map(|i| sized_segment(65_372, i, Some(i + 1)))
        .collect();
    let prefix: Vec<_> = full.iter().map(|s| segment_in(s).unwrap()).collect();
    let fits = |chars| {
        segment_in(&sized_segment(chars, 255, None)).is_ok_and(|last| {
            let mut series = prefix.clone();
            series.push(last);
            check_series_bytes("R", Some("ab"), &series).is_ok()
        })
    };
    let (mut fit, mut over) = (0, 65_536);
    while over - fit > 1 {
        let mid = (fit + over) / 2;
        if fits(mid) { fit = mid } else { over = mid }
    }
    drop(prefix);
    write(&mut db, &[create("ab", full[..128].to_vec())]).unwrap();
    let r = revision(&db, "ab");
    let rest: Vec<GraphOp> = full[128..]
        .iter()
        .chain([&sized_segment(fit, 255, None)])
        .map(|segment| append("ab", segment.clone(), Expected::Revision(r)))
        .collect();
    write(&mut db, &rest).unwrap();
    assert_eq!(series(&db, "ab").len(), 256);
    write(&mut db, &[legacy("bd", 1), expire_edge("bd")]).unwrap();
    let (due, len, r) = (stored(&db), file_len(&path), revision(&db, "ab"));

    let closing = close("ab", Some(255), None, 300, Expected::Revision(r));
    assert!(matches!(
        write(&mut db, &[put_vector(3), closing.clone()]),
        Err(Error::InvalidQuery(_))
    ));
    assert_eq!(file_len(&path), len);
    assert!(stored(&db) == due && revision(&db, "ab") == r);
    assert!(db.get(3).unwrap().is_none());
    // Removing a segment earlier in the batch makes room.
    write(
        &mut db,
        &[
            remove("ab", Some(0), Some(1), Expected::Revision(r)),
            closing,
        ],
    )
    .unwrap();
    let closed = series(&db, "ab");
    assert_eq!((closed.len(), closed[254].valid_to), (255, Some(300)));
    let expected = stored(&db);
    drop(db);
    assert!(stored(&Database::open(&path).unwrap()) == expected);
    verify_graph(&path, None).unwrap();
}

/// An ordinary mutation earlier in the batch does not change the revision
/// a temporal correction checks; a deadline that falls due still deletes
/// the edge before the correction.
#[test]
fn an_earlier_ordinary_mutation_keeps_the_batch_start_revision() {
    let _on = Activation::on();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.vlite");
    let mut db = Database::create(&path, 2).unwrap();
    populate(&mut db);
    write(&mut db, &[create("ab", vec![seg(Some(0), None, 1)])]).unwrap();
    let r = revision(&db, "ab");
    let expire = |at, expected| GraphOp::SetEdgeExpiry {
        edge: edge("ab"),
        expires_at_ms: Some(at),
        expected: Expected::Revision(expected),
    };
    // 2100-01-01.
    let result = write(
        &mut db,
        &[
            expire(4_102_444_800_000, r),
            append("ab", seg(None, Some(0), 0), Expected::Revision(r)),
        ],
    )
    .unwrap();
    let r = result.revision.unwrap();
    assert_eq!(
        series(&db, "ab"),
        [seg(None, Some(0), 0), seg(Some(0), None, 1)]
    );
    assert_eq!(revision(&db, "ab"), r);
    let before = stored(&db);
    assert!(matches!(
        write(
            &mut db,
            &[
                expire(PAST, r),
                append("ab", seg(Some(-9), Some(-5), 2), Expected::Revision(r)),
            ]
        ),
        Err(Error::RevisionMismatch { current: None })
    ));
    assert_eq!(stored(&db), before);
}

#[test]
fn ttl_reclaims_whole_series_and_history_never_restores_them() {
    let _on = Activation::on();
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("s.vlite");
    let mut db = Database::create(&source, 2).unwrap();
    populate(&mut db);
    write(
        &mut db,
        &[
            create("ab", vec![seg(None, Some(10), 1), seg(Some(10), None, 2)]),
            create("bc", vec![seg(None, None, 3)]),
            // A closed interval is history, not expiry: it stays.
            create("cb", vec![seg(Some(0), Some(5), 4)]),
        ],
    )
    .unwrap();
    // Node "a" takes "ab" and "ac" with it; "bc" expires alone.
    write(&mut db, &[expire_node("a"), expire_edge("bc")]).unwrap();
    assert_eq!(db.graph("g").unwrap().info().edge_count, 1);
    let high = graph(&db).next_edge_id();
    let cb = (
        Some("cb".to_owned()),
        edge_id(&db, "cb"),
        Some(vec![seg(Some(0), Some(5), 4)]),
    );
    drop(db);

    // Compaction drops the due series and keeps the closed interval and
    // the ID high-water.
    let compacted = dir.path().join("c.vlite");
    maintenance::compact(&source, &compacted).unwrap();
    assert_eq!(header_flags(&compacted) & FLAG_TEMPORAL, FLAG_TEMPORAL);
    verify_graph(&compacted, None).unwrap();
    {
        let db = Database::open(&compacted).unwrap();
        assert_eq!(stored(&db), std::slice::from_ref(&cb));
        assert!(graph(&db).node_by_key("a").is_none());
        assert_eq!(graph(&db).next_edge_id(), high);
    }

    // The next write reclaims them in the source; reusing a key creates a
    // new edge with only its own series.
    let mut db = Database::open(&source).unwrap();
    write(&mut db, &[create("bc", vec![seg(Some(20), None, 5)])]).unwrap();
    let bc = edge_id(&db, "bc");
    assert!(bc >= high);
    assert_eq!(
        stored(&db),
        [
            cb,
            (
                Some("bc".to_owned()),
                bc,
                Some(vec![seg(Some(20), None, 5)])
            )
        ]
    );
    let expected = stored(&db);
    drop(db);
    assert_eq!(stored(&Database::open(&source).unwrap()), expected);
    verify_graph(&source, None).unwrap();
}

#[test]
fn temporal_state_survives_compaction_conversion_and_checkpoints() {
    let _on = Activation::on();
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("s.vlite");
    let mut db = Database::create(&source, 2).unwrap();
    populate(&mut db);
    let ac = revision(&db, "ac");
    write(
        &mut db,
        &[
            // A closed and a future segment.
            create("ab", vec![seg(None, Some(10), 1), seg(Some(100), None, 2)]),
            promote("ac", ac),
            legacy("bc", 3),
        ],
    )
    .unwrap();
    let expected = stored(&db);
    let high = graph(&db).next_edge_id();
    drop(db);

    let compacted = dir.path().join("c.vlite");
    maintenance::compact(&source, &compacted).unwrap();
    assert_eq!(
        header_flags(&compacted) & (FLAG_GRAPH | FLAG_TEMPORAL),
        FLAG_GRAPH | FLAG_TEMPORAL
    );
    {
        let mut db = Database::open(&compacted).unwrap();
        assert_eq!(stored(&db), expected);
        assert_eq!(graph(&db).next_edge_id(), high);
        // The compacted file keeps accepting corrections.
        let r = revision(&db, "ab");
        write(
            &mut db,
            &[append(
                "ab",
                seg(Some(10), Some(100), 9),
                Expected::Revision(r),
            )],
        )
        .unwrap();
    }

    // A converted store inherits the capability, takes corrections, and its
    // checkpoint image restates the series.
    let store = dir.path().join("store");
    maintenance::convert_to_directory(&source, &store).unwrap();
    let mut db = Database::open(&store).unwrap();
    db.set_checkpoint_threshold(None);
    assert_eq!(stored(&db), expected);
    let r = revision(&db, "ab");
    write(
        &mut db,
        &[append(
            "ab",
            seg(Some(10), Some(100), 5),
            Expected::Revision(r),
        )],
    )
    .unwrap();
    assert!(db.checkpoint().unwrap().is_some());
    let r = revision(&db, "ab");
    write(
        &mut db,
        &[remove("ab", None, Some(10), Expected::Revision(r))],
    )
    .unwrap();
    let after = stored(&db);
    drop(db);
    let db = Database::open(&store).unwrap();
    assert_eq!(stored(&db), after);
    assert_eq!(
        series(&db, "ab"),
        [seg(Some(10), Some(100), 5), seg(Some(100), None, 2)]
    );
    drop(db);

    // A directory store cannot enable the capability itself.
    let fresh = dir.path().join("fresh");
    let mut db = Database::create_directory(&fresh, 2, CreateOptions::default()).unwrap();
    populate(&mut db);
    let before = stored(&db);
    assert!(matches!(
        write(&mut db, &[create("ab", vec![seg(None, None, 1)])]),
        Err(Error::UnsupportedFeature(FLAG_TEMPORAL))
    ));
    assert_eq!(stored(&db), before);
    assert!(!is_temporal(&db));
    write(&mut db, &[legacy("ab", 1)]).unwrap();
}

/// A temporal edge keeps its revision through directory conversion, a
/// checkpoint image and reopening it (Speed P2b snapshot load), without
/// precision: the image restates it, not the image sequence.
#[test]
fn temporal_revisions_survive_conversion_checkpoints_and_reopen() {
    let _on = Activation::on();
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("s.vlite");
    let mut db = Database::create(&source, 2).unwrap();
    populate(&mut db);
    write(&mut db, &[create("ab", vec![seg(None, None, 1)])]).unwrap();
    let original = revision(&db, "ab");
    // Later commits move the sequence past the edge's revision.
    write(&mut db, &[legacy("bc", 1)]).unwrap();
    write(&mut db, &[legacy("bd", 2)]).unwrap();
    drop(db);

    let store = dir.path().join("store");
    maintenance::convert_to_directory(&source, &store).unwrap();
    let mut db = Database::open(&store).unwrap();
    db.set_checkpoint_threshold(None);
    assert_eq!(revision(&db, "ab"), original);
    write(&mut db, &[legacy("cd", 3)]).unwrap();
    assert!(db.checkpoint().unwrap().is_some());
    assert_eq!(revision(&db, "ab"), original);
    drop(db);
    let mut db = Database::open(&store).unwrap();
    assert_eq!(revision(&db, "ab"), original);
    // The token read before conversion still matches.
    write(
        &mut db,
        &[close("ab", None, None, 10, Expected::Revision(original))],
    )
    .unwrap();
    let corrected = revision(&db, "ab");
    assert!(corrected > original);
    write(&mut db, &[legacy("ca", 4)]).unwrap();
    assert!(db.checkpoint().unwrap().is_some());
    drop(db);
    let db = Database::open(&store).unwrap();
    assert_eq!(revision(&db, "ab"), corrected);
}

#[test]
fn encrypted_temporal_files_round_trip() {
    let _on = Activation::on();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.vlite");
    let key = EncryptionKey::from_bytes([7; 32]);
    let options = CreateOptions {
        encryption: Some(EncryptionKey::from_bytes([7; 32])),
        ..CreateOptions::default()
    };
    let mut db = Database::create_with_options(&path, 2, options).unwrap();
    populate(&mut db);
    write(&mut db, &[create("ab", vec![seg(None, Some(10), 1)])]).unwrap();
    let expected = stored(&db);
    drop(db);
    assert_eq!(
        header_flags(&path) & (FLAG_ENCRYPTED | FLAG_TEMPORAL),
        FLAG_ENCRYPTED | FLAG_TEMPORAL
    );
    assert!(matches!(
        Database::open(&path),
        Err(Error::EncryptionKeyRequired)
    ));
    assert!(matches!(
        Database::open_encrypted(&path, &EncryptionKey::from_bytes([8; 32])),
        Err(Error::EncryptionKeyMismatch { .. })
    ));
    assert_eq!(
        stored(&Database::open_encrypted(&path, &key).unwrap()),
        expected
    );
    let compacted = dir.path().join("c.vlite");
    maintenance::compact_with_key(&path, &compacted, &key).unwrap();
    assert_eq!(
        stored(&Database::open_encrypted(&compacted, &key).unwrap()),
        expected
    );
}

#[test]
fn read_only_handles_refuse_and_a_failed_header_write_poisons() {
    let _on = Activation::on();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.vlite");
    let mut db = Database::create(&path, 2).unwrap();
    populate(&mut db);
    drop(db);
    let before = fs::read(&path).unwrap();
    {
        let mut db = Database::open_read_only(&path).unwrap();
        let result = db
            .graph_mut("g")
            .and_then(|mut graph| graph.write_batch(&[create("ab", Vec::new())]));
        assert!(matches!(result, Err(Error::ReadOnly)));
    }

    let mut db = Database::open(&path).unwrap();
    FAIL_HEADER.with(|fail| fail.set(true));
    assert!(matches!(
        write(&mut db, &[create("ab", vec![seg(None, None, 1)])]),
        Err(Error::CommitIndeterminate(_))
    ));
    assert!(write(&mut db, &[legacy("bc", 1)]).is_err());
    drop(db);
    assert_eq!(fs::read(&path).unwrap(), before);
    let _off = Activation::set(false);
    Database::open(&path).unwrap();
}

fn graph_batches(batch: &ChangeBatch) -> Vec<(u64, Vec<GraphChange>)> {
    batch
        .changes
        .iter()
        .map(|change| match &change.kind {
            ChangeKind::GraphBatch {
                revision,
                operations,
                ..
            } => (*revision, operations.clone()),
            other => panic!("unexpected change {other:?}"),
        })
        .collect()
}

#[test]
fn only_the_temporal_feed_reports_temporal_changes_in_commit_order() {
    let _on = Activation::on();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.vlite");
    let mut db = Database::create(&path, 2).unwrap();
    populate(&mut db);
    let cursor = db.feed_cursor().unwrap();
    let s1 = write(&mut db, &[create("ab", vec![seg(None, None, 1)])])
        .unwrap()
        .revision
        .unwrap();
    let s2 = write(
        &mut db,
        &[
            close("ab", None, None, 10, Expected::Revision(s1)),
            append("ab", seg(Some(10), None, 2), Expected::Revision(s1)),
        ],
    )
    .unwrap()
    .revision
    .unwrap();
    let ac = revision(&db, "ac");
    let s3 = write(&mut db, &[promote("ac", ac)])
        .unwrap()
        .revision
        .unwrap();
    let s4 = write(
        &mut db,
        &[
            replace(
                "ab",
                Some(10),
                None,
                seg(Some(10), Some(50), 3),
                Expected::Revision(s2),
            ),
            remove("ab", None, Some(10), Expected::Revision(s2)),
        ],
    )
    .unwrap()
    .revision
    .unwrap();

    // Feeds without temporal changes refuse rather than omit them.
    assert!(matches!(
        db.changes_after(&cursor, 100),
        Err(Error::UnsupportedFeature(FLAG_TEMPORAL))
    ));

    let ab = Some("ab".to_owned());
    let id = edge_id(&db, "ab");
    let temporal = GraphChange::Temporal;
    let batch = db.temporal_changes_after(&cursor, 100).unwrap();
    assert_eq!(batch.head, s4);
    assert_eq!(
        graph_batches(&batch),
        [
            (
                s1,
                vec![temporal(TemporalChange::EdgePut {
                    id,
                    key: ab.clone(),
                    source: node_id(&db, "a"),
                    source_key: "a".into(),
                    relationship: "R".into(),
                    destination: node_id(&db, "b"),
                    destination_key: "b".into(),
                    segments: vec![seg(None, None, 1)],
                    created: true,
                })]
            ),
            (
                s2,
                vec![
                    temporal(TemporalChange::SegmentClosed {
                        id,
                        key: ab.clone(),
                        valid_from: None,
                        valid_to: None,
                        at: 10,
                    }),
                    temporal(TemporalChange::SegmentAppended {
                        id,
                        key: ab.clone(),
                        segment: seg(Some(10), None, 2),
                    }),
                ]
            ),
            (
                s3,
                vec![temporal(TemporalChange::EdgePut {
                    id: edge_id(&db, "ac"),
                    key: Some("ac".into()),
                    source: node_id(&db, "a"),
                    source_key: "a".into(),
                    relationship: "R".into(),
                    destination: node_id(&db, "c"),
                    destination_key: "c".into(),
                    segments: vec![seg(None, None, 7)],
                    created: false,
                })]
            ),
            (
                s4,
                vec![
                    temporal(TemporalChange::SegmentReplaced {
                        id,
                        key: ab.clone(),
                        valid_from: Some(10),
                        valid_to: None,
                        segment: seg(Some(10), Some(50), 3),
                    }),
                    temporal(TemporalChange::SegmentRemoved {
                        id,
                        key: ab,
                        valid_from: None,
                        valid_to: Some(10),
                    }),
                ]
            ),
        ]
    );
    // Cursors continue in commit order.
    let later = FeedCursor {
        lineage: cursor.lineage,
        sequence: s2,
    };
    let revisions: Vec<u64> = graph_batches(&db.temporal_changes_after(&later, 100).unwrap())
        .into_iter()
        .map(|(revision, _)| revision)
        .collect();
    assert_eq!(revisions, [s3, s4]);
    // So does the closed-file feed, before and after compaction.
    drop(db);
    assert!(matches!(
        read_log_changes(&path, None, &cursor, 100),
        Err(Error::UnsupportedFeature(FLAG_TEMPORAL))
    ));
    let compacted = dir.path().join("c.vlite");
    maintenance::compact(&path, &compacted).unwrap();
    assert!(matches!(
        read_log_changes(&compacted, None, &FeedCursor::beginning(), 100),
        Err(Error::UnsupportedFeature(FLAG_TEMPORAL))
    ));
    // In a compacted image the snapshot names later changes' edges and is
    // not reported again.
    {
        let mut db = Database::open(&compacted).unwrap();
        let head = db.feed_cursor().unwrap();
        let r = revision(&db, "ab");
        let s5 = write(
            &mut db,
            &[append("ab", seg(Some(60), None, 6), Expected::Revision(r))],
        )
        .unwrap()
        .revision
        .unwrap();
        assert_eq!(
            graph_batches(&db.temporal_changes_after(&head, 100).unwrap()),
            [(
                s5,
                vec![temporal(TemporalChange::SegmentAppended {
                    id,
                    key: Some("ab".into()),
                    segment: seg(Some(60), None, 6),
                })]
            )]
        );
    }
    // Temporal entries without the capability are corrupt to replay and
    // to every feed.
    set_header_flags(&path, header_flags(&path) & !FLAG_TEMPORAL);
    assert!(matches!(Database::open(&path), Err(Error::Corrupt { .. })));
    assert!(matches!(
        read_log_changes(&path, None, &cursor, 100),
        Err(Error::Corrupt { .. })
    ));
}

#[test]
fn a_precision_image_keeps_series_and_revisions() {
    let _precision = crate::compact::Activation::on();
    let _on = Activation::on();
    let header = format::header_v5_flags(2, 0x5EED_0000_0000_0064, 0, FLAG_GRAPH, None);
    let mut loaded = format::load_with(&mut Cursor::new(header), None).unwrap();
    loaded.flags |= FLAG_PRECISION;
    loaded.sequence = 1;
    loaded.collections.get_mut(&0).unwrap().precision =
        Some(crate::compact::catalog::Precision::Float32);
    let image = |loaded: &format::Loaded| {
        let mut bytes = Vec::new();
        maintenance::write_v5_image(
            &mut bytes,
            loaded,
            loaded.lineage.unwrap(),
            None,
            None,
            loaded.cipher.as_ref(),
            loaded.flags & (FLAG_GRAPH | FLAG_PRECISION | FLAG_TEMPORAL),
            loaded.sequence,
        )
        .unwrap();
        bytes
    };
    let mut db = Database::open_from_bytes(image(&loaded), None).unwrap();
    let start = db.feed_cursor().unwrap();
    populate(&mut db);
    write(
        &mut db,
        &[create(
            "ab",
            vec![seg(None, Some(10), 1), seg(Some(10), None, 2)],
        )],
    )
    .unwrap();
    let ac = revision(&db, "ac");
    write(&mut db, &[promote("ac", ac)]).unwrap();
    let observe = |db: &Database| {
        (
            stored(db),
            revision(db, "ab"),
            revision(db, "ac"),
            graph(db).next_edge_id(),
        )
    };
    let before = observe(&db);
    // A graph first created after the image has the file's capability.
    let batch = db.temporal_changes_after(&start, 100).unwrap();
    let temporal = batch
        .changes
        .iter()
        .filter(|change| {
            matches!(&change.kind, ChangeKind::GraphBatch { operations, .. }
                if matches!(operations[..], [GraphChange::Temporal(_)]))
        })
        .count();
    assert_eq!(temporal, 2);
    let loaded = format::load_with(&mut Cursor::new(db.to_bytes().unwrap()), None).unwrap();
    assert_eq!(
        loaded.flags & (FLAG_PRECISION | FLAG_TEMPORAL),
        FLAG_PRECISION | FLAG_TEMPORAL
    );
    let mut reopened = Database::open_from_bytes(image(&loaded), None).unwrap();
    assert_eq!(observe(&reopened), before);

    // The feed checks the graph frames after a precision image as replay
    // follows them (P3a), temporal ones included: it reports them, and the
    // feeds without temporal changes refuse them as unsupported.
    let head = reopened.feed_cursor().unwrap();
    let r = revision(&reopened, "ab");
    let s = write(
        &mut reopened,
        &[
            close("ab", Some(10), None, 20, Expected::Revision(r)),
            append("ab", seg(Some(20), None, 3), Expected::Revision(r)),
            legacy("bd", 4),
        ],
    )
    .unwrap()
    .revision
    .unwrap();
    let batch = reopened.temporal_changes_after(&head, 10).unwrap();
    assert_eq!(graph_batches(&batch)[0].0, s);
    assert_eq!(graph_batches(&batch)[0].1.len(), 3);
    assert!(matches!(
        reopened.changes_after(&head, 10),
        Err(Error::UnsupportedFeature(FLAG_TEMPORAL))
    ));
    let after = observe(&reopened);
    let bytes = reopened.to_bytes().unwrap();
    assert_eq!(
        observe(&Database::open_from_bytes(bytes, None).unwrap()),
        after
    );
}
