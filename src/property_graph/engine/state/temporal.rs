//! Gap 4: temporal edges in the graph state.
//!
//! A temporal edge is an ordinary edge record (ID, key, endpoints, type,
//! revision, deadline; one key-map and one adjacency entry per logical
//! edge) whose properties live in its ordered validity segments, kept in
//! `Graph::segments`; its own `properties` stay empty. Temporal entries
//! apply only to a state with the temporal capability, every real change
//! advances the edge revision to the commit sequence, and each inverse
//! keeps only the changed segment, never a copy of the whole series.

use super::super::codec::{TemporalEntry, check_series_bytes};
use super::super::temporal::{self as series, reason};
use super::*;

/// The inverse of one applied temporal entry. `revision` is the edge
/// revision before the entry.
pub(in super::super) enum TemporalUndo {
    /// A new temporal edge.
    Created { id: EdgeId, previous_next: u64 },
    /// A legacy edge became temporal; its properties come back.
    Promoted {
        id: EdgeId,
        properties: Properties,
        revision: u64,
    },
    Inserted {
        id: EdgeId,
        index: usize,
        revision: u64,
    },
    Replaced {
        id: EdgeId,
        removed: usize,
        inserted: usize,
        old: Segment,
        revision: u64,
    },
    Closed {
        id: EdgeId,
        index: usize,
        valid_to: Option<i64>,
        revision: u64,
    },
    Removed {
        id: EdgeId,
        index: usize,
        segment: Segment,
        revision: u64,
    },
}

impl GraphState {
    /// Gap 4: whether temporal entries may be applied (the file carries the
    /// temporal capability).
    pub fn is_temporal(&self) -> bool {
        self.temporal
    }

    /// Sets the temporal capability. The adapter enables it with the first
    /// temporal write and on load of a file that has it; it clears it only
    /// to undo a tentative enable before any temporal entry was applied.
    pub fn set_temporal(&mut self, temporal: bool) {
        self.temporal = temporal;
    }
}

impl Graph {
    /// Gap 4: the validity segments of a temporal edge in series order
    /// (possibly empty); `None` for a legacy edge or an unknown ID.
    pub fn segments(&self, id: EdgeId) -> Option<&[Segment]> {
        self.segments.get(&id).map(Vec::as_slice)
    }

    /// The entry restating `edge` in a snapshot or compaction: the whole
    /// series as a temporal `PutEdge` for a temporal edge, else the legacy
    /// `PutEdge`.
    pub fn edge_snapshot_entry(&self, edge: &GraphEdge) -> GraphEntry {
        match self.segments.get(&edge.id) {
            Some(segments) => GraphEntry::Temporal(TemporalEntry::PutEdge {
                edge_id: edge.id,
                source: edge.source,
                destination: edge.destination,
                relationship: edge.relationship.clone(),
                key: edge.key.clone(),
                segments: segments.clone(),
            }),
            None => GraphEntry::PutEdge {
                edge_id: edge.id,
                source: edge.source,
                destination: edge.destination,
                relationship: edge.relationship.clone(),
                key: edge.key.clone(),
                properties: edge.properties.clone(),
            },
        }
    }

    pub(super) fn apply_temporal(
        &mut self,
        sequence: u64,
        entry: &TemporalEntry,
    ) -> Result<TemporalUndo, GraphError> {
        if let TemporalEntry::PutEdge {
            edge_id,
            source,
            destination,
            relationship,
            key,
            segments,
        } = entry
        {
            let edge = GraphEdge {
                id: *edge_id,
                key: key.clone(),
                source: *source,
                relationship: relationship.clone(),
                destination: *destination,
                properties: Properties::new(),
                revision: sequence,
            };
            return self.put_temporal_edge(edge, segments);
        }
        let id = entry.edge_id();
        let revision = self
            .edges
            .get(&id)
            .ok_or(GraphError::EdgeNotFound(id))?
            .revision;
        let segments = self
            .segments
            .get_mut(&id)
            .ok_or(GraphError::InvalidTemporal(reason::NOT_TEMPORAL))?;
        let undo = match entry {
            TemporalEntry::PutEdge { .. } => unreachable!("handled above"),
            TemporalEntry::AppendSegment { segment, .. } => TemporalUndo::Inserted {
                id,
                index: series::insert(segments, segment.clone())?,
                revision,
            },
            TemporalEntry::ReplaceSegment {
                valid_from,
                valid_to,
                segment,
                ..
            } => {
                let (removed, old) = series::remove(segments, *valid_from, *valid_to)?;
                match series::insert(segments, segment.clone()) {
                    Ok(inserted) => TemporalUndo::Replaced {
                        id,
                        removed,
                        inserted,
                        old,
                        revision,
                    },
                    Err(error) => {
                        segments.insert(removed, old);
                        return Err(error);
                    }
                }
            }
            TemporalEntry::CloseSegment {
                valid_from,
                valid_to,
                at,
                ..
            } => {
                let index = series::position(segments, *valid_from, *valid_to)
                    .ok_or(GraphError::InvalidTemporal(reason::NO_SEGMENT))?;
                // Closing at the existing end changes no segment but still
                // advances the revision; planners never emit it.
                series::close(segments, *valid_from, *valid_to, *at)?;
                TemporalUndo::Closed {
                    id,
                    index,
                    valid_to: *valid_to,
                    revision,
                }
            }
            TemporalEntry::RemoveSegment {
                valid_from,
                valid_to,
                ..
            } => {
                let (index, segment) = series::remove(segments, *valid_from, *valid_to)?;
                TemporalUndo::Removed {
                    id,
                    index,
                    segment,
                    revision,
                }
            }
        };
        if matches!(
            entry,
            TemporalEntry::AppendSegment { .. } | TemporalEntry::ReplaceSegment { .. }
        ) {
            let edge = &self.edges[&id];
            if let Err(error) =
                check_series_bytes(&edge.relationship, edge.key.as_deref(), &self.segments[&id])
            {
                self.undo_temporal(undo);
                return Err(error);
            }
        }
        self.edges.get_mut(&id).expect("edge exists").revision = sequence;
        Ok(undo)
    }

    /// Creates a temporal edge, or promotes the legacy edge with its ID and
    /// identity (endpoints, type and key) to one.
    fn put_temporal_edge(
        &mut self,
        edge: GraphEdge,
        segments: &[Segment],
    ) -> Result<TemporalUndo, GraphError> {
        let id = edge.id;
        if let Some(existing) = self.edges.get_mut(&id) {
            if self.segments.contains_key(&id) {
                return Err(GraphError::InvalidTemporal(reason::ALREADY_TEMPORAL));
            }
            if (&existing.key, existing.source, &existing.relationship)
                != (&edge.key, edge.source, &edge.relationship)
                || existing.destination != edge.destination
            {
                return Err(GraphError::EdgeIdUnavailable(id));
            }
            check_series_bytes(&edge.relationship, edge.key.as_deref(), segments)?;
            let properties = std::mem::take(&mut existing.properties);
            let revision = std::mem::replace(&mut existing.revision, edge.revision);
            self.segments.insert(id, segments.to_vec());
            return Ok(TemporalUndo::Promoted {
                id,
                properties,
                revision,
            });
        }
        if id.0 < self.next_edge_id {
            return Err(GraphError::EdgeIdUnavailable(id));
        }
        for endpoint in [edge.source, edge.destination] {
            if !self.nodes.contains_key(&endpoint) {
                return Err(GraphError::NodeNotFound(endpoint));
            }
        }
        if let Some(key) = &edge.key
            && self.edge_keys.contains_key(key)
        {
            return Err(GraphError::EdgeKeyExists(key.clone()));
        }
        check_series_bytes(&edge.relationship, edge.key.as_deref(), segments)?;
        let previous_next = new_id(&mut self.next_edge_id, id.0)?;
        self.insert_edge(edge);
        self.segments.insert(id, segments.to_vec());
        Ok(TemporalUndo::Created { id, previous_next })
    }

    pub(super) fn undo_temporal(&mut self, undo: TemporalUndo) {
        let (id, revision) = match undo {
            TemporalUndo::Created { id, previous_next } => {
                self.segments.remove(&id);
                self.remove_edge(id);
                self.next_edge_id = previous_next;
                return;
            }
            TemporalUndo::Promoted {
                id,
                properties,
                revision,
            } => {
                self.segments.remove(&id);
                self.edges.get_mut(&id).expect("edge exists").properties = properties;
                (id, revision)
            }
            TemporalUndo::Inserted {
                id,
                index,
                revision,
            } => {
                self.series_mut(id).remove(index);
                (id, revision)
            }
            TemporalUndo::Replaced {
                id,
                removed,
                inserted,
                old,
                revision,
            } => {
                let segments = self.series_mut(id);
                segments.remove(inserted);
                segments.insert(removed, old);
                (id, revision)
            }
            TemporalUndo::Closed {
                id,
                index,
                valid_to,
                revision,
            } => {
                self.series_mut(id)[index].valid_to = valid_to;
                (id, revision)
            }
            TemporalUndo::Removed {
                id,
                index,
                segment,
                revision,
            } => {
                self.series_mut(id).insert(index, segment);
                (id, revision)
            }
        };
        self.edges.get_mut(&id).expect("edge exists").revision = revision;
    }

    fn series_mut(&mut self, id: EdgeId) -> &mut Vec<Segment> {
        self.segments.get_mut(&id).expect("temporal edge exists")
    }

    /// INV: every series belongs to a live edge with empty properties, is
    /// valid and fits one entry; none exists without the capability.
    pub(super) fn check_segments(&self, temporal: bool) -> Result<(), GraphError> {
        let violation = GraphError::InvariantViolation;
        if !temporal && !self.segments.is_empty() {
            return Err(violation("temporal edge without the temporal capability"));
        }
        for (id, segments) in &self.segments {
            let edge = self.edges.get(id).ok_or(violation("dangling series"))?;
            if !edge.properties.is_empty() {
                return Err(violation("temporal edge with edge properties"));
            }
            series::validate_series(segments)
                .and_then(|()| {
                    check_series_bytes(&edge.relationship, edge.key.as_deref(), segments)
                })
                .map_err(|_| violation("invalid series"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
