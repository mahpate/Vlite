//! Private S1 stale-summary status: a bounded, read-only status of one C1
//! summary at a captured clock C. Contract:
//! `docs/projects/roadmap/gap1-c2-remainder-contract.md` section 6 S1 and
//! decisions 2-3. Staleness is derived at read time from the retained
//! DERIVED_FROM edges, source memory revisions and TTL at C; nothing is
//! written, repaired, cascaded or persisted. Crate-private only.

use std::{collections::BTreeMap, sync::atomic::AtomicBool};

use sha2::{Digest, Sha256};

use super::recall::{RecallError, RecallRequest};
use super::{
    LABEL_MEMORY, MemoryError, MemoryRef, MemoryResult, MemoryScope, existing_memory, hex,
    memory_key, opaque, string_property,
};
use crate::{
    MetadataValue,
    property_graph::{
        Direction, GraphLimits, GraphRef, Neighbors, NodeSelector, NodeView, ReadOptions,
        TruncationReason,
    },
};

const SCHEMA: &str = "consolidate:v1";
const DERIVED_FROM: &str = "DERIVED_FROM";
const EDGE_DOMAIN: &[u8] = b"vlite.memory.consolidate.edge.v1\0";
/// C1's source cap and the most DERIVED_FROM edges one status examines.
const MAX_EDGES: u64 = 256;

/// Exactly the C1 summary property names, besides the optional `content`.
const SUMMARY_FIELDS: &[&str] = &[
    "scope",
    "id",
    "kind",
    "content_digest",
    "sources",
    "provenance",
    "importance",
    "observed_at_ms",
    "created_at_ms",
    "updated_at_ms",
    "memory_revision",
    "reference_count",
    "evidence",
    "c1_schema",
    "c1_transformation",
    "c1_source_digest",
    "c1_source_count",
];

#[derive(Clone, Debug)]
pub(crate) struct SummaryStatusRequest {
    pub scope: MemoryScope,
    pub summary_id: String,
    /// The captured clock C; TTL visibility is decided at C only.
    pub now_ms: u64,
}

/// `source_count` is the summary's `c1_source_count`. Of the retained edges,
/// `live_sources` reach a source visible at C (`changed_sources` of those
/// carry a different memory revision) and `expired_sources` one hidden by
/// TTL at C. Forgotten or reclaimed sources are the remainder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SummaryStatus {
    pub summary_id: String,
    pub source_count: usize,
    pub live_sources: usize,
    pub changed_sources: usize,
    pub expired_sources: usize,
    pub stale: bool,
    pub view_sequence: u64,
}

fn put_s(out: &mut Vec<u8>, value: &str) -> MemoryResult<()> {
    let length = u32::try_from(value.len()).map_err(|_| MemoryError::InvalidBundle)?;
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

/// The C1 DERIVED_FROM edge key (C1 contract section 4).
fn derived_edge_key(summary_key: &str, source_key: &str) -> MemoryResult<String> {
    let mut input = EDGE_DOMAIN.to_vec();
    put_s(&mut input, summary_key)?;
    put_s(&mut input, source_key)?;
    Ok(format!("derived:v1:{}", hex(&Sha256::digest(input))))
}

/// Nonzero canonical decimal u64: no sign, leading zero or whitespace.
fn canonical_revision(value: &MetadataValue) -> Option<u64> {
    let MetadataValue::String(value) = value else {
        return None;
    };
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

fn digest_shaped(value: &str) -> bool {
    value.len() == 67
        && value.starts_with("v1:")
        && value[3..]
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Strict C1 summary decoding; returns `c1_source_count`.
fn decode_summary(node: &NodeView, scope_key: &str, id: &str) -> MemoryResult<u64> {
    let invalid = MemoryError::InvalidBundle;
    let names_exact = SUMMARY_FIELDS
        .iter()
        .all(|name| node.properties.contains_key(*name))
        && node
            .properties
            .keys()
            .all(|name| name == "content" || SUMMARY_FIELDS.contains(&name.as_str()));
    if node.labels.len() != 1 || !node.labels.contains(LABEL_MEMORY) || !names_exact {
        return Err(invalid);
    }
    let memory = existing_memory(node).map_err(|_| MemoryError::InvalidBundle)?;
    let text = |name| string_property(node, name).map_err(|_| MemoryError::InvalidBundle);
    if memory.scope != scope_key
        || memory.id != id
        || memory.kind != "episodic"
        || memory.memory_revision != 1
        || text("c1_schema")? != SCHEMA
        || !opaque(text("c1_transformation")?)
        || !digest_shaped(text("c1_source_digest")?)
        || !matches!(
            node.properties.get("updated_at_ms"),
            Some(MetadataValue::Integer(_))
        )
        || !matches!(
            node.properties.get("content"),
            None | Some(MetadataValue::String(_))
        )
    {
        return Err(invalid);
    }
    match node.properties.get("c1_source_count") {
        Some(MetadataValue::Integer(count)) if (1..=MAX_EDGES as i64).contains(count) => {
            Ok(*count as u64)
        }
        _ => Err(invalid),
    }
}

/// A retained edge's source: a memory of this scope under its own key.
fn decode_source(node: &NodeView, scope_key: &str) -> MemoryResult<u64> {
    if !node.labels.contains(LABEL_MEMORY) {
        return Err(MemoryError::InvalidBundle);
    }
    let source = existing_memory(node).map_err(|_| MemoryError::InvalidBundle)?;
    if source.scope != scope_key
        || node.key != memory_key(scope_key, &source.id)
        || !matches!(source.kind.as_str(), "entity" | "episodic")
    {
        return Err(MemoryError::InvalidBundle);
    }
    Ok(source.memory_revision)
}

impl MemoryRef<'_> {
    pub(crate) fn summary_status(
        &self,
        request: &SummaryStatusRequest,
    ) -> MemoryResult<SummaryStatus> {
        request.scope.validate()?;
        if !opaque(&request.summary_id) {
            return Err(MemoryError::InvalidTarget);
        }
        let now = request.now_ms;
        if now > i64::MAX as u64 {
            return Err(MemoryError::InvalidClock);
        }
        let scope_key = request.scope.key();
        let graph = self.db.graph(&self.graph)?;
        let read = graph.with_clock(now);
        let scope_node = read
            .get_node(&scope_key)?
            .output
            .ok_or(MemoryError::InvalidScope)?;
        if string_property(&scope_node, "tuple")? != hex(&request.scope.encode()) {
            return Err(MemoryError::DigestCollision);
        }
        if super::erasure::barrier(&read, &scope_key)?.is_some() {
            return Err(MemoryError::DeletionPending);
        }
        let summary_key = memory_key(&scope_key, &request.summary_id);
        let summary = read.get_node(&summary_key)?;
        let view_sequence = summary.view_sequence;
        let summary = summary.output.ok_or(MemoryError::MissingMemory)?;
        let source_count = decode_summary(&summary, &scope_key, &request.summary_id)?;
        let limits = GraphLimits {
            max_edges_examined: MAX_EDGES,
            ..GraphLimits::DEFAULT
        };
        let (counts, _, _) = derive(
            &graph,
            &scope_key,
            &summary_key,
            source_count,
            now,
            limits,
            None,
        )?;
        let counts = counts.ok_or(MemoryError::OperationLimit)?;
        Ok(SummaryStatus {
            summary_id: request.summary_id.clone(),
            source_count: source_count as usize,
            live_sources: counts.live,
            changed_sources: counts.changed,
            expired_sources: counts.expired,
            stale: counts.stale,
            view_sequence,
        })
    }

    /// S2: the stale label of one recall hit at the recall clock C. Examined
    /// DERIVED_FROM edges are charged to the recall graph budget. `None` for
    /// a non-summary; for a summary only when `allow_partial` accepted an
    /// exhausted budget, which is then marked in `truncation`. Never guessed.
    pub(super) fn recall_stale_label(
        &self,
        request: &RecallRequest<'_>,
        scope: &MemoryScope,
        id: &str,
        deadline_ms: u64,
        edges_charged: &mut u64,
        truncation: &mut Vec<TruncationReason>,
    ) -> Result<Option<bool>, RecallError> {
        let now = request.clock_ms;
        let scope_key = scope.key();
        let summary_key = memory_key(&scope_key, id);
        let graph = self.db.graph(&self.graph)?;
        let read = graph.with_clock(now);
        if read
            .memory_string_property(&summary_key, "c1_schema")
            .is_none()
        {
            return Ok(None);
        }
        let summary = read
            .get_node(&summary_key)?
            .output
            .ok_or(MemoryError::InvalidBundle)?;
        let source_count = decode_summary(&summary, &scope_key, id)?;
        let remaining = request
            .limits
            .graph
            .max_edges_examined
            .saturating_sub(*edges_charged);
        let reasons = if remaining == 0 {
            vec![TruncationReason::EdgeLimit]
        } else {
            let limits = GraphLimits {
                max_edges_examined: remaining.min(MAX_EDGES),
                deadline_ms,
                ..GraphLimits::DEFAULT
            };
            let (counts, examined, reasons) = derive(
                &graph,
                &scope_key,
                &summary_key,
                source_count,
                now,
                limits,
                request.cancel,
            )?;
            *edges_charged += examined;
            match counts {
                Some(counts) => return Ok(Some(counts.stale)),
                // S1's own 256-edge cap, not recall's budget, stopped the read.
                None if remaining >= MAX_EDGES => return Err(MemoryError::OperationLimit.into()),
                None => reasons,
            }
        };
        if !request.allow_partial {
            return Err(RecallError::Budget("graph"));
        }
        for reason in reasons {
            if !truncation.contains(&reason) {
                truncation.push(reason);
            }
        }
        Ok(None)
    }
}

/// Retained-edge counts of one decoded summary at C.
struct Counts {
    live: usize,
    changed: usize,
    expired: usize,
    stale: bool,
}

/// Reads and checks a decoded summary's retained DERIVED_FROM edges within
/// `limits.max_edges_examined`. Returns the counts (only for a complete
/// read), the edges examined and the read's truncation reasons.
fn derive(
    graph: &GraphRef<'_>,
    scope_key: &str,
    summary_key: &str,
    source_count: u64,
    now: u64,
    limits: GraphLimits,
    cancel: Option<&AtomicBool>,
) -> MemoryResult<(Option<Counts>, u64, Vec<TruncationReason>)> {
    // Retained edges, independent of TTL: clock 0 hides nothing that a
    // deadline in the epoch could hide. Visibility is then decided at C.
    let limits = GraphLimits {
        max_edges_per_node: MAX_EDGES,
        max_output_edges: MAX_EDGES,
        max_output_nodes: MAX_EDGES,
        ..limits
    };
    let edges = graph.with_clock(0).neighbors(
        &Neighbors {
            node: NodeSelector::key(summary_key),
            direction: Direction::Out,
            types: Some(vec![DERIVED_FROM.into()]),
            edge_conditions: Vec::new(),
            limit: Some(MAX_EDGES),
            cursor: None,
        },
        &ReadOptions {
            limits,
            cancel,
            ..ReadOptions::default()
        },
    )?;
    let examined = edges.stats.edges_examined;
    if !edges.complete {
        return Ok((None, examined, edges.truncation));
    }
    let output = edges.output;
    let retained = output.edges.len() as u64;
    if retained > source_count {
        return Err(MemoryError::InvalidBundle);
    }
    let nodes: BTreeMap<u64, &NodeView> = output.nodes.iter().map(|node| (node.id, node)).collect();
    let (mut live, mut changed, mut expired) = (0, 0, 0);
    for edge in &output.edges {
        let edge_revision = edge.properties.get("c1_source_memory_revision");
        if edge.key.as_deref() != Some(&derived_edge_key(summary_key, &edge.target_key)?)
            || edge.expires_at_ms.is_some()
            || edge.properties.len() != 2
            || edge.properties.get("c1_schema") != Some(&MetadataValue::String(SCHEMA.into()))
        {
            return Err(MemoryError::InvalidBundle);
        }
        let edge_revision = edge_revision
            .and_then(canonical_revision)
            .ok_or(MemoryError::InvalidBundle)?;
        let source = nodes.get(&edge.target).ok_or(MemoryError::InvalidBundle)?;
        let source_revision = decode_source(source, scope_key)?;
        if source.expires_at_ms.is_some_and(|deadline| deadline <= now) {
            expired += 1;
        } else {
            live += 1;
            if source_revision != edge_revision {
                changed += 1;
            }
        }
    }
    let counts = Counts {
        live,
        changed,
        expired,
        stale: retained < source_count || changed > 0 || expired > 0,
    };
    Ok((Some(counts), examined, Vec::new()))
}

#[cfg(test)]
mod tests;
