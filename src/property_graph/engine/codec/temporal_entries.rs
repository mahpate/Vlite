//! Gap 4: temporal edge entries (tags 65..=69).
//!
//! These entries appear only in files whose header carries the temporal
//! capability flag; legacy edge entries keep their meaning and payload.
//! Payloads (little-endian):
//!
//! - `bounds`: `flags u8` (bit 0: `valid_from` present, bit 1: `valid_to`
//!   present, other bits zero), then each present bound as `i64`.
//! - `segment`: `bounds`, then `properties_len u32` and the properties.
//! - 65 `PutEdge`: `edge_id u64`, `source u64`, `destination u64`,
//!   `type_len u8`, type, `key_len u16`, key (empty: keyless),
//!   `segment_count u16` (at most 256), then the segments in series order.
//!   Creates or promotes the edge with its whole series, or restates it in
//!   a snapshot.
//! - 66 `AppendSegment`: `edge_id u64`, `segment`.
//! - 67 `ReplaceSegment`: `edge_id u64`, old `bounds`, new `segment`.
//! - 68 `CloseSegment`: `edge_id u64`, old `bounds`, `at i64`.
//! - 69 `RemoveSegment`: `edge_id u64`, old `bounds`.

use super::super::error::GraphError;
use super::super::model::{EdgeId, NodeId, validate_edge_key, validate_relationship_type};
use super::super::temporal::{self, MAX_SEGMENTS, Segment};
use super::{Reader, properties_len, put_properties, tag};

/// One temporal edge entry. Segments are addressed by their exact current
/// bounds; there is no segment ID.
#[derive(Clone, Debug, PartialEq)]
pub enum TemporalEntry {
    /// Creates a temporal edge, or promotes the legacy edge with this ID
    /// (same endpoints, type and key) to one, with this whole series.
    PutEdge {
        edge_id: EdgeId,
        source: NodeId,
        destination: NodeId,
        relationship: String,
        key: Option<String>,
        segments: Vec<Segment>,
    },
    /// Inserts a segment at its ordered position (backfill is allowed).
    AppendSegment { edge_id: EdgeId, segment: Segment },
    /// Replaces the segment with exactly the old bounds.
    ReplaceSegment {
        edge_id: EdgeId,
        valid_from: Option<i64>,
        valid_to: Option<i64>,
        segment: Segment,
    },
    /// Sets `valid_to = at` on the segment with exactly the old bounds.
    CloseSegment {
        edge_id: EdgeId,
        valid_from: Option<i64>,
        valid_to: Option<i64>,
        at: i64,
    },
    /// Removes the segment with exactly the old bounds.
    RemoveSegment {
        edge_id: EdgeId,
        valid_from: Option<i64>,
        valid_to: Option<i64>,
    },
}

impl TemporalEntry {
    pub fn tag(&self) -> u8 {
        match self {
            Self::PutEdge { .. } => tag::TEMPORAL_PUT_EDGE,
            Self::AppendSegment { .. } => tag::TEMPORAL_APPEND_SEGMENT,
            Self::ReplaceSegment { .. } => tag::TEMPORAL_REPLACE_SEGMENT,
            Self::CloseSegment { .. } => tag::TEMPORAL_CLOSE_SEGMENT,
            Self::RemoveSegment { .. } => tag::TEMPORAL_REMOVE_SEGMENT,
        }
    }

    /// The edge the entry changes.
    pub fn edge_id(&self) -> EdgeId {
        match self {
            Self::PutEdge { edge_id, .. }
            | Self::AppendSegment { edge_id, .. }
            | Self::ReplaceSegment { edge_id, .. }
            | Self::CloseSegment { edge_id, .. }
            | Self::RemoveSegment { edge_id, .. } => *edge_id,
        }
    }

    /// Checks every rule that does not depend on state.
    pub fn validate(&self) -> Result<(), GraphError> {
        match self {
            Self::PutEdge {
                relationship,
                key,
                segments,
                ..
            } => {
                validate_relationship_type(relationship)?;
                if let Some(key) = key {
                    validate_edge_key(key)?;
                }
                temporal::validate_series(segments)
            }
            Self::AppendSegment { segment, .. } => temporal::validate_segment(segment),
            Self::ReplaceSegment {
                valid_from,
                valid_to,
                segment,
                ..
            } => {
                temporal::check_bounds(*valid_from, *valid_to)?;
                temporal::validate_segment(segment)
            }
            Self::CloseSegment {
                valid_from,
                valid_to,
                at,
                ..
            } => temporal::check_close(*valid_from, *valid_to, *at).map(drop),
            Self::RemoveSegment {
                valid_from,
                valid_to,
                ..
            } => temporal::check_bounds(*valid_from, *valid_to),
        }
    }
}

const HAS_FROM: u8 = 0b01;
const HAS_TO: u8 = 0b10;

fn put_bounds(valid_from: Option<i64>, valid_to: Option<i64>, bytes: &mut Vec<u8>) {
    let flags = if valid_from.is_some() { HAS_FROM } else { 0 }
        | if valid_to.is_some() { HAS_TO } else { 0 };
    bytes.push(flags);
    for bound in [valid_from, valid_to].into_iter().flatten() {
        bytes.extend_from_slice(&bound.to_le_bytes());
    }
}

fn bounds_len(valid_from: Option<i64>, valid_to: Option<i64>) -> usize {
    1 + 8 * (usize::from(valid_from.is_some()) + usize::from(valid_to.is_some()))
}

fn put_segment(segment: &Segment, bytes: &mut Vec<u8>) {
    put_bounds(segment.valid_from, segment.valid_to, bytes);
    put_properties(&segment.properties, bytes);
}

fn segment_len(segment: &Segment) -> Result<usize, GraphError> {
    Ok(bounds_len(segment.valid_from, segment.valid_to) + properties_len(&segment.properties)?)
}

pub(super) fn encode(entry: &TemporalEntry, bytes: &mut Vec<u8>) {
    bytes.extend_from_slice(&entry.edge_id().0.to_le_bytes());
    match entry {
        TemporalEntry::PutEdge {
            source,
            destination,
            relationship,
            key,
            segments,
            ..
        } => {
            bytes.extend_from_slice(&source.0.to_le_bytes());
            bytes.extend_from_slice(&destination.0.to_le_bytes());
            bytes.push(relationship.len() as u8);
            bytes.extend_from_slice(relationship.as_bytes());
            let key = key.as_deref().unwrap_or("");
            bytes.extend_from_slice(&(key.len() as u16).to_le_bytes());
            bytes.extend_from_slice(key.as_bytes());
            bytes.extend_from_slice(&(segments.len() as u16).to_le_bytes());
            for segment in segments {
                put_segment(segment, bytes);
            }
        }
        TemporalEntry::AppendSegment { segment, .. } => put_segment(segment, bytes),
        TemporalEntry::ReplaceSegment {
            valid_from,
            valid_to,
            segment,
            ..
        } => {
            put_bounds(*valid_from, *valid_to, bytes);
            put_segment(segment, bytes);
        }
        TemporalEntry::CloseSegment {
            valid_from,
            valid_to,
            at,
            ..
        } => {
            put_bounds(*valid_from, *valid_to, bytes);
            bytes.extend_from_slice(&at.to_le_bytes());
        }
        TemporalEntry::RemoveSegment {
            valid_from,
            valid_to,
            ..
        } => put_bounds(*valid_from, *valid_to, bytes),
    }
}

/// Exact payload length of a validated entry.
pub(super) fn payload_len(entry: &TemporalEntry) -> Result<usize, GraphError> {
    Ok(8 + match entry {
        TemporalEntry::PutEdge {
            relationship,
            key,
            segments,
            ..
        } => {
            16 + 1
                + relationship.len()
                + 2
                + key.as_ref().map_or(0, String::len)
                + 2
                + segments
                    .iter()
                    .map(segment_len)
                    .sum::<Result<usize, GraphError>>()?
        }
        TemporalEntry::AppendSegment { segment, .. } => segment_len(segment)?,
        TemporalEntry::ReplaceSegment {
            valid_from,
            valid_to,
            segment,
            ..
        } => bounds_len(*valid_from, *valid_to) + segment_len(segment)?,
        TemporalEntry::CloseSegment {
            valid_from,
            valid_to,
            ..
        } => bounds_len(*valid_from, *valid_to) + 8,
        TemporalEntry::RemoveSegment {
            valid_from,
            valid_to,
            ..
        } => bounds_len(*valid_from, *valid_to),
    })
}

fn read_i64(r: &mut Reader<'_>) -> Result<i64, GraphError> {
    r.array().map(i64::from_le_bytes)
}

fn read_bounds(r: &mut Reader<'_>) -> Result<(Option<i64>, Option<i64>), GraphError> {
    let flags = r.u8()?;
    if flags & !(HAS_FROM | HAS_TO) != 0 {
        return Err(GraphError::Corrupt("invalid temporal bounds flags"));
    }
    let valid_from = if flags & HAS_FROM != 0 {
        Some(read_i64(r)?)
    } else {
        None
    };
    let valid_to = if flags & HAS_TO != 0 {
        Some(read_i64(r)?)
    } else {
        None
    };
    Ok((valid_from, valid_to))
}

fn read_segment(r: &mut Reader<'_>) -> Result<Segment, GraphError> {
    let (valid_from, valid_to) = read_bounds(r)?;
    Ok(Segment {
        valid_from,
        valid_to,
        properties: r.properties()?,
    })
}

/// Decodes the payload of tag 65..=69; the caller checks trailing bytes and
/// validates the entry.
pub(super) fn decode(tag: u8, r: &mut Reader<'_>) -> Result<TemporalEntry, GraphError> {
    let edge_id = EdgeId(r.u64()?);
    Ok(match tag {
        tag::TEMPORAL_PUT_EDGE => {
            let source = NodeId(r.u64()?);
            let destination = NodeId(r.u64()?);
            let len = r.u8()? as usize;
            let relationship = r.string(len)?;
            let len = r.u16()? as usize;
            let key = if len == 0 { None } else { Some(r.string(len)?) };
            let count = r.u16()? as usize;
            if count > MAX_SEGMENTS {
                return Err(GraphError::InvalidTemporal(temporal::reason::SEGMENT_LIMIT));
            }
            let segments = (0..count)
                .map(|_| read_segment(r))
                .collect::<Result<_, _>>()?;
            TemporalEntry::PutEdge {
                edge_id,
                source,
                destination,
                relationship,
                key,
                segments,
            }
        }
        tag::TEMPORAL_APPEND_SEGMENT => TemporalEntry::AppendSegment {
            edge_id,
            segment: read_segment(r)?,
        },
        tag::TEMPORAL_REPLACE_SEGMENT => {
            let (valid_from, valid_to) = read_bounds(r)?;
            TemporalEntry::ReplaceSegment {
                edge_id,
                valid_from,
                valid_to,
                segment: read_segment(r)?,
            }
        }
        tag::TEMPORAL_CLOSE_SEGMENT => {
            let (valid_from, valid_to) = read_bounds(r)?;
            TemporalEntry::CloseSegment {
                edge_id,
                valid_from,
                valid_to,
                at: read_i64(r)?,
            }
        }
        tag::TEMPORAL_REMOVE_SEGMENT => {
            let (valid_from, valid_to) = read_bounds(r)?;
            TemporalEntry::RemoveSegment {
                edge_id,
                valid_from,
                valid_to,
            }
        }
        other => return Err(GraphError::UnknownEntryTag(other)),
    })
}
