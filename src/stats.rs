//! Per-search execution counters and query-plan descriptions.
//!
//! Counters describe work performed by one search call. They never contain
//! vectors, payloads, metadata values or filter values.

/// Work counters accumulated by one search call.
///
/// All counters start at zero and saturate instead of overflowing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchStats {
    /// Live records examined by an exact scan (after candidate selection).
    pub records_scanned: u64,
    /// Full-precision f32 distance computations.
    pub distance_evaluations: u64,
    /// Quantized-code distance computations (zero unless quantized ANN is used).
    pub quantized_distance_evaluations: u64,
    /// HNSW graph nodes whose distance was evaluated during traversal.
    pub graph_nodes_visited: u64,
    /// Buffered (post-build) records merged into an ANN result.
    pub buffered_records_merged: u64,
    /// Candidates re-scored with exact distances after quantized traversal.
    pub reranked_candidates: u64,
    /// Candidate IDs produced by a metadata-index lookup.
    pub index_candidates: u64,
}

/// Strategy selected by the `auto` search planner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanStrategy {
    /// Exact scan over every live record of the collection.
    ExactScan,
    /// Exact scoring restricted to a re-verified metadata-index candidate set.
    ExactIndexed,
    /// Filter-aware traversal of a ready HNSW index.
    FilteredAnn,
    /// Unfiltered traversal of a ready HNSW index.
    Ann,
}

impl PlanStrategy {
    /// Stable snake_case name for logs and metrics.
    pub fn name(self) -> &'static str {
        match self {
            Self::ExactScan => "exact_scan",
            Self::ExactIndexed => "exact_indexed",
            Self::FilteredAnn => "filtered_ann",
            Self::Ann => "ann",
        }
    }
}

/// Description of how one planned search was executed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryPlan {
    pub strategy: PlanStrategy,
    /// Estimated candidate count from metadata indexes, when any applied.
    pub estimated_candidates: Option<u64>,
    pub live_records: u64,
    /// Indexed field whose postings produced the estimate.
    pub index_field: Option<String>,
    pub reason: &'static str,
}
