//! Rebuildable, deterministic HNSW graph. No index bytes are persisted.

use std::{
    cmp::Reverse,
    collections::{BTreeMap, BinaryHeap, HashSet},
};

use crate::{DistanceMetric, Error, Record, Result, SearchHit, metric_distance, vector_norm};

/// Index construction parameters. A changed configuration requires a rebuild.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HnswConfig {
    pub m: usize,
    pub ef_construction: usize,
    pub seed: u64,
}

impl Default for HnswConfig {
    fn default() -> Self {
        Self {
            m: 16,
            ef_construction: 100,
            seed: 42,
        }
    }
}

impl HnswConfig {
    pub(crate) fn validate(self) -> Result<()> {
        if !(2..=64).contains(&self.m) || !(self.m..=4096).contains(&self.ef_construction) {
            return Err(Error::InvalidHnswConfig);
        }
        Ok(())
    }
}

/// Maximum search candidate width, not a total memory or execution-time bound.
pub const MAX_EF_SEARCH: usize = 100_000;
pub const MAX_HNSW_BUFFER_IDS: usize = 256;
pub const MAX_HNSW_BUFFER_VECTOR_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
struct Candidate {
    distance: f64,
    node: usize,
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}
impl Eq for Candidate {}
impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.distance
            .total_cmp(&other.distance)
            .then_with(|| self.node.cmp(&other.node))
    }
}

struct Node {
    id: u64,
    norm: f64,
    links: Vec<Vec<usize>>,
}

pub(crate) struct Index {
    pub config: HnswConfig,
    pub metric: DistanceMetric,
    nodes: Vec<Node>,
    entry: Option<usize>,
    buffered: BTreeMap<u64, Buffered>,
    buffered_bytes: usize,
}

struct Buffered {
    original: Option<Vec<f32>>,
    current_bytes: usize,
}

impl Index {
    fn distance(&self, a: &[f32], b: &[f32], a_norm: f64, b_norm: f64) -> f64 {
        metric_distance(a, b, self.metric, a_norm, b_norm)
            .expect("cosine graph contains only nonzero vectors")
    }
    #[cfg(any(feature = "server", test))]
    pub fn buffer_usage(&self) -> (usize, usize) {
        (self.buffered.len(), self.buffered_bytes)
    }

    pub fn build(
        records: &BTreeMap<u64, Record>,
        config: HnswConfig,
        metric: DistanceMetric,
    ) -> Self {
        let mut index = Self {
            config,
            metric,
            nodes: Vec::with_capacity(records.len()),
            entry: None,
            buffered: BTreeMap::new(),
            buffered_bytes: 0,
        };
        for &id in records.keys() {
            if metric == DistanceMetric::Cosine && vector_norm(&records[&id].vector) == 0.0 {
                continue;
            }
            index.insert(id, records);
        }
        index
    }

    /// Called only after sync, before authoritative records are replaced.
    pub fn track(&mut self, id: u64, previous: Option<&Record>, current_bytes: usize) -> bool {
        let prior = self.buffered.get(&id);
        let original_bytes = prior.map_or_else(
            || previous.map_or(0, |record| record.vector.len() * 4),
            |entry| entry.original.as_ref().map_or(0, |vector| vector.len() * 4),
        );
        let removed = prior.map_or(0, |entry| original_bytes + entry.current_bytes);
        let bytes = self.buffered_bytes - removed + original_bytes + current_bytes;
        if (prior.is_none() && self.buffered.len() == MAX_HNSW_BUFFER_IDS)
            || bytes > MAX_HNSW_BUFFER_VECTOR_BYTES
        {
            return false;
        }
        self.buffered
            .entry(id)
            .and_modify(|entry| entry.current_bytes = current_bytes)
            .or_insert_with(|| Buffered {
                original: previous.map(|record| record.vector.clone()),
                current_bytes,
            });
        self.buffered_bytes = bytes;
        true
    }

    fn vector<'a>(&'a self, node: usize, records: &'a BTreeMap<u64, Record>) -> &'a [f32] {
        let id = self.nodes[node].id;
        if let Some(entry) = self.buffered.get(&id) {
            entry
                .original
                .as_deref()
                .expect("graph IDs have original vectors")
        } else {
            &records[&id].vector
        }
    }

    fn candidate(
        &self,
        node: usize,
        query: &[f32],
        query_norm: f64,
        records: &BTreeMap<u64, Record>,
    ) -> Candidate {
        Candidate {
            distance: self.distance(
                self.vector(node, records),
                query,
                self.nodes[node].norm,
                query_norm,
            ),
            node,
        }
    }

    fn greedy(
        &self,
        query: &[f32],
        query_norm: f64,
        mut best: Candidate,
        layer: usize,
        records: &BTreeMap<u64, Record>,
    ) -> Candidate {
        loop {
            let previous = best;
            for &neighbor in &self.nodes[previous.node].links[layer] {
                let candidate = self.candidate(neighbor, query, query_norm, records);
                if candidate < best {
                    best = candidate;
                }
            }
            if best == previous {
                return best;
            }
        }
    }

    fn layer(
        &self,
        query: &[f32],
        query_norm: f64,
        entry: Candidate,
        layer: usize,
        ef: usize,
        records: &BTreeMap<u64, Record>,
    ) -> Vec<Candidate> {
        let mut visited = HashSet::from([entry.node]);
        let mut candidates = BinaryHeap::from([Reverse(entry)]);
        let mut nearest = BinaryHeap::from([entry]);
        while let Some(Reverse(current)) = candidates.pop() {
            if nearest.len() >= ef && current > *nearest.peek().expect("nonempty nearest") {
                break;
            }
            for &node in &self.nodes[current.node].links[layer] {
                if !visited.insert(node) {
                    continue;
                }
                let candidate = self.candidate(node, query, query_norm, records);
                if nearest.len() < ef || candidate < *nearest.peek().expect("nonempty nearest") {
                    candidates.push(Reverse(candidate));
                    nearest.push(candidate);
                    if nearest.len() > ef {
                        nearest.pop();
                    }
                }
            }
        }
        let mut nearest = nearest.into_vec();
        nearest.sort_unstable();
        nearest
    }

    fn select(
        &self,
        candidates: &[Candidate],
        limit: usize,
        records: &BTreeMap<u64, Record>,
    ) -> Vec<usize> {
        let mut selected = Vec::with_capacity(limit);
        for candidate in candidates {
            // Preserve directionally diverse links rather than only a dense local cluster.
            if selected.iter().all(|&node| {
                self.distance(
                    self.vector(candidate.node, records),
                    self.vector(node, records),
                    self.nodes[candidate.node].norm,
                    self.nodes[node].norm,
                ) >= candidate.distance
            }) {
                selected.push(candidate.node);
                if selected.len() == limit {
                    return selected;
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
        selected
    }

    fn level(&self, id: u64) -> usize {
        let mut value = id ^ self.config.seed;
        let mut level = 0;
        loop {
            value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut mixed = value;
            mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            mixed ^= mixed >> 31;
            if !mixed.is_multiple_of(self.config.m as u64) || level == 16 {
                return level;
            }
            level += 1;
        }
    }

    fn insert(&mut self, id: u64, records: &BTreeMap<u64, Record>) {
        let level = self.level(id);
        let new = self.nodes.len();
        self.nodes.push(Node {
            id,
            norm: if self.metric == DistanceMetric::Cosine {
                vector_norm(&records[&id].vector)
            } else {
                0.0
            },
            links: vec![Vec::new(); level + 1],
        });
        let Some(entry) = self.entry else {
            self.entry = Some(new);
            return;
        };
        let top = self.nodes[entry].links.len() - 1;
        let query = &records[&id].vector;
        let query_norm = self.nodes[new].norm;
        let mut best = self.candidate(entry, query, query_norm, records);
        for layer in ((level + 1)..=top).rev() {
            best = self.greedy(query, query_norm, best, layer, records);
        }
        for layer in (0..=level.min(top)).rev() {
            let candidates = self.layer(
                query,
                query_norm,
                best,
                layer,
                self.config.ef_construction,
                records,
            );
            let neighbors = self.select(&candidates, self.config.m, records);
            self.nodes[new].links[layer] = neighbors.clone();
            let limit = if layer == 0 {
                2 * self.config.m
            } else {
                self.config.m
            };
            for neighbor in neighbors {
                self.nodes[neighbor].links[layer].push(new);
                if self.nodes[neighbor].links[layer].len() > limit {
                    let mut candidates = self.nodes[neighbor].links[layer]
                        .iter()
                        .map(|&node| {
                            self.candidate(
                                node,
                                self.vector(neighbor, records),
                                self.nodes[neighbor].norm,
                                records,
                            )
                        })
                        .collect::<Vec<_>>();
                    candidates.sort_unstable();
                    self.nodes[neighbor].links[layer] = self.select(&candidates, limit, records);
                }
            }
            best = candidates[0];
        }
        if level > top {
            self.entry = Some(new);
        }
    }

    pub fn search(
        &self,
        query: &[f32],
        k: usize,
        ef: usize,
        records: &BTreeMap<u64, Record>,
    ) -> Vec<SearchHit> {
        let mut hits = Vec::new();
        let query_norm = if self.metric == DistanceMetric::Cosine {
            vector_norm(query)
        } else {
            0.0
        };
        if let Some(entry) = self.entry {
            let mut best = self.candidate(entry, query, query_norm, records);
            for layer in (1..self.nodes[entry].links.len()).rev() {
                best = self.greedy(query, query_norm, best, layer, records);
            }
            hits.extend(
                self.layer(
                    query,
                    query_norm,
                    best,
                    0,
                    ef + self.buffered.len(),
                    records,
                )
                .into_iter()
                .filter(|candidate| !self.buffered.contains_key(&self.nodes[candidate.node].id))
                .take(k)
                .map(|candidate| SearchHit {
                    id: self.nodes[candidate.node].id,
                    distance: candidate.distance,
                }),
            );
        }
        hits.extend(self.buffered.keys().filter_map(|&id| {
            records.get(&id).and_then(|record| {
                metric_distance(
                    &record.vector,
                    query,
                    self.metric,
                    if self.metric == DistanceMetric::Cosine {
                        vector_norm(&record.vector)
                    } else {
                        0.0
                    },
                    query_norm,
                )
                .map(|distance| SearchHit { id, distance })
            })
        }));
        hits.sort_by(|a, b| {
            a.distance
                .total_cmp(&b.distance)
                .then_with(|| a.id.cmp(&b.id))
        });
        hits.truncate(k);
        hits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_preserves_original_vectors_and_enforces_exact_limits() {
        let record = Record {
            vector: vec![0.0; crate::MAX_DIMENSIONS],
            payload: vec![],
            metadata: crate::Metadata::new(),
        };
        let records = BTreeMap::from([(0, record.clone())]);
        let mut index = Index::build(&records, HnswConfig::default(), DistanceMetric::SquaredL2);
        let vector_bytes = crate::MAX_DIMENSIONS * 4;
        for id in 0..16 {
            assert!(index.track(id, Some(&record), vector_bytes));
        }
        assert_eq!(index.buffer_usage(), (16, MAX_HNSW_BUFFER_VECTOR_BYTES));
        assert!(index.track(0, Some(&record), 0));
        assert_eq!(index.vector(0, &BTreeMap::new()), record.vector);
        assert!(index.track(0, Some(&record), vector_bytes));
        assert!(!index.track(16, Some(&record), vector_bytes));
        assert_eq!(index.buffer_usage(), (16, MAX_HNSW_BUFFER_VECTOR_BYTES));
        let mut index = Index::build(
            &BTreeMap::new(),
            HnswConfig::default(),
            DistanceMetric::SquaredL2,
        );
        for id in 0..MAX_HNSW_BUFFER_IDS {
            assert!(index.track(id as u64, None, 4));
            assert!(index.track(id as u64, Some(&record), 0));
        }
        assert_eq!(index.buffer_usage(), (MAX_HNSW_BUFFER_IDS, 0));
        assert!(!index.track(u64::MAX, None, 0));
    }

    #[test]
    fn graph_links_have_bounded_degree_valid_layers_and_no_self_or_duplicate_edges() {
        let records = (0..200)
            .map(|id| {
                (
                    id,
                    Record {
                        vector: vec![(id % 17) as f32, (id / 17) as f32],
                        payload: vec![],
                        metadata: crate::Metadata::new(),
                    },
                )
            })
            .collect();
        for m in [2, 8, 64] {
            let config = HnswConfig {
                m,
                ef_construction: 100,
                seed: 42,
            };
            let index = Index::build(&records, config, DistanceMetric::SquaredL2);
            for (position, node) in index.nodes.iter().enumerate() {
                for (layer, links) in node.links.iter().enumerate() {
                    assert!(links.len() <= if layer == 0 { 2 * m } else { m });
                    let mut seen = HashSet::new();
                    for &neighbor in links {
                        assert_ne!(neighbor, position);
                        assert!(seen.insert(neighbor));
                        assert!(index.nodes[neighbor].links.len() > layer);
                    }
                }
            }
        }
    }
}
