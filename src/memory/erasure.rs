//! Gap 1 E1: crate-private exact-scope erasure barrier and pending job.
//!
//! Contract: `docs/projects/roadmap/gap1-e1-erasure-barrier-contract.md`
//! (ratified, provisional). Beginning erasure writes one graph frame with the
//! singleton job, the begin receipt outcome and the begin receipt. Job-node
//! presence is the permanent barrier; E1 deletes nothing.

use sha2::{Digest, Sha256};

use super::{
    LABEL_RECEIPT, LABEL_RECEIPT_OUTCOME, MAX_STRING_VALUE_BYTES, MemoryError, MemoryMut,
    MemoryRef, MemoryResult, MemoryScope, hex, opaque, operation_receipt_key, receipt_outcome_key,
    string_property,
};
use crate::{
    Metadata, MetadataValue,
    property_graph::{GraphOp, GraphOpOutcome, GraphRef, NodeView},
};

const SCHEMA: &str = "erase-scope:v1";
const LABEL_JOB: &str = "VliteMemoryErasureJob";
const BEGIN_DOMAIN: &[u8] = b"vlite.memory.erasure.begin.v1\0";
const MAX_COLLECTION_BYTES: usize = 63;

#[derive(Clone, Debug)]
pub(crate) struct EraseScopeRequest {
    pub scope: MemoryScope,
    pub idempotency_key: String,
    pub expected_scope_revision: u64,
    pub now_ms: u64,
    pub receipt_window_ms: u64,
    pub deadline_ms: Option<u64>,
}

#[derive(Clone, Debug)]
pub(crate) struct ErasureStatusRequest {
    pub scope: MemoryScope,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ErasureState {
    Pending,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LogicalVisibility {
    Hidden,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ErasureStatus {
    pub job_key: String,
    pub revision: u64,
    pub generation: u64,
    pub state: ErasureState,
    pub logical_visibility: LogicalVisibility,
    pub deleted_count: u64,
    pub remaining: Option<u64>,
    pub resume_token: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BeginErasureResult {
    pub status: ErasureStatus,
    pub replayed: bool,
}

#[derive(Clone, Copy)]
enum Field {
    String,
    Integer,
}

const JOB_FIELDS: &[(&str, Field)] = &[
    ("e1_schema", Field::String),
    ("scope", Field::String),
    ("e1_scope_tuple", Field::String),
    ("e1_collection", Field::String),
    ("begin_digest", Field::String),
    ("generation", Field::String),
    ("state", Field::String),
    ("logical_visibility", Field::String),
    ("deleted_count", Field::String),
    ("remaining", Field::String),
    ("created_at_ms", Field::Integer),
];

const RECEIPT_FIELDS: &[(&str, Field)] = &[
    ("e1_schema", Field::String),
    ("scope", Field::String),
    ("e1_scope_tuple", Field::String),
    ("e1_collection", Field::String),
    ("digest", Field::String),
    ("expires_at_ms", Field::String),
    ("job_key", Field::String),
    ("outcome_chunks", Field::Integer),
];

const OUTCOME_FIELDS: &[(&str, Field)] = &[
    ("receipt_key", Field::String),
    ("outcome", Field::String),
    ("index", Field::Integer),
];

/// `JobKey`: the per-scope singleton (contract section 3).
fn job_key(scope_key: &str) -> String {
    operation_receipt_key("erase-scope-job", scope_key, "singleton")
}

fn begin_receipt_key(scope_key: &str, idempotency_key: &str) -> String {
    operation_receipt_key("begin-erasure", scope_key, idempotency_key)
}

/// `BeginBytes` (contract section 3); `collection` is the binding.
fn begin_bytes(
    scope: &MemoryScope,
    collection: Option<&str>,
    expected_scope_revision: u64,
    receipt_window_ms: u64,
) -> Vec<u8> {
    let scope_bytes = scope.encode();
    let mut bytes = BEGIN_DOMAIN.to_vec();
    bytes.extend_from_slice(&(scope_bytes.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&scope_bytes);
    match collection {
        None => bytes.push(0),
        Some(name) => {
            bytes.push(1);
            bytes.extend_from_slice(&(name.len() as u32).to_be_bytes());
            bytes.extend_from_slice(name.as_bytes());
        }
    }
    bytes.extend_from_slice(&expected_scope_revision.to_be_bytes());
    bytes.extend_from_slice(&receipt_window_ms.to_be_bytes());
    bytes
}

fn begin_digest(bytes: &[u8]) -> String {
    format!("v1:{}", hex(&Sha256::digest(bytes)))
}

/// The exact five-line LF outcome (contract section 4).
fn outcome_text(job_key: &str) -> String {
    format!("{SCHEMA}\n{job_key}\n1\npending\nunknown\n")
}

fn digest_shaped(value: &str) -> bool {
    value.len() == 67
        && value.starts_with("v1:")
        && value[3..]
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Canonical nonzero u64 decimal: no sign, leading zero or whitespace.
fn canonical_nonzero(value: &str) -> Option<u64> {
    let bytes = value.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 20
        || !matches!(bytes[0], b'1'..=b'9')
        || !bytes.iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    value.parse().ok()
}

/// Exactly one label and exactly these property names and types; every
/// String property within the 4096-byte bound.
fn exact(node: &NodeView, label: &str, fields: &[(&str, Field)]) -> MemoryResult<()> {
    let typed = fields.iter().all(|(name, field)| {
        matches!(
            (node.properties.get(*name), field),
            (Some(MetadataValue::String(_)), Field::String)
                | (Some(MetadataValue::Integer(_)), Field::Integer)
        )
    });
    let bounded = node.properties.values().all(|value| match value {
        MetadataValue::String(value) => value.len() <= MAX_STRING_VALUE_BYTES,
        _ => true,
    });
    if node.labels.len() != 1
        || !node.labels.contains(label)
        || node.properties.len() != fields.len()
        || !typed
        || !bounded
        || node.expires_at_ms.is_some()
    {
        return Err(MemoryError::InvalidBundle);
    }
    Ok(())
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

/// Schema, scope key, tuple and binding shared by the job and the receipt.
fn common(
    node: &NodeView,
    scope: &MemoryScope,
    scope_key: &str,
    binding: &str,
) -> MemoryResult<()> {
    if text(node, "e1_schema") != SCHEMA || text(node, "scope") != scope_key {
        return Err(MemoryError::InvalidBundle);
    }
    if text(node, "e1_scope_tuple") != hex(&scope.encode()) {
        return Err(MemoryError::DigestCollision);
    }
    if text(node, "e1_collection") != binding {
        return Err(MemoryError::InvalidBundle);
    }
    Ok(())
}

struct Job {
    begin_digest: String,
    revision: u64,
}

fn decode_job(
    node: &NodeView,
    scope: &MemoryScope,
    scope_key: &str,
    binding: &str,
) -> MemoryResult<Job> {
    exact(node, LABEL_JOB, JOB_FIELDS)?;
    common(node, scope, scope_key, binding)?;
    if !digest_shaped(text(node, "begin_digest"))
        || text(node, "generation") != "1"
        || text(node, "state") != "pending"
        || text(node, "logical_visibility") != "hidden"
        || text(node, "deleted_count") != "0"
        || text(node, "remaining") != "unknown"
        || integer(node, "created_at_ms").is_none_or(|created| created < 0)
    {
        return Err(MemoryError::InvalidBundle);
    }
    Ok(Job {
        begin_digest: text(node, "begin_digest").to_owned(),
        revision: node.revision,
    })
}

/// Strict begin receipt decoding, live or expired; returns digest and expiry.
fn decode_receipt(
    read: &GraphRef<'_>,
    receipt: &NodeView,
    scope: &MemoryScope,
    scope_key: &str,
    binding: &str,
    job: Option<&Job>,
) -> MemoryResult<(String, u64)> {
    exact(receipt, LABEL_RECEIPT, RECEIPT_FIELDS)?;
    common(receipt, scope, scope_key, binding)?;
    let digest = text(receipt, "digest");
    if !digest_shaped(digest)
        || text(receipt, "job_key") != job_key(scope_key)
        || integer(receipt, "outcome_chunks") != Some(1)
    {
        return Err(MemoryError::InvalidBundle);
    }
    let expiry =
        canonical_nonzero(text(receipt, "expires_at_ms")).ok_or(MemoryError::InvalidBundle)?;
    let job = job.ok_or(MemoryError::InvalidBundle)?;
    if digest != job.begin_digest || receipt.revision != job.revision {
        return Err(MemoryError::InvalidBundle);
    }
    if read
        .get_node(&receipt_outcome_key(&receipt.key, 1))?
        .output
        .is_some()
    {
        return Err(MemoryError::InvalidBundle);
    }
    let outcome = read
        .get_node(&receipt_outcome_key(&receipt.key, 0))?
        .output
        .ok_or(MemoryError::InvalidBundle)?;
    exact(&outcome, LABEL_RECEIPT_OUTCOME, OUTCOME_FIELDS)?;
    if text(&outcome, "receipt_key") != receipt.key
        || integer(&outcome, "index") != Some(0)
        || text(&outcome, "outcome") != outcome_text(&job_key(scope_key))
    {
        return Err(MemoryError::InvalidBundle);
    }
    Ok((digest.to_owned(), expiry))
}

fn pending(job_key: String, revision: u64) -> ErasureStatus {
    ErasureStatus {
        job_key,
        revision,
        generation: 1,
        state: ErasureState::Pending,
        logical_visibility: LogicalVisibility::Hidden,
        deleted_count: 0,
        remaining: None,
        resume_token: None,
    }
}

/// Barrier probe by exact key: presence only, returning the job revision.
pub(super) fn barrier(read: &GraphRef<'_>, scope_key: &str) -> MemoryResult<Option<u64>> {
    Ok(read
        .get_node(&job_key(scope_key))?
        .output
        .map(|job| job.revision))
}

/// Reads the provisioned scope node and verifies its exact tuple.
fn scope_node(read: &GraphRef<'_>, scope: &MemoryScope, scope_key: &str) -> MemoryResult<NodeView> {
    let node = read
        .get_node(scope_key)?
        .output
        .ok_or(MemoryError::InvalidScope)?;
    if string_property(&node, "tuple")? != hex(&scope.encode()) {
        return Err(MemoryError::DigestCollision);
    }
    Ok(node)
}

impl MemoryRef<'_> {
    /// The provisioned scope node's graph revision; barrier-exempt, read-only.
    pub(crate) fn scope_revision(&self, scope: &MemoryScope) -> MemoryResult<u64> {
        scope.validate()?;
        let read = self.db.graph(&self.graph)?;
        Ok(scope_node(&read, scope, &scope.key())?.revision)
    }

    /// Strict, read-only current job status; allowed through the barrier.
    pub(crate) fn erasure_status(
        &self,
        request: &ErasureStatusRequest,
    ) -> MemoryResult<ErasureStatus> {
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
        Ok(pending(key, job.revision))
    }
}

impl MemoryMut<'_> {
    /// Begins logical erasure of exactly one provisioned scope (section 2).
    pub(crate) fn begin_erasure(
        &mut self,
        request: &EraseScopeRequest,
    ) -> MemoryResult<BeginErasureResult> {
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
        if request.expected_scope_revision == 0 {
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
        let scope_revision = scope_node(&read, scope, &scope_key)?.revision;
        let job_key = job_key(&scope_key);
        let job = read
            .get_node(&job_key)?
            .output
            .map(|node| decode_job(&node, scope, &scope_key, binding))
            .transpose()?;
        let bytes = begin_bytes(
            scope,
            collection.as_deref(),
            request.expected_scope_revision,
            request.receipt_window_ms,
        );
        let digest = begin_digest(&bytes);
        let receipt_key = begin_receipt_key(&scope_key, &request.idempotency_key);
        if let Some(receipt) = read.get_node(&receipt_key)?.output {
            let (stored, expiry) =
                decode_receipt(&read, &receipt, scope, &scope_key, binding, job.as_ref())?;
            if now < expiry {
                if stored != digest {
                    return Err(MemoryError::IdempotencyConflict);
                }
                return Ok(BeginErasureResult {
                    status: pending(job_key, receipt.revision),
                    replayed: true,
                });
            }
        }
        if job.is_some() {
            return Err(MemoryError::JobConflict);
        }
        if request.deadline_ms.is_some_and(|deadline| now >= deadline) {
            return Err(MemoryError::DeadlineExceeded);
        }
        if scope_revision != request.expected_scope_revision {
            return Err(MemoryError::RevisionConflict);
        }

        let string = |value: &str| MetadataValue::String(value.to_owned());
        let mut common = Metadata::new();
        common.insert("e1_schema".into(), string(SCHEMA));
        common.insert("scope".into(), string(&scope_key));
        common.insert("e1_scope_tuple".into(), string(&hex(&scope.encode())));
        common.insert("e1_collection".into(), string(binding));
        let mut job_properties = common.clone();
        job_properties.insert("begin_digest".into(), string(&digest));
        for (name, value) in [
            ("generation", "1"),
            ("state", "pending"),
            ("logical_visibility", "hidden"),
            ("deleted_count", "0"),
            ("remaining", "unknown"),
        ] {
            job_properties.insert(name.into(), string(value));
        }
        job_properties.insert("created_at_ms".into(), MetadataValue::Integer(now as i64));
        let mut outcome_properties = Metadata::new();
        outcome_properties.insert("receipt_key".into(), string(&receipt_key));
        outcome_properties.insert("outcome".into(), string(&outcome_text(&job_key)));
        outcome_properties.insert("index".into(), MetadataValue::Integer(0));
        let mut receipt_properties = common;
        receipt_properties.insert("digest".into(), string(&digest));
        receipt_properties.insert(
            "expires_at_ms".into(),
            string(&(now + request.receipt_window_ms).to_string()),
        );
        receipt_properties.insert("job_key".into(), string(&job_key));
        receipt_properties.insert("outcome_chunks".into(), MetadataValue::Integer(1));
        #[cfg_attr(not(test), allow(unused_mut))]
        let mut ops = vec![
            GraphOp::CreateNode {
                key: job_key.clone(),
                labels: vec![LABEL_JOB.into()],
                properties: job_properties,
            },
            GraphOp::CreateNode {
                key: receipt_outcome_key(&receipt_key, 0),
                labels: vec![LABEL_RECEIPT_OUTCOME.into()],
                properties: outcome_properties,
            },
            GraphOp::CreateNode {
                key: receipt_key,
                labels: vec![LABEL_RECEIPT.into()],
                properties: receipt_properties,
            },
        ];
        #[cfg(test)]
        super::FAIL_NEXT_WRITE_BATCH.with(|fail| {
            if fail.replace(false) {
                ops.push(GraphOp::CreateNode {
                    key: scope_key.clone(),
                    labels: vec![super::LABEL_SCOPE.into()],
                    properties: Metadata::new(),
                });
            }
        });
        let batch = self.db.graph_mut(&self.graph)?.write_batch_at(&ops, now)?;
        let revision = match batch.outcomes.first() {
            Some(GraphOpOutcome::Node(write)) => write.revision,
            _ => {
                return Err(MemoryError::Storage(crate::Error::InvalidQuery(
                    "erasure job was not created",
                )));
            }
        };
        Ok(BeginErasureResult {
            status: pending(job_key, revision),
            replayed: false,
        })
    }
}

#[cfg(test)]
mod tests;
