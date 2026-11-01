"""Independent stdlib-only logical reference for P4 typed property snapshots.

Run python -B tests\\compact_graph_property_oracle.py --self-test.
This is NOT a serializer, VLGCSR01, an on-disk layout, a full graph write
admission model, Rust parity proof, or storage/performance acceptance.

Source semantics at base 4f43da9b93024d5932627788064d04f0a8735caa:
src/property_graph/engine/model.rs:14-29 defines u64 node/edge IDs and exactly
String, Integer(i64), Float(f64), Boolean, GeoPoint{latitude, longitude}.
src/property_graph/engine/property_map.rs:1-6,77-86,146-158 defines UTF-8 key
ordering and last-insert-wins. properties.rs:19-81 validates property names,
values, field count and existing aggregate size; :153-155 rejects unsorted or
duplicate canonical names. limits.rs:24-30 supplies 64 fields, 65536 aggregate
bytes, 1..256 name bytes and at most 4096 string bytes. Names are property
names, NOT node/edge keys or labels; no identifier grammar is imposed on them.

Logical interchange schema "vlite.compact-graph-properties", version 1:
root = {schema, version, cases, invalid_cases}, both case lists nonempty.
Valid case = {name, purpose, input, expected}; malformed case =
{name, input, error}, where error is a required rejection diagnostic substring.
input = {mode: "insertions"|"snapshot", nodes: [row], edges: [row]}.
row = {id: u64 JSON integer, properties: [typed entry]}. IDs are unique within
each entity kind, but may be shared across kinds. All u64 bit patterns are
interchange IDs; allocator high-water marks and exhaustion are NOT modeled.
entry = {name, type, value} for String, Integer, Boolean; {name, type, bits}
for Float; {name, type, latitude_bits, longitude_bits} for GeoPoint.
Type tags are the five exact case-sensitive Rust variant names. Integer values
are exact JSON integers in i64 range, never floats or booleans. Boolean values
must be JSON true/false. String values may be empty. UTF-8 strings/names are
not normalized; unpaired surrogates are rejected. Bits are exactly 16 lowercase
hex digits representing an IEEE-754 f64 bit pattern, without prefix or numeric
conversion in snapshots. All finite patterns, including both zeros and
subnormals, are retained verbatim; coordinates also require inclusive
latitude [-90,90], longitude [-180,180].

Insertion property lists permit repeats with last-wins per name. Each listed
entry must be valid; final maps obey count/size limits (no intermediate-map
admission checks). Snapshot property lists must already be strictly sorted by
UTF-8 bytes and unique. Input entity rows may be arbitrarily ordered in either
mode. expected = {nodes, edges}, with rows strictly increasing by ID and
properties strictly sorted/unique. Canonical output is the same shape.
Unknown or missing fields, nulls, arrays/objects as values, unknown tags and
floating-point JSON numbers are invalid everywhere. Duplicate JSON object
keys are rejected before schema validation.

Aggregate size uses ONLY the existing properties::validate accounting:
4 + sum(2 + UTF-8 name bytes + 1 + value charge), charges String=4+UTF-8 bytes,
Integer/Float=8, Boolean=1, GeoPoint=16. This enforces the existing 65536 limit;
it does not specify or emit the private codec's byte format or sizes.
No overlay, TTL, graph topology, schema, attachments, revisions, temporal,
batch/read budgets or graph/node/edge-key validation are modeled.
Goldens are hand-authored independently, never produced or rewritten here.
Self-test checks invalid fixture mutations through the CLI with exact counts,
limit boundaries and seeded shuffles. Shuffles preserve same-name insertion
chronology and never reorder canonical snapshot properties.
"""

import argparse
import copy
import json
import math
from pathlib import Path
import random
import re
import struct
import subprocess
import sys
import tempfile


FIXTURE = Path(__file__).resolve().parent / "fixtures" / "compact" / "graph-property-cases.json"
SCHEMA = "vlite.compact-graph-properties"
SHUFFLE_SEEDS = (17, 941)


def require(condition, message):
    if not condition:
        raise ValueError(message)


def keys(value, fields, context):
    require(type(value) is dict and set(value) == set(fields.split()),
            f"{context}: expected keys {fields}")


def integer(value, low, high, context):
    require(type(value) is int and low <= value <= high,
            f"{context}: expected integer in [{low}, {high}]")


def utf8(value, context, low=0, high=None):
    require(type(value) is str, f"{context}: expected UTF-8 string")
    try:
        encoded = value.encode("utf-8")
    except UnicodeEncodeError as error:
        raise ValueError(f"{context}: invalid UTF-8 string") from error
    require(len(encoded) >= low and (high is None or len(encoded) <= high),
            f"{context}: UTF-8 byte length outside [{low}, {high}]")
    return encoded


def float_bits(value, context):
    require(type(value) is str and re.fullmatch("[0-9a-f]{16}", value) is not None,
            f"{context}: expected 16 lowercase hex digits")
    number = struct.unpack(">d", bytes.fromhex(value))[0]
    require(math.isfinite(number), f"{context}: float must be finite")
    return number


def typed_entry(entry, context):
    require(type(entry) is dict, f"{context}: expected typed entry object")
    tag = entry.get("type")
    require(type(tag) is str and tag in
            ("String", "Integer", "Float", "Boolean", "GeoPoint"),
            f"{context}: unknown property type")
    fields = {"Float": "name type bits",
              "GeoPoint": "name type latitude_bits longitude_bits"}
    keys(entry, fields.get(tag, "name type value"), context)
    name_bytes = utf8(entry["name"], f"{context}.name", 1, 256)
    if tag == "String":
        charge = 4 + len(utf8(entry["value"], f"{context}.value", 0, 4096))
    elif tag == "Integer":
        integer(entry["value"], -(2**63), 2**63 - 1, f"{context}.value")
        charge = 8
    elif tag == "Boolean":
        require(type(entry["value"]) is bool, f"{context}.value: expected boolean")
        charge = 1
    elif tag == "Float":
        float_bits(entry["bits"], f"{context}.bits")
        charge = 8
    else:
        for field, bound in (("latitude_bits", 90), ("longitude_bits", 180)):
            number = float_bits(entry[field], f"{context}.{field}")
            require(-bound <= number <= bound, f"{context}.{field}: coordinate out of range")
        charge = 16
    return name_bytes, 2 + len(name_bytes) + 1 + charge


def property_snapshot(entries, context, canonical):
    require(type(entries) is list, f"{context}: expected property array")
    by_name = {}
    previous = None
    for index, entry in enumerate(entries):
        name, charge = typed_entry(entry, f"{context}[{index}]")
        if canonical:
            require(previous is None or previous < name,
                    f"{context}[{index}]: canonical names must be strictly sorted and unique")
        previous = name
        by_name[name] = (entry, charge)
    require(len(by_name) <= 64, f"{context}: properties exceed 64 fields")
    require(4 + sum(charge for _, charge in by_name.values()) <= 65536,
            f"{context}: aggregate properties exceed 65536 bytes")
    return [copy.deepcopy(by_name[name][0]) for name in sorted(by_name)]


def entity_snapshot(rows, context, canonical_properties, canonical_rows=False):
    require(type(rows) is list, f"{context}: expected entity array")
    result, seen, previous = [], set(), None
    for index, row in enumerate(rows):
        location = f"{context}[{index}]"
        keys(row, "id properties", location)
        integer(row["id"], 0, 2**64 - 1, f"{location}.id")
        require(row["id"] not in seen, f"{location}: duplicate entity ID")
        if canonical_rows:
            require(previous is None or previous < row["id"],
                    f"{location}: canonical IDs must be strictly increasing")
        previous = row["id"]
        seen.add(row["id"])
        result.append({"id": row["id"], "properties": property_snapshot(
            row["properties"], f"{location}.properties", canonical_properties)})
    return sorted(result, key=lambda row: row["id"])


def snapshot(value, context="input"):
    keys(value, "mode nodes edges", context)
    require(type(value["mode"]) is str and value["mode"] in ("insertions", "snapshot"),
            f"{context}.mode: expected insertions or snapshot")
    return {field: entity_snapshot(value[field], f"{context}.{field}",
                                   value["mode"] == "snapshot")
            for field in ("nodes", "edges")}


def canonical_expected(value, context):
    keys(value, "nodes edges", context)
    return {field: entity_snapshot(value[field], f"{context}.{field}", True, True)
            for field in ("nodes", "edges")}


def canonical_json(value):
    return json.dumps(value, ensure_ascii=True, sort_keys=True, separators=(",", ":"))


def unique_object(pairs):
    result = {}
    for name, value in pairs:
        require(name not in result, f"duplicate JSON key {name!r}")
        result[name] = value
    return result


def reject_constant(value):
    raise ValueError(f"invalid JSON constant {value!r}")


def reject_float(value):
    raise ValueError(f"non-integer JSON number {value!r}")


def load_fixture(path):
    return json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=unique_object,
                      parse_constant=reject_constant, parse_float=reject_float)


def expect_rejection(value, diagnostic, context):
    try:
        snapshot(value, context)
    except ValueError as error:
        require(diagnostic in str(error),
                f"{context}: expected rejection {diagnostic!r}, got {str(error)!r}")
    else:
        raise ValueError(f"{context}: malformed input was accepted")


def shuffled_input(value, seed):
    result = copy.deepcopy(value)
    rng = random.Random(seed)
    for field in ("nodes", "edges"):
        rng.shuffle(result[field])
        if result["mode"] == "insertions":
            for row in result[field]:
                groups = {}
                for entry in row["properties"]:
                    groups.setdefault(entry["name"], []).append(entry)
                ordered = list(groups.values())
                rng.shuffle(ordered)
                row["properties"] = [entry for group in ordered for entry in group]
    return result


def validate_fixture(path, report=False):
    document = load_fixture(path)
    keys(document, "schema version cases invalid_cases", "fixture")
    require(document["schema"] == SCHEMA, "unsupported fixture schema")
    require(type(document["version"]) is int and document["version"] == 1,
            "version: expected integer 1")
    names, outputs = set(), []
    for field in ("cases", "invalid_cases"):
        require(type(document[field]) is list and document[field],
                f"{field}: expected nonempty array")
        for index, case in enumerate(document[field]):
            context = f"{field}[{index}]"
            keys(case, "name purpose input expected" if field == "cases" else
                 "name input error", context)
            utf8(case["name"], f"{context}.name", 1)
            require(case["name"] not in names, f"{context}: duplicate case name")
            names.add(case["name"])
            context = case["name"]
            if field == "invalid_cases":
                utf8(case["error"], f"{context}.error", 1)
                expect_rejection(case["input"], case["error"], context)
                continue
            utf8(case["purpose"], f"{context}.purpose", 1)
            expected = canonical_expected(case["expected"], f"{context}.expected")
            result = snapshot(case["input"], context)
            require(canonical_json(result) == canonical_json(expected),
                    f"{context}: typed snapshot differs from fixed golden")
            for seed in SHUFFLE_SEEDS:
                shuffled = snapshot(shuffled_input(case["input"], seed), context)
                require(canonical_json(shuffled) == canonical_json(expected),
                        f"{context}: shuffle {seed} differs from fixed golden")
            outputs.append({"name": case["name"], "snapshot": result})
            if report:
                print(f"Verified {context}")
    return document, outputs


def self_test(path):
    document, _ = validate_fixture(path)
    require([case["name"] for case in document["cases"]] ==
            ["empty-maps", "mixed-node-and-edge", "exact-ieee-bits"],
            "self-test requires the three bundled golden cases in their fixture order")
    mutations = []

    def mutate(label, change, diagnostic):
        broken = copy.deepcopy(document)
        change(broken)
        mutations.append((label, json.dumps(broken), diagnostic))

    def entry_change(label, change, diagnostic, field="nodes"):
        mutate(label, lambda d: change(d["cases"][1]["input"][field][0]["properties"][0]),
               diagnostic)

    for tag in ("Null", "Array", "Object", "float", "", None, 5, [], {}):
        entry_change(f"unknown-tag-{len(mutations)}", lambda e, tag=tag:
                     e.__setitem__("type", tag), "unknown property type")
    for value in (True, False, -(2**63) - 1, 2**63, "1", None, [], {}):
        entry_change(f"bad-integer-{len(mutations)}", lambda e, value=value:
                     (e.clear(), e.update(name="x", type="Integer", value=value)),
                     "expected integer")
    for value in (0, 1, "false", None, [], {}):
        entry_change(f"bad-boolean-{len(mutations)}", lambda e, value=value:
                     (e.clear(), e.update(name="x", type="Boolean", value=value)),
                     "expected boolean", "edges")
    for bits in ("0", "00000000000000000", "0x0000000000000000", "3FF0000000000000",
                 "gggggggggggggggg", 0, True, None, [], {}):
        entry_change(f"bad-bits-{len(mutations)}", lambda e, bits=bits:
                     (e.clear(), e.update(name="x", type="Float", bits=bits)),
                     "expected 16 lowercase hex digits")
    for bits in ("7ff0000000000000", "fff0000000000000", "7ff8000000000000",
                 "7ff0000000000001", "fff8000000000001"):
        for tag in ("Float", "GeoPoint"):
            entry_change(f"nonfinite-{tag}-{bits}", lambda e, tag=tag, bits=bits:
                         (e.clear(), e.update({"name": "x", "type": tag,
                          **({"bits": bits} if tag == "Float" else
                             {"latitude_bits": bits, "longitude_bits": "0000000000000000"})})),
                         "float must be finite")
    for field, bits in (("latitude_bits", "4056800000000001"),
                        ("latitude_bits", "c056800000000001"),
                        ("longitude_bits", "4066800000000001"),
                        ("longitude_bits", "c066800000000001"),
                        ("longitude_bits", "7ff0000000000000")):
        entry_change(f"invalid-geo-{field}-{bits}", lambda e, field=field, bits=bits:
                     (e.clear(), e.update(name="x", type="GeoPoint",
                      latitude_bits="0000000000000000", longitude_bits="0000000000000000"),
                      e.__setitem__(field, bits)),
                     "float must be finite" if bits == "7ff0000000000000"
                     else "coordinate out of range")
    for value, diagnostic in (("", "byte length"), ("x" * 257, "byte length"),
                              ("\u00e9" * 129, "byte length"), ("\ud800", "invalid UTF-8"),
                              (1, "expected UTF-8"), (None, "expected UTF-8")):
        entry_change(f"bad-name-{len(mutations)}", lambda e, value=value:
                     e.__setitem__("name", value), diagnostic)
    for value, diagnostic in (("x" * 4097, "byte length"), ("\u00e9" * 2049, "byte length"),
                              ("\udfff", "invalid UTF-8"), (False, "expected UTF-8"),
                              (None, "expected UTF-8"), ([], "expected UTF-8"),
                              ({}, "expected UTF-8")):
        entry_change(f"bad-string-{len(mutations)}", lambda e, value=value:
                     (e.clear(), e.update(name="x", type="String", value=value)), diagnostic)
    for field in ("nodes", "edges"):
        for value in (True, False, -1, 2**64, "1", None, [], {}):
            mutate(f"{field}-bad-id-{len(mutations)}", lambda d, field=field, value=value:
                   d["cases"][1]["input"][field][0].__setitem__("id", value),
                   "expected integer")
        mutate(f"{field}-duplicate-id", lambda d, field=field:
               d["cases"][1]["input"][field].append(
                   copy.deepcopy(d["cases"][1]["input"][field][0])), "duplicate entity ID")
        for value in (None, {}, 3):
            mutate(f"{field}-bad-rows-{len(mutations)}", lambda d, field=field, value=value:
                   d["cases"][1]["input"].__setitem__(field, value), "expected entity array")
        mutate(f"{field}-bad-properties", lambda d, field=field:
               d["cases"][1]["input"][field][0].__setitem__("properties", {}), "property array")
        mutate(f"{field}-bad-row", lambda d, field=field:
               d["cases"][1]["input"][field].__setitem__(0, None), "expected keys")
        mutate(f"{field}-duplicate-canonical-name", lambda d, field=field:
               d["cases"][1]["expected"][field][0]["properties"].append(
                   copy.deepcopy(d["cases"][1]["expected"][field][0]["properties"][-1])),
               "strictly sorted and unique")
        mutate(f"{field}-unsorted-canonical-name", lambda d, field=field:
               d["cases"][1]["expected"][field][0]["properties"].reverse(),
               "strictly sorted and unique")
        mutate(f"{field}-unsorted-canonical-id", lambda d, field=field:
               d["cases"][1]["expected"][field].reverse(), "strictly increasing")
    for tag, fields in (("String", "name type value"), ("Integer", "name type value"),
                        ("Boolean", "name type value"), ("Float", "name type bits"),
                        ("GeoPoint", "name type latitude_bits longitude_bits")):
        prototype = {"name": "x", "type": tag, **(
            {"bits": "0000000000000000"} if tag == "Float" else
            {"latitude_bits": "0000000000000000", "longitude_bits": "0000000000000000"}
            if tag == "GeoPoint" else {"value": "" if tag == "String" else
                                     False if tag == "Boolean" else 0})}
        for missing in fields.split():
            entry_change(f"{tag}-missing-{missing}", lambda e, p=prototype, missing=missing:
                         (e.clear(), e.update(p), e.pop(missing)),
                         "unknown property type" if missing == "type" else "expected keys")
        entry_change(f"{tag}-extra-field", lambda e, p=prototype:
                     (e.clear(), e.update(p), e.update(extra=0)), "expected keys")
    mutate("entry-not-object", lambda d:
           d["cases"][1]["input"]["nodes"][0]["properties"].__setitem__(0, None),
           "expected typed entry object")
    for key, value, diagnostic in (
        ("schema", "unknown", "unsupported fixture schema"),
        ("version", True, "expected integer 1"), ("version", 2, "expected integer 1"),
        ("cases", [], "expected nonempty array"), ("cases", {}, "expected nonempty array"),
        ("invalid_cases", [], "expected nonempty array"),
    ):
        mutate(f"root-{key}-{len(mutations)}", lambda d, key=key, value=value:
               d.__setitem__(key, value), diagnostic)
    mutate("extra-root", lambda d: d.update(extra=0), "expected keys")
    mutate("extra-input", lambda d: d["cases"][1]["input"].update(ttl=0), "expected keys")
    mutate("invalid-mode", lambda d: d["cases"][1]["input"].update(mode=[]), "expected insertions")
    mutate("duplicate-case-name", lambda d:
           d["cases"][1].update(name=d["cases"][0]["name"]), "duplicate case name")
    mutate("wrong-golden", lambda d:
           d["cases"][1]["expected"]["nodes"][0]["properties"][3].update(value=True),
           "differs from fixed golden")
    mutate("golden-zero-sign-loss", lambda d:
           d["cases"][2]["expected"]["nodes"][0]["properties"][1].update(bits="0000000000000000"),
           "differs from fixed golden")
    mutate("false-invalid-case", lambda d:
           d["invalid_cases"][0].update(input={"mode": "snapshot", "nodes": [], "edges": []}),
           "malformed input was accepted")
    for label, contents, diagnostic in (
        ("duplicate-json-key", '{"schema":1,"schema":2}', "duplicate JSON key"),
        ("nested-duplicate-json-key", '{"row":{"bits":0,"bits":1}}', "duplicate JSON key"),
        ("json-float", '{"version":1.0}', "non-integer JSON number"),
        ("integer-exponent", '{"version":1e0}', "non-integer JSON number"),
        ("nan", '{"value":NaN}', "invalid JSON constant"),
        ("infinity", '{"value":Infinity}', "invalid JSON constant"),
        ("truncated-json", '{"schema":', "Expecting value"),
        ("root-array", "[]", "expected keys"),
    ):
        mutations.append((label, contents, diagnostic))

    def map_input(entries):
        return {"mode": "insertions", "nodes": [{"id": 0, "properties": entries}], "edges": []}

    boundaries = [
        {"name": "\u00e9" * 128, "type": "String", "value": "\u00e9" * 2048},
    ]
    snapshot(map_input(boundaries), "exact UTF-8 name/string limits")
    fields = [{"name": f"f{i:02}", "type": "Integer", "value": i} for i in range(64)]
    snapshot(map_input(fields), "exact field count")
    oversize_fields = fields + [{"name": "z", "type": "Boolean", "value": False}]
    size_entries = [{"name": f"s{i:02}", "type": "String", "value": "v" * 4096}
                    for i in range(15)]
    # Hand-computed existing validator charge: 4 + 15*(10+4096) + 10 + 3932 = 65536.
    size_entries.append({"name": "s15", "type": "String", "value": "v" * 3932})
    snapshot(map_input(size_entries), "exact aggregate limit")
    size_entries[-1]["value"] += "v"
    for label, entries, diagnostic in (
        ("field-count-limit", oversize_fields, "properties exceed 64 fields"),
        ("aggregate-size-limit", size_entries, "aggregate properties exceed 65536 bytes"),
    ):
        mutate(label, lambda d, entries=entries:
               d["cases"][0].update(input=map_input(entries)), diagnostic)
    # Limits apply after last-wins: many insertions may collapse to one tiny field.
    repeated = [{"name": "x", "type": "String", "value": "v" * 4096}] * 70
    repeated += [{"name": "x", "type": "Boolean", "value": False}]
    require(snapshot(map_input(repeated))["nodes"][0]["properties"] ==
            [{"name": "x", "type": "Boolean", "value": False}], "last-wins limit regression")
    require(len(mutations) == 140, f"self-test mutation accounting changed: {len(mutations)}")
    require(len({label for label, _, _ in mutations}) == len(mutations),
            "self-test duplicate mutation label")
    with tempfile.TemporaryDirectory(prefix="vlite-graph-property-oracle-",
                                     dir=Path(__file__).resolve().parent) as directory:
        for label, contents, diagnostic in mutations:
            bad = Path(directory) / f"{label}.json"
            bad.write_text(contents, encoding="utf-8")
            result = subprocess.run(
                [sys.executable, "-B", str(Path(__file__).resolve()), "--fixture", str(bad)],
                capture_output=True, text=True, timeout=30, check=False)
            require(result.returncode == 1 and diagnostic in result.stderr,
                    f"self-test {label}: rejection failed: "
                    f"{result.returncode} {result.stdout} {result.stderr}")
    return len(mutations)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", type=Path, default=FIXTURE, help="override golden JSON path")
    parser.add_argument("--self-test", action="store_true", help="also reject mutated fixtures")
    parser.add_argument("--snapshot", action="store_true", help="print canonical logical snapshots")
    args = parser.parse_args()
    try:
        document, outputs = validate_fixture(args.fixture, report=not args.snapshot)
        if args.snapshot:
            print(canonical_json(outputs))
        else:
            print(f"Verified {len(outputs)} property cases, "
                  f"{len(document['invalid_cases'])} malformed cases and "
                  f"{len(outputs) * len(SHUFFLE_SEEDS)} deterministic shuffles: {args.fixture}")
        if args.self_test:
            print(f"Rejected {self_test(args.fixture)} mutated invalid fixtures")
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"{args.fixture}: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
