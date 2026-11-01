//! Gap 4: key-addressed temporal-edge mutations.
//!
//! They plan into the same batch as ordinary mutations, count toward its
//! entry limit and observe (and are observed by) earlier mutations of the
//! batch. Every mutation of an existing series needs the exact revision of
//! the edge, which covers its whole series.

use super::super::codec::TemporalEntry;
use super::super::temporal::{self as series, Segment, reason};
use super::*;

/// One temporal-edge mutation of the planner's graph. Segments are
/// addressed by their exact current bounds; `expected` must be
/// [`Precondition::Revision`].
#[derive(Clone, Debug, PartialEq)]
pub enum TemporalMutation {
    /// Creates a temporal edge with this series (possibly empty). With a
    /// key that is live, an identical temporal edge (same endpoints, type
    /// and series) is returned unchanged (`created = false`); any other
    /// edge fails `EdgeKeyConflict`.
    CreateEdge {
        key: Option<String>,
        source: String,
        relationship: String,
        destination: String,
        segments: Vec<Segment>,
    },
    /// Makes a legacy edge temporal: one all-time segment with its current
    /// properties. The edge keeps its ID, key and endpoints.
    Promote {
        edge: EdgeSelector,
        expected: Precondition,
    },
    /// Inserts a segment at its ordered position (backfill is allowed).
    AppendSegment {
        edge: EdgeSelector,
        segment: Segment,
        expected: Precondition,
    },
    /// Replaces the segment with exactly these bounds (bounds may change).
    ReplaceSegment {
        edge: EdgeSelector,
        valid_from: Option<i64>,
        valid_to: Option<i64>,
        segment: Segment,
        expected: Precondition,
    },
    /// Ends the segment with exactly these bounds at `at`, strictly inside
    /// it; `at` equal to its finite end is a no-op that plans nothing.
    CloseSegment {
        edge: EdgeSelector,
        valid_from: Option<i64>,
        valid_to: Option<i64>,
        at: i64,
        expected: Precondition,
    },
    /// Removes the segment with exactly these bounds; the edge stays, with
    /// a possibly empty series, until it is deleted or expires.
    RemoveSegment {
        edge: EdgeSelector,
        valid_from: Option<i64>,
        valid_to: Option<i64>,
        expected: Precondition,
    },
}

fn same_series(a: &[Segment], b: &[Segment]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(a, b)| {
            a.has_bounds(b.valid_from, b.valid_to) && same_properties(&a.properties, &b.properties)
        })
}

impl Planner<'_> {
    /// Plans one temporal mutation; see [`Planner::mutate`]. The state must
    /// have the temporal capability, which the adapter enables with the
    /// first temporal write.
    pub fn temporal(&mut self, mutation: &TemporalMutation) -> Result<MutationOutcome, GraphError> {
        self.reclaim_due();
        if !self.state.is_temporal() {
            return Err(GraphError::InvalidTemporal(reason::CAPABILITY));
        }
        let (id, entry) = match mutation {
            TemporalMutation::CreateEdge {
                key,
                source,
                relationship,
                destination,
                segments,
            } => {
                return self.create_temporal_edge(key, source, relationship, destination, segments);
            }
            TemporalMutation::Promote { edge, expected } => {
                let id = self.revised_edge(edge, *expected)?;
                let edge = self.graph().edge(id).expect("checked above");
                let entry = TemporalEntry::PutEdge {
                    edge_id: id,
                    source: edge.source,
                    destination: edge.destination,
                    relationship: edge.relationship.clone(),
                    key: edge.key.clone(),
                    segments: vec![Segment {
                        valid_from: None,
                        valid_to: None,
                        properties: edge.properties.clone(),
                    }],
                };
                (id, entry)
            }
            TemporalMutation::AppendSegment {
                edge,
                segment,
                expected,
            } => {
                let id = self.revised_edge(edge, *expected)?;
                let entry = TemporalEntry::AppendSegment {
                    edge_id: id,
                    segment: segment.clone(),
                };
                (id, entry)
            }
            TemporalMutation::ReplaceSegment {
                edge,
                valid_from,
                valid_to,
                segment,
                expected,
            } => {
                let id = self.revised_edge(edge, *expected)?;
                let entry = TemporalEntry::ReplaceSegment {
                    edge_id: id,
                    valid_from: *valid_from,
                    valid_to: *valid_to,
                    segment: segment.clone(),
                };
                (id, entry)
            }
            TemporalMutation::CloseSegment {
                edge,
                valid_from,
                valid_to,
                at,
                expected,
            } => {
                let id = self.revised_edge(edge, *expected)?;
                if !series::check_close(*valid_from, *valid_to, *at)? {
                    // An exact no-op once the revision and segment check.
                    let segments = self
                        .graph()
                        .segments(id)
                        .ok_or(GraphError::InvalidTemporal(reason::NOT_TEMPORAL))?;
                    series::position(segments, *valid_from, *valid_to)
                        .ok_or(GraphError::InvalidTemporal(reason::NO_SEGMENT))?;
                    return Ok(MutationOutcome::Edge { id, created: false });
                }
                let entry = TemporalEntry::CloseSegment {
                    edge_id: id,
                    valid_from: *valid_from,
                    valid_to: *valid_to,
                    at: *at,
                };
                (id, entry)
            }
            TemporalMutation::RemoveSegment {
                edge,
                valid_from,
                valid_to,
                expected,
            } => {
                let id = self.revised_edge(edge, *expected)?;
                let entry = TemporalEntry::RemoveSegment {
                    edge_id: id,
                    valid_from: *valid_from,
                    valid_to: *valid_to,
                };
                (id, entry)
            }
        };
        self.push(GraphEntry::Temporal(entry))?;
        Ok(MutationOutcome::Edge { id, created: false })
    }

    /// A live edge whose revision matches `expected`, which must name one.
    /// The revision is the one the edge had before this batch if a temporal
    /// mutation of the batch already changed it, so one batch can close a
    /// segment and append its successor under the same revision check.
    fn revised_edge(
        &mut self,
        edge: &EdgeSelector,
        expected: Precondition,
    ) -> Result<EdgeId, GraphError> {
        if !matches!(expected, Precondition::Revision(_)) {
            return Err(GraphError::InvalidTemporal(reason::EXPECTED));
        }
        let found: Option<&GraphEdge> = match edge {
            EdgeSelector::Id(id) => self.graph().edge(*id),
            EdgeSelector::Key(key) => {
                validate_edge_key(key)?;
                self.graph().edge_by_key(key)
            }
        };
        let current = found.map(|edge| {
            self.temporal_revisions
                .get(&edge.id)
                .copied()
                .unwrap_or(edge.revision)
        });
        check(expected, current)?;
        let (id, revision) = match (found, edge) {
            (Some(edge), _) => (edge.id, edge.revision),
            (None, EdgeSelector::Id(id)) => return Err(GraphError::EdgeNotFound(*id)),
            (None, EdgeSelector::Key(key)) => {
                return Err(GraphError::UnknownEdgeKey(key.clone()));
            }
        };
        if revision < self.sequence {
            self.temporal_revisions.entry(id).or_insert(revision);
        }
        Ok(id)
    }

    fn create_temporal_edge(
        &mut self,
        key: &Option<String>,
        source: &str,
        relationship: &str,
        destination: &str,
        segments: &[Segment],
    ) -> Result<MutationOutcome, GraphError> {
        let id = EdgeId(self.graph().next_edge_id());
        let entry = |source, destination| {
            GraphEntry::Temporal(TemporalEntry::PutEdge {
                edge_id: id,
                source,
                destination,
                relationship: relationship.into(),
                key: key.clone(),
                segments: segments.to_vec(),
            })
        };
        // Input rules first, so an invalid retry never reads as a conflict.
        entry(NodeId(0), NodeId(0)).validate()?;
        validate_node_key(source)?;
        validate_node_key(destination)?;
        let source = self.node_id(source)?;
        let destination = self.node_id(destination)?;
        if let Some(key) = key
            && let Some(existing) = self.graph().edge_by_key(key)
        {
            let identical = existing.source == source
                && existing.destination == destination
                && existing.relationship == relationship
                && self
                    .graph()
                    .segments(existing.id)
                    .is_some_and(|existing| same_series(existing, segments));
            return if identical {
                Ok(MutationOutcome::Edge {
                    id: existing.id,
                    created: false,
                })
            } else {
                Err(GraphError::EdgeKeyConflict(key.clone()))
            };
        }
        self.push(entry(source, destination))?;
        Ok(MutationOutcome::Edge { id, created: true })
    }
}

#[cfg(test)]
mod tests;
