"""Independent Python write reference for vlite-cypher-1 (Python 3.11+, stdlib).

Only the accepted Python front end/read primitives are imported. Storage sizes
are hand-derived from DO-003; no subset implementation is read or executed.
"""

from __future__ import annotations

import argparse
import copy
from functools import cmp_to_key
import json
from pathlib import Path
import random
import re
import struct
import sys

import graph_subset_oracle as read


FIXTURE = Path(__file__).resolve().parent / "fixtures" / "graph_subset"
BODY_CAP = 16777216
OP_CAP = 4096
MUTATION = None
MUTANTS = (
    "merge-by-triple", "bypass-unique", "bypass-required", "bypass-endpoints",
    "bypass-type", "bypass-triple", "omit-detach-edge", "ttl-inclusive",
    "validity-from-exclusive", "validity-to-inclusive", "sample-clock-per-clause",
    "accept-4097", "omit-encoding-overhead", "omit-reclaim-bytes",
    "append-before-final-validation", "retain-partial-cleanup", "reverse-delete-order",
    "combine-patches", "remove-absent-errors", "patch-clears-expiry",
    "merge-replaces-extras", "create-edge-numeric-equality", "create-keyless-dedup",
    "limit-before-mutations", "skip-detach-work", "delete-vectors",
    "return-stale-binding", "merge-allows-temporal", "reuse-deleted-id",
    "ignore-final-output-cap", "reset-work-per-clause", "rerun-match",
)
NOT_MODELED = [
    "deadline execution", "scratch execution", "cancellation", "writer guard",
    "I/O and indeterminate commits", "durability and reopen",
]


def mutated(name):
    return MUTATION == name


def fail(kind, reason=None):
    raise read.ReadFailure(kind, reason)


def invalid(reason):
    fail("InvalidQuery", "subset " + reason)


def properties_size(properties):
    # DO-003 decisions.md:116-125: count(4), name length(2), name, tag/payload.
    if len(properties) > 64:
        fail("InvalidMetadata")
    total = 4
    for name, value in properties.items():
        if not 1 <= len(name.encode("utf-8")) <= 256:
            fail("InvalidMetadata")
        try:
            total += 2 + len(name.encode("utf-8")) + read.value_size(value)
        except read.Diagnostic:
            fail("InvalidMetadata")
    if total > 65536:
        fail("InvalidMetadata")
    return total


def labels_size(labels):
    if len(labels) > 8 or any(not re.fullmatch(r"[A-Za-z0-9_]{1,63}", x) for x in labels):
        fail("InvalidQuery")
    return 1 + sum(1 + len(x) for x in labels)


def key_valid(key):
    if not isinstance(key, str) or not 1 <= len(key.encode("utf-8")) <= 256:
        fail("InvalidQuery")


def operation_size(op):
    """Entry envelope + payload, never JSON size or parameter-map size.

    DO-003 decisions.md:114,132-137; reclaim:1099-1106.
    Cross-check: engine/codec.rs:38-40,508-567; properties.rs:8-11,57-80.
    """
    kind = op["op"]
    if kind == "CreateNode":
        return 8 + 8 + 2 + len(op["key"].encode("utf-8")) + labels_size(op["labels"]) + 4 + properties_size(op["properties"])
    if kind == "CreateEdge":
        return 8 + 24 + 1 + len(op["type"]) + 2 + (len(op["key"].encode("utf-8")) if op["key"] is not None else 0) + 4 + properties_size(op["properties"])
    if kind == "PatchNode":
        return 8 + 8 + labels_size(op["add_labels"]) + labels_size(op["remove_labels"]) + 4 + properties_size(op["set"]) + 1 + sum(2 + len(x.encode("utf-8")) for x in op["unset"])
    if kind == "PatchEdge":
        return 8 + 8 + 4 + properties_size(op["set"]) + 1 + sum(2 + len(x.encode("utf-8")) for x in op["unset"])
    if kind in ("DeleteNode", "DeleteEdge"):
        return 8 + 8
    raise ValueError("unrecognized operation")


def bit_equal(left, right):
    if left["tag"] != right["tag"]:
        return False
    if left["tag"] == "Float":
        return struct.pack("<d", left["value"]) == struct.pack("<d", right["value"])
    if left["tag"] == "GeoPoint":
        return all(struct.pack("<d", left[x]) == struct.pack("<d", right[x])
                   for x in ("latitude", "longitude"))
    return read.typed_equal(left, right)


class Meter(read.ReadMeter):
    def check(self, phase):
        # Execution time/cancellation are deliberately not simulated.
        return

    def allocate(self, size):
        # Scratch input is validated; Python allocation is not Rust scratch.
        return

    def scan(self):
        self.charge()
        self.edges += 1
        if self.edges > self.limits["max_edges_examined"]:
            invalid("edge budget exhausted")


def canonical_graph(graph):
    result = copy.deepcopy(graph)
    for name in ("nodes", "edges"):
        result[name].sort(key=lambda entity: int(entity["id"]))
    for node in result["nodes"]:
        node["labels"].sort(key=lambda label: label.encode("utf-8"))
    result["attachments"].sort(key=lambda a: (int(a["node_id"]), a["collection"].encode("utf-8")))
    result["records"].sort(key=lambda r: (r["collection"].encode("utf-8"), int(r["id"])))
    return result


def read_graph(graph):
    return {key: graph[key] for key in ("nodes", "edges", "temporal_capable", "ttl_clock_ms", "view_sequence")}


def materialize(graph, statement, params, meter, ttl):
    if statement.pattern is None:
        meter.binding_insert(1)
        return [{}]
    pattern = statement.pattern
    nodes = {n["id"]: n for n in graph["nodes"]}

    def live(entity):
        expiry = entity.get("expires_at_ms")
        return expiry is None or (ttl <= int(expiry) if mutated("ttl-inclusive") else ttl < int(expiry))

    def match(item, entity, edge=False):
        if item.anchor:
            role, ref = item.anchor
            if entity["key" if role == "KEY" else "id"] != params[ref.name]["value"]:
                return False
        if edge:
            if item.types and entity["type"] not in [token.value for token in item.types]:
                return False
        elif not all(token.value in entity["labels"] for token in item.labels):
            return False
        for token, ref in item.properties:
            meter.charge()
            if not read.typed_equal(entity["properties"].get(token.value, read.NULL), params[ref.name]):
                return False
        return True

    rows = []
    starts = sorted(nodes.values(), key=lambda n: int(n["id"]))
    if pattern.first.anchor:
        meter.charge()
        role, ref = pattern.first.anchor
        starts = [n for n in starts if n["key" if role == "KEY" else "id"] == params[ref.name]["value"]]
    for node in starts:
        if not pattern.first.anchor:
            meter.scan()
        meter.visit(node["id"])
        if not live(node) or not match(pattern.first, node):
            continue
        if pattern.relationship is None:
            meter.binding_insert(len(rows) + 1)
            rows.append({pattern.first.variable.value: node})
            continue
        rel = pattern.relationship
        if rel.anchor:
            meter.charge()
        degree = 0
        for raw in sorted(graph["edges"], key=lambda e: int(e["id"])):
            destinations = []
            if rel.direction in ("out", "both") and raw["source"] == node["id"]:
                destinations.append(raw["target"])
            if rel.direction in ("in", "both") and raw["target"] == node["id"] and (
                    rel.direction == "in" or raw["source"] != node["id"]):
                destinations.append(raw["source"])
            for destination in destinations:
                degree += 1
                meter.examine(node["id"], degree)
                if not live(raw) or not live(nodes[destination]):
                    continue
                edge = raw
                if "segments" in raw:
                    segments = [s for s in raw["segments"]
                                if (s["from_ms"] is None or (int(s["from_ms"]) < ttl if mutated("validity-from-exclusive") else int(s["from_ms"]) <= ttl))
                                and (s["to_ms"] is None or (ttl <= int(s["to_ms"]) if mutated("validity-to-inclusive") else ttl < int(s["to_ms"])))]
                    if not segments:
                        continue
                    edge = dict(raw, properties=segments[0]["properties"])
                if not match(rel, edge, edge=True) or not match(pattern.second, nodes[destination]):
                    continue
                if pattern.first.variable.value == pattern.second.variable.value and destination != node["id"]:
                    continue
                meter.visit(destination)
                meter.binding_insert(len(rows) + 1)
                row = {pattern.first.variable.value: node, pattern.second.variable.value: nodes[destination]}
                if rel.variable:
                    row[rel.variable.value] = edge
                if pattern.path:
                    row[pattern.path.value] = {"length": 1, "edge_ids": [edge["id"]],
                                               "node_ids": [node["id"], destination]}
                rows.append(row)
    if pattern.relationship:
        def compare(left, right):
            meter.charge()
            bindings = [(name, kind) for name, kind in statement.bindings if name in left]
            a, b = read.row_identity(left, bindings), read.row_identity(right, bindings)
            return (a > b) - (a < b)
        rows.sort(key=cmp_to_key(compare))
    if statement.where:
        rows = [row for row in rows if read.expression(statement.where, row, params, meter) is True]
    return rows


class Transaction:
    def __init__(self, graph, statement, params, bounds, clock):
        self.original = copy.deepcopy(graph)
        self.graph = canonical_graph(graph)
        self.statement = statement
        self.params = params
        self.meter = Meter(bounds, {})
        self.ttl = int(clock.get("ttl_clock_ms", graph["ttl_clock_ms"]))
        self.clock = clock
        self.sequence = str(int(graph["view_sequence"]) + 1)
        if graph["temporal_capable"] and self.ttl > 2**63 - 1:
            fail("InvalidQuery")
        self.nodes = {n["id"]: n for n in self.graph["nodes"]}
        self.edges = {e["id"]: e for e in self.graph["edges"]}
        self.operations = []
        self.bytes = 4
        self.reclaims = 0
        self.touched = set()
        self.detach_examined = 0
        self.rows = materialize(self.graph, statement, params, self.meter, self.ttl)
        self.initial_bindings = dict(statement.bindings)
        self.cleanup()

    def cleanup(self):
        def due(entity):
            expiry = entity.get("expires_at_ms")
            return expiry is not None and (int(expiry) < self.ttl if mutated("ttl-inclusive") else int(expiry) <= self.ttl)
        due_nodes = {i for i, n in self.nodes.items()
                     if due(n)}
        due_edges = {i for i, e in self.edges.items()
                     if e["source"] in due_nodes or e["target"] in due_nodes
                     or due(e)}
        if due_nodes or due_edges:
            for i in due_edges:
                del self.edges[i]
            for i in due_nodes:
                del self.nodes[i]
            self.graph["attachments"] = [a for a in self.graph["attachments"] if a["node_id"] not in due_nodes]
            if not mutated("omit-reclaim-bytes"):
                self.bytes += 16  # 8 envelope + one u64 clock; one opening reclaim.
            self.reclaims = 1

    def resolve(self, row, name):
        kind = self.initial_bindings[name]
        bound = row[name]
        entities = self.nodes if kind == "node" else self.edges
        if kind == "path":
            if any(i not in self.nodes for i in bound["node_ids"]) or any(i not in self.edges for i in bound["edge_ids"]):
                invalid("binding was deleted")
            return bound, kind
        if bound["id"] not in entities:
            if mutated("return-stale-binding"):
                return bound, kind
            invalid("binding was deleted")
        return entities[bound["id"]], kind

    def expected(self, entity, kind):
        identity = kind, entity["id"]
        expected = "any" if identity in self.touched else {"revision": entity["revision"]}
        self.touched.add(identity)
        return expected

    def stage(self, op):
        self.meter.charge()
        cap = OP_CAP + int(mutated("accept-4097"))
        if mutated("combine-patches") and self.operations and op["op"] in ("PatchNode", "PatchEdge"):
            old = self.operations[-1]
            if old["op"] == op["op"] and old["id"] == op["id"]:
                self.bytes -= operation_size(old)
                old["set"].update(op["set"])
                for name in op["unset"]:
                    old["set"].pop(name, None)
                old["unset"] = sorted((set(old["unset"]) | set(op["unset"])) - set(op["set"]))
                if op["op"] == "PatchNode":
                    old["add_labels"] = sorted((set(old["add_labels"]) | set(op["add_labels"])) - set(op["remove_labels"]))
                    old["remove_labels"] = sorted((set(old["remove_labels"]) | set(op["remove_labels"])) - set(op["add_labels"]))
                self.bytes += operation_size(old)
                return
        if len(self.operations) >= cap:
            fail("BatchTooLarge")
        self.bytes += operation_size(op) - (8 if mutated("omit-encoding-overhead") else 0)
        if self.bytes > BODY_CAP:
            fail("BatchTooLarge")
        self.operations.append(op)

    def schema(self):
        for rule in self.graph["schema"]["nodes"]:
            seen = {}
            for node in self.nodes.values():
                if rule["label"] not in node["labels"]:
                    continue
                for name, field in rule["properties"].items():
                    value = node["properties"].get(name)
                    if value is None:
                        if field["required"] and not mutated("bypass-required"):
                            fail("GraphSchemaViolation")
                        continue
                    if value["tag"] != field["type"] and not mutated("bypass-type"):
                        fail("GraphSchemaViolation")
                    if field["unique"] and not mutated("bypass-unique"):
                        if any(read.typed_equal(value, old) for old in seen.get(name, [])):
                            fail("GraphSchemaViolation")
                        seen.setdefault(name, []).append(value)
        for rule in self.graph["schema"]["edges"]:
            triples = set()
            for edge in self.edges.values():
                if edge["type"] != rule["type"]:
                    continue
                for name, field in rule["properties"].items():
                    value = edge["properties"].get(name)
                    if value is None:
                        if field["required"] and not mutated("bypass-required"):
                            fail("GraphSchemaViolation")
                    elif value["tag"] != field["type"] and not mutated("bypass-type"):
                        fail("GraphSchemaViolation")
                if rule["endpoints"] and not mutated("bypass-endpoints") and not any(
                        a in self.nodes[edge["source"]]["labels"] and b in self.nodes[edge["target"]]["labels"]
                        for a, b in rule["endpoints"]):
                    fail("GraphSchemaViolation")
                triple = edge["source"], edge["target"]
                if rule["unique_triple"] and triple in triples and not mutated("bypass-triple"):
                    fail("GraphSchemaViolation")
                triples.add(triple)

    def allocate_id(self, kind):
        name = "next_node_id" if kind == "node" else "next_edge_id"
        result = self.graph[name]
        if mutated("reuse-deleted-id"):
            entities = self.nodes if kind == "node" else self.edges
            result = str(next(i for i in range(1, len(entities) + 2) if str(i) not in entities))
        if int(result) >= 2**64 - 1:
            fail("IdSpaceExhausted")
        self.graph[name] = str(int(result) + 1)
        return result

    def create(self, pattern, row, merge):
        rel = pattern.relationship
        item = rel if rel else pattern.first
        key = self.params[item.anchor[1].name]["value"] if item.anchor else None
        if key is not None:
            key_valid(key)
        properties = {name.value: copy.deepcopy(self.params[ref.name]) for name, ref in item.properties}
        properties_size(properties)
        if rel:
            source, _ = self.resolve(row, pattern.first.variable.value)
            target, _ = self.resolve(row, pattern.second.variable.value)
            kind, collection = "rel", self.edges
            relationship = rel.types[0].value
            if not re.fullmatch(r"[A-Za-z0-9_]{1,63}", relationship):
                fail("InvalidQuery")
            variable = rel.variable.value
        else:
            kind, collection = "node", self.nodes
            labels = sorted({t.value for t in item.labels})
            labels_size(labels)
            variable = item.variable.value
        if key is not None:
            self.meter.charge()
        existing = next((e for e in collection.values() if key is not None and e["key"] == key), None)
        if rel and ((merge and mutated("merge-by-triple")) or (key is None and mutated("create-keyless-dedup"))):
            existing = next((e for e in collection.values() if e["source"] == source["id"]
                             and e["target"] == target["id"] and e["type"] == relationship), existing)
        if existing:
            if rel and "segments" in existing:
                if merge and not mutated("merge-allows-temporal"):
                    invalid("temporal mutation unsupported")
                if not merge:
                    fail("GraphKeyConflict")
            if not merge and not rel:
                fail("GraphKeyExists")
            equal = read.typed_equal if merge or mutated("create-edge-numeric-equality") else bit_equal
            fits = all(name in existing["properties"] and equal(value, existing["properties"][name])
                       for name, value in properties.items())
            if not merge:
                fits = fits and set(existing["properties"]) == set(properties)
            fits = fits and (existing["source"] == source["id"] and existing["target"] == target["id"]
                             and existing["type"] == relationship if rel
                             else all(label in existing["labels"] for label in labels))
            if not fits:
                fail("GraphKeyConflict")
            if merge and mutated("merge-replaces-extras"):
                existing["properties"] = copy.deepcopy(properties)
                if not rel:
                    existing["labels"] = labels
            row[variable] = existing
            return
        identity = self.allocate_id("node" if kind == "node" else "edge")
        entity = {"id": identity, "key": key, "properties": properties, "revision": self.sequence}
        if rel:
            entity.update(source=source["id"], target=target["id"], type=relationship)
            op = dict(op="CreateEdge", **copy.deepcopy(entity))
        else:
            entity["labels"] = labels
            op = dict(op="CreateNode", **copy.deepcopy(entity))
        collection[identity] = entity
        self.stage(op)
        self.schema()
        self.touched.add((kind, identity))
        row[variable] = entity

    def patch(self, clause):
        pending = {}
        for row in self.rows:
            for target, ref in clause.items:
                variable = target.variable.value if isinstance(target, read.Field) else target[0].value
                entity, kind = self.resolve(row, variable)
                if kind == "rel" and "segments" in entity:
                    invalid("temporal mutation unsupported")
                identity = kind, entity["id"]
                if identity not in pending:
                    pending[identity] = (entity, {"op": "PatchNode" if kind == "node" else "PatchEdge",
                                                "id": entity["id"], "set": {}, "unset": []})
                    if kind == "node":
                        pending[identity][1].update(add_labels=[], remove_labels=[])
                op = pending[identity][1]
                if isinstance(target, read.Field):
                    name = target.name.value
                    if clause.kind == "SET":
                        op["set"][name] = copy.deepcopy(self.params[ref.name])
                    elif name not in op["unset"]:
                        if mutated("remove-absent-errors") and name not in entity["properties"]:
                            fail("GraphNodeNotFound")
                        op["unset"].append(name)
                else:
                    name = target[1].value
                    field = "add_labels" if clause.kind == "SET" else "remove_labels"
                    if name not in op[field]:
                        op[field].append(name)
        for (kind, _), (entity, op) in pending.items():
            op["expected"] = self.expected(entity, kind)
            op["unset"].sort(key=lambda x: x.encode("utf-8"))
            properties_size(op["set"])
            entity["properties"].update(op["set"])
            for name in op["unset"]:
                entity["properties"].pop(name, None)
            properties_size(entity["properties"])
            if kind == "node":
                for name in ("add_labels", "remove_labels"):
                    op[name].sort()
                entity["labels"] = sorted((set(entity["labels"]) | set(op["add_labels"])) - set(op["remove_labels"]))
                labels_size(entity["labels"])
            entity["revision"] = self.sequence
            if mutated("patch-clears-expiry"):
                entity.pop("expires_at_ms", None)
            self.stage(op)
            self.schema()

    def delete(self, clause):
        targets = {"node": {}, "rel": {}}
        for row in self.rows:
            for token in clause.items:
                entity, kind = self.resolve(row, token.value)
                targets[kind][entity["id"]] = entity
        if clause.kind == "DETACH DELETE":
            # Bound discovery before insertion; no unbounded adjacency set.
            for node_id in targets["node"]:
                degree = 0
                for edge in self.edges.values():
                    if node_id not in (edge["source"], edge["target"]):
                        continue
                    degree += 1
                    if not mutated("skip-detach-work"):
                        self.meter.examine(node_id, degree)
                    self.detach_examined += 1
                    if mutated("omit-detach-edge") and degree == 1:
                        continue
                    if edge["id"] not in targets["rel"]:
                        if len(self.operations) + len(targets["node"]) + len(targets["rel"]) + 1 > OP_CAP + int(mutated("accept-4097")):
                            fail("BatchTooLarge")
                        targets["rel"][edge["id"]] = edge
        for kind in (("node", "rel") if mutated("reverse-delete-order") else ("rel", "node")):
            for identity in sorted(targets[kind], key=int):
                entity = targets[kind][identity]
                if kind == "node" and any(identity in (e["source"], e["target"]) for e in self.edges.values()):
                    fail("GraphNodeHasEdges")
                op = {"op": "DeleteNode" if kind == "node" else "DeleteEdge", "id": identity,
                      "expected": self.expected(entity, kind)}
                self.stage(op)
                if kind == "rel":
                    del self.edges[identity]
                else:
                    del self.nodes[identity]
                    if mutated("delete-vectors"):
                        attached = {(a["collection"], a["record_id"]) for a in self.graph["attachments"] if a["node_id"] == identity}
                        self.graph["records"] = [r for r in self.graph["records"] if (r["collection"], r["id"]) not in attached]
                    self.graph["attachments"] = [a for a in self.graph["attachments"] if a["node_id"] != identity]

    def project(self):
        returning = self.statement.returning
        if returning is None:
            return [], []
        projected = []
        for row in self.rows:
            self.meter.charge()
            cells = []
            for item in returning.items:
                entity, kind = self.resolve(row, item.variable.value)
                if kind == "rel" and "segments" in entity:
                    segment = next(s for s in entity["segments"]
                                   if (s["from_ms"] is None or int(s["from_ms"]) <= self.ttl)
                                   and (s["to_ms"] is None or self.ttl < int(s["to_ms"])))
                    entity = dict(entity, properties=segment["properties"])
                cells.append(copy.deepcopy(entity["properties"].get(item.property.value, read.NULL))
                             if item.property else read.entity_result(entity, kind))
            projected.append((read.row_identity(row, self.statement.bindings), cells))
        columns = [item.column for item in returning.items]
        if returning.sorts:
            def compare(left, right):
                self.meter.charge()
                for token, direction in returning.sorts:
                    index = columns.index(token.value)
                    a, b = read.scalar_sort_key(left[1][index]), read.scalar_sort_key(right[1][index])
                    delta = (a > b) - (a < b)
                    if delta:
                        return -delta if direction == "DESC" else delta
                return (left[0] > right[0]) - (left[0] < right[0])
            projected.sort(key=cmp_to_key(compare))
        skip = int(self.params[returning.skip.name]["value"]) if returning.skip else 0
        limit = int(self.params[returning.limit.name]["value"]) if returning.limit else None
        projected = projected[skip:][:limit]
        self.meter.returned(len(projected))
        nodes, edges = set(), set()
        paths = 0
        for _, cells in projected:
            for cell in cells:
                if cell["tag"] == "Node":
                    nodes.add(cell["id"])
                elif cell["tag"] == "Edge":
                    edges.add(cell["id"])
                elif cell["tag"] == "Path":
                    nodes.update(cell["node_ids"])
                    edges.update(cell["edge_ids"])
                    paths += 1
        if not mutated("ignore-final-output-cap"):
            self.meter.output(len(nodes), len(edges), paths)
        return columns, [cells for _, cells in projected]

    def run(self):
        if mutated("limit-before-mutations") and self.statement.returning and self.statement.returning.limit:
            self.rows = self.rows[:int(self.params[self.statement.returning.limit.name]["value"])]
        for index, clause in enumerate(self.statement.mutations):
            if mutated("reset-work-per-clause"):
                self.meter.work = 0
            if mutated("sample-clock-per-clause") and self.clock.get("ttl_samples_ms"):
                samples = self.clock["ttl_samples_ms"]
                self.ttl = int(samples[min(index, len(samples) - 1)])
                self.cleanup()
            if mutated("rerun-match") and index:
                current = dict(self.graph, nodes=list(self.nodes.values()), edges=list(self.edges.values()))
                self.rows = materialize(current, self.statement, self.params, self.meter, self.ttl)
            if clause.kind in ("CREATE", "MERGE"):
                for row in self.rows:
                    for pattern in clause.items:
                        self.create(pattern, row, clause.kind == "MERGE")
            elif clause.kind in ("SET", "REMOVE"):
                self.patch(clause)
            else:
                self.delete(clause)
        columns, rows = self.project()
        if self.operations and int(self.sequence) >= 2**64:
            return not_modeled_result(self.original, ["commit sequence exhaustion"])
        if self.operations:
            self.graph.update(nodes=list(self.nodes.values()), edges=list(self.edges.values()),
                              view_sequence=self.sequence)
            graph = canonical_graph(self.graph)
        else:
            graph = self.original
        return {
            "outcome": "result", "columns": columns, "rows": rows,
            "ttl_clock_ms": str(self.ttl), "as_of_ms": str(self.ttl) if self.graph["temporal_capable"] else None,
            "view_sequence": self.original["view_sequence"], "revision": self.sequence if self.operations else None,
            "operations": self.operations, "commit_frames": int(bool(self.operations)),
            "counters": {"operations": len(self.operations), "body_bytes": self.bytes if self.operations else 0,
                         "reclaim_entries": self.reclaims if self.operations else 0,
                         "binding_rows": len(self.rows), "detach_edges_examined": self.detach_examined},
            "graph": graph, "not_modeled": NOT_MODELED,
        }


def evaluate_write(graph, statement, params=None, limits=None, clock=None):
    """Copy-on-write transaction; every modeled failure returns the whole input."""
    params = {} if params is None else params
    clock = {} if clock is None else clock
    tx = None
    try:
        bounds = read.read_limits(limits)
        if isinstance(statement, str):
            statement = read.parse(statement, params, max_hops=bounds["max_hops"])
        else:
            read.validate_parameters(statement, params, 0)
            read.validate_hops(statement, bounds["max_hops"])
        if statement.kind != "write":
            return not_modeled_result(graph, ["read statement"])
        for reference in statement.references:
            if reference.use == "KEY":
                key_valid(params[reference.name]["value"])
        if set(clock) - {"ttl_clock_ms", "ttl_samples_ms"}:
            return not_modeled_result(graph, ["execution clock controls"])
        tx = Transaction(graph, statement, params, bounds, clock)
        return tx.run()
    except (read.Diagnostic, read.ReadFailure) as error:
        result = {"outcome": "reject", "error": error.result(), "operations": [],
                  "commit_frames": 0, "graph": copy.deepcopy(graph), "not_modeled": NOT_MODELED}
        if tx is not None and mutated("append-before-final-validation") and tx.operations:
            result["commit_frames"] = 1
            result["operations"] = copy.deepcopy(tx.operations)
            result["graph"] = canonical_graph(dict(tx.graph, nodes=list(tx.nodes.values()), edges=list(tx.edges.values()),
                                                   view_sequence=tx.sequence))
        if tx is not None and mutated("retain-partial-cleanup"):
            partial = copy.deepcopy(graph)
            due = {n["id"] for n in partial["nodes"] if n.get("expires_at_ms") is not None and int(n["expires_at_ms"]) <= tx.ttl}
            partial["nodes"] = [n for n in partial["nodes"] if n["id"] not in due]
            partial["edges"] = [e for e in partial["edges"] if e["source"] not in due and e["target"] not in due
                                and not (e.get("expires_at_ms") is not None and int(e["expires_at_ms"]) <= tx.ttl)]
            partial["attachments"] = [a for a in partial["attachments"] if a["node_id"] not in due]
            result["graph"] = partial
        return result


def not_modeled_result(graph, features):
    return {"outcome": "NOT_MODELED", "graph": copy.deepcopy(graph),
            "operations": [], "commit_frames": 0, "not_modeled": features}


def load_json(path):
    return read.unique_json(path.read_text(encoding="utf-8"))


def expand_data(value, index=None, depth=0):
    """Bounded declarative fixture expansion, independent of transaction code."""
    read.require(depth <= 24, "expansion-depth")
    if isinstance(value, str):
        return value.replace("{i}", str(index)) if index is not None else value
    if isinstance(value, list):
        result = []
        for item in value:
            if isinstance(item, dict) and "series" in item:
                read.exact_keys(item, ("series", "start", "template"))
                read.require(type(item["series"]) is int and 0 <= item["series"] <= 10000, "series-count")
                read.require(type(item["start"]) is int and 0 <= item["start"] <= 10000, "series-start")
                read.require(not contains_series(item["template"]), "nested-series")
                result.extend(expand_data(item["template"], i, depth + 1)
                              for i in range(item["start"], item["start"] + item["series"]))
            else:
                result.append(expand_data(item, index, depth + 1))
        return result
    if isinstance(value, dict):
        result = {key: expand_data(child, index, depth + 1) for key, child in value.items()}
        if result.get("tag") == "String":
            result["value"] = read.expand(result["value"])
        return result
    return value


def contains_series(value):
    if isinstance(value, dict):
        return "series" in value or any(contains_series(child) for child in value.values())
    return isinstance(value, list) and any(contains_series(child) for child in value)


GRAPH_KEYS = (
    "nodes", "edges", "temporal_capable", "ttl_clock_ms", "view_sequence",
    "next_node_id", "next_edge_id", "attachments", "records", "schema",
)


def expand_graph(spec, graphs, trail=()):
    if isinstance(spec, str):
        read.require(spec in graphs, "graph-reference")
        read.require(spec not in trail and len(trail) < 16, "graph-cycle")
        return expand_graph(graphs[spec], graphs, trail + (spec,))
    read.require(isinstance(spec, dict), "graph-object")
    if "base" not in spec:
        read.exact_keys(spec, GRAPH_KEYS)
        return canonical_graph(expand_data(spec))
    read.exact_keys(spec, ("base",), ("put_nodes", "drop_nodes", "put_edges", "drop_edges", "replace"))
    result = expand_graph(spec["base"], graphs, trail)
    for name in ("nodes", "edges"):
        read.require(isinstance(spec.get("drop_" + name, []), list)
                     and isinstance(spec.get("put_" + name, []), list), "overlay-array")
        drop = set(expand_data(spec.get("drop_" + name, [])))
        put = expand_data(spec.get("put_" + name, []))
        ids = drop | {entity["id"] for entity in put}
        result[name] = [entity for entity in result[name] if entity["id"] not in ids] + copy.deepcopy(put)
    read.exact_keys(spec.get("replace", {}), (), GRAPH_KEYS[2:])
    for name, value in spec.get("replace", {}).items():
        result[name] = expand_data(value)
    return canonical_graph(result)


def expected_result(case, graph, graphs):
    spec = case["expect"]
    if "not_modeled" in spec:
        return not_modeled_result(graph, spec["not_modeled"])
    if "error" in spec:
        return {"outcome": "reject", "error": spec["error"], "operations": [],
                "commit_frames": 0, "graph": graph, "not_modeled": NOT_MODELED}
    final = expand_graph(spec["graph"], graphs)
    ops = expand_data(spec["operations"])
    ttl = case.get("clock", {}).get("ttl_clock_ms", graph["ttl_clock_ms"])
    return {"outcome": "result", "columns": spec.get("columns", []), "rows": spec.get("rows", []),
            "ttl_clock_ms": ttl, "as_of_ms": ttl if graph["temporal_capable"] else None,
            "view_sequence": graph["view_sequence"], "revision": final["view_sequence"] if ops else None,
            "operations": ops, "commit_frames": int(bool(ops)),
            "counters": {"operations": len(ops), "body_bytes": spec["body_bytes"],
                         "reclaim_entries": spec.get("reclaim_entries", 0),
                         "binding_rows": spec.get("binding_rows", 1),
                         "detach_edges_examined": spec.get("detach_edges_examined", 0)},
            "graph": final, "not_modeled": NOT_MODELED}


def run_case(case, graphs):
    graph = expand_graph(case["graph"], graphs)
    return evaluate_write(graph, case["text"], read.expand_params(case.get("params", {})),
                          case.get("limits"), case.get("clock"))


def exact_equal(left, right):
    if isinstance(left, dict) and isinstance(right, dict):
        if left.keys() != right.keys():
            return False
        if left.get("tag") in ("Float", "GeoPoint") and left.get("tag") == right.get("tag"):
            return bit_equal(left, right)
        return all(exact_equal(left[key], right[key]) for key in left)
    if type(left) is type(right) and isinstance(left, (list, tuple)):
        return len(left) == len(right) and all(exact_equal(a, b) for a, b in zip(left, right))
    return type(left) is type(right) and left == right


def compare_result(actual, expected, context):
    if exact_equal(actual, expected):
        return

    def difference(left, right, path):
        if type(left) is not type(right):
            return path + ": type differs"
        if isinstance(left, dict):
            if left.keys() != right.keys():
                return path + ": keys differ"
            for key in left:
                if not exact_equal(left[key], right[key]):
                    return difference(left[key], right[key], path + "." + key)
        if isinstance(left, list):
            if len(left) != len(right):
                return f"{path}: length {len(left)} != {len(right)}"
            for index, (a, b) in enumerate(zip(left, right)):
                if not exact_equal(a, b):
                    return difference(a, b, f"{path}[{index}]")
        return f"{path}: {str(left)[:120]!r} != {str(right)[:120]!r}"

    raise ValueError(context + ": " + difference(actual, expected, "result"))


def validate_schema(schema):
    read.exact_keys(schema, ("nodes", "edges"))
    for kind in ("nodes", "edges"):
        read.require(isinstance(schema[kind], list), "schema-array")
        names = set()
        for rule in schema[kind]:
            read.exact_keys(rule, ("label", "properties") if kind == "nodes"
                            else ("type", "properties", "endpoints", "unique_triple"))
            name = rule["label" if kind == "nodes" else "type"]
            read.require(isinstance(name, str) and re.fullmatch(r"[A-Za-z0-9_]{1,63}", name)
                         and name not in names, "schema-name")
            names.add(name)
            read.require(isinstance(rule["properties"], dict), "schema-properties")
            for name, field in rule["properties"].items():
                read.require(isinstance(name, str) and 1 <= len(name.encode("utf-8")) <= 256, "schema-field")
                read.exact_keys(field, ("type", "required", "unique") if kind == "nodes" else ("type", "required"))
                read.require(field["type"] in ("String", "Integer", "Float", "Boolean", "GeoPoint"), "schema-type")
                read.require(type(field["required"]) is bool and
                             (kind == "edges" or type(field["unique"]) is bool), "schema-boolean")
            if kind == "edges":
                read.require(type(rule["unique_triple"]) is bool, "schema-boolean")
                read.require(isinstance(rule["endpoints"], list), "schema-endpoints")
                for pair in rule["endpoints"]:
                    read.require(isinstance(pair, list) and len(pair) == 2 and
                                 all(isinstance(x, str) and re.fullmatch(r"[A-Za-z0-9_]{1,63}", x) for x in pair),
                                 "schema-endpoints")


def validate_graph(graph):
    read.exact_keys(graph, GRAPH_KEYS)
    read.validate_read_graph(read_graph(graph))
    for kind in ("node", "edge"):
        read.decimal(graph["next_" + kind + "_id"], code="next-id")
        entities = graph["nodes" if kind == "node" else "edges"]
        read.require(all(int(entity["id"]) < int(graph["next_" + kind + "_id"]) for entity in entities), "high-water")
        for entity in entities:
            read.require(int(entity["revision"]) <= int(graph["view_sequence"]), "future-revision")
            try:
                properties_size(entity["properties"])
            except read.ReadFailure as error:
                raise ValueError("fixture: properties-size") from error
    validate_schema(graph["schema"])
    read.require(isinstance(graph["records"], list) and isinstance(graph["attachments"], list), "attachment-array")
    records, attached_records, attached_nodes = set(), set(), set()
    for record in graph["records"]:
        read.exact_keys(record, ("collection", "id", "payload", "revision"))
        read.require(isinstance(record["collection"], str) and bool(record["collection"]), "collection")
        read.decimal(record["id"], code="record-id")
        read.decimal(record["revision"], code="record-revision")
        identity = record["collection"], record["id"]
        read.require(identity not in records, "duplicate-record")
        records.add(identity)
    node_ids = {n["id"] for n in graph["nodes"]}
    for attachment in graph["attachments"]:
        read.exact_keys(attachment, ("node_id", "collection", "record_id"))
        identity = attachment["collection"], attachment["record_id"]
        owner = attachment["node_id"], attachment["collection"]
        read.require(attachment["node_id"] in node_ids and identity in records, "dangling-attachment")
        read.require(identity not in attached_records and owner not in attached_nodes, "duplicate-attachment")
        attached_records.add(identity)
        attached_nodes.add(owner)


def validate_operation(op):
    kind = op.get("op")
    if kind in ("CreateNode", "CreateEdge"):
        read.read_entity({key: value for key, value in op.items() if key != "op"}, edge=kind == "CreateEdge")
        read.exact_keys(op, ("op", "id", "key", "properties", "revision") +
                        (("labels",) if kind == "CreateNode" else ("source", "target", "type")))
        return
    read.require(kind in ("PatchNode", "PatchEdge", "DeleteNode", "DeleteEdge"), "operation-kind")
    required = ("op", "id", "expected")
    if kind in ("PatchNode", "PatchEdge"):
        required += ("set", "unset")
        if kind == "PatchNode":
            required += ("add_labels", "remove_labels")
    read.exact_keys(op, required)
    read.decimal(op["id"], code="operation-id")
    expected = op["expected"]
    if expected != "any":
        read.exact_keys(expected, ("revision",))
        read.decimal(expected["revision"], code="expected-revision")
    if kind.startswith("Patch"):
        read.read_properties(op["set"])
        unset = op["unset"]
        read.require(isinstance(unset, list) and all(isinstance(x, str) and
                     1 <= len(x.encode("utf-8")) <= 256 for x in unset), "unset")
        read.require(unset == sorted(set(unset), key=lambda x: x.encode("utf-8"))
                     and not set(unset) & set(op["set"]), "unset")
        if kind == "PatchNode":
            for name in ("add_labels", "remove_labels"):
                read.require(isinstance(op[name], list) and op[name] == sorted(set(op[name]))
                             and len(op[name]) <= 8 and
                             all(re.fullmatch(r"[A-Za-z0-9_]{1,63}", x) for x in op[name]), "patch-labels")
            read.require(not set(op["add_labels"]) & set(op["remove_labels"]), "patch-labels")


GROUPS = frozenset(("create", "merge", "patch", "delete", "detach", "binding",
                    "schema", "time", "limits", "atomicity", "validation", "not-modeled"))
ERRORS = frozenset((
    "Syntax", "Unsupported", "Parameter", "Limit", "InvalidMetadata",
    "InvalidQuery", "GraphLimitExceedsCeiling", "GraphKeyExists", "GraphKeyConflict",
    "GraphSchemaViolation", "GraphNodeHasEdges", "GraphNodeNotFound",
    "GraphEdgeNotFound", "BatchTooLarge", "IdSpaceExhausted", "UnsupportedFeature",
))


def validate_fixture(document, verify_hash=True):
    read.exact_keys(document, ("schema", "version", "content_hash", "graphs", "cases"))
    read.require(document["schema"] == "vlite.graph-subset-write"
                 and type(document["version"]) is int and document["version"] == 1, "write-schema")
    read.require(isinstance(document["graphs"], dict) and bool(document["graphs"]), "graphs")
    for name in document["graphs"]:
        read.require(re.fullmatch(r"[a-z0-9][a-z0-9-]*", name), "graph-id")
        validate_graph(expand_graph(name, document["graphs"]))
    read.require(isinstance(document["cases"], list) and bool(document["cases"]), "cases-array")
    ids = set()
    for case in document["cases"]:
        read.exact_keys(case, ("id", "group", "origin", "cite", "graph", "text", "params", "expect"),
                        ("limits", "clock"))
        read.require(isinstance(case["id"], str) and re.fullmatch(r"[a-z0-9][a-z0-9-]*", case["id"])
                     and case["id"] not in ids, "case-id")
        ids.add(case["id"])
        read.require(isinstance(case["group"], str) and case["group"] in GROUPS, "case-group")
        read.require(case["origin"] == "local" and isinstance(case["cite"], str) and bool(case["cite"]), "case-provenance")
        read.require(isinstance(case["text"], str), "case-text")
        read.require(isinstance(case["params"], dict), "params")
        read.validate_fixture_params(case["params"])
        read.validate_read_clock(case.get("clock", {}))
        read.exact_keys(case.get("limits", {}), (), tuple(read.READ_DEFAULTS))
        read.require(all(type(x) is int and 0 <= x < 2**64 for x in case.get("limits", {}).values()), "limit-value")
        graph = expand_graph(case["graph"], document["graphs"])
        validate_graph(graph)
        spec = case["expect"]
        if "not_modeled" in spec:
            read.exact_keys(spec, ("not_modeled",))
            read.require(spec["not_modeled"] in (["execution clock controls"],
                                                ["commit sequence exhaustion"]), "not-modeled")
        elif "error" in spec:
            read.exact_keys(spec, ("error",))
            error = spec["error"]
            read.exact_keys(error, ("kind",), ("reason", "offset", "feature"))
            read.require(error["kind"] in ERRORS, "error-kind")
            if "offset" in error:
                read.require(type(error["offset"]) is int and 0 <= error["offset"] <= len(case["text"].encode("utf-8")), "error-offset")
            if "reason" in error:
                read.require(error["kind"] == "InvalidQuery" and error["reason"] in
                             read.READ_REASONS | {"subset binding was deleted", "subset temporal mutation unsupported"}, "error-reason")
        else:
            read.exact_keys(spec, ("graph", "operations", "body_bytes"),
                            ("columns", "rows", "binding_rows", "reclaim_entries", "detach_edges_examined"))
            validate_graph(expand_graph(spec["graph"], document["graphs"]))
            read.require(isinstance(spec["operations"], list), "operations")
            ops = expand_data(spec["operations"])
            read.require(len(ops) <= OP_CAP, "operation-count")
            for op in ops:
                validate_operation(op)
            for name in ("body_bytes", "binding_rows", "reclaim_entries", "detach_edges_examined"):
                value = spec.get(name, 0)
                read.require(type(value) is int and value >= 0, "counter-value")
            read.require(spec["body_bytes"] <= BODY_CAP and bool(spec["body_bytes"]) == bool(ops), "body-bytes")
            read.require(spec.get("reclaim_entries", 0) <= 1, "reclaim-count")
            columns, rows = spec.get("columns", []), spec.get("rows", [])
            read.require(isinstance(columns, list) and all(isinstance(c, str) for c in columns)
                         and len(set(columns)) == len(columns), "columns")
            read.require(isinstance(rows, list), "rows")
            for row in rows:
                read.require(isinstance(row, list) and len(row) == len(columns), "row-width")
                for cell in row:
                    read.read_cell(cell)
    read.require(isinstance(document["content_hash"], str) and
                 re.fullmatch(r"[0-9a-f]{64}", document["content_hash"]), "hash-shape")
    if verify_hash:
        read.require(read.content_hash(document) == document["content_hash"], "write-hash")
    return document


def validate_mutants(document, cases):
    read.exact_keys(document, ("schema", "version", "content_hash", "mutants"))
    read.require(document["schema"] == "vlite.graph-subset-write-mutants"
                 and type(document["version"]) is int and document["version"] == 1, "mutant-schema")
    read.require(isinstance(document["mutants"], list), "mutants-array")
    seen = set()
    ids = {case["id"] for case in cases}
    for mutant in document["mutants"]:
        read.exact_keys(mutant, ("name", "change", "cases"))
        name = mutant["name"]
        read.require(isinstance(name, str) and name in MUTANTS and name not in seen, "mutant-name")
        seen.add(name)
        read.require(isinstance(mutant["change"], str) and bool(mutant["change"]), "mutant-change")
        read.require(isinstance(mutant["cases"], list) and bool(mutant["cases"])
                     and all(isinstance(case, str) for case in mutant["cases"])
                     and len(set(mutant["cases"])) == len(mutant["cases"]), "mutant-cases")
        read.require(set(mutant["cases"]) <= ids, "mutant-case-id")
    read.require(seen == set(MUTANTS), "mutant-coverage")
    read.require(read.content_hash(document) == document["content_hash"], "mutant-hash")


def mutation_trials(document, cases, graphs):
    global MUTATION
    validate_mutants(document, cases)
    lookup = {case["id"]: case for case in cases}
    named_kills = 0
    try:
        for mutant in document["mutants"]:
            MUTATION = mutant["name"]
            for case_id in mutant["cases"]:
                case = lookup[case_id]
                actual = run_case(case, graphs)
                expected = expected_result(case, expand_graph(case["graph"], graphs), graphs)
                if exact_equal(actual, expected):
                    raise ValueError(f"surviving mutant {MUTATION}: {case_id}")
                named_kills += 1
    finally:
        MUTATION = None
    return len(document["mutants"]), named_kills


def malformed_trials(document, mutants):
    small = copy.deepcopy(document)
    small["graphs"] = {"empty": small["graphs"]["empty"]}
    small["cases"] = [small["cases"][0]]
    small["content_hash"] = read.content_hash(small)
    trials = []
    for path, replacement in (
        (("extra",), True), (("version",), True), (("schema",), "unknown"),
        (("content_hash",), "short"), (("content_hash",), "0" * 64),
        (("graphs", "empty", "extra"), True), (("graphs", "empty", "next_node_id"), "-1"),
        (("graphs", "empty", "schema", "extra"), True),
        (("cases", 0, "extra"), True), (("cases", 0, "id"), "BAD"),
        (("cases", 0, "group"), "unknown"), (("cases", 0, "origin"), "external"),
        (("cases", 0, "cite"), ""), (("cases", 0, "graph"), "unknown"),
        (("cases", 0, "limits"), {"unknown": 1}),
        (("cases", 0, "limits"), {"write_deadline_ms": True}),
        (("cases", 0, "clock"), {"unknown": 1}),
        (("cases", 0, "expect", "extra"), True),
        (("cases", 0, "expect", "body_bytes"), -1),
        (("cases", 0, "expect", "body_bytes"), BODY_CAP + 1),
        (("cases", 0, "expect", "operations", 0, "extra"), True),
        (("cases", 0, "expect", "operations", 0, "revision"), "-1"),
        (("cases", 0, "expect", "operations", 0, "properties"), {"bad": {"tag": "Null"}}),
        (("cases", 0, "expect", "graph", "replace", "next_node_id"), "1"),
        (("cases", 0, "expect", "graph", "put_nodes", 0, "revision"), "12"),
        (("cases", 0, "expect", "graph", "extra"), True),
        (("cases", 0, "expect", "columns"), ["x", "x"]),
        (("cases", 0, "expect", "rows"), [[{"tag": "Null"}]]),
        (("cases", 0, "expect", "reclaim_entries"), 2),
    ):
        bad = copy.deepcopy(small)
        target = bad
        for key in path[:-1]:
            target = target[key]
        target[path[-1]] = replacement
        trials.append(bad)
    bad = copy.deepcopy(small)
    bad["cases"].append(copy.deepcopy(bad["cases"][0]))
    trials.append(bad)
    bad = copy.deepcopy(small)
    bad["graphs"]["empty"] = {"base": "empty"}
    trials.append(bad)
    bad = copy.deepcopy(small)
    bad["cases"][0]["expect"]["operations"] = [{"series": 10001, "start": 1, "template": {}}]
    trials.append(bad)
    bad = copy.deepcopy(small)
    bad["cases"][0]["expect"]["operations"] = [{"series": True, "start": 1, "template": {}}]
    trials.append(bad)
    bad = copy.deepcopy(small)
    bad["cases"][0]["expect"]["operations"] = [
        {"series": 1, "start": 1, "template": [{"series": 1, "start": 1, "template": {}}]}]
    trials.append(bad)
    for bad in trials:
        try:
            validate_fixture(bad)
        except ValueError:
            pass
        else:
            raise ValueError("malformed write fixture accepted")
    count = len(trials)
    for path, replacement in (
        (("extra",), True), (("schema",), "unknown"), (("version",), True),
        (("content_hash",), "0" * 64), (("mutants", 0, "name"), "unknown"),
        (("mutants", 0, "change"), ""), (("mutants", 0, "cases"), []),
        (("mutants", 0, "cases"), ["unknown"]), (("mutants", 0, "extra"), True),
    ):
        bad = copy.deepcopy(mutants)
        target = bad
        for key in path[:-1]:
            target = target[key]
        target[path[-1]] = replacement
        try:
            validate_mutants(bad, document["cases"])
        except ValueError:
            pass
        else:
            raise ValueError("malformed write mutant map accepted")
        count += 1
    for raw in ('{"x":1,"x":2}', '{"x":NaN}', '{"x":'):
        try:
            read.unique_json(raw)
        except ValueError:
            pass
        else:
            raise ValueError("malformed raw write fixture accepted")
        count += 1
    return count


def tampered_expectation_trials(document):
    # Rehash valid-but-wrong goldens so hash validation is not the only guard.
    for path, replacement in (
        (("body_bytes",), 33),
        (("graph", "put_nodes", 0, "key"), "wrong"),
        (("operations", 0, "labels"), ["Wrong"]),
    ):
        bad = copy.deepcopy(document)
        bad["graphs"] = {"empty": bad["graphs"]["empty"]}
        bad["cases"] = [bad["cases"][0]]
        target = bad["cases"][0]["expect"]
        for key in path[:-1]:
            target = target[key]
        target[path[-1]] = replacement
        bad["content_hash"] = read.content_hash(bad)
        validate_fixture(bad)
        case = bad["cases"][0]
        expected = expected_result(case, expand_graph(case["graph"], bad["graphs"]), bad["graphs"])
        read.require(not exact_equal(run_case(case, bad["graphs"]), expected), "tampered-golden-not-detected")
    return 3


def boundary_trials():
    count = 0
    # Fifteen one-byte names with 4096-byte strings: 4 + 15*4104 = 61564.
    # p adds 8+3963/3964/3965 -> 65535/65536/65537.
    big = {"tag": "String", "value": "x" * 4096}
    for tail, expected in ((3963, 65535), (3964, 65536), (3965, None)):
        properties = {name: big for name in "abcdefghijklmno"}
        properties["p"] = {"tag": "String", "value": "y" * tail}
        try:
            actual = properties_size(properties)
        except read.ReadFailure as error:
            read.same(error.result(), {"kind": "InvalidMetadata"}, "metadata cap+1")
            read.require(expected is None, "metadata cap rejected")
        else:
            read.same(actual, expected, "metadata exact cap")
        count += 1
    for fields in (63, 64, 65):
        properties = {f"p{i}": {"tag": "Boolean", "value": True} for i in range(fields)}
        try:
            properties_size(properties)
        except read.ReadFailure as error:
            read.same(error.result(), {"kind": "InvalidMetadata"}, "field cap+1")
            read.require(fields == 65, "field cap rejected")
        else:
            read.require(fields <= 64, "field cap accepted")
        count += 1
    for size in (255, 256, 257):
        try:
            key_valid("k" * size)
        except read.ReadFailure:
            read.require(size == 257, "key cap rejected")
        else:
            read.require(size <= 256, "key cap accepted")
        count += 1
    for labels in (7, 8, 9):
        try:
            labels_size([f"L{i}" for i in range(labels)])
        except read.ReadFailure:
            read.require(labels == 9, "label cap rejected")
        else:
            read.require(labels <= 8, "label cap accepted")
        count += 1
    # u16 unset length, including a 256-byte UTF-8 field; empty metadata=4.
    op = {"op": "PatchEdge", "set": {}, "unset": ["x" * 256]}
    read.same(operation_size(op), 283, "unset u16: 8+8+4+4+1+2+256")
    count += 1
    # Exact canonical float bits must survive full-state comparisons, not only
    # the keyed-create comparator.
    read.require(not exact_equal({"tag": "Float", "value": 0.0},
                                 {"tag": "Float", "value": -0.0}), "signed-zero comparison")
    return count + 1


def self_test(directory):
    document = validate_fixture(load_json(directory / "write-cases.json"))
    pristine = copy.deepcopy(document)
    ids = set()
    for case in document["cases"]:
        read.require(case["id"] not in ids, "duplicate-id")
        ids.add(case["id"])
        read.require(case["origin"] == "local" and bool(case["cite"]), "case-provenance")
        graph = expand_graph(case["graph"], document["graphs"])
        original = copy.deepcopy(graph)
        params = read.expand_params(case["params"])
        controls = copy.deepcopy((params, case.get("limits"), case.get("clock")))
        actual = evaluate_write(graph, case["text"], params, case.get("limits"), case.get("clock"))
        compare_result(actual, expected_result(case, graph, document["graphs"]), case["id"])
        compare_result(graph, original, case["id"] + " input unchanged")
        compare_result((params, case.get("limits"), case.get("clock")), controls, case["id"] + " controls unchanged")
    mutants = load_json(directory / "write-mutant-map.json")
    malformed = malformed_trials(document, mutants)
    tampered = tampered_expectation_trials(document)
    boundaries = boundary_trials()
    for seed in (17, 941, 2026, 73, 65537):
        cases = list(document["cases"])
        randomizer = random.Random(seed)
        randomizer.shuffle(cases)
        for case in cases:
            graph = expand_graph(case["graph"], document["graphs"])
            if len(graph["nodes"]) + len(graph["edges"]) > 20:
                continue
            randomizer.shuffle(graph["nodes"])
            randomizer.shuffle(graph["edges"])
            params = list(read.expand_params(case["params"]).items())
            randomizer.shuffle(params)
            expected = expected_result(case, graph, document["graphs"])
            if expected["outcome"] == "reject" or not expected["operations"]:
                expected["graph"] = copy.deepcopy(graph)
            compare_result(evaluate_write(graph, case["text"], dict(params), case.get("limits"), case.get("clock")),
                           expected, case["id"] + " shuffle")
    killed, named = mutation_trials(mutants, document["cases"], document["graphs"])
    restored = {case_id for mutant in mutants["mutants"] for case_id in mutant["cases"]}
    for case in document["cases"]:
        if case["id"] in restored:
            expected = expected_result(case, expand_graph(case["graph"], document["graphs"]), document["graphs"])
            compare_result(run_case(case, document["graphs"]), expected, case["id"] + " restored")
    for case in document["cases"][:5]:
        params = read.expand_params(case["params"])
        statement = read.parse(case["text"], params)
        graph = expand_graph(case["graph"], document["graphs"])
        compare_result(evaluate_write(graph, statement, params),
                       expected_result(case, graph, document["graphs"]), case["id"] + " AST input")
    compare_result(document, pristine, "fixture immutability")
    read.require(len(ids) >= 150 and killed == len(MUTANTS), "coverage minimum")
    print(f"write-cases={len(ids)} malformed={malformed} boundaries={boundaries} "
          f"tampered-goldens={tampered} shuffle-seeds=5 restored-cases={len(restored)} "
          f"mutants-killed={killed} named-kills={named} survived=0 equivalent=0")
    print("SELF-TEST OK")


def main():
    cli = argparse.ArgumentParser(description=__doc__)
    cli.add_argument("--self-test", action="store_true", required=True)
    cli.add_argument("--fixture", type=Path, default=FIXTURE)
    args = cli.parse_args()
    try:
        self_test(args.fixture)
    except (ValueError, KeyError, OSError, read.Diagnostic, read.ReadFailure) as error:
        print(f"SELF-TEST FAILED: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
