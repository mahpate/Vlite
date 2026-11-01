"""Independent Python 3 stdlib logical reference for private P4 label postings.

Run python -B tests\\compact_graph_label_oracle.py --self-test.
Default fixture paths are relative to __file__, not the working directory.
Schema vlite.compact-graph-label-logical v1 is NOT a byte format or public API.
Hand-computed expectations are read-only; no size or speed claims are made.

Sources: docs/projects/roadmap/p4-compact-graph.md (dictionary-encoded labels,
sorted label sets and postings); src/property_graph/engine/model.rs
(validate_label, validate_labels); limits.rs (8 labels, 63 ASCII bytes);
state.rs (labels index, nodes_with_label, exclusive label_scan); filter.rs
(every requested label must match). Expiry, properties, edges, keys and overlay
mutations are out of scope. Engine read paths filter expiry elsewhere.

Nodes are {id: u64 JSON integer, labels: [string]}; IDs include 0 and u64::MAX.
Labels match [A-Za-z0-9_]{1,63}. Duplicates collapse before the eight-distinct
label bound; input order is irrelevant. Empty node label sets are valid.
The dictionary is sorted by UTF-8 bytes, with zero-based codes. node_map follows
compact_graph_oracle.py: ascending public IDs, zero-based ordinals and ascending
label code sets. Postings list {label, code, ids} in dictionary order; each ids
array is ascending. Unlabeled nodes appear in node_map but never in postings.

Queries are {op: "scan", label, after?} or {op: "all", labels, after?, limit?}.
Missing/null after is unbounded; otherwise IDs must be strictly above its u64
value. Unknown valid labels yield []. all requires 1..8 distinct valid labels
(unlike the engine's empty default NodeFilter) and carries EVERY label.
An omitted limit is unbounded; a supplied limit must be an integer >= 1. It is
an abstract reference-only count with no engine width (u32/usize) upper bound.
Unknown fields and non-integer/bool IDs, cursors or limits reject explicitly.
results contains one ascending ID array per query, in query order.

--self-test verifies goldens, exact malformed reasons, five seeded node/label/
query-label order shuffles per golden, and deterministic input and expectation
mutations. These are test-harness mutations, not graph overlay semantics.
"""

import argparse
import copy
import json
from pathlib import Path
import random
import re
import sys


FIXTURE = Path(__file__).resolve().parent / "fixtures" / "compact" / "graph-label-cases.json"
SCHEMA = "vlite.compact-graph-label-logical"
U64_MAX = 2**64 - 1
SHUFFLE_SEEDS = (17, 941, 2026, 65537, 104729)
LABEL = re.compile(r"[A-Za-z0-9_]{1,63}")


def require(condition, message):
    if not condition:
        raise ValueError(message)


def keys(value, required, context, optional=""):
    require(isinstance(value, dict), f"{context}: expected object")
    wanted, allowed = set(required.split()), set(optional.split())
    require(set(value) <= wanted | allowed, f"{context}: unknown field")
    require(wanted <= set(value), f"{context}: missing field")


def u64(value, context):
    require(type(value) is int and 0 <= value <= U64_MAX,
            f"{context}: expected u64 integer")


def label(value):
    require(isinstance(value, str) and LABEL.fullmatch(value) is not None,
            "label: invalid ASCII identifier")


def labels(value, query=False):
    require(isinstance(value, list), "labels: expected array")
    require(all(isinstance(item, str) for item in value), "label: invalid ASCII identifier")
    distinct = set(value)
    if query:
        require(1 <= len(distinct) <= 8, "query labels: require 1..8 distinct labels")
    else:
        require(len(distinct) <= 8, "labels: at most 8 distinct labels")
    for item in sorted(distinct):
        label(item)
    return distinct


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, f"duplicate JSON key {key!r}")
        result[key] = value
    return result


def reject_constant(value):
    raise ValueError(f"invalid JSON constant {value!r}")


def load_fixture(path):
    return json.loads(path.read_text(encoding="utf-8"),
                      object_pairs_hook=unique_object, parse_constant=reject_constant)


def validate_input(value):
    keys(value, "nodes queries", "input")
    require(isinstance(value["nodes"], list), "nodes: expected array")
    seen = set()
    for node in value["nodes"]:
        keys(node, "id labels", "node")
        u64(node["id"], "node id")
        require(node["id"] not in seen, "duplicate node ID")
        seen.add(node["id"])
        labels(node["labels"])
    require(isinstance(value["queries"], list) and value["queries"],
            "queries: expected nonempty array")
    for query in value["queries"]:
        require(isinstance(query, dict), "query: expected object")
        require(isinstance(query.get("op"), str) and query["op"] in ("scan", "all"),
                "query: invalid operation")
        if query["op"] == "scan":
            keys(query, "op label", "query", "after")
            label(query["label"])
        else:
            keys(query, "op labels", "query", "after limit")
            labels(query["labels"], query=True)
            if "limit" in query:
                require(type(query["limit"]) is int and query["limit"] >= 1,
                        "limit: expected positive integer")
        if query.get("after") is not None:
            u64(query["after"], "after")


def evaluate(value):
    """Derive indexes from sets; answer queries independently by record scans."""
    validate_input(value)
    nodes = sorted(value["nodes"], key=lambda node: node["id"])
    node_labels = {node["id"]: set(node["labels"]) for node in nodes}
    dictionary = sorted({item for items in node_labels.values() for item in items},
                        key=lambda item: item.encode("utf-8"))
    codes = {item: index for index, item in enumerate(dictionary)}
    node_map = [{"id": node["id"], "ordinal": index,
                 "labels": sorted(codes[item] for item in node_labels[node["id"]])}
                for index, node in enumerate(nodes)]
    members = {item: [] for item in dictionary}
    for node in nodes:
        for item in node_labels[node["id"]]:
            members[item].append(node["id"])
    postings = [{"label": item, "code": codes[item], "ids": members[item]}
                for item in dictionary]
    results = []
    for query in value["queries"]:
        wanted = {query["label"]} if query["op"] == "scan" else set(query["labels"])
        found = [node["id"] for node in nodes
                 if wanted <= node_labels[node["id"]]
                 and (query.get("after") is None or node["id"] > query["after"])]
        if "limit" in query:
            found = found[:query["limit"]]
        results.append(found)
    return {"dictionary": dictionary, "node_map": node_map,
            "postings": postings, "results": results}


def exact(actual, expected, context):
    """Reject bool/int equality and noncanonical expected shapes or ordering."""
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
    return json.dumps(value, sort_keys=True, ensure_ascii=True,
                      separators=(",", ":"), allow_nan=False)


def case_input(case):
    return {"nodes": case["nodes"], "queries": case["queries"]}


def rejects(action, reason, context):
    try:
        action()
    except ValueError as error:
        require(str(error) == reason,
                f"{context}: expected rejection {reason!r}, got {str(error)!r}")
    else:
        raise ValueError(f"{context}: expected rejection {reason!r}")


def validate_fixture(path):
    document = load_fixture(path)
    keys(document, "schema version cases malformed", "root")
    require(document["schema"] == SCHEMA, "schema: unsupported fixture schema")
    require(type(document["version"]) is int and document["version"] == 1,
            "version: expected integer 1")
    require(isinstance(document["cases"], list) and len(document["cases"]) >= 4,
            "cases: require at least 4 goldens")
    require(isinstance(document["malformed"], list) and len(document["malformed"]) >= 8,
            "malformed: require at least 8 cases")
    names, queries = set(), 0
    for field in ("cases", "malformed"):
        for case in document[field]:
            keys(case, "name purpose nodes queries expected" if field == "cases"
                 else "name input reason", field)
            require(isinstance(case["name"], str) and case["name"], "case: missing name")
            require(case["name"] not in names, "duplicate case name")
            names.add(case["name"])
            if field == "cases":
                require(isinstance(case["purpose"], str) and case["purpose"],
                        "case: missing purpose")
                exact(evaluate(case_input(case)), case["expected"], case["name"])
                queries += len(case["queries"])
            else:
                require(isinstance(case["reason"], str) and case["reason"],
                        "malformed: missing rejection reason")
                rejects(lambda: evaluate(case["input"]), case["reason"], case["name"])
    return document, queries


def self_test(document):
    shuffles, mutations, rejected, changed = 0, 0, 0, 0
    for case in document["cases"]:
        baseline = evaluate(case_input(case))
        for seed in SHUFFLE_SEEDS:
            shuffled = copy.deepcopy(case_input(case))
            rng = random.Random(seed)
            rng.shuffle(shuffled["nodes"])
            for node in shuffled["nodes"]:
                rng.shuffle(node["labels"])
            for query in shuffled["queries"]:
                if query["op"] == "all":
                    rng.shuffle(query["labels"])
            exact(evaluate(shuffled), case["expected"], f"{case['name']} shuffle {seed}")
            shuffles += 1

        for step in range(32):
            value = copy.deepcopy(case_input(case))
            kind = step % 8
            if kind == 0:
                used = {node["id"] for node in value["nodes"]}
                fresh = next(index for index in range(len(used) + 1) if index not in used)
                value["nodes"].append({"id": fresh, "labels": [f"Mutation_{step}"]})
            elif kind == 1:
                value["nodes"].append({"id": (-1, U64_MAX + 1, True, 1.0)[step // 8],
                                       "labels": []})
            elif kind == 2:
                node = value["nodes"][0] if value["nodes"] else {"id": 0, "labels": []}
                if not value["nodes"]:
                    value["nodes"].append(copy.deepcopy(node))
                value["nodes"].append(copy.deepcopy(node))
            elif kind == 3:
                value["nodes"].append({"id": 123, "labels": ["bad-label"]})
                # A fresh ID ensures this mutation reaches label validation.
                used = {node["id"] for node in value["nodes"][:-1]}
                value["nodes"][-1]["id"] = next(
                    index for index in range(len(used) + 1) if index not in used)
            elif kind == 4:
                value["queries"].append({"op": "all", "labels": []})
            elif kind == 5:
                value["queries"].append({"op": "all", "labels": [f"L{i}" for i in range(9)]})
            elif kind == 6:
                value["queries"][0]["after"] = True
            else:
                value["queries"].append({"op": "all", "labels": ["A"], "limit": 0})
            try:
                actual = evaluate(value)
            except ValueError:
                rejected += 1
            else:
                require(canonical(actual) != canonical(baseline),
                        f"{case['name']} mutation {step}: unchanged result")
                changed += 1
            mutations += 1

        # Corrupt each expected surface and every query result, not just inputs.
        broken_blocks = []
        for field in ("dictionary", "node_map", "postings"):
            broken = copy.deepcopy(case["expected"])
            broken[field].append("corrupt")
            broken_blocks.append(broken)
        for index in range(len(case["queries"])):
            broken = copy.deepcopy(case["expected"])
            broken["results"][index].append(0)
            broken_blocks.append(broken)
        if baseline["node_map"]:
            broken = copy.deepcopy(case["expected"])
            broken["node_map"][0]["ordinal"] = False
            broken_blocks.append(broken)
        for index, broken in enumerate(broken_blocks):
            try:
                exact(baseline, broken, f"{case['name']} expected mutation {index}")
            except ValueError:
                rejected += 1
            else:
                raise ValueError(f"{case['name']}: corrupt expectation accepted")
            mutations += 1
    duplicate_keys = (('{"a": 1, "a": 2}', "a"),
                      ('{"cases": [{"name": "x", "name": "y"}]}', "name"))
    for text, key in duplicate_keys:
        rejects(lambda: json.loads(text, object_pairs_hook=unique_object,
                                   parse_constant=reject_constant),
                f"duplicate JSON key {key!r}", "duplicate key mutation")
        rejected += 1
        mutations += 1
    require(shuffles >= 5 * len(document["cases"]), "self-test: insufficient shuffles")
    require(mutations >= 100 and changed > 0 and rejected > 0,
            "self-test: insufficient mutation coverage")
    return shuffles, mutations, rejected, changed


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", type=Path, default=FIXTURE)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    try:
        document, queries = validate_fixture(args.fixture)
        print(f"Verified {len(document['cases'])} goldens, {queries} queries, "
              f"{len(document['malformed'])} malformed cases with exact rejection reasons")
        if args.self_test:
            shuffles, mutations, rejected, changed = self_test(document)
            print(f"Verified {shuffles} seeded shuffles and {mutations} deterministic mutations: "
                  f"{rejected} rejected, {changed} changed results")
    except (OSError, ValueError) as error:
        print(f"{args.fixture}: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
