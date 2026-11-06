//! E2 resume chunks (`docs/projects/roadmap/gap1-e2-resume-contract.md`):
//! dedicated checks, and one named kill test for every hand mutant of the
//! contract's section 9 (`mutant_*` below; the ledger names them).
//!
//! The two mutants that are equivalent under `&mut Database` are recorded,
//! not killed (E1's ledger precedent): dropping the job revision guard of
//! the chunk frame, and reusing an expired receipt without its replacement
//! guard. Nothing can change the job or the receipt between the planning
//! read and the frame while the handle holds `&mut Database`.
//!
//! Scopes that must span several chunks use hub memories: a memory with
//! thousands of extra edges fills a frame (section 5), so two or three such
//! memories need one chunk each without thousands of memories.

use std::path::PathBuf;

use super::super::{
    FAIL_NEXT_WRITE_BATCH, LABEL_CONTENT, LABEL_RECEIPT, LABEL_RECEIPT_OUTCOME, MemoryError,
    MemoryEvidence, MemoryItem, MemoryKind, MemoryMut, MemoryRef, MemoryResult, MemoryScope,
    RememberRequest, RememberResult, content_edge_key, content_key, evidence_record_id, hex,
    memory_key, operation_receipt_key, receipt_outcome_key, scan_memories,
};
use super::{
    BeginErasureResult, EraseScopeRequest, ErasureState, ErasureStatusRequest, LABEL_JOB, job_key,
    resume::{
        ChunkSummary, ErasureJob, ErasureJobRequest, ResumeErasureRequest, ResumeErasureResult,
        token,
    },
};
use crate::{
    CollectionSettings, CollectionSpec, Database, Expected, Metadata, MetadataValue, Mutation,
    property_graph::{
        Direction, EdgeRef, GraphLimits, GraphOp, Neighbors, NodeSelector, NodeView, ReadOptions,
    },
};

const GRAPH: &str = "memory";
/// The handle's evidence collection; a second one exists for foreign cases.
const BINDING: &str = "c";
const OTHER_COLLECTION: &str = "d";
/// Request clocks sit far above the wall clock so that TTL outcomes never
/// depend on it (section 7).
const T0: u64 = 3_000_000_000_000;
const WINDOW: u64 = 60_000;

fn scope() -> MemoryScope {
    MemoryScope {
        tenant: "t".into(),
        agent: "a".into(),
        user: None,
        session: None,
    }
}

/// A child scope (a user under the same tenant and agent) and a sibling
/// agent: neither is the erased scope (E1 section 5).
fn child_scope() -> MemoryScope {
    MemoryScope {
        user: Some("u".into()),
        ..scope()
    }
}

fn sibling_scope() -> MemoryScope {
    MemoryScope {
        agent: "b".into(),
        ..scope()
    }
}

fn item(id: &str, content: &str) -> MemoryItem {
    MemoryItem {
        id: id.into(),
        kind: MemoryKind::Entity,
        content: content.into(),
        source: "source".into(),
        importance: Some(0.5),
        observed_at_ms: Some(0),
        provenance: Vec::new(),
        references: Vec::new(),
        merge_into: None,
        expected_revision: None,
        expiry_ms: None,
        expiry_valid: true,
        evidence: Vec::new(),
    }
}

fn evidence_item(id: &str, evidence: &str, scope: &MemoryScope) -> MemoryItem {
    MemoryItem {
        evidence: vec![MemoryEvidence {
            id: evidence.into(),
            scope: scope.clone(),
        }],
        ..item(id, &format!("content {id}"))
    }
}

fn upsert(node: &NodeView) -> GraphOp {
    GraphOp::UpsertNode {
        key: node.key.clone(),
        labels: node.labels.iter().cloned().collect(),
        properties: node.properties.clone(),
        expected: Expected::Any,
    }
}

fn string(value: &str) -> MetadataValue {
    MetadataValue::String(value.into())
}

#[derive(Clone, Copy, Debug)]
enum Backend {
    Plain,
    Encrypted,
    Directory,
    EncryptedDirectory,
}

const BACKENDS: [Backend; 4] = [
    Backend::Plain,
    Backend::Encrypted,
    Backend::Directory,
    Backend::EncryptedDirectory,
];

impl Backend {
    fn directory(self) -> bool {
        matches!(self, Self::Directory | Self::EncryptedDirectory)
    }

    fn encrypted(self) -> bool {
        matches!(self, Self::Encrypted | Self::EncryptedDirectory)
    }
}

fn encryption_key() -> crate::EncryptionKey {
    crate::EncryptionKey::from_bytes([9; 32])
}

/// One database on a chosen backend, reopenable in place.
struct Store {
    _dir: tempfile::TempDir,
    path: PathBuf,
    backend: Backend,
    db: Option<Database>,
    seeds: u32,
    /// The evidence collection the handles are bound to (E1 section 2).
    binding: Option<&'static str>,
}

impl Store {
    fn new(backend: Backend) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("e2");
        let options = crate::CreateOptions {
            encryption: backend.encrypted().then(encryption_key),
            ..crate::CreateOptions::default()
        };
        let mut db = if backend.directory() {
            Database::create_directory(&path, 2, options).unwrap()
        } else {
            Database::create_with_options(&path, 2, options).unwrap()
        };
        db.create_graph(GRAPH).unwrap();
        for name in [BINDING, OTHER_COLLECTION] {
            db.create_collection(
                name,
                CollectionSpec {
                    dimensions: 2,
                    settings: CollectionSettings::default(),
                },
            )
            .unwrap();
        }
        Self {
            _dir: dir,
            path,
            backend,
            db: Some(db),
            seeds: 0,
            binding: Some(BINDING),
        }
    }

    fn plain() -> Self {
        Self::new(Backend::Plain)
    }

    fn reopen(&mut self) {
        self.db = None;
        self.db = Some(if self.backend.encrypted() {
            Database::open_encrypted(&self.path, &encryption_key()).unwrap()
        } else {
            Database::open(&self.path).unwrap()
        });
    }

    /// Closes the store, compacts it into a new file and continues on the
    /// copy (plain and encrypted files only).
    fn compact(&mut self) {
        assert!(!self.backend.directory());
        self.db = None;
        let target = self.path.with_extension("compact");
        if self.backend.encrypted() {
            crate::maintenance::compact_with_key(&self.path, &target, &encryption_key()).unwrap();
        } else {
            crate::maintenance::compact(&self.path, &target).unwrap();
        }
        self.path = target;
        self.reopen();
    }

    fn db(&self) -> &Database {
        self.db.as_ref().unwrap()
    }

    fn db_mut(&mut self) -> &mut Database {
        self.db.as_mut().unwrap()
    }

    fn writer_with(&mut self, binding: Option<&str>) -> MemoryMut<'_> {
        MemoryMut::bind(self.db_mut(), GRAPH, binding).unwrap()
    }

    fn writer(&mut self) -> MemoryMut<'_> {
        let binding = self.binding;
        self.writer_with(binding)
    }

    fn reader_with(&self, binding: Option<&str>) -> MemoryRef<'_> {
        MemoryRef::bind(self.db(), GRAPH, binding).unwrap()
    }

    fn reader(&self) -> MemoryRef<'_> {
        self.reader_with(self.binding)
    }

    fn sequence(&self) -> u64 {
        self.db().sequence
    }

    /// Whether `key` is a visible node at the explicit clock `now`.
    fn visible_at(&self, key: &str, now: u64) -> bool {
        self.db()
            .graph(GRAPH)
            .unwrap()
            .with_clock(now)
            .get_node(key)
            .unwrap()
            .output
            .is_some()
    }

    /// Whether the evidence record `id` exists in `collection`.
    fn has_record(&self, collection: &str, id: &str) -> bool {
        self.db()
            .collection(collection)
            .unwrap()
            .get(evidence_record_id(id))
            .unwrap()
            .is_some()
    }

    fn node(&self, key: &str) -> Option<NodeView> {
        self.db()
            .graph(GRAPH)
            .unwrap()
            .get_node(key)
            .unwrap()
            .output
    }

    fn raw(&mut self, ops: Vec<GraphOp>) {
        for part in ops.chunks(4_000) {
            self.db_mut()
                .graph_mut(GRAPH)
                .unwrap()
                .write_batch(part)
                .unwrap();
        }
    }

    fn provision(&mut self, scope: &MemoryScope) {
        self.writer_with(None).provision_scope(scope).unwrap();
    }

    fn remember(&mut self, scope: &MemoryScope, items: Vec<MemoryItem>) -> RememberResult {
        self.seeds += 1;
        let request = RememberRequest {
            scope: scope.clone(),
            idempotency_key: format!("seed-{}", self.seeds),
            receipt_window_ms: WINDOW,
            now_ms: T0,
            deadline_ms: None,
            allow_future_observations: false,
            items,
        };
        self.writer().remember(&request).unwrap()
    }

    /// `count` inline memories `prefix0000..` in creation order.
    fn remember_many(&mut self, scope: &MemoryScope, prefix: &str, count: usize) {
        for start in (0..count).step_by(400) {
            let end = (start + 400).min(count);
            let items = (start..end)
                .map(|index| item(&format!("{prefix}{index:05}"), &format!("content {index}")))
                .collect();
            self.remember(scope, items);
        }
    }

    /// Puts one evidence record `id` into `collection`.
    fn evidence(&mut self, collection: &str, id: &str) {
        self.raw(vec![GraphOp::Vector {
            collection: collection.into(),
            mutation: Mutation::Put {
                id: evidence_record_id(id),
                vector: vec![1.0, 0.0],
                payload: id.as_bytes().to_vec(),
            },
        }]);
    }

    fn sinks(&mut self, prefix: &str, count: usize) {
        let ops = (0..count)
            .map(|index| GraphOp::CreateNode {
                key: format!("{prefix}-{index}"),
                labels: vec!["Sink".into()],
                properties: Metadata::new(),
            })
            .collect();
        self.raw(ops);
    }

    /// `count` edges from `source` to the first `count` sinks of `prefix`.
    fn link(&mut self, source: &str, prefix: &str, count: usize) {
        let ops = (0..count)
            .map(|index| GraphOp::CreateEdge {
                key: None,
                source: source.into(),
                relationship: "LINK".into(),
                destination: format!("{prefix}-{index}"),
                properties: Metadata::new(),
            })
            .collect();
        self.raw(ops);
    }

    /// Gives the memory `id` of `scope` exactly `count` extra edges.
    fn hub(&mut self, scope: &MemoryScope, id: &str, count: usize) {
        let prefix = format!("sink-{id}");
        self.sinks(&prefix, count);
        self.link(&memory_key(&scope.key(), id), &prefix, count);
    }

    fn edge_count(&self, key: &str) -> usize {
        let graph = self.db().graph(GRAPH).unwrap();
        let mut cursor = None;
        let mut count = 0;
        loop {
            let page = graph
                .neighbors(
                    &Neighbors {
                        node: NodeSelector::key(key),
                        direction: Direction::Both,
                        types: None,
                        edge_conditions: Vec::new(),
                        limit: Some(GraphLimits::CEILING.max_output_edges),
                        cursor,
                    },
                    &ReadOptions {
                        limits: GraphLimits::CEILING,
                        ..ReadOptions::default()
                    },
                )
                .unwrap();
            count += page.output.edges.len();
            match page.cursor {
                Some(next) => cursor = Some(next),
                None => return count,
            }
        }
    }

    /// The IDs of the live memories of `scope`, in memory-key order.
    fn live(&self, scope: &MemoryScope) -> Vec<String> {
        let mut ids: Vec<String> = scan_memories(&self.db().graph(GRAPH).unwrap(), &scope.key())
            .unwrap()
            .into_iter()
            .map(|memory| memory.id)
            .collect();
        ids.sort();
        ids
    }

    /// The begin request as a caller sends it now: it pins the scope's
    /// current revision, which a retry keeps even after maintenance
    /// renumbers the graph.
    fn begin_request(&self, scope: &MemoryScope, key: &str, now: u64) -> EraseScopeRequest {
        EraseScopeRequest {
            scope: scope.clone(),
            idempotency_key: key.into(),
            expected_scope_revision: self.reader().scope_revision(scope).unwrap(),
            now_ms: now,
            receipt_window_ms: WINDOW,
            deadline_ms: None,
        }
    }

    fn begin_with(&mut self, scope: &MemoryScope, key: &str, now: u64) -> BeginErasureResult {
        let request = self.begin_request(scope, key, now);
        self.writer().begin_erasure(&request).unwrap()
    }

    fn begin(&mut self, scope: &MemoryScope) -> BeginErasureResult {
        self.begin_with(scope, "begin", T0)
    }

    fn try_job(&self, scope: &MemoryScope) -> MemoryResult<ErasureJob> {
        self.reader().erasure_job(&ErasureJobRequest {
            scope: scope.clone(),
        })
    }

    fn job(&self, scope: &MemoryScope) -> ErasureJob {
        self.try_job(scope).unwrap()
    }

    /// A request carrying the fence of the current job.
    fn request(&self, scope: &MemoryScope, key: &str, now: u64) -> ResumeErasureRequest {
        let job = self.job(scope);
        ResumeErasureRequest {
            scope: scope.clone(),
            job_key: job.job_key.clone(),
            idempotency_key: key.into(),
            expected_revision: job.revision,
            generation: job.generation,
            resume_token: job.resume_token.clone().expect("a pending job"),
            now_ms: now,
            receipt_window_ms: WINDOW,
            deadline_ms: None,
        }
    }

    fn resume(&mut self, request: &ResumeErasureRequest) -> MemoryResult<ResumeErasureResult> {
        self.writer().resume_erasure(request)
    }

    fn next(&mut self, scope: &MemoryScope, key: &str, now: u64) -> ResumeErasureResult {
        let request = self.request(scope, key, now);
        self.resume(&request).unwrap()
    }

    /// Runs `call` and asserts that it wrote nothing (not even a sequence).
    fn unchanged<T>(&mut self, call: impl FnOnce(&mut Self) -> T) -> T {
        let before = self.sequence();
        let result = call(self);
        assert_eq!(self.sequence(), before, "the call wrote");
        result
    }
}

fn error_of<T: std::fmt::Debug>(result: MemoryResult<T>) -> MemoryError {
    result.expect_err("an error was expected")
}

fn is_storage(error: &MemoryError) -> bool {
    matches!(error, MemoryError::Storage(_))
}

/// `A`, `B` and `C` memories with 2,500 extra edges each: one chunk each
/// (two do not fit one frame), so a job takes three chunks.
fn three_chunk_scope(store: &mut Store, scope: &MemoryScope) {
    store.provision(scope);
    store.remember(scope, vec![item("a", "a"), item("b", "b"), item("c", "c")]);
    store.sinks("sink", 2_500);
    for id in ["a", "b", "c"] {
        store.link(&memory_key(&scope.key(), id), "sink", 2_500);
    }
}

#[test]
fn a_chunk_deletes_memories_edges_content_and_evidence_then_completes() {
    let mut store = Store::plain();
    let scope = scope();
    store.provision(&scope);
    store.evidence(BINDING, "ev-a");
    store.remember(&scope, vec![item("b", "inline")]);
    let mut a = evidence_item("a", "ev-a", &scope);
    a.content = "x".repeat(9_000);
    a.references = vec!["b".into()];
    store.remember(&scope, vec![a]);
    let (a_key, b_key) = (memory_key(&scope.key(), "a"), memory_key(&scope.key(), "b"));
    store.raw(vec![
        GraphOp::CreateNode {
            key: "outside".into(),
            labels: vec!["Other".into()],
            properties: Metadata::new(),
        },
        GraphOp::CreateEdge {
            key: None,
            source: a_key.clone(),
            relationship: "TOUCHES".into(),
            destination: "outside".into(),
            properties: Metadata::new(),
        },
    ]);
    // Three content edges, the reference and the outside edge.
    assert_eq!(store.edge_count(&a_key), 5);
    for index in 0..3 {
        assert!(store.node(&content_key(&a_key, index)).is_some());
    }
    assert!(store.has_record(BINDING, "ev-a"));
    let sibling = sibling_scope();
    let child = child_scope();
    for other in [&sibling, &child] {
        store.provision(other);
        store.remember(other, vec![item("keep", "kept")]);
    }

    store.begin(&scope);
    let job = store.job(&scope);
    assert_eq!(job.state, ErasureState::Pending);
    assert_eq!(
        (job.generation, job.deleted_count, job.remaining, job.chunks),
        (1, 0, None, 0)
    );
    assert!(job.resume_token.is_some());

    let result = store.next(&scope, "k1", T0 + 1);
    assert!(!result.replayed);
    assert_eq!(
        result.chunk,
        ChunkSummary {
            memories_deleted: 2,
            remaining_after: Some(0)
        }
    );
    assert_eq!(result.job.state, ErasureState::Logical);
    assert_eq!(
        (
            result.job.deleted_count,
            result.job.remaining,
            result.job.chunks,
            result.job.generation
        ),
        (2, Some(0), 1, 1)
    );
    assert_eq!(result.job.resume_token, None);
    assert_eq!(result.job.job_key, job.job_key);
    assert_eq!(result.job.revision, result.receipt_revision);
    assert_eq!(result.commit_sequence, result.receipt_revision);
    assert_eq!(result.receipt_revision, store.sequence());

    assert!(store.node(&a_key).is_none() && store.node(&b_key).is_none());
    for index in 0..3 {
        assert!(store.node(&content_key(&a_key, index)).is_none());
    }
    assert!(store.node("outside").is_some());
    assert_eq!(store.edge_count("outside"), 0);
    assert!(!store.has_record(BINDING, "ev-a"));
    assert!(store.live(&scope).is_empty());
    assert_eq!(store.job(&scope), result.job);
    // The sibling and child scopes are untouched and unbarred.
    for other in [&sibling, &child] {
        assert_eq!(store.live(other), ["keep"]);
        store.remember(other, vec![item("more", "more")]);
    }
    // The barrier stays on the erased scope.
    let blocked = RememberRequest {
        scope: scope.clone(),
        idempotency_key: "after".into(),
        receipt_window_ms: WINDOW,
        now_ms: T0 + 2,
        deadline_ms: None,
        allow_future_observations: false,
        items: vec![item("again", "again")],
    };
    assert!(matches!(
        store.writer().remember(&blocked),
        Err(MemoryError::DeletionPending)
    ));
}

#[test]
fn an_empty_scope_completes_in_its_first_chunk() {
    let mut store = Store::plain();
    let scope = scope();
    store.provision(&scope);
    store.begin(&scope);
    let result = store.next(&scope, "k1", T0 + 1);
    assert_eq!(
        result.chunk,
        ChunkSummary {
            memories_deleted: 0,
            remaining_after: Some(0)
        }
    );
    assert_eq!(result.job.state, ErasureState::Logical);
    assert_eq!((result.job.deleted_count, result.job.chunks), (0, 1));
    assert_eq!(store.job(&scope), result.job);
}

/// Mutant: allow a resume on a `logical` job.
#[test]
fn mutant_resume_on_a_logical_job() {
    let mut store = Store::plain();
    let scope = scope();
    store.provision(&scope);
    store.remember(&scope, vec![item("m", "m")]);
    store.begin(&scope);
    let original = store.request(&scope, "k1", T0 + 1);
    let first = store.resume(&original).unwrap();
    assert_eq!(first.job.state, ErasureState::Logical);
    // A request that passes every fence check of the completed job (current
    // revision, generation and token) still finds no chunk to run: the
    // state is decided before the deadline and the fence.
    let job = &first.job;
    let fenced = ResumeErasureRequest {
        scope: scope.clone(),
        job_key: job.job_key.clone(),
        idempotency_key: "k2".into(),
        expected_revision: job.revision,
        generation: 1,
        resume_token: token(
            &scope,
            Some(BINDING),
            &job.job_key,
            1,
            job.revision,
            job.chunks,
        ),
        now_ms: T0 + 2,
        receipt_window_ms: WINDOW,
        deadline_ms: None,
    };
    store.unchanged(|store| {
        assert!(matches!(
            store.resume(&fenced),
            Err(MemoryError::JobConflict)
        ));
        let late = ResumeErasureRequest {
            deadline_ms: Some(T0),
            ..fenced.clone()
        };
        assert!(matches!(store.resume(&late), Err(MemoryError::JobConflict)));
    });
    // The completing chunk's own key still replays its original view.
    let replay = store.unchanged(|store| store.resume(&original).unwrap());
    assert!(replay.replayed);
    assert_eq!(replay.chunk, first.chunk);
    assert_eq!(replay.job, first.job);
}

/// Mutants: ignore `expected_revision`; ignore the generation; accept a
/// token of another revision; accept a token of another chunk count.
#[test]
fn mutant_each_fence_field_alone_refuses_the_chunk() {
    let mut store = Store::plain();
    let scope = scope();
    three_chunk_scope(&mut store, &scope);
    store.begin(&scope);
    let good = store.request(&scope, "k1", T0 + 1);
    let job = store.job(&scope);
    let key = &job.job_key;
    for (label, request) in [
        (
            "revision",
            ResumeErasureRequest {
                expected_revision: good.expected_revision + 1,
                ..good.clone()
            },
        ),
        (
            "generation",
            ResumeErasureRequest {
                generation: 2,
                ..good.clone()
            },
        ),
        (
            "token revision",
            ResumeErasureRequest {
                resume_token: token(&scope, Some(BINDING), key, 1, job.revision + 1, job.chunks),
                ..good.clone()
            },
        ),
        (
            "token chunk count",
            ResumeErasureRequest {
                resume_token: token(&scope, Some(BINDING), key, 1, job.revision, job.chunks + 1),
                ..good.clone()
            },
        ),
        (
            "token generation",
            ResumeErasureRequest {
                resume_token: token(&scope, Some(BINDING), key, 2, job.revision, job.chunks),
                ..good.clone()
            },
        ),
        (
            "token binding",
            ResumeErasureRequest {
                resume_token: token(&scope, None, key, 1, job.revision, job.chunks),
                ..good.clone()
            },
        ),
        (
            "token scope",
            ResumeErasureRequest {
                resume_token: token(
                    &sibling_scope(),
                    Some(BINDING),
                    key,
                    1,
                    job.revision,
                    job.chunks,
                ),
                ..good.clone()
            },
        ),
        (
            "token job key",
            ResumeErasureRequest {
                resume_token: token(&scope, Some(BINDING), "other", 1, job.revision, job.chunks),
                ..good.clone()
            },
        ),
    ] {
        let sequence = store.sequence();
        let error = error_of(store.resume(&request));
        assert!(
            matches!(error, MemoryError::JobConflict),
            "{label}: {error:?}"
        );
        assert_eq!(store.sequence(), sequence, "{label} wrote");
        assert_eq!(store.job(&scope), job, "{label} changed the job");
        assert_eq!(store.live(&scope), ["a", "b", "c"], "{label}");
    }
    // The untouched request is accepted.
    let result = store.resume(&good).unwrap();
    assert_eq!(result.chunk.memories_deleted, 1);
}

/// Every later fence value is read again from the new job: the token of the
/// previous revision is stale (JobConflict), the current one works.
#[test]
fn a_token_is_valid_only_for_its_revision_and_chunk_count() {
    let mut store = Store::plain();
    let scope = scope();
    three_chunk_scope(&mut store, &scope);
    store.begin(&scope);
    let stale = store.request(&scope, "k1", T0 + 1);
    let first = store.resume(&stale).unwrap();
    assert_eq!(first.job.state, ErasureState::Pending);
    assert_eq!(first.job.chunks, 1);
    let again = ResumeErasureRequest {
        idempotency_key: "k2".into(),
        ..stale.clone()
    };
    assert!(matches!(
        store.resume(&again),
        Err(MemoryError::JobConflict)
    ));
    let fresh = store.request(&scope, "k2", T0 + 2);
    assert_eq!(fresh.resume_token, first.job.resume_token.clone().unwrap());
    let second = store.resume(&fresh).unwrap();
    assert_eq!(second.job.chunks, 2);
    assert_eq!(second.job.deleted_count, 2);
    assert_eq!(second.job.remaining, Some(1));
    assert_ne!(second.job.resume_token, first.job.resume_token);
}

/// Mutant: replay returns the current job instead of the stored outcome.
#[test]
fn mutant_replay_returns_the_original_chunk_view_after_later_chunks() {
    let mut store = Store::plain();
    let scope = scope();
    three_chunk_scope(&mut store, &scope);
    store.begin(&scope);
    let mut originals = Vec::new();
    for (step, key) in ["k1", "k2", "k3"].into_iter().enumerate() {
        let request = store.request(&scope, key, T0 + 1 + step as u64);
        let result = store.resume(&request).unwrap();
        originals.push((request, result));
    }
    assert_eq!(originals[2].1.job.state, ErasureState::Logical);
    assert_eq!(
        originals
            .iter()
            .map(|(_, result)| result.chunk.remaining_after)
            .collect::<Vec<_>>(),
        [Some(2), Some(1), Some(0)]
    );
    for (request, original) in &originals {
        // Replay needs no current fence and no clock inside the window: the
        // identical request answers from the stored outcome.
        let replay = store.unchanged(|store| {
            store
                .resume(&ResumeErasureRequest {
                    now_ms: request.now_ms + 7,
                    ..request.clone()
                })
                .unwrap()
        });
        assert!(replay.replayed);
        assert_eq!(replay.chunk, original.chunk);
        assert_eq!(replay.job, original.job, "{}", request.idempotency_key);
        assert_eq!(replay.receipt_revision, original.receipt_revision);
        assert_eq!(replay.commit_sequence, original.commit_sequence);
    }
    // The current job is not the first chunk's.
    assert_ne!(store.job(&scope), originals[0].1.job);
}

/// Mutants: the digest omits the token; the digest omits the window.
#[test]
fn mutant_digest_binds_the_fence_token_and_window() {
    let mut store = Store::plain();
    let scope = scope();
    three_chunk_scope(&mut store, &scope);
    store.begin(&scope);
    let request = store.request(&scope, "k1", T0 + 1);
    store.resume(&request).unwrap();
    let job = store.job(&scope);
    let key = &job.job_key;
    for (label, changed) in [
        (
            "token",
            ResumeErasureRequest {
                resume_token: token(&scope, Some(BINDING), key, 1, job.revision, job.chunks),
                ..request.clone()
            },
        ),
        (
            "window",
            ResumeErasureRequest {
                receipt_window_ms: WINDOW + 1,
                ..request.clone()
            },
        ),
        (
            "revision",
            ResumeErasureRequest {
                expected_revision: request.expected_revision + 1,
                ..request.clone()
            },
        ),
        (
            "generation",
            ResumeErasureRequest {
                generation: 2,
                ..request.clone()
            },
        ),
    ] {
        let error = store.unchanged(|store| error_of(store.resume(&changed)));
        assert!(
            matches!(error, MemoryError::IdempotencyConflict),
            "{label}: {error:?}"
        );
    }
    // The clock and the deadline are not hashed: the same request replays.
    let replay = store.unchanged(|store| {
        store
            .resume(&ResumeErasureRequest {
                now_ms: T0 + 50,
                deadline_ms: Some(T0),
                ..request.clone()
            })
            .unwrap()
    });
    assert!(replay.replayed);
}

/// Mutant: check the deadline before the replay. Also the order of the
/// deadline against the fence.
#[test]
fn mutant_deadline_is_checked_after_the_replay_and_before_the_fence() {
    let mut store = Store::plain();
    let scope = scope();
    three_chunk_scope(&mut store, &scope);
    store.begin(&scope);
    let request = store.request(&scope, "k1", T0 + 1);
    let first = store.resume(&request).unwrap();
    // Replay ignores the deadline.
    let expired = ResumeErasureRequest {
        deadline_ms: Some(T0 + 1),
        ..request.clone()
    };
    let replay = store.unchanged(|store| store.resume(&expired).unwrap());
    assert_eq!(replay.job, first.job);
    // A new chunk past its deadline is refused before the fence is read:
    // the request below also has a stale fence.
    let late = ResumeErasureRequest {
        idempotency_key: "k2".into(),
        deadline_ms: Some(T0 + 1),
        ..request.clone()
    };
    let error = store.unchanged(|store| error_of(store.resume(&late)));
    assert!(matches!(error, MemoryError::DeadlineExceeded), "{error:?}");
    // The deadline is exclusive: now one before it is allowed.
    let fresh = ResumeErasureRequest {
        deadline_ms: Some(T0 + 3),
        now_ms: T0 + 2,
        ..store.request(&scope, "k2", T0 + 2)
    };
    assert!(!store.resume(&fresh).unwrap().replayed);
}

/// Mutant: probe the missing job before the receipt. A receipt beside no
/// job is damage (`InvalidBundle`), a missing job alone is `MissingMemory`.
#[test]
fn mutant_missing_job_is_probed_after_the_receipt_and_the_key_after_both() {
    let mut store = Store::plain();
    let scope = scope();
    three_chunk_scope(&mut store, &scope);
    // No job yet.
    let probe = ResumeErasureRequest {
        scope: scope.clone(),
        job_key: job_key(&scope.key()),
        idempotency_key: "k1".into(),
        expected_revision: 1,
        generation: 1,
        resume_token: token(&scope, Some(BINDING), &job_key(&scope.key()), 1, 1, 0),
        now_ms: T0 + 1,
        receipt_window_ms: WINDOW,
        deadline_ms: None,
    };
    let error = store.unchanged(|store| error_of(store.resume(&probe)));
    assert!(matches!(error, MemoryError::MissingMemory), "{error:?}");
    let error = store.unchanged(|store| error_of(store.try_job(&scope)));
    assert!(matches!(error, MemoryError::MissingMemory), "{error:?}");
    store.begin(&scope);
    // A job key that is not this scope's, beside a v1 job and no receipt.
    let wrong = ResumeErasureRequest {
        job_key: "receipt:v1:other".into(),
        ..store.request(&scope, "k1", T0 + 1)
    };
    let error = store.unchanged(|store| error_of(store.resume(&wrong)));
    assert!(matches!(error, MemoryError::MissingMemory), "{error:?}");
    let request = store.request(&scope, "k1", T0 + 1);
    store.resume(&request).unwrap();
    // The same wrong key beside a live receipt: the key decides first.
    let wrong = ResumeErasureRequest {
        job_key: "receipt:v1:other".into(),
        ..request.clone()
    };
    let error = store.unchanged(|store| error_of(store.resume(&wrong)));
    assert!(matches!(error, MemoryError::MissingMemory), "{error:?}");
    // The job vanishes: its receipt is damage, an unused key is not.
    let job = job_key(&scope.key());
    store.raw(vec![GraphOp::DeleteNode {
        key: job.clone(),
        expected: Expected::Any,
    }]);
    let error = store.unchanged(|store| error_of(store.resume(&request)));
    assert!(matches!(error, MemoryError::InvalidBundle), "{error:?}");
    let fresh = ResumeErasureRequest {
        idempotency_key: "k2".into(),
        ..request
    };
    let error = store.unchanged(|store| error_of(store.resume(&fresh)));
    assert!(matches!(error, MemoryError::MissingMemory), "{error:?}");
}

/// Mutant: require E1's revision equality for the begin receipt of a v2
/// job. Also E1's other accessors over a v2 job.
#[test]
fn e1_surfaces_over_a_progressed_job() {
    let mut store = Store::plain();
    let scope = scope();
    three_chunk_scope(&mut store, &scope);
    let begun = store.begin(&scope);
    assert!(!begun.replayed);
    store.next(&scope, "k1", T0 + 1);
    // `erasure_status` is E1's frozen accessor: it knows only a v1 job.
    let error = store.unchanged(|store| {
        error_of(store.reader().erasure_status(&ErasureStatusRequest {
            scope: scope.clone(),
        }))
    });
    assert!(matches!(error, MemoryError::InvalidBundle), "{error:?}");
    // The begin replays its original view, at the begin receipt's revision
    // (below the job's, which a chunk has since raised).
    let replay = store.unchanged(|store| store.begin_with(&scope, "begin", T0));
    assert!(replay.replayed);
    assert_eq!(replay.status.revision, begun.status.revision);
    assert_eq!(replay.status.deleted_count, 0);
    assert!(store.job(&scope).revision > begun.status.revision);
    store.next(&scope, "k2", T0 + 2);
    store.next(&scope, "k3", T0 + 3);
    // Completed: the job is `logical` and the begin still replays.
    assert_eq!(store.job(&scope).state, ErasureState::Logical);
    let replay = store.unchanged(|store| store.begin_with(&scope, "begin", T0));
    assert!(replay.replayed);
    assert_eq!(replay.status.revision, begun.status.revision);
    // Another key finds the job (E1 section 2): `JobConflict`.
    let error = store.unchanged(|store| {
        let expected = store.reader().scope_revision(&scope).unwrap();
        error_of(store.writer().begin_erasure(&EraseScopeRequest {
            scope: scope.clone(),
            idempotency_key: "begin-2".into(),
            expected_scope_revision: expected,
            now_ms: T0,
            receipt_window_ms: WINDOW,
            deadline_ms: None,
        }))
    });
    assert!(matches!(error, MemoryError::JobConflict), "{error:?}");
}

/// Mutants: count guards wrongly (4,096 versus 4,095 or 4,097). One hub
/// memory costs `3 + extra + 1` operations and `2` guards, so 4,090 extra
/// edges are exactly 4,096 and 4,091 are 4,097.
#[test]
fn mutant_fit_rule_is_exact_at_4096_operations() {
    for (extra, fits) in [(4_090, true), (4_091, false)] {
        let mut store = Store::plain();
        let scope = scope();
        store.provision(&scope);
        store.remember(&scope, vec![item("h", "hub")]);
        store.hub(&scope, "h", extra);
        store.begin(&scope);
        let request = store.request(&scope, "k1", T0 + 1);
        if fits {
            let result = store.resume(&request).unwrap();
            assert_eq!(
                result.chunk,
                ChunkSummary {
                    memories_deleted: 1,
                    remaining_after: Some(0)
                },
                "{extra}"
            );
            assert_eq!(result.job.state, ErasureState::Logical);
            assert_eq!(store.edge_count("sink-h-0"), 0);
            assert!(store.live(&scope).is_empty());
        } else {
            let before = store.job(&scope);
            // The first candidate alone does not fit: the job stops, and
            // retrying cannot change that.
            for attempt in 0..2 {
                let error = store.unchanged(|store| error_of(store.resume(&request)));
                assert!(
                    matches!(error, MemoryError::OperationLimit),
                    "{extra} attempt {attempt}: {error:?}"
                );
            }
            assert_eq!(store.job(&scope), before);
            assert_eq!(store.live(&scope), ["h"]);
        }
    }
}

/// Mutant: count a shared edge twice. Two memories joined by one edge cost
/// `3 + (Ka + Kb + 1) + 2` operations and `3` guards: `Ka + Kb = 4,087` is
/// exactly 4,096 and counts the shared edge once.
#[test]
fn mutant_an_edge_shared_by_two_chunk_members_counts_once() {
    for (extra, both) in [(4_087usize, true), (4_088, false)] {
        let mut store = Store::plain();
        let scope = scope();
        store.provision(&scope);
        store.remember(&scope, vec![item("a", "a"), item("b", "b")]);
        let (a_key, b_key) = (memory_key(&scope.key(), "a"), memory_key(&scope.key(), "b"));
        store.hub(&scope, "a", extra / 2);
        store.hub(&scope, "b", extra - extra / 2);
        store.raw(vec![GraphOp::CreateEdge {
            key: None,
            source: a_key.clone(),
            relationship: "SHARED".into(),
            destination: b_key.clone(),
            properties: Metadata::new(),
        }]);
        store.begin(&scope);
        let result = store.next(&scope, "k1", T0 + 1);
        if both {
            assert_eq!(
                result.chunk,
                ChunkSummary {
                    memories_deleted: 2,
                    remaining_after: Some(0)
                },
                "{extra}"
            );
            assert_eq!(result.job.state, ErasureState::Logical);
            assert!(store.live(&scope).is_empty());
        } else {
            assert_eq!(
                result.chunk,
                ChunkSummary {
                    memories_deleted: 1,
                    remaining_after: Some(1)
                },
                "{extra}"
            );
            assert_eq!(result.job.state, ErasureState::Pending);
            assert_eq!(store.live(&scope), ["b"]);
            // The shared edge went with `a`; the rest of `b`'s edges stay.
            assert_eq!(store.edge_count(&b_key), extra - extra / 2);
        }
    }
}

/// A memory of more than 4,096 edges can never fit: it ends a chunk, and as
/// the first candidate it stops the job without skipping it. Its content is
/// not validated, so the stop is `OperationLimit`, not `InvalidBundle`.
#[test]
fn a_hub_of_more_than_4096_edges_stops_the_job_without_skipping_anything() {
    let mut store = Store::plain();
    let scope = scope();
    store.provision(&scope);
    store.remember(&scope, vec![item("small", "s")]);
    store.remember(&scope, vec![item("hub", &"x".repeat(9_000))]);
    let hub_key = memory_key(&scope.key(), "hub");
    store.hub(&scope, "hub", 4_100);
    store.raw(vec![
        GraphOp::CreateNode {
            key: "outside".into(),
            labels: vec!["Other".into()],
            properties: Metadata::new(),
        },
        GraphOp::CreateEdge {
            key: None,
            source: content_key(&hub_key, 1),
            relationship: "STRAY".into(),
            destination: "outside".into(),
            properties: Metadata::new(),
        },
    ]);
    store.begin(&scope);
    let first = store.next(&scope, "k1", T0 + 1);
    assert_eq!(
        first.chunk,
        ChunkSummary {
            memories_deleted: 1,
            remaining_after: Some(1)
        }
    );
    assert_eq!(first.job.state, ErasureState::Pending);
    for attempt in 0..2u64 {
        let request = store.request(&scope, &format!("stuck-{attempt}"), T0 + 2 + attempt);
        let error = store.unchanged(|store| error_of(store.resume(&request)));
        assert!(matches!(error, MemoryError::OperationLimit), "{error:?}");
    }
    assert_eq!(store.live(&scope), ["hub"]);
}

/// Mutant: decode only chunk members instead of the window. The hub fits a
/// frame alone (4,095) and a second memory would not (4,097), so the chunk
/// holds only the hub; the broken second memory is still in the window.
#[test]
fn mutant_the_whole_window_is_decoded_not_only_the_chunk_members() {
    let mut store = Store::plain();
    let scope = scope();
    store.provision(&scope);
    store.remember(&scope, vec![item("h", "h"), item("x", "x")]);
    store.hub(&scope, "h", 4_089);
    let x_key = memory_key(&scope.key(), "x");
    let original = store.node(&x_key).unwrap();
    store.begin(&scope);
    type Fault = Box<dyn Fn(&mut NodeView)>;
    let faults: [(&str, Fault); 4] = [
        (
            "missing required property",
            Box::new(|node| {
                node.properties.remove("content_digest");
            }),
        ),
        (
            "non-numeric revision",
            Box::new(|node| {
                node.properties
                    .insert("memory_revision".into(), string("many"));
            }),
        ),
        (
            "extra label",
            Box::new(|node| {
                node.labels.insert("Extra".into());
            }),
        ),
        (
            "id that does not match the key",
            Box::new(|node| {
                node.properties.insert("id".into(), string("someone-else"));
            }),
        ),
    ];
    for (label, fault) in faults {
        let mut broken = original.clone();
        fault(&mut broken);
        store.raw(vec![upsert(&broken)]);
        let request = store.request(&scope, "k1", T0 + 1);
        let error = store.unchanged(|store| error_of(store.resume(&request)));
        assert!(
            matches!(error, MemoryError::InvalidBundle),
            "{label}: {error:?}"
        );
        store.raw(vec![upsert(&original)]);
    }
    let first = store.next(&scope, "k1", T0 + 2);
    assert_eq!(
        first.chunk,
        ChunkSummary {
            memories_deleted: 1,
            remaining_after: Some(1)
        }
    );
    assert_eq!(store.live(&scope), ["x"]);
}

/// Mutants: order by key instead of node ID; mark `logical` while
/// `remaining` is unknown; the 4,097 cut; guards (2,046 memories per chunk
/// is exactly 4,096). 4,098 memories: the first window holds 4,097, so the
/// first chunk cannot know how many remain.
#[test]
fn mutant_chunks_follow_node_id_order_and_a_full_window_leaves_remaining_unknown() {
    let mut store = Store::plain();
    let scope = scope();
    store.provision(&scope);
    store.remember_many(&scope, "m", 4_098);
    store.begin(&scope);
    let mut requests = Vec::new();
    let mut results = Vec::new();
    for (step, key) in ["k1", "k2", "k3"].into_iter().enumerate() {
        let request = store.request(&scope, key, T0 + 1 + step as u64);
        results.push(store.resume(&request).unwrap());
        requests.push(request);
        if step == 0 {
            // The 2,046 oldest memories went; the last 2,052 remain.
            let live = store.live(&scope);
            assert_eq!(live.len(), 2_052);
            assert_eq!(live.first().map(String::as_str), Some("m02046"));
            assert_eq!(live.last().map(String::as_str), Some("m04097"));
        }
    }
    assert_eq!(
        results
            .iter()
            .map(|result| result.chunk)
            .collect::<Vec<_>>(),
        [
            ChunkSummary {
                memories_deleted: 2_046,
                remaining_after: None
            },
            ChunkSummary {
                memories_deleted: 2_046,
                remaining_after: Some(6)
            },
            ChunkSummary {
                memories_deleted: 6,
                remaining_after: Some(0)
            },
        ]
    );
    assert_eq!(results[0].job.state, ErasureState::Pending);
    assert_eq!(results[0].job.remaining, None);
    assert_eq!(results[1].job.state, ErasureState::Pending);
    assert_eq!(results[2].job.state, ErasureState::Logical);
    assert_eq!(results[2].job.deleted_count, 4_098);
    assert_eq!(results[2].job.chunks, 3);
    assert!(store.live(&scope).is_empty());
    // The bounds of the stored numbers hold at their exact edges: a chunk of
    // 2,046 memories and a total of 2 * 2,046 decode and replay.
    for (request, original) in requests.iter().zip(&results) {
        let replay = store.unchanged(|store| store.resume(request).unwrap());
        assert!(replay.replayed);
        assert_eq!(replay.job, original.job);
    }
}

/// Sections 5 and 7 (R14, R15): a memory hidden by TTL at the request clock
/// is not a candidate; the frame's opening reclaim removes it without
/// counting it; reclaim leaves its content nodes behind, the residue E4
/// sweeps.
#[test]
fn a_ttl_hidden_memory_is_not_a_candidate_and_leaves_its_content_as_residue() {
    let mut store = Store::plain();
    let scope = scope();
    store.provision(&scope);
    store.remember(&scope, vec![item("a", "a")]);
    let mut timed = item("t", &"x".repeat(9_000));
    timed.expiry_ms = Some(T0 + 100);
    store.remember(&scope, vec![timed]);
    let t_key = memory_key(&scope.key(), "t");
    store.begin(&scope);
    assert!(store.visible_at(&t_key, T0 + 50));
    assert!(!store.visible_at(&t_key, T0 + 500));
    let result = store.next(&scope, "k1", T0 + 500);
    assert_eq!(
        result.chunk,
        ChunkSummary {
            memories_deleted: 1,
            remaining_after: Some(0)
        }
    );
    assert_eq!(result.job.state, ErasureState::Logical);
    assert_eq!(result.job.deleted_count, 1);
    assert!(store.node(&t_key).is_none());
    for index in 0..3 {
        let content = store.node(&content_key(&t_key, index));
        assert!(content.is_some(), "content node {index} is residue");
        assert_eq!(store.edge_count(&content_key(&t_key, index)), 0);
    }
}

/// Section 7: a memory that expires after the request clock but before the
/// commit clock is already gone when its planned operation runs. The frame
/// fails (`Storage`), nothing commits, the failure is stable at the same
/// clock, and a retry at a clock past the expiry succeeds.
#[test]
fn a_memory_expiring_between_the_request_and_commit_clocks_fails_the_chunk_whole() {
    let mut store = Store::plain();
    let scope = scope();
    store.provision(&scope);
    let mut timed = item("t", "t");
    timed.expiry_ms = Some(T0 + 100);
    store.remember(&scope, vec![item("a", "a"), timed]);
    let (a_key, t_key) = (memory_key(&scope.key(), "a"), memory_key(&scope.key(), "t"));
    store.begin(&scope);
    let job = store.job(&scope);
    // The harness-only high-water poke: the commit clock is above the
    // request clock.
    store.db_mut().graph_clock = T0 + 200;
    for attempt in 0..2 {
        let request = store.request(&scope, "k1", T0 + 50);
        let error = store.unchanged(|store| error_of(store.resume(&request)));
        assert!(is_storage(&error), "attempt {attempt}: {error:?}");
        assert_eq!(store.job(&scope), job);
        assert!(store.visible_at(&a_key, T0 + 50));
        assert!(store.visible_at(&t_key, T0 + 50));
    }
    let retry = store.request(&scope, "k1", T0 + 300);
    let result = store.resume(&retry).unwrap();
    assert_eq!(
        result.chunk,
        ChunkSummary {
            memories_deleted: 1,
            remaining_after: Some(0)
        }
    );
    assert_eq!(result.job.state, ErasureState::Logical);
}

/// A later chunk completes with nothing deleted when the remaining memories
/// expired between chunks (section 5, Result).
#[test]
fn a_later_chunk_completes_empty_when_the_rest_expired() {
    let mut store = Store::plain();
    let scope = scope();
    store.provision(&scope);
    let mut timed = item("t", "t");
    timed.expiry_ms = Some(T0 + 100);
    store.remember(&scope, vec![item("a", "a"), timed]);
    store.sinks("sink", 2_500);
    for id in ["a", "t"] {
        store.link(&memory_key(&scope.key(), id), "sink", 2_500);
    }
    store.begin(&scope);
    let first = store.next(&scope, "k1", T0 + 1);
    assert_eq!(
        first.chunk,
        ChunkSummary {
            memories_deleted: 1,
            remaining_after: Some(1)
        }
    );
    let second = store.next(&scope, "k2", T0 + 500);
    assert_eq!(
        second.chunk,
        ChunkSummary {
            memories_deleted: 0,
            remaining_after: Some(0)
        }
    );
    assert_eq!(second.job.state, ErasureState::Logical);
    assert_eq!((second.job.deleted_count, second.job.chunks), (1, 2));
    assert_eq!(store.edge_count("sink-0"), 0);
}

/// An application edge into a memory is an ordinary incident edge: it goes
/// with the memory, while the other end stays.
#[test]
fn a_foreign_edge_into_a_memory_is_an_ordinary_incident_edge() {
    let mut store = Store::plain();
    let scope = scope();
    store.provision(&scope);
    store.remember(&scope, vec![item("m", "m")]);
    let m_key = memory_key(&scope.key(), "m");
    store.raw(vec![
        GraphOp::CreateNode {
            key: "outside".into(),
            labels: vec!["Other".into()],
            properties: Metadata::new(),
        },
        GraphOp::CreateEdge {
            key: None,
            source: "outside".into(),
            relationship: "HAS_CONTENT".into(),
            destination: m_key.clone(),
            properties: Metadata::new(),
        },
        GraphOp::CreateEdge {
            key: None,
            source: m_key.clone(),
            relationship: "TOUCHES".into(),
            destination: "outside".into(),
            properties: Metadata::new(),
        },
    ]);
    assert_eq!(store.edge_count(&m_key), 2);
    store.begin(&scope);
    let result = store.next(&scope, "k1", T0 + 1);
    assert_eq!(result.chunk.memories_deleted, 1);
    assert_eq!(result.job.state, ErasureState::Logical);
    assert!(store.node(&m_key).is_none());
    assert!(store.node("outside").is_some());
    assert_eq!(store.edge_count("outside"), 0);
}

type ContentFault = (&'static str, bool, fn(&mut Store, &str, &str));

/// A memory with 9,000 bytes of text (content nodes 0..=2), a job begun.
fn content_store() -> (Store, MemoryScope, String) {
    let mut store = Store::plain();
    let scope = scope();
    store.provision(&scope);
    store.remember(&scope, vec![item("a", &"x".repeat(9_000))]);
    store.raw(vec![GraphOp::CreateNode {
        key: "outside".into(),
        labels: vec!["Other".into()],
        properties: Metadata::new(),
    }]);
    store.begin(&scope);
    let a_key = memory_key(&scope.key(), "a");
    (store, scope, a_key)
}

fn edit_node(store: &mut Store, key: &str, change: impl FnOnce(&mut NodeView)) {
    let mut node = store.node(key).unwrap();
    change(&mut node);
    store.raw(vec![upsert(&node)]);
}

/// Mutants: skip the content edge validation; skip the content node
/// validation; skip the content node's edge check; skip its attachment
/// check. Each fault is `InvalidBundle` (or `ForeignAttachment` for an
/// attachment) and the chunk writes nothing.
#[test]
fn mutant_content_validation_refuses_every_malformed_content_node() {
    let faults: [ContentFault; 12] = [
        (
            "an extra edge leaves a content node",
            false,
            |store, a, _| {
                store.raw(vec![GraphOp::CreateEdge {
                    key: None,
                    source: content_key(a, 1),
                    relationship: "STRAY".into(),
                    destination: "outside".into(),
                    properties: Metadata::new(),
                }]);
            },
        ),
        (
            "an extra edge enters a content node",
            false,
            |store, a, _| {
                store.raw(vec![GraphOp::CreateEdge {
                    key: None,
                    source: "outside".into(),
                    relationship: "STRAY".into(),
                    destination: content_key(a, 0),
                    properties: Metadata::new(),
                }]);
            },
        ),
        ("an attachment on a content node", true, |store, a, _| {
            store.evidence(BINDING, "att");
            store.raw(vec![GraphOp::Attach {
                node: content_key(a, 2),
                collection: BINDING.into(),
                record: evidence_record_id("att"),
            }]);
        }),
        ("a content edge with another key", false, |store, a, _| {
            store.raw(vec![
                GraphOp::DeleteEdge {
                    edge: EdgeRef::Key(content_edge_key(a, 1)),
                    expected: Expected::Any,
                },
                GraphOp::CreateEdge {
                    key: Some("renamed".into()),
                    source: a.into(),
                    relationship: "HAS_CONTENT".into(),
                    destination: content_key(a, 1),
                    properties: Metadata::new(),
                },
            ]);
        }),
        (
            "a content edge to a node with another key",
            false,
            |store, a, scope_key| {
                let mut properties = Metadata::new();
                properties.insert("scope".into(), string(scope_key));
                properties.insert("index".into(), MetadataValue::Integer(7));
                properties.insert("content".into(), string("r"));
                store.raw(vec![
                    GraphOp::CreateNode {
                        key: "rogue".into(),
                        labels: vec![LABEL_CONTENT.into()],
                        properties,
                    },
                    GraphOp::CreateEdge {
                        key: Some(content_edge_key(a, 7)),
                        source: a.into(),
                        relationship: "HAS_CONTENT".into(),
                        destination: "rogue".into(),
                        properties: Metadata::new(),
                    },
                ]);
            },
        ),
        (
            "a content node with an extra label",
            false,
            |store, a, _| {
                edit_node(store, &content_key(a, 0), |node| {
                    node.labels.insert("Extra".into());
                });
            },
        ),
        ("a content node with another label", false, |store, a, _| {
            edit_node(store, &content_key(a, 0), |node| {
                node.labels = ["Other".to_owned()].into_iter().collect();
            });
        }),
        ("a content node of another scope", false, |store, a, _| {
            edit_node(store, &content_key(a, 0), |node| {
                node.properties
                    .insert("scope".into(), string("scope:v1:00"));
            });
        }),
        ("a negative index", false, |store, a, _| {
            edit_node(store, &content_key(a, 0), |node| {
                node.properties
                    .insert("index".into(), MetadataValue::Integer(-1));
            });
        }),
        ("a text index", false, |store, a, _| {
            edit_node(store, &content_key(a, 0), |node| {
                node.properties.insert("index".into(), string("0"));
            });
        }),
        ("a missing index", false, |store, a, _| {
            edit_node(store, &content_key(a, 0), |node| {
                node.properties.remove("index");
            });
        }),
        (
            "an index that disagrees with the keys",
            false,
            |store, a, _| {
                edit_node(store, &content_key(a, 0), |node| {
                    node.properties
                        .insert("index".into(), MetadataValue::Integer(1));
                });
            },
        ),
    ];
    for (label, foreign, apply) in faults {
        let (mut store, scope, a_key) = content_store();
        apply(&mut store, &a_key, &scope.key());
        let job = store.job(&scope);
        let request = store.request(&scope, "k1", T0 + 1);
        let error = store.unchanged(|store| error_of(store.resume(&request)));
        let right = if foreign {
            matches!(error, MemoryError::ForeignAttachment)
        } else {
            matches!(error, MemoryError::InvalidBundle)
        };
        assert!(right, "{label}: {error:?}");
        assert_eq!(store.job(&scope), job, "{label}");
        assert_eq!(store.live(&scope), ["a"], "{label}");
        assert!(store.node(&content_key(&a_key, 0)).is_some(), "{label}");
    }
}

/// The evidence stops of section 5 are A2's and are never skipped.
#[test]
fn evidence_stops_follow_a2_and_nothing_is_skipped() {
    // (name, setup, expected error); `setup` runs before the job begins.
    type Setup = fn(&mut Store, &MemoryScope, &str);
    type Check = fn(&MemoryError) -> bool;
    let cases: [(&str, Setup, Check); 5] = [
        (
            "a job bound to no collection",
            |store, _, _| {
                store.binding = None;
            },
            |error| matches!(error, MemoryError::InvalidEvidence),
        ),
        (
            "evidence attached in another collection",
            |store, _, a| {
                store.evidence(OTHER_COLLECTION, "ev-d");
                store.raw(vec![
                    GraphOp::Detach {
                        node: a.into(),
                        collection: BINDING.into(),
                    },
                    GraphOp::Attach {
                        node: a.into(),
                        collection: OTHER_COLLECTION.into(),
                        record: evidence_record_id("ev-d"),
                    },
                ]);
            },
            |error| matches!(error, MemoryError::ForeignAttachment),
        ),
        (
            "an application attachment on a memory without evidence",
            |store, scope, _| {
                store.remember(scope, vec![item("plain", "p")]);
                store.evidence(BINDING, "stray");
                store.raw(vec![GraphOp::Attach {
                    node: memory_key(&scope.key(), "plain"),
                    collection: BINDING.into(),
                    record: evidence_record_id("stray"),
                }]);
            },
            |error| matches!(error, MemoryError::ForeignAttachment),
        ),
        (
            "an evidence record owned by another node",
            |store, _, a| {
                store.raw(vec![
                    GraphOp::CreateNode {
                        key: "outside".into(),
                        labels: vec!["Other".into()],
                        properties: Metadata::new(),
                    },
                    GraphOp::Detach {
                        node: a.into(),
                        collection: BINDING.into(),
                    },
                    GraphOp::Attach {
                        node: "outside".into(),
                        collection: BINDING.into(),
                        record: evidence_record_id("ev-a"),
                    },
                ]);
            },
            |error| matches!(error, MemoryError::ForeignAttachment),
        ),
        (
            "a memory with two evidence entries",
            |store, _, a| {
                edit_node(store, a, |node| {
                    node.properties.insert(
                        "evidence".into(),
                        string(&format!("{};{}", hex(b"ev-a"), hex(b"ev-b"))),
                    );
                });
            },
            |error| matches!(error, MemoryError::InvalidBundle),
        ),
    ];
    for (label, setup, expected) in cases {
        let mut store = Store::plain();
        let scope = scope();
        store.provision(&scope);
        store.evidence(BINDING, "ev-a");
        store.remember(&scope, vec![evidence_item("a", "ev-a", &scope)]);
        let a_key = memory_key(&scope.key(), "a");
        setup(&mut store, &scope, &a_key);
        store.begin(&scope);
        let job = store.job(&scope);
        let request = store.request(&scope, "k1", T0 + 1);
        let error = store.unchanged(|store| error_of(store.resume(&request)));
        assert!(expected(&error), "{label}: {error:?}");
        assert_eq!(store.job(&scope), job, "{label}");
        assert!(store.visible_at(&a_key, T0 + 1), "{label}");
        assert!(store.has_record(BINDING, "ev-a"), "{label}");
    }
}

/// R5: a record has one owner. A memory that names a record owned by a
/// memory of another scope stops the job with `ForeignAttachment`, and that
/// record stays attached to its owner.
#[test]
fn a_record_owned_by_a_memory_of_another_scope_is_never_detached_or_deleted() {
    let mut store = Store::plain();
    let scope = scope();
    let other = sibling_scope();
    store.provision(&scope);
    store.provision(&other);
    store.evidence(BINDING, "ev-a");
    store.evidence(BINDING, "ev-t");
    store.remember(&scope, vec![evidence_item("a", "ev-a", &scope)]);
    store.remember(&other, vec![evidence_item("t", "ev-t", &other)]);
    let a_key = memory_key(&scope.key(), "a");
    let t_key = memory_key(&other.key(), "t");
    // `a` now names the other scope's record instead of its own.
    store.raw(vec![GraphOp::Detach {
        node: a_key.clone(),
        collection: BINDING.into(),
    }]);
    edit_node(&mut store, &a_key, |node| {
        node.properties
            .insert("evidence".into(), string(&hex(b"ev-t")));
    });
    store.begin(&scope);
    let job = store.job(&scope);
    let request = store.request(&scope, "k1", T0 + 1);
    let error = store.unchanged(|store| error_of(store.resume(&request)));
    assert!(matches!(error, MemoryError::ForeignAttachment), "{error:?}");
    assert_eq!(store.job(&scope), job);
    assert_eq!(store.live(&other), ["t"]);
    assert!(store.has_record(BINDING, "ev-t"));
    assert!(store.has_record(BINDING, "ev-a"));
    let owner = store
        .db()
        .memory_attachment_owner(BINDING, evidence_record_id("ev-t"), T0)
        .unwrap();
    assert_eq!(owner, Some((GRAPH.to_owned(), t_key)));
}

/// Mutant: retain evidence by A2's rule. A survivor that references a
/// deleted memory makes A2 keep its evidence record. A scope erasure leaves
/// no memory behind, so E2 deletes the record in every case.
#[test]
fn mutant_evidence_is_never_retained_even_when_a_survivor_references_it() {
    let mut store = Store::plain();
    let scope = scope();
    store.provision(&scope);
    store.evidence(BINDING, "ev-a");
    store.remember(
        &scope,
        vec![evidence_item("a", "ev-a", &scope), item("b", "b")],
    );
    let mut survivor = item("s", "s");
    survivor.references = vec!["a".into()];
    store.remember(&scope, vec![survivor]);
    store.sinks("sink", 2_500);
    for id in ["a", "b"] {
        store.link(&memory_key(&scope.key(), id), "sink", 2_500);
    }
    store.begin(&scope);
    let first = store.next(&scope, "k1", T0 + 1);
    // `a` alone fills the chunk; `b` and the survivor `s` remain.
    assert_eq!(first.chunk.memories_deleted, 1);
    assert_eq!(store.live(&scope), ["b", "s"]);
    assert!(!store.has_record(BINDING, "ev-a"));
    let second = store.next(&scope, "k2", T0 + 2);
    assert_eq!(second.job.state, ErasureState::Logical);
    assert_eq!(second.job.deleted_count, 3);
}

/// Section 6: after its receipt expires, a request is judged by the fence
/// (`JobConflict`, or `DeadlineExceeded` first), and its key can be reused
/// by a new chunk, which replaces the old receipt.
#[test]
fn an_expired_receipt_is_ignored_and_its_key_is_reusable() {
    let mut store = Store::plain();
    let scope = scope();
    three_chunk_scope(&mut store, &scope);
    store.begin(&scope);
    let first_request = store.request(&scope, "k1", T0 + 1);
    store.resume(&first_request).unwrap();
    let late = T0 + 1 + WINDOW + 5;
    let retry = ResumeErasureRequest {
        now_ms: late,
        ..first_request.clone()
    };
    let error = store.unchanged(|store| error_of(store.resume(&retry)));
    assert!(matches!(error, MemoryError::JobConflict), "{error:?}");
    let expired = ResumeErasureRequest {
        deadline_ms: Some(T0 + 10),
        ..retry.clone()
    };
    let error = store.unchanged(|store| error_of(store.resume(&expired)));
    assert!(matches!(error, MemoryError::DeadlineExceeded), "{error:?}");
    // The key is free again for the current fence.
    let reuse = store.request(&scope, "k1", late);
    let second = store.resume(&reuse).unwrap();
    assert!(!second.replayed);
    assert_eq!(second.job.chunks, 2);
    let replay = store.unchanged(|store| store.resume(&reuse).unwrap());
    assert!(replay.replayed);
    assert_eq!(replay.job, second.job);
    // The key now belongs to the second chunk: the first request conflicts.
    let error = store.unchanged(|store| error_of(store.resume(&retry)));
    assert!(
        matches!(error, MemoryError::IdempotencyConflict),
        "{error:?}"
    );
}

/// The `remember` request that `three_chunk_scope` seeds: its receipt
/// predates every barrier.
fn seed_request(scope: &MemoryScope) -> RememberRequest {
    RememberRequest {
        scope: scope.clone(),
        idempotency_key: "seed-1".into(),
        receipt_window_ms: WINDOW,
        now_ms: T0,
        deadline_ms: None,
        allow_future_observations: false,
        items: vec![item("a", "a"), item("b", "b"), item("c", "c")],
    }
}

fn node_revision(store: &Store, key: &str) -> u64 {
    store.node(key).unwrap().revision
}

fn resume_receipt(scope: &MemoryScope, key: &str) -> String {
    operation_receipt_key("resume-erasure", &scope.key(), key)
}

/// Section 7: a directory checkpoint restates every graph node at one
/// sequence, so after a checkpoint and reopen the job and its receipts share
/// a revision. Mutant: find the latest receipt by revision equality instead
/// of `chunk_no` (older receipts then stop decoding and replaying).
#[test]
fn mutant_latest_receipt_is_found_by_chunk_no_after_a_checkpoint_and_reopen() {
    for backend in [Backend::Directory, Backend::EncryptedDirectory] {
        let mut store = Store::new(backend);
        let scope = scope();
        three_chunk_scope(&mut store, &scope);
        let begin_request = store.begin_request(&scope, "begin", T0);
        store.writer().begin_erasure(&begin_request).unwrap();
        let r1 = store.request(&scope, "k1", T0 + 1);
        let first = store.resume(&r1).unwrap();
        let r2 = store.request(&scope, "k2", T0 + 2);
        let second = store.resume(&r2).unwrap();
        // A receipt of another operation that predates the barrier replays.
        let seeded = store.writer().remember(&seed_request(&scope)).unwrap();
        assert!(seeded.replayed, "{backend:?}");
        let stale = store.request(&scope, "k3", T0 + 3);
        // A later, unrelated commit puts the checkpoint sequence above the
        // job's revision, so the relabelling changes the job's revision.
        store.raw(vec![GraphOp::CreateNode {
            key: "marker".into(),
            labels: vec!["Other".into()],
            properties: Metadata::new(),
        }]);
        assert!(store.db_mut().checkpoint().unwrap().is_some());
        store.reopen();
        // The graph revisions were relabelled: one value for every node.
        let job_name = job_key(&scope.key());
        let revision = node_revision(&store, &job_name);
        assert_eq!(
            node_revision(&store, &resume_receipt(&scope, "k1")),
            revision,
            "{backend:?}"
        );
        assert_eq!(
            node_revision(&store, &resume_receipt(&scope, "k2")),
            revision,
            "{backend:?}"
        );
        let job = store.job(&scope);
        assert_eq!(
            (job.state, job.chunks, job.deleted_count, job.remaining),
            (ErasureState::Pending, 2, 2, Some(1))
        );
        assert_eq!(job.revision, revision);
        assert_ne!(job.revision, second.job.revision, "{backend:?}");
        // The token read before the checkpoint is stale.
        let error = store.unchanged(|store| error_of(store.resume(&stale)));
        assert!(matches!(error, MemoryError::JobConflict), "{error:?}");
        // Older keys keep replaying their original chunk numbers.
        for (request, original) in [(&r1, &first), (&r2, &second)] {
            let replay = store.unchanged(|store| store.resume(request).unwrap());
            assert!(replay.replayed, "{backend:?}");
            assert_eq!(replay.chunk, original.chunk, "{backend:?}");
        }
        // The begin receipt of a v2 job is checked by digest alone.
        let begin = store.unchanged(|store| store.writer().begin_erasure(&begin_request).unwrap());
        assert!(begin.replayed, "{backend:?}");
        // The receipt that predates the barrier fails closed (E4 reconciles).
        let error =
            store.unchanged(|store| error_of(store.writer().remember(&seed_request(&scope))));
        assert!(
            matches!(error, MemoryError::DeletionPending),
            "{backend:?}: {error:?}"
        );
        // The job resumes from a fresh read and completes.
        let third = store.next(&scope, "k3", T0 + 4);
        assert_eq!(third.job.state, ErasureState::Logical);
        assert_eq!(third.job.deleted_count, 3);
        store.reopen();
        assert_eq!(store.job(&scope).state, ErasureState::Logical);
        let replay = store.unchanged(|store| store.resume(&r1).unwrap());
        assert!(replay.replayed);
        assert_eq!(replay.chunk, first.chunk);
    }
}

/// Section 7: compaction renumbers graph revisions per body, so nothing may
/// depend on a relation between them. The job, its receipts and the begin
/// keep working; a receipt that predates the barrier either replays or
/// fails closed.
#[test]
fn a_compaction_keeps_the_job_and_every_receipt_usable() {
    for backend in [Backend::Plain, Backend::Encrypted] {
        let mut store = Store::new(backend);
        let scope = scope();
        three_chunk_scope(&mut store, &scope);
        let begin_request = store.begin_request(&scope, "begin", T0);
        store.writer().begin_erasure(&begin_request).unwrap();
        let r1 = store.request(&scope, "k1", T0 + 1);
        let first = store.resume(&r1).unwrap();
        let r2 = store.request(&scope, "k2", T0 + 2);
        let second = store.resume(&r2).unwrap();
        store.compact();
        let job = store.job(&scope);
        assert_eq!(
            (job.state, job.chunks, job.deleted_count, job.remaining),
            (ErasureState::Pending, 2, 2, Some(1)),
            "{backend:?}"
        );
        for (request, original) in [(&r1, &first), (&r2, &second)] {
            let replay = store.unchanged(|store| store.resume(request).unwrap());
            assert!(replay.replayed, "{backend:?}");
            assert_eq!(replay.chunk, original.chunk, "{backend:?}");
        }
        let begin = store.unchanged(|store| store.writer().begin_erasure(&begin_request).unwrap());
        assert!(begin.replayed, "{backend:?}");
        let seeded = store.unchanged(|store| store.writer().remember(&seed_request(&scope)));
        assert!(
            matches!(&seeded, Ok(result) if result.replayed)
                || matches!(&seeded, Err(MemoryError::DeletionPending)),
            "{backend:?}: {seeded:?}"
        );
        let third = store.next(&scope, "k3", T0 + 4);
        assert_eq!(third.job.state, ErasureState::Logical, "{backend:?}");
        assert_eq!(third.job.deleted_count, 3);
        store.reopen();
        assert_eq!(store.job(&scope), third.job);
    }
}

/// Section 7 (Reopen): every step round-trips on all four backends with a
/// reopen between the steps, including a pre-append failure (nothing
/// changes) and a reopen straight after the append (the whole chunk is
/// there).
#[test]
fn every_step_survives_reopen_on_all_backends() {
    for backend in BACKENDS {
        let mut store = Store::new(backend);
        let scope = scope();
        let other = sibling_scope();
        store.provision(&scope);
        store.provision(&other);
        store.evidence(BINDING, "ev-a");
        store.remember(
            &scope,
            vec![evidence_item("a", "ev-a", &scope), item("b", "b")],
        );
        store.remember(&other, vec![item("keep", "kept")]);
        store.sinks("sink", 2_100);
        for id in ["a", "b"] {
            store.link(&memory_key(&scope.key(), id), "sink", 2_100);
        }
        store.begin(&scope);
        let begun = store.job(&scope);
        store.reopen();
        assert_eq!(store.job(&scope), begun, "{backend:?}");

        // A pre-append failure leaves everything as it was.
        let r1 = store.request(&scope, "k1", T0 + 1);
        FAIL_NEXT_WRITE_BATCH.with(|fail| fail.set(true));
        let error = store.unchanged(|store| error_of(store.resume(&r1)));
        assert!(is_storage(&error), "{backend:?}: {error:?}");
        assert!(!FAIL_NEXT_WRITE_BATCH.with(|fail| fail.replace(false)));
        store.reopen();
        assert_eq!(store.job(&scope), begun, "{backend:?}");
        assert_eq!(store.live(&scope), ["a", "b"], "{backend:?}");
        assert!(store.has_record(BINDING, "ev-a"), "{backend:?}");

        // The chunk, then a reopen straight after the append.
        let first = store.resume(&r1).unwrap();
        assert_eq!(first.chunk.memories_deleted, 1, "{backend:?}");
        store.reopen();
        assert_eq!(store.job(&scope), first.job, "{backend:?}");
        assert!(!store.has_record(BINDING, "ev-a"), "{backend:?}");
        let replay = store.unchanged(|store| store.resume(&r1).unwrap());
        assert!(replay.replayed, "{backend:?}");
        assert_eq!(replay.job, first.job, "{backend:?}");
        store.reopen();

        let r2 = store.request(&scope, "k2", T0 + 2);
        let second = store.resume(&r2).unwrap();
        assert_eq!(second.job.state, ErasureState::Logical, "{backend:?}");
        store.reopen();
        assert_eq!(store.job(&scope), second.job, "{backend:?}");
        for (request, original) in [(&r1, &first), (&r2, &second)] {
            let replay = store.unchanged(|store| store.resume(request).unwrap());
            assert!(replay.replayed, "{backend:?}");
            assert_eq!(replay.job, original.job, "{backend:?}");
        }
        // The barrier stays; the other scope is unbarred.
        let blocked = RememberRequest {
            scope: scope.clone(),
            idempotency_key: "after".into(),
            receipt_window_ms: WINDOW,
            now_ms: T0 + 3,
            deadline_ms: None,
            allow_future_observations: false,
            items: vec![item("again", "again")],
        };
        assert!(
            matches!(
                store.writer().remember(&blocked),
                Err(MemoryError::DeletionPending)
            ),
            "{backend:?}"
        );
        store.remember(&other, vec![item("more", "more")]);
        store.reopen();
        assert_eq!(store.live(&other), ["keep", "more"], "{backend:?}");
    }
}

enum Target {
    Job,
    Receipt(&'static str),
    Outcome(&'static str),
}

enum Edit {
    Set(&'static str, MetadataValue),
    Remove(&'static str),
    Labels(&'static [&'static str]),
}

struct Row {
    name: &'static str,
    target: Target,
    edits: Vec<Edit>,
    /// A tuple mismatch is `DigestCollision`, every other defect
    /// `InvalidBundle`.
    collision: bool,
}

fn row(name: &'static str, target: Target, edits: Vec<Edit>) -> Row {
    Row {
        name,
        target,
        edits,
        collision: false,
    }
}

fn set(name: &'static str, value: &str) -> Edit {
    Edit::Set(name, string(value))
}

fn target_key(scope: &MemoryScope, target: &Target) -> String {
    match target {
        Target::Job => job_key(&scope.key()),
        Target::Receipt(key) => resume_receipt(scope, key),
        Target::Outcome(key) => receipt_outcome_key(&resume_receipt(scope, key), 0),
    }
}

/// An outcome text from `[chunk_no, deleted, total, remaining, state]`.
fn outcome_of(job: &str, parts: [&str; 5]) -> String {
    format!(
        "erase-chunk:v1\n{job}\n{}\n{}\n{}\n{}\n{}\n",
        parts[0], parts[1], parts[2], parts[3], parts[4]
    )
}

fn outcome_row(name: &'static str, key: &'static str, body: String) -> Row {
    row(name, Target::Outcome(key), vec![set("outcome", &body)])
}

/// One row for each decoder rule of section 4 (DJ1-DJ6, DR1-DR3, DO1-DO4),
/// each a single repairable defect of a job that has run two chunks
/// (`e2_chunks` 2, 2 deleted, 1 remaining; receipts `k1` and `k2`).
fn decoder_rows(j: &str) -> Vec<Row> {
    use Target::{Job, Outcome, Receipt};
    let int = MetadataValue::Integer;
    let o = |parts: [&str; 5]| outcome_of(j, parts);
    let mut rows = vec![
        row("dj1 extra property", Job, vec![set("x", "1")]),
        row("dj1 missing property", Job, vec![Edit::Remove("e2_chunks")]),
        row("dj1 wrong type", Job, vec![Edit::Set("generation", int(1))]),
        row(
            "dj1 extra label",
            Job,
            vec![Edit::Labels(&[LABEL_JOB, "Extra"])],
        ),
        row("dj1 other label", Job, vec![Edit::Labels(&["Other"])]),
        row(
            "dj2 unknown schema",
            Job,
            vec![set("e1_schema", "erase-scope:v3")],
        ),
        row("dj2 scope", Job, vec![set("scope", "scope:v1:00")]),
        row("dj2 collection", Job, vec![set("e1_collection", "other")]),
        row("dj3 digest shape", Job, vec![set("begin_digest", "v1:xyz")]),
        row("dj3 generation", Job, vec![set("generation", "2")]),
        row(
            "dj3 visibility",
            Job,
            vec![set("logical_visibility", "visible")],
        ),
        row(
            "dj3 created",
            Job,
            vec![Edit::Set("created_at_ms", int(-1))],
        ),
        row("dj4 state", Job, vec![set("state", "done")]),
        row(
            "dj4 deleted leading zero",
            Job,
            vec![set("deleted_count", "02")],
        ),
        row("dj4 deleted empty", Job, vec![set("deleted_count", "")]),
        row("dj4 deleted sign", Job, vec![set("deleted_count", "+2")]),
        row(
            "dj4 remaining above 4095",
            Job,
            vec![set("remaining", "4096")],
        ),
        row(
            "dj4 remaining leading zero",
            Job,
            vec![set("remaining", "01")],
        ),
        row("dj4 remaining negative", Job, vec![set("remaining", "-1")]),
        row("dj4 chunks zero", Job, vec![set("e2_chunks", "0")]),
        row("dj4 chunks leading zero", Job, vec![set("e2_chunks", "02")]),
        row(
            "dj5 logical with remaining",
            Job,
            vec![set("state", "logical")],
        ),
        row(
            "dj5 pending with nothing remaining",
            Job,
            vec![set("remaining", "0")],
        ),
        row(
            "dj6 deleted above 2046 per chunk",
            Job,
            vec![set("deleted_count", "4093")],
        ),
        row("dr1 extra property", Receipt("k2"), vec![set("x", "1")]),
        row(
            "dr1 missing property",
            Receipt("k2"),
            vec![Edit::Remove("digest")],
        ),
        row(
            "dr1 wrong type",
            Receipt("k2"),
            vec![set("outcome_chunks", "1")],
        ),
        row(
            "dr1 extra label",
            Receipt("k2"),
            vec![Edit::Labels(&[LABEL_RECEIPT, "Extra"])],
        ),
        row(
            "dr2 the begin receipt's schema",
            Receipt("k2"),
            vec![set("e1_schema", "erase-scope:v1")],
        ),
        row(
            "dr2 scope",
            Receipt("k2"),
            vec![set("scope", "scope:v1:00")],
        ),
        row(
            "dr2 collection",
            Receipt("k2"),
            vec![set("e1_collection", "other")],
        ),
        row(
            "dr3 digest shape",
            Receipt("k2"),
            vec![set("digest", "v1:abc")],
        ),
        row(
            "dr3 job key",
            Receipt("k2"),
            vec![set("job_key", "receipt:v1:other")],
        ),
        row(
            "dr3 outcome chunks",
            Receipt("k2"),
            vec![Edit::Set("outcome_chunks", int(2))],
        ),
        row(
            "dr3 expiry zero",
            Receipt("k2"),
            vec![set("expires_at_ms", "0")],
        ),
        row(
            "dr3 expiry leading zero",
            Receipt("k2"),
            vec![set("expires_at_ms", "0123")],
        ),
        row(
            "dr3 expiry word",
            Receipt("k2"),
            vec![set("expires_at_ms", "never")],
        ),
        row(
            "do1 receipt key",
            Outcome("k2"),
            vec![set("receipt_key", "wrong")],
        ),
        row("do1 index", Outcome("k2"), vec![Edit::Set("index", int(1))]),
        row("do1 extra property", Outcome("k2"), vec![set("x", "1")]),
        row(
            "do1 extra label",
            Outcome("k2"),
            vec![Edit::Labels(&[LABEL_RECEIPT_OUTCOME, "Extra"])],
        ),
        outcome_row(
            "do2 no final newline",
            "k2",
            o(["2", "1", "2", "1", "pending"]).trim_end().to_owned(),
        ),
        outcome_row(
            "do2 extra line",
            "k2",
            format!("{}extra\n", o(["2", "1", "2", "1", "pending"])),
        ),
        outcome_row(
            "do2 six lines",
            "k2",
            format!("erase-chunk:v1\n{j}\n2\n1\n2\n1\n"),
        ),
        outcome_row(
            "do2 schema line",
            "k2",
            o(["2", "1", "2", "1", "pending"]).replacen("erase-chunk:v1", "erase-chunk:v2", 1),
        ),
        outcome_row(
            "do2 leading zero",
            "k2",
            o(["02", "1", "2", "1", "pending"]),
        ),
        outcome_row(
            "do2 carriage returns",
            "k2",
            o(["2", "1", "2", "1", "pending"]).replace('\n', "\r\n"),
        ),
        outcome_row(
            "do2 remaining word",
            "k2",
            o(["2", "1", "2", "maybe", "pending"]),
        ),
        outcome_row("do2 state word", "k2", o(["2", "1", "2", "1", "done"])),
        outcome_row(
            "do3 another job key",
            "k2",
            o(["2", "1", "2", "1", "pending"]).replacen(j, "receipt:v1:other", 1),
        ),
        outcome_row("do3 chunk zero", "k2", o(["0", "1", "2", "1", "pending"])),
        outcome_row(
            "do3 deleted above 2046",
            "k2",
            o(["2", "2047", "2047", "1", "pending"]),
        ),
        outcome_row(
            "do3 total below deleted",
            "k2",
            o(["2", "1", "0", "1", "pending"]),
        ),
        outcome_row(
            "do3 total above 2046 per chunk",
            "k2",
            o(["2", "1", "4093", "1", "pending"]),
        ),
        outcome_row(
            "do3 remaining above 4095",
            "k2",
            o(["2", "1", "2", "4096", "pending"]),
        ),
        outcome_row(
            "do3 logical with remaining",
            "k2",
            o(["2", "1", "2", "1", "logical"]),
        ),
        outcome_row(
            "do3 pending with nothing remaining",
            "k2",
            o(["2", "1", "2", "0", "pending"]),
        ),
        outcome_row(
            "do3 empty chunk with remaining",
            "k2",
            o(["2", "0", "2", "1", "pending"]),
        ),
        outcome_row(
            "do3 first chunk total differs",
            "k1",
            o(["1", "1", "2", "2", "pending"]),
        ),
        outcome_row(
            "do4 chunk above the job's",
            "k2",
            o(["3", "1", "2", "1", "pending"]),
        ),
        outcome_row(
            "do4 latest remaining differs",
            "k2",
            o(["2", "1", "2", "2", "pending"]),
        ),
        outcome_row(
            "do4 latest remaining unknown",
            "k2",
            o(["2", "1", "2", "unknown", "pending"]),
        ),
        outcome_row(
            "do4 latest total differs",
            "k2",
            o(["2", "1", "1", "1", "pending"]),
        ),
        outcome_row(
            "do4 logical but not the latest",
            "k1",
            o(["1", "1", "1", "0", "logical"]),
        ),
        outcome_row(
            "do4 older total above the job's",
            "k1",
            o(["1", "3", "3", "2", "pending"]),
        ),
    ];
    for target in [Job, Receipt("k2")] {
        let name = if matches!(target, Job) {
            "dj2 tuple"
        } else {
            "dr2 tuple"
        };
        rows.push(Row {
            collision: true,
            ..row(name, target, vec![set("e1_scope_tuple", "00")])
        });
    }
    rows
}

/// Every decoder rule rejects its single defect with the documented error,
/// and an accepted bound stays accepted. The job and the receipts of the
/// base are valid again after each repair.
#[test]
fn every_decoder_rule_of_section_4_rejects_its_defect() {
    let mut store = Store::plain();
    let scope = scope();
    three_chunk_scope(&mut store, &scope);
    store.begin(&scope);
    let r1 = store.request(&scope, "k1", T0 + 1);
    store.resume(&r1).unwrap();
    let r2 = store.request(&scope, "k2", T0 + 2);
    store.resume(&r2).unwrap();
    let rz = store.request(&scope, "kz", T0 + 3);
    let job_name = job_key(&scope.key());
    for row in decoder_rows(&job_name) {
        let key = target_key(&scope, &row.target);
        let original = store.node(&key).unwrap();
        let mut changed = original.clone();
        for edit in &row.edits {
            match edit {
                Edit::Set(name, value) => {
                    changed.properties.insert((*name).into(), value.clone());
                }
                Edit::Remove(name) => {
                    changed.properties.remove(*name);
                }
                Edit::Labels(labels) => {
                    changed.labels = labels.iter().map(|label| (*label).to_owned()).collect();
                }
            }
        }
        store.raw(vec![upsert(&changed)]);
        let request = match row.target {
            Target::Job => &rz,
            Target::Receipt("k1") | Target::Outcome("k1") => &r1,
            _ => &r2,
        };
        let error = store.unchanged(|store| error_of(store.resume(request)));
        let right = if row.collision {
            matches!(error, MemoryError::DigestCollision)
        } else {
            matches!(error, MemoryError::InvalidBundle)
        };
        assert!(right, "{}: {error:?}", row.name);
        match row.target {
            Target::Job => {
                let error = error_of(store.try_job(&scope));
                let right = if row.collision {
                    matches!(error, MemoryError::DigestCollision)
                } else {
                    matches!(error, MemoryError::InvalidBundle)
                };
                assert!(right, "{} (erasure_job): {error:?}", row.name);
            }
            _ => assert!(store.try_job(&scope).is_ok(), "{}", row.name),
        }
        store.raw(vec![upsert(&original)]);
        assert!(store.try_job(&scope).is_ok(), "{} repaired", row.name);
        let replay = store.unchanged(|store| store.resume(&r2).unwrap());
        assert!(replay.replayed, "{} repaired", row.name);
    }

    // The accepted bounds: 4,092 deleted over two chunks and 4,095 remaining.
    for (name, value) in [("deleted_count", "4092"), ("remaining", "4095")] {
        let original = store.node(&job_name).unwrap();
        let mut at_bound = original.clone();
        at_bound.properties.insert(name.into(), string(value));
        store.raw(vec![upsert(&at_bound)]);
        let job = store.try_job(&scope);
        assert!(job.is_ok(), "{name} {value}: {job:?}");
        store.raw(vec![upsert(&original)]);
    }

    // DR4: the outcome node 0 must exist and the node 1 must not.
    let receipt = resume_receipt(&scope, "k2");
    let (outcome0, outcome1) = (
        receipt_outcome_key(&receipt, 0),
        receipt_outcome_key(&receipt, 1),
    );
    let original = store.node(&outcome0).unwrap();
    store.raw(vec![GraphOp::DeleteNode {
        key: outcome0.clone(),
        expected: Expected::Any,
    }]);
    let error = store.unchanged(|store| error_of(store.resume(&r2)));
    assert!(matches!(error, MemoryError::InvalidBundle), "dr4 {error:?}");
    store.raw(vec![upsert(&original)]);
    let mut extra = original.clone();
    extra.key = outcome1.clone();
    extra
        .properties
        .insert("index".into(), MetadataValue::Integer(1));
    store.raw(vec![upsert(&extra)]);
    let error = store.unchanged(|store| error_of(store.resume(&r2)));
    assert!(matches!(error, MemoryError::InvalidBundle), "dr4 {error:?}");
    store.raw(vec![GraphOp::DeleteNode {
        key: outcome1,
        expected: Expected::Any,
    }]);
    assert!(store.unchanged(|store| store.resume(&r2).unwrap()).replayed);

    // DR6: an outcome node beside no receipt is damage, at either index.
    let orphan = resume_receipt(&scope, "orphan");
    let request = ResumeErasureRequest {
        idempotency_key: "orphan".into(),
        ..rz.clone()
    };
    for index in [0, 1] {
        let mut stray = original.clone();
        stray.key = receipt_outcome_key(&orphan, index);
        stray
            .properties
            .insert("receipt_key".into(), string(&orphan));
        stray
            .properties
            .insert("index".into(), MetadataValue::Integer(index as i64));
        store.raw(vec![upsert(&stray)]);
        let error = store.unchanged(|store| error_of(store.resume(&request)));
        assert!(
            matches!(error, MemoryError::InvalidBundle),
            "dr6 index {index}: {error:?}"
        );
        store.raw(vec![GraphOp::DeleteNode {
            key: stray.key.clone(),
            expected: Expected::Any,
        }]);
    }
}

/// Section 2: structural validation runs in the documented order, before any
/// read; then the unprovisioned scope, the tuple and the deadline.
#[test]
fn structural_validation_follows_the_documented_order() {
    let mut store = Store::plain();
    let scope = scope();
    store.provision(&scope);
    store.begin(&scope);
    let good = store.request(&scope, "k1", T0 + 1);
    let mut request = ResumeErasureRequest {
        scope: MemoryScope {
            tenant: String::new(),
            ..scope.clone()
        },
        idempotency_key: String::new(),
        now_ms: i64::MAX as u64 + 1,
        receipt_window_ms: 0,
        expected_revision: 0,
        generation: 0,
        job_key: String::new(),
        resume_token: "t1:ABC".into(),
        deadline_ms: Some(0),
    };
    let mut seen = Vec::new();
    let mut step = |store: &mut Store, request: &ResumeErasureRequest| {
        let error = store.unchanged(|store| error_of(store.resume(request)));
        seen.push(format!("{error:?}"));
    };
    step(&mut store, &request);
    request.scope = scope.clone();
    step(&mut store, &request);
    request.idempotency_key = "k1".into();
    step(&mut store, &request);
    request.now_ms = T0 + 1;
    step(&mut store, &request);
    request.receipt_window_ms = u64::MAX;
    step(&mut store, &request);
    request.receipt_window_ms = WINDOW;
    step(&mut store, &request);
    request.expected_revision = good.expected_revision;
    step(&mut store, &request);
    request.generation = 1;
    step(&mut store, &request);
    request.job_key = good.job_key.clone();
    step(&mut store, &request);
    // A token of the right shape but uppercase hex is still a shape error.
    request.resume_token = format!("t1:{}", "A".repeat(64));
    step(&mut store, &request);
    // The right shape, a wrong value: the passed deadline answers first.
    request.resume_token = format!("t1:{}", "0".repeat(64));
    step(&mut store, &request);
    request.deadline_ms = None;
    step(&mut store, &request);
    assert_eq!(
        seen,
        [
            "InvalidScope",
            "InvalidKey",
            "InvalidClock",
            "InvalidReceiptWindow",
            "InvalidReceiptWindow",
            "InvalidTarget",
            "InvalidTarget",
            "InvalidKey",
            "InvalidTarget",
            "InvalidTarget",
            "DeadlineExceeded",
            "JobConflict",
        ]
    );
    // A collection name of 0 or 64 bytes is refused before any read.
    for name in [String::new(), "x".repeat(64)] {
        let mut handle = MemoryMut {
            db: store.db_mut(),
            graph: GRAPH.into(),
            evidence_collection: Some(name.clone()),
        };
        let error = error_of(handle.resume_erasure(&good));
        assert!(
            matches!(error, MemoryError::InvalidBundle),
            "{name:?}: {error:?}"
        );
    }
}

/// An unprovisioned scope is `InvalidScope` and a scope node with another
/// tuple `DigestCollision`, for the read and for the chunk, as in E1.
#[test]
fn an_unprovisioned_scope_and_a_tuple_mismatch_are_refused_like_e1() {
    let mut store = Store::plain();
    let scope = scope();
    let probe = |scope: &MemoryScope| ResumeErasureRequest {
        scope: scope.clone(),
        job_key: job_key(&scope.key()),
        idempotency_key: "k1".into(),
        expected_revision: 1,
        generation: 1,
        resume_token: token(scope, Some(BINDING), &job_key(&scope.key()), 1, 1, 0),
        now_ms: T0,
        receipt_window_ms: WINDOW,
        deadline_ms: None,
    };
    let error = error_of(store.try_job(&scope));
    assert!(matches!(error, MemoryError::InvalidScope), "{error:?}");
    let error = store.unchanged(|store| error_of(store.resume(&probe(&scope))));
    assert!(matches!(error, MemoryError::InvalidScope), "{error:?}");
    store.provision(&scope);
    edit_node(&mut store, &scope.key(), |node| {
        node.properties.insert("tuple".into(), string("00"));
    });
    let error = error_of(store.try_job(&scope));
    assert!(matches!(error, MemoryError::DigestCollision), "{error:?}");
    let error = store.unchanged(|store| error_of(store.resume(&probe(&scope))));
    assert!(matches!(error, MemoryError::DigestCollision), "{error:?}");
}
