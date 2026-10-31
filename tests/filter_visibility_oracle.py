"""Independent stdlib reference for the bounded P5 filter/visibility contract.

Run python tests\\filter_visibility_oracle.py --self-test. Defaults use __file__.
Schema vlite.filter-visibility-golden v1 uses the existing metadata JSON tags:
string, integer (canonical decimal i64 string), float (finite JSON float),
boolean, and geopoint. No null/array metadata or int/float/bool coercion.
Ranges have nullable, inclusive, same-type numeric endpoints, at least one set.

Rows are full revisions, including metadata/expiry edits and tombstones.
Sealed rows are strictly ordered by (id, revision); overlay by (revision, id),
with revisions newer than every sealed revision. Revisions are unique per ID
across the snapshot. Segment array order is output order, not revision order.
IDs, revisions, deadlines and query clocks are u64 JSON integers (not booleans).
Committed row revisions must be nonzero; zero remains valid for IDs and clocks.
Latest revision wins BEFORE deletion/TTL/predicate checks; no fallback.

Complete summaries cover ALL physical rows, including expired/deleted/shadowed
ones; extra dictionary values and wider numeric intervals are safe. Missing
types/fields in complete coverage prove absence. Stale/unknown coverage is
untrusted even if its contents claim absence. Per-type numeric intervals and
equality dictionaries are supersets, never the final conjunction predicate.
Candidates are retained physical IDs plus overlay IDs, resolved to visible
latest rows and rechecked. Outputs are ascending IDs, not score/rank order.

Geo equality normalizes zero like other f64 keys. Geo radius is accepted ONLY
in pruning probes and always UNKNOWN for a nonempty segment unless another
condition proves disjointness. Radius filtering/scoring, ANN, performance,
storage authentication, generations and serving formats are outside v1.
Expected answers are hand-authored in golden.json, never written by this tool.
"""

import argparse
import copy
import json
import math
from pathlib import Path
import re
import subprocess
import sys
import tempfile


FIXTURE = Path(__file__).resolve().parent / "fixtures" / "filter_visibility" / "golden.json"
SCHEMA = "vlite.filter-visibility-golden"
U64_MAX = 2**64 - 1
I64_MIN = -(2**63)
I64_MAX = 2**63 - 1
TYPES = ("string", "integer", "float", "boolean", "geopoint")


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


def u64(value, context):
    require(type(value) is int and 0 <= value <= U64_MAX,
            f"{context}: expected u64 integer")


def text(value, maximum, context):
    require(isinstance(value, str), f"{context}: expected string")
    require(len(value.encode("utf-8")) <= maximum, f"{context}: UTF-8 byte limit {maximum}")


def name(value, context):
    text(value, 256, context)
    require(bool(value.strip()), f"{context}: expected nonempty name")


def field(value, context):
    text(value, 256, context)
    require(bool(value), f"{context}: expected nonempty field")


def payload(tag, value, context):
    if tag == "integer":
        require(isinstance(value, str) and re.fullmatch(r"0|-?[1-9][0-9]*", value),
                f"{context}: expected canonical decimal i64 string")
        # Bound length before int(), including on Python versions with digit limits.
        require(len(value) <= 20, f"{context}: i64 out of range")
        number = int(value)
        require(I64_MIN <= number <= I64_MAX, f"{context}: i64 out of range")
        return number
    if tag == "float":
        require(type(value) is float and math.isfinite(value),
                f"{context}: expected finite f64 JSON float")
        return 0.0 if value == 0.0 else value
    if tag == "boolean":
        require(type(value) is bool, f"{context}: expected boolean")
        return value
    if tag == "string":
        text(value, 4096, context)
        return value
    if tag == "geopoint":
        keys(value, "latitude longitude", context)
        lat = payload("float", value["latitude"], f"{context}.latitude")
        lon = payload("float", value["longitude"], f"{context}.longitude")
        require(-90.0 <= lat <= 90.0 and -180.0 <= lon <= 180.0,
                f"{context}: geo coordinates out of range")
        return lat, lon
    raise ValueError(f"{context}: unsupported metadata type {tag!r}")


def typed(value, context):
    keys(value, "type value", context)
    tag = value["type"]
    require(isinstance(tag, str) and tag in TYPES, f"{context}: unsupported metadata type")
    return tag, payload(tag, value["value"], context)


def metadata(value, context):
    require(isinstance(value, dict) and len(value) <= 64,
            f"{context}: metadata must be an object with at most 64 fields")
    size = 4
    for key, item in value.items():
        field(key, context)
        tag, decoded = typed(item, f"{context}.{key}")
        size += 2 + len(key.encode("utf-8")) + 1
        size += (4 + len(decoded.encode("utf-8")) if tag == "string"
                 else {"integer": 8, "float": 8, "boolean": 1, "geopoint": 16}[tag])
    require(size <= 65536, f"{context}: encoded metadata byte limit 65536")


def conditions(value, context, probes=False):
    require(isinstance(value, list) and len(value) <= 64,
            f"{context}: expected at most 64 conditions")
    for index, condition in enumerate(value):
        location = f"{context}[{index}]"
        require(isinstance(condition, dict), f"{location}: expected condition object")
        op = condition.get("op")
        if op == "eq":
            keys(condition, "op field value", location)
            typed(condition["value"], location)
        elif op == "range":
            keys(condition, "op field min max", location)
            bounds = [typed(condition[key], location) for key in ("min", "max")
                      if condition[key] is not None]
            require(bounds and all(tag in ("integer", "float") for tag, _ in bounds),
                    f"{location}: range requires numeric bound")
            require(len(bounds) == 1 or
                    (bounds[0][0] == bounds[1][0] and bounds[0][1] <= bounds[1][1]),
                    f"{location}: same-type range bounds must have min <= max")
        elif op == "georadius" and probes:
            keys(condition, "op field latitude longitude radius_meters", location)
            payload("geopoint", {"latitude": condition["latitude"],
                                 "longitude": condition["longitude"]}, location)
            radius = payload("float", condition["radius_meters"], location)
            require(radius >= 0.0, f"{location}: radius must be nonnegative")
        else:
            raise ValueError(f"{location}: unsupported condition (radius is pruning-only)")
        field(condition["field"], location)


def matches(value, predicates):
    for condition in predicates:
        item = value.get(condition["field"])
        if item is None:
            return False
        tag, decoded = typed(item, "metadata")
        if condition["op"] == "eq":
            if (tag, decoded) != typed(condition["value"], "equality"):
                return False
        else:
            for key, lower in (("min", True), ("max", False)):
                bound = condition[key]
                if bound is not None:
                    bound_tag, bound_value = typed(bound, "range")
                    if tag != bound_tag or (decoded < bound_value if lower else decoded > bound_value):
                        return False
    return True


def summary(value, rows, context):
    keys(value, "coverage fields", context)
    require(value["coverage"] in ("complete", "stale", "unknown"),
            f"{context}: invalid coverage")
    require(isinstance(value["fields"], dict) and len(value["fields"]) <= 64,
            f"{context}: summary fields must be an object with at most 64 fields")
    for key, entries in value["fields"].items():
        field(key, context)
        require(isinstance(entries, list) and entries, f"{context}.{key}: nonempty type summaries")
        tags = []
        for entry in entries:
            require(isinstance(entry, dict), f"{context}.{key}: expected summary object")
            tag = entry.get("type")
            require(isinstance(tag, str) and tag in TYPES, f"{context}: unsupported summary type")
            tags.append(tag)
            if tag in ("integer", "float"):
                keys(entry, "type min max", context)
                lo = payload(tag, entry["min"], context)
                hi = payload(tag, entry["max"], context)
                require(lo <= hi, f"{context}: summary min > max")
            else:
                keys(entry, "type values", context)
                require(isinstance(entry["values"], list) and entry["values"],
                        f"{context}: nonempty summary dictionary")
                decoded = [payload(tag, item, context) for item in entry["values"]]
                require(all(a < b for a, b in zip(decoded, decoded[1:])),
                        f"{context}: dictionary must be strictly sorted and unique after zero normalization")
        require(len(set(tags)) == len(tags), f"{context}: duplicate summary type")
    if value["coverage"] == "complete":
        for row in rows:
            for key, item in row["metadata"].items():
                tag, decoded = typed(item, context)
                entry = next((entry for entry in value["fields"].get(key, [])
                              if entry["type"] == tag), None)
                require(entry is not None and covers(entry, decoded),
                        f"{context}: complete summary does not cover physical row {row['id']} field {key}")


def covers(entry, value):
    tag = entry["type"]
    if tag in ("integer", "float"):
        return payload(tag, entry["min"], "summary") <= value <= payload(tag, entry["max"], "summary")
    return value in [payload(tag, item, "summary") for item in entry["values"]]


def possible(value, condition):
    if condition["op"] == "georadius":
        return "unknown"
    if value["coverage"] != "complete":
        return "unknown"
    if condition["op"] == "eq":
        tag, decoded = typed(condition["value"], "equality")
        entry = next((entry for entry in value["fields"].get(condition["field"], [])
                      if entry["type"] == tag), None)
        return "keep" if entry is not None and covers(entry, decoded) else "prune"
    bound = condition["min"] if condition["min"] is not None else condition["max"]
    tag = bound["type"]
    entry = next((entry for entry in value["fields"].get(condition["field"], [])
                  if entry["type"] == tag), None)
    if entry is None:
        return "prune"
    if condition["min"] is not None:
        if payload(tag, entry["max"], "summary") < typed(condition["min"], "range")[1]:
            return "prune"
    if condition["max"] is not None:
        if payload(tag, entry["min"], "summary") > typed(condition["max"], "range")[1]:
            return "prune"
    return "keep"


def pruning(segments, predicates):
    result = []
    for segment in segments:
        states = [possible(segment["summary"], condition) for condition in predicates]
        status = ("prune" if not segment["rows"] or "prune" in states else
                  "unknown" if "unknown" in states else "keep")
        result.append({"segment": segment["name"], "status": status})
    return result


def rows(value, context, seen, overlay=False):
    require(isinstance(value, list), f"{context}: expected row array")
    previous = None
    for index, row in enumerate(value):
        location = f"{context}[{index}]"
        keys(row, "id revision deleted expires_at metadata", location)
        u64(row["id"], f"{location}.id")
        u64(row["revision"], f"{location}.revision")
        require(row["revision"] > 0, f"{location}: expected nonzero revision")
        require(type(row["deleted"]) is bool, f"{location}: deleted must be boolean")
        if row["expires_at"] is not None:
            u64(row["expires_at"], f"{location}.expires_at")
        metadata(row["metadata"], f"{location}.metadata")
        identity = row["id"], row["revision"]
        require(identity not in seen, f"{location}: duplicate ID/revision")
        seen.add(identity)
        order = (row["revision"], row["id"]) if overlay else identity
        require(previous is None or previous < order, f"{location}: row ordering violation")
        previous = order


def id_list(value, context):
    require(isinstance(value, list), f"{context}: expected ID array")
    for item in value:
        u64(item, context)
    require(all(a < b for a, b in zip(value, value[1:])),
            f"{context}: IDs must be strictly sorted and unique")


def pruning_answer(value, segments, context):
    require(isinstance(value, list) and len(value) == len(segments),
            f"{context}: expected one pruning outcome per segment")
    for answer, segment in zip(value, segments):
        keys(answer, "segment status", context)
        require(answer["segment"] == segment["name"], f"{context}: segment ordering mismatch")
        require(answer["status"] in ("keep", "prune", "unknown"), f"{context}: invalid pruning status")


def evaluate(case, query):
    latest = {}
    for row in [row for segment in case["segments"] for row in segment["rows"]] + case["overlay"]:
        if row["id"] not in latest or row["revision"] > latest[row["id"]]["revision"]:
            latest[row["id"]] = row
    visible = {id_: row for id_, row in latest.items()
               if not row["deleted"] and
               (row["expires_at"] is None or row["expires_at"] > query["clock"])}
    outcomes = pruning(case["segments"], query["conditions"])
    retained_ids = {row["id"] for segment, outcome in zip(case["segments"], outcomes)
                    if outcome["status"] != "prune" for row in segment["rows"]}
    retained_ids.update(row["id"] for row in case["overlay"])
    candidates = sorted(retained_ids & visible.keys())
    matching = sorted(id_ for id_, row in visible.items() if matches(row["metadata"], query["conditions"]))
    rechecked = [id_ for id_ in candidates if matches(visible[id_]["metadata"], query["conditions"])]
    require(rechecked == matching, f"{case['name']}: unsafe candidate superset")
    return {"visible_ids": sorted(visible), "candidate_ids": candidates,
            "matching_ids": matching, "pruning": outcomes}


def validate_fixture(path, report=False):
    document = load_fixture(path)
    keys(document, "schema version cases", "root")
    require(document["schema"] == SCHEMA, "schema: unsupported fixture schema")
    require(type(document["version"]) is int and document["version"] == 1,
            "version: expected integer 1")
    require(isinstance(document["cases"], list) and document["cases"], "cases: nonempty array required")
    names = set()
    count = 0
    for case in document["cases"]:
        keys(case, "name purpose segments overlay queries pruning_probes", "case")
        name(case["name"], "case.name")
        require(case["name"] not in names, f"duplicate case name {case['name']!r}")
        names.add(case["name"])
        name(case["purpose"], f"{case['name']}.purpose")
        context = case["name"]
        require(isinstance(case["segments"], list), f"{context}: expected segment array")
        segment_names, seen = set(), set()
        sealed_max = -1
        for segment in case["segments"]:
            keys(segment, "name rows summary", context)
            name(segment["name"], f"{context}.segment")
            require(segment["name"] not in segment_names, f"{context}: duplicate segment name")
            segment_names.add(segment["name"])
            rows(segment["rows"], f"{context}.{segment['name']}", seen)
            summary(segment["summary"], segment["rows"], f"{context}.{segment['name']}.summary")
            sealed_max = max([sealed_max] + [row["revision"] for row in segment["rows"]])
        rows(case["overlay"], f"{context}.overlay", seen, overlay=True)
        require(all(row["revision"] > sealed_max for row in case["overlay"]),
                f"{context}: overlay revisions must be newer than sealed revisions")
        require(isinstance(case["queries"], list) and case["queries"],
                f"{context}: queries must be nonempty array")
        query_names = set()
        for query in case["queries"]:
            keys(query, "name clock conditions expected", context)
            name(query["name"], context)
            require(query["name"] not in query_names, f"{context}: duplicate query name")
            query_names.add(query["name"])
            location = f"{context}/{query['name']}"
            u64(query["clock"], f"{location}.clock")
            conditions(query["conditions"], location)
            expected = query["expected"]
            keys(expected, "visible_ids candidate_ids matching_ids pruning", location)
            for key in ("visible_ids", "candidate_ids", "matching_ids"):
                id_list(expected[key], f"{location}.{key}")
            pruning_answer(expected["pruning"], case["segments"], location)
            actual = evaluate(case, query)
            for key in actual:
                require(actual[key] == expected[key],
                        f"{location}.{key}: expected {expected[key]!r}, oracle {actual[key]!r}")
            count += 1
            if report:
                print(f"OK {location}: visible={len(actual['visible_ids'])} "
                      f"candidates={len(actual['candidate_ids'])} matches={actual['matching_ids']}")
        require(isinstance(case["pruning_probes"], list), f"{context}: expected pruning probe array")
        for probe in case["pruning_probes"]:
            keys(probe, "name conditions expected", context)
            name(probe["name"], context)
            require(probe["name"] not in query_names, f"{context}: duplicate query/probe name")
            query_names.add(probe["name"])
            location = f"{context}/{probe['name']}"
            conditions(probe["conditions"], location, probes=True)
            pruning_answer(probe["expected"], case["segments"], location)
            actual = pruning(case["segments"], probe["conditions"])
            require(actual == probe["expected"],
                    f"{location}: expected {probe['expected']!r}, oracle {actual!r}")
            count += 1
            if report:
                print(f"OK {location}: pruning-only {actual}")
    return len(names), count


def self_test(path):
    """Exercise CLI rejection, including diagnostics and exit status, on copies."""
    document = load_fixture(path)
    mutations = []

    def mutated(label, change, diagnostic):
        value = copy.deepcopy(document)
        change(value)
        mutations.append((label, json.dumps(value, allow_nan=False), diagnostic))

    def query(value):
        return value["cases"][0]["queries"][0]

    def row(value):
        return value["cases"][0]["segments"][0]["rows"][0]

    mutated("wrong-matches", lambda d: query(d)["expected"].__setitem__("matching_ids", []), "oracle")
    mutated("wrong-visible", lambda d: query(d)["expected"].__setitem__("visible_ids", []), "oracle")
    mutated("wrong-candidates", lambda d: query(d)["expected"].__setitem__("candidate_ids", []), "oracle")
    mutated("wrong-pruning", lambda d: query(d)["expected"]["pruning"][0].__setitem__(
        "status", "prune"), "oracle")
    mutated("missing-root", lambda d: d.pop("cases"), "expected keys")
    mutated("extra-root", lambda d: d.__setitem__("extra", 1), "expected keys")
    mutated("bad-schema", lambda d: d.__setitem__("schema", "other"), "unsupported fixture schema")
    mutated("bool-version", lambda d: d.__setitem__("version", True), "version:")
    mutated("future-version", lambda d: d.__setitem__("version", 2), "version:")
    mutated("missing-row", lambda d: row(d).pop("revision"), "expected keys")
    mutated("extra-row", lambda d: row(d).__setitem__("extra", 1), "expected keys")
    mutated("bool-id", lambda d: row(d).__setitem__("id", True), "expected u64")
    mutated("large-id", lambda d: row(d).__setitem__("id", U64_MAX + 1), "expected u64")
    mutated("negative-revision", lambda d: row(d).__setitem__("revision", -1), "expected u64")
    mutated("zero-revision", lambda d: row(d).__setitem__("revision", 0), "nonzero revision")
    mutated("bool-revision", lambda d: row(d).__setitem__("revision", False), "expected u64")
    mutated("large-revision", lambda d: row(d).__setitem__("revision", U64_MAX + 1), "expected u64")
    mutated("bool-expiry", lambda d: row(d).__setitem__("expires_at", True), "expected u64")
    mutated("large-expiry", lambda d: row(d).__setitem__("expires_at", U64_MAX + 1), "expected u64")
    mutated("negative-clock", lambda d: query(d).__setitem__("clock", -1), "expected u64")
    mutated("bool-clock", lambda d: query(d).__setitem__("clock", True), "expected u64")
    mutated("large-clock", lambda d: query(d).__setitem__("clock", U64_MAX + 1), "expected u64")
    mutated("bad-deleted", lambda d: row(d).__setitem__("deleted", 0), "deleted must be boolean")
    for label, value, diagnostic in [
        ("null-metadata", None, "metadata must be an object"),
        ("array-metadata", [], "metadata must be an object"),
        ("null-value", {"n": None}, "expected keys"),
        ("array-value", {"n": []}, "expected keys"),
        ("unknown-tag", {"n": {"type": "null", "value": None}}, "unsupported metadata type"),
        ("bool-integer", {"n": {"type": "integer", "value": True}}, "canonical decimal i64"),
        ("numeric-integer", {"n": {"type": "integer", "value": 1}}, "canonical decimal i64"),
        ("large-integer", {"n": {"type": "integer", "value": str(I64_MAX + 1)}}, "i64 out of range"),
        ("small-integer", {"n": {"type": "integer", "value": str(I64_MIN - 1)}}, "i64 out of range"),
        ("noncanonical-integer", {"n": {"type": "integer", "value": "-0"}}, "canonical decimal i64"),
        ("int-float", {"n": {"type": "float", "value": 1}}, "finite f64 JSON float"),
        ("int-bool", {"n": {"type": "boolean", "value": 1}}, "expected boolean"),
        ("bad-geo", {"n": {"type": "geopoint", "value": {"latitude": 91.0, "longitude": 0.0}}},
         "geo coordinates out of range"),
        ("empty-field", {"": {"type": "boolean", "value": True}}, "nonempty field"),
        ("long-field", {"x" * 257: {"type": "boolean", "value": True}}, "UTF-8 byte limit"),
        ("long-string", {"n": {"type": "string", "value": "x" * 4097}}, "UTF-8 byte limit"),
        ("too-many-fields", {str(i): {"type": "boolean", "value": True} for i in range(65)},
         "at most 64 fields"),
        ("too-many-bytes", {str(i): {"type": "string", "value": "x" * 4096} for i in range(16)},
         "encoded metadata byte limit"),
    ]:
        mutated(label, lambda d, value=value: row(d).__setitem__("metadata", value), diagnostic)
    for label, predicate, diagnostic in [
        ("unbounded-range", {"op": "range", "field": "n", "min": None, "max": None}, "numeric bound"),
        ("mixed-bounds", {"op": "range", "field": "n", "min": {"type": "integer", "value": "0"},
                          "max": {"type": "float", "value": 1.0}}, "same-type range"),
        ("reversed-bounds", {"op": "range", "field": "n", "min": {"type": "integer", "value": "2"},
                             "max": {"type": "integer", "value": "1"}}, "same-type range"),
        ("bool-range", {"op": "range", "field": "n", "min": {"type": "boolean", "value": True},
                       "max": None}, "numeric bound"),
        ("null-predicate", {"op": "isnull", "field": "n"}, "unsupported condition"),
        ("radius-final", {"op": "georadius", "field": "g", "latitude": 0.0,
                         "longitude": 0.0, "radius_meters": 1.0}, "pruning-only"),
        ("extra-predicate", {"op": "eq", "field": "n", "value": {"type": "integer", "value": "1"},
                            "extra": 0}, "expected keys"),
    ]:
        mutated(label, lambda d, predicate=predicate: query(d).__setitem__("conditions", [predicate]),
                diagnostic)
    mutated("condition-limit", lambda d: query(d).__setitem__(
        "conditions", query(d)["conditions"] * 65), "at most 64 conditions")
    mutated("row-order", lambda d: d["cases"][0]["segments"][0]["rows"].reverse(), "row ordering")
    mutated("duplicate-revision", lambda d: d["cases"][0]["segments"][0]["rows"].append(
        copy.deepcopy(row(d))), "duplicate ID/revision")
    mutated("unsorted-answer", lambda d: query(d)["expected"]["visible_ids"].reverse(), "strictly sorted")
    mutated("bool-answer-id", lambda d: query(d)["expected"].__setitem__("matching_ids", [True]),
            "expected u64")
    mutated("uncovered-summary", lambda d: d["cases"][0]["segments"][0]["summary"].__setitem__(
        "fields", {}), "does not cover physical row")
    mutated("bad-coverage", lambda d: d["cases"][0]["segments"][0]["summary"].__setitem__(
        "coverage", "corrupt"), "invalid coverage")
    mutated("missing-summary", lambda d: d["cases"][0]["segments"][0]["summary"].pop(
        "coverage"), "expected keys")
    mutated("extra-summary", lambda d: d["cases"][0]["segments"][0]["summary"].__setitem__(
        "extra", 0), "expected keys")
    mutated("reversed-summary", lambda d: d["cases"][0]["segments"][0]["summary"]["fields"]["n"][0]
            .update({"min": "0", "max": "-1"}), "summary min > max")
    mutated("duplicate-summary-type", lambda d: d["cases"][0]["segments"][0]["summary"]["fields"]["n"]
            .append({"type": "integer", "min": "0", "max": "1"}), "duplicate summary type")
    mutated("duplicate-dictionary", lambda d: d["cases"][0]["segments"][0]["summary"]["fields"]["n"][2]
            .__setitem__("values", [False, False, True]), "strictly sorted and unique")
    overlay_case = next(i for i, case in enumerate(document["cases"])
                        if case["segments"] and len(case["overlay"]) > 1)
    mutated("overlay-order", lambda d: d["cases"][overlay_case]["overlay"].reverse(),
            "row ordering violation")
    mutated("overlay-before-sealed", lambda d: d["cases"][overlay_case]["overlay"][0].__setitem__(
        "revision", 2), "newer than sealed revisions")
    mutated("duplicate-cross-segment-revision", lambda d: d["cases"][overlay_case]["segments"][1]
            ["rows"][0].__setitem__("revision", 1), "duplicate ID/revision")
    geo_case = next(i for i, case in enumerate(document["cases"]) if case["pruning_probes"])
    mutated("wrong-radius-pruning", lambda d: d["cases"][geo_case]["pruning_probes"][0]["expected"][0]
            .__setitem__("status", "prune"), "oracle")
    mutated("negative-radius", lambda d: d["cases"][geo_case]["pruning_probes"][0]["conditions"][0]
            .__setitem__("radius_meters", -1.0), "radius must be nonnegative")
    mutations.extend([
        ("duplicate-key", '{"schema":1,"schema":2}', "duplicate JSON key"),
        ("nested-duplicate", '{"metadata":{"n":1,"n":2}}', "duplicate JSON key"),
        ("nan", '{"n":NaN}', "invalid JSON constant"),
        ("infinity", '{"n":Infinity}', "invalid JSON constant"),
        ("negative-infinity", '{"n":-Infinity}', "invalid JSON constant"),
        ("overflow-float", '{"n":1e999}', "non-finite JSON number"),
        ("truncated", '{"schema":', "Expecting value"),
        ("root-array", "[]", "expected keys"),
        ("invalid-unicode", '{"schema":"\\ud800","version":1,"cases":[]}', "unsupported fixture schema"),
    ])
    with tempfile.TemporaryDirectory(prefix="vlite-filter-oracle-") as directory:
        for label, contents, diagnostic in mutations:
            broken = Path(directory) / f"{label}.json"
            broken.write_text(contents, encoding="utf-8")
            result = subprocess.run(
                [sys.executable, str(Path(__file__).resolve()), "--fixture", str(broken)],
                capture_output=True, text=True, timeout=30, check=False,
            )
            require(result.returncode == 1 and diagnostic in result.stderr,
                    f"self-test {label}: rejection failed: "
                    f"{result.returncode} {result.stdout} {result.stderr}")
    return len(mutations)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", type=Path, default=FIXTURE, help="override golden JSON path")
    parser.add_argument("--self-test", action="store_true", help="also reject temporary corrupt fixtures")
    args = parser.parse_args()
    try:
        cases, queries = validate_fixture(args.fixture, report=True)
        print(f"Verified {cases} filter/visibility cases and {queries} queries/probes: {args.fixture}")
        if args.self_test:
            print(f"Rejected {self_test(args.fixture)} malformed or incorrect fixtures")
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"{args.fixture}: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
