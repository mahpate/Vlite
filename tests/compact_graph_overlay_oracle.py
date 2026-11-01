"""Stdlib-only logical reference for the approved P4 committed graph overlay.

Run python tests\\compact_graph_overlay_oracle.py --self-test; paths use __file__.
Schema vlite.compact-graph-overlay-logical v1 is NOT a byte format or public API.
Sources: p4-compact-graph.md (mutations/expiry), compact-storage-contract.md
sections 5-6 (accessor precedence/TTL), and docs/GRAPH.md (committed reads/TTL).
Engine constraints: state.rs new_id/PutNode/PutEdge/PatchEdge, mutate.rs
CreateEdge, state/temporal.rs put_temporal_edge identity, and state/snapshot.rs
close/load_edge high-water validation. Shared adjacency cites its read sources.
Hand-authored goldens are never generated or rewritten by this tool.

A base has base_sequence, next_node_id/next_edge_id high-water marks and full
nodes/edges in compact_graph_oracle's logical row schema. Each overlay is an
already committed coherent state-projection delta, NOT a write-operation log:
{sequence, nodes, edges}, with entries {id, row}. Null row is a tombstone;
otherwise row contains labels for nodes or source/target/relationship for
edges, WITHOUT id. Existing edge identity is immutable; topology changes
require a tombstone and a fresh ID. Optional deadline preserves the existing
deadline when omitted on an update; explicit null removes it. New rows without
a deadline are immortal.
Sequences are unique u64 integers strictly above base_sequence. Entry IDs are
unique within each entity kind in each commit. Input arrays are unordered;
sequence, never array position or deadline, decides newest committed winner.
Within a commit, new rows allocate in ascending ID order in each namespace,
independent of array order; this asserts a possible committed projection,
not the order of arbitrary writes. Base marks must exceed every base ID.
New IDs must meet the running mark, which advances to checked id+1 and never
decreases after tombstones. Thus deleted IDs cannot return and u64::MAX cannot
be allocated. Existing updates do not allocate. We check physical endpoint
coherence after each complete commit, not intermediate entry order: a node
tombstone must be accompanied by tombstones for its remaining incident edges.

Select newest physical rows FIRST, then apply one explicit clock per case.
Deadline <= clock hides a row, never falling back to an older row. A hidden
node masks all incident edges. Output live rows ascend by public ID, retain
their winning sequence, sort label sets by UTF-8 bytes and normalize missing
deadlines to null. Exact rows and every adjacency result must match the golden.
Adjacency reuses compact_graph_oracle's logical projection; this oracle does
not author or compare CSR, dictionaries, ordinals or immutable-codec goldens.

Only labels, endpoints, relationships, deadlines and allocation marks are
modeled. This is not full write admission or full engine/public-API parity:
properties, keys, write-time clock, schema admission, attachments/ownership,
mutation planning/permissions, partial transactions, WAL/framing, activation,
performance and temporal behavior are excluded. In particular, node tombstones
do NOT introduce node-deletion permissions or represent permission to delete
incident edges.
The clock controls visibility of current winners, not historical selection.
"""

import argparse
import copy
import json
from pathlib import Path
import random
import subprocess
import sys
import tempfile

import compact_graph_oracle as graph


FIXTURE = Path(__file__).resolve().parent / "fixtures" / "compact" / "graph-overlay-cases.json"
SCHEMA = "vlite.compact-graph-overlay-logical"
SHUFFLE_SEEDS = (17, 941)


def projected_case(case, nodes, edges):
    return {"name": case["name"], "purpose": case["purpose"],
            "nodes": nodes, "edges": edges, "clock": case["clock"],
            "queries": case["queries"], "expected": {}}


def materialize(case):
    """Resolve complete commits while checking physical, not TTL, coherence."""
    nodes = {row["id"]: (case["base_sequence"], row) for row in case["base"]["nodes"]}
    edges = {row["id"]: (case["base_sequence"], row) for row in case["base"]["edges"]}
    marks = {field: case["base"][mark] for field, mark in
             (("nodes", "next_node_id"), ("edges", "next_edge_id"))}
    for overlay in sorted(case["overlays"], key=lambda commit: commit["sequence"]):
        for field, state in (("nodes", nodes), ("edges", edges)):
            for entry in sorted(overlay[field], key=lambda item: item["id"]):
                public_id, row = entry["id"], entry["row"]
                existing = state.get(public_id)
                context = f"{case['name']}: {field} ID {public_id}"
                if row is None:
                    graph.require(existing is not None, f"{context}: tombstone for absent row")
                    del state[public_id]
                    continue
                if existing is None:
                    graph.require(public_id >= marks[field],
                                  f"{context}: insertion below high-water mark")
                    graph.require(public_id < graph.U64_MAX, f"{context}: ID space exhausted")
                    marks[field] = public_id + 1
                elif field == "edges":
                    graph.require(all(row[key] == existing[1][key] for key in
                                      ("source", "target", "relationship")),
                                  f"{context}: immutable edge identity")
                replacement = {"id": public_id, **row}
                if "deadline" not in row and existing is not None:
                    replacement["deadline"] = existing[1].get("deadline")
                state[public_id] = (overlay["sequence"], replacement)
        for _, edge in edges.values():
            graph.require(edge["source"] in nodes and edge["target"] in nodes,
                          f"{case['name']}: unknown endpoint after commit {overlay['sequence']}")
    return nodes, edges


def validate_input(case):
    graph.keys(case, "name purpose base_sequence base overlays clock queries expected", "case")
    graph.u64(case["base_sequence"], "base_sequence")
    graph.keys(case["base"], "nodes edges next_node_id next_edge_id", "base")
    graph.validate_input(projected_case(case, case["base"]["nodes"], case["base"]["edges"]))
    for field, mark in (("nodes", "next_node_id"), ("edges", "next_edge_id")):
        graph.u64(case["base"][mark], f"base.{mark}")
        graph.require(all(row["id"] < case["base"][mark] for row in case["base"][field]),
                      f"base.{mark}: high-water mark must exceed every base ID")
    graph.require(isinstance(case["overlays"], list) and case["overlays"],
                  "overlays: expected nonempty array")
    sequences = set()
    for index, overlay in enumerate(case["overlays"]):
        context = f"{case['name']}.overlays[{index}]"
        graph.keys(overlay, "sequence nodes edges", context)
        graph.u64(overlay["sequence"], f"{context}.sequence")
        graph.require(overlay["sequence"] > case["base_sequence"],
                      f"{context}: sequence must exceed base_sequence")
        graph.require(overlay["sequence"] not in sequences, f"{context}: duplicate commit sequence")
        sequences.add(overlay["sequence"])
        for field in ("nodes", "edges"):
            graph.require(isinstance(overlay[field], list), f"{context}.{field}: expected array")
            seen = set()
            for entry in overlay[field]:
                location = f"{context}.{field}"
                graph.keys(entry, "id row", location)
                graph.u64(entry["id"], f"{location}.id")
                graph.require(entry["id"] not in seen, f"{location}: duplicate entry ID")
                seen.add(entry["id"])
                row = entry["row"]
                if row is None:
                    continue
                graph.keys(row, "labels" if field == "nodes" else "source target relationship",
                           f"{location}.row", "deadline")
                if row.get("deadline") is not None:
                    graph.u64(row["deadline"], f"{location}.deadline")
                if field == "nodes":
                    graph.strings(row["labels"], f"{location}.labels")
                else:
                    graph.u64(row["source"], f"{location}.source")
                    graph.u64(row["target"], f"{location}.target")
                    graph.text(row["relationship"], f"{location}.relationship")
    materialize(case)


def evaluate(case):
    """Independent row/version/visibility selection; shared adjacency only."""
    physical_nodes, physical_edges = materialize(case)

    def visible(row):
        deadline = row.get("deadline")
        return deadline is None or case["clock"] < deadline

    nodes = {public_id: value for public_id, value in physical_nodes.items() if visible(value[1])}
    edges = {public_id: value for public_id, value in physical_edges.items()
             if visible(value[1]) and value[1]["source"] in nodes and value[1]["target"] in nodes}

    def rows(state, field):
        result = []
        for public_id, (sequence, row) in sorted(state.items()):
            record = {"id": public_id, "sequence": sequence, "deadline": row.get("deadline")}
            if field == "nodes":
                record["labels"] = sorted(row["labels"], key=str.encode)
            else:
                record.update({key: row[key] for key in ("source", "target", "relationship")})
            result.append(record)
        return result

    projection = graph.evaluate(projected_case(
        case, [row for _, row in nodes.values()], [row for _, row in edges.values()]))
    return {"nodes": rows(nodes, "nodes"), "edges": rows(edges, "edges"),
            "adjacency": projection["adjacency"]}


def perturb(case, seed):
    value = copy.deepcopy(case)
    rng = random.Random(seed)
    rng.shuffle(value["overlays"])
    for layer in [value["base"], *value["overlays"]]:
        for field in ("nodes", "edges"):
            rng.shuffle(layer[field])
            for entry in layer[field]:
                row = entry.get("row", entry)
                if isinstance(row, dict) and "labels" in row:
                    rng.shuffle(row["labels"])
    for query in value["queries"]:
        if query["relationship"] is not None:
            rng.shuffle(query["relationship"])
    return value


def validate_fixture(path, report=False):
    document = graph.load_fixture(path)
    graph.keys(document, "schema version cases", "root")
    graph.require(document["schema"] == SCHEMA, "schema: unsupported fixture schema")
    graph.require(type(document["version"]) is int and document["version"] == 1,
                  "version: expected integer 1")
    graph.require(isinstance(document["cases"], list) and document["cases"],
                  "cases: expected nonempty array")
    names, queries = set(), 0
    for case in document["cases"]:
        validate_input(case)
        graph.require(case["name"] not in names, "duplicate case name")
        names.add(case["name"])
        actual = evaluate(case)
        graph.exact(actual, case["expected"], f"{case['name']}.expected")
        for seed in SHUFFLE_SEEDS:
            shuffled = perturb(case, seed)
            validate_input(shuffled)
            graph.require(graph.canonical(evaluate(shuffled)) == graph.canonical(actual),
                          f"{case['name']}: shuffle {seed} canonical output mismatch")
        queries += len(case["queries"])
        if report:
            print(f"OK {case['name']}: nodes={len(actual['nodes'])} "
                  f"edges={len(actual['edges'])} queries={len(case['queries'])} shuffles=2")
    return len(names), queries


def self_test(path):
    """Require real CLI failure/diagnostics for schema and golden corruption."""
    document = graph.load_fixture(path)
    mutations = []

    def mutate(label, case_name, change, diagnostic="oracle"):
        value = copy.deepcopy(document)
        selected = next((case for case in value["cases"] if case["name"] == case_name), None)
        graph.require(selected is not None, f"self-test requires case {case_name!r}")
        change(selected)
        mutations.append((label, json.dumps(value, ensure_ascii=True, allow_nan=False), diagnostic))

    topology = "replacement-topology"
    tombstones = "tombstone-precedence"
    expiry = "latest-expiry-cascade"
    boundary = "u64-clock-boundary"
    mutate("stale-node-labels", topology,
           lambda c: c["expected"]["nodes"][0].__setitem__("labels", ["old"]))
    mutate("stale-node-sequence", topology,
           lambda c: c["expected"]["nodes"][0].__setitem__("sequence", 10))
    mutate("stale-edge-sequence", topology,
           lambda c: c["expected"]["edges"][0].__setitem__("sequence", 11))
    mutate("wrong-fresh-edge-endpoint", topology,
           lambda c: c["expected"]["edges"][0].__setitem__("source", 1))
    mutate("wrong-fresh-edge-relationship", topology,
           lambda c: c["expected"]["edges"][0].__setitem__("relationship", "z"))
    mutate("reversed-live-rows", topology, lambda c: c["expected"]["edges"].reverse())
    mutate("collapsed-parallel", topology, lambda c: c["expected"]["adjacency"][2].pop(1))
    mutate("duplicate-loop", topology,
           lambda c: c["expected"]["adjacency"][2].append(["b", 23]))
    mutate("stale-adjacency", topology,
           lambda c: c["expected"]["adjacency"][0].append(["z", 20]))
    mutate("inclusive-cursor", topology,
           lambda c: c["expected"]["adjacency"][3].insert(0, ["a", 26]))
    mutate("numeric-only-cursor", topology,
           lambda c: c["expected"]["adjacency"].__setitem__(3, []))
    mutate("ignored-filter", topology,
           lambda c: c["expected"]["adjacency"][5].append(["a", 26]))
    mutate("ignored-empty-filter", topology,
           lambda c: c["expected"]["adjacency"][6].append(["a", 26]))
    mutate("ignored-zero-limit", topology,
           lambda c: c["expected"]["adjacency"][7].append(["a", 26]))
    mutate("fallback-tombstoned-base", tombstones,
           lambda c: c["expected"]["edges"].append(
               {"id": 4, "sequence": 1, "source": 2, "target": 2,
                "relationship": "old", "deadline": None}))
    mutate("fallback-expired-node", expiry,
           lambda c: c["expected"]["nodes"].append(
               {"id": 1, "sequence": 4, "labels": ["old"], "deadline": None}))
    mutate("fallback-expired-edge", expiry,
           lambda c: c["expected"]["edges"].append(
               {"id": 10, "sequence": 4, "source": 2, "target": 3,
                "relationship": "old", "deadline": None}))
    mutate("visible-overlay-cascade", expiry,
           lambda c: c["expected"]["adjacency"][0].append(["overlay", 15]))
    mutate("lost-cleared-deadline", expiry,
           lambda c: c["expected"]["nodes"][0].__setitem__("deadline", 100))
    mutate("omission-is-not-node-expiry-removal", expiry,
           lambda c: c["overlays"][1]["nodes"][1]["row"].pop("deadline"))
    mutate("lost-cleared-edge-deadline", expiry,
           lambda c: next(entry for entry in c["overlays"][1]["edges"]
                          if entry["id"] == 12)["row"].pop("deadline"))
    mutate("omitted-node-deadline-cleared", expiry,
           lambda c: c["expected"]["nodes"][1].__setitem__("deadline", None))
    mutate("omitted-edge-deadline-cleared", expiry,
           lambda c: c["expected"]["edges"][1].__setitem__("deadline", None))
    mutate("rounded-u64-id", boundary,
           lambda c: c["expected"]["nodes"][1].__setitem__("id", 9007199254740992))
    mutate("equality-lives-at-max-clock", boundary,
           lambda c: c["expected"]["adjacency"][0].append(["due", 0]))
    mutate("bool-expected-id", boundary,
           lambda c: c["expected"]["nodes"][0].__setitem__("id", False))
    mutate("extra-expected-field", topology,
           lambda c: c["expected"].__setitem__("csr", {}))
    mutate("partial-node-row", topology,
           lambda c: c["overlays"][0]["nodes"][0]["row"].pop("labels"), "expected keys")
    mutate("partial-edge-row", topology,
           lambda c: c["overlays"][0]["edges"][0]["row"].pop("source"), "expected keys")
    mutate("row-id-not-allowed", topology,
           lambda c: c["overlays"][0]["nodes"][0]["row"].__setitem__("id", 1), "expected keys")
    mutate("boolean-tombstone", topology,
           lambda c: c["overlays"][0]["nodes"][0].__setitem__("row", False), "expected keys")
    mutate("extra-tombstone-field", tombstones,
           lambda c: c["overlays"][0]["nodes"][0].__setitem__("deadline", 100), "expected keys")
    mutate("duplicate-node-entry", topology,
           lambda c: c["overlays"][0]["nodes"].append(c["overlays"][0]["nodes"][0]),
           "duplicate entry ID")
    mutate("duplicate-edge-entry", topology,
           lambda c: c["overlays"][0]["edges"].append(c["overlays"][0]["edges"][0]),
           "duplicate entry ID")
    mutate("duplicate-sequence", topology,
           lambda c: c["overlays"].append(copy.deepcopy(c["overlays"][0])),
           "duplicate commit sequence")
    mutate("sequence-at-base", topology,
           lambda c: c["overlays"][0].__setitem__("sequence", c["base_sequence"]),
           "sequence must exceed")
    mutate("sequence-before-base", topology,
           lambda c: c["overlays"][0].__setitem__("sequence", 0), "sequence must exceed")
    mutate("dangling-final-endpoint", topology,
           lambda c: c["overlays"][0]["edges"][0]["row"].__setitem__("target", 999),
           "unknown endpoint")
    mutate("incoherent-earlier-commit", topology,
           lambda c: c["overlays"][1]["edges"][0]["row"].__setitem__("source", 999),
           "unknown endpoint")
    mutate("node-tombstone-with-incident-edge", topology,
           lambda c: c["overlays"][0]["nodes"].append({"id": 3, "row": None}), "unknown endpoint")
    mutate("duplicate-base-node", topology,
           lambda c: c["base"]["nodes"].append(c["base"]["nodes"][0]), "duplicate nodes ID")
    mutate("duplicate-base-edge", topology,
           lambda c: c["base"]["edges"].append(c["base"]["edges"][0]), "duplicate edges ID")

    def allocation_probe(case):
        case["base_sequence"] = 1
        case["base"] = {
            "next_node_id": 4, "next_edge_id": 6,
            "nodes": [{"id": public_id, "labels": []} for public_id in (1, 2, 3)],
            "edges": [{"id": 5, "source": 1, "target": 2, "relationship": "r"}],
        }
        case["overlays"] = [{"sequence": 2, "nodes": [], "edges": []}]

    def probe(label, change, diagnostic):
        def apply(case):
            allocation_probe(case)
            change(case)
        mutate(label, topology, apply, diagnostic)

    for label, replacement in (
        ("changed-source", {"source": 3, "target": 2, "relationship": "r"}),
        ("changed-target", {"source": 1, "target": 3, "relationship": "r"}),
        ("changed-relationship", {"source": 1, "target": 2, "relationship": "q"}),
        ("changed-endpoint-and-relationship", {"source": 3, "target": 2, "relationship": "q"}),
    ):
        probe(label, lambda c, row=replacement: c["overlays"][0]["edges"].append(
            {"id": 5, "row": row}), "immutable edge identity")

    def below_edge_mark(case):
        case["base"]["edges"][0]["id"] = 9
        case["base"]["next_edge_id"] = 10
        case["overlays"][0]["edges"] = [
            {"id": 3, "row": {"source": 1, "target": 2, "relationship": "r"}}]

    probe("edge-insertion-below-mark", below_edge_mark, "insertion below high-water mark")
    probe("node-insertion-below-mark",
          lambda c: c["overlays"][0]["nodes"].append({"id": 0, "row": {"labels": []}}),
          "insertion below high-water mark")

    def resurrect(case, field):
        case["overlays"][0]["edges"] = [{"id": 5, "row": None}]
        if field == "nodes":
            case["overlays"][0]["nodes"] = [{"id": 2, "row": None}]
        case["overlays"].append({
            "sequence": 3,
            "nodes": [{"id": 2, "row": {"labels": []}}] if field == "nodes" else [],
            "edges": [{"id": 5, "row": {"source": 1, "target": 2, "relationship": "r"}}]
            if field == "edges" else [],
        })

    for field, mark, largest in (("nodes", "next_node_id", 3), ("edges", "next_edge_id", 5)):
        probe(f"resurrect-{field}", lambda c, field=field: resurrect(c, field),
              "insertion below high-water mark")
        for label, value in (("equal", largest), ("lower", 0)):
            probe(f"{field}-base-mark-{label}",
                  lambda c, mark=mark, value=value: c["base"].__setitem__(mark, value),
                  "high-water mark must exceed")
        for label, value in (("boolean", True), ("negative", -1),
                             ("overflow", 2**64), ("string", "6")):
            probe(f"{field}-base-mark-{label}",
                  lambda c, mark=mark, value=value: c["base"].__setitem__(mark, value),
                  "expected u64")
        probe(f"{field}-missing-base-mark",
              lambda c, mark=mark: c["base"].pop(mark), "expected keys")

        def exhausted(case, field=field, mark=mark):
            case["base"][mark] = graph.U64_MAX
            row = {"labels": []} if field == "nodes" else {
                "source": 1, "target": 2, "relationship": "r"}
            case["overlays"][0][field] = [{"id": graph.U64_MAX, "row": row}]

        probe(f"{field}-allocation-exhaustion", exhausted, "ID space exhausted")

        def exhausted_after_delete(case, field=field, mark=mark):
            case["base"][field][0]["id"] = graph.U64_MAX - 1
            if field == "nodes":
                case["base"]["edges"] = []
            case["base"][mark] = graph.U64_MAX
            case["overlays"][0][field] = [{"id": graph.U64_MAX - 1, "row": None}]
            row = {"labels": []} if field == "nodes" else {
                "source": 1, "target": 2, "relationship": "r"}
            case["overlays"].append({"sequence": 3, "nodes": [], "edges": []})
            case["overlays"][1][field] = [{"id": graph.U64_MAX - 1, "row": row}]

        probe(f"{field}-exhausted-mark-retained-after-delete", exhausted_after_delete,
              "insertion below high-water mark")

        def invalid_base_max(case, field=field, mark=mark):
            case["base"][field][0]["id"] = graph.U64_MAX
            if field == "nodes":
                case["base"]["edges"] = []
            case["base"][mark] = graph.U64_MAX

        probe(f"{field}-unallocatable-base-ID", invalid_base_max,
              "high-water mark must exceed")
    for label, value in (("bool", True), ("negative", -1), ("overflow", 2**64), ("string", "1")):
        for field in ("sequence", "id", "deadline", "clock", "base_sequence", "endpoint", "limit"):
            def change(case, field=field, value=value):
                overlay = case["overlays"][0]
                if field == "sequence":
                    overlay["sequence"] = value
                elif field == "id":
                    overlay["nodes"][0]["id"] = value
                elif field == "deadline":
                    overlay["nodes"][0]["row"]["deadline"] = value
                elif field == "endpoint":
                    overlay["edges"][0]["row"]["source"] = value
                elif field == "limit":
                    case["queries"][0]["limit"] = value
                else:
                    case[field] = value
            mutate(f"{label}-{field}", topology, change, "expected u64")
    mutate("edge-deadline-overflow", topology,
           lambda c: c["overlays"][0]["edges"][0]["row"].__setitem__("deadline", 2**64),
           "expected u64")
    mutate("float-sequence", topology,
           lambda c: c["overlays"][0].__setitem__("sequence", 13.0), "non-integer JSON number")
    mutate("duplicate-label", topology,
           lambda c: c["overlays"][0]["nodes"][0]["row"].__setitem__("labels", ["n", "n"]),
           "duplicate string")
    mutate("non-array-labels", topology,
           lambda c: c["overlays"][0]["nodes"][0]["row"].__setitem__("labels", "n"),
           "expected string array")
    mutate("surrogate-relationship", topology,
           lambda c: c["overlays"][0]["edges"][0]["row"].__setitem__("relationship", "\ud800"),
           "invalid UTF-8")
    mutate("empty-relationship", topology,
           lambda c: c["overlays"][0]["edges"][0]["row"].__setitem__("relationship", ""),
           "expected nonempty")
    mutate("invalid-direction", topology,
           lambda c: c["queries"][0].__setitem__("direction", "sideways"), "invalid direction")
    mutate("bad-filter", topology,
           lambda c: c["queries"][0].__setitem__("relationship", "a"), "expected string array")
    mutate("duplicate-filter", topology,
           lambda c: c["queries"][0].__setitem__("relationship", ["a", "a"]), "duplicate string")
    mutate("bad-cursor", topology,
           lambda c: c["queries"][3]["after"].__setitem__("edge", True), "expected u64")
    mutate("extra-case-field", topology,
           lambda c: c.__setitem__("as_of", 12), "expected keys")
    mutate("overlays-not-array", topology, lambda c: c.__setitem__("overlays", {}), "nonempty array")
    mutate("empty-overlays", topology, lambda c: c.__setitem__("overlays", []), "nonempty array")
    mutate("nodes-not-array", topology,
           lambda c: c["overlays"][0].__setitem__("nodes", None), "expected array")
    mutate("duplicate-case-name", topology,
           lambda c: c.__setitem__("name", tombstones), "duplicate case name")
    for label, contents, diagnostic in (
        ("duplicate-json-key", '{"schema":1,"schema":2}', "duplicate JSON key"),
        ("nested-duplicate-key", '{"row":{"deadline":null,"deadline":0}}', "duplicate JSON key"),
        ("nan", '{"clock":NaN}', "invalid JSON constant"),
        ("infinity", '{"clock":Infinity}', "invalid JSON constant"),
        ("truncated-json", '{"schema":', "Expecting value"),
        ("root-array", "[]", "expected keys"),
    ):
        mutations.append((label, contents, diagnostic))
    for label, key, value, diagnostic in (
        ("bad-schema", "schema", "unknown", "unsupported fixture schema"),
        ("boolean-version", "version", True, "expected integer 1"),
        ("empty-cases", "cases", [], "expected nonempty array"),
    ):
        broken = copy.deepcopy(document)
        broken[key] = value
        mutations.append((label, json.dumps(broken), diagnostic))
    with tempfile.TemporaryDirectory(prefix="vlite-graph-overlay-oracle-",
                                     dir=Path(__file__).resolve().parent) as directory:
        for label, contents, diagnostic in mutations:
            broken = Path(directory) / f"{label}.json"
            broken.write_text(contents, encoding="utf-8")
            result = subprocess.run(
                [sys.executable, "-B", str(Path(__file__).resolve()), "--fixture", str(broken)],
                capture_output=True, text=True, timeout=30, check=False)
            graph.require(result.returncode == 1 and diagnostic in result.stderr,
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
        print(f"Verified {cases} compact graph overlay cases and {queries} queries: {args.fixture}")
        if args.self_test:
            print(f"Rejected {self_test(args.fixture)} malformed or incorrect fixtures")
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"{args.fixture}: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
