"""Independent stdlib-only reference for planned P4 compact graph semantics.

Run python tests\\compact_graph_oracle.py --self-test. Defaults use __file__.
Schema vlite.compact-graph-logical v1 is logical JSON, NOT a byte encoding.
Golden expected blocks are hand-authored; this tool never writes expectations.

Sources: docs/projects/roadmap/p4-compact-graph.md (immutable representation,
expiry); compact-storage-contract.md sections 2, 3, 5-7 (P4 ownership,
critical source data, accessor parity, TTL and determinism); docs/GRAPH.md:69-76.
Equality expires: src/property_graph/engine/state.rs:79-80, Expiries::is_due,
uses deadline <= now_ms. Graph::edge_expired at :366-370 cascades both endpoints.
Graph::adjacency at :305-329 and Side at :638-660 define order/filter/cursor;
Adjacency::next at :609-625 merges a self-loop once for Both.

Nodes: {id, labels, deadline?}; edges: {id, source, target, relationship,
deadline?}. Deadlines may be absent or null. IDs, deadlines, clock and limits
are u64 JSON integers, never floats/bools. Labels are unique UTF-8 strings.
Queries: {node, direction: out|in|both, relationship: [types]|null,
after: {relationship, edge}|null, limit}. Type arrays model a BTreeSet, including
an empty filter. Cursors need not name an existing edge or relationship.
Missing/expired query nodes yield an empty low-level adjacency list; this does
not model public GraphRef start-node errors, mutations, overlays or attachments.

Ordinals and dictionary codes are zero-based array indexes. node_map contains
ascending live public IDs and sorted label codes; edge_table contains ascending
live edge IDs and node-ordinal endpoints. Dictionaries contain only live values,
sorted by UTF-8 bytes, without Unicode normalization. CSR offsets have N+1
entries; neighbors are EDGE ordinals, not destination IDs or node ordinals.
Rows order memberships by (relationship UTF-8 bytes, public edge ID).
Out and in each contain every live edge exactly once, including self-loops.
Adjacency contains (relationship string, public edge ID) pairs per query,
exclusive after filtering and before limit. TTL uses one clock per case.
Canonical JSON output must match for listed edges and shuffles seeded 17, 941.
No codec, framing, byte sizes or performance claims are made.
"""

import argparse
import copy
import json
from pathlib import Path
import random
import subprocess
import sys
import tempfile


FIXTURE = Path(__file__).resolve().parent / "fixtures" / "compact" / "graph-csr-cases.json"
SCHEMA = "vlite.compact-graph-logical"
U64_MAX = 2**64 - 1
SHUFFLE_SEEDS = (17, 941)


def require(condition, message):
    if not condition:
        raise ValueError(message)


def keys(value, required, context, optional=""):
    wanted, allowed = set(required.split()), set(optional.split())
    require(isinstance(value, dict) and wanted <= set(value) <= wanted | allowed,
            f"{context}: expected keys {required} (optional: {optional})")


def u64(value, context):
    require(type(value) is int and 0 <= value <= U64_MAX,
            f"{context}: expected u64 integer")


def text(value, context):
    require(isinstance(value, str) and bool(value), f"{context}: expected nonempty UTF-8 string")
    try:
        value.encode("utf-8")
    except UnicodeEncodeError as error:
        raise ValueError(f"{context}: invalid UTF-8 string") from error


def strings(value, context):
    require(isinstance(value, list), f"{context}: expected string array")
    seen = set()
    for item in value:
        text(item, context)
        require(item not in seen, f"{context}: duplicate string")
        seen.add(item)


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, f"duplicate JSON key {key!r}")
        result[key] = value
    return result


def reject_constant(value):
    raise ValueError(f"invalid JSON constant {value!r}")


def reject_float(value):
    raise ValueError(f"non-integer JSON number {value!r}")


def load_fixture(path):
    return json.loads(path.read_text(encoding="utf-8"),
                      object_pairs_hook=unique_object,
                      parse_constant=reject_constant, parse_float=reject_float)


def validate_input(case):
    keys(case, "name purpose nodes edges clock queries expected", "case")
    text(case["name"], "case.name")
    text(case["purpose"], f"{case['name']}.purpose")
    context = case["name"]
    u64(case["clock"], f"{context}.clock")
    node_ids, edge_ids = set(), set()
    for field, seen in (("nodes", node_ids), ("edges", edge_ids)):
        require(isinstance(case[field], list), f"{context}.{field}: expected array")
        for index, row in enumerate(case[field]):
            location = f"{context}.{field}[{index}]"
            keys(row, "id labels" if field == "nodes" else "id source target relationship",
                 location, "deadline")
            u64(row["id"], f"{location}.id")
            require(row["id"] not in seen, f"{location}: duplicate {field} ID")
            seen.add(row["id"])
            if row.get("deadline") is not None:
                u64(row["deadline"], f"{location}.deadline")
            if field == "nodes":
                strings(row["labels"], f"{location}.labels")
            else:
                for endpoint in ("source", "target"):
                    u64(row[endpoint], f"{location}.{endpoint}")
                    require(row[endpoint] in node_ids, f"{location}: unknown endpoint")
                text(row["relationship"], f"{location}.relationship")
    require(isinstance(case["queries"], list) and case["queries"],
            f"{context}.queries: expected nonempty array")
    for index, query in enumerate(case["queries"]):
        location = f"{context}.queries[{index}]"
        keys(query, "node direction relationship after limit", location)
        u64(query["node"], f"{location}.node")
        require(isinstance(query["direction"], str) and query["direction"] in ("out", "in", "both"),
                f"{location}: invalid direction")
        if query["relationship"] is not None:
            strings(query["relationship"], f"{location}.relationship")
        if query["after"] is not None:
            keys(query["after"], "relationship edge", f"{location}.after")
            text(query["after"]["relationship"], f"{location}.after.relationship")
            u64(query["after"]["edge"], f"{location}.after.edge")
        u64(query["limit"], f"{location}.limit")


def live(row, clock):
    return row.get("deadline") is None or row["deadline"] > clock


def edge_key(edge):
    return edge["relationship"].encode("utf-8"), edge["id"]


def evaluate(case, edges=None):
    """Derive from records, independently of Rust and any prospective codec."""
    clock = case["clock"]
    nodes = sorted((node for node in case["nodes"] if live(node, clock)),
                   key=lambda node: node["id"])
    node_ordinals = {node["id"]: index for index, node in enumerate(nodes)}
    records = case["edges"] if edges is None else edges
    visible_edges = sorted(
        (edge for edge in records if live(edge, clock)
         and edge["source"] in node_ordinals and edge["target"] in node_ordinals),
        key=lambda edge: edge["id"])
    relationships = sorted({edge["relationship"] for edge in visible_edges}, key=str.encode)
    labels = sorted({label for node in nodes for label in node["labels"]}, key=str.encode)
    relationship_codes = {value: index for index, value in enumerate(relationships)}
    label_codes = {value: index for index, value in enumerate(labels)}
    edge_ordinals = {edge["id"]: index for index, edge in enumerate(visible_edges)}
    node_map = [{"id": node["id"], "ordinal": index,
                 "labels": sorted(label_codes[label] for label in node["labels"])}
                for index, node in enumerate(nodes)]
    edge_table = [{"id": edge["id"], "ordinal": index,
                   "source": node_ordinals[edge["source"]], "target": node_ordinals[edge["target"]],
                   "relationship": relationship_codes[edge["relationship"]]}
                  for index, edge in enumerate(visible_edges)]

    # Build CSR by grouped records, but answer queries by a separate direct scan.
    indexes = {}
    for direction, endpoint in (("out", "source"), ("in", "target")):
        rows = [[] for _ in nodes]
        for edge in visible_edges:
            rows[node_ordinals[edge[endpoint]]].append(edge)
        offsets, neighbors = [0], []
        for row in rows:
            neighbors.extend(edge_ordinals[edge["id"]] for edge in sorted(row, key=edge_key))
            offsets.append(len(neighbors))
        indexes[direction] = {"offsets": offsets, "neighbors": neighbors}

    adjacency = []
    for query in case["queries"]:
        cursor = query["after"]
        lower = None if cursor is None else (cursor["relationship"].encode("utf-8"), cursor["edge"])
        found = []
        for edge in visible_edges:
            outgoing = edge["source"] == query["node"]
            incoming = edge["target"] == query["node"]
            incident = (outgoing if query["direction"] == "out" else
                        incoming if query["direction"] == "in" else outgoing or incoming)
            if not incident:
                continue
            if query["relationship"] is not None and edge["relationship"] not in query["relationship"]:
                continue
            if lower is not None and edge_key(edge) <= lower:
                continue
            found.append(edge)
        adjacency.append([[edge["relationship"], edge["id"]]
                          for edge in sorted(found, key=edge_key)[:query["limit"]]])
    return {"node_map": node_map, "relationships": relationships, "labels": labels,
            "edge_table": edge_table, "out_csr": indexes["out"], "in_csr": indexes["in"],
            "adjacency": adjacency}


def exact(actual, expected, context):
    """Python bool/int equality must not let malformed expected blocks pass."""
    require(type(actual) is type(expected), f"{context}: oracle type mismatch")
    if isinstance(actual, dict):
        require(set(actual) == set(expected), f"{context}: oracle keys mismatch")
        for key in actual:
            exact(actual[key], expected[key], f"{context}.{key}")
    elif isinstance(actual, list):
        require(len(actual) == len(expected), f"{context}: oracle length mismatch")
        for index, (left, right) in enumerate(zip(actual, expected)):
            exact(left, right, f"{context}[{index}]")
    else:
        require(actual == expected, f"{context}: expected {expected!r}, oracle {actual!r}")


def canonical(value):
    return json.dumps(value, sort_keys=True, ensure_ascii=True, separators=(",", ":"), allow_nan=False)


def validate_fixture(path, report=False):
    document = load_fixture(path)
    keys(document, "schema version cases", "root")
    require(document["schema"] == SCHEMA, "schema: unsupported fixture schema")
    require(type(document["version"]) is int and document["version"] == 1,
            "version: expected integer 1")
    require(isinstance(document["cases"], list) and document["cases"],
            "cases: expected nonempty array")
    names, count = set(), 0
    for case in document["cases"]:
        validate_input(case)
        require(case["name"] not in names, "duplicate case name")
        names.add(case["name"])
        actual = evaluate(case)
        exact(actual, case["expected"], f"{case['name']}.expected")
        for seed in SHUFFLE_SEEDS:
            shuffled = list(case["edges"])
            random.Random(seed).shuffle(shuffled)
            require(canonical(evaluate(case, shuffled)) == canonical(actual),
                    f"{case['name']}: shuffle {seed} canonical output mismatch")
        count += len(case["queries"])
        if report:
            print(f"OK {case['name']}: nodes={len(actual['node_map'])} "
                  f"edges={len(actual['edge_table'])} queries={len(case['queries'])} shuffles=2")
    return len(names), count


def self_test(path):
    """Mutate hand-authored goldens and check real CLI diagnostics/exit status."""
    document = load_fixture(path)
    mutations = []

    def mutate(label, case_name, change, diagnostic="oracle"):
        value = copy.deepcopy(document)
        selected = next((case for case in value["cases"] if case["name"] == case_name), None)
        require(selected is not None, f"self-test requires case {case_name!r}")
        change(selected)
        mutations.append((label, json.dumps(value, ensure_ascii=True, allow_nan=False), diagnostic))

    mutate("wrong-node-ordinal", "single-node",
           lambda c: c["expected"]["node_map"][0].__setitem__("ordinal", 1))
    mutate("rounded-large-id", "large-ids",
           lambda c: c["expected"]["node_map"][1].__setitem__("id", 9007199254740992))
    mutate("reversed-node-map", "large-ids", lambda c: c["expected"]["node_map"].reverse())
    mutate("wrong-edge-ordinal", "parallel-same",
           lambda c: c["expected"]["edge_table"][0].__setitem__("ordinal", 1))
    mutate("wrong-source", "parallel-same",
           lambda c: c["expected"]["edge_table"][0].__setitem__("source", 1))
    mutate("wrong-target", "parallel-same",
           lambda c: c["expected"]["edge_table"][0].__setitem__("target", 0))
    mutate("relationship-dictionary-order", "unicode-relationships",
           lambda c: c["expected"]["relationships"].reverse())
    mutate("label-dictionary-order", "label-order", lambda c: c["expected"]["labels"].reverse())
    mutate("wrong-label-code", "label-order",
           lambda c: c["expected"]["node_map"][0].__setitem__("labels", [0]))
    mutate("duplicate-dictionary-value", "parallel-same",
           lambda c: c["expected"]["relationships"].append("r"))
    mutate("extra-unused-label", "single-node", lambda c: c["expected"]["labels"].append("ghost"))
    mutate("wrong-relationship-code", "parallel-different",
           lambda c: c["expected"]["edge_table"][0].__setitem__("relationship", 0))
    mutate("offset-start", "parallel-same",
           lambda c: c["expected"]["out_csr"]["offsets"].__setitem__(0, 1))
    mutate("offset-end", "parallel-same",
           lambda c: c["expected"]["in_csr"]["offsets"].__setitem__(2, 1))
    mutate("offset-missing-row", "single-node", lambda c: c["expected"]["out_csr"]["offsets"].pop())
    mutate("offset-decreasing", "parallel-same",
           lambda c: c["expected"]["out_csr"].__setitem__("offsets", [0, 2, 1]))
    mutate("unsorted-csr", "parallel-different",
           lambda c: c["expected"]["out_csr"]["neighbors"].reverse())
    mutate("missing-in-membership", "self-loops",
           lambda c: c["expected"]["in_csr"]["neighbors"].pop())
    mutate("duplicate-out-membership", "self-loops",
           lambda c: c["expected"]["out_csr"]["neighbors"].append(0))
    mutate("unsorted-adjacency", "parallel-different",
           lambda c: c["expected"]["adjacency"][0].reverse())
    mutate("collapsed-parallel-edge", "parallel-same",
           lambda c: c["expected"]["adjacency"][0].pop())
    mutate("duplicated-self-loop", "self-loops",
           lambda c: c["expected"]["adjacency"][2].append(["r", 8]))
    mutate("visible-expired-edge", "expired-edge",
           lambda c: c["expected"]["adjacency"][0].append(["r", 2]))
    mutate("visible-node-cascade", "expired-node",
           lambda c: c["expected"]["adjacency"][0].append(["r", 1]))
    mutate("visible-equal-deadline", "deadline-equals-clock",
           lambda c: c["expected"]["adjacency"][0].append(["r", 3]))
    mutate("inclusive-cursor", "paging",
           lambda c: c["expected"]["adjacency"][1].insert(0, ["a", 20]))
    mutate("numeric-only-cursor", "paging",
           lambda c: c["expected"]["adjacency"].__setitem__(1, []))
    mutate("cursor-type-not-in-filter", "paging",
           lambda c: c["expected"]["adjacency"][3].insert(0, ["a", 20]))
    mutate("ignored-limit", "paging",
           lambda c: c["expected"]["adjacency"][0].append(["a", 20]))
    mutate("ignored-empty-filter", "filters",
           lambda c: c["expected"]["adjacency"][2].append(["a", 2]))
    mutate("wrong-direction", "self-loops",
           lambda c: c["expected"]["adjacency"][0].append(["s", 2]))
    mutate("bool-expected-ordinal", "single-node",
           lambda c: c["expected"]["node_map"][0].__setitem__("ordinal", False))
    mutate("extra-expected-field", "empty",
           lambda c: c["expected"].__setitem__("bytes", []))
    mutate("duplicate-node-id", "parallel-same", lambda c: c["nodes"].append(c["nodes"][0]),
           "duplicate nodes ID")
    mutate("duplicate-edge-id", "parallel-same", lambda c: c["edges"].append(c["edges"][0]),
           "duplicate edges ID")
    for endpoint in ("source", "target"):
        mutate(f"unknown-{endpoint}", "parallel-same",
               lambda c, endpoint=endpoint: c["edges"][0].__setitem__(endpoint, 999),
               "unknown endpoint")
    for label, value in (("bool-id", True), ("negative-id", -1), ("overflow-id", 2**64),
                         ("string-id", "42")):
        mutate(label, "single-node", lambda c, value=value: c["nodes"][0].__setitem__("id", value),
               "expected u64")
    mutate("float-id", "single-node",
           lambda c: c["nodes"][0].__setitem__("id", 42.0), "non-integer JSON number")
    mutate("bad-deadline", "single-node",
           lambda c: c["nodes"][0].__setitem__("deadline", True), "expected u64")
    mutate("bad-edge-deadline", "parallel-same",
           lambda c: c["edges"][0].__setitem__("deadline", -1), "expected u64")
    mutate("bad-clock", "single-node", lambda c: c.__setitem__("clock", False), "expected u64")
    mutate("duplicate-label", "single-node",
           lambda c: c["nodes"][0].__setitem__("labels", ["A", "A"]), "duplicate string")
    mutate("labels-not-array", "single-node",
           lambda c: c["nodes"][0].__setitem__("labels", "A"), "expected string array")
    mutate("bad-utf8", "single-node",
           lambda c: c["nodes"][0].__setitem__("labels", ["\ud800"]), "invalid UTF-8")
    mutate("missing-edge-field", "parallel-same",
           lambda c: c["edges"][0].pop("target"), "expected keys")
    mutate("extra-node-field", "single-node",
           lambda c: c["nodes"][0].__setitem__("ordinal", 0), "expected keys")
    mutate("bad-direction", "single-node",
           lambda c: c["queries"][0].__setitem__("direction", "sideways"), "invalid direction")
    mutate("bad-filter", "filters",
           lambda c: c["queries"][0].__setitem__("relationship", "a"), "expected string array")
    mutate("duplicate-filter", "filters",
           lambda c: c["queries"][0].__setitem__("relationship", ["a", "a"]), "duplicate string")
    mutate("bad-cursor-id", "paging",
           lambda c: c["queries"][1]["after"].__setitem__("edge", True), "expected u64")
    mutate("bad-cursor-shape", "paging",
           lambda c: c["queries"][1].__setitem__("after", ["a", 20]), "expected keys")
    mutate("bad-cursor-kind", "paging",
           lambda c: c["queries"][1]["after"].__setitem__("relationship", 1), "expected nonempty")
    mutate("bad-limit", "single-node",
           lambda c: c["queries"][0].__setitem__("limit", -1), "expected u64")
    mutations.extend([
        ("duplicate-json-key", '{"schema":1,"schema":2}', "duplicate JSON key"),
        ("nested-duplicate-json-key", '{"a":{"id":1,"id":2}}', "duplicate JSON key"),
        ("nan", '{"clock":NaN}', "invalid JSON constant"),
        ("infinity", '{"clock":Infinity}', "invalid JSON constant"),
        ("truncated-json", '{"schema":', "Expecting value"),
        ("root-array", "[]", "expected keys"),
    ])
    require(len(mutations) >= 30, "self-test requires at least 30 negative mutations")
    with tempfile.TemporaryDirectory(prefix="vlite-graph-oracle-",
                                     dir=Path(__file__).resolve().parent) as directory:
        for label, contents, diagnostic in mutations:
            broken = Path(directory) / f"{label}.json"
            broken.write_text(contents, encoding="utf-8")
            result = subprocess.run(
                [sys.executable, str(Path(__file__).resolve()), "--fixture", str(broken)],
                capture_output=True, text=True, timeout=30, check=False)
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
        print(f"Verified {cases} compact graph cases and {queries} queries: {args.fixture}")
        if args.self_test:
            print(f"Rejected {self_test(args.fixture)} malformed or incorrect fixtures")
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"{args.fixture}: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
