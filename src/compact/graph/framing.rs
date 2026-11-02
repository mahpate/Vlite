//! Private P4 graph-block framing around the unchanged VLGCSR06 codec.

use super::{Graph, GraphBody, engine_state};
use crate::property_graph::engine::GraphState;
use crate::property_graph::frame::{self, compacted_bodies};
use crate::{Error, Result};
use std::collections::BTreeSet;

pub(crate) const KIND: u16 = 0x0300;
const VERSION: u16 = 1;
const CRITICAL: u16 = 1;
const HEADER_LEN: usize = 32;
const SECTION_ENTRY_LEN: usize = 8;
const MAX_BODY: usize = 64 << 20;

#[cfg(test)]
thread_local! {
    static TEST_BODY_LIMIT: std::cell::Cell<usize> = const { std::cell::Cell::new(MAX_BODY) };
}

fn body_limit() -> usize {
    #[cfg(test)]
    {
        TEST_BODY_LIMIT.with(std::cell::Cell::get)
    }
    #[cfg(not(test))]
    {
        MAX_BODY
    }
}

#[cfg(test)]
pub(crate) struct BodyLimit(usize);

#[cfg(test)]
impl BodyLimit {
    pub(crate) fn set(limit: usize) -> Self {
        Self(TEST_BODY_LIMIT.with(|cell| cell.replace(limit)))
    }
}

#[cfg(test)]
impl Drop for BodyLimit {
    fn drop(&mut self) {
        TEST_BODY_LIMIT.with(|cell| cell.set(self.0));
    }
}

fn corrupt(offset: u64) -> Error {
    Error::Corrupt {
        offset,
        reason: "invalid compact graph block",
    }
}

fn encode_source(source: &super::GraphSource) -> Result<(u32, Vec<u8>)> {
    let count = source
        .nodes
        .len()
        .checked_add(source.edges.len())
        .and_then(|count| u32::try_from(count).ok())
        .ok_or_else(|| corrupt(0))?;
    let section = super::encode(source).map_err(|_| corrupt(0))?;
    let body_len = HEADER_LEN
        .checked_add(SECTION_ENTRY_LEN)
        .and_then(|len| len.checked_add(section.len()))
        .filter(|&len| len <= body_limit())
        .ok_or_else(|| corrupt(0))?;
    let section_len = u32::try_from(section.len()).map_err(|_| corrupt(0))?;
    let mut body = Vec::new();
    body.try_reserve_exact(body_len).map_err(|_| corrupt(0))?;
    for field in [KIND, VERSION, CRITICAL, 1] {
        body.extend_from_slice(&field.to_le_bytes());
    }
    body.extend_from_slice(&u64::from(source.graph_id).to_le_bytes());
    body.extend_from_slice(&0u64.to_le_bytes());
    body.extend_from_slice(&((HEADER_LEN + SECTION_ENTRY_LEN) as u32).to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&((HEADER_LEN + SECTION_ENTRY_LEN) as u32).to_le_bytes());
    body.extend_from_slice(&section_len.to_le_bytes());
    body.extend_from_slice(&section);
    debug_assert_eq!(body.len(), body_len);
    Ok((count, body))
}

/// Encodes one graph after applying the checkpoint's shared expiry clock.
pub(crate) fn encode_graph(graph: &Graph, now_ms: Option<u64>) -> Result<(u32, Vec<u8>)> {
    let mut source = super::GraphSource::from_graph(graph).map_err(|_| corrupt(0))?;
    source.nodes.retain(|node| {
        !now_ms.is_some_and(|now| node.deadline.is_some_and(|deadline| deadline <= now))
    });
    let nodes: BTreeSet<_> = source.nodes.iter().map(|node| node.id).collect();
    source.edges.retain(|edge| {
        nodes.contains(&edge.source)
            && nodes.contains(&edge.target)
            && !now_ms.is_some_and(|now| edge.deadline.is_some_and(|deadline| deadline <= now))
    });
    encode_source(&source)
}

/// Preflights every graph before a checkpoint writer emits its header.
pub(crate) fn preflight(state: &GraphState, now_ms: Option<u64>) -> Result<()> {
    for graph in state.graphs() {
        encode_graph(graph, now_ms)?;
    }
    Ok(())
}

/// Decodes and fully validates a block into the same snapshot bodies op 15
/// uses, so callers share PendingSnapshot and graph snapshot validation.
pub(crate) fn graph_keys(
    owner: u16,
    count: u32,
    payload: &[u8],
    floor: u64,
    offset: u64,
) -> Result<Vec<GraphBody>> {
    if payload
        .len()
        .checked_add(HEADER_LEN + SECTION_ENTRY_LEN)
        .is_none_or(|len| len > MAX_BODY)
    {
        return Err(corrupt(offset));
    }
    let compact = super::decode(payload).map_err(|_| corrupt(offset))?;
    if compact.source.graph_id != owner
        || compact
            .source
            .nodes
            .len()
            .checked_add(compact.source.edges.len())
            .and_then(|items| u32::try_from(items).ok())
            != Some(count)
    {
        return Err(corrupt(offset));
    }
    let state = engine_state(&compact.source).map_err(|_| corrupt(offset))?;
    snapshot_bodies(&state, floor, owner, offset)
}

fn snapshot_bodies(
    state: &GraphState,
    floor: u64,
    owner: u16,
    offset: u64,
) -> Result<Vec<GraphBody>> {
    let mut bodies = Vec::new();
    compacted_bodies(state, None, floor, |graph, count, body| {
        if graph.0 != owner {
            return Err(corrupt(offset));
        }
        bodies.push(frame::decode_frame(Some(floor), 0, count, body, offset)?);
        Ok(())
    })?;
    if bodies.is_empty() {
        return Err(corrupt(offset));
    }
    Ok(bodies)
}

#[cfg(test)]
mod tests;
