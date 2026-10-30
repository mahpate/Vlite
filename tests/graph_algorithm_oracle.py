"""Tiny, stdlib-only mathematical oracles, not graph engine implementations.

Run `python tests\\graph_algorithm_oracle.py [fixture.json]`. Relative fixture
arguments resolve beside this script, independent of the working directory.
PageRank expectations are stationary solutions, not invented iteration counts:
future iterative tests compare scores within score_abs_tolerance, and separately
require convergence with the requested L1 tolerance before max_iterations.
"""

import argparse
from collections import Counter
from fractions import Fraction
import json
import math
from pathlib import Path
import subprocess
import sys
import tempfile


SCHEMA = "vlite.graph-algorithm-golden"
VERSION = 1


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, f"duplicate JSON key {key!r}")
        result[key] = value
    return result


def reject_constant(value):
    raise ValueError(f"invalid JSON constant {value!r}")


def require(condition, message):
    if not condition:
        raise ValueError(message)


def keys(value, names, context):
    require(isinstance(value, dict) and set(value) == set(names.split()),
            f"{context}: expected keys {names}")


def integer(value, minimum, maximum, context):
    require(type(value) is int and minimum <= value <= maximum,
            f"{context}: expected integer in [{minimum}, {maximum}]")


def rational(value):
    require(type(value) in (int, float) and math.isfinite(value),
            f"not a finite number: {value!r}")
    return Fraction(str(value))


def project(case):
    """Unbudgeted, tiny induced projection; no TTL/storage/exhaustion claims."""
    nodes, edges, scope = case["nodes"], case["edges"], case["parameters"]["scope"]
    require(1 <= len(nodes) <= 6, "fixtures must contain 1..6 nodes")
    for node in nodes:
        keys(node, "id public_id labels", "node")
        integer(node["id"], 0, 2**64 - 1, "node ID")
        require(isinstance(node["public_id"], str) and node["public_id"], "node public ID")
        require(isinstance(node["labels"], list)
                and all(isinstance(label, str) for label in node["labels"]), "node labels")
    by_id = {node["id"]: node for node in nodes}
    require(len(by_id) == len(nodes), "duplicate node ID")
    require(len({node["public_id"] for node in nodes}) == len(nodes), "duplicate node public ID")
    require(len(edges) <= 12, "fixtures must contain at most 12 physical edges")
    edge_ids, public_ids = set(), set()
    for edge in edges:
        keys(edge, "id public_id source target relationship weight", "edge")
        integer(edge["id"], 0, 2**64 - 1, "edge ID")
        require(edge["id"] not in edge_ids, "duplicate edge ID")
        require(isinstance(edge["public_id"], str) and edge["public_id"]
                and edge["public_id"] not in public_ids, "duplicate/invalid edge public ID")
        edge_ids.add(edge["id"])
        public_ids.add(edge["public_id"])
        for field in ("source", "target"):
            integer(edge[field], 0, 2**64 - 1, f"edge {field}")
        require(edge["source"] in by_id and edge["target"] in by_id, "missing edge endpoint")
        require(isinstance(edge["relationship"], str), "edge relationship")
        require(rational(edge["weight"]) >= 0, "negative edge weight")
        if type(edge["weight"]) is int:
            require(edge["weight"] <= 2**53, "integer edge weight cannot be represented exactly")
    keys(scope, "seeds max_hops direction node_labels relationship_types", "scope")
    integer(scope["max_hops"], 0, 4, "max_hops")
    require(scope["direction"] in ("out", "in", "both"), "invalid direction")
    for field in ("node_labels", "relationship_types"):
        require(scope[field] == "any" or
                (isinstance(scope[field], list) and scope[field]
                 and all(isinstance(item, str) for item in scope[field])),
                f"invalid scope {field}")
    eligible = {
        node["id"] for node in nodes
        if scope["node_labels"] == "any"
        or set(scope["node_labels"]).issubset(node["labels"])
    }
    seeds = scope["seeds"]
    require(isinstance(seeds, list), "seeds must be a list")
    for seed in seeds:
        integer(seed, 0, 2**64 - 1, "seed ID")
    require(isinstance(seeds, list) and seeds and set(seeds).issubset(eligible),
            "seeds must exist and satisfy the label filter")
    arcs = []
    for edge in sorted(edges, key=lambda e: (e["relationship"].encode("utf-8"), e["id"])):
        if edge["source"] not in eligible or edge["target"] not in eligible:
            continue
        if scope["relationship_types"] != "any" and edge["relationship"] not in scope["relationship_types"]:
            continue
        u, v = edge["source"], edge["target"]
        if scope["direction"] == "in":
            u, v = v, u
        arcs.append((u, v, edge["id"], edge["weight"]))
        if scope["direction"] == "both" and u != v:
            arcs.append((v, u, edge["id"], edge["weight"]))
    admitted = set(seeds)
    frontier = set(seeds)
    for _ in range(scope["max_hops"]):
        frontier = {v for u, v, _, _ in arcs if u in frontier} - admitted
        admitted.update(frontier)
    arcs = [arc for arc in arcs if arc[0] in admitted and arc[1] in admitted]
    return sorted(admitted), arcs


def shortest_path(case, nodes, arcs):
    parameters = case["parameters"]
    keys(parameters, "scope source target weight_policy", "shortest path parameters")
    require(parameters["weight_policy"] in ("unit", "explicit"), "weight policy")
    source, target = parameters["source"], parameters["target"]
    require(source in parameters["scope"]["seeds"], "path source must be a seed")
    require(target in {node["id"] for node in case["nodes"]}, "missing path target")
    candidates = []

    def walk(node, cost, edge_ids, node_ids):
        if node == target:
            candidates.append((cost, len(edge_ids), edge_ids, node_ids))
        if len(edge_ids) == parameters["scope"]["max_hops"]:
            return
        for u, v, edge_id, weight in arcs:
            if u == node:
                # Enumerate walks, including repeated vertices and zero cycles.
                value = 1.0 if parameters["weight_policy"] == "unit" else float(weight)
                walk(v, cost + value, edge_ids + (edge_id,), node_ids + (v,))

    walk(source, 0.0, (), (source,))
    if not candidates:
        return {"status": "no_path_within_scope_and_hops", "path": None}
    cost, hops, edge_ids, node_ids = min(candidates, key=lambda item: item[:3])
    return {"status": "found", "path": {
        "cost": cost, "hops": hops, "edge_ids": list(edge_ids), "node_ids": list(node_ids),
    }}


def solve(matrix, rhs):
    """Exact Gauss-Jordan elimination, deliberately not power iteration."""
    n = len(rhs)
    rows = [matrix[i][:] + [rhs[i]] for i in range(n)]
    for column in range(n):
        pivot = next(row for row in range(column, n) if rows[row][column])
        rows[column], rows[pivot] = rows[pivot], rows[column]
        divisor = rows[column][column]
        rows[column] = [value / divisor for value in rows[column]]
        for row in range(n):
            if row != column:
                factor = rows[row][column]
                rows[row] = [a - factor * b for a, b in zip(rows[row], rows[column])]
    return [row[-1] for row in rows]


def pagerank(case, nodes, arcs):
    parameters = case["parameters"]
    keys(parameters, "scope damping tolerance max_iterations personalization weight_policy",
         "PageRank parameters")
    damping = rational(parameters["damping"])
    require(0 < damping < 1 and 0 < rational(parameters["tolerance"]) < 1, "PageRank options")
    integer(parameters["max_iterations"], 1, 100, "max_iterations")
    require(parameters["weight_policy"] in ("unit", "explicit"), "weight policy")
    masses = parameters["personalization"]
    p = {node: Fraction(1, len(nodes)) for node in nodes}
    if masses is not None:
        require(isinstance(masses, list) and masses, "empty personalization")
        p = dict.fromkeys(nodes, Fraction(0))
        seen = set()
        for entry in masses:
            keys(entry, "node_id mass", "personalization")
            node, mass = entry["node_id"], rational(entry["mass"])
            require(node in parameters["scope"]["seeds"] and node in p
                    and node not in seen and mass > 0, "invalid personalization")
            seen.add(node)
            p[node] = mass
        total = sum(p.values())
        p = {node: mass / total for node, mass in p.items()}
    outgoing = {node: {other: Fraction(0) for other in nodes} for node in nodes}
    for u, v, _, weight in arcs:
        outgoing[u][v] += 1 if parameters["weight_policy"] == "unit" else rational(weight)
    transition = {}
    for u in nodes:
        total = sum(outgoing[u].values())
        transition[u] = {v: outgoing[u][v] / total if total else p[v] for v in nodes}
    matrix = [[Fraction(int(u == v)) - damping * transition[u][v] for u in nodes] for v in nodes]
    ranks = solve(matrix, [(1 - damping) * p[v] for v in nodes])
    require(all(rank >= 0 for rank in ranks) and sum(ranks) == 1, "invalid stationary mass")
    residual = sum(abs(ranks[i] - ((1 - damping) * p[v] + damping * sum(
        ranks[j] * transition[u][v] for j, u in enumerate(nodes)))) for i, v in enumerate(nodes))
    require(residual == 0, "nonzero exact stationary residual")
    return {
        "stationary_scores": [{"node_id": node, "score": float(rank)} for node, rank in zip(nodes, ranks)],
        "mass": float(sum(ranks)), "fixed_point_l1_residual": float(residual),
    }


def communities(case, nodes, arcs):
    parameters = case["parameters"]
    keys(parameters, "scope max_sweeps", "label propagation parameters")
    integer(parameters["max_sweeps"], 1, 100, "max_sweeps")
    neighbors = {node: set() for node in nodes}
    for u, v, _, _ in arcs:
        if u != v:
            neighbors[u].add(v)
            neighbors[v].add(u)
    labels = {node: node for node in nodes}
    history = []
    converged = False
    for _ in range(parameters["max_sweeps"]):
        previous = labels.copy()
        for node in nodes:
            counts = Counter(labels[other] for other in neighbors[node])
            if counts:
                maximum = max(counts.values())
                tied = {label for label, count in counts.items() if count == maximum}
                if labels[node] not in tied:
                    labels[node] = min(tied)
        history.append([{"node_id": node, "label": labels[node]} for node in nodes])
        if labels == previous:
            converged = True
            break
    canonical = {label: min(node for node in nodes if labels[node] == label)
                 for label in set(labels.values())}
    return {
        "assignments": [{"node_id": node, "community_id": canonical[labels[node]]} for node in nodes],
        "sweep_assignments": history, "sweeps": len(history),
        "converged": converged, "complete": converged,
    }


def splitmix64(value):
    mask = (1 << 64) - 1
    value = (value + 0x9E3779B97F4A7C15) & mask
    value = ((value ^ (value >> 30)) * 0xBF58476D1CE4E5B9) & mask
    value = ((value ^ (value >> 27)) * 0x94D049BB133111EB) & mask
    return value ^ (value >> 31)


def betweenness(case, nodes, arcs):
    parameters = case["parameters"]
    keys(parameters, "scope samples sampling_seed", "betweenness parameters")
    integer(parameters["sampling_seed"], 0, 2**64 - 1, "sampling_seed")
    integer(parameters["samples"], len(nodes), 64, "full-sample count")
    neighbors = {node: set() for node in nodes}
    for u, v, _, _ in arcs:
        if u != v:
            neighbors[u].add(v)
    totals = dict.fromkeys(nodes, Fraction(0))
    sources = sorted(nodes, key=lambda node: (splitmix64(parameters["sampling_seed"] ^ node), node))
    for source in sources:
        for target in nodes:
            if source == target:
                continue
            paths = []

            def enumerate_paths(path):
                if path[-1] == target:
                    paths.append(path)
                    return
                for other in sorted(neighbors[path[-1]]):
                    if other not in path:
                        enumerate_paths(path + (other,))

            enumerate_paths((source,))
            if paths:
                shortest = min(map(len, paths))
                winners = [path for path in paths if len(path) == shortest]
                for node in nodes:
                    totals[node] += Fraction(sum(node in path[1:-1] for path in winners), len(winners))
    divisor = (len(nodes) - 1) * (len(nodes) - 2)
    return {
        "scores": [{"node_id": node, "score": float(totals[node] / divisor) if len(nodes) > 2 else 0.0}
                   for node in nodes],
        "selected_sources": sources, "completed_sources": sources,
        "completed_source_count": len(nodes), "estimated": False,
    }


ALGORITHMS = {
    "weighted_shortest_path": shortest_path,
    "pagerank": pagerank,
    "label_propagation": communities,
    "betweenness": betweenness,
}


def compare(actual, expected, tolerance, path):
    if isinstance(actual, dict):
        require(isinstance(expected, dict) and actual.keys() == expected.keys(), f"{path}: key mismatch")
        for key in actual:
            compare(actual[key], expected[key], tolerance, f"{path}.{key}")
    elif isinstance(actual, list):
        require(isinstance(expected, list) and len(actual) == len(expected), f"{path}: length mismatch")
        for index, (left, right) in enumerate(zip(actual, expected)):
            compare(left, right, tolerance, f"{path}[{index}]")
    elif type(actual) is float and path.rsplit(".", 1)[-1] in ("score", "mass", "fixed_point_l1_residual"):
        rational(expected)
        require(math.isclose(actual, expected, rel_tol=0, abs_tol=tolerance),
                f"{path}: expected {expected!r}, oracle {actual!r}")
    else:
        require(type(actual) is type(expected) or
                (type(actual) is float and type(expected) in (int, float)), f"{path}: type mismatch")
        require(actual == expected, f"{path}: expected {expected!r}, oracle {actual!r}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("fixture", nargs="?",
                        default=Path("fixtures") / "graph_algorithms" / "golden.json", type=Path)
    parser.add_argument("--self-test", action="store_true", help="also verify corrupted fixture rejection")
    args = parser.parse_args()
    path = args.fixture if args.fixture.is_absolute() else Path(__file__).resolve().parent / args.fixture
    data = json.loads(path.read_text(encoding="utf-8"),
                      object_pairs_hook=unique_object, parse_constant=reject_constant)
    keys(data, "schema version contract cases", "fixture")
    require(data["schema"] == SCHEMA and type(data["version"]) is int and data["version"] == VERSION,
            "unsupported schema/version")
    keys(data["contract"], "description score_abs_tolerance mass_abs_tolerance residual_abs_tolerance "
         "ordering bounds pagerank_reference exclusions", "contract")
    tolerances = {key: float(rational(data["contract"][key])) for key in
                  ("score_abs_tolerance", "mass_abs_tolerance", "residual_abs_tolerance")}
    require(all(0 < value <= 1e-8 for value in tolerances.values()), "invalid numerical tolerances")
    require(isinstance(data["cases"], list) and data["cases"], "empty cases")
    seen = set()
    for case in data["cases"]:
        keys(case, "name algorithm purpose nodes edges parameters expected", "case")
        require(isinstance(case["name"], str) and case["name"].strip()
                and case["name"] not in seen, "duplicate/invalid case name")
        seen.add(case["name"])
        require(case["algorithm"] in ALGORITHMS, "unknown algorithm")
        nodes, arcs = project(case)
        output = ALGORITHMS[case["algorithm"]](case, nodes, arcs)
        actual = {"scope_node_ids": nodes, "scope_edge_ids": sorted({arc[2] for arc in arcs}), "output": output}
        compare(actual, case["expected"], tolerances["score_abs_tolerance"], case["name"])
        if case["algorithm"] == "pagerank":
            for field, tolerance_key in (("mass", "mass_abs_tolerance"),
                                         ("fixed_point_l1_residual", "residual_abs_tolerance")):
                compare(output[field], case["expected"]["output"][field],
                        tolerances[tolerance_key], f"{case['name']}.{field}")
    print(f"Checked {len(seen)} tiny graph algorithm golden cases ({SCHEMA} v{VERSION}).")
    if args.self_test:
        case = next(case for case in data["cases"]
                    if case["algorithm"] == "weighted_shortest_path"
                    and case["expected"]["output"]["status"] == "found")
        case["expected"]["output"]["path"]["cost"] += 1
        mutations = [
            ("wrong-cost.json", json.dumps(data), "oracle"),
            ("duplicate-key.json", '{"version":1,' + path.read_text(encoding="utf-8").lstrip()[1:],
             "duplicate JSON key"),
        ]
        with tempfile.TemporaryDirectory(prefix="vlite-graph-oracle-") as directory:
            for name, contents, diagnostic in mutations:
                broken = Path(directory) / name
                broken.write_text(contents, encoding="utf-8")
                result = subprocess.run(
                    [sys.executable, str(Path(__file__).resolve()), str(broken)],
                    capture_output=True, text=True, timeout=30, check=False,
                )
                require(result.returncode != 0 and diagnostic in result.stderr,
                        f"{name}: invalid fixture was not rejected: {result.stdout} {result.stderr}")
        print("Corrupted graph goldens and duplicate keys correctly rejected")


if __name__ == "__main__":
    main()
