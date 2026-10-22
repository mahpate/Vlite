//! Rebuildable, deterministic HNSW graph. The datafile never stores index
//! bytes; the optional ANN snapshot sidecar (`ann_snapshot`) may cache a fully
//! revalidated copy of this in-memory state.

use std::{
    borrow::Cow,
    cell::Cell,
    cmp::Reverse,
    collections::{BTreeMap, BinaryHeap, HashSet},
    io::{self, Read, Write},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use crate::{
    AnnOptions, Condition, DistanceMetric, Error, Record, Result, SearchHit, SearchStats,
    ann_snapshot::{Parse, Source, corrupt},
    metadata, metric_distance,
    quantize::{Quantization, ScalarU8},
    vector_norm,
};

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

/// `(id, norm, links per layer)` for every node, in node order.
pub(crate) struct GraphExport {
    pub entry: Option<usize>,
    pub nodes: Vec<(u64, f64, Vec<Vec<usize>>)>,
}

struct Node {
    id: u64,
    norm: f64,
    links: Vec<Vec<usize>>,
}

pub(crate) struct Index {
    pub config: HnswConfig,
    pub metric: DistanceMetric,
    /// Readiness identity (quantization) and rerank width.
    pub options: AnnOptions,
    nodes: Vec<Node>,
    entry: Option<usize>,
    buffered: BTreeMap<u64, Buffered>,
    buffered_bytes: usize,
    /// ScalarU8 only: codec and node-major codes (`dimensions` bytes per node).
    quantizer: Option<ScalarU8>,
    codes: Vec<u8>,
}

/// Node IDs and u8 codes copied from committed records for one quantized
/// build. Holds no f32 vectors, payloads or metadata.
pub(crate) struct QuantizedSnapshot {
    quantizer: ScalarU8,
    ids: Vec<u64>,
    codes: Vec<u8>,
    /// Cosine zero-norm records excluded from the graph.
    skipped: usize,
}

impl QuantizedSnapshot {
    /// Trains bounds on, and encodes, every graph-eligible record.
    pub fn encode(records: &BTreeMap<u64, Record>, metric: DistanceMetric) -> Self {
        let eligible = || {
            records.iter().filter(move |(_, record)| {
                metric != DistanceMetric::Cosine || vector_norm(&record.vector) != 0.0
            })
        };
        let dimensions = records.values().next().map_or(0, |r| r.vector.len());
        let quantizer = ScalarU8::train(
            dimensions,
            eligible().map(|(_, record)| record.vector.as_slice()),
        );
        let mut ids = Vec::new();
        let mut codes = Vec::new();
        for (&id, record) in eligible() {
            ids.push(id);
            quantizer.encode_into(&record.vector, &mut codes);
        }
        Self {
            quantizer,
            skipped: records.len() - ids.len(),
            ids,
            codes,
        }
    }
}

/// Component sizes of one index; see `AnnMemoryReport`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IndexMemory {
    pub graph_nodes: usize,
    pub link_bytes: usize,
    pub code_bytes: usize,
    pub codebook_bytes: usize,
    pub buffer_vector_bytes: usize,
}

struct Buffered {
    original: Option<Vec<f32>>,
    current_bytes: usize,
}

impl Index {
    /// Plain-data copy of the base graph for the disk-based ANN sidecar. `None`
    /// for quantized graphs (their vectors are not stored here).
    pub(crate) fn export_graph(&self) -> Option<GraphExport> {
        if self.quantized() {
            return None;
        }
        Some(GraphExport {
            entry: self.entry,
            nodes: self
                .nodes
                .iter()
                .map(|node| (node.id, node.norm, node.links.clone()))
                .collect(),
        })
    }

    fn distance(&self, a: &[f32], b: &[f32], a_norm: f64, b_norm: f64) -> f64 {
        metric_distance(a, b, self.metric, a_norm, b_norm)
            .expect("cosine graph contains only nonzero vectors")
    }
    pub fn buffer_usage(&self) -> (usize, usize) {
        (self.buffered.len(), self.buffered_bytes)
    }

    pub fn memory(&self) -> IndexMemory {
        IndexMemory {
            graph_nodes: self.nodes.len(),
            link_bytes: self
                .nodes
                .iter()
                .flat_map(|node| &node.links)
                .map(|links| links.len() * size_of::<usize>())
                .sum(),
            code_bytes: self.codes.len(),
            codebook_bytes: self.quantizer.as_ref().map_or(0, ScalarU8::codebook_bytes),
            buffer_vector_bytes: self.buffered_bytes,
        }
    }

    /// Graph nodes whose record was deleted or replaced after construction.
    /// Such IDs stay buffered until the index is rebuilt.
    pub fn stale_nodes(&self) -> usize {
        self.buffered
            .keys()
            .filter(|&&id| self.nodes.binary_search_by_key(&id, |node| node.id).is_ok())
            .count()
    }

    pub fn graph_nodes(&self) -> usize {
        self.nodes.len()
    }

    fn quantized(&self) -> bool {
        self.options.quantization == Quantization::ScalarU8
    }

    fn codes_of(&self, node: usize) -> &[u8] {
        let dimensions = self
            .quantizer
            .as_ref()
            .expect("quantized index")
            .dimensions();
        &self.codes[node * dimensions..(node + 1) * dimensions]
    }

    /// Traversal distance from an f32 query to one graph node.
    fn node_distance(
        &self,
        node: usize,
        query: &[f32],
        query_norm: f64,
        records: &BTreeMap<u64, Record>,
    ) -> f64 {
        match &self.quantizer {
            Some(quantizer) => quantizer.distance(
                self.metric,
                query,
                query_norm,
                self.codes_of(node),
                self.nodes[node].norm,
            ),
            None => self.distance(
                self.vector(node, records),
                query,
                self.nodes[node].norm,
                query_norm,
            ),
        }
    }

    /// A node's traversal vector as a query: f32 or dequantized codes.
    fn node_query<'a>(&'a self, node: usize, records: &'a BTreeMap<u64, Record>) -> Cow<'a, [f32]> {
        match &self.quantizer {
            Some(quantizer) => Cow::Owned(quantizer.decode(self.codes_of(node))),
            None => Cow::Borrowed(self.vector(node, records)),
        }
    }

    /// Readiness identity: `(config, metric, quantization)`.
    pub fn ready_for(
        &self,
        config: HnswConfig,
        metric: DistanceMetric,
        quantization: Quantization,
    ) -> bool {
        self.config == config && self.metric == metric && self.options.quantization == quantization
    }

    pub fn build(
        records: &BTreeMap<u64, Record>,
        config: HnswConfig,
        metric: DistanceMetric,
        options: AnnOptions,
    ) -> Self {
        Self::build_cancellable(records, config, metric, options, &AtomicBool::new(false))
            .expect("synchronous build is never cancelled")
    }

    pub fn build_cancellable(
        records: &BTreeMap<u64, Record>,
        config: HnswConfig,
        metric: DistanceMetric,
        options: AnnOptions,
        cancel: &AtomicBool,
    ) -> Option<Self> {
        Self::build_with_progress(
            records,
            config,
            metric,
            options,
            cancel,
            &AtomicUsize::new(0),
        )
    }

    pub fn build_with_progress(
        records: &BTreeMap<u64, Record>,
        config: HnswConfig,
        metric: DistanceMetric,
        options: AnnOptions,
        cancel: &AtomicBool,
        processed: &AtomicUsize,
    ) -> Option<Self> {
        if options.quantization == Quantization::ScalarU8 {
            let snapshot = QuantizedSnapshot::encode(records, metric);
            return Self::build_quantized(snapshot, config, metric, options, cancel, processed);
        }
        let mut index = Self::empty(config, metric, options, records.len());
        for &id in records.keys() {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            if metric == DistanceMetric::Cosine && vector_norm(&records[&id].vector) == 0.0 {
                processed.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            index.insert(id, records);
            processed.fetch_add(1, Ordering::Relaxed);
        }
        if cancel.load(Ordering::Relaxed) {
            None
        } else {
            Some(index)
        }
    }

    /// Builds a ScalarU8 graph from codes only; construction distances use
    /// dequantized vectors.
    pub fn build_quantized(
        snapshot: QuantizedSnapshot,
        config: HnswConfig,
        metric: DistanceMetric,
        options: AnnOptions,
        cancel: &AtomicBool,
        processed: &AtomicUsize,
    ) -> Option<Self> {
        debug_assert_eq!(options.quantization, Quantization::ScalarU8);
        processed.fetch_add(snapshot.skipped, Ordering::Relaxed);
        let mut index = Self::empty(config, metric, options, snapshot.ids.len());
        index.codes = snapshot.codes;
        index.quantizer = Some(snapshot.quantizer);
        let no_records = BTreeMap::new();
        for id in snapshot.ids {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            index.insert(id, &no_records);
            processed.fetch_add(1, Ordering::Relaxed);
        }
        if cancel.load(Ordering::Relaxed) {
            None
        } else {
            Some(index)
        }
    }

    fn empty(
        config: HnswConfig,
        metric: DistanceMetric,
        options: AnnOptions,
        capacity: usize,
    ) -> Self {
        Self {
            config,
            metric,
            options,
            nodes: Vec::with_capacity(capacity),
            entry: None,
            buffered: BTreeMap::new(),
            buffered_bytes: 0,
            quantizer: None,
            codes: Vec::new(),
        }
    }

    pub fn adopt_buffer(&mut self, changes: Self) {
        self.buffered = changes.buffered;
        self.buffered_bytes = changes.buffered_bytes;
    }

    /// Called only after sync, before authoritative records are replaced.
    ///
    /// Unquantized graphs keep a copy of each changed graph vector because
    /// traversal reads vectors from current records. Quantized graphs score
    /// their own codes, so they keep no f32 copy and count only current bytes.
    pub fn track(&mut self, id: u64, previous: Option<&Record>, current_bytes: usize) -> bool {
        let keep_original = !self.quantized();
        let prior = self.buffered.get(&id);
        let original_bytes = prior.map_or_else(
            || {
                previous
                    .filter(|_| keep_original)
                    .map_or(0, |record| record.vector.len() * 4)
            },
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
                original: previous
                    .filter(|_| keep_original)
                    .map(|record| record.vector.clone()),
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
        visits: &Cell<u64>,
    ) -> Candidate {
        visits.set(visits.get() + 1);
        Candidate {
            distance: self.node_distance(node, query, query_norm, records),
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
        visits: &Cell<u64>,
    ) -> Candidate {
        loop {
            let previous = best;
            for &neighbor in &self.nodes[previous.node].links[layer] {
                let candidate = self.candidate(neighbor, query, query_norm, records, visits);
                if candidate < best {
                    best = candidate;
                }
            }
            if best == previous {
                return best;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn layer(
        &self,
        query: &[f32],
        query_norm: f64,
        entry: Candidate,
        layer: usize,
        ef: usize,
        records: &BTreeMap<u64, Record>,
        visits: &Cell<u64>,
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
                let candidate = self.candidate(node, query, query_norm, records, visits);
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
            let candidate_query = self.node_query(candidate.node, records);
            let candidate_norm = self.nodes[candidate.node].norm;
            // Preserve directionally diverse links rather than only a dense local cluster.
            if selected.iter().all(|&node| {
                self.node_distance(node, &candidate_query, candidate_norm, records)
                    >= candidate.distance
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
        let cosine = self.metric == DistanceMetric::Cosine;
        let (query, norm) = match &self.quantizer {
            Some(quantizer) => {
                let codes = self.codes_of(new);
                let norm = if cosine { quantizer.norm(codes) } else { 0.0 };
                (Cow::Owned(quantizer.decode(codes)), norm)
            }
            None => {
                let vector = records[&id].vector.as_slice();
                (
                    Cow::Borrowed(vector),
                    if cosine { vector_norm(vector) } else { 0.0 },
                )
            }
        };
        self.nodes.push(Node {
            id,
            norm,
            links: vec![Vec::new(); level + 1],
        });
        let Some(entry) = self.entry else {
            self.entry = Some(new);
            return;
        };
        let top = self.nodes[entry].links.len() - 1;
        let query = &*query;
        let query_norm = self.nodes[new].norm;
        let visits = Cell::new(0);
        let mut best = self.candidate(entry, query, query_norm, records, &visits);
        for layer in ((level + 1)..=top).rev() {
            best = self.greedy(query, query_norm, best, layer, records, &visits);
        }
        for layer in (0..=level.min(top)).rev() {
            let candidates = self.layer(
                query,
                query_norm,
                best,
                layer,
                self.config.ef_construction,
                records,
                &visits,
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
                    let neighbor_query = self.node_query(neighbor, records);
                    let mut candidates = self.nodes[neighbor].links[layer]
                        .iter()
                        .map(|&node| {
                            self.candidate(
                                node,
                                &neighbor_query,
                                self.nodes[neighbor].norm,
                                records,
                                &visits,
                            )
                        })
                        .collect::<Vec<_>>();
                    drop(neighbor_query);
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

    /// Base-layer traversal whose result heap holds only eligible graph nodes.
    ///
    /// Every reachable node may be expanded as a bridge; only nodes that are
    /// unbuffered and whose current metadata matches enter `results`. Traversal
    /// stops once `ef` eligible results are held and the nearest unexpanded
    /// candidate is worse than the worst of them, so selective or empty-match
    /// filters may visit every reachable node.
    #[allow(clippy::too_many_arguments)]
    fn filtered_layer(
        &self,
        query: &[f32],
        query_norm: f64,
        entry: Candidate,
        ef: usize,
        records: &BTreeMap<u64, Record>,
        expiries: &BTreeMap<u64, u64>,
        now_ms: u64,
        conditions: &[Condition],
        visits: &Cell<u64>,
    ) -> Vec<Candidate> {
        let eligible = |node: usize| {
            let id = self.nodes[node].id;
            !self.buffered.contains_key(&id)
                && !expiries.get(&id).is_some_and(|expiry| *expiry <= now_ms)
                && records
                    .get(&id)
                    .is_some_and(|record| metadata::matches(&record.metadata, conditions))
        };
        let mut visited = HashSet::from([entry.node]);
        let mut candidates = BinaryHeap::from([Reverse(entry)]);
        let mut results = BinaryHeap::with_capacity(ef.min(self.nodes.len()).saturating_add(1));
        if eligible(entry.node) {
            results.push(entry);
        }
        while let Some(Reverse(current)) = candidates.pop() {
            if results.len() >= ef && current > *results.peek().expect("nonempty results") {
                break;
            }
            for &node in &self.nodes[current.node].links[0] {
                if !visited.insert(node) {
                    continue;
                }
                let candidate = self.candidate(node, query, query_norm, records, visits);
                if results.len() < ef || candidate < *results.peek().expect("nonempty results") {
                    candidates.push(Reverse(candidate));
                    if eligible(node) {
                        results.push(candidate);
                        if results.len() > ef {
                            results.pop();
                        }
                    }
                }
            }
        }
        let mut results = results.into_vec();
        results.sort_unstable();
        results
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn search_filtered_with_expiry(
        &self,
        query: &[f32],
        k: usize,
        ef: usize,
        rerank: AnnOptions,
        records: &BTreeMap<u64, Record>,
        expiries: &BTreeMap<u64, u64>,
        now_ms: u64,
        conditions: &[Condition],
        stats: &mut SearchStats,
    ) -> Vec<SearchHit> {
        let width = rerank.rerank_width(k, ef);
        let query_norm = self.query_norm(query);
        let mut hits = Vec::new();
        let visits = Cell::new(0);
        if let Some(entry) = self.entry {
            let best = self.descend(query, query_norm, entry, records, &visits);
            let retain = ef.max(k).min(self.nodes.len());
            let eligible = self.filtered_layer(
                query, query_norm, best, retain, records, expiries, now_ms, conditions, &visits,
            );
            hits = self.finish_graph_hits(eligible, query, query_norm, k, width, records, stats);
        }
        self.count_visits(stats, &visits);
        self.merge_buffered(
            hits, query, query_norm, k, records, expiries, now_ms, conditions, stats,
        )
    }

    /// Converts traversal-ordered eligible candidates into graph hits.
    ///
    /// Unquantized distances are already exact. ScalarU8 candidates are
    /// re-scored exactly from current records (unbuffered graph nodes are
    /// unchanged since construction) over the rerank `width`, then cut to k.
    #[allow(clippy::too_many_arguments)]
    fn finish_graph_hits(
        &self,
        eligible: Vec<Candidate>,
        query: &[f32],
        query_norm: f64,
        k: usize,
        width: usize,
        records: &BTreeMap<u64, Record>,
        stats: &mut SearchStats,
    ) -> Vec<SearchHit> {
        if self.quantizer.is_none() {
            return eligible
                .into_iter()
                .take(k)
                .map(|candidate| SearchHit {
                    id: self.nodes[candidate.node].id,
                    distance: candidate.distance,
                })
                .collect();
        }
        let mut hits: Vec<SearchHit> = eligible
            .into_iter()
            .take(width)
            .filter_map(|candidate| {
                let id = self.nodes[candidate.node].id;
                let vector = &records[&id].vector;
                let norm = if self.metric == DistanceMetric::Cosine {
                    vector_norm(vector)
                } else {
                    0.0
                };
                stats.reranked_candidates = stats.reranked_candidates.saturating_add(1);
                stats.distance_evaluations = stats.distance_evaluations.saturating_add(1);
                metric_distance(vector, query, self.metric, norm, query_norm)
                    .map(|distance| SearchHit { id, distance })
            })
            .collect();
        hits.sort_by(|a, b| {
            a.distance
                .total_cmp(&b.distance)
                .then_with(|| a.id.cmp(&b.id))
        });
        hits.truncate(k);
        hits
    }

    fn count_visits(&self, stats: &mut SearchStats, visits: &Cell<u64>) {
        stats.graph_nodes_visited = stats.graph_nodes_visited.saturating_add(visits.get());
        if self.quantizer.is_some() {
            stats.quantized_distance_evaluations = stats
                .quantized_distance_evaluations
                .saturating_add(visits.get());
        } else {
            stats.distance_evaluations = stats.distance_evaluations.saturating_add(visits.get());
        }
    }

    fn query_norm(&self, query: &[f32]) -> f64 {
        if self.metric == DistanceMetric::Cosine {
            vector_norm(query)
        } else {
            0.0
        }
    }

    fn descend(
        &self,
        query: &[f32],
        query_norm: f64,
        entry: usize,
        records: &BTreeMap<u64, Record>,
        visits: &Cell<u64>,
    ) -> Candidate {
        let mut best = self.candidate(entry, query, query_norm, records, visits);
        for layer in (1..self.nodes[entry].links.len()).rev() {
            best = self.greedy(query, query_norm, best, layer, records, visits);
        }
        best
    }

    #[allow(clippy::too_many_arguments)]
    fn merge_buffered(
        &self,
        mut hits: Vec<SearchHit>,
        query: &[f32],
        query_norm: f64,
        k: usize,
        records: &BTreeMap<u64, Record>,
        expiries: &BTreeMap<u64, u64>,
        now_ms: u64,
        conditions: &[Condition],
        stats: &mut SearchStats,
    ) -> Vec<SearchHit> {
        let before = hits.len();
        hits.extend(self.buffered.keys().filter_map(|&id| {
            records
                .get(&id)
                .filter(|record| {
                    !expiries.get(&id).is_some_and(|expiry| *expiry <= now_ms)
                        && metadata::matches(&record.metadata, conditions)
                })
                .and_then(|record| {
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
        let merged = (hits.len() - before) as u64;
        stats.buffered_records_merged = stats.buffered_records_merged.saturating_add(merged);
        stats.distance_evaluations = stats.distance_evaluations.saturating_add(merged);
        hits.sort_by(|a, b| {
            a.distance
                .total_cmp(&b.distance)
                .then_with(|| a.id.cmp(&b.id))
        });
        hits.truncate(k);
        hits
    }
}

/// Smallest encoded node: `id u64`, `norm u64`, `levels u8`, one `count u16`.
const MIN_SNAPSHOT_NODE_BYTES: u64 = 19;

fn metric_code(metric: DistanceMetric) -> io::Result<u8> {
    match metric {
        DistanceMetric::SquaredL2 => Ok(0),
        DistanceMetric::Cosine => Ok(1),
        DistanceMetric::InnerProduct => Err(io::Error::other("inner product has no HNSW index")),
    }
}

impl Index {
    /// Writes one ANN snapshot section body (format v1), from `m` through the
    /// node-major codes. Identity and digest framing belong to `ann_snapshot`.
    pub(crate) fn write_snapshot(&self, w: &mut impl Write) -> io::Result<()> {
        let too_large = || io::Error::other("HNSW index exceeds ANN snapshot format v1 limits");
        let u32_of = |value: usize| u32::try_from(value).map_err(|_| too_large());
        w.write_all(&u32_of(self.config.m)?.to_le_bytes())?;
        w.write_all(&u32_of(self.config.ef_construction)?.to_le_bytes())?;
        w.write_all(&self.config.seed.to_le_bytes())?;
        w.write_all(&[
            metric_code(self.metric)?,
            self.options.quantization.code(),
            self.options.rerank_multiplier,
            0,
        ])?;
        // Links are stored as u32 node positions.
        u32_of(self.nodes.len())?;
        w.write_all(&(self.nodes.len() as u64).to_le_bytes())?;
        w.write_all(
            &self
                .entry
                .map_or(u64::MAX, |entry| entry as u64)
                .to_le_bytes(),
        )?;
        for node in &self.nodes {
            w.write_all(&node.id.to_le_bytes())?;
            w.write_all(&node.norm.to_bits().to_le_bytes())?;
            w.write_all(&[u8::try_from(node.links.len()).map_err(|_| too_large())?])?;
            for links in &node.links {
                let count = u16::try_from(links.len()).map_err(|_| too_large())?;
                w.write_all(&count.to_le_bytes())?;
                for &target in links {
                    w.write_all(&u32_of(target)?.to_le_bytes())?;
                }
            }
        }
        w.write_all(&u32_of(self.buffered.len())?.to_le_bytes())?;
        for (&id, entry) in &self.buffered {
            w.write_all(&id.to_le_bytes())?;
            w.write_all(&(entry.current_bytes as u64).to_le_bytes())?;
            match &entry.original {
                None => w.write_all(&[0])?,
                Some(original) => {
                    w.write_all(&[1])?;
                    for value in original {
                        w.write_all(&value.to_bits().to_le_bytes())?;
                    }
                }
            }
        }
        match &self.quantizer {
            None => w.write_all(&[0]),
            Some(quantizer) => {
                w.write_all(&[1])?;
                quantizer.write_snapshot(w)?;
                w.write_all(&self.codes)
            }
        }
    }

    /// Reads one section body and accepts it only if it is exactly a state
    /// that `build`/`track`/`adopt_buffer` could have produced for the current
    /// `records` (see `check_snapshot`). Every count is bounded by current
    /// in-memory state and by the unread file bytes before allocation.
    pub(crate) fn read_snapshot<R: Read>(
        source: &mut Source<R>,
        records: &BTreeMap<u64, Record>,
        dimensions: usize,
    ) -> Parse<Self> {
        let m = source.u32()? as usize;
        let ef_construction = source.u32()? as usize;
        let seed = source.u64()?;
        let config = HnswConfig {
            m,
            ef_construction,
            seed,
        };
        config.validate().map_err(|_| corrupt("hnsw config"))?;
        let metric = match source.u8()? {
            0 => DistanceMetric::SquaredL2,
            1 => DistanceMetric::Cosine,
            _ => return Err(corrupt("metric")),
        };
        let quantization =
            Quantization::from_code(source.u8()?).ok_or_else(|| corrupt("quantization"))?;
        let options = AnnOptions {
            quantization,
            rerank_multiplier: source.u8()?,
        };
        options.validate().map_err(|_| corrupt("ann options"))?;
        if source.u8()? != 0 {
            return Err(corrupt("reserved"));
        }
        let node_count = source.u64()?;
        let max_nodes = records.len().saturating_add(MAX_HNSW_BUFFER_IDS) as u64;
        if node_count > max_nodes
            || node_count.saturating_mul(MIN_SNAPSHOT_NODE_BYTES) > source.remaining()
        {
            return Err(corrupt("node count"));
        }
        let node_count = node_count as usize;
        let entry = source.u64()?;
        let mut index = Self::empty(config, metric, options, node_count);
        for _ in 0..node_count {
            let id = source.u64()?;
            if index.nodes.last().is_some_and(|last| last.id >= id) {
                return Err(corrupt("node order"));
            }
            let norm = f64::from_bits(source.u64()?);
            let levels = usize::from(source.u8()?);
            if levels != index.level(id) + 1 {
                return Err(corrupt("node level"));
            }
            let mut links = Vec::with_capacity(levels);
            for layer in 0..levels {
                let count = usize::from(source.u16()?);
                if count > if layer == 0 { 2 * m } else { m } {
                    return Err(corrupt("node degree"));
                }
                let mut targets = Vec::with_capacity(count);
                for _ in 0..count {
                    targets.push(source.u32()? as usize);
                }
                links.push(targets);
            }
            index.nodes.push(Node { id, norm, links });
        }
        index.entry = match entry {
            u64::MAX => None,
            entry if entry < node_count as u64 => Some(entry as usize),
            _ => return Err(corrupt("entry")),
        };
        let buffered_count = source.u32()? as usize;
        if buffered_count > MAX_HNSW_BUFFER_IDS {
            return Err(corrupt("buffer count"));
        }
        let mut original_bytes = 0usize;
        for _ in 0..buffered_count {
            let id = source.u64()?;
            if index
                .buffered
                .last_key_value()
                .is_some_and(|(&last, _)| last >= id)
            {
                return Err(corrupt("buffer order"));
            }
            let current_bytes = source.u64()?;
            let expected = records.get(&id).map_or(0, |record| record.vector.len() * 4);
            if current_bytes != expected as u64 {
                return Err(corrupt("buffer current bytes"));
            }
            let original = match source.u8()? {
                0 => None,
                1 if quantization == Quantization::None => {
                    original_bytes = original_bytes.saturating_add(dimensions * 4);
                    if original_bytes > MAX_HNSW_BUFFER_VECTOR_BYTES {
                        return Err(corrupt("buffer bytes"));
                    }
                    let vector = source.f32s(dimensions)?;
                    if vector.iter().any(|value| !value.is_finite()) {
                        return Err(corrupt("buffer original"));
                    }
                    Some(vector)
                }
                _ => return Err(corrupt("buffer original")),
            };
            index.buffered_bytes = index
                .buffered_bytes
                .saturating_add(expected)
                .saturating_add(original.as_ref().map_or(0, |vector| vector.len() * 4));
            index.buffered.insert(
                id,
                Buffered {
                    original,
                    current_bytes: expected,
                },
            );
        }
        if index.buffered_bytes > MAX_HNSW_BUFFER_VECTOR_BYTES {
            return Err(corrupt("buffer bytes"));
        }
        match source.u8()? {
            0 => {}
            1 => {
                let quantizer = ScalarU8::read_snapshot(source, dimensions)?;
                let length = node_count
                    .checked_mul(quantizer.dimensions())
                    .ok_or_else(|| corrupt("codes"))?;
                index.codes = source.bytes(length)?;
                index.quantizer = Some(quantizer);
            }
            _ => return Err(corrupt("quantizer flag")),
        }
        index.check_snapshot(records, dimensions)?;
        Ok(index)
    }

    /// Semantic invariants that make every later search, `track` and
    /// `stale_nodes` call total (no panics) and that reject any state the
    /// builder could not have produced for the current records.
    fn check_snapshot(&self, records: &BTreeMap<u64, Record>, dimensions: usize) -> Parse<()> {
        let count = self.nodes.len();
        let top = self.nodes.iter().map(|node| node.links.len()).max();
        let first_top = top.and_then(|top| self.nodes.iter().position(|n| n.links.len() == top));
        if self.entry != first_top {
            return Err(corrupt("entry"));
        }
        let mut sorted = Vec::new();
        for (position, node) in self.nodes.iter().enumerate() {
            for (layer, links) in node.links.iter().enumerate() {
                if links.iter().any(|&target| {
                    target >= count || target == position || self.nodes[target].links.len() <= layer
                }) {
                    return Err(corrupt("links"));
                }
                sorted.clear();
                sorted.extend_from_slice(links);
                sorted.sort_unstable();
                if sorted.windows(2).any(|pair| pair[0] == pair[1]) {
                    return Err(corrupt("links"));
                }
            }
        }
        let cosine = self.metric == DistanceMetric::Cosine;
        match &self.quantizer {
            None if self.quantized() => return Err(corrupt("quantizer missing")),
            Some(_) if !self.quantized() => return Err(corrupt("quantizer unexpected")),
            Some(quantizer) if quantizer.dimensions() != dimensions && count != 0 => {
                return Err(corrupt("quantizer dimensions"));
            }
            _ => {}
        }
        let mut codes = Vec::new();
        for (position, node) in self.nodes.iter().enumerate() {
            let buffered = self.buffered.get(&node.id);
            let live = records.get(&node.id);
            let norm = match &self.quantizer {
                None => {
                    let vector = match (buffered, live) {
                        (Some(entry), _) => entry.original.as_deref(),
                        (None, record) => record.map(|record| record.vector.as_slice()),
                    }
                    .ok_or_else(|| corrupt("node vector"))?;
                    let norm = if cosine { vector_norm(vector) } else { 0.0 };
                    if cosine && norm == 0.0 {
                        return Err(corrupt("node vector"));
                    }
                    norm
                }
                Some(quantizer) => {
                    let stored = self.codes_of(position);
                    if buffered.is_none() {
                        let record = live.ok_or_else(|| corrupt("node vector"))?;
                        if cosine && vector_norm(&record.vector) == 0.0 {
                            return Err(corrupt("node vector"));
                        }
                        codes.clear();
                        quantizer.encode_into(&record.vector, &mut codes);
                        if codes != stored {
                            return Err(corrupt("codes"));
                        }
                    }
                    if cosine { quantizer.norm(stored) } else { 0.0 }
                }
            };
            if node.norm.to_bits() != norm.to_bits() {
                return Err(corrupt("node norm"));
            }
        }
        let in_graph = |id: u64| self.nodes.binary_search_by_key(&id, |n| n.id).is_ok();
        for (&id, entry) in &self.buffered {
            // Only a cosine zero-norm record is tracked with an original but
            // left out of the graph.
            if let Some(original) = &entry.original
                && !in_graph(id)
                && !(cosine && vector_norm(original) == 0.0)
            {
                return Err(corrupt("buffer original"));
            }
        }
        for (&id, record) in records {
            if !self.buffered.contains_key(&id)
                && !in_graph(id)
                && !(cosine && vector_norm(&record.vector) == 0.0)
            {
                return Err(corrupt("incomplete graph"));
            }
        }
        Ok(())
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
        let mut index = Index::build(
            &records,
            HnswConfig::default(),
            DistanceMetric::SquaredL2,
            AnnOptions::default(),
        );
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
            AnnOptions::default(),
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
            let index = Index::build(
                &records,
                config,
                DistanceMetric::SquaredL2,
                AnnOptions::default(),
            );
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
