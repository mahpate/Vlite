//! TEST-007: bounded reclamation of expired receipt groups. Every expectation
//! is derived from the reclamation rules (grace, exact shapes, chunk
//! relations, edges, attachments, barriers, budgets, window and fallback),
//! never from the planner's output.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use sha2::{Digest, Sha256};

use super::super::consolidate::{ConsolidateInput, ConsolidateRequest, ConsolidateSummary};
use super::super::erasure::EraseScopeRequest;
use super::super::{
    FAIL_NEXT_WRITE_BATCH, ForgetRequest, ForgetTarget, LABEL_RECEIPT, LABEL_RECEIPT_OUTCOME,
    MAX_OPS, MemoryError, MemoryItem, MemoryKind, MemoryMut, MemoryScope, RememberRequest,
    RememberResult, forget_receipt_key, hex, operation_receipt_key, receipt_key,
    receipt_outcome_key,
};
use super::{
    FAIL_RECLAIM_ATTEMPT, Limits, RECEIPT_RECLAIM_GRACE_MS, RECLAIM_MAX_OPS, RECLAIM_MAX_READS,
    RECLAIM_SCAN, RECLAIM_SEED_MULTIPLIER, plan_with, write,
};
use crate::{
    Database, Error, Expected, Metadata, MetadataValue,
    changes::{ChangeKind, FeedCursor},
    property_graph::{FindNodes, GraphChange, GraphLimits, GraphOp, ReadOptions},
};

const GRAPH: &str = "memory";
/// The request clock at which a group expiring at 110 first becomes eligible.
const ELIGIBLE: u64 = 110 + RECEIPT_RECLAIM_GRACE_MS;
const WIDE: Limits = Limits {
    scan: RECLAIM_SCAN,
    max_ops: MAX_OPS,
    max_reads: usize::MAX,
};

fn database(path: &Path) -> Database {
    let mut db = Database::create(path.join("reclaim.vlite"), 2).unwrap();
    db.create_graph(GRAPH).unwrap();
    db
}

fn scope(agent: &str) -> MemoryScope {
    MemoryScope {
        tenant: "tenant".into(),
        agent: agent.into(),
        user: None,
        session: None,
    }
}

fn provision(db: &mut Database, scope: &MemoryScope) {
    MemoryMut::bind(db, GRAPH, None)
        .unwrap()
        .provision_scope(scope)
        .unwrap();
}

fn item(id: &str, content: &str, now_ms: u64) -> MemoryItem {
    MemoryItem {
        id: id.into(),
        kind: MemoryKind::Entity,
        content: content.into(),
        source: "source".into(),
        importance: None,
        observed_at_ms: Some(now_ms as i64),
        provenance: Vec::new(),
        references: Vec::new(),
        merge_into: None,
        expected_revision: None,
        expiry_ms: None,
        expiry_valid: true,
        evidence: Vec::new(),
    }
}

fn remember_request(
    scope: &MemoryScope,
    key: &str,
    now_ms: u64,
    window_ms: u64,
    items: Vec<MemoryItem>,
) -> RememberRequest {
    RememberRequest {
        scope: scope.clone(),
        idempotency_key: key.into(),
        receipt_window_ms: window_ms,
        now_ms,
        deadline_ms: None,
        allow_future_observations: false,
        items,
    }
}

/// One memory `id` remembered under `key` at `now_ms` with a 100 ms window.
fn remember(
    db: &mut Database,
    scope: &MemoryScope,
    key: &str,
    id: &str,
    now_ms: u64,
) -> RememberResult {
    let request = remember_request(scope, key, now_ms, 100, vec![item(id, id, now_ms)]);
    MemoryMut::bind(db, GRAPH, None)
        .unwrap()
        .remember(&request)
        .unwrap()
}

fn forget_request(
    scope: &MemoryScope,
    key: &str,
    now_ms: u64,
    window_ms: u64,
    id: &str,
) -> ForgetRequest {
    ForgetRequest {
        scope: scope.clone(),
        idempotency_key: key.into(),
        receipt_window_ms: window_ms,
        now_ms,
        deadline_ms: None,
        targets: vec![ForgetTarget {
            id: id.into(),
            expected_revision: 1,
        }],
    }
}

fn consolidate_request(
    scope: &MemoryScope,
    key: &str,
    now_ms: u64,
    window_ms: u64,
    input: &str,
    summary: &str,
) -> ConsolidateRequest {
    ConsolidateRequest {
        scope: scope.clone(),
        idempotency_key: key.into(),
        now_ms,
        clock_valid: true,
        receipt_window_ms: window_ms,
        deadline_ms: None,
        input_budget: None,
        inputs: vec![ConsolidateInput {
            id: input.into(),
            expected_memory_revision: Some(1),
            retain: true,
        }],
        summary: ConsolidateSummary {
            id: summary.into(),
            content: format!("summary of {input}"),
            source: "source".into(),
            observed_at_ms: Some(now_ms as i64),
            importance: None,
            expiry_ms: None,
            expiry_valid: true,
            provenance: Vec::new(),
        },
        transformation: "summarize".into(),
        evidence: None,
        allow_future_observations: false,
    }
}

fn sha_hex(value: &str) -> String {
    hex(&Sha256::digest(value.as_bytes()))
}

/// An arbitrary well-formed scope key that names no provisioned scope.
fn loose_scope(name: &str) -> String {
    format!("scope:v1:{}", sha_hex(name))
}

#[derive(Clone, Copy)]
enum Shape {
    /// The remember and forget receipt with `n` outcome chunks.
    A2(usize),
    /// The consolidate receipt (one outcome chunk).
    C1,
}

fn string(value: &str) -> MetadataValue {
    MetadataValue::String(value.into())
}

/// The create operations of a group as the 0.7.x encoders write it: the
/// receipt first, then its chunks in index order.
fn legacy_ops(scope_key: &str, name: &str, expires_at_ms: u64, shape: Shape) -> Vec<GraphOp> {
    let key = format!("receipt:v1:{scope_key}:{}", sha_hex(name));
    let mut properties = Metadata::new();
    properties.insert("scope".into(), string(scope_key));
    properties.insert("digest".into(), string(&format!("v1:{}", "d".repeat(64))));
    properties.insert("expires_at_ms".into(), string(&expires_at_ms.to_string()));
    let chunks = match shape {
        Shape::A2(chunks) => chunks,
        Shape::C1 => {
            properties.insert("c1_scope_tuple".into(), string("00"));
            properties.insert("c1_schema".into(), string(super::SCHEMA));
            properties.insert("c1_collection".into(), string(""));
            1
        }
    };
    properties.insert(
        "outcome_chunks".into(),
        MetadataValue::Integer(chunks as i64),
    );
    let mut ops = vec![GraphOp::CreateNode {
        key: key.clone(),
        labels: vec![LABEL_RECEIPT.into()],
        properties,
    }];
    for index in 0..chunks {
        ops.push(chunk_op(&key, index));
    }
    ops
}

fn chunk_op(receipt_key: &str, index: usize) -> GraphOp {
    let mut properties = Metadata::new();
    properties.insert("receipt_key".into(), string(receipt_key));
    properties.insert("index".into(), MetadataValue::Integer(index as i64));
    properties.insert("outcome".into(), string("outcome"));
    GraphOp::CreateNode {
        key: receipt_outcome_key(receipt_key, index),
        labels: vec![LABEL_RECEIPT_OUTCOME.into()],
        properties,
    }
}

fn created_key(op: &GraphOp) -> String {
    match op {
        GraphOp::CreateNode { key, .. } => key.clone(),
        _ => unreachable!("legacy groups only create nodes"),
    }
}

fn properties(op: &mut GraphOp) -> &mut Metadata {
    match op {
        GraphOp::CreateNode { properties, .. } => properties,
        _ => unreachable!("legacy groups only create nodes"),
    }
}

fn labels(op: &mut GraphOp) -> &mut Vec<String> {
    match op {
        GraphOp::CreateNode { labels, .. } => labels,
        _ => unreachable!("legacy groups only create nodes"),
    }
}

fn apply(db: &mut Database, ops: &[GraphOp]) {
    db.graph_mut(GRAPH).unwrap().write_batch(ops).unwrap();
}

/// Writes a legacy group and returns its keys in delete order.
fn legacy_group(
    db: &mut Database,
    scope_key: &str,
    name: &str,
    expires_at_ms: u64,
    shape: Shape,
) -> Vec<String> {
    let ops = legacy_ops(scope_key, name, expires_at_ms, shape);
    apply(db, &ops);
    ops.iter().map(created_key).collect()
}

fn revision(db: &Database, key: &str) -> Option<u64> {
    db.graph(GRAPH)
        .unwrap()
        .get_node(key)
        .unwrap()
        .output
        .map(|node| node.revision)
}

fn node_id(db: &Database, key: &str) -> u64 {
    db.graph(GRAPH)
        .unwrap()
        .get_node(key)
        .unwrap()
        .output
        .unwrap()
        .id
}

fn present(db: &Database, keys: &[String]) -> bool {
    keys.iter().all(|key| revision(db, key).is_some())
}

fn absent(db: &Database, keys: &[String]) -> bool {
    keys.iter().all(|key| revision(db, key).is_none())
}

/// Every node of the graph by key, with its revision.
fn snapshot(db: &Database) -> BTreeMap<String, u64> {
    let graph = db.graph(GRAPH).unwrap();
    let mut nodes = BTreeMap::new();
    let mut cursor = None;
    loop {
        let page = graph
            .find_nodes(
                &FindNodes {
                    limit: Some(1_000),
                    cursor,
                    ..FindNodes::default()
                },
                &ReadOptions {
                    limits: GraphLimits::CEILING,
                    ..ReadOptions::default()
                },
            )
            .unwrap();
        for node in page.output {
            nodes.insert(node.key, node.revision);
        }
        match page.cursor {
            Some(next) => cursor = Some(next),
            None => {
                assert!(page.complete);
                return nodes;
            }
        }
    }
}

fn plan_at(
    db: &Database,
    own: &str,
    now_ms: u64,
    headroom: usize,
    seed: u64,
    limits: Limits,
) -> Vec<String> {
    plan_with(
        &db.graph(GRAPH).unwrap().with_clock(now_ms),
        own,
        now_ms,
        headroom,
        seed,
        limits,
    )
    .into_iter()
    .map(|op| match op {
        GraphOp::DeleteNode {
            key,
            expected: Expected::Revision(_),
        } => key,
        other => panic!("reclamation planned {other:?}"),
    })
    .collect()
}

fn plan_wide(db: &Database, now_ms: u64) -> Vec<String> {
    plan_at(db, "", now_ms, MAX_OPS, 0, WIDE)
}

fn concat(groups: &[&Vec<String>]) -> Vec<String> {
    groups
        .iter()
        .flat_map(|group| group.iter().cloned())
        .collect()
}

fn erase(db: &mut Database, scope: &MemoryScope, now_ms: u64) {
    let expected_scope_revision = revision(db, &scope.key()).unwrap();
    MemoryMut::bind(db, GRAPH, None)
        .unwrap()
        .begin_erasure(&EraseScopeRequest {
            scope: scope.clone(),
            idempotency_key: "erase".into(),
            expected_scope_revision,
            now_ms,
            receipt_window_ms: 100,
            deadline_ms: None,
        })
        .unwrap();
}

#[test]
fn the_constants_are_the_specified_bounds() {
    assert_eq!(RECEIPT_RECLAIM_GRACE_MS, 300_000);
    assert_eq!(RECLAIM_SCAN, 512);
    assert_eq!(RECLAIM_SEED_MULTIPLIER, 0x9E37_79B9_7F4A_7C15);
    assert_eq!(RECLAIM_MAX_OPS, 1_024);
    assert_eq!(RECLAIM_MAX_READS, 16_384);
    let limits = Limits::DEFAULT;
    assert_eq!(
        (limits.scan, limits.max_ops, limits.max_reads),
        (512, 1_024, 16_384)
    );
}

#[test]
fn an_expired_receipt_is_kept_until_the_grace_ends() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(dir.path());
    let writer = scope("writer");
    provision(&mut db, &writer);
    let group = legacy_group(&mut db, &loose_scope("a"), "g", 110, Shape::A2(1));
    assert!(plan_wide(&db, ELIGIBLE - 1).is_empty());
    assert_eq!(plan_wide(&db, ELIGIBLE), group);

    remember(&mut db, &writer, "early", "m1", ELIGIBLE - 1);
    assert!(present(&db, &group), "kept at 300,109");
    remember(&mut db, &writer, "late", "m2", ELIGIBLE);
    assert!(absent(&db, &group), "deleted at 300,110");

    // The grace sum overflows: never eligible.
    let edge = legacy_group(
        &mut db,
        &loose_scope("b"),
        "overflow",
        u64::MAX - RECEIPT_RECLAIM_GRACE_MS + 1,
        Shape::A2(1),
    );
    let overflow = plan_wide(&db, u64::MAX);
    assert!(edge.iter().all(|key| !overflow.contains(key)));
    let last = legacy_group(
        &mut db,
        &loose_scope("b"),
        "last",
        u64::MAX - RECEIPT_RECLAIM_GRACE_MS,
        Shape::A2(1),
    );
    let planned = plan_wide(&db, u64::MAX);
    assert!(last.iter().all(|key| planned.contains(key)));
    assert!(edge.iter().all(|key| !planned.contains(key)));
    assert!(present(&db, &edge));
}

#[test]
fn the_writing_requests_own_receipt_is_never_reclaimed() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(dir.path());
    let writer = scope("writer");
    provision(&mut db, &writer);
    let first = remember(&mut db, &writer, "k", "m", 10);
    let own = receipt_key(&writer.key(), "k");
    let own_id = node_id(&db, &own);
    let own_group = vec![own.clone(), receipt_outcome_key(&own, 0)];
    assert_eq!(plan_wide(&db, ELIGIBLE), own_group);
    assert!(plan_at(&db, &own, ELIGIBLE, MAX_OPS, 0, WIDE).is_empty());

    // An own-key retry after expiry writes a fresh result in place, as before.
    let retry = remember(&mut db, &writer, "k", "m", ELIGIBLE);
    assert!(!retry.replayed);
    assert!(retry.receipt_revision > first.receipt_revision);
    let receipt = db
        .graph(GRAPH)
        .unwrap()
        .get_node(&own)
        .unwrap()
        .output
        .unwrap();
    assert_eq!(receipt.id, own_id);
    assert_eq!(receipt.revision, retry.receipt_revision);
    assert_eq!(
        receipt.properties.get("expires_at_ms"),
        Some(&string(&(ELIGIBLE + 100).to_string()))
    );
    assert!(present(&db, &own_group));
}

#[test]
fn live_erasure_and_unrelated_nodes_are_never_touched() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(dir.path());
    let writer = scope("writer");
    let erased = scope("erased");
    provision(&mut db, &writer);
    provision(&mut db, &erased);
    // A live receipt: expires at 300,150, eligible from 600,150.
    remember(&mut db, &writer, "live", "m", 300_050);
    // An E1 receipt and its job, expired long ago.
    erase(&mut db, &erased, 10);
    let eligible = legacy_group(&mut db, &loose_scope("a"), "g", 110, Shape::A2(1));
    let before = snapshot(&db);
    assert_eq!(plan_wide(&db, ELIGIBLE), eligible);

    remember(&mut db, &writer, "next", "m2", ELIGIBLE);
    let after = snapshot(&db);
    let own = receipt_key(&writer.key(), "next");
    for (key, revision) in &before {
        if eligible.contains(key) {
            assert!(!after.contains_key(key), "{key} was reclaimed");
        } else {
            assert_eq!(after.get(key), Some(revision), "{key} was touched");
        }
    }
    assert!(after.contains_key(&own));
}

#[test]
fn a_scope_behind_an_erasure_barrier_keeps_its_receipts() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(dir.path());
    let barred = scope("barred");
    let open = scope("open");
    let writer = scope("writer");
    for scope in [&barred, &open, &writer] {
        provision(&mut db, scope);
    }
    let kept = legacy_group(&mut db, &barred.key(), "kept", 110, Shape::A2(1));
    let kept_too = legacy_group(&mut db, &barred.key(), "kept-too", 110, Shape::C1);
    let gone = legacy_group(&mut db, &open.key(), "gone", 110, Shape::A2(1));
    erase(&mut db, &barred, 200);
    assert_eq!(plan_wide(&db, ELIGIBLE), gone);
    remember(&mut db, &writer, "k", "m", ELIGIBLE);
    assert!(absent(&db, &gone));
    assert!(present(&db, &kept) && present(&db, &kept_too));
}

fn attach(ops: &mut Vec<GraphOp>, node: String, record: u64) {
    ops.push(GraphOp::Vector {
        collection: "default".into(),
        mutation: crate::Mutation::Put {
            id: record,
            vector: vec![1.0, 0.0],
            payload: Vec::new(),
        },
    });
    ops.push(GraphOp::Attach {
        node,
        collection: "default".into(),
        record,
    });
}

fn expire(ops: &mut Vec<GraphOp>, index: usize) {
    let key = created_key(&ops[index]);
    ops.push(GraphOp::SetNodeExpiry {
        key,
        expires_at_ms: Some(u64::MAX / 2),
        expected: Expected::Any,
    });
}

fn self_edge(ops: &mut Vec<GraphOp>, source: usize, destination: usize) {
    let source = created_key(&ops[source]);
    let destination = created_key(&ops[destination]);
    ops.push(GraphOp::CreateEdge {
        key: None,
        source,
        relationship: "PIN".into(),
        destination,
        properties: Metadata::new(),
    });
}

/// Each case breaks exactly one rule of an otherwise eligible group.
#[test]
fn malformed_partial_or_pinned_groups_are_skipped() {
    type Edit = fn(&mut Vec<GraphOp>);
    let cases: [(&str, Shape, Edit); 32] = [
        ("receipt graph expiry", Shape::A2(1), |ops| expire(ops, 0)),
        ("extra label", Shape::A2(1), |ops| {
            labels(&mut ops[0]).push("Extra".into())
        }),
        ("foreign label", Shape::A2(1), |ops| {
            *labels(&mut ops[0]) = vec!["VliteMemoryReceiptX".into()]
        }),
        ("extra property", Shape::A2(1), |ops| {
            properties(&mut ops[0]).insert("extra".into(), string("x"));
        }),
        ("missing property", Shape::A2(1), |ops| {
            properties(&mut ops[0]).remove("digest");
        }),
        ("chunk count as string", Shape::A2(1), |ops| {
            properties(&mut ops[0]).insert("outcome_chunks".into(), string("1"));
        }),
        ("expiry as integer", Shape::A2(1), |ops| {
            properties(&mut ops[0]).insert("expires_at_ms".into(), MetadataValue::Integer(110));
        }),
        ("c1 schema", Shape::C1, |ops| {
            properties(&mut ops[0]).insert("c1_schema".into(), string("consolidate:v2"));
        }),
        ("c1 two chunks", Shape::C1, |ops| {
            properties(&mut ops[0]).insert("outcome_chunks".into(), MetadataValue::Integer(2));
            let key = created_key(&ops[0]);
            ops.push(chunk_op(&key, 1));
        }),
        ("e1 shape", Shape::A2(1), |ops| {
            let receipt = properties(&mut ops[0]);
            for name in ["e1_schema", "e1_scope_tuple", "e1_collection", "job_key"] {
                receipt.insert(name.into(), string("x"));
            }
        }),
        ("uppercase scope", Shape::A2(1), |ops| {
            let scope = format!("scope:v1:{}", "A".repeat(64));
            properties(&mut ops[0]).insert("scope".into(), string(&scope));
        }),
        ("short scope", Shape::A2(1), |ops| {
            let scope = format!("scope:v1:{}", "a".repeat(63));
            properties(&mut ops[0]).insert("scope".into(), string(&scope));
        }),
        ("scope of another key", Shape::A2(1), |ops| {
            properties(&mut ops[0]).insert("scope".into(), string(&loose_scope("other")));
        }),
        ("bad digest", Shape::A2(1), |ops| {
            let digest = format!("v2:{}", "d".repeat(64));
            properties(&mut ops[0]).insert("digest".into(), string(&digest));
        }),
        ("leading zero expiry", Shape::A2(1), |ops| {
            properties(&mut ops[0]).insert("expires_at_ms".into(), string("0110"));
        }),
        ("signed expiry", Shape::A2(1), |ops| {
            properties(&mut ops[0]).insert("expires_at_ms".into(), string("+110"));
        }),
        ("zero expiry", Shape::A2(1), |ops| {
            properties(&mut ops[0]).insert("expires_at_ms".into(), string("0"));
        }),
        ("expiry above u64", Shape::A2(1), |ops| {
            let value = string("18446744073709551616");
            properties(&mut ops[0]).insert("expires_at_ms".into(), value);
        }),
        ("zero chunks", Shape::A2(1), |ops| {
            properties(&mut ops[0]).insert("outcome_chunks".into(), MetadataValue::Integer(0));
        }),
        ("negative chunks", Shape::A2(1), |ops| {
            properties(&mut ops[0]).insert("outcome_chunks".into(), MetadataValue::Integer(-1));
        }),
        ("4,097 chunks", Shape::A2(1), |ops| {
            let value = MetadataValue::Integer(4_097);
            properties(&mut ops[0]).insert("outcome_chunks".into(), value);
        }),
        ("missing chunk", Shape::A2(2), |ops| {
            ops.pop();
        }),
        ("missing first chunk", Shape::A2(2), |ops| {
            ops.remove(1);
        }),
        ("chunk of another receipt", Shape::A2(1), |ops| {
            properties(&mut ops[1]).insert("receipt_key".into(), string("receipt:v1:other"));
        }),
        ("chunk index", Shape::A2(2), |ops| {
            properties(&mut ops[2]).insert("index".into(), MetadataValue::Integer(0));
        }),
        ("chunk extra property", Shape::A2(1), |ops| {
            properties(&mut ops[1]).insert("extra".into(), string("x"));
        }),
        ("chunk label", Shape::A2(1), |ops| {
            labels(&mut ops[1]).push("Extra".into())
        }),
        ("chunk graph expiry", Shape::A2(1), |ops| expire(ops, 1)),
        ("edge on the receipt", Shape::A2(1), |ops| {
            self_edge(ops, 0, 0)
        }),
        ("edge into a chunk", Shape::A2(1), |ops| {
            self_edge(ops, 0, 1)
        }),
        ("attached receipt", Shape::A2(1), |ops| {
            let key = created_key(&ops[0]);
            attach(ops, key, 1);
        }),
        ("attached chunk", Shape::A2(1), |ops| {
            let key = created_key(&ops[1]);
            attach(ops, key, 2);
        }),
    ];

    let dir = tempfile::tempdir().unwrap();
    let mut db = database(dir.path());
    let mut skipped = Vec::new();
    for (name, shape, edit) in cases {
        let mut ops = legacy_ops(&loose_scope(name), name, 110, shape);
        edit(&mut ops);
        apply(&mut db, &ops);
        skipped.push((name, created_key(&ops[0])));
    }
    let control = legacy_group(&mut db, &loose_scope("control"), "control", 110, Shape::C1);
    let before = snapshot(&db);
    assert_eq!(plan_wide(&db, u64::MAX / 4), control);
    for seed in [0, 1, 7, 40, 80] {
        let planned = plan_at(&db, "", u64::MAX / 4, MAX_OPS, seed, WIDE);
        for (name, key) in &skipped {
            assert!(!planned.contains(key), "{name} was planned");
        }
    }

    let writer = scope("writer");
    provision(&mut db, &writer);
    let before_writer = snapshot(&db);
    remember(&mut db, &writer, "k", "m", u64::MAX / 4);
    let after = snapshot(&db);
    for (key, revision) in &before {
        if control.contains(key) {
            assert!(!after.contains_key(key));
        } else {
            assert_eq!(after.get(key), Some(revision), "{key} was touched");
        }
    }
    assert!(before_writer.len() > before.len());
}

#[test]
fn surplus_chunks_go_with_their_receipt_and_a_bad_surplus_skips_the_group() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(dir.path());
    // N = 1 with surplus chunks 1 and 2.
    let mut ops = legacy_ops(&loose_scope("a"), "surplus", 110, Shape::A2(1));
    let receipt = created_key(&ops[0]);
    ops.extend([chunk_op(&receipt, 1), chunk_op(&receipt, 2)]);
    apply(&mut db, &ops);
    let surplus: Vec<String> = ops.iter().map(created_key).collect();
    // N = 1 with surplus chunk 1, a gap at 2 and chunk 3: the probe stops at 2.
    let mut ops = legacy_ops(&loose_scope("b"), "gap", 110, Shape::A2(1));
    let receipt = created_key(&ops[0]);
    ops.extend([chunk_op(&receipt, 1), chunk_op(&receipt, 3)]);
    apply(&mut db, &ops);
    let gap: Vec<String> = ops[..3].iter().map(created_key).collect();
    let stranded = created_key(&ops[3]);
    // N = 1 with a surplus chunk claiming the wrong index.
    let mut ops = legacy_ops(&loose_scope("c"), "bad", 110, Shape::A2(1));
    let receipt = created_key(&ops[0]);
    let mut bad = chunk_op(&receipt, 1);
    properties(&mut bad).insert("index".into(), MetadataValue::Integer(9));
    ops.push(bad);
    apply(&mut db, &ops);
    let bad: Vec<String> = ops.iter().map(created_key).collect();

    assert_eq!(plan_wide(&db, ELIGIBLE), concat(&[&surplus, &gap]));
    // The surplus counts against the cap after reading: four nodes do not fit
    // in a first group of three, so the next group is planned instead.
    assert_eq!(plan_at(&db, "", ELIGIBLE, 3, 0, WIDE), gap);

    let writer = scope("writer");
    provision(&mut db, &writer);
    remember(&mut db, &writer, "k", "m", ELIGIBLE);
    assert!(absent(&db, &surplus) && absent(&db, &gap));
    assert!(present(&db, &bad));
    assert!(revision(&db, &stranded).is_some());
}

#[test]
fn budgets_bound_each_write_and_one_large_first_group_fits_the_headroom() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(dir.path());
    let large = legacy_group(&mut db, &loose_scope("a"), "large", 110, Shape::A2(7));
    let small: Vec<Vec<String>> = (0..10)
        .map(|index| {
            legacy_group(
                &mut db,
                &loose_scope("b"),
                &format!("small-{index}"),
                110,
                Shape::A2(1),
            )
        })
        .collect();
    let limits = Limits {
        scan: RECLAIM_SCAN,
        max_ops: 6,
        max_reads: usize::MAX,
    };
    let plan = |headroom: usize, limits: Limits| plan_at(&db, "", ELIGIBLE, headroom, 0, limits);

    // Headroom 0 plans nothing.
    assert!(plan(0, limits).is_empty());
    // The large first group (8 nodes) fits a headroom of 8 and stops the rest.
    assert_eq!(plan(8, limits), large);
    assert_eq!(plan(100, limits), large);
    // With less headroom it is skipped; later groups stop at min(headroom, 6).
    assert_eq!(plan(7, limits), concat(&[&small[0], &small[1], &small[2]]));
    assert_eq!(plan(5, limits), concat(&[&small[0], &small[1]]));
    assert!(plan(1, limits).is_empty());
    // No more than `scan` members are examined.
    let narrow = Limits { scan: 3, ..WIDE };
    assert_eq!(
        plan(MAX_OPS, narrow),
        concat(&[&large, &small[0], &small[1]])
    );
    // With the default bounds, every group fits.
    let all: Vec<String> = std::iter::once(&large)
        .chain(small.iter())
        .flatten()
        .cloned()
        .collect();
    assert_eq!(plan(MAX_OPS, Limits::DEFAULT), all);
}

#[test]
fn the_read_budget_drops_a_partial_group() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(dir.path());
    let groups: Vec<Vec<String>> = (0..3)
        .map(|index| {
            legacy_group(
                &mut db,
                &loose_scope("a"),
                &format!("g{index}"),
                110,
                Shape::A2(1),
            )
        })
        .collect();
    // The first group costs 7 reads (barrier, receipt edges and attachments,
    // chunk 0, its edges and attachments, the chunk 1 probe); later groups of
    // the same scope cost 6 (the barrier is cached).
    let reads = |max_reads| plan_at(&db, "", ELIGIBLE, MAX_OPS, 0, Limits { max_reads, ..WIDE });
    assert!(reads(0).is_empty());
    assert!(reads(6).is_empty());
    assert_eq!(reads(7), groups[0]);
    assert_eq!(reads(12), groups[0]);
    assert_eq!(reads(13), concat(&[&groups[0], &groups[1]]));
    assert_eq!(reads(18), concat(&[&groups[0], &groups[1]]));
    assert_eq!(reads(19), concat(&[&groups[0], &groups[1], &groups[2]]));
}

type ErrorFn = fn() -> Error;

fn fallback_errors() -> [(&'static str, ErrorFn); 4] {
    [
        ("BatchTooLarge", || Error::BatchTooLarge),
        ("GraphNodeNotFound", || Error::GraphNodeNotFound),
        ("GraphNodeHasEdges", || Error::GraphNodeHasEdges {
            incident: 1,
        }),
        ("RevisionMismatch", || Error::RevisionMismatch {
            current: Some(1),
        }),
    ]
}

#[test]
fn a_failed_combined_attempt_falls_back_to_the_own_operations() {
    for (name, error) in fallback_errors() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(dir.path());
        let writer = scope("writer");
        provision(&mut db, &writer);
        let group = legacy_group(&mut db, &loose_scope("a"), "g", 110, Shape::A2(1));
        let before = db.sequence;
        FAIL_RECLAIM_ATTEMPT.with(|fail| fail.set(Some(error)));
        let result = remember(&mut db, &writer, "k", "m", ELIGIBLE);
        assert!(FAIL_RECLAIM_ATTEMPT.with(|fail| fail.get().is_none()));
        assert!(!result.replayed, "{name}");
        assert_eq!(db.sequence, before + 1, "{name}: one commit");
        assert_eq!(result.commit_sequence, db.sequence, "{name}");
        assert!(present(&db, &group), "{name}: the targets stay");
        let own = receipt_key(&writer.key(), "k");
        assert_eq!(revision(&db, &own), Some(result.receipt_revision), "{name}");
    }

    // Any other error is returned unchanged.
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(dir.path());
    let writer = scope("writer");
    provision(&mut db, &writer);
    let group = legacy_group(&mut db, &loose_scope("a"), "g", 110, Shape::A2(1));
    let before = db.sequence;
    let state = snapshot(&db);
    FAIL_RECLAIM_ATTEMPT.with(|fail| fail.set(Some(|| Error::InvalidQuery("injected"))));
    let request = remember_request(&writer, "k", ELIGIBLE, 100, vec![item("m", "m", ELIGIBLE)]);
    let result = MemoryMut::bind(&mut db, GRAPH, None)
        .unwrap()
        .remember(&request);
    assert!(
        matches!(
            &result,
            Err(MemoryError::Storage(Error::InvalidQuery("injected")))
        ),
        "{result:?}"
    );
    assert_eq!(db.sequence, before);
    assert_eq!(snapshot(&db), state);
    assert!(present(&db, &group));
}

/// The engine's own pre-frame refusals take the same single fallback.
#[test]
fn write_falls_back_once_on_real_engine_refusals() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(dir.path());
    let mut pinned = legacy_ops(&loose_scope("a"), "pinned", 110, Shape::A2(1));
    self_edge(&mut pinned, 0, 0);
    apply(&mut db, &pinned);
    let pinned_key = created_key(&pinned[0]);
    let pinned_revision = revision(&db, &pinned_key).unwrap();
    let delete = |key: &str, revision: u64| GraphOp::DeleteNode {
        key: key.into(),
        expected: Expected::Revision(revision),
    };
    let reclaims: [(&str, Vec<GraphOp>); 4] = [
        ("missing node", vec![delete("missing", 1)]),
        (
            "node with edges",
            vec![delete(&pinned_key, pinned_revision)],
        ),
        (
            "stale revision",
            vec![delete(&pinned_key, pinned_revision + 1)],
        ),
        (
            "too many entries",
            (0..4_096)
                .map(|index| delete(&format!("x{index}"), 1))
                .collect(),
        ),
    ];
    let own_node = |key: &str| GraphOp::CreateNode {
        key: key.into(),
        labels: vec!["Own".into()],
        properties: Metadata::new(),
    };
    for (index, (name, reclaim)) in reclaims.into_iter().enumerate() {
        let own_key = format!("own-{index}");
        let before = db.sequence;
        let batch = write(&mut db, GRAPH, reclaim, vec![own_node(&own_key)], 1_000).unwrap();
        assert_eq!(batch.outcomes.len(), 1, "{name}: the own operations alone");
        assert_eq!(db.sequence, before + 1, "{name}");
        assert!(revision(&db, &own_key).is_some(), "{name}");
        assert_eq!(revision(&db, &pinned_key), Some(pinned_revision), "{name}");
    }
    // An own failure after the fallback is returned as the own error.
    let before = db.sequence;
    let error = write(
        &mut db,
        GRAPH,
        vec![delete("missing", 1)],
        vec![own_node("own-0")],
        1_000,
    )
    .unwrap_err();
    assert!(matches!(error, Error::GraphKeyExists), "{error:?}");
    assert_eq!(db.sequence, before);
    // An empty reclaim list is the plain write: no combined attempt.
    FAIL_RECLAIM_ATTEMPT.with(|fail| fail.set(Some(|| Error::BatchTooLarge)));
    write(
        &mut db,
        GRAPH,
        Vec::new(),
        vec![own_node("own-plain")],
        1_000,
    )
    .unwrap();
    assert!(
        FAIL_RECLAIM_ATTEMPT.with(|fail| fail.take().is_some()),
        "no combined attempt was made"
    );
    assert!(revision(&db, "own-plain").is_some());
}

#[test]
fn a_failing_own_batch_reclaims_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(dir.path());
    let writer = scope("writer");
    provision(&mut db, &writer);
    let group = legacy_group(&mut db, &loose_scope("a"), "g", 110, Shape::A2(1));
    let state = snapshot(&db);
    let before = db.sequence;
    FAIL_NEXT_WRITE_BATCH.with(|fail| fail.set(true));
    let request = remember_request(&writer, "k", ELIGIBLE, 100, vec![item("m", "m", ELIGIBLE)]);
    let result = MemoryMut::bind(&mut db, GRAPH, None)
        .unwrap()
        .remember(&request);
    assert!(result.is_err());
    assert_eq!(db.sequence, before);
    assert_eq!(snapshot(&db), state);
    assert!(present(&db, &group));
}

#[test]
fn reclamation_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(dir.path());
    let writer = scope("writer");
    provision(&mut db, &writer);
    let group = legacy_group(&mut db, &loose_scope("a"), "g", 110, Shape::A2(2));
    remember(&mut db, &writer, "k", "m", ELIGIBLE);
    assert!(absent(&db, &group));
    let state = snapshot(&db);
    let sequence = db.sequence;
    drop(db);
    let db = Database::open(dir.path().join("reclaim.vlite")).unwrap();
    assert_eq!(db.sequence, sequence);
    assert_eq!(snapshot(&db), state);
}

#[test]
fn a_backlog_drains_in_bounded_writes() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(dir.path());
    let writer = scope("writer");
    provision(&mut db, &writer);
    let mut groups = Vec::new();
    for batch in 0..3 {
        let mut ops = Vec::new();
        for index in 0..500 {
            ops.extend(legacy_ops(
                &loose_scope("a"),
                &format!("g{batch}-{index}"),
                110,
                Shape::A2(1),
            ));
        }
        apply(&mut db, &ops);
        groups.extend(
            ops.chunks(2)
                .map(|group| group.iter().map(created_key).collect::<Vec<_>>()),
        );
    }
    let mut by_id: Vec<(u64, &Vec<String>)> = groups
        .iter()
        .map(|group| (node_id(&db, &group[0]), group))
        .collect();
    by_id.sort();
    let lowest: Vec<String> = by_id[..512]
        .iter()
        .flat_map(|(_, group)| group.iter().cloned())
        .collect();
    assert_eq!(
        plan_at(&db, "", ELIGIBLE, 4_090, 0, Limits::DEFAULT),
        lowest
    );

    let remaining = |db: &Database| {
        groups
            .iter()
            .flatten()
            .filter(|key| revision(db, key).is_some())
            .count()
    };
    let mut left = remaining(&db);
    assert_eq!(left, 3_000);
    for write in 0..3 {
        remember(&mut db, &writer, &format!("k{write}"), "m", ELIGIBLE);
        let now = remaining(&db);
        assert!(
            left - now <= RECLAIM_MAX_OPS,
            "write {write} deleted {}",
            left - now
        );
        left = now;
    }
    assert_eq!(left, 0);
}

#[test]
fn the_window_starts_at_the_seed_wraps_once_and_counts_hidden_members() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(dir.path());
    let ops: Vec<GraphOp> = (0..600)
        .map(|index| GraphOp::CreateNode {
            key: format!("r{index}"),
            labels: vec![LABEL_RECEIPT.into()],
            properties: Metadata::new(),
        })
        .collect();
    apply(&mut db, &ops);
    apply(
        &mut db,
        &[GraphOp::CreateNode {
            key: "other".into(),
            labels: vec!["Other".into()],
            properties: Metadata::new(),
        }],
    );
    let r: Vec<u64> = (0..600)
        .map(|index| node_id(&db, &format!("r{index}")))
        .collect();
    assert!(r.windows(2).all(|pair| pair[0] < pair[1]));
    let next = node_id(&db, "other") + 1;
    let window = |db: &Database, seed: u64, limit: usize| {
        db.graph(GRAPH)
            .unwrap()
            .memory_label_window(LABEL_RECEIPT, seed, limit)
    };
    assert_eq!(window(&db, 0, 512), r[..512]);
    let wrapped: Vec<u64> = r[500..].iter().chain(&r[..412]).copied().collect();
    assert_eq!(window(&db, r[500], 512), wrapped);
    assert_eq!(window(&db, r[500] + next, 512), wrapped);
    assert!(window(&db, 5, 0).is_empty());
    for seed in 0..next + 2 {
        let ids = window(&db, seed, 512);
        assert_eq!(ids.len(), 512, "seed {seed}");
        assert_eq!(
            ids.iter().collect::<BTreeSet<_>>().len(),
            512,
            "seed {seed}"
        );
        let mut all = window(&db, seed, 10_000);
        assert_eq!(all.len(), 600, "seed {seed}: each member exactly once");
        all.sort_unstable();
        assert_eq!(all, r, "seed {seed}");
    }

    // r0..r9 get a graph deadline; hidden at that clock, they still count.
    let deadline = crate::current_time_ms() + 3_600_000;
    let expiries: Vec<GraphOp> = (0..10)
        .map(|index| GraphOp::SetNodeExpiry {
            key: format!("r{index}"),
            expires_at_ms: Some(deadline),
            expected: Expected::Any,
        })
        .collect();
    apply(&mut db, &expiries);
    let hidden = db.graph(GRAPH).unwrap().with_clock(deadline);
    assert!(hidden.get_node_by_id(r[0]).unwrap().output.is_none());
    assert_eq!(hidden.memory_label_window(LABEL_RECEIPT, 0, 512), r[..512]);
}

#[test]
fn receipts_reused_in_place_cannot_hide_eligible_groups() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(dir.path());
    let writer = scope("writer");
    provision(&mut db, &writer);
    let mut ids = Vec::new();
    for index in 0..512 {
        let key = format!("k{index}");
        let request = remember_request(&writer, &key, 1_000, 60_000, vec![item("m", "m", 1_000)]);
        MemoryMut::bind(&mut db, GRAPH, None)
            .unwrap()
            .remember(&request)
            .unwrap();
        ids.push(node_id(&db, &receipt_key(&writer.key(), &key)));
    }
    for (index, id) in ids.iter().enumerate() {
        let key = format!("k{index}");
        let request = remember_request(&writer, &key, 61_001, 60_000, vec![item("m", "m", 1_000)]);
        let result = MemoryMut::bind(&mut db, GRAPH, None)
            .unwrap()
            .remember(&request)
            .unwrap();
        assert!(!result.replayed);
        let receipt = db
            .graph(GRAPH)
            .unwrap()
            .get_node(&receipt_key(&writer.key(), &key))
            .unwrap()
            .output
            .unwrap();
        assert_eq!(receipt.id, *id, "reused in place");
        assert_eq!(
            receipt.properties.get("expires_at_ms"),
            Some(&string("121001"))
        );
    }
    let group = legacy_group(&mut db, &loose_scope("a"), "behind", 110, Shape::A2(1));
    let eligible_id = node_id(&db, &group[0]);
    assert!(ids.iter().all(|id| *id < eligible_id));
    let next = node_id(&db, &group[1]) + 1;
    let planned = |seed| plan_at(&db, "", ELIGIBLE, 4_090, seed, Limits::DEFAULT);
    assert!(planned(0).is_empty());
    assert_eq!(planned(eligible_id), group);
    for seed in 0..next {
        let keys = planned(seed);
        assert!(keys.is_empty() || keys == group, "seed {seed}: {keys:?}");
    }
}

#[test]
fn reclaim_deletes_are_ordinary_graph_deletes_in_the_feed() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(dir.path());
    let writer = scope("writer");
    provision(&mut db, &writer);
    let first = legacy_group(&mut db, &loose_scope("a"), "g1", 110, Shape::A2(2));
    let second = legacy_group(&mut db, &loose_scope("b"), "g2", 110, Shape::C1);
    let result = remember(&mut db, &writer, "k", "m", ELIGIBLE);
    let changes = db.changes_after(&FeedCursor::beginning(), 10_000).unwrap();
    let commit: Vec<_> = changes
        .changes
        .iter()
        .filter(|change| change.sequence == result.commit_sequence)
        .collect();
    assert_eq!(commit.len(), 1, "one commit, one graph batch");
    let ChangeKind::GraphBatch { operations, .. } = &commit[0].kind else {
        panic!("not a graph batch: {:?}", commit[0].kind);
    };
    let reclaimed: BTreeSet<String> = concat(&[&first, &second]).into_iter().collect();
    let deleted: Vec<String> = operations
        .iter()
        .map_while(|change| match change {
            GraphChange::NodeDeleted { key, .. } => Some(key.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(deleted.iter().cloned().collect::<BTreeSet<_>>(), reclaimed);
    assert_eq!(deleted.len(), reclaimed.len());
    let rest = &operations[deleted.len()..];
    assert!(!rest.is_empty());
    assert!(
        rest.iter()
            .all(|change| !matches!(change, GraphChange::NodeDeleted { .. }))
    );
}

#[test]
fn the_request_clock_decides() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(dir.path());
    let writer = scope("writer");
    provision(&mut db, &writer);
    let early = legacy_group(&mut db, &loose_scope("a"), "early", 110, Shape::A2(1));
    let late = legacy_group(&mut db, &loose_scope("b"), "late", 100_000, Shape::A2(1));
    remember(&mut db, &writer, "k1", "m1", ELIGIBLE);
    assert!(absent(&db, &early));
    assert!(present(&db, &late));
    // A later request with an earlier clock deletes nothing its clock does
    // not make eligible.
    remember(&mut db, &writer, "k2", "m2", 250_000);
    assert!(present(&db, &late));
    remember(&mut db, &writer, "k3", "m3", 399_999);
    assert!(present(&db, &late));
    remember(&mut db, &writer, "k4", "m4", 400_000);
    assert!(absent(&db, &late));
}

fn group_of(db: &Database, receipt: String) -> Vec<String> {
    let count = match db
        .graph(GRAPH)
        .unwrap()
        .get_node(&receipt)
        .unwrap()
        .output
        .unwrap()
        .properties
        .get("outcome_chunks")
    {
        Some(MetadataValue::Integer(count)) => *count as usize,
        other => panic!("{other:?}"),
    };
    let chunks: Vec<String> = (0..count)
        .map(|index| receipt_outcome_key(&receipt, index))
        .collect();
    std::iter::once(receipt).chain(chunks).collect()
}

#[test]
fn receipts_from_all_three_encoders_are_reclaimed() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(dir.path());
    let owner = scope("owner");
    let writer = scope("writer");
    provision(&mut db, &owner);
    provision(&mut db, &writer);
    remember(&mut db, &owner, "seed", "m", 10);
    // A multi-chunk remember outcome: 900 deduplicated aliases of one memory.
    let items = (0..900)
        .map(|index| item(&format!("{index:04}-{}", "x".repeat(31)), "m", 10))
        .collect();
    MemoryMut::bind(&mut db, GRAPH, None)
        .unwrap()
        .remember(&remember_request(&owner, "large", 10, 100, items))
        .unwrap();
    MemoryMut::bind(&mut db, GRAPH, None)
        .unwrap()
        .forget(&forget_request(&owner, "forget", 10, 100, "m"))
        .unwrap();
    remember(&mut db, &owner, "seed-c", "n", 10);
    MemoryMut::bind(&mut db, GRAPH, None)
        .unwrap()
        .consolidate(&consolidate_request(&owner, "c", 10, 100, "n", "s"))
        .unwrap();

    let large = group_of(&db, receipt_key(&owner.key(), "large"));
    assert!(large.len() >= 3, "a multi-chunk outcome");
    let groups = concat(&[
        &group_of(&db, receipt_key(&owner.key(), "seed")),
        &large,
        &group_of(&db, forget_receipt_key(&owner.key(), "forget")),
        &group_of(&db, receipt_key(&owner.key(), "seed-c")),
        &group_of(&db, operation_receipt_key("consolidate", &owner.key(), "c")),
    ]);
    assert!(present(&db, &groups));
    remember(&mut db, &writer, "late", "w", ELIGIBLE - 1);
    assert!(present(&db, &groups));
    remember(&mut db, &writer, "later", "w2", ELIGIBLE);
    assert!(absent(&db, &groups));
}

#[test]
fn remember_forget_and_consolidate_each_reclaim() {
    for operation in ["remember", "forget", "consolidate"] {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(dir.path());
        let writer = scope("writer");
        provision(&mut db, &writer);
        remember(&mut db, &writer, "seed", "m", 300_000);
        let group = legacy_group(&mut db, &loose_scope("a"), "g", 110, Shape::A2(1));
        let before = db.sequence;
        let mut bound = MemoryMut::bind(&mut db, GRAPH, None).unwrap();
        match operation {
            "remember" => {
                let request =
                    remember_request(&writer, "k", ELIGIBLE, 100, vec![item("n", "n", ELIGIBLE)]);
                assert!(!bound.remember(&request).unwrap().replayed);
            }
            "forget" => {
                let request = forget_request(&writer, "k", ELIGIBLE, 100, "m");
                assert!(!bound.forget(&request).unwrap().replayed);
            }
            _ => {
                let request = consolidate_request(&writer, "k", ELIGIBLE, 100, "m", "s");
                assert!(!bound.consolidate(&request).unwrap().replayed);
            }
        }
        assert_eq!(db.sequence, before + 1, "{operation}: one commit");
        assert!(absent(&db, &group), "{operation}");
    }
}

#[test]
fn replays_refusals_errors_erasure_and_read_only_files_reclaim_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(dir.path());
    let writer = scope("writer");
    let erased = scope("erased");
    provision(&mut db, &writer);
    provision(&mut db, &erased);
    let window = 600_000;
    let seed = remember_request(&writer, "r", 1_000, window, vec![item("m", "m", 1_000)]);
    let mut bound = MemoryMut::bind(&mut db, GRAPH, None).unwrap();
    bound.remember(&seed).unwrap();
    bound
        .remember(&remember_request(
            &writer,
            "r2",
            1_000,
            window,
            vec![item("n", "n", 1_000)],
        ))
        .unwrap();
    bound
        .forget(&forget_request(&writer, "f", 1_000, window, "m"))
        .unwrap();
    bound
        .consolidate(&consolidate_request(&writer, "c", 1_000, window, "n", "s"))
        .unwrap();
    let group = legacy_group(&mut db, &loose_scope("a"), "g", 110, Shape::A2(1));
    let state = snapshot(&db);
    let sequence = db.sequence;
    let unchanged = |db: &Database, what: &str| {
        assert_eq!(db.sequence, sequence, "{what}");
        assert_eq!(snapshot(db), state, "{what}");
    };

    let mut bound = MemoryMut::bind(&mut db, GRAPH, None).unwrap();
    let mut replay = seed.clone();
    replay.now_ms = ELIGIBLE;
    assert!(bound.remember(&replay).unwrap().replayed);
    let forget = forget_request(&writer, "f", ELIGIBLE, window, "m");
    assert!(bound.forget(&forget).unwrap().replayed);
    let mut consolidate = consolidate_request(&writer, "c", 1_000, window, "n", "s");
    consolidate.now_ms = ELIGIBLE;
    assert!(bound.consolidate(&consolidate).unwrap().replayed);
    unchanged(&db, "replays");

    let mut bound = MemoryMut::bind(&mut db, GRAPH, None).unwrap();
    let mut conflict = replay.clone();
    conflict.items[0].content = "changed".into();
    assert!(bound.remember(&conflict).is_err());
    let mut late = remember_request(&writer, "d", ELIGIBLE, 100, vec![item("x", "x", ELIGIBLE)]);
    late.deadline_ms = Some(ELIGIBLE - 1);
    assert!(bound.remember(&late).is_err());
    let stranger = remember_request(
        &scope("stranger"),
        "s",
        ELIGIBLE,
        100,
        vec![item("x", "x", ELIGIBLE)],
    );
    assert!(bound.remember(&stranger).is_err());
    let missing = forget_request(&writer, "missing", ELIGIBLE, 100, "nobody");
    assert!(bound.forget(&missing).is_err());
    let missing = consolidate_request(&writer, "c2", ELIGIBLE, 100, "nobody", "t");
    assert!(bound.consolidate(&missing).is_err());
    unchanged(&db, "refusals and failed requests");

    // Erasure writes only its own job and receipt.
    erase(&mut db, &erased, ELIGIBLE);
    assert!(present(&db, &group), "erasure reclaims nothing");
    let state = snapshot(&db);
    let sequence = db.sequence;
    drop(db);

    let path = dir.path().join("reclaim.vlite");
    let mut read_only = Database::open_read_only(&path).unwrap();
    if let Ok(mut bound) = MemoryMut::bind(&mut read_only, GRAPH, None) {
        let request =
            remember_request(&writer, "ro", ELIGIBLE, 100, vec![item("y", "y", ELIGIBLE)]);
        assert!(bound.remember(&request).is_err());
    }
    assert_eq!(read_only.sequence, sequence);
    drop(read_only);
    let db = Database::open(&path).unwrap();
    assert_eq!(db.sequence, sequence);
    assert_eq!(snapshot(&db), state);
    assert!(present(&db, &group));
}
