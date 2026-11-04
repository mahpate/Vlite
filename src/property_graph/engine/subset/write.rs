//! Private write executor for `vlite-cypher-1` (Gap 6 `gap6-write-executor`).
//!
//! One statement plans one batch: MATCH bindings are materialized once at the
//! captured clock, every mutation clause is staged into one [`Planner`], and
//! the caller commits the returned body as one frame or nothing at all.

use super::super::codec::GraphBody;
use super::super::error::GraphError;
use super::super::language::LanguageError;
use super::super::model::{GraphId, Precondition};
use super::super::state::GraphState;
use super::eval::OutputValue;
use super::meter::{MeterClock, ReadBounds};
use super::plan::ParameterEntry;

/// Statement accounting reported with every staged write.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct WriteCounters {
    pub(crate) operations: usize,
    pub(crate) body_bytes: u64,
    pub(crate) reclaim_entries: usize,
    pub(crate) binding_rows: usize,
    pub(crate) detach_edges_examined: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WriteResult {
    pub(crate) columns: Vec<String>,
    pub(crate) rows: Vec<Vec<OutputValue>>,
    pub(crate) as_of_ms: Option<i64>,
    pub(crate) ttl_clock_ms: u64,
    pub(crate) view_sequence: u64,
    pub(crate) revision: Option<u64>,
    pub(crate) counters: WriteCounters,
}

/// A planned statement. `body` holds the reclaim entry (if any) and one entry
/// per staged operation; `expected` lists the precondition of each counted
/// entry in body order (`None` for creations). Commit `body` only when
/// `result.revision` is set.
#[derive(Debug)]
pub(crate) struct StagedWrite {
    pub(crate) body: GraphBody,
    pub(crate) expected: Vec<Option<Precondition>>,
    pub(crate) result: WriteResult,
}

#[derive(Debug)]
pub(crate) enum WriteError {
    Query(LanguageError),
    /// Envelope-level parameter metadata rejected before graph access.
    Metadata(GraphError),
    Graph(GraphError),
    SequenceExhausted,
}

impl From<GraphError> for WriteError {
    fn from(error: GraphError) -> Self {
        Self::Graph(error)
    }
}

/// Plans one write statement against `state` at `ttl_clock_ms` (the clock C
/// captured once by the caller). Returns `None` for a statement without
/// mutations. Planning never changes `state`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute<C: MeterClock>(
    state: &mut GraphState,
    graph_id: GraphId,
    view_sequence: u64,
    temporal_capable: bool,
    ttl_clock_ms: u64,
    text: &str,
    parameters: &[ParameterEntry],
    bounds: ReadBounds,
    clock: &C,
) -> Result<Option<StagedWrite>, WriteError> {
    let _ = (
        state,
        graph_id,
        view_sequence,
        temporal_capable,
        ttl_clock_ms,
        text,
        parameters,
        bounds,
        clock,
    );
    Err(WriteError::Graph(GraphError::InvalidQuery(
        "subset write executor unavailable",
    )))
}
