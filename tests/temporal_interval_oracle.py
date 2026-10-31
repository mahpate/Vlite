"""Stdlib reference for Gap4 validity intervals, not a graph implementation.

Run python tests\\temporal_interval_oracle.py --self-test from the checkout.
The default fixture is resolved beside __file__; --fixture overrides it.

Schema v1 has named series (segments, edge/source/target retention and TTL,
and explicit T/C queries), invalid_series with expected validation errors, and
limit_cases. Only limit-case *inputs* expand count into adjacent [i,i+1)
segments; expected results are always hand-authored, never generated.
Every object has exact required keys. Segment properties are JSON objects.
Null bounds mean infinities; null deadlines mean no TTL. Empty series are legal.
No commits, revisions, traversal, schema enforcement, or wall clock are modeled.
"""

import argparse
import copy
import json
import math
from pathlib import Path
import subprocess
import sys
import tempfile


FIXTURE = Path(__file__).resolve().parent / "fixtures" / "temporal_intervals" / "golden.json"
SCHEMA = "vlite.temporal-interval-golden"
I64_MIN = -(2**63)
I64_MAX = 2**63 - 1
MAX_SEGMENTS = 256


class InvalidSeries(ValueError):
    """Stable fixture-local error names, not proposed engine errors."""


def require(condition, message):
    if not condition:
        raise ValueError(message)


def keys(value, names, context):
    require(isinstance(value, dict) and set(value) == set(names.split()),
            f"{context}: expected keys {names}")


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, f"duplicate JSON key {key!r}")
        result[key] = value
    return result


def reject_constant(value):
    raise ValueError(f"invalid JSON constant {value!r}")


def finite_float(value):
    result = float(value)
    require(math.isfinite(result), f"non-finite JSON number {value!r}")
    return result


def load_fixture(path):
    return json.loads(path.read_text(encoding="utf-8"),
                      object_pairs_hook=unique_object,
                      parse_constant=reject_constant, parse_float=finite_float)


def is_timestamp(value):
    return type(value) is int and I64_MIN <= value <= I64_MAX


def validate_series(segments):
    if not isinstance(segments, list):
        raise InvalidSeries("series_type")
    if len(segments) > MAX_SEGMENTS:
        raise InvalidSeries("segment_limit")
    previous = None
    for segment in segments:
        if not isinstance(segment, dict) or set(segment) != {
                "valid_from", "valid_to", "properties"}:
            raise InvalidSeries("segment_fields")
        start, end = segment["valid_from"], segment["valid_to"]
        if any(bound is not None and not is_timestamp(bound) for bound in (start, end)):
            raise InvalidSeries("timestamp")
        if not isinstance(segment["properties"], dict):
            raise InvalidSeries("properties")
        if start is not None and end is not None and start >= end:
            raise InvalidSeries("bounds")
        if previous is not None:
            prior_start, prior_end = previous["valid_from"], previous["valid_to"]
            if start is None or (prior_start is not None and start < prior_start):
                raise InvalidSeries("order")
            if prior_end is None or start < prior_end:
                raise InvalidSeries("overlap")
        previous = segment


def validate_entity(entity, context):
    keys(entity, "retained expires_at_ms", context)
    require(type(entity["retained"]) is bool, f"{context}: retained must be boolean")
    deadline = entity["expires_at_ms"]
    require(deadline is None or (type(deadline) is int and deadline >= 0),
            f"{context}: deadline must be null or a nonnegative integer")


def select(segments, entities, as_of, clock):
    """Direct bounded scan; neither binary search nor integer infinity sentinels."""
    for entity in entities:
        deadline = entity["expires_at_ms"]
        if not entity["retained"] or (deadline is not None and clock >= deadline):
            return None
    for segment in segments:
        start, end = segment["valid_from"], segment["valid_to"]
        if (start is None or start <= as_of) and (end is None or as_of < end):
            return segment
    return None


def same_json(left, right):
    # Keep booleans and numeric property types distinct in golden comparisons.
    return json.dumps(left, sort_keys=True, allow_nan=False) == json.dumps(
        right, sort_keys=True, allow_nan=False)


def check_queries(segments, entities, queries, context):
    require(isinstance(queries, list) and queries, f"{context}: queries must be nonempty")
    for index, query in enumerate(queries):
        location = f"{context}.queries[{index}]"
        keys(query, "T C expected", location)
        require(is_timestamp(query["T"]), f"{location}: T must be signed i64")
        require(type(query["C"]) is int and query["C"] >= 0,
                f"{location}: C must be a nonnegative integer")
        expected = query["expected"]
        if expected is not None:
            validate_series([expected])
        actual = select(segments, entities, query["T"], query["C"])
        require(same_json(actual, expected),
                f"{location}: selection mismatch: expected {expected!r}, got {actual!r}")
    return len(queries)


def expect_series_error(segments, expected, context):
    try:
        validate_series(segments)
    except InvalidSeries as error:
        require(str(error) == expected,
                f"{context}: expected error {expected!r}, got {str(error)!r}")
    else:
        require(expected is None, f"{context}: expected error {expected!r}, got acceptance")


def validate_fixture(path):
    document = load_fixture(path)
    keys(document, "schema version series invalid_series limit_cases", "root")
    require(document["schema"] == SCHEMA, "schema: unsupported fixture schema")
    require(type(document["version"]) is int and document["version"] == 1,
            "version: expected integer 1")
    names = set()
    query_count = 0
    for group in ("series", "invalid_series", "limit_cases"):
        cases = document[group]
        require(isinstance(cases, list) and cases, f"{group}: expected a nonempty array")
        for case in cases:
            fields = {
                "series": "name segments edge source target queries",
                "invalid_series": "name segments expected_error",
                "limit_cases": "name segment_count expected_error queries",
            }[group]
            keys(case, fields, group)
            name = case["name"]
            require(isinstance(name, str) and name.strip(), f"{group}: invalid name")
            require(name not in names, f"{group}: duplicate name {name!r}")
            names.add(name)
            if group == "series":
                validate_series(case["segments"])
                entities = [case[field] for field in ("edge", "source", "target")]
                for field, entity in zip(("edge", "source", "target"), entities):
                    validate_entity(entity, f"{name}.{field}")
                query_count += check_queries(case["segments"], entities, case["queries"], name)
            else:
                expected = case["expected_error"]
                errors = {"series_type", "segment_fields", "timestamp", "properties",
                          "bounds", "order", "overlap", "segment_limit"}
                require(isinstance(expected, str) and expected in errors
                        or (group == "limit_cases" and expected is None),
                        f"{name}: invalid expected_error")
                if group == "invalid_series":
                    expect_series_error(case["segments"], expected, name)
                else:
                    count = case["segment_count"]
                    require(type(count) is int and 0 <= count <= 257,
                            f"{name}: limit input count must be in [0,257]")
                    segments = [{"valid_from": i, "valid_to": i + 1,
                                 "properties": {"value": "same"}} for i in range(count)]
                    expect_series_error(segments, expected, name)
                    if expected is None:
                        query_count += check_queries(segments, [], case["queries"], name)
                    else:
                        require(case["queries"] == [], f"{name}: rejected input cannot have queries")
    return len(names), query_count


def self_test(path):
    """Exercise the actual CLI with temporary files; never rewrite the source fixture."""
    document = load_fixture(path)
    mutations = []

    def mutated(label, change, diagnostic):
        value = copy.deepcopy(document)
        change(value)
        mutations.append((label, json.dumps(value, allow_nan=False), diagnostic))

    def first_query(value):
        return value["series"][0]["queries"][0]

    mutated("wrong-selection", lambda d: first_query(d).__setitem__(
        "expected", {"valid_from": None, "valid_to": None, "properties": {"wrong": True}}),
        "selection mismatch")
    matching = next((i, j) for i, case in enumerate(document["series"])
                    for j, query in enumerate(case["queries"])
                    if query["expected"] is not None and query["expected"]["valid_to"] is not None)
    i, j = matching
    mutated("wrong-bounds", lambda d: d["series"][i]["queries"][j]["expected"].__setitem__(
        "valid_to", None), "selection mismatch")
    mutated("bool-T", lambda d: first_query(d).__setitem__("T", True), "T must be signed i64")
    mutated("large-T", lambda d: first_query(d).__setitem__("T", I64_MAX + 1),
            "T must be signed i64")
    mutated("bool-C", lambda d: first_query(d).__setitem__("C", False),
            "C must be a nonnegative integer")
    mutated("negative-C", lambda d: first_query(d).__setitem__("C", -1),
            "C must be a nonnegative integer")
    mutated("missing-query-field", lambda d: first_query(d).pop("expected"), "expected keys")
    mutated("extra-query-field", lambda d: first_query(d).__setitem__("extra", 0), "expected keys")
    mutated("missing-root-field", lambda d: d.pop("series"), "expected keys")
    mutated("extra-root-field", lambda d: d.__setitem__("extra", []), "expected keys")
    mutated("bool-version", lambda d: d.__setitem__("version", True), "version:")
    mutated("wrong-series-type", lambda d: d.__setitem__("series", {}), "nonempty array")
    mutated("bad-retained", lambda d: d["series"][0]["edge"].__setitem__("retained", 1),
            "retained must be boolean")
    mutated("bad-deadline", lambda d: d["series"][0]["target"].__setitem__(
        "expires_at_ms", True), "deadline must be null or a nonnegative integer")
    mutated("negative-deadline", lambda d: d["series"][0]["source"].__setitem__(
        "expires_at_ms", -1), "deadline must be null or a nonnegative integer")
    mutated("missing-entity-field", lambda d: d["series"][0]["edge"].pop("retained"),
            "expected keys")
    mutated("missing-segment-field", lambda d: d["series"][0]["segments"][0].pop(
        "valid_to"), "segment_fields")
    mutated("extra-segment-field", lambda d: d["series"][0]["segments"][0].__setitem__(
        "extra", None), "segment_fields")
    mutated("bool-bound", lambda d: d["series"][0]["segments"][0].__setitem__(
        "valid_from", False), "timestamp")
    mutated("wrong-error", lambda d: d["invalid_series"][0].__setitem__(
        "expected_error", "overlap"), "expected error")
    mutations.extend([
        ("duplicate-key", '{"schema": 1, "schema": 2}', "duplicate JSON key"),
        ("nested-duplicate", '{"x": {"T": 0, "T": 1}}', "duplicate JSON key"),
        ("nan", '{"x": NaN}', "invalid JSON constant"),
        ("infinity", '{"x": Infinity}', "invalid JSON constant"),
        ("negative-infinity", '{"x": -Infinity}', "invalid JSON constant"),
        ("overflow-float", '{"x": 1e999}', "non-finite JSON number"),
        ("truncated", '{"schema":', "Expecting value"),
        ("root-array", "[]", "expected keys"),
    ])
    with tempfile.TemporaryDirectory(prefix="vlite-temporal-oracle-") as directory:
        for label, text, diagnostic in mutations:
            broken = Path(directory) / f"{label}.json"
            broken.write_text(text, encoding="utf-8")
            result = subprocess.run(
                [sys.executable, str(Path(__file__).resolve()), "--fixture", str(broken)],
                capture_output=True, text=True, timeout=30, check=False,
            )
            require(result.returncode == 1 and diagnostic in result.stderr,
                    f"self-test {label} was not rejected correctly: "
                    f"{result.returncode} {result.stdout} {result.stderr}")
    return len(mutations)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", type=Path, default=FIXTURE, help="override golden JSON path")
    parser.add_argument("--self-test", action="store_true", help="also reject temporary mutated fixtures")
    args = parser.parse_args()
    try:
        cases, queries = validate_fixture(args.fixture)
        print(f"Verified {cases} temporal cases and {queries} selections: {args.fixture}")
        if args.self_test:
            count = self_test(args.fixture)
            print(f"Rejected {count} temporary malformed or incorrect fixtures")
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"{args.fixture}: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
