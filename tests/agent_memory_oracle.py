"""Independent, bounded LOGICAL reference for Gap 1 slice A (sections 1/2/5/6/9).

No Rust parity, graph/frame physical bytes, durability, concurrency, ranking, facts,
temporal relations, consolidation, chunked erasure or backups are modeled. The
fixture-local length:value provenance encoding is counted against its 4,096-byte
stored-property limit; JSON UTF-8 size is only a logical body budget, not an engine
frame-size claim. IDs are caller supplied.
Fixture-only attach/edge operations seed external attachments and ordinary graph
edges; these are not relation memories or a proposed public API.

Fixture-local assumptions (not a Rust implementation/parity contract):
* JSON field/error names, projections and caller-supplied IDs are fixture APIs.
  Opaque strings, including keys and source IDs, are limited to 256 UTF-8 bytes.
* Scope framing uses presence bytes, four-byte big-endian lengths and SHA-256 v1;
  collisions are injected only by force_collision. Memory keys are (scope, ID).
  Evidence IDs name records in one implicit, pre-provisioned collection.
* A positive receipt_window in milliseconds is mandatory; no retention-policy
  object is modeled. Expired receipts are ignored/overwritten, not eagerly swept.
  Logical receipt IDs and commit sequences are monotonic counters, not Rust IDs.
* Digest v1 hashes canonical typed JSON (defaults, exact optional scope fields,
  float64 importance, positive zero); list order matters, map order does not.
  Deadlines are execution limits, excluded from the digest, not memory expiry.
* Evaluation order (an implementation detail, never asserted by goldens): request/
  scope, clock/window/deadline, coarse budgets, item/target scalars, receipt,
  scope-collision/stateful references, then ordered planning conflicts and computed
  operation budget. Every rejected golden step declares exactly one repairable
  defect (see verify_single_defect), so no golden depends on precedence between
  two defects. Failed plans discard all reclamation.
  A missing forget target is MissingMemory, not an idempotent successful delete.
  Bundles have at most 4,096 entries even if most would deduplicate. Captured/read
  clocks are nonnegative i64; expiry/window arithmetic is checked u64.
* Gap 1 does not define repeated caller IDs inside one remember bundle: this
  fixture rejects two items with the same (scope, memory_id) as DuplicateItem
  before any state change (fixture:duplicate-bundle-ids-reject). Distinct IDs may
  still exact-deduplicate to one memory. An empty bundle is InvalidBundle.
* Only entity/episodic nodes are modeled. Revisions start at one; a merge bumps
  once. A separate revision guard costs one operation for each changed existing
  node. Creation/update clocks are captured; observed_at must be caller data.
* Provenance is an ordered list capped at 64; default is the source alone.
  The fixture-local length:value/-; encoding is capped at 4,096 UTF-8 bytes.
  References are directed list entries (parallel entries count separately).
  Sharing uses references to the sole owner, never multiple attachments.
  Forward references resolve to retained caller IDs at the end of the plan.
  Forget retains evidence referenced through a retained memory's provenance
  reference; it leaves the record orphaned when deleting its owner.
* The fixture-only attach operation transfers an existing record to one external
  owner, retaining memory provenance; any such external ownership blocks forget.
  Fixture-only edge creates ordinary entity edges, with globally unique edge IDs.
* Logical plans charge receipt/node/reference operations, separate expiry/guard
  operations, record creation plus attachment, and forget edge/detach/record/node
  deletes. Remember charges physical outcome chunk nodes and UTF-8 content chunk
  nodes/HAS_CONTENT edges. Forget charges its outcome chunks and any content
  chunks/edges deleted in the same graph frame. This is the only operation model.
  Expiry reclamation costs no user operations. No frame-size claim.
  Expiry immediately removes incident links/ownership, but retains orphan records.
* State is a raw storage diagnostic (IDs can repeat across scopes); visible/get
  are clock-filtered projections. Evidence visibility requires a live sole owner
  in the exact requested scope, including when reached through references.
* Repeat/count is fixture compression for items, content and provenance only.

Golden inputs may use {"repeat": value, "count": n} in items, content or provenance
to exercise large bounds without a 16-MiB fixture; an optional "enumerate" list
names string fields of a repeated object that get the zero-based index appended.
Expected outputs never expand. Rejected remember/forget steps carry a "defect":
{"tag", "path", "set" | "delete": true, optional "step"} naming their one repair
(see apply_repair for the one-leaf edits allowed).
Run: python tests\\agent_memory_oracle.py --self-test
"""

import argparse
import copy
import hashlib
import itertools
import json
import math
from pathlib import Path
import random
import sys


FIXTURE = Path(__file__).parent / "fixtures" / "agent_memory" / "golden.json"
I64_MIN, I64_MAX, U64_MAX = -(2**63), 2**63 - 1, 2**64 - 1
MAX_OPS, MAX_BODY = 4096, 16 * 1024 * 1024
# Each semantic mutation names the golden (case, step) that must expose it and a
# fragment of the resulting mismatch, so a kill for an unrelated reason fails.
MUTATIONS = {
    "slash_scope": ("scope-framing", 0, "got {'same_encoding': True, 'same_key': True}"),
    "alias_collision": ("scope-collision-rejected", 1, "got {'actions': [['created', 'n', 1]]"),
    "wildcard_scope": ("shared-is-not-wildcard", 1, "got {'memories': ['m']"),
    "session_without_user": ("session-requires-user-first", 0,
                             "got {'actions': [['created', 'm', 1]]"),
    "future_allowed": ("future-observation", 0, "got {'actions': [['created', 'm', 1]]"),
    "importance_unbounded": ("importance-out-of-range-only", 0,
                             "got {'actions': [['created', 'm', 1]]"),
    "provenance_unbounded": ("provenance-cap", 0, "got {'actions': [['created', 'm', 1]]"),
    "provenance_bytes_unbounded": ("provenance-encoded-size-limit", 1,
                                   "got {'actions': [['created', 'over-limit', 1]]"),
    "ops_unbounded": ("bundle-entry-cap-before-dedup", 0,
                      "got {'actions': [['created', 'm0', 1], ['noop', 'm0', 1]"),
    "body_unbounded": ("body-budget", 0, "got {'error': 'OperationLimit'}"),
    "receipt_conflict_ignored": ("receipt-conflict", 1,
                                 "got {'actions': [['created', 'm', 1]], 'receipt': 1,"),
    "deadline_hashed": ("receipt-replay-no-ttl-extension", 1,
                        "got {'error': 'IdempotencyConflict'}"),
    "receipt_never_expires": ("receipt-expiry-exact-dedup", 1,
                              "got {'actions': [['created', 'm', 1]], 'receipt': 1,"),
    "dedup_ignores_source": ("independent-source-separate-memory", 0,
                             "got {'actions': [['created', 'm', 1], ['noop', 'm', 1]]"),
    "revision_ignored": ("explicit-merge-revision-guard", 3,
                         "got {'actions': [['merged', 'm', 3]]"),
    "foreign_detached": ("foreign-attachment-blocks-whole-forget", 2,
                         "got {'deleted': ['m', 'n']"),
    "shared_deleted": ("forget-keeps-shared-evidence", 3,
                       "got {'memories': ['n'], 'evidence': []"),
    "dangling_edges": ("entity-forget-incident-edges", 3,
                       "got {'memories': ['n'], 'evidence': [], 'edges': ['link']"),
    "expiry_inclusive": ("expiry-boundary-hides-not-deletes-evidence", 2,
                         "got {'memories': ['m'], 'evidence': ['e']"),
    "expiry_deletes_evidence": ("expiry-boundary-hides-not-deletes-evidence", 3,
                                "got {'memories': ['m'], 'evidence': []"),
    "cross_scope_allowed": ("cross-scope-evidence", 0, "got {'actions': [['created', 'm', 1]]"),
    "non_atomic": ("atomic-late-identity-conflict", 1, "error changed state"),
    "merge_unguarded": ("merge-must-have-expected-revision", 1,
                        "got {'actions': [['merged', 'm', 2]]"),
    "replay_reads_state": ("retry-after-reference-forgotten", 3,
                           "got {'error': 'MissingReference'}"),
    "expiry_not_reclaimed": ("expired-reference-is-missing", 1,
                             "got {'actions': [['created', 'n', 1]]"),
    "coownership_allowed": ("record-cannot-have-two-owners", 0,
                            "got {'actions': [['created', 'm', 1], ['created', 'n', 1]]"),
    "multiple_records_allowed": ("owner-cannot-have-two-records-in-collection", 0,
                                 "got {'actions': [['created', 'm', 1]]"),
    "receipt_op_free": ("candidate-create-budget-rejected", 0,
                        "got {'actions': [['created', '0', 1]"),
    "outcome_ops_free": ("candidate-create-budget-rejected", 0,
                          "got {'actions': [['created', '0', 1]"),
    "expiry_op_free": ("expiry-operation-budget", 0, "got {'actions': [['created', 'm', 1]]"),
    "guard_op_free": ("separate-merge-guard-budget", 1, "got {'actions': [['merged', 'm', 2]]"),
    "outbound_reference_free": ("forget-outbound-reference-over-budget", 3,
                                "got {'deleted': ['n']"),
    "dedup_alias_allowed": ("dedup-alias-m-q-r", 1,
                            "got {'actions': [['noop', 'm', 1], ['created', 'r', 1]]"),
    "receipt_window_default": ("missing-explicit-receipt-window", 0,
                               "got {'actions': [['created', 'm', 1]]"),
    "raw_request_digest": ("typed-digest-defaults", 1, "got {'error': 'IdempotencyConflict'}"),
    "global_memory_ids": ("cross-scope-references", 1, "got {'actions': [['created', 'n', 1]]"),
    "observed_at_default": ("missing-caller-observation", 0,
                            "got {'actions': [['created', 'm', 1]]"),
    "inbound_reference_free": ("forget-separate-guard-over-budget", 3, "got {'deleted': ['m']"),
    "incident_reference_double_count": ("forget-between-targets-counted-once", 2,
                                        "got {'error': 'OperationLimit'}"),
    "duplicate_bundle_ids_allowed": ("duplicate-bundle-id-rejected", 0,
                                     "got {'actions': [['created', 'm', 1], ['noop', 'm', 1]]"),
}


class ModelError(ValueError):
    pass


def check(condition, error):
    if not condition:
        raise ModelError(error)


def integer(value, low, high):
    return type(value) is int and low <= value <= high


def opaque(value):
    return isinstance(value, str) and 0 < len(value.encode("utf-8")) <= 256


def canonical(value, *, allow_nonfinite=False):
    return json.dumps(value, sort_keys=True, separators=(",", ":"),
                      ensure_ascii=False, allow_nan=allow_nonfinite).encode("utf-8")


def digest(value):
    return "v1:" + hashlib.sha256(canonical(value)).hexdigest()


def encoded_provenance_bytes(entries):
    total = 0
    for entry in entries:
        if not isinstance(entry, dict):
            continue
        for field in ("source", "kind", "uri", "transformation"):
            value = entry.get(field)
            if value is None:
                total += 2
            elif isinstance(value, str):
                length = len(value.encode("utf-8"))
                total += len(str(length)) + 1 + length + 1
    return total


def outcome_chunk_operations(actions):
    # Version-one packed actions: count:hex(two-bit action codes):noop entries.
    # Count bytes independently of Rust serialization, including canonical IDs.
    size = len(str(len(actions))) + 2 + 2 * ((len(actions) + 3) // 4)
    for index, (action, memory_id, revision) in enumerate(actions):
        if action == "noop":
            size += len(str(index)) + 1 + 2 * len(memory_id.encode("utf-8"))
            size += 1 + len(str(revision)) + 1
    return (size + 4095) // 4096


def forget_outcome_chunk_operations(ids):
    ordered = sorted(ids)
    size = len("deleted:")
    size += sum(2 * len(memory_id.encode("utf-8")) for memory_id in ordered)
    size += max(0, len(ordered) - 1)
    return (size + 4095) // 4096


def content_chunk_operations(content):
    if len(content.encode("utf-8")) <= 4096:
        return 0
    chunks, current = 1, 0
    for character in content:
        size = len(character.encode("utf-8"))
        if current + size > 4096:
            chunks += 1
            current = 0
        current += size
    return 2 * chunks  # One node and one HAS_CONTENT edge per UTF-8 chunk.


def scope_tuple(value, mutation=None):
    check(isinstance(value, dict) and {"tenant", "agent"} <= value.keys()
          and value.keys() <= {"tenant", "agent", "user", "session"}, "InvalidScope")
    result = tuple(value.get(k) for k in ("tenant", "agent", "user", "session"))
    check(all(opaque(v) for v in result[:2])
          and all(v is None or opaque(v) for v in result[2:]), "InvalidScope")
    check(result[3] is None or result[2] is not None
          or mutation == "session_without_user", "InvalidScope")
    return result


def frame_scope(scope, mutation=None):
    if mutation == "slash_scope":
        return "/".join(v or "" for v in scope).encode("utf-8")
    result = bytearray(b"scope:v1")
    for value in scope:
        if value is None:
            result.extend(b"\x00")
        else:
            encoded = value.encode("utf-8")
            result.extend(b"\x01" + len(encoded).to_bytes(4, "big") + encoded)
    return bytes(result)


class Model:
    def __init__(self, mutation=None, force_collision=False):
        self.mutation = mutation
        self.force_collision = force_collision
        self.scopes = {}
        self.memories = {}
        self.evidence = {}
        self.edges = {}
        self.receipts = {}
        self.sequence = 0

    def is_mutant(self, name):
        return self.mutation == name

    def scope_key(self, scope):
        payload = frame_scope(scope, self.mutation)
        return "scope:v1:" + ("forced" if self.force_collision else
                             hashlib.sha256(payload).hexdigest())

    def verify_scope(self, scope):
        key = self.scope_key(scope)
        check(key not in self.scopes or self.scopes[key] == scope
              or self.is_mutant("alias_collision"), "DigestCollision")
        return key

    def same_scope(self, left, right):
        if self.is_mutant("wildcard_scope"):
            return left[:2] == right[:2]
        return left == right

    def visible(self, memory, clock):
        expiry = memory["expiry"]
        return expiry is None or (clock <= expiry if self.is_mutant("expiry_inclusive")
                                  else clock < expiry)

    def validate_request(self, operation, request, clock):
        check(isinstance(request, dict), "InvalidRequest")
        required = {"scope", "key", "items" if operation == "remember" else "targets"}
        allowed = required | {"deadline", "receipt_window"}
        if operation == "remember":
            allowed |= {"allow_future"}
        check(required <= request.keys() and request.keys() <= allowed, "InvalidRequest")
        scope = scope_tuple(request["scope"], self.mutation)
        check(opaque(request["key"]), "InvalidKey")
        check(integer(clock, 0, I64_MAX), "InvalidClock")
        window = request.get("receipt_window", 100 if self.is_mutant("receipt_window_default")
                             else None)
        check(integer(window, 1, U64_MAX) and clock + window <= U64_MAX,
              "InvalidReceiptWindow")
        deadline = request.get("deadline")
        check(deadline is None or integer(deadline, 0, U64_MAX), "InvalidDeadline")
        check(deadline is None or clock < deadline, "DeadlineExceeded")
        items = request["items" if operation == "remember" else "targets"]
        check(isinstance(items, list) and len(items) > 0, "InvalidBundle")
        check(len(items) <= MAX_OPS or self.is_mutant("ops_unbounded"), "OperationLimit")
        # Size invalid scalar inputs too; scalar validation supplies the named error.
        check(len(canonical(request, allow_nonfinite=True)) <= MAX_BODY
              or self.is_mutant("body_unbounded"),
              "BodyLimit")
        return scope, window, items

    def validate_items(self, scope, items, request, clock):
        check(type(request.get("allow_future", False)) is bool, "InvalidPolicy")
        normalized = []
        for item in items:
            check(isinstance(item, dict) and {"id", "kind", "content", "source"} <= item.keys()
                  and item.keys() <= {"id", "kind", "content", "source", "importance",
                                      "observed_at", "expiry", "provenance", "references",
                                      "evidence", "merge_into", "expected_revision"},
                  "InvalidItem")
            check(opaque(item["id"]) and opaque(item["source"]), "InvalidIdentity")
            check(item["kind"] in ("episodic", "entity"), "UnsupportedKind")
            check(isinstance(item["content"], str), "InvalidContent")
            importance = item.get("importance", 0.5)
            check(type(importance) in (int, float)
                  and (type(importance) is int or math.isfinite(importance))
                  and (0 <= importance <= 1 or self.is_mutant("importance_unbounded")),
                  "InvalidImportance")
            observed = item.get("observed_at", clock if self.is_mutant("observed_at_default")
                                else None)
            check(integer(observed, I64_MIN, I64_MAX), "InvalidObservation")
            check(observed <= clock or request.get("allow_future", False)
                  or self.is_mutant("future_allowed"), "FutureObservation")
            expiry = item.get("expiry")
            check(expiry is None or integer(expiry, 0, U64_MAX), "InvalidExpiry")
            provenance = item.get("provenance", [{"source": item["source"]}])
            check(isinstance(provenance, list), "InvalidProvenance")
            check(len(provenance) <= 64 or self.is_mutant("provenance_unbounded"),
                  "ProvenanceLimit")
            check(encoded_provenance_bytes(provenance) <= 4096
                  or self.is_mutant("provenance_bytes_unbounded"), "ProvenanceLimit")
            for entry in provenance:
                check(isinstance(entry, dict) and {"source"} <= entry.keys()
                      and entry.keys() <= {"source", "kind", "uri", "transformation"}
                      and opaque(entry["source"])
                      and all(isinstance(v, str) for v in entry.values()), "InvalidProvenance")
            refs, evidence = item.get("references", []), item.get("evidence", [])
            check(isinstance(refs, list) and all(opaque(v) for v in refs), "InvalidReference")
            check(isinstance(evidence, list), "InvalidEvidence")
            for record in evidence:
                check(isinstance(record, dict) and set(record) == {"id", "scope"}
                      and opaque(record["id"]), "InvalidEvidence")
                evidence_scope = scope_tuple(record["scope"], self.mutation)
                check(evidence_scope == scope or self.is_mutant("cross_scope_allowed"),
                      "CrossScope")
            if "merge_into" in item:
                check(opaque(item["merge_into"]), "InvalidIdentity")
                check(integer(item.get("expected_revision"), 1, U64_MAX)
                      or self.is_mutant("merge_unguarded"), "ExpectedRevisionRequired")
            elif "expected_revision" in item:
                raise ModelError("InvalidItem")
            normalized.append({
                **copy.deepcopy(item), "importance": float(importance) or 0.0,
                "observed_at": observed,
                "expiry": expiry, "provenance": copy.deepcopy(provenance),
                "references": refs, "evidence": evidence,
            })
        # Fixture-local (Gap 1 is silent): one bundle may not repeat a caller ID.
        check(len({item["id"] for item in normalized}) == len(normalized)
              or self.is_mutant("duplicate_bundle_ids_allowed"), "DuplicateItem")
        return normalized

    def memory_key(self, scope, memory_id):
        return memory_id if self.is_mutant("global_memory_ids") else (scope, memory_id)

    def reclaim(self, clock):
        if self.is_mutant("expiry_not_reclaimed"):
            return
        due = {key for key, memory in self.memories.items()
               if not self.visible(memory, clock)}
        for key in due:
            del self.memories[key]
        self.edges = {key: pair for key, pair in self.edges.items()
                      if not due.intersection(pair)}
        for memory in self.memories.values():
            memory["references"] = [ref for ref in memory["references"] if ref not in due]
        for record in self.evidence.values():
            record["owners"].difference_update(due)

    def validate_references(self, scope, items):
        local = {self.memory_key(scope, item["id"]) for item in items}
        for item in items:
            for ref in item["references"]:
                check(self.memory_key(scope, ref) in self.memories
                      or self.memory_key(scope, ref) in local, "MissingReference")
            for record in item["evidence"]:
                existing = self.evidence.get(record["id"])
                check(existing is None or existing["scope"] == scope
                      or self.is_mutant("cross_scope_allowed"), "CrossScope")

    def typed_request(self, request, scope, items=None):
        result = copy.deepcopy(request)
        result["scope"] = dict(zip(("tenant", "agent", "user", "session"), scope))
        if items is not None:
            result["items"] = copy.deepcopy(items)
            result["allow_future"] = request.get("allow_future", False)
            for item in result["items"]:
                for record in item["evidence"]:
                    record["scope"] = dict(zip(
                        ("tenant", "agent", "user", "session"),
                        scope_tuple(record["scope"], self.mutation)))
        return result

    def receipt(self, operation, scope, request, clock):
        payload = copy.deepcopy(request)
        if not self.is_mutant("deadline_hashed"):
            payload.pop("deadline", None)
        request_digest = digest({"version": 1, "operation": operation, "request": payload})
        identity = (operation, scope, request["key"])
        previous = self.receipts.get(identity)
        if previous and (clock < previous["expiry"] or self.is_mutant("receipt_never_expires")):
            check(previous["digest"] == request_digest
                  or self.is_mutant("receipt_conflict_ignored"), "IdempotencyConflict")
            return identity, request_digest, copy.deepcopy(previous["outcome"])
        return identity, request_digest, None

    def remember(self, request, clock, fixture_pair=None):
        scope, window, items = self.validate_request("remember", request, clock)
        normalized = self.validate_items(scope, items, request, clock)
        if self.is_mutant("replay_reads_state"):
            self.validate_references(scope, normalized)
        payload = request if self.is_mutant("raw_request_digest") else self.typed_request(
            request, scope, normalized)
        identity, request_digest, replay = self.receipt("remember", scope, payload, clock)
        if replay is not None:
            return replay
        self.verify_scope(scope)
        self.reclaim(clock)
        self.validate_references(scope, normalized)
        planned_ops = 0 if self.is_mutant("receipt_op_free") else 1
        actions = []
        for item in normalized:
            target = self.memories.get(self.memory_key(scope, item.get("merge_into")))
            if "merge_into" in item:
                check(target is not None, "MissingMemory")
            content_digest = digest(item["content"])
            exact = next((m for m in self.memories.values()
                          if m["scope"] == scope and m["kind"] == item["kind"]
                          and m["content_digest"] == content_digest
                          and (item["source"] in m["sources"]
                               or self.is_mutant("dedup_ignores_source"))), None)
            if exact:
                caller = self.memories.get(self.memory_key(scope, item["id"]))
                check(caller is None or caller is exact
                      or self.is_mutant("dedup_alias_allowed"), "IdentityConflict")
                actions.append(["noop", exact["id"], exact["revision"]])
                continue
            if "merge_into" in item:
                check(target["kind"] == item["kind"] and
                      target["content_digest"] == content_digest, "MergeIdentityConflict")
                check(target["revision"] == item.get("expected_revision")
                      or self.is_mutant("revision_ignored") or self.is_mutant("merge_unguarded"),
                      "RevisionConflict")
                combined_provenance = target["provenance"] + item["provenance"]
                check(len(combined_provenance) <= 64
                      or self.is_mutant("provenance_unbounded"), "ProvenanceLimit")
                check(encoded_provenance_bytes(combined_provenance) <= 4096
                      or self.is_mutant("provenance_bytes_unbounded"), "ProvenanceLimit")
                target["provenance"].extend(copy.deepcopy(item["provenance"]))
                target["sources"].add(item["source"])
                target["revision"] += 1
                target["updated_at"] = clock
                memory = target
                action = "merged"
                planned_ops += 1 + (not self.is_mutant("guard_op_free"))
            else:
                check(self.memory_key(scope, item["id"]) not in self.memories,
                      "IdentityConflict")
                memory = {
                    "id": item["id"], "scope": scope, "kind": item["kind"],
                    "content": item["content"], "content_digest": content_digest,
                    "sources": {item["source"]}, "revision": 1,
                    "importance": item["importance"], "created_at": clock,
                    "updated_at": clock, "observed_at": item["observed_at"],
                    "expiry": item["expiry"], "provenance": copy.deepcopy(item["provenance"]),
                    "references": [], "evidence": set(),
                }
                self.memories[self.memory_key(scope, item["id"])] = memory
                action = "created"
                planned_ops += 1
                planned_ops += content_chunk_operations(item["content"])
                if item["expiry"] is not None and not self.is_mutant("expiry_op_free"):
                    planned_ops += 1
                if item["expiry"] is not None and item["expiry"] <= clock:
                    self.reclaim(clock)
            memory_key = self.memory_key(scope, memory["id"])
            check(not (item["references"] or item["evidence"])
                  or memory_key in self.memories, "MissingMemory")
            memory["references"].extend(self.memory_key(scope, ref)
                                        for ref in item["references"])
            planned_ops += len(item["references"])
            for record in item["evidence"]:
                existing = self.evidence.get(record["id"])
                check(existing is None or (
                    not existing["foreign"] and
                    (not existing["owners"] or existing["owners"] == {memory_key}))
                    or self.is_mutant("coownership_allowed"), "RecordAlreadyAttached")
                attached = {eid for eid, value in self.evidence.items()
                            if memory_key in value["owners"]}
                check(not attached or attached == {record["id"]}
                      or self.is_mutant("multiple_records_allowed"), "RecordAlreadyAttached")
                evidence = self.evidence.setdefault(record["id"], {
                    "scope": scope, "owners": set(), "foreign": set()})
                planned_ops += (2 if existing is None else
                                int(memory_key not in evidence["owners"]))
                evidence["owners"].add(memory_key)
                memory["evidence"].add(record["id"])
            actions.append([action, memory["id"], memory["revision"]])
        # A provisional ID that deduplicated to another ID is not a new anchor.
        check(all(ref in self.memories for memory in self.memories.values()
                  for ref in memory["references"]), "MissingReference")
        if not self.is_mutant("outcome_ops_free"):
            planned_ops += outcome_chunk_operations(actions)
        check(planned_ops <= MAX_OPS or self.is_mutant("ops_unbounded"), "OperationLimit")
        self.scopes[self.scope_key(scope)] = scope
        return self.commit(identity, request_digest, clock + window, {"actions": actions})

    def forget(self, request, clock):
        scope, window, targets = self.validate_request("forget", request, clock)
        ids = []
        for target in targets:
            check(isinstance(target, dict) and set(target) == {"id", "revision"}
                  and opaque(target["id"]) and integer(target["revision"], 1, U64_MAX),
                  "InvalidTarget")
            check(target["id"] not in ids, "DuplicateTarget")
            ids.append(target["id"])
        payload = request if self.is_mutant("raw_request_digest") else self.typed_request(
            request, scope)
        identity, request_digest, replay = self.receipt("forget", scope, payload, clock)
        if replay is not None:
            return replay
        self.verify_scope(scope)
        self.reclaim(clock)
        keys = {self.memory_key(scope, memory_id) for memory_id in ids}
        for target in targets:
            memory = self.memories.get(self.memory_key(scope, target["id"]))
            check(memory is not None, "MissingMemory")
            check(memory["revision"] == target["revision"]
                  or self.is_mutant("revision_ignored"), "RevisionConflict")
            for evidence_id in memory["evidence"]:
                check(not self.evidence[evidence_id]["foreign"]
                      or self.is_mutant("foreign_detached"), "ForeignAttachment")
        affected_evidence = {e for key in keys for e in self.memories[key]["evidence"]}
        retained_evidence = self.referenced_evidence(set(self.memories) - keys)
        deleted_evidence = affected_evidence - retained_evidence
        planned_ops = ((not self.is_mutant("receipt_op_free")) + len(keys)
                       + (0 if self.is_mutant("guard_op_free") else len(keys))
                       + sum(bool(self.evidence[e]["owners"] & keys)
                             for e in affected_evidence)
                       + len(deleted_evidence)
                       + sum(a in keys or b in keys for a, b in self.edges.values())
                       + sum((ref in keys and not self.is_mutant("inbound_reference_free"))
                                 or (key in keys and
                                     not self.is_mutant("outbound_reference_free"))
                             for key, m in self.memories.items() for ref in m["references"])
                       + (sum(key in keys and ref in keys
                              for key, m in self.memories.items()
                              for ref in m["references"])
                          if self.is_mutant("incident_reference_double_count") else 0))
        if not self.is_mutant("outcome_ops_free"):
            planned_ops += forget_outcome_chunk_operations(ids)
        if not self.is_mutant("content_delete_ops_free"):
            planned_ops += sum(
                content_chunk_operations(self.memories[key]["content"])
                for key in keys)
        check(planned_ops <= MAX_OPS or self.is_mutant("ops_unbounded"), "OperationLimit")
        for memory_key in keys:
            memory = self.memories.pop(memory_key)
            for evidence_id in memory["evidence"]:
                evidence = self.evidence[evidence_id]
                evidence["owners"].discard(memory_key)
                if evidence_id in deleted_evidence or self.is_mutant("shared_deleted"):
                    del self.evidence[evidence_id]
            if not self.is_mutant("dangling_edges"):
                self.edges = {k: v for k, v in self.edges.items() if memory_key not in v}
            for retained in self.memories.values():
                retained["references"] = [ref for ref in retained["references"]
                                           if ref != memory_key]
        return self.commit(identity, request_digest, clock + window, {"deleted": sorted(ids)})

    def referenced_evidence(self, roots):
        """Resolve provenance through owners even if that owner is being forgotten."""
        visited, evidence = set(), set()
        pending = list(roots)
        while pending:
            key = pending.pop()
            if key in visited or key not in self.memories:
                continue
            visited.add(key)
            memory = self.memories[key]
            evidence.update(memory["evidence"])
            pending.extend(memory["references"])
        return evidence

    def commit(self, identity, request_digest, expiry, outcome):
        self.sequence += 1
        outcome = {**outcome, "receipt": self.sequence, "sequence": self.sequence}
        self.receipts[identity] = {
            "digest": request_digest, "expiry": expiry, "outcome": copy.deepcopy(outcome)}
        return outcome

    def mutate(self, operation, request, clock, fixture_pair=None):
        # A detached planner is the logical atomic boundary, not storage simulation.
        planned = self if self.is_mutant("non_atomic") else copy.deepcopy(self)
        outcome = (planned.remember(request, clock, fixture_pair)
                   if operation == "remember" else planned.forget(request, clock))
        self.__dict__.update(planned.__dict__)
        return outcome

    def execute(self, step, fixture_pair=None):
        op = step["op"]
        if op in ("remember", "forget"):
            return self.mutate(op, expand_request(step["request"]), step["clock"], fixture_pair)
        if op == "scope_pair":
            left = scope_tuple(step["left"], self.mutation)
            right = scope_tuple(step["right"], self.mutation)
            return {"same_encoding": frame_scope(left, self.mutation) ==
                    frame_scope(right, self.mutation),
                    "same_key": self.scope_key(left) == self.scope_key(right)}
        if op == "state":
            return {"memories": sorted(m["id"] for m in self.memories.values()),
                    "evidence": sorted(self.evidence),
                    "edges": sorted(self.edges), "receipts": len(self.receipts),
                    "sequence": self.sequence}
        if op == "attach":
            check(step["evidence"] in self.evidence, "MissingEvidence")
            self.evidence[step["evidence"]]["owners"].clear()
            self.evidence[step["evidence"]]["foreign"] = {step["owner"]}
            return {"attached": step["evidence"]}
        if op == "edge":
            matches = [[key for key, memory in self.memories.items()
                        if memory["id"] == step[k]] for k in ("from", "to")]
            check(all(len(keys) == 1 for keys in matches), "MissingMemory")
            left_key, right_key = (keys[0] for keys in matches)
            left, right = self.memories[left_key], self.memories[right_key]
            check(left is not None and right is not None, "MissingMemory")
            check(left["kind"] == right["kind"] == "entity", "InvalidEdge")
            check(left["scope"] == right["scope"], "CrossScope")
            check(step["id"] not in self.edges, "IdentityConflict")
            self.edges[step["id"]] = (left_key, right_key)
            return {"edge": step["id"]}
        scope = scope_tuple(step["scope"], self.mutation)
        self.verify_scope(scope)
        clock = step["clock"]
        check(integer(clock, 0, I64_MAX), "InvalidClock")
        visible = {k: m for k, m in self.memories.items()
                   if self.same_scope(m["scope"], scope) and self.visible(m, clock)}
        if self.is_mutant("expiry_deletes_evidence"):
            for m in self.memories.values():
                if not self.visible(m, clock):
                    for evidence_id in m["evidence"]:
                        self.evidence.pop(evidence_id, None)
        if op == "visible":
            return {"memories": sorted(m["id"] for m in visible.values()),
                    "evidence": sorted(e for e, record in self.evidence.items()
                                       if record["owners"] & visible.keys()),
                    "edges": sorted(k for k, (a, b) in self.edges.items()
                                    if a in visible and b in visible)}
        key = self.memory_key(scope, step["id"])
        check(key in visible, "MissingMemory")
        memory = visible[key]
        projected = {**memory, "evidence": sorted(memory["evidence"]),
                     "provenance_count": len(memory["provenance"])}
        return {key: projected[key] for key in step["fields"]}


def expand(value):
    if (isinstance(value, dict) and {"repeat", "count"} <= value.keys()
            and value.keys() <= {"repeat", "count", "enumerate"}):
        check(integer(value["count"], 0, MAX_BODY + 1), "InvalidExpansion")
        element = value["repeat"]
        fields = value.get("enumerate", [])
        check(isinstance(fields, list) and all(isinstance(field, str) for field in fields)
              and len(set(fields)) == len(fields)
              and (not fields or isinstance(element, dict)
                   and all(isinstance(element.get(field), str) for field in fields)),
              "InvalidExpansion")
        if isinstance(element, str):
            return element * value["count"]
        result = [copy.deepcopy(element) for _ in range(value["count"])]
        for index, entry in enumerate(result):
            for field in fields:
                entry[field] += str(index)
        return result
    return copy.deepcopy(value)


def expand_request(request):
    result = copy.deepcopy(request)
    if isinstance(result, dict) and "items" in result:
        result["items"] = expand(result["items"])
        if isinstance(result["items"], list):
            for item in result["items"]:
                if isinstance(item, dict):
                    for field in ("content", "provenance"):
                        if field in item:
                            item[field] = expand(item[field])
    return result


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate JSON key")
        result[key] = value
    return result


def reject_constant(value):
    raise ValueError("nonfinite JSON constant: " + value)


def finite_float(value):
    result = float(value)
    if not math.isfinite(result):
        raise ValueError("nonfinite JSON float")
    return result


def parse_fixture(text):
    result = json.loads(text, object_pairs_hook=unique_object,
                        parse_constant=reject_constant, parse_float=finite_float)
    validate_fixture(result)
    return result


SCALARS = (type(None), bool, int, float, str)


def apply_repair(step, defect):
    """Return a copy of step with one leaf edit: set an absent key, a scalar or an
    empty container; delete a scalar, or a list entry identical to a retained
    sibling except for its "id". Larger edits could hide extra defects."""
    def require(condition):
        if not condition:
            raise ValueError("malformed agent-memory fixture defect")

    def twin(left, right):
        return (isinstance(right, dict) and "id" in right
                and canonical({k: v for k, v in left.items() if k != "id"})
                == canonical({k: v for k, v in right.items() if k != "id"}))
    result = copy.deepcopy(step)
    parent, path = result, defect["path"]
    for key in path[:-1]:
        require(isinstance(parent, dict) and isinstance(key, str) and key in parent
                or isinstance(parent, list) and type(key) is int and key < len(parent))
        parent = parent[key]
    leaf = path[-1]
    if isinstance(parent, dict):
        require(isinstance(leaf, str))
        present = leaf in parent
    else:
        require(isinstance(parent, list) and type(leaf) is int and leaf < len(parent))
        present = True
    old = parent[leaf] if present else None
    if "delete" in defect:
        require(present and (isinstance(old, SCALARS) or isinstance(parent, list)
                             and isinstance(old, dict) and "id" in old
                             and any(twin(old, other) for position, other
                                     in enumerate(parent) if position != leaf)))
        del parent[leaf]
    else:
        require(not present or isinstance(old, SCALARS) or old in ([], {}))
        require(not present or canonical(old) != canonical(defect["set"]))
        parent[leaf] = copy.deepcopy(defect["set"])
    return result


def validate_defect(steps, index):
    def require(condition):
        if not condition:
            raise ValueError("malformed agent-memory fixture defect")
    defect = steps[index]["defect"]
    require(isinstance(defect, dict) and {"tag", "path"} <= defect.keys()
            and defect.keys() <= {"tag", "path", "set", "delete", "step"}
            and ("set" in defect) != ("delete" in defect)
            and defect.get("delete", True) is True and opaque(defect["tag"]))
    path = defect["path"]
    require(isinstance(path, list) and (path == ["clock"] or len(path) >= 2
                                        and path[0] == "request")
            and all(isinstance(key, str) or type(key) is int and key >= 0 for key in path))
    target = defect.get("step", index)
    require(type(target) is int and 0 <= target <= index)
    require(steps[target]["op"] in ("remember", "forget")
            and (target == index or "error" not in steps[target]["expect"]))
    apply_repair(steps[target], defect)


def validate_fixture(fixture):
    def require(condition):
        if not condition:
            raise ValueError("malformed agent-memory fixture")
    require(isinstance(fixture, dict) and set(fixture) == {"schema", "cases"})
    require(fixture["schema"] == "vlite.agent-memory-golden.v1")
    require(isinstance(fixture["cases"], list) and len(fixture["cases"]) >= 20)
    names = set()
    shapes = {
        "remember": {"op", "request", "clock", "expect"},
        "forget": {"op", "request", "clock", "expect"},
        "visible": {"op", "scope", "clock", "expect"},
        "get": {"op", "scope", "clock", "id", "fields", "expect"},
        "state": {"op", "expect"},
        "scope_pair": {"op", "left", "right", "expect"},
        "attach": {"op", "evidence", "owner", "expect"},
        "edge": {"op", "id", "from", "to", "expect"},
    }
    projection = {"id", "kind", "revision", "importance", "created_at", "updated_at",
                  "observed_at", "expiry", "evidence", "provenance_count", "content"}
    for case in fixture["cases"]:
        require(isinstance(case, dict) and {"name", "steps", "assumptions"} <= case.keys()
                and case.keys() <= {"name", "steps", "force_collision", "assumptions"})
        require(isinstance(case["assumptions"], list) and bool(case["assumptions"])
                and all(isinstance(value, str) and value.startswith(("spec: ", "fixture: "))
                        for value in case["assumptions"]))
        require(opaque(case["name"]) and case["name"] not in names)
        names.add(case["name"])
        require(type(case.get("force_collision", False)) is bool)
        require(isinstance(case["steps"], list) and len(case["steps"]) > 0)
        for index, step in enumerate(case["steps"]):
            require(isinstance(step, dict) and isinstance(step.get("op"), str)
                    and step["op"] in shapes)
            require(set(step) - {"defect"} == shapes[step["op"]])
            require(isinstance(step["expect"], dict) and bool(step["expect"]))
            # Every rejection must name its single defect; accepted steps name none.
            require(("error" in step["expect"]) == ("defect" in step))
            if "error" in step["expect"]:
                require(set(step["expect"]) == {"error"}
                        and opaque(step["expect"]["error"])
                        and step["op"] in ("remember", "forget"))
            if "clock" in step:
                require(type(step["clock"]) is int)
            if step["op"] in ("remember", "forget"):
                require(isinstance(step["request"], dict))
            if step["op"] == "get":
                require(isinstance(step["fields"], list) and len(step["fields"]) > 0
                        and len(set(step["fields"])) == len(step["fields"])
                        and all(field in projection for field in step["fields"]))
            if "defect" in step:
                validate_defect(case["steps"], index)
        # Rejections Gap 1 does not define must be labelled as fixture assumptions.
        errors = {step["expect"].get("error") for step in case["steps"]}
        for error, label in (("DuplicateItem", "fixture: duplicate-bundle-ids-reject"),
                             ("InvalidBundle", "fixture: empty-bundle-rejects")):
            require(error not in errors
                    or any(value.startswith(label) for value in case["assumptions"]))


def run_case(case, mutation=None):
    model = Model(mutation, case.get("force_collision", False))
    for index, step in enumerate(case["steps"]):
        before = copy.deepcopy(model.__dict__)
        try:
            actual = model.execute(step, (case["name"], index))
        except ModelError as error:
            actual = {"error": str(error)}
            if model.__dict__ != before:
                raise AssertionError(f"{case['name']} step {index}: error changed state")
        if actual != step["expect"]:
            raise AssertionError(f"{case['name']} step {index}: "
                                 f"expected {step['expect']!r}, got {actual!r}")
    return model


def verify_single_defect(case):
    """Enforce that every rejected golden step contains exactly one defect.

    Each rejected step declares one defect whose one-leaf repair (apply_repair) is
    applied to that step, or to an earlier accepted setup step. The case prefix is
    replayed on a fresh model: earlier steps must still match their goldens, steps
    in between keep their accepted/rejected status, and the repaired and rejected
    steps must both be accepted. Hence the golden error is caused by the declared
    defect alone, and any second defect keeps the step rejected and fails here.
    """
    count = 0
    for index, step in enumerate(case["steps"]):
        if "defect" not in step:
            continue
        defect = step["defect"]
        target = defect.get("step", index)
        steps = copy.deepcopy(case["steps"][:index + 1])
        steps[target] = apply_repair(steps[target], defect)
        model = Model(None, case.get("force_collision", False))
        for position, candidate in enumerate(steps):
            try:
                actual = model.execute(candidate, (case["name"], position))
            except ModelError as error:
                actual = {"error": str(error)}
            if position < target:
                matches = actual == candidate["expect"]
            elif position in (target, index):
                matches = "error" not in actual
            else:
                matches = ("error" in actual) == ("error" in candidate["expect"])
            if not matches:
                raise AssertionError(
                    f"{case['name']} step {index}: repairing defect {defect['tag']!r} "
                    f"left step {position} with {actual!r}")
        count += 1
    return count


# Orthogonal defects injected into each rejected step: (op or any, path, value, error).
SECOND_DEFECTS = (
    (None, ["request", "unexpected"], True, "InvalidRequest"),
    (None, ["request", "receipt_window"], 0, "InvalidReceiptWindow"),
    ("remember", ["request", "items", 0, "importance"], 2, "InvalidImportance"),
    ("forget", ["request", "targets", 0, "revision"], 0, "InvalidTarget"),
)


def second_defect_checks(fixture):
    """Every rejected step with an injected second defect must fail verification
    with exactly the injected error once its declared defect is repaired."""
    count = 0
    for case in fixture["cases"]:
        for index, step in enumerate(case["steps"]):
            if "defect" not in step:
                continue
            repair, detected = step["defect"]["path"], 0
            same_step = step["defect"].get("step", index) == index
            for op, path, value, error in SECOND_DEFECTS:
                shared = min(len(path), len(repair))
                if (op not in (None, step["op"])
                        or same_step and path[:shared] == repair[:shared]):
                    continue
                try:
                    injected = apply_repair(step, {"tag": "second", "path": path,
                                                   "set": value})
                    if same_step:
                        apply_repair(injected, step["defect"])
                except ValueError:
                    continue  # no such injection point, or it disturbs the declared repair
                candidate = {**case, "steps": case["steps"][:index] + [injected]}
                try:
                    verify_single_defect(candidate)
                except AssertionError as failure:
                    if (str(failure).startswith(f"{case['name']} step {index}: ")
                            and str(failure).endswith(repr({"error": error}))):
                        detected += 1
                        continue
                    raise AssertionError(f"second defect at {path!r} was detected for an "
                                         f"unintended reason: {failure}") from None
                raise AssertionError(f"{case['name']} step {index}: second defect at "
                                     f"{path!r} was not detected")
            if not detected:
                raise AssertionError(f"{case['name']} step {index}: no second defect "
                                     "could be injected")
            count += detected
    return count


def malformed_checks(fixture):
    invalid = ["{}", "[]", '{"schema":1,"schema":2,"cases":[]}',
               '{"schema":"vlite.agent-memory-golden.v1","cases":NaN}',
               '{"schema":"vlite.agent-memory-golden.v1","cases":1e999}']
    changes = [
        lambda x: x.update(schema="unknown"),
        lambda x: x.update(extra=True),
        lambda x: x.update(cases=[]),
        lambda x: x["cases"].append(copy.deepcopy(x["cases"][0])),
        lambda x: x["cases"][0].update(steps=[]),
        lambda x: x["cases"][0].update(force_collision=1),
        lambda x: x["cases"][0].pop("assumptions"),
        lambda x: x["cases"][0].update(assumptions="not a list"),
        lambda x: x["cases"][0].update(assumptions=[]),
        lambda x: x["cases"][0].update(assumptions=[1]),
        lambda x: x["cases"][0].update(assumptions=["unclassified"]),
        lambda x: x["cases"][0]["steps"][0].update(op="unknown"),
        lambda x: x["cases"][0]["steps"][0].update(extra=True),
        lambda x: x["cases"][0]["steps"][0].update(expect={}),
        lambda x: x["cases"][0]["steps"][0].update(expect={"error": 1}),
    ]

    def rejected(x):
        return next(step for case in x["cases"] for step in case["steps"]
                    if "defect" in step)

    def accepted(x):
        return next(step for case in x["cases"] for step in case["steps"]
                    if step["op"] == "remember" and "error" not in step["expect"])

    def labelled(x, error, label):
        case = next(case for case in x["cases"]
                    if any(step["expect"].get("error") == error for step in case["steps"]))
        case["assumptions"] = [value for value in case["assumptions"]
                               if not value.startswith(label)]

    changes += [
        lambda x: rejected(x).pop("defect"),
        lambda x: accepted(x).update(defect={"tag": "t", "path": ["clock"], "set": 1}),
        lambda x: next(step for case in x["cases"] for step in case["steps"]
                       if step["op"] == "state").update(
            expect={"error": "MissingMemory"},
            defect={"tag": "t", "path": ["clock"], "set": 1}),
        lambda x: rejected(x).update(defect="tag"),
        lambda x: rejected(x)["defect"].update(delete=True),
        lambda x: rejected(x)["defect"].pop("set"),
        lambda x: rejected(x)["defect"].update(extra=True),
        lambda x: rejected(x)["defect"].update(tag=""),
        lambda x: rejected(x)["defect"].update(path=[]),
        lambda x: rejected(x)["defect"].update(path=["expect", "error"]),
        lambda x: rejected(x)["defect"].update(path=["request"]),
        lambda x: rejected(x)["defect"].update(path=["request", True]),
        lambda x: rejected(x)["defect"].update(path=["request", "missing", "leaf"]),
        lambda x: rejected(x)["defect"].update(path=["request", "items"], set=[]),
        lambda x: rejected(x).update(defect={"tag": "t", "path": ["request", "scope"],
                                             "delete": True}),
        lambda x: rejected(x).update(defect={"tag": "t", "path": ["request", "key"],
                                             "delete": False}),
        lambda x: rejected(x).update(defect={"tag": "t", "path": ["request", "items", 0],
                                             "delete": True}),
        lambda x: next(case for case in x["cases"] if case["name"] == "dedup-alias-m-q-r")[
            "steps"][1].update(defect={"tag": "t", "path": ["request", "items", 1],
                                       "delete": True}),
        lambda x: rejected(x)["defect"].update(step=99),
        lambda x: rejected(x)["defect"].update(step=True),
        lambda x: labelled(x, "DuplicateItem", "fixture: duplicate-bundle-ids-reject"),
        lambda x: labelled(x, "InvalidBundle", "fixture: empty-bundle-rejects"),
    ]
    for change in changes:
        candidate = copy.deepcopy(fixture)
        change(candidate)
        invalid.append(json.dumps(candidate))
    for text in invalid:
        try:
            parse_fixture(text)
        except ValueError:
            continue
        raise AssertionError("accepted malformed fixture")
    return len(invalid)


def permutation_checks():
    rng = random.Random(20261101)
    scope = {"tenant": "t", "agent": "a"}
    items = [{"id": name, "kind": "entity", "content": name, "source": name,
              "observed_at": 10}
             for name in ("a", "b", "c", "d")]
    baseline = None
    count = 0
    for permutation in itertools.permutations(items):
        ordered = list(copy.deepcopy(permutation))
        rng.shuffle(ordered)
        model = Model()
        model.mutate("remember", {"scope": scope, "key": "write", "items": ordered,
                                 "receipt_window": 100}, 10)
        snapshot = copy.deepcopy(model.memories)
        if baseline is None:
            baseline = snapshot
        if snapshot != baseline:
            raise AssertionError("independent remember order changed state")
        targets = [{"id": item["id"], "revision": 1} for item in ordered]
        rng.shuffle(targets)
        result = model.mutate("forget", {"scope": scope, "key": "erase",
                                         "targets": targets, "receipt_window": 100}, 11)
        if (result["deleted"] != ["a", "b", "c", "d"]
                or model.memories or model.edges or model.evidence):
            raise AssertionError("independent forget order changed state")
        # Canonical map order must never affect receipt digests.
        pairs = list(scope.items())
        rng.shuffle(pairs)
        if digest(dict(pairs)) != digest(scope):
            raise AssertionError("object key order changed digest")
        # A semantically identical request with reversed map keys must replay.
        original = {"scope": scope, "key": "write", "items": ordered, "receipt_window": 100}
        replay = model.mutate("remember", dict(reversed(list(original.items()))), 12)
        if replay["sequence"] != 1 or model.sequence != 2 or model.memories:
            raise AssertionError("canonical replay changed forgotten state")
        count += 1
    return count


def boundary_checks():
    """Direct non-JSON scalar and inclusive-limit checks supplement the goldens."""
    scope = {"tenant": "t", "agent": "a"}
    item = {"id": "m", "kind": "episodic", "content": "x", "source": "s",
            "observed_at": 10}
    count = 0
    for importance in (float("nan"), float("inf"), -float("inf"), 10**1000):
        model = Model()
        request = {"scope": scope, "key": "k", "receipt_window": 100,
                   "items": [{**item, "importance": importance}]}
        try:
            model.mutate("remember", request, 10)
        except ModelError as error:
            if str(error) != "InvalidImportance":
                raise
        else:
            raise AssertionError("accepted nonfinite/out-of-range importance")
        if model.memories or model.receipts or model.sequence:
            raise AssertionError("invalid scalar changed state")
        count += 1
    model = Model()
    request = {"scope": scope, "key": "k", "receipt_window": 100,
               "items": [{**item, "id": str(i), "content": str(i)}
                         for i in range(MAX_OPS - 2)]}
    result = model.mutate("remember", request, 10)
    if (result["actions"] != [["created", str(i), 1] for i in range(MAX_OPS - 2)]
            or len(model.memories) != MAX_OPS - 2):
        raise AssertionError("4094 distinct creates plus receipt and outcome rejected")
    request["items"].append({**item, "id": "extra", "content": "extra"})
    try:
        Model().mutate("remember", request, 10)
    except ModelError as error:
        if str(error) != "OperationLimit":
            raise
    else:
        raise AssertionError("4095 distinct creates plus receipt and outcome accepted")
    count += 2
    request["items"] = [{**item, "id": f"d{i}"} for i in range(MAX_OPS)]
    result = Model().mutate("remember", request, 10)
    if result["actions"][0] != ["created", "d0", 1] or any(
            action != ["noop", "d0", 1] for action in result["actions"][1:]):
        raise AssertionError("within-bundle exact duplicates changed memory")
    count += 1
    model = Model()
    request["items"] = [copy.deepcopy(item), copy.deepcopy(item)]
    try:
        model.mutate("remember", request, 10)
    except ModelError as error:
        if str(error) != "DuplicateItem":
            raise
    else:
        raise AssertionError("repeated caller ID in one bundle accepted")
    if model.memories or model.receipts or model.sequence:
        raise AssertionError("repeated caller ID changed state")
    count += 1
    request = {"scope": scope, "key": "body", "receipt_window": 100,
               "items": [{**item, "content": ""}]}
    padding = MAX_BODY - len(canonical(request))
    request["items"][0]["content"] = "x" * padding
    if len(canonical(request)) != MAX_BODY:
        raise AssertionError("body boundary setup is not exactly 16 MiB")
    try:
        Model().mutate("remember", request, 10)
    except ModelError as error:
        if str(error) != "OperationLimit":
            raise
    else:
        raise AssertionError("16-MiB content ignored physical chunk operations")
    request["items"][0]["content"] += "x"
    try:
        Model().mutate("remember", request, 10)
    except ModelError as error:
        if str(error) != "BodyLimit":
            raise
    else:
        raise AssertionError("16-MiB-plus-one body accepted")
    count += 2
    model = Model()
    request = {"scope": scope, "key": "attached", "receipt_window": 100, "items": [{
        **item, "id": str(i), "content": str(i),
        "evidence": [{"id": f"e{i}", "scope": scope}]} for i in range(1366)]}
    try:
        model.mutate("remember", request, 10)
    except ModelError as error:
        if str(error) != "OperationLimit":
            raise
    else:
        raise AssertionError("evidence operations bypassed the bundle limit")
    if model.memories or model.evidence or model.receipts:
        raise AssertionError("oversized attachment plan changed state")
    count += 1
    model = Model()
    model.mutate("remember", {"scope": scope, "key": "nodes", "receipt_window": 100, "items": [
        {**item, "kind": "entity"},
        {**item, "id": "n", "kind": "entity", "content": "y"},
    ]}, 10)
    for i in range(MAX_OPS):
        model.execute({"op": "edge", "id": f"edge{i}", "from": "m", "to": "n"})
    before = copy.deepcopy(model.__dict__)
    try:
        model.mutate("forget", {"scope": scope, "key": "erase",
                                "targets": [{"id": "m", "revision": 1}],
                                "receipt_window": 100}, 11)
    except ModelError as error:
        if str(error) != "OperationLimit":
            raise
    else:
        raise AssertionError("oversized entity erase was treated as small forget")
    if model.__dict__ != before:
        raise AssertionError("oversized entity erase changed state")
    count += 1
    return count


def self_test(fixture):
    for case in fixture["cases"]:
        run_case(case)
    single = sum(verify_single_defect(case) for case in fixture["cases"])
    second = second_defect_checks(fixture)
    malformed = malformed_checks(fixture)
    permutations = permutation_checks()
    boundaries = boundary_checks()
    cases = {case["name"]: case for case in fixture["cases"]}
    killed = 0
    for mutation, (name, index, reason) in MUTATIONS.items():
        if name not in cases or index >= len(cases[name]["steps"]):
            raise AssertionError(f"mutation {mutation} names no golden {name} step {index}")
        try:
            run_case(cases[name], mutation)
        except AssertionError as failure:
            if not (str(failure).startswith(f"{name} step {index}: ")
                    and reason in str(failure)):
                raise AssertionError(f"mutation {mutation} was detected for an unintended "
                                     f"reason: {failure}") from None
            killed += 1
            continue
        except (ModelError, KeyError, TypeError) as error:
            raise AssertionError(f"mutation {mutation} crashed: {error!r}") from None
        raise AssertionError("undetected semantic mutation: " + mutation)
    print(f"PASS: {len(fixture['cases'])} golden cases; {single} single-defect "
          f"rejections repaired and accepted; {second} second-defect injections "
          f"detected; {malformed} malformed fixtures rejected; {permutations} seeded "
          f"permutations; {boundaries} scalar/boundary checks; "
          f"{killed}/{len(MUTATIONS)} semantic mutations detected at their intended step")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true", required=True)
    parser.add_argument("--fixture", type=Path, default=FIXTURE)
    args = parser.parse_args()
    try:
        self_test(parse_fixture(args.fixture.read_text(encoding="utf-8")))
    except (ValueError, AssertionError, KeyError, TypeError, OSError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
