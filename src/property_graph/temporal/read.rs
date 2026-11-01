//! Private, gated slice C1 reads. C2 operations and cursor continuation are
//! intentionally not exposed by this view.

#![cfg_attr(not(test), allow(dead_code))]

use std::ops::Range;

use crate::format::FLAG_TEMPORAL;
use crate::{Error, Result};

use super::{TemporalSegment, segment_out};
use crate::property_graph::engine;
use crate::property_graph::read::{EdgeView, GraphRead, NodeView};
use crate::property_graph::{EdgeRef, FindNodes, GraphRef, Neighbors, ReadOptions};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TemporalRead<T> {
    pub(crate) read: GraphRead<T>,
    pub(crate) as_of_ms: i64,
    pub(crate) ttl_clock_ms: u64,
}

/// `None` validity identifies a legacy edge; open temporal bounds are
/// `Some((None, None))`, not a legacy edge.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TemporalEdgeView {
    pub(crate) edge: EdgeView,
    pub(crate) validity: Option<(Option<i64>, Option<i64>)>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TemporalNeighborhood {
    pub(crate) edges: Vec<TemporalEdgeView>,
    pub(crate) nodes: Vec<NodeView>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Timeline {
    pub(crate) edge_id: u64,
    pub(crate) revision: u64,
    pub(crate) segments: Vec<TemporalSegment>,
    pub(crate) bounds: Range<usize>,
    pub(crate) total_segments: usize,
}

pub(crate) struct TemporalReadView<'db> {
    graph: GraphRef<'db>,
}

pub(crate) fn resolve_as_of(as_of: Option<i64>, clock: u64) -> Result<i64> {
    as_of.map_or_else(
        || {
            i64::try_from(clock).map_err(|_| {
                Error::InvalidQuery(
                    "TTL clock does not fit a default validity instant; specify as_of",
                )
            })
        },
        Ok,
    )
}

impl<'db> GraphRef<'db> {
    fn temporal_view(&self, as_of_ms: Option<i64>) -> Result<TemporalReadView<'db>> {
        if !super::supported() || self.db.flags & FLAG_TEMPORAL == 0 {
            return Err(Error::UnsupportedFeature(FLAG_TEMPORAL));
        }
        Ok(TemporalReadView {
            graph: GraphRef { as_of_ms, ..*self },
        })
    }

    pub(crate) fn as_of(&self, as_of_ms: i64) -> Result<TemporalReadView<'db>> {
        self.temporal_view(Some(as_of_ms))
    }

    /// Resolve default T=C once per operation, never when making the view.
    pub(crate) fn temporal_read(&self) -> Result<TemporalReadView<'db>> {
        self.temporal_view(None)
    }
}

impl<'db> TemporalReadView<'db> {
    pub(crate) fn with_clock(&self, clock: u64) -> Self {
        Self {
            graph: self.graph.with_clock(clock),
        }
    }

    fn captured(&self) -> Result<(GraphRef<'db>, i64, u64)> {
        if !super::supported() {
            return Err(Error::UnsupportedFeature(FLAG_TEMPORAL));
        }
        let clock = self.graph.now();
        let as_of = resolve_as_of(self.graph.as_of_ms, clock)?;
        Ok((self.graph.with_clock(clock), as_of, clock))
    }

    fn wrap<T>(read: GraphRead<T>, as_of_ms: i64, ttl_clock_ms: u64) -> TemporalRead<T> {
        TemporalRead {
            read,
            as_of_ms,
            ttl_clock_ms,
        }
    }

    fn edge(&self, edge: EdgeView, as_of: i64) -> Result<TemporalEdgeView> {
        let validity = self
            .graph
            .graph
            .segments(engine::EdgeId(edge.id))
            .map(|series| {
                engine::temporal::segment_at(series, as_of)
                    .map(|segment| (segment.valid_from, segment.valid_to))
                    .ok_or(Error::InvalidQuery(
                        "temporal read returned no matching segment",
                    ))
            })
            .transpose()?;
        Ok(TemporalEdgeView { edge, validity })
    }

    fn get_edge_by(&self, edge: EdgeRef) -> Result<TemporalRead<Option<TemporalEdgeView>>> {
        let (graph, as_of, clock) = self.captured()?;
        let mut read = match edge {
            EdgeRef::Id(id) => graph.get_edge(id)?,
            EdgeRef::Key(key) => graph.get_edge_by_key(&key)?,
        };
        let output = read
            .output
            .take()
            .map(|edge| self.edge(edge, as_of))
            .transpose()?;
        Ok(Self::wrap(
            GraphRead {
                output,
                complete: read.complete,
                truncation: read.truncation,
                view_sequence: read.view_sequence,
                stats: read.stats,
                cursor: read.cursor,
            },
            as_of,
            clock,
        ))
    }

    pub(crate) fn get_edge(&self, id: u64) -> Result<TemporalRead<Option<TemporalEdgeView>>> {
        self.get_edge_by(EdgeRef::Id(id))
    }

    pub(crate) fn get_edge_by_key(
        &self,
        key: &str,
    ) -> Result<TemporalRead<Option<TemporalEdgeView>>> {
        self.get_edge_by(EdgeRef::Key(key.into()))
    }

    pub(crate) fn get_node(&self, key: &str) -> Result<TemporalRead<Option<NodeView>>> {
        let (graph, as_of, clock) = self.captured()?;
        Ok(Self::wrap(graph.get_node(key)?, as_of, clock))
    }

    pub(crate) fn get_node_by_id(&self, id: u64) -> Result<TemporalRead<Option<NodeView>>> {
        let (graph, as_of, clock) = self.captured()?;
        Ok(Self::wrap(graph.get_node_by_id(id)?, as_of, clock))
    }

    pub(crate) fn find_nodes(
        &self,
        request: &FindNodes,
        options: &ReadOptions<'_>,
    ) -> Result<TemporalRead<Vec<NodeView>>> {
        let (graph, as_of, clock) = self.captured()?;
        Ok(Self::wrap(
            graph.find_nodes(request, options)?,
            as_of,
            clock,
        ))
    }

    pub(crate) fn neighbors(
        &self,
        request: &Neighbors,
        options: &ReadOptions<'_>,
    ) -> Result<TemporalRead<TemporalNeighborhood>> {
        let (graph, as_of, clock) = self.captured()?;
        let read = graph.neighbors(request, options)?;
        let edges = read
            .output
            .edges
            .into_iter()
            .map(|edge| self.edge(edge, as_of))
            .collect::<Result<_>>()?;
        Ok(Self::wrap(
            GraphRead {
                output: TemporalNeighborhood {
                    edges,
                    nodes: read.output.nodes,
                },
                complete: read.complete,
                truncation: read.truncation,
                view_sequence: read.view_sequence,
                stats: read.stats,
                cursor: read.cursor,
            },
            as_of,
            clock,
        ))
    }

    /// Inspect retained segments by explicit index bounds; validity is
    /// ignored, but edge and endpoint TTL are never bypassed.
    pub(crate) fn timeline(
        &self,
        edge: EdgeRef,
        bounds: Range<usize>,
    ) -> Result<TemporalRead<Option<Timeline>>> {
        if bounds.start > bounds.end || bounds.end > engine::temporal::MAX_SEGMENTS {
            return Err(Error::InvalidQuery(
                "timeline bounds must satisfy start <= end <= 256",
            ));
        }
        let (graph, as_of, clock) = self.captured()?;
        let edge = match edge {
            EdgeRef::Id(id) => graph.graph.edge(engine::EdgeId(id)),
            EdgeRef::Key(key) => {
                engine::model::validate_edge_key(&key)
                    .map_err(crate::property_graph::convert::graph_error)?;
                graph.graph.edge_by_key(&key)
            }
        }
        .filter(|edge| engine::temporal_visibility::edge_retained_at(graph.graph, edge, clock));
        let output = edge
            .map(|edge| -> Result<Timeline> {
                let series = graph
                    .graph
                    .segments(edge.id)
                    .ok_or(Error::InvalidQuery("timeline requires a temporal edge"))?;
                let start = bounds.start.min(series.len());
                let end = bounds.end.min(series.len());
                Ok(Timeline {
                    edge_id: edge.id.0,
                    revision: edge.revision,
                    segments: series[start..end].iter().map(segment_out).collect(),
                    bounds,
                    total_segments: series.len(),
                })
            })
            .transpose()?;
        Ok(Self::wrap(
            GraphRead {
                output,
                complete: true,
                truncation: Vec::new(),
                view_sequence: graph.view_sequence,
                stats: Default::default(),
                cursor: None,
            },
            as_of,
            clock,
        ))
    }
}

#[cfg(test)]
mod tests;
