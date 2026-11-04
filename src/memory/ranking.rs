//! Ranking over an already eligible, bounded candidate union; no discovery or visibility checks.

use super::MemoryScope;
use std::cmp::Ordering;
use std::collections::BTreeSet;

pub(crate) const RANKING_VERSION: &str = "private.agent-memory-ranking.v1";
const MAX_CANDIDATES: usize = 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FutureObservations {
    Reject,
    Clamp,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RankingPolicy {
    pub(crate) weights: [f64; 3],
    pub(crate) half_life_ms: Option<f64>,
    pub(crate) limit: usize,
    pub(crate) future_observations: FutureObservations,
}

impl Default for RankingPolicy {
    fn default() -> Self {
        Self {
            weights: [1.0, 0.0, 0.0],
            half_life_ms: None,
            limit: 20,
            future_observations: FutureObservations::Reject,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RankedCandidate {
    pub(crate) scope: MemoryScope,
    pub(crate) id: String,
    pub(crate) observed_at_ms: i64,
    pub(crate) rank: Option<usize>,
    pub(crate) importance: f64,
    pub(crate) segment: Option<(i64, i64)>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ScoredCandidate {
    pub(crate) candidate: RankedCandidate,
    pub(crate) relevance: f64,
    pub(crate) decay: Option<f64>,
    pub(crate) importance: f64,
    pub(crate) score: f64,
    pub(crate) effective_t: i64,
    pub(crate) ranking_version: &'static str,
    pub(crate) expanded_only: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RankingError {
    Weights,
    HalfLifeRequired,
    HalfLife,
    Limit,
    CandidateCount,
    Scope,
    Importance,
    Rank,
    DuplicateRank,
    DuplicateIdentity,
    DenseRanks,
    SegmentBounds,
    AgeOverflow,
    FutureObservation,
    NonFiniteScore,
}

pub(super) fn validate_policy(policy: &RankingPolicy) -> Result<(), RankingError> {
    if policy.weights.iter().any(|w| !w.is_finite() || *w < 0.0)
        || (policy.weights.iter().sum::<f64>() - 1.0).abs() > 1e-12
    {
        return Err(RankingError::Weights);
    }
    if policy.half_life_ms.is_none() && policy.weights[1] != 0.0 {
        return Err(RankingError::HalfLifeRequired);
    }
    if policy
        .half_life_ms
        .is_some_and(|h| !h.is_finite() || h <= 0.0)
    {
        return Err(RankingError::HalfLife);
    }
    if !(1..=100).contains(&policy.limit) {
        return Err(RankingError::Limit);
    }
    Ok(())
}

fn scope_key(scope: &MemoryScope) -> (&str, &str, Option<&str>, Option<&str>) {
    (
        &scope.tenant,
        &scope.agent,
        scope.user.as_deref(),
        scope.session.as_deref(),
    )
}

fn canonical_cmp(left: &RankedCandidate, right: &RankedCandidate) -> Ordering {
    scope_key(&left.scope)
        .cmp(&scope_key(&right.scope))
        .then_with(|| left.id.cmp(&right.id))
        .then_with(|| left.segment.cmp(&right.segment))
}

pub(super) fn rank(
    candidates: Vec<RankedCandidate>,
    effective_t: i64,
    policy: &RankingPolicy,
) -> Result<Vec<ScoredCandidate>, RankingError> {
    validate_policy(policy)?;
    if candidates.len() > MAX_CANDIDATES {
        return Err(RankingError::CandidateCount);
    }
    let mut identities = BTreeSet::new();
    let mut ranks = BTreeSet::new();
    // Validate the entire union, including candidates that will not survive the output limit.
    for candidate in &candidates {
        if candidate.scope.session.is_some() && candidate.scope.user.is_none() {
            return Err(RankingError::Scope);
        }
        let age = effective_t
            .checked_sub(candidate.observed_at_ms)
            .ok_or(RankingError::AgeOverflow)?;
        if age < 0 && policy.future_observations == FutureObservations::Reject {
            return Err(RankingError::FutureObservation);
        }
        if !candidate.importance.is_finite() || !(0.0..=1.0).contains(&candidate.importance) {
            return Err(RankingError::Importance);
        }
        if let Some(position) = candidate.rank {
            if !(1..=MAX_CANDIDATES).contains(&position) {
                return Err(RankingError::Rank);
            }
            if !ranks.insert(position) {
                return Err(RankingError::DuplicateRank);
            }
        }
        if candidate.segment.is_some_and(|(start, end)| start >= end) {
            return Err(RankingError::SegmentBounds);
        }
        if !identities.insert((
            scope_key(&candidate.scope),
            &candidate.id,
            candidate.segment,
        )) {
            return Err(RankingError::DuplicateIdentity);
        }
    }
    if ranks.iter().copied().ne(1..=ranks.len()) {
        return Err(RankingError::DenseRanks);
    }

    let mut scored = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        // The signed difference has already been checked, before the future clamp.
        let age = effective_t
            .checked_sub(candidate.observed_at_ms)
            .ok_or(RankingError::AgeOverflow)?
            .max(0);
        let relevance = candidate.rank.map_or(0.0, |position| 1.0 / position as f64);
        let decay = policy.half_life_ms.map(|h| 1.0 / (1.0 + age as f64 / h));
        let importance = candidate.importance;
        let score = policy.weights[0] * relevance
            + policy.weights[1] * decay.unwrap_or(0.0)
            + policy.weights[2] * importance;
        if !relevance.is_finite() || decay.is_some_and(|d| !d.is_finite()) || !score.is_finite() {
            return Err(RankingError::NonFiniteScore);
        }
        scored.push(ScoredCandidate {
            expanded_only: candidate.rank.is_none(),
            candidate,
            relevance,
            decay,
            importance,
            score,
            effective_t,
            ranking_version: RANKING_VERSION,
        });
    }
    scored.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .expect("validated finite ranking scores")
            .then_with(|| canonical_cmp(&left.candidate, &right.candidate))
    });
    scored.truncate(policy.limit);
    Ok(scored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn fixture() -> Value {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/agent_memory/ranking_golden.json"
        ))
        .unwrap()
    }

    fn number(value: &Value) -> f64 {
        if let Some(marker) = value.get("$number").and_then(Value::as_str) {
            return match marker {
                "nan" => f64::NAN,
                "+inf" => f64::INFINITY,
                "-inf" => f64::NEG_INFINITY,
                _ => panic!("unknown nonfinite marker"),
            };
        }
        value.as_f64().expect("representable numeric fixture")
    }

    fn policy(value: &Value) -> RankingPolicy {
        let mut policy = RankingPolicy::default();
        if value.is_null() {
            return policy;
        }
        if let Some(weights) = value.get("weights") {
            policy.weights = std::array::from_fn(|i| number(&weights[i]));
        }
        if let Some(half_life) = value.get("half_life_ms") {
            policy.half_life_ms = Some(number(half_life));
        }
        if let Some(limit) = value.get("limit") {
            policy.limit = usize::try_from(limit.as_u64().unwrap()).unwrap();
        }
        if let Some(future) = value.get("future_observations") {
            policy.future_observations = match future.as_str().unwrap() {
                "reject" => FutureObservations::Reject,
                "clamp" => FutureObservations::Clamp,
                _ => panic!("unrepresentable future policy"),
            };
        }
        policy
    }

    fn candidate(value: &Value) -> RankedCandidate {
        let scope = &value["scope"];
        RankedCandidate {
            scope: MemoryScope {
                tenant: scope[0].as_str().unwrap().into(),
                agent: scope[1].as_str().unwrap().into(),
                user: scope[2].as_str().map(str::to_owned),
                session: scope[3].as_str().map(str::to_owned),
            },
            id: value["id"].as_str().unwrap().into(),
            observed_at_ms: value.get("O").map_or(0, |v| v.as_i64().unwrap()),
            rank: value
                .get("rank")
                .filter(|v| !v.is_null())
                .map(|v| usize::try_from(v.as_u64().unwrap()).unwrap()),
            importance: value.get("importance").map_or(0.5, number),
            segment: value
                .get("segment")
                .filter(|v| !v.is_null())
                .map(|v| (v[0].as_i64().unwrap(), v[1].as_i64().unwrap())),
        }
    }

    fn records(catalog: &Value, references: &Value) -> Vec<Value> {
        let mut output = Vec::new();
        for name in references.as_array().unwrap() {
            let record = &catalog[name.as_str().unwrap()];
            if let Some(ids) = record.get("ids") {
                for id in ids.as_array().unwrap() {
                    let mut value = record["template"].clone();
                    value["id"] = id.clone();
                    output.push(value);
                }
            } else {
                output.push(record.clone());
            }
        }
        output
    }

    // Only expected fixture fractions use this conversion; the kernel always computes f64.
    fn fraction(value: &Value) -> f64 {
        let value = value.as_str().unwrap();
        match value.split_once('/') {
            Some((numerator, denominator)) => {
                numerator.parse::<f64>().unwrap() / denominator.parse::<f64>().unwrap()
            }
            None => value.parse().unwrap(),
        }
    }

    fn close(actual: f64, expected: f64, context: &str) {
        assert!(
            actual.is_finite()
                && (actual - expected).abs() <= 2e-15 * expected.abs().max(f64::MIN_POSITIVE),
            "{context}: {actual:?} != {expected:?}"
        );
    }

    fn assert_expected(actual: &[ScoredCandidate], expected: &[Value], t: i64, name: &str) {
        assert_eq!(actual.len(), expected.len(), "{name}");
        for (actual, expected) in actual.iter().zip(expected) {
            let expected_candidate = candidate(expected);
            assert_eq!(actual.candidate.scope, expected_candidate.scope, "{name}");
            assert_eq!(actual.candidate.id, expected_candidate.id, "{name}");
            assert_eq!(
                actual.candidate.segment, expected_candidate.segment,
                "{name}"
            );
            assert_eq!(actual.candidate.rank, expected_candidate.rank, "{name}");
            close(actual.relevance, fraction(&expected["R"]), name);
            match (actual.decay, expected["D"].is_null()) {
                (Some(actual), false) => close(actual, fraction(&expected["D"]), name),
                (None, true) => {}
                _ => panic!("{name}: decay presence mismatch"),
            }
            close(actual.importance, fraction(&expected["I"]), name);
            close(actual.candidate.importance, fraction(&expected["I"]), name);
            close(actual.score, fraction(&expected["S"]), name);
            assert_eq!(actual.effective_t, t, "{name}");
            assert_eq!(actual.ranking_version, RANKING_VERSION, "{name}");
            assert_eq!(
                actual.expanded_only,
                expected["expanded_only"].as_bool().unwrap(),
                "{name}"
            );
        }
    }

    #[test]
    fn ranking_golden_valid_cases_and_permutations() {
        let fixture = fixture();
        let cases = fixture["cases"].as_array().unwrap();
        assert_eq!(cases.len(), 42);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let policy_record = &fixture["policies"][case["policy"].as_str().unwrap()];
            let policy = policy(&policy_record["input"]);
            let expected_policy = &policy_record["expected"];
            for (i, weight) in policy.weights.iter().enumerate() {
                close(*weight, fraction(&expected_policy["weights"][i]), name);
            }
            assert_eq!(
                policy.limit,
                expected_policy["limit"].as_u64().unwrap() as usize,
                "{name}"
            );
            assert_eq!(
                policy.half_life_ms,
                expected_policy
                    .get("half_life_ms")
                    .filter(|v| !v.is_null())
                    .map(fraction),
                "{name}"
            );
            let candidates: Vec<_> = records(&fixture["candidates"], &case["candidates"])
                .iter()
                .map(candidate)
                .collect();
            let expected = records(&fixture["expected_memories"], &case["expected_memories"]);
            let t = case["T"].as_i64().unwrap();
            assert_eq!(t, case["expected_T"].as_i64().unwrap(), "{name}");
            // Reverse and every rotation provide deterministic input-order perturbations.
            for reverse in [false, true] {
                let mut shuffled = candidates.clone();
                if reverse {
                    shuffled.reverse();
                }
                for _ in 0..candidates.len().max(1) {
                    let actual = rank(shuffled.clone(), t, &policy).unwrap();
                    assert_expected(&actual, &expected, t, name);
                    if !shuffled.is_empty() {
                        shuffled.rotate_left(1);
                    }
                }
            }
        }
    }

    fn typed_only_skip(name: &str) -> Option<&'static str> {
        match name {
            "importance_null_not_default"
            | "importance_boolean"
            | "half_life_null"
            | "weight_null"
            | "weight_boolean"
            | "limit_boolean"
            | "limit_null"
            | "T_boolean"
            | "rank_boolean"
            | "rank_fractional"
            | "O_decimal_not_integer"
            | "id_not_string" => Some("typed scalar excludes null, boolean or wrong type"),
            "rank_negative" | "limit_negative" => Some("usize excludes negative values"),
            "T_above_i64"
            | "T_below_i64"
            | "O_above_i64"
            | "O_below_i64"
            | "segment_bound_overflow"
            | "segment_infinite_bound_not_supported" => {
                Some("i64 excludes out-of-range or absent bounds")
            }
            "weight_wrong_length"
            | "segment_wrong_shape"
            | "scope_wrong_arity"
            | "scope_null_tenant"
            | "scope_numeric_user" => Some("typed tuple/array/scope shape"),
            "missing_T"
            | "retrieval_field_out_of_scope"
            | "MMR_policy_out_of_scope"
            | "candidate_unknown_similarity"
            | "candidate_set_not_array"
            | "policy_null_not_default" => Some("typed requests have no JSON object shape"),
            "future_policy_unknown" => Some("enum excludes unknown future policy"),
            _ => None,
        }
    }

    fn expected_error(value: &str) -> RankingError {
        match value {
            "importance" => RankingError::Importance,
            "half_life" => RankingError::HalfLife,
            "half_life_required" => RankingError::HalfLifeRequired,
            "weights" => RankingError::Weights,
            "rank" => RankingError::Rank,
            "duplicate_id" => RankingError::DuplicateIdentity,
            "dense_ranks" => RankingError::DenseRanks,
            // The Rust kernel also caps expanded-only candidates, unlike the oracle.
            "rank_count" => RankingError::CandidateCount,
            "duplicate_rank" => RankingError::DuplicateRank,
            "age_overflow" => RankingError::AgeOverflow,
            "limit" => RankingError::Limit,
            "segment_bounds" => RankingError::SegmentBounds,
            "session_requires_user" => RankingError::Scope,
            "future_observation" => RankingError::FutureObservation,
            other => panic!("missing representable fixture error: {other}"),
        }
    }

    #[test]
    fn ranking_golden_invalid_cases_with_enumerated_typed_only_skips() {
        let fixture = fixture();
        let mut replayed = 0;
        let mut skipped = 0;
        for case in fixture["invalid"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            if let Some(reason) = typed_only_skip(name) {
                assert!(!reason.is_empty());
                skipped += 1;
                continue;
            }
            let input = &case["input"];
            let candidates = if let Some(count) = input["candidates"].get("$ranked_count") {
                (1..=count.as_u64().unwrap())
                    .map(|position| {
                        candidate(&json!({
                            "id": format!("rank-{position}"), "scope": ["t","a",null,null],
                            "O": 0, "rank": position
                        }))
                    })
                    .collect()
            } else {
                input["candidates"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(candidate)
                    .collect()
            };
            let actual = rank(
                candidates,
                input["T"].as_i64().unwrap(),
                &policy(input.get("policy").unwrap_or(&Value::Null)),
            );
            assert_eq!(
                actual.unwrap_err(),
                expected_error(case["error"].as_str().unwrap()),
                "{name}"
            );
            replayed += 1;
        }
        assert_eq!((replayed, skipped), (40, 32));
    }

    #[test]
    fn ranking_golden_comparators() {
        let fixture = fixture();
        let cases = fixture["comparators"].as_array().unwrap();
        assert_eq!(cases.len(), 4);
        for case in cases {
            let mut candidates: Vec<_> = case["input"]
                .as_array()
                .unwrap()
                .iter()
                .map(candidate)
                .collect();
            candidates.sort_by(canonical_cmp);
            let expected: Vec<_> = case["expected"]
                .as_array()
                .unwrap()
                .iter()
                .map(candidate)
                .collect();
            assert_eq!(candidates, expected, "{}", case["name"]);
        }
    }

    fn basic(id: &str, position: Option<usize>) -> RankedCandidate {
        candidate(&json!({"id":id, "scope":["t","a",null,null], "O":0, "rank":position}))
    }

    #[test]
    fn ranking_candidate_and_rank_caps_validate_before_limit() {
        let policy = RankingPolicy {
            limit: 1,
            ..RankingPolicy::default()
        };
        let thousand: Vec<_> = (1..=1_000)
            .map(|i| basic(&format!("m{i}"), Some(i)))
            .collect();
        let result = rank(thousand.clone(), 0, &policy).unwrap();
        assert_eq!(result[0].candidate.rank, Some(1));
        let mut too_many = thousand;
        too_many.push(basic("expanded", None));
        assert_eq!(
            rank(too_many, 0, &policy),
            Err(RankingError::CandidateCount)
        );
        let expanded: Vec<_> = (0..1_001).map(|i| basic(&format!("e{i}"), None)).collect();
        assert_eq!(
            rank(expanded, 0, &policy),
            Err(RankingError::CandidateCount)
        );
        assert_eq!(
            rank(vec![basic("a", Some(1_001))], 0, &policy),
            Err(RankingError::Rank)
        );
        let mut invalid = basic("outside-output", None);
        invalid.importance = f64::NAN;
        assert_eq!(
            rank(vec![basic("a", Some(1)), invalid], 0, &policy),
            Err(RankingError::Importance)
        );
    }

    #[test]
    fn ranking_weights_are_not_renormalized_or_epsilon_tied() {
        let policy = RankingPolicy {
            weights: [1.0000000000005, 0.0, 0.0],
            ..RankingPolicy::default()
        };
        let output = rank(vec![basic("a", Some(1))], 0, &policy).unwrap();
        assert_eq!(output[0].score, policy.weights[0]);
        assert_ne!(output[0].score, 1.0);
        let policy = RankingPolicy {
            weights: [0.0, 0.0, 1.0],
            ..RankingPolicy::default()
        };
        let mut higher = basic("z", None);
        higher.importance = 0.5 + f64::EPSILON;
        let output = rank(vec![basic("a", None), higher], 0, &policy).unwrap();
        assert_eq!(output[0].candidate.id, "z");
    }

    #[test]
    fn ranking_checked_age_precedes_future_clamp_and_unused_decay() {
        let policy = RankingPolicy {
            future_observations: FutureObservations::Clamp,
            ..RankingPolicy::default()
        };
        let mut future = basic("future", None);
        future.observed_at_ms = 1;
        assert_eq!(
            rank(vec![future.clone()], i64::MIN, &policy),
            Err(RankingError::AgeOverflow)
        );
        let output = rank(vec![future.clone()], 0, &policy).unwrap();
        assert_eq!(output[0].decay, None);
        assert_eq!(output[0].relevance, 0.0);
        assert_eq!(
            rank(vec![future], 0, &RankingPolicy::default()),
            Err(RankingError::FutureObservation)
        );
        let mut old = basic("old", None);
        old.observed_at_ms = -1;
        assert_eq!(
            rank(vec![old], i64::MAX, &policy),
            Err(RankingError::AgeOverflow)
        );
    }
}
