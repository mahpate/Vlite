"""Independent logical oracle for the PROPOSED X1 contract, not Rust parity.

Authority: docs/projects/roadmap/gap1-x1-logical-delete-contract.md.
Uses the accepted C1 reference only for unchanged primitives, normalization and
content/node sizing. Never imports or modifies Rust or an accepted fixture.
Setup, raw edits, JSON reopen and forget are fixture operations, not APIs.
Clocks, graph IDs, revisions and attachment owners are explicit fixture inputs.
Source content chunk revisions default to their owner's fixture graph revision;
content_chunk_revisions overrides them, and content_attachments injects a block.
HAS_CONTENT edges of setup sources have explicit fixture IDs. Generated edges
receive monotonic fixture IDs, not predictions of actual engine allocation.
opening_reclaims explicitly charges the one optional opening reclaim envelope.
No I/O rollback, real-backend durability, concurrency or performance is modeled.
Run with --self-test, also under -O; otherwise read one fixture case on stdin.
"""

import argparse
import copy
import hashlib
import json
from pathlib import Path
import re
import struct
import sys

import c1_consolidation_oracle as c1


FIXTURE = Path(__file__).parent / "fixtures" / "agent_memory" / "x1_golden.json"
SCHEMA = "consolidate-x1:v2"
SOURCE_DOMAIN = b"vlite.memory.consolidate-x1.sources.v2\0"
REQUEST_DOMAIN = b"vlite.memory.consolidate-x1.request.v2\0"
OUTCOME_DOMAIN = b"vlite.memory.consolidate-x1.outcome.v2\0"
EDGE_DOMAIN = b"vlite.memory.consolidate-x1.edge.v2\0"
DISPOSITIONS = {"retain": 0, "logical_delete": 1}
DIGEST = re.compile(r"v2:[0-9a-f]{64}\Z")
RECEIPT_FIELDS = {
    "scope": str, "x1_scope_tuple": str, "x1_schema": str,
    "x1_collection": str, "digest": str, "expires_at_ms": str, "outcome_chunks": int,
}
require, ensure = c1.require, c1.ensure


def digest(value):
    return "v2:" + hashlib.sha256(value).hexdigest()


def normalize(raw, mutation=None):
    require(isinstance(raw, dict), "InvalidBundle")
    adapted = copy.deepcopy(raw)
    dry_run = adapted.pop("dry_run", False)
    require(type(dry_run) is bool, "InvalidBundle")
    inputs = adapted.get("inputs")
    # C1 retains its structural scalar checks; only the disposition is widened.
    if isinstance(inputs, list):
        for item in inputs:
            if isinstance(item, dict):
                item["disposition"] = "retain"
    result = c1.normalize(adapted)
    dispositions = {}
    for item in raw["inputs"]:
        disposition = item.get("disposition")
        require(type(disposition) is str and disposition in DISPOSITIONS, "InvalidBundle")
        dispositions[item["id"]] = disposition
    for item in result["inputs"]:
        item["disposition"] = dispositions[item["id"]]
    sb = c1.framed(c1.scope_bytes(result["scope"]))
    encoded = c1.u32(len(result["inputs"])) + b"".join(
        c1.s(item["id"]) + c1.u64(item["expected_memory_revision"])
        + bytes([0 if mutation == "disposition_unhashed"
                 else DISPOSITIONS[item["disposition"]]])
        for item in result["inputs"])
    # C1 canonical_bytes' tail is identical by the proposed contract, not by
    # an implementation-dependent search/replace of byte sequences.
    _, old = c1.canonical_bytes(result)
    offset = len(b"vlite.memory.consolidate.request.v1\0") + len(sb) + len(encoded)
    result["source_bytes"] = SOURCE_DOMAIN + sb + encoded
    result["request_bytes"] = REQUEST_DOMAIN + sb + encoded + old[offset:]
    require(len(result["request_bytes"]) <= c1.LIMIT, "BodyLimit")
    result["source_digest"] = digest(result["source_bytes"])
    result["request_digest"] = digest(result["request_bytes"])
    result["dry_run"] = dry_run
    return result


def manifest(inputs):
    entries = sorted((bytes.fromhex(c1.sha(c1.utf8(item["id"]))),
                      item["expected_memory_revision"], DISPOSITIONS[item["disposition"]])
                     for item in inputs)
    require(len({entry[0] for entry in entries}) == len(entries), "DigestCollision")
    return (c1.u32(len(entries)) + b"".join(
        identity + c1.u64(revision) + bytes([disposition])
        for identity, revision, disposition in entries)).hex()


def decode_manifest(value, count):
    require(type(value) is str and len(value) == 2 * (4 + 41 * count)
            and re.fullmatch(r"[0-9a-f]+", value) is not None, "InvalidBundle")
    raw = bytes.fromhex(value)
    require(struct.unpack(">I", raw[:4])[0] == count and 1 <= count <= 256, "InvalidBundle")
    result, previous = [], b""
    for offset in range(4, len(raw), 41):
        identity = raw[offset:offset + 32]
        revision = struct.unpack(">Q", raw[offset + 32:offset + 40])[0]
        disposition = raw[offset + 40]
        require(identity > previous and revision > 0 and disposition in (0, 1),
                "InvalidBundle")
        result.append((identity.hex(), revision, disposition))
        previous = identity
    return result


def manifest_properties(inputs):
    encoded = manifest(inputs)
    values = {"x1_sources_" + format(index // 4096, "04x"): encoded[index:index + 4096]
              for index in range(0, len(encoded), 4096)}
    return dict(values, x1_source_chunks=len(values))


def stored_manifest(properties):
    count = properties.get("x1_source_chunks")
    require(type(count) is int and 1 <= count <= 6, "InvalidBundle")
    keys = {"x1_sources_" + format(index, "04x") for index in range(count)}
    require({key for key in properties if key.startswith("x1_sources")} == keys,
            "InvalidBundle")
    chunks = [properties["x1_sources_" + format(index, "04x")] for index in range(count)]
    require(all(type(value) is str and 0 < len(value) <= 4096
                and len(value) % 2 == 0
                and (index == count - 1 or len(value) == 4096)
                for index, value in enumerate(chunks)), "InvalidBundle")
    return "".join(chunks)


def outcome_bytes(outcome):
    return (OUTCOME_DOMAIN + c1.s(outcome["id"]) + c1.u64(outcome["revision"])
            + c1.u32(outcome["N"]) + c1.s(outcome["source_digest"])
            + c1.u32(len(outcome["deleted"])) + b"".join(
                c1.s(value) for value in outcome["deleted"])
            + c1.u32(len(outcome["retained_shared"])) + b"".join(
                c1.s(value) for value in outcome["retained_shared"]))


class Reader:
    def __init__(self, value):
        self.value, self.offset = value, 0

    def take(self, count):
        require(count >= 0 and self.offset + count <= len(self.value), "InvalidBundle")
        result = self.value[self.offset:self.offset + count]
        self.offset += count
        return result

    def number(self, width):
        return int.from_bytes(self.take(width), "big")

    def text(self, maximum=256):
        count = self.number(4)
        require(1 <= count <= maximum, "InvalidBundle")
        try:
            result = self.take(count).decode("utf-8")
        except UnicodeDecodeError:
            raise c1.Rejected("InvalidBundle") from None
        require(c1.text(result, 1, maximum), "InvalidBundle")
        return result

    def strings(self):
        count = self.number(4)
        require(count <= 256, "InvalidBundle")
        values = [self.text() for _ in range(count)]
        require(values == sorted(set(values), key=c1.utf8), "InvalidBundle")
        return values


def decode_outcome(value):
    reader = Reader(value)
    require(reader.take(len(OUTCOME_DOMAIN)) == OUTCOME_DOMAIN, "InvalidBundle")
    result = {"id": reader.text(), "revision": reader.number(8),
              "N": reader.number(4), "source_digest": reader.text(67),
              "deleted": reader.strings(), "retained_shared": reader.strings()}
    require(result["revision"] == 1 and 1 <= result["N"] <= 256
            and DIGEST.fullmatch(result["source_digest"]) is not None
            and len(result["deleted"]) <= result["N"]
            and result["id"] not in result["deleted"]
            and reader.offset == len(value), "InvalidBundle")
    return result


def receipt_key(request):
    return ("receipt:v1:" + c1.scope_key(request["scope"]) + ":"
            + c1.sha(b"consolidate-x1:v1\0" + c1.utf8(request["idempotency_key"])))


def chunk_key(key, index):
    return "receipt-outcome:v1:" + c1.sha(c1.utf8(key)) + ":" + format(index, "04x")


def receipt_nodes(request, collection, outcome):
    key = receipt_key(request)
    encoded = outcome_bytes(outcome).hex()
    chunks = [c1.node(chunk_key(key, index // 4096), "VliteMemoryReceiptOutcome", {
        "receipt_key": key, "index": index // 4096, "outcome": encoded[index:index + 4096],
    }) for index in range(0, len(encoded), 4096)]
    receipt = c1.node(key, "VliteMemoryReceipt", {
        "scope": c1.scope_key(request["scope"]),
        "x1_scope_tuple": c1.scope_bytes(request["scope"]).hex(),
        "x1_schema": SCHEMA, "x1_collection": collection or "",
        "digest": request["request_digest"],
        "expires_at_ms": str(request["C"] + request["receipt_window_ms"]),
        "outcome_chunks": len(chunks),
    })
    return receipt, chunks


def decode_receipt(value, request, collection):
    props = c1.exact_node(value["node"], "VliteMemoryReceipt", RECEIPT_FIELDS)
    key = receipt_key(request)
    require(value["node"]["key"] == key and props["scope"] == c1.scope_key(request["scope"])
            and props["x1_schema"] == SCHEMA, "InvalidBundle")
    require(props["x1_scope_tuple"] == c1.scope_bytes(request["scope"]).hex(),
            "DigestCollision")
    require(props["x1_collection"] == (collection or "")
            and DIGEST.fullmatch(props["digest"]) is not None
            and re.fullmatch(r"[1-9][0-9]{0,19}", props["expires_at_ms"]) is not None
            and int(props["expires_at_ms"]) <= c1.U64_MAX
            and 1 <= props["outcome_chunks"] <= 66
            and len(value["outcomes"]) == props["outcome_chunks"], "InvalidBundle")
    encoded = ""
    for index, chunk in enumerate(value["outcomes"]):
        cp = c1.exact_node(chunk, "VliteMemoryReceiptOutcome", c1.OUTCOME_FIELDS)
        require(chunk["key"] == chunk_key(key, index) and cp["receipt_key"] == key
                and cp["index"] == index
                and re.fullmatch(r"[0-9a-f]{2,4096}", cp["outcome"]) is not None
                and len(cp["outcome"]) % 2 == 0
                and (index == len(value["outcomes"]) - 1 or len(cp["outcome"]) == 4096),
                "InvalidBundle")
        encoded += cp["outcome"]
    return props, decode_outcome(bytes.fromhex(encoded))


def entry_len(entry):
    fixed = {"DeleteNode": 16, "DeleteEdge": 16, "Detach": 20, "VectorDelete": 28}
    if "properties" in entry:
        require(len(entry["properties"]) <= 64
                and all(c1.text(key) and (type(value) is not str or c1.text(value, 0, 4096))
                        for key, value in entry["properties"].items()), "BodyLimit")
    return fixed[entry["op"]] if entry["op"] in fixed else c1.entry_len(entry)


def same_scope(left, right):
    return c1.scope_bytes(left) == c1.scope_bytes(right)


def visible(memory, clock):
    return memory["expiry_ms"] is None or clock < memory["expiry_ms"]


class Model(c1.Model):
    def __init__(self, setup, scope, mutation=None):
        super().__init__(setup, scope)
        self.mutation = mutation
        self.fail_preappend = setup.get("fail_preappend", False)
        self.edges = copy.deepcopy(setup.get("edges", []))
        for index, source in enumerate(self.sources):
            source.setdefault("graph_revision", index + 1)
            source.setdefault("evidence", [])
        for edge in self.edges:
            edge.setdefault("scope", scope)
            edge.setdefault("relationship", "REFERENCES")
            edge.setdefault("revision", 1)
            edge.setdefault("expiry_ms", None)
        ensure(len({edge["id"] for edge in self.edges}) == len(self.edges),
               "fixture edge IDs must be unique")
        self.next_edge_id = 1 + max((edge["id"] for edge in self.edges), default=0)

    def memories(self):
        return self.sources + self.summaries

    def find(self, scope, identity):
        candidates = [m for m in self.memories()
                      if same_scope(m["scope"], scope) and m["id"] == identity]
        require(len(candidates) <= 1, "InvalidBundle")
        return candidates[0] if candidates else None

    def edge_visible(self, edge, clock):
        return (edge["expiry_ms"] is None or clock < edge["expiry_ms"]) and all(
            memory is None or visible(memory, clock)
            for memory in (self.find(edge["scope"], edge[endpoint])
                           for endpoint in ("from", "to")))

    def plan(self, request, collection, old):
        c, scope, summary = request["C"], request["scope"], request["summary"]
        require(request.get("deadline") is None or c < request["deadline"], "DeadlineExceeded")
        require(request["allow_future_observations"] or summary["observed_at_ms"] <= c,
                "FutureObservation")
        require(summary["expiry_ms"] is None or c < summary["expiry_ms"], "MissingMemory")
        selected = []
        for item in request["inputs"]:
            memory = self.find(scope, item["id"])
            require(memory is not None and visible(memory, c), "MissingMemory")
            require(not memory["malformed"], "InvalidBundle")
            require(memory["kind"] in ("entity", "episodic"), "UnsupportedKind")
            require(self.mutation == "revision_unchecked"
                    or memory["revision"] == item["expected_memory_revision"], "RevisionConflict")
            selected.append(memory)
        for memory in self.memories():
            if same_scope(memory["scope"], scope) and visible(memory, c):
                require(memory["id"] != summary["id"] and not (
                    memory["kind"] == "episodic" and memory["content"] == summary["content"]
                    and memory["source"] == summary["source"]), "IdentityConflict")
        if request["evidence"] is not None:
            records = [r for r in self.records if r["id"] == request["evidence"]["id"]
                       and r["collection"] == collection]
            require(collection is not None and len(records) == 1, "InvalidEvidence")
            require(same_scope(records[0]["scope"], scope), "CrossScope")
            require(not records[0]["owners"], "RecordAlreadyAttached")
        deleted = [item["id"] for item in request["inputs"]
                   if item["disposition"] == "logical_delete"]
        dead = set(deleted)
        live = {m["id"]: m for m in self.memories()
                if same_scope(m["scope"], scope) and visible(m, c)}
        reachable, pending = set(live) - dead, list(set(live) - dead)
        while pending:
            identity = pending.pop()
            for edge in self.edges:
                if (same_scope(edge["scope"], scope) and edge["from"] == identity
                        and edge["relationship"] == "REFERENCES"
                        and (edge["expiry_ms"] is None or c < edge["expiry_ms"])
                        and edge["to"] in live and edge["to"] not in reachable):
                    reachable.add(edge["to"])
                    pending.append(edge["to"])
        shared = {evidence for identity in reachable for evidence in live[identity]["evidence"]}
        cleanups, kept = [], []
        for memory in selected:
            if memory["id"] not in dead:
                continue
            require(not memory.get("content_attachments"), "ForeignAttachment")
            actual = [r for r in self.records if same_scope(r["scope"], scope)
                      and memory["id"] in r["owners"]]
            if self.mutation != "foreign_allowed":
                require(all(r["collection"] == collection and not r.get("foreign", False)
                            for r in actual), "ForeignAttachment")
                require(set(memory["attachments"]) <= set(memory["evidence"]),
                        "ForeignAttachment")
            require(len(memory["evidence"]) <= 1, "InvalidBundle")
            if not memory["evidence"]:
                require(not actual or self.mutation == "foreign_allowed", "ForeignAttachment")
                continue
            require(collection is not None, "InvalidEvidence")
            records = [r for r in self.records if r["collection"] == collection
                       and r["id"] == memory["evidence"][0]]
            require(len(records) == 1, "InvalidBundle")
            record = records[0]
            require(same_scope(record["scope"], scope), "CrossScope")
            require(self.mutation == "foreign_allowed" or (
                record["owners"] == [memory["id"]] and not record.get("foreign", False)),
                "ForeignAttachment")
            require(memory["attachments"] == memory["evidence"], "InvalidBundle")
            retain = record["id"] in shared and self.mutation != "shared_deleted"
            if retain:
                kept.append(record["id"])
            cleanups.append((record["id"], memory["id"], retain))
        outcome = {"id": summary["id"], "revision": 1, "N": len(selected),
                   "source_digest": request["source_digest"], "deleted": deleted,
                   "retained_shared": sorted(set(kept), key=c1.utf8)}
        entries = c1.graph_plan(request, collection)
        creates = [e for e in entries if e["role"] not in ("derived", "receipt", "outcome")]
        props = creates[0]["properties"]
        for key in list(props):
            if key.startswith("c1_"):
                del props[key]
        props.update(x1_schema=SCHEMA, x1_transformation=request["transformation"],
                     x1_source_digest=request["source_digest"], x1_source_count=len(selected))
        props.update(manifest_properties(request["inputs"]))
        retained = {i["id"] for i in request["inputs"] if i["disposition"] == "retain"}
        derived = []
        for item in request["inputs"]:
            if item["id"] not in retained:
                continue
            source = c1.memory_key(scope, summary["id"])
            target = c1.memory_key(scope, item["id"])
            derived.append({"op": "PutEdge", "role": "derived", "from": source, "to": target,
                            "key": "derived-x1:v2:" + c1.sha(
                                EDGE_DOMAIN + c1.s(source) + c1.s(target)),
                            "relationship": "DERIVED_FROM",
                            "properties": {"x1_schema": SCHEMA,
                                           "x1_source_memory_revision":
                                           str(item["expected_memory_revision"])}})
        attach = [e for e in creates if e["role"] == "attach"]
        entries = [e for e in creates if e["role"] != "attach"] + derived + attach
        for record_id, identity, retain in sorted(cleanups, key=lambda x: c1.utf8(x[0])):
            entries.append({"op": "Detach", "role": "detach", "id": record_id, "owner": identity})
            if not retain:
                entries.append({"op": "VectorDelete", "role": "vector_delete", "id": record_id})
        incident = sorted((e for e in self.edges if same_scope(e["scope"], scope)
                           and (e["from"] in dead or e["to"] in dead)
                           and self.edge_visible(e, c)),
                          key=lambda e: e["id"])
        if self.mutation == "incident_omitted":
            incident = incident[1:]
        entries += [{"op": "DeleteEdge", "role": "incident", "id": e["id"],
                     "expected_revision": e["revision"]} for e in incident]
        # Source content chunks use the accepted C1 scalar-safe packing; their
        # HAS_CONTENT edges are supplied explicitly in fixtures, never inferred.
        content_keys = sorted((
            ("content:v1:" + c1.sha(c1.utf8(c1.memory_key(scope, m["id"])))
             + ":" + format(index, "04x"),
             m.get("content_chunk_revisions", {}).get(str(index), m["graph_revision"]))
            for m in selected if m["id"] in dead
            for index in range(len(c1.content_chunks(m["content"])))), key=lambda x: c1.utf8(x[0]))
        for key, revision in content_keys:
            entries.append({"op": "DeleteNode", "role": "old_content",
                            "key": key, "expected_revision": revision})
        entries += [{"op": "DeleteNode", "role": "target", "id": m["id"],
                     "expected_revision": m["graph_revision"]}
                    for m in selected if m["id"] in dead]
        receipt, chunks = receipt_nodes(request, collection, outcome)
        previous = [] if old is None else old["outcomes"]
        if self.mutation != "obsolete_chunks_kept":
            entries += [{"op": "DeleteNode", "role": "obsolete_outcome", "key": node["key"]}
                        for node in previous[len(chunks):]]
        entries += [dict(node, op="PutNode", role="outcome") for node in chunks]
        entries.append(dict(receipt, op="PutNode", role="receipt"))
        physical = len(entries)
        logical = physical + len(deleted) + (1 + min(len(previous), len(chunks)) if old else 0)
        require(physical <= 4096 and (self.mutation == "delete_guard_free" or logical <= 4096),
                "OperationLimit")
        commit_clock = max(c, self.wall, self.high_water)
        require(all(visible(m, commit_clock) for m in selected)
                and (summary["expiry_ms"] is None or summary["expiry_ms"] > commit_clock),
                "fixture:EndpointRefused")
        opening = [{"op": "Reclaim", "role": "opening_reclaim"}] * self.opening_reclaims
        physical_entries = opening + entries
        frame_bytes = 4 + sum(entry_len(e) for e in physical_entries)
        require(frame_bytes <= c1.LIMIT, "BodyLimit")
        return {"outcome": outcome, "entries": physical_entries,
                "physical_ops": physical, "logical_ops": logical, "frame_bytes": frame_bytes,
                "receipt": receipt, "chunks": chunks, "clock": commit_clock,
                "cleanups": cleanups, "retained": sorted(retained, key=c1.utf8),
                "derived": [{"id": item["id"], "properties": {
                    "x1_schema": SCHEMA,
                    "x1_source_memory_revision": str(item["expected_memory_revision"]),
                }} for item in request["inputs"] if item["id"] in retained]}

    def write(self, raw):
        request = normalize(raw, self.mutation)
        sk = c1.scope_key(request["scope"])
        require(sk in self.scopes, "InvalidScope")
        binding = self.scopes[sk]
        require(binding["tuple"] == c1.scope_bytes(request["scope"]).hex(), "DigestCollision")
        key = receipt_key(request)
        old = self.receipts.get(key)
        if not request["dry_run"] and old is not None:
            props, outcome = decode_receipt(old, request, binding["collection"])
            if request["C"] < int(props["expires_at_ms"]):
                require(props["digest"] == request["request_digest"], "IdempotencyConflict")
                require(outcome["id"] == request["summary"]["id"]
                        and outcome["N"] == len(request["inputs"])
                        and outcome["source_digest"] == request["source_digest"]
                        and outcome["deleted"] == [i["id"] for i in request["inputs"]
                                                  if i["disposition"] == "logical_delete"],
                        "InvalidBundle")
                if self.mutation == "replay_checks_missing_source":
                    self.plan(request, binding["collection"], old)
                return {"status": "replayed", "outcome": outcome,
                        "sequence": old["sequence"], "receipt_revision": old["revision"],
                        "physical_ops": 0, "logical_ops": 0, "commit_frames": 0}
        try:
            plan = self.plan(request, binding["collection"], None if request["dry_run"] else old)
        except c1.Rejected as error:
            if self.mutation == "partial_failure":
                self.sequence += 1
            if not request["dry_run"]:
                raise
            return {"status": "plan", "conflicts": [str(error)], "physical_ops": None,
                    "logical_ops": None, "frame_bytes": None, "commit_frames": 0}
        result = {k: plan[k] for k in ("physical_ops", "logical_ops", "frame_bytes")}
        result.update(outcome_chunks=len(plan["chunks"]),
                      obsolete_outcomes=sum(e["role"] == "obsolete_outcome"
                                            for e in plan["entries"]))
        if request["dry_run"] and self.mutation != "dry_run_writes":
            return dict(result, status="plan", conflicts=[], commit_frames=0)
        # Publish only after every check. Explicit fixture injection represents
        # preappend refusal, never arbitrary I/O failure after a possible append.
        require(not self.fail_preappend, "fixture:PreappendRefused")
        dead, scope = set(plan["outcome"]["deleted"]), request["scope"]
        expired = {(c1.scope_key(m["scope"]), m["id"]) for m in self.memories()
                   if not visible(m, plan["clock"])}
        self.sources = [m for m in self.sources if not (same_scope(m["scope"], scope)
                                                       and m["id"] in dead)
                        and (c1.scope_key(m["scope"]), m["id"]) not in expired]
        self.summaries = [m for m in self.summaries if not (same_scope(m["scope"], scope)
                                                          and m["id"] in dead)
                          and (c1.scope_key(m["scope"]), m["id"]) not in expired]
        removed_edges = {e["id"] for e in plan["entries"] if e["role"] == "incident"}
        self.edges = [e for e in self.edges if e["id"] not in removed_edges
                      and (e["expiry_ms"] is None or e["expiry_ms"] > plan["clock"])
                      and all((c1.scope_key(e["scope"]), e[endpoint]) not in expired
                              for endpoint in ("from", "to"))]
        removed_records = {identity for identity, _, keep in plan["cleanups"] if not keep}
        self.records = [r for r in self.records if not (
            same_scope(r["scope"], scope) and r["collection"] == binding["collection"]
            and r["id"] in removed_records)]
        for identity, owner, _ in plan["cleanups"]:
            for record in self.records:
                if (same_scope(record["scope"], scope) and record["collection"] == binding["collection"]
                        and record["id"] == identity):
                    record["owners"] = [value for value in record["owners"] if value != owner]
        for record in self.records:
            record["owners"] = [owner for owner in record["owners"]
                                if (c1.scope_key(record["scope"]), owner) not in expired]
        for m in self.summaries:
            m["derived"] = [entry for entry in m["derived"]
                            if not (same_scope(m["scope"], scope) and entry["id"] in dead)
                            and (c1.scope_key(m["scope"]), entry["id"]) not in expired]
        self.sequence += 1
        self.high_water = plan["clock"]
        summary = copy.deepcopy(request["summary"])
        props = copy.deepcopy(next(e for e in plan["entries"] if e["role"] == "summary")["properties"])
        self.summaries.append(dict(summary, scope=scope, kind="episodic", revision=1,
                                   graph_revision=self.sequence, malformed=False,
                                   evidence=[] if request["evidence"] is None else [request["evidence"]["id"]],
                                   attachments=[] if request["evidence"] is None else [request["evidence"]["id"]],
                                   properties=props, derived=plan["derived"]))
        by_key = {c1.memory_key(scope, item["id"]): item["id"] for item in request["inputs"]}
        for entry in plan["entries"]:
            if entry["role"] not in ("derived", "content_edge"):
                continue
            self.edges.append({
                "id": self.next_edge_id, "scope": scope, "from": summary["id"],
                "to": by_key[entry["to"]] if entry["role"] == "derived" else entry["to"],
                "relationship": entry["relationship"], "revision": self.sequence,
                "expiry_ms": None,
            })
            self.next_edge_id += 1
        if request["evidence"] is not None:
            for record in self.records:
                if record["collection"] == binding["collection"] and record["id"] == request["evidence"]["id"]:
                    record["owners"] = [summary["id"]]
        self.receipts[key] = {"node": plan["receipt"], "outcomes": plan["chunks"],
                             "revision": 1 if old is None else old["revision"] + 1,
                             "sequence": self.sequence}
        return dict(result, status="created", outcome=plan["outcome"],
                    sequence=self.sequence, commit_frames=1, entries=plan["entries"],
                    request_digest=request["request_digest"],
                    source_digest=request["source_digest"], manifest=stored_manifest(props))

    def status(self, scope, identity, clock):
        summary = self.find(scope, identity)
        require(summary is not None and visible(summary, clock), "MissingMemory")
        props = summary["properties"]
        require(props.get("x1_schema") == SCHEMA
                and c1.text(props.get("x1_transformation"))
                and type(props.get("x1_source_count")) is int
                and DIGEST.fullmatch(props.get("x1_source_digest", "")) is not None,
                "InvalidBundle")
        sources = decode_manifest(stored_manifest(props), props["x1_source_count"])
        require(isinstance(summary["derived"], list), "InvalidBundle")
        retained = {}
        for edge in summary["derived"]:
            require(isinstance(edge, dict) and edge.keys() == {"id", "properties"}
                    and c1.text(edge["id"]) and type(edge["properties"]) is dict,
                    "InvalidBundle")
            fields = edge["properties"]
            require(fields.keys() == {"x1_schema", "x1_source_memory_revision"}
                    and fields["x1_schema"] == SCHEMA
                    and type(fields["x1_source_memory_revision"]) is str
                    and re.fullmatch(r"[1-9][0-9]{0,19}", fields["x1_source_memory_revision"])
                    and int(fields["x1_source_memory_revision"]) <= c1.U64_MAX,
                    "InvalidBundle")
            retained[c1.sha(c1.utf8(edge["id"]))] = edge
        expected = {identity for identity, _, disposition in sources if disposition == 0}
        require(len(retained) == len(summary["derived"]) and retained.keys() <= expected,
                "InvalidBundle")
        stale = False
        for source_hash, revision, disposition in sources:
            if disposition == 1:
                if self.mutation == "intentional_delete_stale":
                    stale = True
                continue
            edge = retained.get(source_hash)
            require(edge is None or int(edge["properties"]["x1_source_memory_revision"]) == revision,
                    "InvalidBundle")
            source = self.find(scope, edge["id"]) if edge else None
            stale |= source is None or not visible(source, clock) or source["revision"] != revision
        return {"stale": stale, "source_count": len(sources),
                "intentionally_deleted": sum(d == 1 for _, _, d in sources)}


def snapshot(model):
    return {key: copy.deepcopy(value) for key, value in model.__dict__.items() if key != "mutation"}


def projection(actual, expected):
    if isinstance(expected, dict):
        ensure(isinstance(actual, dict), "expected mapping")
        return {key: projection(actual[key], value) if key in actual
                else {"missing_key": key} for key, value in expected.items()}
    return actual


def run_case(case, base, mutation=None):
    setup = c1.expand(case.get("setup", {}))
    model = Model(setup, base["scope"], mutation)
    results = []
    for index, step in enumerate(case["steps"]):
        operation = step.get("op", "write")
        before = snapshot(model)
        try:
            if operation == "write":
                request = c1.expand(c1.merge(base, step.get("request", {})))
                result = model.write(request)
                if request.get("dry_run") or result["status"] == "replayed":
                    ensure(snapshot(model) == before, "dry run/replay changed state")
            elif operation == "status":
                result = model.status(base["scope"], step.get("id", "z"), step.get("C", 10))
            elif operation == "state":
                result = {"memories": sorted(m["id"] for m in model.memories()),
                          "records": sorted(r["id"] for r in model.records),
                          "owners": {r["id"]: r["owners"] for r in model.records},
                          "edges": sorted(e["id"] for e in model.edges), "sequence": model.sequence}
            elif operation == "edit":
                c1.edit(model.__dict__, step["change"])
                result = {"edited": True}
            elif operation == "reopen":
                restored = json.loads(json.dumps(snapshot(model), allow_nan=False))
                model.__dict__.update(restored)
                ensure(snapshot(model) == before, "logical reopen changed state")
                result = {"reopened": True}
            else:
                raise AssertionError("unknown fixture operation: " + operation)
        except c1.Rejected as error:
            ensure(snapshot(model) == before, "rejection changed state")
            result = {"error": str(error)}
        if "expect" in step:
            got = projection(result, step["expect"])
            ensure(got == step["expect"],
                   f"{case['name']} step {index}: got {got!r}, want {step['expect']!r}")
        results.append(result)
    return results


def self_test(fixture):
    ensure(fixture["schema"] == "fixture.memory-x1.v2", "wrong fixture schema")
    cases, base = fixture["cases"], fixture["base_request"]
    names = {case["name"]: case for case in cases}
    ensure(len(names) == len(cases), "duplicate case name")
    for case in cases:
        run_case(case, base)
    kills = []
    for mutant in fixture["mutations"]:
        try:
            run_case(names[mutant["case"]], base, mutant["name"])
        except AssertionError as error:
            # A named assertion mismatch/atomicity failure is the only kill;
            # no syntax/import/process errors qualify.
            kills.append((mutant["name"], mutant["case"], str(error)[:160]))
        else:
            raise AssertionError("surviving mutation: " + mutant["name"])
        run_case(names[mutant["case"]], base)
    for count in (1, 64, 256):
        items = [{"id": f"m{index:03}", "expected_memory_revision": index + 1,
                  "disposition": "logical_delete" if index % 2 else "retain"}
                 for index in range(count)]
        encoded = manifest(items)
        ensure(len(encoded) == 8 + 82 * count, "manifest byte bound")
        ensure(len(decode_manifest(encoded, count)) == count, "manifest roundtrip")
        properties = manifest_properties(items)
        ensure(stored_manifest(properties) == encoded, "manifest property chunks")
        ensure(len(properties) <= 7 and all(len(value) <= 4096 for key, value
                                           in properties.items() if key != "x1_source_chunks"),
               "manifest string/field ceilings")
        for bad in (encoded + "00", encoded[:-2], encoded.upper(), "zz" + encoded[2:]):
            try:
                decode_manifest(bad, count)
            except c1.Rejected:
                pass
            else:
                raise AssertionError("accepted corrupt manifest")
    for op, size in (("DeleteNode", 16), ("DeleteEdge", 16), ("Detach", 20),
                     ("VectorDelete", 28), ("Attach", 28), ("Reclaim", 16)):
        ensure(entry_len({"op": op}) == size, "codec arithmetic: " + op)
    request = normalize(base)
    vectors = fixture["byte_vectors"]
    actual_vectors = {
        "source_hex": request["source_bytes"].hex(),
        "source_digest": request["source_digest"],
        "request_hex": request["request_bytes"].hex(),
        "request_digest": request["request_digest"],
        "request_length": len(request["request_bytes"]),
        "manifest": manifest(request["inputs"]),
        "receipt_key": receipt_key(request),
    }
    initial = Model({}, base["scope"]).write(base)
    actual_vectors.update(outcome_hex=outcome_bytes(initial["outcome"]).hex(),
                          outcome_length=len(outcome_bytes(initial["outcome"])))
    ensure(actual_vectors == vectors, "independently authored binary byte vectors differ")
    ensure([entry_len(e) for e in initial["entries"]] == [912, 16, 618, 527],
           "independently counted smallest-delete entry lengths")
    changed = c1.merge(base, {"inputs": [{"id": "m", "expected_memory_revision": 1,
                                        "disposition": "retain"}]})
    ensure(request["request_digest"] != normalize(changed)["request_digest"],
           "disposition omitted from digest")
    ensure(normalize(c1.merge(base, {"dry_run": True, "C": 11,
                                    "deadline": 100, "input_budget": 256}))["request_digest"]
           == request["request_digest"], "execution controls hashed")
    v1 = c1.normalize(c1.merge(changed, {}))
    ensure(v1["request_digest"] == fixture["c1_request_digest"], "C1 v1 bytes changed")
    codec_checks(request)
    print(f"X1 proposed contract: {len(cases)} cases, {len(kills)} killed, "
          "0 survived, 0 equivalents; manifest and fixed-entry boundary checks passed.")
    for name, case, evidence in kills:
        print(f"  KILL {name} via {case}: {evidence}")


def codec_checks(request):
    """Maximum shapes and malformed bytes, independently of transaction state."""
    maximum = {
        "id": "z" * 256, "revision": 1, "N": 256, "source_digest": "v2:" + "0" * 64,
        "deleted": ["d" + format(index, "03d") + "x" * 252 for index in range(256)],
        "retained_shared": ["e" + format(index, "03d") + "x" * 252 for index in range(256)],
    }
    node, chunks = receipt_nodes(request, "c", maximum)
    ensure(len(chunks) == 66, "maximum outcome must occupy 66 chunks")
    stored = {"node": node, "outcomes": chunks}
    ensure(decode_receipt(stored, request, "c")[1] == maximum, "maximum outcome roundtrip")
    corruptions = [
        {"path": ["node", "properties", "outcome_chunks"], "set": 65},
        {"path": ["node", "properties", "outcome_chunks"], "set": True},
        {"path": ["node", "properties", "digest"], "set": "v1:" + "0" * 64},
        {"path": ["node", "properties", "x1_schema"], "set": "consolidate:v1"},
        {"path": ["node", "properties", "expires_at_ms"], "set": "18446744073709551616"},
        {"path": ["outcomes", 0, "properties", "outcome"], "set": "00"},
        {"path": ["outcomes", 0, "properties", "index"], "set": True},
        {"path": ["outcomes", 0, "properties", "outcome"], "set": "AB" * 2048},
        {"path": ["outcomes", 0, "properties", "unexpected"], "set": "x"},
        {"path": ["outcomes", 1, "key"], "set": chunks[0]["key"]},
    ]
    for edit in corruptions:
        broken = copy.deepcopy(stored)
        c1.edit(broken, edit)
        try:
            decode_receipt(broken, request, "c")
        except c1.Rejected:
            pass
        else:
            raise AssertionError("accepted corrupt receipt: " + repr(edit["path"]))
    encoded = outcome_bytes(maximum)
    invalid_outcomes = [
        encoded + b"\0", encoded[:-1], b"bad", encoded.replace(OUTCOME_DOMAIN,
                                                             b"consolidate:v1\0", 1),
        outcome_bytes(dict(maximum, deleted=["b", "a"])),
        outcome_bytes(dict(maximum, retained_shared=["e", "e"])),
        outcome_bytes(dict(maximum, revision=2)),
    ]
    for broken in invalid_outcomes:
        try:
            decode_outcome(broken)
        except c1.Rejected:
            pass
        else:
            raise AssertionError("accepted corrupt outcome")
    print("Codec checks: 66-chunk maximum, 10 malformed receipts, 7 malformed outcomes.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    fixture = json.loads(FIXTURE.read_text(encoding="utf-8"))
    if args.self_test:
        self_test(fixture)
    else:
        case = json.load(sys.stdin)
        print(json.dumps(run_case(case, fixture["base_request"]), ensure_ascii=False,
                         allow_nan=False, sort_keys=True))


if __name__ == "__main__":
    main()
