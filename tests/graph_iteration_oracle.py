"""Independent tiny finite PageRank/LPA references for extracted projections.

Run python tests\\graph_iteration_oracle.py [fixture.json] [--self-test].
Relative fixture paths resolve beside this script. Exact rational PageRank steps
interpret binary64 inputs via Fraction.from_float; exported numbers are compared
within fixture tolerances, not claimed bitwise equal to iterative f64 arithmetic.
A synthetic budget counts only vertex updates, not VLite work accounting.
"""

import argparse
from collections import Counter
import copy
from fractions import Fraction
import json
import math
from pathlib import Path
import subprocess
import sys
import tempfile


SCHEMA = "vlite.graph-iteration-golden"
VERSION = 1
DEFAULT = Path("fixtures") / "graph_algorithms" / "iterative-cases.json"


def require(condition, message):
    if not condition:
        raise ValueError(message)


def keys(value, names, context):
    require(type(value) is dict and set(value) == set(names.split()),
            f"{context}: expected keys {names}")


def integer(value, lower, upper, context):
    require(type(value) is int and lower <= value <= upper,
            f"{context}: expected integer in [{lower}, {upper}]")


def number(value):
    require(type(value) in (int, float), "expected finite number, not bool")
    try:
        binary = float(value)
    except OverflowError as error:
        raise ValueError("numeric overflow") from error
    require(math.isfinite(binary), "nonfinite number or numeric overflow")
    return Fraction.from_float(binary)


def finite_export(value):
    try:
        result = float(value)
    except OverflowError as error:
        raise ValueError("numeric overflow") from error
    require(math.isfinite(result), "numeric overflow")
    return result


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, f"duplicate JSON key {key!r}")
        result[key] = value
    return result


def reject_constant(value):
    raise ValueError(f"nonfinite JSON constant {value}")


def parse_float(value):
    result = float(value)
    require(math.isfinite(result), "nonfinite JSON number or numeric overflow")
    return result


def load_text(text):
    return json.loads(text, object_pairs_hook=unique_object,
                      parse_constant=reject_constant, parse_float=parse_float)


def projection(case):
    nodes, edges = case["nodes"], case["edges"]
    require(type(nodes) is list and 1 <= len(nodes) <= 6, "expected 1..6 nodes")
    for node in nodes:
        integer(node, 0, 2**64 - 1, "node ID")
    require(len(set(nodes)) == len(nodes), "duplicate node ID")
    require(type(edges) is list and len(edges) <= 12, "expected <=12 physical edges")
    seen = set()
    weighted = case["algorithm"] == "pagerank"
    for edge in edges:
        keys(edge, "id source target weight" if weighted else "id source target", "edge")
        integer(edge["id"], 0, 2**64 - 1, "edge ID")
        require(edge["id"] not in seen, "duplicate edge ID")
        seen.add(edge["id"])
        for field in ("source", "target"):
            integer(edge[field], 0, 2**64 - 1, f"edge {field}")
            require(edge[field] in nodes, "unknown edge endpoint")
        if weighted:
            weight = number(edge["weight"])
            require(weight >= 0, "negative weight")
            if type(edge["weight"]) is int:
                integer(edge["weight"], 0, 2**53, "exact integer weight")
    return sorted(nodes), sorted(edges, key=lambda edge: edge["id"])


def options(case, nodes):
    parameters = case["parameters"]
    if case["algorithm"] == "pagerank":
        keys(parameters, "seed_ids damping tolerance max_iterations personalization", "PageRank options")
        seeds = parameters["seed_ids"]
        require(type(seeds) is list and 1 <= len(seeds) <= len(nodes), "invalid scoped seeds")
        for seed in seeds:
            integer(seed, 0, 2**64 - 1, "scoped seed ID")
            require(seed in nodes, "unknown scoped seed")
        require(len(set(seeds)) == len(seeds), "duplicate scoped seed")
        require(0 < number(parameters["damping"]) < 1, "invalid damping")
        require(0 < number(parameters["tolerance"]) < 1, "invalid tolerance")
        integer(parameters["max_iterations"], 1, 100, "max_iterations")
        masses = parameters["personalization"]
        if masses is not None:
            require(type(masses) is list and 1 <= len(masses) <= len(nodes),
                    "invalid personalization")
            seen, total = set(), Fraction(0)
            for entry in masses:
                keys(entry, "node_id mass", "personalization entry")
                integer(entry["node_id"], 0, 2**64 - 1, "personalization ID")
                require(entry["node_id"] in seeds and entry["node_id"] not in seen,
                        "duplicate or unknown personalization ID")
                seen.add(entry["node_id"])
                mass = number(entry["mass"])
                require(mass > 0, "nonpositive personalization mass")
                total += mass
            finite_export(total)
    else:
        keys(parameters, "max_sweeps", "LPA options (weights not supported)")
        integer(parameters["max_sweeps"], 1, 100, "max_sweeps")
    budget = case["synthetic_vertex_update_budget"]
    if budget is not None:
        integer(budget, 0, 600, "synthetic vertex-update budget")


def pagerank(case, nodes, edges):
    parameters = case["parameters"]
    damping, tolerance = number(parameters["damping"]), number(parameters["tolerance"])
    p = dict.fromkeys(nodes, Fraction(1, len(nodes)))
    if parameters["personalization"] is not None:
        p = dict.fromkeys(nodes, Fraction(0))
        for entry in parameters["personalization"]:
            p[entry["node_id"]] = number(entry["mass"])
        total = sum(p.values())
        p = {node: mass / total for node, mass in p.items()}
    outgoing = {node: dict.fromkeys(nodes, Fraction(0)) for node in nodes}
    for edge in edges:
        outgoing[edge["source"]][edge["target"]] += number(edge["weight"])
        finite_export(outgoing[edge["source"]][edge["target"]])
    totals = {node: sum(outgoing[node].values()) for node in nodes}
    for total in totals.values():
        finite_export(total)
    ranks, trace, used = p.copy(), [], 0
    reason, converged = "iteration_limit", False
    budget = case["synthetic_vertex_update_budget"]
    for _ in range(parameters["max_iterations"]):
        dangling = sum(ranks[node] for node in nodes if totals[node] == 0)
        candidate = {}
        for v in nodes:
            if budget is not None and used == budget:
                reason = "synthetic_interruption"
                break
            used += 1
            incoming = sum(ranks[u] * outgoing[u][v] / totals[u]
                           for u in nodes if totals[u])
            candidate[v] = (1 - damping) * p[v] + damping * (incoming + dangling * p[v])
        if len(candidate) != len(nodes):
            break
        residual = sum(abs(candidate[node] - ranks[node]) for node in nodes)
        require(sum(candidate.values()) == 1 and all(value >= 0 for value in candidate.values()),
                "invalid exact rank mass")
        ranks = candidate
        trace.append({"scores": [{"node_id": node, "score": finite_export(ranks[node])}
                                 for node in nodes], "residual": finite_export(residual)})
        if residual <= tolerance:
            converged, reason = True, None
            break
    return {"trace": trace, "result": {
        "scores": trace[-1]["scores"] if trace else None,
        "residual": trace[-1]["residual"] if trace else None,
        "completed_iterations": len(trace), "converged": converged,
        "complete": converged, "reason": reason,
    }}


def lpa(case, nodes, edges):
    neighbors = {node: set() for node in nodes}
    for edge in edges:
        u, v = edge["source"], edge["target"]
        if u != v:
            neighbors[u].add(v)
            neighbors[v].add(u)
    labels, trace, used = {node: node for node in nodes}, [], 0
    reason, converged = "iteration_limit", False
    budget = case["synthetic_vertex_update_budget"]
    for _ in range(case["parameters"]["max_sweeps"]):
        candidate = labels.copy()
        for node in nodes:
            if budget is not None and used == budget:
                reason = "synthetic_interruption"
                break
            used += 1
            votes = Counter(candidate[other] for other in sorted(neighbors[node]))
            if votes:
                maximum = max(votes.values())
                tied = {label for label, count in votes.items() if count == maximum}
                if candidate[node] not in tied:
                    candidate[node] = min(tied)
        else:
            trace.append([{"node_id": node, "label": candidate[node]} for node in nodes])
            converged = candidate == labels
            labels = candidate
            if converged:
                reason = None
                break
            continue
        break
    assignments = None
    if trace:
        canonical = {label: min(node for node in nodes if labels[node] == label)
                     for label in set(labels.values())}
        assignments = [{"node_id": node, "label": labels[node],
                        "community_id": canonical[labels[node]]} for node in nodes]
    return {"trace": trace, "result": {
        "assignments": assignments, "completed_sweeps": len(trace),
        "converged": converged, "complete": converged, "reason": reason,
    }}


def compare(actual, expected, tolerances, path):
    if type(actual) is dict:
        require(type(expected) is dict and actual.keys() == expected.keys(), f"{path}: key mismatch")
        for key in actual:
            compare(actual[key], expected[key], tolerances, f"{path}.{key}")
    elif type(actual) is list:
        require(type(expected) is list and len(actual) == len(expected), f"{path}: length mismatch")
        for index, (left, right) in enumerate(zip(actual, expected)):
            compare(left, right, tolerances, f"{path}[{index}]")
    elif type(actual) is float:
        value = number(expected)
        field = path.rsplit(".", 1)[-1]
        require(field in tolerances, f"{path}: unsupported numeric field")
        require(0 <= value <= (2 if field == "residual" else 1), f"{path}: invalid value")
        require(math.isclose(actual, float(value), rel_tol=0, abs_tol=tolerances[field]),
                f"{path}: expected {expected!r}, oracle {actual!r}")
    else:
        require(type(actual) is type(expected) and actual == expected,
                f"{path}: expected {expected!r}, oracle {actual!r}")


def check(data):
    keys(data, "schema version contract cases", "fixture")
    require(data["schema"] == SCHEMA and type(data["version"]) is int
            and data["version"] == VERSION, "unsupported schema/version")
    contract = data["contract"]
    keys(contract, "description bounds ordering reference interruption exclusions "
         "score_abs_tolerance residual_abs_tolerance", "contract")
    for field in ("description", "bounds", "ordering", "reference", "interruption", "exclusions"):
        require(type(contract[field]) is str and contract[field].strip(), f"invalid contract {field}")
    tolerances = {}
    for field in ("score", "residual"):
        value = number(contract[f"{field}_abs_tolerance"])
        require(0 < value <= Fraction(1, 10**12), "invalid comparison tolerance")
        tolerances[field] = float(value)
    require(type(data["cases"]) is list and 8 <= len(data["cases"]) <= 12, "expected 8..12 cases")
    names = set()
    for case in data["cases"]:
        keys(case, "name algorithm purpose nodes edges parameters "
             "synthetic_vertex_update_budget expected", "case")
        require(type(case["name"]) is str and case["name"].strip()
                and case["name"] not in names, "duplicate/invalid case name")
        names.add(case["name"])
        require(type(case["purpose"]) is str and case["purpose"].strip(), "invalid purpose")
        require(type(case["algorithm"]) is str
                and case["algorithm"] in ("pagerank", "label_propagation"), "unknown algorithm")
        nodes, edges = projection(case)
        options(case, nodes)
        actual = (pagerank if case["algorithm"] == "pagerank" else lpa)(case, nodes, edges)
        compare(actual, case["expected"], tolerances, case["name"])
    return len(names)


def self_test(data):
    """Mutate authored expectations/inputs, never generate replacement goldens."""
    tested = 0

    def rejects(document):
        nonlocal tested
        try:
            check(document)
        except ValueError:
            tested += 1
            return
        raise ValueError("self-test: invalid fixture accepted")

    def mutate(index, change):
        document = copy.deepcopy(data)
        change(document["cases"][index])
        rejects(document)

    pr = next(i for i, case in enumerate(data["cases"])
              if case["algorithm"] == "pagerank" and case["expected"]["trace"])
    community = next(i for i, case in enumerate(data["cases"])
                     if case["algorithm"] == "label_propagation" and case["expected"]["trace"])
    for index, count in ((pr, "completed_iterations"), (community, "completed_sweeps")):
        mutate(index, lambda c: c["expected"]["result"].__setitem__(count, 99))
        mutate(index, lambda c: c["expected"]["result"].__setitem__(count, True))
        for flag in ("converged", "complete"):
            mutate(index, lambda c: c["expected"]["result"].__setitem__(
                flag, not c["expected"]["result"][flag]))
        mutate(index, lambda c: c["expected"]["result"].__setitem__("reason", "invented"))
    mutate(pr, lambda c: c["expected"]["trace"][0]["scores"][0].__setitem__("score", 0.9))
    mutate(pr, lambda c: c["expected"]["result"]["scores"][0].__setitem__("score", 0.9))
    mutate(pr, lambda c: c["expected"]["trace"][0].__setitem__("residual", 0.9))
    mutate(pr, lambda c: c["expected"]["result"].__setitem__("residual", 0.9))
    mutate(community, lambda c: c["expected"]["trace"][0][0].__setitem__("label", 99))
    mutate(community, lambda c: c["expected"]["result"]["assignments"][0].__setitem__("community_id", 99))
    mutate(community, lambda c: c["expected"]["result"]["assignments"][0].__setitem__("label", 99))
    for i, case in enumerate(data["cases"]):
        if case["expected"]["result"]["reason"] == "synthetic_interruption":
            mutate(i, lambda c: c["expected"]["result"].update(
                complete=True, converged=True, reason=None))
    for field in ("damping", "tolerance"):
        for value in (0, 1, -1, True, "0.5", float("nan"), float("inf"), 10**400):
            mutate(pr, lambda c: c["parameters"].__setitem__(field, value))
    for index, field in ((pr, "max_iterations"), (community, "max_sweeps")):
        for value in (0, 101, True, 1.5):
            mutate(index, lambda c: c["parameters"].__setitem__(field, value))
    for masses in ([], [{"node_id": 999, "mass": 1}],
                   [{"node_id": 1, "mass": 1}, {"node_id": 1, "mass": 2}],
                   [{"node_id": True, "mass": 1}]):
        mutate(pr, lambda c: c["parameters"].__setitem__("personalization", masses))
    mutate(pr, lambda c: c["parameters"].update(
        seed_ids=[c["nodes"][0]],
        personalization=[{"node_id": c["nodes"][1], "mass": 1}]))
    for seeds in ([], [999], [1, 1], [True]):
        mutate(pr, lambda c: c["parameters"].__setitem__("seed_ids", seeds))
    for mass in (0, -1, True, float("nan"), float("inf"), 10**400):
        mutate(pr, lambda c: c["parameters"].__setitem__(
            "personalization", [{"node_id": c["parameters"]["seed_ids"][0], "mass": mass}]))
    mutate(pr, lambda c: c["parameters"].update(
        seed_ids=c["nodes"][:],
        personalization=[{"node_id": node, "mass": 1.7e308} for node in c["nodes"]]))
    for weight in (-1, True, "1", float("nan"), float("inf"), 2**53 + 1):
        mutate(pr, lambda c: c["edges"][0].__setitem__("weight", weight))
    mutate(pr, lambda c: c["edges"].extend([
        {"id": 100, "source": c["nodes"][0], "target": c["nodes"][0], "weight": 1.7e308},
        {"id": 101, "source": c["nodes"][0], "target": c["nodes"][1], "weight": 1.7e308}]))
    mutate(community, lambda c: c["parameters"].__setitem__("weight_policy", "unit"))
    mutate(community, lambda c: c["edges"][0].__setitem__("weight", 1))
    mutate(pr, lambda c: c["parameters"].__setitem__("unknown", 1))
    mutate(pr, lambda c: c["nodes"].__setitem__(0, True))
    mutate(pr, lambda c: c["nodes"].append(c["nodes"][0]))
    mutate(pr, lambda c: c["edges"].append(copy.deepcopy(c["edges"][0])))
    mutate(pr, lambda c: c["edges"][0].__setitem__("target", 999))
    for value in (-1, 601, True, 1.5):
        mutate(pr, lambda c: c.__setitem__("synthetic_vertex_update_budget", value))
    for text in ('{"schema":1,"schema":2}', '{"x":NaN}', '{"x":Infinity}',
                 '{"x":-Infinity}', '{"x":1e400}'):
        try:
            load_text(text)
        except ValueError:
            tested += 1
        else:
            raise ValueError("self-test: invalid JSON accepted")
    with tempfile.TemporaryDirectory(prefix="vlite-iteration-oracle-") as directory:
        bad = Path(directory) / "bad.json"
        bad.write_text('{"version":1,"version":1}', encoding="utf-8")
        script = str(Path(__file__).resolve())
        for arguments, diagnostic in (([str(bad)], "duplicate JSON key"),
                                      (["--unknown-option"], "unrecognized arguments"),
                                      ([str(Path(directory) / "missing.json")], "oracle failed")):
            result = subprocess.run([sys.executable, script, *arguments], capture_output=True,
                                    text=True, timeout=30, check=False)
            require(result.returncode != 0 and diagnostic in result.stderr,
                    f"self-test: CLI failure not reported: {result.stdout} {result.stderr}")
            tested += 1
    return tested


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("fixture", nargs="?", type=Path, default=DEFAULT)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    path = args.fixture if args.fixture.is_absolute() else Path(__file__).resolve().parent / args.fixture
    try:
        data = load_text(path.read_text(encoding="utf-8"))
        count = check(data)
        rejected = self_test(data) if args.self_test else None
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        print(f"oracle failed: {error}", file=sys.stderr)
        return 1
    print(f"Checked {count} finite iteration goldens ({SCHEMA} v{VERSION}).")
    if rejected is not None:
        print(f"Self-test rejected {rejected} mutations and CLI failures.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
