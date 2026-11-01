"""Independent Python 3 stdlib logical reference for Gap 4 temporal reads.

Run python -B tests\\temporal_read_oracle.py --self-test. Fixtures are resolved
beside __file__; --fixture overrides them. This is not an implementation, byte
format, or public API. Edge/property filters, types filters, return filters,
retrieve, cursors and query text are OUT OF SCOPE. Output caps, cancellation,
elapsed-time deadlines, revisions, properties and persistence are not modeled.

Sources: gap4-temporal-graph.md section 2; engine/traverse.rs traverse/search,
Layers::canonical/path and other_end; engine/state.rs adjacency merge. Both
directions merge by (relationship UTF-8 bytes, edge ID), self-loops once.
Starts are deduplicated in first-occurrence order. Query anchors must exist
and be alive at C (fixture-local validation, not public API errors).

Input has exact nodes/edges/queries keys; optional deadline may be absent/null,
optional segments absent means legacy all-time, [] means never visible.
Segments have exact from/to keys: sorted non-overlapping half-open signed i64
bounds, null infinities, at most 256. IDs, C and TTL deadlines are u64 integers.
Queries have op/T/C/direction and node (neighbors), start (traverse), or
source/destination (shortest). Walks also require max_hops (0..4),
max_visited_nodes (1..10000), max_edges_per_node (1..10000), budget (1..100000).
Budget counts examined entries, including temporally/TTL-hidden entries.
Traversal paths omit depth-zero anchors. Truncation records visited, degree
(first encountered first, at most 64), and budget; max_hops is not truncation.
Shortest records no non-destination last-layer nodes and fully examines the
arrival layer before certifying a lexicographically canonical edge-ID path.

Schema vlite.temporal-read-logical v1 has hand-authored cases and malformed raw
JSON inputs with exact fixture-local errors. No expectations are generated or
written. Self-test checks goldens, five seeded input shuffles per golden, raw
malformed reasons, 256/257 segment-limit inputs, and 160 seeded mutations.
"""

import argparse
import copy
import json
from pathlib import Path
import random
import sys


FIXTURE = Path(__file__).resolve().parent / "fixtures" / "temporal_intervals" / "read-cases.json"
SCHEMA = "vlite.temporal-read-logical"
U64_MAX = 2**64 - 1
I64_MIN, I64_MAX = -(2**63), 2**63 - 1
SHUFFLE_SEEDS = (17, 941, 2026, 73, 65537)


def require(condition, message):
    if not condition:
        raise ValueError(message)


def keys(value, required, context, optional=""):
    wanted, allowed = set(required.split()), set(optional.split())
    require(isinstance(value, dict) and wanted <= set(value) <= wanted | allowed,
            f"{context}: expected keys {required} (optional: {optional})")


def integer(value, low, high, context, kind):
    require(type(value) is int and low <= value <= high,
            f"{context}: expected {kind} integer")


def u64(value, context):
    integer(value, 0, U64_MAX, context, "u64")


def timestamp(value, context):
    integer(value, I64_MIN, I64_MAX, context, "i64")


def text(value, context):
    require(isinstance(value, str) and bool(value),
            f"{context}: expected nonempty UTF-8 string")
    try:
        value.encode("utf-8")
    except UnicodeEncodeError as error:
        raise ValueError(f"{context}: invalid UTF-8 string") from error


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, f"duplicate JSON key {key!r}")
        result[key] = value
    return result


def reject_float(value):
    raise ValueError(f"non-integer JSON number {value!r}")


def reject_constant(value):
    raise ValueError(f"invalid JSON constant {value!r}")


def parse_json(raw):
    return json.loads(raw, object_pairs_hook=unique_object,
                      parse_float=reject_float, parse_constant=reject_constant)


def segments_valid(segments, context):
    require(isinstance(segments, list), f"{context}: expected array")
    require(len(segments) <= 256, f"{context}: at most 256 segments")
    previous = None
    for index, segment in enumerate(segments):
        location = f"{context}[{index}]"
        keys(segment, "from to", location)
        start, end = segment["from"], segment["to"]
        for field in ("from", "to"):
            if segment[field] is not None:
                timestamp(segment[field], f"{location}.{field}")
        require(start is None or end is None or start < end,
                f"{location}: from must precede to")
        if previous is not None:
            prior_start, prior_end = previous
            require(start is not None and (prior_start is None or start >= prior_start),
                    f"{location}: segments must be sorted")
            require(prior_end is not None and start >= prior_end,
                    f"{location}: segments overlap")
        previous = start, end


def alive(record, clock):
    return record.get("deadline") is None or clock < record["deadline"]


def validate_input(value):
    keys(value, "nodes edges queries", "input")
    nodes, edge_ids = {}, set()
    for field in ("nodes", "edges"):
        require(isinstance(value[field], list), f"input.{field}: expected array")
        for index, record in enumerate(value[field]):
            location = f"input.{field}[{index}]"
            keys(record, "id" if field == "nodes" else "id source target relationship",
                 location, "deadline" if field == "nodes" else "deadline segments")
            u64(record["id"], f"{location}.id")
            seen = nodes if field == "nodes" else edge_ids
            require(record["id"] not in seen, f"{location}: duplicate ID")
            if record.get("deadline") is not None:
                u64(record["deadline"], f"{location}.deadline")
            if field == "nodes":
                nodes[record["id"]] = record
            else:
                edge_ids.add(record["id"])
                for endpoint in ("source", "target"):
                    u64(record[endpoint], f"{location}.{endpoint}")
                    require(record[endpoint] in nodes, f"{location}: unknown endpoint")
                text(record["relationship"], f"{location}.relationship")
                if "segments" in record:
                    segments_valid(record["segments"], f"{location}.segments")
    queries = value["queries"]
    require(isinstance(queries, list) and bool(queries),
            "input.queries: expected nonempty array")
    for index, query in enumerate(queries):
        location = f"input.queries[{index}]"
        require(isinstance(query, dict), f"{location}: expected object")
        op = query.get("op")
        require(isinstance(op, str) and op in ("neighbors", "traverse", "shortest"),
                f"{location}: invalid op")
        fields = {"neighbors": "node", "traverse": "start",
                  "shortest": "source destination"}[op]
        walk = "" if op == "neighbors" else (
            " max_hops max_visited_nodes max_edges_per_node budget")
        keys(query, "op T C direction " + fields + walk, location)
        timestamp(query["T"], f"{location}.T")
        u64(query["C"], f"{location}.C")
        require(isinstance(query["direction"], str)
                and query["direction"] in ("out", "in", "both"),
                f"{location}: invalid direction")
        if op == "traverse":
            require(isinstance(query["start"], list) and bool(query["start"]),
                    f"{location}.start: expected nonempty array")
            require(len(query["start"]) <= 10000, f"{location}.start: at most 10000 entries")
            anchors = query["start"]
        else:
            anchors = [query[field] for field in fields.split()]
        for anchor in anchors:
            u64(anchor, f"{location}.anchor")
            require(anchor in nodes, f"{location}: unknown anchor")
            require(alive(nodes[anchor], query["C"]), f"{location}: anchor expired at C")
        if op != "neighbors":
            for field, low, high in (("max_hops", 0, 4), ("max_visited_nodes", 1, 10000),
                                     ("max_edges_per_node", 1, 10000), ("budget", 1, 100000)):
                integer(query[field], low, high, f"{location}.{field}", f"[{low},{high}]")


def adjacency(edges, node, direction):
    # A union of records implements the engine merge without duplicating loops.
    return sorted((edge for edge in edges
                   if (direction != "in" and edge["source"] == node)
                   or (direction != "out" and edge["target"] == node)),
                  key=lambda edge: (edge["relationship"].encode("utf-8"), edge["id"]))


def visible(edge, nodes, query):
    if not all(alive(record, query["C"])
               for record in (edge, nodes[edge["source"]], nodes[edge["target"]])):
        return False
    return "segments" not in edge or any(
        (segment["from"] is None or segment["from"] <= query["T"])
        and (segment["to"] is None or query["T"] < segment["to"])
        for segment in edge["segments"])


def other_end(edge, node):
    return edge["target"] if edge["source"] == node else edge["source"]


class Layers:
    def __init__(self):
        self.order = []
        self.index = {}
        self.preds = []

    def visit(self, node, depth):
        position = len(self.order)
        self.index[node] = position
        self.order.append((node, depth))
        self.preds.append([])
        return position

    def canonical(self):
        best = []
        for position, preds in enumerate(self.preds):
            if not preds:
                best.append((position, []))
            else:
                parent, edge = min(preds, key=lambda pair: (best[pair[0]][1], pair[1]))
                best.append((parent, best[parent][1] + [edge]))
        return best

    def path(self, best, position):
        edges = best[position][1]
        nodes = [self.order[position][0]]
        while best[position][0] != position:
            position = best[position][0]
            nodes.append(self.order[position][0])
        return {"nodes": list(reversed(nodes)), "edges": edges}


class Walk:
    def __init__(self, value, query):
        self.nodes = {node["id"]: node for node in value["nodes"]}
        self.edges = value["edges"]
        self.query = query
        self.layers = Layers()
        self.spent = 0
        self.flags = {"visited": False, "degree": [], "budget": False}

    def entries(self, node):
        return adjacency(self.edges, node, self.query["direction"])

    def examine(self, node, index):
        if index >= self.query["max_edges_per_node"]:
            if node not in self.flags["degree"] and len(self.flags["degree"]) < 64:
                self.flags["degree"].append(node)
            return False
        if self.spent >= self.query["budget"]:
            self.flags["budget"] = True
            return False
        self.spent += 1
        return True

    def full(self):
        return len(self.layers.order) >= self.query["max_visited_nodes"]

    def result(self, content, arrivals=False):
        return dict(content, truncation=self.flags, edges_examined=self.spent,
                    nodes_visited=len(self.layers.order) + int(arrivals))

    def traverse(self):
        layers, edges, edge_ids = self.layers, [], set()
        seeded = True
        for node in dict.fromkeys(self.query["start"]):
            if self.full():
                self.flags["visited"] = True
                seeded = False
                break
            layers.visit(node, 0)
        head, stopped = 0, False
        while seeded and not stopped and head < len(layers.order):
            current = head
            node, depth = layers.order[head]
            head += 1
            if depth >= self.query["max_hops"]:
                break
            for index, edge in enumerate(self.entries(node)):
                if not self.examine(node, index):
                    stopped = self.flags["budget"]
                    break
                if not visible(edge, self.nodes, self.query):
                    continue
                next_node = other_end(edge, node)
                position = layers.index.get(next_node)
                if position is None:
                    if self.full():
                        self.flags["visited"] = True
                        stopped = True
                        break
                    position = layers.visit(next_node, depth + 1)
                if edge["id"] not in edge_ids:
                    edge_ids.add(edge["id"])
                    edges.append(edge["id"])
                if layers.order[position][1] == depth + 1:
                    layers.preds[position].append((current, edge["id"]))
        best = layers.canonical()
        return self.result({
            "nodes": [{"id": node, "depth": depth} for node, depth in layers.order],
            "edges": edges,
            "paths": [layers.path(best, position)
                      for position, (_, depth) in enumerate(layers.order) if depth != 0]})

    def shortest(self):
        query, layers = self.query, self.layers
        source, destination = query["source"], query["destination"]
        layers.visit(source, 0)
        if source == destination:
            return self.result({"outcome": "found", "path": {"nodes": [source], "edges": []}})
        arrivals = []
        begin, end = 0, len(layers.order)
        for depth in range(query["max_hops"]):
            last = depth + 1 == query["max_hops"]
            for current in range(begin, end):
                node = layers.order[current][0]
                for index, edge in enumerate(self.entries(node)):
                    if not self.examine(node, index):
                        return self.result({"outcome": "exhausted"}, bool(arrivals))
                    if not visible(edge, self.nodes, query):
                        continue
                    next_node = other_end(edge, node)
                    if next_node == destination:
                        if not arrivals and self.full():
                            self.flags["visited"] = True
                            return self.result({"outcome": "exhausted"})
                        arrivals.append((current, edge["id"]))
                        continue
                    if last or arrivals:
                        continue
                    position = layers.index.get(next_node)
                    if position is None:
                        if self.full():
                            self.flags["visited"] = True
                            return self.result({"outcome": "exhausted"})
                        position = layers.visit(next_node, depth + 1)
                    if layers.order[position][1] == depth + 1:
                        layers.preds[position].append((current, edge["id"]))
            if arrivals:
                best = layers.canonical()
                parent, edge = min(arrivals, key=lambda pair: (best[pair[0]][1], pair[1]))
                path = layers.path(best, parent)
                path["nodes"].append(destination)
                path["edges"] = path["edges"] + [edge]
                return self.result({"outcome": "found", "path": path}, True)
            if end == len(layers.order):
                return self.result({"outcome": "no_path"})
            begin, end = end, len(layers.order)
        return self.result({"outcome": "no_path"})


def evaluate(value):
    validate_input(value)
    nodes = {node["id"]: node for node in value["nodes"]}
    results = []
    for query in value["queries"]:
        if query["op"] == "neighbors":
            results.append({"edges": [edge["id"] for edge in adjacency(
                value["edges"], query["node"], query["direction"])
                if visible(edge, nodes, query)]})
        else:
            walk = Walk(value, query)
            results.append(walk.traverse() if query["op"] == "traverse" else walk.shortest())
    return results


def id_array(value, context):
    require(isinstance(value, list), f"{context}: expected array")
    for item in value:
        u64(item, context)


def validate_path(value, context):
    keys(value, "nodes edges", context)
    for field in ("nodes", "edges"):
        id_array(value[field], f"{context}.{field}")
    require(len(value["nodes"]) == len(value["edges"]) + 1,
            f"{context}: path lengths disagree")


def validate_expected(expected, queries):
    require(isinstance(expected, list) and len(expected) == len(queries),
            "expected: result count mismatch")
    for index, (result, query) in enumerate(zip(expected, queries)):
        location = f"expected[{index}]"
        op = query["op"]
        if op == "neighbors":
            keys(result, "edges", location)
            id_array(result["edges"], f"{location}.edges")
            continue
        common = "truncation edges_examined nodes_visited"
        if op == "traverse":
            keys(result, "nodes edges paths " + common, location)
            require(isinstance(result["nodes"], list), f"{location}.nodes: expected array")
            for node in result["nodes"]:
                keys(node, "id depth", f"{location}.nodes")
                u64(node["id"], f"{location}.nodes.id")
                integer(node["depth"], 0, 4, f"{location}.nodes.depth", "[0,4]")
            id_array(result["edges"], f"{location}.edges")
            require(isinstance(result["paths"], list), f"{location}.paths: expected array")
            for path in result["paths"]:
                validate_path(path, f"{location}.paths")
        else:
            require(isinstance(result, dict) and isinstance(result.get("outcome"), str)
                    and result["outcome"] in ("found", "no_path", "exhausted"),
                    f"{location}: invalid outcome")
            keys(result, "outcome " + common + (" path" if result["outcome"] == "found" else ""),
                 location)
            if result["outcome"] == "found":
                validate_path(result["path"], f"{location}.path")
        keys(result["truncation"], "visited degree budget", f"{location}.truncation")
        for field in ("visited", "budget"):
            require(type(result["truncation"][field]) is bool,
                    f"{location}.truncation.{field}: expected boolean")
        id_array(result["truncation"]["degree"], f"{location}.truncation.degree")
        for field in ("edges_examined", "nodes_visited"):
            u64(result[field], f"{location}.{field}")


def same_json(left, right):
    return json.dumps(left, sort_keys=True, allow_nan=False) == json.dumps(
        right, sort_keys=True, allow_nan=False)


def check_case(case):
    keys(case, "name purpose input expected", "case")
    text(case["name"], "case.name")
    text(case["purpose"], "case.purpose")
    actual = evaluate(case["input"])
    validate_expected(case["expected"], case["input"]["queries"])
    require(same_json(actual, case["expected"]),
            f"{case['name']}: result mismatch: expected {case['expected']!r}, got {actual!r}")


def check_fixture(document):
    keys(document, "schema version cases malformed", "root")
    require(document["schema"] == SCHEMA, "schema: unsupported fixture schema")
    require(type(document["version"]) is int and document["version"] == 1,
            "version: expected integer 1")
    for field, minimum in (("cases", 6), ("malformed", 15)):
        require(isinstance(document[field], list) and len(document[field]) >= minimum,
                f"{field}: expected at least {minimum} cases")
    names = set()
    queries = 0
    for case in document["cases"]:
        check_case(case)
        require(case["name"] not in names, "case: duplicate name")
        names.add(case["name"])
        queries += len(case["input"]["queries"])
    for case in document["malformed"]:
        keys(case, "name input_json expected_error", "malformed")
        for field in ("name", "input_json", "expected_error"):
            text(case[field], f"malformed.{field}")
        require(case["name"] not in names, "case: duplicate name")
        names.add(case["name"])
        try:
            evaluate(parse_json(case["input_json"]))
        except ValueError as error:
            require(str(error) == case["expected_error"],
                    f"{case['name']}: expected error {case['expected_error']!r}, got {str(error)!r}")
        else:
            raise ValueError(f"{case['name']}: malformed input accepted")
    return queries


def self_test(document):
    queries = check_fixture(document)
    # Only inputs are expanded; acceptance and the exact error are hand chosen.
    segments = [{"from": index, "to": index + 1} for index in range(256)]
    segments_valid(segments, "limit")
    try:
        segments_valid(segments + [{"from": 256, "to": 257}], "limit")
    except ValueError as error:
        require(str(error) == "limit: at most 256 segments", "segment limit: wrong error")
    else:
        raise ValueError("segment limit: accepted 257 segments")
    shuffles = 0
    for case in document["cases"]:
        for seed in SHUFFLE_SEEDS:
            shuffled = copy.deepcopy(case)
            rng = random.Random(seed)
            rng.shuffle(shuffled["input"]["nodes"])
            rng.shuffle(shuffled["input"]["edges"])
            check_case(shuffled)
            shuffles += 1
    # The first golden deliberately exposes both a temporal and a legacy edge.
    base = document["cases"][0]
    require(base["expected"][0] == {"edges": [0, 5]}, "mutation anchor: expected edges [0,5]")
    rng = random.Random(4704)
    rejected, changed = 0, 0
    distinct = set()
    for index in range(160):
        kind = index % 16
        case = copy.deepcopy(rng.choice(document["cases"]) if kind < 8 or kind >= 14 else base)
        value = case["input"]
        query = rng.choice(value["queries"]) if kind < 8 else value["queries"][0]
        edge, node = value["edges"][0], value["nodes"][0]
        if kind == 0:
            query["T"] = True
        elif kind == 1:
            query["C"] = -rng.randint(1, 1000)
        elif kind == 2:
            node["id"] = U64_MAX + rng.randint(1, 1000)
        elif kind == 3:
            value["nodes"].append(copy.deepcopy(node))
        elif kind == 4:
            edge["source"] = 1000 + rng.randrange(1000)
        elif kind == 5:
            edge["relationship"] = ""
        elif kind == 6:
            edge["segments"] = [{"from": rng.randrange(1000), "to": False}]
        elif kind == 7:
            query["unexpected"] = rng.randrange(1000)
        elif kind == 8:
            edge["deadline"] = rng.randrange(6)
        elif kind == 9:
            edge["segments"] = [{"from": 1000 + rng.randrange(1000), "to": None}]
        elif kind == 10:
            edge["id"] = 1000 + rng.randrange(1000)
        elif kind == 11:
            edge["relationship"] = "z" + str(rng.randrange(1000))
        elif kind == 12:
            value["nodes"][1]["deadline"] = rng.randrange(6)
        elif kind == 13:
            query["T"] = 1000 + rng.randrange(1000)
        elif kind == 14:
            result = rng.choice(case["expected"])
            if "edges_examined" in result:
                result["edges_examined"] += rng.randint(1, 1000)
            else:
                result["edges"].append(1000 + rng.randrange(1000))
        else:
            result = rng.choice(case["expected"])
            if "paths" in result and result["paths"]:
                result["paths"][0]["edges"][0] = 1000 + rng.randrange(1000)
            elif "path" in result and result["path"]["edges"]:
                result["path"]["edges"][0] = 1000 + rng.randrange(1000)
            elif "nodes_visited" in result:
                result["nodes_visited"] += rng.randint(1, 1000)
            else:
                result["edges"].append(1000 + rng.randrange(1000))
        distinct.add(json.dumps(case, sort_keys=True, allow_nan=False))
        try:
            actual = evaluate(value)
            validate_expected(case["expected"], value["queries"])
        except ValueError:
            rejected += 1
        else:
            require(not same_json(actual, case["expected"]),
                    f"mutation {index}: undetected input/expectation change")
            changed += 1
    require(len(distinct) >= 100, "mutations: fewer than 100 distinct mutations")
    print(f"goldens: {len(document['cases'])}; queries: {queries}; "
          f"malformed: {len(document['malformed'])}; shuffles: {shuffles}; "
          f"segment limits: 2; mutations: {rejected + changed} "
          f"({rejected} rejected, {changed} changed, {len(distinct)} distinct)")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", type=Path, default=FIXTURE)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    try:
        document = parse_json(args.fixture.read_text(encoding="utf-8"))
        if args.self_test:
            self_test(document)
        else:
            count = check_fixture(document)
            print(f"goldens: {len(document['cases'])}; queries: {count}; "
                  f"malformed: {len(document['malformed'])}")
    except (OSError, ValueError) as error:
        print(f"temporal read oracle: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
