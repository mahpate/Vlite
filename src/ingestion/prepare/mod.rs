#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use sha2::{Digest, Sha256};

const MAX_SOURCE_BYTES: usize = 1_048_576;
const MAX_CHUNKS: usize = 512;
const MAX_CHUNK_ENTITIES: usize = 256;
const MAX_CHUNK_RELATIONS: usize = 512;
// Decoded-output (1 MiB) and encoded per-observation attribute (8 KiB) accounting
// remain Rust-core gates; no byte encoding is specified for this preparation slice.
const MAX_DOCUMENT_ENTITIES: usize = 4_096;
const MAX_DOCUMENT_RELATIONS: usize = 8_192;
const MAX_DOCUMENT_MENTIONS: usize = 16_384;
const MAX_RELATION_UNION_BYTES: usize = 1_024;
const MAX_ATTRIBUTE_FIELDS: usize = 16;
const MAX_ATTRIBUTE_STRING_BYTES: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ErrorCategory {
    SourceInvalidUtf8,
    SourceTooLarge,
    SourceEmpty,
    SourceIdentity,
    ChunkSize,
    ChunkOverlap,
    ChunkStrategy,
    ChunkLimit,
    ChunkProgress,
    EntityKeyEmpty,
    EntityKeyTooLong,
    ChunkEntityLimit,
    ChunkRelationLimit,
    DocumentEntityLimit,
    DocumentRelationLimit,
    DocumentMentionLimit,
    EvidenceSpan,
    EvidenceBoundary,
    EvidenceSurface,
    LocalIdDuplicate,
    LocalReferenceInvalid,
    ConfidenceInvalid,
    AttributeFields,
    AttributeKey,
    AttributeStringLimit,
    AttributeValue,
    MentionConflict,
    RelationConflict,
    EntityKind,
    Predicate,
    EntityRules,
    RelationRules,
    EntityRuleDuplicate,
    RelationRuleDuplicate,
}

impl ErrorCategory {
    fn as_str(self) -> &'static str {
        match self {
            Self::SourceInvalidUtf8 => "source_invalid_utf8",
            Self::SourceTooLarge => "source_too_large",
            Self::SourceEmpty => "source_empty",
            Self::SourceIdentity => "source_identity",
            Self::ChunkSize => "chunk_size",
            Self::ChunkOverlap => "chunk_overlap",
            Self::ChunkStrategy => "chunk_strategy",
            Self::ChunkLimit => "chunk_limit",
            Self::ChunkProgress => "chunk_progress",
            Self::EntityKeyEmpty => "entity_key_empty",
            Self::EntityKeyTooLong => "entity_key_too_long",
            Self::ChunkEntityLimit => "chunk_entity_limit",
            Self::ChunkRelationLimit => "chunk_relation_limit",
            Self::DocumentEntityLimit => "document_entity_limit",
            Self::DocumentRelationLimit => "document_relation_limit",
            Self::DocumentMentionLimit => "document_mention_limit",
            Self::EvidenceSpan => "evidence_span",
            Self::EvidenceBoundary => "evidence_boundary",
            Self::EvidenceSurface => "entity_surface_mismatch",
            Self::LocalIdDuplicate => "local_id_duplicate",
            Self::LocalReferenceInvalid => "local_reference_invalid",
            Self::ConfidenceInvalid => "confidence_invalid",
            Self::AttributeFields => "attribute_fields",
            Self::AttributeKey => "attribute_key",
            Self::AttributeStringLimit => "attribute_string_limit",
            Self::AttributeValue => "attribute_value",
            Self::MentionConflict => "mention_conflict",
            Self::RelationConflict => "relation_conflict",
            Self::EntityKind => "entity_kind",
            Self::Predicate => "predicate",
            Self::EntityRules => "entity_rules",
            Self::RelationRules => "relation_rules",
            Self::EntityRuleDuplicate => "entity_rule_duplicate",
            Self::RelationRuleDuplicate => "relation_rule_duplicate",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PreparationError(ErrorCategory);

impl PreparationError {
    pub(crate) fn category(self) -> &'static str {
        self.0.as_str()
    }
}

type PrepResult<T> = Result<T, PreparationError>;
type LocalRelationKey = (u32, String, u32, [usize; 2], Vec<u8>);
type ResolvedRelationKey = (
    u32,
    EntityIdentity,
    Vec<u8>,
    EntityIdentity,
    [usize; 2],
    Vec<u8>,
);

fn fail<T>(category: ErrorCategory) -> PrepResult<T> {
    Err(PreparationError(category))
}

fn validate_name(value: &str, category: ErrorCategory) -> PrepResult<()> {
    if value.is_empty() || value.as_bytes().contains(&0) {
        return fail(category);
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SourceIdentity {
    pub(crate) tenant: String,
    pub(crate) source_id: String,
    pub(crate) uri: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Source {
    pub(crate) tenant: String,
    pub(crate) source_id: String,
    pub(crate) uri: Option<String>,
    pub(crate) byte_length: usize,
}

pub(crate) fn validate_source(raw: &[u8], identity: &SourceIdentity) -> PrepResult<Source> {
    let text =
        std::str::from_utf8(raw).map_err(|_| PreparationError(ErrorCategory::SourceInvalidUtf8))?;
    if raw.len() > MAX_SOURCE_BYTES {
        return fail(ErrorCategory::SourceTooLarge);
    }
    if text.is_empty() {
        return fail(ErrorCategory::SourceEmpty);
    }
    let tenant = identity.tenant.as_bytes();
    let source_id = identity.source_id.as_bytes();
    if tenant.is_empty()
        || tenant.len() > 128
        || tenant.contains(&0)
        || source_id.is_empty()
        || source_id.len() > 256
        || source_id.contains(&0)
        || identity
            .uri
            .as_ref()
            .is_some_and(|uri| uri.len() > 2_048 || uri.as_bytes().contains(&0))
    {
        return fail(ErrorCategory::SourceIdentity);
    }
    Ok(Source {
        tenant: identity.tenant.clone(),
        source_id: identity.source_id.clone(),
        uri: identity.uri.clone(),
        byte_length: raw.len(),
    })
}

pub(crate) fn validate_binding(binding: &str) -> PrepResult<()> {
    if binding.is_empty() || binding.as_bytes().contains(&0) {
        return fail(ErrorCategory::SourceIdentity);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChunkStrategy {
    FixedUtf8V1,
    ParagraphV1,
}

impl ChunkStrategy {
    pub(crate) fn parse(value: &str) -> PrepResult<Self> {
        match value {
            "FixedUtf8V1" => Ok(Self::FixedUtf8V1),
            "ParagraphV1" => Ok(Self::ParagraphV1),
            _ => fail(ErrorCategory::ChunkStrategy),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Chunk {
    pub(crate) ordinal: u32,
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) text: String,
    pub(crate) text_hex: String,
    pub(crate) sha256: String,
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}

fn digest(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub(crate) fn split_chunks(
    raw: &[u8],
    strategy: ChunkStrategy,
    max_bytes: usize,
    overlap_bytes: usize,
) -> PrepResult<Vec<Chunk>> {
    let text =
        std::str::from_utf8(raw).map_err(|_| PreparationError(ErrorCategory::SourceInvalidUtf8))?;
    if !(1_024..=16_384).contains(&max_bytes) {
        return fail(ErrorCategory::ChunkSize);
    }
    if overlap_bytes > 1_024 || overlap_bytes > max_bytes / 4 {
        return fail(ErrorCategory::ChunkOverlap);
    }
    if raw.is_empty() {
        return fail(ErrorCategory::SourceEmpty);
    }
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < raw.len() {
        let hard_limit = start.saturating_add(max_bytes).min(raw.len());
        let mut end = hard_limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        if strategy == ChunkStrategy::ParagraphV1 && end < raw.len() {
            let lower = start + max_bytes.div_ceil(2);
            if lower <= end {
                let mut selected = None;
                let mut pos = lower.saturating_sub(2);
                while pos + 2 <= end {
                    if raw[pos] == b'\n' && raw[pos + 1] == b'\n' {
                        selected = Some(pos + 2);
                    }
                    pos += 1;
                }
                if let Some(break_end) = selected {
                    end = break_end;
                }
            }
        }
        if end <= start {
            return fail(ErrorCategory::ChunkProgress);
        }
        let bytes = &raw[start..end];
        let chunk_text = text
            .get(start..end)
            .ok_or(PreparationError(ErrorCategory::ChunkProgress))?;
        chunks.push(Chunk {
            ordinal: u32::try_from(chunks.len())
                .map_err(|_| PreparationError(ErrorCategory::ChunkLimit))?,
            start,
            end,
            text: chunk_text.to_owned(),
            text_hex: hex(bytes),
            sha256: digest(bytes),
        });
        if chunks.len() > MAX_CHUNKS {
            return fail(ErrorCategory::ChunkLimit);
        }
        if end == raw.len() {
            break;
        }
        let mut next = end.saturating_sub(overlap_bytes);
        while next < raw.len() && !text.is_char_boundary(next) {
            next += 1;
        }
        if next <= start || next >= raw.len() {
            return fail(ErrorCategory::ChunkProgress);
        }
        start = next;
    }
    Ok(chunks)
}

pub(crate) fn normalize_key(key: &str) -> PrepResult<String> {
    let bytes = key.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut pending_space = false;
    for &byte in bytes {
        if matches!(byte, b' ' | b'\t' | b'\r' | b'\n') {
            if !output.is_empty() {
                pending_space = true;
            }
            continue;
        }
        if pending_space {
            output.push(b' ');
            pending_space = false;
        }
        output.push(if byte.is_ascii_uppercase() {
            byte + (b'a' - b'A')
        } else {
            byte
        });
    }
    if output.is_empty() {
        return fail(ErrorCategory::EntityKeyEmpty);
    }
    if output.len() > 256 {
        return fail(ErrorCategory::EntityKeyTooLong);
    }
    String::from_utf8(output).map_err(|_| PreparationError(ErrorCategory::EntityKeyEmpty))
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Attribute {
    Null,
    Bool(bool),
    Integer(i64),
    Unsigned(u64),
    Float(f64),
    String(String),
}

pub(crate) type Attributes = BTreeMap<String, Attribute>;

impl Attribute {
    fn append_canonical(&self, out: &mut Vec<u8>) {
        match self {
            Self::Null => out.push(0),
            Self::Bool(value) => {
                out.push(1);
                out.push(u8::from(*value));
            }
            Self::Integer(value) => {
                out.push(2);
                out.extend_from_slice(&value.to_le_bytes());
            }
            Self::Unsigned(value) => {
                out.push(3);
                out.extend_from_slice(&value.to_le_bytes());
            }
            Self::Float(value) => {
                out.push(4);
                let canonical = if *value == 0.0 { 0.0 } else { *value };
                out.extend_from_slice(&canonical.to_bits().to_le_bytes());
            }
            Self::String(value) => {
                out.push(5);
                let bytes = value.as_bytes();
                out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                out.extend_from_slice(bytes);
            }
        }
    }
}

pub(crate) fn attributes_key(attributes: &Attributes) -> Vec<u8> {
    let mut output = Vec::new();
    for (key, value) in attributes {
        let bytes = key.as_bytes();
        output.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        output.extend_from_slice(bytes);
        value.append_canonical(&mut output);
    }
    output
}

pub(crate) fn validate_attributes(attributes: Attributes) -> PrepResult<Attributes> {
    if attributes.len() > MAX_ATTRIBUTE_FIELDS {
        return fail(ErrorCategory::AttributeFields);
    }
    for (key, value) in &attributes {
        if key.is_empty() {
            return fail(ErrorCategory::AttributeKey);
        }
        if key.len() > MAX_ATTRIBUTE_STRING_BYTES {
            return fail(ErrorCategory::AttributeStringLimit);
        }
        match value {
            Attribute::String(text) if text.len() > MAX_ATTRIBUTE_STRING_BYTES => {
                return fail(ErrorCategory::AttributeStringLimit);
            }
            Attribute::Float(number) if !number.is_finite() => {
                return fail(ErrorCategory::AttributeValue);
            }
            _ => {}
        }
    }
    Ok(attributes)
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct EntityObservation {
    pub(crate) local_id: u32,
    pub(crate) kind: String,
    pub(crate) key: String,
    pub(crate) normalized_key: String,
    pub(crate) surface: String,
    pub(crate) span: [usize; 2],
    pub(crate) attributes: Attributes,
    pub(crate) confidence: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RelationObservation {
    pub(crate) subject_local_id: u32,
    pub(crate) predicate: String,
    pub(crate) object_local_id: u32,
    pub(crate) span: [usize; 2],
    pub(crate) attributes: Attributes,
    pub(crate) confidence: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Extraction {
    pub(crate) entities: Vec<EntityObservation>,
    pub(crate) relations: Vec<RelationObservation>,
}

fn validate_confidence(confidence: Option<f64>) -> PrepResult<Option<f64>> {
    if confidence.is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value)) {
        return fail(ErrorCategory::ConfidenceInvalid);
    }
    Ok(confidence.map(|value| if value == 0.0 { 0.0 } else { value }))
}

fn validate_span(span: [usize; 2], text: &str) -> PrepResult<()> {
    let [start, end] = span;
    if start >= end || end > text.len() {
        return fail(ErrorCategory::EvidenceSpan);
    }
    if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
        return fail(ErrorCategory::EvidenceBoundary);
    }
    Ok(())
}

pub(crate) fn validate_extraction(
    chunk_text: &str,
    entities: Vec<EntityObservation>,
    relations: Vec<RelationObservation>,
) -> PrepResult<Extraction> {
    if entities.len() > MAX_CHUNK_ENTITIES {
        return fail(ErrorCategory::ChunkEntityLimit);
    }
    let mut ids = BTreeSet::new();
    let mut validated_entities = Vec::with_capacity(entities.len());
    for mut entity in entities {
        validate_name(&entity.kind, ErrorCategory::EntityKind)?;
        if !ids.insert(entity.local_id) {
            return fail(ErrorCategory::LocalIdDuplicate);
        }
        validate_span(entity.span, chunk_text)?;
        if chunk_text.as_bytes().get(entity.span[0]..entity.span[1])
            != Some(entity.surface.as_bytes())
        {
            return fail(ErrorCategory::EvidenceSurface);
        }
        entity.normalized_key = normalize_key(&entity.key)?;
        entity.attributes = validate_attributes(entity.attributes)?;
        entity.confidence = validate_confidence(entity.confidence)?;
        validated_entities.push(entity);
    }
    let mut local_relations: Vec<RelationObservation> = Vec::new();
    let mut relation_indexes: BTreeMap<LocalRelationKey, usize> = BTreeMap::new();
    for mut relation in relations {
        validate_name(&relation.predicate, ErrorCategory::Predicate)?;
        if !ids.contains(&relation.subject_local_id) || !ids.contains(&relation.object_local_id) {
            return fail(ErrorCategory::LocalReferenceInvalid);
        }
        validate_span(relation.span, chunk_text)?;
        relation.attributes = validate_attributes(relation.attributes)?;
        relation.confidence = validate_confidence(relation.confidence)?;
        let key = (
            relation.subject_local_id,
            relation.predicate.clone(),
            relation.object_local_id,
            relation.span,
            attributes_key(&relation.attributes),
        );
        if let Some(index) = relation_indexes.get(&key) {
            if local_relations[*index].confidence != relation.confidence {
                return fail(ErrorCategory::RelationConflict);
            }
        } else {
            relation_indexes.insert(key, local_relations.len());
            local_relations.push(relation);
        }
    }
    let validated_relations = local_relations;
    if validated_relations.len() > MAX_CHUNK_RELATIONS {
        return fail(ErrorCategory::ChunkRelationLimit);
    }
    Ok(Extraction {
        entities: validated_entities,
        relations: validated_relations,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EntityRule {
    pub(crate) rule_id: String,
    pub(crate) literal: String,
    pub(crate) kind: String,
    pub(crate) key: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RelationRule {
    pub(crate) rule_id: String,
    pub(crate) predicate: String,
    pub(crate) subject_kind: String,
    pub(crate) object_kind: String,
}

#[derive(Default)]
struct TrieNode {
    next: BTreeMap<u8, usize>,
    failure: usize,
    output: Vec<usize>,
}

pub(crate) fn dictionary_extract(
    text: &str,
    entity_rules: &[EntityRule],
    relation_rules: &[RelationRule],
) -> PrepResult<Extraction> {
    let mut entity_ids = BTreeSet::new();
    for rule in entity_rules {
        validate_name(&rule.rule_id, ErrorCategory::EntityRules)?;
        validate_name(&rule.kind, ErrorCategory::EntityRules)?;
        validate_name(&rule.key, ErrorCategory::EntityRules)?;
        if rule.literal.is_empty() {
            return fail(ErrorCategory::EntityRules);
        }
        normalize_key(&rule.key)?;
        if !entity_ids.insert(&rule.rule_id) {
            return fail(ErrorCategory::EntityRuleDuplicate);
        }
    }
    let mut relation_ids = BTreeSet::new();
    for rule in relation_rules {
        validate_name(&rule.rule_id, ErrorCategory::RelationRules)?;
        validate_name(&rule.predicate, ErrorCategory::RelationRules)?;
        validate_name(&rule.subject_kind, ErrorCategory::RelationRules)?;
        validate_name(&rule.object_kind, ErrorCategory::RelationRules)?;
        if !relation_ids.insert(&rule.rule_id) {
            return fail(ErrorCategory::RelationRuleDuplicate);
        }
    }
    let mut trie = vec![TrieNode::default()];
    for (index, rule) in entity_rules.iter().enumerate() {
        let mut state = 0usize;
        for &byte in rule.literal.as_bytes() {
            let next = if let Some(next) = trie[state].next.get(&byte) {
                *next
            } else {
                let next = trie.len();
                trie.push(TrieNode::default());
                trie[state].next.insert(byte, next);
                next
            };
            state = next;
        }
        trie[state].output.push(index);
    }
    let mut queue = VecDeque::new();
    let root_children: Vec<usize> = trie[0].next.values().copied().collect();
    for child in root_children {
        queue.push_back(child);
    }
    while let Some(parent) = queue.pop_front() {
        let children: Vec<(u8, usize)> = trie[parent]
            .next
            .iter()
            .map(|(byte, child)| (*byte, *child))
            .collect();
        for (byte, child) in children {
            let mut fallback = trie[parent].failure;
            while fallback != 0 && !trie[fallback].next.contains_key(&byte) {
                fallback = trie[fallback].failure;
            }
            if let Some(next) = trie[fallback].next.get(&byte)
                && *next != child
            {
                trie[child].failure = *next;
            }
            let inherited = trie[trie[child].failure].output.clone();
            trie[child].output.extend(inherited);
            queue.push_back(child);
        }
    }
    let mut candidates = Vec::new();
    let mut state = 0usize;
    for (byte_index, &byte) in text.as_bytes().iter().enumerate() {
        while state != 0 && !trie[state].next.contains_key(&byte) {
            state = trie[state].failure;
        }
        state = trie[state].next.get(&byte).copied().unwrap_or(0);
        let end = byte_index + 1;
        if !text.is_char_boundary(end) {
            continue;
        }
        for &rule_index in &trie[state].output {
            let rule = &entity_rules[rule_index];
            let start = end - rule.literal.len();
            if text.is_char_boundary(start) {
                candidates.push((start, end, rule_index));
                if candidates.len() > MAX_CHUNK_ENTITIES {
                    return fail(ErrorCategory::ChunkEntityLimit);
                }
            }
        }
    }
    candidates.sort_by(|left, right| {
        left.0.cmp(&right.0).then_with(|| {
            entity_rules[left.2]
                .rule_id
                .as_bytes()
                .cmp(entity_rules[right.2].rule_id.as_bytes())
        })
    });
    let mut entities = Vec::with_capacity(candidates.len());
    for (local_id, (start, end, rule_index)) in candidates.into_iter().enumerate() {
        let rule = &entity_rules[rule_index];
        entities.push(EntityObservation {
            local_id: u32::try_from(local_id)
                .map_err(|_| PreparationError(ErrorCategory::ChunkEntityLimit))?,
            kind: rule.kind.clone(),
            key: rule.key.clone(),
            normalized_key: String::new(),
            surface: text[start..end].to_owned(),
            span: [start, end],
            attributes: Attributes::new(),
            confidence: None,
        });
    }
    let mut line_at = vec![0u32; text.len() + 1];
    let mut line = 0u32;
    for (index, &byte) in text.as_bytes().iter().enumerate() {
        line_at[index] = line;
        if byte == b'\n' {
            line = line.saturating_add(1);
        }
    }
    line_at[text.len()] = line;
    let mut distinct = Vec::new();
    let mut relation_indexes = BTreeSet::new();
    for rule in relation_rules {
        for subject in &entities {
            if subject.kind != rule.subject_kind || subject.surface.as_bytes().contains(&b'\n') {
                continue;
            }
            for object in &entities {
                if subject.local_id == object.local_id
                    || object.kind != rule.object_kind
                    || object.surface.as_bytes().contains(&b'\n')
                    || line_at[subject.span[0]] != line_at[object.span[0]]
                {
                    continue;
                }
                let span = [
                    subject.span[0].min(object.span[0]),
                    subject.span[1].max(object.span[1]),
                ];
                if span[1] - span[0] > MAX_RELATION_UNION_BYTES {
                    continue;
                }
                let relation = RelationObservation {
                    subject_local_id: subject.local_id,
                    predicate: rule.predicate.clone(),
                    object_local_id: object.local_id,
                    span,
                    attributes: Attributes::new(),
                    confidence: None,
                };
                let key = (
                    relation.subject_local_id,
                    relation.predicate.as_bytes().to_vec(),
                    relation.object_local_id,
                    relation.span,
                );
                if relation_indexes.insert(key) {
                    distinct.push(relation);
                }
                if distinct.len() > MAX_CHUNK_RELATIONS {
                    return fail(ErrorCategory::ChunkRelationLimit);
                }
            }
        }
    }
    distinct.sort_by(|left, right| {
        left.span
            .cmp(&right.span)
            .then_with(|| left.subject_local_id.cmp(&right.subject_local_id))
            .then_with(|| left.predicate.as_bytes().cmp(right.predicate.as_bytes()))
            .then_with(|| left.object_local_id.cmp(&right.object_local_id))
    });
    let relations = distinct;
    Ok(Extraction {
        entities,
        relations,
    })
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct EntityIdentity {
    pub(crate) binding: String,
    pub(crate) tenant: String,
    pub(crate) kind: String,
    pub(crate) normalized_key: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ResolvedMention {
    pub(crate) chunk: u32,
    pub(crate) entity: EntityIdentity,
    pub(crate) source_span: [usize; 2],
    pub(crate) chunk_span: [usize; 2],
    pub(crate) surface: String,
    pub(crate) attributes: Attributes,
    pub(crate) confidence: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ResolvedRelation {
    pub(crate) chunk: u32,
    pub(crate) subject: EntityIdentity,
    pub(crate) predicate: String,
    pub(crate) object: EntityIdentity,
    pub(crate) source_span: [usize; 2],
    pub(crate) chunk_span: [usize; 2],
    pub(crate) attributes: Attributes,
    pub(crate) confidence: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Resolution {
    pub(crate) entities: Vec<EntityIdentity>,
    pub(crate) mentions: Vec<ResolvedMention>,
    pub(crate) relations: Vec<ResolvedRelation>,
}

fn source_span(start: usize, span: [usize; 2]) -> PrepResult<[usize; 2]> {
    Ok([
        start
            .checked_add(span[0])
            .ok_or(PreparationError(ErrorCategory::EvidenceSpan))?,
        start
            .checked_add(span[1])
            .ok_or(PreparationError(ErrorCategory::EvidenceSpan))?,
    ])
}

pub(crate) fn resolve(
    binding: &str,
    tenant: &str,
    chunks: &[Chunk],
    extractions: &[Extraction],
) -> PrepResult<Resolution> {
    if chunks.len() != extractions.len() {
        return fail(ErrorCategory::LocalReferenceInvalid);
    }
    let mut anchors = BTreeSet::new();
    let mut mention_map: BTreeMap<(u32, [usize; 2], EntityIdentity), ResolvedMention> =
        BTreeMap::new();
    let mut relation_map: BTreeMap<ResolvedRelationKey, ResolvedRelation> = BTreeMap::new();
    for (chunk, extraction) in chunks.iter().zip(extractions) {
        let mut by_local_id = BTreeMap::new();
        for observation in &extraction.entities {
            let identity = EntityIdentity {
                binding: binding.to_owned(),
                tenant: tenant.to_owned(),
                kind: observation.kind.clone(),
                normalized_key: observation.normalized_key.clone(),
            };
            anchors.insert(identity.clone());
            by_local_id.insert(observation.local_id, identity.clone());
            let source_span = source_span(chunk.start, observation.span)?;
            let mention = ResolvedMention {
                chunk: chunk.ordinal,
                entity: identity.clone(),
                source_span,
                chunk_span: observation.span,
                surface: observation.surface.clone(),
                attributes: observation.attributes.clone(),
                confidence: observation.confidence,
            };
            let key = (chunk.ordinal, source_span, identity);
            if let Some(previous) = mention_map.get(&key) {
                if previous != &mention {
                    return fail(ErrorCategory::MentionConflict);
                }
            } else {
                mention_map.insert(key, mention);
            }
        }
        for observation in &extraction.relations {
            let subject = by_local_id
                .get(&observation.subject_local_id)
                .ok_or(PreparationError(ErrorCategory::LocalReferenceInvalid))?
                .clone();
            let object = by_local_id
                .get(&observation.object_local_id)
                .ok_or(PreparationError(ErrorCategory::LocalReferenceInvalid))?
                .clone();
            let source_span = source_span(chunk.start, observation.span)?;
            let relation = ResolvedRelation {
                chunk: chunk.ordinal,
                subject: subject.clone(),
                predicate: observation.predicate.clone(),
                object: object.clone(),
                source_span,
                chunk_span: observation.span,
                attributes: observation.attributes.clone(),
                confidence: observation.confidence,
            };
            let key = (
                chunk.ordinal,
                subject,
                observation.predicate.as_bytes().to_vec(),
                object,
                source_span,
                attributes_key(&observation.attributes),
            );
            if let Some(previous) = relation_map.get(&key) {
                if previous != &relation {
                    return fail(ErrorCategory::RelationConflict);
                }
            } else {
                relation_map.insert(key, relation);
            }
        }
    }
    if anchors.len() > MAX_DOCUMENT_ENTITIES {
        return fail(ErrorCategory::DocumentEntityLimit);
    }
    if mention_map.len() > MAX_DOCUMENT_MENTIONS {
        return fail(ErrorCategory::DocumentMentionLimit);
    }
    if relation_map.len() > MAX_DOCUMENT_RELATIONS {
        return fail(ErrorCategory::DocumentRelationLimit);
    }
    let mut mentions: Vec<_> = mention_map.into_values().collect();
    mentions.sort_by(|left, right| {
        left.chunk
            .cmp(&right.chunk)
            .then_with(|| left.source_span.cmp(&right.source_span))
            .then_with(|| left.entity.cmp(&right.entity))
    });
    let mut relations: Vec<_> = relation_map.into_values().collect();
    relations.sort_by(|left, right| {
        left.chunk
            .cmp(&right.chunk)
            .then_with(|| left.source_span.cmp(&right.source_span))
            .then_with(|| left.subject.cmp(&right.subject))
            .then_with(|| left.predicate.as_bytes().cmp(right.predicate.as_bytes()))
            .then_with(|| left.object.cmp(&right.object))
            .then_with(|| attributes_key(&left.attributes).cmp(&attributes_key(&right.attributes)))
    });
    Ok(Resolution {
        entities: anchors.into_iter().collect(),
        mentions,
        relations,
    })
}
