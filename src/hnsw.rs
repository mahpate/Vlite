//! Rebuildable, deterministic HNSW graph. No index bytes are persisted.

use std::{
    cmp::Reverse,
    collections::{BTreeMap, BinaryHeap, HashSet},
};

use crate::{Error, Record, Result, SearchHit};

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
    links: Vec<Vec<usize>>,
}

pub(crate) struct Index {
    pub config: HnswConfig,
    nodes: Vec<Node>,
    entry: Option<usize>,
}

fn distance(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(&a, &b)| {
            let difference = f64::from(a) - f64::from(b);
            difference * difference
        })
        .sum()
}

impl Index {
    pub fn build(records: &BTreeMap<u64, Record>, config: HnswConfig) -> Self {
        let mut index = Self {
            config,
            nodes: Vec::with_capacity(records.len()),
            entry: None,
        };
        for &id in records.keys() {
            index.insert(id, records);
        }
        index
    }

    fn vector<'a>(&self, node: usize, records: &'a BTreeMap<u64, Record>) -> &'a [f32] {
        &records[&self.nodes[node].id].vector
    }

    fn candidate(&self, node: usize, query: &[f32], records: &BTreeMap<u64, Record>) -> Candidate {
        Candidate {
            distance: distance(query, self.vector(node, records)),
            node,
        }
    }

    fn greedy(
        &self,
        query: &[f32],
        mut best: Candidate,
        layer: usize,
        records: &BTreeMap<u64, Record>,
    ) -> Candidate {
        loop {
            let previous = best;
            for &neighbor in &self.nodes[previous.node].links[layer] {
                let candidate = self.candidate(neighbor, query, records);
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
                let candidate = self.candidate(node, query, records);
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
                distance(
                    self.vector(candidate.node, records),
                    self.vector(node, records),
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
            links: vec![Vec::new(); level + 1],
        });
        let Some(entry) = self.entry else {
            self.entry = Some(new);
            return;
        };
        let top = self.nodes[entry].links.len() - 1;
        let query = &records[&id].vector;
        let mut best = self.candidate(entry, query, records);
        for layer in ((level + 1)..=top).rev() {
            best = self.greedy(query, best, layer, records);
        }
        for layer in (0..=level.min(top)).rev() {
            let candidates = self.layer(query, best, layer, self.config.ef_construction, records);
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
                        .map(|&node| self.candidate(node, self.vector(neighbor, records), records))
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
        let Some(entry) = self.entry else {
            return Vec::new();
        };
        let mut best = self.candidate(entry, query, records);
        for layer in (1..self.nodes[entry].links.len()).rev() {
            best = self.greedy(query, best, layer, records);
        }
        self.layer(query, best, 0, ef, records)
            .into_iter()
            .take(k)
            .map(|candidate| SearchHit {
                id: self.nodes[candidate.node].id,
                distance: candidate.distance,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_links_have_bounded_degree_valid_layers_and_no_self_or_duplicate_edges() {
        let records = (0..200)
            .map(|id| {
                (
                    id,
                    Record {
                        vector: vec![(id % 17) as f32, (id / 17) as f32],
                        payload: vec![],
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
            let index = Index::build(&records, config);
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
