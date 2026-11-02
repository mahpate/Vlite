"""Independent ranking-only oracle; no Rust parity is claimed.

Source: docs/projects/roadmap/gap1-agent-memory.md sections 3 and 8.
Run python -B tests\\memory_ranking_oracle.py --self-test (also works with -O).
Without --self-test, read one JSON request from stdin and emit one JSON result.
Only an already-bounded eligible set is modeled: no retrieval, scope filtering,
TTL, temporal visibility, MMR, exact rerank, custom rerankers or providers.
Graph-only status is explained, but graph paths/similarity are not invented.

Resolved ambiguities (each is a fixture-local assumption, not spec):
* Version is "fixture.agent-memory-ranking.v1", not an engine/API version.
* T is required and represents captured now as well as the decay reference;
  no separate historical as-of clock or wall-clock default is modeled.
  No half-life is invented: when
  wD=0 and it is absent, policy half_life_ms and the raw D diagnostic are null.
  Raw R/D/I/S diagnostics and their serialization are fixture-local, not an API.
  All candidates are validated before limiting; limits outside 1..100 reject.
* Future observations reject by default. The explicit fixture-local policy
  future_observations="clamp" allows them; "reject" is the default.
  Signed-i64 T-O is checked BEFORE clamping, including negative overflow.
* Numbers are exact rationals from JSON decimal spelling (Decimal), not
  simulated f64 arithmetic. Direct Python floats use their repr decimal.
  R/D/I/S and numeric policy fields are reduced Fraction strings ("1", "1/3").
  Expected and actual JSON structures, including those strings, compare exactly;
  no score epsilon, rounding, or platform-f64 equivalence is claimed.
* The weight sum must satisfy abs(sum-1) <= 1/1000000000000, inclusive,
  in exact rational arithmetic; accepted weights are NOT renormalized.
  Probes use +/-5e-13 and +/-2e-12 offsets, safely inside/outside even as
  binary64 values. An exclusive-boundary mutant is not distinguishable there.
* Scope is exactly [tenant, agent, user|null, session|null]. Strings compare
  lexicographically by Unicode code point, with no case folding/normalization;
  null sorts before every present string (including ""). IDs are strings.
* Missing/null rank means expanded-only. Ranked positions are dense 1..k,
  k <= 1000; this input representation/bound is fixture-local. Booleans are
  not numbers/integers. Uniqueness keys are (scope, id, segment), not bare IDs.
* Missing/null segment is non-temporal and sorts before a present segment.
  Present bounds are two finite signed-i64 integers with from < to; compare
  from then to. Bounds are ordering metadata, never visibility predicates.
* Input/policy/candidate objects reject unknown fields; null importance, limit,
  half-life or weights are invalid, not defaults. Named errors are local only.
* Fixture catalogs are lossless input/expected JSON references; an ids/template
  entry expands its explicitly listed IDs without calculating scores or order.
  No expected arithmetic, sorting or slicing is performed. The fixture-only
  {"$number": "..."} marker injects nan/+inf/-inf
  into malformed inputs; it is NOT part of the oracle's input schema.
  {"$ranked_count": n} expands only rejection-test inputs into n distinct
  candidates with dense ranks 1..n; it never constructs expected output.
* Seeded shuffles test a fixed set of eligible inputs, not candidate discovery.
  Mutants each alter one named behavior and must fail a static golden, including
  malformed-input and comparator goldens; no expectations are computed/written.
"""

import argparse
import copy
from decimal import Decimal
from fractions import Fraction
import json
import math
from pathlib import Path
import random
import sys


FIXTURE = Path(__file__).resolve().parent / "fixtures" / "agent_memory" / "ranking_golden.json"
VERSION = "fixture.agent-memory-ranking.v1"
I64_MIN, I64_MAX = -(2**63), 2**63 - 1
SUM_TOLERANCE = Fraction(1, 10**12)
SEEDS = (7, 101, 2026, 65537, 99173)
MUTATIONS = (
    "rank_instead_of_reciprocal",
    "unclamped_age",
    "expanded_nonzero_relevance",
    "ignore_scope_ties",
    "ignore_id_ties",
    "ignore_segment_ties",
    "limit_before_sort",
    "skip_weight_validation",
    "allow_zero_half_life",
    "wrong_importance_default",
    "ascending_score",
    "wrong_decay_formula",
    "unchecked_age_overflow",
    "accept_duplicate_rank",
    "accept_duplicate_id",
    "renormalize_weights",
    "allow_future_observations",
    "accept_sparse_ranks",
    "allow_excess_ranks",
    "allow_missing_half_life",
    "allow_session_without_user",
    "id_only_uniqueness",
    "wrong_default_limit",
    "omit_version",
    "omit_effective_time",
)


class RankingError(ValueError):
    """Named fixture-local validation failure, not an engine error taxonomy."""


def require(condition, error):
    if not condition:
        raise RankingError(error)


def fields(value, required, optional, error):
    require(isinstance(value, dict), error)
    require(set(required) <= set(value) <= set(required) | set(optional), error)


def timestamp(value, error):
    require(type(value) is int and I64_MIN <= value <= I64_MAX, error)
    return value


def number(value, error):
    require(type(value) in (int, float, Decimal), error)
    if type(value) is float:
        require(math.isfinite(value), error)
        return Fraction(repr(value))
    if type(value) is Decimal:
        require(value.is_finite(), error)
    return Fraction(value)


def scope_key(scope):
    return tuple((0, "") if part is None else (1, part) for part in scope)


def canonical_key(candidate, mutation=None):
    scope = () if mutation == "ignore_scope_ties" else scope_key(candidate["scope"])
    identity = "" if mutation == "ignore_id_ties" else candidate["id"]
    segment = candidate.get("segment")
    bounds = (0,) if segment is None else (1, *segment)
    if mutation == "ignore_segment_ties":
        bounds = (0,)
    return scope, identity, bounds


def rank_memories(request, mutation=None):
    """Validate, rank and explain a bounded set; mutate only for self-testing."""
    fields(request, ("T", "candidates"), ("policy",), "request_fields")
    effective_t = timestamp(request["T"], "time_i64")
    policy = request.get("policy", {})
    fields(policy, (), ("weights", "half_life_ms", "limit", "future_observations"),
           "policy_fields")
    future_policy = policy.get("future_observations", "reject")
    require(future_policy in ("reject", "clamp"), "future_policy")
    raw_weights = policy.get("weights", [1, 0, 0])
    require(isinstance(raw_weights, list) and len(raw_weights) == 3, "weights")
    weights = [number(weight, "weights") for weight in raw_weights]
    if mutation != "skip_weight_validation":
        tolerance_ok = abs(sum(weights) - 1) <= SUM_TOLERANCE
        require(all(weight >= 0 for weight in weights) and tolerance_ok, "weights")
    if mutation == "renormalize_weights" and sum(weights):
        weights = [weight / sum(weights) for weight in weights]
    require("half_life_ms" in policy or weights[1] == 0 or
            mutation == "allow_missing_half_life", "half_life_required")
    half_life = (number(policy["half_life_ms"], "half_life")
                 if "half_life_ms" in policy else None)
    if mutation == "allow_missing_half_life" and half_life is None and weights[1] > 0:
        half_life = Fraction(1)
    require(half_life is None or half_life > 0 or
            (mutation == "allow_zero_half_life" and half_life == 0), "half_life")
    # A zero-accepting mutant needs a defined downstream value to expose acceptance.
    if mutation == "allow_zero_half_life" and half_life == 0:
        half_life = Fraction(1)
    limit = policy.get("limit", 19 if mutation == "wrong_default_limit" else 20)
    require(type(limit) is int and 1 <= limit <= 100, "limit")
    candidates = request["candidates"]
    require(isinstance(candidates, list), "candidates")
    ids, ranks, scored = set(), set(), []
    for candidate in candidates:
        fields(candidate, ("id", "scope", "O"),
               ("rank", "importance", "segment"), "candidate_fields")
        identity = candidate["id"]
        require(isinstance(identity, str), "memory_id")
        scope = candidate["scope"]
        require(isinstance(scope, list) and len(scope) == 4 and
                all(isinstance(part, str) for part in scope[:2]) and
                all(part is None or isinstance(part, str) for part in scope[2:]), "scope")
        require(scope[3] is None or scope[2] is not None or
                mutation == "allow_session_without_user", "session_requires_user")
        observation = timestamp(candidate["O"], "observation_i64")
        age = effective_t - observation
        require(mutation == "unchecked_age_overflow" or I64_MIN <= age <= I64_MAX,
                "age_overflow")
        require(observation <= effective_t or future_policy == "clamp" or
                mutation == "allow_future_observations", "future_observation")
        if mutation != "unclamped_age":
            age = max(0, age)
        importance = number(candidate.get(
            "importance", 0 if mutation == "wrong_importance_default" else Decimal("0.5")),
            "importance")
        require(0 <= importance <= 1, "importance")
        rank = candidate.get("rank")
        require(rank is None or (type(rank) is int and rank >= 1), "rank")
        if rank is not None:
            require(rank not in ranks or mutation == "accept_duplicate_rank",
                    "duplicate_rank")
            ranks.add(rank)
        segment = candidate.get("segment")
        if segment is not None:
            require(isinstance(segment, list) and len(segment) == 2, "segment")
            start = timestamp(segment[0], "segment_i64")
            end = timestamp(segment[1], "segment_i64")
            require(start < end, "segment_bounds")
        identity_key = (tuple(scope), identity,
                        None if segment is None else tuple(segment))
        if mutation == "id_only_uniqueness":
            identity_key = identity
        require(identity_key not in ids or mutation == "accept_duplicate_id",
                "duplicate_id")
        ids.add(identity_key)
        relevance = Fraction(0) if rank is None else Fraction(1, rank)
        if mutation == "rank_instead_of_reciprocal" and rank is not None:
            relevance = Fraction(rank)
        if mutation == "expanded_nonzero_relevance" and rank is None:
            relevance = Fraction(1)
        decay = None if half_life is None else 1 / (1 + Fraction(age) / half_life)
        if mutation == "wrong_decay_formula" and half_life is not None:
            decay = 1 / (1 + Fraction(age) / half_life) ** 2
        score = sum(weight * component for weight, component in
                    zip(weights, (relevance, decay if decay is not None else 0,
                                  importance)))
        result = {
            "id": identity, "scope": scope, "segment": segment,
            "R": str(relevance), "D": None if decay is None else str(decay),
            "I": str(importance), "S": str(score),
            "ranking_version": VERSION, "rank": rank, "expanded_only": rank is None,
        }
        if mutation == "omit_version":
            del result["ranking_version"]
        scored.append((score, canonical_key(candidate, mutation), result))
    require(mutation == "allow_excess_ranks" or len(ranks) <= 1000, "rank_count")
    require(mutation == "accept_sparse_ranks" or ranks == set(range(1, len(ranks) + 1)),
            "dense_ranks")
    if mutation == "limit_before_sort":
        scored = scored[:limit]
    scored.sort(key=lambda entry: (entry[0] if mutation == "ascending_score"
                                  else -entry[0], entry[1]))
    output = {
        "T": effective_t,
        "policy": {"weights": list(map(str, weights)),
                   "half_life_ms": None if half_life is None else str(half_life),
                   "limit": limit,
                   "ranking_version": VERSION},
        "memories": [entry[2] for entry in scored[:limit]],
    }
    if mutation == "omit_effective_time":
        del output["T"]
    return output


def unique_object(pairs):
    value = {}
    for key, item in pairs:
        require(key not in value, "duplicate_json_key")
        value[key] = item
    return value


def reject_constant(value):
    raise RankingError("nonfinite_json_number")


def parse_json(raw):
    return json.loads(raw, parse_float=Decimal, parse_constant=reject_constant,
                      object_pairs_hook=unique_object)


def fixture_numbers(value):
    if isinstance(value, dict):
        if set(value) == {"$ranked_count"}:
            count = value["$ranked_count"]
            require(type(count) is int and 0 <= count <= 1001, "fixture_ranked_count")
            return [{"id": f"rank-{rank}", "scope": ["t", "a", None, None],
                     "O": 0, "rank": rank} for rank in range(1, count + 1)]
        if set(value) == {"$number"}:
            require(value["$number"] in ("nan", "+inf", "-inf"), "fixture_number")
            return Decimal(value["$number"].replace("inf", "Infinity"))
        return {key: fixture_numbers(item) for key, item in value.items()}
    if isinstance(value, list):
        return [fixture_numbers(item) for item in value]
    return value


def resolve_case(fixture, case):
    def records(catalog, references):
        output = []
        for name in references:
            record = catalog[name]
            if "ids" in record:
                output.extend(dict(copy.deepcopy(record["template"]), id=identity)
                              for identity in record["ids"])
            else:
                output.append(copy.deepcopy(record))
        return output

    policy = fixture["policies"][case["policy"]]
    request = {"T": case["T"],
               "candidates": records(fixture["candidates"], case["candidates"])}
    if policy["input"] is not None:
        request["policy"] = copy.deepcopy(policy["input"])
    expected = {"T": case["expected_T"], "policy": policy["expected"],
                "memories": records(fixture["expected_memories"],
                                    case["expected_memories"])}
    return request, expected


def golden_failures(fixture, mutation=None):
    failures = []
    for case in fixture["cases"]:
        request, expected = resolve_case(fixture, case)
        try:
            actual = rank_memories(request, mutation)
        except (RankingError, ZeroDivisionError) as error:
            failures.append(f'{case["name"]}: unexpected {error}')
        else:
            if actual != expected:
                failures.append(f'{case["name"]}: output mismatch')
    for case in fixture["invalid"]:
        try:
            rank_memories(fixture_numbers(case["input"]), mutation)
        except RankingError as error:
            if str(error) != case["error"]:
                failures.append(f'{case["name"]}: expected {case["error"]}, got {error}')
        else:
            failures.append(f'{case["name"]}: accepted malformed input')
    for case in fixture["comparators"]:
        actual = sorted(case["input"], key=lambda item: canonical_key(item, mutation))
        if actual != case["expected"]:
            failures.append(f'{case["name"]}: comparator mismatch')
    return failures


def self_test(path):
    fixture = parse_json(path.read_text(encoding="utf-8"))
    require(fixture["schema"] == "vlite.memory-ranking-golden.v1", "fixture_schema")
    require(len(fixture["cases"]) >= 30, "at_least_30_ranking_goldens")
    require(len(fixture["invalid"]) >= 12, "at_least_12_invalid_goldens")
    names = [case["name"] for group in ("cases", "invalid", "comparators")
             for case in fixture[group]]
    require(len(names) == len(set(names)), "unique_fixture_names")
    for group in ("cases", "invalid", "comparators"):
        for case in fixture[group]:
            require(isinstance(case.get("assumptions"), list) and
                    all(item in fixture["assumptions"] for item in case["assumptions"]),
                    f'assumptions:{case["name"]}')
    failures = golden_failures(fixture)
    require(not failures, "\n".join(failures))
    # These probes remain unambiguous when JSON decimals enter as Python floats.
    for case in fixture["cases"]:
        if case["policy"] in ("tolerance_plus", "tolerance_minus"):
            request, expected = resolve_case(fixture, case)
            request["policy"]["weights"] = [
                float(weight) for weight in request["policy"]["weights"]]
            require(rank_memories(request) == expected, f'float_probe:{case["name"]}')
    for case in fixture["invalid"]:
        if case["name"] in ("weight_plus_2e_minus_12", "weight_minus_2e_minus_12"):
            request = copy.deepcopy(case["input"])
            request["policy"]["weights"] = [
                float(weight) for weight in request["policy"]["weights"]]
            try:
                rank_memories(request)
            except RankingError as error:
                require(str(error) == "weights", f'float_probe:{case["name"]}:{error}')
            else:
                raise RankingError(f'float_probe:{case["name"]}:accepted')
    permutations = 0
    for case in fixture["cases"]:
        request, expected = resolve_case(fixture, case)
        for seed in SEEDS:
            shuffled = copy.deepcopy(request)
            random.Random(seed).shuffle(shuffled["candidates"])
            require(rank_memories(shuffled) == expected,
                    f'permutation:{case["name"]}:{seed}')
            permutations += 1
    detections = {}
    require(set(fixture["mutation_checks"]) == set(MUTATIONS), "mutation_check_names")
    for mutation in MUTATIONS:
        caught = golden_failures(fixture, mutation)
        require(bool(caught), f"undetected_mutation:{mutation}")
        intended = fixture["mutation_checks"][mutation]
        require(intended in caught, f"wrong_mutation_reason:{mutation}:{caught}")
        detections[mutation] = intended
    require(len(detections) >= 12, "at_least_12_detected_mutations")
    print(f'PASS ranking={len(fixture["cases"])} invalid={len(fixture["invalid"])} '
          f'comparators={len(fixture["comparators"])} permutations={permutations} '
          f'mutations={len(detections)}/{len(MUTATIONS)} exact-rational')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--fixture", type=Path, default=FIXTURE)
    args = parser.parse_args()
    try:
        if args.self_test:
            self_test(args.fixture)
        else:
            print(json.dumps(rank_memories(parse_json(sys.stdin.read())),
                             ensure_ascii=True, sort_keys=True))
    except (RankingError, json.JSONDecodeError, OSError) as error:
        print(f"FAIL {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
