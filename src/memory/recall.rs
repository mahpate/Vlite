//! Private bounded recall. Exact discovery never falls back to ANN.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

use super::ranking::{self, RankedCandidate, RankingPolicy, ScoredCandidate};
use super::{
    LABEL_MEMORY, MemoryError, MemoryKind, MemoryRef, MemoryScope, Provenance, content_edge_key,
    content_key, evidence_record_id, existing_memory, hex, memory_key, opaque, string_property,
};
use crate::{
    Condition, MetadataValue, SearchHit,
    hybrid::{HybridQuery, MAX_HYBRID_TEXT_BYTES, MAX_HYBRID_TOKENS, VectorLeg, fuse_ranked},
    property_graph::{
        FindNodes, GraphLimits, GraphPath, NodeFilter, NodeSelector, NodeView, PropertyProjection,
        ReadOptions, Traverse, TruncationReason,
    },
    rerank::{ExactReranker, MmrReranker, RerankCandidate, Reranker},
};

type RecallResult<T> = Result<T, RecallError>;

#[derive(Debug)]
pub(super) enum RecallError {
    Memory(MemoryError),
    Storage(crate::Error),
    Ranking(ranking::RankingError),
    InvalidLimits,
    InvalidScopes,
    InvalidQuery,
    UnsupportedPlan,
    Budget(&'static str),
    Cancelled,
    Deadline,
}

impl From<MemoryError> for RecallError {
    fn from(error: MemoryError) -> Self {
        Self::Memory(error)
    }
}

impl From<crate::Error> for RecallError {
    fn from(error: crate::Error) -> Self {
        Self::Storage(error)
    }
}

impl From<ranking::RankingError> for RecallError {
    fn from(error: ranking::RankingError) -> Self {
        Self::Ranking(error)
    }
}

#[derive(Clone, Debug)]
pub(super) struct RecallLimits {
    pub candidates: usize,
    pub source_records: usize,
    pub vector_components: usize,
    pub evidence_bytes: usize,
    pub mmr_components: usize,
    pub graph: GraphLimits,
}

impl Default for RecallLimits {
    fn default() -> Self {
        Self {
            candidates: 200,
            source_records: 100_000,
            vector_components: 1_000_000,
            evidence_bytes: 1024 * 1024,
            mmr_components: 10_000_000,
            graph: GraphLimits::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Selection {
    Ranked,
    Exact,
    Mmr { lambda: f64 },
}

#[derive(Clone, Debug)]
pub(super) struct RecallSeed {
    pub scope: MemoryScope,
    pub id: String,
}

#[derive(Clone, Debug)]
pub(super) struct RecallRequest<'a> {
    pub scopes: Vec<MemoryScope>,
    pub kinds: Vec<MemoryKind>,
    /// Conjoined with mandatory scope, kind and owner-retention checks.
    pub filter: Vec<Condition>,
    pub query: Option<HybridQuery>,
    pub seeds: Vec<RecallSeed>,
    pub as_of_ms: Option<i64>,
    pub clock_ms: u64,
    pub policy: RankingPolicy,
    pub selection: Selection,
    pub limits: RecallLimits,
    pub expand: bool,
    pub allow_partial: bool,
    pub snippet_bytes: usize,
    pub cancel: Option<&'a AtomicBool>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct RecallEvidence {
    pub id: String,
    pub record_id: u64,
    pub revision: u64,
    pub snippet: Vec<u8>,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct RecallHit {
    pub ranking: ScoredCandidate,
    pub memory_revision: u64,
    pub graph_revision: u64,
    pub provenance: Vec<Provenance>,
    pub evidence: Vec<RecallEvidence>,
    pub content: String,
    pub content_truncated: bool,
    pub paths: Vec<GraphPath>,
    pub distance: Option<f64>,
    pub bm25: Option<f64>,
    pub vector_rank: Option<u32>,
    pub text_rank: Option<u32>,
    /// S2: the S1 stale label of a C1 summary hit; `None` otherwise.
    pub stale: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct RecallStats {
    pub source_records_charged: usize,
    pub candidates_examined: usize,
    pub edges_charged: u64,
    pub vector_components: usize,
    pub mmr_components: usize,
    pub evidence_bytes: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct RecallOutput {
    pub hits: Vec<RecallHit>,
    pub scopes: Vec<MemoryScope>,
    pub as_of_ms: i64,
    pub clock_ms: u64,
    pub sequence: u64,
    pub stats: RecallStats,
    pub complete: bool,
    pub truncation: Vec<TruncationReason>,
    pub selection: Selection,
    pub candidate_mode: &'static str,
    pub historical_discovery: &'static str,
}

struct Work<'a> {
    request: &'a RecallRequest<'a>,
    started: Instant,
    stats: RecallStats,
}

impl Work<'_> {
    fn remaining_ms(&self) -> RecallResult<u64> {
        self.check()?;
        self.request
            .limits
            .graph
            .deadline_ms
            .checked_sub(u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX))
            .filter(|remaining| *remaining > 0)
            .ok_or(RecallError::Deadline)
    }

    fn check(&self) -> RecallResult<()> {
        if self
            .request
            .cancel
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
        {
            return Err(RecallError::Cancelled);
        }
        if self.started.elapsed().as_millis() >= u128::from(self.request.limits.graph.deadline_ms) {
            return Err(RecallError::Deadline);
        }
        Ok(())
    }

    fn candidates(&mut self, count: usize) -> RecallResult<()> {
        self.check()?;
        charge(
            &mut self.stats.candidates_examined,
            count,
            self.request.limits.candidates,
            "candidates",
        )
    }

    fn bytes(&mut self, count: usize) -> RecallResult<()> {
        self.check()?;
        charge(
            &mut self.stats.evidence_bytes,
            count,
            self.request.limits.evidence_bytes,
            "evidence_bytes",
        )
    }
}

fn charge(used: &mut usize, amount: usize, cap: usize, name: &'static str) -> RecallResult<()> {
    let next = used.checked_add(amount).ok_or(RecallError::Budget(name))?;
    if next > cap {
        return Err(RecallError::Budget(name));
    }
    *used = next;
    Ok(())
}

fn validate(request: &RecallRequest<'_>) -> RecallResult<i64> {
    if request.scopes.is_empty() || request.scopes.len() > 16 {
        return Err(RecallError::InvalidScopes);
    }
    let mut keys = BTreeSet::new();
    for scope in &request.scopes {
        scope.validate()?;
        if !keys.insert(scope.key()) {
            return Err(RecallError::InvalidScopes);
        }
    }
    let limits = &request.limits;
    if !(1..=1000).contains(&limits.candidates)
        || !(1..=100_000).contains(&limits.source_records)
        || !(1..=4_000_000).contains(&limits.vector_components)
        || !(1..=4 * 1024 * 1024).contains(&limits.evidence_bytes)
        || !(1..=100_000_000).contains(&limits.mmr_components)
        || request.snippet_bytes > 4096
        || request.kinds.is_empty()
        || request.kinds.contains(&MemoryKind::Unsupported)
    {
        return Err(RecallError::InvalidLimits);
    }
    limits
        .graph
        .validate()
        .map_err(|_| RecallError::InvalidLimits)?;
    crate::metadata::validate_filter(&request.filter)?;
    if request.query.is_none() && request.seeds.is_empty() {
        return Err(RecallError::InvalidQuery);
    }
    for seed in &request.seeds {
        if !opaque(&seed.id) || !request.scopes.contains(&seed.scope) {
            return Err(RecallError::InvalidScopes);
        }
    }
    if request.seeds.len() > limits.candidates {
        return Err(RecallError::Budget("candidates"));
    }
    if let Some(query) = &request.query {
        if query.vector_leg != VectorLeg::Exact {
            // Existing ANN filters cannot express an authoritative attachment-owner
            // allowlist before discovery. Refuse rather than post-filter or leak.
            return Err(RecallError::UnsupportedPlan);
        }
        if query.k == 0 || query.k > limits.candidates || query.text.len() > MAX_HYBRID_TEXT_BYTES {
            return Err(RecallError::InvalidQuery);
        }
        let tokens = crate::text_index::tokenize(&query.text)
            .into_iter()
            .collect::<BTreeSet<_>>();
        if tokens.len() > MAX_HYBRID_TOKENS
            || ((query.vector.is_none() || query.fusion.vector_weight == 0.0)
                && (tokens.is_empty() || query.fusion.text_weight == 0.0))
        {
            return Err(RecallError::InvalidQuery);
        }
        crate::metadata::validate_filter(&query.conditions)?;
        // Validate the shared fusion policy before any source work.
        fuse_ranked(&[], &[], query.k, query.fusion)?;
    }
    if request.selection != Selection::Ranked {
        let Some(query) = &request.query else {
            return Err(RecallError::UnsupportedPlan);
        };
        if query.vector.is_none()
            || query.fusion.vector_weight == 0.0
            || !query.text.is_empty()
            || !request.seeds.is_empty()
            || request.expand
            || request.policy.weights != [1.0, 0.0, 0.0]
        {
            return Err(RecallError::UnsupportedPlan);
        }
    }
    if let Selection::Mmr { lambda } = request.selection
        && (!lambda.is_finite() || !(0.0..=1.0).contains(&lambda))
    {
        return Err(RecallError::InvalidQuery);
    }
    // Validate policy even for an empty union.
    ranking::rank(Vec::new(), 0, &request.policy)?;
    request.as_of_ms.map_or_else(
        || i64::try_from(request.clock_ms).map_err(|_| RecallError::InvalidQuery),
        Ok,
    )
}

fn scope_cmp(a: &MemoryScope, b: &MemoryScope) -> std::cmp::Ordering {
    (&a.tenant, &a.agent, &a.user, &a.session).cmp(&(&b.tenant, &b.agent, &b.user, &b.session))
}

impl MemoryRef<'_> {
    fn recall_collection(&self) -> crate::Result<Option<crate::CollectionRef<'_>>> {
        let collection = self
            .evidence_collection
            .as_deref()
            .map(|name| self.db.collection(name))
            .transpose()?;
        if let Some(collection) = collection {
            self.db.state(collection.id()).records.resident()?;
        }
        Ok(collection)
    }

    pub(super) fn recall(&self, request: &RecallRequest<'_>) -> RecallResult<RecallOutput> {
        let mut work = Work {
            request,
            started: Instant::now(),
            stats: RecallStats::default(),
        };
        work.check()?;
        let time = validate(request)?;
        self.db.ensure_healthy()?;
        let graph = self.db.graph(&self.graph)?.with_clock(request.clock_ms);
        let collection = self.recall_collection()?;
        let scan = graph
            .memory_scan_bound()
            .checked_mul(request.scopes.len())
            .and_then(|n| n.checked_add(collection.map_or(0, |c| c.len())))
            .ok_or(RecallError::Budget("source_records"))?;
        if scan > request.limits.source_records {
            return Err(RecallError::Budget("source_records"));
        }
        work.stats.source_records_charged = scan;
        // Bound all node discovery before cloning properties or scanning filters.
        let mut eligible = BTreeMap::<String, (MemoryScope, NodeView)>::new();
        for scope in &request.scopes {
            work.check()?;
            let Some(scope_node) = graph.get_node(&scope.key())?.output else {
                return Err(MemoryError::InvalidScope.into());
            };
            if string_property(&scope_node, "tuple")? != hex(&scope.encode()) {
                return Err(MemoryError::DigestCollision.into());
            }
            let mut cursor = None;
            loop {
                work.check()?;
                let page = graph.find_nodes(
                    &FindNodes {
                        label: Some(LABEL_MEMORY.into()),
                        conditions: {
                            let mut conditions = mandatory_filter(scope, &request.kinds).conditions;
                            conditions.extend(request.filter.clone());
                            conditions
                        },
                        limit: Some(1000),
                        cursor,
                    },
                    &ReadOptions {
                        limits: GraphLimits {
                            max_edges_examined: request.limits.source_records as u64,
                            deadline_ms: work.remaining_ms()?,
                            ..GraphLimits::CEILING
                        },
                        projection: PropertyProjection::Only(
                            [
                                "scope",
                                "id",
                                "kind",
                                "content_digest",
                                "sources",
                                "provenance",
                                "memory_revision",
                                "importance",
                                "created_at_ms",
                                "observed_at_ms",
                                "reference_count",
                                "evidence",
                            ]
                            .into_iter()
                            .map(str::to_owned)
                            .collect(),
                        ),
                        cancel: request.cancel,
                    },
                )?;
                for node in page.output {
                    work.check()?;
                    let memory = existing_memory(&node)?;
                    if request.kinds.iter().any(|kind| kind.label() == memory.kind) {
                        if memory_key(&scope.key(), &memory.id) != node.key {
                            return Err(MemoryError::DigestCollision.into());
                        }
                        eligible.insert(node.key.clone(), (scope.clone(), node));
                    }
                }
                match page.cursor {
                    Some(next) => cursor = Some(next),
                    None if page.complete => break,
                    None => return Err(RecallError::Budget("source_scan")),
                }
            }
        }
        let mut owners = BTreeMap::<u64, String>::new();
        if let Some(collection) = collection {
            for (key, (_, node)) in &eligible {
                work.check()?;
                let memory = existing_memory(node)?;
                for id in &memory.evidence {
                    let record = evidence_record_id(id);
                    if self.db.memory_attachment_owner(
                        collection.name(),
                        record,
                        request.clock_ms,
                    )? != Some((self.graph.clone(), key.clone()))
                    {
                        // Corrupt/outside attachments never authorize evidence.
                        continue;
                    }
                    if let Some(record_value) = collection.get_at(record, request.clock_ms)?
                        && request.query.as_ref().is_none_or(|query| {
                            crate::metadata::matches(&record_value.metadata, &query.conditions)
                        })
                    {
                        owners.insert(record, key.clone());
                    }
                }
            }
        }
        let mut candidates = BTreeMap::<String, Candidate>::new();
        if let Some(query) = &request.query {
            let collection = collection.ok_or(MemoryError::InvalidEvidence)?;
            let depth = query.fusion.candidates.unwrap_or(query.k.max(50));
            let mut vectors = Vec::new();
            let dense = if let Some(vector) = query
                .vector
                .as_ref()
                .filter(|_| query.fusion.vector_weight > 0.0)
            {
                crate::validate_vector(collection.dimensions(), vector)?;
                crate::validate_metric_query(vector, query.metric)?;
                let components = owners
                    .len()
                    .checked_mul(collection.dimensions())
                    .ok_or(RecallError::Budget("vector_components"))?;
                charge(
                    &mut work.stats.vector_components,
                    components,
                    request.limits.vector_components,
                    "vector_components",
                )?;
                work.candidates(depth.min(owners.len()))?;
                work.check()?;
                for id in owners.keys() {
                    work.check()?;
                    let record = collection
                        .get_at(*id, request.clock_ms)?
                        .ok_or(MemoryError::InvalidBundle)?;
                    if record.vector.is_empty() {
                        continue;
                    }
                    vectors.push(RerankCandidate {
                        id: *id,
                        vector: record.vector.clone(),
                        payload: Vec::new(),
                        distance: 0.0,
                    });
                }
                ExactReranker {
                    metric: query.metric,
                }
                .rerank(vector, &vectors, depth)?
            } else {
                Vec::new()
            };
            work.check()?;
            let terms = crate::text_index::tokenize(&query.text)
                .into_iter()
                .collect::<BTreeSet<_>>();
            let text = if !terms.is_empty() && query.fusion.text_weight > 0.0 {
                work.candidates(depth.min(owners.len()))?;
                let index = &self.db.state(collection.id()).text_index;
                let status = index.status();
                if status.declared_fields == 0 {
                    return Err(RecallError::InvalidQuery);
                }
                if !status.available {
                    return Err(crate::Error::IndexNotReady.into());
                }
                // At most 32 posting lists, each bounded by the preflighted
                // collection size. Filtering occurs before leg top-N selection.
                index.search(&terms, depth, |id| owners.contains_key(&id))
            } else {
                Vec::new()
            };
            work.check()?;
            let sparse = text
                .iter()
                .map(|(id, score)| SearchHit {
                    id: *id,
                    distance: *score,
                })
                .collect::<Vec<_>>();
            let fused = fuse_ranked(&dense, &sparse, query.k, query.fusion)?;
            for (index, hit) in fused.into_iter().enumerate() {
                let key = owners.get(&hit.id).ok_or(MemoryError::InvalidBundle)?;
                candidates.insert(
                    key.clone(),
                    Candidate {
                        rank: Some(index + 1),
                        distance: hit.distance,
                        bm25: text
                            .iter()
                            .find(|(id, _)| *id == hit.id)
                            .map(|(_, score)| *score),
                        vector_rank: hit.vector_rank,
                        text_rank: hit.text_rank,
                        paths: Vec::new(),
                    },
                );
            }
            if request.selection != Selection::Ranked {
                let selected_ids = dense.iter().map(|hit| hit.id).collect::<BTreeSet<_>>();
                vectors.retain(|candidate| selected_ids.contains(&candidate.id));
                let selected = match request.selection {
                    Selection::Exact => dense.iter().take(query.k).cloned().collect(),
                    Selection::Mmr { lambda } => {
                        // Existing MMR recomputes redundancy for every prior selection.
                        // n^3*d is a conservative bound on its total component work.
                        let n = vectors.len();
                        let cost = n
                            .checked_mul(n)
                            .and_then(|n2| n2.checked_mul(n))
                            .and_then(|n3| n3.checked_mul(collection.dimensions()))
                            .ok_or(RecallError::Budget("mmr_components"))?;
                        charge(
                            &mut work.stats.mmr_components,
                            cost,
                            request.limits.mmr_components,
                            "mmr_components",
                        )?;
                        MmrReranker { lambda }.rerank(
                            query.vector.as_ref().unwrap(),
                            &vectors,
                            request.policy.limit.min(query.k),
                        )?
                    }
                    Selection::Ranked => unreachable!(),
                };
                candidates.clear();
                for (index, hit) in selected.into_iter().enumerate() {
                    let key = owners.get(&hit.id).ok_or(MemoryError::InvalidBundle)?;
                    let (leg_position, leg_hit) = dense
                        .iter()
                        .enumerate()
                        .find(|(_, leg_hit)| leg_hit.id == hit.id)
                        .ok_or(MemoryError::InvalidBundle)?;
                    candidates.insert(
                        key.clone(),
                        Candidate {
                            rank: Some(index + 1),
                            distance: Some(leg_hit.distance),
                            bm25: None,
                            vector_rank: Some((leg_position + 1) as u32),
                            text_rank: None,
                            paths: Vec::new(),
                        },
                    );
                }
            }
        }
        for seed in &request.seeds {
            work.candidates(1)?;
            let key = memory_key(&seed.scope.key(), &seed.id);
            let Some((_, node)) = eligible.get(&key) else {
                return Err(MemoryError::MissingMemory.into());
            };
            if string_property(node, "kind")? != "entity" {
                return Err(RecallError::InvalidQuery);
            }
            candidates.entry(key).or_default();
        }
        let mut truncation = Vec::new();
        if request.expand {
            self.expand(
                request,
                time,
                &eligible,
                &mut candidates,
                &mut work,
                &mut truncation,
            )?;
        }
        let mut ranked = Vec::new();
        let mut by_identity = BTreeMap::new();
        for (key, candidate) in &candidates {
            work.check()?;
            let (scope, node) = eligible.get(key).ok_or(MemoryError::InvalidBundle)?;
            let memory = existing_memory(node)?;
            by_identity.insert((scope.key(), memory.id.clone()), key.clone());
            ranked.push(RankedCandidate {
                scope: scope.clone(),
                id: memory.id,
                observed_at_ms: memory.observed_at_ms,
                rank: candidate.rank,
                importance: memory.importance,
                segment: None,
            });
        }
        let scored = ranking::rank(ranked, time, &request.policy)?;
        let mut hits = Vec::new();
        for ranking in scored {
            work.check()?;
            let key = by_identity
                .get(&(ranking.candidate.scope.key(), ranking.candidate.id.clone()))
                .ok_or(MemoryError::InvalidBundle)?;
            let (_, node) = &eligible[key];
            let memory = existing_memory(node)?;
            let mut provenance = memory.provenance;
            provenance.sort_by(|a, b| {
                (&a.source, &a.kind, &a.uri, &a.transformation).cmp(&(
                    &b.source,
                    &b.kind,
                    &b.uri,
                    &b.transformation,
                ))
            });
            let mut evidence = Vec::new();
            if let Some(collection) = collection {
                for id in &memory.evidence {
                    let record_id = evidence_record_id(id);
                    if owners.get(&record_id) != Some(key) {
                        continue;
                    }
                    let record = collection
                        .get_at(record_id, request.clock_ms)?
                        .ok_or(MemoryError::InvalidBundle)?;
                    let length = record.payload.len().min(request.snippet_bytes);
                    work.bytes(length)?;
                    evidence.push(RecallEvidence {
                        id: id.clone(),
                        record_id,
                        revision: collection
                            .revision(record_id)?
                            .ok_or(MemoryError::InvalidBundle)?,
                        snippet: record.payload[..length].to_vec(),
                        truncated: length < record.payload.len(),
                    });
                }
            }
            evidence.sort_by(|a, b| a.id.cmp(&b.id));
            let (content, content_truncated) =
                content_excerpt(&graph, node, request.snippet_bytes, &mut work)?;
            let candidate = &candidates[key];
            let stale = self.recall_stale_label(
                request,
                &ranking.candidate.scope,
                &ranking.candidate.id,
                work.remaining_ms()?,
                &mut work.stats.edges_charged,
                &mut truncation,
            )?;
            hits.push(RecallHit {
                ranking,
                memory_revision: memory.memory_revision,
                graph_revision: node.revision,
                provenance,
                evidence,
                content,
                content_truncated,
                paths: candidate.paths.clone(),
                distance: candidate.distance,
                bm25: candidate.bm25,
                vector_rank: candidate.vector_rank,
                text_rank: candidate.text_rank,
                stale,
            });
        }
        work.check()?;
        let mut scopes = request.scopes.clone();
        scopes.sort_by(scope_cmp);
        Ok(RecallOutput {
            hits,
            scopes,
            as_of_ms: time,
            clock_ms: request.clock_ms,
            sequence: self.db.sequence,
            stats: work.stats,
            complete: truncation.is_empty(),
            truncation,
            selection: request.selection,
            candidate_mode: "exact-eligible-evidence-union",
            historical_discovery: "retained current anchors and evidence only; not exhaustive history",
        })
    }

    fn expand(
        &self,
        request: &RecallRequest<'_>,
        time: i64,
        eligible: &BTreeMap<String, (MemoryScope, NodeView)>,
        candidates: &mut BTreeMap<String, Candidate>,
        work: &mut Work<'_>,
        truncation: &mut Vec<TruncationReason>,
    ) -> RecallResult<()> {
        let graph = self.db.graph(&self.graph)?.with_clock(request.clock_ms);
        let mut visited = 0;
        let mut output_nodes = 0;
        let mut output_edges = 0;
        let mut output_paths = 0;
        let mut scopes = request.scopes.iter().collect::<Vec<_>>();
        scopes.sort_by(|a, b| scope_cmp(a, b));
        for scope in scopes {
            work.check()?;
            let start = candidates
                .keys()
                .filter(|key| eligible[*key].0 == *scope)
                .map(|key| NodeSelector::key(key))
                .collect::<Vec<_>>();
            if start.is_empty() {
                continue;
            }
            let remaining_edges = request
                .limits
                .graph
                .max_edges_examined
                .saturating_sub(work.stats.edges_charged);
            let remaining_nodes = request
                .limits
                .graph
                .max_visited_nodes
                .saturating_sub(visited);
            if remaining_edges == 0 || remaining_nodes == 0 {
                if !request.allow_partial {
                    return Err(RecallError::Budget("graph"));
                }
                if remaining_edges == 0 {
                    truncation.push(TruncationReason::EdgeLimit);
                }
                if remaining_nodes == 0 {
                    truncation.push(TruncationReason::VisitedLimit);
                }
                break;
            }
            let traversal = Traverse {
                start,
                direction: crate::property_graph::Direction::Out,
                types: Some(vec!["REFERENCES".into()]),
                expand_filter: mandatory_filter(scope, &request.kinds),
                return_filter: {
                    let mut filter = mandatory_filter(scope, &request.kinds);
                    filter.conditions.extend(request.filter.clone());
                    filter
                },
                edge_filter: Vec::new(),
                include_paths: true,
            };
            let options = ReadOptions {
                limits: GraphLimits {
                    max_edges_examined: remaining_edges,
                    max_visited_nodes: remaining_nodes,
                    max_output_nodes: request
                        .limits
                        .graph
                        .max_output_nodes
                        .saturating_sub(output_nodes),
                    max_output_edges: request
                        .limits
                        .graph
                        .max_output_edges
                        .saturating_sub(output_edges),
                    max_output_paths: request
                        .limits
                        .graph
                        .max_output_paths
                        .saturating_sub(output_paths),
                    deadline_ms: work.remaining_ms()?,
                    ..request.limits.graph
                },
                projection: PropertyProjection::None,
                cancel: request.cancel,
            };
            let read = if self.db.flags & crate::format::FLAG_TEMPORAL != 0 {
                graph.as_of(time)?.traverse(&traversal, &options)?.read
            } else {
                graph.traverse(&traversal, &options)?
            };
            work.check()?;
            work.stats.edges_charged += read.stats.edges_examined;
            visited += read.stats.nodes_visited;
            output_nodes += read.output.nodes.len() as u64;
            output_edges += read.output.edges.len() as u64;
            output_paths += read.output.paths.len() as u64;
            if !read.complete && !request.allow_partial {
                return Err(RecallError::Budget("graph"));
            }
            truncation.extend(read.truncation);
            for reached in read.output.nodes {
                // Starts were charged by their discovery leg or explicit seed.
                // Traversal work still charges them against graph limits.
                if reached.depth == 0 {
                    continue;
                }
                work.candidates(1)?;
                if !eligible.contains_key(&reached.node.key) {
                    continue;
                }
                let candidate = candidates.entry(reached.node.key).or_default();
                for path in &read.output.paths {
                    if path.nodes.last() == Some(&reached.node.id) {
                        candidate.paths.push(path.clone());
                    }
                }
                candidate
                    .paths
                    .sort_by(|a, b| (&a.edges, &a.nodes).cmp(&(&b.edges, &b.nodes)));
                candidate.paths.dedup();
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct Candidate {
    rank: Option<usize>,
    distance: Option<f64>,
    bm25: Option<f64>,
    vector_rank: Option<u32>,
    text_rank: Option<u32>,
    paths: Vec<GraphPath>,
}

fn excerpt_length(text: &str, cap: usize) -> usize {
    let mut length = cap.min(text.len());
    while !text.is_char_boundary(length) {
        length -= 1;
    }
    length
}

fn mandatory_filter(scope: &MemoryScope, kinds: &[MemoryKind]) -> NodeFilter {
    let mut conditions = vec![Condition::Eq {
        field: "scope".into(),
        value: MetadataValue::String(scope.key()),
    }];
    if kinds.iter().all(|kind| *kind == kinds[0]) {
        conditions.push(Condition::Eq {
            field: "kind".into(),
            value: MetadataValue::String(kinds[0].label().into()),
        });
    }
    NodeFilter {
        labels: vec![LABEL_MEMORY.into()],
        conditions,
    }
}

fn content_excerpt(
    graph: &crate::property_graph::GraphRef<'_>,
    node: &NodeView,
    cap: usize,
    work: &mut Work<'_>,
) -> RecallResult<(String, bool)> {
    if let Some(content) = graph.memory_string_property(&node.key, "content") {
        let length = excerpt_length(content, cap);
        work.bytes(length)?;
        return Ok((content[..length].to_owned(), length < content.len()));
    }
    // Chunked content exceeds the largest projection (4096 bytes), so only
    // the first chunk is needed; do not reconstruct an unbounded body.
    let chunk_key = content_key(&node.key, 0);
    let edge = graph
        .get_edge_by_key(&content_edge_key(&node.key, 0))?
        .output
        .ok_or(MemoryError::InvalidBundle)?;
    if edge.source_key != node.key
        || edge.target_key != chunk_key
        || edge.relationship != "HAS_CONTENT"
    {
        return Err(MemoryError::InvalidBundle.into());
    }
    if graph.memory_string_property(&chunk_key, "scope") != Some(string_property(node, "scope")?) {
        return Err(MemoryError::InvalidBundle.into());
    }
    let text = graph
        .memory_string_property(&chunk_key, "content")
        .ok_or(MemoryError::InvalidBundle)?;
    let length = excerpt_length(text, cap);
    work.bytes(length)?;
    Ok((text[..length].to_owned(), true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{
        MemoryEvidence, MemoryItem, MemoryMut, RememberRequest, ranking::FutureObservations,
    };
    use crate::{Database, Metadata, Mutation};
    use std::{fs, path::Path};

    fn scope(tenant: &str) -> MemoryScope {
        MemoryScope {
            tenant: tenant.into(),
            agent: "agent".into(),
            user: None,
            session: None,
        }
    }

    fn database(path: &Path) -> Database {
        let mut db = Database::create(path, 2).unwrap();
        db.create_graph("memory").unwrap();
        db.set_collection_settings(
            "default",
            crate::CollectionSettings {
                text_fields: vec!["text".into()],
                ..crate::CollectionSettings::default()
            },
        )
        .unwrap();
        db
    }

    #[allow(clippy::too_many_arguments)]
    fn remember(
        db: &mut Database,
        scope: &MemoryScope,
        id: &str,
        vector: Option<Vec<f32>>,
        now: u64,
        expiry: Option<u64>,
        importance: f64,
        references: Vec<String>,
    ) -> RememberRequest {
        MemoryMut::bind(db, "memory", None)
            .unwrap()
            .provision_scope(scope)
            .unwrap();
        let evidence = if let Some(vector) = vector {
            let record = evidence_record_id(id);
            let mut metadata = Metadata::new();
            metadata.insert("text".into(), MetadataValue::String(format!("memory {id}")));
            db.graph_mut("memory")
                .unwrap()
                .write_batch(&[crate::property_graph::GraphOp::Vector {
                    collection: "default".into(),
                    mutation: Mutation::PutWithMetadata {
                        id: record,
                        vector,
                        payload: id.as_bytes().to_vec(),
                        metadata,
                    },
                }])
                .unwrap();
            vec![MemoryEvidence {
                id: id.into(),
                scope: scope.clone(),
            }]
        } else {
            Vec::new()
        };
        let req = RememberRequest {
            scope: scope.clone(),
            idempotency_key: format!("write-{id}"),
            receipt_window_ms: 1000,
            now_ms: now,
            deadline_ms: None,
            allow_future_observations: false,
            items: vec![MemoryItem {
                id: id.into(),
                kind: MemoryKind::Entity,
                content: format!("content {id}"),
                source: "source".into(),
                importance: Some(importance),
                observed_at_ms: Some(now as i64 - 1000),
                provenance: Vec::new(),
                references,
                merge_into: None,
                expected_revision: None,
                expiry_ms: expiry,
                expiry_valid: true,
                evidence,
            }],
        };
        MemoryMut::bind(db, "memory", Some("default"))
            .unwrap()
            .remember(&req)
            .unwrap();
        req
    }

    fn request(scope: MemoryScope, now: u64) -> RecallRequest<'static> {
        RecallRequest {
            scopes: vec![scope],
            kinds: vec![MemoryKind::Entity, MemoryKind::Episodic],
            filter: Vec::new(),
            query: Some(HybridQuery::new(Some(vec![1.0, 0.0]), "", 20)),
            seeds: Vec::new(),
            as_of_ms: None,
            clock_ms: now,
            policy: RankingPolicy::default(),
            selection: Selection::Ranked,
            limits: RecallLimits {
                graph: GraphLimits {
                    deadline_ms: 5_000,
                    ..GraphLimits::default()
                },
                ..RecallLimits::default()
            },
            expand: false,
            allow_partial: false,
            snippet_bytes: 512,
            cancel: None,
        }
    }

    fn recall(db: &Database, req: &RecallRequest<'_>) -> RecallResult<RecallOutput> {
        MemoryRef::bind(db, "memory", Some("default"))
            .unwrap()
            .recall(req)
    }

    fn ids(result: &RecallOutput) -> Vec<&str> {
        result
            .hits
            .iter()
            .map(|hit| hit.ranking.candidate.id.as_str())
            .collect()
    }

    #[test]
    fn recall_collection_dispatch_refuses_compact_and_preserves_mixed_resident_targets() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("dispatch.vlite"));
        let now = crate::current_time_ms() + 30_000;
        remember(
            &mut db,
            &scope("tenant"),
            "evidence",
            Some(vec![1.0, 0.0]),
            now,
            None,
            1.0,
            Vec::new(),
        );
        let req = request(scope("tenant"), now);
        assert_eq!(ids(&recall(&db, &req).unwrap()), ["evidence"]);
        let compact_id = db
            .create_collection(
                "compact",
                crate::CollectionSpec {
                    dimensions: 2,
                    settings: crate::CollectionSettings::default(),
                },
            )
            .unwrap()
            .id;
        let generation = crate::compact::source::Capture::default()
            .finish(None)
            .unwrap();
        db.state_mut(compact_id).records = crate::records::CollectionRecords::Compact {
            view: generation.view(compact_id),
            overlay: Default::default(),
        };
        let sequence = db.sequence;
        let bytes = db.to_bytes();
        // Isolate the evidence seam: public graph acquisition already refuses
        // mixed handles in 1a, independently of the evidence target.
        let binding = |collection: Option<&str>| MemoryRef {
            db: &db,
            graph: "memory".into(),
            evidence_collection: collection.map(str::to_owned),
        };
        assert!(
            matches!(
                binding(Some("compact")).recall_collection(),
                Err(crate::Error::UnsupportedFeature(
                    crate::format::FLAG_COMPACT
                ))
            ),
            "recall evidence acquisition must refuse a compact target"
        );
        let resident = binding(Some("default"));
        let collection = resident
            .recall_collection()
            .expect("resident evidence acquisition must ignore unrelated compact collections")
            .unwrap();
        assert_eq!(collection.name(), "default");
        let id = evidence_record_id("evidence");
        assert_eq!(
            collection.get_at(id, now).unwrap().unwrap().payload,
            b"evidence",
            "resident evidence must remain readable in a mixed handle"
        );
        let text = db.state(collection.id()).text_index.status();
        assert!(text.available);
        assert_eq!(text.declared_fields, 1);
        assert_eq!(text.documents, 1);
        assert!(binding(None).recall_collection().unwrap().is_none());
        assert!(
            matches!(
                binding(Some("missing")).recall_collection(),
                Err(crate::Error::CollectionNotFound)
            ),
            "missing evidence target must retain its resident error"
        );
        assert!(
            matches!(
                resident.recall(&req),
                Err(RecallError::Storage(crate::Error::UnsupportedFeature(
                    crate::format::FLAG_COMPACT
                )))
            ),
            "full recall must retain the existing graph-acquisition refusal"
        );
        assert_eq!(db.sequence, sequence, "recall refusal sequence");
        assert_eq!(db.to_bytes(), bytes, "recall refusal bytes");
    }

    #[test]
    fn recall_filters_scope_kind_and_owner_before_candidate_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("scope.vlite"));
        let now = crate::current_time_ms() + 30_000;
        remember(
            &mut db,
            &scope("secret"),
            "secret",
            Some(vec![1.0, 0.0]),
            now,
            None,
            1.0,
            vec![],
        );
        remember(
            &mut db,
            &scope("visible"),
            "visible",
            Some(vec![2.0, 0.0]),
            now,
            None,
            0.5,
            vec![],
        );
        let mut req = request(scope("visible"), now);
        req.query.as_mut().unwrap().k = 1;
        req.query.as_mut().unwrap().fusion.candidates = Some(1);
        let out = recall(&db, &req).unwrap();
        assert_eq!(ids(&out), ["visible"]);
        assert_eq!(out.hits[0].distance, Some(1.0));
        assert_eq!(out.stats.candidates_examined, 1);
        assert_eq!(out.scopes, vec![scope("visible")]);

        req.filter.push(Condition::Eq {
            field: "importance".into(),
            value: MetadataValue::Float(1.0),
        });
        assert!(recall(&db, &req).unwrap().hits.is_empty());
        req.filter.clear();
        req.kinds = vec![MemoryKind::Episodic];
        assert!(recall(&db, &req).unwrap().hits.is_empty());

        // Duplicated/forged metadata cannot make an outside owner authoritative.
        db.graph_mut("memory")
            .unwrap()
            .write_batch(&[
                crate::property_graph::GraphOp::Detach {
                    node: memory_key(&scope("visible").key(), "visible"),
                    collection: "default".into(),
                },
                crate::property_graph::GraphOp::CreateNode {
                    key: "outside".into(),
                    labels: vec!["Outside".into()],
                    properties: Metadata::new(),
                },
                crate::property_graph::GraphOp::Attach {
                    node: "outside".into(),
                    collection: "default".into(),
                    record: evidence_record_id("visible"),
                },
            ])
            .unwrap();
        req.kinds = vec![MemoryKind::Entity];
        assert!(recall(&db, &req).unwrap().hits.is_empty());
    }

    #[test]
    fn recall_explains_hybrid_ranks_and_graph_only_zero_relevance() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("hybrid.vlite"));
        let now = crate::current_time_ms() + 30_000;
        let scope = scope("tenant");
        remember(&mut db, &scope, "expanded", None, now, None, 1.0, vec![]);
        remember(
            &mut db,
            &scope,
            "pivot",
            Some(vec![1.0, 0.0]),
            now,
            None,
            0.5,
            vec!["expanded".into()],
        );
        let mut req = request(scope.clone(), now);
        req.query.as_mut().unwrap().text = "pivot".into();
        req.expand = true;
        let out = recall(&db, &req).unwrap();
        assert_eq!(ids(&out), ["pivot", "expanded"]);
        assert_eq!(out.hits[0].ranking.relevance, 1.0);
        assert_eq!(out.hits[0].vector_rank, Some(1));
        assert_eq!(out.hits[0].text_rank, Some(1));
        assert!(out.hits[0].bm25.unwrap() > 0.0);
        assert_eq!(out.hits[1].ranking.relevance, 0.0);
        assert!(out.hits[1].ranking.expanded_only);
        assert_eq!(out.hits[1].paths.len(), 1);
        assert!(out.stats.edges_charged >= 1);
        req.policy.weights = [0.0, 0.0, 1.0];
        assert_eq!(ids(&recall(&db, &req).unwrap()), ["expanded", "pivot"]);

        req.query = None;
        req.expand = false;
        req.seeds = vec![RecallSeed {
            scope,
            id: "expanded".into(),
        }];
        assert_eq!(ids(&recall(&db, &req).unwrap()), ["expanded"]);
    }

    #[test]
    fn recall_expiry_at_c_is_independent_of_t_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let key = crate::EncryptionKey::from_bytes([31; 32]);
        for (name, encrypted, directory) in [
            ("plain.vlite", false, false),
            ("encrypted.vlite", true, false),
            ("plain-dir", false, true),
            ("encrypted-dir", true, true),
        ] {
            let path = dir.path().join(name);
            let options = crate::CreateOptions {
                encryption: encrypted.then(|| key.clone()),
                ..crate::CreateOptions::default()
            };
            let mut db = if directory {
                Database::create_directory(&path, 2, options).unwrap()
            } else {
                Database::create_with_options(&path, 2, options).unwrap()
            };
            db.create_graph("memory").unwrap();
            let now = crate::current_time_ms() + 30_000;
            let expires = now + 100;
            let scope = scope("tenant");
            let write = remember(
                &mut db,
                &scope,
                "m",
                Some(vec![1.0, 0.0]),
                now,
                Some(expires),
                0.5,
                vec![],
            );
            let mut req = request(scope, expires - 1);
            req.as_of_ms = Some(now as i64 - 500);
            let before = recall(&db, &req).unwrap();
            assert_eq!(ids(&before), ["m"], "{name}");
            assert_eq!(before.hits[0].evidence[0].snippet, b"m");
            let sequence = db.sequence;
            MemoryMut::bind(&mut db, "memory", Some("default"))
                .unwrap()
                .remember(&write)
                .unwrap();
            assert_eq!(db.sequence, sequence);
            assert_eq!(recall(&db, &req).unwrap(), before);
            db.close().unwrap();
            let db = if encrypted {
                Database::open_encrypted(&path, &key).unwrap()
            } else {
                Database::open(&path).unwrap()
            };
            assert_eq!(recall(&db, &req).unwrap(), before, "{name}");
            req.clock_ms = expires;
            let after = recall(&db, &req).unwrap();
            assert!(after.hits.is_empty(), "{name}");
            assert!(after.complete);
            assert!(db.get(evidence_record_id("m")).unwrap().is_some(), "{name}");
        }
    }

    #[test]
    fn recall_source_vector_candidate_and_byte_budgets_are_precharged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("budgets.vlite");
        let mut db = database(&path);
        let now = crate::current_time_ms() + 30_000;
        let scope = scope("tenant");
        remember(
            &mut db,
            &scope,
            "a",
            Some(vec![1.0, 0.0]),
            now,
            None,
            0.5,
            vec![],
        );
        remember(
            &mut db,
            &scope,
            "b",
            Some(vec![2.0, 0.0]),
            now,
            None,
            0.5,
            vec![],
        );
        db.sync().unwrap();
        db.close().unwrap();
        let bytes = fs::read(&path).unwrap();
        let db = Database::open(&path).unwrap();
        let sequence = db.sequence;
        let mut req = request(scope, now);
        let exact_scan = db.graph("memory").unwrap().memory_scan_bound() + 2;
        req.limits.source_records = exact_scan - 1;
        assert!(matches!(
            recall(&db, &req),
            Err(RecallError::Budget("source_records"))
        ));
        req.limits.source_records = exact_scan;
        req.limits.vector_components = 3;
        assert!(matches!(
            recall(&db, &req),
            Err(RecallError::Budget("vector_components"))
        ));
        req.limits.vector_components = 4;
        assert_eq!(recall(&db, &req).unwrap().stats.vector_components, 4);
        req.query.as_mut().unwrap().k = 1;
        req.query.as_mut().unwrap().fusion.candidates = Some(2);
        req.limits.candidates = 1;
        assert!(matches!(
            recall(&db, &req),
            Err(RecallError::Budget("candidates"))
        ));
        req.limits.candidates = 2;
        let used = recall(&db, &req).unwrap().stats.evidence_bytes;
        assert_eq!(used, "content a".len() + 1);
        req.limits.evidence_bytes = used - 1;
        assert!(matches!(
            recall(&db, &req),
            Err(RecallError::Budget("evidence_bytes"))
        ));
        req.limits.evidence_bytes = used;
        assert_eq!(recall(&db, &req).unwrap().stats.evidence_bytes, used);
        assert_eq!(db.sequence, sequence);
        db.close().unwrap();
        assert_eq!(fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn recall_limits_cancellation_and_future_policy_reject_explicitly() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("limits.vlite"));
        let now = crate::current_time_ms() + 30_000;
        let scope = scope("tenant");
        remember(
            &mut db,
            &scope,
            "m",
            Some(vec![1.0, 0.0]),
            now,
            None,
            0.5,
            vec![],
        );
        let req = request(scope, now);
        for invalid in [
            RecallLimits {
                candidates: 1001,
                ..req.limits.clone()
            },
            RecallLimits {
                source_records: 100_001,
                ..req.limits.clone()
            },
            RecallLimits {
                vector_components: 4_000_001,
                ..req.limits.clone()
            },
            RecallLimits {
                evidence_bytes: 4 * 1024 * 1024 + 1,
                ..req.limits.clone()
            },
            RecallLimits {
                mmr_components: 100_000_001,
                ..req.limits.clone()
            },
        ] {
            let mut invalid_req = req.clone();
            invalid_req.limits = invalid;
            assert!(matches!(
                recall(&db, &invalid_req),
                Err(RecallError::InvalidLimits)
            ));
        }
        let mut invalid = req.clone();
        invalid.policy.limit = 101;
        assert!(matches!(
            recall(&db, &invalid),
            Err(RecallError::Ranking(ranking::RankingError::Limit))
        ));
        invalid = req.clone();
        invalid.scopes = (0..17).map(|i| self::scope(&i.to_string())).collect();
        assert!(matches!(
            recall(&db, &invalid),
            Err(RecallError::InvalidScopes)
        ));
        let cancel = AtomicBool::new(true);
        let mut cancelled = req.clone();
        cancelled.cancel = Some(&cancel);
        assert!(matches!(
            recall(&db, &cancelled),
            Err(RecallError::Cancelled)
        ));
        let work = Work {
            request: &req,
            started: Instant::now() - std::time::Duration::from_secs(10),
            stats: RecallStats::default(),
        };
        assert!(matches!(work.check(), Err(RecallError::Deadline)));
        invalid = req.clone();
        invalid.as_of_ms = Some(now as i64 - 2000);
        assert!(matches!(
            recall(&db, &invalid),
            Err(RecallError::Ranking(
                ranking::RankingError::FutureObservation
            ))
        ));
        invalid.policy.future_observations = FutureObservations::Clamp;
        assert_eq!(ids(&recall(&db, &invalid).unwrap()), ["m"]);
        invalid.query.as_mut().unwrap().vector_leg = VectorLeg::ApproximateReady {
            config: crate::HnswConfig::default(),
            ef_search: 50,
            options: crate::AnnOptions::default(),
        };
        assert!(matches!(
            recall(&db, &invalid),
            Err(RecallError::UnsupportedPlan)
        ));
    }

    #[test]
    fn recall_mmr_is_vector_only_and_charges_component_work() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("mmr.vlite"));
        let now = crate::current_time_ms() + 30_000;
        let scope = scope("tenant");
        remember(
            &mut db,
            &scope,
            "near",
            Some(vec![1.0, 0.0]),
            now,
            None,
            0.5,
            vec![],
        );
        remember(
            &mut db,
            &scope,
            "diverse",
            Some(vec![0.0, 1.0]),
            now,
            None,
            0.5,
            vec![],
        );
        let mut req = request(scope, now);
        req.selection = Selection::Mmr { lambda: 0.5 };
        req.limits.mmr_components = 15;
        assert!(matches!(
            recall(&db, &req),
            Err(RecallError::Budget("mmr_components"))
        ));
        req.limits.mmr_components = 16;
        let out = recall(&db, &req).unwrap();
        assert_eq!(ids(&out), ["near", "diverse"]);
        assert_eq!(out.stats.mmr_components, 16);
        req.selection = Selection::Exact;
        assert_eq!(ids(&recall(&db, &req).unwrap()), ["near", "diverse"]);
        req.selection = Selection::Mmr { lambda: 0.5 };
        req.query.as_mut().unwrap().text = "memory".into();
        assert!(matches!(
            recall(&db, &req),
            Err(RecallError::UnsupportedPlan)
        ));
        req.query.as_mut().unwrap().text.clear();
        req.policy.weights = [0.5, 0.0, 0.5];
        assert!(matches!(
            recall(&db, &req),
            Err(RecallError::UnsupportedPlan)
        ));
        req.policy.weights = [1.0, 0.0, 0.0];
        req.query.as_mut().unwrap().k = 1;
        for selection in [Selection::Exact, Selection::Mmr { lambda: 0.5 }] {
            req.selection = selection;
            assert_eq!(ids(&recall(&db, &req).unwrap()), ["near"]);
        }
        remember(
            &mut db,
            &req.scopes[0],
            "similar",
            Some(vec![0.9, 0.1]),
            now,
            None,
            0.5,
            vec![],
        );
        req.query.as_mut().unwrap().k = 20;
        req.selection = Selection::Mmr { lambda: 0.25 };
        req.limits.mmr_components = 54;
        let out = recall(&db, &req).unwrap();
        assert_eq!(ids(&out), ["near", "diverse", "similar"]);
        assert_eq!(out.hits[1].ranking.candidate.rank, Some(2));
        assert_eq!(
            out.hits[1].vector_rank,
            Some(3),
            "raw leg rank is not MMR position"
        );
        assert_eq!(
            out.hits[1].distance,
            Some(2.0),
            "raw squared-L2 distance is retained"
        );
    }

    #[test]
    fn recall_graph_work_exhaustion_is_never_an_unmarked_success() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("graph-budget.vlite"));
        let now = crate::current_time_ms() + 30_000;
        let scope = scope("tenant");
        for id in ["a", "b", "c"] {
            remember(&mut db, &scope, id, None, now, None, 0.5, vec![]);
        }
        remember(
            &mut db,
            &scope,
            "root",
            None,
            now,
            None,
            0.5,
            vec!["a".into(), "b".into(), "c".into()],
        );
        let mut req = request(scope.clone(), now);
        req.query = None;
        req.seeds = vec![RecallSeed {
            scope,
            id: "root".into(),
        }];
        req.expand = true;
        req.limits.graph.max_edges_examined = 1;
        assert!(matches!(
            recall(&db, &req),
            Err(RecallError::Budget("graph"))
        ));
        req.allow_partial = true;
        let out = recall(&db, &req).unwrap();
        assert!(!out.complete);
        assert!(!out.truncation.is_empty());
        assert_eq!(out.stats.edges_charged, 1);
    }

    #[test]
    fn recall_expansion_never_uses_foreign_scope_or_kind_as_a_bridge() {
        use crate::property_graph::GraphOp;
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("bridges.vlite"));
        let now = crate::current_time_ms() + 30_000;
        let visible = scope("visible");
        let foreign = scope("foreign");
        for id in ["root", "destination", "episode"] {
            remember(&mut db, &visible, id, None, now, None, 0.5, vec![]);
        }
        remember(&mut db, &foreign, "bridge", None, now, None, 0.5, vec![]);
        let episode = memory_key(&visible.key(), "episode");
        let graph = db.graph("memory").unwrap();
        let mut node = graph.get_node(&episode).unwrap().output.unwrap();
        node.properties
            .insert("kind".into(), MetadataValue::String("episodic".into()));
        let mut ops = vec![GraphOp::UpsertNode {
            key: episode.clone(),
            labels: node.labels.into_iter().collect(),
            properties: node.properties,
            expected: crate::Expected::Revision(node.revision),
        }];
        for (index, (from, to)) in [
            (
                memory_key(&visible.key(), "root"),
                memory_key(&foreign.key(), "bridge"),
            ),
            (
                memory_key(&foreign.key(), "bridge"),
                memory_key(&visible.key(), "destination"),
            ),
            (memory_key(&visible.key(), "root"), episode.clone()),
            (episode, memory_key(&visible.key(), "destination")),
        ]
        .into_iter()
        .enumerate()
        {
            ops.push(GraphOp::CreateEdge {
                key: Some(format!("bridge-{index}")),
                source: from,
                relationship: "REFERENCES".into(),
                destination: to,
                properties: Metadata::new(),
            });
        }
        db.graph_mut("memory").unwrap().write_batch(&ops).unwrap();
        let mut req = request(visible.clone(), now);
        req.query = None;
        req.kinds = vec![MemoryKind::Entity];
        req.seeds = vec![RecallSeed {
            scope: visible,
            id: "root".into(),
        }];
        req.expand = true;
        let out = recall(&db, &req).unwrap();
        assert_eq!(ids(&out), ["root"]);
        assert_eq!(out.stats.edges_charged, 2);
        // Even jointly authorized scopes do not authorize a cross-scope path.
        req.scopes.push(foreign);
        assert_eq!(ids(&recall(&db, &req).unwrap()), ["root"]);
    }

    #[test]
    fn recall_temporal_expansion_uses_half_open_t_independently_of_c() {
        use crate::property_graph::{GraphOp, TemporalOp, TemporalSegment};
        let _activation = crate::property_graph::temporal::Activation::on();
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("temporal-source.vlite");
        let mut db = database(&source);
        let now = crate::current_time_ms() + 30_000;
        let scope = scope("tenant");
        for id in ["root", "target"] {
            remember(&mut db, &scope, id, None, now, None, 0.5, vec![]);
        }
        db.close().unwrap();
        let path = dir.path().join("temporal.vlite");
        crate::property_graph::temporal::enable_temporal(&source, &path, None).unwrap();
        let mut db = Database::open(&path).unwrap();
        db.graph_mut("memory")
            .unwrap()
            .write_batch(&[GraphOp::Temporal(TemporalOp::CreateEdge {
                key: Some("valid-reference".into()),
                source: memory_key(&scope.key(), "root"),
                relationship: "REFERENCES".into(),
                destination: memory_key(&scope.key(), "target"),
                segments: vec![TemporalSegment {
                    valid_from: Some(now as i64),
                    valid_to: Some(now as i64 + 10),
                    properties: Metadata::new(),
                }],
            })])
            .unwrap();
        let mut req = request(scope.clone(), now + 100);
        req.query = None;
        req.seeds = vec![RecallSeed {
            scope,
            id: "root".into(),
        }];
        req.expand = true;
        for (offset, expected) in [
            (-1, vec!["root"]),
            (0, vec!["root", "target"]),
            (9, vec!["root", "target"]),
            (10, vec!["root"]),
        ] {
            req.as_of_ms = Some(now as i64 + offset);
            assert_eq!(
                ids(&recall(&db, &req).unwrap()),
                expected,
                "T offset {offset}"
            );
        }
    }

    #[test]
    fn recall_text_leg_duplicates_and_projection_have_explicit_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("projection.vlite"));
        let now = crate::current_time_ms() + 30_000;
        let scope = scope("tenant");
        let mut write = remember(
            &mut db,
            &scope,
            "text",
            Some(vec![1.0, 0.0]),
            now,
            None,
            0.5,
            vec![],
        );
        let mut req = request(scope.clone(), now);
        req.query.as_mut().unwrap().k = 1;
        req.query.as_mut().unwrap().fusion.candidates = Some(1);
        req.query.as_mut().unwrap().text = "text".into();
        req.limits.candidates = 1;
        assert!(matches!(
            recall(&db, &req),
            Err(RecallError::Budget("candidates"))
        ));
        req.limits.candidates = 2;
        let out = recall(&db, &req).unwrap();
        assert_eq!(
            out.stats.candidates_examined, 2,
            "duplicate across both legs is charged"
        );
        assert_eq!(ids(&out), ["text"]);
        req.query.as_mut().unwrap().vector = None;
        let out = recall(&db, &req).unwrap();
        assert_eq!(out.stats.vector_components, 0);
        assert_eq!(out.hits[0].vector_rank, None);
        assert_eq!(out.hits[0].text_rank, Some(1));
        assert_eq!(out.hits[0].distance, None);

        write.idempotency_key = "large-content".into();
        write.items[0].id = "large".into();
        write.items[0].content = "\u{20ac}".repeat(2_000);
        write.items[0].evidence.clear();
        MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .remember(&write)
            .unwrap();
        req.query = None;
        req.seeds = vec![RecallSeed {
            scope,
            id: "large".into(),
        }];
        req.snippet_bytes = 4;
        req.limits.evidence_bytes = 3;
        let out = recall(&db, &req).unwrap();
        assert_eq!(out.hits[0].content, "\u{20ac}");
        assert!(out.hits[0].content_truncated);
        assert_eq!(out.stats.evidence_bytes, 3);
        req.limits.evidence_bytes = 2;
        assert!(matches!(
            recall(&db, &req),
            Err(RecallError::Budget("evidence_bytes"))
        ));
    }

    #[test]
    fn expansion_candidate_boundary_excludes_already_charged_starts() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("candidate-boundary.vlite"));
        let now = crate::current_time_ms() + 30_000;
        let scope = scope("tenant");
        remember(&mut db, &scope, "a", None, now, None, 0.5, vec![]);
        remember(
            &mut db,
            &scope,
            "root",
            Some(vec![1.0, 0.0]),
            now,
            None,
            0.5,
            vec!["a".into()],
        );
        let mut req = request(scope.clone(), now);
        req.expand = true;
        req.query.as_mut().unwrap().k = 1;
        req.query.as_mut().unwrap().fusion.candidates = Some(1);
        for from_query in [false, true] {
            let mut case = req.clone();
            if !from_query {
                case.query = None;
                case.seeds = vec![RecallSeed {
                    scope: scope.clone(),
                    id: "root".into(),
                }];
            }
            case.limits.candidates = 2;
            let out = recall(&db, &case).unwrap();
            assert_eq!(out.hits.len(), 2, "query start: {from_query}");
            assert_eq!(out.stats.candidates_examined, 2);
            assert!(out.complete);
            case.limits.candidates = 1;
            assert!(matches!(
                recall(&db, &case),
                Err(RecallError::Budget("candidates"))
            ));
        }
        // Both dense pivots are already charged; reaching one from the other
        // does not examine a new candidate or consume a second output allowance.
        remember(
            &mut db,
            &scope,
            "b",
            Some(vec![2.0, 0.0]),
            now,
            None,
            0.5,
            vec!["root".into()],
        );
        req.query.as_mut().unwrap().k = 2;
        req.query.as_mut().unwrap().fusion.candidates = Some(2);
        req.limits.graph.max_hops = 1;
        req.limits.candidates = 3;
        let out = recall(&db, &req).unwrap();
        assert_eq!(out.hits.len(), 3);
        assert_eq!(out.stats.candidates_examined, 3);
    }

    #[test]
    fn expansion_scope_set_order_and_exhaustion_reasons_are_canonical() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("scope-order.vlite"));
        let now = crate::current_time_ms() + 30_000;
        let scopes = [scope("A"), scope("B")];
        for scope in &scopes {
            remember(&mut db, scope, "target", None, now, None, 0.5, vec![]);
            remember(
                &mut db,
                scope,
                "root",
                None,
                now,
                None,
                0.5,
                vec!["target".into()],
            );
        }
        let mut req = request(scopes[0].clone(), now);
        req.scopes = scopes.to_vec();
        req.query = None;
        req.seeds = scopes
            .iter()
            .map(|scope| RecallSeed {
                scope: scope.clone(),
                id: "root".into(),
            })
            .collect();
        req.expand = true;
        req.allow_partial = true;
        req.limits.graph.max_edges_examined = 1;
        let first = recall(&db, &req).unwrap();
        req.scopes.reverse();
        assert_eq!(recall(&db, &req).unwrap(), first);
        assert_eq!(first.truncation, [TruncationReason::EdgeLimit]);
        assert!(
            first
                .hits
                .iter()
                .any(|hit| hit.ranking.candidate.scope == scopes[0]
                    && hit.ranking.candidate.id == "target")
        );
        req.limits.graph.max_edges_examined = 10;
        req.limits.graph.max_visited_nodes = 2;
        let first = recall(&db, &req).unwrap();
        req.scopes.reverse();
        assert_eq!(recall(&db, &req).unwrap(), first);
        assert_eq!(first.truncation, [TruncationReason::VisitedLimit]);
    }

    #[test]
    fn body_projection_utf8_boundaries_never_split_scalars() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("utf8.vlite"));
        let now = crate::current_time_ms() + 30_000;
        let scope = scope("tenant");
        let mut write = remember(&mut db, &scope, "initial", None, now, None, 0.5, vec![]);
        let body = "a\u{20ac}\u{1f642}z";
        for (id, content) in [("inline", body.to_owned()), ("chunked", body.repeat(600))] {
            write.idempotency_key = id.into();
            write.items[0].id = id.into();
            write.items[0].content = content;
            MemoryMut::bind(&mut db, "memory", None)
                .unwrap()
                .remember(&write)
                .unwrap();
            let mut req = request(scope.clone(), now);
            req.query = None;
            req.seeds = vec![RecallSeed {
                scope: scope.clone(),
                id: id.into(),
            }];
            for (cap, expected) in [
                (0, ""),
                (1, "a"),
                (2, "a"),
                (3, "a"),
                (4, "a\u{20ac}"),
                (5, "a\u{20ac}"),
                (7, "a\u{20ac}"),
                (8, "a\u{20ac}\u{1f642}"),
                (9, body),
            ] {
                req.snippet_bytes = cap;
                let out = recall(&db, &req).unwrap();
                assert_eq!(out.hits[0].content, expected, "{id}, cap {cap}");
                assert_eq!(out.stats.evidence_bytes, expected.len());
                assert_eq!(
                    out.hits[0].content_truncated,
                    id == "chunked" || cap < body.len()
                );
            }
        }
    }

    #[test]
    fn text_top_n_filters_foreign_and_unrequested_kinds_before_selection() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("text-filter.vlite"));
        let now = crate::current_time_ms() + 30_000;
        let visible = scope("visible");
        for (scope, id) in [
            (scope("foreign"), "foreign"),
            (visible.clone(), "wrong-kind"),
            (visible.clone(), "visible"),
        ] {
            remember(
                &mut db,
                &scope,
                id,
                Some(vec![1.0, 0.0]),
                now,
                None,
                0.5,
                vec![],
            );
            let text = if id == "visible" {
                format!("needle {}", "filler ".repeat(100))
            } else {
                "needle needle".into()
            };
            db.graph_mut("memory")
                .unwrap()
                .write_batch(&[crate::property_graph::GraphOp::Vector {
                    collection: "default".into(),
                    mutation: Mutation::PutWithMetadata {
                        id: evidence_record_id(id),
                        vector: vec![1.0, 0.0],
                        payload: id.as_bytes().to_vec(),
                        metadata: Metadata::from([("text".into(), MetadataValue::String(text))]),
                    },
                }])
                .unwrap();
        }
        let node = db
            .graph("memory")
            .unwrap()
            .get_node(&memory_key(&visible.key(), "wrong-kind"))
            .unwrap()
            .output
            .unwrap();
        let mut properties = node.properties;
        properties.insert("kind".into(), MetadataValue::String("unsupported".into()));
        db.graph_mut("memory")
            .unwrap()
            .write_batch(&[crate::property_graph::GraphOp::UpsertNode {
                key: node.key,
                labels: node.labels.into_iter().collect(),
                properties,
                expected: crate::Expected::Revision(node.revision),
            }])
            .unwrap();
        let index = &db.state(0).text_index;
        let terms = BTreeSet::from(["needle".to_owned()]);
        let top = index.search(&terms, 1, |_| true);
        assert_ne!(
            top[0].0,
            evidence_record_id("visible"),
            "fixture must displace authorized hit without prefilter"
        );
        let mut req = request(visible, now);
        req.query = Some(HybridQuery::new(None, "needle", 1));
        req.query.as_mut().unwrap().fusion.candidates = Some(1);
        req.limits.candidates = 1;
        // Mixed allowed kinds require the postfilter, not one-kind equality.
        assert_eq!(ids(&recall(&db, &req).unwrap()), ["visible"]);
        req.query.as_mut().unwrap().vector = Some(vec![1.0, 0.0]);
        req.query.as_mut().unwrap().text.clear();
        req.query.as_mut().unwrap().k = 3;
        req.query.as_mut().unwrap().fusion.candidates = Some(3);
        req.limits.candidates = 3;
        assert_eq!(ids(&recall(&db, &req).unwrap()), ["visible"]);
    }

    #[test]
    fn evidence_projection_and_snippet_ceiling_are_exact() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir.path().join("evidence-cap.vlite"));
        let now = crate::current_time_ms() + 30_000;
        let scope = scope("tenant");
        remember(
            &mut db,
            &scope,
            "bytes",
            Some(vec![1.0, 0.0]),
            now,
            None,
            0.5,
            vec![],
        );
        let mut req = request(scope, now);
        for cap in [0, 1, 4, 5, 6, 4096] {
            req.snippet_bytes = cap;
            let out = recall(&db, &req).unwrap();
            let hit = &out.hits[0];
            assert_eq!(hit.evidence[0].snippet, &b"bytes"[..cap.min(5)]);
            assert_eq!(hit.evidence[0].truncated, cap < 5);
            assert_eq!(
                out.stats.evidence_bytes,
                cap.min(5) + cap.min("content bytes".len())
            );
        }
        req.snippet_bytes = 4097;
        assert!(matches!(recall(&db, &req), Err(RecallError::InvalidLimits)));
    }
}
