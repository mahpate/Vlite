"""Independent, stdlib-only seeded sampled betweenness reference.

Run `python tests\\graph_betweenness_oracle.py [--self-test] [--fixture PATH]`.
Relative fixture paths resolve beside this script, independent of the working
directory. The default is fixtures\\graph_algorithms\\betweenness-cases.json.
Exact rational scores are the reference; f64_hex and f64_repr describe one final
conversion to binary64, not bitwise parity with iterative f64 Rust reductions.
interrupt_after_sources is a deterministic interruption before the next source
can commit; that source contributes nothing, including partially computed work.
This models already-expired flags, not database TTL evaluation or work budgets.
"""

import argparse
from collections import deque
from copy import deepcopy
from fractions import Fraction
import json
from pathlib import Path
import sys


SCHEMA = "vlite.graph-betweenness-golden"
VERSION = 1
U64_MAX = (1 << 64) - 1
SPLITMIX64_VECTORS = (
    (0, "0xe220a8397b1dcdaf"),
    (1, "0x910a2dec89025cc1"),
    (2, "0x975835de1c9756ce"),
    (3, "0x1d0b14e4db018fed"),
    (4, "0x6e73e372e2338aca"),
    (5, "0x63033b0ca389c35a"),
    (U64_MAX, "0xe4d971771b652c20"),
)


def require(condition, message):
    if not condition:
        raise ValueError(message)


def keys(value, required, context, optional=""):
    require(type(value) is dict, f"{context}: expected object")
    names, extras = set(required.split()), set(optional.split())
    require(names <= value.keys() and value.keys() <= names | extras,
            f"{context}: expected fields {required}; optional {optional}")


def integer(value, minimum, maximum, context):
    require(type(value) is int and value >= minimum
            and (maximum is None or value <= maximum), f"{context}: invalid integer")


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, f"duplicate JSON key {key!r}")
        result[key] = value
    return result


def reject_constant(value):
    raise ValueError(f"invalid JSON constant {value!r}")


def load(text):
    return json.loads(text, object_pairs_hook=unique_object, parse_constant=reject_constant)


def splitmix64(value):
    integer(value, 0, U64_MAX, "SplitMix64 input")
    x = (value + 0x9E3779B97F4A7C15) & U64_MAX
    x = ((x ^ (x >> 30)) * 0xBF58476D1CE4E5B9) & U64_MAX
    x = ((x ^ (x >> 27)) * 0x94D049BB133111EB) & U64_MAX
    return (x ^ (x >> 31)) & U64_MAX


def project(case):
    """Validate physical records, then induce a simple visible arc graph."""
    keys(case, "id description nodes edges direction seed samples expected", "case",
         "interrupt_after_sources")
    for field in ("id", "description"):
        require(type(case[field]) is str and case[field].strip(), f"case: invalid {field}")
    require(case["direction"] in ("Out", "In", "Both"), "case: bad direction")
    integer(case["seed"], 0, U64_MAX, "seed")
    integer(case["samples"], 0, None, "samples")
    if "interrupt_after_sources" in case:
        integer(case["interrupt_after_sources"], 0, None, "interrupt_after_sources")
    require(type(case["nodes"]) is list and len(case["nodes"]) <= 8,
            "case: expected at most eight nodes")
    require(type(case["edges"]) is list and len(case["edges"]) <= 32,
            "case: expected at most 32 edges")
    by_id = {}
    for node in case["nodes"]:
        keys(node, "id", "node", "expired")
        integer(node["id"], 0, U64_MAX, "node id")
        require(node["id"] not in by_id, "duplicate node id")
        if "expired" in node:
            require(type(node["expired"]) is bool, "node: expired must be boolean")
        by_id[node["id"]] = node
    nodes = sorted(node for node, record in by_id.items() if not record.get("expired", False))
    adjacency = {node: set() for node in nodes}
    edge_ids = set()
    for edge in case["edges"]:
        keys(edge, "id source target", "edge", "expired")
        for field in ("id", "source", "target"):
            integer(edge[field], 0, U64_MAX, f"edge {field}")
        require(edge["id"] not in edge_ids, "duplicate edge id")
        edge_ids.add(edge["id"])
        require(edge["source"] in by_id and edge["target"] in by_id, "unknown edge endpoint")
        if "expired" in edge:
            require(type(edge["expired"]) is bool, "edge: expired must be boolean")
        u, v = edge["source"], edge["target"]
        if edge.get("expired", False) or u not in adjacency or v not in adjacency or u == v:
            continue
        if case["direction"] == "In":
            u, v = v, u
        adjacency[u].add(v)
        if case["direction"] == "Both":
            adjacency[v].add(u)
    return nodes, {node: sorted(neighbors) for node, neighbors in adjacency.items()}


def source_dependencies(source, nodes, adjacency):
    """Unweighted Brandes with rational sigma and dependency accumulation."""
    predecessors = {node: [] for node in nodes}
    sigma = dict.fromkeys(nodes, Fraction(0))
    sigma[source] = Fraction(1)
    distance = {source: 0}
    queue, stack = deque([source]), []
    while queue:
        v = queue.popleft()
        stack.append(v)
        for w in adjacency[v]:
            if w not in distance:
                distance[w] = distance[v] + 1
                queue.append(w)
            if distance[w] == distance[v] + 1:
                sigma[w] += sigma[v]
                predecessors[w].append(v)
    dependency = dict.fromkeys(nodes, Fraction(0))
    for w in reversed(stack):
        for v in predecessors[w]:
            dependency[v] += sigma[v] / sigma[w] * (Fraction(1) + dependency[w])
    dependency[source] = Fraction(0)
    return dependency


def fraction_text(value):
    return f"{value.numerator}/{value.denominator}"


def evaluate(case):
    nodes, adjacency = project(case)
    n = len(nodes)
    requested = min(case["samples"], n)
    selected = sorted(nodes, key=lambda node: (splitmix64(case["seed"] ^ node), node))[:requested]
    completed = selected[:case.get("interrupt_after_sources", requested)]
    k = len(completed)
    raw = dict.fromkeys(nodes, Fraction(0))
    for source in completed:
        dependency = source_dependencies(source, nodes, adjacency)
        for node in nodes:
            raw[node] += dependency[node]
    if k == 0:
        status = "no_scores"
    elif k == n:
        status = "exact"
    elif k == requested:
        status = "complete_estimated"
    else:
        status = "partial_estimated"
    scores = None
    if k:
        scores = []
        for node in nodes:
            score = (Fraction(n, k) * raw[node] / ((n - 1) * (n - 2))
                     if n > 2 else Fraction(0))
            binary64 = float(score)
            scores.append({"node_id": node, "exact": fraction_text(score),
                           "f64_hex": binary64.hex(), "f64_repr": repr(binary64)})
    return {"selected": selected, "completed": completed, "k": k, "n": n, "status": status,
            "raw_sums": [{"node_id": node, "exact": fraction_text(raw[node])} for node in nodes],
            "scores": scores}


def compare(actual, expected, context):
    require(type(actual) is type(expected), f"{context}: type mismatch")
    if type(actual) is dict:
        require(actual.keys() == expected.keys(), f"{context}: field mismatch")
        for key in actual:
            compare(actual[key], expected[key], f"{context}.{key}")
    elif type(actual) is list:
        require(len(actual) == len(expected), f"{context}: length mismatch")
        for index, (left, right) in enumerate(zip(actual, expected)):
            compare(left, right, f"{context}[{index}]")
    else:
        require(actual == expected, f"{context}: expected {expected!r}, oracle {actual!r}")


def validate(data):
    keys(data, "schema version contract splitmix64_vectors cases", "fixture")
    require(data["schema"] == SCHEMA, "unsupported schema")
    integer(data["version"], VERSION, VERSION, "version")
    keys(data["contract"], "description ordering arithmetic interruption exclusions", "contract")
    require(all(type(value) is str and value.strip() for value in data["contract"].values()),
            "contract: expected nonempty strings")
    compare([{"input": value, "output": output} for value, output in SPLITMIX64_VECTORS],
            data["splitmix64_vectors"], "SplitMix64 vectors")
    for value, output in SPLITMIX64_VECTORS:
        require(splitmix64(value) == int(output, 16), f"SplitMix64({value}): vector mismatch")
    require(type(data["cases"]) is list and data["cases"], "fixture: empty cases")
    seen = set()
    for case in data["cases"]:
        actual = evaluate(case)
        require(case["id"] not in seen, "duplicate case id")
        seen.add(case["id"])
        compare(actual, case["expected"], case["id"])
    return len(seen)


def exhaustive_dependencies(source, nodes, adjacency):
    """Independent check: enumerate simple paths, then count shortest winners."""
    totals = dict.fromkeys(nodes, Fraction(0))
    for target in nodes:
        if target == source:
            continue
        paths = []

        def walk(path):
            if path[-1] == target:
                paths.append(path)
                return
            for neighbor in adjacency[path[-1]]:
                if neighbor not in path:
                    walk(path + (neighbor,))

        walk((source,))
        if not paths:
            continue
        shortest = min(map(len, paths))
        winners = [path for path in paths if len(path) == shortest]
        for node in nodes:
            totals[node] += Fraction(sum(node in path[1:-1] for path in winners), len(winners))
    return totals


def self_test(data):
    for case in data["cases"]:
        nodes, adjacency = project(case)
        for source in nodes:
            compare(source_dependencies(source, nodes, adjacency),
                    exhaustive_dependencies(source, nodes, adjacency), f"{case['id']} source {source}")
    path = {"id": "analytical", "description": "Spec golden", "nodes": [{"id": i} for i in (1, 2, 3)],
            "edges": [{"id": 1, "source": 1, "target": 2}, {"id": 2, "source": 2, "target": 3}],
            "direction": "Out", "seed": 0, "samples": 3, "expected": {}}
    compare([row["exact"] for row in evaluate(path)["scores"]], ["0/1", "1/2", "0/1"],
            "authoritative A->B->C golden")
    path["direction"] = "Both"
    compare([row["exact"] for row in evaluate(path)["scores"]], ["0/1", "1/1", "0/1"],
            "Both does not divide by two again")

    baseline = deepcopy(data)
    base = deepcopy(path)
    base.update(id="mutation_baseline", description="Five-node path for negative tests",
                nodes=[{"id": i} for i in range(1, 6)],
                edges=[{"id": i, "source": i, "target": i + 1} for i in range(1, 5)],
                samples=5)
    base["expected"] = evaluate(base)
    baseline["cases"] = [base]
    validate(baseline)
    mutations = []

    def mutate(label, location, value=None, delete=False):
        broken = deepcopy(baseline)
        parent = broken
        for part in location[:-1]:
            parent = parent[part]
        if delete:
            del parent[location[-1]]
        else:
            parent[location[-1]] = value
        mutations.append((label, json.dumps(broken)))

    c = ["cases", 0]
    e = c + ["expected"]
    for field in baseline:
        mutate(f"missing fixture {field}", [field], delete=True)
    for field in baseline["contract"]:
        mutate(f"missing contract {field}", ["contract", field], delete=True)
    for field in base:
        mutate(f"missing case {field}", c + [field], delete=True)
    for field in base["expected"]:
        mutate(f"missing expected {field}", e + [field], delete=True)
    for field in base["nodes"][0]:
        mutate(f"missing node {field}", c + ["nodes", 0, field], delete=True)
    for field in base["edges"][0]:
        mutate(f"missing edge {field}", c + ["edges", 0, field], delete=True)
    for collection in ("raw_sums", "scores"):
        for field in base["expected"][collection][0]:
            mutate(f"missing {collection} {field}", e + [collection, 0, field], delete=True)
    for field in ("input", "output"):
        mutate(f"missing vector {field}", ["splitmix64_vectors", 0, field], delete=True)
    values = [
        ("selected order", e + ["selected"], list(reversed(base["expected"]["selected"]))),
        ("completed order", e + ["completed"], list(reversed(base["expected"]["completed"]))),
        ("raw sum", e + ["raw_sums", 1, "exact"], "999/1"),
        ("raw sum unreduced", e + ["raw_sums", 0, "exact"], "0/2"),
        ("score rational", e + ["scores", 1, "exact"], "1/7"),
        ("f64 hex", e + ["scores", 1, "f64_hex"], "0x0.0p+0"),
        ("f64 repr", e + ["scores", 1, "f64_repr"], "0.0"),
        ("status", e + ["status"], "complete_estimated"),
        ("k", e + ["k"], 4), ("boolean k", e + ["k"], True),
        ("n", e + ["n"], 4), ("null scores", e + ["scores"], None),
        ("score node", e + ["scores", 0, "node_id"], 99),
        ("bad direction", c + ["direction"], "out"),
        ("direction type", c + ["direction"], []),
        ("duplicate node", c + ["nodes", 1, "id"], 1),
        ("duplicate edge", c + ["edges", 1, "id"], 1),
        ("unknown source", c + ["edges", 0, "source"], 99),
        ("unknown target", c + ["edges", 0, "target"], 99),
        ("negative node", c + ["nodes", 0, "id"], -1),
        ("oversized node", c + ["nodes", 0, "id"], 1 << 64),
        ("boolean node", c + ["nodes", 0, "id"], True),
        ("negative edge", c + ["edges", 0, "id"], -1),
        ("boolean endpoint", c + ["edges", 0, "source"], True),
        ("node expired type", c + ["nodes", 0, "expired"], 1),
        ("edge expired type", c + ["edges", 0, "expired"], "false"),
        ("negative seed", c + ["seed"], -1), ("oversized seed", c + ["seed"], 1 << 64),
        ("float seed", c + ["seed"], 0.0), ("boolean seed", c + ["seed"], False),
        ("string seed", c + ["seed"], "0"),
        ("negative samples", c + ["samples"], -1),
        ("float samples", c + ["samples"], 5.0), ("boolean samples", c + ["samples"], True),
        ("negative interruption", c + ["interrupt_after_sources"], -1),
        ("float interruption", c + ["interrupt_after_sources"], 1.0),
        ("boolean interruption", c + ["interrupt_after_sources"], True),
        ("interrupted source credited", c + ["interrupt_after_sources"], 1),
        ("zero sample credited", c + ["samples"], 0),
        ("expired node credited", c + ["nodes", 1, "expired"], True),
        ("expired edge credited", c + ["edges", 1, "expired"], True),
        ("unknown case field", c + ["unexpected"], 0),
        ("unknown expected field", e + ["unexpected"], 0),
        ("unknown node field", c + ["nodes", 0, "unexpected"], 0),
        ("unknown edge field", c + ["edges", 0, "unexpected"], 0),
        ("unknown fixture field", ["unexpected"], 0),
        ("bad schema", ["schema"], "other"), ("boolean version", ["version"], True),
        ("bad version", ["version"], 2),
        ("bad vector", ["splitmix64_vectors", 0, "output"], "0x0000000000000000"),
        ("empty cases", ["cases"], []),
        ("duplicate case", ["cases"], [base, deepcopy(base)]),
    ]
    for label, location, value in values:
        mutate(label, location, value)
    mutations.extend([
        ("duplicate JSON key", '{"version":1,' + json.dumps(baseline)[1:]),
        ("nonfinite JSON", json.dumps(baseline).replace('"seed": 0', '"seed": NaN', 1)),
        ("invalid JSON", "{"),
    ])
    for label, text in mutations:
        try:
            validate(load(text))
        except ValueError:
            continue
        raise ValueError(f"self-test: accepted negative mutation {label}")
    require(len(mutations) >= 40, "self-test: fewer than 40 negative mutations")
    return len(mutations)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", type=Path,
                        default=Path("fixtures") / "graph_algorithms" / "betweenness-cases.json")
    parser.add_argument("--self-test", action="store_true",
                        help="cross-check all sources and reject at least 40 corrupted fixtures")
    args = parser.parse_args()
    path = args.fixture if args.fixture.is_absolute() else Path(__file__).resolve().parent / args.fixture
    try:
        data = load(path.read_text(encoding="utf-8"))
        count = validate(data)
        print(f"Checked {count} seeded betweenness cases ({SCHEMA} v{VERSION}).")
        if args.self_test:
            negative_count = self_test(data)
            print(f"Exhaustive shortest-path cross-checks passed; rejected {negative_count} mutations.")
    except (OSError, ValueError) as error:
        print(f"Betweenness oracle failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
