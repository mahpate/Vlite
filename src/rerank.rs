//! Candidate-only re-ranking; pluggable cross-encoders implement `Reranker`.
use crate::{DistanceMetric, Error, Result, SearchHit};
#[derive(Clone, Debug)]
pub struct RerankCandidate {
    pub id: u64,
    pub vector: Vec<f32>,
    pub payload: Vec<u8>,
    pub distance: f64,
}
pub trait Reranker: Send + Sync {
    fn rerank(
        &self,
        query: &[f32],
        candidates: &[RerankCandidate],
        limit: usize,
    ) -> Result<Vec<SearchHit>>;
}
pub struct ExactReranker {
    pub metric: DistanceMetric,
}
impl Reranker for ExactReranker {
    fn rerank(
        &self,
        query: &[f32],
        candidates: &[RerankCandidate],
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        crate::validate_dimensions(query.len())?;
        crate::validate_vector(query.len(), query)?;
        let norm = crate::validate_metric_query(query, self.metric)?;
        let mut hits = Vec::new();
        let mut ids = std::collections::BTreeSet::new();
        for c in candidates {
            if !ids.insert(c.id) {
                return Err(Error::InvalidQuery("duplicate rerank candidate"));
            }
            crate::validate_vector(query.len(), &c.vector)?;
            if let Some(distance) = crate::metric_distance(
                query,
                &c.vector,
                self.metric,
                norm,
                crate::vector_norm(&c.vector),
            ) {
                hits.push(SearchHit { id: c.id, distance });
            }
        }
        hits.sort_by(|a, b| a.distance.total_cmp(&b.distance).then(a.id.cmp(&b.id)));
        hits.truncate(limit);
        Ok(hits)
    }
}
/// Maximal marginal relevance using cosine similarity (lambda=1 means relevance only).
pub struct MmrReranker {
    pub lambda: f64,
}
impl Reranker for MmrReranker {
    fn rerank(
        &self,
        query: &[f32],
        candidates: &[RerankCandidate],
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        if !self.lambda.is_finite() || !(0.0..=1.0).contains(&self.lambda) {
            return Err(Error::InvalidQuery("MMR lambda must be 0..=1"));
        }
        let hits = ExactReranker {
            metric: DistanceMetric::Cosine,
        }
        .rerank(query, candidates, candidates.len())?;
        let vectors: std::collections::BTreeMap<_, _> =
            candidates.iter().map(|c| (c.id, &c.vector)).collect();
        let mut remaining = hits;
        let mut selected: Vec<SearchHit> = Vec::new();
        while selected.len() < limit && !remaining.is_empty() {
            let mut best = 0;
            let mut best_score = f64::NEG_INFINITY;
            for (i, c) in remaining.iter().enumerate() {
                let v = vectors[&c.id];
                let redundancy = selected
                    .iter()
                    .map(|s| {
                        let other = vectors[&s.id];
                        1.0 - crate::metric_distance(
                            v,
                            other,
                            DistanceMetric::Cosine,
                            crate::vector_norm(v),
                            crate::vector_norm(other),
                        )
                        .unwrap_or(1.0)
                    })
                    .reduce(f64::max)
                    .unwrap_or(0.0);
                let score = self.lambda * (1.0 - c.distance) - (1.0 - self.lambda) * redundancy;
                if score > best_score || (score == best_score && c.id < remaining[best].id) {
                    best = i;
                    best_score = score;
                }
            }
            selected.push(remaining.remove(best));
        }
        Ok(selected)
    }
}
impl crate::CollectionRef<'_> {
    /// Fetches full-precision vectors for a bounded top-N list, then invokes the provider.
    pub fn rerank(
        &self,
        query: &[f32],
        hits: &[SearchHit],
        limit: usize,
        reranker: &dyn Reranker,
    ) -> Result<Vec<SearchHit>> {
        if hits.len() > crate::MAX_BATCH_OPERATIONS {
            return Err(Error::BatchTooLarge);
        }
        let mut candidates = Vec::new();
        for hit in hits {
            if let Some(record) = self.get(hit.id)? {
                candidates.push(RerankCandidate {
                    id: hit.id,
                    vector: record.vector.clone(),
                    payload: record.payload.clone(),
                    distance: hit.distance,
                });
            }
        }
        reranker.rerank(query, &candidates, limit)
    }
}
