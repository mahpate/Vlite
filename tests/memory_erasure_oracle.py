"""Independent proposed E1 semantic reference; Python standard library only.

Source: gap1-e1-erasure-barrier-contract.md, subordinate to Gap 1/C2.
No Rust erasure implementation exists or was copied. New private semantics,
not an API/wire/format, storage durability or secure-erasure claim.
Run python tests\\memory_erasure_oracle.py --self-test, also with -O.
Without --self-test stdin is a fixture case; stdout is its projected results.

fixture: scopes, items, ordinary receipts and injected faults are harness setup.
Ordinary "allowed" results indicate only barrier permission, not execution of
remember/forget/C1/S1. Those implementations retain their accepted own oracles.
Seeded ordinary receipts abstract their unchanged decoder as malformed flag,
live expiry, semantic digest and actual graph revision; no A/C1 bytes are invented.
Snapshot reopen is logical JSON equality, NOT evidence for four real backends.
Test graph revisions use the frame sequence; scopes/old receipts default to 1.
Faults are preappend only; no poisoned-handle or arbitrary I/O rollback model.
Golden expectations are static projections. Catalogs expand literal scenarios,
not expected arithmetic/hashes. Each rejected step has exactly one one-leaf
repair; independent second defects must remain after that repair. Explicit
multi-defect precedence probes are kept outside the single-defect goldens.
Groups are append-only by sub-slice: E2/E3/E4 can add distinct groups later.
"""

import argparse
import copy
import hashlib
import json
from pathlib import Path
import re
import struct
import sys


FIXTURE = Path(__file__).parent / "fixtures" / "agent_memory" / "erasure_golden.json"
U64, I64, BODY = 2**64 - 1, 2**63 - 1, 16_777_216
SCHEMA = "erase-scope:v1"
MUTANT = None


class Refusal(ValueError):
    pass


def need(condition, reason):
    if not condition:
        raise Refusal(reason)


def verify(condition, reason):
    if not condition:
        raise AssertionError(reason)


def uint(value, low=0, high=U64):
    return type(value) is int and low <= value <= high


def opaque(value, low=1, high=256):
    try:
        return isinstance(value, str) and low <= len(value.encode("utf-8")) <= high
    except UnicodeEncodeError:
        return False


def sha(raw):
    return hashlib.sha256(raw).hexdigest()


def string(value):
    raw = value.encode("utf-8")
    return struct.pack(">I", len(raw)) + raw


def opt(value):
    return b"\0" if value is None else b"\1" + string(value)


def scope_bytes(scope):
    need(isinstance(scope, dict) and {"tenant", "agent"} <= scope.keys()
         and scope.keys() <= {"tenant", "agent", "user", "session"}, "InvalidScope")
    parts = [scope.get(key) for key in ("tenant", "agent", "user", "session")]
    need(all(opaque(part) for part in parts[:2])
         and all(part is None or opaque(part) for part in parts[2:])
         and (parts[3] is None or parts[2] is not None), "InvalidScope")
    return b"scope:v1" + b"".join(opt(part) for part in parts)


def skey(scope):
    return "scope:v1:" + sha(scope_bytes(scope))


def rkey(op, scope, key):
    return "receipt:v1:" + skey(scope) + ":" + sha((op + ":v1\0" + key).encode("utf-8"))


def jkey(scope):
    return rkey("erase-scope-job", scope, "singleton")


def okey(receipt):
    return "receipt-outcome:v1:" + sha(receipt.encode("utf-8")) + ":0000"


def request_bytes(request, collection):
    sb = scope_bytes(request["scope"])
    window = 0 if MUTANT == "window" else request["receipt_window_ms"]
    revision = 0 if MUTANT == "revision_hash" else request["expected_scope_revision"]
    binding = None if MUTANT == "binding_hash" else collection
    return (b"vlite.memory.erasure.begin.v1\0" + struct.pack(">I", len(sb)) + sb
            + opt(binding) + struct.pack(">QQ", revision, window))


def digest_shape(value):
    return isinstance(value, str) and re.fullmatch(r"v1:[0-9a-f]{64}", value) is not None


def decimal(value, low=0):
    need(isinstance(value, str) and re.fullmatch(r"0|[1-9][0-9]*", value) is not None
         and len(value) <= 20 and low <= int(value) <= U64, "InvalidBundle")
    return int(value)


def merge(base, update):
    value = copy.deepcopy(base)
    for key, item in update.items():
        if isinstance(item, dict) and isinstance(value.get(key), dict):
            value[key] = merge(value[key], item)
        else:
            value[key] = copy.deepcopy(item)
    return value


def expand(value):
    """Fixture-only bounded pattern descriptors expand inputs, never goldens."""
    if isinstance(value, dict):
        if value.keys() == {"$text", "count"}:
            verify(isinstance(value["$text"], str) and uint(value["count"], 0, 257),
                   "invalid input text descriptor")
            return value["$text"] * value["count"]
        return {key: expand(item) for key, item in value.items()}
    if isinstance(value, list):
        return [expand(item) for item in value]
    return value


def validate(request):
    scope_bytes(request.get("scope"))
    need(opaque(request.get("idempotency_key")), "InvalidKey")
    c, window = request.get("now_ms"), request.get("receipt_window_ms")
    need(uint(c, 0, I64), "InvalidClock")
    need(uint(window, 1) and c + window <= U64, "InvalidReceiptWindow")
    need(uint(request.get("expected_scope_revision"), 1), "InvalidTarget")
    need(request.get("deadline_ms") is None or uint(request["deadline_ms"]), "InvalidBundle")
    need(request.keys() <= {"scope", "idempotency_key", "now_ms", "receipt_window_ms",
                            "expected_scope_revision", "deadline_ms"}, "InvalidBundle")


def outcome(job):
    return SCHEMA + "\n" + job + "\n1\npending\nunknown\n"


def make_node(key, label, props, revision):
    return {"key": key, "labels": [label], "properties": props, "revision": revision}


JOB_TYPES = {
    "e1_schema": str, "scope": str, "e1_scope_tuple": str, "e1_collection": str,
    "begin_digest": str, "generation": str, "state": str, "logical_visibility": str,
    "deleted_count": str, "remaining": str, "created_at_ms": int,
}
RECEIPT_TYPES = {
    "e1_schema": str, "scope": str, "e1_scope_tuple": str, "e1_collection": str,
    "digest": str, "expires_at_ms": str, "job_key": str, "outcome_chunks": int,
}
OUTCOME_TYPES = {"receipt_key": str, "outcome": str, "index": int}


def exact(node, key, label, types):
    need(isinstance(node, dict)
         and node.keys() == {"key", "labels", "properties", "revision"}
         and node["key"] == key and node["labels"] == [label]
         and uint(node["revision"], 1), "InvalidBundle")
    props = node["properties"]
    need(isinstance(props, dict)
         and (MUTANT == "loose_properties" or props.keys() == types.keys())
         and types.keys() <= props.keys()
         and all(type(props[name]) is typ for name, typ in types.items())
         and all(not isinstance(value, str) or opaque(value, 0, 4096)
                 for value in props.values()), "InvalidBundle")
    return props


def common(props, scope, collection):
    need(props["e1_schema"] == SCHEMA and props["scope"] == skey(scope), "InvalidBundle")
    need(props["e1_scope_tuple"] == scope_bytes(scope).hex(), "DigestCollision")
    need(props["e1_collection"] == (collection or ""), "InvalidBundle")


def decode_job(node, scope, collection):
    props = exact(node, jkey(scope), "VliteMemoryErasureJob", JOB_TYPES)
    common(props, scope, collection)
    need(digest_shape(props["begin_digest"]) and props["generation"] == "1"
         and props["state"] == "pending" and props["logical_visibility"] == "hidden"
         and props["deleted_count"] == "0" and props["remaining"] == "unknown"
         and uint(props["created_at_ms"], 0, I64), "InvalidBundle")
    return props


def decode_receipt(receipt, nodes, scope, collection, key, job):
    props = exact(receipt, key, "VliteMemoryReceipt", RECEIPT_TYPES)
    common(props, scope, collection)
    need(digest_shape(props["digest"]) and
         (MUTANT == "receipt_job_key" or props["job_key"] == jkey(scope))
         and props["outcome_chunks"] == 1, "InvalidBundle")
    expiry = decimal(props["expires_at_ms"], 1)
    need(job is not None, "InvalidBundle")
    need((MUTANT == "receipt_digest" or
          props["digest"] == job["properties"]["begin_digest"])
         and (MUTANT == "receipt_revision" or receipt["revision"] == job["revision"]),
         "InvalidBundle")
    expected = okey(key)
    siblings = [node for node in nodes.values() if
                node["key"].startswith(expected.rsplit(":", 1)[0] + ":")]
    need(len(siblings) == 1 and expected in nodes, "InvalidBundle")
    decoded = exact(nodes[expected], expected, "VliteMemoryReceiptOutcome", OUTCOME_TYPES)
    need(decoded["receipt_key"] == key and decoded["index"] == 0
         and decoded["outcome"].isascii() and len(decoded["outcome"]) <= 4096
         and (MUTANT == "loose_outcome" or decoded["outcome"] == outcome(jkey(scope))),
         "InvalidBundle")
    return props, expiry


def node_len(node):
    props = node["properties"]
    metadata = 4 + sum(2 + len(name.encode("utf-8")) + 1
                       + (4 + len(value.encode("utf-8")) if type(value) is str else 8)
                       for name, value in props.items())
    need(metadata <= 65_536, "BodyLimit")
    return (8 + 8 + 2 + len(node["key"].encode("utf-8")) + 1
            + sum(1 + len(label.encode("utf-8")) for label in node["labels"]) + 4 + metadata)


class Model:
    def __init__(self, setup, base_scope):
        self.scopes = {}
        self.nodes = {}
        self.old = []
        self.sequence = setup.get("sequence", 1)
        for seed in setup.get("scopes", [{"scope": base_scope}]):
            scope = seed["scope"]
            self.scopes[skey(scope)] = {
                "scope": scope, "tuple": seed.get("tuple", scope_bytes(scope).hex()),
                "revision": seed.get("revision", 1), "collection": seed.get("collection", "c"),
                "items": seed.get("items", ["m"])}
        for receipt in setup.get("ordinary_receipts", []):
            self.old.append(merge({"scope": base_scope, "key": "old", "digest": "same",
                                   "expires": 100, "expiry_valid": True, "revision": 1,
                                   "malformed_scope": False, "malformed_digest": False,
                                   "malformed_outcome": False,
                                   "scope_error": "InvalidBundle",
                                   "digest_error": "InvalidBundle",
                                   "outcome_error": "InvalidBundle",
                                   "result": "original"}, receipt))

    def lookup_scope(self, scope):
        need(skey(scope) in self.scopes, "InvalidScope")
        seed = self.scopes[skey(scope)]
        need(seed["tuple"] == scope_bytes(scope).hex(), "DigestCollision")
        return seed

    def barrier(self, scope):
        if MUTANT == "no_barrier":
            return None
        if MUTANT == "children_barred":
            for seed in self.scopes.values():
                parent = seed["scope"]
                if all(parent.get(name) is None or parent.get(name) == scope.get(name)
                       for name in ("tenant", "agent", "user", "session")):
                    if jkey(parent) in self.nodes:
                        return self.nodes[jkey(parent)]
        return self.nodes.get(jkey(scope))

    def status(self, job, scope, collection, revision=None):
        decode_job(job, scope, collection)
        return {"job_key": jkey(scope), "revision": job["revision"] if revision is None else revision,
                "generation": 1, "state": "pending", "logical_visibility": "hidden",
                "deleted_count": 0, "remaining": None, "resume_token": None}

    def begin(self, request, fault):
        validate(request)
        if MUTANT == "barrier_before_scope" and self.barrier(request["scope"]) is not None:
            need(False, "DeletionPending")
        seed = self.lookup_scope(request["scope"])
        collection = seed["collection"]
        need(collection is None or opaque(collection, 0 if MUTANT == "allow_empty_binding" else 1, 63),
             "InvalidBundle")
        scope, c = request["scope"], request["now_ms"]
        job = self.nodes.get(jkey(scope))
        if job is not None:
            decode_job(job, scope, collection)
        raw = request_bytes(request, collection)
        digest = "v1:" + sha(raw)
        key = rkey("begin-erasure", scope, request["idempotency_key"])
        receipt = self.nodes.get(key)
        if receipt is not None:
            if MUTANT == "skip_expired_decode" and c >= int(
                    receipt["properties"].get("expires_at_ms", "0")):
                props, expiry = receipt["properties"], 0
            else:
                props, expiry = decode_receipt(receipt, self.nodes, scope, collection, key, job)
            live = c <= expiry if MUTANT == "expiry_inclusive" else c < expiry
            if live:
                need(props["digest"] == digest, "IdempotencyConflict")
                return {"status": self.status(job, scope, collection, receipt["revision"]),
                        "replayed": True}
        if MUTANT == "job_after_deadline":
            need(request.get("deadline_ms") is None or c < request["deadline_ms"],
                 "DeadlineExceeded")
            need(job is None or MUTANT == "replace_job", "JobConflict")
        else:
            need(job is None or MUTANT == "replace_job", "JobConflict")
        if MUTANT == "revision_before_deadline":
            need(MUTANT == "skip_scope_revision"
                 or seed["revision"] == request["expected_scope_revision"], "RevisionConflict")
            need(request.get("deadline_ms") is None or c < request["deadline_ms"],
                 "DeadlineExceeded")
        elif MUTANT != "job_after_deadline":
            need(request.get("deadline_ms") is None or c < request["deadline_ms"],
                 "DeadlineExceeded")
        if MUTANT != "revision_before_deadline":
            need(MUTANT == "skip_scope_revision"
                 or seed["revision"] == request["expected_scope_revision"], "RevisionConflict")
        revision = self.sequence + 1
        props = {"e1_schema": SCHEMA, "scope": skey(scope),
                 "e1_scope_tuple": scope_bytes(scope).hex(), "e1_collection": collection or ""}
        job_node = make_node(jkey(scope), "VliteMemoryErasureJob", dict(
            props, begin_digest=digest, generation="1", state="pending",
            logical_visibility="hidden", deleted_count="0", remaining="unknown",
            created_at_ms=c), revision)
        out_node = make_node(okey(key), "VliteMemoryReceiptOutcome", {
            "receipt_key": key, "index": 0, "outcome": outcome(jkey(scope))}, revision)
        receipt_node = make_node(key, "VliteMemoryReceipt", dict(
            props, digest=digest, expires_at_ms=str(c + request["receipt_window_ms"]),
            job_key=jkey(scope), outcome_chunks=1), revision)
        planned = [job_node, out_node, receipt_node]
        physical = len(planned)
        lengths = [node_len(node) for node in planned]
        body = 4 + sum(lengths)
        need(physical < 4096 if MUTANT == "ops_exclusive" else physical <= 4096,
             "OperationLimit")
        need(body < BODY if MUTANT == "frame_exclusive" else body <= BODY, "BodyLimit")
        if MUTANT == "non_atomic" and fault:
            self.nodes[jkey(scope)] = job_node
        need(not fault, "fixture:PreappendRefused")
        for node in planned:
            need(node["key"] not in self.nodes or MUTANT == "replace_job", "InvalidBundle")
        self.nodes.update({node["key"]: node for node in planned})
        self.sequence = revision
        if MUTANT == "delete_items":
            seed["items"] = []
        return {"status": self.status(job_node, scope, collection), "replayed": False,
                "begin_hex": raw.hex(), "digest": digest, "receipt_key": key,
                "outcome_key": okey(key), "outcome": outcome(jkey(scope)),
                "outcome_bytes": len(outcome(jkey(scope))), "physical_ops": physical,
                "entry_bytes": lengths, "frame_bytes": body,
                "frame_bytes_with_reclaim": (
                    body if MUTANT == "omit_reclaim_bytes" else body + 16),
                "nodes": planned}

    def gate(self, op, request):
        need(op in ("provision_scope", "erasure_status", "remember", "forget",
                    "consolidate", "summary_status", "scope_revision"), "InvalidBundle")
        scope = request["scope"]
        scope_bytes(scope)
        if op == "provision_scope":
            if skey(scope) not in self.scopes:
                need(jkey(scope) not in self.nodes, "InvalidBundle")
                self.scopes[skey(scope)] = {"scope": scope, "tuple": scope_bytes(scope).hex(),
                                           "collection": "c", "items": [],
                                           "revision": self.sequence + 1}
                self.sequence += 1
                return {"status": "allowed"}
        if MUTANT == "barrier_before_scope" and self.barrier(scope) is not None:
            need(False, "DeletionPending")
        self.lookup_scope(scope)
        job = self.barrier(scope)
        if op == "erasure_status":
            need(job is not None, "MissingMemory")
            return {"status": self.status(job, scope, self.scopes[skey(scope)]["collection"])}
        if op == "scope_revision":
            return {"revision": self.scopes[skey(scope)]["revision"]}
        if job is not None and MUTANT != "skip_" + op:
            if op in ("remember", "forget", "consolidate"):
                old = [receipt for receipt in self.old
                       if (MUTANT == "old_cross_scope" or
                           scope_bytes(receipt["scope"]) == scope_bytes(scope))
                       and receipt["op"] == op
                       and (MUTANT == "old_cross_key" or
                            receipt["key"] == request.get("key", "old"))]
                if old:
                    receipt = old[0]
                    live = (not receipt["expiry_valid"] or
                            request.get("now_ms", 10) < receipt["expires"])
                    if op in ("remember", "forget") and not live and MUTANT != "decode_expired_old":
                        continue_replay = False
                    else:
                        continue_replay = True
                    if continue_replay:
                        if op == "consolidate":
                            need(receipt["expiry_valid"], receipt.get("expiry_error", "InvalidBundle"))
                        need(not receipt["malformed_scope"], receipt["scope_error"])
                        need(not receipt["malformed_digest"], receipt["digest_error"])
                        need(not receipt["malformed_outcome"], receipt["outcome_error"])
                    older = receipt["revision"] < job["revision"]
                    matches = receipt["digest"] == request.get("digest", "same")
                    if MUTANT != "no_old_replay" and continue_replay and live and matches and (
                            older or MUTANT == "ignore_old_revision"):
                        return {"status": "replayed", "result": receipt["result"]}
            need(False, "DeletionPending")
        return {"status": "allowed"}

    def recall(self, scopes, seeds=None):
        need(len({scope_bytes(scope) for scope in scopes}) == len(scopes), "InvalidScopes")
        requested = {scope_bytes(scope) for scope in scopes}
        seeds = seeds or []
        for seed in seeds:
            need(scope_bytes(seed["scope"]) in requested, "InvalidScopes")
        # This is the existing whole-request scan preflight, before per-scope probes.
        preflight_scopes = len(scopes)
        hits, barred, seen = [], [], set()
        barred_seed = False
        for scope in scopes:
            job = self.barrier(scope)
            if MUTANT == "recall_before_scope" and job is not None:
                seed = self.scopes.get(skey(scope))
            elif MUTANT == "recall_before_tuple" and job is not None:
                seed = self.scopes.get(skey(scope))
            else:
                seed = self.lookup_scope(scope)
            key = skey(scope)
            if job is not None and MUTANT != "skip_recall":
                if any(scope_bytes(item["scope"]) == scope_bytes(scope) for item in seeds):
                    if MUTANT == "early_barred_seed":
                        need(False, "DeletionPending")
                    barred_seed = True
                if key not in seen and MUTANT != "no_diagnostic":
                    barred.append(copy.deepcopy(scope))
                seen.add(key)
                continue
            hits.extend({"scope": key, "id": item} for item in seed["items"])
        need(not barred_seed, "DeletionPending")
        return {"hits": hits, "deletion_barriers": barred, "partial": False,
                "preflight_scope_count": preflight_scopes}


def at(value, path):
    for key in path:
        if key == "$only":
            verify(len(value) == 1, "$only needs one entry")
            key = next(iter(value))
        value = value[key]
    return value


def edit(value, change):
    parent = at(value, change["path"][:-1])
    key = change["path"][-1]
    if change.get("delete"):
        del parent[key]
    else:
        parent[key] = copy.deepcopy(change["set"])


def execute(model, step, base):
    for change in step.get("edits", []):
        edit(model.__dict__, change)
    if step.get("corrupt", {}).get("enabled"):
        change = step["corrupt"]
        key = {"job": jkey(base["scope"]),
               "receipt": rkey("begin-erasure", base["scope"], base["idempotency_key"]),
               "outcome": okey(rkey("begin-erasure", base["scope"],
                                   base["idempotency_key"]))}[change["node"]]
        edit(model.nodes[key], change["change"])
    before = copy.deepcopy(model.__dict__)
    request = expand(merge(base, step.get("request", {})))
    for change in step.get("request_edits", []):
        edit(request, change)
    op = step.get("op", "begin")
    try:
        if op == "begin":
            return model.begin(request, step.get("fault", False))
        if op == "inspect":
            return copy.deepcopy(model.__dict__)
        if op == "reopen":
            model.__dict__ = json.loads(json.dumps(model.__dict__, ensure_ascii=False))
            verify(model.__dict__ == before, "reopen changed logical state")
            return {"reopened": True}
        if op == "recall":
            return model.recall(step.get("scopes", [request["scope"]]), step.get("seeds"))
        if op == "vectors":
            validate(request)
            collection = step.get("collection", "c")
            raw = request_bytes(request, collection)
            return {"begin_hex": raw.hex(), "digest": "v1:" + sha(raw),
                    "job_key": jkey(request["scope"]),
                    "receipt_key": rkey("begin-erasure", request["scope"],
                                        request["idempotency_key"])}
        result = model.gate(op, request)
        if op in ("erasure_status", "summary_status", "scope_revision"):
            verify(model.__dict__ == before, "status read mutated state")
        return result
    except Refusal as error:
        verify(before == model.__dict__, "refusal changed state")
        return {"error": str(error)}


def project(actual, expected):
    if isinstance(expected, dict):
        return {key: project(actual[key], value) for key, value in expected.items()}
    return actual


def run_case(case, base, check=True):
    model = Model(expand(case.get("setup", {})), base["scope"])
    results = []
    for index, step in enumerate(case["steps"]):
        try:
            actual = execute(model, step, base)
            if check:
                verify(project(actual, step["expect"]) == step["expect"],
                       f"expected {step['expect']!r}, got {actual!r}")
        except (KeyError, AssertionError) as error:
            raise AssertionError(f"{case['name']} step {index}: {error}") from None
        results.append(actual)
    return results


def cases(fixture):
    result = copy.deepcopy(fixture["groups"]["E1"]["cases"])
    for item in fixture["groups"]["E1"].get("rejections", []):
        result.append({"name": item["name"], "setup": item.get("setup", {}),
                       "assumptions": ["spec: E1 structural validation with one repair."],
                       "steps": [{"request": item["request"], "expect": {"error": item["error"]},
                                  "defect": dict(item["repair"], tag=item["name"])}]})
    for item in fixture["groups"]["E1"].get("matrix", []):
        if "ops" not in item:
            result.append(copy.deepcopy(item))
            continue
        for op in item["ops"]:
            value = copy.deepcopy(item)
            value["name"] += "-" + op
            del value["ops"]
            value["steps"][1]["op"] = op
            result.append(value)
    for item in fixture["groups"]["E1"].get("corruptions", []):
        result.append({
            "name": item["name"] + "-live",
            "assumptions": ["spec: strict reserved object decoding.",
                            "fixture: one raw corrupt-field toggle."],
            "steps": [
                {"expect": {"replayed": False}},
                {"request": {"now_ms": 11},
                 "corrupt": {"enabled": True, "node": item["node"], "change": item["change"]},
                 "expect": {"error": item.get("error", "InvalidBundle")},
                 "defect": {"tag": item["name"],
                            "path": ["steps", 1, "corrupt", "enabled"], "set": False}},
            ]})
    return result


def repairs(all_cases, base):
    repaired_count, second_count = 0, 0
    for case in all_cases:
        for index, step in enumerate(case["steps"]):
            verify(("error" in step["expect"]) == ("defect" in step),
                   "missing/extra defect: " + case["name"])
            if "defect" not in step:
                continue
            defect = step["defect"]
            verify(defect.keys() in ({"tag", "path", "set"}, {"tag", "path", "delete"}),
                   "bad defect fields")
            fixed = copy.deepcopy(case)
            parent = at(fixed, defect["path"][:-1])
            key = defect["path"][-1]
            old = parent.get(key) if isinstance(parent, dict) else parent[key]
            verify(not isinstance(old, (dict, list)) or not old, "non-leaf repair")
            edit(fixed, defect)
            fixed["steps"] = fixed["steps"][:index + 1]
            actual = run_case(fixed, base, check=False)
            verify("error" not in actual[-1], f"{case['name']} repair left {actual[-1]!r}")
            repaired_count += 1
            if fixed["steps"][index].get("op", "begin") == "begin":
                fixed["steps"][index].setdefault("request", {})["idempotency_key"] = ""
                if defect["path"][-1] == "idempotency_key":
                    continue
                second = run_case(fixed, base, check=False)[-1]
                verify(second == {"error": "InvalidKey"}, "second defect escaped")
                second_count += 1
    return repaired_count, second_count


def self_test(fixture):
    verify(fixture["schema"] == "fixture.memory-erasure.v1", "fixture schema")
    base = fixture["base_request"]
    all_cases = cases(fixture)
    names = set()
    for case in all_cases:
        verify(case["name"] not in names and case.get("assumptions"), "case identity")
        verify(all(item.startswith(("spec: ", "fixture: ")) for item in case["assumptions"]),
               "unlabelled fixture assumption")
        names.add(case["name"])
        run_case(case, base)
    sample = run_case(next(case for case in all_cases if case["name"] == "exact-vectors"), base)[0]
    for node, expected in zip(sample["nodes"], fixture["groups"]["E1"]["node_manifests"]):
        types = {name: "String" if type(value) is str else "Integer"
                 for name, value in node["properties"].items()}
        verify(types == expected["types"] and node["labels"] == expected["labels"],
               "static node property/label manifest")
        verify(node.keys() == {"key", "labels", "properties", "revision"},
               "unexpected graph envelope/TTL")
        verify(project(node["properties"], expected["values"]) == expected["values"],
               "static node property values")
    repaired, second = repairs(all_cases, base)
    precedence = precedence_probes(fixture)
    vectors = 0
    base_vector = execute(Model({}, base["scope"]), {"op": "vectors"}, base)
    for probe in fixture["groups"]["E1"]["digest_probes"]:
        if "baseline" in probe:
            base_vector = execute(Model({}, base["scope"]),
                                  dict(probe["baseline"], op="vectors"), base)
        else:
            base_vector = execute(Model({}, base["scope"]), {"op": "vectors"}, base)
        actual = execute(Model({}, base["scope"]), dict(probe["step"], op="vectors"), base)
        for field in ("digest", "job_key", "receipt_key"):
            verify((actual[field] == base_vector[field]) == probe["equal"][field],
                   "digest probe " + probe["name"] + " " + field)
        vectors += 1
    killed = 0
    global MUTANT
    for mutation in fixture["mutations"]:
        MUTANT = mutation["name"]
        try:
            if "probe" in mutation:
                precedence_probes(fixture, mutation["probe"])
            else:
                case = next(item for item in all_cases if item["name"] == mutation["case"])
                run_case(case, base)
        except AssertionError as error:
            named_probes = {
                "job-conflict-before-deadline", "revision-after-deadline",
                "missing-scope-before-barrier", "tuple-collision-before-barrier",
                "begin-missing-scope-before-barrier", "begin-tuple-collision-before-barrier",
                "recall-missing-scope-before-barrier", "recall-tuple-collision-before-barrier",
                "recall-barred-seed-after-scope-loop",
                "expired-malformed-remember", "expired-malformed-forget",
                "expired-malformed-consolidate",
            }
            prefix = ((mutation["probe"] + ":" if mutation["probe"] in named_probes
                       else "expired precedence " + mutation["probe"])
                      if "probe" in mutation else f"{case['name']} step {mutation['step']}:")
            verify(str(error).startswith(prefix) and mutation["reason"] in str(error),
                   f"unintended mutation kill: {error}")
            killed += 1
        else:
            raise AssertionError("surviving mutation: " + MUTANT)
        finally:
            MUTANT = None
    equivalent = 0
    for mutation in fixture.get("equivalent_mutations", []):
        MUTANT = mutation["name"]
        try:
            for case in all_cases:
                run_case(case, base)
        finally:
            MUTANT = None
        equivalent += 1
    print(f"PASS E1: {len(all_cases)} cases; {sum(len(c['steps']) for c in all_cases)} steps; "
          f"{repaired} single defects repaired; {second} second defects; {vectors} digest probes; "
          f"{precedence} explicit precedence probes; "
          f"{killed}/{len(fixture['mutations'])} intended mutations; "
          f"{equivalent} bounded-equivalent survivors")


def precedence_probes(fixture, only=None):
    """Malformed expired state precedes permanent-job conflict, not a golden
    with an impossible one-leaf repair to fresh creation. These two conditions
    are deliberately combined only here; removing corruption yields JobConflict.
    """
    count, base = 0, fixture["base_request"]
    named = (
        "job-conflict-before-deadline", "revision-after-deadline",
        "missing-scope-before-barrier", "tuple-collision-before-barrier",
        "begin-missing-scope-before-barrier", "begin-tuple-collision-before-barrier",
        "recall-missing-scope-before-barrier", "recall-tuple-collision-before-barrier",
        "recall-barred-seed-after-scope-loop",
        "expired-malformed-remember", "expired-malformed-forget",
        "expired-malformed-consolidate",
    )
    if only is None:
        count += sum(precedence_probes(fixture, name) for name in named)
    if only in ("job-conflict-before-deadline", "revision-after-deadline"):
        model = Model({}, base["scope"])
        if only == "job-conflict-before-deadline":
            execute(model, {}, base)
            actual = execute(model, {"request": {"now_ms": 110, "deadline_ms": 0}}, base)
            verify(actual == {"error": "JobConflict"},
                   f"{only}: expected JobConflict, got {actual}")
        else:
            actual = execute(model, {"request": {
                "deadline_ms": 0, "expected_scope_revision": 2}}, base)
            verify(actual == {"error": "DeadlineExceeded"},
                   f"{only}: expected DeadlineExceeded, got {actual}")
        return 1
    if only in ("missing-scope-before-barrier", "tuple-collision-before-barrier",
                "begin-missing-scope-before-barrier", "begin-tuple-collision-before-barrier"):
        model = Model({}, base["scope"])
        execute(model, {}, base)
        if only in ("missing-scope-before-barrier", "begin-missing-scope-before-barrier"):
            model.scopes.clear()
            expected = "InvalidScope"
        else:
            model.scopes[skey(base["scope"])]["tuple"] = "00"
            expected = "DigestCollision"
        operation = "begin" if only.startswith("begin-") else "remember"
        actual = execute(model, {"op": operation}, base)
        verify(actual == {"error": expected},
               f"{only}: expected {expected}, got {actual}")
        return 1
    if only in ("recall-missing-scope-before-barrier",
                "recall-tuple-collision-before-barrier",
                "recall-barred-seed-after-scope-loop"):
        model = Model({}, base["scope"])
        execute(model, {}, base)
        if only == "recall-missing-scope-before-barrier":
            model.scopes.clear()
            step = {"op": "recall", "scopes": [base["scope"]]}
            expected = "InvalidScope"
        elif only == "recall-tuple-collision-before-barrier":
            model.scopes[skey(base["scope"])]["tuple"] = "00"
            step = {"op": "recall", "scopes": [base["scope"]]}
            expected = "DigestCollision"
        else:
            missing = {"tenant": "missing", "agent": "a"}
            step = {"op": "recall", "scopes": [base["scope"], missing],
                    "seeds": [{"scope": base["scope"], "id": "secret"}]}
            expected = "InvalidScope"
        actual = execute(model, step, base)
        verify(actual == {"error": expected},
               f"{only}: expected {expected}, got {actual}")
        return 1
    if only in ("expired-malformed-remember", "expired-malformed-forget",
                "expired-malformed-consolidate"):
        operation = only.rsplit("-", 1)[-1]
        model = Model({"ordinary_receipts": [{
            "op": operation, "expires": 10, "malformed_scope": True,
            "scope_error": "DigestCollision",
        }]}, base["scope"])
        execute(model, {}, base)
        actual = execute(model, {
            "op": operation, "request": {"now_ms": 10},
        }, base)
        expected = "DigestCollision" if operation == "consolidate" else "DeletionPending"
        verify(actual == {"error": expected},
               f"{only}: expected {expected}, got {actual}")
        return 1
    for corruption in fixture["groups"]["E1"]["corruptions"]:
        if only is not None and corruption["name"] != only:
            continue
        model = Model({}, base["scope"])
        execute(model, {}, base)
        candidate = {"request": {"now_ms": 110},
                     "corrupt": dict(corruption, enabled=True)}
        actual = execute(model, candidate, base)
        verify(actual == {"error": corruption.get("error", "InvalidBundle")},
               f"expired precedence {corruption['name']}: "
               f"expected {corruption.get('error', 'InvalidBundle')}, got {actual}")
        model = Model({}, base["scope"])
        execute(model, {}, base)
        verify(execute(model, {"request": {"now_ms": 110}}, base)
               == {"error": "JobConflict"}, "expired valid receipt replaced barrier")
        count += 1
    return count


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
            print(json.dumps(run_case(json.load(sys.stdin), fixture["base_request"], check=False)))
    except (ValueError, KeyError, TypeError, AssertionError, OSError) as error:
        print("FAIL: " + str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
