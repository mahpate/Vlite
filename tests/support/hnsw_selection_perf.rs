//! Focused equivalence checks and bounded, opt-in construction profiling.
//! Run with `cargo test --release --lib hnsw::selection_perf -- --nocapture`.

use super::*;
use std::{hint::black_box, time::Instant};

fn records(count: usize, dimensions: usize, ties: bool) -> BTreeMap<u64, Record> {
    let mut state = 42u64;
    (0..count)
        .map(|id| {
            let vector = (0..dimensions)
                .map(|d| {
                    state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                    if ties {
                        ((id % 8 + d) % 4) as f32
                    } else {
                        (state >> 48) as f32 / 32768.0 - 1.0
                    }
                })
                .collect();
            (
                id as u64 * 3 + 1,
                Record {
                    vector,
                    payload: vec![],
                    metadata: crate::Metadata::new(),
                },
            )
        })
        .collect()
}

// The original selection implementation, intentionally independent of `select`.
fn reference_select<const PROFILE: bool>(
    index: &Index,
    candidates: &[Candidate],
    limit: usize,
    records: &BTreeMap<u64, Record>,
) -> (Vec<usize>, usize, usize) {
    let mut selected = Vec::with_capacity(limit);
    let mut candidate_queries = 0;
    let mut pair_distances = 0;
    for candidate in candidates {
        let candidate_query = index.node_query(candidate.node, records);
        if PROFILE {
            candidate_queries += 1;
        }
        let candidate_norm = index.nodes[candidate.node].norm;
        if selected.iter().all(|&node| {
            if PROFILE {
                pair_distances += 1;
            }
            index.node_distance(node, &candidate_query, candidate_norm, records)
                >= candidate.distance
        }) {
            selected.push(candidate.node);
            if selected.len() == limit {
                return (selected, candidate_queries, pair_distances);
            }
        }
    }
    for candidate in candidates {
        if !selected.contains(&candidate.node) {
            selected.push(candidate.node);
            if selected.len() == limit {
                break;
            }
        }
    }
    (selected, candidate_queries, pair_distances)
}

fn fingerprint(index: &Index) -> u64 {
    let mut bytes = Vec::new();
    index.write_snapshot(&mut bytes).unwrap();
    bytes.into_iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
    })
}

fn candidates(index: &Index, records: &BTreeMap<u64, Record>, query_node: usize) -> Vec<Candidate> {
    let query = index.node_query(query_node, records);
    let visits = Cell::new(0);
    let mut candidates = (0..index.nodes.len())
        .filter(|&node| node != query_node)
        .map(|node| index.candidate(node, &query, index.nodes[query_node].norm, records, &visits))
        .collect::<Vec<_>>();
    candidates.sort_unstable();
    candidates
}

#[test]
fn selection_matches_original_for_metrics_quantization_ties_and_limits() {
    for ties in [false, true] {
        let records = records(160, 17, ties);
        for metric in [DistanceMetric::SquaredL2, DistanceMetric::Cosine] {
            for quantization in [Quantization::None, Quantization::ScalarU8] {
                let index = Index::build(
                    &records,
                    HnswConfig::default(),
                    metric,
                    AnnOptions {
                        quantization,
                        ..AnnOptions::default()
                    },
                );
                for query_node in [0, 17, 80, 159] {
                    let candidates = candidates(&index, &records, query_node);
                    for count in [0, 1, 7, candidates.len()] {
                        for limit in [2, 16, 32, 64, 128] {
                            assert_eq!(
                                index.select(&candidates[..count], limit, &records),
                                reference_select::<false>(
                                    &index,
                                    &candidates[..count],
                                    limit,
                                    &records
                                )
                                .0,
                                "ties={ties}, metric={metric:?}, quantization={quantization:?}, query={query_node}, count={count}, limit={limit}"
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn construction_preserves_original_snapshot_bytes() {
    let records = records(3_000, 32, false);
    // Captured from the original implementation with the same deterministic data.
    for (metric, quantization, expected) in [
        (
            DistanceMetric::SquaredL2,
            Quantization::None,
            16929988181799570983,
        ),
        (
            DistanceMetric::SquaredL2,
            Quantization::ScalarU8,
            2198838869134152650,
        ),
        (
            DistanceMetric::Cosine,
            Quantization::None,
            8897825992413431978,
        ),
        (
            DistanceMetric::Cosine,
            Quantization::ScalarU8,
            3242429306425500422,
        ),
    ] {
        let index = Index::build(
            &records,
            HnswConfig::default(),
            metric,
            AnnOptions {
                quantization,
                ..AnnOptions::default()
            },
        );
        assert_eq!(fingerprint(&index), expected, "{metric:?}/{quantization:?}");
    }
}

#[test]
fn cached_selection_uses_original_buffered_vectors() {
    let mut records = records(160, 17, false);
    let mut index = Index::build(
        &records,
        HnswConfig::default(),
        DistanceMetric::Cosine,
        AnnOptions::default(),
    );
    let original = candidates(&index, &records, 0);
    let expected = reference_select::<false>(&index, &original, 32, &records).0;
    for node in (0..160).step_by(3) {
        let id = index.nodes[node].id;
        assert!(index.track(id, records.get(&id), 17 * 4));
        if node % 2 == 0 {
            records.remove(&id);
            assert!(index.track(id, None, 0));
        } else {
            records.get_mut(&id).unwrap().vector.fill(-999.0);
        }
    }
    let buffered = candidates(&index, &records, 0);
    assert_eq!(buffered, original);
    assert_eq!(index.select(&buffered, 32, &records), expected);
    assert_eq!(
        index.select(&buffered, 128, &records),
        reference_select::<false>(&index, &buffered, 128, &records).0
    );
}

#[test]
#[ignore = "bounded release-mode before/after construction benchmark"]
fn profile_construction_and_selection() {
    let records = records(3_000, 32, false);
    for metric in [DistanceMetric::SquaredL2, DistanceMetric::Cosine] {
        for quantization in [Quantization::None, Quantization::ScalarU8] {
            let options = AnnOptions {
                quantization,
                ..AnnOptions::default()
            };
            let config = HnswConfig::default();
            let mut timings = Vec::new();
            let mut last = None;
            for _ in 0..3 {
                let start = Instant::now();
                let index = Index::build(&records, config, metric, options);
                timings.push(start.elapsed().as_micros());
                let hash = fingerprint(&index);
                if let Some((previous, _)) = &last {
                    assert_eq!(hash, *previous);
                }
                last = Some((hash, index));
            }
            let (hash, index) = last.unwrap();
            println!(
                "construction metric={metric:?} quantization={quantization:?} us={timings:?} snapshot_fingerprint={hash}"
            );
            let sets = (0..64)
                .map(|i| {
                    let mut candidates = candidates(&index, &records, i * 43);
                    candidates.truncate(100);
                    candidates
                })
                .collect::<Vec<_>>();
            let mut queries = 0;
            let mut distances = 0;
            let mut selected = 0;
            for candidates in &sets {
                let (result, q, d) = reference_select::<true>(&index, candidates, 16, &records);
                assert_eq!(result, index.select(candidates, 16, &records));
                queries += q;
                distances += d;
                selected += result.len();
            }
            println!(
                "selection_profile metric={metric:?} quantization={quantization:?} calls={} candidate_queries={queries} pair_distances={distances} selected={selected} reference_vector_fetches={} cached_vector_fetches={}",
                sets.len(),
                queries + distances,
                if quantization == Quantization::None {
                    queries
                } else {
                    queries + distances
                }
            );
            let mut reference_times = Vec::new();
            let mut current_times = Vec::new();
            for round in 0..7 {
                // Alternate order to reduce warm-cache/order bias.
                for reference in if round % 2 == 0 {
                    [true, false]
                } else {
                    [false, true]
                } {
                    let start = Instant::now();
                    for _ in 0..100 {
                        for candidates in &sets {
                            if reference {
                                black_box(reference_select::<false>(
                                    black_box(&index),
                                    black_box(candidates),
                                    16,
                                    black_box(&records),
                                ));
                            } else {
                                black_box(index.select(
                                    black_box(candidates),
                                    16,
                                    black_box(&records),
                                ));
                            }
                        }
                    }
                    let elapsed = start.elapsed().as_micros();
                    if reference {
                        reference_times.push(elapsed);
                    } else {
                        current_times.push(elapsed);
                    }
                }
            }
            println!(
                "selection_timing metric={metric:?} quantization={quantization:?} reference_us={reference_times:?} current_us={current_times:?} selections_per_sample=6400"
            );
        }
    }
}
