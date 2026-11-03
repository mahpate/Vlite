//! Crate-private, provisional agent-memory write core.
//!
//! Receipts and memory nodes are graph nodes in one `GraphMut::write_batch`.
//! The graph batch is the transaction boundary; no KV write or second frame
//! participates in a remember operation.

use std::collections::{BTreeMap, BTreeSet};

mod consolidate;
mod ranking;
mod recall;

use sha2::{Digest, Sha256};

use crate::{
    Database, Error, Expected, Metadata, MetadataValue, Result as VliteResult,
    property_graph::{
        Direction, EdgeRef, FindNodes, GraphLimits, GraphOp, GraphRef, Neighbors, NodeSelector,
        NodeView, ReadOptions,
    },
};

#[cfg(test)]
thread_local! {
    static FAIL_NEXT_WRITE_BATCH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static BUMP_BEFORE_WRITE_BATCH: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
    static BUMPED_BATCH_SEQUENCE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static BUMPED_BATCH_END: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

const LABEL_MEMORY: &str = "VliteMemory";
const LABEL_RECEIPT: &str = "VliteMemoryReceipt";
const LABEL_RECEIPT_OUTCOME: &str = "VliteMemoryReceiptOutcome";
const LABEL_CONTENT: &str = "VliteMemoryContent";
const LABEL_SCOPE: &str = "VliteMemoryScope";
const MAX_OPS: usize = 4_096;
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
const MAX_PROVENANCE: usize = 64;
const MAX_OPAQUE_BYTES: usize = 256;
const MAX_STRING_VALUE_BYTES: usize = 4_096;

type MemoryResult<T> = std::result::Result<T, MemoryError>;

#[derive(Debug)]
pub(crate) enum MemoryError {
    InvalidScope,
    InvalidKey,
    InvalidClock,
    InvalidReceiptWindow,
    InvalidExpiry,
    InvalidEvidence,
    DeadlineExceeded,
    InvalidBundle,
    DuplicateItem,
    OperationLimit,
    BodyLimit,
    InvalidIdentity,
    UnsupportedKind,
    InvalidContent,
    InvalidImportance,
    InvalidObservation,
    FutureObservation,
    InvalidProvenance,
    ProvenanceLimit,
    InvalidReference,
    MissingReference,
    CrossScope,
    DigestCollision,
    IdempotencyConflict,
    IdentityConflict,
    MissingMemory,
    MergeIdentityConflict,
    ExpectedRevisionRequired,
    RevisionConflict,
    RecordAlreadyAttached,
    ForeignAttachment,
    InvalidTarget,
    DuplicateTarget,
    Storage(Error),
}

impl From<Error> for MemoryError {
    fn from(error: Error) -> Self {
        match error {
            Error::BatchTooLarge => Self::BodyLimit,
            Error::GraphRecordNotAttachable | Error::GraphAttachmentConflict => {
                Self::RecordAlreadyAttached
            }
            other => Self::Storage(other),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MemoryScope {
    pub tenant: String,
    pub agent: String,
    pub user: Option<String>,
    pub session: Option<String>,
}

impl MemoryScope {
    fn validate(&self) -> MemoryResult<()> {
        if !opaque(&self.tenant)
            || !opaque(&self.agent)
            || self.user.as_deref().is_some_and(|value| !opaque(value))
            || self.session.as_deref().is_some_and(|value| !opaque(value))
            || (self.session.is_some() && self.user.is_none())
        {
            return Err(MemoryError::InvalidScope);
        }
        Ok(())
    }

    fn encode(&self) -> Vec<u8> {
        let mut bytes = b"scope:v1".to_vec();
        for value in [
            Some(self.tenant.as_str()),
            Some(self.agent.as_str()),
            self.user.as_deref(),
            self.session.as_deref(),
        ] {
            match value {
                None => bytes.push(0),
                Some(value) => {
                    bytes.push(1);
                    bytes.extend_from_slice(&(value.len() as u32).to_be_bytes());
                    bytes.extend_from_slice(value.as_bytes());
                }
            }
        }
        bytes
    }

    fn key(&self) -> String {
        format!("scope:v1:{}", hex(&Sha256::digest(self.encode())))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MemoryKind {
    Entity,
    Episodic,
    Unsupported,
}

impl MemoryKind {
    fn label(self) -> &'static str {
        match self {
            Self::Entity => "entity",
            Self::Episodic => "episodic",
            Self::Unsupported => "unsupported",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Provenance {
    pub source: String,
    pub kind: Option<String>,
    pub uri: Option<String>,
    pub transformation: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct MemoryItem {
    pub id: String,
    pub kind: MemoryKind,
    pub content: String,
    pub source: String,
    pub importance: Option<f64>,
    pub observed_at_ms: Option<i64>,
    pub provenance: Vec<Provenance>,
    pub references: Vec<String>,
    pub merge_into: Option<String>,
    pub expected_revision: Option<u64>,
    pub expiry_ms: Option<u64>,
    pub expiry_valid: bool,
    pub evidence: Vec<MemoryEvidence>,
}

#[derive(Clone, Debug)]
pub(crate) struct MemoryEvidence {
    pub id: String,
    pub scope: MemoryScope,
}

#[derive(Clone, Debug)]
pub(crate) struct RememberRequest {
    pub scope: MemoryScope,
    pub idempotency_key: String,
    pub receipt_window_ms: u64,
    pub now_ms: u64,
    pub deadline_ms: Option<u64>,
    pub allow_future_observations: bool,
    pub items: Vec<MemoryItem>,
}

#[derive(Clone, Debug)]
pub(crate) struct ForgetTarget {
    pub id: String,
    pub expected_revision: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct ForgetRequest {
    pub scope: MemoryScope,
    pub idempotency_key: String,
    pub receipt_window_ms: u64,
    pub now_ms: u64,
    pub deadline_ms: Option<u64>,
    pub targets: Vec<ForgetTarget>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ForgetResult {
    pub deleted: Vec<String>,
    pub receipt_revision: u64,
    pub commit_sequence: u64,
    pub replayed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MemoryAction {
    pub action: &'static str,
    pub id: String,
    pub revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RememberResult {
    pub actions: Vec<MemoryAction>,
    pub receipt_revision: u64,
    pub commit_sequence: u64,
    pub replayed: bool,
}

/// A checked binding to an already-created graph and optional evidence collection.
pub(crate) struct MemoryRef<'db> {
    db: &'db Database,
    graph: String,
    evidence_collection: Option<String>,
}

pub(crate) struct MemoryMut<'db> {
    db: &'db mut Database,
    graph: String,
    evidence_collection: Option<String>,
}

impl<'db> MemoryRef<'db> {
    pub(crate) fn bind(
        db: &'db Database,
        graph: &str,
        evidence_collection: Option<&str>,
    ) -> MemoryResult<Self> {
        db.graph(graph)?;
        if let Some(collection) = evidence_collection {
            db.collection(collection)?;
        }
        Ok(Self {
            db,
            graph: graph.to_owned(),
            evidence_collection: evidence_collection.map(str::to_owned),
        })
    }

    pub(crate) fn graph(&self) -> VliteResult<GraphRef<'_>> {
        self.db.graph(&self.graph)
    }
}

impl<'db> MemoryMut<'db> {
    pub(crate) fn bind(
        db: &'db mut Database,
        graph: &str,
        evidence_collection: Option<&str>,
    ) -> MemoryResult<Self> {
        db.graph(graph)?;
        if let Some(collection) = evidence_collection {
            db.collection(collection)?;
        }
        Ok(Self {
            db,
            graph: graph.to_owned(),
            evidence_collection: evidence_collection.map(str::to_owned),
        })
    }

    pub(crate) fn provision_scope(&mut self, scope: &MemoryScope) -> MemoryResult<()> {
        scope.validate()?;
        let key = scope.key();
        let tuple = hex(&scope.encode());
        if let Some(node) = self.db.graph(&self.graph)?.get_node(&key)?.output {
            if string_property(&node, "tuple")? != tuple {
                return Err(MemoryError::DigestCollision);
            }
            return Ok(());
        }
        let mut properties = Metadata::new();
        properties.insert("tuple".into(), MetadataValue::String(tuple));
        self.db
            .graph_mut(&self.graph)?
            .write_batch(&[GraphOp::CreateNode {
                key,
                labels: vec![LABEL_SCOPE.into()],
                properties,
            }])?;
        Ok(())
    }

    pub(crate) fn remember(&mut self, request: &RememberRequest) -> MemoryResult<RememberResult> {
        let normalized = validate_request(request)?;
        let scope_key = request.scope.key();
        let scope_node = self
            .db
            .graph(&self.graph)?
            .with_clock(request.now_ms)
            .get_node(&scope_key)?
            .output
            .ok_or(MemoryError::InvalidScope)?;
        if string_property(&scope_node, "tuple")? != hex(&request.scope.encode()) {
            return Err(MemoryError::DigestCollision);
        }
        let receipt_key = receipt_key(&scope_key, &request.idempotency_key);
        let digest = request_digest(request, &normalized);
        let receipt_read = self
            .db
            .graph(&self.graph)?
            .with_clock(request.now_ms)
            .get_node(&receipt_key)?;
        if let Some(receipt) = receipt_read.output {
            let expired = receipt
                .properties
                .get("expires_at_ms")
                .and_then(|value| match value {
                    MetadataValue::String(value) => value.parse::<u64>().ok(),
                    _ => None,
                })
                .is_some_and(|expiry| request.now_ms >= expiry);
            if expired {
                // Expired receipts are replaced by the retry's new result below.
            } else {
                verify_scope(&receipt, &scope_key)?;
                let stored_digest = string_property(&receipt, "digest")?;
                if stored_digest != digest {
                    return Err(MemoryError::IdempotencyConflict);
                }
                let outcome = decode_outcome(
                    &self.db.graph(&self.graph)?.with_clock(request.now_ms),
                    &receipt,
                )?;
                return decode_result(&outcome, receipt.revision, true, &normalized);
            }
        }
        if request
            .deadline_ms
            .is_some_and(|deadline| request.now_ms >= deadline)
        {
            return Err(MemoryError::DeadlineExceeded);
        }
        if !request.allow_future_observations
            && normalized
                .iter()
                .any(|item| item.observed_at_ms > request.now_ms as i64)
        {
            return Err(MemoryError::FutureObservation);
        }

        let read = self.db.graph(&self.graph)?.with_clock(request.now_ms);
        if let Some(collection) = self.evidence_collection.as_deref() {
            for evidence in normalized.iter().flat_map(|item| &item.evidence) {
                if let Some((owner_graph, owner_key)) = self.db.memory_attachment_owner(
                    collection,
                    evidence_record_id(&evidence.id),
                    request.now_ms,
                )? && owner_graph == self.graph
                    && let Some(owner) = read.get_node(&owner_key)?.output
                    && owner.labels.contains(LABEL_MEMORY)
                    && string_property(&owner, "scope")? != scope_key
                {
                    return Err(MemoryError::CrossScope);
                }
            }
        }
        let mut existing = scan_memories(&read, &scope_key)?;
        let planned = plan(
            request,
            &normalized,
            &scope_key,
            self.evidence_collection.as_deref(),
            &mut existing,
        )?;
        if planned.encoded_request_bytes > MAX_BODY_BYTES {
            return Err(MemoryError::BodyLimit);
        }
        let outcome = encode_actions(&planned.actions);
        let outcome_chunk_count = outcome.len().div_ceil(MAX_STRING_VALUE_BYTES);
        if planned.budget_operations + 1 + outcome_chunk_count > MAX_OPS {
            return Err(MemoryError::OperationLimit);
        }
        let mut receipt_properties = Metadata::new();
        receipt_properties.insert("scope".into(), MetadataValue::String(scope_key.clone()));
        receipt_properties.insert("digest".into(), MetadataValue::String(digest));
        receipt_properties.insert(
            "expires_at_ms".into(),
            MetadataValue::String((request.now_ms + request.receipt_window_ms).to_string()),
        );
        let mut ops = planned.ops;
        let graph = self.db.graph(&self.graph)?.with_clock(request.now_ms);
        for outcome_op in encode_outcome(&receipt_key, &outcome, &mut receipt_properties)? {
            match outcome_op {
                GraphOp::CreateNode {
                    key,
                    labels,
                    properties,
                } => {
                    if let Some(node) = graph.get_node(&key)?.output {
                        ops.push(GraphOp::UpsertNode {
                            key,
                            labels,
                            properties,
                            expected: Expected::Revision(node.revision),
                        });
                    } else {
                        ops.push(GraphOp::CreateNode {
                            key,
                            labels,
                            properties,
                        });
                    }
                }
                _ => unreachable!("outcome encoding only creates nodes"),
            }
        }
        if let Some(receipt) = graph.get_node(&receipt_key)?.output {
            ops.push(GraphOp::UpsertNode {
                key: receipt_key.clone(),
                labels: vec![LABEL_RECEIPT.into()],
                properties: receipt_properties,
                expected: Expected::Revision(receipt.revision),
            });
        } else {
            ops.push(GraphOp::CreateNode {
                key: receipt_key.clone(),
                labels: vec![LABEL_RECEIPT.into()],
                properties: receipt_properties,
            });
        }
        if ops.len() > MAX_OPS {
            return Err(MemoryError::OperationLimit);
        }

        #[cfg(test)]
        if let Some(key) = BUMP_BEFORE_WRITE_BATCH.with(|target| target.borrow_mut().take()) {
            let node = self
                .db
                .graph(&self.graph)?
                .get_node(&key)?
                .output
                .ok_or(MemoryError::MissingMemory)?;
            self.db
                .graph_mut(&self.graph)?
                .write_batch(&[GraphOp::UpsertNode {
                    key,
                    labels: node.labels.into_iter().collect(),
                    properties: node.properties,
                    expected: Expected::Revision(node.revision),
                }])?;
            BUMPED_BATCH_SEQUENCE.with(|sequence| sequence.set(self.db.sequence));
            BUMPED_BATCH_END.with(|end| end.set(self.db.end));
        }
        #[cfg(test)]
        FAIL_NEXT_WRITE_BATCH.with(|fail| {
            if fail.replace(false) {
                ops.push(GraphOp::CreateNode {
                    key: scope_key.clone(),
                    labels: vec![LABEL_SCOPE.into()],
                    properties: Metadata::new(),
                });
            }
        });
        let batch = self
            .db
            .graph_mut(&self.graph)?
            .write_batch_at(&ops, request.now_ms)?;
        let receipt_write = batch
            .outcomes
            .last()
            .and_then(|outcome| match outcome {
                crate::property_graph::GraphOpOutcome::Node(write) => Some(*write),
                _ => None,
            })
            .ok_or(MemoryError::Storage(Error::InvalidQuery(
                "memory receipt was not created",
            )))?;
        Ok(RememberResult {
            actions: planned.actions,
            receipt_revision: receipt_write.revision,
            commit_sequence: receipt_write.revision,
            replayed: false,
        })
    }

    pub(crate) fn forget(&mut self, request: &ForgetRequest) -> MemoryResult<ForgetResult> {
        validate_forget_request(request)?;
        let scope_key = request.scope.key();
        let read = self.db.graph(&self.graph)?.with_clock(request.now_ms);
        let scope_node = read
            .get_node(&scope_key)?
            .output
            .ok_or(MemoryError::InvalidScope)?;
        if string_property(&scope_node, "tuple")? != hex(&request.scope.encode()) {
            return Err(MemoryError::DigestCollision);
        }
        let key = forget_receipt_key(&scope_key, &request.idempotency_key);
        let digest = forget_request_digest(request);
        let existing_receipt = read.get_node(&key)?.output;
        if let Some(receipt) = &existing_receipt
            && !receipt_expired(receipt, request.now_ms)
        {
            verify_scope(receipt, &scope_key)?;
            if string_property(receipt, "digest")? != digest {
                return Err(MemoryError::IdempotencyConflict);
            }
            let outcome = decode_outcome(&read, receipt)?;
            return decode_forget_result(&outcome, receipt.revision, true, &request.targets);
        }
        if request
            .deadline_ms
            .is_some_and(|deadline| request.now_ms >= deadline)
        {
            return Err(MemoryError::DeadlineExceeded);
        }

        let memories = scan_memories(&read, &scope_key)?;
        let mut targets = Vec::with_capacity(request.targets.len());
        let mut target_keys = BTreeSet::new();
        for target in &request.targets {
            let memory = memories
                .iter()
                .find(|memory| memory.id == target.id)
                .cloned()
                .ok_or(MemoryError::MissingMemory)?;
            if memory.memory_revision != target.expected_revision {
                return Err(MemoryError::RevisionConflict);
            }
            target_keys.insert(memory.key.clone());
            targets.push(memory);
        }

        let mut incident_edges = BTreeMap::new();
        for memory in &targets {
            let mut cursor = None;
            loop {
                let result = read.neighbors(
                    &Neighbors {
                        node: NodeSelector::key(&memory.key),
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
                for edge in result.output.edges {
                    incident_edges.insert(edge.id, edge);
                }
                if incident_edges.len() > MAX_OPS {
                    return Err(MemoryError::OperationLimit);
                }
                match result.cursor {
                    Some(next) => cursor = Some(next),
                    None if result.complete => break,
                    None => return Err(MemoryError::OperationLimit),
                }
            }
        }
        if incident_edges.len() > MAX_OPS {
            return Err(MemoryError::OperationLimit);
        }

        let mut owned_evidence = BTreeMap::<String, (String, u64)>::new();
        let evidence_collection_id = self
            .evidence_collection
            .as_deref()
            .map(|collection| self.db.resolve(collection))
            .transpose()?;
        for memory in &targets {
            let attachments = read.memory_node_attachments(&memory.key)?;
            if memory.evidence.is_empty() {
                if !attachments.is_empty() {
                    return Err(MemoryError::ForeignAttachment);
                }
                continue;
            }
            let evidence_collection_id =
                evidence_collection_id.ok_or(MemoryError::InvalidEvidence)?;
            if attachments
                .iter()
                .any(|(collection, _)| *collection != evidence_collection_id)
            {
                return Err(MemoryError::ForeignAttachment);
            }
            let attached_record = attachments
                .iter()
                .find(|(collection, _)| *collection == evidence_collection_id)
                .map(|(_, record)| *record);
            if memory.evidence.len() != 1 {
                return Err(MemoryError::InvalidBundle);
            }
            let evidence_id = memory.evidence[0].clone();
            let record = evidence_record_id(&evidence_id);
            let evidence_collection = self
                .evidence_collection
                .as_deref()
                .ok_or(MemoryError::InvalidEvidence)?;
            let owner =
                self.db
                    .memory_attachment_owner(evidence_collection, record, request.now_ms)?;
            if owner.as_ref() != Some(&(self.graph.clone(), memory.key.clone())) {
                return Err(MemoryError::ForeignAttachment);
            }
            if attached_record != Some(record) {
                return Err(MemoryError::InvalidBundle);
            }
            owned_evidence.insert(evidence_id, (memory.key.clone(), record));
        }

        let retained_evidence = retained_evidence_ids(&read, &memories, &target_keys, &scope_key)?;
        let mut ops = Vec::new();
        if !owned_evidence.is_empty() {
            let evidence_collection = self
                .evidence_collection
                .as_deref()
                .ok_or(MemoryError::InvalidEvidence)?;
            for (evidence_id, (node, record)) in &owned_evidence {
                ops.push(GraphOp::Detach {
                    node: node.clone(),
                    collection: evidence_collection.to_owned(),
                });
                if !retained_evidence.contains(evidence_id) {
                    ops.push(GraphOp::Vector {
                        collection: evidence_collection.to_owned(),
                        mutation: crate::Mutation::Delete { id: *record },
                    });
                }
            }
        }
        for edge in incident_edges.values() {
            ops.push(GraphOp::DeleteEdge {
                edge: EdgeRef::Id(edge.id),
                expected: Expected::Revision(edge.revision),
            });
        }
        for edge in incident_edges
            .values()
            .filter(|edge| edge.relationship == "HAS_CONTENT")
        {
            if !target_keys.contains(&edge.source_key) {
                continue;
            }
            let node = read
                .get_node(&edge.target_key)?
                .output
                .ok_or(MemoryError::InvalidBundle)?;
            ops.push(GraphOp::DeleteNode {
                key: node.key,
                expected: Expected::Revision(node.revision),
            });
        }
        for memory in &targets {
            ops.push(GraphOp::DeleteNode {
                key: memory.key.clone(),
                expected: Expected::Revision(memory.graph_revision),
            });
        }

        let deleted = request
            .targets
            .iter()
            .map(|target| target.id.clone())
            .collect::<Vec<_>>();
        let encoded = encode_forget_result(&deleted);
        let mut receipt_properties = Metadata::new();
        receipt_properties.insert("scope".into(), MetadataValue::String(scope_key.clone()));
        receipt_properties.insert("digest".into(), MetadataValue::String(digest));
        receipt_properties.insert(
            "expires_at_ms".into(),
            MetadataValue::String((request.now_ms + request.receipt_window_ms).to_string()),
        );
        for outcome_op in encode_outcome(&key, &encoded, &mut receipt_properties)? {
            let GraphOp::CreateNode {
                key: chunk_key,
                labels,
                properties,
            } = outcome_op
            else {
                unreachable!("outcome encoding only creates nodes")
            };
            if let Some(node) = read.get_node(&chunk_key)?.output {
                ops.push(GraphOp::UpsertNode {
                    key: chunk_key,
                    labels,
                    properties,
                    expected: Expected::Revision(node.revision),
                });
            } else {
                ops.push(GraphOp::CreateNode {
                    key: chunk_key,
                    labels,
                    properties,
                });
            }
        }
        if let Some(receipt) = &existing_receipt {
            ops.push(GraphOp::UpsertNode {
                key: key.clone(),
                labels: vec![LABEL_RECEIPT.into()],
                properties: receipt_properties,
                expected: Expected::Revision(receipt.revision),
            });
        } else {
            ops.push(GraphOp::CreateNode {
                key: key.clone(),
                labels: vec![LABEL_RECEIPT.into()],
                properties: receipt_properties,
            });
        }
        let guard_operations = targets.len();
        if ops.len() + guard_operations > MAX_OPS {
            return Err(MemoryError::OperationLimit);
        }
        let batch = self
            .db
            .graph_mut(&self.graph)?
            .write_batch_at(&ops, request.now_ms)?;
        let receipt_write = batch
            .outcomes
            .last()
            .and_then(|outcome| match outcome {
                crate::property_graph::GraphOpOutcome::Node(write) => Some(*write),
                _ => None,
            })
            .ok_or(MemoryError::Storage(Error::InvalidQuery(
                "memory receipt was not written",
            )))?;
        Ok(ForgetResult {
            deleted,
            receipt_revision: receipt_write.revision,
            commit_sequence: receipt_write.revision,
            replayed: false,
        })
    }
}

#[derive(Clone)]
struct ExistingMemory {
    key: String,
    id: String,
    kind: String,
    scope: String,
    digest: String,
    sources: Vec<String>,
    graph_revision: u64,
    memory_revision: u64,
    provenance: Vec<Provenance>,
    importance: f64,
    created_at_ms: i64,
    observed_at_ms: i64,
    reference_count: u64,
    expiry_ms: Option<u64>,
    evidence: Vec<String>,
}

struct Planned {
    ops: Vec<GraphOp>,
    actions: Vec<MemoryAction>,
    budget_operations: usize,
    encoded_request_bytes: usize,
}

fn plan(
    request: &RememberRequest,
    items: &[NormalizedItem],
    scope_key: &str,
    evidence_collection: Option<&str>,
    existing: &mut Vec<ExistingMemory>,
) -> MemoryResult<Planned> {
    let mut ops = Vec::new();
    let mut actions = Vec::with_capacity(items.len());
    let mut requested_ids = BTreeSet::new();
    for item in items {
        if !requested_ids.insert(item.id.as_str()) {
            return Err(MemoryError::DuplicateItem);
        }
    }
    let mut canonical_ids = BTreeMap::new();
    let mut created_ids = BTreeSet::new();
    let mut merged_ids = BTreeSet::new();
    let mut pending_references = Vec::new();
    let mut created_node_indices = BTreeMap::new();
    let mut persisted_upsert_indices = BTreeMap::new();
    let mut planned_merge_operations = 0usize;
    for item in items {
        let digest = content_digest(&item.content);
        let merge_target = if let Some(target_id) = item.merge_into.as_deref() {
            Some(
                existing
                    .iter()
                    .find(|memory| memory.scope == scope_key && memory.id == target_id)
                    .cloned()
                    .ok_or(MemoryError::MissingMemory)?,
            )
        } else {
            None
        };
        let exact = existing
            .iter()
            .find(|memory| {
                memory.scope == scope_key
                    && memory.kind == item.kind.label()
                    && memory.digest == digest
                    && memory.sources.iter().any(|source| source == &item.source)
            })
            .cloned();
        if let Some(memory) = exact {
            if existing
                .iter()
                .any(|candidate| candidate.id == item.id && candidate.key != memory.key)
            {
                return Err(MemoryError::IdentityConflict);
            }
            actions.push(MemoryAction {
                action: "noop",
                id: memory.id.clone(),
                revision: memory.memory_revision,
            });
            if item.id == memory.id {
                canonical_ids.insert(item.id.as_str(), memory.key);
            }
            continue;
        }
        if let Some(memory) = merge_target {
            if memory.kind != item.kind.label() || memory.digest != digest {
                return Err(MemoryError::MergeIdentityConflict);
            }
            let expected = item
                .expected_revision
                .ok_or(MemoryError::ExpectedRevisionRequired)?;
            if expected != memory.memory_revision {
                return Err(MemoryError::RevisionConflict);
            }
            let mut merged = memory.provenance.clone();
            merged.extend(item.provenance.clone());
            if merged.len() > MAX_PROVENANCE
                || encode_provenance(&merged).len() > MAX_STRING_VALUE_BYTES
            {
                return Err(MemoryError::ProvenanceLimit);
            }
            let mut sources = memory.sources.clone();
            if !sources.contains(&item.source) {
                sources.push(item.source.clone());
            }
            if encode_sources(&sources).len() > MAX_STRING_VALUE_BYTES {
                return Err(MemoryError::ProvenanceLimit);
            }
            let next_revision = memory
                .memory_revision
                .checked_add(1)
                .ok_or(MemoryError::RevisionConflict)?;
            let next_reference_count = memory
                .reference_count
                .checked_add(item.references.len() as u64)
                .ok_or(MemoryError::OperationLimit)?;
            let mut evidence = memory.evidence.clone();
            for record in &item.evidence {
                if !evidence.contains(&record.id) {
                    if !evidence.is_empty() {
                        return Err(MemoryError::RecordAlreadyAttached);
                    }
                    evidence.push(record.id.clone());
                }
            }
            let mut properties = memory_properties(MemoryProperties {
                scope: scope_key,
                id: &memory.id,
                kind: &memory.kind,
                content: &item.content,
                digest: &digest,
                sources: &sources,
                provenance: &merged,
                importance: memory.importance,
                created_at_ms: memory.created_at_ms,
                updated_at_ms: request.now_ms as i64,
                observed_at_ms: memory.observed_at_ms,
                memory_revision: next_revision,
                reference_count: next_reference_count,
                evidence: &evidence,
            });
            if item.content.len() > MAX_STRING_VALUE_BYTES {
                properties.remove("content");
            }
            if let Some(&index) = created_node_indices.get(&memory.key) {
                if let GraphOp::CreateNode {
                    properties: created_properties,
                    ..
                } = &mut ops[index]
                {
                    *created_properties = properties;
                }
            } else if let Some(&index) = persisted_upsert_indices.get(&memory.key) {
                ops[index] = GraphOp::UpsertNode {
                    key: memory.key.clone(),
                    labels: vec![LABEL_MEMORY.into()],
                    properties,
                    expected: Expected::Revision(memory.graph_revision),
                };
            } else {
                let index = ops.len();
                ops.push(GraphOp::UpsertNode {
                    key: memory.key.clone(),
                    labels: vec![LABEL_MEMORY.into()],
                    properties,
                    expected: Expected::Revision(memory.graph_revision),
                });
                persisted_upsert_indices.insert(memory.key.clone(), index);
            }
            planned_merge_operations += 2;
            let current_evidence = memory.evidence.iter().collect::<BTreeSet<_>>();
            for evidence in &item.evidence {
                if !current_evidence.contains(&evidence.id) {
                    ops.push(GraphOp::Attach {
                        node: memory.key.clone(),
                        collection: evidence_collection
                            .ok_or(MemoryError::InvalidEvidence)?
                            .to_owned(),
                        record: evidence_record_id(&evidence.id),
                    });
                }
            }
            actions.push(MemoryAction {
                action: "merged",
                id: memory.id.clone(),
                revision: next_revision,
            });
            canonical_ids.insert(item.id.as_str(), memory.key.clone());
            merged_ids.insert(item.id.as_str());
            pending_references.push((
                item.id.clone(),
                memory.key.clone(),
                memory.reference_count,
                item.references.clone(),
            ));
            let virtual_memory = existing
                .iter_mut()
                .find(|candidate| candidate.key == memory.key)
                .ok_or(MemoryError::MissingMemory)?;
            virtual_memory.sources = sources;
            virtual_memory.provenance = merged;
            virtual_memory.memory_revision = next_revision;
            virtual_memory.reference_count = next_reference_count;
            virtual_memory.evidence = evidence;
            continue;
        }

        let key = memory_key(scope_key, &item.id);
        let identity_conflict = existing.iter().any(|memory| memory.key == key);
        if identity_conflict {
            return Err(MemoryError::IdentityConflict);
        }
        if item.merge_into.is_some() {
            return Err(MemoryError::MissingMemory);
        }
        let properties = memory_properties(MemoryProperties {
            scope: scope_key,
            id: &item.id,
            kind: item.kind.label(),
            content: &item.content,
            digest: &digest,
            sources: std::slice::from_ref(&item.source),
            provenance: &item.provenance,
            importance: item.importance,
            created_at_ms: request.now_ms as i64,
            updated_at_ms: request.now_ms as i64,
            observed_at_ms: item.observed_at_ms,
            memory_revision: 1,
            reference_count: item.references.len() as u64,
            evidence: &item
                .evidence
                .iter()
                .map(|value| value.id.clone())
                .collect::<Vec<_>>(),
        });
        let node_index = ops.len();
        if item.content.len() <= MAX_STRING_VALUE_BYTES {
            ops.push(GraphOp::CreateNode {
                key: key.clone(),
                labels: vec![LABEL_MEMORY.into()],
                properties,
            });
        } else {
            let mut props = properties;
            props.remove("content");
            ops.push(GraphOp::CreateNode {
                key: key.clone(),
                labels: vec![LABEL_MEMORY.into()],
                properties: props,
            });
            let chunks = content_chunks(&item.content)?;
            for (index, chunk) in chunks.into_iter().enumerate() {
                let chunk_key = content_key(&key, index);
                let mut chunk_properties = Metadata::new();
                chunk_properties
                    .insert("scope".into(), MetadataValue::String(scope_key.to_owned()));
                chunk_properties.insert(
                    "index".into(),
                    MetadataValue::Integer(
                        i64::try_from(index).map_err(|_| MemoryError::OperationLimit)?,
                    ),
                );
                chunk_properties.insert("content".into(), MetadataValue::String(chunk));
                ops.push(GraphOp::CreateNode {
                    key: chunk_key.clone(),
                    labels: vec![LABEL_CONTENT.into()],
                    properties: chunk_properties,
                });
                ops.push(GraphOp::CreateEdge {
                    key: Some(content_edge_key(&key, index)),
                    source: key.clone(),
                    relationship: "HAS_CONTENT".into(),
                    destination: chunk_key,
                    properties: Metadata::new(),
                });
            }
        }
        created_node_indices.insert(key.clone(), node_index);
        actions.push(MemoryAction {
            action: "created",
            id: item.id.clone(),
            revision: 1,
        });
        created_ids.insert(item.id.as_str());
        canonical_ids.insert(item.id.as_str(), key.clone());
        existing.push(ExistingMemory {
            key: key.clone(),
            id: item.id.clone(),
            kind: item.kind.label().to_owned(),
            scope: scope_key.to_owned(),
            digest,
            sources: vec![item.source.clone()],
            graph_revision: 0,
            memory_revision: 1,
            provenance: item.provenance.clone(),
            importance: item.importance,
            created_at_ms: request.now_ms as i64,
            observed_at_ms: item.observed_at_ms,
            reference_count: item.references.len() as u64,
            expiry_ms: item.expiry_ms,
            evidence: item.evidence.iter().map(|value| value.id.clone()).collect(),
        });
        pending_references.push((item.id.clone(), key.clone(), 0, item.references.clone()));
        if let Some(expiry_ms) = item.expiry_ms {
            ops.push(GraphOp::SetNodeExpiry {
                key: memory_key(scope_key, &item.id),
                expires_at_ms: Some(expiry_ms),
                expected: Expected::Any,
            });
        }
        if item
            .expiry_ms
            .is_some_and(|expiry| expiry <= request.now_ms)
            && (!item.references.is_empty() || !item.evidence.is_empty())
        {
            return Err(MemoryError::MissingMemory);
        }
        for evidence in &item.evidence {
            ops.push(GraphOp::Attach {
                node: key.clone(),
                collection: evidence_collection
                    .ok_or(MemoryError::InvalidEvidence)?
                    .to_owned(),
                record: evidence_record_id(&evidence.id),
            });
        }
    }
    for (item_id, source_key, first_reference, references) in pending_references {
        if !created_ids.contains(item_id.as_str()) && !merged_ids.contains(item_id.as_str()) {
            continue;
        }
        let source_key = canonical_ids
            .get(item_id.as_str())
            .cloned()
            .unwrap_or(source_key);
        for (reference_index, reference) in references.iter().enumerate() {
            let target_key = if let Some(key) = canonical_ids.get(reference.as_str()) {
                key.clone()
            } else {
                existing
                    .iter()
                    .find(|memory| memory.scope == scope_key && memory.id == *reference)
                    .ok_or(MemoryError::MissingReference)?
                    .key
                    .clone()
            };
            if existing.iter().any(|memory| {
                memory.key == target_key
                    && memory
                        .expiry_ms
                        .is_some_and(|expiry| expiry <= request.now_ms)
            }) {
                return Err(MemoryError::MissingReference);
            }
            if !target_key.starts_with(&format!("mem:v1:{scope_key}:")) {
                return Err(MemoryError::CrossScope);
            }
            ops.push(GraphOp::CreateEdge {
                key: Some(reference_edge_key(
                    &source_key,
                    &target_key,
                    first_reference + reference_index as u64,
                )),
                source: source_key.clone(),
                relationship: "REFERENCES".into(),
                destination: target_key,
                properties: Metadata::new(),
            });
        }
    }
    let encoded_request_bytes = encode_request(request, items).len();
    Ok(Planned {
        budget_operations: ops.len() + planned_merge_operations - persisted_upsert_indices.len(),
        ops,
        actions,
        encoded_request_bytes,
    })
}

#[derive(Clone)]
struct NormalizedItem {
    id: String,
    kind: MemoryKind,
    content: String,
    source: String,
    importance: f64,
    observed_at_ms: i64,
    provenance: Vec<Provenance>,
    references: Vec<String>,
    merge_into: Option<String>,
    expected_revision: Option<u64>,
    expiry_ms: Option<u64>,
    evidence: Vec<MemoryEvidence>,
}

fn validate_request(request: &RememberRequest) -> MemoryResult<Vec<NormalizedItem>> {
    request.scope.validate()?;
    if !opaque(&request.idempotency_key) {
        return Err(MemoryError::InvalidKey);
    }
    if request.now_ms > i64::MAX as u64 {
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
    if request.items.is_empty() || request.items.len() > MAX_OPS {
        return Err(if request.items.is_empty() {
            MemoryError::InvalidBundle
        } else {
            MemoryError::OperationLimit
        });
    }
    let mut ids = BTreeSet::new();
    let mut normalized = Vec::with_capacity(request.items.len());
    for item in &request.items {
        if item.kind == MemoryKind::Unsupported {
            return Err(MemoryError::UnsupportedKind);
        }
        if !opaque(&item.id) || !opaque(&item.source) {
            return Err(MemoryError::InvalidIdentity);
        }
        if !ids.insert(item.id.as_str()) {
            return Err(MemoryError::DuplicateItem);
        }
        if item.content.len() > MAX_BODY_BYTES {
            return Err(MemoryError::BodyLimit);
        }
        if !item.expiry_valid {
            return Err(MemoryError::InvalidExpiry);
        }
        let importance = item.importance.unwrap_or(0.5);
        if !importance.is_finite() || !(0.0..=1.0).contains(&importance) {
            return Err(MemoryError::InvalidImportance);
        }
        let observed_at_ms = item.observed_at_ms.ok_or(MemoryError::InvalidObservation)?;
        if item.provenance.len() > MAX_PROVENANCE {
            return Err(MemoryError::ProvenanceLimit);
        }
        let provenance = if item.provenance.is_empty() {
            vec![Provenance {
                source: item.source.clone(),
                kind: None,
                uri: None,
                transformation: None,
            }]
        } else {
            item.provenance.clone()
        };
        if encode_provenance(&provenance).len() > MAX_STRING_VALUE_BYTES {
            return Err(MemoryError::ProvenanceLimit);
        }
        if provenance.iter().any(|entry| {
            !opaque(&entry.source)
                || entry
                    .kind
                    .as_deref()
                    .is_some_and(|value| value.len() > MAX_OPAQUE_BYTES)
                || entry
                    .uri
                    .as_deref()
                    .is_some_and(|value| value.len() > MAX_OPAQUE_BYTES)
                || entry
                    .transformation
                    .as_deref()
                    .is_some_and(|value| value.len() > MAX_OPAQUE_BYTES)
        }) {
            return Err(MemoryError::InvalidProvenance);
        }
        if item.references.iter().any(|reference| !opaque(reference)) {
            return Err(MemoryError::InvalidReference);
        }
        for evidence in &item.evidence {
            if !opaque(&evidence.id) {
                return Err(MemoryError::InvalidEvidence);
            }
            evidence.scope.validate()?;
            if evidence.scope != request.scope {
                return Err(MemoryError::CrossScope);
            }
        }
        if item.merge_into.is_some() != item.expected_revision.is_some() {
            return Err(MemoryError::ExpectedRevisionRequired);
        }
        if item
            .merge_into
            .as_deref()
            .is_some_and(|target| !opaque(target))
        {
            return Err(MemoryError::InvalidIdentity);
        }
        normalized.push(NormalizedItem {
            id: item.id.clone(),
            kind: item.kind,
            content: item.content.clone(),
            source: item.source.clone(),
            importance: if importance == 0.0 { 0.0 } else { importance },
            observed_at_ms,
            provenance,
            references: item.references.clone(),
            merge_into: item.merge_into.clone(),
            expected_revision: item.expected_revision,
            expiry_ms: item.expiry_ms,
            evidence: item.evidence.clone(),
        });
    }
    if encode_request(request, &normalized).len() > MAX_BODY_BYTES {
        return Err(MemoryError::BodyLimit);
    }
    Ok(normalized)
}

fn scan_memories(graph: &GraphRef<'_>, scope: &str) -> MemoryResult<Vec<ExistingMemory>> {
    let conditions = vec![crate::Condition::Eq {
        field: "scope".into(),
        value: MetadataValue::String(scope.into()),
    }];
    let mut cursor = None;
    let mut nodes = Vec::new();
    loop {
        let read = graph.find_nodes(
            &FindNodes {
                label: Some(LABEL_MEMORY.into()),
                conditions: conditions.clone(),
                limit: Some(1_000),
                cursor,
            },
            &ReadOptions {
                limits: GraphLimits::CEILING,
                ..ReadOptions::default()
            },
        )?;
        nodes.extend(read.output);
        match read.cursor {
            Some(next) => cursor = Some(next),
            None if read.complete => break,
            None => return Err(MemoryError::OperationLimit),
        }
    }
    nodes.iter().map(existing_memory).collect()
}

fn existing_memory(node: &NodeView) -> MemoryResult<ExistingMemory> {
    let scope = string_property(node, "scope")?.to_owned();
    let id = string_property(node, "id")?.to_owned();
    let kind = string_property(node, "kind")?.to_owned();
    let digest = string_property(node, "content_digest")?.to_owned();
    let sources = decode_sources(string_property(node, "sources")?)?;
    let provenance = decode_provenance(string_property(node, "provenance")?)?;
    Ok(ExistingMemory {
        key: node.key.clone(),
        id,
        kind,
        scope,
        digest,
        sources,
        graph_revision: node.revision,
        memory_revision: string_property(node, "memory_revision")?
            .parse()
            .map_err(|_| MemoryError::InvalidBundle)?,
        provenance,
        importance: float_property(node, "importance")?,
        created_at_ms: integer_property(node, "created_at_ms")?,
        observed_at_ms: integer_property(node, "observed_at_ms")?,
        reference_count: string_property(node, "reference_count")?
            .parse()
            .map_err(|_| MemoryError::InvalidBundle)?,
        expiry_ms: node.expires_at_ms,
        evidence: match node.properties.get("evidence") {
            None => Vec::new(),
            Some(MetadataValue::String(value)) => decode_evidence(value)?,
            Some(_) => return Err(MemoryError::InvalidBundle),
        },
    })
}

struct MemoryProperties<'a> {
    scope: &'a str,
    id: &'a str,
    kind: &'a str,
    content: &'a str,
    digest: &'a str,
    sources: &'a [String],
    provenance: &'a [Provenance],
    importance: f64,
    created_at_ms: i64,
    updated_at_ms: i64,
    observed_at_ms: i64,
    memory_revision: u64,
    reference_count: u64,
    evidence: &'a [String],
}

fn memory_properties(memory: MemoryProperties<'_>) -> Metadata {
    let mut properties = Metadata::new();
    for (name, value) in [
        ("scope", memory.scope.to_owned()),
        ("id", memory.id.to_owned()),
        ("kind", memory.kind.to_owned()),
        ("content_digest", memory.digest.to_owned()),
        ("sources", encode_sources(memory.sources)),
        ("provenance", encode_provenance(memory.provenance)),
    ] {
        properties.insert(name.into(), MetadataValue::String(value));
    }
    if memory.content.len() <= MAX_STRING_VALUE_BYTES {
        properties.insert(
            "content".into(),
            MetadataValue::String(memory.content.to_owned()),
        );
    }
    properties.insert("importance".into(), MetadataValue::Float(memory.importance));
    properties.insert(
        "observed_at_ms".into(),
        MetadataValue::Integer(memory.observed_at_ms),
    );
    properties.insert(
        "created_at_ms".into(),
        MetadataValue::Integer(memory.created_at_ms),
    );
    properties.insert(
        "updated_at_ms".into(),
        MetadataValue::Integer(memory.updated_at_ms),
    );
    properties.insert(
        "memory_revision".into(),
        MetadataValue::String(memory.memory_revision.to_string()),
    );
    properties.insert(
        "reference_count".into(),
        MetadataValue::String(memory.reference_count.to_string()),
    );
    properties.insert(
        "evidence".into(),
        MetadataValue::String(encode_evidence(memory.evidence)),
    );
    properties
}

fn encode_evidence(evidence: &[String]) -> String {
    let mut ids = evidence.to_vec();
    ids.sort();
    ids.dedup();
    ids.iter()
        .map(|id| hex(id.as_bytes()))
        .collect::<Vec<_>>()
        .join(";")
}

fn decode_evidence(value: &str) -> MemoryResult<Vec<String>> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    value
        .split(';')
        .map(|id| String::from_utf8(unhex(id)?).map_err(|_| MemoryError::InvalidBundle))
        .collect()
}

fn evidence_record_id(id: &str) -> u64 {
    u64::from_be_bytes(Sha256::digest(id.as_bytes())[..8].try_into().unwrap())
}

fn string_property<'a>(node: &'a NodeView, name: &str) -> MemoryResult<&'a str> {
    match node.properties.get(name) {
        Some(MetadataValue::String(value)) => Ok(value),
        _ => Err(MemoryError::Storage(Error::InvalidQuery(
            "memory graph property is missing or has the wrong type",
        ))),
    }
}

fn integer_property(node: &NodeView, name: &str) -> MemoryResult<i64> {
    match node.properties.get(name) {
        Some(MetadataValue::Integer(value)) => Ok(*value),
        _ => Err(MemoryError::Storage(Error::InvalidQuery(
            "memory graph property is missing or has the wrong type",
        ))),
    }
}

fn float_property(node: &NodeView, name: &str) -> MemoryResult<f64> {
    match node.properties.get(name) {
        Some(MetadataValue::Float(value)) => Ok(*value),
        _ => Err(MemoryError::Storage(Error::InvalidQuery(
            "memory graph property is missing or has the wrong type",
        ))),
    }
}

fn verify_scope(node: &NodeView, scope: &str) -> MemoryResult<()> {
    if string_property(node, "scope")? != scope {
        return Err(MemoryError::DigestCollision);
    }
    Ok(())
}

fn memory_key(scope: &str, id: &str) -> String {
    format!("mem:v1:{scope}:{}", hex(&Sha256::digest(id.as_bytes())))
}

fn receipt_key(scope: &str, idempotency_key: &str) -> String {
    operation_receipt_key("remember", scope, idempotency_key)
}

fn forget_receipt_key(scope: &str, idempotency_key: &str) -> String {
    operation_receipt_key("forget", scope, idempotency_key)
}

fn operation_receipt_key(operation: &str, scope: &str, idempotency_key: &str) -> String {
    let mut input = format!("{operation}:v1\0").into_bytes();
    input.extend_from_slice(idempotency_key.as_bytes());
    format!("receipt:v1:{scope}:{}", hex(&Sha256::digest(input)))
}

fn receipt_expired(receipt: &NodeView, now_ms: u64) -> bool {
    receipt
        .properties
        .get("expires_at_ms")
        .and_then(|value| match value {
            MetadataValue::String(value) => value.parse::<u64>().ok(),
            _ => None,
        })
        .is_some_and(|expiry| now_ms >= expiry)
}

fn validate_forget_request(request: &ForgetRequest) -> MemoryResult<()> {
    request.scope.validate()?;
    if !opaque(&request.idempotency_key) {
        return Err(MemoryError::InvalidKey);
    }
    if request.now_ms > i64::MAX as u64 {
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
    if request.targets.is_empty() || request.targets.len() > MAX_OPS {
        return Err(if request.targets.is_empty() {
            MemoryError::InvalidTarget
        } else {
            MemoryError::OperationLimit
        });
    }
    let mut ids = BTreeSet::new();
    for target in &request.targets {
        if !opaque(&target.id) || target.expected_revision == 0 {
            return Err(MemoryError::InvalidTarget);
        }
        if !ids.insert(target.id.as_str()) {
            return Err(MemoryError::DuplicateTarget);
        }
    }
    Ok(())
}

fn forget_request_digest(request: &ForgetRequest) -> String {
    let mut output = String::new();
    output.push_str("{\"key\":");
    output.push_str(&json_string(&request.idempotency_key));
    output.push_str(",\"receipt_window\":");
    output.push_str(&request.receipt_window_ms.to_string());
    output.push_str(",\"scope\":{\"agent\":");
    output.push_str(&json_string(&request.scope.agent));
    output.push_str(",\"session\":");
    output.push_str(&option_json(request.scope.session.as_deref()));
    output.push_str(",\"tenant\":");
    output.push_str(&json_string(&request.scope.tenant));
    output.push_str(",\"user\":");
    output.push_str(&option_json(request.scope.user.as_deref()));
    output.push_str("},\"targets\":[");
    for (index, target) in request.targets.iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        output.push_str("{\"id\":");
        output.push_str(&json_string(&target.id));
        output.push_str(",\"revision\":");
        output.push_str(&target.expected_revision.to_string());
        output.push('}');
    }
    output.push_str("]}");
    format!("v1:{}", hex(&Sha256::digest(output.as_bytes())))
}

fn encode_forget_result(deleted: &[String]) -> String {
    let mut ids = deleted.to_vec();
    ids.sort();
    format!(
        "deleted:{}",
        ids.iter()
            .map(|id| hex(id.as_bytes()))
            .collect::<Vec<_>>()
            .join(";")
    )
}

fn decode_forget_result(
    encoded: &str,
    receipt_revision: u64,
    replayed: bool,
    targets: &[ForgetTarget],
) -> MemoryResult<ForgetResult> {
    let values = encoded
        .strip_prefix("deleted:")
        .ok_or(MemoryError::InvalidBundle)?;
    let mut deleted = if values.is_empty() {
        Vec::new()
    } else {
        values
            .split(';')
            .map(|value| String::from_utf8(unhex(value)?).map_err(|_| MemoryError::InvalidBundle))
            .collect::<MemoryResult<Vec<_>>>()?
    };
    deleted.sort();
    let mut expected = targets
        .iter()
        .map(|target| target.id.clone())
        .collect::<Vec<_>>();
    expected.sort();
    if deleted != expected {
        return Err(MemoryError::InvalidBundle);
    }
    Ok(ForgetResult {
        deleted,
        receipt_revision,
        commit_sequence: receipt_revision,
        replayed,
    })
}

fn retained_evidence_ids(
    graph: &GraphRef<'_>,
    memories: &[ExistingMemory],
    deleted_keys: &BTreeSet<String>,
    scope: &str,
) -> MemoryResult<BTreeSet<String>> {
    let by_key = memories
        .iter()
        .map(|memory| (memory.key.as_str(), memory))
        .collect::<BTreeMap<_, _>>();
    let mut reachable = memories
        .iter()
        .filter(|memory| !deleted_keys.contains(&memory.key))
        .map(|memory| memory.key.clone())
        .collect::<BTreeSet<_>>();
    let mut pending = reachable.iter().cloned().collect::<Vec<_>>();
    while let Some(key) = pending.pop() {
        let mut cursor = None;
        loop {
            let neighbors = graph.neighbors(
                &Neighbors {
                    node: NodeSelector::key(&key),
                    direction: Direction::Out,
                    types: Some(vec!["REFERENCES".into()]),
                    edge_conditions: Vec::new(),
                    limit: Some(GraphLimits::CEILING.max_output_nodes),
                    cursor,
                },
                &ReadOptions {
                    limits: GraphLimits::CEILING,
                    ..ReadOptions::default()
                },
            )?;
            for node in neighbors.output.nodes {
                if by_key.get(node.key.as_str()).is_some_and(|memory| {
                    memory.scope == scope && reachable.insert(node.key.clone())
                }) {
                    pending.push(node.key);
                }
            }
            match neighbors.cursor {
                Some(next) => cursor = Some(next),
                None if neighbors.complete => break,
                None => return Err(MemoryError::OperationLimit),
            }
        }
    }
    Ok(reachable
        .iter()
        .filter_map(|key| by_key.get(key.as_str()))
        .flat_map(|memory| memory.evidence.iter().cloned())
        .collect())
}

fn receipt_outcome_key(receipt_key: &str, index: usize) -> String {
    format!(
        "receipt-outcome:v1:{}:{index:04x}",
        hex(&Sha256::digest(receipt_key))
    )
}

fn content_key(memory_key: &str, index: usize) -> String {
    format!(
        "content:v1:{}:{index:04x}",
        hex(&Sha256::digest(memory_key))
    )
}

fn content_edge_key(memory_key: &str, index: usize) -> String {
    format!(
        "content-edge:v1:{}:{index:04x}",
        hex(&Sha256::digest(memory_key))
    )
}

fn reference_edge_key(source: &str, destination: &str, index: u64) -> String {
    let mut key = source.as_bytes().to_vec();
    key.push(0);
    key.extend_from_slice(destination.as_bytes());
    key.extend_from_slice(&index.to_be_bytes());
    format!("reference:v1:{}", hex(&Sha256::digest(key)))
}

fn content_digest(content: &str) -> String {
    format!(
        "v1:{}",
        hex(&Sha256::digest(json_string(content).as_bytes()))
    )
}

fn request_digest(request: &RememberRequest, items: &[NormalizedItem]) -> String {
    format!(
        "v1:{}",
        hex(&Sha256::digest(encode_request(request, items)))
    )
}

fn encode_request(request: &RememberRequest, items: &[NormalizedItem]) -> Vec<u8> {
    let scope = &request.scope;
    let mut output = String::new();
    output.push_str("{\"allow_future\":");
    output.push_str(if request.allow_future_observations {
        "true"
    } else {
        "false"
    });
    output.push_str(",\"items\":[");
    for (index, item) in items.iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        output.push_str("{\"content\":");
        output.push_str(&json_string(&item.content));
        output.push_str(",\"evidence\":[");
        for (evidence_index, evidence) in item.evidence.iter().enumerate() {
            if evidence_index != 0 {
                output.push(',');
            }
            output.push_str("{\"id\":");
            output.push_str(&json_string(&evidence.id));
            output.push_str(",\"scope\":{\"agent\":");
            output.push_str(&json_string(&evidence.scope.agent));
            output.push_str(",\"session\":");
            output.push_str(&option_json(evidence.scope.session.as_deref()));
            output.push_str(",\"tenant\":");
            output.push_str(&json_string(&evidence.scope.tenant));
            output.push_str(",\"user\":");
            output.push_str(&option_json(evidence.scope.user.as_deref()));
            output.push_str("}}");
        }
        output.push_str("],\"expiry\":");
        output.push_str(
            &item
                .expiry_ms
                .map_or_else(|| "null".into(), |expiry| expiry.to_string()),
        );
        output.push_str(",\"id\":");
        output.push_str(&json_string(&item.id));
        output.push_str(",\"importance\":");
        output.push_str(&float_json(item.importance));
        output.push_str(",\"kind\":");
        output.push_str(&json_string(item.kind.label()));
        output.push_str(",\"observed_at\":");
        output.push_str(&item.observed_at_ms.to_string());
        output.push_str(",\"provenance\":[");
        for (p_index, entry) in item.provenance.iter().enumerate() {
            if p_index != 0 {
                output.push(',');
            }
            output.push_str("{\"source\":");
            output.push_str(&json_string(&entry.source));
            for (key, value) in [
                ("kind", &entry.kind),
                ("transformation", &entry.transformation),
                ("uri", &entry.uri),
            ] {
                if let Some(value) = value {
                    output.push(',');
                    output.push_str(&json_string(key));
                    output.push(':');
                    output.push_str(&json_string(value));
                }
            }
            output.push('}');
        }
        output.push_str("],\"references\":[");
        for (r_index, reference) in item.references.iter().enumerate() {
            if r_index != 0 {
                output.push(',');
            }
            output.push_str(&json_string(reference));
        }
        output.push_str("],\"source\":");
        output.push_str(&json_string(&item.source));
        if let Some(target) = &item.merge_into {
            output.push_str(",\"merge_into\":");
            output.push_str(&json_string(target));
            output.push_str(",\"expected_revision\":");
            output.push_str(&item.expected_revision.unwrap_or_default().to_string());
        }
        output.push('}');
    }
    output.push_str("],\"key\":");
    output.push_str(&json_string(&request.idempotency_key));
    output.push_str(",\"receipt_window\":");
    output.push_str(&request.receipt_window_ms.to_string());
    output.push_str(",\"scope\":{\"agent\":");
    output.push_str(&json_string(&scope.agent));
    output.push_str(",\"session\":");
    output.push_str(&option_json(scope.session.as_deref()));
    output.push_str(",\"tenant\":");
    output.push_str(&json_string(&scope.tenant));
    output.push_str(",\"user\":");
    output.push_str(&option_json(scope.user.as_deref()));
    output.push_str("}}");
    output.into_bytes()
}

fn encode_actions(actions: &[MemoryAction]) -> String {
    let mut packed = vec![0_u8; actions.len().div_ceil(4)];
    let mut noops = String::new();
    for (index, action) in actions.iter().enumerate() {
        let code = match action.action {
            "created" => 0_u8,
            "merged" => 1,
            "noop" => {
                noops.push_str(&format!(
                    "{index}:{}:{};",
                    hex(action.id.as_bytes()),
                    action.revision
                ));
                2
            }
            _ => unreachable!("only validated memory actions are encoded"),
        };
        packed[index / 4] |= code << ((index % 4) * 2);
    }
    format!("{}:{}:{}", actions.len(), hex(&packed), noops)
}

fn encode_outcome(
    receipt_key: &str,
    outcome: &str,
    properties: &mut Metadata,
) -> MemoryResult<Vec<GraphOp>> {
    if outcome.len() > MAX_BODY_BYTES {
        return Err(MemoryError::BodyLimit);
    }
    let chunks = outcome
        .as_bytes()
        .chunks(MAX_STRING_VALUE_BYTES)
        .collect::<Vec<_>>();
    properties.insert(
        "outcome_chunks".into(),
        MetadataValue::Integer(i64::try_from(chunks.len()).map_err(|_| MemoryError::BodyLimit)?),
    );
    let mut ops = Vec::with_capacity(chunks.len());
    for (index, chunk) in chunks.iter().enumerate() {
        let content = std::str::from_utf8(chunk)
            .map_err(|_| MemoryError::InvalidBundle)?
            .to_owned();
        let mut chunk_properties = Metadata::new();
        chunk_properties.insert(
            "receipt_key".into(),
            MetadataValue::String(receipt_key.to_owned()),
        );
        chunk_properties.insert(
            "index".into(),
            MetadataValue::Integer(i64::try_from(index).map_err(|_| MemoryError::BodyLimit)?),
        );
        chunk_properties.insert("outcome".into(), MetadataValue::String(content));
        ops.push(GraphOp::CreateNode {
            key: receipt_outcome_key(receipt_key, index),
            labels: vec![LABEL_RECEIPT_OUTCOME.into()],
            properties: chunk_properties,
        });
    }
    Ok(ops)
}

fn decode_outcome(graph: &GraphRef<'_>, node: &NodeView) -> MemoryResult<String> {
    let count = usize::try_from(integer_property(node, "outcome_chunks")?)
        .map_err(|_| MemoryError::InvalidBundle)?;
    if count == 0 || count > MAX_BODY_BYTES.div_ceil(MAX_STRING_VALUE_BYTES) {
        return Err(MemoryError::InvalidBundle);
    }
    let mut outcome = String::new();
    for index in 0..count {
        let chunk_key = receipt_outcome_key(&node.key, index);
        let chunk = graph
            .get_node(&chunk_key)?
            .output
            .ok_or(MemoryError::InvalidBundle)?;
        if string_property(&chunk, "receipt_key")? != node.key
            || integer_property(&chunk, "index")? != index as i64
        {
            return Err(MemoryError::InvalidBundle);
        }
        outcome.push_str(string_property(&chunk, "outcome")?);
    }
    Ok(outcome)
}

fn decode_result(
    encoded: &str,
    receipt_revision: u64,
    replayed: bool,
    normalized: &[NormalizedItem],
) -> MemoryResult<RememberResult> {
    let mut encoded_parts = encoded.splitn(3, ':');
    let count = encoded_parts
        .next()
        .ok_or(MemoryError::InvalidBundle)?
        .parse::<usize>()
        .map_err(|_| MemoryError::InvalidBundle)?;
    let packed = unhex(encoded_parts.next().ok_or(MemoryError::InvalidBundle)?)?;
    let noops = encoded_parts.next().ok_or(MemoryError::InvalidBundle)?;
    if count != normalized.len() || packed.len() != count.div_ceil(4) {
        return Err(MemoryError::InvalidBundle);
    }
    let noop_map = noops
        .split_terminator(';')
        .map(|entry| {
            let mut fields = entry.split(':');
            let index = fields
                .next()
                .ok_or(MemoryError::InvalidBundle)?
                .parse::<usize>()
                .map_err(|_| MemoryError::InvalidBundle)?;
            let id = String::from_utf8(unhex(fields.next().ok_or(MemoryError::InvalidBundle)?)?)
                .map_err(|_| MemoryError::InvalidBundle)?;
            let revision = fields
                .next()
                .ok_or(MemoryError::InvalidBundle)?
                .parse::<u64>()
                .map_err(|_| MemoryError::InvalidBundle)?;
            if fields.next().is_some() {
                return Err(MemoryError::InvalidBundle);
            }
            Ok((index, (id, revision)))
        })
        .collect::<MemoryResult<BTreeMap<_, _>>>()?;
    let mut actions = Vec::new();
    for index in 0..count {
        let code = (packed[index / 4] >> ((index % 4) * 2)) & 3;
        let (action, id, revision) = match code {
            0 => ("created", normalized[index].id.clone(), 1),
            1 => (
                "merged",
                normalized[index]
                    .merge_into
                    .clone()
                    .ok_or(MemoryError::InvalidBundle)?,
                normalized[index]
                    .expected_revision
                    .ok_or(MemoryError::InvalidBundle)?
                    .checked_add(1)
                    .ok_or(MemoryError::RevisionConflict)?,
            ),
            2 => {
                let (id, revision) = noop_map
                    .get(&index)
                    .cloned()
                    .ok_or(MemoryError::InvalidBundle)?;
                ("noop", id, revision)
            }
            _ => return Err(MemoryError::InvalidBundle),
        };
        actions.push(MemoryAction {
            action,
            id,
            revision,
        });
    }
    Ok(RememberResult {
        actions,
        receipt_revision,
        commit_sequence: receipt_revision,
        replayed,
    })
}

fn encode_provenance(entries: &[Provenance]) -> String {
    let mut out = String::new();
    for entry in entries {
        append_provenance_field(&mut out, Some(&entry.source));
        append_provenance_field(&mut out, entry.kind.as_deref());
        append_provenance_field(&mut out, entry.uri.as_deref());
        append_provenance_field(&mut out, entry.transformation.as_deref());
    }
    out
}

fn decode_provenance(value: &str) -> MemoryResult<Vec<Provenance>> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    let mut bytes = value.as_bytes();
    let mut entries = Vec::new();
    while !bytes.is_empty() {
        let source = take_provenance_field(&mut bytes)?.ok_or(MemoryError::InvalidProvenance)?;
        let kind = take_provenance_field(&mut bytes)?;
        let uri = take_provenance_field(&mut bytes)?;
        let transformation = take_provenance_field(&mut bytes)?;
        entries.push(Provenance {
            source,
            kind,
            uri,
            transformation,
        });
    }
    Ok(entries)
}

fn append_provenance_field(output: &mut String, value: Option<&str>) {
    if let Some(value) = value {
        use std::fmt::Write as _;
        write!(output, "{}:", value.len()).expect("writing to String cannot fail");
        output.push_str(value);
        output.push(';');
    } else {
        output.push_str("-;");
    }
}

fn take_provenance_field(bytes: &mut &[u8]) -> MemoryResult<Option<String>> {
    if let Some(rest) = bytes.strip_prefix(b"-;") {
        *bytes = rest;
        return Ok(None);
    }
    let colon = bytes
        .iter()
        .position(|byte| *byte == b':')
        .ok_or(MemoryError::InvalidProvenance)?;
    let length = std::str::from_utf8(&bytes[..colon])
        .map_err(|_| MemoryError::InvalidProvenance)?
        .parse::<usize>()
        .map_err(|_| MemoryError::InvalidProvenance)?;
    let value_start = colon + 1;
    let value_end = value_start
        .checked_add(length)
        .filter(|end| *end < bytes.len() && bytes[*end] == b';')
        .ok_or(MemoryError::InvalidProvenance)?;
    let value = std::str::from_utf8(&bytes[value_start..value_end])
        .map_err(|_| MemoryError::InvalidProvenance)?
        .to_owned();
    *bytes = &bytes[value_end + 1..];
    Ok(Some(value))
}

fn encode_sources(sources: &[String]) -> String {
    let mut sorted = sources.to_vec();
    sorted.sort();
    sorted.dedup();
    sorted
        .iter()
        .map(|source| hex(source.as_bytes()))
        .collect::<Vec<_>>()
        .join(";")
}

fn decode_sources(value: &str) -> MemoryResult<Vec<String>> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    value
        .split(';')
        .map(|source| String::from_utf8(unhex(source)?).map_err(|_| MemoryError::InvalidProvenance))
        .collect()
}

fn content_chunks(content: &str) -> MemoryResult<Vec<String>> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for character in content.chars() {
        let encoded = character.len_utf8();
        if !current.is_empty() && current.len() + encoded > MAX_STRING_VALUE_BYTES {
            chunks.push(std::mem::take(&mut current));
        }
        current.push(character);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks
        .iter()
        .any(|chunk| chunk.len() > MAX_STRING_VALUE_BYTES)
    {
        return Err(MemoryError::BodyLimit);
    }
    Ok(chunks)
}

fn opaque(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_OPAQUE_BYTES
}

fn json_string(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 2);
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            c if c <= '\u{1f}' => {
                use std::fmt::Write as _;
                write!(output, "\\u{:04x}", c as u32).expect("writing to String cannot fail");
            }
            c => output.push(c),
        }
    }
    output.push('"');
    output
}

fn option_json(value: Option<&str>) -> String {
    value.map_or_else(|| "null".into(), json_string)
}

fn float_json(value: f64) -> String {
    if value == 0.0 {
        "0.0".into()
    } else {
        value.to_string()
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

fn unhex(value: &str) -> MemoryResult<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return Err(MemoryError::InvalidBundle);
    }
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let high = (pair[0] as char)
                .to_digit(16)
                .ok_or(MemoryError::InvalidBundle)?;
            let low = (pair[1] as char)
                .to_digit(16)
                .ok_or(MemoryError::InvalidBundle)?;
            Ok(((high << 4) | low) as u8)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use crate::property_graph::{FindNodes, GraphLimits, Neighbors, NodeSelector, ReadOptions};
    use serde_json::{Value, json};
    use std::collections::BTreeSet;
    use std::{fs, path::Path};

    fn database(path: &Path) -> Database {
        let mut db = Database::create(path, 2).unwrap();
        db.create_graph("memory").unwrap();
        db
    }

    fn provision(db: &mut Database, scope: &MemoryScope) {
        MemoryMut::bind(db, "memory", None)
            .unwrap()
            .provision_scope(scope)
            .unwrap();
    }

    fn request(id: &str, key: &str) -> RememberRequest {
        RememberRequest {
            scope: MemoryScope {
                tenant: "tenant".into(),
                agent: "agent".into(),
                user: Some("user".into()),
                session: None,
            },
            idempotency_key: key.into(),
            receipt_window_ms: 100,
            now_ms: 10,
            deadline_ms: None,
            allow_future_observations: false,
            items: vec![MemoryItem {
                id: id.into(),
                kind: MemoryKind::Episodic,
                content: "hello".into(),
                source: "source".into(),
                importance: None,
                observed_at_ms: Some(10),
                provenance: Vec::new(),
                references: Vec::new(),
                merge_into: None,
                expected_revision: None,
                expiry_ms: None,
                expiry_valid: true,
                evidence: Vec::new(),
            }],
        }
    }

    fn timed_request(id: &str, key: &str, now_ms: u64) -> RememberRequest {
        let mut request = request(id, key);
        request.now_ms = now_ms;
        request.items[0].observed_at_ms = Some(now_ms as i64);
        request
    }

    fn distinct_request(id: &str, key: &str) -> RememberRequest {
        let mut request = request(id, key);
        request.items[0].content = format!("content-{id}");
        request
    }

    fn remember(db: &mut Database, request: &RememberRequest) -> RememberResult {
        MemoryMut::bind(db, "memory", None)
            .unwrap()
            .remember(request)
            .unwrap()
    }

    fn seed_vector(db: &mut Database, id: &str) -> u64 {
        let record = evidence_record_id(id);
        db.graph_mut("memory")
            .unwrap()
            .write_batch(&[GraphOp::Vector {
                collection: "default".into(),
                mutation: crate::Mutation::Put {
                    id: record,
                    vector: vec![1.0, 0.0],
                    payload: id.as_bytes().to_vec(),
                },
            }])
            .unwrap();
        record
    }

    fn forget_request(
        scope: MemoryScope,
        key: &str,
        now_ms: u64,
        targets: &[(&str, u64)],
    ) -> ForgetRequest {
        ForgetRequest {
            scope,
            idempotency_key: key.into(),
            receipt_window_ms: 100,
            now_ms,
            deadline_ms: None,
            targets: targets
                .iter()
                .map(|(id, expected_revision)| ForgetTarget {
                    id: (*id).into(),
                    expected_revision: *expected_revision,
                })
                .collect(),
        }
    }

    fn assert_memory_unchanged(db: &Database, key: &str, sequence: u64, revision: Option<u64>) {
        assert_eq!(db.sequence, sequence);
        assert_eq!(
            db.graph("memory")
                .unwrap()
                .get_node(key)
                .unwrap()
                .output
                .map(|node| node.revision),
            revision
        );
    }

    fn memory_evidence(id: &str, scope: &MemoryScope) -> MemoryEvidence {
        MemoryEvidence {
            id: id.into(),
            scope: scope.clone(),
        }
    }

    fn node(db: &Database, key: &str) -> NodeView {
        db.graph("memory")
            .unwrap()
            .get_node(key)
            .unwrap()
            .output
            .unwrap()
    }

    fn replace_properties(db: &mut Database, node: NodeView, properties: Metadata) {
        db.graph_mut("memory")
            .unwrap()
            .write_batch(&[GraphOp::UpsertNode {
                key: node.key,
                labels: node.labels.into_iter().collect(),
                properties,
                expected: Expected::Revision(node.revision),
            }])
            .unwrap();
    }

    fn seed_long_noops(db: &mut Database, count: usize) -> RememberRequest {
        let seed = request(&"c".repeat(256), "canonical");
        provision(db, &seed.scope);
        remember(db, &seed);
        let key = memory_key(&seed.scope.key(), &seed.items[0].id);
        let memory = node(db, &key);
        let mut properties = memory.properties.clone();
        properties.insert(
            "memory_revision".into(),
            MetadataValue::String(u64::MAX.to_string()),
        );
        replace_properties(db, memory, properties);
        RememberRequest {
            idempotency_key: "long-noops".into(),
            items: (0..count)
                .map(|index| MemoryItem {
                    id: format!("alias-{index}"),
                    ..seed.items[0].clone()
                })
                .collect(),
            ..seed
        }
    }

    fn tampered_receipt_error(tamper: &str) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tampered.vlite");
        let mut db = database(&path);
        let req = seed_long_noops(&mut db, 20);
        remember(&mut db, &req);
        let receipt = node(&db, &receipt_key(&req.scope.key(), &req.idempotency_key));
        let count = integer_property(&receipt, "outcome_chunks").unwrap();
        assert!(count > 1);
        let first = node(&db, &receipt_outcome_key(&receipt.key, 0));
        match tamper {
            "missing" => {
                db.graph_mut("memory")
                    .unwrap()
                    .write_batch(&[GraphOp::DeleteNode {
                        key: first.key,
                        expected: Expected::Revision(first.revision),
                    }])
                    .unwrap();
            }
            "index" | "receipt_key" | "action_count" | "action_ids" => {
                let mut properties = first.properties.clone();
                match tamper {
                    "index" => {
                        properties.insert("index".into(), MetadataValue::Integer(1));
                    }
                    "receipt_key" => {
                        let other = request("another-memory", "another-receipt");
                        remember(&mut db, &other);
                        properties.insert(
                            "receipt_key".into(),
                            MetadataValue::String(receipt_key(
                                &other.scope.key(),
                                &other.idempotency_key,
                            )),
                        );
                    }
                    "action_count" => {
                        let outcome = string_property(&first, "outcome").unwrap();
                        properties.insert(
                            "outcome".into(),
                            MetadataValue::String(outcome.replacen("20:", "19:", 1)),
                        );
                    }
                    "action_ids" => {
                        let outcome = string_property(&first, "outcome").unwrap();
                        // A non-UTF-8 ID cannot be decoded into an action.
                        properties.insert(
                            "outcome".into(),
                            MetadataValue::String(outcome.replacen("0:6363", "0:ff63", 1)),
                        );
                    }
                    _ => unreachable!(),
                }
                replace_properties(&mut db, first, properties);
            }
            "zero" | "extra" | "excessive" => {
                let mut properties = receipt.properties.clone();
                let count = match tamper {
                    "zero" => 0,
                    "extra" => count + 1,
                    "excessive" => 9_999,
                    _ => unreachable!(),
                };
                properties.insert("outcome_chunks".into(), MetadataValue::Integer(count));
                replace_properties(&mut db, receipt.clone(), properties);
            }
            _ => panic!("unknown tampering"),
        }
        let sequence = db.sequence;
        drop(db);
        let before = fs::read(&path).unwrap();
        let mut db = Database::open(&path).unwrap();
        if matches!(tamper, "zero" | "excessive") {
            // Exercise the envelope validator directly as well: downstream parsing
            // otherwise masks a missing zero-count check.
            let receipt = node(&db, &receipt.key);
            assert!(
                matches!(
                    decode_outcome(&db.graph("memory").unwrap(), &receipt),
                    Err(MemoryError::InvalidBundle)
                ),
                "{tamper}"
            );
        }
        assert!(
            matches!(
                MemoryMut::bind(&mut db, "memory", None)
                    .unwrap()
                    .remember(&req),
                Err(MemoryError::InvalidBundle)
            ),
            "{tamper}"
        );
        assert_eq!(db.sequence, sequence);
        drop(db);
        assert_eq!(fs::read(path).unwrap(), before, "{tamper}");
    }

    #[test]
    fn forget_entity_and_episodic_shared_and_unshared_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("forget-evidence.vlite");
        let mut db = database(&path);
        let now = crate::current_time_ms() + 30_000;
        let shared_record = seed_vector(&mut db, "shared-evidence");
        let unshared_record = seed_vector(&mut db, "unshared-evidence");
        let scope = MemoryScope {
            tenant: "tenant".into(),
            agent: "agent".into(),
            user: Some("user".into()),
            session: None,
        };
        provision(&mut db, &scope);

        let mut entity = timed_request("entity", "entity-write", now);
        entity.items[0].kind = MemoryKind::Entity;
        entity.items[0].evidence.push(MemoryEvidence {
            id: "shared-evidence".into(),
            scope: scope.clone(),
        });
        MemoryMut::bind(&mut db, "memory", Some("default"))
            .unwrap()
            .remember(&entity)
            .unwrap();

        let mut referencer = timed_request("referencer", "referencer-write", now);
        referencer.items[0].kind = MemoryKind::Entity;
        referencer.items[0].content = "different content".into();
        referencer.items[0].references.push("entity".into());
        MemoryMut::bind(&mut db, "memory", Some("default"))
            .unwrap()
            .remember(&referencer)
            .unwrap();

        let mut episode = timed_request("episode", "episode-write", now);
        episode.items[0].evidence.push(MemoryEvidence {
            id: "unshared-evidence".into(),
            scope: scope.clone(),
        });
        MemoryMut::bind(&mut db, "memory", Some("default"))
            .unwrap()
            .remember(&episode)
            .unwrap();

        let entity_key = memory_key(&scope.key(), "entity");
        let episode_key = memory_key(&scope.key(), "episode");
        let deleted_entity = MemoryMut::bind(&mut db, "memory", Some("default"))
            .unwrap()
            .forget(&forget_request(
                scope.clone(),
                "forget-entity",
                now + 1,
                &[("entity", 1)],
            ))
            .unwrap();
        assert_eq!(deleted_entity.deleted, ["entity"]);
        assert!(
            db.graph("memory")
                .unwrap()
                .get_node(&entity_key)
                .unwrap()
                .output
                .is_none()
        );
        assert!(db.get(shared_record).unwrap().is_some());
        assert!(db.get(unshared_record).unwrap().is_some());

        let deleted_episode = MemoryMut::bind(&mut db, "memory", Some("default"))
            .unwrap()
            .forget(&forget_request(
                scope,
                "forget-episode",
                now + 2,
                &[("episode", 1)],
            ))
            .unwrap();
        assert_eq!(deleted_episode.deleted, ["episode"]);
        assert!(
            db.graph("memory")
                .unwrap()
                .get_node(&episode_key)
                .unwrap()
                .output
                .is_none()
        );
        assert!(db.get(unshared_record).unwrap().is_none());
        assert!(db.get(shared_record).unwrap().is_some());
    }

    #[test]
    fn forget_rejects_foreign_attachment_without_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("foreign-attachment.vlite");
        let mut db = database(&path);
        let now = crate::current_time_ms() + 30_000;
        let record = seed_vector(&mut db, "foreign-evidence");
        let mut request = timed_request("memory", "write", now);
        request.items[0].evidence.push(MemoryEvidence {
            id: "foreign-evidence".into(),
            scope: request.scope.clone(),
        });
        provision(&mut db, &request.scope);
        MemoryMut::bind(&mut db, "memory", Some("default"))
            .unwrap()
            .remember(&request)
            .unwrap();

        let memory_key = memory_key(&request.scope.key(), "memory");
        db.graph_mut("memory")
            .unwrap()
            .write_batch_at(
                &[
                    GraphOp::Detach {
                        node: memory_key.clone(),
                        collection: "default".into(),
                    },
                    GraphOp::CreateNode {
                        key: "outside-owner".into(),
                        labels: vec!["Outside".into()],
                        properties: Metadata::new(),
                    },
                    GraphOp::Attach {
                        node: "outside-owner".into(),
                        collection: "default".into(),
                        record,
                    },
                ],
                now + 1,
            )
            .unwrap();

        let before_sequence = db.sequence;
        let before_end = db.end;
        let before_memory = node(&db, &memory_key);
        let result = MemoryMut::bind(&mut db, "memory", Some("default"))
            .unwrap()
            .forget(&forget_request(
                request.scope.clone(),
                "erase",
                now + 2,
                &[("memory", 1)],
            ));
        assert!(matches!(result, Err(MemoryError::ForeignAttachment)));
        assert_eq!(db.sequence, before_sequence);
        assert_eq!(db.end, before_end);
        assert_eq!(node(&db, &memory_key).revision, before_memory.revision);
        assert!(db.get(record).unwrap().is_some());
        assert!(
            db.graph("memory")
                .unwrap()
                .get_node(&forget_receipt_key(&request.scope.key(), "erase"))
                .unwrap()
                .output
                .is_none()
        );
    }

    #[test]
    fn expiry_at_clock_boundary_hides_memory_and_evidence_but_retains_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("expiry-boundary.vlite");
        let mut db = database(&path);
        let now = crate::current_time_ms() + 30_000;
        let expires_at = now + 5;
        let record = seed_vector(&mut db, "expiry-evidence");
        let mut request = timed_request("expiring", "expiry-write", now);
        request.items[0].kind = MemoryKind::Entity;
        request.items[0].expiry_ms = Some(expires_at);
        request.items[0].evidence.push(MemoryEvidence {
            id: "expiry-evidence".into(),
            scope: request.scope.clone(),
        });
        provision(&mut db, &request.scope);
        MemoryMut::bind(&mut db, "memory", Some("default"))
            .unwrap()
            .remember(&request)
            .unwrap();
        let key = memory_key(&request.scope.key(), "expiring");
        assert!(
            db.graph("memory")
                .unwrap()
                .with_clock(expires_at - 1)
                .get_node(&key)
                .unwrap()
                .output
                .is_some()
        );
        assert_eq!(
            db.graph("memory")
                .unwrap()
                .with_clock(expires_at - 1)
                .memory_node_attachments(&key)
                .unwrap()
                .len(),
            1
        );
        assert!(
            db.graph("memory")
                .unwrap()
                .with_clock(expires_at)
                .get_node(&key)
                .unwrap()
                .output
                .is_none()
        );
        assert!(
            db.graph("memory")
                .unwrap()
                .with_clock(expires_at)
                .memory_node_attachments(&key)
                .unwrap()
                .is_empty()
        );
        assert!(db.get(record).unwrap().is_some());

        db.graph_mut("memory")
            .unwrap()
            .write_batch_at(
                &[GraphOp::CreateNode {
                    key: "expiry-reclaim-trigger".into(),
                    labels: Vec::new(),
                    properties: Metadata::new(),
                }],
                expires_at + 1,
            )
            .unwrap();
        assert!(
            db.graph("memory")
                .unwrap()
                .get_node(&key)
                .unwrap()
                .output
                .is_none()
        );
        assert!(db.get(record).unwrap().is_some());
        assert!(
            db.graph("memory")
                .unwrap()
                .with_clock(expires_at + 1)
                .memory_node_attachments(&key)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn forget_removes_incident_edges_and_content_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("forget-content.vlite");
        let mut db = database(&path);
        let now = crate::current_time_ms() + 30_000;
        let mut target = timed_request("target", "target-write", now);
        target.items[0].kind = MemoryKind::Entity;
        target.items[0].content = "x".repeat(MAX_STRING_VALUE_BYTES + 1);
        provision(&mut db, &target.scope);
        MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .remember(&target)
            .unwrap();
        let mut retained = timed_request("retained", "retained-write", now);
        retained.items[0].kind = MemoryKind::Entity;
        MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .remember(&retained)
            .unwrap();
        let target_key = memory_key(&target.scope.key(), "target");
        let retained_key = memory_key(&target.scope.key(), "retained");
        db.graph_mut("memory")
            .unwrap()
            .write_batch_at(
                &[GraphOp::CreateEdge {
                    key: Some("fixture-incident-edge".into()),
                    source: target_key.clone(),
                    relationship: "FIXTURE_LINK".into(),
                    destination: retained_key.clone(),
                    properties: Metadata::new(),
                }],
                now + 1,
            )
            .unwrap();
        assert_eq!(find_label(&db, LABEL_CONTENT).len(), 2);

        MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .forget(&forget_request(
                target.scope,
                "erase-target",
                now + 2,
                &[("target", 1)],
            ))
            .unwrap();
        assert!(find_label(&db, LABEL_CONTENT).is_empty());
        assert!(
            db.graph("memory")
                .unwrap()
                .get_edge_by_key("fixture-incident-edge")
                .unwrap()
                .output
                .is_none()
        );
        assert!(
            db.graph("memory")
                .unwrap()
                .get_node(&retained_key)
                .unwrap()
                .output
                .is_some()
        );
    }

    #[test]
    fn forget_receipt_replays_after_reopen_on_all_backends() {
        let dir = tempfile::tempdir().unwrap();
        let encryption_key = crate::EncryptionKey::from_bytes([9; 32]);
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
            db.create_graph("memory").unwrap();
            let now = crate::current_time_ms() + 30_000;
            let request = timed_request("forgotten", "write", now);
            provision(&mut db, &request.scope);
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .remember(&request)
                .unwrap();
            let forget =
                forget_request(request.scope.clone(), "erase", now + 1, &[("forgotten", 1)]);
            let result = MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .forget(&forget)
                .unwrap();
            drop(db);

            let mut db = if encrypted {
                Database::open_encrypted(&path, &encryption_key).unwrap()
            } else {
                Database::open(&path).unwrap()
            };
            let before = db.sequence;
            let replay = MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .forget(&forget)
                .unwrap();
            assert!(replay.replayed, "{name}");
            assert_eq!(replay.deleted, result.deleted, "{name}");
            assert_eq!(replay.commit_sequence, result.commit_sequence, "{name}");
            assert_eq!(db.sequence, before, "{name}");

            let mut mismatch = forget.clone();
            mismatch.targets[0].expected_revision = 2;
            assert!(
                matches!(
                    MemoryMut::bind(&mut db, "memory", None)
                        .unwrap()
                        .forget(&mismatch),
                    Err(MemoryError::IdempotencyConflict)
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn expiry_and_evidence_survive_reopen_on_all_backends() {
        let dir = tempfile::tempdir().unwrap();
        let encryption_key = crate::EncryptionKey::from_bytes([11; 32]);
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
            db.create_graph("memory").unwrap();
            let now = crate::current_time_ms() + 30_000;
            let expires_at = now + 10_000;
            let record = seed_vector(&mut db, "reopen-evidence");
            let mut request = timed_request("expires", "expiry-reopen", now);
            request.items[0].kind = MemoryKind::Entity;
            request.items[0].expiry_ms = Some(expires_at);
            request.items[0].evidence.push(MemoryEvidence {
                id: "reopen-evidence".into(),
                scope: request.scope.clone(),
            });
            provision(&mut db, &request.scope);
            MemoryMut::bind(&mut db, "memory", Some("default"))
                .unwrap()
                .remember(&request)
                .unwrap();
            let key = memory_key(&request.scope.key(), "expires");
            drop(db);

            let mut db = if encrypted {
                Database::open_encrypted(&path, &encryption_key).unwrap()
            } else {
                Database::open(&path).unwrap()
            };
            assert!(
                db.graph("memory")
                    .unwrap()
                    .with_clock(expires_at - 1)
                    .get_node(&key)
                    .unwrap()
                    .output
                    .is_some(),
                "{name}"
            );
            assert_eq!(
                db.graph("memory")
                    .unwrap()
                    .with_clock(expires_at - 1)
                    .memory_node_attachments(&key)
                    .unwrap()
                    .len(),
                1,
                "{name}"
            );
            assert!(
                db.graph("memory")
                    .unwrap()
                    .with_clock(expires_at)
                    .get_node(&key)
                    .unwrap()
                    .output
                    .is_none(),
                "{name}"
            );
            assert!(db.get(record).unwrap().is_some(), "{name}");
            db.graph_mut("memory")
                .unwrap()
                .write_batch_at(
                    &[GraphOp::CreateNode {
                        key: "expiry-reopen-reclaim".into(),
                        labels: Vec::new(),
                        properties: Metadata::new(),
                    }],
                    expires_at + 1,
                )
                .unwrap();
            assert!(db.get(record).unwrap().is_some(), "{name}");
            assert!(
                db.graph("memory")
                    .unwrap()
                    .with_clock(expires_at + 1)
                    .memory_node_attachments(&key)
                    .unwrap()
                    .is_empty(),
                "{name}"
            );
        }
    }

    #[test]
    fn corrupted_forget_outcome_is_invalid_without_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("forget-outcome.vlite");
        let mut db = database(&path);
        let now = crate::current_time_ms() + 30_000;
        let request = timed_request("target", "write", now);
        provision(&mut db, &request.scope);
        MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .remember(&request)
            .unwrap();
        let forget = forget_request(request.scope.clone(), "erase", now + 1, &[("target", 1)]);
        MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .forget(&forget)
            .unwrap();

        let receipt_key = forget_receipt_key(&request.scope.key(), "erase");
        let outcome_key = receipt_outcome_key(&receipt_key, 0);
        let outcome = node(&db, &outcome_key);
        let mut properties = outcome.properties.clone();
        properties.insert(
            "outcome".into(),
            MetadataValue::String(format!("deleted:{}", hex(b"other-target"))),
        );
        replace_properties(&mut db, outcome, properties);
        let before_sequence = db.sequence;
        let before_end = db.end;
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .forget(&forget),
            Err(MemoryError::InvalidBundle)
        ));
        assert_eq!(db.sequence, before_sequence);
        assert_eq!(db.end, before_end);
    }

    #[test]
    fn receipt_chunk_count_tampering_is_invalid_without_writes() {
        for tamper in ["zero", "extra", "excessive"] {
            tampered_receipt_error(tamper);
        }
    }

    #[test]
    fn receipt_chunk_index_tampering_is_invalid_without_writes() {
        tampered_receipt_error("index");
    }

    #[test]
    fn receipt_chunk_owner_tampering_is_invalid_without_writes() {
        tampered_receipt_error("receipt_key");
    }

    #[test]
    fn receipt_missing_chunk_and_action_tampering_is_invalid_without_writes() {
        for tamper in ["missing", "action_count", "action_ids"] {
            tampered_receipt_error(tamper);
        }
    }

    fn merging_request(seed: &RememberRequest, key: &str) -> RememberRequest {
        let mut req = seed.clone();
        req.idempotency_key = key.into();
        req.items = (0..2)
            .map(|index| MemoryItem {
                id: format!("merge-{index}"),
                source: format!("new-source-{index}"),
                merge_into: Some(seed.items[0].id.clone()),
                expected_revision: Some(index + 1),
                ..seed.items[0].clone()
            })
            .collect();
        req
    }

    #[test]
    fn planned_merges_guard_the_scanned_graph_revision() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("guards.vlite"));
        let seed = request("target", "seed");
        provision(&mut db, &seed.scope);
        remember(&mut db, &seed);
        let graph_revision = node(&db, &memory_key(&seed.scope.key(), "target")).revision;
        for count in [1, 2] {
            let mut req = merging_request(&seed, "merge");
            req.items.truncate(count);
            let normalized = validate_request(&req).unwrap();
            let mut existing =
                scan_memories(&db.graph("memory").unwrap(), &req.scope.key()).unwrap();
            let planned = plan(&req, &normalized, &req.scope.key(), None, &mut existing).unwrap();
            assert_eq!(planned.budget_operations, count * 2);
            assert_eq!(planned.ops.len(), 1);
            for op in planned.ops {
                assert!(matches!(op, GraphOp::UpsertNode {
                    expected: Expected::Revision(revision), ..
                } if revision == graph_revision));
            }
        }
    }

    #[test]
    fn stale_plan_revision_fails_at_write_batch_without_a_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stale.vlite");
        let mut db = database(&path);
        let seed = request("target", "seed");
        provision(&mut db, &seed.scope);
        remember(&mut db, &seed);
        let key = memory_key(&seed.scope.key(), "target");
        let before = node(&db, &key);
        let sequence = db.sequence;
        let req = merging_request(&seed, "stale");
        BUMP_BEFORE_WRITE_BATCH.with(|target| *target.borrow_mut() = Some(key.clone()));
        let result = MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .remember(&req);
        assert!(
            matches!(result, Err(MemoryError::Storage(Error::RevisionMismatch {
            current: Some(current)
        })) if current == sequence + 1)
        );
        assert_eq!(
            db.sequence,
            sequence + 1,
            "only the independent write committed"
        );
        assert_eq!(
            db.sequence,
            BUMPED_BATCH_SEQUENCE.with(std::cell::Cell::get)
        );
        assert_eq!(db.end, BUMPED_BATCH_END.with(std::cell::Cell::get));
        let after = node(&db, &key);
        assert_eq!(after.properties, before.properties);
        assert_eq!(after.revision, sequence + 1);
        assert!(
            db.graph("memory")
                .unwrap()
                .get_node(&receipt_key(&seed.scope.key(), "stale"))
                .unwrap()
                .output
                .is_none()
        );
        let end = db.end;
        drop(db);
        assert_eq!(fs::metadata(path).unwrap().len(), end);
    }

    #[test]
    fn inline_content_boundary_is_inclusive_and_utf8_exact() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("inline.vlite"));
        for (index, content) in [
            "x".repeat(4_096),
            "x".repeat(4_097),
            format!("{}€", "x".repeat(4_093)),
            format!("{}€", "x".repeat(4_094)),
        ]
        .into_iter()
        .enumerate()
        {
            let mut req = request(&format!("boundary-{index}"), &format!("boundary-{index}"));
            req.items[0].content = content.clone();
            provision(&mut db, &req.scope);
            remember(&mut db, &req);
            let key = memory_key(&req.scope.key(), &req.items[0].id);
            let memory = node(&db, &key);
            if content.len() == 4_096 {
                assert_eq!(string_property(&memory, "content").unwrap(), content);
                assert!(
                    db.graph("memory")
                        .unwrap()
                        .get_node(&content_key(&key, 0))
                        .unwrap()
                        .output
                        .is_none()
                );
                assert!(
                    db.graph("memory")
                        .unwrap()
                        .get_edge_by_key(&content_edge_key(&key, 0))
                        .unwrap()
                        .output
                        .is_none()
                );
            } else {
                assert_large_content_readback(&db, &req.scope, &req.items[0].id, &content);
            }
        }
    }

    #[test]
    fn exact_operation_boundaries_charge_receipts_outcomes_and_content() {
        let dir = tempfile::tempdir().unwrap();
        for chunked in [false, true] {
            let path = dir.path().join(format!("budget-{chunked}.vlite"));
            let mut db = database(&path);
            let mut req = request("m", "accepted");
            provision(&mut db, &req.scope);
            let accepted = if chunked { 4_090 } else { 4_094 };
            req.items = (0..accepted)
                .map(|index| MemoryItem {
                    id: format!("m{index}"),
                    content: if chunked && index == 0 {
                        "x".repeat(4_097)
                    } else {
                        format!("content-{index}")
                    },
                    ..req.items[0].clone()
                })
                .collect();
            let normalized = validate_request(&req).unwrap();
            let planned = plan(&req, &normalized, &req.scope.key(), None, &mut Vec::new()).unwrap();
            assert_eq!(planned.budget_operations, 4_094);
            assert_eq!(encode_actions(&planned.actions).len().div_ceil(4_096), 1);
            let sequence = db.sequence;
            remember(&mut db, &req);
            assert_eq!(db.sequence, sequence + 1);
            drop(db);
            let baseline = fs::read(&path).unwrap();
            let mut db = Database::open(&path).unwrap();
            // Use a fresh scope so every item is a create, not a deduplicated noop.
            req.scope.agent = "other-agent".into();
            provision(&mut db, &req.scope);
            req.items.push(MemoryItem {
                id: "extra".into(),
                content: "extra".into(),
                ..req.items[0].clone()
            });
            let sequence = db.sequence;
            drop(db);
            let rejected_before = fs::read(&path).unwrap();
            assert!(
                rejected_before.len() > baseline.len(),
                "scope provisioning was independent"
            );
            let mut db = Database::open(&path).unwrap();
            assert!(matches!(
                MemoryMut::bind(&mut db, "memory", None)
                    .unwrap()
                    .remember(&req),
                Err(MemoryError::OperationLimit)
            ));
            assert_eq!(db.sequence, sequence);
            assert!(
                scan_memories(&db.graph("memory").unwrap(), &req.scope.key())
                    .unwrap()
                    .is_empty()
            );
            drop(db);
            assert_eq!(fs::read(path).unwrap(), rejected_before);
        }
    }

    #[test]
    fn oversized_physical_frame_is_rejected_without_append() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frame-budget.vlite");
        let mut db = database(&path);
        let mut req = request("m", "frame-budget");
        provision(&mut db, &req.scope);
        req.items = (0..3_800)
            .map(|index| MemoryItem {
                id: format!("m{index}"),
                content: format!("{index:04}{}", "x".repeat(4_092)),
                ..req.items[0].clone()
            })
            .collect();
        let normalized = validate_request(&req).unwrap();
        let planned = plan(&req, &normalized, &req.scope.key(), None, &mut Vec::new()).unwrap();
        assert!(planned.encoded_request_bytes <= MAX_BODY_BYTES);
        assert!(planned.budget_operations + 2 < MAX_OPS);
        let sequence = db.sequence;
        drop(db);
        let before = fs::read(&path).unwrap();
        let mut db = Database::open(&path).unwrap();
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .remember(&req),
            Err(MemoryError::BodyLimit)
        ));
        assert_eq!(db.sequence, sequence);
        assert!(
            scan_memories(&db.graph("memory").unwrap(), &req.scope.key())
                .unwrap()
                .is_empty()
        );
        assert!(
            db.graph("memory")
                .unwrap()
                .get_node(&receipt_key(&req.scope.key(), &req.idempotency_key))
                .unwrap()
                .output
                .is_none()
        );
        drop(db);
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn worst_case_noop_receipts_replay_on_all_backends_and_charge_outcome_ops() {
        let dir = tempfile::tempdir().unwrap();
        let encryption_key = crate::EncryptionKey::from_bytes([7; 32]);
        for (name, directory, encrypted) in [
            ("plain-file", false, false),
            ("encrypted-file", false, true),
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
            db.create_graph("memory").unwrap();
            let req = seed_long_noops(&mut db, MAX_OPS);
            let sequence = db.sequence;
            let result = remember(&mut db, &req);
            assert_eq!(db.sequence, sequence + 1);
            assert!(
                result
                    .actions
                    .iter()
                    .all(|action| action.id.len() == 256 && action.revision == u64::MAX)
            );
            let receipt = node(&db, &receipt_key(&req.scope.key(), &req.idempotency_key));
            assert_eq!(integer_property(&receipt, "outcome_chunks").unwrap(), 540);
            drop(db);
            let mut db = if encrypted {
                Database::open_encrypted(&path, &encryption_key).unwrap()
            } else {
                Database::open(&path).unwrap()
            };
            let sequence = db.sequence;
            let replay = remember(&mut db, &req);
            assert!(replay.replayed);
            assert_eq!(replay.actions, result.actions);
            assert_eq!(db.sequence, sequence);

            let mut boundary = req.clone();
            boundary.idempotency_key = "worst-budget".into();
            boundary.items.pop();
            let mut created = request("with-references", "unused").items.remove(0);
            created.content = "different content".into();
            created.references = vec![req.items[0].id.clone(); 3_554];
            // References use the retained canonical ID, never a noop alias.
            created.references.fill("c".repeat(256));
            boundary.items.push(created);
            let normalized = validate_request(&boundary).unwrap();
            let mut existing =
                scan_memories(&db.graph("memory").unwrap(), &boundary.scope.key()).unwrap();
            let planned = plan(
                &boundary,
                &normalized,
                &boundary.scope.key(),
                None,
                &mut existing,
            )
            .unwrap();
            let chunk_count = encode_actions(&planned.actions).len().div_ceil(4_096);
            assert_eq!(chunk_count, 540);
            assert_eq!(planned.budget_operations + 1 + chunk_count, MAX_OPS);
            remember(&mut db, &boundary);
            let sequence = db.sequence;
            let end = db.end;
            boundary.idempotency_key = "worst-rejected".into();
            boundary.items.last_mut().unwrap().id = "extra-reference".into();
            boundary.items.last_mut().unwrap().content = "another different content".into();
            boundary
                .items
                .last_mut()
                .unwrap()
                .references
                .push("c".repeat(256));
            assert!(matches!(
                MemoryMut::bind(&mut db, "memory", None)
                    .unwrap()
                    .remember(&boundary),
                Err(MemoryError::OperationLimit)
            ));
            assert_eq!(db.sequence, sequence);
            assert_eq!(db.end, end);
        }
    }

    #[test]
    fn receipt_and_memory_are_written_in_one_graph_frame_and_replay() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.vlite");
        let mut db = database(&path);
        let req = request("m1", "k1");
        provision(&mut db, &req.scope);
        let before = fs::metadata(&path).unwrap().len();
        let result = MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .remember(&req)
            .unwrap();
        assert_eq!(result.commit_sequence, result.receipt_revision);
        assert_eq!(result.actions[0].action, "created");
        assert_eq!(
            db.graph("memory")
                .unwrap()
                .get_node(&memory_key(&req.scope.key(), "m1"))
                .unwrap()
                .output
                .unwrap()
                .revision,
            result.commit_sequence
        );
        assert!(fs::metadata(&path).unwrap().len() > before);
        drop(db);

        let mut db = Database::open(&path).unwrap();
        let replay = MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .remember(&request("m1", "k1"))
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.actions, result.actions);
        assert_eq!(replay.commit_sequence, result.commit_sequence);
        let mismatch = MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .remember(&request("other", "k1"));
        assert!(matches!(mismatch, Err(MemoryError::IdempotencyConflict)));
    }

    #[test]
    fn receipt_and_memory_survive_plain_encrypted_file_and_directory_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let encryption_key = crate::EncryptionKey::from_bytes([7; 32]);
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
            db.create_graph("memory").unwrap();
            let req = request("m1", "reopen");
            provision(&mut db, &req.scope);
            let result = MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .remember(&req)
                .unwrap();
            drop(db);

            let mut db = if encrypted {
                Database::open_encrypted(&path, &encryption_key).unwrap()
            } else {
                Database::open(&path).unwrap()
            };
            let replay = MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .remember(&req)
                .unwrap();
            assert!(replay.replayed, "{name}");
            assert_eq!(replay.actions, result.actions, "{name}");
            assert_eq!(replay.commit_sequence, result.commit_sequence, "{name}");
            let mismatch = MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .remember(&request("different", "reopen"));
            assert!(
                matches!(mismatch, Err(MemoryError::IdempotencyConflict)),
                "{name}"
            );
        }
    }

    #[test]
    fn expired_execution_deadline_does_not_block_matching_receipt_replay() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.vlite");
        let mut db = database(&path);
        let req = request("m1", "deadline");
        provision(&mut db, &req.scope);
        MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .remember(&req)
            .unwrap();

        let retry = RememberRequest {
            now_ms: 5,
            deadline_ms: Some(5),
            ..req
        };
        let replay = MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .remember(&retry)
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.actions[0].id, "m1");
    }

    #[test]
    fn validation_and_operation_budget_fail_without_appending() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.vlite");
        let mut db = database(&path);
        let req = request("baseline", "seed");
        provision(&mut db, &req.scope);
        MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .remember(&req)
            .unwrap();
        let baseline_sequence = db.sequence;
        let baseline_memory_revision = db
            .graph("memory")
            .unwrap()
            .get_node(&memory_key(&req.scope.key(), "baseline"))
            .unwrap()
            .output
            .unwrap()
            .revision;
        let baseline_receipt_count = find_label(&db, LABEL_RECEIPT).len();
        drop(db);
        let baseline = fs::read(&path).unwrap();
        let mut db = Database::open(&path).unwrap();
        let invalid = RememberRequest {
            items: vec![MemoryItem {
                importance: Some(f64::NAN),
                ..request("invalid", "invalid").items.remove(0)
            }],
            ..request("invalid", "invalid")
        };
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .remember(&invalid),
            Err(MemoryError::InvalidImportance)
        ));
        assert_eq!(db.sequence, baseline_sequence);
        assert_eq!(
            db.graph("memory")
                .unwrap()
                .get_node(&memory_key(&req.scope.key(), "baseline"))
                .unwrap()
                .output
                .unwrap()
                .revision,
            baseline_memory_revision
        );
        assert_eq!(find_label(&db, LABEL_RECEIPT).len(), baseline_receipt_count);
        drop(db);
        assert_eq!(fs::read(&path).unwrap(), baseline);
        let mut db = Database::open(&path).unwrap();

        let mut oversized = request("m0", "many");
        oversized.items = (0..MAX_OPS)
            .map(|index| MemoryItem {
                id: format!("m{index}"),
                kind: MemoryKind::Episodic,
                content: "x".into(),
                source: format!("s{index}"),
                importance: None,
                observed_at_ms: Some(10),
                provenance: Vec::new(),
                references: Vec::new(),
                merge_into: None,
                expected_revision: None,
                expiry_ms: None,
                expiry_valid: true,
                evidence: Vec::new(),
            })
            .collect();
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .remember(&oversized),
            Err(MemoryError::OperationLimit)
        ));
        assert_eq!(db.sequence, baseline_sequence);
        assert_eq!(
            db.graph("memory")
                .unwrap()
                .get_node(&memory_key(&req.scope.key(), "baseline"))
                .unwrap()
                .output
                .unwrap()
                .revision,
            baseline_memory_revision
        );
        assert_eq!(find_label(&db, LABEL_RECEIPT).len(), baseline_receipt_count);
        assert!(
            db.graph("memory")
                .unwrap()
                .get_node(&memory_key(&oversized.scope.key(), "m0"))
                .unwrap()
                .output
                .is_none()
        );
        let mut oversized_body = request("body", "body");
        oversized_body.items[0].content = "x".repeat(MAX_BODY_BYTES);
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .remember(&oversized_body),
            Err(MemoryError::BodyLimit)
        ));
        assert_eq!(db.sequence, baseline_sequence);
        assert_eq!(find_label(&db, LABEL_RECEIPT).len(), baseline_receipt_count);
        drop(db);
        assert_eq!(fs::read(&path).unwrap(), baseline);
    }

    #[test]
    fn exact_dedup_is_scope_kind_content_and_source_specific() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.vlite");
        let mut db = database(&path);
        let first = request("m1", "one");
        provision(&mut db, &first.scope);
        let created = MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .remember(&first)
            .unwrap();
        let second = RememberRequest {
            idempotency_key: "two".into(),
            items: vec![MemoryItem {
                id: "another-id".into(),
                ..first.items[0].clone()
            }],
            ..first.clone()
        };
        let duplicate = MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .remember(&second)
            .unwrap();
        assert_eq!(duplicate.actions[0].action, "noop");
        assert_eq!(duplicate.actions[0].id, "m1");
        assert_ne!(duplicate.commit_sequence, created.commit_sequence);

        let independent = RememberRequest {
            idempotency_key: "three".into(),
            items: vec![MemoryItem {
                id: "independent".into(),
                source: "other-source".into(),
                ..first.items[0].clone()
            }],
            ..first
        };
        let created_independent = MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .remember(&independent)
            .unwrap();
        assert_eq!(created_independent.actions[0].action, "created");
    }

    #[test]
    fn large_noop_receipt_outcomes_are_chunked_and_replayable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.vlite");
        let mut db = database(&path);
        let first = request("seed", "seed");
        provision(&mut db, &first.scope);
        MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .remember(&first)
            .unwrap();

        for (case, count, id_len) in [("36-byte", 900, 36), ("256-byte", MAX_OPS, 256)] {
            let items = (0..count)
                .map(|index| {
                    let prefix = format!("{index:04}-");
                    let id = format!("{prefix}{}", "x".repeat(id_len - prefix.len()));
                    MemoryItem {
                        id,
                        ..first.items[0].clone()
                    }
                })
                .collect();
            let retry = RememberRequest {
                idempotency_key: format!("large-outcome-{case}"),
                items,
                ..first.clone()
            };
            let before = db.sequence;
            let result = MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .remember(&retry)
                .unwrap();
            assert!(result.actions.iter().all(|action| action.action == "noop"));
            assert_eq!(db.sequence, before + 1);
            let receipt = db
                .graph("memory")
                .unwrap()
                .get_node(&receipt_key(&retry.scope.key(), &retry.idempotency_key))
                .unwrap()
                .output
                .unwrap();
            assert!(integer_property(&receipt, "outcome_chunks").unwrap() > 1);
            let outcome_nodes = find_label(&db, LABEL_RECEIPT_OUTCOME);
            assert!(
                outcome_nodes
                    .iter()
                    .any(|node| { string_property(node, "receipt_key").unwrap() == receipt.key })
            );
            if case == "256-byte" {
                drop(db);
                db = Database::open(&path).unwrap();
            }
            let replay = MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .remember(&retry)
                .unwrap();
            assert!(replay.replayed);
            assert_eq!(replay.actions, result.actions);
            assert_eq!(db.sequence, before + 1, "{case}");
        }
    }

    fn assert_large_content_readback(db: &Database, scope: &MemoryScope, id: &str, content: &str) {
        let graph = db.graph("memory").unwrap();
        let key = memory_key(&scope.key(), id);
        let memory = graph.get_node(&key).unwrap().output.unwrap();
        assert!(!memory.properties.contains_key("content"));
        let chunks = content_chunks(content).unwrap();
        let expected_lengths = if matches!(content.len(), 4_097 | 4_098) && content.ends_with('€')
        {
            vec![content.len() - 3, 3]
        } else {
            content
                .as_bytes()
                .chunks(MAX_STRING_VALUE_BYTES)
                .map(<[u8]>::len)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            chunks.iter().map(String::len).collect::<Vec<_>>(),
            expected_lengths
        );
        let mut rebuilt = String::new();
        for (index, expected) in chunks.iter().enumerate() {
            let chunk_key = content_key(&key, index);
            let node = graph.get_node(&chunk_key).unwrap().output.unwrap();
            assert_eq!(node.labels, BTreeSet::from([LABEL_CONTENT.to_owned()]));
            assert_eq!(integer_property(&node, "index").ok(), Some(index as i64));
            let actual = string_property(&node, "content").unwrap();
            assert!(actual.len() <= MAX_STRING_VALUE_BYTES);
            assert!(actual.is_char_boundary(actual.len()));
            assert_eq!(actual, expected);
            rebuilt.push_str(actual);

            let edge_key = content_edge_key(&key, index);
            let edge = graph.get_edge_by_key(&edge_key).unwrap().output.unwrap();
            assert_eq!(edge.key.as_deref(), Some(edge_key.as_str()));
            assert_eq!(edge.relationship, "HAS_CONTENT");
            assert_eq!(edge.source_key, key);
            assert_eq!(edge.target_key, chunk_key);
        }
        let neighbors = graph
            .neighbors(
                &Neighbors::new(NodeSelector::key(&key)),
                &ReadOptions {
                    limits: GraphLimits::CEILING,
                    ..ReadOptions::default()
                },
            )
            .unwrap()
            .output;
        assert_eq!(neighbors.edges.len(), chunks.len());
        for (index, edge) in neighbors.edges.iter().enumerate() {
            assert_eq!(
                edge.key.as_deref(),
                Some(content_edge_key(&key, index).as_str())
            );
            assert_eq!(edge.relationship, "HAS_CONTENT");
        }
        assert_eq!(rebuilt, content);
        assert!(
            graph
                .get_node(&content_key(&key, chunks.len()))
                .unwrap()
                .output
                .is_none()
        );
        assert!(
            graph
                .get_edge_by_key(&content_edge_key(&key, chunks.len()))
                .unwrap()
                .output
                .is_none()
        );
    }

    #[test]
    fn large_content_chunks_read_back_on_all_reopen_backends() {
        let dir = tempfile::tempdir().unwrap();
        let encryption_key = crate::EncryptionKey::from_bytes([7; 32]);
        let contents = [
            format!("{}€", "a".repeat(4_095)),
            "b".repeat(8_192),
            "c".repeat(8_193),
        ];
        assert_eq!(contents[0].len(), 4_098);
        for (case, content) in contents.iter().enumerate() {
            for (name, directory, encrypted) in [
                ("plain-file", false, false),
                ("encrypted-file", false, true),
                ("plain-directory", true, false),
                ("encrypted-directory", true, true),
            ] {
                let path = dir.path().join(format!("{name}-{case}"));
                let options = crate::CreateOptions {
                    encryption: encrypted.then(|| encryption_key.clone()),
                    ..crate::CreateOptions::default()
                };
                let mut db = if directory {
                    Database::create_directory(&path, 2, options).unwrap()
                } else {
                    Database::create_with_options(&path, 2, options).unwrap()
                };
                db.create_graph("memory").unwrap();
                let req = request(&format!("large-{case}"), &format!("large-{case}"));
                provision(&mut db, &req.scope);
                let mut large = req.clone();
                large.items[0].content = content.clone();
                MemoryMut::bind(&mut db, "memory", None)
                    .unwrap()
                    .remember(&large)
                    .unwrap();
                assert_large_content_readback(&db, &large.scope, &large.items[0].id, content);
                drop(db);

                let db = if encrypted {
                    Database::open_encrypted(&path, &encryption_key).unwrap()
                } else {
                    Database::open(&path).unwrap()
                };
                assert_large_content_readback(&db, &large.scope, &large.items[0].id, content);
            }
        }
    }

    #[test]
    fn large_content_dedup_and_merge_preserve_chunk_nodes_and_edges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.vlite");
        let mut db = database(&path);
        let mut created = request("large", "large-create");
        created.items[0].content = "z".repeat(8_193);
        provision(&mut db, &created.scope);
        MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .remember(&created)
            .unwrap();
        let original_chunk_keys = content_chunks(&created.items[0].content)
            .unwrap()
            .iter()
            .enumerate()
            .map(|(index, _)| content_key(&memory_key(&created.scope.key(), "large"), index))
            .collect::<Vec<_>>();
        let original_edge_keys = (0..original_chunk_keys.len())
            .map(|index| content_edge_key(&memory_key(&created.scope.key(), "large"), index))
            .collect::<Vec<_>>();

        let sequence_before = db.sequence;
        let mut duplicate = created.clone();
        duplicate.idempotency_key = "large-dedup".into();
        duplicate.items[0].id = "alias".into();
        let result = MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .remember(&duplicate)
            .unwrap();
        assert_eq!(result.actions[0].action, "noop");
        assert_eq!(
            db.sequence,
            sequence_before + 1,
            "new receipt uses one frame"
        );
        let receipt_sequence = db.sequence;
        let replay = MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .remember(&duplicate)
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(
            db.sequence, receipt_sequence,
            "receipt replay is frame-free"
        );

        let mut merge = created.clone();
        merge.idempotency_key = "large-merge".into();
        merge.items[0].id = "merge-alias".into();
        merge.items[0].source = "second-source".into();
        merge.items[0].merge_into = Some("large".into());
        merge.items[0].expected_revision = Some(1);
        merge.items[0].provenance = vec![Provenance {
            source: "second-source".into(),
            kind: None,
            uri: None,
            transformation: None,
        }];
        MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .remember(&merge)
            .unwrap();
        let actual_chunks = content_chunks(&created.items[0].content)
            .unwrap()
            .iter()
            .enumerate()
            .map(|(index, _)| content_key(&memory_key(&created.scope.key(), "large"), index))
            .collect::<Vec<_>>();
        let actual_edges = (0..actual_chunks.len())
            .map(|index| content_edge_key(&memory_key(&created.scope.key(), "large"), index))
            .collect::<Vec<_>>();
        assert_eq!(actual_chunks, original_chunk_keys);
        assert_eq!(actual_edges, original_edge_keys);
        assert_large_content_readback(&db, &created.scope, "large", &created.items[0].content);
    }

    #[test]
    fn large_content_chunk_and_edge_ops_are_budgeted_before_append() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.vlite");
        let mut db = database(&path);
        let req = request("too-large", "too-many-content-ops");
        provision(&mut db, &req.scope);
        let sequence = db.sequence;
        drop(db);
        let baseline = fs::read(&path).unwrap();
        let mut db = Database::open(&path).unwrap();
        let mut oversized = req;
        oversized.items[0].content = "x".repeat(MAX_STRING_VALUE_BYTES * 2_048);
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .remember(&oversized),
            Err(MemoryError::OperationLimit)
        ));
        assert_eq!(db.sequence, sequence);
        assert!(find_label(&db, LABEL_MEMORY).is_empty());
        assert!(find_label(&db, LABEL_CONTENT).is_empty());
        assert!(find_label(&db, LABEL_RECEIPT).is_empty());
        drop(db);
        assert_eq!(fs::read(&path).unwrap(), baseline);
    }

    #[test]
    fn exact_dedup_scans_all_memory_pages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.vlite");
        let mut db = database(&path);
        let mut seed = request("m0", "seed-many");
        provision(&mut db, &seed.scope);
        seed.items = (0..1_100)
            .map(|index| MemoryItem {
                id: format!("memory-{index}"),
                kind: MemoryKind::Episodic,
                content: format!("content-{index}"),
                source: format!("source-{index}"),
                importance: None,
                observed_at_ms: Some(10),
                provenance: Vec::new(),
                references: Vec::new(),
                merge_into: None,
                expected_revision: None,
                expiry_ms: None,
                expiry_valid: true,
                evidence: Vec::new(),
            })
            .collect();
        MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .remember(&seed)
            .unwrap();

        let mut duplicate = RememberRequest {
            idempotency_key: "dedup-many".into(),
            items: vec![seed.items[1_099].clone()],
            ..seed
        };
        duplicate.items[0].id = "alias".into();
        let result = MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .remember(&duplicate)
            .unwrap();
        assert_eq!(result.actions[0].action, "noop");
        assert_eq!(result.actions[0].id, "memory-1099");
    }

    #[test]
    fn graph_batch_failure_keeps_receipt_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.vlite");
        let mut db = database(&path);
        let request = request("m1", "atomic");
        provision(&mut db, &request.scope);
        let sequence = db.sequence;
        drop(db);
        let before = fs::read(&path).unwrap();
        let mut db = Database::open(&path).unwrap();
        FAIL_NEXT_WRITE_BATCH.with(|fail| fail.set(true));
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .remember(&request),
            Err(MemoryError::Storage(Error::GraphKeyExists))
        ));
        assert_eq!(db.sequence, sequence);
        let graph = db.graph("memory").unwrap();
        assert!(
            graph
                .get_node(&receipt_key(&request.scope.key(), &request.idempotency_key))
                .unwrap()
                .output
                .is_none()
        );
        assert!(
            graph
                .get_node(&memory_key(&request.scope.key(), "m1"))
                .unwrap()
                .output
                .is_none()
        );
        assert!(
            graph
                .get_node(&memory_key(&request.scope.key(), "m1"))
                .unwrap()
                .output
                .is_none()
        );
        drop(db);
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn wrongly_seeded_scope_tuple_is_a_digest_collision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.vlite");
        let mut db = database(&path);
        let scope = request("m", "k").scope;
        let mut properties = Metadata::new();
        properties.insert("tuple".into(), MetadataValue::String("wrong-tuple".into()));
        db.graph_mut("memory")
            .unwrap()
            .write_batch(&[GraphOp::CreateNode {
                key: scope.key(),
                labels: vec![LABEL_SCOPE.into()],
                properties,
            }])
            .unwrap();
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .provision_scope(&scope),
            Err(MemoryError::DigestCollision)
        ));
    }

    #[test]
    fn provenance_encoded_property_limit_accepts_4096_and_rejects_4097_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.vlite");
        let mut db = database(&path);
        let mut accepted = request("at-limit", "at-limit");
        accepted.items[0].provenance = (0..64)
            .map(|_| Provenance {
                source: "p".into(),
                kind: None,
                uri: Some("u".repeat(52)),
                transformation: None,
            })
            .collect();
        assert_eq!(
            encode_provenance(&accepted.items[0].provenance).len(),
            4_096
        );
        provision(&mut db, &accepted.scope);
        MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .remember(&accepted)
            .unwrap();
        let sequence = db.sequence;
        let mut rejected = accepted;
        rejected.items[0].id = "over-limit".into();
        rejected.idempotency_key = "over-limit".into();
        rejected.items[0].provenance[0].uri = Some("u".repeat(53));
        assert_eq!(
            encode_provenance(&rejected.items[0].provenance).len(),
            4_097
        );
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .remember(&rejected),
            Err(MemoryError::ProvenanceLimit)
        ));
        assert_eq!(db.sequence, sequence);
        assert!(
            db.graph("memory")
                .unwrap()
                .get_node(&receipt_key(&rejected.scope.key(), "over-limit"))
                .unwrap()
                .output
                .is_none()
        );
    }

    #[test]
    fn merge_identity_conflict_is_preflighted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.vlite");
        let mut db = database(&path);
        let mut created = request("target", "seed");
        created.items[0].kind = MemoryKind::Entity;
        provision(&mut db, &created.scope);
        MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .remember(&created)
            .unwrap();
        let sequence = db.sequence;
        let mut merge = request("alias", "merge");
        merge.scope = created.scope.clone();
        merge.items[0].kind = MemoryKind::Entity;
        merge.items[0].content = "different".into();
        merge.items[0].merge_into = Some("target".into());
        merge.items[0].expected_revision = Some(1);
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .remember(&merge),
            Err(MemoryError::MergeIdentityConflict)
        ));
        assert_eq!(db.sequence, sequence);
        assert!(
            db.graph("memory")
                .unwrap()
                .get_node(&receipt_key(&merge.scope.key(), "merge"))
                .unwrap()
                .output
                .is_none()
        );
    }

    #[test]
    fn merge_preserves_importance_creation_and_observation_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.vlite");
        let mut db = database(&path);
        let mut created = request("target", "seed");
        created.items[0].kind = MemoryKind::Entity;
        created.items[0].importance = Some(0.2);
        created.items[0].observed_at_ms = Some(7);
        provision(&mut db, &created.scope);
        MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .remember(&created)
            .unwrap();
        let mut merge = request("alias", "merge");
        merge.scope = created.scope.clone();
        merge.now_ms = 20;
        merge.items[0].kind = MemoryKind::Entity;
        merge.items[0].source = "source-2".into();
        merge.items[0].importance = Some(1.0);
        merge.items[0].observed_at_ms = Some(19);
        merge.items[0].merge_into = Some("target".into());
        merge.items[0].expected_revision = Some(1);
        MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .remember(&merge)
            .unwrap();
        let node = db
            .graph("memory")
            .unwrap()
            .get_node(&memory_key(&created.scope.key(), "target"))
            .unwrap()
            .output
            .unwrap();
        assert_eq!(float_property(&node, "importance").unwrap(), 0.2);
        assert_eq!(integer_property(&node, "created_at_ms").unwrap(), 10);
        assert_eq!(integer_property(&node, "observed_at_ms").unwrap(), 7);
        assert_eq!(integer_property(&node, "updated_at_ms").unwrap(), 20);
    }

    #[test]
    fn reference_edge_indexes_continue_across_merges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.vlite");
        let mut db = database(&path);
        let base = request("anchor", "anchor");
        provision(&mut db, &base.scope);
        MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .remember(&base)
            .unwrap();
        let mut target = request("target", "target");
        target.scope = base.scope.clone();
        target.items[0].content = "target-content".into();
        MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .remember(&target)
            .unwrap();
        let mut create = request("memory", "create-memory");
        create.scope = base.scope.clone();
        create.items[0].kind = MemoryKind::Entity;
        create.items[0].content = "shared-content".into();
        create.items[0].references = vec!["anchor".into()];
        MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .remember(&create)
            .unwrap();
        for (index, reference) in ["target", "target"].iter().enumerate() {
            let mut merge = request(&format!("merge-{index}"), &format!("merge-{index}"));
            merge.scope = base.scope.clone();
            merge.items[0].kind = MemoryKind::Entity;
            merge.items[0].content = "shared-content".into();
            merge.items[0].source = format!("source-{index}");
            merge.items[0].references = vec![(*reference).into()];
            merge.items[0].merge_into = Some("memory".into());
            merge.items[0].expected_revision = Some(index as u64 + 1);
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .remember(&merge)
                .unwrap();
        }
        let source_key = memory_key(&base.scope.key(), "memory");
        let anchor_key = memory_key(&base.scope.key(), "anchor");
        let target_key = memory_key(&base.scope.key(), "target");
        let first = db
            .graph("memory")
            .unwrap()
            .get_edge_by_key(&reference_edge_key(&source_key, &anchor_key, 0))
            .unwrap()
            .output
            .unwrap();
        let second = db
            .graph("memory")
            .unwrap()
            .get_edge_by_key(&reference_edge_key(&source_key, &target_key, 1))
            .unwrap()
            .output
            .unwrap();
        let third = db
            .graph("memory")
            .unwrap()
            .get_edge_by_key(&reference_edge_key(&source_key, &target_key, 2))
            .unwrap()
            .output
            .unwrap();
        assert_eq!(first.source_key, source_key);
        assert_eq!(second.source_key, source_key);
        assert_eq!(third.source_key, source_key);
    }

    #[test]
    fn op12_receipt_is_the_batch_revision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.vlite");
        let mut db = database(&path);
        let req = request("m1", "k1");
        provision(&mut db, &req.scope);
        let scope = req.scope.key();
        let result = MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .remember(&req)
            .unwrap();
        let receipt = db
            .graph("memory")
            .unwrap()
            .get_node(&receipt_key(&scope, "k1"))
            .unwrap()
            .output
            .unwrap();
        assert_eq!(receipt.revision, result.commit_sequence);
        let memory = db
            .graph("memory")
            .unwrap()
            .get_node(&memory_key(&scope, "m1"))
            .unwrap()
            .output
            .unwrap();
        assert_eq!(memory.revision, result.commit_sequence);
    }

    fn expanded(value: &Value) -> Value {
        let Some(object) = value.as_object() else {
            return value.clone();
        };
        if !object.contains_key("repeat") || !object.contains_key("count") {
            return value.clone();
        }
        let count = object["count"].as_u64().unwrap() as usize;
        match &object["repeat"] {
            Value::String(repeat) => Value::String(repeat.repeat(count)),
            Value::Object(repeated) => {
                let enumerate = object
                    .get("enumerate")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                Value::Array(
                    (0..count)
                        .map(|index| {
                            let mut item = repeated.clone();
                            for field in &enumerate {
                                let field = field.as_str().unwrap();
                                let original = item[field].as_str().unwrap();
                                item.insert(field.into(), json!(format!("{original}{index}")));
                            }
                            Value::Object(item)
                        })
                        .collect(),
                )
            }
            _ => panic!("oracle repeat value must be text or object"),
        }
    }

    fn fixture_scope(value: &Value) -> MemoryScope {
        MemoryScope {
            tenant: value
                .get("tenant")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            agent: value
                .get("agent")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            user: value.get("user").and_then(Value::as_str).map(str::to_owned),
            session: value
                .get("session")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }
    }

    fn fixture_request(value: &Value, clock: &Value, clock_offset: u64) -> RememberRequest {
        let mut items = expanded(&value["items"])
            .as_array()
            .cloned()
            .unwrap_or_default();
        let items = items
            .iter_mut()
            .map(|item| {
                if let Some(content) = item.get("content") {
                    item["content"] = expanded(content);
                }
                if let Some(provenance) = item.get("provenance") {
                    item["provenance"] = expanded(provenance);
                }
                let provenance = item
                    .get("provenance")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .map(|entry| Provenance {
                                source: entry["source"].as_str().unwrap_or_default().into(),
                                kind: entry.get("kind").and_then(Value::as_str).map(str::to_owned),
                                uri: entry.get("uri").and_then(Value::as_str).map(str::to_owned),
                                transformation: entry
                                    .get("transformation")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let kind = match item.get("kind").and_then(Value::as_str) {
                    Some("entity") => MemoryKind::Entity,
                    Some("episodic") => MemoryKind::Episodic,
                    _ => MemoryKind::Unsupported,
                };
                let references = item
                    .get("references")
                    .and_then(Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                MemoryItem {
                    id: item
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                    kind,
                    content: expanded(&item["content"])
                        .as_str()
                        .unwrap_or_default()
                        .into(),
                    source: item
                        .get("source")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                    importance: item.get("importance").and_then(Value::as_f64),
                    observed_at_ms: item
                        .get("observed_at")
                        .and_then(Value::as_i64)
                        .map(|value| value.saturating_add(clock_offset as i64)),
                    provenance,
                    references,
                    merge_into: item
                        .get("merge_into")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    expected_revision: item.get("expected_revision").and_then(Value::as_u64),
                    expiry_ms: item
                        .get("expiry")
                        .and_then(Value::as_u64)
                        .map(|value| value.saturating_add(clock_offset)),
                    expiry_valid: item
                        .get("expiry")
                        .is_none_or(|value| value.is_null() || value.as_u64().is_some()),
                    evidence: item
                        .get("evidence")
                        .and_then(Value::as_array)
                        .map(|values| {
                            values
                                .iter()
                                .map(|entry| MemoryEvidence {
                                    id: entry
                                        .get("id")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_owned(),
                                    scope: fixture_scope(&entry["scope"]),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                }
            })
            .collect();
        let scope = fixture_scope(&value["scope"]);
        RememberRequest {
            scope,
            idempotency_key: value
                .get("key")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
            receipt_window_ms: value
                .get("receipt_window")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            now_ms: clock
                .as_u64()
                .unwrap_or_default()
                .saturating_add(clock_offset),
            deadline_ms: value
                .get("deadline")
                .and_then(Value::as_u64)
                .map(|value| value.saturating_add(clock_offset)),
            allow_future_observations: value
                .get("allow_future")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            items,
        }
    }

    fn fixture_forget_request(value: &Value, clock: &Value, clock_offset: u64) -> ForgetRequest {
        ForgetRequest {
            scope: fixture_scope(&value["scope"]),
            idempotency_key: value
                .get("key")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
            receipt_window_ms: value
                .get("receipt_window")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            now_ms: clock
                .as_u64()
                .unwrap_or_default()
                .saturating_add(clock_offset),
            deadline_ms: value
                .get("deadline")
                .and_then(Value::as_u64)
                .map(|value| value.saturating_add(clock_offset)),
            targets: value["targets"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|target| ForgetTarget {
                    id: target["id"].as_str().unwrap_or_default().into(),
                    expected_revision: target["revision"].as_u64().unwrap_or_default(),
                })
                .collect(),
        }
    }

    fn fixture_evidence_ids(case: &Value) -> BTreeMap<u64, String> {
        let mut records = BTreeMap::new();
        for step in case["steps"].as_array().unwrap() {
            if step["op"] != "remember" {
                continue;
            }
            for item in expanded(&step["request"]["items"])
                .as_array()
                .into_iter()
                .flatten()
            {
                for evidence in item["evidence"].as_array().into_iter().flatten() {
                    let id = evidence["id"].as_str().unwrap().to_owned();
                    records.insert(evidence_record_id(&id), id);
                }
            }
        }
        records
    }

    fn seed_missing_fixture_evidence(
        db: &mut Database,
        request: &RememberRequest,
        evidence_ids: &BTreeMap<u64, String>,
        clock: u64,
    ) {
        let mut missing = BTreeSet::new();
        for evidence in request.items.iter().flat_map(|item| &item.evidence) {
            let record = evidence_record_id(&evidence.id);
            if db.get(record).unwrap().is_none() {
                missing.insert(record);
            }
        }
        if missing.is_empty() {
            return;
        }
        let ops = missing
            .into_iter()
            .map(|record| {
                let id = evidence_ids
                    .get(&record)
                    .expect("fixture evidence map contains every referenced record");
                GraphOp::Vector {
                    collection: "default".into(),
                    mutation: crate::Mutation::Put {
                        id: record,
                        vector: vec![1.0, 0.0],
                        payload: id.as_bytes().to_vec(),
                    },
                }
            })
            .collect::<Vec<_>>();
        db.graph_mut("memory")
            .unwrap()
            .write_batch_at(&ops, clock)
            .unwrap();
    }

    fn fixture_time_near_ceiling(value: &Value) -> bool {
        match value {
            Value::Object(object) => object.iter().any(|(key, value)| {
                (matches!(
                    key.as_str(),
                    "clock" | "deadline" | "expiry" | "observed_at"
                ) && value
                    .as_u64()
                    .is_some_and(|time| time >= i64::MAX as u64 - 120_000))
                    || fixture_time_near_ceiling(value)
            }),
            Value::Array(values) => values.iter().any(fixture_time_near_ceiling),
            _ => false,
        }
    }

    fn fixture_skip_reason(case: &Value) -> Option<&'static str> {
        if case.get("force_collision").and_then(Value::as_bool) == Some(true) {
            return Some("fixture-only forced hash collision; no injectable production hash");
        }
        for step in case["steps"].as_array().unwrap() {
            match step["op"].as_str().unwrap() {
                "forget" | "visible" | "edge" | "attach" => {}
                "remember" => {
                    let request = &step["request"];
                    let expanded_items = expanded(&request["items"]);
                    for item in expanded_items.as_array().into_iter().flatten() {
                        if item.get("importance").is_some_and(Value::is_null) {
                            return Some(
                                "explicit JSON null is not representable by typed Option<f64>",
                            );
                        }
                        if item
                            .get("content")
                            .is_some_and(|value| !value.is_string() && !is_string_repeat(value))
                        {
                            return Some(
                                "non-string JSON content is not representable by the Rust request type",
                            );
                        }
                        if item
                            .get("observed_at")
                            .is_some_and(|value| value.as_i64().is_none())
                            && item.get("observed_at").is_some()
                        {
                            return Some(
                                "out-of-range or non-integer JSON observation is not representable",
                            );
                        }
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn is_string_repeat(value: &Value) -> bool {
        value
            .as_object()
            .is_some_and(|object| object.get("repeat").is_some_and(Value::is_string))
    }

    fn fixture_error_name(error: &MemoryError) -> &'static str {
        match error {
            MemoryError::InvalidScope => "InvalidScope",
            MemoryError::InvalidKey => "InvalidKey",
            MemoryError::InvalidClock => "InvalidClock",
            MemoryError::InvalidReceiptWindow => "InvalidReceiptWindow",
            MemoryError::InvalidExpiry => "InvalidExpiry",
            MemoryError::InvalidEvidence => "InvalidEvidence",
            MemoryError::DeadlineExceeded => "DeadlineExceeded",
            MemoryError::InvalidBundle => "InvalidBundle",
            MemoryError::DuplicateItem => "DuplicateItem",
            MemoryError::OperationLimit => "OperationLimit",
            MemoryError::BodyLimit => "BodyLimit",
            MemoryError::InvalidIdentity => "InvalidIdentity",
            MemoryError::UnsupportedKind => "UnsupportedKind",
            MemoryError::InvalidContent => "InvalidContent",
            MemoryError::InvalidImportance => "InvalidImportance",
            MemoryError::InvalidObservation => "InvalidObservation",
            MemoryError::FutureObservation => "FutureObservation",
            MemoryError::InvalidProvenance => "InvalidProvenance",
            MemoryError::ProvenanceLimit => "ProvenanceLimit",
            MemoryError::InvalidReference => "InvalidReference",
            MemoryError::MissingReference => "MissingReference",
            MemoryError::CrossScope => "CrossScope",
            MemoryError::DigestCollision => "DigestCollision",
            MemoryError::IdempotencyConflict => "IdempotencyConflict",
            MemoryError::IdentityConflict => "IdentityConflict",
            MemoryError::MissingMemory => "MissingMemory",
            MemoryError::MergeIdentityConflict => "MergeIdentityConflict",
            MemoryError::ExpectedRevisionRequired => "ExpectedRevisionRequired",
            MemoryError::RevisionConflict => "RevisionConflict",
            MemoryError::RecordAlreadyAttached => "RecordAlreadyAttached",
            MemoryError::ForeignAttachment => "ForeignAttachment",
            MemoryError::InvalidTarget => "InvalidTarget",
            MemoryError::DuplicateTarget => "DuplicateTarget",
            MemoryError::Storage(_) => "Storage",
        }
    }

    fn compare_actions(result: &RememberResult, expected: &Value, case_name: &str, step: usize) {
        let actions = result
            .actions
            .iter()
            .map(|action| json!([action.action, action.id, action.revision]))
            .collect::<Vec<_>>();
        assert_eq!(
            json!(actions),
            expected["actions"],
            "{case_name} step {step} actions"
        );
    }

    fn compare_fixture_get(db: &Database, step: &Value, case_name: &str, clock_offset: u64) {
        let scope = fixture_scope(&step["scope"]);
        let key = memory_key(&scope.key(), step["id"].as_str().unwrap());
        let node = db
            .graph("memory")
            .unwrap()
            .with_clock(step["clock"].as_u64().unwrap())
            .get_node(&key)
            .unwrap()
            .output
            .unwrap_or_else(|| panic!("{case_name}: expected memory node {key}"));
        let provenance_count = decode_provenance(string_property(&node, "provenance").unwrap())
            .unwrap()
            .len();
        let mut actual = serde_json::Map::new();
        for field in step["fields"].as_array().unwrap() {
            let field = field.as_str().unwrap();
            let value = match field {
                "importance" => json!(float_property(&node, "importance").unwrap()),
                "created_at" => {
                    json!(integer_property(&node, "created_at_ms").unwrap() - clock_offset as i64)
                }
                "updated_at" => {
                    json!(integer_property(&node, "updated_at_ms").unwrap() - clock_offset as i64)
                }
                "observed_at" => {
                    json!(integer_property(&node, "observed_at_ms").unwrap() - clock_offset as i64)
                }
                "expiry" => node
                    .expires_at_ms
                    .map_or(Value::Null, |expiry| json!(expiry - clock_offset)),
                "provenance_count" => json!(provenance_count),
                "revision" => json!(
                    string_property(&node, "memory_revision")
                        .unwrap()
                        .parse::<u64>()
                        .unwrap()
                ),
                other => panic!("{case_name}: unsupported get projection {other}"),
            };
            actual.insert(field.into(), value);
        }
        assert_eq!(Value::Object(actual), step["expect"], "{case_name} get");
    }

    fn find_label(db: &Database, label: &str) -> Vec<NodeView> {
        let read = db
            .graph("memory")
            .unwrap()
            .find_nodes(
                &FindNodes {
                    label: Some(label.into()),
                    limit: Some(1_000),
                    ..FindNodes::default()
                },
                &ReadOptions {
                    limits: GraphLimits::CEILING,
                    ..ReadOptions::default()
                },
            )
            .unwrap();
        assert!(read.complete, "fixture state query must be complete");
        read.output
    }

    fn find_label_at(db: &Database, label: &str, clock: u64) -> Vec<NodeView> {
        let read = db
            .graph("memory")
            .unwrap()
            .with_clock(clock)
            .find_nodes(
                &FindNodes {
                    label: Some(label.into()),
                    limit: Some(1_000),
                    ..FindNodes::default()
                },
                &ReadOptions {
                    limits: GraphLimits::CEILING,
                    ..ReadOptions::default()
                },
            )
            .unwrap();
        assert!(read.complete, "fixture state query must be complete");
        read.output
    }

    fn compare_fixture_state(
        db: &Database,
        expected: &Value,
        case_name: &str,
        evidence_ids: &BTreeMap<u64, String>,
        managed_evidence: &BTreeSet<u64>,
        fixture_edges: &BTreeMap<String, (String, String)>,
        logical_sequence: u64,
    ) {
        let graph = db.graph("memory").unwrap().with_clock(0);
        let memories = find_label_at(db, LABEL_MEMORY, 0)
            .into_iter()
            .map(|node| string_property(&node, "id").unwrap().to_owned())
            .collect::<BTreeSet<_>>();
        let memories = memories.into_iter().collect::<Vec<_>>();
        let receipt_count = find_label_at(db, LABEL_RECEIPT, 0).len();
        let evidence = managed_evidence
            .iter()
            .filter_map(|record| {
                db.get(*record)
                    .ok()
                    .flatten()
                    .and_then(|_| evidence_ids.get(record).cloned())
            })
            .collect::<BTreeSet<_>>();
        let edges = fixture_edges
            .keys()
            .filter(|key| graph.get_edge_by_key(key).unwrap().output.is_some())
            .cloned()
            .collect::<BTreeSet<_>>();
        assert_eq!(
            json!(memories),
            expected["memories"],
            "{case_name} memories"
        );
        assert_eq!(
            json!(evidence.into_iter().collect::<Vec<_>>()),
            expected["evidence"],
            "{case_name} evidence"
        );
        assert_eq!(
            json!(edges.into_iter().collect::<Vec<_>>()),
            expected["edges"],
            "{case_name} edges"
        );
        assert_eq!(
            json!(receipt_count),
            expected["receipts"],
            "{case_name} receipts"
        );
        assert_eq!(
            json!(logical_sequence),
            expected["sequence"],
            "{case_name} logical sequence"
        );
    }

    fn compare_fixture_visible(
        db: &Database,
        step: &Value,
        case_name: &str,
        evidence_ids: &BTreeMap<u64, String>,
        fixture_edges: &BTreeMap<String, (String, String)>,
        clock_offset: u64,
    ) {
        let clock = step["clock"].as_u64().unwrap().saturating_add(clock_offset);
        let scope = fixture_scope(&step["scope"]).key();
        let graph = db.graph("memory").unwrap().with_clock(clock);
        let read = graph
            .find_nodes(
                &FindNodes {
                    label: Some(LABEL_MEMORY.into()),
                    conditions: vec![crate::Condition::Eq {
                        field: "scope".into(),
                        value: MetadataValue::String(scope),
                    }],
                    limit: Some(1_000),
                    ..FindNodes::default()
                },
                &ReadOptions {
                    limits: GraphLimits::CEILING,
                    ..ReadOptions::default()
                },
            )
            .unwrap();
        assert!(read.complete, "{case_name}: visible query must be complete");
        let nodes = read.output;
        let memory_ids = nodes
            .iter()
            .map(|node| string_property(node, "id").unwrap().to_owned())
            .collect::<BTreeSet<_>>();
        let evidence = nodes
            .iter()
            .flat_map(|node| graph.memory_node_attachments(&node.key).unwrap())
            .filter_map(|(_, record)| {
                db.get(record)
                    .ok()
                    .flatten()
                    .and_then(|_| evidence_ids.get(&record).cloned())
            })
            .collect::<BTreeSet<_>>();
        let visible_ids = nodes
            .iter()
            .map(|node| {
                (
                    string_property(node, "id").unwrap().to_owned(),
                    node.key.clone(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let edges = fixture_edges
            .iter()
            .filter(|(key, (source, destination))| {
                visible_ids.contains_key(source)
                    && visible_ids.contains_key(destination)
                    && graph.get_edge_by_key(key).unwrap().output.is_some()
            })
            .map(|(key, _)| key.clone())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            json!(memory_ids.into_iter().collect::<Vec<_>>()),
            step["expect"]["memories"],
            "{case_name} visible memories"
        );
        assert_eq!(
            json!(evidence.into_iter().collect::<Vec<_>>()),
            step["expect"]["evidence"],
            "{case_name} visible evidence"
        );
        assert_eq!(
            json!(edges.into_iter().collect::<Vec<_>>()),
            step["expect"]["edges"],
            "{case_name} visible edges"
        );
    }

    #[test]
    fn golden_storage_errors_keep_their_raw_classification() {
        assert_eq!(
            fixture_error_name(&MemoryError::Storage(Error::RevisionMismatch {
                current: Some(1)
            })),
            "Storage"
        );
        assert_eq!(
            fixture_error_name(&MemoryError::Storage(Error::GraphKeyExists)),
            "Storage"
        );
    }

    #[test]
    fn forget_rejects_tampered_scope_tuple() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("forget-scope.vlite"));
        let scope = request("m", "seed").scope;
        provision(&mut db, &scope);
        remember(&mut db, &request("m", "seed"));
        let scope_node = node(&db, &scope.key());
        let mut properties = scope_node.properties.clone();
        properties.insert("tuple".into(), MetadataValue::String("wrong-tuple".into()));
        replace_properties(&mut db, scope_node, properties);
        let key = memory_key(&scope.key(), "m");
        let revision = node(&db, &key).revision;
        let sequence = db.sequence;
        let result = MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .forget(&forget_request(scope, "erase", 10, &[("m", 1)]));
        assert!(matches!(result, Err(MemoryError::DigestCollision)));
        assert_memory_unchanged(&db, &key, sequence, Some(revision));
    }

    #[test]
    fn forget_receipt_expires_at_window_end() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("forget-expiry.vlite"));
        let seed_a = distinct_request("a", "seed");
        let scope = seed_a.scope.clone();
        provision(&mut db, &scope);
        remember(&mut db, &seed_a);
        remember(&mut db, &distinct_request("b", "seed-b"));
        let forget_a = forget_request(scope.clone(), "erase-a", 10, &[("a", 1)]);
        let first = MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .forget(&forget_a)
            .unwrap();
        assert!(!first.replayed);
        let receipt_key = forget_receipt_key(&scope.key(), "erase-a");
        let receipt = node(&db, &receipt_key);
        assert_eq!(string_property(&receipt, "expires_at_ms").unwrap(), "110");
        let mut replay_before_expiry = forget_a.clone();
        replay_before_expiry.now_ms = 109;
        assert!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .forget(&replay_before_expiry)
                .unwrap()
                .replayed
        );
        let sequence = db.sequence;
        let mut replay_at_expiry = forget_a;
        replay_at_expiry.now_ms = 110;
        let result = MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .forget(&replay_at_expiry);
        assert!(matches!(result, Err(MemoryError::MissingMemory)));
        assert_memory_unchanged(
            &db,
            &memory_key(&scope.key(), "b"),
            sequence,
            Some(node(&db, &memory_key(&scope.key(), "b")).revision),
        );

        let forget_b = forget_request(scope.clone(), "erase-b", 11, &[("b", 1)]);
        MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .forget(&forget_b)
            .unwrap();
        let mut before_boundary = forget_b.clone();
        before_boundary.now_ms = 110;
        assert!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .forget(&before_boundary)
                .unwrap()
                .replayed
        );
        let sequence = db.sequence;
        let mut at_boundary = forget_b;
        at_boundary.now_ms = 111;
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .forget(&at_boundary),
            Err(MemoryError::MissingMemory)
        ));
        assert_eq!(db.sequence, sequence);
    }

    #[test]
    fn forget_honours_deadline_ms() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("forget-deadline.vlite"));
        let seed = request("m", "seed");
        provision(&mut db, &seed.scope);
        remember(&mut db, &seed);
        let key = memory_key(&seed.scope.key(), "m");
        let revision = node(&db, &key).revision;
        let mut expired = forget_request(seed.scope.clone(), "erase", 10, &[("m", 1)]);
        expired.deadline_ms = Some(10);
        let sequence = db.sequence;
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .forget(&expired),
            Err(MemoryError::DeadlineExceeded)
        ));
        assert_memory_unchanged(&db, &key, sequence, Some(revision));
        expired.deadline_ms = Some(11);
        assert!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .forget(&expired)
                .is_ok()
        );
    }

    #[test]
    fn forget_rejects_attachment_on_memory_without_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("forget-stray.vlite"));
        let seed = request("m", "seed");
        provision(&mut db, &seed.scope);
        remember(&mut db, &seed);
        let record = seed_vector(&mut db, "stray");
        let key = memory_key(&seed.scope.key(), "m");
        db.graph_mut("memory")
            .unwrap()
            .write_batch(&[GraphOp::Attach {
                node: key.clone(),
                collection: "default".into(),
                record,
            }])
            .unwrap();
        assert_eq!(
            db.graph("memory")
                .unwrap()
                .memory_node_attachments(&key)
                .unwrap()
                .len(),
            1
        );
        let revision = node(&db, &key).revision;
        let sequence = db.sequence;
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .forget(&forget_request(seed.scope, "erase", 10, &[("m", 1)])),
            Err(MemoryError::ForeignAttachment)
        ));
        assert_memory_unchanged(&db, &key, sequence, Some(revision));
        assert!(db.get(record).unwrap().is_some());
    }

    #[test]
    fn forget_rejects_attachment_in_second_collection() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("forget-second-collection.vlite"));
        db.create_collection(
            "aux",
            crate::CollectionSpec {
                dimensions: 2,
                settings: crate::CollectionSettings::default(),
            },
        )
        .unwrap();
        let seed = request("m", "seed");
        provision(&mut db, &seed.scope);
        seed_vector(&mut db, "e");
        let mut with_evidence = seed.clone();
        with_evidence.items[0]
            .evidence
            .push(memory_evidence("e", &seed.scope));
        MemoryMut::bind(&mut db, "memory", Some("default"))
            .unwrap()
            .remember(&with_evidence)
            .unwrap();
        let key = memory_key(&seed.scope.key(), "m");
        let extra_record = 99;
        db.graph_mut("memory")
            .unwrap()
            .write_batch(&[
                GraphOp::Vector {
                    collection: "aux".into(),
                    mutation: crate::Mutation::Put {
                        id: extra_record,
                        vector: vec![0.0, 1.0],
                        payload: b"foreign".to_vec(),
                    },
                },
                GraphOp::Attach {
                    node: key.clone(),
                    collection: "aux".into(),
                    record: extra_record,
                },
            ])
            .unwrap();
        assert_eq!(
            db.graph("memory")
                .unwrap()
                .memory_node_attachments(&key)
                .unwrap()
                .len(),
            2
        );
        let revision = node(&db, &key).revision;
        let sequence = db.sequence;
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", Some("default"))
                .unwrap()
                .forget(&forget_request(seed.scope, "erase", 10, &[("m", 1)])),
            Err(MemoryError::ForeignAttachment)
        ));
        assert_memory_unchanged(&db, &key, sequence, Some(revision));
        assert!(db.get(evidence_record_id("e")).unwrap().is_some());
        assert!(
            db.collection("aux")
                .unwrap()
                .get(extra_record)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn forget_rejects_multi_evidence_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("forget-multi-evidence.vlite"));
        let seed = request("m", "seed");
        provision(&mut db, &seed.scope);
        seed_vector(&mut db, "e");
        let mut with_evidence = seed.clone();
        with_evidence.items[0]
            .evidence
            .push(memory_evidence("e", &seed.scope));
        MemoryMut::bind(&mut db, "memory", Some("default"))
            .unwrap()
            .remember(&with_evidence)
            .unwrap();
        let key = memory_key(&seed.scope.key(), "m");
        let mut memory = node(&db, &key);
        let mut properties = memory.properties.clone();
        properties.insert(
            "evidence".into(),
            MetadataValue::String(format!("{};{}", hex(b"e"), hex(b"e2"))),
        );
        replace_properties(&mut db, memory.clone(), properties);
        memory = node(&db, &key);
        let revision = memory.revision;
        let sequence = db.sequence;
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", Some("default"))
                .unwrap()
                .forget(&forget_request(seed.scope, "erase", 10, &[("m", 1)])),
            Err(MemoryError::InvalidBundle)
        ));
        assert_memory_unchanged(&db, &key, sequence, Some(revision));
        assert!(db.get(evidence_record_id("e")).unwrap().is_some());
    }

    #[test]
    fn forget_ignores_foreign_has_content_edge_into_target() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("forget-inbound-content.vlite"));
        let seed = request("m", "seed");
        provision(&mut db, &seed.scope);
        remember(&mut db, &seed);
        let key = memory_key(&seed.scope.key(), "m");
        db.graph_mut("memory")
            .unwrap()
            .write_batch(&[
                GraphOp::CreateNode {
                    key: "outside".into(),
                    labels: vec!["Outside".into()],
                    properties: Metadata::new(),
                },
                GraphOp::CreateEdge {
                    key: None,
                    source: "outside".into(),
                    relationship: "HAS_CONTENT".into(),
                    destination: key.clone(),
                    properties: Metadata::new(),
                },
            ])
            .unwrap();
        let result = MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .forget(&forget_request(seed.scope, "erase", 10, &[("m", 1)]))
            .unwrap();
        assert_eq!(result.deleted, vec!["m"]);
        assert!(
            db.graph("memory")
                .unwrap()
                .get_node(&key)
                .unwrap()
                .output
                .is_none()
        );
        assert!(
            db.graph("memory")
                .unwrap()
                .get_node("outside")
                .unwrap()
                .output
                .is_some()
        );
    }

    #[test]
    fn forget_deleted_order_matches_replay() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("forget-order.vlite"));
        let seed = distinct_request("a", "seed");
        provision(&mut db, &seed.scope);
        remember(&mut db, &seed);
        remember(&mut db, &distinct_request("b", "seed-b"));
        let forget = forget_request(seed.scope, "erase", 10, &[("b", 1), ("a", 1)]);
        let first = MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .forget(&forget)
            .unwrap();
        assert_eq!(first.deleted, vec!["b", "a"]);
        assert!(!first.replayed);
        let replay = MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .forget(&forget)
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.deleted, vec!["a", "b"]);
    }

    #[test]
    fn forget_reuses_key_after_receipt_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("forget-reuse-key.vlite"));
        let seed = distinct_request("a", "seed");
        provision(&mut db, &seed.scope);
        remember(&mut db, &seed);
        remember(&mut db, &distinct_request("b", "seed-b"));
        let first = forget_request(seed.scope.clone(), "reusable", 10, &[("a", 1)]);
        MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .forget(&first)
            .unwrap();
        let receipt_key = forget_receipt_key(&seed.scope.key(), "reusable");
        let old_receipt_revision = node(&db, &receipt_key).revision;
        let second = forget_request(seed.scope.clone(), "reusable", 110, &[("b", 1)]);
        let result = MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .forget(&second)
            .unwrap();
        assert!(!result.replayed);
        assert_eq!(result.deleted, vec!["b"]);
        let receipt = node(&db, &receipt_key);
        assert!(receipt.revision > old_receipt_revision);
        assert_ne!(
            string_property(&receipt, "digest").unwrap(),
            forget_request_digest(&first)
        );
        let replay = MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .forget(&forget_request(seed.scope, "reusable", 111, &[("b", 1)]))
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.deleted, vec!["b"]);
    }

    #[test]
    fn forget_rejects_empty_targets() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("forget-empty.vlite"));
        let scope = request("m", "seed").scope;
        provision(&mut db, &scope);
        let sequence = db.sequence;
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .forget(&forget_request(scope, "erase", 10, &[])),
            Err(MemoryError::InvalidTarget)
        ));
        assert_eq!(db.sequence, sequence);
    }

    #[test]
    fn forget_target_count_limit() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("forget-target-count.vlite"));
        let scope = request("m", "seed").scope;
        provision(&mut db, &scope);
        let too_many = (0..=MAX_OPS)
            .map(|index| ForgetTarget {
                id: format!("missing-{index}"),
                expected_revision: 1,
            })
            .collect::<Vec<_>>();
        let mut request = forget_request(scope.clone(), "too-many", 10, &[]);
        request.targets = too_many;
        let sequence = db.sequence;
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .forget(&request),
            Err(MemoryError::OperationLimit)
        ));
        assert_eq!(db.sequence, sequence);
        request.targets.pop();
        request.idempotency_key = "at-limit".into();
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .forget(&request),
            Err(MemoryError::MissingMemory)
        ));
        assert_eq!(db.sequence, sequence);
    }

    #[test]
    fn forget_rejects_invalid_target_ids() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("forget-invalid-id.vlite"));
        let scope = request("m", "seed").scope;
        provision(&mut db, &scope);
        let sequence = db.sequence;
        for id in ["", &"x".repeat(MAX_OPAQUE_BYTES + 1)] {
            assert!(matches!(
                MemoryMut::bind(&mut db, "memory", None)
                    .unwrap()
                    .forget(&forget_request(scope.clone(), "erase", 10, &[(id, 1)])),
                Err(MemoryError::InvalidTarget)
            ));
            assert_eq!(db.sequence, sequence);
        }
    }

    #[test]
    fn forget_rejects_zero_expected_revision() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("forget-zero-revision.vlite"));
        let seed = request("m", "seed");
        provision(&mut db, &seed.scope);
        remember(&mut db, &seed);
        let key = memory_key(&seed.scope.key(), "m");
        let revision = node(&db, &key).revision;
        let sequence = db.sequence;
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .forget(&forget_request(seed.scope, "erase", 10, &[("m", 0)])),
            Err(MemoryError::InvalidTarget)
        ));
        assert_memory_unchanged(&db, &key, sequence, Some(revision));
    }

    #[test]
    fn forget_rejects_duplicate_targets() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("forget-duplicate-target.vlite"));
        let seed = request("m", "seed");
        provision(&mut db, &seed.scope);
        remember(&mut db, &seed);
        let key = memory_key(&seed.scope.key(), "m");
        let revision = node(&db, &key).revision;
        let sequence = db.sequence;
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .forget(&forget_request(
                    seed.scope,
                    "erase",
                    10,
                    &[("m", 1), ("m", 1)]
                )),
            Err(MemoryError::DuplicateTarget)
        ));
        assert_memory_unchanged(&db, &key, sequence, Some(revision));
    }

    #[test]
    fn forget_replay_with_different_window_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("forget-window-conflict.vlite"));
        let seed = request("m", "seed");
        provision(&mut db, &seed.scope);
        remember(&mut db, &seed);
        let first = forget_request(seed.scope.clone(), "erase", 10, &[("m", 1)]);
        MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .forget(&first)
            .unwrap();
        let receipt_key = forget_receipt_key(&seed.scope.key(), "erase");
        let receipt_revision = node(&db, &receipt_key).revision;
        let sequence = db.sequence;
        let mut changed_window = first;
        changed_window.now_ms = 11;
        changed_window.receipt_window_ms = 200;
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .forget(&changed_window),
            Err(MemoryError::IdempotencyConflict)
        ));
        assert_memory_unchanged(&db, &receipt_key, sequence, Some(receipt_revision));
    }

    #[test]
    fn forget_multi_target_replay_deleted_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("forget-sorted-replay.vlite"));
        let seed = distinct_request("a", "seed");
        provision(&mut db, &seed.scope);
        remember(&mut db, &seed);
        remember(&mut db, &distinct_request("b", "seed-b"));
        let forget = forget_request(seed.scope, "erase", 10, &[("a", 1), ("b", 1)]);
        assert_eq!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .forget(&forget)
                .unwrap()
                .deleted,
            vec!["a", "b"]
        );
        let replay = MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .forget(&forget)
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.deleted, vec!["a", "b"]);
    }

    #[test]
    fn forget_transitively_referenced_targets_retain_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("forget-transitive-evidence.vlite"));
        let mut d2 = distinct_request("d2", "d2");
        provision(&mut db, &d2.scope);
        seed_vector(&mut db, "e1");
        d2.items[0].evidence.push(memory_evidence("e1", &d2.scope));
        MemoryMut::bind(&mut db, "memory", Some("default"))
            .unwrap()
            .remember(&d2)
            .unwrap();
        let mut d1 = distinct_request("d1", "d1");
        d1.items[0].references.push("d2".into());
        remember(&mut db, &d1);
        let mut survivor = distinct_request("s", "survivor");
        survivor.items[0].references.push("d1".into());
        remember(&mut db, &survivor);
        let result = MemoryMut::bind(&mut db, "memory", Some("default"))
            .unwrap()
            .forget(&forget_request(
                d2.scope.clone(),
                "erase-chain",
                10,
                &[("d1", 1), ("d2", 1)],
            ))
            .unwrap();
        assert_eq!(result.deleted, vec!["d1", "d2"]);
        assert!(db.get(evidence_record_id("e1")).unwrap().is_some());
        assert!(
            db.graph("memory")
                .unwrap()
                .get_node(&memory_key(&d2.scope.key(), "s"))
                .unwrap()
                .output
                .is_some()
        );
    }

    #[test]
    fn forget_replay_rejects_foreign_scope_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("forget-foreign-receipt.vlite"));
        let seed = request("m", "seed");
        provision(&mut db, &seed.scope);
        remember(&mut db, &seed);
        let forget = forget_request(seed.scope.clone(), "erase", 10, &[("m", 1)]);
        MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .forget(&forget)
            .unwrap();
        let receipt_key = forget_receipt_key(&seed.scope.key(), "erase");
        let receipt = node(&db, &receipt_key);
        let mut properties = receipt.properties.clone();
        properties.insert(
            "scope".into(),
            MetadataValue::String("foreign-scope".into()),
        );
        replace_properties(&mut db, receipt, properties);
        let revision = node(&db, &receipt_key).revision;
        let sequence = db.sequence;
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .forget(&forget),
            Err(MemoryError::DigestCollision)
        ));
        assert_memory_unchanged(&db, &receipt_key, sequence, Some(revision));
    }

    #[test]
    fn forget_write_reclaims_entities_expired_by_request_clock() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("forget-reclaims-expiry.vlite"));
        let now = crate::current_time_ms() + 30_000;
        let mut seed = distinct_request("x", "seed-x");
        seed.now_ms = now;
        seed.items[0].observed_at_ms = Some(now as i64);
        let mut y = seed.items[0].clone();
        y.id = "y".into();
        y.content = "content-y".into();
        y.expiry_ms = None;
        seed.items[0].expiry_ms = Some(now + 10);
        seed.items.push(y);
        provision(&mut db, &seed.scope);
        remember(&mut db, &seed);
        let scope = seed.scope.clone();
        MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .forget(&forget_request(
                scope.clone(),
                "erase-y",
                now + 100,
                &[("y", 1)],
            ))
            .unwrap();
        assert!(
            db.graph("memory")
                .unwrap()
                .with_clock(0)
                .get_node(&memory_key(&scope.key(), "x"))
                .unwrap()
                .output
                .is_none()
        );
    }

    #[test]
    fn forget_unprovisioned_scope() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("forget-unprovisioned.vlite"));
        let scope = request("m", "seed").scope;
        let sequence = db.sequence;
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .forget(&forget_request(scope, "erase", 10, &[("m", 1)])),
            Err(MemoryError::InvalidScope)
        ));
        assert_eq!(db.sequence, sequence);
    }

    #[test]
    fn forget_rejects_stale_expected_revision_after_merge() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("forget-stale-revision.vlite"));
        let seed = request("m", "seed");
        provision(&mut db, &seed.scope);
        remember(&mut db, &seed);
        let mut merge = request("m", "merge");
        merge.items[0].merge_into = Some("m".into());
        merge.items[0].expected_revision = Some(1);
        merge.items[0].source = "another-source".into();
        remember(&mut db, &merge);
        let key = memory_key(&seed.scope.key(), "m");
        let revision = node(&db, &key).revision;
        let sequence = db.sequence;
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .forget(&forget_request(
                    seed.scope.clone(),
                    "stale",
                    12,
                    &[("m", 1)]
                )),
            Err(MemoryError::RevisionConflict)
        ));
        assert_memory_unchanged(&db, &key, sequence, Some(revision));
        assert!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .forget(&forget_request(seed.scope, "fresh", 12, &[("m", 2)]))
                .is_ok()
        );
    }

    #[test]
    fn forget_rejects_clock_beyond_i64_max() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("forget-invalid-clock.vlite"));
        let seed = request("m", "seed");
        provision(&mut db, &seed.scope);
        remember(&mut db, &seed);
        let key = memory_key(&seed.scope.key(), "m");
        let revision = node(&db, &key).revision;
        let sequence = db.sequence;
        let mut invalid = forget_request(seed.scope, "erase", i64::MAX as u64 + 1, &[("m", 1)]);
        invalid.receipt_window_ms = 1;
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .forget(&invalid),
            Err(MemoryError::InvalidClock)
        ));
        assert_memory_unchanged(&db, &key, sequence, Some(revision));
    }

    #[test]
    fn forget_rejects_zero_receipt_window() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("forget-zero-window.vlite"));
        let seed = request("m", "seed");
        provision(&mut db, &seed.scope);
        remember(&mut db, &seed);
        let key = memory_key(&seed.scope.key(), "m");
        let revision = node(&db, &key).revision;
        let sequence = db.sequence;
        let mut invalid = forget_request(seed.scope, "erase", 10, &[("m", 1)]);
        invalid.receipt_window_ms = 0;
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .forget(&invalid),
            Err(MemoryError::InvalidReceiptWindow)
        ));
        assert_memory_unchanged(&db, &key, sequence, Some(revision));
    }

    #[test]
    fn forget_rejects_overflowing_receipt_window_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("forget-overflow-window.vlite"));
        let seed = request("m", "seed");
        provision(&mut db, &seed.scope);
        remember(&mut db, &seed);
        let key = memory_key(&seed.scope.key(), "m");
        let revision = node(&db, &key).revision;
        let sequence = db.sequence;
        let mut invalid = forget_request(seed.scope, "erase", 1, &[("m", 1)]);
        invalid.receipt_window_ms = u64::MAX;
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .forget(&invalid),
            Err(MemoryError::InvalidReceiptWindow)
        ));
        assert_memory_unchanged(&db, &key, sequence, Some(revision));
    }

    #[test]
    fn forget_rejects_invalid_idempotency_key() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("forget-invalid-key.vlite"));
        let seed = request("m", "seed");
        provision(&mut db, &seed.scope);
        remember(&mut db, &seed);
        let key = memory_key(&seed.scope.key(), "m");
        let revision = node(&db, &key).revision;
        let sequence = db.sequence;
        for invalid_key in ["", &"k".repeat(MAX_OPAQUE_BYTES + 1)] {
            assert!(matches!(
                MemoryMut::bind(&mut db, "memory", None)
                    .unwrap()
                    .forget(&forget_request(
                        seed.scope.clone(),
                        invalid_key,
                        10,
                        &[("m", 1)]
                    )),
                Err(MemoryError::InvalidKey)
            ));
            assert_memory_unchanged(&db, &key, sequence, Some(revision));
        }
    }

    #[test]
    fn expired_owner_with_reference_is_missing_memory() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("expired-reference-owner.vlite"));
        let seed = distinct_request("x", "seed");
        provision(&mut db, &seed.scope);
        remember(&mut db, &seed);
        let sequence = db.sequence;
        let mut expired = timed_request("m", "expired", 20);
        expired.scope = seed.scope.clone();
        expired.items[0].content = "content-m".into();
        expired.items[0].expiry_ms = Some(20);
        expired.items[0].references.push("x".into());
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .remember(&expired),
            Err(MemoryError::MissingMemory)
        ));
        assert_eq!(db.sequence, sequence);
        assert!(
            db.graph("memory")
                .unwrap()
                .find_nodes(
                    &FindNodes {
                        label: Some(LABEL_MEMORY.into()),
                        conditions: Vec::new(),
                        limit: None,
                        cursor: None,
                    },
                    &ReadOptions::default(),
                )
                .unwrap()
                .output
                .into_iter()
                .filter(|node| string_property(node, "scope")
                    .is_ok_and(|value| value == seed.scope.key()))
                .count()
                == 1
        );
    }

    #[test]
    fn evidence_record_id_is_pinned() {
        assert_eq!(evidence_record_id("e"), 0x3f79bb7b435b0532);
    }

    #[test]
    fn merge_adding_first_evidence_attaches_record() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("merge-first-evidence.vlite"));
        let seed = timed_request("m", "seed", 10);
        provision(&mut db, &seed.scope);
        remember(&mut db, &seed);
        seed_vector(&mut db, "e");
        let mut merge = seed.clone();
        merge.idempotency_key = "merge".into();
        merge.now_ms = 11;
        merge.items[0].observed_at_ms = Some(11);
        merge.items[0].source = "second-source".into();
        merge.items[0].merge_into = Some("m".into());
        merge.items[0].expected_revision = Some(1);
        merge.items[0]
            .evidence
            .push(memory_evidence("e", &seed.scope));
        MemoryMut::bind(&mut db, "memory", Some("default"))
            .unwrap()
            .remember(&merge)
            .unwrap();
        assert_eq!(
            db.graph("memory")
                .unwrap()
                .memory_node_attachments(&memory_key(&seed.scope.key(), "m"))
                .unwrap(),
            vec![(db.resolve("default").unwrap(), evidence_record_id("e"))]
        );
    }

    #[test]
    fn remember_rejects_non_opaque_evidence_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("invalid-evidence-id.vlite"));
        let seed = request("m", "seed");
        provision(&mut db, &seed.scope);
        let sequence = db.sequence;
        for evidence_id in ["", &"e".repeat(MAX_OPAQUE_BYTES + 1)] {
            let mut invalid = seed.clone();
            invalid.items[0]
                .evidence
                .push(memory_evidence(evidence_id, &seed.scope));
            assert!(matches!(
                MemoryMut::bind(&mut db, "memory", Some("default"))
                    .unwrap()
                    .remember(&invalid),
                Err(MemoryError::InvalidEvidence)
            ));
            assert_eq!(db.sequence, sequence);
            assert!(
                scan_memories(&db.graph("memory").unwrap(), &seed.scope.key())
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn remember_rejects_invalid_evidence_scope() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("invalid-evidence-scope.vlite"));
        let seed = request("m", "seed");
        provision(&mut db, &seed.scope);
        let mut invalid_scope = seed.scope.clone();
        invalid_scope.tenant.clear();
        let mut invalid = seed.clone();
        invalid.items[0]
            .evidence
            .push(memory_evidence("e", &invalid_scope));
        let sequence = db.sequence;
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", Some("default"))
                .unwrap()
                .remember(&invalid),
            Err(MemoryError::InvalidScope)
        ));
        assert_eq!(db.sequence, sequence);
    }

    #[test]
    fn remember_foreign_non_memory_owner_is_record_already_attached() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("foreign-non-memory-owner.vlite"));
        let seed = request("m", "seed");
        provision(&mut db, &seed.scope);
        let record = seed_vector(&mut db, "e");
        db.graph_mut("memory")
            .unwrap()
            .write_batch(&[
                GraphOp::CreateNode {
                    key: "outside-owner".into(),
                    labels: vec!["Outside".into()],
                    properties: Metadata::new(),
                },
                GraphOp::Attach {
                    node: "outside-owner".into(),
                    collection: "default".into(),
                    record,
                },
            ])
            .unwrap();
        let mut with_evidence = seed.clone();
        with_evidence.items[0]
            .evidence
            .push(memory_evidence("e", &seed.scope));
        let sequence = db.sequence;
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", Some("default"))
                .unwrap()
                .remember(&with_evidence),
            Err(MemoryError::RecordAlreadyAttached)
        ));
        assert_eq!(db.sequence, sequence);
        assert!(
            db.graph("memory")
                .unwrap()
                .get_node(&memory_key(&seed.scope.key(), "m"))
                .unwrap()
                .output
                .is_none()
        );
    }

    #[test]
    fn legacy_memory_without_evidence_property_reads_as_no_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("legacy-memory-evidence.vlite"));
        let seed = request("m", "seed");
        provision(&mut db, &seed.scope);
        remember(&mut db, &seed);
        let key = memory_key(&seed.scope.key(), "m");
        let memory = node(&db, &key);
        let mut properties = memory.properties.clone();
        properties.remove("evidence");
        replace_properties(&mut db, memory, properties);
        let scanned = scan_memories(&db.graph("memory").unwrap(), &seed.scope.key()).unwrap();
        assert!(scanned[0].evidence.is_empty());
        assert!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .remember(&seed)
                .is_ok()
        );
    }

    #[test]
    fn corrupted_memory_evidence_property_is_invalid_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("corrupt-memory-evidence.vlite"));
        let seed = request("m", "seed");
        provision(&mut db, &seed.scope);
        remember(&mut db, &seed);
        let key = memory_key(&seed.scope.key(), "m");
        let memory = node(&db, &key);
        let mut properties = memory.properties.clone();
        properties.insert("evidence".into(), MetadataValue::Integer(1));
        replace_properties(&mut db, memory, properties);
        let revision = node(&db, &key).revision;
        let sequence = db.sequence;
        let mut fresh_request = seed.clone();
        fresh_request.idempotency_key = "fresh-scan".into();
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .remember(&fresh_request),
            Err(MemoryError::InvalidBundle)
        ));
        assert_memory_unchanged(&db, &key, sequence, Some(revision));
    }

    #[test]
    fn encode_outcome_chunks_at_exactly_max_string_value_bytes() {
        for (length, expected_chunks) in
            [(MAX_STRING_VALUE_BYTES, 1), (MAX_STRING_VALUE_BYTES + 1, 2)]
        {
            let mut properties = Metadata::new();
            let ops = encode_outcome("receipt-key", &"x".repeat(length), &mut properties).unwrap();
            assert_eq!(ops.len(), expected_chunks);
            assert_eq!(
                properties.get("outcome_chunks"),
                Some(&MetadataValue::Integer(expected_chunks as i64))
            );
        }
    }

    #[test]
    fn remember_evidence_without_seeded_vector_record_is_record_already_attached() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("missing-evidence-record.vlite"));
        let seed = request("m", "seed");
        provision(&mut db, &seed.scope);
        let mut with_evidence = seed.clone();
        with_evidence.items[0]
            .evidence
            .push(memory_evidence("missing", &seed.scope));
        let sequence = db.sequence;
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", Some("default"))
                .unwrap()
                .remember(&with_evidence),
            Err(MemoryError::RecordAlreadyAttached)
        ));
        assert_eq!(db.sequence, sequence);
        assert!(
            db.graph("memory")
                .unwrap()
                .get_node(&memory_key(&seed.scope.key(), "m"))
                .unwrap()
                .output
                .is_none()
        );
    }

    #[test]
    fn replay_every_representable_write_golden() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/agent_memory/golden.json"))
                .unwrap();
        let mut replayed_cases = Vec::new();
        let mut skipped_cases = Vec::new();
        for case in fixture["cases"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            if let Some(reason) = fixture_skip_reason(case) {
                skipped_cases.push((name.to_owned(), reason));
                continue;
            }
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("memory-oracle.vlite");
            let mut db = database(&path);
            let evidence_ids = fixture_evidence_ids(case);
            let mut sequence_offset = db.sequence;
            let mut logical_sequence = 0_u64;
            let mut receipt_sequences = BTreeMap::<String, u64>::new();
            let mut receipt_revisions = BTreeMap::<String, u64>::new();
            let mut provisioned = BTreeSet::new();
            let mut managed_evidence = BTreeSet::<u64>::new();
            let mut fixture_edges = BTreeMap::new();
            let clock_offset = if fixture_time_near_ceiling(case) {
                0
            } else {
                crate::current_time_ms().saturating_add(60_000)
            };
            let mut current_clock = clock_offset;
            for (index, step) in case["steps"].as_array().unwrap().iter().enumerate() {
                if let Some(clock) = step.get("clock").and_then(Value::as_u64) {
                    current_clock = clock.saturating_add(clock_offset);
                }
                match step["op"].as_str().unwrap() {
                    "scope_pair" => {
                        let left = fixture_scope(&step["left"]);
                        let right = fixture_scope(&step["right"]);
                        let expected = &step["expect"];
                        assert_eq!(
                            left.encode() == right.encode(),
                            expected["same_encoding"].as_bool().unwrap(),
                            "{name} scope encoding"
                        );
                        assert_eq!(
                            left.key() == right.key(),
                            expected["same_key"].as_bool().unwrap(),
                            "{name} scope key"
                        );
                    }
                    "remember" => {
                        let request =
                            fixture_request(&step["request"], &step["clock"], clock_offset);
                        let receipt_identity = format!(
                            "remember:{}:{}",
                            request.scope.key(),
                            request.idempotency_key
                        );
                        let validation = validate_request(&request);
                        if validation.is_ok() {
                            let before = db.sequence;
                            seed_missing_fixture_evidence(
                                &mut db,
                                &request,
                                &evidence_ids,
                                current_clock,
                            );
                            sequence_offset += db.sequence - before;
                        }
                        if validation.is_ok() && !provisioned.contains(&request.scope.key()) {
                            let before = db.sequence;
                            MemoryMut::bind(&mut db, "memory", None)
                                .unwrap()
                                .provision_scope(&request.scope)
                                .unwrap();
                            sequence_offset += db.sequence - before;
                            provisioned.insert(request.scope.key());
                        }
                        let before = db.sequence;
                        let result = MemoryMut::bind(&mut db, "memory", Some("default"))
                            .unwrap()
                            .remember(&request);
                        let expected = &step["expect"];
                        if let Some(expected_error) = expected.get("error").and_then(Value::as_str)
                        {
                            let error = match result {
                                Err(error) => error,
                                Ok(actual) => {
                                    panic!(
                                        "{name} step {index}: expected {expected_error}, got {actual:?}"
                                    )
                                }
                            };
                            assert_eq!(
                                fixture_error_name(&error),
                                expected_error,
                                "{name} step {index}: {error:?}"
                            );
                            assert_eq!(
                                db.sequence, before,
                                "{name} step {index} rejected remember appended a frame"
                            );
                        } else {
                            let result = result
                                .unwrap_or_else(|error| panic!("{name} step {index}: {error:?}"));
                            managed_evidence.extend(
                                request
                                    .items
                                    .iter()
                                    .flat_map(|item| &item.evidence)
                                    .map(|evidence| evidence_record_id(&evidence.id)),
                            );
                            compare_actions(&result, expected, name, index);
                            assert_eq!(
                                result.commit_sequence, result.receipt_revision,
                                "{name} step {index} receipt revision"
                            );
                            if !result.replayed {
                                logical_sequence += 1;
                                assert_eq!(
                                    result.commit_sequence - sequence_offset,
                                    logical_sequence,
                                    "{name} step {index} sequence"
                                );
                                receipt_sequences
                                    .insert(receipt_identity.clone(), logical_sequence);
                                receipt_revisions
                                    .insert(receipt_identity.clone(), result.commit_sequence);
                            }
                            assert_eq!(
                                receipt_sequences.get(&receipt_identity).copied(),
                                expected["sequence"].as_u64(),
                                "{name} step {index} original logical sequence"
                            );
                            assert_eq!(
                                receipt_revisions.get(&receipt_identity).copied(),
                                Some(result.commit_sequence),
                                "{name} step {index} original receipt revision"
                            );
                        }
                    }
                    "forget" => {
                        let request =
                            fixture_forget_request(&step["request"], &step["clock"], clock_offset);
                        let receipt_identity =
                            format!("forget:{}:{}", request.scope.key(), request.idempotency_key);
                        let before = db.sequence;
                        let result = MemoryMut::bind(&mut db, "memory", Some("default"))
                            .unwrap()
                            .forget(&request);
                        let expected = &step["expect"];
                        if let Some(expected_error) = expected.get("error").and_then(Value::as_str)
                        {
                            let error = match result {
                                Err(error) => error,
                                Ok(actual) => panic!(
                                    "{name} step {index}: expected {expected_error}, got {actual:?}"
                                ),
                            };
                            assert_eq!(
                                fixture_error_name(&error),
                                expected_error,
                                "{name} step {index}: {error:?}"
                            );
                            assert_eq!(
                                db.sequence, before,
                                "{name} step {index} rejected forget appended a frame"
                            );
                        } else {
                            let result = result
                                .unwrap_or_else(|error| panic!("{name} step {index}: {error:?}"));
                            managed_evidence.retain(|record| db.get(*record).unwrap().is_some());
                            let mut deleted = result.deleted.clone();
                            deleted.sort();
                            assert_eq!(json!(deleted), expected["deleted"], "{name} step {index}");
                            assert_eq!(
                                result.commit_sequence, result.receipt_revision,
                                "{name} step {index} receipt revision"
                            );
                            if !result.replayed {
                                logical_sequence += 1;
                                assert_eq!(
                                    result.commit_sequence - sequence_offset,
                                    logical_sequence,
                                    "{name} step {index} sequence"
                                );
                                receipt_sequences
                                    .insert(receipt_identity.clone(), logical_sequence);
                                receipt_revisions
                                    .insert(receipt_identity.clone(), result.commit_sequence);
                            }
                            assert_eq!(
                                receipt_sequences.get(&receipt_identity).copied(),
                                expected["sequence"].as_u64(),
                                "{name} step {index} original logical sequence"
                            );
                            assert_eq!(
                                receipt_revisions.get(&receipt_identity).copied(),
                                Some(result.commit_sequence),
                                "{name} step {index} original receipt revision"
                            );
                        }
                    }
                    "visible" => compare_fixture_visible(
                        &db,
                        step,
                        name,
                        &evidence_ids,
                        &fixture_edges,
                        clock_offset,
                    ),
                    "get" => compare_fixture_get(&db, step, name, clock_offset),
                    "state" => compare_fixture_state(
                        &db,
                        &step["expect"],
                        name,
                        &evidence_ids,
                        &managed_evidence,
                        &fixture_edges,
                        logical_sequence,
                    ),
                    "edge" => {
                        let nodes = find_label_at(&db, LABEL_MEMORY, current_clock);
                        let endpoint = |field: &str| {
                            nodes
                                .iter()
                                .find(|node| string_property(node, "id").unwrap() == step[field])
                                .map(|node| node.key.clone())
                                .unwrap_or_else(|| {
                                    panic!("{name} step {index}: missing {field} memory")
                                })
                        };
                        let key = step["id"].as_str().unwrap().to_owned();
                        let source = endpoint("from");
                        let destination = endpoint("to");
                        let before = db.sequence;
                        db.graph_mut("memory")
                            .unwrap()
                            .write_batch_at(
                                &[GraphOp::CreateEdge {
                                    key: Some(key.clone()),
                                    source: source.clone(),
                                    relationship: "FIXTURE_EDGE".into(),
                                    destination: destination.clone(),
                                    properties: Metadata::new(),
                                }],
                                current_clock,
                            )
                            .unwrap();
                        sequence_offset += db.sequence - before;
                        fixture_edges.insert(
                            key.clone(),
                            (
                                step["from"].as_str().unwrap().into(),
                                step["to"].as_str().unwrap().into(),
                            ),
                        );
                        assert_eq!(step["expect"]["edge"], key);
                    }
                    "attach" => {
                        let evidence = step["evidence"].as_str().unwrap();
                        let record = evidence_record_id(evidence);
                        let owner = find_label_at(&db, LABEL_MEMORY, current_clock)
                            .into_iter()
                            .find(|node| {
                                decode_evidence(
                                    string_property(node, "evidence").unwrap_or_default(),
                                )
                                .is_ok_and(|ids| ids.iter().any(|id| id == evidence))
                            })
                            .unwrap_or_else(|| {
                                panic!("{name} step {index}: evidence memory owner not found")
                            });
                        let outside = format!("fixture-outside:{evidence}");
                        let before = db.sequence;
                        db.graph_mut("memory")
                            .unwrap()
                            .write_batch_at(
                                &[
                                    GraphOp::Detach {
                                        node: owner.key,
                                        collection: "default".into(),
                                    },
                                    GraphOp::CreateNode {
                                        key: outside.clone(),
                                        labels: vec!["FixtureOutsideMemory".into()],
                                        properties: Metadata::new(),
                                    },
                                    GraphOp::Attach {
                                        node: outside,
                                        collection: "default".into(),
                                        record,
                                    },
                                ],
                                current_clock,
                            )
                            .unwrap();
                        sequence_offset += db.sequence - before;
                        assert_eq!(step["expect"]["attached"], evidence);
                    }
                    unexpected => panic!("{name}: unclassified oracle operation {unexpected}"),
                }
            }
            replayed_cases.push(name.to_owned());
        }
        println!(
            "Rust memory oracle replay: {} cases; skipped {}: {}",
            replayed_cases.len(),
            skipped_cases.len(),
            skipped_cases
                .iter()
                .map(|(name, reason)| format!("{name} ({reason})"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        assert!(!replayed_cases.is_empty());
        assert_eq!(
            replayed_cases.len() + skipped_cases.len(),
            fixture["cases"].as_array().unwrap().len()
        );
    }
}
