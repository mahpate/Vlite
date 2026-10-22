mod vector_support;
use vlite::{
    DistanceMetric,
    rerank::{ExactReranker, MmrReranker, RerankCandidate, Reranker},
};
fn candidates() -> Vec<RerankCandidate> {
    [
        (3, vec![0.0, 1.0]),
        (2, vec![0.99, 0.01]),
        (1, vec![1.0, 0.0]),
    ]
    .into_iter()
    .map(|(id, vector)| RerankCandidate {
        id,
        vector,
        payload: Vec::new(),
        distance: 100.0,
    })
    .collect()
}
#[test]
fn exact_reorders_quantized_candidates_and_mmr_diversifies() {
    let cs = candidates();
    let exact = ExactReranker {
        metric: DistanceMetric::Cosine,
    };
    let hits = exact.rerank(&[1.0, 0.0], &cs, 3).unwrap();
    assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), [1, 2, 3]);
    let mmr = MmrReranker { lambda: 0.2 };
    let hits = mmr.rerank(&[1.0, 0.0], &cs, 2).unwrap();
    assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), [1, 3]);
    assert!(exact.rerank(&[0.0, 0.0], &cs, 1).is_err());
    assert!(
        MmrReranker { lambda: f64::NAN }
            .rerank(&[1.0, 0.0], &cs, 1)
            .is_err()
    );
    let mut bad = cs.clone();
    bad[0].vector = vec![1.0];
    assert!(exact.rerank(&[1.0, 0.0], &bad, 1).is_err());
    let duplicates = vec![cs[0].clone(), cs[0].clone()];
    assert!(exact.rerank(&[1.0, 0.0], &duplicates, 1).is_err());
    assert!(exact.rerank(&[1.0, 0.0], &[], 10).unwrap().is_empty());
}
#[test]
fn engine_adapter_and_custom_provider() {
    use vector_support::*;
    let f = Fixture::new();
    let mut db = f.db();
    seed(&mut db);
    let c = db.collection("default").unwrap();
    let hits = c.search(&[3.0, 1.0], 4).unwrap();
    let result = c
        .rerank(
            &[1.0, 1.0],
            &hits,
            1,
            &ExactReranker {
                metric: DistanceMetric::SquaredL2,
            },
        )
        .unwrap();
    assert_eq!(result[0].id, 1);
}
