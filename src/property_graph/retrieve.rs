//! Hybrid pivot-and-expand retrieval on [`GraphRef`] (EPIC-010,
//! REQ-040..REQ-042).
//!
//! `retrieve` runs one pivot search through the unchanged
//! `CollectionRef::search_hybrid` (exact or ready-ANN vector leg, BM25 text
//! leg, or both fused), re-validates every pivot record against the same
//! view, maps pivot records to this graph's nodes through attachments,
//! expands from those nodes with the traversal semantics of REQ-022, and
//! projects evidence records attached to the returned nodes.
//!
//! Everything happens inside one call borrowing `&Database`, so the pivot
//! search, attachments, graph and evidence records observe one committed
//! state, reported as `view_sequence` (Section 3.5). The pivot phase is
//! bounded by PD-015 (`k`) and PD-016 (exact-scan pivots only on
//! collections at or below `max_exact_scan_records` live records); it has
//! no deadline and its duration is reported as `stats.pivot_ms`. The
//! expansion phase (traversal and evidence) is bounded by the read limits,
//! the PD-008 deadline and cancellation; its duration is `stats.expand_ms`.
//! All bounds are validated before any search or traversal work.
//!
//! Results never carry vectors.

use std::collections::BTreeSet;

use crate::hybrid::{FusionOptions, HybridQuery, VectorLeg};
use crate::{Condition, DistanceMetric, Error, Metadata, Result, SearchStats, metadata};

use super::GraphRef;
use super::convert::graph_error;
use super::engine::NodeId;
use super::read::{
    self, Direction, EdgeView, GraphPath, GraphReadStats, NodeFilter, NodeSelector,
    PropertyProjection, ReadOptions, Traverse, TraversedNode, TruncationReason,
};

/// Most unmapped pivot record IDs listed (REQ-041).
const MAX_UNMAPPED_IDS: usize = 64;
/// Most evidence collections and projected evidence fields.
const MAX_EVIDENCE_NAMES: usize = 64;

/// The pivot search: `search_hybrid` over `collection` with at least one of
/// `vector` and `text`. `filter` applies to both legs and to re-validation.
#[derive(Clone, Debug, PartialEq)]
pub struct Pivot {
    pub collection: String,
    pub vector: Option<Vec<f32>>,
    pub text: Option<String>,
    /// PD-015: at most [`Pivot::MAX_K`].
    pub k: usize,
    pub filter: Vec<Condition>,
    pub metric: DistanceMetric,
    pub vector_leg: VectorLeg,
    pub fusion: FusionOptions,
    /// PD-016: an exact vector leg is only run on collections with at most
    /// this many live records (`GraphPivotTooExpensive` otherwise); at most
    /// [`Pivot::MAX_EXACT_SCAN_RECORDS`].
    pub max_exact_scan_records: u64,
}

impl Pivot {
    pub const DEFAULT_K: usize = 20;
    pub const MAX_K: usize = 100;
    pub const DEFAULT_MAX_EXACT_SCAN_RECORDS: u64 = 100_000;
    pub const MAX_EXACT_SCAN_RECORDS: u64 = 1_000_000;

    /// No vector or text yet; exact squared-L2 vector leg, default fusion.
    pub fn new(collection: &str, k: usize) -> Self {
        Self {
            collection: collection.into(),
            vector: None,
            text: None,
            k,
            filter: Vec::new(),
            metric: DistanceMetric::SquaredL2,
            vector_leg: VectorLeg::Exact,
            fusion: FusionOptions::default(),
            max_exact_scan_records: Self::DEFAULT_MAX_EXACT_SCAN_RECORDS,
        }
    }
}

/// The expansion from pivot nodes, as a [`Traverse`] without start nodes.
#[derive(Clone, Debug, PartialEq)]
pub struct Expansion {
    pub direction: Direction,
    pub types: Option<Vec<String>>,
    pub expand_filter: NodeFilter,
    pub return_filter: NodeFilter,
    pub edge_filter: Vec<Condition>,
    pub include_paths: bool,
}

impl Default for Expansion {
    /// Outgoing edges of every type, no filters, no paths.
    fn default() -> Self {
        Self {
            direction: Direction::Out,
            types: None,
            expand_filter: NodeFilter::default(),
            return_filter: NodeFilter::default(),
            edge_filter: Vec::new(),
            include_paths: false,
        }
    }
}

/// Which attached records of returned nodes are projected as evidence.
/// The default projects none (no collections).
#[derive(Clone, Debug, PartialEq)]
pub struct EvidenceSpec {
    /// Collections whose attached records are evidence, in output order
    /// per node (at most 64; duplicates ignored).
    pub collections: Vec<String>,
    /// Projected metadata fields (at most 64 names for `Only`).
    pub fields: PropertyProjection,
    /// PD-013: payload excerpt bytes, at most
    /// [`EvidenceSpec::MAX_EXCERPT_BYTES`].
    pub max_excerpt_bytes: usize,
    /// Evidence items returned, at most [`EvidenceSpec::MAX_ITEMS`]; more
    /// candidates give `OutputLimit`.
    pub max_items: u64,
}

impl EvidenceSpec {
    pub const DEFAULT_EXCERPT_BYTES: usize = 512;
    pub const MAX_EXCERPT_BYTES: usize = 4_096;
    pub const DEFAULT_ITEMS: u64 = 100;
    pub const MAX_ITEMS: u64 = 1_000;
}

impl Default for EvidenceSpec {
    fn default() -> Self {
        Self {
            collections: Vec::new(),
            fields: PropertyProjection::All,
            max_excerpt_bytes: Self::DEFAULT_EXCERPT_BYTES,
            max_items: Self::DEFAULT_ITEMS,
        }
    }
}

/// One hybrid retrieval request (REQ-040).
#[derive(Clone, Debug, PartialEq)]
pub struct Retrieve {
    pub pivot: Pivot,
    pub expansion: Expansion,
    pub evidence: EvidenceSpec,
}

/// A pivot record mapped to a node of this graph, with its fused scores.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct PivotHit {
    pub record_id: u64,
    pub node_id: u64,
    pub node_key: String,
    pub score: f64,
    pub distance: Option<f64>,
    pub bm25: Option<f64>,
    pub vector_rank: Option<u32>,
    pub text_rank: Option<u32>,
}

/// Pivot records not attached to a node of this graph (REQ-041).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct UnmappedPivots {
    pub count: u64,
    /// The first 64 in pivot order.
    pub record_ids: Vec<u64>,
}

/// An attached record of a returned node (REQ-042). Never carries vectors.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Evidence {
    pub collection: String,
    pub record_id: u64,
    pub revision: u64,
    pub view_sequence: u64,
    pub node_id: u64,
    pub metadata: Metadata,
    /// A prefix of the payload of at most `max_excerpt_bytes`, cut at a
    /// character boundary when the payload is UTF-8.
    pub payload_excerpt: Vec<u8>,
    pub payload_bytes: u64,
    pub excerpt_truncated: bool,
}

/// Work done by a retrieval.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RetrieveStats {
    /// Pivot search, re-validation and mapping.
    pub pivot_ms: u64,
    /// Traversal and evidence projection.
    pub expand_ms: u64,
    /// Hits returned by the pivot search.
    pub pivot_candidates: u64,
    pub pivots_mapped: u64,
    /// Hits failing re-validation (no longer live or not matching `filter`).
    pub pivots_rejected: u64,
    pub evidence_items: u64,
    /// Vector-leg work of the pivot search.
    pub search: SearchStats,
    pub expansion: GraphReadStats,
}

/// The result of [`GraphRef::retrieve`]. `complete` is true exactly when
/// `truncation` is empty; truncation only ever applies to the expansion
/// and evidence, never to the pivots.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Retrieval {
    /// Mapped pivots in pivot (score) order.
    pub pivots: Vec<PivotHit>,
    pub unmapped_pivots: UnmappedPivots,
    /// Pivot nodes at depth 0 and the nodes reached from them.
    pub nodes: Vec<TraversedNode>,
    pub edges: Vec<EdgeView>,
    /// With `include_paths`, one path from a pivot per non-pivot node.
    pub paths: Vec<GraphPath>,
    pub evidence: Vec<Evidence>,
    pub complete: bool,
    pub truncation: Vec<TruncationReason>,
    pub view_sequence: u64,
    pub stats: RetrieveStats,
}

fn ceiling(limit: &'static str, requested: u64, ceiling: u64) -> Result<()> {
    if requested > ceiling {
        return Err(Error::GraphLimitExceedsCeiling {
            limit,
            requested,
            ceiling,
        });
    }
    Ok(())
}

fn rank(reason: &TruncationReason) -> u8 {
    match reason {
        TruncationReason::VisitedLimit => 0,
        TruncationReason::EdgeLimit => 1,
        TruncationReason::DegreeLimit { .. } => 2,
        TruncationReason::OutputLimit => 3,
        TruncationReason::Deadline => 4,
        TruncationReason::Cancelled => 5,
    }
}

/// Adds `reason` once, keeping declaration order (INV-010).
fn add_truncation(truncation: &mut Vec<TruncationReason>, reason: TruncationReason) {
    if truncation.iter().all(|known| rank(known) != rank(&reason)) {
        truncation.push(reason);
        truncation.sort_by_key(rank);
    }
}

fn project(metadata: &Metadata, fields: &PropertyProjection) -> Metadata {
    match fields {
        PropertyProjection::All => metadata.clone(),
        PropertyProjection::None => Metadata::new(),
        PropertyProjection::Only(names) => names
            .iter()
            .filter_map(|name| Some((name.clone(), metadata.get(name)?.clone())))
            .collect(),
    }
}

fn excerpt(payload: &[u8], max_bytes: usize) -> &[u8] {
    if payload.len() <= max_bytes {
        return payload;
    }
    match std::str::from_utf8(payload) {
        Ok(text) => {
            let mut end = max_bytes;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            &payload[..end]
        }
        Err(_) => &payload[..max_bytes],
    }
}

fn elapsed(clock: &dyn Fn() -> u64, since: u64) -> u64 {
    clock().saturating_sub(since)
}

impl GraphRef<'_> {
    /// Pivot search, mapping, expansion and evidence in one call and one
    /// view (REQ-040..REQ-042). See [`Retrieve`] and the module docs.
    ///
    /// Errors (before any work): `GraphLimitExceedsCeiling` for read limits
    /// or `pivot_k`, `max_exact_scan_records`, `max_excerpt_bytes`,
    /// `max_evidence_items` above their ceilings; `GraphPivotTooExpensive`
    /// (PD-016); `CollectionNotFound`; `InvalidQuery`. The pivot search
    /// reports its own errors (`InvalidQuery`, `DimensionMismatch`,
    /// `IndexNotReady` for an ANN leg without a ready index).
    pub fn retrieve(&self, request: &Retrieve, options: &ReadOptions<'_>) -> Result<Retrieval> {
        self.retrieve_with_clock(request, options, &read::clock())
    }

    fn retrieve_with_clock(
        &self,
        request: &Retrieve,
        options: &ReadOptions<'_>,
        clock: &dyn Fn() -> u64,
    ) -> Result<Retrieval> {
        let db = self.db;
        let Retrieve {
            pivot,
            expansion,
            evidence,
        } = request;
        options.limits.validate().map_err(graph_error)?;
        ceiling("pivot_k", pivot.k as u64, Pivot::MAX_K as u64)?;
        ceiling(
            "max_exact_scan_records",
            pivot.max_exact_scan_records,
            Pivot::MAX_EXACT_SCAN_RECORDS,
        )?;
        ceiling(
            "max_excerpt_bytes",
            evidence.max_excerpt_bytes as u64,
            EvidenceSpec::MAX_EXCERPT_BYTES as u64,
        )?;
        ceiling(
            "max_evidence_items",
            evidence.max_items,
            EvidenceSpec::MAX_ITEMS,
        )?;
        let pivot_collection = db.collection(&pivot.collection)?;
        let mut evidence_collections: Vec<(&str, u16)> = Vec::new();
        for name in &evidence.collections {
            let id = db.collection(name)?.id;
            if evidence_collections.iter().all(|(_, known)| *known != id) {
                evidence_collections.push((name, id));
            }
        }
        if evidence_collections.len() > MAX_EVIDENCE_NAMES {
            return Err(Error::InvalidQuery(
                "evidence collections must be at most 64",
            ));
        }
        if let PropertyProjection::Only(names) = &evidence.fields
            && names.len() > MAX_EVIDENCE_NAMES
        {
            return Err(Error::InvalidQuery("evidence fields must be at most 64"));
        }
        let mut traverse = Traverse {
            start: Vec::new(),
            direction: expansion.direction,
            types: expansion.types.clone(),
            expand_filter: expansion.expand_filter.clone(),
            return_filter: expansion.return_filter.clone(),
            edge_filter: expansion.edge_filter.clone(),
            include_paths: expansion.include_paths,
        };
        read::traverse_op(&traverse)?;
        let exact_vector_leg = pivot.vector.is_some()
            && pivot.fusion.vector_weight > 0.0
            && matches!(pivot.vector_leg, VectorLeg::Exact);
        if exact_vector_leg && pivot_collection.len() as u64 > pivot.max_exact_scan_records {
            return Err(Error::GraphPivotTooExpensive);
        }

        // Pivot phase: the unchanged hybrid search, re-validation, mapping.
        let pivot_started = clock();
        // Gap 3: pivots and expansion see the graph at one read clock.
        let at = self.with_clock(self.now());
        let query = HybridQuery {
            vector: pivot.vector.clone(),
            text: pivot.text.clone().unwrap_or_default(),
            k: pivot.k,
            conditions: pivot.filter.clone(),
            metric: pivot.metric,
            vector_leg: pivot.vector_leg,
            fusion: pivot.fusion,
        };
        let (hits, search) = pivot_collection.search_hybrid(&query)?;
        let graph_state = db.graphs()?;
        let mut stats = RetrieveStats {
            pivot_candidates: hits.len() as u64,
            search,
            ..RetrieveStats::default()
        };
        let mut pivots = Vec::new();
        let mut unmapped = UnmappedPivots::default();
        for hit in hits {
            let live = db
                .get_in(pivot_collection.id, hit.id)?
                .is_some_and(|record| metadata::matches(&record.metadata, &pivot.filter));
            if !live {
                stats.pivots_rejected += 1;
                continue;
            }
            let node = graph_state
                .attachment_owner(pivot_collection.id, hit.id)
                .filter(|(graph, _)| *graph == self.graph.id())
                .filter(|(_, node)| !self.graph.node_expired(*node, at.now()))
                .and_then(|(_, node)| self.graph.node(node));
            let Some(node) = node else {
                unmapped.count += 1;
                if unmapped.record_ids.len() < MAX_UNMAPPED_IDS {
                    unmapped.record_ids.push(hit.id);
                }
                continue;
            };
            pivots.push(PivotHit {
                record_id: hit.id,
                node_id: node.id.0,
                node_key: node.key.clone(),
                score: hit.score,
                distance: hit.distance,
                bm25: hit.bm25,
                vector_rank: hit.vector_rank,
                text_rank: hit.text_rank,
            });
        }
        stats.pivots_mapped = pivots.len() as u64;
        stats.pivot_ms = elapsed(clock, pivot_started);

        // Expansion phase: traversal from the pivot nodes, then evidence.
        let expand_started = clock();
        let mut retrieval = Retrieval {
            pivots,
            unmapped_pivots: unmapped,
            nodes: Vec::new(),
            edges: Vec::new(),
            paths: Vec::new(),
            evidence: Vec::new(),
            complete: true,
            truncation: Vec::new(),
            view_sequence: self.view_sequence,
            stats,
        };
        if !retrieval.pivots.is_empty() {
            let mut seen = BTreeSet::new();
            traverse.start = retrieval
                .pivots
                .iter()
                .filter(|hit| seen.insert(hit.node_id))
                .map(|hit| NodeSelector::Id(hit.node_id))
                .collect();
            let read = at.traverse_with_clock(&traverse, options, clock)?;
            retrieval.nodes = read.output.nodes;
            retrieval.edges = read.output.edges;
            retrieval.paths = read.output.paths;
            retrieval.truncation = read.truncation;
            retrieval.stats.expansion = read.stats;
        }
        if !evidence_collections.is_empty() {
            'nodes: for node in &retrieval.nodes {
                let attached: Vec<(u16, u64)> =
                    self.graph.attachments(NodeId(node.node.id)).collect();
                for &(name, collection) in &evidence_collections {
                    for &(_, record_id) in attached.iter().filter(|(c, _)| *c == collection) {
                        let Some(record) = db.get_in(collection, record_id)? else {
                            continue;
                        };
                        if retrieval.evidence.len() as u64 >= evidence.max_items {
                            add_truncation(
                                &mut retrieval.truncation,
                                TruncationReason::OutputLimit,
                            );
                            break 'nodes;
                        }
                        let payload_excerpt = excerpt(&record.payload, evidence.max_excerpt_bytes);
                        retrieval.evidence.push(Evidence {
                            collection: name.into(),
                            record_id,
                            revision: db.revision_in(collection, record_id)?.unwrap_or(0),
                            view_sequence: self.view_sequence,
                            node_id: node.node.id,
                            metadata: project(&record.metadata, &evidence.fields),
                            payload_excerpt: payload_excerpt.to_vec(),
                            payload_bytes: record.payload.len() as u64,
                            excerpt_truncated: payload_excerpt.len() < record.payload.len(),
                        });
                    }
                }
            }
        }
        retrieval.stats.evidence_items = retrieval.evidence.len() as u64;
        retrieval.stats.expand_ms = elapsed(clock, expand_started);
        retrieval.complete = retrieval.truncation.is_empty();
        Ok(retrieval)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::Database;
    use crate::property_graph::GraphLimits;

    /// A graph whose single pivot node fans out to `fanout` topics, with a
    /// clock that advances `step` ms per reading.
    fn fan_out(dir: &std::path::Path, fanout: u64) -> Database {
        let mut db = Database::create(dir.join("deadline.vlite"), 2).unwrap();
        db.put(1, &[0.0, 0.0], b"pivot").unwrap();
        db.create_graph("g").unwrap();
        let mut graph = db.graph_mut("g").unwrap();
        graph.create_node("hub", &[], &Metadata::new()).unwrap();
        graph.attach("hub", "default", 1).unwrap();
        for leaf in 0..fanout {
            let key = format!("leaf-{leaf}");
            graph.create_node(&key, &[], &Metadata::new()).unwrap();
            graph
                .create_edge(None, "hub", "LINKS", &key, &Metadata::new())
                .unwrap();
        }
        db
    }

    fn request() -> Retrieve {
        let mut pivot = Pivot::new("default", 1);
        pivot.vector = Some(vec![0.0, 0.0]);
        Retrieve {
            pivot,
            expansion: Expansion::default(),
            evidence: EvidenceSpec {
                collections: vec!["default".into()],
                ..EvidenceSpec::default()
            },
        }
    }

    #[test]
    fn deadline_truncates_the_expansion_and_keeps_the_pivots() {
        let dir = tempfile::tempdir().unwrap();
        let db = fan_out(dir.path(), 500);
        let graph = db.graph("g").unwrap();
        let now = Cell::new(0u64);
        // Each reading advances 100 ms: the pivot phase spends 100 ms and
        // the expansion's 250 ms deadline expires after a few meter checks.
        let clock = || {
            let value = now.get();
            now.set(value + 100);
            value
        };
        let options = ReadOptions {
            limits: GraphLimits {
                max_output_nodes: 1_000,
                max_output_edges: 2_000,
                ..GraphLimits::DEFAULT
            },
            ..ReadOptions::default()
        };
        let result = graph
            .retrieve_with_clock(&request(), &options, &clock)
            .unwrap();
        assert_eq!(result.pivots.len(), 1);
        assert_eq!(result.pivots[0].node_key, "hub");
        assert!(!result.complete);
        assert_eq!(result.truncation, vec![TruncationReason::Deadline]);
        assert!(result.stats.expansion.edges_examined < 500);
        assert_eq!(result.stats.pivot_ms, 100);
        assert!(result.stats.expand_ms >= options.limits.deadline_ms);
        // Evidence is still projected for the returned pivot node.
        assert_eq!(result.evidence.len(), 1);
        assert_eq!(result.evidence[0].payload_excerpt, b"pivot");

        // A clock that never advances never expires: the same request is
        // complete, so the truncation above came from the deadline alone.
        let frozen = || 0;
        let result = graph
            .retrieve_with_clock(&request(), &options, &frozen)
            .unwrap();
        assert!(result.complete, "{:?}", result.truncation);
        assert_eq!(result.nodes.len(), 501);
    }

    #[test]
    fn excerpts_cut_at_char_boundaries() {
        assert_eq!(excerpt("aé".as_bytes(), 2), b"a");
        assert_eq!(excerpt(&[0xff, 0xfe, 0xfd], 2), &[0xff, 0xfe]);
        assert_eq!(excerpt(b"abc", 5), b"abc");
    }
}
