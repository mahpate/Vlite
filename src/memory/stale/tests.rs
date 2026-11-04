//! S1 stale-summary status (Gap 1 C2 contract section 6 S1). Sources are
//! raw valid memories; summaries come from the real C1 `consolidate`;
//! forget and merge use the real A2/A1 paths. Clocks run at `now + 1h` so the
//! wall clock never reclaims a source or summary between the steps.

use std::path::Path;

use super::super::consolidate::{ConsolidateInput, ConsolidateRequest, ConsolidateSummary};
use super::super::{
    ForgetRequest, ForgetTarget, LABEL_MEMORY, LABEL_SCOPE, MemoryItem, MemoryKind, MemoryMut,
    MemoryProperties, MemoryRef, MemoryScope, Provenance, RememberRequest, content_digest, hex,
    memory_key, memory_properties,
};
use super::{SummaryStatus, SummaryStatusRequest, derived_edge_key};
use crate::{
    Database, Expected, Metadata, MetadataValue,
    property_graph::{EdgeRef, GraphOp},
};

const GRAPH: &str = "memory";

fn scope() -> MemoryScope {
    MemoryScope {
        tenant: "t".into(),
        agent: "a".into(),
        user: Some("u".into()),
        session: None,
    }
}

fn other_scope() -> MemoryScope {
    MemoryScope {
        agent: "other".into(),
        ..scope()
    }
}

fn clock() -> u64 {
    crate::current_time_ms() + 3_600_000
}

fn key(id: &str) -> String {
    memory_key(&scope().key(), id)
}

fn edge_key(summary: &str, source: &str) -> String {
    derived_edge_key(&key(summary), &key(source)).unwrap()
}

fn setup(db: &mut Database) {
    db.create_graph(GRAPH).unwrap();
    MemoryMut::bind(db, GRAPH, None)
        .unwrap()
        .provision_scope(&scope())
        .unwrap();
}

fn database(dir: &Path) -> Database {
    let mut db = Database::create(dir.join("s1.vlite"), 2).unwrap();
    setup(&mut db);
    db
}

fn write(db: &mut Database, ops: &[GraphOp]) {
    db.graph_mut(GRAPH).unwrap().write_batch(ops).unwrap();
}

/// Valid revision-1 memory properties for `id` in `scope`.
fn memory(scope: &MemoryScope, id: &str, kind: &str, now: u64) -> Metadata {
    let origin = "origin".to_owned();
    let content = format!("content {id}");
    memory_properties(MemoryProperties {
        scope: &scope.key(),
        id,
        kind,
        content: &content,
        digest: &content_digest(&content),
        sources: std::slice::from_ref(&origin),
        provenance: &[Provenance {
            source: origin.clone(),
            kind: None,
            uri: None,
            transformation: None,
        }],
        importance: 0.5,
        created_at_ms: now as i64,
        updated_at_ms: now as i64,
        observed_at_ms: now as i64,
        memory_revision: 1,
        reference_count: 0,
        evidence: &[],
    })
}

/// Raw Entity sources in the base scope, with an optional deadline.
fn sources(db: &mut Database, ids: &[String], now: u64, expiry: Option<u64>) {
    let mut ops = Vec::new();
    for id in ids {
        ops.push(GraphOp::CreateNode {
            key: key(id),
            labels: vec![LABEL_MEMORY.into()],
            properties: memory(&scope(), id, "entity", now),
        });
        if let Some(expiry) = expiry {
            ops.push(GraphOp::SetNodeExpiry {
                key: key(id),
                expires_at_ms: Some(expiry),
                expected: Expected::Any,
            });
        }
    }
    write(db, &ops);
}

fn ids(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn numbered(count: usize) -> Vec<String> {
    (0..count).map(|index| format!("s{index:03}")).collect()
}

fn consolidate_in(
    db: &mut Database,
    scope: MemoryScope,
    summary: &str,
    inputs: &[String],
    now: u64,
    expiry: Option<u64>,
) {
    let request = ConsolidateRequest {
        scope,
        idempotency_key: format!("consolidate-{summary}"),
        now_ms: now,
        clock_valid: true,
        receipt_window_ms: 1_000,
        deadline_ms: None,
        input_budget: Some(256),
        inputs: inputs
            .iter()
            .map(|id| ConsolidateInput {
                id: id.clone(),
                expected_memory_revision: Some(1),
                retain: true,
            })
            .collect(),
        summary: ConsolidateSummary {
            id: summary.into(),
            content: format!("summary {summary}"),
            source: "summarizer".into(),
            observed_at_ms: Some(now as i64),
            importance: None,
            expiry_ms: expiry,
            expiry_valid: true,
            provenance: Vec::new(),
        },
        transformation: "summarize".into(),
        evidence: None,
        allow_future_observations: false,
    };
    MemoryMut::bind(db, GRAPH, None)
        .unwrap()
        .consolidate(&request)
        .unwrap();
}

fn consolidate(db: &mut Database, summary: &str, inputs: &[String], now: u64) {
    consolidate_in(db, scope(), summary, inputs, now, None);
}

fn forget(db: &mut Database, id: &str, now: u64) {
    let request = ForgetRequest {
        scope: scope(),
        idempotency_key: format!("forget-{id}"),
        receipt_window_ms: 1_000,
        now_ms: now,
        deadline_ms: None,
        targets: vec![ForgetTarget {
            id: id.into(),
            expected_revision: 1,
        }],
    };
    let result = MemoryMut::bind(db, GRAPH, None)
        .unwrap()
        .forget(&request)
        .unwrap();
    assert_eq!(result.deleted, vec![id.to_owned()]);
}

/// Merges an alias with a new source into `id`: memory revision 1 -> 2.
fn merge(db: &mut Database, id: &str, now: u64) {
    let request = RememberRequest {
        scope: scope(),
        idempotency_key: format!("merge-{id}"),
        receipt_window_ms: 1_000,
        now_ms: now,
        deadline_ms: None,
        allow_future_observations: false,
        items: vec![MemoryItem {
            id: format!("alias-{id}"),
            kind: MemoryKind::Entity,
            content: format!("content {id}"),
            source: "other-origin".into(),
            importance: None,
            observed_at_ms: Some(now as i64),
            provenance: Vec::new(),
            references: Vec::new(),
            merge_into: Some(id.into()),
            expected_revision: Some(1),
            expiry_ms: None,
            expiry_valid: true,
            evidence: Vec::new(),
        }],
    };
    let result = MemoryMut::bind(db, GRAPH, None)
        .unwrap()
        .remember(&request)
        .unwrap();
    assert_eq!(result.actions[0].action, "merged");
    assert_eq!(result.actions[0].revision, 2);
}

fn request(scope: MemoryScope, id: &str, now: u64) -> SummaryStatusRequest {
    SummaryStatusRequest {
        scope,
        summary_id: id.into(),
        now_ms: now,
    }
}

/// Status at C; asserts the read appended nothing.
fn status_of(db: &Database, request: &SummaryStatusRequest) -> Result<SummaryStatus, String> {
    let before = (db.sequence, db.end);
    let result = MemoryRef::bind(db, GRAPH, None)
        .unwrap()
        .summary_status(request);
    assert_eq!(before, (db.sequence, db.end), "status appended");
    result.map_err(|error| format!("{error:?}"))
}

fn status(db: &Database, id: &str, now: u64) -> Result<SummaryStatus, String> {
    status_of(db, &request(scope(), id, now))
}

/// Expected status: (source_count, live, changed, expired, stale).
fn expect(
    db: &Database,
    id: &str,
    (count, live, changed, expired, stale): (usize, usize, usize, usize, bool),
) -> SummaryStatus {
    SummaryStatus {
        summary_id: id.into(),
        source_count: count,
        live_sources: live,
        changed_sources: changed,
        expired_sources: expired,
        stale,
        view_sequence: db.sequence,
    }
}

fn error(name: &str) -> Result<SummaryStatus, String> {
    Err(name.to_owned())
}

/// A summary over sources a, b, c created at C+1.
fn three(dir: &Path) -> (Database, u64) {
    let now = clock();
    let mut db = database(dir);
    sources(&mut db, &ids(&["a", "b", "c"]), now, None);
    consolidate(&mut db, "s", &ids(&["a", "b", "c"]), now + 1);
    (db, now)
}

fn patch_edge(db: &mut Database, summary: &str, source: &str, set: Metadata, unset: &[&str]) {
    write(
        db,
        &[GraphOp::PatchEdge {
            edge: EdgeRef::Key(edge_key(summary, source)),
            set,
            unset: unset.iter().map(|name| (*name).to_owned()).collect(),
            expected: Expected::Any,
        }],
    );
}

fn revision(value: MetadataValue) -> Metadata {
    Metadata::from([("c1_source_memory_revision".into(), value)])
}

fn text(value: &str) -> MetadataValue {
    MetadataValue::String(value.into())
}

/// Replaces the summary's properties and labels at its current revision.
fn rewrite_summary(
    db: &mut Database,
    id: &str,
    edit: impl FnOnce(&mut Metadata, &mut Vec<String>),
) {
    let node = db
        .graph(GRAPH)
        .unwrap()
        .get_node(&key(id))
        .unwrap()
        .output
        .unwrap();
    let mut properties = node.properties.clone();
    let mut labels: Vec<String> = node.labels.iter().cloned().collect();
    edit(&mut properties, &mut labels);
    write(
        db,
        &[GraphOp::UpsertNode {
            key: key(id),
            labels,
            properties,
            expected: Expected::Revision(node.revision),
        }],
    );
}

fn delete_edge(db: &mut Database, summary: &str, source: &str) {
    write(
        db,
        &[GraphOp::DeleteEdge {
            edge: EdgeRef::Key(edge_key(summary, source)),
            expected: Expected::Any,
        }],
    );
}

#[test]
fn s1_fresh_summary_is_not_stale() {
    let dir = tempfile::tempdir().unwrap();
    let (db, now) = three(dir.path());
    let status = status(&db, "s", now + 2).unwrap();
    assert_eq!(status, expect(&db, "s", (3, 3, 0, 0, false)));
    assert_eq!(status.view_sequence, db.sequence);
}

#[test]
fn s1_forgotten_source_is_stale() {
    let dir = tempfile::tempdir().unwrap();
    let (mut db, now) = three(dir.path());
    forget(&mut db, "b", now + 2);
    assert_eq!(
        status(&db, "s", now + 3),
        Ok(expect(&db, "s", (3, 2, 0, 0, true)))
    );
    forget(&mut db, "a", now + 4);
    forget(&mut db, "c", now + 5);
    assert_eq!(
        status(&db, "s", now + 6),
        Ok(expect(&db, "s", (3, 0, 0, 0, true)))
    );
}

#[test]
fn s1_merged_source_is_stale() {
    let dir = tempfile::tempdir().unwrap();
    let (mut db, now) = three(dir.path());
    merge(&mut db, "a", now + 2);
    assert_eq!(
        status(&db, "s", now + 3),
        Ok(expect(&db, "s", (3, 3, 1, 0, true)))
    );
}

#[test]
fn s1_edge_revision_ahead_of_source_is_stale() {
    // Any difference counts, not only a source revision that moved ahead.
    let dir = tempfile::tempdir().unwrap();
    let (mut db, now) = three(dir.path());
    patch_edge(&mut db, "s", "b", revision(text("2")), &[]);
    assert_eq!(
        status(&db, "s", now + 2),
        Ok(expect(&db, "s", (3, 3, 1, 0, true)))
    );
}

#[test]
fn s1_source_expiry_boundary_at_captured_clock() {
    let dir = tempfile::tempdir().unwrap();
    let now = clock();
    let expiry = now + 100;
    let mut db = database(dir.path());
    sources(&mut db, &ids(&["a", "c"]), now, None);
    sources(&mut db, &ids(&["b"]), now, Some(expiry));
    consolidate(&mut db, "s", &ids(&["a", "b", "c"]), now + 1);
    assert_eq!(
        status(&db, "s", expiry - 1),
        Ok(expect(&db, "s", (3, 3, 0, 0, false))),
        "C = E - 1"
    );
    assert_eq!(
        status(&db, "s", expiry),
        Ok(expect(&db, "s", (3, 2, 0, 1, true))),
        "C = E"
    );
    assert_eq!(
        status(&db, "s", expiry + 1),
        Ok(expect(&db, "s", (3, 2, 0, 1, true))),
        "C = E + 1"
    );
    // A later write past E reclaims the source and its edge: missing now.
    db.graph_mut(GRAPH)
        .unwrap()
        .write_batch_at(
            &[GraphOp::CreateNode {
                key: "unrelated".into(),
                labels: vec!["Other".into()],
                properties: Metadata::new(),
            }],
            expiry + 1,
        )
        .unwrap();
    assert_eq!(
        status(&db, "s", expiry + 1),
        Ok(expect(&db, "s", (3, 2, 0, 0, true))),
        "reclaimed"
    );
}

#[test]
fn s1_expired_summary_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let now = clock();
    let expiry = now + 100;
    let mut db = database(dir.path());
    sources(&mut db, &ids(&["a"]), now, None);
    consolidate_in(&mut db, scope(), "s", &ids(&["a"]), now + 1, Some(expiry));
    assert_eq!(
        status(&db, "s", expiry - 1),
        Ok(expect(&db, "s", (1, 1, 0, 0, false)))
    );
    assert_eq!(status(&db, "s", expiry), error("MissingMemory"));
    assert_eq!(status(&db, "s", expiry + 1), error("MissingMemory"));
}

#[test]
fn s1_malformed_derived_edges_are_invalid_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let (mut db, now) = three(dir.path());
    let at = now + 2;
    for value in [
        text("01"),
        text("0"),
        text("x"),
        text(""),
        text(" 1"),
        text("+1"),
        text("18446744073709551616"),
        MetadataValue::Integer(1),
    ] {
        patch_edge(&mut db, "s", "b", revision(value.clone()), &[]);
        assert_eq!(status(&db, "s", at), error("InvalidBundle"), "{value:?}");
    }
    patch_edge(
        &mut db,
        "s",
        "b",
        revision(text("18446744073709551615")),
        &[],
    );
    assert_eq!(
        status(&db, "s", at),
        Ok(expect(&db, "s", (3, 3, 1, 0, true))),
        "u64::MAX is canonical"
    );
    patch_edge(
        &mut db,
        "s",
        "b",
        Metadata::new(),
        &["c1_source_memory_revision"],
    );
    assert_eq!(status(&db, "s", at), error("InvalidBundle"), "missing");
    patch_edge(&mut db, "s", "b", revision(text("1")), &[]);
    assert_eq!(
        status(&db, "s", at),
        Ok(expect(&db, "s", (3, 3, 0, 0, false)))
    );
    for (name, value) in [("c1_schema", text("consolidate:v2")), ("extra", text("x"))] {
        patch_edge(
            &mut db,
            "s",
            "b",
            Metadata::from([(name.into(), value)]),
            &[],
        );
        assert_eq!(status(&db, "s", at), error("InvalidBundle"), "{name}");
    }
    patch_edge(
        &mut db,
        "s",
        "b",
        Metadata::from([("c1_schema".into(), text("consolidate:v1"))]),
        &["extra"],
    );
    assert_eq!(
        status(&db, "s", at),
        Ok(expect(&db, "s", (3, 3, 0, 0, false)))
    );

    // An edge deadline is not part of the C1 schema.
    for (deadline, expected) in [
        (Some(now + 1_000_000), error("InvalidBundle")),
        (None, Ok(expect(&db, "s", (3, 3, 0, 0, false)))),
    ] {
        write(
            &mut db,
            &[GraphOp::SetEdgeExpiry {
                edge: EdgeRef::Key(edge_key("s", "b")),
                expires_at_ms: deadline,
                expected: Expected::Any,
            }],
        );
        let expected = expected.map(|status| SummaryStatus {
            view_sequence: db.sequence,
            ..status
        });
        assert_eq!(status(&db, "s", at), expected, "edge expiry {deadline:?}");
    }
}

#[test]
fn s1_foreign_or_extra_derived_edges_are_invalid_bundle() {
    let edge = |destination: String, key: String| GraphOp::CreateEdge {
        key: Some(key),
        source: memory_key(&scope().key(), "s"),
        relationship: "DERIVED_FROM".into(),
        destination,
        properties: Metadata::from([
            ("c1_schema".into(), text("consolidate:v1")),
            ("c1_source_memory_revision".into(), text("1")),
        ]),
    };
    // A valid source under a wrong edge key.
    let dir = tempfile::tempdir().unwrap();
    let (mut db, now) = three(dir.path());
    delete_edge(&mut db, "s", "b");
    write(&mut db, &[edge(key("b"), "derived:v1:wrong".into())]);
    assert_eq!(
        status(&db, "s", now + 2),
        error("InvalidBundle"),
        "edge key"
    );

    // More retained edges than c1_source_count.
    let dir = tempfile::tempdir().unwrap();
    let (mut db, now) = three(dir.path());
    sources(&mut db, &ids(&["d"]), now, None);
    write(&mut db, &[edge(key("d"), edge_key("s", "d"))]);
    assert_eq!(status(&db, "s", now + 2), error("InvalidBundle"), "extra");

    // A destination in another scope, under its correctly derived key.
    let dir = tempfile::tempdir().unwrap();
    let (mut db, now) = three(dir.path());
    MemoryMut::bind(&mut db, GRAPH, None)
        .unwrap()
        .provision_scope(&other_scope())
        .unwrap();
    let foreign = memory_key(&other_scope().key(), "b");
    write(
        &mut db,
        &[GraphOp::CreateNode {
            key: foreign.clone(),
            labels: vec![LABEL_MEMORY.into()],
            properties: memory(&other_scope(), "b", "entity", now),
        }],
    );
    delete_edge(&mut db, "s", "b");
    let derived = derived_edge_key(&key("s"), &foreign).unwrap();
    write(&mut db, &[edge(foreign, derived)]);
    assert_eq!(status(&db, "s", now + 2), error("InvalidBundle"), "foreign");

    // A destination that is not a memory.
    let dir = tempfile::tempdir().unwrap();
    let (mut db, now) = three(dir.path());
    write(
        &mut db,
        &[GraphOp::CreateNode {
            key: "plain-node".into(),
            labels: vec!["Other".into()],
            properties: Metadata::new(),
        }],
    );
    delete_edge(&mut db, "s", "b");
    let derived = derived_edge_key(&key("s"), "plain-node").unwrap();
    write(&mut db, &[edge("plain-node".into(), derived)]);
    assert_eq!(
        status(&db, "s", now + 2),
        error("InvalidBundle"),
        "non-memory"
    );
}

type Edit = Box<dyn Fn(&mut Metadata, &mut Vec<String>)>;

fn set(name: &'static str, value: MetadataValue) -> Edit {
    Box::new(move |properties, _| {
        properties.insert(name.into(), value.clone());
    })
}

#[test]
fn s1_non_c1_nodes_are_invalid_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let (mut db, now) = three(dir.path());
    let at = now + 2;
    // A source (Entity) and a plain Episodic memory are not C1 summaries.
    assert_eq!(status(&db, "a", at), error("InvalidBundle"), "entity");
    write(
        &mut db,
        &[
            GraphOp::CreateNode {
                key: key("plain"),
                labels: vec![LABEL_MEMORY.into()],
                properties: memory(&scope(), "plain", "episodic", now),
            },
            GraphOp::CreateNode {
                key: key("alien"),
                labels: vec!["Other".into()],
                properties: memory(&scope(), "alien", "episodic", now),
            },
        ],
    );
    assert_eq!(status(&db, "plain", at), error("InvalidBundle"), "plain");
    assert_eq!(status(&db, "alien", at), error("InvalidBundle"), "label");

    let cases: Vec<(&str, Edit)> = vec![
        ("schema", set("c1_schema", text("consolidate:v2"))),
        ("count 0", set("c1_source_count", MetadataValue::Integer(0))),
        (
            "count 257",
            set("c1_source_count", MetadataValue::Integer(257)),
        ),
        ("count text", set("c1_source_count", text("3"))),
        ("digest", set("c1_source_digest", text("v1:00"))),
        (
            "transformation",
            set("c1_transformation", MetadataValue::Integer(1)),
        ),
        ("revision", set("memory_revision", text("2"))),
        ("kind", set("kind", text("entity"))),
        ("id", set("id", text("other"))),
        ("updated", set("updated_at_ms", text("1"))),
        ("extra", set("extra", text("x"))),
        (
            "missing",
            Box::new(|properties, _| {
                properties.remove("c1_source_digest");
            }),
        ),
        (
            "extra label",
            Box::new(|_, labels| labels.push("Extra".into())),
        ),
    ];
    for (name, edit) in cases {
        let original = db
            .graph(GRAPH)
            .unwrap()
            .get_node(&key("s"))
            .unwrap()
            .output
            .unwrap();
        rewrite_summary(&mut db, "s", |properties, labels| edit(properties, labels));
        assert_eq!(status(&db, "s", at), error("InvalidBundle"), "{name}");
        rewrite_summary(&mut db, "s", |properties, labels| {
            *properties = original.properties.clone();
            *labels = original.labels.iter().cloned().collect();
        });
        assert_eq!(
            status(&db, "s", at),
            Ok(expect(&db, "s", (3, 3, 0, 0, false))),
            "{name} restored"
        );
    }
}

#[test]
fn s1_scope_identity_and_clock_are_checked() {
    let dir = tempfile::tempdir().unwrap();
    let (mut db, now) = three(dir.path());
    let at = now + 2;
    // A summary in another provisioned scope is not visible from this one.
    MemoryMut::bind(&mut db, GRAPH, None)
        .unwrap()
        .provision_scope(&other_scope())
        .unwrap();
    write(
        &mut db,
        &[GraphOp::CreateNode {
            key: memory_key(&other_scope().key(), "f"),
            labels: vec![LABEL_MEMORY.into()],
            properties: memory(&other_scope(), "f", "entity", now),
        }],
    );
    consolidate_in(&mut db, other_scope(), "x", &ids(&["f"]), now + 1, None);
    assert_eq!(status(&db, "x", at), error("MissingMemory"), "out of scope");
    assert_eq!(
        status_of(&db, &request(other_scope(), "x", at)),
        Ok(expect(&db, "x", (1, 1, 0, 0, false)))
    );
    assert_eq!(status(&db, "unknown", at), error("MissingMemory"));
    let unprovisioned = MemoryScope {
        agent: "never".into(),
        ..scope()
    };
    assert_eq!(
        status_of(&db, &request(unprovisioned, "s", at)),
        error("InvalidScope")
    );
    let invalid = MemoryScope {
        user: None,
        session: Some("s".into()),
        ..scope()
    };
    assert_eq!(
        status_of(&db, &request(invalid, "s", at)),
        error("InvalidScope")
    );
    assert_eq!(status(&db, "", at), error("InvalidTarget"));
    assert_eq!(status(&db, &"x".repeat(257), at), error("InvalidTarget"));
    assert_eq!(status(&db, "s", i64::MAX as u64 + 1), error("InvalidClock"));
    assert!(status(&db, "s", i64::MAX as u64).is_ok());
}

#[test]
fn s1_scope_tuple_mismatch_is_digest_collision() {
    // A scope node at the request's scope key whose tuple encodes another
    // scope: the key collided, so the read stops before any summary decode.
    let dir = tempfile::tempdir().unwrap();
    let (mut db, now) = three(dir.path());
    let at = now + 2;
    let scope_key = scope().key();
    let node = db
        .graph(GRAPH)
        .unwrap()
        .get_node(&scope_key)
        .unwrap()
        .output
        .unwrap();
    let foreign = MetadataValue::String(hex(&other_scope().encode()));
    assert_ne!(node.properties.get("tuple"), Some(&foreign));
    let mut properties = node.properties.clone();
    properties.insert("tuple".into(), foreign);
    write(
        &mut db,
        &[GraphOp::UpsertNode {
            key: scope_key.clone(),
            labels: vec![LABEL_SCOPE.into()],
            properties,
            expected: Expected::Revision(node.revision),
        }],
    );
    let planted = db
        .graph(GRAPH)
        .unwrap()
        .get_node(&scope_key)
        .unwrap()
        .output
        .unwrap();
    let summary = db
        .graph(GRAPH)
        .unwrap()
        .get_node(&key("s"))
        .unwrap()
        .output
        .unwrap();
    assert_eq!(status(&db, "s", at), error("DigestCollision"));
    assert_eq!(status(&db, "unknown", at), error("DigestCollision"));
    // No side effects: status_of checks nothing was appended; both nodes
    // are byte-for-byte what they were before the reads.
    let graph = db.graph(GRAPH).unwrap();
    assert_eq!(graph.get_node(&scope_key).unwrap().output, Some(planted));
    assert_eq!(graph.get_node(&key("s")).unwrap().output, Some(summary));
}

#[test]
fn s1_ignores_non_derived_from_outgoing_edges() {
    // Only DERIVED_FROM edges are examined; any other well-formed outgoing
    // edge of the summary changes neither the counts nor the verdict.
    let dir = tempfile::tempdir().unwrap();
    let (mut db, now) = three(dir.path());
    let at = now + 2;
    let before = status(&db, "s", at).unwrap();
    assert_eq!(before, expect(&db, "s", (3, 3, 0, 0, false)));
    write(
        &mut db,
        &[GraphOp::CreateEdge {
            key: Some("relates:s:a".into()),
            source: key("s"),
            relationship: "RELATES_TO".into(),
            destination: key("a"),
            properties: Metadata::from([("note".into(), text("x"))]),
        }],
    );
    let after = status(&db, "s", at).unwrap();
    assert_eq!(after, expect(&db, "s", (3, 3, 0, 0, false)));
    assert_eq!(
        SummaryStatus {
            view_sequence: before.view_sequence,
            ..after.clone()
        },
        before
    );
    assert_eq!(after.view_sequence, db.sequence);
    // Still independent of the other edge once the summary goes stale.
    forget(&mut db, "b", now + 3);
    assert_eq!(
        status(&db, "s", now + 4),
        Ok(expect(&db, "s", (3, 2, 0, 0, true)))
    );
}

#[test]
fn s1_examines_at_most_256_derived_edges() {
    for count in [255usize, 256] {
        let dir = tempfile::tempdir().unwrap();
        let now = clock();
        let mut db = database(dir.path());
        let inputs = numbered(count);
        sources(&mut db, &inputs, now, None);
        consolidate(&mut db, "s", &inputs, now + 1);
        assert_eq!(
            status(&db, "s", now + 2),
            Ok(expect(&db, "s", (count, count, 0, 0, false))),
            "{count}"
        );
        forget(&mut db, &inputs[0], now + 3);
        assert_eq!(
            status(&db, "s", now + 4),
            Ok(expect(&db, "s", (count, count - 1, 0, 0, true))),
            "{count} forgotten"
        );
    }
    // A 257th DERIVED_FROM edge exceeds the examination cap before decoding.
    let dir = tempfile::tempdir().unwrap();
    let now = clock();
    let mut db = database(dir.path());
    let inputs = numbered(256);
    sources(&mut db, &inputs, now, None);
    sources(&mut db, &ids(&["extra"]), now, None);
    consolidate(&mut db, "s", &inputs, now + 1);
    write(
        &mut db,
        &[GraphOp::CreateEdge {
            key: None,
            source: key("s"),
            relationship: "DERIVED_FROM".into(),
            destination: key("extra"),
            properties: Metadata::new(),
        }],
    );
    assert_eq!(status(&db, "s", now + 2), error("OperationLimit"));
}

#[test]
fn s1_status_survives_reopen_on_every_backend() {
    let dir = tempfile::tempdir().unwrap();
    let encryption_key = crate::EncryptionKey::from_bytes([5; 32]);
    for (name, directory, encrypted) in [
        ("plain.vlite", false, false),
        ("encrypted.vlite", false, true),
        ("plain-directory", true, false),
        ("encrypted-directory", true, true),
    ] {
        let path = dir.path().join(name);
        let options = crate::CreateOptions {
            encryption: encrypted.then(|| encryption_key.clone()),
            ..crate::CreateOptions::default()
        };
        let mut db = if directory {
            Database::create_directory(&path, 2, options).unwrap()
        } else {
            Database::create_with_options(&path, 2, options).unwrap()
        };
        setup(&mut db);
        let now = clock();
        let expiry = now + 100;
        sources(&mut db, &ids(&["a", "b", "c"]), now, None);
        sources(&mut db, &ids(&["d"]), now, Some(expiry));
        consolidate(&mut db, "stale", &ids(&["a", "b", "c", "d"]), now + 1);
        consolidate(&mut db, "fresh", &ids(&["c"]), now + 1);
        forget(&mut db, "a", now + 2);
        merge(&mut db, "b", now + 3);
        let before = [
            status(&db, "stale", expiry).unwrap(),
            status(&db, "fresh", expiry).unwrap(),
        ];
        assert_eq!(
            before[0],
            expect(&db, "stale", (4, 2, 1, 1, true)),
            "{name}"
        );
        assert_eq!(
            before[1],
            expect(&db, "fresh", (1, 1, 0, 0, false)),
            "{name}"
        );
        drop(db);

        let db = if encrypted {
            Database::open_encrypted(&path, &encryption_key).unwrap()
        } else {
            Database::open(&path).unwrap()
        };
        let after = [
            status(&db, "stale", expiry).unwrap(),
            status(&db, "fresh", expiry).unwrap(),
        ];
        assert_eq!(after, before, "{name}");
    }
}
