"""Independent stdlib-only graph node -> vector-record attachment reference.

Run python -B tests\\compact_graph_attachment_oracle.py --self-test.
This is logical test evidence for a future private compact codec, NOT a
serializer, on-disk format, public API, Rust parity proof or performance claim.
Goldens are hand-authored, never generated or rewritten by this tool.

Authoritative source lines in this checkout:
src/property_graph/engine/state.rs:108-109 stores node -> u16 collection -> u64
record; :347-353 reads collections ascending; :512-527 deletes attachments
and releases owners; :777-778,824-832 defines the global inverse owner index;
:835-840 specifies atomic application and adapter-owned record checks;
:1157-1215 checks Attach/Detach in NodeNotFound, NodeAlreadyAttached,
RecordAlreadyAttached order and frees detached records immediately;
:1384-1445 checks dangling attachments, duplicate owners and inverse equality.
src/property_graph/engine/state/tests.rs:806-880 exercises uniqueness across
graphs, detach-then-attach, replacement retention and deletion release.
src/property_graph/engine/state/snapshot.rs:93-98,290-322,458-465 requires graphs
ascending and attachments ascending by (node, collection).
src/property_graph/engine/mutate.rs:1-16 resolves mutations against earlier
tentative operations; vector existence is the adapter's responsibility.
src/property_graph/engine/error.rs:46-68 supplies the exact error names/fields.
src/property_graph/engine/codec.rs:145-149 says reclaim at deadline <= clock
removes expired nodes with all edges and attachments; expiry/TTL is otherwise
out of scope here.

Schema "vlite.compact-graph-attachment-logical" v1:
root = {schema, version, cases, invalid_cases}; valid case =
{name, purpose, input, expected}; invalid case = {name, input, error}, with a
required diagnostic substring. input = {mode: "snapshot"|"batches",
snapshot, batches}. snapshot = {graphs, owners}; graph row =
{graph: u64, nodes: [u64], attachments: [{node: u64, collection: u16,
record: u64}]}; owner row = {collection, record, graph, node}.
The explicit owner index must equal the inverse of ALL attachment maps.
Graphs and node IDs are unique; attachments require existing nodes and one
record per (graph,node,collection); (collection,record) has at most one owner
across all graphs. All integer fields reject booleans and floats.

Snapshot mode requires empty batches and strictly canonical graphs, nodes,
attachments and owners. Batch mode permits unordered starting snapshot rows
(not duplicates), then applies ordered {graph, ops} atomic batches. Operations
are {op:"attach",node,collection,record}, {op:"detach",node,collection},
{op:"put_node",node}, {op:"delete_node",node}. Put creates or replaces, keeping
attachments; delete frees them. First semantic error stops a batch without
changing either maps or owners; all operation schemas are validated first.
Empty logical batches are allowed; batch admission budgets are not modeled.
Graph creation/deletion is excluded: batches must address a listed graph.
expected = {snapshot, outcomes}, canonical final snapshot plus one outcome
per batch: {status:"ok"} or {status:"error",error:<Rust name>,<Rust fields>}.
NodeNotFound's tuple payload is named node in this logical interchange.
Owners ascend by (collection,record); attachments by (node,collection).

The code wins where the requested scope differs: state.rs:766-768,1391-1397
limits graph IDs and checks allocator high-water marks; :780-785 and
error.rs:38-40 prevent allocating u64::MAX. This deliberately models full u64
logical IDs (including 0/MAX) without allocation, keys, revisions or graph
limits, not unrestricted Rust write admission. DeleteNode's edge-incidence
rules (state.rs:1081-1095) are out of scope: all nodes here have no edges.
Vector record existence, vector mutation content and adapter behavior are not
modeled. Snapshot ordering is strict; seeded row shuffles exercise batch-mode
starting snapshots, while shuffled strict snapshots must reject unless their
order is unchanged. Canonicalized shuffles must also preserve every golden.
"""

import argparse
import copy
import json
from pathlib import Path
import random
import subprocess
import sys
import tempfile


FIXTURE = Path(__file__).resolve().parent / "fixtures" / "compact" / "graph-attachment-cases.json"
SCHEMA = "vlite.compact-graph-attachment-logical"
U64_MAX = 2**64 - 1
SHUFFLE_SEEDS = (17, 941, 2026, 65535, 104729)


def require(condition, message):
    if not condition:
        raise ValueError(message)


def keys(value, fields, context):
    require(type(value) is dict and set(value) == set(fields.split()),
            f"{context}: expected keys {fields}")


def integer(value, high, context):
    require(type(value) is int and 0 <= value <= high,
            f"{context}: expected integer in [0, {high}]")


def text(value, context):
    require(type(value) is str and value, f"{context}: expected nonempty UTF-8 string")
    try:
        value.encode("utf-8")
    except UnicodeEncodeError as error:
        raise ValueError(f"{context}: invalid UTF-8 string") from error


def array(value, context):
    require(type(value) is list, f"{context}: expected array")


def increasing(previous, current, canonical, context):
    require(not canonical or previous is None or previous < current,
            f"{context}: noncanonical order")


def inverse(graphs):
    result = {}
    for graph, nodes in graphs.items():
        for node, attachments in nodes.items():
            for collection, record in attachments.items():
                key = (collection, record)
                require(key not in result, "owner conflict: record attached twice")
                result[key] = (graph, node)
    return result


def check_invariants(graphs, owners):
    require(owners == inverse(graphs), "owner index differs from inverse attachment maps")


def read_snapshot(value, canonical, context):
    keys(value, "graphs owners", context)
    array(value["graphs"], f"{context}.graphs")
    graphs, previous_graph = {}, None
    for index, row in enumerate(value["graphs"]):
        location = f"{context}.graphs[{index}]"
        keys(row, "graph nodes attachments", location)
        graph = row["graph"]
        integer(graph, U64_MAX, f"{location}.graph")
        require(graph not in graphs, f"{location}: duplicate graph")
        increasing(previous_graph, graph, canonical, location)
        previous_graph = graph
        array(row["nodes"], f"{location}.nodes")
        nodes, previous_node = {}, None
        for node in row["nodes"]:
            integer(node, U64_MAX, f"{location}.node")
            require(node not in nodes, f"{location}: duplicate node")
            increasing(previous_node, node, canonical, f"{location}.nodes")
            previous_node = node
            nodes[node] = {}
        array(row["attachments"], f"{location}.attachments")
        previous_attachment = None
        for attachment in row["attachments"]:
            keys(attachment, "node collection record", f"{location}.attachment")
            for field, high in (("node", U64_MAX), ("collection", 65535),
                                ("record", U64_MAX)):
                integer(attachment[field], high, f"{location}.attachment.{field}")
            node, collection, record = (attachment[field] for field in
                                        ("node", "collection", "record"))
            require(node in nodes, f"{location}: dangling attachment node {node}")
            require(collection not in nodes[node],
                    f"{location}: duplicate node collection attachment")
            increasing(previous_attachment, (node, collection), canonical,
                       f"{location}.attachments")
            previous_attachment = (node, collection)
            nodes[node][collection] = record
        graphs[graph] = nodes
    array(value["owners"], f"{context}.owners")
    owners, previous_owner = {}, None
    for row in value["owners"]:
        keys(row, "collection record graph node", f"{context}.owner")
        for field in ("collection", "record", "graph", "node"):
            integer(row[field], 65535 if field == "collection" else U64_MAX,
                    f"{context}.owner.{field}")
        key = (row["collection"], row["record"])
        require(key not in owners, f"{context}: duplicate owner")
        increasing(previous_owner, key, canonical, f"{context}.owners")
        previous_owner = key
        owners[key] = (row["graph"], row["node"])
    check_invariants(graphs, owners)
    return graphs, owners


def canonical_snapshot(graphs, owners):
    check_invariants(graphs, owners)
    return {
        "graphs": [
            {"graph": graph, "nodes": sorted(nodes),
             "attachments": [
                 {"node": node, "collection": collection, "record": record}
                 for node, attachments in sorted(nodes.items())
                 for collection, record in sorted(attachments.items())]}
            for graph, nodes in sorted(graphs.items())],
        "owners": [
            {"collection": collection, "record": record, "graph": graph, "node": node}
            for (collection, record), (graph, node) in sorted(owners.items())]}


def validate_input(value, context):
    keys(value, "mode snapshot batches", context)
    require(type(value["mode"]) is str and value["mode"] in ("snapshot", "batches"),
            f"{context}.mode: expected snapshot or batches")
    graphs, owners = read_snapshot(value["snapshot"], value["mode"] == "snapshot", context)
    array(value["batches"], f"{context}.batches")
    require(value["mode"] != "snapshot" or not value["batches"],
            f"{context}: snapshot mode requires empty batches")
    for index, batch in enumerate(value["batches"]):
        location = f"{context}.batches[{index}]"
        keys(batch, "graph ops", location)
        integer(batch["graph"], U64_MAX, f"{location}.graph")
        require(batch["graph"] in graphs, f"{location}: unknown batch graph")
        array(batch["ops"], f"{location}.ops")
        for op in batch["ops"]:
            require(type(op) is dict, f"{location}: expected operation object")
            tag = op.get("op")
            require(type(tag) is str and tag in
                    ("attach", "detach", "put_node", "delete_node"),
                    f"{location}: unknown operation")
            fields = {"attach": "op node collection record", "detach": "op node collection",
                      "put_node": "op node", "delete_node": "op node"}
            keys(op, fields[tag], f"{location}.{tag}")
            for field in fields[tag].split()[1:]:
                integer(op[field], 65535 if field == "collection" else U64_MAX,
                        f"{location}.{tag}.{field}")
    return graphs, owners


def operation(graphs, owners, graph, op):
    nodes, node, tag = graphs[graph], op["node"], op["op"]
    if tag == "put_node":
        nodes.setdefault(node, {})
        return None
    if node not in nodes:
        return {"status": "error", "error": "NodeNotFound", "node": node}
    attachments = nodes[node]
    if tag == "delete_node":
        for collection, record in attachments.items():
            del owners[(collection, record)]
        del nodes[node]
        return None
    collection = op["collection"]
    if tag == "detach":
        if collection not in attachments:
            return {"status": "error", "error": "AttachmentNotFound",
                    "node": node, "collection": collection}
        record = attachments.pop(collection)
        del owners[(collection, record)]
        return None
    if collection in attachments:
        return {"status": "error", "error": "NodeAlreadyAttached",
                "node": node, "collection": collection}
    record = op["record"]
    if (collection, record) in owners:
        owner_graph, owner_node = owners[(collection, record)]
        return {"status": "error", "error": "RecordAlreadyAttached",
                "collection": collection, "record": record,
                "graph": owner_graph, "node": owner_node}
    attachments[collection] = record
    owners[(collection, record)] = (graph, node)
    return None


def evaluate(value, context="input"):
    graphs, owners = validate_input(value, context)
    outcomes = []
    for batch in value["batches"]:
        trial_graphs, trial_owners = copy.deepcopy(graphs), owners.copy()
        outcome = {"status": "ok"}
        for op in batch["ops"]:
            error = operation(trial_graphs, trial_owners, batch["graph"], op)
            check_invariants(trial_graphs, trial_owners)
            if error is not None:
                outcome = error
                break
        if outcome["status"] == "ok":
            graphs, owners = trial_graphs, trial_owners
        check_invariants(graphs, owners)
        outcomes.append(outcome)
    return {"snapshot": canonical_snapshot(graphs, owners), "outcomes": outcomes}


def validate_expected(value, context):
    keys(value, "snapshot outcomes", context)
    read_snapshot(value["snapshot"], True, context)
    array(value["outcomes"], f"{context}.outcomes")
    fields = {"NodeNotFound": "node", "NodeAlreadyAttached": "node collection",
              "AttachmentNotFound": "node collection",
              "RecordAlreadyAttached": "collection record graph node"}
    for outcome in value["outcomes"]:
        require(type(outcome) is dict, f"{context}: expected outcome object")
        if outcome.get("status") == "ok":
            keys(outcome, "status", context)
        else:
            require(outcome.get("status") == "error", f"{context}: invalid outcome status")
            name = outcome.get("error")
            require(type(name) is str and name in fields, f"{context}: unknown error name")
            keys(outcome, "status error " + fields[name], context)
            for field in fields[name].split():
                integer(outcome[field], 65535 if field == "collection" else U64_MAX,
                        f"{context}.{field}")


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
        evaluate(value, context)
    except ValueError as error:
        require(diagnostic in str(error),
                f"{context}: expected rejection {diagnostic!r}, got {str(error)!r}")
    else:
        raise ValueError(f"{context}: malformed input was accepted")


def shuffled_input(value, seed):
    result = copy.deepcopy(value)
    rng = random.Random(seed)
    snapshot = result["snapshot"]
    rng.shuffle(snapshot["graphs"])
    rng.shuffle(snapshot["owners"])
    for graph in snapshot["graphs"]:
        rng.shuffle(graph["nodes"])
        rng.shuffle(graph["attachments"])
    return result


def validate_fixture(path, report=False):
    document = load_fixture(path)
    keys(document, "schema version cases invalid_cases", "fixture")
    require(document["schema"] == SCHEMA, "unsupported fixture schema")
    require(type(document["version"]) is int and document["version"] == 1,
            "version: expected integer 1")
    names, outputs, shuffles = set(), [], 0
    for field in ("cases", "invalid_cases"):
        require(type(document[field]) is list and document[field],
                f"{field}: expected nonempty array")
        for case in document[field]:
            keys(case, "name purpose input expected" if field == "cases" else
                 "name input error", field)
            text(case["name"], f"{field}.name")
            require(case["name"] not in names, "duplicate case name")
            names.add(case["name"])
            if field == "invalid_cases":
                text(case["error"], f"{case['name']}.error")
                expect_rejection(case["input"], case["error"], case["name"])
                continue
            text(case["purpose"], f"{case['name']}.purpose")
            validate_expected(case["expected"], f"{case['name']}.expected")
            actual = evaluate(case["input"], case["name"])
            require(canonical_json(actual) == canonical_json(case["expected"]),
                    f"{case['name']}: result differs from fixed golden")
            for seed in SHUFFLE_SEEDS:
                shuffled = shuffled_input(case["input"], seed)
                if shuffled["mode"] == "snapshot":
                    if shuffled["snapshot"] != case["input"]["snapshot"]:
                        expect_rejection(shuffled, "noncanonical order", case["name"])
                    # Normalization is test setup, not relaxed snapshot admission.
                    graphs, owners = read_snapshot(shuffled["snapshot"], False, case["name"])
                    shuffled["snapshot"] = canonical_snapshot(graphs, owners)
                require(canonical_json(evaluate(shuffled)) == canonical_json(actual),
                        f"{case['name']}: shuffle {seed} changed result")
                shuffles += 1
            outputs.append({"name": case["name"], **actual})
            if report:
                print(f"Verified {case['name']}")
    return document, outputs, shuffles


def self_test(path):
    document, _, shuffles = validate_fixture(path)
    require(len(document["cases"]) >= 5, "self-test requires at least 5 goldens")
    require(len(document["invalid_cases"]) >= 10,
            "self-test requires at least 10 malformed cases")
    mutations = []
    # Sample numeric surfaces, including expected error payloads, reject bad ints.
    targets = []
    for index, case in enumerate(document["cases"]):
        def walk(value, path_parts):
            if type(value) is dict:
                for field, child in value.items():
                    if type(child) is int:
                        targets.append((index, path_parts + [field], field))
                    else:
                        walk(child, path_parts + [field])
            elif type(value) is list:
                for position, child in enumerate(value):
                    if type(child) is int:
                        targets.append((index, path_parts + [position], "node"))
                    else:
                        walk(child, path_parts + [position])
        walk(case["input"], ["input"])
        walk(case["expected"], ["expected"])
    require(len(targets) >= 12, "self-test requires diverse numeric golden surfaces")
    rng = random.Random(4172026)
    rng.shuffle(targets)
    for index, parts, field in targets[:20]:
        high = 65535 if field == "collection" else U64_MAX
        for bad in (True, False, -1, high + 1, "0", None, [], {}):
            broken = copy.deepcopy(document)
            parent = broken["cases"][index]
            for part in parts[:-1]:
                parent = parent[part]
            parent[parts[-1]] = bad
            mutations.append((canonical_json(broken), "expected integer"))

    # Semantically valid mutations must change results, not merely be rejected.
    changed = 0
    transfer = next(case for case in document["cases"]
                    if case["name"] == "detach-then-attach")
    for record in range(1000, 1040):
        value = copy.deepcopy(transfer["input"])
        value["snapshot"]["graphs"][0]["attachments"][0]["record"] = record
        value["snapshot"]["owners"][0]["record"] = record
        value["batches"][0]["ops"][1]["record"] = record
        require(canonical_json(evaluate(value)) != canonical_json(transfer["expected"]),
                "valid record mutation did not change result")
        broken = copy.deepcopy(document)
        index = document["cases"].index(transfer)
        broken["cases"][index]["input"] = value
        mutations.append((canonical_json(broken), "differs from fixed golden"))
        changed += 1

    for contents, diagnostic in (
        ('{"schema":1,"schema":2}', "duplicate JSON key"),
        ('{"snapshot":{"node":0,"node":1}}', "duplicate JSON key"),
        ('{"version":1.0}', "non-integer JSON number"),
        ('{"node":1e0}', "non-integer JSON number"),
        ('{"record":NaN}', "invalid JSON constant"),
        ('{"record":Infinity}', "invalid JSON constant"),
        ('{"record":-Infinity}', "invalid JSON constant"),
        ('{"schema":', "Expecting value"),
        ('[]', "expected keys"),
    ):
        mutations.append((contents, diagnostic))
    with tempfile.TemporaryDirectory(prefix="vlite-attachment-oracle-") as directory:
        fixture = Path(directory) / "mutated.json"
        for index, (contents, diagnostic) in enumerate(mutations):
            fixture.write_text(contents, encoding="utf-8")
            result = subprocess.run(
                [sys.executable, "-B", str(Path(__file__).resolve()),
                 "--fixture", str(fixture)], capture_output=True, text=True, check=False)
            require(result.returncode == 1 and diagnostic in result.stderr,
                    f"mutation {index}: expected CLI rejection {diagnostic!r}; "
                    f"exit={result.returncode}, stderr={result.stderr!r}")
    require(len(mutations) >= 100, "self-test requires at least 100 mutations")
    print(f"Self-test: {len(document['cases'])} goldens, "
          f"{len(document['invalid_cases'])} malformed cases, {shuffles} seeded shuffles; "
          f"{len(mutations)} deterministic mutations rejected by CLI "
          f"({changed} valid mutations changed results)")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", type=Path, default=FIXTURE)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--snapshot", action="store_true",
                        help="print canonical logical snapshots and batch outcomes")
    args = parser.parse_args()
    try:
        document, outputs, shuffles = validate_fixture(args.fixture, report=not args.snapshot)
        if args.snapshot:
            print(canonical_json(outputs))
        else:
            print(f"Verified {len(outputs)} attachment cases, "
                  f"{len(document['invalid_cases'])} malformed cases, "
                  f"{shuffles} seeded shuffles: {args.fixture}")
        if args.self_test:
            self_test(args.fixture)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"{args.fixture}: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
