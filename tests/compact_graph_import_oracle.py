"""Stdlib-only LOGICAL reference for P4 parallel graph import.

Run python tests\\compact_graph_import_oracle.py --self-test; paths use __file__.
No flag verifies the hand-authored golden; this tool NEVER writes goldens.
Schema vlite.compact-graph-import-logical v1 is NOT a byte format or an API.

Sources (repository file:line):
docs/projects/roadmap/p4-compact-graph.md:87-95, "Parallel import and durable
publication": partition validation, ordinal IDs, dictionaries, bounded sorted
spill runs, degree prefix sums, global checks before atomic installation.
docs/projects/roadmap/p4-compact-graph.md:58-85, "Mutations, expiry and
attachments": deadlines, node cascade, physical ownership until reclamation.
docs/GRAPH.md:42-56: unique keys and atomic batches; :64-95: TTL and protection.
src/property_graph/engine/state.rs:223-228: initial node/edge marks are ZERO;
:254-259: marks; :316-321: adjacency order (relationship, edge ID);
:780-786: checked ID increment; :1042-1048: duplicate node keys;
:1105-1118: endpoints before edge allocation; :1161-1190: attachment checks,
node/collection collision BEFORE global (collection, record) ownership.
src/property_graph/engine/mutate.rs:3-16: tentative ordered admission/rollback,
adapter checks record existence; :365-381: CreateNode key check then allocation;
:321-325: unknown key is UnknownNodeKey; :443-446: source then destination
key resolution (ordinal misses use state.rs:1108-1111 NodeNotFound).
Logical rows/order and strict JSON helpers mirror/reuse compact_graph_oracle;
docstring, fixture override and self-test follow compact_graph_overlay_oracle.

Assumptions: a new graph in an otherwise empty ownership domain; all referenced
records already exist and have no expiry (adapter work is outside this model).
Input is an ordered nodes array followed by an ordered edges array, NOT an
interleaved stream. Error ordinal is zero-based in that concatenation; all
attachments on a node are admitted in listed order at that node's ordinal.
Optional key means an anonymous logical node (not an invented engine key);
key endpoints are {key: string}, ordinal endpoints {ordinal: integer}.
No edge keys. Nodes/edges allocate in their own namespaces by input order.
Labels are sets. String/collection syntax is prevalidated; this reference is
not the complete engine string/size-limit validator or full write admission.

This is a PHYSICAL import image, not a read projection or a clocked mutation
batch. Deadlines, including 0 and u64::MAX, persist without filtering. No clock
is invented, no expired key/owner is freed, and no deadline is interpreted as
an ordering key. Clocked reclamation/visibility remains the engine's concern.
Absent/null deadlines normalize to null. Properties, schema, WAL, crypto,
temporal semantics, durable filesystem publication and performance are excluded.

Result is exactly {image: ...} OR {error: {class, ordinal}}, never a partial
image. Rows ascend by ID; node labels and dictionaries sort by UTF-8 bytes
without normalization. Attachments sort by collection. Out/in adjacency holds
[relationship, public edge ID] pairs ordered by that tuple, with a self-loop
once on EACH side and parallel edges retained. Degree prefix sums have N+1
entries, start at zero and end at E. Marks equal admitted node/edge counts.

The independent sequential baseline stops at its first admission error.
Parallel simulation locally validates contiguous partitions, resolves keys
globally, assigns ordinal IDs, gathers global errors before install, sorts
bounded membership runs and k-way merges them, then prefix-sums degrees.
W=1,2,3,4,7,16; R=1,2,3,unbounded; balanced/front-heavy/back-heavy splits
exercise empty partitions and skew. Runs are in-memory logical spill runs,
not disk I/O or a memory/performance claim. No worker scheduling is modeled.
"""

import argparse
import copy
import heapq
from pathlib import Path
import subprocess
import sys
import tempfile

import compact_graph_oracle as graph


FIXTURE = Path(__file__).resolve().parent / "fixtures" / "compact_graph_import" / "golden.json"
SCHEMA = "vlite.compact-graph-import-logical"
WORKERS = (1, 2, 3, 4, 7, 16)
RUNS = (1, 2, 3, None)
LAYOUTS = ("balanced", "front-heavy", "back-heavy")


def validate_case(case):
    graph.keys(case, "name nodes edges expected", "case")
    graph.text(case["name"], "case.name")
    for kind in ("nodes", "edges"):
        graph.require(isinstance(case[kind], list), f"{kind}: expected array")
        for row in case[kind]:
            if kind == "nodes":
                graph.keys(row, "labels", "node", "key deadline attachments")
                graph.strings(row["labels"], "node.labels")
                if "key" in row:
                    graph.text(row["key"], "node.key")
                attachments = row.get("attachments", [])
                graph.require(isinstance(attachments, list), "attachments: expected array")
                for attachment in attachments:
                    graph.keys(attachment, "collection record", "attachment")
                    graph.u64(attachment["collection"], "collection")
                    graph.require(attachment["collection"] <= 65535, "collection: expected u16")
                    graph.u64(attachment["record"], "record")
            else:
                graph.keys(row, "source target relationship", "edge", "deadline")
                graph.text(row["relationship"], "relationship")
                for side in ("source", "target"):
                    endpoint = row[side]
                    graph.require(isinstance(endpoint, dict) and
                                  set(endpoint) in ({"key"}, {"ordinal"}),
                                  "endpoint: expected key or ordinal object")
                    if "key" in endpoint:
                        graph.text(endpoint["key"], "endpoint.key")
                    else:
                        graph.u64(endpoint["ordinal"], "endpoint.ordinal")
            if row.get("deadline") is not None:
                graph.u64(row["deadline"], "deadline")


def error_result(kind, ordinal):
    return {"error": {"class": kind, "ordinal": ordinal}}


def resolve(endpoint, keys, node_count):
    if "key" in endpoint:
        return keys.get(endpoint["key"])
    ordinal = endpoint["ordinal"]
    return ordinal if ordinal < node_count else None


def node_row(row, node_id):
    return {"id": node_id, "key": row.get("key"),
            "labels": sorted(row["labels"], key=str.encode),
            "deadline": row.get("deadline"),
            "attachments": sorted(
                [[item["collection"], item["record"]] for item in row.get("attachments", [])])}


def edge_row(row, edge_id, source, target):
    return {"id": edge_id, "source": source, "target": target,
            "relationship": row["relationship"], "deadline": row.get("deadline")}


def image_result(nodes, edges, adjacency, prefixes):
    return {"image": {
        "nodes": nodes, "edges": edges, "adjacency": adjacency,
        "degree_prefix_sums": prefixes,
        "dictionaries": {
            "labels": sorted({label for row in nodes for label in row["labels"]}, key=str.encode),
            "relationships": sorted({row["relationship"] for row in edges}, key=str.encode)},
        "next_node_id": len(nodes), "next_edge_id": len(edges)}}


def sequential(case):
    """Direct admission and per-node scans, without partition/run machinery."""
    nodes, edges, keys, owners = [], [], {}, set()
    for ordinal, row in enumerate(case["nodes"]):
        key = row.get("key")
        if key is not None:
            if key in keys:
                return error_result("NodeKeyExists", ordinal)
            keys[key] = ordinal
        collections = set()
        for attachment in row.get("attachments", []):
            collection, record = attachment["collection"], attachment["record"]
            if collection in collections:
                return error_result("NodeAlreadyAttached", ordinal)
            if (collection, record) in owners:
                return error_result("RecordAlreadyAttached", ordinal)
            collections.add(collection)
            owners.add((collection, record))
        nodes.append(node_row(row, ordinal))
    for edge_id, row in enumerate(case["edges"]):
        endpoints = []
        for side in ("source", "target"):
            endpoint = row[side]
            node_id = resolve(endpoint, keys, len(nodes))
            if node_id is None:
                kind = "UnknownNodeKey" if "key" in endpoint else "NodeNotFound"
                return error_result(kind, len(nodes) + edge_id)
            endpoints.append(node_id)
        edges.append(edge_row(row, edge_id, *endpoints))
    adjacency, prefixes = {}, {}
    for direction, endpoint in (("out", "source"), ("in", "target")):
        adjacency[direction], prefixes[direction] = [], [0]
        for node in nodes:
            memberships = sorted(
                ([edge["relationship"], edge["id"]] for edge in edges
                 if edge[endpoint] == node["id"]),
                key=lambda pair: (pair[0].encode("utf-8"), pair[1]))
            adjacency[direction].append(memberships)
            prefixes[direction].append(prefixes[direction][-1] + len(memberships))
    return image_result(nodes, edges, adjacency, prefixes)


def partitions(length, workers, layout):
    if layout == "balanced":
        cuts = [length * index // workers for index in range(workers + 1)]
    else:
        sizes = [max(0, length - workers + 1)] + [
            1 if index < min(length, workers - 1) else 0 for index in range(workers - 1)]
        if layout == "back-heavy":
            sizes.reverse()
        cuts = [0]
        for size in sizes:
            cuts.append(cuts[-1] + size)
    return [range(cuts[index], cuts[index + 1]) for index in range(workers)]


def node_conflicts(indexed_nodes):
    """Errors retain sub-operation order, even when workers finish backwards."""
    keys, owners, errors = set(), set(), []
    for ordinal, row in sorted(indexed_nodes):
        key = row.get("key")
        if key is not None:
            if key in keys:
                errors.append((ordinal, 0, 0, "NodeKeyExists"))
            keys.add(key)
        collections = set()
        for step, attachment in enumerate(row.get("attachments", []), 1):
            collection, record = attachment["collection"], attachment["record"]
            if collection in collections:
                errors.append((ordinal, step, 0, "NodeAlreadyAttached"))
            if (collection, record) in owners:
                errors.append((ordinal, step, 1, "RecordAlreadyAttached"))
            collections.add(collection)
            owners.add((collection, record))
    return errors


def parallel(case, workers, run_size, layout, broken_local_only=False):
    """Simulated workers stage output; installation happens only after checks."""
    node_count = len(case["nodes"])
    events = [("node", row) for row in case["nodes"]] + [
        ("edge", row) for row in case["edges"]]
    staged_nodes, staged_edges, errors = [], [], []
    for partition in reversed(partitions(len(events), workers, layout)):
        local_nodes = [(ordinal, events[ordinal][1]) for ordinal in partition
                       if events[ordinal][0] == "node"]
        for ordinal in partition:
            kind, row = events[ordinal]
            validate_case({"name": "local", "nodes": [row] if kind == "node" else [],
                           "edges": [row] if kind == "edge" else [], "expected": {}})
            if kind == "edge":
                staged_edges.append((ordinal, row))
        errors.extend(node_conflicts(local_nodes))
        staged_nodes.extend(local_nodes)
    if not broken_local_only:
        errors.extend(node_conflicts(staged_nodes))
    keys = {}
    for ordinal, row in sorted(staged_nodes):
        if "key" in row:
            keys.setdefault(row["key"], ordinal)
    nodes = [node_row(row, ordinal) for ordinal, row in sorted(staged_nodes)]
    edges = []
    for ordinal, row in sorted(staged_edges):
        endpoints = [resolve(row[side], keys, node_count) for side in ("source", "target")]
        for step, (side, node_id) in enumerate(zip(("source", "target"), endpoints)):
            if node_id is None:
                kind = "UnknownNodeKey" if "key" in row[side] else "NodeNotFound"
                errors.append((ordinal, step, 0, kind))
        if all(node_id is not None for node_id in endpoints):
            edges.append(edge_row(row, ordinal - node_count, *endpoints))
    if errors:
        ordinal, _, _, kind = min(errors)
        return error_result(kind, ordinal)

    adjacency, prefixes = {}, {}
    for direction, endpoint in (("out", "source"), ("in", "target")):
        runs, buffer = [], []
        for edge in edges:
            buffer.append((edge[endpoint], edge["relationship"].encode("utf-8"), edge["id"]))
            if run_size is not None and len(buffer) == run_size:
                runs.append(sorted(buffer))
                buffer = []
        if buffer:
            runs.append(sorted(buffer))
        rows, degrees = [[] for _ in nodes], [0 for _ in nodes]
        for node_id, relationship, edge_id in heapq.merge(*runs):
            rows[node_id].append([relationship.decode("utf-8"), edge_id])
            degrees[node_id] += 1
        offsets = [0]
        for degree in degrees:
            offsets.append(offsets[-1] + degree)
        adjacency[direction], prefixes[direction] = rows, offsets
    return image_result(nodes, edges, adjacency, prefixes)


def verify_document(document):
    graph.keys(document, "schema version cases", "root")
    graph.require(document["schema"] == SCHEMA, "schema: unsupported fixture schema")
    graph.require(type(document["version"]) is int and document["version"] == 1,
                  "version: expected integer 1")
    graph.require(isinstance(document["cases"], list) and document["cases"],
                  "cases: expected nonempty array")
    names, checks = set(), 0
    for case in document["cases"]:
        validate_case(case)
        graph.require(case["name"] not in names, "duplicate case name")
        names.add(case["name"])
        baseline = sequential(case)
        graph.exact(baseline, case["expected"], case["name"] + ".golden")
        for workers in WORKERS:
            for run_size in RUNS:
                for layout in LAYOUTS:
                    actual = parallel(case, workers, run_size, layout)
                    graph.exact(actual, baseline,
                                f"{case['name']}: W={workers} R={run_size} {layout}")
                    checks += 1
    return len(names), checks


def self_test(document):
    negatives = []

    def mutate(name, change):
        broken = copy.deepcopy(document)
        change(broken)
        negatives.append((name, broken))

    def case(doc, name):
        return next(item for item in doc["cases"] if item["name"] == name)

    mutate("mutated image", lambda doc: case(doc, "single-node")["expected"]["image"][
        "nodes"][0].__setitem__("key", "changed"))
    mutate("wrong ID", lambda doc: case(doc, "single-node")["expected"]["image"][
        "nodes"][0].__setitem__("id", 1))
    mutate("wrong adjacency order", lambda doc: case(doc, "parallel-edges")["expected"][
        "image"]["adjacency"]["out"][0].reverse())
    mutate("wrong error ordinal", lambda doc: case(doc, "multiple-errors")["expected"][
        "error"].__setitem__("ordinal", 3))
    mutate("wrong error class", lambda doc: case(doc, "unknown-endpoint")["expected"][
        "error"].__setitem__("class", "NodeKeyExists"))
    mutate("schema mismatch", lambda doc: doc.__setitem__("schema", "not-an-import"))
    mutate("version mismatch", lambda doc: doc.__setitem__("version", 2))
    mutate("boolean version", lambda doc: doc.__setitem__("version", True))
    mutate("unknown root field", lambda doc: doc.__setitem__("extra", None))
    mutate("unknown input field", lambda doc: case(doc, "single-node")["nodes"][0].__setitem__(
        "extra", None))
    mutate("unknown image field", lambda doc: case(doc, "single-node")["expected"][
        "image"].__setitem__("extra", None))
    mutate("wrong input type", lambda doc: case(doc, "single-node")["nodes"][0].__setitem__(
        "deadline", True))
    mutate("wrong output type", lambda doc: case(doc, "single-node")["expected"][
        "image"].__setitem__("next_node_id", True))
    mutate("wrong prefix sums", lambda doc: case(doc, "self-loop")["expected"][
        "image"]["degree_prefix_sums"].__setitem__("in", [0, 0]))
    mutate("wrong dictionary order", lambda doc: case(doc, "sparse-labels")["expected"][
        "image"]["dictionaries"]["labels"].reverse())
    for name, broken in negatives:
        try:
            verify_document(broken)
        except ValueError:
            continue
        raise ValueError(f"self-test: checker accepted {name}")

    detected = 0
    for name in ("duplicate-key-boundary", "duplicate-owner-boundary"):
        probe = case(document, name)
        actual = parallel(probe, 2, 1, "balanced", broken_local_only=True)
        try:
            graph.exact(actual, sequential(probe), name + ".broken-pipeline")
        except ValueError:
            detected += 1
    graph.require(detected == 2, "self-test: per-partition-only duplicate bug escaped parity")

    script = Path(__file__).resolve()
    with tempfile.TemporaryDirectory(prefix="vlite-import-oracle-", dir=script.parent) as directory:
        result = subprocess.run([sys.executable, "-B", str(script)], cwd=directory,
                                capture_output=True, text=True, timeout=60, check=False)
        graph.require(result.returncode == 0,
                      f"default CLI failed from another cwd: {result.stdout} {result.stderr}")
    return len(negatives), detected


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", type=Path, default=FIXTURE, help="override golden JSON path")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    try:
        document = graph.load_fixture(args.fixture)
        cases, checks = verify_document(document)
        print(f"Verified {cases} hand-authored compact graph import goldens.")
        print(f"Matched {checks} parallel pipelines (6 W x 4 R x 3 partition layouts per case).")
        if args.self_test:
            rejected, detected = self_test(document)
            print(f"Rejected {rejected} malformed or incorrect fixture mutations.")
            print(f"Detected {detected} broken-pipeline parity failures; default CLI passed from another cwd.")
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"{args.fixture}: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
