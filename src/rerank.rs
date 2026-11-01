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
        let mut ids = std::collections::BTreeSet::new();
        for c in candidates {
            if !ids.insert(c.id) {
                return Err(Error::InvalidQuery("duplicate rerank candidate"));
            }
            crate::validate_vector(query.len(), &c.vector)?;
        }
        // Finite operands score identically in either order (exact_kernels).
        let rows: Vec<&[f32]> = candidates.iter().map(|c| &c.vector[..]).collect();
        let distances =
            crate::exact_kernels::Scorer::new(query, self.metric, norm).score(&rows, None);
        let mut hits: Vec<SearchHit> = candidates
            .iter()
            .zip(distances)
            .filter_map(|(c, distance)| distance.map(|distance| SearchHit { id: c.id, distance }))
            .collect();
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
        let norms: std::collections::BTreeMap<_, _> = candidates
            .iter()
            .map(|c| (c.id, crate::vector_norm(&c.vector)))
            .collect();
        let mut remaining = hits;
        let mut selected: Vec<SearchHit> = Vec::new();
        while selected.len() < limit && !remaining.is_empty() {
            let mut redundancies = vec![
                if selected.is_empty() {
                    0.0
                } else {
                    f64::NEG_INFINITY
                };
                remaining.len()
            ];
            for selected_hit in &selected {
                let query = vectors[&selected_hit.id];
                let query_norm = norms[&selected_hit.id];
                let rows: Vec<&[f32]> = remaining
                    .iter()
                    .map(|candidate| vectors[&candidate.id].as_slice())
                    .collect();
                let row_norms: Vec<f64> = remaining
                    .iter()
                    .map(|candidate| norms[&candidate.id])
                    .collect();
                let distances =
                    crate::exact_kernels::Scorer::new(query, DistanceMetric::Cosine, query_norm)
                        .score(&rows, Some(&row_norms));
                for (redundancy, distance) in redundancies.iter_mut().zip(distances) {
                    // This is the original row-first call: metric_distance(v, query).
                    let similarity = 1.0 - distance.unwrap_or(1.0);
                    *redundancy = redundancy.max(similarity);
                }
            }
            let mut best = 0;
            let mut best_score = f64::NEG_INFINITY;
            for (i, c) in remaining.iter().enumerate() {
                let score =
                    self.lambda * (1.0 - c.distance) - (1.0 - self.lambda) * redundancies[i];
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exact_kernels::{self, Kernel};

    fn kernels() -> Vec<Kernel> {
        let mut kernels = vec![Kernel::Portable];
        #[cfg(target_arch = "x86_64")]
        if std::arch::is_x86_feature_detected!("avx2") {
            kernels.push(Kernel::Avx2);
        }
        kernels
    }

    fn oracle_rerank(
        query: &[f32],
        candidates: &[RerankCandidate],
        limit: usize,
        metric: DistanceMetric,
    ) -> Vec<SearchHit> {
        let query_norm = if metric == DistanceMetric::Cosine {
            crate::vector_norm(query)
        } else {
            0.0
        };
        let mut hits: Vec<_> = candidates
            .iter()
            .filter_map(|candidate| {
                crate::metric_distance(
                    &candidate.vector,
                    query,
                    metric,
                    crate::vector_norm(&candidate.vector),
                    query_norm,
                )
                .map(|distance| SearchHit {
                    id: candidate.id,
                    distance,
                })
            })
            .collect();
        hits.sort_by(|a, b| a.distance.total_cmp(&b.distance).then(a.id.cmp(&b.id)));
        hits.truncate(limit);
        hits
    }

    fn oracle_mmr(
        query: &[f32],
        candidates: &[RerankCandidate],
        limit: usize,
        lambda: f64,
    ) -> Vec<SearchHit> {
        let mut remaining =
            oracle_rerank(query, candidates, candidates.len(), DistanceMetric::Cosine);
        let vectors: std::collections::BTreeMap<_, _> = candidates
            .iter()
            .map(|candidate| (candidate.id, candidate.vector.as_slice()))
            .collect();
        let mut selected: Vec<SearchHit> = Vec::new();
        while selected.len() < limit && !remaining.is_empty() {
            let mut best = 0;
            let mut best_score = f64::NEG_INFINITY;
            for (index, candidate) in remaining.iter().enumerate() {
                let row = vectors[&candidate.id];
                let redundancy = selected
                    .iter()
                    .map(|selected| {
                        let other = vectors[&selected.id];
                        1.0 - crate::metric_distance(
                            row,
                            other,
                            DistanceMetric::Cosine,
                            crate::vector_norm(row),
                            crate::vector_norm(other),
                        )
                        .unwrap_or(1.0)
                    })
                    .reduce(f64::max)
                    .unwrap_or(0.0);
                let score = lambda * (1.0 - candidate.distance) - (1.0 - lambda) * redundancy;
                if score > best_score || (score == best_score && candidate.id < remaining[best].id)
                {
                    best = index;
                    best_score = score;
                }
            }
            selected.push(remaining.remove(best));
        }
        selected
    }

    fn assert_same_hits(actual: &[SearchHit], expected: &[SearchHit]) {
        assert_eq!(actual.len(), expected.len());
        for (actual, expected) in actual.iter().zip(expected) {
            assert_eq!(actual.id, expected.id);
            assert_eq!(actual.distance.to_bits(), expected.distance.to_bits());
        }
    }

    fn scenario(dims: usize) -> (Vec<f32>, Vec<RerankCandidate>) {
        let mut query = vec![0.0; dims];
        for (i, value) in query.iter_mut().enumerate() {
            *value = match i % 8 {
                0 => 0.5,
                1 => -0.25,
                2 => f32::from_bits(1),
                3 => -0.0,
                4 => 2.0,
                5 => -1.0,
                6 => f32::MAX,
                _ => f32::MIN_POSITIVE,
            };
        }
        let mut candidates = Vec::new();
        for id in [19, 9, 13, 3, 15, 5, 11, 1, 17, 7, 21] {
            let vector = match id {
                9 | 3 => query.clone(),
                _ => query
                    .iter()
                    .enumerate()
                    .map(|(i, value)| match (id + i as u64) % 5 {
                        0 => -*value,
                        1 => 0.0,
                        2 => f32::from_bits(1),
                        3 => *value,
                        _ => -*value * 0.5,
                    })
                    .collect(),
            };
            candidates.push(RerankCandidate {
                id,
                vector,
                payload: Vec::new(),
                distance: 0.0,
            });
        }
        (query, candidates)
    }

    #[test]
    fn exact_and_mmr_reranking_match_row_first_oracle_for_each_kernel() {
        let dimensions = (1..=67).chain([128, 384, 768]);
        for kernel in kernels() {
            exact_kernels::force_kernel(kernel, || {
                for dims in dimensions.clone() {
                    let (query, candidates) = scenario(dims);
                    for metric in [
                        DistanceMetric::SquaredL2,
                        DistanceMetric::Cosine,
                        DistanceMetric::InnerProduct,
                    ] {
                        let actual = ExactReranker { metric }
                            .rerank(&query, &candidates, candidates.len())
                            .unwrap();
                        assert_same_hits(
                            &actual,
                            &oracle_rerank(&query, &candidates, candidates.len(), metric),
                        );
                    }
                    for lambda in [0.0, 0.25, 0.5, 1.0] {
                        for limit in [0, 1, 3, candidates.len(), candidates.len() + 1] {
                            let actual = MmrReranker { lambda }
                                .rerank(&query, &candidates, limit)
                                .unwrap();
                            assert_same_hits(
                                &actual,
                                &oracle_mmr(&query, &candidates, limit, lambda),
                            );
                        }
                    }
                }
            });
        }
    }

    #[test]
    fn scorer_norm_modes_match_oracle_across_metrics_and_dimensions() {
        let dimensions = (0..=67).chain([128, 384, 768]);
        for kernel in kernels() {
            exact_kernels::force_kernel(kernel, || {
                for dims in dimensions.clone() {
                    let query: Vec<f32> = (0..dims)
                        .map(|i| match i % 6 {
                            0 => -0.0,
                            1 => f32::from_bits(1),
                            2 => f32::MAX,
                            3 => f32::MIN_POSITIVE,
                            4 => 0.5,
                            _ => -0.25,
                        })
                        .collect();
                    let rows: Vec<Vec<f32>> = (0..9)
                        .map(|row| {
                            (0..dims)
                                .map(|i| match (i + row) % 6 {
                                    0 => -0.0,
                                    1 => f32::from_bits(1),
                                    2 => f32::MAX,
                                    3 => f32::MIN_POSITIVE,
                                    4 => -0.5,
                                    _ => 1.0,
                                })
                                .collect()
                        })
                        .collect();
                    let refs: Vec<&[f32]> = rows.iter().map(Vec::as_slice).collect();
                    let norms: Vec<f64> = rows.iter().map(|row| crate::vector_norm(row)).collect();
                    for metric in [
                        DistanceMetric::SquaredL2,
                        DistanceMetric::Cosine,
                        DistanceMetric::InnerProduct,
                    ] {
                        let query_norm = if metric == DistanceMetric::Cosine {
                            crate::vector_norm(&query)
                        } else {
                            0.0
                        };
                        let scorer = exact_kernels::Scorer::new(&query, metric, query_norm);
                        for stored_norms in [false, true] {
                            let actual = scorer.score(&refs, stored_norms.then_some(&norms[..]));
                            for (i, row) in rows.iter().enumerate() {
                                let row_norm = if stored_norms {
                                    norms[i]
                                } else if metric == DistanceMetric::Cosine {
                                    crate::vector_norm(row)
                                } else {
                                    0.0
                                };
                                let expected = crate::metric_distance(
                                    row, &query, metric, row_norm, query_norm,
                                );
                                assert_eq!(
                                    actual[i].map(f64::to_bits),
                                    expected.map(f64::to_bits),
                                    "{kernel:?} {metric:?} dims={dims} stored={stored_norms} row={i}"
                                );
                            }
                        }
                    }
                }
            });
        }
    }

    #[test]
    fn mismatched_pairwise_rows_retain_the_oracle_fallback() {
        let query = [1.0f32, -0.0, f32::from_bits(1)];
        let rows: [&[f32]; 3] = [&[1.0, 2.0], &[], &[f32::from_bits(1); 4]];
        let norms: Vec<_> = rows.iter().map(|row| crate::vector_norm(row)).collect();
        for kernel in kernels() {
            exact_kernels::force_kernel(kernel, || {
                let actual = exact_kernels::Scorer::new(
                    &query,
                    DistanceMetric::Cosine,
                    crate::vector_norm(&query),
                )
                .score(&rows, Some(&norms));
                for (i, row) in rows.iter().enumerate() {
                    let expected = crate::metric_distance(
                        row,
                        &query,
                        DistanceMetric::Cosine,
                        norms[i],
                        crate::vector_norm(&query),
                    );
                    assert_eq!(actual[i].map(f64::to_bits), expected.map(f64::to_bits));
                }
            });
        }
    }
}
