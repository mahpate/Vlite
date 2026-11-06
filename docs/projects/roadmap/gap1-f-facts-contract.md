STATUS: DRAFT, paused 2026-11-06; unreviewed; NOT ratifiable; not done: golden expected values (cases are named only), hand mutants (35 numbered lines carrying 39 edit ideas, several thin and none yet checked against code), outcome byte layouts and strict decoder rules of the correct and forget-series receipts, evidence handling for fact segments, the F6 entity-forget read, a final line-by-line citation pass (citations marked "function-level" are not pinned to lines).

# Gap 1 F: agent-memory fact track, crate-private core contract (proposed draft)

Status: **PROPOSED DRAFT. Not ratified, not frozen, not implemented, not
activated.** Every shape marked PROPOSAL needs the coordinator before anything
is built. Adopted inputs are only those the brief and the C2 remainder
contract name: decisions 14-20 as drafted and coordinator ruling A (section 1).
Base: worktree HEAD `336874db`, branch `mahpate-f1-temporal-memory`
(uncommitted E2 work in `src/memory/erasure*` is cited as found in the working
tree). Authority: [C2 remainder](gap1-c2-remainder-contract.md) section 6 F1-F6
and decisions 14-20, [Gap 1](gap1-agent-memory.md) sections 1, 4 and 5,
[Gap 4](gap4-temporal-graph.md), the
[memory serving contract](serving-memory-contract.md) and the
[adapter mapping](gap1-adapter-mapping.md). The template is the
[E2 contract](gap1-e2-resume-contract.md). "Exact" below specifies a proposed
private semantic version, not a public API, wire shape or released format.

## 1. Boundary, identity and adopted inputs

This contract specifies the crate-private core of the FACT track, in a new
module `src/memory/facts.rs` with tests in `src/memory/facts/tests.rs`
(C2 section 5 rule 1):

| Slice | Operation | Core entry (proposed) |
|---|---|---|
| F1 | declare and use slots; remember facts and relations | `MemoryMut::assert_facts` |
| F2 | close, replace, remove and supersede one fact segment | `MemoryMut::correct_facts` |
| F3 | read one slot at fixed T and C, or its history | `MemoryRef::find_facts` |
| F6 | delete one whole series with its history; entity-forget seam | `MemoryMut::forget_series`, seam to E3 |

F4 (disputed assertions, contradiction labels) and F5 (recall over fact
segments) are DEFERRED. Section 11 lists only the seams this design keeps open.

Nothing else changes:

- No wire, REST, gRPC, MCP, CLI, client, OpenAPI or proto change; no public
  `Error` or Rust API change; no header flag, WAL operation or snapshot
  encoding (the temporal entries and flag 64 already exist in Gap 4); no Cargo
  change; no dependency on KV, leases or the Gap 2 ledger.
- New persisted objects exist only in graph files that already carry flag 64
  (`format::FLAG_TEMPORAL`). A file without any slot node or fact series
  behaves exactly as before (rollback rule, section 10).
- Dark: every fact test runs under the test-only `Activation` seam. Later the
  same code ships behind the `--cfg vlite_unstable_temporal` build gate (T1)
  until the owner flips activation. Without the seam or gate a fact write is
  refused with `Error::UnsupportedFeature(64)` and nothing is written
  (section 13, test `f_refusal_without_seam_writes_nothing`).
- Single process, one `&mut Database` call per operation. No worker, timer or
  retry loop.
- Recall, consolidate and stale keep refusing or ignoring the new kinds until
  F5 (decision 16, section 9).
- Not secure erasure. Removing or deleting a segment is not privacy erasure
  (Gap 1 section 5); backups, replicas, logs and feeds keep what they held.

Adopted inputs (from the brief; each stays valid only while the coordinator
keeps it):

| Input | Content |
|---|---|
| Decision 14 | One slot node per `(scope, subject, predicate, slot_key)` carrying the immutable mode and a bounded series list. Series cap per slot: **64** (ruling A fixes the number decision 14 left open) |
| Decision 15 | A series is a keyed temporal edge between in-scope entity anchors, with a fixed relationship name and a key derived from slot and destination. Versioned evidence uses new records referenced from segment properties |
| Decision 16 | Crate-private `MemoryKind::Fact` and `MemoryKind::Relation`; recall keeps refusing them until F5 |
| Decision 17 | An exact-end close commits a receipt and no temporal entry |
| Decision 18 | Fact query limits 20 default, 100 maximum, no paging; `history` forbids an explicit T |
| Decision 19 | Crate-private `MemoryError` variants only; no public `Error` change. Of the five variants it names, `DeletionPending` and `JobConflict` already exist (`src/memory.rs:118`, `:120`) and `ReconciliationRequired` belongs to E4. F adds exactly `SlotConflict` and `SlotModeConflict` |
| Decision 20 | F1-F6, when accepted, deliver Gap 4 slice D; the coordinator owns that document update |
| Ruling A | Bindings may declare optional `slots: [{predicate, mode}]`. The mode is stored on the slot node at first use and is immutable. A change needs a higher binding generation. Binding files without slots stay byte-valid for 0.7.1. The bindings-file version is bumped only when slots are used |

Note: C2 section 9 still lists decisions 14-16, 18 and 20 as "held". This
draft follows the brief, which adopts them; the coordinator should update C2
(erratum E-F1 in the notes file).

## 2. Base facts (verified against the working tree)

Citations are `file:line` unless marked function-level (not pinned).

| # | Fact | Where |
|---|---|---|
| B1 | Frame limits: 4,096 operations, 16 MiB, 64 provenance entries, 256-byte opaque values, 4,096-byte string values | `src/memory.rs:74-78` |
| B2 | Node labels and the six `(label, key prefix)` kinds the core writes. `names.rs` reserves every label starting `VliteMemory` and exactly the listed key prefixes; legacy adoption accepts only listed pairs. A new `VliteMemorySlot` / `slot:v1:` kind must join the const (array length 7) or raw writers could plant `slot:v1:` keys | `src/memory.rs:57-73`, `src/memory/names.rs:15-24`, `src/memory/startup.rs:120-128` |
| B3 | `MemoryError` variants (no `Slot*` yet); `Error::BatchTooLarge` converts to `BodyLimit`. Two exhaustive matches must gain arms: `mapping::memory_error` (function-level) and the test helper `fixture_error_name` | `src/memory.rs:83-125`, `:130`, `:4676-4701`, `src/memory/mapping.rs` |
| B4 | `MemoryKind` is `{Entity, Episodic, Unsupported}`. `validate_request` refuses only `Unsupported`; so adding `Fact` and `Relation` needs a second refusal in `remember`. Recall refuses `Unsupported` at `recall.rs:255`; its mandatory filter adds a `kind` condition only when all requested kinds are equal; C1 and S1 accept only the strings `entity` and `episodic` | `src/memory.rs:186-200`, `:1279-1281`, `src/memory/recall.rs:255`, `:906-921`, `src/memory/consolidate.rs:587`, `src/memory/stale.rs` (`decode_source`, function-level) |
| B5 | `opaque()` is length only: non-empty and at most 256 bytes. It does not reject controls; the adapter does (mapping section 6, `gap1-adapter-mapping.md` "Lengths and characters") | `src/memory.rs:2215-2217` |
| B6 | `remember` order: validate, scope node and tuple, barrier probe, receipt probe or replay with barrier rules, deadline, future observation, evidence owner checks, plan, budget `planned.budget_operations + 1 + outcome_chunk_count > MAX_OPS`, write | `src/memory.rs:365-480` (budget at `:473`) |
| B7 | Keys: `memory_key = mem:v1:{ScopeKey}:{hex(H(id))}` (145 bytes); `operation_receipt_key(op, scope, key)`; `receipt_outcome_key` | `src/memory.rs:1603`, `:1615`, `:1796` |
| B8 | C1 is the template for a new operation with its own digest, receipt and strict decoder: domains, `put_u32/put_s/put_o/put_scope`, `digest`, `decode_outcome`, field tables `RECEIPT_FIELDS` / `OUTCOME_FIELDS`, `decode_receipt` | `src/memory/consolidate.rs:21-29`, `:112-142`, `:327-359`, `:395-458` |
| B9 | GetReceipt lookup handles forget, consolidate, begin and resume receipts only; remember is refused because its outcome cannot be decoded without the request | `src/memory/receipt.rs:44-130` |
| B10 | Binding file: version must equal `memory-bindings-v1`; `Document`, `Entry`, `GrantEntry`, `ScopeEntry` all `deny_unknown_fields`; any serde error is `BindingError::Malformed`. The server turns a parse error into a startup failure with the fixed message "memory bindings file is invalid" | `src/memory/binding.rs:18`, `:353`, `:371-373`, `:387-389`, `:435`; `src/server/admin.rs:362-383` |
| B11 | Rotation rule: if `state` or the grants digest changed the generation must advance (`GenerationNotAdvanced`), if unchanged it must be equal (`GenerationChanged`). The stored record is compared by value at startup and rewritten | `src/memory/binding.rs` (`validate_against`, function-level), `src/memory/binding/record.rs` (function-level) |
| B12 | Temporal engine: half-open `[valid_from, valid_to)` in `i64` with `None` ends; 256 segments per series; touching legal; only the first segment may be open-started; close needs `from < at < to` or `at == to` (no-op) | `src/property_graph/engine/temporal.rs:24`, `:128-215` (function-level for `check_close`) |
| B13 | `TemporalOp` has `CreateEdge`, `PromoteEdge`, `AppendSegment`, `ReplaceSegment`, `CloseSegment`, `RemoveSegment`. Every change to an existing series needs `Expected::Revision`; inside one batch the revision is the one the edge had before the batch; an edge created earlier in the batch cannot be corrected in that batch; an exact-end close plans nothing but still checks revision and segment; a keyed `CreateEdge` retry with an identical series returns `created = false`, anything else is `EdgeKeyConflict` | `src/property_graph/temporal.rs:31-101`, `src/property_graph/engine/mutate/temporal.rs:78-263` (no-op at `:147-156`, conflict at `:260`) |
| B14 | The seam: `property_graph::temporal::supported()` is a test-only thread local, `Activation` switches it; the engine's own `supported()` is `cfg!(test)`. Non-temporal files refuse temporal operations with `UnsupportedFeature(64)` | `src/property_graph/temporal.rs:269`, `:287`, `src/property_graph/engine/temporal.rs:33` |
| B15 | Reads: on a flag-64 file every plain graph read (including memory-core reads) needs `supported()` and is evaluated at T = C. `GraphRef::as_of` and `timeline` exist; `timeline` ignores validity but enforces TTL, 0..=256 segments | `src/property_graph/read.rs:769-833`, `src/property_graph/temporal/read.rs:124`, `:299` |
| B16 | `GraphOp::DeleteEdge` on a temporal edge deletes the whole series; `DeleteNode` fails if any incident edge remains; A2 forget and E2 `incident_edges` list neighbors in both directions at T = C and therefore do not see a series hidden at C | `src/memory.rs:578` (forget), `src/memory/erasure/resume.rs:541-548`, engine rule (function-level) |
| B17 | `Attach` binds one record to at most one node per collection; many fact versions cannot attach to one slot node, which is why decision 15 uses unattached records referenced from segment properties | `src/property_graph/mod.rs:767` (`memory_attachment_owner`) |
| B18 | Reclaim entries of due TTL are planned inside the same frame today; their bytes count toward the 16 MiB body, their entries not toward 4,096 | `src/property_graph/engine/mutate.rs:312`, `src/property_graph/engine/mutate/temporal.rs:79` |
| B19 | Graph revisions are relabelled by compaction and upgrade (all revisions, including temporal edges) and by a same-lineage directory checkpoint (nodes and ordinary edges restated at the checkpoint sequence, temporal edge revisions kept and clamped). So slot and series revisions have no relation after maintenance (E2 R13) | `src/property_graph/engine/state/temporal.rs:85-100`, `src/frame.rs:343-383`, [E2 section 7](gap1-e2-resume-contract.md#7-atomicity-budgets-clocks-maintenance-and-rollback) |
| B20 | Barrier: `erasure::barrier` is a presence probe of the job node. A write in a barred scope replays a matching live pre-barrier receipt or fails `DeletionPending`; recall omits the scope; S1 `summary_status` fails `DeletionPending` | `src/memory/erasure.rs:425-430`, `src/memory.rs:405-421`, `src/memory/recall.rs:384-395`, `src/memory/stale.rs:193` |
| B21 | Engine limits: key 256 bytes, 8 labels, label and relationship names at most 63 bytes, 64 properties per element, 65,536 property bytes, 4,096 batch entries, 16 MiB batch | `src/property_graph/engine/limits.rs` (function-level) |

## 3. Slot declaration (ruling A) and the binding record

### 3.1 Binding file syntax (PROPOSAL, shape of the adopted ruling)

An entry may carry an optional `slots` array, each element
`{"predicate": <string>, "mode": "exclusive" | "multivalued"}`, both keys
required, no other key (`deny_unknown_fields`).

- **Version rule (R2).** `version` stays `memory-bindings-v1` for a file with no
  `slots` key anywhere. A file in which at least one entry has a nonempty
  `slots` array must say `memory-bindings-v2`. A `v1` file with a `slots` key
  is `Malformed` (exactly what 0.7.1 does with an unknown key); a `v2` file
  with no nonempty `slots` is `Malformed` (so "v1 iff 0.7.1 can read it").
- **Validation (R3, R4).** `predicate`: 1..=64 bytes, ASCII, first byte
  `[a-z0-9]`, rest `[a-z0-9._-]`; unique within the entry (`InvalidSlot`, a new
  crate-private `BindingError`); at most 256 slots per entry (`TooLarge`);
  `mode` outside the enum is `Malformed`. The file caps are unchanged (1 MiB,
  256 bindings, 1,024 grants, 256 scopes, `binding.rs:25-28`).
- **Core input.** The adapter looks the request predicate up in the entry's
  declared slots (exact match) and passes the declared `mode` with every fact
  item. An undeclared predicate never reaches the core (adapter refusal,
    section 11.3). The core does not read the bindings file.

### 3.2 Stored binding record and generation (PROPOSAL, R6)

Slots are part of binding state: any change to the declared `(predicate, mode)`
set is a change that requires a higher `generation` (rotation rule B11), like a
grants change. The record gains an optional `slots_digest` property:

`SlotsDigest = "v1:" + hex(H("vlite.memory.binding.slots.v1\0" || U32(n) ||
(S(predicate) || U8(mode: 1 exclusive, 2 multivalued)) for each slot sorted by
predicate bytes ascending))`.

- A binding with no slots writes the record **exactly as today**: no
  `slots_digest` property and the existing record schema. `Recorded::of` and the
  `changed` test treat an absent digest as the empty set.
- A binding with slots writes `slots_digest` and a bumped record schema marker
  (the record decoder rejects unknown property names as `Corrupt` today, so a
  0.7.1 reader that meets the property fails closed; this is acceptable because
  such a graph is flag-64 and one-way anyway, section 10).
- Alternative considered: store the full slot list in the record (bounded by 256
  entries); rejected for size and because only equality is needed.

### 3.3 Byte-compatibility rule for 0.7.1 (rollback)

1. A binding file without slots serializes and parses exactly as today
   (version `memory-bindings-v1`, no `slots` key); 0.7.1 accepts it.
2. A file with slots is `memory-bindings-v2`. 0.7.1 rejects it as `Malformed`
   (the unsupported version value, and the unknown `slots` key) and the server
   fails startup with "memory bindings file is invalid" (`src/server/admin.rs:362-383`).
   Operators downgrading must first remove the slots and restore `v1`.
3. A graph that holds a slot node or a series is flag-64 and is refused by
   0.7.1 at open with `UnsupportedFeature(64)`, whatever the bindings say. The
   bindings-file rule therefore protects the case "slots declared, no fact
   written yet", where a downgrade is still possible.
4. Whether refusing the whole file (rather than ignoring `slots`) is acceptable
   is **R2, coordinator**: the alternative (ignore unknown keys in 0.7.1) cannot
   be done retroactively.

### 3.4 Slot identity and the slot node

Slot identity is `(scope, subject_id, predicate, slot_key)`; each of the three
strings is `opaque` (1..=256 bytes).

```text
SlotDigest = H("vlite.memory.fact.slot.v1\0" || SCOPE(ScopeBytes)
               || S(subject_id) || S(predicate) || S(slot_key))
SlotKey    = "slot:v1:" || ScopeKey || ":" || hex(SlotDigest)       // 146 bytes
```

Notation is E1 section 3 and C1 (`H` SHA-256, `S(s) = U32(len) || bytes`,
`SCOPE(b) = U32(len) || b`, `O(x)`: `0` or `1 || S(x)`, `U8`, `U32`, `U64` big
endian, `I64` two's complement big endian, `OI64(None) = 0`,
`OI64(Some(v)) = 1 || I64(v)`, `OU64` likewise, `F64(x) = U64(IEEE-754 bits)`
with `-0.0` normalized to `+0.0`; non-finite values are rejected before any
encoding).

The slot is created implicitly at first use in the same frame as the first
`assert_facts` that names it (R7; this supersedes the C2 F1 sketch of a
separate `provision_slot`). Slot node, exactly one label `VliteMemorySlot`, no
graph TTL:

| Property | Type | Meaning |
|---|---|---|
| `f1_schema` | String | `slot:v1` |
| `scope` | String | ScopeKey |
| `f1_scope_tuple` | String | hex of ScopeBytes (mismatch: `DigestCollision`) |
| `subject` | String | subject id (opaque) |
| `predicate` | String | predicate (opaque) |
| `slot_key` | String | caller slot key (opaque) |
| `mode` | String | `exclusive` or `multivalued`, immutable (decision 14) |
| `series_count` | Integer | 0..=64 |
| `series` | String | present iff `series_count >= 1`: the 64-hex `DestHash` of each series, strictly ascending, concatenated, length `64 * series_count` (at most 4,096 bytes, the string-value cap) |

The series list is a registry, not the truth: the truth is the series edge
(section 5). A listed series whose edge is gone (reclaimed with an anchor by
TTL, or forgotten) is skipped by reads and pruned by the next slot write
(DG4). The slot node's graph revision is the concurrency guard of every write
that touches the slot (section 8): a slot write is a node write guarded by
`Expected::Revision(r)` (the `put_node` pattern, `consolidate.rs:484`) and the
creation form for a new slot.

## 4. Types, operations and error precedence

All types are crate-private. Strings named `*_id`, `predicate`, `slot_key`,
`source` are `opaque`.

```text
SlotMode = Exclusive | Multivalued
FactKind = Fact | Relation                         // MemoryKind::{Fact, Relation}
SlotExpected = Absent | Revision(u64)
FactSlot { subject_id: String, predicate: String, slot_key: String }
FactInterval { valid_from: Option<i64>, valid_to: Option<i64> }   // None = open

FactItem {
  id: String, kind: FactKind, slot: FactSlot, mode: SlotMode,
  destination_id: String, interval: FactInterval,
  content: String,                       // 0..=4096 bytes (R14)
  source: String, provenance: Vec<Provenance>,
  importance: Option<f64>, confidence: Option<f64>,
  observed_at_ms: Option<i64>, evidence: Vec<MemoryEvidence>,   // evidence: R20, 9.5
  slot_expected: SlotExpected,
}
AssertFactsRequest {
  scope: MemoryScope, idempotency_key: String, now_ms: u64,
  receipt_window_ms: u64, deadline_ms: Option<u64>,
  allow_future_observations: bool, items: Vec<FactItem>,
}
FactAction = Created | Appended | Noop
FactOutcome { id: String, action: FactAction, series_key: String }
AssertFactsResult {
  outcomes: Vec<FactOutcome>,            // request order
  slot_revisions: Vec<(FactSlot, u64)>,  // revision at commit, one per touched slot
  receipt_revision: u64, commit_sequence: u64, replayed: bool,
  logical_operations: usize,             // charged budget, 0 on replay
}

CorrectionAction =
    Close   { at_ms: i64 }
  | Remove
  | Replace { at: ReplaceAt, replacement: FactItem }
ReplaceAt = At(i64) | Interval(FactInterval)       // supersede at c, or explicit interval correction
CorrectFactsRequest {
  scope, idempotency_key, now_ms, receipt_window_ms, deadline_ms,
  slot: FactSlot, mode: SlotMode,
  series_key: String, series_revision: u64,        // the series named by the caller
  slot_revision: u64,
  interval: FactInterval,                          // exact bounds of the target segment
  action: CorrectionAction,
}
CorrectFactsResult {
  outcome: FactOutcome,                            // Appended/Created for supersede, Noop for exact-end close
  receipt_revision, commit_sequence, replayed, logical_operations
}

FactQuery {
  scope: MemoryScope, slot: FactSlot,
  as_of_ms: Option<i64>,                           // forbidden with history
  history: bool, limit: Option<u32>,               // 20 default, 1..=100
  now_ms: u64, allow_partial: bool, limits: GraphLimits,
}
FactView {
  series_key: String, slot: FactSlot, destination_id: String,
  interval: FactInterval, fact_id: String, kind: FactKind,
  content: String, observed_at_ms: i64, importance: f64, confidence: Option<f64>,
  source: String, provenance: Vec<Provenance>, evidence: Vec<u64>,
  series_revision: u64, status: Accepted,          // Disputed is F4
}
FactsResult {
  slot_revision: Option<u64>,                      // None: the slot node does not exist
  facts: Vec<FactView>,
  as_of_ms: i64, ttl_clock_ms: u64, view_sequence: u64,
  complete: bool, truncation: Vec<TruncationReason>, barred: bool,
}
ForgetSeriesRequest {
  scope, idempotency_key, now_ms, receipt_window_ms, deadline_ms,
  series_key: String, series_revision: u64,       // the slot is derived from the key (R8)
}
ForgetSeriesResult { series_key: String, receipt_revision, commit_sequence, replayed, logical_operations }
```

New crate-private surface: `MemoryMut::assert_facts(&AssertFactsRequest) ->
MemoryResult<AssertFactsResult>`, `MemoryMut::correct_facts(&CorrectFactsRequest)
-> MemoryResult<CorrectFactsResult>`, `MemoryMut::forget_series(&ForgetSeriesRequest)
-> MemoryResult<ForgetSeriesResult>`, `MemoryRef::find_facts(&FactQuery) ->
MemoryResult<FactsResult>`. New `MemoryError` variants: `SlotConflict` and
`SlotModeConflict` only. Existing variants carry every other failure, with these
reuses (each a PROPOSAL, R14-R16): an invalid interval is `InvalidTarget`; a
bad `confidence` is `InvalidImportance`; content over 4,096 bytes is
`InvalidContent`; more than 64 series in a slot or 256 segments in a series is
`OperationLimit`; a temporal frame whose estimate exceeds the B4 cap is
`BodyLimit` (existing code, no new `Error` variant, per the coordinator's B4
ruling).

### 4.1 `assert_facts`: validation and order

Structural validation, in this order, before any read:

1. scope (`InvalidScope`); opaque `idempotency_key` (`InvalidKey`); `now_ms` in
   `0..=i64::MAX` (`InvalidClock`); positive `receipt_window_ms` with checked
   `now + window` (`InvalidReceiptWindow`);
2. `items` nonempty (`InvalidBundle`) and at most 4,096 (`OperationLimit`);
3. per item, in request order: `kind` is `Fact` or `Relation` (else
   `UnsupportedKind`); `id`, `source`, slot strings, `destination_id` opaque
   (`InvalidIdentity`); duplicate `id` in the request (`DuplicateItem`);
   `content` at most 4,096 bytes (`InvalidContent`); interval valid, that is
   `valid_from < valid_to` when both finite (`InvalidTarget`); importance and
   confidence finite in `[0, 1]` (`InvalidImportance`); `observed_at_ms`
   present (`InvalidObservation`); provenance count (`ProvenanceLimit`) and
   entries (`InvalidProvenance`) as A1; evidence ids and scope as A1
   (`InvalidEvidence`, `CrossScope`); at most 64 evidence entries per item;
4. mode consistency inside the request: every item of one slot carries the same
   `mode` and the same `slot_expected` (`InvalidBundle`; two items of one slot
   disagreeing on the guard cannot both be satisfied);
5. the encoded request fits 16 MiB (`BodyLimit`).

Then, strictly in this order (the A1/C1 pattern, B6):

6. Read the provisioned scope node and verify its tuple (`InvalidScope`,
   `DigestCollision`).
7. Barrier probe; receipt probe of `R("assert-facts", key)`. A live receipt with
   the same digest replays (section 6) after the barrier rule: in a barred scope
   a live pre-barrier receipt replays, a different digest or a post-barrier
   receipt is `DeletionPending`; outside a barrier a different digest is
   `IdempotencyConflict`. An expired receipt is ignored and replaced. A receipt
   whose outcome node is missing or malformed is `InvalidBundle`.
8. Barrier present and no replay: `DeletionPending`.
9. `deadline_ms` present and `now_ms >= deadline`: `DeadlineExceeded`.
10. Future observation without `allow_future_observations`: `FutureObservation`.
11. Anchors: for each distinct subject and destination, read the node
    `memory_key(scope, id)` at C. Missing or hidden by TTL at C:
    `MissingReference`; a node that is not a `VliteMemory` entity of this scope
    (kind not `entity`, scope property differs): `CrossScope` when the scope
    differs, `InvalidReference` otherwise. Subject equal to destination is
    allowed (a self relation) and is one anchor.
12. Per distinct slot (sorted by `SlotKey` bytes): read the slot node.
    Present: decode strictly (DS1-DS6); its `mode` must equal the item mode
    (`SlotModeConflict`); `slot_expected` must be `Revision(r)` with `r` equal
    to the node revision (any mismatch, including `Absent` against an existing
    slot or `Revision` against a missing one: `RevisionConflict`). Absent: the
    expected value must be `Absent` (else `RevisionConflict`) and the slot is
    created in this frame. For each listed series read its edge strictly
    (DE1-DE8).
13. Plan (section 7): exclusive overlap across all the slot's series
    (`SlotConflict`), same-series overlap in a multivalued slot
    (`SlotConflict`), exact duplicate (`Noop`), series cap 64 and segment cap 256
    (`OperationLimit`). Every planning failure changes nothing.
14. Budget (section 8): operations plus guards plus receipt and outcome nodes
    at most 4,096 (`OperationLimit`); estimated frame bytes against the 16 MiB
    body and the B4 transport cap (`BodyLimit`).
15. One `write_batch_at(&ops, now)` frame. A frame error is returned as A2
    returns it (the engine's `Storage` error) and is never converted to
    `RevisionConflict`: under `&mut Database` nothing can change the slot
    between steps 12 and 15 except the TTL reclaim of section 8.
    `CommitIndeterminate` is preserved and never retried blindly.

### 4.2 `correct_facts`: validation and order

Structural validation: scope, key, clock, window as 4.1.1; `slot` strings,
`series_key` opaque (`InvalidTarget`; shape check: `fact:v1:` + 64 hex + `:` +
64 hex, else `InvalidTarget`); `series_revision` and `slot_revision` nonzero
(`InvalidTarget`); `interval` valid (`InvalidTarget`); the action's own fields:
`Close.at_ms` any `i64`; `Remove` carries nothing; `Replace.replacement` is a
complete `FactItem` validated as in 4.1.3 and its `slot`/`mode` must equal the
request's (`InvalidTarget`), its `destination_id` may differ (a new series);
`Replace` with `ReplaceAt::Interval` is an explicit interval correction.

Then: scope node, barrier and receipt of `R("correct-facts", key)` (replay rules
as 4.1.7); deadline; slot node (absent: `MissingMemory`; mode mismatch:
`SlotModeConflict`; revision not equal to `slot_revision`: `RevisionConflict`);
the series must be listed in the slot and its edge must exist and decode
(`MissingMemory`); series revision not equal to `series_revision`:
`RevisionConflict`; the segment with exactly `interval` must exist
(`MissingMemory`).

Semantics (Gap 1 section 4, C2 F2):

- **Close at c.** Valid when `from < c < to` (open ends as infinities) with the
  engine's `check_close`; `c == to` (finite) is the exact-end no-op: the frame
  carries the receipt and outcome only (`FactAction::Noop`, decision 17), no
  slot write, series untouched. Any other `c` is `InvalidTarget`.
- **Replace at c (supersede).** One frame: `CloseSegment` of the old segment at
  `c`, then, when the replacement keeps the destination, `AppendSegment` of the
  replacement starting at `c` to the same series; when the destination changes,
  `CreateEdge` of a new series carrying the replacement (a changed destination
  is a different series, Gap 4 doc, "changed destination" paragraph). The two
  operations on one existing series share the pre-batch revision (B13). The
  replacement interval must start at `c` (`valid_from = Some(c)`) and end at or
  before the old end; else `InvalidTarget`. The slot guard applies to the whole
  supersession, and the exclusive overlap check runs on the state after the
  close.
- **Replace with explicit interval (correction).** `ReplaceSegment` with the
  replacement's content and the corrected interval, same series and destination
  only (else `InvalidTarget`).
- **Remove.** `RemoveSegment`; the edge stays with a possibly empty series until
  `forget_series` deletes it. Not privacy erasure.

Planning, budget and write follow 4.1.13-4.1.15.

### 4.3 `find_facts`

Structural validation: scope; slot strings (`InvalidTarget`); `now_ms` in
`0..=i64::MAX` (`InvalidClock`); `history` with `as_of_ms` present is
`InvalidTarget` (decision 18); `limit` default 20, outside `1..=100` is
`InvalidTarget`; limits validate as recall (`InvalidTarget`).

Read order: scope node (`InvalidScope`, `DigestCollision`); barrier. A barred
scope returns `FactsResult { facts: [], barred: true, complete: false, .. }`
(OMIT with a marker, consistent with recall; alternative `DeletionPending`, as
S1 does, is **R11, coordinator**). Then the slot node at C: absent gives an
empty result with `slot_revision: None` (not an error: a slot that was never
asserted has no facts). Present: strict decode, listed series in ascending
`DestHash` order, each read at (T, C):

- **At T** (default `T = now_ms` as `i64`, or `as_of_ms`): per listed series,
  `GraphRef::as_of` of the series edge by key returns the segment valid at T if
  any; at most 64 reads. The result order is series key, then lower bound, then
  `destination_id` (serving contract "Facts and corrections").
- **History**: per listed series, `timeline(edge, 0..=256)`; all retained
  segments, ordered by series key, `valid_from` (open first), `valid_to`.

`limit` caps the returned `FactView` count; reaching it before all series are
read marks `complete = false` with `TruncationReason::Output` and, without
`allow_partial`, is `OperationLimit` (no partial data, no paging). Work
exhaustion follows `allow_partial` the same way. Authorization, storage and
cancellation errors never return partial data. The result reports the T and C
used and the read's `view_sequence`. Stale labels are not computed for facts
(the adapter reports `stale: false`).

### 4.4 `forget_series`

Structural validation as 4.2 (scope, key, clock, window, `series_key` shape,
nonzero `series_revision`; no slot strings, no mode, no `slot_revision`: the
Forget target on the wire carries only `series{id, revision}`). Then scope,
barrier, receipt of `R("forget-series", key)`, deadline; the slot node is found
by `"slot:v1:" || ScopeKey || ":" || <the slot digest hex inside the series
key>` (`MissingMemory` when absent), decoded strictly, and its revision is read
fresh in this call and used as the frame guard (the caller cannot supply it);
the series must be listed and decoded (`MissingMemory`), and its revision must
equal `series_revision` (`RevisionConflict`). One frame: `GraphOp::DeleteEdge` of the series (deletes the whole
history, B16), the slot node write removing the series from the list (or, with
the emptied-slot rule R9, deleting the node), deletion of the fact-evidence
records the series' segments reference that no other segment and no attachment
holds (section 11, not fully specified here), receipt and outcome. Closing
validity is never deletion (Gap 1 section 5).

## 5. Persisted schemas and strict decoder rules

### 5.1 Series edge

A keyed temporal edge created by `TemporalOp::CreateEdge`:

- `key` = `SeriesKey = "fact:v1:" || hex(SlotDigest) || ":" || hex(DestHash)`
  (137 bytes), with `DestHash = H("vlite.memory.fact.destination.v1\0" ||
  S(destination_id))`. The cleartext `hex(SlotDigest)` lets E3 and F6 find the
  slot of a series seen as an incident edge.
- `relationship` = `VLITE_FACT` (fixed, 10 bytes).
- `source` = `memory_key(ScopeKey, subject_id)`, `destination` =
  `memory_key(ScopeKey, destination_id)`: live, unexpired entity anchors of the
  same scope (4.1.11).
- Segments: each carries its validity only in the typed bounds, never in
  properties; a property named `valid_from` or `valid_to` is a decode failure
  (DE7).

### 5.2 Segment properties (PROPOSAL; names and types are not yet approved)

| Property | Type | Required | Meaning |
|---|---|---|---|
| `f1_schema` | String | yes | `fact:v1` |
| `fact_id` | String | yes | item id (opaque); unique within a request only |
| `fact_kind` | String | yes | `fact` or `relation` |
| `destination_id` | String | yes | id of the destination anchor (redundant, verified against the edge endpoint) |
| `content` | String | no | present iff nonempty, at most 4,096 bytes |
| `observed_at_ms` | Integer | yes | caller data, `i64` |
| `created_at_ms` | Integer | yes | the frame's captured clock C, `i64` |
| `importance` | Float | yes | default 0.5, finite, `[0, 1]` |
| `confidence` | Float | no | finite, `[0, 1]`; separate from importance |
| `source` | String | yes | opaque |
| `provenance` | String | yes | A1's provenance encoding, at most 4,096 bytes |
| `evidence` | String | no | present iff nonempty: canonical decimal record ids, strictly ascending, `,`-separated, at most 64 |
| `status` | String | yes | `accepted` (F4 reserves `disputed`) |

Belief (confidence, observed time, provenance) is metadata on the segment, not
a second time axis: past answers reflect the currently retained correction, not
past beliefs (Gap 1 section 4). There is no bitemporal belief timeline.

### 5.3 Strict decoder rules

Series and slot decoding is strict even for objects whose receipt expired.

- **DS1.** Slot node: exactly one label `VliteMemorySlot`; exactly the
  properties of 3.4 (`series` iff `series_count >= 1`); string bounds; no graph
  TTL.
- **DS2.** `f1_schema` is `slot:v1`; `scope` equals the ScopeKey the key embeds;
  `f1_scope_tuple` is the hex of the scope bytes (mismatch: `DigestCollision`).
- **DS3.** `subject`, `predicate`, `slot_key` are opaque and `SlotKey`
  recomputed from them equals the node key.
- **DS4.** `mode` is `exclusive` or `multivalued`; `series_count` is in
  `0..=64`; `series` has length `64 * series_count`, lowercase hex, strictly
  ascending 64-hex blocks.
- **DS5.** A listed `DestHash` whose edge `fact:v1:...` is absent is allowed
  (reclaimed or forgotten, DG4); a present edge must decode (DE1-DE8).
- **DS6.** Nothing else is checked. In particular no relation between the slot
  revision and any series revision (B19).
- **DE1.** The edge is temporal, has the key shape of 5.1, `relationship` is
  `VLITE_FACT`, its slot digest hex equals the slot's, and its `DestHash` is a
  member of the slot's list (an unlisted series of a decoded slot is
  `InvalidBundle`: the registry and the edge must agree, except during the
  window DG4 allows after a reclaim).
- **DE2.** `source` and `destination` keys are `memory_key(ScopeKey, id)` of live
  `VliteMemory` entity nodes of the scope; the segment `destination_id` hashes
  to the key's `DestHash`.
- **DE3.** At most 256 segments; engine order and overlap rules hold.
- **DE4.** Each segment has exactly the properties of 5.2 with the required set;
  types as listed; `f1_schema` is `fact:v1`.
- **DE5.** Within one series every `fact_kind` is equal (a series is a fact
  series or a relation series). PROPOSAL (R12): the kind could instead be a
  slot property; the recommendation is the slot-level kind, decided at review.
- **DE6.** Exclusive slot: no two segments of any two listed series overlap
  (touching is legal). A stored violation is `InvalidBundle`.
- **DE7.** No property named `valid_from` or `valid_to`.
- **DE8.** `evidence` ids are canonical decimals, ascending, at most 64.
- **DG1.** The graph holds no series edge whose slot node is absent unless the
  edge is hidden or reclaimed (a series without a slot is residue: fail closed
  on read, `InvalidBundle`).
- **DG2.** The slot of a series edge is found by recomputing `SlotKey` from the
  `hex(SlotDigest)` inside the series key and the ScopeKey of its anchors.
- **DG3.** No slot lists more than 64 series.
- **DG4.** The next slot write removes listed series whose edge is absent.

### 5.4 Receipts

Receipt label `VliteMemoryReceipt`, key `R(op, idempotency_key)` with
`op` in `assert-facts`, `correct-facts`, `forget-series` (R16: the names are new
and versioned; the adapter maps them to the wire operations the way
`begin-erasure` maps to `forget`). Properties (exactly): `scope`,
`f1_scope_tuple`, `f1_schema` (`assert-facts:v1`, `correct-facts:v1`,
`forget-series:v1`), `f1_collection`, `digest`, `expires_at_ms` (canonical
decimal String), `outcome_chunks` (Integer, 1 for F2 and F6; for F1 the A1
rule, `ceil(len / 4096)`, because an outcome can exceed one chunk). Outcome
nodes: label `VliteMemoryReceiptOutcome`, key `receipt_outcome_key(receipt_key,
index)`, properties `receipt_key`, `index`, `outcome`.

Outcome text (PROPOSAL, ASCII, one line per item, final newline): the first
line is the schema, then for `assert-facts` one line per item in request order:
`<c|a|n>:<hex(item id)>:<hex(DestHash)>` (`c` created series, `a` appended
segment, `n` noop), followed by one `slot:<hex(SlotDigest)>` line per touched
slot in ascending order. The correct and forget-series outcome layouts, and DO
rules for all three, are NOT DONE in this draft.

## 6. Digests, receipts and replay (exact bytes)

Hash, notation and framing are section 3.4 and C1 section 3. Digest strings
are `"v1:" + hex(H(bytes))`. A1 and A2 digests are JSON-based and unchanged;
the three F digests are length-delimited binary under their own domain strings
and operation names (C2 section 5 rule 3, decision 9). Fields that are not
request identity (`idempotency_key` is in the receipt key; `now_ms`,
`receipt_window_ms`, `deadline_ms` are not hashed) match A1 and C1.

```text
ITEM(i, with_guard) =
    S(i.id) || U8(kind: 1 Fact, 2 Relation)
 || S(i.subject_id) || S(i.predicate) || S(i.slot_key) || U8(mode: 1 exclusive, 2 multivalued)
 || S(i.destination_id) || OI64(i.valid_from) || OI64(i.valid_to)
 || S(i.content) || S(i.source) || F64(i.importance or 0.5) || OF64(i.confidence)
 || I64(i.observed_at_ms)
 || U32(n_prov) || { S(source) || O(kind) || O(uri) || O(transformation) }*   // after A1's default fill-in
 || U32(n_evidence) || { S(evidence id) }*                                    // request order
 || [ with_guard: OU64(slot_expected: Absent -> 0, Revision(r) -> 1 || U64(r)) ]

AssertDigest  = "v1:" + hex(H( "vlite.memory.fact.assert.v1\0"
                  || SCOPE(ScopeBytes) || O(bound_collection)
                  || U8(allow_future_observations) || U32(n_items)
                  || ITEM(item_1, true) || ... || ITEM(item_n, true) ))        // request order

CorrectDigest = "v1:" + hex(H( "vlite.memory.fact.correct.v1\0"
                  || SCOPE(ScopeBytes) || O(bound_collection)
                  || S(subject_id) || S(predicate) || S(slot_key) || U8(mode)
                  || S(series_key) || U64(series_revision) || U64(slot_revision)
                  || OI64(valid_from) || OI64(valid_to)
                  || U8(action: 1 Close, 2 Remove, 3 ReplaceAt, 4 ReplaceInterval)
                  || body ))
     body: Close = I64(at); Remove = empty;
           ReplaceAt = I64(at) || ITEM(replacement, false);
           ReplaceInterval = OI64(from) || OI64(to) || ITEM(replacement, false)

ForgetSeriesDigest = "v1:" + hex(H( "vlite.memory.fact.forget.v1\0"
                  || SCOPE(ScopeBytes) || O(bound_collection)
                  || S(series_key) || U64(series_revision) ))
```

`bound_collection` is the handle's evidence collection name (`None` when the
binding has none), as C1 and E2 bind it.

Replay (all three operations), after the barrier rule of 4.1.7: a live receipt
with the same digest returns the stored outcome with `replayed = true` before
the state, deadline and fence are consulted; the slot revisions and the receipt
revision it reports are the receipt node's revision (the revision at commit; a
later write or maintenance makes them stale by design, so callers needing live
values call `find_facts`). A live receipt with another digest is
`IdempotencyConflict`. An expired receipt is ignored and its receipt and
outcome nodes are replaced in the retry's frame (A1 rule), so an expired retry
is a fresh evaluation: against a slot that advanced it fails `RevisionConflict`.

Idempotent: an exact retry within the window of each of the three operations.
Also idempotent by state, independent of any receipt: an assertion that exactly
repeats an existing segment (same bounds, same properties except
`created_at_ms`) is `FactAction::Noop` with no segment written (R13). Not
idempotent: a repeated `Close` with a different `at`, or a repeated `Remove`
(the segment is gone: `MissingMemory`).

## 7. Validity, belief and planning

7.1 **Validity.** `[valid_from, valid_to)` in `i64` milliseconds with `None` as
the open end (no sentinel values); `valid_from < valid_to` when both are finite;
`Some(i64::MIN)` and `Some(i64::MAX)` are ordinary finite bounds. Overlap test:
`a.from < b.to && b.from < a.to` with `None` read as the matching infinity;
touching (`a.to == b.from`) is legal. All comparisons are on `i64`, never on
sums or differences, so nothing can wrap. Validity is read only from the typed
segment bounds (DE7).

7.2 **Exclusive slots.** No two segments of any two of the slot's series
overlap. The planner builds, from the strictly decoded slot, the set of all
segments of all listed series (at most 64 series times 256 segments), applies
the request's items in request order, and tests each new interval against that
set including items accepted earlier in the same request. An overlap, except an
exact duplicate (R13), is `SlotConflict`. Checking only the newest series, or
only the same destination, is wrong (mutants 8, 12).

7.3 **Multivalued slots.** Parallel values on different destinations may
overlap. Within one series the engine's rule holds, so an overlap on the same
destination is `SlotConflict` from the planner (not the engine's
`InvalidTemporal(overlap)`, R13).

7.4 **Disagreement.** The only policy in F1-F3 is `reject`. `disputed` is F4
(section 11).

7.5 **New series versus append.** An item whose `SeriesKey` is not listed in the
slot creates a series (`CreateEdge` with that item's segment). Several items of
one request with the same new `SeriesKey` fold into one `CreateEdge` with their
segments in ascending order (an edge created earlier in a batch cannot be
appended to in that batch, B13). An item for an existing series is an
`AppendSegment` with `Expected::Revision(series revision read in this call)`.
Several appends to one existing series share the pre-batch revision.

7.6 **Belief.** Confidence, observed time, importance and provenance are
segment metadata (5.2). Nothing reconstructs past beliefs; `created_at_ms` is
the commit clock and is never used as validity.

7.7 **Anchors.** Subject and destination must exist, be live at C and be of kind
`entity` in the request scope (4.1.11). The core never creates anchors
(R10). When an anchor expires, its incident series is hidden at C and reclaimed
with it (B16, B18); when an anchor is forgotten by A2 its incident series is
deleted by the A2 edge pass. Both leave a dangling registry entry that reads
tolerate and the next slot write prunes (DG4). Residue beyond that (orphaned
versioned evidence, a stale registry on a slot that is never written again) is
the F6/E3 seam (section 9.5).

## 8. Atomicity, budgets, clocks and crash cuts

8.1 **One frame.** Each of `assert_facts`, `correct_facts` and `forget_series`
is exactly one `write_batch_at(&ops, now)` frame holding: the slot node writes
(one per touched slot, creation or guarded update, or deletion under R9), the
temporal operations, the receipt node and its outcome node(s). Validation,
planning and pre-append refusals change nothing. A guard failure fails the whole
frame (`Storage` from the engine, or `RevisionConflict` from the planner's
pre-check) and leaves no receipt. `find_facts` writes nothing.

8.2 **Operation counting** (user operations, as A1 and C1 count): slot writes +
temporal operations (an exact-end close counts one, conservatively, although the
engine plans nothing) + 1 receipt + outcome chunks, at most 4,096; over the cap
is `OperationLimit` and a request of more than 4,096 items fails earlier at
4.1.2. For `forget_series`: 1 edge delete + 1 slot write or delete + each
referenced evidence record deletion + receipt + outcome. Reclaim entries are
internal and not counted (B18).

8.3 **Bytes.** The frame body is at most 16 MiB, counted with the engine's own
entry-size functions (`codec/temporal_entries.rs` for `PutEdge`/segments,
`C1 section 7` for node entries), not a hand formula; a series must fit one body
(`reason::SERIES_BYTES`). Caps: 256 segments per series (B12), 64 series per
slot (ruling A), 4,096 bytes per string value, 65,536 property bytes per
element, 64 properties per element.

8.4 **B4, the temporal commit ruling (coordinator).** Cited by heading
("Commit framing and bounded units" in `serving-temporal-contract.md`, which
another agent is editing, so not by line): (a) due reclaim of a batch that
contains a temporal operation commits as separate earlier bounded frame(s), then
the batch frame (idempotent, TTL-only); (b) a temporal batch whose rendered unit
could exceed the smallest transport cap is refused before any write with an
existing error code and no new public `Error` variant. Applied here: the
planner estimates the rendered bytes of the whole frame; above the smallest
transport cap (8 MiB, R18) it returns `BodyLimit` before any write. The
atomicity wording of this section holds under both behaviours: today reclaim is
planned inside the frame (B18); under (a) a crash between a reclaim frame and
the fact frame leaves only already-due TTL cleanup, never a half fact change, and
the retry re-reads everything.

8.5 **Crash cuts.** Before the frame commits: nothing. After it commits: the
receipt exists, so an exact retry replays (A1 rule: a receipt present means the
whole frame committed; absent means a fresh retry may proceed on a healthy
handle). `CommitIndeterminate` is surfaced and never retried blindly. There is
no multi-frame fact operation, so no resumable job and no `ReconciliationRequired`
(that is E4).

8.6 **Race and the single writer.** Under `&mut Database` nothing interleaves
inside one call; the race is between two callers with the same stale expected
value. Every mutation that changes any series of a slot writes the slot node
under `Expected::Revision(r)` with `r` read in the same call, so the second
caller's stale `slot_expected` or `slot_revision` is `RevisionConflict` and the
first commit wins. The exact-end close is the one mutation that writes no slot
node (R24); its slot guard is the planner's read-compare of `slot_revision`.
Revision guards use only revisions read in the same call; no logic relies on a
relation between a slot revision and a series revision, or between revisions
across a maintenance event (B19).

## 9. Interactions

9.1 **Barrier (E1/E2).** A write in a barred scope replays a matching live
pre-barrier receipt, otherwise fails `DeletionPending` (4.1.7-8). `find_facts`
omits with `barred: true` (R11). A barrier never hides a stored receipt from
lookup (B9).

9.2 **TTL.** The slot node has no graph TTL and a series edge has no expiry of
its own: a fact lives exactly as long as both anchors. An anchor hidden at C
(`now_ms >= expiry`) hides its incident series; due reclaim deletes the series
with the anchor (B16, B18). The equality case (C equals expiry) hides, as A1.

9.3 **S1 and C1.** A fact is not a `VliteMemory` node, so it cannot be a C1
source (C1 and S1 accept only `entity` and `episodic` nodes, B4) and no
`DERIVED_FROM` edge can point at it (R22). S1 and S2 are unchanged.

9.4 **Recall.** `remember` refuses `Fact` and `Relation` with `UnsupportedKind`
(a second check next to `memory.rs:1279`, so no fact-kind memory node can be
created), recall refuses them with the same error it gives `Unsupported`
(`recall.rs:255`, one match arm), and the mandatory filter is untouched. Slot
nodes carry `VliteMemorySlot`, not `VliteMemory`, so `scan_memories` and recall
never see them. Both refusals stay until F5.

9.5 **Evidence, A2 and the E3 seam (open, R20, R21).** Versioned evidence records
are unattached and referenced by id from segment properties (decision 15, B17).
A2 `forget` and the E2 chunk planner list incident edges at T = C (B16): a series
valid at C is deleted wholesale (its slot registry goes stale and its referenced
evidence is orphaned); a series hidden at C makes `DeleteNode` fail with a
`Storage` error (fail closed). So until F6 and E3 integrate series, A2 and E2
over scopes or anchors that hold series are incomplete or fail. Options in R21;
the recommendation is that the F-track stays dark behind the gate until the F6/E3
seam lands, with a fail-closed guard in the meantime. The E3 and E4 contract
drafts (`gap1-e3-entity-erasure-contract.md`, `gap1-e4-reconciliation-contract.md`,
untracked in the working tree when this was written) were not read for this
draft; R21 must be reconciled with them.

9.6 **Receipts.** The live window and the retention are A1's: receipts carry no
graph TTL, survive anchor forget, and decode strictly even when expired. The
three new receipt kinds need GetReceipt lookup in the adapter (section 11.3).

9.7 **Binding generation.** Every write re-checks the binding record and
generation with the existing `check_binding` hook (`startup.rs`, function-level)
before the frame; a stale generation is `BindingGeneration`.

## 10. Maintenance, rollback and backends

10.1 **Compaction and upgrade** relabel every graph revision, temporal edges
included (B19). Persisted fact objects carry no revision numbers (slot, series
and segment properties hold none), so they decode identically afterwards;
caller-held revisions go stale and fail `RevisionConflict` until re-read.
Receipt replays report the receipt's current revision (stale by design).

10.2 **Directory checkpoint** (same lineage): temporal edge revisions are kept
(clamped), nodes and ordinary edges are restated at the checkpoint sequence
(B19). The slot node and its series therefore change revision independently.
This contract never compares them.

10.3 **Reopen and backends.** Every persisted object round-trips on plain file,
encrypted file, directory store and encrypted directory store. A directory store
inherits the temporal capability only by conversion (`temporal.rs` module note),
so the directory tests start from an `enable_temporal` copy-out. Encryption adds
no plaintext; nothing here changes the cipher or the key schedule.

10.4 **Rollback to 0.7.1.** A file without any slot node or fact series behaves
exactly as before. A file that holds either is a flag-64 file: 0.7.1 refuses it
at open with `UnsupportedFeature(64)` (one-way; the pre-activation file, which
`enable_temporal` leaves untouched, is the rollback path). A bindings file with
slots is refused by 0.7.1 as in 3.3. A 0.7.1 server therefore never sees a slot
node, a series edge or a `slots_digest` record.

10.5 **PITR and restore.** Not verified (notes file): graph revisions after a
restore are assumed to be relabelled like a compaction, so nothing may rely on
them.

## 11. Seams for F4 and F5, and inputs the serving adapter needs

11.1 **F4 (disputed).** Seams kept open: the segment `status` property exists
and is `accepted` in F1-F3, the property allowlist of DE4 is versioned by
`f1_schema` (`fact:v2` would add `disputed`), `FactItem` would gain a
`disagreement` field (not in F1 types, so the digest gains it only with a new
domain version), and `FactView.status` has the `Disputed` slot. `SlotConflict`
remains the `reject` outcome.

11.2 **F5 (recall).** Seams kept open: the recall refusal arm (9.4) is the single
hook F5 replaces; every segment already carries `created_at_ms`, `importance`,
`confidence` and `observed_at_ms`; no per-anchor fact index is designed here, and
F5 must revisit mandatory transit filtering (Gap 1 section 3, near the line that
limits kinds to Entity and Episodic).

11.3 **Inputs the serving adapter needs** (no wire shape or REST behaviour is
frozen here). The adapter must be able to fill:

| Core input or output | Wire source or sink (names only, not frozen) |
|---|---|
| `FactItem.slot`, `destination_id`, `interval`, `slot_expected` | `MemoryAssertion.{subject_id, predicate, slot_key, destination_id, interval, slot_expected}` of a `fact` or `relation` item |
| `FactItem.mode` | the binding's declared slot for the predicate (3.1); undeclared predicate: adapter refusal, never the core |
| `FactItem.{id, content, source, provenance, importance, confidence, observed_at_ms, evidence}` | the `MemoryItem` fields; today the adapter refuses `confidence`, `assertion` and `metadata` (`gap1-adapter-mapping.md`, request fields with no first-slice mapping) |
| `disagreement` | `reject` only; `disputed` refused until F4 |
| Mixed bundles | a Remember bundle with entities and facts cannot be one core call: R10 (adapter refuses mixed bundles) |
| `FactQuery` | `MemoryFactsRequest.{scope, slot, as_of_ms, history, limit, limits, allow_partial}` |
| `FactsResult` | `MemoryFactsResponse.facts` and `MemoryReadMeta.{as_of_ms, ttl_clock_ms, sequence = view_sequence, complete, truncation, limitations}`; `barred` maps to a `deletion_barrier` truncation; `stale` is always false |
| `CorrectFactsRequest` | `MemoryCorrectionRequest.{slot, series, slot_revision, interval, action, at_ms, correction_interval, replacement}`; the replacement's own `assertion.slot_expected` is redundant with `slot_revision` (open question: the adapter must require them equal or ignore one) |
| `ForgetSeriesRequest` | Forget `target.series{id, revision}`; the core derives the slot from the series key and reads the slot revision itself (R8) |
| `FactView.series_key`, `series_revision`, slot revision | `MemoryFactView.series{id, revision}`, `slot_revision` |
| `FactOutcome` | `MemoryOutcome` actions `created`, `noop`, `corrected`, `deleted` (`Appended` maps to `created`, R-note in the notes file) |
| Receipt lookup | `ReceiptOperation` gains the three F operations; their stored outcomes are self-describing, so GetReceipt needs no request (unlike remember, B9) |
| Errors | `SlotConflict`, `SlotModeConflict` need adapter mappings (they break the exhaustive `memory_error` match); the codes are the adapter's to propose, none is frozen here |

## 12. Ruling table (R1-R26)

Status: **Adopted** means the brief or C2 already fixes it; **PROPOSAL** needs the
coordinator. Recommendations are the draft's, not rulings.

| R | Question | Options | Recommendation | Status |
|---|---|---|---|---|
| R1 | Optional per-binding `slots` | none (facts not declared) / `slots: [{predicate, mode}]` | as ruled | Adopted (ruling A) |
| R2 | File version when slots are used | (a) `v2` iff any slot, `v1` with `slots` is `Malformed`; (b) add `slots` to `v1` (breaks 0.7.1 rollback); (c) always bump (breaks slotless rollback) | (a) | PROPOSAL, coordinator |
| R3 | Predicate charset and length in the file | (a) 1..=64 bytes `[a-z0-9][a-z0-9._-]*`; (b) opaque 1..=256 like the wire `MemoryKey` | (a) for config safety; the request predicate must equal a declared one | PROPOSAL |
| R4 | Max slots per binding | 64 / 256 / 1,024 | 256 | PROPOSAL |
| R5 | A mode change on a declared predicate | (a) slot mode immutable, applies to new slots only, old slots `SlotModeConflict` until emptied (R9); (b) allow widening exclusive to multivalued | (a) | Adopted (ruling A, decision 14); consequence needs a coordinator note |
| R6 | How the record carries slots | (a) optional `slots_digest`, record schema bumped only when present; (b) full list; (c) nothing | (a) | PROPOSAL |
| R7 | Slot creation | (a) implicit at first assert; (b) explicit `provision_slot` (the C2 F1 sketch) | (a), mode already comes from the binding | PROPOSAL |
| R8 | External series id | (a) the series key (137 bytes, fits `MemoryKey`); (b) the creating item id (needs an index); the Forget target has only `series{id, revision}` | (a), slot derived from the key | PROPOSAL |
| R9 | Emptied slot node after the last series is deleted | (a) never delete; (b) delete in the same frame; (c) sweep later | (b), since it is also the only way a mode can ever change | PROPOSAL, coordinator |
| R10 | Mixed Remember bundles (anchors and facts in one frame) | (a) adapter refuses; (b) later F1b creates anchors inside the fact frame | (a) | PROPOSAL |
| R11 | `find_facts` over a barred scope | (a) omit with `barred: true`; (b) `DeletionPending` like S1 | (a), as recall and the mapping doc | PROPOSAL, coordinator |
| R12 | Where `fact` versus `relation` lives | (a) per segment; (b) per slot; (c) per series | (b), decided at review | PROPOSAL |
| R13 | Exact duplicate and same-destination overlap | duplicate: `Noop` or conflict; overlap: `SlotConflict` or engine error | `Noop`; `SlotConflict` | PROPOSAL |
| R14 | Reuse of existing errors | new variants (`InvalidInterval`, `FactLimit`) or reuse `InvalidTarget`, `InvalidContent`, `OperationLimit` | reuse (decision 19 allows only two new variants) | PROPOSAL |
| R15 | Bad `confidence` | new variant / `InvalidImportance` | reuse | PROPOSAL |
| R16 | Receipt operation names | `assert-facts`, `correct-facts`, `forget-series` / reuse `remember`, `forget` | new names (C2 rule 3) | PROPOSAL |
| R17 | Receipt and slot property prefix | `f1_` / `fact_` | `f1_` (E2 reuses `e1_`) | PROPOSAL |
| R18 | B4 size rule for fact frames | refuse above 8 MiB estimated / above 16 MiB only | 8 MiB, existing `BodyLimit` | PROPOSAL (rendered-unit size model unverified) |
| R19 | Series cap per slot | 64 | 64 | Adopted (ruling A) |
| R20 | Evidence for fact segments | (a) references only, adapter creates records first; (b) core creates records; ownership, A2 rules, deletion of unreferenced records | (a) | PROPOSAL, NOT DONE |
| R21 | A2 and E2 over anchors holding series | (a) F-track stays dark until F6/E3 land; (b) fail-closed guard in forget and the E2 planner; (c) validity-ignoring incident-edge read for F6 and E3 | (a) plus (b); (c) is the F6/E3 design | PROPOSAL, coordinator |
| R22 | Fact as a C1 source | stays refused / allow | stays refused until F5 or X1 | PROPOSAL |
| R23 | Raw-write reservation of `fact:v1:` edge keys | extend `names.rs` to edges / rely on strict decoders | rely on decoders now; revisit at activation | PROPOSAL |
| R24 | Exact-end close writes a slot node | (a) no slot write, planner read-compare; (b) write it | (a) | PROPOSAL |
| R25 | Self relation (subject equals destination) | allow / refuse | allow | PROPOSAL |
| R26 | `SlotConflict` detail | unit variant, details via `find_facts` / variant carries competing ids and bounds | unit variant now (all `MemoryError` variants but `Storage` are units) | PROPOSAL |

## 13. Acceptance and test plan

### 13.1 Backends, seam and refusal

Every case runs on plain file, encrypted file, directory store and encrypted
directory store, with a reopen after each step and after compaction (and
checkpoint for directories). All tests hold an `Activation` (`temporal.rs:287`)
and start from an `enable_temporal` copy-out; production code never changes a
gate. Test `f_refusal_without_seam_writes_nothing`: each of the four operations with
`Activation` off, and with the seam on but a non-temporal file, returns
`UnsupportedFeature(64)` and the file is byte-identical afterwards (no slot
node, no receipt).

### 13.2 Independent oracle (specified, not written)

Files: `tests/memory_fact_oracle.py` and
`tests/fixtures/agent_memory/fact_golden.json`, written by a reviewer other than
the implementer; the implementer replays and never edits them (C2 rule 6). CI
wiring is one added line owned by the oracle slice (`.github/workflows/ci.yml`
oracle list). The accepted `tests/temporal_interval_oracle.py` stays unchanged.

Oracle inputs: a list of steps `{op: assert | correct | find | forget_series |
reopen | compact | checkpoint | clock, request}`; scope tuples; anchor ids
(entities are pre-created by A1 steps); the binding-declared mode per predicate.
Oracle computes independently: `SlotDigest`, `SlotKey`, `DestHash`, `SeriesKey`,
the three digests (section 6), `SlotsDigest`, overlap decisions, close, replace,
remove and supersede results, validity at T, history order, receipt and outcome
text (5.4), budget counts. Outputs per step: `ok` with the outcome lines, the
projected state (slots: mode and ascending series list; series: key, endpoints,
segments with typed bounds and property values) and counters, or the error name
from section 4. Revisions are never absolute in goldens; the oracle checks only
"advanced" and "unchanged", because maintenance relabels them (B19).

Golden cases (names; expected values are NOT DONE in this draft):

`g_slot_digest_vectors`, `g_series_key_vectors`, `g_assert_digest_vectors`,
`g_correct_digest_vectors`, `g_forget_digest_vectors`, `g_slots_digest_vectors`,
`g_exclusive_overlap_touching_gap`, `g_exclusive_all_series`,
`g_multivalued_parallel`, `g_multivalued_same_destination_overlap`,
`g_mode_conflict_immutable`, `g_slot_race`, `g_cross_scope_anchor`,
`g_expired_anchor`, `g_i64_extremes`, `g_segment_cap_256_257`,
`g_series_cap_64_65`, `g_ops_4096_4097`, `g_close_inside_exact_end_outside`,
`g_supersede_same_destination`, `g_supersede_changed_destination`,
`g_replace_interval_correction`, `g_remove_segment`,
`g_replay_conflict_expiry`, `g_find_at_t_edges`, `g_find_history_order`,
`g_find_limits_partial`, `g_find_barrier`, `g_forget_series_and_slot`,
`g_reopen_four_backends`, `g_refusal_without_seam`, `g_recall_refuses_kinds`.

### 13.3 Named Rust tests (selected)

`f_slot_digest_golden`, `f_distinct_predicates_distinct_slots`,
`f_cross_scope_same_strings_distinct`, `f_series_key_golden`,
`f_two_slots_same_destination`, `f_slot_race_second_writer_conflicts`,
`f_append_bumps_slot_revision`, `f_exclusive_overlap_across_all_series`,
`f_exclusive_touching_legal`, `f_exclusive_open_ends_overlap`,
`f_multivalued_parallel_values`, `f_slot_mode_conflict`,
`f_slot_mode_immutable`, `f_remember_refuses_fact_kinds`,
`f_recall_refuses_fact_kinds`, `f_validity_only_from_bounds`,
`f_find_at_t_edges`, `f_series_cap_64_65`, `f_segment_cap_256_257`,
`f_ops_4096_4097`, `f_close_exact_end_receipt_no_entry`,
`f_close_outside_rejected`, `f_supersede_atomic_failure`,
`f_supersede_stale_series_revision`, `f_changed_destination_new_series`,
`f_replay_identical`, `f_replay_after_state_change`,
`f_digest_slot_expected_distinguishes`, `f_atomic_failure_no_receipt`,
`f_find_as_of_vs_now`, `f_history_forbids_as_of`, `f_limits_20_100`,
`f_barrier_blocks_assert`, `f_cross_scope_anchor_refused`,
`f_expired_anchor_refused`, `f_forget_series_deletes_edge`,
`f_forget_series_updates_slot`, `f_dangling_series_pruned`,
`f_refusal_without_seam_writes_nothing`, `f_decoder_*` (one per DS, DE, DG
rule), `f_reopen_*` (four backends), `f_compaction_relabels_guards_go_stale`.

### 13.4 Hand mutants (each: the single edit, the killing test)

1. SlotDigest omits `predicate`: `f_distinct_predicates_distinct_slots`.
2. SlotDigest omits `slot_key`: `f_slot_digest_golden`.
3. SlotDigest omits the scope bytes: `f_cross_scope_same_strings_distinct`.
4. SeriesKey omits the slot digest: `f_two_slots_same_destination`.
5. SeriesKey hashes the subject instead of the destination: `f_series_key_golden`.
6. Slot write without `Expected::Revision`: `f_slot_race_second_writer_conflicts`.
7. Append does not write the slot node: `f_append_bumps_slot_revision`.
8. Exclusive overlap checks only the newest series: `f_exclusive_overlap_across_all_series`.
9. Overlap uses `<=` (touching counts as overlap): `f_exclusive_touching_legal`.
10. Open ends treated as zero: `f_exclusive_open_ends_overlap`.
11. Multivalued handled as exclusive: `f_multivalued_parallel_values`.
12. Exclusive handled as multivalued: `f_exclusive_overlap_across_all_series`.
13. Stored mode never compared: `f_slot_mode_conflict`.
14. Mode overwritten by a later write: `f_slot_mode_immutable`.
15. `remember` accepts `Fact`: `f_remember_refuses_fact_kinds`.
16. Recall accepts `Fact`: `f_recall_refuses_fact_kinds`.
17. Validity read from a property: `f_validity_only_from_bounds`.
18. Valid end inclusive at T: `f_find_at_t_edges`.
19. Series cap off by one: `f_series_cap_64_65`.
20. Segment cap check removed: `f_segment_cap_256_257`.
21. Operation budget off by one: `f_ops_4096_4097`.
22. Exact-end close plans an entry (or omits the receipt): `f_close_exact_end_receipt_no_entry`.
23. Close outside the segment accepted: `f_close_outside_rejected`.
24. Supersede in two frames: `f_supersede_atomic_failure`.
25. Supersede without the series revision guard: `f_supersede_stale_series_revision`.
26. Changed destination appended to the old series: `f_changed_destination_new_series`.
27. Replay re-evaluates instead of returning the stored outcome: `f_replay_after_state_change`.
28. Digest omits `slot_expected`: `f_digest_slot_expected_distinguishes`.
29. Receipt written outside the frame: `f_atomic_failure_no_receipt`.
30. `find_facts` ignores `as_of_ms`: `f_find_as_of_vs_now`; history allows T: `f_history_forbids_as_of`; limits wrong: `f_limits_20_100`.
31. Barrier ignored by `assert_facts`: `f_barrier_blocks_assert`.
32. Cross-scope or expired anchor accepted: `f_cross_scope_anchor_refused`, `f_expired_anchor_refused`.
33. `forget_series` removes segments but not the edge: `f_forget_series_deletes_edge`; leaves the registry stale: `f_forget_series_updates_slot`.
34. Dangling registry entry never pruned: `f_dangling_series_pruned`.
35. A temporal op issued before the seam check (slot node written first): `f_refusal_without_seam_writes_nothing`.

Lines 30, 32 and 33 each carry several edit ideas with their own tests, so the
list is 35 numbered lines holding 39 edit ideas; some are thin (22, 29 and 35
depend on the final frame layout).

### 13.5 Fail-first

After implementation each behaviour group needs a real stub or mutation that
makes its tests fail for the intended reason and pass when restored; unexecuted
or storage-invariant-equivalent mutations are not kills (C2 rule 5). Groups:
keys and digests (1-5, 28), slot guard and race (6, 7), overlap (8-12), mode
(13, 14), kinds (15, 16), validity (17, 18), caps and budget (19-21), correction
(22-26), replay (27, 29), find (30), barrier and anchors (31, 32), forget (33,
34), seam (35).

## 14. Slice order, dependencies, sizes

Relative sizes, not effort: F1 L, F2 M, F3 M, F6 M (as C2).

1. Oracle slice first (specified in 13.2), by a reviewer other than the
   implementer.
2. F1 (`assert_facts`, slot and series schemas, the hooks of section 2 B2-B4:
   `memory.rs` kinds, errors, `MEMORY_NODE_KINDS`, `validate_request`, test
   name table; `recall.rs` one arm; `mapping.rs` two arms (not in C2's F1 file
   list, erratum E-F4)). Serialized: it owns the persisted schemas.
3. F2 and F3 in parallel after F1 (both only add functions to `facts.rs`; they
   conflict on that file and on `facts/tests.rs`, so they serialize on those
   unless split into submodules `facts/correct.rs` and `facts/find.rs`, which
   this draft recommends).
4. F6 after F1, ideally together with E3 (the entity-forget seam and R21 are one
   design); F6 can start earlier for the series delete alone.
5. Dependencies: T1 (`--cfg vlite_unstable_temporal` gate), T2 (temporal
   serving) and T3 (the activation owner decision) gate activation, not code. The
   F-track ships dark under the gate until the owner flips activation.

## 15. Not frozen

- Everything marked PROPOSAL, notably R2-R4, R6-R18, R20-R26.
- The property names, types and the `f1_` prefix of 3.4 and 5.2.
- The outcome text and all DO rules; the correct and forget-series receipts.
- The evidence design of fact segments (R20) and the F6 entity-forget read (R21).
- The B4 size model (R18), the golden expected values, the golden file layout.
- Error choices for `SlotConflict` and `SlotModeConflict` on the wire.
- Anything about F4 and F5 beyond the seams of section 11.
- No claim about speed, size or capacity is made.
