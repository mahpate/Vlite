//! Query planner policy and the opt-in `auto` search mode.
//!
//! `auto` chooses between an exact scan, exact scoring over a re-verified
//! metadata-index candidate set, and ready-only (filtered) HNSW traversal. It
//! never builds or rebuilds an index, and existing search methods keep their
//! explicit semantics.

use crate::{
    AnnOptions, CollectionMut, CollectionRef, Condition, Database, DistanceMetric, Error,
    HnswConfig, MAX_EF_SEARCH, PlanStrategy, QueryPlan, Result, SearchHit, SearchStats, metadata,
    validate_metric_query, validate_vector,
};
use std::collections::BTreeSet;

/// Thresholds for choosing exact-over-candidates versus filtered ANN.
///
/// `exact_max_candidates` is valid in `1..=1_000_000` and
/// `exact_max_selectivity_ppm` in `0..=1_000_000`. A filtered `auto` query is
/// answered exactly over index candidates when the estimate is at most
/// `max(exact_max_candidates, floor(exact_max_selectivity_ppm × live / 1e6))`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlannerPolicy {
    pub exact_max_candidates: u32,
    pub exact_max_selectivity_ppm: u32,
}

impl Default for PlannerPolicy {
    fn default() -> Self {
        Self {
            exact_max_candidates: 4096,
            exact_max_selectivity_ppm: 20_000,
        }
    }
}

impl PlannerPolicy {
    pub(crate) fn validate(self) -> std::result::Result<(), &'static str> {
        if !(1..=1_000_000).contains(&self.exact_max_candidates) {
            return Err("planner exact_max_candidates must be 1..=1000000");
        }
        if self.exact_max_selectivity_ppm > 1_000_000 {
            return Err("planner exact_max_selectivity_ppm must be 0..=1000000");
        }
        Ok(())
    }

    /// Largest candidate estimate answered exactly for `live` records.
    pub fn exact_limit(self, live: usize) -> usize {
        let ratio = u128::from(self.exact_max_selectivity_ppm) * live as u128 / 1_000_000;
        let limit = ratio.max(u128::from(self.exact_max_candidates));
        usize::try_from(limit).unwrap_or(usize::MAX)
    }
}

/// Result of an `auto` search: hits, the executed plan and work counters.
#[derive(Clone, Debug, PartialEq)]
pub struct PlannedSearch {
    pub hits: Vec<SearchHit>,
    pub plan: QueryPlan,
    pub stats: SearchStats,
}

impl<'a> CollectionRef<'a> {
    /// Planned search per the collection's `PlannerPolicy` (Section 3.3).
    ///
    /// Without a filter, a ready index whose identity is `(config, metric,
    /// options.quantization)` gives `Ann`; otherwise `ExactScan`. With a
    /// filter, a metadata-index estimate within the exact limit gives
    /// `ExactIndexed` (candidates re-verified; results identical to exact
    /// filtered search); otherwise a ready index gives `FilteredAnn` and
    /// anything else `ExactScan`. Unindexed filters count as exceeding the
    /// limit. `InnerProduct` and empty collections always plan exactly.
    /// Never builds an index. `ef_search` is validated (1..=`MAX_EF_SEARCH`
    /// and ≥ min(k, live)) regardless of the chosen strategy, so validity does
    /// not depend on index state.
    #[allow(clippy::too_many_arguments)]
    pub fn search_auto(
        &self,
        query: &[f32],
        k: usize,
        conditions: &[Condition],
        metric: DistanceMetric,
        config: HnswConfig,
        options: AnnOptions,
        ef_search: usize,
    ) -> Result<PlannedSearch> {
        let db: &'a Database = self.db;
        db.ensure_healthy()?;
        let state = db.state(self.id);
        validate_vector(state.dimensions, query)?;
        metadata::validate_filter(conditions)?;
        let query_norm = validate_metric_query(query, metric)?;
        config.validate()?;
        options.validate()?;
        let live = state.records.len();
        if !(1..=MAX_EF_SEARCH).contains(&ef_search) || ef_search < k.min(live) {
            return Err(Error::InvalidEfSearch);
        }
        let ready = metric != DistanceMetric::InnerProduct
            && live > 0
            && state
                .index
                .as_ref()
                .is_some_and(|index| index.ready_for(config, metric, options.quantization));
        let mut stats = SearchStats::default();
        let mut plan = QueryPlan {
            strategy: PlanStrategy::ExactScan,
            estimated_candidates: None,
            live_records: live as u64,
            index_field: None,
            reason: "",
        };
        let ann = |stats: &mut SearchStats| {
            db.search_ready_with_options_in(
                self.id, query, k, ef_search, config, options, conditions, metric, stats,
            )
        };
        let exact = |stats: &mut SearchStats, candidates: Option<&BTreeSet<u64>>| {
            state.exact_search_candidates(
                query,
                query_norm,
                k,
                conditions,
                metric,
                candidates,
                crate::current_time_ms(),
                stats,
            )
        };
        let hits = if conditions.is_empty() {
            if ready {
                plan.strategy = PlanStrategy::Ann;
                plan.reason = "unfiltered query with a ready index";
                ann(&mut stats)?
            } else {
                plan.reason = no_ann_reason(metric, live);
                exact(&mut stats, None)
            }
        } else {
            let limit = state.settings.planner.exact_limit(live);
            let lookup = state.metadata_indexes.lookup(conditions, limit);
            if let Some(lookup) = &lookup {
                plan.estimated_candidates = Some(lookup.estimate);
                plan.index_field = Some(lookup.field.clone());
            }
            match lookup.and_then(|lookup| lookup.ids) {
                Some(ids) => {
                    plan.strategy = PlanStrategy::ExactIndexed;
                    plan.reason = if live == 0 {
                        "empty collection"
                    } else {
                        "estimated candidates within the exact limit"
                    };
                    stats.index_candidates = ids.len() as u64;
                    exact(&mut stats, Some(&ids))
                }
                None if ready => {
                    plan.strategy = PlanStrategy::FilteredAnn;
                    plan.reason = if plan.estimated_candidates.is_some() {
                        "estimated candidates exceed the exact limit; ready index"
                    } else {
                        "no indexed filter condition; ready index"
                    };
                    ann(&mut stats)?
                }
                None => {
                    plan.reason = no_ann_reason(metric, live);
                    exact(&mut stats, None)
                }
            }
        };
        Ok(PlannedSearch { hits, plan, stats })
    }

    /// `search_auto` using the collection's persisted settings for metric,
    /// HNSW configuration, quantization and rerank multiplier.
    pub fn search_auto_with_settings(
        &self,
        query: &[f32],
        k: usize,
        conditions: &[Condition],
        ef_search: usize,
    ) -> Result<PlannedSearch> {
        let settings = self.db.state(self.id).settings.clone();
        self.search_auto(
            query,
            k,
            conditions,
            settings.metric,
            settings.hnsw,
            AnnOptions {
                quantization: settings.quantization,
                rerank_multiplier: settings.rerank_multiplier,
            },
            ef_search,
        )
    }
}

fn no_ann_reason(metric: DistanceMetric, live: usize) -> &'static str {
    if live == 0 {
        "empty collection"
    } else if metric == DistanceMetric::InnerProduct {
        "metric has no ANN support"
    } else {
        "no ready index for this config, metric and quantization"
    }
}

impl CollectionMut<'_> {
    /// See `CollectionRef::search_auto`.
    #[allow(clippy::too_many_arguments)]
    pub fn search_auto(
        &self,
        query: &[f32],
        k: usize,
        conditions: &[Condition],
        metric: DistanceMetric,
        config: HnswConfig,
        options: AnnOptions,
        ef_search: usize,
    ) -> Result<PlannedSearch> {
        self.view()
            .search_auto(query, k, conditions, metric, config, options, ef_search)
    }

    /// See `CollectionRef::search_auto_with_settings`.
    pub fn search_auto_with_settings(
        &self,
        query: &[f32],
        k: usize,
        conditions: &[Condition],
        ef_search: usize,
    ) -> Result<PlannedSearch> {
        self.view()
            .search_auto_with_settings(query, k, conditions, ef_search)
    }
}

impl Database {
    /// `auto` search of the default collection; see `CollectionRef::search_auto`.
    #[allow(clippy::too_many_arguments)]
    pub fn search_auto(
        &self,
        query: &[f32],
        k: usize,
        conditions: &[Condition],
        metric: DistanceMetric,
        config: HnswConfig,
        options: AnnOptions,
        ef_search: usize,
    ) -> Result<PlannedSearch> {
        CollectionRef { db: self, id: 0 }
            .search_auto(query, k, conditions, metric, config, options, ef_search)
    }
}
