//! Gap 1 E2: crate-private fenced resume chunks of the exact-scope erasure
//! job.
//!
//! Contract: `docs/projects/roadmap/gap1-e2-resume-contract.md` (ratified,
//! provisional).
//! One chunk deletes whole live memories of the exact scope and advances the
//! job in one graph frame; the job is the fence. Nothing here is public,
//! served or a format change: memory state stays ordinary graph nodes.

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use super::super::{
    ExistingMemory, LABEL_CONTENT, LABEL_MEMORY, LABEL_RECEIPT, LABEL_RECEIPT_OUTCOME, MAX_OPS,
    MemoryError, MemoryMut, MemoryRef, MemoryResult, MemoryScope, content_edge_key, content_key,
    evidence_record_id, existing_memory, hex, memory_key, opaque, operation_receipt_key,
    receipt_outcome_key, string_property,
};
use super::{
    ErasureState, Job, LABEL_JOB, LogicalVisibility, MAX_COLLECTION_BYTES, OUTCOME_FIELDS,
    Progress, RECEIPT_FIELDS, SCHEMA_V2, canonical_nonzero, common, decode_job, digest_shaped,
    exact, integer, job_key, scope_node, text,
};
use crate::{
    Condition, Database, Expected, Metadata, MetadataValue, Mutation,
    property_graph::{
        Direction, EdgeRef, EdgeView, FindNodes, GraphLimits, GraphOp, GraphOpOutcome, GraphRef,
        Neighbors, NodeSelector, NodeView, ReadOptions,
    },
};

const CHUNK_SCHEMA: &str = "erase-chunk:v1";
const TOKEN_DOMAIN: &[u8] = b"vlite.memory.erasure.token.v1\0";
const RESUME_DOMAIN: &[u8] = b"vlite.memory.erasure.resume.v1\0";
/// A chunk holds at most (4,096 - 4) / 2 memories: each costs an operation
/// and a guard, and the job upsert, the outcome, the receipt and the job
/// guard are fixed.
pub(super) const MAX_CHUNK_MEMORIES: u64 = ((MAX_OPS - 4) / 2) as u64;
/// The candidate window; a window shorter than this has a known total.
const WINDOW: usize = MAX_OPS + 1;
const PAGE: u64 = 1_000;
/// A known `remaining` is at most a full known window minus one deletion.
pub(super) const MAX_KNOWN_REMAINING: u64 = (MAX_OPS - 1) as u64;

#[derive(Clone, Debug)]
pub(crate) struct ErasureJobRequest {
    pub scope: MemoryScope,
}

/// The current job as serving reports it (section 2 of the contract).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ErasureJob {
    pub job_key: String,
    pub revision: u64,
    pub generation: u64,
    pub state: ErasureState,
    pub logical_visibility: LogicalVisibility,
    pub deleted_count: u64,
    pub remaining: Option<u64>,
    pub chunks: u64,
    /// `Some` exactly while the job is pending.
    pub resume_token: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct ResumeErasureRequest {
    pub scope: MemoryScope,
    pub job_key: String,
    pub idempotency_key: String,
    pub expected_revision: u64,
    pub generation: u64,
    pub resume_token: String,
    pub now_ms: u64,
    pub receipt_window_ms: u64,
    pub deadline_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ChunkSummary {
    pub memories_deleted: u64,
    pub remaining_after: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResumeErasureResult {
    pub job: ErasureJob,
    pub chunk: ChunkSummary,
    pub receipt_revision: u64,
    pub commit_sequence: u64,
    pub replayed: bool,
}

/// Canonical u64 decimal: `0` or no leading zero, digits only.
pub(super) fn canonical_counter(value: &str) -> Option<u64> {
    let bytes = value.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 20
        || !bytes.iter().all(u8::is_ascii_digit)
        || (bytes.len() > 1 && bytes[0] == b'0')
    {
        return None;
    }
    value.parse().ok()
}

/// `SCOPE(ScopeBytes) || O(collection)` of the contract's section 3.
fn put_scope_binding(bytes: &mut Vec<u8>, scope: &MemoryScope, collection: Option<&str>) {
    let scope_bytes = scope.encode();
    bytes.extend_from_slice(&(scope_bytes.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&scope_bytes);
    match collection {
        None => bytes.push(0),
        Some(name) => {
            bytes.push(1);
            put_string(bytes, name);
        }
    }
}

fn put_string(bytes: &mut Vec<u8>, value: &str) {
    bytes.extend_from_slice(&(value.len() as u32).to_be_bytes());
    bytes.extend_from_slice(value.as_bytes());
}

/// The derived fence token (contract section 3): not stored, not secret.
pub(super) fn token(
    scope: &MemoryScope,
    collection: Option<&str>,
    job_key: &str,
    generation: u64,
    revision: u64,
    chunks: u64,
) -> String {
    let mut bytes = TOKEN_DOMAIN.to_vec();
    put_scope_binding(&mut bytes, scope, collection);
    put_string(&mut bytes, job_key);
    bytes.extend_from_slice(&generation.to_be_bytes());
    bytes.extend_from_slice(&revision.to_be_bytes());
    bytes.extend_from_slice(&chunks.to_be_bytes());
    format!("t1:{}", hex(&Sha256::digest(bytes)))
}

fn token_shaped(value: &str) -> bool {
    value.len() == 67
        && value.starts_with("t1:")
        && value[3..]
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn resume_digest(
    request: &ResumeErasureRequest,
    collection: Option<&str>,
    job_key: &str,
) -> String {
    let mut bytes = RESUME_DOMAIN.to_vec();
    put_scope_binding(&mut bytes, &request.scope, collection);
    put_string(&mut bytes, job_key);
    bytes.extend_from_slice(&request.expected_revision.to_be_bytes());
    bytes.extend_from_slice(&request.generation.to_be_bytes());
    put_string(&mut bytes, &request.resume_token);
    bytes.extend_from_slice(&request.receipt_window_ms.to_be_bytes());
    format!("v1:{}", hex(&Sha256::digest(bytes)))
}

fn resume_receipt_key(scope_key: &str, idempotency_key: &str) -> String {
    operation_receipt_key("resume-erasure", scope_key, idempotency_key)
}

fn state_text(state: ErasureState) -> &'static str {
    match state {
        ErasureState::Pending => "pending",
        ErasureState::Logical => "logical",
    }
}

fn remaining_text(remaining: Option<u64>) -> String {
    remaining.map_or_else(|| "unknown".to_owned(), |remaining| remaining.to_string())
}

/// One chunk's persisted numbers (the seven-line outcome of section 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Outcome {
    chunk_no: u64,
    deleted: u64,
    total: u64,
    remaining: Option<u64>,
    state: ErasureState,
}

fn outcome_text(job_key: &str, outcome: &Outcome) -> String {
    format!(
        "{CHUNK_SCHEMA}\n{job_key}\n{}\n{}\n{}\n{}\n{}\n",
        outcome.chunk_no,
        outcome.deleted,
        outcome.total,
        remaining_text(outcome.remaining),
        state_text(outcome.state),
    )
}

/// DO2: exactly the seven lines with canonical numbers, and re-rendering the
/// parsed numbers reproduces the stored text.
fn parse_outcome(stored: &str, job_key: &str) -> Option<Outcome> {
    let lines: Vec<&str> = stored.strip_suffix('\n')?.split('\n').collect();
    if lines.len() != 7 || lines[0] != CHUNK_SCHEMA || lines[1] != job_key {
        return None;
    }
    let outcome = Outcome {
        chunk_no: canonical_counter(lines[2])?,
        deleted: canonical_counter(lines[3])?,
        total: canonical_counter(lines[4])?,
        remaining: match lines[5] {
            "unknown" => None,
            value => Some(canonical_counter(value)?),
        },
        state: match lines[6] {
            "pending" => ErasureState::Pending,
            "logical" => ErasureState::Logical,
            _ => return None,
        },
    };
    (outcome_text(job_key, &outcome) == stored).then_some(outcome)
}

/// DO3: the cross-field rules of one outcome.
fn outcome_valid(outcome: &Outcome) -> bool {
    let bound = u128::from(MAX_CHUNK_MEMORIES);
    outcome.chunk_no >= 1
        && outcome.deleted <= MAX_CHUNK_MEMORIES
        && outcome.total >= outcome.deleted
        && u128::from(outcome.total) <= bound * u128::from(outcome.chunk_no)
        && (outcome.chunk_no != 1 || outcome.total == outcome.deleted)
        && outcome
            .remaining
            .is_none_or(|remaining| remaining <= MAX_KNOWN_REMAINING)
        && (outcome.state == ErasureState::Logical) == (outcome.remaining == Some(0))
        && (outcome.deleted != 0 || outcome.remaining == Some(0))
}

/// DO4: an outcome against the job it belongs to.
fn outcome_fits_job(outcome: &Outcome, progress: &Progress) -> bool {
    outcome.chunk_no <= progress.chunks
        && outcome.total <= progress.deleted_count
        && (outcome.state != ErasureState::Logical || outcome.chunk_no == progress.chunks)
        && (outcome.chunk_no != progress.chunks
            || (outcome.total == progress.deleted_count
                && outcome.remaining == progress.remaining
                && outcome.state == progress.state))
}

/// A strictly decoded resume receipt with its outcome.
struct Decoded {
    digest: String,
    expiry: u64,
    revision: u64,
    outcome: Outcome,
}

/// Strict resume receipt decoding, live or expired (contract section 4:
/// DR1-DR5, DO1-DO5). A receipt needs a valid v2 job.
fn decode_resume_receipt(
    read: &GraphRef<'_>,
    receipt: &NodeView,
    scope: &MemoryScope,
    scope_key: &str,
    binding: &str,
    job: Option<&Job>,
) -> MemoryResult<Decoded> {
    exact(receipt, LABEL_RECEIPT, RECEIPT_FIELDS)?;
    common(receipt, scope, scope_key, binding, CHUNK_SCHEMA)?;
    let digest = text(receipt, "digest");
    if !digest_shaped(digest)
        || text(receipt, "job_key") != job_key(scope_key)
        || integer(receipt, "outcome_chunks") != Some(1)
    {
        return Err(MemoryError::InvalidBundle);
    }
    let expiry =
        canonical_nonzero(text(receipt, "expires_at_ms")).ok_or(MemoryError::InvalidBundle)?;
    let progress = job
        .and_then(|job| job.progress.as_ref())
        .ok_or(MemoryError::InvalidBundle)?;
    if read
        .get_node(&receipt_outcome_key(&receipt.key, 1))?
        .output
        .is_some()
    {
        return Err(MemoryError::InvalidBundle);
    }
    let node = read
        .get_node(&receipt_outcome_key(&receipt.key, 0))?
        .output
        .ok_or(MemoryError::InvalidBundle)?;
    exact(&node, LABEL_RECEIPT_OUTCOME, OUTCOME_FIELDS)?;
    if text(&node, "receipt_key") != receipt.key || integer(&node, "index") != Some(0) {
        return Err(MemoryError::InvalidBundle);
    }
    let outcome = parse_outcome(text(&node, "outcome"), &job_key(scope_key))
        .filter(|outcome| outcome_valid(outcome) && outcome_fits_job(outcome, progress))
        .ok_or(MemoryError::InvalidBundle)?;
    Ok(Decoded {
        digest: digest.to_owned(),
        expiry,
        revision: receipt.revision,
        outcome,
    })
}

/// The job as serving reports it.
fn job_view(scope: &MemoryScope, collection: Option<&str>, key: &str, job: &Job) -> ErasureJob {
    let (state, deleted_count, remaining, chunks) = match &job.progress {
        None => (ErasureState::Pending, 0, None, 0),
        Some(progress) => (
            progress.state,
            progress.deleted_count,
            progress.remaining,
            progress.chunks,
        ),
    };
    ErasureJob {
        job_key: key.to_owned(),
        revision: job.revision,
        generation: 1,
        state,
        logical_visibility: LogicalVisibility::Hidden,
        deleted_count,
        remaining,
        chunks,
        resume_token: (state == ErasureState::Pending)
            .then(|| token(scope, collection, key, 1, job.revision, chunks)),
    }
}

/// The original chunk's view of a replayed resume (contract section 6).
fn replay_view(
    scope: &MemoryScope,
    collection: Option<&str>,
    key: &str,
    decoded: &Decoded,
) -> ResumeErasureResult {
    let outcome = &decoded.outcome;
    ResumeErasureResult {
        job: ErasureJob {
            job_key: key.to_owned(),
            revision: decoded.revision,
            generation: 1,
            state: outcome.state,
            logical_visibility: LogicalVisibility::Hidden,
            deleted_count: outcome.total,
            remaining: outcome.remaining,
            chunks: outcome.chunk_no,
            resume_token: (outcome.state == ErasureState::Pending).then(|| {
                token(
                    scope,
                    collection,
                    key,
                    1,
                    decoded.revision,
                    outcome.chunk_no,
                )
            }),
        },
        chunk: ChunkSummary {
            memories_deleted: outcome.deleted,
            remaining_after: outcome.remaining,
        },
        receipt_revision: decoded.revision,
        commit_sequence: decoded.revision,
        replayed: true,
    }
}

impl MemoryRef<'_> {
    /// The current job, strictly decoded (v1 or v2); read-only and exempt
    /// from the barrier. Serving uses it to hand out the resume token.
    pub(crate) fn erasure_job(&self, request: &ErasureJobRequest) -> MemoryResult<ErasureJob> {
        request.scope.validate()?;
        let scope_key = request.scope.key();
        let read = self.db.graph(&self.graph)?;
        scope_node(&read, &request.scope, &scope_key)?;
        let key = job_key(&scope_key);
        let node = read
            .get_node(&key)?
            .output
            .ok_or(MemoryError::MissingMemory)?;
        let binding = self.evidence_collection.as_deref().unwrap_or_default();
        let job = decode_job(&node, &request.scope, &scope_key, binding)?;
        Ok(job_view(
            &request.scope,
            self.evidence_collection.as_deref(),
            &key,
            &job,
        ))
    }
}

/// One memory of a chunk: the decoded node and the evidence record it owns.
struct Member {
    memory: ExistingMemory,
    record: Option<u64>,
}

/// A planned chunk (contract section 5).
struct Plan {
    members: Vec<Member>,
    /// Unique incident edges: ID -> revision, in ascending ID order.
    edges: BTreeMap<u64, u64>,
    /// Content nodes by the ID of their `HAS_CONTENT` edge: key and revision.
    content: BTreeMap<u64, (String, u64)>,
    remaining_after: Option<u64>,
}

/// The first 4,097 live memories of the scope in ascending node-ID order.
fn scan_window(read: &GraphRef<'_>, scope_key: &str) -> MemoryResult<Vec<NodeView>> {
    let conditions = vec![Condition::Eq {
        field: "scope".into(),
        value: MetadataValue::String(scope_key.into()),
    }];
    let mut cursor = None;
    let mut nodes = Vec::new();
    loop {
        let page = read.find_nodes(
            &FindNodes {
                label: Some(LABEL_MEMORY.into()),
                conditions: conditions.clone(),
                limit: Some(PAGE),
                cursor,
            },
            &ReadOptions {
                limits: GraphLimits::CEILING,
                ..ReadOptions::default()
            },
        )?;
        nodes.extend(page.output);
        if nodes.len() >= WINDOW {
            nodes.truncate(WINDOW);
            return Ok(nodes);
        }
        match page.cursor {
            Some(next) => cursor = Some(next),
            None if page.complete => return Ok(nodes),
            None => return Err(MemoryError::OperationLimit),
        }
    }
}

/// Every `existing_memory` error and every failed check is `InvalidBundle`.
fn decode_candidate(node: &NodeView, scope_key: &str) -> MemoryResult<ExistingMemory> {
    let memory = existing_memory(node).map_err(|_| MemoryError::InvalidBundle)?;
    if node.labels.len() != 1
        || !node.labels.contains(LABEL_MEMORY)
        || memory.scope != scope_key
        || node.key != memory_key(scope_key, &memory.id)
    {
        return Err(MemoryError::InvalidBundle);
    }
    Ok(memory)
}

/// The unique incident edges of `key`, both directions; `None` when more
/// than 4,096 exist (the node can never fit one frame).
fn incident_edges(read: &GraphRef<'_>, key: &str) -> MemoryResult<Option<BTreeMap<u64, EdgeView>>> {
    let mut edges = BTreeMap::new();
    let mut cursor = None;
    loop {
        let page = read.neighbors(
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
        )?;
        for edge in page.output.edges {
            edges.insert(edge.id, edge);
        }
        if edges.len() > MAX_OPS {
            return Ok(None);
        }
        match page.cursor {
            Some(next) => cursor = Some(next),
            None if page.complete => return Ok(Some(edges)),
            None => return Err(MemoryError::OperationLimit),
        }
    }
}

/// Step 1 of the per-candidate plan: A2's evidence rules, without retention.
fn candidate_evidence(
    db: &Database,
    graph: &str,
    collection: Option<&str>,
    collection_id: Option<u16>,
    read: &GraphRef<'_>,
    memory: &ExistingMemory,
    now: u64,
) -> MemoryResult<Option<u64>> {
    let attachments = read.memory_node_attachments(&memory.key)?;
    if memory.evidence.is_empty() {
        if !attachments.is_empty() {
            return Err(MemoryError::ForeignAttachment);
        }
        return Ok(None);
    }
    let collection_id = collection_id.ok_or(MemoryError::InvalidEvidence)?;
    if attachments
        .iter()
        .any(|(attached, _)| *attached != collection_id)
    {
        return Err(MemoryError::ForeignAttachment);
    }
    let attached_record = attachments
        .iter()
        .find(|(attached, _)| *attached == collection_id)
        .map(|(_, record)| *record);
    if memory.evidence.len() != 1 {
        return Err(MemoryError::InvalidBundle);
    }
    let record = evidence_record_id(&memory.evidence[0]);
    let collection = collection.ok_or(MemoryError::InvalidEvidence)?;
    let owner = db.memory_attachment_owner(collection, record, now)?;
    if owner.as_ref() != Some(&(graph.to_owned(), memory.key.clone())) {
        return Err(MemoryError::ForeignAttachment);
    }
    if attached_record != Some(record) {
        return Err(MemoryError::InvalidBundle);
    }
    Ok(Some(record))
}

/// Step 3 of the per-candidate plan: the verified content nodes of `memory`,
/// keyed by their `HAS_CONTENT` edge.
fn candidate_content(
    read: &GraphRef<'_>,
    memory: &ExistingMemory,
    edges: &BTreeMap<u64, EdgeView>,
    scope_key: &str,
) -> MemoryResult<BTreeMap<u64, (String, u64)>> {
    let mut content = BTreeMap::new();
    for edge in edges
        .values()
        .filter(|edge| edge.relationship == "HAS_CONTENT" && edge.source_key == memory.key)
    {
        let target = read
            .get_node(&edge.target_key)?
            .output
            .ok_or(MemoryError::InvalidBundle)?;
        let index = match target.properties.get("index") {
            Some(MetadataValue::Integer(index)) if *index >= 0 => *index as usize,
            _ => return Err(MemoryError::InvalidBundle),
        };
        if edge.key.as_deref() != Some(content_edge_key(&memory.key, index).as_str())
            || target.key != content_key(&memory.key, index)
            || target.labels.len() != 1
            || !target.labels.contains(LABEL_CONTENT)
            || string_property(&target, "scope").ok() != Some(scope_key)
        {
            return Err(MemoryError::InvalidBundle);
        }
        match incident_edges(read, &target.key)? {
            Some(others) if others.len() == 1 && others.contains_key(&edge.id) => {}
            _ => return Err(MemoryError::InvalidBundle),
        }
        if !read.memory_node_attachments(&target.key)?.is_empty() {
            return Err(MemoryError::ForeignAttachment);
        }
        content.insert(edge.id, (target.key.clone(), target.revision));
    }
    Ok(content)
}

/// Plans one chunk (contract section 5). Changes nothing.
fn plan_chunk(
    db: &Database,
    graph: &str,
    collection: Option<&str>,
    read: &GraphRef<'_>,
    scope_key: &str,
    now: u64,
) -> MemoryResult<Plan> {
    let window = scan_window(read, scope_key)?;
    let candidates = window
        .iter()
        .map(|node| decode_candidate(node, scope_key))
        .collect::<MemoryResult<Vec<_>>>()?;
    let total = (window.len() < WINDOW).then_some(window.len() as u64);
    let collection_id = collection.map(|name| db.resolve(name)).transpose()?;
    let mut members: Vec<Member> = Vec::new();
    let mut edges: BTreeMap<u64, u64> = BTreeMap::new();
    let mut content: BTreeMap<u64, (String, u64)> = BTreeMap::new();
    let mut evidence_ops = 0usize;
    for memory in candidates {
        let record = candidate_evidence(db, graph, collection, collection_id, read, &memory, now)?;
        let Some(found) = incident_edges(read, &memory.key)? else {
            break;
        };
        let added_content = candidate_content(read, &memory, &found, scope_key)?;
        let added_edges = found.keys().filter(|id| !edges.contains_key(*id)).count();
        let evidence = evidence_ops + 2 * usize::from(record.is_some());
        let memories = members.len() + 1;
        let operations = 3
            + evidence
            + edges.len()
            + added_edges
            + content.len()
            + added_content.len()
            + memories;
        let guards = memories + 1;
        if operations + guards > MAX_OPS {
            break;
        }
        evidence_ops = evidence;
        for (id, edge) in &found {
            edges.entry(*id).or_insert(edge.revision);
        }
        content.extend(added_content);
        members.push(Member { memory, record });
    }
    if members.is_empty() && !window.is_empty() {
        return Err(MemoryError::OperationLimit);
    }
    let remaining_after = total.map(|total| total - members.len() as u64);
    Ok(Plan {
        members,
        edges,
        content,
        remaining_after,
    })
}

impl MemoryMut<'_> {
    /// Deletes one bounded chunk of the live memories of the scope of an
    /// existing erasure job and advances the job in the same frame
    /// (contract section 2: validation, replay, fence, plan, frame).
    pub(crate) fn resume_erasure(
        &mut self,
        request: &ResumeErasureRequest,
    ) -> MemoryResult<ResumeErasureResult> {
        request.scope.validate()?;
        if !opaque(&request.idempotency_key) {
            return Err(MemoryError::InvalidKey);
        }
        let now = request.now_ms;
        if now > i64::MAX as u64 {
            return Err(MemoryError::InvalidClock);
        }
        if request.receipt_window_ms == 0 || now.checked_add(request.receipt_window_ms).is_none() {
            return Err(MemoryError::InvalidReceiptWindow);
        }
        if request.expected_revision == 0 || request.generation == 0 {
            return Err(MemoryError::InvalidTarget);
        }
        if !opaque(&request.job_key) {
            return Err(MemoryError::InvalidKey);
        }
        if !token_shaped(&request.resume_token) {
            return Err(MemoryError::InvalidTarget);
        }
        let collection = self.evidence_collection.clone();
        if collection
            .as_deref()
            .is_some_and(|name| name.is_empty() || name.len() > MAX_COLLECTION_BYTES)
        {
            return Err(MemoryError::InvalidBundle);
        }
        let binding = collection.as_deref().unwrap_or_default();
        let scope = &request.scope;
        let scope_key = scope.key();

        let read = self.db.graph(&self.graph)?.with_clock(now);
        scope_node(&read, scope, &scope_key)?;
        let job_key = job_key(&scope_key);
        let job = read
            .get_node(&job_key)?
            .output
            .map(|node| decode_job(&node, scope, &scope_key, binding))
            .transpose()?;
        let receipt_key = resume_receipt_key(&scope_key, &request.idempotency_key);
        let existing_receipt = read.get_node(&receipt_key)?.output;
        let decoded = match &existing_receipt {
            Some(receipt) => Some(decode_resume_receipt(
                &read,
                receipt,
                scope,
                &scope_key,
                binding,
                job.as_ref(),
            )?),
            None => {
                for index in 0..2 {
                    if read
                        .get_node(&receipt_outcome_key(&receipt_key, index))?
                        .output
                        .is_some()
                    {
                        return Err(MemoryError::InvalidBundle);
                    }
                }
                None
            }
        };
        let job = job.ok_or(MemoryError::MissingMemory)?;
        if request.job_key != job_key {
            return Err(MemoryError::MissingMemory);
        }
        let digest = resume_digest(request, collection.as_deref(), &job_key);
        if let Some(decoded) = &decoded
            && now < decoded.expiry
        {
            if decoded.digest != digest {
                return Err(MemoryError::IdempotencyConflict);
            }
            return Ok(replay_view(scope, collection.as_deref(), &job_key, decoded));
        }
        let progress = job.progress;
        if progress.is_some_and(|progress| progress.state == ErasureState::Logical) {
            return Err(MemoryError::JobConflict);
        }
        if request.deadline_ms.is_some_and(|deadline| now >= deadline) {
            return Err(MemoryError::DeadlineExceeded);
        }
        let chunks = progress.map_or(0, |progress| progress.chunks);
        if request.generation != 1
            || request.expected_revision != job.revision
            || request.resume_token
                != token(
                    scope,
                    collection.as_deref(),
                    &job_key,
                    1,
                    job.revision,
                    chunks,
                )
        {
            return Err(MemoryError::JobConflict);
        }

        let plan = plan_chunk(
            self.db,
            &self.graph,
            collection.as_deref(),
            &read,
            &scope_key,
            now,
        )?;
        let deleted = plan.members.len() as u64;
        let total = progress
            .map_or(0, |progress| progress.deleted_count)
            .checked_add(deleted)
            .ok_or(MemoryError::InvalidBundle)?;
        let next_chunks = chunks.checked_add(1).ok_or(MemoryError::InvalidBundle)?;
        let state = if plan.remaining_after == Some(0) {
            ErasureState::Logical
        } else {
            ErasureState::Pending
        };
        let outcome = Outcome {
            chunk_no: next_chunks,
            deleted,
            total,
            remaining: plan.remaining_after,
            state,
        };

        let string = |value: &str| MetadataValue::String(value.to_owned());
        let mut ops = Vec::new();
        for member in &plan.members {
            if let (Some(record), Some(name)) = (member.record, collection.as_deref()) {
                ops.push(GraphOp::Detach {
                    node: member.memory.key.clone(),
                    collection: name.to_owned(),
                });
                ops.push(GraphOp::Vector {
                    collection: name.to_owned(),
                    mutation: Mutation::Delete { id: record },
                });
            }
        }
        for (&id, &revision) in &plan.edges {
            ops.push(GraphOp::DeleteEdge {
                edge: EdgeRef::Id(id),
                expected: Expected::Revision(revision),
            });
        }
        for (key, revision) in plan.content.values() {
            ops.push(GraphOp::DeleteNode {
                key: key.clone(),
                expected: Expected::Revision(*revision),
            });
        }
        for member in &plan.members {
            ops.push(GraphOp::DeleteNode {
                key: member.memory.key.clone(),
                expected: Expected::Revision(member.memory.graph_revision),
            });
        }
        let mut job_properties = Metadata::new();
        for (name, value) in [
            ("e1_schema", SCHEMA_V2),
            ("scope", scope_key.as_str()),
            ("e1_scope_tuple", hex(&scope.encode()).as_str()),
            ("e1_collection", binding),
            ("begin_digest", job.begin_digest.as_str()),
            ("generation", "1"),
            ("state", state_text(state)),
            ("logical_visibility", "hidden"),
            ("deleted_count", total.to_string().as_str()),
            ("remaining", remaining_text(plan.remaining_after).as_str()),
            ("e2_chunks", next_chunks.to_string().as_str()),
        ] {
            job_properties.insert(name.into(), string(value));
        }
        job_properties.insert(
            "created_at_ms".into(),
            MetadataValue::Integer(job.created_at_ms),
        );
        ops.push(GraphOp::UpsertNode {
            key: job_key.clone(),
            labels: vec![LABEL_JOB.into()],
            properties: job_properties,
            expected: Expected::Revision(job.revision),
        });
        let outcome_key = receipt_outcome_key(&receipt_key, 0);
        let mut outcome_properties = Metadata::new();
        outcome_properties.insert("receipt_key".into(), string(&receipt_key));
        outcome_properties.insert("outcome".into(), string(&outcome_text(&job_key, &outcome)));
        outcome_properties.insert("index".into(), MetadataValue::Integer(0));
        let mut receipt_properties = Metadata::new();
        for (name, value) in [
            ("e1_schema", CHUNK_SCHEMA),
            ("scope", scope_key.as_str()),
            ("e1_scope_tuple", hex(&scope.encode()).as_str()),
            ("e1_collection", binding),
            ("digest", digest.as_str()),
            (
                "expires_at_ms",
                (now + request.receipt_window_ms).to_string().as_str(),
            ),
            ("job_key", job_key.as_str()),
        ] {
            receipt_properties.insert(name.into(), string(value));
        }
        receipt_properties.insert("outcome_chunks".into(), MetadataValue::Integer(1));
        // An expired receipt of the same key is replaced, guarded by its
        // revision (A2's rule); otherwise the nodes are new.
        match &existing_receipt {
            Some(old) => {
                let old_outcome = read
                    .get_node(&outcome_key)?
                    .output
                    .ok_or(MemoryError::InvalidBundle)?;
                ops.push(GraphOp::UpsertNode {
                    key: outcome_key,
                    labels: vec![LABEL_RECEIPT_OUTCOME.into()],
                    properties: outcome_properties,
                    expected: Expected::Revision(old_outcome.revision),
                });
                ops.push(GraphOp::UpsertNode {
                    key: receipt_key.clone(),
                    labels: vec![LABEL_RECEIPT.into()],
                    properties: receipt_properties,
                    expected: Expected::Revision(old.revision),
                });
            }
            None => {
                ops.push(GraphOp::CreateNode {
                    key: outcome_key,
                    labels: vec![LABEL_RECEIPT_OUTCOME.into()],
                    properties: outcome_properties,
                });
                ops.push(GraphOp::CreateNode {
                    key: receipt_key.clone(),
                    labels: vec![LABEL_RECEIPT.into()],
                    properties: receipt_properties,
                });
            }
        }
        #[cfg(test)]
        super::super::FAIL_NEXT_WRITE_BATCH.with(|fail| {
            if fail.replace(false) {
                ops.push(GraphOp::CreateNode {
                    key: scope_key.clone(),
                    labels: vec![super::super::LABEL_SCOPE.into()],
                    properties: Metadata::new(),
                });
            }
        });
        let batch = self.db.graph_mut(&self.graph)?.write_batch_at(&ops, now)?;
        let revision = match batch.outcomes.last() {
            Some(GraphOpOutcome::Node(write)) => write.revision,
            _ => {
                return Err(MemoryError::Storage(crate::Error::InvalidQuery(
                    "erasure chunk receipt was not written",
                )));
            }
        };
        Ok(ResumeErasureResult {
            job: ErasureJob {
                job_key: job_key.clone(),
                revision,
                generation: 1,
                state,
                logical_visibility: LogicalVisibility::Hidden,
                deleted_count: total,
                remaining: plan.remaining_after,
                chunks: next_chunks,
                resume_token: (state == ErasureState::Pending).then(|| {
                    token(
                        scope,
                        collection.as_deref(),
                        &job_key,
                        1,
                        revision,
                        next_chunks,
                    )
                }),
            },
            chunk: ChunkSummary {
                memories_deleted: deleted,
                remaining_after: plan.remaining_after,
            },
            receipt_revision: revision,
            commit_sequence: revision,
            replayed: false,
        })
    }
}
