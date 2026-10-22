//! ANN quality sampling, component memory reporting, index health and the
//! `AnnOptions`-aware HNSW entry points (EPIC-007).
//!
//! Every method here uses only a ready index identified by `(config, metric,
//! options.quantization)`; none builds synchronously except the explicit
//! `build_hnsw_with_options`.

use crate::{
    AnnOptions, CollectionMut, CollectionRef, Condition, Database, DistanceMetric, Error,
    HnswConfig, HnswMaintenancePolicy, HnswMaintenanceReport, HnswMaintenanceStatus,
    HnswRebuildLimits, MAX_EF_SEARCH, Result, SearchHit, SearchStats, collection::Collection, hnsw,
    validate_ann_metric, validate_metric_query,
};
use std::collections::BTreeSet;

/// Sampled recall options. Bounds: `samples` 1..=32 (default 8), `k` 1..=100
/// (default 10), `ef_search` 1..=`MAX_EF_SEARCH` and ≥ min(k, live),
/// `max_distance_evaluations` default 50,000,000.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QualitySampleOptions {
    pub samples: usize,
    pub k: usize,
    pub ef_search: usize,
    pub seed: u64,
    pub max_distance_evaluations: u64,
}

impl Default for QualitySampleOptions {
    fn default() -> Self {
        Self {
            samples: 8,
            k: 10,
            ef_search: 100,
            seed: 0,
            max_distance_evaluations: 50_000_000,
        }
    }
}

/// Sampled recall of a ready index against exact search.
///
/// `recall_at_k` = Σ|ANN ∩ exact| / Σ|exact| over the sampled queries; `None`
/// when skipped. `skipped_reason` is `"budget"` when `samples × live × dims`
/// exceeds `max_distance_evaluations`, or `"empty"` without usable samples.
#[derive(Clone, Debug, PartialEq)]
pub struct AnnQualityReport {
    pub recall_at_k: Option<f64>,
    pub samples_used: usize,
    /// Exact plus quantized distance computations performed for sampling.
    pub distance_evaluations: u64,
    pub skipped_reason: Option<&'static str>,
}

/// Component byte counts of one ANN index. Not total process memory: the f32
/// records counted by `resident_vector_bytes` stay resident regardless of
/// quantization, and allocator/heap overhead is not included.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AnnMemoryReport {
    pub graph_nodes: u64,
    /// Neighbour-link storage (`usize` per link) of the graph.
    pub graph_link_bytes: u64,
    /// ScalarU8 codes (one byte per graph node dimension); 0 when unquantized.
    pub code_bytes: u64,
    /// ScalarU8 per-dimension min/scale (f64 each); 0 when unquantized.
    pub codebook_bytes: u64,
    /// Current f32 bytes of buffered post-build records.
    pub buffer_vector_bytes: u64,
    /// f32 bytes of every live record vector (`live × dims × 4`).
    pub resident_vector_bytes: u64,
}

/// Staleness of a ready index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HnswHealth {
    pub graph_nodes: u64,
    /// Graph nodes whose record was deleted or replaced after construction.
    pub stale_nodes: u64,
    /// IDs written after construction and scored exactly (includes new IDs).
    pub buffered_ids: u64,
}

/// SplitMix64: a small deterministic generator for seeded sampling.
fn next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn ready_index(
    db: &Database,
    collection: u16,
    config: HnswConfig,
    metric: DistanceMetric,
    options: AnnOptions,
) -> Result<(&Collection, &hnsw::Index)> {
    db.ensure_healthy()?;
    config.validate()?;
    validate_ann_metric(metric)?;
    options.validate()?;
    let state = db.state(collection);
    let index = state
        .index
        .as_ref()
        .filter(|index| index.ready_for(config, metric, options.quantization))
        .ok_or(Error::IndexNotReady)?;
    Ok((state, index))
}

impl CollectionRef<'_> {
    /// Component byte counts of the ready index; `IndexNotReady` otherwise.
    pub fn ann_memory(
        &self,
        config: HnswConfig,
        metric: DistanceMetric,
        options: AnnOptions,
    ) -> Result<AnnMemoryReport> {
        let (state, index) = ready_index(self.db, self.id, config, metric, options)?;
        let memory = index.memory();
        Ok(AnnMemoryReport {
            graph_nodes: memory.graph_nodes as u64,
            graph_link_bytes: memory.link_bytes as u64,
            code_bytes: memory.code_bytes as u64,
            codebook_bytes: memory.codebook_bytes as u64,
            buffer_vector_bytes: memory.buffer_vector_bytes as u64,
            resident_vector_bytes: (state.records.len() as u64)
                .saturating_mul(state.dimensions as u64)
                .saturating_mul(4),
        })
    }

    /// Stale-node and buffer counters of the ready index.
    pub fn hnsw_health(
        &self,
        config: HnswConfig,
        metric: DistanceMetric,
        options: AnnOptions,
    ) -> Result<HnswHealth> {
        let (_, index) = ready_index(self.db, self.id, config, metric, options)?;
        Ok(HnswHealth {
            graph_nodes: index.graph_nodes() as u64,
            stale_nodes: index.stale_nodes() as u64,
            buffered_ids: index.buffer_usage().0 as u64,
        })
    }

    /// Deterministic sampled recall@k of the ready index against exact
    /// search, using stored vectors chosen by `sample.seed` as queries.
    pub fn ann_quality(
        &self,
        config: HnswConfig,
        metric: DistanceMetric,
        options: AnnOptions,
        sample: QualitySampleOptions,
    ) -> Result<AnnQualityReport> {
        let (state, _) = ready_index(self.db, self.id, config, metric, options)?;
        if !(1..=32).contains(&sample.samples) {
            return Err(Error::InvalidQuery("quality samples must be 1..=32"));
        }
        if !(1..=100).contains(&sample.k) {
            return Err(Error::InvalidQuery("quality k must be 1..=100"));
        }
        let live = state.records.len();
        if !(1..=MAX_EF_SEARCH).contains(&sample.ef_search) || sample.ef_search < sample.k.min(live)
        {
            return Err(Error::InvalidEfSearch);
        }
        let skipped = |reason| AnnQualityReport {
            recall_at_k: None,
            samples_used: 0,
            distance_evaluations: 0,
            skipped_reason: Some(reason),
        };
        let budget = sample.samples as u128 * live as u128 * state.dimensions as u128;
        if budget > u128::from(sample.max_distance_evaluations) {
            return Ok(skipped("budget"));
        }
        let mut seed = sample.seed;
        let mut positions = BTreeSet::new();
        while positions.len() < sample.samples.min(live) {
            positions.insert((next(&mut seed) % live as u64) as usize);
        }
        let mut stats = SearchStats::default();
        let (mut found, mut expected, mut used) = (0usize, 0usize, 0usize);
        let mut sampled = positions.iter().peekable();
        for (position, record) in state.records.values().enumerate() {
            if sampled.peek() != Some(&&position) {
                continue;
            }
            sampled.next();
            let query = record.vector.as_slice();
            // Zero vectors are not valid cosine queries.
            let Ok(query_norm) = validate_metric_query(query, metric) else {
                continue;
            };
            let exact = state.exact_search_candidates(
                query,
                query_norm,
                sample.k,
                &[],
                metric,
                None,
                crate::current_time_ms(),
                &mut stats,
            );
            let approximate = self.db.search_ready_with_options_in(
                self.id,
                query,
                sample.k,
                sample.ef_search,
                config,
                options,
                &[],
                metric,
                &mut stats,
            )?;
            let truth: BTreeSet<u64> = exact.iter().map(|hit| hit.id).collect();
            found += approximate
                .iter()
                .filter(|hit| truth.contains(&hit.id))
                .count();
            expected += truth.len();
            used += 1;
        }
        if expected == 0 {
            return Ok(skipped("empty"));
        }
        Ok(AnnQualityReport {
            recall_at_k: Some(found as f64 / expected as f64),
            samples_used: used,
            distance_evaluations: stats
                .distance_evaluations
                .saturating_add(stats.quantized_distance_evaluations),
            skipped_reason: None,
        })
    }

    /// Ready-only filtered ANN with explicit `AnnOptions`; never builds.
    /// `options.rerank_multiplier` applies to this query only.
    #[allow(clippy::too_many_arguments)]
    pub fn search_approximate_ready_with_options(
        &self,
        query: &[f32],
        k: usize,
        ef_search: usize,
        config: HnswConfig,
        options: AnnOptions,
        conditions: &[Condition],
        metric: DistanceMetric,
    ) -> Result<(Vec<SearchHit>, SearchStats)> {
        let mut stats = SearchStats::default();
        let hits = self.db.search_ready_with_options_in(
            self.id, query, k, ef_search, config, options, conditions, metric, &mut stats,
        )?;
        Ok((hits, stats))
    }
}

impl CollectionMut<'_> {
    /// Synchronously builds the index identified by `(config, metric,
    /// options.quantization)`, superseding this collection's background job.
    pub fn build_hnsw_with_options(
        &mut self,
        config: HnswConfig,
        metric: DistanceMetric,
        options: AnnOptions,
    ) -> Result<()> {
        self.db
            .build_hnsw_with_options_in(self.id, config, metric, options)
    }

    /// Background rebuild with options. ScalarU8 snapshots hold u8 codes and
    /// count `dims` bytes per record against `max_snapshot_vector_bytes`.
    pub fn start_hnsw_rebuild_with_options(
        &mut self,
        config: HnswConfig,
        metric: DistanceMetric,
        options: AnnOptions,
        limits: HnswRebuildLimits,
    ) -> Result<()> {
        self.db
            .start_rebuild_with_options_in(self.id, config, metric, options, limits)
    }

    /// `hnsw_maintenance_status` for the index with `options.quantization`.
    pub fn hnsw_maintenance_status_with_options(
        &self,
        config: HnswConfig,
        metric: DistanceMetric,
        options: AnnOptions,
        policy: HnswMaintenancePolicy,
    ) -> Result<HnswMaintenanceStatus> {
        self.db
            .maintenance_status_with_options_in(self.id, config, metric, options, policy)
    }

    /// `maintain_hnsw` for the index with `options.quantization`. A pending
    /// job for a different quantization returns `HnswRebuildBusy`.
    pub fn maintain_hnsw_with_options(
        &mut self,
        config: HnswConfig,
        metric: DistanceMetric,
        options: AnnOptions,
        policy: HnswMaintenancePolicy,
    ) -> Result<HnswMaintenanceReport> {
        self.db
            .maintain_with_options_in(self.id, config, metric, options, policy)
    }

    /// See `CollectionRef::ann_memory`.
    pub fn ann_memory(
        &self,
        config: HnswConfig,
        metric: DistanceMetric,
        options: AnnOptions,
    ) -> Result<AnnMemoryReport> {
        self.view().ann_memory(config, metric, options)
    }

    /// See `CollectionRef::hnsw_health`.
    pub fn hnsw_health(
        &self,
        config: HnswConfig,
        metric: DistanceMetric,
        options: AnnOptions,
    ) -> Result<HnswHealth> {
        self.view().hnsw_health(config, metric, options)
    }

    /// See `CollectionRef::ann_quality`.
    pub fn ann_quality(
        &self,
        config: HnswConfig,
        metric: DistanceMetric,
        options: AnnOptions,
        sample: QualitySampleOptions,
    ) -> Result<AnnQualityReport> {
        self.view().ann_quality(config, metric, options, sample)
    }

    /// See `CollectionRef::search_approximate_ready_with_options`.
    #[allow(clippy::too_many_arguments)]
    pub fn search_approximate_ready_with_options(
        &self,
        query: &[f32],
        k: usize,
        ef_search: usize,
        config: HnswConfig,
        options: AnnOptions,
        conditions: &[Condition],
        metric: DistanceMetric,
    ) -> Result<(Vec<SearchHit>, SearchStats)> {
        self.view().search_approximate_ready_with_options(
            query, k, ef_search, config, options, conditions, metric,
        )
    }
}

/// Default-collection delegates.
impl Database {
    fn default_collection(&self) -> CollectionRef<'_> {
        CollectionRef { db: self, id: 0 }
    }

    fn default_collection_mut(&mut self) -> CollectionMut<'_> {
        CollectionMut { db: self, id: 0 }
    }

    /// See `CollectionMut::build_hnsw_with_options`.
    pub fn build_hnsw_with_options(
        &mut self,
        config: HnswConfig,
        metric: DistanceMetric,
        options: AnnOptions,
    ) -> Result<()> {
        self.default_collection_mut()
            .build_hnsw_with_options(config, metric, options)
    }

    /// See `CollectionMut::start_hnsw_rebuild_with_options`.
    pub fn start_hnsw_rebuild_with_options(
        &mut self,
        config: HnswConfig,
        metric: DistanceMetric,
        options: AnnOptions,
        limits: HnswRebuildLimits,
    ) -> Result<()> {
        self.default_collection_mut()
            .start_hnsw_rebuild_with_options(config, metric, options, limits)
    }

    /// See `CollectionMut::hnsw_maintenance_status_with_options`.
    pub fn hnsw_maintenance_status_with_options(
        &self,
        config: HnswConfig,
        metric: DistanceMetric,
        options: AnnOptions,
        policy: HnswMaintenancePolicy,
    ) -> Result<HnswMaintenanceStatus> {
        self.maintenance_status_with_options_in(0, config, metric, options, policy)
    }

    /// See `CollectionMut::maintain_hnsw_with_options`.
    pub fn maintain_hnsw_with_options(
        &mut self,
        config: HnswConfig,
        metric: DistanceMetric,
        options: AnnOptions,
        policy: HnswMaintenancePolicy,
    ) -> Result<HnswMaintenanceReport> {
        self.default_collection_mut()
            .maintain_hnsw_with_options(config, metric, options, policy)
    }

    /// See `CollectionRef::ann_memory`.
    pub fn ann_memory(
        &self,
        config: HnswConfig,
        metric: DistanceMetric,
        options: AnnOptions,
    ) -> Result<AnnMemoryReport> {
        self.default_collection()
            .ann_memory(config, metric, options)
    }

    /// See `CollectionRef::hnsw_health`.
    pub fn hnsw_health(
        &self,
        config: HnswConfig,
        metric: DistanceMetric,
        options: AnnOptions,
    ) -> Result<HnswHealth> {
        self.default_collection()
            .hnsw_health(config, metric, options)
    }

    /// See `CollectionRef::ann_quality`.
    pub fn ann_quality(
        &self,
        config: HnswConfig,
        metric: DistanceMetric,
        options: AnnOptions,
        sample: QualitySampleOptions,
    ) -> Result<AnnQualityReport> {
        self.default_collection()
            .ann_quality(config, metric, options, sample)
    }

    /// See `CollectionRef::search_approximate_ready_with_options`.
    #[allow(clippy::too_many_arguments)]
    pub fn search_approximate_ready_with_options(
        &self,
        query: &[f32],
        k: usize,
        ef_search: usize,
        config: HnswConfig,
        options: AnnOptions,
        conditions: &[Condition],
        metric: DistanceMetric,
    ) -> Result<(Vec<SearchHit>, SearchStats)> {
        self.default_collection()
            .search_approximate_ready_with_options(
                query, k, ef_search, config, options, conditions, metric,
            )
    }
}
