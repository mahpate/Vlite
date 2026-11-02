"""Independent PRIVATE C1 reference, derived only from frozen contract 305c46a1.

Source: docs/projects/roadmap/gap1-c1-consolidation-contract.md sections 1-8,
with coordinator erratum 24a24d46: full content node is 4,337, not 4,336.
No C1 Rust was consulted. No API, wire, durability or implementation parity claim.
Run python tests\\c1_consolidation_oracle.py --self-test (also under -O).
Without --self-test, stdin is one fixture case; stdout is its step results.

Fixture assumptions (not additional contract):
* JSON request spelling mirrors the contract. Unknown fields/type-only failures
  have local InvalidBundle errors where the contract gives no category.
* Setup, generated inputs/text, raw node edits, forget and JSON roundtrip reopen
  are test harness operations, not proposed memory APIs. Records carry explicit
  scope/collection/owners. Missing record or binding is fixture: InvalidEvidence;
  owners, even expired owners, must be detached before a record is attachable.
* Setup sources have a minimal retained diagnostic shape, not a reconstruction of
  every A1 stored property. malformed=True injects an existing malformed source.
* Logical replacement budgets charge one guard for receipt and one for outcome;
  physical budgets never do. Synthetic graph receipt revisions/sequence are
  monotonic fixture counters, returned from stored receipt state, not outcome.
* commit_clock=max(C, wall, high_water) is explicit, never a wall-clock race.
  fixture:EndpointRefused names an injected preappend endpoint refusal, NOT a
  promise about the Rust/P3b error spelling. Only preappend atomicity is modeled.
  Optional opening reclamation has explicit count/bytes. No I/O rollback claim.
* Snapshot reopen tests logical retention only, NOT four real engine backends.
  REFERENCES-only projection/forget interactions below are bounded diagnostics,
  not a replacement for independently authorized implementation tests.
* Golden expected objects are projections, but each projected object/list/value
  compares exactly. Full node property sets are separately checked against static
  golden manifests. Large text descriptors expand inputs only, never expected
  hashes/lengths. Every rejected golden step has exactly one one-leaf repair.
  Precedence probes with multiple defects are separate, explicitly ordered tests.
"""

import argparse
import copy
import hashlib
import json
import math
from pathlib import Path
import re
import struct
import sys


FIXTURE = Path(__file__).parent / "fixtures" / "agent_memory" / "c1_consolidation_golden.json"
LIMIT = 16_777_216
U64_MAX, I64_MAX, I64_MIN = 2**64 - 1, 2**63 - 1, -(2**63)
SCHEMA = "consolidate:v1"
DIGEST = re.compile(r"v1:[0-9a-f]{64}\Z")
MUTATION = None


class Rejected(ValueError):
    pass


def require(condition, error):
    if not condition:
        raise Rejected(error)


def ensure(condition, message):
    if not condition:
        raise AssertionError(message)


def integer(value, low=0, high=U64_MAX):
    return type(value) is int and low <= value <= high


def utf8(value):
    return value.encode("utf-8")


def text(value, low=1, high=256):
    if not isinstance(value, str):
        return False
    try:
        return low <= len(utf8(value)) <= high
    except UnicodeEncodeError:
        return False


def sha(value):
    return hashlib.sha256(value).hexdigest()


def h(value):
    return "v1:" + sha(value)


def u32(value):
    return struct.pack(">I", value)


def u64(value):
    return struct.pack(">Q", value)


def s(value):
    value = utf8(value)
    return u32(len(value)) + value


def framed(value):
    return u32(len(value)) + value


def optional(value):
    return b"\0" if value is None else b"\1" + s(value)


def ou(value):
    return b"\0" if value is None else b"\1" + u64(value)


def scope_bytes(scope):
    require(isinstance(scope, dict) and {"tenant", "agent"} <= scope.keys()
            and scope.keys() <= {"tenant", "agent", "user", "session"}, "InvalidScope")
    values = [scope.get(key) for key in ("tenant", "agent", "user", "session")]
    require(all(text(value) for value in values[:2])
            and all(value is None or text(value) for value in values[2:])
            and (values[3] is None or values[2] is not None), "InvalidScope")
    return b"scope:v1" + b"".join(optional(value) for value in values)


def scope_key(scope):
    return "scope:v1:" + sha(scope_bytes(scope))


def memory_key(scope, identity):
    return "mem:v1:" + scope_key(scope) + ":" + sha(utf8(identity))


def provenance_text(entries):
    return "".join("-;" if entry[field] is None else
                   str(len(utf8(entry[field]))) + ":" + entry[field] + ";"
                   for entry in entries
                   for field in ("source", "kind", "uri", "transformation"))


def expand(value):
    """Only explicit fixture descriptors expand; no expected values are derived."""
    if isinstance(value, list):
        return [expand(item) for item in value]
    if not isinstance(value, dict):
        return value
    if set(value) == {"$text", "bytes"}:
        pattern, length = utf8(value["$text"]), value["bytes"]
        ensure(pattern and integer(length, 0, LIMIT + 1024), "bad text descriptor")
        raw = (pattern * ((length + len(pattern) - 1) // len(pattern)))[:length]
        return raw.decode("utf-8")
    if "$repeat" in value:
        ensure(value.keys() <= {"$repeat", "count", "enumerate"}
               and integer(value["count"], 0, 5000), "bad repeat descriptor")
        result = []
        for index in range(value["count"]):
            item = expand(copy.deepcopy(value["$repeat"]))
            for key in value.get("enumerate", []):
                item[key] += str(index)
            result.append(item)
        return result
    if set(value) == {"$number"}:
        return {"nan": float("nan"), "inf": float("inf"), "zero": 0.0}[value["$number"]]
    if set(value) == {"$ordered", "reverse"}:
        result = expand(value["$ordered"])
        return list(reversed(result)) if value["reverse"] else result
    return {key: expand(item) for key, item in value.items()}


def merge(base, update):
    result = copy.deepcopy(base)
    for key, value in update.items():
        if isinstance(value, dict) and isinstance(result.get(key), dict) and not any(
                name.startswith("$") for name in value):
            result[key] = merge(result[key], value)
        else:
            result[key] = copy.deepcopy(value)
    return result


def normalize(raw):
    require(isinstance(raw, dict), "InvalidBundle")
    scope = raw.get("scope")
    sb = scope_bytes(scope)
    require(text(raw.get("idempotency_key")), "InvalidKey")
    c = raw.get("C")
    require(integer(c, 0, I64_MAX), "InvalidClock")
    window = raw.get("receipt_window_ms")
    require(integer(window, 1) and c + window <= U64_MAX, "InvalidReceiptWindow")
    budget = raw.get("input_budget", 64)
    inputs = raw.get("inputs")
    require(integer(budget, 1) and isinstance(inputs, list) and inputs, "InvalidBundle")
    require(budget <= 256 and len(inputs) <= budget, "OperationLimit")
    ids, normalized = set(), []
    for item in inputs:
        require(isinstance(item, dict) and text(item.get("id"))
                and integer(item.get("expected_memory_revision"), 1), "InvalidTarget")
    for item in inputs:
        require(item["id"] not in ids, "DuplicateTarget")
        ids.add(item["id"])
    for item in inputs:
        require(item.get("disposition") == "retain", "InvalidBundle")
        require(item.keys() == {"id", "expected_memory_revision", "disposition"},
                "InvalidBundle")
        normalized.append(copy.deepcopy(item))
    summary = raw.get("summary", {})
    require(isinstance(summary, dict) and text(summary.get("id"))
            and text(summary.get("source")), "InvalidIdentity")
    content = summary.get("content")
    require(text(content, 1, U64_MAX), "InvalidContent")
    require(len(utf8(content)) <= LIMIT, "BodyLimit")
    importance = summary.get("importance", 0.5)
    require(type(importance) in (int, float) and math.isfinite(importance)
            and 0 <= importance <= 1, "InvalidImportance")
    importance = 0.0 if importance == 0 else float(importance)
    observed = summary.get("observed_at_ms")
    require(integer(observed, I64_MIN, I64_MAX), "InvalidObservation")
    expiry = summary.get("expiry_ms")
    require(expiry is None or integer(expiry), "InvalidExpiry")
    prov = summary.get("provenance", [])
    require(isinstance(prov, list), "InvalidProvenance")
    require(len(prov) <= 64, "ProvenanceLimit")
    if not prov:
        prov = [{"source": summary["source"]}]
    normalized_prov = []
    size = 0
    for entry in prov:
        require(isinstance(entry, dict), "InvalidProvenance")
        current = {key: entry.get(key) for key in
                   ("source", "kind", "uri", "transformation")}
        # Count encodable fields before field validity (contract section 6).
        for value in current.values():
            if value is None:
                size += 2
            elif isinstance(value, str):
                try:
                    length = len(utf8(value))
                except UnicodeEncodeError:
                    raise Rejected("InvalidProvenance") from None
                size += len(str(length)) + 1 + length + 1
        normalized_prov.append(current)
    require(size <= 4096, "ProvenanceLimit")
    for entry, current in zip(prov, normalized_prov):
        require(entry.keys() <= current.keys() and text(current["source"])
                and all(value is None or text(value, 0) for key, value in current.items()
                        if key != "source"), "InvalidProvenance")
    require(text(raw.get("transformation")), "InvalidProvenance")
    evidence = raw.get("evidence")
    if evidence is not None:
        require(isinstance(evidence, dict) and text(evidence.get("id")), "InvalidEvidence")
        eb = scope_bytes(evidence.get("scope"))
        require(eb == sb, "CrossScope")
        require(evidence.keys() == {"id", "scope"}, "InvalidEvidence")
    allow = raw.get("allow_future_observations", False)
    require(type(allow) is bool, "InvalidBundle")
    deadline = raw.get("deadline")
    require(deadline is None or integer(deadline), "InvalidBundle")
    require(raw.keys() <= {"scope", "idempotency_key", "C", "receipt_window_ms",
                           "input_budget", "inputs", "summary", "transformation",
                           "evidence", "allow_future_observations", "deadline"}
            and summary.keys() <= {"id", "source", "content", "importance",
                                   "observed_at_ms", "expiry_ms", "provenance"},
            "InvalidBundle")
    normalized.sort(key=lambda item: utf8(item["id"]))
    result = dict(raw, inputs=normalized, input_budget=budget,
                  allow_future_observations=allow, evidence=evidence,
                  summary=dict(summary, importance=importance, expiry_ms=expiry,
                               provenance=normalized_prov))
    source, request = canonical_bytes(result)
    require(len(request) <= LIMIT, "BodyLimit")
    result.update(source_bytes=source, request_bytes=request,
                  source_digest=h(source), request_digest=h(request))
    return result


def canonical_bytes(request):
    sb = framed(scope_bytes(request["scope"]))
    inputs = u32(len(request["inputs"])) + b"".join(
        s(item["id"]) + u64(item["expected_memory_revision"]) + b"\0"
        for item in request["inputs"])
    source = b"vlite.memory.consolidate.sources.v1\0" + sb + inputs
    summary = request["summary"]
    prov = summary["provenance"]
    evidence = request["evidence"]
    eb = (b"\0" if evidence is None else
          b"\1" + s(evidence["id"]) + framed(scope_bytes(evidence["scope"])))
    body = (b"vlite.memory.consolidate.request.v1\0" + sb + inputs
            + s(summary["id"]) + s(summary["content"]) + s(summary["source"])
            + struct.pack(">q", summary["observed_at_ms"])
            + struct.pack(">d", summary["importance"]) + ou(summary["expiry_ms"])
            + u32(len(prov)) + b"".join(
                s(entry["source"]) + optional(entry["kind"]) + optional(entry["uri"])
                + optional(entry["transformation"]) for entry in prov)
            + s(request["transformation"]) + eb
            + bytes([request["allow_future_observations"]]))
    body += u64(0 if MUTATION == "window_unhashed" else request["receipt_window_ms"])
    return source, body


def content_chunks(content):
    raw = utf8(content)
    if len(raw) <= 4096:
        return []
    chunks, start = [], 0
    while start < len(raw):
        end = min(start + 4096, len(raw))
        while end < len(raw) and raw[end] & 0xC0 == 0x80:
            end -= 1
        chunks.append(raw[start:end].decode("utf-8"))
        start = end
    return chunks


def node(key, label, properties):
    return {"key": key, "labels": [label], "properties": properties}


def metadata_len(properties):
    total = 4
    for key, value in properties.items():
        require(type(value) in (str, int, float), "InvalidBundle")
        payload = 4 + len(utf8(value)) if type(value) is str else 8
        total += 2 + len(utf8(key)) + 1 + payload
    return total


def entry_len(entry):
    if entry["op"] == "Attach":
        return 28
    if entry["op"] == "SetNodeExpiry":
        return 24
    if entry["op"] == "Reclaim":
        return 0 if MUTATION == "reclaim_free" else 16
    m = metadata_len(entry["properties"])
    require(m <= 65_536, "BodyLimit")
    if entry["op"] == "PutNode":
        return (8 + 8 + 2 + len(utf8(entry["key"])) + 1
                + sum(1 + len(utf8(label)) for label in entry["labels"]) + 4 + m)
    return 8 + 24 + 1 + len(utf8(entry["relationship"])) + 2 + len(
        utf8(entry["key"])) + 4 + m


def graph_plan(request, collection):
    summary, scope = request["summary"], request["scope"]
    sk = scope_key(scope)
    mk = memory_key(scope, summary["id"])
    rk = ("receipt:v1:" + sk + ":"
          + sha(b"consolidate:v1\0" + utf8(request["idempotency_key"])))
    ok = "receipt-outcome:v1:" + sha(utf8(rk)) + ":0000"
    content_digest = h(utf8(json.dumps(summary["content"], ensure_ascii=False,
                                     separators=(",", ":"))))
    props = {
        "scope": sk, "id": summary["id"], "kind": "episodic",
        "content_digest": content_digest, "sources": utf8(summary["source"]).hex(),
        "provenance": provenance_text(summary["provenance"]),
        "memory_revision": "1", "reference_count": "0",
        "evidence": "" if request["evidence"] is None else utf8(request["evidence"]["id"]).hex(),
        "importance": summary["importance"], "observed_at_ms": summary["observed_at_ms"],
        "created_at_ms": request["C"], "updated_at_ms": request["C"],
        "c1_schema": SCHEMA, "c1_transformation": request["transformation"],
        "c1_source_digest": request["source_digest"], "c1_source_count": len(request["inputs"]),
    }
    chunks = content_chunks(summary["content"])
    if not chunks:
        props["content"] = summary["content"]
    entries = [dict(node(mk, "VliteMemory", props), op="PutNode", role="summary")]
    if summary["expiry_ms"] is not None:
        entries.append({"op": "SetNodeExpiry", "expiry": summary["expiry_ms"], "role": "expiry"})
    for index, content in enumerate(chunks):
        suffix = sha(utf8(mk)) + ":" + format(index, "04x")
        ck = "content:v1:" + suffix
        entries.append(dict(node(ck, "VliteMemoryContent",
                                 {"scope": sk, "index": index, "content": content}),
                            op="PutNode", role="content"))
        entries.append({"op": "PutEdge", "role": "content_edge", "from": mk, "to": ck,
                        "key": "content-edge:v1:" + suffix, "relationship": "HAS_CONTENT",
                        "properties": {}})
    for item in request["inputs"]:
        target = memory_key(scope, item["id"])
        entries.append({
            "op": "PutEdge", "role": "derived", "from": mk, "to": target,
            "key": "derived:v1:" + sha(b"vlite.memory.consolidate.edge.v1\0" + s(mk) + s(target)),
            "relationship": "DERIVED_FROM",
            "properties": {"c1_schema": SCHEMA,
                           "c1_source_memory_revision": str(item["expected_memory_revision"])}})
    if request["evidence"] is not None:
        entries.append({"op": "Attach", "role": "attach", "id": request["evidence"]["id"]})
    outcome = (SCHEMA + "\n" + utf8(summary["id"]).hex() + "\n1\n"
               + str(len(request["inputs"])) + "\n" + request["source_digest"] + "\n")
    entries.append(dict(node(ok, "VliteMemoryReceiptOutcome",
                             {"receipt_key": rk, "index": 0, "outcome": outcome}),
                        op="PutNode", role="outcome"))
    entries.append(dict(node(rk, "VliteMemoryReceipt", {
        "scope": sk, "c1_scope_tuple": scope_bytes(scope).hex(), "c1_schema": SCHEMA,
        "c1_collection": collection or "", "digest": request["request_digest"],
        "expires_at_ms": str(request["C"] + request["receipt_window_ms"]), "outcome_chunks": 1}),
                        op="PutNode", role="receipt"))
    return entries


RECEIPT_FIELDS = {"scope": str, "c1_scope_tuple": str, "c1_schema": str,
                  "c1_collection": str, "digest": str, "expires_at_ms": str,
                  "outcome_chunks": int}
OUTCOME_FIELDS = {"receipt_key": str, "index": int, "outcome": str}


def exact_node(value, label, shape):
    require(isinstance(value, dict) and value.keys() == {"key", "labels", "properties"}
            and value["labels"] == [label] and isinstance(value["key"], str)
            and isinstance(value["properties"], dict)
            and value["properties"].keys() == shape.keys()
            and all(type(value["properties"][key]) is typ for key, typ in shape.items()),
            "InvalidBundle")
    return value["properties"]


def decode_outcome(value):
    require(text(value, 1, 4096) and value.isascii(), "InvalidBundle")
    lines = value.split("\n")
    require(len(lines) == 6 and lines[-1] == "" and lines[0] == SCHEMA
            and re.fullmatch(r"(?:[0-9a-f]{2}){1,256}", lines[1]) is not None
            and lines[2] == "1"
            and re.fullmatch(r"[1-9][0-9]*", lines[3]) is not None
            and len(lines[3]) <= 3 and 1 <= int(lines[3]) <= 256
            and DIGEST.fullmatch(lines[4]) is not None, "InvalidBundle")
    try:
        identity = bytes.fromhex(lines[1]).decode("utf-8")
    except UnicodeDecodeError:
        raise Rejected("InvalidBundle") from None
    require(text(identity), "InvalidBundle")
    return {"id": identity, "revision": 1, "N": int(lines[3]), "source_digest": lines[4]}


def decode_receipt(receipt, outcomes, request, collection, rk):
    props = exact_node(receipt["node"], "VliteMemoryReceipt", RECEIPT_FIELDS)
    require(receipt["node"]["key"] == rk and props["scope"] == scope_key(request["scope"])
            and props["c1_schema"] == SCHEMA, "InvalidBundle")
    require(props["c1_scope_tuple"] == scope_bytes(request["scope"]).hex(), "DigestCollision")
    require(props["c1_collection"] == (collection or ""), "InvalidBundle")
    require(DIGEST.fullmatch(props["digest"]) is not None
            and re.fullmatch(r"[1-9][0-9]*", props["expires_at_ms"]) is not None
            and len(props["expires_at_ms"]) <= 20
            and int(props["expires_at_ms"]) <= U64_MAX
            and props["outcome_chunks"] == 1, "InvalidBundle")
    require(isinstance(outcomes, list) and len(outcomes) == 1, "InvalidBundle")
    op = exact_node(outcomes[0], "VliteMemoryReceiptOutcome", OUTCOME_FIELDS)
    require(outcomes[0]["key"] == "receipt-outcome:v1:" + sha(utf8(rk)) + ":0000"
            and op["receipt_key"] == rk and op["index"] == 0, "InvalidBundle")
    return props, decode_outcome(op["outcome"])


class Model:
    def __init__(self, setup, base_scope):
        self.scopes = {}
        self.sources = []
        self.records = []
        self.summaries = []
        self.receipts = {}
        self.sequence = 0
        self.high_water = setup.get("high_water", 0)
        self.wall = setup.get("wall", 0)
        self.opening_reclaims = setup.get("opening_reclaims", 0)
        ensure(integer(self.opening_reclaims, 0, 1), "at most one opening reclaim")
        for binding in setup.get("bindings", [{"scope": base_scope, "collection": "c"}]):
            self.scopes[scope_key(binding["scope"])] = {
                "tuple": scope_bytes(binding["scope"]).hex(),
                "collection": binding.get("collection")}
        for source in setup.get("sources", [{"id": "m"}]):
            self.sources.append(merge(
                {"scope": base_scope, "revision": 1, "kind": "entity", "expiry_ms": None,
                 "content": "source", "source": "origin", "attachments": [],
                 "properties": {"retained": "unchanged"}, "malformed": False}, source))
        for record in setup.get("records", []):
            self.records.append(merge(
                {"scope": base_scope, "collection": "c", "owners": []}, record))
        for change in setup.get("edits", []):
            edit(self.__dict__, change)

    def state(self):
        return copy.deepcopy(self.__dict__)

    def write(self, raw):
        request = normalize(raw)
        sk = scope_key(request["scope"])
        require(sk in self.scopes, "InvalidScope")
        binding = self.scopes[sk]
        require(binding["tuple"] == scope_bytes(request["scope"]).hex(), "DigestCollision")
        entries = graph_plan(request, binding["collection"])
        rk = entries[-1]["key"]
        old = self.receipts.get(rk)
        if old is not None:
            props, outcome = decode_receipt(old, old["outcomes"], request,
                                            binding["collection"], rk)
            if request["C"] < int(props["expires_at_ms"]):
                require(props["digest"] == request["request_digest"], "IdempotencyConflict")
                require(outcome == {"id": request["summary"]["id"], "revision": 1,
                                    "N": len(request["inputs"]),
                                    "source_digest": request["source_digest"]}, "InvalidBundle")
                return {"status": "replayed", "outcome": outcome,
                        "outcome_ascii": old["outcomes"][0]["properties"]["outcome"],
                        "receipt_revision": old["revision"], "sequence": old["sequence"],
                        "request_digest": request["request_digest"]}
        c, summary = request["C"], request["summary"]
        require(request.get("deadline") is None or c < request["deadline"], "DeadlineExceeded")
        require(request["allow_future_observations"] or summary["observed_at_ms"] <= c,
                "FutureObservation")
        if MUTATION != "born_expired_late":
            require(summary["expiry_ms"] is None or summary["expiry_ms"] > c, "MissingMemory")
        for item in request["inputs"]:
            candidates = [source for source in self.sources + self.summaries
                          if scope_bytes(source["scope"]) == scope_bytes(request["scope"])
                          and source["id"] == item["id"]]
            require(candidates and (candidates[0]["expiry_ms"] is None
                                    or c < candidates[0]["expiry_ms"]), "MissingMemory")
            source = candidates[0]
            require(not source["malformed"], "InvalidBundle")
            require(source["kind"] in ("entity", "episodic"), "UnsupportedKind")
            require(source["revision"] == item["expected_memory_revision"], "RevisionConflict")
        if MUTATION == "born_expired_late":
            require(summary["expiry_ms"] is None or summary["expiry_ms"] > c, "InvalidBundle")
        for source in self.sources + self.summaries:
            if scope_bytes(source["scope"]) == scope_bytes(request["scope"]) and (
                    source["expiry_ms"] is None or c < source["expiry_ms"]):
                require(source["id"] != summary["id"] and not (
                    source["kind"] == "episodic" and source["content"] == summary["content"]
                    and source["source"] == summary["source"]), "IdentityConflict")
        evidence = request["evidence"]
        record = None
        if evidence is not None:
            require(binding["collection"] is not None, "InvalidEvidence")
            candidates = [record for record in self.records if record["id"] == evidence["id"]
                          and record["collection"] == binding["collection"]]
            require(len(candidates) == 1, "InvalidEvidence")
            record = candidates[0]
            require(scope_bytes(record["scope"]) == scope_bytes(request["scope"]), "CrossScope")
            require(not record["owners"], "RecordAlreadyAttached")
        physical = len(entries)
        require(physical <= 4096, "OperationLimit")
        logical = physical + (2 if old else 0)
        require(logical <= 4096, "OperationLimit")
        commit_clock = max(c, self.wall, self.high_water)
        opening = [{"op": "Reclaim", "role": "opening_reclaim"}] * self.opening_reclaims
        if summary["expiry_ms"] is not None and summary["expiry_ms"] <= commit_clock:
            # Reclaim is byte-charged but the final endpoint validation refuses.
            failed = opening + entries[:2] + [{"op": "Reclaim", "role": "due_reclaim"}] + entries[2:]
            require(sum(entry_len(entry) for entry in failed) + 4 <= LIMIT, "BodyLimit")
            raise Rejected("fixture:EndpointRefused")
        # A selected source becoming due at the one captured commit clock also
        # loses its endpoint before derived edges can be validated.
        selected_ids = {item["id"] for item in request["inputs"]}
        require(not any(scope_bytes(source["scope"]) == scope_bytes(request["scope"])
                        and source["id"] in selected_ids
                        and source["expiry_ms"] is not None
                        and source["expiry_ms"] <= commit_clock
                        for source in self.sources + self.summaries),
                "fixture:EndpointRefused")
        entries = opening + entries
        sizes = [entry_len(entry) for entry in entries]
        frame = 4 + sum(sizes)
        require(frame <= LIMIT, "BodyLimit")
        outcome_node = {key: copy.deepcopy(entries[-2][key])
                        for key in ("key", "labels", "properties")}
        receipt_node = {key: copy.deepcopy(entries[-1][key])
                        for key in ("key", "labels", "properties")}
        outcome = decode_outcome(outcome_node["properties"]["outcome"])
        # No model state changes until all preappend validation has passed.
        expired_owners = {(scope_key(source["scope"]), source["id"])
                          for source in self.sources + self.summaries
                          if source["expiry_ms"] is not None
                          and source["expiry_ms"] <= commit_clock}
        self.sources = [source for source in self.sources
                        if source["expiry_ms"] is None or commit_clock < source["expiry_ms"]]
        self.summaries = [source for source in self.summaries
                          if source["expiry_ms"] is None or commit_clock < source["expiry_ms"]]
        for candidate in self.records:
            candidate["owners"] = [owner for owner in candidate["owners"]
                                   if (scope_key(candidate["scope"]), owner) not in expired_owners]
        for candidate in self.summaries:
            candidate["derived"] = [identity for identity in candidate["derived"]
                                    if (scope_key(candidate["scope"]), identity)
                                    not in expired_owners]
        self.sequence += 1
        self.high_water = commit_clock
        self.summaries.append(dict(copy.deepcopy(summary), scope=request["scope"],
                                   revision=1, kind="episodic", malformed=False,
                                   properties=copy.deepcopy(entries[len(opening)]["properties"]),
                                   attachments=[] if record is None else [record["id"]],
                                   derived=[item["id"] for item in request["inputs"]]))
        if record is not None:
            record["owners"] = [summary["id"]]
        revision = 1 if old is None else old["revision"] + 1
        self.receipts[rk] = {"node": receipt_node, "outcomes": [outcome_node],
                             "revision": revision, "sequence": self.sequence}
        groups = {}
        for entry, size in zip(entries, sizes):
            groups.setdefault(entry["role"], []).append(size)
        return {"status": "created", "outcome": outcome,
                "outcome_ascii": outcome_node["properties"]["outcome"],
                "outcome_bytes": len(utf8(outcome_node["properties"]["outcome"])),
                "receipt_revision": revision, "sequence": self.sequence,
                "request_digest": request["request_digest"],
                "source_digest": request["source_digest"],
                "request_bytes": len(request["request_bytes"]),
                "source_bytes_hex": request["source_bytes"].hex(),
                "physical_ops": physical, "logical_ops": logical, "frame_bytes": frame,
                "entry_bytes": groups, "chunk_bytes": [len(utf8(chunk)) for chunk in
                                                      content_chunks(summary["content"])],
                "entries": entries}


def at(value, path):
    for key in path:
        if key == "$only":
            ensure(len(value) == 1, "$only requires exactly one key")
            key = next(iter(value))
        value = value[key]
    return value


def edit(value, change):
    path = change["path"]
    ensure(isinstance(path, list) and path and all(
        isinstance(key, str) or integer(key) for key in path), "bad edit path")
    parent = at(value, path[:-1])
    key = path[-1]
    if change.get("delete") is True:
        del parent[key]
    else:
        parent[key] = copy.deepcopy(change["set"])


def project(actual, expected):
    if isinstance(expected, dict):
        ensure(isinstance(actual, dict), "projection requires object")
        return {key: project(actual[key], value) for key, value in expected.items()}
    return actual


def execute(model, step, base):
    for change in step.get("edits", []):
        edit(model.__dict__, expand(change))
    corruption = step.get("corruption")
    if corruption and corruption["enabled"]:
        ensure(len(model.receipts) == 1, "corruption requires one receipt")
        edit(next(iter(model.receipts.values())), expand(corruption["change"]))
    op = step.get("op", "write")
    if op == "reopen":
        model.__dict__ = json.loads(json.dumps(model.__dict__, ensure_ascii=False))
        return {"reopened": True}
    if op == "forget":
        identity = step["id"]
        model.sources = [source for source in model.sources if source["id"] != identity]
        model.summaries = [source for source in model.summaries if source["id"] != identity]
        for record in model.records:
            record["owners"] = [owner for owner in record["owners"] if owner != identity]
        for summary in model.summaries:
            summary["derived"] = [source for source in summary["derived"] if source != identity]
        return {"forgotten": identity}
    if op == "inspect":
        return model.state()
    if op == "visible":
        scope, clock = step.get("scope", base["scope"]), step["clock"]
        live = [source for source in model.sources + model.summaries
                if scope_bytes(source["scope"]) == scope_bytes(scope)
                and (source["expiry_ms"] is None or clock < source["expiry_ms"])]
        owners = {source["id"] for source in live}
        collection = model.scopes.get(scope_key(scope), {}).get("collection")
        return {"memories": sorted(source["id"] for source in live),
                "evidence": sorted(record["id"] for record in model.records
                                   if scope_bytes(record["scope"]) == scope_bytes(scope)
                                   and record["collection"] == collection
                                   and len(record["owners"]) == 1
                                   and record["owners"][0] in owners)}
    if op == "references":
        # This is intentionally only a relationship filter, not B traversal.
        return {"references": [edge for edge in step["edges"]
                               if edge["relationship"] == "REFERENCES"]}
    request = expand(merge(base, step.get("request", {})))
    for change in step.get("request_edits", []):
        edit(request, expand(change))
    before = model.state()
    try:
        if op == "digest":
            normalized = normalize(request)
            return {"request_digest": normalized["request_digest"],
                    "source_digest": normalized["source_digest"],
                    "request_bytes": len(normalized["request_bytes"]),
                    "request_hex": normalized["request_bytes"].hex(),
                    "source_bytes_hex": normalized["source_bytes"].hex()}
        require(op == "write", "InvalidBundle")
        return model.write(request)
    except Rejected as error:
        ensure(model.state() == before, "rejection changed state")
        return {"error": str(error)}


def run_case(case, base, repair=False):
    model = Model(expand(case.get("setup", {})), base["scope"])
    results = []
    for index, step in enumerate(case["steps"]):
        actual = execute(model, step, base)
        expected = step["expect"]
        if repair:
            ensure("error" not in actual, f"{case['name']} repair step {index}: {actual}")
        else:
            try:
                selected = project(actual, expected)
            except (KeyError, AssertionError) as error:
                raise AssertionError(f"{case['name']} step {index}: "
                                     f"expected {expected!r}; projection {error}") from None
            ensure(selected == expected,
                   f"{case['name']} step {index}: expected {expected!r}, got {selected!r}")
        results.append(actual)
    return results, model


def repair_checks(case, base):
    count = 0
    for index, step in enumerate(case["steps"]):
        rejected = "error" in step["expect"]
        ensure(rejected == ("defect" in step), f"{case['name']} missing/extra defect")
        if not rejected:
            continue
        defect = step["defect"]
        ensure(set(defect) in ({"tag", "path", "set"}, {"tag", "path", "delete"})
               and isinstance(defect["tag"], str), "bad defect descriptor")
        repaired = copy.deepcopy(case)
        repaired["steps"] = repaired["steps"][:index + 1]
        path = defect["path"]
        parent = at(repaired, path[:-1])
        old = parent.get(path[-1]) if isinstance(parent, dict) else parent[path[-1]]
        # One scalar/empty-container/descriptor leaf cannot conceal two defects.
        ensure(not isinstance(old, (dict, list)) or not old, "repair replaces non-leaf")
        edit(repaired, defect)
        model = Model(expand(repaired.get("setup", {})), base["scope"])
        for position, candidate in enumerate(repaired["steps"]):
            actual = execute(model, candidate, base)
            if position == index or (path[0] == "steps" and path[1] == position):
                ensure("error" not in actual,
                       f"{case['name']} repair {defect['tag']} left {actual!r}")
            else:
                ensure(("error" in actual) == ("error" in candidate["expect"]),
                       f"{case['name']} repair changed preceding acceptance")
        count += 1
    return count


def self_test(fixture):
    ensure(fixture["schema"] == "fixture.c1-consolidation.v1", "bad fixture schema")
    base = fixture["base_request"]
    names = set()
    repairs = 0
    all_results = {}
    cases = fixture_cases(fixture)
    for case in cases:
        ensure(case["name"] not in names and case.get("assumptions")
               and all(note.startswith(("spec: ", "fixture: ")) for note in case["assumptions"]),
               "invalid case identity/assumptions")
        names.add(case["name"])
        results, _ = run_case(case, base)
        all_results[case["name"]] = results
        repairs += repair_checks(case, base)
    baseline = all_results["exact-inline-schema"][0]
    for entry in baseline["entries"]:
        role = entry["role"]
        shape = fixture["property_sets"][role]
        actual = {key: ("String" if type(value) is str else
                        "Integer" if type(value) is int else "Float")
                  for key, value in entry["properties"].items()}
        ensure(actual == shape, f"{role} property schema differs")
    for entry, expected in zip(baseline["entries"], fixture["exact_nodes"]):
        ensure(project(entry, expected) == expected, "static node/key/label mismatch")
    ensure(len(baseline["entries"]) == len(fixture["exact_nodes"]), "entry count")
    chunked = all_results["utf8-packing"][0]
    for entry in chunked["entries"]:
        if entry["role"] in ("content", "content_edge"):
            shape = {key: ("String" if type(value) is str else "Integer")
                     for key, value in entry["properties"].items()}
            ensure(shape == fixture["property_sets"][entry["role"]], "chunk property set")
    for role, expected in fixture["chunk_keys"].items():
        entry = next(entry for entry in chunked["entries"] if entry["role"] == role)
        ensure(project(entry, expected) == expected, "chunk key/label/relationship mismatch")
    hash_count = hash_checks(fixture)
    precedence_count = precedence_checks(base)
    second = second_defect_checks(cases, base)
    boundaries = boundary_checks()
    global MUTATION
    killed = 0
    for mutation in fixture["mutations"]:
        MUTATION = mutation["name"]
        case = next(case for case in cases if case["name"] == mutation["case"])
        try:
            run_case(case, base)
        except AssertionError as error:
            ensure(str(error).startswith(f"{case['name']} step {mutation['step']}:")
                   and mutation["reason"] in str(error), f"unintended mutation kill: {error}")
            killed += 1
        else:
            raise AssertionError("surviving mutation: " + MUTATION)
        finally:
            MUTATION = None
    print(f"PASS: {len(names)} golden cases; "
          f"{sum(len(case['steps']) for case in cases)} golden steps; "
          f"{repairs} single-defect rejections repaired; {hash_count} hash probes; "
          f"{precedence_count} ordered precedence probes; "
          f"{second} second-defect injections; {boundaries} strict boundary checks; "
          f"{killed}/{len(fixture['mutations'])} intended-reason mutations")


def fixture_cases(fixture):
    """Corruption catalog expands literal inputs/errors, never expectations."""
    cases = copy.deepcopy(fixture["cases"])
    for rejection in fixture.get("rejections", []):
        step = {"request": rejection["request"], "expect": {"error": rejection["error"]},
                "op": rejection.get("op", "write"),
                "defect": dict(rejection["repair"], tag=rejection["name"])}
        cases.append({"name": rejection["name"], "setup": rejection.get("setup", {}),
                      "assumptions": rejection.get("assumptions", [
                          "spec: sections 2 and 6 structural/stateful rejection.",
                          "fixture: JSON spelling/type errors are fixture inputs."]),
                      "steps": [step]})
    for corruption in fixture.get("corruptions", []):
        for expired in ((False, True) if corruption.get("expired", True) else (False,)):
            cases.append({
                "name": corruption["name"] + ("-expired" if expired else "-live"),
                "assumptions": ["spec: section 5 intrinsic receipt decoding precedes expiry.",
                                "fixture: exactly one raw-storage corruption is toggled."],
                "steps": [
                    {"expect": {"status": "created"}},
                    {"request": ({"C": 110, "summary": {"id": "y", "content": "new"}}
                                 if expired else {"C": 11}),
                     "corruption": {"enabled": True, "change": corruption["change"]},
                     "expect": {"error": corruption.get("error", "InvalidBundle")},
                     "defect": {"tag": corruption["name"],
                                "path": ["steps", 1, "corruption", "enabled"], "set": False}}
                ]})
    return cases


def hash_checks(fixture):
    base = expand(fixture["base_request"])
    original = normalize(base)
    count = 0
    for probe in fixture["hash_probes"]:
        reference = copy.deepcopy(base)
        for change in probe.get("base_edits", []):
            edit(reference, expand(change))
        original = normalize(reference)
        changed = copy.deepcopy(reference)
        for change in probe["edits"]:
            edit(changed, expand(change))
        result = normalize(changed)
        equal = result["request_digest"] == original["request_digest"]
        ensure(equal == probe["equal"], "hash probe " + probe["name"])
        source_equal = result["source_digest"] == original["source_digest"]
        ensure(source_equal == probe["source_equal"], "source hash probe " + probe["name"])
        count += 1
    return count


def second_defect_checks(cases, base):
    """A second independent key defect must survive each declared repair."""
    count = 0
    for case in cases:
        for index, step in enumerate(case["steps"]):
            if "defect" not in step or case["name"] == "key-empty":
                continue
            repaired = copy.deepcopy(case)
            edit(repaired, step["defect"])
            model = Model(expand(repaired.get("setup", {})), base["scope"])
            for candidate in repaired["steps"][:index]:
                execute(model, candidate, base)
            candidate = copy.deepcopy(repaired["steps"][index])
            candidate.setdefault("request_edits", []).append(
                {"path": ["idempotency_key"], "set": ""})
            actual = execute(model, candidate, base)
            ensure(actual == {"error": "InvalidKey"},
                   f"{case['name']} second defect escaped: {actual}")
            count += 1
    return count


def boundary_checks():
    count = 0
    valid = SCHEMA + "\n" + ("ff" * 256) + "\n1\n256\nv1:" + "0" * 64 + "\n"
    # 256-byte opaque ID must also be valid UTF-8.
    valid = valid.replace("ff" * 256, "61" * 256)
    ensure(len(valid) == 602 and decode_outcome(valid)["N"] == 256, "max outcome")
    count += 1
    invalid = [
        valid.replace("\n256\n", "\n257\n"),
        valid.replace("\n256\n", "\n0\n"),
        valid.replace("\n256\n", "\n+1\n"),
        valid.replace("61" * 256, "61" * 257),
        valid.replace("61" * 256, ""),
        valid.replace("61" * 256, "a"),
        valid.replace("\n1\n", "\n01\n"),
        valid.replace("v1:" + "0" * 64, "v1:" + "A" * 64),
        valid.replace("consolidate:v1", "consolidate:v2"),
        valid.replace("61" * 256, "00"),
    ]
    # NUL is a valid opaque scalar under the contract; do not invent a ban.
    invalid.pop()
    for value in invalid:
        try:
            decode_outcome(value)
        except Rejected as error:
            ensure(str(error) == "InvalidBundle", "wrong decoder boundary category")
        else:
            raise AssertionError("malformed boundary accepted")
        count += 1
    for content, expected in (("x" * 4096, []), ("x" * 4097, [4096, 1]),
                              ("\U0001f600" * 1025, [4096, 4]),
                              ("\u20ac" * 4096, [4095, 4095, 4095, 3])):
        ensure([len(utf8(chunk)) for chunk in content_chunks(content)] == expected,
               "UTF-8 packing boundary")
        count += 1
    ensure(decode_outcome(SCHEMA + "\n00\n1\n1\nv1:" + "0" * 64 + "\n")["id"] == "\0",
           "opaque Unicode was silently restricted")
    return count + 1


def precedence_checks(base):
    """Only these deliberately multi-defect probes rely on section 6 order."""
    checks = [
        ({"scope": {"session": "bad"}, "idempotency_key": ""}, "InvalidScope"),
        ({"idempotency_key": "", "C": -1}, "InvalidKey"),
        ({"C": -1, "receipt_window_ms": 0}, "InvalidClock"),
        ({"receipt_window_ms": 0, "inputs": []}, "InvalidReceiptWindow"),
        ({"inputs": [], "summary": {"id": ""}}, "InvalidBundle"),
        ({"summary": {"id": "", "content": ""}}, "InvalidIdentity"),
        ({"summary": {"content": "", "importance": 2}}, "InvalidContent"),
        ({"summary": {"importance": 2, "observed_at_ms": True}}, "InvalidImportance"),
        ({"summary": {"observed_at_ms": True, "expiry_ms": -1}}, "InvalidObservation"),
        ({"summary": {"expiry_ms": -1}, "transformation": ""}, "InvalidExpiry"),
        ({"transformation": "", "evidence": {"id": ""}}, "InvalidProvenance"),
        ({"evidence": {"id": "", "scope": {}}}, "InvalidEvidence"),
        ({"deadline": 10, "summary": {"observed_at_ms": 11, "expiry_ms": 0}},
         "DeadlineExceeded"),
        ({"summary": {"observed_at_ms": 11, "expiry_ms": 0}}, "FutureObservation"),
    ]
    for update, expected in checks:
        model = Model({}, base["scope"])
        actual = execute(model, {"request": update}, base)
        ensure(actual == {"error": expected}, f"precedence {expected}: {actual}")
    for setup, update, expected in [
            ({"sources": []}, {"summary": {"expiry_ms": 10}}, "MissingMemory"),
            ({"sources": [{"id": "m", "malformed": True}]},
             {"summary": {"expiry_ms": 10}}, "MissingMemory"),
            ({"sources": [{"id": "m", "kind": "fact", "revision": 2}]},
             {}, "UnsupportedKind"),
            ({"sources": [{"id": "m", "malformed": True, "kind": "fact"}]},
             {}, "InvalidBundle"),
            ({"sources": [{"id": "m", "malformed": True, "expiry_ms": 10}]},
             {}, "MissingMemory"),
    ]:
        actual = execute(Model(setup, base["scope"]), {"request": update}, base)
        ensure(actual == {"error": expected}, f"state precedence {expected}: {actual}")
    return len(checks) + 5


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--fixture", type=Path, default=FIXTURE)
    args = parser.parse_args()
    try:
        fixture = json.loads(args.fixture.read_text(encoding="utf-8"))
        if args.self_test:
            self_test(fixture)
        else:
            case = json.load(sys.stdin)
            base = fixture["base_request"]
            model = Model(expand(case.get("setup", {})), base["scope"])
            print(json.dumps([execute(model, step, base) for step in case["steps"]],
                             ensure_ascii=False, allow_nan=False))
    except (ValueError, AssertionError, KeyError, TypeError, OSError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
