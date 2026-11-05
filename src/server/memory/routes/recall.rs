//! Recall (contract sections 4 and 8, mapping sections 1, 2 and 6).

use std::collections::BTreeSet;

use crate::hybrid::{FusionOptions, HybridQuery};
use crate::memory::{
    FutureObservations, RankingPolicy, RecallError, RecallHit, RecallLimits, RecallOutput,
    RecallRequest, RecallSeed, Selection, recall_error,
};
use crate::property_graph::{GraphLimits, TruncationReason};

use super::*;

/// `text` contract bound and the core hybrid text cap (mapping section 6).
const MAX_TEXT_BYTES: usize = 16_384;
const CORE_TEXT_BYTES: usize = 1_024;
const MAX_SCOPES: usize = 16;
const MAX_SEEDS: usize = 64;
const MAX_KINDS: usize = 4;
/// Every view's `content` and snippet bound in UTF-8 bytes.
const SNIPPET_BYTES: usize = 4_096;

/// Recall field refusals in contract schema field order (mapping sections 1
/// and 6). **PROVISIONAL** order: the contract names none for reads.
fn recall_refusal(body: &Map<String, Value>, evidence: bool) -> Option<&'static str> {
    if body.contains_key("embedding_provider") {
        return Some("embedding_provider");
    }
    let kinds = body.get("kinds").and_then(Value::as_array);
    if kinds
        .into_iter()
        .flatten()
        .any(|kind| matches!(kind.as_str(), Some("fact" | "relation")))
    {
        return Some("fact_kinds");
    }
    if body.contains_key("filter") {
        return Some("filter");
    }
    if body.contains_key("as_of_ms") {
        return Some("temporal_read");
    }
    let projection = body.get("projection");
    if projection.and_then(|p| p.get("paths")) == Some(&Value::Bool(true)) {
        return Some("paths");
    }
    if projection
        .and_then(|p| p.get("metadata"))
        .and_then(Value::as_array)
        .is_some_and(|names| !names.is_empty())
    {
        return Some("metadata");
    }
    if (body.contains_key("text") || body.contains_key("vector")) && !evidence {
        return Some("evidence_recall");
    }
    body.get("text")
        .and_then(Value::as_str)
        .filter(|text| (CORE_TEXT_BYTES + 1..=MAX_TEXT_BYTES).contains(&text.len()))
        .map(|_| "query_text_length")
}

fn unsupported_ranking() -> Fail {
    Fail::wire(WireCode {
        status: 422,
        code: "memory_unsupported_ranking",
        feature: None,
    })
}

fn bounded_plan() -> Fail {
    Fail::wire(WireCode {
        status: 422,
        code: "memory_bounded_plan_unsupported",
        feature: None,
    })
}

/// `MemoryRanking` to the core policy and selection.
fn ranking(value: Option<Value>, limit: usize) -> Result<(RankingPolicy, Selection), Fail> {
    let mut policy = RankingPolicy {
        limit,
        ..RankingPolicy::default()
    };
    let Some(value) = value else {
        return Ok((policy, Selection::Ranked));
    };
    let mut fields = Fields::new(value, "ranking")?;
    let selection = fields.take("selection")?;
    if let Some(weights) = fields.take("weights")? {
        let weights = wire::array(weights, "ranking", 3, 3)?;
        for (slot, weight) in policy.weights.iter_mut().zip(weights) {
            *slot = wire::number(weight, "ranking", 0.0, f64::MAX)?;
        }
    }
    policy.half_life_ms = fields
        .take("half_life_ms")?
        .map(|value| wire::number(value, "ranking", 0.0, f64::MAX))
        .transpose()?;
    if policy.half_life_ms == Some(0.0) {
        return Err(Fail::invalid("ranking"));
    }
    policy.future_observations = match fields.take("future_observations")? {
        None => FutureObservations::Reject,
        Some(value) => match value.as_str() {
            Some("reject") => FutureObservations::Reject,
            Some("clamp") => FutureObservations::Clamp,
            _ => return Err(Fail::invalid("ranking")),
        },
    };
    let lambda = fields
        .take("lambda")?
        .map(|value| wire::number(value, "ranking", 0.0, 1.0))
        .transpose()?
        .unwrap_or(0.5);
    fields.finish()?;
    let selection = match selection.as_ref().map(Value::as_str) {
        None | Some(Some("weighted")) => Selection::Ranked,
        Some(Some("exact")) => Selection::Exact,
        Some(Some("mmr")) => Selection::Mmr { lambda },
        _ => return Err(Fail::invalid("ranking")),
    };
    Ok((policy, selection))
}

/// `MemoryLimits` within the contract bounds and defaults.
fn limits(value: Option<Value>) -> Result<(RecallLimits, u64), Fail> {
    let mut fields = Fields::new(value.unwrap_or_else(|| json!({})), "limits")?;
    let mut read = |name: &'static str, min: u64, max: u64, default: u64| {
        fields
            .take(name)
            .and_then(|value| wire::integer(value, "limits", min, max, default))
    };
    let candidates = read("candidates", 1, 1_000, 200)?;
    let vector_components = read("vector_components", 1, 4_000_000, 1_000_000)?;
    let evidence_bytes = read("evidence_bytes", 1, 4_194_304, 1_048_576)?;
    let mmr_components = read("mmr_components", 1, 100_000_000, 10_000_000)?;
    let deadline_ms = read("deadline_ms", 1, 5_000, 250)?;
    let graph = GraphLimits {
        max_hops: read("max_hops", 0, 4, 2)?,
        max_visited_nodes: read("max_visited_nodes", 1, 10_000, 1_000)?,
        max_edges_examined: read("max_edges_examined", 1, 100_000, 10_000)?,
        max_edges_per_node: read("max_edges_per_node", 1, 10_000, 1_000)?,
        deadline_ms,
        max_output_nodes: read("max_output_nodes", 0, 1_000, 100)?,
        max_output_edges: read("max_output_edges", 0, 2_000, 200)?,
        max_output_paths: read("max_output_paths", 0, 100, 20)?,
    };
    fields.finish()?;
    Ok((
        RecallLimits {
            candidates: candidates as usize,
            source_records: 100_000,
            vector_components: vector_components as usize,
            evidence_bytes: evidence_bytes as usize,
            mmr_components: mmr_components as usize,
            graph,
        },
        deadline_ms,
    ))
}

/// `MemoryProjection`: `(snippets, provenance)`; refused fields ran first.
fn projection(value: Option<Value>) -> Result<(bool, bool), Fail> {
    let Some(value) = value else {
        return Ok((true, true));
    };
    let mut fields = Fields::new(value, "projection")?;
    let flag = |value: Option<Value>, default: bool| match value {
        None => Ok(default),
        Some(Value::Bool(flag)) => Ok(flag),
        Some(_) => Err(Fail::invalid("projection")),
    };
    let snippets = flag(fields.take("snippets")?, true)?;
    let provenance = flag(fields.take("provenance")?, true)?;
    flag(fields.take("paths")?, false)?;
    if let Some(metadata) = fields.take("metadata")? {
        wire::array(metadata, "projection", 0, 0)?;
    }
    fields.finish()?;
    Ok((snippets, provenance))
}

/// Absent kinds means every served kind; empty or duplicate kinds reject.
fn kinds(value: Option<Value>) -> Result<Vec<MemoryKind>, Fail> {
    let Some(value) = value else {
        return Ok(vec![MemoryKind::Entity, MemoryKind::Episodic]);
    };
    let mut kinds = Vec::new();
    for value in wire::array(value, "kinds", 1, MAX_KINDS)? {
        let kind = kind(value).map_err(|_| Fail::invalid("kinds"))?;
        if kinds.contains(&kind) {
            return Err(Fail::invalid("kinds"));
        }
        kinds.push(kind);
    }
    Ok(kinds)
}

/// The exact read allowlist: `scopes` replaces `scope` and must contain it.
fn scopes(fields: &mut Fields, tenant: &str) -> Result<(MemoryScope, Vec<MemoryScope>), Fail> {
    let scope = wire::scope(fields.require("scope")?, tenant, "scope")?;
    let Some(value) = fields.take("scopes")? else {
        return Ok((scope.clone(), vec![scope]));
    };
    let mut scopes = Vec::new();
    for value in wire::array(value, "scopes", 1, MAX_SCOPES)? {
        let next = wire::scope(value, tenant, "scopes")?;
        if scopes.contains(&next) {
            return Err(Fail::invalid("scopes"));
        }
        scopes.push(next);
    }
    if !scopes.contains(&scope) {
        return Err(Fail::invalid("scopes"));
    }
    Ok((scope, scopes))
}

/// A validated recall, ready for the core once the database is held.
struct Parsed {
    scopes: Vec<MemoryScope>,
    kinds: Vec<MemoryKind>,
    query: Option<HybridQuery>,
    seeds: Vec<RecallSeed>,
    policy: RankingPolicy,
    selection: Selection,
    limits: RecallLimits,
    deadline_ms: u64,
    allow_partial: bool,
    snippets: bool,
    provenance: bool,
}

/// Step 5: request validation against the contract bounds.
fn parse(mut fields: Fields, scope: MemoryScope, scopes: Vec<MemoryScope>) -> Result<Parsed, Fail> {
    let vector = fields
        .take("vector")?
        .map(|value| {
            wire::array(value, "vector", 1, usize::MAX)?
                .into_iter()
                .map(|n| {
                    n.as_f64()
                        .map(|n| n as f32)
                        .filter(|n| n.is_finite())
                        .ok_or(Fail::invalid("vector"))
                })
                .collect::<Result<Vec<f32>, _>>()
        })
        .transpose()?;
    let text = match fields.take("text")? {
        None => None,
        Some(Value::String(text)) if !text.is_empty() && text.len() <= MAX_TEXT_BYTES => Some(text),
        Some(_) => return Err(Fail::invalid("text")),
    };
    let seeds = match fields.take("entity_seeds")? {
        None => Vec::new(),
        Some(value) => {
            let mut ids = BTreeSet::new();
            wire::array(value, "entity_seeds", 0, MAX_SEEDS)?
                .into_iter()
                .map(|value| {
                    let id = wire::key(value, "entity_seeds")?;
                    if !ids.insert(id.clone()) {
                        return Err(Fail::invalid("entity_seeds"));
                    }
                    Ok(RecallSeed {
                        scope: scope.clone(),
                        id,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?
        }
    };
    let kinds = kinds(fields.take("kinds")?)?;
    let limit = wire::integer(fields.take("limit")?, "limit", 1, 100, 20)? as usize;
    let (policy, selection) = ranking(fields.take("ranking")?, limit)?;
    let (limits, deadline_ms) = limits(fields.take("limits")?)?;
    let (snippets, provenance) = projection(fields.take("projection")?)?;
    let approximate = match fields.take("mode")? {
        None => false,
        Some(value) => match value.as_str() {
            Some("exact") => false,
            Some("approximate") => true,
            _ => return Err(Fail::invalid("mode")),
        },
    };
    let allow_partial = wire::boolean(fields.take("allow_partial")?, "allow_partial")?;
    fields.finish()?;
    if limit > limits.candidates {
        return Err(Fail::invalid("limit"));
    }
    if vector.is_none() && text.is_none() && seeds.is_empty() {
        return Err(Fail::invalid("body"));
    }
    // Exact and MMR selections are vector-only and refuse every blend
    // (contract section 4).
    if selection != Selection::Ranked
        && (vector.is_none()
            || text.is_some()
            || !seeds.is_empty()
            || policy.weights != [1.0, 0.0, 0.0]
            || policy.half_life_ms.is_some())
    {
        return Err(unsupported_ranking());
    }
    // No ready approximate index belongs to a binding, and none is built.
    if approximate {
        return Err(bounded_plan());
    }
    let query = (vector.is_some() || text.is_some()).then(|| HybridQuery {
        fusion: FusionOptions {
            candidates: Some(limits.candidates),
            ..FusionOptions::default()
        },
        ..HybridQuery::new(vector, text.unwrap_or_default(), limits.candidates)
    });
    Ok(Parsed {
        scopes,
        kinds,
        query,
        seeds,
        policy,
        selection,
        limits,
        deadline_ms,
        allow_partial,
        snippets,
        provenance,
    })
}

/// Recall core errors. `allow_partial: true` never turns an exhausted
/// budget into a partial answer: the bounded plan is refused (section 6).
fn recall_fail(error: RecallError, allow_partial: bool) -> Fail {
    if let RecallError::Budget(field) = error {
        return if allow_partial {
            bounded_plan()
        } else {
            Fail::invalid(field)
        };
    }
    match recall_error(&error) {
        WireMapping::Code(code) => Fail::wire(code),
        WireMapping::Delegate => match error {
            RecallError::Storage(error) | RecallError::Memory(MemoryError::Storage(error)) => {
                ApiError::from(error).into()
            }
            _ => internal(),
        },
        WireMapping::Cancelled => internal(),
    }
}

fn truncation(reason: &TruncationReason) -> Result<&'static str, Fail> {
    Ok(match reason {
        TruncationReason::VisitedLimit => "visited_nodes",
        TruncationReason::EdgeLimit => "edges",
        TruncationReason::DegreeLimit { .. } => "degree",
        TruncationReason::OutputLimit => "output",
        TruncationReason::Deadline => "deadline",
        TruncationReason::Cancelled => return Err(internal()),
    })
}

fn selection_name(selection: Selection) -> &'static str {
    match selection {
        Selection::Ranked => "weighted",
        Selection::Exact => "exact",
        Selection::Mmr { .. } => "mmr",
    }
}

/// One `MemoryView`. The core runs with `allow_partial: false`, so a stale
/// label of `None` is a non-summary hit (`stale: false`), never unknown.
fn view(hit: RecallHit, binding: &str, parsed: &Parsed) -> Result<Value, Fail> {
    if hit.content_truncated {
        return Err(internal());
    }
    let candidate = &hit.ranking.candidate;
    let mut scores = json!({
        "R": hit.ranking.relevance,
        "D": hit.ranking.decay,
        "I": hit.ranking.importance,
        "S": hit.ranking.score,
    });
    if let Some(rank) = candidate.rank {
        scores["fused_rank"] = json!(rank);
    }
    if let Some(rank) = hit.vector_rank {
        scores["dense_rank"] = json!(rank);
    }
    if let Some(rank) = hit.text_rank {
        scores["text_rank"] = json!(rank);
    }
    if let Some(distance) = hit.distance {
        scores["distance"] = json!(distance);
    }
    if let Some(bm25) = hit.bm25 {
        scores["bm25"] = json!(bm25);
    }
    let evidence = hit
        .evidence
        .iter()
        .map(|evidence| {
            let mut view = json!({"collection": binding, "record": evidence.record_id.to_string()});
            if parsed.snippets
                && let Ok(snippet) = std::str::from_utf8(&evidence.snippet)
            {
                view["snippet"] = json!(snippet);
            }
            view
        })
        .collect::<Vec<_>>();
    let mut view = json!({
        "id": candidate.id,
        "revision": hit.memory_revision.to_string(),
        "scope": wire::effective(&candidate.scope),
        "kind": hit.kind,
        "content": hit.content,
        "observed_at_ms": candidate.observed_at_ms.to_string(),
        "created_at_ms": hit.created_at_ms.to_string(),
        "updated_at_ms": hit.updated_at_ms.to_string(),
        "importance": candidate.importance,
        "status": "accepted",
        "stale": hit.stale.unwrap_or(false),
        "segment": null,
        "evidence": evidence,
        "scores": scores,
        "metadata": {},
    });
    if parsed.provenance {
        let mut provenance = hit.provenance.clone();
        provenance.sort_by(|a, b| (&a.kind, &a.source, &a.uri).cmp(&(&b.kind, &b.source, &b.uri)));
        let provenance = provenance
            .into_iter()
            .map(|source| {
                let kind = source.kind.ok_or_else(internal)?;
                let mut view = json!({
                    "source_id": source.source,
                    "source_kind": kind,
                    "content_digest": hit.content_digest,
                });
                if let Some(uri) = source.uri {
                    view["uri"] = json!(uri);
                }
                if let Some(transformation) = source.transformation {
                    view["transformation_id"] = json!(transformation);
                }
                Ok(view)
            })
            .collect::<Result<Vec<_>, Fail>>()?;
        view["provenance"] = json!(provenance);
    }
    // PROVISIONAL: a hit carries the reference count, not the IDs, so the
    // optional field is omitted rather than reported incomplete.
    if hit.reference_count == 0 {
        view["references"] = json!([]);
    }
    if let Some(expiry) = hit.expiry_ms {
        view["expires_at_ms"] = json!(expiry.to_string());
    }
    Ok(view)
}

fn response(output: RecallOutput, binding: &str, parsed: &Parsed) -> Result<Value, Fail> {
    let mut truncation = output
        .truncation
        .iter()
        .map(truncation)
        .collect::<Result<Vec<_>, _>>()?;
    if !output.deletion_barriers.is_empty() {
        truncation.push("deletion_barrier");
    }
    let stats = &output.stats;
    // The core charges returned content to the same byte budget; the wire
    // reports evidence snippet bytes only (contract section 8 examples).
    let evidence_bytes: usize = output
        .hits
        .iter()
        .flat_map(|hit| &hit.evidence)
        .map(|evidence| evidence.snippet.len())
        .sum();
    let meta = json!({
        "scopes": output.scopes.iter().map(wire::effective).collect::<Vec<_>>(),
        "as_of_ms": output.as_of_ms.to_string(),
        "ttl_clock_ms": output.clock_ms.to_string(),
        "sequence": output.sequence.to_string(),
        "complete": truncation.is_empty(),
        "truncation": truncation,
        "limitations": ["retained_state_only"],
        "mode": if parsed.seeds.is_empty() { "exact_enumeration" } else { "bounded_expansion" },
        "candidates_examined": stats.candidates_examined.to_string(),
        "edges_charged": stats.edges_charged.to_string(),
        "vector_components": stats.vector_components.to_string(),
        "mmr_components": stats.mmr_components.to_string(),
        "evidence_bytes": evidence_bytes.to_string(),
    });
    let memories = output
        .hits
        .into_iter()
        .map(|hit| view(hit, binding, parsed))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({
        "memories": memories,
        "meta": meta,
        "ranking_version": "memory-ranking-v1",
        "selection": selection_name(output.selection),
    }))
}

pub(super) async fn recall(
    State(service): State<Service>,
    caller: Caller,
    Extension(request_id): Extension<RequestId>,
    clock: Option<Extension<Clock>>,
    name: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    bytes: Result<Bytes, BytesRejection>,
) -> Response {
    let role = caller.role.clone();
    let mut call = Call::new(service.clone(), caller, request_id, clock, "recall");
    let evidence_ops: &[Op] = &[Op::Read, Op::Search];
    let (bound, name) = match resolve(&service, &role, name, WireOperation::Recall, evidence_ops) {
        Ok(found) => found,
        Err(fail) => return call.finish(Err(fail), None),
    };
    call.binding = Some(name.clone());
    let parsed = body(&headers, bytes).and_then(|mut fields| {
        let (scope, scopes) = scopes(&mut fields, &bound.tenant)?;
        for scope in &scopes {
            bound.authorize(scope)?;
        }
        refuse(&bound, fields.raw(), |body| {
            recall_refusal(body, bound.evidence.is_some())
        })?;
        parse(fields, scope, scopes)
    });
    let parsed = match parsed {
        Ok(parsed) => parsed,
        Err(fail) => return call.finish(Err(fail), None),
    };
    let call = Arc::new(call);
    let inner = call.clone();
    let result = service
        .run_read(move |db| Ok(recall_in(db, &inner, &bound, &name, &parsed)))
        .await
        .map_err(Fail::from)
        .and_then(|result| result);
    call.finish(result, None)
}

fn recall_in(
    db: &Database,
    call: &Call,
    bound: &Bound,
    binding: &str,
    parsed: &Parsed,
) -> Result<Value, Fail> {
    bound.covers(db)?;
    let now = call.now();
    let remaining = parsed.deadline_ms.saturating_sub(call.elapsed_ms());
    if remaining == 0 {
        return Err(Fail::wire(WireCode {
            status: 504,
            code: "deadline_exceeded",
            feature: None,
        }));
    }
    let mut limits = parsed.limits.clone();
    limits.graph.deadline_ms = remaining;
    let request = RecallRequest {
        scopes: parsed.scopes.clone(),
        kinds: parsed.kinds.clone(),
        filter: Vec::new(),
        query: parsed.query.clone(),
        seeds: parsed.seeds.clone(),
        as_of_ms: None,
        clock_ms: now,
        policy: parsed.policy.clone(),
        selection: parsed.selection,
        limits,
        expand: !parsed.seeds.is_empty(),
        allow_partial: false,
        snippet_bytes: SNIPPET_BYTES,
        cancel: None,
    };
    let output = MemoryRef::bind(db, &bound.graph, bound.evidence.as_deref())
        .map_err(core_error)?
        .recall(&request)
        .map_err(|error| recall_fail(error, parsed.allow_partial))?;
    response(output, binding, parsed)
}

#[cfg(test)]
#[path = "../pins_recall2.rs"]
mod pins_recall2;
