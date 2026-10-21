//! Hybrid lexical + vector search: BM25 and vector legs fused by weighted
//! Reciprocal Rank Fusion. The vector leg never builds an index.

use crate::{
    AnnOptions, CollectionMut, CollectionRef, Condition, Database, DistanceMetric, Error,
    HnswConfig, Result, SearchStats, metadata, text_index::tokenize,
};
use std::collections::{BTreeMap, BTreeSet};

/// Maximum hybrid `k` and fused leg depth.
pub const MAX_HYBRID_K: usize = 1000;
/// Maximum hybrid text bytes.
pub const MAX_HYBRID_TEXT_BYTES: usize = 1024;
/// Maximum unique query tokens.
pub const MAX_HYBRID_TOKENS: usize = 32;

/// How the vector leg of a hybrid query is evaluated. Never builds an index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VectorLeg {
    /// Exact search (uses metadata-index candidates when declared).
    Exact,
    /// Ready-only ANN over the index identified by `(config, metric,
    /// options.quantization)`; `IndexNotReady` otherwise. `ef_search` must be
    /// at least `min(candidates, live)`.
    ApproximateReady {
        config: HnswConfig,
        ef_search: usize,
        options: AnnOptions,
    },
}

/// Weighted Reciprocal Rank Fusion options.
///
/// Bounds: `candidates` k..=1000 (default `max(k, 50)` when None), `rrf_k`
/// 1..=1000 (default 60), weights finite 0..=10 and not both zero (default 1).
/// A leg with weight 0 is not evaluated.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FusionOptions {
    pub candidates: Option<usize>,
    pub rrf_k: u32,
    pub vector_weight: f64,
    pub text_weight: f64,
}

impl Default for FusionOptions {
    fn default() -> Self {
        Self {
            candidates: None,
            rrf_k: 60,
            vector_weight: 1.0,
            text_weight: 1.0,
        }
    }
}

/// One hybrid query (text ≤1,024 bytes and ≤32 unique tokens; k ≤1000).
///
/// At least one of a vector or a text with tokens is required. `conditions`
/// filter both legs. `metric` applies to the vector leg.
#[derive(Clone, Debug, PartialEq)]
pub struct HybridQuery {
    pub vector: Option<Vec<f32>>,
    pub text: String,
    pub k: usize,
    pub conditions: Vec<Condition>,
    pub metric: DistanceMetric,
    pub vector_leg: VectorLeg,
    pub fusion: FusionOptions,
}

impl HybridQuery {
    /// Exact squared-L2 vector leg with default fusion and no filter.
    pub fn new(vector: Option<Vec<f32>>, text: impl Into<String>, k: usize) -> Self {
        Self {
            vector,
            text: text.into(),
            k,
            conditions: Vec::new(),
            metric: DistanceMetric::SquaredL2,
            vector_leg: VectorLeg::Exact,
            fusion: FusionOptions::default(),
        }
    }
}

/// One fused result, ordered by `score` descending then `id` ascending.
///
/// `score = Σ weight / (rrf_k + rank)` over the legs (1-based ranks) that
/// returned the record.
#[derive(Clone, Debug, PartialEq)]
pub struct HybridHit {
    pub id: u64,
    pub score: f64,
    pub distance: Option<f64>,
    pub bm25: Option<f64>,
    pub vector_rank: Option<u32>,
    pub text_rank: Option<u32>,
}

fn validate_weight(weight: f64) -> Result<()> {
    if weight.is_finite() && (0.0..=10.0).contains(&weight) {
        Ok(())
    } else {
        Err(Error::InvalidQuery("fusion weights must be finite 0..=10"))
    }
}

impl CollectionRef<'_> {
    /// Hybrid search per REQ-008. Stats count the vector leg's work.
    pub fn search_hybrid(&self, query: &HybridQuery) -> Result<(Vec<HybridHit>, SearchStats)> {
        let db = self.db;
        db.ensure_healthy()?;
        let fusion = query.fusion;
        if query.k > MAX_HYBRID_K {
            return Err(Error::InvalidQuery("hybrid k must be at most 1000"));
        }
        let candidates = fusion.candidates.unwrap_or(query.k.max(50));
        if !(query.k..=MAX_HYBRID_K).contains(&candidates) {
            return Err(Error::InvalidQuery("fusion candidates must be k..=1000"));
        }
        if !(1..=1000).contains(&fusion.rrf_k) {
            return Err(Error::InvalidQuery("rrf_k must be 1..=1000"));
        }
        validate_weight(fusion.vector_weight)?;
        validate_weight(fusion.text_weight)?;
        if fusion.vector_weight == 0.0 && fusion.text_weight == 0.0 {
            return Err(Error::InvalidQuery("fusion weights must not both be zero"));
        }
        if query.text.len() > MAX_HYBRID_TEXT_BYTES {
            return Err(Error::InvalidQuery("hybrid text exceeds 1024 bytes"));
        }
        let terms: BTreeSet<String> = tokenize(&query.text).into_iter().collect();
        if terms.len() > MAX_HYBRID_TOKENS {
            return Err(Error::InvalidQuery("hybrid text exceeds 32 unique tokens"));
        }
        if query.vector.is_none() && terms.is_empty() {
            return Err(Error::InvalidQuery(
                "hybrid search needs a vector or text tokens",
            ));
        }
        metadata::validate_filter(&query.conditions)?;
        let state = db.state(self.id);
        // A zero-weight text leg is disabled and never consults the index.
        if fusion.text_weight > 0.0 && !terms.is_empty() {
            let status = state.text_index.status();
            if status.declared_fields == 0 {
                return Err(Error::InvalidQuery(
                    "text search requires declared text_fields",
                ));
            }
            if !status.available {
                return Err(Error::IndexNotReady);
            }
        }
        if let VectorLeg::ApproximateReady { options, .. } = query.vector_leg {
            options.validate()?;
        }
        let mut stats = SearchStats::default();
        let vector_hits = match &query.vector {
            Some(vector) if fusion.vector_weight > 0.0 => match query.vector_leg {
                VectorLeg::Exact => db.search_in(
                    self.id,
                    vector,
                    candidates,
                    &query.conditions,
                    query.metric,
                    &mut stats,
                )?,
                VectorLeg::ApproximateReady {
                    config,
                    ef_search,
                    options,
                } => db.search_ready_with_options_in(
                    self.id,
                    vector,
                    candidates,
                    ef_search,
                    config,
                    options,
                    &query.conditions,
                    query.metric,
                    &mut stats,
                )?,
            },
            _ => Vec::new(),
        };
        if query.k == 0 {
            return Ok((Vec::new(), stats));
        }
        let text_hits = if fusion.text_weight > 0.0 && !terms.is_empty() {
            state.text_index.search(&terms, candidates, |id| {
                state
                    .records
                    .get(&id)
                    .is_some_and(|record| metadata::matches(&record.metadata, &query.conditions))
            })
        } else {
            Vec::new()
        };
        let rrf_k = f64::from(fusion.rrf_k);
        let mut fused: BTreeMap<u64, HybridHit> = BTreeMap::new();
        for (position, hit) in vector_hits.iter().enumerate() {
            let rank = position as u32 + 1;
            let fused_hit = fused.entry(hit.id).or_insert_with(|| empty_hit(hit.id));
            fused_hit.score += fusion.vector_weight / (rrf_k + f64::from(rank));
            fused_hit.distance = Some(hit.distance);
            fused_hit.vector_rank = Some(rank);
        }
        for (position, &(id, score)) in text_hits.iter().enumerate() {
            let rank = position as u32 + 1;
            let fused_hit = fused.entry(id).or_insert_with(|| empty_hit(id));
            fused_hit.score += fusion.text_weight / (rrf_k + f64::from(rank));
            fused_hit.bm25 = Some(score);
            fused_hit.text_rank = Some(rank);
        }
        let mut hits: Vec<HybridHit> = fused.into_values().collect();
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
        hits.truncate(query.k);
        Ok((hits, stats))
    }
}

fn empty_hit(id: u64) -> HybridHit {
    HybridHit {
        id,
        score: 0.0,
        distance: None,
        bm25: None,
        vector_rank: None,
        text_rank: None,
    }
}

impl CollectionMut<'_> {
    /// See `CollectionRef::search_hybrid`.
    pub fn search_hybrid(&self, query: &HybridQuery) -> Result<(Vec<HybridHit>, SearchStats)> {
        self.view().search_hybrid(query)
    }
}

impl Database {
    /// Hybrid search of the default collection; see `CollectionRef::search_hybrid`.
    pub fn search_hybrid(&self, query: &HybridQuery) -> Result<(Vec<HybridHit>, SearchStats)> {
        CollectionRef { db: self, id: 0 }.search_hybrid(query)
    }
}
