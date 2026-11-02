//! Crate-private, provisional agent-memory write core.
//!
//! Receipts and memory nodes are graph nodes in one `GraphMut::write_batch`.
//! The graph batch is the transaction boundary; no KV write or second frame
//! participates in a remember operation.

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};

use crate::{
    Database, Error, Expected, Metadata, MetadataValue, Result as VliteResult,
    property_graph::{FindNodes, GraphLimits, GraphOp, GraphRef, NodeView, ReadOptions},
};

const LABEL_MEMORY: &str = "VliteMemory";
const LABEL_RECEIPT: &str = "VliteMemoryReceipt";
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
    Storage(Error),
}

impl From<Error> for MemoryError {
    fn from(error: Error) -> Self {
        match error {
            Error::BatchTooLarge => Self::BodyLimit,
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
            .get_node(&scope_key)?
            .output
            .ok_or(MemoryError::InvalidScope)?;
        if string_property(&scope_node, "tuple")? != hex(&request.scope.encode()) {
            return Err(MemoryError::DigestCollision);
        }
        let receipt_key = receipt_key(&scope_key, &request.idempotency_key);
        let digest = request_digest(request, &normalized);
        let receipt_read = self.db.graph(&self.graph)?.get_node(&receipt_key)?;
        if let Some(receipt) = receipt_read.output {
            verify_scope(&receipt, &scope_key)?;
            let stored_digest = string_property(&receipt, "digest")?;
            if stored_digest != digest {
                return Err(MemoryError::IdempotencyConflict);
            }
            let outcome = decode_outcome(&receipt)?;
            return decode_result(&outcome, receipt.revision, true, &normalized);
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

        let read = self.db.graph(&self.graph)?;
        let mut existing = scan_memories(&read, &scope_key)?;
        let planned = plan(request, &normalized, &scope_key, &mut existing)?;
        if planned.budget_operations + 1 > MAX_OPS {
            return Err(MemoryError::OperationLimit);
        }
        if planned.encoded_request_bytes > MAX_BODY_BYTES {
            return Err(MemoryError::BodyLimit);
        }
        let outcome = encode_actions(&planned.actions);
        let mut receipt_properties = Metadata::new();
        receipt_properties.insert("scope".into(), MetadataValue::String(scope_key));
        receipt_properties.insert("digest".into(), MetadataValue::String(digest));
        encode_outcome(&outcome, &mut receipt_properties)?;
        let mut ops = planned.ops;
        ops.push(GraphOp::CreateNode {
            key: receipt_key,
            labels: vec![LABEL_RECEIPT.into()],
            properties: receipt_properties,
        });
        if ops.len() > MAX_OPS {
            return Err(MemoryError::OperationLimit);
        }

        let batch = self.db.graph_mut(&self.graph)?.write_batch(&ops)?;
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
    let mut reference_bases = BTreeMap::new();
    let mut revision_guards = 0usize;
    for item in items {
        let digest = content_digest(&item.content);
        let mut target = if let Some(target_id) = item.merge_into.as_deref() {
            Some(
                existing
                    .iter()
                    .find(|memory| memory.scope == scope_key && memory.id == target_id)
                    .cloned()
                    .ok_or(MemoryError::MissingMemory)?,
            )
        } else {
            existing
                .iter()
                .find(|memory| {
                    memory.scope == scope_key
                        && memory.kind == item.kind.label()
                        && memory.digest == digest
                        && memory.sources.iter().any(|source| source == &item.source)
                })
                .cloned()
        };
        if let Some(memory) = target.take() {
            if item.merge_into.is_some() {
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
                let properties = memory_properties(MemoryProperties {
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
                });
                ops.push(GraphOp::UpsertNode {
                    key: memory.key.clone(),
                    labels: vec![LABEL_MEMORY.into()],
                    properties,
                    expected: Expected::Revision(memory.graph_revision),
                });
                revision_guards += 1;
                actions.push(MemoryAction {
                    action: "merged",
                    id: memory.id,
                    revision: next_revision,
                });
                canonical_ids.insert(item.id.as_str(), memory.key);
                merged_ids.insert(item.id.as_str());
                reference_bases.insert(item.id.as_str(), memory.reference_count);
                continue;
            }
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
        });
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
        actions.push(MemoryAction {
            action: "created",
            id: item.id.clone(),
            revision: 1,
        });
        created_ids.insert(item.id.as_str());
        canonical_ids.insert(item.id.as_str(), key.clone());
        existing.push(ExistingMemory {
            key,
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
        });
    }
    for item in items {
        let is_new = created_ids.contains(item.id.as_str());
        let is_merged = merged_ids.contains(item.id.as_str());
        if !is_new && !is_merged {
            continue;
        }
        let source_key = canonical_ids
            .get(item.id.as_str())
            .ok_or(MemoryError::IdentityConflict)?
            .clone();
        let first_reference = reference_bases
            .get(item.id.as_str())
            .copied()
            .unwrap_or_default();
        for (reference_index, reference) in item.references.iter().enumerate() {
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
        budget_operations: ops.len() + revision_guards,
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
    properties
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
    let mut input = b"remember:v1\0".to_vec();
    input.extend_from_slice(idempotency_key.as_bytes());
    format!("receipt:v1:{scope}:{}", hex(&Sha256::digest(input)))
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
        output.push_str(",\"evidence\":[],\"expiry\":null,\"id\":");
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

fn encode_outcome(outcome: &str, properties: &mut Metadata) -> MemoryResult<()> {
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
    for (index, chunk) in chunks.iter().enumerate() {
        properties.insert(
            format!("outcome_{index:04}"),
            MetadataValue::String(
                std::str::from_utf8(chunk)
                    .map_err(|_| MemoryError::InvalidBundle)?
                    .to_owned(),
            ),
        );
    }
    Ok(())
}

fn decode_outcome(node: &NodeView) -> MemoryResult<String> {
    let count = usize::try_from(integer_property(node, "outcome_chunks")?)
        .map_err(|_| MemoryError::InvalidBundle)?;
    if count == 0 || count > MAX_BODY_BYTES.div_ceil(MAX_STRING_VALUE_BYTES) {
        return Err(MemoryError::InvalidBundle);
    }
    let mut outcome = String::new();
    for index in 0..count {
        outcome.push_str(string_property(node, &format!("outcome_{index:04}"))?);
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
    use crate::property_graph::{FindNodes, GraphLimits, ReadOptions};
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
            }],
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
        let mut items = Vec::new();
        for index in 0..10 {
            let target_id = format!("target-{index}-{}", "x".repeat(240));
            let mut seeded = request(&target_id, &format!("seed-{index}"));
            seeded.items[0].content = format!("content-{index}");
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .remember(&seeded)
                .unwrap();

            let mut alias = seeded.items[0].clone();
            alias.id = format!("alias-{index}-{}", "y".repeat(240));
            items.push(alias);
        }
        let retry = RememberRequest {
            idempotency_key: "large-outcome".into(),
            items,
            ..first
        };
        let result = MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .remember(&retry)
            .unwrap();
        assert!(result.actions.iter().all(|action| action.action == "noop"));
        let receipt = db
            .graph("memory")
            .unwrap()
            .get_node(&receipt_key(&retry.scope.key(), "large-outcome"))
            .unwrap()
            .output
            .unwrap();
        assert!(integer_property(&receipt, "outcome_chunks").unwrap() > 1);
        let replay = MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .remember(&retry)
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.actions, result.actions);
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
        let mut request = request("m1", "atomic");
        provision(&mut db, &request.scope);
        request.items.push(MemoryItem {
            id: "m2".into(),
            kind: MemoryKind::Episodic,
            content: "later".into(),
            source: "source-2".into(),
            importance: Some(0.5),
            observed_at_ms: Some(10),
            provenance: Vec::new(),
            references: vec!["missing".into()],
            merge_into: None,
            expected_revision: None,
        });
        drop(db);
        let before = fs::read(&path).unwrap();
        let mut db = Database::open(&path).unwrap();
        assert!(matches!(
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .remember(&request),
            Err(MemoryError::MissingReference)
        ));
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
        drop(db);
        assert_eq!(fs::read(&path).unwrap(), before);
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

    fn fixture_request(value: &Value, clock: &Value) -> RememberRequest {
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
                    content: item
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                    source: item
                        .get("source")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                    importance: item.get("importance").and_then(Value::as_f64),
                    observed_at_ms: item.get("observed_at").and_then(Value::as_i64),
                    provenance,
                    references,
                    merge_into: item
                        .get("merge_into")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    expected_revision: item.get("expected_revision").and_then(Value::as_u64),
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
            now_ms: clock.as_u64().unwrap_or_default(),
            deadline_ms: value.get("deadline").and_then(Value::as_u64),
            allow_future_observations: value
                .get("allow_future")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            items,
        }
    }

    fn fixture_skip_reason(case: &Value) -> Option<&'static str> {
        if case.get("force_collision").and_then(Value::as_bool) == Some(true) {
            return Some("fixture-only forced hash collision; no injectable production hash");
        }
        let mut receipt_deadlines = BTreeMap::new();
        for step in case["steps"].as_array().unwrap() {
            match step["op"].as_str().unwrap() {
                "forget" => return Some("A2 forget is outside slice A1"),
                "visible" => return Some("expiry visibility is outside slice A1"),
                "edge" => return Some("fixture-seeded ordinary graph edges are outside A1"),
                "attach" => {
                    return Some("fixture-seeded external attachment is not a Rust memory API");
                }
                "remember" => {
                    let request = &step["request"];
                    let key = request
                        .get("key")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let clock = step["clock"].as_u64().unwrap_or_default();
                    if receipt_deadlines
                        .get(key)
                        .is_some_and(|deadline| clock >= *deadline)
                    {
                        return Some("receipt retention expiry is outside this A1 candidate");
                    }
                    let window = request
                        .get("receipt_window")
                        .and_then(Value::as_u64)
                        .unwrap_or_default();
                    receipt_deadlines
                        .entry(key.to_owned())
                        .or_insert_with(|| clock.saturating_add(window));
                    let expanded_items = expanded(&request["items"]);
                    for item in expanded_items.as_array().into_iter().flatten() {
                        if item.get("expiry").is_some_and(|value| !value.is_null()) {
                            return Some("A2 expiry writes are outside slice A1");
                        }
                        if item
                            .get("evidence")
                            .and_then(Value::as_array)
                            .is_some_and(|values| !values.is_empty())
                        {
                            return Some(
                                "fixture evidence IDs are opaque and lack seeded vector records",
                            );
                        }
                        if item.get("importance").is_some_and(Value::is_null) {
                            return Some(
                                "explicit JSON null is not representable by typed Option<f64>",
                            );
                        }
                        if item.get("content").is_some_and(|value| !value.is_string()) {
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

    fn fixture_error_name(error: &MemoryError) -> &'static str {
        match error {
            MemoryError::InvalidScope => "InvalidScope",
            MemoryError::InvalidKey => "InvalidKey",
            MemoryError::InvalidClock => "InvalidClock",
            MemoryError::InvalidReceiptWindow => "InvalidReceiptWindow",
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
            MemoryError::Storage(Error::RevisionMismatch { .. }) => "RevisionConflict",
            MemoryError::Storage(Error::GraphKeyExists) => "IdentityConflict",
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

    fn compare_fixture_get(db: &Database, step: &Value, case_name: &str) {
        let scope = fixture_scope(&step["scope"]);
        let key = memory_key(&scope.key(), step["id"].as_str().unwrap());
        let node = db
            .graph("memory")
            .unwrap()
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
                "created_at" => json!(integer_property(&node, "created_at_ms").unwrap()),
                "updated_at" => json!(integer_property(&node, "updated_at_ms").unwrap()),
                "observed_at" => json!(integer_property(&node, "observed_at_ms").unwrap()),
                "expiry" => Value::Null,
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

    fn compare_fixture_state(db: &Database, expected: &Value, case_name: &str) {
        let memories = find_label(db, LABEL_MEMORY)
            .into_iter()
            .map(|node| string_property(&node, "id").unwrap().to_owned())
            .collect::<BTreeSet<_>>();
        let memories = memories.into_iter().collect::<Vec<_>>();
        let receipt_count = find_label(db, LABEL_RECEIPT).len();
        assert_eq!(
            json!(memories),
            expected["memories"],
            "{case_name} memories"
        );
        assert_eq!(json!([]), expected["evidence"], "{case_name} evidence");
        assert_eq!(json!([]), expected["edges"], "{case_name} edges");
        assert_eq!(
            json!(receipt_count),
            expected["receipts"],
            "{case_name} receipts"
        );
        assert_eq!(
            json!(receipt_count),
            expected["sequence"],
            "{case_name} logical sequence"
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
            let mut provisioned = BTreeSet::new();
            let mut provision_frames = 0_u64;
            for (index, step) in case["steps"].as_array().unwrap().iter().enumerate() {
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
                        let request = fixture_request(&step["request"], &step["clock"]);
                        let validation = validate_request(&request);
                        if validation.is_ok() && !provisioned.contains(&request.scope.key()) {
                            let before = db.sequence;
                            MemoryMut::bind(&mut db, "memory", None)
                                .unwrap()
                                .provision_scope(&request.scope)
                                .unwrap();
                            provision_frames += db.sequence - before;
                            provisioned.insert(request.scope.key());
                        }
                        let result = MemoryMut::bind(&mut db, "memory", None)
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
                        } else {
                            let result = result
                                .unwrap_or_else(|error| panic!("{name} step {index}: {error:?}"));
                            compare_actions(&result, expected, name, index);
                            assert_eq!(
                                result.commit_sequence - 1 - provision_frames,
                                expected["sequence"].as_u64().unwrap(),
                                "{name} step {index} sequence"
                            );
                            assert_eq!(
                                result.commit_sequence, result.receipt_revision,
                                "{name} step {index} receipt revision"
                            );
                        }
                    }
                    "get" => compare_fixture_get(&db, step, name),
                    "state" => compare_fixture_state(&db, &step["expect"], name),
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
