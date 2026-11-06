"""STATUS: DRAFT, paused 2026-11-05; unreviewed; NOT ratifiable; PARTIAL (see Q:\\f1-scratch\\oracle-notes.md).

Independent proposed E2 semantic reference; Python standard library only.
Source: gap1-e2-resume-contract.md (commit 336874db) read-only, plus the accepted E1
contract/oracle/golden (pure helpers imported read-only from memory_erasure_oracle.py).
No Rust was read or copied. Private semantics only, not an API/wire/format claim.
Run python tests\\memory_erasure_resume_oracle.py --self-test, also with -O.
Without --self-test stdin is one fixture case; stdout is its step results (E1 protocol).

MODELLED (partial): E1 begin, erasure_job, E1 erasure_status on v2, structural
validation, steps 1-11 of section 2 (receipt/job decoders in condensed form),
fence, replay, window/decode/fit rule/frame order for plain memories (private edges,
evidence flag), v1->v2, results, logical, reopen, preappend fault, corrupt toggles.
NOT MODELLED yet: TTL/high_water/race, content nodes, shared edges, record owners,
barrier matrix after chunks, maintenance relabel, child/sibling scopes, commit
indeterminate faults, most precedence probes, per-rule decoder goldens (a sample only).

Assumptions (also in the golden case `assumptions`, details in the notes file):
A1 a memory with no evidence and no attachment needs no evidence collection (the
   Stops paragraph says "over a scope with evidence"; step 1 literally says
   "Otherwise the handle must carry a collection").
A2 begin replay over a v2 job returns E1's constant begin view with the receipt revision.
A3 "no controls" in opaque keys means Unicode category Cc.
A4 decoder rules evaluate in listed order; goldens pin no multi-defect decoder order.
A5 request values "$job" (fence read via erasure_job) and "$derived" (JobKey) are
   inputs obtained through the API, never expected values; "$same_as": n copies the
   resolved request of step n (retries).
A6 frame operations are pinned as ordered run-length lists of operation kinds.
A7 revisions/sequences are model-relative like E1 (one setup frame = sequence 1).
"""

import argparse
import copy
import importlib.util
import itertools
import json
from pathlib import Path
import re
import struct
import sys
import unicodedata

HERE = Path(__file__).parent
FIXTURE = HERE / "fixtures" / "agent_memory" / "erasure_resume_golden.json"
_spec = importlib.util.spec_from_file_location("erasure_e1_readonly", HERE / "memory_erasure_oracle.py")
E1 = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(E1)
need, verify, Refusal = E1.need, E1.verify, E1.Refusal
U64, I64 = E1.U64, E1.I64
MUTANT = None
MAX_OPS, WINDOW = 4096, 4097
JOB_V1 = E1.JOB_TYPES
JOB_V2 = dict(E1.JOB_TYPES, e2_chunks=str)


def token_of(scope, binding, revision, chunks, generation=1):
    sb = E1.scope_bytes(scope)
    revision = 0 if MUTANT == "token_ignores_revision" else revision
    raw = (b"vlite.memory.erasure.token.v1\0" + struct.pack(">I", len(sb)) + sb + E1.opt(binding)
           + E1.string(E1.jkey(scope)) + struct.pack(">QQQ", generation, revision, chunks))
    return "t1:" + E1.sha(raw)


def resume_bytes(request, binding):
    sb = E1.scope_bytes(request["scope"])
    token = "" if MUTANT == "digest_omits_token" else request["resume_token"]
    window = 0 if MUTANT == "digest_omits_window" else request["receipt_window_ms"]
    return (b"vlite.memory.erasure.resume.v1\0" + struct.pack(">I", len(sb)) + sb + E1.opt(binding)
            + E1.string(E1.jkey(request["scope"]))
            + struct.pack(">QQ", request["expected_revision"], request["generation"])
            + E1.string(token) + struct.pack(">Q", window))


def opaque_key(value):
    return E1.opaque(value) and not any(unicodedata.category(c) == "Cc" for c in value)


def validate(request, binding):
    E1.scope_bytes(request.get("scope"))
    need(opaque_key(request.get("idempotency_key")), "InvalidKey")
    c, w = request.get("now_ms"), request.get("receipt_window_ms")
    need(E1.uint(c, 0, I64), "InvalidClock")
    need(E1.uint(w, 1) and c + w <= U64, "InvalidReceiptWindow")
    need(E1.uint(request.get("expected_revision"), 1) and E1.uint(request.get("generation"), 1),
         "InvalidTarget")
    need(opaque_key(request.get("job_key")), "InvalidKey")
    need(isinstance(request.get("resume_token"), str)
         and re.fullmatch(r"t1:[0-9a-f]{64}", request["resume_token"]) is not None, "InvalidTarget")
    need(binding is None or E1.opaque(binding, 1, 63), "InvalidBundle")
    need(request.get("deadline_ms") is None or E1.uint(request["deadline_ms"]), "InvalidBundle")


def canon(text, low=0, high=U64):
    need(isinstance(text, str) and re.fullmatch(r"0|[1-9][0-9]*", text) is not None, "InvalidBundle")
    need(low <= int(text) <= high, "InvalidBundle")
    return int(text)


def decode_job(node, scope, binding):
    probe = node["properties"] if isinstance(node, dict) and isinstance(node.get("properties"), dict) else {}
    version = 2 if probe.get("e1_schema") == "erase-scope:v2" else 1
    props = E1.exact(node, E1.jkey(scope), "VliteMemoryErasureJob", JOB_V2 if version == 2 else JOB_V1)
    need(props["e1_schema"] == "erase-scope:v%d" % version and props["scope"] == E1.skey(scope),
         "InvalidBundle")
    need(props["e1_scope_tuple"] == E1.scope_bytes(scope).hex(), "DigestCollision")
    need(props["e1_collection"] == (binding or ""), "InvalidBundle")
    need(E1.digest_shape(props["begin_digest"]) and props["generation"] == "1"
         and props["logical_visibility"] == "hidden" and E1.uint(props["created_at_ms"], 0, I64),
         "InvalidBundle")
    need(props["state"] in ("pending", "logical"), "InvalidBundle")
    deleted = E1.decimal(props["deleted_count"])
    remaining = None
    if props["remaining"] != "unknown":
        remaining = E1.decimal(props["remaining"])
        need(remaining <= 4095, "InvalidBundle")
    chunks = E1.decimal(props["e2_chunks"], 1) if version == 2 else 0
    need((props["state"] == "logical") == (remaining == 0), "InvalidBundle")
    if version == 1:
        need(props["state"] == "pending" and deleted == 0 and remaining is None, "InvalidBundle")
    else:
        need(deleted <= 2046 * chunks, "InvalidBundle")
    return {"version": version, "state": props["state"], "deleted": deleted, "remaining": remaining,
            "chunks": chunks, "revision": node["revision"], "props": props}


def parse_outcome(text, job_key):
    need(isinstance(text, str) and text.isascii() and len(text) < 512, "InvalidBundle")
    lines = text.split("\n")
    need(len(lines) == 8 and lines[7] == "" and lines[0] == "erase-chunk:v1" and lines[1] == job_key,
         "InvalidBundle")
    chunk_no, deleted, total = canon(lines[2], 1), canon(lines[3]), canon(lines[4])
    remaining = None if lines[5] == "unknown" else canon(lines[5], 0, 4095)
    state = lines[6]
    need(state in ("pending", "logical") and (state == "logical") == (remaining == 0), "InvalidBundle")
    need(deleted <= 2046 and deleted <= total <= 2046 * chunk_no
         and (chunk_no != 1 or total == deleted) and (deleted != 0 or remaining == 0), "InvalidBundle")
    return {"chunk_no": chunk_no, "deleted": deleted, "total": total, "remaining": remaining,
            "state": state, "remaining_text": lines[5]}


def expand_memories(items):
    result = []
    for item in items:
        if isinstance(item, dict) and item.keys() == {"$memories"}:
            spec = item["$memories"]
            verify(E1.uint(spec["count"], 0, 5000), "bad memories descriptor")
            for index in range(spec["count"]):
                result.append({"id": spec.get("prefix", "m") + str(index),
                               "edges": spec.get("edges", 0)})
        else:
            result.append(copy.deepcopy(item))
    return result


class Model:
    def __init__(self, setup, base_scope):
        self.binding = setup.get("binding", "c")
        self.sequence = setup.get("sequence", 1)
        self.scopes = {}
        for scope in setup.get("scopes", [base_scope]):
            self.scopes[E1.skey(scope)] = {"scope": scope, "tuple": E1.scope_bytes(scope).hex(),
                                          "revision": 1}
        self.nodes = {}
        self.memories = []
        for item in expand_memories(setup.get("memories", [])):
            memory = dict({"edges": 0, "evidence": False, "decode_fault": False, "foreign": False},
                          **item)
            memory["scope"] = E1.skey(item.get("scope", base_scope))
            self.memories.append(memory)
        self.gone, self.records = [], []

    def lookup_scope(self, scope):
        need(E1.skey(scope) in self.scopes, "InvalidScope")
        seed = self.scopes[E1.skey(scope)]
        need(seed["tuple"] == E1.scope_bytes(scope).hex(), "DigestCollision")
        return seed

    def v1_view(self, scope, revision):
        return {"job_key": E1.jkey(scope), "revision": revision, "generation": 1, "state": "pending",
                "logical_visibility": "hidden", "deleted_count": 0, "remaining": None,
                "resume_token": None}

    def job_view(self, scope, revision, chunks, state, deleted, remaining):
        return {"job_key": E1.jkey(scope), "revision": revision, "generation": 1, "state": state,
                "logical_visibility": "hidden", "deleted_count": deleted, "remaining": remaining,
                "chunks": chunks,
                "resume_token": token_of(scope, self.binding, revision, chunks)
                if state == "pending" else None}

    def begin_receipt(self, receipt, key, scope, info, job):
        if info is not None and info["version"] == 2 and MUTANT != "require_begin_revision_equality":
            receipt = dict(receipt, revision=job["revision"])
        return E1.decode_receipt(receipt, self.nodes, scope, self.binding, key, job)

    def begin(self, request, fault):
        E1.validate(request)
        seed = self.lookup_scope(request["scope"])
        need(self.binding is None or E1.opaque(self.binding, 1, 63), "InvalidBundle")
        scope, c = request["scope"], request["now_ms"]
        job = self.nodes.get(E1.jkey(scope))
        info = decode_job(job, scope, self.binding) if job is not None else None
        raw = E1.request_bytes(request, self.binding)
        digest = "v1:" + E1.sha(raw)
        key = E1.rkey("begin-erasure", scope, request["idempotency_key"])
        receipt = self.nodes.get(key)
        if receipt is not None:
            props, expiry = self.begin_receipt(receipt, key, scope, info, job)
            if c < expiry:
                need(props["digest"] == digest, "IdempotencyConflict")
                return {"replayed": True, "status": self.v1_view(scope, receipt["revision"])}
        need(job is None, "JobConflict")
        need(request.get("deadline_ms") is None or c < request["deadline_ms"], "DeadlineExceeded")
        need(seed["revision"] == request["expected_scope_revision"], "RevisionConflict")
        need(not fault, "fixture:PreappendRefused")
        revision = self.sequence + 1
        common = {"e1_schema": "erase-scope:v1", "scope": E1.skey(scope),
                  "e1_scope_tuple": E1.scope_bytes(scope).hex(), "e1_collection": self.binding or ""}
        planned = [
            E1.make_node(E1.jkey(scope), "VliteMemoryErasureJob", dict(
                common, begin_digest=digest, generation="1", state="pending",
                logical_visibility="hidden", deleted_count="0", remaining="unknown",
                created_at_ms=c), revision),
            E1.make_node(E1.okey(key), "VliteMemoryReceiptOutcome", {
                "receipt_key": key, "index": 0, "outcome": E1.outcome(E1.jkey(scope))}, revision),
            E1.make_node(key, "VliteMemoryReceipt", dict(
                common, digest=digest, expires_at_ms=str(c + request["receipt_window_ms"]),
                job_key=E1.jkey(scope), outcome_chunks=1), revision)]
        self.nodes.update({node["key"]: node for node in planned})
        self.sequence = revision
        return {"replayed": False, "status": self.v1_view(scope, revision), "begin_hex": raw.hex(),
                "digest": digest, "receipt_key": key, "outcome_key": E1.okey(key)}

    def erasure_job(self, scope):
        E1.scope_bytes(scope)
        self.lookup_scope(scope)
        job = self.nodes.get(E1.jkey(scope))
        need(job is not None, "MissingMemory")
        i = decode_job(job, scope, self.binding)
        return {"job": self.job_view(scope, i["revision"], i["chunks"], i["state"], i["deleted"],
                                     i["remaining"])}

    def status(self, scope):
        E1.scope_bytes(scope)
        self.lookup_scope(scope)
        job = self.nodes.get(E1.jkey(scope))
        need(job is not None, "MissingMemory")
        need(decode_job(job, scope, self.binding)["version"] == 1, "InvalidBundle")
        return {"status": self.v1_view(scope, job["revision"])}

    def decode_resume_receipt(self, receipt, key, scope, info):
        props = E1.exact(receipt, key, "VliteMemoryReceipt", E1.RECEIPT_TYPES)
        need(props["e1_schema"] == "erase-chunk:v1" and props["scope"] == E1.skey(scope), "InvalidBundle")
        need(props["e1_scope_tuple"] == E1.scope_bytes(scope).hex(), "DigestCollision")
        need(props["e1_collection"] == (self.binding or ""), "InvalidBundle")
        need(E1.digest_shape(props["digest"]) and props["job_key"] == E1.jkey(scope)
             and props["outcome_chunks"] == 1, "InvalidBundle")
        expiry = E1.decimal(props["expires_at_ms"], 1)
        outcome_key = E1.okey(key)
        need(outcome_key in self.nodes and outcome_key[:-4] + "0001" not in self.nodes, "InvalidBundle")
        need(info is not None and info["version"] == 2, "InvalidBundle")
        node = E1.exact(self.nodes[outcome_key], outcome_key, "VliteMemoryReceiptOutcome",
                        E1.OUTCOME_TYPES)
        need(node["receipt_key"] == key and node["index"] == 0, "InvalidBundle")
        parsed = parse_outcome(node["outcome"], E1.jkey(scope))
        latest = (receipt["revision"] == info["revision"] if MUTANT == "latest_by_revision"
                  else parsed["chunk_no"] == info["chunks"])
        need(parsed["chunk_no"] <= info["chunks"] and parsed["total"] <= info["deleted"]
             and (parsed["state"] != "logical" or parsed["chunk_no"] == info["chunks"]), "InvalidBundle")
        if latest:
            need(parsed["total"] == info["deleted"] and parsed["remaining"] == info["remaining"]
                 and parsed["state"] == info["state"], "InvalidBundle")
        return {"digest": props["digest"], "expiry": expiry, "parsed": parsed,
                "revision": receipt["revision"]}

    def plan(self, scope_key):
        mine = [m for m in self.memories if m["scope"] == scope_key]
        if MUTANT == "order_by_key":
            mine = sorted(mine, key=lambda m: m["id"])
        window = mine[:WINDOW]
        for memory in window if MUTANT != "decode_chunk_only" else []:
            need(not memory["decode_fault"], "InvalidBundle")
        known = len(window) < WINDOW
        chosen, evidence, edges = [], 0, 0
        for memory in window:
            need(not memory["foreign"], "ForeignAttachment")
            need(not memory["evidence"] or self.binding is not None, "InvalidEvidence")
            gain = 2 if memory["evidence"] else 0
            cost = 4 + evidence + gain + edges + memory["edges"] + 2 * (len(chosen) + 1)
            cost += {"guards_low": -1, "guards_high": 1}.get(MUTANT, 0)
            if memory["edges"] > 4096 or cost > MAX_OPS:
                need(chosen, "OperationLimit")
                break
            if MUTANT == "decode_chunk_only":
                need(not memory["decode_fault"], "InvalidBundle")
            chosen.append(memory)
            evidence += gain
            edges += memory["edges"]
        return chosen, len(window), known

    def resume(self, request, fault):
        validate(request, self.binding)
        scope, now = request["scope"], request["now_ms"]
        self.lookup_scope(scope)
        job = self.nodes.get(E1.jkey(scope))
        info = decode_job(job, scope, self.binding) if job is not None else None
        digest = "v1:" + E1.sha(resume_bytes(request, self.binding))
        key = E1.rkey("resume-erasure", scope, request["idempotency_key"])
        receipt, outcome_key = self.nodes.get(key), E1.okey(key)
        if MUTANT == "missing_job_before_receipt":
            need(job is not None, "MissingMemory")
        live = None
        if receipt is not None:
            live = self.decode_resume_receipt(receipt, key, scope, info)
        else:
            need(outcome_key not in self.nodes and outcome_key[:-4] + "0001" not in self.nodes,
                 "InvalidBundle")
        need(info is not None, "MissingMemory")
        need(request["job_key"] == E1.jkey(scope), "MissingMemory")
        deadline = request.get("deadline_ms")
        if MUTANT == "deadline_before_replay":
            need(deadline is None or now < deadline, "DeadlineExceeded")
        if live is not None and now < live["expiry"]:
            need(live["digest"] == digest, "IdempotencyConflict")
            parsed = live["parsed"]
            if MUTANT == "replay_current_job":
                return dict(self.erasure_job(scope), replayed=True)
            return {"replayed": True,
                    "job": self.job_view(scope, live["revision"], parsed["chunk_no"], parsed["state"],
                                         parsed["total"], parsed["remaining"]),
                    "chunk": {"memories_deleted": parsed["deleted"],
                              "remaining_after": parsed["remaining"]},
                    "receipt_revision": live["revision"], "commit_sequence": live["revision"]}
        need(info["state"] != "logical" or MUTANT == "resume_on_logical", "JobConflict")
        need(deadline is None or now < deadline, "DeadlineExceeded")
        if MUTANT != "ignore_generation":
            need(request["generation"] == 1, "JobConflict")
        if MUTANT != "ignore_expected_revision":
            need(request["expected_revision"] == info["revision"], "JobConflict")
        if MUTANT != "ignore_token":
            need(request["resume_token"] == token_of(scope, self.binding, info["revision"],
                                                     info["chunks"]), "JobConflict")
        chosen, window, known = self.plan(E1.skey(scope))
        need(not fault, "fixture:PreappendRefused")
        revision, deleted = self.sequence + 1, len(chosen)
        remaining = window - deleted if known else None
        logical = remaining == 0 or (MUTANT == "logical_when_unknown" and remaining is None)
        state = "logical" if logical else "pending"
        chunk_no, total = info["chunks"] + 1, info["deleted"] + deleted
        tags = []
        for memory in chosen:
            if memory["evidence"]:
                tags += ["Detach"] + ([] if MUTANT == "skip_evidence_delete" else ["VectorDelete"])
        if MUTANT != "edges_not_deleted":
            tags += ["DeleteEdge"] * sum(m["edges"] for m in chosen)
        tags += ["DeleteNode:memory"] * deleted
        replaced = "Upsert" if receipt is not None else "Create"
        tags += ["UpsertNode:job", replaced + "Node:outcome", replaced + "Node:receipt"]
        text = "erase-chunk:v1\n%s\n%d\n%d\n%d\n%s\n%s\n" % (
            E1.jkey(scope), chunk_no, deleted, total,
            "unknown" if remaining is None else remaining, state)
        common = {"e1_schema": "erase-chunk:v1", "scope": E1.skey(scope),
                  "e1_scope_tuple": E1.scope_bytes(scope).hex(), "e1_collection": self.binding or ""}
        job_props = dict(info["props"], e1_schema="erase-scope:v2", state=state,
                         deleted_count=str(total), e2_chunks=str(chunk_no),
                         remaining="unknown" if remaining is None else str(remaining))
        self.nodes[E1.jkey(scope)] = E1.make_node(E1.jkey(scope), "VliteMemoryErasureJob",
                                                  job_props, revision)
        self.nodes[outcome_key] = E1.make_node(outcome_key, "VliteMemoryReceiptOutcome", {
            "receipt_key": key, "index": 0, "outcome": text}, revision)
        self.nodes[key] = E1.make_node(key, "VliteMemoryReceipt", dict(
            common, digest=digest, expires_at_ms=str(now + request["receipt_window_ms"]),
            job_key=E1.jkey(scope), outcome_chunks=1), revision)
        self.sequence = revision
        self.memories = [m for m in self.memories if m not in chosen]
        self.gone += [m["id"] for m in chosen]
        self.records += [m["id"] for m in chosen if m["evidence"] and MUTANT != "skip_evidence_delete"]
        return {"replayed": False,
                "job": self.job_view(scope, revision, chunk_no, state, total, remaining),
                "chunk": {"memories_deleted": deleted, "remaining_after": remaining},
                "receipt_revision": revision, "commit_sequence": revision,
                "frame_runs": [[kind, len(list(group))] for kind, group in itertools.groupby(tags)],
                "ops": len(tags), "guards": deleted + 1, "digest": digest, "receipt_key": key,
                "outcome_key": outcome_key, "outcome": text,
                "deleted_ids": [m["id"] for m in chosen][:8]}

    def view(self):
        return {"sequence": self.sequence, "memories": [m["id"] for m in self.memories],
                "gone_count": len(self.gone), "deleted_records": list(self.records),
                "node_count": len(self.nodes)}


def fence_of(model, scope):
    job = model.nodes.get(E1.jkey(scope))
    try:
        info = decode_job(job, scope, model.binding)
        return info["revision"], info["chunks"]
    except (Refusal, TypeError, KeyError):
        return (job["revision"] if isinstance(job, dict) else 1), 0


def resolve(model, base, step, history):
    raw = copy.deepcopy(step.get("request", {}))
    if "$same_as" in raw:
        start = copy.deepcopy(history[raw.pop("$same_as")])
    else:
        start = copy.deepcopy(base)
    request = E1.merge(start, raw)
    try:
        scope = request["scope"]
        revision, chunks = fence_of(model, scope)
        if request.get("job_key") == "$derived":
            request["job_key"] = E1.jkey(scope)
        if request.get("expected_revision") == "$job":
            request["expected_revision"] = revision
        if request.get("resume_token") == "$job":
            request["resume_token"] = token_of(scope, model.binding, revision, chunks)
    except Refusal:
        pass
    return request


def corrupt(model, change, scope, begin_key):
    selector = change["node"]
    if selector == "job":
        key = E1.jkey(scope)
    elif selector == "begin-receipt":
        key = E1.rkey("begin-erasure", scope, begin_key)
    else:
        kind, _, idem = selector.partition(":")
        key = E1.rkey("resume-erasure", scope, idem)
        key = key if kind == "receipt" else E1.okey(key)
    if change["change"].get("remove"):
        del model.nodes[key]
    else:
        E1.edit(model.nodes[key], change["change"])


def execute(model, step, base, begin_base, history, index):
    if step.get("corrupt", {}).get("enabled"):
        corrupt(model, step["corrupt"], begin_base["scope"], begin_base["idempotency_key"])
    before = copy.deepcopy(model.__dict__)
    op = step.get("op", "resume")
    try:
        if op == "begin":
            request = E1.merge(begin_base, step.get("request", {}))
            return model.begin(request, step.get("fault", False))
        if op == "resume":
            request = resolve(model, base, step, history)
            history[index] = request
            return model.resume(request, step.get("fault", False))
        scope = E1.merge(base, step.get("request", {}))["scope"]
        if op == "job":
            return model.erasure_job(scope)
        if op == "status":
            return model.status(scope)
        if op == "inspect":
            return model.view()
        if op == "reopen":
            model.__dict__ = json.loads(json.dumps(model.__dict__, ensure_ascii=False))
            verify(model.__dict__ == before, "reopen changed logical state")
            return {"reopened": True}
        if op == "vectors":
            request = resolve(model, base, step, history)
            validate(request, model.binding)
            raw = resume_bytes(request, model.binding)
            return {"resume_hex": raw.hex(), "digest": "v1:" + E1.sha(raw),
                    "receipt_key": E1.rkey("resume-erasure", request["scope"], request["idempotency_key"]),
                    "token_job_r2_c0": token_of(request["scope"], model.binding, 2, 0)}
        raise AssertionError("unknown op " + op)
    except Refusal as error:
        verify(before == model.__dict__, "refusal changed state")
        return {"error": str(error)}


def project(actual, expected):
    if isinstance(expected, dict):
        return {key: project(actual[key], value) for key, value in expected.items()}
    return actual


def run_case(case, fixture, check=True):
    base, begin_base = fixture["base_resume"], fixture["base_request"]
    model = Model(case.get("setup", {}), begin_base["scope"])
    results, history = [], {}
    for index, step in enumerate(case["steps"]):
        try:
            actual = execute(model, step, base, begin_base, history, index)
            if check:
                verify(project(actual, step["expect"]) == step["expect"],
                       f"expected {step['expect']!r}, got {actual!r}")
        except (KeyError, AssertionError) as error:
            raise AssertionError(f"{case['name']} step {index}: {error}") from None
        results.append(actual)
    return results


def cases(fixture):
    group = fixture["groups"]["E2"]
    result = copy.deepcopy(group["cases"])
    for item in group.get("rejections", []):
        result.append({"name": item["name"], "setup": item.get("setup", {}),
                       "assumptions": ["spec: E2 structural validation with one repair."],
                       "steps": [{"op": "begin", "expect": {"replayed": False}},
                                 {"request": item["request"], "expect": {"error": item["error"]},
                                  "defect": dict(item["repair"], tag=item["name"])}]})
    for item in group.get("corruptions", []):
        op = item.get("op", "job" if item["node"] == "job" else "resume")
        probe = {"op": op} if op == "job" else {"request": {"$same_as": 1, "now_ms": 21}}
        result.append({
            "name": item["name"] + "-live",
            "assumptions": ["spec: strict reserved object decoding (section 4).",
                            "fixture: one raw corrupt toggle on a pending v2 job after chunk 1."],
            "setup": {"memories": [{"$memories": {"count": 2100}}]},
            "steps": [{"op": "begin", "expect": {"replayed": False}},
                      {"expect": {"job": {"chunks": 1, "remaining": 54}}},
                      dict(probe, corrupt={"enabled": True, "node": item["node"],
                                           "change": item["change"]},
                           expect={"error": item.get("error", "InvalidBundle")},
                           defect={"tag": item["name"], "path": ["steps", 2, "corrupt", "enabled"],
                                   "set": False})]})
    return result


def repairs(all_cases, fixture):
    count = 0
    for case in all_cases:
        for index, step in enumerate(case["steps"]):
            verify(("error" in step["expect"]) == ("defect" in step or "no_repair" in step),
                   "missing/extra defect: " + case["name"])
            if "defect" not in step:
                continue
            defect = step["defect"]
            verify(defect.keys() in ({"tag", "path", "set"}, {"tag", "path", "delete"}), "bad defect")
            fixed = copy.deepcopy(case)
            parent = E1.at(fixed, defect["path"][:-1])
            old = parent.get(defect["path"][-1]) if isinstance(parent, dict) else parent[defect["path"][-1]]
            verify(not isinstance(old, (dict, list)) or not old, "non-leaf repair")
            E1.edit(fixed, defect)
            fixed["steps"] = fixed["steps"][:index + 1]
            actual = run_case(fixed, fixture, check=False)
            verify("error" not in actual[-1], f"{case['name']} repair left {actual[-1]!r}")
            count += 1
    return count


def self_test(fixture):
    global MUTANT
    verify(fixture["schema"] == "fixture.memory-erasure-resume.v1", "fixture schema")
    all_cases = cases(fixture)
    names = set()
    for case in all_cases:
        verify(case["name"] not in names and case.get("assumptions"), "case identity")
        verify(all(a.startswith(("spec: ", "fixture: ", "assumption A")) for a in case["assumptions"]),
               "unlabelled assumption")
        names.add(case["name"])
        run_case(case, fixture)
    repaired = repairs(all_cases, fixture)
    killed = 0
    for mutation in fixture["mutations"]:
        MUTANT = mutation["name"]
        try:
            case = next(item for item in all_cases if item["name"] == mutation["case"])
            run_case(case, fixture)
        except AssertionError as error:
            verify(str(error).startswith(f"{case['name']} step {mutation['step']}:")
                   and mutation["reason"] in str(error), f"unintended mutation kill: {error}")
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
                run_case(case, fixture)
        finally:
            MUTANT = None
        equivalent += 1
    # E1 compatibility: this oracle's begin must produce E1's own digest and keys.
    base = fixture["base_request"]
    ours = Model({}, base["scope"]).begin(dict(base), False)
    theirs = E1.Model({}, base["scope"]).begin(dict(base), False)
    verify(ours["digest"] == theirs["digest"] and ours["receipt_key"] == theirs["receipt_key"]
           and ours["outcome_key"] == theirs["outcome_key"], "E1 begin vectors differ")
    print(f"PASS E2 (partial draft): {len(all_cases)} cases; "
          f"{sum(len(c['steps']) for c in all_cases)} steps; {repaired} single defects repaired; "
          f"{killed}/{len(fixture['mutations'])} intended mutations; "
          f"{equivalent} bounded-equivalent survivors")


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
            print(json.dumps(run_case(json.load(sys.stdin), fixture, check=False)))
    except (ValueError, KeyError, TypeError, AssertionError, OSError) as error:
        print("FAIL: " + str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
