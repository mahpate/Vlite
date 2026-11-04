//! Private C1 consolidation: one caller-produced Episodic summary derived from
//! retained Entity/Episodic sources, written with its receipt in one graph
//! batch. Frozen contract: `docs/projects/roadmap/gap1-c1-consolidation-contract.md`.
//! Crate-private only: no adapter, public API or `Error` variant.

use sha2::{Digest, Sha256};

use super::{
    LABEL_CONTENT, LABEL_MEMORY, LABEL_RECEIPT, LABEL_RECEIPT_OUTCOME, MAX_BODY_BYTES, MAX_OPS,
    MAX_PROVENANCE, MAX_STRING_VALUE_BYTES, MemoryError, MemoryEvidence, MemoryMut,
    MemoryProperties, MemoryResult, MemoryScope, Provenance, content_chunks, content_digest,
    content_edge_key, content_key, encode_provenance, evidence_record_id, existing_memory, hex,
    memory_key, memory_properties, opaque, operation_receipt_key, receipt_outcome_key,
    scan_memories, string_property, unhex,
};
use crate::{
    Error, Expected, Metadata, MetadataValue,
    property_graph::{GraphOp, GraphOpOutcome, GraphRef, NodeView},
};

const SCHEMA: &str = "consolidate:v1";
const OPERATION: &str = "consolidate";
const DEFAULT_INPUT_BUDGET: u64 = 64;
const MAX_INPUT_BUDGET: u64 = 256;
const DEFAULT_IMPORTANCE: f64 = 0.5;
const DERIVED_FROM: &str = "DERIVED_FROM";
const SOURCES_DOMAIN: &[u8] = b"vlite.memory.consolidate.sources.v1\0";
const REQUEST_DOMAIN: &[u8] = b"vlite.memory.consolidate.request.v1\0";
const EDGE_DOMAIN: &[u8] = b"vlite.memory.consolidate.edge.v1\0";

/// One source memory. `expected_memory_revision` is `None` when the caller's
/// value is not a `u64`; `retain` is false for any other disposition.
#[derive(Clone, Debug)]
pub(crate) struct ConsolidateInput {
    pub id: String,
    pub expected_memory_revision: Option<u64>,
    pub retain: bool,
}

/// The caller-produced summary. `observed_at_ms` is `None` when the value is
/// not an `i64`; `expiry_valid` is false when the expiry is not a `u64`.
#[derive(Clone, Debug)]
pub(crate) struct ConsolidateSummary {
    pub id: String,
    pub content: String,
    pub source: String,
    pub observed_at_ms: Option<i64>,
    pub importance: Option<f64>,
    pub expiry_ms: Option<u64>,
    pub expiry_valid: bool,
    pub provenance: Vec<Provenance>,
}

/// `clock_valid` is false when execution C is not a `u64`.
#[derive(Clone, Debug)]
pub(crate) struct ConsolidateRequest {
    pub scope: MemoryScope,
    pub idempotency_key: String,
    pub now_ms: u64,
    pub clock_valid: bool,
    pub receipt_window_ms: u64,
    pub deadline_ms: Option<u64>,
    pub input_budget: Option<u64>,
    pub inputs: Vec<ConsolidateInput>,
    pub summary: ConsolidateSummary,
    pub transformation: String,
    pub evidence: Option<MemoryEvidence>,
    pub allow_future_observations: bool,
}

/// The created or replayed summary. The summary memory revision is always 1;
/// `logical_operations` is the charged write budget (0 on replay).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConsolidateResult {
    pub summary_id: String,
    pub summary_revision: u64,
    pub source_count: usize,
    pub source_digest: String,
    pub receipt_revision: u64,
    pub commit_sequence: u64,
    pub replayed: bool,
    pub logical_operations: usize,
}

/// A structurally valid request in canonical form (contract sections 2-3).
struct Normalized {
    /// Canonical input set: `(id, expected memory revision)` by UTF-8 bytes.
    inputs: Vec<(String, u64)>,
    importance: f64,
    observed_at_ms: i64,
    provenance: Vec<Provenance>,
    source_bytes: Vec<u8>,
    request_bytes: Vec<u8>,
}

/// The decoded one-chunk outcome (contract section 5).
#[derive(Debug, PartialEq, Eq)]
struct StoredOutcome {
    id: String,
    count: usize,
    source_digest: String,
}

struct StoredReceipt {
    digest: String,
    expires_at_ms: u64,
    outcome: StoredOutcome,
    receipt_revision: u64,
    outcome_revision: u64,
}

fn put_u32(out: &mut Vec<u8>, value: usize) -> MemoryResult<()> {
    let value = u32::try_from(value).map_err(|_| MemoryError::BodyLimit)?;
    out.extend_from_slice(&value.to_be_bytes());
    Ok(())
}

fn put_s(out: &mut Vec<u8>, value: &str) -> MemoryResult<()> {
    put_u32(out, value.len())?;
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

fn put_o(out: &mut Vec<u8>, value: Option<&str>) -> MemoryResult<()> {
    match value {
        None => out.push(0),
        Some(value) => {
            out.push(1);
            put_s(out, value)?;
        }
    }
    Ok(())
}

fn put_scope(out: &mut Vec<u8>, scope: &MemoryScope) -> MemoryResult<()> {
    let bytes = scope.encode();
    put_u32(out, bytes.len())?;
    out.extend_from_slice(&bytes);
    Ok(())
}

fn digest(bytes: &[u8]) -> String {
    format!("v1:{}", hex(&Sha256::digest(bytes)))
}

fn optional_text(value: Option<&str>) -> bool {
    value.is_none_or(|value| value.len() <= super::MAX_OPAQUE_BYTES)
}

/// Contract section 6 step 1, in order; nothing reads storage.
fn normalize(request: &ConsolidateRequest) -> MemoryResult<Normalized> {
    request.scope.validate()?;
    if !opaque(&request.idempotency_key) {
        return Err(MemoryError::InvalidKey);
    }
    if !request.clock_valid || request.now_ms > i64::MAX as u64 {
        return Err(MemoryError::InvalidClock);
    }
    if request.receipt_window_ms == 0
        || request
            .now_ms
            .checked_add(request.receipt_window_ms)
            .is_none()
    {
        return Err(MemoryError::InvalidReceiptWindow);
    }
    let budget = request.input_budget.unwrap_or(DEFAULT_INPUT_BUDGET);
    if budget == 0 || request.inputs.is_empty() {
        return Err(MemoryError::InvalidBundle);
    }
    if budget > MAX_INPUT_BUDGET || request.inputs.len() as u64 > budget {
        return Err(MemoryError::OperationLimit);
    }
    let mut inputs = Vec::with_capacity(request.inputs.len());
    for input in &request.inputs {
        match input.expected_memory_revision {
            Some(revision) if revision != 0 && opaque(&input.id) => {
                inputs.push((input.id.clone(), revision));
            }
            _ => return Err(MemoryError::InvalidTarget),
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    if !inputs.iter().all(|(id, _)| seen.insert(id.as_str())) {
        return Err(MemoryError::DuplicateTarget);
    }
    if request.inputs.iter().any(|input| !input.retain) {
        return Err(MemoryError::InvalidBundle);
    }
    let summary = &request.summary;
    if !opaque(&summary.id) || !opaque(&summary.source) {
        return Err(MemoryError::InvalidIdentity);
    }
    if summary.content.is_empty() {
        return Err(MemoryError::InvalidContent);
    }
    if summary.content.len() > MAX_BODY_BYTES {
        return Err(MemoryError::BodyLimit);
    }
    let importance = summary.importance.unwrap_or(DEFAULT_IMPORTANCE);
    if !importance.is_finite() || !(0.0..=1.0).contains(&importance) {
        return Err(MemoryError::InvalidImportance);
    }
    let importance = if importance == 0.0 { 0.0 } else { importance };
    let observed_at_ms = summary
        .observed_at_ms
        .ok_or(MemoryError::InvalidObservation)?;
    if !summary.expiry_valid {
        return Err(MemoryError::InvalidExpiry);
    }
    if summary.provenance.len() > MAX_PROVENANCE {
        return Err(MemoryError::ProvenanceLimit);
    }
    let provenance = if summary.provenance.is_empty() {
        vec![Provenance {
            source: summary.source.clone(),
            kind: None,
            uri: None,
            transformation: None,
        }]
    } else {
        summary.provenance.clone()
    };
    if encode_provenance(&provenance).len() > MAX_STRING_VALUE_BYTES {
        return Err(MemoryError::ProvenanceLimit);
    }
    if !provenance.iter().all(|entry| {
        opaque(&entry.source)
            && optional_text(entry.kind.as_deref())
            && optional_text(entry.uri.as_deref())
            && optional_text(entry.transformation.as_deref())
    }) || !opaque(&request.transformation)
    {
        return Err(MemoryError::InvalidProvenance);
    }
    if let Some(evidence) = &request.evidence {
        if !opaque(&evidence.id) {
            return Err(MemoryError::InvalidEvidence);
        }
        evidence.scope.validate()?;
        if evidence.scope.encode() != request.scope.encode() {
            return Err(MemoryError::CrossScope);
        }
    }
    inputs.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));

    let mut set = Vec::new();
    put_u32(&mut set, inputs.len())?;
    for (id, revision) in &inputs {
        put_s(&mut set, id)?;
        set.extend_from_slice(&revision.to_be_bytes());
        set.push(0);
    }
    let mut scoped_set = Vec::new();
    put_scope(&mut scoped_set, &request.scope)?;
    scoped_set.extend_from_slice(&set);

    let mut source_bytes = SOURCES_DOMAIN.to_vec();
    source_bytes.extend_from_slice(&scoped_set);

    let mut request_bytes = REQUEST_DOMAIN.to_vec();
    request_bytes.extend_from_slice(&scoped_set);
    put_s(&mut request_bytes, &summary.id)?;
    put_s(&mut request_bytes, &summary.content)?;
    put_s(&mut request_bytes, &summary.source)?;
    request_bytes.extend_from_slice(&observed_at_ms.to_be_bytes());
    request_bytes.extend_from_slice(&importance.to_bits().to_be_bytes());
    match summary.expiry_ms {
        None => request_bytes.push(0),
        Some(expiry) => {
            request_bytes.push(1);
            request_bytes.extend_from_slice(&expiry.to_be_bytes());
        }
    }
    put_u32(&mut request_bytes, provenance.len())?;
    for entry in &provenance {
        put_s(&mut request_bytes, &entry.source)?;
        put_o(&mut request_bytes, entry.kind.as_deref())?;
        put_o(&mut request_bytes, entry.uri.as_deref())?;
        put_o(&mut request_bytes, entry.transformation.as_deref())?;
    }
    put_s(&mut request_bytes, &request.transformation)?;
    match &request.evidence {
        None => request_bytes.push(0),
        Some(evidence) => {
            request_bytes.push(1);
            put_s(&mut request_bytes, &evidence.id)?;
            put_scope(&mut request_bytes, &evidence.scope)?;
        }
    }
    request_bytes.push(u8::from(request.allow_future_observations));
    request_bytes.extend_from_slice(&request.receipt_window_ms.to_be_bytes());
    if request_bytes.len() > MAX_BODY_BYTES {
        return Err(MemoryError::BodyLimit);
    }
    Ok(Normalized {
        inputs,
        importance,
        observed_at_ms,
        provenance,
        source_bytes,
        request_bytes,
    })
}

pub(super) fn digest_shaped(value: &str) -> bool {
    value.len() == 67
        && value.starts_with("v1:")
        && value[3..]
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Nonzero canonical decimal: no sign, leading zero or whitespace.
pub(super) fn canonical_decimal(value: &str, max_digits: usize) -> Option<u64> {
    let bytes = value.as_bytes();
    if bytes.is_empty()
        || bytes.len() > max_digits
        || !matches!(bytes[0], b'1'..=b'9')
        || !bytes.iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    value.parse().ok()
}

fn decode_outcome(value: &str) -> MemoryResult<StoredOutcome> {
    let invalid = MemoryError::InvalidBundle;
    if value.is_empty() || value.len() > MAX_STRING_VALUE_BYTES || !value.is_ascii() {
        return Err(invalid);
    }
    let lines: Vec<&str> = value.split('\n').collect();
    let [schema, id_hex, revision, count, source_digest, ""] = lines.as_slice() else {
        return Err(invalid);
    };
    if *schema != SCHEMA
        || id_hex.is_empty()
        || id_hex.len() > 2 * super::MAX_OPAQUE_BYTES
        || !id_hex
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        || *revision != "1"
        || !digest_shaped(source_digest)
    {
        return Err(invalid);
    }
    let count = canonical_decimal(count, 3)
        .filter(|count| (1..=MAX_INPUT_BUDGET).contains(count))
        .ok_or(MemoryError::InvalidBundle)?;
    let id = String::from_utf8(unhex(id_hex)?).map_err(|_| MemoryError::InvalidBundle)?;
    if !opaque(&id) {
        return Err(MemoryError::InvalidBundle);
    }
    Ok(StoredOutcome {
        id,
        count: count as usize,
        source_digest: (*source_digest).to_owned(),
    })
}

#[derive(Clone, Copy)]
enum Field {
    String,
    Integer,
}

/// Exactly one label and exactly these property names and types.
fn exact_node(node: &NodeView, label: &str, fields: &[(&str, Field)]) -> bool {
    node.labels.len() == 1
        && node.labels.contains(label)
        && node.properties.len() == fields.len()
        && fields.iter().all(|(name, field)| {
            matches!(
                (node.properties.get(*name), field),
                (Some(MetadataValue::String(_)), Field::String)
                    | (Some(MetadataValue::Integer(_)), Field::Integer)
            )
        })
}

fn text<'a>(node: &'a NodeView, name: &str) -> &'a str {
    match node.properties.get(name) {
        Some(MetadataValue::String(value)) => value,
        _ => "",
    }
}

fn integer(node: &NodeView, name: &str) -> Option<i64> {
    match node.properties.get(name) {
        Some(MetadataValue::Integer(value)) => Some(*value),
        _ => None,
    }
}

const RECEIPT_FIELDS: &[(&str, Field)] = &[
    ("scope", Field::String),
    ("c1_scope_tuple", Field::String),
    ("c1_schema", Field::String),
    ("c1_collection", Field::String),
    ("digest", Field::String),
    ("expires_at_ms", Field::String),
    ("outcome_chunks", Field::Integer),
];

const OUTCOME_FIELDS: &[(&str, Field)] = &[
    ("receipt_key", Field::String),
    ("index", Field::Integer),
    ("outcome", Field::String),
];

/// Strict decoding of any present C1 receipt, live or expired (section 5).
fn decode_receipt(
    read: &GraphRef<'_>,
    receipt: &NodeView,
    scope: &MemoryScope,
    scope_key: &str,
    collection: &str,
) -> MemoryResult<StoredReceipt> {
    if !exact_node(receipt, LABEL_RECEIPT, RECEIPT_FIELDS)
        || text(receipt, "scope") != scope_key
        || text(receipt, "c1_schema") != SCHEMA
    {
        return Err(MemoryError::InvalidBundle);
    }
    if text(receipt, "c1_scope_tuple") != hex(&scope.encode()) {
        return Err(MemoryError::DigestCollision);
    }
    if text(receipt, "c1_collection") != collection {
        return Err(MemoryError::InvalidBundle);
    }
    let digest = text(receipt, "digest");
    let expires_at_ms = canonical_decimal(text(receipt, "expires_at_ms"), 20);
    let (Some(expires_at_ms), true, Some(1)) = (
        expires_at_ms,
        digest_shaped(digest),
        integer(receipt, "outcome_chunks"),
    ) else {
        return Err(MemoryError::InvalidBundle);
    };
    let outcome_node = read
        .get_node(&receipt_outcome_key(&receipt.key, 0))?
        .output
        .ok_or(MemoryError::InvalidBundle)?;
    if !exact_node(&outcome_node, LABEL_RECEIPT_OUTCOME, OUTCOME_FIELDS)
        || text(&outcome_node, "receipt_key") != receipt.key
        || integer(&outcome_node, "index") != Some(0)
    {
        return Err(MemoryError::InvalidBundle);
    }
    Ok(StoredReceipt {
        digest: digest.to_owned(),
        expires_at_ms,
        outcome: decode_outcome(text(&outcome_node, "outcome"))?,
        receipt_revision: receipt.revision,
        outcome_revision: outcome_node.revision,
    })
}

/// GetReceipt lookup of any present C1 receipt, decoded with the same strict
/// rules as replay: the summary ID and the stored expiry.
pub(super) fn lookup(
    read: &GraphRef<'_>,
    receipt: &NodeView,
    scope: &MemoryScope,
    scope_key: &str,
    collection: &str,
) -> MemoryResult<(String, u64)> {
    let stored = decode_receipt(read, receipt, scope, scope_key, collection)?;
    Ok((stored.outcome.id, stored.expires_at_ms))
}

fn derived_edge_key(summary_key: &str, source_key: &str) -> MemoryResult<String> {
    let mut input = EDGE_DOMAIN.to_vec();
    put_s(&mut input, summary_key)?;
    put_s(&mut input, source_key)?;
    Ok(format!("derived:v1:{}", hex(&Sha256::digest(input))))
}

fn string(value: impl Into<String>) -> MetadataValue {
    MetadataValue::String(value.into())
}

/// Creates the node, or replaces it at its current revision.
fn put_node(key: String, label: &str, properties: Metadata, revision: Option<u64>) -> GraphOp {
    let labels = vec![label.to_owned()];
    match revision {
        None => GraphOp::CreateNode {
            key,
            labels,
            properties,
        },
        Some(revision) => GraphOp::UpsertNode {
            key,
            labels,
            properties,
            expected: Expected::Revision(revision),
        },
    }
}

impl MemoryMut<'_> {
    pub(crate) fn consolidate(
        &mut self,
        request: &ConsolidateRequest,
    ) -> MemoryResult<ConsolidateResult> {
        let normalized = normalize(request)?;
        let now = request.now_ms;
        let summary = &request.summary;
        let count = normalized.inputs.len();
        let scope_key = request.scope.key();
        let source_digest = digest(&normalized.source_bytes);
        let request_digest = digest(&normalized.request_bytes);
        let collection = self.evidence_collection.clone().unwrap_or_default();
        let receipt_key = operation_receipt_key(OPERATION, &scope_key, &request.idempotency_key);
        let summary_key = memory_key(&scope_key, &summary.id);

        let read = self.db.graph(&self.graph)?.with_clock(now);
        let scope_node = read
            .get_node(&scope_key)?
            .output
            .ok_or(MemoryError::InvalidScope)?;
        if string_property(&scope_node, "tuple")? != hex(&request.scope.encode()) {
            return Err(MemoryError::DigestCollision);
        }
        let barrier = super::erasure::barrier(&read, &scope_key)?;
        let mut replaced = None;
        if let Some(receipt) = read.get_node(&receipt_key)?.output {
            let stored = decode_receipt(&read, &receipt, &request.scope, &scope_key, &collection)?;
            if now < stored.expires_at_ms {
                if stored.digest != request_digest {
                    return Err(match barrier {
                        Some(_) => MemoryError::DeletionPending,
                        None => MemoryError::IdempotencyConflict,
                    });
                }
                let expected = StoredOutcome {
                    id: summary.id.clone(),
                    count,
                    source_digest,
                };
                if stored.outcome != expected {
                    return Err(MemoryError::InvalidBundle);
                }
                if barrier.is_some_and(|job| stored.receipt_revision >= job) {
                    return Err(MemoryError::DeletionPending);
                }
                return Ok(ConsolidateResult {
                    summary_id: expected.id,
                    summary_revision: 1,
                    source_count: count,
                    source_digest: expected.source_digest,
                    receipt_revision: stored.receipt_revision,
                    commit_sequence: stored.receipt_revision,
                    replayed: true,
                    logical_operations: 0,
                });
            }
            replaced = Some((stored.receipt_revision, stored.outcome_revision));
        }
        if barrier.is_some() {
            return Err(MemoryError::DeletionPending);
        }

        if request.deadline_ms.is_some_and(|deadline| now >= deadline) {
            return Err(MemoryError::DeadlineExceeded);
        }
        if !request.allow_future_observations && normalized.observed_at_ms > now as i64 {
            return Err(MemoryError::FutureObservation);
        }
        if summary.expiry_ms.is_some_and(|expiry| expiry <= now) {
            return Err(MemoryError::MissingMemory);
        }
        let mut source_keys = Vec::with_capacity(count);
        for (id, revision) in &normalized.inputs {
            let key = memory_key(&scope_key, id);
            let node = read
                .get_node(&key)?
                .output
                .ok_or(MemoryError::MissingMemory)?;
            if !node.labels.contains(LABEL_MEMORY) {
                return Err(MemoryError::InvalidBundle);
            }
            let source = existing_memory(&node).map_err(|_| MemoryError::InvalidBundle)?;
            if source.scope != scope_key || &source.id != id {
                return Err(MemoryError::InvalidBundle);
            }
            if !matches!(source.kind.as_str(), "entity" | "episodic") {
                return Err(MemoryError::UnsupportedKind);
            }
            if source.memory_revision != *revision {
                return Err(MemoryError::RevisionConflict);
            }
            source_keys.push((key, *revision));
        }
        let summary_digest = content_digest(&summary.content);
        if read.get_node(&summary_key)?.output.is_some()
            || scan_memories(&read, &scope_key)?.iter().any(|memory| {
                memory.kind == "episodic"
                    && memory.digest == summary_digest
                    && memory.sources.contains(&summary.source)
            })
        {
            return Err(MemoryError::IdentityConflict);
        }
        let mut attach = None;
        if let Some(evidence) = &request.evidence {
            let collection = self
                .evidence_collection
                .as_deref()
                .ok_or(MemoryError::InvalidEvidence)?;
            let record = evidence_record_id(&evidence.id);
            let collection_id = self.db.resolve(collection)?;
            if self.db.get_at_in(collection_id, record, now)?.is_none() {
                return Err(MemoryError::InvalidEvidence);
            }
            if let Some((owner_graph, owner_key)) =
                self.db.memory_attachment_owner(collection, record, now)?
            {
                if owner_graph == self.graph
                    && let Some(owner) = read.get_node(&owner_key)?.output
                    && owner.labels.contains(LABEL_MEMORY)
                    && string_property(&owner, "scope")? != scope_key
                {
                    return Err(MemoryError::CrossScope);
                }
                return Err(MemoryError::RecordAlreadyAttached);
            }
            attach = Some((collection.to_owned(), record));
        }

        let chunks = if summary.content.len() > MAX_STRING_VALUE_BYTES {
            content_chunks(&summary.content)?
        } else {
            Vec::new()
        };
        let physical = count
            + 2 * chunks.len()
            + usize::from(attach.is_some())
            + usize::from(summary.expiry_ms.is_some())
            + 3;
        let logical = physical + if replaced.is_some() { 2 } else { 0 };
        if physical > MAX_OPS || logical > MAX_OPS {
            return Err(MemoryError::OperationLimit);
        }

        let evidence: Vec<String> = request.evidence.iter().map(|e| e.id.clone()).collect();
        let mut properties = memory_properties(MemoryProperties {
            scope: &scope_key,
            id: &summary.id,
            kind: "episodic",
            content: &summary.content,
            digest: &summary_digest,
            sources: std::slice::from_ref(&summary.source),
            provenance: &normalized.provenance,
            importance: normalized.importance,
            created_at_ms: now as i64,
            updated_at_ms: now as i64,
            observed_at_ms: normalized.observed_at_ms,
            memory_revision: 1,
            reference_count: 0,
            evidence: &evidence,
        });
        properties.insert("c1_schema".into(), string(SCHEMA));
        properties.insert(
            "c1_transformation".into(),
            string(request.transformation.as_str()),
        );
        properties.insert("c1_source_digest".into(), string(source_digest.as_str()));
        properties.insert(
            "c1_source_count".into(),
            MetadataValue::Integer(count as i64),
        );
        let mut ops = Vec::with_capacity(physical);
        ops.push(put_node(
            summary_key.clone(),
            LABEL_MEMORY,
            properties,
            None,
        ));
        if let Some(expiry) = summary.expiry_ms {
            ops.push(GraphOp::SetNodeExpiry {
                key: summary_key.clone(),
                expires_at_ms: Some(expiry),
                expected: Expected::Any,
            });
        }
        for (index, chunk) in chunks.into_iter().enumerate() {
            let chunk_key = content_key(&summary_key, index);
            let mut chunk_properties = Metadata::new();
            chunk_properties.insert("scope".into(), string(scope_key.as_str()));
            chunk_properties.insert("index".into(), MetadataValue::Integer(index as i64));
            chunk_properties.insert("content".into(), MetadataValue::String(chunk));
            ops.push(put_node(
                chunk_key.clone(),
                LABEL_CONTENT,
                chunk_properties,
                None,
            ));
            ops.push(GraphOp::CreateEdge {
                key: Some(content_edge_key(&summary_key, index)),
                source: summary_key.clone(),
                relationship: "HAS_CONTENT".into(),
                destination: chunk_key,
                properties: Metadata::new(),
            });
        }
        for (source_key, revision) in source_keys {
            let mut edge_properties = Metadata::new();
            edge_properties.insert("c1_schema".into(), string(SCHEMA));
            edge_properties.insert(
                "c1_source_memory_revision".into(),
                string(revision.to_string()),
            );
            ops.push(GraphOp::CreateEdge {
                key: Some(derived_edge_key(&summary_key, &source_key)?),
                source: summary_key.clone(),
                relationship: DERIVED_FROM.into(),
                destination: source_key,
                properties: edge_properties,
            });
        }
        if let Some((collection, record)) = attach {
            ops.push(GraphOp::Attach {
                node: summary_key.clone(),
                collection,
                record,
            });
        }
        let mut outcome_properties = Metadata::new();
        outcome_properties.insert("receipt_key".into(), string(receipt_key.as_str()));
        outcome_properties.insert("index".into(), MetadataValue::Integer(0));
        outcome_properties.insert(
            "outcome".into(),
            string(format!(
                "{SCHEMA}\n{}\n1\n{count}\n{source_digest}\n",
                hex(summary.id.as_bytes())
            )),
        );
        ops.push(put_node(
            receipt_outcome_key(&receipt_key, 0),
            LABEL_RECEIPT_OUTCOME,
            outcome_properties,
            replaced.map(|(_, outcome)| outcome),
        ));
        let mut receipt_properties = Metadata::new();
        receipt_properties.insert("scope".into(), string(scope_key.as_str()));
        receipt_properties.insert(
            "c1_scope_tuple".into(),
            string(hex(&request.scope.encode())),
        );
        receipt_properties.insert("c1_schema".into(), string(SCHEMA));
        receipt_properties.insert("c1_collection".into(), string(collection));
        receipt_properties.insert("digest".into(), string(request_digest));
        receipt_properties.insert(
            "expires_at_ms".into(),
            string((now + request.receipt_window_ms).to_string()),
        );
        receipt_properties.insert("outcome_chunks".into(), MetadataValue::Integer(1));
        ops.push(put_node(
            receipt_key,
            LABEL_RECEIPT,
            receipt_properties,
            replaced.map(|(receipt, _)| receipt),
        ));
        debug_assert_eq!(ops.len(), physical);

        let batch = self.db.graph_mut(&self.graph)?.write_batch_at(&ops, now)?;
        let (Some(commit_sequence), Some(GraphOpOutcome::Node(receipt))) =
            (batch.revision, batch.outcomes.last())
        else {
            return Err(MemoryError::Storage(Error::InvalidQuery(
                "consolidation receipt was not written",
            )));
        };
        Ok(ConsolidateResult {
            summary_id: summary.id.clone(),
            summary_revision: 1,
            source_count: count,
            source_digest,
            receipt_revision: receipt.revision,
            commit_sequence,
            replayed: false,
            logical_operations: logical,
        })
    }
}

#[cfg(test)]
mod tests;
